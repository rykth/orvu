//! Turns each request into one `orvu-sdk` call, and the result of that call
//! into a response. It is a translation table and nothing more. It does not
//! repeat checks the engine already makes, it caches nothing and it reads no
//! clock.

use crate::errors::{self, on, reply};
use crate::pb;
use crate::rules::Guard;
use orvu_crdt::{parse_path, JsonValue, ReplicaId, Seg, Span, TreeNode};
use orvu_sdk::JsonChange;
use crate::search::Searcher;
use orvu_sdk::Client;
use std::collections::BTreeMap;
use tonic::Status;

/// The most operations one `Batch` may hold. A longer batch is rejected before
/// anything runs.
pub const MAX_BATCH_OPS: usize = 1024;

/// The most reply data one `Batch` collects, in bytes. Replies are held in
/// memory until the whole batch is done, and without a limit a batch of reads
/// of one large object could use any amount of it. Once the replies so far add
/// up to more than this, the operations that are left are not run and each
/// gets `CODE_TOO_LARGE`. The limit matches the largest message a gRPC client
/// accepts unless it is told otherwise.
pub const MAX_BATCH_REPLY_BYTES: usize = 4 * 1024 * 1024;

/// The largest request the sidecar takes, in bytes. A larger one is refused
/// with `OUT_OF_RANGE` before it is read. Replies have no limit of their
/// own on the sidecar's side; a client has its own limit for what it reads.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// The envelope rules, and a type that proves a request passed them. The
/// envelope is the outer request message, checked before any work is done.
///
/// The fields of `Validated` are private and only [`envelope::check`] builds
/// one, so a request cannot reach [`apply`] without passing every rule. And
/// `apply` returns a `pb::Response`, so it cannot reject a request at all. The
/// split is there for batches. If `apply` could reject a request, a batch would
/// stop partway, after an earlier write had already been applied, with a
/// `Status` that tells the client its state is unknown.
mod envelope {
    use super::pb;
    use orvu_sdk::ConsistencyClass;

    /// A request that has passed [`check`].
    pub struct Validated {
        op: pb::request::Op,
        /// `Some` exactly when `op` is a `Declare`. It is the class the
        /// request names, worked out in `check` so that `apply` has no
        /// conversion that can fail.
        class: Option<ConsistencyClass>,
    }

    impl Validated {
        /// The search the request is, if it is one.
        pub fn search(&self) -> Option<&pb::Search> {
            match &self.op {
                pb::request::Op::Search(search) => Some(search),
                _ => None,
            }
        }

        /// Splits the checked request into its op and, for a `Declare`, its class.
        pub fn into_parts(self) -> (pb::request::Op, Option<ConsistencyClass>) {
            (self.op, self.class)
        }
    }

    /// Every reason this layer can reject a request, in one place. Each can
    /// be decided from the envelope alone, before any engine call. The error
    /// text is the end of a sentence, so the same rule reads as "request
    /// carries no op" and as "batch request 3 carries no op; nothing was
    /// applied".
    ///
    /// A new rule goes here and nowhere else. An engine answer, such as a
    /// refusal or a type error, is not an envelope rule. It is a response
    /// value. Putting one here would turn "refused, your state is intact" into
    /// a raised exception in every generated client.
    pub fn check(req: pb::Request) -> Result<Validated, String> {
        let op = match req.op {
            Some(op) => op,
            None => return Err("carries no op".into()),
        };
        let class = match &op {
            pb::request::Op::Declare(d) => match pb::Class::try_from(d.object_class) {
                Ok(pb::Class::Ap) => Some(ConsistencyClass::Ap),
                Ok(pb::Class::Escrow) => Some(ConsistencyClass::Escrow),
                // `CLASS_UNSPECIFIED` is the proto3 default, so a client that
                // writes `Declare(object="x")` and forgets the field ends up
                // here the first time it runs. Guessing AP would treat a
                // bounded counter (Escrow) as an ordinary AP object, so we
                // reject the request and do not pick a default.
                Ok(pb::Class::Unspecified) | Err(_) => {
                    return Err("declares an unknown consistency class".into())
                }
            },
            // proto3 cannot tell a `value` that was left out from an empty
            // one, and an empty value is how a mark is removed, which has its
            // own op. So a format with no value is a mistake in the request.
            pb::request::Op::RichtextFormat(f) if f.value.is_empty() => {
                return Err("formats with an empty value; use richtext_unformat to remove a mark".into())
            }
            pb::request::Op::RichtextInsertMarked(f) if f.value.is_empty() => {
                return Err("marks the text it puts in with an empty value".into())
            }
            pb::request::Op::Search(s) if crate::search::team_of_index(&s.object).is_none() => {
                return Err(format!("searches {:?}, which is not a team's index, orvu/<team>/index", s.object))
            }
            pb::request::Op::Search(s) if s.limit > crate::search::MOST => {
                return Err(format!("asks for {} results; the most is {}", s.limit, crate::search::MOST))
            }
            _ => None,
        };
        Ok(Validated { op, class })
    }
}

