//! Our own metrics registry. It uses atomics, so it takes no locks. It is one
//! fixed struct, shared through an `Arc` by all the runtime's tasks. To read
//! it, call `snapshot()`. That does not go through the engine. Counters use
//! `fetch_add`, gauges use `store`, and everything uses `Relaxed` ordering.

#[cfg(orvu_loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(orvu_loom))]
use std::sync::atomic::{AtomicU64, Ordering};

/// Bucket upper limits (Prometheus `le`) for histograms of time taken, in
/// seconds. There are 11 limits, which makes 12 buckets. The last bucket is
/// `+Inf` and has no limit listed here.
pub const LATENCY_BOUNDS: &[f64] =
    &[0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0];

/// Bucket upper limits (Prometheus `le`) for histograms of payload size, in
/// bytes. There are 8 limits, which makes 9 buckets.
pub const BYTE_BOUNDS: &[f64] =
    &[64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0];

/// A histogram with a fixed number of buckets that takes no locks. `NB` is the
/// number of buckets, counting the `+Inf` bucket, so `boundaries.len()` is
/// `NB - 1`.
///
/// Each bucket holds only its own count, so `observe` touches exactly one
/// bucket. The running totals that Prometheus expects are added up when the
/// text is rendered.
///
/// The sum is kept multiplied by `scale`, as the bits of an `f64` inside an
/// `AtomicU64`. It is updated with a compare-and-swap loop, so values with a
/// fractional part are not cut short. `snapshot` divides by `scale` again.
#[derive(Debug)]
pub struct Histogram<const NB: usize> {
    buckets: [AtomicU64; NB],
    sum_bits: AtomicU64,
    boundaries: &'static [f64],
    scale: f64,
}

