//! `orvud setup`: makes what each machine of a team needs to find the others
//! and to trust them. That is a settings file and a certificate per machine.
//!
//! One folder is made for each machine. It is copied to that machine, and the
//! sidecar is started there from the settings file inside it. Beside them,
//! in `authority`, the key that signed the certificates is kept, so that a
//! machine can be added later. Nothing here talks to the network or starts
//! anything, apart from looking the machines' names up.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The port a machine listens on for the others when none is given.
pub const DEFAULT_PORT: u16 = 7440;

/// The namespace every machine of a team made here serves. It is not `local`,
/// which is the one a sidecar with no setup serves.
pub const NAMESPACE: &str = "team";

/// The folder, beside the machines' folders, that keeps what signs their
/// certificates. No machine may have this name.
pub const AUTHORITY: &str = "authority";

/// One machine of the team, as given on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// What the machine is called. Its folder gets this name.
    pub name: String,
    /// Where the others reach it: a name or an address, without the port.
    pub host: String,
    pub port: u16,
    /// The machine's number, if one was given. A machine keeps its number
    /// for as long as it keeps its data.
    pub number: Option<u64>,
}

impl Machine {
    /// Reads `name=host` or `name=host:port`, with `#number` after it if the
    /// machine is to have that number. The host is a name, an IPv4 address,
    /// or an IPv6 address in square brackets.
    pub fn parse(text: &str) -> Result<Machine, String> {
        let wrong = |why: &str| {
            format!("{text:?} is not a machine: {why}. Write it as name=address, such as alpha=10.0.0.5")
        };
        let (name, address) = text.split_once('=').ok_or_else(|| wrong("there is no `=` in it"))?;
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        let name_ok = !name.is_empty()
            && name.len() <= 64
            && name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && name.chars().all(plain);
        if !name_ok {
            return Err(wrong("the name has to be letters, digits, `.`, `_` and `-`, and start with a letter or a digit"));
        }
        let (address, number) = match address.split_once('#') {
            None => (address, None),
            Some((address, given)) => match given.parse::<u64>() {
                Ok(number) if number > crate::local::LARGEST_REPLICA => {
                    return Err(wrong(&format!(
                        "the number can be at most {}, because a program may hold it as a \
                         floating-point number, which is exact only that far",
                        crate::local::LARGEST_REPLICA
                    )))
                }
                Ok(number) if number > 0 => (address, Some(number)),
                _ => return Err(wrong("the number has to be a whole number above 0, such as #3")),
            },
        };
        let (host, port) = if let Some(rest) = address.strip_prefix('[') {
            // An IPv6 address is full of colons, so it goes in brackets and
            // the port, if any, follows them.
            let (inside, after) = rest.split_once(']').ok_or_else(|| wrong("the `[` is never closed"))?;
            if inside.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(wrong("what is in the brackets is not an IPv6 address"));
            }
            let port = match after.strip_prefix(':') {
                Some(port) => Some(port),
                None if after.is_empty() => None,
                None => return Err(wrong("only a port can follow the brackets")),
            };
            (format!("[{inside}]"), port)
        } else {
            match address.split_once(':') {
                Some((_, rest)) if rest.contains(':') => {
                    return Err(wrong("an IPv6 address goes in square brackets, such as [fd00::5]"))
                }
                Some((host, port)) => (host.to_string(), Some(port)),
                None => (address.to_string(), None),
            }
        };
        if !host.starts_with('[') && (host.is_empty() || !host.chars().all(plain)) {
            return Err(wrong("the address has to be a machine's name or its IP address"));
        }
        let port = match port {
            None => DEFAULT_PORT,
            Some(given) => match given.parse::<u16>() {
                Ok(port) if port > 0 => port,
                _ => return Err(wrong("the port has to be a number from 1 to 65535")),
            },
        };
        Ok(Machine { name: name.to_string(), host, port, number })
    }

    /// Where the other machines reach this one, as it goes in their settings.
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The address this machine listens on: every network it is on, at its
    /// port.
    fn listen(&self) -> String {
        // A machine given by an IPv4 address is dialed over IPv4. Anything
        // else may be dialed over IPv6: a name can stand for either kind of
        // address, and which is not known here. `[::]` takes both kinds.
        let every = if self.host.parse::<std::net::Ipv4Addr>().is_ok() { "0.0.0.0" } else { "[::]" };
        format!("{every}:{}", self.port)
    }
}

/// The number of each machine, in the order the machines were given. A
/// machine that was given a number has it. The others get the lowest numbers
/// that are free, in order, so a team given with no numbers at all is
/// numbered from 1.
pub fn numbers(machines: &[Machine]) -> Result<Vec<u64>, String> {
    let mut taken = std::collections::BTreeMap::new();
    for machine in machines {
        if let Some(number) = machine.number {
            if let Some(other) = taken.insert(number, &machine.name) {
                return Err(format!("{other:?} and {:?} both have the number {number}", machine.name));
            }
        }
    }
    let mut free = (1..).filter(|number| !taken.contains_key(number));
    Ok(machines.iter().map(|machine| machine.number.unwrap_or_else(|| free.next().expect("numbers do not run out"))).collect())
}

/// Checks the machines as a team: enough of them, and no two alike.
pub fn check(machines: &[Machine]) -> Result<(), String> {
    numbers(machines)?;
    if machines.len() < 2 {
        return Err("a team needs at least two machines. One machine needs no setup: run `orvud --local`".into());
    }
    for (i, machine) in machines.iter().enumerate() {
        if machine.name.eq_ignore_ascii_case(AUTHORITY) {
            return Err(format!(
                "a machine cannot be called {:?}: the folder of that name keeps the key that signs the certificates",
                machine.name
            ));
        }
        for other in &machines[..i] {
            if other.name.eq_ignore_ascii_case(&machine.name) {
                return Err(format!("two machines are called {:?}", machine.name));
            }
            if other.address().eq_ignore_ascii_case(&machine.address()) {
                return Err(format!(
                    "{:?} and {:?} have the same address, {}",
                    other.name,
                    machine.name,
                    machine.address()
                ));
            }
        }
    }
    Ok(())
}

/// The settings file for the machine at `index`. The machines have to have
/// passed `check`.
pub fn settings(machines: &[Machine], index: usize) -> String {
    let me = &machines[index];
    let replica = numbers(machines).expect("the machines were checked")[index];
    format!(
        "# Settings for the machine {name:?}, made by `orvud setup`.\n\
         # Start the sidecar on that machine with:\n\
         #     orvud --config orvu.toml --socket orvud.sock\n\
         \n\
         # This machine's number. No two machines of a team may share one, and\n\
         # a machine keeps its number for as long as it keeps its data.\n\
         replica = {replica}\n\
         namespace = {namespace:?}\n\
         \n\
         # Where this machine listens for the others, and where it finds them.\n\
         listen = {listen:?}\n\
         {peers}\n\
         \n\
         # The files that let the machines trust each other. `node-key.pem` is\n\
         # this machine's own secret.\n\
         [security]\n\
         cert = \"node.pem\"\n\
         key = \"node-key.pem\"\n\
         ca = \"ca.pem\"\n\
         \n\
         # Where the data is kept, next to this file.\n\
         [storage]\n\
         path = \"data/orvu.redb\"\n\
         \n\
         # A budget is shared out between the machines once a second, so that\n\
         # each can spend from it without asking, and most of it can still be\n\
         # spent when one machine is away.\n\
         [budget]\n\
         rebalance_ms = 1000\n\
         \n\
         # The sidecar keeps no log and serves no page of numbers unless asked.\n\
         # To have a log, take the `# ` off the three lines of [logs] below.\n\
         # The level is one of error, warn, info, debug or trace. Leave `file`\n\
         # out to have the log on the screen.\n\
         # [logs]\n\
         # level = \"info\"\n\
         # file = \"orvud.log\"\n\
         #\n\
         # To have a page that Prometheus can collect, at /metrics, take the\n\
         # `# ` off the two lines of [metrics] below. Anyone who can reach the\n\
         # page's address can read it.\n\
         # [metrics]\n\
         # addr = \"127.0.0.1:9464\"\n",
        name = me.name,
        namespace = NAMESPACE,
        listen = me.listen(),
        peers = peers_line(machines, index),
    )
}

/// The `peers` line of the settings for the machine at `index`: every other
/// machine's address.
pub fn peers_line(machines: &[Machine], index: usize) -> String {
    format!("peers = {}", peers_value(machines, index))
}

/// The value of the `peers` line for the machine at `index`.
fn peers_value(machines: &[Machine], index: usize) -> String {
    let others: Vec<String> = machines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != index)
        .map(|(_, other)| format!("{:?}", other.address()))
        .collect();
    format!("[{}]", others.join(", "))
}

