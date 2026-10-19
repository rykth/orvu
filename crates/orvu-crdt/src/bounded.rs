//! Bounded counter: a counter that keeps `value >= 0`, even while replicas
//! cannot reach each other. It works by escrow: each replica holds a share of
//! the total, called its allowance, and can only spend from its own share.
//!
//! The state is three maps. Their entries only ever grow, and a merge takes the
//! larger entry for each key.
//! - deposits: an increment adds here and gives the depositing replica that
//!   much allowance.
//! - withdrawals: a decrement adds here and spends allowance.
//! - transfers: moves allowance from one replica to another.
//!
//! A replica's local allowance is
//!   deposits[r] + transfers-in(r) - transfers-out(r) - withdrawals[r]
//!
//! A decrement or transfer is refused when it asks for more than the acting
//! replica's local allowance. That is how `value >= 0` always holds without the
//! replicas having to agree with each other first. For the replica doing the
//! spending, a merge can only add to what others have transferred to it. So
//! the allowance it sees on its own copy is never more than it really has, and
//! it can never overspend.

use crate::counter::{clamp_i64, sum};
use crate::id::ReplicaId;
use crate::DeltaCrdt;
use std::collections::BTreeMap;

/// A counter that replicas can change independently and that never goes below
/// zero.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct BoundedCounter {
    deposits: BTreeMap<ReplicaId, u64>,
    withdrawals: BTreeMap<ReplicaId, u64>,
    transfers: BTreeMap<(ReplicaId, ReplicaId), u64>,
}

/// A change to a `BoundedCounter`, to send to other replicas.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct BoundedDelta {
    deposits: BTreeMap<ReplicaId, u64>,
    withdrawals: BTreeMap<ReplicaId, u64>,
    transfers: BTreeMap<(ReplicaId, ReplicaId), u64>,
}

impl BoundedDelta {
    /// Rebuilds a delta from its three maps. Used when decoding a received
    /// message.
    pub fn from_parts(
        deposits: BTreeMap<ReplicaId, u64>,
        withdrawals: BTreeMap<ReplicaId, u64>,
        transfers: BTreeMap<(ReplicaId, ReplicaId), u64>,
    ) -> Self {
        Self { deposits, withdrawals, transfers }
    }
    /// The deposit totals in this delta, by replica.
    pub fn deposits(&self) -> &BTreeMap<ReplicaId, u64> { &self.deposits }
    /// The withdrawal totals in this delta, by replica.
    pub fn withdrawals(&self) -> &BTreeMap<ReplicaId, u64> { &self.withdrawals }
    /// The transfer totals in this delta, by `(from, to)` pair.
    pub fn transfers(&self) -> &BTreeMap<(ReplicaId, ReplicaId), u64> { &self.transfers }
}

/// Adds `by` to the total at `k`, stopping at `u64::MAX`.
fn bump<K: Ord>(map: &mut BTreeMap<K, u64>, k: K, by: u64) -> u64 {
    let e = map.entry(k).or_insert(0);
    *e = e.saturating_add(by);
    *e
}

/// Adds `by` to the total at `k`, or returns `None` and changes nothing if
/// the total would pass `u64::MAX`. A spend must be recorded in full: one
/// recorded as less than it was would leave allowance that is not there.
fn try_bump<K: Ord>(map: &mut BTreeMap<K, u64>, k: K, by: u64) -> Option<u64> {
    let total = map.get(&k).copied().unwrap_or(0).checked_add(by)?;
    map.insert(k, total);
    Some(total)
}

fn max_merge<K: Ord + Copy>(dst: &mut BTreeMap<K, u64>, src: &BTreeMap<K, u64>) {
    for (&k, &v) in src {
        let e = dst.entry(k).or_insert(0);
        *e = (*e).max(v);
    }
}

impl BoundedCounter {
    /// The counter's value: all deposits minus all withdrawals. It is worked
    /// out exactly and then stops at `i64::MAX` or `i64::MIN`, so huge totals
    /// never crash or wrap around.
    pub fn value(&self) -> i64 {
        clamp_i64(sum(&self.deposits) - sum(&self.withdrawals))
    }

