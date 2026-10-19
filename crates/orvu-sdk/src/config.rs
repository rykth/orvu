//! Settings read from a TOML file and turned into a `NodeConfig`.

use crate::error::{Result, SdkError};
use orvu_crdt::ReplicaId;
use orvu_runtime::{Acl, Action, FsyncPolicy, NodeConfig};
use orvu_transport::PeerAddr;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

/// The `[security]` section: paths to this node's certificate, its private key
/// and the CA certificate.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca: PathBuf,
}

/// The `[storage]` section: where and how changes are saved to disk.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    /// Path of the log on disk. Leave it out to keep everything in memory and
    /// save nothing.
    pub path: Option<String>,
    /// `"always"` or `"interval"`. Any value other than `"always"` is treated
    /// as `"interval"`.
    pub fsync: String,
    /// How often to flush to disk, in milliseconds, when `fsync = "interval"`.
    pub interval_ms: u64,
    /// How many deltas (small changes) to save between snapshots. `0` turns
    /// compaction off.
    pub compaction_threshold: usize,
    /// Take on saved data that has no note of the machine number it was
    /// written under, and note this machine's number beside it. Such data is
    /// refused otherwise, because it is not known whose writes it holds. Only
    /// for data known to have been written under this machine's number.
    pub adopt_data: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: None,
            fsync: "interval".to_string(),
            interval_ms: 100,
            compaction_threshold: 10_000,
            adopt_data: false,
        }
    }
}

/// The `[membership]` section: how long to wait before giving up on a member
/// that has gone silent.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MembershipConfig {
    /// How long, in milliseconds, a member this node has no direct connection
    /// to may go without its heartbeat moving forward before it is marked
    /// `Suspect`. If left out, it is twice the catch-up interval
    /// (`anti_entropy_interval_ms`), the same as in `NodeConfig::new`. Zero
    /// is refused.
    pub suspect_ms: Option<u64>,
    /// How long, in milliseconds, a `Suspect` member stays before it is
    /// removed. If left out, it is twice the catch-up interval, the same as in
    /// `NodeConfig::new`. Zero is refused.
    pub dead_ms: Option<u64>,
    /// How long, in milliseconds, a removed member is remembered. While it
    /// is, news about it is taken only with a newer heartbeat than the last
    /// one heard, so a machine restarted with its clock behind that may not
    /// be taken back until this has passed. If left out, it is 24 hours, the
    /// same as in `NodeConfig::new`. Zero is refused.
    pub forget_ms: Option<u64>,
}

/// The `[link]` section: the time limits of the links to other machines. Each
/// one left out keeps the engine's own value.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LinkConfig {
    /// How long, in milliseconds, a link on which nothing at all is heard is
    /// kept before it is given up. 30 seconds if left out.
    pub idle_ms: Option<u64>,
    /// How often, in milliseconds, a link with nothing to send sends a tiny
    /// packet so that it is not given up. It must be less than `idle_ms`.
    /// 5 seconds if left out.
    pub keep_alive_ms: Option<u64>,
    /// The longest one dial may take, in milliseconds. 10 seconds if left out.
    pub dial_ms: Option<u64>,
    /// How long, in milliseconds, a connected machine may send nothing before
    /// it is let go and dialed again. Five times `anti_entropy_interval_ms`
    /// if left out. It must be at least three times
    /// `anti_entropy_interval_ms`, and at least 1000.
    pub quiet_ms: Option<u64>,
    /// The longest wait, in milliseconds, between two dials to a machine that
    /// cannot be reached. 5 seconds if left out.
    pub longest_dial_wait_ms: Option<u64>,
}

/// The `[metrics]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// A host:port to serve `GET /metrics` on. If left out, there is no such
    /// endpoint.
    pub addr: Option<String>,
}

/// The `[logs]` section. The engine does not read it. It is there for the
/// program that runs the engine, such as the sidecar, which writes its log
/// as this section says.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogsConfig {
    /// How much to write: `"error"`, `"warn"`, `"info"`, `"debug"` or
    /// `"trace"`. If left out, nothing is written.
    pub level: Option<String>,
    /// The file to write to, added to at the end. A path that is not
    /// absolute is taken from the folder the settings file is in. If left
    /// out, the log goes to standard error.
    pub file: Option<String>,
}

