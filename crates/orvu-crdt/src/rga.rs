//! An ordered list that several machines can edit separately and then merge.
//! The algorithm is known as RGA (Replicated Growable Array).
//!
//! Every element gets a "dot" when it is inserted: the id of the machine
//! (replica) that inserted it, plus that machine's own counter. A dot never
//! changes and is never reused, so it names the element for good. An insert
//! records the dot of the element it goes after, its predecessor, or `None`
//! for the front of the list.
//!
//! A delete does not take the element out. It adds the element's dot to a set
//! of "tombstones". A tombstoned element is hidden from readers but stays in
//! place, so elements that were inserted after it still have something to
//! hang on to.
//!
//! The elements and the tombstones only ever grow. That is why `merge` gives
//! the same list whatever order deltas arrive in, however they are grouped,
//! and even if the same delta arrives twice.
//!
//! When several elements were inserted after the same predecessor, the one
//! with the higher counter comes first, and the higher machine id breaks a
//! tie. A new element's counter is always higher than the counter of every
//! element its machine has seen so far, its own or anyone else's. So a new
//! insert lands directly after its predecessor, in front of everything that
//! was already there. Only inserts made at the same spot by machines that
//! have not seen each other's yet are ordered by the tie-break.
//!
//! This is plain RGA. It does not have the extra rules that YATA (the
//! algorithm behind Yjs) uses to reduce interleaving of text typed at the same
//! spot.

use crate::context::{DotContext, MAX_COUNTER};
use crate::error::WriteError;
use crate::id::{Dot, ReplicaId};
use crate::DeltaCrdt;
use std::collections::{BTreeMap, BTreeSet};

/// One element of the list: its value, and the dot of the element it was
/// inserted after (`None` means the front of the list).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Element<T> {
    pub predecessor: Option<Dot>,
    pub value: T,
}

/// An ordered list that merges. Each element is named by its dot for as long as
/// it exists, so an insert or delete points at an element and not at an index
/// that could shift.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Rga<T> {
    elements: BTreeMap<Dot, Element<T>>,
    tombstones: BTreeSet<Dot>,
    ctx: DotContext,
}

impl<T> Default for Rga<T> {
    fn default() -> Self {
        Self { elements: BTreeMap::new(), tombstones: BTreeSet::new(), ctx: DotContext::new() }
    }
}

/// A change to send to other machines: the elements it adds, the dots it
/// tombstones, and a causal context. The list itself leaves the context empty,
/// because the elements already say which dots exist. A context that arrives
/// from elsewhere is kept and merged.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RgaDelta<T> {
    elements: BTreeMap<Dot, Element<T>>,
    tombstones: BTreeSet<Dot>,
    ctx: DotContext,
}

impl<T> RgaDelta<T> {
    /// Builds a delta from its three parts. Used when decoding one that came
    /// over the network.
    pub fn from_parts(
        elements: BTreeMap<Dot, Element<T>>,
        tombstones: BTreeSet<Dot>,
        ctx: DotContext,
    ) -> Self {
        Self { elements, tombstones, ctx }
    }

    /// The elements this delta carries, ordered by machine id and then counter.
    pub fn elements(&self) -> impl Iterator<Item = (&Dot, &Element<T>)> {
        self.elements.iter()
    }

    /// The dots this delta tombstones.
    pub fn tombstones(&self) -> impl Iterator<Item = &Dot> {
        self.tombstones.iter()
    }

    /// The causal context that comes with this delta.
    pub fn context(&self) -> &DotContext {
        &self.ctx
    }
}

