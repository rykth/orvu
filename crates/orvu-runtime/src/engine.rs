//! The engine task. It is the only owner of a `Replica`.
//!
//! Everything arrives as a `Command` on one mpsc channel: local changes,
//! deltas (small changes) from other machines, and catch-up queries. Commands
//! are handled one at a time, so there is no question about the order things
//! are merged in and no locks are needed. Each delta that is made here or
//! applied here is broadcast to the writer task of every connection.
//!
//! Stopping has a channel of its own (`EngineHandle::stopper`), which the
//! engine looks at before each command, so a stop never waits behind the
//! commands already queued. Those are not run: each caller sees its answer
//! channel closed, the same as a call that comes after the stop.

use crate::config::{Acl, Action};
use crate::frontier::{merge_member, stable_frontier, sweep_membership, MemberEntry};
use crate::persistence::{PersistHandle, Saved};
use orvu_core::codec::MemberInfo;
use orvu_core::{Applied, Replica, SnapshotEntry, TraceId, TypedCrdt, TypedDelta, WireDelta};
use orvu_crdt::{DotContext, Hlc, ReplicaId, VersionVector};
use orvu_store::FsyncPolicy;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::{broadcast, mpsc, oneshot};

// Boxed so the closure can be stored in a command and sent over the channel.
type MutateFn = Box<dyn FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> TypedDelta + Send>;

/// A change that may refuse. The closure returns `None` to refuse.
pub type TryMutateFn = Box<dyn FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> Option<TypedDelta> + Send>;

/// What the caller gets back from a change that may be refused. On `Applied`
/// the engine has taken the change and will save it and send it out, so no
/// `WireDelta` is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryOutcome {
    NoObject,
    Refused,
    /// The object turned the write away before the change was made (see
    /// `orvu_core::MutateOutcome::Unwritable`).
    Unwritable(orvu_crdt::WriteError),
    Applied,
    /// This node can no longer save to disk, so it takes no more changes.
    StorageFailed,
}

/// What `Command::Read` runs on an object: it is given the object, or `None`
/// if there is none, and sends its answer back by itself.
pub type Look = Box<dyn FnOnce(Option<&TypedCrdt>) + Send>;

/// A request to the engine task.
pub enum Command {
    Create { object_id: String, crdt: TypedCrdt, done: oneshot::Sender<()> },
    Mutate {
        object_id: String,
        f: MutateFn,
        wall: u64,
        /// The id of the request this change was made for, if it has one.
        /// The change then carries it as its trace id, and a receipt for it
        /// is saved in the same step as the change: see `orvu_store::Receipt`.
        receipt: Option<[u8; 16]>,
        /// The delta, or why there is none: `Some` reason when the object
        /// turned the write away, `None` when it was not done for another
        /// reason (no such object, the change panicked, or saving failed).
        done: oneshot::Sender<Result<WireDelta, Option<orvu_crdt::WriteError>>>,
    },
    TryMutate {
        object_id: String,
        f: TryMutateFn,
        wall: u64,
        /// As for `Mutate`.
        receipt: Option<[u8; 16]>,
        done: oneshot::Sender<TryOutcome>,
    },
    Snapshot { object_id: String, reply: oneshot::Sender<Option<TypedCrdt>> },
    /// Look at one object where it is, without copying it. `look` is given
    /// the object, or `None` if there is no such object, and sends what it
    /// found back by itself.
    Read { object_id: String, look: Look },
    /// Take in a change from another machine. The answer is `Some` when the
    /// change was refused in a way the sender should hear about.
    Receive { delta: WireDelta, wall: u64, from: ReplicaId, reply: oneshot::Sender<Option<Refused>> },
    /// Merge a batch of snapshots sent by another machine to help us catch up.
    /// A snapshot is the whole current state of an object.
    ReceiveSnapshot { objects: Vec<SnapshotEntry>, wall: u64, from: ReplicaId, reply: oneshot::Sender<()> },
    /// For catch-up: which updates we have seen, for every object we know.
    Digest { reply: oneshot::Sender<Vec<(String, DotContext)>> },
    /// Record which updates another machine has seen for each object. This
    /// feeds the stable frontier, the set of updates every member has. The
    /// reader sends this and does not wait for an answer.
    ObservePeer {
        from: ReplicaId,
        contexts: std::collections::BTreeMap<String, DotContext>,
    },
    /// Build full snapshots of the given objects from the replica in memory.
    /// They are sent to a machine that is too far behind to catch up from the
    /// log. This is the one catch-up step that needs the replica.
    SnapshotFor {
        object_ids: Vec<String>,
        reply: oneshot::Sender<Vec<SnapshotEntry>>,
    },
    /// Merge member entries that another machine sent us. For each member the
    /// entry with the highest heartbeat wins. Entries about this node are
    /// ignored, because this node knows its own state best.
    ObserveMembers { members: Vec<MemberInfo> },
    /// Raise this node's heartbeat, refresh its own member entry, run one pass
    /// of the check for members that stopped responding, and return the member
    /// list to send to other machines.
    MembershipView { reply: oneshot::Sender<Vec<MemberInfo>> },
    /// A connection to `replica` was opened. Connections are counted, since
    /// there can be more than one.
    PeerUp { replica: ReplicaId },
    /// A connection to `replica` ended. When the count reaches 0 the member is
    /// marked `Suspect` at once, and the sweep removes it after `dead_timeout`.
    PeerDown { replica: ReplicaId },
    /// Ask what another machine last said it has seen, for each object, in
    /// its catch-up digest. Empty if nothing was heard from it.
    SeenBy { by: ReplicaId, reply: oneshot::Sender<std::collections::BTreeMap<String, VersionVector>> },
    /// Ask for the current stable frontier of every object.
    StableFrontier { reply: oneshot::Sender<std::collections::BTreeMap<String, VersionVector>> },
    /// Ask how many deltas are in the saved log right now. Used for monitoring
    /// and tests.
    TailLen { reply: oneshot::Sender<usize> },
    /// Create the object if it does not exist, and report whether it has the
    /// requested type. `true` means it was created or already had that type.
    /// `false` means it exists with a different type.
    Open { object_id: String, empty: TypedCrdt, reply: oneshot::Sender<bool> },
}

/// Asks the engine to stop, through `EngineHandle::stopper`. The engine sees
/// it before any command still in its queue. It saves everything, hands
/// every saved change to the connections, and then answers on the sender,
/// so `shutdown` can wait for it.
pub type Stopper = mpsc::Sender<oneshot::Sender<()>>;

/// Why a change from another machine was refused: a timestamp too far
/// ahead, or a change no honest machine could send. The connection it came
/// on passes this back to the sender in a `Nack`.
#[derive(Clone, Debug, PartialEq)]
pub struct Refused {
    pub reason: orvu_core::NackReason,
    pub detail: String,
}

/// A handle to the running engine task.
#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<Command>,
    stop: Stopper,
    /// Deltas made or applied on this node, for the writers to send out.
    outbound: broadcast::Sender<WireDelta>,
    /// Ids of objects whose state just changed, for anyone subscribed to changes.
    changes: broadcast::Sender<String>,
}

