//! The `own` level of notes and tasks, through the sidecar: an agent with it
//! changes only notes and tasks that are its own as they are when the
//! request comes, and a batch that touches another agent's is turned away
//! whole.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::agents::{self, Agents};
use orvud::pb;
use orvud::pb::request::Op;
use serde_json::{json, Value};
use std::sync::Arc;

struct Sidecar {
    dp: orvud::server::Dataplane,
    client: Arc<Client>,
    path: std::path::PathBuf,
    _folder: tempfile::TempDir,
}

async fn sidecar() -> Sidecar {
    let folder = tempfile::tempdir().unwrap();
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    let path = folder.path().join(agents::FILE);
    let dp = orvud::server::Dataplane::new(client.clone(), ReplicaId(1), "default").admitting(Arc::new(Agents::from_file(path.clone())));
    Sidecar { dp, client, path, _folder: folder }
}

impl Sidecar {
    /// A session for the agent `name`, with `rules`, or none.
    async fn agent(&self, name: &str, rules: &[&str]) -> String {
        let key = agents::add(&self.path, name).unwrap();
        if !rules.is_empty() {
            agents::allow(&self.path, name, Some(orvud::rules::Rules::parse(rules).unwrap())).unwrap();
        }
        let opening = pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: key.into() };
        self.dp.open_for_test(opening).await.unwrap().session
    }

    async fn run(&self, session: &str, op: Op) -> Result<pb::Response, tonic::Status> {
        self.dp.execute_for_test(pb::Request { session: session.into(), op: Some(op), request_id: Vec::new() }).await
    }

    async fn batch(&self, session: &str, ops: Vec<Op>) -> Result<pb::BatchResponse, tonic::Status> {
        let requests = ops.into_iter().map(|op| pb::Request { session: String::new(), op: Some(op), request_id: Vec::new() }).collect();
        self.dp.batch_for_test(pb::BatchRequest { session: session.into(), requests, request_id: Vec::new() }).await
    }

    async fn board(&self) -> Value {
        let value = self.client.json("orvu/crew/tasks").await.unwrap().to_value().await.unwrap();
        serde_json::from_str(&value.to_json_string()).unwrap()
    }

    async fn note(&self, key: &str) -> Vec<String> {
        self.client.mv_map("orvu/crew/notes-mv").await.unwrap().get(key).await.unwrap()
    }
}

fn set(path: Value, value: Value) -> Op {
    Op::JsonSet(pb::JsonSet { object: "orvu/crew/tasks".into(), path_json: path.to_string(), value_json: value.to_string() })
}
fn remove(path: Value) -> Op {
    Op::JsonRemove(pb::JsonRemove { object: "orvu/crew/tasks".into(), path_json: path.to_string() })
}
fn task(by: &str) -> Value {
    json!({"title": "Draft", "state": "todo", "created": 1, "created_by": by})
}
fn put_note(key: &str, by: &str, text: &str) -> Op {
    Op::MvMapPut(pb::MvMapPut { object: "orvu/crew/notes-mv".into(), key: key.into(), value: format!("{by}\u{1f}{text}") })
}
fn remove_note(key: &str) -> Op {
    Op::MvMapRemove(pb::MvMapRemove { object: "orvu/crew/notes-mv".into(), key: key.into() })
}
fn move_task(id: &str) -> Op {
    Op::TreeMoveNode(pb::TreeMoveNode { object: "orvu/crew/tasks-tree".into(), child: id.into(), new_parent: "root".into() })
}
/// What a claim of a task nobody holds writes, as the libraries write it:
/// the name first, so that the rest is written on a task the agent holds.
fn claim(id: &str, by: &str) -> Vec<Op> {
    claim_on("orvu/crew/tasks", id, by)
}
fn claim_on(board: &str, id: &str, by: &str) -> Vec<Op> {
    let set = |path: Value, value: Value| {
        Op::JsonSet(pb::JsonSet { object: board.into(), path_json: path.to_string(), value_json: value.to_string() })
    };
    vec![
        set(json!([id, "claimed_by"]), json!(by)),
        set(json!([id, "seen"]), json!(5)),
        set(json!([id, "lease"]), json!(900)),
        set(json!([id, "state"]), json!("doing")),
    ]
}
fn refused(answer: Result<impl std::fmt::Debug, tonic::Status>) -> String {
    match answer {
        Err(status) if status.code() == tonic::Code::PermissionDenied => status.message().to_string(),
        other => panic!("not refused: {other:?}"),
    }
}

