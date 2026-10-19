//! The public `Node`. Start it with a transport and a config. It owns a replica
//! (this machine's copy of the data) and syncs with every machine listed in the
//! config.

use crate::antientropy::antientropy_task;
use crate::config::NodeConfig;
use crate::engine::{self, Command, EngineHandle};
use crate::peer::{handshake_at, reader_task, writer_task};
use crate::persistence::PersistHandle;
use orvu_core::{TypedCrdt, TypedDelta, WireDelta};
use orvu_crdt::{Hlc, ReplicaId, VersionVector};
use orvu_transport::{Conn, Transport};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Maps each connected machine's `ReplicaId` to the control channel of its
/// connection. With it the node can send a `SyncMessage`, such as a
/// `BorrowRequest`, to one chosen machine and not to all of them. `wire_peer`
/// adds an entry after the handshake.
/// The connections that are open right now, by the machine at the other end.
/// Two machines that list each other end up with two connections, one dialed
/// from each side, so a machine can have more than one entry.
#[derive(Clone, Default)]
pub(crate) struct PeerRegistry {
    inner: Arc<std::sync::Mutex<RegistryInner>>,
    /// Fires on a clean shutdown: each connection's writer sends what is
    /// waiting and ends.
    drain: CancellationToken,
    /// Wakes `drained` each time a writer ends.
    writer_ended: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct RegistryInner {
    next_id: u64,
    conns: std::collections::BTreeMap<ReplicaId, Vec<(u64, mpsc::Sender<orvu_core::SyncMessage>)>>,
    /// How many connection writers are running.
    writers: usize,
}

/// Counts one running writer for as long as it is held.
pub(crate) struct WriterRunning(PeerRegistry);

impl Drop for WriterRunning {
    fn drop(&mut self) {
        self.0.inner.lock().unwrap().writers -= 1;
        self.0.writer_ended.notify_waiters();
    }
}

impl PeerRegistry {
    /// Counts a writer that is starting. It counts until the returned value
    /// is dropped.
    pub(crate) fn writer_started(&self) -> WriterRunning {
        self.inner.lock().unwrap().writers += 1;
        WriterRunning(self.clone())
    }

    /// What tells the writers to send what is waiting and end.
    pub(crate) fn drain_signal(&self) -> CancellationToken {
        self.drain.clone()
    }

    /// Tells every writer to send what is waiting and end, and waits until
    /// they have, or until `limit` has passed. A machine that does not read
    /// cannot hold a shutdown up for longer than that.
    pub(crate) async fn drain(&self, limit: std::time::Duration) {
        self.drain.cancel();
        let all_ended = async {
            loop {
                let woken = self.writer_ended.notified();
                tokio::pin!(woken);
                woken.as_mut().enable();
                if self.inner.lock().unwrap().writers == 0 {
                    return;
                }
                woken.await;
            }
        };
        let _ = tokio::time::timeout(limit, all_ended).await;
    }

    /// Records a connection to `replica`. Returns an id for taking this one
    /// connection out again with `remove`.
    pub(crate) fn add(&self, replica: ReplicaId, tx: mpsc::Sender<orvu_core::SyncMessage>) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.conns.entry(replica).or_default().push((id, tx));
        id
    }

    /// Takes out the one connection `id`. Other connections to the same
    /// machine stay.
    pub(crate) fn remove(&self, replica: ReplicaId, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(list) = inner.conns.get_mut(&replica) {
            list.retain(|(other, _)| *other != id);
            if list.is_empty() {
                inner.conns.remove(&replica);
            }
        }
    }

    /// The machines with at least one open connection right now, in order of
    /// id. A connection stays here only while the machine at the other end
    /// keeps talking: one that goes silent is closed and taken out.
    pub(crate) fn connected(&self) -> Vec<ReplicaId> {
        self.inner.lock().unwrap().conns.keys().copied().collect()
    }

    /// Every open connection to `replica`, newest first.
    pub(crate) fn senders(&self, replica: ReplicaId) -> Vec<mpsc::Sender<orvu_core::SyncMessage>> {
        let inner = self.inner.lock().unwrap();
        inner.conns.get(&replica).map(|l| l.iter().rev().map(|(_, tx)| tx.clone()).collect()).unwrap_or_default()
    }
}

/// How long to wait before a configured machine is dialed again.
///
/// A machine that is only starting up should be found quickly, so the first
/// wait is short. One that is gone for good, or was given by a name nobody
/// knows, should not be dialed four times a second for ever, so each wait is
/// twice the one before, up to a longest one, which is
/// `NodeConfig::longest_dial_wait`.
pub(crate) struct DialPause {
    next: std::time::Duration,
    longest: std::time::Duration,
}

impl DialPause {
    const FIRST: std::time::Duration = std::time::Duration::from_millis(250);
    /// A connection that was held this long counts as one that worked.
    const LASTED: std::time::Duration = std::time::Duration::from_secs(5);

    /// Waits that grow up to `longest`. A `longest` below the first wait
    /// makes every wait `longest`.
    pub(crate) fn new(longest: std::time::Duration) -> Self {
        Self { next: Self::FIRST.min(longest), longest }
    }

    /// The wait after a dial that failed.
    pub(crate) fn after_failure(&mut self) -> std::time::Duration {
        let wait = self.next;
        self.next = (wait * 2).min(self.longest);
        wait
    }

    /// The wait after a connection that was held for `held` and has ended. If
    /// it lasted, the waits start over. If it ended as soon as it was made,
    /// which is what a machine that turns us away each time does, it counts
    /// as one more failure.
    pub(crate) fn after_connection(&mut self, held: std::time::Duration) -> std::time::Duration {
        if held >= Self::LASTED {
            self.next = Self::FIRST.min(self.longest);
        }
        self.after_failure()
    }
}

/// The longest a clean shutdown waits for the last changes to be sent.
const SEND_ON_SHUTDOWN: std::time::Duration = std::time::Duration::from_secs(2);

/// Starts nodes. See `Node::spawn`.
pub struct Node;

/// Why a node could not start.
#[derive(Debug)]
pub enum SpawnError {
    /// The storage file could not be opened or read.
    Storage(String),
    /// The network could not be set up, for example the listen address could
    /// not be taken.
    Transport(orvu_transport::TransportError),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::Storage(why) => write!(f, "cannot open the store: {why}"),
            SpawnError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SpawnError::Storage(_) => None,
            SpawnError::Transport(e) => Some(e),
        }
    }
}

impl From<orvu_transport::TransportError> for SpawnError {
    fn from(e: orvu_transport::TransportError) -> Self {
        SpawnError::Transport(e)
    }
}

impl From<std::io::Error> for SpawnError {
    fn from(e: std::io::Error) -> Self {
        SpawnError::Transport(e.into())
    }
}

tokio::task_local! {
    /// The id of the request the changes made here are for. See
    /// `with_receipt`.
    static RECEIPT: [u8; 16];
}

/// Runs `work`, and every change it makes through a node is made for the
/// request `id`: it carries `id` as its trace id, and a receipt saying so is
/// saved in the same step as the change. After a crash, `receipt_made`
/// on the restarted node says which requests were done. A change that was
/// refused, or not made, leaves no receipt.
///
/// It holds for the task that runs `work`, not for tasks `work` starts.
pub async fn with_receipt<F: std::future::Future>(id: [u8; 16], work: F) -> F::Output {
    RECEIPT.scope(id, work).await
}

/// The receipt id the changes made here are made under, inside
/// `with_receipt`, or `None`.
pub fn current_receipt() -> Option<[u8; 16]> {
    RECEIPT.try_with(|id| *id).ok()
}

/// A handle to a running node. Use it to read and change objects and to shut
/// the node down.
pub struct NodeHandle {
    engine: mpsc::Sender<Command>,
    /// Where `shutdown` asks the engine to stop. The engine sees this ahead
    /// of anything still in its queue.
    stopper: crate::engine::Stopper,
    /// Fires as soon as `shutdown` is called. From then on nothing new is
    /// sent to the engine, and calls still waiting for room in its queue give
    /// up.
    stopping: CancellationToken,
    changes: tokio::sync::broadcast::Sender<String>,
    wall: Arc<AtomicU64>,
    cancel: CancellationToken,
    cfg: NodeConfig,
    /// Fires when the thread that writes to the store has stopped and dropped
    /// the store, which releases the lock on the backing file. `shutdown` takes
    /// it out and waits on it, so it is `None` after that. It sits behind a
    /// mutex because `shutdown` only has `&self`.
    writer_exit: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// Shared with the engine task, which updates it on every state change.
    metrics: Arc<crate::metrics::Metrics>,
    /// Used to send to one machine: maps a `ReplicaId` to its control channel.
    peers: PeerRegistry,
    /// Whether saving to disk has failed. Only a record, so holding it does
    /// not keep the writer thread alive.
    storage: crate::persistence::StorageHealth,
    /// The receipts of the last ten minutes or so: see `Receipts`.
    receipts: crate::persistence::Receipts,
}

impl Node {
    /// Starts a node that listens on `cfg.listen` and dials every machine in
    /// `cfg.peers`. A storage file that cannot be opened or read gives
    /// `SpawnError::Storage`.
    pub async fn spawn(
        transport: Arc<dyn Transport>,
        cfg: NodeConfig,
    ) -> Result<NodeHandle, SpawnError> {
        let store = match &cfg.store_path {
            Some(path) => Some(Box::new(
                orvu_store::RedbStore::open(path).map_err(|e| SpawnError::Storage(e.to_string()))?,
            ) as Box<dyn orvu_store::Store>),
            None => None,
        };
        Self::spawn_inner(transport, cfg, store).await
    }

    /// Like `spawn`, but saves to `store` instead of the file at
    /// `cfg.store_path`, which is not used. What `store` holds is loaded
    /// first, and it is saved to as `cfg.fsync` says. A test can pass a
    /// `MemStore` here and make it fail, to see what a node does when it can
    /// no longer save.
    pub async fn spawn_with_store(
        transport: Arc<dyn Transport>,
        cfg: NodeConfig,
        store: Box<dyn orvu_store::Store>,
    ) -> Result<NodeHandle, SpawnError> {
        Self::spawn_inner(transport, cfg, Some(store)).await
    }