impl<const NB: usize> Histogram<NB> {
    pub fn new(boundaries: &'static [f64], scale: f64) -> Self {
        debug_assert_eq!(boundaries.len(), NB - 1, "NB must be boundaries + 1 (the +Inf bucket)");
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_bits: AtomicU64::new(0), // 0u64 is the bit-pattern of 0.0f64
            boundaries,
            scale,
        }
    }

    /// Records one value. It takes no locks and the work is bounded by the
    /// small, fixed number of buckets, so it is fine to call from the engine
    /// task.
    pub fn observe(&self, value: f64) {
        // Pick the first limit that is >= value. If there is none, use the
        // `+Inf` bucket (index NB-1).
        let mut idx = self.boundaries.len();
        for (i, &b) in self.boundaries.iter().enumerate() {
            if value <= b {
                idx = i;
                break;
            }
        }
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        // Add to the scaled sum, which is stored as the bits of an f64, using
        // a compare-and-swap loop. A plain `(value * scale) as u64` fetch_add
        // would drop the fractional part of a value (0.5 at scale 1.0 would
        // add nothing).
        let mut cur = self.sum_bits.load(Ordering::Relaxed);
        loop {
            let new = (f64::from_bits(cur) + value * self.scale).to_bits();
            match self.sum_bits.compare_exchange_weak(cur, new, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Copies the current counts and sum into plain data.
    pub fn snapshot(&self) -> HistogramSnapshot<NB> {
        let mut buckets = [0u64; NB];
        let mut count = 0u64;
        for (i, a) in self.buckets.iter().enumerate() {
            let v = a.load(Ordering::Relaxed);
            buckets[i] = v;
            count += v;
        }
        HistogramSnapshot {
            buckets,
            sum: f64::from_bits(self.sum_bits.load(Ordering::Relaxed)) / self.scale,
            count,
        }
    }
}

/// A plain-data copy of a `Histogram`. Each entry in `buckets` is that
/// bucket's own count. The renderer adds them up into the running totals that
/// Prometheus `le` buckets hold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistogramSnapshot<const NB: usize> {
    pub buckets: [u64; NB],
    pub sum: f64,
    pub count: u64,
}

impl<const NB: usize> Default for HistogramSnapshot<NB> {
    fn default() -> Self {
        Self { buckets: [0; NB], sum: 0.0, count: 0 }
    }
}

/// All the metrics a node keeps. Share it through an `Arc`.
#[derive(Debug)]
pub struct Metrics {
    // counters (only ever go up)
    pub deltas_applied_local: AtomicU64,
    pub deltas_applied_remote: AtomicU64,
    pub antientropy_rounds: AtomicU64,
    pub antientropy_repaired_deltas: AtomicU64,
    pub snapshots_sent: AtomicU64,
    pub snapshots_received: AtomicU64,
    /// How many times an object was copied whole to answer a read. A read
    /// that only looks does not count.
    pub objects_copied: AtomicU64,
    pub members_suspected: AtomicU64,
    pub members_evicted: AtomicU64,
    pub writes_dropped_unauthorized: AtomicU64,
    /// Changes and snapshots from other machines refused because a timestamp
    /// in them was too far ahead of this node's clock.
    pub changes_refused_clock_ahead: AtomicU64,
    /// Changes and snapshots taken in although a timestamp in them was ahead
    /// of this node's clock by more than a second: the clocks disagree.
    pub clock_skew_seen: AtomicU64,
    /// The machine that wrote the change most recently refused for being too
    /// far ahead, as the change itself names it: another machine could put
    /// any name there. It may only have passed on a time it took in from
    /// another machine's fast clock. Means nothing while nothing has been refused.
    pub refused_clock_ahead_last_writer: AtomicU64,
    /// Changes from other machines dropped while they waited: after waiting
    /// too long for the changes they depend on, which catch-up brings in
    /// again later, or because by the time they were ready their object had
    /// no room left for another writer.
    pub waiting_changes_dropped: AtomicU64,
    /// Changes and snapshots from other machines refused because no honest
    /// machine could have sent them: numbers that skip ahead, a record with
    /// gaps, or more machines writing to one object than allowed.
    pub changes_refused_malformed: AtomicU64,
    /// `Nack`s sent to say that a change from another machine was refused.
    pub nacks_sent: AtomicU64,
    /// Refused changes that got no `Nack`, because the connection had
    /// already sent as many as it may in that second.
    pub nacks_held_back: AtomicU64,
    /// `Nack`s received from other machines: they refused something this
    /// node sent.
    pub nacks_received: AtomicU64,
    // gauges (current value)
    pub peers_up: AtomicU64,
    /// The time on this node's own clock, in milliseconds since 1970: the
    /// latest timestamp it has given or taken in. How far this runs ahead of
    /// the wall clock is worked out each time the metrics are read, so it
    /// shrinks as real time catches up even while the node is idle.
    pub clock_wall_ms: AtomicU64,
    pub members_known: AtomicU64,
    pub objects: AtomicU64,
    pub deltas_since_snapshot: AtomicU64,
    // histograms (how values are spread out)
    pub mutate_latency: Histogram<12>,
    pub remote_apply_latency: Histogram<12>,
    pub antientropy_round: Histogram<12>,
    pub snapshot_bytes: Histogram<9>,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            deltas_applied_local: AtomicU64::new(0),
            deltas_applied_remote: AtomicU64::new(0),
            antientropy_rounds: AtomicU64::new(0),
            antientropy_repaired_deltas: AtomicU64::new(0),
            snapshots_sent: AtomicU64::new(0),
            snapshots_received: AtomicU64::new(0),
            objects_copied: AtomicU64::new(0),
            members_suspected: AtomicU64::new(0),
            members_evicted: AtomicU64::new(0),
            writes_dropped_unauthorized: AtomicU64::new(0),
            changes_refused_clock_ahead: AtomicU64::new(0),
            clock_skew_seen: AtomicU64::new(0),
            refused_clock_ahead_last_writer: AtomicU64::new(0),
            waiting_changes_dropped: AtomicU64::new(0),
            changes_refused_malformed: AtomicU64::new(0),
            nacks_sent: AtomicU64::new(0),
            nacks_held_back: AtomicU64::new(0),
            nacks_received: AtomicU64::new(0),
            peers_up: AtomicU64::new(0),
            clock_wall_ms: AtomicU64::new(0),
            members_known: AtomicU64::new(0),
            objects: AtomicU64::new(0),
            deltas_since_snapshot: AtomicU64::new(0),
            mutate_latency: Histogram::new(LATENCY_BOUNDS, 1e6),
            remote_apply_latency: Histogram::new(LATENCY_BOUNDS, 1e6),
            antientropy_round: Histogram::new(LATENCY_BOUNDS, 1e6),
            snapshot_bytes: Histogram::new(BYTE_BOUNDS, 1.0),
        }
    }
}

