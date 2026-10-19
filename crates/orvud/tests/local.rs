//! Starting with no setup: one data folder, no settings file, no certificates
//! and no network.

use orvud::local;
use orvud::pb;
use orvud::pb::request::Op;

/// A temporary folder that only its owner can get into. The sidecar refuses a
/// data folder that is open to other users, and a temporary folder is not
/// always private when it is made.
fn private_folder() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let folder = tempfile::tempdir().unwrap();
    std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    folder
}

async fn service(folder: &std::path::Path) -> (local::Local, orvud::server::Dataplane, String) {
    let engine = local::start(folder).await.expect("starts");
    let dp = orvud::server::Dataplane::new(engine.client.clone(), engine.replica, &engine.namespace);
    let session = dp
        .open_for_test(pb::OpenRequest {
            namespace: String::new(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open")
        .session;
    (engine, dp, session)
}

async fn exec(dp: &orvud::server::Dataplane, session: &str, op: Op) -> pb::Response {
    dp.execute_for_test(pb::Request { session: session.into(), op: Some(op), request_id: Vec::new() }).await.expect("request")
}

fn int_of(r: pb::Response) -> i64 {
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => i,
        other => panic!("expected a number, got {other:?}"),
    }
}

// The whole point of local mode: start in an empty folder, store a value, stop,
// start again, and the value is still there.
#[tokio::test]
async fn a_value_survives_a_restart() {
    let folder = private_folder();
    let data = folder.path().join("data"); // does not exist yet

    let (engine, dp, session) = service(&data).await;
    exec(&dp, &session, Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 5 })).await;
    engine.client.shutdown().await;
    drop((dp, engine));

    let (engine, dp, session) = service(&data).await;
    let read = exec(&dp, &session, Op::CounterValue(pb::CounterValue { object: "visits".into() })).await;
    assert_eq!(int_of(read), 5);
    engine.client.shutdown().await;
}

// Local mode must not behave differently from a networked sidecar. The other
// test files already run every operation against an engine with no network.
// This one checks a spread of them against an engine started the local way: a
// write and a read on several types, a refusal, and a read of something that
// is not there.
#[tokio::test]
async fn the_usual_operations_work_the_same_in_local_mode() {
    let folder = private_folder();
    let (engine, dp, s) = service(folder.path()).await;
    let ack = |r: pb::Response| assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "{r:?}");

    ack(exec(&dp, &s, Op::RegisterSet(pb::RegisterSet { object: "title".into(), value: "Plan".into() })).await);
    ack(exec(&dp, &s, Op::SetAdd(pb::SetAdd { object: "tags".into(), member: "a".into() })).await);
    ack(exec(&dp, &s, Op::ListInsert(pb::ListInsert { object: "notes".into(), index: 0, value: "x".into() })).await);
    ack(exec(&dp, &s, Op::JsonSet(pb::JsonSet {
        object: "doc".into(),
        path_json: r#"["done"]"#.into(),
        value_json: "true".into(),
    })).await);
    ack(exec(&dp, &s, Op::BoundedIncrement(pb::BoundedIncrement { object: "budget".into(), n: 3 })).await);

    let read = exec(&dp, &s, Op::RegisterGet(pb::RegisterGet { object: "title".into() })).await;
    assert!(matches!(
        read.result,
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::StringValue(ref t)) })) if t == "Plan"
    ));
    let len = exec(&dp, &s, Op::ListLen(pb::ListLen { object: "notes".into() })).await;
    assert_eq!(int_of(len), 1);

    let too_much = Op::BoundedTryDecrement(pb::BoundedTryDecrement { object: "budget".into(), n: 10 });
    assert!(matches!(exec(&dp, &s, too_much).await.result, Some(pb::response::Result::Refused(_))));

    let missing = exec(&dp, &s, Op::CounterValue(pb::CounterValue { object: "nothing".into() })).await;
    assert!(matches!(
        missing.result,
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) }))
    ));
    engine.client.shutdown().await;
}

