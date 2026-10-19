//! `orvu-store`: saves the log of deltas to disk, behind a small trait.
//!
//! The trait's methods are ordinary blocking calls, because the standard
//! backend (`redb`, a database kept in one file) blocks while it reads and
//! writes. Code that must not block should call the store from a thread set
//! aside for it. That way this module needs no async runtime of its own.

use orvu_core::{SnapshotEntry, WireDelta};
use orvu_crdt::DotContext;
use std::collections::BTreeMap;

mod redb_store;
pub use redb_store::{RedbStore, UNSAVED_LIMIT};

/// What can go wrong with a store. The kinds are kept apart so a caller can
/// decide what to do: wait and try again, tell someone the disk is full, or
/// stop because the file is damaged.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Another program, or another store in this one, already has the file
    /// open.
    #[error("the store file is already open somewhere else")]
    AlreadyOpen,
    /// Reading or writing the file failed. A full disk is one of these, with
    /// the kind `StorageFull`.
    #[error("could not read or write the store file: {0}")]
    Io(#[source] std::io::Error),
    /// What is in the file cannot be read back: the file is damaged, or a
    /// saved record does not decode.
    #[error("corrupt stored data: {0}")]
    Corrupted(String),
    /// Anything else the database reported.
    #[error("storage error: {0}")]
    Other(String),
}

impl From<orvu_core::Error> for StoreError {
    fn from(e: orvu_core::Error) -> Self {
        StoreError::Corrupted(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A note that a change asked for under a request id was made. A program
/// that is not sure whether its change went through, because the answer
/// never came, sends it again with the same id, and the receipt says it was
/// done. It is saved in the same step as the change it follows, so after a
/// crash both are there or neither is.
///
/// `id` is the request's own id, as whoever asked made it. `at_ms` is when
/// the change was made, in milliseconds since 1970.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Receipt {
    pub id: [u8; 16],
    pub at_ms: u64,
}

/// How long a receipt is kept, in milliseconds: ten minutes. A store lets
/// go of receipts older than that, counted back from the newest it saves.
pub const RECEIPT_KEEP_MS: u64 = 10 * 60 * 1000;

/// How soon a delta should be safely on disk. This is a setting for the code
/// that uses a store. Nothing in this module reads it or flushes by itself:
/// the caller looks at the setting and calls `flush` to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// Flush after every delta, so it is on disk before the change that made
    /// it is reported as done.
    Always,
    /// Flush at most every `ms` milliseconds, so several deltas are written
    /// together. A crash can lose up to that much work.
    Interval { ms: u64 },
}

impl Default for FsyncPolicy {
    fn default() -> Self {
        FsyncPolicy::Interval { ms: 100 }
    }
}

/// A log of applied deltas that is kept on disk and only added to at the end.
///
/// `append` records a delta. `flush` makes sure the recorded deltas are on
/// disk. `replay` returns every delta in the log in the order it was appended,
/// including ones not flushed yet. Recovery after a restart reads it, and by
/// then only the flushed ones are left.
pub trait Store: Send {
    fn append(&mut self, delta: &WireDelta) -> Result<()>;
    /// Records `receipt`. It is saved by the next `flush`, in the same step
    /// as everything appended before it, so a receipt written right after
    /// the delta it is for is saved with it or not at all. Receipts older
    /// than `RECEIPT_KEEP_MS` before the newest one saved may be let go.
    fn receipt(&mut self, receipt: Receipt) -> Result<()>;
    /// The receipts kept, oldest first, those not flushed yet included.
    fn receipts(&self) -> Result<Vec<Receipt>>;
    fn replay(&self) -> Result<Vec<WireDelta>>;
    fn flush(&mut self) -> Result<()>;
    /// Replaces the snapshot with `entries` and empties the delta log, as one
    /// step that either happens completely or not at all. See `compact_to`
    /// for what the snapshot has to include.
    fn compact(&mut self, entries: &[SnapshotEntry]) -> Result<()> {
        self.compact_to(entries, &[])
    }
    /// Replaces the snapshot with `snapshot` and makes the delta log hold
    /// exactly `tail`, numbered again from 0. Both happen as one step that
    /// either completes or does nothing. `compact(x)` is `compact_to(x, &[])`.
    ///
    /// Everything that was in the log is dropped, deltas not flushed yet
    /// included. So `snapshot` and `tail` together must hold every delta
    /// appended so far. A snapshot built from the live data, or from `replay`,
    /// does. If two entries have the same object id, the later one is kept.
    fn compact_to(&mut self, snapshot: &[SnapshotEntry], tail: &[WireDelta]) -> Result<()>;
    /// The saved snapshot entries, in order of object id. Empty if the log has
    /// never been compacted.
    fn load_snapshot(&self) -> Result<Vec<SnapshotEntry>>;

