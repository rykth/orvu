//! The one writer that saves data to disk. It is a dedicated blocking thread
//! that owns the `Store` and works through the requests sent to it, such as
//! appends and flushes. Forcing data to disk (fsync) can be slow. That wait
//! happens on this thread and never on the async engine task.

use orvu_core::{SnapshotEntry, WireDelta};
use orvu_crdt::DotContext;
use orvu_core::fold_into_snapshot;
use orvu_crdt::VersionVector;
use orvu_store::{FsyncPolicy, Store};
use std::collections::BTreeMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// What the store returns for catch-up: the deltas another machine is missing,
/// plus the "floor", which says for each object which updates the saved
/// snapshot already includes.
#[derive(Default)]
pub struct StoreReads {
    pub deltas: Vec<WireDelta>,
    pub floor: BTreeMap<String, DotContext>,
}

/// Why a save did not happen. Once saving fails the writer gives up for good,
/// so every later save fails with the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageFailed(pub String);

impl std::fmt::Display for StorageFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "saving to disk failed: {}", self.0)
    }
}

/// What an ack carries: `Ok` once the delta is on disk, or why it never will be.
pub type Saved = Result<(), StorageFailed>;

/// Whether saving has failed, shared by the writer thread and everyone who
/// needs to know. It is set once, by the writer, and never cleared.
#[derive(Clone, Default)]
pub struct StorageHealth(Arc<HealthInner>);

#[derive(Default)]
struct HealthInner {
    why: OnceLock<String>,
    /// Wakes those waiting in `lost` when the failure is recorded.
    lost: tokio::sync::Notify,
}

impl StorageHealth {
    /// Why saving failed, or `None` while it still works.
    pub fn failure(&self) -> Option<StorageFailed> {
        self.0.why.get().map(|why| StorageFailed(why.clone()))
    }

    /// Records why saving failed. Only the first reason is kept. Returns
    /// true if this was the first.
    pub(crate) fn set(&self, why: String) -> bool {
        let first = self.0.why.set(why).is_ok();
        if first {
            self.0.lost.notify_waiters();
        }
        first
    }

    /// Finishes once saving has failed, at once if it already has.
    pub async fn lost(&self) {
        loop {
            let woken = self.0.lost.notified();
            tokio::pin!(woken);
            // Registered before the check, so a failure recorded in between
            // still wakes this wait.
            woken.as_mut().enable();
            if self.0.why.get().is_some() {
                return;
            }
            woken.await;
        }
    }
}

/// A request to the writer thread.
pub enum Persist {
    /// Saves a delta, and the receipt that goes with it, if there is one,
    /// in the same step.
    Append { delta: Box<WireDelta>, receipt: Option<orvu_store::Receipt>, ack: Option<oneshot::Sender<Saved>> },
    Flush { ack: oneshot::Sender<Saved> },
    Compact { entries: Vec<SnapshotEntry> },
    Query { peer: BTreeMap<String, DotContext>, reply: oneshot::Sender<StoreReads> },
    FrontierCompact { frontier: BTreeMap<String, VersionVector>, max_tail: usize },
    TailLen { reply: oneshot::Sender<usize> },
}

/// The receipts of the last `RECEIPT_KEEP_MS`: those the store held when the
/// node started, and every one recorded since, each as soon as it is handed
/// to the writer with its change. It says which requests were done, and is
/// what the store says, or will say once it saves what it was given: a
/// receipt and its change are saved together or lost together.
#[derive(Clone, Default)]
pub struct Receipts(Arc<std::sync::Mutex<ReceiptsInner>>);

#[derive(Default)]
struct ReceiptsInner {
    by_id: std::collections::HashMap<[u8; 16], u64>,
    /// In the order they were added, so the old ones are let go from the
    /// front.
    order: std::collections::VecDeque<orvu_store::Receipt>,
}