#[tokio::test]
async fn the_machine_number_is_the_same_after_a_restart() {
    let folder = private_folder();

    let first = local::start(folder.path()).await.unwrap();
    let number = first.replica;
    assert_ne!(number.0, 0);
    first.client.shutdown().await;
    drop(first);

    let second = local::start(folder.path()).await.unwrap();
    assert_eq!(second.replica, number);
    second.client.shutdown().await;
}

#[tokio::test]
async fn two_folders_get_different_machine_numbers() {
    let (a, b) = (private_folder(), private_folder());
    let first = local::start(a.path()).await.unwrap();
    let second = local::start(b.path()).await.unwrap();
    assert_ne!(first.replica, second.replica);
    first.client.shutdown().await;
    second.client.shutdown().await;
}

#[tokio::test]
async fn a_second_sidecar_on_the_same_folder_is_refused() {
    let folder = private_folder();
    let first = local::start(folder.path()).await.unwrap();

    let err = local::start(folder.path()).await.err().expect("the folder is in use");
    let message = err.to_string();
    assert!(message.contains("already using"), "{message}");
    assert!(message.contains(&folder.path().display().to_string()), "{message}");

    // Once the first one is gone, the folder can be used again.
    first.client.shutdown().await;
    drop(first);
    let again = local::start(folder.path()).await.expect("the folder is free again");
    again.client.shutdown().await;
}

#[tokio::test]
async fn a_damaged_machine_number_file_is_reported_not_replaced() {
    let folder = private_folder();
    std::fs::write(folder.path().join("replica"), "not a number\n").unwrap();
    let err = local::start(folder.path()).await.err().expect("the file is damaged");
    assert!(err.to_string().contains("replica"), "{err}");
    assert_eq!(std::fs::read_to_string(folder.path().join("replica")).unwrap(), "not a number\n");
}

#[tokio::test]
async fn the_data_folder_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let folder = private_folder();
    let data = folder.path().join("data");
    let engine = local::start(&data).await.unwrap();
    assert_eq!(std::fs::metadata(&data).unwrap().permissions().mode() & 0o777, 0o700);
    engine.client.shutdown().await;
}

// A folder that already exists is used as it is, so it has to be private
// already. One that other users can read is refused, with the command that
// fixes it.
#[tokio::test]
async fn an_existing_folder_other_users_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let folder = private_folder();
    let data = folder.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();

    let err = local::start(&data).await.err().expect("the folder is open to others");
    let message = err.to_string();
    assert!(message.contains("chmod 700"), "{message}");
    assert_eq!(std::fs::read_dir(&data).unwrap().count(), 0, "something was written into it");

    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let engine = local::start(&data).await.expect("a private folder is fine");
    engine.client.shutdown().await;
}

// The saved data was written under one machine number. If the number file is
// gone, starting under a new number would strand whatever the old one held, so
// the start is refused.
#[tokio::test]
async fn saved_data_without_its_machine_number_is_refused() {
    let folder = private_folder();
    let first = local::start(folder.path()).await.unwrap();
    first.client.shutdown().await;
    drop(first);

    std::fs::remove_file(folder.path().join("replica")).unwrap();
    let err = local::start(folder.path()).await.err().expect("the number is missing");
    let message = err.to_string();
    assert!(message.contains("replica"), "{message}");
    assert!(!folder.path().join("replica").exists(), "a new number was picked anyway");
}

// The folder can be claimed before the engine starts. Whatever fails in
// between, such as putting the socket in place, then leaves no saved data and
// no machine number behind.
#[tokio::test]
async fn claiming_a_folder_writes_nothing_but_the_lock() {
    let folder = private_folder();
    let data = folder.path().join("data");
    let claimed = local::claim(&data).expect("claims");
    let names: Vec<_> =
        std::fs::read_dir(&data).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, ["orvud.lock"]);

    // A second claim is refused while the first is held.
    assert!(local::claim(&data).is_err());

    let engine = claimed.start(local::IfCopied::Refuse).await.expect("starts");
    assert!(data.join("replica").exists());
    engine.client.shutdown().await;
}

