//! The sidecar program. It starts the engine, from a settings file or with no
//! setup at all, and serves on a Unix socket.
//!
//! It is kept thin. Everything that matters is in the library, which is what
//! the tests drive. Logging goes through `tracing` macros, and is written
//! only if the settings ask for it (see `orvud::observe`). The one line a
//! background program should always print goes to stderr, whatever the
//! logging setup. Without it a successful start would print nothing, and you
//! could not tell the program was running. Everything it prints there goes
//! through `orvud::say`, which never waits and never fails, so a reader that
//! went away or stopped reading cannot keep it from stopping.

use orvud::cli::{AgentAction, Command, Mode, Options};
use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    let code = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => {
            let code = runtime.block_on(start());
            // Work that cannot be stopped, such as fetching the model for
            // search, is not waited for: the sidecar stops when it is asked to.
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            code
        }
        Err(e) => {
            orvud::say::line(format_args!("orvud: cannot start: {e}"));
            std::process::ExitCode::FAILURE
        }
    };
    // What is still to be printed is printed, unless nobody is reading.
    orvud::say::flush(std::time::Duration::from_secs(1));
    code
}

async fn start() -> std::process::ExitCode {
    let command = match orvud::cli::parse(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            orvud::say::line(format_args!("orvud: {message}"));
            return std::process::ExitCode::FAILURE;
        }
    };
    let options = match command {
        Command::Help => {
            println!("{}", orvud::cli::USAGE);
            return std::process::ExitCode::SUCCESS;
        }
        Command::Version => {
            println!("orvud {}", orvud::VERSION);
            return std::process::ExitCode::SUCCESS;
        }
        Command::Setup { folder, machines, earlier } => {
            for warning in orvud::setup::unresolved(&machines, orvud::setup::resolves) {
                orvud::say::line(format_args!("orvud: {warning}"));
            }
            let made = match &earlier {
                Some(earlier) => orvud::setup::write_after(&folder, &machines, earlier),
                None => orvud::setup::write(&folder, &machines).map(|()| machines),
            };
            return match made {
                Ok(machines) => {
                    println!("{}", orvud::setup::next_steps(&folder, &machines));
                    std::process::ExitCode::SUCCESS
                }
                Err(message) => {
                    orvud::say::line(format_args!("orvud: {message}"));
                    std::process::ExitCode::FAILURE
                }
            };
        }
        Command::AddMachine { folder, machine } => {
            // Looked up first, so that a warning is not lost below the steps.
            for warning in orvud::setup::unresolved(std::slice::from_ref(&machine), orvud::setup::resolves) {
                orvud::say::line(format_args!("orvud: {warning}"));
            }
            return match orvud::setup::add(&folder, machine) {
                Ok(added) => {
                    println!("{}", orvud::setup::added_steps(&folder, &added));
                    std::process::ExitCode::SUCCESS
                }
                Err(message) => {
                    orvud::say::line(format_args!("orvud: {message}"));
                    std::process::ExitCode::FAILURE
                }
            };
        }
        Command::Leave { settings, wait } => {
            return match orvud::boot::leave(&settings, wait).await {
                Ok(leaving) => {
                    println!("{}", left(&leaving, wait));
                    if leaving.done() { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
                }
                Err(e) => {
                    orvud::say::line(format_args!("orvud: cannot leave: {e}"));
                    std::process::ExitCode::FAILURE
                }
            };
        }
        Command::Agent { action, mode } => {
            return match agent(action, &mode) {
                Ok(said) => {
                    println!("{said}");
                    std::process::ExitCode::SUCCESS
                }
                Err(message) => {
                    orvud::say::line(format_args!("orvud: {message}"));
                    std::process::ExitCode::FAILURE
                }
            };
        }
        Command::Audit { mode, filter, json } => {
            let shown = orvud::audit::file_for(&mode, env).and_then(|path| {
                orvud::audit::each(&path, &filter, |line| {
                    println!("{}", if json { line.to_string() } else { orvud::audit::shown(&line) })
                })
            });
            return match shown {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(message) => {
                    orvud::say::line(format_args!("orvud: {message}"));
                    std::process::ExitCode::FAILURE
                }
            };
        }
        Command::Run(options) => options,
    };
    match run(options).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            orvud::say::line(format_args!("orvud: {e}"));
            std::process::ExitCode::FAILURE
        }
    }
}