    async fn spawn_inner(
        transport: Arc<dyn Transport>,
        cfg: NodeConfig,
        store: Option<Box<dyn orvu_store::Store>>,
    ) -> Result<NodeHandle, SpawnError> {
        let cancel = CancellationToken::new();
        let wall = Arc::new(AtomicU64::new(1));
        if let Some(set) = cfg.quiet_limit.filter(|q| *q < cfg.lowest_quiet_limit()) {
            tracing::warn!(
                set = ?set,
                used = ?cfg.quiet_limit(),
                "the quiet limit is below three anti-entropy intervals or below one second and would close healthy links; the higher value is used"
            );
        }

        // Load what the store holds, if there is one, and start the thread
        // that writes to it. A store that cannot be read is returned from
        // spawn as a storage error.
        let (persist, snapshot, seed, writer_exit, policy) = match store {
            Some(store) => {
                let receipts = store
                    .receipts()
                    .map_err(|e| SpawnError::Storage(e.to_string()))?;
                let snapshot = store
                    .load_snapshot()
                    .map_err(|e| SpawnError::Storage(e.to_string()))?;
                let seed = store
                    .replay()
                    .map_err(|e| SpawnError::Storage(e.to_string()))?;
                let (handle, exit) = crate::persistence::spawn_writer(store, cfg.fsync);
                for receipt in receipts {
                    handle.receipts().add(receipt);
                }
                (handle, snapshot, seed, Some(exit), cfg.fsync)
            }
            None => {
                // No store path: use an in-memory `MemStore` so there is always
                // a store for catch-up to read from. Catch-up is machines
                // comparing what they have seen and resending what is missing.
                // Nothing is kept on disk and each spawn starts empty. A
                // "save" here is instant, so it is done with every change:
                // waiting for a timer would only hold changes back from other
                // machines for nothing.
                let policy = orvu_store::FsyncPolicy::Always;
                let (handle, exit) =
                    crate::persistence::spawn_writer(Box::new(orvu_store::MemStore::new()), policy);
                (handle, Vec::new(), Vec::new(), Some(exit), policy)
            }
        };
        // Each reader gets a cheap clone of the store writer's handle for
        // catch-up.
        let reader_persist = persist.clone();
        let storage = persist.health();
        let receipts = persist.receipts();

        let metrics = Arc::new(crate::metrics::Metrics::default());
        let eng = engine::spawn(
            cfg.replica,
            cfg.causal_buffer_cap,
            // A change still waiting after two catch-up rounds is dropped:
            // catch-up will have sent what it waits for, or sends it again.
            cfg.anti_entropy_interval * 2,
            cfg.ship_queue_cap,
            crate::engine::Persistence {
                handle: Some(persist),
                policy,
                compaction_threshold: cfg.compaction_threshold,
                snapshot,
                deltas: seed,
            },
            cfg.acl.clone(),
            cfg.suspect_timeout,
            cfg.dead_timeout,
            cfg.forget_removed_after,
            metrics.clone(),
        );
        let changes = eng.changes_sender();

        // The Prometheus `/metrics` HTTP endpoint, only if an address is
        // configured.
        if let Some(addr) = &cfg.metrics_addr {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            tokio::spawn(crate::metrics_server::serve_metrics(
                listener,
                metrics.clone(),
                cancel.clone(),
            ));
        }

        let peers = PeerRegistry::default();

        // Everything that talks to other machines stops on `links`: on
        // shutdown, and also once saving has failed. A node that cannot save
        // shows its data to nobody, and if it stayed connected the others
        // would keep it as a live member whose view never moves. Nothing
        // could then count as seen by every member, and their logs would only
        // grow. So it closes every connection, stops dialing and stops
        // listening, and the others see it as gone.
        let links = cancel.child_token();
        {
            let links = links.clone();
            let storage = storage.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = links.cancelled() => {}
                    _ = storage.lost() => {
                        tracing::error!(
                            error = %storage.failure().map(|f| f.to_string()).unwrap_or_default(),
                            "saving failed; closing every connection to other machines"
                        );
                        links.cancel();
                    }
                }
            });
        }

        // Listener: accept incoming connections.
        let listener = transport.listen(&cfg.listen, links.clone()).await?;
        {
            let eng = eng.clone();
            let cfg = cfg.clone();
            let wall = wall.clone();
            let cancel = links.clone();
            let persist = reader_persist.clone();
            let metrics = metrics.clone();
            let peers = peers.clone();
            let inflight: crate::peer::InflightHandshakes =
                Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        conn = listener.accept() => match conn {
                            Ok(conn) => {
                                // Decide whether to admit the connection from
                                // the identity the transport verified, before
                                // any I/O. There is no membership check. Any
                                // machine may join, so a replica that is not in
                                // `cfg.peers` is a new machine and is welcome.
                                // `bind_identity` and the access list (ACL)
                                // still apply after the hello.
                                let Some(id) = crate::peer::verified_identity(conn.as_ref())
                                else {
                                    continue; // no verified identity, so bind_identity would reject it anyway
                                };
                                let Some(slot) = crate::peer::HandshakeSlot::claim(&inflight, id)
                                else {
                                    continue; // this identity is already handshaking
                                };
                                // The handshake runs in its own task and stops
                                // on shutdown. In this loop, one stalled
                                // handshake would hold up every other machine.
                                // The task also holds a `PersistHandle` clone,
                                // and `shutdown()` waits for the store writer
                                // to exit, which needs every clone dropped.
                                // Without the shutdown branch, shutdown would
                                // wait for the stalled handshake to time out.
                                let (eng, cfg, wall, cancel2, persist, metrics, peers) = (
                                    eng.clone(), cfg.clone(), wall.clone(), cancel.clone(),
                                    persist.clone(), metrics.clone(), peers.clone(),
                                );
                                tokio::spawn(async move {
                                    let _slot = slot;
                                    tokio::select! {
                                        _ = cancel2.cancelled() => {}
                                        _ = wire_peer(conn, &eng, &cfg, wall, cancel2.clone(), false, persist, metrics, peers) => {}
                                    }
                                });
                            }
                            // Tell listener faults apart from failures of one
                            // connection. The short sleep below stops this loop
                            // spinning when the listener is dead (`Closed`) or
                            // the process is out of file descriptors (`Io`). A
                            // failed handshake from a stranger is passed on to
                            // us as an error too. Sleeping on each of those
                            // would limit the loop to 200 errors a second. A
                            // fast flood of connections with no credentials
                            // could then keep the accept queue full, and
                            // machines that had already authenticated would be
                            // discarded. So a failure of one connection is
                            // skipped with no delay.
                            //
                            // Skipping without a delay cannot spin, because an
                            // error for one connection only exists after
                            // incoming I/O. Each one comes from a handshake
                            // task that was started for an admitted connection,
                            // so the network and `MAX_HANDSHAKES` limit how
                            // fast they arrive. `Closed` and `Io` are the only
                            // errors that can appear with no I/O at all: a dead
                            // channel returns at once every time, and
                            // `accept()` fails at once when file descriptors
                            // run out. That is why those two are slowed down.
                            // If the transport ever reported some other error
                            // with no connection behind it, that error would
                            // need the delay too.
                            Err(orvu_transport::TransportError::Closed | orvu_transport::TransportError::Io(_)) => tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                            },
                            Err(_) => continue,
                        }
                    }
                }
            });
        }

        // Dialer: connect to each configured machine.
        for peer in &cfg.peers {
            let transport = transport.clone();
            let eng = eng.clone();
            let cfg = cfg.clone();
            let wall = wall.clone();
            let cancel = links.clone();
            let peer = peer.clone();
            let persist = reader_persist.clone();
            let metrics = metrics.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                let mut pause = DialPause::new(cfg.longest_dial_wait);
                loop {
                    // A dial to a machine that is switched off is not
                    // refused, it goes unanswered for a long while. Shutdown
                    // does not wait for it.
                    let dialed = tokio::select! {
                        _ = cancel.cancelled() => break,
                        dialed = transport.dial(peer.clone(), cancel.clone()) => dialed,
                    };
                    let mut wait = None;
                    if let Ok(conn) = dialed {
                        let connected = std::time::Instant::now();
                        let wired = wire_peer(
                            conn, &eng, &cfg, wall.clone(), cancel.clone(), true,
                            persist.clone(), metrics.clone(), peers.clone(),
                        )
                        .await;
                        if let Some(ended) = wired {
                            // Connected. Nothing more to do until this
                            // connection ends: the other machine stopped, or
                            // the network between us went away. Then it is
                            // dialed again, so that a machine that comes back
                            // is found without either side being restarted.
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = ended => {}
                            }
                            wait = Some(pause.after_connection(connected.elapsed()));
                        }
                    }
                    // A dial can fail because the other machine is not up yet,
                    // or because the transport turned us away under load.
                    // Neither is permanent, so wait and try again. The wait
                    // also ends on shutdown, so shutdown is not delayed.
                    let wait = wait.unwrap_or_else(|| pause.after_failure());
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
            });
        }

        // Rebalancing, if switched on: move `BoundedCounter` allowance between
        // machines on a timer, before anyone runs out.
        if let Some(period) = cfg.rebalance_interval {
            tokio::spawn(crate::rebalance::rebalance_task(
                eng.sender(),
                peers.clone(),
                cfg.replica,
                wall.clone(),
                period,
                cancel.clone(),
            ));
        }

        Ok(NodeHandle {
            engine: eng.sender(),
            stopper: eng.stopper(),
            stopping: CancellationToken::new(),
            changes,
            wall,
            cancel,
            cfg,
            writer_exit: std::sync::Mutex::new(writer_exit),
            metrics,
            peers: peers.clone(),
            storage,
            receipts,
        })
    }
}

/// Sets up one connection. One handshake opens a single two-way stream. Its
/// write half goes to the writer task and its read half to the reader task. The
/// catch-up task gets `control_tx` so it can send through the same writer.
///
/// Returns `None` when the handshake failed, so the dialer knows to try
/// again. When the connection was set up, returns something to wait on that
/// finishes when the connection has ended.
#[allow(clippy::too_many_arguments)]
async fn wire_peer(
    conn: Box<dyn Conn>,
    eng: &EngineHandle,
    cfg: &NodeConfig,
    wall: Arc<AtomicU64>,
    cancel: CancellationToken,
    initiator: bool,
    persist: PersistHandle,
    metrics: Arc<crate::metrics::Metrics>,
    peers: PeerRegistry,
) -> Option<tokio::task::JoinHandle<()>> {
    let (write, read, peer) =
        handshake_at(conn.as_ref(), cfg.replica, &cfg.namespace, initiator, &cfg.acl, cfg.proto_version).await?;
    let replica = ReplicaId(peer);

    // Tell the engine this machine is connected, so it counts as a member.
    let _ = eng.sender().send(Command::PeerUp { replica }).await;

    // Control channel: catch-up messages go to the writer and out on the same
    // stream.
    let (control_tx, control_rx) = mpsc::channel::<orvu_core::SyncMessage>(64);
    let conn_id = peers.add(replica, control_tx.clone());

    // This connection's own stop signal. Shutdown stops it too.
    let link = cancel.child_token();

    // Counted while it runs, so a clean shutdown can wait for it to send
    // what is waiting.
    let running = peers.writer_started();
    let writer = writer_task(
        write,
        eng.subscribe(),
        control_rx,
        cfg.namespace.clone(),
        cfg.delta_batch,
        cfg.anti_entropy_interval,
        link.clone(),
        peers.drain_signal(),
        persist.health(),
    );
    tokio::spawn(async move {
        writer.await;
        drop(running);
    });
    tokio::spawn(antientropy_task(
        eng.sender(),
        control_tx.clone(),
        cfg.anti_entropy_interval,
        cfg.namespace.clone(),
        link.clone(),
        metrics.clone(),
    ));
    // Run the reader until it ends, then forget this connection and tell the
    // engine it is gone. If it ended because the other machine went silent,
    // this connection's writer and catch-up are stopped and the connection is
    // closed, so the dialer can start over with a fresh one.
    let reader_eng = eng.sender();
    let acl = cfg.acl.clone();
    let quiet = cfg.quiet_limit();
    Some(tokio::spawn(async move {
        let went_quiet = reader_task(
            read, reader_eng.clone(), replica, control_tx, wall, link.clone(), persist, metrics, acl, quiet,
        )
        .await;
        if went_quiet {
            link.cancel();
            conn.close().await;
        }
        peers.remove(replica, conn_id);
        let _ = reader_eng.send(Command::PeerDown { replica }).await;
    }))
}