impl Receipts {
    /// Adds `receipt`, and lets go of those older than the time receipts are
    /// kept for, counted back from it.
    pub fn add(&self, receipt: orvu_store::Receipt) {
        let mut inner = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.by_id.insert(receipt.id, receipt.at_ms);
        inner.order.push_back(receipt);
        let cutoff = receipt.at_ms.saturating_sub(orvu_store::RECEIPT_KEEP_MS);
        while let Some(old) = inner.order.front().copied() {
            if old.at_ms >= cutoff {
                break;
            }
            inner.order.pop_front();
            if inner.by_id.get(&old.id) == Some(&old.at_ms) {
                inner.by_id.remove(&old.id);
            }
        }
    }

    /// Whether a change was made under the receipt `id` within the time
    /// receipts are kept for, before `now_ms`.
    pub fn made(&self, id: &[u8; 16], now_ms: u64) -> bool {
        let inner = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.by_id.get(id).is_some_and(|at| now_ms.saturating_sub(*at) <= orvu_store::RECEIPT_KEEP_MS)
    }
}

/// A handle for sending requests to the writer thread. The engine and the
/// connection reader tasks hold clones. It is not stored in `NodeHandle`.
#[derive(Clone)]
pub struct PersistHandle {
    tx: mpsc::Sender<Persist>,
    health: StorageHealth,
    receipts: Receipts,
}

impl PersistHandle {
    /// Queues a delta to be saved. Does not block. Pass an `ack` and await it
    /// to learn when the delta is on disk, or that it never will be. Under
    /// `FsyncPolicy::Always` that is right after this delta's own flush. Under
    /// `Interval` it is after the next regular flush, so deltas are still
    /// saved in batches.
    pub fn append(&self, delta: WireDelta, ack: Option<oneshot::Sender<Saved>>) {
        let _ = self.tx.send(Persist::Append { delta: Box::new(delta), receipt: None, ack });
    }

    /// Like `append`, and saves `receipt` in the same step as the delta, so
    /// after a crash both are on disk or neither is.
    pub fn append_with_receipt(&self, delta: WireDelta, receipt: orvu_store::Receipt, ack: Option<oneshot::Sender<Saved>>) {
        self.receipts.add(receipt);
        let _ = self.tx.send(Persist::Append { delta: Box::new(delta), receipt: Some(receipt), ack });
    }

    /// Forces everything queued so far to disk, and waits until that is done.
    /// Fails if saving has failed, now or earlier, or the writer is gone.
    pub async fn flush(&self) -> Saved {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(Persist::Flush { ack }).is_err() {
            return Err(StorageFailed("the writer has stopped".into()));
        }
        rx.await.unwrap_or_else(|_| Err(StorageFailed("the writer has stopped".into())))
    }

    /// Why saving failed, or `None` while it still works.
    pub fn failure(&self) -> Option<StorageFailed> {
        self.health.failure()
    }

    /// The receipts recorded through this writer, and those it was given at
    /// the start.
    pub fn receipts(&self) -> Receipts {
        self.receipts.clone()
    }

    /// The shared record of whether saving has failed, for code that should
    /// not hold the writer open, such as the node handle.
    pub fn health(&self) -> StorageHealth {
        self.health.clone()
    }

    /// Queues a compaction: replace the snapshot with `entries` and empty the
    /// log. Does not block.
    pub fn compact(&self, entries: Vec<SnapshotEntry>) {
        let _ = self.tx.send(Persist::Compact { entries });
    }

    /// Queues a compaction guided by `frontier`, which says for each object
    /// how far every other member has got. Deltas at or below the frontier are
    /// merged into the snapshot. Deltas above it stay in the log. As a safety
    /// limit, if more than `max_tail` deltas would stay, all of them are merged
    /// into the snapshot. Does not block.
    pub fn frontier_compact(&self, frontier: BTreeMap<String, VersionVector>, max_tail: usize) {
        let _ = self.tx.send(Persist::FrontierCompact { frontier, max_tail });
    }