// An agent with tasks:own adds tasks as itself, claims an open task of
// another agent's, and then changes, finishes and gives back only what it
// added or holds.
#[tokio::test]
async fn tasks_own_claims_an_open_task_and_changes_only_its_own() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&boss, set(json!(["t-open"]), task("boss"))).await.unwrap();
    s.run(&boss, set(json!(["t-held"]), task("boss"))).await.unwrap();
    s.batch(&boss, claim("t-held", "boss")).await.unwrap();
    // Its own task: added, put in the tree, changed and finished.
    s.run(&alice, move_task("t-mine")).await.unwrap();
    s.run(&alice, set(json!(["t-mine"]), task("alice"))).await.unwrap();
    s.run(&alice, set(json!(["t-mine", "title"]), json!("Mine"))).await.unwrap();
    s.run(&alice, set(json!(["t-mine", "finished"]), json!({"result": 1, "by": "alice", "id": "f"}))).await.unwrap();
    // An open task of another's: claimed whole, then worked on and given back.
    s.batch(&alice, claim("t-open", "alice")).await.unwrap();
    assert_eq!(s.board().await["t-open"]["claimed_by"], "alice");
    s.run(&alice, set(json!(["t-open", "seen"]), json!(6))).await.unwrap();
    let give_back = vec![
        set(json!(["t-open", "state"]), json!("todo")),
        remove(json!(["t-open", "seen"])),
        remove(json!(["t-open", "lease"])),
        remove(json!(["t-open", "claimed_by"])),
    ];
    s.batch(&alice, give_back).await.unwrap();
    assert!(s.board().await["t-open"].get("claimed_by").is_none());
    // Not open any more once given back to nobody? It is open again, and not
    // hers: she may not change or finish it now.
    let why = refused(s.run(&alice, set(json!(["t-open", "title"]), json!("Mine now"))).await);
    assert!(why.contains("alice may not change a task"), "{why}");
    refused(s.run(&alice, set(json!(["t-open", "finished"]), json!({"result": 1, "by": "alice", "id": "f"}))).await);
    // A task another agent holds: nothing at all.
    refused(s.batch(&alice, claim("t-held", "alice")).await);
    refused(s.run(&alice, set(json!(["t-held", "finished"]), json!({"result": 1, "by": "alice", "id": "f"}))).await);
    refused(s.run(&alice, set(json!(["t-held", "state"]), json!("todo"))).await);
    refused(s.run(&alice, remove(json!(["t-held"]))).await);
    refused(s.run(&alice, remove(json!(["t-open"]))).await);
    refused(s.run(&alice, move_task("t-held")).await);
    // A claim only takes the claim's own fields.
    let mut greedy = claim("t-open", "alice");
    greedy.push(set(json!(["t-open", "title"]), json!("Mine now")));
    refused(s.batch(&alice, greedy).await);
    let board = s.board().await;
    assert_eq!(board["t-held"]["claimed_by"], "boss");
    assert_eq!(board["t-held"]["state"], "doing");
    assert_eq!(board["t-open"]["title"], "Draft");
    assert!(board["t-held"].get("finished").is_none());
}

// A batch that touches its own task and another agent's is turned away
// whole, and nothing of it is made.
#[tokio::test]
async fn a_batch_mixing_its_own_and_anothers_is_refused_whole() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own", "notes:own"]).await;
    s.run(&boss, set(json!(["t-boss"]), task("boss"))).await.unwrap();
    s.run(&alice, set(json!(["t-alice"]), task("alice"))).await.unwrap();
    s.run(&boss, put_note("plan", "boss", "ship")).await.unwrap();
    s.run(&alice, put_note("mine", "alice", "draft")).await.unwrap();
    let why = refused(
        s.batch(&alice, vec![set(json!(["t-alice", "title"]), json!("x")), set(json!(["t-boss", "title"]), json!("x"))]).await,
    );
    assert!(why.contains("batch request 1") && why.contains("nothing was applied"), "{why}");
    refused(s.batch(&alice, vec![put_note("mine", "alice", "y"), put_note("plan", "alice", "y")]).await);
    assert_eq!(s.board().await["t-alice"]["title"], "Draft");
    assert_eq!(s.note("mine").await, ["alice\u{1f}draft"]);
}

// With notes:own an agent adds notes and changes or removes only those whose
// every value is its own.
#[tokio::test]
async fn notes_own_changes_only_its_own_notes() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["notes:own"]).await;
    s.run(&boss, put_note("plan", "boss", "ship")).await.unwrap();
    s.run(&alice, put_note("mine", "alice", "draft")).await.unwrap();
    s.run(&alice, put_note("mine", "alice", "final")).await.unwrap();
    let why = refused(s.run(&alice, put_note("plan", "alice", "mine now")).await);
    assert!(why.contains("alice may not change another agent's note"), "{why}");
    refused(s.run(&alice, remove_note("plan")).await);
    assert_eq!(s.note("plan").await, ["boss\u{1f}ship"]);
    s.run(&alice, remove_note("mine")).await.unwrap();
    assert!(s.note("mine").await.is_empty());
    // A note written by a program that gave no name is nobody's own.
    s.run(&boss, Op::MvMapPut(pb::MvMapPut { object: "orvu/crew/notes-mv".into(), key: "anon".into(), value: "\u{1f}x".into() }))
        .await
        .unwrap();
    refused(s.run(&alice, put_note("anon", "alice", "y")).await);
}

