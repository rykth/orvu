//! Sessions, and two rules that are easy to get wrong. A resume token the
//! sidecar has never seen starts a new session and is not an error. An
//! operation that names an unknown session is rejected, not served.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use std::sync::Arc;

async fn engine() -> Arc<Client> {
    let fabric = Arc::new(MemTransport::new());
    let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine starts"))
}

async fn dataplane() -> orvud::server::Dataplane {
    orvud::server::Dataplane::new(engine().await, ReplicaId(1), "default")
}

#[tokio::test]
async fn open_returns_a_session_and_the_engine_replica_id() {
    let dp = dataplane().await;
    let resp = dp
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open succeeds");
    assert!(!resp.session.is_empty(), "a session id is issued");
    assert!(!resp.resume_token.is_empty(), "a resume token is issued");
    assert_eq!(resp.replica_id, 1, "the engine's own replica id, not the app's");
    assert_eq!(resp.pid, std::process::id(), "the sidecar's own process");
}

#[tokio::test]
async fn an_unrecognized_resume_token_yields_a_fresh_session_not_an_error() {
    let dp = dataplane().await;
    let resp = dp
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: "token-from-a-sidecar-that-has-since-restarted".into(),
            credential: Vec::new(),
        })
        .await
        .expect("an unknown resume token is not an error");
    assert!(!resp.session.is_empty());
}

#[tokio::test]
async fn an_op_on_an_unknown_session_is_refused_at_the_boundary() {
    let dp = dataplane().await;
    let err = dp
        .execute_for_test(pb::Request {
            session: "never-issued".into(),
            op: Some(pb::request::Op::CounterValue(pb::CounterValue {
                object: "visits".into(),
            })),
            request_id: Vec::new(),
        })
        .await
        .expect_err("an unknown session must not be served");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

fn open_req(ns: &str, resume: &str) -> pb::OpenRequest {
    pb::OpenRequest { namespace: ns.into(), resume_token: resume.into(), credential: Vec::new() }
}

#[tokio::test]
async fn resuming_a_known_token_returns_the_same_session() {
    let dp = dataplane().await;
    let a = dp.open_for_test(open_req("default", "")).await.unwrap();
    let b = dp.open_for_test(open_req("default", &a.resume_token)).await.unwrap();
    assert_eq!(a.session, b.session);
    assert_eq!(a.resume_token, b.resume_token);
    assert_ne!(a.session, a.resume_token, "the token is not the id");
    assert_eq!(a.session.len(), 32, "the id is 128 bits, hex");
    assert!(a.session.bytes().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(a.resume_token.len(), 32, "128 bits, hex");
}

/// A resume token is tied to the exact namespace text it was opened with. A
/// client that opens "" and comes back as "default" gets a new session, even
/// though both mean the same namespace. Both requests are served. Only the
/// session differs, and getting a new session is not an error.
#[tokio::test]
async fn resuming_under_a_different_namespace_is_a_fresh_session() {
    let dp = dataplane().await;
    let a = dp.open_for_test(open_req("", "")).await.unwrap();
    let b = dp.open_for_test(open_req("default", &a.resume_token)).await.unwrap();
    assert_ne!(a.session, b.session);
}

/// `OpenRequest.namespace` is the first field of the first message a generated
/// client sees. Nothing after `Open` looks at it. `exec` passes an
/// operation's `object` to the engine exactly as it arrives. So if `Open` did
/// not check the namespace, the sidecar would serve its one configured
/// namespace to every session, whatever the session asked for.
#[tokio::test]
async fn a_namespace_this_sidecar_does_not_serve_is_refused() {
    let dp = dataplane().await;
    let err = dp
        .open_for_test(open_req("someone-elses-namespace", ""))
        .await
        .expect_err("a namespace we do not serve must not be served");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    // The namespace we do serve still opens.
    dp.open_for_test(open_req("default", "")).await.expect("our own namespace opens");
}

#[tokio::test]
async fn a_valid_session_passes_the_guard() {
    let dp = dataplane().await;
    let a = dp.open_for_test(open_req("default", "")).await.unwrap();
    // The session check passes and the operation runs, so we get an answer
    // and not Unauthenticated.
    dp.execute_for_test(pb::Request {
        session: a.session,
        op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "v".into() })),
        request_id: Vec::new(),
    })
    .await
    .expect("a valid session is served");
}

#[test]
fn the_least_recently_used_session_is_evicted_at_the_cap() {
    let s = orvud::session::Sessions::with_cap(3);
    let (first, _) = s.open("d", "", None);
    let (second, _) = s.open("d", "", None);
    let (third, _) = s.open("d", "", None);
    assert!(s.get(&first).is_some(), "touch the oldest so `second` is now LRU");
    let (fourth, _) = s.open("d", "", None);
    assert!(s.get(&second).is_none(), "LRU evicted");
    assert!(s.get(&first).is_some());
    assert!(s.get(&third).is_some());
    assert!(s.get(&fourth).is_some());
}

