//! The gRPC service and the Unix socket it listens on.

use crate::agents::{Agents, Refusal};
use crate::audit::{Audit, Entry, Outcome};
use crate::rules::Rules;
use crate::search::Searcher;
use crate::pb::{self, data_plane_server::DataPlane};
use crate::requests::{Answer, Begun, Call as Sent, Promise, Requests};
use crate::session::{SessionState, Sessions};
use crate::stats::{op_index, Call, Ended, Stats};
use orvu_crdt::ReplicaId;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The gRPC service. It answers the four calls Open, Execute, Batch and Watch.
pub struct Dataplane {
    client: Arc<orvu_sdk::Client>,
    /// The engine's replica id. `orvu_sdk::Client` has no way to ask for it,
    /// so whoever built the engine passes it in.
    replica: ReplicaId,
    /// The one namespace this engine serves, from the same config the engine
    /// was built from. It is passed in for the same reason as `replica`.
    namespace: String,
    sessions: Arc<Sessions>,
    /// How many watches are open, over all sessions. It is one count for the
    /// whole process, shared with the other service.
    watches: Arc<std::sync::atomic::AtomicUsize>,
    /// The most watches one session, and the sidecar, may have open.
    watch_limits: (usize, usize),
    /// Who is let in.
    agents: Arc<Agents>,
    /// The record of who changed what.
    audit: Arc<Audit>,
    /// Lets in only agents with keys, whatever the agents file says. For
    /// programs that come over the network.
    keys_only: bool,
    /// What is counted for the page of numbers.
    stats: Arc<Stats>,
    /// Search by meaning.
    searcher: Arc<Searcher>,
    /// Fires once the sidecar starts stopping. From then on no new work is
    /// taken, a batch runs no more of its operations, and open watches end.
    stopping: tokio_util::sync::CancellationToken,
    /// The answers given to calls that carried a request id.
    requests: Arc<Requests>,
}

/// What a call with a request id is to do.
enum Recalled {
    /// Its id is known: this is the answer it was given.
    Answered(Answer),
    /// It is to be made, and its answer kept, if it has an id.
    Make(Option<Promise>),
}

impl Dataplane {
    pub fn new(client: Arc<orvu_sdk::Client>, replica: ReplicaId, namespace: &str) -> Self {
        let requests = Requests::new(
            receipts_of(&client),
            crate::requests::MOST_PER_AGENT,
            crate::requests::KEEP,
            crate::requests::MOST_BYTES_PER_AGENT,
        );
        Self {
            requests: Arc::new(requests),
            client,
            replica,
            namespace: namespace.to_string(),
            sessions: Arc::new(Sessions::new()),
            watches: crate::watch::open_in_process(),
            watch_limits: (crate::watch::MAX_WATCHES_PER_SESSION, crate::watch::MAX_WATCHES),
            agents: Arc::new(Agents::open_to_all()),
            audit: Arc::new(Audit::off()),
            keys_only: false,
            stats: Arc::default(),
            searcher: Arc::new(Searcher::off()),
            stopping: Default::default(),
        }
    }

    /// Answers searches with `searcher`.
    pub fn searching(mut self, searcher: Arc<Searcher>) -> Self {
        self.searcher = searcher;
        self
    }

    /// Lets in only the agents in `agents`, once it names any.
    pub fn admitting(mut self, agents: Arc<Agents>) -> Self {
        self.agents = agents;
        self
    }

    /// Lets in only agents with keys, and nobody while the agents file names
    /// none.
    pub fn keys_only(mut self) -> Self {
        self.keys_only = true;
        self
    }

    /// Records who changed what in `audit`.
    pub fn recording(mut self, audit: Arc<Audit>) -> Self {
        self.audit = audit;
        self
    }

    /// Whether a session's agent is still let in, and the rules it is held to
    /// now. Over the network, only an agent with a key ever is.
    ///
    /// A session whose agent is turned away, because it was removed or its
    /// key was replaced, is ended here. Otherwise whoever still holds an old
    /// key could keep calling on its sessions and keep them in use. When the
    /// agents file cannot be read, or was changed only a moment ago and may
    /// still be half-written, the call is turned away but the session stays,
    /// since that may pass.
    fn still_admits(
        agents: &Agents,
        keys_only: bool,
        sessions: &Sessions,
        id: &str,
        session: &SessionState,
    ) -> Result<Option<Rules>, Status> {
        let admitted = match (keys_only, session.agent.as_ref()) {
            (true, Some(agent)) => agents.still_admits_with_key(agent),
            (true, None) => Err(Refusal::KeysOnly),
            (false, agent) => agents.still_admits(agent),
        };
        if let Err(refusal) = &admitted {
            if !matches!(refusal, Refusal::Unreadable(_)) && agents.settled() {
                sessions.end(id);
            }
        }
        admitted.map_err(refused)
    }

    /// The name of the agent a session belongs to, if it has one. Asking
    /// does not count as using the session.
    fn agent_of(&self, session: &str) -> Option<String> {
        self.sessions.peek(session)?.agent.as_ref().map(|agent| agent.name.clone())
    }

    /// Records `entry` as not allowed, if `status` says it was not.
    async fn not_allowed(&self, entry: Entry, agent: Option<&str>, status: &Status) {
        if status.code() == tonic::Code::PermissionDenied {
            self.audit.record(entry, agent, Outcome::NotAllowed(status.message())).await;
        }
    }

    // For tests only: fewer sessions, so that dropping one is cheap to test.
    #[doc(hidden)]
    pub fn session_cap(mut self, cap: usize) -> Self {
        self.sessions = Arc::new(Sessions::with_cap(cap));
        self
    }

    // For tests only: fewer watches, so that reaching the limits is cheap to
    // test. The watches are counted apart from the rest of the process, so
    // that tests running at the same time do not use up each other's.
    #[doc(hidden)]
    pub fn watch_limits(mut self, per_session: usize, overall: usize) -> Self {
        self.watch_limits = (per_session, overall);
        self.watches = Arc::default();
        self
    }

    // For tests only: fewer request ids remembered, or for less time, so
    // that reaching the limits is cheap to test.
    #[doc(hidden)]
    pub fn remembering(mut self, most: usize, keep: std::time::Duration, most_bytes: usize) -> Self {
        self.requests = Arc::new(Requests::new(receipts_of(&self.client), most, keep, most_bytes));
        self
    }

    // For tests only: the bytes of answers kept in memory.
    #[doc(hidden)]
    pub fn remembered_bytes(&self) -> usize {
        self.requests.bytes()
    }

    /// The replies to a batch sent again whose answer is kept: the kept
    /// answers of its changes, and its reads read again, now.
    async fn read_again(&self, req: &pb::BatchRequest, parts: Vec<Option<pb::Response>>) -> Result<Vec<pb::Response>, Status> {
        let reads: Vec<pb::Request> =
            req.requests.iter().zip(&parts).filter(|(_, kept)| kept.is_none()).map(|(one, _)| one.clone()).collect();
        for op in reads.iter().filter_map(|one| one.op.as_ref()) {
            self.searcher.moves().before(&self.client, crate::rules::object(op)).await.map_err(Status::internal)?;
        }
        let mut read = crate::exec::dispatch_batch_guarded(&self.client, &req.session, reads, self.stopping.clone(), Vec::new(), Vec::new())
            .await?
            .into_iter();
        Ok(parts.into_iter().map(|kept| kept.unwrap_or_else(|| read.next().unwrap_or_default())).collect())
    }

    /// Looks up the request id `id` of a call by the session's agent. A call
    /// with an id that is still being made waits for that one's answer.
    async fn recall(&self, session: &SessionState, call: Sent, id: &[u8], writes: bool) -> Result<Recalled, Status> {
        if id.len() > crate::requests::MOST_ID_BYTES {
            return Err(Status::invalid_argument(format!(
                "a request id may be at most {} bytes, and this one is {}; nothing was applied",
                crate::requests::MOST_ID_BYTES,
                id.len()
            )));
        }
        // A call that changes nothing can be made again as it is: its id is
        // not used, and nothing is kept of what it read.
        if id.is_empty() || !writes {
            return Ok(Recalled::Make(None));
        }
        let agent = session.agent.as_ref().map(|agent| agent.name.as_str()).unwrap_or_default();
        loop {
            match self.requests.begin(agent, call, id) {
                Begun::New(promise) => return Ok(Recalled::Make(Some(promise))),
                // A call with the same id that ended with no answer is made
                // again.
                Begun::Known(slot) => {
                    if let Some(answer) = crate::requests::answer(slot).await {
                        return Ok(Recalled::Answered(answer));
                    }
                }
            }
        }
    }