// An agent that may write tasks still cannot change who added a task, by
// writing the task whole or the name alone.
#[tokio::test]
async fn the_agent_that_added_a_task_cannot_be_changed() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:write"]).await;
    s.run(&boss, set(json!(["t-boss"]), task("boss"))).await.unwrap();
    refused(s.run(&alice, set(json!(["t-boss"]), task("alice"))).await);
    refused(s.run(&alice, set(json!(["t-boss", "created_by"]), json!("alice"))).await);
    let mut board = s.board().await;
    board["t-boss"]["created_by"] = json!("alice");
    refused(s.run(&alice, set(json!([]), board)).await);
    assert_eq!(s.board().await["t-boss"]["created_by"], "boss");
    // Everything else stays open to it.
    s.run(&alice, set(json!(["t-boss", "title"]), json!("Edited"))).await.unwrap();
    s.run(&alice, remove(json!(["t-boss"]))).await.unwrap();
}

// What was looked at before a request runs is looked at again as each write
// is made, in the same step as the write: a task another agent claimed in
// between is not changed.
#[tokio::test]
async fn a_task_claimed_in_between_is_not_changed() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    s.run(&boss, set(json!(["t-open"]), task("boss"))).await.unwrap();
    let rules = orvud::rules::Rules::parse(&["tasks:own"]).unwrap();
    let ops = claim("t-open", "alice");
    let guards = orvud::rules::guards(&s.client, &rules, "alice", &ops.iter().collect::<Vec<_>>()).await.unwrap();
    // Bob claims it after alice's request was looked at and before it runs.
    s.batch(&boss, claim("t-open", "bob")).await.unwrap();
    let searcher = orvud::search::Searcher::off();
    for (op, guard) in ops.into_iter().zip(guards) {
        let request = pb::Request { session: String::new(), op: Some(op), request_id: Vec::new() };
        let answer = orvud::exec::dispatch_guarded(&s.client, &searcher, |_| true, request, guard).await.unwrap();
        assert!(matches!(answer.result, Some(pb::response::Result::Refused(_))), "{answer:?}");
    }
    let board = s.board().await;
    assert_eq!(board["t-open"]["claimed_by"], "bob");
    assert_eq!(board["t-open"]["seen"], 5);
}

// A note another agent wrote after the first look is not written over.
#[tokio::test]
async fn a_note_written_in_between_is_not_written_over() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let rules = orvud::rules::Rules::parse(&["notes:own"]).unwrap();
    let op = put_note("plan", "alice", "mine");
    let guards = orvud::rules::guards(&s.client, &rules, "alice", &[&op]).await.unwrap();
    s.run(&boss, put_note("plan", "boss", "ship")).await.unwrap();
    let searcher = orvud::search::Searcher::off();
    let request = pb::Request { session: String::new(), op: Some(op), request_id: Vec::new() };
    let answer = orvud::exec::dispatch_guarded(&s.client, &searcher, |_| true, request, guards[0].clone()).await.unwrap();
    assert!(matches!(answer.result, Some(pb::response::Result::Refused(_))), "{answer:?}");
    assert_eq!(s.note("plan").await, ["boss\u{1f}ship"]);
}

// What a claim writes besides the name is written only once the claim has
// landed, checked in the same step as each write: a request that would set
// an open task's state or lease first, and claim it after, changes nothing.
#[tokio::test]
async fn the_rest_of_a_claim_is_written_only_once_the_claim_has_landed() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&boss, set(json!(["t-open"]), task("boss"))).await.unwrap();
    for first in [set(json!(["t-open", "state"]), json!("done")), set(json!(["t-open", "lease"]), json!(1))] {
        let _ = s.batch(&alice, vec![first, set(json!(["t-open", "claimed_by"]), json!("alice"))]).await;
        let board = s.board().await;
        assert_eq!(board["t-open"], task("boss"), "{board}");
    }
    s.batch(&alice, claim("t-open", "alice")).await.unwrap();
    assert_eq!(s.board().await["t-open"]["state"], "doing");
}

// A claim of a task on one team's board says nothing about a task of the
// same id on another team's.
#[tokio::test]
async fn a_claim_on_one_board_opens_no_task_on_another() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&boss, set(json!(["t-1"]), task("boss"))).await.unwrap();
    let other = |path: Value, value: Value| {
        Op::JsonSet(pb::JsonSet { object: "orvu/other/tasks".into(), path_json: path.to_string(), value_json: value.to_string() })
    };
    s.run(&boss, other(json!(["t-1"]), task("boss"))).await.unwrap();
    let crossed = vec![set(json!(["t-1", "claimed_by"]), json!("alice")), other(json!(["t-1", "state"]), json!("done"))];
    let _ = s.batch(&alice, crossed).await;
    let value = s.client.json("orvu/other/tasks").await.unwrap().to_value().await.unwrap();
    let theirs: Value = serde_json::from_str(&value.to_json_string()).unwrap();
    assert_eq!(theirs["t-1"], task("boss"));
}

