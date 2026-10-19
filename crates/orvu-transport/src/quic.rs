//! A transport over QUIC, using the quinn library. QUIC always uses TLS 1.3, so
//! the connection is always encrypted. Both peers present a node certificate
//! and check the other side's certificate against a configured certificate
//! authority (CA). This is called mutual TLS. A peer whose certificate was not
//! issued under that CA gets no usable stream. `QuicConn::identity()` returns
//! the verified peer's `URI:orvu://replica/<id>` subject alternative name
//! (SAN).

use crate::error::{Result, TransportError};
use crate::traits::{Conn, Listener, StreamRead, StreamWrite, Transport};
use crate::types::{Identity, PeerAddr};
use async_trait::async_trait;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::RootCertStore;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Reads the certificates out of PEM text, in order, as DER. Fails if there
/// are none.
pub(crate) fn certs_from_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut chain = Vec::new();
    for item in x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes()) {
        let block = item.map_err(|e| TransportError::Tls(format!("bad cert PEM: {e}")))?;
        if block.label == "CERTIFICATE" {
            chain.push(CertificateDer::from(block.contents));
        }
    }
    if chain.is_empty() {
        return Err(TransportError::Tls("no certificates in PEM".into()));
    }
    Ok(chain)
}

/// Reads the first private key out of PEM text, as DER.
pub(crate) fn key_from_pem(pem: &str) -> Result<PrivateKeyDer<'static>> {
    for item in x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes()) {
        let block = item.map_err(|e| TransportError::Tls(format!("bad key PEM: {e}")))?;
        if block.label.ends_with("PRIVATE KEY") {
            return PrivateKeyDer::try_from(block.contents)
                .map_err(|e| TransportError::Tls(format!("bad private key: {e}")));
        }
    }
    Err(TransportError::Tls("no private key in PEM".into()))
}

/// Builds the set of trusted CA certificates from the ones in `ca_pem`.
pub(crate) fn roots_from_pem(ca_pem: &str) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    for cert in certs_from_pem(ca_pem)? {
        roots
            .add(cert)
            .map_err(|e| TransportError::Tls(format!("bad CA cert: {e}")))?;
    }
    Ok(roots)
}

/// The time limits of a QUIC link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkTimes {
    /// A link on which nothing at all has been heard for this long is given
    /// up. This is how a machine whose cable was pulled, or that froze, is
    /// noticed. Both ends say what they want, and the shorter of the two is
    /// used.
    pub idle: Duration,
    /// How often a link that has nothing else to send sends a tiny packet, so
    /// a healthy quiet link is not given up as idle. It should be well under
    /// `idle`.
    pub keep_alive: Duration,
    /// The longest one dial may take, looking up the name included. A dial
    /// to a machine that is switched off is not refused, it gets no answer,
    /// and this is how long we wait for one.
    pub dial: Duration,
}

impl Default for LinkTimes {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(30),
            keep_alive: Duration::from_secs(5),
            dial: Duration::from_secs(10),
        }
    }
}

/// A `Transport` that connects machines over QUIC with mutual TLS.
pub struct QuicTransport {
    server_config: quinn::ServerConfig,
    client_config: quinn::ClientConfig,
    times: LinkTimes,
}

impl QuicTransport {
    /// Builds a transport from this node's certificate chain and key, and the
    /// CA certificates to check peers against. All three are PEM text. Whether
    /// we dial or accept, the peer must present a certificate issued under
    /// `ca_pem`.
    pub fn new(cert_chain_pem: &str, key_pem: &str, ca_pem: &str) -> Result<Self> {
        Self::with_times(cert_chain_pem, key_pem, ca_pem, LinkTimes::default())
    }