/// `Ok(response)` means this layer did its job, whatever the engine answered,
/// and that includes a refusal. `Err(Status)` means the request itself was
/// rejected, and every such rejection is a rule in [`envelope::check`]. The one
/// `?` here is that check. There is no second one, because [`apply`] cannot
/// fail.
///
/// A search is answered by `searcher`, which finds only what `may` lets
/// through: what the one asking may read.
pub async fn dispatch(
    client: &Client,
    searcher: &Searcher,
    may: impl Fn(&str) -> bool + Send + 'static,
    req: pb::Request,
) -> Result<pb::Response, Status> {
    dispatch_guarded(client, searcher, may, req, None).await
}

/// As `dispatch`, with the write checked by `guard` against the object as it
/// is when the write is made, in the same step: see `rules::guards`. A write
/// the guard turns away changes nothing and is answered as refused.
pub async fn dispatch_guarded(
    client: &Client,
    searcher: &Searcher,
    may: impl Fn(&str) -> bool + Send + 'static,
    req: pb::Request,
    guard: Option<Guard>,
) -> Result<pb::Response, Status> {
    let valid =
        envelope::check(req).map_err(|e| Status::invalid_argument(format!("request {e}")))?;
    if let Some(stopped) = cannot_save(client) {
        return Ok(stopped);
    }
    if let Some(search) = valid.search() {
        return Ok(match searcher.search(&search.object, &search.query, search.limit, may).await {
            Ok(found) => errors::json(found),
            Err(why) => errors::no_search(why),
        });
    }
    Ok(apply(client, valid, guard).await)
}

/// The answer to every operation once the node can no longer save, or
/// `None` while it can. Such a node may hold a change in memory that is not
/// on disk, so it shows nothing, not even whether an object exists.
fn cannot_save(client: &Client) -> Option<pb::Response> {
    client.storage_failure().map(|why| errors::to_response(orvu_sdk::SdkError::Storage(why)))
}

