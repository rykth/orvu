//! Dropping a `Listener` must free its port and stop its accept pump, the
//! background task that takes in new connections.
//!
//! The listener spawns a pump task, which owns a clone of the QUIC `Endpoint`.
//! The pump exits only when it is cancelled. Without a drop guard, dropping the returned
//! listener would leave the pump and its bound socket alive for as long as the
//! parent cancellation token lives. The port would never be freed, and the
//! leftover pump would keep accepting connections into a channel nobody reads.
//! The test checks this by binding the same port again. That fails with
//! "address already in use" while the pump still holds the socket.

use orvu_transport::{generate_dev_ca, issue_node_cert, DevCa, QuicTransport, Transport};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn fleet(ca: &DevCa, replica: u64) -> Arc<QuicTransport> {
    let c = issue_node_cert(ca, replica);
    Arc::new(QuicTransport::new(&c.cert_pem, &c.key_pem, &ca.cert_pem).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_quic_listener_frees_its_udp_port() {
    orvu_test_support::within("dropping a quic listener frees its udp port", TEST_LIMIT, async {
        install_crypto();
        let ca = generate_dev_ca();
        let server = fleet(&ca, 2);
        let listener = server
            .listen("127.0.0.1:0", CancellationToken::new())
            .await
            .unwrap();
        let addr = listener.local_addr();
        drop(listener);
        // The pump sees the cancellation and drops its Endpoint clone a moment
        // later, longer on a busy machine, so the port is tried again until
        // it is free or 5s have passed. Without the drop guard it is never
        // freed.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut rebound = std::net::UdpSocket::bind(&addr);
        while rebound.is_err() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
            rebound = std::net::UdpSocket::bind(&addr);
        }
        assert!(
            rebound.is_ok(),
            "dropping the QUIC listener must free its UDP port; rebind got {:?}",
            rebound.err()
        );
    })
    .await
}
