//! Reading the command line. This is kept apart from `main` so that it can be
//! tested without starting anything.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

pub const USAGE: &str = "usage: orvud --local [--data-dir <folder>] [--new-number | --moved | --restored] [--socket <path>] [--network <address>] [--model <folder>] [--exit-when-idle <seconds>]\n\
       orvud --config <path.toml> [--socket <path>] [--network <address>] [--model <folder>] [--exit-when-idle <seconds>]\n\
       orvud setup <folder> [--from <earlier folder>] <name>=<address> <name>=<address> ...\n\
       orvud setup <folder> --add <name>=<address>\n\
       orvud leave --config <path.toml> [--wait <seconds>]\n\
       orvud agent add|remove <name> (--local [--data-dir <folder>] | --config <path.toml>)\n\
       orvud agent add <name> --address <host:port> --connection <file> (--local [--data-dir <folder>] | --config <path.toml>)\n\
       orvud agent list (--local [--data-dir <folder>] | --config <path.toml>)\n\
       orvud agent allow <name> <rule> <rule> ... | all (--local [--data-dir <folder>] | --config <path.toml>)\n\
       orvud audit (--local [--data-dir <folder>] | --config <path.toml>) [--agent <name>] [--object <start>] [--since <time>] [--not-allowed] [--json]\n\
       orvud --version | --help\n\
\n\
--local             Run on this machine only: no settings file, no certificates,\n\
                    no network. Data is kept in the data folder.\n\
--data-dir          The data folder for --local. The default is\n\
                    $XDG_DATA_HOME/orvu, or ~/.local/share/orvu\n\
                    (~/Library/Application Support/orvu on macOS).\n\
--new-number        For --local, when the data folder was copied from another\n\
                    computer that still uses it: keep the data and take a new\n\
                    machine number. Without this, or --moved, such a folder is\n\
                    refused, because two computers must never share a number.\n\
--moved             For --local, when the data folder was moved here from\n\
                    another computer that no longer uses it: keep the data\n\
                    and the number.\n\
--restored          For --local, when the data folder was restored from a\n\
                    backup or rolled back in any way: keep the data and take\n\
                    a new machine number, so that new changes are not taken\n\
                    for ones the other machines have seen. Use it once.\n\
--config            Run from a settings file, connected to other machines.\n\
--socket            The socket to serve on. The default is\n\
                    $XDG_RUNTIME_DIR/orvud.sock. With --local and no\n\
                    $XDG_RUNTIME_DIR it is orvud.sock inside the data folder.\n\
                    With --config and no $XDG_RUNTIME_DIR it must be given.\n\
--network           Also serve agents on other machines and in containers, at\n\
                    this address, such as 0.0.0.0:7441. Every connection is\n\
                    encrypted, and only agents with keys are let in, so it\n\
                    needs at least one: add one first with `orvud agent add`.\n\
--model             The folder that holds the model search by meaning uses, or\n\
                    $ORVU_MODEL. Without either, the model is fetched from the\n\
                    internet the first time a team searches, 30 MB, into a\n\
                    folder inside the data folder (with --config, the\n\
                    settings file's).\n\
--exit-when-idle    Stop once no client has been connected for this many seconds.\n\
setup               Make the settings and certificates for a team of machines,\n\
                    one folder per machine inside <folder>. An address is a\n\
                    machine's name or IP address, with :port if it is not 7440.\n\
                    The key that signs the certificates is kept in\n\
                    <folder>/authority. With --add, one machine is added to a\n\
                    team made earlier in <folder>, and the others need not be\n\
                    stopped. With --from, the team made earlier in that folder\n\
                    is made again, keeping every number it gave out, to take\n\
                    machines out at once.\n\
leave               Take this machine out of its team for good. Stop its\n\
                    sidecar first. It hands its share of every budget to the\n\
                    machines it reaches, waits until they have it, at most\n\
                    --wait seconds (60 if not given), and stops. It should not\n\
                    be started again afterwards.\n\
agent               Add, remove or list the agents the sidecar lets in. Adding\n\
                    one prints its key, once. While there are none, every\n\
                    program that can reach the socket is let in. The sidecar\n\
                    does not have to be stopped for this. With --address and\n\
                    --connection, adding one writes a file that holds all an\n\
                    agent needs to connect over the network, its key with it.\n\
agent allow         Give an agent rules for what it may do, such as\n\
                    notes:write tasks:read budgets:spend. The blocks are notes,\n\
                    documents, tallies, tasks, budgets, inbox, roster and\n\
                    objects. The levels are read and write, and for budgets\n\
                    read, spend and top-up. Notes and tasks also take own:\n\
                    read them all, and change only the agent's own.\n\
                    A block left out may not be used.\n\
                    `all` takes the rules away, and the agent may do everything.\n\
audit               Show who changed what through this sidecar, oldest first:\n\
                    only one agent's changes (- for those made with no key),\n\
                    only objects whose names start with <start>, only those\n\
                    from <time> ago (such as 30m, 2h or 7d), or only what was\n\
                    not allowed. --json prints the lines as they are kept.";

