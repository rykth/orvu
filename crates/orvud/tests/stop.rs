//! When the sidecar stops serving. It stops when it is asked to, and also once
//! its engine can no longer save: then it cannot serve anything until it is
//! started again from what is on disk, so it stops and says why, and whatever
//! runs it can start it again.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::server::Stop;
use std::sync::Arc;
use std::time::Duration;

async fn client_on(store: &orvu_runtime::MemStore) -> Client {
    let fabric = Arc::new(MemTransport::new());
    let mut cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    cfg.fsync = orvu_runtime::FsyncPolicy::Always;
    Client::start_with_store(fabric.endpoint(1), cfg, Box::new(store.clone())).await.expect("engine")
}

#[tokio::test]
async fn a_sidecar_that_cannot_save_stops_and_says_why() {
    let store = orvu_runtime::MemStore::new();
    let client = client_on(&store).await;
    let counter = client.counter("visits").await.unwrap();
    counter.increment(1).await.unwrap();

    let waiting = {
        let client = client.clone();
        tokio::spawn(async move { orvud::server::until_stopped(&client, std::future::pending()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "stopped while saving still works");

    store.fail_saving(true);
    assert!(counter.increment(10).await.is_err(), "a change that was not saved was answered as done");
    let stop = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the sidecar kept serving after saving failed")
        .unwrap();
    match stop {
        Stop::CannotSave(why) => {
            let said = Stop::CannotSave(why).message().expect("a reason to give");
            assert!(said.starts_with("cannot save changes: "), "{said}");
            assert!(said.ends_with("stopping so that it can be started again from what is on disk"), "{said}");
        }
        Stop::Asked => panic!("stopped as if asked to"),
    }
    client.shutdown().await;
}

#[tokio::test]
async fn a_sidecar_asked_to_stop_stops_with_nothing_to_say() {
    let store = orvu_runtime::MemStore::new();
    let client = client_on(&store).await;
    let stop = tokio::time::timeout(Duration::from_secs(5), orvud::server::until_stopped(&client, async {}))
        .await
        .expect("did not stop when asked");
    assert!(matches!(stop, Stop::Asked));
    assert!(stop.message().is_none());
    client.shutdown().await;
}

/// A temporary folder that only its owner can get into, as the sidecar
/// wants for its data.
fn private_folder() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let folder = tempfile::tempdir().unwrap();
    std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    folder
}

/// A gRPC client connected to the socket at `path`.
async fn connect(
    path: &std::path::Path,
) -> orvud::pb::data_plane_client::DataPlaneClient<tonic::transport::Channel> {
    let path = path.to_path_buf();
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .expect("connects");
    orvud::pb::data_plane_client::DataPlaneClient::new(channel)
}

// The real program, whose disk stops taking writes. It answers the change
// with the reason, then exits with a failure and says why, so that whatever
// runs it starts it again. The disk is made to fail by limiting how big a
// file the running program may write, with `prlimit`; where that tool is
// missing the test has nothing to fail the disk with and does nothing.
#[tokio::test(flavor = "multi_thread")]
async fn the_program_exits_with_a_failure_once_it_cannot_save() {
    use orvud::pb;
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};
    if Command::new("prlimit").arg("--version").output().is_err() {
        eprintln!("prlimit is not installed; skipping");
        return;
    }
    let dir = private_folder();
    let socket = dir.path().join("s");
    // Writing past the limit sends a signal that would kill the program at
    // once. It is ignored, so the write fails with an error instead, as on a
    // full disk. Ignoring it carries over into the program.
    let mut child = Command::new("sh")
        .args(["-c", "trap '' XFSZ; exec \"$0\" \"$@\""])
        .arg(env!("CARGO_BIN_EXE_orvud"))
        .args(["--local", "--data-dir"])
        .arg(dir.path().join("data"))
        .arg("--socket")
        .arg(&socket)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    stderr.read_line(&mut said).unwrap();
    assert!(said.contains("serving"), "{said}");

    let mut client = connect(&socket).await;
    let opened = client
        .open(pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() })
        .await
        .unwrap()
        .into_inner();
    let add = |n| pb::Request {
        session: opened.session.clone(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n })),
        request_id: Vec::new(),
    };
    client.execute(add(1)).await.unwrap();

    let limited = Command::new("prlimit")
        .arg("--pid")
        .arg(child.id().to_string())
        .arg("--fsize=1")
        .status()
        .unwrap();
    assert!(limited.success(), "prlimit failed");
    let answer = client.execute(add(10)).await.unwrap().into_inner();
    match answer.result {
        Some(pb::response::Result::Failed(f)) => assert!(f.detail.starts_with("cannot save changes"), "{f:?}"),
        other => panic!("expected the reason it cannot save, got {other:?}"),
    }
    drop(client);

    let mut status = None;
    for _ in 0..100 {
        if let Some(s) = child.try_wait().unwrap() {
            status = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the program kept running after it could no longer save");
    };
    let mut rest = String::new();
    stderr.read_to_string(&mut rest).unwrap();
    assert!(!status.success(), "it exited as if nothing was wrong: {rest}");
    assert!(rest.contains("orvud: cannot save changes: "), "{rest}");
    assert!(rest.contains("stopping so that it can be started again from what is on disk"), "{rest}");
}

