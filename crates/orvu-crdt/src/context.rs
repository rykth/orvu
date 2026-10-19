//! A causal context: the record of which updates a replica has seen. Each
//! update is named by a dot, a unique tag. The record is a dotted version
//! vector: a version vector (how far we have seen each replica's updates with
//! none missing) plus a "cloud" of dots that arrived ahead of earlier ones.
//! The add-wins OR-Set and OR-Map use it to tell an entry that was removed
//! from one that was never seen.

use crate::error::WriteError;
use crate::id::{Dot, ReplicaId};
use crate::version_vector::VersionVector;
use std::collections::BTreeSet;

/// The highest counter a dot may have: 2^62.
///
/// A machine would need more writes than it could make in a lifetime to get
/// near it, so only a broken or hostile sender ever sends a counter above it,
/// and such a message is refused when it is decoded. Keeping well clear of
/// `u64::MAX` means a counter here always has room for one more, so nothing
/// has to stop at the top and reuse its last number. A machine that has used
/// every counter up to this one is refused a new dot (see `fresh_dot`).
///
/// The same limit holds both ways: a counter up to it is taken in from
/// another machine, and a write here never makes one above it. So a write
/// that succeeds here is never refused elsewhere for its counter. A counter
/// taken in can use up the counters left here, though: a list inserts after the
/// highest counter it holds from anyone, and a set or map goes on from the
/// highest of its own counters it has seen. A hostile machine that sends a
/// counter at the limit leaves no fresh dot, and every later write to that
/// object is refused here with `WriteError::NoFreshDot`. That is the worst it
/// can do: the writer is told, and the copies never drift apart.
pub const MAX_COUNTER: u64 = 1 << 62;

/// The set of dots a replica has seen, stored compactly.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DotContext {
    vv: VersionVector,
    cloud: BTreeSet<Dot>,
}

impl DotContext {
    pub fn new() -> Self {
        Self { vv: VersionVector::new(), cloud: BTreeSet::new() }
    }

    /// Rebuilds a context from its two parts. Used when decoding a received
    /// message.
    ///
    /// The result is compacted: cloud dots that follow straight on from `vv`
    /// are folded into it. So the same set of dots gives the same context,
    /// however it was split between the two parts.
    pub fn from_parts(vv: VersionVector, cloud: impl IntoIterator<Item = Dot>) -> Self {
        let mut ctx = Self {
            vv,
            cloud: cloud.into_iter().collect(),
        };
        ctx.compact();
        ctx
    }

    /// For each replica, how far its updates have been seen with none missing.
    pub fn version_vector(&self) -> &VersionVector {
        &self.vv
    }

