//! Helpers for the tests of the other modules.
//!
//! A test that starts a machine has to say which port it listens on, and the
//! port has to be free. Asking the system for a free port and giving it back
//! does not do: the system may hand the same port to the next program that
//! asks, and every link a machine dials asks for one.
//!
//! So a port is taken from where this system never hands any out, and it is
//! held for as long as the program that picked it runs, by a lock on one byte
//! of a shared file: the byte as far into the file as the port's number. The
//! system lets go of a program's locks when it ends, however it ends, so a
//! port is never left held by a program that is gone. Test programs that run
//! side by side, in Rust or in Python, lock the same file and are never given
//! the same port. A program keeps that one file open, however many ports it
//! picks.
//!
//! The file is `orvu-test-ports.lock` in the folder TMPDIR names, or in
//! /tmp. Python has other names for the temporary folder; the Python tests
//! use only this one.
//! `python/tests/ports.py` is the same thing in Python.

use std::collections::BTreeSet;
use std::fs::File;
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The ports to pick from when this system is not known to hand them out.
pub const USUAL: Range<u16> = 20_000..30_000;

/// The file whose bytes are locked to hold a port. Not to be opened by
/// anything else in a program that picks ports: closing any file open on it
/// lets go of every lock the program has there.
fn held() -> PathBuf {
    let folder = std::env::var_os("TMPDIR").filter(|named| !named.is_empty()).unwrap_or("/tmp".into());
    PathBuf::from(folder).join("orvu-test-ports.lock")
}

/// The lowest and highest port the system hands out by itself. Linux says.
/// Elsewhere the usual range is taken.
pub fn handed_out() -> (u16, u16) {
    let said = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap_or_default();
    let ends: Vec<u16> = said.split_whitespace().filter_map(|n| n.parse().ok()).collect();
    match ends[..] {
        [low, high] => (low, high),
        _ => (49152, 65535),
    }
}

/// The ports to pick from on a system that hands out `system`, which is its
/// lowest and highest. `None` if it leaves too few to be worth it. The end of
/// the range is one past the last port, so it is a `u32`.
pub fn window(system: (u16, u16)) -> Option<Range<u32>> {
    let (low, high) = (u32::from(system.0), u32::from(system.1));
    let (below, above) = (1024..low.max(1024), (high + 1)..65_536);
    let usual = u32::from(USUAL.start)..u32::from(USUAL.end);
    for gap in [&below, &above] {
        if gap.start <= usual.start && usual.end <= gap.end {
            return Some(usual);
        }
    }
    let gap = if below.len() >= above.len() { below } else { above };
    if gap.len() < 1000 {
        return None;
    }
    Some(gap.end.saturating_sub(10_000).max(gap.start)..gap.end)
}

/// What this program holds.
struct Held {
    /// `held()`, open, once it is first needed. It is never closed: closing
    /// any file open on it would let go of every lock this program has there.
    file: Option<File>,
    /// Where `file` was found. It stays where it is if the temporary folder
    /// the program names changes; every other program looks there still.
    path: Option<PathBuf>,
    /// The ports this program holds. The system does not stop two threads of
    /// one program locking the same byte, so they take turns, and look here
    /// first.
    mine: BTreeSet<u16>,
}

static HELD: Mutex<Held> = Mutex::new(Held { file: None, path: None, mine: BTreeSet::new() });

/// What this program holds, for one thread at a time. A test that failed
/// while it had its turn left nothing half done that matters, so the others
/// go on.
fn state() -> MutexGuard<'static, Held> {
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

static MAKINGS: AtomicU64 = AtomicU64::new(0);