/// A systemd user unit that keeps `machine`'s sidecar running, and starts it
/// again if it stops with a failure. It expects the machine's folder at
/// `~/orvu/<name>` and the program at `~/.local/bin/orvud`; the file says
/// how to change that, and how to install it.
pub fn unit(machine: &Machine) -> String {
    let name = &machine.name;
    format!(
        "# Keeps the sidecar of the machine {name:?} running. Made by `orvud setup`.\n\
         #\n\
         # It expects this folder at ~/orvu/{name} and the orvud program at\n\
         # ~/.local/bin/orvud. If either is somewhere else, change the paths below\n\
         # (`command -v orvud` shows where the program is). Then, on that machine:\n\
         #\n\
         #     systemctl --user link ~/orvu/{name}/orvud-{name}.service\n\
         #     systemctl --user enable --now orvud-{name}.service\n\
         #\n\
         # To keep it running while nobody is logged in to the machine:\n\
         #\n\
         #     loginctl enable-linger\n\
         #\n\
         # `systemctl --user status orvud-{name}` shows how it is doing, and\n\
         # `journalctl --user -u orvud-{name}` what it said.\n\
         \n\
         [Unit]\n\
         Description=Orvu sidecar for the machine {name}\n\
         \n\
         [Service]\n\
         WorkingDirectory=%h/orvu/{name}\n\
         ExecStart=%h/.local/bin/orvud --config %h/orvu/{name}/orvu.toml --socket %h/orvu/{name}/orvud.sock\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// A warning for each machine given by a name that `resolves` cannot find
/// at its port. A machine given by its IP address is not looked up.
pub fn unresolved(machines: &[Machine], resolves: impl Fn(&str, u16) -> bool) -> Vec<String> {
    machines
        .iter()
        .filter(|machine| !machine.host.starts_with('[') && machine.host.parse::<std::net::Ipv4Addr>().is_err())
        .filter(|machine| !resolves(&machine.host, machine.port))
        .map(|machine| {
            format!(
                "warning: {:?}, the address of {}, cannot be looked up on this computer. The settings are made \
                 all the same: check the name, and that the other machines can find it",
                machine.host, machine.name
            )
        })
        .collect()
}

/// True if `host` can be looked up here, as the sidecar would look it up.
pub fn resolves(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    (host, port).to_socket_addrs().is_ok_and(|mut found| found.next().is_some())
}

/// What signs the machines' certificates: a certificate authority of the
/// team's own, made here.
struct Authority {
    /// The certificate as it was first written. It is what the machines
    /// trust, so it is what goes to a machine added later.
    cert_pem: String,
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

impl Authority {
    /// What the authority's certificate says. It is the same each time, so
    /// that a key read back signs certificates the machines already trust.
    fn params() -> rcgen::CertificateParams {
        let mut params = rcgen::CertificateParams::new(vec![]).expect("no names is always valid");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let mut name = rcgen::DistinguishedName::new();
        name.push(rcgen::DnType::CommonName, "orvu dev CA");
        params.distinguished_name = name;
        params
    }

    fn new() -> Authority {
        let key = rcgen::KeyPair::generate().expect("making a key cannot fail");
        let cert = Authority::params().self_signed(&key).expect("signing cannot fail");
        Authority { cert_pem: cert.pem(), cert, key }
    }

    /// The authority kept in `folder`.
    fn read(folder: &Path) -> Result<Authority, String> {
        let read = |name: &str| std::fs::read_to_string(folder.join(name));
        let (cert_pem, key_pem) = match (read("ca.pem"), read("ca-key.pem")) {
            (Ok(cert), Ok(key)) => (cert, key),
            (Err(e), _) | (_, Err(e)) => {
                return Err(format!(
                    "cannot read the key that signed this team's certificates, in {}: {e}. Settings made \
                     before `orvud setup` kept it cannot have a machine added: make them all again, giving \
                     each machine that stays its number",
                    folder.display()
                ))
            }
        };
        let key = rcgen::KeyPair::from_pem(&key_pem)
            .map_err(|e| format!("{} is not a key that can sign: {e}", folder.join("ca-key.pem").display()))?;
        let cert = Authority::params().self_signed(&key).expect("signing cannot fail");
        Ok(Authority { cert_pem, cert, key })
    }

    /// A certificate and key for the machine numbered `replica`, saying so
    /// in the form the sidecars check: `orvu://replica/<number>`.
    fn issue(&self, replica: u64) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(vec![]).expect("no names is always valid");
        let mut name = rcgen::DistinguishedName::new();
        name.push(rcgen::DnType::CommonName, "orvu-node");
        params.distinguished_name = name;
        params.subject_alt_names = vec![
            rcgen::SanType::DnsName("orvu-node".try_into().expect("a valid name")),
            rcgen::SanType::URI(
                orvu_transport::types::replica_identity_uri(replica).try_into().expect("a valid address"),
            ),
        ];
        let key = rcgen::KeyPair::generate().expect("making a key cannot fail");
        let cert = params.signed_by(&key, &self.cert, &self.key).expect("signing cannot fail");
        (cert.pem(), key.serialize_pem())
    }
}

/// Writes `text` to a new file at `path`, readable as `mode` says.
fn create(path: &Path, text: &str, mode: u32) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(|e| failed("write", path, e))
}

/// Fills `own` with the files of the machine at `index`: its settings, its
/// certificate and key, the authority's certificate, and its service file.
fn fill(own: &Path, machines: &[Machine], index: usize, number: u64, authority: &Authority) -> Result<(), String> {
    let machine = &machines[index];
    let (cert, key) = authority.issue(number);
    let files = [
        ("orvu.toml".to_string(), settings(machines, index), 0o644),
        ("ca.pem".to_string(), authority.cert_pem.clone(), 0o644),
        ("node.pem".to_string(), cert, 0o644),
        // The key is this machine's secret, so only its owner may read it.
        ("node-key.pem".to_string(), key, 0o600),
        (format!("orvud-{}.service", machine.name), unit(machine), 0o644),
    ];
    for (name, text, mode) in files {
        create(&own.join(name), &text, mode)?;
    }
    Ok(())
}

/// What the authority folder keeps of the team: the machines in it, and
/// the machines taken out of it before, whose numbers are never given out
/// again.
#[derive(Debug, Default)]
struct Kept {
    machines: Vec<Machine>,
    gone: Vec<Machine>,
}

/// How a machine taken out of the team starts its line in the list.
const GONE: &str = "gone ";

impl Kept {
    /// The list kept in `list`.
    fn read(list: &Path) -> Result<Kept, String> {
        let text = std::fs::read_to_string(list).map_err(|e| failed("read", list, e))?;
        let mut kept = Kept::default();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
            let (into, line) = match line.strip_prefix(GONE) {
                Some(rest) => (&mut kept.gone, rest),
                None => (&mut kept.machines, line),
            };
            let machine = Machine::parse(line).map_err(|e| format!("{} is not a list of machines: {e}", list.display()))?;
            if machine.number.is_none() {
                return Err(format!("{} is not a list of machines: {line:?} has no number", list.display()));
            }
            into.push(machine);
        }
        Ok(kept)
    }

    /// The machine taken out earlier that had `number`, if there was one.
    fn gone_with(&self, number: u64) -> Option<&Machine> {
        self.gone.iter().find(|machine| machine.number == Some(number))
    }

    /// The highest number ever given out in this team, or 0.
    fn highest(&self) -> u64 {
        self.machines.iter().chain(&self.gone).filter_map(|machine| machine.number).max().unwrap_or(0)
    }
}

/// Refuses `machine` if it was given the number of a machine taken out.
fn not_gone(kept: &Kept, machine: &Machine) -> Result<(), String> {
    match machine.number.and_then(|number| kept.gone_with(number)) {
        Some(gone) => Err(format!(
            "{:?} cannot have the number {}: it was {:?}'s, which was taken out of the team. The others keep \
             that machine's data under it, and a number is never given out twice. Leave the number out to \
             have a new one",
            machine.name,
            gone.number.expect("listed with its number"),
            gone.name
        )),
        None => Ok(()),
    }
}

/// The list of the team's machines, one a line, each with its number, as
/// the authority folder keeps it, followed by the machines taken out.
fn listed(machines: &[Machine], gone: &[Machine]) -> String {
    let numbered = numbers(machines).expect("the machines were checked");
    let mut text = String::from("# The machines of this team, made by `orvud setup`. Leave this file as it is.\n");
    for (machine, number) in machines.iter().zip(numbered) {
        text.push_str(&format!("{}={}#{number}\n", machine.name, machine.address()));
    }
    if !gone.is_empty() {
        text.push_str("# Machines taken out. Their numbers are never given out again.\n");
    }
    for machine in gone {
        text.push_str(&format!("{GONE}{}={}#{}\n", machine.name, machine.address(), machine.number.expect("numbered")));
    }
    text
}

/// Waits until this program is the only one changing the team kept in
/// `kept`, and keeps it so until what is given back is dropped. Two
/// machines added at once would otherwise both read the list, both take the
/// same next number, and one would drop out of the list.
fn hold(kept: &Path) -> Result<std::fs::File, String> {
    let path = kept.join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| failed("open", &path, e))?;
    file.lock().map_err(|e| failed("lock", &path, e))?;
    Ok(file)
}

/// Makes `folder`, and in it one folder per machine holding that machine's
/// settings file, its certificate and key, the certificate of the authority
/// that signed them all, and a service file to keep its sidecar running.
///
/// The authority's key is kept in `authority`, beside them, where only its
/// owner can read it, with the list of the machines. It is what lets a
/// machine be added later. It is never copied to a machine.
pub fn write(folder: &Path, machines: &[Machine]) -> Result<(), String> {
    write_team(folder, machines, &[]).map_err(Unmade::why)
}