// ---- a data folder belongs to one computer ---------------------------

/// Starts the engine in `folder` as if on the computer called `computer`.
async fn start_on(folder: &std::path::Path, computer: &str, if_copied: local::IfCopied) -> orvu_sdk::Result<local::Local> {
    local::claim(folder)?.start_on(Some(computer), if_copied).await
}

async fn stop(engine: local::Local) {
    engine.client.shutdown().await;
    drop(engine);
}

// A data folder copied to a second computer would make two machines share a
// number. The folder notes which computer it is used on, and another one is
// refused, with a message that says what happened and how to go on.
#[tokio::test]
async fn a_folder_copied_to_another_computer_is_refused() {
    let folder = private_folder();
    let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    let number = std::fs::read_to_string(folder.path().join("replica")).unwrap();
    stop(first).await;

    let err = start_on(folder.path(), "computer-two", local::IfCopied::Refuse).await.err().expect("a copy was started");
    let message = err.to_string();
    assert!(message.contains("copied from another computer"), "{message}");
    assert!(message.contains("--new-number") && message.contains("--moved"), "{message}");
    assert!(message.contains(&folder.path().display().to_string()), "{message}");
    // The commands to run are given whole, with the folder in them.
    for flag in ["--new-number", "--moved"] {
        let command = format!("orvud --local --data-dir {} {flag}", folder.path().display());
        assert!(message.contains(&command), "{message}");
    }
    assert_eq!(std::fs::read_to_string(folder.path().join("replica")).unwrap(), number, "the number was changed");

    // On its own computer it still starts, under its number.
    let again = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.expect("starts at home");
    assert_eq!(format!("{}\n", again.replica.0), number);
    stop(again).await;
}

// A copy that is to go on as a machine of its own keeps the data and takes a
// new number. What it writes from then on is under the new number, and the
// folder now belongs to this computer.
#[tokio::test]
async fn a_copy_can_take_a_new_number_and_keep_the_data() {
    let folder = private_folder();
    let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    let old = first.replica;
    first.client.counter("visits").await.unwrap().increment(5).await.unwrap();
    stop(first).await;

    let copy = start_on(folder.path(), "computer-two", local::IfCopied::NewNumber).await.expect("starts as a copy");
    assert_ne!(copy.replica, old, "the copy kept the old number");
    let visits = copy.client.counter("visits").await.unwrap();
    assert_eq!(visits.value().await.unwrap(), 5, "the data was not kept");
    visits.increment(2).await.unwrap();
    assert_eq!(visits.value().await.unwrap(), 7);
    let new = copy.replica;
    stop(copy).await;

    // From now on the folder is this computer's, under the new number.
    let again = start_on(folder.path(), "computer-two", local::IfCopied::Refuse).await.expect("starts again");
    assert_eq!(again.replica, new);
    assert_eq!(again.client.counter("visits").await.unwrap().value().await.unwrap(), 7);
    stop(again).await;
    assert!(start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.is_err());
}

// A folder restored from a backup, or rolled back some other way, has lost
// changes it may already have sent. Writing under its old number would reuse
// numbers the others have already seen, and those writes would be lost. So
// it can take a new number on its own computer too, keeping the data.
#[tokio::test]
async fn a_restored_folder_can_take_a_new_number_on_its_own_computer() {
    for computer in [Some("computer-one"), None] {
        let folder = private_folder();
        let first = local::claim(folder.path()).unwrap().start_on(computer, local::IfCopied::Refuse).await.unwrap();
        let old = first.replica;
        first.client.counter("visits").await.unwrap().increment(5).await.unwrap();
        stop(first).await;

        let restored = local::claim(folder.path())
            .unwrap()
            .start_on(computer, local::IfCopied::Restored)
            .await
            .expect("starts after a restore");
        assert_ne!(restored.replica, old, "{computer:?}: the restored folder kept its old number");
        let visits = restored.client.counter("visits").await.unwrap();
        assert_eq!(visits.value().await.unwrap(), 5, "{computer:?}: the data was not kept");
        visits.increment(2).await.unwrap();
        let new = restored.replica;
        stop(restored).await;

        // Later starts need no flag and keep the new number.
        let again = local::claim(folder.path()).unwrap().start_on(computer, local::IfCopied::Refuse).await.unwrap();
        assert_eq!(again.replica, new, "{computer:?}");
        assert_eq!(again.client.counter("visits").await.unwrap().value().await.unwrap(), 7);
        stop(again).await;
    }
}