/// What `orvud leave` did, for the person who ran it.
fn left(leaving: &orvu_runtime::Leaving, wait: std::time::Duration) -> String {
    let mut text = String::new();
    for (budget, to, amount) in &leaving.given {
        text.push_str(&format!("Handed {amount} of {budget} to machine {}.\n", to.0));
    }
    if leaving.given.is_empty() {
        text.push_str("This machine held none of any budget.\n");
    }
    for (budget, amount) in &leaving.kept {
        text.push_str(&format!("Still holds {amount} of {budget}.\n"));
    }
    if leaving.done() {
        text.push_str(
            "The other machines have it all, and give this one nothing from now on. It has left \
             the team: do not start it again under its number. Take its address out of the peers \
             line of the others' settings when they next start. Its certificate is still valid: \
             the others would still let it in.",
        );
    } else {
        text.push_str(&format!(
            "Within {} seconds the other machines were not all heard to have it. Run this again, \
             with a longer --wait, while they are running.",
            wait.as_secs()
        ));
    }
    text
}

fn env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

/// Does what `orvud agent` asked, and says what it did.
fn agent(action: AgentAction, mode: &Mode) -> Result<String, String> {
    let path = orvud::agents::file_for(mode, env)?;
    match action {
        AgentAction::Add(name) => {
            let key = orvud::agents::add(&path, &name)?;
            Ok(format!(
                "Added {name}. Its key is shown only this once:\n\n{key}\n\n\
                 Give it to the agent as ORVU_KEY. From now on the sidecar lets in only agents with a key."
            ))
        }
        AgentAction::AddConnected { name, address, file } => {
            // Checked before the agent is added, so that a file that cannot
            // be written does not leave an agent behind whose key nobody has.
            if file.exists() {
                return Err(format!("{} is there already, and is not written over", file.display()));
            }
            let (certificate, _) = orvud::network::certificate(&orvud::network::folder(&path))?;
            let key = orvud::agents::add(&path, &name)?;
            let connection = orvud::network::Connection { address, key, certificate };
            if let Err(e) = connection.write(&file) {
                let _ = orvud::agents::remove(&path, &name);
                return Err(e);
            }
            // The address the agent dials is not always one the sidecar can
            // listen on, such as a name another machine knows it by. The
            // sidecar listens on every address, on the same port.
            let port = connection.address.rsplit_once(':').map_or("7441", |(_, port)| port);
            Ok(format!(
                "Added {name}, and wrote all it needs to connect over the network to {}. Give it that file, \
                 and keep it as secret as a key: it holds one. For {name} to reach the sidecar at {}, start the \
                 sidecar with --network and an address it can listen on, such as --network 0.0.0.0:{port}.",
                file.display(),
                connection.address
            ))
        }
        AgentAction::Remove(name) => {
            let left = orvud::agents::remove(&path, &name)?;
            let removed = format!("Removed {name}. A program connected as {name} is turned away at its next call.");
            if left > 0 {
                return Ok(removed);
            }
            Ok(format!(
                "{removed}\nNo agents are left, so the sidecar now lets nobody in. To let every program in again, \
                 delete {}.",
                path.display()
            ))
        }
        AgentAction::Allow(name, words) => {
            let rules = match words.as_slice() {
                [all] if all == "all" => None,
                _ => Some(orvud::rules::Rules::parse(&words)?),
            };
            orvud::agents::allow(&path, &name, rules.clone())?;
            Ok(match rules {
                None => format!("{name} has no rules now, and may do everything."),
                Some(rules) => format!("{name} may now do only this: {rules}. A program connected as {name} is held to it from its next call."),
            })
        }
        AgentAction::List => {
            let agents = orvud::agents::read(&path)?;
            if agents.is_empty() && path.exists() {
                return Ok(format!(
                    "No agents, so the sidecar lets nobody in. To let every program in again, delete {}.",
                    path.display()
                ));
            }
            if agents.is_empty() {
                return Ok("No agents. Every program that can reach the socket is let in.".into());
            }
            let line = |agent: orvud::agents::Listed| match agent.rules {
                None => format!("{}: everything", agent.name),
                Some(rules) => format!("{}: {rules}", agent.name),
            };
            Ok(agents.into_iter().map(line).collect::<Vec<_>>().join("\n"))
        }
    }
}