impl EngineHandle {
    pub fn sender(&self) -> mpsc::Sender<Command> {
        self.tx.clone()
    }
    /// Where to ask the engine to stop. See `Stopper`.
    pub fn stopper(&self) -> Stopper {
        self.stop.clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<WireDelta> {
        self.outbound.subscribe()
    }
    pub fn subscribe_changes(&self) -> broadcast::Receiver<String> {
        self.changes.subscribe()
    }
    pub fn changes_sender(&self) -> broadcast::Sender<String> {
        self.changes.clone()
    }
}

/// Everything the engine needs to save data and to recover it on startup.
/// Grouped in one struct so `spawn` takes fewer arguments and calls to it are
/// easier to read.
#[derive(Default)]
pub struct Persistence {
    /// Handle to the writer thread. `None` means nothing is saved and the node
    /// runs in memory only.
    pub handle: Option<PersistHandle>,
    pub policy: FsyncPolicy,
    /// How many deltas to save between snapshots. `0` turns compaction off.
    pub compaction_threshold: usize,
    /// The snapshot to load on startup, when recovering.
    pub snapshot: Vec<SnapshotEntry>,
    /// The deltas saved after that snapshot, replayed on startup when
    /// recovering.
    pub deltas: Vec<WireDelta>,
}

/// Starts the engine task. Callers supply the `wall` values in commands: the
/// time of day in milliseconds, which must never go backwards.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    replica: ReplicaId,
    causal_cap: usize,
    waiting_limit: std::time::Duration,
    outbound_cap: usize,
    p: Persistence,
    acl: Acl,
    suspect_timeout: std::time::Duration,
    dead_timeout: std::time::Duration,
    forget_after: std::time::Duration,
    metrics: std::sync::Arc<crate::metrics::Metrics>,
) -> EngineHandle {
    let (tx, mut rx) = mpsc::channel::<Command>(1024);
    let (stop, mut stop_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
    let (outbound, _) = broadcast::channel::<WireDelta>(outbound_cap);
    let out2 = outbound.clone();
    let (changes, _) = broadcast::channel::<String>(outbound_cap);
    let ch2 = changes.clone();

    let Persistence { handle: persist, policy, compaction_threshold, snapshot, deltas } = p;

    // Under `Interval` a local change is answered at once, but it may only
    // leave for other machines once it is on disk. Otherwise this node could
    // crash, come back without it, and give its number to a different change.
    // Each change waits here, in the order it was made, until the writer says
    // it is saved. One that is never saved never leaves.
    let (held, mut held_rx) = mpsc::unbounded_channel::<(oneshot::Receiver<Saved>, WireDelta)>();
    let out3 = outbound.clone();
    let forwarder = tokio::spawn(async move {
        while let Some((saved, d)) = held_rx.recv().await {
            if let Ok(Ok(())) = saved.await {
                make_room(&out3, outbound_cap).await;
                let _ = out3.send(d);
            }
        }
    });

    tokio::spawn(async move {
        let self_id = replica;
        let mut replica = Replica::new(replica, causal_cap)
            .with_waiting_limit(u64::try_from(waiting_limit.as_millis()).unwrap_or(u64::MAX));
        let mut objects: Vec<String> = Vec::new();
        let mut since_snapshot: usize = 0;
        let mut members: std::collections::BTreeMap<ReplicaId, MemberEntry> =
            std::collections::BTreeMap::new();
        // This node's own heartbeat. Each time the member list is built it
        // moves to the wall clock in milliseconds, or up by one if the clock
        // has not moved past it, so it never goes back while the node runs.
        // Starting from the clock rather than from 0 means a node restarted
        // under the same id, even with an empty store, is heard as newer than
        // anything it said before it stopped.
        let mut self_heartbeat: u64 = 0;
        // Members dropped as dead, with the last heartbeat heard from each and
        // when they were dropped. Another machine may not have dropped one yet
        // and still pass it on. Taking that back in would bring the member
        // back to life with the view it had when it went quiet, and hold the
        // frontier back again. So it only comes back with a newer heartbeat,
        // or a connection. After `forget_after` the entry is forgotten, so a
        // machine whose clock went far back is not shut out for good, and the
        // list does not grow without end.
        let mut gone: std::collections::BTreeMap<ReplicaId, (u64, std::time::Instant)> =
            std::collections::BTreeMap::new();
        let rng = SystemRandom::new();
        // Set by `Stop`, and answered once the engine has finished.
        let mut stopped: Option<oneshot::Sender<()>> = None;
        let mut clock_check = ClockCheck::default();

        // Recovery: load the snapshot first, then replay the deltas saved after
        // it. They were saved in the order they were applied, so none of them
        // has to wait for an earlier one. `replay` creates each missing object
        // from the CRDT type of its delta. Both move the clock past every
        // timestamp they load, so a new write here is never older than one
        // made before the restart.
        for entry in snapshot {
            if !objects.contains(&entry.object_id) {
                objects.push(entry.object_id.clone());
            }
            if let Err(e) = replica.install(&entry.object_id, entry.state, entry.context) {
                tracing::warn!(object = %entry.object_id, error = %e, "failed to install snapshot object");
            }
        }
        for d in deltas {
            if !objects.contains(&d.object_id) {
                objects.push(d.object_id.clone());
            }
            let _ = replica.replay(d.clone());
        }
        metrics.set_objects(objects.len() as u64);
        // What was loaded may have been stamped by a clock that was fast.
        clock_check.own(&replica, wall_ms(), &metrics);

        loop {
            // A stop is looked at first, so it does not wait behind the
            // queue. What is still queued is dropped after the loop, which
            // closes each caller's answer channel.
            let cmd = tokio::select! {
                biased;
                Some(done) = stop_rx.recv() => {
                    if let Some(p) = &persist {
                        let _ = p.flush().await;
                    }
                    stopped = Some(done);
                    break;
                }
                cmd = rx.recv() => match cmd {
                    Some(cmd) => cmd,
                    None => break,
                },
            };
            // Once saving has failed, memory may hold a change that is not on
            // disk. Nobody may see it, so clients and other machines are
            // refused from here on. Restarting reloads what is on disk.
            let cmd = if storage_failed(&persist) {
                match refuse_unsaved(cmd) {
                    Some(cmd) => cmd,
                    None => continue,
                }
            } else {
                cmd
            };
            match cmd {
                Command::Create { object_id, crdt, done } => {
                    replica.create(&object_id, crdt);
                    if !objects.contains(&object_id) {
                        objects.push(object_id);
                        metrics.set_objects(objects.len() as u64);
                    }
                    let _ = done.send(());
                }
                Command::Mutate { object_id, f, wall, receipt, done } => {
                    let t0 = std::time::Instant::now();
                    // The caller's closure runs here. If it panics, that must
                    // not kill the engine task, because all syncing would stop
                    // without any error being reported. So the panic is caught.
                    // A panic counts as not done: the change is dropped and
                    // the engine keeps running.
                    let mut produced = match std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| replica.write(&object_id, wall, f)),
                    ) {
                        Ok(orvu_core::MutateOutcome::Applied(d)) => Ok(d),
                        Ok(orvu_core::MutateOutcome::Unwritable(why)) => Err(Some(why)),
                        _ => Err(None),
                    };
                    if let Ok(d) = &mut produced {
                        let mut tid = [0u8; 16];
                        if let Some(id) = receipt {
                            d.trace_id = TraceId::from_array(id);
                        } else if rng.fill(&mut tid).is_ok() {
                            d.trace_id = TraceId::from_array(tid);
                        }
                        // The span's fields are recorded when it is created, so
                        // any subscriber sees them. We do not call `.entered()`.
                        // Under FsyncPolicy::Always this handler awaits, and a
                        // span guard must not be held across an `.await`.
                        let _span = tracing::info_span!("mutate", object = %object_id, trace_id = %d.trace_id.to_hex());
                        metrics.record_delta_applied_local();
                        // Save first, send second, so no other machine sees
                        // a change this one could still lose.
                        let receipt = receipt.map(|id| orvu_store::Receipt { id, at_ms: wall });
                        if save_then_send(&persist, policy, d, receipt, &out2, &held).await {
                            let _ = ch2.send(d.object_id.clone());
                            maybe_compact(&members, self_id, &objects, &persist, &mut since_snapshot, compaction_threshold, 1);
                            metrics.set_deltas_since_snapshot(since_snapshot as u64);
                        } else {
                            produced = Err(None);
                        }
                    }
                    clock_check.own(&replica, wall, &metrics);
                    metrics.mutate_latency.observe(t0.elapsed().as_secs_f64());
                    let _ = done.send(produced);
                }
                Command::TryMutate { object_id, f, wall, receipt, done } => {
                    let t0 = std::time::Instant::now();
                    let outcome = std::panic::catch_unwind(
                        std::panic::AssertUnwindSafe(|| replica.try_mutate(&object_id, wall, f)),
                    )
                    .unwrap_or(orvu_core::MutateOutcome::Refused); // a panic counts as refused and the engine keeps running
                    clock_check.own(&replica, wall, &metrics);
                    let reply = match outcome {
                        orvu_core::MutateOutcome::NoObject => TryOutcome::NoObject,
                        orvu_core::MutateOutcome::Refused => TryOutcome::Refused,
                        orvu_core::MutateOutcome::Unwritable(why) => TryOutcome::Unwritable(why),
                        orvu_core::MutateOutcome::Applied(mut d) => {
                            let mut tid = [0u8; 16];
                            if let Some(id) = receipt {
                                d.trace_id = TraceId::from_array(id);
                            } else if rng.fill(&mut tid).is_ok() {
                                d.trace_id = TraceId::from_array(tid);
                            }
                            let _span = tracing::info_span!("try_mutate", object = %object_id, trace_id = %d.trace_id.to_hex());
                            metrics.record_delta_applied_local();
                            // Save first, send second, as in `Mutate`.
                            let receipt = receipt.map(|id| orvu_store::Receipt { id, at_ms: wall });
                            if !save_then_send(&persist, policy, &d, receipt, &out2, &held).await {
                                metrics.mutate_latency.observe(t0.elapsed().as_secs_f64());
                                let _ = done.send(TryOutcome::StorageFailed);
                                continue;
                            }
                            let _ = ch2.send(d.object_id.clone());
                            maybe_compact(&members, self_id, &objects, &persist, &mut since_snapshot, compaction_threshold, 1);
                            metrics.set_deltas_since_snapshot(since_snapshot as u64);
                            TryOutcome::Applied
                        }
                    };
                    metrics.mutate_latency.observe(t0.elapsed().as_secs_f64());
                    let _ = done.send(reply);
                }
                Command::Snapshot { object_id, reply } => {
                    let snap = replica.object(&object_id).map(|o| o.crdt().clone());
                    if snap.is_some() {
                        metrics.record_object_copied();
                    }
                    let _ = reply.send(snap);
                }
                Command::Read { object_id, look } => {
                    // The caller's closure runs here, on the object itself.
                    // `NodeHandle::read` wraps it so that a panic in it is
                    // caught and handed to the caller. This catch is a second
                    // one, in case something is ever sent here unwrapped: a
                    // panic must not kill the engine task.
                    let object = replica.object(&object_id).map(|o| o.crdt());
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| look(object)));
                }
                Command::Receive { delta, wall, from, reply } => {
                    let t0 = std::time::Instant::now();
                    // Write check on the sender. `from` is the verified identity
                    // of the connection this delta arrived on. It is not the
                    // replica in `delta.dot`, which is the delta's original
                    // author. So the rule "our state only holds writes from
                    // senders allowed to write" is checked for each direct
                    // connection. It only holds for the whole cluster if every
                    // machine uses the same access list. With the same list
                    // everywhere, a replica that may not write is refused by
                    // every machine and its writes never get in. With lists
                    // that differ, a write from a refused replica C can still
                    // reach A through a machine B that lets C write: B applies
                    // it and passes it on, and A sees `from = B`, which is
                    // allowed. This does not check the original author. That
                    // would need deltas signed by their author.
                    if !acl.permits(from, Action::Write) {
                        tracing::warn!(from = from.0, object = %delta.object_id, "authz: dropping unauthorized write");
                        metrics.record_write_dropped_unauthorized();
                        let _ = reply.send(None);
                        continue;
                    }
                    let dropped_before = replica.waiting_dropped();
                    let result = replica.receive(delta.clone(), wall);
                    metrics.add_waiting_changes_dropped(replica.waiting_dropped() - dropped_before);
                    // Track the object so it appears in digests, even when
                    // `receive` created it for us. A refused delta may have
                    // created nothing.
                    if !objects.contains(&delta.object_id) && replica.object(&delta.object_id).is_some() {
                        objects.push(delta.object_id.clone());
                        metrics.set_objects(objects.len() as u64);
                    }
                    let mut refused = None;
                    match result {
                        Ok((status, drained)) => {
                            clock_check.skew(from, replica.last_lead(), &metrics);
                            let applied = if status == Applied::Applied { 1 } else { 0 };
                            let n = applied + drained.len();
                            if status == Applied::Applied {
                                // This block has no `.await`, so it is safe to
                                // enter the span. The trace_id travels with the
                                // delta, which links this apply to the mutate
                                // that made it.
                                let _span = tracing::info_span!(
                                    "apply_remote_delta",
                                    object = %delta.object_id,
                                    from = from.0,
                                    trace_id = %delta.trace_id.to_hex()
                                ).entered();
                                metrics.record_delta_applied_remote();
                                let _ = ch2.send(delta.object_id.clone());
                                if let Some(p) = &persist {
                                    p.append(delta.clone(), None);
                                }
                                let _ = out2.send(delta); // pass it on to other machines (a repeat does no harm)
                            }
                            // Deltas that were waiting for this one have now been
                            // applied too. Save them and pass them on. Otherwise
                            // machines that are not connected to everyone would
                            // never get them.
                            for d in drained {
                                metrics.record_delta_applied_remote();
                                let _ = ch2.send(d.object_id.clone());
                                if let Some(p) = &persist {
                                    p.append(d.clone(), None);
                                }
                                let _ = out2.send(d);
                            }
                            maybe_compact(&members, self_id, &objects, &persist, &mut since_snapshot, compaction_threshold, n);
                            metrics.set_deltas_since_snapshot(since_snapshot as u64);
                        }
                        Err(e @ orvu_core::Error::TooFarAhead { lead_ms, writer }) => {
                            clock_check.refused(from, writer, lead_ms, &metrics);
                            refused = Some(Refused {
                                reason: orvu_core::NackReason::Known(orvu_proto::v1::NackReason::ClockAhead),
                                detail: e.to_string(),
                            });
                        }
                        Err(e @ orvu_core::Error::MalformedPayload(_)) => {
                            metrics.record_change_refused_malformed();
                            tracing::warn!(from = from.0, object = %delta.object_id, error = %e, "refusing a change no honest machine could send");
                            refused = Some(Refused {
                                reason: orvu_core::NackReason::Known(orvu_proto::v1::NackReason::MalformedPayload),
                                detail: e.to_string(),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                object = %delta.object_id,
                                error = %e,
                                "dropping unapplicable remote delta (type mismatch or full buffer)"
                            );
                        }
                    }
                    clock_check.own(&replica, wall, &metrics);
                    metrics.remote_apply_latency.observe(t0.elapsed().as_secs_f64());
                    let _ = reply.send(refused);
                }
                Command::ReceiveSnapshot { objects: entries, wall, from, reply } => {
                    // Like the write check in `Receive` above, this checks the
                    // sender on this connection and not the original author.
                    if !acl.permits(from, Action::Write) {
                        tracing::warn!(from = from.0, "authz: dropping unauthorized snapshot");
                        metrics.record_write_dropped_unauthorized();
                        let _ = reply.send(());
                        continue;
                    }
                    let mut merged_any = false;
                    let dropped_before = replica.waiting_dropped();
                    for entry in entries {
                        let id = entry.object_id.clone();
                        match replica.merge_snapshot(&entry.object_id, entry.state, entry.context, wall) {
                            // A snapshot holding no change creates nothing.
                            Ok(drained) if replica.object(&id).is_none() => debug_assert!(drained.is_empty()),
                            Ok(drained) => {
                                clock_check.skew(from, replica.last_lead(), &metrics);
                                // Counted only once taken in, so that a
                                // refused snapshot leaves nothing behind.
                                if !objects.contains(&id) {
                                    objects.push(id.clone());
                                    metrics.set_objects(objects.len() as u64);
                                }
                                let _ = ch2.send(id.clone());
                                for d in drained {
                                    let _ = ch2.send(d.object_id.clone());
                                    let _ = out2.send(d);
                                }
                                merged_any = true;
                            }
                            Err(orvu_core::Error::TooFarAhead { lead_ms, writer }) => {
                                clock_check.refused(from, writer, lead_ms, &metrics);
                            }
                            Err(e @ orvu_core::Error::MalformedPayload(_)) => {
                                metrics.record_change_refused_malformed();
                                tracing::warn!(from = from.0, object = %id, error = %e, "refusing a snapshot no honest machine could send");
                            }
                            Err(e) => {
                                tracing::warn!(object = %id, error = %e, "failed to merge inbound snapshot");
                            }
                        }
                    }
                    metrics.add_waiting_changes_dropped(replica.waiting_dropped() - dropped_before);
                    clock_check.own(&replica, wall, &metrics);
                    // Save the merged state to disk. The store cannot append a
                    // snapshot, so a full compaction is how the merged state
                    // survives a restart without asking the sending machine
                    // again. Does nothing if there is no store.
                    if merged_any {
                        metrics.record_snapshot_received();
                        if let Some(p) = &persist {
                            p.compact(build_snapshot(&replica, &objects));
                            since_snapshot = 0;
                            metrics.set_deltas_since_snapshot(0);
                        }
                    }
                    let _ = reply.send(());
                }
                Command::Digest { reply } => {
                    let _ = reply.send(own_contexts(&replica, &objects));
                }
                Command::ObservePeer { from, contexts } => {
                    if let Some(e) = members.get_mut(&from) {
                        e.contexts = contexts;
                        e.updated_at = std::time::Instant::now();
                    }
                }
                Command::SnapshotFor { object_ids, reply } => {
                    // The snapshot is built from memory, which can hold local
                    // changes not on disk yet. Save them first, so the other
                    // machine is never given one this node could still lose.
                    // If saving fails, give nothing.
                    let saved = match &persist {
                        Some(p) => p.flush().await.is_ok(),
                        None => true,
                    };
                    let entries = if saved { build_snapshot(&replica, &object_ids) } else { Vec::new() };
                    let _ = reply.send(entries);
                }
                Command::PeerUp { replica: id } => {
                    gone.remove(&id);
                    let e = members.entry(id).or_insert_with(|| MemberEntry {
                        refs: 0,
                        contexts: std::collections::BTreeMap::new(),
                        heartbeat: 0,
                        updated_at: std::time::Instant::now(),
                        status: crate::frontier::MemberStatus::Alive,
                        suspect_since: std::time::Instant::now(),
                    });
                    e.refs += 1;
                    e.updated_at = std::time::Instant::now();
                    e.status = crate::frontier::MemberStatus::Alive;
                    // This node has a member entry too (refs 1), but it is not
                    // counted as a connected peer.
                    metrics.set_peers_up(
                        members.iter().filter(|(id, e)| **id != self_id && e.refs > 0).count() as u64,
                    );
                    metrics.set_members_known(members.len() as u64);
                }
                Command::ObserveMembers { members: gossiped } => {
                    let now = std::time::Instant::now();
                    for mi in gossiped {
                        // This node knows its own state best, so never merge
                        // an entry about it that came from another machine.
                        if mi.replica == self_id {
                            continue;
                        }
                        if let Some(&(last, at)) = gone.get(&mi.replica) {
                            if mi.heartbeat <= last && now.duration_since(at) < forget_after {
                                continue;
                            }
                            gone.remove(&mi.replica);
                        }
                        merge_member(&mut members, mi.replica, mi.heartbeat, mi.contexts, now);
                    }
                    metrics.set_members_known(members.len() as u64);
                }
                Command::MembershipView { reply } => {
                    let now = std::time::Instant::now();
                    self_heartbeat = next_heartbeat(self_heartbeat, wall_ms());
                    // Refresh this node's own entry. It holds exactly what
                    // `Command::Digest` reports, plus our heartbeat.
                    let self_contexts = own_contexts(&replica, &objects);
                    let e = members.entry(self_id).or_insert_with(|| MemberEntry {
                        refs: 1,
                        contexts: std::collections::BTreeMap::new(),
                        heartbeat: 0,
                        updated_at: now,
                        status: crate::frontier::MemberStatus::Alive,
                        suspect_since: now,
                    });
                    e.contexts = self_contexts.into_iter().collect();
                    e.heartbeat = self_heartbeat;
                    e.updated_at = now;
                    let before: Vec<(ReplicaId, u64)> = members.iter().map(|(id, m)| (*id, m.heartbeat)).collect();
                    let sweep = sweep_membership(&mut members, self_id, now, suspect_timeout, dead_timeout);
                    for (id, heartbeat) in before {
                        if !members.contains_key(&id) {
                            gone.insert(id, (heartbeat, now));
                        }
                    }
                    gone.retain(|_, (_, at)| now.duration_since(*at) < forget_after);
                    metrics.add_members_suspected(sweep.suspected);
                    metrics.add_members_evicted(sweep.evicted);
                    metrics.set_members_known(members.len() as u64);
                    let view: Vec<MemberInfo> = members
                        .iter()
                        .map(|(id, m)| MemberInfo {
                            replica: *id,
                            heartbeat: m.heartbeat,
                            contexts: m
                                .contexts
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        })
                        .collect();
                    let _ = reply.send(view);
                }
                Command::PeerDown { replica: id } => {
                    if let Some(e) = members.get_mut(&id) {
                        e.refs = e.refs.saturating_sub(1);
                        // No connections left. A dropped connection is strong
                        // evidence, so mark the member Suspect right away
                        // without waiting for suspect_timeout. Reconnecting, or
                        // hearing a higher heartbeat for it from another
                        // machine, makes it Alive again.
                        if e.refs == 0 {
                            e.status = crate::frontier::MemberStatus::Suspect;
                            e.suspect_since = std::time::Instant::now();
                            metrics.add_members_suspected(1);
                        }
                        metrics.set_peers_up(
                            members.iter().filter(|(id, e)| **id != self_id && e.refs > 0).count() as u64,
                        );
                    }
                }
                Command::SeenBy { by, reply } => {
                    let seen = members.get(&by).map(|m| {
                        m.contexts.iter().map(|(id, ctx)| (id.clone(), ctx.version_vector().clone())).collect()
                    });
                    let _ = reply.send(seen.unwrap_or_default());
                }
                Command::StableFrontier { reply } => {
                    let mut out = std::collections::BTreeMap::new();
                    for id in &objects {
                        out.insert(id.clone(), stable_frontier(&members, self_id, id));
                    }
                    let _ = reply.send(out);
                }
                Command::TailLen { reply } => {
                    let n = match &persist {
                        Some(p) => p.tail_len().await,
                        None => 0,
                    };
                    let _ = reply.send(n);
                }
                Command::Open { object_id, empty, reply } => {
                    let ok = match replica.object(&object_id) {
                        // The same enum variant means the same CRDT type.
                        Some(obj) => {
                            std::mem::discriminant(obj.crdt()) == std::mem::discriminant(&empty)
                        }
                        None => {
                            replica.create(&object_id, empty);
                            if !objects.contains(&object_id) {
                                objects.push(object_id.clone());
                                metrics.set_objects(objects.len() as u64);
                            }
                            true
                        }
                    };
                    let _ = reply.send(ok);
                }
            }
        }
        // Take no more calls, and drop every one still queued, which closes
        // its answer channel. Dropping the queue is not enough: a caller
        // whose turn came just before the close can still put its call in
        // after the queue was emptied, and that call would sit there
        // unanswered for as long as anyone holds a sender. `recv` after
        // `close` waits for such a call and hands it over.
        rx.close();
        while rx.recv().await.is_some() {}
        // Everything is saved now. Let the queue of saved changes empty out
        // to the connections before saying the engine has stopped, so that a
        // clean shutdown can still send them.
        drop(held);
        let _ = forwarder.await;
        if let Some(done) = stopped {
            let _ = done.send(());
        }
    });

    EngineHandle { tx, stop, outbound, changes }
}

