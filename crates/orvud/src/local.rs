//! Starting the sidecar with no setup: no settings file, no certificates and
//! no network. Everything it needs lives in one data folder.
//!
//! The folder holds five things: the saved data (`orvu.redb`), this machine's
//! number (`replica`), the same number noted beside the data
//! (`orvu.redb.replica`, as an engine started from settings keeps it), a note
//! of the computer the folder is used on (`computer`), and a lock file
//! (`orvud.lock`) that stops two sidecars from using the folder at once. While
//! the program runs, it also holds a note of where its socket is
//! (`orvud.where`).

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::{Client, Result, SdkError};
use orvu_transport::MemTransport;
use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The namespace a local sidecar serves.
pub const LOCAL_NAMESPACE: &str = "local";

/// A running local engine. Keep it for as long as the engine runs: dropping it
/// gives up the lock on the data folder.
pub struct Local {
    pub client: Arc<Client>,
    pub replica: ReplicaId,
    pub namespace: String,
    _lock: File,
}

/// A data folder that this sidecar has taken for itself and nobody else can
/// use. Nothing has been written into it yet apart from the lock file.
pub struct Claimed {
    data_dir: PathBuf,
    lock: File,
}

fn config_error(what: &str, path: &Path, e: impl std::fmt::Display) -> SdkError {
    SdkError::Config(format!("{what} {}: {e}", path.display()))
}

/// Starts an engine that keeps its data in `data_dir` and talks to no other
/// machine. This is `claim` followed by `Claimed::start`, and a folder last
/// used on another computer is refused.
pub async fn start(data_dir: &Path) -> Result<Local> {
    claim(data_dir)?.start(IfCopied::Refuse).await
}

/// What to do with a data folder that was last used on another computer.
///
/// A folder keeps its machine number. Copied to a second computer and run on
/// both, it would make two machines share a number, which must never happen:
/// each would take the other's changes for its own. So by default such a
/// folder is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IfCopied {
    /// Do not start.
    #[default]
    Refuse,
    /// Keep the data and take a new machine number. For a copy that is to go
    /// on as a machine of its own while the original keeps running. What was
    /// saved before keeps the number it was written under; only what is
    /// written from now on is under the new one.
    NewNumber,
    /// Keep the data and the number. Only for a folder that was moved here and
    /// is no longer used where it came from.
    AcceptMove,    /// Keep the data and take a new machine number, on any computer. For a
    /// folder restored from a backup or rolled back some other way: it has
    /// lost changes the others may hold, and writing under its old number
    /// would reuse numbers they have already seen.
    Restored,
}

/// Takes the data folder for this sidecar. The folder is created if it is
/// missing. Fails if another sidecar is already using it, or if other users
/// can get into it.
///
/// This writes nothing but the lock file. A program can do it first, sort out
/// everything else that might fail, and only then start the engine.
pub fn claim(data_dir: &Path) -> Result<Claimed> {
    use std::os::unix::fs::MetadataExt;
    orvu_sdk::make_private_folder(data_dir)
        .map_err(|e| config_error("cannot create the data folder", data_dir, e))?;
    // A folder that was already there is used as it is, so it has to be
    // private already. The saved data and the socket inside it are only safe
    // from other users because they cannot get into the folder.
    let folder = std::fs::metadata(data_dir)
        .map_err(|e| config_error("cannot look at the data folder", data_dir, e))?;
    if !folder.is_dir() {
        return Err(SdkError::Config(format!("{} is not a folder", data_dir.display())));
    }
    if folder.mode() & 0o077 != 0 {
        return Err(SdkError::Config(format!(
            "the data folder {} can be used by other users (its mode is {:o}): \
             run `chmod 700 {}`, or use a different --data-dir",
            data_dir.display(),
            folder.mode() & 0o777,
            data_dir.display()
        )));
    }
    let lock = lock(data_dir, || {
        format!(
            "another orvud is already using the data folder {}: stop it, or use a different --data-dir",
            data_dir.display()
        )
    })?;
    // The lock file was made by us just now, or by an earlier run of ours. A
    // folder that belongs to someone else is not ours to keep data in, even
    // if its mode looks private.
    let lock_owner = lock.metadata().map_err(|e| config_error("cannot look at the lock in", data_dir, e))?;
    if lock_owner.uid() != folder.uid() {
        return Err(SdkError::Config(format!(
            "the data folder {} belongs to another user: use a different --data-dir",
            data_dir.display()
        )));
    }
    Ok(Claimed { data_dir: data_dir.to_path_buf(), lock })
}