/// The file at `path`, open. Anyone may use it, whoever made it.
fn open_held(path: &std::path::Path) -> std::io::Result<File> {
    // Each try fails only if another program put the file in place, or took
    // it away, at the same moment. A hundred in a row means something else is
    // wrong.
    for _ in 0..100 {
        // Opened as it is, if it is there: in a shared temporary folder,
        // Linux may refuse to open someone else's file in a way that would
        // make it if it were not.
        match std::fs::OpenOptions::new().read(true).write(true).open(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            opened => return opened,
        }
        // Made under another name and put in place only once anyone may use
        // it, so that a program stopped partway leaves nothing in the way.
        // The name is one no other program makes, even one given the same
        // process number later.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        let turn = MAKINGS.fetch_add(1, Ordering::Relaxed);
        let making = path.with_file_name(format!("orvu-test-ports.lock.{}.{now}.{turn}", std::process::id()));
        let made = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o666).open(&making);
        let placed = made.and_then(|made| {
            made.set_permissions(std::fs::Permissions::from_mode(0o666))?;
            std::fs::hard_link(&making, path)?;
            Ok(made)
        });
        let _ = std::fs::remove_file(&making);
        match placed {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            placed => return placed,
        }
    }
    Err(std::io::Error::other(format!("{} could be neither opened nor made", path.display())))
}

impl Held {
    /// `held()`, open. If the file was taken away or put back since it was
    /// opened, the one there now is opened, and this program's ports are
    /// locked in it again; one another program took in between cannot be
    /// got back.
    fn file(&mut self) -> std::io::Result<&File> {
        let path = self.path.get_or_insert_with(held).clone();
        let same = |open: &File| match (open.metadata(), std::fs::metadata(&path)) {
            (Ok(open), Ok(there)) => open.dev() == there.dev() && open.ino() == there.ino(),
            _ => false,
        };
        if !self.file.as_ref().is_some_and(same) {
            let file = open_held(&path)?;
            for port in &self.mine {
                let _ = lock(&file, *port, libc::F_WRLCK as _);
            }
            self.file = Some(file);
        }
        Ok(self.file.as_ref().expect("opened above"))
    }
}

/// Locks, or with `F_UNLCK` unlocks, the byte for `port`. False if another
/// program has it locked.
fn lock(file: &File, port: u16, kind: libc::c_int) -> std::io::Result<bool> {
    // SAFETY: an all-zero `flock` is a valid value of a plain C struct; the
    // fields that matter are set below.
    let mut byte: libc::flock = unsafe { std::mem::zeroed() };
    byte.l_type = kind as _;
    byte.l_whence = libc::SEEK_SET as _;
    byte.l_start = libc::off_t::from(port);
    byte.l_len = 1;
    // SAFETY: the file is open for as long as the call, and `byte` is a valid
    // `flock` that the call only reads.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &byte) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EAGAIN | libc::EACCES) => Ok(false),
        _ => Err(e),
    }
}

/// Gives a port back before the program ends. One that cannot be given back
/// stays held, until the program ends.
pub fn release(port: u16) {
    let mut state = state();
    let given_back = match (&state.file, state.mine.contains(&port)) {
        (Some(file), true) => lock(file, port, libc::F_UNLCK as _).is_ok(),
        _ => false,
    };
    if given_back {
        state.mine.remove(&port);
    }
}