    /// Like `new`, with time limits other than the default ones.
    pub fn with_times(cert_chain_pem: &str, key_pem: &str, ca_pem: &str, times: LinkTimes) -> Result<Self> {
        let chain = certs_from_pem(cert_chain_pem)?;
        let key = key_from_pem(key_pem)?;
        let transport = Arc::new(link_settings(&times)?);
        let roots = Arc::new(roots_from_pem(ca_pem)?);

        // Server side: present our certificate, and require the client to
        // present one issued under the CA.
        let client_verifier = rustls::server::WebPkiClientVerifier::builder(roots.clone())
            .build()
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        let mut server_crypto = rustls::ServerConfig::builder()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(chain.clone(), key.clone_key())
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        server_crypto.alpn_protocols = vec![b"orvu/1".to_vec()];
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        ));
        server_config.transport_config(transport.clone());

        // Client side: present our certificate, and check the server's
        // certificate and name against the CA.
        let mut client_crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(chain, key)
            .map_err(|e| TransportError::Tls(e.to_string()))?;
        client_crypto.alpn_protocols = vec![b"orvu/1".to_vec()];
        let mut client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        ));
        client_config.transport_config(transport);

        Ok(Self { server_config, client_config, times })
    }
}

/// The QUIC settings that carry the idle time and the keep-alive.
fn link_settings(times: &LinkTimes) -> Result<quinn::TransportConfig> {
    let idle = quinn::IdleTimeout::try_from(times.idle)
        .map_err(|_| TransportError::Tls(format!("the idle time {:?} is too long", times.idle)))?;
    let mut settings = quinn::TransportConfig::default();
    settings.max_idle_timeout(Some(idle));
    settings.keep_alive_interval(Some(times.keep_alive));
    Ok(settings)
}

/// How long one address of a name is given to answer before the next one is
/// tried beside it.
const NEXT_ADDRESS: Duration = Duration::from_millis(250);

/// Connects to one address, giving up after `limit`. Without a limit, an
/// address where nobody answers is waited on until the link's idle time has
/// passed.
async fn dial_addr(config: quinn::ClientConfig, addr: SocketAddr, limit: Duration) -> Result<Box<dyn Conn>> {
    // A local endpoint on a system-chosen port, used only for this dial. It
    // has to be of the same kind as the address being dialed: an IPv4 socket
    // cannot reach an IPv6 address.
    let local: SocketAddr = if addr.is_ipv6() {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    };
    let mut endpoint =
        quinn::Endpoint::client(local).map_err(|e| TransportError::Dial(e.to_string()))?;
    endpoint.set_default_client_config(config);
    let connecting = endpoint
        .connect(addr, "orvu-node")
        .map_err(|e| TransportError::Dial(e.to_string()))?;
    let conn = tokio::time::timeout(limit, connecting)
        .await
        .map_err(|_| TransportError::Dial(format!("{addr} did not answer within {limit:?}")))?
        .map_err(|e| TransportError::Dial(e.to_string()))?;
    Ok(Box::new(QuicConn(conn)))
}

impl QuicTransport {
    /// Dials the addresses `peer` names, as `dial` describes, with no overall
    /// time limit.
    async fn dial_any(&self, peer: &PeerAddr) -> Result<Box<dyn Conn>> {
        // The machine is given as `host:port`. The host is a name or an
        // address. A name is looked up on every dial, so a machine that has
        // moved to another address is found again.
        let found = tokio::net::lookup_host(peer.0.as_str())
            .await
            .map_err(|e| TransportError::Dial(format!("cannot find {}: {e}", peer.0)))?;
        // A name can have several addresses, and the machine may answer on
        // only some of them. `localhost` is one such name: it is both an IPv6
        // and an IPv4 address, and a dial to the one nobody listens on does
        // not fail, it waits. So the addresses are tried in turn, each a
        // little after the one before and without waiting for it to give up,
        // and the first to connect is used. The rest are dropped with the
        // set. A machine that answers on its first address is dialed once.
        let mut left = found.collect::<Vec<_>>().into_iter();
        let mut tries = tokio::task::JoinSet::new();
        let mut failed = TransportError::Dial(format!("no address was found for {}", peer.0));
        loop {
            if tries.is_empty() {
                match left.next() {
                    Some(addr) => drop(tries.spawn(dial_addr(self.client_config.clone(), addr, self.times.dial))),
                    None => return Err(failed),
                }
            }
            tokio::select! {
                done = tries.join_next() => match done {
                    Some(Ok(Ok(conn))) => return Ok(conn),
                    Some(Ok(Err(e))) => failed = TransportError::Dial(format!("{}: {e}", peer.0)),
                    Some(Err(e)) => failed = TransportError::Dial(format!("{}: {e}", peer.0)),
                    None => {}
                },
                _ = tokio::time::sleep(NEXT_ADDRESS), if left.len() > 0 => {
                    if let Some(addr) = left.next() {
                        tries.spawn(dial_addr(self.client_config.clone(), addr, self.times.dial));
                    }
                }
            }
        }
    }
}

