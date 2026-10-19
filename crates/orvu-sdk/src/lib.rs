//! `orvu-sdk`: the API developers use to work with Orvu from inside their own
//! program.
//!
//! Start a `Client`, open typed handles on it for shared data such as counters
//! and sets, change them like local values, and subscribe to live changes. The
//! engine runs inside your process (library mode).

pub mod client;
pub mod config;
pub mod error;
pub mod handle;
#[cfg(test)]
mod hardening_tests;
#[cfg(test)]
mod ticket_tests;

pub use client::{make_private_folder, ChangeFollower, Client};
pub use orvu_runtime::{current_receipt, with_receipt, Receipt, RECEIPT_KEEP_MS};
pub use config::SdkConfig;
pub use error::{Result, SdkError};
pub use orvu_crdt::{Span, TreeNode};
pub use handle::{BoundedCounter, Cart, Counter, Handle, Json, JsonChange, List, Map, MvMap, Register, RichText, Set, Tree};

/// The consistency class of an object: the rules it follows when machines
/// cannot reach each other. The class is decided by which constructor you call.
/// So it is visible where the object is opened and is never a hidden default,
/// and the code shows what you trade for availability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsistencyClass {
    /// A data type that merges without asking any other machine. Always
    /// available.
    Ap,
    /// A bounded counter, which keeps a number at or above zero by giving each
    /// machine an allowance to spend. Available as long as the local allowance
    /// lasts.
    Escrow,
}

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