/// The translation table. Every answer is a `pb::Response`, so no branch can
/// return a `Status` that `dispatch_batch`'s first pass does not already know
/// about.
async fn apply(client: &Client, valid: envelope::Validated, guard: Option<Guard>) -> pb::Response {
    use pb::request::Op;
    if let Some(stopped) = cannot_save(client) {
        return stopped;
    }
    let (op, class) = valid.into_parts();
    // A change made for a call sent again, which its receipt says was made
    // already, is answered as made. Only a part that makes two changes goes
    // on, to make the second if only the first was made.
    if let Some(receipt) = orvu_sdk::current_receipt() {
        if client.receipt_made(&receipt) && !(guard.is_some() && makes_two_changes(&op)) {
            return errors::ack();
        }
    }
    // A read of an object that does not exist answers "absent" and creates
    // nothing. Opening the handle would create an empty object of the type
    // that was asked for, and the name could then never hold another type.
    if let Some(object) = read_target(&op) {
        if !client.exists(object).await {
            // Saving may have failed just now, and then nothing was read.
            return cannot_save(client).unwrap_or_else(|| errors::opt_string(None));
        }
    }
    match op {
        Op::Declare(d) => match class {
            Some(class) => reply(client.declare(&d.object, class), |()| errors::ack()),
            // This cannot happen: `check` either works out a Declare's class
            // or rejects the request. If it ever did, the answer is a failed
            // response. It is not a panic, because a panic while handling a
            // request takes the connection's task down with it, and it is not
            // a status.
            None => errors::engine_failed("declare: class not resolved".into()),
        },
        Op::CounterIncrement(o) => {
            on(client.counter(&o.object).await, |h| async move { h.increment(o.n).await }, |()| errors::ack()).await
        }
        Op::CounterDecrement(o) => {
            on(client.counter(&o.object).await, |h| async move { h.decrement(o.n).await }, |()| errors::ack()).await
        }
        Op::CounterValue(o) => {
            on(client.counter(&o.object).await, |h| async move { h.value().await }, errors::int).await
        }
        Op::RegisterSet(o) => {
            on(client.register(&o.object).await, |h| async move { h.set(&o.value).await }, |()| errors::ack()).await
        }
        Op::RegisterGet(o) => {
            on(client.register(&o.object).await, |h| async move { h.get().await }, errors::opt_string).await
        }
        Op::SetAdd(o) => {
            on(client.set(&o.object).await, |h| async move { h.add(&o.member).await }, |()| errors::ack()).await
        }
        Op::SetRemove(o) => {
            on(client.set(&o.object).await, |h| async move { h.remove(&o.member).await }, |()| errors::ack()).await
        }
        Op::SetContains(o) => {
            on(client.set(&o.object).await, |h| async move { h.contains(&o.member).await }, errors::boolean).await
        }
        Op::SetElements(o) => {
            on(client.set(&o.object).await, |h| async move { h.elements().await }, errors::strings).await
        }
        Op::MapPut(o) => {
            on(client.map(&o.object).await, |h| async move { h.put(&o.key, &o.value).await }, |()| errors::ack()).await
        }
        Op::MapRemove(o) => {
            on(client.map(&o.object).await, |h| async move { h.remove(&o.key).await }, |()| errors::ack()).await
        }
        Op::MapGet(o) => {
            on(client.map(&o.object).await, |h| async move { h.get(&o.key).await }, errors::opt_string).await
        }
        Op::MapKeys(o) => {
            on(client.map(&o.object).await, |h| async move { h.keys().await }, errors::strings).await
        }
        Op::MapEntries(o) => {
            on(client.map(&o.object).await, |h| async move { h.entries().await }, errors::pairs).await
        }
        Op::MvMapPut(o) => match guard {
            Some(guard) => {
                let put = |h: orvu_sdk::MvMap| async move { h.put_if(&o.key, &o.value, move |now| guard.note(now)).await };
                on(client.mv_map(&o.object).await, put, |()| errors::ack()).await
            }
            None => on(client.mv_map(&o.object).await, |h| async move { h.put(&o.key, &o.value).await }, |()| errors::ack()).await,
        },
        Op::MvMapRemove(o) => match guard {
            Some(guard) => {
                let remove = |h: orvu_sdk::MvMap| async move { h.remove_if(&o.key, move |now| guard.note(now)).await };
                on(client.mv_map(&o.object).await, remove, |()| errors::ack()).await
            }
            None => on(client.mv_map(&o.object).await, |h| async move { h.remove(&o.key).await }, |()| errors::ack()).await,
        },
        Op::MvMapGet(o) => {
            on(client.mv_map(&o.object).await, |h| async move { h.get(&o.key).await }, errors::strings).await
        }
        Op::MvMapKeys(o) => {
            on(client.mv_map(&o.object).await, |h| async move { h.keys().await }, errors::strings).await
        }
        Op::MvMapEntries(o) => {
            on(client.mv_map(&o.object).await, |h| async move { h.entries().await }, errors::multi_pairs).await
        }
        // Written by the sidecar with the top-up it goes with: see below.
        Op::CartAdd(o) if guard.is_some() && crate::rules::is_topped_record(&o.object) => errors::ack(),
        Op::CartAdd(o) => {
            on(client.cart(&o.object).await, |h| async move { h.add(&o.item, o.qty).await }, |()| errors::ack()).await
        }
        Op::CartRemoveQty(o) => {
            on(client.cart(&o.object).await, |h| async move { h.remove_qty(&o.item, o.qty).await }, |()| errors::ack()).await
        }
        Op::CartRemoveItem(o) => {
            on(client.cart(&o.object).await, |h| async move { h.remove_item(&o.item).await }, |()| errors::ack()).await
        }
        Op::CartQuantity(o) => {
            on(client.cart(&o.object).await, |h| async move { h.quantity(&o.item).await }, errors::int).await
        }
        Op::CartItems(o) => {
            on(client.cart(&o.object).await, |h| async move { h.items().await }, errors::counted).await
        }
        Op::ListInsert(o) => {
            on(client.list(&o.object).await, |h| async move { h.insert(index(o.index), o.value).await }, |()| errors::ack()).await
        }
        Op::ListDelete(o) => {
            on(client.list(&o.object).await, |h| async move { h.delete(index(o.index)).await }, |()| errors::ack()).await
        }
        Op::ListGet(o) => {
            on(client.list(&o.object).await, |h| async move { h.get(index(o.index)).await }, errors::opt_string).await
        }
        Op::ListLen(o) => {
            on(client.list(&o.object).await, |h| async move { h.len().await }, |n| errors::int(n as i64)).await
        }
        Op::ListToVec(o) => {
            on(client.list(&o.object).await, |h| async move { h.to_vec().await }, errors::strings).await
        }
        Op::JsonSet(o) => match (parse_path(&o.path_json), JsonValue::from_json_str(&o.value_json)) {
            (Ok(path), Ok(value)) if guard.is_some() => {
                guarded(client, &o.object, JsonChange::Set { path, value }, guard).await
            }
            (Ok(path), Ok(value)) => {
                on(client.json(&o.object).await, |h| async move { h.set(&path, value).await }, |()| errors::ack()).await
            }
            (Err(e), _) | (_, Err(e)) => errors::bad_json(e.to_string()),
        },
        Op::JsonRemove(o) => match parse_path(&o.path_json) {
            Ok(path) if guard.is_some() => guarded(client, &o.object, JsonChange::Remove { path }, guard).await,
            Ok(path) => {
                on(client.json(&o.object).await, |h| async move { h.remove(&path).await }, |()| errors::ack()).await
            }
            Err(e) => errors::bad_json(e.to_string()),
        },
        Op::JsonInsert(o) => match (parse_path(&o.path_json), JsonValue::from_json_str(&o.value_json)) {
            (Ok(path), Ok(value)) if guard.is_some() => {
                guarded(client, &o.object, JsonChange::Insert { path, index: index(o.index), value }, guard).await
            }
            (Ok(path), Ok(value)) => {
                let at = index(o.index);
                // The engine refuses an insert where something that is not a
                // list is, so nothing it held can be lost.
                on(client.json(&o.object).await, |h| async move { h.insert(&path, at, value).await }, |()| errors::ack()).await
            }
            (Err(e), _) | (_, Err(e)) => errors::bad_json(e.to_string()),
        },
        Op::JsonGet(o) => match parse_path(&o.path_json) {
            Ok(path) => {
                on(client.json(&o.object).await, |h| async move { h.get(&path).await }, opt_json).await
            }
            Err(e) => errors::bad_json(e.to_string()),
        },
        Op::JsonToValue(o) => {
            on(client.json(&o.object).await, |h| async move { h.to_value().await }, json_value).await
        }
        Op::RichtextInsert(o) => {
            on(client.rich_text(&o.object).await, |h| async move { h.insert(index(o.index), o.text).await }, |()| errors::ack()).await
        }
        Op::RichtextDelete(o) => {
            on(client.rich_text(&o.object).await, |h| async move { h.delete(index(o.index), index(o.len)).await }, |()| errors::ack()).await
        }
        Op::RichtextFormat(o) => {
            on(
                client.rich_text(&o.object).await,
                |h| async move { h.format(index(o.index), index(o.len), o.attr, o.value).await },
                |()| errors::ack(),
            )
            .await
        }
        Op::RichtextUnformat(o) => {
            on(
                client.rich_text(&o.object).await,
                |h| async move { h.unformat(index(o.index), index(o.len), o.attr).await },
                |()| errors::ack(),
            )
            .await
        }
        Op::RichtextInsertMarked(o) => {
            on(
                client.rich_text(&o.object).await,
                |h| async move { h.insert_marked(index(o.index), o.text, o.attr, o.value).await },
                |()| errors::ack(),
            )
            .await
        }
        Op::RichtextText(o) => {
            on(client.rich_text(&o.object).await, |h| async move { h.text().await }, |t| errors::value(pb::read_value::V::StringValue(t))).await
        }
        Op::RichtextToSpans(o) => {
            on(client.rich_text(&o.object).await, |h| async move { h.to_spans().await }, spans_value).await
        }
        Op::TreeMoveNode(o) => {
            on(client.tree(&o.object).await, |h| async move { h.move_node(&o.child, &o.new_parent).await }, |()| errors::ack()).await
        }
        Op::TreeMoveNodeAt(o) => {
            on(
                client.tree(&o.object).await,
                |h| async move { h.move_node_at(&o.child, &o.new_parent, index(o.index)).await },
                |()| errors::ack(),
            )
            .await
        }
        Op::TreeSetLabel(o) => {
            on(client.tree(&o.object).await, |h| async move { h.set_label(&o.node, &o.label).await }, |()| errors::ack()).await
        }
        Op::TreeChildren(o) => {
            on(client.tree(&o.object).await, |h| async move { h.children(&o.node).await }, errors::strings).await
        }
        Op::TreeToNodes(o) => {
            on(client.tree(&o.object).await, |h| async move { h.to_nodes().await }, nodes_value).await
        }
        Op::TreeParent(o) => {
            on(client.tree(&o.object).await, |h| async move { h.parent(&o.node).await }, errors::opt_string).await
        }
        // The bounded counter ops, which can be refused. A refusal
        // (`Exhausted`) goes through `to_response` like every other error and
        // arrives as `Refused`. The allowance is not checked here first. The
        // engine checks it at the moment it applies the change, and that check
        // is what keeps `value >= 0`.
        // For an agent with rules, the line in a budget's record of who topped
        // it up is written by the sidecar, right after the top-up and as part
        // of making it, so that the two land together or not at all however
        // the request they are in is cut short. The line the agent sent with
        // the top-up stands for that one, and is not written itself.
        Op::BoundedIncrement(o) if guard.is_some() && crate::rules::topped_record(&o.object).is_some() => {
            let (agent, record) = (guard.map(|g| g.agent().to_string()).unwrap_or_default(), crate::rules::topped_record(&o.object));
            // For a call with a request id, the top-up and its line each have
            // a receipt, so that a call sent again after only the first was
            // made makes the line, and only it.
            let first = orvu_sdk::current_receipt();
            let line = first.map(crate::requests::second_receipt);
            let made = |receipt: Option<[u8; 16]>| receipt.is_some_and(|receipt| client.receipt_made(&receipt));
            if !made(first) {
                // A top-up the budget could not hold whole is turned away, so
                // the line never says more was added than was.
                let topped =
                    on(client.bounded_counter(&o.object).await, |h| async move { h.try_increment(o.n).await }, |()| errors::ack()).await;
                if !matches!(topped.result, Some(pb::response::Result::Ack(_))) {
                    return topped;
                }
                crate::requests::stop_here_if_asked("top-up");
            }
            if made(line) {
                return errors::ack();
            }
            let record = record.unwrap_or_default();
            let add = on(client.cart(&record).await, |h| async move { h.add(&agent, o.n).await }, |()| errors::ack());
            match line {
                Some(line) => orvu_sdk::with_receipt(line, add).await,
                None => add.await,
            }
        }
        Op::BoundedIncrement(o) => {
            on(client.bounded_counter(&o.object).await, |h| async move { h.increment(o.n).await }, |()| errors::ack()).await
        }
        Op::BoundedTryDecrement(o) => {
            on(client.bounded_counter(&o.object).await, |h| async move { h.try_decrement(o.n).await }, |()| errors::ack()).await
        }
        Op::BoundedTransfer(o) => {
            on(
                client.bounded_counter(&o.object).await,
                |h| async move { h.transfer(ReplicaId(o.to_replica), o.n).await },
                |()| errors::ack(),
            )
            .await
        }
        Op::BoundedValue(o) => {
            on(client.bounded_counter(&o.object).await, |h| async move { h.value().await }, errors::int).await
        }
        Op::BoundedAllowance(o) => {
            on(
                client.bounded_counter(&o.object).await,
                |h| async move { h.local_allowance().await },
                // An allowance above i64::MAX is reported as i64::MAX, so a
                // client cannot tell that apart from a real allowance of
                // exactly that size. A read has no unsigned number to carry
                // it, and 2^63 units of allowance is not a realistic counter.
                // `fn index` clamps in the same way.
                |n| errors::int(i64::try_from(n).unwrap_or(i64::MAX)),
            )
            .await
        }
        // A search on its own is answered before it gets here. One in a batch
        // is not run: it can take a while, and the rest of the batch would
        // wait for it.
        Op::Search(_) => errors::no_search("a search cannot be part of a batch; send it on its own".into()),
    }
}