/// How the engine is started.
#[derive(Debug, PartialEq, Eq)]
pub enum Mode {
    /// From a settings file, connected to other machines.
    Config(PathBuf),
    /// On this machine only. `None` means the default data folder.
    Local { data_dir: Option<PathBuf> },
}

/// What the command line asked for when it asked to run.
#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub mode: Mode,
    pub socket: Option<PathBuf>,
    /// The address to serve on over the network, if any.
    pub network: Option<String>,
    pub exit_when_idle: Option<Duration>,
    /// The folder that holds the model for search by meaning. `None` means
    /// it is fetched into the data folder when it is first needed.
    pub model: Option<PathBuf>,
    /// What to do with a local data folder last used on another computer.
    pub if_copied: crate::local::IfCopied,
}

/// What `orvud agent` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentAction {
    Add(String),
    /// Adds an agent and writes a connection file for it: what it needs to
    /// connect over the network to `address`.
    AddConnected { name: String, address: String, file: PathBuf },
    Remove(String),
    List,
    /// Gives an agent rules. `["all"]` takes them away.
    Allow(String, Vec<String>),
}

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Options),
    /// Make the settings for a team of machines in a folder.
    /// With `earlier`, the team made in that folder is made again.
    Setup { folder: PathBuf, machines: Vec<crate::setup::Machine>, earlier: Option<PathBuf> },
    /// Add one machine to a team whose settings were made in a folder.
    AddMachine { folder: PathBuf, machine: crate::setup::Machine },
    /// Take this machine out of its team for good.
    Leave { settings: PathBuf, wait: Duration },
    /// Change or show the agents the sidecar started this way lets in.
    Agent { action: AgentAction, mode: Mode },
    /// Show the record of who changed what.
    Audit { mode: Mode, filter: crate::audit::Filter, json: bool },
    Help,
    Version,
}

