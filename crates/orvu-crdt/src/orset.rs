//! OR-Set (observed-remove set), in its optimized form: a set where a remove
//! only undoes the adds it has seen. Each add tags the element with a fresh dot
//! (a unique tag for one update). A remove cancels the dots the remover
//! currently holds for that element. If one replica adds an element while
//! another removes it, the add wins.

use crate::context::DotContext;
use crate::error::WriteError;
use crate::dot_index::{count_own_dots, merge_presence, DotIndex};
use crate::id::{Dot, ReplicaId};
use crate::DeltaCrdt;
use std::collections::{BTreeMap, BTreeSet};

/// A set where an add wins over a remove made at the same time.
#[derive(Clone, Debug, Default)]
pub struct OrSet<E: Ord> {
    dots: BTreeMap<E, BTreeSet<Dot>>,
    ctx: DotContext,
    /// Which element each dot tags. Worked out from `dots`, so that a change
    /// can find the elements it touches without going through all of them.
    index: DotIndex<E>,
}

// Two sets are the same if they hold the same dots and have seen the same.
// The index is worked out from the dots and says nothing more.
impl<E: Ord> PartialEq for OrSet<E> {
    fn eq(&self, other: &Self) -> bool {
        self.dots == other.dots && self.ctx == other.ctx
    }
}

impl<E: Ord> Eq for OrSet<E> {}

/// A change to an `OrSet`, to send to other replicas.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct OrSetDelta<E: Ord> {
    dots: BTreeMap<E, BTreeSet<Dot>>,
    ctx: DotContext,
}

impl<E: Ord> OrSetDelta<E> {
    /// How many dots tag `e` in this delta. Meant for diagnostics and tests.
    pub fn dot_count(&self, e: &E) -> usize {
        self.dots.get(e).map_or(0, |s| s.len())
    }

    /// Rebuilds a delta from its map of elements to dots and its context. Used
    /// when decoding a received message.
    pub fn from_parts(dots: BTreeMap<E, BTreeSet<Dot>>, ctx: DotContext) -> Self {
        Self { dots, ctx }
    }

    /// The elements this delta tags, with their dots.
    pub fn elements(&self) -> impl Iterator<Item = (&E, &BTreeSet<Dot>)> {
        self.dots.iter()
    }

    /// The context sent with this delta: every dot the delta knows about,
    /// whether it is still present or has been cancelled.
    pub fn context(&self) -> &DotContext {
        &self.ctx
    }
}

impl<E: Ord + Clone> OrSet<E> {
    /// Adds `e` on behalf of replica `r` and returns the delta to send.
    /// Refuses with `WriteError::NoFreshDot`, changing nothing, if `r` has no
    /// dot left (see `MAX_COUNTER`).
    pub fn add(&mut self, e: E, r: ReplicaId) -> Result<OrSetDelta<E>, WriteError> {
        let d = self.ctx.fresh_dot(r)?;
        let mut dctx = DotContext::new();
        // The new dot replaces every dot this copy already holds for `e`. This
        // copy has seen those dots, so it may cancel them, and listing them in
        // `dctx` is what cancels them. Dots it has not seen are not in `dctx`,
        // so adds made elsewhere at the same time survive.
        if let Some(old) = self.dots.get(&e) {
            for &o in old {
                dctx.insert(o);
            }
        }
        dctx.insert(d);
        let delta = OrSetDelta {
            dots: BTreeMap::from([(e, BTreeSet::from([d]))]),
            ctx: dctx,
        };
        self.merge(delta.clone());
        Ok(delta)
    }

    /// Removes `e` by cancelling the dots this copy holds for it. Adds this copy
    /// has not seen are not affected.
    pub fn remove(&mut self, e: &E) -> OrSetDelta<E> {
        let mut dctx = DotContext::new();
        if let Some(dots) = self.dots.get(e) {
            for &d in dots {
                dctx.insert(d);
            }
        }
        let delta = OrSetDelta { dots: BTreeMap::new(), ctx: dctx };
        self.merge(delta.clone());
        delta
    }

    /// True if `e` is in the set.
    pub fn contains(&self, e: &E) -> bool {
        self.dots.get(e).is_some_and(|s| !s.is_empty())
    }

    /// The elements currently in the set.
    pub fn elements(&self) -> impl Iterator<Item = &E> {
        self.dots.keys()
    }
}

impl<E: Ord + Clone> DeltaCrdt for OrSet<E> {
    type Delta = OrSetDelta<E>;

