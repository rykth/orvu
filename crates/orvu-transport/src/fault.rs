//! A test tool that loses traffic between replicas on command.
//!
//! A `Cuts` table lists the links that are cut. Each entry is a pair of
//! replica ids with a direction. `FaultTransport` wraps any `Transport` and
//! applies the table to it. A stream that a cut has touched is "poisoned".
//!
//! What it cannot do: a transport can lose packets, but it cannot pause a
//! task or shift a clock, so it cannot imitate a stalled process. Healing a
//! cut always costs a reconnect, because a poisoned stream stays poisoned for
//! good. A write to a poisoned stream is never slowed down, so a cut link
//! never pushes back on the sender.

use crate::error::{Result, TransportError};
use crate::traits::{Conn, Listener, StreamRead, StreamWrite, Transport};
use crate::types::{parse_replica_identity_uri, Identity, PeerAddr};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;

/// The shared table of cut links. Make one per test and give a clone to every
/// endpoint.
///
/// A cut has a direction: `(from, to)` means traffic from `from` to `to` is
/// lost. A one-way cut is half of a full cut, not a separate mode.
#[derive(Default)]
pub struct Cuts {
    state: Mutex<HashSet<(u64, u64)>>,
    wakers: Mutex<Vec<Waker>>,
    poisoned: AtomicU64,
    /// Which replica listens on which address, recorded by
    /// `FaultTransport::listen`. `Conn::identity()` is only available after a
    /// dial, so this map is the only way `dial` can tell who it is about to
    /// dial and refuse.
    addrs: Mutex<HashMap<String, u64>>,
}

impl Cuts {
    /// Make an empty table, with no links cut.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Lose traffic in both directions between `a` and `b`.
    pub fn cut(&self, a: u64, b: u64) {
        {
            let mut s = self.state.lock().unwrap();
            s.insert((a, b));
            s.insert((b, a));
        }
        self.wake_all();
    }

    /// Lose traffic from `from` to `to` only. Traffic the other way keeps
    /// flowing.
    pub fn cut_one_way(&self, from: u64, to: u64) {
        self.state.lock().unwrap().insert((from, to));
        self.wake_all();
    }

    /// Restore both directions between `a` and `b`. Every stream the cut
    /// poisoned ends instead of carrying on. See the module docs.
    pub fn heal(&self, a: u64, b: u64) {
        {
            let mut s = self.state.lock().unwrap();
            s.remove(&(a, b));
            s.remove(&(b, a));
        }
        self.wake_all();
    }

    /// Restore every link.
    pub fn heal_all(&self) {
        self.state.lock().unwrap().clear();
        self.wake_all();
    }

    /// Cut `node` off from each of `peers`, leaving the peers connected to
    /// each other.
    pub fn isolate(&self, node: u64, peers: &[u64]) {
        for &p in peers {
            self.cut(node, p);
        }
    }

    /// How many stream halves have been poisoned since the table was made.
    /// Check that this is above zero in every test that cuts links between
    /// several replicas. A cut aimed at the wrong replica id cuts nothing, and
    /// the test then passes for an unrelated reason. Never check for an exact
    /// number, because both ends of a link count for the same direction.
    pub fn poisoned_count(&self) -> u64 {
        self.poisoned.load(Ordering::Relaxed)
    }

    pub(crate) fn is_cut(&self, from: u64, to: u64) -> bool {
        self.state.lock().unwrap().contains(&(from, to))
    }

    /// Park the waker `w` if this direction is cut right now, and report
    /// whether it was parked. A parked waker is woken the next time the table
    /// changes. The check and the push happen under the same `state` lock, so
    /// a heal cannot land between them and wake an empty list, which would
    /// leave `w` never woken. `park` does not have this guarantee.
    pub(crate) fn park_if_cut(&self, from: u64, to: u64, w: Waker) -> bool {
        let state = self.state.lock().unwrap();
        if state.contains(&(from, to)) {
            self.push_waker(w);
            true
        } else {
            false
        }
    }

    /// Park `w` whatever the table says. This is for a reader that is waiting
    /// inside its inner stream and must still hear about a later cut.
    ///
    /// This is not tied to the check that `advance` makes. A cut that lands
    /// between that check and this call wakes the list before `w` is on it,
    /// so `w` is not woken until the table changes again. That can only delay
    /// poisoning. It cannot make a test pass wrongly, because the peer's
    /// write half poisons on its own and no byte arrives in the meantime.
    pub(crate) fn park(&self, w: Waker) {
        self.push_waker(w);
    }

