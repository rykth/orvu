//! Watch is a stream of an object's value. These tests check three things.
//! What `watch_object` in `orvu-sdk` promises still holds through the sidecar.
//! When a subscriber disconnects, the task that fed it ends. An engine failure
//! arrives as an event on the stream, not as a gRPC status (an error for the
//! whole call).

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;

async fn opened() -> (orvud::server::Dataplane, String) {
    let fabric = Arc::new(MemTransport::new());
    let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    let client = Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine"));
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default");
    let s = dp
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open")
        .session;
    (dp, s)
}

async fn bump(dp: &orvud::server::Dataplane, s: &str, n: u64) {
    dp.execute_for_test(pb::Request {
        session: s.into(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement {
            object: "visits".into(),
            n,
        })),
        request_id: Vec::new(),
    })
    .await
    .expect("increment");
}

fn watch_req(s: &str, kind: pb::Kind) -> pb::WatchRequest {
    pb::WatchRequest { session: s.into(), object: "visits".into(), kind: kind as i32 }
}

fn int_of(ev: pb::WatchEvent) -> i64 {
    match ev.event {
        Some(pb::watch_event::Event::Value(pb::ReadValue {
            v: Some(pb::read_value::V::IntValue(i)),
        })) => i,
        other => panic!("expected an int event, got {other:?}"),
    }
}

#[tokio::test]
async fn a_subscriber_sees_the_current_value_immediately() {
    let (dp, s) = opened().await;
    bump(&dp, &s, 4).await;

    let (mut stream, _guard) =
        dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("watch opens");

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the current value arrives without waiting for a write")
        .expect("stream is live")
        .expect("no status error");
    assert_eq!(int_of(first), 4, "CURRENT value on subscribe, not only future changes");
}

#[tokio::test]
async fn a_watch_reports_a_later_write() {
    let (dp, s) = opened().await;
    bump(&dp, &s, 0).await; // create the counter, so the watch has a value to start from
    let (mut stream, _guard) =
        dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("watch opens");

    let initial = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("initial")
        .expect("live")
        .expect("ok");
    assert_eq!(int_of(initial), 0);
    bump(&dp, &s, 6).await;

    let ev = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("a write produces an event")
        .expect("live")
        .expect("ok");
    assert_eq!(int_of(ev), 6);
}