    /// The deltas in the log that another machine has not seen yet, including
    /// ones not flushed yet. `peer` says, for each object, which updates that
    /// machine has seen. This is how a machine that fell behind is sent what
    /// it is missing.
    fn deltas_peer_lacks(&self, peer: &BTreeMap<String, DotContext>) -> Result<Vec<WireDelta>>;

    /// For each object, which updates the saved snapshot includes. Empty
    /// before the first compaction. Another machine needs the snapshot of an
    /// object exactly when it has not seen everything listed here for it.
    fn snapshot_contexts(&self) -> Result<BTreeMap<String, DotContext>>;
}

/// True if `peer`, another machine's record of what it has seen for each
/// object, does not include `delta`. An object the peer has no record of counts
/// as nothing seen.
pub(crate) fn peer_lacks(peer: &BTreeMap<String, DotContext>, delta: &WireDelta) -> bool {
    match peer.get(&delta.object_id) {
        Some(ctx) => !ctx.contains(delta.dot),
        None => true,
    }
}

#[derive(Default)]
struct MemInner {
    /// While true, `append` and `flush` fail as a full disk would.
    failing: bool,
    /// Deltas that have been flushed.
    log: Vec<WireDelta>,
    /// Deltas appended since the last flush.
    pending: Vec<WireDelta>,
    /// Receipts that have been flushed, and those recorded since.
    receipts: Vec<Receipt>,
    pending_receipts: Vec<Receipt>,
    snapshot: BTreeMap<String, SnapshotEntry>,
}

/// A store that keeps everything in memory, for tests. A clone shares the same
/// data, so a test can give one clone to the code that writes and keep another
/// to look at what was written.
///
/// It keeps to the same rules as `RedbStore`. An appended delta only counts as
/// saved after `flush`, and `reopened` gives the store a restart would find,
/// without the deltas that were never flushed.
#[derive(Clone, Default)]
pub struct MemStore {
    inner: std::sync::Arc<std::sync::Mutex<MemInner>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The store as it would be after a crash and a restart: a separate store
    /// holding the snapshot and the flushed deltas. Deltas that were appended
    /// but never flushed are gone, as they would be on disk.
    pub fn reopened(&self) -> Self {
        let inner = self.inner.lock().unwrap();
        let copy = MemInner {
            failing: false,
            log: inner.log.clone(),
            pending: Vec::new(),
            receipts: inner.receipts.clone(),
            pending_receipts: Vec::new(),
            snapshot: inner.snapshot.clone(),
        };
        Self { inner: std::sync::Arc::new(std::sync::Mutex::new(copy)) }
    }

    /// Makes saving fail, or work again. While it fails, `append` and `flush`
    /// return the error a full disk gives, so a test can see what the code
    /// using the store does when saving fails. Every clone sees it.
    pub fn fail_saving(&self, failing: bool) {
        self.inner.lock().unwrap().failing = failing;
    }

