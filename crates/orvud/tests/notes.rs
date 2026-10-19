//! A team's notes used to be kept in a plain map, with who wrote each in a
//! second one. The sidecar moves them over the first time the team's notes
//! are used, by any request, a batch or a watch: once, keeping who wrote
//! each, without changing a note written since, and without leaving
//! anything behind for a team that has no old notes.

use futures::StreamExt;
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use orvud::pb::request::Op;
use std::sync::Arc;
use std::time::Duration;

const NOTES: &str = "orvu/crew/notes-mv";

async fn opened(client: Arc<Client>) -> (orvud::server::Dataplane, String) {
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default");
    let opening = pb::OpenRequest { namespace: "default".into(), resume_token: String::new(), credential: Vec::new() };
    let session = dp.open_for_test(opening).await.expect("open").session;
    (dp, session)
}

async fn engine() -> Arc<Client> {
    let fabric = Arc::new(MemTransport::new());
    Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.expect("engine"))
}

/// Notes as they used to be kept: two of them, one with who wrote it.
async fn old_notes(client: &Client) {
    let notes = client.map("orvu/crew/notes").await.unwrap();
    notes.put("deploy-date", "friday").await.unwrap();
    notes.put("plan", "ship it").await.unwrap();
    client.map("orvu/crew/notes-by").await.unwrap().put("plan", "alice").await.unwrap();
    client.map("orvu/crew/index").await.unwrap().put("notes", "map").await.unwrap();
}

async fn run(dp: &orvud::server::Dataplane, session: &str, op: Op) -> pb::Response {
    dp.execute_for_test(pb::Request { session: session.into(), op: Some(op), request_id: Vec::new() }).await.expect("answered")
}

fn entries() -> Op {
    Op::MvMapEntries(pb::MvMapEntries { object: NOTES.into() })
}

/// The notes in the new map, as the sidecar answers them.
async fn notes(dp: &orvud::server::Dataplane, session: &str) -> Vec<(String, Vec<String>)> {
    match run(dp, session, entries()).await.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::MultiPairs(p)) })) => {
            p.pairs.into_iter().map(|pair| (pair.key, pair.values)).collect()
        }
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) })) => Vec::new(),
        other => panic!("not a map's entries: {other:?}"),
    }
}

fn moved() -> Vec<(String, Vec<String>)> {
    vec![
        (String::new(), vec!["\u{1f}moved".to_string()]),
        ("deploy-date".to_string(), vec!["\u{1f}friday".to_string()]),
        ("plan".to_string(), vec!["alice\u{1f}ship it".to_string()]),
    ]
}

#[tokio::test]
async fn old_notes_are_moved_over_with_who_wrote_them() {
    let client = engine().await;
    old_notes(&client).await;
    let (dp, s) = opened(client.clone()).await;
    assert_eq!(notes(&dp, &s).await, moved());
    // The team's list names the notes, and the old ones are left as they were.
    let index = client.map("orvu/crew/index").await.unwrap();
    assert_eq!(index.get("notes-mv").await.unwrap().as_deref(), Some("mvmap"));
    assert_eq!(client.map("orvu/crew/notes").await.unwrap().keys().await.unwrap().len(), 2);
    assert_eq!(client.map("orvu/crew/notes-by").await.unwrap().get("plan").await.unwrap().as_deref(), Some("alice"));
}

#[tokio::test]
async fn notes_are_moved_once() {
    let client = engine().await;
    old_notes(&client).await;
    let (dp, s) = opened(client.clone()).await;
    let remove = pb::MvMapRemove { object: NOTES.into(), key: "plan".into() };
    run(&dp, &s, Op::MvMapRemove(remove)).await;
    assert_eq!(notes(&dp, &s).await.len(), 2);
    // Nor by a sidecar started again, which knows only what is kept.
    let (again, s) = opened(client).await;
    assert_eq!(notes(&again, &s).await.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["", "deploy-date"]);
}

#[tokio::test]
async fn a_note_written_since_keeps_what_was_written() {
    let client = engine().await;
    old_notes(&client).await;
    client.mv_map(NOTES).await.unwrap().put("plan", "bob\u{1f}wait").await.unwrap();
    let (dp, s) = opened(client).await;
    let found = notes(&dp, &s).await;
    assert_eq!(found[2], ("plan".to_string(), vec!["bob\u{1f}wait".to_string()]));
    assert_eq!(found[1].0, "deploy-date");
}

