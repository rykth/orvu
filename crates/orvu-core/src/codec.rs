//! Turning messages into bytes and back.
//!
//! The engine works with `SyncMessage` values. A `Codec` decides what bytes
//! they become, and it is a trait so that the encoding can be swapped.
//! `ProtobufCodec` is the standard one. It writes each message with its length
//! in front, as a protobuf varint. `decode` takes the bytes of exactly one
//! message and refuses anything after it, so two messages stuck together are
//! an error and not one message with the second lost. Splitting a stream into
//! messages is the transport's job (`orvu-transport`'s `frame`).

use crate::error::{Error, Result};
use crate::object::{SnapshotEntry, WireDelta};
use crate::wire;
use orvu_crdt::{DotContext, ReplicaId};
use orvu_proto::v1 as pb;
use prost::Message;

/// A message one machine sends another.
#[derive(Clone, Debug, PartialEq)]
pub enum SyncMessage {
    Hello {
        proto_version: u32,
        replica: ReplicaId,
        namespaces: Vec<String>,
    },
    HelloAck {
        proto_version: u32,
        replica: ReplicaId,
        accepted: Vec<String>,
    },
    DeltaBatch {
        namespace: String,
        deltas: Vec<WireDelta>,
    },
    Ack {
        namespace: String,
        observed: DotContext,
    },
    /// Says that something was refused, and why. `object_id` and `dot`
    /// name the refused change when there is one; otherwise they are empty
    /// and `None`.
    Nack {
        reason: NackReason,
        detail: String,
        object_id: String,
        dot: Option<orvu_crdt::Dot>,
    },
    ContextDigest {
        namespace: String,
        entries: std::collections::BTreeMap<String, orvu_crdt::DotContext>,
    },
    SnapshotBatch {
        namespace: String,
        objects: Vec<SnapshotEntry>,
    },
    MembershipDigest {
        members: Vec<MemberInfo>,
    },
    BorrowRequest {
        namespace: String,
        object_id: String,
        amount: u64,
        from: ReplicaId,
    },
    /// Says only "still here". Sent on a connection that has had nothing
    /// else to send for a while.
    KeepAlive,
}

/// Why a machine refused something. A newer machine may send a reason this
/// version has no name for. It is kept as the number it came as, so it is
/// never mistaken for "unspecified" and goes back out unchanged. An error
/// would be worse: a whole message from a newer machine would look broken
/// for a field that is only there to explain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NackReason {
    Known(pb::NackReason),
    Unknown(i32),
}

impl NackReason {
    fn from_wire(n: i32) -> Self {
        match pb::NackReason::try_from(n) {
            Ok(known) => NackReason::Known(known),
            Err(_) => NackReason::Unknown(n),
        }
    }

    fn to_wire(self) -> i32 {
        match self {
            NackReason::Known(known) => known as i32,
            NackReason::Unknown(n) => n,
        }
    }
}

/// What is known about one machine in the group: its heartbeat, a number it
/// keeps raising to show it is alive, and what it has seen for each object.
/// The second part has the same shape as in `ContextDigest`.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberInfo {
    pub replica: ReplicaId,
    pub heartbeat: u64,
    pub contexts: Vec<(String, DotContext)>,
}