    /// Keeps the list from growing without limit: a waker that would wake the
    /// same task as one already parked is not added again.
    fn push_waker(&self, w: Waker) {
        let mut wakers = self.wakers.lock().unwrap();
        if !wakers.iter().any(|parked| parked.will_wake(&w)) {
            wakers.push(w);
        }
    }

    pub(crate) fn note_poisoned(&self) {
        self.poisoned.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_listen(&self, addr: &str, replica: u64) {
        self.addrs.lock().unwrap().insert(addr.to_string(), replica);
    }

    pub(crate) fn replica_at(&self, addr: &str) -> Option<u64> {
        self.addrs.lock().unwrap().get(addr).copied()
    }

    /// Take every parked waker off the list and wake it.
    ///
    /// Locks are taken in the order `state` then `wakers`, as `park_if_cut`
    /// does. So never call this while holding `state`. A woken task that
    /// parks again needs `state`, and the caller would deadlock on the lock
    /// it already holds.
    ///
    /// The `wakers` lock is released at the end of the `let`, before any
    /// waker runs. A woken task may park again straight away, and that would
    /// deadlock if the lock were still held.
    fn wake_all(&self) {
        let ws: Vec<Waker> = std::mem::take(&mut *self.wakers.lock().unwrap());
        for w in ws {
            w.wake();
        }
    }
}

/// The state of one wrapped stream half. It only moves forward: `Clear ->
/// Poisoned -> Dead`. A poisoned half must never go back to `Clear`. If a
/// stream carried on after a gap, the reader would lose its place in the
/// message framing. It would hold the header of a frame whose tail was
/// dropped, and read the next frame's bytes as that missing tail. Because
/// poisoning is permanent, that cannot happen at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Clear,
    Poisoned,
    Dead,
}

/// Move a half's state forward according to the table. `from`/`to` is the
/// direction this half carries. The two halves of one stream carry opposite
/// directions, as `FaultConn::wrap_halves` explains.
fn advance(phase: &mut Phase, cuts: &Cuts, from: u64, to: u64) -> Phase {
    let cut = cuts.is_cut(from, to);
    match (*phase, cut) {
        (Phase::Clear, true) => {
            cuts.note_poisoned();
            *phase = Phase::Poisoned;
        }
        (Phase::Poisoned, false) => *phase = Phase::Dead,
        _ => {}
    }
    *phase
}

struct FaultWrite {
    inner: StreamWrite,
    cuts: Arc<Cuts>,
    from: u64,
    to: u64,
    phase: Phase,
}

impl AsyncWrite for FaultWrite {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match advance(&mut this.phase, &this.cuts, this.from, this.to) {
            Phase::Clear => Pin::new(&mut this.inner).poll_write(cx, buf),
            // The write reports success and the bytes are dropped. A node
            // that is cut off must not be able to tell. The whole buffer must
            // be reported as written, or `write_all` loops forever.
            Phase::Poisoned => Poll::Ready(Ok(buf.len())),
            Phase::Dead => Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "link cut"))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match advance(&mut this.phase, &this.cuts, this.from, this.to) {
            Phase::Clear => Pin::new(&mut this.inner).poll_flush(cx),
            // A dropped write leaves nothing to flush.
            _ => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match advance(&mut this.phase, &this.cuts, this.from, this.to) {
            Phase::Clear => Pin::new(&mut this.inner).poll_shutdown(cx),
            _ => Poll::Ready(Ok(())),
        }
    }
}

struct FaultRead {
    inner: StreamRead,
    cuts: Arc<Cuts>,
    from: u64,
    to: u64,
    phase: Phase,
}

impl AsyncRead for FaultRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match advance(&mut this.phase, &this.cuts, this.from, this.to) {
            Phase::Clear => {
                let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
                if polled.is_pending() {
                    // Our waker is now held by the inner stream, and after a
                    // cut no byte will arrive to fire it. Park it in the
                    // table too, so a cut that lands now still wakes us.
                    this.cuts.park(cx.waker().clone());
                }
                polled
            }
            // Stay quiet instead of failing, so this looks the same as a peer
            // with nothing to send. The waker is parked so a heal can wake
            // us. Every `Cuts` method that changes the table wakes the list.
            Phase::Poisoned => {
                if this
                    .cuts
                    .park_if_cut(this.from, this.to, cx.waker().clone())
                {
                    Poll::Pending
                } else {
                    this.phase = Phase::Dead;
                    Poll::Ready(Ok(()))
                }
            }
            // End of stream: `Ready(Ok(()))` with nothing filled in. A
            // caller's read loop ends on this, and the caller can then
            // reconnect.
            Phase::Dead => Poll::Ready(Ok(())),
        }
    }
}