    /// How much replica `r` may still withdraw or transfer, as far as this copy
    /// knows. On `r`'s own copy, merges only raise it, so it is safe to act on.
    ///
    /// What `r` received should always be at least what it gave away and
    /// withdrew, so this should never go below zero. It stops at zero in case
    /// it ever does. The sums are worked out exactly, however large, so a
    /// total past `u64::MAX` neither crashes nor wraps; the answer stops at
    /// `u64::MAX`, which is never more than `r` really has.
    pub fn allowance(&self, r: ReplicaId) -> u64 {
        let deposited = self.deposits.get(&r).copied().unwrap_or(0) as i128;
        let received: i128 = self.transfers.iter().filter(|((_, to), _)| *to == r).map(|(_, &v)| v as i128).sum();
        let given: i128 = self.transfers.iter().filter(|((from, _), _)| *from == r).map(|(_, &v)| v as i128).sum();
        let withdrawn = self.withdrawals.get(&r).copied().unwrap_or(0) as i128;
        (deposited + received - given - withdrawn).clamp(0, u64::MAX as i128) as u64
    }

    /// The total replica `r` has withdrawn so far. Withdrawals only grow, and a
    /// merge keeps the larger number, so this never goes down. Comparing two
    /// readings shows how much `r` spent in between, which gives an estimate of
    /// its demand.
    pub fn withdrawn(&self, r: ReplicaId) -> u64 {
        self.withdrawals.get(&r).copied().unwrap_or(0)
    }

    /// The total other replicas have ever transferred to replica `r`. A
    /// transfer `r` makes to itself is not counted.
    /// Transfer totals only grow, and a merge keeps the larger number, so
    /// this never goes down, whatever `r` gives away or spends. A rise
    /// between two readings shows allowance that arrived in between. The sum
    /// stops at `u64::MAX`.
    pub fn received(&self, r: ReplicaId) -> u64 {
        let total: u128 =
            self.transfers.iter().filter(|((from, to), _)| *to == r && *from != r).map(|(_, &v)| v as u128).sum();
        total.min(u64::MAX as u128) as u64
    }

    /// Deposits `n` and gives replica `r` that much allowance. Always succeeds.
    /// The deposit total stops at `u64::MAX`.
    pub fn increment(&mut self, n: u64, r: ReplicaId) -> BoundedDelta {
        let total = bump(&mut self.deposits, r, n);
        BoundedDelta { deposits: BTreeMap::from([(r, total)]), ..Default::default() }
    }

    /// Adds `n` as `increment` does, or returns `None` and changes nothing if
    /// `n` could not be added whole: if `r`'s deposit total would pass
    /// `u64::MAX`, or the value would pass `i64::MAX`, where `increment` would
    /// keep less than `n`.
    pub fn try_increment(&mut self, n: u64, r: ReplicaId) -> Option<BoundedDelta> {
        if i128::from(self.value()) + i128::from(n) > i128::from(i64::MAX) {
            return None;
        }
        let total = try_bump(&mut self.deposits, r, n)?;
        Some(BoundedDelta { deposits: BTreeMap::from([(r, total)]), ..Default::default() })
    }

    /// Withdraws `n` from replica `r`'s local allowance. Refuses, returning
    /// `None`, if the allowance is less than `n`, or if `r`'s withdrawal
    /// total would pass `u64::MAX`.
    pub fn try_decrement(&mut self, n: u64, r: ReplicaId) -> Option<BoundedDelta> {
        if self.allowance(r) < n {
            return None;
        }
        let total = try_bump(&mut self.withdrawals, r, n)?;
        Some(BoundedDelta { withdrawals: BTreeMap::from([(r, total)]), ..Default::default() })
    }

    /// Moves `n` of replica `r`'s allowance to `to`. Refuses, returning `None`,
    /// if the allowance is less than `n`, or if the total `r` has moved to
    /// `to` would pass `u64::MAX`.
    pub fn transfer(&mut self, to: ReplicaId, n: u64, r: ReplicaId) -> Option<BoundedDelta> {
        if self.allowance(r) < n {
            return None;
        }
        let total = try_bump(&mut self.transfers, (r, to), n)?;
        Some(BoundedDelta { transfers: BTreeMap::from([((r, to), total)]), ..Default::default() })
    }
}

impl DeltaCrdt for BoundedCounter {
    type Delta = BoundedDelta;

