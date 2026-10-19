//! Three nodes on the in-memory transport agree after each of them changes
//! the same counter.

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

async fn await_value(h: &NodeHandle, obj: &str, want: i64) -> bool {
    for _ in 0..150 {
        if counter(h, obj).await == want { return true; }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    counter(h, obj).await == want
}

#[tokio::test(flavor = "multi_thread")]
async fn three_nodes_agree_after_each_changes_the_counter() {
    orvu_test_support::within("three nodes agree after each changes the counter", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());

        let names = ["n1", "n2", "n3"];
        let mut handles: Vec<NodeHandle> = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let mut cfg = NodeConfig::new(ReplicaId(i as u64 + 1), *name);
            cfg.peers = names.iter().filter(|n| *n != name).map(|n| PeerAddr::new(*n)).collect();
            cfg.anti_entropy_interval = Duration::from_millis(25);
            let h = Node::spawn(fabric.endpoint(i as u64 + 1), cfg).await.unwrap();
            h.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
            handles.push(h);
        }

        // Each node in turn adds a different amount. No node waits for the
        // others to catch up before the next one makes its change.
        for (i, h) in handles.iter().enumerate() {
            let amt = (i as u64 + 1) * 10;
            let rid = ReplicaId(i as u64 + 1);
            h.mutate("c", move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(rid, amt)),
                _ => unreachable!(),
            }).await;
        }

        let expected = 10 + 20 + 30;
        for h in &handles {
            assert!(await_value(h, "c", expected).await, "node converged to {expected}");
        }
    })
    .await
}