pub trait Codec: Send + Sync {
    fn encode(&self, message: &SyncMessage) -> Vec<u8>;
    fn decode(&self, bytes: &[u8]) -> Result<SyncMessage>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProtobufCodec;

fn encode_delta(d: &WireDelta) -> pb::Delta {
    pb::Delta {
        object_id: d.object_id.clone(),
        crdt_type: d.delta.crdt_type() as i32,
        dot: Some(wire::encode_dot(d.dot)),
        hlc: Some(wire::encode_hlc(d.hlc)),
        deps: Some(wire::encode_context(&d.deps)),
        payload: wire::encode_delta_payload(&d.delta),
        trace_id: if d.trace_id.is_none() { Vec::new() } else { d.trace_id.as_bytes().to_vec() },
    }
}

fn decode_delta(d: pb::Delta) -> Result<WireDelta> {
    let kind = pb::CrdtType::try_from(d.crdt_type)
        .map_err(|_| Error::UnknownCrdtType(d.crdt_type))?;
    let dot = wire::decode_dot(&d.dot.ok_or(Error::MissingField("Delta.dot"))?)?;
    let hlc = wire::decode_hlc(&d.hlc.ok_or(Error::MissingField("Delta.hlc"))?);
    let deps = wire::decode_context(&d.deps.ok_or(Error::MissingField("Delta.deps"))?)?;
    Ok(WireDelta {
        object_id: d.object_id,
        dot,
        hlc,
        deps,
        delta: wire::decode_delta_payload(kind, &d.payload)?,
        trace_id: crate::trace::TraceId::from_bytes(&d.trace_id),
    })
}

/// Encodes one delta as bytes that can be stored on disk and read back alone.
/// It is the same `Delta` message that is sent between machines, without the
/// `SyncMessage` wrapper. So a delta on disk and a delta on the network are
/// always encoded the same way.
pub fn encode_wire_delta(delta: &WireDelta) -> Vec<u8> {
    encode_delta(delta).encode_to_vec()
}

/// Reads back bytes written by [`encode_wire_delta`].
pub fn decode_wire_delta(bytes: &[u8]) -> Result<WireDelta> {
    let d = pb::Delta::decode(bytes).map_err(|e| Error::MalformedPayload(e.to_string()))?;
    decode_delta(d)
}

/// Builds the message for one snapshot entry. It is not wrapped and not yet
/// turned into bytes.
fn snapshot_object(entry: &SnapshotEntry) -> pb::SnapshotObject {
    pb::SnapshotObject {
        object_id: entry.object_id.clone(),
        crdt_type: entry.state.crdt_type() as i32,
        payload: wire::encode_delta_payload(&entry.state),
        context: Some(wire::encode_context(&entry.context)),
    }
}

/// Reads one snapshot entry out of its message.
fn snapshot_entry_from(s: pb::SnapshotObject) -> Result<SnapshotEntry> {
    let kind = pb::CrdtType::try_from(s.crdt_type).map_err(|_| Error::UnknownCrdtType(s.crdt_type))?;
    let state = wire::decode_delta_payload(kind, &s.payload)?;
    let context = wire::decode_context(
        s.context.as_ref().ok_or(Error::MissingField("SnapshotObject.context"))?,
    )?;
    Ok(SnapshotEntry { object_id: s.object_id, state, context })
}

/// Encodes one snapshot entry as bytes that can be stored on disk and read
/// back alone.
pub fn encode_snapshot_entry(entry: &SnapshotEntry) -> Vec<u8> {
    snapshot_object(entry).encode_to_vec()
}

/// Reads back bytes written by [`encode_snapshot_entry`].
pub fn decode_snapshot_entry(bytes: &[u8]) -> Result<SnapshotEntry> {
    let s = pb::SnapshotObject::decode(bytes).map_err(|e| Error::MalformedPayload(e.to_string()))?;
    snapshot_entry_from(s)
}

/// A `Nack` for a `DeltaBatch` that `decode` refused because a change in it
/// cannot be read: an unknown data type, or a payload that is not what its
/// type says. It names the first such change. `None` when the bytes are not
/// such a batch, so a message of a kind this version does not know, which is
/// skipped on purpose, gets no Nack.
pub fn nack_for_unreadable(bytes: &[u8]) -> Option<SyncMessage> {
    let mut cursor = bytes;
    let envelope = pb::SyncMessage::decode_length_delimited(&mut cursor).ok()?;
    let Some(pb::sync_message::Payload::DeltaBatch(batch)) = envelope.payload else {
        return None;
    };
    batch.deltas.into_iter().find_map(|d| {
        let object_id = d.object_id.clone();
        let dot = d.dot.as_ref().and_then(|dot| wire::decode_dot(dot).ok());
        let error = decode_delta(d).err()?;
        let reason = match error {
            Error::UnknownCrdtType(_) => pb::NackReason::UnknownCrdtType,
            _ => pb::NackReason::MalformedPayload,
        };
        Some(SyncMessage::Nack {
            reason: NackReason::Known(reason),
            detail: format!("the whole batch was dropped because this change cannot be read: {error}"),
            object_id,
            dot,
        })
    })
}

impl Codec for ProtobufCodec {
    fn encode(&self, message: &SyncMessage) -> Vec<u8> {
        let payload = match message {
            SyncMessage::Hello {
                proto_version,
                replica,
                namespaces,
            } => pb::sync_message::Payload::Hello(pb::Hello {
                proto_version: *proto_version,
                replica: replica.0,
                namespaces: namespaces.clone(),
            }),

            SyncMessage::HelloAck {
                proto_version,
                replica,
                accepted,
            } => pb::sync_message::Payload::HelloAck(pb::HelloAck {
                proto_version: *proto_version,
                replica: replica.0,
                accepted_namespaces: accepted.clone(),
            }),

            SyncMessage::DeltaBatch { namespace, deltas } => {
                pb::sync_message::Payload::DeltaBatch(pb::DeltaBatch {
                    namespace: namespace.clone(),
                    deltas: deltas.iter().map(encode_delta).collect(),
                })
            }

            SyncMessage::Ack {
                namespace,
                observed,
            } => pb::sync_message::Payload::Ack(pb::Ack {
                namespace: namespace.clone(),
                observed: Some(wire::encode_context(observed)),
            }),

            SyncMessage::Nack { reason, detail, object_id, dot } => {
                pb::sync_message::Payload::Nack(pb::Nack {
                    reason: reason.to_wire(),
                    detail: detail.clone(),
                    object_id: object_id.clone(),
                    dot: dot.map(wire::encode_dot),
                })
            }

            SyncMessage::ContextDigest { namespace, entries } => {
                pb::sync_message::Payload::ContextDigest(pb::ContextDigest {
                    namespace: namespace.clone(),
                    entries: entries
                        .iter()
                        .map(|(k, ctx)| (k.clone(), wire::encode_context(ctx)))
                        .collect(),
                })
            }

            SyncMessage::SnapshotBatch { namespace, objects } => {
                pb::sync_message::Payload::SnapshotBatch(pb::SnapshotBatch {
                    namespace: namespace.clone(),
                    objects: objects.iter().map(snapshot_object).collect(),
                })
            }

            SyncMessage::MembershipDigest { members } => {
                pb::sync_message::Payload::MembershipDigest(pb::MembershipDigest {
                    members: members
                        .iter()
                        .map(|m| pb::MemberInfo {
                            replica: m.replica.0,
                            heartbeat: m.heartbeat,
                            contexts: m
                                .contexts
                                .iter()
                                .map(|(k, ctx)| (k.clone(), wire::encode_context(ctx)))
                                .collect(),
                        })
                        .collect(),
                })
            }

            SyncMessage::BorrowRequest { namespace, object_id, amount, from } => {
                pb::sync_message::Payload::BorrowRequest(pb::BorrowRequest {
                    namespace: namespace.clone(),
                    object_id: object_id.clone(),
                    amount: *amount,
                    from_replica: from.0,
                })
            }

            SyncMessage::KeepAlive => pb::sync_message::Payload::KeepAlive(pb::KeepAlive {}),
        };

        let mut buf = Vec::new();
        pb::SyncMessage {
            payload: Some(payload),
        }
        .encode_length_delimited(&mut buf)
        .expect("encoding into a Vec cannot fail");
        buf
    }

