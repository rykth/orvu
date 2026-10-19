//! Search by meaning, through the sidecar, with the made-up model: the
//! notes, structured notes and documents of a team are found by what they
//! mean, changes show within a moment, and an agent finds only what it may
//! read.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::agents::{self, Agents};
use orvud::pb;
use orvud::pb::request::Op;
use orvud::search::{ModelFrom, Pace, Searcher};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A sidecar that searches with the made-up model, and a session on it.
struct Sidecar {
    dp: orvud::server::Dataplane,
    searcher: Arc<Searcher>,
    session: String,
    _folder: tempfile::TempDir,
    _stop: tokio_util::sync::DropGuard,
}

async fn sidecar(from: impl FnOnce(&Path) -> ModelFrom) -> Sidecar {
    sidecar_paced(from, Pace::default()).await
}

async fn sidecar_paced(from: impl FnOnce(&Path) -> ModelFrom, pace: Pace) -> Sidecar {
    let folder = tempfile::tempdir().unwrap();
    let model = folder.path().join("model");
    std::fs::create_dir(&model).unwrap();
    orvu_search::tiny::write(&model).unwrap();
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    let stop = CancellationToken::new();
    let searcher = Searcher::start_paced(client.clone(), from(folder.path()), stop.clone(), pace);
    let file = folder.path().join(agents::FILE);
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default")
        .admitting(Arc::new(Agents::from_file(file)))
        .searching(searcher.clone());
    let session = open(&dp, "").await;
    Sidecar { dp, searcher, session, _folder: folder, _stop: stop.drop_guard() }
}

async fn searching() -> Sidecar {
    sidecar(|folder| ModelFrom::Folder(folder.join("model"))).await
}

/// A sidecar that searches, keeping up at `pace`.
async fn paced(pace: Pace) -> Sidecar {
    sidecar_paced(|folder| ModelFrom::Folder(folder.join("model")), pace).await
}

/// A pace at which no report of a change is followed, everything is read
/// again every `again`, and once reports were dropped, at most once in
/// `resting`.
fn unfollowed(again: Duration, resting: Duration) -> Pace {
    Pace { again, resting, following: false }
}

const LONG: Duration = Duration::from_secs(3600);

async fn open(dp: &orvud::server::Dataplane, key: &str) -> String {
    let opening = pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: key.into() };
    dp.open_for_test(opening).await.unwrap().session
}

impl Sidecar {
    async fn run(&self, session: &str, op: Op) -> pb::Response {
        self.dp.execute_for_test(pb::Request { session: session.into(), op: Some(op), request_id: Vec::new() }).await.unwrap()
    }

    /// Puts what the Python and TypeScript libraries put for a simple note,
    /// from a program that gave no name.
    async fn note(&self, name: &str, value: &str) {
        self.listed("notes-mv", "mvmap").await;
        let put = pb::MvMapPut { object: "orvu/crew/notes-mv".into(), key: name.into(), value: format!("\u{1f}{value}") };
        self.run(&self.session, Op::MvMapPut(put)).await;
    }

    async fn structured(&self, name: &str, value: &str) {
        self.listed(&format!("note/{name}"), "json").await;
        let object = format!("orvu/crew/note/{name}");
        let set = pb::JsonSet { object, path_json: "[]".into(), value_json: value.into() };
        self.run(&self.session, Op::JsonSet(set)).await;
    }

    async fn document(&self, name: &str, text: &str) {
        self.document_of("crew", name, text).await;
    }

    /// Puts a task on the board as the libraries do, with its notes if it
    /// has any.
    async fn task(&self, id: &str, title: &str, notes: Option<&str>) {
        self.listed("tasks", "json").await;
        let mut fields = serde_json::json!({"title": title, "state": "todo", "created": 1});
        if let Some(notes) = notes {
            fields["notes"] = notes.into();
        }
        let set = pb::JsonSet { object: "orvu/crew/tasks".into(), path_json: format!("[{id:?}]"), value_json: fields.to_string() };
        self.run(&self.session, Op::JsonSet(set)).await;
    }

    async fn document_of(&self, team: &str, name: &str, text: &str) {
        self.listed_of(team, &format!("doc/{name}"), "richtext").await;
        let insert = pb::RichtextInsert { object: format!("orvu/{team}/doc/{name}"), index: 0, text: text.into() };
        self.run(&self.session, Op::RichtextInsert(insert)).await;
    }