impl Claimed {
    /// Starts the engine in the claimed folder. `if_copied` says what to do if
    /// the folder was last used on another computer.
    pub async fn start(self, if_copied: IfCopied) -> Result<Local> {
        let computer = this_computer();
        self.start_on(computer.as_deref(), if_copied).await
    }

    /// Like `start`, as if this were the computer called `computer`, or one
    /// that cannot tell which computer it is if `None`. Tests use it to play
    /// a second computer.
    pub async fn start_on(self, computer: Option<&str>, if_copied: IfCopied) -> Result<Local> {
        let Claimed { data_dir, lock } = self;
        let replica = replica_id(&data_dir, computer, if_copied)?;
        note_number(&data_dir, replica)?;
        let config = node_config(&data_dir, replica)?;
        // A link that stays inside this program. Nothing listens on the network.
        let transport = Arc::new(MemTransport::new()).endpoint(replica.0);
        let client = Client::start(transport, config).await?;
        Ok(Local { client: Arc::new(client), replica, namespace: LOCAL_NAMESPACE.to_string(), _lock: lock })
    }
}

/// The engine's settings for a local sidecar in `data_dir`. Each change is
/// saved to disk before it is answered, unless `orvud.toml` in the folder asks
/// for saving every so often instead.
pub fn node_config(data_dir: &Path, replica: ReplicaId) -> Result<NodeConfig> {
    let mut config = NodeConfig::new(replica, "local");
    config.namespace = LOCAL_NAMESPACE.to_string();
    config.store_path = Some(data_dir.join(STORE_FILE));
    config.fsync = crate::observe::local_fsync(data_dir).map_err(SdkError::Config)?;
    Ok(config)
}

/// The name of the file the saved data is kept in.
const STORE_FILE: &str = "orvu.redb";

/// Takes the lock on a data folder, or says `in_use` if another sidecar has
/// it. The lock belongs to the open file, so the system releases it when the
/// sidecar ends, however it ends.
pub(crate) fn lock(data_dir: &Path, in_use: impl FnOnce() -> String) -> Result<File> {
    lock_on(data_dir, in_use, file_system_over_network)
}

