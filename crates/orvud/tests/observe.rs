//! The sidecar's log and its page of numbers. Both are off unless the
//! settings turn them on.

use orvud::observe::{Observe, LOCAL_SETTINGS};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};

fn private_folder() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[test]
fn nothing_is_shown_unless_asked() {
    let dir = private_folder();
    let observe = Observe::local(dir.path()).unwrap();
    assert_eq!(observe.level(), None);
    assert_eq!(observe.log_file(), None);
    assert_eq!(observe.metrics_addr(), None);
}

#[test]
fn a_settings_file_in_the_data_folder_turns_them_on() {
    let dir = private_folder();
    std::fs::write(
        dir.path().join(LOCAL_SETTINGS),
        "[logs]\nlevel = \"debug\"\nfile = \"orvud-log.txt\"\n[metrics]\naddr = \"127.0.0.1:9464\"\n",
    )
    .unwrap();
    let observe = Observe::local(dir.path()).unwrap();
    assert_eq!(observe.level(), Some(tracing::Level::DEBUG));
    // A file that is not given with its full path goes in the data folder.
    assert_eq!(observe.log_file(), Some(dir.path().join("orvud-log.txt")));
    assert_eq!(observe.metrics_addr(), Some("127.0.0.1:9464"));
}

#[test]
fn a_mistake_in_the_settings_is_reported_before_anything_starts() {
    let dir = private_folder();
    for (text, says) in [
        ("[logs]\nlevel = \"loud\"\n", "loud"),
        ("[logs]\nlevl = \"info\"\n", "levl"),
        // A file with no level would be a log that is never written.
        ("[logs]\nfile = \"orvud.log\"\n", "level"),
        // Only the log, the page and how often to save are set here. The
        // rest of the sidecar in this mode needs no settings.
        ("[storage]\npath = \"x\"\n", "path"),
        ("[sync]\npeers = []\n", "sync"),
        ("[storage]\nfsync = \"sometimes\"\n", "sometimes"),
        ("[storage]\nfsync = \"interval\"\ninterval_ms = 0\n", "interval_ms"),
    ] {
        std::fs::write(dir.path().join(LOCAL_SETTINGS), text).unwrap();
        let err = Observe::local(dir.path()).unwrap_err();
        assert!(err.contains(says) && err.contains(LOCAL_SETTINGS), "{text:?}: {err}");
    }
}

// At the most detailed levels, the libraries underneath say a great deal
// about every message. Only their warnings and errors are written.
#[test]
fn only_the_sidecar_and_the_engine_write_their_details() {
    let dir = private_folder();
    std::fs::write(dir.path().join(LOCAL_SETTINGS), "[logs]\nlevel = \"trace\"\n").unwrap();
    let filter = Observe::local(dir.path()).unwrap().log_filter().unwrap();
    use tracing::Level;
    assert!(filter.would_enable("orvud::server", &Level::TRACE));
    assert!(filter.would_enable("orvu_runtime::node", &Level::DEBUG));
    assert!(!filter.would_enable("h2::proto::connection", &Level::DEBUG));
    assert!(!filter.would_enable("quinn_proto", &Level::INFO));
    assert!(filter.would_enable("quinn_proto", &Level::WARN));

    // At a level that says less, the libraries say as little.
    std::fs::write(dir.path().join(LOCAL_SETTINGS), "[logs]\nlevel = \"error\"\n").unwrap();
    let filter = Observe::local(dir.path()).unwrap().log_filter().unwrap();
    assert!(!filter.would_enable("quinn_proto", &Level::WARN));
    assert!(!filter.would_enable("orvud::server", &Level::WARN));
}

#[test]
fn from_a_settings_file_the_sidecar_takes_the_page_over_from_the_engine() {
    let text = "replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n\
                [logs]\nlevel = \"warn\"\n[metrics]\naddr = \"127.0.0.1:9464\"\n";
    let mut settings = orvu_sdk::SdkConfig::parse_toml(text).unwrap();
    let observe = Observe::take(&mut settings, std::path::Path::new("/etc/orvu")).unwrap();
    assert_eq!(observe.level(), Some(tracing::Level::WARN));
    assert_eq!(observe.metrics_addr(), Some("127.0.0.1:9464"));
    // The sidecar serves the page with its own numbers next to the engine's,
    // so the engine must not serve one of its own on the same address.
    assert_eq!(settings.metrics.addr, None);
}

