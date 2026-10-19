//! End-to-end tests over real QUIC on localhost: two nodes end up with the
//! same value, and a node whose certificate does not match its claimed replica
//! id is refused.

use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::{generate_dev_ca, issue_node_cert, PeerAddr, QuicTransport};
use std::sync::Arc;
use std::time::Duration;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A free port on 127.0.0.1, as "ip:port".
fn free_addr() -> String {
    format!("127.0.0.1:{}", orvu_test_support::free_port())
}

async fn counter(h: &NodeHandle, obj: &str) -> i64 {
    match h.snapshot(obj).await {
        Some(TypedCrdt::PnCounter(pn)) => pn.value(),
        _ => -1,
    }
}

async fn await_value(h: &NodeHandle, obj: &str, want: i64) -> bool {
    for _ in 0..150 {
        if counter(h, obj).await == want { return true; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    counter(h, obj).await == want
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_converge_over_real_quic() {
    orvu_test_support::within("two nodes converge over real quic", TEST_LIMIT, async {
        install_crypto();
        // Create a development certificate authority and one certificate per node.
        // The replica id in each certificate's SAN (subject alternative name)
        // matches the node's NodeConfig replica. Both nodes trust the same authority.
        let ca = generate_dev_ca();
        let c1 = issue_node_cert(&ca, 1);
        let c2 = issue_node_cert(&ca, 2);
        let transport_a = Arc::new(QuicTransport::new(&c1.cert_pem, &c1.key_pem, &ca.cert_pem).unwrap());
        let transport_b = Arc::new(QuicTransport::new(&c2.cert_pem, &c2.key_pem, &ca.cert_pem).unwrap());

        // Pick both ports first, so each node knows the other's address before it
        // starts. A fixed number would not do: it can be in use by another
        // program, or handed out by the system to any program's outgoing
        // connection.
        let addr_a = &free_addr();
        let addr_b = &free_addr();

        let mut cfg_a = NodeConfig::new(ReplicaId(1), addr_a);
        cfg_a.peers = vec![PeerAddr::new(addr_b)];
        let mut cfg_b = NodeConfig::new(ReplicaId(2), addr_b);
        cfg_b.peers = vec![PeerAddr::new(addr_a)];

        let a = Node::spawn(transport_a, cfg_a).await.unwrap();
        let b = Node::spawn(transport_b, cfg_b).await.unwrap();

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;

        a.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
            _ => unreachable!(),
        }).await;
        b.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(2), 3)),
            _ => unreachable!(),
        }).await;

        assert!(await_value(&a, "c", 8).await, "A converged to 8");
        assert!(await_value(&b, "c", 8).await, "B converged to 8");
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn quic_peer_with_mismatched_cert_identity_is_rejected() {
    orvu_test_support::within("quic peer with mismatched cert identity is rejected", TEST_LIMIT, async {
        install_crypto();
        // This node's certificate says replica 2, but its NodeConfig claims
        // replica 3 in the Hello message. The honest node must reject the
        // connection during the handshake.
        let ca = generate_dev_ca();
        let c_honest = issue_node_cert(&ca, 1);
        let c_mismatched = issue_node_cert(&ca, 2); // SAN replica 2 ...
        let transport_a = Arc::new(QuicTransport::new(&c_honest.cert_pem, &c_honest.key_pem, &ca.cert_pem).unwrap());
        let transport_b = Arc::new(QuicTransport::new(&c_mismatched.cert_pem, &c_mismatched.key_pem, &ca.cert_pem).unwrap());

        let addr_a = &free_addr();
        let addr_b = &free_addr();

        let mut cfg_a = NodeConfig::new(ReplicaId(1), addr_a);
        cfg_a.peers = vec![PeerAddr::new(addr_b)];
        let mut cfg_b = NodeConfig::new(ReplicaId(3), addr_b); // ... but Hello claims replica 3
        cfg_b.peers = vec![PeerAddr::new(addr_a)];

        let a = Node::spawn(transport_a, cfg_a).await.unwrap();
        let b = Node::spawn(transport_b, cfg_b).await.unwrap();

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
            _ => unreachable!(),
        }).await;

        // B must never see A's increment, because the handshake rejected the
        // certificate that did not match.
        assert!(!await_value(&b, "c", 5).await, "mismatched cert identity must not sync");
    })
    .await
}
