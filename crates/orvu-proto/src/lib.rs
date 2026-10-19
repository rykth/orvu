//! `orvu-proto`: the messages Orvu machines send each other.
//!
//! The `.proto` files in this module's `proto` folder define the messages,
//! and every programming language works from those same files. This module is the Rust
//! version of them. `prost-build` generates it each time the module is built.
//!
//! This module does not depend on `orvu-crdt`. It holds the message types and
//! nothing else. It does not turn a message into one of the engine's data
//! types or back. Leaving that out keeps the message formats usable from any
//! language.

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/orvu.v1.rs"));
}

pub use prost::Message;

/// The protocol version a machine announces when it connects.
///
/// A change that only adds things keeps this number. A change that breaks
/// older machines raises it. Version 2 changed how a JSON document's changes
/// are sent, and what they mean: a machine on version 1 reads them wrong.
/// Machines on different numbers refuse to sync with each other.
pub const PROTO_VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use super::v1::*;
    use prost::Message;

    #[test]
    fn dot_round_trips() {
        let d = Dot {
            replica: 7,
            counter: 42,
        };
        let bytes = d.encode_to_vec();
        assert_eq!(Dot::decode(&bytes[..]).unwrap(), d);
    }

    #[test]
    fn dot_context_round_trips() {
        let ctx = DotContext {
            prefix: Some(VersionVector {
                prefix: [(1u64, 3u64), (2u64, 9u64)].into_iter().collect(),
            }),
            cloud: vec![Dot {
                replica: 2,
                counter: 11,
            }],
        };
        let bytes = ctx.encode_to_vec();
        assert_eq!(DotContext::decode(&bytes[..]).unwrap(), ctx);
    }

    #[test]
    fn map_encoding_is_deterministic() {
        let a = VersionVector {
            prefix: [(1u64, 1u64), (2, 2), (3, 3)].into_iter().collect(),
        };
        let b = VersionVector {
            prefix: [(3u64, 3u64), (2, 2), (1, 1)].into_iter().collect(),
        };
        assert_eq!(a.encode_to_vec(), b.encode_to_vec());
    }

    #[test]
    fn pn_counter_delta_round_trips() {
        let d = PnCounterDelta {
            inc: [(1u64, 10u64)].into_iter().collect(),
            dec: [(2u64, 4u64)].into_iter().collect(),
        };
        let bytes = d.encode_to_vec();
        assert_eq!(PnCounterDelta::decode(&bytes[..]).unwrap(), d);
    }

    #[test]
    fn lww_register_delta_round_trips_including_tombstone() {
        let set = LwwRegisterDelta {
            value: Some("hi".to_string()),
            ts: Some(Hlc {
                wall: 5,
                logical: 1,
            }),
            writer: 3,
        };
        assert_eq!(
            LwwRegisterDelta::decode(&set.encode_to_vec()[..]).unwrap(),
            set
        );

        let tombstone = LwwRegisterDelta {
            value: None,
            ts: Some(Hlc {
                wall: 6,
                logical: 0,
            }),
            writer: 3,
        };
        assert_eq!(
            LwwRegisterDelta::decode(&tombstone.encode_to_vec()[..]).unwrap(),
            tombstone
        );
    }

    #[test]
    fn or_set_delta_round_trips() {
        let d = OrSetDelta {
            elements: [(
                "apple".to_string(),
                DotSet {
                    dots: vec![Dot {
                        replica: 1,
                        counter: 1,
                    }],
                },
            )]
            .into_iter()
            .collect(),
            ctx: Some(DotContext {
                prefix: Some(VersionVector {
                    prefix: [(1u64, 1u64)].into_iter().collect(),
                }),
                cloud: vec![],
            }),
        };
        let bytes = d.encode_to_vec();
        assert_eq!(OrSetDelta::decode(&bytes[..]).unwrap(), d);
    }

    #[test]
    fn crdt_type_maps_from_wire_values() {
        assert_eq!(CrdtType::try_from(1), Ok(CrdtType::PnCounter));
        assert_eq!(CrdtType::try_from(3), Ok(CrdtType::OrSet));
        assert!(CrdtType::try_from(99).is_err());
    }

    #[test]
    fn sync_message_envelope_round_trips_each_variant() {
        let hello = SyncMessage {
            payload: Some(sync_message::Payload::Hello(Hello {
                proto_version: super::PROTO_VERSION,
                replica: 1,
                namespaces: vec!["default".to_string()],
            })),
        };
        assert_eq!(
            SyncMessage::decode(&hello.encode_to_vec()[..]).unwrap(),
            hello
        );

        let nack = SyncMessage {
            payload: Some(sync_message::Payload::Nack(Nack {
                reason: NackReason::UnknownCrdtType as i32,
                detail: "type 99".to_string(),
                object_id: "o".to_string(),
                dot: Some(Dot { replica: 2, counter: 5 }),
            })),
        };
        assert_eq!(
            SyncMessage::decode(&nack.encode_to_vec()[..]).unwrap(),
            nack
        );
    }

    #[test]
    fn delta_batch_round_trips() {
        let batch = SyncMessage {
            payload: Some(sync_message::Payload::DeltaBatch(DeltaBatch {
                namespace: "default".to_string(),
                deltas: vec![Delta {
                    object_id: "counter-1".to_string(),
                    crdt_type: CrdtType::PnCounter as i32,
                    dot: Some(Dot {
                        replica: 1,
                        counter: 1,
                    }),
                    hlc: Some(Hlc {
                        wall: 10,
                        logical: 0,
                    }),
                    deps: Some(DotContext::default()),
                    payload: vec![1, 2, 3],
                    trace_id: Vec::new(),
                }],
            })),
        };
        assert_eq!(
            SyncMessage::decode(&batch.encode_to_vec()[..]).unwrap(),
            batch
        );
    }

    #[test]
    fn context_digest_round_trips() {
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "obj".to_string(),
            DotContext {
                prefix: None,
                cloud: vec![],
            },
        );
        let msg = ContextDigest {
            namespace: "default".to_string(),
            entries,
        };
        let bytes = msg.encode_to_vec();
        assert_eq!(ContextDigest::decode(&bytes[..]).unwrap(), msg);
    }

    // The asking machine is `from_replica`, not `from`, which Python cannot
    // use as a name. The number stays 4, so the bytes are the same as before.
    #[test]
    fn a_borrow_request_names_the_asker_under_field_4() {
        let msg = BorrowRequest {
            namespace: String::new(),
            object_id: String::new(),
            amount: 0,
            from_replica: 9,
        };
        // Field 4 as a varint: tag (4 << 3) = 0x20, then the value 9.
        assert_eq!(msg.encode_to_vec(), vec![0x20, 9]);
        assert_eq!(BorrowRequest::decode(&[0x20u8, 9][..]).unwrap(), msg);
    }

    // The same for the machine giving allowance in a transfer: field 1.
    #[test]
    fn a_transfer_names_the_giver_under_field_1() {
        let t = Transfer { from_replica: 3, to: 4, amount: 5 };
        assert_eq!(t.encode_to_vec(), vec![0x08, 3, 0x10, 4, 0x18, 5]);
        assert_eq!(Transfer::decode(&[0x08u8, 3, 0x10, 4, 0x18, 5][..]).unwrap(), t);
    }

    #[test]
    fn length_delimited_framing_separates_messages() {
        let a = SyncMessage {
            payload: Some(sync_message::Payload::Ack(Ack {
                namespace: "a".to_string(),
                observed: None,
            })),
        };
        let b = SyncMessage {
            payload: Some(sync_message::Payload::Ack(Ack {
                namespace: "b".to_string(),
                observed: None,
            })),
        };

        let mut stream = Vec::new();
        a.encode_length_delimited(&mut stream).unwrap();
        b.encode_length_delimited(&mut stream).unwrap();

        let mut cursor = &stream[..];
        assert_eq!(SyncMessage::decode_length_delimited(&mut cursor).unwrap(), a);
        assert_eq!(SyncMessage::decode_length_delimited(&mut cursor).unwrap(), b);
        assert!(cursor.is_empty());
    }
}

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