    fn check_saving(inner: &MemInner) -> Result<()> {
        if inner.failing {
            return Err(StoreError::Io(std::io::ErrorKind::StorageFull.into()));
        }
        Ok(())
    }
}

impl Store for MemStore {
    fn append(&mut self, delta: &WireDelta) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::check_saving(&inner)?;
        inner.pending.push(delta.clone());
        Ok(())
    }
    fn receipt(&mut self, receipt: Receipt) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::check_saving(&inner)?;
        inner.pending_receipts.push(receipt);
        Ok(())
    }
    fn receipts(&self) -> Result<Vec<Receipt>> {
        let inner = self.inner.lock().unwrap();
        let mut all: Vec<Receipt> = inner.receipts.iter().chain(&inner.pending_receipts).copied().collect();
        all.sort_by_key(|receipt| (receipt.at_ms, receipt.id));
        Ok(all)
    }
    fn replay(&self) -> Result<Vec<WireDelta>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.log.iter().chain(&inner.pending).cloned().collect())
    }
    fn flush(&mut self) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        Self::check_saving(&inner)?;
        let pending = std::mem::take(&mut inner.pending);
        inner.log.extend(pending);
        let receipts = std::mem::take(&mut inner.pending_receipts);
        if let Some(newest) = receipts.iter().map(|receipt| receipt.at_ms).max() {
            inner.receipts.extend(receipts);
            let cutoff = newest.saturating_sub(RECEIPT_KEEP_MS);
            inner.receipts.retain(|receipt| receipt.at_ms >= cutoff);
        }
        Ok(())
    }
    fn compact_to(&mut self, snapshot: &[SnapshotEntry], tail: &[WireDelta]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.snapshot = snapshot.iter().map(|e| (e.object_id.clone(), e.clone())).collect();
        inner.log = tail.to_vec();
        inner.pending.clear();
        let receipts = std::mem::take(&mut inner.pending_receipts);
        inner.receipts.extend(receipts);
        Ok(())
    }
    fn load_snapshot(&self) -> Result<Vec<SnapshotEntry>> {
        Ok(self.inner.lock().unwrap().snapshot.values().cloned().collect())
    }
    fn deltas_peer_lacks(&self, peer: &BTreeMap<String, DotContext>) -> Result<Vec<WireDelta>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.log.iter().chain(&inner.pending).filter(|d| peer_lacks(peer, d)).cloned().collect())
    }
    fn snapshot_contexts(&self) -> Result<BTreeMap<String, DotContext>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.snapshot.iter().map(|(id, e)| (id.clone(), e.context.clone())).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_crdt::{Dot, DotContext, Hlc, PnCounter, ReplicaId};
    use orvu_core::TypedDelta;

    /// A counter delta. Each `counter` gives a different delta, so tests can
    /// use that number to tell deltas apart.
    pub(crate) fn delta(counter: u64) -> WireDelta {
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

    #[test]
    fn memstore_append_then_replay_preserves_order() {
        let mut s = MemStore::new();
        s.append(&delta(1)).unwrap();
        s.append(&delta(2)).unwrap();
        let got: Vec<u64> = s.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(got, vec![1, 2]);
    }

    fn entry(id: &str, counter: u64) -> SnapshotEntry {
        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), counter));
        SnapshotEntry {
            object_id: id.to_string(),
            state: orvu_core::TypedCrdt::PnCounter(Default::default()).snapshot(),
            context: ctx,
        }
    }

    // The in-memory store has to lose what the real one would lose, or a test
    // that forgets to flush would pass here and fail for real.
    #[test]
    fn memstore_reopened_keeps_only_what_was_flushed() {
        let mut s = MemStore::new();
        s.append(&delta(1)).unwrap();
        s.flush().unwrap();
        s.append(&delta(2)).unwrap();

        let seen: Vec<u64> = s.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(seen, vec![1, 2], "replay returns unflushed deltas too");

        let after: Vec<u64> = s.reopened().replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(after, vec![1], "the unflushed delta did not survive");
    }

    // The in-memory store keeps receipts as the real one does: only those
    // flushed are there after a restart.
    #[test]
    fn memstore_reopened_keeps_only_flushed_receipts() {
        let mut s = MemStore::new();
        let (first, second) = (Receipt { id: [1; 16], at_ms: 1 }, Receipt { id: [2; 16], at_ms: 2 });
        s.append(&delta(1)).unwrap();
        s.receipt(first).unwrap();
        s.flush().unwrap();
        s.append(&delta(2)).unwrap();
        s.receipt(second).unwrap();
        assert_eq!(s.receipts().unwrap(), vec![first, second]);
        assert_eq!(s.reopened().receipts().unwrap(), vec![first]);
    }

    // A test can make the in-memory store fail to save, as a full disk would.
    #[test]
    fn memstore_can_be_made_to_fail_saving() {
        let mut s = MemStore::new();
        s.append(&delta(1)).unwrap();
        s.clone().fail_saving(true);
        assert!(matches!(s.flush(), Err(StoreError::Io(_))), "a flush worked while saving fails");
        assert!(matches!(s.append(&delta(2)), Err(StoreError::Io(_))), "an append worked while saving fails");
        assert!(s.reopened().replay().unwrap().is_empty(), "something was saved while saving fails");
        s.fail_saving(false);
        s.flush().unwrap();
        assert_eq!(s.reopened().replay().unwrap().len(), 1);
    }

    #[test]
    fn memstore_snapshot_is_kept_by_object_id() {
        let mut s = MemStore::new();
        s.compact(&[entry("b", 1), entry("a", 1), entry("b", 2)]).unwrap();
        assert_eq!(s.load_snapshot().unwrap(), vec![entry("a", 1), entry("b", 2)]);
    }

    #[test]
    fn a_decode_error_does_not_claim_to_be_about_a_delta() {
        let err = StoreError::from(orvu_core::Error::MissingField("x"));
        assert!(err.to_string().starts_with("corrupt stored data"), "{err}");
    }

    #[test]
    fn memstore_clone_shares_state() {
        let mut s = MemStore::new();
        let s2 = s.clone();
        s.append(&delta(1)).unwrap();
        assert_eq!(s2.replay().unwrap().len(), 1, "clone sees the append");
    }

    #[test]
    fn memstore_compact_replaces_snapshot_and_clears_log() {
        use orvu_crdt::DotContext;
        use orvu_core::{SnapshotEntry, TypedCrdt};
        let mut s = MemStore::new();
        s.append(&delta(1)).unwrap();
        s.append(&delta(2)).unwrap();
        let entry = SnapshotEntry {
            object_id: "o".into(),
            state: TypedCrdt::PnCounter(Default::default()).snapshot(),
            context: DotContext::new(),
        };
        s.compact(std::slice::from_ref(&entry)).unwrap();
        assert!(s.replay().unwrap().is_empty(), "log cleared by compaction");
        assert_eq!(s.load_snapshot().unwrap(), vec![entry], "snapshot stored");
    }

    #[test]
    fn deltas_peer_lacks_returns_the_uncovered_tail() {
        use std::collections::BTreeMap;
        let mut s = MemStore::new();
        s.append(&delta(1)).unwrap();
        s.append(&delta(2)).unwrap();
        s.append(&delta(3)).unwrap();

        // The other machine has (1,1) for "o", so it is missing (1,2) and (1,3).
        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 1));
        let mut peer = BTreeMap::new();
        peer.insert("o".to_string(), ctx);
        let lacked: Vec<u64> =
            s.deltas_peer_lacks(&peer).unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(lacked, vec![2, 3]);

        // A machine that has seen everything is sent nothing.
        let mut full = DotContext::new();
        for c in 1..=3u64 {
            full.insert(Dot::new(ReplicaId(1), c));
        }
        let mut peer2 = BTreeMap::new();
        peer2.insert("o".to_string(), full);
        assert!(s.deltas_peer_lacks(&peer2).unwrap().is_empty());
    }

    #[test]
    fn snapshot_contexts_reflects_the_last_compaction() {
        use std::collections::BTreeMap;
        use orvu_core::{SnapshotEntry, TypedCrdt};
        let mut s = MemStore::new();
        assert!(s.snapshot_contexts().unwrap().is_empty(), "empty before any compaction");

        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 5));
        let entry = SnapshotEntry {
            object_id: "o".into(),
            state: TypedCrdt::PnCounter(Default::default()).snapshot(),
            context: ctx.clone(),
        };
        s.compact(std::slice::from_ref(&entry)).unwrap();
        let floor: BTreeMap<String, DotContext> = s.snapshot_contexts().unwrap();
        assert_eq!(floor.get("o"), Some(&ctx));
    }

    #[test]
    fn memstore_compact_to_retains_the_tail() {
        use orvu_core::{SnapshotEntry, TypedCrdt};
        let mut s = MemStore::new();
        s.append(&delta(1)).unwrap();
        let entry = SnapshotEntry {
            object_id: "o".into(),
            state: TypedCrdt::PnCounter(Default::default()).snapshot(),
            context: DotContext::new(),
        };
        // Compact, keeping one delta in the log.
        s.compact_to(std::slice::from_ref(&entry), std::slice::from_ref(&delta(9))).unwrap();
        assert_eq!(s.load_snapshot().unwrap(), vec![entry], "snapshot replaced");
        let tail: Vec<u64> = s.replay().unwrap().iter().map(|d| d.dot.counter).collect();
        assert_eq!(tail, vec![9], "tail retained, not cleared");
    }
}

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