// A folder that was moved, not copied, keeps its number on the new computer.
#[tokio::test]
async fn a_moved_folder_can_keep_its_number() {
    let folder = private_folder();
    let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    let number = first.replica;
    stop(first).await;

    let moved = start_on(folder.path(), "computer-two", local::IfCopied::AcceptMove).await.expect("starts");
    assert_eq!(moved.replica, number);
    stop(moved).await;
    let again = start_on(folder.path(), "computer-two", local::IfCopied::Refuse).await.expect("starts again");
    assert_eq!(again.replica, number);
    stop(again).await;
    assert!(start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.is_err());
}

// On the computer the folder belongs to, asking for a new number or saying
// it was moved changes nothing. A script that always passes the flag must
// not get a new number on every start.
#[tokio::test]
async fn the_flags_change_nothing_on_the_folders_own_computer() {
    let folder = private_folder();
    let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    let number = first.replica;
    stop(first).await;
    for if_copied in [local::IfCopied::NewNumber, local::IfCopied::AcceptMove] {
        let again = start_on(folder.path(), "computer-one", if_copied).await.unwrap();
        assert_eq!(again.replica, number, "{if_copied:?}");
        stop(again).await;
    }
}

// A folder from before computers were noted has no note. It is taken to
// belong to the computer it is next started on, and is checked from then on.
#[tokio::test]
async fn a_folder_with_no_note_of_its_computer_gets_one() {
    let folder = private_folder();
    let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    let number = first.replica;
    stop(first).await;
    std::fs::remove_file(folder.path().join("computer")).unwrap();

    let again = start_on(folder.path(), "computer-two", local::IfCopied::Refuse).await.expect("no note, no refusal");
    assert_eq!(again.replica, number);
    stop(again).await;
    assert!(folder.path().join("computer").exists());
    assert!(start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.is_err());
}

// The data notes the number it was written as, beside it, as it does when
// started from settings. A copy that takes a new number notes the new one.
#[tokio::test]
async fn the_data_notes_its_machine_number() {
    let folder = private_folder();
    let note = folder.path().join("orvu.redb.replica");
    let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    assert_eq!(std::fs::read_to_string(&note).unwrap(), format!("{}\n", first.replica.0));
    stop(first).await;

    let copy = start_on(folder.path(), "computer-two", local::IfCopied::NewNumber).await.unwrap();
    assert_eq!(std::fs::read_to_string(&note).unwrap(), format!("{}\n", copy.replica.0));
    stop(copy).await;
}

