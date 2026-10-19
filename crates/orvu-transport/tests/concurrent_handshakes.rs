//! Two things are checked here. One party's stalled or
//! failed handshake must not delay another party's. A handshake is the exchange
//! that sets up a new connection, and it runs before the listener knows who
//! the other side is. And a QUIC source that has not proved its address, and
//! so could be forged, must not be able to use up the admission budget (the
//! limit on handshakes in progress).
//!
//! On QUIC the handshake is `incoming.await`, which covers QUIC and TLS. If
//! the listener ran it inline, one connection at a time, a single connection
//! whose handshake never completes would hold the listener until quinn's idle
//! timeout (about 30s), and every honest peer would wait behind it. A normal
//! accept takes about 25ms.
//!
//! Two kinds of attacker appear below. A "mute" attacker is modelled by
//! replaying a real QUIC Initial (the first packet of a handshake) from a
//! plain UDP socket that never answers. The server allocates an `Incoming` for
//! it and that handshake can never complete. Each distinct real Initial has
//! its own connection id and becomes its own connection on the server. quinn
//! treats the same Initial replayed from many sources as one connection, so
//! the spoofed-flood test below captures a fresh Initial per source. The
//! server answers a source that has not proved its address with a Retry
//! packet, which the client must answer to show it can receive at that
//! address. Mute attackers never do, so the Retry check turns them all away
//! before admission. The second kind is a validated attacker that answers the
//! Retry and then stalls the TLS handshake. It gets past the Retry check, and
//! it is the reason each handshake runs in its own spawned task. The last
//! test builds it from a real quinn client cut off by a simple UDP proxy.

use orvu_transport::{generate_dev_ca, issue_node_cert, DevCa, PeerAddr, QuicTransport, Transport};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A QUIC transport holding a certificate for `replica`, issued by `ca`.
fn fleet(ca: &DevCa, replica: u64) -> Arc<QuicTransport> {
    let c = issue_node_cert(ca, replica);
    Arc::new(QuicTransport::new(&c.cert_pem, &c.key_pem, &ca.cert_pem).unwrap())
}

/// Captures one real QUIC Initial packet. It points a real quinn client at a
/// UDP socket that never answers and takes the first packet the client sends.
/// That packet is a valid Initial, so replaying it makes a real server
/// allocate an `Incoming` whose handshake can never complete. Each capture
/// uses a fresh client connection, so each captured Initial carries a
/// different connection id.
async fn capture_initial(client: &Arc<QuicTransport>) -> Vec<u8> {
    let cap = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let cap_addr = cap.local_addr().unwrap().to_string();
    let c = client.clone();
    // This dial never completes, because the capture socket never answers.
    // Abort it once we have the first packet. That drops the client endpoint
    // and frees its socket.
    let dial = tokio::spawn(async move {
        let _ = c
            .dial(PeerAddr::new(cap_addr), CancellationToken::new())
            .await;
    });
    let mut buf = vec![0u8; 2048];
    let (n, _src) = cap.recv_from(&mut buf).await.unwrap();
    dial.abort();
    buf.truncate(n);
    buf
}

/// Accepts from a listener on a separate task. It sends on the channel for
/// each accepted `Conn` and holds the `Conn` so it stays open during the test.
fn spawn_accept_loop(listener: Box<dyn orvu_transport::Listener>) -> mpsc::UnboundedReceiver<()> {
    let (tx, rx) = mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(_conn) => {
                    let _ = tx.send(());
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                Err(_) => continue,
            }
        }
    });
    rx
}