#[test]
fn an_evicted_sessions_token_no_longer_resumes() {
    let s = orvud::session::Sessions::with_cap(1);
    let (a, tok) = s.open("d", "", None);
    let (b, _) = s.open("d", "", None);
    assert!(s.get(&a).is_none());
    let (c, _) = s.open("d", &tok, None);
    assert_ne!(c, a);
    assert_ne!(c, b);
}

#[tokio::test]
async fn a_guessed_sequential_session_id_is_refused() {
    let dp = dataplane().await;
    let _ = dp.open_for_test(open_req("default", "")).await.unwrap();
    for guess in ["s1", "1", "s0"] {
        let err = dp
            .execute_for_test(pb::Request {
                session: guess.into(),
                op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "v".into() })),
                request_id: Vec::new(),
            })
            .await
            .expect_err("a guessable id must not authenticate");
        assert_eq!(err.code(), tonic::Code::Unauthenticated, "guess {guess}");
    }
}

#[tokio::test]
async fn open_reports_the_sidecar_version() {
    let dp = dataplane().await;
    let opened = dp.open_for_test(open_req("default", "")).await.expect("open");
    assert_eq!(opened.version, orvud::VERSION);
    assert!(!opened.version.is_empty());
}

/// Marks a session as having a watch open, as an open watch does. The watch
/// sends nothing, so the session is not used by it.
fn watching(s: &orvud::session::Sessions, id: &str) {
    let state = s.get(id).expect("the session is open");
    state.watches.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

// Sessions that only hold a quiet watch do not take every place. Most
// sessions here hold one, and programs that make calls keep coming. The two
// that keep making calls keep their sessions, and the quiet watchers go.
#[test]
fn callers_keep_their_sessions_while_many_quiet_sessions_watch() {
    let s = orvud::session::Sessions::with_cap(8);
    let watchers: Vec<String> = (0..7)
        .map(|_| {
            let (id, _) = s.open("d", "", None);
            watching(&s, &id);
            id
        })
        .collect();
    let (a, _) = s.open("d", "", None);
    let (b, _) = s.open("d", "", None);
    for _ in 0..20 {
        assert!(s.get(&a).is_some(), "the first caller lost its session");
        assert!(s.get(&b).is_some(), "the second caller lost its session");
        let (a2, _) = s.open("d", "", None);
        let (b2, _) = s.open("d", "", None);
        assert!(s.get(&a2).is_some() && s.get(&b2).is_some());
    }
    assert!(watchers.iter().all(|id| !s.contains(id)), "a quiet watcher kept its place");
}

// When the places are full, a watcher that is dropped opens a new session,
// and that drops one more: the session used longest ago after it. It does not
// set off a run where each watcher that comes back pushes out the next one,
// until every watcher has been dropped once.
#[test]
fn a_dropped_watcher_coming_back_drops_only_the_next_session_used_longest_ago() {
    let s = orvud::session::Sessions::with_cap(6);
    let (w1, _) = s.open("d", "", None);
    let (caller, _) = s.open("d", "", None);
    let mut watchers = vec![w1.clone()];
    for _ in 0..4 {
        watchers.push(s.open("d", "", None).0);
    }
    for w in &watchers {
        watching(&s, w);
    }
    // Used in this order, so the first watcher was used longest ago and the
    // caller next.
    assert!(s.get(&w1).is_some() && s.get(&caller).is_some());
    for w in &watchers[1..] {
        assert!(s.get(w).is_some());
    }
    let (newcomer, _) = s.open("d", "", None);
    // Each watcher that was dropped comes back with a new session, until no
    // more are dropped.
    let mut dropped = Vec::new();
    for _ in 0..20 {
        let Some(i) = watchers.iter().position(|w| !s.contains(w)) else { break };
        dropped.push(watchers[i].clone());
        let (back, _) = s.open("d", "", None);
        watching(&s, &back);
        watchers[i] = back;
    }
    assert_eq!(dropped, vec![w1], "more than one watcher was dropped");
    assert!(!s.contains(&caller), "the next session used longest ago should have gone");
    assert!(s.contains(&newcomer));
    assert!(watchers.iter().all(|w| s.contains(w)));
}

// Every session goes in the order it was last used, whether it is new or
// not and whether it has a watch open or not.
#[test]
fn sessions_go_in_the_order_they_were_last_used() {
    let s = orvud::session::Sessions::with_cap(3);
    let (a, _) = s.open("d", "", None);
    let (b, _) = s.open("d", "", None);
    let (c, _) = s.open("d", "", None);
    watching(&s, &a);
    // Used in the order b, a, c.
    for id in [&b, &a, &c] {
        assert!(s.get(id).is_some());
    }
    let (_d, _) = s.open("d", "", None);
    assert!(!s.contains(&b));
    let (_e, _) = s.open("d", "", None);
    assert!(!s.contains(&a), "a session with a watch open went out of turn");
    let (_f, _) = s.open("d", "", None);
    assert!(!s.contains(&c));
}

fn agent(name: &str) -> Option<orvud::agents::Agent> {
    Some(orvud::agents::Agent { name: name.into(), fingerprint: format!("fingerprint-of-{name}") })
}

fn count_open(s: &orvud::session::Sessions, ids: &[String]) -> usize {
    ids.iter().filter(|id| s.contains(id)).count()
}

// An agent with a key may hold at most a quarter of the places. A new session
// past that drops the agent's own session used longest ago, even when sessions
// of others were used longer ago still and places are free.
#[test]
fn an_agent_past_its_share_drops_only_its_own_sessions() {
    let s = orvud::session::Sessions::with_cap(8);
    let (alice, _) = s.open("d", "", agent("alice"));
    let (local, _) = s.open("d", "", None);
    let (m1, _) = s.open("d", "", agent("mallory"));
    let (m2, _) = s.open("d", "", agent("mallory"));
    let (m3, _) = s.open("d", "", agent("mallory"));
    assert!(!s.contains(&m1), "the agent went past its share");
    assert!(s.contains(&m2) && s.contains(&m3));
    assert!(s.contains(&alice), "another agent's session was dropped");
    assert!(s.contains(&local), "a session without a key was dropped");
}

// One agent opens session after session and keeps every one of them busy, as
// it would by watching an object it writes all the time. Sessions of other
// agents that sit idle are still there, because that agent only ever pushes
// out its own.
#[test]
fn callers_of_other_agents_keep_their_sessions_while_one_agent_keeps_many_busy() {
    let s = orvud::session::Sessions::with_cap(8);
    let (alice, _) = s.open("d", "", agent("alice"));
    let (bob, _) = s.open("d", "", agent("bob"));
    let mut mallory = Vec::new();
    for _ in 0..30 {
        mallory.push(s.open("d", "", agent("mallory")).0);
        for id in &mallory {
            let _ = s.get(id);
        }
    }
    assert!(s.contains(&alice), "alice lost her session");
    assert!(s.contains(&bob), "bob lost his session");
    assert_eq!(count_open(&s, &mallory), 2, "mallory should hold exactly her share");
}

// Programs without a key, on the local socket, are trusted and share the
// places as before: they are not held to a share, and at the limit the session
// used longest ago goes, whoever it belongs to.
#[test]
fn sessions_without_a_key_are_not_held_to_a_share() {
    let s = orvud::session::Sessions::with_cap(8);
    let local: Vec<String> = (0..8).map(|_| s.open("d", "", None).0).collect();
    assert_eq!(count_open(&s, &local), 8, "a session without a key was dropped below the limit");
    let (alice, _) = s.open("d", "", agent("alice"));
    assert!(!s.contains(&local[0]), "the session used longest ago should have gone");
    assert_eq!(count_open(&s, &local), 7);
    let _ = s.open("d", "", None);
    assert!(!s.contains(&local[1]));
    assert!(s.contains(&alice));
}

// An agent whose key was replaced is a new agent with the same name. The
// sessions the old key opened are not counted in its share, so the new key's
// sessions do not push them out, and they do not push out the new key's.
#[test]
fn the_share_belongs_to_the_key_and_not_only_the_name() {
    let s = orvud::session::Sessions::with_cap(8);
    let key = |fingerprint: &str| Some(orvud::agents::Agent { name: "alice".into(), fingerprint: fingerprint.into() });
    let (old, new) = (key("old"), key("new"));
    let (o1, _) = s.open("d", "", old.clone());
    let (o2, _) = s.open("d", "", old);
    let (n1, _) = s.open("d", "", new.clone());
    let (n2, _) = s.open("d", "", new.clone());
    assert_eq!(count_open(&s, &[o1, o2]), 2, "the new key's sessions pushed out the old key's");
    assert_eq!(count_open(&s, &[n1, n2]), 2);
}

// The limits a sidecar starts with.
#[test]
fn the_session_limits_are_what_the_readme_says() {
    assert_eq!(orvud::session::MAX_SESSIONS, 1024);
    assert_eq!(orvud::session::MAX_SESSIONS_PER_AGENT, 256);
}