/// How many times `make_room` lets the connections run before it gives up.
const ROOM_TRIES: usize = 64;

/// Waits a moment for room in the queue to the connections when it is full.
/// Many saved changes can be let go at once, after a flush and above all at
/// shutdown. Sent all together into a full queue, the oldest would push out
/// changes a connection has not taken yet, and those would be missing at the
/// other end until the next catch-up. Letting the connections run takes them
/// out of the queue. A connection stuck on a machine that does not read
/// never makes room, so this gives up after a few tries and sends anyway:
/// that one connection misses the change, and catch-up repairs it later.
async fn make_room(out: &broadcast::Sender<WireDelta>, cap: usize) {
    for _ in 0..ROOM_TRIES {
        if out.len() < cap {
            return;
        }
        tokio::task::yield_now().await;
    }
}

/// True if this node can no longer save to disk. It then takes no more
/// changes and shows its data to nobody (see `refuse_unsaved`): a change it
/// could not save could be lost in a crash, and its number given to a
/// different change after the restart.
fn storage_failed(persist: &Option<PersistHandle>) -> bool {
    persist.as_ref().is_some_and(|p| p.failure().is_some())
}

/// Answers a command that a node which can no longer save must refuse, and
/// returns `None`. Any other command is handed back to be run as usual.
///
/// Refused: every read and write from clients, and everything that would
/// show this node's data to another machine (its digest, its member list, a
/// snapshot). Changes from other machines are dropped. What is left is
/// bookkeeping that shows nothing.
fn refuse_unsaved(cmd: Command) -> Option<Command> {
    match cmd {
        Command::Create { done, .. } => {
            let _ = done.send(());
        }
        Command::Mutate { done, .. } => {
            let _ = done.send(Err(None));
        }
        Command::TryMutate { done, .. } => {
            let _ = done.send(TryOutcome::StorageFailed);
        }
        Command::Snapshot { reply, .. } => {
            let _ = reply.send(None);
        }
        // Not run. The caller's answer channel goes with it, which the caller
        // sees as no answer.
        Command::Read { look, .. } => drop(look),
        Command::Open { reply, .. } => {
            let _ = reply.send(false);
        }
        Command::Receive { reply, .. } => {
            let _ = reply.send(None);
        }
        Command::ReceiveSnapshot { reply, .. } => {
            let _ = reply.send(());
        }
        Command::SnapshotFor { reply, .. } => {
            let _ = reply.send(Vec::new());
        }
        // Dropped unanswered, which ends the catch-up loop that asked.
        Command::Digest { reply } => drop(reply),
        Command::MembershipView { reply } => drop(reply),
        other => return Some(other),
    }
    None
}

