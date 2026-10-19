//! A `Transport` that works inside one process, built on tokio channels.
//!
//! `listen("a")` registers a mailbox under the name "a". `dial("a")` finds it,
//! and both sides get a `Conn` whose streams are `tokio::io::duplex` pipes. It
//! uses no sockets and behaves the same on every run, which makes it a good
//! base for testing the code that sits on top of a transport.

use crate::error::{Result, TransportError};
use crate::traits::{Conn, Listener, StreamRead, StreamWrite, Transport};
use crate::types::{replica_identity_uri, Identity, PeerAddr};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// The two `incoming` receivers use `tokio::sync::Mutex`, not `std::sync::Mutex`,
// because the lock is held across an `.await`. A `std::sync::MutexGuard` is
// `!Send`, so the async-trait futures would not compile with it. The `registry`
// below keeps `std::sync::Mutex` because its lock is never held across an await.

/// One end of a two-way connection. Its channels carry newly opened streams,
/// each as a writer and reader pair.
struct MemConn {
    /// Streams this side opens are sent to the peer's `incoming`.
    outgoing: mpsc::Sender<(StreamWrite, StreamRead)>,
    /// Streams the peer opened, waiting to be accepted here.
    incoming: tokio::sync::Mutex<mpsc::Receiver<(StreamWrite, StreamRead)>>,
    /// The remote peer's identity URI, when the connection was dialed or
    /// accepted through a `MemEndpoint`. `None` for connections made through
    /// `MemTransport` directly.
    peer_uri: Option<String>,
}

#[async_trait]
impl Conn for MemConn {
    async fn open_stream(&self) -> Result<(StreamWrite, StreamRead)> {
        // A two-way pipe. What we write, the peer reads, and the other way
        // round.
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let (our_r, our_w) = tokio::io::split(ours);
        let (their_r, their_w) = tokio::io::split(theirs);
        self.outgoing
            .send((Box::new(their_w), Box::new(their_r)))
            .await
            .map_err(|_| TransportError::Closed)?;
        Ok((Box::new(our_w), Box::new(our_r)))
    }

    async fn accept_stream(&self) -> Result<(StreamWrite, StreamRead)> {
        let mut rx = self.incoming.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }

    fn identity(&self) -> Identity {
        Identity { uri: self.peer_uri.clone(), fingerprint: None }
    }

    async fn close(&self) {}
}

struct MemListener {
    addr: String,
    incoming: tokio::sync::Mutex<mpsc::Receiver<Box<dyn Conn>>>,
}

#[async_trait]
impl Listener for MemListener {
    fn local_addr(&self) -> String {
        self.addr.clone()
    }
    async fn accept(&self) -> Result<Box<dyn Conn>> {
        let mut rx = self.incoming.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }
}

type Registry = Arc<Mutex<HashMap<String, mpsc::Sender<Box<dyn Conn>>>>>;
/// Maps an address to a replica. An entry is recorded whenever a `MemEndpoint`
/// listens on `addr`, so a dialing endpoint can learn which replica it reached.
type ReplicaRegistry = Arc<Mutex<HashMap<String, u64>>>;

/// The shared in-process network. Clone it and give a clone to each node. All
/// clones share one registry, so nodes in the same test can find each other.
#[derive(Clone, Default)]
pub struct MemTransport {
    registry: Registry,
    replicas: ReplicaRegistry,
}

impl MemTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a transport on this network that acts as one replica.
    /// Connections dialed or accepted through it report the remote peer's
    /// `orvu://replica/<id>` URI from `Conn::identity()`.
    pub fn endpoint(self: &Arc<Self>, replica: u64) -> Arc<dyn Transport> {
        Arc::new(MemEndpoint { fabric: self.clone(), local: replica })
    }
}

fn conn_pair(dialer_peer_uri: Option<String>, listener_peer_uri: Option<String>) -> (MemConn, MemConn) {
    let (a_tx, a_rx) = mpsc::channel(16);
    let (b_tx, b_rx) = mpsc::channel(16);
    (
        MemConn { outgoing: b_tx, incoming: tokio::sync::Mutex::new(a_rx), peer_uri: dialer_peer_uri },
        MemConn { outgoing: a_tx, incoming: tokio::sync::Mutex::new(b_rx), peer_uri: listener_peer_uri },
    )
}

