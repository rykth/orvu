//! Tests for the loop that accepts connections from other nodes (the "mesh"
//! listener). It has to keep working when a stranger connects, when a
//! connection stalls, and when many stalled connections arrive at once. It also
//! has to stay open to new members.
//!
//! No connection, and no flood of connections, from anyone may stop the
//! listener from letting honest machines in. Most tests here check that an
//! honest machine still ends up with the same data. The last test covers open
//! membership. There is no fixed list of members, so a replica that is not in
//! `cfg.peers` must still be able to join. That test fails if anyone adds a
//! membership check to this loop.

use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::{generate_dev_ca, issue_node_cert, DevCa, PeerAddr, QuicTransport, Transport};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

/// How long any one step that waits on the network may take before the test
/// fails. Far more than any step needs, even on a busy machine, and far less
/// than a stuck run that nobody notices.
const STEP: Duration = Duration::from_secs(30);

/// Waits for `step`, or fails the test naming `what` once `STEP` has passed.
/// Every wait on a node or the network goes through here, so a step that
/// never finishes becomes a failure that says which step it was.
async fn within<T>(what: &str, step: impl Future<Output = T>) -> T {
    orvu_test_support::within(what, STEP, step).await
}

fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A QUIC transport that holds a certificate for `replica`, issued by the
/// fleet's certificate authority `ca`.
fn fleet_transport(ca: &DevCa, replica: u64) -> Arc<dyn Transport> {
    let c = issue_node_cert(ca, replica);
    Arc::new(QuicTransport::new(&c.cert_pem, &c.key_pem, &ca.cert_pem).unwrap())
}

/// A free 127.0.0.1 UDP port for a listener, as "ip:port".
fn free_udp_addr() -> String {
    format!("127.0.0.1:{}", free_udp_port())
}

fn free_udp_port() -> u16 {
    orvu_test_support::free_port()
}

async fn counter(h: &NodeHandle, obj: &str) -> i64 {
    match within("reading the counter", h.snapshot(obj)).await {
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

fn increment(rid: ReplicaId, amt: u64) -> impl FnOnce(&mut TypedCrdt, orvu_crdt::Hlc, ReplicaId) -> TypedDelta + Send + 'static {
    move |crdt, _hlc, _replica| match crdt {
        TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(rid, amt)),
        _ => unreachable!(),
    }
}

/// Starts the listening node under test. It is replica 1 and has no configured
/// peers, so it only accepts connections. Catch-up between machines runs every
/// 25ms, so the time to agree reflects how fast connections are accepted and
/// not how long the catch-up timer waits.
async fn spawn_listener(ca: &DevCa) -> (NodeHandle, String) {
    let addr = free_udp_addr();
    let mut cfg = NodeConfig::new(ReplicaId(1), addr.clone());
    cfg.anti_entropy_interval = Duration::from_millis(25);
    let h = within("starting the listener", Node::spawn(fleet_transport(ca, 1), cfg)).await.unwrap();
    (h, addr)
}