/// Whether `op` is made as two changes: a top-up of a budget, for an agent
/// with rules, goes with its line in the record of who topped it up.
pub fn makes_two_changes(op: &pb::request::Op) -> bool {
    matches!(op, pb::request::Op::BoundedIncrement(o) if crate::rules::topped_record(&o.object).is_some())
}

/// Runs the ops in order. Each result stands on its own. There is no
/// transaction: if op 2 is refused, that says nothing about ops 1 and 3, and
/// they are not undone. Ops are not merged together, and they do not succeed
/// or fail as a group.
///
/// The envelope check is the one all-or-nothing part. An invalid envelope is a
/// bug in the client that can be found before any engine call. A client cannot
/// recover from a batch that was partly applied without it being told. A
/// generated client that retries the batch after the exception would apply a
/// second time every op that went through and is not safe to repeat. So every
/// request is checked before the first engine call, and if one fails nothing
/// is applied. An engine answer (a refusal, a type error) is not part of the
/// envelope check. It stays independent and nothing is undone.
///
/// This cannot drift apart from the single-op path. The rules are
/// [`envelope::check`], the same function [`dispatch`] uses, and [`apply`] has
/// no `Status` to return. So a new rule covers batches too, whether or not its
/// author thought about them.
///
/// A session id works like a password, so an inner `session` that is set and
/// differs from the batch's is rejected too, in the same pass. An empty inner
/// session takes the batch's. This rule is not in `check` because only a batch
/// has an outer session to disagree with.
///
/// Once the checks have passed, the operations run in a task of their own. If
/// the caller gives up or its time limit runs out partway through, the batch
/// still runs to the end. Stopping in the middle would leave the first
/// operations applied and the rest not, and a caller that then tried again
/// would apply the first ones twice.
///
/// The one exception is the sidecar stopping. Once `stopping` fires, the
/// operations not yet run are not run, so that a large batch does not hold
/// the stop up, and each comes back saying so. The caller is still answered,
/// and can tell from the replies which operations were made.
pub async fn dispatch_batch(
    client: &Client,
    session: &str,
    reqs: Vec<pb::Request>,
    stopping: tokio_util::sync::CancellationToken,
) -> Result<Vec<pb::Response>, Status> {
    let guards = vec![None; reqs.len()];
    dispatch_batch_guarded(client, session, reqs, stopping, guards, Vec::new()).await
}