/// The `[budget]` section: how the machines share out what a bounded counter,
/// such as a budget, has left.
///
/// Each machine can only spend its own share of a bounded counter. These
/// settings decide how a machine that is short gets more.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BudgetConfig {
    /// How often, in milliseconds, this machine evens the shares out ahead
    /// of need, so that a machine that spends finds its share already there.
    /// Leave it out to have no evening out: a machine that runs short then
    /// asks another for more at that moment and waits for the answer.
    pub rebalance_ms: Option<u64>,
    /// How long, in milliseconds, a machine that is short waits for another
    /// machine to lend it some before it says no. Leave it out for the
    /// engine's own wait, which is two seconds.
    pub borrow_wait_ms: Option<u64>,
}

/// The whole config file. A field or section it does not know is an error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SdkConfig {
    pub replica: u64,
    pub namespace: String,
    pub listen: String,
    #[serde(default)]
    pub peers: Vec<String>,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub membership: MembershipConfig,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub link: LinkConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub logs: LogsConfig,
    #[serde(default)]
    pub security: Option<TlsConfig>,
    #[serde(default)]
    pub acl: Option<BTreeMap<String, Vec<String>>>,
}

/// The `[sync]` section: timing and queue sizes for exchanging changes with
/// other machines.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncConfig {
    pub delta_batch_ms: u64,
    pub anti_entropy_interval_ms: u64,
    pub ship_queue_cap: usize,
    pub causal_buffer_cap: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        // The same defaults as `NodeConfig::new`.
        Self {
            delta_batch_ms: 10,
            anti_entropy_interval_ms: 1000,
            ship_queue_cap: 4096,
            causal_buffer_cap: 4096,
        }
    }
}