// Taking a new number writes three files. If it stops after any of them, the
// folder still looks copied, so the next start is refused the same way and
// taking a new number can simply be tried again. It is never told to go back
// to the number it was leaving behind.
#[tokio::test]
async fn a_new_number_that_stops_halfway_can_be_taken_again() {
    for stops_at in ["orvu.redb.replica", "replica", "computer"] {
        let folder = private_folder();
        let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
        let old = first.replica;
        first.client.counter("visits").await.unwrap().increment(5).await.unwrap();
        stop(first).await;

        // A folder where the file is to be staged makes writing it fail.
        let blocker = folder.path().join(format!("{stops_at}.new"));
        std::fs::create_dir(&blocker).unwrap();
        assert!(start_on(folder.path(), "computer-two", local::IfCopied::NewNumber).await.is_err(), "{stops_at}");
        std::fs::remove_dir(&blocker).unwrap();

        let message = start_on(folder.path(), "computer-two", local::IfCopied::Refuse)
            .await
            .err()
            .unwrap_or_else(|| panic!("{stops_at}: started"))
            .to_string();
        assert!(message.contains("copied from another computer"), "{stops_at}: {message}");
        assert!(!message.contains(&format!("put {} in", old.0)), "{stops_at}: {message}");

        let copy = start_on(folder.path(), "computer-two", local::IfCopied::NewNumber).await.expect("starts as a copy");
        assert_ne!(copy.replica, old, "{stops_at}");
        assert_eq!(copy.client.counter("visits").await.unwrap().value().await.unwrap(), 5);
        let new = copy.replica;
        stop(copy).await;
        let again = start_on(folder.path(), "computer-two", local::IfCopied::Refuse).await.expect("starts again");
        assert_eq!(again.replica, new, "{stops_at}");
        stop(again).await;
    }
}

// A `replica` file that says one machine and a note on the data that says
// another is refused, and the message says which file to put right.
#[tokio::test]
async fn a_replica_file_the_data_disagrees_with_is_refused() {
    let folder = private_folder();
    let first = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    let number = first.replica.0;
    stop(first).await;
    let other = if number == 1 { 2 } else { number - 1 };
    std::fs::write(folder.path().join("replica"), format!("{other}\n")).unwrap();

    let message = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.err().expect("started").to_string();
    let replica = folder.path().join("replica").display().to_string();
    assert!(message.contains(&format!("put {number} in {replica}")), "{message}");
}

// The note does not hold the computer's own id, which is meant to be kept
// private, only a value worked out from it.
#[tokio::test]
async fn the_note_does_not_give_the_computer_away() {
    let folder = private_folder();
    let engine = start_on(folder.path(), "secret-machine-id", local::IfCopied::Refuse).await.unwrap();
    stop(engine).await;
    let note = std::fs::read_to_string(folder.path().join("computer")).unwrap();
    assert!(!note.contains("secret-machine-id"), "{note}");
}

/// Runs the real program on `folder` with `flags`, until it stops by itself
/// a second after it starts. Returns whether it started, and what it said.
fn run_orvud(folder: &std::path::Path, flags: &[&str]) -> (bool, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
        .args(["--local", "--data-dir"])
        .arg(folder)
        .arg("--socket")
        .arg(folder.join("s"))
        .args(["--exit-when-idle", "1"])
        .args(flags)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    (out.status.success() && said.contains("serving"), said)
}

// The same through the program and its flags.
#[test]
fn the_program_refuses_a_copied_folder_until_told_what_it_is() {
    let folder = private_folder();
    let (started, said) = run_orvud(folder.path(), &[]);
    assert!(started, "{said}");
    let number = std::fs::read_to_string(folder.path().join("replica")).unwrap();
    // As if the folder had come from another computer.
    std::fs::write(folder.path().join("computer"), "somewhere else\n").unwrap();

    let (started, said) = run_orvud(folder.path(), &[]);
    assert!(!started, "a copied folder was started: {said}");
    assert!(said.contains("copied from another computer") && said.contains("--new-number"), "{said}");

    let (started, said) = run_orvud(folder.path(), &["--moved"]);
    assert!(started, "{said}");
    assert_eq!(std::fs::read_to_string(folder.path().join("replica")).unwrap(), number);

    std::fs::write(folder.path().join("computer"), "somewhere else\n").unwrap();
    let (started, said) = run_orvud(folder.path(), &["--new-number"]);
    assert!(started, "{said}");
    assert_ne!(std::fs::read_to_string(folder.path().join("replica")).unwrap(), number);
}

