//! The agents a sidecar lets in, and the keys they prove who they are with.
//!
//! The list is a small file, `agents`, next to the sidecar's data. Each line
//! is an agent's name, the fingerprint of its key, and, if it has any, its
//! rules (see `crate::rules`). The key itself is shown
//! once, when the agent is added, and is not kept: someone who reads the file
//! learns who may connect, but cannot connect as them.
//!
//! While there is no such file, the sidecar lets in every program that can
//! reach its socket, as it always has. Once there is one, a program has to
//! give one of the keys in it, and it is that agent. A file with no agents
//! left in it lets nobody in: removing the last agent must not open the
//! sidecar to everybody again. Deleting the file does that.
//!
//! The running sidecar reads the file again whenever it has changed, so an
//! agent that is added can connect at once, and one that is removed is turned
//! away at its next call. A watch it has open is ended within a second.
//!
//! The file keeps agents apart only from programs that cannot change it. A
//! program running as the sidecar's own user can add a key of its own.

use crate::rules::Rules;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The name of the file, in the data folder of a local sidecar or next to
/// the settings file of one started with `--config`.
pub const FILE: &str = "agents";

/// What every key starts with, so that a key is easy to recognise, in a
/// settings file or a log, as something to keep secret.
const KEY_START: &str = "orvu-key-";

/// Where the agents file is for a sidecar started in `mode`: in the data
/// folder of a local one, and next to the settings file of one started with
/// `--config`. `env` reads the environment, for the default data folder.
pub fn file_for(mode: &crate::cli::Mode, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    match mode {
        crate::cli::Mode::Local { data_dir: Some(folder) } => Ok(folder.join(FILE)),
        crate::cli::Mode::Local { data_dir: None } => crate::local::default_data_dir(env)
            .map(|folder| folder.join(FILE))
            .ok_or_else(|| "there is no home folder to keep data in: set $HOME, or pass --data-dir".to_string()),
        // A path that leads nowhere is a mistake, and the agents put next to
        // it would never be read by the sidecar.
        crate::cli::Mode::Config(path) if !path.is_file() => Err(format!(
            "there is no settings file at {}: give the one the sidecar is started with",
            path.display()
        )),
        crate::cli::Mode::Config(path) => Ok(path.parent().unwrap_or(Path::new("")).join(FILE)),
    }
}

/// One agent the sidecar lets in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    /// The fingerprint of its key.
    pub fingerprint: String,
}

/// One line of the agents file: an agent, and its rules. `None` is no rules,
/// which lets it do everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub fingerprint: String,
    pub rules: Option<Rules>,
}

/// Whether `name` can be an agent's name. It is the rule for every name in
/// Orvu: a letter or digit, then up to 63 letters, digits, dots, dashes and
/// underscores.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A new key: 32 random bytes, written as hex after `orvu-key-`.
fn new_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS entropy source");
    format!("{KEY_START}{}", hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The fingerprint of a key: its SHA-256, as hex. A key is 32 random bytes,
/// far too many to guess, so a plain hash is enough to keep it from being
/// worked out from the file.
fn fingerprint(key: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, key).as_ref())
}

/// Reads the list. A missing file is an empty list. Anything else that is
/// wrong with it is an error, never an empty list: a file that cannot be read
/// must not let everybody in.
pub fn read(path: &Path) -> Result<Vec<Listed>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read the agents file {}: {e}", path.display())),
    };
    let mut agents: Vec<Listed> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let wrong =
            || format!("line {} of the agents file {} is not \"<name> <fingerprint> [<rules>]\"", number + 1, path.display());
        let mut parts = line.split_whitespace();
        let (Some(name), Some(fingerprint)) = (parts.next(), parts.next()) else {
            return Err(wrong());
        };
        // The rules are apart by spaces, as on the command line. Commas are
        // taken too.
        let words: Vec<&str> = parts.flat_map(|part| part.split(',')).filter(|word| !word.is_empty()).collect();
        let rules = match words.as_slice() {
            [] => None,
            words => Some(Rules::parse(words).map_err(|why| format!("{}: {why}", wrong()))?),
        };
        if !valid_name(name) || fingerprint.len() != 64 || !fingerprint.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(wrong());
        }
        if agents.iter().any(|agent| agent.name == name) {
            return Err(format!("the agents file {} names {name} twice", path.display()));
        }
        agents.push(Listed { name: name.to_string(), fingerprint: fingerprint.to_ascii_lowercase(), rules });
    }
    Ok(agents)
}

