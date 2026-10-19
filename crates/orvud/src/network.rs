//! Serving agents on other machines, and in containers, over the network.
//!
//! The sidecar listens on a TCP address given with `--network`, besides its
//! Unix socket. Every connection is encrypted with TLS, under a certificate
//! the sidecar makes for itself the first time it is needed and keeps next to
//! its agents file: `network-cert.pem`, and `network-key.pem`, which only its
//! owner can read. Nobody has to buy or sign a certificate: an agent is given
//! this one, with its key, in a connection file, and trusts it and no other.
//!
//! Only agents with keys are let in over the network. The sidecar does not
//! start serving the network until its agents file names an agent, and a
//! program that comes over the network without a key is refused, even if the
//! agents file is later deleted.

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The certificate's file, next to the agents file.
pub const CERTIFICATE: &str = "network-cert.pem";

/// The certificate's private key's file. Only its owner can read it.
pub const PRIVATE_KEY: &str = "network-key.pem";

/// The name the certificate is made out to. An agent checks the sidecar's
/// certificate against this name and not against the address it dials, so
/// that the same certificate does for any address the sidecar is reached by.
pub const NAME: &str = "orvud";

/// How long a connection may take to agree on encryption before it is
/// dropped. A connection that never does would otherwise be held open for
/// good.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// The certificate and its key, as PEM text, made and kept in `folder` if
/// they are not there yet.
pub fn certificate(folder: &Path) -> Result<(String, String), String> {
    let (cert_path, key_path) = (folder.join(CERTIFICATE), folder.join(PRIVATE_KEY));
    let fail = |what: &str, path: &Path, e: &dyn std::fmt::Display| format!("cannot {what} {}: {e}", path.display());
    let read = || -> Result<(String, String), String> {
        let cert = std::fs::read_to_string(&cert_path).map_err(|e| fail("read the certificate", &cert_path, &e))?;
        let key = std::fs::read_to_string(&key_path).map_err(|e| fail("read the certificate's key", &key_path, &e))?;
        Ok((cert, key))
    };
    // Made already: the usual case, with no need to wait for anybody.
    if cert_path.exists() {
        return read();
    }
    // A local sidecar's data folder may not be there yet. It is made the way
    // the sidecar makes it, so that only its owner can get into it.
    orvu_sdk::make_private_folder(folder).map_err(|e| fail("create the folder", folder, &e))?;
    // One program at a time makes it. One that waited here finds it made.
    let lock = folder.join(".network.lock");
    let turn = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock)
        .map_err(|e| fail("lock", &lock, &e))?;
    turn.lock().map_err(|e| fail("lock", &lock, &e))?;
    if cert_path.exists() {
        return read();
    }
    // A key with no certificate is what a program stopped between writing
    // the two leaves. Nobody can have been given that certificate, so both
    // are made again.
    let key = rcgen::KeyPair::generate().map_err(|e| format!("cannot make a key for the certificate: {e}"))?;
    let made = rcgen::CertificateParams::new(vec![NAME.to_string()])
        .and_then(|params| params.self_signed(&key))
        .map_err(|e| format!("cannot make the certificate: {e}"))?;
    let (cert, key) = (made.pem(), key.serialize_pem());
    // Each is written beside its place and moved there, the key first: a
    // certificate is never there without its key. Only the owner can read
    // the key.
    let put = |path: &Path, text: &str, mode: u32| {
        let new = path.with_file_name(format!(".{}.new", path.file_name().unwrap_or_default().to_string_lossy()));
        let _ = std::fs::remove_file(&new);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&new)
            .and_then(|mut file| file.write_all(text.as_bytes()).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::rename(&new, path))
            .map_err(|e| fail("write", path, &e))
    };
    put(&key_path, &key, 0o600)?;
    put(&cert_path, &cert, 0o644)?;
    std::fs::File::open(folder).and_then(|folder| folder.sync_all()).map_err(|e| fail("write", folder, &e))?;
    Ok((cert, key))
}

/// What encrypts the connections, from the certificate in `folder`.
pub fn acceptor(folder: &Path) -> Result<tokio_rustls::TlsAcceptor, String> {
    let (cert, key) = certificate(folder)?;
    let certs = CertificateDer::pem_slice_iter(cert.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("the certificate in {} cannot be read: {e}", folder.join(CERTIFICATE).display()))?;
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes())
        .map_err(|e| format!("the key in {} cannot be read: {e}", folder.join(PRIVATE_KEY).display()))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .and_then(|config| config.with_no_client_auth().with_single_cert(certs, key))
        .map_err(|e| format!("the certificate cannot be used: {e}"))?;
    let mut config = config;
    // gRPC speaks HTTP/2, and a client asks for it by this name.
    config.alpn_protocols = vec![b"h2".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// What an agent needs to connect over the network: where, its key, and the
/// certificate to trust. It is written to a file the agent is given.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Connection {
    pub address: String,
    pub key: String,
    pub certificate: String,
}