impl NodeHandle {
    /// Whether a change was made under the receipt `id`, before this node
    /// started or since, within the time receipts are kept for: ten minutes,
    /// `RECEIPT_KEEP_MS`. See `with_receipt`.
    pub fn receipt_made(&self, id: &[u8; 16]) -> bool {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.receipts.made(id, now_ms)
    }

    fn wall(&self) -> u64 {
        crate::peer::now_wall(&self.wall)
    }

    /// Hands `command` to the engine, unless the node is stopping. Returns
    /// whether it was taken. A command that is not taken is dropped, and so
    /// is its reply channel, so the caller gets the answer it would get from
    /// an engine that has stopped.
    async fn send(&self, command: Command) -> bool {
        if self.stopping.is_cancelled() {
            return false;
        }
        tokio::select! {
            biased;
            _ = self.stopping.cancelled() => false,
            sent = self.engine.send(command) => sent.is_ok(),
        }
    }

    /// Creates the object `object_id` from `crdt`.
    pub async fn create(&self, object_id: &str, crdt: TypedCrdt) {
        let (done, rx) = oneshot::channel();
        self.send(Command::Create { object_id: object_id.into(), crdt, done }).await;
        let _ = rx.await;
    }

    /// Changes `object_id` by running `f` on it. Returns the delta it made,
    /// which goes to the other machines once it is saved, or `None` if no
    /// delta was made or this node can no longer save (see
    /// `storage_failure`).
    pub async fn mutate<F>(&self, object_id: &str, f: F) -> Option<WireDelta>
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> TypedDelta + Send + 'static,
    {
        self.write(object_id, f).await.ok()
    }

    /// Like `mutate`, but says why there is no delta: `Err(Some(why))` if the
    /// object turned the write away, because it has no fresh dot left for
    /// this machine or no room for one more writer, and `Err(None)` if the
    /// change was not done for another reason (no such object, the engine
    /// is gone, or saving failed). Either way nothing was changed or sent.
    pub async fn write<F>(&self, object_id: &str, f: F) -> Result<WireDelta, Option<orvu_crdt::WriteError>>
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> TypedDelta + Send + 'static,
    {
        let (done, rx) = oneshot::channel();
        let receipt = RECEIPT.try_with(|id| *id).ok();
        self.send(Command::Mutate { object_id: object_id.into(), f: Box::new(f), wall: self.wall(), receipt, done }).await;
        rx.await.unwrap_or(Err(None))
    }

    /// Like `mutate`, but the closure may refuse by returning `None`. On
    /// `Applied` the engine has taken the delta, and it goes to the other
    /// machines once it is saved. On `Refused` or `NoObject` no delta was
    /// made, sent or stored. On `StorageFailed` this node can no longer save,
    /// and the change was not taken or not saved.
    pub async fn try_mutate<F>(&self, object_id: &str, f: F) -> crate::engine::TryOutcome
    where
        F: FnOnce(&mut TypedCrdt, Hlc, ReplicaId) -> Option<TypedDelta> + Send + 'static,
    {
        let (done, rx) = oneshot::channel();
        let receipt = RECEIPT.try_with(|id| *id).ok();
        self.send(Command::TryMutate { object_id: object_id.into(), f: Box::new(f), wall: self.wall(), receipt, done }).await;
        rx.await.unwrap_or(crate::engine::TryOutcome::NoObject)
    }

    /// Sends a `BorrowRequest` to the machine `to` only. Returns `false` if
    /// `to` is not connected right now, or if its channel is full or closed. It
    /// never waits and never panics.
    pub async fn send_borrow_request(&self, to: ReplicaId, object_id: &str, amount: u64) -> bool {
        // Try each open connection to `to` until one takes the message.
        self.peers.senders(to).into_iter().any(|tx| {
            tx.try_send(orvu_core::SyncMessage::BorrowRequest {
                namespace: self.cfg.namespace.clone(),
                object_id: object_id.to_string(),
                amount,
                from: self.cfg.replica,
            })
            .is_ok()
        })
    }

    /// The machines this node has an open connection to right now, in order
    /// of id. A connection is kept only while the other machine keeps
    /// talking, so one that has stopped drops out, and a machine heard about
    /// only through others is never here. Asking changes nothing and costs
    /// little, unlike `known_members`.
    pub fn connected_peers(&self) -> Vec<ReplicaId> {
        self.peers.connected().into_iter().filter(|&p| p != self.cfg.replica).collect()
    }

    /// This node's replica id, taken from its config.
    pub fn replica_id(&self) -> ReplicaId {
        self.cfg.replica
    }

    /// How long a refused bounded-counter decrement waits for another machine
    /// to grant it allowance.
    pub fn borrow_timeout(&self) -> std::time::Duration {
        self.cfg.borrow_timeout
    }

