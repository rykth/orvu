//! Node configuration. It is a plain struct. It is not read from a file and
//! has no serde support.

use orvu_crdt::ReplicaId;
use orvu_store::FsyncPolicy;
use orvu_transport::{LinkTimes, PeerAddr};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

/// Something a machine may be allowed to do in a namespace. There is no `Read`
/// action: what we send out to another machine is not filtered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Sync,
    Write,
}

/// The access list for a namespace: which machine may do what. Machines are
/// looked up by their verified `ReplicaId`. `None` means no list was set and
/// everything is allowed. `Some(map)` is an allow-list, and a machine that is
/// not in it is refused every action.
#[derive(Debug, Clone, Default)]
pub struct Acl(Option<BTreeMap<ReplicaId, BTreeSet<Action>>>);

impl Acl {
    /// No access list. Every authenticated machine is allowed everything.
    pub fn open() -> Self {
        Acl(None)
    }
    /// An allow-list. Only the listed machines get the actions listed for them.
    pub fn allow_list(map: BTreeMap<ReplicaId, BTreeSet<Action>>) -> Self {
        Acl(Some(map))
    }
    /// Whether `id` may perform `action`.
    pub fn permits(&self, id: ReplicaId, action: Action) -> bool {
        match &self.0 {
            None => true,
            Some(map) => map.get(&id).is_some_and(|s| s.contains(&action)),
        }
    }
}

/// The quiet limit is at least this many anti-entropy intervals. See
/// `NodeConfig::quiet_limit`.
pub const QUIET_INTERVALS_AT_LEAST: u32 = 3;

/// The quiet limit is never below this, however short the anti-entropy
/// interval. See `NodeConfig::quiet_limit`.
pub const QUIET_LIMIT_AT_LEAST: Duration = Duration::from_secs(1);

/// The settings for one node.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub replica: ReplicaId,
    pub namespace: String,
    pub listen: String,
    pub peers: Vec<PeerAddr>,
    pub ship_queue_cap: usize,
    pub causal_buffer_cap: usize,
    pub delta_batch: Duration,
    pub anti_entropy_interval: Duration,
    /// How long a connected machine may send nothing at all before the
    /// connection is closed and it is dialed again. `None` (the default) is
    /// five times `anti_entropy_interval`, and a value below three times that
    /// interval, or below one second, is raised to it (see `quiet_limit()`).
    ///
    /// Each connection sends something at least about once an interval even
    /// when it has nothing to say, a small keep-alive message, and it does so
    /// without waiting on the engine. So a machine that is only busy, saving
    /// to a slow disk or building a large snapshot, is not let go. A machine
    /// stays silent this long when its process has stopped running, or when
    /// the link between the two is gone. Machines that talk to each other
    /// should use the same `anti_entropy_interval`. If they do not, this
    /// limit should be at least three times the longest one among them on
    /// every machine: a machine with a long interval may be quiet that long,
    /// and one with a short interval would otherwise let it go.
    pub quiet_limit: Option<Duration>,
    /// The longest wait between two dials to a machine on `peers` that cannot
    /// be reached. The waits start at a quarter of a second and double after
    /// each failed dial until they reach this. A machine that comes back is
    /// found at most this long after it is up.
    pub longest_dial_wait: Duration,
    /// The time limits of a QUIC link: when a link on which nothing is heard
    /// is given up, how often a quiet link sends a packet to show it is
    /// alive, and how long a dial may take. The node does not build its link
    /// itself, so these are for whoever does: `orvu-sdk` builds its
    /// `QuicTransport` with them.
    pub link: LinkTimes,
    /// A bounded counter cannot go below zero, and each machine has its own
    /// allowance to spend. When a decrement is refused because the allowance
    /// ran out, this is how long it waits for another machine to lend some
    /// before giving up with `Exhausted`.
    pub borrow_timeout: Duration,
    /// How often to rebalance bounded-counter allowances ahead of need. `None`
    /// (the default) turns the rebalancing loop off. `Some(d)` shares
    /// `BoundedCounter` allowance out to other machines every `d`.
    pub rebalance_interval: Option<Duration>,
    /// How long a member we have no direct connection to may go without its
    /// heartbeat moving forward before it is marked `Suspect`.
    pub suspect_timeout: Duration,
    /// How long a member stays `Suspect` before it is removed.
    pub dead_timeout: Duration,
    /// How long a removed member is remembered. While it is, news about it
    /// from other machines is taken only if its heartbeat is newer than the
    /// last one heard. After this it is forgotten and any news brings it back.
    pub forget_removed_after: Duration,
    /// Where the log is kept on disk. `None` keeps everything in memory and
    /// saves nothing.
    pub store_path: Option<PathBuf>,
    /// When appended deltas are forced to disk (fsync).
    pub fsync: FsyncPolicy,
    /// How many deltas to save between snapshots. `0` turns compaction off, so
    /// the log is never swapped for a snapshot.
    pub compaction_threshold: usize,
    /// The access list for the namespace. `Acl::open()` (the default) allows
    /// every authenticated machine.
    pub acl: Acl,
    /// A host:port to serve Prometheus metrics on, at `GET /metrics` over HTTP.
    /// `None` (the default) means no such endpoint.
    pub metrics_addr: Option<String>,
    /// The protocol version this node announces when it connects, and the
    /// only one it will sync with. It starts as `orvu_proto::PROTO_VERSION`,
    /// and there is no reason to change it outside tests, where it stands in
    /// for a machine running another version.
    pub proto_version: u32,
}