impl Connection {
    /// Writes the connection to `path`, which must not be there yet. Only
    /// its owner can read it, since it holds a key.
    pub fn write(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).expect("a connection is plain text") + "\n";
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(|e| format!("cannot write the connection file {}: {e}", path.display()))
    }
}

/// The folder the certificate is kept in: the one the agents file is in.
pub fn folder(agents_file: &Path) -> PathBuf {
    agents_file.parent().filter(|folder| !folder.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf()
}

/// One connection over the network, encrypted, and counted while it is
/// open.
pub struct Secure {
    stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    _open: crate::server::OpenConnection,
    /// Its place among the connections that may be open at once, given up
    /// when it closes.
    _place: tokio::sync::OwnedSemaphorePermit,
}

impl tonic::transport::server::Connected for Secure {
    type ConnectInfo = <tokio::net::TcpStream as tonic::transport::server::Connected>::ConnectInfo;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.stream.get_ref().0.connect_info()
    }
}

impl tokio::io::AsyncRead for Secure {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Secure {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// How many connections may be agreeing on encryption at once. More wait in
/// the system's queue of connections not taken yet.
const AGREEING: usize = 64;

/// How many connections over the network may be open at once. More wait
/// until one closes.
const MOST_OPEN: usize = 256;

/// The encrypted connections that arrive on `listener`. Each is set up in a
/// task of its own, so that a slow or broken one holds none of the others
/// up, and one that does not finish within ten seconds is dropped. No more
/// than 64 are being set up at once, and no more than 256 are open: past
/// that, connections wait, so that a flood of them cannot take all the
/// sidecar has. Once nobody takes the connections any more, `listener` is
/// let go.
pub fn incoming(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    connections: crate::server::Connections,
) -> tokio_stream::wrappers::ReceiverStream<std::io::Result<Secure>> {
    incoming_within(listener, acceptor, connections, MOST_OPEN)
}

/// Like `incoming`, with no more than `most` connections open at once.
fn incoming_within(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    connections: crate::server::Connections,
    most: usize,
) -> tokio_stream::wrappers::ReceiverStream<std::io::Result<Secure>> {
    let (ready, arrived) = tokio::sync::mpsc::channel(64);
    let agreeing = Arc::new(tokio::sync::Semaphore::new(AGREEING));
    let open = Arc::new(tokio::sync::Semaphore::new(most));
    tokio::spawn(async move {
        loop {
            // A place among the open connections, and among those being set
            // up, is taken before a connection is.
            let places = tokio::select! {
                _ = ready.closed() => break,
                places = async {
                    let open = open.clone().acquire_owned().await;
                    let agreeing = agreeing.clone().acquire_owned().await;
                    (open, agreeing)
                } => places,
            };
            let (Ok(open), Ok(agreeing)) = places else { break };
            let stream = tokio::select! {
                _ = ready.closed() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot take a network connection");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let (acceptor, handed, connections) = (acceptor.clone(), ready.clone(), connections.clone());
            tokio::spawn(async move {
                let agreed = tokio::time::timeout(HANDSHAKE, acceptor.accept(stream)).await;
                drop(agreeing);
                match agreed {
                    Ok(Ok(stream)) => {
                        let secure = Secure { stream, _open: connections.enter(), _place: open };
                        let _ = handed.send(Ok(secure)).await;
                    }
                    Ok(Err(e)) => tracing::debug!(error = %e, "a network connection did not agree on encryption"),
                    Err(_) => tracing::debug!("a network connection took too long to agree on encryption"),
                }
            });
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(arrived)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_certificate_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = certificate(dir.path()).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"), "{cert}");
        assert!(key.contains("PRIVATE KEY"));
        assert_eq!(certificate(dir.path()).unwrap(), (cert, key));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(PRIVATE_KEY)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        acceptor(dir.path()).unwrap();
    }

    // The folder may not be there yet, before the sidecar has first run.
    #[test]
    fn the_folder_is_made_if_it_is_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("data");
        certificate(&folder).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&folder).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_connection_file_holds_what_an_agent_needs_and_only_its_owner_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alice.orvu");
        let connection = Connection { address: "10.0.0.5:7441".into(), key: "orvu-key-00".into(), certificate: "PEM".into() };
        connection.write(&path).unwrap();
        let read: Connection = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, connection);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // A file that is there already is not written over.
        assert!(connection.write(&path).unwrap_err().contains("cannot write the connection file"));
    }

    // A key with no certificate beside it is what a sidecar stopped between
    // writing the two leaves. Nobody can have been given that certificate,
    // so both are made again.
    #[test]
    fn a_key_left_alone_is_made_again_with_its_certificate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PRIVATE_KEY), "half made").unwrap();
        let (cert, key) = certificate(dir.path()).unwrap();
        assert!(cert.starts_with("-----BEGIN CERTIFICATE-----"));
        assert_ne!(key, "half made");
        acceptor(dir.path()).unwrap();
    }

    /// A TLS connection to `acceptor` from a client that trusts `trusted`,
    /// asking for the name `orvud`. Gives whether it was made.
    async fn handshake(acceptor: tokio_rustls::TlsAcceptor, trusted: &str) -> bool {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = acceptor.accept(stream).await;
            }
        });
        let mut roots = rustls::RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(trusted.as_bytes()) {
            roots.add(cert.unwrap()).unwrap();
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from(NAME).unwrap();
        connector.connect(name, stream).await.is_ok()
    }

    // A client trusts the certificate in its connection file, made out to
    // `orvud`, and no other: not another sidecar's, and not one made out to
    // another name.
    #[tokio::test]
    async fn a_client_trusts_only_its_sidecars_certificate() {
        let ours = tempfile::tempdir().unwrap();
        let (cert, _) = certificate(ours.path()).unwrap();
        assert!(handshake(acceptor(ours.path()).unwrap(), &cert).await);
        let theirs = tempfile::tempdir().unwrap();
        certificate(theirs.path()).unwrap();
        assert!(!handshake(acceptor(theirs.path()).unwrap(), &cert).await);
        // A certificate made out to another name, trusted all the same, is
        // still refused for the name `orvud`.
        let other = tempfile::tempdir().unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let made = rcgen::CertificateParams::new(vec!["elsewhere".to_string()]).unwrap().self_signed(&key).unwrap();
        std::fs::write(other.path().join(CERTIFICATE), made.pem()).unwrap();
        std::fs::write(other.path().join(PRIVATE_KEY), key.serialize_pem()).unwrap();
        assert!(!handshake(acceptor(other.path()).unwrap(), &made.pem()).await);
    }

    // Once nobody takes the connections any more, the address is let go.
    #[tokio::test]
    async fn the_address_is_let_go_when_serving_ends() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let arriving = incoming(listener, acceptor(dir.path()).unwrap(), crate::server::Connections::default());
        // Let it get as far as waiting for a connection.
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(arriving);
        let free = async {
            loop {
                if tokio::net::TcpListener::bind(address).await.is_ok() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        assert!(tokio::time::timeout(Duration::from_secs(3), free).await.is_ok(), "{address} was still taken");
    }

    // No more connections are open at once than the limit. One more waits
    // until another closes.
    #[tokio::test]
    async fn connections_wait_beyond_the_limit() {
        use tokio_stream::StreamExt;
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = certificate(dir.path()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut arriving = incoming_within(listener, acceptor(dir.path()).unwrap(), crate::server::Connections::default(), 1);
        let dial = || {
            let cert = cert.clone();
            tokio::spawn(async move {
                let mut roots = rustls::RootCertStore::empty();
                roots.add(CertificateDer::from_pem_slice(cert.as_bytes()).unwrap()).unwrap();
                let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                    .with_protocol_versions(&[&rustls::version::TLS13])
                    .unwrap()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let stream = tokio::net::TcpStream::connect(address).await.unwrap();
                tokio_rustls::TlsConnector::from(Arc::new(config))
                    .connect(rustls::pki_types::ServerName::try_from(NAME).unwrap(), stream)
                    .await
                    .map(|_| ())
                    .is_ok()
            })
        };
        let _first_client = dial();
        let first = tokio::time::timeout(Duration::from_secs(3), arriving.next()).await.unwrap().unwrap().unwrap();
        let _second_client = dial();
        assert!(tokio::time::timeout(Duration::from_millis(500), arriving.next()).await.is_err(), "a second came in");
        drop(first);
        assert!(tokio::time::timeout(Duration::from_secs(3), arriving.next()).await.is_ok(), "the second never came in");
    }

    #[test]
    fn a_broken_certificate_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CERTIFICATE), "not a certificate").unwrap();
        std::fs::write(dir.path().join(PRIVATE_KEY), "not a key").unwrap();
        let wrong = acceptor(dir.path()).err().unwrap();
        assert!(wrong.contains("cannot be read") || wrong.contains("cannot be used"), "{wrong}");
    }
}