    /// Takes no new work once `stopping` fires.
    pub fn stops_with(mut self, stopping: tokio_util::sync::CancellationToken) -> Self {
        self.stopping = stopping;
        self
    }

    /// Refuses new work once the sidecar is stopping.
    fn not_stopping(&self) -> Result<(), Status> {
        not_stopping(&self.stopping)
    }

    /// Counts the calls this answers into `stats`.
    pub fn counting(mut self, stats: Arc<Stats>) -> Self {
        self.stats = stats;
        self
    }

    /// Opens a watch, and counts it.
    async fn subscribe(&self, req: pb::WatchRequest) -> Result<(crate::watch::WatchStream, std::sync::Weak<()>), Status> {
        let (session_id, object) = (req.session.clone(), req.object.clone());
        // Who asked, for the record, taken before a refusal can end the
        // session.
        let asker = self.agent_of(&session_id);
        let opened = match self.not_stopping().and_then(|()| self.session(&req.session)) {
            // The rules are asked again here, and while the watch is open, so
            // a watch an agent may no longer read ends too.
            Ok((session, _)) => {
                let (agents, keys_only, object) = (self.agents.clone(), self.keys_only, req.object.clone());
                let (sessions, id) = (self.sessions.clone(), session_id.clone());
                let watched = session.clone();
                let check = move || {
                    let rules = Self::still_admits(&agents, keys_only, &sessions, &id, &watched)?;
                    may_watch(&watched, rules.as_ref(), &object)
                };
                let slot = || {
                    let (per_session, overall) = self.watch_limits;
                    crate::watch::Slot::take(self.watches.clone(), overall, session.clone(), per_session)
                };
                match check().and_then(|()| slot()) {
                    Ok(slot) => {
                        // A watch that ends because the agent may no longer
                        // read is recorded, as a refusal when it opens is.
                        let (audit, agent, object) = (self.audit.clone(), asker.clone(), req.object.clone());
                        let (sessions, id, stopping) = (self.sessions.clone(), session_id.clone(), self.stopping.clone());
                        // The place among the open watches is held for as
                        // long as the watch can still be asked about, which
                        // is for as long as it runs.
                        let admitted: crate::watch::Admitted = Arc::new(move |sending| {
                            let _held = &slot;
                            // A watch would otherwise hold the stop up for as
                            // long as it is open.
                            not_stopping(&stopping)?;
                            // A session that was dropped takes its watches
                            // with it.
                            let closed = || Status::unauthenticated("the session was closed; call Open again");
                            if !sessions.contains(&id) {
                                return Err(closed());
                            }
                            check().inspect_err(|refusal| {
                                let entry = Entry::call("watch", Some(&object));
                                audit.record_soon(entry, agent.clone(), refusal.message().to_string());
                            })?;
                            // Sending a value counts as using the session,
                            // once its agent is known to be still let in.
                            // Only waiting for one does not.
                            if sending && sessions.get(&id).is_none() {
                                return Err(closed());
                            }
                            Ok(())
                        });
                        self.searcher.moves().before(&self.client, &req.object).await.map_err(Status::internal)?;
                        crate::watch::subscribe_admitted(&self.client, req, admitted).await
                    }
                    Err(refusal) => Err(refusal),
                }
            }
            Err(e) => Err(e),
        };
        match &opened {
            Ok(_) => self.stats.watch(),
            Err(_) => self.stats.rejected(Call::Watch),
        }
        if let Err(status) = &opened {
            let entry = Entry::call("watch", Some(&object));
            self.not_allowed(entry, asker.as_deref(), status).await;
        }
        opened
    }

    /// The engine's replica id. Clients are told it so that a program in
    /// another language, such as Python, can name the replica it writes
    /// through (`bounded_transfer` needs one).
    fn replica_id(&self) -> u64 {
        self.replica.0
    }

    /// The session, if it exists and its agent is still let in, with the
    /// rules its agent is held to now, if any. An agent that was removed is
    /// turned away here, at its next call, and so is a program that came in
    /// without a key before the first agent was added.
    ///
    /// The session is marked as used only after its agent is found to be
    /// still let in, so a call that is turned away never keeps it in use.
    fn session(&self, id: &str) -> Result<(Arc<SessionState>, Option<Rules>), Status> {
        let unknown = || Status::unauthenticated("unknown session; call Open first");
        let session = self.sessions.peek(id).ok_or_else(unknown)?;
        let rules = Self::still_admits(&self.agents, self.keys_only, &self.sessions, id, &session)?;
        self.sessions.get(id).ok_or_else(unknown)?;
        Ok((session, rules))
    }

    // For tests only. The tests call the service directly and not over a
    // socket, so a connection problem cannot be mistaken for a logic bug.
    #[doc(hidden)]
    pub async fn open_for_test(&self, r: pb::OpenRequest) -> Result<pb::OpenResponse, Status> {
        self.open(Request::new(r)).await.map(|r| r.into_inner())
    }
    #[doc(hidden)]
    pub async fn execute_for_test(&self, r: pb::Request) -> Result<pb::Response, Status> {
        self.execute(Request::new(r)).await.map(|r| r.into_inner())
    }
    #[doc(hidden)]
    pub async fn watch_for_test(
        &self,
        r: pb::WatchRequest,
    ) -> Result<(crate::watch::WatchStream, std::sync::Weak<()>), Status> {
        self.subscribe(r).await
    }
    #[doc(hidden)]
    pub async fn batch_for_test(&self, r: pb::BatchRequest) -> Result<pb::BatchResponse, Status> {
        self.batch(Request::new(r)).await.map(|r| r.into_inner())
    }
}

/// Whether `req` changes something. Only such a call's request id is used.
fn writes(req: &pb::Request) -> bool {
    req.op.as_ref().and_then(Entry::of).is_some()
}

/// Whether `req` is made as two changes, each with a receipt of its own.
fn makes_two_changes(req: &pb::Request) -> bool {
    req.op.as_ref().is_some_and(crate::exec::makes_two_changes)
}

/// Asks the engine whether a change was made under a receipt id.
fn receipts_of(client: &Arc<orvu_sdk::Client>) -> crate::requests::Made {
    let client = client.clone();
    Arc::new(move |id: &[u8; 16]| client.receipt_made(id))
}

/// Whether the session's agent may run `req`, by its rules. With no rules it
/// may run anything but what the machines keep for themselves. A request
/// with no operation is left to the envelope check, which turns it away.
fn may(session: &SessionState, rules: Option<&Rules>, req: &pb::Request) -> Result<(), Status> {
    if let Some(op) = &req.op {
        crate::rules::not_reserved(crate::rules::object(op)).map_err(Status::permission_denied)?;
    }
    match (rules, session.agent.as_ref(), req.op.as_ref()) {
        (Some(rules), Some(agent), Some(op)) => {
            crate::rules::check(rules, &agent.name, op).map_err(Status::permission_denied)
        }
        _ => Ok(()),
    }
}

/// Whether the session's agent may make the operations of one request
/// together, by its rules, where one goes only with another. `Err` gives the
/// number of the one it may not make.
fn may_together(session: &SessionState, rules: Option<&Rules>, reqs: &[&pb::Request]) -> Result<(), (usize, Status)> {
    match (rules, session.agent.as_ref()) {
        (Some(_), Some(agent)) => {
            let ops: Vec<Option<&pb::request::Op>> = reqs.iter().map(|one| one.op.as_ref()).collect();
            crate::rules::check_together(&agent.name, &ops).map_err(|(i, why)| (i, Status::permission_denied(why)))
        }
        _ => Ok(()),
    }
}

/// Whether the session's agent may watch `object`, by its rules.
fn may_watch(session: &SessionState, rules: Option<&Rules>, object: &str) -> Result<(), Status> {
    crate::rules::not_reserved(object).map_err(Status::permission_denied)?;
    match (rules, session.agent.as_ref()) {
        (Some(rules), Some(agent)) => {
            crate::rules::check_watch(rules, &agent.name, object).map_err(Status::permission_denied)
        }
        _ => Ok(()),
    }
}

/// `UNAVAILABLE` once `stopping` has fired: the sidecar takes no new work
/// while it stops. A client takes it as it takes a sidecar that is not there.
fn not_stopping(stopping: &tokio_util::sync::CancellationToken) -> Result<(), Status> {
    match stopping.is_cancelled() {
        true => Err(Status::unavailable("the sidecar is stopping")),
        false => Ok(()),
    }
}