fn remote_id(conn: &dyn Conn) -> Option<u64> {
    conn.identity()
        .uri
        .as_deref()
        .and_then(parse_replica_identity_uri)
}

struct FaultConn {
    inner: Box<dyn Conn>,
    cuts: Arc<Cuts>,
    local: u64,
    remote: Option<u64>,
}

impl FaultConn {
    /// The two halves of one stream check different directions. The write
    /// half sends `local -> remote` and the read half receives `remote ->
    /// local`. Swapping them would reverse every one-way test with no error.
    fn wrap_halves(&self, w: StreamWrite, r: StreamRead) -> (StreamWrite, StreamRead) {
        let Some(remote) = self.remote else {
            // We do not know who the peer is, so there is no pair to look
            // up. The stream passes through uncut.
            return (w, r);
        };
        (
            Box::new(FaultWrite {
                inner: w,
                cuts: self.cuts.clone(),
                from: self.local,
                to: remote,
                phase: Phase::Clear,
            }),
            Box::new(FaultRead {
                inner: r,
                cuts: self.cuts.clone(),
                from: remote,
                to: self.local,
                phase: Phase::Clear,
            }),
        )
    }
}

#[async_trait]
impl Conn for FaultConn {
    async fn open_stream(&self) -> Result<(StreamWrite, StreamRead)> {
        let (w, r) = self.inner.open_stream().await?;
        Ok(self.wrap_halves(w, r))
    }
    async fn accept_stream(&self) -> Result<(StreamWrite, StreamRead)> {
        let (w, r) = self.inner.accept_stream().await?;
        Ok(self.wrap_halves(w, r))
    }
    fn identity(&self) -> Identity {
        self.inner.identity()
    }
    async fn close(&self) {
        self.inner.close().await
    }
}

struct FaultListener {
    inner: Box<dyn Listener>,
    cuts: Arc<Cuts>,
    local: u64,
}

#[async_trait]
impl Listener for FaultListener {
    fn local_addr(&self) -> String {
        self.inner.local_addr()
    }
    async fn accept(&self) -> Result<Box<dyn Conn>> {
        let conn = self.inner.accept().await?;
        let remote = remote_id(conn.as_ref());
        Ok(Box::new(FaultConn {
            inner: conn,
            cuts: self.cuts.clone(),
            local: self.local,
            remote,
        }))
    }
}

/// A `Transport` that loses traffic according to a shared `Cuts` table.
///
/// It can wrap any transport. All it needs is `Conn::identity()`, to work out
/// which pair of replicas a connection joins, so it works over
/// `MemTransport`. Cutting streams would work over `QuicTransport` unchanged.
/// Refusing dials would not work across processes. `dial` looks the peer up
/// in the `addrs` map, which lives in one process, so in another process the
/// lookup finds nothing and the dial is not refused.
pub struct FaultTransport {
    inner: Arc<dyn Transport>,
    cuts: Arc<Cuts>,
    local: u64,
}

impl FaultTransport {
    /// Wrap `inner` so its traffic follows `cuts`. `local` is the replica id
    /// of this endpoint.
    pub fn wrap(inner: Arc<dyn Transport>, cuts: &Arc<Cuts>, local: u64) -> Arc<dyn Transport> {
        Arc::new(Self {
            inner,
            cuts: cuts.clone(),
            local,
        })
    }
}

#[async_trait]
impl Transport for FaultTransport {
    async fn dial(&self, peer: PeerAddr, cancel: CancellationToken) -> Result<Box<dyn Conn>> {
        // Refuse before calling the inner transport. Otherwise the inner
        // transport hands the connection to the listener, and the handshake
        // with the cut peer stalls for its full timeout. Failing here lets
        // the dialer retry at its own pace. A cut in either direction is
        // enough to refuse, because the handshake needs both.
        if let Some(remote) = self.cuts.replica_at(&peer.0) {
            if self.cuts.is_cut(self.local, remote) || self.cuts.is_cut(remote, self.local) {
                return Err(TransportError::Dial(format!(
                    "cut: {} <-> {remote}",
                    self.local
                )));
            }
        }
        let conn = self.inner.dial(peer, cancel).await?;
        let remote = remote_id(conn.as_ref());
        Ok(Box::new(FaultConn {
            inner: conn,
            cuts: self.cuts.clone(),
            local: self.local,
            remote,
        }))
    }