/// Starts an honest node that dials `listener_addr`.
async fn spawn_dialer(ca: &DevCa, replica: u64, listener_addr: &str) -> NodeHandle {
    let addr = free_udp_addr();
    let mut cfg = NodeConfig::new(ReplicaId(replica), addr);
    cfg.peers = vec![PeerAddr::new(listener_addr)];
    cfg.anti_entropy_interval = Duration::from_millis(25);
    within("starting a dialer", Node::spawn(fleet_transport(ca, replica), cfg)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_handshake_does_not_kill_the_mesh_listener() {
    install_crypto();
    let ca = generate_dev_ca();
    // A stranger whose certificate comes from a different authority, so it has
    // no fleet credentials at all.
    let rogue_ca = generate_dev_ca();
    let rogue = fleet_transport(&rogue_ca, 99);

    let (a, addr_a) = spawn_listener(&ca).await;
    within("creating the counter", a.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The stranger tries to connect once. The dial fails, because mutual TLS
    // rejects a certificate from another authority. The test is about what
    // that does to the listener.
    let bad = within("the stranger's dial", rogue.dial(PeerAddr::new(addr_a.clone()), Default::default())).await;
    assert!(bad.is_err(), "the rogue CA must be rejected by mutual TLS");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // An honest machine from the fleet must still be able to connect and end
    // up with the same data.
    let b = spawn_dialer(&ca, 2, &addr_a).await;
    within("creating the counter", b.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    within("changing the counter", b.mutate("c", increment(ReplicaId(2), 3))).await;
    assert!(
        await_value_within(&a, "c", 3, Duration::from_secs(3)).await,
        "after one failed handshake, the listener never let the honest machine in"
    );

    within("shutting a down", a.shutdown()).await;
    within("shutting b down", b.shutdown()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_connection_does_not_block_honest_peers() {
    install_crypto();
    let ca = generate_dev_ca();
    let (a, addr_a) = spawn_listener(&ca).await;
    within("creating the counter", a.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Someone with valid fleet credentials connects and never opens a stream.
    // We keep the connection for the whole test. Dropping it would close it,
    // and that would free an accept loop that was stuck waiting on it, so the
    // test would pass when it should fail.
    let staller_t = fleet_transport(&ca, 7);
    let _staller = within("the staller's dial", staller_t.dial(PeerAddr::new(addr_a.clone()), Default::default()))
        .await
        .expect("the staller holds fleet credentials, so TLS admits it");
    tokio::time::sleep(Duration::from_millis(200)).await; // the listener accepts the stalled connection first

    // The honest machine must still connect and agree within a few seconds.
    // That is far more than it normally takes (milliseconds). It is far less
    // than the 10s handshake timeout, which is when a loop that waited for
    // each handshake in turn would move on.
    let b = spawn_dialer(&ca, 2, &addr_a).await;
    within("creating the counter", b.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    within("changing the counter", b.mutate("c", increment(ReplicaId(2), 3))).await;
    assert!(
        await_value_within(&a, "c", 3, Duration::from_secs(3)).await,
        "a stalled connection with valid credentials kept the honest machine out"
    );

    within("shutting a down", a.shutdown()).await;
    within("shutting b down", b.shutdown()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_stalled_connections_does_not_delay_an_honest_peer() {
    install_crypto();
    // 64 stalled connections at once. That is more than any fixed limit a
    // listener is likely to have, and cheap enough to keep the test fast
    // (about 64 short-lived UDP sockets).
    const FLOOD: usize = 64;

    let ca = generate_dev_ca();
    let (a, addr_a) = spawn_listener(&ca).await;
    within("creating the counter", a.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start the flood. Each task connects, which mutual TLS allows because the
    // certificate is from the fleet. It then holds the connection and never
    // opens a stream. We do not wait for every dial to finish. A stuck accept
    // loop stops letting connections in, so waiting for all of them would hang
    // the test on the very fault it checks for. The 1s sleep gives the flood
    // time to arrive first.
    let flood_t = fleet_transport(&ca, 7);
    for _ in 0..FLOOD {
        let t = flood_t.clone();
        let addr = addr_a.clone();
        tokio::spawn(async move {
            if let Ok(conn) = t.dial(PeerAddr::new(addr), Default::default()).await {
                tokio::time::sleep(Duration::from_secs(60)).await; // longer than the test runs
                drop(conn);
            }
        });
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    // The honest machine must get all the way through quickly despite the
    // flood: dial, handshake and sync. It normally takes well under a second.
    // The bound is 8s because a machine busy with many other test programs
    // has been seen to need over 5s. It stays below the 10s handshake
    // timeout, which is what a listener that served the flood first would
    // make the honest machine wait.
    let start = std::time::Instant::now();
    let b = spawn_dialer(&ca, 2, &addr_a).await;
    within("creating the counter", b.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    within("changing the counter", b.mutate("c", increment(ReplicaId(2), 3))).await;
    let converged = await_value_within(&a, "c", 3, Duration::from_secs(8)).await;
    let elapsed = start.elapsed();
    println!("honest peer converged under a {FLOOD}-connection flood in {elapsed:?}");
    assert!(
        converged,
        "the honest machine had not caught up after {elapsed:?}, \
         with {FLOOD} stalled connections ahead of it"
    );

    within("shutting a down", a.shutdown()).await;
    within("shutting b down", b.shutdown()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_completes_while_a_stalled_peer_is_in_flight() {
    install_crypto();
    let ca = generate_dev_ca();
    let (a, addr_a) = spawn_listener(&ca).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A machine with valid fleet credentials connects and never finishes its
    // hello, the first message of the handshake. We keep the connection for
    // the whole test. Dropping it would end the handshake early, and the test
    // would pass when it should fail.
    let staller_t = fleet_transport(&ca, 7);
    let _staller = within("the staller's dial", staller_t.dial(PeerAddr::new(addr_a.clone()), Default::default()))
        .await
        .expect("the staller holds fleet credentials, so TLS admits it");
    tokio::time::sleep(Duration::from_millis(200)).await; // its handshake is now in progress

    // Shutdown must return quickly. It must not wait for the stalled
    // handshake, which can last up to the 10s handshake timeout. A handshake
    // in progress holds a clone of PersistHandle. shutdown() waits for the
    // persistence writer to exit, and that only happens once every clone has
    // been dropped.
    let start = std::time::Instant::now();
    let done = tokio::time::timeout(Duration::from_secs(5), a.shutdown()).await;
    assert!(
        done.is_ok(),
        "shutdown waited for a stalled machine's handshake \
         (still not done after {:?})",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_concurrent_handshake_from_the_same_identity_is_refused() {
    install_crypto();
    let ca = generate_dev_ca();
    let (a, addr_a) = spawn_listener(&ca).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connection A: replica 9 connects and stalls by never opening a stream.
    // The listener allows one handshake at a time per identity, so this one
    // holds identity 9's slot for the full 10s handshake timeout.
    let nine = fleet_transport(&ca, 9);
    let conn_a = within("the first dial", nine.dial(PeerAddr::new(addr_a.clone()), Default::default()))
        .await
        .expect("the staller holds fleet credentials, so TLS admits it");
    tokio::time::sleep(Duration::from_millis(300)).await; // the server claims 9's slot

    // Connection B: the same identity again, while A's handshake is still in
    // progress. The accept loop must refuse the second claim and drop the
    // connection, so opening a stream on B fails within a couple of seconds.
    // That is far less than the 10s A still holds, so A must be the one
    // occupying the slot when B is refused. If the slot were not claimed, the
    // server would start a handshake for B too and wait in accept_stream(),
    // and B would stay usable for the whole time.
    //
    // The streams we open to test B are kept open, never dropped. Dropping
    // one would make that handshake fail with a read error and close B for a
    // different reason. The test would then pass even with the slot claim
    // removed.
    let conn_b = within("the second dial", nine.dial(PeerAddr::new(addr_a.clone()), Default::default()))
        .await
        .expect("TLS admits the duplicate — the refusal under test is at admission, not TLS");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut held = Vec::new();
    let mut refused = false;
    while tokio::time::Instant::now() < deadline {
        match within("opening a stream", conn_b.open_stream()).await {
            Ok(s) => held.push(s), // B is not closed on our side yet, so keep the stream and try again
            Err(_) => {
                refused = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        refused,
        "a second connection from a machine whose first handshake is still \
         going on was let in; the accept loop should claim one handshake slot \
         per machine"
    );

    drop(held);
    drop(conn_a);
    within("shutting a down", a.shutdown()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replica_not_in_the_configured_peer_set_can_still_join() {
    install_crypto();
    // Any machine with a valid certificate may join. The listener's
    // `cfg.peers` does not list replica 9, and replica 9 must join anyway.
    // A cluster with a fixed list of members would refuse this with a
    // membership check. Adding that check here would shut out every new
    // replica that has a right to join. This test fails if anyone adds a
    // membership check, or a check on who dialed whom, to the accept loop.
    let ca = generate_dev_ca();
    let (a, addr_a) = spawn_listener(&ca).await; // cfg.peers is empty
    within("creating the counter", a.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let nine = spawn_dialer(&ca, 9, &addr_a).await;
    within("creating the counter", nine.create("c", TypedCrdt::PnCounter(PnCounter::default()))).await;
    within("changing the counter", nine.mutate("c", increment(ReplicaId(9), 4))).await;
    assert!(
        await_value_within(&a, "c", 4, Duration::from_secs(3)).await,
        "a machine that is not in cfg.peers could not join"
    );

    // The other direction works too: the newcomer sees the listener's writes.
    within("changing the counter", a.mutate("c", increment(ReplicaId(1), 2))).await;
    assert!(
        await_value_within(&nine, "c", 6, Duration::from_secs(3)).await,
        "a machine that is not in cfg.peers never got the listener's change"
    );

    within("shutting a down", a.shutdown()).await;
    within("shutting nine down", nine.shutdown()).await;
}
