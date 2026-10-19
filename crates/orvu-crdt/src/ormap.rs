//! OR-Map (observed-remove map): a map whose keys behave like an add-wins set
//! and whose values are CRDTs of their own. A key is present exactly when it
//! has at least one live dot (a unique tag for one update). Values are always
//! merged with their own `DeltaCrdt::merge`, whether or not the key is present.

use crate::context::DotContext;
use crate::error::WriteError;
use crate::dot_index::{count_own_dots, merge_presence, DotIndex};
use crate::id::{Dot, ReplicaId};
use crate::DeltaCrdt;
use std::collections::{BTreeMap, BTreeSet};

/// A map that replicas can change independently. If one replica updates a key
/// while another removes it, the update wins. Each value is a CRDT and merges
/// by its own rules.
///
/// # How removal works
///
/// `remove` cancels the key's dots but keeps its value. So adding the key back
/// brings the old value back instead of starting fresh: `update` on a removed
/// key carries on from the old state. Removed values are never freed either.
/// They stay in the map and in every [`DeltaCrdt::snapshot`]. This type never
/// cleans them up by itself.
///
/// A real reset would need the value type to offer a "clear" delta. Throwing
/// the value away instead would stop replicas from agreeing, because a replica
/// that never saw the value would end up with different contents.
#[derive(Clone, Debug)]
pub struct OrMap<K: Ord, V> {
    dots: BTreeMap<K, BTreeSet<Dot>>,
    values: BTreeMap<K, V>,
    ctx: DotContext,
    /// Which key each dot tags. Worked out from `dots`, so that a change can
    /// find the keys it touches without going through all of them.
    index: DotIndex<K>,
}

impl<K: Ord, V> Default for OrMap<K, V> {
    fn default() -> Self {
        Self {
            dots: BTreeMap::new(),
            values: BTreeMap::new(),
            ctx: DotContext::new(),
            index: DotIndex::default(),
        }
    }
}

// Two maps are the same if they hold the same dots and values and have seen
// the same. The index is worked out from the dots and says nothing more.
impl<K: Ord, V: PartialEq> PartialEq for OrMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.dots == other.dots && self.values == other.values && self.ctx == other.ctx
    }
}

impl<K: Ord, V: Eq> Eq for OrMap<K, V> {}

/// A change to an `OrMap`, to send to other replicas.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct OrMapDelta<K: Ord, V: DeltaCrdt> {
    dots: BTreeMap<K, BTreeSet<Dot>>,
    values: BTreeMap<K, V::Delta>,
    ctx: DotContext,
}

impl<K: Ord, V: DeltaCrdt> OrMapDelta<K, V> {
    /// Rebuilds a delta from its parts. Used when decoding a received message.
    pub fn from_parts(
        dots: BTreeMap<K, BTreeSet<Dot>>,
        values: BTreeMap<K, V::Delta>,
        ctx: DotContext,
    ) -> Self {
        Self { dots, values, ctx }
    }

    /// The keys this delta marks as present, with their dots.
    pub fn dots(&self) -> impl Iterator<Item = (&K, &BTreeSet<Dot>)> {
        self.dots.iter()
    }

    /// The value deltas in this delta, by key.
    pub fn values(&self) -> impl Iterator<Item = (&K, &V::Delta)> {
        self.values.iter()
    }

    /// The context sent with this delta: every dot the delta knows about,
    /// whether it is still present or has been cancelled.
    pub fn context(&self) -> &DotContext {
        &self.ctx
    }
}