// Only the machine id tells computers apart. A host name can change under a
// computer that stays the same, and would make its own folder look copied.
#[test]
fn this_computer_is_known_by_its_machine_id_only() {
    let id = std::fs::read_to_string("/etc/machine-id").ok().map(|id| id.trim().to_string()).filter(|id| !id.is_empty());
    assert_eq!(local::this_computer(), id.map(|id| format!("machine-id {id}")));
    assert_eq!(local::this_computer(), local::this_computer());
}

// Where the computer cannot be told, no note is kept and none is checked: a
// folder is never refused for a name this computer cannot vouch for.
#[tokio::test]
async fn a_computer_that_cannot_be_told_keeps_no_note_and_is_not_refused() {
    let folder = private_folder();
    let engine = local::claim(folder.path()).unwrap().start_on(None, local::IfCopied::Refuse).await.unwrap();
    let number = engine.replica;
    stop(engine).await;
    assert!(!folder.path().join("computer").exists());

    // A folder noted for a computer is not refused either, and its note is
    // left as it was.
    let noted = start_on(folder.path(), "computer-one", local::IfCopied::Refuse).await.unwrap();
    stop(noted).await;
    let note = std::fs::read_to_string(folder.path().join("computer")).unwrap();
    let engine = local::claim(folder.path()).unwrap().start_on(None, local::IfCopied::Refuse).await.expect("refused");
    assert_eq!(engine.replica, number);
    stop(engine).await;
    assert_eq!(std::fs::read_to_string(folder.path().join("computer")).unwrap(), note);
}

#[test]
fn the_default_folder_follows_the_system() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| std::ffi::OsString::from(v))
    };
    if cfg!(target_os = "macos") {
        assert_eq!(
            local::default_data_dir(env(&[("HOME", "/Users/a")])).unwrap(),
            std::path::PathBuf::from("/Users/a/Library/Application Support/orvu")
        );
    } else {
        assert_eq!(
            local::default_data_dir(env(&[("HOME", "/home/a")])).unwrap(),
            std::path::PathBuf::from("/home/a/.local/share/orvu")
        );
        assert_eq!(
            local::default_data_dir(env(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "/data")])).unwrap(),
            std::path::PathBuf::from("/data/orvu")
        );
    }
    assert!(local::default_data_dir(env(&[])).is_none());
    // A path that is not absolute is ignored, as the XDG convention says. The
    // folder would otherwise depend on where the program was started.
    assert!(local::default_data_dir(env(&[("HOME", "home/a")])).is_none());
    if !cfg!(target_os = "macos") {
        assert_eq!(
            local::default_data_dir(env(&[("HOME", "/home/a"), ("XDG_DATA_HOME", "data")])).unwrap(),
            std::path::PathBuf::from("/home/a/.local/share/orvu")
        );
    }
    // Two starts with no flags get the same folder.
    assert_eq!(
        local::default_data_dir(env(&[("HOME", "/home/a")])),
        local::default_data_dir(env(&[("HOME", "/home/a")]))
    );
}

#[test]
fn the_default_socket_is_private() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| std::ffi::OsString::from(v))
    };
    let data = std::path::Path::new("/home/a/.local/share/orvu");
    assert_eq!(
        local::default_socket(data, env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])),
        std::path::PathBuf::from("/run/user/1000/orvud.sock")
    );
    assert_eq!(local::default_socket(data, env(&[])), data.join("orvud.sock"));
    assert_eq!(local::default_socket(data, env(&[("XDG_RUNTIME_DIR", "run")])), data.join("orvud.sock"));
}

// ---- over a real socket ------------------------------------------------

use orvud::pb::data_plane_client::DataPlaneClient;
use std::time::Duration;

/// A gRPC client connected to the socket at `path`.
async fn connect(path: &std::path::Path) -> DataPlaneClient<tonic::transport::Channel> {
    let path = path.to_path_buf();
    // The address is not used: the connector below opens the socket instead.
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
    DataPlaneClient::new(channel)
}

fn open_request() -> pb::OpenRequest {
    pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() }
}

