//! How the sidecar itself can fail: its socket on disk and requests it cannot
//! accept. One sidecar serves many apps from one engine, so a bad request that
//! killed the process would take every app down with it.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn engine() -> Arc<Client> {
    let fabric = Arc::new(MemTransport::new());
    Arc::new(
        Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1"))
            .await
            .expect("engine"),
    )
}

async fn opened() -> (orvud::server::Dataplane, String) {
    let dp = orvud::server::Dataplane::new(engine().await, ReplicaId(1), "default");
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

/// Lists every entry directly under `parent` that other users could reach by
/// name and whose permissions are too open: a socket not at 0600, or a
/// directory not at 0700. A socket inside a 0700 directory cannot be reached
/// by other users, so it is not reported, whatever its own mode.
fn exposures(parent: &Path) -> Vec<String> {
    use std::os::unix::fs::FileTypeExt;
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(md) = entry.metadata() else { continue };
        let mode = md.permissions().mode() & 0o777;
        let name = entry.file_name().to_string_lossy().into_owned();
        if md.file_type().is_socket() && mode != 0o600 {
            found.push(format!("socket {name} at {mode:o}"));
        } else if md.is_dir() && mode != 0o700 {
            found.push(format!("directory {name} at {mode:o}"));
        }
    }
    found
}

// This runs on two threads so the loop below can look at the directory while
// the server is creating its socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_socket_is_never_reachable_while_permissive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("orvud.sock");
    let client = engine().await;
    let p = path.clone();
    let server =
        tokio::spawn(async move { orvud::server::serve_uds(&p, client, ReplicaId(1), "default").await });

    // Look at the directory as often as the scheduler allows, starting before
    // the server creates its socket, and check every look. Stop once the socket
    // is at 0600 and nothing else is left in the directory. The loop does not
    // sleep. The deadline is what stops it if that never happens.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut violations = BTreeSet::new();
    let mut settled = false;
    while Instant::now() < deadline {
        violations.extend(exposures(dir.path()));
        let mode = std::fs::metadata(&path)
            .ok()
            .map(|m| m.permissions().mode() & 0o777);
        let entries = std::fs::read_dir(dir.path()).expect("read dir").count();
        if mode == Some(0o600) && entries == 1 {
            settled = true;
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        settled,
        "the socket reached 0600 with nothing else left, within the deadline"
    );
    assert!(
        violations.is_empty(),
        "a socket was reachable while permissive: {violations:?}"
    );

    server.abort();
}

#[tokio::test]
async fn a_socket_path_too_long_to_stage_is_refused_with_the_limit_named() {
    let base = tempfile::tempdir().expect("tempdir");
    // Make the parent directory's name long enough that the operator's path
    // fits in the limit, but the temporary name the socket is first created
    // under does not.
    let pad = 100usize.saturating_sub(base.path().as_os_str().len() + 1);
    let parent = base.path().join("p".repeat(pad));
    std::fs::create_dir(&parent).expect("parent");
    let path = parent.join("ok");
    assert!(
        path.as_os_str().len() <= 107,
        "the operator's own path fits, so only the staged name is over the limit"
    );

    let err = orvud::server::serve_uds(&path, engine().await, ReplicaId(1), "default")
        .await
        .expect_err("a staged name past the limit is refused up front");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("107"),
        "the error names the limit: {err}"
    );
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0, "nothing was staged");
}

#[tokio::test]
async fn a_non_socket_at_the_socket_path_is_refused_not_clobbered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("orvud.sock");
    std::fs::write(&path, b"an operator's file").expect("write");

    let err = orvud::server::serve_uds(&path, engine().await, ReplicaId(1), "default")
        .await
        .expect_err("a regular file at the socket path must not be replaced");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).expect("read"), b"an operator's file");
}

/// A socket file on disk does not mean the program that created it has
/// stopped. Deleting one that is still in use would cut off a working `orvud`,
/// and its clients would be left with a path that nothing answers any more.
#[tokio::test]
async fn a_socket_someone_is_still_serving_is_refused_not_unlinked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("orvud.sock");
    let live = tokio::net::UnixListener::bind(&path).expect("a daemon already here");

    let err = orvud::server::serve_uds(&path, engine().await, ReplicaId(1), "default")
        .await
        .expect_err("a socket still being served must not be unlinked");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    assert!(path.exists(), "the live socket is still in place");
    drop(live);
}

// A request with no operation is covered in ops_basic.rs by
// `a_request_with_no_op_is_invalid_argument`, so it is not tested again here.

#[tokio::test]
async fn an_unknown_class_number_in_declare_is_rejected_not_defaulted() {
    let (dp, s) = opened().await;

    // 99 is not a Class this build knows. A client generated from a newer
    // proto file could send it. If the sidecar guessed AP, an object meant to
    // be Escrow would be handled as AP with no error. The ops_basic test covers
    // Unspecified (0). This one covers a number that is not a known class.
    let err = dp
        .execute_for_test(pb::Request {
            session: s,
            op: Some(pb::request::Op::Declare(pb::Declare {
                object: "x".into(),
                object_class: 99,
            })),
            request_id: Vec::new(),
        })
        .await
        .expect_err("an unknown class must not be guessed");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn the_engine_survives_a_rejected_request() {
    let (dp, s) = opened().await;

    let _ = dp
        .execute_for_test(pb::Request {
            session: s.clone(),
            op: None,
            request_id: Vec::new(),
        })
        .await;

    let ok = dp
        .execute_for_test(pb::Request {
            session: s,
            op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement {
                object: "visits".into(),
                n: 1,
            })),
            request_id: Vec::new(),
        })
        .await
        .expect("the engine still serves after a rejected request");
    assert!(matches!(ok.result, Some(pb::response::Result::Ack(_))));
}

// The socket can be put in place before any engine exists, so a bad socket
// path is found before the machine joins the others. Dropping it without
// serving removes the file again.
#[tokio::test]
async fn a_socket_is_bound_without_an_engine_and_removed_when_dropped() {
    let base = tempfile::tempdir().expect("tempdir");
    let path = base.path().join("orvud.sock");
    let bound = orvud::server::bind_uds(&path).await.expect("binds");
    assert!(path.exists());
    drop(bound);
    assert!(!path.exists(), "the socket file was left behind");
    assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 0, "something was left behind");
}

// Asked to stop, the sidecar returns from serving and takes its socket file
// with it.
#[tokio::test]
async fn serving_stops_when_asked_and_removes_the_socket() {
    let base = tempfile::tempdir().expect("tempdir");
    let path = base.path().join("orvud.sock");
    let bound = orvud::server::bind_uds(&path).await.expect("binds");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn({
        let client = engine().await;
        async move {
            orvud::server::serve_bound(bound, client, ReplicaId(1), "default", async {
                let _ = stopped.await;
            })
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(path.exists());

    stop.send(()).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), serving)
        .await
        .expect("serving did not stop")
        .unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(!path.exists(), "the socket file was left behind");
}

// A second sidecar on the same path is refused, and the first keeps its socket.
#[tokio::test]
async fn a_second_bind_on_a_live_socket_is_refused_and_leaves_it_alone() {
    let base = tempfile::tempdir().expect("tempdir");
    let path = base.path().join("orvud.sock");
    let first = orvud::server::bind_uds(&path).await.expect("binds");
    let err = orvud::server::bind_uds(&path).await.err().expect("the path is in use");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    assert!(path.exists(), "the first sidecar lost its socket");
    drop(first);
}