/// The socket to serve on when started from a settings file and `--socket`
/// was not given.
///
/// `$XDG_RUNTIME_DIR` belongs to one user and has mode `0700`, so only that
/// user can reach a socket inside it. `/tmp` is not safe. Its mode is `1777`,
/// so another local user can create `/tmp/orvud.sock` first as their own
/// socket, and the sticky bit then stops us removing it. That user would also
/// be listening on the well-known path every client connects to by default,
/// and a client has no way to tell. So when `$XDG_RUNTIME_DIR` is unset or
/// empty we ask for `--socket` and do not guess.
fn default_socket() -> Result<PathBuf, String> {
    match env("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join("orvud.sock")),
        _ => Err(format!(
            "--socket is required when $XDG_RUNTIME_DIR is unset\n{}",
            orvud::cli::USAGE
        )),
    }
}

async fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    let Options { mode, socket, network, exit_when_idle, model, if_copied } = options;
    // Where the model for search comes from is settled first, so that a
    // setting that cannot be used stops the sidecar before it starts. A
    // sidecar a program starts by itself is given no flags, so the folder can
    // also be named in ORVU_MODEL.
    let model = model.or_else(|| env("ORVU_MODEL").filter(|named| !named.is_empty()).map(Into::into));
    let source = match model {
        Some(_) => String::new(),
        None => model_source()?,
    };

    // Both ways of starting take the page's address and put the socket in
    // place before the engine starts. An address or a socket path that
    // cannot be used is then found before anything is written to disk and
    // before this machine joins the others. A sidecar that kept failing here
    // would otherwise show up and disappear again on every attempt to
    // restart it.
    //
    // Each way gives the engine, the replica id it runs under and the
    // namespace it serves, so nothing clients are told can differ from what is
    // in use. `_held` keeps the lock on the data folder until the end of
    // this function, and the note of where the socket is. The note comes
    // first, so it is taken away while the folder is still held and no other
    // sidecar can have written its own in the meantime.
    let agents_file = orvud::agents::file_for(&mode, env)?;
    let agents = std::sync::Arc::new(orvud::agents::Agents::from_file(agents_file.clone()));
    let audit_file = orvud::audit::file_for(&mode, env)?;
    // The network is set up first, like the socket: only agents with keys
    // are let in over it, so there has to be one, and its address has to be
    // free.
    let network = match network {
        None => None,
        Some(address) => {
            if !agents.names_any() {
                return Err(format!(
                    "--network lets in only agents with keys, and there are none yet: add one with `orvud agent add`\n{}",
                    orvud::cli::USAGE
                )
                .into());
            }
            let acceptor = orvud::network::acceptor(&orvud::network::folder(&agents_file))?;
            let listener = tokio::net::TcpListener::bind(&address)
                .await
                .map_err(|e| format!("cannot serve the network on {address}: {e}"))?;
            Some((listener, acceptor))
        }
    };
    let (socket, bound, client, replica, namespace, _held, page, home) = match mode {
        Mode::Local { data_dir } => {
            let data_dir = match data_dir {
                Some(dir) => dir,
                None => orvud::local::default_data_dir(env)
                    .ok_or("there is no home folder to keep data in: set $HOME, or pass --data-dir")?,
            };
            // The folder is taken first. The socket may go inside it, and a
            // second sidecar on the same folder should be told so before it
            // touches anything.
            let claimed = orvud::local::claim(&data_dir)?;
            // The log starts before the engine, so that what the engine
            // says while it starts is in it.
            let observe = orvud::observe::Observe::local(&data_dir)?;
            observe.start_logs()?;
            let page = observe.bind_page().await?;
            let socket = socket.unwrap_or_else(|| orvud::local::default_socket(&data_dir, env));
            let bound = orvud::server::bind_uds(&socket).await?;
            let local = claimed.start(if_copied).await?;
            let client = local.client.clone();
            // Where the socket is goes in the data folder, so that a program
            // with fewer settings than this one finds it. Not being able to
            // write it is no reason to stop: such a program can be given the
            // socket instead.
            let note = match orvud::local::SocketNote::write(&data_dir, &socket) {
                Ok(note) => Some(note),
                Err(e) => {
                    orvud::say::line(format_args!(
                        "orvud: cannot note the socket in {}: {e}",
                        data_dir.join(orvud::local::SOCKET_NOTE).display()
                    ));
                    None
                }
            };
            let (replica, namespace) = (local.replica, local.namespace.clone());
            (socket, bound, client, replica, namespace, (note, Some(local), None), page, data_dir)
        }
        Mode::Config(path) => {
            let socket = match socket {
                Some(path) => path,
                None => default_socket()?,
            };
            let (mut settings, folder) = orvud::boot::read(&path)?;
            let observe = orvud::observe::Observe::take(&mut settings, &folder)?;
            observe.start_logs()?;
            let page = observe.bind_page().await?;
            // The storage is taken before the socket, so that a second
            // sidecar on it is told so before it puts a socket anywhere.
            let claim = orvud::boot::claim(&mut settings, &folder)?;
            let bound = orvud::server::bind_uds(&socket).await?;
            let (client, replica, namespace) = orvud::boot::start(settings, &folder, &claim).await?;
            (socket, bound, client, replica, namespace, (None, None, Some(claim)), page, folder)
        }
    };
    // The record of who changed what. The data folder is there by now.
    let audit = std::sync::Arc::new(
        orvud::audit::Audit::to_file(audit_file.clone())
            .map_err(|e| format!("cannot open the audit file {}: {e}", audit_file.display()))?,
    );
    // The page is served from here on, until the sidecar stops. Its address
    // was taken before the engine started.
    let connections = orvud::server::Connections::default();
    let stats = std::sync::Arc::new(orvud::stats::Stats::default());
    let stop_page = tokio_util::sync::CancellationToken::new();
    let numbers = orvud::observe::numbers(client.clone(), stats.clone(), connections.clone());
    orvud::observe::serve_page(page, numbers, stop_page.clone());
    let _stop_page = stop_page.drop_guard();
    tracing::info!(
        socket = %socket.display(),
        replica = replica.0,
        namespace = %namespace,
        "orvud serving"
    );
    // Listened for before the line below, so that a script that waits for
    // it and then asks the sidecar to stop finds it ready to.
    let asked = stop_signal();
    // Printed only now, when the socket is in place and the engine is up, so
    // that a script waiting for this line can rely on it.
    let on_network = match &network {
        Some((listener, _)) => format!(" and on the network at {}", listener.local_addr()?),
        None => String::new(),
    };
    orvud::say::line(format_args!(
        "orvud {}: serving on {}{on_network} as replica {} in namespace {namespace:?}",
        orvud::VERSION,
        socket.display(),
        replica.0
    ));

    // Serving the socket stops by itself once nobody has been connected for
    // that long, and stopping it stops the rest.
    if let Some(limit) = exit_when_idle {
        connections.stop_when_idle(limit);
    }
    // One signal to stop stops the socket and the network alike. It also
    // fires once the engine can no longer save: every request is then
    // answered with that reason, and only a restart from what is on disk
    // gets the sidecar going again, so it stops and lets whatever runs it
    // start it again. Requests already in progress are answered first.
    let stopping = tokio_util::sync::CancellationToken::new();
    let signal = stopping.clone();
    let watched = client.clone();
    let why_stopped = std::sync::Arc::new(std::sync::OnceLock::new());
    let reason = why_stopped.clone();
    tokio::spawn(async move {
        let stop = orvud::server::until_stopped(&watched, asked).await;
        if let Some(said) = stop.message() {
            tracing::error!("{said}");
        }
        let _ = reason.set(stop);
        signal.cancel();
    });
    // Search by meaning. Nothing is loaded or fetched until a team searches.
    let from = match model {
        Some(folder) => orvud::search::ModelFrom::Folder(folder),
        None => orvud::search::ModelFrom::Fetched {
            folder: home.join("models").join("potion-base-8M"),
            source,
        },
    };
    let searcher = orvud::search::Searcher::start(client.clone(), from, stopping.clone());
    let local = orvud::server::serve_bound_admitting(
        bound,
        client.clone(),
        replica,
        &namespace,
        connections.clone(),
        stats.clone(),
        agents.clone(),
        audit.clone(),
        searcher.clone(),
        stopping.clone().cancelled_owned(),
    );
    let remote = async {
        match network {
            Some((listener, acceptor)) => {
                let (client, audit, stopped) = (client.clone(), audit.clone(), stopping.clone().cancelled_owned());
                orvud::server::serve_network(
                    listener, acceptor, client, replica, &namespace, connections, stats, agents, audit, searcher,
                    stopped,
                )
                .await
            }
            // Nothing to serve: this waits with the socket.
            None => {
                stopping.cancelled().await;
                Ok(())
            }
        }
    };
    // Whichever stops first, by failing, stops the other. Once stopping,
    // each is given a few seconds to answer what is in progress, and no more.
    let wait = orvud::server::STOP_WAIT;
    let (served, served_network) = tokio::join!(
        async {
            let served = orvud::server::finish_within(local, stopping.clone(), wait).await;
            stopping.cancel();
            served
        },
        async {
            let served = orvud::server::finish_within(remote, stopping.clone(), wait).await;
            stopping.cancel();
            served
        }
    );
    let served = served.and(served_network);
    // Every line still waiting is written before the sidecar stops.
    audit.close();

    // Serving has ended, because we were asked to stop or because it failed.
    // Either way the engine is shut down properly, which saves what has not
    // been written to disk yet and releases the storage file.
    client.shutdown().await;
    orvud::say::line("orvud: stopped");
    served?;
    // A sidecar that stopped because it cannot save exits with a failure, so
    // that a supervisor sees it and starts it again.
    if let Some(said) = why_stopped.get().and_then(orvud::server::Stop::message) {
        return Err(said.into());
    }
    Ok(())
}

