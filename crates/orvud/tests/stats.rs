//! What the sidecar counts about the calls it answers, for its page of
//! numbers.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use orvud::stats::{op_name, Stats, OPS};
use std::sync::Arc;

async fn opened() -> (orvud::server::Dataplane, Arc<Stats>, String) {
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "s")).await.unwrap());
    let stats = Arc::new(Stats::default());
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default").counting(stats.clone());
    let s = dp
        .open_for_test(pb::OpenRequest { namespace: "default".into(), resume_token: String::new(), credential: Vec::new() })
        .await
        .unwrap()
        .session;
    (dp, stats, s)
}

fn request(s: &str, op: pb::request::Op) -> pb::Request {
    pb::Request { session: s.into(), op: Some(op), request_id: Vec::new() }
}

// Every operation the sidecar offers is counted under the name it has in the
// message definitions, which is also the name clients send it by.
#[test]
fn every_operation_is_counted_under_its_own_name() {
    let text = include_str!("../proto/orvu/v1/dataplane.proto");
    let request = &text[text.find("message Request {").unwrap()..];
    let ops = &request[request.find("oneof op {").unwrap()..request.find("\n  }").unwrap()];
    let names: Vec<&str> = ops
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).filter(|_| line.trim_end().ends_with(';')))
        .collect();
    assert_eq!(names.len(), 54);
    assert_eq!(OPS.to_vec(), names);
    let increment = pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "c".into(), n: 1 });
    assert_eq!(op_name(&increment), "counter_increment");
    let parent = pb::request::Op::TreeParent(pb::TreeParent { object: "t".into(), node: "a".into() });
    assert_eq!(op_name(&parent), "tree_parent");
}

#[tokio::test]
async fn each_operation_is_counted_with_how_it_ended() {
    let (dp, stats, s) = opened().await;
    let increment = || pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "c".into(), n: 1 });
    for _ in 0..2 {
        dp.execute_for_test(request(&s, increment())).await.unwrap();
    }
    // A bounded counter with nothing in it says no.
    let take = pb::request::Op::BoundedTryDecrement(pb::BoundedTryDecrement { object: "b".into(), n: 1 });
    dp.execute_for_test(request(&s, take)).await.unwrap();
    // A name used as another type fails.
    let add = pb::request::Op::SetAdd(pb::SetAdd { object: "c".into(), member: "x".into() });
    dp.execute_for_test(request(&s, add)).await.unwrap();
    // A batch counts each operation in it.
    let batch = pb::BatchRequest { session: s.clone(), requests: vec![request(&s, increment()), request(&s, increment())], request_id: Vec::new() };
    dp.batch_for_test(batch).await.unwrap();

    let page = stats.render(0);
    for line in [
        "orvu_sidecar_operations_total{op=\"counter_increment\",result=\"done\"} 4\n",
        "orvu_sidecar_operations_total{op=\"bounded_try_decrement\",result=\"refused\"} 1\n",
        "orvu_sidecar_operations_total{op=\"set_add\",result=\"failed\"} 1\n",
        "orvu_sidecar_execute_seconds_count 4\n",
        "orvu_sidecar_batch_seconds_count 1\n",
    ] {
        assert!(page.contains(line), "{line} is not in\n{page}");
    }
    // An operation that was not asked for has no line.
    assert!(!page.contains("op=\"list_insert\""), "{page}");
}

// A call the sidecar turns away is counted once, by the kind of call,
// whatever it held: an unknown session, a request that is itself broken, a
// batch that is too large.
#[tokio::test]
async fn a_call_turned_away_is_counted_once() {
    let (dp, stats, s) = opened().await;
    let increment = || pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "c".into(), n: 1 });
    let declare = pb::request::Op::Declare(pb::Declare { object: "d".into(), object_class: 0 });
    dp.execute_for_test(request(&s, declare)).await.unwrap_err();
    dp.execute_for_test(request("nobody", increment())).await.unwrap_err();
    let too_many = pb::BatchRequest { session: s.clone(), requests: (0..2_000).map(|_| request(&s, increment())).collect(), request_id: Vec::new() };
    dp.batch_for_test(too_many).await.unwrap_err();
    let lost = pb::BatchRequest { session: "nobody".into(), requests: vec![request("nobody", increment())], request_id: Vec::new() };
    dp.batch_for_test(lost).await.unwrap_err();
    let watch = pb::WatchRequest { session: "nobody".into(), object: "c".into(), kind: pb::Kind::Counter as i32 };
    assert!(dp.watch_for_test(watch).await.is_err());

    let page = stats.render(0);
    for line in [
        "orvu_sidecar_rejected_total{call=\"execute\"} 2\n",
        "orvu_sidecar_rejected_total{call=\"batch\"} 2\n",
        "orvu_sidecar_rejected_total{call=\"watch\"} 1\n",
        "orvu_sidecar_execute_seconds_count 2\n",
        "orvu_sidecar_batch_seconds_count 2\n",
    ] {
        assert!(page.contains(line), "{line} is not in\n{page}");
    }
    // None of what was turned away is counted as an operation.
    assert!(!page.contains("orvu_sidecar_operations_total{"), "{page}");
}

#[tokio::test]
async fn watches_and_connections_are_counted() {
    let (dp, stats, s) = opened().await;
    let watch = pb::WatchRequest { session: s.clone(), object: "c".into(), kind: pb::Kind::Counter as i32 };
    let _first = dp.watch_for_test(watch.clone()).await.unwrap();
    let _second = dp.watch_for_test(watch).await.unwrap();
    let page = stats.render(3);
    assert!(page.contains("orvu_sidecar_watches_total 2\n"), "{page}");
    assert!(page.contains("# TYPE orvu_sidecar_connections gauge\norvu_sidecar_connections 3\n"), "{page}");
}