impl NodeConfig {
    /// How long a connected machine may stay silent before it is let go:
    /// `quiet_limit`, or five times `anti_entropy_interval` when that is not
    /// set. It is never less than three intervals (`QUIET_INTERVALS_AT_LEAST`)
    /// nor less than one second (`QUIET_LIMIT_AT_LEAST`). A healthy link with
    /// nothing to carry sends something about once an interval, so a shorter
    /// limit would close it now and then for no reason and dial it again, and
    /// with a very short interval a machine busy for a moment would be let go.
    pub fn quiet_limit(&self) -> Duration {
        let lowest = self.lowest_quiet_limit();
        self.quiet_limit.unwrap_or(self.anti_entropy_interval * 5).max(lowest)
    }

    /// The smallest `quiet_limit` the node uses: three times
    /// `anti_entropy_interval`, and at least one second.
    pub fn lowest_quiet_limit(&self) -> Duration {
        (self.anti_entropy_interval * QUIET_INTERVALS_AT_LEAST).max(QUIET_LIMIT_AT_LEAST)
    }

    /// A config with reasonable defaults. You pass `replica` and `listen` here
    /// and fill in `peers` afterwards.
    pub fn new(replica: ReplicaId, listen: impl Into<String>) -> Self {
        let anti_entropy_interval = Duration::from_secs(1);
        Self {
            replica,
            namespace: "default".to_string(),
            listen: listen.into(),
            peers: Vec::new(),
            ship_queue_cap: 4096,
            causal_buffer_cap: 4096,
            delta_batch: Duration::from_millis(10),
            anti_entropy_interval,
            quiet_limit: None,
            longest_dial_wait: Duration::from_secs(5),
            link: LinkTimes::default(),
            borrow_timeout: Duration::from_secs(2),
            rebalance_interval: None,
            suspect_timeout: anti_entropy_interval * 2,
            dead_timeout: anti_entropy_interval * 2,
            forget_removed_after: Duration::from_secs(24 * 3600),
            store_path: None,
            fsync: FsyncPolicy::default(),
            compaction_threshold: 10_000,
            acl: Acl::open(),
            metrics_addr: None,
            proto_version: orvu_proto::PROTO_VERSION,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quiet_limit_is_never_below_three_catch_up_intervals() {
        let mut cfg = NodeConfig::new(ReplicaId(1), "x");
        cfg.anti_entropy_interval = Duration::from_secs(1);
        assert_eq!(cfg.quiet_limit(), Duration::from_secs(5), "five intervals when not set");
        cfg.quiet_limit = Some(Duration::from_millis(500));
        assert_eq!(cfg.quiet_limit(), Duration::from_secs(3), "a limit set too low is raised");
        cfg.quiet_limit = Some(Duration::from_secs(4));
        assert_eq!(cfg.quiet_limit(), Duration::from_secs(4), "a limit high enough is kept");
    }

    // With a short catch-up interval, three intervals is a very short time,
    // and a link held up for a moment by a busy machine would be closed. So
    // the quiet limit is never below one second either.
    #[test]
    fn the_quiet_limit_is_never_below_one_second() {
        let mut cfg = NodeConfig::new(ReplicaId(1), "x");
        cfg.anti_entropy_interval = Duration::from_millis(50);
        assert_eq!(cfg.quiet_limit(), Duration::from_secs(1), "five intervals is only 250 ms");
        cfg.quiet_limit = Some(Duration::from_millis(300));
        assert_eq!(cfg.quiet_limit(), Duration::from_secs(1), "a limit set too low is raised");
        assert_eq!(cfg.lowest_quiet_limit(), Duration::from_secs(1));
        cfg.quiet_limit = Some(Duration::from_millis(1200));
        assert_eq!(cfg.quiet_limit(), Duration::from_millis(1200));
    }

    #[test]
    fn acl_open_allows_everything() {
        let acl = Acl::open();
        assert!(acl.permits(ReplicaId(2), Action::Sync));
        assert!(acl.permits(ReplicaId(2), Action::Write));
    }

    #[test]
    fn acl_allow_list_grants_only_listed_actions() {
        use std::collections::{BTreeMap, BTreeSet};
        let mut map = BTreeMap::new();
        map.insert(ReplicaId(2), BTreeSet::from([Action::Sync, Action::Write]));
        map.insert(ReplicaId(3), BTreeSet::from([Action::Sync])); // may sync, may not write
        let acl = Acl::allow_list(map);

        assert!(acl.permits(ReplicaId(2), Action::Write));
        assert!(acl.permits(ReplicaId(3), Action::Sync));
        assert!(!acl.permits(ReplicaId(3), Action::Write)); // sync but not write
        assert!(!acl.permits(ReplicaId(4), Action::Sync));  // not listed, so refused
    }

    #[test]
    fn acl_empty_allow_list_denies_all() {
        use std::collections::BTreeMap;
        let acl = Acl::allow_list(BTreeMap::new());
        assert!(!acl.permits(ReplicaId(2), Action::Sync));
    }
}
