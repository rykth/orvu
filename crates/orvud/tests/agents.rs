//! Agents and their keys. While the sidecar names no agents it lets in every
//! program, as it always has. Once it names one, a program has to give a key,
//! and the session is that agent's. An agent that is removed, and a program
//! that came in without a key before the first agent was added, are turned
//! away at their next call.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::agents::{self, Agents};
use orvud::pb;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tonic::Code;

async fn dataplane(file: &Path) -> orvud::server::Dataplane {
    dataplane_with(Agents::from_file(file.to_path_buf())).await
}

/// One that takes the agents file as settled at once, rather than after it
/// has been left alone for a moment, so a removal ends sessions straight away.
async fn settled_dataplane(file: &Path) -> orvud::server::Dataplane {
    dataplane_with(Agents::from_file(file.to_path_buf()).settling_after(std::time::Duration::ZERO)).await
}

async fn dataplane_with(agents: Agents) -> orvud::server::Dataplane {
    let fabric = Arc::new(MemTransport::new());
    let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    let client = Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine starts"));
    orvud::server::Dataplane::new(client, ReplicaId(1), "default").admitting(Arc::new(agents))
}

fn opening(key: &str, resume: &str) -> pb::OpenRequest {
    pb::OpenRequest { namespace: String::new(), resume_token: resume.into(), credential: key.as_bytes().to_vec() }
}

fn read(session: &str) -> pb::Request {
    pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "visits".into() })),
        request_id: Vec::new(),
    }
}

fn file() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(agents::FILE);
    (dir, path)
}

#[tokio::test]
async fn with_no_agents_a_program_needs_no_key() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let opened = dp.open_for_test(opening("", "")).await.unwrap();
    assert_eq!(opened.agent, "");
    dp.execute_for_test(read(&opened.session)).await.unwrap();
    // A key is refused, so that a program that expects to be checked
    // finds out that nothing checks it.
    let refused = dp.open_for_test(opening("orvu-key-00", "")).await.unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(refused.message().contains("has no agents"), "{}", refused.message());
}

#[tokio::test]
async fn with_agents_a_program_is_the_agent_whose_key_it_gives() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let alice = agents::add(&path, "alice").unwrap();
    let opened = dp.open_for_test(opening(&alice, "")).await.unwrap();
    assert_eq!(opened.agent, "alice");
    dp.execute_for_test(read(&opened.session)).await.unwrap();

    for (key, says) in [("", "none was given"), ("orvu-key-0123", "not one this sidecar knows")] {
        let refused = dp.open_for_test(opening(key, "")).await.unwrap_err();
        assert_eq!(refused.code(), Code::PermissionDenied, "{key:?}");
        assert!(refused.message().contains(says), "{key:?}: {}", refused.message());
    }
}

// A program that came in before the first agent was added would otherwise
// keep working without a key for as long as it stayed connected.
#[tokio::test]
async fn adding_the_first_agent_turns_away_programs_without_a_key() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let opened = dp.open_for_test(opening("", "")).await.unwrap();
    agents::add(&path, "alice").unwrap();
    let refused = dp.execute_for_test(read(&opened.session)).await.unwrap_err();
    // Not UNAUTHENTICATED: a client answers that by opening a new session
    // and trying again, which would go round for ever.
    assert_eq!(refused.code(), Code::PermissionDenied);
}

