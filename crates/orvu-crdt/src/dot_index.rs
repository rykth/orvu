//! Which key each dot tags, for a map or a set that has to find the keys a
//! change can touch without going through all of them.
//!
//! A change that removes a key arrives as "these dots are cancelled" and does
//! not name the key. Without this, the only way to find the key is to look at
//! every key the object has, for every change, and that makes each change
//! cost more the more the object holds.

use crate::context::{keep_dots, DotContext};
use crate::id::Dot;
use crate::version_vector::VersionVector;
use std::collections::{BTreeMap, BTreeSet};

/// For each dot a map or a set holds, the key it tags.
///
/// This is worked out from the object's own dots and holds nothing they do
/// not. It is not part of what two objects are compared by, and it is never
/// sent anywhere.
#[derive(Clone, Debug)]
pub(crate) struct DotIndex<K> {
    /// The key a dot tags.
    first: BTreeMap<Dot, K>,
    /// Further keys tagged by a dot that is already in `first`. A dot is made
    /// for one update to one key, so nothing this code writes ever puts an
    /// entry here. A change that arrives from elsewhere could carry the same
    /// dot under two keys, or under a great many, and it has to be handled
    /// the way it always was and cost no more for each.
    more: BTreeMap<Dot, BTreeSet<K>>,
}

impl<K> Default for DotIndex<K> {
    fn default() -> Self {
        Self { first: BTreeMap::new(), more: BTreeMap::new() }
    }
}

impl<K: Ord + Clone> DotIndex<K> {
    /// Notes that `d` tags `k`.
    pub(crate) fn insert(&mut self, d: Dot, k: &K) {
        match self.first.get(&d) {
            None => {
                self.first.insert(d, k.clone());
            }
            Some(have) if have == k => {}
            Some(_) => {
                self.more.entry(d).or_default().insert(k.clone());
            }
        }
    }

    /// Notes that `d` no longer tags `k`.
    pub(crate) fn remove(&mut self, d: Dot, k: &K) {
        if self.first.get(&d) == Some(k) {
            self.first.remove(&d);
            // Another key tagged by the same dot takes its place.
            if let Some(others) = self.more.get_mut(&d) {
                if let Some(next) = others.pop_first() {
                    self.first.insert(d, next);
                }
            }
        } else if let Some(others) = self.more.get_mut(&d) {
            others.remove(k);
        }
        if self.more.get(&d).is_some_and(|others| others.is_empty()) {
            self.more.remove(&d);
        }
    }

    /// The keys that hold at least one dot `ctx` has seen. These are the only
    /// keys a change with that context can take a dot away from.
    pub(crate) fn tagged_by(&self, ctx: &DotContext) -> BTreeSet<K> {
        #[cfg(test)]
        ASKED.with(|n| n.set(n.get() + 1));
        let mut keys = BTreeSet::new();
        // A context is a run of counters for each replica, plus single dots.
        // Dots sort by replica and then by counter, so each run is one range.
        for (replica, up_to) in ctx.version_vector().iter() {
            let run = Dot::new(replica, 0)..=Dot::new(replica, up_to);
            keys.extend(self.first.range(run.clone()).map(|(_, k)| k.clone()));
            keys.extend(self.more.range(run).flat_map(|(_, others)| others.iter().cloned()));
        }
        for d in ctx.cloud() {
            keys.extend(self.first.get(&d).cloned());
            keys.extend(self.more.get(&d).into_iter().flatten().cloned());
        }
        keys
    }

    /// Brings the index in line with a key whose dots went from `before` to
    /// `after`.
    pub(crate) fn changed(&mut self, k: &K, before: &BTreeSet<Dot>, after: &BTreeSet<Dot>) {
        for &d in before.difference(after) {
            self.remove(d, k);
        }
        for &d in after.difference(before) {
            self.insert(d, k);
        }
    }

    /// Every pair of dot and key, sorted. For checking the index against the
    /// dots it was worked out from.
    #[cfg(test)]
    pub(crate) fn pairs(&self) -> Vec<(Dot, K)> {
        let mut all: Vec<(Dot, K)> = self.first.iter().map(|(d, k)| (*d, k.clone())).collect();
        all.extend(self.more.iter().flat_map(|(d, others)| others.iter().map(|k| (*d, k.clone()))));
        all.sort();
        all
    }
}

/// Adds to a change's context every dot it carries that the context does
/// not list. A change has seen the dots it carries, whether or not its
/// context says so, and counting them as seen keeps a later write here from
/// being given the same dot. They are taken in all at once, as taking them
/// one by one would go through the context again for each.
pub(crate) fn count_own_dots(ctx: &mut DotContext, dots: impl IntoIterator<Item = Dot>) {
    let unlisted: Vec<Dot> = dots.into_iter().filter(|&d| !ctx.contains(d)).collect();
    if !unlisted.is_empty() {
        ctx.merge(&DotContext::from_parts(VersionVector::new(), unlisted));
    }
}

