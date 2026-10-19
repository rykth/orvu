//! The dialer keeps trying. A configured peer whose first dial fails must
//! still be reached on a later attempt.
//!
//! The transport sheds load: when it is over capacity it drops an incoming
//! connection, even an honest one, instead of making it wait. If the dialer
//! tried only once, a dropped dial would leave the link dead until the process
//! restarts. Shedding is only safe because the dialer tries again. It also
//! dials again when a connection it made has ended, so a machine that went
//! away and came back is reached again.

use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::mem::MemTransport;
use orvu_transport::PeerAddr;
use std::sync::Arc;
use std::time::Duration;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

async fn counter(h: &NodeHandle, obj: &str) -> i64 {
    match h.snapshot(obj).await {
        Some(TypedCrdt::PnCounter(pn)) => pn.value(),
        _ => -1,
    }
}

/// Polls until `obj` reaches `want` or `bound` runs out. Returns true only if
/// it got there in time.
async fn await_value_within(h: &NodeHandle, obj: &str, want: i64, bound: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + bound;
    while tokio::time::Instant::now() < deadline {
        if counter(h, obj).await == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    counter(h, obj).await == want
}

fn increment(
    rid: ReplicaId,
    amt: u64,
) -> impl FnOnce(&mut TypedCrdt, orvu_crdt::Hlc, ReplicaId) -> TypedDelta + Send + 'static {
    move |crdt, _hlc, _replica| match crdt {
        TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(rid, amt)),
        _ => unreachable!(),
    }
}

/// A peer whose first dial fails must connect on a later attempt.
///
/// Because the transport sheds load, an honest peer that arrives when the
/// listener is over capacity is dropped, not delayed. If the dialer tried only
/// once, that would be a dead link until restart, so shedding is only safe
/// because the dialer retries.
///
/// The first dial always fails here, with no timing involved. On
/// `MemTransport`, dialing an address with no registered listener fails at
/// once (`TransportError::Dial("no listener at ...")`). There is no timeout
/// and no race with an ICMP reply, as there would be against a dead UDP port.
/// Node B starts dialing "a" while nothing is registered as "a". A comes up
/// on that address only after a long pause, far longer than B's dialer task
/// needs to be scheduled and fail. A has no configured peers of its own, so
/// the only way A and B can end up with the same data is a later dial from B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_whose_first_dial_fails_connects_on_a_later_attempt() {
    orvu_test_support::within("a peer whose first dial fails connects on a later attempt", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());

        // B dials "a" before any listener exists there, so the first dial fails.
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(25);
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.mutate("c", increment(ReplicaId(2), 3)).await;

        // Plenty of time for B's dialer task to make its first attempt, which fails.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // A comes up late on the address B is configured to dial.
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.anti_entropy_interval = Duration::from_millis(25);
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;

        let start = std::time::Instant::now();
        let converged = await_value_within(&a, "c", 3, Duration::from_secs(5)).await;
        let elapsed = start.elapsed();
        assert!(
            converged,
            "B's first dial failed because A was not up yet, and no later dial \
             connected in {elapsed:?}; a failed dial should be tried again"
        );
        println!("late-started peer converged {elapsed:?} after A came up");

        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}

/// A machine that went away and came back must be dialed again.
///
/// B lists nobody, so it never dials. A lists B. The first B is shut down and
/// a new one comes up on the same address with nothing in it, as a machine
/// does after a restart. The only way the new B can get A's data is for A to
/// dial once more after its first connection ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_that_went_away_and_came_back_is_dialed_again() {
    orvu_test_support::within("a peer that went away and came back is dialed again", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());

        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(25);
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("c", increment(ReplicaId(1), 3)).await;

        let quiet_b = || {
            let mut cfg = NodeConfig::new(ReplicaId(2), "b");
            cfg.anti_entropy_interval = Duration::from_millis(25);
            cfg
        };
        let b = Node::spawn(fabric.endpoint(2), quiet_b()).await.unwrap();
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        assert!(
            await_value_within(&b, "c", 3, Duration::from_secs(5)).await,
            "the two never connected in the first place"
        );

        b.shutdown().await;
        a.mutate("c", increment(ReplicaId(1), 4)).await;
        // Plenty of time for A to notice that the connection has ended.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let b = Node::spawn(fabric.endpoint(2), quiet_b()).await.unwrap();
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        assert!(
            await_value_within(&b, "c", 7, Duration::from_secs(5)).await,
            "A did not dial B again after B came back, so B never caught up"
        );

        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}