/// Reads the arguments that follow the program name. An error is a message
/// for the user and ends with the usage text.
///
/// Arguments are taken as the system hands them over, because a folder name
/// does not have to be valid text. Flags are compared as text, and a folder or
/// socket path is kept exactly as it was given.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let fail = |message: String| format!("{message}\n{USAGE}");
    // A flag given twice is a mistake. Quietly using the last one would let a
    // script that adds its own flag change where the data goes.
    fn once<T>(slot: &mut Option<T>, flag: &str, value: T, fail: impl Fn(String) -> String) -> Result<(), String> {
        match slot.replace(value) {
            Some(_) => Err(fail(format!("{flag} was given twice"))),
            None => Ok(()),
        }
    }
    let mut args = args.into_iter().peekable();
    if args.peek().is_some_and(|first| first == "setup") {
        args.next();
        return setup(args, fail);
    }
    if args.peek().is_some_and(|first| first == "leave") {
        args.next();
        return leave(args, fail);
    }
    if args.peek().is_some_and(|first| first == "audit") {
        args.next();
        return audit(args, fail);
    }
    let mut agent = None;
    if args.peek().is_some_and(|first| first == "agent") {
        args.next();
        let action = args.next();
        agent = Some(match action.as_ref().and_then(|a| a.to_str()) {
            Some("list") => AgentAction::List,
            Some(word @ ("add" | "remove")) => {
                let name = match args.next() {
                    Some(name) if !name.to_str().is_some_and(|text| text.starts_with("--")) => name,
                    _ => return Err(fail(format!("agent {word} needs the agent's name"))),
                };
                let Some(name) = name.to_str().map(str::to_string) else {
                    return Err(fail(format!("{} cannot be an agent's name", name.to_string_lossy())));
                };
                if word == "add" { AgentAction::Add(name) } else { AgentAction::Remove(name) }
            }
            Some("allow") => {
                let name = match args.next() {
                    Some(name) if !name.to_str().is_some_and(|text| text.starts_with("--")) => name,
                    _ => return Err(fail("agent allow needs the agent's name".into())),
                };
                let Some(name) = name.to_str().map(str::to_string) else {
                    return Err(fail(format!("{} cannot be an agent's name", name.to_string_lossy())));
                };
                let mut rules = Vec::new();
                while let Some(rule) = args.next_if(|word| !word.to_str().is_some_and(|text| text.starts_with("--"))) {
                    match rule.into_string() {
                        Ok(rule) => rules.push(rule),
                        Err(rule) => return Err(fail(format!("{} is not a rule", rule.to_string_lossy()))),
                    }
                }
                if rules.is_empty() {
                    return Err(fail("agent allow needs rules, such as notes:write, or all".into()));
                }
                AgentAction::Allow(name, rules)
            }
            Some("--help" | "-h") => return Ok(Command::Help),
            _ => return Err(fail("agent is followed by add, remove, list or allow".into())),
        });
    }
    let (mut config, mut data_dir, mut socket, mut idle, mut local) = (None, None, None, None, None);
    let (mut network, mut address, mut connection, mut model) = (None, None, None, None);
    let (mut new_number, mut moved, mut restored) = (None, None, None);
    while let Some(arg) = args.next() {
        // A flag must not take the next flag as its value. Otherwise
        // `--config --socket x` would mean a settings file named "--socket",
        // which fails later with a confusing message.
        let mut value = |flag: &str| match args.next() {
            Some(v) if v.to_str().is_some_and(|text| text.starts_with("--")) => Err(fail(format!(
                "{flag} needs a value, got the flag {}",
                v.to_string_lossy()
            ))),
            Some(v) if v.is_empty() => Err(fail(format!("{flag} needs a value, got an empty one"))),
            Some(v) => Ok(v),
            None => Err(fail(format!("{flag} needs a value"))),
        };
        match arg.to_str() {
            Some("--local") => once(&mut local, "--local", (), fail)?,
            Some("--new-number") => once(&mut new_number, "--new-number", (), fail)?,
            Some("--moved") => once(&mut moved, "--moved", (), fail)?,
            Some("--restored") => once(&mut restored, "--restored", (), fail)?,
            Some("--config") => once(&mut config, "--config", PathBuf::from(value("--config")?), fail)?,
            Some("--data-dir") => once(&mut data_dir, "--data-dir", PathBuf::from(value("--data-dir")?), fail)?,
            Some("--socket") => once(&mut socket, "--socket", PathBuf::from(value("--socket")?), fail)?,
            Some("--network") => once(&mut network, "--network", text(value("--network")?, "--network", fail)?, fail)?,
            Some("--address") => once(&mut address, "--address", text(value("--address")?, "--address", fail)?, fail)?,
            Some("--connection") => once(&mut connection, "--connection", PathBuf::from(value("--connection")?), fail)?,
            Some("--model") => once(&mut model, "--model", PathBuf::from(value("--model")?), fail)?,
            Some("--exit-when-idle") => {
                let given = value("--exit-when-idle")?;
                let seconds = given.to_str().and_then(|text| text.parse::<u64>().ok()).filter(|n| *n > 0);
                match seconds {
                    Some(seconds) => {
                        once(&mut idle, "--exit-when-idle", Duration::from_secs(seconds), fail)?
                    }
                    None => {
                        return Err(fail(format!(
                            "--exit-when-idle needs a whole number of seconds above 0, got {}",
                            given.to_string_lossy()
                        )))
                    }
                }
            }
            // Asking for help or the version is not a mistake.
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--version" | "-V") => return Ok(Command::Version),
            _ => return Err(fail(format!("unknown argument: {}", arg.to_string_lossy()))),
        }
    }
    let mode = match (local.is_some(), config) {
        (true, Some(_)) => return Err(fail("--local and --config cannot be used together".into())),
        (true, None) => Mode::Local { data_dir },
        (false, Some(path)) if data_dir.is_none() => Mode::Config(path),
        (false, Some(_)) => return Err(fail("--data-dir only goes with --local".into())),
        (false, None) => return Err(fail("one of --local or --config is required".into())),
    };
    let given: Vec<&str> = [("--new-number", new_number), ("--moved", moved), ("--restored", restored)]
        .into_iter()
        .filter_map(|(flag, given)| given.map(|()| flag))
        .collect();
    let if_copied = match given[..] {
        [] => crate::local::IfCopied::Refuse,
        ["--new-number"] => crate::local::IfCopied::NewNumber,
        ["--moved"] => crate::local::IfCopied::AcceptMove,
        ["--restored"] => crate::local::IfCopied::Restored,
        [first, second, ..] => return Err(fail(format!("{first} and {second} cannot be used together"))),
        [other] => unreachable!("{other} is not one of these flags"),
    };
    if let (Mode::Config(_), Some(flag)) = (&mode, given.first()) {
        return Err(fail(format!("{flag} only goes with --local")));
    }
    if let Some(action) = agent {
        // The agents belong to a sidecar's data, not to how it runs.
        if socket.is_some() || idle.is_some() || network.is_some() || model.is_some() || !given.is_empty() {
            return Err(fail("agent takes only --local, --data-dir or --config".into()));
        }
        let action = match (action, address, connection) {
            (action, None, None) => action,
            (AgentAction::Add(name), Some(address), Some(file)) => AgentAction::AddConnected { name, address, file },
            (AgentAction::Add(_), _, _) => return Err(fail("--address and --connection go together".into())),
            _ => return Err(fail("--address and --connection only go with agent add".into())),
        };
        return Ok(Command::Agent { action, mode });
    }
    if address.is_some() || connection.is_some() {
        return Err(fail("--address and --connection only go with agent add".into()));
    }
    Ok(Command::Run(Options { mode, socket, network, exit_when_idle: idle, model, if_copied }))
}