#[tokio::test]
async fn an_agent_that_is_removed_is_turned_away_at_its_next_call() {
    let (_dir, path) = file();
    let dp = settled_dataplane(&path).await;
    let alice = agents::add(&path, "alice").unwrap();
    let bob = agents::add(&path, "bob").unwrap();
    let mut alices = Vec::new();
    for _ in 0..3 {
        alices.push(dp.open_for_test(opening(&alice, "")).await.unwrap().session);
    }
    let bobs = dp.open_for_test(opening(&bob, "")).await.unwrap().session;
    agents::remove(&path, "alice").unwrap();

    assert_eq!(dp.execute_for_test(read(&alices[0])).await.unwrap_err().code(), Code::PermissionDenied);
    let batch = pb::BatchRequest { session: alices[1].clone(), requests: vec![read("")], request_id: Vec::new() };
    assert_eq!(dp.batch_for_test(batch).await.unwrap_err().code(), Code::PermissionDenied);
    let watch = pb::WatchRequest { session: alices[2].clone(), object: "visits".into(), kind: pb::Kind::Counter as i32 };
    assert_eq!(dp.watch_for_test(watch).await.err().unwrap().code(), Code::PermissionDenied);
    assert_eq!(dp.open_for_test(opening(&alice, "")).await.unwrap_err().code(), Code::PermissionDenied);
    // Being turned away ends each session, so it is not kept in use.
    for session in &alices {
        assert_eq!(dp.execute_for_test(read(session)).await.unwrap_err().code(), Code::Unauthenticated);
    }
    // The others carry on.
    dp.execute_for_test(read(&bobs)).await.unwrap();
}

// A watch that is open when its agent is removed is ended, even if the
// object it watches never changes again.
#[tokio::test]
async fn an_open_watch_ends_when_its_agent_is_removed() {
    use futures::StreamExt;
    let (_dir, path) = file();
    let dp = settled_dataplane(&path).await;
    let alice = agents::add(&path, "alice").unwrap();
    let session = dp.open_for_test(opening(&alice, "")).await.unwrap().session;
    let watch = pb::WatchRequest { session: session.clone(), object: "visits".into(), kind: pb::Kind::Counter as i32 };
    let (mut events, _alive) = dp.watch_for_test(watch).await.unwrap();
    agents::remove(&path, "alice").unwrap();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), events.next()).await;
    let refused = ended.expect("the watch was not ended").expect("the watch ended with no word").unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    // And the session ended with it.
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::Unauthenticated);
}

// A resume token brings back a session only for the key that opened it.
#[tokio::test]
async fn a_session_is_resumed_only_with_the_same_key() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let alice = agents::add(&path, "alice").unwrap();
    let bob = agents::add(&path, "bob").unwrap();
    let first = dp.open_for_test(opening(&alice, "")).await.unwrap();
    let again = dp.open_for_test(opening(&alice, &first.resume_token)).await.unwrap();
    assert_eq!(again.session, first.session);
    let other = dp.open_for_test(opening(&bob, &first.resume_token)).await.unwrap();
    assert_ne!(other.session, first.session);
    assert_eq!(other.agent, "bob");
}

fn orvud(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_orvud")).args(args).output().unwrap()
}

#[test]
fn the_agent_command_adds_lists_and_removes_agents() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let data = data.to_str().unwrap();

    let none = orvud(&["agent", "list", "--local", "--data-dir", data]);
    assert!(none.status.success());
    assert!(String::from_utf8_lossy(&none.stdout).contains("No agents"));

    let added = orvud(&["agent", "add", "alice", "--local", "--data-dir", data]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let said = String::from_utf8_lossy(&added.stdout);
    let key = said.lines().find(|line| line.starts_with("orvu-key-")).expect("the key is printed");
    // The data folder was made, private, and the key works.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(data).unwrap().permissions().mode() & 0o777, 0o700);
    let file = Path::new(data).join(agents::FILE);
    assert_eq!(Agents::from_file(file).admit(key.as_bytes()).unwrap().unwrap().name, "alice");

    let twice = orvud(&["agent", "add", "alice", "--local", "--data-dir", data]);
    assert!(!twice.status.success());
    assert!(String::from_utf8_lossy(&twice.stderr).contains("already an agent called alice"));

    let listed = orvud(&["agent", "list", "--local", "--data-dir", data]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "alice: everything\n");

    let ruled = orvud(&["agent", "allow", "alice", "tasks:write", "notes:read", "--local", "--data-dir", data]);
    assert!(ruled.status.success(), "{}", String::from_utf8_lossy(&ruled.stderr));
    let listed = orvud(&["agent", "list", "--local", "--data-dir", data]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "alice: notes:read tasks:write\n");
    let wrong = orvud(&["agent", "allow", "alice", "tasks:admin", "--local", "--data-dir", data]);
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("is not a rule"));
    assert!(orvud(&["agent", "allow", "alice", "all", "--local", "--data-dir", data]).status.success());
    let listed = orvud(&["agent", "list", "--local", "--data-dir", data]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "alice: everything\n");

    assert!(orvud(&["agent", "remove", "alice", "--local", "--data-dir", data]).status.success());
    let gone = orvud(&["agent", "list", "--local", "--data-dir", data]);
    assert!(String::from_utf8_lossy(&gone.stdout).contains("No agents"));
}

