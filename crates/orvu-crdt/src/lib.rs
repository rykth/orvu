//! `orvu-crdt`: the shared data types of the Orvu platform. They are CRDTs
//! (conflict-free replicated data types): data that several machines can
//! change separately and then merge, always ending up with the same value.
//! A change travels between machines as a "delta", a small record of just that
//! change. The crate also holds the bookkeeping that tracks which changes each
//! machine has already seen.

pub mod id;
pub mod bounded;
pub mod clock;
pub mod context;
pub mod counter;
mod error;
mod dot_index;
pub mod json;
pub mod mvmap;
pub mod ormap;
pub mod orset;
pub mod register;
pub mod rga;
pub mod richtext;
mod fracindex;
mod tree;
pub mod version_vector;

pub use bounded::{BoundedCounter, BoundedDelta};
pub use clock::Hlc;
pub use context::{DotContext, MAX_COUNTER};
pub use counter::{PnCounter, PnDelta};
pub use error::WriteError;
pub use id::{Dot, ReplicaId};
pub use json::{parse_path, FieldKey, Generation, JsonCrdt, JsonDelta, JsonError, JsonParseError, JsonScalar, JsonTag, JsonValue, MAX_NESTING as MAX_JSON_NESTING, MAX_GENERATION_STEP, OrderedF64, Seg, Shape};
pub use mvmap::{MvMap, MvMapDelta};
pub use ormap::{OrMap, OrMapDelta};
pub use orset::{OrSet, OrSetDelta};
pub use register::{LwwDelta, LwwRegister};
pub use rga::{Element, Rga, RgaDelta};
pub use richtext::{RichText, RichTextDelta, Span};
pub use tree::{NodeId, Tree, TreeDelta, TreeNode};
pub use version_vector::VersionVector;

/// A CRDT (conflict-free replicated data type): shared data that several
/// machines can each change on their own and combine later, with no machine in
/// charge.
///
/// When you change a value you get back a "delta", a small record of just that
/// change. Send the delta to the other machines and they fold it in with
/// `merge`.
///
/// `merge` must give the same result whatever order deltas arrive in, however
/// they are grouped, and even if the same delta arrives twice. That is what
/// lets every machine end up with the same value. Every type that implements
/// this trait has to guarantee it.
///
/// `snapshot` returns one delta that holds the whole current state. Merging it
/// into an empty value gives a copy of this one. Use it the first time two
/// machines sync, or to save the state.
///
/// ```
/// use orvu_crdt::{DeltaCrdt, PnCounter, ReplicaId};
/// let mut a = PnCounter::default();
/// let d = a.increment(ReplicaId(1), 5);
/// let mut b = PnCounter::default();
/// b.merge(d);
/// assert_eq!(a.value(), b.value());
/// ```
pub trait DeltaCrdt {
    /// The small record of a change that this type sends to other machines.
    type Delta;
    /// Folds a delta into this value.
    fn merge(&mut self, delta: Self::Delta);
    /// Returns the whole current state as a single delta.
    fn snapshot(&self) -> Self::Delta;
}

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
