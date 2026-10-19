//! A call that carries a request id is made once, however often it is sent.
//! A client sends a change again, with the same id, when its answer never
//! came, and is given the answer the first one got.

use orvu_crdt::ReplicaId;
use orvu_runtime::{FsyncPolicy, MemStore, NodeConfig};
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::agents::{self, Agents};
use orvud::pb;
use orvud::server::Dataplane;
use std::sync::Arc;
use std::time::Duration;
use tonic::Code;

async fn engine(store: &MemStore) -> Arc<Client> {
    let fabric = Arc::new(MemTransport::new());
    let mut cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    cfg.fsync = FsyncPolicy::Always;
    Arc::new(Client::start_with_store(fabric.endpoint(1), cfg, Box::new(store.clone())).await.expect("engine"))
}

async fn session(dp: &Dataplane, key: &str) -> String {
    let opening = pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: key.as_bytes().to_vec() };
    dp.open_for_test(opening).await.expect("open").session
}

fn add_one(session: &str, id: &[u8]) -> pb::Request {
    pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 1 })),
        request_id: id.to_vec(),
    }
}

fn value(session: &str) -> pb::Request {
    pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "visits".into() })),
        request_id: Vec::new(),
    }
}

async fn visits(dp: &Dataplane, session: &str) -> i64 {
    match dp.execute_for_test(value(session)).await.unwrap().result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(n)) })) => n,
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) })) => 0,
        other => panic!("expected a number, got {other:?}"),
    }
}

fn acked(response: &pb::Response) -> bool {
    matches!(response.result, Some(pb::response::Result::Ack(_)))
}

#[tokio::test]
async fn a_call_sent_again_is_answered_and_not_made_again() {
    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default");
    let s = session(&dp, "").await;
    for _ in 0..3 {
        assert!(acked(&dp.execute_for_test(add_one(&s, b"first")).await.unwrap()));
    }
    assert_eq!(visits(&dp, &s).await, 1);
    // Another id is another call, and a call with no id is made each time.
    dp.execute_for_test(add_one(&s, b"second")).await.unwrap();
    dp.execute_for_test(add_one(&s, b"")).await.unwrap();
    dp.execute_for_test(add_one(&s, b"")).await.unwrap();
    assert_eq!(visits(&dp, &s).await, 4);
}

// A batch is one call. Sent again, its changes get the answers they got and
// are not made again, and its reads are read again, fresh.
#[tokio::test]
async fn a_batch_sent_again_reads_afresh_and_makes_nothing_again() {
    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default");
    let s = session(&dp, "").await;
    let batch = || pb::BatchRequest {
        session: s.clone(),
        requests: vec![add_one("", b"ignored"), value("")],
        request_id: b"batch".to_vec(),
    };
    let first = dp.batch_for_test(batch()).await.unwrap().responses;
    dp.execute_for_test(add_one(&s, b"")).await.unwrap();
    let again = dp.batch_for_test(batch()).await.unwrap().responses;
    assert_eq!(first[0], again[0]);
    assert_eq!(read_number(&first[1]), 1);
    assert_eq!(read_number(&again[1]), 2, "the read was read again");
    assert_eq!(visits(&dp, &s).await, 2);
}

// A call that changes nothing is not remembered: its id is not used, and
// it is answered afresh each time.
#[tokio::test]
async fn a_read_sent_again_is_read_afresh() {
    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default");
    let s = session(&dp, "").await;
    let read = || pb::Request { request_id: b"read".to_vec(), ..value(&s) };
    assert_eq!(read_number(&dp.execute_for_test(read()).await.unwrap()), 0);
    dp.execute_for_test(add_one(&s, b"")).await.unwrap();
    assert_eq!(read_number(&dp.execute_for_test(read()).await.unwrap()), 1);
    let reads = || pb::BatchRequest { session: s.clone(), requests: vec![value("")], request_id: b"reads".to_vec() };
    dp.batch_for_test(reads()).await.unwrap();
    dp.execute_for_test(add_one(&s, b"")).await.unwrap();
    assert_eq!(read_number(&dp.batch_for_test(reads()).await.unwrap().responses[0]), 2);
    assert_eq!(dp.remembered_bytes(), 0);
}

// What is remembered is bounded in bytes too. A batch's reads, which can be
// large, are not remembered at all.
#[tokio::test]
async fn what_is_remembered_is_bounded_in_bytes() {
    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default")
        .remembering(10_000, Duration::from_secs(600), 1_000);
    let s = session(&dp, "").await;
    let big = pb::Request {
        session: s.clone(),
        op: Some(pb::request::Op::SetAdd(pb::SetAdd { object: "big".into(), member: "x".repeat(100_000) })),
        request_id: Vec::new(),
    };
    dp.execute_for_test(big).await.unwrap();
    let elements = pb::Request {
        session: String::new(),
        op: Some(pb::request::Op::SetElements(pb::SetElements { object: "big".into() })),
        request_id: Vec::new(),
    };
    for n in 0..200u32 {
        let batch = pb::BatchRequest {
            session: s.clone(),
            requests: vec![add_one("", b""), elements.clone()],
            request_id: n.to_be_bytes().to_vec(),
        };
        dp.batch_for_test(batch).await.unwrap();
        assert!(dp.remembered_bytes() <= 1_000, "{} bytes remembered", dp.remembered_bytes());
    }
    // Ids let go of for room are still not made again: the receipts say so.
    let again = pb::BatchRequest { session: s.clone(), requests: vec![add_one("", b"")], request_id: 0u32.to_be_bytes().to_vec() };
    dp.batch_for_test(again).await.unwrap();
    assert_eq!(visits(&dp, &s).await, 200);
}

