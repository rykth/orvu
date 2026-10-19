//! The tasks that run for each connected machine: a writer that sends deltas in
//! batches, a reader that handles the messages coming in, and the handshake
//! that starts the connection. A delta is one small change sent to other
//! machines.

use crate::engine::Command;
use crate::persistence::{PersistHandle, StorageHealth};
use orvu_core::{Codec, ProtobufCodec, SyncMessage, WireDelta};
use orvu_crdt::DotContext;
use orvu_transport::frame::{read_frame, write_frame};
use orvu_transport::{Conn, StreamRead, StreamWrite};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

/// Owns the connection's one outgoing stream. Deltas from the broadcast channel
/// are collected and sent as one `DeltaBatch` every `batch` interval. Control
/// messages are written as soon as they arrive. One example is the
/// `ContextDigest` used for catch-up, where machines compare what they have
/// seen and resend what is missing. Nothing else writes to this stream, so two
/// messages can never get mixed together on the wire.
///
/// When nothing at all has been written for `keep_alive`, a `KeepAlive` is
/// sent. The other machine lets a connection go once it has heard nothing on
/// it for a while, and the catch-up summaries that would otherwise keep it
/// busy have to be asked of the engine. An engine can be held up for longer
/// than that, saving to a slow disk or building a large snapshot, and the
/// link is fine all the same. The keep-alive comes from here and waits on
/// nothing, so only a machine whose process has stopped running, or whose
/// link is gone, goes silent.
///
/// `drain` fires on a clean shutdown: what is waiting is sent at once and the
/// writer ends. Once `storage` says saving has failed, nothing more is
/// written: the node shows its data to nobody.
#[allow(clippy::too_many_arguments)]
pub async fn writer_task(
    mut write: StreamWrite,
    mut deltas: broadcast::Receiver<WireDelta>,
    mut control: mpsc::Receiver<SyncMessage>,
    namespace: String,
    batch: Duration,
    keep_alive: Duration,
    cancel: CancellationToken,
    drain: CancellationToken,
    storage: StorageHealth,
) {
    let codec = ProtobufCodec;
    let mut ticker = interval(batch);
    let mut pending: Vec<WireDelta> = Vec::new();
    let mut control_open = true;
    // Fires once nothing has been written for `keep_alive`. Every write
    // starts it over.
    let idle = tokio::time::sleep(keep_alive);
    tokio::pin!(idle);
    let wrote = |idle: &mut std::pin::Pin<&mut tokio::time::Sleep>| {
        idle.as_mut().reset(tokio::time::Instant::now() + keep_alive)
    };
    // Once saving has failed this node shows its data to nobody, so nothing
    // more is written. Whatever arrives is dropped.
    let silent = || storage.failure().is_some();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            // A clean shutdown: send what is waiting for the next batch now,
            // then end. The deltas already handed to this writer are taken
            // too, so nothing made before the shutdown is left behind.
            _ = drain.cancelled() => {
                let missed = take_waiting(&mut deltas, &mut pending);
                if missed > 0 {
                    tracing::warn!(missed, "this connection fell behind and missed changes while shutting down; they are saved and catch-up sends them later");
                }
                if !pending.is_empty() && !silent() {
                    let msg = SyncMessage::DeltaBatch { namespace: namespace.clone(), deltas: std::mem::take(&mut pending) };
                    let _ = write_or_cancel(&mut write, &codec.encode(&msg), &cancel).await;
                }
                break;
            },
            recv = deltas.recv() => match recv {
                Ok(d) => pending.push(d),
                // Lagged: this task fell behind the broadcast buffer and missed
                // some deltas. Catch-up fills the gap, so carry on from here.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "this connection fell behind and missed changes; catch-up fills the gap");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Control messages are written straight away and do not wait for
            // the delta batch. Once the sender is dropped, `if control_open`
            // turns this branch off so a closed channel does not spin the loop.
            ctrl = control.recv(), if control_open => match ctrl {
                Some(_) if silent() => {}
                Some(msg) => {
                    if !write_or_cancel(&mut write, &codec.encode(&msg), &cancel).await {
                        break;
                    }
                    wrote(&mut idle);
                }
                None => control_open = false,
            },
            _ = &mut idle => {
                wrote(&mut idle);
                if silent() { continue; }
                if !write_or_cancel(&mut write, &codec.encode(&SyncMessage::KeepAlive), &cancel).await {
                    break;
                }
            },
            _ = ticker.tick() => {
                if silent() { pending.clear(); }
                if pending.is_empty() { continue; }
                let msg = SyncMessage::DeltaBatch {
                    namespace: namespace.clone(),
                    deltas: std::mem::take(&mut pending),
                };
                if !write_or_cancel(&mut write, &codec.encode(&msg), &cancel).await {
                    break; // the other machine is gone, or we are shutting down
                }
                wrote(&mut idle);
            }
        }
    }
}

/// Takes every delta waiting in the broadcast and puts it in `pending`.
/// Returns how many this writer missed because it fell behind.
///
/// Falling behind loses only the oldest ones. The rest are still there and
/// are taken too. The missed ones are saved on disk, and catch-up brings them
/// to the other machine next time the two are connected.
fn take_waiting(deltas: &mut broadcast::Receiver<WireDelta>, pending: &mut Vec<WireDelta>) -> u64 {
    let mut missed = 0;
    loop {
        match deltas.try_recv() {
            Ok(d) => pending.push(d),
            Err(broadcast::error::TryRecvError::Lagged(n)) => missed += n,
            Err(_) => return missed,
        }
    }
}

/// Writes one frame, giving up if `cancel` fires first. Returns `false` if the
/// write failed or was given up. A machine that has stopped reading can keep a
/// write waiting for ever, and without this the task would never notice a
/// shutdown.
async fn write_or_cancel(write: &mut StreamWrite, bytes: &[u8], cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        done = write_frame(write, bytes) => done.is_ok(),
    }
}

