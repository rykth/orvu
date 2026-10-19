//! What the sidecar shows of itself: a log, and a page of numbers that a
//! tool such as Prometheus can collect. Both are off unless the settings turn
//! them on.
//!
//! Started from a settings file, they are its `[logs]` and `[metrics]`
//! sections. Started with `--local`, which needs no settings, they can be put
//! in a small file of their own in the data folder, `orvud.toml`. That file
//! holds those two sections and one more, `[storage]`, which says how often a
//! local sidecar saves to disk.

use orvu_sdk::config::{LogsConfig, MetricsConfig};
use orvu_runtime::FsyncPolicy;
use orvu_sdk::SdkConfig;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The name of the settings file in the data folder, for `--local`.
pub const LOCAL_SETTINGS: &str = "orvud.toml";

/// The sections `orvud.toml` may hold.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LocalSettings {
    logs: LogsConfig,
    metrics: MetricsConfig,
    storage: LocalStorage,
}

/// The `[storage]` section of `orvud.toml`: how often a local sidecar saves.
/// The words are the same as in a full settings file, but the default is not:
/// a local sidecar saves each change before it answers.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LocalStorage {
    /// `"always"`: each change is on disk before it is answered. `"interval"`:
    /// changes are saved together every `interval_ms`, which is faster, but a
    /// change answered just before the sidecar is killed can be lost.
    fsync: String,
    interval_ms: u64,
}

impl Default for LocalStorage {
    fn default() -> Self {
        LocalStorage { fsync: "always".into(), interval_ms: 100 }
    }
}

impl LocalStorage {
    fn policy(&self) -> Result<FsyncPolicy, String> {
        match self.fsync.as_str() {
            "always" => Ok(FsyncPolicy::Always),
            "interval" if self.interval_ms == 0 => {
                Err("[storage] interval_ms must be greater than 0".into())
            }
            "interval" => Ok(FsyncPolicy::Interval { ms: self.interval_ms }),
            other => Err(format!("[storage] fsync must be \"always\" or \"interval\", not {other:?}")),
        }
    }
}