    /// Looks at one object where it is and returns what `f` makes of it, or
    /// `None` if there is no such object. Also `None`, with `f` not run, once
    /// this node can no longer save (see `storage_failure`).
    ///
    /// Nothing is copied but what `f` returns. `snapshot` copies the whole
    /// object, which costs more the more the object holds, so a read of one
    /// thing should go through here.
    ///
    /// `f` runs inside the engine and holds everything else up while it
    /// does. It should look and return. An answer that takes long to work
    /// out is better worked out from a `snapshot`, outside the engine.
    ///
    /// If `f` panics, the engine carries on and the panic is passed to the
    /// caller, as if `f` had run there. That needs panics to unwind, as they
    /// do by default: in a program built with `panic = "abort"`, a panic in
    /// `f` ends the program. If the caller has stopped waiting by the time
    /// the engine gets to it, `f` is not run.
    pub async fn read<R, F>(&self, object_id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&TypedCrdt) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (reply, rx) = oneshot::channel();
        let look: crate::engine::Look = Box::new(move |object: Option<&TypedCrdt>| {
            if reply.is_closed() {
                return; // nobody is waiting for the answer
            }
            let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| object.map(f)));
            let _ = reply.send(answer);
        });
        self.send(Command::Read { object_id: object_id.into(), look }).await;
        match rx.await {
            Ok(Ok(answer)) => answer,
            // What went wrong in `f` belongs to the caller who wrote it.
            Ok(Err(panic)) => std::panic::resume_unwind(panic),
            Err(_) => None, // the engine has stopped
        }
    }

    /// Returns a copy of the current state of `object_id`, or `None` if this
    /// node does not have it or can no longer save (see `storage_failure`). The copy costs more the more the object holds:
    /// use `read` to look at one thing in it.
    pub async fn snapshot(&self, object_id: &str) -> Option<TypedCrdt> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Snapshot { object_id: object_id.into(), reply }).await;
        rx.await.ok().flatten()
    }

    /// Leaves the team for good: hands all the allowance this machine holds,
    /// on every bounded counter, to the machines it is connected to, and
    /// waits until they have it, for at most `wait`. See `crate::leave`.
    ///
    /// The machine is first noted as gone, in `crate::leave::LEFT`, so that
    /// the others give it no allowance back. It should not be started again
    /// under its number afterwards. Fails, changing nothing, if no other
    /// machine connects within `wait`.
    pub async fn leave(&self, wait: std::time::Duration) -> Result<crate::Leaving, crate::LeaveError> {
        crate::leave::leave(self, wait, self.cfg.anti_entropy_interval).await
    }

    /// What the machine `by` last said it has seen, for each object, in its
    /// catch-up digest. Empty if nothing was heard from it.
    pub async fn seen_by(&self, by: ReplicaId) -> std::collections::BTreeMap<String, VersionVector> {
        let (reply, rx) = oneshot::channel();
        if !self.send(Command::SeenBy { by, reply }).await {
            return std::collections::BTreeMap::new();
        }
        rx.await.unwrap_or_default()
    }

    /// What this node has seen, for each object it has.
    pub(crate) async fn own_contexts(&self) -> Vec<(String, orvu_crdt::DotContext)> {
        let (reply, rx) = oneshot::channel();
        if !self.send(Command::Digest { reply }).await {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// The ids of every object this node has.
    pub(crate) async fn object_ids(&self) -> Vec<String> {
        self.own_contexts().await.into_iter().map(|(id, _)| id).collect()
    }

    /// The number of machines listed in the config. It does not say how many are connected.
    pub fn peer_count(&self) -> usize {
        self.cfg.peers.len()
    }

    /// Subscribes to change events. Each event is the id of an object that changed.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.changes.subscribe()
    }

    /// Why this node can no longer save to disk, or `None` while saving works.
    ///
    /// Once saving has failed, memory may hold a change that is not on disk,
    /// so the node stops for clients and for other machines. Every change is
    /// refused (`mutate` returns `None`, `try_mutate` returns
    /// `TryOutcome::StorageFailed`), every read finds nothing (`read`,
    /// `snapshot`), and nothing more is sent to other machines. Every
    /// connection to them is closed and new ones are turned away, so they
    /// see this node as gone. Restarting
    /// the node loads what is on disk and is the way back.
    pub fn storage_failure(&self) -> Option<String> {
        self.storage.failure().map(|why| why.to_string())
    }

    /// Finishes once this node can no longer save to disk, at once if it
    /// already cannot. Code that waits for changes can wait on this as well,
    /// so that it ends when the node stops serving.
    pub async fn saving_failed(&self) {
        self.storage.lost().await
    }

    /// A copy of this node's metrics at this moment. Taking it locks nothing.
    pub fn metrics(&self) -> crate::metrics::MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Makes sure `object_id` exists with the type of `empty`. Returns `true`
    /// if it now has that type, and `false` if it already exists with a
    /// different type.
    pub async fn open(&self, object_id: &str, empty: TypedCrdt) -> bool {
        let (reply, rx) = oneshot::channel();
        if !self.send(Command::Open { object_id: object_id.into(), empty, reply }).await {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// The stability frontier for each object: the updates that every other
    /// member is known to have seen. Empty when there are no other members.
    pub async fn stable_frontier(&self) -> std::collections::BTreeMap<String, VersionVector> {
        let (reply, rx) = oneshot::channel();
        if !self.send(Command::StableFrontier { reply }).await {
            return std::collections::BTreeMap::new();
        }
        rx.await.unwrap_or_default()
    }

    /// The replica ids of every member this node knows about: itself, the nodes
    /// connected to it, and members it has only heard about through membership
    /// gossip. A member heard about through gossip is dropped once its entry
    /// has gone stale.
    ///
    /// This is a `MembershipView` query and it has side effects. It refreshes
    /// this node's own entry, which raises the gossip heartbeat, and it runs a
    /// pass that drops stale members. That is fine for an occasional look, but
    /// do not call it in a tight polling loop.
    pub async fn known_members(&self) -> Vec<ReplicaId> {
        let (reply, rx) = oneshot::channel();
        if !self.send(Command::MembershipView { reply }).await {
            return Vec::new();
        }
        rx.await
            .map(|members| members.into_iter().map(|m| m.replica).collect())
            .unwrap_or_default()
    }

    /// How many deltas are in the stored log that catch-up reads from.
    pub async fn durable_tail_len(&self) -> usize {
        let (reply, rx) = oneshot::channel();
        if !self.send(Command::TailLen { reply }).await {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Finishes once `shutdown` has been called on this node. Code that waits
    /// for changes can wait on this as well, so that it is let go when the
    /// node stops.
    pub async fn stopped(&self) {
        self.cancel.cancelled().await
    }

    /// Stops the node and waits until the store has been flushed and released.
    ///
    /// Changes made before the call are saved first, then sent to the
    /// machines still connected, and only then are the connections closed.
    /// Sending them waits at most `SEND_ON_SHUTDOWN`, so a machine that does
    /// not read cannot hold the shutdown up. Calls still waiting in the
    /// engine's queue, or for room in it, when this is called are not run:
    /// each gets the answer of a call made after the node stopped.
    pub async fn shutdown(&self) {
        // Nothing new goes in from here, and calls waiting for room give up.
        // The engine sees the stop ahead of what is already queued.
        self.stopping.cancel();
        let (done, rx) = oneshot::channel();
        let _ = self.stopper.send(done).await;
        let _ = rx.await;
        // The engine has saved everything and handed it to the connections.
        self.peers.drain(SEND_ON_SHUTDOWN).await;
        self.cancel.cancel();
        // The engine task has ended and dropped its `PersistHandle`. Wait for
        // the writer thread to flush, drop the store and release the file lock.
        // Take the receiver out first so the mutex is not held while waiting.
        let exit = self.writer_exit.lock().unwrap().take();
        if let Some(exit) = exit {
            let _ = exit.await;
        }
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    // Two machines that list each other have two connections. When one of
    // them ends, the other must still be found.
    #[test]
    fn ending_one_connection_keeps_the_other() {
        let registry = PeerRegistry::default();
        let (first_tx, _first_rx) = mpsc::channel(1);
        let (second_tx, mut second_rx) = mpsc::channel(1);
        let first = registry.add(ReplicaId(2), first_tx);
        let _second = registry.add(ReplicaId(2), second_tx);

        registry.remove(ReplicaId(2), first);

        let left = registry.senders(ReplicaId(2));
        assert_eq!(left.len(), 1);
        left[0].try_send(orvu_core::SyncMessage::MembershipDigest { members: Vec::new() }).unwrap();
        assert!(second_rx.try_recv().is_ok(), "the connection that is left is the second one");
    }

    #[test]
    fn a_machine_with_no_connection_left_is_forgotten() {
        let registry = PeerRegistry::default();
        let (tx, _rx) = mpsc::channel(1);
        let id = registry.add(ReplicaId(2), tx);
        registry.remove(ReplicaId(2), id);
        assert!(registry.senders(ReplicaId(2)).is_empty());
    }
}

#[cfg(test)]
mod dial_pause_tests {
    use super::DialPause;
    use std::time::Duration;

    #[test]
    fn each_wait_is_twice_the_one_before_up_to_five_seconds() {
        let mut pause = DialPause::new(Duration::from_secs(5));
        let waits: Vec<u64> = (0..7).map(|_| pause.after_failure().as_millis() as u64).collect();
        assert_eq!(waits, [250, 500, 1000, 2000, 4000, 5000, 5000]);
    }

    #[test]
    fn the_longest_wait_can_be_set() {
        let mut pause = DialPause::new(Duration::from_secs(1));
        let waits: Vec<u64> = (0..5).map(|_| pause.after_failure().as_millis() as u64).collect();
        assert_eq!(waits, [250, 500, 1000, 1000, 1000]);
    }

    // A longest wait below the first one makes every wait that long.
    #[test]
    fn a_longest_wait_below_the_first_caps_the_first_too() {
        let mut pause = DialPause::new(Duration::from_millis(100));
        let waits: Vec<u64> = (0..3).map(|_| pause.after_failure().as_millis() as u64).collect();
        assert_eq!(waits, [100, 100, 100]);
        assert_eq!(pause.after_connection(Duration::from_secs(60)), Duration::from_millis(100));
    }

    // A connection that lasted is a machine that works. When it ends, the
    // machine is dialed again at once. A connection that ended as soon as it
    // was made is no better than a dial that failed.
    #[test]
    fn only_a_connection_that_lasted_starts_the_waits_over() {
        let mut pause = DialPause::new(Duration::from_secs(5));
        for _ in 0..4 {
            pause.after_failure();
        }
        assert_eq!(pause.after_connection(Duration::from_millis(40)), Duration::from_millis(4000));
        assert_eq!(pause.after_connection(Duration::from_secs(60)), Duration::from_millis(250));
        assert_eq!(pause.after_failure(), Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Acl, Action};
    use orvu_crdt::{PnCounter, ReplicaId};
    use orvu_transport::mem::MemTransport;
    use orvu_transport::PeerAddr;
    use std::time::Duration;

    async fn lone_node() -> NodeHandle {
        let fabric = Arc::new(MemTransport::new());
        Node::spawn(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "a")).await.unwrap()
    }

    // A storage file that cannot be opened is reported as a storage problem,
    // not as a network one.
    #[tokio::test]
    async fn a_store_that_cannot_be_opened_is_a_storage_error() {
        let folder = std::env::temp_dir().join(format!("orvu-runtime-bad-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let mut cfg = NodeConfig::new(ReplicaId(1), "a");
        cfg.store_path = Some(folder.clone());
        let fabric = Arc::new(MemTransport::new());
        let err = Node::spawn(fabric.endpoint(1), cfg).await.err().expect("a folder was opened as a store");
        std::fs::remove_dir_all(&folder).unwrap();
        assert!(matches!(err, SpawnError::Storage(_)), "got {err:?}");
        assert!(err.to_string().contains("cannot open the store"), "{err}");
    }

    // A read is given the object itself and returns only what it makes of
    // it. Nothing has to be copied to look at one thing.
    #[tokio::test]
    async fn a_read_returns_what_the_closure_makes_of_the_object() {
        let a = lone_node().await;
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("c", |crdt, _hlc, replica| match crdt {
            TypedCrdt::PnCounter(pn) => orvu_core::TypedDelta::PnCounter(pn.increment(replica, 7)),
            _ => unreachable!(),
        })
        .await;
        let read = a.read("c", |crdt| match crdt {
            TypedCrdt::PnCounter(pn) => pn.value() * 2,
            _ => -1,
        });
        assert_eq!(read.await, Some(14));
        assert_eq!(a.read("nothing-here", |_| 1).await, None, "an object that is not there was read");
        a.shutdown().await;
    }

    // The closure runs inside the engine. If it panics, the engine has to
    // carry on: everything else this node does goes through it. The caller
    // is the one whose code went wrong, and the caller is where it shows.
    #[tokio::test]
    async fn a_read_that_panics_shows_in_the_caller_and_does_not_stop_the_engine() {
        let a = Arc::new(lone_node().await);
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        let caller = {
            let a = a.clone();
            tokio::spawn(async move { a.read("c", |_| -> i64 { panic!("the closure went wrong") }).await })
        };
        let ended = caller.await.expect_err("the caller was told nothing");
        assert!(ended.is_panic());
        let said = ended.into_panic();
        assert_eq!(said.downcast_ref::<&str>().copied(), Some("the closure went wrong"));
        assert_eq!(a.read("c", |_| 5).await, Some(5), "the engine stopped with the closure");
        assert_eq!(value(&a, "c").await, 0);
        a.shutdown().await;
    }

    // A caller that has stopped waiting gets no answer, so the work for one
    // is not done. Under a run of calls that gave up, the engine would
    // otherwise spend its time on answers nobody takes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_nobody_waits_for_is_not_run() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let a = Arc::new(lone_node().await);
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        // The engine is kept busy, so that the next read waits in line.
        let busy = {
            let a = a.clone();
            tokio::spawn(async move { a.read("c", |_| std::thread::sleep(Duration::from_millis(300))).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let given_up = tokio::time::timeout(
            Duration::from_millis(20),
            a.read("c", move |_| flag.store(true, Ordering::SeqCst)),
        )
        .await;
        assert!(given_up.is_err(), "the read was answered while the engine was busy");
        busy.await.unwrap();
        assert_eq!(a.read("c", |_| 1).await, Some(1)); // the engine has got past it
        assert!(!ran.load(Ordering::SeqCst), "work was done for a caller that had gone");
        a.shutdown().await;
    }

    // A copy of a whole object is counted, so that a read that goes back to
    // copying shows up, in a test and on a running node.
    #[tokio::test]
    async fn whole_copies_are_counted_and_looks_are_not() {
        let a = lone_node().await;
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        assert_eq!(a.metrics().objects_copied, 0);
        a.read("c", |_| ()).await;
        a.read("nothing-here", |_| ()).await;
        assert_eq!(a.metrics().objects_copied, 0);
        a.snapshot("c").await;
        a.snapshot("c").await;
        a.snapshot("nothing-here").await;
        assert_eq!(a.metrics().objects_copied, 2, "a copy that was not made was counted, or one was missed");
        a.shutdown().await;
    }

    async fn value(h: &NodeHandle, obj: &str) -> i64 {
        match h.snapshot(obj).await {
            Some(TypedCrdt::PnCounter(pn)) => pn.value(),
            _ => -1,
        }
    }

    async fn increment(h: &NodeHandle, obj: &str, by: u64) {
        h.mutate(obj, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, by)),
            _ => unreachable!(),
        }).await;
    }

    #[tokio::test]
    async fn two_nodes_converge_over_mem_transport() {
        let fabric = Arc::new(MemTransport::new());

        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;

        a.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
            _ => unreachable!(),
        }).await;

        // Give the nodes a moment to sync.
        for _ in 0..50 {
            if value(&b, "c").await == 5 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "c").await, 5);
    }

    #[tokio::test]
    async fn send_borrow_request_targets_connected_peers() {
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = std::time::Duration::from_millis(30);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = std::time::Duration::from_millis(30);
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        // Wait until A has registered B, which means the connection is up.
        let mut sent = false;
        for _ in 0..50 {
            if a.send_borrow_request(ReplicaId(2), "sku", 1).await { sent = true; break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(sent, "A can target the connected peer B");
        // A machine that is not connected can never be sent to.
        assert!(!a.send_borrow_request(ReplicaId(999), "sku", 1).await);
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn borrow_request_triggers_a_grant_transfer() {
        use orvu_core::{TypedCrdt, TypedDelta};
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = std::time::Duration::from_millis(30);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = std::time::Duration::from_millis(30);
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        // A holds all the allowance and B has none.
        a.create("sku", TypedCrdt::BoundedCounter(Default::default())).await;
        a.mutate("sku", |c, _h, r| match c {
            TypedCrdt::BoundedCounter(bc) => TypedDelta::BoundedCounter(bc.increment(10, r)),
            other => panic!("unexpected {other:?}"),
        }).await;
        b.create("sku", TypedCrdt::BoundedCounter(Default::default())).await;

        // B's allowance on "sku", as `node` sees it.
        async fn allowance_of_b(node: &NodeHandle) -> u64 {
            match node.snapshot("sku").await {
                Some(TypedCrdt::BoundedCounter(bc)) => bc.allowance(ReplicaId(2)),
                _ => 0,
            }
        }
        // Wait until the link is up, then send the borrow request.
        for _ in 0..50 {
            if b.send_borrow_request(ReplicaId(1), "sku", 4).await { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // Wait for A's transfer to arrive. B's allowance should rise to 4.
        let mut granted = 0;
        for _ in 0..100 {
            granted = allowance_of_b(&b).await;
            if granted >= 4 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(granted, 4, "A granted the borrow; B's allowance rose");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn rebalance_spreads_allowance_to_a_peer() {
        use orvu_core::{TypedCrdt, TypedDelta};
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = std::time::Duration::from_millis(30);
        cfg_a.rebalance_interval = Some(std::time::Duration::from_millis(40));
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = std::time::Duration::from_millis(30);
        cfg_b.rebalance_interval = Some(std::time::Duration::from_millis(40));
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        // A creates the counter and adds to it. B never asks for anything.
        a.create("stock", TypedCrdt::BoundedCounter(Default::default())).await;
        a.mutate("stock", |c, _h, r| match c {
            TypedCrdt::BoundedCounter(bc) => TypedDelta::BoundedCounter(bc.increment(10, r)),
            other => panic!("unexpected {other:?}"),
        }).await;

        // Wait until B sees its own allowance rise from a transfer it did not
        // ask for.
        let mut spread = false;
        for _ in 0..100 {
            if let Some(TypedCrdt::BoundedCounter(bc)) = b.snapshot("stock").await {
                if bc.allowance(ReplicaId(2)) >= 4 && bc.value() == 10 {
                    spread = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(spread, "proactive rebalancing spread allowance to peer B");
        a.shutdown().await;
        b.shutdown().await;
    }

    /// Machines 1 - 2 - 3 in a line: 1 and 3 know of each other only through 2.
    /// Only machine 1 rebalances.
    async fn line_of_three(fabric: &Arc<MemTransport>) -> (NodeHandle, NodeHandle, NodeHandle) {
        let mk = |id: u64, name: &'static str, peers: Vec<&'static str>, rebalance: bool| {
            let mut cfg = NodeConfig::new(ReplicaId(id), name);
            cfg.peers = peers.into_iter().map(PeerAddr::new).collect();
            cfg.anti_entropy_interval = Duration::from_millis(30);
            if rebalance {
                cfg.rebalance_interval = Some(Duration::from_millis(30));
            }
            cfg
        };
        let a = Node::spawn(fabric.endpoint(1), mk(1, "a", vec!["b"], true)).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), mk(2, "b", vec!["c"], false)).await.unwrap();
        let c = Node::spawn(fabric.endpoint(3), mk(3, "c", vec![], false)).await.unwrap();
        (a, b, c)
    }

    // The machines a node is connected to are the ones with a live
    // connection, not the ones it has only heard of.
    #[tokio::test]
    async fn connected_peers_are_only_the_ones_with_a_connection() {
        let fabric = Arc::new(MemTransport::new());
        let (a, b, c) = line_of_three(&fabric).await;
        for _ in 0..200 {
            if a.known_members().await.contains(&ReplicaId(3)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(a.known_members().await.contains(&ReplicaId(3)), "1 never heard of 3");
        assert_eq!(a.connected_peers(), vec![ReplicaId(2)]);
        assert_eq!(b.connected_peers(), vec![ReplicaId(1), ReplicaId(3)]);

        b.shutdown().await;
        for _ in 0..200 {
            if a.connected_peers().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(a.connected_peers().is_empty(), "a closed connection is still listed");
        a.shutdown().await;
        c.shutdown().await;
    }

    // Allowance handed to a machine we have no connection to could not be
    // spent or lent back by anyone we can reach. Rebalancing gives only to
    // machines connected right now.
    #[tokio::test]
    async fn rebalance_gives_nothing_to_a_machine_it_is_not_connected_to() {
        use orvu_core::{TypedCrdt, TypedDelta};
        let fabric = Arc::new(MemTransport::new());
        let (a, b, c) = line_of_three(&fabric).await;
        a.create("stock", TypedCrdt::BoundedCounter(Default::default())).await;
        a.mutate("stock", |crdt, _h, r| match crdt {
            TypedCrdt::BoundedCounter(bc) => TypedDelta::BoundedCounter(bc.increment(30, r)),
            other => panic!("unexpected {other:?}"),
        })
        .await;
        for _ in 0..200 {
            if a.known_members().await.contains(&ReplicaId(3)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(a.known_members().await.contains(&ReplicaId(3)), "1 never heard of 3");
        // Plenty of rounds for 1 to hand out what it would.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let Some(TypedCrdt::BoundedCounter(bc)) = a.snapshot("stock").await else { panic!("no counter") };
        assert_eq!(bc.allowance(ReplicaId(3)), 0, "allowance went to a machine with no connection");
        assert_eq!(bc.allowance(ReplicaId(2)), 15, "the connected machine did not get its share");
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    #[tokio::test]
    async fn rebalance_drains_an_idle_node_toward_the_spender() {
        use orvu_core::{TypedCrdt, TypedDelta};
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let mk = |id: u64, name: &'static str, peers: Vec<&'static str>| {
            let mut cfg = NodeConfig::new(ReplicaId(id), name);
            cfg.peers = peers.into_iter().map(PeerAddr::new).collect();
            cfg.anti_entropy_interval = std::time::Duration::from_millis(30);
            cfg.rebalance_interval = Some(std::time::Duration::from_millis(40));
            cfg
        };
        let a = Node::spawn(fabric.endpoint(1), mk(1, "a", vec!["b", "c"])).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), mk(2, "b", vec!["a", "c"])).await.unwrap();
        let c = Node::spawn(fabric.endpoint(3), mk(3, "c", vec!["a", "b"])).await.unwrap();

        // C adds to the counter and then does nothing more. B is the only one
        // spending.
        c.create("stock", TypedCrdt::BoundedCounter(Default::default())).await;
        c.mutate("stock", |crdt, _h, r| match crdt {
            TypedCrdt::BoundedCounter(bc) => TypedDelta::BoundedCounter(bc.increment(300, r)),
            other => panic!("unexpected {other:?}"),
        })
        .await;

        // An equal spread could never take C below its share of value/N, about
        // 100 here. Weighting by demand sets idle C's target to 0, so it gives
        // away everything.
        let mut drained = false;
        for _ in 0..150 {
            let _ = b
                .try_mutate("stock", |crdt, _h, r| match crdt {
                    TypedCrdt::BoundedCounter(bc) => {
                        bc.try_decrement(1, r).map(TypedDelta::BoundedCounter)
                    }
                    _ => None,
                })
                .await;
            if let Some(TypedCrdt::BoundedCounter(cc)) = c.snapshot("stock").await {
                if cc.allowance(ReplicaId(3)) == 0 && cc.value() > 0 {
                    drained = true;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(drained, "the idle funder drained toward the spender");
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    #[tokio::test]
    async fn peer_with_mismatched_identity_is_rejected() {
        let fabric = Arc::new(MemTransport::new());

        // Node A is honest (replica 1).
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        // Node B claims replica 2 in its config and `Hello`, but its transport
        // identity is replica 9, as if the endpoint were set up wrongly. A must
        // reject the connection.
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(9), cfg_b).await.unwrap(); // identity 9 != Hello 2

        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("k", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
            _ => unreachable!(),
        }).await;

        // B must not get the value. A rejects B at the handshake because B's
        // verified identity (9) does not match the replica id B claims (2). B's
        // check of A passes, since A's identity (1) matches its claim (1), but
        // the connection still fails because A has the data and refuses it.
        // Give it time and expect no value.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(value(&b, "k").await != 5, "mismatched-identity peer must not sync");

        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn a_peer_without_sync_permission_never_converges() {
        use std::collections::BTreeMap;
        let fabric = Arc::new(MemTransport::new());
        // A (replica 1) allows nobody. Replica 2 is not listed, so it is denied
        // sync.
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.acl = Acl::allow_list(BTreeMap::new());
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "k", 5).await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(value(&b, "k").await != 5, "denied-sync peer must not converge");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn an_initiator_rejects_a_peer_it_denies_sync() {
        use std::collections::BTreeMap;
        let fabric = Arc::new(MemTransport::new());
        // A (replica 1) grants B (replica 2) write but not sync, and A dials
        // out to B. This tests the sync check on the dialing side, which runs
        // after `bind_identity`. The accepting side is covered by the test
        // above. Write is granted here, so the values could only converge if
        // the sync check were skipped. B is open and only listens. It has no
        // configured peers of its own.
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        let mut map = BTreeMap::new();
        map.insert(ReplicaId(2), std::collections::BTreeSet::from([Action::Write]));
        cfg_a.acl = Acl::allow_list(map);
        cfg_a.peers = vec![PeerAddr::new("b")];
        let cfg_b = NodeConfig::new(ReplicaId(2), "b");

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        b.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&b, "k", 5).await;

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(value(&a, "k").await != 5, "initiator must not converge with a peer it denies sync");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn a_read_only_peer_converges_one_way() {
        use std::collections::{BTreeMap, BTreeSet};
        let fabric = Arc::new(MemTransport::new());
        // A (replica 1) grants replica 2 sync but not write.
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        let mut map = BTreeMap::new();
        map.insert(ReplicaId(2), BTreeSet::from([Action::Sync]));
        cfg_a.acl = Acl::allow_list(map);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "k", 7).await; // A's write, which B may read
        b.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&b, "k", 3).await; // B's write, which A must drop

        // A's write reaches B, which sees 7 or 10 depending on its own write. A
        // never sees B's +3.
        for _ in 0..100 {
            if value(&b, "k").await >= 7 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(value(&b, "k").await >= 7, "read-only peer still receives A's writes");
        assert_eq!(value(&a, "k").await, 7, "A must NOT contain the read-only peer's write");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn reader_driven_repair_converges_across_three_nodes() {
        // A and B sync through live delta streaming first, before C exists.
        // Then C joins late, connected only to B. It never connects to A and
        // never creates "k". C's connection subscribes to B's outgoing
        // broadcast only after A's delta has gone by, so streaming cannot carry
        // it. The only way C can learn "k" is B's handling of C's
        // `ContextDigest`: a store query, then a `DeltaBatch` sent to C alone.
        // So this tests catch-up done by the reader, not just relaying.
        let fabric = Arc::new(MemTransport::new());

        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);

        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("k", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 4)),
            _ => unreachable!(),
        }).await;

        // Let A and B settle through live streaming before C exists.
        for _ in 0..50 {
            if value(&b, "k").await == 4 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "k").await, 4, "A and B converged over live gossip");

        // C joins late, connected only to B. It never connects to A and never
        // creates "k". Only B's digest handling can bring it the value.
        let mut cfg_c = NodeConfig::new(ReplicaId(3), "c");
        cfg_c.peers = vec![PeerAddr::new("b")];
        cfg_c.anti_entropy_interval = Duration::from_millis(30);
        let c = Node::spawn(fabric.endpoint(3), cfg_c).await.unwrap();

        let mut got = -1;
        for _ in 0..200 {
            got = value(&c, "k").await;
            if got == 4 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(got, 4, "C converged through B's reader-driven anti-entropy path");

        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    #[tokio::test]
    async fn late_joiner_catches_up_via_anti_entropy() {
        let fabric = Arc::new(MemTransport::new());

        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 9)),
            _ => unreachable!(),
        }).await;

        // B joins after A has already made its change, so only catch-up can
        // bring it up to date.
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;

        for _ in 0..100 {
            if value(&b, "c").await == 9 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "c").await, 9, "anti-entropy back-filled the late joiner");
    }

    #[tokio::test]
    async fn late_joiner_discovers_unknown_object_via_anti_entropy() {
        let fabric = Arc::new(MemTransport::new());

        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 9)),
            _ => unreachable!(),
        }).await;

        // B joins later and does not create "c". It has to find out about the
        // object through catch-up alone.
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        for _ in 0..150 {
            if value(&b, "c").await == 9 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "c").await, 9, "B discovered and converged on an object it never created");
    }

    #[tokio::test]
    async fn change_events_fire_on_local_and_remote_mutations() {
        let fabric = Arc::new(MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;

        let mut a_changes = a.subscribe();
        let mut b_changes = b.subscribe();

        a.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 1)),
            _ => unreachable!(),
        }).await;

        // The local change event.
        assert_eq!(a_changes.recv().await.unwrap(), "c");
        // The remote change event, once the delta reaches B.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), b_changes.recv()).await.unwrap().unwrap(),
            "c"
        );
    }

    #[tokio::test]
    async fn a_panicking_mutate_does_not_kill_the_node() {
        let fabric = Arc::new(MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let node = Node::spawn(fabric.endpoint(1), cfg).await.unwrap();
        node.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;

        // A closure that panics must not take the node down. `mutate` returns
        // `None`.
        let panicked = node.mutate("c", |_, _, _| panic!("boom")).await;
        assert!(panicked.is_none(), "panicking mutate yields None");

        // The node is still alive and a normal mutation still works.
        node.mutate("c", |crdt, _hlc, _replica| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 5)),
            _ => unreachable!(),
        }).await;
        assert_eq!(value(&node, "c").await, 5, "node survived the panic and still mutates");
    }

    #[tokio::test]
    async fn shutdown_stops_the_engine() {
        let fabric = Arc::new(MemTransport::new());
        let node = Node::spawn(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "solo"))
            .await
            .unwrap();
        node.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        node.shutdown().await;
        // The engine has stopped, so a mutation no longer produces a delta.
        let produced = node
            .mutate("c", |crdt, _hlc, replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, 1)),
                _ => unreachable!(),
            })
            .await;
        assert!(produced.is_none(), "no mutation after shutdown");
    }

    // Many changes are waiting to be taken in by a busy engine when the node
    // is shut down. The shutdown does not wait behind them: the ones not yet
    // taken in are let go, unanswered, as they would be a moment later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn changes_still_waiting_to_get_in_do_not_hold_up_a_shutdown() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const WAITING: usize = 5000;
        let fabric = Arc::new(MemTransport::new());
        let node = Arc::new(Node::spawn(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "solo")).await.unwrap());
        node.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        let applied = Arc::new(AtomicUsize::new(0));
        let changes: Vec<_> = (0..WAITING)
            .map(|_| {
                let (node, applied) = (node.clone(), applied.clone());
                tokio::spawn(async move {
                    let made = node
                        .mutate("c", |crdt, _hlc, replica| match crdt {
                            // A change that takes the engine a while.
                            TypedCrdt::PnCounter(pn) => {
                                std::thread::sleep(std::time::Duration::from_micros(200));
                                TypedDelta::PnCounter(pn.increment(replica, 1))
                            }
                            _ => unreachable!(),
                        })
                        .await;
                    if made.is_some() {
                        applied.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        // Wait until the engine's queue is full, so that the rest are waiting
        // to get in.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while node.engine.capacity() > 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        node.shutdown().await;
        for change in changes {
            change.await.unwrap();
        }
        let applied = applied.load(Ordering::Relaxed);
        assert!(applied < WAITING / 2, "{applied} of {WAITING} changes were made: the shutdown waited for those still waiting to get in");
    }

    // A node whose save failed under `Always` still holds that change in
    // memory. It must stop: no read shows the change, and no other machine
    // gets it, not even through catch-up.
    #[tokio::test]
    async fn a_node_that_cannot_save_shows_its_unsaved_change_to_nobody() {
        let fabric = Arc::new(MemTransport::new());
        let quick = |cfg: &mut NodeConfig| {
            cfg.delta_batch = Duration::from_millis(5);
            cfg.anti_entropy_interval = Duration::from_millis(20);
        };
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.fsync = orvu_store::FsyncPolicy::Always;
        quick(&mut cfg_a);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        quick(&mut cfg_b);
        let store = orvu_store::MemStore::new();
        let a = Node::spawn_with_store(fabric.endpoint(1), cfg_a, Box::new(store.clone())).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "c", 1).await;
        for _ in 0..100 {
            if value(&b, "c").await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(value(&b, "c").await, 1, "the saved change never arrived");

        store.fail_saving(true);
        let unsaved = a
            .mutate("c", |crdt, _hlc, replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, 10)),
                _ => unreachable!(),
            })
            .await;
        assert!(unsaved.is_none(), "a change that was not saved was reported as done");
        assert!(a.storage_failure().is_some());
        tokio::time::timeout(Duration::from_secs(1), a.saving_failed()).await.expect("saving_failed never finished");

        assert_eq!(a.snapshot("c").await, None, "a copy was given after saving failed");
        assert_eq!(a.read("c", |_| ()).await, None, "a read was answered after saving failed");

        // Even once the disk works again, nothing more comes out of this node
        // until it is restarted: not the change, and not even word that it
        // has it. Catch-up tells other machines what this node has seen. Had
        // that reached `b`, `b` would count the unsaved change as seen by
        // every member, though after a restart this node no longer has it.
        store.fail_saving(false);
        for _ in 0..30 {
            let told = b.stable_frontier().await.get("c").map(|vv| vv.get(ReplicaId(1))).unwrap_or(0);
            assert!(told <= 1, "another machine heard that this node has the unsaved change");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(value(&b, "c").await, 1, "the unsaved change reached another machine");
        a.shutdown().await;
        b.shutdown().await;
    }

    // A node that can no longer save leaves the others: it closes its
    // connections and turns new ones away. Otherwise the others keep it as a
    // live member whose view never moves, and nothing can count as seen by
    // every member any more, so their logs only grow.
    #[tokio::test]
    async fn a_node_that_cannot_save_leaves_and_the_others_carry_on() {
        let fabric = Arc::new(MemTransport::new());
        let quick = |cfg: &mut NodeConfig| {
            cfg.delta_batch = Duration::from_millis(5);
            cfg.anti_entropy_interval = Duration::from_millis(20);
            cfg.suspect_timeout = Duration::from_millis(100);
            cfg.dead_timeout = Duration::from_millis(100);
        };
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b"), PeerAddr::new("c")];
        cfg_a.fsync = orvu_store::FsyncPolicy::Always;
        quick(&mut cfg_a);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a"), PeerAddr::new("c")];
        quick(&mut cfg_b);
        let mut cfg_c = NodeConfig::new(ReplicaId(3), "c");
        quick(&mut cfg_c);
        let store = orvu_store::MemStore::new();
        let a = Node::spawn_with_store(fabric.endpoint(1), cfg_a, Box::new(store.clone())).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        let c = Node::spawn(fabric.endpoint(3), cfg_c).await.unwrap();

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "c", 1).await;
        for _ in 0..200 {
            if value(&b, "c").await == 1 && value(&c, "c").await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!((value(&b, "c").await, value(&c, "c").await), (1, 1), "the first change never arrived");

        store.fail_saving(true);
        increment(&a, "c", 10).await;
        assert!(a.storage_failure().is_some());

        // The others drop it from their members, and a change made on one of
        // them comes to count as seen by everyone left.
        increment(&b, "c", 1).await;
        let mut left = false;
        let mut stable = false;
        for _ in 0..300 {
            left = !b.known_members().await.contains(&ReplicaId(1))
                && !c.known_members().await.contains(&ReplicaId(1));
            stable = b.stable_frontier().await.get("c").map(|vv| vv.get(ReplicaId(2))).unwrap_or(0) >= 1;
            if left && stable {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(left, "the node that cannot save is still counted as a member");
        assert!(stable, "the frontier stopped moving for the nodes that still work");

        // A machine that dials it now is turned away.
        let mut cfg_d = NodeConfig::new(ReplicaId(4), "d");
        cfg_d.peers = vec![PeerAddr::new("a")];
        quick(&mut cfg_d);
        let d = Node::spawn(fabric.endpoint(4), cfg_d).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(d.metrics().peers_up, 0, "a node that cannot save took a new connection");
        assert_eq!(a.metrics().peers_up, 0, "a node that cannot save kept a connection");

        for n in [a, b, c, d] {
            n.shutdown().await;
        }
    }

    // Changes made just before a clean shutdown still reach a connected
    // machine: they are saved, then sent, and only then is the connection
    // closed. Here every timer that would send them sooner is an hour long.
    #[tokio::test]
    async fn changes_made_just_before_shutdown_reach_a_connected_peer() {
        let dir = tempfile::tempdir().unwrap();
        let fabric = Arc::new(MemTransport::new());
        let slow = |cfg: &mut NodeConfig| {
            cfg.delta_batch = Duration::from_secs(3600);
            cfg.anti_entropy_interval = Duration::from_secs(3600);
        };
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.store_path = Some(dir.path().join("a.redb"));
        cfg_a.fsync = orvu_store::FsyncPolicy::Interval { ms: 3_600_000 };
        slow(&mut cfg_a);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        slow(&mut cfg_b);
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        for _ in 0..100 {
            if a.metrics().peers_up == 1 && b.metrics().peers_up == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(a.metrics().peers_up, 1, "the two nodes never connected");

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "c", 5).await;
        a.shutdown().await;

        let mut got = -1;
        for _ in 0..100 {
            got = value(&b, "c").await;
            if got == 5 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(got, 5, "a change made just before shutdown never reached the peer");
        b.shutdown().await;
    }

    // More changes wait at shutdown than the queue to the connections has
    // room for. They are handed over a few at a time, as the connections
    // take them, so none is left behind.
    #[tokio::test]
    async fn changes_beyond_the_queue_size_still_reach_a_peer_at_shutdown() {
        const MADE: i64 = 50;
        let dir = tempfile::tempdir().unwrap();
        let fabric = Arc::new(MemTransport::new());
        let slow = |cfg: &mut NodeConfig| {
            cfg.delta_batch = Duration::from_secs(3600);
            cfg.anti_entropy_interval = Duration::from_secs(3600);
            cfg.ship_queue_cap = 4;
        };
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.store_path = Some(dir.path().join("a.redb"));
        cfg_a.fsync = orvu_store::FsyncPolicy::Interval { ms: 3_600_000 };
        slow(&mut cfg_a);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        slow(&mut cfg_b);
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        for _ in 0..100 {
            if a.metrics().peers_up == 1 && b.metrics().peers_up == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(a.metrics().peers_up, 1, "the two nodes never connected");

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        for _ in 0..MADE {
            increment(&a, "c", 1).await;
        }
        a.shutdown().await;

        let mut got = -1;
        for _ in 0..100 {
            got = value(&b, "c").await;
            if got == MADE {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(got, MADE, "changes made before shutdown were left behind");
        b.shutdown().await;
    }

    #[tokio::test]
    async fn a_node_recovers_counter_state_from_disk() {
        use orvu_store::FsyncPolicy;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.redb");

        let mut cfg = NodeConfig::new(ReplicaId(1), "solo");
        cfg.store_path = Some(path.clone());
        cfg.fsync = FsyncPolicy::Always; // durable before the mutate returns

        let fabric = Arc::new(MemTransport::new());
        let node = Node::spawn(fabric.endpoint(1), cfg.clone()).await.unwrap();
        node.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        node.mutate("c", |crdt, _hlc, r| match crdt {
            TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, 5)),
            _ => unreachable!(),
        }).await;
        node.shutdown().await; // flush and stop
        drop(node);

        // Start again on the same path with a new transport, so there are no
        // other machines and no network. The only way to see 5 is from disk.
        let fabric2 = Arc::new(MemTransport::new());
        let node2 = Node::spawn(fabric2.endpoint(1), cfg).await.unwrap();
        assert_eq!(value(&node2, "c").await, 5, "state recovered from disk alone");
    }

    #[tokio::test]
    async fn a_node_recovers_multi_object_state_after_compaction() {
        use orvu_store::FsyncPolicy;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.redb");

        let mut cfg = NodeConfig::new(ReplicaId(1), "solo");
        cfg.store_path = Some(path.clone());
        cfg.fsync = FsyncPolicy::Always;
        cfg.compaction_threshold = 3; // force at least one compaction below

        {
            let fabric = Arc::new(MemTransport::new());
            let node = Node::spawn(fabric.endpoint(1), cfg.clone()).await.unwrap();
            node.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
            node.create("s", TypedCrdt::OrSet(orvu_crdt::OrSet::default())).await;
            // Compaction swaps the stored log for a snapshot. With no other
            // machines the frontier is empty, so the compaction at the 3rd
            // mutate (log length equals the threshold) does nothing. The 6th
            // reaches the threshold again with a log of 6, which is more than
            // 3. That triggers the fallback that folds everything into a
            // snapshot and cuts the log.
            node.mutate("c", |crdt, _h, r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, 5)),
                _ => unreachable!(),
            }).await;
            node.mutate("c", |crdt, _h, r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, 2)),
                _ => unreachable!(),
            }).await;
            node.mutate("s", |crdt, _h, r| match crdt {
                TypedCrdt::OrSet(set) => TypedDelta::OrSet(set.add("x".to_string(), r).unwrap()),
                _ => unreachable!(),
            }).await;
            node.mutate("s", |crdt, _h, r| match crdt {
                TypedCrdt::OrSet(set) => TypedDelta::OrSet(set.add("y".to_string(), r).unwrap()),
                _ => unreachable!(),
            }).await;
            node.mutate("s", |crdt, _h, r| match crdt {
                TypedCrdt::OrSet(set) => TypedDelta::OrSet(set.add("z".to_string(), r).unwrap()),
                _ => unreachable!(),
            }).await;
            node.mutate("s", |crdt, _h, r| match crdt {
                TypedCrdt::OrSet(set) => TypedDelta::OrSet(set.add("w".to_string(), r).unwrap()),
                _ => unreachable!(),
            }).await;
            node.shutdown().await; // flush, stop and release the file lock
        }

        // Check that compaction really ran: a snapshot is on disk and the delta
        // log was cut short. Otherwise this would only test replaying a full
        // log.
        {
            use orvu_store::Store;
            let store = orvu_store::RedbStore::open(&path).unwrap();
            assert!(!store.load_snapshot().unwrap().is_empty(), "a snapshot was written to disk");
            assert!(store.replay().unwrap().len() < 6, "the delta log was truncated by compaction");
        } // drop the direct store handle before node2 reopens the path

        // Start again on the same path with a new transport (no other machines,
        // no network).
        let fabric2 = Arc::new(MemTransport::new());
        let node2 = Node::spawn(fabric2.endpoint(1), cfg).await.unwrap();
        assert_eq!(value(&node2, "c").await, 7, "counter recovered across a compaction");
        match node2.snapshot("s").await {
            Some(TypedCrdt::OrSet(set)) => {
                assert!(set.contains(&"x".to_string()) && set.contains(&"y".to_string()),
                    "set recovered across a compaction");
            }
            other => panic!("expected a set, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovered_compacted_node_serves_a_snapshot_to_a_far_behind_peer() {
        use orvu_store::FsyncPolicy;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.redb");
        let fabric = Arc::new(MemTransport::new());

        // First, A stores its changes and compacts, then shuts down, which
        // releases the lock.
        {
            let mut cfg = NodeConfig::new(ReplicaId(1), "a");
            cfg.store_path = Some(path.clone());
            cfg.fsync = FsyncPolicy::Always;
            cfg.compaction_threshold = 3; // compaction fires within these 3 mutations
            let a = Node::spawn(fabric.endpoint(1), cfg).await.unwrap();
            a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
            for by in [5u64, 2, 3] {
                a.mutate("c", move |crdt, _h, r| match crdt {
                    TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, by)),
                    _ => unreachable!(),
                }).await;
            }
            a.shutdown().await;
        }

        // Next, start A again from the compacted store. It has no deltas left
        // after the snapshot, so a machine that is far behind can only be
        // repaired with a snapshot. That only works if the record of what the
        // stored snapshot covers is loaded on startup.
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.store_path = Some(path.clone());
        cfg_a.fsync = FsyncPolicy::Always;
        cfg_a.compaction_threshold = 3;
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        let a2 = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();

        // B joins new and far behind, and never created "c". Only a snapshot
        // can bring it the state and let it find out about the object.
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        for _ in 0..150 {
            if value(&b, "c").await == 10 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "c").await, 10, "recovered+compacted node served a snapshot to the far-behind peer");

        a2.shutdown().await;
    }

    #[tokio::test]
    async fn far_behind_peer_catches_up_via_snapshot_transfer() {
        use orvu_store::FsyncPolicy;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.redb");
        let fabric = Arc::new(MemTransport::new());

        // A stores its changes with a very low compaction threshold, so its
        // early deltas are folded into a snapshot on disk before B connects. A
        // machine that is behind that snapshot can only be repaired by being
        // sent a snapshot.
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        cfg_a.store_path = Some(path);
        cfg_a.fsync = FsyncPolicy::Always;
        cfg_a.compaction_threshold = 2; // compaction fires within the mutations below
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();

        a.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        a.create("s", TypedCrdt::OrSet(orvu_crdt::OrSet::default())).await;
        for by in [1u64, 2, 3, 4] {
            a.mutate("c", move |crdt, _h, r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, by)),
                _ => unreachable!(),
            }).await;
        }
        a.mutate("s", |crdt, _h, r| match crdt {
            TypedCrdt::OrSet(set) => TypedDelta::OrSet(set.add("x".to_string(), r).unwrap()),
            _ => unreachable!(),
        }).await;
        a.mutate("s", |crdt, _h, r| match crdt {
            TypedCrdt::OrSet(set) => TypedDelta::OrSet(set.add("y".to_string(), r).unwrap()),
            _ => unreachable!(),
        }).await;

        // B joins only now, far behind. It should reach the full counter (10)
        // and set ({x,y}). A's early state is in the snapshot, so catch-up has
        // to go through the snapshot transfer.
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        for _ in 0..150 {
            if value(&b, "c").await == 10 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "c").await, 10, "counter caught up from the compacted store");
        match b.snapshot("s").await {
            Some(TypedCrdt::OrSet(set)) => assert!(
                set.contains(&"x".to_string()) && set.contains(&"y".to_string()),
                "set caught up from the compacted store",
            ),
            other => panic!("expected a set, got {other:?}"),
        }

        a.shutdown().await;
    }

    #[tokio::test]
    async fn frontier_spans_indirect_peers_via_gossip() {
        // A line A(1)-B(2)-C(3): A and C each connect only to B and never to
        // each other.
        let fabric = Arc::new(MemTransport::new());
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        let mut cfg_c = NodeConfig::new(ReplicaId(3), "c");
        cfg_c.peers = vec![PeerAddr::new("b")];
        // Faster gossip for the test.
        for c in [&mut cfg_a, &mut cfg_b, &mut cfg_c] {
            c.anti_entropy_interval = Duration::from_millis(30);
        }

        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let c = Node::spawn(fabric.endpoint(3), cfg_c).await.unwrap();

        c.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&c, "k", 5).await;

        // A learns that C is a member through B's gossip, though A never
        // connected to C.
        let mut ok = false;
        for _ in 0..100 {
            if a.known_members().await.contains(&ReplicaId(3)) {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok, "A must learn indirect member C via gossip");
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    #[tokio::test]
    async fn dropped_direct_peer_is_suspected_then_evicted() {
        // A(1) and B(2) sync, then B crashes. A suspects B as soon as the
        // connection drops and removes it after `dead_timeout`, so log cleanup
        // can move forward again.
        //
        // `suspect_timeout` is set very high (10s) so the ordinary route, where
        // a member goes from Alive to Suspect after `suspect_timeout` of
        // silence, cannot happen inside the polling window below. With the
        // quick route, B's disconnect (its connection count reaching 0) marks
        // it Suspect at once, and B is removed about `dead_timeout` (120ms)
        // later, well inside the window. Without the quick route B would stay
        // Alive for 10s, far longer than the polling loop, and the assertion
        // would fail. So this test checks the quick route in the `PeerDown`
        // handling specifically.
        let fabric = Arc::new(MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        cfg_a.suspect_timeout = Duration::from_secs(10);
        cfg_a.dead_timeout = Duration::from_millis(120);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "k", 5).await;
        for _ in 0..100 {
            if value(&b, "k").await == 5 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "k").await, 5, "A and B converged");

        let mut knows = false;
        for _ in 0..100 {
            if a.known_members().await.contains(&ReplicaId(2)) { knows = true; break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(knows, "A knows the live member B");

        b.shutdown().await;

        let mut evicted = false;
        for _ in 0..200 {
            if !a.known_members().await.contains(&ReplicaId(2)) { evicted = true; break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(evicted, "crashed direct peer B must be evicted after dead_timeout");
        a.shutdown().await;
    }

    #[tokio::test]
    async fn a_live_indirect_member_is_not_falsely_evicted() {
        // In a line A(1)-B(2)-C(3), a live member C that A only knows through B
        // must stay in A's view across many suspect and dead windows. C's
        // gossiped heartbeat reaches A through B and keeps its entry fresh. The
        // rule that a higher heartbeat clears suspicion has its own unit test,
        // `frontier::a_higher_heartbeat_refutes_suspicion`.
        let fabric = Arc::new(MemTransport::new());
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        let mut cfg_c = NodeConfig::new(ReplicaId(3), "c");
        cfg_c.peers = vec![PeerAddr::new("b")];
        for c in [&mut cfg_a, &mut cfg_b, &mut cfg_c] {
            c.anti_entropy_interval = Duration::from_millis(30);
            c.suspect_timeout = Duration::from_millis(60);
            c.dead_timeout = Duration::from_millis(60);
        }
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let c = Node::spawn(fabric.endpoint(3), cfg_c).await.unwrap();

        c.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&c, "k", 5).await;

        // A learns about C through B.
        let mut ok = false;
        for _ in 0..100 {
            if a.known_members().await.contains(&ReplicaId(3)) { ok = true; break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok, "A learns indirect member C");

        // Across several suspect and dead windows C stays known, because its
        // heartbeat clears any suspicion.
        for _ in 0..15 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                a.known_members().await.contains(&ReplicaId(3)),
                "a live indirect member must never be falsely evicted",
            );
        }
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    #[tokio::test]
    async fn a_restarted_member_is_taken_back_at_once_by_those_who_only_hear_of_it() {
        // A line A(1)-B(2)-C(3). C knows A only through B. A runs for a while,
        // stops, is dropped by both, and starts again under the same id. C
        // must take it back within a few rounds, not after A has been up as
        // long as it was the first time.
        let fabric = Arc::new(MemTransport::new());
        let round = Duration::from_millis(20);
        let config = |id: u64, name: &str, peers: &[&str]| {
            let mut c = NodeConfig::new(ReplicaId(id), name);
            c.peers = peers.iter().map(|p| PeerAddr::new(*p)).collect();
            c.anti_entropy_interval = round;
            c.suspect_timeout = Duration::from_millis(60);
            c.dead_timeout = Duration::from_millis(60);
            c
        };
        let b = Node::spawn(fabric.endpoint(2), config(2, "b", &[])).await.unwrap();
        let c = Node::spawn(fabric.endpoint(3), config(3, "c", &["b"])).await.unwrap();
        let a = Node::spawn(fabric.endpoint(1), config(1, "a", &["b"])).await.unwrap();

        let knows_a = || async { c.known_members().await.contains(&ReplicaId(1)) };
        let mut ok = false;
        for _ in 0..100 {
            if knows_a().await { ok = true; break; }
            tokio::time::sleep(round).await;
        }
        assert!(ok, "C learns of A through B");
        // Let A build up a long run of heartbeats.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(knows_a().await, "C still knows A while it runs");

        a.shutdown().await;
        drop(a);
        let mut dropped = false;
        for _ in 0..100 {
            if !knows_a().await { dropped = true; break; }
            tokio::time::sleep(round).await;
        }
        assert!(dropped, "C drops A once it stops");

        let a = Node::spawn(fabric.endpoint(1), config(1, "a", &["b"])).await.unwrap();
        let mut back = false;
        for _ in 0..15 {
            if knows_a().await { back = true; break; }
            tokio::time::sleep(round).await;
        }
        assert!(back, "C did not take the restarted A back within a few rounds");
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    }

    #[tokio::test]
    async fn metrics_count_convergence_and_membership() {
        let fabric = Arc::new(MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "k", 5).await;
        for _ in 0..100 {
            if value(&b, "k").await == 5 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "k").await, 5);

        // B applied A's write as a delta from another machine. A knows B as a
        // connected machine.
        let mb = b.metrics();
        assert!(mb.deltas_applied_remote >= 1, "B applied a remote delta");
        let ma = a.metrics();
        assert!(ma.deltas_applied_local >= 1, "A applied its own local write");
        // Latency histograms are recorded on both the writing side and the
        // receiving side.
        assert!(ma.mutate_latency.count >= 1, "A timed at least one local mutate");
        assert!(mb.remote_apply_latency.count >= 1, "B timed at least one remote apply");
        assert_eq!(ma.objects, 1, "A has one object");
        for _ in 0..100 {
            if a.metrics().peers_up == 1 && a.metrics().members_known >= 1 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(a.metrics().peers_up, 1, "A has one peer up");
        assert!(a.metrics().members_known >= 1);

        // A quick check that the Prometheus output renders.
        let text = crate::render_prometheus(&a.metrics());
        assert!(text.contains("orvu_peers_up"));
        assert!(text.contains("orvu_deltas_applied_remote"));

        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn metrics_count_suspicion_and_eviction() {
        let fabric = Arc::new(MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        cfg_a.suspect_timeout = Duration::from_secs(10); // so the quick route is the only one that can fire
        cfg_a.dead_timeout = Duration::from_millis(120);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&a, "k", 1).await;
        for _ in 0..100 {
            if a.metrics().peers_up == 1 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        b.shutdown().await;

        // The `PeerDown` quick route suspects B at once, and removal follows
        // after `dead_timeout`. With its only connection gone, A's catch-up
        // loop for that connection has ended, so nothing sends `MembershipView`
        // any more. The loop below calls `known_members` to run the sweep, as
        // `dropped_direct_peer_is_suspected_then_evicted` does.
        let mut ok = false;
        for _ in 0..200 {
            let _ = a.known_members().await;
            let m = a.metrics();
            if m.members_suspected >= 1 && m.members_evicted >= 1 { ok = true; break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok, "dropping B must bump members_suspected and members_evicted");
        a.shutdown().await;
    }

    #[tokio::test]
    async fn metrics_count_unauthorized_write_drops() {
        // A grants B sync but not write, so B is read-only. B's write is
        // dropped and counted, and never applied.
        use std::collections::{BTreeMap, BTreeSet};
        let fabric = Arc::new(MemTransport::new());
        let mut acl_map = BTreeMap::new();
        acl_map.insert(ReplicaId(2), BTreeSet::from([Action::Sync]));
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        cfg_a.acl = Acl::allow_list(acl_map);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);

        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();
        // A holds an empty "k", so `value(&a, "k")` reads 0 unless B's write
        // gets in.
        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        b.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        increment(&b, "k", 3).await;

        let mut ok = false;
        for _ in 0..150 {
            if a.metrics().writes_dropped_unauthorized >= 1 { ok = true; break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok, "A must count B's unauthorized write as dropped");
        assert_eq!(value(&a, "k").await, 0, "the dropped write is never applied");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn metrics_count_antientropy_and_snapshots() {
        // A late joiner that is far behind syncs through a snapshot. Check that
        // the counters move.
        let fabric = Arc::new(MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.anti_entropy_interval = Duration::from_millis(30);
        cfg_a.compaction_threshold = 3; // low, so a compaction runs and a new machine needs a snapshot
        let a = Node::spawn(fabric.endpoint(1), cfg_a).await.unwrap();
        a.create("k", TypedCrdt::PnCounter(PnCounter::default())).await;
        for _ in 0..10 { increment(&a, "k", 1).await; }

        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = Duration::from_millis(30);
        let b = Node::spawn(fabric.endpoint(2), cfg_b).await.unwrap();

        for _ in 0..150 {
            if value(&b, "k").await == 10 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(value(&b, "k").await, 10, "B converged");

        assert!(a.metrics().antientropy_rounds >= 1, "A ran anti-entropy rounds");
        assert!(b.metrics().antientropy_rounds >= 1, "B ran anti-entropy rounds");
        assert!(a.metrics().snapshots_sent >= 1, "A sent B a snapshot");
        assert!(b.metrics().snapshots_received >= 1, "B received a snapshot");
        assert!(a.metrics().snapshot_bytes.count >= 1, "A recorded a snapshot size sample");
        assert!(a.metrics().snapshot_bytes.sum > 0.0, "the recorded snapshot payload is non-empty");
        assert!(a.metrics().antientropy_round.count >= 1, "A timed at least one anti-entropy round");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn frontier_compaction_bounds_the_tail_and_recovers() {
        use orvu_store::FsyncPolicy;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.redb");
        let mut cfg = NodeConfig::new(ReplicaId(1), "solo");
        cfg.store_path = Some(path.clone());
        cfg.fsync = FsyncPolicy::Always;
        cfg.compaction_threshold = 2; // low, so compaction and its fallback run repeatedly

        let fabric = Arc::new(MemTransport::new());
        let node = Node::spawn(fabric.endpoint(1), cfg.clone()).await.unwrap();
        node.create("c", TypedCrdt::PnCounter(PnCounter::default())).await;
        for by in [1u64, 2, 3, 4, 5, 6] {
            node.mutate("c", move |crdt, _h, r| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(r, by)),
                _ => unreachable!(),
            }).await;
        }

        // With no other machines the frontier is empty, so deltas only leave
        // the stored log through the fallback once it grows past the cap. The
        // log should stay well below the 6 mutations, which shows compaction
        // ran and kept it short.
        let mut tail = node.durable_tail_len().await;
        for _ in 0..100 {
            if tail <= 4 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            tail = node.durable_tail_len().await;
        }
        assert!(tail <= 4, "frontier-compaction backstop bounds the tail (got {tail})");
        node.shutdown().await;
        drop(node);

        // Recovery after the compaction: the full value survives (1+2+3+4+5+6 =
        // 21).
        let fabric2 = Arc::new(MemTransport::new());
        let node2 = Node::spawn(fabric2.endpoint(1), cfg).await.unwrap();
        assert_eq!(value(&node2, "c").await, 21, "counter recovered across frontier compaction");
        node2.shutdown().await;
    }
}