    /// How many deltas are in the saved log right now. Used for monitoring and
    /// tests. Returns 0 if the writer is gone.
    pub async fn tail_len(&self) -> usize {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Persist::TailLen { reply }).is_ok() {
            rx.await.unwrap_or(0)
        } else {
            0
        }
    }

    /// Reads from the store what another machine is missing: the deltas in the
    /// log it has not seen, and the snapshot floor. The writer thread does the
    /// read. Used by catch-up. Returns an empty result if the writer is gone.
    pub async fn query(&self, peer: BTreeMap<String, DotContext>) -> StoreReads {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Persist::Query { peer, reply }).is_ok() {
            rx.await.unwrap_or_default()
        } else {
            StoreReads::default()
        }
    }
}

/// Starts the writer thread. Returns a handle, plus a receiver that fires once
/// the thread has stopped and dropped the store. Callers can wait on it to
/// know the lock on the store's file is released before opening the same path
/// again.
pub fn spawn_writer(store: Box<dyn Store>, policy: FsyncPolicy) -> (PersistHandle, oneshot::Receiver<()>) {
    let (tx, rx) = mpsc::channel::<Persist>();
    let (exit_tx, exit_rx) = oneshot::channel();
    let health = StorageHealth::default();
    let shared = health.clone();
    std::thread::spawn(move || {
        let mut store = store;
        writer_loop(Writer { store: store.as_mut(), waiting: Vec::new(), health: shared }, rx, policy);
        drop(store); // release the file lock first, then signal that we exited
        let _ = exit_tx.send(());
    });
    (PersistHandle { tx, health, receipts: Receipts::default() }, exit_rx)
}

/// Answers a `Query` using the store's catch-up read methods. The writer
/// flushes before calling it, so everything it reads is on disk.
fn serve_query(store: &dyn Store, peer: &BTreeMap<String, DotContext>) -> StoreReads {
    let deltas = store.deltas_peer_lacks(peer).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "anti-entropy read (deltas) failed");
        Vec::new()
    });
    let floor = store.snapshot_contexts().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "anti-entropy read (floor) failed");
        BTreeMap::new()
    });
    StoreReads { deltas, floor }
}

/// Splits `tail` in two using `frontier`. A delta whose dot (its id: the
/// replica that made it and a counter) is at or below the frontier is merged
/// into the snapshot. A delta above the frontier stays in the tail.
///
/// If more than `max_tail` deltas would stay, which happens when a stalled
/// machine holds the frontier down, everything is merged and the tail is left
/// empty. That machine then catches up from a snapshot.
///
/// Returns (new_snapshot, tail).
fn frontier_fold(
    snapshot: Vec<SnapshotEntry>,
    tail: Vec<WireDelta>,
    frontier: &BTreeMap<String, VersionVector>,
    max_tail: usize,
) -> (Vec<SnapshotEntry>, Vec<WireDelta>) {
    let is_below = |d: &WireDelta| {
        frontier
            .get(&d.object_id)
            .is_some_and(|vv| d.dot.counter <= vv.get(d.dot.replica))
    };
    let (below, above): (Vec<WireDelta>, Vec<WireDelta>) =
        tail.into_iter().partition(|d| is_below(d));
    if above.len() > max_tail {
        let all: Vec<WireDelta> = below.into_iter().chain(above).collect();
        (fold_into_snapshot(snapshot, all), Vec::new())
    } else {
        (fold_into_snapshot(snapshot, below), above)
    }
}

/// The writer thread's state: the store, the acks of deltas appended but not
/// on disk yet, and the shared record of whether saving has failed.
struct Writer<'a> {
    store: &'a mut dyn Store,
    waiting: Vec<oneshot::Sender<Saved>>,
    health: StorageHealth,
}