struct QuicConn(quinn::Connection);

#[async_trait]
impl Conn for QuicConn {
    async fn open_stream(&self) -> Result<(StreamWrite, StreamRead)> {
        let (w, r) = self.0.open_bi().await.map_err(|e| TransportError::Dial(e.to_string()))?;
        Ok((Box::new(w), Box::new(r)))
    }
    async fn accept_stream(&self) -> Result<(StreamWrite, StreamRead)> {
        let (w, r) = self.0.accept_bi().await.map_err(|_| TransportError::Closed)?;
        Ok((Box::new(w), Box::new(r)))
    }
    fn identity(&self) -> Identity {
        // quinn gives us the peer's verified certificate chain as
        // `Vec<CertificateDer<'static>>`. The first certificate is the peer's
        // own, and its `URI:orvu://replica/<id>` SAN is the identity URI.
        let uri = self
            .0
            .peer_identity()
            .and_then(|any| any.downcast::<Vec<CertificateDer<'static>>>().ok())
            .and_then(|chain| {
                chain
                    .first()
                    .and_then(|ee| crate::san_uri_from_cert_der(ee.as_ref()))
            });
        Identity { uri, fingerprint: None }
    }
    async fn close(&self) {
        self.0.close(0u32.into(), b"bye");
    }
}

struct QuicListener {
    addr: SocketAddr,
    queue: crate::accept_pump::AcceptQueue,
    /// Kept so the endpoint lives at least as long as the listener. The accept
    /// loop has its own clone.
    _endpoint: quinn::Endpoint,
    /// Cancels the accept loop's token when the listener is dropped. The loop
    /// then exits and drops its endpoint clone, which frees the UDP port.
    /// Without this the loop would keep running, and keep the socket open,
    /// until the caller's token is cancelled.
    _guard: tokio_util::sync::DropGuard,
}

#[async_trait]
impl Listener for QuicListener {
    fn local_addr(&self) -> String {
        self.addr.to_string()
    }
    async fn accept(&self) -> Result<Box<dyn Conn>> {
        self.queue.accept().await
    }
}

#[async_trait]
impl Transport for QuicTransport {
    async fn dial(&self, peer: PeerAddr, cancel: CancellationToken) -> Result<Box<dyn Conn>> {
        // The whole dial, the name lookup included, ends when the caller
        // cancels it or when the dial time limit has passed, whichever comes
        // first. The attempts still running are dropped with it.
        let limit = self.times.dial;
        tokio::select! {
            _ = cancel.cancelled() => Err(TransportError::Dial(format!("{}: the dial was cancelled", peer.0))),
            dialed = tokio::time::timeout(limit, self.dial_any(&peer)) => match dialed {
                Ok(dialed) => dialed,
                Err(_) => Err(TransportError::Dial(format!("{} did not answer within {limit:?}", peer.0))),
            },
        }
    }