/// Where the model is fetched from: ORVU_MODEL_SOURCE, another place such as
/// a copy inside a network with no internet, or the one it comes from. Each
/// file is checked against its pin all the same.
fn model_source() -> Result<String, String> {
    let Some(source) = env("ORVU_MODEL_SOURCE").filter(|source| !source.is_empty()) else {
        return Ok(orvu_search::POTION_SOURCE.to_string());
    };
    match source.into_string() {
        Ok(source) if source.starts_with("http://") || source.starts_with("https://") => {
            Ok(if source.ends_with('/') { source } else { format!("{source}/") })
        }
        Ok(source) => Err(format!("ORVU_MODEL_SOURCE has to be a web address, starting with http:// or https://, not {source}")),
        Err(source) => Err(format!("ORVU_MODEL_SOURCE has to be a web address, not {}", source.to_string_lossy())),
    }
}

/// Finishes when the process is asked to stop: Ctrl-C (SIGINT), or SIGTERM,
/// which is what a service manager such as systemd sends. The signals are
/// listened for from the moment this is called, not from when it is first
/// waited on, so a signal sent right after the sidecar says it is serving
/// stops it cleanly too.
fn stop_signal() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    let interrupt = signal(SignalKind::interrupt());
    let terminate = signal(SignalKind::terminate());
    async move {
        let mut interrupt = match interrupt {
            Ok(s) => s,
            Err(e) => {
                orvud::say::line(format_args!("orvud: cannot listen for Ctrl-C ({e})"));
                std::future::pending::<()>().await;
                return;
            }
        };
        let mut terminate = match terminate {
            Ok(s) => s,
            Err(e) => {
                orvud::say::line(format_args!("orvud: cannot listen for SIGTERM ({e}); only Ctrl-C will stop it cleanly"));
                interrupt.recv().await;
                return;
            }
        };
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    }
}