/// Reads `orvud.toml` in the data folder. With no such file, every setting
/// takes its default.
fn read_local(data_dir: &Path) -> Result<LocalSettings, String> {
    let path = data_dir.join(LOCAL_SETTINGS);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<LocalSettings>(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LocalSettings::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// How often a local sidecar saves to disk, from `orvud.toml` in the data
/// folder. Unless the file says otherwise, each change is saved before it is
/// answered.
pub fn local_fsync(data_dir: &Path) -> Result<FsyncPolicy, String> {
    let path = data_dir.join(LOCAL_SETTINGS);
    read_local(data_dir)?.storage.policy().map_err(|e| format!("{}: {e}", path.display()))
}

/// What to show, read and checked. Nothing is started until it is asked for.
#[derive(Debug)]
pub struct Observe {
    level: Option<tracing::Level>,
    log_file: Option<PathBuf>,
    metrics_addr: Option<String>,
}

impl Observe {
    /// Reads `orvud.toml` in the data folder. With no such file, nothing is
    /// shown.
    ///
    /// The `[storage]` section is checked here too, so a mistake in it stops
    /// the sidecar before anything starts. It is used by `local_fsync`.
    pub fn local(data_dir: &Path) -> Result<Observe, String> {
        let path = data_dir.join(LOCAL_SETTINGS);
        let settings = read_local(data_dir)?;
        settings.storage.policy().map_err(|e| format!("{}: {e}", path.display()))?;
        Observe::new(settings.logs, settings.metrics.addr, data_dir)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Takes the `[logs]` and `[metrics]` sections out of settings read from
    /// a file in `folder`. The engine is then started without them: the
    /// sidecar serves the page itself, with its own numbers next to the
    /// engine's.
    pub fn take(settings: &mut SdkConfig, folder: &Path) -> Result<Observe, String> {
        let logs = std::mem::take(&mut settings.logs);
        Observe::new(logs, settings.metrics.addr.take(), folder)
    }

    fn new(logs: LogsConfig, metrics_addr: Option<String>, folder: &Path) -> Result<Observe, String> {
        let level = match logs.level.as_deref() {
            None => None,
            Some("error") => Some(tracing::Level::ERROR),
            Some("warn") => Some(tracing::Level::WARN),
            Some("info") => Some(tracing::Level::INFO),
            Some("debug") => Some(tracing::Level::DEBUG),
            Some("trace") => Some(tracing::Level::TRACE),
            Some(other) => {
                return Err(format!(
                    "[logs] level must be \"error\", \"warn\", \"info\", \"debug\" or \"trace\", not {other:?}"
                ))
            }
        };
        // A file with no level would be a log that is never written.
        if level.is_none() && logs.file.is_some() {
            return Err("[logs] file needs a level as well, such as level = \"info\"".into());
        }
        let log_file = logs.file.map(|file| folder.join(file));
        Ok(Observe { level, log_file, metrics_addr })
    }

    /// How much is logged, or `None` for no log.
    pub fn level(&self) -> Option<tracing::Level> {
        self.level
    }

    /// The file the log is added to, or `None` for standard error.
    pub fn log_file(&self) -> Option<PathBuf> {
        self.log_file.clone()
    }

    /// Where the page is served, or `None` for no page.
    pub fn metrics_addr(&self) -> Option<&str> {
        self.metrics_addr.as_deref()
    }

    /// Which log lines are written, or `None` for no log. The sidecar and
    /// the engine write theirs at the level asked for. The libraries
    /// underneath write theirs only from `warn` up, because at the detailed
    /// levels they say a great deal about every message.
    pub fn log_filter(&self) -> Option<tracing_subscriber::filter::Targets> {
        let level = self.level?;
        let theirs = std::cmp::min(level, tracing::Level::WARN);
        // Every target of this project starts with "orvu".
        Some(tracing_subscriber::filter::Targets::new().with_target("orvu", level).with_default(theirs))
    }

    /// Starts writing the log, if there is one. Call it once, before the
    /// engine starts, so that what the engine says while it starts is in
    /// it.
    pub fn start_logs(&self) -> Result<(), String> {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let Some(filter) = self.log_filter() else { return Ok(()) };
        let logs = tracing_subscriber::fmt();
        let started = match &self.log_file {
            Some(path) => {
                use std::os::unix::fs::OpenOptionsExt;
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(path)
                    .map_err(|e| format!("cannot open the log file {}: {e}", path.display()))?;
                logs.with_ansi(false).with_writer(std::sync::Mutex::new(file)).finish().with(filter).try_init()
            }
            // Standard error is written so that a reader who stopped reading
            // cannot hold the sidecar up.
            None => logs.with_writer(crate::say::Writer).finish().with(filter).try_init(),
        };
        started.map_err(|e| format!("cannot start the log: {e}"))
    }

    /// Takes the page's address, if there is a page. Call it before the
    /// engine starts: an address that is in use then stops the start before
    /// anything is written and before this machine joins the others.
    pub async fn bind_page(&self) -> Result<Option<tokio::net::TcpListener>, String> {
        let Some(addr) = &self.metrics_addr else { return Ok(None) };
        tokio::net::TcpListener::bind(addr)
            .await
            .map(Some)
            .map_err(|e| format!("cannot serve the metrics page on {addr}: {e}"))
    }
}

/// Serves the page on the address `bind_page` took, if there is one, until
/// `stop` is cancelled. Each time it is asked for, `render` gives its text.
pub fn serve_page(
    listener: Option<tokio::net::TcpListener>,
    render: orvu_runtime::Render,
    stop: tokio_util::sync::CancellationToken,
) {
    if let Some(listener) = listener {
        tokio::spawn(orvu_runtime::serve_page(listener, render, stop));
    }
}

impl Observe {
}

/// The page: the engine's numbers, then the sidecar's own.
pub fn numbers(
    client: Arc<orvu_sdk::Client>,
    stats: Arc<crate::stats::Stats>,
    connections: crate::server::Connections,
) -> orvu_runtime::Render {
    Arc::new(move || orvu_runtime::render_prometheus(&client.metrics()) + &stats.render(connections.open()))
}