// A sidecar started from a settings file keeps its agents next to it.
#[test]
fn with_a_settings_file_the_agents_are_kept_next_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("orvu.toml");
    std::fs::write(&settings, "").unwrap();
    let added = orvud(&["agent", "add", "alice", "--config", settings.to_str().unwrap()]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert_eq!(agents::read(&dir.path().join(agents::FILE)).unwrap()[0].name, "alice");
}

fn allow(path: &Path, name: &str, rules: &[&str]) {
    agents::allow(path, name, Some(orvud::rules::Rules::parse(rules).unwrap())).unwrap();
}

/// A write of a structured note's field, as the blocks write one.
fn put(session: &str, object: &str) -> pb::Request {
    pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::JsonSet(pb::JsonSet { object: object.into(), path_json: r#"["k"]"#.into(), value_json: r#""v""#.into() })),
        request_id: Vec::new(),
    }
}

fn get(session: &str, object: &str) -> pb::Request {
    pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::JsonGet(pb::JsonGet { object: object.into(), path_json: r#"["k"]"#.into() })),
        request_id: Vec::new(),
    }
}

// An agent with rules may do what they allow and nothing else, and is told
// why, with its rules.
#[tokio::test]
async fn an_agent_with_rules_is_held_to_them() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let key = agents::add(&path, "alice").unwrap();
    allow(&path, "alice", &["notes:read"]);
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    dp.execute_for_test(get(&session, "orvu/crew/note/plan")).await.unwrap();
    let refused = dp.execute_for_test(put(&session, "orvu/crew/note/plan")).await.unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(refused.message(), "alice may not change notes. Its rules are: notes:read");
}

// Rules apply from the next call, and an agent without rules may do
// everything.
#[tokio::test]
async fn new_rules_apply_from_the_next_call() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let key = agents::add(&path, "alice").unwrap();
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    dp.execute_for_test(put(&session, "orvu/crew/note/plan")).await.unwrap();
    allow(&path, "alice", &["tasks:write"]);
    assert_eq!(dp.execute_for_test(put(&session, "orvu/crew/note/plan")).await.unwrap_err().code(), Code::PermissionDenied);
    agents::allow(&path, "alice", None).unwrap();
    dp.execute_for_test(put(&session, "orvu/crew/note/plan")).await.unwrap();
}

// One operation it may not run turns a whole batch away, before any of it
// runs.
#[tokio::test]
async fn a_batch_with_one_forbidden_operation_runs_none() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let key = agents::add(&path, "alice").unwrap();
    allow(&path, "alice", &["notes:write"]);
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    let batch = pb::BatchRequest {
        session: session.clone(),
        requests: vec![put("", "orvu/crew/note/plan"), put("", "orvu/crew/roster")],
        request_id: Vec::new(),
    };
    let refused = dp.batch_for_test(batch).await.unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(refused.message().starts_with("batch request 1: alice may not"), "{}", refused.message());
    let after = dp.execute_for_test(get(&session, "orvu/crew/note/plan")).await.unwrap();
    assert!(matches!(after.result, Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) }))), "{after:?}");
}