/// A value that has to be text, such as an address.
fn text(value: OsString, flag: &str, fail: impl Fn(String) -> String) -> Result<String, String> {
    value.into_string().map_err(|value| fail(format!("{flag} needs text, got {}", value.to_string_lossy())))
}

/// Reads what follows `audit`: where the sidecar keeps its data, and which
/// lines to show.
fn audit(args: impl Iterator<Item = OsString>, fail: impl Fn(String) -> String) -> Result<Command, String> {
    let (mut local, mut data_dir, mut config) = (false, None, None);
    let mut filter = crate::audit::Filter::default();
    let mut json = false;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let Some(flag) = arg.to_str() else {
            return Err(fail(format!("unknown argument: {}", arg.to_string_lossy())));
        };
        let mut value = || match args.next().map(|value| value.into_string()) {
            Some(Ok(value)) if !value.starts_with("--") && !value.is_empty() => Ok(value),
            _ => Err(fail(format!("{flag} needs a value"))),
        };
        match flag {
            "--local" => local = true,
            "--data-dir" => data_dir = Some(PathBuf::from(value()?)),
            "--config" => config = Some(PathBuf::from(value()?)),
            "--agent" => filter.agent = Some(value()?),
            "--object" => filter.object = Some(value()?),
            "--since" => filter.since = Some(crate::audit::duration(&value()?).map_err(&fail)?),
            "--not-allowed" => filter.not_allowed = true,
            "--json" => json = true,
            "--help" | "-h" => return Ok(Command::Help),
            _ => return Err(fail(format!("unknown argument: {flag}"))),
        }
    }
    let mode = match (local, config) {
        (true, Some(_)) => return Err(fail("--local and --config cannot be used together".into())),
        (true, None) => Mode::Local { data_dir },
        (false, Some(path)) if data_dir.is_none() => Mode::Config(path),
        (false, Some(_)) => return Err(fail("--data-dir only goes with --local".into())),
        (false, None) => return Err(fail("one of --local or --config is required".into())),
    };
    Ok(Command::Audit { mode, filter, json })
}