    async fn listen(&self, addr: &str, cancel: CancellationToken) -> Result<Box<dyn Listener>> {
        let sock: SocketAddr = addr.parse().map_err(|_| TransportError::Listen(format!("bad addr {addr}")))?;
        let endpoint = quinn::Endpoint::server(self.server_config.clone(), sock)
            .map_err(|e| TransportError::Listen(e.to_string()))?;
        let bound = endpoint.local_addr().map_err(|e| TransportError::Listen(e.to_string()))?;

        let (tx, queue) = crate::accept_pump::AcceptQueue::new(crate::accept_pump::ACCEPT_QUEUE_CAP);
        let admission = crate::accept_pump::HandshakeAdmission::new(
            crate::accept_pump::PER_IP_HANDSHAKES,
            crate::accept_pump::PER_BLOCK_HANDSHAKES,
            crate::accept_pump::MAX_HANDSHAKES,
        );
        // The accept loop needs its own handle, because `Endpoint::accept`
        // borrows `&self`.
        let ep = endpoint.clone();
        // A child of the caller's token. The loop ends when the caller cancels
        // its token (shutdown), or when the listener is dropped (the drop
        // guard below).
        let pump_token = cancel.child_token();
        let guard = pump_token.clone().drop_guard();
        tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    _ = pump_token.cancelled() => break,
                    i = ep.accept() => match i {
                        Some(i) => i,
                        None => break, // endpoint closed
                    },
                };
                // Check the source address before counting the attempt. A
                // QUIC Initial packet arrives over UDP, so its source address
                // is not verified and can be forged. If we counted it, an
                // attacker could forge a new address for every packet and get
                // around the per-address limit at no cost. `retry()` asks the
                // client to prove it owns its address. It keeps no state: we
                // send one packet and remember nothing about the connection.
                // `may_retry()` is always true when the address has not been
                // validated.
                //
                // This has a cost. Nothing in this codebase resumes an earlier
                // session, so every new dial arrives unvalidated and is asked
                // to retry. That is one extra round trip for every connection,
                // at startup and on every reconnect, not only under load. We
                // still retry every time. `admission.claim` is only reached
                // after this check, so a retry that only happened under load
                // would, the rest of the time, give slots straight to
                // unverified addresses. A flood from forged addresses could
                // then use up the slots and shut out an honest peer. The test
                // `unvalidated_sources_are_retried_not_admitted_so_a_spoofed_flood_cannot_shed_an_honest_peer`
                // checks for this. RFC 9000 section 8.1 also recommends a
                // retry before keeping any state, for a separate reason: it
                // stops the server being used to amplify traffic.
                if !incoming.remote_address_validated() && incoming.may_retry() {
                    let _ = incoming.retry();
                    continue;
                }
                let ip = incoming.remote_address().ip();
                let Some(permit) = admission.claim(ip) else {
                    // No slot is free for this attempt. Refuse it so the
                    // client finds out now and does not wait for a timeout.
                    incoming.refuse();
                    continue;
                };
                let tx = tx.clone();
                // Each handshake runs in its own task. The retry check above
                // only turns away clients that never prove their address. This
                // task guards against a different attacker: one that answers
                // the retry, so `remote_address_validated()` is true and it
                // gets a permit, and then stalls the rest of the TLS
                // handshake. If the handshake ran inside this loop, that one
                // stalled client would block the whole loop for up to
                // HANDSHAKE_TIMEOUT. Retries and new attempts from every other
                // source would wait behind it, so handshakes would again be
                // handled one at a time. The test
                // `a_validated_client_that_stalls_the_tls_handshake_does_not_block_other_sources`
                // fails if this spawn is removed and the handshake is awaited
                // here.
                tokio::spawn(async move {
                    let _permit = permit;
                    // `Incoming` is `IntoFuture`, not `Future`, so it cannot be
                    // passed to `timeout` directly.
                    let done = tokio::time::timeout(
                        crate::accept_pump::HANDSHAKE_TIMEOUT,
                        async move { incoming.await },
                    )
                    .await;
                    let result = match done {
                        Ok(Ok(conn)) => Ok(Box::new(QuicConn(conn)) as Box<dyn Conn>),
                        Ok(Err(e)) => Err(TransportError::Listen(e.to_string())),
                        Err(_) => Err(TransportError::Listen("handshake timed out".into())),
                    };
                    // `offer` never waits, so the permit is given back as soon
                    // as the handshake is over.
                    tx.offer(result);
                });
            }
        });
        Ok(Box::new(QuicListener { addr: bound, queue, _endpoint: endpoint, _guard: guard }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_frame, write_frame};
    use std::time::Duration;

    #[tokio::test]
    async fn dials_an_ipv6_address() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = crate::generate_dev_ca();
        let n1 = crate::issue_node_cert(&ca, 1);
        let n2 = crate::issue_node_cert(&ca, 2);
        let server = QuicTransport::new(&n2.cert_pem, &n2.key_pem, &ca.cert_pem).unwrap();
        let client = QuicTransport::new(&n1.cert_pem, &n1.key_pem, &ca.cert_pem).unwrap();

        // Some machines have IPv6 switched off. There is nothing to test then.
        let Ok(listener) = server.listen("[::1]:0", CancellationToken::new()).await else {
            return;
        };
        let addr = PeerAddr::new(listener.local_addr());
        let (dialed, accepted) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), client.dial(addr, CancellationToken::new())),
            tokio::time::timeout(Duration::from_secs(5), listener.accept()),
        );
        assert!(dialed.expect("dial timed out").is_ok());
        assert!(accepted.expect("accept timed out").is_ok());
    }

    fn pair() -> (QuicTransport, QuicTransport) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = crate::generate_dev_ca();
        let n1 = crate::issue_node_cert(&ca, 1);
        let n2 = crate::issue_node_cert(&ca, 2);
        (
            QuicTransport::new(&n2.cert_pem, &n2.key_pem, &ca.cert_pem).unwrap(),
            QuicTransport::new(&n1.cert_pem, &n1.key_pem, &ca.cert_pem).unwrap(),
        )
    }

    // A machine can be given by name. `localhost` is the one name every
    // computer knows.
    #[tokio::test]
    async fn dials_a_machine_by_name() {
        let (server, client) = pair();
        let listener = server.listen("127.0.0.1:0", CancellationToken::new()).await.unwrap();
        let port = listener.local_addr().rsplit(':').next().unwrap().to_string();
        let named = PeerAddr::new(format!("localhost:{port}"));
        let (dialed, accepted) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(20), client.dial(named, CancellationToken::new())),
            tokio::time::timeout(Duration::from_secs(20), listener.accept()),
        );
        let conn = dialed.expect("dial timed out").expect("the name was not dialed");
        assert_eq!(conn.identity().uri.as_deref(), Some("orvu://replica/2"));
        assert!(accepted.expect("accept timed out").is_ok());
    }

    // `localhost` is two addresses. A machine that listens on both must get
    // one connection from a dial, not one for each.
    #[tokio::test]
    async fn a_name_with_two_addresses_that_both_answer_gets_one_connection() {
        let (server, client) = pair();
        let v4 = server.listen("127.0.0.1:0", CancellationToken::new()).await.unwrap();
        let port = v4.local_addr().rsplit(':').next().unwrap().to_string();
        // Some machines have IPv6 switched off. There is nothing to test then.
        let Ok(v6) = server.listen(&format!("[::1]:{port}"), CancellationToken::new()).await else {
            return;
        };
        let named = PeerAddr::new(format!("localhost:{port}"));
        let conn = tokio::time::timeout(Duration::from_secs(10), client.dial(named, CancellationToken::new()))
            .await
            .expect("dial timed out")
            .expect("the name was not dialed");

        let mut accepted = 0;
        let count = async {
            loop {
                tokio::select! {
                    got = v4.accept() => { if got.is_ok() { accepted += 1 } }
                    got = v6.accept() => { if got.is_ok() { accepted += 1 } }
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(800), count).await;
        assert_eq!(accepted, 1, "the machine was connected to {accepted} times by one dial");
        conn.close().await;
    }

    fn pair_with(times: LinkTimes) -> (QuicTransport, QuicTransport) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = crate::generate_dev_ca();
        let n1 = crate::issue_node_cert(&ca, 1);
        let n2 = crate::issue_node_cert(&ca, 2);
        (
            QuicTransport::with_times(&n2.cert_pem, &n2.key_pem, &ca.cert_pem, times).unwrap(),
            QuicTransport::with_times(&n1.cert_pem, &n1.key_pem, &ca.cert_pem, times).unwrap(),
        )
    }

    /// A UDP socket that takes packets and never answers, as a machine that is
    /// switched off looks to a dial.
    fn nobody_answers() -> (std::net::UdpSocket, PeerAddr) {
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = PeerAddr::new(silent.local_addr().unwrap().to_string());
        (silent, addr)
    }

    #[tokio::test]
    async fn a_dial_that_gets_no_answer_gives_up_at_its_time_limit() {
        let times = LinkTimes { dial: Duration::from_millis(300), ..LinkTimes::default() };
        let (_server, client) = pair_with(times);
        let (_silent, addr) = nobody_answers();
        let started = std::time::Instant::now();
        let dialed = tokio::time::timeout(Duration::from_secs(5), client.dial(addr, CancellationToken::new()))
            .await
            .expect("the dial went on past its time limit");
        assert!(matches!(dialed, Err(TransportError::Dial(_))));
        assert!(started.elapsed() < Duration::from_secs(2), "it took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_dial_ends_when_it_is_cancelled() {
        let (_server, client) = pair();
        let (_silent, addr) = nobody_answers();
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            stop.cancel();
        });
        let dialed = tokio::time::timeout(Duration::from_secs(3), client.dial(addr, cancel))
            .await
            .expect("the dial did not end when it was cancelled");
        assert!(dialed.is_err());
    }

    /// Passes UDP packets between one client and `server`, until `cut` is set.
    /// Then it drops them all, as a pulled cable does.
    async fn cable(server: SocketAddr) -> (PeerAddr, Arc<std::sync::atomic::AtomicBool>) {
        use std::sync::atomic::Ordering;
        let front = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let back = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        back.connect(server).await.unwrap();
        let addr = PeerAddr::new(front.local_addr().unwrap().to_string());
        let cut = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pulled = cut.clone();
        tokio::spawn(async move {
            let mut client = None;
            let (mut up, mut down) = (vec![0u8; 65536], vec![0u8; 65536]);
            loop {
                tokio::select! {
                    Ok((n, from)) = front.recv_from(&mut up) => {
                        client = Some(from);
                        if !pulled.load(Ordering::SeqCst) {
                            let _ = back.send(&up[..n]).await;
                        }
                    }
                    Ok(n) = back.recv(&mut down) => {
                        if let (Some(to), false) = (client, pulled.load(Ordering::SeqCst)) {
                            let _ = front.send_to(&down[..n], to).await;
                        }
                    }
                }
            }
        });
        (addr, cut)
    }

    fn short_idle() -> LinkTimes {
        LinkTimes { idle: Duration::from_millis(600), keep_alive: Duration::from_millis(150), ..LinkTimes::default() }
    }

    #[tokio::test]
    async fn a_link_whose_cable_was_pulled_is_given_up_after_the_idle_time() {
        let (server, client) = pair_with(short_idle());
        let listener = server.listen("127.0.0.1:0", CancellationToken::new()).await.unwrap();
        let (addr, cut) = cable(listener.local_addr().parse().unwrap()).await;
        let conn = client.dial(addr, CancellationToken::new()).await.expect("dial");
        cut.store(true, std::sync::atomic::Ordering::SeqCst);
        let ended = tokio::time::timeout(Duration::from_secs(3), conn.accept_stream())
            .await
            .expect("the link was not given up after the idle time");
        assert!(ended.is_err());
    }

    #[tokio::test]
    async fn a_healthy_quiet_link_is_kept_open() {
        let (server, client) = pair_with(short_idle());
        let listener = server.listen("127.0.0.1:0", CancellationToken::new()).await.unwrap();
        let (addr, _cut) = cable(listener.local_addr().parse().unwrap()).await;
        let conn = client.dial(addr, CancellationToken::new()).await.expect("dial");
        // Nothing is sent for more than three times the idle time.
        let waited = tokio::time::timeout(Duration::from_secs(2), conn.accept_stream()).await;
        assert!(waited.is_err(), "a quiet link that was still working was given up");
    }

    #[tokio::test]
    async fn a_name_nobody_knows_is_a_dial_error_that_says_which() {
        let (_server, client) = pair();
        for bad in ["no-such-machine.invalid:7440", "127.0.0.1", "not an address"] {
            let failed = client.dial(PeerAddr::new(bad), CancellationToken::new()).await;
            match failed {
                Err(TransportError::Dial(message)) => assert!(message.contains(bad), "{message}"),
                Err(other) => panic!("{bad}: expected a dial error, got {other}"),
                Ok(_) => panic!("{bad} was dialed"),
            }
        }
    }

    #[tokio::test]
    async fn quic_round_trips_and_exposes_peer_identity() {
        // rustls 0.23 needs a crypto provider installed once per process.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = crate::generate_dev_ca();
        let n1 = crate::issue_node_cert(&ca, 1);
        let n2 = crate::issue_node_cert(&ca, 2);

        let server = Arc::new(QuicTransport::new(&n2.cert_pem, &n2.key_pem, &ca.cert_pem).unwrap());
        let client = Arc::new(QuicTransport::new(&n1.cert_pem, &n1.key_pem, &ca.cert_pem).unwrap());

        let listener = server.listen("127.0.0.1:0", CancellationToken::new()).await.unwrap();
        let addr = listener.local_addr();
        let srv = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap();
            let id = conn.identity().uri; // the client's identity (replica 1)
            let (_w, mut r) = conn.accept_stream().await.unwrap();
            (id, read_frame(&mut r).await.unwrap())
        });
        let conn = client.dial(PeerAddr::new(addr), CancellationToken::new()).await.unwrap();
        assert_eq!(conn.identity().uri.as_deref(), Some("orvu://replica/2")); // the server's identity
        let (mut w, _r) = conn.open_stream().await.unwrap();
        write_frame(&mut w, b"hi").await.unwrap();
        let (client_id, bytes) = srv.await.unwrap();
        assert_eq!(client_id.as_deref(), Some("orvu://replica/1"));
        assert_eq!(bytes, b"hi");
    }

    #[tokio::test]
    async fn quic_rejects_a_cert_from_a_different_ca() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = crate::generate_dev_ca();
        let rogue_ca = crate::generate_dev_ca();
        // Create the server's certificate once so its certificate and key
        // match. The only reason the connection can then fail is that the rogue
        // client's certificate was not issued under `ca`.
        let srv = crate::issue_node_cert(&ca, 2);
        let server =
            Arc::new(QuicTransport::new(&srv.cert_pem, &srv.key_pem, &ca.cert_pem).unwrap());
        let rogue = crate::issue_node_cert(&rogue_ca, 1);
        let client =
            Arc::new(QuicTransport::new(&rogue.cert_pem, &rogue.key_pem, &ca.cert_pem).unwrap());
        let listener = server.listen("127.0.0.1:0", CancellationToken::new()).await.unwrap();
        let addr = listener.local_addr();
        // The server does real work on the stream: it accepts a connection,
        // accepts a two-way stream, reads one frame and sends it back. A
        // verified peer completes this whole round trip. An untrusted peer's
        // connection is closed by the client-certificate check before any
        // stream is handled. Doing this work is what lets the assertion below
        // tell a trusted CA from an untrusted one. If the server only accepted
        // the connection and dropped it, the client's read would fail for any
        // peer, verified or not.
        tokio::spawn(async move {
            if let Ok(conn) = listener.accept().await {
                if let Ok((mut w, mut r)) = conn.accept_stream().await {
                    if let Ok(frame) = read_frame(&mut r).await {
                        let _ = write_frame(&mut w, &frame).await;
                        // Keep the connection open until the client has read
                        // the reply. This read finishes with `Closed` once the
                        // client drops its side. Dropping `conn` right away
                        // would send a CONNECTION_CLOSE that could arrive
                        // before the reply bytes.
                        let _ = read_frame(&mut r).await;
                    }
                }
            }
        });
        // The rogue client's certificate was not issued under `ca`, so the
        // server rejects it.
        let dialed = client.dial(PeerAddr::new(addr), CancellationToken::new()).await;
        // A full round trip means dialing, opening a stream, writing a frame
        // and reading the server's reply. Only a peer the server verified can
        // get that far. If the server ever wrongly accepted the client, the
        // timeout makes the test fail cleanly and not hang.
        let round_trip_ok = match dialed {
            Err(_) => false,
            Ok(conn) => match conn.open_stream().await {
                Err(_) => false,
                Ok((mut w, mut r)) => tokio::time::timeout(Duration::from_secs(5), async {
                    write_frame(&mut w, b"hi").await.is_ok()
                        && read_frame(&mut r).await.map(|b| b == b"hi").unwrap_or(false)
                })
                .await
                .unwrap_or(false),
            },
        };
        assert!(
            !round_trip_ok,
            "rogue client from a different CA must not complete a stream round-trip"
        );
    }
}