impl Writer<'_> {
    /// Records that saving failed and tells everyone still waiting. Only the
    /// first failure is kept and logged: from then on nothing more is saved.
    fn fail(&mut self, what: &str, e: orvu_store::StoreError) {
        if self.health.set(format!("{what}: {e}")) {
            tracing::error!(error = %e, "{what} failed; this node takes no more changes");
        }
        let why = self.health.failure();
        for ack in self.waiting.drain(..) {
            let _ = ack.send(Err(why.clone().unwrap_or_else(|| StorageFailed(e.to_string()))));
        }
    }

    /// Everything appended so far is on disk: tell those waiting.
    fn saved(&mut self) {
        for ack in self.waiting.drain(..) {
            let _ = ack.send(Ok(()));
        }
    }

    fn append(&mut self, delta: &WireDelta, receipt: Option<orvu_store::Receipt>, ack: Option<oneshot::Sender<Saved>>) {
        if let Some(why) = self.health.failure() {
            if let Some(ack) = ack {
                let _ = ack.send(Err(why));
            }
            return;
        }
        if let Some(ack) = ack {
            self.waiting.push(ack);
        }
        // Nothing is flushed between the two, so the receipt is saved in
        // the same step as its delta.
        let saved = self.store.append(delta).and_then(|()| match receipt {
            Some(receipt) => self.store.receipt(receipt),
            None => Ok(()),
        });
        if let Err(e) = saved {
            self.fail("saving a change", e);
        }
    }

    fn flush(&mut self) -> Saved {
        if let Some(why) = self.health.failure() {
            return Err(why);
        }
        match self.store.flush() {
            Ok(()) => {
                self.saved();
                Ok(())
            }
            Err(e) => {
                self.fail("flushing to disk", e);
                Err(self.health.failure().unwrap_or_else(|| StorageFailed("flush failed".into())))
            }
        }
    }

    /// Replaces the snapshot with `entries`, which are built from the live
    /// data, so every delta appended so far is in them and on disk after.
    fn compact(&mut self, entries: &[SnapshotEntry]) {
        if self.health.failure().is_some() {
            return;
        }
        match self.store.compact(entries) {
            Ok(()) => self.saved(),
            Err(e) => self.fail("compacting the log", e),
        }
    }

    /// Runs a frontier-guided compaction. It flushes first so that nothing
    /// waiting to be written is lost, reads the snapshot and the log, merges
    /// them with `frontier_fold`, and writes the result back in one step that
    /// either happens completely or not at all.
    ///
    /// The result replaces everything in the store, so it must be built from
    /// everything in the store. If either read fails nothing is written, and
    /// the failure counts as a failed save: going on with an empty read would
    /// wipe the store.
    fn frontier_compact(&mut self, frontier: BTreeMap<String, VersionVector>, max_tail: usize) {
        if self.flush().is_err() {
            return;
        }
        let snapshot = match self.store.load_snapshot() {
            Ok(s) => s,
            Err(e) => return self.fail("reading the snapshot to compact it", e),
        };
        let tail = match self.store.replay() {
            Ok(t) => t,
            Err(e) => return self.fail("reading the log to compact it", e),
        };
        let (new_snapshot, kept_tail) = frontier_fold(snapshot, tail, &frontier, max_tail);
        if let Err(e) = self.store.compact_to(&new_snapshot, &kept_tail) {
            self.fail("compacting the log", e);
        }
    }

    /// Answers a catch-up read. It flushes first, so what it hands to another
    /// machine is on disk here. If saving has failed it hands out nothing.
    fn query(&mut self, peer: &BTreeMap<String, DotContext>) -> StoreReads {
        match self.flush() {
            Ok(()) => serve_query(self.store, peer),
            Err(_) => StoreReads::default(),
        }
    }

    fn tail_len(&self) -> usize {
        self.store.replay().map(|t| t.len()).unwrap_or(0)
    }
}