/// True if something listens on `port`. Any other trouble with looking is an
/// error: going on to the next port would not help.
fn in_use(port: u16) -> std::io::Result<bool> {
    use std::io::ErrorKind::{AddrInUse, AddrNotAvailable, Unsupported};
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, UdpSocket};
    for everywhere in [SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)), SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))] {
        let looked = [TcpListener::bind(everywhere).map(drop), UdpSocket::bind(everywhere).map(drop)];
        for looked in looked {
            match looked {
                Ok(()) => {}
                Err(e) if e.kind() == AddrInUse => return Ok(true),
                // A machine with no IPv6 has nothing to look at there.
                Err(e) if everywhere.is_ipv6() && (matches!(e.kind(), AddrNotAvailable | Unsupported) || e.raw_os_error() == Some(libc::EAFNOSUPPORT)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(false)
}

static TURN: AtomicU64 = AtomicU64::new(0);

/// A free port among `among`, or why there is none.
pub fn free_port_among(among: Range<u32>) -> Result<u16, String> {
    if among.is_empty() {
        return Err("no free port: there are none to choose from".to_string());
    }
    if among.start < 1 || among.end > 65_536 {
        return Err(format!("{among:?} are not ports, which are 1 to 65535"));
    }
    let count = u64::from(among.end - among.start);
    let start = u64::from(std::process::id()) * 7919 % count;
    // Steps of 13 spread the picks out. Every port is still reached, unless
    // the number of ports is itself a multiple of 13.
    let step = if count % 13 == 0 { 1 } else { 13 };
    let mut state = state();
    state.file().map_err(|e| format!("cannot open {}: {e}", held().display()))?;
    let Held { file: Some(file), mine, .. } = &mut *state else { unreachable!("opened above") };
    for _ in 0..count {
        let turn = TURN.fetch_add(1, Ordering::Relaxed);
        let port = (u64::from(among.start) + (start + turn % count * step) % count) as u16;
        let failed = |e: std::io::Error| format!("cannot look at port {port}: {e}");
        if mine.contains(&port) || !lock(file, port, libc::F_WRLCK as _).map_err(failed)? {
            continue;
        }
        let busy = in_use(port);
        if busy.as_ref().is_ok_and(|busy| !busy) {
            mine.insert(port);
            return Ok(port);
        }
        let _ = lock(file, port, libc::F_UNLCK as _);
        busy.map_err(failed)?;
    }
    Err(format!(
        "no free port from {} to {}: all are in use or held in {}",
        among.start,
        among.end - 1,
        held().display()
    ))
}

/// A port nothing is using, for a machine to listen on. It is this program's
/// until the program ends. Panics, saying why, if there is none.
pub fn free_port() -> u16 {
    let among = window(handed_out()).unwrap_or(u32::from(USUAL.start)..u32::from(USUAL.end));
    free_port_among(among).unwrap_or_else(|why| panic!("{why}"))
}

/// Waits for `step`, or panics naming `what` once `limit` has passed.
///
/// A test that waits on a machine or the network should wait through this,
/// so that a step that never finishes fails the test and says which step it
/// was, rather than leaving the run stuck with nothing to show. Needs a Tokio
/// runtime with its timer, which `#[tokio::test]` gives.
pub async fn within<T>(what: &str, limit: std::time::Duration, step: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(limit, step).await {
        Ok(done) => done,
        Err(_) => panic!("{what} did not finish within {limit:?}"),
    }
}

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Where the ports are taken from follows what this system hands out. A
    // place that is right on one machine is wrong on one set up another way.
    #[test]
    fn where_ports_are_taken_from_follows_what_the_system_hands_out() {
        assert_eq!(window((32768, 60999)), Some(20_000..30_000)); // Linux as it comes
        assert_eq!(window((49152, 65535)), Some(20_000..30_000)); // a Mac
        assert_eq!(window((10000, 65535)), Some(1024..10_000)); // only the bottom is left
        assert_eq!(window((1024, 40000)), Some(55_536..65_536)); // only the top is left
        assert_eq!(window((1024, 65535)), None); // nothing is left
        assert_eq!(window((1024, 65000)), None); // too little is left to be worth it
    }

    #[test]
    fn ports_are_not_ones_the_system_hands_out_and_no_two_are_the_same() {
        let (low, high) = handed_out();
        let Some(among) = window((low, high)) else { return };
        let picked: Vec<u16> = (0..200).map(|_| free_port()).collect();
        for port in &picked {
            assert!(among.contains(&u32::from(*port)) && !(low..=high).contains(port), "{port}");
        }
        let different: std::collections::BTreeSet<u16> = picked.iter().copied().collect();
        assert_eq!(different.len(), picked.len(), "the same port was picked twice");
    }

    // A program that picks ports when it is told to: this test program, run
    // again with only the test below, which does nothing unless it is asked.
    // Its temporary folder is one of its own, so it and the tests' own
    // picking never meet. Answers follow "answer: ", to be told apart from
    // what the test runner prints, which can come first on the same line.
    #[test]
    #[ignore]
    fn picker() {
        if std::env::var_os("ORVU_TEST_PICKER").is_none() {
            return;
        }
        for line in std::io::stdin().lines() {
            let line = line.unwrap();
            let words: Vec<u32> = line.split_whitespace().skip(1).filter_map(|n| n.parse().ok()).collect();
            let answer = if line.starts_with("pick") {
                free_port_among(words[0]..words[1]).map_or("none".to_string(), |port| port.to_string())
            } else if line.starts_with("where") {
                held().display().to_string()
            } else if line.starts_with("look") {
                look(&where_anyone_looks(), words[0] as u16).to_string()
            } else if line.starts_with("holder") {
                holder(&where_anyone_looks(), words[0] as u16).map_or("nobody".to_string(), |pid| pid.to_string())
            } else if let Some(folder) = line.strip_prefix("move ") {
                // As a test that gives itself a temporary folder of its own
                // might. Nothing else runs in this program at the time.
                std::env::set_var("TMPDIR", folder);
                "moved".to_string()
            } else if line.starts_with("block") {
                // What a program killed while making the file would leave, had
                // it been given this program's number.
                let making = held().with_file_name(format!("orvu-test-ports.lock.{}", std::process::id()));
                std::fs::create_dir(making).unwrap();
                "blocked".to_string()
            } else {
                release(words[0] as u16);
                "given back".to_string()
            };
            println!("answer: {answer}");
        }
    }

    struct Picker(std::process::Child, std::io::Lines<std::io::BufReader<std::process::ChildStdout>>);

    impl Picker {
        // A picker with its temporary folder in `folder`.
        fn new(folder: &std::path::Path) -> Picker {
            Picker::start(|program| program.env("TMPDIR", folder), None)
        }

        // A picker started as `set_up` says, allowed `open_files` open files
        // if that is fewer than it would be.
        fn start(set_up: impl FnOnce(&mut std::process::Command) -> &mut std::process::Command, open_files: Option<u32>) -> Picker {
            use std::process::Stdio;
            let me = std::env::current_exe().unwrap();
            let lower = open_files.map_or(String::new(), |most| format!("ulimit -S -n {most} && "));
            let mut program = std::process::Command::new("sh");
            program
                .args(["-c", &format!("{lower}umask 077 && exec \"$0\" \"$@\"")])
                .arg(me)
                .args(["--exact", "tests::picker", "--ignored", "--nocapture", "--test-threads=1"])
                .env("ORVU_TEST_PICKER", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let mut program = set_up(&mut program).spawn().unwrap();
            let lines = std::io::BufRead::lines(std::io::BufReader::new(program.stdout.take().unwrap()));
            Picker(program, lines)
        }

        fn ask(&mut self, line: &str) -> String {
            writeln!(self.0.stdin.as_mut().unwrap(), "{line}").unwrap();
            for said in self.1.by_ref() {
                if let Some((_, answer)) = said.unwrap().split_once("answer: ") {
                    return answer.to_string();
                }
            }
            panic!("the picker ended without answering {line}");
        }
    }

    impl Drop for Picker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // A folder of its own for a test, taken away when the test ends, also
    // when it fails.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let folder = std::env::temp_dir().join(format!("orvu-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&folder);
            std::fs::create_dir_all(&folder).unwrap();
            Scratch(folder)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // `count` ports next to those `free_port` gives, on the side where the
    // system hands none out either, so no program is given any of them.
    fn given_to_nobody(count: u32) -> Range<u32> {
        let given = window(handed_out()).unwrap_or(u32::from(USUAL.start)..u32::from(USUAL.end));
        let (low, _) = handed_out();
        if given.end + count <= u32::from(low) {
            given.end..given.end + count
        } else {
            assert!(given.start >= 1024 + count, "no room beside {given:?}");
            given.start - count..given.start
        }
    }

    // A port for the programs of one test to pick, and what to ask them.
    // This test program holds it, where every other program looks, so that no
    // other test's programs are given it and look at it at the same time.
    fn own_port() -> (u16, String) {
        let port = free_port();
        (port, format!("pick {port} {}", u32::from(port) + 1))
    }

    // Where a person or another program finds the locks, worked out here
    // apart from the code under test: `orvu-test-ports.lock` in the folder
    // TMPDIR names, or /tmp. The Python tests look there too.
    fn where_anyone_looks() -> PathBuf {
        let folder = std::env::var_os("TMPDIR").filter(|named| !named.is_empty()).unwrap_or("/tmp".into());
        PathBuf::from(folder).join("orvu-test-ports.lock")
    }

    // Whether the byte for `port` in `held` is locked by some program, found
    // out by trying to lock it, as a program that knows nothing of how ports
    // are picked would.
    fn look(held: &std::path::Path, port: u16) -> &'static str {
        let looked = std::fs::OpenOptions::new().read(true).write(true).open(held).unwrap();
        // SAFETY: an all-zero `flock` is a valid value of a plain C struct.
        let mut byte: libc::flock = unsafe { std::mem::zeroed() };
        byte.l_type = libc::F_WRLCK as _;
        byte.l_whence = libc::SEEK_SET as _;
        byte.l_start = libc::off_t::from(port);
        byte.l_len = 1;
        // SAFETY: the file is open for the call, which only reads `byte`.
        let free = unsafe { libc::fcntl(looked.as_raw_fd(), libc::F_SETLK, &byte) } == 0;
        if free { "free" } else { "held" }
    }

    // Which program holds `port` in the file `held`, by its process number,
    // as another program sees it.
    fn holder(held: &std::path::Path, port: u16) -> Option<i32> {
        let looked = std::fs::OpenOptions::new().read(true).write(true).open(held).unwrap();
        // SAFETY: an all-zero `flock` is a valid value of a plain C struct.
        let mut byte: libc::flock = unsafe { std::mem::zeroed() };
        byte.l_type = libc::F_WRLCK as _;
        byte.l_whence = libc::SEEK_SET as _;
        byte.l_start = libc::off_t::from(port);
        byte.l_len = 1;
        // SAFETY: the file is open for the call, and `byte` is a valid
        // `flock` for it to fill in.
        assert_eq!(unsafe { libc::fcntl(looked.as_raw_fd(), libc::F_GETLK, &mut byte) }, 0);
        (byte.l_type != libc::F_UNLCK as _).then_some(byte.l_pid)
    }

    // A port held by this program can be seen to be held by it from another.
    // Once given back it is not held by this program any more, though
    // another program may take it straight away.
    #[test]
    fn a_port_held_here_is_held_where_anyone_can_see() {
        let port = free_port();
        let me = std::process::id().to_string();
        let mut other = Picker::start(|program| program, None);
        assert_eq!(other.ask(&format!("look {port}")), "held");
        assert_eq!(other.ask(&format!("holder {port}")), me);
        release(port);
        assert_ne!(other.ask(&format!("holder {port}")), me);
    }

    #[test]
    fn a_port_is_held_by_one_byte_as_far_in_as_its_number() {
        let (port, pick) = own_port();
        let scratch = Scratch::new("byte");
        let mut picker = Picker::new(&scratch.0);
        assert_eq!(picker.ask(&pick), port.to_string());
        let held = scratch.0.join("orvu-test-ports.lock");
        assert_eq!([look(&held, port - 1), look(&held, port), look(&held, port + 1)], ["free", "held", "free"]);
    }

    // Python has other names for the temporary folder; the Python tests use
    // only this one, as here.
    #[test]
    fn where_ports_are_held_is_found_as_python_finds_it() {
        let scratch = Scratch::new("where");
        let mut picker = Picker::start(|program| program.env_remove("TMPDIR").env("TMP", &scratch.0).env("TEMP", &scratch.0), None);
        assert_eq!(picker.ask("where"), "/tmp/orvu-test-ports.lock");
    }

    // The file can be taken away while programs hold ports in it, by a person
    // or by whatever clears out old files. A program that held a port there
    // takes it again in the file that is there now, the next time it picks.
    #[test]
    fn a_port_is_still_held_after_the_file_was_taken_away() {
        let ((port, pick), (other, pick_other)) = (own_port(), own_port());
        let scratch = Scratch::new("taken-away");
        let (mut first, mut second) = (Picker::new(&scratch.0), Picker::new(&scratch.0));
        assert_eq!(first.ask(&pick), port.to_string());
        std::fs::remove_file(scratch.0.join("orvu-test-ports.lock")).unwrap();
        assert_eq!(first.ask(&pick_other), other.to_string());
        assert_eq!(second.ask(&pick), "none");
    }

    // A test may give itself a temporary folder of its own. The ports the
    // program holds stay held where every other program looks.
    #[test]
    fn a_port_is_still_held_after_the_temporary_folder_changes() {
        let ((port, pick), (other, pick_other)) = (own_port(), own_port());
        let (scratch, elsewhere) = (Scratch::new("stays"), Scratch::new("elsewhere"));
        let mut picker = Picker::new(&scratch.0);
        assert_eq!(picker.ask(&pick), port.to_string());
        assert_eq!(picker.ask(&format!("move {}", elsewhere.0.display())), "moved");
        assert_eq!(picker.ask(&pick_other), other.to_string());
        assert_eq!(look(&scratch.0.join("orvu-test-ports.lock"), port), "held");
    }

    // A program killed while making the file leaves what it was making
    // behind. A program given the same number later must not be stopped by
    // it.
    #[test]
    fn what_a_program_was_making_when_it_ended_does_not_stop_another() {
        let (port, pick) = own_port();
        let scratch = Scratch::new("making");
        let mut picker = Picker::new(&scratch.0);
        assert_eq!(picker.ask("block"), "blocked");
        assert_eq!(picker.ask(&pick), port.to_string());
    }

    // Ports that are none, or not ports at all, are said to be so.
    #[test]
    fn a_choice_of_no_ports_or_not_ports_is_refused() {
        for among in [25_000..25_000, 65_530..65_540, 0..5] {
            let why = free_port_among(among.clone()).unwrap_err();
            assert!(why.contains("no free port") || why.contains("not ports"), "{among:?}: {why}");
        }
    }

    // A port something listens on over TCP is in use, though nothing listens
    // on it over UDP.
    #[test]
    fn a_port_listened_on_over_tcp_is_not_given() {
        // A port given here was found free a moment ago, but a program that
        // does not pick ports this way can take it in between. That is seldom,
        // and then another port is listened on.
        let mut refused = Vec::new();
        let (port, _listening) = (0..10)
            .find_map(|_| {
                let port = free_port();
                match std::net::TcpListener::bind(("127.0.0.1", port)) {
                    Ok(listening) => Some((port, listening)),
                    Err(e) => {
                        refused.push(format!("{port}: {e}"));
                        None
                    }
                }
            })
            .unwrap_or_else(|| panic!("no port given could be listened on: {refused:?}"));
        if !refused.is_empty() {
            println!("ports given that could not be listened on: {refused:?}");
        }
        release(port);
        let why = free_port_among(u32::from(port)..u32::from(port) + 1).unwrap_err();
        assert!(why.contains("no free port"), "{why}");
    }

    // A test that fails while ports are being picked does not stop the
    // other tests of the same program picking.
    #[test]
    fn a_failure_while_picking_does_not_stop_later_picks() {
        let failed = std::thread::spawn(|| {
            let _picking = HELD.lock();
            panic!("failed while picking");
        });
        assert!(failed.join().is_err());
        let port = free_port();
        release(port);
    }

    // A port is held for as long as the program that picked it runs, however
    // it ends: even one killed outright gives its ports back.
    #[test]
    fn a_port_is_held_until_its_program_ends_however_it_ends() {
        let (port, pick) = own_port();
        let scratch = Scratch::new("ended");
        let (mut first, mut second) = (Picker::new(&scratch.0), Picker::new(&scratch.0));
        assert_eq!(first.ask(&pick), port.to_string());
        assert_eq!(second.ask(&pick), "none");
        drop(first);
        assert_eq!(second.ask(&pick), port.to_string());
    }

    // A port given back can be picked by another program while the first runs.
    #[test]
    fn a_port_given_back_can_be_picked_by_another_program() {
        let (port, pick) = own_port();
        let scratch = Scratch::new("given-back");
        let (mut first, mut second) = (Picker::new(&scratch.0), Picker::new(&scratch.0));
        assert_eq!(first.ask(&pick), port.to_string());
        assert_eq!(first.ask(&format!("release {port}")), "given back");
        assert_eq!(second.ask(&pick), port.to_string());
        assert_eq!(first.ask(&pick), "none");
    }

    // A program may be allowed only a few open files. Picking many ports must
    // not use one up for each.
    //
    // The picker holds its ports in a file of its own, where no other program
    // looks, and it looks at each port it picks by listening on it for a
    // moment. So it picks from ports no other program is given: one given to
    // a test elsewhere could be found in use, at that moment, by that test.
    #[test]
    fn picking_many_ports_does_not_use_up_open_files() {
        let scratch = Scratch::new("open-files");
        let mut picker = Picker::start(|program| program.env("TMPDIR", &scratch.0), Some(64));
        let among = given_to_nobody(500);
        let ask = format!("pick {} {}", among.start, among.end);
        let picked: std::collections::BTreeSet<String> = (0..300).map(|_| picker.ask(&ask)).collect();
        assert!(!picked.contains("none") && picked.len() == 300, "{} picked", picked.len());
    }

    // Programs of different people on one machine share where ports are held.
    // What one of them makes there, another can use, whatever its own
    // settings.
    #[test]
    fn what_holds_the_ports_can_be_used_by_everyone() {
        use std::os::unix::fs::PermissionsExt;
        let (port, pick) = own_port();
        let scratch = Scratch::new("everyone");
        assert_eq!(Picker::new(&scratch.0).ask(&pick), port.to_string());
        let made: Vec<(PathBuf, u32)> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .map(|path| {
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
                (path, mode)
            })
            .collect();
        assert!(!made.is_empty() && made.iter().all(|(_, mode)| [0o666, 0o1777, 0o777].contains(mode)), "{made:?}");
    }

    #[test]
    fn when_no_port_is_left_it_says_so_and_does_not_go_on_for_ever() {
        let port = free_port(); // held by this program
        let why = free_port_among(u32::from(port)..u32::from(port) + 1).unwrap_err();
        assert!(why.contains("no free port"), "{why}");
    }

    // The arithmetic that walks the ports must not repeat one early however
    // large the process number is.
    #[test]
    fn a_whole_round_gives_every_port_once() {
        let among = 61_000u32..61_050;
        // Some may be in use on this machine. None is given twice, and the
        // round ends by saying that no port is left, before a 51st pick.
        // Another run of this test, from another copy of the code, can hold
        // every one of them for a moment; then this one waits for it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let (mut picked, mut ended) = (Vec::new(), false);
        while picked.is_empty() && std::time::Instant::now() < deadline {
            if ended {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ended = false;
            for _ in 0..=50 {
                match free_port_among(among.clone()) {
                    Ok(port) => picked.push(port),
                    Err(_) => {
                        ended = true;
                        break;
                    }
                }
            }
        }
        for port in &picked {
            release(*port);
        }
        let different: BTreeSet<u16> = picked.iter().copied().collect();
        assert!(ended && !picked.is_empty() && different.len() == picked.len(), "{picked:?}");
    }
}