    fn decode(&self, bytes: &[u8]) -> Result<SyncMessage> {
        let mut cursor = bytes;
        let envelope = pb::SyncMessage::decode_length_delimited(&mut cursor)
            .map_err(|e| Error::MalformedPayload(e.to_string()))?;
        // The bytes hold one message and nothing more. Anything after it
        // would be a second message dropped without a word.
        if !cursor.is_empty() {
            return Err(Error::MalformedPayload(format!(
                "{} bytes after the message",
                cursor.len()
            )));
        }

        match envelope
            .payload
            .ok_or(Error::MissingField("SyncMessage.payload"))?
        {
            pb::sync_message::Payload::Hello(h) => Ok(SyncMessage::Hello {
                proto_version: h.proto_version,
                replica: ReplicaId(h.replica),
                namespaces: h.namespaces,
            }),

            pb::sync_message::Payload::HelloAck(h) => Ok(SyncMessage::HelloAck {
                proto_version: h.proto_version,
                replica: ReplicaId(h.replica),
                accepted: h.accepted_namespaces,
            }),

            pb::sync_message::Payload::DeltaBatch(b) => Ok(SyncMessage::DeltaBatch {
                namespace: b.namespace,
                deltas: b
                    .deltas
                    .into_iter()
                    .map(decode_delta)
                    .collect::<Result<Vec<_>>>()?,
            }),

            pb::sync_message::Payload::Ack(a) => Ok(SyncMessage::Ack {
                namespace: a.namespace,
                observed: wire::decode_context(
                    &a.observed.ok_or(Error::MissingField("Ack.observed"))?,
                )?,
            }),

            pb::sync_message::Payload::Nack(n) => Ok(SyncMessage::Nack {
                reason: NackReason::from_wire(n.reason),
                detail: n.detail,
                object_id: n.object_id,
                dot: n.dot.as_ref().map(wire::decode_dot).transpose()?,
            }),

            pb::sync_message::Payload::ContextDigest(d) => Ok(SyncMessage::ContextDigest {
                namespace: d.namespace,
                entries: d
                    .entries
                    .into_iter()
                    .map(|(k, ctx)| Ok((k, wire::decode_context(&ctx)?)))
                    .collect::<Result<_>>()?,
            }),

            pb::sync_message::Payload::SnapshotBatch(b) => Ok(SyncMessage::SnapshotBatch {
                namespace: b.namespace,
                objects: b
                    .objects
                    .into_iter()
                    .map(snapshot_entry_from)
                    .collect::<Result<Vec<_>>>()?,
            }),

            pb::sync_message::Payload::MembershipDigest(d) => Ok(SyncMessage::MembershipDigest {
                members: d
                    .members
                    .into_iter()
                    .map(|m| {
                        Ok(MemberInfo {
                            replica: ReplicaId(m.replica),
                            heartbeat: m.heartbeat,
                            contexts: m
                                .contexts
                                .into_iter()
                                .map(|(k, ctx)| Ok((k, wire::decode_context(&ctx)?)))
                                .collect::<Result<_>>()?,
                        })
                    })
                    .collect::<Result<_>>()?,
            }),

            pb::sync_message::Payload::BorrowRequest(b) => Ok(SyncMessage::BorrowRequest {
                namespace: b.namespace,
                object_id: b.object_id,
                amount: b.amount,
                from: ReplicaId(b.from_replica),
            }),

            pb::sync_message::Payload::KeepAlive(_) => Ok(SyncMessage::KeepAlive),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Object, TypedCrdt, TypedDelta};
    use orvu_crdt::{Hlc, OrSet};

    fn sample_delta() -> WireDelta {
        let mut o = Object::new("set-1", TypedCrdt::OrSet(OrSet::default()));
        o.mutate(ReplicaId(4), Hlc { wall: 9, logical: 1 }, |c, _hlc, _replica| match c {
            TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.add("apple".to_string(), ReplicaId(4)).unwrap()),
            other => panic!("unexpected {other:?}"),
        }).unwrap()
    }