    async fn listed(&self, short: &str, kind: &str) {
        self.listed_of("crew", short, kind).await;
    }

    async fn listed_of(&self, team: &str, short: &str, kind: &str) {
        let put = pb::MapPut { object: format!("orvu/{team}/index"), key: short.into(), value: kind.into() };
        self.run(&self.session, Op::MapPut(put)).await;
    }

    /// Waits until the searcher has read at least `more` objects more than
    /// `since`, for up to five seconds.
    async fn read_more(&self, since: u64, more: u64) {
        for _ in 0..100 {
            if self.searcher.counts().0 >= since + more {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the searcher read {} objects, not {more} more than {since}", self.searcher.counts().0);
    }

    async fn search_as(&self, session: &str, query: &str, limit: u32) -> pb::Response {
        let search = pb::Search { object: "orvu/crew/index".into(), query: query.into(), limit };
        self.run(session, Op::Search(search)).await
    }

    /// What a search found, as (kind, name) pairs, the closest first.
    async fn found(&self, query: &str) -> Vec<(String, String)> {
        hits(&self.search_as(&self.session, query, 0).await)
            .iter()
            .map(|hit| (hit["kind"].as_str().unwrap().to_string(), hit["name"].as_str().unwrap().to_string()))
            .collect()
    }
}

fn hits(response: &pb::Response) -> Vec<Value> {
    match &response.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Json(text)) })) => {
            serde_json::from_str::<Vec<Value>>(text).unwrap()
        }
        other => panic!("not a list of what was found: {other:?}"),
    }
}

fn failed(response: &pb::Response) -> (i32, String) {
    match &response.result {
        Some(pb::response::Result::Failed(f)) => (f.code, f.detail.clone()),
        other => panic!("not a failure: {other:?}"),
    }
}

fn pairs(found: &[(&str, &str)]) -> Vec<(String, String)> {
    found.iter().map(|(kind, name)| (kind.to_string(), name.to_string())).collect()
}

/// Asks `ask` again until it finds `expected`, in any order, for up to five
/// seconds.
async fn becomes<F, Fut>(mut expected: Vec<(String, String)>, ask: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    expected.sort();
    let sorted = || async {
        let mut got = ask().await;
        got.sort();
        got
    };
    let mut got = sorted().await;
    for _ in 0..100 {
        if got == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        got = sorted().await;
    }
    assert_eq!(got, expected);
}

