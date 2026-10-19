//! `orvu-runtime`: connects a `Replica` to a `Transport`, so the data on this
//! machine stays in step with the other machines.
//!
//! One engine task owns the replica. Each connection has a reader task and a
//! writer task that pass deltas (small changes) back and forth. A timer task
//! runs catch-up: each side says which updates it has seen, and the other side
//! resends what is missing, reading it from the store on disk.

pub mod antientropy;
pub mod config;
pub mod rebalance;
pub mod engine;
pub mod frontier;
pub mod leave;
pub mod metrics;
mod metrics_server;
pub mod node;
pub mod peer;
mod persistence;

pub use config::{Acl, Action, NodeConfig, QUIET_INTERVALS_AT_LEAST, QUIET_LIMIT_AT_LEAST};
pub use engine::TryOutcome;
pub use orvu_store::{FsyncPolicy, MemStore, Receipt, Store, RECEIPT_KEEP_MS};
pub use metrics::{render_histogram, render_prometheus, Metrics, MetricsSnapshot};
pub use metrics_server::{serve_page, Render};
pub use leave::{LeaveError, Leaving};
pub use node::{current_receipt, with_receipt, Node, NodeHandle, SpawnError};

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
