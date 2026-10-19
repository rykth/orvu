//! The envelope is the outer request and response messages. Client libraries
//! in other languages are generated from them, so these tests fix the parts of
//! their shape that carry meaning.

use orvud::pb;
use prost::Message;

#[test]
fn a_request_round_trips_through_prost() {
    let req = pb::Request {
        session: "s1".into(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement {
            object: "visits".into(),
            n: 3,
        })),
        request_id: Vec::new(),
    };
    let bytes = req.encode_to_vec();
    assert_eq!(pb::Request::decode(&bytes[..]).unwrap(), req);
}

#[test]
fn absent_is_distinguishable_from_the_empty_string() {
    let absent = pb::ReadValue { v: Some(pb::read_value::V::Absent(pb::Absent {})) };
    let empty = pb::ReadValue { v: Some(pb::read_value::V::StringValue(String::new())) };
    assert_ne!(absent, empty);
    assert_ne!(absent.encode_to_vec(), empty.encode_to_vec());
}

#[test]
fn a_refusal_is_a_result_variant_not_an_error_type() {
    // Refused is one of the variants of Response::result. That makes a refusal
    // a normal answer and not a gRPC status, which is an error for the whole
    // call. This test stops building if the variant is removed or moved out of
    // the oneof. It does not prove the server never returns a status error.
    let refused = pb::Response {
        result: Some(pb::response::Result::Refused(pb::Refused {
            detail: "allowance exhausted".into(),
        })),
    };
    match refused.result {
        Some(pb::response::Result::Refused(r)) => assert_eq!(r.detail, "allowance exhausted"),
        other => panic!("expected a Refused variant, got {other:?}"),
    }
}

#[test]
fn the_service_is_generated_for_both_sides() {
    // This stops compiling if build.rs no longer asks for build_server or
    // build_client.
    use orvud::pb::{data_plane_client::DataPlaneClient, data_plane_server::DataPlaneServer};
    fn names<T>() {}
    names::<DataPlaneClient<tonic::transport::Channel>>();
    names::<DataPlaneServer<NoopPlane>>();
}

struct NoopPlane;