/// Makes the settings for a team again, in `folder`, as `write` does, for a
/// team whose settings were made earlier in `earlier`: to take machines out
/// of it at once, with a new key the machines taken out do not have.
///
/// The machines that stay are given their numbers. The numbers of the
/// machines left out are kept in the new list, as those of machines taken
/// out, with those taken out before, so that no number is ever given out
/// twice: neither here, to a machine given no number or the number of one
/// taken out, nor later by `add`. Gives back the team with every machine's
/// number.
///
/// `earlier` is then marked as made again here, and refuses to be added to
/// or made again from, so that only one list gives out numbers.
pub fn write_after(folder: &Path, machines: &[Machine], earlier: &Path) -> Result<Vec<Machine>, String> {
    write_after_by(folder, machines, earlier, write_team)
}

/// `write_after`, with `make` making the new team in `folder` from the
/// machines in it and those taken out. The tests use it to stop a run
/// partway, as a crash would.
fn write_after_by(
    folder: &Path,
    machines: &[Machine],
    earlier: &Path,
    make: impl FnOnce(&Path, &[Machine], &[Machine]) -> Result<(), Unmade>,
) -> Result<Vec<Machine>, String> {
    check(machines)?;
    let new_place = std::path::absolute(folder).map_err(|e| failed("find", folder, e))?;
    // The mark below keeps the folder on one line of its own.
    if new_place.as_os_str().as_bytes().contains(&b'\n') {
        return Err(format!(
            "{:?} has a line break in it. Give a folder whose name has none",
            new_place.as_os_str()
        ));
    }
    let before = folder_kept(earlier)?;
    // The earlier folder stays held until the new team is made, so that
    // nothing adds a machine to it meanwhile.
    let _held = hold(&before)?;
    not_replaced(&before)?;
    let kept = Kept::read(&before.join("machines"))?;
    for machine in machines {
        not_gone(&kept, machine)?;
    }
    // A machine with no number takes the lowest number nobody has had.
    let mut used: std::collections::BTreeSet<u64> =
        kept.machines.iter().chain(&kept.gone).chain(machines).filter_map(|machine| machine.number).collect();
    let mut team = machines.to_vec();
    for machine in team.iter_mut().filter(|machine| machine.number.is_none()) {
        let number = (1..).find(|number| !used.contains(number)).expect("numbers do not run out");
        if number > crate::local::LARGEST_REPLICA {
            return Err("every number is taken: give the machine one with #".into());
        }
        used.insert(number);
        machine.number = Some(number);
    }
    let staying: std::collections::BTreeSet<u64> = team.iter().filter_map(|machine| machine.number).collect();
    let mut gone = kept.gone;
    gone.extend(kept.machines.into_iter().filter(|machine| !machine.number.is_some_and(|n| staying.contains(&n))));
    gone.sort_by_key(|machine| machine.number);
    // Something already in the way is refused before the mark is written,
    // so that it never holds the earlier folder back.
    if !nothing_made_at(&new_place).map_err(|e| failed("look at", &new_place, e))? {
        return Err(format!(
            "{} is not empty. Give a folder that is empty or does not exist yet",
            folder.display()
        ));
    }
    // The earlier folder is marked as being made again before the new team
    // is made. From then on its list gives out no numbers: the new one does.
    // Once the new team is finished, the mark says so. If making it fails
    // before the team was put in place, the mark is taken away again. If it
    // fails later, or it cannot be told when, the mark is taken away only if
    // nothing was put where it points, since the new team may be in place
    // even though a later step failed.
    // If this run is stopped before the new folder is there, the mark is
    // left saying it was being made, and the next run to use the earlier
    // folder finds nothing where it points and removes it.
    let marker = before.join(REPLACED);
    mark(&marker, PENDING, &new_place)?;
    match make(folder, &team, &gone) {
        Ok(()) => {}
        // Something else may be in the new folder now, another run's team or
        // the person's own files, but none of it is this run's.
        Err(Unmade::NothingPlaced(why)) => {
            let ours = std::fs::read(&marker).is_ok_and(|text| text == marked(PENDING, &new_place));
            if ours && std::fs::remove_file(&marker).is_ok() {
                return Err(format!(
                    "{why}. Nothing was made, and {} is used as before",
                    earlier.display()
                ));
            }
            return Err(format!(
                "{why}. Nothing was made, but {} could not be taken away, so {} is held back. If you \
                 know the team was not made again anywhere, remove that file, and {} is used as before",
                marker.display(),
                earlier.display(),
                earlier.display()
            ));
        }
        Err(Unmade::MaybePlaced(why)) => {
            if matches!(nothing_made_at(&new_place), Ok(true)) {
                let _ = std::fs::remove_file(&marker);
                return Err(why);
            }
            return Err(format!(
                "{why}. The new team may be complete in {new}, so {old} is held back. Another run, or \
                 someone else, may have put something in {new} too, so first make sure what is there was \
                 made from {old} by this run, and leave it alone if it was not. If it was, and it holds \
                 the finished team, use it from now on. If it was, but it is not finished, remove it. \
                 Unless you use it, remove {}, and {old} is used as before",
                marker.display(),
                new = new_place.display(),
                old = earlier.display()
            ));
        }
    }
    mark(&marker, FINISHED, &new_place).map_err(|why| {
        format!(
            "the team was made in {}, but {why}. Until that file says the team was finished, {} is refused",
            new_place.display(),
            earlier.display()
        )
    })?;
    Ok(team)
}

/// The file in an authority folder naming the folder its team was made
/// again in, and whether that was finished.
const REPLACED: &str = "replaced";

/// How the mark starts while the new folder is being made.
const PENDING: &str = "pending ";

/// How the mark starts once the new folder was made to the end.
const FINISHED: &str = "finished ";

/// What a mark holds: `state`, then the folder `place` byte for byte, then
/// one line break.
fn marked(state: &str, place: &Path) -> Vec<u8> {
    [state.as_bytes(), place.as_os_str().as_bytes(), b"\n"].concat()
}

/// Writes the mark at `marker`, as `marked` makes it. It is written beside
/// and put in place whole, and kept on the disk before this returns, so that
/// a crash leaves either the old mark or the new one.
fn mark(marker: &Path, state: &str, place: &Path) -> Result<(), String> {
    let fresh = marker.with_file_name(".replaced.new");
    let _ = std::fs::remove_file(&fresh);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&fresh)
        .and_then(|mut file| {
            file.write_all(&marked(state, place))?;
            file.sync_all()
        })
        .map_err(|e| failed("write", &fresh, e))?;
    std::fs::rename(&fresh, marker).map_err(|e| failed("write", marker, e))?;
    let above = marker.parent().expect("the mark is in the authority folder");
    std::fs::File::open(above).and_then(|dir| dir.sync_all()).map_err(|e| failed("write", above, e))
}

/// Refuses the team kept in `kept` if it was made again in another folder.
/// Its list is then no longer the one numbers are taken from: a number
/// taken from it could be one the new team has already given out.
///
/// A mark saying the new team was finished always refuses, even if that
/// folder has since moved, as is a mark written by an earlier version of
/// this program, which held the folder alone. A mark saying it was still
/// being made is left by a run that was stopped before it finished. If
/// nothing, or only an empty folder, is where it points, that run never
/// made the new folder:
/// the mark is removed, saying so, and the team is used as before. If
/// something is there, or that cannot be told, or the folder it names
/// cannot be read from it for sure, it is refused, since the new team may
/// have been made after all. Only call this while holding the team with
/// `hold`.
fn not_replaced(kept: &Path) -> Result<(), String> {
    let marker = kept.join(REPLACED);
    let text = match std::fs::read(&marker) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(failed("read", &marker, e)),
    };
    let Some(pending) = text.strip_prefix(PENDING.as_bytes()) else {
        let named = text.strip_prefix(FINISHED.as_bytes()).unwrap_or(&text);
        let named = named.strip_suffix(b"\n").unwrap_or(named);
        let place = Path::new(std::ffi::OsStr::from_bytes(named));
        let missing = place.is_absolute()
            && matches!(std::fs::symlink_metadata(place), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        if missing {
            return Err(format!(
                "this team was made again in {place}, and its machines are listed there, but nothing is \
                 there now. It may have been moved, or it may have been lost, if the computer stopped \
                 before it was all kept on the disk. If it was moved, use it where it is now: add machines \
                 to it, and make the team again from it. If you are sure it is lost, and that no machine \
                 was given a number from it, remove {} and this folder is used as before",
                marker.display(),
                place = place.display()
            ));
        }
        return Err(format!(
            "this team was made again in {}, and its machines are listed there now. Add machines to \
             that folder, and make the team again from it. If it was moved since, use it where it is now",
            place.display()
        ));
    };
    // The folder is written byte for byte and ends the mark with one line
    // break. Anything else was not written by a finished `mark`, so which
    // folder it means cannot be known for sure.
    let place = pending
        .strip_suffix(b"\n")
        .filter(|named| !named.contains(&b'\n'))
        .map(|named| Path::new(std::ffi::OsStr::from_bytes(named)))
        .filter(|place| place.is_absolute())
        .ok_or_else(|| {
            format!(
                "this team may have been made again in another folder, but which one cannot be read from {}. \
                 If you know the team was not made again anywhere, remove that file, and this folder is \
                 used as before",
                marker.display()
            )
        })?;
    match nothing_made_at(place) {
        Ok(true) => {}
        found => {
            let seen = match found {
                Ok(_) => "something is there".to_string(),
                Err(e) => format!("it cannot be looked at: {e}"),
            };
            return Err(format!(
                "this team was being made again in {place}, but the run making it may have been \
                 interrupted, and {seen}. Another run, or someone else, may have put something there too, \
                 so first make sure what is there was made from this folder, and leave it alone if it was \
                 not. If it was, and it holds the finished team, use it from now on, and add machines to \
                 it rather than to this one. If it was, but it is not finished, remove it. Unless you use \
                 it, remove {}, and this folder is used as before",
                marker.display(),
                place = place.display()
            ));
        }
    }
    std::fs::remove_file(&marker).map_err(|e| failed("remove", &marker, e))?;
    crate::say::line(format_args!(
        "orvud: {} said this team was being made again in {}, but nothing was made there: the run making \
         it stopped before it made it. The mark is removed, and this folder is used as before",
        marker.display(),
        place.display()
    ));
    Ok(())
}