impl<K, V> OrMap<K, V>
where
    K: Ord + Clone,
    V: DeltaCrdt + Default + Clone,
    V::Delta: Clone,
{
    /// Changes the value at `k`, starting from a default value if the map has
    /// none, and marks the key present with a new dot. `f` makes the change and
    /// returns the value's delta for it.
    ///
    /// Refuses with `WriteError::NoFreshDot` if `r` has no dot left (see
    /// `MAX_COUNTER`). Then `f` is not called and nothing changes.
    pub fn update(
        &mut self,
        k: K,
        r: ReplicaId,
        f: impl FnOnce(&mut V) -> V::Delta,
    ) -> Result<OrMapDelta<K, V>, WriteError> {
        let d = self.ctx.fresh_dot(r)?;
        let mut v = self.values.get(&k).cloned().unwrap_or_default();
        let vdelta = f(&mut v);

        let mut dctx = DotContext::new();
        // The new dot replaces every dot this copy already holds for `k`. This
        // copy has seen those dots, so it may cancel them, and listing them in
        // `dctx` is what cancels them. Dots it has not seen are not in `dctx`,
        // so adds made elsewhere at the same time survive.
        if let Some(old) = self.dots.get(&k) {
            for &o in old {
                dctx.insert(o);
            }
        }
        dctx.insert(d);
        let delta = OrMapDelta {
            dots: BTreeMap::from([(k.clone(), BTreeSet::from([d]))]),
            values: BTreeMap::from([(k, vdelta)]),
            ctx: dctx,
        };
        self.merge(delta.clone());
        Ok(delta)
    }

    /// How many `update`s `r` can still make here before it runs out of dots.
    pub(crate) fn room(&self, r: ReplicaId) -> u64 {
        self.ctx.room(r)
    }

    /// Changes the value at `k` only if the key is present, and returns `None`
    /// without calling `f` if it is not. Unlike `update`, this does not give
    /// the key a new dot. So if another replica removes the key at the same
    /// time, the key stays removed.
    pub fn update_present(
        &mut self,
        k: &K,
        f: impl FnOnce(&mut V) -> V::Delta,
    ) -> Option<OrMapDelta<K, V>> {
        let mut v = self.get(k)?.clone();
        let vdelta = f(&mut v);
        let delta = OrMapDelta {
            dots: BTreeMap::new(),
            values: BTreeMap::from([(k.clone(), vdelta)]),
            ctx: DotContext::new(),
        };
        self.merge(delta.clone());
        Some(delta)
    }

    /// Removes `k` by cancelling the dots this copy holds for it. The value is
    /// kept, as described on `OrMap`.
    pub fn remove(&mut self, k: &K) -> OrMapDelta<K, V> {
        let mut dctx = DotContext::new();
        if let Some(dots) = self.dots.get(k) {
            for &d in dots {
                dctx.insert(d);
            }
        }
        let delta = OrMapDelta {
            dots: BTreeMap::new(),
            values: BTreeMap::new(),
            ctx: dctx,
        };
        self.merge(delta.clone());
        delta
    }

    /// The value at `k`, or `None` if the key is not present.
    pub fn get(&self, k: &K) -> Option<&V> {
        match self.dots.get(k) {
            Some(s) if !s.is_empty() => self.values.get(k),
            _ => None,
        }
    }

    /// The keys currently present.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.dots
            .iter()
            .filter(|(_, s)| !s.is_empty())
            .map(|(k, _)| k)
    }
}