/// As `dispatch_batch`, with each write checked by its guard, as
/// `dispatch_guarded` does.
///
/// `receipts` holds, for a batch sent with a request id, the receipt id each
/// part's change is saved under, and whether it was saved before the sidecar
/// last started. Such a part is answered as made, and not made again.
pub async fn dispatch_batch_guarded(
    client: &Client,
    session: &str,
    reqs: Vec<pb::Request>,
    stopping: tokio_util::sync::CancellationToken,
    mut guards: Vec<Option<Guard>>,
    mut receipts: Vec<Option<([u8; 16], bool)>>,
) -> Result<Vec<pb::Response>, Status> {
    guards.resize(reqs.len(), None);
    receipts.resize(reqs.len(), None);
    if reqs.len() > MAX_BATCH_OPS {
        return Err(Status::invalid_argument(format!(
            "batch holds {} operations and the limit is {MAX_BATCH_OPS}; nothing was applied",
            reqs.len()
        )));
    }
    let mut valid = Vec::with_capacity(reqs.len());
    for (i, req) in reqs.into_iter().enumerate() {
        if !req.session.is_empty() && req.session != session {
            return Err(Status::invalid_argument(format!(
                "batch request {i} carries a session other than the batch's; nothing was applied"
            )));
        }
        valid.push(envelope::check(req).map_err(|e| {
            Status::invalid_argument(format!("batch request {i} {e}; nothing was applied"))
        })?);
    }
    // From here no request can be rejected, so every op runs and every op
    // gets a result.
    let client = client.clone();
    let run = tokio::spawn(async move {
        use prost::Message;
        let mut out = Vec::with_capacity(valid.len());
        let mut bytes = 0;
        // Once a write a guard checks is turned away as it is made, the rest
        // of the batch is not made: the agent's own things changed under it.
        let mut halted = false;
        for ((req, guard), receipt) in valid.into_iter().zip(guards).zip(receipts) {
            if receipt.is_some_and(|(_, saved)| saved) {
                out.push(errors::ack());
                continue;
            }
            if halted {
                out.push(errors::to_response(orvu_sdk::SdkError::NotAllowed(
                    "not made: an earlier part of the batch was not allowed when it came to be made".into(),
                )));
                continue;
            }
            if bytes > MAX_BATCH_REPLY_BYTES {
                out.push(errors::too_large());
                continue;
            }
            if stopping.is_cancelled() {
                out.push(errors::stopping());
                continue;
            }
            let guarded = guard.is_some();
            let reply = match receipt {
                Some((id, _)) => orvu_sdk::with_receipt(id, apply(&client, req, guard)).await,
                None => apply(&client, req, guard).await,
            };
            halted = guarded && matches!(reply.result, Some(pb::response::Result::Refused(_)));
            bytes += reply.encoded_len();
            out.push(reply);
        }
        out
    });
    run.await.map_err(|e| Status::internal(format!("the batch stopped unexpectedly: {e}")))
}