impl<T> Rga<T> {
    /// Every dot that can be reached from the front of the list, tombstoned
    /// ones included, in list order. The walk starts at the front and visits
    /// each element followed by everything inserted after it, then moves on.
    /// Elements that share a predecessor are visited highest
    /// `(counter, replica)` first.
    ///
    /// An element whose predecessor has not arrived yet cannot be reached, so
    /// it is left out until the predecessor arrives.
    fn ordered_dots(&self) -> Vec<Dot> {
        let mut children: BTreeMap<Option<Dot>, Vec<Dot>> = BTreeMap::new();
        for (dot, el) in &self.elements {
            children.entry(el.predecessor).or_default().push(*dot);
        }
        // Elements with the same predecessor: higher (counter, replica) first.
        for kids in children.values_mut() {
            kids.sort_by_key(|d| std::cmp::Reverse((d.counter, d.replica.0)));
        }
        let mut out = Vec::new();
        // Walk with our own stack, so a long list cannot overflow the call
        // stack. Push in reverse so the element that sorts first is popped
        // first. Each element is followed by everything hanging off it, which
        // keeps a run typed on one machine together when another machine typed
        // at the same spot.
        let mut stack: Vec<Dot> = Vec::new();
        if let Some(roots) = children.get(&None) {
            for &d in roots.iter().rev() {
                stack.push(d);
            }
        }
        while let Some(d) = stack.pop() {
            out.push(d);
            if let Some(kids) = children.get(&Some(d)) {
                for &k in kids.iter().rev() {
                    stack.push(k);
                }
            }
        }
        out
    }

    /// The dots of the elements a reader sees, in list order. These are the
    /// ones reachable from the front and not tombstoned.
    pub fn visible_dots(&self) -> Vec<Dot> {
        self.ordered_dots()
            .into_iter()
            .filter(|d| !self.tombstones.contains(d))
            .collect()
    }

    /// How many elements are visible. Tombstoned ones do not count.
    pub fn len(&self) -> usize {
        self.visible_dots().len()
    }

    /// True when no element is visible.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The visible elements with their dots, in list order.
    pub fn visible(&self) -> Vec<(Dot, &T)> {
        self.visible_dots().into_iter().map(|d| (d, &self.elements[&d].value)).collect()
    }

    /// The visible element at `index`, if any.
    pub fn get(&self, index: usize) -> Option<&T> {
        self.visible_dots().get(index).map(|d| &self.elements[d].value)
    }
}

impl<T: Clone> Rga<T> {
    /// Inserts `value` after `predecessor`, or at the front of the list for
    /// `None`. The new element gets a fresh dot. Returns the delta that was
    /// applied.
    ///
    /// On this machine the new element lands directly after `predecessor`. Its
    /// counter is one more than the highest counter among the elements this
    /// machine holds, so it sorts in front of every one of them. If another
    /// machine inserted after the same predecessor without having seen this
    /// element, the two are ordered by counter and then by machine id.
    ///
    /// Refuses, changing nothing, with `WriteError::UnknownPredecessor` if
    /// `predecessor` is not in the list (a deleted element still is), and
    /// with `WriteError::NoFreshDot` if no counter up to `MAX_COUNTER` is
    /// left that is both unused and higher than every element here. Only a
    /// broken or hostile sender, handing this list an element with a counter
    /// at the cap, can bring the second about.
    pub fn insert_after(
        &mut self,
        predecessor: Option<Dot>,
        value: T,
        r: ReplicaId,
    ) -> Result<RgaDelta<T>, WriteError> {
        if let Some(p) = predecessor {
            if !self.elements.contains_key(&p) {
                return Err(WriteError::UnknownPredecessor(p));
            }
        }
        // One past every element held here, this machine's own included. That
        // makes the dot new and puts it in front of everything seen so far.
        let seen = self.elements.keys().map(|d| d.counter).max().unwrap_or(0);
        // A context received from another machine may know of dots from `r`
        // whose elements are not here, so stay clear of those as well.
        let own = self.ctx.fresh_dot(r)?.counter;
        let counter = own.max(seen.saturating_add(1));
        if counter > MAX_COUNTER {
            return Err(WriteError::NoFreshDot);
        }
        let dot = Dot::new(r, counter);
        // The element is its own record of the dot, so the context is left
        // alone. Recording every dot there would add an entry that never goes
        // away each time the counter skips ahead.
        let delta = RgaDelta {
            elements: BTreeMap::from([(dot, Element { predecessor, value })]),
            tombstones: BTreeSet::new(),
            ctx: DotContext::new(),
        };
        self.merge(delta.clone());
        Ok(delta)
    }