/// Hands a message to this connection's writer, giving up if `cancel` fires
/// first. The writer's queue can stay full for as long as the other machine
/// does not read.
async fn send_or_cancel(control: &mpsc::Sender<SyncMessage>, msg: SyncMessage, cancel: &CancellationToken) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = control.send(msg) => {}
    }
}

/// The time of day in milliseconds since 1970, for stamping a change or a
/// received message. `last` remembers the highest value handed out so far, so
/// the result never goes backwards even if the system clock is set back.
pub(crate) fn now_wall(last: &std::sync::atomic::AtomicU64) -> u64 {
    use std::sync::atomic::Ordering;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    last.fetch_max(now, Ordering::Relaxed).max(now)
}

/// The most `Nack`s one connection sends in a second. A machine that sends a
/// flood of bad changes gets this many and no more; the rest are counted in
/// `nacks_held_back`. A few are enough to say which changes are bad and why.
pub const NACKS_PER_SECOND: u32 = 10;

/// Reads messages from another machine, decodes them, and passes them to the
/// engine. For a `DeltaBatch`, each delta is applied. A `ContextDigest` says
/// what the other machine has seen. This task asks the store which deltas that
/// machine is missing and sends them back through this connection's writer. If
/// it is too far behind for that, the engine builds a snapshot (the whole
/// current state of an object) to send as well. A `SnapshotBatch` is merged in.
/// `Hello` and `HelloAck` were already handled by `handshake`.
///
/// If the other machine sends nothing at all for `quiet` while this task is
/// waiting for it, the task ends and returns `true`. A machine whose process
/// has stopped running, or whose link is gone, does not close the connection,
/// it just goes silent, and without this the task would wait for it for
/// ever. A healthy machine writes something at least about once an
/// anti-entropy interval, a `KeepAlive` when it has nothing else to say, even
/// while its engine is busy, so `quiet` should be a few of those. Time spent
/// here handling a message does not count: only time spent waiting for the
/// other machine does. A `KeepAlive` needs no handling, and a message of a
/// kind this version does not know is skipped; both still count as heard.
#[allow(clippy::too_many_arguments)]
pub async fn reader_task(
    read: StreamRead,
    engine: mpsc::Sender<Command>,
    replica: orvu_crdt::ReplicaId,
    control: mpsc::Sender<SyncMessage>,
    wall: std::sync::Arc<std::sync::atomic::AtomicU64>,
    cancel: CancellationToken,
    persist: PersistHandle,
    metrics: std::sync::Arc<crate::metrics::Metrics>,
    acl: crate::config::Acl,
    quiet: Duration,
) -> bool {
    let codec = ProtobufCodec;
    let mut read = QuietLimit::new(read, quiet);
    let mut nacks = NackLimit::new();
    let mut nack_logs = NackLimit::new();
    loop {
        // The silence is counted from now, not from the last message, so the
        // time spent handling that message is not held against the other
        // machine.
        read.restart();
        tokio::select! {
            _ = cancel.cancelled() => break,
            frame = read_frame(&mut read) => {
                let bytes = match frame {
                    Ok(b) => b,
                    Err(_) if read.went_quiet() => {
                        tracing::info!(peer = replica.0, ?quiet, "a connected machine sent nothing for too long; letting it go");
                        return true;
                    }
                    Err(_) => break,
                };
                let msg = match codec.decode(&bytes) {
                    Ok(m) => m,
                    // A batch with a change in it that cannot be read is
                    // dropped whole. The Nack names that change.
                    Err(_) => {
                        if let Some(nack) = orvu_core::nack_for_unreadable(&bytes) {
                            nacks.send(&control, &metrics, nack);
                        }
                        continue;
                    }
                };
                match msg {
                    SyncMessage::DeltaBatch { deltas, .. } => {
                        for d in deltas {
                            let (object_id, dot) = (d.object_id.clone(), d.dot);
                            let (reply, done) = oneshot::channel();
                            if engine.send(Command::Receive { delta: d, wall: now_wall(&wall), from: replica, reply }).await.is_err() {
                                return false;
                            }
                            if let Ok(Some(refused)) = done.await {
                                let nack = SyncMessage::Nack {
                                    reason: refused.reason,
                                    detail: refused.detail,
                                    object_id,
                                    dot: Some(dot),
                                };
                                nacks.send(&control, &metrics, nack);
                            }
                        }
                    }
                    // The other machine refused something this node sent.
                    // There is nothing to do about it here, but someone
                    // should be able to see it: it is counted, and logged
                    // a few times a second at most.
                    SyncMessage::Nack { reason, detail, object_id, dot } => {
                        metrics.record_nack_received();
                        if nack_logs.allow() {
                            tracing::warn!(
                                peer = replica.0,
                                ?reason,
                                object = %object_id,
                                ?dot,
                                %detail,
                                "the other machine refused something this node sent"
                            );
                        }
                    }
                    // Catch-up is done here in the reader. The store is queried
                    // on this task, not on the engine loop, and the missing
                    // deltas go out through our own writer. The engine is only
                    // asked to record what the other machine has seen and to
                    // build a snapshot when one is needed.
                    SyncMessage::ContextDigest { namespace, entries } => {
                        let peer_ctxs: BTreeMap<String, DotContext> = entries.into_iter().collect();
                        let reads = persist.query(peer_ctxs.clone()).await;

                        // Objects that the remaining log cannot repair: the
                        // stored snapshot covers updates the other machine has
                        // not seen.
                        let empty = DotContext::new();
                        let mut snapshot_ids: Vec<String> = Vec::new();
                        for (obj, fctx) in &reads.floor {
                            let pctx = peer_ctxs.get(obj.as_str()).unwrap_or(&empty);
                            if !pctx.covers(fctx) {
                                snapshot_ids.push(obj.clone());
                            }
                        }

                        // The deltas from the log that the other machine is
                        // missing, leaving out the objects that get a snapshot.
                        // They go to that machine only, through our own
                        // writer's control channel.
                        let mut deltas = reads.deltas;
                        deltas.retain(|d| !snapshot_ids.contains(&d.object_id));
                        if !deltas.is_empty() {
                            metrics.add_repaired_deltas(deltas.len() as u64);
                            let msg = SyncMessage::DeltaBatch { namespace: namespace.clone(), deltas };
                            send_or_cancel(&control, msg, &cancel).await;
                        }

                        // Too far behind: ask the engine for snapshots of its
                        // current state.
                        if !snapshot_ids.is_empty() {
                            let (reply, done) = oneshot::channel();
                            if engine.send(Command::SnapshotFor { object_ids: snapshot_ids, reply }).await.is_err() {
                                return false;
                            }
                            if let Ok(objects) = done.await {
                                if !objects.is_empty() {
                                    metrics.record_snapshot_sent();
                                    let msg = SyncMessage::SnapshotBatch { namespace, objects };
                                    metrics.snapshot_bytes.observe(codec.encode(&msg).len() as f64);
                                    send_or_cancel(&control, msg, &cancel).await;
                                }
                            }
                        }

                        // Tell the engine what the other machine has seen. We
                        // do not wait for an answer, and it is fine if that
                        // machine has already left.
                        let _ = engine.send(Command::ObservePeer { from: replica, contexts: peer_ctxs }).await;
                    }
                    // Another machine sent whole-object state for catch-up.
                    // Merge it in.
                    SyncMessage::SnapshotBatch { objects, .. } => {
                        let (reply, done) = oneshot::channel();
                        if engine.send(Command::ReceiveSnapshot { objects, wall: now_wall(&wall), from: replica, reply }).await.is_err() {
                            return false;
                        }
                        let _ = done.await;
                    }
                    // Membership gossip: merge the other machine's list of
                    // members. Rows about ourselves are skipped and the newest
                    // heartbeat wins. This lets the record of what every member
                    // has seen include members this node never connects to.
                    SyncMessage::MembershipDigest { members } => {
                        let _ = engine.send(Command::ObserveMembers { members }).await;
                    }
                    // A borrow request for a bounded counter, which is a
                    // counter that cannot go below zero and where each machine
                    // has an allowance. The other machine asks for some of
                    // ours. We transfer it from our own allowance, or refuse if
                    // we do not have enough. The transfer goes out as a normal
                    // delta and we do not wait for the result.
                    //
                    // A machine may only ask for itself. `from` is what the
                    // message says and `replica` is who the connection really
                    // belongs to, so a request naming anyone else is dropped.
                    // Lending is a write made for the asker, so a machine that
                    // may not write gets nothing either.
                    SyncMessage::BorrowRequest { object_id, amount, from, .. } => {
                        if from != replica || !acl.permits(replica, crate::config::Action::Write) {
                            tracing::warn!(from = replica.0, claimed = from.0, object = %object_id, "authz: dropping a borrow request");
                            continue;
                        }
                        let (done, _rx) = oneshot::channel();
                        let f: crate::engine::TryMutateFn = Box::new(move |crdt, _hlc, me| match crdt {
                            orvu_core::TypedCrdt::BoundedCounter(b) => {
                                b.transfer(from, amount, me).map(orvu_core::TypedDelta::BoundedCounter)
                            }
                            _ => None,
                        });
                        let _ = engine
                            .send(Command::TryMutate { object_id, f, wall: now_wall(&wall), receipt: None, done })
                            .await;
                    }
                    _ => { /* Hello, HelloAck, Ack and KeepAlive are ignored once the connection is running */ }
                }
            }
        }
    }
    false
}

