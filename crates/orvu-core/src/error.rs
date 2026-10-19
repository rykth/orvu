//! The errors this module can return.

use orvu_crdt::ReplicaId;
use orvu_proto::v1::CrdtType;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("delta is {delta:?} but object holds {object:?}")]
    TypeMismatch { delta: CrdtType, object: CrdtType },

    #[error("unknown CRDT type on the wire: {0}")]
    UnknownCrdtType(i32),

    #[error("malformed payload: {0}")]
    MalformedPayload(String),

    /// A change made here was refused, and nothing was changed.
    #[error("write refused: {0}")]
    Write(#[from] orvu_crdt::WriteError),

    #[error("missing required field: {0}")]
    MissingField(&'static str),

    #[error("causal buffer full at {0} deltas; peer must be repaired by anti-entropy")]
    CausalBufferFull(usize),

    /// `writer` is the machine whose timestamp it is. That machine may only
    /// have passed on a time it took in from another machine's fast clock.
    #[error("timestamp written by machine {} is {lead_ms} ms ahead of this machine's clock", writer.0)]
    TooFarAhead { lead_ms: u64, writer: ReplicaId },
}

pub type Result<T> = std::result::Result<T, Error>;