impl<K, V> DeltaCrdt for OrMap<K, V>
where
    K: Ord + Clone,
    V: DeltaCrdt + Default + Clone,
    V::Delta: Clone,
{
    type Delta = OrMapDelta<K, V>;

    fn merge(&mut self, mut delta: OrMapDelta<K, V>) {
        // A key's dot counts as seen once it is held. See `count_own_dots`.
        count_own_dots(&mut delta.ctx, delta.dots.values().flatten().copied());

        // 1. Merge the values. This happens whether or not the key is present.
        for (k, vdelta) in delta.values {
            self.values.entry(k).or_default().merge(vdelta);
        }

        // 2. Merge which keys are present, using the same add-wins rule as the
        // OR-Set. A key that loses its last dot keeps its value. See "How
        // removal works" on `OrMap`.
        merge_presence(&mut self.dots, &mut self.index, &self.ctx, &delta.dots, &delta.ctx);

        self.ctx.merge(&delta.ctx);
    }

    fn snapshot(&self) -> OrMapDelta<K, V> {
        OrMapDelta {
            dots: self.dots.clone(),
            values: self
                .values
                .iter()
                .map(|(k, v)| (k.clone(), v.snapshot()))
                .collect(),
            ctx: self.ctx.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dot_index::{check_presence, looked_at};
    use crate::counter::PnCounter;

    #[test]
    fn update_creates_key_and_value_merges() {
        let mut a = OrMap::<&str, PnCounter>::default();
        a.update("apple", ReplicaId(1), |c| c.increment(ReplicaId(1), 5)).unwrap();
        assert_eq!(a.get(&"apple").map(|c| c.value()), Some(5));

        // Increments from two replicas on the same key add up.
        let mut b = OrMap::<&str, PnCounter>::default();
        b.merge(a.snapshot());
        let d = b.update("apple", ReplicaId(2), |c| c.increment(ReplicaId(2), 3)).unwrap();
        a.merge(d);
        assert_eq!(a.get(&"apple").map(|c| c.value()), Some(8));
    }

    #[test]
    fn remove_hides_key() {
        let mut m = OrMap::<&str, PnCounter>::default();
        m.update("x", ReplicaId(1), |c| c.increment(ReplicaId(1), 1)).unwrap();
        assert!(m.get(&"x").is_some());
        m.remove(&"x");
        assert!(m.get(&"x").is_none());
        assert_eq!(m.keys().count(), 0);
    }

    #[test]
    fn delta_from_parts_round_trips_via_accessors() {
        // Get a delta from `update`, read its parts, rebuild it with
        // `from_parts`, and check the rebuilt delta equals the original.
        let mut m: OrMap<String, PnCounter> = OrMap::default();
        let d = m.update("k".to_string(), ReplicaId(1), |c| {
            c.increment(ReplicaId(1), 3)
        }).unwrap();
        let dots: BTreeMap<_, _> = d.dots().map(|(k, s)| (k.clone(), s.clone())).collect();
        let values: BTreeMap<_, _> = d.values().map(|(k, v)| (k.clone(), v.clone())).collect();
        let rebuilt = OrMapDelta::from_parts(dots, values, d.context().clone());
        assert_eq!(rebuilt, d);
    }

    type Counts = OrMap<u8, PnCounter>;

    /// Checks that `map` is what the old merge makes of `before` and `delta`,
    /// with the dots `delta` carries counted as seen by it, and that the
    /// index still says what the dots say.
    fn compare(map: &Counts, before: Counts, mut delta: OrMapDelta<u8, PnCounter>) {
        count_own_dots(&mut delta.ctx, delta.dots.values().flatten().copied());
        check_presence(
            &before.dots, &before.index, &before.ctx, &delta.dots, &delta.ctx, &map.dots, &map.index,
        );
        let (mut values, mut ctx) = (before.values, before.ctx);
        for (k, vdelta) in delta.values {
            values.entry(k).or_default().merge(vdelta);
        }
        ctx.merge(&delta.ctx);
        assert_eq!(map.values, values, "the two merges hold different values");
        assert_eq!(map.ctx, ctx, "the two merges saw different things");
    }

    // A change to one key looks at that key and no other, however many the
    // map holds. Going through all of them for every change is what made a
    // write slower the more a map held.
    #[test]
    fn a_change_to_one_key_looks_at_one_key() {
        let mut big = OrMap::<u32, PnCounter>::default();
        let mut other = OrMap::<u32, PnCounter>::default();
        for k in 0..1000 {
            let d = big.update(k, ReplicaId(1), |c| c.increment(ReplicaId(1), 1)).unwrap();
            other.merge(d);
        }
        let new_key = other.update(5000, ReplicaId(2), |c| c.increment(ReplicaId(2), 1)).unwrap();
        big.merge(new_key);
        assert_eq!(looked_at(), 1);
        let old_key = other.update(7, ReplicaId(2), |c| c.increment(ReplicaId(2), 1)).unwrap();
        big.merge(old_key);
        assert_eq!(looked_at(), 1);
        let removed = other.remove(&500);
        big.merge(removed);
        assert_eq!(looked_at(), 1);
        assert!(big.get(&500).is_none());
        assert_eq!(big.get(&7).map(|c| c.value()), Some(2));
        // A change to a value that gives the key no new dot looks at no key.
        let value_only = other.update_present(&8, |c| c.increment(ReplicaId(2), 1)).unwrap();
        big.merge(value_only);
        assert_eq!(looked_at(), 0);
        assert_eq!(big.get(&8).map(|c| c.value()), Some(2));
        // A whole copy of another map has seen everything, and so looks at
        // everything. That is as it has to be.
        big.merge(other.snapshot());
        assert_eq!(looked_at(), 1000);
        assert_eq!(big, other);
    }

    // Two maps that hold the same are the same, however they came to it. The
    // index is not part of what they are compared by.
    #[test]
    fn maps_that_hold_the_same_are_equal_whatever_order_things_arrived_in() {
        let dot = Dot::new(ReplicaId(1), 1);
        let under = |key: u8| {
            OrMapDelta::<u8, PnCounter>::from_parts(
                BTreeMap::from([(key, BTreeSet::from([dot]))]),
                BTreeMap::new(),
                DotContext::new(),
            )
        };
        // One dot under two keys, which only a change from elsewhere can do.
        let (mut a, mut b) = (Counts::default(), Counts::default());
        a.merge(under(1));
        a.merge(under(2));
        b.merge(under(2));
        b.merge(under(1));
        assert_eq!(a, b);
    }

    proptest::proptest! {
        // Three copies of a map take random updates and removes, pass
        // changes and whole copies to each other in any order and more than
        // once, and are sent changes no copy made, some with one dot under
        // two keys. After every merge the result has to be what the old
        // merge gives.
        #[test]
        fn the_merge_keeps_what_the_merge_through_all_keys_kept(
            ops in proptest::collection::vec((0u8..6, 0usize..3, 0u8..6, proptest::prelude::any::<u64>()), 0..80)
        ) {
            let mut copies = vec![Counts::default(); 3];
            let mut sent: Vec<OrMapDelta<u8, PnCounter>> = Vec::new();
            for (kind, at, k, more) in ops {
                let more = more as usize;
                let before = copies[at].clone();
                let replica = ReplicaId(at as u64 + 1);
                let delta = match kind {
                    // A change made here goes through the same merge.
                    0 => copies[at].update(k, replica, |c| c.increment(replica, 1)).unwrap(),
                    1 => copies[at].remove(&k),
                    2 => match copies[at].update_present(&k, |c| c.increment(replica, 1)) {
                        Some(delta) => delta,
                        None => continue,
                    },
                    3 if !sent.is_empty() => {
                        let delta = sent[more % sent.len()].clone();
                        copies[at].merge(delta.clone());
                        delta
                    }
                    4 => {
                        let whole = copies[more % 3].snapshot();
                        copies[at].merge(whole.clone());
                        whole
                    }
                    5 => {
                        // A change nobody made: a dot under one key or two,
                        // and a context that has seen a few dots.
                        let dot = Dot::new(ReplicaId((more % 3) as u64 + 1), ((more >> 4) % 6) as u64 + 1);
                        let mut dots = BTreeMap::from([(k, BTreeSet::from([dot]))]);
                        if (more >> 8) % 3 == 0 {
                            dots.insert(k.wrapping_add(1) % 6, BTreeSet::from([dot]));
                        }
                        let mut ctx = DotContext::new();
                        for shift in [12, 20, 28] {
                            if (more >> shift) % 2 == 0 {
                                ctx.insert(Dot::new(
                                    ReplicaId(((more >> (shift + 1)) % 3) as u64 + 1),
                                    ((more >> (shift + 3)) % 6) as u64 + 1,
                                ));
                            }
                        }
                        let delta = OrMapDelta::from_parts(dots, BTreeMap::new(), ctx);
                        copies[at].merge(delta.clone());
                        delta
                    }
                    _ => continue,
                };
                compare(&copies[at], before, delta.clone());
                if kind < 3 || kind == 5 {
                    sent.push(delta);
                }
            }
        }
    }

    #[test]
    fn an_update_with_no_fresh_dot_is_refused_and_changes_nothing() {
        let r = ReplicaId(1);
        let mut m = Counts::default();
        m.merge(OrMapDelta::from_parts(BTreeMap::new(), BTreeMap::new(), crate::context::seen_up_to(r, crate::MAX_COUNTER)));
        let before = m.clone();
        let mut called = false;
        let refused = m.update(1, r, |c| {
            called = true;
            c.increment(r, 1)
        });
        assert_eq!(refused, Err(WriteError::NoFreshDot));
        assert!(!called, "the value is not touched");
        assert_eq!(m, before);
    }

    // The same as for the set: a key's dot that its change did not list as
    // seen is seen once it is held.
    #[test]
    fn a_key_held_counts_as_seen() {
        let r = ReplicaId(7);
        let from_elsewhere = OrMapDelta::<u8, PnCounter>::from_parts(
            BTreeMap::from([(1, BTreeSet::from([Dot::new(r, 1)]))]),
            BTreeMap::new(),
            DotContext::new(),
        );
        let mut m = Counts::default();
        m.merge(from_elsewhere);
        m.update(2, r, |c| c.increment(r, 1)).unwrap();
        assert_eq!(m.keys().copied().collect::<Vec<_>>(), vec![1, 2]);
    }
}