/// Holds the lock that lets one `orvud agent` command at a time change the
/// list, until it is dropped. Two that read the list at the same moment
/// would otherwise each write back their own change, and one would be lost.
fn changing(path: &Path) -> Result<std::fs::File, String> {
    let lock = path.with_file_name(format!(".{FILE}.lock"));
    let fail = |e: std::io::Error| format!("cannot lock the agents file {}: {e}", lock.display());
    let file = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&lock).map_err(fail)?;
    file.lock().map_err(fail)?;
    Ok(file)
}

/// Writes the list in place of the old one. The new file is written beside
/// it and then moved over it, so a sidecar reading it at that moment sees the
/// old list or the new one, never half of one. Only its owner can read it.
/// The folder is saved to disk too, so that after a power cut the file is the
/// new list and not the old one.
fn write(path: &Path, agents: &[Listed]) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("cannot write the agents file {}: {e}", path.display());
    let mut text =
        String::from("# The agents this sidecar lets in: a name, the fingerprint of its key, and any rules.\n");
    for agent in agents {
        match &agent.rules {
            None => text.push_str(&format!("{} {}\n", agent.name, agent.fingerprint)),
            Some(rules) => text.push_str(&format!("{} {} {rules}\n", agent.name, agent.fingerprint)),
        }
    }
    let new = path.with_file_name(format!(".{FILE}.new"));
    let _ = std::fs::remove_file(&new);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&new).map_err(fail)?;
    file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).map_err(fail)?;
    std::fs::rename(&new, path).map_err(fail)?;
    let folder = path.parent().filter(|folder| !folder.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::File::open(folder).and_then(|folder| folder.sync_all()).map_err(fail)
}

/// Adds an agent and returns its key. The key is not kept anywhere: this is
/// the one time it can be seen.
pub fn add(path: &Path, name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!(
            "{name:?} cannot be an agent's name: use a letter or digit, then up to 63 letters, digits, dots, dashes and underscores"
        ));
    }
    // A local sidecar's data folder may not be there yet. It is made the way
    // the sidecar makes it, so that only its owner can get into it.
    if let Some(folder) = path.parent().filter(|folder| !folder.as_os_str().is_empty()) {
        orvu_sdk::make_private_folder(folder).map_err(|e| format!("cannot create the folder {}: {e}", folder.display()))?;
    }
    let _changing = changing(path)?;
    let mut agents = read(path)?;
    if agents.iter().any(|agent| agent.name == name) {
        return Err(format!("there is already an agent called {name}. Remove it first to give it a new key"));
    }
    let key = new_key();
    agents.push(Listed { name: name.to_string(), fingerprint: fingerprint(key.as_bytes()), rules: None });
    write(path, &agents)?;
    Ok(key)
}

/// Removes an agent, and returns how many are left. A program connected as
/// that agent is turned away at its next call. With none left, the sidecar
/// lets nobody in.
pub fn remove(path: &Path, name: &str) -> Result<usize, String> {
    let _changing = changing(path)?;
    let mut agents = read(path)?;
    let before = agents.len();
    agents.retain(|agent| agent.name != name);
    if agents.len() == before {
        return Err(format!("there is no agent called {name}"));
    }
    write(path, &agents)?;
    Ok(agents.len())
}

/// Gives an agent rules, in place of the ones it had. `None` takes its rules
/// away, which lets it do everything. A program connected as the agent is held
/// to the new rules from its next call.
pub fn allow(path: &Path, name: &str, rules: Option<Rules>) -> Result<(), String> {
    let _changing = changing(path)?;
    let mut agents = read(path)?;
    let Some(agent) = agents.iter_mut().find(|agent| agent.name == name) else {
        return Err(format!("there is no agent called {name}"));
    };
    agent.rules = rules;
    write(path, &agents)
}