fn read_number(response: &pb::Response) -> i64 {
    match &response.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(n)) })) => *n,
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) })) => 0,
        other => panic!("expected a number, got {other:?}"),
    }
}

// The note that a change was made is saved with the change. A sidecar that
// stopped before it could answer knows, once it is back, that the change was
// made, and answers the call sent again without making it again.
#[tokio::test]
async fn a_change_made_before_a_restart_is_not_made_again() {
    let store = MemStore::new();
    let client = engine(&store).await;
    let dp = Dataplane::new(client.clone(), ReplicaId(1), "default");
    let s = session(&dp, "").await;
    dp.execute_for_test(add_one(&s, b"before")).await.unwrap();
    let batch = |s: &str| pb::BatchRequest {
        session: s.into(),
        requests: vec![add_one("", b""), value(""), add_one("", b"")],
        request_id: b"batch".to_vec(),
    };
    dp.batch_for_test(batch(&s)).await.unwrap();
    client.shutdown().await;

    let dp = Dataplane::new(engine(&store.reopened()).await, ReplicaId(1), "default");
    let s = session(&dp, "").await;
    assert_eq!(visits(&dp, &s).await, 3);
    assert!(acked(&dp.execute_for_test(add_one(&s, b"before")).await.unwrap()));
    let replies = dp.batch_for_test(batch(&s)).await.unwrap().responses;
    assert!(acked(&replies[0]) && acked(&replies[2]), "{replies:?}");
    assert_eq!(visits(&dp, &s).await, 3);
    // An id it never saw is a new call.
    dp.execute_for_test(add_one(&s, b"after")).await.unwrap();
    assert_eq!(visits(&dp, &s).await, 4);
}

// An id belongs to the agent that sent it. Another agent's call with the
// same id is a call of its own: it is made, and is not given the first
// agent's answer.
#[tokio::test]
async fn an_agent_cannot_use_another_agents_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(agents::FILE);
    let alice = agents::add(&path, "alice").unwrap();
    let bob = agents::add(&path, "bob").unwrap();
    let store = MemStore::new();
    let client = engine(&store).await;
    let admitting = |client: Arc<Client>| {
        Dataplane::new(client, ReplicaId(1), "default").admitting(Arc::new(Agents::from_file(path.clone())))
    };
    let dp = admitting(client.clone());
    let (a, b) = (session(&dp, &alice).await, session(&dp, &bob).await);
    dp.execute_for_test(add_one(&a, b"shared")).await.unwrap();
    dp.execute_for_test(add_one(&b, b"shared")).await.unwrap();
    dp.execute_for_test(add_one(&a, b"alice-only")).await.unwrap();
    assert_eq!(visits(&dp, &a).await, 3);
    // The same after a restart, where only the saved receipts are left.
    client.shutdown().await;
    let dp = admitting(engine(&store.reopened()).await);
    let b = session(&dp, &bob).await;
    dp.execute_for_test(add_one(&b, b"alice-only")).await.unwrap();
    assert_eq!(visits(&dp, &b).await, 4, "bob's call was made");
    let a = session(&dp, &alice).await;
    dp.execute_for_test(add_one(&a, b"alice-only")).await.unwrap();
    assert_eq!(visits(&dp, &a).await, 4, "alice's call was not made again");
}

// What is kept in memory is bounded: the newest ids of each agent, for a
// while. A call sent again past that is still not made again, because the
// receipt saved with its change says it was made.
#[tokio::test]
async fn a_call_let_go_of_from_memory_is_still_not_made_again() {
    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default")
        .remembering(2, Duration::from_secs(600), usize::MAX);
    let s = session(&dp, "").await;
    for id in [b"a", b"b", b"c"] {
        dp.execute_for_test(add_one(&s, id)).await.unwrap();
    }
    dp.execute_for_test(add_one(&s, b"c")).await.unwrap();
    dp.execute_for_test(add_one(&s, b"a")).await.unwrap();
    assert_eq!(visits(&dp, &s).await, 3, "the oldest id was let go, and not made again");

    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default")
        .remembering(100, Duration::ZERO, usize::MAX);
    let s = session(&dp, "").await;
    dp.execute_for_test(add_one(&s, b"a")).await.unwrap();
    dp.execute_for_test(add_one(&s, b"a")).await.unwrap();
    assert_eq!(visits(&dp, &s).await, 1, "an id past its time in memory was not made again");
}

#[tokio::test]
async fn an_id_longer_than_64_bytes_is_refused() {
    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default");
    let s = session(&dp, "").await;
    let refused = dp.execute_for_test(add_one(&s, &[1; 65])).await.unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    let batch = pb::BatchRequest { session: s.clone(), requests: vec![value("")], request_id: vec![1; 65] };
    assert_eq!(dp.batch_for_test(batch).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(visits(&dp, &s).await, 0);
}

// A call sent again while the first is still being made waits for that one's
// answer, and is not made beside it.
#[tokio::test]
async fn a_call_sent_again_while_the_first_runs_waits_for_it() {
    let dp = Dataplane::new(engine(&MemStore::new()).await, ReplicaId(1), "default");
    let s = session(&dp, "").await;
    let (one, two, three) = tokio::join!(
        dp.execute_for_test(add_one(&s, b"same")),
        dp.execute_for_test(add_one(&s, b"same")),
        dp.execute_for_test(add_one(&s, b"same")),
    );
    assert!(acked(&one.unwrap()) && acked(&two.unwrap()) && acked(&three.unwrap()));
    assert_eq!(visits(&dp, &s).await, 1);
}
