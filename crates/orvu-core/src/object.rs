//! One wrapper type that can hold any of the data types.
//!
//! The engine keeps objects of different data types in a single map, so it
//! needs one type that can be any of them. `TypedCrdt` is that type for the
//! data and `TypedDelta` is the same for a change. The two are kept in step:
//! merging a change of the wrong type returns an error.

use crate::error::{Error, Result};
use orvu_crdt::{
    BoundedCounter, BoundedDelta, DeltaCrdt, JsonCrdt, JsonDelta, LwwDelta, LwwRegister, MvMap,
    MvMapDelta, OrMap, OrMapDelta, OrSet, OrSetDelta, PnCounter, PnDelta, Rga, RgaDelta, RichText, RichTextDelta,
    Tree, TreeDelta,
};
use orvu_crdt::{Dot, DotContext, Hlc, ReplicaId};
use orvu_proto::v1::CrdtType;

#[derive(Clone, Debug, PartialEq)]
pub enum TypedCrdt {
    PnCounter(PnCounter),
    LwwRegister(LwwRegister<String>),
    OrSet(OrSet<String>),
    OrMap(OrMap<String, LwwRegister<String>>),
    OrMapCounter(OrMap<String, PnCounter>),
    Rga(Rga<String>),
    Json(JsonCrdt),
    RichText(RichText),
    BoundedCounter(BoundedCounter),
    Tree(Tree),
    MvMap(MvMap<String, String>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedDelta {
    PnCounter(PnDelta),
    LwwRegister(LwwDelta<String>),
    OrSet(OrSetDelta<String>),
    OrMap(OrMapDelta<String, LwwRegister<String>>),
    OrMapCounter(OrMapDelta<String, PnCounter>),
    Rga(RgaDelta<String>),
    Json(JsonDelta),
    RichText(RichTextDelta),
    BoundedCounter(BoundedDelta),
    Tree(TreeDelta),
    MvMap(MvMapDelta<String, String>),
}

impl TypedCrdt {
    /// An empty value of the data type the message names.
    pub fn empty(kind: CrdtType) -> Result<Self> {
        match kind {
            CrdtType::PnCounter => Ok(Self::PnCounter(PnCounter::default())),
            CrdtType::LwwRegister => Ok(Self::LwwRegister(LwwRegister::default())),
            CrdtType::OrSet => Ok(Self::OrSet(OrSet::default())),
            CrdtType::OrMap => Ok(Self::OrMap(OrMap::default())),
            CrdtType::OrMapCounter => Ok(Self::OrMapCounter(OrMap::default())),
            CrdtType::RgaList => Ok(Self::Rga(Rga::default())),
            CrdtType::Json => Ok(Self::Json(JsonCrdt::default())),
            CrdtType::RichText => Ok(Self::RichText(RichText::default())),
            CrdtType::BoundedCounter => Ok(Self::BoundedCounter(BoundedCounter::default())),
            CrdtType::Tree => Ok(Self::Tree(Tree::default())),
            CrdtType::MvMap => Ok(Self::MvMap(MvMap::default())),
            CrdtType::Unspecified => Err(Error::UnknownCrdtType(0)),
        }
    }

    pub fn crdt_type(&self) -> CrdtType {
        match self {
            Self::PnCounter(_) => CrdtType::PnCounter,
            Self::LwwRegister(_) => CrdtType::LwwRegister,
            Self::OrSet(_) => CrdtType::OrSet,
            Self::OrMap(_) => CrdtType::OrMap,
            Self::OrMapCounter(_) => CrdtType::OrMapCounter,
            Self::Rga(_) => CrdtType::RgaList,
            Self::Json(_) => CrdtType::Json,
            Self::RichText(_) => CrdtType::RichText,
            Self::BoundedCounter(_) => CrdtType::BoundedCounter,
            Self::Tree(_) => CrdtType::Tree,
            Self::MvMap(_) => CrdtType::MvMap,
        }
    }

    /// Merges a delta of the same type. A delta of a different type is an
    /// error.
    pub fn merge(&mut self, delta: TypedDelta) -> Result<()> {
        match (self, delta) {
            (Self::PnCounter(c), TypedDelta::PnCounter(d)) => c.merge(d),
            (Self::LwwRegister(r), TypedDelta::LwwRegister(d)) => r.merge(d),
            (Self::OrSet(s), TypedDelta::OrSet(d)) => s.merge(d),
            (Self::OrMap(m), TypedDelta::OrMap(d)) => m.merge(d),
            (Self::OrMapCounter(m), TypedDelta::OrMapCounter(d)) => m.merge(d),
            (Self::Rga(r), TypedDelta::Rga(d)) => r.merge(d),
            (Self::Json(j), TypedDelta::Json(d)) => j.merge(d),
            (Self::RichText(t), TypedDelta::RichText(d)) => t.merge(d),
            (Self::BoundedCounter(b), TypedDelta::BoundedCounter(d)) => b.merge(d),
            (Self::Tree(t), TypedDelta::Tree(d)) => t.merge(d),
            (Self::MvMap(m), TypedDelta::MvMap(d)) => m.merge(d),
            (object, delta) => {
                return Err(Error::TypeMismatch {
                    delta: delta.crdt_type(),
                    object: object.crdt_type(),
                })
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> TypedDelta {
        match self {
            Self::PnCounter(c) => TypedDelta::PnCounter(c.snapshot()),
            Self::LwwRegister(r) => TypedDelta::LwwRegister(r.snapshot()),
            Self::OrSet(s) => TypedDelta::OrSet(s.snapshot()),
            Self::OrMap(m) => TypedDelta::OrMap(m.snapshot()),
            Self::OrMapCounter(m) => TypedDelta::OrMapCounter(m.snapshot()),
            Self::Rga(r) => TypedDelta::Rga(r.snapshot()),
            Self::Json(j) => TypedDelta::Json(j.snapshot()),
            Self::RichText(t) => TypedDelta::RichText(t.snapshot()),
            Self::BoundedCounter(b) => TypedDelta::BoundedCounter(b.snapshot()),
            Self::Tree(t) => TypedDelta::Tree(t.snapshot()),
            Self::MvMap(m) => TypedDelta::MvMap(m.snapshot()),
        }
    }
}

impl TypedDelta {
    /// The latest timestamp anywhere in this change or state, or `None` if it
    /// holds none. Registers, map values, tree moves, labels and positions,
    /// rich-text marks and every part of a JSON document carry timestamps.
    /// Counters, sets and plain lists do not.
    pub fn latest_stamp(&self) -> Option<Hlc> {
        self.latest_stamp_by().map(|(h, _)| h)
    }

    /// The latest timestamp, as `latest_stamp`, with the machine that wrote it.
    pub fn latest_stamp_by(&self) -> Option<(Hlc, ReplicaId)> {
        let none = (Hlc::ZERO, ReplicaId(0));
        let latest = match self {
            Self::LwwRegister(d) => (d.ts(), d.writer()),
            Self::OrMap(d) => registers_stamp(d),
            Self::Json(d) => json_stamp(d),
            Self::RichText(d) => d.marks().map_or(none, registers_stamp),
            Self::Tree(d) => {
                let moves = d.wire_parts().map(|(h, r, ..)| (h, r)).max().unwrap_or(none);
                let labels = d.labels().map_or(none, registers_stamp);
                let positions = d.positions().map_or(none, registers_stamp);
                moves.max(labels).max(positions)
            }
            Self::PnCounter(_)
            | Self::OrSet(_)
            | Self::OrMapCounter(_)
            | Self::Rga(_)
            | Self::BoundedCounter(_)
            | Self::MvMap(_) => none,
        };
        (latest.0 != Hlc::ZERO).then_some(latest)
    }

    pub fn crdt_type(&self) -> CrdtType {
        match self {
            Self::PnCounter(_) => CrdtType::PnCounter,
            Self::LwwRegister(_) => CrdtType::LwwRegister,
            Self::OrSet(_) => CrdtType::OrSet,
            Self::OrMap(_) => CrdtType::OrMap,
            Self::OrMapCounter(_) => CrdtType::OrMapCounter,
            Self::Rga(_) => CrdtType::RgaList,
            Self::Json(_) => CrdtType::Json,
            Self::RichText(_) => CrdtType::RichText,
            Self::BoundedCounter(_) => CrdtType::BoundedCounter,
            Self::Tree(_) => CrdtType::Tree,
            Self::MvMap(_) => CrdtType::MvMap,
        }
    }
}

fn registers_stamp(d: &OrMapDelta<String, LwwRegister<String>>) -> (Hlc, ReplicaId) {
    d.values().map(|(_, v)| (v.ts(), v.writer())).max().unwrap_or((Hlc::ZERO, ReplicaId(0)))
}

// A JSON node keeps its array elements as whole nodes, not changes, so each
// one is turned into a change to look inside it.
fn json_stamp(d: &JsonDelta) -> (Hlc, ReplicaId) {
    let mut latest = (Hlc::ZERO, ReplicaId(0));
    if let Some(t) = d.tag() {
        latest = latest.max((t.ts(), t.writer()));
        // The generations a tag names carry timestamps too, and they can be
        // newer than the tag's own.
        if let Some(tag) = t.value() {
            latest = latest
                .max((tag.generation.ts, tag.generation.writer))
                .max((tag.parent.ts, tag.parent.writer));
        }
    }
    if let Some(s) = d.scalar() {
        latest = latest.max((s.ts(), s.writer()));
    }
    if let Some(o) = d.object() {
        for (_, v) in o.values() {
            latest = latest.max(json_stamp(v));
        }
    }
    if let Some(a) = d.array() {
        for (_, e) in a.elements() {
            latest = latest.max(json_stamp(&e.value.snapshot()));
        }
    }
    latest
}

/// The most machines whose changes one object takes in from other machines.
///
/// Every change to an object carries one number for each machine that has
/// written to it. Without a limit, a sender could make up any number of
/// machine names, and every later change to the object would carry them all.
/// A change or snapshot from another machine that would bring an object a
/// writer it does not have yet and take it past this is refused. So is a
/// change made here by a machine that would be one writer too many
/// (`WriteError::TooManyWriters`), because the others would refuse it. A
/// machine already writing to the object may always write: its change adds
/// no writer that a machine which has seen its earlier changes does not
/// know, even on an object already past the limit.
///
/// The check counts the writers this machine has seen, so near or past the
/// limit an object can stop syncing. A new machine cannot start writing to
/// it. A machine that has not yet seen one of its writers refuses that
/// writer's changes, and stays cut off from them. Two machines can each let
/// in a different new writer at the same time, and then each refuses the
/// other's. The limit is only there to bound what a hostile sender can do,
/// and real teams stay far below it. A team of more than this many machines
/// all writing to one object is not supported.
///
/// A machine allowed to write can fill an object's places with made-up
/// machine names, and then no new machine can write to that object on any
/// machine those changes reached. Like other harm a writer can do on
/// purpose, that is stopped only by not letting it write.
pub const MAX_WRITERS_PER_OBJECT: usize = 1024;

/// How many machines' changes an object that has seen `seen` would hold once
/// it has also seen `more`, and the change by `author` if one is given.
fn writers_with(seen: &DotContext, more: &DotContext, author: Option<ReplicaId>) -> usize {
    let writers = |c: &DotContext| -> std::collections::BTreeSet<ReplicaId> {
        c.version_vector().iter().map(|(r, _)| r).chain(c.cloud().map(|d| d.replica)).collect()
    };
    let mut all = writers(seen);
    all.extend(writers(more));
    all.extend(author);
    all.len()
}

/// Whether taking in `more`, and the change by `author` if one is given,
/// would bring an object that has seen `seen` a writer it does not have yet
/// and leave it with more than `MAX_WRITERS_PER_OBJECT`. This is the one
/// rule used everywhere, for changes made here and for changes and
/// snapshots from other machines: something that adds no new writer is
/// always let in, even to an object already past the limit, so that a
/// machine already writing to it stays in step with the others.
pub(crate) fn adds_writer_past_limit(seen: &DotContext, more: &DotContext, author: Option<ReplicaId>) -> bool {
    let after = writers_with(seen, more, author);
    after > writers_with(seen, &DotContext::new(), None) && after > MAX_WRITERS_PER_OBJECT
}


/// One change, in the form the engine works with. It has the same fields as
/// the message `orvu_proto::v1::Delta`.
#[derive(Clone, Debug, PartialEq)]
pub struct WireDelta {
    pub object_id: String,
    pub dot: Dot,
    pub hlc: Hlc,
    /// What the author had seen before making this change.
    pub deps: DotContext,
    pub delta: TypedDelta,
    /// The id tracing tools use to follow this change. Changes made here
    /// start with `TraceId::none()`. Whoever sends the change on can set it.
    pub trace_id: crate::trace::TraceId,
}

impl WireDelta {
    /// True if this delta's dot follows on from its own dependencies.
    ///
    /// A machine numbers its changes to an object 1, 2, 3 and so on, and each
    /// change depends on everything that machine had seen, its own earlier
    /// changes included. So a real delta is either number 1 or depends on the
    /// number just before it. A delta that skips ahead was not made that way.
    /// Accepting one would let another machine use up this machine's numbers.
    pub fn follows_its_deps(&self) -> bool {
        match self.dot.counter.checked_sub(1) {
            None => false,
            Some(0) => true,
            Some(previous) => self.deps.contains(Dot::new(self.dot.replica, previous)),
        }
    }
}

/// What came of a local change that is allowed to say no. `Refused` means the
/// closure returned `None` because the change was not allowed. In that case no
/// dot was used up and there is nothing to send.
// One variant is much larger than the others. That is fine here: the value is
// returned once and taken apart straight away, and is never kept around.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum MutateOutcome {
    NoObject,
    Refused,
    /// The object turned the write away before the change was made: it has
    /// no fresh dot left for this machine, or no room for one more writer.
    /// Nothing changed.
    Unwritable(orvu_crdt::WriteError),
    Applied(WireDelta),
}

/// The whole current state of one object, for saving to disk. The state is
/// held as one delta containing everything (see `TypedCrdt::snapshot()`),
/// together with the record of which updates it includes.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotEntry {
    pub object_id: String,
    pub state: TypedDelta,
    pub context: DotContext,
}

/// One shared object: its data, plus the record of which updates it has seen
/// (its context).
///
/// The engine keeps that record itself, in the same way for every data type.
/// So putting updates in a sensible order works the same for a counter as for
/// a set.
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    id: String,
    ctx: DotContext,
    crdt: TypedCrdt,
}

impl Object {
    pub fn new(id: impl Into<String>, crdt: TypedCrdt) -> Self {
        Self {
            id: id.into(),
            ctx: DotContext::new(),
            crdt,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn crdt(&self) -> &TypedCrdt {
        &self.crdt
    }

    pub fn context(&self) -> &DotContext {
        &self.ctx
    }

    /// Rebuilds an object from a snapshot. It starts with an empty value of
    /// the right type, merges the saved state in, and takes the saved context
    /// as its own.
    pub fn from_state(id: impl Into<String>, state: TypedDelta, context: DotContext) -> Result<Self> {
        let mut crdt = TypedCrdt::empty(state.crdt_type())?;
        crdt.merge(state)?;
        Ok(Self { id: id.into(), ctx: context, crdt })
    }

    /// True if this update has already been applied.
    pub fn has_dot(&self, dot: Dot) -> bool {
        self.ctx.contains(dot)
    }

    /// True if this object has seen everything in `deps`, which is what a
    /// delta from another machine needs before it can be applied.
    pub fn is_ready(&self, deps: &DotContext) -> bool {
        self.ctx.covers(deps)
    }

    /// Makes a change here and gives it a new dot.
    ///
    /// `f` does the actual change to the data and returns the delta for it.
    /// The returned `WireDelta` records what this object had seen before the
    /// change. A receiver must have seen all of that before it applies the
    /// change.
    ///
    /// If this object has no fresh dot left for `replica` (see
    /// `orvu_crdt::MAX_COUNTER`), or `replica` has not written to it and it
    /// already has changes from `MAX_WRITERS_PER_OBJECT` machines, this
    /// refuses with `Error::Write` before `f` is called, and nothing changes.
    pub fn mutate<F>(&mut self, replica: ReplicaId, hlc: Hlc, f: F) -> Result<WireDelta>
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> TypedDelta,
    {
        let dot = self.fresh_dot(replica)?;
        let deps = self.ctx.clone();
        let delta = f(&mut self.crdt, hlc, replica);
        self.ctx.insert(dot);
        Ok(WireDelta {
            object_id: self.id.clone(),
            dot,
            hlc,
            deps,
            delta,
            trace_id: crate::trace::TraceId::none(),
        })
    }

    /// Like `mutate`, but `f` may say no by returning `None`. Then no dot is
    /// used up and no `WireDelta` comes back. When `f` returns `None` it must
    /// not have changed the data. The bounded counter keeps to this, because
    /// it checks whether a change is allowed before it makes it.
    ///
    /// If the object turns the write away as `mutate` would, this returns
    /// that error, before `f` is called.
    pub fn try_mutate<F>(&mut self, replica: ReplicaId, hlc: Hlc, f: F) -> Result<Option<WireDelta>>
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> Option<TypedDelta>,
    {
        let dot = self.fresh_dot(replica)?;
        let deps = self.ctx.clone();
        // `None` means refused, and the context is left alone.
        let Some(delta) = f(&mut self.crdt, hlc, replica) else { return Ok(None) };
        self.ctx.insert(dot);
        Ok(Some(WireDelta {
            object_id: self.id.clone(),
            dot,
            hlc,
            deps,
            delta,
            trace_id: crate::trace::TraceId::none(),
        }))
    }

    /// The dot for this machine's next change. A machine that has not written
    /// to the object yet is turned away once the object has changes from
    /// `MAX_WRITERS_PER_OBJECT` machines: everyone else would refuse its
    /// change, and taking it here would leave this machine's own record past
    /// the limit, so that it refused every later change to the object.
    ///
    /// A machine already among the object's writers may always write, even
    /// to an object saved past the limit under older rules: its change adds
    /// no new writer.
    fn fresh_dot(&self, replica: ReplicaId) -> std::result::Result<Dot, orvu_crdt::WriteError> {
        if adds_writer_past_limit(&self.ctx, &DotContext::new(), Some(replica)) {
            return Err(orvu_crdt::WriteError::TooManyWriters);
        }
        self.ctx.fresh_dot(replica)
    }

    /// Applies a delta from another machine. The caller must have checked
    /// with `is_ready` first.
    ///
    /// A dot that was already applied is skipped, so a delta that arrives
    /// twice costs nothing. Merging it again would not change the data either.
    pub fn apply_remote(&mut self, dot: Dot, delta: TypedDelta) -> Result<()> {
        if self.ctx.contains(dot) {
            return Ok(());
        }
        self.crdt.merge(delta)?;
        self.ctx.insert(dot);
        Ok(())
    }

    /// Merges a whole-state snapshot, and the context that came with it, into
    /// this object.
    ///
    /// `Object::from_state` starts again from nothing, which is what recovery
    /// after a restart needs. This one adds to what is here: the existing
    /// state, and any changes made here that the snapshot has not seen, are
    /// kept. There is no `is_ready` check, because a snapshot holds everything
    /// it depends on.
    pub fn merge_snapshot(&mut self, state: TypedDelta, context: DotContext) -> Result<()> {
        self.crdt.merge(state)?;
        self.ctx.merge(&context);
        Ok(())
    }
}

/// Combines a set of snapshot entries with a batch of deltas and returns new
/// snapshot entries.
///
/// Each object is rebuilt from its snapshot, the deltas are applied to it, and
/// its whole state is written out again. Recovery after a restart works the
/// same way. The order of the deltas does not matter. Anything that cannot be
/// used is skipped with a warning: a snapshot entry that cannot be rebuilt, a
/// delta of a type that cannot be built, or a delta its object refuses.
pub fn fold_into_snapshot(
    snapshot: Vec<SnapshotEntry>,
    deltas: Vec<WireDelta>,
) -> Vec<SnapshotEntry> {
    let mut objects: std::collections::BTreeMap<String, Object> = std::collections::BTreeMap::new();
    for e in snapshot {
        match Object::from_state(&e.object_id, e.state, e.context) {
            Ok(o) => {
                objects.insert(e.object_id, o);
            }
            Err(err) => {
                tracing::warn!(object_id = %e.object_id, error = %err, "fold_into_snapshot: skipping a snapshot entry that cannot be rebuilt");
            }
        }
    }
    for d in deltas {
        if !objects.contains_key(&d.object_id) {
            match TypedCrdt::empty(d.delta.crdt_type()) {
                Ok(empty) => {
                    objects.insert(d.object_id.clone(), Object::new(&d.object_id, empty));
                }
                Err(e) => {
                    tracing::warn!(object_id = %d.object_id, error = %e, "fold_into_snapshot: skipping delta of unbuildable CRDT type");
                    continue;
                }
            }
        }
        if let Some(obj) = objects.get_mut(&d.object_id) {
            if let Err(e) = obj.apply_remote(d.dot, d.delta) {
                tracing::warn!(object_id = %d.object_id, error = %e, "fold_into_snapshot: skipping delta the object rejected");
            }
        }
    }
    objects
        .into_iter()
        .map(|(id, o)| SnapshotEntry {
            object_id: id,
            state: o.crdt().snapshot(),
            context: o.context().clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_crdt::ReplicaId;

    fn at(wall: u64) -> Hlc {
        Hlc { wall, logical: 0 }
    }

    // Every type that holds timestamps gives up its latest one, wherever it
    // sits: in a register, a map's values, a tree's moves, labels or
    // positions, rich-text marks, or deep inside a JSON document.
    #[test]
    fn latest_stamp_finds_the_newest_timestamp_in_every_type() {
        use orvu_crdt::{JsonValue, Seg};
        let r = ReplicaId(1);

        let mut reg = LwwRegister::default();
        reg.set("a".to_string(), at(5), r);
        assert_eq!(TypedCrdt::LwwRegister(reg).snapshot().latest_stamp(), Some(at(5)));

        let mut map: OrMap<String, LwwRegister<String>> = OrMap::default();
        map.update("k".into(), r, |v| v.set("a".into(), at(3), r)).unwrap();
        map.update("j".into(), r, |v| v.set("b".into(), at(8), r)).unwrap();
        assert_eq!(TypedCrdt::OrMap(map).snapshot().latest_stamp(), Some(at(8)));

        let mut text = RichText::default();
        text.insert_str(0, "hi", r).unwrap();
        text.format(0, 2, "bold", "true", at(6), r).unwrap();
        assert_eq!(TypedCrdt::RichText(text).snapshot().latest_stamp(), Some(at(6)));

        let mut tree = Tree::default();
        tree.move_node("a".into(), "root".into(), at(2), r);
        assert_eq!(TypedCrdt::Tree(tree.clone()).snapshot().latest_stamp(), Some(at(2)));
        tree.set_label("a".into(), "A".into(), at(4), r).unwrap();
        assert_eq!(TypedCrdt::Tree(tree.clone()).snapshot().latest_stamp(), Some(at(4)));
        tree.move_node_at("b".into(), "root".into(), 0, at(7), r).unwrap();
        assert_eq!(TypedCrdt::Tree(tree).snapshot().latest_stamp(), Some(at(7)));

        let mut doc = JsonCrdt::default();
        doc.set(&[], &JsonValue::Object(Default::default()), at(1), r).unwrap();
        doc.set(&[Seg::Key("list".into())], &JsonValue::Array(vec![]), at(2), r).unwrap();
        doc.insert(&[Seg::Key("list".into())], 0, &JsonValue::Num(1.0), at(9), r).unwrap();
        doc.set(&[Seg::Key("n".into())], &JsonValue::Num(2.0), at(4), r).unwrap();
        assert_eq!(TypedCrdt::Json(doc).snapshot().latest_stamp(), Some(at(9)));

        // Written through the API, a parent's shape tag is always as new as
        // anything under it, but a change from another machine need not be
        // so tidy. A newer stamp kept only inside an array element, or only
        // in a tree position, is still found.
        let element = orvu_crdt::Element { predecessor: None, value: JsonCrdt::from_value(&JsonValue::Num(1.0), at(9), r) };
        let array = RgaDelta::from_parts([(Dot::new(r, 1), element)].into(), Default::default(), DotContext::new());
        let tag = LwwDelta::from_parts(Some(orvu_crdt::JsonTag { shape: orvu_crdt::Shape::Array, ..Default::default() }), at(2), r);
        let json = TypedDelta::Json(JsonDelta::from_parts(Some(tag), None, Some(array), None));
        assert_eq!(json.latest_stamp(), Some(at(9)));
        // A tag's generations carry timestamps of their own, and a tag from
        // another machine may name a generation newer than the tag itself.
        // Both the node's generation and its parent's are read.
        let generation = |wall| orvu_crdt::Generation { ts: at(wall), writer: r, step: 1 };
        let tag = orvu_crdt::JsonTag { shape: orvu_crdt::Shape::Object, generation: generation(12), parent: generation(3) };
        let json = TypedDelta::Json(JsonDelta::from_parts(Some(LwwDelta::from_parts(Some(tag), at(2), r)), None, None, None));
        assert_eq!(json.latest_stamp(), Some(at(12)));
        let tag = orvu_crdt::JsonTag { shape: orvu_crdt::Shape::Object, generation: generation(3), parent: generation(11) };
        let json = TypedDelta::Json(JsonDelta::from_parts(Some(LwwDelta::from_parts(Some(tag), at(2), r)), None, None, None));
        assert_eq!(json.latest_stamp(), Some(at(11)));
        let position = LwwDelta::from_parts(Some("a0".to_string()), at(5), r);
        let positions = OrMapDelta::from_parts(Default::default(), [("n".to_string(), position)].into(), DotContext::new());
        let tree = TypedDelta::Tree(TreeDelta::from_wire([(at(1), r, "n".into(), "root".into())]).with_positions(Some(positions)));
        assert_eq!(tree.latest_stamp(), Some(at(5)));

        // Types without timestamps have none.
        let mut pn = PnCounter::default();
        pn.increment(r, 3);
        assert_eq!(TypedCrdt::PnCounter(pn).snapshot().latest_stamp(), None);
        assert_eq!(TypedCrdt::empty(CrdtType::LwwRegister).unwrap().snapshot().latest_stamp(), None);
    }

    // When the newest stamp in a JSON change sits in a generation its tag
    // names, the machine named with it is the one that wrote that
    // generation, which need not be the one that wrote the tag.
    #[test]
    fn latest_stamp_by_names_the_writer_of_a_newer_generation() {
        let tag_writer = ReplicaId(1);
        let generation = |wall, writer| orvu_crdt::Generation { ts: at(wall), writer, step: 1 };
        let json = |tag| TypedDelta::Json(JsonDelta::from_parts(Some(LwwDelta::from_parts(Some(tag), at(2), tag_writer)), None, None, None));

        let tag = orvu_crdt::JsonTag {
            shape: orvu_crdt::Shape::Object,
            generation: generation(12, ReplicaId(9)),
            parent: generation(3, ReplicaId(8)),
        };
        assert_eq!(json(tag).latest_stamp_by(), Some((at(12), ReplicaId(9))));

        let tag = orvu_crdt::JsonTag {
            shape: orvu_crdt::Shape::Object,
            generation: generation(3, ReplicaId(9)),
            parent: generation(11, ReplicaId(8)),
        };
        assert_eq!(json(tag).latest_stamp_by(), Some((at(11), ReplicaId(8))));
    }

    #[test]
    fn merge_applies_matching_delta() {
        let mut source = PnCounter::default();
        let delta = source.increment(ReplicaId(1), 7);

        let mut object = TypedCrdt::empty(CrdtType::PnCounter).unwrap();
        object.merge(TypedDelta::PnCounter(delta)).unwrap();

        match object {
            TypedCrdt::PnCounter(c) => assert_eq!(c.value(), 7),
            other => panic!("expected PnCounter, got {other:?}"),
        }
    }

    #[test]
    fn merge_rejects_mismatched_delta() {
        let mut source = PnCounter::default();
        let delta = source.increment(ReplicaId(1), 1);

        let mut object = TypedCrdt::empty(CrdtType::OrSet).unwrap();
        let err = object.merge(TypedDelta::PnCounter(delta)).unwrap_err();

        assert_eq!(
            err,
            Error::TypeMismatch {
                delta: CrdtType::PnCounter,
                object: CrdtType::OrSet,
            }
        );
    }

    #[test]
    fn empty_rejects_the_unspecified_type() {
        assert_eq!(
            TypedCrdt::empty(CrdtType::Unspecified).unwrap_err(),
            Error::UnknownCrdtType(0)
        );
    }

    use orvu_crdt::OrSet;

    fn counter_object() -> Object {
        Object::new("c", TypedCrdt::empty(CrdtType::PnCounter).unwrap())
    }

    #[test]
    fn mutate_mints_a_dot_and_records_prior_deps() {
        let mut o = counter_object();
        let first = o.mutate(ReplicaId(1), Hlc::ZERO, |c, _hlc, _replica| match c {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 3)),
            other => panic!("unexpected {other:?}"),
        }).unwrap();

        assert_eq!(first.dot, Dot::new(ReplicaId(1), 1));
        assert_eq!(first.deps, DotContext::new(), "first change depends on nothing");
        assert!(o.has_dot(first.dot));

        let second = o.mutate(ReplicaId(1), Hlc::ZERO, |c, _hlc, _replica| match c {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 1)),
            other => panic!("unexpected {other:?}"),
        }).unwrap();
        assert_eq!(second.dot, Dot::new(ReplicaId(1), 2));
        assert!(
            second.deps.contains(first.dot),
            "second change depends on the first"
        );
    }

    #[test]
    fn mutate_can_build_an_lww_delta_from_the_passed_clock() {
        use orvu_crdt::LwwRegister;
        let mut o = Object::new("r", TypedCrdt::LwwRegister(LwwRegister::default()));
        o.mutate(ReplicaId(1), Hlc { wall: 5, logical: 0 }, |crdt, hlc, replica| match crdt {
            TypedCrdt::LwwRegister(r) => TypedDelta::LwwRegister(r.set("hi".to_string(), hlc, replica)),
            other => panic!("unexpected {other:?}"),
        }).unwrap();
        match o.crdt() {
            TypedCrdt::LwwRegister(r) => assert_eq!(r.get().map(String::as_str), Some("hi")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn try_mutate_refusal_leaves_object_untouched() {
        use orvu_crdt::BoundedCounter;
        let mut o = Object::new("b", TypedCrdt::BoundedCounter(BoundedCounter::default()));
        // A withdrawal with no allowance is refused. It uses up no dot and
        // there is nothing to send.
        let before = o.clone();
        let out = o.try_mutate(ReplicaId(1), Hlc::ZERO, |c, _h, r| match c {
            TypedCrdt::BoundedCounter(b) => b.try_decrement(1, r).map(TypedDelta::BoundedCounter),
            _ => None,
        });
        assert_eq!(out, Ok(None), "refused");
        assert_eq!(o, before, "object byte-identical after refusal");
        // A deposit goes through and behaves just like `mutate`.
        let out = o.try_mutate(ReplicaId(1), Hlc::ZERO, |c, _h, r| match c {
            TypedCrdt::BoundedCounter(b) => Some(TypedDelta::BoundedCounter(b.increment(5, r))),
            _ => None,
        });
        assert!(o.has_dot(out.unwrap().expect("applied").dot));
    }

    // A snapshot from a broken or hostile sender can say this machine has
    // used its last dot. Its next change here is refused, before the change
    // runs, rather than given a dot already taken.
    #[test]
    fn a_change_with_no_fresh_dot_is_refused() {
        let mut o = counter_object();
        let mut vv = orvu_crdt::VersionVector::new();
        vv.observe(ReplicaId(1), orvu_crdt::MAX_COUNTER);
        o.merge_snapshot(TypedCrdt::PnCounter(Default::default()).snapshot(), DotContext::from_parts(vv, [])).unwrap();
        let before = o.clone();
        let refused = o.mutate(ReplicaId(1), Hlc::ZERO, |_, _, _| panic!("the change must not run"));
        assert_eq!(refused.unwrap_err(), Error::Write(orvu_crdt::WriteError::NoFreshDot));
        let tried = o.try_mutate(ReplicaId(1), Hlc::ZERO, |_, _, _| panic!("the change must not run"));
        assert_eq!(tried, Err(Error::Write(orvu_crdt::WriteError::NoFreshDot)));
        assert_eq!(o, before);
        // Another machine's change goes through.
        assert!(o.mutate(ReplicaId(2), Hlc::ZERO, |c, _, r| match c {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, 1)),
            other => panic!("unexpected {other:?}"),
        }).is_ok());
    }

    #[test]
    fn readiness_tracks_the_local_context() {
        let mut author = counter_object();
        let d1 = author.mutate(ReplicaId(1), Hlc::ZERO, |c, _hlc, _replica| match c {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 1)),
            other => panic!("unexpected {other:?}"),
        }).unwrap();
        let d2 = author.mutate(ReplicaId(1), Hlc::ZERO, |c, _hlc, _replica| match c {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 1)),
            other => panic!("unexpected {other:?}"),
        }).unwrap();