/// Why a program was not let in.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The sidecar names agents and was given no key.
    NoKey,
    /// The key is not one of the sidecar's agents'.
    UnknownKey,
    /// A key was given, but the sidecar names no agents, so it would check
    /// nothing. The program expected to be checked, and is told it was not.
    NoAgents,
    /// The agents file cannot be read, so nobody is let in.
    Unreadable(String),
    /// The sidecar had agents, and the last one was removed.
    NobodyLeft,
    /// Over the network only agents with keys are let in, and there is no
    /// agents file, or no key was given.
    KeysOnly,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NoKey => write!(f, "this sidecar only lets in agents with a key, and none was given"),
            Refusal::UnknownKey => write!(f, "this key is not one this sidecar knows: the agent may have been removed"),
            Refusal::NoAgents => write!(f, "a key was given, but this sidecar has no agents, so it cannot check it"),
            Refusal::Unreadable(why) => write!(f, "{why}"),
            Refusal::NobodyLeft => write!(f, "this sidecar lets nobody in: its last agent was removed. Add one, or delete its agents file to let every program in"),
            Refusal::KeysOnly => write!(f, "only agents with keys are let in over the network, and this sidecar names none, or none was given"),
        }
    }
}

/// How long the agents file must have been left alone before it counts as
/// settled. An editor that empties the file and then writes it again, or a
/// plain overwrite, leaves it empty or half-written for a moment, and that
/// moment must not end anybody's session for good.
pub const SETTLE: Duration = Duration::from_secs(2);

/// When the file was last seen, to tell whether it has changed since. The
/// time of the last change to the file itself (`ctime`) cannot be set back by
/// a program, unlike the time it was last written to.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    length: u64,
}

impl Stamp {
    /// Whether the file changed so lately that another change in the same
    /// moment might leave the same stamp. Such a file is read again each
    /// time, until it is older.
    fn recent(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        now - self.changed.0.max(self.modified.0) < 2
    }
}

fn stamp(path: &Path) -> Option<Stamp> {
    let found = std::fs::metadata(path).ok()?;
    Some(Stamp {
        device: found.dev(),
        inode: found.ino(),
        modified: (found.mtime(), found.mtime_nsec()),
        changed: (found.ctime(), found.ctime_nsec()),
        length: found.len(),
    })
}

/// The list as last read, as name and rules by fingerprint, or `None` if
/// there is no file.
type Known = Result<Option<Arc<BTreeMap<String, (String, Option<Rules>)>>>, String>;

/// The agents a running sidecar lets in. It reads the file again when it has
/// changed.
pub struct Agents {
    /// `None` lets every program in, and is for tests and for programs that
    /// embed the service.
    path: Option<PathBuf>,
    seen: Mutex<Option<(Option<Stamp>, Known)>>,
    /// How long the file must have been left alone before a refusal is
    /// taken as meant. See `settled`.
    settle: Duration,
}

impl Agents {
    /// Lets every program in, and checks no key.
    pub fn open_to_all() -> Self {
        Self { path: None, seen: Mutex::new(None), settle: SETTLE }
    }

    /// The agents named in the file at `path`.
    pub fn from_file(path: PathBuf) -> Self {
        Self { path: Some(path), seen: Mutex::new(None), settle: SETTLE }
    }

    // For tests only: a different settle time, so that a file just written
    // can count as settled without waiting.
    #[doc(hidden)]
    pub fn settling_after(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// The list, read again if the file has changed. Every call asks this, so
    /// the file is looked at and read with no lock held: the lock only
    /// guards the copy kept from last time.
    fn known(&self) -> Known {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        let now = stamp(path);
        if let Some((when, known)) = self.seen.lock().expect("agents lock").as_ref() {
            if *when == now && !now.is_some_and(|now| now.recent()) {
                return known.clone();
            }
        }
        let known = match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            _ => read(path).map(|agents| {
                Some(Arc::new(agents.into_iter().map(|agent| (agent.fingerprint, (agent.name, agent.rules))).collect()))
            }),
        };
        *self.seen.lock().expect("agents lock") = Some((now, known.clone()));
        known
    }

