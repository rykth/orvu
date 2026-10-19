//! The most important tests in this module. A refusal is the engine saying no
//! and leaving everything as it was, for example when a bounded counter (one
//! that cannot go below zero) has no allowance left. It must reach the client
//! as a normal answer, with nothing changed, and the client must be able to
//! tell it apart from every kind of failure. A build that turned `Exhausted`
//! into a gRPC status (an error for the whole call) must not pass this file,
//! because bounded counters cannot work correctly through such a sidecar.
//!
//! About the setup: `BoundedIncrement` gives the new allowance to the replica
//! (machine) that made the call. So after `increment(n)` this replica's
//! allowance is exactly `n`, and a test with one replica can decrement
//! without being granted anything first.

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
    let s = dp.open_for_test(pb::OpenRequest {
        namespace: "default".into(), resume_token: String::new(), credential: Vec::new(),
    }).await.expect("open").session;
    (dp, s)
}

async fn exec(dp: &orvud::server::Dataplane, s: &str, op: pb::request::Op) -> pb::Response {
    dp.execute_for_test(pb::Request { session: s.into(), op: Some(op), request_id: Vec::new() })
        .await
        .expect("the BOUNDARY succeeded; the engine's answer is in the response")
}

fn int_of(r: pb::Response) -> i64 {
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => i,
        other => panic!("expected an int, got {other:?}"),
    }
}

async fn value(dp: &orvud::server::Dataplane, s: &str) -> i64 {
    int_of(exec(dp, s, pb::request::Op::BoundedValue(pb::BoundedValue { object: "stock".into() })).await)
}

async fn allowance(dp: &orvud::server::Dataplane, s: &str) -> i64 {
    int_of(exec(dp, s, pb::request::Op::BoundedAllowance(pb::BoundedAllowance { object: "stock".into() })).await)
}

async fn increment(dp: &orvud::server::Dataplane, s: &str, n: u64) -> pb::Response {
    exec(dp, s, pb::request::Op::BoundedIncrement(pb::BoundedIncrement { object: "stock".into(), n })).await
}

#[tokio::test]
async fn an_exhausted_decrement_is_a_refusal_value_with_state_intact() {
    let (dp, s) = opened().await;
    increment(&dp, &s, 3).await;
    assert_eq!(value(&dp, &s).await, 3);

    let r = exec(&dp, &s, pb::request::Op::BoundedTryDecrement(pb::BoundedTryDecrement {
        object: "stock".into(), n: 10,
    })).await;

    match r.result {
        Some(pb::response::Result::Refused(_)) => {}
        Some(pb::response::Result::Failed(f)) => {
            panic!("a refusal must NOT be a failure; got code {:?}", f.code())
        }
        other => panic!("expected Refused, got {other:?}"),
    }

    assert_eq!(value(&dp, &s).await, 3, "a refused decrement changed nothing: no oversell, no partial apply");
    assert_eq!(allowance(&dp, &s).await, 3, "and the allowance is untouched too");
}

#[tokio::test]
async fn a_refusal_and_a_failure_are_different_variants() {
    let (dp, s) = opened().await;
    increment(&dp, &s, 1).await;

    let refused = exec(&dp, &s, pb::request::Op::BoundedTryDecrement(pb::BoundedTryDecrement {
        object: "stock".into(), n: 99,
    })).await;
    // 'stock' is a bounded counter, so using it as a set is a type error.
    let failed = exec(&dp, &s, pb::request::Op::SetAdd(pb::SetAdd {
        object: "stock".into(), member: "x".into(),
    })).await;

    assert!(matches!(refused.result, Some(pb::response::Result::Refused(_))));
    assert!(matches!(failed.result, Some(pb::response::Result::Failed(_))));
    assert_ne!(refused.result, failed.result,
               "a client MUST be able to branch on refused-vs-failed");
}

#[tokio::test]
async fn a_successful_decrement_still_acks() {
    let (dp, s) = opened().await;
    increment(&dp, &s, 5).await;
    let r = exec(&dp, &s, pb::request::Op::BoundedTryDecrement(pb::BoundedTryDecrement {
        object: "stock".into(), n: 2,
    })).await;
    assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "got {r:?}");
    assert_eq!(value(&dp, &s).await, 3);
    assert_eq!(allowance(&dp, &s).await, 3, "the decrement spent allowance");
}

#[tokio::test]
async fn an_over_allowance_transfer_is_a_refusal_value_with_state_intact() {
    let (dp, s) = opened().await;
    increment(&dp, &s, 2).await;

    let r = exec(&dp, &s, pb::request::Op::BoundedTransfer(pb::BoundedTransfer {
        object: "stock".into(), to_replica: 2, n: 50,
    })).await;
    match r.result {
        Some(pb::response::Result::Refused(_)) => {}
        Some(pb::response::Result::Failed(f)) => {
            panic!("a refused transfer must NOT be a failure; got code {:?}", f.code())
        }
        other => panic!("expected Refused, got {other:?}"),
    }
    assert_eq!(allowance(&dp, &s).await, 2, "a refused transfer gave nothing away");
    assert_eq!(value(&dp, &s).await, 2);
}

#[tokio::test]
async fn a_covered_transfer_moves_allowance_and_leaves_value_alone() {
    let (dp, s) = opened().await;
    increment(&dp, &s, 5).await;

    let r = exec(&dp, &s, pb::request::Op::BoundedTransfer(pb::BoundedTransfer {
        object: "stock".into(), to_replica: 2, n: 4,
    })).await;
    assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "got {r:?}");
    assert_eq!(allowance(&dp, &s).await, 1);
    assert_eq!(value(&dp, &s).await, 5, "a transfer moves rights, it does not change the value");
}

#[tokio::test]
async fn bounded_ops_on_a_non_bounded_object_are_failures_not_refusals() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::SetAdd(pb::SetAdd { object: "stock".into(), member: "x".into() })).await;
    let r = exec(&dp, &s, pb::request::Op::BoundedTryDecrement(pb::BoundedTryDecrement {
        object: "stock".into(), n: 1,
    })).await;
    match r.result {
        Some(pb::response::Result::Failed(f)) => assert_eq!(f.code(), pb::Code::ClassMismatch),
        other => panic!("expected Failed(ClassMismatch), got {other:?}"),
    }
}