        let mut peer = counter_object();
        assert!(peer.is_ready(&d1.deps), "first delta has no dependencies");
        assert!(!peer.is_ready(&d2.deps), "second delta needs the first");

        peer.apply_remote(d1.dot, d1.delta).unwrap();
        assert!(peer.is_ready(&d2.deps), "dependency now satisfied");
    }

    #[test]
    fn apply_remote_is_idempotent() {
        let mut author = Object::new("s", TypedCrdt::OrSet(OrSet::default()));
        let d = author.mutate(ReplicaId(1), Hlc::ZERO, |c, _hlc, _replica| match c {
            TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.add("x".to_string(), ReplicaId(1)).unwrap()),
            other => panic!("unexpected {other:?}"),
        }).unwrap();

        let mut peer = Object::new("s", TypedCrdt::OrSet(OrSet::default()));
        peer.apply_remote(d.dot, d.delta.clone()).unwrap();
        let once = peer.clone();
        peer.apply_remote(d.dot, d.delta).unwrap();
        assert_eq!(peer, once, "re-applying an observed dot changes nothing");
    }

    #[test]
    fn apply_remote_rejects_a_type_mismatch() {
        let mut o = counter_object();
        let mut s = OrSet::<String>::default();
        let bad = TypedDelta::OrSet(s.add("x".to_string(), ReplicaId(9)).unwrap());
        let err = o.apply_remote(Dot::new(ReplicaId(9), 1), bad).unwrap_err();
        assert!(matches!(err, Error::TypeMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn or_map_mutate_and_merge_round_trip() {
        use orvu_crdt::OrMap;
        let mut o = Object::new("m", TypedCrdt::OrMap(OrMap::default()));
        let wd = o.mutate(ReplicaId(1), Hlc::ZERO, |c, hlc, r| match c {
            TypedCrdt::OrMap(m) => {
                TypedDelta::OrMap(m.update("k".to_string(), r, |reg| reg.set("v".to_string(), hlc, r)).unwrap())
            }
            other => panic!("unexpected {other:?}"),
        }).unwrap();

        // A second object applies the delta and ends up the same.
        let mut o2 = Object::new("m", TypedCrdt::OrMap(OrMap::default()));
        o2.apply_remote(wd.dot, wd.delta.clone()).unwrap();
        match o2.crdt() {
            TypedCrdt::OrMap(m) => assert_eq!(
                m.get(&"k".to_string()).and_then(|reg| reg.get().cloned()),
                Some("v".to_string())
            ),
            other => panic!("unexpected {other:?}"),
        }

        // A delta of the wrong type is refused.
        let mut o3 = Object::new("m", TypedCrdt::OrMap(OrMap::default()));
        let mut c = PnCounter::default();
        let bad = TypedDelta::PnCounter(c.increment(ReplicaId(9), 1));
        let err = o3.apply_remote(Dot::new(ReplicaId(9), 1), bad).unwrap_err();
        assert!(matches!(err, Error::TypeMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn or_map_counter_mutate_and_merge_round_trip() {
        use orvu_crdt::OrMap;
        let mut o = Object::new("cart", TypedCrdt::OrMapCounter(OrMap::default()));
        let wd = o.mutate(ReplicaId(1), Hlc::ZERO, |c, _hlc, r| match c {
            TypedCrdt::OrMapCounter(m) => {
                TypedDelta::OrMapCounter(m.update("apple".to_string(), r, |pn| pn.increment(r, 5)).unwrap())
            }
            other => panic!("unexpected {other:?}"),
        }).unwrap();

        // A second object applies the delta and ends up the same.
        let mut o2 = Object::new("cart", TypedCrdt::OrMapCounter(OrMap::default()));
        o2.apply_remote(wd.dot, wd.delta.clone()).unwrap();
        match o2.crdt() {
            TypedCrdt::OrMapCounter(m) => {
                assert_eq!(m.get(&"apple".to_string()).map(|pn| pn.value()), Some(5))
            }
            other => panic!("unexpected {other:?}"),
        }

        // A delta of the wrong type is refused.
        let mut o3 = Object::new("cart", TypedCrdt::OrMapCounter(OrMap::default()));
        let mut c = PnCounter::default();
        let bad = TypedDelta::PnCounter(c.increment(ReplicaId(9), 1));
        let err = o3.apply_remote(Dot::new(ReplicaId(9), 1), bad).unwrap_err();
        assert!(matches!(err, Error::TypeMismatch { .. }), "got {err:?}");
    }

    #[test]
    fn merge_snapshot_preserves_a_concurrent_local_write() {
        use orvu_crdt::{Dot, DotContext, PnCounter, ReplicaId};

        // Replica B has made its own increment of 3, with dot (2,1).
        let mut local = Object::new("c", TypedCrdt::PnCounter(PnCounter::default()));
        local.mutate(ReplicaId(2), Hlc::ZERO, |crdt, _h, _r| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(2), 3)),
            other => panic!("unexpected {other:?}"),
        }).unwrap();

        // A snapshot from A holds A's increment of 5, dot (1,1), and A's context.
        let mut a = PnCounter::default();
        a.increment(ReplicaId(1), 5);
        let mut a_ctx = DotContext::new();
        a_ctx.insert(Dot::new(ReplicaId(1), 1));

        local.merge_snapshot(TypedDelta::PnCounter(a.snapshot()), a_ctx).unwrap();

        match local.crdt() {
            TypedCrdt::PnCounter(pn) => assert_eq!(pn.value(), 8, "local 3 + snapshot 5, nothing lost"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(local.has_dot(Dot::new(ReplicaId(2), 1)), "local dot survives");
        assert!(local.has_dot(Dot::new(ReplicaId(1), 1)), "snapshot dot absorbed");
    }

    #[test]
    fn fold_into_snapshot_applies_deltas_over_a_base() {
        use orvu_crdt::{Dot, DotContext, Hlc, PnCounter, ReplicaId};
        // The starting snapshot: counter "c" at 5 from replica 1, dot (1,1).
        let mut base = PnCounter::default();
        base.increment(ReplicaId(1), 5);
        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 1));
        let base_entry = SnapshotEntry {
            object_id: "c".into(),
            state: TypedCrdt::PnCounter(base).snapshot(),
            context: ctx,
        };
        // A +3 delta from replica 2. It is a different replica, so the two
        // amounts add up.
        let mut c2 = PnCounter::default();
        let inc = c2.increment(ReplicaId(2), 3);
        let d = WireDelta {
            object_id: "c".into(),
            dot: Dot::new(ReplicaId(2), 1),
            hlc: Hlc::ZERO,
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(inc),
            trace_id: crate::trace::TraceId::none(),
        };
        let out = fold_into_snapshot(vec![base_entry], vec![d]);
        assert_eq!(out.len(), 1);
        let obj = Object::from_state(&out[0].object_id, out[0].state.clone(), out[0].context.clone()).unwrap();
        match obj.crdt() {
            TypedCrdt::PnCounter(pn) => assert_eq!(pn.value(), 8, "5 from r1 + folded 3 from r2"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(out[0].context.contains(Dot::new(ReplicaId(2), 1)), "folded dot in context");
    }

    #[test]
    fn fold_into_snapshot_rebuilds_from_empty_with_orset_removal() {
        use orvu_crdt::{Dot, DotContext, Hlc, OrSet, ReplicaId};
        let mut s = OrSet::<String>::default();
        let mk = |dot, delta| WireDelta { object_id: "s".into(), dot, hlc: Hlc::ZERO, deps: DotContext::new(), delta, trace_id: crate::trace::TraceId::none() };
        let dx = mk(Dot::new(ReplicaId(1), 1), TypedDelta::OrSet(s.add("x".to_string(), ReplicaId(1)).unwrap()));
        let dy = mk(Dot::new(ReplicaId(1), 2), TypedDelta::OrSet(s.add("y".to_string(), ReplicaId(1)).unwrap()));
        let drm = mk(Dot::new(ReplicaId(1), 3), TypedDelta::OrSet(s.remove(&"x".to_string())));
        let out = fold_into_snapshot(Vec::new(), vec![dx, dy, drm]);
        assert_eq!(out.len(), 1);
        let obj = Object::from_state(&out[0].object_id, out[0].state.clone(), out[0].context.clone()).unwrap();
        match obj.crdt() {
            TypedCrdt::OrSet(set) => {
                assert!(set.contains(&"y".to_string()), "y present");
                assert!(!set.contains(&"x".to_string()), "x removed");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
