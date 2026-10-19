//! The record of who changed what, as the service writes it: one line for
//! each change, with the agent that made it, and one for each thing an agent
//! was not allowed to do. Reads are not recorded.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::agents::{self, Agents};
use orvud::audit::{self, Audit, Filter};
use orvud::pb;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

struct Sidecar {
    dp: orvud::server::Dataplane,
    audit: Arc<Audit>,
    folder: tempfile::TempDir,
}

impl Sidecar {
    async fn new() -> Self {
        let folder = tempfile::tempdir().unwrap();
        let fabric = Arc::new(MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
        let client = Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine starts"));
        let audit = Arc::new(Audit::to_file(folder.path().join(audit::FILE)).unwrap());
        let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default")
            .admitting(Arc::new(Agents::from_file(folder.path().join(agents::FILE))))
            .recording(audit.clone());
        Sidecar { dp, audit, folder }
    }

    fn agents(&self) -> PathBuf {
        self.folder.path().join(agents::FILE)
    }

    async fn open(&self, key: &str) -> Result<String, tonic::Status> {
        let opening = pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: key.as_bytes().to_vec() };
        self.dp.open_for_test(opening).await.map(|opened| opened.session)
    }

    /// Every line recorded so far.
    fn lines(&self) -> Vec<Value> {
        self.audit.close();
        audit::read(&self.folder.path().join(audit::FILE), &Filter::default()).unwrap()
    }
}

fn put(session: &str, object: &str, key: &str) -> pb::Request {
    pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::MapPut(pb::MapPut { object: object.into(), key: key.into(), value: "secret".into() })),
        request_id: Vec::new(),
    }
}

fn get(session: &str, object: &str) -> pb::Request {
    pb::Request { session: session.into(), op: Some(pb::request::Op::MapGet(pb::MapGet { object: object.into(), key: "k".into() })), request_id: Vec::new() }
}

fn summary(line: &Value) -> String {
    let text = |name: &str| line.get(name).and_then(Value::as_str).unwrap_or("-").to_string();
    format!("{} {} {} {}", text("agent"), text("change"), text("object"), text("outcome"))
}

#[tokio::test]
async fn a_change_is_recorded_with_the_agent_that_made_it() {
    let sidecar = Sidecar::new().await;
    let key = agents::add(&sidecar.agents(), "alice").unwrap();
    let session = sidecar.open(&key).await.unwrap();
    sidecar.dp.execute_for_test(put(&session, "orvu/crew/notes", "deploy")).await.unwrap();
    sidecar.dp.execute_for_test(get(&session, "orvu/crew/notes")).await.unwrap();
    let lines = sidecar.lines();
    assert_eq!(lines.iter().map(summary).collect::<Vec<_>>(), ["alice map_put orvu/crew/notes done"]);
    assert_eq!(lines[0]["key"], "deploy");
    assert!(!lines[0].to_string().contains("secret"));
}

// With no agents set up, changes are recorded with no name.
#[tokio::test]
async fn without_keys_a_change_has_no_agent() {
    let sidecar = Sidecar::new().await;
    let session = sidecar.open("").await.unwrap();
    sidecar.dp.execute_for_test(put(&session, "visits", "k")).await.unwrap();
    assert_eq!(sidecar.lines().iter().map(summary).collect::<Vec<_>>(), ["- map_put visits done"]);
}

#[tokio::test]
async fn each_change_of_a_batch_is_recorded() {
    let sidecar = Sidecar::new().await;
    let key = agents::add(&sidecar.agents(), "alice").unwrap();
    let session = sidecar.open(&key).await.unwrap();
    let batch = pb::BatchRequest {
        session: session.clone(),
        requests: vec![put("", "orvu/crew/notes", "a"), get("", "orvu/crew/notes"), put("", "orvu/crew/notes-by", "a")],
        request_id: Vec::new(),
    };
    sidecar.dp.batch_for_test(batch).await.unwrap();
    assert_eq!(
        sidecar.lines().iter().map(summary).collect::<Vec<_>>(),
        ["alice map_put orvu/crew/notes done", "alice map_put orvu/crew/notes-by done"]
    );
}

// What an agent was not allowed to do is recorded with what it was told: a
// key that is not known, a change or a read its rules forbid, one request
// of a batch, and a watch.
#[tokio::test]
async fn what_was_not_allowed_is_recorded_with_why() {
    let sidecar = Sidecar::new().await;
    let key = agents::add(&sidecar.agents(), "alice").unwrap();
    agents::allow(&sidecar.agents(), "alice", Some(orvud::rules::Rules::parse(&["notes:read"]).unwrap())).unwrap();
    assert!(sidecar.open("orvu-key-0123").await.is_err());
    let session = sidecar.open(&key).await.unwrap();
    assert!(sidecar.dp.execute_for_test(put(&session, "orvu/crew/note/plan", "a")).await.is_err());
    assert!(sidecar.dp.execute_for_test(get(&session, "orvu/crew/tasks")).await.is_err());
    let batch = pb::BatchRequest { session: session.clone(), requests: vec![get("", "orvu/crew/notes"), put("", "orvu/crew/roster", "a")], request_id: Vec::new() };
    assert!(sidecar.dp.batch_for_test(batch).await.is_err());
    let watch = pb::WatchRequest { session: session.clone(), object: "orvu/crew/roster".into(), kind: pb::Kind::Map as i32 };
    assert!(sidecar.dp.watch_for_test(watch).await.is_err());
    let lines = sidecar.lines();
    assert_eq!(
        lines.iter().map(summary).collect::<Vec<_>>(),
        [
            "- open - not allowed",
            "alice map_put orvu/crew/note/plan not allowed",
            "alice read orvu/crew/tasks not allowed",
            "alice map_put orvu/crew/roster not allowed",
            "alice watch orvu/crew/roster not allowed",
        ]
    );
    assert_eq!(
        lines[1]["why"],
        "alice may not change orvu/crew/note/plan as another kind of object than the json it is. Its rules are: notes:read"
    );
}