impl Metrics {
    pub fn record_delta_applied_local(&self) { self.deltas_applied_local.fetch_add(1, Ordering::Relaxed); }
    pub fn record_delta_applied_remote(&self) { self.deltas_applied_remote.fetch_add(1, Ordering::Relaxed); }
    pub fn record_antientropy_round(&self) { self.antientropy_rounds.fetch_add(1, Ordering::Relaxed); }
    pub fn add_repaired_deltas(&self, n: u64) { self.antientropy_repaired_deltas.fetch_add(n, Ordering::Relaxed); }
    pub fn record_snapshot_sent(&self) { self.snapshots_sent.fetch_add(1, Ordering::Relaxed); }
    pub fn record_snapshot_received(&self) { self.snapshots_received.fetch_add(1, Ordering::Relaxed); }
    pub fn record_object_copied(&self) { self.objects_copied.fetch_add(1, Ordering::Relaxed); }
    pub fn add_members_suspected(&self, n: u64) { self.members_suspected.fetch_add(n, Ordering::Relaxed); }
    pub fn add_members_evicted(&self, n: u64) { self.members_evicted.fetch_add(n, Ordering::Relaxed); }
    pub fn record_write_dropped_unauthorized(&self) { self.writes_dropped_unauthorized.fetch_add(1, Ordering::Relaxed); }
    pub fn record_refused_clock_ahead(&self, writer: u64) {
        self.refused_clock_ahead_last_writer.store(writer, Ordering::Relaxed);
        self.changes_refused_clock_ahead.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_clock_skew(&self) { self.clock_skew_seen.fetch_add(1, Ordering::Relaxed); }
    pub fn add_waiting_changes_dropped(&self, n: u64) { self.waiting_changes_dropped.fetch_add(n, Ordering::Relaxed); }
    pub fn record_change_refused_malformed(&self) { self.changes_refused_malformed.fetch_add(1, Ordering::Relaxed); }
    pub fn record_nack_sent(&self) { self.nacks_sent.fetch_add(1, Ordering::Relaxed); }
    pub fn record_nack_held_back(&self) { self.nacks_held_back.fetch_add(1, Ordering::Relaxed); }
    pub fn record_nack_received(&self) { self.nacks_received.fetch_add(1, Ordering::Relaxed); }

    pub fn set_peers_up(&self, v: u64) { self.peers_up.store(v, Ordering::Relaxed); }
    pub fn set_members_known(&self, v: u64) { self.members_known.store(v, Ordering::Relaxed); }
    pub fn set_objects(&self, v: u64) { self.objects.store(v, Ordering::Relaxed); }
    pub fn set_deltas_since_snapshot(&self, v: u64) { self.deltas_since_snapshot.store(v, Ordering::Relaxed); }
    pub fn set_clock_wall_ms(&self, v: u64) { self.clock_wall_ms.store(v, Ordering::Relaxed); }

    /// A copy of every metric, read now.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        self.snapshot_at(now_ms)
    }