// A watch is a read. One the agent may not read is refused, and one open
// when its rules stop allowing it ends.
#[tokio::test]
async fn a_watch_follows_the_rules() {
    use futures::StreamExt;
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    let key = agents::add(&path, "alice").unwrap();
    allow(&path, "alice", &["notes:read"]);
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    let watch = |object: &str| pb::WatchRequest { session: session.clone(), object: object.into(), kind: pb::Kind::Map as i32 };
    assert_eq!(dp.watch_for_test(watch("orvu/crew/roster")).await.err().unwrap().code(), Code::PermissionDenied);
    let (mut events, _alive) = dp.watch_for_test(watch("orvu/crew/note/plan")).await.unwrap();
    allow(&path, "alice", &["tasks:read"]);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), events.next()).await;
    let refused = ended.expect("the watch was not ended").expect("the watch ended with no word").unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
}

// Over the network only agents with keys are let in: not a program without
// one while the agents file names none, and not anybody once it is gone.
#[tokio::test]
async fn over_the_network_only_agents_with_keys_are_let_in() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await.keys_only();
    let refused = dp.open_for_test(opening("", "")).await.unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(refused.message().contains("only agents with keys"), "{}", refused.message());
    let key = agents::add(&path, "alice").unwrap();
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    dp.execute_for_test(read(&session)).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::PermissionDenied);
}

#[test]
fn adding_an_agent_can_write_its_connection_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let file = dir.path().join("alice.orvu");
    let args = |file: &Path| {
        vec!["agent".to_string(), "add".into(), "alice".into(), "--address".into(), "box:7441".into(), "--connection".into(),
            file.to_str().unwrap().into(), "--local".into(), "--data-dir".into(), data.to_str().unwrap().into()]
    };
    let added = Command::new(env!("CARGO_BIN_EXE_orvud")).args(args(&file)).output().unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    // The key is in the file, and not printed.
    assert!(!String::from_utf8_lossy(&added.stdout).contains("orvu-key-"));
    let kept: orvud::network::Connection = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(kept.address, "box:7441");
    assert_eq!(Agents::from_file(data.join(agents::FILE)).admit(kept.key.as_bytes()).unwrap().unwrap().name, "alice");
    assert_eq!(kept.certificate, std::fs::read_to_string(data.join(orvud::network::CERTIFICATE)).unwrap());
    // A file that is there already is not written over, and no agent is
    // added whose key nobody would have.
    let again = Command::new(env!("CARGO_BIN_EXE_orvud")).args(args(&file).into_iter().map(|a| if a == "alice" { "bob".to_string() } else { a }).collect::<Vec<_>>()).output().unwrap();
    assert!(!again.status.success());
    assert_eq!(agents::read(&data.join(agents::FILE)).unwrap().len(), 1);
}

// A watch open over the network ends when the agents file is deleted, as a
// new call would be refused.
#[tokio::test]
async fn over_the_network_an_open_watch_ends_when_the_agents_file_goes() {
    use futures::StreamExt;
    let (_dir, path) = file();
    let dp = dataplane(&path).await.keys_only();
    let key = agents::add(&path, "alice").unwrap();
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    let watch = pb::WatchRequest { session, object: "visits".into(), kind: pb::Kind::Counter as i32 };
    let (mut events, _alive) = dp.watch_for_test(watch).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), events.next()).await;
    let refused = ended.expect("the watch was not ended").expect("the watch ended with no word").unwrap_err();
    assert_eq!(refused.code(), Code::PermissionDenied);
}