// Notes, structured notes and documents are found by what they mean, the
// closest first, each with what kind it is and where it is.
#[tokio::test]
async fn a_teams_notes_and_documents_are_found_by_meaning() {
    let s = searching().await;
    s.note("refund-policy", "customers get their money back").await;
    s.note("deploy-date", "friday").await;
    s.structured("q3", r#"{"sales": "rose", "revenue": "grew"}"#).await;
    s.document("handbook", "Ship on friday.\n\nA refund is paid and money back within days.").await;
    assert_eq!(s.found("refund").await, pairs(&[("document", "handbook"), ("note", "refund-policy")]));
    assert_eq!(s.found("income").await, pairs(&[("structured", "q3")]));
    let hit = hits(&s.search_as(&s.session, "refund", 1).await);
    assert_eq!(hit.len(), 1);
    assert_eq!(hit[0]["paragraph"], 2);
    assert_eq!(hit[0]["text"], "A refund is paid and money back within days.");
    assert!(hit[0]["score"].as_f64().unwrap() > 0.0);
    assert!(s.found("the cat").await.is_empty(), "nothing in common, nothing found");
}

// A change after a team was first searched is found within a moment, and
// something taken away is no longer found.
#[tokio::test]
async fn changes_are_found_within_a_moment() {
    let s = searching().await;
    s.note("deploy-date", "friday").await;
    assert!(s.found("refund").await.is_empty());
    s.note("refund-policy", "money back").await;
    s.document("draft", "repay").await;
    becomes(pairs(&[("note", "refund-policy"), ("document", "draft")]), || s.found("refund")).await;
    let remove = pb::MvMapRemove { object: "orvu/crew/notes-mv".into(), key: "refund-policy".into() };
    s.run(&s.session, Op::MvMapRemove(remove)).await;
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
}

// Notes kept the way they used to be are moved over by the first search,
// and found there.
#[tokio::test]
async fn notes_kept_the_old_way_are_moved_over_and_found() {
    let s = searching().await;
    s.listed("notes", "map").await;
    for name in ["refund-policy", "deploy-date"] {
        let put = pb::MapPut { object: "orvu/crew/notes".into(), key: name.into(), value: "money back".into() };
        s.run(&s.session, Op::MapPut(put)).await;
    }
    becomes(pairs(&[("note", "deploy-date"), ("note", "refund-policy")]), || s.found("refund")).await;
}

// An agent finds only what its rules let it read.
#[tokio::test]
async fn an_agent_finds_only_what_it_may_read() {
    let s = searching().await;
    s.note("refund-policy", "money back").await;
    s.document("handbook", "refund").await;
    let path = s._folder.path().join(agents::FILE);
    let key = agents::add(&path, "alice").unwrap();
    agents::allow(&path, "alice", Some(orvud::rules::Rules::parse(&["documents:read"]).unwrap())).unwrap();
    let alice = open(&s.dp, &key).await;
    let found = hits(&s.search_as(&alice, "refund", 0).await);
    assert_eq!(found.iter().map(|hit| hit["name"].as_str().unwrap()).collect::<Vec<_>>(), ["handbook"]);
    let key = agents::add(&path, "carol").unwrap();
    agents::allow(&path, "carol", Some(orvud::rules::Rules::parse(&["notes:read"]).unwrap())).unwrap();
    let carol = open(&s.dp, &key).await;
    let found = hits(&s.search_as(&carol, "refund", 0).await);
    assert_eq!(found.iter().map(|hit| hit["name"].as_str().unwrap()).collect::<Vec<_>>(), ["refund-policy"]);
}

// A long paragraph is searched to its end, though the model reads only the
// start of a text.
#[tokio::test]
async fn a_long_paragraph_is_searched_to_its_end() {
    let s = searching().await;
    let long = format!("{}refund", "deploy ".repeat(3000));
    s.document("handbook", &long).await;
    let found = hits(&s.search_as(&s.session, "money back", 0).await);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["paragraph"], 1);
    assert!(found[0]["text"].as_str().unwrap().ends_with("refund"));
}