#[tokio::test]
async fn an_unspecified_or_unknown_kind_is_rejected_rather_than_guessed() {
    let (dp, s) = opened().await;
    let err = dp
        .watch_for_test(watch_req(&s, pb::Kind::Unspecified))
        .await
        .err()
        .expect("an unspecified kind must not be guessed");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let err = dp
        .watch_for_test(pb::WatchRequest { session: s, object: "visits".into(), kind: 999 })
        .await
        .err()
        .expect("an unrecognized kind must not be guessed");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn an_unknown_session_is_a_status() {
    let (dp, _s) = opened().await;
    let err = dp
        .watch_for_test(watch_req("nope", pb::Kind::Counter))
        .await
        .err()
        .expect("envelope failure");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn watching_as_the_wrong_kind_is_a_failed_first_event_not_a_status() {
    let (dp, s) = opened().await;
    bump(&dp, &s, 1).await; // "visits" is now a counter

    let (mut stream, _guard) = dp
        .watch_for_test(watch_req(&s, pb::Kind::Set))
        .await
        .expect("an engine answer must not be a Status");

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the failure arrives as an event")
        .expect("stream carries the event")
        .expect("no status error");
    match first.event {
        Some(pb::watch_event::Event::Failed(f)) => {
            assert_eq!(f.code(), pb::Code::TypeMismatch, "the specific code: {f:?}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let end = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream ends rather than hanging");
    assert!(end.is_none(), "after Failed the stream ends normally");
}

#[tokio::test]
async fn a_dropped_subscriber_takes_its_forwarding_task_with_it() {
    let (dp, s) = opened().await;
    bump(&dp, &s, 0).await; // create the counter, so the watch has a value to start from
    let (mut stream, guard) =
        dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("watch opens");

    // Once the first event has arrived, the forwarding task is waiting in its loop.
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("initial")
        .expect("live")
        .expect("ok");
    assert!(guard.upgrade().is_some(), "the forwarding task is alive while the stream is held");

    drop(stream);
    // Nothing is written after this. The subscriber leaving must be enough to
    // end the task. The loop waits up to the deadline, so a slow machine makes
    // the test take longer and does not make it fail.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while guard.upgrade().is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(guard.upgrade().is_none(), "the forwarding task must end when its subscriber leaves, with no write to wake it");
}

#[tokio::test]
async fn one_subscriber_leaving_does_not_wedge_another() {
    let (dp, s) = opened().await;
    let (first, _g1) =
        dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("first watch");
    drop(first);

    let (mut second, _g2) =
        dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("second watch");

    bump(&dp, &s, 3).await;
    // Wait until the value 3 arrives. Do not count events.
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(ev)) = second.next().await {
            if int_of(ev) == 3 {
                return true;
            }
        }
        false
    })
    .await
    .expect("the surviving subscriber still receives");
    assert!(got, "stream ended before delivering 3");
}

#[tokio::test]
async fn the_real_watch_method_refuses_an_unknown_session() {
    // This calls the real `watch()` method of the service, not the
    // `watch_for_test` helper. If the session check is removed from `watch()`,
    // this test fails.
    use orvud::pb::data_plane_server::DataPlane;
    let (dp, _s) = opened().await;
    let err = DataPlane::watch(&dp, tonic::Request::new(watch_req("nope", pb::Kind::Counter)))
        .await
        .err()
        .expect("an unknown session must not open a stream");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

async fn first_of(stream: &mut orvud::watch::WatchStream) -> pb::WatchEvent {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("an event arrives")
        .expect("live")
        .expect("ok")
}

#[tokio::test]
async fn a_bounded_counter_watches_as_an_int() {
    let (dp, s) = opened().await;
    dp.execute_for_test(pb::Request {
        session: s.clone(),
        op: Some(pb::request::Op::BoundedIncrement(pb::BoundedIncrement {
            object: "visits".into(),
            n: 5,
        })),
        request_id: Vec::new(),
    })
    .await
    .expect("increment");
    let (mut stream, _g) =
        dp.watch_for_test(watch_req(&s, pb::Kind::Bounded)).await.expect("watch opens");
    assert_eq!(int_of(first_of(&mut stream).await), 5);
}

#[tokio::test]
async fn a_list_watches_as_strings() {
    let (dp, s) = opened().await;
    dp.execute_for_test(pb::Request {
        session: s.clone(),
        op: Some(pb::request::Op::ListInsert(pb::ListInsert {
            object: "visits".into(),
            index: 0,
            value: "a".into(),
        })),
        request_id: Vec::new(),
    })
    .await
    .expect("insert");
    let (mut stream, _g) =
        dp.watch_for_test(watch_req(&s, pb::Kind::List)).await.expect("watch opens");
    match first_of(&mut stream).await.event {
        Some(pb::watch_event::Event::Value(pb::ReadValue {
            v: Some(pb::read_value::V::Strings(l)),
        })) => assert_eq!(l.values, vec!["a".to_string()]),
        other => panic!("expected strings, got {other:?}"),
    }
}

#[tokio::test]
async fn a_map_that_keeps_clashes_watches_with_every_value() {
    let (dp, s) = opened().await;
    dp.execute_for_test(pb::Request {
        session: s.clone(),
        op: Some(pb::request::Op::MvMapPut(pb::MvMapPut {
            object: "visits".into(),
            key: "k".into(),
            value: "a".into(),
        })),
        request_id: Vec::new(),
    })
    .await
    .expect("put");
    let (mut stream, _g) =
        dp.watch_for_test(watch_req(&s, pb::Kind::MvMap)).await.expect("watch opens");
    match first_of(&mut stream).await.event {
        Some(pb::watch_event::Event::Value(pb::ReadValue {
            v: Some(pb::read_value::V::MultiPairs(l)),
        })) => {
            assert_eq!(l.pairs.len(), 1);
            assert_eq!((l.pairs[0].key.as_str(), l.pairs[0].values.clone()), ("k", vec!["a".to_string()]));
        }
        other => panic!("expected multi_pairs, got {other:?}"),
    }
}

fn str_of(ev: pb::WatchEvent) -> String {
    match ev.event {
        Some(pb::watch_event::Event::Value(pb::ReadValue {
            v: Some(pb::read_value::V::StringValue(v)),
        })) => v,
        other => panic!("expected a string event, got {other:?}"),
    }
}

/// Skipping a value equal to the last one is one of the three things
/// `watch_object` promises. The sidecar passes those on and does not implement
/// them again, and this test checks that the skipping still works through it.
///
/// The check is on something that does arrive. After the same value is
/// written twice, the next event is the new value and never the repeat. The
/// test cannot check for "exactly one event". Showing that an event did not
/// arrive means waiting a fixed time for it, and that would be a guess about
/// how this machine schedules tasks. Whether the forwarding task even wakes up
/// for the duplicate depends on timing, so counting events would test the
/// scheduler and not the skipping.
#[tokio::test]
async fn a_repeated_write_does_not_surface_as_a_repeated_event() {
    let (dp, s) = opened().await;
    let set = |v: &'static str| {
        let (dp, s) = (&dp, s.clone());
        async move {
            dp.execute_for_test(pb::Request {
                session: s,
                op: Some(pb::request::Op::RegisterSet(pb::RegisterSet {
                    object: "note".into(),
                    value: v.into(),
                })),
                request_id: Vec::new(),
            })
            .await
            .expect("set");
        }
    };

    set("v").await;
    let (mut stream, _g) = dp
        .watch_for_test(pb::WatchRequest {
            session: s.clone(),
            object: "note".into(),
            kind: pb::Kind::Register as i32,
        })
        .await
        .expect("watch opens");
    assert_eq!(str_of(first_of(&mut stream).await), "v", "current value on subscribe");

    set("v").await; // the same value again, so nothing changed
    set("w").await;

    assert_eq!(
        str_of(first_of(&mut stream).await),
        "w",
        "the repeated write must not surface as an event of its own"
    );
}

/// A data plane whose node saves to `store` before it answers each change.
async fn opened_on(store: &orvu_runtime::MemStore) -> (orvud::server::Dataplane, String) {
    let fabric = Arc::new(MemTransport::new());
    let mut cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    cfg.fsync = orvu_runtime::FsyncPolicy::Always;
    let client = Client::start_with_store(fabric.endpoint(1), cfg, Box::new(store.clone())).await.expect("engine");
    let dp = orvud::server::Dataplane::new(Arc::new(client), ReplicaId(1), "default");
    let s = dp
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open")
        .session;
    (dp, s)
}

fn cannot_save(f: &pb::Failed) {
    assert_eq!(f.code(), pb::Code::Engine, "{f:?}");
    assert!(f.detail.starts_with("cannot save changes"), "{f:?}");
}

// Once a change could not be saved, the sidecar stops serving: a watch that
// is open ends with the reason, a new one gets only the reason, and a read
// is answered with the reason, never with the unsaved value.
#[tokio::test]
async fn a_sidecar_that_cannot_save_ends_watches_and_refuses_reads() {
    let store = orvu_runtime::MemStore::new();
    let (dp, s) = opened_on(&store).await;
    bump(&dp, &s, 1).await;
    let (mut stream, _guard) = dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("watch opens");
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.unwrap().unwrap().unwrap();
    assert_eq!(int_of(first), 1);

    store.fail_saving(true);
    let r = dp
        .execute_for_test(pb::Request {
            session: s.clone(),
            op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 10 })),
            request_id: Vec::new(),
        })
        .await
        .expect("an answer, not a status");
    match r.result {
        Some(pb::response::Result::Failed(f)) => cannot_save(&f),
        other => panic!("expected Failed, got {other:?}"),
    }

    let ended = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the open watch was not ended")
        .expect("the reason comes before the end")
        .expect("no status error");
    match ended.event {
        Some(pb::watch_event::Event::Failed(f)) => cannot_save(&f),
        other => panic!("expected Failed, got {other:?}"),
    }
    let end = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.expect("the stream ends");
    assert!(end.is_none(), "after Failed the stream ends");

    let (mut late, _guard) = dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("an answer, not a status");
    let first = tokio::time::timeout(Duration::from_secs(5), late.next()).await.unwrap().unwrap().unwrap();
    match first.event {
        Some(pb::watch_event::Event::Failed(f)) => cannot_save(&f),
        other => panic!("a new watch after saving failed got {other:?}"),
    }

    let read = dp
        .execute_for_test(pb::Request {
            session: s,
            op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "visits".into() })),
            request_id: Vec::new(),
        })
        .await
        .expect("an answer, not a status");
    match read.result {
        Some(pb::response::Result::Failed(f)) => cannot_save(&f),
        other => panic!("a read after saving failed got {other:?}"),
    }
}