/// How a merge finds the keys it has to look at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Find {
    /// Ask the index which keys the change can touch.
    ByIndex,
    /// Go through every key there is.
    All,
}

/// Merges which keys are present in a map or a set, with the add-wins rule
/// `keep_dots` describes, and keeps the index in line. `dots` and `ctx` are
/// ours, `their_dots` and `their_ctx` came with the change.
///
/// Only two kinds of key can change: one the change tags, and one that holds
/// a dot the change's context has seen, which is how a remove arrives. Every
/// other key keeps its dots whatever the change says, so it is not looked
/// at. A change to one key then costs the same among a hundred keys as among
/// a hundred thousand.
///
/// This does not merge the contexts. The caller does that afterwards: the
/// work here needs our context as it was before the change, or the change's
/// new dots would look already seen and be dropped.
///
/// Returns the keys it looked at. Every key whose dots changed is among them,
/// so a caller that keeps more beside each key's dots can bring just those
/// keys up to date, without working out the same keys again.
pub(crate) fn merge_presence<K: Ord + Clone>(
    dots: &mut BTreeMap<K, BTreeSet<Dot>>,
    index: &mut DotIndex<K>,
    ctx: &DotContext,
    their_dots: &BTreeMap<K, BTreeSet<Dot>>,
    their_ctx: &DotContext,
) -> BTreeSet<K> {
    // A change that tags half of our keys or more is a whole copy, or close
    // to one. Asking the index would name nearly every key anyway, and
    // asking costs more than going through them all.
    let find = if their_dots.len() * 2 >= dots.len() { Find::All } else { Find::ByIndex };
    merge_presence_by(find, dots, index, ctx, their_dots, their_ctx)
}

/// Does the work of `merge_presence`, finding the keys the way it is told
/// to. Both ways give the same result.
pub(crate) fn merge_presence_by<K: Ord + Clone>(
    find: Find,
    dots: &mut BTreeMap<K, BTreeSet<Dot>>,
    index: &mut DotIndex<K>,
    ctx: &DotContext,
    their_dots: &BTreeMap<K, BTreeSet<Dot>>,
    their_ctx: &DotContext,
) -> BTreeSet<K> {
    let mut touched = match find {
        Find::ByIndex => index.tagged_by(their_ctx),
        Find::All => dots.keys().cloned().collect(),
    };
    touched.extend(their_dots.keys().cloned());
    #[cfg(test)]
    LOOKED_AT.with(|n| n.set(touched.len()));

    let empty = BTreeSet::new();
    for k in &touched {
        let mine = dots.get(k).unwrap_or(&empty);
        let theirs = their_dots.get(k).unwrap_or(&empty);

        let kept = keep_dots(mine, theirs, ctx, their_ctx);
        index.changed(k, mine, &kept);

        if kept.is_empty() {
            dots.remove(k);
        } else {
            dots.insert(k.clone(), kept);
        }
    }
    touched
}