// A long note is searched in pieces, each of which says whose it is, and it
// is found once, however many of its pieces are close.
#[tokio::test]
async fn a_long_note_is_found_once_and_by_its_name() {
    let s = searching().await;
    s.note("handbook", &"refund money back ".repeat(200)).await;
    s.note("refund-policy", "repay").await;
    let found = hits(&s.search_as(&s.session, "refund", 0).await);
    let names: Vec<&str> = found.iter().map(|hit| hit["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["handbook", "refund-policy"], "{found:?}");
    // What is about deploying is only in the note's second piece.
    s.note("plan", &format!("{}friday", "refund ".repeat(150))).await;
    becomes(pairs(&[("note", "plan")]), || s.found("deploy")).await;
    let found = hits(&s.search_as(&s.session, "deploy", 0).await);
    assert!(found[0]["text"].as_str().unwrap().starts_with("plan: "), "{found:?}");
}

// A note with nothing in it yet is found by its name.
#[tokio::test]
async fn a_note_with_nothing_in_it_is_found_by_its_name() {
    let s = searching().await;
    s.note("deploy-date", "").await;
    s.structured("refund", "{}").await;
    assert_eq!(s.found("ship").await, pairs(&[("note", "deploy-date")]));
    assert_eq!(s.found("money").await, pairs(&[("structured", "refund")]));
}

// Where the model is fetched from is looked at only when it is fetched, and
// has to be a web address.
#[test]
fn where_the_model_is_fetched_from_is_looked_at_only_when_it_is_used() {
    let folder = tempfile::tempdir().unwrap();
    let start = |source: &str, model: Option<&Path>| {
        let mut sidecar = std::process::Command::new(env!("CARGO_BIN_EXE_orvud"));
        sidecar.args(["--local", "--data-dir"]).arg(folder.path().join("data")).arg("--socket").arg(folder.path().join("s"));
        if let Some(model) = model {
            sidecar.arg("--model").arg(model);
        }
        sidecar
            .env("HOME", folder.path())
            .env_remove("ORVU_MODEL")
            .env_remove("XDG_DATA_HOME")
            .env("ORVU_MODEL_SOURCE", source)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    // A sidecar that does not stop by itself is stopped after ten seconds.
    let mut refused = start("mirror.local/potion", None);
    for _ in 0..100 {
        if refused.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = refused.kill();
    let refused = refused.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success());
    assert!(said.contains("ORVU_MODEL_SOURCE") && !said.contains("serving on"), "refused only after it started: {said}");
    // Given a model, it starts. What it says is read on a thread of its own,
    // so that a sidecar that says nothing does not hold the test up.
    let mut given = start("mirror.local/potion", Some(&folder.path().join("model")));
    let err = given.stderr.take().unwrap();
    let (tell, told) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(std::io::BufReader::new(err)).map_while(Result::ok) {
            if line.contains("serving on") {
                let _ = tell.send(line);
            }
        }
    });
    let serving = told.recv_timeout(Duration::from_secs(10));
    let _ = given.kill();
    let _ = given.wait();
    assert!(serving.is_ok(), "the sidecar did not start");
}

// An agent that may read no notes and no documents has nothing to search,
// and is told so, before anything is read for it.
#[tokio::test]
async fn an_agent_that_may_read_nothing_searched_may_not_search() {
    let s = searching().await;
    s.note("refund-policy", "money back").await;
    let path = s._folder.path().join(agents::FILE);
    let key = agents::add(&path, "bob").unwrap();
    agents::allow(&path, "bob", Some(orvud::rules::Rules::parse(&["budgets:spend"]).unwrap())).unwrap();
    let bob = open(&s.dp, &key).await;
    let search = pb::Search { object: "orvu/crew/index".into(), query: "refund".into(), limit: 0 };
    let refused = s.dp.execute_for_test(pb::Request { session: bob, op: Some(Op::Search(search)), request_id: Vec::new() }).await.unwrap_err();
    assert_eq!(refused.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        refused.message(),
        "bob may not read notes, documents or tasks, so it cannot search. Its rules are: budgets:spend"
    );
    assert_eq!(s.searcher.teams(), 0);
}

// A search names a team's index, asks for at most 50, and is not part of a
// batch.
#[tokio::test]
async fn a_search_is_asked_for_in_one_way() {
    let s = searching().await;
    for (object, limit) in [("orvu/crew/notes", 0), ("crew", 0), ("orvu/crew/index", 51)] {
        let search = pb::Search { object: object.into(), query: "refund".into(), limit };
        let request = pb::Request { session: s.session.clone(), op: Some(Op::Search(search)), request_id: Vec::new() };
        let refused = s.dp.execute_for_test(request).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument, "{object} {limit}");
    }
    let search = pb::Search { object: "orvu/crew/index".into(), query: "refund".into(), limit: 0 };
    let batch = pb::BatchRequest {
        session: s.session.clone(),
        requests: vec![pb::Request { session: String::new(), op: Some(Op::Search(search)), request_id: Vec::new() }],
        request_id: Vec::new(),
    };
    let answered = s.dp.batch_for_test(batch).await.unwrap();
    let (code, why) = failed(&answered.responses[0]);
    assert_eq!(code, pb::Code::NoSearch as i32);
    assert!(why.contains("batch"), "{why}");
}

// A team that has nothing finds nothing, and the sidecar keeps nothing for
// it: any agent may search, under any team name.
#[tokio::test]
async fn a_team_that_has_nothing_is_not_kept() {
    let s = searching().await;
    for team in ["nobody", "nothing-here", "made-up"] {
        let search = pb::Search { object: format!("orvu/{team}/index"), query: "refund".into(), limit: 0 };
        assert!(hits(&s.run(&s.session, Op::Search(search)).await).is_empty());
    }
    assert_eq!(s.searcher.teams(), 0);
    s.note("refund-policy", "money back").await;
    assert_eq!(s.found("refund").await.len(), 1);
    assert_eq!(s.searcher.teams(), 1);
}

// A team whose index object is not a list of what it has finds nothing; it
// is not a failure to search.
#[tokio::test]
async fn a_team_whose_index_is_another_kind_finds_nothing() {
    let s = searching().await;
    let count = pb::CounterIncrement { object: "orvu/crew/index".into(), n: 1 };
    s.run(&s.session, Op::CounterIncrement(count)).await;
    assert!(hits(&s.search_as(&s.session, "refund", 0).await).is_empty());
}