// ---- how many watches, and for how long --------------------------------

/// Opens a session on `dp`.
async fn open_on(dp: &orvud::server::Dataplane) -> String {
    dp.open_for_test(pb::OpenRequest { namespace: "default".into(), resume_token: String::new(), credential: Vec::new() })
        .await
        .expect("open")
        .session
}

/// A service on a new engine, built by `build` from a plain one.
async fn service(build: impl FnOnce(orvud::server::Dataplane) -> orvud::server::Dataplane) -> orvud::server::Dataplane {
    let fabric = Arc::new(MemTransport::new());
    let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    let client = Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine"));
    build(orvud::server::Dataplane::new(client, ReplicaId(1), "default"))
}

/// Waits until the task behind a watch has ended.
async fn ended(guard: &std::sync::Weak<()>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while guard.upgrade().is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(guard.upgrade().is_none(), "the task behind the watch is still running");
}

/// Waits for the watch to end, skipping any values still on the way, and
/// gives the status it ended with.
async fn end_of(stream: &mut orvud::watch::WatchStream) -> tonic::Status {
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.next().await {
                Some(Ok(_)) => continue,
                other => return other,
            }
        }
    })
    .await
    .expect("the watch went on");
    end.expect("the watch says why it ended").expect_err("a status")
}

// Each value a watch sends counts as using its session, and a watch with
// nothing to send does not. So when a run of new sessions fills the places,
// the sessions used longest ago go, a quiet watcher among them, while a
// watcher whose watch keeps sending values stays.
#[tokio::test]
async fn a_watch_that_keeps_sending_keeps_its_session_and_a_quiet_one_does_not() {
    let dp = service(|dp| dp.session_cap(3)).await;
    let busy = open_on(&dp).await;
    bump(&dp, &busy, 1).await;
    let (mut stream, _guard) = dp.watch_for_test(watch_req(&busy, pb::Kind::Counter)).await.expect("watch opens");
    assert_eq!(int_of(first_of(&mut stream).await), 1);
    // A watch on an object nobody writes has nothing to send.
    let quiet = open_on(&dp).await;
    let quiet_req = pb::WatchRequest { session: quiet.clone(), object: "quiet".into(), kind: pb::Kind::Counter as i32 };
    let (_quiet_stream, _) = dp.watch_for_test(quiet_req).await.expect("watch opens");
    // Many new sessions, each making one change that the busy watch sees.
    let mut others = Vec::new();
    for n in 2..=30 {
        let other = open_on(&dp).await;
        bump(&dp, &other, 1).await;
        assert_eq!(int_of(first_of(&mut stream).await), n, "the busy watch missed a value");
        others.push(other);
    }
    let alive = |s: &str| {
        let req = pb::Request {
            session: s.into(),
            op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "visits".into() })),
            request_id: Vec::new(),
        };
        let dp = &dp;
        async move { dp.execute_for_test(req).await.is_ok() }
    };
    assert!(!alive(&quiet).await, "the quiet watcher, used longest ago, should have gone");
    assert!(!alive(&others[0]).await, "an older new session should have gone");
    assert!(alive(&others[others.len() - 1]).await, "the newest session was dropped");
    assert!(alive(&busy).await, "the watcher whose watch kept sending was dropped");
}