// Once a write of a batch is turned away as it is made, the rest of the batch
// is not made either.
#[tokio::test]
async fn a_batch_stops_at_a_write_turned_away_as_it_is_made() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&boss, set(json!(["t-open"]), task("boss"))).await.unwrap();
    s.run(&alice, set(json!(["t-mine"]), task("alice"))).await.unwrap();
    let rules = orvud::rules::Rules::parse(&["tasks:own"]).unwrap();
    let ops = vec![set(json!(["t-open", "claimed_by"]), json!("alice")), set(json!(["t-mine", "title"]), json!("Changed"))];
    let guards = orvud::rules::guards(&s.client, &rules, "alice", &ops.iter().collect::<Vec<_>>()).await.unwrap();
    s.batch(&boss, claim("t-open", "bob")).await.unwrap();
    let requests = ops.into_iter().map(|op| pb::Request { session: String::new(), op: Some(op), request_id: Vec::new() }).collect();
    let stopping = tokio_util::sync::CancellationToken::new();
    let answers = orvud::exec::dispatch_batch_guarded(&s.client, "", requests, stopping, guards, Vec::new()).await.unwrap();
    for answer in &answers {
        assert!(matches!(answer.result, Some(pb::response::Result::Refused(_))), "{answer:?}");
    }
    assert_eq!(s.board().await["t-mine"]["title"], "Draft");
}

// What an agent with tasks:own writes on a task it holds names only itself,
// or nobody: it cannot hand the task to another agent, real or made up, or
// say another finished or changed it.
#[tokio::test]
async fn a_holder_names_only_itself_on_a_task() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&boss, set(json!(["t-1"]), task("boss"))).await.unwrap();
    s.batch(&alice, claim("t-1", "alice")).await.unwrap();
    for (field, value) in [
        ("claimed_by", json!("ghost")),
        ("claimed_by", json!("boss")),
        ("finished_by", json!("boss")),
        ("finished", json!({"result": 1, "by": "boss", "id": "f"})),
    ] {
        refused(s.run(&alice, set(json!(["t-1", field]), value)).await);
    }
    // Who last changed it is the sidecar's to write: what it writes there
    // is not kept.
    s.run(&alice, set(json!(["t-1", "updated_by"]), json!("boss"))).await.unwrap();
    assert_ne!(s.board().await["t-1"]["updated_by"], "boss");
    refused(s.run(&alice, set(json!(["t-1", "finished", "by"]), json!("boss"))).await);
    assert_eq!(s.board().await["t-1"]["claimed_by"], "alice");
    // Its own name, or nobody's.
    s.run(&alice, set(json!(["t-1", "updated_by"]), json!("alice"))).await.unwrap();
    s.run(&alice, set(json!(["t-1", "finished"]), json!({"result": null, "by": null, "id": "f"}))).await.unwrap();
    s.run(&alice, remove(json!(["t-1", "finished"]))).await.unwrap();
    s.run(&alice, remove(json!(["t-1", "claimed_by"]))).await.unwrap();
    // Its own task, written whole, names nobody else either.
    s.run(&alice, set(json!(["t-mine"]), task("alice"))).await.unwrap();
    let mut handed = task("alice");
    handed["claimed_by"] = json!("ghost");
    refused(s.run(&alice, set(json!(["t-mine"]), handed)).await);
}

fn top_up(n: u64) -> Op {
    Op::BoundedIncrement(pb::BoundedIncrement { object: "orvu/crew/budget/b1".into(), n })
}
fn topped_line(n: u64) -> Op {
    Op::CartAdd(pb::CartAdd { object: "orvu/crew/budget-topped/b1".into(), item: "alice".into(), qty: n })
}

impl Sidecar {
    /// What budget b1 holds, and what its record says alice topped it up by.
    async fn topped(&self) -> (i64, i64) {
        let left = match self.client.exists("orvu/crew/budget/b1").await {
            true => self.client.bounded_counter("orvu/crew/budget/b1").await.unwrap().value().await.unwrap(),
            false => 0,
        };
        let line = match self.client.exists("orvu/crew/budget-topped/b1").await {
            true => self.client.cart("orvu/crew/budget-topped/b1").await.unwrap().quantity("alice").await.unwrap(),
            false => 0,
        };
        (left as i64, line as i64)
    }
}