/// Starts the real program on `dir` and waits until it says it is serving.
fn start(dir: &std::path::Path) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_orvud"))
        .args(["--local", "--data-dir"])
        .arg(dir)
        .arg("--socket")
        .arg(dir.join("s"))
        .args(["--exit-when-idle", "1"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    BufReader::new(child.stderr.as_mut().unwrap()).read_line(&mut said).unwrap();
    assert!(said.contains("serving"), "{said}");
    child
}

#[test]
fn the_log_goes_where_the_settings_say() {
    let dir = private_folder();
    std::fs::write(dir.path().join(LOCAL_SETTINGS), "[logs]\nlevel = \"info\"\nfile = \"orvud-log.txt\"\n").unwrap();
    let mut child = start(dir.path());
    assert!(child.wait().unwrap().success());
    let log = std::fs::read_to_string(dir.path().join("orvud-log.txt")).unwrap();
    assert!(log.contains("INFO") && log.contains("orvud serving"), "{log}");
}

#[test]
fn without_settings_there_is_no_log_and_no_page() {
    let dir = private_folder();
    let mut child = start(dir.path());
    assert!(child.wait().unwrap().success());
    let mut rest = String::new();
    child.stderr.take().unwrap().read_to_string(&mut rest).unwrap();
    // What it always prints, and nothing from the log. The record of who
    // changed what is kept whatever the settings say.
    assert!(!rest.contains("INFO"), "{rest}");
    let made: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    let kept = ["orvu.redb", "orvu.redb.replica", "replica", "computer", "orvud.lock", "audit.log"];
    assert!(made.iter().all(|name| kept.contains(&name.to_str().unwrap())), "{made:?}");
}

/// The page at `addr`, or what went wrong.
fn page(addr: &str) -> std::io::Result<String> {
    let mut stream = std::net::TcpStream::connect(addr)?;
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    Ok(answer)
}

#[test]
fn the_page_shows_the_engines_numbers_while_the_sidecar_runs() {
    let dir = private_folder();
    let addr = format!("127.0.0.1:{}", orvu_test_support::free_port());
    std::fs::write(dir.path().join(LOCAL_SETTINGS), format!("[metrics]\naddr = \"{addr}\"\n")).unwrap();
    let mut child = start(dir.path());
    let answer = page(&addr).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.contains("orvu_objects "), "{answer}");
    // The sidecar's own numbers come after the engine's.
    assert!(answer.contains("orvu_sidecar_connections 0\n"), "{answer}");
    assert!(child.wait().unwrap().success());
    assert!(page(&addr).is_err(), "the page outlived the sidecar");
}

#[test]
fn a_page_address_that_is_taken_stops_the_start() {
    let dir = private_folder();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    std::fs::write(dir.path().join(LOCAL_SETTINGS), format!("[metrics]\naddr = \"{addr}\"\n")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_orvud"))
        .args(["--local", "--data-dir"])
        .arg(dir.path())
        .arg("--socket")
        .arg(dir.path().join("s"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("metrics") && said.contains(&addr.to_string()), "{said}");
    // It was found before anything was started: no data was written and no
    // socket is left, and the folder is free for the next start.
    assert!(!dir.path().join("orvu.redb").exists(), "the engine had started");
    assert!(!dir.path().join("s").exists(), "the socket was left behind");
    std::fs::remove_file(dir.path().join(LOCAL_SETTINGS)).unwrap();
    let mut child = start(dir.path());
    assert!(child.wait().unwrap().success());
}

// The same from a settings file: the machine does not join the others and
// leave again when the page cannot be served.
#[test]
fn a_page_address_that_is_taken_is_found_before_the_machine_joins_the_others() {
    let top = tempfile::tempdir().unwrap();
    let crew = top.path().join("crew");
    let (port, other) = (orvu_test_support::free_port(), orvu_test_support::free_port());
    let made = Command::new(env!("CARGO_BIN_EXE_orvud"))
        .arg("setup")
        .arg(&crew)
        .arg(format!("alpha=127.0.0.1:{port}"))
        .arg(format!("beta=127.0.0.1:{other}"))
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let alpha = crew.join("alpha");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut settings = std::fs::OpenOptions::new().append(true).open(alpha.join("orvu.toml")).unwrap();
    write!(settings, "\n[metrics]\naddr = \"{}\"\n", taken.local_addr().unwrap()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_orvud"))
        .arg("--config")
        .arg(alpha.join("orvu.toml"))
        .arg("--socket")
        .arg(alpha.join("s"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("metrics"));
    assert!(!alpha.join("data").exists(), "the engine had started");
    assert!(!alpha.join("s").exists(), "the socket was left behind");
}

// From a settings file, the same two sections, with the log file next to it.
#[test]
fn a_sidecar_started_from_settings_logs_and_serves_the_page_as_they_say() {
    let top = tempfile::tempdir().unwrap();
    let crew = top.path().join("crew");
    let (port, other) = (orvu_test_support::free_port(), orvu_test_support::free_port());
    // The other machine is never started. It does not need to be.
    let made = Command::new(env!("CARGO_BIN_EXE_orvud"))
        .arg("setup")
        .arg(&crew)
        .arg(format!("alpha=127.0.0.1:{port}"))
        .arg(format!("beta=127.0.0.1:{other}"))
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let alpha = crew.join("alpha");
    let addr = format!("127.0.0.1:{}", orvu_test_support::free_port());
    let mut settings = std::fs::OpenOptions::new().append(true).open(alpha.join("orvu.toml")).unwrap();
    write!(settings, "\n[logs]\nlevel = \"info\"\nfile = \"orvud-log.txt\"\n[metrics]\naddr = \"{addr}\"\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_orvud"))
        .arg("--config")
        .arg(alpha.join("orvu.toml"))
        .arg("--socket")
        .arg(alpha.join("s"))
        .args(["--exit-when-idle", "1"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    BufReader::new(child.stderr.as_mut().unwrap()).read_line(&mut said).unwrap();
    assert!(said.contains("serving"), "{said}");
    let answer = page(&addr).unwrap();
    assert!(answer.contains("orvu_objects "), "{answer}");
    assert!(child.wait().unwrap().success());
    let log = std::fs::read_to_string(alpha.join("orvud-log.txt")).unwrap();
    assert!(log.contains("orvud serving"), "{log}");
}