/// Saves a local change and sends it to other machines, in that order.
/// Returns false if the change could not be saved: it is then not sent, and
/// must not be reported as done.
///
/// Under `Always` this waits for the change to be on disk, then sends it.
/// Under `Interval` it returns at once and hands the change to the queue that
/// sends it once the writer has saved it.
async fn save_then_send(
    persist: &Option<PersistHandle>,
    policy: FsyncPolicy,
    d: &WireDelta,
    receipt: Option<orvu_store::Receipt>,
    outbound: &broadcast::Sender<WireDelta>,
    held: &mpsc::UnboundedSender<(oneshot::Receiver<Saved>, WireDelta)>,
) -> bool {
    let Some(p) = persist else {
        let _ = outbound.send(d.clone());
        return true;
    };
    let (ack, saved) = oneshot::channel();
    match receipt {
        Some(receipt) => p.append_with_receipt(d.clone(), receipt, Some(ack)),
        None => p.append(d.clone(), Some(ack)),
    }
    match policy {
        FsyncPolicy::Always => match saved.await {
            Ok(Ok(())) => {
                let _ = outbound.send(d.clone());
                true
            }
            Ok(Err(why)) => {
                tracing::error!(object = %d.object_id, error = %why, "a change could not be saved; it was not sent");
                false
            }
            Err(_) => {
                tracing::error!(object = %d.object_id, "the writer stopped before the change was saved; it was not sent");
                false
            }
        },
        FsyncPolicy::Interval { .. } => {
            let _ = held.send((saved, d.clone()));
            true
        }
    }
}

/// A timestamp further ahead of this node's clock than this, in
/// milliseconds, is taken in but counted and logged as clock skew.
pub const CLOCK_SKEW_WARN_MS: u64 = 1_000;

/// How often at most a clock warning is logged for one peer.
const CLOCK_WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// This node's own clock running further ahead of its wall time than this,
/// in milliseconds, is logged as a warning: half the limit other machines
/// hold its changes to.
pub const CLOCK_AHEAD_WARN_MS: u64 = orvu_core::MAX_CLOCK_LEAD_MS / 2;

/// Counts changes from peers whose clocks run ahead of this node's, and logs
/// them, at most once a minute per peer and kind so a peer with a bad clock
/// cannot flood the log. Also watches this node's own clock.
#[derive(Default)]
struct ClockCheck {
    last_warned: std::collections::BTreeMap<(ReplicaId, bool), std::time::Instant>,
    own_warned: Option<std::time::Instant>,
}

impl ClockCheck {
    fn due(&mut self, from: ReplicaId, refused: bool) -> bool {
        let now = std::time::Instant::now();
        match self.last_warned.get(&(from, refused)) {
            Some(t) if now.duration_since(*t) < CLOCK_WARN_EVERY => false,
            _ => {
                self.last_warned.insert((from, refused), now);
                true
            }
        }
    }

    /// A change or snapshot was refused for being too far ahead. `from`
    /// passed it on; `writer` stamped it.
    fn refused(&mut self, from: ReplicaId, writer: ReplicaId, lead_ms: u64, metrics: &crate::metrics::Metrics) {
        metrics.record_refused_clock_ahead(writer.0);
        if self.due(from, true) {
            tracing::warn!(
                from = from.0,
                writer = writer.0,
                lead_ms,
                limit_ms = orvu_core::MAX_CLOCK_LEAD_MS,
                "refusing a change stamped too far ahead of this node's clock. The machine named `writer` \
                 stamped it and `from` passed it on. The time may have come from another machine whose clock \
                 is fast, as a machine takes on every time it receives. Check the clocks of all the machines"
            );
        }
    }

    /// At start-up, after a write or after taking changes in: hands this
    /// node's clock to the metrics, and warns if it runs ahead of `now_wall`.
    fn own(&mut self, replica: &Replica, now_wall: u64, metrics: &crate::metrics::Metrics) {
        // The lead over time 0 is the clock's own time.
        metrics.set_clock_wall_ms(replica.clock_lead(0));
        let ahead_ms = replica.clock_lead(now_wall);
        if ahead_ms <= CLOCK_AHEAD_WARN_MS {
            return;
        }
        let now = std::time::Instant::now();
        if self.own_warned.is_some_and(|t| now.duration_since(t) < CLOCK_WARN_EVERY) {
            return;
        }
        self.own_warned = Some(now);
        tracing::warn!(
            ahead_ms,
            limit_ms = orvu_core::MAX_CLOCK_LEAD_MS,
            "this machine's clock is ahead of its own time, probably because another machine's clock is fast \
             or this machine's time was set back. Machines whose clocks are behind will refuse its changes \
             once it is more than `limit_ms` ahead of them, until real time catches up"
        );
    }

    /// A change or snapshot was taken in. `lead_ms` is how far ahead of this
    /// node's clock it was.
    fn skew(&mut self, from: ReplicaId, lead_ms: u64, metrics: &crate::metrics::Metrics) {
        if lead_ms <= CLOCK_SKEW_WARN_MS {
            return;
        }
        metrics.record_clock_skew();
        if self.due(from, false) {
            tracing::warn!(from = from.0, lead_ms, "a peer's clock runs ahead of this node's");
        }
    }
}

/// For every tracked object, which updates this node has seen. This is what
/// the node sends as its catch-up digest, and also what goes in its own member
/// entry.
fn own_contexts(replica: &Replica, objects: &[String]) -> Vec<(String, DotContext)> {
    objects
        .iter()
        .filter_map(|id| replica.observed_context(id).map(|ctx| (id.clone(), ctx.clone())))
        .collect()
}

/// Builds a snapshot of each object in `objects`: its current state with every
/// applied delta merged in. Ids the replica does not have are skipped.
fn build_snapshot(replica: &Replica, objects: &[String]) -> Vec<SnapshotEntry> {
    objects
        .iter()
        .filter_map(|id| {
            replica.object(id).map(|o| SnapshotEntry {
                object_id: id.clone(),
                state: o.crdt().snapshot(),
                context: o.context().clone(),
            })
        })
        .collect()
}

/// Adds `appended` to the count of deltas saved since the last compaction.
/// When the count reaches `threshold`, asks the writer for a compaction guided
/// by the stable frontier: deltas every member has are merged into the
/// snapshot and the rest stay in the log. Does nothing without a writer or
/// when `threshold` is zero.
fn maybe_compact(
    members: &std::collections::BTreeMap<ReplicaId, MemberEntry>,
    self_id: ReplicaId,
    objects: &[String],
    persist: &Option<PersistHandle>,
    since: &mut usize,
    threshold: usize,
    appended: usize,
) {
    let Some(p) = persist else { return };
    if threshold == 0 {
        return;
    }
    *since += appended;
    if *since >= threshold {
        let mut frontier = std::collections::BTreeMap::new();
        for id in objects {
            frontier.insert(id.clone(), stable_frontier(members, self_id, id));
        }
        p.frontier_compact(frontier, threshold);
        *since = 0;
    }
}