/// Keeps one connection to `NACKS_PER_SECOND` in any one second, for the
/// `Nack`s it sends and for the ones it logs.
struct NackLimit {
    since: tokio::time::Instant,
    used: u32,
}

impl NackLimit {
    fn new() -> Self {
        Self { since: tokio::time::Instant::now(), used: 0 }
    }

    /// Whether one more fits in the current second.
    fn allow(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        if now.duration_since(self.since) >= Duration::from_secs(1) {
            self.since = now;
            self.used = 0;
        }
        if self.used < NACKS_PER_SECOND {
            self.used += 1;
            true
        } else {
            false
        }
    }

    /// Hands `nack` to this connection's writer if the limit allows and the
    /// writer has room, and counts it as sent or held back. It never waits:
    /// a machine that sends bad changes and does not read must not hold up
    /// this reader.
    fn send(&mut self, control: &mpsc::Sender<SyncMessage>, metrics: &crate::metrics::Metrics, nack: SyncMessage) {
        if self.allow() && control.try_send(nack).is_ok() {
            metrics.record_nack_sent();
        } else {
            metrics.record_nack_held_back();
        }
    }
}

/// A read half that fails once the other machine has sent nothing for
/// `limit`. Each byte that arrives starts the time over, so a large message
/// that arrives slowly but steadily is not cut off.
struct QuietLimit {
    inner: StreamRead,
    limit: Duration,
    deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
    quiet: bool,
}

impl QuietLimit {
    fn new(inner: StreamRead, limit: Duration) -> Self {
        Self { inner, limit, deadline: Box::pin(tokio::time::sleep(limit)), quiet: false }
    }

    /// Starts counting the silence over, from now.
    fn restart(&mut self) {
        self.deadline.as_mut().reset(tokio::time::Instant::now() + self.limit);
    }

    /// Whether the last read failed because the other machine was silent.
    fn went_quiet(&self) -> bool {
        self.quiet
    }
}

impl tokio::io::AsyncRead for QuietLimit {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::future::Future;
        use std::task::Poll;
        let this = &mut *self;
        let before = buf.filled().len();
        match std::pin::Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(done) => {
                if buf.filled().len() > before {
                    this.restart();
                }
                Poll::Ready(done)
            }
            Poll::Pending => match this.deadline.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    this.quiet = true;
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "the other machine sent nothing for too long",
                    )))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// The longest one handshake may take. It covers setting up the stream and two