/// Reads what follows `setup`: the folder, then the machines, or `--add`
/// and the one machine to add.
fn setup(args: impl Iterator<Item = OsString>, fail: impl Fn(String) -> String) -> Result<Command, String> {
    let mut folder = None;
    let mut machines = Vec::new();
    let mut add = false;
    let mut earlier = None;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--add") => {
                if add {
                    return Err(fail("--add was given twice".into()));
                }
                if args.peek().is_none_or(|next| next.to_str().is_some_and(|text| text.starts_with("--"))) {
                    return Err(fail("--add needs the machine to add, as name=address".into()));
                }
                add = true;
            }
            Some("--from") => {
                if earlier.is_some() {
                    return Err(fail("--from was given twice".into()));
                }
                match args.next() {
                    Some(value) if !value.is_empty() && !value.to_str().is_some_and(|text| text.starts_with("--") || text.contains('=')) => {
                        earlier = Some(PathBuf::from(value))
                    }
                    _ => return Err(fail("--from needs the folder the team was made in before".into())),
                }
            }
            Some(flag) if flag.starts_with("--") => {
                return Err(fail(format!("setup takes a folder and machines, not the flag {flag}")))
            }
            // A folder is never written like a machine. This is the folder
            // having been left out, and the first machine would be taken
            // for it.
            Some(text) if folder.is_none() && text.contains('=') => {
                return Err(fail(format!("the folder comes first, and {text:?} is a machine")))
            }
            _ if folder.is_none() => folder = Some(PathBuf::from(arg)),
            Some(text) => machines.push(crate::setup::Machine::parse(text).map_err(&fail)?),
            None => return Err(fail(format!("{} is not a machine", arg.to_string_lossy()))),
        }
    }
    let Some(folder) = folder else {
        return Err(fail("setup needs a folder to put the settings in".into()));
    };
    if add {
        if earlier.is_some() {
            return Err(fail("--from makes a team again, and does not go with --add".into()));
        }
        let mut machines = machines.into_iter();
        return match (machines.next(), machines.next()) {
            (Some(machine), None) => Ok(Command::AddMachine { folder, machine }),
            _ => Err(fail("--add adds one machine at a time".into())),
        };
    }
    crate::setup::check(&machines).map_err(&fail)?;
    Ok(Command::Setup { folder, machines, earlier })
}

/// Reads what follows `leave`: the settings of the machine that leaves, and
/// how long to wait for the others to have its share.
fn leave(args: impl Iterator<Item = OsString>, fail: impl Fn(String) -> String) -> Result<Command, String> {
    let (mut settings, mut wait) = (None, None);
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        let Some(flag) = arg.to_str() else {
            return Err(fail(format!("unknown argument: {}", arg.to_string_lossy())));
        };
        let mut value = || match args.next() {
            Some(value) if !value.is_empty() && !value.to_str().is_some_and(|text| text.starts_with("--")) => Ok(value),
            _ => Err(fail(format!("{flag} needs a value"))),
        };
        match flag {
            "--config" if settings.is_none() => settings = Some(PathBuf::from(value()?)),
            "--wait" if wait.is_none() => {
                let given = value()?;
                match given.to_str().and_then(|text| text.parse::<u64>().ok()).filter(|n| *n > 0) {
                    Some(seconds) => wait = Some(Duration::from_secs(seconds)),
                    None => {
                        return Err(fail(format!(
                            "--wait needs a whole number of seconds above 0, got {}",
                            given.to_string_lossy()
                        )))
                    }
                }
            }
            "--config" | "--wait" => return Err(fail(format!("{flag} was given twice"))),
            "--local" => return Err(fail("leave is for a machine of a team, started with --config".into())),
            "--help" | "-h" => return Ok(Command::Help),
            _ => return Err(fail(format!("unknown argument: {flag}"))),
        }
    }
    let Some(settings) = settings else {
        return Err(fail("leave needs --config and the machine's settings file".into()));
    };
    Ok(Command::Leave { settings, wait: wait.unwrap_or(LEAVE_WAIT) })
}

