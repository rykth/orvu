//! The replica id a client is told must be the one the engine runs under, and
//! the namespace a session asks for must be the one the config declares.

use orvu_crdt::ReplicaId;
use orvud::{boot, pb};

/// A config whose `[security]` paths point into `certs`.
fn config(replica: u64, certs: &std::path::Path) -> String {
    let certs = certs.display();
    format!(
        "replica = {replica}\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n\
         [security]\ncert = \"{certs}/node.pem\"\nkey = \"{certs}/node-key.pem\"\n\
         ca = \"{certs}/ca.pem\"\n"
    )
}

/// Makes a certificate authority and one certificate for `replica`, and writes
/// them where `config` points. They are made new on every run, so no private
/// key is stored in the repository.
fn write_certs(dir: &std::path::Path, replica: u64) {
    let ca = orvu_transport::generate_dev_ca();
    let node = orvu_transport::issue_node_cert(&ca, replica);
    std::fs::write(dir.join("ca.pem"), &ca.cert_pem).unwrap();
    std::fs::write(dir.join("node.pem"), &node.cert_pem).unwrap();
    std::fs::write(dir.join("node-key.pem"), &node.key_pem).unwrap();
}

#[tokio::test]
async fn open_reports_the_replica_id_the_config_declares() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.toml");
    write_certs(dir.path(), 7);
    std::fs::write(&path, config(7, dir.path())).unwrap();

    // This starts the engine the way the real program does, with
    // `Client::from_toml`, over QUIC.
    let (client, replica, namespace, _claim) = boot::from_toml(&path).await.expect("starts");
    assert_eq!(replica, ReplicaId(7));
    assert_eq!(namespace, "n", "the namespace comes from the same file as the engine");

    let dp = orvud::server::Dataplane::new(client, replica, &namespace);
    let resp = dp
        .open_for_test(open_req("n"))
        .await
        .expect("the config's own namespace is served");
    assert_eq!(resp.replica_id, 7);

    // An empty namespace means "whatever this sidecar serves", so it opens too.
    dp.open_for_test(open_req("")).await.expect("an empty namespace is the sidecar's own");

    // The namespace field is checked. The config declares "n", so asking for
    // "default" is rejected. The sidecar does not answer from the namespace it
    // has when the client asked for another one.
    let err = dp
        .open_for_test(open_req("default"))
        .await
        .expect_err("a namespace this engine does not serve must be refused");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

fn open_req(ns: &str) -> pb::OpenRequest {
    pb::OpenRequest { namespace: ns.into(), resume_token: String::new(), credential: Vec::new() }
}

#[test]
fn the_toml_replica_is_what_the_parser_reports() {
    assert_eq!(boot::replica_of(&config(42, std::path::Path::new("/nonexistent"))).expect("parses"), ReplicaId(42));
}

// ---- the storage folder ------------------------------------------------

/// A config like `config`, that keeps its data in `storage`.
fn stored(replica: u64, certs: &std::path::Path, storage: &std::path::Path) -> String {
    format!("{}[storage]\npath = \"{}\"\n", config(replica, certs), storage.join("orvu.redb").display())
}

/// Writes certificates and a config for `replica` into `dir`, keeping data in
/// `storage`. Returns the config's path.
fn settings(dir: &std::path::Path, replica: u64, storage: &std::path::Path) -> std::path::PathBuf {
    write_certs(dir, replica);
    let path = dir.join("c.toml");
    std::fs::write(&path, stored(replica, dir, storage)).unwrap();
    path
}

// Two sidecars must not use one storage folder. The second is refused before
// its engine starts, whichever settings file it was started from.
#[tokio::test]
async fn a_second_sidecar_on_the_same_storage_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("data");
    let path = settings(dir.path(), 7, &storage);

    let (mut first, folder) = boot::read(&path).unwrap();
    let held = boot::claim(&mut first, &folder).expect("claims");
    let (mut second, folder) = boot::read(&path).unwrap();
    let message = boot::claim(&mut second, &folder).err().expect("the storage is in use").to_string();
    assert!(message.contains("already using"), "{message}");
    assert!(message.contains(&storage.display().to_string()), "{message}");

    drop(held);
    boot::claim(&mut second, &folder).expect("the storage is free again");
}