/// A program that is not let in. It is PERMISSION_DENIED, never
/// UNAUTHENTICATED: a client answers that by opening a new session and trying
/// again, which would go round for ever.
fn refused(why: Refusal) -> Status {
    Status::permission_denied(why.to_string())
}

#[tonic::async_trait]
impl DataPlane for Dataplane {
    async fn open(&self, req: Request<pb::OpenRequest>) -> Result<Response<pb::OpenResponse>, Status> {
        let req = req.into_inner();
        // One engine means one namespace, and nothing after this point looks
        // at the field: `exec` passes an op's `object` to the engine as it
        // arrives. So a namespace that is not ours is rejected. Serving our
        // own namespace in its place would leave the client believing it
        // works in a namespace it does not, and a session's namespace would
        // mean nothing.
        //
        // This is an envelope failure, so it is a gRPC status. Nothing reached
        // the engine, and asking for the wrong namespace is a bug in the
        // client, not an engine answer. Empty means "whatever this sidecar
        // serves".
        if !req.namespace.is_empty() && req.namespace != self.namespace {
            return Err(Status::invalid_argument(format!(
                "this sidecar serves namespace {:?}, not {:?}",
                self.namespace, req.namespace
            )));
        }
        let admitted = if self.keys_only {
            self.agents.admit_with_key(&req.credential).map(Some)
        } else {
            self.agents.admit(&req.credential)
        }
        .map_err(refused);
        let agent = match admitted {
            Ok(agent) => agent,
            Err(status) => {
                self.not_allowed(Entry::call("open", None), None, &status).await;
                return Err(status);
            }
        };
        let name = agent.as_ref().map(|agent| agent.name.clone()).unwrap_or_default();
        // The namespace is stored as it was sent, so an empty one stays
        // empty. A resume token then only brings back a client that opens the
        // same way it did the first time, with the same key.
        let (session, resume_token) = self.sessions.open(&req.namespace, &req.resume_token, agent);
        Ok(Response::new(pb::OpenResponse {
            session,
            resume_token,
            replica_id: self.replica_id(),
            version: crate::VERSION.to_string(),
            agent: name,
            pid: std::process::id(),
        }))
    }

    async fn execute(&self, req: Request<pb::Request>) -> Result<Response<pb::Response>, Status> {
        let req = req.into_inner();
        let started = std::time::Instant::now();
        let index = req.op.as_ref().map(op_index);
        let id = req.session.clone();
        let change = req.op.as_ref().and_then(Entry::of);
        // What a refusal is recorded as: the change, or the read.
        let asked = change.clone().or_else(|| req.op.as_ref().map(|op| Entry::call("read", Some(crate::rules::object(op)))));
        let asker = self.agent_of(&id);
        let answered = match self.not_stopping().and_then(|()| self.session(&req.session)) {
            Ok((session, rules)) => match may(&session, rules.as_ref(), &req)
                .and_then(|()| may_together(&session, rules.as_ref(), &[&req]).map_err(|(_, refusal)| refusal))
            {
                Ok(()) => match self.recall(&session, Sent::One, &req.request_id, writes(&req)).await {
                Err(refusal) => Err(refusal),
                Ok(Recalled::Answered(Answer::One(response))) => Ok(response),
                Ok(Recalled::Answered(Answer::Batch(_))) => Err(Status::internal("a single call was remembered as a batch")),
                Ok(Recalled::Make(promise)) => {
                    // The change is made and recorded by a task of its own,
                    // which goes on if the caller stops waiting, so that a
                    // change that was made is always recorded.
                    let (client, audit, searcher) = (self.client.clone(), self.audit.clone(), self.searcher.clone());
                    let agent = session.agent.as_ref().map(|agent| agent.name.clone());
                    let (recorded, by) = (change.clone(), agent.clone());
                    let reader = agent.clone();
                    // Whose the tasks and notes it changes are is read just
                    // before it runs, and again as each write is made.
                    let guarding = rules.clone().zip(agent.clone());
                    let run = tokio::spawn(async move {
                        // A change saved before the sidecar last started is
                        // not made again.
                        if promise.as_ref().is_some_and(|promise| promise.saved(0)) && !makes_two_changes(&req) {
                            let made = crate::errors::ack();
                            if let Some(promise) = promise {
                                promise.keep(Answer::One(made.clone()));
                            }
                            return Ok(made);
                        }
                        // A search finds only what the agent may read now, by
                        // the rules every read goes by.
                        let may = move |object: &str| match (&rules, &reader) {
                            (Some(rules), Some(agent)) => crate::rules::check_watch(rules, agent, object).is_ok(),
                            _ => true,
                        };
                        // A team's old notes are moved over before its notes
                        // are first used.
                        if let Some(op) = &req.op {
                            searcher.moves().before(&client, crate::rules::object(op)).await.map_err(Status::internal)?;
                        }
                        let guard = match (&guarding, &req.op) {
                            (Some((rules, agent)), Some(op)) => crate::rules::guards(&client, rules, agent, &[op])
                                .await
                                .map_err(|(_, why)| Status::permission_denied(why))?
                                .pop()
                                .flatten(),
                            _ => None,
                        };
                        let dispatched = crate::exec::dispatch_guarded(&client, &searcher, may, req, guard);
                        // A change made for a call with an id is saved with
                        // its receipt.
                        let answered = match &promise {
                            Some(promise) => orvu_sdk::with_receipt(promise.receipt(0), dispatched).await,
                            None => dispatched.await,
                        };
                        if let (Ok(response), Some(change)) = (&answered, recorded) {
                            audit.record(change, by.as_deref(), Outcome::Answered(response)).await;
                        }
                        if let Some(promise) = promise {
                            if let Ok(response) = &answered {
                                promise.keep(Answer::One(response.clone()));
                            }
                            crate::requests::stop_here_if_asked("answer");
                        }
                        answered
                    });
                    match run.await {
                        Ok(answered) => answered,
                        Err(e) => {
                            if let Some(change) = change {
                                self.audit.record(change, agent.as_deref(), Outcome::Broke).await;
                            }
                            Err(Status::internal(format!("the request stopped unexpectedly: {e}")))
                        }
                    }
                }
                },
                Err(refusal) => Err(refusal),
            },
            Err(e) => Err(e),
        };
        if let (Err(status), Some(asked)) = (&answered, asked) {
            self.not_allowed(asked, asker.as_deref(), status).await;
        }
        self.stats.call(Call::Execute, started.elapsed());
        match (&answered, index) {
            (Ok(response), Some(index)) => self.stats.operation(index, Ended::of(response)),
            (Ok(_), None) => {}
            (Err(_), _) => self.stats.rejected(Call::Execute),
        }
        answered.map(Response::new)
    }