// ---- standard error that nobody reads -----------------------------------

/// Starts the real program on a new folder with `flags`, its standard error
/// going to `stderr`.
fn start_with_stderr(dir: &std::path::Path, flags: &[&str], stderr: std::process::Stdio) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
        .args(["--local", "--data-dir"])
        .arg(dir.join("data"))
        .arg("--socket")
        .arg(dir.join("s"))
        .args(flags)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap()
}

/// Reads the line the program prints once it is serving.
fn serving_line(from: impl std::io::Read) -> String {
    use std::io::BufRead;
    let mut said = String::new();
    std::io::BufReader::new(from).read_line(&mut said).unwrap();
    assert!(said.contains("serving"), "{said}");
    said
}

fn terminate(child: &std::process::Child) {
    let sent = std::process::Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap();
    assert!(sent.success());
}

/// How the program ended, waiting at most `limit`. A program still running
/// then is killed, and the test fails.
fn exit_within(child: &mut std::process::Child, limit: Duration, what: &str) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("{what}: the program was still running after {limit:?}");
}

// The program reading its standard error has gone. The sidecar still stops
// when nobody has been connected for the time it was given, and exits as
// usual.
#[test]
fn a_sidecar_whose_stderr_is_closed_still_stops_when_idle() {
    let dir = private_folder();
    let mut child = start_with_stderr(dir.path(), &["--exit-when-idle", "1"], std::process::Stdio::piped());
    serving_line(child.stderr.take().unwrap());
    // The reader is dropped here: writing to standard error now fails.
    let status = exit_within(&mut child, Duration::from_secs(15), "idle with its stderr closed");
    assert!(status.success(), "{status:?}");
}

// The same, stopped by SIGTERM.
#[test]
fn a_sidecar_whose_stderr_is_closed_still_stops_when_asked() {
    let dir = private_folder();
    let mut child = start_with_stderr(dir.path(), &[], std::process::Stdio::piped());
    serving_line(child.stderr.take().unwrap());
    terminate(&child);
    let status = exit_within(&mut child, Duration::from_secs(15), "SIGTERM with its stderr closed");
    assert!(status.success(), "{status:?}");
}

// Nobody reads standard error any more, and it is full. Writing to it would
// wait for ever, and the sidecar must stop all the same.
#[test]
fn a_sidecar_whose_stderr_is_full_still_stops_when_asked() {
    use std::io::Write;
    let dir = private_folder();
    let (reader, mut writer) = std::io::pipe().unwrap();
    let mut child = start_with_stderr(dir.path(), &[], writer.try_clone().unwrap().into());
    serving_line(reader.try_clone().unwrap());
    // Fill the pipe. This thread then waits for room that never comes, until
    // the reader is dropped at the end of the test.
    std::thread::spawn(move || while writer.write_all(&[b'.'; 512]).is_ok() {});
    std::thread::sleep(Duration::from_millis(500));
    terminate(&child);
    let status = exit_within(&mut child, Duration::from_secs(15), "SIGTERM with its stderr full");
    assert!(status.success(), "{status:?}");
    drop(reader);
}

// ---- stopping with work in progress -----------------------------------------

/// A service on a new engine that stops serving when `stopping` fires, with a
/// session on it.
async fn stopping_service(stopping: &tokio_util::sync::CancellationToken) -> (orvud::server::Dataplane, String) {
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.expect("engine"));
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default").stops_with(stopping.clone());
    let opened = dp
        .open_for_test(orvud::pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() })
        .await
        .unwrap();
    (dp, opened.session)
}

fn add_one(session: &str) -> orvud::pb::Request {
    use orvud::pb;
    pb::Request {
        session: session.into(),
        op: Some(pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 1 })),
        request_id: Vec::new(),
    }
}

fn is_stopping(status: &tonic::Status) {
    assert_eq!(status.code(), tonic::Code::Unavailable, "{status:?}");
    assert!(status.message().contains("stopping"), "{status:?}");
}

