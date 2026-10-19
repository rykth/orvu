//! Multi-value map: a map whose keys each hold every value written to them at
//! the same time. A write replaces the values it has seen. Two writes to one
//! key made at the same time, neither having seen the other, both stay, and
//! every reader sees both until a later write replaces them.
//!
//! Each write tags its value with a fresh dot (a unique tag for one write). A
//! key holds one value per live dot. Merging uses the same add-wins rule as
//! the OR-Set: a dot survives unless the other side has seen it and dropped
//! it.

use crate::context::DotContext;
use crate::error::WriteError;
use crate::dot_index::{count_own_dots, merge_presence, DotIndex};
use crate::id::{Dot, ReplicaId};
use crate::DeltaCrdt;
use std::collections::{BTreeMap, BTreeSet};

/// A map that keeps every value written to a key at the same time, instead of
/// picking one. A later write that has seen them replaces them all.
///
/// Unlike `OrMap`, a value that has been replaced or removed is dropped. A
/// value is tied to the dot it was written with and never changes, so a copy
/// that has seen the dot go knows not to take it back.
#[derive(Clone, Debug)]
pub struct MvMap<K: Ord, V> {
    dots: BTreeMap<K, BTreeSet<Dot>>,
    /// The value written with each live dot, by key.
    values: BTreeMap<K, BTreeMap<Dot, V>>,
    ctx: DotContext,
    /// Which key each dot tags. Worked out from `dots`, so that a change can
    /// find the keys it touches without going through all of them.
    index: DotIndex<K>,
}

impl<K: Ord, V> Default for MvMap<K, V> {
    fn default() -> Self {
        Self {
            dots: BTreeMap::new(),
            values: BTreeMap::new(),
            ctx: DotContext::new(),
            index: DotIndex::default(),
        }
    }
}

// Two maps are the same if they hold the same values and have seen the same.
// The dots and the index are worked out from the values.
impl<K: Ord, V: PartialEq> PartialEq for MvMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values && self.ctx == other.ctx
    }
}

impl<K: Ord, V: Eq> Eq for MvMap<K, V> {}

/// A change to an `MvMap`, to send to other replicas.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MvMapDelta<K: Ord, V> {
    entries: BTreeMap<K, BTreeMap<Dot, V>>,
    ctx: DotContext,
}

impl<K: Ord, V> Default for MvMapDelta<K, V> {
    fn default() -> Self {
        Self { entries: BTreeMap::new(), ctx: DotContext::new() }
    }
}

impl<K: Ord, V> MvMapDelta<K, V> {
    /// Rebuilds a delta from its parts. Used when decoding a received message.
    pub fn from_parts(entries: BTreeMap<K, BTreeMap<Dot, V>>, ctx: DotContext) -> Self {
        Self { entries, ctx }
    }

    /// The values this delta carries, by key, each with its dot.
    pub fn entries(&self) -> impl Iterator<Item = (&K, &BTreeMap<Dot, V>)> {
        self.entries.iter()
    }

    /// The context sent with this delta: every dot the delta knows about,
    /// whether it is still present or has been dropped.
    pub fn context(&self) -> &DotContext {
        &self.ctx
    }
}

impl<K: Ord, V: Ord> MvMapDelta<K, V> {
    /// Adds the value `v`, written with dot `d`, at `k`. Used when decoding a
    /// received message. A dot given a second value keeps the one a merge
    /// would keep.
    pub fn add(&mut self, k: K, d: Dot, v: V) {
        keep(self.entries.entry(k).or_default(), d, v);
    }
}

/// Puts the value `v`, written with dot `d`, among `vals`. One dot is only
/// ever written with one value. A change from elsewhere could still send
/// another, and every copy has to keep the same one, so the smaller wins.
fn keep<V: Ord>(vals: &mut BTreeMap<Dot, V>, d: Dot, v: V) {
    match vals.get(&d) {
        Some(have) if *have <= v => {}
        _ => {
            vals.insert(d, v);
        }
    }
}

