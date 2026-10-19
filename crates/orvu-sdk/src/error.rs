//! The errors this module returns.

use thiserror::Error;

/// Everything that can go wrong when using a `Client` or a handle.
#[derive(Debug, Error)]
pub enum SdkError {
    /// The id already names an object of a different type.
    #[error("object {id} is not a {expected}")]
    TypeMismatch { id: String, expected: &'static str },
    /// The node could not carry out the request, for example because it is no
    /// longer running.
    #[error("engine unavailable")]
    Engine,
    /// The node can no longer save to disk, so it takes no more changes. A
    /// change it could not save is not reported as done. Restart it once the
    /// disk is fixed, for example once space has been freed. Also returned
    /// when starting a node whose storage file cannot be opened or read.
    #[error("cannot save changes: {0}")]
    Storage(String),
    /// A bounded-counter operation was refused, because this machine's
    /// allowance was too small and none could be borrowed. A counter that must
    /// never go below zero has to refuse sometimes, and this is where the
    /// caller sees it.
    #[error("bounded counter exhausted: no local allowance")]
    Exhausted,
    /// A rich-text mark was given an empty value. A mark is removed with
    /// `unformat`, never by setting it to nothing.
    #[error("a mark needs a value; use unformat to remove one")]
    EmptyValue,
    /// A list index was past the end of the visible list.
    #[error("index {index} out of bounds (len {len})")]
    OutOfBounds { index: usize, len: usize },
    /// A write was refused and changed nothing: a list insert named an item
    /// that is not in the list, this machine has no change numbers left for
    /// the object, so it can only read it, or the object already has changes
    /// from the most machines it takes and none from this one (see
    /// `orvu_crdt::WriteError`). The message says which.
    #[error("write refused: {0}")]
    Write(#[from] orvu_crdt::WriteError),
    /// A JSON path could not be used for the write.
    #[error("json path error: {0}")]
    Json(#[from] orvu_crdt::JsonError),
    /// The network layer failed.
    #[error("transport error: {0}")]
    Transport(#[from] orvu_transport::TransportError),
    /// The config could not be read or is not valid.
    #[error("config error: {0}")]
    Config(String),
    /// The id is already open under a different consistency class. A plain
    /// counter and a bounded counter are different objects with different
    /// rules, so one id cannot name both.
    #[error("object {id} is already open as {declared:?}")]
    ClassMismatch { id: String, declared: crate::ConsistencyClass },
    /// A write made only if a check of the object as it was at that moment
    /// allowed it was not allowed, and changed nothing. The message says why.
    #[error("{0}")]
    NotAllowed(String),
}

impl From<orvu_runtime::SpawnError> for SdkError {
    fn from(e: orvu_runtime::SpawnError) -> Self {
        match e {
            orvu_runtime::SpawnError::Storage(why) => SdkError::Storage(why),
            orvu_runtime::SpawnError::Transport(e) => SdkError::Transport(e),
        }
    }
}

/// A `Result` whose error is `SdkError`.
pub type Result<T> = std::result::Result<T, SdkError>;