// Once the sidecar is stopping, no new work is taken.
#[tokio::test]
async fn a_stopping_sidecar_takes_no_new_work() {
    use orvud::pb;
    let stopping = tokio_util::sync::CancellationToken::new();
    let (dp, s) = stopping_service(&stopping).await;
    dp.execute_for_test(add_one(&s)).await.expect("taken while serving");
    stopping.cancel();
    is_stopping(&dp.execute_for_test(add_one(&s)).await.expect_err("taken while stopping"));
    let batch = pb::BatchRequest { session: s.clone(), requests: vec![add_one(&s)], request_id: Vec::new() };
    is_stopping(&dp.batch_for_test(batch).await.expect_err("taken while stopping"));
    let watch = pb::WatchRequest { session: s.clone(), object: "visits".into(), kind: pb::Kind::Counter as i32 };
    match dp.watch_for_test(watch).await {
        Err(status) => is_stopping(&status),
        Ok(_) => panic!("a watch was opened while stopping"),
    }
}

// A watch that is open when the sidecar starts stopping ends, and says why,
// so that it does not hold the stop up.
#[tokio::test]
async fn a_watch_ends_when_the_sidecar_starts_stopping() {
    use futures::StreamExt;
    use orvud::pb;
    let stopping = tokio_util::sync::CancellationToken::new();
    let (dp, s) = stopping_service(&stopping).await;
    dp.execute_for_test(add_one(&s)).await.unwrap();
    let watch = pb::WatchRequest { session: s.clone(), object: "visits".into(), kind: pb::Kind::Counter as i32 };
    let (mut stream, task) = dp.watch_for_test(watch).await.expect("opens");
    stream.next().await.expect("the first value").expect("a value");
    stopping.cancel();
    let end = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.expect("the watch went on");
    is_stopping(&end.expect("it says why").expect_err("a status"));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while task.upgrade().is_some() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(task.upgrade().is_none(), "the task behind the watch is still running");
}

// A batch that is running when the sidecar starts stopping does not run the
// rest of its operations. Each says it was not run, so the caller knows which
// were made.
#[tokio::test]
async fn a_batch_runs_no_more_once_the_sidecar_is_stopping() {
    use orvud::pb;
    let fabric = Arc::new(MemTransport::new());
    let client = Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.expect("engine");
    let stopping = tokio_util::sync::CancellationToken::new();
    stopping.cancel();
    let requests = (0..5).map(|_| add_one("s")).collect();
    let replies = orvud::exec::dispatch_batch(&client, "s", requests, stopping).await.expect("answered");
    assert_eq!(replies.len(), 5);
    for reply in replies {
        match reply.result {
            Some(pb::response::Result::Failed(f)) => assert!(f.detail.contains("stopping"), "{f:?}"),
            other => panic!("an operation ran while stopping: {other:?}"),
        }
    }
    let counter = client.counter("visits").await.unwrap();
    assert_eq!(counter.value().await.unwrap(), 0);
}

// Serving is given only so long to finish once the sidecar starts stopping.
// A request that never ends cannot keep it from stopping.
#[tokio::test(start_paused = true)]
async fn serving_is_given_only_so_long_to_finish() {
    let stopping = tokio_util::sync::CancellationToken::new();
    let served = orvud::server::finish_within(std::future::pending(), stopping.clone(), Duration::from_secs(5));
    tokio::pin!(served);
    assert!(tokio::time::timeout(Duration::from_secs(60), &mut served).await.is_err(), "it stopped unasked");
    stopping.cancel();
    let started = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(60), &mut served).await.expect("it went on serving").unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(5));
}

// The real program, with a watch open, stops promptly when asked to. The
// watch is told the sidecar is stopping.
#[tokio::test(flavor = "multi_thread")]
async fn the_program_with_a_watch_open_stops_when_asked() {
    use futures::StreamExt;
    use orvud::pb;
    let dir = private_folder();
    let mut child = start_with_stderr(dir.path(), &[], std::process::Stdio::piped());
    let stderr = child.stderr.take().unwrap();
    serving_line(stderr);
    let mut client = connect(&dir.path().join("s")).await;
    let opened = client
        .open(pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() })
        .await
        .unwrap()
        .into_inner();
    client.execute(add_one(&opened.session)).await.unwrap();
    let watch = pb::WatchRequest { session: opened.session.clone(), object: "visits".into(), kind: pb::Kind::Counter as i32 };
    let mut stream = client.watch(watch).await.unwrap().into_inner();
    stream.next().await.expect("the first value").expect("a value");
    terminate(&child);
    let end = tokio::time::timeout(Duration::from_secs(5), stream.next()).await.expect("the watch went on");
    is_stopping(&end.expect("it says why").expect_err("a status"));
    let status = exit_within(&mut child, Duration::from_secs(4), "SIGTERM with a watch open");
    assert!(status.success(), "{status:?}");
}
