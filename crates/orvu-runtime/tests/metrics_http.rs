use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{Node, NodeConfig};
use orvu_transport::mem::MemTransport;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Sends a hand-written HTTP/1.1 GET to `addr` and returns the full response
/// text. This needs no HTTP client library. The server closes the connection
/// after one response, so read_to_end reaches the end of the stream.
async fn http_get(addr: &str, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_endpoint_serves_prometheus_text_and_404s_unknown_paths() {
    orvu_test_support::within("metrics endpoint serves prometheus text and 404s unknown paths", TEST_LIMIT, async {
        let fabric = Arc::new(MemTransport::new());
        let mut cfg = NodeConfig::new(ReplicaId(1), "m");
        let addr = format!("127.0.0.1:{}", orvu_test_support::free_port());
        cfg.metrics_addr = Some(addr.clone());
        let n = Node::spawn(fabric.endpoint(1), cfg).await.unwrap();

        // Change a counter so that one metric is not zero.
        n.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        n.mutate("k", |crdt, _h, _r| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 3)),
            _ => unreachable!(),
        })
        .await;

        let resp = http_get(&addr, "/metrics").await;
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "status line, got:\n{resp}");
        assert!(resp.contains("text/plain; version=0.0.4"), "prometheus content-type");
        assert!(resp.contains("orvu_deltas_applied_local"), "body carries orvu_ metrics:\n{resp}");

        let notfound = http_get(&addr, "/nope").await;
        assert!(notfound.starts_with("HTTP/1.1 404 Not Found\r\n"), "got:\n{notfound}");

        n.shutdown().await;
    })
    .await
}