// Once its watch has ended, a session that is not used grows old like any
// other, and is dropped when it is the one used longest ago.
#[tokio::test]
async fn a_session_whose_watch_ended_can_be_dropped() {
    let dp = service(|dp| dp.session_cap(2)).await;
    let watcher = open_on(&dp).await;
    let (stream, guard) = dp.watch_for_test(watch_req(&watcher, pb::Kind::Counter)).await.expect("watch opens");
    drop(stream);
    ended(&guard).await;
    let other = open_on(&dp).await;
    bump(&dp, &other, 0).await;
    let _third = open_on(&dp).await;
    let status = dp
        .execute_for_test(pb::Request {
            session: watcher,
            op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "visits".into() })),
            request_id: Vec::new(),
        })
        .await
        .expect_err("the session used longest ago is still there");
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
}

// A session with a quiet watch open is dropped to make room like any other
// when it is the one used longest ago, and its watches end with UNAUTHENTICATED, so a
// session's watches never outlive it.
#[tokio::test]
async fn a_watch_ends_when_its_session_is_dropped() {
    let dp = service(|dp| dp.session_cap(2)).await;
    let (first, second) = (open_on(&dp).await, open_on(&dp).await);
    let (mut oldest, guard) = dp.watch_for_test(watch_req(&first, pb::Kind::Counter)).await.expect("watch opens");
    let (_newer, _) = dp.watch_for_test(watch_req(&second, pb::Kind::Counter)).await.expect("watch opens");
    // A third session pushes out the one used longest ago.
    let _third = open_on(&dp).await;
    let status = end_of(&mut oldest).await;
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
    ended(&guard).await;
    dp.execute_for_test(pb::Request {
        session: second,
        op: Some(pb::request::Op::CounterValue(pb::CounterValue { object: "visits".into() })),
        request_id: Vec::new(),
    })
    .await
    .expect("the session used last was dropped in place of the oldest");
}