/// small messages, which take milliseconds on a healthy link. Ten seconds
/// leaves room for a busy machine or a slow long-distance link. It also limits
/// how long a connection that stalls, or sends its bytes very slowly, can keep
/// a place on the accepting side. With no limit, one party that never finishes
/// its hello would block whatever is waiting on it forever.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The identities that have a handshake in progress on the accepting side. Each
/// identity may have at most one. A second connection for an identity that is
/// still handshaking is dropped at accept and not queued, so no party can use
/// up another party's place.
///
/// This only limits a flood from one party. Someone who can create many valid
/// identities still gets one place for each. There is no rate limit per source
/// here.
pub(crate) type InflightHandshakes = Arc<Mutex<BTreeSet<u64>>>;

/// A claim on one identity's handshake place. Dropping it gives the place back
/// on every way out: success, timeout, failure or panic. A place that was never
/// given back would lock that machine out for good, which is worse than the
/// flood this guard protects against.
pub(crate) struct HandshakeSlot {
    id: u64,
    set: InflightHandshakes,
}

impl HandshakeSlot {
    pub(crate) fn claim(set: &InflightHandshakes, id: u64) -> Option<Self> {
        set.lock()
            .unwrap()
            .insert(id)
            .then(|| Self { id, set: set.clone() })
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

/// The replica id (the number that names a machine) that the transport has
/// verified for this connection, or `None` if the connection carries no
/// identity that can be read as one.
///
/// `bind_identity` does the same check after the hello. Having it here lets the
/// accept loop turn away a connection with no identity before any I/O. Anything
/// dropped here would have been dropped after the hello anyway.
pub(crate) fn verified_identity(conn: &dyn Conn) -> Option<u64> {
    conn.identity()
        .uri
        .as_deref()
        .and_then(orvu_transport::types::parse_replica_identity_uri)
}

/// Exchanges `Hello` and `HelloAck` on a new stream. Returns the stream and the
/// other machine's replica id.
///
/// Each side names its namespace, and the handshake fails unless both name the
/// same one. Two machines set up for different namespaces hold different data
/// under the same object ids, so they must not sync.
///
/// Both sides must also speak the same protocol version, the one this build
/// speaks (`orvu_proto::PROTO_VERSION`). See [`handshake_at`].
///
/// The whole exchange must finish within `HANDSHAKE_TIMEOUT`. That stops a
/// party that opens a connection and then stalls, or sends bytes slowly enough
/// to get past an idle timer, from holding it forever. Callers cannot tell a
/// timeout from a failed handshake. Both return `None` and the connection is
/// dropped.
pub async fn handshake(
    conn: &dyn Conn,
    me: orvu_crdt::ReplicaId,
    namespace: &str,
    initiator: bool,
    acl: &crate::config::Acl,
) -> Option<(orvu_transport::StreamWrite, orvu_transport::StreamRead, u64)> {
    handshake_at(conn, me, namespace, initiator, acl, orvu_proto::PROTO_VERSION).await
}

/// The same as [`handshake`], speaking protocol version `version`.
///
/// A machine on another version is refused. The same bytes can mean something
/// else to it, so syncing with it could quietly damage data on both sides. The
/// side that finds the difference sends a `Nack` with reason `PROTO_VERSION`
/// that names both numbers, logs a warning with both, and drops the
/// connection. The side that gets the `Nack` logs it too. A machine from
/// before this check does not compare numbers, but the newer side still does,
/// whichever side dialed.
pub async fn handshake_at(
    conn: &dyn Conn,
    me: orvu_crdt::ReplicaId,
    namespace: &str,
    initiator: bool,
    acl: &crate::config::Acl,
    version: u32,
) -> Option<(orvu_transport::StreamWrite, orvu_transport::StreamRead, u64)> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake_inner(conn, me, namespace, initiator, acl, version),
    )
    .await
    .ok()
    .flatten()
}

async fn handshake_inner(
    conn: &dyn Conn,
    me: orvu_crdt::ReplicaId,
    namespace: &str,
    initiator: bool,
    acl: &crate::config::Acl,
    version: u32,
) -> Option<(orvu_transport::StreamWrite, orvu_transport::StreamRead, u64)> {
    let codec = ProtobufCodec;
    let (mut w, mut r) = if initiator {
        conn.open_stream().await.ok()?
    } else {
        conn.accept_stream().await.ok()?
    };

    let hello = SyncMessage::Hello {
        proto_version: version,
        replica: me,
        namespaces: vec![namespace.to_string()],
    };
    if initiator {
        write_frame(&mut w, &codec.encode(&hello)).await.ok()?;
        let peer = codec.decode(&read_frame(&mut r).await.ok()?).ok()?;
        let (peer_id, theirs) = match peer {
            SyncMessage::HelloAck { replica, accepted, proto_version } if accepted.iter().any(|n| n == namespace) => {
                (replica.0, proto_version)
            }
            SyncMessage::Nack { reason, detail, .. } => {
                tracing::warn!(
                    me = me.0,
                    peer = verified_identity(conn),
                    ours = version,
                    ?reason,
                    %detail,
                    "the other machine refused to connect"
                );
                return None;
            }
            _ => return None,
        };
        bind_identity(conn, peer_id)?;
        if theirs != version {
            refuse_version(&mut w, &mut r, me, peer_id, version, theirs).await;
            return None;
        }
        if !acl.permits(orvu_crdt::ReplicaId(peer_id), crate::config::Action::Sync) {
            return None;
        }
        Some((w, r, peer_id))
    } else {
        let peer = codec.decode(&read_frame(&mut r).await.ok()?).ok()?;
        let SyncMessage::Hello { replica, namespaces, proto_version: theirs } = peer else {
            return None;
        };
        let peer_id = replica.0;
        // The identity is checked first, so only a machine that is who it
        // says it is learns anything from the answer.
        bind_identity(conn, peer_id)?;
        if theirs != version {
            refuse_version(&mut w, &mut r, me, peer_id, version, theirs).await;
            return None;
        }
        if !namespaces.iter().any(|n| n == namespace) {
            return None;
        }
        if !acl.permits(orvu_crdt::ReplicaId(peer_id), crate::config::Action::Sync) {
            return None;
        }
        let ack = SyncMessage::HelloAck {
            proto_version: version,
            replica: me,
            accepted: vec![namespace.to_string()],
        };
        write_frame(&mut w, &codec.encode(&ack)).await.ok()?;
        Some((w, r, peer_id))
    }
}