/// The wall clock in milliseconds since 1970, or 0 if the clock is set
/// before that.
fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The heartbeat after `last`: the wall clock `now_ms` when it is ahead,
/// otherwise one more than `last`. It never goes back, even if the clock does.
fn next_heartbeat(last: u64, now_ms: u64) -> u64 {
    now_ms.max(last.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::spawn_writer;
    use orvu_core::TypedDelta;
    use orvu_crdt::{Dot, PnCounter};
    use orvu_store::{FsyncPolicy, MemStore, Store};
    use tokio::sync::oneshot;

    /// Suspect and dead timeout for tests. Long enough that no member is
    /// removed in the middle of a test.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// How long a change may wait for what it depends on, for tests that do
    /// not look at it.
    const WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    /// How long a dropped member is remembered, for tests that do not look
    /// at it.
    const FORGET: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

    /// A new metrics registry for each test engine.
    fn mx() -> std::sync::Arc<crate::metrics::Metrics> {
        std::sync::Arc::new(crate::metrics::Metrics::default())
    }

    async fn call<T>(tx: &mpsc::Sender<Command>, make: impl FnOnce(oneshot::Sender<T>) -> Command) -> T {
        let (reply, rx) = oneshot::channel();
        tx.send(make(reply)).await.unwrap();
        rx.await.unwrap()
    }

    // A member dropped as dead is not brought back by another machine that
    // still passes on the old news about it. Only a newer heartbeat, or a
    // connection, brings it back.
    #[tokio::test]
    async fn a_dead_member_is_not_brought_back_by_old_news() {
        let quick = std::time::Duration::from_millis(20);
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), quick, quick, FORGET, mx());
        let tx = eng.sender();
        let news = |heartbeat| vec![MemberInfo { replica: ReplicaId(5), heartbeat, contexts: Vec::new() }];
        let view = || async {
            call(&tx, |reply| Command::MembershipView { reply }).await.into_iter().map(|m| m.replica).collect::<Vec<_>>()
        };
        tx.send(Command::ObserveMembers { members: news(3) }).await.unwrap();
        assert!(view().await.contains(&ReplicaId(5)));
        // Quiet for long enough: suspected, then dropped.
        for _ in 0..3 {
            tokio::time::sleep(quick * 2).await;
            view().await;
        }
        assert!(!view().await.contains(&ReplicaId(5)), "a quiet member was never dropped");

        tx.send(Command::ObserveMembers { members: news(3) }).await.unwrap();
        assert!(!view().await.contains(&ReplicaId(5)), "old news brought a dead member back");

        tx.send(Command::ObserveMembers { members: news(4) }).await.unwrap();
        assert!(view().await.contains(&ReplicaId(5)), "a newer heartbeat did not bring it back");
    }

    // A dropped member is remembered only for a while. After that, news about
    // it is taken even if its heartbeat is not newer, so a machine whose clock
    // went far back is not shut out for good.
    #[tokio::test]
    async fn a_dropped_member_is_forgotten_after_a_while() {
        let quick = std::time::Duration::from_millis(20);
        let forget = std::time::Duration::from_millis(200);
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), quick, quick, forget, mx());
        let tx = eng.sender();
        let news = |heartbeat| vec![MemberInfo { replica: ReplicaId(5), heartbeat, contexts: Vec::new() }];
        let view = || async {
            call(&tx, |reply| Command::MembershipView { reply }).await.into_iter().map(|m| m.replica).collect::<Vec<_>>()
        };
        tx.send(Command::ObserveMembers { members: news(3) }).await.unwrap();
        assert!(view().await.contains(&ReplicaId(5)));
        for _ in 0..3 {
            tokio::time::sleep(quick * 2).await;
            view().await;
        }
        assert!(!view().await.contains(&ReplicaId(5)), "a quiet member was never dropped");
        tx.send(Command::ObserveMembers { members: news(2) }).await.unwrap();
        assert!(!view().await.contains(&ReplicaId(5)), "old news brought it back too soon");

        tokio::time::sleep(forget + quick).await;
        tx.send(Command::ObserveMembers { members: news(2) }).await.unwrap();
        assert!(view().await.contains(&ReplicaId(5)), "a dropped member was never forgotten");
    }

    // The heartbeat follows the wall clock, and never goes back when the
    // clock does.
    #[test]
    fn the_heartbeat_follows_the_clock_and_never_goes_back() {
        assert_eq!(next_heartbeat(0, 1_000), 1_000);
        assert_eq!(next_heartbeat(1_000, 1_005), 1_005);
        assert_eq!(next_heartbeat(1_005, 1_005), 1_006);
        assert_eq!(next_heartbeat(1_006, 10), 1_007);
        assert_eq!(next_heartbeat(u64::MAX, 10), u64::MAX);
        assert!(wall_ms() > 1_600_000_000_000, "the heartbeat starts from the wall clock");
    }

    #[tokio::test]
    async fn create_mutate_snapshot_through_the_engine() {
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();

        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;

        let d = call(&tx, |done| Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _hlc, _replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
                _ => unreachable!(),
            }),
            wall: 10,
            receipt: None,
            done,
        }).await;
        assert!(d.is_ok());

        let snap = call(&tx, |reply| Command::Snapshot { object_id: "c".into(), reply }).await;
        match snap {
            Some(TypedCrdt::PnCounter(pn)) => assert_eq!(pn.value(), 5),
            _ => panic!("expected counter = 5"),
        }
    }

    // A snapshot this machine refuses does not count as an object it has.
    #[tokio::test]
    async fn a_refused_snapshot_adds_no_object() {
        let m = mx();
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, m.clone());
        let tx = eng.sender();
        let state = TypedCrdt::PnCounter(PnCounter::default()).snapshot();
        let mut gapped = DotContext::new();
        gapped.insert(Dot::new(ReplicaId(9), 2));
        let entry = |id: &str, context: DotContext| SnapshotEntry { object_id: id.into(), state: state.clone(), context };

        call(&tx, |reply| Command::ReceiveSnapshot { objects: vec![entry("x", gapped)], wall: 1, from: ReplicaId(2), reply }).await;
        assert_eq!(m.objects.load(std::sync::atomic::Ordering::Relaxed), 0);

        let mut whole = DotContext::new();
        whole.insert(Dot::new(ReplicaId(9), 1));
        call(&tx, |reply| Command::ReceiveSnapshot { objects: vec![entry("y", whole)], wall: 1, from: ReplicaId(2), reply }).await;
        assert_eq!(m.objects.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    fn set_register(value: &str, wall: u64, done: oneshot::Sender<Result<WireDelta, Option<orvu_crdt::WriteError>>>) -> Command {
        let value = value.to_string();
        Command::Mutate {
            object_id: "r".into(),
            f: Box::new(move |crdt, hlc, r| match crdt {
                TypedCrdt::LwwRegister(reg) => TypedDelta::LwwRegister(reg.set(value, hlc, r)),
                _ => unreachable!(),
            }),
            wall,
            receipt: None,
            done,
        }
    }

    async fn register_value(tx: &mpsc::Sender<Command>) -> Option<String> {
        match call(tx, |reply| Command::Snapshot { object_id: "r".into(), reply }).await {
            Some(TypedCrdt::LwwRegister(reg)) => reg.get().cloned(),
            other => panic!("expected a register, got {other:?}"),
        }
    }

    // A write that lives only in the snapshot after a compaction still moves
    // the clock when the node starts again. Otherwise a new write, made while
    // the wall clock is behind the old one, would get an older timestamp and
    // lose to it without a word.
    #[tokio::test]
    async fn after_a_restart_a_new_write_beats_one_kept_only_in_the_snapshot() {
        let store = MemStore::new();
        let (persist, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Always);
        let eng = spawn(
            ReplicaId(1),
            64,
            WAIT,
            64,
            Persistence { handle: Some(persist), policy: FsyncPolicy::Always, compaction_threshold: 1, ..Default::default() },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            mx(),
        );
        let tx = eng.sender();
        call(&tx, |done| Command::Create {
            object_id: "r".into(),
            crdt: TypedCrdt::LwwRegister(Default::default()),
            done,
        })
        .await;
        call(&tx, |done| set_register("first", 1_000_000, done)).await.unwrap();
        call(&tx, |done| set_register("old", 1_000_000, done)).await.unwrap();

        // Wait until both writes have been folded into the snapshot.
        let mut tries = 0;
        while !store.replay().unwrap().is_empty() || store.load_snapshot().unwrap().is_empty() {
            tries += 1;
            assert!(tries < 500, "the log was never compacted");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let eng2 = spawn(
            ReplicaId(1),
            64,
            WAIT,
            64,
            Persistence { snapshot: store.load_snapshot().unwrap(), deltas: store.replay().unwrap(), ..Default::default() },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            mx(),
        );
        let tx2 = eng2.sender();
        assert_eq!(register_value(&tx2).await.as_deref(), Some("old"));
        // The wall clock is now far behind the old write.
        call(&tx2, |done| set_register("new", 10, done)).await.unwrap();
        assert_eq!(register_value(&tx2).await.as_deref(), Some("new"));
    }

    fn register_delta_from(replica: u64, counter: u64, value: &str, wall: u64) -> WireDelta {
        let mut reg = orvu_crdt::LwwRegister::default();
        let hlc = Hlc { wall, logical: 0 };
        let mut deps = DotContext::new();
        for c in 1..counter {
            deps.insert(Dot::new(ReplicaId(replica), c));
        }
        WireDelta {
            object_id: "r".into(),
            dot: Dot::new(ReplicaId(replica), counter),
            hlc,
            deps,
            delta: TypedDelta::LwwRegister(reg.set(value.to_string(), hlc, ReplicaId(replica))),
            trace_id: orvu_core::TraceId::none(),
        }
    }

    // A change stamped far ahead of this node's clock is refused and counted.
    // One only a little ahead is applied, and counted as clock skew.
    #[tokio::test]
    async fn changes_from_a_clock_that_runs_ahead_are_counted() {
        let m = mx();
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, m.clone());
        let tx = eng.sender();
        let now = 1_000_000_000;

        let far = register_delta_from(2, 1, "future", now + 10 * 60_000);
        call(&tx, |reply| Command::Receive { delta: far, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot().changes_refused_clock_ahead, 1);
        assert_eq!(m.snapshot().objects, 0, "a refused change leaves no object behind");
        assert!(call(&tx, |reply| Command::Snapshot { object_id: "r".into(), reply }).await.is_none());

        let near = register_delta_from(2, 1, "near", now + 2_000);
        call(&tx, |reply| Command::Receive { delta: near, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(register_value(&tx).await.as_deref(), Some("near"));
        assert_eq!(m.snapshot().clock_skew_seen, 1);

        let on_time = register_delta_from(2, 2, "on time", now + 3);
        call(&tx, |reply| Command::Receive { delta: on_time, wall: now + 3, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot().clock_skew_seen, 1);
        assert_eq!(m.snapshot().changes_refused_clock_ahead, 1);

        // A snapshot that brings a far-future value is refused the same way.
        let mut reg = orvu_crdt::LwwRegister::default();
        let snap = SnapshotEntry {
            object_id: "r".into(),
            state: TypedDelta::LwwRegister(reg.set("future".into(), Hlc { wall: now + 10 * 60_000, logical: 0 }, ReplicaId(3))),
            context: {
                let mut c = DotContext::new();
                c.insert(Dot::new(ReplicaId(3), 1));
                c
            },
        };
        call(&tx, |reply| Command::ReceiveSnapshot { objects: vec![snap], wall: now, from: ReplicaId(3), reply }).await;
        assert_eq!(m.snapshot().changes_refused_clock_ahead, 2);
        assert_eq!(register_value(&tx).await.as_deref(), Some("near"));
    }

    // A change waiting for one that never comes is dropped once it has
    // waited longer than the limit, and counted. A change that only waits
    // leaves no object behind. Changes and snapshots no honest machine could
    // send are refused and counted.
    #[tokio::test]
    async fn dropped_and_refused_changes_are_counted() {
        let m = mx();
        let limit = std::time::Duration::from_millis(1_000);
        let eng = spawn(ReplicaId(1), 64, limit, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, m.clone());
        let tx = eng.sender();
        let now = 1_000_000_000;

        let waits = register_delta_from(2, 2, "waits", now);
        call(&tx, |reply| Command::Receive { delta: waits, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot().objects, 0, "a waiting change created an object");
        assert!(call(&tx, |reply| Command::Snapshot { object_id: "r".into(), reply }).await.is_none());

        let also_waits = register_delta_from(3, 2, "also waits", now + 1_001);
        call(&tx, |reply| Command::Receive { delta: also_waits, wall: now + 1_001, from: ReplicaId(3), reply }).await;
        assert_eq!(m.snapshot().waiting_changes_dropped, 1);

        // A snapshot whose record has gaps.
        let gaps = SnapshotEntry {
            object_id: "r".into(),
            state: TypedCrdt::LwwRegister(orvu_crdt::LwwRegister::default()).snapshot(),
            context: DotContext::from_parts(Default::default(), [Dot::new(ReplicaId(2), 3)]),
        };
        call(&tx, |reply| Command::ReceiveSnapshot { objects: vec![gaps], wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot().changes_refused_malformed, 1);
        assert_eq!(m.snapshot().objects, 0);

        // A snapshot holding no change is not refused, but creates nothing.
        let empty = SnapshotEntry {
            object_id: "empty".into(),
            state: TypedCrdt::LwwRegister(orvu_crdt::LwwRegister::default()).snapshot(),
            context: DotContext::new(),
        };
        call(&tx, |reply| Command::ReceiveSnapshot { objects: vec![empty], wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot().objects, 0, "an empty snapshot created an object");
        assert!(call(&tx, |reply| Command::Digest { reply }).await.is_empty());

        // A change whose number skips ahead.
        let mut skips = register_delta_from(2, 3, "skips", now);
        skips.deps = DotContext::new();
        call(&tx, |reply| Command::Receive { delta: skips, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot().changes_refused_malformed, 2);
    }

    // The engine answers a refused change with why it was refused, so the
    // connection can tell the machine that sent it. An applied change gets
    // no answer of that kind.
    #[tokio::test]
    async fn a_refused_change_is_answered_with_why() {
        use orvu_proto::v1::NackReason as Reason;
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();
        let now = 1_000_000_000;

        let far = register_delta_from(2, 1, "future", now + 10 * 60_000);
        let refused = call(&tx, |reply| Command::Receive { delta: far, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(refused.map(|r| r.reason), Some(orvu_core::NackReason::Known(Reason::ClockAhead)));

        let mut skips = register_delta_from(2, 3, "skips", now);
        skips.deps = DotContext::new();
        let refused = call(&tx, |reply| Command::Receive { delta: skips, wall: now, from: ReplicaId(2), reply }).await;
        let refused = refused.expect("a change that skips ahead was not refused");
        assert_eq!(refused.reason, orvu_core::NackReason::Known(Reason::MalformedPayload));
        assert!(!refused.detail.is_empty());

        let fine = register_delta_from(2, 1, "fine", now);
        let refused = call(&tx, |reply| Command::Receive { delta: fine, wall: now, from: ReplicaId(2), reply }).await;
        assert!(refused.is_none(), "an applied change was answered as refused");
    }

    /// Collects every log line written while it is the default subscriber.
    /// The engine task runs on the test's own thread, so its lines land here.
    #[derive(Clone, Default)]
    struct LogLines(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogLines {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl LogLines {
        fn capture(&self) -> tracing::subscriber::DefaultGuard {
            let me = self.clone();
            let sub = tracing_subscriber::fmt().with_ansi(false).with_writer(move || me.clone()).finish();
            tracing::subscriber::set_default(sub)
        }
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    const MINUTE: u64 = 60_000;

    // A timestamp taken in becomes this node's clock. When that leaves the
    // clock well ahead of the node's own wall time, the node says so: in a
    // gauge, and once in the log, naming how far ahead it is.
    #[tokio::test]
    async fn a_node_whose_clock_was_pushed_ahead_reports_it() {
        let logs = LogLines::default();
        let _guard = logs.capture();
        let m = mx();
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, m.clone());
        let tx = eng.sender();
        let now = 1_000_000_000;

        // A little ahead: shown in the gauge, but no warning.
        let near = register_delta_from(2, 1, "near", now + 2_000);
        call(&tx, |reply| Command::Receive { delta: near, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot_at(now).clock_ahead_of_wall_ms, 2_000);
        assert!(!logs.text().contains("this machine's clock is ahead"), "{}", logs.text());

        // Four minutes ahead, more than half the limit: the node warns.
        let fast = register_delta_from(2, 2, "fast", now + 4 * MINUTE);
        call(&tx, |reply| Command::Receive { delta: fast, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot_at(now).clock_ahead_of_wall_ms, 4 * MINUTE);
        let text = logs.text();
        assert_eq!(text.matches("this machine's clock is ahead").count(), 1, "{text}");
        assert!(text.contains("ahead_ms=240000"), "{text}");

        // A local write made while the wall clock is behind shows the lead
        // too. The warning is not repeated within the minute.
        call(&tx, |done| set_register("mine", now + MINUTE, done)).await.unwrap();
        assert_eq!(m.snapshot_at(now + MINUTE).clock_ahead_of_wall_ms, 3 * MINUTE);
        assert_eq!(logs.text().matches("this machine's clock is ahead").count(), 1);

        // Once real time catches up the gauge goes back to nothing.
        call(&tx, |done| set_register("later", now + 5 * MINUTE, done)).await.unwrap();
        assert_eq!(m.snapshot_at(now + 5 * MINUTE).clock_ahead_of_wall_ms, 0);
    }

    // The lead is worked out when the metrics are read, so it shrinks as
    // real time catches up even while the node does nothing.
    #[tokio::test]
    async fn the_clock_lead_falls_as_time_passes_with_no_activity() {
        // Logging is captured here too: a warning first hit on a thread with
        // nothing listening can be switched off for the other clock tests.
        let _guard = LogLines::default().capture();
        let m = mx();
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, m.clone());
        let tx = eng.sender();
        let now = 1_000_000_000;
        let fast = register_delta_from(2, 1, "fast", now + 4 * MINUTE);
        call(&tx, |reply| Command::Receive { delta: fast, wall: now, from: ReplicaId(2), reply }).await;
        assert_eq!(m.snapshot_at(now).clock_ahead_of_wall_ms, 4 * MINUTE);
        assert_eq!(m.snapshot_at(now + MINUTE).clock_ahead_of_wall_ms, 3 * MINUTE);
        assert_eq!(m.snapshot_at(now + 5 * MINUTE).clock_ahead_of_wall_ms, 0);
    }

    // A node started from saved changes stamped by a fast clock shows the
    // lead at once, before it writes or hears anything.
    #[tokio::test]
    async fn a_node_restarted_from_changes_stamped_ahead_reports_the_lead_at_once() {
        // Logging is captured here too: a warning first hit on a thread with
        // nothing listening can be switched off for the other clock tests.
        let _guard = LogLines::default().capture();
        let m = mx();
        let ahead = wall_ms() + 4 * MINUTE;
        let saved = register_delta_from(2, 1, "fast", ahead);
        let eng = spawn(
            ReplicaId(1),
            64,
            WAIT,
            64,
            Persistence { deltas: vec![saved], ..Default::default() },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            m.clone(),
        );
        // A read only waits for start-up to finish; it changes nothing.
        assert_eq!(register_value(&eng.sender()).await.as_deref(), Some("fast"));
        let lead = m.snapshot().clock_ahead_of_wall_ms;
        assert!((4 * MINUTE - 30_000..=4 * MINUTE).contains(&lead), "lead {lead}");
    }

    // A refused change is logged with the machine that wrote it as well as
    // the one that passed it on, and the metrics name the writer too.
    #[tokio::test]
    async fn a_refusal_names_the_machine_that_wrote_the_change() {
        let logs = LogLines::default();
        let _guard = logs.capture();
        let m = mx();
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, m.clone());
        let tx = eng.sender();
        let now = 1_000_000_000;

        // Machine 7 wrote it; machine 2 passed it on.
        let far = register_delta_from(7, 1, "future", now + 10 * MINUTE);
        call(&tx, |reply| Command::Receive { delta: far, wall: now, from: ReplicaId(2), reply }).await;
        let text = logs.text();
        assert!(text.contains("writer=7"), "{text}");
        assert!(text.contains("from=2"), "{text}");
        assert!(text.contains("another machine"), "{text}");
        assert_eq!(m.snapshot().changes_refused_clock_ahead, 1);
        assert_eq!(m.snapshot().refused_clock_ahead_last_writer, Some(7));
        let page = crate::metrics::render_prometheus(&m.snapshot());
        assert!(page.contains("orvu_refused_clock_ahead_last_writer{writer=\"7\"} 1"), "{page}");
    }

    #[tokio::test]
    async fn engine_recovers_state_from_a_seeded_log() {
        // First, run an engine that saves to a shared MemStore and raise the
        // counter to 5.
        let store = MemStore::new();
        let (persist, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Always);
        let eng = spawn(
            ReplicaId(1),
            64,
            WAIT,
            64,
            Persistence { handle: Some(persist), policy: FsyncPolicy::Always, ..Default::default() },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            mx(),
        );
        let tx = eng.sender();
        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;
        call(&tx, |done| Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _hlc, _replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
                _ => unreachable!(),
            }),
            wall: 10,
            receipt: None,
            done,
        }).await.unwrap(); // under Always the delta is on disk when this returns

        // Then "restart": a new engine loaded from the same store.
        let seed = store.replay().unwrap();
        assert_eq!(seed.len(), 1, "the mutate delta was persisted");
        let eng2 = spawn(ReplicaId(1), 64, WAIT, 64, Persistence { deltas: seed, ..Default::default() }, Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx2 = eng2.sender();
        let snap = call(&tx2, |reply| Command::Snapshot { object_id: "c".into(), reply }).await;
        match snap {
            Some(TypedCrdt::PnCounter(pn)) => assert_eq!(pn.value(), 5, "state recovered from the log"),
            _ => panic!("expected counter = 5 after recovery"),
        }
    }

    /// A store whose `flush` waits until the test lets it through.
    struct GatedStore {
        inner: MemStore,
        open: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl orvu_store::Store for GatedStore {
        fn append(&mut self, delta: &WireDelta) -> orvu_store::Result<()> {
            self.inner.append(delta)
        }
        fn receipt(&mut self, receipt: orvu_store::Receipt) -> orvu_store::Result<()> {
            self.inner.receipt(receipt)
        }
        fn receipts(&self) -> orvu_store::Result<Vec<orvu_store::Receipt>> {
            self.inner.receipts()
        }
        fn replay(&self) -> orvu_store::Result<Vec<WireDelta>> {
            self.inner.replay()
        }
        fn flush(&mut self) -> orvu_store::Result<()> {
            let (lock, wake) = &*self.open;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = wake.wait(open).unwrap();
            }
            self.inner.flush()
        }
        fn compact_to(&mut self, snapshot: &[SnapshotEntry], tail: &[WireDelta]) -> orvu_store::Result<()> {
            self.inner.compact_to(snapshot, tail)
        }
        fn load_snapshot(&self) -> orvu_store::Result<Vec<SnapshotEntry>> {
            self.inner.load_snapshot()
        }
        fn deltas_peer_lacks(
            &self,
            peer: &std::collections::BTreeMap<String, DotContext>,
        ) -> orvu_store::Result<Vec<WireDelta>> {
            self.inner.deltas_peer_lacks(peer)
        }
        fn snapshot_contexts(&self) -> orvu_store::Result<std::collections::BTreeMap<String, DotContext>> {
            self.inner.snapshot_contexts()
        }
    }

    // Under `Always`, a change must be on disk before it is handed to the
    // writers that send it to other machines.
    #[tokio::test(flavor = "multi_thread")]
    async fn under_always_a_change_is_saved_before_it_is_sent() {
        let open = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let store = GatedStore { inner: MemStore::new(), open: open.clone() };
        let (persist, _exit) = spawn_writer(Box::new(store), FsyncPolicy::Always);
        let eng = spawn(
            ReplicaId(1),
            64,
            WAIT,
            64,
            Persistence { handle: Some(persist), policy: FsyncPolicy::Always, ..Default::default() },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            mx(),
        );
        let tx = eng.sender();
        let mut outgoing = eng.subscribe();
        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        })
        .await;

        let (done, finished) = oneshot::channel();
        tx.send(Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _h, r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, 5)),
                _ => unreachable!(),
            }),
            wall: 10,
            receipt: None,
            done,
        })
        .await
        .ok()
        .unwrap();

        // The save is held back, so nothing may have been sent yet.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(outgoing.try_recv().is_err(), "the change was sent before it was saved");

        // Let the save through. Now the change is sent and reported as done.
        *open.0.lock().unwrap() = true;
        open.1.notify_all();
        assert!(finished.await.unwrap().is_ok());
        assert!(outgoing.try_recv().is_ok());
    }

    /// Starts an engine for replica 1 that saves to `store` under `policy`,
    /// with one counter "c" already created.
    async fn engine_on(store: Box<dyn Store>, policy: FsyncPolicy) -> EngineHandle {
        let (persist, _exit) = spawn_writer(store, policy);
        let eng = spawn(
            ReplicaId(1),
            64,
            WAIT,
            64,
            Persistence { handle: Some(persist), policy, ..Default::default() },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            mx(),
        );
        call(&eng.sender(), |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        })
        .await;
        eng
    }

    fn add_one(done: oneshot::Sender<Result<WireDelta, Option<orvu_crdt::WriteError>>>) -> Command {
        Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _h, r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, 1)),
                _ => unreachable!(),
            }),
            wall: 10,
            receipt: None,
            done,
        }
    }

    // Under `Interval` a change is answered at once but only leaves for other
    // machines once it is on disk. If it left first and this machine then
    // crashed, it would come back without the change and could give the same
    // number to a different one.
    #[tokio::test(flavor = "multi_thread")]
    async fn under_interval_a_change_leaves_only_once_it_is_on_disk() {
        let store = MemStore::new();
        let eng = engine_on(Box::new(store.clone()), FsyncPolicy::Interval { ms: 300 }).await;
        let tx = eng.sender();
        let mut outgoing = eng.subscribe();

        // The caller does not wait for the save.
        let started = std::time::Instant::now();
        let mut made = Vec::new();
        for _ in 0..3 {
            made.push(call(&tx, add_one).await.unwrap().dot);
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(250), "the caller waited for the disk");

        // Each change that goes out is already in what a restart would find,
        // and they go out in the order they were made.
        for want in made {
            let got = tokio::time::timeout(std::time::Duration::from_secs(3), outgoing.recv())
                .await
                .expect("the change was never sent")
                .unwrap();
            assert_eq!(got.dot, want, "changes went out of order");
            let on_disk: Vec<_> = store.reopened().replay().unwrap().iter().map(|d| d.dot).collect();
            assert!(on_disk.contains(&got.dot), "a change was sent before it was saved");
        }
    }

    /// A store that can be told to fail its appends or its flushes.
    #[derive(Clone, Default)]
    struct FlakyStore {
        inner: MemStore,
        fail_append: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fail_flush: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FlakyStore {
        fn broken(flag: &std::sync::atomic::AtomicBool) -> bool {
            flag.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl orvu_store::Store for FlakyStore {
        fn append(&mut self, delta: &WireDelta) -> orvu_store::Result<()> {
            if Self::broken(&self.fail_append) {
                return Err(orvu_store::StoreError::Io(std::io::ErrorKind::StorageFull.into()));
            }
            self.inner.append(delta)
        }
        fn receipt(&mut self, receipt: orvu_store::Receipt) -> orvu_store::Result<()> {
            self.inner.receipt(receipt)
        }
        fn receipts(&self) -> orvu_store::Result<Vec<orvu_store::Receipt>> {
            self.inner.receipts()
        }
        fn replay(&self) -> orvu_store::Result<Vec<WireDelta>> {
            self.inner.replay()
        }
        fn flush(&mut self) -> orvu_store::Result<()> {
            if Self::broken(&self.fail_flush) {
                return Err(orvu_store::StoreError::Io(std::io::ErrorKind::StorageFull.into()));
            }
            self.inner.flush()
        }
        fn compact_to(&mut self, snapshot: &[SnapshotEntry], tail: &[WireDelta]) -> orvu_store::Result<()> {
            self.inner.compact_to(snapshot, tail)
        }
        fn load_snapshot(&self) -> orvu_store::Result<Vec<SnapshotEntry>> {
            self.inner.load_snapshot()
        }
        fn deltas_peer_lacks(
            &self,
            peer: &std::collections::BTreeMap<String, DotContext>,
        ) -> orvu_store::Result<Vec<WireDelta>> {
            self.inner.deltas_peer_lacks(peer)
        }
        fn snapshot_contexts(&self) -> orvu_store::Result<std::collections::BTreeMap<String, DotContext>> {
            self.inner.snapshot_contexts()
        }
    }

    fn try_add_one(done: oneshot::Sender<TryOutcome>) -> Command {
        Command::TryMutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _h, r| match crdt {
                TypedCrdt::PnCounter(pn) => Some(TypedDelta::PnCounter(pn.increment(r, 1))),
                _ => None,
            }),
            wall: 10,
            receipt: None,
            done,
        }
    }

    // Under `Always`, a change whose save failed is not reported as done and
    // is not sent. After that the node takes no more changes, even if the
    // disk seems to work again.
    #[tokio::test(flavor = "multi_thread")]
    async fn under_always_a_failed_save_is_an_error_and_stops_changes() {
        for broken in ["append", "flush"] {
            let store = FlakyStore::default();
            let flag = if broken == "append" { &store.fail_append } else { &store.fail_flush };
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let eng = engine_on(Box::new(store.clone()), FsyncPolicy::Always).await;
            let tx = eng.sender();
            let mut outgoing = eng.subscribe();

            assert!(matches!(call(&tx, add_one).await, Err(None)), "{broken}: a failed save was reported as done");
            flag.store(false, std::sync::atomic::Ordering::SeqCst);
            assert!(matches!(call(&tx, add_one).await, Err(None)), "{broken}: a change was taken after saving failed");
            assert_eq!(call(&tx, try_add_one).await, TryOutcome::StorageFailed, "{broken}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(outgoing.try_recv().is_err(), "{broken}: an unsaved change was sent");
        }
    }

    // Under `Always`, a change whose save failed was still made in memory.
    // Nobody may see it: from then on the node answers no reads and serves
    // nothing to other machines, until it is restarted from what is on disk.
    #[tokio::test(flavor = "multi_thread")]
    async fn after_a_failed_save_nothing_is_read_or_served() {
        let store = FlakyStore::default();
        let eng = engine_on(Box::new(store.clone()), FsyncPolicy::Always).await;
        let tx = eng.sender();
        let mut outgoing = eng.subscribe();
        let mut changed = eng.subscribe_changes();
        assert!(call(&tx, add_one).await.is_ok(), "a change while saving works");
        let _ = outgoing.recv().await;
        let _ = changed.recv().await;

        store.fail_flush.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(call(&tx, add_one).await, Err(None)), "a failed save was reported as done");

        // Clients: no copy, no look.
        let copy = call(&tx, |reply| Command::Snapshot { object_id: "c".into(), reply }).await;
        assert!(copy.is_none(), "a read was answered after saving failed: {copy:?}");
        let (seen, looked) = oneshot::channel::<bool>();
        tx.send(Command::Read { object_id: "c".into(), look: Box::new(move |o| { let _ = seen.send(o.is_some()); }) })
            .await
            .ok()
            .unwrap();
        assert!(looked.await.is_err(), "a look was run after saving failed");
        let (reply, opened) = oneshot::channel();
        tx.send(Command::Open { object_id: "d".into(), empty: TypedCrdt::PnCounter(PnCounter::default()), reply })
            .await
            .ok()
            .unwrap();
        assert_ne!(opened.await, Ok(true), "an object was opened after saving failed");

        // Other machines: no digest, no snapshot, no member list.
        let (reply, digest) = oneshot::channel();
        tx.send(Command::Digest { reply }).await.ok().unwrap();
        assert!(digest.await.is_err(), "a digest was given after saving failed");
        let snaps = call(&tx, |reply| Command::SnapshotFor { object_ids: vec!["c".into()], reply }).await;
        assert!(snaps.is_empty(), "a snapshot was given after saving failed");
        let (reply, view) = oneshot::channel();
        tx.send(Command::MembershipView { reply }).await.ok().unwrap();
        assert!(view.await.is_err(), "a member list was given after saving failed");

        // A change from another machine is dropped, not taken in or passed on.
        let mut pn = PnCounter::default();
        let wire = WireDelta {
            object_id: "c".into(),
            dot: Dot::new(ReplicaId(2), 1),
            hlc: Hlc { wall: 1, logical: 0 },
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(pn.increment(ReplicaId(2), 5)),
            trace_id: orvu_core::TraceId::none(),
        };
        call(&tx, |reply| Command::Receive { delta: wire, wall: 1, from: ReplicaId(2), reply }).await;
        // And so is a whole object sent to help this node catch up.
        let mut other = PnCounter::default();
        let state = TypedDelta::PnCounter(other.increment(ReplicaId(3), 4));
        let mut context = DotContext::new();
        context.insert(Dot::new(ReplicaId(3), 1));
        let objects = vec![orvu_core::SnapshotEntry { object_id: "e".into(), state, context }];
        call(&tx, |reply| Command::ReceiveSnapshot { objects, wall: 1, from: ReplicaId(3), reply }).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(outgoing.try_recv().is_err(), "something was sent after saving failed");
        assert!(changed.try_recv().is_err(), "a change was announced after saving failed");
    }

    // Under `Interval` the caller has its answer before the save, so a failed
    // flush cannot be reported to it. The change is still never sent, and
    // every later change is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn under_interval_a_failed_flush_is_never_sent_and_stops_changes() {
        let store = FlakyStore::default();
        store.fail_flush.store(true, std::sync::atomic::Ordering::SeqCst);
        let eng = engine_on(Box::new(store.clone()), FsyncPolicy::Interval { ms: 50 }).await;
        let tx = eng.sender();
        let mut outgoing = eng.subscribe();

        assert!(call(&tx, add_one).await.is_ok(), "answered before the save, as Interval promises");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(outgoing.try_recv().is_err(), "a change that was never saved was sent");
        assert!(matches!(call(&tx, add_one).await, Err(None)), "a change was taken after saving failed");
        assert_eq!(call(&tx, try_add_one).await, TryOutcome::StorageFailed);
    }

    // `Stop` saves everything and answers only once every saved change has
    // been handed to the connections, so a clean shutdown can still send
    // them all.
    // One thread, so the queue of saved changes only empties when the engine
    // lets it run.
    #[tokio::test]
    async fn stop_answers_once_every_saved_change_is_handed_out() {
        const MADE: usize = 600;
        let (persist, _exit) = spawn_writer(Box::new(MemStore::new()), FsyncPolicy::Interval { ms: 3_600_000 });
        let eng = spawn(
            ReplicaId(1),
            64,
            WAIT,
            4 * MADE,
            Persistence { handle: Some(persist), policy: FsyncPolicy::Interval { ms: 3_600_000 }, ..Default::default() },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            mx(),
        );
        let tx = eng.sender();
        let mut outgoing = eng.subscribe();
        call(&tx, |done| Command::Create { object_id: "c".into(), crdt: TypedCrdt::PnCounter(PnCounter::default()), done }).await;
        for _ in 0..MADE {
            assert!(call(&tx, add_one).await.is_ok());
        }
        assert!(outgoing.try_recv().is_err(), "a change left before it was saved");
        let (done, stopped) = oneshot::channel();
        eng.stopper().send(done).await.unwrap();
        stopped.await.unwrap();
        let mut sent = 0;
        while outgoing.try_recv().is_ok() {
            sent += 1;
        }
        assert_eq!(sent, MADE, "Stop answered before every saved change was handed out");
    }

    // `Stop` goes ahead of work still waiting in the queue. A call waiting
    // there gets the same answer as one that comes after the stop: it was
    // not done.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_goes_ahead_of_queued_work() {
        const QUEUED: usize = 100;
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();
        call(&tx, |done| Command::Create { object_id: "c".into(), crdt: TypedCrdt::PnCounter(PnCounter::default()), done }).await;
        // Hold the engine inside a read until the queue is filled.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (inside, entered) = oneshot::channel();
        tx.send(Command::Read {
            object_id: "c".into(),
            look: Box::new(move |_| {
                let _ = inside.send(());
                let _ = held.recv();
            }),
        })
        .await
        .unwrap();
        entered.await.unwrap();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut answers = Vec::new();
        for _ in 0..QUEUED {
            let ran = ran.clone();
            let (done, rx) = oneshot::channel();
            let f: MutateFn = Box::new(move |crdt, _, replica| {
                ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match crdt {
                    TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, 1)),
                    other => panic!("unexpected {other:?}"),
                }
            });
            tx.send(Command::Mutate { object_id: "c".into(), f, wall: 1, receipt: None, done }).await.unwrap();
            answers.push(rx);
        }
        let (done, stopped) = oneshot::channel();
        eng.stopper().send(done).await.unwrap();
        release.send(()).unwrap();
        stopped.await.unwrap();
        for rx in answers {
            assert!(rx.await.is_err(), "a call queued before the stop was answered as done");
        }
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 0, "queued work ran before the stop");
    }

    // A call that goes in just as the engine stops must still be answered,
    // with the answer of a stopped engine. One left in the queue would never
    // be answered, and whoever made it would wait for ever: a connection's
    // reader waiting like that holds the store open, and the node's shutdown
    // then waits for ever too. The caller here has its place in the queue
    // before the stop and puts the call in after it, which is what happens
    // when a caller's thread is held up for a moment at the wrong time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_made_as_the_engine_stops_is_answered() {
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();
        let place = tx.reserve().await.unwrap();
        let (done, stopped) = oneshot::channel();
        eng.stopper().send(done).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (reply, answer) = oneshot::channel();
        place.send(Command::Snapshot { object_id: "c".into(), reply });
        let answered = tokio::time::timeout(std::time::Duration::from_secs(5), answer).await;
        assert!(answered.is_ok(), "a call made as the engine stopped was never answered");
        stopped.await.unwrap();
    }

    // A snapshot sent to help another machine catch up is built from memory,
    // so it must not hold a local change that is not on disk yet.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_catch_up_snapshot_holds_only_saved_changes() {
        let store = MemStore::new();
        let eng = engine_on(Box::new(store.clone()), FsyncPolicy::Interval { ms: 60_000 }).await;
        let tx = eng.sender();
        let made = call(&tx, add_one).await.unwrap().dot;
        let snaps = call(&tx, |reply| Command::SnapshotFor { object_ids: vec!["c".into()], reply }).await;
        assert_eq!(snaps.len(), 1, "no snapshot was handed out");
        let on_disk: Vec<_> = store.reopened().replay().unwrap().iter().map(|d| d.dot).collect();
        assert_eq!(on_disk, vec![made], "the snapshot was handed out before the change was on disk");
        let mut saved = DotContext::new();
        saved.insert(made);
        assert_eq!(snaps[0].context, saved, "the snapshot does not hold exactly the saved change");
        let mut one = PnCounter::default();
        one.increment(ReplicaId(1), 1);
        assert_eq!(snaps[0].state, TypedCrdt::PnCounter(one).snapshot(), "the snapshot's value is not the saved one");
    }

    #[tokio::test]
    async fn crossing_the_threshold_compacts_the_log() {
        let store = MemStore::new();
        let (persist, _exit) = spawn_writer(Box::new(store.clone()), FsyncPolicy::Always);
        let eng = spawn(
            ReplicaId(1),
            64,
            WAIT,
            64,
            Persistence {
                handle: Some(persist),
                policy: FsyncPolicy::Always,
                compaction_threshold: 3,
                ..Default::default()
            },
            Acl::open(),
            TIMEOUT,
            TIMEOUT,
            FORGET,
            mx(),
        );
        let tx = eng.sender();
        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;
        // With no other machines the frontier is empty. The compaction after
        // the 3rd mutate finds a log of 3, which is not over the limit of 3,
        // so it changes nothing. The 6th mutate reaches the threshold again
        // with a log of 6. That is over the limit, so everything is merged
        // into a snapshot.
        for i in 0..6u64 {
            call(&tx, |done| Command::Mutate {
                object_id: "c".into(),
                f: Box::new(move |crdt, _h, _r| match crdt {
                    TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), i + 1)),
                    _ => unreachable!(),
                }),
                wall: 10 + i,
                receipt: None,
                done,
            }).await.unwrap();
        }
        // The compaction runs later on the writer thread, so poll until it shows up.
        let mut snap = Vec::new();
        for _ in 0..100 {
            snap = store.load_snapshot().unwrap();
            if !snap.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(snap.len(), 1, "compaction wrote a snapshot for the one object");
        assert_eq!(snap[0].object_id, "c");
    }

    #[tokio::test]
    async fn receive_snapshot_merges_state_into_the_engine() {
        // Make a snapshot with counter = 5 on a source replica.
        let source = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let stx = source.sender();
        call(&stx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;
        call(&stx, |done| Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _h, _r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
                _ => unreachable!(),
            }),
            wall: 10,
            receipt: None,
            done,
        }).await.unwrap();
        let snap = call(&stx, |reply| Command::Snapshot { object_id: "c".into(), reply }).await;
        let crdt = snap.unwrap();
        let entry = SnapshotEntry {
            object_id: "c".into(),
            state: crdt.snapshot(),
            // A record saying update (1,1) has been seen, which is what the
            // source's digest would report.
            context: {
                let mut ctx = orvu_crdt::DotContext::new();
                ctx.insert(orvu_crdt::Dot::new(ReplicaId(1), 1));
                ctx
            },
        };

        // A new engine receives that snapshot and ends up at 5.
        let eng = spawn(ReplicaId(2), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();
        call(&tx, |reply| Command::ReceiveSnapshot { objects: vec![entry], wall: 1, from: ReplicaId(1), reply }).await;
        let got = call(&tx, |reply| Command::Snapshot { object_id: "c".into(), reply }).await;
        match got {
            Some(TypedCrdt::PnCounter(pn)) => assert_eq!(pn.value(), 5, "state merged from the snapshot"),
            _ => panic!("expected counter = 5"),
        }
    }

    #[tokio::test]
    async fn peer_membership_is_refcounted() {
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();
        // Two connections to machine 2. This happens when two machines each
        // list the other.
        tx.send(Command::PeerUp { replica: ReplicaId(2) }).await.unwrap();
        tx.send(Command::PeerUp { replica: ReplicaId(2) }).await.unwrap();
        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;
        call(&tx, |done| Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _h, _r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 1)),
                _ => unreachable!(),
            }),
            wall: 1,
            receipt: None,
            done,
        }).await.unwrap();

        // One of the two connections drops. Machine 2 is still a member.
        tx.send(Command::PeerDown { replica: ReplicaId(2) }).await.unwrap();

        // Machine 2 reports in a digest that it has seen (1,1). That is only
        // recorded if it is still a member, and then the stable frontier
        // reaches (1,1). If the connection count were wrong and the member was
        // removed on the first PeerDown, the frontier would stay at 0.
        let mut pctx = DotContext::new();
        pctx.insert(orvu_crdt::Dot::new(ReplicaId(1), 1));
        tx.send(Command::ObservePeer {
            from: ReplicaId(2),
            contexts: [("c".to_string(), pctx)].into_iter().collect(),
        }).await.unwrap();
        let f = call(&tx, |reply| Command::StableFrontier { reply }).await;
        assert_eq!(
            f.get("c").map_or(0, |vv| vv.get(ReplicaId(1))), 1,
            "peer 2 still a member after 1 of 2 disconnects; its ack advanced the frontier"
        );
    }

    #[tokio::test]
    async fn observe_peer_advances_the_stable_frontier() {
        // One node with one connected machine. ObservePeer records what that
        // machine has seen, so the stable frontier reaches the update it
        // reported.
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();
        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;
        tx.send(Command::PeerUp { replica: ReplicaId(2) }).await.unwrap();

        let mut pctx = DotContext::new();
        pctx.insert(orvu_crdt::Dot::new(ReplicaId(1), 1));
        tx.send(Command::ObservePeer {
            from: ReplicaId(2),
            contexts: [("c".to_string(), pctx)].into_iter().collect(),
        }).await.unwrap();

        let f = call(&tx, |reply| Command::StableFrontier { reply }).await;
        assert_eq!(f.get("c").map_or(0, |vv| vv.get(ReplicaId(1))), 1);
    }

    #[tokio::test]
    async fn snapshot_for_builds_entries_for_the_requested_objects() {
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();
        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;
        call(&tx, |done| Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _h, _r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 7)),
                _ => unreachable!(),
            }),
            wall: 1,
            receipt: None,
            done,
        }).await.unwrap();

        let snaps = call(&tx, |reply| Command::SnapshotFor {
            object_ids: vec!["c".to_string()],
            reply,
        }).await;
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].object_id, "c");

        // Check that the snapshot includes the change: load it into a new
        // engine and check that the counter is 7.
        let eng2 = spawn(ReplicaId(2), 64, WAIT, 64, Persistence::default(), Acl::open(), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx2 = eng2.sender();
        call(&tx2, |reply| Command::ReceiveSnapshot { objects: snaps, wall: 1, from: ReplicaId(1), reply }).await;
        let got = call(&tx2, |reply| Command::Snapshot { object_id: "c".into(), reply }).await;
        match got {
            Some(TypedCrdt::PnCounter(pn)) => assert_eq!(pn.value(), 7, "snapshot carries the folded mutation"),
            _ => panic!("expected counter = 7 from SnapshotFor"),
        }
    }

    #[tokio::test]
    async fn engine_drops_writes_from_a_peer_without_write_permission() {
        use std::collections::{BTreeMap, BTreeSet};
        // Access list: replica 2 may sync but may not write.
        let mut map = BTreeMap::new();
        map.insert(ReplicaId(2), BTreeSet::from([Action::Sync]));
        let eng = spawn(ReplicaId(1), 64, WAIT, 64, Persistence::default(), Acl::allow_list(map), TIMEOUT, TIMEOUT, FORGET, mx());
        let tx = eng.sender();

        call(&tx, |done| Command::Create {
            object_id: "c".into(),
            crdt: TypedCrdt::PnCounter(PnCounter::default()),
            done,
        }).await;

        // A delta made by replica 2 that arrives on replica 2's connection.
        let mut pn = PnCounter::default();
        let delta = pn.increment(ReplicaId(2), 5);
        let wire = WireDelta {
            object_id: "c".into(),
            dot: Dot::new(ReplicaId(2), 1),
            hlc: Hlc { wall: 1, logical: 0 },
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(delta),
            trace_id: orvu_core::TraceId::none(),
        };
        call(&tx, |reply| Command::Receive { delta: wire, wall: 1, from: ReplicaId(2), reply }).await;

        // It was dropped, so "c" is still zero.
        let got = call(&tx, |reply| Command::Snapshot { object_id: "c".into(), reply }).await;
        match got {
            Some(TypedCrdt::PnCounter(p)) => assert_eq!(p.value(), 0, "unauthorized write must be dropped"),
            _ => panic!("expected a PN counter"),
        }
    }
}