#[cfg(test)]
thread_local! {
    static LOOKED_AT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many keys the last merge on this thread looked at.
#[cfg(test)]
pub(crate) fn looked_at() -> usize {
    LOOKED_AT.with(|n| n.get())
}

#[cfg(test)]
thread_local! {
    static ASKED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times an index on this thread has been asked which keys a
/// context has seen dots of, since `reset_asked`.
#[cfg(test)]
pub(crate) fn asked() -> usize {
    ASKED.with(|n| n.get())
}

#[cfg(test)]
pub(crate) fn reset_asked() {
    ASKED.with(|n| n.set(0));
}

/// The merge as it was before the index: it goes through every key and
/// keeps no index. Kept to check the one above against.
#[cfg(test)]
pub(crate) fn merge_presence_as_before<K: Ord + Clone>(
    dots: &mut BTreeMap<K, BTreeSet<Dot>>,
    ctx: &DotContext,
    their_dots: &BTreeMap<K, BTreeSet<Dot>>,
    their_ctx: &DotContext,
) {
    let mut keys: BTreeSet<K> = dots.keys().cloned().collect();
    keys.extend(their_dots.keys().cloned());
    for k in keys {
        let empty = BTreeSet::new();
        let mine = dots.get(&k).unwrap_or(&empty);
        let theirs = their_dots.get(&k).unwrap_or(&empty);
        let kept = keep_dots(mine, theirs, ctx, their_ctx);
        if kept.is_empty() {
            dots.remove(&k);
        } else {
            dots.insert(k, kept);
        }
    }
}

/// Checks one merge of which keys are present. `dots`, `index` and `ctx` are
/// as they were before it, and `after` and `after_index` are what it left.
/// The result has to be what the old merge gives, whichever way the keys
/// are found, and the index has to say what the dots say.
#[cfg(test)]
pub(crate) fn check_presence<K: Ord + Clone + std::fmt::Debug>(
    dots: &BTreeMap<K, BTreeSet<Dot>>,
    index: &DotIndex<K>,
    ctx: &DotContext,
    their_dots: &BTreeMap<K, BTreeSet<Dot>>,
    their_ctx: &DotContext,
    after: &BTreeMap<K, BTreeSet<Dot>>,
    after_index: &DotIndex<K>,
) {
    let mut as_before = dots.clone();
    merge_presence_as_before(&mut as_before, ctx, their_dots, their_ctx);
    assert_eq!(after, &as_before, "the two merges kept different dots");

    let mut from_dots: Vec<(Dot, K)> =
        after.iter().flat_map(|(k, held)| held.iter().map(|d| (*d, k.clone()))).collect();
    from_dots.sort();
    assert_eq!(after_index.pairs(), from_dots, "the index and the dots disagree");

    for find in [Find::ByIndex, Find::All] {
        let (mut dots, mut index) = (dots.clone(), index.clone());
        merge_presence_by(find, &mut dots, &mut index, ctx, their_dots, their_ctx);
        assert_eq!(dots, as_before, "{find:?} kept different dots");
        assert_eq!(index.pairs(), from_dots, "{find:?} left the index and the dots disagreeing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ReplicaId;

    fn dot(replica: u64, counter: u64) -> Dot {
        Dot::new(ReplicaId(replica), counter)
    }

    #[test]
    fn finds_the_keys_a_context_has_seen_dots_of() {
        let mut index = DotIndex::default();
        index.insert(dot(1, 1), &"a");
        index.insert(dot(1, 2), &"b");
        index.insert(dot(1, 5), &"c");
        index.insert(dot(2, 1), &"d");

        let mut seen = DotContext::new();
        seen.insert(dot(1, 1));
        seen.insert(dot(1, 2)); // replica 1, up to 2, as a run
        seen.insert(dot(2, 7)); // a single dot nothing here holds
        assert_eq!(index.tagged_by(&seen), BTreeSet::from(["a", "b"]));
        seen.insert(dot(1, 5)); // a single dot, ahead of the run
        assert_eq!(index.tagged_by(&seen), BTreeSet::from(["a", "b", "c"]));
        assert!(index.tagged_by(&DotContext::new()).is_empty());
    }

    #[test]
    fn a_dot_that_tags_two_keys_is_kept_for_both() {
        let mut index = DotIndex::default();
        index.insert(dot(1, 1), &"a");
        index.insert(dot(1, 1), &"b");
        index.insert(dot(1, 1), &"b"); // twice does nothing more
        let mut seen = DotContext::new();
        seen.insert(dot(1, 1));
        assert_eq!(index.tagged_by(&seen), BTreeSet::from(["a", "b"]));
        assert_eq!(index.pairs(), vec![(dot(1, 1), "a"), (dot(1, 1), "b")]);

        index.remove(dot(1, 1), &"a");
        assert_eq!(index.tagged_by(&seen), BTreeSet::from(["b"]));
        index.remove(dot(1, 1), &"b");
        assert!(index.pairs().is_empty());
    }

    // Nothing this code writes puts one dot under two keys, but a change from
    // another machine can put it under any number. That must cost no more
    // for each key than an ordinary dot does. With a plain list of the extra
    // keys this test would run for hours, and every later change to the
    // same object would be slow for good.
    #[test]
    fn one_dot_under_very_many_keys_costs_no_more_for_each() {
        const KEYS: u32 = 100_000;
        let mut index = DotIndex::default();
        for k in 0..KEYS {
            index.insert(dot(1, 1), &k);
        }
        index.insert(dot(1, 2), &KEYS);
        let mut seen = DotContext::new();
        seen.insert(dot(1, 1));
        assert_eq!(index.tagged_by(&seen).len(), KEYS as usize);
        // A context that has seen none of it finds nothing, and does not go
        // through the many keys to learn that.
        let mut elsewhere = DotContext::new();
        elsewhere.insert(dot(2, 7));
        for _ in 0..10_000 {
            assert!(index.tagged_by(&elsewhere).is_empty());
        }
        for k in 0..KEYS {
            index.remove(dot(1, 1), &k);
        }
        assert_eq!(index.pairs(), vec![(dot(1, 2), KEYS)]);
    }

    #[test]
    fn removing_what_is_not_there_changes_nothing() {
        let mut index = DotIndex::default();
        index.insert(dot(1, 1), &"a");
        index.remove(dot(1, 1), &"b");
        index.remove(dot(9, 9), &"a");
        assert_eq!(index.pairs(), vec![(dot(1, 1), "a")]);
    }
}