impl SdkConfig {
    /// Parses the text of a TOML config file.
    pub fn parse_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| SdkError::Config(e.to_string()))
    }

    /// Turns this config into a `NodeConfig`, with the access list from the
    /// `[acl]` section applied. The `[security]` section is not used here,
    /// because it belongs to the network link and not to the node.
    ///
    /// Returns a config error for a setting the engine could not run with: a
    /// zero in `[sync]` for `delta_batch_ms`, `anti_entropy_interval_ms` or
    /// `ship_queue_cap`, a zero for `rebalance_ms` in `[budget]` or for any
    /// setting in `[link]`, a zero for any setting in `[membership]`, a
    /// `keep_alive_ms` not below `idle_ms`, a
    /// `quiet_ms` below three times `anti_entropy_interval_ms` or below 1000,
    /// or an `fsync` that is neither `"always"` nor `"interval"`.
    pub fn into_node_config(self) -> Result<NodeConfig> {
        for (name, value) in [
            ("delta_batch_ms", self.sync.delta_batch_ms),
            ("anti_entropy_interval_ms", self.sync.anti_entropy_interval_ms),
            ("ship_queue_cap", self.sync.ship_queue_cap as u64),
        ] {
            if value == 0 {
                return Err(SdkError::Config(format!("[sync] {name} must be greater than 0")));
            }
        }
        let fsync = match self.storage.fsync.as_str() {
            "always" => FsyncPolicy::Always,
            "interval" => FsyncPolicy::Interval { ms: self.storage.interval_ms },
            other => {
                return Err(SdkError::Config(format!(
                    "[storage] fsync must be \"always\" or \"interval\", not {other:?}"
                )))
            }
        };
        // A timer that fires every 0 milliseconds would never let anything
        // else run. Leaving the setting out is how evening out is turned off.
        if self.budget.rebalance_ms == Some(0) {
            return Err(SdkError::Config(
                "[budget] rebalance_ms must be greater than 0. Leave it out to turn it off".into(),
            ));
        }
        // A member that may stay quiet for no time at all would be suspected,
        // and dropped, as soon as it was looked at. A dropped member
        // remembered for no time at all would be forgotten at once, and old
        // news about it would bring it back.
        for (name, value) in [
            ("suspect_ms", self.membership.suspect_ms),
            ("dead_ms", self.membership.dead_ms),
            ("forget_ms", self.membership.forget_ms),
        ] {
            if value == Some(0) {
                return Err(SdkError::Config(format!("[membership] {name} must be greater than 0")));
            }
        }
        let link = &self.link;
        for (name, value) in [
            ("idle_ms", link.idle_ms),
            ("keep_alive_ms", link.keep_alive_ms),
            ("dial_ms", link.dial_ms),
            ("quiet_ms", link.quiet_ms),
            ("longest_dial_wait_ms", link.longest_dial_wait_ms),
        ] {
            if value == Some(0) {
                return Err(SdkError::Config(format!("[link] {name} must be greater than 0")));
            }
        }
        let acl = self.build_acl()?;

        let mut cfg = NodeConfig::new(ReplicaId(self.replica), self.listen);
        cfg.acl = acl;
        cfg.namespace = self.namespace;
        cfg.peers = self.peers.into_iter().map(PeerAddr::new).collect();
        cfg.delta_batch = Duration::from_millis(self.sync.delta_batch_ms);
        cfg.anti_entropy_interval = Duration::from_millis(self.sync.anti_entropy_interval_ms);
        cfg.suspect_timeout = match self.membership.suspect_ms {
            Some(ms) => Duration::from_millis(ms),
            None => cfg.anti_entropy_interval * 2,
        };
        cfg.dead_timeout = match self.membership.dead_ms {
            Some(ms) => Duration::from_millis(ms),
            None => cfg.anti_entropy_interval * 2,
        };
        if let Some(ms) = self.membership.forget_ms {
            cfg.forget_removed_after = Duration::from_millis(ms);
        }
        cfg.rebalance_interval = self.budget.rebalance_ms.map(Duration::from_millis);
        if let Some(ms) = self.budget.borrow_wait_ms {
            cfg.borrow_timeout = Duration::from_millis(ms);
        }
        cfg.metrics_addr = self.metrics.addr;
        cfg.ship_queue_cap = self.sync.ship_queue_cap;
        cfg.causal_buffer_cap = self.sync.causal_buffer_cap;
        cfg.store_path = self.storage.path.map(PathBuf::from);
        cfg.fsync = fsync;
        cfg.compaction_threshold = self.storage.compaction_threshold;
        let ms = Duration::from_millis;
        if let Some(v) = self.link.idle_ms {
            cfg.link.idle = ms(v);
        }
        if let Some(v) = self.link.keep_alive_ms {
            cfg.link.keep_alive = ms(v);
        }
        if let Some(v) = self.link.dial_ms {
            cfg.link.dial = ms(v);
        }
        cfg.quiet_limit = self.link.quiet_ms.map(ms);
        if let Some(v) = self.link.longest_dial_wait_ms {
            cfg.longest_dial_wait = ms(v);
        }
        // A healthy connection with nothing to carry sends something about
        // once a catch-up interval. A quiet limit close to that would close
        // it now and then for no reason, and dial it again. Below a second,
        // a machine only busy for a moment would be let go.
        if let Some(quiet) = cfg.quiet_limit {
            let lowest = cfg.lowest_quiet_limit();
            if quiet < lowest {
                return Err(SdkError::Config(format!(
                    "[link] quiet_ms ({} ms) must be at least {} times [sync] anti_entropy_interval_ms and at least {} ms, so at least {} ms",
                    quiet.as_millis(),
                    orvu_runtime::QUIET_INTERVALS_AT_LEAST,
                    orvu_runtime::QUIET_LIMIT_AT_LEAST.as_millis(),
                    lowest.as_millis()
                )));
            }
        }
        // A link that sends its sign of life no sooner than it would be given
        // up for silence is given up even when it is healthy.
        if cfg.link.keep_alive >= cfg.link.idle {
            return Err(SdkError::Config(format!(
                "[link] keep_alive_ms ({} ms) must be less than idle_ms ({} ms)",
                cfg.link.keep_alive.as_millis(),
                cfg.link.idle.as_millis()
            )));
        }
        Ok(cfg)
    }

    /// Builds the access list from the `[acl]` table. With no `[acl]` table,
    /// every machine is allowed.
    pub fn build_acl(&self) -> Result<Acl> {
        let Some(raw) = &self.acl else { return Ok(Acl::open()) };
        let mut map = BTreeMap::new();
        for (k, actions) in raw {
            let id = k
                .parse::<u64>()
                .map_err(|_| SdkError::Config(format!("bad acl replica id: {k}")))?;
            let mut set = BTreeSet::new();
            for a in actions {
                let action = match a.as_str() {
                    "sync" => Action::Sync,
                    "write" => Action::Write,
                    other => return Err(SdkError::Config(format!("unknown acl action: {other}"))),
                };
                set.insert(action);
            }
            map.insert(ReplicaId(id), set);
        }
        Ok(Acl::allow_list(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 7
            namespace = "shopping"
            listen = "127.0.0.1:7440"
            peers = ["127.0.0.1:7441"]
            [sync]
            delta_batch_ms = 5
            anti_entropy_interval_ms = 250
            ship_queue_cap = 8
            causal_buffer_cap = 8
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert_eq!(nc.replica, ReplicaId(7));
        assert_eq!(nc.namespace, "shopping");
        assert_eq!(nc.peers, vec![PeerAddr::new("127.0.0.1:7441")]);
        assert_eq!(nc.delta_batch, Duration::from_millis(5));
    }

    fn with_link(section: &str) -> Result<NodeConfig> {
        SdkConfig::parse_toml(&format!(
            "replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n[sync]\nanti_entropy_interval_ms = 200\n[link]\n{section}\n"
        ))?
        .into_node_config()
    }

    #[test]
    fn the_link_section_sets_the_time_limits() {
        let nc = with_link(
            "idle_ms = 3000\nkeep_alive_ms = 500\ndial_ms = 2000\nquiet_ms = 1500\nlongest_dial_wait_ms = 1000",
        )
        .unwrap();
        assert_eq!(nc.link.idle, Duration::from_millis(3000));
        assert_eq!(nc.link.keep_alive, Duration::from_millis(500));
        assert_eq!(nc.link.dial, Duration::from_millis(2000));
        assert_eq!(nc.quiet_limit(), Duration::from_millis(1500));
        assert_eq!(nc.longest_dial_wait, Duration::from_millis(1000));
    }

    #[test]
    fn the_link_section_is_optional_and_keeps_the_engine_defaults() {
        let nc = with_link("").unwrap();
        let engine = NodeConfig::new(ReplicaId(1), "x");
        assert_eq!(nc.link, engine.link);
        assert_eq!(nc.longest_dial_wait, engine.longest_dial_wait);
        assert_eq!(nc.quiet_limit(), Duration::from_millis(1000), "five catch-up intervals");
    }

    #[test]
    fn a_zero_link_time_is_refused() {
        for name in ["idle_ms", "keep_alive_ms", "dial_ms", "quiet_ms", "longest_dial_wait_ms"] {
            let err = with_link(&format!("{name} = 0")).unwrap_err();
            assert!(err.to_string().contains(name), "{name}: {err}");
        }
    }

    // A quiet limit close to the catch-up interval closes healthy links that
    // only have nothing to say, and dials them again, over and over.
    #[test]
    fn a_quiet_limit_below_three_catch_up_intervals_is_refused() {
        let with = |quiet: u64| {
            SdkConfig::parse_toml(&format!(
                "replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n[sync]\nanti_entropy_interval_ms = 400\n[link]\nquiet_ms = {quiet}\n"
            ))?
            .into_node_config()
        };
        // The interval is 400 ms, so the lowest allowed is 1200.
        let err = with(1100).unwrap_err().to_string();
        assert!(err.contains("quiet_ms") && err.contains("anti_entropy_interval_ms"), "{err}");
        let err = with(1199).unwrap_err().to_string();
        assert!(err.contains("1200"), "the message should say the lowest value allowed: {err}");
        assert_eq!(with(1200).unwrap().quiet_limit(), Duration::from_millis(1200));
    }

    // Three short catch-up intervals is still too short a time to wait on a
    // machine that is only busy for a moment.
    #[test]
    fn a_quiet_limit_below_one_second_is_refused() {
        // The interval in `with_link` is 200 ms, so three of them is 600.
        let err = with_link("quiet_ms = 999").unwrap_err().to_string();
        assert!(err.contains("quiet_ms") && err.contains("1000"), "{err}");
        assert_eq!(with_link("quiet_ms = 1000").unwrap().quiet_limit(), Duration::from_millis(1000));
    }

    #[test]
    fn a_keep_alive_that_is_not_below_the_idle_time_is_refused() {
        let err = with_link("idle_ms = 1000\nkeep_alive_ms = 1000").unwrap_err();
        assert!(err.to_string().contains("keep_alive_ms"), "{err}");
        // Only one given: checked against the other's default.
        assert!(with_link("idle_ms = 4000").unwrap_err().to_string().contains("keep_alive_ms"));
        assert!(with_link("keep_alive_ms = 30000").is_err());
    }

    #[test]
    fn sync_section_is_optional_and_defaults() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert_eq!(nc.delta_batch, Duration::from_millis(10));
        assert!(nc.peers.is_empty());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            bogus = true
            "#,
        );
        assert!(matches!(err, Err(SdkError::Config(_))));
    }

    #[test]
    fn parses_storage_section() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            [storage]
            path = "/var/lib/orvu/n.redb"
            fsync = "always"
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert_eq!(
            nc.store_path.as_deref(),
            Some(std::path::Path::new("/var/lib/orvu/n.redb"))
        );
        assert_eq!(nc.fsync, orvu_runtime::FsyncPolicy::Always);
    }

    #[test]
    fn storage_defaults_to_in_memory_interval() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert!(nc.store_path.is_none());
        assert_eq!(nc.fsync, orvu_runtime::FsyncPolicy::Interval { ms: 100 });
    }

    #[test]
    fn parses_compaction_threshold() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            [storage]
            path = "/tmp/n.redb"
            compaction_threshold = 500
            "#,
        )
        .unwrap();
        assert_eq!(cfg.into_node_config().unwrap().compaction_threshold, 500);
    }

    #[test]
    fn compaction_threshold_defaults_to_10k() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.into_node_config().unwrap().compaction_threshold, 10_000);
    }

    #[test]
    fn parses_membership_timeouts() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            [membership]
            suspect_ms = 250
            dead_ms = 400
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert_eq!(nc.suspect_timeout, Duration::from_millis(250));
        assert_eq!(nc.dead_timeout, Duration::from_millis(400));
    }

    // A machine restarted with its clock behind the last heartbeat it sent
    // is not taken back until the others forget it was dropped, so how long
    // that is has to be something a settings file can change.
    #[test]
    fn the_membership_section_sets_how_long_a_dropped_member_is_remembered() {
        let parse = |extra: &str| {
            SdkConfig::parse_toml(&format!("replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n{extra}"))
                .unwrap()
                .into_node_config()
                .unwrap()
        };
        assert_eq!(parse("[membership]\nforget_ms = 60000\n").forget_removed_after, Duration::from_secs(60));
        let engine = NodeConfig::new(ReplicaId(1), "x");
        assert_eq!(parse("").forget_removed_after, engine.forget_removed_after, "the engine's own when left out");
    }

    #[test]
    fn membership_timeouts_default_to_two_intervals() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            [sync]
            anti_entropy_interval_ms = 250
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert_eq!(nc.suspect_timeout, Duration::from_millis(500));
        assert_eq!(nc.dead_timeout, Duration::from_millis(500));
    }

    #[test]
    fn membership_timeouts_default_with_default_interval() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert_eq!(nc.suspect_timeout, Duration::from_millis(2000));
        assert_eq!(nc.dead_timeout, Duration::from_millis(2000));
    }

    #[test]
    fn metrics_addr_maps_from_toml() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:7000"
            [metrics]
            addr = "127.0.0.1:9090"
            "#,
        )
        .unwrap();
        let nc = cfg.into_node_config().unwrap();
        assert_eq!(nc.metrics_addr.as_deref(), Some("127.0.0.1:9090"));
    }

    #[test]
    fn metrics_addr_absent_is_none() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:7000"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.into_node_config().unwrap().metrics_addr, None);
    }

    #[test]
    fn the_logs_section_is_read_and_is_off_when_left_out() {
        let base = "replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:7000\"\n";
        let cfg = SdkConfig::parse_toml(base).unwrap();
        assert_eq!((cfg.logs.level, cfg.logs.file), (None, None));
        let cfg = SdkConfig::parse_toml(&format!("{base}[logs]\nlevel = \"info\"\nfile = \"orvud.log\"\n")).unwrap();
        assert_eq!(cfg.logs.level.as_deref(), Some("info"));
        assert_eq!(cfg.logs.file.as_deref(), Some("orvud.log"));
        // A setting it does not know is a mistake, as in every other section.
        let err = SdkConfig::parse_toml(&format!("{base}[logs]\nlevl = \"info\"\n")).unwrap_err();
        assert!(err.to_string().contains("levl"), "{err}");
    }

    #[test]
    fn parses_security_section() {
        let cfg = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:7440"
            [security]
            cert = "/etc/orvu/node1.pem"
            key  = "/etc/orvu/node1-key.pem"
            ca   = "/etc/orvu/ca.pem"
            "#,
        ).unwrap();
        let tls = cfg.security.as_ref().expect("security parsed");
        assert_eq!(tls.cert, std::path::PathBuf::from("/etc/orvu/node1.pem"));
        assert_eq!(tls.ca, std::path::PathBuf::from("/etc/orvu/ca.pem"));
    }

    #[test]
    fn parses_acl_section() {
        use orvu_crdt::ReplicaId;
        use orvu_runtime::Action;
        let sdk = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:7440"
            [acl]
            2 = ["sync", "write"]
            3 = ["sync"]
            "#,
        )
        .unwrap();
        let acl = sdk.build_acl().unwrap();
        assert!(acl.permits(ReplicaId(2), Action::Write));
        assert!(acl.permits(ReplicaId(3), Action::Sync));
        assert!(!acl.permits(ReplicaId(3), Action::Write));
        assert!(!acl.permits(ReplicaId(4), Action::Sync));
    }

    #[test]
    fn rejects_unknown_acl_action() {
        let sdk = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:7440"
            [acl]
            2 = ["admin"]
            "#,
        )
        .unwrap();
        assert!(sdk.build_acl().is_err());
    }

    /// `SdkConfig` refuses anything it does not know, so a section with an
    /// unknown name is an error and is never quietly ignored.
    #[test]
    fn an_unknown_section_is_a_config_error() {
        let err = SdkConfig::parse_toml(
            r#"
            replica = 1
            namespace = "n"
            listen = "127.0.0.1:0"
            [extras]
            anything = 1
            "#,
        );
        assert!(matches!(err, Err(SdkError::Config(_))));
    }

    fn with_section(section: &str) -> Result<NodeConfig> {
        let text = format!("replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n{section}\n");
        SdkConfig::parse_toml(&text)?.into_node_config()
    }

    // A zero here would crash the engine or quietly stop it syncing, so it is
    // refused when the settings are read.
    #[test]
    fn zero_sync_settings_are_config_errors() {
        for bad in ["delta_batch_ms = 0", "anti_entropy_interval_ms = 0", "ship_queue_cap = 0"] {
            let got = with_section(&format!("[sync]\n{bad}"));
            assert!(matches!(got, Err(SdkError::Config(_))), "{bad} was accepted");
        }
        assert!(with_section("[sync]\ndelta_batch_ms = 1").is_ok());
    }

    // A member that may stay quiet for no time at all is suspected, and
    // dropped, as soon as it is looked at, however well it is doing.
    #[test]
    fn a_zero_suspect_or_dead_time_is_a_config_error() {
        for name in ["suspect_ms", "dead_ms"] {
            let got = with_section(&format!("[membership]\n{name} = 0"));
            let Err(SdkError::Config(msg)) = got else { panic!("{name} = 0 was accepted: {got:?}") };
            assert!(msg.contains(&format!("[membership] {name}")), "{msg}");
            assert!(with_section(&format!("[membership]\n{name} = 1")).is_ok());
        }
    }

    // With no time to remember it, a dropped member would be forgotten at
    // once, and old news about it would bring it straight back.
    #[test]
    fn a_zero_forget_time_is_a_config_error() {
        let got = with_section("[membership]\nforget_ms = 0");
        let Err(SdkError::Config(msg)) = got else { panic!("forget_ms = 0 was accepted: {got:?}") };
        assert!(msg.contains("[membership] forget_ms"), "{msg}");
        assert!(with_section("[membership]\nforget_ms = 1").is_ok());
    }

    #[test]
    fn an_unknown_fsync_value_is_a_config_error() {
        let got = with_section("[storage]\nfsync = \"alway\"");
        assert!(matches!(got, Err(SdkError::Config(_))), "a typo was accepted");
        assert!(with_section("[storage]\nfsync = \"always\"").is_ok());
        assert!(with_section("[storage]\nfsync = \"interval\"").is_ok());
    }

    #[test]
    fn budget_settings_are_read() {
        let cfg = with_section("[budget]\nrebalance_ms = 500\nborrow_wait_ms = 250").unwrap();
        assert_eq!(cfg.rebalance_interval, Some(Duration::from_millis(500)));
        assert_eq!(cfg.borrow_timeout, Duration::from_millis(250));
    }

    // With no `[budget]` section a machine behaves as it did before there
    // was one: no evening out, and a two-second wait to borrow.
    #[test]
    fn budget_settings_default_to_what_the_engine_does_by_itself() {
        let cfg = with_section("").unwrap();
        let plain = NodeConfig::new(ReplicaId(1), "127.0.0.1:0");
        assert_eq!(cfg.rebalance_interval, plain.rebalance_interval);
        assert_eq!(cfg.rebalance_interval, None);
        assert_eq!(cfg.borrow_timeout, plain.borrow_timeout);
        // The wait is the engine's own unless the file gives one. It is not
        // written down a second time here.
        let read = SdkConfig::parse_toml("replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n").unwrap();
        assert_eq!(read.budget.borrow_wait_ms, None);
    }

    #[test]
    fn a_zero_rebalance_time_and_a_mistyped_budget_setting_are_config_errors() {
        for bad in ["rebalance_ms = 0", "rebalance = 500", "rebalance_ms = \"often\""] {
            let got = with_section(&format!("[budget]\n{bad}"));
            assert!(matches!(got, Err(SdkError::Config(_))), "{bad} was accepted");
        }
        // Not waiting at all is a fair choice: say no at once.
        assert!(with_section("[budget]\nborrow_wait_ms = 0").is_ok());
    }

    // The access list must come out of the conversion itself. A caller who
    // forgot a second step would otherwise get a node open to everyone.
    #[test]
    fn into_node_config_applies_the_acl() {
        use orvu_crdt::ReplicaId;
        use orvu_runtime::Action;
        let cfg = with_section("[acl]\n2 = [\"sync\", \"write\"]").unwrap();
        assert!(cfg.acl.permits(ReplicaId(2), Action::Write));
        assert!(!cfg.acl.permits(ReplicaId(9), Action::Sync), "a machine not on the list got in");

        assert!(matches!(with_section("[acl]\n2 = [\"admin\"]"), Err(SdkError::Config(_))));
    }

    #[test]
    fn no_acl_section_is_open() {
        use orvu_crdt::ReplicaId;
        use orvu_runtime::Action;
        let sdk = SdkConfig::parse_toml(
            "replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:7440\"\n",
        )
        .unwrap();
        assert!(sdk.build_acl().unwrap().permits(ReplicaId(9), Action::Write));
    }
}