// A top-up and its line in the record of who topped up land together, or
// neither does, however the batch they are in is cut short: by a write that
// is turned away, by the size of the replies, or by the sidecar stopping.
#[tokio::test]
async fn a_top_up_and_its_line_land_together() {
    // Every way, in both orders, is tried before any is judged.
    let mut apart = Vec::new();
    for line_first in [true, false] {
        let pair = |middle: Vec<Op>| {
            let mut ops = vec![if line_first { topped_line(100) } else { top_up(100) }];
            ops.extend(middle);
            ops.push(if line_first { top_up(100) } else { topped_line(100) });
            ops
        };
        // A write turned away part way.
        let s = sidecar().await;
        let boss = s.agent("boss", &[]).await;
        let alice = s.agent("alice", &["tasks:own", "budgets:top-up"]).await;
        s.run(&boss, set(json!(["t"]), task("boss"))).await.unwrap();
        let cut = pair(vec![set(json!(["t", "state"]), json!("doing")), set(json!(["t", "claimed_by"]), json!("alice"))]);
        let _ = s.batch(&alice, cut).await;
        let (left, line) = s.topped().await;
        if left != line {
            apart.push(format!("turned away, line first {line_first}: {left} topped up, {line} on the record"));
        }
        // The replies grown past their limit part way.
        let s = sidecar().await;
        let boss = s.agent("boss", &[]).await;
        let alice = s.agent("alice", &["budgets:top-up", "notes:read"]).await;
        s.run(&boss, put_note("big", "boss", &"x".repeat(8192))).await.unwrap();
        let read = || Op::MvMapGet(pb::MvMapGet { object: "orvu/crew/notes-mv".into(), key: "big".into() });
        let _ = s.batch(&alice, pair((0..600).map(|_| read()).collect())).await;
        let (left, line) = s.topped().await;
        if left != line {
            apart.push(format!("replies too large, line first {line_first}: {left} topped up, {line} on the record"));
        }
        // The sidecar stopping right after the first of the pair: as each is
        // made, the line goes with the top-up.
        let s = sidecar().await;
        let rules = orvud::rules::Rules::parse(&["budgets:top-up"]).unwrap();
        let ops = pair(Vec::new());
        let guards = orvud::rules::guards(&s.client, &rules, "alice", &ops.iter().collect::<Vec<_>>()).await.unwrap();
        let first = vec![pb::Request { session: String::new(), op: Some(ops[0].clone()), request_id: Vec::new() }];
        let stopping = tokio_util::sync::CancellationToken::new();
        orvud::exec::dispatch_batch_guarded(&s.client, "", first, stopping, guards[..1].to_vec(), Vec::new()).await.unwrap();
        let (left, line) = s.topped().await;
        if left != line {
            apart.push(format!("stopped, line first {line_first}: {left} topped up, {line} on the record"));
        }
    }
    assert!(apart.is_empty(), "{apart:#?}");
}

// The task board is changed one task at a time, by every agent with rules:
// a path that starts anywhere but at a task's id is turned away, so the board
// cannot be made a list, or written whole.
#[tokio::test]
async fn the_board_is_changed_one_task_at_a_time() {
    for rules in ["tasks:own", "tasks:write"] {
        let s = sidecar().await;
        let boss = s.agent("boss", &[]).await;
        let alice = s.agent("alice", &[rules]).await;
        s.run(&boss, set(json!(["t-1"]), task("boss"))).await.unwrap();
        s.run(&alice, set(json!(["t-mine"]), task("alice"))).await.unwrap();
        let insert = |path: Value| {
            Op::JsonInsert(pb::JsonInsert { object: "orvu/crew/tasks".into(), path_json: path.to_string(), index: 0, value_json: "\"x\"".into() })
        };
        for op in [
            set(json!([0]), json!("x")),
            remove(json!([0])),
            set(json!([]), json!({})),
            insert(json!([])),
            set(json!(["t-mine", 0]), json!("x")),
            remove(json!(["t-mine", 0])),
        ] {
            let why = refused(s.run(&alice, op.clone()).await);
            assert!(why.contains("alice may not"), "{rules} {op:?}: {why}");
        }
        let board = s.board().await;
        assert_eq!(board["t-1"], task("boss"), "{rules}");
        assert_eq!(board["t-mine"], task("alice"), "{rules}");
    }
}

// Whoever with rules changes a task is named as the one that last changed
// it, whether or not it says so itself.
#[tokio::test]
async fn the_agent_that_last_changed_a_task_is_named_by_the_sidecar() {
    let s = sidecar().await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    let bob = s.agent("bob", &["tasks:write"]).await;
    s.run(&alice, set(json!(["t-1"]), task("alice"))).await.unwrap();
    assert!(s.board().await["t-1"].get("updated_by").is_none(), "adding is not changing");
    s.run(&bob, set(json!(["t-1", "title"]), json!("Bob's"))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "bob");
    s.run(&alice, set(json!(["t-1", "title"]), json!("Alice's"))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "alice");
    s.run(&bob, remove(json!(["t-1", "title"]))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "bob");
}