    async fn listen(&self, addr: &str, cancel: CancellationToken) -> Result<Box<dyn Listener>> {
        let inner = self.inner.listen(addr, cancel).await?;
        // Record the address that was really bound, so a listen on port 0
        // can still be looked up.
        self.cuts.note_listen(&inner.local_addr(), self.local);
        Ok(Box::new(FaultListener {
            inner,
            cuts: self.cuts.clone(),
            local: self.local,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_frame, write_frame};
    use crate::mem::MemTransport;
    use std::task::Wake;
    use std::time::Duration;

    struct CountWaker(AtomicU64);
    impl Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_cut_is_symmetric_and_a_one_way_cut_is_not() {
        let c = Cuts::new();
        c.cut(1, 2);
        assert!(
            c.is_cut(1, 2) && c.is_cut(2, 1),
            "cut must block both directions"
        );
        let c = Cuts::new();
        c.cut_one_way(1, 2);
        assert!(c.is_cut(1, 2), "the named direction must be cut");
        assert!(
            !c.is_cut(2, 1),
            "the reverse direction must still flow"
        );
    }

    #[test]
    fn an_uncut_pair_is_not_cut() {
        let c = Cuts::new();
        c.cut(1, 2);
        assert!(!c.is_cut(1, 3));
        assert!(!c.is_cut(3, 1));
    }

    #[test]
    fn heal_removes_both_directions_and_heal_all_clears() {
        let c = Cuts::new();
        c.cut(1, 2);
        c.heal(1, 2);
        assert!(!c.is_cut(1, 2) && !c.is_cut(2, 1));
        c.cut(1, 2);
        c.cut(1, 3);
        c.heal_all();
        assert!(!c.is_cut(1, 2) && !c.is_cut(1, 3));
    }

    #[test]
    fn heal_of_a_one_way_cut_removes_it() {
        let c = Cuts::new();
        c.cut_one_way(1, 2);
        c.heal(1, 2);
        assert!(!c.is_cut(1, 2));
    }

    #[test]
    fn isolate_cuts_the_node_from_every_listed_peer_both_ways() {
        let c = Cuts::new();
        c.isolate(1, &[2, 3]);
        for p in [2, 3] {
            assert!(c.is_cut(1, p), "1 -> {p} must be cut");
            assert!(c.is_cut(p, 1), "{p} -> 1 must be cut");
        }
        assert!(
            !c.is_cut(2, 3),
            "isolate must not cut the peers from EACH OTHER"
        );
    }

    #[test]
    fn every_mutation_wakes_parked_readers() {
        let c = Cuts::new();
        c.cut_one_way(8, 9);
        let counter = Arc::new(CountWaker(AtomicU64::new(0)));
        c.park_if_cut(8, 9, Waker::from(counter.clone()));
        c.cut(1, 2);
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "a cut must wake parked readers"
        );
        c.park_if_cut(1, 2, Waker::from(counter.clone()));
        c.heal(1, 2);
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            2,
            "a HEAL must wake them too, or it hangs"
        );
    }

    #[test]
    fn a_woken_waker_is_not_woken_twice() {
        let c = Cuts::new();
        c.cut_one_way(8, 9);
        let counter = Arc::new(CountWaker(AtomicU64::new(0)));
        c.park_if_cut(8, 9, Waker::from(counter.clone()));
        c.cut(1, 2);
        c.cut(1, 3);
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "the park list must be drained on wake"
        );
    }