    /// Dots that arrived ahead of earlier ones from the same replica, so they
    /// cannot be folded into the version vector yet.
    pub fn cloud(&self) -> impl Iterator<Item = Dot> + '_ {
        self.cloud.iter().copied()
    }

    /// True if `self` has seen everything `other` has.
    ///
    /// Use this to decide whether a delta from another replica is ready to
    /// apply. It is ready only once the local context covers the context its
    /// author relied on.
    ///
    /// If our version vector is at least as high as theirs, we have their whole
    /// unbroken run of updates. That leaves only their cloud dots to check one
    /// by one.
    pub fn covers(&self, other: &DotContext) -> bool {
        self.vv.dominates(&other.vv) && other.cloud.iter().all(|&d| self.contains(d))
    }

    /// True if this dot has been seen, whether it is covered by the version
    /// vector or sits in the cloud.
    pub fn contains(&self, d: Dot) -> bool {
        self.vv.get(d.replica) >= d.counter || self.cloud.contains(&d)
    }

    fn max_for(&self, r: ReplicaId) -> u64 {
        // The cloud is sorted by replica and then by counter, so `r`'s
        // highest cloud dot is the last of its own, found without going
        // through anyone else's.
        #[cfg(test)]
        WALKED.with(|n| n.set(n.get() + 1));
        let clouded = self.of(r).next_back().map_or(0, |d| d.counter);
        self.vv.get(r).max(clouded)
    }

    /// `r`'s dots in the cloud, in order.
    fn of(&self, r: ReplicaId) -> std::collections::btree_set::Range<'_, Dot> {
        self.cloud.range(Dot::new(r, 0)..=Dot::new(r, u64::MAX))
    }

    /// The next unused dot for `r`. This does not record the dot. Call `insert`
    /// to do that.
    ///
    /// Counters stop at `MAX_COUNTER`. Once `r` has used it there is no
    /// unused dot left, and this refuses with `WriteError::NoFreshDot` rather
    /// than hand out a dot that is already taken.
    pub fn fresh_dot(&self, r: ReplicaId) -> Result<Dot, WriteError> {
        let next = self.max_for(r).saturating_add(1);
        if next > MAX_COUNTER {
            return Err(WriteError::NoFreshDot);
        }
        Ok(Dot::new(r, next))
    }

    /// How many fresh dots `r` has left: how many writes it can still make
    /// against this context, each recording its dot before the next.
    pub(crate) fn room(&self, r: ReplicaId) -> u64 {
        MAX_COUNTER.saturating_sub(self.max_for(r))
    }

    /// Records that dot `d` has been seen.
    pub fn insert(&mut self, d: Dot) {
        if self.contains(d) {
            return;
        }
        self.cloud.insert(d);
        self.settle(d.replica);
    }

    /// Takes in every dot `other` has seen.
    ///
    /// Only the replicas `other` tells us something new about are put back
    /// into the standard form, so the cost follows the size of `other`, not
    /// of the dots we already hold.
    pub fn merge(&mut self, other: &DotContext) {
        let mut touched = BTreeSet::new();
        for (r, c) in other.vv.iter() {
            if c > self.vv.get(r) {
                touched.insert(r);
            }
        }
        self.vv.merge(&other.vv);
        for &d in &other.cloud {
            if !self.contains(d) {
                self.cloud.insert(d);
                touched.insert(d.replica);
            }
        }
        for r in touched {
            self.settle(r);
        }
    }

    /// Puts `r`'s part of the context into the standard form that `compact`
    /// gives the whole of it: cloud dots the version vector now covers are
    /// dropped, and a run that follows straight on from it is folded in. The
    /// other replicas' dots are not looked at.
    fn settle(&mut self, r: ReplicaId) {
        let mut top = self.vv.get(r);
        let covered: Vec<Dot> = self.of(r).take_while(|d| d.counter <= top).copied().collect();
        #[cfg(test)]
        WALKED.with(|n| n.set(n.get() + covered.len() + 1));
        for d in covered {
            self.cloud.remove(&d);
        }
        // A counter at `u64::MAX` has nothing after it.
        while let Some(next) = top.checked_add(1) {
            if !self.cloud.remove(&Dot::new(r, next)) {
                break;
            }
            #[cfg(test)]
            WALKED.with(|n| n.set(n.get() + 1));
            top = next;
        }
        self.vv.observe(r, top);
    }

    /// Puts the context into its one standard form. Cloud dots that follow
    /// straight on from the version vector are folded into it, and then any
    /// cloud dot the version vector already covers is dropped.
    pub fn compact(&mut self) {
        // The cloud is sorted by replica and then by counter. So one pass in
        // that order folds a whole run of dots, each following on from the
        // last. A counter at `u64::MAX` has nothing after it, which
        // `checked_add` takes care of.
        #[cfg(test)]
        WALKED.with(|n| n.set(n.get() + self.cloud.len()));
        for d in &self.cloud {
            if self.vv.get(d.replica).checked_add(1) == Some(d.counter) {
                self.vv.observe(d.replica, d.counter);
            }
        }
        // Drop cloud dots the version vector covers, both the ones just folded
        // in and any that a merge moved the version vector past. `contains` is
        // already true for such a dot, so keeping it adds nothing. It would
        // waste memory, and two contexts that have seen the same updates would
        // compare unequal. The rules `merge` must follow depend on them
        // comparing equal.
        self.cloud.retain(|d| d.counter > self.vv.get(d.replica));
    }
}

