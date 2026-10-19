//! PN-Counter (positive-negative counter): a counter that can go up and down.
//! It keeps a running total of increments and a running total of decrements
//! for each replica. The value is all the increments minus all the decrements.

use crate::id::ReplicaId;
use crate::DeltaCrdt;
use std::collections::BTreeMap;

/// A counter that replicas can raise and lower independently.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PnCounter {
    inc: BTreeMap<ReplicaId, u64>,
    dec: BTreeMap<ReplicaId, u64>,
}

/// A change to a `PnCounter`, to send to other replicas.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PnDelta {
    inc: BTreeMap<ReplicaId, u64>,
    dec: BTreeMap<ReplicaId, u64>,
}

impl PnDelta {
    /// Rebuilds a delta from its two maps of per-replica totals. Used when
    /// decoding a received message.
    pub fn from_parts(inc: BTreeMap<ReplicaId, u64>, dec: BTreeMap<ReplicaId, u64>) -> Self {
        Self { inc, dec }
    }

    /// Per-replica increment totals carried by this delta.
    pub fn inc(&self) -> &BTreeMap<ReplicaId, u64> {
        &self.inc
    }

    /// Per-replica decrement totals carried by this delta.
    pub fn dec(&self) -> &BTreeMap<ReplicaId, u64> {
        &self.dec
    }
}

/// Adds `by` to `r`'s total. A total stops at `u64::MAX` rather than
/// crash or wrap around.
fn bump(map: &mut BTreeMap<ReplicaId, u64>, r: ReplicaId, by: u64) -> u64 {
    let e = map.entry(r).or_insert(0);
    *e = e.saturating_add(by);
    *e
}

/// All the totals in `m` added up. An `i128` holds the sum of any number of
/// `u64`s this program could keep, so it never overflows.
pub(crate) fn sum(m: &BTreeMap<impl Ord, u64>) -> i128 {
    m.values().map(|&v| v as i128).sum()
}

/// `n` as an `i64`, stopping at the ends of its range.
pub(crate) fn clamp_i64(n: i128) -> i64 {
    n.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn max_merge(dst: &mut BTreeMap<ReplicaId, u64>, src: &BTreeMap<ReplicaId, u64>) {
    for (&r, &v) in src {
        let e = dst.entry(r).or_insert(0);
        *e = (*e).max(v);
    }
}

impl PnCounter {
    /// Adds `by` on behalf of replica `r`. The delta carries `r`'s new increment
    /// total, which stops at `u64::MAX`.
    pub fn increment(&mut self, r: ReplicaId, by: u64) -> PnDelta {
        let total = bump(&mut self.inc, r, by);
        PnDelta { inc: BTreeMap::from([(r, total)]), dec: BTreeMap::new() }
    }

    /// Subtracts `by` on behalf of replica `r`. The delta carries `r`'s new
    /// decrement total, which stops at `u64::MAX`.
    pub fn decrement(&mut self, r: ReplicaId, by: u64) -> PnDelta {
        let total = bump(&mut self.dec, r, by);
        PnDelta { inc: BTreeMap::new(), dec: BTreeMap::from([(r, total)]) }
    }

    /// The current value: all increments minus all decrements. It is worked
    /// out exactly and then stops at `i64::MAX` or `i64::MIN`, so huge totals
    /// never crash or wrap around.
    pub fn value(&self) -> i64 {
        clamp_i64(sum(&self.inc) - sum(&self.dec))
    }
}

impl DeltaCrdt for PnCounter {
    type Delta = PnDelta;

    fn merge(&mut self, delta: PnDelta) {
        max_merge(&mut self.inc, &delta.inc);
        max_merge(&mut self.dec, &delta.dec);
    }

    fn snapshot(&self) -> PnDelta {
        PnDelta { inc: self.inc.clone(), dec: self.dec.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_counts_inc_and_dec() {
        let mut c = PnCounter::default();
        c.increment(ReplicaId(1), 5);
        c.decrement(ReplicaId(1), 2);
        assert_eq!(c.value(), 3);
    }

    #[test]
    fn merge_is_idempotent_and_commutative() {
        let mut a = PnCounter::default();
        let da = a.increment(ReplicaId(1), 5);
        let mut b = PnCounter::default();
        let db = b.increment(ReplicaId(2), 3);

        let mut a2 = a.clone();
        a2.merge(db.clone());
        a2.merge(db.clone()); // merging the same delta twice changes nothing
        let mut b2 = b.clone();
        b2.merge(da);
        assert_eq!(a2.value(), 8);
        assert_eq!(a2, b2); // both sides end up in the same state
    }

    #[test]
    fn pn_delta_round_trips_through_parts() {
        let mut c = PnCounter::default();
        let d = c.increment(ReplicaId(1), 5);
        let rebuilt = PnDelta::from_parts(d.inc().clone(), d.dec().clone());
        assert_eq!(rebuilt, d);

        let mut applied = PnCounter::default();
        applied.merge(rebuilt);
        assert_eq!(applied.value(), 5);
    }

    // Totals stop at the largest number they can hold instead of crashing
    // or wrapping around, and the value stops at the ends of an i64.
    #[test]
    fn huge_values_neither_crash_nor_wrap() {
        let mut c = PnCounter::default();
        c.increment(ReplicaId(1), u64::MAX);
        let d = c.increment(ReplicaId(1), 1);
        assert_eq!(d.inc()[&ReplicaId(1)], u64::MAX);
        assert_eq!(c.value(), i64::MAX);
        c.increment(ReplicaId(2), u64::MAX);
        assert_eq!(c.value(), i64::MAX);
        c.decrement(ReplicaId(1), u64::MAX);
        assert_eq!(c.value(), i64::MAX, "two maxed adds less one maxed take is still past the top");
        c.decrement(ReplicaId(2), u64::MAX);
        assert_eq!(c.value(), 0);
        c.decrement(ReplicaId(3), u64::MAX);
        assert_eq!(c.value(), i64::MIN);

        // The same through a merge of totals another machine sent.
        let many = (1..=4).map(|r| (ReplicaId(r), u64::MAX)).collect();
        let mut m = PnCounter::default();
        m.merge(PnDelta::from_parts(many, BTreeMap::new()));
        assert_eq!(m.value(), i64::MAX);
        let mut n = PnCounter::default();
        n.merge(PnDelta::from_parts(BTreeMap::from([(ReplicaId(1), 5)]), BTreeMap::from([(ReplicaId(1), 3)])));
        assert_eq!(n.value(), 2);
    }
}