/// Several mute QUIC Initials from one source (127.0.0.3) must not delay an
/// honest peer from another source. The honest client uses its normal
/// loopback source, 127.0.0.1.
///
/// This does not test that handshakes run side by side. Every mute Initial
/// here arrives from a source address that has not been validated, so the
/// Retry check turns the whole flood away before it reaches `admission.claim`
/// or the per-handshake `tokio::spawn`. The test passes unchanged with the
/// spawn inlined. What it checks is the Retry check against a flood from one
/// source. Among the QUIC tests in this file, only
/// `a_validated_client_that_stalls_the_tls_handshake_does_not_block_other_sources`
/// below reaches the spawn.
///
/// The attacker and the honest client must have different source addresses.
/// If both used 127.0.0.1, the per-source limit would apply to the honest peer
/// too. The flood would then be turned away instead of held, and the test
/// would pass for the wrong reason. The replay sockets are bound to 127.0.0.3.
/// The honest client dials 127.0.0.1 over loopback, so the kernel gives it
/// the source 127.0.0.1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_unvalidated_quic_initials_from_one_source_is_retried_and_does_not_delay_an_honest_peer() {
    orvu_test_support::within("a flood of unvalidated quic initials from one source is retried and does not delay an honest peer", TEST_LIMIT, async {
        install_crypto();
        let ca = generate_dev_ca();
        let server = fleet(&ca, 2);
        let attacker = fleet(&ca, 4);
        let honest = fleet(&ca, 3);

        let listener = server
            .listen("127.0.0.1:0", CancellationToken::new())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server_addr: SocketAddr = addr.parse().unwrap();

        // A mute attacker never produces a Conn, so the only Ok from the server is
        // the honest peer's.
        let mut rx = spawn_accept_loop(listener);

        // Several different mute Initials from one source (127.0.0.3). Hold the
        // sockets so their source ports stay open for the whole test.
        const FLOOD: usize = 8;
        let mut socks = Vec::new();
        for _ in 0..FLOOD {
            let initial = capture_initial(&attacker).await;
            let s = UdpSocket::bind("127.0.0.3:0").await.unwrap();
            s.send_to(&initial, server_addr).await.unwrap();
            socks.push(s);
        }
        // Wait so the flood arrives and the listener takes it before the honest dial.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let start = Instant::now();
        let honest_addr = addr.clone();
        let honest_dial = tokio::spawn(async move {
            honest
                .dial(PeerAddr::new(honest_addr), CancellationToken::new())
                .await
        });

        let accepted = tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .is_ok();
        let elapsed = start.elapsed();
        println!("honest peer accepted behind the flood in {elapsed:?} (accepted={accepted})");

        assert!(
            accepted,
            "the honest machine was not let in after {elapsed:?}, behind a flood of {FLOOD} silent first packets"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "a flood from another address held the honest machine up for {elapsed:?}; \
             handshakes run one at a time would take the idle timeout"
        );

        drop(socks);
        let _ = honest_dial.await;
    })
    .await
}

/// A failed QUIC handshake must still come out of `accept()` as an `Err`.
///
/// A caller that loops on `accept()` has to carry on after an `Err` instead of
/// stopping. This is the only test that shows a failed handshake reaches the
/// caller as an `Err`. If the listener hid these failures, a caller's own
/// test that a rejected handshake does not stop its listener would pass even
/// if the caller stopped on the first `Err`, because it would never see one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_quic_handshake_still_surfaces_as_err() {
    orvu_test_support::within("a rejected quic handshake still surfaces as err", TEST_LIMIT, async {
        install_crypto();
        let ca = generate_dev_ca();
        let rogue_ca = generate_dev_ca();

        let server = fleet(&ca, 2);
        // A rogue client whose certificate is signed by a different CA, but which
        // trusts the real CA. It accepts the server while the server rejects it.
        // The failure is on the server's side of the handshake, which is the side
        // `accept()` reports.
        let rogue_cert = issue_node_cert(&rogue_ca, 1);
        let rogue =
            Arc::new(QuicTransport::new(&rogue_cert.cert_pem, &rogue_cert.key_pem, &ca.cert_pem).unwrap());
        let honest = fleet(&ca, 3);

        let listener = server
            .listen("127.0.0.1:0", CancellationToken::new())
            .await
            .unwrap();
        let addr = listener.local_addr();

        // The rogue dials; its handshake fails on the server side.
        let rogue_addr = addr.clone();
        tokio::spawn(async move {
            let _ = rogue
                .dial(PeerAddr::new(rogue_addr), CancellationToken::new())
                .await;
        });

        let first = tokio::time::timeout(Duration::from_secs(8), listener.accept())
            .await
            .expect("accept() must yield the rogue's failed handshake, not hang");
        assert!(
            first.is_err(),
            "a rejected QUIC handshake must surface as Err from accept(), not be swallowed"
        );

        // The listener must keep serving: an honest peer connects afterwards.
        let honest_addr = addr.clone();
        let honest_dial = tokio::spawn(async move {
            honest
                .dial(PeerAddr::new(honest_addr), CancellationToken::new())
                .await
        });
        let second = tokio::time::timeout(Duration::from_secs(8), listener.accept())
            .await
            .expect("accept() must yield the honest conn");
        assert!(
            second.is_ok(),
            "the listener must keep accepting honest peers after a rejected handshake"
        );
        let _ = honest_dial.await;
    })
    .await
}