fn refused_as_too_many(opened: Result<(orvud::watch::WatchStream, std::sync::Weak<()>), tonic::Status>) {
    match opened {
        Err(status) => {
            assert_eq!(status.code(), tonic::Code::ResourceExhausted, "{status:?}");
            assert!(status.message().contains("too many watches"), "{status:?}");
        }
        Ok(_) => panic!("a watch past the limit was opened"),
    }
}

// One session may have only so many watches open. Ending one makes room for
// another, and so does a watch that ended at once with an engine answer.
#[tokio::test]
async fn a_session_has_only_so_many_watches() {
    let dp = service(|dp| dp.watch_limits(2, 100)).await;
    let s = open_on(&dp).await;
    let (first, first_task) = dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("first");
    let (_second, _) = dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("second");
    refused_as_too_many(dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await);
    // Another session is not held to this one's limit.
    let other = open_on(&dp).await;
    let (_theirs, _) = dp.watch_for_test(watch_req(&other, pb::Kind::Counter)).await.expect("another session's");

    drop(first);
    ended(&first_task).await;
    let (_third, _) = dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await.expect("room once one ended");
    refused_as_too_many(dp.watch_for_test(watch_req(&s, pb::Kind::Counter)).await);
}

// A watch that ends at once, with an engine answer, gives its place back.
#[tokio::test]
async fn a_watch_that_fails_at_once_gives_its_place_back() {
    let dp = service(|dp| dp.watch_limits(1, 100)).await;
    let s = open_on(&dp).await;
    bump(&dp, &s, 1).await;
    for _ in 0..3 {
        let (mut stream, _) = dp.watch_for_test(watch_req(&s, pb::Kind::Set)).await.expect("opens with a Failed event");
        let ev = first_of(&mut stream).await;
        assert!(matches!(ev.event, Some(pb::watch_event::Event::Failed(_))), "{ev:?}");
    }
}

// The sidecar as a whole has only so many watches open, whoever has them.
#[tokio::test]
async fn the_sidecar_has_only_so_many_watches() {
    let dp = service(|dp| dp.watch_limits(10, 3)).await;
    let (a, b) = (open_on(&dp).await, open_on(&dp).await);
    let (_a1, _) = dp.watch_for_test(watch_req(&a, pb::Kind::Counter)).await.expect("a1");
    let (_a2, _) = dp.watch_for_test(watch_req(&a, pb::Kind::Counter)).await.expect("a2");
    let (b1, b1_task) = dp.watch_for_test(watch_req(&b, pb::Kind::Counter)).await.expect("b1");
    refused_as_too_many(dp.watch_for_test(watch_req(&b, pb::Kind::Counter)).await);
    refused_as_too_many(dp.watch_for_test(watch_req(&a, pb::Kind::Counter)).await);
    drop(b1);
    ended(&b1_task).await;
    let (_b2, _) = dp.watch_for_test(watch_req(&b, pb::Kind::Counter)).await.expect("room once one ended");
}

// The limits a sidecar starts with.
#[test]
fn the_watch_limits_are_what_the_readme_says() {
    assert_eq!(orvud::watch::MAX_WATCHES_PER_SESSION, 256);
    assert_eq!(orvud::watch::MAX_WATCHES, 4096);
}
