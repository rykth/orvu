//! Checks that the per-source admission limit (`admission.claim`) is wired into
//! the listener's accept loop. The limit caps how many handshakes one source
//! address may have in progress at once. A handshake is the exchange that sets
//! up and authenticates a new connection.
//!
//! `HandshakeAdmission` has unit tests of its own, but they cannot show that a
//! real listener calls `claim`. This test fails if that call is removed from
//! the accept loop.
//!
//! What it checks: the limit holds back the attacker only. Handshakes beyond
//! the limit from one address are turned away, while an honest peer from
//! another address is unaffected.

use orvu_transport::{generate_dev_ca, issue_node_cert, DevCa, PeerAddr, QuicTransport, Transport};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
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

// ---------------------------------------------------------------------------
// QUIC checks the source address before the admission limit. A source that has not proved its address could be forged, so the
// server answers it with a stateless Retry packet and it never reaches
// `admission.claim`. A flood of sockets that never answer is turned away at
// that earlier check, so it cannot be used to test the limit. The only way to
// fill the per-source limit is with handshakes whose address has been
// validated. Each filler answers the Retry, which proves its address and
// claims a permit. It then stalls and holds the permit for HANDSHAKE_TIMEOUT.
// A filler is built from a simple UDP proxy that forwards exactly three
// packets and then goes quiet: the bare Initial (the first packet of a QUIC
// handshake), the Retry, and the second Initial that carries the Retry token.
// ---------------------------------------------------------------------------

/// Starts one QUIC handshake from source `src_ip` to `server_addr` that
/// validates its address and then stalls. The proxy forwards the two Initials
/// from the client and the one Retry from the server, then drops everything,
/// so the handshake stalls while holding a permit. Returns the proxy task and
/// the dial task. The caller keeps both alive.
async fn spawn_validated_stall(
    server_addr: SocketAddr,
    src_ip: &str,
    client: Arc<QuicTransport>,
) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let facing_client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = facing_client.local_addr().unwrap().to_string();
    let facing_server = UdpSocket::bind(format!("{src_ip}:0")).await.unwrap();
    let proxy = tokio::spawn(async move {
        let mut client_addr = None;
        let mut c2s = 0usize;
        let mut s2c = 0usize;
        let mut from_client = vec![0u8; 2048];
        let mut from_server = vec![0u8; 2048];
        loop {
            tokio::select! {
                r = facing_client.recv_from(&mut from_client) => {
                    let Ok((n, src)) = r else { break };
                    client_addr = Some(src);
                    if c2s < 2 {
                        c2s += 1;
                        let _ = facing_server.send_to(&from_client[..n], server_addr).await;
                    }
                }
                r = facing_server.recv_from(&mut from_server) => {
                    let Ok((n, _)) = r else { break };
                    if s2c < 1 {
                        if let Some(dst) = client_addr {
                            s2c += 1;
                            let _ = facing_client.send_to(&from_server[..n], dst).await;
                        }
                    }
                }
            }
        }
    });
    let dial = tokio::spawn(async move {
        let _ = client
            .dial(PeerAddr::new(proxy_addr), CancellationToken::new())
            .await;
    });
    (proxy, dial)
}

/// A UDP proxy from source `src_ip` that relays everything in both directions.
/// A real quinn client dialing through it completes the handshake when it is
/// admitted. When the per-source limit is full it receives the server's
/// `refuse()` (a CONNECTION_CLOSE) instead, so being turned away shows up as a
/// prompt `dial()` error.
async fn full_forward_proxy(server_addr: SocketAddr, src_ip: &str) -> (String, tokio::task::JoinHandle<()>) {
    let facing_client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = facing_client.local_addr().unwrap().to_string();
    let facing_server = UdpSocket::bind(format!("{src_ip}:0")).await.unwrap();
    let h = tokio::spawn(async move {
        let mut client_addr = None;
        let mut from_client = vec![0u8; 2048];
        let mut from_server = vec![0u8; 2048];
        loop {
            tokio::select! {
                r = facing_client.recv_from(&mut from_client) => {
                    let Ok((n, src)) = r else { break };
                    client_addr = Some(src);
                    let _ = facing_server.send_to(&from_client[..n], server_addr).await;
                }
                r = facing_server.recv_from(&mut from_server) => {
                    let Ok((n, _)) = r else { break };
                    if let Some(dst) = client_addr {
                        let _ = facing_client.send_to(&from_server[..n], dst).await;
                    }
                }
            }
        }
    });
    (proxy_addr, h)
}