/// How long a node that refused a machine for its version waits for that
/// machine to read the `Nack` and hang up, before it drops the connection
/// itself. Dropping it at once can throw away the `Nack` before it is sent.
const LINGER_AFTER_REFUSAL: Duration = Duration::from_secs(1);

/// Tells a machine on protocol version `theirs` that this one speaks `ours`
/// and will not sync with it, and logs both numbers. The `Nack` goes out,
/// the stream is closed, and this waits a moment for the other machine to
/// hang up, so the `Nack` is not lost when the connection is dropped.
async fn refuse_version(
    w: &mut orvu_transport::StreamWrite,
    r: &mut orvu_transport::StreamRead,
    me: orvu_crdt::ReplicaId,
    peer: u64,
    ours: u32,
    theirs: u32,
) {
    use tokio::io::AsyncWriteExt;
    tracing::warn!(
        me = me.0,
        peer,
        ours,
        theirs,
        "refusing a machine that speaks another protocol version. Machines sync only with the same version, \
         so run the same Orvu version on both"
    );
    let nack = SyncMessage::Nack {
        reason: orvu_core::NackReason::Known(orvu_proto::v1::NackReason::ProtoVersion),
        detail: format!("this machine speaks protocol version {ours}, and yours is version {theirs}"),
        object_id: String::new(),
        dot: None,
    };
    if write_frame(w, &ProtobufCodec.encode(&nack)).await.is_err() {
        return;
    }
    let _ = w.shutdown().await;
    let _ = tokio::time::timeout(LINGER_AFTER_REFUSAL, async {
        while read_frame(r).await.is_ok() {}
    })
    .await;
}