fn writer_loop(mut w: Writer<'_>, rx: mpsc::Receiver<Persist>, policy: FsyncPolicy) {
    // Under `Always` every append is flushed at once. Under `Interval` the
    // flush waits for the timer, and the acks with it.
    let period = match policy {
        FsyncPolicy::Always => None,
        FsyncPolicy::Interval { ms } => Some(Duration::from_millis(ms.max(1))),
    };
    let mut last = Instant::now();
    loop {
        let msg = match period {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(period) => rx.recv_timeout(period.saturating_sub(last.elapsed())),
        };
        match msg {
            Ok(Persist::Append { delta, receipt, ack }) => {
                w.append(&delta, receipt, ack);
                match period {
                    None => {
                        let _ = w.flush();
                    }
                    Some(period) if last.elapsed() >= period => {
                        let _ = w.flush();
                        last = Instant::now();
                    }
                    Some(_) => {}
                }
            }
            Ok(Persist::Flush { ack }) => {
                let _ = ack.send(w.flush());
                last = Instant::now();
            }
            Ok(Persist::Compact { entries }) => w.compact(&entries),
            Ok(Persist::Query { peer, reply }) => {
                let _ = reply.send(w.query(&peer));
            }
            Ok(Persist::FrontierCompact { frontier, max_tail }) => w.frontier_compact(frontier, max_tail),
            Ok(Persist::TailLen { reply }) => {
                let _ = reply.send(w.tail_len());
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = w.flush();
                last = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = w.flush();
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_crdt::{Dot, DotContext, Hlc, PnCounter, ReplicaId};
    use orvu_core::TypedDelta;
    use orvu_store::MemStore;

    fn delta(counter: u64) -> WireDelta {
        let mut c = PnCounter::default();
        WireDelta {
            object_id: "o".to_string(),
            dot: Dot::new(ReplicaId(1), counter),
            hlc: Hlc::ZERO,
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(c.increment(ReplicaId(1), 1)),
            trace_id: orvu_core::TraceId::none(),
        }
    }

    #[tokio::test]
    async fn always_acks_after_the_delta_is_durable() {
        let store = MemStore::new();
        let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Always);
        let (ack, rx) = oneshot::channel();
        handle.append(delta(1), Some(ack));
        rx.await.unwrap().unwrap(); // returns only once the delta is saved
        assert_eq!(store.replay().unwrap().len(), 1);
    }

    // Under `Interval` an ack waits for the regular flush. It does not force
    // one of its own, so changes are still saved in batches.
    #[tokio::test]
    async fn interval_acks_when_the_regular_flush_has_saved_the_delta() {
        let store = MemStore::new();
        let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Interval { ms: 400 });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (ack, mut rx) = oneshot::channel();
        handle.append(delta(1), Some(ack));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err(), "acked before the interval was up");
        assert!(store.reopened().replay().unwrap().is_empty(), "flushed before the interval was up");
        let saved = tokio::time::timeout(std::time::Duration::from_secs(2), rx).await.unwrap().unwrap();
        assert!(saved.is_ok());
        assert_eq!(store.reopened().replay().unwrap().len(), 1, "acked before it was on disk");
    }

    // Catch-up reads must not hand out a delta that is not on disk yet.
    #[tokio::test]
    async fn catch_up_reads_only_hand_out_saved_deltas() {
        use std::collections::BTreeMap;
        let store = MemStore::new();
        let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Interval { ms: 60_000 });
        handle.append(delta(1), None);
        let reads = handle.query(BTreeMap::new()).await;
        let on_disk = store.reopened().replay().unwrap().len();
        for d in &reads.deltas {
            assert!(d.dot.counter as usize <= on_disk, "catch-up handed out an unsaved delta");
        }
    }

    #[tokio::test]
    async fn interval_flush_persists_pending() {
        let store = MemStore::new();
        let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Interval { ms: 50 });
        handle.append(delta(1), None);
        handle.append(delta(2), None);
        handle.flush().await.unwrap();
        let got: Vec<u64> = store.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(got, vec![1, 2]);
    }

    #[tokio::test]
    async fn writer_exit_fires_after_all_handles_drop() {
        let store = MemStore::new();
        let (handle, exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Interval { ms: 20 });
        handle.append(delta(1), None);
        drop(handle); // the last PersistHandle is gone, so the channel disconnects
        // The writer flushes, drops the store, and signals exit.
        tokio::time::timeout(std::time::Duration::from_secs(2), exit)
            .await
            .expect("writer exits promptly")
            .expect("exit signal received");
        assert_eq!(store.replay().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn compact_reaches_the_store() {
        use orvu_crdt::DotContext;
        use orvu_core::{SnapshotEntry, TypedCrdt};
        let store = MemStore::new();
        let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Interval { ms: 20 });
        handle.append(delta(1), None);
        let entry = SnapshotEntry {
            object_id: "o".into(),
            state: TypedCrdt::PnCounter(Default::default()).snapshot(),
            context: DotContext::new(),
        };
        handle.compact(vec![entry.clone()]);
        handle.flush().await.unwrap(); // handled after the compact, so the compact is done by now
        assert_eq!(store.load_snapshot().unwrap(), vec![entry]);
        assert!(store.replay().unwrap().is_empty(), "compaction cleared the log");
    }

    #[tokio::test]
    async fn query_round_trips_through_the_writer() {
        use std::collections::BTreeMap;
        let store = MemStore::new();
        let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Always);
        handle.append(delta(1), None);
        handle.append(delta(2), None);
        handle.flush().await.unwrap(); // handled before the query, so both appends are done

        // A machine that has seen nothing is missing every delta in the log.
        let reads = handle.query(BTreeMap::new()).await;
        let got: Vec<u64> = reads.deltas.iter().map(|d| d.dot.counter).collect();
        assert_eq!(got, vec![1, 2]);
        assert!(reads.floor.is_empty(), "no compaction -> empty floor");
    }

    #[test]
    fn frontier_fold_keeps_above_frontier_and_folds_below() {
        use orvu_crdt::{Dot, ReplicaId, VersionVector};
        use std::collections::BTreeMap;
        // The tail is (1,1),(1,2),(1,3) for object "o". The frontier reaches (1,2).
        let tail = vec![delta(1), delta(2), delta(3)];
        let mut vv = VersionVector::new();
        vv.observe(ReplicaId(1), 2);
        let mut frontier = BTreeMap::new();
        frontier.insert("o".to_string(), vv);

        let (snap, kept) = frontier_fold(Vec::new(), tail, &frontier, 100);
        let kept_counters: Vec<u64> = kept.iter().map(|d| d.dot.counter).collect();
        assert_eq!(kept_counters, vec![3], "above-frontier (1,3) kept as tail");
        assert_eq!(snap.len(), 1);
        assert!(snap[0].context.contains(Dot::new(ReplicaId(1), 2)), "(1,2) folded into snapshot");
        assert!(!snap[0].context.contains(Dot::new(ReplicaId(1), 3)), "(1,3) not folded");
    }

    #[test]
    fn frontier_fold_backstop_folds_all_when_tail_exceeds_cap() {
        use orvu_crdt::{Dot, ReplicaId};
        use std::collections::BTreeMap;
        let tail = vec![delta(1), delta(2), delta(3)];
        // With an empty frontier all three are above it. With max_tail 2 that is
        // too many to keep, so everything is merged into the snapshot.
        let (snap, kept) = frontier_fold(Vec::new(), tail, &BTreeMap::new(), 2);
        assert!(kept.is_empty(), "backstop empties the tail");
        assert_eq!(snap.len(), 1);
        assert!(snap[0].context.contains(Dot::new(ReplicaId(1), 3)), "everything folded");
    }

    /// A store whose reads of the snapshot or of the log can be made to fail.
    #[derive(Clone, Default)]
    struct UnreadableStore {
        inner: MemStore,
        fail_snapshot: Arc<std::sync::atomic::AtomicBool>,
        fail_replay: Arc<std::sync::atomic::AtomicBool>,
    }

    fn unreadable() -> orvu_store::StoreError {
        orvu_store::StoreError::Corrupted("cannot read".into())
    }

    impl Store for UnreadableStore {
        fn append(&mut self, delta: &WireDelta) -> orvu_store::Result<()> {
            self.inner.append(delta)
        }
        fn receipt(&mut self, receipt: orvu_store::Receipt) -> orvu_store::Result<()> {
            self.inner.receipt(receipt)
        }
        fn receipts(&self) -> orvu_store::Result<Vec<orvu_store::Receipt>> {
            self.inner.receipts()
        }
        fn replay(&self) -> orvu_store::Result<Vec<WireDelta>> {
            if self.fail_replay.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(unreadable());
            }
            self.inner.replay()
        }
        fn flush(&mut self) -> orvu_store::Result<()> {
            self.inner.flush()
        }
        fn compact_to(&mut self, snapshot: &[SnapshotEntry], tail: &[WireDelta]) -> orvu_store::Result<()> {
            self.inner.compact_to(snapshot, tail)
        }
        fn load_snapshot(&self) -> orvu_store::Result<Vec<SnapshotEntry>> {
            if self.fail_snapshot.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(unreadable());
            }
            self.inner.load_snapshot()
        }
        fn deltas_peer_lacks(&self, peer: &BTreeMap<String, DotContext>) -> orvu_store::Result<Vec<WireDelta>> {
            self.inner.deltas_peer_lacks(peer)
        }
        fn snapshot_contexts(&self) -> orvu_store::Result<BTreeMap<String, DotContext>> {
            self.inner.snapshot_contexts()
        }
    }

    // A compaction rewrites the whole store from what it reads. If a read
    // fails, it must not go on as if the store were empty and wipe it. It
    // leaves the store alone and counts as a failed save.
    #[tokio::test]
    async fn a_compaction_that_cannot_read_the_store_leaves_it_alone() {
        use orvu_core::TypedCrdt;
        for broken in ["snapshot", "log"] {
            let store = UnreadableStore::default();
            let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Always);
            // One object in the snapshot and two deltas in the log.
            let entry = SnapshotEntry {
                object_id: "s".into(),
                state: TypedCrdt::PnCounter(Default::default()).snapshot(),
                context: DotContext::new(),
            };
            store.inner.clone().compact(std::slice::from_ref(&entry)).unwrap();
            handle.append(delta(1), None);
            handle.append(delta(2), None);
            handle.flush().await.unwrap();

            let flag = if broken == "snapshot" { &store.fail_snapshot } else { &store.fail_replay };
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            handle.frontier_compact(BTreeMap::new(), 0);
            let after = handle.flush().await; // handled after the compaction
            flag.store(false, std::sync::atomic::Ordering::SeqCst);

            assert_eq!(store.load_snapshot().unwrap(), vec![entry], "{broken}: the snapshot was replaced");
            let log: Vec<u64> = store.replay().unwrap().iter().map(|d| d.dot.counter).collect();
            assert_eq!(log, vec![1, 2], "{broken}: the log was replaced");
            assert!(after.is_err(), "{broken}: the failed read was not recorded");
            assert!(handle.failure().is_some(), "{broken}: the failed read was not recorded");
        }
    }

    #[tokio::test]
    async fn frontier_compact_and_tail_len_through_the_writer() {
        use orvu_crdt::{Dot, ReplicaId, VersionVector};
        use std::collections::BTreeMap;
        let store = MemStore::new();
        let (handle, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Always);
        handle.append(delta(1), None);
        handle.append(delta(2), None);
        handle.append(delta(3), None);
        handle.flush().await.unwrap();

        // The frontier reaches (1,2), so (1,1) and (1,2) are merged and (1,3) is kept.
        let mut vv = VersionVector::new();
        vv.observe(ReplicaId(1), 2);
        let mut frontier = BTreeMap::new();
        frontier.insert("o".to_string(), vv);
        handle.frontier_compact(frontier, 100);
        handle.flush().await.unwrap(); // handled after the compaction

        assert_eq!(handle.tail_len().await, 1, "only the above-frontier delta remains");
        let snap = store.load_snapshot().unwrap();
        assert_eq!(snap.len(), 1, "a snapshot was written");
        assert!(snap[0].context.contains(Dot::new(ReplicaId(1), 2)));
    }
}