/// Whether a run making a team could not have put anything at `place`:
/// nothing is there, or only an empty folder, which the person may have
/// made before the run. A run only ever puts a whole folder there, never an
/// empty one. A link, even to an empty folder, counts as something.
fn nothing_made_at(place: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(place) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
        Ok(found) if found.is_dir() => Ok(std::fs::read_dir(place)?.next().is_none()),
        Ok(_) => Ok(false),
    }
}

/// The authority folder of the team made in `earlier`.
fn folder_kept(earlier: &Path) -> Result<std::path::PathBuf, String> {
    let kept = earlier.join(AUTHORITY);
    if !kept.join("machines").is_file() {
        return Err(format!(
            "{} has no list of the machines made earlier, so the numbers they had are not known. Give the \
             folder the earlier settings were made in",
            kept.display()
        ));
    }
    Ok(kept)
}

fn write_team(folder: &Path, machines: &[Machine], gone: &[Machine]) -> Result<(), Unmade> {
    write_team_by(folder, machines, gone, &doing)
}

/// `write_team`, with `steps` taking each step that puts the team in place,
/// as `staged_by` does.
fn write_team_by(folder: &Path, machines: &[Machine], gone: &[Machine], steps: Steps) -> Result<(), Unmade> {
    check(machines).map_err(Unmade::NothingPlaced)?;
    let numbered = numbers(machines).map_err(Unmade::NothingPlaced)?;
    staged_by(folder, steps, |staging| {
        let authority = Authority::new();
        for (index, machine) in machines.iter().enumerate() {
            let own = staging.join(&machine.name);
            orvu_sdk::make_private_folder(&own).map_err(|e| failed("make the folder", &own, e))?;
            fill(&own, machines, index, numbered[index], &authority)?;
        }
        let kept = staging.join(AUTHORITY);
        orvu_sdk::make_private_folder(&kept).map_err(|e| failed("make the folder", &kept, e))?;
        create(&kept.join("ca.pem"), &authority.cert_pem, 0o644)?;
        create(&kept.join("ca-key.pem"), &authority.key.serialize_pem(), 0o600)?;
        create(&kept.join("lock"), "", 0o600)?;
        create(&kept.join("machines"), &listed(machines, gone), 0o644)
    })
}

/// What adding a machine made.
#[derive(Debug)]
pub struct Added {
    /// The new machine's number.
    pub number: u64,
    /// The whole team, the new machine last.
    pub machines: Vec<Machine>,
    /// The machines whose settings in the folder could not have their peers
    /// changed, because the file is gone or has no `peers` line, and why.
    pub not_changed: Vec<(String, String)>,
}

/// Adds `machine` to the team whose settings were made in `folder`: makes
/// its folder, with a certificate signed by the key kept in `authority`,
/// adds it to the list there, and to the `peers` line of the others'
/// settings in `folder`.
///
/// A machine given no number gets one above every number ever given out in
/// the team, those of machines taken out included, so that it never takes
/// the number of another. A machine given the number of one taken out is
/// refused. Only one `add` runs at a time on a team: another waits. A team
/// made again in another folder is refused, naming that folder.
pub fn add(folder: &Path, mut machine: Machine) -> Result<Added, String> {
    let kept = folder.join(AUTHORITY);
    let authority = Authority::read(&kept)?;
    let _held = hold(&kept)?;
    not_replaced(&kept)?;
    let list = kept.join("machines");
    let kept_list = Kept::read(&list)?;
    if machine.number.is_none() {
        let highest = kept_list.highest();
        if highest >= crate::local::LARGEST_REPLICA {
            return Err("every number is taken: give the machine one with #".into());
        }
        machine.number = Some(highest + 1);
    }
    not_gone(&kept_list, &machine)?;
    let Kept { mut machines, gone } = kept_list;
    machines.push(machine);
    check(&machines)?;
    let index = machines.len() - 1;
    let new = &machines[index];
    let number = new.number.expect("given above");
    staged(&folder.join(&new.name), |own| fill(own, &machines, index, number, &authority))?;

    // The list is changed next, by putting a new file in place of the old
    // one, so that it is never left half written.
    let fresh = kept.join(".machines.new");
    let _ = std::fs::remove_file(&fresh);
    create(&fresh, &listed(&machines, &gone), 0o644)?;
    std::fs::rename(&fresh, &list).map_err(|e| failed("replace", &list, e))?;

    let mut not_changed = Vec::new();
    for (other, known) in machines[..index].iter().enumerate() {
        let path = folder.join(&known.name).join("orvu.toml");
        if let Err(why) = new_peers(&path, &peers_value(&machines, other)) {
            not_changed.push((known.name.clone(), why));
        }
    }
    Ok(Added { number, machines, not_changed })
}

/// Puts `value` in place of the value of `peers` in the settings at `path`,
/// leaving the rest as it is. The old value may be spread over several
/// lines: the settings are read as the sidecar reads them, to find where it
/// starts and ends.
fn new_peers(path: &Path, value: &str) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    struct Peers {
        peers: Option<toml::Spanned<toml::Value>>,
    }
    let text = std::fs::read_to_string(path).map_err(|e| failed("read", path, e))?;
    let read: Peers = toml::from_str(&text).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
    let Some(old) = read.peers else {
        return Err(format!("{} has no peers line", path.display()));
    };
    let span = old.span();
    let changed = format!("{}{value}{}", &text[..span.start], &text[span.end..]);
    let fresh = path.with_extension("toml.new");
    let _ = std::fs::remove_file(&fresh);
    create(&fresh, &changed, 0o644)?;
    std::fs::rename(&fresh, path).map_err(|e| failed("replace", path, e))
}

/// What to do once a machine was added, for the person who ran the command.
pub fn added_steps(folder: &Path, added: &Added) -> String {
    let machines = &added.machines;
    let new = machines.last().expect("the new machine is in the list");
    let mut text = format!(
        "Added {} as machine {}, reached at {}. Its settings are in {}.\n\n\
         Copy that folder to the machine and start its sidecar there, as for the\n\
         others. It holds that machine's secret key, so send it nowhere else. The\n\
         machines that are running let it in and need not be stopped: it reaches\n\
         them. So that they also reach it after they next start, their settings in\n\
         {} now list it, and on each machine the peers line of orvu.toml should\n\
         read:\n\n",
        new.name,
        added.number,
        new.address(),
        folder.join(&new.name).display(),
        folder.display(),
    );
    for (index, machine) in machines[..machines.len() - 1].iter().enumerate() {
        text.push_str(&format!("  {}: {}\n", machine.name, peers_line(machines, index)));
    }
    for (name, why) in &added.not_changed {
        text.push_str(&format!("\nThe settings of {name} in the folder were not changed: {why}.\n"));
    }
    text.push_str(&format!("\nThe machines reach each other on UDP port {}. Open it between them.", ports(machines)));
    text
}

fn failed(what: &str, path: &Path, e: std::io::Error) -> String {
    format!("cannot {what} {}: {e}", path.display())
}

/// Lets `fill` make the contents of `folder` in a folder beside it, and puts
/// that in place only when `fill` has finished. If `fill` fails, what it made
/// is removed, so that half a team's keys are never left lying about and the
/// next try is not stopped by them.
///
/// `folder` has to be empty or not there yet.
fn staged(folder: &Path, fill: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), String> {
    staged_telling(folder, fill).map_err(Unmade::why)
}

/// Why a folder could not be made, and whether it may be in place even so.
#[derive(Debug)]
enum Unmade {
    /// It failed before the folder was put in place: nothing was put there.
    NothingPlaced(String),
    /// It failed once the folder was put in place, or it cannot be told
    /// when, so the folder may be there and complete.
    MaybePlaced(String),
}