#[cfg(test)]
thread_local! {
    static WALKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many cloud dots compacting has gone through on this thread since
/// `reset_walked`.
#[cfg(test)]
pub(crate) fn walked() -> usize {
    WALKED.with(|n| n.get())
}

#[cfg(test)]
pub(crate) fn reset_walked() {
    WALKED.with(|n| n.set(0));
}

/// A context that has seen `r`'s dots up to `counter`, for tests that need
/// a machine close to running out of dots.
#[cfg(test)]
pub(crate) fn seen_up_to(r: ReplicaId, counter: u64) -> DotContext {
    let mut vv = VersionVector::new();
    vv.observe(r, counter);
    DotContext::from_parts(vv, [])
}

/// Works out which of one element's dots survive when two replicas merge.
/// Each side passes the dots it holds for the element, and its context.
///
/// A dot is kept if:
/// - both sides hold it, or
/// - I hold it and their context has never seen it, or
/// - they hold it and my context has never seen it.
///
/// If one side holds a dot that the other side has seen but does not hold, the
/// other side removed it, so it is dropped. A dot the other side has never
/// seen is an add made at the same time, so it survives. This is the
/// "add-wins" rule: when an add and a remove happen at once, the add wins.
pub(crate) fn keep_dots(
    mine: &BTreeSet<Dot>,
    theirs: &BTreeSet<Dot>,
    my_ctx: &DotContext,
    their_ctx: &DotContext,
) -> BTreeSet<Dot> {
    let mut kept: BTreeSet<Dot> = mine.intersection(theirs).copied().collect();
    for &d in mine {
        if !their_ctx.contains(d) {
            kept.insert(d);
        }
    }
    for &d in theirs {
        if !my_ctx.contains(d) {
            kept.insert(d);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    // Contexts are built from bytes another machine sent, so even impossible
    // numbers must not crash.
    #[test]
    fn the_highest_counter_does_not_crash() {
        let mut vv = VersionVector::new();
        vv.observe(ReplicaId(1), u64::MAX);
        let ctx = DotContext::from_parts(vv, [Dot::new(ReplicaId(1), 5)]);
        assert!(ctx.contains(Dot::new(ReplicaId(1), u64::MAX)));
        assert_eq!(ctx.cloud().count(), 0);
        assert_eq!(ctx.fresh_dot(ReplicaId(1)), Err(WriteError::NoFreshDot));
    }

    // Past the cap there is no fresh dot, and handing out the last one again
    // would give two writes one name. So it is refused.
    #[test]
    fn no_fresh_dot_past_the_cap() {
        let r = ReplicaId(1);
        let near = seen_up_to(r, MAX_COUNTER - 1);
        assert_eq!(near.fresh_dot(r), Ok(Dot::new(r, MAX_COUNTER)));
        assert_eq!(near.room(r), 1);
        let at = seen_up_to(r, MAX_COUNTER);
        assert_eq!(at.fresh_dot(r), Err(WriteError::NoFreshDot));
        assert_eq!(at.room(r), 0);
        // A dot at the cap that waits in the cloud counts too.
        let clouded = DotContext::from_parts(VersionVector::new(), [Dot::new(r, MAX_COUNTER)]);
        assert_eq!(clouded.fresh_dot(r), Err(WriteError::NoFreshDot));
        // Another machine is not held back by it.
        assert_eq!(at.fresh_dot(ReplicaId(2)), Ok(Dot::new(ReplicaId(2), 1)));
    }

    // A long run of dots folds into the version vector in one go, however the
    // sender ordered them. Doing this one dot per pass would take minutes.
    #[test]
    fn a_long_run_of_cloud_dots_folds_quickly() {
        let n = 200_000u64;
        let dots = (1..=n).rev().map(|c| Dot::new(ReplicaId(1), c));
        let started = std::time::Instant::now();
        let ctx = DotContext::from_parts(VersionVector::new(), dots);
        assert_eq!(ctx.version_vector().get(ReplicaId(1)), n);
        assert_eq!(ctx.cloud().count(), 0);
        assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
    }

    #[test]
    fn fresh_dot_then_insert_advances() {
        let mut ctx = DotContext::new();
        let d1 = ctx.fresh_dot(ReplicaId(1)).unwrap();
        assert_eq!(d1, Dot::new(ReplicaId(1), 1));
        ctx.insert(d1);
        assert!(ctx.contains(d1));
        let d2 = ctx.fresh_dot(ReplicaId(1)).unwrap();
        assert_eq!(d2, Dot::new(ReplicaId(1), 2));
    }

    #[test]
    fn compaction_folds_contiguous_cloud() {
        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 2)); // 1 is missing, so 2 waits in the cloud
        assert!(ctx.contains(Dot::new(ReplicaId(1), 2)));
        assert!(!ctx.contains(Dot::new(ReplicaId(1), 1)));
        ctx.insert(Dot::new(ReplicaId(1), 1)); // 1 arrives, so both move into the version vector
        assert!(ctx.contains(Dot::new(ReplicaId(1), 1)));
        assert!(ctx.contains(Dot::new(ReplicaId(1), 2)));
        // The next unused dot for replica 1 is 3.
        assert_eq!(ctx.fresh_dot(ReplicaId(1)).unwrap(), Dot::new(ReplicaId(1), 3));
    }

    #[test]
    fn merge_is_idempotent() {
        let mut a = DotContext::new();
        a.insert(Dot::new(ReplicaId(1), 1));
        let b = a.clone();
        a.merge(&b);
        a.merge(&b);
        assert_eq!(a, b);
    }

    #[test]
    fn merge_prunes_dots_dominated_by_version_vector() {
        // `a` saw only dot 2, with 1 missing. Its version vector is empty and
        // dot 2 sits in the cloud.
        let mut a = DotContext::new();
        a.insert(Dot::new(ReplicaId(1), 2));
        // `b` saw dots 1 and 2 in order. Its version vector is at 2 and its
        // cloud is empty.
        let mut b = DotContext::new();
        b.insert(Dot::new(ReplicaId(1), 1));
        b.insert(Dot::new(ReplicaId(1), 2));
        // Merging `b` moves `a`'s version vector to 2. Dot 2 in `a`'s cloud is
        // now covered and has to be dropped, so that the two contexts, which
        // have seen the same updates, compare equal.
        a.merge(&b);
        assert!(a.contains(Dot::new(ReplicaId(1), 2)));
        assert_eq!(a, b, "causally-identical contexts must be structurally equal");
        assert_eq!(a.fresh_dot(ReplicaId(1)).unwrap(), Dot::new(ReplicaId(1), 3));
    }

    #[test]
    fn accessors_expose_prefix_and_cloud() {
        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 1));
        ctx.insert(Dot::new(ReplicaId(1), 3)); // 2 is missing, so 3 stays in the cloud
        assert_eq!(ctx.version_vector().get(ReplicaId(1)), 1);
        assert_eq!(
            ctx.cloud().collect::<Vec<_>>(),
            vec![Dot::new(ReplicaId(1), 3)]
        );
    }

    #[test]
    fn covers_requires_the_whole_contiguous_prefix() {
        let mut a = DotContext::new();
        a.insert(Dot::new(ReplicaId(1), 1));
        a.insert(Dot::new(ReplicaId(1), 2));
        let mut b = DotContext::new();
        b.insert(Dot::new(ReplicaId(1), 1));
        assert!(a.covers(&b));
        assert!(!b.covers(&a));
        assert!(a.covers(&DotContext::new()), "empty deps are always covered");
    }

    #[test]
    fn covers_accounts_for_clouded_dots() {
        let mut a = DotContext::new();
        a.insert(Dot::new(ReplicaId(1), 2)); // 1 is missing, so this goes to the cloud
        let mut b = DotContext::new();
        b.insert(Dot::new(ReplicaId(1), 2));
        assert!(a.covers(&b), "same clouded dot is covered");

        let mut c = DotContext::new();
        c.insert(Dot::new(ReplicaId(1), 1));
        assert!(!a.covers(&c), "a never observed dot 1");
    }

    // A machine can be sent many dots that do not follow on from each other,
    // and has to keep them all. Each write after that, and each change taken
    // in, must cost the same as before, not go through every dot kept.
    #[test]
    fn many_dots_kept_do_not_slow_each_write() {
        let gapped = (1..=20_000u64).map(|n| Dot::new(ReplicaId(1), 2 * n));
        let mut ctx = DotContext::from_parts(VersionVector::new(), gapped);
        reset_walked();
        for _ in 0..1000 {
            let d = ctx.fresh_dot(ReplicaId(2)).unwrap();
            ctx.insert(d);
        }
        for n in 1..=1000u64 {
            ctx.merge(&DotContext::from_parts(VersionVector::new(), [Dot::new(ReplicaId(3), n)]));
        }
        let walked = walked();
        assert!(walked < 2000 * 50, "{walked} dots gone through");
        assert_eq!(ctx.version_vector().get(ReplicaId(2)), 1000);
        assert_eq!(ctx.version_vector().get(ReplicaId(3)), 1000);
        assert_eq!(ctx.cloud().count(), 20_000);
    }

    /// The context the way it used to be kept up, going through every dot
    /// each time, to check the quicker way against.
    fn compact_as_before(ctx: &mut DotContext) {
        for d in &ctx.cloud {
            if ctx.vv.get(d.replica).checked_add(1) == Some(d.counter) {
                ctx.vv.observe(d.replica, d.counter);
            }
        }
        ctx.cloud.retain(|d| d.counter > ctx.vv.get(d.replica));
    }

    fn max_for_as_before(ctx: &DotContext, r: ReplicaId) -> u64 {
        let clouded = ctx.cloud.iter().filter(|d| d.replica == r).map(|d| d.counter).max();
        ctx.vv.get(r).max(clouded.unwrap_or(0))
    }

    proptest::proptest! {
        // Random dots, some near the highest counter there is, are inserted
        // one at a time or merged in as contexts. After each step the context
        // has to be what the old way gives, and so has the next fresh dot.
        #[test]
        fn the_quick_way_keeps_what_the_old_way_kept(
            ops in proptest::collection::vec(
                (0u8..3, 1u64..4, 0u64..12, proptest::collection::vec((1u64..4, 0u64..12), 0..6)),
                0..60,
            )
        ) {
            let counter = |c: u64| if c >= 9 { u64::MAX - (11 - c) } else { c + 1 };
            let mut ctx = DotContext::new();
            let mut before = DotContext::new();
            for (kind, r, c, more) in ops {
                let d = Dot::new(ReplicaId(r), counter(c));
                if kind == 0 {
                    ctx.insert(d);
                    if !before.contains(d) {
                        before.cloud.insert(d);
                        compact_as_before(&mut before);
                    }
                } else {
                    let mut vv = VersionVector::new();
                    if kind == 2 {
                        vv.observe(ReplicaId(r), counter(c));
                    }
                    let mut other = DotContext { vv, cloud: BTreeSet::new() };
                    for (r, c) in more {
                        other.cloud.insert(Dot::new(ReplicaId(r), counter(c)));
                    }
                    compact_as_before(&mut other);
                    ctx.merge(&other);
                    before.vv.merge(&other.vv);
                    before.cloud.extend(other.cloud.iter().copied());
                    compact_as_before(&mut before);
                }
                proptest::prop_assert_eq!(&ctx, &before);
                for r in 1..4 {
                    proptest::prop_assert_eq!(ctx.max_for(ReplicaId(r)), max_for_as_before(&before, ReplicaId(r)));
                }
            }
        }
    }

    #[test]
    fn from_parts_round_trips_and_compacts() {
        let mut original = DotContext::new();
        original.insert(Dot::new(ReplicaId(1), 1));
        original.insert(Dot::new(ReplicaId(1), 2));
        original.insert(Dot::new(ReplicaId(2), 5)); // 1 to 4 are missing, so this goes to the cloud

        let rebuilt = DotContext::from_parts(
            original.version_vector().clone(),
            original.cloud().collect::<Vec<_>>(),
        );
        assert_eq!(rebuilt, original);
    }
}