/// `lock`, with `over_network` telling whether the folder is on a network
/// file system.
fn lock_on(
    data_dir: &Path,
    in_use: impl FnOnce() -> String,
    over_network: impl Fn(&Path) -> Option<&'static str>,
) -> Result<File> {
    let path = data_dir.join("orvud.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| config_error("cannot open the lock file", &path, e))?;
    match file.try_lock() {
        Ok(()) => {
            if let Some(warning) = network_warning(data_dir, over_network) {
                tracing::warn!(folder = %data_dir.display(), "{warning}");
                crate::say::line(format_args!("orvud: warning: {warning}"));
            }
            Ok(file)
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(SdkError::Config(in_use())),
        Err(std::fs::TryLockError::Error(e)) => Err(config_error("cannot lock", &path, e)),
    }
}

/// What to say when the folder `data_dir` is on a file system reached over
/// the network, which `over_network` tells by naming its kind. `None` when it
/// is not, or that cannot be told.
///
/// The lock that keeps a second sidecar out of a folder is not to be relied
/// on there: some network file systems do not pass locks on at all, or lose
/// them when the connection drops. The folder is still used, as it may be
/// the only place there is, but whoever started the sidecar is told.
fn network_warning(data_dir: &Path, over_network: impl Fn(&Path) -> Option<&'static str>) -> Option<String> {
    let kind = over_network(data_dir)?;
    Some(format!(
        "the folder {} is on a network file system ({kind}). The lock that keeps a second \
         sidecar out of it may not hold there, and two sidecars using one folder can damage \
         its data. Keep it on a local disk",
        data_dir.display()
    ))
}

/// The kind of network file system `path` is on, or `None` if it is on a
/// local one or that cannot be told. Only Linux is asked.
fn file_system_over_network(path: &Path) -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: an all-zero `statfs` is a valid value of a plain C struct,
        // which the call fills in.
        let mut found: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `name` is a valid C string and `found` a valid `statfs`,
        // both alive for the whole call.
        if unsafe { libc::statfs(name.as_ptr(), &mut found) } != 0 {
            return None;
        }
        // The number is 32 bits wide, whatever type holds it on this system.
        let magic = (found.f_type as u64) & 0xFFFF_FFFF;
        let fuse = if magic == FUSE { fuse_kind(path) } else { None };
        network_kind(magic, fuse.as_deref())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

/// The number Linux gives every FUSE file system.
const FUSE: u64 = 0x6573_5546;

/// The kind of network file system with the number `magic`, which Linux's
/// `statfs` gives. Every FUSE file system has the same number, so for one of
/// those `fuse` is the kind the system lists it as, such as `fuse.sshfs`;
/// a FUSE file system can as well be on a local disk.
fn network_kind(magic: u64, fuse: Option<&str>) -> Option<&'static str> {
    match magic {
        0x6969 => Some("NFS"),
        0x517B | 0xFF53_4D42 | 0xFE53_4D42 => Some("SMB (CIFS)"),
        0x0102_1997 => Some("9P"),
        0x5346_414F | 0x6B41_4653 => Some("AFS"),
        0x00C3_6400 => Some("CephFS"),
        0x7375_7245 => Some("Coda"),
        0x564C => Some("NetWare"),
        FUSE => match fuse? {
            "fuse.sshfs" => Some("SSHFS"),
            "fuse.rclone" => Some("rclone"),
            "fuse.s3fs" => Some("s3fs"),
            "fuse.gcsfuse" => Some("gcsfuse"),
            "fuse.glusterfs" => Some("GlusterFS"),
            "fuse.ceph-fuse" => Some("CephFS"),
            _ => None,
        },
        _ => None,
    }
}

/// The kind the system lists the FUSE file system `path` is on as, such as
/// `fuse.sshfs`: the kind of the deepest mount that holds `path`.
#[cfg(target_os = "linux")]
fn fuse_kind(path: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        // The fields are: id, parent, device, root, mount point, options,
        // optional fields, then " - " and the kind, the source and more
        // options. A space in the mount point is written as \040.
        let Some((head, tail)) = line.split_once(" - ") else { continue };
        let Some(point) = head.split(' ').nth(4) else { continue };
        let Some(kind) = tail.split(' ').next() else { continue };
        let point = point.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\");
        let depth = Path::new(&point).components().count();
        if path.starts_with(&point) && best.as_ref().is_none_or(|(deepest, _)| depth >= *deepest) {
            best = Some((depth, kind.to_string()));
        }
    }
    best.map(|(_, kind)| kind)
}

/// This machine's number. It is picked at random the first time and read back
/// from the `replica` file after that, so it stays the same across restarts.
///
/// A number is only picked when the folder holds no saved data yet. Saved data
/// was written under one number, and some of it, such as an allowance on a
/// bounded counter, can only ever be used under that number. So a folder with
/// saved data and no number file is an error, not a first start.
///
/// Beside the number the folder notes which computer it is used on. A folder
/// with no note, from before notes were kept, is taken to belong to this
/// computer. One whose note names another computer is dealt with as
/// `if_copied` says. Where it cannot be told which computer this is, no note
/// is written and none is checked. A folder said to be restored takes a new
/// number wherever it is.
fn replica_id(data_dir: &Path, computer: Option<&str>, if_copied: IfCopied) -> Result<ReplicaId> {
    let path = data_dir.join("replica");
    let note = data_dir.join(COMPUTER_FILE);
    let here = computer.map(fingerprint);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let replica = match text.trim().parse::<u64>() {
                Ok(n) if n != 0 => ReplicaId(n),
                _ => {
                    return Err(SdkError::Config(format!(
                        "{} does not hold a machine number: it holds {:?}",
                        path.display(),
                        text.trim()
                    )))
                }
            };
            let noted = match std::fs::read_to_string(&note) {
                Ok(noted) => Some(noted.trim().to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(config_error("cannot read", &note, e)),
            };
            if if_copied == IfCopied::Restored {
                return new_number(data_dir, replica, here.as_deref());
            }
            let Some(here) = here else {
                return Ok(replica);
            };
            match noted {
                Some(noted) if noted == here => Ok(replica),
                None => {
                    write_safely(data_dir, COMPUTER_FILE, &here)?;
                    Ok(replica)
                }
                Some(_) => match if_copied {
                    IfCopied::Refuse => {
                        let command = start_command(data_dir);
                        Err(SdkError::Config(format!(
                            "the data folder {} seems to have been copied from another computer: it was \
                             last used on a different one, as machine {}. Two computers must never run as \
                             the same machine. If the other computer still uses its copy, start this one \
                             once with `{command} --new-number`: it keeps the data and takes a new number \
                             of its own. If the folder was moved here and is no longer used where it came \
                             from, start it once with `{command} --moved`",
                            data_dir.display(),
                            replica.0
                        )))
                    }
                    IfCopied::AcceptMove => {
                        write_safely(data_dir, COMPUTER_FILE, &here)?;
                        Ok(replica)
                    }
                    // The note of the computer goes last. Until it is
                    // written the folder still looks copied, so a start that
                    // stops partway is refused next time as before, and a new
                    // number can simply be taken again.
                    IfCopied::NewNumber | IfCopied::Restored => new_number(data_dir, replica, Some(&here)),
                },
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if data_dir.join(STORE_FILE).exists() {
                return Err(SdkError::Config(format!(
                    "{} holds saved data but the file {} is missing, so it is not known which \
                     machine number the data was saved under: restore that file, or use a \
                     different --data-dir",
                    data_dir.display(),
                    path.display()
                )));
            }
            let replica = random_replica();
            if let Some(here) = &here {
                write_safely(data_dir, COMPUTER_FILE, here)?;
            }
            write_safely(data_dir, "replica", &replica.0.to_string())?;
            Ok(replica)
        }
        Err(e) => Err(config_error("cannot read", &path, e)),
    }
}

