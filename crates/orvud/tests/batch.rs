//! A Batch is several independent operations sent in one call. It is not a
//! transaction. If an operation in the middle is refused or fails, the others
//! still run. That is easy to assume wrongly, so these tests check it.

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

#[tokio::test]
async fn one_failing_op_does_not_abort_the_others() {
    let (dp, s) = opened().await;

    let reqs = vec![
        pb::Request { session: s.clone(), op: Some(pb::request::Op::CounterIncrement(
            pb::CounterIncrement { object: "a".into(), n: 1 })), request_id: Vec::new() },
        // The middle operation fails because 'a' is a counter, not a set.
        pb::Request { session: s.clone(), op: Some(pb::request::Op::SetAdd(
            pb::SetAdd { object: "a".into(), member: "x".into() })), request_id: Vec::new() },
        pb::Request { session: s.clone(), op: Some(pb::request::Op::CounterIncrement(
            pb::CounterIncrement { object: "a".into(), n: 2 })), request_id: Vec::new() },
    ];

    let out = dp.batch_for_test(pb::BatchRequest { session: s.clone(), requests: reqs, request_id: Vec::new() })
        .await
        .expect("the batch itself is accepted");
    assert_eq!(out.responses.len(), 3, "one result per request, in order");
    assert!(matches!(out.responses[0].result, Some(pb::response::Result::Ack(_))));
    assert!(matches!(out.responses[1].result, Some(pb::response::Result::Failed(_))));
    assert!(matches!(out.responses[2].result, Some(pb::response::Result::Ack(_))),
            "the third op ran despite the second failing");

    let v = dp.execute_for_test(pb::Request {
        session: s, op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "a".into() })),
        request_id: Vec::new(),
    }).await.expect("read");
    match v.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => {
            assert_eq!(i, 3, "both increments applied");
        }
        other => panic!("expected an int, got {other:?}"),
    }
}

#[tokio::test]
async fn an_empty_batch_is_an_empty_result_not_an_error() {
    let (dp, s) = opened().await;

    let out = dp.batch_for_test(pb::BatchRequest { session: s, requests: Vec::new(), request_id: Vec::new() })
        .await
        .expect("an empty batch is legal");
    assert!(out.responses.is_empty());
}

/// Sets counter `a` to 5 and builds the batch [inc 1, a request with no
/// operation, inc 2]. The caller sends it.
async fn seeded_with_an_op_less_batch() -> (orvud::server::Dataplane, String, Vec<pb::Request>) {
    let (dp, s) = opened().await;
    dp.execute_for_test(pb::Request {
        session: s.clone(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "a".into(), n: 5 })),
        request_id: Vec::new(),
    })
    .await
    .expect("seed");
    let reqs = vec![
        pb::Request { session: s.clone(), op: Some(pb::request::Op::CounterIncrement(
            pb::CounterIncrement { object: "a".into(), n: 1 })), request_id: Vec::new() },
        pb::Request { session: s.clone(), op: None, request_id: Vec::new() },
        pb::Request { session: s.clone(), op: Some(pb::request::Op::CounterIncrement(
            pb::CounterIncrement { object: "a".into(), n: 2 })), request_id: Vec::new() },
    ];
    (dp, s, reqs)
}

#[tokio::test]
async fn an_op_less_request_is_rejected_as_invalid_argument_naming_its_index() {
    let (dp, s, reqs) = seeded_with_an_op_less_batch().await;
    let err = dp.batch_for_test(pb::BatchRequest { session: s, requests: reqs, request_id: Vec::new() })
        .await
        .expect_err("a batch with an op-less request is rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("batch request 1"), "names the offending index: {}", err.message());
}

#[tokio::test]
async fn an_op_less_batch_leaves_the_engine_untouched() {
    let (dp, s, reqs) = seeded_with_an_op_less_batch().await;
    // The reply is ignored. This test only checks what the engine holds afterwards.
    let _ = dp.batch_for_test(pb::BatchRequest { session: s.clone(), requests: reqs, request_id: Vec::new() }).await;

    let v = dp.execute_for_test(pb::Request {
        session: s, op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "a".into() })),
        request_id: Vec::new(),
    }).await.expect("read");
    match v.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => {
            assert_eq!(i, 5, "the first op must not have applied");
        }
        other => panic!("expected an int, got {other:?}"),
    }
}