    /// Whether the agents file has been left alone for the settle time, so a
    /// refusal it gives can be taken as meant. Until then it may be in the
    /// middle of being written, empty or with only some of its lines. The time
    /// of the last change to the file (`ctime`) is used, which a program
    /// cannot set back or forward, so a file given a write time in the past
    /// does not count as settled early. A change time ahead of the clock,
    /// which only a clock set back can bring about, counts as not settled.
    ///
    /// Some editors save by moving the old file aside and writing a new one,
    /// so for a moment there is no file at all. A missing file is only taken
    /// as meant once its folder has been left alone for the settle time too.
    /// With no folder, or no file to look at, there is nothing to wait for.
    pub fn settled(&self) -> bool {
        self.settled_at(std::time::SystemTime::now())
    }

    /// `settled`, as it would be at the time `now`.
    fn settled_at(&self, now: std::time::SystemTime) -> bool {
        let Some(path) = &self.path else {
            return true;
        };
        let found = match std::fs::metadata(path) {
            Ok(found) => found,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let folder = path.parent().filter(|folder| !folder.as_os_str().is_empty()).unwrap_or(Path::new("."));
                match std::fs::metadata(folder) {
                    Ok(found) => found,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
                    // A folder that cannot be looked at never ends a session.
                    Err(_) => return false,
                }
            }
            // Nor does a file that cannot be looked at.
            Err(_) => return false,
        };
        let (Ok(secs), Ok(nanos)) = (u64::try_from(found.ctime()), u32::try_from(found.ctime_nsec())) else {
            return true;
        };
        let changed = std::time::UNIX_EPOCH + Duration::new(secs, nanos);
        now.duration_since(changed).is_ok_and(|age| age >= self.settle)
    }

    /// Whether the agents file names at least one agent, so that only
    /// programs with keys are let in.
    pub fn names_any(&self) -> bool {
        matches!(self.known(), Ok(Some(known)) if !known.is_empty())
    }

    /// Who a program is, from the key it gave when it connected. `None`
    /// means the sidecar has no agents file and the program gave no key: it
    /// is let in, as anybody was before agents had keys.
    pub fn admit(&self, credential: &[u8]) -> Result<Option<Agent>, Refusal> {
        self.admit_as(credential, false)
    }

    /// Like `admit`, for a program that comes over the network: one with no
    /// key is never let in.
    pub fn admit_with_key(&self, credential: &[u8]) -> Result<Agent, Refusal> {
        self.admit_as(credential, true)?.ok_or(Refusal::KeysOnly)
    }

    fn admit_as(&self, credential: &[u8], keys_only: bool) -> Result<Option<Agent>, Refusal> {
        let known = self.known().map_err(Refusal::Unreadable)?;
        if keys_only && known.is_none() {
            return Err(Refusal::KeysOnly);
        }
        // Space around the key, such as the line ending of a file it was read
        // from, is not part of it.
        let credential = credential.trim_ascii();
        match known {
            None if credential.is_empty() => Ok(None),
            None => Err(Refusal::NoAgents),
            Some(known) if known.is_empty() => Err(Refusal::NobodyLeft),
            Some(_) if credential.is_empty() => Err(Refusal::NoKey),
            Some(known) => {
                let fingerprint = fingerprint(credential);
                match known.get(&fingerprint) {
                    Some((name, _)) => Ok(Some(Agent { name: name.clone(), fingerprint })),
                    None => Err(Refusal::UnknownKey),
                }
            }
        }
    }

    /// Whether a program that was let in as `agent` still is, and if so,
    /// the rules it is held to now: `None` for none, which lets it do
    /// everything. An agent that was removed is not let in, nor is a program
    /// with no key once the sidecar has agents, nor anybody once the last
    /// agent was removed. With the file deleted, everybody is let in again.
    pub fn still_admits(&self, agent: Option<&Agent>) -> Result<Option<Rules>, Refusal> {
        self.still_admits_as(agent, false)
    }

    /// Like `still_admits`, for a program that came over the network: with
    /// the agents file gone, it is not let in, as nobody without a key is.
    pub fn still_admits_with_key(&self, agent: &Agent) -> Result<Option<Rules>, Refusal> {
        self.still_admits_as(Some(agent), true)
    }

    fn still_admits_as(&self, agent: Option<&Agent>, keys_only: bool) -> Result<Option<Rules>, Refusal> {
        let known = self.known().map_err(Refusal::Unreadable)?;
        match (known, agent) {
            (None, _) if keys_only => Err(Refusal::KeysOnly),
            (None, _) => Ok(None),
            (Some(known), _) if known.is_empty() => Err(Refusal::NobodyLeft),
            (Some(_), None) => Err(Refusal::NoKey),
            (Some(known), Some(agent)) => match known.get(&agent.fingerprint) {
                Some((name, rules)) if *name == agent.name => Ok(rules.clone()),
                _ => Err(Refusal::UnknownKey),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_key_is_shown_once_and_only_its_fingerprint_is_kept() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let key = add(&path, "alice").unwrap();
        assert!(key.starts_with("orvu-key-") && key.len() == 9 + 64, "{key}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(&key[9..]), "the key itself was written down");
        assert_eq!(read(&path).unwrap(), [Listed { name: "alice".into(), fingerprint: fingerprint(key.as_bytes()), rules: None }]);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn each_agent_has_a_key_of_its_own() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let alice = add(&path, "alice").unwrap();
        let bob = add(&path, "bob").unwrap();
        assert_ne!(alice, bob);
        let agents = Agents::from_file(path);
        assert_eq!(agents.admit(alice.as_bytes()).unwrap().unwrap().name, "alice");
        assert_eq!(agents.admit(bob.as_bytes()).unwrap().unwrap().name, "bob");
    }

    #[test]
    fn mistakes_are_explained() {
        let dir = folder();
        let path = dir.path().join(FILE);
        add(&path, "alice").unwrap();
        assert!(add(&path, "alice").unwrap_err().contains("already an agent called alice"));
        assert!(add(&path, "no spaces").unwrap_err().contains("cannot be an agent's name"));
        assert!(add(&path, "").unwrap_err().contains("cannot be an agent's name"));
        assert!(remove(&path, "bob").unwrap_err().contains("no agent called bob"));
        remove(&path, "alice").unwrap();
        assert_eq!(read(&path).unwrap(), []);
    }

    #[test]
    fn with_no_agents_everybody_is_let_in_without_a_key() {
        let dir = folder();
        let agents = Agents::from_file(dir.path().join(FILE));
        assert_eq!(agents.admit(b""), Ok(None));
        assert_eq!(agents.still_admits(None), Ok(None));
        // A key means the program expects to be checked. It is told that it
        // cannot be.
        assert_eq!(agents.admit(b"orvu-key-00"), Err(Refusal::NoAgents));
        assert_eq!(Agents::open_to_all().admit(b""), Ok(None));
    }

    #[test]
    fn with_agents_a_key_is_needed_and_has_to_be_known() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone());
        let key = add(&path, "alice").unwrap();
        assert_eq!(agents.admit(b""), Err(Refusal::NoKey));
        assert_eq!(agents.admit(b"orvu-key-0123"), Err(Refusal::UnknownKey));
        let alice = agents.admit(key.as_bytes()).unwrap();
        assert_eq!(agents.still_admits(alice.as_ref()), Ok(None));
        // A program that came in before the first agent was added is no
        // longer let in.
        assert_eq!(agents.still_admits(None), Err(Refusal::NoKey));
    }

    #[test]
    fn an_agent_that_is_removed_is_turned_away() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone());
        let key = add(&path, "alice").unwrap();
        add(&path, "bob").unwrap();
        let alice = agents.admit(key.as_bytes()).unwrap();
        remove(&path, "alice").unwrap();
        assert_eq!(agents.still_admits(alice.as_ref()), Err(Refusal::UnknownKey));
        assert_eq!(agents.admit(key.as_bytes()), Err(Refusal::UnknownKey));
        // Added again under the same name, it has a new key. The old one
        // still does not work.
        let again = add(&path, "alice").unwrap();
        assert_eq!(agents.still_admits(alice.as_ref()), Err(Refusal::UnknownKey));
        assert_eq!(agents.admit(again.as_bytes()).unwrap().unwrap().name, "alice");
    }

    // A file that cannot be read must not let everybody in.
    #[test]
    fn a_file_that_cannot_be_read_lets_nobody_in() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone());
        std::fs::write(&path, "alice not-a-fingerprint\n").unwrap();
        assert!(matches!(agents.admit(b""), Err(Refusal::Unreadable(why)) if why.contains("line 1")));
        assert!(matches!(agents.still_admits(None), Err(Refusal::Unreadable(_))));
        std::fs::write(&path, format!("alice {0}\nalice {0}\n", "a".repeat(64))).unwrap();
        assert!(matches!(agents.admit(b""), Err(Refusal::Unreadable(why)) if why.contains("twice")));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(agents.admit(b""), Err(Refusal::Unreadable(_))));
    }

    // Once there have been agents, taking the last one away must not open
    // the sidecar to everybody again. Only deleting the file does that.
    #[test]
    fn removing_the_last_agent_lets_nobody_in() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone());
        let key = add(&path, "alice").unwrap();
        remove(&path, "alice").unwrap();
        assert_eq!(agents.admit(b""), Err(Refusal::NobodyLeft));
        assert_eq!(agents.admit(key.as_bytes()), Err(Refusal::NobodyLeft));
        assert_eq!(agents.still_admits(None), Err(Refusal::NobodyLeft));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(agents.admit(b""), Ok(None));
    }

    // Two changes at the same moment, from two `orvud agent` commands, both
    // land.
    #[test]
    fn changes_made_at_the_same_moment_are_all_kept() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for number in 0..10 {
                        add(&path, &format!("agent-{thread}-{number}")).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(read(&path).unwrap().len(), 80);
    }

    // A file written over in place, to the same length, with its time set
    // back to what it was, is still seen to have changed.
    #[test]
    fn a_change_that_keeps_the_size_and_time_is_still_seen() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone());
        let alice = add(&path, "alice").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let when = file.metadata().unwrap().modified().unwrap();
        assert!(agents.admit(alice.as_bytes()).is_ok());
        let other = "b".repeat(64);
        let text = std::fs::read_to_string(&path).unwrap().replace(&fingerprint(alice.as_bytes()), &other);
        std::fs::write(&path, text).unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(when).unwrap();
        assert_eq!(agents.admit(alice.as_bytes()), Err(Refusal::UnknownKey));
    }

    // A key read from a file often keeps its line ending.
    #[test]
    fn a_key_with_space_around_it_is_the_same_key() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let key = add(&path, "alice").unwrap();
        let agents = Agents::from_file(path);
        assert_eq!(agents.admit(format!("{key}\n").as_bytes()).unwrap().unwrap().name, "alice");
        assert_eq!(agents.admit(format!(" {key}\r\n").as_bytes()).unwrap().unwrap().name, "alice");
    }

    // A settings file that is not there is a mistake in the path: the
    // sidecar would never read the agents put next to it.
    #[test]
    fn a_settings_file_has_to_be_there() {
        let dir = folder();
        let settings = dir.path().join("orvu.toml");
        let mode = crate::cli::Mode::Config(settings.clone());
        let none = |_: &str| None;
        assert!(file_for(&mode, none).unwrap_err().contains("no settings file"));
        std::fs::write(&settings, "").unwrap();
        assert_eq!(file_for(&mode, none).unwrap(), dir.path().join(FILE));
    }

    // Rules are kept in the file, and a running sidecar holds an agent to
    // the ones it has now.
    #[test]
    fn rules_are_kept_with_the_agent_and_read_again() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone());
        let key = add(&path, "alice").unwrap();
        let alice = agents.admit(key.as_bytes()).unwrap();
        assert_eq!(agents.still_admits(alice.as_ref()), Ok(None));
        let rules = Rules::parse(&["notes:write", "budgets:spend"]).unwrap();
        allow(&path, "alice", Some(rules.clone())).unwrap();
        assert_eq!(read(&path).unwrap()[0].rules, Some(rules.clone()));
        assert!(std::fs::read_to_string(&path).unwrap().contains(" notes:write budgets:spend\n"));
        assert_eq!(agents.still_admits(alice.as_ref()), Ok(Some(rules)));
        allow(&path, "alice", Some(Rules::parse(&["none"]).unwrap())).unwrap();
        assert_eq!(agents.still_admits(alice.as_ref()), Ok(Some(Rules::default())));
        allow(&path, "alice", None).unwrap();
        assert_eq!(agents.still_admits(alice.as_ref()), Ok(None));
        assert!(allow(&path, "bob", None).unwrap_err().contains("no agent called bob"));
    }

    // Rules in the file are written apart by spaces, as on the command line,
    // and commas are taken too.
    #[test]
    fn rules_in_the_file_may_be_apart_by_spaces_or_commas() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let want = Some(Rules::parse(&["notes:write", "budgets:spend"]).unwrap());
        for line in ["alice {fp} notes:write budgets:spend", "alice {fp} notes:write,budgets:spend"] {
            std::fs::write(&path, line.replace("{fp}", &"a".repeat(64)) + "\n").unwrap();
            assert_eq!(read(&path).unwrap()[0].rules, want, "{line}");
        }
    }

    #[test]
    fn a_wrong_rule_in_the_file_lets_nobody_in() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone());
        std::fs::write(&path, format!("alice {} notes:admin\n", "a".repeat(64))).unwrap();
        assert!(matches!(agents.admit(b""), Err(Refusal::Unreadable(why)) if why.contains("is not a rule")));
    }

    #[test]
    fn names_follow_the_rule_for_every_name() {
        for good in ["alice", "a", "agent-7", "x.y_z", &"a".repeat(64)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "-a", ".a", "a b", "é", "a/b", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    fn changed(path: &Path) -> std::time::SystemTime {
        let found = std::fs::metadata(path).unwrap();
        std::time::UNIX_EPOCH + Duration::new(found.ctime() as u64, found.ctime_nsec() as u32)
    }

    #[test]
    fn a_missing_file_is_settled_once_its_folder_is() {
        let dir = folder();
        let path = dir.path().join(FILE);
        // Nothing to wait for.
        assert!(Agents::from_file(path.clone()).settling_after(Duration::ZERO).settled());
        // The folder was only just made, so the file may be on its way.
        assert!(!Agents::from_file(path.clone()).settled());
        // With no folder there is nothing to wait for either.
        assert!(Agents::from_file(dir.path().join("gone").join(FILE)).settled());
    }

    // An agents file that cannot be looked at never counts as settled, so it
    // never ends a session, however long it has been left alone.
    #[test]
    fn a_file_that_cannot_be_looked_at_is_never_settled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder();
        let path = dir.path().join(FILE);
        add(&path, "alice").unwrap();
        let agents = Agents::from_file(path.clone()).settling_after(Duration::ZERO);
        assert!(agents.settled());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let looked = std::fs::metadata(&path).map(drop);
        let settled = agents.settled();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(looked.map_err(|e| e.kind()), Err(std::io::ErrorKind::PermissionDenied), "the file could still be looked at");
        assert!(!settled, "a file that cannot be looked at counted as settled");
    }

    // A change time ahead of the clock can only come from a clock set back,
    // and does not count as settled, for the file or for its folder.
    #[test]
    fn a_change_ahead_of_the_clock_is_not_settled() {
        let dir = folder();
        let path = dir.path().join(FILE);
        let agents = Agents::from_file(path.clone()).settling_after(Duration::ZERO);
        let early = |path: &Path| changed(path) - Duration::from_secs(60);
        assert!(agents.settled_at(changed(dir.path())));
        assert!(!agents.settled_at(early(dir.path())));
        add(&path, "alice").unwrap();
        assert!(agents.settled_at(changed(&path)));
        assert!(!agents.settled_at(early(&path)));
    }
}
