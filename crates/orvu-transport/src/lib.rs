//! `orvu-transport`: moves bytes between machines.
//!
//! This crate defines the `Transport` and `Conn` traits and implements them:
//! `QuicTransport` for real networks, `MemTransport` for tests. It carries
//! whole messages (frames) without looking inside them. It does not understand
//! the shared data or the sync protocol, and it does not depend on `orvu-core`.

mod accept_pump;
pub mod certid;
pub mod error;
pub mod fault;
pub mod frame;
pub mod mem;
pub mod quic;
pub mod traits;
pub mod types;

pub use certid::{generate_dev_ca, issue_node_cert, san_uri_from_cert_der, DevCa, NodeCertPems};
pub use error::{Result, TransportError};
pub use fault::{Cuts, FaultTransport};
pub use frame::MAX_FRAME;

/// Installs the default rustls crypto provider for the whole process. Calling
/// it more than once is harmless.
pub fn install_default_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
pub use mem::MemTransport;
pub use quic::{LinkTimes, QuicTransport};
pub use traits::{Conn, Listener, StreamRead, StreamWrite, Transport};
pub use types::{Identity, PeerAddr};

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