// A call of an agent that was removed is turned away, which ends its
// session, and is still recorded under the agent's name.
#[tokio::test]
async fn a_removed_agents_refused_calls_are_recorded_with_its_name() {
    let sidecar = Sidecar::new().await;
    let key = agents::add(&sidecar.agents(), "alice").unwrap();
    agents::add(&sidecar.agents(), "bob").unwrap();
    let (one, two, three) = (sidecar.open(&key).await.unwrap(), sidecar.open(&key).await.unwrap(), sidecar.open(&key).await.unwrap());
    agents::remove(&sidecar.agents(), "alice").unwrap();
    assert!(sidecar.dp.execute_for_test(put(&one, "orvu/crew/notes", "a")).await.is_err());
    let batch = pb::BatchRequest { session: two, requests: vec![put("", "orvu/crew/roster", "a")], request_id: Vec::new() };
    assert!(sidecar.dp.batch_for_test(batch).await.is_err());
    let watch = pb::WatchRequest { session: three, object: "orvu/crew/tasks".into(), kind: pb::Kind::Map as i32 };
    assert!(sidecar.dp.watch_for_test(watch).await.is_err());
    assert_eq!(
        sidecar.lines().iter().map(summary).collect::<Vec<_>>(),
        [
            "alice map_put orvu/crew/notes not allowed",
            "alice batch - not allowed",
            "alice watch orvu/crew/tasks not allowed",
        ]
    );
}

// A program that stops waiting does not stop a change that has begun, and
// the change is recorded all the same.
#[tokio::test]
async fn a_change_whose_caller_stopped_waiting_is_recorded() {
    let sidecar = Sidecar::new().await;
    let key = agents::add(&sidecar.agents(), "alice").unwrap();
    let session = sidecar.open(&key).await.unwrap();
    let requests: Vec<_> = (0..500).map(|n| put("", "orvu/crew/notes", &format!("k{n}"))).collect();
    let batch = pb::BatchRequest { session: session.clone(), requests, request_id: Vec::new() };
    let gave_up = tokio::time::timeout(std::time::Duration::from_micros(10), sidecar.dp.batch_for_test(batch)).await;
    assert!(gave_up.is_err(), "the batch was done before the caller stopped waiting");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(sidecar.lines().len(), 500);
}

// A watch that ends because the agent may no longer read what it watches is
// recorded too.
#[tokio::test]
async fn a_watch_that_ends_for_lack_of_rights_is_recorded() {
    use futures::StreamExt;
    let sidecar = Sidecar::new().await;
    let key = agents::add(&sidecar.agents(), "alice").unwrap();
    let session = sidecar.open(&key).await.unwrap();
    let watch = pb::WatchRequest { session, object: "orvu/crew/notes".into(), kind: pb::Kind::Map as i32 };
    let (mut events, _alive) = sidecar.dp.watch_for_test(watch).await.unwrap();
    agents::allow(&sidecar.agents(), "alice", Some(orvud::rules::Rules::parse(&["tasks:read"]).unwrap())).unwrap();
    while let Some(Ok(_)) = tokio::time::timeout(std::time::Duration::from_secs(5), events.next()).await.expect("the watch did not end") {}
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(sidecar.lines().iter().map(summary).collect::<Vec<_>>(), ["alice watch orvu/crew/notes not allowed"]);
}

fn orvud(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_orvud")).args(args).output().unwrap()
}

fn write_record(folder: &Path) {
    std::fs::write(
        folder.join(audit::FILE),
        concat!(
            r#"{"time":"2026-10-15T14:00:00.000Z","agent":"alice","change":"map_put","object":"orvu/crew/notes","key":"deploy","outcome":"done"}"#, "\n",
            r#"{"time":"2026-10-15T14:00:01.000Z","agent":null,"change":"open","outcome":"not allowed","why":"no key"}"#, "\n",
        ),
    )
    .unwrap();
}

#[test]
fn the_audit_command_shows_the_record() {
    let dir = tempfile::tempdir().unwrap();
    write_record(dir.path());
    let data = dir.path().to_str().unwrap();
    let shown = orvud(&["audit", "--local", "--data-dir", data]);
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    assert_eq!(
        String::from_utf8_lossy(&shown.stdout),
        "2026-10-15 14:00:00  alice  map_put  orvu/crew/notes  key=deploy  done\n2026-10-15 14:00:01  -  open  not allowed: no key\n"
    );
    let picked = orvud(&["audit", "--local", "--data-dir", data, "--not-allowed", "--json"]);
    assert_eq!(
        String::from_utf8_lossy(&picked.stdout),
        "{\"agent\":null,\"change\":\"open\",\"outcome\":\"not allowed\",\"time\":\"2026-10-15T14:00:01.000Z\",\"why\":\"no key\"}\n"
    );
    let nothing = orvud(&["audit", "--local", "--data-dir", data, "--agent", "bob"]);
    assert_eq!(String::from_utf8_lossy(&nothing.stdout), "");
}