#[async_trait]
impl Transport for MemTransport {
    async fn dial(&self, peer: PeerAddr, _cancel: CancellationToken) -> Result<Box<dyn Conn>> {
        let tx = {
            let reg = self.registry.lock().unwrap();
            reg.get(&peer.0)
                .cloned()
                .ok_or_else(|| TransportError::Dial(format!("no listener at {}", peer.0)))?
        };
        let (dialer_side, listener_side) = conn_pair(None, None);
        tx.send(Box::new(listener_side))
            .await
            .map_err(|_| TransportError::Closed)?;
        Ok(Box::new(dialer_side))
    }

    async fn listen(&self, addr: &str, _cancel: CancellationToken) -> Result<Box<dyn Listener>> {
        let (tx, rx) = mpsc::channel(16);
        self.registry.lock().unwrap().insert(addr.to_string(), tx);
        Ok(Box::new(MemListener {
            addr: addr.to_string(),
            incoming: tokio::sync::Mutex::new(rx),
        }))
    }
}

/// A `Transport` that acts as one replica on a shared `MemTransport` network.
/// `listen` records that `addr` belongs to this replica. `dial` looks up which
/// replica listens on the target address and labels both ends of the
/// connection: the dialer sees the remote's URI, and the acceptor sees this
/// endpoint's.
struct MemEndpoint {
    fabric: Arc<MemTransport>,
    local: u64,
}

#[async_trait]
impl Transport for MemEndpoint {
    async fn dial(&self, peer: PeerAddr, _cancel: CancellationToken) -> Result<Box<dyn Conn>> {
        let (remote, tx) = {
            let replicas = self.fabric.replicas.lock().unwrap();
            let reg = self.fabric.registry.lock().unwrap();
            let tx = reg
                .get(&peer.0)
                .cloned()
                .ok_or_else(|| TransportError::Dial(format!("no listener at {}", peer.0)))?;
            (replicas.get(&peer.0).copied(), tx)
        };
        let (dialer_side, listener_side) = conn_pair(
            remote.map(replica_identity_uri),
            Some(replica_identity_uri(self.local)),
        );
        tx.send(Box::new(listener_side))
            .await
            .map_err(|_| TransportError::Closed)?;
        Ok(Box::new(dialer_side))
    }

    async fn listen(&self, addr: &str, cancel: CancellationToken) -> Result<Box<dyn Listener>> {
        self.fabric.replicas.lock().unwrap().insert(addr.to_string(), self.local);
        self.fabric.listen(addr, cancel).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{read_frame, write_frame};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn dial_rendezvous_with_listen_and_streams_carry_frames() {
        let t = MemTransport::new();
        let cancel = CancellationToken::new();
        let listener = t.listen("node-a", cancel.clone()).await.unwrap();

        let dial_t = t.clone();
        let dialer = tokio::spawn(async move {
            dial_t.dial(PeerAddr::new("node-a"), CancellationToken::new()).await.unwrap()
        });

        let server_conn = listener.accept().await.unwrap();
        let client_conn = dialer.await.unwrap();

        // The client opens a stream, the server accepts it, and a frame goes
        // across.
        let (mut cw, _cr) = client_conn.open_stream().await.unwrap();
        let (_sw, mut sr) = server_conn.accept_stream().await.unwrap();
        write_frame(&mut cw, b"ping").await.unwrap();
        assert_eq!(read_frame(&mut sr).await.unwrap(), b"ping");
    }

    #[tokio::test]
    async fn dial_unknown_address_fails() {
        let t = MemTransport::new();
        let err = t.dial(PeerAddr::new("nope"), CancellationToken::new()).await;
        assert!(matches!(err, Err(TransportError::Dial(_))));
    }

    #[tokio::test]
    async fn mem_conn_reports_peer_identity_uri() {
        use std::sync::Arc;
        let fabric = Arc::new(MemTransport::new());
        let a = fabric.endpoint(1);
        let b = fabric.endpoint(2);
        let listener = b.listen("b", CancellationToken::new()).await.unwrap();
        let dialer_conn = a.dial(PeerAddr::new("b"), CancellationToken::new()).await.unwrap();
        let accepted = listener.accept().await.unwrap();

        // The dialer reached replica 2, and the acceptor saw replica 1 connect.
        assert_eq!(dialer_conn.identity().uri.as_deref(), Some("orvu://replica/2"));
        assert_eq!(accepted.identity().uri.as_deref(), Some("orvu://replica/1"));
    }
}