/// The command that starts a local sidecar on `data_dir`, as it can be typed
/// into a shell: the folder is spelled out in full, and quoted if it needs to
/// be. A folder whose name is not text is spelled byte by byte, in the
/// `$'...'` quotes bash and zsh read, so that the command still names it.
fn start_command(data_dir: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let folder = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let folder = match folder.to_str() {
        Some(text) if text.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c)) => text.to_string(),
        Some(text) => format!("'{}'", text.replace('\'', r"'\''")),
        None => {
            let mut quoted = String::from("$'");
            for &byte in folder.as_os_str().as_bytes() {
                match byte {
                    b'\'' | b'\\' => quoted.extend(['\\', byte as char]),
                    b' '..=b'~' => quoted.push(byte as char),
                    _ => quoted.push_str(&format!("\\x{byte:02x}")),
                }
            }
            quoted.push('\'');
            quoted
        }
    };
    format!("orvud --local --data-dir {folder}")
}

/// Notes `replica` beside the saved data as well, as an engine started from
/// settings does, so that the folder can later be run from settings under
/// its own number. A note that names another machine is refused. A folder
/// given a new number has the new one noted already.
fn note_number(data_dir: &Path, replica: ReplicaId) -> Result<()> {
    let note = data_note(&data_dir.join(STORE_FILE));
    match read_number(&note)? {
        Some(noted) if noted == replica.0 => Ok(()),
        Some(noted) => Err(SdkError::Config(format!(
            "{} says this folder is machine {}, but {} says the data in it was written as machine \
             {noted}. The data's own note is the one to go by: put {noted} in {}",
            data_dir.join("replica").display(),
            replica.0,
            note.display(),
            data_dir.join("replica").display()
        ))),
        None => write_safely(data_dir, &format!("{STORE_FILE}.replica"), &replica.0.to_string()),
    }
}

/// Takes a machine number other than `old` for the data in `data_dir`, keeps
/// it, and notes that the folder is used on the computer `here`, if known.
///
/// The number beside the data goes first: were the folder's number to go
/// first and that one not follow, the two would disagree, and the data's own
/// note, which is the one to go by, would name the old number.
fn new_number(data_dir: &Path, old: ReplicaId, here: Option<&str>) -> Result<ReplicaId> {
    let fresh = loop {
        let fresh = random_replica();
        if fresh != old {
            break fresh;
        }
    };
    write_safely(data_dir, &format!("{STORE_FILE}.replica"), &fresh.0.to_string())?;
    write_safely(data_dir, "replica", &fresh.0.to_string())?;
    if let Some(here) = here {
        write_safely(data_dir, COMPUTER_FILE, here)?;
    }
    Ok(fresh)
}