    /// A copy of every metric, with the clock lead measured against
    /// `now_ms`, the wall clock in milliseconds since 1970.
    pub fn snapshot_at(&self, now_ms: u64) -> MetricsSnapshot {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        MetricsSnapshot {
            deltas_applied_local: g(&self.deltas_applied_local),
            deltas_applied_remote: g(&self.deltas_applied_remote),
            antientropy_rounds: g(&self.antientropy_rounds),
            antientropy_repaired_deltas: g(&self.antientropy_repaired_deltas),
            snapshots_sent: g(&self.snapshots_sent),
            snapshots_received: g(&self.snapshots_received),
            objects_copied: g(&self.objects_copied),
            members_suspected: g(&self.members_suspected),
            members_evicted: g(&self.members_evicted),
            writes_dropped_unauthorized: g(&self.writes_dropped_unauthorized),
            changes_refused_clock_ahead: g(&self.changes_refused_clock_ahead),
            clock_skew_seen: g(&self.clock_skew_seen),
            refused_clock_ahead_last_writer: (g(&self.changes_refused_clock_ahead) > 0)
                .then(|| g(&self.refused_clock_ahead_last_writer)),
            waiting_changes_dropped: g(&self.waiting_changes_dropped),
            changes_refused_malformed: g(&self.changes_refused_malformed),
            nacks_sent: g(&self.nacks_sent),
            nacks_held_back: g(&self.nacks_held_back),
            nacks_received: g(&self.nacks_received),
            peers_up: g(&self.peers_up),
            clock_ahead_of_wall_ms: g(&self.clock_wall_ms).saturating_sub(now_ms),
            members_known: g(&self.members_known),
            objects: g(&self.objects),
            deltas_since_snapshot: g(&self.deltas_since_snapshot),
            mutate_latency: self.mutate_latency.snapshot(),
            remote_apply_latency: self.remote_apply_latency.snapshot(),
            antientropy_round: self.antientropy_round.snapshot(),
            snapshot_bytes: self.snapshot_bytes.snapshot(),
        }
    }
}

/// A plain-data copy of `Metrics`, taken at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MetricsSnapshot {
    pub deltas_applied_local: u64,
    pub deltas_applied_remote: u64,
    pub antientropy_rounds: u64,
    pub antientropy_repaired_deltas: u64,
    pub snapshots_sent: u64,
    pub snapshots_received: u64,
    pub objects_copied: u64,
    pub members_suspected: u64,
    pub members_evicted: u64,
    pub writes_dropped_unauthorized: u64,
    pub changes_refused_clock_ahead: u64,
    pub clock_skew_seen: u64,
    /// The writer of the last refused change, or `None` if none was refused.
    pub refused_clock_ahead_last_writer: Option<u64>,
    /// See `Metrics::waiting_changes_dropped`.
    pub waiting_changes_dropped: u64,
    /// See `Metrics::changes_refused_malformed`.
    pub changes_refused_malformed: u64,
    /// See `Metrics::nacks_sent`.
    pub nacks_sent: u64,
    /// See `Metrics::nacks_held_back`.
    pub nacks_held_back: u64,
    /// See `Metrics::nacks_received`.
    pub nacks_received: u64,
    pub peers_up: u64,
    /// How far this node's own clock runs ahead of the wall clock at the
    /// moment the copy was taken, in milliseconds. 0 when it does not.
    pub clock_ahead_of_wall_ms: u64,
    pub members_known: u64,
    pub objects: u64,
    pub deltas_since_snapshot: u64,
    pub mutate_latency: HistogramSnapshot<12>,
    pub remote_apply_latency: HistogramSnapshot<12>,
    pub antientropy_round: HistogramSnapshot<12>,
    pub snapshot_bytes: HistogramSnapshot<9>,
}

