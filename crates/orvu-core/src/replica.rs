//! One machine's copy of the data: many objects, one clock, and one waiting
//! room for deltas that arrived too early.
//!
//! A `Replica` does no networking. It turns changes made here into deltas to
//! send, and takes in deltas from other machines. A delta that depends on
//! updates not seen yet is held back until they arrive.

use crate::buffer::CausalBuffer;
use crate::error::{Error, Result};
use crate::object::{adds_writer_past_limit, MutateOutcome, Object, TypedCrdt, TypedDelta, WireDelta, MAX_WRITERS_PER_OBJECT};
use orvu_crdt::{DotContext, Hlc, ReplicaId};
use std::collections::BTreeMap;

/// What happened to a received delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Applied straight away. This may have let waiting deltas through too.
    Applied,
    /// It depends on updates not seen yet, so it was put in the buffer.
    Buffered,
    /// Seen before, so it was ignored.
    Duplicate,
}

pub struct Replica {
    id: ReplicaId,
    clock: Hlc,
    objects: BTreeMap<String, Object>,
    buffer: CausalBuffer,
    /// How far ahead of this machine's clock the last change or snapshot
    /// taken in from another machine was, in milliseconds.
    last_lead: u64,
    /// How long, in milliseconds, a delta may wait in the buffer before it
    /// is dropped.
    waiting_limit: u64,
    /// How many waiting deltas have been dropped for waiting too long.
    waiting_dropped: u64,
}

/// How long a delta may wait for what it depends on, in milliseconds, unless
/// `with_waiting_limit` says otherwise.
pub const DEFAULT_WAITING_LIMIT_MS: u64 = 2_000;

/// How far ahead of this machine's wall clock, in milliseconds, a timestamp
/// from another machine may be. The clock never goes back, so taking in one
/// from far in the future would leave every later write here sharing that
/// one wall time, and losing to writes it should beat. Five minutes is far
/// more than clocks kept by any time service drift apart.
pub const MAX_CLOCK_LEAD_MS: u64 = 5 * 60 * 1000;

/// The latest timestamp a change carries, its own or one inside its value,
/// and the machine that wrote it.
fn stamp_of(delta: &WireDelta) -> (Hlc, ReplicaId) {
    let own = (delta.hlc, delta.dot.replica);
    delta.delta.latest_stamp_by().map_or(own, |s| s.max(own))
}

/// True if `c` has seen no change at all.
fn holds_nothing(c: &DotContext) -> bool {
    c.version_vector().iter().next().is_none() && c.cloud().next().is_none()
}

impl Replica {
    pub fn new(id: ReplicaId, buffer_capacity: usize) -> Self {
        Self {
            id,
            clock: Hlc::ZERO,
            objects: BTreeMap::new(),
            buffer: CausalBuffer::with_capacity(buffer_capacity),
            last_lead: 0,
            waiting_limit: DEFAULT_WAITING_LIMIT_MS,
            waiting_dropped: 0,
        }
    }

    /// Sets how long, in milliseconds, a delta may wait in the buffer for
    /// what it depends on.
    pub fn with_waiting_limit(mut self, ms: u64) -> Self {
        self.waiting_limit = ms;
        self
    }

    /// How many waiting deltas have been dropped so far: for waiting too
    /// long, or because by the time they were ready the object had no room
    /// left for their writer (see `MAX_WRITERS_PER_OBJECT`).
    pub fn waiting_dropped(&self) -> u64 {
        self.waiting_dropped
    }

    /// How far ahead of this machine's clock, in milliseconds, the last
    /// change or snapshot passed to `receive` or `merge_snapshot` was. 0 if
    /// it was not ahead. A caller can use this to warn about clocks that
    /// disagree.
    pub fn last_lead(&self) -> u64 {
        self.last_lead
    }

    /// How far this machine's own clock runs ahead of `now_wall`, in
    /// milliseconds. 0 if it does not. Every timestamp taken in becomes the
    /// clock, so one from a machine whose clock is fast leaves the clock
    /// ahead here too, as does a wall clock that has been set back.
    pub fn clock_lead(&self, now_wall: u64) -> u64 {
        self.clock.lead_over(now_wall)
    }

    /// Turns away a timestamp too far ahead of `now_wall`, and remembers how
    /// far ahead it was. `writer` is the machine that wrote the timestamp.
    fn check_lead(&mut self, (stamp, writer): (Hlc, ReplicaId), now_wall: u64) -> Result<()> {
        let lead = stamp.lead_over(now_wall);
        self.last_lead = lead;
        if lead > MAX_CLOCK_LEAD_MS {
            return Err(Error::TooFarAhead { lead_ms: lead, writer });
        }
        Ok(())
    }

    pub fn id(&self) -> ReplicaId {
        self.id
    }

    pub fn object(&self, id: &str) -> Option<&Object> {
        self.objects.get(id)
    }

    /// How many deltas are waiting for updates they depend on.
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Which updates have been seen for one object. Machines compare these to
    /// work out what the other is missing.
    pub fn observed_context(&self, object_id: &str) -> Option<&DotContext> {
        self.objects.get(object_id).map(Object::context)
    }

    /// Which updates have been seen for every object, by object id. This is
    /// what a `ContextDigest` message carries.
    pub fn observed_contexts(&self) -> BTreeMap<String, DotContext> {
        self.objects
            .iter()
            .map(|(id, obj)| (id.clone(), obj.context().clone()))
            .collect()
    }

    /// Creates an empty object. If one with this id already exists, it is left
    /// as it is.
    pub fn create(&mut self, object_id: &str, crdt: TypedCrdt) {
        self.objects
            .entry(object_id.to_string())
            .or_insert_with(|| Object::new(object_id, crdt));
    }

    /// Puts an object in place from a snapshot, replacing any object already
    /// there. Recovery after a restart does this first, and then replays the
    /// deltas made after the snapshot.
    ///
    /// A context with gaps, which only a hostile snapshot could have brought
    /// in, loses them here. Dropping a number only means the change it names
    /// would be taken in again if it came back, which changes nothing.
    ///
    /// The clock is moved past every timestamp in the state, so a change made
    /// here afterwards is newer than anything loaded, even if the wall clock
    /// has been set back since the state was saved.
    pub fn install(&mut self, object_id: &str, state: TypedDelta, context: DotContext) -> Result<()> {
        let context = DotContext::from_parts(context.version_vector().clone(), []);
        let stamp = state.latest_stamp();
        let object = Object::from_state(object_id, state, context)?;
        self.objects.insert(object_id.to_string(), object);
        if let Some(stamp) = stamp {
            self.clock.update(0, stamp);
        }
        Ok(())
    }

    /// Changes an object here and returns the delta to send to others.
    ///
    /// Returns `None` if the object does not exist, or if it turns the write
    /// away (see `write`). Then nothing changes.
    pub fn mutate<F>(&mut self, object_id: &str, now_wall: u64, f: F) -> Option<WireDelta>
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> TypedDelta,
    {
        match self.write(object_id, now_wall, f) {
            MutateOutcome::Applied(wd) => Some(wd),
            _ => None,
        }
    }