/// The file beside the saved data that notes the number it was written as.
/// The engine writes it, the first time it starts on the data.
pub(crate) fn data_note(store: &Path) -> PathBuf {
    let mut note = store.as_os_str().to_owned();
    note.push(".replica");
    PathBuf::from(note)
}

/// The machine number in the file at `path`, or `None` if there is no such
/// file.
pub(crate) fn read_number(path: &Path) -> Result<Option<u64>> {
    match std::fs::read_to_string(path) {
        Ok(text) => match text.trim().parse::<u64>() {
            Ok(number) => Ok(Some(number)),
            Err(_) => Err(SdkError::Config(format!(
                "{} does not hold a machine number: it holds {:?}",
                path.display(),
                text.trim()
            ))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SdkError::Config(format!("cannot read {}: {e}", path.display()))),
    }
}

/// The file that notes which computer the folder is used on.
const COMPUTER_FILE: &str = "computer";

/// Writes `text` to the file `name` in `folder`, so that it is either all
/// there or not changed at all, even if the power goes partway through.
pub(crate) fn write_safely(folder: &Path, name: &str, text: &str) -> Result<()> {
    // Write to a second name and rename it into place, so that a crash
    // partway through cannot leave a half-written file.
    let staged = folder.join(format!("{name}.new"));
    let path = folder.join(name);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&staged)
        .map_err(|e| config_error("cannot write", &staged, e))?;
    writeln!(file, "{text}").map_err(|e| config_error("cannot write", &staged, e))?;
    file.sync_all().map_err(|e| config_error("cannot save", &staged, e))?;
    std::fs::rename(&staged, &path).map_err(|e| config_error("cannot write", &path, e))?;
    // The new name is only safe from a power cut once the folder itself has
    // been saved. Data is written under the number next, so it must not be
    // possible for the data to outlive the number.
    File::open(folder)
        .and_then(|folder| folder.sync_all())
        .map_err(|e| config_error("cannot save", folder, e))
}

/// What the `computer` file holds for the computer called `computer`. The
/// computer's id is meant to be kept to itself, so only a value worked out
/// from it is written down.
fn fingerprint(computer: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, format!("orvu data folder\n{computer}").as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Which computer this is, or `None` if it cannot be told. On Linux that is
/// its machine id from `/etc/machine-id`. A Mac has no such file, and gives
/// its hardware id instead, the one System Information shows as "Hardware
/// UUID". A host name is not used in place of either: it can change under a
/// computer that stays the same, as it does on a laptop that joins another
/// network or in a container made again over the same folder, and the
/// computer's own folder would then look copied.
pub fn this_computer() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        hardware_computer(hardware_id())
    }
    #[cfg(not(target_os = "macos"))]
    {
        computer_of(std::fs::read_to_string("/etc/machine-id").ok())
    }
}

/// The Mac's hardware id, or `None` if the system does not give it within a
/// second.
#[cfg(target_os = "macos")]
fn hardware_id() -> Option<[u8; 16]> {
    let mut id = [0u8; 16];
    let wait = libc::timespec { tv_sec: 1, tv_nsec: 0 };
    // SAFETY: `id` has room for the 16 bytes the call writes, and `wait`
    // lives for the whole call.
    let done = unsafe { libc::gethostuuid(id.as_mut_ptr(), &wait) };
    (done == 0).then_some(id)
}

/// Which computer has the hardware id `id`. An id of all zeros names none.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn hardware_computer(id: Option<[u8; 16]>) -> Option<String> {
    let id = id?;
    if id == [0u8; 16] {
        return None;
    }
    let hex: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(format!("hardware-id {hex}"))
}

/// Which computer has the machine id `id`, as `/etc/machine-id` held it.
fn computer_of(id: Option<String>) -> Option<String> {
    let id = id?;
    let id = id.trim();
    (!id.is_empty()).then(|| format!("machine-id {id}"))
}

/// The largest machine number there may be: 2^53 - 1. Up to there a language
/// that keeps every number as a floating-point value, such as JavaScript,
/// still holds it exactly. A client that rounded this machine's number would
/// name a machine that does not exist.
pub const LARGEST_REPLICA: u64 = (1 << 53) - 1;