// Deeper writes under the names on a task are names too.
#[tokio::test]
async fn names_written_deeper_are_checked_too() {
    let s = sidecar().await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&alice, set(json!(["t-1"]), task("alice"))).await.unwrap();
    s.run(&alice, set(json!(["t-1", "finished"]), json!({"result": 1, "by": "alice", "id": "f"}))).await.unwrap();
    for path in [json!(["t-1", "claimed_by", "x"]), json!(["t-1", "finished_by", "x"]), json!(["t-1", "finished", "by", "x"])] {
        refused(s.run(&alice, set(path, json!("boss"))).await);
    }
    s.run(&alice, set(json!(["t-1", "finished", "result"]), json!(2))).await.unwrap();
}

// A top-up the budget could not hold whole is turned away, so that the record
// never says more was added than was.
#[tokio::test]
async fn a_top_up_the_budget_cannot_hold_whole_is_turned_away() {
    let s = sidecar().await;
    let alice = s.agent("alice", &["budgets:top-up"]).await;
    let pair = |n: u64| vec![top_up(n), topped_line(n)];
    s.batch(&alice, pair(i64::MAX as u64 - 5)).await.unwrap();
    let answers = s.batch(&alice, pair(10)).await.unwrap();
    assert!(matches!(answers.responses[0].result, Some(pb::response::Result::Refused(_))), "{answers:?}");
    let answers = s.batch(&alice, pair(u64::MAX)).await.unwrap();
    assert!(matches!(answers.responses[0].result, Some(pb::response::Result::Refused(_))), "{answers:?}");
    s.batch(&alice, pair(5)).await.unwrap();
    let (left, line) = s.topped().await;
    assert_eq!((left, line), (i64::MAX, i64::MAX));
}

// For an agent with rules, only the sidecar writes who last changed a task:
// what the agent writes there itself is not kept, and a whole task written
// over one that is there is named too.
#[tokio::test]
async fn only_the_sidecar_names_who_last_changed_a_task() {
    let s = sidecar().await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    let bob = s.agent("bob", &["tasks:write"]).await;
    let mut added = task("alice");
    added["updated_by"] = json!("boss");
    s.run(&alice, set(json!(["t-1"]), added)).await.unwrap();
    assert!(s.board().await["t-1"].get("updated_by").is_none(), "adding is not changing");
    s.batch(&bob, vec![set(json!(["t-1", "title"]), json!("Bob's")), set(json!(["t-1", "updated_by"]), json!("boss"))]).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "bob");
    s.batch(&alice, vec![set(json!(["t-1", "title"]), json!("Alice's")), remove(json!(["t-1", "updated_by"]))]).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "alice");
    s.run(&alice, set(json!(["t-1", "updated_by", "x"]), json!("boss"))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "alice");
    s.run(&bob, set(json!(["t-1", "notes"]), json!("Bob's"))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "bob");
    // A task written whole over one that is there is named too.
    let mut whole = task("alice");
    whole["updated_by"] = json!("boss");
    s.run(&alice, set(json!(["t-1"]), whole)).await.unwrap();
    assert_eq!(s.board().await["t-1"]["updated_by"], "alice");
}

// A change and the name of who made it are one change, made in one step: the
// engine reports it once, and nothing can come between the two.
#[tokio::test]
async fn a_change_and_its_name_are_one_change() {
    let s = sidecar().await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&alice, set(json!(["t-1"]), task("alice"))).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let reports = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = reports.clone();
    let _following = s.client.on_change(move |object| {
        if object == "orvu/crew/tasks" {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    s.run(&alice, set(json!(["t-1", "title"]), json!("Changed"))).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(reports.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(s.board().await["t-1"]["updated_by"], "alice");
}

// The old way of saying who finished a task is a name too.
#[tokio::test]
async fn the_old_finisher_field_names_only_the_agent_itself() {
    let s = sidecar().await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    s.run(&alice, set(json!(["t-1"]), task("alice"))).await.unwrap();
    refused(s.run(&alice, set(json!(["t-1", "finished_by"]), json!("boss"))).await);
    refused(s.run(&alice, set(json!(["t-1", "finished_by", "x"]), json!("boss"))).await);
    let mut whole = task("alice");
    whole["finished_by"] = json!("boss");
    refused(s.run(&alice, set(json!(["t-1"]), whole)).await);
    s.run(&alice, set(json!(["t-1", "finished_by"]), json!("alice"))).await.unwrap();
}

// The record of a firm task taken over names two agents, and an agent with
// tasks:own takes no firm task over, so it writes none.
#[tokio::test]
async fn tasks_own_writes_no_record_of_a_takeover() {
    let s = sidecar().await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    let bob = s.agent("bob", &["tasks:write"]).await;
    s.run(&alice, set(json!(["t-1"]), task("alice"))).await.unwrap();
    let record = json!({"by": "alice", "from": "boss", "at": 1});
    refused(s.run(&alice, set(json!(["t-1", "taken_over"]), record.clone())).await);
    refused(s.run(&alice, set(json!(["t-1", "taken_over", "from"]), json!("boss"))).await);
    let mut whole = task("alice");
    whole["taken_over"] = record.clone();
    refused(s.run(&alice, set(json!(["t-1"]), whole)).await);
    // An agent that may change tasks writes it, as the firm tasks do.
    s.run(&bob, set(json!(["t-1", "taken_over"]), record)).await.unwrap();
}

// A name under orvu/ whose team is empty, or not a name a team can have,
// belongs to no block, for an agent with rules: it is no way round the
// rules of the block it looks like.
#[tokio::test]
async fn an_object_of_no_team_is_no_way_round_the_rules() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own", "notes:own"]).await;
    let on = |board: &str, path: Value, value: Value| {
        Op::JsonSet(pb::JsonSet { object: board.into(), path_json: path.to_string(), value_json: value.to_string() })
    };
    let mut held = task("boss");
    held["claimed_by"] = json!("boss");
    for board in ["orvu//tasks", "orvu/.x/tasks", "orvu/a b/tasks", "orvu/-x/tasks"] {
        s.run(&boss, on(board, json!(["t-1"]), held.clone())).await.unwrap();
        for (path, value) in [
            (json!(["t-1", "title"]), json!("Mine now")),
            (json!(["t-1", "updated_by"]), json!("ghost")),
            (json!(["t-1", "claimed_by"]), json!("alice")),
        ] {
            refused(s.run(&alice, on(board, path, value)).await);
        }
    }
    let notes = "orvu//notes-mv";
    let boss_note = Op::MvMapPut(pb::MvMapPut { object: notes.into(), key: "plan".into(), value: "boss\u{1f}ship".into() });
    s.run(&boss, boss_note).await.unwrap();
    refused(s.run(&alice, Op::MvMapRemove(pb::MvMapRemove { object: notes.into(), key: "plan".into() })).await);
    let value = s.client.json("orvu//tasks").await.unwrap().get(&[orvu_crdt::Seg::Key("t-1".into())]).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&value.unwrap().to_json_string()).unwrap(), held);
}

