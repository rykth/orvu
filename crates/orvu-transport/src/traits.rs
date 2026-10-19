//! The traits a transport implements and its callers rely on.
//!
//! A stream is a writer half and a reader half, so two tasks can each own one.
//! A transport that can carry several streams on one connection, such as QUIC,
//! gives each logical channel its own stream. The code that uses this opens a
//! single stream per connection.

use crate::error::Result;
use crate::types::{Identity, PeerAddr};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

pub type StreamWrite = Box<dyn AsyncWrite + Send + Unpin>;
pub type StreamRead = Box<dyn AsyncRead + Send + Unpin>;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn dial(&self, peer: PeerAddr, cancel: CancellationToken) -> Result<Box<dyn Conn>>;
    async fn listen(&self, addr: &str, cancel: CancellationToken) -> Result<Box<dyn Listener>>;
}

#[async_trait]
pub trait Listener: Send + Sync {
    /// The address the listener ended up on. Useful when the caller asked for
    /// port 0 and the system picked a port.
    fn local_addr(&self) -> String;
    async fn accept(&self) -> Result<Box<dyn Conn>>;
}

#[async_trait]
pub trait Conn: Send + Sync {
    async fn open_stream(&self) -> Result<(StreamWrite, StreamRead)>;
    async fn accept_stream(&self) -> Result<(StreamWrite, StreamRead)>;
    fn identity(&self) -> Identity;
    async fn close(&self);
}