    /// Like `mutate`, but says why nothing was done: `NoObject` if there is
    /// no such object, and `Unwritable` if the object has no fresh dot left
    /// for this machine, or this machine has not written to it and it already
    /// has changes from `MAX_WRITERS_PER_OBJECT` machines. It is never
    /// `Refused`.
    pub fn write<F>(&mut self, object_id: &str, now_wall: u64, f: F) -> MutateOutcome
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> TypedDelta,
    {
        self.try_mutate(object_id, now_wall, |crdt, hlc, replica| Some(f(crdt, hlc, replica)))
    }

    /// A local change that is allowed to say no. The result is `Refused` if
    /// the closure returns `None`, `Unwritable` if the object turns the write
    /// away before the closure runs (see `write`), `NoObject` if there is no
    /// such object, and `Applied(delta)` otherwise.
    pub fn try_mutate<F>(&mut self, object_id: &str, now_wall: u64, f: F) -> MutateOutcome
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> Option<TypedDelta>,
    {
        let hlc = self.clock.tick(now_wall);
        let id = self.id;
        match self.objects.get_mut(object_id) {
            None => MutateOutcome::NoObject,
            Some(o) => match o.try_mutate(id, hlc, f) {
                Ok(None) => MutateOutcome::Refused,
                Ok(Some(wd)) => MutateOutcome::Applied(wd),
                Err(Error::Write(e)) => MutateOutcome::Unwritable(e),
                // `Object::try_mutate` refuses only with `Error::Write`.
                Err(_) => MutateOutcome::Refused,
            },
        }
    }

    /// Takes in a delta from another machine. It is applied now, or kept in
    /// the buffer until the updates it depends on arrive.
    ///
    /// Returns what happened to `delta`, plus every waiting delta this call
    /// let through, so the caller can record those and pass them on too.
    ///
    /// A delta whose dot does not follow on from what it says it depends on is
    /// refused with `MalformedPayload` (see `WireDelta::follows_its_deps`).
    ///
    /// A delta carrying a timestamp more than `MAX_CLOCK_LEAD_MS` ahead of
    /// `now_wall`, in its own stamp or inside its value, is refused with
    /// `TooFarAhead`, and changes nothing here. Otherwise the clock is moved
    /// past that timestamp.
    ///
    /// A delta that would bring its object a writer it does not have yet,
    /// counting the ones the delta depends on, and leave it with changes from
    /// more than `MAX_WRITERS_PER_OBJECT` machines, is refused with
    /// `MalformedPayload`. One that adds no new writer is taken in even if
    /// the object is already past the limit.
    ///
    /// A delta that has to wait is dropped once it has waited longer than the
    /// waiting limit (see `with_waiting_limit`), measured in the `now_wall`
    /// values passed here and to `merge_snapshot`. `waiting_dropped` counts
    /// these. The object a delta is for is created only once a delta for it
    /// is applied, never by one that waits or is refused.
    pub fn receive(&mut self, delta: WireDelta, now_wall: u64) -> Result<(Applied, Vec<WireDelta>)> {
        let stamp = stamp_of(&delta);
        self.check_lead(stamp, now_wall)?;
        self.take_in(delta, now_wall, stamp.0, true)
    }

    /// Takes in a delta this machine saved before it stopped, while starting
    /// up again. It is applied like one from `receive`, but however far ahead
    /// of today's clock its timestamp is: the clock may have been set back
    /// since it was saved. The clock is moved past it.
    pub fn replay(&mut self, delta: WireDelta) -> Result<(Applied, Vec<WireDelta>)> {
        let stamp = stamp_of(&delta);
        // What was saved here was taken in already, so the limit on writers
        // is not checked again: a change this machine made to a full object
        // would otherwise be lost.
        self.take_in(delta, 0, stamp.0, false)
    }

    fn take_in(
        &mut self,
        delta: WireDelta,
        now_wall: u64,
        stamp: Hlc,
        limit_writers: bool,
    ) -> Result<(Applied, Vec<WireDelta>)> {
        if !delta.follows_its_deps() {
            return Err(Error::MalformedPayload(format!(
                "dot {:?} does not follow on from the delta's dependencies",
                delta.dot
            )));
        }
        let no_context = DotContext::new();
        let seen = self.objects.get(&delta.object_id).map_or(&no_context, Object::context);
        // Once applied, the object will have seen everything the delta
        // depends on, and the delta itself. Checked now, so that one that
        // would go past the limit does not wait in the buffer either.
        if limit_writers && adds_writer_past_limit(seen, &delta.deps, Some(delta.dot.replica)) {
            return Err(Error::MalformedPayload(format!(
                "object {} would have changes from more than {MAX_WRITERS_PER_OBJECT} machines",
                delta.object_id
            )));
        }
        self.clock.update(now_wall, stamp);
        self.expire_waiting(now_wall);

        let ready = match self.objects.get(&delta.object_id) {
            Some(object) if object.has_dot(delta.dot) => return Ok((Applied::Duplicate, Vec::new())),
            Some(object) => object.is_ready(&delta.deps),
            None => holds_nothing(&delta.deps),
        };

        if !ready {
            // A copy that is already waiting does not take a second place,
            // and does not start its wait again.
            if !self.buffer.contains(&delta.object_id, delta.dot) {
                self.buffer.push(delta, now_wall)?;
            }
            return Ok((Applied::Buffered, Vec::new()));
        }

        match self.objects.get_mut(&delta.object_id) {
            Some(object) => object.apply_remote(delta.dot, delta.delta)?,
            None => {
                // The first change to an object this machine does not have
                // yet. The object is only kept once the change is in it.
                let mut object = Object::new(delta.object_id.clone(), TypedCrdt::empty(delta.delta.crdt_type())?);
                object.apply_remote(delta.dot, delta.delta)?;
                self.objects.insert(delta.object_id, object);
            }
        }
        let drained = self.drain_ready();
        Ok((Applied::Applied, drained))
    }

    /// Drops the deltas that have waited in the buffer longer than the limit.
    /// The regular catch-up between machines brings them in again later.
    fn expire_waiting(&mut self, now_wall: u64) {
        let dropped = self.buffer.expire(now_wall, self.waiting_limit);
        self.waiting_dropped += dropped as u64;
    }