/// A random machine number between 1 and `LARGEST_REPLICA`. Zero is
/// left out, because a request that forgets to name a machine reads as
/// machine 0.
fn random_replica() -> ReplicaId {
    loop {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).expect("OS entropy source");
        let n = u64::from_le_bytes(bytes) & LARGEST_REPLICA;
        if n != 0 {
            return ReplicaId(n);
        }
    }
}

/// The data folder to use when `--data-dir` was not given, or `None` if the
/// system gives no home folder to put it in.
///
/// On macOS it is `~/Library/Application Support/orvu`. Elsewhere it follows
/// the XDG convention: `$XDG_DATA_HOME/orvu`, or `~/.local/share/orvu` when
/// that variable is not set. A variable that is empty or does not hold an
/// absolute path counts as not set, as the convention says. The folder would
/// otherwise depend on where the program was started.
pub fn default_data_dir(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| absolute(env(name));
    if cfg!(target_os = "macos") {
        return set("HOME").map(|home| home.join("Library/Application Support/orvu"));
    }
    set("XDG_DATA_HOME")
        .map(|dir| dir.join("orvu"))
        .or_else(|| set("HOME").map(|home| home.join(".local/share/orvu")))
}

/// The socket to serve on in local mode when `--socket` was not given:
/// `$XDG_RUNTIME_DIR/orvud.sock` if that variable holds an absolute path, and
/// otherwise a socket inside the data folder, which only its owner can enter.
pub fn default_socket(data_dir: &Path, env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    match absolute(env("XDG_RUNTIME_DIR")) {
        Some(dir) => dir.join("orvud.sock"),
        None => data_dir.join("orvud.sock"),
    }
}

/// The note in the data folder that says where a local sidecar's socket is.
///
/// The socket's place can depend on settings a terminal has and a program
/// started some other way may not, such as `$XDG_RUNTIME_DIR`. The data
/// folder is found the same way by both, so the sidecar writes where its
/// socket is there, and the client libraries look for it there.
///
/// Only the sidecar holding the folder's lock writes the note, so two
/// sidecars never both write it. The note is taken away when this is
/// dropped, if it still names this sidecar's socket. A note left by a
/// sidecar that was killed names a socket nobody answers on, and readers
/// check that before they use it.
pub struct SocketNote {
    path: PathBuf,
    socket: Vec<u8>,
}

/// The name of the note in the data folder.
pub const SOCKET_NOTE: &str = "orvud.where";

impl SocketNote {
    /// Writes the note for `socket`, as a whole path, in `data_dir`.
    pub fn write(data_dir: &Path, socket: &Path) -> std::io::Result<SocketNote> {
        use std::os::unix::ffi::OsStrExt;
        let socket = std::path::absolute(socket)?.into_os_string().as_bytes().to_vec();
        let path = data_dir.join(SOCKET_NOTE);
        // Written beside it and then moved into place, so that a reader
        // never sees half a note.
        let fresh = data_dir.join(format!("{SOCKET_NOTE}.{}", std::process::id()));
        let written = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&fresh)
            .and_then(|mut file| file.write_all(&socket))
            .and_then(|()| std::fs::rename(&fresh, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&fresh);
            return Err(e);
        }
        Ok(SocketNote { path, socket })
    }
}