// A team that has only things that are not searched finds nothing, and the
// sidecar keeps nothing for it.
#[tokio::test]
async fn a_team_with_nothing_searched_is_not_kept() {
    let s = searching().await;
    s.listed("tasks-tree", "tree").await;
    s.listed("tally/votes", "cart").await;
    s.listed("tasks", "json").await;
    assert!(s.found("refund").await.is_empty());
    assert_eq!(s.searcher.teams(), 0);
}

// What is searched is what the team's index lists, whether the index was
// made just now or a while ago: a document written without being listed is
// not found, as it would not be after the sidecar started again.
#[tokio::test]
async fn only_what_the_team_lists_is_searched() {
    let s = searching().await;
    s.document("handbook", "deploy").await;
    assert!(s.found("refund").await.is_empty());
    let insert = pb::RichtextInsert { object: "orvu/crew/doc/unlisted".into(), index: 0, text: "refund".into() };
    s.run(&s.session, Op::RichtextInsert(insert)).await;
    s.document("draft", "money back").await;
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.found("refund").await, pairs(&[("document", "draft")]));
}

// Something written first and listed after is found within a moment, as it
// can be when the two changes come from another machine in either order.
#[tokio::test]
async fn something_listed_after_it_was_written_is_found_within_a_moment() {
    let s = searching().await;
    s.document("handbook", "deploy").await;
    assert!(s.found("refund").await.is_empty());
    let insert = pb::RichtextInsert { object: "orvu/crew/doc/draft".into(), index: 0, text: "refund".into() };
    s.run(&s.session, Op::RichtextInsert(insert)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.listed("doc/draft", "richtext").await;
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
}

// Something taken off the team's list is no longer found, though it is
// still there.
#[tokio::test]
async fn something_taken_off_the_list_is_not_found() {
    let s = searching().await;
    s.document("draft", "refund").await;
    assert_eq!(s.found("refund").await, pairs(&[("document", "draft")]));
    let remove = pb::MapRemove { object: "orvu/crew/index".into(), key: "doc/draft".into() };
    s.run(&s.session, Op::MapRemove(remove)).await;
    becomes(Vec::new(), || s.found("refund")).await;
}

// A team whose list names only things that are not there finds nothing, and
// the sidecar keeps nothing for it.
#[tokio::test]
async fn a_team_listing_only_what_is_not_there_is_not_kept() {
    let s = searching().await;
    s.listed("note/nothing", "json").await;
    s.listed("doc/nothing", "richtext").await;
    assert!(s.found("refund").await.is_empty());
    assert_eq!(s.searcher.teams(), 0);
    // What it is given later is found within a moment all the same.
    s.document("draft", "refund").await;
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
}

// Something kept under a document's name, but not a document itself, is not
// searched, whenever it changes.
#[tokio::test]
async fn only_notes_documents_and_tasks_are_searched() {
    let s = searching().await;
    s.document("handbook", "deploy").await;
    assert_eq!(s.found("refund").await, Vec::<(String, String)>::new());
    s.listed("doc/handbook/comments", "richtext").await;
    let insert = pb::RichtextInsert { object: "orvu/crew/doc/handbook/comments".into(), index: 0, text: "refund".into() };
    s.run(&s.session, Op::RichtextInsert(insert)).await;
    s.document("draft", "money back").await;
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
}

// At most as many are found as asked for, and five when the search does not
// say.
#[tokio::test]
async fn at_most_as_many_are_found_as_asked_for() {
    let s = searching().await;
    for n in 0..8 {
        s.note(&format!("refund-{n}"), "money back").await;
    }
    assert_eq!(hits(&s.search_as(&s.session, "refund", 0).await).len(), 5);
    assert_eq!(hits(&s.search_as(&s.session, "refund", 2).await).len(), 2);
    assert_eq!(hits(&s.search_as(&s.session, "refund", 50).await).len(), 8);
}

// A sidecar that does not search, or whose model cannot be loaded, says why.
#[tokio::test]
async fn a_search_that_cannot_be_done_says_why() {
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default");
    let session = open(&dp, "").await;
    let search = pb::Search { object: "orvu/crew/index".into(), query: "refund".into(), limit: 0 };
    let answered = dp.execute_for_test(pb::Request { session, op: Some(Op::Search(search)), request_id: Vec::new() }).await.unwrap();
    assert_eq!(failed(&answered), (pb::Code::NoSearch as i32, "this sidecar does not search".into()));

    let s = sidecar(|folder| ModelFrom::Folder(folder.join("nothing-here"))).await;
    // A team that has nothing needs no model to find nothing.
    assert!(hits(&s.search_as(&s.session, "refund", 0).await).is_empty());
    s.note("refund-policy", "money back").await;
    let (code, why) = failed(&s.search_as(&s.session, "refund", 0).await);
    assert_eq!(code, pb::Code::NoSearch as i32);
    assert!(why.contains("nothing-here"), "{why}");
}

// Every searched team is read again every so often, so a change whose report
// never came is found all the same.
#[tokio::test]
async fn every_searched_team_is_read_again_every_so_often() {
    let s = paced(unfollowed(Duration::from_millis(200), LONG)).await;
    s.document("handbook", "deploy").await;
    assert!(s.found("refund").await.is_empty());
    s.document("draft", "refund").await;
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
}

// When the engine dropped reports of changes, everything is read again at
// once, and then not again for a while however often reports are dropped.
#[tokio::test]
async fn everything_is_read_again_at_once_when_reports_were_dropped() {
    let s = paced(unfollowed(LONG, LONG)).await;
    s.document("handbook", "deploy").await;
    assert!(s.found("refund").await.is_empty());
    s.document("draft", "refund").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(s.found("refund").await.is_empty(), "nothing else reads it again");
    s.searcher.missed();
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
    // Resting now: dropped again, nothing is read again yet.
    s.document("plan", "money back").await;
    s.searcher.missed();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.found("refund").await, pairs(&[("document", "draft")]));
}