    fn round_trip(message: SyncMessage) {
        let codec = ProtobufCodec;
        let bytes = codec.encode(&message);
        assert_eq!(codec.decode(&bytes).unwrap(), message);
    }

    #[test]
    fn hello_round_trips() {
        round_trip(SyncMessage::Hello {
            proto_version: orvu_proto::PROTO_VERSION,
            replica: ReplicaId(1),
            namespaces: vec!["default".to_string()],
        });
    }

    #[test]
    fn delta_batch_round_trips() {
        round_trip(SyncMessage::DeltaBatch {
            namespace: "default".to_string(),
            deltas: vec![sample_delta()],
        });
    }

    #[test]
    fn ack_round_trips() {
        let mut ctx = DotContext::new();
        ctx.insert(orvu_crdt::Dot::new(ReplicaId(1), 1));
        round_trip(SyncMessage::Ack {
            namespace: "default".to_string(),
            observed: ctx,
        });
    }

    #[test]
    fn keep_alive_round_trips() {
        round_trip(SyncMessage::KeepAlive);
    }

    // A machine of an older version gets messages of kinds it does not know,
    // such as a keep-alive from a newer one. It must come out as an error to
    // skip, not as some other message.
    #[test]
    fn a_message_of_an_unknown_kind_is_an_error() {
        // Field 15 of the wrapper, holding an empty message, with the length
        // in front.
        let bytes = [2u8, (15 << 3) | 2, 0];
        assert!(matches!(ProtobufCodec.decode(&bytes), Err(Error::MissingField(_))));
    }