/// Makes one write to the JSON document `object` that `guard` checks, in the
/// same step, against what is where the write is as the write is made.
async fn guarded(client: &Client, object: &str, change: JsonChange, guard: Option<Guard>) -> pb::Response {
    let Some(guard) = guard else { return errors::engine_failed("a guarded write with no guard".into()) };
    let (path, value, insert) = match &change {
        JsonChange::Set { path, value } => (path.clone(), Some(value.clone()), false),
        JsonChange::Insert { path, value, .. } => (path.clone(), Some(value.clone()), true),
        JsonChange::Remove { path } => (path.clone(), None, false),
    };
    // Who last changed a task is written by the sidecar alone, for an agent
    // with rules: a write of it is answered as made, and not made.
    if matches!(path.as_slice(), [Seg::Key(_), Seg::Key(field), ..] if field == "updated_by") {
        return errors::ack();
    }
    let at = crate::rules::looked_at(&path);
    let board = object.to_string();
    let agent = JsonValue::Str(guard.agent().to_string());
    // The change, and the agent's name as the one that last changed the
    // task, are made in one step and sent as one change, so that nothing can
    // come between them. A task written whole over one that is there is
    // named too; one added is not, as adding is not changing.
    let decide = move |now: Option<&JsonValue>| {
        guard.board(&board, &path, value.as_ref(), insert, now, false)?;
        // What is not an object is no task, as the guard takes it.
        let now = now.filter(|task| matches!(task, JsonValue::Object(_)));
        Ok(match (path.as_slice(), change) {
            ([Seg::Key(_)], JsonChange::Set { path, value: JsonValue::Object(mut fields) }) => {
                match now {
                    Some(_) => fields.insert("updated_by".into(), agent),
                    None => fields.remove("updated_by"),
                };
                // A firm task's number is the highest of the ticket numbers
                // written on it, and must never go down. A task written whole
                // starts afresh, so the numbers it had are carried over into
                // it, beside any it brings.
                if let Some(JsonValue::Object(task)) = now {
                    if let Some(JsonValue::Object(had)) = task.get("tickets") {
                        let mut tickets = match fields.remove("tickets") {
                            Some(JsonValue::Object(brought)) => brought,
                            _ => Default::default(),
                        };
                        for (number, mark) in had {
                            tickets.entry(number.clone()).or_insert_with(|| mark.clone());
                        }
                        fields.insert("tickets".into(), JsonValue::Object(tickets));
                    }
                }
                vec![JsonChange::Set { path, value: JsonValue::Object(fields) }]
            }
            ([Seg::Key(_)], change) => vec![change],
            // The whole of a task's ticket numbers, written as an object,
            // adds the ones not there yet, each by itself, and leaves the
            // ones that are as they are. Written whole, they would start
            // afresh, and drop what other machines are adding.
            ([Seg::Key(id), Seg::Key(field)], JsonChange::Set { value: JsonValue::Object(numbers), .. })
                if field == "tickets" && now.is_some() =>
            {
                let had = match now {
                    Some(JsonValue::Object(task)) => match task.get("tickets") {
                        Some(JsonValue::Object(had)) => had.clone(),
                        _ => Default::default(),
                    },
                    _ => Default::default(),
                };
                let mut changes: Vec<JsonChange> = numbers
                    .into_iter()
                    .filter(|(number, _)| !had.contains_key(number))
                    .map(|(number, mark)| JsonChange::Set {
                        path: vec![Seg::Key(id.clone()), Seg::Key("tickets".into()), Seg::Key(number)],
                        value: mark,
                    })
                    .collect();
                changes.push(JsonChange::Set { path: vec![Seg::Key(id.clone()), Seg::Key("updated_by".into())], value: agent });
                changes
            }
            // A write inside a task that is not there is turned away by the
            // guard above. Should one get here, it names nobody, as a name
            // would leave behind a task that nobody added.
            ([Seg::Key(_), ..], change) if now.is_none() => vec![change],
            ([Seg::Key(id), ..], change) => {
                let named = vec![Seg::Key(id.clone()), Seg::Key("updated_by".into())];
                vec![change, JsonChange::Set { path: named, value: agent }]
            }
            (_, change) => vec![change],
        })
    };
    let write = |h: orvu_sdk::Json| async move { h.change_with(&at, decide).await };
    on(client.json(object).await, write, |()| errors::ack()).await
}