// The storage folder keeps the machine number it was first used with, and
// settings that later name another number for it are refused.
#[tokio::test]
async fn storage_keeps_the_number_it_was_first_used_with() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("data");
    let path = settings(dir.path(), 7, &storage);
    let (client, _, _, claim) = boot::from_toml(&path).await.expect("starts");
    client.shutdown().await;
    drop(claim);
    assert_eq!(std::fs::read_to_string(storage.join("replica")).unwrap(), "7\n");

    let path = settings(dir.path(), 8, &storage);
    let message = boot::from_toml(&path).await.err().expect("another number was accepted").to_string();
    assert!(message.contains("machine 7") && message.contains("replica = 8"), "{message}");
    assert_eq!(std::fs::read_to_string(storage.join("replica")).unwrap(), "7\n");
}

/// Starts from settings for `replica` on `storage`, adds `n` to a counter
/// and stops. What is saved stays in `storage`.
async fn run_once(dir: &std::path::Path, replica: u64, storage: &std::path::Path, n: u64) {
    let (client, _, _, claim) = boot::from_toml(&settings(dir, replica, storage)).await.expect("starts");
    client.counter("visits").await.unwrap().increment(n).await.unwrap();
    client.shutdown().await;
    drop(claim);
}

// Starting again under the number the storage was first used with works, and
// leaves the storage's notes as they were.
#[tokio::test]
async fn storage_starts_again_under_its_own_number() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("data");
    run_once(dir.path(), 7, &storage, 2).await;
    run_once(dir.path(), 7, &storage, 3).await;
    let (client, _, _, _claim) = boot::from_toml(&settings(dir.path(), 7, &storage)).await.expect("starts");
    assert_eq!(client.counter("visits").await.unwrap().value().await.unwrap(), 5);
    client.shutdown().await;
    assert_eq!(std::fs::read_to_string(storage.join("replica")).unwrap(), "7\n");
    assert_eq!(std::fs::read_to_string(storage.join("orvu.redb.replica")).unwrap(), "7\n");
}

// Saved data with no note of its number is refused under a number the
// settings name by mistake, and keeps being refused: the refusal writes
// nothing that a second start could take as leave to go ahead.
#[tokio::test]
async fn data_with_no_number_is_refused_every_time_until_it_is_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("data");
    run_once(dir.path(), 5, &storage, 4).await;
    std::fs::remove_file(storage.join("replica")).unwrap();
    std::fs::remove_file(storage.join("orvu.redb.replica")).unwrap();

    for _ in 0..2 {
        let message = boot::from_toml(&settings(dir.path(), 8, &storage)).await.err().expect("taken on").to_string();
        assert!(message.contains("adopt_data"), "{message}");
        assert!(!storage.join("replica").exists(), "a refused start wrote down its number");
    }

    // A `replica` file that names the settings' number is no leave to take
    // the data on either: only the settings can give that.
    std::fs::write(storage.join("replica"), "5\n").unwrap();
    let message = boot::from_toml(&settings(dir.path(), 5, &storage)).await.err().expect("taken on").to_string();
    assert!(message.contains("adopt_data"), "{message}");
    std::fs::remove_file(storage.join("replica")).unwrap();

    // Taken on, on purpose, under the number it was written as.
    let path = settings(dir.path(), 5, &storage);
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("adopt_data = true\n");
    std::fs::write(&path, text).unwrap();
    let (client, replica, _, _claim) = boot::from_toml(&path).await.expect("starts");
    assert_eq!(replica, ReplicaId(5));
    assert_eq!(client.counter("visits").await.unwrap().value().await.unwrap(), 4);
    client.shutdown().await;
    assert_eq!(std::fs::read_to_string(storage.join("replica")).unwrap(), "5\n");
}

