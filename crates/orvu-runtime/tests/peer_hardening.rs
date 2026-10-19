//! What a connected machine is and is not allowed to make a node do.
//!
//! Several tests here talk to a node through a hand-made connection, so they
//! can send messages an honest node never would.

use orvu_core::{Codec, ProtobufCodec, SyncMessage, TypedCrdt, TypedDelta};
use orvu_crdt::{BoundedCounter, LwwRegister, PnCounter, ReplicaId};
use orvu_runtime::config::{Acl, Action};
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::frame::write_frame;
use orvu_transport::{Conn, MemTransport, PeerAddr, StreamRead, StreamWrite};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Starts node 1, listening on "a", with a counter that cannot go below zero
/// called "stock". Node 1 has put in 10, so it has an allowance of 10.
async fn node_with_stock(network: &Arc<MemTransport>, acl: Acl) -> NodeHandle {
    let mut settings = NodeConfig::new(ReplicaId(1), "a");
    settings.acl = acl;
    let node = Node::spawn(network.endpoint(1), settings).await.unwrap();
    node.create("stock", TypedCrdt::BoundedCounter(BoundedCounter::default())).await;
    node.mutate("stock", |data, _time, me| match data {
        TypedCrdt::BoundedCounter(stock) => TypedDelta::BoundedCounter(stock.increment(10, me)),
        _ => unreachable!(),
    })
    .await
    .unwrap();
    node
}

/// Connects to the node at "a" as machine `me` and does the greeting by hand.
/// Returns the connection and its stream.
async fn connect_as(
    network: &Arc<MemTransport>,
    me: u64,
) -> (Box<dyn Conn>, StreamWrite, StreamRead) {
    let conn = network
        .endpoint(me)
        .dial(PeerAddr::new("a"), CancellationToken::new())
        .await
        .unwrap();
    let (write, read, _) =
        orvu_runtime::peer::handshake(conn.as_ref(), ReplicaId(me), "default", true, &Acl::open())
            .await
            .expect("the greeting succeeds");
    (conn, write, read)
}

async fn send(write: &mut StreamWrite, message: SyncMessage) {
    write_frame(write, &ProtobufCodec.encode(&message)).await.unwrap();
}

async fn allowance(node: &NodeHandle, of: u64) -> u64 {
    match node.snapshot("stock").await {
        Some(TypedCrdt::BoundedCounter(stock)) => stock.allowance(ReplicaId(of)),
        _ => panic!("stock is missing"),
    }
}

