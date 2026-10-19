use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{Node, NodeConfig};
use orvu_transport::mem::MemTransport;
use orvu_transport::PeerAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Records the pair (span_name, trace_id) each time a span is created.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<(String, String)>>>);

struct TraceVisitor {
    trace_id: Option<String>,
}
impl Visit for TraceVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "trace_id" {
            self.trace_id = Some(value.to_string());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "trace_id" {
            // A field written as `%value` arrives through record_debug as its
            // Display text, with no quotes.
            self.trace_id = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for Captured {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let mut v = TraceVisitor { trace_id: None };
        attrs.record(&mut v);
        if let Some(tid) = v.trace_id {
            self.0
                .lock()
                .unwrap()
                .push((attrs.metadata().name().to_string(), tid));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn trace_id_propagates_from_mutate_to_remote_apply() {
    orvu_test_support::within("trace id propagates from mutate to remote apply", TEST_LIMIT, async {
        let captured = Captured::default();
        let store = captured.0.clone();
        let _ = tracing_subscriber::registry().with(captured).try_init();

        let fabric = Arc::new(MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("k", |crdt, _h, _r| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
            _ => unreachable!(),
        })
        .await;

        // Wait until B has the same value. That shows the delta (the small change
        // A sent), and the trace_id it carries, reached B.
        for _ in 0..100 {
            if let Some(TypedCrdt::PnCounter(pn)) = b.snapshot("k").await {
                if pn.value() == 5 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // give the apply span time to be recorded

        let rows = store.lock().unwrap().clone();
        let zero = "0".repeat(32);
        let mutate_tid = rows
            .iter()
            .find(|(n, _)| n == "mutate")
            .map(|(_, t)| t.clone())
            .expect("a mutate span was recorded on A");
        let apply_tid = rows
            .iter()
            .find(|(n, _)| n == "apply_remote_delta")
            .map(|(_, t)| t.clone());

        assert_ne!(mutate_tid, zero, "the mutate generated a non-zero trace_id");
        assert_eq!(
            apply_tid.as_deref(),
            Some(mutate_tid.as_str()),
            "B's apply_remote_delta span carries the SAME trace_id A minted"
        );

        a.shutdown().await;
        b.shutdown().await;
    })
    .await
}