// A change of a task that is not there names nobody, and leaves no task
// behind.
#[tokio::test]
async fn a_change_of_a_missing_task_leaves_nothing_behind() {
    for rules in ["tasks:own", "tasks:write"] {
        let s = sidecar().await;
        let alice = s.agent("alice", &[rules]).await;
        s.run(&alice, set(json!(["t-1"]), task("alice"))).await.unwrap();
        let _ = s.run(&alice, remove(json!(["ghost", "title"]))).await;
        assert!(s.board().await.get("ghost").is_none(), "{rules}: {}", s.board().await);
        s.run(&alice, set(json!(["ghost"]), task("alice"))).await.unwrap();
        assert_eq!(s.board().await["ghost"], task("alice"), "{rules}");
    }
}

// A write inside a task that is not there is turned away, for every agent
// with rules, at any depth: it would leave a task behind that nobody added.
#[tokio::test]
async fn a_write_inside_a_missing_task_is_turned_away() {
    for rules in ["tasks:own", "tasks:write"] {
        let s = sidecar().await;
        let alice = s.agent("alice", &[rules]).await;
        s.run(&alice, set(json!(["t-1"]), task("alice"))).await.unwrap();
        for op in [
            set(json!(["ghost", "title"]), json!("x")),
            set(json!(["ghost", "tags", "a"]), json!("x")),
            remove(json!(["ghost", "title"])),
            remove(json!(["ghost", "tags", "a"])),
        ] {
            let why = refused(s.run(&alice, op.clone()).await);
            assert!(why.contains("a task that is not there"), "{rules} {op:?}: {why}");
        }
        assert!(s.board().await.get("ghost").is_none(), "{rules}");
        // Added whole, it is there, and its fields can be written.
        s.run(&alice, set(json!(["ghost"]), task("alice"))).await.unwrap();
        s.run(&alice, set(json!(["ghost", "title"]), json!("x"))).await.unwrap();
    }
}

// The same, when the task is taken away between the first look and the
// write: the write is turned away as it is made.
#[tokio::test]
async fn a_task_removed_in_between_is_not_written_into() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    s.run(&boss, set(json!(["t-1"]), task("boss"))).await.unwrap();
    let rules = orvud::rules::Rules::parse(&["tasks:write"]).unwrap();
    let op = set(json!(["t-1", "title"]), json!("x"));
    let guards = orvud::rules::guards(&s.client, &rules, "alice", &[&op]).await.unwrap();
    s.run(&boss, remove(json!(["t-1"]))).await.unwrap();
    let searcher = orvud::search::Searcher::off();
    let request = pb::Request { session: String::new(), op: Some(op), request_id: Vec::new() };
    let answer = orvud::exec::dispatch_guarded(&s.client, &searcher, |_| true, request, guards[0].clone()).await.unwrap();
    assert!(matches!(answer.result, Some(pb::response::Result::Refused(_))), "{answer:?}");
    assert!(s.board().await.get("t-1").is_none());
}