    /// Merges a whole-state snapshot for one object, creating the object if
    /// it is not here yet. A snapshot that holds no change creates nothing.
    ///
    /// The snapshot may bring in updates that waiting deltas depend on.
    /// Returns every waiting delta the merge let through, so the caller can
    /// record those and pass them on.
    ///
    /// A snapshot whose context has gaps, numbers seen with earlier ones from
    /// the same machine missing, is refused with `MalformedPayload`. Changes
    /// only ever arrive in order, so a machine's own record never has gaps.
    /// Taking such a record in would make every change made here afterwards
    /// carry all those numbers to everyone else.
    ///
    /// So is one that would bring the object a writer it does not have yet
    /// and leave it with changes from more than `MAX_WRITERS_PER_OBJECT`
    /// machines. One that adds no new writer is taken in even if the object
    /// is already past the limit.
    ///
    /// A snapshot may hold changes this machine made but no longer has, for
    /// example after it was restored from an old backup. They are taken in
    /// like any others, and this machine's next change is numbered after
    /// them. That only helps a change made after the snapshot arrived: one
    /// made before it reuses a number the others have seen, and they ignore
    /// it as a repeat. A machine cannot tell that it was restored, so one
    /// that was must be given a new machine number before it writes.
    ///
    /// A snapshot holding a timestamp more than `MAX_CLOCK_LEAD_MS` ahead of
    /// `now_wall` is refused with `TooFarAhead`, as a delta would be.
    /// Otherwise the clock is moved past its latest timestamp, so a change
    /// made here afterwards wins over what it brought.
    pub fn merge_snapshot(
        &mut self,
        object_id: &str,
        state: TypedDelta,
        context: DotContext,
        now_wall: u64,
    ) -> Result<Vec<WireDelta>> {
        if let Some(dot) = context.cloud().next() {
            return Err(Error::MalformedPayload(format!(
                "snapshot context has seen {dot:?} without the numbers before it"
            )));
        }
        let no_context = DotContext::new();
        let seen = self.objects.get(object_id).map_or(&no_context, Object::context);
        if adds_writer_past_limit(seen, &context, None) {
            return Err(Error::MalformedPayload(format!(
                "object {object_id} would have changes from more than {MAX_WRITERS_PER_OBJECT} machines"
            )));
        }
        let stamp = state.latest_stamp_by();
        self.check_lead(stamp.unwrap_or((Hlc::ZERO, self.id)), now_wall)?;
        let stamp = stamp.map(|(h, _)| h);
        self.expire_waiting(now_wall);
        match self.objects.get_mut(object_id) {
            Some(object) => object.merge_snapshot(state, context)?,
            // A snapshot that holds no change brings nothing to keep.
            None if holds_nothing(&context) => return Ok(Vec::new()),
            None => {
                let mut object = Object::new(object_id, TypedCrdt::empty(state.crdt_type())?);
                object.merge_snapshot(state, context)?;
                self.objects.insert(object_id.to_string(), object);
            }
        }
        if let Some(stamp) = stamp {
            self.clock.update(now_wall, stamp);
        }
        Ok(self.drain_ready())
    }