impl Unmade {
    fn why(self) -> String {
        match self {
            Unmade::NothingPlaced(why) | Unmade::MaybePlaced(why) => why,
        }
    }
}

/// `staged`, telling whether the folder was put in place when it fails.
fn staged_telling(folder: &Path, fill: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), Unmade> {
    staged_by(folder, &doing, fill)
}

/// A step that puts a made folder in place, once what goes in it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Keeping what was made beside on the disk.
    Keep,
    /// Taking away the empty folder already where it goes.
    ClearEmpty,
    /// Moving it into place.
    Rename,
    /// Keeping the folder above on the disk once it is in place.
    KeepAbove,
}

/// Takes a step: given the step and what doing it means, does it. Only the
/// tests take steps any other way, to make one fail, or seem to.
type Steps<'a> = &'a dyn Fn(Step, &mut dyn FnMut() -> std::io::Result<()>) -> std::io::Result<()>;

/// Takes each step as it is.
fn doing(_: Step, step: &mut dyn FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    step()
}

/// `staged_telling`, with `steps` taking each step that puts the folder in
/// place.
fn staged_by(folder: &Path, steps: Steps, fill: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), Unmade> {
    let before = Unmade::NothingPlaced;
    match std::fs::read_dir(folder) {
        // Never written into a folder that has something in it: an earlier
        // team's keys would be replaced, or mixed with the new ones.
        Ok(mut inside) => {
            if inside.next().is_some() {
                return Err(before(format!(
                    "{} is not empty. Give a folder that is empty or does not exist yet",
                    folder.display()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(before(failed("look into", folder, e))),
    }
    let name =
        folder.file_name().ok_or_else(|| before(format!("{} is not a folder to make", folder.display())))?;
    let mut staging_name = std::ffi::OsString::from(".");
    staging_name.push(name);
    staging_name.push(format!(".being-made-{}", std::process::id()));
    let staging = folder.with_file_name(staging_name);
    if let Some(above) = folder.parent().filter(|above| !above.as_os_str().is_empty()) {
        std::fs::create_dir_all(above).map_err(|e| before(failed("make the folder", above, e)))?;
    }
    orvu_sdk::make_private_folder(&staging).map_err(|e| before(failed("make the folder", &staging, e)))?;
    let made_aside = std::fs::symlink_metadata(&staging).map_err(|e| before(failed("look at", &staging, e)))?;
    let made = fill(&staging).map_err(before).and_then(|()| {
        // Everything made is kept on the disk before it is put in place, and
        // the folder it is put in afterwards, so that whatever is written
        // after this returns is never kept while the folder is lost.
        steps(Step::Keep, &mut || on_disk(&staging).map_err(std::io::Error::other))
            .map_err(|e| before(e.to_string()))?;
        // An empty folder that was already there is replaced by the new one.
        match steps(Step::ClearEmpty, &mut || std::fs::remove_dir(folder)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(before(failed("replace", folder, e)));
            }
            _ => {}
        }
        // The folder is only in place once it is done. Whatever fails after
        // that may leave it there.
        steps(Step::Rename, &mut || std::fs::rename(&staging, folder))
            .map_err(|e| failed("make the folder", folder, e))
            .map_err(|why| match still_aside(&staging, &made_aside) {
                true => before(why),
                false => Unmade::MaybePlaced(why),
            })?;
        let above = folder.parent().filter(|above| !above.as_os_str().is_empty()).unwrap_or(Path::new("."));
        steps(Step::KeepAbove, &mut || std::fs::File::open(above).and_then(|dir| dir.sync_all()))
            .map_err(|e| Unmade::MaybePlaced(failed("write", above, e)))
    });
    if made.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    made
}

/// Whether the folder made beside, found as `made` when it was made, is
/// still there, so that a rename that failed did not move it. A rename can
/// fail and have moved it even so, as on a network disk that tries it again
/// after the first try worked. Then the folder is gone from beside, and may
/// be in place. Only the same folder counts: another one made there since
/// does not.
fn still_aside(staging: &Path, made: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(staging)
        .is_ok_and(|now| now.is_dir() && now.dev() == made.dev() && now.ino() == made.ino())
}

/// Keeps `path` on the disk: every file and folder in it, and then the
/// folder itself, so that none of them can be lost if the computer stops.
fn on_disk(path: &Path) -> Result<(), String> {
    let kind = std::fs::symlink_metadata(path).map_err(|e| failed("look at", path, e))?.file_type();
    if kind.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|e| failed("look into", path, e))? {
            on_disk(&entry.map_err(|e| failed("look into", path, e))?.path())?;
        }
    } else if !kind.is_file() {
        return Ok(());
    }
    std::fs::File::open(path).and_then(|opened| opened.sync_all()).map_err(|e| failed("write", path, e))
}

/// What to do with the folders, for the person who ran the command.
pub fn next_steps(folder: &Path, machines: &[Machine]) -> String {
    let widest = machines.iter().map(|machine| machine.name.len()).max().unwrap_or(0);
    let mut text = format!("Made the settings for {} machines in {}:\n\n", machines.len(), folder.display());
    let numbered = numbers(machines).expect("the machines were checked");
    for (machine, number) in machines.iter().zip(numbered) {
        // The folder's name ends with the machine's, so padding the name
        // lines the rest up too.
        text.push_str(&format!(
            "  {}{:pad$}  machine {number}  reached at {}\n",
            folder.join(&machine.name).display(),
            "",
            machine.address(),
            pad = widest - machine.name.len(),
        ));
    }
    text.push_str(&format!(
        "\nCopy each folder to its machine, as ~/orvu/<name>. Each holds that\n\
         machine's secret key, so send it nowhere else. Then start the sidecar on\n\
         each machine, from inside its folder:\n\n  \
         orvud --config orvu.toml --socket orvud.sock\n\n\
         or have systemd keep it running: orvud-<name>.service in each folder says\n\
         how.\n\n\
         The machines reach each other on UDP port {}. Open it between them.\n\n\
         The key that signs the certificates is kept in\n\n  \
         {}\n\n\
         Keep it there, and send it nowhere: anyone with it can make a machine the\n\
         others trust. With it, a machine can be added later, and the others need\n\
         not be stopped:\n\n  \
         orvud setup {} --add <name>=<address>\n\n\
         A machine keeps its number. If you make these settings again, to take a\n\
         machine out at once, make them in a new folder from this one, and give\n\
         each machine that stays its number:\n\n  \
         {}",
        ports(machines),
        folder.join(AUTHORITY).display(),
        folder.display(),
        again(folder, machines),
    ));
    text
}

/// The command that makes the same team again in a new folder, keeping the
/// numbers given out in `folder`, with every machine's number written out.
fn again(folder: &Path, machines: &[Machine]) -> String {
    let numbered = numbers(machines).expect("the machines were checked");
    let mut command = format!("orvud setup <new-folder> --from {}", folder.display());
    for (machine, number) in machines.iter().zip(numbered) {
        command.push_str(&format!(" {}={}#{number}", machine.name, written(machine)));
    }
    command
}

/// A machine's address the way it was given: without the port if it is the
/// usual one.
fn written(machine: &Machine) -> String {
    if machine.port == DEFAULT_PORT {
        machine.host.clone()
    } else {
        machine.address()
    }
}

fn ports(machines: &[Machine]) -> String {
    let mut ports: Vec<u16> = machines.iter().map(|machine| machine.port).collect();
    ports.sort_unstable();
    ports.dedup();
    let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
    ports.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(text: &str) -> Machine {
        Machine::parse(text).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_machine_is_a_name_and_an_address() {
        assert_eq!(
            machine("alpha=10.0.0.5"),
            Machine { name: "alpha".into(), host: "10.0.0.5".into(), port: 7440, number: None }
        );
        assert_eq!(machine("b-2=box.example.com:9000").address(), "box.example.com:9000");
        assert_eq!(machine("c=[fd00::5]").address(), "[fd00::5]:7440");
        assert_eq!(machine("c=[fd00::5]:9000").address(), "[fd00::5]:9000");
        assert_eq!(machine("c=[fd00::5]:9000").listen(), "[::]:9000");
        assert_eq!(machine("alpha=10.0.0.5").listen(), "0.0.0.0:7440");
    }

    // A name can stand for an IPv4 address, an IPv6 one, or both, and which
    // is not known here. `[::]` takes connections of both kinds.
    #[test]
    fn a_machine_given_by_name_listens_for_both_kinds_of_address() {
        assert_eq!(machine("d=localhost").listen(), "[::]:7440");
        assert_eq!(machine("d=box3.example.com:9000").listen(), "[::]:9000");
    }

    // A machine keeps its number for as long as it keeps its data. So when a
    // machine is taken out of the team, the ones after it must not move up.
    #[test]
    fn a_machine_can_be_given_its_number() {
        assert_eq!(machine("gamma=10.0.0.7#3").number, Some(3));
        assert_eq!(machine("gamma=box:9000#12").address(), "box:9000");
        assert_eq!(machine("gamma=[fd00::5]:9000#12").number, Some(12));
        let team = |texts: &[&str]| numbers(&texts.iter().map(|text| machine(text)).collect::<Vec<_>>());
        assert_eq!(team(&["a=h1", "b=h2", "c=h3"]).unwrap(), [1, 2, 3]);
        // Beta is gone. Gamma is still machine 3.
        assert_eq!(team(&["a=h1", "c=h3#3"]).unwrap(), [1, 3]);
        // The ones with no number get the lowest that are free, in order.
        assert_eq!(team(&["a=h1#2", "b=h2", "c=h3", "d=h4#3"]).unwrap(), [2, 1, 4, 3]);
        assert!(team(&["a=h1#3", "b=h2#3"]).unwrap_err().contains("both have the number 3"));
        for wrong in ["a=h1#0", "a=h1#", "a=h1#three", "a=h1#-1"] {
            let message = Machine::parse(wrong).expect_err(wrong);
            assert!(message.contains("the number has to be"), "{wrong:?} gave {message:?}");
        }
        // A client may hold the number as a floating-point value, which is
        // exact only up to 2^53 - 1. Past that it would name another machine.
        assert_eq!(machine("a=h1#9007199254740991").number, Some(9_007_199_254_740_991));
        for too_big in ["a=h1#9007199254740992", "a=h1#9223372036854775808", "a=h1#18446744073709551615"] {
            let message = Machine::parse(too_big).expect_err(too_big);
            assert!(message.contains("at most 9007199254740991"), "{too_big:?} gave {message:?}");
        }

        let machines = [machine("alpha=10.0.0.5"), machine("gamma=10.0.0.7#3")];
        let gamma = orvu_sdk::SdkConfig::parse_toml(&settings(&machines, 1)).unwrap();
        assert_eq!(gamma.replica, 3);
        assert!(next_steps(Path::new("crew"), &machines).contains("crew/gamma  machine 3  reached at"));
    }

    // If the files cannot all be written, none are left behind. A folder
    // with half a team's keys in it would also stop the next try.
    #[test]
    fn a_setup_that_fails_partway_leaves_nothing_behind() {
        let top = std::env::temp_dir().join(format!("orvud-setup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(&top).unwrap();
        let folder = top.join("crew");
        let failed = staged(&folder, |staging| {
            std::fs::create_dir(staging.join("alpha")).unwrap();
            std::fs::write(staging.join("alpha/node-key.pem"), "secret").unwrap();
            Err("the disk is full".to_string())
        });
        assert_eq!(failed.unwrap_err(), "the disk is full");
        assert_eq!(std::fs::read_dir(&top).unwrap().count(), 0, "something was left behind");

        staged(&folder, |staging| std::fs::write(staging.join("made"), "x").map_err(|e| e.to_string())).unwrap();
        assert!(folder.join("made").exists());
        assert_eq!(std::fs::read_dir(&top).unwrap().count(), 1, "the folder it was made in is still there");
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn a_machine_written_wrongly_is_explained() {
        for (text, says) in [
            ("alpha", "no `=`"),
            ("=10.0.0.5", "the name has to be"),
            ("my box=10.0.0.5", "the name has to be"),
            ("-a=10.0.0.5", "the name has to be"),
            ("alpha=", "a machine's name or its IP address"),
            ("alpha=10.0.0.5:", "the port has to be"),
            ("alpha=10.0.0.5:0", "the port has to be"),
            ("alpha=10.0.0.5:70000", "the port has to be"),
            ("alpha=fd00::5", "square brackets"),
            ("alpha=[fd00::5", "never closed"),
            ("alpha=[10.0.0.5]", "not an IPv6 address"),
            ("alpha=[fd00::5]9000", "only a port"),
            ("alpha=ten \"quoted\"", "a machine's name or its IP address"),
        ] {
            let message = Machine::parse(text).expect_err(text);
            assert!(message.contains(says), "{text:?} gave {message:?}");
            assert!(message.contains("name=address"), "{text:?} did not show the form");
        }
    }

    #[test]
    fn a_team_needs_two_machines_that_differ() {
        let team = |texts: &[&str]| check(&texts.iter().map(|text| machine(text)).collect::<Vec<_>>());
        assert!(team(&["a=10.0.0.5", "b=10.0.0.6"]).is_ok());
        // One computer can hold several machines, on different ports.
        assert!(team(&["a=127.0.0.1:7001", "b=127.0.0.1:7002"]).is_ok());
        assert!(team(&["a=10.0.0.5"]).unwrap_err().contains("at least two"));
        assert!(team(&[]).unwrap_err().contains("at least two"));
        assert!(team(&["a=10.0.0.5", "A=10.0.0.6"]).unwrap_err().contains("two machines are called"));
        assert!(team(&["a=box", "b=BOX"]).unwrap_err().contains("the same address"));
    }

    // The file is read back with the reader the sidecar uses, so a setting it
    // would refuse cannot be written here.
    #[test]
    fn the_settings_are_ones_the_sidecar_reads() {
        let machines = [machine("alpha=10.0.0.5"), machine("beta=box.example.com:9000"), machine("c=[fd00::5]")];
        let read = |index| {
            orvu_sdk::SdkConfig::parse_toml(&settings(&machines, index)).expect("the sidecar reads it")
        };
        let alpha = read(0);
        assert_eq!((alpha.replica, alpha.namespace.as_str()), (1, "team"));
        assert_eq!(alpha.listen, "0.0.0.0:7440");
        assert_eq!(alpha.peers, ["box.example.com:9000", "[fd00::5]:7440"]);
        assert_eq!(alpha.storage.path.as_deref(), Some("data/orvu.redb"));
        assert_eq!(alpha.budget.rebalance_ms, Some(1000));
        let security = alpha.security.expect("it has certificates");
        assert_eq!(security.key, std::path::PathBuf::from("node-key.pem"));

        let beta = read(1);
        assert_eq!((beta.replica, beta.listen.as_str()), (2, "[::]:9000"));
        assert_eq!(beta.peers, ["10.0.0.5:7440", "[fd00::5]:7440"]);
        assert_eq!(read(2).listen, "[::]:7440");
        for index in 0..3 {
            read(index).into_node_config().expect("the engine can run with it");
        }
    }

    // The log and the page are off, and the file shows how to turn them on:
    // taking the `# ` off their lines is enough.
    #[test]
    fn the_settings_show_how_to_turn_on_the_log_and_the_page() {
        let machines = [machine("alpha=10.0.0.5"), machine("beta=10.0.0.6")];
        let text = settings(&machines, 0);
        let off = orvu_sdk::SdkConfig::parse_toml(&text).unwrap();
        assert_eq!((off.logs.level, off.metrics.addr), (None, None));
        let on: String = text
            .lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rest) if rest.starts_with('[') || rest.starts_with("level") || rest.starts_with("file") || rest.starts_with("addr") => rest,
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let on = orvu_sdk::SdkConfig::parse_toml(&on).unwrap();
        assert_eq!(on.logs.level.as_deref(), Some("info"));
        assert_eq!(on.logs.file.as_deref(), Some("orvud.log"));
        assert_eq!(on.metrics.addr.as_deref(), Some("127.0.0.1:9464"));
    }

    // The unit is read by systemd, which is not run here: the lines it acts
    // on are checked instead.
    #[test]
    fn the_service_file_starts_the_sidecar_from_its_settings_and_restarts_it() {
        let text = unit(&machine("alpha=10.0.0.5"));
        let lines: Vec<&str> = text.lines().filter(|line| !line.starts_with('#') && !line.is_empty()).collect();
        let at = |section: &str| lines.iter().position(|line| *line == section).unwrap_or_else(|| panic!("no {section} in {text}"));
        let (unit_at, service, install) = (at("[Unit]"), at("[Service]"), at("[Install]"));
        assert!(unit_at < service && service < install, "{text}");
        let service_lines = &lines[service + 1..install];
        assert!(service_lines.contains(&"WorkingDirectory=%h/orvu/alpha"), "{text}");
        assert!(
            service_lines.contains(&"ExecStart=%h/.local/bin/orvud --config %h/orvu/alpha/orvu.toml --socket %h/orvu/alpha/orvud.sock"),
            "{text}"
        );
        assert!(service_lines.contains(&"Restart=on-failure"), "{text}");
        assert!(lines[install + 1..].contains(&"WantedBy=default.target"), "{text}");
        // How to install it is in the file.
        assert!(text.contains("systemctl --user link ~/orvu/alpha/orvud-alpha.service"), "{text}");
        assert!(text.contains("systemctl --user enable --now orvud-alpha.service"), "{text}");
        assert!(text.contains("loginctl enable-linger"), "{text}");
    }

    // A name this computer cannot look up is warned about. An address needs
    // no looking up, and is not.
    #[test]
    fn names_that_do_not_resolve_are_warned_about() {
        let machines = [machine("alpha=10.0.0.5"), machine("beta=known.example"), machine("gamma=gone.example:9000"), machine("c=[fd00::5]")];
        let asked = std::cell::RefCell::new(Vec::new());
        let warnings = unresolved(&machines, |host, port| {
            asked.borrow_mut().push(format!("{host}:{port}"));
            host == "known.example"
        });
        assert_eq!(*asked.borrow(), ["known.example:7440", "gone.example:9000"]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("gamma") && warnings[0].contains("gone.example"), "{warnings:?}");
        assert!(unresolved(&machines[..1], |_, _| false).is_empty());
    }

    #[test]
    fn the_next_steps_name_every_folder_and_the_port_to_open() {
        let machines = [machine("alpha=10.0.0.5"), machine("beta=10.0.0.6:9000")];
        let text = next_steps(Path::new("crew"), &machines);
        assert!(text.contains("crew/alpha") && text.contains("crew/beta"), "{text}");
        assert!(text.contains("orvud --config orvu.toml --socket orvud.sock"), "{text}");
        assert!(text.contains("UDP port 7440, 9000"), "{text}");
    }

    /// A team of three made in a new folder in `top`, called `crew`.
    fn crew(top: &Path) -> std::path::PathBuf {
        let first = top.join("crew");
        write(&first, &[machine("alpha=10.0.0.5"), machine("beta=10.0.0.6"), machine("gamma=10.0.0.7")])
            .expect("writes");
        first
    }

    /// The names a new folder could have that are easy to get wrong: a
    /// space at the end, a space at the start, bytes that are not text.
    fn awkward_names() -> Vec<std::ffi::OsString> {
        use std::os::unix::ffi::OsStrExt;
        vec![
            "crew2".into(),
            "crew2 ".into(),
            " crew2".into(),
            std::ffi::OsStr::from_bytes(b"crew2\xff").to_owned(),
        ]
    }

    fn bytes_of(state: &str, place: &Path) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        [state.as_bytes(), place.as_os_str().as_bytes(), b"\n"].concat()
    }

    // The run making the team again is stopped, as a crash would stop it,
    // after the new folder is made but before the mark says it was
    // finished. The earlier folder must already hold the mark saying it was
    // being made, naming the new folder exactly, and must refuse to give out
    // a number while the new folder is there, whatever it is called.
    #[test]
    fn a_run_stopped_after_making_the_new_team_leaves_the_earlier_one_refused() {
        for name in awkward_names() {
            let top = tempfile::tempdir().unwrap();
            let first = crew(top.path());
            let second = top.path().join(&name);
            let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
            let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_after_by(&second, &stay, &first, |folder, machines, gone| {
                    write_team(folder, machines, gone)?;
                    panic!("stopped here, as a crash would");
                })
            }));
            assert!(stopped.is_err(), "{name:?}: the run was not stopped");
            assert!(second.join("authority/machines").is_file(), "{name:?}: the new team was not made");
            let marker = first.join("authority").join(REPLACED);
            let mark = std::fs::read(&marker).unwrap_or_else(|e| panic!("{name:?}: no mark: {e}"));
            assert_eq!(mark, bytes_of(PENDING, &second), "{name:?}");

            let refused = add(&first, machine("delta=10.0.0.8")).map(|added| added.number);
            assert!(refused.as_ref().is_err_and(|why| why.contains("interrupted")), "{name:?}: {refused:?}");
            assert!(refused.as_ref().is_err_and(|why| why.contains("leave it alone")), "{name:?}: {refused:?}");
            assert!(marker.exists(), "{name:?}: the mark was removed");
            assert!(!first.join("delta").exists(), "{name:?}");
        }
    }

    // A team made again to the end names its folder exactly in the mark,
    // and the earlier folder is refused from then on.
    #[test]
    fn a_finished_mark_names_the_new_folder_exactly() {
        for name in awkward_names() {
            let top = tempfile::tempdir().unwrap();
            let first = crew(top.path());
            let second = top.path().join(&name);
            let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
            write_after(&second, &stay, &first).unwrap_or_else(|e| panic!("{name:?}: {e}"));
            let marker = first.join("authority").join(REPLACED);
            assert_eq!(std::fs::read(&marker).unwrap(), bytes_of(FINISHED, &second), "{name:?}");
            assert!(add(&first, machine("delta=10.0.0.8")).is_err(), "{name:?}");
            assert_eq!(add(&second, machine("delta=10.0.0.8")).unwrap().number, 4, "{name:?}");
        }
    }

    // The mark keeps one folder a line, so a folder whose name has a line
    // break in it could not be written down. It is refused before anything
    // is written.
    #[test]
    fn a_new_folder_with_a_line_break_in_its_name_is_refused_before_anything_is_written() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let second = top.path().join("crew\n2");
        let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
        let refused = write_after(&second, &stay, &first).unwrap_err();
        assert!(refused.contains("line"), "{refused}");
        assert!(!first.join("authority").join(REPLACED).exists());
        let left: Vec<_> = std::fs::read_dir(top.path()).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(left, ["crew"], "something else was made");
        assert_eq!(add(&first, machine("delta=10.0.0.8")).unwrap().number, 4);
    }

    // A mark that cannot be read for sure is never taken for a leftover:
    // the folder is refused and the mark kept.
    #[test]
    fn a_mark_that_cannot_be_read_for_sure_is_refused_and_kept() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let marker = first.join("authority").join(REPLACED);
        let gone = top.path().join("crew2");
        let gone = gone.to_str().unwrap();
        for mark in [
            "pending ".to_string(),
            "pending \n".to_string(),
            "pending crew2\n".to_string(),
            format!("pending {gone}"),
            format!("pending {gone}\n\n"),
        ] {
            std::fs::write(&marker, &mark).unwrap();
            let refused = add(&first, machine("delta=10.0.0.8")).map(|added| added.number);
            assert!(refused.is_err(), "{mark:?}: {refused:?}");
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), mark, "the mark was changed");
            assert!(!first.join("delta").exists(), "{mark:?}");
        }
    }

    // A finished team whose folder is not where the mark says may have been
    // moved, or lost if the computer stopped before it was all on the disk.
    // The message says both, and how to clear the mark by hand.
    #[test]
    fn a_finished_team_that_is_not_there_says_it_may_be_moved_or_lost() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let marker = first.join("authority").join(REPLACED);
        std::fs::write(&marker, bytes_of(FINISHED, &top.path().join("crew2"))).unwrap();
        let refused = add(&first, machine("delta=10.0.0.8")).unwrap_err();
        for says in ["crew2", "moved", "lost", "remove", &marker.display().to_string()] {
            assert!(refused.contains(says), "{says:?} missing from {refused}");
        }
    }

    // The new team is put in place, but the step making it still fails
    // afterwards, as it does if the folder above cannot be kept on the disk.
    // The mark saying it was being made must stay, or the earlier folder and
    // the new one would both give out the same numbers.
    #[test]
    fn a_failed_run_that_left_the_new_team_in_place_keeps_the_earlier_one_refused() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let second = top.path().join("crew2");
        let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
        let failed = write_after_by(&second, &stay, &first, |folder, machines, gone| {
            write_team(folder, machines, gone)?;
            Err(Unmade::MaybePlaced("the disk said no".to_string()))
        })
        .unwrap_err();
        assert!(failed.contains("the disk said no") && failed.contains("crew2"), "{failed}");
        assert!(failed.contains("held back"), "{failed}");
        assert!(failed.contains("made from") && failed.contains("leave it alone"), "{failed}");
        assert!(second.join("authority/machines").is_file());
        let marker = first.join("authority").join(REPLACED);
        assert_eq!(std::fs::read(&marker).unwrap(), bytes_of(PENDING, &second));
        let refused = add(&first, machine("delta=10.0.0.8")).map(|added| added.number);
        assert!(refused.as_ref().is_err_and(|why| why.contains("interrupted")), "{refused:?}");
        assert!(!first.join("delta").exists());
    }

    /// Checks that a run from `earlier` that failed before putting anything
    /// in place says so, takes its own mark away, and leaves nothing of its
    /// own behind in `top`.
    fn nothing_made_and_used_as_before(failed: &str, earlier: &Path, top: &Path) {
        assert!(failed.contains("Nothing was made"), "{failed}");
        assert!(!failed.contains("held back") && !failed.contains("remove"), "{failed}");
        assert!(!earlier.join("authority").join(REPLACED).exists(), "the mark was kept");
        let left: Vec<_> = std::fs::read_dir(top)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("being-made"))
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
        assert!(add(earlier, machine("zeta=10.0.0.20")).is_ok());
    }

    // Two teams are made again into the same new folder at once, from two
    // different earlier folders. The other run puts its team there while
    // this one is still making its own, so this one cannot put its team in
    // place. Nothing of this run's was made, so its earlier folder must not
    // be held back, and the other team must be left alone.
    #[test]
    fn a_run_that_loses_the_new_folder_to_another_run_holds_nothing_back() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let other = top.path().join("band");
        write(&other, &[machine("one=10.0.1.5"), machine("two=10.0.1.6")]).unwrap();
        // The other run's team, made beside, is put in the new folder by
        // hand, as the other run would put it there.
        let theirs = top.path().join("theirs");
        write_after(&theirs, &[machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")], &first).unwrap();
        let second = top.path().join("crew2");
        let stay = [machine("one=10.0.1.5#1"), machine("three=10.0.1.7")];
        let failed = write_after_by(&second, &stay, &other, |folder, _, _| {
            staged_telling(folder, |staging| {
                std::fs::rename(&theirs, folder).unwrap();
                std::fs::write(staging.join("machines"), "x").map_err(|e| e.to_string())
            })
        })
        .unwrap_err();
        nothing_made_and_used_as_before(&failed, &other, top.path());
        let list = std::fs::read_to_string(second.join("authority/machines")).unwrap();
        assert!(list.contains("alpha") && !list.contains("three"), "{list}");
    }

    // Something is put in the new folder after it was found empty, but
    // before the team is put there. The team is never put in place, so the
    // earlier folder is not held back, and what was put there is kept.
    #[test]
    fn a_file_put_in_the_new_folder_during_the_run_holds_nothing_back() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let second = top.path().join("crew2");
        let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
        let failed = write_after_by(&second, &stay, &first, |folder, machines, gone| {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join("notes.txt"), "mine").unwrap();
            write_team(folder, machines, gone)
        })
        .unwrap_err();
        nothing_made_and_used_as_before(&failed, &first, top.path());
        assert_eq!(std::fs::read_to_string(second.join("notes.txt")).unwrap(), "mine");
    }

    // A run that fails before putting its team in place only takes away
    // the mark it wrote. If the mark says something else by then, it is
    // kept, and the earlier folder stays held back.
    #[test]
    fn a_run_that_put_nothing_in_place_keeps_a_mark_it_did_not_write() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let second = top.path().join("crew2");
        let marker = first.join("authority").join(REPLACED);
        let elsewhere = top.path().join("crew3");
        let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
        let failed = write_after_by(&second, &stay, &first, |_, _, _| {
            std::fs::write(&marker, bytes_of(PENDING, &elsewhere)).unwrap();
            Err(Unmade::NothingPlaced("no".to_string()))
        })
        .unwrap_err();
        assert!(
            failed.contains("Nothing was made") && failed.contains("held back"),
            "{failed}"
        );
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            bytes_of(PENDING, &elsewhere)
        );
    }

    // The person made the new folder, empty, before the run, and the run
    // was stopped before it put the team there. An empty folder is never
    // what a run leaves, so the mark is a leftover and is removed.
    #[test]
    fn an_empty_folder_where_the_new_team_was_to_go_does_not_lock_the_earlier_one_out() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let second = top.path().join("crew2");
        std::fs::create_dir(&second).unwrap();
        let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_after_by(&second, &stay, &first, |_, _, _| panic!("stopped here, as a crash would"))
        }));
        assert!(stopped.is_err());
        let marker = first.join("authority").join(REPLACED);
        assert_eq!(std::fs::read(&marker).unwrap(), bytes_of(PENDING, &second));
        assert_eq!(add(&first, machine("delta=10.0.0.8")).unwrap().number, 4);
        assert!(!marker.exists(), "the leftover mark was kept");

        // A run that fails, even one that cannot tell whether it put the
        // team there, takes its own mark away if only the empty folder is
        // there.
        let failed = write_after_by(&second, &stay, &first, |_, _, _| {
            Err(Unmade::MaybePlaced("no".to_string()))
        });
        assert_eq!(failed.unwrap_err(), "no");
        assert!(!marker.exists(), "the mark was kept");
        assert_eq!(add(&first, machine("eps=10.0.0.9")).unwrap().number, 5);
    }

    // Anything but an empty folder at the place a mark names may be the new
    // team, or part of it: a folder with something in it, a file, or a link
    // to an empty folder. The earlier folder is refused and the mark kept.
    #[test]
    fn only_a_real_empty_folder_counts_as_nothing_made() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let marker = first.join("authority").join(REPLACED);
        let full = top.path().join("full");
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("node-key.pem"), "").unwrap();
        let file = top.path().join("file");
        std::fs::write(&file, "").unwrap();
        let empty = top.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let link = top.path().join("link");
        std::os::unix::fs::symlink(&empty, &link).unwrap();
        for place in [&full, &file, &link] {
            std::fs::write(&marker, bytes_of(PENDING, place)).unwrap();
            let refused = add(&first, machine("delta=10.0.0.8")).map(|added| added.number);
            assert!(refused.as_ref().is_err_and(|why| why.contains("interrupted")), "{place:?}: {refused:?}");
            assert_eq!(std::fs::read(&marker).unwrap(), bytes_of(PENDING, place), "{place:?}");
        }
    }

    /// Makes `step` fail, and takes every other step as it is.
    fn failing(step: Step) -> impl Fn(Step, &mut dyn FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
        move |now, take| {
            if now == step {
                return Err(std::io::Error::other("the disk said no"));
            }
            take()
        }
    }

    /// Makes the team again from a new `crew`, in `crew2`, taking its steps
    /// with `steps`. Gives back the earlier folder, the new one, and what
    /// the run said.
    fn made_again_by(top: &Path, steps: Steps) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let first = crew(top);
        let second = top.join("crew2");
        let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
        let failed = write_after_by(&second, &stay, &first, |folder, machines, gone| {
            write_team_by(folder, machines, gone, steps)
        })
        .unwrap_err();
        (first, second, failed)
    }

    // A step that fails before the team is moved into place leaves nothing
    // there: the run says so, takes its mark away, leaves nothing of its
    // own behind, and the earlier folder is used as before.
    #[test]
    fn a_step_that_fails_before_the_team_is_moved_into_place_holds_nothing_back() {
        for step in [Step::Keep, Step::ClearEmpty, Step::Rename] {
            let top = tempfile::tempdir().unwrap();
            if step == Step::ClearEmpty {
                // Only an empty folder already there is taken away.
                std::fs::create_dir(top.path().join("crew2")).unwrap();
            }
            let (first, second, failed) = made_again_by(top.path(), &failing(step));
            assert!(failed.contains("the disk said no"), "{step:?}: {failed}");
            assert!(nothing_made_at(&second).unwrap(), "{step:?}: something is in the new folder");
            nothing_made_and_used_as_before(&failed, &first, top.path());
        }
    }

    // Writing what goes in the folder fails: nothing is put in place, and
    // nothing of it is left beside.
    #[test]
    fn a_folder_whose_contents_cannot_be_written_holds_nothing_back() {
        let top = tempfile::tempdir().unwrap();
        let first = crew(top.path());
        let second = top.path().join("crew2");
        let stay = [machine("alpha=10.0.0.5#1"), machine("beta=10.0.0.6#2")];
        let failed = write_after_by(&second, &stay, &first, |folder, _, _| {
            staged_telling(folder, |staging| {
                std::fs::write(staging.join("machines"), "half").unwrap();
                Err("the disk is full".to_string())
            })
        })
        .unwrap_err();
        assert!(failed.contains("the disk is full"), "{failed}");
        assert!(!second.exists());
        nothing_made_and_used_as_before(&failed, &first, top.path());
    }

    // The team is moved into place, but the folder above cannot be kept on
    // the disk afterwards. The team is there, so the mark saying it was
    // being made must stay, and the earlier folder must give out no number.
    #[test]
    fn a_failure_after_the_team_is_moved_into_place_keeps_the_earlier_one_refused() {
        let top = tempfile::tempdir().unwrap();
        let (first, second, failed) = made_again_by(top.path(), &failing(Step::KeepAbove));
        assert!(failed.contains("the disk said no") && failed.contains("held back"), "{failed}");
        assert!(second.join("authority/machines").is_file(), "the team is not in place");
        let marker = first.join("authority").join(REPLACED);
        assert_eq!(std::fs::read(&marker).unwrap(), bytes_of(PENDING, &second));
        let refused = add(&first, machine("delta=10.0.0.8")).map(|added| added.number);
        assert!(refused.as_ref().is_err_and(|why| why.contains("interrupted")), "{refused:?}");
        assert!(!first.join("delta").exists());
    }

    // On a network disk a rename can be tried again after the first try
    // worked, and the second try then fails because the folder is gone. The
    // rename says it failed, but the team is in place, so the mark must stay
    // and the earlier folder must give out no number. That holds too if
    // another folder is made where the team was made beside.
    #[test]
    fn a_rename_that_fails_after_moving_the_team_keeps_the_earlier_one_refused() {
        for another_beside in [false, true] {
            let top = tempfile::tempdir().unwrap();
            let beside = top.path().join(format!(".crew2.being-made-{}", std::process::id()));
            let worked_then_failed = |now: Step, take: &mut dyn FnMut() -> std::io::Result<()>| {
                take()?;
                if now == Step::Rename {
                    if another_beside {
                        std::fs::create_dir(&beside).unwrap();
                    }
                    return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
                }
                Ok(())
            };
            rename_failed_but_moved(top.path(), &worked_then_failed);
        }
    }

    /// Checks that a run whose rename failed after moving its team into
    /// place keeps its mark, and that the earlier folder is refused.
    fn rename_failed_but_moved(top: &Path, steps: Steps) {
        let (first, second, failed) = made_again_by(top, steps);
        assert!(failed.contains("held back"), "{failed}");
        assert!(second.join("authority/machines").is_file(), "the team is not in place");
        let marker = first.join("authority").join(REPLACED);
        assert_eq!(std::fs::read(&marker).unwrap(), bytes_of(PENDING, &second));
        let refused = add(&first, machine("delta=10.0.0.8")).map(|added| added.number);
        assert!(refused.as_ref().is_err_and(|why| why.contains("interrupted")), "{refused:?}");
        assert!(!first.join("delta").exists());
    }
}