// What sits where a task would be but is not one, such as null, a number or
// some text, counts as no task: nothing is written inside it.
#[tokio::test]
async fn what_is_not_a_task_counts_as_none() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:write"]).await;
    for (id, there) in [("ghost", json!(null)), ("seven", json!(7)), ("word", json!("x"))] {
        s.run(&boss, set(json!([id]), there.clone())).await.unwrap();
        let why = refused(s.run(&alice, set(json!([id, "title"]), json!("x"))).await);
        assert!(why.contains("a task that is not there"), "{id}: {why}");
        assert_eq!(s.board().await[id], there, "{id}");
    }
}

// A task written whole over one that is there, by an agent with rules, keeps
// the ticket numbers written on it. A firm task's number is the highest of
// them, and must never go down.
#[tokio::test]
async fn a_task_written_whole_keeps_its_ticket_numbers() {
    let s = sidecar().await;
    let bob = s.agent("bob", &["tasks:write"]).await;
    let mut firm = task("bob");
    firm["tickets"] = json!({"5": true, "9": true});
    s.run(&bob, set(json!(["t-1"]), firm)).await.unwrap();
    s.run(&bob, set(json!(["t-1"]), task("bob"))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["tickets"], json!({"5": true, "9": true}));
    let mut more = task("bob");
    more["tickets"] = json!({"12": true});
    s.run(&bob, set(json!(["t-1"]), more)).await.unwrap();
    assert_eq!(s.board().await["t-1"]["tickets"], json!({"5": true, "9": true, "12": true}));
    let mut odd = task("bob");
    odd["tickets"] = json!("none");
    s.run(&bob, set(json!(["t-1"]), odd)).await.unwrap();
    assert_eq!(s.board().await["t-1"]["tickets"], json!({"5": true, "9": true, "12": true}));
    // A task added whole is written as it is.
    s.run(&bob, set(json!(["t-2"]), task("bob"))).await.unwrap();
    assert!(s.board().await["t-2"].get("tickets").is_none());
}

// Ticket numbers are only ever added to a task by an agent with rules: one
// that is written cannot be changed or taken off, so a firm task's number
// never goes down. Writing the whole of them adds the new ones.
#[tokio::test]
async fn ticket_numbers_on_a_task_are_only_ever_added() {
    let s = sidecar().await;
    let bob = s.agent("bob", &["tasks:write"]).await;
    let carol = s.agent("carol", &["tasks:write"]).await;
    let mut firm = task("bob");
    firm["tickets"] = json!({"5": true});
    s.run(&bob, set(json!(["t-1"]), firm)).await.unwrap();
    for op in [
        set(json!(["t-1", "tickets"]), json!("none")),
        set(json!(["t-1", "tickets"]), json!([])),
        remove(json!(["t-1", "tickets"])),
        remove(json!(["t-1", "tickets", "5"])),
        set(json!(["t-1", "tickets", "5"]), json!(false)),
        set(json!(["t-1", "tickets", "5", "x"]), json!(1)),
        set(json!(["t-1", "tickets", "7", "x"]), json!(1)),
    ] {
        let why = refused(s.run(&bob, op.clone()).await);
        assert!(why.contains("bob may not"), "{op:?}: {why}");
    }
    assert_eq!(s.board().await["t-1"]["tickets"], json!({"5": true}));
    s.run(&bob, set(json!(["t-1", "tickets", "7"]), json!(true))).await.unwrap();
    s.run(&bob, set(json!(["t-1", "tickets"]), json!({}))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["tickets"], json!({"5": true, "7": true}));
    assert_eq!(s.board().await["t-1"]["updated_by"], "bob");
    // Another agent writing them whole is named as the one that last
    // changed the task.
    s.run(&carol, set(json!(["t-1", "tickets"]), json!({"5": false, "9": true}))).await.unwrap();
    assert_eq!(s.board().await["t-1"]["tickets"], json!({"5": true, "7": true, "9": true}));
    assert_eq!(s.board().await["t-1"]["updated_by"], "carol");
}

// A task whose "finished" mark is false is not finished, as the libraries
// read it: an agent with tasks:own may claim it while nobody holds it.
#[tokio::test]
async fn a_task_marked_finished_false_is_open() {
    let s = sidecar().await;
    let boss = s.agent("boss", &[]).await;
    let alice = s.agent("alice", &["tasks:own"]).await;
    for (id, mark) in [("t-false", json!(false)), ("t-null", json!(null))] {
        let mut open = task("boss");
        open["finished"] = mark;
        s.run(&boss, set(json!([id]), open)).await.unwrap();
        s.batch(&alice, claim(id, "alice")).await.unwrap();
        assert_eq!(s.board().await[id]["claimed_by"], "alice");
    }
    let mut done = task("boss");
    done["finished"] = json!({"result": 1, "by": "boss", "id": "f"});
    s.run(&boss, set(json!(["t-done"]), done)).await.unwrap();
    refused(s.batch(&alice, claim("t-done", "alice")).await);
}