// Reading everything again leaves alone an object whose texts did not
// change, and puts in again one whose texts did.
#[tokio::test]
async fn an_object_whose_texts_did_not_change_is_left_alone() {
    let s = paced(unfollowed(Duration::from_millis(50), LONG)).await;
    s.document("handbook", "deploy").await;
    assert!(s.found("refund").await.is_empty());
    let (reads, puts) = s.searcher.counts();
    s.read_more(reads, 5).await;
    assert_eq!(s.searcher.counts().1, puts, "read again and left alone");
    let insert = pb::RichtextInsert { object: "orvu/crew/doc/handbook".into(), index: 0, text: "refund ".into() };
    s.run(&s.session, Op::RichtextInsert(insert)).await;
    becomes(pairs(&[("document", "handbook")]), || s.found("refund")).await;
    assert_eq!(s.searcher.counts().1, puts + 1);
}

// An object that cannot be read keeps what it had in the index, and is found
// as it was.
#[tokio::test]
async fn an_object_that_cannot_be_read_keeps_what_it_had() {
    let s = paced(unfollowed(Duration::from_millis(50), LONG)).await;
    s.document("draft", "refund").await;
    assert_eq!(s.found("refund").await, pairs(&[("document", "draft")]));
    s.searcher.fail_reads("orvu/crew/doc/draft", true);
    let (reads, _) = s.searcher.counts();
    s.read_more(reads, 5).await;
    assert_eq!(s.found("refund").await, pairs(&[("document", "draft")]));
}

// A team found to have nothing is answered at once for a while, without
// reading it again. When reports were dropped, that is forgotten, as a
// dropped report may have been what said it has something now.
#[tokio::test]
async fn a_team_found_to_have_nothing_is_forgotten_when_reports_were_dropped() {
    let s = paced(unfollowed(LONG, Duration::ZERO)).await;
    s.listed("doc/draft", "richtext").await;
    assert!(s.found("refund").await.is_empty());
    assert_eq!(s.searcher.teams(), 0);
    let insert = pb::RichtextInsert { object: "orvu/crew/doc/draft".into(), index: 0, text: "refund".into() };
    s.run(&s.session, Op::RichtextInsert(insert)).await;
    assert!(s.found("refund").await.is_empty(), "known to have nothing");
    s.searcher.missed();
    becomes(pairs(&[("document", "draft")]), || s.found("refund")).await;
}

