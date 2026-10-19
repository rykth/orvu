//! Why a write made on this machine can be refused.

use crate::id::Dot;

/// Why a write was refused. A refused write changes nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteError {
    /// This machine has used every dot counter up to `MAX_COUNTER`, so there
    /// is no fresh dot to give the write. No machine gets there by writing:
    /// only a broken or hostile sender, handing this machine a dot with its
    /// own id and a counter at the cap, can bring it about. Giving the write
    /// a dot that is already used would lose it, or another write, quietly.
    NoFreshDot,
    /// The object already holds changes from the most machines it takes in
    /// from others, and none from this one. Every other machine would refuse
    /// a change from a new writer, so it is refused here too, and the copies
    /// never drift apart. The machine can still read the object and take in
    /// the changes of the machines writing to it.
    TooManyWriters,
    /// A list insert named an item to go after that is not in the list. An
    /// item that was deleted is still in the list and is fine to name.
    UnknownPredecessor(Dot),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::NoFreshDot => {
                write!(f, "this object has no change numbers left for this machine; it can only be read")
            }
            WriteError::TooManyWriters => write!(
                f,
                "this object already has changes from the most machines it takes, and none from this one; \
                 it can only be read here"
            ),
            WriteError::UnknownPredecessor(d) => {
                write!(f, "the list has no item ({}, {}) to insert after", d.replica.0, d.counter)
            }
        }
    }
}

impl std::error::Error for WriteError {}
