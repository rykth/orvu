//! Turns an SDK `watch()` stream into a gRPC server stream, which is one call
//! that keeps sending messages to the client.
//!
//! `watch_object` in the SDK already does the three things that matter. It
//! sends the current value as soon as you subscribe. It skips a value equal to
//! the last one it sent. If the watcher falls behind and misses changes (a
//! `Lagged` error), it reads the object again. So this module only forwards.
//!
//! A broken stream does no harm. `watch` sends the object's value, not the
//! individual changes, so a client that reconnects gets the current value
//! again. A dropped stream can miss values in between. It cannot miss the
//! value that all machines end up with.
//!
//! The same rule as `Execute` applies: an engine answer (a wrong kind, a class
//! mismatch, a node that can no longer save) is a `Failed` event, never a
//! `Status`. A watch whose node can no longer save ends with a `Failed` event
//! that says so. A `Status` is only for the
//! envelope: an unknown session (checked by the caller), a `kind` that
//! cannot be used, or too many watches open already.
//!
//! A watch ends with `UNAUTHENTICATED` once its session is gone, so a
//! session's watches do not outlive it. Each value a watch sends counts as
//! using its session, so a watcher whose object keeps changing is not the
//! session used longest ago. A quiet watch does not count, and its session
//! can be dropped to make room like any other.

use crate::exec::{json_value, nodes_value, spans_value};
use crate::{errors, pb};
use orvu_sdk::Client;
use futures::StreamExt;
use std::pin::Pin;
use crate::session::SessionState;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

/// The stream of events that a `Watch` call returns.
pub type WatchStream =
    Pin<Box<dyn futures::Stream<Item = Result<pb::WatchEvent, Status>> + Send>>;

const CHANNEL: usize = 64;

/// The most watches one session may have open at once. Each holds a task and
/// a queue, so a program that kept opening them would otherwise use memory
/// without end.
pub const MAX_WATCHES_PER_SESSION: usize = 256;

/// The most watches the sidecar has open at once, over all sessions, on the
/// Unix socket and the network together.
pub const MAX_WATCHES: usize = 4096;

/// How many watches are open in this process. The Unix socket and the
/// network each have a service of their own, and both count here, so that
/// together they stay within `MAX_WATCHES`.
static OPEN: std::sync::LazyLock<Arc<AtomicUsize>> = std::sync::LazyLock::new(Arc::default);

/// The count of the watches open in this process, shared by every service.
pub fn open_in_process() -> Arc<AtomicUsize> {
    OPEN.clone()
}

/// A place taken among the open watches. It is given back when dropped.
pub struct Slot {
    overall: Arc<AtomicUsize>,
    session: Arc<SessionState>,
}

impl Slot {
    /// Takes a place, if both the sidecar and the session have one left.
    /// Otherwise the watch is refused with `RESOURCE_EXHAUSTED`.
    pub fn take(
        overall: Arc<AtomicUsize>,
        overall_limit: usize,
        session: Arc<SessionState>,
        session_limit: usize,
    ) -> Result<Slot, Status> {
        let take = |count: &AtomicUsize, limit: usize| {
            count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < limit).then_some(n + 1)).is_ok()
        };
        if !take(&session.watches, session_limit) {
            return Err(Status::resource_exhausted(format!(
                "too many watches open on this session: the most is {session_limit}; end one first"
            )));
        }
        if !take(&overall, overall_limit) {
            session.watches.fetch_sub(1, Ordering::AcqRel);
            return Err(Status::resource_exhausted(format!(
                "too many watches open on this sidecar: the most is {overall_limit}; end one first"
            )));
        }
        Ok(Slot { overall, session })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.overall.fetch_sub(1, Ordering::AcqRel);
        self.session.watches.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Whether the watcher is still let in. A watch asks before each value it
/// sends, and once a second while nothing changes, and ends with the
/// `Status` it gives. It is told `true` when a value is about to be sent,
/// which counts as using the session, and `false` when the watch is only
/// waiting, which does not.
pub type Admitted = Arc<dyn Fn(bool) -> Result<(), Status> + Send + Sync>;

/// How often a watch on an object that does not change asks whether its
/// watcher is still let in.
const RECHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// Starts a watch. Returns the stream and a `Weak` to a marker that the
/// forwarding task holds for as long as it runs, so a test can check that the
/// task ends when the subscriber leaves.
pub async fn subscribe(
    client: &Client,
    req: pb::WatchRequest,
) -> Result<(WatchStream, Weak<()>), Status> {
    subscribe_admitted(client, req, Arc::new(|_| Ok(()))).await
}