/// Renders a snapshot as Prometheus text. A counter or gauge gets a
/// `# TYPE orvu_<name> <counter|gauge>` line followed by `orvu_<name> <value>`.
/// A histogram gets a `# TYPE` line, one `_bucket` line per limit plus `+Inf`,
/// then `_sum` and `_count`.
pub fn render_prometheus(s: &MetricsSnapshot) -> String {
    let mut out = String::new();
    let mut counter = |name: &str, v: u64| {
        out.push_str(&format!("# TYPE orvu_{name} counter\norvu_{name} {v}\n"));
    };
    counter("deltas_applied_local", s.deltas_applied_local);
    counter("deltas_applied_remote", s.deltas_applied_remote);
    counter("antientropy_rounds", s.antientropy_rounds);
    counter("antientropy_repaired_deltas", s.antientropy_repaired_deltas);
    counter("snapshots_sent", s.snapshots_sent);
    counter("snapshots_received", s.snapshots_received);
    counter("objects_copied", s.objects_copied);
    counter("members_suspected", s.members_suspected);
    counter("members_evicted", s.members_evicted);
    counter("writes_dropped_unauthorized", s.writes_dropped_unauthorized);
    counter("changes_refused_clock_ahead", s.changes_refused_clock_ahead);
    counter("clock_skew_seen", s.clock_skew_seen);
    counter("waiting_changes_dropped", s.waiting_changes_dropped);
    counter("changes_refused_malformed", s.changes_refused_malformed);
    counter("nacks_sent", s.nacks_sent);
    counter("nacks_held_back", s.nacks_held_back);
    counter("nacks_received", s.nacks_received);
    let mut gauge = |name: &str, v: u64| {
        out.push_str(&format!("# TYPE orvu_{name} gauge\norvu_{name} {v}\n"));
    };
    gauge("peers_up", s.peers_up);
    gauge("members_known", s.members_known);
    gauge("objects", s.objects);
    gauge("deltas_since_snapshot", s.deltas_since_snapshot);
    gauge("clock_ahead_of_wall_ms", s.clock_ahead_of_wall_ms);
    // Machine ids do not fit in a Prometheus number, so the id is a label.
    if let Some(w) = s.refused_clock_ahead_last_writer {
        out.push_str(&format!(
            "# TYPE orvu_refused_clock_ahead_last_writer gauge\norvu_refused_clock_ahead_last_writer{{writer=\"{w}\"}} 1\n"
        ));
    }
    render_histogram(&mut out, "mutate_latency_seconds", &s.mutate_latency, LATENCY_BOUNDS);
    render_histogram(&mut out, "remote_apply_latency_seconds", &s.remote_apply_latency, LATENCY_BOUNDS);
    render_histogram(&mut out, "antientropy_round_seconds", &s.antientropy_round, LATENCY_BOUNDS);
    render_histogram(&mut out, "snapshot_bytes", &s.snapshot_bytes, BYTE_BOUNDS);
    out
}

/// Adds one histogram to `out` as Prometheus text, named `orvu_<name>`: a
/// `# TYPE` line, one `_bucket` line per limit plus `+Inf`, then `_sum` and
/// `_count`.
pub fn render_histogram<const NB: usize>(out: &mut String, name: &str, s: &HistogramSnapshot<NB>, bounds: &[f64]) {
    out.push_str(&format!("# TYPE orvu_{name} histogram\n"));
    let mut cum = 0u64;
    for (i, b) in bounds.iter().enumerate() {
        cum += s.buckets[i];
        out.push_str(&format!("orvu_{name}_bucket{{le=\"{b}\"}} {cum}\n"));
    }
    cum += s.buckets[bounds.len()]; // the +Inf bucket
    out.push_str(&format!("orvu_{name}_bucket{{le=\"+Inf\"}} {cum}\n"));
    out.push_str(&format!("orvu_{name}_sum {}\n", s.sum));
    out.push_str(&format!("orvu_{name}_count {}\n", s.count));
}

#[cfg(test)]
mod tests {
    use super::*;

    // The clock lead is measured when the copy is taken.
    #[test]
    fn the_clock_lead_is_measured_against_the_time_of_reading() {
        let m = Metrics::default();
        assert_eq!(m.snapshot().clock_ahead_of_wall_ms, 0);
        m.set_clock_wall_ms(1_000_000);
        assert_eq!(m.snapshot_at(995_000).clock_ahead_of_wall_ms, 5_000);
        assert_eq!(m.snapshot_at(999_000).clock_ahead_of_wall_ms, 1_000);
        assert_eq!(m.snapshot_at(1_000_001).clock_ahead_of_wall_ms, 0);
    }