impl<K, V> MvMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    /// Writes `v` at `k` on behalf of replica `r` and returns the delta to
    /// send. The write replaces every value this copy holds for `k`. Values
    /// written elsewhere that this copy has not seen yet are kept beside it.
    ///
    /// Refuses with `WriteError::NoFreshDot`, changing nothing, if `r` has no
    /// dot left (see `MAX_COUNTER`).
    pub fn put(&mut self, k: K, v: V, r: ReplicaId) -> Result<MvMapDelta<K, V>, WriteError> {
        let d = self.ctx.fresh_dot(r)?;
        let mut dctx = self.seen(&k);
        dctx.insert(d);
        let delta = MvMapDelta {
            entries: BTreeMap::from([(k, BTreeMap::from([(d, v)]))]),
            ctx: dctx,
        };
        self.merge(delta.clone());
        Ok(delta)
    }

    /// Removes every value this copy holds for `k`. A value written elsewhere
    /// that this copy has not seen yet survives the remove.
    pub fn remove(&mut self, k: &K) -> MvMapDelta<K, V> {
        let delta = MvMapDelta { entries: BTreeMap::new(), ctx: self.seen(k) };
        self.merge(delta.clone());
        delta
    }

    /// A context listing the dots this copy holds for `k`. Sending it is what
    /// drops them elsewhere.
    fn seen(&self, k: &K) -> DotContext {
        let mut dctx = DotContext::new();
        for &d in self.dots.get(k).into_iter().flatten() {
            dctx.insert(d);
        }
        dctx
    }

    /// The values at `k`, with repeats left out, in the order their writes'
    /// dots sort in. Empty if the key is not present. More than one value
    /// means writes made at the same time clashed and nothing has settled it.
    pub fn get(&self, k: &K) -> Vec<&V> {
        let mut had = BTreeSet::new();
        self.values
            .get(k)
            .into_iter()
            .flat_map(|vals| vals.values())
            .filter(|v| had.insert(*v))
            .collect()
    }

    /// The keys currently present.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.values.keys()
    }

    /// Every key present with its values, as `get` gives them.
    pub fn entries(&self) -> impl Iterator<Item = (&K, Vec<&V>)> {
        self.values.keys().map(|k| (k, self.get(k)))
    }
}

impl<K, V> DeltaCrdt for MvMap<K, V>
where
    K: Ord + Clone,
    V: Ord + Clone,
{
    type Delta = MvMapDelta<K, V>;

    fn merge(&mut self, mut delta: MvMapDelta<K, V>) {
        // A value is written with its dot, so the change has seen that dot,
        // whether or not its context says so. See `count_own_dots`.
        count_own_dots(&mut delta.ctx, delta.entries.values().flat_map(|vals| vals.keys().copied()));
        let their_dots: BTreeMap<K, BTreeSet<Dot>> = delta
            .entries
            .iter()
            .map(|(k, vals)| (k.clone(), vals.keys().copied().collect()))
            .collect();

        // The keys whose values can change are the ones whose dots can: the
        // ones the change carries values for, and the ones holding a dot the
        // change has seen. Merging the dots works them out, and they are
        // taken from there rather than worked out a second time.
        let touched = merge_presence(&mut self.dots, &mut self.index, &self.ctx, &their_dots, &delta.ctx);
        #[cfg(test)]
        LOOKED_AT.with(|n| n.set(touched.len()));
        self.ctx.merge(&delta.ctx);

        let mut theirs = delta.entries;
        for k in touched {
            let live = self.dots.get(&k);
            let mut kept = self.values.remove(&k).unwrap_or_default();
            kept.retain(|d, _| live.is_some_and(|l| l.contains(d)));
            for (d, v) in theirs.remove(&k).unwrap_or_default() {
                if live.is_some_and(|l| l.contains(&d)) {
                    keep(&mut kept, d, v);
                }
            }
            if !kept.is_empty() {
                self.values.insert(k, kept);
            }
        }
    }

    fn snapshot(&self) -> MvMapDelta<K, V> {
        MvMapDelta { entries: self.values.clone(), ctx: self.ctx.clone() }
    }
}