impl Drop for SocketNote {
    fn drop(&mut self) {
        if std::fs::read(&self.path).is_ok_and(|noted| noted == self.socket) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The value as a path, if it is there and is an absolute path.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value.map(PathBuf::from).filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Taking the lock on a folder on a network file system says so, in the
    // log and to whoever started the sidecar, and still takes it.
    #[test]
    fn taking_the_lock_on_a_network_file_system_warns() {
        #[derive(Clone, Default)]
        struct Logged(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Logged {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logged = Logged::default();
        let writer = logged.clone();
        let log = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let folder = tempfile::tempdir().unwrap();
        let taken = tracing::subscriber::with_default(log, || lock_on(folder.path(), String::new, |_| Some("NFS")));
        assert!(taken.is_ok());
        let said = String::from_utf8(logged.0.lock().unwrap().clone()).unwrap();
        assert!(said.contains("network file system (NFS)"), "{said:?}");
        // A local folder is not warned about.
        let quiet = Logged::default();
        let writer = quiet.clone();
        let log = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        let other = tempfile::tempdir().unwrap();
        tracing::subscriber::with_default(log, || lock_on(other.path(), String::new, |_| None)).unwrap();
        assert!(quiet.0.lock().unwrap().is_empty());
    }

    // The kinds of file system known to be reached over the network are told
    // by the number the system gives each kind, and anything else is taken
    // to be local.
    #[test]
    fn network_file_systems_are_told_apart_by_their_number() {
        assert_eq!(network_kind(0x6969, None), Some("NFS"));
        assert_eq!(network_kind(0xFF53_4D42, None), Some("SMB (CIFS)"));
        assert_eq!(network_kind(0xFE53_4D42, None), Some("SMB (CIFS)"));
        assert_eq!(network_kind(0x517B, None), Some("SMB (CIFS)"));
        assert_eq!(network_kind(0x0102_1997, None), Some("9P"));
        assert_eq!(network_kind(0x6573_5546, Some("fuse.sshfs")), Some("SSHFS"));
        assert_eq!(network_kind(0x6573_5546, Some("fuse.rclone")), Some("rclone"));
        // A FUSE file system can as well be a local disk.
        assert_eq!(network_kind(0x6573_5546, Some("fuse.ntfs-3g")), None);
        assert_eq!(network_kind(0x6573_5546, None), None);
        assert_eq!(network_kind(0xEF53, None), None); // ext4
        assert_eq!(network_kind(0x0102_1994, None), None); // tmpfs
    }

    // A data folder on a network file system is used, with a warning that
    // says which folder and why.
    #[test]
    fn a_data_folder_on_a_network_file_system_is_warned_about() {
        let folder = tempfile::tempdir().unwrap();
        let said = network_warning(folder.path(), |_| Some("NFS")).expect("no warning");
        assert!(said.contains(&folder.path().display().to_string()), "{said}");
        assert!(said.contains("NFS") && said.contains("local disk"), "{said}");
        assert_eq!(network_warning(folder.path(), |_| None), None);
        // A folder for tests is on a local disk.
        assert_eq!(network_warning(folder.path(), file_system_over_network), None);
        assert!(lock(folder.path(), String::new).is_ok());
    }

    // What a FUSE file system is listed as comes from the mount that holds
    // the folder.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_mount_that_holds_a_folder_is_found() {
        let folder = tempfile::tempdir().unwrap();
        assert!(fuse_kind(folder.path()).is_some_and(|kind| !kind.is_empty()));
        assert_eq!(fuse_kind(Path::new("/proc/self")).as_deref(), Some("proc"));
    }

    #[test]
    fn a_mac_is_named_by_its_hardware_id() {
        assert_eq!(hardware_computer(None), None);
        assert_eq!(hardware_computer(Some([0; 16])), None);
        let id = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0, 1, 2, 3, 4, 5, 6, 7];
        assert_eq!(
            hardware_computer(Some(id)).as_deref(),
            Some("hardware-id 123456789abcdef00001020304050607")
        );
    }

    #[test]
    fn a_computer_with_no_machine_id_is_not_named() {
        assert_eq!(computer_of(None), None);
        assert_eq!(computer_of(Some(" \n".into())), None);
        assert_eq!(computer_of(Some("abc123\n".into())).as_deref(), Some("machine-id abc123"));
    }

    #[test]
    fn a_start_command_can_be_typed_as_it_is() {
        let plain = Path::new("/no/such/place/orvu");
        assert_eq!(start_command(plain), "orvud --local --data-dir /no/such/place/orvu");
        let spaced = Path::new("/no/such/Application Support/it's");
        assert_eq!(start_command(spaced), "orvud --local --data-dir '/no/such/Application Support/it'\\''s'");
    }

    #[test]
    fn a_start_command_for_a_folder_whose_name_is_not_text_names_that_folder() {
        use std::os::unix::ffi::OsStrExt;
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"/no/such/caf\xe9 it's\\\x01"));
        assert_eq!(start_command(odd), r"orvud --local --data-dir $'/no/such/caf\xe9 it\'s\\\x01'");
    }

    #[test]
    fn a_random_machine_number_fits_in_every_language() {
        for _ in 0..10_000 {
            let n = random_replica().0;
            assert!((1..=LARGEST_REPLICA).contains(&n), "{n}");
            // It survives a round trip through a floating-point number.
            assert_eq!(n as f64 as u64, n);
        }
    }
}