    #[test]
    fn counters_and_gauges_reflect_in_snapshot() {
        let m = Metrics::default();
        m.record_delta_applied_remote();
        m.record_delta_applied_remote();
        m.add_members_suspected(3);
        m.set_peers_up(2);
        m.set_objects(5);
        let s = m.snapshot();
        assert_eq!(s.deltas_applied_remote, 2);
        assert_eq!(s.members_suspected, 3);
        assert_eq!(s.peers_up, 2);
        assert_eq!(s.objects, 5);
        assert_eq!(s.deltas_applied_local, 0);
    }

    #[test]
    fn gauge_set_is_last_writer() {
        let m = Metrics::default();
        m.set_peers_up(2);
        m.set_peers_up(1);
        assert_eq!(m.snapshot().peers_up, 1);
    }

    #[test]
    fn render_prometheus_emits_typed_lines() {
        let s = MetricsSnapshot {
            deltas_applied_remote: 7,
            peers_up: 1,
            ..Default::default()
        };
        let text = render_prometheus(&s);
        assert!(text.contains("# TYPE orvu_deltas_applied_remote counter\norvu_deltas_applied_remote 7\n"));
        assert!(text.contains("# TYPE orvu_peers_up gauge\norvu_peers_up 1\n"));
    }

    #[test]
    fn render_prometheus_emits_histogram_block() {
        let m = Metrics::default();
        m.mutate_latency.observe(0.002); // 2ms
        let text = render_prometheus(&m.snapshot());
        assert!(text.contains("# TYPE orvu_mutate_latency_seconds histogram"));
        // Rendered buckets are running totals, so the <=0.0025 bucket includes
        // the 2ms value.
        assert!(text.contains("orvu_mutate_latency_seconds_bucket{le=\"0.0025\"} 1"),
            "cumulative le=0.0025 bucket counts the 2ms sample:\n{text}");
        assert!(text.contains("orvu_mutate_latency_seconds_bucket{le=\"+Inf\"} 1"));
        assert!(text.contains("orvu_mutate_latency_seconds_count 1"));
        // A byte histogram is rendered too, even when it is empty.
        assert!(text.contains("# TYPE orvu_snapshot_bytes histogram"));
        assert!(text.contains("orvu_snapshot_bytes_bucket{le=\"+Inf\"} 0"));
    }

    #[test]
    fn histogram_observe_buckets_and_sum() {
        // 3 limits (1,2,3) make 4 buckets (<=1, <=2, <=3, +Inf).
        let h: Histogram<4> = Histogram::new(&[1.0, 2.0, 3.0], 1.0);
        h.observe(0.5); // bucket 0
        h.observe(2.0); // bucket 1 (a value equal to the limit goes in that bucket)
        h.observe(2.5); // bucket 2
        h.observe(9.0); // +Inf bucket (3)
        let s = h.snapshot();
        assert_eq!(s.buckets, [1, 1, 1, 1], "one observation per bucket, non-cumulative");
        assert_eq!(s.count, 4);
        assert_eq!(s.sum, 0.5 + 2.0 + 2.5 + 9.0);
    }

    #[test]
    fn histogram_scale_renders_sum_in_unit() {
        // With scale 1e6, values go in as seconds, the sum is stored in
        // microseconds, and the snapshot gives seconds again.
        let h: Histogram<12> = Histogram::new(LATENCY_BOUNDS, 1e6);
        h.observe(0.002); // 2ms goes in the <=0.0025 bucket (index 2)
        let s = h.snapshot();
        assert_eq!(s.count, 1);
        assert!((s.sum - 0.002).abs() < 1e-9, "sum renders back to seconds: {}", s.sum);
        assert_eq!(s.buckets[2], 1, "2ms lands in the <=0.0025s bucket");
    }

    #[test]
    fn byte_bounds_top_bucket_is_inf() {
        let h: Histogram<9> = Histogram::new(BYTE_BOUNDS, 1.0);
        h.observe(5_000_000.0); // above the last limit (1_048_576), so the +Inf bucket (index 8)
        let s = h.snapshot();
        assert_eq!(s.buckets[8], 1);
        assert_eq!(s.count, 1);
        assert_eq!(s.sum, 5_000_000.0);
    }
}