// One agent opens many sessions, each watching a counter that it writes over
// and over, so each of its watches keeps sending values and keeps its session
// in use. Another agent's session that sits idle is not pushed out by them.
#[tokio::test]
async fn one_agent_keeping_many_watches_busy_cannot_push_out_another_agent() {
    use futures::StreamExt;
    let (_dir, path) = file();
    let dp = dataplane(&path).await.session_cap(8);
    let alice = agents::add(&path, "alice").unwrap();
    let mallory = agents::add(&path, "mallory").unwrap();
    let quiet = dp.open_for_test(opening(&alice, "")).await.unwrap().session;
    let bump = |session: String| pb::Request {
        session,
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "m".into(), n: 1 })),
        request_id: Vec::new(),
    };
    // The counter exists before anyone watches it, so a new watch sends its
    // value at once.
    dp.execute_for_test(bump(quiet.clone())).await.unwrap();
    let mut streams = Vec::new();
    for n in 1..=20i64 {
        let session = dp.open_for_test(opening(&mallory, "")).await.unwrap().session;
        let req = pb::WatchRequest { session: session.clone(), object: "m".into(), kind: pb::Kind::Counter as i32 };
        let (mut stream, guard) = dp.watch_for_test(req).await.expect("watch opens");
        let _first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await.unwrap();
        streams.push((stream, guard));
        dp.execute_for_test(bump(session)).await.unwrap();
        // Every watch still open sends the new value, which uses its session.
        let mut still = Vec::new();
        for (mut stream, guard) in streams {
            match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await.unwrap() {
                Some(Ok(pb::WatchEvent {
                    event: Some(pb::watch_event::Event::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(v)) })),
                })) if v == n + 1 => still.push((stream, guard)),
                Some(Ok(other)) => panic!("expected {}, got {other:?}", n + 1),
                _ => {}
            }
        }
        streams = still;
    }
    dp.execute_for_test(read(&quiet)).await.expect("alice's idle session was pushed out");
    assert!(streams.len() <= 2, "mallory kept {} sessions", streams.len());
}

// Alice's key leaks, and she is given a new one. Whoever holds the old key
// keeps calling on the sessions it opened. Those calls are turned away, and
// they end those sessions instead of keeping them in use, so the real alice's
// new sessions are not pushed out by them.
#[tokio::test]
async fn sessions_of_a_replaced_key_end_and_do_not_push_out_the_new_ones() {
    let (_dir, path) = file();
    let dp = settled_dataplane(&path).await.session_cap(8);
    let leaked = agents::add(&path, "alice").unwrap();
    let mut old = Vec::new();
    for _ in 0..2 {
        old.push(dp.open_for_test(opening(&leaked, "")).await.unwrap().session);
    }
    agents::remove(&path, "alice").unwrap();
    let alice = agents::add(&path, "alice").unwrap();
    let mut new = Vec::new();
    for _ in 0..2 {
        for id in &old {
            let _ = dp.execute_for_test(read(id)).await;
        }
        new.push(dp.open_for_test(opening(&alice, "")).await.unwrap().session);
    }
    for id in &old {
        let _ = dp.execute_for_test(read(id)).await;
    }
    for id in &new {
        dp.execute_for_test(read(id)).await.expect("one of alice's new sessions was pushed out");
    }
    for id in &old {
        let gone = dp.execute_for_test(read(id)).await.unwrap_err();
        assert_eq!(gone.code(), Code::Unauthenticated, "a session of the old key is still there: {}", gone.message());
    }
}

// While the agents file cannot be read, calls are turned away but the
// sessions stay, since that may pass. A call turned away does not count as
// using its session, though.
#[tokio::test]
async fn a_call_turned_away_while_the_agents_file_is_broken_keeps_its_session_but_does_not_use_it() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await.session_cap(8);
    let alice = agents::add(&path, "alice").unwrap();
    let first = dp.open_for_test(opening(&alice, "")).await.unwrap().session;
    let second = dp.open_for_test(opening(&alice, "")).await.unwrap().session;
    let good = std::fs::read_to_string(&path).unwrap();
    let broken = || std::fs::write(&path, "this is not an agents file\n").unwrap();

    broken();
    assert_eq!(dp.execute_for_test(read(&first)).await.unwrap_err().code(), Code::PermissionDenied);
    std::fs::write(&path, &good).unwrap();
    // The first session is still the one used longest ago, so a third
    // session, past alice's share of two, pushes it out.
    let third = dp.open_for_test(opening(&alice, "")).await.unwrap().session;
    assert_eq!(dp.execute_for_test(read(&first)).await.unwrap_err().code(), Code::Unauthenticated);

    broken();
    assert_eq!(dp.execute_for_test(read(&second)).await.unwrap_err().code(), Code::PermissionDenied);
    std::fs::write(&path, &good).unwrap();
    dp.execute_for_test(read(&second)).await.expect("the session went while the file could not be read");
    dp.execute_for_test(read(&third)).await.unwrap();
}