#[tokio::test]
async fn a_team_with_no_old_notes_is_left_as_it_is() {
    let client = engine().await;
    let (dp, s) = opened(client.clone()).await;
    assert!(notes(&dp, &s).await.is_empty());
    for object in [NOTES, "orvu/crew/index", "orvu/crew/notes", "orvu/crew/notes-by"] {
        assert!(!client.exists(object).await, "{object}");
    }
}

#[tokio::test]
async fn a_batch_or_a_watch_moves_them_too() {
    let client = engine().await;
    old_notes(&client).await;
    let (dp, s) = opened(client.clone()).await;
    let batch = pb::BatchRequest { session: s.clone(), requests: vec![pb::Request { session: String::new(), op: Some(entries()), request_id: Vec::new() }], request_id: Vec::new() };
    dp.batch_for_test(batch).await.unwrap();
    assert!(client.exists(NOTES).await);

    let client = engine().await;
    old_notes(&client).await;
    let (dp, s) = opened(client.clone()).await;
    let watch = pb::WatchRequest { session: s.clone(), object: NOTES.into(), kind: pb::Kind::MvMap as i32 };
    let (mut stream, _guard) = dp.watch_for_test(watch).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.unwrap().unwrap().unwrap();
    match first.event {
        Some(pb::watch_event::Event::Value(pb::ReadValue { v: Some(pb::read_value::V::MultiPairs(p)) })) => {
            assert_eq!(p.pairs.len(), 3);
        }
        other => panic!("not the moved notes: {other:?}"),
    }
}

// A team that started with the new notes is not marked as moved while it
// has no old notes: another machine may still hold some it has not sent yet,
// and a mark would hide them there for good. They are moved over when they
// come.
#[tokio::test]
async fn old_notes_that_come_late_are_still_moved_over() {
    let client = engine().await;
    let (dp, s) = opened(client.clone()).await;
    let put = pb::MvMapPut { object: NOTES.into(), key: "owner".into(), value: "\u{1f}alice".into() };
    run(&dp, &s, Op::MvMapPut(put)).await;
    assert_eq!(notes(&dp, &s).await, vec![("owner".to_string(), vec!["\u{1f}alice".to_string()])]);
    old_notes(&client).await;
    let found = notes(&dp, &s).await;
    assert_eq!(found.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["", "deploy-date", "owner", "plan"]);
    assert_eq!(found[3], ("plan".to_string(), vec!["alice\u{1f}ship it".to_string()]));
}

// A move that fails turns the request away, so that nothing can change the
// notes before the move is made whole. Here the team's list is another kind
// of object, which a program with no rules could make it.
#[tokio::test]
async fn a_move_that_fails_turns_the_request_away() {
    let client = engine().await;
    client.map("orvu/crew/notes").await.unwrap().put("plan", "ship it").await.unwrap();
    client.counter("orvu/crew/index").await.unwrap().increment(1).await.unwrap();
    let (dp, s) = opened(client.clone()).await;
    for _ in 0..2 {
        let refused = dp.execute_for_test(pb::Request { session: s.clone(), op: Some(entries()), request_id: Vec::new() }).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Internal);
        assert!(refused.message().contains("could not be moved over"), "{}", refused.message());
    }
    let batch = pb::BatchRequest { session: s.clone(), requests: vec![pb::Request { session: String::new(), op: Some(entries()), request_id: Vec::new() }], request_id: Vec::new() };
    assert_eq!(dp.batch_for_test(batch).await.unwrap_err().code(), tonic::Code::Internal);
    let watch = pb::WatchRequest { session: s.clone(), object: NOTES.into(), kind: pb::Kind::MvMap as i32 };
    assert_eq!(dp.watch_for_test(watch).await.err().unwrap().code(), tonic::Code::Internal);
    let marked = client.mv_map(NOTES).await.unwrap().get("").await.unwrap();
    assert!(marked.is_empty(), "a move that failed was marked as done");
}