    async fn batch(&self, req: Request<pb::BatchRequest>) -> Result<Response<pb::BatchResponse>, Status> {
        let req = req.into_inner();
        let started = std::time::Instant::now();
        // A batch that is too large is turned away whole, so its operations
        // are not looked at.
        let indexes: Vec<_> = if req.requests.len() <= crate::exec::MAX_BATCH_OPS {
            req.requests.iter().map(|r| r.op.as_ref().map(op_index)).collect()
        } else {
            Vec::new()
        };
        let id = req.session.clone();
        // The one request of the batch the agent was not allowed, if any.
        let mut denied = None;
        let asker = self.agent_of(&id);
        let answered = match self.not_stopping().and_then(|()| self.session(&req.session)) {
            // A batch with one operation the agent may not run is turned away
            // whole, before any of it runs.
            Ok((session, rules)) => match req
                .requests
                .iter()
                .enumerate()
                .try_for_each(|(i, one)| {
                    may(&session, rules.as_ref(), one).map_err(|refusal| {
                        denied = one.op.as_ref().map(|op| {
                            Entry::of(op).unwrap_or_else(|| Entry::call("read", Some(crate::rules::object(op))))
                        });
                        Status::permission_denied(format!("batch request {i}: {}; nothing was applied", refusal.message()))
                    })
                })
                .and_then(|()| {
                    let all: Vec<&pb::Request> = req.requests.iter().collect();
                    may_together(&session, rules.as_ref(), &all).map_err(|(i, refusal)| {
                        denied = req.requests[i].op.as_ref().and_then(Entry::of);
                        Status::permission_denied(format!("batch request {i}: {}; nothing was applied", refusal.message()))
                    })
                }) {
                Ok(()) => match self.recall(&session, Sent::Batch, &req.request_id, req.requests.iter().any(writes)).await {
                Err(refusal) => Err(refusal),
                Ok(Recalled::Answered(Answer::Batch(parts))) => self.read_again(&req, parts).await,
                Ok(Recalled::Answered(Answer::One(_))) => Err(Status::internal("a batch was remembered as a single call")),
                Ok(Recalled::Make(promise)) => {
                    // As for one request, the batch is run and recorded by a
                    // task of its own, which goes on if the caller stops
                    // waiting.
                    // For a batch with an id, the receipt of each part, and
                    // whether it was saved before the sidecar last started.
                    let receipts: Vec<Option<([u8; 16], bool)>> = (0..req.requests.len())
                        .map(|i| promise.as_ref().map(|promise| (promise.receipt(i as u32), promise.saved(i as u32))))
                        .collect();
                    // A part that makes two changes is looked at again, so
                    // that the second is made if only the first was.
                    let receipts: Vec<Option<([u8; 16], bool)>> = receipts
                        .into_iter()
                        .zip(&req.requests)
                        .map(|(receipt, one)| receipt.map(|(id, saved)| (id, saved && !makes_two_changes(one))))
                        .collect();
                    let made_before: Vec<bool> = receipts.iter().map(|receipt| receipt.is_some_and(|(_, saved)| saved)).collect();
                    let wrote: Vec<bool> = req.requests.iter().map(writes).collect();
                    let changes: Vec<_> = req
                        .requests
                        .iter()
                        .zip(&made_before)
                        .map(|(one, made)| if *made { None } else { one.op.as_ref().and_then(Entry::of) })
                        .collect();
                    let (client, audit, moves) = (self.client.clone(), self.audit.clone(), self.searcher.moves().clone());
                    let stopping = self.stopping.clone();
                    let agent = session.agent.as_ref().map(|agent| agent.name.clone());
                    let guarding = rules.clone().zip(agent.clone());
                    let run = tokio::spawn(async move {
                        // A batch whose every part was saved before the
                        // sidecar last started is answered as made.
                        if !made_before.is_empty() && made_before.iter().all(|made| *made) {
                            let made = vec![crate::errors::ack(); made_before.len()];
                            if let Some(promise) = promise {
                                promise.keep(Answer::Batch(made.iter().cloned().map(Some).collect()));
                            }
                            return Ok(made);
                        }
                        for op in req.requests.iter().filter_map(|one| one.op.as_ref()) {
                            moves.before(&client, crate::rules::object(op)).await.map_err(Status::internal)?;
                        }
                        // The whole batch is looked at at once, so that one
                        // part cannot make another look like the agent's own.
                        let guards = match &guarding {
                            Some((rules, agent)) => {
                                let ops: Vec<&pb::request::Op> = req.requests.iter().filter_map(|one| one.op.as_ref()).collect();
                                if ops.len() == req.requests.len() {
                                    crate::rules::guards(&client, rules, agent, &ops).await.map_err(|(i, why)| {
                                        Status::permission_denied(format!("batch request {i}: {why}; nothing was applied"))
                                    })?
                                } else {
                                    Vec::new()
                                }
                            }
                            None => Vec::new(),
                        };
                        let answered = crate::exec::dispatch_batch_guarded(
                            &client,
                            &req.session,
                            req.requests,
                            stopping,
                            guards,
                            receipts,
                        )
                        .await;
                        match &answered {
                            Ok(responses) => {
                                for (change, response) in changes.into_iter().zip(responses) {
                                    if let Some(change) = change {
                                        audit.record(change, agent.as_deref(), Outcome::Answered(response)).await;
                                    }
                                }
                            }
                            // The batch broke partway: some of it may have
                            // been made.
                            Err(status) if status.code() == tonic::Code::Internal => {
                                for change in changes.into_iter().flatten() {
                                    audit.record(change, agent.as_deref(), Outcome::Broke).await;
                                }
                            }
                            // Turned away before any of it ran.
                            Err(_) => {}
                        }
                        if let Some(promise) = promise {
                            // Only the answers of changes are kept: reads are
                            // read again when the batch is sent again.
                            if let Ok(responses) = &answered {
                                let kept = responses.iter().zip(&wrote).map(|(one, wrote)| wrote.then(|| one.clone())).collect();
                                promise.keep(Answer::Batch(kept));
                            }
                            crate::requests::stop_here_if_asked("answer");
                        }
                        answered
                    });
                    run.await.unwrap_or_else(|e| Err(Status::internal(format!("the batch stopped unexpectedly: {e}"))))
                }
                },
                Err(refusal) => Err(refusal),
            },
            Err(e) => Err(e),
        };
        if let Err(status) = &answered {
            let entry = denied.unwrap_or_else(|| Entry::call("batch", None));
            self.not_allowed(entry, asker.as_deref(), status).await;
        }
        self.stats.call(Call::Batch, started.elapsed());
        match &answered {
            Ok(responses) => {
                for (index, response) in indexes.iter().zip(responses) {
                    if let Some(index) = index {
                        self.stats.operation(*index, Ended::of(response));
                    }
                }
            }
            // A batch is taken whole or not at all.
            Err(_) => self.stats.rejected(Call::Batch),
        }
        Ok(Response::new(pb::BatchResponse { responses: answered? }))
    }

    type WatchStream = crate::watch::WatchStream;
    async fn watch(&self, req: Request<pb::WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        let (stream, _guard) = self.subscribe(req.into_inner()).await?;
        Ok(Response::new(stream))
    }
}

