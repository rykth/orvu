//! A version vector: for each replica, the highest update counter seen from
//! it. An entry of 5 means that replica's updates 1 to 5 have all been seen.

use crate::id::ReplicaId;
use std::collections::BTreeMap;

/// Records, for each replica, how many of its updates have been seen.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct VersionVector(BTreeMap<ReplicaId, u64>);

impl VersionVector {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// The entry for `r`, or 0 if nothing from `r` has been seen.
    pub fn get(&self, r: ReplicaId) -> u64 {
        self.0.get(&r).copied().unwrap_or(0)
    }

    /// Raises `r`'s entry to `counter` if it is lower. Calling this again with
    /// the same or a smaller number changes nothing.
    ///
    /// A `counter` of 0 does nothing. Storing a zero entry would make this
    /// vector compare unequal to one that never heard of `r`, although the two
    /// mean the same thing.
    pub fn observe(&mut self, r: ReplicaId, counter: u64) {
        if counter == 0 {
            return;
        }
        let e = self.0.entry(r).or_insert(0);
        *e = (*e).max(counter);
    }

    /// Takes in everything `other` has seen, keeping the higher entry for each
    /// replica.
    pub fn merge(&mut self, other: &VersionVector) {
        for (&r, &c) in &other.0 {
            self.observe(r, c);
        }
    }

    /// True if `self` has seen everything `other` has: every entry in `other`
    /// is matched by an entry in `self` that is at least as high.
    pub fn dominates(&self, other: &VersionVector) -> bool {
        other.0.iter().all(|(&r, &c)| self.get(r) >= c)
    }

    /// Every stored entry, as `(replica, counter)`.
    pub fn iter(&self) -> impl Iterator<Item = (ReplicaId, u64)> + '_ {
        self.0.iter().map(|(&r, &c)| (r, c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_is_max_merge() {
        let mut vv = VersionVector::new();
        vv.observe(ReplicaId(1), 3);
        vv.observe(ReplicaId(1), 2); // lower, so ignored
        assert_eq!(vv.get(ReplicaId(1)), 3);
        assert_eq!(vv.get(ReplicaId(9)), 0);
    }

    #[test]
    fn observe_zero_is_a_no_op() {
        let mut vv = VersionVector::new();
        vv.observe(ReplicaId(1), 0);
        assert_eq!(vv, VersionVector::new());
    }

    #[test]
    fn merge_and_dominates() {
        let mut a = VersionVector::new();
        a.observe(ReplicaId(1), 3);
        let mut b = VersionVector::new();
        b.observe(ReplicaId(1), 1);
        b.observe(ReplicaId(2), 5);
        a.merge(&b);
        assert_eq!(a.get(ReplicaId(1)), 3);
        assert_eq!(a.get(ReplicaId(2)), 5);
        assert!(a.dominates(&b));
        assert!(!b.dominates(&a));
    }
}