    /// Lets through every waiting delta that has become ready, and returns
    /// the ones that were applied, in the order they were applied.
    ///
    /// Applying one delta can make another one ready, so this keeps going
    /// round until a pass finds nothing more to apply.
    fn drain_ready(&mut self) -> Vec<WireDelta> {
        let mut applied = Vec::new();
        loop {
            // A waiting delta may have arrived another way in the meantime,
            // for example inside a snapshot. It has nothing left to do, so it
            // gives up its place.
            {
                let Self { objects, buffer, .. } = self;
                buffer.take_ready(|d| objects.get(&d.object_id).is_some_and(|o| o.has_dot(d.dot)));
            }
            let ready = {
                let Self {
                    objects, buffer, ..
                } = self;
                buffer.take_ready(|d| {
                    objects
                        .get(&d.object_id)
                        .is_some_and(|o| !o.has_dot(d.dot) && o.is_ready(&d.deps))
                })
            };

            if ready.is_empty() {
                break;
            }

            for d in ready {
                if let Some(object) = self.objects.get_mut(&d.object_id) {
                    // There was room for its writer when it began to wait,
                    // but other deltas may have taken that room since. A
                    // writer the object already has is always let through.
                    if adds_writer_past_limit(object.context(), &DotContext::new(), Some(d.dot.replica)) {
                        self.waiting_dropped += 1;
                        tracing::warn!(object_id = %d.object_id, "dropping a waiting delta that would go past the limit on writers");
                        continue;
                    }
                    // A delta of the wrong type fails here. It is dropped with
                    // a warning, so that one bad delta cannot hold up the rest
                    // of the buffer.
                    match object.apply_remote(d.dot, d.delta.clone()) {
                        Ok(()) => applied.push(d),
                        Err(e) => {
                            tracing::warn!(object_id = %d.object_id, error = %e, "dropping a waiting delta its object refused");
                        }
                    }
                }
            }
        }
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_crdt::{OrSet, PnCounter};

    fn counter_replica(id: u64) -> Replica {
        let mut r = Replica::new(ReplicaId(id), 64);
        r.create("c", TypedCrdt::PnCounter(PnCounter::default()));
        r
    }

    fn bump(r: &mut Replica, by: u64, wall: u64) -> WireDelta {
        let id = r.id();
        r.mutate("c", wall, |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(id, by)),
            other => panic!("unexpected {other:?}"),
        })
        .expect("object exists")
    }

    fn value(r: &Replica) -> i64 {
        match r.object("c").unwrap().crdt() {
            TypedCrdt::PnCounter(pn) => pn.value(),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn register_replica(id: u64) -> Replica {
        let mut r = Replica::new(ReplicaId(id), 64);
        r.create("r", TypedCrdt::LwwRegister(orvu_crdt::LwwRegister::default()));
        r
    }

    fn write(r: &mut Replica, v: &str, wall: u64) -> WireDelta {
        let v = v.to_string();
        r.mutate("r", wall, move |crdt, hlc, rep| match crdt {
            TypedCrdt::LwwRegister(reg) => TypedDelta::LwwRegister(reg.set(v, hlc, rep)),
            other => panic!("unexpected {other:?}"),
        })
        .expect("object exists")
    }

    fn read(r: &Replica) -> Option<String> {
        match r.object("r").unwrap().crdt() {
            TypedCrdt::LwwRegister(reg) => reg.get().cloned(),
            other => panic!("unexpected {other:?}"),
        }
    }

    const MINUTE: u64 = 60_000;
    const NOW: u64 = 1_000_000_000;

    // A change stamped far in the future would drag this machine's clock
    // along with it, and every write here would share that one timestamp
    // from then on. It is refused, and leaves nothing behind.
    #[test]
    fn a_change_from_far_in_the_future_is_refused() {
        let mut liar = register_replica(1);
        let forged = write(&mut liar, "from the future", NOW + 10 * MINUTE);

        let mut victim = register_replica(2);
        let err = victim.receive(forged.clone(), NOW).unwrap_err();
        assert_eq!(err, Error::TooFarAhead { lead_ms: 10 * MINUTE, writer: ReplicaId(1) });
        assert_eq!(read(&victim), None);
        assert_eq!(victim.observed_contexts().get("r").unwrap().version_vector().get(ReplicaId(1)), 0);
        assert_eq!(victim.pending(), 0);
        // Nor does it start an object this machine did not have.
        let mut empty = Replica::new(ReplicaId(3), 64);
        assert!(empty.receive(forged, NOW).is_err());
        assert!(empty.object("r").is_none());

        // The victim's own clock was not moved, so its next write is stamped
        // with its own time.
        let mine = write(&mut victim, "mine", NOW + 1);
        assert_eq!(mine.hlc, Hlc { wall: NOW + 1, logical: 0 });

        // An honest change from the same machine still gets in.
        let mut honest = register_replica(1);
        let d = write(&mut honest, "honest", NOW + 2);
        assert_eq!(victim.receive(d, NOW + 2).unwrap().0, Applied::Applied);
        assert_eq!(read(&victim).as_deref(), Some("honest"));
    }

    // The value itself can carry a timestamp of its own. A far-future one
    // there would win every later write to the register, so it is refused
    // too, even when the change's own timestamp looks fine.
    #[test]
    fn a_far_future_timestamp_inside_the_change_is_refused() {
        let mut liar = register_replica(1);
        let mut forged = write(&mut liar, "x", NOW + 10 * MINUTE);
        forged.hlc = Hlc { wall: NOW, logical: 0 };

        let mut victim = register_replica(2);
        assert!(matches!(victim.receive(forged, NOW), Err(Error::TooFarAhead { .. })));
        assert_eq!(read(&victim), None);
    }

    // The limit is generous. A change a little ahead, from a machine whose
    // clock runs fast, is taken in, and how far ahead it was is kept so the
    // caller can warn about it.
    #[test]
    fn a_change_a_little_ahead_is_applied_and_its_lead_reported() {
        let mut fast = register_replica(1);
        let d = write(&mut fast, "fast", NOW + 2_000);

        let mut r = register_replica(2);
        assert_eq!(r.receive(d, NOW).unwrap().0, Applied::Applied);
        assert_eq!(r.last_lead(), 2_000);
        assert_eq!(read(&r).as_deref(), Some("fast"));

        let mut slow = register_replica(3);
        let d = write(&mut slow, "slow", NOW - 5);
        r.receive(d, NOW).unwrap();
        assert_eq!(r.last_lead(), 0);

        // Just inside the limit is still taken in.
        let mut edge = register_replica(4);
        let d = write(&mut edge, "edge", NOW + MAX_CLOCK_LEAD_MS);
        assert_eq!(r.receive(d, NOW).unwrap().0, Applied::Applied);
    }

    // A snapshot carries the timestamps of every value in it. One far ahead
    // is refused like a change would be. One within the limit moves the
    // clock, so a write made here afterwards wins over what it brought.
    #[test]
    fn snapshots_are_held_to_the_same_limit_and_move_the_clock() {
        let mut other = register_replica(1);
        write(&mut other, "theirs", NOW + 10 * MINUTE);
        let obj = other.object("r").unwrap();
        let (state, ctx) = (obj.crdt().snapshot(), obj.context().clone());

        let mut r = register_replica(2);
        assert!(matches!(
            r.merge_snapshot("r", state, ctx, NOW),
            Err(Error::TooFarAhead { .. })
        ));
        assert_eq!(read(&r), None);

        let mut other = register_replica(1);
        write(&mut other, "theirs", NOW + 30_000);
        let obj = other.object("r").unwrap();
        r.merge_snapshot("r", obj.crdt().snapshot(), obj.context().clone(), NOW).unwrap();
        assert_eq!(r.last_lead(), 30_000);
        write(&mut r, "mine", NOW);
        assert_eq!(read(&r).as_deref(), Some("mine"));
    }

    // A refusal names the machine that wrote the timestamp, which need not be
    // the machine that passed it on. Here machine 1 passes on a snapshot of
    // a register machine 3 wrote.
    #[test]
    fn a_refusal_names_the_machine_that_wrote_the_timestamp() {
        let mut writer = register_replica(3);
        let d = write(&mut writer, "fast", NOW + 10 * MINUTE);
        let mut carrier = register_replica(1);
        carrier.receive(d, NOW + 10 * MINUTE).unwrap();
        let obj = carrier.object("r").unwrap();

        let mut r = register_replica(2);
        let err = r.merge_snapshot("r", obj.crdt().snapshot(), obj.context().clone(), NOW).unwrap_err();
        assert_eq!(err, Error::TooFarAhead { lead_ms: 10 * MINUTE, writer: ReplicaId(3) });

        // A counter change holds no timestamp of its own inside; the change's
        // stamp is the one refused, and its writer is the one named.
        let mut fast = counter_replica(4);
        let d = bump(&mut fast, 1, NOW + 10 * MINUTE);
        let err = counter_replica(2).receive(d, NOW).unwrap_err();
        assert_eq!(err, Error::TooFarAhead { lead_ms: 10 * MINUTE, writer: ReplicaId(4) });
    }

    // Taking in a timestamp moves this machine's clock past it, so the clock
    // can run ahead of its own wall time. `clock_lead` says by how much.
    #[test]
    fn the_clock_lead_says_how_far_the_clock_runs_ahead_of_wall_time() {
        let mut r = register_replica(2);
        assert_eq!(r.clock_lead(NOW), 0);
        let mut fast = register_replica(1);
        r.receive(write(&mut fast, "fast", NOW + 3 * MINUTE), NOW).unwrap();
        assert_eq!(r.clock_lead(NOW), 3 * MINUTE);
        assert_eq!(r.clock_lead(NOW + MINUTE), 2 * MINUTE);
        assert_eq!(r.clock_lead(NOW + 4 * MINUTE), 0);
    }

    // What this machine saved before it stopped is loaded back however far
    // ahead of today's clock it is: the clock may have been set back since.
    // Loading it moves the clock past it, so a new write still wins.
    #[test]
    fn loading_saved_state_moves_the_clock_past_it() {
        let mut before = register_replica(1);
        write(&mut before, "old", NOW + 10 * MINUTE);
        let obj = before.object("r").unwrap();

        let mut after = Replica::new(ReplicaId(1), 64);
        after.install("r", obj.crdt().snapshot(), obj.context().clone()).unwrap();
        write(&mut after, "new", NOW);
        assert_eq!(read(&after).as_deref(), Some("new"));

        // The same for changes saved after the snapshot.
        let mut before = register_replica(1);
        let d = write(&mut before, "old", NOW + 10 * MINUTE);
        let mut after = register_replica(1);
        assert_eq!(after.replay(d).unwrap().0, Applied::Applied);
        write(&mut after, "new", NOW);
        assert_eq!(read(&after).as_deref(), Some("new"));
    }

    // Every machine numbers its own changes 1, 2, 3 and so on, and each one
    // depends on the one before. A delta whose number skips ahead is not
    // something a real machine sends.
    #[test]
    fn a_delta_whose_number_skips_ahead_is_refused() {
        let mut a = counter_replica(1);
        let mut victim = counter_replica(2);
        let mut forged = bump(&mut a, 1, 10);
        forged.dot = orvu_crdt::Dot::new(ReplicaId(2), u64::MAX);

        let before = victim.observed_contexts();
        assert!(matches!(victim.receive(forged, 11), Err(Error::MalformedPayload(_))));
        assert_eq!(victim.observed_contexts(), before);
        assert_eq!(victim.pending(), 0);

        // The victim can still make changes, and they still get number 1.
        let d = bump(&mut victim, 4, 12);
        assert_eq!(d.dot, orvu_crdt::Dot::new(ReplicaId(2), 1));
    }

    // Each machine's changes are numbered without gaps and each one depends
    // on the one before, so changes alone can never leave this machine
    // holding numbers with gaps between them. Deltas that skip numbers wait
    // or are refused, and the record of what this machine has seen stays
    // whole.
    #[test]
    fn deltas_that_skip_numbers_leave_no_gaps() {
        let mut a = counter_replica(1);
        let template = bump(&mut a, 1, 10);
        let mut victim = counter_replica(2);
        for n in 1..=50u64 {
            // One that says it depends on the number before, which never
            // came, has to wait; one that depends on nothing is refused.
            for says_before in [true, false] {
                let mut forged = template.clone();
                forged.dot = orvu_crdt::Dot::new(ReplicaId(9), 2 * n);
                forged.deps = DotContext::new();
                if says_before {
                    forged.deps.insert(orvu_crdt::Dot::new(ReplicaId(9), 2 * n - 1));
                }
                let _ = victim.receive(forged, 11);
            }
        }
        for ctx in victim.observed_contexts().values() {
            assert_eq!(ctx.cloud().count(), 0);
        }
    }

    // A snapshot from another machine holds that machine's whole record of
    // what it has seen, which has no gaps either. One whose record has gaps
    // was not made by a machine, and taking it in would make every later
    // change here carry those numbers, however many there are.
    #[test]
    fn a_snapshot_whose_record_has_gaps_is_refused() {
        let mut a = counter_replica(1);
        bump(&mut a, 2, 10);
        let state = a.object("c").unwrap().crdt().snapshot();
        let mut ctx = a.object("c").unwrap().context().clone();
        ctx.insert(orvu_crdt::Dot::new(ReplicaId(9), 2));

        let mut b = counter_replica(2);
        let before = b.observed_contexts();
        assert!(matches!(
            b.merge_snapshot("c", state.clone(), ctx, 100),
            Err(Error::MalformedPayload(_))
        ));
        assert_eq!(b.observed_contexts(), before);
        assert_eq!(value(&b), 0);
        // Nor does a refused snapshot of an object this machine does not have
        // leave an empty one behind.
        let mut gapped = DotContext::new();
        gapped.insert(orvu_crdt::Dot::new(ReplicaId(9), 2));
        assert!(b.merge_snapshot("new", state.clone(), gapped, 100).is_err());
        assert!(b.object("new").is_none());
        // A whole record is still taken in.
        let ctx = a.object("c").unwrap().context().clone();
        b.merge_snapshot("c", state, ctx, 100).unwrap();
        assert_eq!(value(&b), 2);
    }

    // A record with gaps could be taken in from a snapshot before such
    // snapshots were refused, and saved. Loading it again must not keep the
    // gaps: every change made here would carry them, and other machines
    // would wait for ever for the numbers missing before them.
    #[test]
    fn a_saved_record_with_gaps_loses_them_when_loaded() {
        let mut a = counter_replica(1);
        bump(&mut a, 2, 10);
        let state = a.object("c").unwrap().crdt().snapshot();
        let mut ctx = a.object("c").unwrap().context().clone();
        ctx.insert(orvu_crdt::Dot::new(ReplicaId(9), 2));

        let mut b = Replica::new(ReplicaId(2), 64);
        b.install("c", state, ctx).unwrap();
        assert_eq!(b.object("c").unwrap().context().cloud().count(), 0);
        assert_eq!(value(&b), 2);
        let d = bump(&mut b, 3, 11);

        let mut c = counter_replica(3);
        let snap = a.object("c").unwrap();
        c.merge_snapshot("c", snap.crdt().snapshot(), snap.context().clone(), 100).unwrap();
        assert_eq!(c.receive(d, 12).unwrap().0, Applied::Applied);
        let snap = b.object("c").unwrap();
        c.merge_snapshot("c", snap.crdt().snapshot(), snap.context().clone(), 100).unwrap();
        assert_eq!(value(&c), 5);
    }

    // A delta is waiting, and then what it carries arrives another way, inside
    // a snapshot. It must not stay in the buffer.
    #[test]
    fn a_waiting_delta_that_arrives_another_way_leaves_the_buffer() {
        let mut a = counter_replica(1);
        let _d1 = bump(&mut a, 2, 10);
        let d2 = bump(&mut a, 3, 11);
        let state = a.object("c").unwrap().crdt().snapshot();
        let ctx = a.object("c").unwrap().context().clone();

        let mut b = counter_replica(2);
        assert_eq!(b.receive(d2, 12).unwrap().0, Applied::Buffered);
        assert_eq!(b.pending(), 1);

        let released = b.merge_snapshot("c", state, ctx, 100).unwrap();
        assert!(released.is_empty(), "the snapshot already held d2");
        assert_eq!(b.pending(), 0);
        assert_eq!(value(&b), 5);
    }

    // The same delta sent twice while it has to wait takes one place in the
    // buffer and is reported once when it goes through.
    #[test]
    fn a_delta_sent_twice_while_waiting_is_kept_once() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 2, 10);
        let d2 = bump(&mut a, 3, 11);

        let mut b = counter_replica(2);
        assert_eq!(b.receive(d2.clone(), 12).unwrap().0, Applied::Buffered);
        assert_eq!(b.receive(d2, 13).unwrap().0, Applied::Buffered);
        assert_eq!(b.pending(), 1);

        let (_, released) = b.receive(d1, 14).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(value(&b), 5);
    }

    #[test]
    fn observed_contexts_is_empty_for_a_fresh_replica() {
        let r = Replica::new(ReplicaId(1), 64);
        assert!(r.observed_contexts().is_empty());
    }

    #[test]
    fn observed_contexts_has_an_entry_per_object_with_its_context() {
        let mut r = Replica::new(ReplicaId(1), 64);
        r.create("c", TypedCrdt::PnCounter(Default::default()));
        r.mutate("c", 1, |crdt, _h, rep| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(rep, 5)),
            _ => unreachable!(),
        });
        let ctxs = r.observed_contexts();
        assert_eq!(ctxs.len(), 1);
        // The entry's context is the same as what `observed_context` returns
        // for that id.
        assert_eq!(ctxs.get("c"), r.observed_context("c").cloned().as_ref());
    }

    #[test]
    fn in_order_delivery_applies_immediately() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 5, 10);

        let mut b = counter_replica(2);
        assert_eq!(b.receive(d1, 11).unwrap().0, Applied::Applied);
        assert_eq!(value(&b), 5);
        assert_eq!(b.pending(), 0);
    }

    #[test]
    fn out_of_order_delivery_buffers_then_releases() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 5, 10);
        let d2 = bump(&mut a, 3, 11);

        let mut b = counter_replica(2);
        assert_eq!(b.receive(d2, 12).unwrap().0, Applied::Buffered);
        assert_eq!(b.pending(), 1);
        assert_eq!(value(&b), 0, "effect is not visible before its cause");

        assert_eq!(b.receive(d1, 13).unwrap().0, Applied::Applied);
        assert_eq!(b.pending(), 0, "buffered delta was released");
        assert_eq!(value(&b), 8);
    }

    #[test]
    fn a_chain_of_buffered_deltas_cascades() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 1, 10);
        let d2 = bump(&mut a, 2, 11);
        let d3 = bump(&mut a, 4, 12);

        let mut b = counter_replica(2);
        b.receive(d3, 13).unwrap();
        b.receive(d2, 14).unwrap();
        assert_eq!(b.pending(), 2);

        assert_eq!(b.receive(d1, 15).unwrap().0, Applied::Applied);
        assert_eq!(b.pending(), 0, "both parked deltas cascaded");
        assert_eq!(value(&b), 7);
    }

    #[test]
    fn duplicate_delivery_is_reported_and_harmless() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 5, 10);

        let mut b = counter_replica(2);
        b.receive(d1.clone(), 11).unwrap();
        assert_eq!(b.receive(d1, 12).unwrap().0, Applied::Duplicate);
        assert_eq!(value(&b), 5);
    }

    #[test]
    fn buffer_overflow_is_reported() {
        let mut a = counter_replica(1);
        let _d1 = bump(&mut a, 1, 10);
        let d2 = bump(&mut a, 1, 11);
        let d3 = bump(&mut a, 1, 12);

        let mut b = Replica::new(ReplicaId(2), 1);
        b.create("c", TypedCrdt::PnCounter(PnCounter::default()));
        assert_eq!(b.receive(d2, 13).unwrap().0, Applied::Buffered);
        assert_eq!(b.receive(d3, 14).unwrap_err(), Error::CausalBufferFull(1));
    }

    #[test]
    fn an_unknown_object_is_created_on_receive() {
        let mut a = Replica::new(ReplicaId(1), 16);
        a.create("s", TypedCrdt::OrSet(OrSet::default()));
        let d = a
            .mutate("s", 10, |crdt, _hlc, _replica| match crdt {
                TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.add("x".to_string(), ReplicaId(1)).unwrap()),
                other => panic!("unexpected {other:?}"),
            })
            .unwrap();

        let mut b = Replica::new(ReplicaId(2), 16);
        assert_eq!(b.receive(d, 11).unwrap().0, Applied::Applied);
        match b.object("s").unwrap().crdt() {
            TypedCrdt::OrSet(s) => assert!(s.contains(&"x".to_string())),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn install_restores_state_and_continues_dots() {
        // Take a counter to 5 on replica A.
        let mut a = counter_replica(1);
        let d = bump(&mut a, 5, 10);
        let state = a.object("c").unwrap().crdt().snapshot();
        let ctx = a.object("c").unwrap().context().clone();

        // Install that snapshot on a new replica B.
        let mut b = Replica::new(ReplicaId(1), 64);
        b.install("c", state, ctx).unwrap();
        assert_eq!(value(&b), 5, "state restored from snapshot");

        // Receiving the same delta again changes nothing, because the context
        // already includes it.
        assert_eq!(b.receive(d, 11).unwrap().0, Applied::Duplicate);
        assert_eq!(value(&b), 5);
    }

    #[test]
    fn receive_reports_buffer_drained_deltas() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 2, 10);
        let d2 = bump(&mut a, 3, 11); // depends on d1

        let mut b = counter_replica(2);
        // Deliver d2 first. It has to wait, and nothing is let through.
        let (s2, drained2) = b.receive(d2.clone(), 12).unwrap();
        assert_eq!(s2, Applied::Buffered);
        assert!(drained2.is_empty());

        // Deliver d1. It is applied and lets d2 through, and d2 must be
        // reported back.
        let (s1, drained1) = b.receive(d1, 13).unwrap();
        assert_eq!(s1, Applied::Applied);
        assert_eq!(drained1.len(), 1);
        assert_eq!(drained1[0].dot, d2.dot);
    }

    #[test]
    fn merge_snapshot_absorbs_state_and_releases_buffered_deltas() {
        // A makes two increments, the second depending on the first. Keep A's
        // snapshot state and context after both.
        let mut a = counter_replica(1);
        let _d1 = bump(&mut a, 2, 10);
        let _d2 = bump(&mut a, 3, 11); // depends on d1
        let state = a.object("c").unwrap().crdt().snapshot();
        let ctx = a.object("c").unwrap().context().clone();

        // B gets a later delta, d3, which needs d1 and d2. It has to wait.
        let d3 = bump(&mut a, 4, 12);
        let mut b = counter_replica(2);
        assert_eq!(b.receive(d3.clone(), 20).unwrap().0, Applied::Buffered);
        assert_eq!(b.pending(), 1);

        // Merging A's snapshot, which includes d1 and d2, lets d3 through.
        let drained = b.merge_snapshot("c", state, ctx, 100).unwrap();
        assert_eq!(drained.len(), 1, "d3 was released by the snapshot merge");
        assert_eq!(drained[0].dot, d3.dot);
        assert_eq!(b.pending(), 0);
        assert_eq!(value(&b), 9, "2 + 3 + 4 all present");
    }

    #[test]
    fn merge_snapshot_is_idempotent() {
        let mut a = counter_replica(1);
        let _ = bump(&mut a, 5, 10);
        let state = a.object("c").unwrap().crdt().snapshot();
        let ctx = a.object("c").unwrap().context().clone();

        let mut b = counter_replica(2);
        b.merge_snapshot("c", state.clone(), ctx.clone(), 100).unwrap();
        let once = value(&b);
        b.merge_snapshot("c", state, ctx, 100).unwrap();
        assert_eq!(value(&b), once, "re-merging a snapshot changes nothing");
    }

    #[test]
    fn merge_snapshot_auto_creates_an_unknown_object() {
        let mut a = Replica::new(ReplicaId(1), 16);
        a.create("s", TypedCrdt::OrSet(OrSet::default()));
        a.mutate("s", 10, |crdt, _hlc, _replica| match crdt {
            TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.add("x".to_string(), ReplicaId(1)).unwrap()),
            other => panic!("unexpected {other:?}"),
        });
        let state = a.object("s").unwrap().crdt().snapshot();
        let ctx = a.object("s").unwrap().context().clone();

        let mut b = Replica::new(ReplicaId(2), 16); // never created "s"
        let drained = b.merge_snapshot("s", state, ctx, 100).unwrap();
        assert!(drained.is_empty());
        match b.object("s").unwrap().crdt() {
            TypedCrdt::OrSet(s) => assert!(s.contains(&"x".to_string())),
            other => panic!("unexpected {other:?}"),
        }
    }

    // A delta whose earlier changes never come would otherwise wait for
    // good, and once the buffer was full every out-of-order delta after it
    // would be refused. It is dropped once it has waited longer than the
    // limit, and the regular catch-up brings it back in later.
    #[test]
    fn a_waiting_delta_is_dropped_once_it_has_waited_too_long() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 1, 10);
        let d2 = bump(&mut a, 2, 11);
        let d3 = bump(&mut a, 4, 12);

        let mut b = Replica::new(ReplicaId(2), 1).with_waiting_limit(1_000);
        b.create("c", TypedCrdt::PnCounter(PnCounter::default()));
        assert_eq!(b.receive(d2.clone(), NOW).unwrap().0, Applied::Buffered);
        // Still within the limit: the buffer is full, and stays so.
        assert_eq!(b.receive(d3.clone(), NOW + 1_000).unwrap_err(), Error::CausalBufferFull(1));
        assert_eq!(b.waiting_dropped(), 0);
        // Past the limit, the old one gives up its place.
        assert_eq!(b.receive(d3.clone(), NOW + 1_001).unwrap().0, Applied::Buffered);
        assert_eq!(b.waiting_dropped(), 1);
        assert_eq!(b.pending(), 1);
        // Sending the same delta again does not make it wait any longer.
        assert_eq!(b.receive(d3.clone(), NOW + 1_500).unwrap().0, Applied::Buffered);
        assert_eq!(b.receive(d1.clone(), NOW + 2_002).unwrap().0, Applied::Applied);
        assert_eq!(b.waiting_dropped(), 2, "d3 waited from NOW + 1_001");
        assert_eq!(value(&b), 1);
        // Catch-up sends the dropped ones again, and they go in.
        b.receive(d2, NOW + 2_003).unwrap();
        b.receive(d3, NOW + 2_004).unwrap();
        assert_eq!(value(&b), 7);
    }

    // A delta that only waits, or is refused, must not leave an object behind:
    // otherwise a sender could make this machine hold any number of empty
    // objects. The object appears once a delta for it is applied.
    #[test]
    fn an_object_appears_only_when_a_change_for_it_is_applied() {
        let mut a = counter_replica(1);
        let d1 = bump(&mut a, 2, 10);
        let d2 = bump(&mut a, 3, 11);

        let mut b = Replica::new(ReplicaId(2), 64);
        for n in 0..50 {
            let mut waiting = d2.clone();
            waiting.object_id = format!("o{n}");
            assert_eq!(b.receive(waiting, NOW).unwrap().0, Applied::Buffered);
        }
        assert!(b.observed_contexts().is_empty(), "a waiting change created an object");

        let mut skipping = d2.clone();
        skipping.deps = DotContext::new();
        skipping.object_id = "skip".into();
        assert!(b.receive(skipping, NOW).is_err());
        assert!(b.object("skip").is_none());

        // An empty snapshot holds no change, so it creates nothing either.
        let empty = TypedCrdt::PnCounter(PnCounter::default()).snapshot();
        b.merge_snapshot("empty", empty, DotContext::new(), NOW).unwrap();
        assert!(b.object("empty").is_none());

        // The first change that applies brings the object, and lets the one
        // waiting for it through.
        let mut first = d1.clone();
        first.object_id = "o7".into();
        let (status, released) = b.receive(first, NOW).unwrap();
        assert_eq!(status, Applied::Applied);
        assert_eq!(released.len(), 1);
        assert_eq!(b.observed_contexts().len(), 1);
    }

    // A machine restored from an old backup has lost changes it made and
    // sent. Its peers still hold them, perhaps only inside a snapshot once
    // they have tidied up. Taking them back in moves this machine's own
    // numbering past them, so its next change gets a number nobody has seen
    // and is taken in everywhere rather than ignored as a repeat.
    #[test]
    fn a_machine_restored_from_a_backup_catches_up_on_its_own_changes() {
        let mut b = counter_replica(2);
        let backup = bump(&mut b, 1, 10);
        let second = bump(&mut b, 1, 11);
        bump(&mut b, 1, 12);
        // `a` has all three, but only as a snapshot.
        let mut a = counter_replica(1);
        let state = b.object("c").unwrap().crdt().snapshot();
        let ctx = b.object("c").unwrap().context().clone();
        a.merge_snapshot("c", state, ctx, NOW).unwrap();
        assert_eq!(value(&a), 3);

        // `b` comes back with only its first change.
        let mut b = Replica::new(ReplicaId(2), 64);
        b.replay(backup.clone()).unwrap();
        assert_eq!(value(&b), 1);

        let state = a.object("c").unwrap().crdt().snapshot();
        let ctx = a.object("c").unwrap().context().clone();
        b.merge_snapshot("c", state, ctx, NOW).unwrap();
        assert_eq!(value(&b), 3);
        let next = bump(&mut b, 1, 13);
        assert_eq!(next.dot, orvu_crdt::Dot::new(ReplicaId(2), 4));
        assert_eq!(a.receive(next, NOW).unwrap().0, Applied::Applied);
        assert_eq!(value(&a), 4);
        assert_eq!(value(&b), 4);

        // A lost change of its own sent back as a change is taken in too.
        let mut c = Replica::new(ReplicaId(2), 64);
        c.replay(backup).unwrap();
        assert_eq!(c.receive(second, NOW).unwrap().0, Applied::Applied);
        assert_eq!(bump(&mut c, 1, 13).dot, orvu_crdt::Dot::new(ReplicaId(2), 3));
    }

    // A machine restored from a backup cannot tell that it was. If it writes
    // before it has caught up, its change reuses a number another machine
    // already holds for a lost change, and that machine takes it for the one
    // it has. Under a new machine number the same change is taken in.
    #[test]
    fn a_restored_machine_must_write_under_a_new_number() {
        let mut b = counter_replica(2);
        let backup = bump(&mut b, 1, 10);
        let lost = bump(&mut b, 1, 11);
        let mut a = counter_replica(1);
        a.receive(backup.clone(), NOW).unwrap();
        a.receive(lost, NOW).unwrap();

        let mut b = Replica::new(ReplicaId(2), 64);
        b.replay(backup.clone()).unwrap();
        let reused = bump(&mut b, 5, 12);
        assert_eq!(a.receive(reused, NOW).unwrap().0, Applied::Duplicate, "the change is lost");

        let mut c = Replica::new(ReplicaId(3), 64);
        c.replay(backup).unwrap();
        let fresh = bump(&mut c, 5, 12);
        assert_eq!(a.receive(fresh, NOW).unwrap().0, Applied::Applied);
        assert_eq!(value(&a), 7);
    }

    fn first_change_from(replica: u64) -> WireDelta {
        let mut c = PnCounter::default();
        WireDelta {
            object_id: "c".into(),
            dot: orvu_crdt::Dot::new(ReplicaId(replica), 1),
            hlc: Hlc { wall: NOW, logical: 0 },
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(c.increment(ReplicaId(replica), 1)),
            trace_id: crate::trace::TraceId::none(),
        }
    }

    // Every change carries one number for each machine that has written to
    // the object. A sender could make up any number of machine names, and
    // from then on every change to the object would carry them all. So an
    // object takes in changes from at most `MAX_WRITERS_PER_OBJECT` machines.
    #[test]
    fn an_object_takes_changes_from_a_limited_number_of_machines() {
        let mut b = counter_replica(2);
        bump(&mut b, 1, 10);
        // This machine is one of the writers already.
        for r in 1000..1000 + MAX_WRITERS_PER_OBJECT as u64 - 1 {
            assert_eq!(b.receive(first_change_from(r), NOW).unwrap().0, Applied::Applied);
        }
        let full = b.observed_contexts();
        assert_eq!(full["c"].version_vector().iter().count(), MAX_WRITERS_PER_OBJECT);

        let one_more = first_change_from(5000);
        assert!(matches!(b.receive(one_more, NOW), Err(Error::MalformedPayload(_))));
        assert_eq!(b.observed_contexts(), full);

        // A change that depends on machines not seen here would go past the
        // limit once applied, so it is refused rather than kept waiting.
        let mut waits = first_change_from(1000);
        waits.dot = orvu_crdt::Dot::new(ReplicaId(1000), 2);
        waits.deps.insert(orvu_crdt::Dot::new(ReplicaId(1000), 1));
        waits.deps.insert(orvu_crdt::Dot::new(ReplicaId(6000), 1));
        assert!(matches!(b.receive(waits, NOW), Err(Error::MalformedPayload(_))));
        assert_eq!(b.pending(), 0);

        // So is a snapshot that would.
        let mut over = full["c"].clone();
        over.insert(orvu_crdt::Dot::new(ReplicaId(7000), 1));
        let state = b.object("c").unwrap().crdt().snapshot();
        assert!(matches!(b.merge_snapshot("c", state, over, NOW), Err(Error::MalformedPayload(_))));
        assert_eq!(b.observed_contexts(), full);

        // Machines already writing go on as before, this one included.
        let mut next = first_change_from(1000);
        next.dot = orvu_crdt::Dot::new(ReplicaId(1000), 2);
        next.deps = full["c"].clone();
        assert_eq!(b.receive(next, NOW).unwrap().0, Applied::Applied);
        assert_eq!(b.observed_contexts()["c"].version_vector().get(ReplicaId(1000)), 2);
        bump(&mut b, 1, 11);
        // Machine 1000's second change says its total is still 1.
        assert_eq!(value(&b), MAX_WRITERS_PER_OBJECT as i64 + 1);
    }

    // A delta can wait while the object still has room for its writer, and
    // the room is gone by the time it is ready. It is dropped then.
    #[test]
    fn a_waiting_delta_is_held_to_the_writer_limit_when_it_is_let_through() {
        let mut b = counter_replica(2);
        bump(&mut b, 1, 10);
        for r in 1000..1000 + MAX_WRITERS_PER_OBJECT as u64 - 2 {
            b.receive(first_change_from(r), NOW).unwrap();
        }
        let mut second = first_change_from(1000);
        second.dot = orvu_crdt::Dot::new(ReplicaId(1000), 2);
        second.deps = b.observed_contexts()["c"].clone();
        let mut waits = first_change_from(6000);
        waits.deps = second.deps.clone();
        waits.deps.insert(second.dot);
        assert_eq!(b.receive(waits, NOW).unwrap().0, Applied::Buffered);

        assert_eq!(b.receive(first_change_from(7000), NOW).unwrap().0, Applied::Applied);
        let (_, released) = b.receive(second, NOW).unwrap();
        assert!(released.is_empty());
        assert_eq!(b.pending(), 0);
        let ctx = &b.observed_contexts()["c"];
        assert_eq!(ctx.version_vector().iter().count(), MAX_WRITERS_PER_OBJECT);
        assert_eq!(ctx.version_vector().get(ReplicaId(6000)), 0);
        assert_eq!(b.waiting_dropped(), 1, "a dropped waiting delta is counted");
    }

    // A machine new to a full object cannot write to it: every other machine
    // that has it full would refuse the change, so taking it here would leave
    // the copies apart, and would leave this one's own record past the limit,
    // so that it refused every later change to the object. The write is
    // refused here instead, and the machine goes on hearing the others.
    #[test]
    fn a_new_machine_cannot_write_to_a_full_object() {
        let mut b = counter_replica(2);
        for r in 1000..1000 + MAX_WRITERS_PER_OBJECT as u64 {
            b.receive(first_change_from(r), NOW).unwrap();
        }
        let full = b.observed_contexts();
        let before = b.object("c").unwrap().clone();

        let refused = b.write("c", 11, |crdt, _, replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, 1)),
            other => panic!("unexpected {other:?}"),
        });
        assert!(matches!(refused, MutateOutcome::Unwritable(orvu_crdt::WriteError::TooManyWriters)), "{refused:?}");
        assert!(b.mutate("c", 11, |_, _, _| panic!("the change must not run")).is_none());
        let tried = b.try_mutate("c", 11, |_, _, _| panic!("the change must not run"));
        assert!(matches!(tried, MutateOutcome::Unwritable(orvu_crdt::WriteError::TooManyWriters)), "{tried:?}");
        assert_eq!(b.object("c").unwrap(), &before);
        assert_eq!(b.observed_contexts(), full);

        // It still takes in changes and snapshots from the machines writing.
        let mut next = first_change_from(1000);
        next.dot = orvu_crdt::Dot::new(ReplicaId(1000), 2);
        next.deps = full["c"].clone();
        assert_eq!(b.receive(next, NOW).unwrap().0, Applied::Applied);
        let mut other = counter_replica(1500);
        let state = b.object("c").unwrap().crdt().snapshot();
        other.merge_snapshot("c", state, b.observed_contexts()["c"].clone(), NOW).unwrap();
        let state = other.object("c").unwrap().crdt().snapshot();
        let ctx = other.object("c").unwrap().context().clone();
        b.merge_snapshot("c", state, ctx, NOW).unwrap();
        assert_eq!(value(&b), MAX_WRITERS_PER_OBJECT as i64);
        // A machine already writing to it writes here as before.
        let mut a = counter_replica(1000);
        let state = b.object("c").unwrap().crdt().snapshot();
        a.merge_snapshot("c", state, b.observed_contexts()["c"].clone(), NOW).unwrap();
        let d = bump(&mut a, 1, 12);
        assert_eq!(d.dot, orvu_crdt::Dot::new(ReplicaId(1000), 3));
        assert_eq!(b.receive(d, NOW).unwrap().0, Applied::Applied);
    }

    // A change this machine saved is loaded back after a restart even if its
    // object is past the limit, as one saved by an older version could be.
    #[test]
    fn a_saved_change_is_not_held_to_the_writer_limit_when_loaded() {
        let mut saved: Vec<WireDelta> = (1000..1000 + MAX_WRITERS_PER_OBJECT as u64).map(first_change_from).collect();
        let mut own = first_change_from(2);
        for d in &saved {
            own.deps.insert(d.dot);
        }
        saved.push(own);
        let mut again = Replica::new(ReplicaId(2), 64);
        for d in saved {
            assert_eq!(again.replay(d).unwrap().0, Applied::Applied);
        }
        assert_eq!(value(&again), MAX_WRITERS_PER_OBJECT as i64 + 1);
    }

    // An object saved past the limit, under older rules, still takes writes
    // from the machines already writing to it. Only a machine new to it is
    // turned away.
    #[test]
    fn a_writer_already_in_an_object_past_the_limit_can_still_write() {
        let mut saved: Vec<WireDelta> = (1000..1000 + MAX_WRITERS_PER_OBJECT as u64).map(first_change_from).collect();
        let mut own = first_change_from(2);
        for d in &saved {
            own.deps.insert(d.dot);
        }
        saved.push(own);
        let mut a = Replica::new(ReplicaId(2), 64);
        let mut stranger = Replica::new(ReplicaId(3), 64);
        for d in &saved {
            a.replay(d.clone()).unwrap();
            stranger.replay(d.clone()).unwrap();
        }
        assert_eq!(a.observed_contexts()["c"].version_vector().iter().count(), MAX_WRITERS_PER_OBJECT + 1);

        let wrote = a.write("c", NOW, |crdt, _, replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, 1)),
            other => panic!("unexpected {other:?}"),
        });
        let MutateOutcome::Applied(d) = wrote else { panic!("{wrote:?}") };
        assert_eq!(value(&a), MAX_WRITERS_PER_OBJECT as i64 + 2);

        let d2 = bump(&mut a, 1, NOW);
        assert_eq!(value(&a), MAX_WRITERS_PER_OBJECT as i64 + 3);

        // The changes reach a machine holding the same data, and are taken
        // in there: they add no writer, though their deps name all of them.
        // The second arrives first and waits for the first.
        let mut b = Replica::new(ReplicaId(4), 64);
        for d in &saved {
            b.replay(d.clone()).unwrap();
        }
        let sent = |d: &WireDelta| crate::codec::decode_wire_delta(&crate::codec::encode_wire_delta(d)).unwrap();
        assert_eq!(b.receive(sent(&d2), NOW).unwrap().0, Applied::Buffered);
        let (applied, released) = b.receive(sent(&d), NOW).unwrap();
        assert_eq!(applied, Applied::Applied);
        assert_eq!(released.len(), 1, "the waiting change is let through");
        assert_eq!(b.waiting_dropped(), 0);
        assert_eq!(value(&b), MAX_WRITERS_PER_OBJECT as i64 + 3);

        // So is a snapshot of it, which adds no writer either.
        let mut c = Replica::new(ReplicaId(5), 64);
        for d in &saved {
            c.replay(d.clone()).unwrap();
        }
        let state = a.object("c").unwrap().crdt().snapshot();
        c.merge_snapshot("c", state, a.observed_contexts()["c"].clone(), NOW).unwrap();
        assert_eq!(value(&c), MAX_WRITERS_PER_OBJECT as i64 + 3);

        let refused = stranger.write("c", NOW, |crdt, _, replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, 1)),
            other => panic!("unexpected {other:?}"),
        });
        assert!(matches!(refused, MutateOutcome::Unwritable(orvu_crdt::WriteError::TooManyWriters)), "{refused:?}");
    }
}