#[cfg(test)]
thread_local! {
    static LOOKED_AT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many keys the last merge on this thread looked at for their values.
#[cfg(test)]
fn looked_at() -> usize {
    LOOKED_AT.with(|n| n.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Notes = MvMap<String, String>;

    fn s(x: &str) -> String {
        x.to_string()
    }

    fn values(m: &Notes, k: &str) -> Vec<String> {
        m.get(&s(k)).into_iter().cloned().collect()
    }

    #[test]
    fn a_put_is_read_back() {
        let mut m = Notes::default();
        m.put(s("plan"), s("ship it"), ReplicaId(1)).unwrap();
        assert_eq!(values(&m, "plan"), vec![s("ship it")]);
        assert_eq!(m.keys().cloned().collect::<Vec<_>>(), vec![s("plan")]);
    }

    #[test]
    fn writes_made_at_the_same_time_are_both_kept() {
        let (mut a, mut b) = (Notes::default(), Notes::default());
        let from_a = a.put(s("plan"), s("ship it"), ReplicaId(1)).unwrap();
        let from_b = b.put(s("plan"), s("wait"), ReplicaId(2)).unwrap();
        a.merge(from_b);
        b.merge(from_a);
        assert_eq!(values(&a, "plan"), vec![s("ship it"), s("wait")]);
        assert_eq!(a, b);
    }

    #[test]
    fn a_write_that_saw_the_clash_settles_it() {
        let (mut a, mut b) = (Notes::default(), Notes::default());
        let from_a = a.put(s("plan"), s("ship it"), ReplicaId(1)).unwrap();
        let from_b = b.put(s("plan"), s("wait"), ReplicaId(2)).unwrap();
        a.merge(from_b);
        b.merge(from_a);
        let settle = a.put(s("plan"), s("ship friday"), ReplicaId(1)).unwrap();
        b.merge(settle);
        assert_eq!(values(&a, "plan"), vec![s("ship friday")]);
        assert_eq!(a, b);
        // The values it replaced are gone, not just hidden.
        assert_eq!(b.values[&s("plan")].len(), 1);
    }

    #[test]
    fn a_later_write_replaces_the_earlier_one() {
        let (mut a, mut b) = (Notes::default(), Notes::default());
        let one = a.put(s("plan"), s("one"), ReplicaId(1)).unwrap();
        let two = a.put(s("plan"), s("two"), ReplicaId(1)).unwrap();
        b.merge(one);
        b.merge(two);
        assert_eq!(values(&b, "plan"), vec![s("two")]);
    }

    #[test]
    fn the_same_value_written_twice_at_once_reads_once() {
        let (mut a, mut b) = (Notes::default(), Notes::default());
        let from_a = a.put(s("plan"), s("same"), ReplicaId(1)).unwrap();
        b.put(s("plan"), s("same"), ReplicaId(2)).unwrap();
        b.merge(from_a);
        assert_eq!(values(&b, "plan"), vec![s("same")]);
    }

    #[test]
    fn a_remove_drops_what_it_saw_and_keeps_what_it_did_not() {
        let (mut a, mut b) = (Notes::default(), Notes::default());
        let first = a.put(s("plan"), s("one"), ReplicaId(1)).unwrap();
        b.merge(first);
        let removed = b.remove(&s("plan"));
        assert!(b.get(&s("plan")).is_empty());
        assert_eq!(b.keys().count(), 0);
        // A write made at the same time as the remove survives it.
        let at_once = a.put(s("plan"), s("two"), ReplicaId(1)).unwrap();
        a.merge(removed);
        b.merge(at_once);
        assert_eq!(values(&a, "plan"), vec![s("two")]);
        assert_eq!(a, b);
    }

    #[test]
    fn a_dropped_value_does_not_come_back_late() {
        let (mut a, mut b) = (Notes::default(), Notes::default());
        let old = a.put(s("plan"), s("old"), ReplicaId(1)).unwrap();
        let new = a.put(s("plan"), s("new"), ReplicaId(1)).unwrap();
        b.merge(new);
        b.merge(old);
        assert_eq!(values(&b, "plan"), vec![s("new")]);
    }

    #[test]
    fn a_delta_rebuilt_from_its_parts_is_the_same() {
        let mut m = Notes::default();
        let d = m.put(s("k"), s("v"), ReplicaId(1)).unwrap();
        let entries = d.entries().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(MvMapDelta::from_parts(entries, d.context().clone()), d);
    }

    // A value from elsewhere whose dot its change did not list as seen is
    // still seen once it is held. Otherwise the next write here could be
    // given the same dot and be lost behind the value already there.
    #[test]
    fn a_value_held_counts_as_seen() {
        let from_elsewhere = MvMapDelta::from_parts(
            BTreeMap::from([(s("k"), BTreeMap::from([(Dot::new(ReplicaId(7), 1), s("a"))]))]),
            DotContext::new(),
        );
        let mut m = Notes::default();
        m.merge(from_elsewhere);
        m.put(s("k"), s("z"), ReplicaId(7)).unwrap();
        assert_eq!(values(&m, "k"), vec![s("z")]);
    }

    // A change from elsewhere can send any number of values whose dots it
    // does not list as seen. Counting them as seen must not go through the
    // dots already counted once for each of them, which would hold the engine
    // up for as long as the square of their number.
    #[test]
    fn counting_many_values_as_seen_does_not_go_through_them_each_time() {
        // Every other counter, so that none of them follow on from another.
        let values: BTreeMap<Dot, u32> =
            (1..=2000).map(|n| (Dot::new(ReplicaId(1), 2 * n), n as u32)).collect();
        let mut m = MvMap::<u8, u32>::default();
        crate::context::reset_walked();
        m.merge(MvMapDelta::from_parts(BTreeMap::from([(1, values)]), DotContext::new()));
        let walked = crate::context::walked();
        assert!(walked < 2000 * 50, "{walked} dots gone through");
        assert_eq!(m.get(&1).len(), 2000);
    }

    // A change from elsewhere could write two values with one dot. Both
    // sides have to keep the same one, whichever arrives first.
    #[test]
    fn one_dot_with_two_values_settles_the_same_either_way() {
        let dot = Dot::new(ReplicaId(1), 1);
        let with = |v: &str| {
            MvMapDelta::from_parts(
                BTreeMap::from([(s("k"), BTreeMap::from([(dot, s(v))]))]),
                DotContext::new(),
            )
        };
        let (mut a, mut b) = (Notes::default(), Notes::default());
        a.merge(with("x"));
        a.merge(with("y"));
        b.merge(with("y"));
        b.merge(with("x"));
        assert_eq!(a, b);
        assert_eq!(values(&a, "k"), vec![s("x")]);
    }

    thread_local! {
        static COMPARED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A value that counts how often it is compared.
    #[derive(Clone, Debug)]
    struct Counted(u32);

    impl PartialEq for Counted {
        fn eq(&self, other: &Self) -> bool {
            COMPARED.with(|n| n.set(n.get() + 1));
            self.0 == other.0
        }
    }
    impl Eq for Counted {}
    impl PartialOrd for Counted {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for Counted {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            COMPARED.with(|n| n.set(n.get() + 1));
            self.0.cmp(&other.0)
        }
    }

    // A change from elsewhere can give one key any number of values. Reading
    // them must not compare each with every other, which would hold the
    // engine up for as long as the square of their number.
    #[test]
    fn reading_many_values_does_not_compare_each_with_every_other() {
        let values: BTreeMap<Dot, Counted> =
            (1..=2000).map(|n| (Dot::new(ReplicaId(1), n), Counted(n as u32))).collect();
        let mut m = MvMap::<u8, Counted>::default();
        m.merge(MvMapDelta::from_parts(BTreeMap::from([(1, values)]), DotContext::new()));
        COMPARED.with(|n| n.set(0));
        assert_eq!(m.get(&1).len(), 2000);
        let compared = COMPARED.with(|n| n.get());
        assert!(compared < 2000 * 50, "{compared} comparisons");
    }

    // A change to one key looks at that key and no other, however many the
    // map holds.
    #[test]
    fn a_change_to_one_key_looks_at_one_key() {
        let (mut big, mut other) = (MvMap::<u32, u32>::default(), MvMap::<u32, u32>::default());
        for k in 0..1000 {
            let d = big.put(k, k, ReplicaId(1)).unwrap();
            other.merge(d);
        }
        let changed = other.put(7, 70, ReplicaId(2)).unwrap();
        big.merge(changed);
        assert_eq!(crate::dot_index::looked_at(), 1);
        assert_eq!(looked_at(), 1);
        let removed = other.remove(&500);
        big.merge(removed);
        assert_eq!(crate::dot_index::looked_at(), 1);
        assert_eq!(looked_at(), 1);
        assert!(big.get(&500).is_empty());
        assert_eq!(big.get(&7), vec![&70]);
    }

    // A merge works out which keys it touches once. Working them out again
    // for the values is what made each change cost twice what it had to.
    #[test]
    fn a_merge_asks_the_index_once() {
        let (mut big, mut other) = (MvMap::<u32, u32>::default(), MvMap::<u32, u32>::default());
        for k in 0..1000 {
            let d = big.put(k, k, ReplicaId(1)).unwrap();
            other.merge(d);
        }
        for change in [other.put(7, 70, ReplicaId(2)).unwrap(), other.remove(&500)] {
            crate::dot_index::reset_asked();
            big.merge(change);
            assert_eq!(crate::dot_index::asked(), 1);
        }
        assert_eq!(big, other);
    }

    // A machine at the last dot used to give its next put that dot again.
    // The put said it had worked, but a value that sorts after the one
    // already held under that dot was dropped. Now it is refused.
    #[test]
    fn a_put_with_no_fresh_dot_is_refused_and_changes_nothing() {
        let r = ReplicaId(7);
        let top = Dot::new(r, crate::MAX_COUNTER);
        let mut m = Notes::default();
        m.merge(MvMapDelta::from_parts(BTreeMap::from([(s("k"), BTreeMap::from([(top, s("a"))]))]), DotContext::new()));
        let before = m.clone();
        assert_eq!(m.put(s("k"), s("z"), r), Err(WriteError::NoFreshDot));
        assert_eq!(m, before);
        assert_eq!(values(&m, "k"), vec![s("a")]);
    }
}