/// How long `orvud leave` waits, when not told, for the other machines to
/// connect and to have this one's share.
pub const LEAVE_WAIT: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, String> {
        parse(line.split_whitespace().map(OsString::from))
    }

    fn run(line: &str) -> Options {
        match parse_str(line) {
            Ok(Command::Run(options)) => options,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn local_needs_nothing_else() {
        let options = run("--local");
        assert_eq!(options.mode, Mode::Local { data_dir: None });
        assert_eq!(options.socket, None);
        assert_eq!(options.exit_when_idle, None);
    }

    #[test]
    fn every_flag_is_read() {
        let options = run("--local --data-dir /d --socket /s --exit-when-idle 30");
        assert_eq!(options.mode, Mode::Local { data_dir: Some("/d".into()) });
        assert_eq!(options.socket, Some("/s".into()));
        assert_eq!(options.exit_when_idle, Some(Duration::from_secs(30)));

        assert_eq!(run("--config /c.toml").mode, Mode::Config("/c.toml".into()));
    }

    // A data folder that was last used on another computer is refused unless
    // one of these says what to do with it.
    #[test]
    fn a_copied_folder_is_dealt_with_by_a_flag() {
        use crate::local::IfCopied;
        assert_eq!(run("--local").if_copied, IfCopied::Refuse);
        assert_eq!(run("--local --new-number").if_copied, IfCopied::NewNumber);
        assert_eq!(run("--local --data-dir /d --moved").if_copied, IfCopied::AcceptMove);
        assert_eq!(run("--config /c.toml").if_copied, IfCopied::Refuse);
        assert_eq!(run("--local --restored").if_copied, IfCopied::Restored);
        for (line, says) in [
            ("--local --new-number --moved", "--new-number and --moved cannot be used together"),
            ("--local --restored --moved", "--moved and --restored cannot be used together"),
            ("--local --new-number --restored", "--new-number and --restored cannot be used together"),
            ("--local --restored --restored", "--restored was given twice"),
            ("--config c.toml --restored", "--restored only goes with --local"),
            ("agent list --local --restored", "agent takes only"),
            ("--local --new-number --new-number", "--new-number was given twice"),
            ("--config c.toml --new-number", "--new-number only goes with --local"),
            ("--config c.toml --moved", "--moved only goes with --local"),
            ("agent list --local --moved", "agent takes only"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
        }
    }

    #[test]
    fn help_and_version_are_not_errors() {
        assert_eq!(parse_str("--help"), Ok(Command::Help));
        assert_eq!(parse_str("-h"), Ok(Command::Help));
        assert_eq!(parse_str("--version"), Ok(Command::Version));
    }

    #[test]
    fn mistakes_are_explained() {
        for (line, says) in [
            ("", "one of --local or --config is required"),
            ("--local --config c.toml", "cannot be used together"),
            ("--config c.toml --data-dir /d", "only goes with --local"),
            ("--config --socket x", "needs a value, got the flag --socket"),
            ("--socket", "needs a value"),
            ("--local --exit-when-idle soon", "whole number of seconds"),
            ("--local --exit-when-idle 0", "whole number of seconds"),
            ("--local --frobnicate", "unknown argument: --frobnicate"),
            ("--local --data-dir /a --data-dir /b", "--data-dir was given twice"),
            ("--local --socket /a --socket /b", "--socket was given twice"),
            ("--local --local", "--local was given twice"),
            ("--config a.toml --config b.toml", "--config was given twice"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
            assert!(message.contains("usage:"), "{line:?} did not show the usage");
        }
    }

    #[test]
    fn setup_takes_a_folder_and_machines() {
        match parse_str("setup crew alpha=10.0.0.5 beta=box.example.com:9000") {
            Ok(Command::Setup { folder, machines, earlier: None }) => {
                assert_eq!(folder, PathBuf::from("crew"));
                let read: Vec<String> = machines.iter().map(|m| format!("{}@{}", m.name, m.address())).collect();
                assert_eq!(read, ["alpha@10.0.0.5:7440", "beta@box.example.com:9000"]);
            }
            other => panic!("expected a setup, got {other:?}"),
        }
        assert_eq!(parse_str("setup --help"), Ok(Command::Help));
        match parse_str("setup crew2 --from crew alpha=10.0.0.5#1 gamma=10.0.0.7#3") {
            Ok(Command::Setup { folder, machines, earlier }) => {
                assert_eq!((folder, earlier), (PathBuf::from("crew2"), Some(PathBuf::from("crew"))));
                assert_eq!(machines.len(), 2);
            }
            other => panic!("expected a setup, got {other:?}"),
        }
        for (line, says) in [
            ("setup crew2 alpha=10.0.0.5 beta=10.0.0.6 --from", "--from needs the folder"),
            ("setup crew2 --from alpha=10.0.0.5 beta=10.0.0.6", "--from needs the folder"),
            ("setup crew2 --from a --from b alpha=10.0.0.5 beta=10.0.0.6", "--from was given twice"),
            ("setup crew --from old --add gamma=10.0.0.7", "does not go with --add"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
        }
    }

    #[test]
    fn setup_mistakes_are_explained() {
        for (line, says) in [
            ("setup", "needs a folder"),
            ("setup crew", "at least two machines"),
            ("setup crew alpha=10.0.0.5", "at least two machines"),
            ("setup crew alpha=10.0.0.5 beta", "name=address"),
            ("setup crew alpha=10.0.0.5 alpha=10.0.0.6", "two machines are called"),
            ("setup crew --local alpha=10.0.0.5", "not the flag --local"),
            // The folder was forgotten. The first machine is not taken for it.
            ("setup alpha=10.0.0.5 beta=10.0.0.6 gamma=10.0.0.7", "the folder comes first"),
            // `setup` is only a command in first place.
            ("--local setup", "unknown argument: setup"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
            assert!(message.contains("usage:"), "{line:?} did not show the usage");
        }
    }

    #[test]
    fn setup_can_add_one_machine() {
        match parse_str("setup crew --add gamma=10.0.0.7#4") {
            Ok(Command::AddMachine { folder, machine }) => {
                assert_eq!(folder, PathBuf::from("crew"));
                assert_eq!((machine.name.as_str(), machine.number), ("gamma", Some(4)));
            }
            other => panic!("expected a machine to add, got {other:?}"),
        }
        for (line, says) in [
            ("setup crew --add", "--add needs the machine"),
            ("setup --add gamma=10.0.0.7", "the folder comes first"),
            ("setup crew --add gamma=10.0.0.7 delta=10.0.0.8", "one machine at a time"),
            ("setup crew alpha=10.0.0.5 --add gamma=10.0.0.7", "one machine at a time"),
            ("setup crew --add gamma", "name=address"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
        }
    }

    #[test]
    fn leave_takes_the_settings_and_a_wait() {
        assert_eq!(
            parse_str("leave --config crew/alpha/orvu.toml"),
            Ok(Command::Leave { settings: "crew/alpha/orvu.toml".into(), wait: Duration::from_secs(60) })
        );
        assert_eq!(
            parse_str("leave --config a.toml --wait 5"),
            Ok(Command::Leave { settings: "a.toml".into(), wait: Duration::from_secs(5) })
        );
        assert_eq!(parse_str("leave --help"), Ok(Command::Help));
        for (line, says) in [
            ("leave", "leave needs --config"),
            ("leave --local", "a machine of a team"),
            ("leave --config a.toml --wait 0", "whole number of seconds"),
            ("leave --config a.toml --wait", "--wait needs a value"),
            ("leave --config a.toml --socket s", "unknown argument: --socket"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
            assert!(message.contains("usage:"), "{line:?} did not show the usage");
        }
    }

    #[test]
    fn agent_takes_an_action_and_where_the_sidecar_keeps_its_data() {
        assert_eq!(
            parse_str("agent add alice --local"),
            Ok(Command::Agent { action: AgentAction::Add("alice".into()), mode: Mode::Local { data_dir: None } })
        );
        assert_eq!(
            parse_str("agent remove bob --local --data-dir /d"),
            Ok(Command::Agent {
                action: AgentAction::Remove("bob".into()),
                mode: Mode::Local { data_dir: Some("/d".into()) }
            })
        );
        assert_eq!(
            parse_str("agent list --config crew/alpha/orvu.toml"),
            Ok(Command::Agent { action: AgentAction::List, mode: Mode::Config("crew/alpha/orvu.toml".into()) })
        );
        assert_eq!(parse_str("agent --help"), Ok(Command::Help));
        assert_eq!(
            parse_str("agent allow alice notes:write budgets:spend --local"),
            Ok(Command::Agent {
                action: AgentAction::Allow("alice".into(), vec!["notes:write".into(), "budgets:spend".into()]),
                mode: Mode::Local { data_dir: None }
            })
        );
    }

    #[test]
    fn the_network_and_connection_files_are_asked_for_by_flags() {
        assert_eq!(run("--local --network 0.0.0.0:7441").network.as_deref(), Some("0.0.0.0:7441"));
        assert_eq!(
            parse_str("agent add alice --address box:7441 --connection alice.orvu --local"),
            Ok(Command::Agent {
                action: AgentAction::AddConnected { name: "alice".into(), address: "box:7441".into(), file: "alice.orvu".into() },
                mode: Mode::Local { data_dir: None }
            })
        );
        for (line, says) in [
            ("agent add alice --address box:7441 --local", "go together"),
            ("agent remove alice --address box:7441 --connection a --local", "only go with agent add"),
            ("--local --connection a.orvu", "only go with agent add"),
            ("agent add alice --network 0.0.0.0:1 --local", "agent takes only"),
            ("--local --network a --network b", "--network was given twice"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
        }
    }

    // The model for search by meaning can be given, once, to a sidecar that
    // runs, and is fetched when it is not.
    #[test]
    fn the_model_is_given_by_a_flag() {
        assert_eq!(run("--local --model /models/potion").model, Some("/models/potion".into()));
        assert_eq!(run("--config c.toml").model, None);
        for (line, says) in [
            ("--local --model a --model b", "--model was given twice"),
            ("--local --model", "--model needs a value"),
            ("agent list --model a --local", "agent takes only"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
        }
    }

    #[test]
    fn agent_mistakes_are_explained() {
        for (line, says) in [
            ("agent", "followed by add, remove, list or allow"),
            ("agent rename alice --local", "followed by add, remove, list or allow"),
            ("agent allow --local", "needs the agent's name"),
            ("agent allow alice --local", "needs rules"),
            ("agent add --local", "needs the agent's name"),
            ("agent add", "needs the agent's name"),
            ("agent add alice", "one of --local or --config is required"),
            ("agent list --local --socket /s", "agent takes only"),
            ("agent list --local --exit-when-idle 5", "agent takes only"),
            ("--local agent list", "unknown argument: agent"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
            assert!(message.contains("usage:"), "{line:?} did not show the usage");
        }
    }

    #[test]
    fn audit_takes_where_and_which_lines() {
        match parse_str("audit --local --data-dir /d --agent alice --object orvu/crew/ --since 2h --not-allowed --json") {
            Ok(Command::Audit { mode, filter, json }) => {
                assert_eq!(mode, Mode::Local { data_dir: Some("/d".into()) });
                assert_eq!(filter.agent.as_deref(), Some("alice"));
                assert_eq!(filter.object.as_deref(), Some("orvu/crew/"));
                assert_eq!(filter.since, Some(Duration::from_secs(7200)));
                assert!(filter.not_allowed && json);
            }
            other => panic!("expected an audit, got {other:?}"),
        }
        for (line, says) in [
            ("audit", "one of --local or --config is required"),
            ("audit --local --since soon", "is not a length of time"),
            ("audit --local --agent", "--agent needs a value"),
            ("audit --local --frobnicate", "unknown argument: --frobnicate"),
        ] {
            let message = parse_str(line).expect_err(line);
            assert!(message.contains(says), "{line:?} gave {message:?}");
            assert!(message.contains("usage:"), "{line:?} did not show the usage");
        }
    }

    // A folder name does not have to be valid text on Linux. It is passed
    // through as it is.
    #[test]
    fn a_path_that_is_not_valid_text_is_accepted() {
        use std::os::unix::ffi::OsStringExt;
        let odd = OsString::from_vec(vec![b'/', b'd', 0xff]);
        let args = [OsString::from("--local"), OsString::from("--data-dir"), odd.clone()];
        match parse(args) {
            Ok(Command::Run(options)) => {
                assert_eq!(options.mode, Mode::Local { data_dir: Some(PathBuf::from(odd)) })
            }
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_value_is_a_mistake() {
        let args = [OsString::from("--local"), OsString::from("--data-dir"), OsString::new()];
        let message = parse(args).expect_err("an empty folder name");
        assert!(message.contains("--data-dir needs a value"), "{message}");
    }
}