    #[test]
    fn park_if_cut_parks_only_a_cut_direction() {
        let c = Cuts::new();
        let counter = Arc::new(CountWaker(AtomicU64::new(0)));
        assert!(
            !c.park_if_cut(1, 2, Waker::from(counter.clone())),
            "an uncut pair must not park"
        );
        c.cut(3, 4);
        c.cut_one_way(1, 2);
        assert!(
            !c.park_if_cut(2, 1, Waker::from(counter.clone())),
            "the uncut reverse must not park"
        );
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        c.heal_all(); // wakes everything parked, so the calls above parked nothing
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            0,
            "a refused park must leave nothing to wake"
        );
        c.cut(1, 2);
        assert!(
            c.park_if_cut(1, 2, Waker::from(counter.clone())),
            "a cut direction must park"
        );
        c.heal(1, 2);
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "the parked waker must be woken by the heal"
        );
    }

    #[test]
    fn poisoned_count_starts_at_zero_and_counts_notes() {
        let c = Cuts::new();
        assert_eq!(c.poisoned_count(), 0);
        c.note_poisoned();
        c.note_poisoned();
        assert_eq!(c.poisoned_count(), 2);
    }

    #[test]
    fn a_listened_address_resolves_to_its_replica() {
        let c = Cuts::new();
        c.note_listen("node-7", 7);
        assert_eq!(c.replica_at("node-7"), Some(7));
        assert_eq!(
            c.replica_at("node-9"),
            None,
            "an unknown address must not resolve"
        );
    }

    #[test]
    fn every_mutator_wakes_parked_wakers() {
        for which in 0..4 {
            let c = Cuts::new();
            c.cut_one_way(8, 9); // so heal/heal_all have something to do
            let counter = Arc::new(CountWaker(AtomicU64::new(0)));
            c.park_if_cut(8, 9, Waker::from(counter.clone()));
            match which {
                0 => c.cut(1, 2),
                1 => c.cut_one_way(1, 2),
                2 => c.heal(1, 2),
                _ => c.heal_all(),
            }
            assert_eq!(
                counter.0.load(Ordering::SeqCst),
                1,
                "mutator #{which} must wake parked wakers"
            );
        }
    }

    /// A waker that parks itself again, once, from inside `wake`. If
    /// `wake_all` still held the wakers lock while calling it, that would
    /// deadlock.
    struct ReparkWaker {
        cuts: Arc<Cuts>,
        woken: AtomicU64,
    }
    impl Wake for ReparkWaker {
        fn wake(self: Arc<Self>) {
            if self.woken.fetch_add(1, Ordering::SeqCst) == 0 {
                self.cuts.park_if_cut(8, 9, Waker::from(self.clone()));
            }
        }
    }

    #[test]
    fn wake_all_releases_its_lock_before_invoking_wakers() {
        let c = Cuts::new();
        c.cut_one_way(8, 9);
        let w = Arc::new(ReparkWaker {
            cuts: c.clone(),
            woken: AtomicU64::new(0),
        });
        c.park_if_cut(8, 9, Waker::from(w.clone()));
        let (tx, rx) = std::sync::mpsc::channel();
        let c2 = c.clone();
        std::thread::spawn(move || {
            c2.cut(1, 2);
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("wake_all deadlocked: it held the wakers lock across wake()");
        assert_eq!(w.woken.load(Ordering::SeqCst), 1);
        // The waker parked itself again, so the next change to the table wakes it.
        c.cut(1, 3);
        assert_eq!(w.woken.load(Ordering::SeqCst), 2);
    }

    /// Two wrapped endpoints over one in-memory transport, already connected,
    /// with one stream open between them. Returns (write-half at 1,
    /// read-half at 2).
    async fn linked(cuts: &Arc<Cuts>) -> (crate::traits::StreamWrite, crate::traits::StreamRead) {
        let fabric = Arc::new(MemTransport::new());
        let one = FaultTransport::wrap(fabric.endpoint(1), cuts, 1);
        let two = FaultTransport::wrap(fabric.endpoint(2), cuts, 2);
        let cancel = CancellationToken::new();
        let listener = two.listen("node-2", cancel.clone()).await.unwrap();
        let dialed = tokio::spawn({
            let one = one.clone();
            let cancel = cancel.clone();
            async move { one.dial(PeerAddr::new("node-2"), cancel).await.unwrap() }
        });
        let accepted = listener.accept().await.unwrap();
        let dialed = dialed.await.unwrap();
        let (w, _) = dialed.open_stream().await.unwrap();
        let (_, r) = accepted.accept_stream().await.unwrap();
        (w, r)
    }

    #[tokio::test]
    async fn bytes_flow_until_a_cut_then_the_reader_hears_nothing() {
        let cuts = Cuts::new();
        let (mut w, mut r) = linked(&cuts).await;
        write_frame(&mut w, b"before").await.unwrap();
        assert_eq!(read_frame(&mut r).await.unwrap(), b"before");

        cuts.cut(1, 2);
        write_frame(&mut w, b"after")
            .await
            .expect("a poisoned write must SUCCEED, not error");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), read_frame(&mut r))
                .await
                .is_err(),
            "a cut reader must stay quiet, not EOF and not error"
        );
        assert!(
            cuts.poisoned_count() > 0,
            "the cut must actually have bitten"
        );
    }

    #[tokio::test]
    async fn a_heal_kills_the_poisoned_stream_so_reconnect_can_rebuild() {
        let cuts = Cuts::new();
        let (mut w, mut r) = linked(&cuts).await;
        write_frame(&mut w, b"x").await.unwrap();
        let _ = read_frame(&mut r).await.unwrap();

        cuts.cut(1, 2);
        // Start a read on the poisoned stream first, then heal. This is the
        // path that hangs if a woken reader does not check the table again.
        let reader = tokio::spawn(async move { read_frame(&mut r).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cuts.heal(1, 2);

        let got = tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .expect("heal must wake the parked reader — a hang here is the bug")
            .unwrap();
        assert!(
            got.is_err(),
            "a healed stream must be DEAD (EOF/error), never resume"
        );
    }

    #[tokio::test]
    async fn a_one_way_cut_leaves_the_reverse_direction_working() {
        // This test fixes which direction each half checks. If the write half
        // checked (remote, local) or the read half checked (local, remote),
        // only this test catches it. Without it, every one-way test would
        // check the opposite of what it says and still pass.
        let cuts = Cuts::new();
        let fabric = Arc::new(MemTransport::new());
        let one = FaultTransport::wrap(fabric.endpoint(1), &cuts, 1);
        let two = FaultTransport::wrap(fabric.endpoint(2), &cuts, 2);
        let cancel = CancellationToken::new();
        let l1 = one.listen("node-1", cancel.clone()).await.unwrap();
        let dialed = tokio::spawn({
            let two = two.clone();
            let cancel = cancel.clone();
            async move { two.dial(PeerAddr::new("node-1"), cancel).await.unwrap() }
        });
        let at_one = l1.accept().await.unwrap();
        let at_two = dialed.await.unwrap();

        // 1 -> 2 is cut. 2 -> 1 must still work.
        cuts.cut_one_way(1, 2);

        let (mut w2, _) = at_two.open_stream().await.unwrap();
        let (_, mut r1) = at_one.accept_stream().await.unwrap();
        write_frame(&mut w2, b"two-to-one").await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), read_frame(&mut r1))
                .await
                .expect("2 -> 1 is NOT cut and must arrive")
                .unwrap(),
            b"two-to-one"
        );

        let (mut w1, _) = at_one.open_stream().await.unwrap();
        let (_, mut r2) = at_two.accept_stream().await.unwrap();
        write_frame(&mut w1, b"one-to-two").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), read_frame(&mut r2))
                .await
                .is_err(),
            "1 -> 2 IS cut and must be lost"
        );
    }

    #[tokio::test]
    async fn a_conn_whose_peer_cannot_be_identified_is_never_cut() {
        // The plain in-memory transport gives a connection no identity, so
        // there is no pair of replicas to look up, and the connection is left
        // alone. Cutting traffic we cannot place would cause faults no test
        // asked for.
        let cuts = Cuts::new();
        let fabric = Arc::new(MemTransport::new());
        let one = FaultTransport::wrap(
            fabric.clone() as Arc<dyn crate::traits::Transport>,
            &cuts,
            1,
        );
        let cancel = CancellationToken::new();
        let listener = one.listen("anon", cancel.clone()).await.unwrap();
        let dialed = tokio::spawn({
            let one = one.clone();
            let cancel = cancel.clone();
            async move { one.dial(PeerAddr::new("anon"), cancel).await.unwrap() }
        });
        let accepted = listener.accept().await.unwrap();
        let dialed = dialed.await.unwrap();
        cuts.cut(1, 2);
        let (mut w, _) = dialed.open_stream().await.unwrap();
        let (_, mut r) = accepted.accept_stream().await.unwrap();
        write_frame(&mut w, b"anon").await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), read_frame(&mut r))
                .await
                .expect("an unattributable conn must pass through")
                .unwrap(),
            b"anon"
        );
    }

    #[tokio::test]
    async fn a_dial_between_cut_peers_is_refused() {
        let cuts = Cuts::new();
        let fabric = Arc::new(MemTransport::new());
        let one = FaultTransport::wrap(fabric.endpoint(1), &cuts, 1);
        let two = FaultTransport::wrap(fabric.endpoint(2), &cuts, 2);
        let cancel = CancellationToken::new();
        let _l = two.listen("node-2", cancel.clone()).await.unwrap();
        cuts.cut(1, 2);
        assert!(
            one.dial(PeerAddr::new("node-2"), cancel.clone())
                .await
                .is_err(),
            "dialling across a cut must fail fast rather than stall a handshake"
        );
        cuts.heal(1, 2);
        assert!(
            one.dial(PeerAddr::new("node-2"), cancel).await.is_ok(),
            "a healed pair must dial again"
        );
    }

    #[tokio::test]
    async fn a_reader_blocked_before_the_cut_still_poisons_and_then_dies_on_heal() {
        let cuts = Cuts::new();
        let (_w, mut r) = linked(&cuts).await;
        // Nothing has been written, so this read waits inside the inner stream.
        let reader = tokio::spawn(async move { read_frame(&mut r).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cuts.cut(1, 2);
        tokio::time::timeout(Duration::from_secs(2), async {
            while cuts.poisoned_count() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a reader blocked before the cut must be woken and poison");
        cuts.heal(1, 2);
        let got = tokio::time::timeout(Duration::from_secs(2), reader)
            .await
            .expect("heal must kill a reader that was blocked before the cut")
            .unwrap();
        assert!(got.is_err(), "a healed stream must be dead");
    }

    #[tokio::test]
    async fn a_dead_write_half_is_broken_pipe_and_flush_and_shutdown_stay_ok() {
        use tokio::io::AsyncWriteExt;
        let cuts = Cuts::new();
        let (mut w, _r) = linked(&cuts).await;
        cuts.cut(1, 2);
        w.write_all(b"x").await.unwrap(); // Clear -> Poisoned
        w.flush().await.expect("a poisoned flush is Ok");
        cuts.heal(1, 2);
        let e = w
            .write_all(b"y")
            .await
            .expect_err("a dead write half must error");
        assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe);
        w.flush().await.expect("a dead flush is Ok");
        w.shutdown().await.expect("a dead shutdown is Ok");
    }

    #[tokio::test]
    async fn a_dial_is_refused_when_only_the_reverse_direction_is_cut() {
        let cuts = Cuts::new();
        let fabric = Arc::new(MemTransport::new());
        let one = FaultTransport::wrap(fabric.endpoint(1), &cuts, 1);
        let two = FaultTransport::wrap(fabric.endpoint(2), &cuts, 2);
        let cancel = CancellationToken::new();
        let _l = two.listen("node-2", cancel.clone()).await.unwrap();
        cuts.cut_one_way(2, 1);
        assert!(
            one.dial(PeerAddr::new("node-2"), cancel).await.is_err(),
            "a cut in EITHER direction must refuse the dial"
        );
    }

    #[test]
    fn the_same_waker_is_not_parked_twice() {
        let c = Cuts::new();
        c.cut_one_way(8, 9);
        let counter = Arc::new(CountWaker(AtomicU64::new(0)));
        let w = Waker::from(counter.clone());
        c.park(w.clone());
        c.park(w.clone());
        c.park_if_cut(8, 9, w);
        c.cut(1, 2);
        assert_eq!(
            counter.0.load(Ordering::SeqCst),
            1,
            "an equivalent waker must be parked once"
        );
    }

    #[tokio::test]
    async fn a_poisoned_write_never_reports_a_short_write() {
        // A write that reports fewer bytes than it was given makes
        // `write_all` loop forever on a cut link.
        use tokio::io::AsyncWriteExt;
        let cuts = Cuts::new();
        let (mut w, _r) = linked(&cuts).await;
        cuts.cut(1, 2);
        let big = vec![7u8; 256 * 1024];
        let n = w.write(&big).await.unwrap();
        assert_eq!(
            n,
            big.len(),
            "a poisoned write must consume the WHOLE buffer"
        );
        tokio::time::timeout(Duration::from_secs(2), w.write_all(&big))
            .await
            .expect("write_all must not hang on a poisoned stream")
            .unwrap();
    }
}
