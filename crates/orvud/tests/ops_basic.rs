//! Counter, register and set operations sent through the sidecar, plus the
//! two mistakes a client in another language is likely to make: using an
//! object as the wrong type, and using it under the wrong consistency class
//! (AP or Escrow). Each must come back with its own error code, so the app
//! learns which mistake it made.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use std::sync::Arc;

async fn opened() -> (orvud::server::Dataplane, String) {
    let fabric = Arc::new(MemTransport::new());
    let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    let client = Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine"));
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default");
    let session = dp
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open")
        .session;
    (dp, session)
}

async fn exec(dp: &orvud::server::Dataplane, session: &str, op: pb::request::Op) -> pb::Response {
    dp.execute_for_test(pb::Request { session: session.into(), op: Some(op), request_id: Vec::new() })
        .await
        .expect("the boundary accepted the request")
}

fn expect_int(r: pb::Response) -> i64 {
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => i,
        other => panic!("expected an int read, got {other:?}"),
    }
}

fn expect_failed(r: pb::Response) -> pb::Failed {
    match r.result {
        Some(pb::response::Result::Failed(f)) => f,
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_counter_increments_and_reads_back_through_the_envelope() {
    let (dp, s) = opened().await;
    let ack = exec(&dp, &s, pb::request::Op::CounterIncrement(pb::CounterIncrement {
        object: "visits".into(),
        n: 7,
    })).await;
    assert!(matches!(ack.result, Some(pb::response::Result::Ack(_))));

    let v = exec(&dp, &s, pb::request::Op::CounterValue(pb::CounterValue {
        object: "visits".into(),
    })).await;
    assert_eq!(expect_int(v), 7);
}

#[tokio::test]
async fn an_unset_register_reads_absent_not_the_empty_string() {
    let (dp, s) = opened().await;
    let r = exec(&dp, &s, pb::request::Op::RegisterGet(pb::RegisterGet {
        object: "title".into(),
    })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) })) => {}
        other => panic!("an unset register must read Absent, got {other:?}"),
    }

    exec(&dp, &s, pb::request::Op::RegisterSet(pb::RegisterSet {
        object: "title".into(),
        value: String::new(),
    })).await;

    let r = exec(&dp, &s, pb::request::Op::RegisterGet(pb::RegisterGet {
        object: "title".into(),
    })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::StringValue(s)) })) => {
            assert_eq!(s, "", "set-to-empty is NOT the same as unset");
        }
        other => panic!("expected an empty string value, got {other:?}"),
    }
}

#[tokio::test]
async fn a_set_adds_and_lists() {
    let (dp, s) = opened().await;
    for m in ["a", "b"] {
        exec(&dp, &s, pb::request::Op::SetAdd(pb::SetAdd {
            object: "tags".into(),
            member: m.into(),
        })).await;
    }
    let r = exec(&dp, &s, pb::request::Op::SetElements(pb::SetElements {
        object: "tags".into(),
    })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Strings(l)) })) => {
            let mut got = l.values.clone();
            got.sort();
            assert_eq!(got, vec!["a".to_string(), "b".to_string()]);
        }
        other => panic!("expected a string list, got {other:?}"),
    }
}

#[tokio::test]
async fn using_an_object_as_the_wrong_type_is_a_code_not_a_status() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::CounterIncrement(pb::CounterIncrement {
        object: "visits".into(),
        n: 1,
    })).await;
    // The same id, now used as a set. The engine already knows it as a counter.
    let r = exec(&dp, &s, pb::request::Op::SetAdd(pb::SetAdd {
        object: "visits".into(),
        member: "x".into(),
    })).await;
    assert_eq!(expect_failed(r).code(), pb::Code::TypeMismatch);
}

#[tokio::test]
async fn an_ap_op_on_an_escrow_declared_object_is_a_class_mismatch_code() {
    let (dp, s) = opened().await;
    let declared = exec(&dp, &s, pb::request::Op::Declare(pb::Declare {
        object: "locks".into(),
        object_class: pb::Class::Escrow as i32,
    })).await;
    assert!(matches!(declared.result, Some(pb::response::Result::Ack(_))), "declare must ack: {declared:?}");
    let r = exec(&dp, &s, pb::request::Op::CounterIncrement(pb::CounterIncrement {
        object: "locks".into(),
        n: 1,
    })).await;
    assert_eq!(expect_failed(r).code(), pb::Code::ClassMismatch);
}

#[tokio::test]
async fn a_request_with_no_op_is_invalid_argument() {
    let (dp, s) = opened().await;
    let err = dp
        .execute_for_test(pb::Request { session: s, op: None, request_id: Vec::new() })
        .await
        .expect_err("no op is a boundary error");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn declare_with_an_unspecified_class_is_rejected_at_the_boundary() {
    let (dp, s) = opened().await;
    let err = dp
        .execute_for_test(pb::Request {
            session: s,
            op: Some(pb::request::Op::Declare(pb::Declare {
                object: "x".into(),
                object_class: pb::Class::Unspecified as i32,
            })),
            request_id: Vec::new(),
        })
        .await
        .expect_err("unspecified class is a boundary error");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn declare_ap_acks() {
    let (dp, s) = opened().await;
    let r = exec(&dp, &s, pb::request::Op::Declare(pb::Declare {
        object: "visits".into(),
        object_class: pb::Class::Ap as i32,
    })).await;
    assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "got {r:?}");
}