/// Serves programs that come over the network on `listener`, with every
/// connection encrypted by `acceptor`, until `shutdown`. Only agents with
/// keys are let in. It shares the engine, the agents, the record and what is
/// counted with the Unix socket; its sessions are its own.
#[allow(clippy::too_many_arguments)]
pub async fn serve_network(
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    client: Arc<orvu_sdk::Client>,
    replica: ReplicaId,
    namespace: &str,
    connections: Connections,
    stats: Arc<Stats>,
    agents: Arc<Agents>,
    audit: Arc<Audit>,
    searcher: Arc<Searcher>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> std::io::Result<()> {
    let stopping = tokio_util::sync::CancellationToken::new();
    let dataplane = Dataplane::new(client, replica, namespace)
        .counting(stats)
        .admitting(agents)
        .recording(audit)
        .searching(searcher)
        .keys_only()
        .stops_with(stopping.clone());
    let shutdown = stops(shutdown, stopping);
    // A connection whose other end has gone without a word, as a machine that
    // was switched off does, is found by asking it every half a minute, and
    // closed if it does not answer within ten seconds. It would otherwise
    // hold its place among the open connections for good.
    server()
        .http2_keepalive_interval(Some(std::time::Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(std::time::Duration::from_secs(10)))
        .add_service(data_plane(dataplane, MAX_NETWORK_REQUEST_BYTES))
        .serve_with_incoming_shutdown(crate::network::incoming(listener, acceptor, connections), shutdown)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// A Unix socket (also called a Unix domain socket, UDS) that is in place at
/// its path and ready to be served. Dropping it removes the socket file again,
/// so no file is left behind when serving ends or startup fails later on.
pub struct BoundSocket {
    listener: tokio::net::UnixListener,
    file: SocketFile,
}

/// The socket file on disk. Dropping this removes the file.
struct SocketFile {
    path: PathBuf,
    /// The device and inode of the socket file we put at `path`. The file is
    /// only removed if these still match, so we never delete a file somebody
    /// else put there afterwards.
    file: (u64, u64),
}

/// The socket file, moved into a directory of its own for a moment.
struct Aside {
    dir: PathBuf,
    staged: PathBuf,
}

impl Aside {
    fn remove(self) {
        unstage(&self.dir, &self.staged);
    }
}

impl SocketFile {
    /// Moves the socket file out of the way, into a new `0700` directory next
    /// to it, so that no more clients can find it. `None` if the file at the
    /// path is not ours any more, or cannot be moved; then it is left alone.
    fn move_aside(&self) -> Option<Aside> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(&self.path).ok()?;
        if (md.dev(), md.ino()) != self.file {
            return None;
        }
        let dir = staging_dir(&self.path);
        let staged = dir.join("s");
        std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
        match std::fs::rename(&self.path, &staged) {
            Ok(()) => Some(Aside { dir, staged }),
            Err(_) => {
                let _ = std::fs::remove_dir(&dir);
                None
            }
        }
    }

    /// Puts the socket file back at its path. Whatever turned up there in
    /// the meantime is left alone, as when the socket was first put in place.
    fn put_back(&self, aside: Aside) {
        if let Err(e) = install(&aside.staged, &self.path) {
            crate::say::line(format_args!("orvud: the socket could not be put back at {}: {e}", self.path.display()));
        }
        aside.remove();
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if let Ok(md) = std::fs::symlink_metadata(&self.path) {
            if (md.dev(), md.ino()) == self.file {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Puts a listening socket at `path`. Nobody can reach the socket until it is
/// in place, and from its first moment on disk only its owner can use it.
///
/// This needs no engine, so a program can call it first and find out about a
/// bad socket path before it starts anything else.
///
/// The socket is created inside a new `0700` directory next to `path`. Its mode
/// is set to `0600`, and only then is it given the name `path`, after which the
/// directory is removed. Both parts are needed. `bind` creates the socket file
/// with whatever permissions the process umask leaves. A socket created at a
/// name other processes can reach would accept connections while its
/// permissions were still too open, and a session id works like a password.
/// The directory stops other users from getting to the socket file, whatever
/// the file's own mode is. Do not replace this with a bind straight at `path`,
/// or with a neighbouring name that has no directory around it.
///
/// The name `path` is given with a hard link, which fails if anything is
/// already there. A rename would silently replace it, and that could be the
/// socket of another sidecar that started at the same moment.
///
/// If the process is killed (SIGKILL) partway through, the directory can stay
/// behind with only the socket file in it. Its name is random, so it is never
/// in the way of a later start, and the next start in the same folder removes
/// it.
pub async fn bind_uds(path: &Path) -> std::io::Result<BoundSocket> {
    use std::os::unix::fs::MetadataExt;
    let dir = staging_dir(path);
    let staged = dir.join("s");
    check_sun_path(path)?;
    check_sun_path(&staged)?;
    clear_stale_socket(path).await?;
    clear_left_staging(path);
    let listener = match stage(&dir, &staged) {
        Ok(listener) => listener,
        Err(e) => {
            unstage(&dir, &staged);
            return Err(e);
        }
    };
    let installed = install(&staged, path).and_then(|()| std::fs::symlink_metadata(path));
    // The socket now has its real name, or installing failed. Either way the
    // staged name and the directory have done their job. A failure to remove
    // them does not stop us, but the operator should hear about it.
    let _ = std::fs::remove_file(&staged);
    if let Err(e) = std::fs::remove_dir(&dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "the staging directory was left behind");
        crate::say::line(format_args!("orvud: the staging directory {} was left behind: {e}", dir.display()));
    }
    let md = installed?;
    let file = SocketFile { path: path.to_path_buf(), file: (md.dev(), md.ino()) };
    Ok(BoundSocket { listener, file })
}

/// Serves the data plane on a socket from `bind_uds` until `shutdown`
/// finishes. Requests in progress are allowed to finish. The socket file is
/// removed on the way out.
pub async fn serve_bound(
    bound: BoundSocket,
    client: Arc<orvu_sdk::Client>,
    replica: ReplicaId,
    namespace: &str,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> std::io::Result<()> {
    serve_bound_counted(bound, client, replica, namespace, Connections::default(), shutdown).await
}

/// Like `serve_bound`, and keeps `connections` up to date with how many
/// clients are connected. The caller can use that to stop when nobody is.
pub async fn serve_bound_counted(
    bound: BoundSocket,
    client: Arc<orvu_sdk::Client>,
    replica: ReplicaId,
    namespace: &str,
    connections: Connections,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> std::io::Result<()> {
    serve_bound_observed(bound, client, replica, namespace, connections, Arc::default(), shutdown).await
}

/// Like `serve_bound_counted`, and counts the calls it answers into `stats`
/// for the page of numbers.
pub async fn serve_bound_observed(
    bound: BoundSocket,
    client: Arc<orvu_sdk::Client>,
    replica: ReplicaId,
    namespace: &str,
    connections: Connections,
    stats: Arc<Stats>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> std::io::Result<()> {
    let (agents, audit) = (Arc::new(Agents::open_to_all()), Arc::new(Audit::off()));
    let searcher = Arc::new(Searcher::off());
    serve_bound_admitting(bound, client, replica, namespace, connections, stats, agents, audit, searcher, shutdown).await
}

/// Like `serve_bound_observed`, lets in only the agents in `agents`, once it
/// names any, records who changed what in `audit`, and answers searches with
/// `searcher`.
#[allow(clippy::too_many_arguments)]
pub async fn serve_bound_admitting(
    bound: BoundSocket,
    client: Arc<orvu_sdk::Client>,
    replica: ReplicaId,
    namespace: &str,
    connections: Connections,
    stats: Arc<Stats>,
    agents: Arc<Agents>,
    audit: Arc<Audit>,
    searcher: Arc<Searcher>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> std::io::Result<()> {
    let stopping = tokio_util::sync::CancellationToken::new();
    let dataplane = Dataplane::new(client, replica, namespace)
        .counting(stats)
        .admitting(agents)
        .recording(audit)
        .searching(searcher)
        .stops_with(stopping.clone());
    let svc = data_plane(dataplane, crate::exec::MAX_REQUEST_BYTES);
    let shutdown = stops(shutdown, stopping);
    // The socket is handed over to the stream of incoming connections, which
    // removes the socket file when it ends or when this function returns.
    let incoming = Incoming::new(bound, connections);
    server()
        .add_service(svc)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// The largest request the network service takes, in bytes: gRPC's usual
/// limit. A request is read in full before its key is checked, so anyone who
/// can reach the port could otherwise make the sidecar hold 64 MiB for every
/// request it sends. The Unix socket, which only its owner can reach, takes
/// up to `exec::MAX_REQUEST_BYTES`.
pub const MAX_NETWORK_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// How many calls one connection may have being read and answered at once.
/// More wait until one is answered. Together with the request size limit
/// this bounds the memory one connection can take. A watch counts only until
/// its call is answered, when it starts sending, so a connection can have
/// many more watches open than this.
pub const MAX_CALLS_PER_CONNECTION: usize = 32;

/// The gRPC service for `dataplane`, taking requests of up to `largest`
/// bytes. On the Unix socket that is `exec::MAX_REQUEST_BYTES`, as gRPC's own
/// limit, 4 MiB, is too small for a large value or a large batch of writes.
fn data_plane(dataplane: Dataplane, largest: usize) -> pb::data_plane_server::DataPlaneServer<Dataplane> {
    pb::data_plane_server::DataPlaneServer::new(dataplane).max_decoding_message_size(largest)
}

/// A server that answers no more than `MAX_CALLS_PER_CONNECTION` calls on
/// one connection at once.
fn server() -> tonic::transport::Server {
    tonic::transport::Server::builder().concurrency_limit_per_connection(MAX_CALLS_PER_CONNECTION)
}

/// `shutdown`, which also fires `stopping` when it finishes, so the service
/// stops taking work at the moment the server stops taking connections.
async fn stops(shutdown: impl std::future::Future<Output = ()>, stopping: tokio_util::sync::CancellationToken) {
    shutdown.await;
    stopping.cancel();
}

/// How long requests in progress are given to finish once the sidecar starts
/// stopping. Past that they are cut off.
pub const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Serves until `serving` finishes, but for no longer than `limit` after
/// `stopping` fires. Serving finishes once the requests in progress have been
/// answered, and one that never ends, such as a client that never finishes
/// sending, must not keep the sidecar from stopping.
pub async fn finish_within(
    serving: impl std::future::Future<Output = std::io::Result<()>>,
    stopping: tokio_util::sync::CancellationToken,
    limit: std::time::Duration,
) -> std::io::Result<()> {
    tokio::select! {
        served = serving => served,
        _ = async {
            stopping.cancelled().await;
            tokio::time::sleep(limit).await;
        } => {
            crate::say::line(format_args!(
                "orvud: requests still in progress after {}s were cut off",
                limit.as_secs()
            ));
            Ok(())
        }
    }
}

/// Why the sidecar stopped serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It was asked to: a signal, or nobody connected for long enough.
    Asked,
    /// Its engine can no longer save changes, and says why. It cannot serve
    /// anything until it is started again from what is on disk.
    CannotSave(String),
}

impl Stop {
    /// What to tell whoever runs the sidecar, or `None` when there is nothing
    /// wrong to report.
    pub fn message(&self) -> Option<String> {
        match self {
            Stop::Asked => None,
            Stop::CannotSave(why) => Some(format!(
                "cannot save changes: {why}; stopping so that it can be started again from what is on disk"
            )),
        }
    }
}

/// Finishes when `asked` does, or once `client` can no longer save, and says
/// which. A sidecar whose engine cannot save refuses every request with the
/// reason, and that never gets better on its own, so it stops: a supervisor,
/// or the library that started it, starts it again.
pub async fn until_stopped(client: &orvu_sdk::Client, asked: impl std::future::Future<Output = ()>) -> Stop {
    tokio::select! {
        _ = asked => Stop::Asked,
        _ = client.saving_failed() => Stop::CannotSave(client.storage_failure().unwrap_or_default()),
    }
}

/// How many clients are connected right now, and since when nobody has been.
#[derive(Clone)]
pub struct Connections {
    inner: Arc<std::sync::Mutex<ConnectionState>>,
}

struct ConnectionState {
    open: usize,
    /// When the count last dropped to zero, or when counting started.
    empty_since: std::time::Instant,
    /// Stop serving the socket once nobody has been connected for this long.
    stop_when_idle: Option<std::time::Duration>,
    /// Whether serving the socket stopped for that reason.
    went_idle: bool,
}

impl Default for Connections {
    fn default() -> Self {
        let state = ConnectionState {
            open: 0,
            empty_since: std::time::Instant::now(),
            stop_when_idle: None,
            went_idle: false,
        };
        Self { inner: Arc::new(std::sync::Mutex::new(state)) }
    }
}

impl Connections {
    /// How many clients are connected.
    pub fn open(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).open
    }

    pub(crate) fn enter(&self) -> OpenConnection {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).open += 1;
        OpenConnection { connections: self.clone() }
    }

    /// Makes the socket stop serving once nobody has been connected for
    /// `quiet`. A client that is connected keeps it serving for as long as
    /// it stays, whether or not it sends anything.
    ///
    /// The decision is made where connections are taken in, so a client that
    /// connects at the very moment it is made is not cut off: see `Incoming`.
    pub fn stop_when_idle(&self, quiet: std::time::Duration) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).stop_when_idle = Some(quiet);
    }

    /// Whether the socket stopped serving because nobody was connected.
    pub fn went_idle(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).went_idle
    }

    /// The quiet time asked for, if nobody is connected and it has passed.
    fn quiet_for(&self) -> Option<std::time::Duration> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let quiet = state.stop_when_idle?;
        (state.open == 0 && state.empty_since.elapsed() >= quiet).then_some(quiet)
    }

    /// The quiet time asked for, if any.
    fn idle_limit(&self) -> Option<std::time::Duration> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).stop_when_idle
    }
}

/// The connections coming in on the socket, each counted for as long as it
/// is open. When `Connections::stop_when_idle` was asked for, the stream ends
/// once nobody has been connected for that long, and serving ends with it.
///
/// Deciding that nobody is there has to allow for a client that connects at
/// that very moment. The kernel takes in a connection before the sidecar
/// accepts it, so a client may be connected, as far as it can tell, to a
/// socket that is about to be closed. So the socket file is first moved
/// aside, which stops any more clients from finding it, and every connection
/// already waiting is then accepted. If there was one, the stop is called off
/// and the socket file is put back; if not, nothing can reach the socket any
/// more, and it is closed. A client that tries in between finds no socket,
/// as it would a moment later, and has sent nothing.
struct Incoming {
    listener: Option<tokio::net::UnixListener>,
    file: SocketFile,
    connections: Connections,
    /// Connections accepted while deciding, to be handed out first.
    taken: std::collections::VecDeque<tokio::net::UnixStream>,
    /// When to look again whether nobody is connected.
    look: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    /// Run after deciding that nobody is there and before the socket file
    /// is moved aside. For tests, which connect there.
    before_closing: Option<Box<dyn FnMut() + Send>>,
}

impl Incoming {
    fn new(bound: BoundSocket, connections: Connections) -> Self {
        let BoundSocket { listener, file } = bound;
        Incoming {
            listener: Some(listener),
            file,
            connections,
            taken: Default::default(),
            look: None,
            before_closing: None,
        }
    }

    /// Nobody is connected and the quiet time has passed. Takes in whatever
    /// connected at the last moment, or closes the socket if nothing did.
    fn close_unless_someone_came(&mut self, quiet: std::time::Duration) {
        if let Some(hook) = &mut self.before_closing {
            hook();
        }
        let aside = self.file.move_aside();
        let Some(listener) = self.listener.take() else { return };
        // Should the socket no longer be usable, what it took in is still
        // served, and nothing more.
        let left = drain(listener, &mut self.taken).unwrap_or_default();
        match (left, self.taken.is_empty()) {
            (Some(listener), false) => {
                self.listener = Some(listener);
                if let Some(aside) = aside {
                    self.file.put_back(aside);
                }
            }
            (left, _) => {
                drop(left);
                if let Some(aside) = aside {
                    aside.remove();
                }
                if self.taken.is_empty() {
                    let mut state = self.connections.inner.lock().unwrap_or_else(|e| e.into_inner());
                    state.went_idle = true;
                    drop(state);
                    crate::say::line(format_args!("orvud: nobody has been connected for {}s", quiet.as_secs()));
                }
            }
        }
    }
}

/// Accepts every connection already waiting on `listener`, without waiting
/// for more, and hands the listener back. Each is asked of the socket
/// itself: the listener's record of whether one is waiting can be a moment
/// behind.
fn drain(
    listener: tokio::net::UnixListener,
    taken: &mut std::collections::VecDeque<tokio::net::UnixStream>,
) -> std::io::Result<Option<tokio::net::UnixListener>> {
    let listener = listener.into_std()?;
    listener.set_nonblocking(true)?;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true)?;
                taken.push_back(tokio::net::UnixStream::from_std(stream)?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    tokio::net::UnixListener::from_std(listener).map(Some)
}

impl futures::Stream for Incoming {
    type Item = std::io::Result<Counted>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::future::Future;
        use std::task::Poll;
        let this = &mut *self;
        loop {
            if let Some(stream) = this.taken.pop_front() {
                return Poll::Ready(Some(Ok(Counted { stream, _open: this.connections.enter() })));
            }
            let Some(listener) = &this.listener else { return Poll::Ready(None) };
            match listener.poll_accept(cx) {
                Poll::Ready(Ok((stream, _))) => {
                    return Poll::Ready(Some(Ok(Counted { stream, _open: this.connections.enter() })))
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => {}
            }
            let Some(limit) = this.connections.idle_limit() else { return Poll::Pending };
            if let Some(quiet) = this.connections.quiet_for() {
                this.look = None;
                this.close_unless_someone_came(quiet);
                continue;
            }
            // Looking a few times within the quiet period is exact enough,
            // and it is simpler than being woken on every connect and
            // disconnect.
            let every = (limit / 10).clamp(std::time::Duration::from_millis(10), std::time::Duration::from_secs(1));
            let look = this.look.get_or_insert_with(|| Box::pin(tokio::time::sleep(every)));
            match look.as_mut().poll(cx) {
                Poll::Ready(()) => this.look = None,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// One connected client. Dropping it takes the client off the count.
pub(crate) struct OpenConnection {
    connections: Connections,
}

impl Drop for OpenConnection {
    fn drop(&mut self) {
        let mut state = self.connections.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.open -= 1;
        if state.open == 0 {
            state.empty_since = std::time::Instant::now();
        }
    }
}

/// A client's connection, counted for as long as it is open.
struct Counted {
    stream: tokio::net::UnixStream,
    _open: OpenConnection,
}

impl tonic::transport::server::Connected for Counted {
    type ConnectInfo = <tokio::net::UnixStream as tonic::transport::server::Connected>::ConnectInfo;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.stream.connect_info()
    }
}

impl tokio::io::AsyncRead for Counted {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Counted {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    // The two below hand "write several pieces in one go" through to the
    // socket. Left out, every reply would be copied into one piece first.
    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// Puts a socket at `path` and serves on it until the process ends. This is
/// `bind_uds` followed by `serve_bound` with no way to stop it.
pub async fn serve_uds(
    path: &Path,
    client: Arc<orvu_sdk::Client>,
    replica: ReplicaId,
    namespace: &str,
) -> std::io::Result<()> {
    let bound = bind_uds(path).await?;
    serve_bound(bound, client, replica, namespace, std::future::pending()).await
}

/// On Linux a Unix socket path (`sun_path`) holds 108 bytes, and the last one
/// is the NUL byte that ends it.
const SUN_PATH_MAX: usize = 107;

/// Rejects a path that is too long for the kernel to bind. Checking first and
/// naming the path gives a clearer error than a `bind` that fails after the
/// directory already exists.
fn check_sun_path(p: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let len = p.as_os_str().as_bytes().len();
    if len > SUN_PATH_MAX {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "socket path {} is {len} bytes; a Unix socket path holds at most {SUN_PATH_MAX}",
                p.display()
            ),
        ));
    }
    Ok(())
}

/// The directory the socket is first created in. It sits next to `path` and
/// has a short random name, so two starts cannot pick the same one, even two
/// that have the same process id because they run in different containers.
fn staging_dir(path: &Path) -> PathBuf {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut random = [0u8; 4];
    getrandom::fill(&mut random).expect("OS entropy source");
    let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
    parent.join(format!(".orvud-{name}"))
}

/// Removes the staging directories that starts killed outright left next to
/// `path`. Only a directory that is plainly such a leftover is removed: it
/// has the name a start gives, belongs to this user, holds nothing but the
/// staged socket, which nobody serves, and has been left alone for a minute. One with anything else in it, or
/// nothing yet, or a socket that answers may belong to a start going on right
/// now, or to a sidecar that has moved its socket aside for a moment, and is
/// left alone. Nothing that goes wrong here stops the start.
fn clear_left_staging(path: &Path) {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for entry in entries.flatten() {
        let named_like_ours = entry.file_name().to_str().is_some_and(|name| {
            name.strip_prefix(".orvud-")
                .is_some_and(|hex| hex.len() == 8 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        });
        let dir = entry.path();
        if named_like_ours && left_behind(&dir) {
            let _ = std::fs::remove_file(dir.join("s"));
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

/// True if `dir` is a directory of this user's that holds only a socket
/// named `s`, and nobody serves that socket.
fn left_behind(dir: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(md) = std::fs::symlink_metadata(dir) else { return false };
    // SAFETY: `geteuid` takes nothing and cannot fail.
    if !md.is_dir() || md.uid() != unsafe { libc::geteuid() } {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let names: Vec<_> = entries.map(|e| e.map(|e| e.file_name())).collect();
    if !matches!(&names[..], [Ok(name)] if name == "s") {
        return false;
    }
    let socket = dir.join("s");
    let Ok(staged) = std::fs::symlink_metadata(&socket) else { return false };
    // A start going on right now binds its socket a moment before it listens
    // on it, and in between a connection is refused, as to a leftover. So a
    // folder whose socket was made, or that was changed, a short while ago
    // is never taken for a leftover. A time ahead of the clock counts as
    // recent too.
    let old = |md: &std::fs::Metadata| {
        md.modified()
            .is_ok_and(|changed| changed.elapsed().is_ok_and(|age| age >= LEFT_FOR_GOOD))
    };
    if !old(&md) || !old(&staged) || !staged.file_type().is_socket() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&socket).is_err_and(|e| is_stale(&e))
}

/// How long a staging folder has to be left untouched before it can be taken
/// for one a killed start left behind. Putting a socket in place takes far
/// less.
const LEFT_FOR_GOOD: std::time::Duration = std::time::Duration::from_secs(60);

fn stage(dir: &Path, staged: &Path) -> std::io::Result<tokio::net::UnixListener> {
    // mkdir applies the process umask, which can only take permission bits
    // away from 0700.
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    let listener = tokio::net::UnixListener::bind(staged)?;
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Gives the staged socket its real name. A hard link fails if `path` already
/// exists, so whatever turned up there since the check is left alone.
fn install(staged: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::hard_link(staged, path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            std::io::Error::new(
                e.kind(),
                format!(
                    "{} appeared while this sidecar was starting:                      another one may have started at the same moment",
                    path.display()
                ),
            )
        } else {
            e
        }
    })
}

/// Tries to remove a socket that was only partly set up, and ignores errors.
/// It touches only the two names this start chose.
fn unstage(dir: &Path, staged: &Path) {
    let _ = std::fs::remove_file(staged);
    let _ = std::fs::remove_dir(dir);
}

/// True if a failed attempt to connect shows that nobody is serving the
/// socket. Only "connection refused" does. Any other failure, such as a
/// sidecar too busy to answer or a socket this user may not open, leaves it
/// unknown, and an unknown socket must be treated as in use.
fn is_stale(connect_error: &std::io::Error) -> bool {
    connect_error.kind() == std::io::ErrorKind::ConnectionRefused
}

/// Removes a socket left at `path` by an earlier run. Anything else found
/// there is never deleted, because it may be the operator's file. The start
/// fails with an error.
///
/// A socket is only removed when a connection to it is refused, which means
/// nothing is listening. A socket being there does not mean the program that
/// created it is gone. It may be a second `orvud` started by mistake, or
/// another local user who took the name first. Removing a live socket would
/// cut off a working sidecar and leave its clients with a name that nothing
/// answers any more. So a socket that answers, or one we cannot be sure about,
/// fails the start with `AddrInUse`.
async fn clear_stale_socket(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_socket() => {
            match tokio::net::UnixStream::connect(path).await {
                Err(e) if is_stale(&e) => std::fs::remove_file(path),
                Ok(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "{} is a socket that is still being served: \
                         stop that daemon, or serve on a different --socket path",
                        path.display()
                    ),
                )),
                Err(e) => Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!(
                        "{} is a socket and it is not clear whether it is still being served ({e}): \
                         remove it if it is not, or serve on a different --socket path",
                        path.display()
                    ),
                )),
            }
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "{} exists and is not a socket: remove it, or serve on a different --socket path",
                path.display()
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A companion to `the_socket_is_never_reachable_while_permissive` in the
    /// `hardening` tests. That test looks at the directory again and again,
    /// as fast as it can, for a socket with open permissions. It can miss a
    /// moment it did not look at. This test reads the two modes directly,
    /// before any rename, and does not depend on timing. Keep both, because
    /// each covers something the other does not.
    #[tokio::test]
    async fn staging_is_owner_only_before_anything_is_renamed() {
        let base = tempfile::tempdir().expect("tempdir");
        let path = base.path().join("orvud.sock");
        let dir = staging_dir(&path);
        let staged = dir.join("s");

        let listener = stage(&dir, &staged).expect("stages");

        let mode =
            |p: &Path| std::fs::symlink_metadata(p).expect("exists").permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "the staging directory denies other users traversal");
        assert_eq!(mode(&staged), 0o600, "the bound socket is owner-only");
        assert!(!path.exists(), "nothing has been renamed into place yet");

        drop(listener);
        unstage(&dir, &staged);
    }

    // Only "connection refused" proves that nobody is listening. A sidecar
    // that is too busy to answer, or one this user may not connect to, is
    // still alive, and its socket must not be removed.
    #[test]
    fn only_a_refused_connection_counts_as_a_dead_socket() {
        use std::io::{Error, ErrorKind};
        assert!(is_stale(&Error::from(ErrorKind::ConnectionRefused)));
        assert!(!is_stale(&Error::from(ErrorKind::WouldBlock)));
        assert!(!is_stale(&Error::from(ErrorKind::PermissionDenied)));
        assert!(!is_stale(&Error::from(ErrorKind::TimedOut)));
    }

    // Something appears at the socket path after the check and before the
    // socket is put in place. It must be left alone.
    #[tokio::test]
    async fn installing_never_replaces_what_is_already_there() {
        let base = tempfile::tempdir().expect("tempdir");
        let path = base.path().join("orvud.sock");
        let dir = staging_dir(&path);
        let staged = dir.join("s");
        let _listener = stage(&dir, &staged).expect("stages");

        std::fs::write(&path, b"someone else's file").expect("write");
        let err = install(&staged, &path).expect_err("the name is taken");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else's file", "the file was replaced");
        unstage(&dir, &staged);
    }

    // The counting wrapper must not get in the way of writing several pieces
    // in one go, which the socket underneath can do.
    #[tokio::test]
    async fn a_counted_connection_still_writes_several_pieces_at_once() {
        use tokio::io::AsyncWrite;
        let (stream, _other) = tokio::net::UnixStream::pair().expect("pair");
        assert!(stream.is_write_vectored(), "the socket itself can");
        let connections = Connections::default();
        let counted = Counted { stream, _open: connections.enter() };
        assert!(counted.is_write_vectored(), "the wrapper hides it");
        assert_eq!(connections.open(), 1);
        drop(counted);
        assert_eq!(connections.open(), 0);
    }

    /// A socket in a new folder, and a stream of its connections that stops
    /// as soon as nobody is connected.
    async fn idle_socket() -> (tempfile::TempDir, PathBuf, Incoming, Connections) {
        let base = tempfile::tempdir().expect("tempdir");
        let path = base.path().join("orvud.sock");
        let bound = bind_uds(&path).await.expect("binds");
        let connections = Connections::default();
        connections.stop_when_idle(std::time::Duration::ZERO);
        let incoming = Incoming::new(bound, connections.clone());
        (base, path, incoming, connections)
    }

    // A client connects at the very moment the sidecar decides nobody is
    // there. It is taken in, the stop is called off, and the socket is
    // back where clients look for it.
    #[tokio::test]
    async fn a_client_that_connects_as_the_sidecar_goes_idle_is_taken_in() {
        use futures::StreamExt;
        use std::io::{Read, Write};
        let (base, path, mut incoming, connections) = idle_socket().await;
        let client = Arc::new(std::sync::Mutex::new(None));
        let (at, connected, mut once) = (path.clone(), client.clone(), true);
        incoming.before_closing = Some(Box::new(move || {
            if std::mem::take(&mut once) {
                *connected.lock().unwrap() = Some(std::os::unix::net::UnixStream::connect(&at).expect("connects"));
            }
        }));

        let next = tokio::time::timeout(std::time::Duration::from_secs(5), incoming.next()).await.expect("no wait");
        let mut served = next.expect("the stream ended with a client waiting").expect("accepted");
        assert_eq!(connections.open(), 1);
        assert!(!connections.went_idle());
        assert!(path.exists(), "the socket was not put back");
        let names: Vec<_> = std::fs::read_dir(base.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "something was left beside the socket: {names:?}");

        // It is the client's own connection.
        let mut client = client.lock().unwrap().take().expect("the client connected");
        client.write_all(b"hi").unwrap();
        let mut got = [0u8; 2];
        tokio::io::AsyncReadExt::read_exact(&mut served.stream, &mut got).await.unwrap();
        assert_eq!(&got, b"hi");
        drop(served);
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        assert_eq!(client.read(&mut got).unwrap(), 0, "the server side was closed");

        // Once it has gone, the stream ends, and the socket is gone too.
        assert_eq!(connections.open(), 0);
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), incoming.next()).await.expect("no wait");
        assert!(next.is_none(), "the stream went on with nobody connected");
        assert!(connections.went_idle());
        assert!(!path.exists(), "the socket was left behind");
        assert_eq!(std::fs::read_dir(base.path()).unwrap().count(), 0, "something was left behind");
    }

    // A connection waiting to be accepted when the quiet time is up keeps
    // the socket serving.
    #[tokio::test]
    async fn a_waiting_connection_keeps_the_socket_serving() {
        use futures::StreamExt;
        let (_base, path, mut incoming, connections) = idle_socket().await;
        let _client = std::os::unix::net::UnixStream::connect(&path).expect("connects");
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), incoming.next()).await.expect("no wait");
        assert!(matches!(next, Some(Ok(_))), "the waiting client was not taken in");
        assert!(!connections.went_idle());
        assert!(path.exists());
    }

    // With nobody there, the socket stops serving once the quiet time is up,
    // and not before.
    #[tokio::test]
    async fn the_socket_stops_serving_once_nobody_has_been_there_for_long_enough() {
        use futures::StreamExt;
        let base = tempfile::tempdir().expect("tempdir");
        let path = base.path().join("orvud.sock");
        let connections = Connections::default();
        connections.stop_when_idle(std::time::Duration::from_millis(300));
        let started = std::time::Instant::now();
        let mut incoming = Incoming::new(bind_uds(&path).await.unwrap(), connections.clone());
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), incoming.next()).await.expect("stops");
        assert!(next.is_none());
        assert!(started.elapsed() >= std::time::Duration::from_millis(300), "stopped early");
        assert!(connections.went_idle());
        assert!(!path.exists());
    }

    /// A staging directory as a start killed outright leaves it: a socket
    /// that nobody serves any more, alone in a `0700` directory.
    /// It was left an hour ago.
    fn left_by_a_killed_start(base: &Path, name: &str) -> PathBuf {
        let dir = left_just_now(base, name);
        set_back(&dir.join("s"));
        set_back(&dir);
        dir
    }

    /// The same, left a moment ago. It looks just like the folder of a start
    /// going on right now that has made its socket and not listened on it yet.
    fn left_just_now(base: &Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        drop(std::os::unix::net::UnixListener::bind(dir.join("s")).unwrap());
        dir
    }

    /// Sets the time `path` was last changed an hour back.
    fn set_back(path: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let hour_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t
            - 3600;
        let times = [libc::timespec { tv_sec: hour_ago, tv_nsec: 0 }; 2];
        // SAFETY: `name` is a valid C string and `times` two valid timespecs,
        // both alive for the whole call.
        let done = unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        assert_eq!(done, 0, "{}: {}", path.display(), std::io::Error::last_os_error());
    }

    // A start going on right now has made its socket and not listened on it
    // yet, so a connection to it is refused, just as to a leftover. Its
    // folder was changed a moment ago, and is left alone.
    #[tokio::test]
    async fn a_staging_directory_changed_a_moment_ago_is_left_alone() {
        let base = tempfile::tempdir().expect("tempdir");
        let starting = left_just_now(base.path(), ".orvud-0a1b2c3d");
        // Either one changed a moment ago is enough.
        let socket_new = left_just_now(base.path(), ".orvud-1a1b2c3d");
        set_back(&socket_new);
        let folder_new = left_just_now(base.path(), ".orvud-2a1b2c3d");
        set_back(&folder_new.join("s"));
        let path = base.path().join("orvud.sock");
        let _bound = bind_uds(&path).await.expect("binds");
        for kept in [&starting, &socket_new, &folder_new] {
            assert!(kept.join("s").exists(), "{} was removed", kept.display());
        }
    }

    // A start killed while putting its socket in place leaves its staging
    // directory behind. The next start removes it.
    #[tokio::test]
    async fn a_staging_directory_left_by_a_killed_start_is_removed() {
        let base = tempfile::tempdir().expect("tempdir");
        let left = left_by_a_killed_start(base.path(), ".orvud-0a1b2c3d");
        let path = base.path().join("orvud.sock");
        let _bound = bind_uds(&path).await.expect("binds");
        assert!(!left.exists(), "the leftover staging directory is still there");
    }

    // Anything that is not plainly a leftover is left alone: a socket that is
    // served, as by a start going on right now or a sidecar moving its socket
    // aside; a directory holding anything else, or nothing yet; and names a
    // start does not give.
    #[tokio::test]
    async fn only_plain_leftovers_are_removed() {
        let base = tempfile::tempdir().expect("tempdir");
        let served = base.path().join(".orvud-11111111");
        std::fs::DirBuilder::new().mode(0o700).create(&served).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(served.join("s")).unwrap();
        let more = left_by_a_killed_start(base.path(), ".orvud-22222222");
        std::fs::write(more.join("notes"), b"mine").unwrap();
        let empty = base.path().join(".orvud-33333333");
        std::fs::DirBuilder::new().mode(0o700).create(&empty).unwrap();
        let not_a_socket = base.path().join(".orvud-44444444");
        std::fs::create_dir(&not_a_socket).unwrap();
        std::fs::write(not_a_socket.join("s"), b"a file").unwrap();
        let other_names = [
            left_by_a_killed_start(base.path(), ".orvud-555555555"),
            left_by_a_killed_start(base.path(), ".orvud-6666666"),
            left_by_a_killed_start(base.path(), ".orvud-ABCDEF01"),
            left_by_a_killed_start(base.path(), "orvud-77777777"),
        ];

        let path = base.path().join("orvud.sock");
        let _bound = bind_uds(&path).await.expect("binds");
        for kept in [&served, &more, &empty, &not_a_socket].into_iter().chain(&other_names) {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        for kept in [served.join("s"), more.join("s"), more.join("notes"), not_a_socket.join("s")] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
    }

    #[test]
    fn two_starts_do_not_share_a_staging_directory() {
        let path = Path::new("/run/orvud.sock");
        assert_ne!(staging_dir(path), staging_dir(path));
    }
}