/// QUIC sources that have not been validated get a Retry and are not counted.
/// So a flood from forged addresses cannot fill the global limit and push out
/// an honest peer.
///
/// A QUIC Initial arrives with a source address nobody has checked. An
/// attacker who forges a new address for every packet would otherwise count as
/// a new source each time and fill the global limit with forged entries, and
/// the per-source limit would protect nobody. The listener answers every
/// unvalidated Initial with a stateless `retry()` before it claims a permit.
/// Only a source that has proved it can receive at its claimed address uses
/// any budget.
///
/// Here 320 different mute Initials are replayed across 40 source addresses
/// standing in for forged ones (127.0.1.1 to 127.0.1.40, 8 per address). That
/// is exactly the per-source limit for each address, so none is turned away
/// by it, and well past the global limit of 256. Without the Retry step each
/// one would claim a permit and the honest peer (127.0.0.1, a different
/// source) would be turned away by the global limit. With it, every forged
/// Initial gets a Retry and is dropped, and the honest peer is admitted. All
/// sources here are different loopback addresses, the honest one included.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unvalidated_sources_are_retried_not_admitted_so_a_spoofed_flood_cannot_shed_an_honest_peer() {
    orvu_test_support::within("unvalidated sources are retried not admitted so a spoofed flood cannot shed an honest peer", TEST_LIMIT, async {
        install_crypto();
        let ca = generate_dev_ca();
        let server = fleet(&ca, 2);
        let attacker = fleet(&ca, 4);
        let honest = fleet(&ca, 3);

        let listener = server
            .listen("127.0.0.1:0", CancellationToken::new())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server_addr: SocketAddr = addr.parse().unwrap();

        let mut rx = spawn_accept_loop(listener);

        // Try to fill the global limit from forged sources. 40 addresses × 8 ports
        // gives 320 different mute Initials, each its own connection.
        let mut socks = Vec::new();
        for ip_last in 1u8..=40 {
            for _ in 0..8 {
                let initial = capture_initial(&attacker).await;
                let s = UdpSocket::bind(format!("127.0.1.{ip_last}:0"))
                    .await
                    .unwrap();
                s.send_to(&initial, server_addr).await.unwrap();
                socks.push(s);
            }
        }
        // Wait so every forged Initial is processed before the honest dial.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let start = Instant::now();
        let honest_addr = addr.clone();
        let honest_dial = tokio::spawn(async move {
            honest
                .dial(PeerAddr::new(honest_addr), CancellationToken::new())
                .await
        });
        let admitted = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .is_ok();
        let elapsed = start.elapsed();
        println!("honest peer admitted behind a 320-spoofed-Initial flood in {elapsed:?} (admitted={admitted})");

        assert!(
            admitted,
            "a flood from addresses that were never checked used up every handshake \
             slot and the honest machine was turned away; the retry() check should \
             come before a slot is given"
        );

        drop(socks);
        let _ = honest_dial.await;
    })
    .await
}