fn borrow(amount: u64, from: u64) -> SyncMessage {
    SyncMessage::BorrowRequest {
        namespace: "default".into(),
        object_id: "stock".into(),
        amount,
        from: ReplicaId(from),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_honest_borrow_request_is_granted() {
    orvu_test_support::within("an honest borrow request is granted", TEST_LIMIT, async {
        let network = Arc::new(MemTransport::new());
        let node = node_with_stock(&network, Acl::open()).await;
        let (_conn, mut write, _read) = connect_as(&network, 3).await;

        send(&mut write, borrow(4, 3)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(allowance(&node, 1).await, 6);
        assert_eq!(allowance(&node, 3).await, 4);
        node.shutdown().await;
    })
    .await
}

// Machine 3 asks for allowance to be handed to machine 999. A node only lends
// to the machine that is asking.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrow_request_on_behalf_of_someone_else_is_ignored() {
    orvu_test_support::within("a borrow request on behalf of someone else is ignored", TEST_LIMIT, async {
        let network = Arc::new(MemTransport::new());
        let node = node_with_stock(&network, Acl::open()).await;
        let (_conn, mut write, _read) = connect_as(&network, 3).await;

        send(&mut write, borrow(10, 999)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(allowance(&node, 1).await, 10, "the node gave its allowance away");
        assert_eq!(allowance(&node, 999).await, 0);
        node.shutdown().await;
    })
    .await
}

// Machine 3 may sync but may not write. Lending it allowance would be a write
// made for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_that_may_not_write_cannot_borrow() {
    orvu_test_support::within("a machine that may not write cannot borrow", TEST_LIMIT, async {
        let network = Arc::new(MemTransport::new());
        let acl = Acl::allow_list(BTreeMap::from([(ReplicaId(3), BTreeSet::from([Action::Sync]))]));
        let node = node_with_stock(&network, acl).await;
        let (_conn, mut write, _read) = connect_as(&network, 3).await;

        send(&mut write, borrow(4, 3)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(allowance(&node, 1).await, 10);
        assert_eq!(allowance(&node, 3).await, 0);
        node.shutdown().await;
    })
    .await
}

// Two nodes that list each other but are set up for different namespaces must
// not connect, and must not merge each other's data.
#[tokio::test(flavor = "multi_thread")]
async fn nodes_in_different_namespaces_do_not_sync() {
    orvu_test_support::within("nodes in different namespaces do not sync", TEST_LIMIT, async {
        let network = Arc::new(MemTransport::new());

        let mut prod = NodeConfig::new(ReplicaId(1), "prod-node");
        prod.namespace = "prod".into();
        prod.peers = vec![PeerAddr::new("staging-node")];
        let mut staging = NodeConfig::new(ReplicaId(2), "staging-node");
        staging.namespace = "staging".into();
        staging.peers = vec![PeerAddr::new("prod-node")];

        let prod = Node::spawn(network.endpoint(1), prod).await.unwrap();
        let staging = Node::spawn(network.endpoint(2), staging).await.unwrap();

        staging.create("visits", TypedCrdt::PnCounter(PnCounter::default())).await;
        staging
            .mutate("visits", |data, _time, me| match data {
                TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, 5)),
                _ => unreachable!(),
            })
            .await;
        tokio::time::sleep(Duration::from_millis(800)).await;

        assert!(prod.snapshot("visits").await.is_none(), "staging data reached prod");
        assert_eq!(prod.known_members().await, vec![ReplicaId(1)], "prod took staging as a member");
        assert_eq!(staging.known_members().await, vec![ReplicaId(2)], "staging took prod as a member");
        prod.shutdown().await;
        staging.shutdown().await;
    })
    .await
}

// "Later write wins" compares timestamps, so a change has to carry the real
// time of day and not a count of operations.
#[tokio::test(flavor = "multi_thread")]
async fn a_change_is_stamped_with_the_time_of_day() {
    orvu_test_support::within("a change is stamped with the time of day", TEST_LIMIT, async {
        let network = Arc::new(MemTransport::new());
        let node = Node::spawn(network.endpoint(1), NodeConfig::new(ReplicaId(1), "a")).await.unwrap();
        node.create("title", TypedCrdt::LwwRegister(LwwRegister::default())).await;

        let before = now_ms();
        let delta = node
            .mutate("title", |data, time, me| match data {
                TypedCrdt::LwwRegister(title) => TypedDelta::LwwRegister(title.set("Plan".into(), time, me)),
                _ => unreachable!(),
            })
            .await
            .unwrap();
        let after = now_ms();

        assert!(
            (before..=after).contains(&delta.hlc.wall),
            "stamped {} but the time was between {before} and {after}",
            delta.hlc.wall
        );
        node.shutdown().await;
    })
    .await
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// A connected machine stops reading but keeps asking to be caught up. The node
// has more to send than fits, so its writer and then its reader end up waiting
// on that machine. Shutting the node down must still finish.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_finishes_when_a_connected_machine_stops_reading() {
    orvu_test_support::within("shutdown finishes when a connected machine stops reading", TEST_LIMIT, async {
        let network = Arc::new(MemTransport::new());
        let node = Node::spawn(network.endpoint(1), NodeConfig::new(ReplicaId(1), "a")).await.unwrap();
        node.create("title", TypedCrdt::LwwRegister(LwwRegister::default())).await;

        let (_conn, mut write, _read) = connect_as(&network, 3).await;

        // One change far larger than the stream's buffer. The node's writer cannot
        // finish sending it, because nobody reads.
        let large = "x".repeat(300 * 1024);
        node.mutate("title", move |data, time, me| match data {
            TypedCrdt::LwwRegister(title) => TypedDelta::LwwRegister(title.set(large, time, me)),
            _ => unreachable!(),
        })
        .await
        .unwrap();

        // Each of these asks for everything. The answers queue up behind the
        // writer until the queue is full.
        for _ in 0..100 {
            send(&mut write, SyncMessage::ContextDigest { namespace: "default".into(), entries: BTreeMap::new() })
                .await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        tokio::time::timeout(Duration::from_secs(5), node.shutdown())
            .await
            .expect("shutdown hung behind a machine that stopped reading");
    })
    .await
}

// A machine sends a change no honest machine could send: its number skips
// ahead. The node refuses it and says so with a Nack that names the object
// and the dot, so the sender can tell which change it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_change_is_named_in_a_nack() {
    orvu_test_support::within("a refused change is named in a nack", TEST_LIMIT, async {
        use orvu_core::{NackReason, WireDelta};
        use orvu_crdt::{Dot, DotContext, Hlc};
        let network = Arc::new(MemTransport::new());
        let node = Node::spawn(network.endpoint(1), NodeConfig::new(ReplicaId(1), "a")).await.unwrap();
        let (_conn, mut write, mut read) = connect_as(&network, 3).await;

        let mut counter = PnCounter::default();
        let skips = WireDelta {
            object_id: "visits".into(),
            dot: Dot::new(ReplicaId(3), 5),
            hlc: Hlc { wall: now_ms(), logical: 0 },
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(counter.increment(ReplicaId(3), 1)),
            trace_id: orvu_core::TraceId::none(),
        };
        send(&mut write, SyncMessage::DeltaBatch { namespace: "default".into(), deltas: vec![skips] }).await;

        let nack = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let bytes = orvu_transport::frame::read_frame(&mut read).await.unwrap();
                if let SyncMessage::Nack { reason, object_id, dot, .. } = ProtobufCodec.decode(&bytes).unwrap() {
                    return (reason, object_id, dot);
                }
            }
        })
        .await
        .expect("no Nack came back");
        assert_eq!(nack.0, NackReason::Known(orvu_proto::v1::NackReason::MalformedPayload));
        assert_eq!(nack.1, "visits");
        assert_eq!(nack.2, Some(Dot::new(ReplicaId(3), 5)));
        // The Nack is counted just after it is handed to the writer, so the
        // count can come a moment after the Nack itself has arrived.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while node.metrics().nacks_sent == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(node.metrics().nacks_sent, 1);
        node.shutdown().await;
    })
    .await
}