/// Shutdown must finish quickly while the dialer is still retrying a peer that
/// never came up. The retry loop waits between attempts, and that wait ends as
/// soon as the cancellation token fires. So a configured peer that cannot be
/// reached does not delay `shutdown()`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_completes_while_a_configured_peer_never_comes_up() {
    orvu_test_support::within("shutdown completes while a configured peer never comes up", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());

        // "never" has no listener at any point. Every dial attempt fails, and the
        // dialer keeps retrying until it is cancelled.
        let mut cfg = NodeConfig::new(ReplicaId(2), "b");
        cfg.peers = vec![PeerAddr::new("never")];
        let b = Node::spawn(fabric.endpoint(2), cfg).await.unwrap();

        // Let a few retries pass, so the loop is in its wait between attempts.
        tokio::time::sleep(Duration::from_millis(600)).await;

        assert!(
            tokio::time::timeout(Duration::from_secs(5), b.shutdown()).await.is_ok(),
            "shutdown did not finish while the node kept dialing a machine \
             it could not reach"
        );
    })
    .await
}

/// A link whose dials never finish, as a dial to a machine that is switched
/// off does not for a long while. Everything else is the link it wraps.
struct NeverConnects(Arc<dyn orvu_transport::Transport>);

#[async_trait::async_trait]
impl orvu_transport::Transport for NeverConnects {
    async fn dial(
        &self,
        _peer: PeerAddr,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> orvu_transport::Result<Box<dyn orvu_transport::Conn>> {
        std::future::pending().await
    }
    async fn listen(
        &self,
        addr: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> orvu_transport::Result<Box<dyn orvu_transport::Listener>> {
        self.0.listen(addr, cancel).await
    }
}

/// Shutdown must not wait for a dial that is still under way. A machine that
/// is switched off does not refuse a dial, it leaves it unanswered, and the
/// dialer is in the middle of such a dial most of the time that machine is
/// away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_completes_while_a_dial_is_still_under_way() {
    orvu_test_support::within("shutdown completes while a dial is still under way", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let mut cfg = NodeConfig::new(ReplicaId(2), "b");
        cfg.peers = vec![PeerAddr::new("switched-off")];
        let b = Node::spawn(Arc::new(NeverConnects(fabric.endpoint(2))), cfg).await.unwrap();

        // Time for the dialer to start its dial.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            tokio::time::timeout(Duration::from_secs(5), b.shutdown()).await.is_ok(),
            "shutdown waited for a dial that was never going to finish"
        );
    })
    .await
}

/// A link on which every dial fails at once, and which counts them.
struct AlwaysRefused(Arc<dyn orvu_transport::Transport>, Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl orvu_transport::Transport for AlwaysRefused {
    async fn dial(
        &self,
        peer: PeerAddr,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> orvu_transport::Result<Box<dyn orvu_transport::Conn>> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(orvu_transport::TransportError::Dial(format!("cannot find {}", peer.0)))
    }
    async fn listen(
        &self,
        addr: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> orvu_transport::Result<Box<dyn orvu_transport::Listener>> {
        self.0.listen(addr, cancel).await
    }
}

/// A machine that cannot be reached is tried less and less often. A name
/// that was typed wrongly, or a machine that is gone for good, would
/// otherwise be dialed four times a second for as long as the node runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_that_cannot_be_reached_is_dialed_less_and_less_often() {
    orvu_test_support::within("a machine that cannot be reached is dialed less and less often", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cfg = NodeConfig::new(ReplicaId(2), "b");
        cfg.peers = vec![PeerAddr::new("gone")];
        let b = Node::spawn(Arc::new(AlwaysRefused(fabric.endpoint(2), dials.clone())), cfg).await.unwrap();

        // The waits are a quarter of a second, then a half, then one, then two.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let made = dials.load(std::sync::atomic::Ordering::SeqCst);
        assert!((3..=5).contains(&made), "{made} dials in two and a half seconds");
        b.shutdown().await;
    })
    .await
}

/// The longest wait between dials can be set. With a short one, a machine
/// that cannot be reached is tried more often, and one that comes back is
/// found sooner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_longest_wait_between_dials_can_be_set() {
    orvu_test_support::within("the longest wait between dials can be set", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cfg = NodeConfig::new(ReplicaId(2), "b");
        cfg.peers = vec![PeerAddr::new("gone")];
        cfg.longest_dial_wait = Duration::from_millis(250);
        let b = Node::spawn(Arc::new(AlwaysRefused(fabric.endpoint(2), dials.clone())), cfg).await.unwrap();

        // A dial every quarter of a second.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let made = dials.load(std::sync::atomic::Ordering::SeqCst);
        assert!((8..=12).contains(&made), "{made} dials in two and a half seconds");
        b.shutdown().await;
    })
    .await
}