/// Checks that the identity the transport verified matches the replica id the
/// other machine claimed. The connection is rejected if it has no identity, if
/// the identity cannot be read, or if it names a different replica.
fn bind_identity(conn: &dyn Conn, peer_id: u64) -> Option<()> {
    let verified = conn
        .identity()
        .uri
        .as_deref()
        .and_then(orvu_transport::types::parse_replica_identity_uri);
    match verified {
        Some(id) if id == peer_id => Some(()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_core::{TypedDelta, WireDelta};
    use orvu_crdt::{Dot, DotContext, Hlc, PnCounter, ReplicaId};
    use orvu_proto::Message;
    use orvu_transport::frame::read_frame;

    fn sample() -> WireDelta {
        let mut c = PnCounter::default();
        WireDelta {
            object_id: "c".into(),
            dot: Dot::new(ReplicaId(1), 1),
            hlc: Hlc::ZERO,
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(c.increment(ReplicaId(1), 7)),
            trace_id: orvu_core::TraceId::none(),
        }
    }

    #[tokio::test]
    async fn writer_batches_deltas_into_a_frame() {
        let (w, r) = tokio::io::duplex(64 * 1024);
        let (_wr, wh) = tokio::io::split(w);       // keep the write half of `w`
        let (mut rh, _rw) = tokio::io::split(r);   // keep the read half of `r`

        let (tx, rx) = broadcast::channel(16);
        let (_control_tx, control_rx) = mpsc::channel(16); // held open, unused here
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(writer_task(
            Box::new(wh),
            rx,
            control_rx,
            "default".into(),
            Duration::from_millis(5),
            Duration::from_secs(3600),
            cancel.clone(),
            CancellationToken::new(),
            StorageHealth::default(),
        ));

        tx.send(sample()).unwrap();

        let bytes = read_frame(&mut rh).await.unwrap();
        match ProtobufCodec.decode(&bytes).unwrap() {
            SyncMessage::DeltaBatch { deltas, .. } => assert_eq!(deltas.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
        cancel.cancel();
        let _ = handle.await;
    }

    // On a clean shutdown the writer is told to drain: what is waiting for
    // the next batch goes out now, and then the writer ends.
    #[tokio::test]
    async fn a_drained_writer_sends_what_is_waiting_and_ends() {
        let (w, r) = tokio::io::duplex(64 * 1024);
        let (_wr, wh) = tokio::io::split(w);
        let (mut rh, _rw) = tokio::io::split(r);
        let (tx, rx) = broadcast::channel(16);
        let (_control_tx, control_rx) = mpsc::channel(16);
        let drain = CancellationToken::new();
        let handle = tokio::spawn(writer_task(
            Box::new(wh),
            rx,
            control_rx,
            "default".into(),
            Duration::from_secs(3600), // the next batch would be an hour away
            Duration::from_secs(3600),
            CancellationToken::new(),
            drain.clone(),
            StorageHealth::default(),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await; // past the first tick
        tx.send(sample()).unwrap();
        drain.cancel();

        let bytes = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut rh))
            .await
            .expect("the waiting delta was not sent on drain")
            .unwrap();
        match ProtobufCodec.decode(&bytes).unwrap() {
            SyncMessage::DeltaBatch { deltas, .. } => assert_eq!(deltas.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
        tokio::time::timeout(Duration::from_secs(2), handle).await.expect("the writer did not end").unwrap();
    }

    // A writer that fell behind the broadcast has missed some deltas. On
    // drain it still takes every one that is left, not none of them.
    #[test]
    fn draining_after_falling_behind_takes_what_is_left() {
        let (tx, mut rx) = broadcast::channel(4);
        // Ten deltas into four places: the first six are gone, four are left.
        for _ in 0..10 {
            tx.send(sample()).unwrap();
        }
        let mut pending = Vec::new();
        let missed = take_waiting(&mut rx, &mut pending);
        assert_eq!((pending.len(), missed), (4, 6));
    }

    // The silence is only counted while the reader waits for the other
    // machine. Here the engine takes three times the limit to apply the first
    // message, and the next one arrives a moment after that. The other machine
    // was never silent for long, so the reader must take the second message
    // and not give up on it.
    #[tokio::test]
    async fn time_spent_handling_a_message_is_not_counted_as_silence() {
        let quiet = Duration::from_millis(100);
        let (w, r) = tokio::io::duplex(64 * 1024);
        let (_wr, mut wh) = tokio::io::split(w);
        let (rh, _rw) = tokio::io::split(r);
        let (engine, mut commands) = mpsc::channel::<Command>(8);
        let (control, _control_rx) = mpsc::channel(8);
        let (persist, _exit) = crate::persistence::spawn_writer(
            Box::new(orvu_store::MemStore::new()),
            orvu_store::FsyncPolicy::default(),
        );
        let reader = tokio::spawn(reader_task(
            Box::new(rh),
            engine,
            orvu_crdt::ReplicaId(2),
            control,
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
            CancellationToken::new(),
            persist,
            Arc::new(crate::metrics::Metrics::default()),
            crate::config::Acl::open(),
            quiet,
        ));
        let batch = || {
            ProtobufCodec.encode(&SyncMessage::DeltaBatch { namespace: "default".into(), deltas: vec![sample()] })
        };

        write_frame(&mut wh, &batch()).await.unwrap();
        let Some(Command::Receive { reply, .. }) = commands.recv().await else { panic!("no delta") };
        tokio::time::sleep(quiet * 3).await;
        let _ = reply.send(None);
        tokio::time::sleep(quiet / 3).await;
        write_frame(&mut wh, &batch()).await.unwrap();

        let second = tokio::time::timeout(Duration::from_secs(2), commands.recv()).await;
        assert!(
            matches!(second, Ok(Some(Command::Receive { .. }))),
            "the reader gave up on a machine that was not silent"
        );
        reader.abort();
    }

    /// A reader for machine 2 with a stand-in engine. Returns the frames
    /// side to write to it, the commands it gives the engine, what it hands
    /// its own writer, and its metrics.
    #[allow(clippy::type_complexity)]
    fn reader_for_tests(
        control_room: usize,
    ) -> (
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
        mpsc::Receiver<Command>,
        mpsc::Receiver<SyncMessage>,
        Arc<crate::metrics::Metrics>,
        tokio::task::JoinHandle<bool>,
    ) {
        let (w, r) = tokio::io::duplex(1024 * 1024);
        let (_wr, wh) = tokio::io::split(w);
        let (rh, _rw) = tokio::io::split(r);
        let (engine, commands) = mpsc::channel::<Command>(8);
        let (control, control_rx) = mpsc::channel(control_room);
        let (persist, _exit) = crate::persistence::spawn_writer(
            Box::new(orvu_store::MemStore::new()),
            orvu_store::FsyncPolicy::default(),
        );
        let metrics = Arc::new(crate::metrics::Metrics::default());
        let reader = tokio::spawn(reader_task(
            Box::new(rh),
            engine,
            orvu_crdt::ReplicaId(2),
            control,
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
            CancellationToken::new(),
            persist,
            metrics.clone(),
            crate::config::Acl::open(),
            Duration::from_secs(3600),
        ));
        (wh, commands, control_rx, metrics, reader)
    }

    fn malformed() -> Option<crate::engine::Refused> {
        Some(crate::engine::Refused {
            reason: orvu_core::NackReason::Known(orvu_proto::v1::NackReason::MalformedPayload),
            detail: "skips ahead".into(),
        })
    }

    // A change the engine refuses is answered with a Nack that names the
    // object and the dot of that change.
    #[tokio::test]
    async fn a_refused_change_is_answered_with_a_nack_naming_it() {
        let (mut wh, mut commands, mut control, metrics, reader) = reader_for_tests(16);
        let batch = SyncMessage::DeltaBatch { namespace: "default".into(), deltas: vec![sample()] };
        write_frame(&mut wh, &ProtobufCodec.encode(&batch)).await.unwrap();
        let Some(Command::Receive { reply, .. }) = commands.recv().await else { panic!("no delta") };
        let _ = reply.send(malformed());

        let sent = tokio::time::timeout(Duration::from_secs(2), control.recv()).await;
        match sent {
            Ok(Some(SyncMessage::Nack { reason, object_id, dot, .. })) => {
                assert_eq!(reason, orvu_core::NackReason::Known(orvu_proto::v1::NackReason::MalformedPayload));
                assert_eq!(object_id, "c");
                assert_eq!(dot, Some(Dot::new(ReplicaId(1), 1)));
            }
            other => panic!("expected a Nack, got {other:?}"),
        }
        assert_eq!(metrics.snapshot().nacks_sent, 1);
        reader.abort();
    }

    // A machine that sends a flood of bad changes gets a few Nacks, not one
    // for each. The rest are counted as held back.
    #[tokio::test]
    async fn a_flood_of_refused_changes_gets_only_a_few_nacks() {
        let (mut wh, mut commands, mut control, metrics, reader) = reader_for_tests(1024);
        let batch = SyncMessage::DeltaBatch { namespace: "default".into(), deltas: vec![sample(); 200] };
        write_frame(&mut wh, &ProtobufCodec.encode(&batch)).await.unwrap();
        for _ in 0..200 {
            let Some(Command::Receive { reply, .. }) = commands.recv().await else { panic!("no delta") };
            let _ = reply.send(malformed());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut nacks = 0;
        while let Ok(msg) = control.try_recv() {
            assert!(matches!(msg, SyncMessage::Nack { .. }), "unexpected {msg:?}");
            nacks += 1;
        }
        assert!(nacks >= 1, "no Nack at all");
        assert!(nacks <= NACKS_PER_SECOND as u64, "{nacks} Nacks for one flood");
        let m = metrics.snapshot();
        assert_eq!(m.nacks_sent, nacks);
        assert_eq!(m.nacks_held_back, 200 - nacks);
        reader.abort();
    }

    // A batch that cannot be read at all, because one change in it is
    // broken, is answered with a Nack naming that change.
    #[tokio::test]
    async fn an_unreadable_batch_is_answered_with_a_nack_naming_the_broken_change() {
        let (mut wh, _commands, mut control, _metrics, reader) = reader_for_tests(16);
        let mut bytes = Vec::new();
        orvu_proto::v1::SyncMessage {
            payload: Some(orvu_proto::v1::sync_message::Payload::DeltaBatch(orvu_proto::v1::DeltaBatch {
                namespace: "default".into(),
                deltas: vec![orvu_proto::v1::Delta {
                    object_id: "broken".into(),
                    crdt_type: orvu_proto::v1::CrdtType::PnCounter as i32,
                    dot: Some(orvu_proto::v1::Dot { replica: 6, counter: 2 }),
                    hlc: Some(orvu_proto::v1::Hlc { wall: 1, logical: 0 }),
                    deps: Some(orvu_proto::v1::DotContext::default()),
                    payload: vec![0xff, 0xff, 0xff],
                    trace_id: Vec::new(),
                }],
            })),
        }
        .encode_length_delimited(&mut bytes)
        .unwrap();
        write_frame(&mut wh, &bytes).await.unwrap();
        let sent = tokio::time::timeout(Duration::from_secs(2), control.recv()).await;
        assert!(
            matches!(&sent, Ok(Some(SyncMessage::Nack { object_id, dot: Some(d), .. })) if object_id == "broken" && *d == Dot::new(ReplicaId(6), 2)),
            "got {sent:?}"
        );
        reader.abort();
    }

    // A Nack from the other machine is counted, and the connection goes on.
    #[tokio::test]
    async fn a_received_nack_is_counted() {
        let (mut wh, mut commands, _control, metrics, reader) = reader_for_tests(16);
        let nack = SyncMessage::Nack {
            reason: orvu_core::NackReason::Known(orvu_proto::v1::NackReason::MalformedPayload),
            detail: "skips ahead".into(),
            object_id: "c".into(),
            dot: Some(Dot::new(ReplicaId(1), 1)),
        };
        for _ in 0..3 {
            write_frame(&mut wh, &ProtobufCodec.encode(&nack)).await.unwrap();
        }
        let batch = SyncMessage::DeltaBatch { namespace: "default".into(), deltas: vec![sample()] };
        write_frame(&mut wh, &ProtobufCodec.encode(&batch)).await.unwrap();
        let next = tokio::time::timeout(Duration::from_secs(2), commands.recv()).await;
        assert!(matches!(next, Ok(Some(Command::Receive { .. }))), "the reader stopped after a Nack");
        assert_eq!(metrics.snapshot().nacks_received, 3);
        reader.abort();
    }

    // A connection with nothing to carry still sends something about once
    // a keep-alive period, without asking the engine for anything, so the
    // other end does not take it for a machine that has stopped.
    #[tokio::test]
    async fn an_idle_writer_sends_a_keep_alive() {
        let (w, r) = tokio::io::duplex(64 * 1024);
        let (_wr, wh) = tokio::io::split(w);
        let (mut rh, _rw) = tokio::io::split(r);
        let (_tx, rx) = broadcast::channel::<WireDelta>(16);
        let (_control_tx, control_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(writer_task(
            Box::new(wh),
            rx,
            control_rx,
            "default".into(),
            Duration::from_millis(5),
            Duration::from_millis(50),
            cancel.clone(),
            CancellationToken::new(),
            StorageHealth::default(),
        ));
        for _ in 0..3 {
            let bytes = tokio::time::timeout(Duration::from_millis(500), read_frame(&mut rh))
                .await
                .expect("an idle writer sent nothing")
                .unwrap();
            assert_eq!(ProtobufCodec.decode(&bytes).unwrap(), SyncMessage::KeepAlive);
        }
        cancel.cancel();
        let _ = handle.await;
    }

    // A writer that keeps sending real messages needs no keep-alive between
    // them. The clock is the test's own, moved on only when every task is
    // waiting, so a stalled thread cannot make a gap between two changes
    // look longer than the keep-alive time.
    #[tokio::test(start_paused = true)]
    async fn a_busy_writer_sends_no_keep_alive() {
        let (w, r) = tokio::io::duplex(64 * 1024);
        let (_wr, wh) = tokio::io::split(w);
        let (mut rh, _rw) = tokio::io::split(r);
        let (tx, rx) = broadcast::channel(16);
        let (_control_tx, control_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(writer_task(
            Box::new(wh),
            rx,
            control_rx,
            "default".into(),
            Duration::from_millis(5),
            Duration::from_millis(100),
            cancel.clone(),
            CancellationToken::new(),
            StorageHealth::default(),
        ));
        let feed = tokio::spawn(async move {
            for _ in 0..20 {
                let _ = tx.send(sample());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        for _ in 0..10 {
            let bytes = read_frame(&mut rh).await.unwrap();
            let msg = ProtobufCodec.decode(&bytes).unwrap();
            assert!(matches!(msg, SyncMessage::DeltaBatch { .. }), "unexpected {msg:?}");
        }
        feed.abort();
        cancel.cancel();
        let _ = handle.await;
    }

    // An older machine skips a message of a kind it does not know, such as a
    // keep-alive from a newer machine, and keeps the connection. The bytes
    // still count as something heard.
    #[tokio::test]
    async fn a_message_of_an_unknown_kind_is_skipped_and_counts_as_heard() {
        let quiet = Duration::from_millis(150);
        let (w, r) = tokio::io::duplex(64 * 1024);
        let (_wr, mut wh) = tokio::io::split(w);
        let (rh, _rw) = tokio::io::split(r);
        let (engine, mut commands) = mpsc::channel::<Command>(8);
        let (control, _control_rx) = mpsc::channel(8);
        let (persist, _exit) = crate::persistence::spawn_writer(
            Box::new(orvu_store::MemStore::new()),
            orvu_store::FsyncPolicy::default(),
        );
        let reader = tokio::spawn(reader_task(
            Box::new(rh),
            engine,
            orvu_crdt::ReplicaId(2),
            control,
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
            CancellationToken::new(),
            persist,
            Arc::new(crate::metrics::Metrics::default()),
            crate::config::Acl::open(),
            quiet,
        ));
        // A wrapper whose only field is one this version has no name for.
        let unknown = [2u8, (15 << 3) | 2, 0];
        for _ in 0..6 {
            write_frame(&mut wh, &unknown).await.unwrap();
            tokio::time::sleep(quiet / 3).await;
        }
        assert!(!reader.is_finished(), "the reader gave up on a machine that kept sending");
        let batch = ProtobufCodec.encode(&SyncMessage::DeltaBatch { namespace: "default".into(), deltas: vec![sample()] });
        write_frame(&mut wh, &batch).await.unwrap();
        let next = tokio::time::timeout(Duration::from_secs(2), commands.recv()).await;
        assert!(matches!(next, Ok(Some(Command::Receive { .. }))), "the reader stopped after an unknown message");
        reader.abort();
    }

    // Once saving has failed the node sends nothing more to anyone.
    #[tokio::test]
    async fn a_writer_sends_nothing_once_saving_failed() {
        let (w, r) = tokio::io::duplex(64 * 1024);
        let (_wr, wh) = tokio::io::split(w);
        let (mut rh, _rw) = tokio::io::split(r);
        let (tx, rx) = broadcast::channel(16);
        let (control_tx, control_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let health = StorageHealth::default();
        let handle = tokio::spawn(writer_task(
            Box::new(wh),
            rx,
            control_rx,
            "default".into(),
            Duration::from_millis(5),
            Duration::from_millis(20),
            cancel.clone(),
            CancellationToken::new(),
            health.clone(),
        ));
        health.set("disk full".into());
        tx.send(sample()).unwrap();
        control_tx.send(SyncMessage::MembershipDigest { members: Vec::new() }).await.unwrap();

        let got = tokio::time::timeout(Duration::from_millis(200), read_frame(&mut rh)).await;
        assert!(got.is_err(), "something was sent after saving failed: {got:?}");
        cancel.cancel();
        let _ = handle.await;
    }

    #[test]
    fn a_second_handshake_for_the_same_identity_is_refused_while_the_first_is_live() {
        let set: InflightHandshakes = Arc::new(Mutex::new(BTreeSet::new()));
        let first = HandshakeSlot::claim(&set, 7).expect("first claim");
        assert!(
            HandshakeSlot::claim(&set, 7).is_none(),
            "an identity already handshaking must be refused, not queued: without this \
             a flood from one party spawns one task per connection"
        );
        // A different identity is not affected. No party can use up another's
        // place.
        let other = HandshakeSlot::claim(&set, 8).expect("distinct identity");
        drop(other);
        drop(first);
        assert!(
            HandshakeSlot::claim(&set, 7).is_some(),
            "dropping the slot must release the identity, or a peer is locked out for good"
        );
    }

    /// A connection whose stream setup never finishes. This is the stalled
    /// party the handshake timeout exists for. It presents a readable replica
    /// identity so that nothing fails early, before the stall.
    struct StalledConn;

    #[async_trait::async_trait]
    impl orvu_transport::Conn for StalledConn {
        async fn open_stream(
            &self,
        ) -> orvu_transport::Result<(orvu_transport::StreamWrite, orvu_transport::StreamRead)>
        {
            std::future::pending().await
        }
        async fn accept_stream(
            &self,
        ) -> orvu_transport::Result<(orvu_transport::StreamWrite, orvu_transport::StreamRead)>
        {
            std::future::pending().await
        }
        fn identity(&self) -> orvu_transport::Identity {
            orvu_transport::Identity {
                uri: Some(orvu_transport::types::replica_identity_uri(9)),
                fingerprint: None,
            }
        }
        async fn close(&self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_connection_times_out_in_both_handshake_roles() {
        // The clock is paused, so the runtime moves time forward as soon as
        // every task is idle. `HANDSHAKE_TIMEOUT` then fires at once and the
        // test does not wait ten real seconds. Both roles are tested because
        // the initiator waits on `open_stream` and the acceptor waits on
        // `accept_stream`. A timeout around only one of them would leave the
        // other able to hang forever.
        //
        // The outer timeout is there so that removing the handshake limit gives
        // a clear assertion failure, not a test that hangs until CI kills it
        // with an unexplained job timeout.
        for initiator in [true, false] {
            let outcome = tokio::time::timeout(
                Duration::from_secs(60),
                handshake(
                    &StalledConn,
                    orvu_crdt::ReplicaId(1),
                    "default",
                    initiator,
                    &crate::config::Acl::open(),
                ),
            )
            .await;
            assert!(
                matches!(outcome, Ok(None)),
                "handshake must time out and yield None; it hung past its own bound, \
                 which means HANDSHAKE_TIMEOUT is not being applied (initiator={initiator})"
            );
        }
    }

    #[test]
    fn a_slot_is_released_even_when_the_handshake_panics() {
        // A place that is never given back keeps a legitimate machine out for
        // good. That is worse than the flood this guard protects against.
        let set: InflightHandshakes = Arc::new(Mutex::new(BTreeSet::new()));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = HandshakeSlot::claim(&set, 3).expect("claim");
            panic!("handshake blew up");
        }));
        assert!(caught.is_err(), "the probe must actually panic");
        assert!(set.lock().unwrap().is_empty(), "unwinding must release the slot");
    }
}