    #[test]
    fn nack_round_trips() {
        round_trip(SyncMessage::Nack {
            reason: NackReason::Known(pb::NackReason::UnknownCrdtType),
            detail: "type 99".to_string(),
            object_id: String::new(),
            dot: None,
        });
    }

    // A Nack about one change names its object and its dot, so the sender
    // can tell which change of a batch was refused.
    #[test]
    fn a_nack_names_the_refused_change() {
        round_trip(SyncMessage::Nack {
            reason: NackReason::Known(pb::NackReason::MalformedPayload),
            detail: "skips ahead".to_string(),
            object_id: "doc-7".to_string(),
            dot: Some(orvu_crdt::Dot::new(ReplicaId(4), 12)),
        });
        round_trip(SyncMessage::Nack {
            reason: NackReason::Known(pb::NackReason::ClockAhead),
            detail: "ten minutes ahead".to_string(),
            object_id: "doc-7".to_string(),
            dot: Some(orvu_crdt::Dot::new(ReplicaId(4), 13)),
        });
    }

    // One change in a batch that cannot be read makes the whole batch
    // unreadable. The Nack for it names that change, not just the batch.
    #[test]
    fn the_unreadable_change_of_a_batch_is_named() {
        let good = encode_delta(&sample_delta());
        let mut bad = encode_delta(&sample_delta());
        bad.object_id = "broken".into();
        bad.dot = Some(pb::Dot { replica: 5, counter: 9 });
        bad.payload = vec![0xff, 0xff, 0xff];
        let mut unknown = encode_delta(&sample_delta());
        unknown.object_id = "from-the-future".into();
        unknown.crdt_type = 99;
        let batch = |deltas: Vec<pb::Delta>| {
            let mut bytes = Vec::new();
            pb::SyncMessage {
                payload: Some(pb::sync_message::Payload::DeltaBatch(pb::DeltaBatch {
                    namespace: "default".into(),
                    deltas,
                })),
            }
            .encode_length_delimited(&mut bytes)
            .unwrap();
            bytes
        };

        let bytes = batch(vec![good.clone(), bad.clone(), good.clone()]);
        assert!(ProtobufCodec.decode(&bytes).is_err());
        match nack_for_unreadable(&bytes) {
            Some(SyncMessage::Nack { reason, object_id, dot, .. }) => {
                assert_eq!(reason, NackReason::Known(pb::NackReason::MalformedPayload));
                assert_eq!(object_id, "broken");
                assert_eq!(dot, Some(orvu_crdt::Dot::new(ReplicaId(5), 9)));
            }
            other => panic!("expected a Nack, got {other:?}"),
        }

        let bytes = batch(vec![good.clone(), unknown]);
        match nack_for_unreadable(&bytes) {
            Some(SyncMessage::Nack { reason, object_id, .. }) => {
                assert_eq!(reason, NackReason::Known(pb::NackReason::UnknownCrdtType));
                assert_eq!(object_id, "from-the-future");
            }
            other => panic!("expected a Nack, got {other:?}"),
        }

        // A batch that reads fine, and a message of a kind this version does
        // not know, get no Nack: the second is skipped on purpose.
        assert!(nack_for_unreadable(&batch(vec![good])).is_none());
        assert!(nack_for_unreadable(&[2u8, (15 << 3) | 2, 0]).is_none());
    }

