//! The note of where a local sidecar's socket is, which it keeps in its data
//! folder while it runs. These start the real program, so they are kept in a
//! program of their own: a test that starts a program while another test in
//! the same program holds a data folder's lock could keep that lock for a
//! moment in the started program, and the other test would find its folder
//! in use.

/// A temporary folder that only its owner can get into, as the sidecar
/// wants for its data.
fn private_folder() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let folder = tempfile::tempdir().unwrap();
    std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    folder
}

/// The real program, started by hand on `folder` with its socket at `socket`,
/// once it says it is serving, or what it said if it stopped instead.
fn started_by_hand(folder: &std::path::Path, socket: &std::path::Path) -> Result<Running, String> {
    started_in(&std::env::temp_dir(), folder, socket)
}

/// The same, started in the folder `cwd`.
fn started_in(
    cwd: &std::path::Path,
    folder: &std::path::Path,
    socket: &std::path::Path,
) -> Result<Running, String> {
    use std::io::BufRead;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
        .current_dir(cwd)
        .args(["--local", "--data-dir"])
        .arg(folder)
        .arg("--socket")
        .arg(socket)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    let mut lines = std::io::BufReader::new(child.stderr.take().unwrap()).lines();
    for line in lines.by_ref() {
        let line = line.unwrap();
        said.push_str(&line);
        said.push('\n');
        if line.contains("serving on") {
            // What it says later is not read, so it is let go of in a thread.
            std::thread::spawn(move || lines.for_each(drop));
            return Ok(Running(Some(child)));
        }
    }
    child.wait().unwrap();
    Err(said)
}

/// A sidecar a test started. It is killed if the test fails before it is
/// asked to stop.
#[derive(Debug)]
struct Running(Option<std::process::Child>);

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Asks the program to stop, as Ctrl-C or a service manager would, and waits.
fn ask_to_stop(mut running: Running) {
    let mut child = running.0.take().unwrap();
    let asked = std::process::Command::new("kill").arg("-TERM").arg(child.id().to_string()).status().unwrap();
    assert!(asked.success());
    assert!(child.wait().unwrap().success());
}

fn note_in(folder: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(folder.join("orvud.where")).ok()
}

// A sidecar started by hand in a terminal notes where its socket is in the
// data folder, as one a program starts does. A program started with fewer
// settings than the terminal has, such as an agent's server, finds the data
// folder the same way and so finds the sidecar.
#[test]
fn a_sidecar_started_by_hand_notes_its_socket_in_the_data_folder() {
    let folder = private_folder();
    let socket = folder.path().join("by-hand.sock");
    let first = started_by_hand(folder.path(), &socket).unwrap();
    assert_eq!(note_in(folder.path()).as_deref(), Some(socket.to_str().unwrap()));

    // A second one on the same folder is refused before it writes anything,
    // so the note still names the first.
    let refused = started_by_hand(folder.path(), &folder.path().join("second.sock")).expect_err("two on one folder");
    assert!(refused.contains("already using"), "{refused}");
    assert_eq!(note_in(folder.path()).as_deref(), Some(socket.to_str().unwrap()));

    // The note goes with the sidecar.
    ask_to_stop(first);
    assert_eq!(note_in(folder.path()), None);
}

// A socket given as a path from the folder the sidecar was started in is
// noted as the whole path, which a program started elsewhere can use.
#[test]
fn the_note_names_the_whole_path_of_the_socket() {
    let folder = private_folder();
    let first = started_in(folder.path(), folder.path(), std::path::Path::new("here.sock")).unwrap();
    assert_eq!(note_in(folder.path()).map(std::path::PathBuf::from), Some(folder.path().join("here.sock")));
    ask_to_stop(first);
}

// A note that names some other socket by the time the sidecar stops is not
// its own any more, and is left where it is.
#[test]
fn a_note_that_is_no_longer_its_own_is_left_alone() {
    let folder = private_folder();
    let first = started_by_hand(folder.path(), &folder.path().join("mine.sock")).unwrap();
    std::fs::write(folder.path().join("orvud.where"), "/somewhere/else.sock").unwrap();
    ask_to_stop(first);
    assert_eq!(note_in(folder.path()).as_deref(), Some("/somewhere/else.sock"));
}
