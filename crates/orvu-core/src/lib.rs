//! `orvu-core`: the part of Orvu that keeps copies of data in step.
//!
//! For every object it records which updates have been seen. It turns the
//! engine's data types into messages and back. And it applies updates from
//! other machines in an order that makes sense: an update waits until the
//! updates it was built on have arrived. This module sends and receives
//! nothing itself. Any network layer can sit underneath it.

pub mod error;
pub mod object;
pub mod buffer;
pub mod codec;
pub mod replica;
pub mod trace;
pub mod wire;

pub use buffer::CausalBuffer;
pub use codec::{decode_snapshot_entry, decode_wire_delta, encode_snapshot_entry, encode_wire_delta, nack_for_unreadable, Codec, NackReason, ProtobufCodec, SyncMessage};
pub use error::{Error, Result};
pub use object::{fold_into_snapshot, MutateOutcome, Object, MAX_WRITERS_PER_OBJECT, SnapshotEntry, TypedCrdt, TypedDelta, WireDelta};
pub use replica::{Applied, Replica, DEFAULT_WAITING_LIMIT_MS, MAX_CLOCK_LEAD_MS};
pub use trace::TraceId;

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