    #[test]
    fn context_digest_round_trips() {
        let mut ctx = DotContext::new();
        ctx.insert(orvu_crdt::Dot::new(ReplicaId(1), 3));
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("obj".to_string(), ctx);
        round_trip(SyncMessage::ContextDigest {
            namespace: "default".to_string(),
            entries,
        });
    }

    #[test]
    fn membership_digest_round_trips() {
        let mut ctx = orvu_crdt::DotContext::new();
        ctx.insert(orvu_crdt::Dot::new(orvu_crdt::ReplicaId(3), 2));
        round_trip(SyncMessage::MembershipDigest {
            members: vec![MemberInfo {
                replica: orvu_crdt::ReplicaId(3),
                heartbeat: 7,
                contexts: vec![("o".to_string(), ctx)],
            }],
        });
    }

    #[test]
    fn borrow_request_round_trips() {
        round_trip(SyncMessage::BorrowRequest {
            namespace: "default".to_string(),
            object_id: "sku-42".to_string(),
            amount: 5,
            from: ReplicaId(2),
        });
    }

    // Two messages stuck together are not one message. Reading the first and
    // dropping the second would lose it without a word.
    #[test]
    fn decode_refuses_bytes_after_the_message() {
        let codec = ProtobufCodec;
        let one = codec.encode(&SyncMessage::Hello {
            proto_version: orvu_proto::PROTO_VERSION,
            replica: ReplicaId(1),
            namespaces: vec!["default".to_string()],
        });
        let mut two = one.clone();
        two.extend_from_slice(&one);
        assert!(matches!(codec.decode(&two), Err(Error::MalformedPayload(_))));
        let mut one_more = one.clone();
        one_more.push(0);
        assert!(matches!(codec.decode(&one_more), Err(Error::MalformedPayload(_))));
        assert!(codec.decode(&one).is_ok());
    }

    // A newer machine may send a reason this one does not know. It is kept
    // as the number it came as, not passed off as "unspecified".
    #[test]
    fn an_unknown_nack_reason_is_kept_as_unknown() {
        let mut bytes = Vec::new();
        pb::SyncMessage {
            payload: Some(pb::sync_message::Payload::Nack(pb::Nack { reason: 99, detail: "new".into(), ..Default::default() })),
        }
        .encode_length_delimited(&mut bytes)
        .unwrap();
        assert_eq!(
            ProtobufCodec.decode(&bytes).unwrap(),
            SyncMessage::Nack { reason: NackReason::Unknown(99), detail: "new".into(), object_id: String::new(), dot: None }
        );
        // And it goes back out as the same number.
        assert_eq!(ProtobufCodec.encode(&ProtobufCodec.decode(&bytes).unwrap()), bytes);
        round_trip(SyncMessage::Nack {
            reason: NackReason::Known(pb::NackReason::Unspecified),
            detail: String::new(),
            object_id: String::new(),
            dot: None,
        });
    }