    /// How many inserts `r` can still make here before it runs out of dots.
    /// Each insert takes a counter above every element held and every dot of
    /// `r`'s the context has seen.
    pub(crate) fn room(&self, r: ReplicaId) -> u64 {
        let seen = self.elements.keys().map(|d| d.counter).max().unwrap_or(0);
        self.ctx.room(r).min(MAX_COUNTER.saturating_sub(seen))
    }

    /// Tombstones the element whose dot is `target` and returns the delta.
    /// Doing it twice changes nothing. If the element has not arrived yet the
    /// tombstone is still recorded, so the element is already hidden when its
    /// insert arrives.
    pub fn tombstone(&mut self, target: Dot) -> RgaDelta<T> {
        let delta = RgaDelta {
            elements: BTreeMap::new(),
            tombstones: BTreeSet::from([target]),
            ctx: DotContext::new(),
        };
        self.merge(delta.clone());
        delta
    }

    /// A copy of the visible values, in list order.
    pub fn to_vec(&self) -> Vec<T> {
        self.visible_dots()
            .into_iter()
            .map(|d| self.elements[&d].value.clone())
            .collect()
    }
}

impl<T: Clone> DeltaCrdt for Rga<T> {
    type Delta = RgaDelta<T>;

    fn merge(&mut self, delta: RgaDelta<T>) {
        for (dot, el) in delta.elements {
            self.elements.entry(dot).or_insert(el); // keep ours: one dot is always the same element
        }
        for dot in delta.tombstones {
            self.tombstones.insert(dot);
        }
        self.ctx.merge(&delta.ctx);
    }