    fn merge(&mut self, mut delta: OrSetDelta<E>) {
        count_own_dots(&mut delta.ctx, delta.dots.values().flatten().copied());
        merge_presence(&mut self.dots, &mut self.index, &self.ctx, &delta.dots, &delta.ctx);
        self.ctx.merge(&delta.ctx);
    }

    fn snapshot(&self) -> OrSetDelta<E> {
        OrSetDelta { dots: self.dots.clone(), ctx: self.ctx.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dot_index::{check_presence, looked_at};

    #[test]
    fn add_then_remove_removes() {
        let mut s = OrSet::<&str>::default();
        s.add("apple", ReplicaId(1)).unwrap();
        assert!(s.contains(&"apple"));
        s.remove(&"apple");
        assert!(!s.contains(&"apple"));
    }

    #[test]
    fn concurrent_add_wins_over_remove() {
        // Replica 1 adds "x" and both copies learn about it.
        let mut a = OrSet::<&str>::default();
        let add1 = a.add("x", ReplicaId(1)).unwrap();
        let mut b = OrSet::<&str>::default();
        b.merge(add1);

        // At the same time, `a` removes "x" and `b` adds "x" again. The second
        // add has a new dot that `a` has not seen.
        let rem = a.remove(&"x");
        let add2 = b.add("x", ReplicaId(2)).unwrap();

        // Exchange deltas.
        a.merge(add2);
        b.merge(rem);

        // The second add survives on both copies.
        assert!(a.contains(&"x"));
        assert!(b.contains(&"x"));
        assert_eq!(a, b);
    }

    #[test]
    fn repeated_adds_supersede_prior_dots() {
        let mut s = OrSet::<&str>::default();
        for _ in 0..5 {
            s.add("x", ReplicaId(1)).unwrap();
        }
        assert!(s.contains(&"x"));
        // Each add replaced the previous dot, so one dot is left, not five.
        assert_eq!(s.snapshot().dot_count(&"x"), 1);
    }

    #[test]
    fn or_set_delta_round_trips_through_parts() {
        let mut s = OrSet::<String>::default();
        let d = s.add("apple".to_string(), ReplicaId(1)).unwrap();

        let dots = d
            .elements()
            .map(|(e, set)| (e.clone(), set.clone()))
            .collect();
        let rebuilt = OrSetDelta::from_parts(dots, d.context().clone());
        assert_eq!(rebuilt, d);

        let mut applied = OrSet::<String>::default();
        applied.merge(rebuilt);
        assert!(applied.contains(&"apple".to_string()));
    }

    /// Merges `delta` into `set` and checks that the result is what the old
    /// merge gives, and that the index still says what the dots say.
    fn merge_and_compare(set: &mut OrSet<u8>, delta: OrSetDelta<u8>) {
        let before = set.clone();
        set.merge(delta.clone());
        compare(set, &before, &delta);
    }

    /// Checks that `set` is what the old merge makes of `before` and `delta`,
    /// with the dots `delta` carries counted as seen by it.
    fn compare(set: &OrSet<u8>, before: &OrSet<u8>, delta: &OrSetDelta<u8>) {
        let mut their_ctx = delta.ctx.clone();
        count_own_dots(&mut their_ctx, delta.dots.values().flatten().copied());
        check_presence(
            &before.dots, &before.index, &before.ctx, &delta.dots, &their_ctx, &set.dots, &set.index,
        );
        let mut ctx = before.ctx.clone();
        ctx.merge(&their_ctx);
        assert_eq!(set.ctx, ctx, "the two merges saw different things");
    }

    // A change to one element looks at that element and no other, however
    // many the set holds. Going through all of them for every change is what
    // made a write slower the more a set held.
    #[test]
    fn a_change_to_one_element_looks_at_one_element() {
        let mut big = OrSet::<u32>::default();
        let mut other = OrSet::<u32>::default();
        for e in 0..1000 {
            let d = big.add(e, ReplicaId(1)).unwrap();
            other.merge(d);
        }
        let added = other.add(5000, ReplicaId(2)).unwrap();
        big.merge(added);
        assert_eq!(looked_at(), 1);
        let added_again = other.add(7, ReplicaId(2)).unwrap();
        big.merge(added_again);
        assert_eq!(looked_at(), 1);
        let removed = other.remove(&500);
        big.merge(removed);
        assert_eq!(looked_at(), 1);
        assert!(!big.contains(&500) && big.contains(&5000) && big.contains(&7));
        // A change that touches nothing here looks at nothing.
        let again = other.remove(&500);
        big.merge(again);
        assert_eq!(looked_at(), 0);
        // A whole copy of another set has seen everything, and so looks at
        // everything. That is as it has to be.
        big.merge(other.snapshot());
        assert_eq!(looked_at(), 1000);
        assert_eq!(big, other);
    }

    proptest::proptest! {
        // Three copies of a set take random adds and removes, pass changes
        // and whole copies to each other in any order and more than once,
        // and are sent changes no copy made, some with one dot under two
        // elements. After every merge the result has to be what the old
        // merge gives.
        #[test]
        fn the_merge_keeps_what_the_merge_through_all_elements_kept(
            ops in proptest::collection::vec((0u8..5, 0usize..3, 0u8..6, proptest::prelude::any::<u64>()), 0..80)
        ) {
            let mut copies = vec![OrSet::<u8>::default(); 3];
            let mut sent: Vec<OrSetDelta<u8>> = Vec::new();
            for (kind, at, e, more) in ops {
                let more = more as usize;
                match kind {
                    0 | 1 => {
                        // A change made here goes through the same merge.
                        let before = copies[at].clone();
                        let delta = if kind == 0 {
                            copies[at].add(e, ReplicaId(at as u64 + 1)).unwrap()
                        } else {
                            copies[at].remove(&e)
                        };
                        compare(&copies[at], &before, &delta);
                        sent.push(delta);
                    }
                    2 if !sent.is_empty() => {
                        let delta = sent[more % sent.len()].clone();
                        merge_and_compare(&mut copies[at], delta);
                    }
                    3 => {
                        let whole = copies[more % 3].snapshot();
                        merge_and_compare(&mut copies[at], whole);
                    }
                    4 => {
                        // A change nobody made: a dot under one element or
                        // two, and a context that has seen a few dots.
                        let dot = Dot::new(ReplicaId((more % 3) as u64 + 1), ((more >> 4) % 6) as u64 + 1);
                        let mut dots = BTreeMap::from([(e, BTreeSet::from([dot]))]);
                        if (more >> 8) % 3 == 0 {
                            dots.insert(e.wrapping_add(1) % 6, BTreeSet::from([dot]));
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
                        let delta = OrSetDelta::from_parts(dots, ctx);
                        sent.push(delta.clone());
                        merge_and_compare(&mut copies[at], delta);
                    }
                    _ => {}
                }
            }
        }
    }

    // A machine with no fresh dot left cannot add. Reusing its last dot would
    // let the add replace, or be dropped behind, the one that holds it.
    #[test]
    fn an_add_with_no_fresh_dot_is_refused_and_changes_nothing() {
        let r = ReplicaId(1);
        let mut s = OrSet::<u8>::default();
        s.merge(OrSetDelta::from_parts(BTreeMap::new(), crate::context::seen_up_to(r, crate::MAX_COUNTER)));
        let before = s.clone();
        assert_eq!(s.add(1, r), Err(WriteError::NoFreshDot));
        assert_eq!(s, before);
        assert!(s.add(1, ReplicaId(2)).is_ok());
    }

    // An element from elsewhere whose dot its change did not list as seen
    // is still seen once it is held. Otherwise the next add here could be
    // given the same dot, and the element that came first would be dropped.
    #[test]
    fn an_element_held_counts_as_seen() {
        let r = ReplicaId(7);
        let from_elsewhere = OrSetDelta::from_parts(
            BTreeMap::from([("a", BTreeSet::from([Dot::new(r, 1)]))]),
            DotContext::new(),
        );
        let mut s = OrSet::<&str>::default();
        s.merge(from_elsewhere);
        let added = s.add("b", r).unwrap();
        assert_eq!(added.elements().next().unwrap().1, &BTreeSet::from([Dot::new(r, 2)]));
        assert!(s.contains(&"a") && s.contains(&"b"));
    }

    // Two changes that each carry one dot under a different element can only
    // come from a broken or hostile sender. The dot goes from both elements,
    // whichever change arrives first, so every copy agrees.
    #[test]
    fn one_dot_under_two_elements_in_two_changes_goes_from_both() {
        let dot = Dot::new(ReplicaId(1), 1);
        let under = |e: u8| OrSetDelta::from_parts(BTreeMap::from([(e, BTreeSet::from([dot]))]), DotContext::new());
        let (mut a, mut b) = (OrSet::<u8>::default(), OrSet::<u8>::default());
        a.merge(under(1));
        a.merge(under(2));
        b.merge(under(2));
        b.merge(under(1));
        assert_eq!(a, b);
        assert_eq!(a.elements().count(), 0);
    }
}