// Data that notes its number, in a folder with no `replica` file, is refused
// under another number. Once the settings are put right it starts: the
// refused start did not write its wrong number into the folder.
#[tokio::test]
async fn a_wrong_number_refused_by_the_data_leaves_nothing_behind() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("data");
    run_once(dir.path(), 7, &storage, 1).await;
    std::fs::remove_file(storage.join("replica")).unwrap();

    let message = boot::from_toml(&settings(dir.path(), 8, &storage)).await.err().expect("taken on").to_string();
    assert!(message.contains("machine 7") && message.contains("replica = 8"), "{message}");
    assert!(!storage.join("replica").exists(), "a refused start wrote down its number");

    let (client, replica, _, _claim) = boot::from_toml(&settings(dir.path(), 7, &storage)).await.expect("starts");
    assert_eq!(replica, ReplicaId(7));
    client.shutdown().await;
    assert_eq!(std::fs::read_to_string(storage.join("replica")).unwrap(), "7\n");
}

// When the folder's `replica` file and the data's own note name different
// machines, the message says which file is wrong and what to do with it.
#[tokio::test]
async fn the_folder_and_the_data_disagreeing_names_the_file_to_fix() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("data");
    run_once(dir.path(), 7, &storage, 1).await;
    std::fs::write(storage.join("replica"), "8\n").unwrap();

    for named in [7, 8] {
        let message =
            boot::from_toml(&settings(dir.path(), named, &storage)).await.err().expect("started").to_string();
        let folder_file = storage.join("replica").display().to_string();
        let note = storage.join("orvu.redb.replica").display().to_string();
        assert!(message.contains(&folder_file) && message.contains(&note), "{message}");
        assert!(message.contains("delete") && message.contains("machine 7"), "{message}");
    }
}

// A local sidecar's data folder can be taken over by settings with the same
// number. The data already notes that number beside it, as a local sidecar
// writes it, so it needs nothing more. Settings with another number are
// refused.
#[tokio::test]
async fn a_local_data_folder_can_be_run_from_settings_with_its_own_number() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("data");
    std::fs::create_dir(&storage).unwrap();
    std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o700)).unwrap();
    let engine = orvud::local::start(&storage).await.unwrap();
    let number = engine.replica.0;
    engine.client.counter("visits").await.unwrap().increment(4).await.unwrap();
    engine.client.shutdown().await;
    drop(engine);

    let other = if number == 1 { 2 } else { number - 1 };
    let message = boot::from_toml(&settings(dir.path(), other, &storage)).await.err().expect("refused").to_string();
    assert!(message.contains(&format!("machine {number}")), "{message}");

    let (client, replica, _, _claim) =
        boot::from_toml(&settings(dir.path(), number, &storage)).await.expect("starts under its own number");
    assert_eq!(replica.0, number);
    assert_eq!(client.counter("visits").await.unwrap().value().await.unwrap(), 4);
    client.shutdown().await;
}

// The real program, started twice from one settings file with two sockets.
#[test]
fn the_program_takes_the_storage_folder_for_itself() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let path = settings(dir.path(), 7, &dir.path().join("data"));
    let start = |socket: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
            .arg("--config")
            .arg(&path)
            .arg("--socket")
            .arg(dir.path().join(socket))
            .args(["--exit-when-idle", "2"])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut first = start("one.sock");
    let mut said = String::new();
    std::io::BufReader::new(first.stderr.as_mut().unwrap()).read_line(&mut said).unwrap();
    assert!(said.contains("serving"), "{said}");

    let second = start("two.sock").wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&second.stderr);
    let refused = !second.status.success() && said.contains("already using");
    if !refused {
        // Not left running after the test.
        let _ = first.kill();
    }
    assert!(first.wait().unwrap().success() && refused, "a second sidecar was not refused: {said}");
}

// `local` is the namespace of a sidecar with no setup, and a program that
// finds it takes the sidecar for one that keeps its data on this machine
// only. Settings that ask for it are refused, before anything is started.
#[tokio::test]
async fn settings_may_not_use_the_local_namespace() {
    let dir = tempfile::tempdir().unwrap();
    write_certs(dir.path(), 1);
    let path = dir.path().join("orvu.toml");
    std::fs::write(&path, config(1, dir.path()).replace("namespace = \"n\"", "namespace = \"local\"")).unwrap();
    let message = boot::from_toml(&path).await.err().expect("started").to_string();
    assert!(message.contains("\"local\"") && message.contains("namespace"), "{message}");
    let message = boot::read(&path).expect_err("read").to_string();
    assert!(message.contains("\"local\""), "{message}");
}