    fn merge(&mut self, delta: BoundedDelta) {
        max_merge(&mut self.deposits, &delta.deposits);
        max_merge(&mut self.withdrawals, &delta.withdrawals);
        max_merge(&mut self.transfers, &delta.transfers);
    }

    fn snapshot(&self) -> BoundedDelta {
        BoundedDelta {
            deposits: self.deposits.clone(),
            withdrawals: self.withdrawals.clone(),
            transfers: self.transfers.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(n: u64) -> ReplicaId { ReplicaId(n) }

    // An increment that could not be kept whole is turned away and changes
    // nothing, where a plain one keeps less than it was given.
    #[test]
    fn an_increment_that_cannot_be_kept_whole_is_turned_away() {
        let mut b = BoundedCounter::default();
        assert!(b.try_increment(i64::MAX as u64 - 5, r(1)).is_some());
        assert!(b.try_increment(6, r(2)).is_none());
        assert!(b.try_increment(u64::MAX, r(1)).is_none());
        assert_eq!(b.value(), i64::MAX - 5);
        assert!(b.try_increment(5, r(1)).is_some());
        assert_eq!(b.value(), i64::MAX);
        assert!(b.try_increment(1, r(1)).is_none());
    }

    // What a replica received is only ever added to: giving allowance away
    // or spending it does not lower it.
    #[test]
    fn received_counts_only_transfers_in_and_never_goes_down() {
        let mut b = BoundedCounter::default();
        b.increment(10, r(1));
        b.increment(4, r(2));
        assert_eq!(b.received(r(2)), 0, "a deposit is not something received");
        b.transfer(r(2), 5, r(1)).unwrap();
        assert_eq!(b.received(r(2)), 5);
        b.transfer(r(3), 6, r(2)).unwrap();
        b.try_decrement(2, r(2)).unwrap();
        assert_eq!(b.received(r(2)), 5, "giving away and spending do not lower it");
        b.transfer(r(2), 1, r(3)).unwrap();
        assert_eq!(b.received(r(2)), 6);
        b.transfer(r(2), 2, r(2)).unwrap();
        assert_eq!(b.received(r(2)), 6, "a transfer to itself is not something received");
        let mut big = BoundedCounter::default();
        big.merge(BoundedDelta::from_parts(
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::from([((r(1), r(9)), u64::MAX), ((r(2), r(9)), u64::MAX)]),
        ));
        assert_eq!(big.received(r(9)), u64::MAX, "a sum past the top stops there");
    }

    #[test]
    fn increment_mints_rights_for_the_depositor() {
        let mut b = BoundedCounter::default();
        b.increment(10, r(1));
        assert_eq!(b.value(), 10);
        assert_eq!(b.allowance(r(1)), 10);
        assert_eq!(b.allowance(r(2)), 0, "only the depositor holds rights");
    }

    #[test]
    fn try_decrement_spends_local_allowance_and_refuses_beyond_it() {
        let mut b = BoundedCounter::default();
        b.increment(10, r(1));
        assert!(b.try_decrement(4, r(1)).is_some());
        assert_eq!(b.value(), 6);
        assert_eq!(b.allowance(r(1)), 6);
        // r(2) has no allowance, so it is refused and nothing changes.
        let before = b.clone();
        assert!(b.try_decrement(1, r(2)).is_none());
        assert_eq!(b, before, "refused op leaves state byte-identical");
        // r(1) asks for more than its allowance, so it is refused.
        assert!(b.try_decrement(7, r(1)).is_none());
        assert_eq!(b.value(), 6);
    }

    #[test]
    fn transfer_enables_a_peer_to_decrement() {
        let mut b = BoundedCounter::default();
        b.increment(10, r(1));
        assert!(b.try_decrement(1, r(2)).is_none(), "r2 starts with no rights");
        assert!(b.transfer(r(2), 5, r(1)).is_some());
        assert_eq!(b.allowance(r(1)), 5);
        assert_eq!(b.allowance(r(2)), 5);
        assert!(b.try_decrement(5, r(2)).is_some());
        assert_eq!(b.value(), 5);
        assert!(b.try_decrement(1, r(2)).is_none(), "r2 exhausted");
    }

    #[test]
    fn transfer_refused_beyond_allowance() {
        let mut b = BoundedCounter::default();
        b.increment(3, r(1));
        let before = b.clone();
        assert!(b.transfer(r(2), 5, r(1)).is_none());
        assert_eq!(b, before);
    }

    #[test]
    fn partition_safety_merged_value_never_negative() {
        // r1 deposits 10 and transfers 5 to r2. Then both spend their own
        // allowance at the same time. Once the two copies have merged and
        // nothing more is in flight, the value must be >= 0.
        let mut a = BoundedCounter::default();
        a.increment(10, r(1));
        a.transfer(r(2), 5, r(1));
        let mut b = a.clone(); // both copies have seen the transfer
        let da = a.try_decrement(5, r(1)).unwrap(); // r1 spends its 5
        let db = b.try_decrement(5, r(2)).unwrap(); // r2 spends its 5
        a.merge(db);
        b.merge(da);
        assert_eq!(a, b);
        assert_eq!(a.value(), 0);
        assert!(a.value() >= 0);
    }

    #[test]
    fn merge_is_caj_and_delta_round_trips() {
        let mut a = BoundedCounter::default();
        let d1 = a.increment(5, r(1));
        let mut c = BoundedCounter::default();
        c.merge(d1.clone());
        c.merge(d1); // merging the same delta twice changes nothing
        assert_eq!(c.value(), 5);
        // A delta rebuilt with `from_parts` equals the original.
        let mut b = BoundedCounter::default();
        b.increment(7, r(2));
        let d = b.transfer(r(1), 2, r(2)).unwrap();
        let rebuilt = BoundedDelta::from_parts(
            d.deposits().clone(), d.withdrawals().clone(), d.transfers().clone());
        assert_eq!(rebuilt, d);
    }

    // One bad message must not make the total read as negative, or crash.
    #[test]
    fn huge_values_neither_crash_nor_wrap() {
        let mut b = BoundedCounter::default();
        b.merge(BoundedDelta::from_parts(
            BTreeMap::from([(r(1), u64::MAX), (r(2), u64::MAX)]),
            BTreeMap::new(),
            BTreeMap::new(),
        ));
        assert_eq!(b.value(), i64::MAX);
        assert_eq!(b.allowance(r(1)), u64::MAX);
        let d = b.increment(1, r(1));
        assert_eq!(d.deposits()[&r(1)], u64::MAX, "a deposit total stops at the top");

        // Transfers received that add up past the top still count in full
        // against what was given away.
        let mut t = BoundedCounter::default();
        t.merge(BoundedDelta::from_parts(
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::from([((r(2), r(1)), u64::MAX), ((r(3), r(1)), u64::MAX), ((r(1), r(4)), u64::MAX)]),
        ));
        assert_eq!(t.allowance(r(1)), u64::MAX);
        let mut t = BoundedCounter::default();
        t.merge(BoundedDelta::from_parts(
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::from([((r(2), r(1)), 5), ((r(1), r(3)), u64::MAX), ((r(1), r(4)), u64::MAX)]),
        ));
        assert_eq!(t.allowance(r(1)), 0, "given away past the top is more than was received");
    }

    // A withdrawal or a transfer whose total would pass the top cannot be
    // recorded, so it is refused rather than recorded as less than it was.
    #[test]
    fn a_spend_whose_total_would_pass_the_top_is_refused() {
        let mut b = BoundedCounter::default();
        b.merge(BoundedDelta::from_parts(
            BTreeMap::from([(r(1), u64::MAX)]),
            BTreeMap::from([(r(1), u64::MAX)]),
            BTreeMap::from([((r(2), r(1)), 10), ((r(4), r(1)), u64::MAX), ((r(1), r(3)), u64::MAX - 5)]),
        ));
        // r1 has 15 left: everything it was given, less what it spent and
        // gave away. Its own totals are at or near the top.
        assert_eq!(b.allowance(r(1)), 15);
        let before = b.clone();
        assert!(b.try_decrement(1, r(1)).is_none());
        assert!(b.transfer(r(3), 6, r(1)).is_none());
        assert_eq!(b, before);
        assert!(b.transfer(r(3), 5, r(1)).is_some());
        assert!(b.value() >= 0);
    }
}
