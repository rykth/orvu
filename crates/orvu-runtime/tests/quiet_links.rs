//! A connected machine that goes silent is let go, and dialed again.
//!
//! A machine that stopped running, or whose link is gone without a word, does
//! not close its connections. Nothing more arrives on them, that is all. A
//! healthy machine sends something at least about once an anti-entropy
//! interval, a keep-alive when it has nothing else to say, so a connection
//! that stays silent much longer than that is closed, and the dialer tries
//! again. A healthy link with no changes to carry must not be closed, and
//! neither must the link of a machine that is only busy.

use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::mem::MemTransport;
use orvu_transport::{Conn, Cuts, FaultTransport, Listener, PeerAddr, Transport};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

async fn counter(h: &NodeHandle) -> i64 {
    match h.snapshot("c").await {
        Some(TypedCrdt::PnCounter(pn)) => pn.value(),
        _ => -1,
    }
}

async fn reaches(h: &NodeHandle, want: i64, bound: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + bound;
    while tokio::time::Instant::now() < deadline {
        if counter(h).await == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

async fn add(h: &NodeHandle, by: u64) {
    h.mutate("c", move |crdt, _hlc, me| match crdt {
        TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(me, by)),
        _ => unreachable!(),
    })
    .await;
}

/// Polls until `h` has no connection to `other`, or `bound` runs out.
async fn lets_go(h: &NodeHandle, other: ReplicaId, bound: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + bound;
    while tokio::time::Instant::now() < deadline {
        if !h.send_borrow_request(other, "none", 0).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn config(replica: u64, listen: &str, peers: &[&str]) -> NodeConfig {
    let mut cfg = NodeConfig::new(ReplicaId(replica), listen);
    cfg.peers = peers.iter().map(|p| PeerAddr::new(*p)).collect();
    cfg.anti_entropy_interval = Duration::from_millis(50);
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_that_goes_silent_is_let_go_and_found_again() {
    orvu_test_support::within("a machine that goes silent is let go and found again", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let cuts = Cuts::new();
        let a = Node::spawn(FaultTransport::wrap(fabric.endpoint(1), &cuts, 1), config(1, "a", &["b"]))
            .await
            .unwrap();
        let b = Node::spawn(FaultTransport::wrap(fabric.endpoint(2), &cuts, 2), config(2, "b", &[]))
            .await
            .unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        add(&a, 1).await;
        assert!(reaches(&b, 1, Duration::from_secs(5)).await, "the two never connected");

        // Everything between them is now lost without a word, as when a machine
        // freezes. Both ends must notice within a few anti-entropy intervals.
        cuts.cut(1, 2);
        assert!(lets_go(&a, ReplicaId(2), Duration::from_secs(3)).await, "a kept waiting on a silent machine");
        assert!(lets_go(&b, ReplicaId(1), Duration::from_secs(3)).await, "b kept waiting on a silent machine");

        // Once the link works again, a dials b again and b catches up.
        add(&a, 2).await;
        cuts.heal_all();
        assert!(reaches(&b, 3, Duration::from_secs(10)).await, "b never caught up after the link came back");

        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}

/// A link that counts its dials.
struct Counted(Arc<dyn Transport>, Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Transport for Counted {
    async fn dial(&self, peer: PeerAddr, cancel: CancellationToken) -> orvu_transport::Result<Box<dyn Conn>> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.dial(peer, cancel).await
    }
    async fn listen(&self, addr: &str, cancel: CancellationToken) -> orvu_transport::Result<Box<dyn Listener>> {
        self.0.listen(addr, cancel).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_healthy_link_with_nothing_to_carry_is_kept() {
    orvu_test_support::within("a healthy link with nothing to carry is kept", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let dials = Arc::new(AtomicUsize::new(0));
        let a = Node::spawn(Arc::new(Counted(fabric.endpoint(1), dials.clone())), config(1, "a", &["b"]))
            .await
            .unwrap();
        let b = Node::spawn(fabric.endpoint(2), config(2, "b", &[])).await.unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        add(&a, 1).await;
        assert!(reaches(&b, 1, Duration::from_secs(5)).await, "the two never connected");
        let before = dials.load(Ordering::SeqCst);

        // No changes for a few times the quiet limit (one second, the lowest
        // there is).
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(dials.load(Ordering::SeqCst), before, "a healthy quiet link was closed and dialed again");
        add(&a, 1).await;
        assert!(reaches(&b, 2, Duration::from_secs(2)).await);

        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}

/// When only one end stops hearing the other, that end closes the connection,
/// so the other end lets it go too, though it could still hear.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_that_stops_hearing_the_other_closes_the_link_for_both() {
    orvu_test_support::within("a machine that stops hearing the other closes the link for both", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let cuts = Cuts::new();
        let a = Node::spawn(FaultTransport::wrap(fabric.endpoint(1), &cuts, 1), config(1, "a", &["b"]))
            .await
            .unwrap();
        let b = Node::spawn(FaultTransport::wrap(fabric.endpoint(2), &cuts, 2), config(2, "b", &[]))
            .await
            .unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        add(&a, 1).await;
        assert!(reaches(&b, 1, Duration::from_secs(5)).await, "the two never connected");

        // b can still hear a, but a no longer hears b.
        cuts.cut_one_way(2, 1);
        assert!(lets_go(&b, ReplicaId(1), Duration::from_secs(3)).await, "a did not close the link it could no longer hear");

        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}

/// A store that can be made to take a long time over each save.
#[derive(Clone)]
struct Slow(orvu_store::MemStore, Arc<std::sync::atomic::AtomicU64>);

impl orvu_store::Store for Slow {
    fn append(&mut self, delta: &orvu_core::WireDelta) -> orvu_store::Result<()> {
        std::thread::sleep(Duration::from_millis(self.1.load(Ordering::SeqCst)));
        self.0.append(delta)
    }
    fn receipt(&mut self, receipt: orvu_store::Receipt) -> orvu_store::Result<()> {
        self.0.receipt(receipt)
    }
    fn receipts(&self) -> orvu_store::Result<Vec<orvu_store::Receipt>> {
        self.0.receipts()
    }
    fn replay(&self) -> orvu_store::Result<Vec<orvu_core::WireDelta>> {
        self.0.replay()
    }
    fn flush(&mut self) -> orvu_store::Result<()> {
        self.0.flush()
    }
    fn compact_to(&mut self, snapshot: &[orvu_core::SnapshotEntry], tail: &[orvu_core::WireDelta]) -> orvu_store::Result<()> {
        self.0.compact_to(snapshot, tail)
    }
    fn load_snapshot(&self) -> orvu_store::Result<Vec<orvu_core::SnapshotEntry>> {
        self.0.load_snapshot()
    }
    fn deltas_peer_lacks(
        &self,
        peer: &std::collections::BTreeMap<String, orvu_crdt::DotContext>,
    ) -> orvu_store::Result<Vec<orvu_core::WireDelta>> {
        self.0.deltas_peer_lacks(peer)
    }
    fn snapshot_contexts(&self) -> orvu_store::Result<std::collections::BTreeMap<String, orvu_crdt::DotContext>> {
        self.0.snapshot_contexts()
    }
}

/// A node whose engine is busy for longer than the quiet limit, here on a
/// save that waits for a slow disk, has no catch-up summary to send in that
/// time. The link itself is fine, so it must not be closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_engine_does_not_get_its_links_closed() {
    orvu_test_support::within("a busy engine does not get its links closed", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let dials = Arc::new(AtomicUsize::new(0));
        let delay = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut cfg_a = config(1, "a", &["b"]);
        cfg_a.fsync = orvu_store::FsyncPolicy::Always;
        let a = Node::spawn_with_store(
            Arc::new(Counted(fabric.endpoint(1), dials.clone())),
            cfg_a,
            Box::new(Slow(orvu_store::MemStore::new(), delay.clone())),
        )
        .await
        .unwrap();
        let b = Node::spawn(fabric.endpoint(2), config(2, "b", &[])).await.unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        add(&a, 1).await;
        assert!(reaches(&b, 1, Duration::from_secs(5)).await, "the two never connected");
        let before = dials.load(Ordering::SeqCst);

        // The quiet limit is one second, the lowest there is. This one save
        // takes two and a half times that, and a's engine waits for it.
        delay.store(2500, Ordering::SeqCst);
        add(&a, 1).await;
        delay.store(0, Ordering::SeqCst);
        assert!(reaches(&b, 2, Duration::from_secs(2)).await);
        assert_eq!(dials.load(Ordering::SeqCst), before, "the link of a busy node was closed and dialed again");

        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}