/// Builds the requests for the class test below. It is separate from
/// `seeded_with_an_op_less_batch` so that changing one test's setup cannot
/// change the other's.
fn inc_a(s: &str, n: u64) -> pb::Request {
    pb::Request {
        session: s.into(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "a".into(), n })),
        request_id: Vec::new(),
    }
}

async fn counter_a(dp: &orvud::server::Dataplane, s: &str) -> i64 {
    let v = dp
        .execute_for_test(pb::Request {
            session: s.into(),
            op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "a".into() })),
            request_id: Vec::new(),
        })
        .await
        .expect("read");
    match v.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => i,
        other => panic!("expected an int, got {other:?}"),
    }
}

/// A `Declare` with no class set is the same kind of mistake as a request with
/// no operation. It is a client bug that the envelope shows before the engine
/// is called. It is easy to hit, because `CLASS_UNSPECIFIED = 0` is the proto3
/// default. A Python author who writes `Declare(object="x")` and forgets
/// `object_class` gets it on the first run. If the batch stopped halfway, the
/// `inc 1` before it would already be applied, and the client would only get
/// an error that does not say what was applied. A generated client would then
/// retry and apply the increment twice.
#[tokio::test]
async fn a_declare_with_an_unset_class_in_a_batch_applies_nothing() {
    let (dp, s) = opened().await;
    dp.execute_for_test(inc_a(&s, 5)).await.expect("seed");

    let reqs = vec![
        inc_a(&s, 1),
        pb::Request {
            session: s.clone(),
            op: Some(pb::request::Op::Declare(pb::Declare {
                object: "x".into(),
                // Not set, which is what proto3 sends when the field is left out.
                object_class: pb::Class::Unspecified as i32,
            })),
            request_id: Vec::new(),
        },
        inc_a(&s, 2),
    ];
    let result = dp.batch_for_test(pb::BatchRequest { session: s.clone(), requests: reqs, request_id: Vec::new() }).await;

    // The counter is checked before the error code. What matters here is that
    // the increment before the bad Declare was not applied. A batch that
    // stopped halfway would return the same error code, so checking the code
    // first would let that bug pass.
    assert_eq!(counter_a(&dp, &s).await, 5, "the op before the bad Declare must not have applied");

    let err = result.expect_err("a batch with an unknown class is rejected whole");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("batch request 1"),
        "names the offending index: {}",
        err.message()
    );
}

#[tokio::test]
async fn a_foreign_inner_session_is_rejected_and_applies_nothing() {
    let (dp, s) = opened().await;
    let inc = |session: &str, n| pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "a".into(), n })),
        request_id: Vec::new(),
    };
    // Request 0 names the batch's own session. Request 1 names none, so it
    // uses the batch's. Request 2 names a different session.
    let reqs = vec![inc(&s, 1), inc("", 1), inc("some-other-session", 1)];
    let err = dp.batch_for_test(pb::BatchRequest { session: s.clone(), requests: reqs, request_id: Vec::new() })
        .await
        .expect_err("a mismatched inner session is rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("batch request 2"), "names the offending index: {}", err.message());

    let v = dp.execute_for_test(pb::Request {
        session: s, op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "a".into() })),
        request_id: Vec::new(),
    }).await.expect("read");
    // Nothing was applied, so the counter was never created and reads absent.
    match v.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) })) => {}
        other => panic!("nothing should have been applied, not even the requests before the bad one; got {other:?}"),
    }
}