/// This is the only QUIC test that reaches the per-handshake spawn. The
/// attacker here is a client that properly answers the server's Retry and then
/// stalls the rest of the TLS handshake. Answering the Retry proves it owns
/// its address, so `remote_address_validated()` is true and a permit is
/// claimed. With the spawn inlined, that one stalled handshake blocks the
/// whole accept loop for up to HANDSHAKE_TIMEOUT (10s). That includes Retry
/// processing and admission for every other source. It is the same problem as
/// accepting connections one at a time, one check later.
///
/// A plain socket cannot answer a Retry, because the token has to be sent back
/// inside a new, correctly encrypted Initial. A real quinn client would
/// complete the whole handshake. So the stall is built from a real quinn
/// client cut off by a simple UDP proxy. The proxy forwards exactly two
/// packets from client to server: the bare Initial, then the Initial with the
/// token that the client sends in answer to the Retry. It forwards exactly one
/// packet from server to client, the Retry itself. Then it drops everything.
/// The server validates the source, claims a permit, sends its handshake data
/// to nobody, and stalls.
///
/// The last assertion checks that the stalled handshake does come out of
/// `accept()` as an `Err` in the end. That shows the stall really happened.
/// If the proxy forwarded the wrong number of packets and the Initial with the
/// token never arrived, nothing would be admitted and no `Err` would ever
/// appear. The test would then fail instead of passing without reaching the
/// spawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_validated_client_that_stalls_the_tls_handshake_does_not_block_other_sources() {
    orvu_test_support::within("a validated client that stalls the tls handshake does not block other sources", TEST_LIMIT, async {
        install_crypto();
        let ca = generate_dev_ca();
        let server = fleet(&ca, 2);
        let staller = fleet(&ca, 4);
        let honest = fleet(&ca, 3);

        let listener = server
            .listen("127.0.0.1:0", CancellationToken::new())
            .await
            .unwrap();
        let addr = listener.local_addr();
        let server_addr: SocketAddr = addr.parse().unwrap();

        // An accept loop that reports both outcomes. It sends `true` for an
        // accepted connection, which it keeps open in `held`, and `false` for a
        // failed handshake. The shared `spawn_accept_loop` is not used because it
        // ignores `Err`s, and this test needs the stalled handshake's `Err` to
        // show the stall happened.
        let (tx, mut rx) = mpsc::unbounded_channel::<bool>();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                match listener.accept().await {
                    Ok(conn) => {
                        held.push(conn);
                        let _ = tx.send(true);
                    }
                    Err(_) => {
                        let _ = tx.send(false);
                    }
                }
            }
        });

        // A simple UDP proxy between the stalling client and the server. The socket
        // facing the server is bound to 127.0.0.4, so the staller's traffic reaches
        // the server from a different source than the honest client's 127.0.0.1.
        // Otherwise the per-source limit, and not the spawn, could be what
        // protects the honest peer.
        let facing_client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = facing_client.local_addr().unwrap().to_string();
        let facing_server = UdpSocket::bind("127.0.0.4:0").await.unwrap();
        let c2s_forwarded = Arc::new(AtomicUsize::new(0));
        let c2s = c2s_forwarded.clone();
        tokio::spawn(async move {
            let mut client_addr = None;
            let mut s2c_forwarded = 0usize;
            let mut from_client = vec![0u8; 2048];
            let mut from_server = vec![0u8; 2048];
            loop {
                tokio::select! {
                    r = facing_client.recv_from(&mut from_client) => {
                        let Ok((n, src)) = r else { break };
                        client_addr = Some(src);
                        // Forward the bare Initial and the Initial with the token.
                        // Drop everything after that, so the TLS handshake can
                        // never finish.
                        if c2s.load(Ordering::SeqCst) < 2 {
                            c2s.fetch_add(1, Ordering::SeqCst);
                            let _ = facing_server.send_to(&from_client[..n], server_addr).await;
                        }
                    }
                    r = facing_server.recv_from(&mut from_server) => {
                        let Ok((n, _)) = r else { break };
                        // Forward the Retry. Drop the server's handshake data so
                        // the client can never answer it.
                        if s2c_forwarded < 1 {
                            if let Some(dst) = client_addr {
                                s2c_forwarded += 1;
                                let _ = facing_client.send_to(&from_server[..n], dst).await;
                            }
                        }
                    }
                }
            }
        });

        // A real quinn client dials the proxy. It answers the Retry, which
        // validates 127.0.0.4 on the server, then stalls when the proxy stops
        // forwarding.
        let stall_dial = tokio::spawn(async move {
            let _ = staller
                .dial(PeerAddr::new(proxy_addr), CancellationToken::new())
                .await;
        });
        // Wait for the Initial, the Retry and the Initial with the token to go
        // through. That is a few loopback round trips. After this the stalled
        // handshake holds its admission permit.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            c2s_forwarded.load(Ordering::SeqCst),
            2,
            "the proxy must have forwarded exactly the bare Initial and the tokened Initial"
        );

        let start = Instant::now();
        let honest_addr = addr.clone();
        let honest_dial = tokio::spawn(async move {
            honest
                .dial(PeerAddr::new(honest_addr), CancellationToken::new())
                .await
        });
        // Read events until the honest Ok arrives. An early Err, from the stall
        // ending sooner than expected, must not be taken for it.
        let honest_accepted = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                match rx.recv().await {
                    Some(true) => break true,
                    Some(false) => continue,
                    None => break false,
                }
            }
        })
        .await
        .unwrap_or(false);
        let elapsed = start.elapsed();
        println!("honest peer accepted beside a stalled validated handshake in {elapsed:?}");

        assert!(
            honest_accepted,
            "the honest machine was not let in after {elapsed:?}, beside a stalled handshake"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "a stalled handshake held the honest machine up for {elapsed:?}; \
             if handshakes are not run in their own tasks this is HANDSHAKE_TIMEOUT (10s)"
        );

        // Show that the stall really happened. The admitted handshake must come
        // out as an Err in the end, from our HANDSHAKE_TIMEOUT or from quinn giving
        // up on the handshake itself. Either way it was admitted and stalled for
        // seconds while the honest peer got through.
        let stall_surfaced = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match rx.recv().await {
                    Some(false) => break true,
                    Some(true) => continue,
                    None => break false,
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(
            stall_surfaced,
            "the stalled handshake never surfaced as Err — nothing was admitted, so this \
             test did not actually exercise the per-handshake spawn"
        );

        stall_dial.abort();
        let _ = honest_dial.await;
    })
    .await
}