// Each team's index has a lock of its own: a search of one team does not
// wait while another team's index is being changed.
#[tokio::test]
async fn a_search_of_one_team_does_not_wait_for_another() {
    let s = searching().await;
    s.document("draft", "refund").await;
    s.document_of("other", "plan", "refund").await;
    let other = || async {
        let search = pb::Search { object: "orvu/other/index".into(), query: "refund".into(), limit: 0 };
        hits(&s.run(&s.session, Op::Search(search)).await).len()
    };
    assert_eq!(s.found("refund").await.len(), 1);
    assert_eq!(other().await, 1);
    let (release, released) = std::sync::mpsc::channel();
    assert!(s.searcher.hold("crew", released));
    let quick = Duration::from_secs(2);
    assert_eq!(tokio::time::timeout(quick, other()).await.expect("another team's search waited"), 1);
    assert!(tokio::time::timeout(Duration::from_millis(300), s.found("refund")).await.is_err(), "the held team waits");
    drop(release);
    assert_eq!(tokio::time::timeout(quick, s.found("refund")).await.unwrap().len(), 1);
}

// Two first searches of a team at once make its index once, and both find
// what it has.
#[tokio::test]
async fn two_first_searches_at_once_make_the_index_once() {
    let s = paced(unfollowed(LONG, LONG)).await;
    for name in ["draft", "plan", "report"] {
        s.document(name, "refund").await;
    }
    let (reads, _) = s.searcher.counts();
    let (one, two) = tokio::join!(s.found("refund"), s.found("money"));
    assert_eq!(one.len(), 3);
    assert_eq!(two.len(), 3);
    assert_eq!(s.searcher.counts().0, reads + 3, "each object read once");
    assert_eq!(s.searcher.teams(), 1);
}

// Tasks are found by their titles and notes, each with its id and title,
// and a change to a task is found within a moment.
#[tokio::test]
async fn tasks_are_found_by_their_titles_and_notes() {
    let s = searching().await;
    s.task("t-1", "Pay the refund", None).await;
    s.task("t-2", "Write the summary", Some("deploy on friday")).await;
    s.note("plan", "money back").await;
    assert_eq!(s.found("refund").await, pairs(&[("note", "plan"), ("task", "t-1")]));
    let hit = hits(&s.search_as(&s.session, "ship", 0).await);
    assert_eq!(hit.len(), 1, "{hit:?}");
    assert_eq!(hit[0]["kind"], "task");
    assert_eq!(hit[0]["name"], "t-2");
    assert_eq!(hit[0]["title"], "Write the summary");
    assert_eq!(hit[0]["text"], "Write the summary: deploy on friday");
    let set = pb::JsonSet { object: "orvu/crew/tasks".into(), path_json: r#"["t-1","notes"]"#.into(), value_json: r#""ship it""#.into() };
    s.run(&s.session, Op::JsonSet(set)).await;
    becomes(pairs(&[("task", "t-1"), ("task", "t-2")]), || s.found("ship")).await;
    // What is on the board and is not a task is not searched.
    let set = pb::JsonSet { object: "orvu/crew/tasks".into(), path_json: r#"["odd"]"#.into(), value_json: r#""refund""#.into() };
    s.run(&s.session, Op::JsonSet(set)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.found("refund").await, pairs(&[("note", "plan"), ("task", "t-1")]));
}

// Tasks are found only by an agent that may read them, and an agent that may
// read only tasks can search them.
#[tokio::test]
async fn tasks_are_found_only_by_an_agent_that_may_read_them() {
    let s = searching().await;
    s.task("t-1", "Pay the refund", None).await;
    s.note("plan", "money back").await;
    let path = s._folder.path().join(agents::FILE);
    let names = |found: Vec<Value>| found.iter().map(|hit| hit["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    for (agent, rules, expected) in [("alice", "notes:read", vec!["plan"]), ("carol", "tasks:read", vec!["t-1"])] {
        let key = agents::add(&path, agent).unwrap();
        agents::allow(&path, agent, Some(orvud::rules::Rules::parse(&[rules]).unwrap())).unwrap();
        let session = open(&s.dp, &key).await;
        assert_eq!(names(hits(&s.search_as(&session, "refund", 0).await)), expected, "{agent}");
    }
}