/// The name of the object a read is about, or `None` for an op that writes.
fn read_target(op: &pb::request::Op) -> Option<&str> {
    use pb::request::Op;
    Some(match op {
        Op::CounterValue(o) => &o.object,
        Op::RegisterGet(o) => &o.object,
        Op::SetContains(o) => &o.object,
        Op::SetElements(o) => &o.object,
        Op::MapGet(o) => &o.object,
        Op::MapKeys(o) => &o.object,
        Op::MapEntries(o) => &o.object,
        Op::MvMapGet(o) => &o.object,
        Op::MvMapKeys(o) => &o.object,
        Op::MvMapEntries(o) => &o.object,
        Op::CartQuantity(o) => &o.object,
        Op::CartItems(o) => &o.object,
        Op::ListGet(o) => &o.object,
        Op::ListLen(o) => &o.object,
        Op::ListToVec(o) => &o.object,
        Op::JsonGet(o) => &o.object,
        Op::JsonToValue(o) => &o.object,
        Op::RichtextText(o) => &o.object,
        Op::RichtextToSpans(o) => &o.object,
        Op::TreeChildren(o) => &o.object,
        Op::TreeToNodes(o) => &o.object,
        Op::TreeParent(o) => &o.object,
        Op::BoundedValue(o) => &o.object,
        Op::BoundedAllowance(o) => &o.object,
        _ => return None,
    })
}

