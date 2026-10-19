//! Transport errors. The transport moves bytes and never looks at what they
//! mean.

use thiserror::Error;

/// What can go wrong when connecting, listening, or sending and receiving.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection closed")]
    Closed,
    #[error("dial failed: {0}")]
    Dial(String),
    #[error("listen failed: {0}")]
    Listen(String),
    #[error("tls setup failed: {0}")]
    Tls(String),
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(usize),
}

pub type Result<T> = std::result::Result<T, TransportError>;