// A real client, over the real socket, against an engine started with no
// setup. It is told which version answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_connects_over_the_socket_and_is_told_the_version() {
    let folder = private_folder();
    let socket = folder.path().join("orvud.sock");
    let engine = local::start(&folder.path().join("data")).await.unwrap();
    let bound = orvud::server::bind_uds(&socket).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn({
        let (client, replica, namespace) = (engine.client.clone(), engine.replica, engine.namespace.clone());
        async move {
            orvud::server::serve_bound(bound, client, replica, &namespace, async {
                let _ = stopped.await;
            })
            .await
        }
    });

    let mut client = connect(&socket).await;
    let opened = client.open(open_request()).await.unwrap().into_inner();
    assert_eq!(opened.version, orvud::VERSION);
    assert_eq!(opened.replica_id, engine.replica.0);

    let add = Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 5 });
    client.execute(pb::Request { session: opened.session.clone(), op: Some(add), request_id: Vec::new() }).await.unwrap();
    let read = Op::CounterValue(pb::CounterValue { object: "visits".into() });
    let reply = client.execute(pb::Request { session: opened.session, op: Some(read), request_id: Vec::new() }).await.unwrap();
    assert_eq!(int_of(reply.into_inner()), 5);

    drop(client);
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
    engine.client.shutdown().await;
}

// With the idle limit on, the sidecar stays up while a client is connected,
// even a silent one, and stops once the client has been gone for the limit.
#[tokio::test(flavor = "multi_thread")]
async fn it_stops_when_idle_but_not_while_a_client_is_connected() {
    let folder = private_folder();
    let socket = folder.path().join("orvud.sock");
    let engine = local::start(&folder.path().join("data")).await.unwrap();
    let bound = orvud::server::bind_uds(&socket).await.unwrap();
    let connections = orvud::server::Connections::default();
    let limit = Duration::from_millis(300);
    let serving = tokio::spawn({
        let (client, replica, namespace) = (engine.client.clone(), engine.replica, engine.namespace.clone());
        let connections = connections.clone();
        connections.stop_when_idle(limit);
        async move {
            orvud::server::serve_bound_counted(bound, client, replica, &namespace, connections, std::future::pending())
                .await
        }
    });

    let mut client = connect(&socket).await;
    client.open(open_request()).await.unwrap();
    assert_eq!(connections.open(), 1);

    // Well past the limit, with the client connected and saying nothing.
    tokio::time::sleep(limit * 3).await;
    assert!(!serving.is_finished(), "it stopped while a client was connected");

    drop(client);
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .expect("it did not stop after the client left")
        .unwrap()
        .unwrap();
    engine.client.shutdown().await;
}

// A local sidecar saves each change to disk before it answers, so a change it
// has reported as done survives the sidecar being killed.
#[test]
fn a_local_sidecar_saves_each_change_before_it_answers() {
    let folder = private_folder();
    let config = local::node_config(folder.path(), orvu_crdt::ReplicaId(7)).unwrap();
    assert_eq!(config.fsync, orvu_runtime::FsyncPolicy::Always);
}

// Saving every so often instead is a choice made in `orvud.toml`.
#[test]
fn saving_every_so_often_is_chosen_in_the_settings_file() {
    use orvu_runtime::FsyncPolicy;
    let folder = private_folder();
    let settings = folder.path().join("orvud.toml");
    for (text, want) in [
        ("[storage]\nfsync = \"interval\"\n", FsyncPolicy::Interval { ms: 100 }),
        ("[storage]\nfsync = \"interval\"\ninterval_ms = 250\n", FsyncPolicy::Interval { ms: 250 }),
        ("[storage]\nfsync = \"always\"\n", FsyncPolicy::Always),
    ] {
        std::fs::write(&settings, text).unwrap();
        let config = local::node_config(folder.path(), orvu_crdt::ReplicaId(7)).unwrap();
        assert_eq!(config.fsync, want, "{text:?}");
    }
}