    fn snapshot(&self) -> RgaDelta<T> {
        RgaDelta {
            elements: self.elements.clone(),
            tombstones: self.tombstones.clone(),
            ctx: self.ctx.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two machines each insert at the front without having seen the other's
    // insert. The dot with the higher (counter, replica) must come first.
    #[test]
    fn tie_break_orders_concurrent_inserts_at_same_anchor() {
        let mut a = Rga::<String>::default();
        let mut b = Rga::<String>::default();
        let da = a.insert_after(None, "a".to_string(), ReplicaId(1)).unwrap(); // dot (1,1)
        let db = b.insert_after(None, "b".to_string(), ReplicaId(2)).unwrap(); // dot (2,1)
        a.merge(db);
        b.merge(da);
        // The counters are equal (1) and replica 2 > replica 1, so "b" comes first.
        assert_eq!(a.to_vec(), vec!["b".to_string(), "a".to_string()]);
        assert_eq!(a.to_vec(), b.to_vec(), "both replicas agree");
    }

    #[test]
    fn front_and_after_inserts_place_correctly() {
        let mut r = Rga::<String>::default();
        r.insert_after(None, "world".to_string(), ReplicaId(1)).unwrap();
        let world = r.visible_dots()[0];
        r.insert_after(Some(world), "!".to_string(), ReplicaId(1)).unwrap(); // after world
        r.insert_after(None, "hello".to_string(), ReplicaId(1)).unwrap(); // new front
        // hello (front), then world, then ! (after world)
        assert_eq!(
            r.to_vec(),
            vec!["hello".to_string(), "world".to_string(), "!".to_string()]
        );
    }

    #[test]
    fn tombstone_hides_but_keeps_anchor() {
        let mut r = Rga::<String>::default();
        r.insert_after(None, "x".to_string(), ReplicaId(1)).unwrap();
        let x = r.visible_dots()[0];
        r.insert_after(Some(x), "y".to_string(), ReplicaId(1)).unwrap();
        r.tombstone(x); // delete "x"
        // "x" is hidden but "y" still hangs off it, so "y" stays visible.
        assert_eq!(r.to_vec(), vec!["y".to_string()]);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn tombstone_is_idempotent_and_order_free() {
        let mut r = Rga::<String>::default();
        let d_ins = r.insert_after(None, "x".to_string(), ReplicaId(1)).unwrap();
        let x = r.visible_dots()[0];
        let d_del = r.tombstone(x);
        // A new machine that gets the delete before the insert ends up the same.
        let mut fresh = Rga::<String>::default();
        fresh.merge(d_del.clone());
        fresh.merge(d_ins);
        fresh.merge(d_del); // duplicate delete
        assert_eq!(fresh.to_vec(), Vec::<String>::new());
        assert_eq!(fresh, r, "structural convergence regardless of order/dups");
    }

    #[test]
    fn delta_from_parts_round_trips_via_accessors() {
        let mut r = Rga::<String>::default();
        let d = r.insert_after(None, "v".to_string(), ReplicaId(1)).unwrap();
        let elements: BTreeMap<Dot, Element<String>> =
            d.elements().map(|(dot, el)| (*dot, el.clone())).collect();
        let tombstones: BTreeSet<Dot> = d.tombstones().copied().collect();
        let rebuilt = RgaDelta::from_parts(elements, tombstones, d.context().clone());
        assert_eq!(rebuilt, d);
    }

    #[test]
    fn get_and_len_track_visible_sequence() {
        let mut r = Rga::<String>::default();
        assert!(r.is_empty());
        r.insert_after(None, "a".to_string(), ReplicaId(1)).unwrap();
        let a = r.visible_dots()[0];
        r.insert_after(Some(a), "b".to_string(), ReplicaId(1)).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r.get(0), Some(&"a".to_string()));
        assert_eq!(r.get(1), Some(&"b".to_string()));
        assert_eq!(r.get(2), None);
    }

    // One machine writes "a b c". A second machine, which has made no inserts
    // of its own yet, receives them and inserts "X" after "a". "X" must land
    // right after "a" on both machines.
    #[test]
    fn insert_by_another_machine_lands_where_asked() {
        let mut a = Rga::<String>::default();
        let mut prev = None;
        for s in ["a", "b", "c"] {
            a.insert_after(prev, s.to_string(), ReplicaId(1)).unwrap();
            prev = a.visible_dots().last().copied();
        }
        let mut b = Rga::<String>::default();
        b.merge(a.snapshot());

        let first = b.visible_dots()[0];
        let d = b.insert_after(Some(first), "X".to_string(), ReplicaId(2)).unwrap();
        a.merge(d);

        let want: Vec<String> = ["a", "X", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(b.to_vec(), want);
        assert_eq!(a.to_vec(), want);
    }

    // The same, at the front of the list.
    #[test]
    fn insert_at_front_by_another_machine_lands_first() {
        let mut a = Rga::<String>::default();
        a.insert_after(None, "a".to_string(), ReplicaId(1)).unwrap();
        a.insert_after(None, "b".to_string(), ReplicaId(1)).unwrap();
        a.insert_after(None, "c".to_string(), ReplicaId(1)).unwrap();
        let mut b = Rga::<String>::default();
        b.merge(a.snapshot());

        let d = b.insert_after(None, "z".to_string(), ReplicaId(2)).unwrap();
        a.merge(d);

        assert_eq!(b.to_vec()[0], "z");
        assert_eq!(a.to_vec(), b.to_vec());
    }

    // A machine keeps handing out dots it has never used, even when its
    // counter jumps ahead to pass what it has seen from others.
    #[test]
    fn dots_stay_unique_when_the_counter_jumps() {
        let mut a = Rga::<String>::default();
        for s in ["a", "b", "c"] {
            a.insert_after(None, s.to_string(), ReplicaId(1)).unwrap();
        }
        let mut b = Rga::<String>::default();
        b.insert_after(None, "p".to_string(), ReplicaId(2)).unwrap();
        b.merge(a.snapshot());
        b.insert_after(None, "q".to_string(), ReplicaId(2)).unwrap();
        b.insert_after(None, "r".to_string(), ReplicaId(2)).unwrap();
        assert_eq!(b.len(), 6, "every insert got its own dot");
    }

    // A new item's counter has to be above every item held, so once an item
    // with the highest counter there is has arrived, no insert can sort in
    // front of it. The insert is refused, and nothing changes, rather than
    // put somewhere it was not asked to go or given a dot already taken.
    #[test]
    fn an_insert_once_an_item_at_the_cap_is_held_is_refused() {
        let mut r = Rga::<String>::default();
        for counter in [MAX_COUNTER, u64::MAX] {
            let top = Dot::new(ReplicaId(9), counter);
            let el = Element { predecessor: None, value: "top".to_string() };
            r.merge(RgaDelta::from_parts(BTreeMap::from([(top, el)]), BTreeSet::new(), DotContext::new()));
            let before = r.clone();
            assert_eq!(r.insert_after(None, "mine".to_string(), ReplicaId(1)), Err(WriteError::NoFreshDot));
            assert_eq!(r, before);
        }
        // One below the cap still leaves room for exactly one.
        let mut r = Rga::<String>::default();
        let el = Element { predecessor: None, value: "near".to_string() };
        r.merge(RgaDelta::from_parts(BTreeMap::from([(Dot::new(ReplicaId(9), MAX_COUNTER - 1), el)]), BTreeSet::new(), DotContext::new()));
        assert_eq!(r.room(ReplicaId(1)), 1);
        let d = r.insert_after(None, "mine".to_string(), ReplicaId(1)).unwrap();
        assert_eq!(d.elements().next().unwrap().0.counter, MAX_COUNTER);
        assert!(r.insert_after(None, "more".to_string(), ReplicaId(1)).is_err());
    }

    // An insert after an item the list does not have would be kept but never
    // shown, so it is refused. A deleted item is still there to hang on to.
    #[test]
    fn an_insert_after_an_unknown_item_is_refused() {
        let mut r = Rga::<String>::default();
        r.insert_after(None, "a".to_string(), ReplicaId(1)).unwrap();
        let a = r.visible_dots()[0];
        let nowhere = Dot::new(ReplicaId(5), 3);
        let before = r.clone();
        assert_eq!(r.insert_after(Some(nowhere), "x".to_string(), ReplicaId(1)), Err(WriteError::UnknownPredecessor(nowhere)));
        assert_eq!(r, before);
        // Deleted but present: fine.
        r.tombstone(a);
        r.insert_after(Some(a), "b".to_string(), ReplicaId(1)).unwrap();
        assert_eq!(r.to_vec(), vec!["b".to_string()]);
        // Only a tombstone, with no item: not in the list.
        r.tombstone(nowhere);
        assert_eq!(r.insert_after(Some(nowhere), "y".to_string(), ReplicaId(1)), Err(WriteError::UnknownPredecessor(nowhere)));
    }

    // The context must stay small however many inserts a machine makes after
    // receiving elements from another machine.
    #[test]
    fn context_does_not_grow_with_every_insert() {
        let mut a = Rga::<String>::default();
        for s in ["a", "b", "c", "d", "e"] {
            a.insert_after(None, s.to_string(), ReplicaId(1)).unwrap();
        }
        let mut b = Rga::<String>::default();
        b.merge(a.snapshot());
        for i in 0..200 {
            b.insert_after(None, i.to_string(), ReplicaId(2)).unwrap();
        }
        assert_eq!(b.len(), 205);
        assert_eq!(b.snapshot().context().cloud().count(), 0);
    }
}