/// An index arrives as a u64. One too large for this platform's usize is
/// clamped to usize::MAX, so it does not wrap around to a small number. What
/// the engine then does with an index past the end depends on the type: a list
/// reports OutOfBounds, and rich text and trees treat it as the end.
fn index(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn opt_json(v: Option<JsonValue>) -> pb::Response {
    match v {
        Some(v) => json_value(v),
        None => errors::opt_string(None),
    }
}

fn opt_str(v: Option<String>) -> JsonValue {
    v.map(JsonValue::Str).unwrap_or(JsonValue::Null)
}

/// The one place a JSON document read is turned into text. Reads and watches
/// both use it, so they cannot disagree about the shape.
pub(crate) fn json_value(v: JsonValue) -> pb::Response {
    errors::json(v.to_json_string())
}

/// Rich text as a JSON array of the form `[{"text": .., "marks": {..}}, ..]`.
pub(crate) fn spans_value(v: Vec<Span>) -> pb::Response {
    let arr = v
        .into_iter()
        .map(|s| {
            let marks = s.marks.into_iter().map(|(k, v)| (k, JsonValue::Str(v))).collect();
            let mut o = BTreeMap::new();
            o.insert("text".to_string(), JsonValue::Str(s.text));
            o.insert("marks".to_string(), JsonValue::Object(marks));
            JsonValue::Object(o)
        })
        .collect();
    errors::json(JsonValue::Array(arr).to_json_string())
}

/// A tree as a JSON array of the form
/// `[{"id", "parent", "label", "position"}, ..]`. A node with no parent, label
/// or position has `null` there.
pub(crate) fn nodes_value(v: Vec<TreeNode>) -> pb::Response {
    let arr = v
        .into_iter()
        .map(|n| {
            let mut o = BTreeMap::new();
            o.insert("id".to_string(), JsonValue::Str(n.id));
            o.insert("parent".to_string(), opt_str(n.parent));
            o.insert("label".to_string(), opt_str(n.label));
            o.insert("position".to_string(), opt_str(n.position));
            JsonValue::Object(o)
        })
        .collect();
    errors::json(JsonValue::Array(arr).to_json_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(r: pb::Response) -> String {
        match r.result {
            Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Json(t)) })) => t,
            other => panic!("expected json text, got {other:?}"),
        }
    }

    #[test]
    fn spans_serialize_to_the_expected_shape_exactly() {
        let marked = Span {
            text: "hi".into(),
            marks: [("bold".to_string(), "1".to_string())].into_iter().collect(),
        };
        let plain = Span { text: "yo".into(), marks: Default::default() };
        assert_eq!(
            text(spans_value(vec![marked, plain])),
            r#"[{"marks":{"bold":"1"},"text":"hi"},{"marks":{},"text":"yo"}]"#
        );
    }

    #[test]
    fn nodes_serialize_with_explicit_nulls_never_omitted_fields() {
        let bare = TreeNode { id: "a".into(), parent: None, label: None, position: None };
        let full = TreeNode {
            id: "b".into(),
            parent: Some("a".into()),
            label: Some("B".into()),
            position: Some("m".into()),
        };
        assert_eq!(
            text(nodes_value(vec![bare, full])),
            r#"[{"id":"a","label":null,"parent":null,"position":null},{"id":"b","label":"B","parent":"a","position":"m"}]"#
        );
    }

    #[test]
    fn a_json_document_serializes_to_its_exact_text() {
        let v = JsonValue::from_json_str(r#"{"a":42}"#).unwrap();
        assert_eq!(text(json_value(v)), r#"{"a":42}"#);
    }
}