/// Like `subscribe`, and ends the watch once `admitted` says no.
pub async fn subscribe_admitted(
    client: &Client,
    req: pb::WatchRequest,
    admitted: Admitted,
) -> Result<(WatchStream, Weak<()>), Status> {
    let kind = match pb::Kind::try_from(req.kind) {
        Ok(pb::Kind::Unspecified) | Err(_) => {
            return Err(Status::invalid_argument("watch: kind must be a known kind"))
        }
        Ok(k) => k,
    };
    let (tx, rx) = mpsc::channel::<Result<pb::WatchEvent, Status>>(CHANNEL);
    let guard = Arc::new(());
    let weak = Arc::downgrade(&guard);
    let object = req.object;
    let ended_by = client.clone();

    // When `closed()` wins the select, the pending `stream.next()` is dropped,
    // and nothing is lost by that. The stream is the SDK's
    // `futures::stream::unfold`, which keeps its unfinished work (with the
    // broadcast receiver and the last value sent) inside the stream itself,
    // and `next()` only borrows the stream. We also break on that branch, so
    // the stream is never polled again anyway.
    //
    // There is one branch per kind. Each maps its item through the same
    // helpers that Execute's reads use, so a watch and a read cannot disagree
    // about shape.
    macro_rules! forward {
        ($handle:expr, $map:expr) => {{
            match $handle {
                Ok(handle) => {
                    let mut stream = Box::pin(handle.watch());
                    tokio::spawn(async move {
                        let _alive = guard;
                        let mut recheck = tokio::time::interval(RECHECK);
                        loop {
                            tokio::select! {
                                // The subscriber left. End now and not at the
                                // next write, so an abandoned watch on an
                                // object nobody changes cannot keep a task
                                // waiting forever.
                                _ = tx.closed() => break,
                                // A watcher that is no longer let in, such
                                // as an agent that was removed, is told so
                                // and the watch ends.
                                _ = recheck.tick() => if let Err(refused) = admitted(false) {
                                    let _ = tx.send(Err(refused)).await;
                                    break;
                                },
                                item = stream.next() => match item {
                                    Some(v) => {
                                        if let Err(refused) = admitted(true) {
                                            let _ = tx.send(Err(refused)).await;
                                            break;
                                        }
                                        // The receiver may have been dropped
                                        // between the select and the send.
                                        if tx.send(Ok(event($map(v)))).await.is_err() {
                                            break;
                                        }
                                    }
                                    // The stream ended. If that is because
                                    // the node can no longer save, the
                                    // watcher is told why.
                                    None => {
                                        if let Some(why) = ended_by.storage_failure() {
                                            let stopped = errors::to_response(orvu_sdk::SdkError::Storage(why));
                                            let _ = tx.send(Ok(event(stopped))).await;
                                        }
                                        break;
                                    }
                                },
                            }
                        }
                    });
                }
                Err(e) => {
                    // An engine answer. It is the first and only event. The
                    // channel is empty, so this send cannot block. No task is
                    // started, so the returned `Weak` is already dead, and a
                    // test that checks the task ended would pass on this path
                    // without proving anything. `tx` and `guard` are dropped
                    // at the end of `subscribe`, after `rx` has been handed to
                    // the `ReceiverStream`. So the event can still be
                    // delivered, and the stream then ends.
                    let _ = tx.send(Ok(event(errors::to_response(e)))).await;
                }
            }
        }};
    }

    // An object that already exists is opened in the usual way, which checks
    // its type. One that does not exist yet is not created: the watch gets a
    // handle that stays quiet until an object of that type appears. Creating
    // it here would fix the name to this type before anyone had written it.
    let exists = client.exists(&object).await;
    macro_rules! open {
        ($method:ident, $handle:ident) => {
            if exists {
                client.$method(&object).await
            } else {
                Ok(client.attach::<orvu_sdk::$handle>(&object))
            }
        };
    }

    match kind {
        // Already rejected above. This returns a status and does not panic,
        // because a panic while handling a request takes down the
        // connection's task.
        pb::Kind::Unspecified => {
            return Err(Status::invalid_argument("watch: kind must be a known kind"))
        }
        pb::Kind::Counter => forward!(open!(counter, Counter), errors::int),
        pb::Kind::Bounded => forward!(open!(bounded_counter, BoundedCounter), errors::int),
        pb::Kind::Register => forward!(open!(register, Register), errors::opt_string),
        pb::Kind::Set => forward!(open!(set, Set), errors::strings),
        pb::Kind::List => forward!(open!(list, List), errors::strings),
        pb::Kind::Map => forward!(open!(map, Map), errors::pairs),
        pb::Kind::Cart => forward!(open!(cart, Cart), errors::counted),
        pb::Kind::Json => forward!(open!(json, Json), json_value),
        pb::Kind::Richtext => forward!(open!(rich_text, RichText), spans_value),
        pb::Kind::Tree => forward!(open!(tree, Tree), nodes_value),
        pb::Kind::MvMap => forward!(open!(mv_map, MvMap), errors::multi_pairs),
    }

    Ok((Box::pin(ReceiverStream::new(rx)), weak))
}

/// The shape helpers build a `Response`, but a `WatchEvent` carries the
/// `ReadValue` or `Failed` that sits inside one. Unwrapping it here lets reads
/// and watches share one set of shape helpers.
fn event(r: pb::Response) -> pb::WatchEvent {
    use pb::response::Result as R;
    let event = match r.result {
        Some(R::Value(v)) => pb::watch_event::Event::Value(v),
        Some(R::Failed(f)) => pb::watch_event::Event::Failed(f),
        // No watch path produces any other variant. If one ever does, it is
        // reported as a failed event and not dropped.
        // `to_response` can also give `Refused`. `WatchEvent`'s oneof (a field
        // that holds exactly one of its listed cases) cannot carry that, so it
        // would turn into `Failed{ENGINE}` here. That cannot happen as the code
        // stands: a handle is only obtained through `Client::open`, which fails
        // with `ClassMismatch`, `TypeMismatch` or `Engine` and never with
        // `Exhausted`. If `open` ever can return `Exhausted`, `WatchEvent`
        // needs a variant for it.
        other => return event(errors::engine_failed(format!("watch: unexpected response {other:?}"))),
    };
    pb::WatchEvent { event: Some(event) }
}