/// `PER_IP_HANDSHAKES` (8) handshakes from one source (127.0.0.4) that validate
/// their address and then stall use up that source's limit. A 9th validated
/// connection from 127.0.0.4 must be refused with `Incoming::refuse()`, which
/// shows up as a prompt `dial()` error. An honest peer from 127.0.0.1 must
/// still connect.
///
/// The test tells the two cases apart by the 9th dial. With the limit in place
/// it is full and the server refuses the connection. With the `claim` call
/// removed, the 9th is admitted and completes the handshake, so the dial
/// returns Ok. A flood of sockets that never answer cannot be used here,
/// because the Retry check turns it away before it reaches admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quic_per_ip_cap_refuses_the_over_cap_validated_source() {
    orvu_test_support::within("quic per ip cap refuses the over cap validated source", TEST_LIMIT, async {
        install_crypto();
        let ca = generate_dev_ca();
        let server = fleet(&ca, 2);
        let staller = fleet(&ca, 4);
        let ninth = fleet(&ca, 5);
        let honest = fleet(&ca, 3);

        let listener = server
            .listen("127.0.0.1:0", CancellationToken::new())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server_addr: SocketAddr = addr.parse().unwrap();

        // Keep taking accepted connections and reported errors so nothing backs up
        // behind the accept loop.
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok(c) = listener.accept().await {
                    held.push(c);
                }
            }
        });

        // Use up 127.0.0.4's limit with 8 validated stalls, held for the whole test.
        const CAP: usize = 8;
        let mut held = Vec::new();
        for _ in 0..CAP {
            held.push(spawn_validated_stall(server_addr, "127.0.0.4", staller.clone()).await);
        }
        // Wait so every stall has answered its Retry and claimed its permit.
        tokio::time::sleep(Duration::from_secs(2)).await;

        // A 9th validated connection from 127.0.0.4, through a proxy that forwards
        // everything. The limit is full, so the server must refuse it and the dial
        // must fail promptly.
        let (ninth_proxy_addr, ninth_proxy) = full_forward_proxy(server_addr, "127.0.0.4").await;
        let start = Instant::now();
        let ninth_dial = tokio::time::timeout(
            Duration::from_secs(6),
            ninth.dial(PeerAddr::new(ninth_proxy_addr), CancellationToken::new()),
        )
        .await;
        let ninth_took = start.elapsed();
        // `Box<dyn Conn>` has no Debug, so reduce the result to a bool before asserting.
        let ninth_refused = matches!(ninth_dial, Ok(Err(_)));
        let ninth_timed_out = ninth_dial.is_err();
        println!("9th validated source from a capped IP resolved in {ninth_took:?}: refused={ninth_refused} timed_out={ninth_timed_out}");
        assert!(
            ninth_refused,
            "the 9th VALIDATED connection from a capped source must be REFUSED (dial errors) \
             promptly; with the gate deleted it is admitted and the dial succeeds \
             (refused={ninth_refused}, timed_out={ninth_timed_out})"
        );

        // An honest peer from a different source (127.0.0.1) is unaffected.
        let honest_ok = tokio::time::timeout(
            Duration::from_secs(6),
            honest.dial(PeerAddr::new(addr), CancellationToken::new()),
        )
        .await;
        let honest_connected = matches!(honest_ok, Ok(Ok(_)));
        assert!(
            honest_connected,
            "an honest peer from another source must still connect (connected={honest_connected})"
        );

        ninth_proxy.abort();
        for (p, d) in held {
            p.abort();
            d.abort();
        }
    })
    .await
}