// An editor that empties the file before writing it again, or a plain
// overwrite, leaves the agents file empty or half-written for a moment.
// Calls in that moment are turned away, but their sessions stay and work
// again once the file is whole.
#[tokio::test]
async fn a_call_made_while_the_agents_file_is_being_rewritten_keeps_its_session() {
    let (_dir, path) = file();
    let dp = dataplane(&path).await;
    agents::add(&path, "alice").unwrap();
    let bob = agents::add(&path, "bob").unwrap();
    let session = dp.open_for_test(opening(&bob, "")).await.unwrap().session;
    let good = std::fs::read_to_string(&path).unwrap();
    let half = &good[..good.find("\nbob ").expect("bob is in the file") + 1];

    // Empty, which on its own would let nobody in.
    std::fs::write(&path, "").unwrap();
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::PermissionDenied);
    std::fs::write(&path, &good).unwrap();
    dp.execute_for_test(read(&session)).await.expect("the session went while the file was empty");

    // Half-written, so that bob is not in it yet.
    std::fs::write(&path, half).unwrap();
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::PermissionDenied);
    std::fs::write(&path, &good).unwrap();
    dp.execute_for_test(read(&session)).await.expect("the session went while the file was half-written");
}

// Once the file has settled, a session of an agent that really was removed
// ends at its next call.
#[tokio::test]
async fn a_session_of_a_removed_agent_ends_once_the_file_has_settled() {
    let (_dir, path) = file();
    let dp = settled_dataplane(&path).await;
    let alice = agents::add(&path, "alice").unwrap();
    agents::add(&path, "bob").unwrap();
    let session = dp.open_for_test(opening(&alice, "")).await.unwrap().session;
    agents::remove(&path, "alice").unwrap();
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::PermissionDenied);
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::Unauthenticated);
}

// Some editors save by moving the old file aside and writing a new one in
// its place, so for a moment there is no agents file at all. Over the
// network a call in that moment is turned away, but its session stays and
// works again once the file is back.
#[tokio::test]
async fn over_the_network_a_call_made_while_an_editor_moves_the_file_aside_keeps_its_session() {
    let (dir, path) = file();
    let dp = dataplane(&path).await.keys_only();
    let key = agents::add(&path, "alice").unwrap();
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    let aside = dir.path().join("agents.backup");
    std::fs::rename(&path, &aside).unwrap();
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::PermissionDenied);
    std::fs::rename(&aside, &path).unwrap();
    dp.execute_for_test(read(&session)).await.expect("the session went while the file was moved aside");
}

// Once the folder has been left alone too, a missing agents file is taken as
// meant, and over the network a session's next call ends it.
#[tokio::test]
async fn over_the_network_a_session_ends_once_the_file_is_gone_for_good() {
    let (_dir, path) = file();
    let dp = settled_dataplane(&path).await.keys_only();
    let key = agents::add(&path, "alice").unwrap();
    let session = dp.open_for_test(opening(&key, "")).await.unwrap().session;
    std::fs::remove_file(&path).unwrap();
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::PermissionDenied);
    assert_eq!(dp.execute_for_test(read(&session)).await.unwrap_err().code(), Code::Unauthenticated);
}
