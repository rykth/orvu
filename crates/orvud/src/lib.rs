//! `orvud` is the Orvu sidecar: a small background program that runs the
//! engine and lets programs on the same machine use it over gRPC.
//!
//! It is a translation layer over `orvu_sdk::Client` and does nothing else. One
//! process holds one engine, which means one replica id, one store and one set
//! of peers. It serves several local programs over a Unix socket.
//!
//! One rule shapes everything here: a refusal is a value, not an error. A
//! refusal is the engine saying no and leaving everything as it was, and
//! `Exhausted` is one. It travels as an ordinary response, because a generated
//! client in another language turns a non-OK gRPC status into a raised
//! exception. "Refused, your state is intact" must not arrive looking like
//! "failed, your state is unknown".

pub mod pb {
    tonic::include_proto!("orvu.dataplane.v1");
}

/// The version of this sidecar. Clients are told it when they open a session.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod agents;
pub mod audit;
pub mod boot;
pub mod cli;
pub mod errors;
pub mod exec;
pub mod local;
pub mod network;
pub mod notes;
pub mod rules;
pub mod say;
pub mod search;
pub mod observe;
pub mod requests;
pub mod server;
pub mod session;
pub mod setup;
pub mod stats;
pub mod watch;

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