    #[test]
    fn decode_rejects_garbage() {
        let err = ProtobufCodec.decode(&[0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(err, Err(Error::MalformedPayload(_))), "got {err:?}");
    }

    #[test]
    fn snapshot_entry_round_trips_each_type() {
        use orvu_crdt::{Dot, DotContext, Hlc, LwwRegister, OrSet, PnCounter, ReplicaId};
        use crate::object::{SnapshotEntry, TypedCrdt};

        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 1));
        ctx.insert(Dot::new(ReplicaId(2), 3)); // gap -> cloud, exercises context

        let mut c = PnCounter::default();
        c.increment(ReplicaId(1), 7);
        let mut s = OrSet::<String>::default();
        s.add("x".to_string(), ReplicaId(1)).unwrap();
        let mut r = LwwRegister::<String>::default();
        r.set("v".to_string(), Hlc { wall: 2, logical: 1 }, ReplicaId(1));

        for state in [
            TypedCrdt::PnCounter(c).snapshot(),
            TypedCrdt::OrSet(s).snapshot(),
            TypedCrdt::LwwRegister(r).snapshot(),
        ] {
            let entry = SnapshotEntry { object_id: "o".into(), state, context: ctx.clone() };
            let bytes = encode_snapshot_entry(&entry);
            assert_eq!(decode_snapshot_entry(&bytes).unwrap(), entry);
        }
    }

    #[test]
    fn wire_delta_round_trips_through_bytes() {
        use orvu_crdt::{Dot, DotContext, Hlc, PnCounter, ReplicaId};
        let mut c = PnCounter::default();
        let delta = WireDelta {
            object_id: "o".into(),
            dot: Dot::new(ReplicaId(1), 1),
            hlc: Hlc::ZERO,
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(c.increment(ReplicaId(1), 3)),
            trace_id: crate::trace::TraceId::none(),
        };
        let bytes = encode_wire_delta(&delta);
        assert_eq!(decode_wire_delta(&bytes).unwrap(), delta);
    }

    #[test]
    fn trace_id_round_trips_through_the_codec() {
        use crate::trace::TraceId;
        let mut d = sample_delta();
        d.trace_id = TraceId::from_array([7u8; 16]);
        let back = decode_wire_delta(&encode_wire_delta(&d)).unwrap();
        assert_eq!(back.trace_id, d.trace_id, "a non-zero trace_id survives the round trip");

        let mut untraced = sample_delta();
        untraced.trace_id = TraceId::none();
        let back2 = decode_wire_delta(&encode_wire_delta(&untraced)).unwrap();
        assert!(back2.trace_id.is_none(), "an untraced delta decodes to none()");
    }

    #[test]
    fn snapshot_batch_round_trips() {
        use orvu_crdt::{Dot, DotContext, OrSet, PnCounter, ReplicaId};
        use crate::object::{SnapshotEntry, TypedCrdt};

        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 1));
        ctx.insert(Dot::new(ReplicaId(2), 4)); // gap -> cloud, exercises context

        let mut c = PnCounter::default();
        c.increment(ReplicaId(1), 7);
        let mut s = OrSet::<String>::default();
        s.add("x".to_string(), ReplicaId(1)).unwrap();
        s.add("y".to_string(), ReplicaId(1)).unwrap();
        s.remove(&"x".to_string()); // exercise an add-wins removal in the snapshot

        round_trip(SyncMessage::SnapshotBatch {
            namespace: "default".to_string(),
            objects: vec![
                SnapshotEntry { object_id: "c".into(), state: TypedCrdt::PnCounter(c).snapshot(), context: ctx.clone() },
                SnapshotEntry { object_id: "s".into(), state: TypedCrdt::OrSet(s).snapshot(), context: ctx.clone() },
            ],
        });
    }
}
