//! Typed handles for shared objects. Each handle is a small value tied to one
//! object id, and it is cheap to clone. Its methods build the right `TypedCrdt`
//! and `TypedDelta` for you. A delta is one small change that is sent to the
//! other machines.

use crate::error::{Result, SdkError};
use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeHandle;
use futures::Stream;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

/// Looks at the object `id` where it is and returns what `f` makes of it.
/// Nothing is copied but the answer. `f` returns `None` for an object of
/// another type, which is an engine error, as a missing object is. A node
/// that can no longer save answers no reads, and the error says so.
///
/// `f` runs inside the engine, which does one thing at a time. This is for
/// an answer that is quick to work out: one entry, one number, or a plain
/// listing of what a map or a set holds.
async fn look<R, F>(node: &NodeHandle, id: &str, f: F) -> Result<R>
where
    F: FnOnce(&TypedCrdt) -> Option<R> + Send + 'static,
    R: Send + 'static,
{
    node.read(id, f).await.flatten().ok_or_else(|| not_done(node))
}

/// Takes a copy of the object `id` and returns what `f` makes of the copy.
///
/// This is for an answer that takes longer to work out than the object
/// takes to copy: putting a list in order, laying a text out in spans,
/// drawing up a tree. The engine is held up only for the copy, and the work
/// is done here, outside it. Which reads those are was measured, not guessed.
async fn copy<R, F>(node: &NodeHandle, id: &str, f: F) -> Result<R>
where
    F: FnOnce(&TypedCrdt) -> Option<R>,
{
    node.snapshot(id).await.as_ref().and_then(f).ok_or_else(|| not_done(node))
}

/// Runs a write that may be refused, inside the engine. `apply` makes the
/// change and returns its delta, or says why it cannot, and then it must not
/// have changed anything. A refusal comes back as that error, and no delta is
/// made, saved or sent for it.
async fn refusable<F>(node: &NodeHandle, id: &str, apply: F) -> Result<()>
where
    F: FnOnce(&mut TypedCrdt, orvu_crdt::Hlc, ReplicaId) -> Result<TypedDelta> + Send + 'static,
{
    // The engine's closure can only hand back a delta or nothing, so the
    // reason travels out through this shared slot.
    let failed = Arc::new(std::sync::Mutex::new(None));
    let slot = failed.clone();
    let outcome = node
        .try_mutate(id, move |crdt, hlc, replica| match apply(crdt, hlc, replica) {
            Ok(delta) => Some(delta),
            Err(e) => {
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                None
            }
        })
        .await;
    match outcome {
        orvu_runtime::TryOutcome::Applied => Ok(()),
        orvu_runtime::TryOutcome::NoObject => Err(SdkError::Engine),
        orvu_runtime::TryOutcome::StorageFailed => Err(not_done(node)),
        orvu_runtime::TryOutcome::Unwritable(why) => Err(SdkError::Write(why)),
        // With no reason in the slot, `apply` panicked.
        orvu_runtime::TryOutcome::Refused => {
            Err(failed.lock().unwrap_or_else(|p| p.into_inner()).take().unwrap_or(SdkError::Engine))
        }
    }
}

/// The error for a write the node did not make: the reason the object gave
/// for turning it away if there is one, otherwise as `not_done`.
fn refused_or_not_done(node: &NodeHandle, why: Option<orvu_crdt::WriteError>) -> SdkError {
    why.map_or_else(|| not_done(node), SdkError::Write)
}

/// The error for a call the node did not answer. If the node can no longer
/// save to disk that is the reason, and the caller is told so. Otherwise the
/// engine was not there, or the object was missing or of another type.
pub(crate) fn not_done(node: &NodeHandle) -> SdkError {
    unsaved_or_engine(node.storage_failure())
}

fn unsaved_or_engine(storage_failure: Option<String>) -> SdkError {
    match storage_failure {
        Some(why) => SdkError::Storage(why),
        None => SdkError::Engine,
    }
}

/// Where a watch works its value out. See `look` and `copy`.
#[derive(Clone, Copy)]
enum Work {
    InEngine,
    Outside,
}

/// The code behind every handle's `watch()`. The stream yields the current
/// value first, then one item each time the object `id` changes. It skips a
/// value equal to the one it yielded last.
///
/// If the watcher falls behind and misses change events (a broadcast `Lagged`
/// error), it reads the object again. So the watcher always ends up with the
/// latest value, even if no other change follows. The stream ends when the
/// node is shut down, and when it can no longer save to disk.
fn watch_object<T, F>(node: Arc<NodeHandle>, id: String, work: Work, read: F) -> impl Stream<Item = T> + Send
where
    T: Clone + PartialEq + Send + 'static,
    F: Fn(&TypedCrdt) -> Option<T> + Send + Sync + 'static,
{
    let read = Arc::new(read);
    let rx = node.subscribe();
    futures::stream::unfold(
        (node, id, read, rx, Option::<T>::None, false),
        move |(node, id, read, mut rx, mut last, mut started)| async move {
            loop {
                if started {
                    let event = tokio::select! {
                        _ = node.stopped() => return None,          // the node was shut down, so end the stream
                        _ = node.saving_failed() => return None,    // the node stopped serving, so end the stream
                        event = rx.recv() => event,
                    };
                    match event {
                        Ok(changed) if changed == id => {}          // this object changed, so read it again
                        Ok(_) => continue,                          // a different object, so ignore it
                        Err(RecvError::Lagged(_)) => {}             // fell behind, so read it again
                        Err(RecvError::Closed) => return None,      // the engine is gone, so end the stream
                    }
                }
                started = true;
                let val = match work {
                    Work::InEngine => {
                        let project = read.clone();
                        node.read(&id, move |crdt| project(crdt)).await.flatten()
                    }
                    Work::Outside => node.snapshot(&id).await.as_ref().and_then(|crdt| read(crdt)),
                };
                if let Some(v) = val {
                    if last.as_ref() != Some(&v) {
                        last = Some(v.clone());
                        return Some((v, (node, id, read, rx, last, started)));
                    }
                }
                // The value is the same as last time, or the object is missing
                // or has the wrong type. Wait for the next event.
            }
        },
    )
}

/// Implemented by every handle type, so that `Client::attach` can make any of
/// them.
pub trait Handle {
    #[doc(hidden)]
    fn attach(node: Arc<NodeHandle>, id: &str) -> Self;
}

macro_rules! impl_handle {
    ($($handle:ty),*) => {$(
        impl Handle for $handle {
            fn attach(node: Arc<NodeHandle>, id: &str) -> Self {
                Self { node, id: id.to_string() }
            }
        }
    )*};
}
impl_handle!(Counter, Set, Register, Map, Cart, List, Json, RichText, BoundedCounter, Tree, MvMap);

/// A counter you can add to and subtract from. The value may go below zero.
#[derive(Clone)]
pub struct Counter {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl Counter {
    /// Adds `n` to the counter.
    pub async fn increment(&self, n: u64) -> Result<()> {
        self.node
            .write(&self.id, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(replica, n)),
                _ => unreachable!("Counter handle bound to non-counter object"),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// Subtracts `n` from the counter.
    pub async fn decrement(&self, n: u64) -> Result<()> {
        self.node
            .write(&self.id, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.decrement(replica, n)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// The counter's current value on this machine.
    pub async fn value(&self) -> Result<i64> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::PnCounter(pn) => Some(pn.value()),
            _ => None,
        })
        .await
    }

    /// A live stream of the counter's value. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = i64> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::InEngine, |c| match c {
            TypedCrdt::PnCounter(pn) => Some(pn.value()),
            _ => None,
        })
    }
}

/// A set of strings.
#[derive(Clone)]
pub struct Set {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl Set {
    /// Adds `member` to the set.
    pub async fn add(&self, member: &str) -> Result<()> {
        let m = member.to_string();
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::OrSet(s) => Ok(TypedDelta::OrSet(s.add(m, replica)?)),
            _ => unreachable!(),
        })
        .await
    }

    /// Removes `member` from the set. An add made on another machine that this
    /// one has not seen yet is not affected.
    pub async fn remove(&self, member: &str) -> Result<()> {
        let m = member.to_string();
        self.node
            .write(&self.id, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.remove(&m)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// True if `member` is in the set.
    pub async fn contains(&self, member: &str) -> Result<bool> {
        let member = member.to_string();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::OrSet(s) => Some(s.contains(&member)),
            _ => None,
        })
        .await
    }

    /// Every member currently in the set.
    pub async fn elements(&self) -> Result<Vec<String>> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::OrSet(s) => Some(s.elements().cloned().collect()),
            _ => None,
        })
        .await
    }

    /// A live stream of the set's members. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = Vec<String>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::InEngine, |c| match c {
            TypedCrdt::OrSet(s) => Some(s.elements().cloned().collect()),
            _ => None,
        })
    }
}

/// A single string value. If two machines set it at the same time, the later
/// write wins (last-writer-wins).
#[derive(Clone)]
pub struct Register {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl Register {
    /// Sets the value.
    pub async fn set(&self, value: &str) -> Result<()> {
        let v = value.to_string();
        self.node
            .write(&self.id, move |crdt, hlc, replica| match crdt {
                TypedCrdt::LwwRegister(r) => TypedDelta::LwwRegister(r.set(v, hlc, replica)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// The current value, or `None` if it has never been set.
    pub async fn get(&self) -> Result<Option<String>> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::LwwRegister(r) => Some(r.get().cloned()),
            _ => None,
        })
        .await
    }

    /// A live stream of the register's value. It yields the current value
    /// first, then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = Option<String>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::InEngine, |c| match c {
            TypedCrdt::LwwRegister(r) => Some(r.get().cloned()),
            _ => None,
        })
    }
}

/// A map from string keys to string values. Each value is last-writer-wins: if
/// two machines write the same key at the same time, the later write wins.
///
/// `remove` takes a key out of the map but keeps its stored value. So adding
/// the key back carries on from the old value instead of starting fresh. See
/// the `OrMap` docs.
#[derive(Clone)]
pub struct Map {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl Map {
    /// Sets `key` to `value`.
    pub async fn put(&self, key: &str, value: &str) -> Result<()> {
        let (k, v) = (key.to_string(), value.to_string());
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::OrMap(m) => {
                Ok(TypedDelta::OrMap(m.update(k, replica, |reg| reg.set(v, hlc, replica))?))
            }
            _ => unreachable!(),
        })
        .await
    }

    /// Removes `key` from the map.
    pub async fn remove(&self, key: &str) -> Result<()> {
        let k = key.to_string();
        self.node
            .write(&self.id, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::OrMap(m) => TypedDelta::OrMap(m.remove(&k)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// The value at `key`, or `None` if the key is not in the map.
    pub async fn get(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::OrMap(m) => Some(m.get(&key).and_then(|reg| reg.get().cloned())),
            _ => None,
        })
        .await
    }

    /// The keys currently in the map.
    pub async fn keys(&self) -> Result<Vec<String>> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::OrMap(m) => Some(m.keys().cloned().collect()),
            _ => None,
        })
        .await
    }

    /// Every key in the map with its value.
    pub async fn entries(&self) -> Result<Vec<(String, String)>> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::OrMap(m) => Some(m
                .keys()
                .filter_map(|k| {
                    m.get(k)
                        .and_then(|reg| reg.get().cloned())
                        .map(|v| (k.clone(), v))
                })
                .collect()),
            _ => None,
        })
        .await
    }

    /// A live stream of the map's entries. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = Vec<(String, String)>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::InEngine, |c| match c {
            TypedCrdt::OrMap(m) => Some(
                m.keys()
                    .filter_map(|k| {
                        m.get(k).and_then(|reg| reg.get().cloned()).map(|v| (k.clone(), v))
                    })
                    .collect(),
            ),
            _ => None,
        })
    }
}

/// A map from keys to text that keeps every value written to a key at the
/// same time. Where `Map` keeps the later of two clashing writes and drops the
/// other, this keeps both, and `get` returns both until a write that has seen
/// them replaces them. More than one value at a key means a clash nothing has
/// settled yet.
///
/// A replaced or removed value is dropped, not kept.
#[derive(Clone)]
pub struct MvMap {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

fn mv_entries(m: &orvu_crdt::MvMap<String, String>) -> Vec<(String, Vec<String>)> {
    m.entries().map(|(k, vals)| (k.clone(), vals.into_iter().cloned().collect())).collect()
}

impl MvMap {
    /// Sets `key` to `value`, replacing every value this machine has seen at
    /// `key`.
    pub async fn put(&self, key: &str, value: &str) -> Result<()> {
        let (k, v) = (key.to_string(), value.to_string());
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::MvMap(m) => Ok(TypedDelta::MvMap(m.put(k, v, replica)?)),
            _ => unreachable!(),
        })
        .await
    }

    /// Sets `key` to `value` as `put` does, if `check`, given the values at
    /// `key` at that moment, allows it. The check and the write are one step
    /// in the engine, so nothing can change the values in between. A check
    /// that says no changes nothing, and its reason comes back as
    /// `SdkError::NotAllowed`.
    pub async fn put_if(
        &self,
        key: &str,
        value: &str,
        check: impl FnOnce(&[String]) -> std::result::Result<(), String> + Send + 'static,
    ) -> Result<()> {
        let (k, v) = (key.to_string(), value.to_string());
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::MvMap(m) => {
                let now: Vec<String> = m.get(&k).into_iter().cloned().collect();
                check(&now).map_err(SdkError::NotAllowed)?;
                Ok(TypedDelta::MvMap(m.put(k, v, replica)?))
            }
            _ => Err(SdkError::Engine),
        })
        .await
    }

    /// Removes `key` as `remove` does, if `check`, given the values at `key`
    /// at that moment, allows it, in one step as `put_if` does.
    pub async fn remove_if(
        &self,
        key: &str,
        check: impl FnOnce(&[String]) -> std::result::Result<(), String> + Send + 'static,
    ) -> Result<()> {
        let k = key.to_string();
        refusable(&self.node, &self.id, move |crdt, _hlc, _replica| match crdt {
            TypedCrdt::MvMap(m) => {
                let now: Vec<String> = m.get(&k).into_iter().cloned().collect();
                check(&now).map_err(SdkError::NotAllowed)?;
                Ok(TypedDelta::MvMap(m.remove(&k)))
            }
            _ => Err(SdkError::Engine),
        })
        .await
    }

    /// Removes every value this machine has seen at `key`.
    pub async fn remove(&self, key: &str) -> Result<()> {
        let k = key.to_string();
        self.node
            .write(&self.id, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::MvMap(m) => TypedDelta::MvMap(m.remove(&k)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// The values at `key`: none if the key is not in the map, one normally,
    /// and more than one while writes made at the same time clash.
    pub async fn get(&self, key: &str) -> Result<Vec<String>> {
        let key = key.to_string();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::MvMap(m) => Some(m.get(&key).into_iter().cloned().collect()),
            _ => None,
        })
        .await
    }

    /// The keys currently in the map.
    pub async fn keys(&self) -> Result<Vec<String>> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::MvMap(m) => Some(m.keys().cloned().collect()),
            _ => None,
        })
        .await
    }

    /// Every key in the map with its values.
    pub async fn entries(&self) -> Result<Vec<(String, Vec<String>)>> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::MvMap(m) => Some(mv_entries(m)),
            _ => None,
        })
        .await
    }

    /// A live stream of the map's entries. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = Vec<(String, Vec<String>)>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::InEngine, |c| match c {
            TypedCrdt::MvMap(m) => Some(mv_entries(m)),
            _ => None,
        })
    }
}

/// A shopping cart: a map from item name to a quantity counter.
///
/// A quantity is a signed counter, and `remove_qty` does not check it. So a
/// quantity can go below zero, for example when two machines take items out at
/// the same time. If a number must never go below zero, use `BoundedCounter`.
///
/// `remove_item` takes an item out of the cart but keeps its stored counter. So
/// adding the item back carries on from the old quantity instead of starting
/// from zero. See the `OrMap` docs.
#[derive(Clone)]
pub struct Cart {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl Cart {
    /// Adds `qty` to the quantity of `item`, putting the item in the cart if it
    /// is not there.
    pub async fn add(&self, item: &str, qty: u64) -> Result<()> {
        let k = item.to_string();
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::OrMapCounter(m) => {
                Ok(TypedDelta::OrMapCounter(m.update(k, replica, |pn| pn.increment(replica, qty))?))
            }
            _ => unreachable!("Cart handle bound to non-cart object"),
        })
        .await
    }

    /// Subtracts `qty` from the quantity of `item`. The quantity may go below
    /// zero.
    pub async fn remove_qty(&self, item: &str, qty: u64) -> Result<()> {
        let k = item.to_string();
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::OrMapCounter(m) => {
                Ok(TypedDelta::OrMapCounter(m.update(k, replica, |pn| pn.decrement(replica, qty))?))
            }
            _ => unreachable!(),
        })
        .await
    }

    /// Takes `item` out of the cart.
    pub async fn remove_item(&self, item: &str) -> Result<()> {
        let k = item.to_string();
        self.node
            .write(&self.id, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::OrMapCounter(m) => TypedDelta::OrMapCounter(m.remove(&k)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// The quantity of `item`, or 0 if it is not in the cart.
    pub async fn quantity(&self, item: &str) -> Result<i64> {
        let item = item.to_string();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::OrMapCounter(m) => Some(m.get(&item).map(|pn| pn.value()).unwrap_or(0)),
            _ => None,
        })
        .await
    }

    /// Every item in the cart with its quantity.
    pub async fn items(&self) -> Result<Vec<(String, i64)>> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::OrMapCounter(m) => Some(m
                .keys()
                .map(|k| (k.clone(), m.get(k).map(|pn| pn.value()).unwrap_or(0)))
                .collect()),
            _ => None,
        })
        .await
    }

    /// A live stream of the cart's items. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = Vec<(String, i64)>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::Outside, |c| match c {
            TypedCrdt::OrMapCounter(m) => Some(
                m.keys()
                    .map(|k| (k.clone(), m.get(k).map(|pn| pn.value()).unwrap_or(0)))
                    .collect(),
            ),
            _ => None,
        })
    }
}

/// An ordered list of strings that several machines can edit and merge. The
/// algorithm is known as RGA (Replicated Growable Array).
///
/// An index counts only the elements you can see, not the deleted ones.
/// `insert` and `delete` read a fresh copy of the list, turn the index into the
/// permanent id (dot) of an element, and then make the change. The list may
/// change in between, and that is safe. An insert after an element that was
/// deleted in the meantime still has that element to hang on to, and deleting
/// an element twice changes nothing.
#[derive(Clone)]
pub struct List {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl List {
    /// Inserts `value` so it ends up at position `index`. `index` may be
    /// anything from 0 to the list's length, and the length appends. A larger
    /// index returns `OutOfBounds`.
    pub async fn insert(&self, index: usize, value: impl Into<String>) -> Result<()> {
        let value = value.into();
        let vis = copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::Rga(l) => Some(l.visible_dots()),
            _ => None,
        })
        .await?;
        if index > vis.len() {
            return Err(SdkError::OutOfBounds { index, len: vis.len() });
        }
        let predecessor = if index == 0 { None } else { Some(vis[index - 1]) };
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::Rga(l) => Ok(TypedDelta::Rga(l.insert_after(predecessor, value, replica)?)),
            _ => unreachable!("List handle bound to non-list object"),
        })
        .await
    }

    /// Deletes the visible element at position `index`. `index` must be less
    /// than the list's length, or this returns `OutOfBounds`.
    pub async fn delete(&self, index: usize) -> Result<()> {
        let vis = copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::Rga(l) => Some(l.visible_dots()),
            _ => None,
        })
        .await?;
        if index >= vis.len() {
            return Err(SdkError::OutOfBounds { index, len: vis.len() });
        }
        let target = vis[index];
        self.node
            .write(&self.id, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::Rga(l) => TypedDelta::Rga(l.tombstone(target)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// The visible element at `index`, or `None` if there is none.
    pub async fn get(&self, index: usize) -> Result<Option<String>> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::Rga(l) => Some(l.get(index).cloned()),
            _ => None,
        })
        .await
    }

    /// How many elements are visible.
    pub async fn len(&self) -> Result<usize> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::Rga(l) => Some(l.len()),
            _ => None,
        })
        .await
    }

    /// A copy of the visible elements, in order.
    pub async fn to_vec(&self) -> Result<Vec<String>> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::Rga(l) => Some(l.to_vec()),
            _ => None,
        })
        .await
    }

    /// A live stream of the list's contents. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = Vec<String>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::Outside, |c| match c {
            TypedCrdt::Rga(l) => Some(l.to_vec()),
            _ => None,
        })
    }
}

/// A JSON document that several machines can edit and merge: nested objects,
/// arrays and plain values. A path says where in the document to read or write.
/// A write may go down through object keys but not through an array index. To
/// change something inside an array element, replace the whole element.
///
/// A write that cannot be made returns `SdkError::Json` and changes nothing.
/// The check and the write happen in one step inside the engine, so nothing
/// can change the document in between.
#[derive(Clone)]
pub struct Json {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

/// One write to a JSON document, for `Json::change_if`.
#[derive(Clone, Debug)]
pub enum JsonChange {
    /// As `Json::set`.
    Set { path: Vec<orvu_crdt::Seg>, value: orvu_crdt::JsonValue },
    /// As `Json::insert`.
    Insert { path: Vec<orvu_crdt::Seg>, index: usize, value: orvu_crdt::JsonValue },
    /// As `Json::remove`.
    Remove { path: Vec<orvu_crdt::Seg> },
}

impl Json {
    /// Runs one write on the document inside the engine and passes its
    /// `JsonError` back to the caller if it fails. A write that fails changes
    /// nothing, so no delta is made, saved or sent for it.
    async fn write(
        &self,
        apply: impl FnOnce(
                &mut orvu_crdt::JsonCrdt,
                orvu_crdt::Hlc,
                ReplicaId,
            ) -> std::result::Result<orvu_crdt::JsonDelta, orvu_crdt::JsonError>
            + Send
            + 'static,
    ) -> Result<()> {
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::Json(j) => Ok(TypedDelta::Json(apply(j, hlc, replica)?)),
            _ => Err(SdkError::Engine),
        })
        .await
    }

    /// Sets the value at `path`, creating missing objects along the way.
    pub async fn set(&self, path: &[orvu_crdt::Seg], value: orvu_crdt::JsonValue) -> Result<()> {
        let path = path.to_vec();
        self.write(move |j, hlc, replica| j.set(&path, &value, hlc, replica)).await
    }

    /// Inserts `value` at position `index` of the array at `path`.
    pub async fn insert(
        &self,
        path: &[orvu_crdt::Seg],
        index: usize,
        value: orvu_crdt::JsonValue,
    ) -> Result<()> {
        let path = path.to_vec();
        self.write(move |j, hlc, replica| j.insert(&path, index, &value, hlc, replica)).await
    }

    /// Removes the value at `path`.
    pub async fn remove(&self, path: &[orvu_crdt::Seg]) -> Result<()> {
        let path = path.to_vec();
        self.write(move |j, hlc, replica| j.remove(&path, hlc, replica)).await
    }

    /// Runs one write, as `set`, `insert` or `remove` (`change`), if `check`,
    /// given what is at `at` at that moment, allows it. The check and the
    /// write are one step in the engine, so nothing can change the document
    /// in between. A check that says no changes nothing, and its reason comes
    /// back as `SdkError::NotAllowed`.
    pub async fn change_if(
        &self,
        change: JsonChange,
        at: &[orvu_crdt::Seg],
        check: impl FnOnce(Option<&orvu_crdt::JsonValue>) -> std::result::Result<(), String> + Send + 'static,
    ) -> Result<()> {
        self.change_with(at, move |now| check(now).map(|()| vec![change])).await
    }

    /// Runs the writes `decide` gives, given what is at `at` at that moment,
    /// one after the other in one step in the engine, and sends them to the
    /// other machines as one change: nothing can come between them, here or
    /// there. If `decide` says no, nothing changes and its reason comes back
    /// as `SdkError::NotAllowed`. If the first write fails, nothing changes;
    /// if a later one fails, the writes before it are kept and the rest are
    /// not made.
    pub async fn change_with(
        &self,
        at: &[orvu_crdt::Seg],
        decide: impl FnOnce(Option<&orvu_crdt::JsonValue>) -> std::result::Result<Vec<JsonChange>, String> + Send + 'static,
    ) -> Result<()> {
        let at = at.to_vec();
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::Json(j) => {
                let changes = decide(j.get(&at).as_ref()).map_err(SdkError::NotAllowed)?;
                let mut made: Option<orvu_crdt::JsonDelta> = None;
                for change in changes {
                    let delta = match change {
                        JsonChange::Set { path, value } => j.set(&path, &value, hlc, replica),
                        JsonChange::Insert { path, index, value } => j.insert(&path, index, &value, hlc, replica),
                        JsonChange::Remove { path } => j.remove(&path, hlc, replica),
                    };
                    made = match (made, delta) {
                        (None, Err(e)) => return Err(e.into()),
                        (Some(made), Err(_)) => return Ok(TypedDelta::Json(made)),
                        (None, Ok(delta)) => Some(delta),
                        (Some(made), Ok(delta)) => Some(made.join(delta)),
                    };
                }
                made.map(TypedDelta::Json).ok_or_else(|| SdkError::NotAllowed("nothing to change".into()))
            }
            _ => Err(SdkError::Engine),
        })
        .await
    }

    /// Reads the value at `path`, or returns `None` if nothing is there.
    pub async fn get(&self, path: &[orvu_crdt::Seg]) -> Result<Option<orvu_crdt::JsonValue>> {
        let path = path.to_vec();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::Json(j) => Some(j.get(&path)),
            _ => None,
        })
        .await
    }

    /// The whole document as a plain `JsonValue`.
    pub async fn to_value(&self) -> Result<orvu_crdt::JsonValue> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::Json(j) => Some(j.to_value()),
            _ => None,
        })
        .await
    }

    /// A live stream of the whole document. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = orvu_crdt::JsonValue> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::InEngine, |c| match c {
            TypedCrdt::Json(j) => Some(j.to_value()),
            _ => None,
        })
    }
}

/// Text with formatting that several machines can edit and merge. The text is a
/// list of characters (an RGA, like `List`), and each character has its own
/// last-writer-wins marks, such as bold. An index counts only the visible
/// characters.
#[derive(Clone)]
pub struct RichText {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl RichText {
    /// Inserts the text `s` at `index`. Each Unicode code point becomes one
    /// character.
    pub async fn insert(&self, index: usize, s: impl Into<String>) -> Result<()> {
        let s = s.into();
        // The whole text goes in as one change. Sent one character at a time,
        // another insert could land between two of them and split the text.
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::RichText(t) => Ok(TypedDelta::RichText(t.insert_str(index, &s, replica)?)),
            _ => unreachable!("RichText handle bound to non-richtext object"),
        })
        .await
    }

    /// Deletes `len` visible characters starting at `index`. A range that runs
    /// past the end stops at the end.
    pub async fn delete(&self, index: usize, len: usize) -> Result<()> {
        self.node
            .write(&self.id, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::RichText(t) => TypedDelta::RichText(t.delete(index, len, replica)),
                _ => unreachable!(),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// Sets the mark `attr` to `value` on the characters from `index` up to,
    /// but not including, `index + len`. An empty `value` is refused with
    /// `SdkError::EmptyValue` and changes nothing. Use `unformat` to remove a
    /// mark.
    pub async fn format(
        &self,
        index: usize,
        len: usize,
        attr: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<()> {
        let (attr, value) = (attr.into(), value.into());
        if value.is_empty() {
            return Err(SdkError::EmptyValue);
        }
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::RichText(t) => {
                Ok(TypedDelta::RichText(t.format(index, len, &attr, &value, hlc, replica)?))
            }
            _ => unreachable!(),
        })
        .await
    }

    /// Inserts the text `s` at `index` and sets the mark `attr` to `value` on
    /// exactly the characters it put in, as one change. The mark is tied to
    /// those characters, not to their place, so text another machine puts in
    /// at the same moment never takes it, and this text never goes without
    /// it. An empty `value` is refused with `SdkError::EmptyValue` and changes
    /// nothing.
    pub async fn insert_marked(
        &self,
        index: usize,
        s: impl Into<String>,
        attr: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<()> {
        let (s, attr, value) = (s.into(), attr.into(), value.into());
        if value.is_empty() {
            return Err(SdkError::EmptyValue);
        }
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::RichText(t) => {
                Ok(TypedDelta::RichText(t.insert_marked(index, &s, &attr, &value, hlc, replica)?))
            }
            _ => unreachable!("RichText handle bound to non-richtext object"),
        })
        .await
    }

    /// Removes the mark `attr` from the characters from `index` up to, but not
    /// including, `index + len`.
    pub async fn unformat(&self, index: usize, len: usize, attr: impl Into<String>) -> Result<()> {
        let attr = attr.into();
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::RichText(t) => {
                Ok(TypedDelta::RichText(t.unformat(index, len, &attr, hlc, replica)?))
            }
            _ => unreachable!(),
        })
        .await
    }

    /// The visible text as one string.
    pub async fn text(&self) -> Result<String> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::RichText(t) => Some(t.text()),
            _ => None,
        })
        .await
    }

    /// The document as spans: runs of neighbouring characters that have the
    /// same marks.
    pub async fn to_spans(&self) -> Result<Vec<orvu_crdt::Span>> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::RichText(t) => Some(t.to_spans()),
            _ => None,
        })
        .await
    }

    /// A live stream of the document's spans. It yields the current value
    /// first, then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = Vec<orvu_crdt::Span>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::Outside, |c| match c {
            TypedCrdt::RichText(t) => Some(t.to_spans()),
            _ => None,
        })
    }
}

/// The lock withdrawals from one bounded counter on one node take turns on.
/// Every handle to the same object on the same node gets the same lock. An
/// entry stays only while some withdrawal holds or waits for its lock, so the
/// table holds no more than the withdrawals running right now.
fn spending_turn(node: &Arc<NodeHandle>, id: &str) -> Arc<tokio::sync::Mutex<()>> {
    type Turns = std::collections::HashMap<(usize, String), std::sync::Weak<tokio::sync::Mutex<()>>>;
    static TURNS: std::sync::LazyLock<std::sync::Mutex<Turns>> = std::sync::LazyLock::new(Default::default);
    // The node is told apart by where it lives in memory. The caller holds
    // the node, so no other node can be at that address while the entry is
    // in use.
    let key = (Arc::as_ptr(node) as usize, id.to_string());
    let mut turns = TURNS.lock().unwrap_or_else(|e| e.into_inner());
    turns.retain(|_, held| held.strong_count() > 0);
    if let Some(turn) = turns.get(&key).and_then(|held| held.upgrade()) {
        return turn;
    }
    let turn = Arc::new(tokio::sync::Mutex::new(()));
    turns.insert(key, Arc::downgrade(&turn));
    turn
}

/// Everything other machines have ever handed to `me`. A grant that
/// arrives raises this by its amount, even if it is spent at once, and
/// nothing `me` gives away or spends meanwhile lowers it.
fn ever_received(b: &orvu_crdt::BoundedCounter, me: ReplicaId) -> u64 {
    b.received(me)
}

/// A bounded counter: a counter that cannot go below zero, even while machines
/// cannot reach each other. Each machine has an allowance it may spend.
///
/// `transfer` fails with `Exhausted` when this machine's allowance is too
/// small. `try_decrement` first tries to borrow from another machine, and fails
/// with `Exhausted` if that does not work. Use `transfer` to hand allowance to
/// another machine, and `increment` to add stock, which also adds allowance.
#[derive(Clone)]
pub struct BoundedCounter {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl BoundedCounter {
    /// Adds `n` to the counter and gives this machine that much allowance.
    pub async fn increment(&self, n: u64) -> Result<()> {
        self.node.write(&self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::BoundedCounter(b) => TypedDelta::BoundedCounter(b.increment(n, replica)),
            _ => unreachable!("BoundedCounter handle bound to non-bounded object"),
        }).await.map(|_| ()).map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// Adds `n` as `increment` does, if the counter can hold it whole. If it
    /// cannot, nothing is added and this returns `NotAllowed`, where
    /// `increment` would add less than `n`.
    pub async fn try_increment(&self, n: u64) -> Result<()> {
        refusable(&self.node, &self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::BoundedCounter(b) => b
                .try_increment(n, replica)
                .map(TypedDelta::BoundedCounter)
                .ok_or_else(|| SdkError::NotAllowed(format!("{n} cannot be added whole: the counter cannot hold that much"))),
            _ => Err(SdkError::Engine),
        })
        .await
    }

    /// Subtracts `n` if this machine's allowance covers it. If not, it asks one
    /// connected machine to lend the missing amount, waits up to the node's
    /// `borrow_timeout` for it, and tries again. Returns `Exhausted` if the
    /// allowance is still too small.
    ///
    /// A withdrawal this machine's allowance covers goes straight through,
    /// even while another one on the same counter is borrowing. Withdrawals
    /// that have to borrow take turns, whichever handle they come through.
    /// Otherwise two of them could both wake on the first grant, and the one
    /// that lost would be refused while its own grant was still on the way.
    pub async fn try_decrement(&self, n: u64) -> Result<()> {
        match self.attempt_decrement(n).await {
            orvu_runtime::TryOutcome::Applied => return Ok(()),
            orvu_runtime::TryOutcome::NoObject => return Err(SdkError::Engine),
            orvu_runtime::TryOutcome::StorageFailed => return Err(not_done(&self.node)),
            orvu_runtime::TryOutcome::Unwritable(why) => return Err(SdkError::Write(why)),
            orvu_runtime::TryOutcome::Refused => {}
        }
        // The allowance was too small, so this one has to borrow. Wait for
        // any other borrower on this counter to finish first.
        let turn = spending_turn(&self.node, &self.id);
        let _turn = turn.lock().await;
        let deadline = tokio::time::Instant::now() + self.node.borrow_timeout();
        loop {
            // Try again first. A grant may have come in while we waited for
            // our turn, or while we borrowed.
            match self.attempt_decrement(n).await {
                orvu_runtime::TryOutcome::Applied => return Ok(()),
                orvu_runtime::TryOutcome::NoObject => return Err(SdkError::Engine),
                orvu_runtime::TryOutcome::StorageFailed => return Err(not_done(&self.node)),
                orvu_runtime::TryOutcome::Unwritable(why) => return Err(SdkError::Write(why)),
                orvu_runtime::TryOutcome::Refused => {}
            }
            // A withdrawal that did not need to borrow can spend part of our
            // grant before we do. Then we borrow again, until the deadline.
            if tokio::time::Instant::now() >= deadline || !self.borrow_until(n, deadline).await {
                return Err(SdkError::Exhausted);
            }
        }
    }

    async fn attempt_decrement(&self, n: u64) -> orvu_runtime::TryOutcome {
        self.node.try_mutate(&self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::BoundedCounter(b) => b.try_decrement(n, replica).map(TypedDelta::BoundedCounter),
            _ => None,
        }).await
    }

    /// Borrows allowance from one other machine so that `try_decrement(n)` can
    /// then succeed. Returns true if, before `deadline`, this machine's
    /// allowance reached `n` or the amount asked for arrived. It may arrive
    /// and be spent at once by a withdrawal that did not need to borrow, or
    /// this machine may give allowance away while it waits, so the allowance
    /// alone would not show it. What other machines have handed to this one
    /// does. The counter still cannot go
    /// below zero, because the other machine only gives from its own
    /// allowance.
    async fn borrow_until(&self, n: u64, deadline: tokio::time::Instant) -> bool {
        let me = self.node.replica_id();
        let Ok(cur) = self.local_allowance().await else { return false };
        if cur >= n { return true; }
        let needed = n - cur;
        // Only the machines connected to us right now can be asked. Others
        // are known only through other machines, or have stopped answering.
        // Asking who is connected changes nothing and costs little.
        let connected = self.node.connected_peers();
        // What each of them may still give, by our own copy of the counter,
        // and how much other machines have handed us so far.
        let (others, before) = self
            .node
            .read(&self.id, move |crdt| match crdt {
                TypedCrdt::BoundedCounter(b) => {
                    let others = connected.into_iter().map(|p| (p, b.allowance(p))).collect::<Vec<_>>();
                    (others, ever_received(b, me))
                }
                _ => (Vec::new(), 0),
            })
            .await
            .unwrap_or_default();
        // Ask the first one that has enough allowance to spare, going by our
        // own copy of the counter. A connection can close between the look
        // and the ask, and `send_borrow_request` returns false then, so we
        // move on to the next.
        let mut asked = false;
        for target in others.into_iter().filter(|&(p, has)| p != me && has >= needed).map(|(p, _)| p) {
            if self.node.send_borrow_request(target, &self.id, needed).await {
                asked = true;
                break;
            }
        }
        if !asked {
            return false;
        }
        // Wait for the delta that raises our allowance, until the deadline.
        let id = self.id.clone();
        let node = self.node.clone();
        let wait = async move {
            let mut rx = node.subscribe();
            loop {
                let now = node.read(&id, move |crdt| match crdt {
                    TypedCrdt::BoundedCounter(b) => (b.allowance(me), ever_received(b, me)),
                    _ => (0, 0),
                });
                if now.await.is_some_and(|(has, got)| has >= n || got >= before.saturating_add(needed)) {
                    return true;
                }
                match rx.recv().await {
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
                }
            }
        };
        matches!(tokio::time::timeout_at(deadline, wait).await, Ok(true))
    }

    /// Moves `n` of this machine's allowance to the machine `to`. Returns
    /// `Exhausted` if this machine's allowance is less than `n`.
    pub async fn transfer(&self, to: ReplicaId, n: u64) -> Result<()> {
        match self.node.try_mutate(&self.id, move |crdt, _hlc, replica| match crdt {
            TypedCrdt::BoundedCounter(b) => b.transfer(to, n, replica).map(TypedDelta::BoundedCounter),
            _ => None,
        }).await {
            orvu_runtime::TryOutcome::Applied => Ok(()),
            orvu_runtime::TryOutcome::Refused => Err(SdkError::Exhausted),
            orvu_runtime::TryOutcome::NoObject => Err(SdkError::Engine),
            orvu_runtime::TryOutcome::StorageFailed => Err(not_done(&self.node)),
            orvu_runtime::TryOutcome::Unwritable(why) => Err(SdkError::Write(why)),
        }
    }

    /// The counter's current value on this machine.
    pub async fn value(&self) -> Result<i64> {
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::BoundedCounter(b) => Some(b.value()),
            _ => None,
        })
        .await
    }

    /// How much this machine may still subtract or transfer.
    pub async fn local_allowance(&self) -> Result<u64> {
        let replica = self.node.replica_id();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::BoundedCounter(b) => Some(b.allowance(replica)),
            _ => None,
        })
        .await
    }

    /// A live stream of the counter's value. It yields the current value first,
    /// then a new value each time it changes. An unchanged value is not
    /// repeated, and a watcher that falls behind still gets the latest value.
    pub fn watch(&self) -> impl Stream<Item = i64> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::InEngine, |c| match c {
            TypedCrdt::BoundedCounter(b) => Some(b.value()),
            _ => None,
        })
    }
}

/// A tree that several machines can edit and merge. Nodes are string ids, and
/// the only way to change the tree's shape is to move a node under a parent
/// with `move_node`. Moves made at the same time on different machines end up
/// as the same tree everywhere. A move that would create a loop is skipped
/// without an error.
#[derive(Clone)]
pub struct Tree {
    pub(crate) node: Arc<NodeHandle>,
    pub(crate) id: String,
}

impl Tree {
    /// Moves `child` under `new_parent`. A move that would create a loop, by
    /// putting a node under itself or under one of its own descendants, is
    /// skipped without an error.
    pub async fn move_node(&self, child: &str, new_parent: &str) -> Result<()> {
        let (child, new_parent) = (child.to_string(), new_parent.to_string());
        self.node
            .write(&self.id, move |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.move_node(child, new_parent, hlc, replica)),
                _ => unreachable!("Tree handle bound to non-tree object"),
            })
            .await
            .map(|_| ())
            .map_err(|why| refused_or_not_done(&self.node, why))
    }

    /// Moves `child` under `new_parent` and places it at `index` among that
    /// parent's other children. An `index` past the end puts it last. Reorders
    /// made at the same time on different machines end up in the same order
    /// everywhere.
    pub async fn move_node_at(&self, child: &str, new_parent: &str, index: usize) -> Result<()> {
        let (child, new_parent) = (child.to_string(), new_parent.to_string());
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::Tree(t) => {
                Ok(TypedDelta::Tree(t.move_node_at(child, new_parent, index, hlc, replica)?))
            }
            _ => unreachable!("Tree handle bound to non-tree object"),
        })
        .await
    }

    /// The current parent of `node`, if any.
    pub async fn parent(&self, node: &str) -> Result<Option<String>> {
        let node = node.to_string();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::Tree(t) => Some(t.parent(&node).cloned()),
            _ => None,
        })
        .await
    }

    /// The children of `node` in sibling order. Children that were never placed
    /// with `move_node_at` come first, in id order.
    pub async fn children(&self, node: &str) -> Result<Vec<String>> {
        let node = node.to_string();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::Tree(t) => Some(t.children(&node)),
            _ => None,
        })
        .await
    }

    /// Every node id in the tree, as a child or as a parent, sorted.
    pub async fn nodes(&self) -> Result<Vec<String>> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::Tree(t) => Some(t.nodes()),
            _ => None,
        })
        .await
    }

    /// Sets `node`'s label. A label does not depend on the shape of the tree.
    /// You may label a node that has never been moved into the tree, and a
    /// label stays with its node when the node is moved. If two machines label
    /// the same node at the same time, the later write wins (last-writer-wins).
    pub async fn set_label(&self, node: &str, name: &str) -> Result<()> {
        let (node, name) = (node.to_string(), name.to_string());
        refusable(&self.node, &self.id, move |crdt, hlc, replica| match crdt {
            TypedCrdt::Tree(t) => Ok(TypedDelta::Tree(t.set_label(node, name, hlc, replica)?)),
            _ => unreachable!("Tree handle bound to non-tree object"),
        })
        .await
    }

    /// `node`'s current label, if one was ever set.
    pub async fn label(&self, node: &str) -> Result<Option<String>> {
        let node = node.to_string();
        look(&self.node, &self.id, move |crdt| match crdt {
            TypedCrdt::Tree(t) => Some(t.label(&node)),
            _ => None,
        })
        .await
    }

    /// Every node the tree knows about, whether it was moved or only labelled,
    /// with its parent and label. Sorted by id.
    pub async fn to_nodes(&self) -> Result<Vec<orvu_crdt::TreeNode>> {
        copy(&self.node, &self.id, |crdt| match crdt {
            TypedCrdt::Tree(t) => Some(t.to_nodes()),
            _ => None,
        })
        .await
    }

    /// A live stream of every node, as `to_nodes` returns them. It yields the
    /// current list first, then a new list each time it changes. Each node
    /// carries its label, so a rename also yields a new list. A list of only
    /// parent links would not show it.
    pub fn watch(&self) -> impl Stream<Item = Vec<orvu_crdt::TreeNode>> + Send {
        watch_object(self.node.clone(), self.id.clone(), Work::Outside, |c| match c {
            TypedCrdt::Tree(t) => Some(t.to_nodes()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Client;
    use orvu_crdt::ReplicaId;
    use orvu_runtime::NodeConfig;
    use orvu_transport::mem::MemTransport;
    use std::sync::Arc;
    use crate::SdkError;

    async fn client(id: u64, name: &str) -> Client {
        let fabric = Arc::new(MemTransport::new());
        Client::start(fabric.endpoint(id), NodeConfig::new(ReplicaId(id), name))
            .await
            .unwrap()
    }

    // A change the node did not take because it cannot save says so, and is
    // not passed off as the engine being gone.
    #[test]
    fn a_change_refused_for_storage_says_storage() {
        match super::unsaved_or_engine(Some("disk full".into())) {
            crate::SdkError::Storage(why) => assert_eq!(why, "disk full"),
            other => panic!("expected Storage, got {other:?}"),
        }
        assert!(matches!(super::unsaved_or_engine(None), crate::SdkError::Engine));
    }

    // Once a change could not be saved, the client is told so on every call
    // after it, reads included, so the unsaved change is never shown.
    #[tokio::test]
    async fn after_a_failed_save_every_call_says_storage() {
        let store = orvu_runtime::MemStore::new();
        let mut cfg = NodeConfig::new(ReplicaId(1), "unsaved");
        cfg.fsync = orvu_runtime::FsyncPolicy::Always;
        let fabric = Arc::new(MemTransport::new());
        let c = Client::start_with_store(fabric.endpoint(1), cfg, Box::new(store.clone())).await.unwrap();
        let counter = c.counter("n").await.unwrap();
        let list = c.list("l").await.unwrap();
        counter.increment(1).await.unwrap();

        store.fail_saving(true);
        assert!(matches!(counter.increment(10).await, Err(SdkError::Storage(_))));
        let storage = |r: std::result::Result<(), SdkError>, what: &str| match r {
            Err(SdkError::Storage(why)) => assert!(!why.is_empty(), "{what}"),
            other => panic!("{what}: expected Storage, got {other:?}"),
        };
        storage(counter.value().await.map(|v| panic!("the counter read {v}")), "a look");
        storage(list.to_vec().await.map(|v| panic!("the list read {v:?}")), "a copy");
        storage(c.counter("other").await.map(|_| ()), "an open");
        assert!(c.storage_failure().is_some());
    }

    // A watch ends once its node can no longer save, even with no change
    // to wake it.
    #[tokio::test]
    async fn a_watch_ends_when_saving_fails() {
        use futures::StreamExt;
        let store = orvu_runtime::MemStore::new();
        let mut cfg = NodeConfig::new(ReplicaId(1), "unsaved-watch");
        cfg.fsync = orvu_runtime::FsyncPolicy::Always;
        let fabric = Arc::new(MemTransport::new());
        let c = Client::start_with_store(fabric.endpoint(1), cfg, Box::new(store.clone())).await.unwrap();
        let counter = c.counter("n").await.unwrap();
        counter.increment(1).await.unwrap();
        let mut stream = Box::pin(counter.watch());
        assert_eq!(stream.next().await, Some(1));

        store.fail_saving(true);
        let _ = counter.increment(10).await;
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .expect("the watch did not end when saving failed");
        assert_eq!(next, None, "the watch showed a value after saving failed");
    }

    // A healthy node reports no storage failure, so its errors stay as they were.
    // A write made only if its check allows it is checked against the object
    // as it is, and one the check turns away changes nothing.
    #[tokio::test]
    async fn a_checked_write_sees_the_object_as_it_is_and_may_be_turned_away() {
        use orvu_crdt::{JsonValue, Seg};
        let c = client(1, "checked").await;
        let notes = c.mv_map("notes").await.unwrap();
        notes.put("plan", "bob").await.unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let saw = seen.clone();
        let refused = notes.put_if("plan", "alice", move |now| {
            *saw.lock().unwrap() = now.to_vec();
            Err("not yours".into())
        });
        assert!(matches!(refused.await, Err(SdkError::NotAllowed(why)) if why == "not yours"));
        assert_eq!(*seen.lock().unwrap(), ["bob"]);
        assert!(matches!(notes.remove_if("plan", |_| Err("no".into())).await, Err(SdkError::NotAllowed(_))));
        assert_eq!(notes.get("plan").await.unwrap(), ["bob"]);
        notes.put_if("plan", "alice", |now| if now == ["bob"] { Ok(()) } else { Err("changed".into()) }).await.unwrap();
        notes.remove_if("plan", |now| if now == ["alice"] { Ok(()) } else { Err("changed".into()) }).await.unwrap();
        assert!(notes.get("plan").await.unwrap().is_empty());

        let board = c.json("board").await.unwrap();
        let key = |k: &str| vec![Seg::Key(k.into())];
        board.set(&key("t"), JsonValue::Str("bob".into())).await.unwrap();
        let (set, at) = (crate::JsonChange::Set { path: key("t"), value: JsonValue::Str("alice".into()) }, key("t"));
        let refused = board.change_if(set.clone(), &at, |now| match now {
            Some(JsonValue::Str(by)) if by == "bob" => Err("bob's".into()),
            _ => Ok(()),
        });
        assert!(matches!(refused.await, Err(SdkError::NotAllowed(_))));
        assert_eq!(board.get(&key("t")).await.unwrap(), Some(JsonValue::Str("bob".into())));
        board.change_if(set, &at, |now| if now.is_some() { Ok(()) } else { Err("gone".into()) }).await.unwrap();
        assert_eq!(board.get(&key("t")).await.unwrap(), Some(JsonValue::Str("alice".into())));
    }

    #[tokio::test]
    async fn a_healthy_node_reports_no_storage_failure() {
        let c = client(1, "healthy").await;
        assert_eq!(c.node().storage_failure(), None);
    }

    /// A client whose saved data already holds the counter "n" and the
    /// bounded counter "b", each with the record `seen`, as if other
    /// machines' changes had brought it there.
    async fn client_with_counter_seen(seen: orvu_crdt::DotContext) -> Client {
        use orvu_runtime::Store;
        let mut store = orvu_runtime::MemStore::new();
        let entry = |id: &str, crdt: orvu_core::TypedCrdt| orvu_core::SnapshotEntry {
            object_id: id.into(),
            state: crdt.snapshot(),
            context: seen.clone(),
        };
        let entries = [
            entry("n", orvu_core::TypedCrdt::PnCounter(Default::default())),
            entry("b", orvu_core::TypedCrdt::BoundedCounter(Default::default())),
        ];
        store.compact_to(&entries, &[]).unwrap();
        let fabric = Arc::new(MemTransport::new());
        Client::start_with_store(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "seeded"), Box::new(store))
            .await
            .unwrap()
    }

    // A record that has used up this machine's change numbers on an object,
    // which only a hostile machine can bring about, leaves the object for
    // reading only. A write says so plainly.
    #[tokio::test]
    async fn a_write_with_no_change_numbers_left_says_so() {
        let mut seen = orvu_crdt::DotContext::new();
        let mut vv = orvu_crdt::VersionVector::new();
        vv.observe(ReplicaId(1), orvu_crdt::MAX_COUNTER);
        seen.merge(&orvu_crdt::DotContext::from_parts(vv, []));
        let c = client_with_counter_seen(seen).await;
        let counter = c.counter("n").await.unwrap();
        match counter.increment(1).await {
            Err(e @ SdkError::Write(orvu_crdt::WriteError::NoFreshDot)) => {
                assert!(e.to_string().contains("no change numbers left for this machine"), "{e}");
                assert!(e.to_string().contains("can only be read"), "{e}");
            }
            other => panic!("expected a refused write, got {other:?}"),
        }
        assert!(matches!(counter.decrement(1).await, Err(SdkError::Write(orvu_crdt::WriteError::NoFreshDot))));
        assert_eq!(counter.value().await.unwrap(), 0);
        let bounded = c.bounded_counter("b").await.unwrap();
        assert!(matches!(bounded.increment(1).await, Err(SdkError::Write(orvu_crdt::WriteError::NoFreshDot))));
        assert!(matches!(bounded.try_decrement(1).await, Err(SdkError::Write(orvu_crdt::WriteError::NoFreshDot))));
    }

    // A machine new to an object that already has changes from the most
    // machines it takes is refused a write here, because every other machine
    // would refuse it.
    #[tokio::test]
    async fn a_write_to_an_object_with_the_most_writers_is_refused_here() {
        let mut seen = orvu_crdt::DotContext::new();
        for r in 1000..1000 + orvu_core::MAX_WRITERS_PER_OBJECT as u64 {
            seen.insert(orvu_crdt::Dot::new(ReplicaId(r), 1));
        }
        let c = client_with_counter_seen(seen).await;
        let counter = c.counter("n").await.unwrap();
        match counter.increment(1).await {
            Err(e @ SdkError::Write(orvu_crdt::WriteError::TooManyWriters)) => {
                assert!(e.to_string().contains("changes from the most machines it takes"), "{e}");
            }
            other => panic!("expected a refused write, got {other:?}"),
        }
        assert_eq!(counter.value().await.unwrap(), 0);
        let bounded = c.bounded_counter("b").await.unwrap();
        assert!(matches!(bounded.increment(1).await, Err(SdkError::Write(orvu_crdt::WriteError::TooManyWriters))));
    }

    #[tokio::test]
    async fn counter_round_trips() {
        let c = client(1, "a").await;
        let counter = c.counter("votes").await.unwrap();
        counter.increment(5).await.unwrap();
        counter.decrement(2).await.unwrap();
        assert_eq!(counter.value().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn set_round_trips() {
        let c = client(1, "a").await;
        let set = c.set("room").await.unwrap();
        set.add("alice").await.unwrap();
        set.add("bob").await.unwrap();
        set.remove("alice").await.unwrap();
        assert!(set.contains("bob").await.unwrap());
        assert!(!set.contains("alice").await.unwrap());
        assert_eq!(set.elements().await.unwrap(), vec!["bob".to_string()]);
    }

    #[tokio::test]
    async fn register_round_trips() {
        let c = client(1, "a").await;
        let title = c.register("doc-title").await.unwrap();
        assert_eq!(title.get().await.unwrap(), None);
        title.set("Draft").await.unwrap();
        assert_eq!(title.get().await.unwrap(), Some("Draft".to_string()));
    }

    #[tokio::test]
    async fn map_round_trips() {
        let c = client(1, "map-node").await;
        let m = c.map("profile").await.unwrap();
        m.put("name", "alice").await.unwrap();
        m.put("city", "berlin").await.unwrap();
        assert_eq!(m.get("name").await.unwrap(), Some("alice".to_string()));
        m.remove("city").await.unwrap();
        assert_eq!(m.get("city").await.unwrap(), None);
        let mut keys = m.keys().await.unwrap();
        keys.sort();
        assert_eq!(keys, vec!["name".to_string()]);
        assert_eq!(
            m.entries().await.unwrap(),
            vec![("name".to_string(), "alice".to_string())]
        );
    }

    #[tokio::test]
    async fn mv_map_round_trips() {
        let c = client(1, "mv-node").await;
        let m = c.mv_map("notes").await.unwrap();
        assert_eq!(m.get("plan").await.unwrap(), Vec::<String>::new());
        m.put("plan", "ship it").await.unwrap();
        m.put("plan", "ship friday").await.unwrap();
        m.put("owner", "alice").await.unwrap();
        m.put("blank", "").await.unwrap();
        // A write replaces what this machine saw, so one value remains.
        assert_eq!(m.get("plan").await.unwrap(), vec!["ship friday".to_string()]);
        // An empty value is a value, and a key that is not there has none.
        assert_eq!(m.get("blank").await.unwrap(), vec![String::new()]);
        m.remove("owner").await.unwrap();
        assert_eq!(m.keys().await.unwrap(), vec!["blank".to_string(), "plan".to_string()]);
        assert_eq!(
            m.entries().await.unwrap(),
            vec![
                ("blank".to_string(), vec![String::new()]),
                ("plan".to_string(), vec!["ship friday".to_string()]),
            ]
        );
        // The id now names a map that keeps clashes, so it is not a plain map,
        // and the other way round. The kind is named as everywhere else.
        assert!(matches!(c.map("notes").await, Err(crate::SdkError::TypeMismatch { .. })));
        c.map("plain").await.unwrap();
        assert!(matches!(
            c.mv_map("plain").await,
            Err(crate::SdkError::TypeMismatch { expected: "mvmap", .. })
        ));
    }

    #[tokio::test]
    async fn mv_map_watch_follows_changes() {
        use futures::StreamExt;
        let c = client(1, "mv-watch").await;
        let m = c.mv_map("notes").await.unwrap();
        let mut stream = Box::pin(m.watch());
        let wait = std::time::Duration::from_secs(2);
        let first = tokio::time::timeout(wait, stream.next()).await.unwrap().unwrap();
        assert!(first.is_empty());
        m.put("plan", "ship it").await.unwrap();
        let next = tokio::time::timeout(wait, stream.next()).await.unwrap().unwrap();
        assert_eq!(next, vec![("plan".to_string(), vec!["ship it".to_string()])]);
    }

    #[tokio::test]
    async fn cart_round_trips() {
        let c = client(1, "cart-node").await;
        let cart = c.cart("groceries").await.unwrap();
        cart.add("apple", 5).await.unwrap();
        cart.add("apple", 2).await.unwrap();
        cart.add("pear", 3).await.unwrap();
        assert_eq!(cart.quantity("apple").await.unwrap(), 7);
        cart.remove_qty("apple", 4).await.unwrap();
        assert_eq!(cart.quantity("apple").await.unwrap(), 3);

        // Taking away more than is there goes below zero. Counters are signed,
        // so this is allowed.
        cart.remove_qty("pear", 10).await.unwrap();
        assert_eq!(cart.quantity("pear").await.unwrap(), -7);

        // remove_item takes the item out of the cart. An item that is not there
        // reads 0.
        cart.remove_item("pear").await.unwrap();
        assert_eq!(cart.quantity("pear").await.unwrap(), 0);

        let mut items = cart.items().await.unwrap();
        items.sort();
        assert_eq!(items, vec![("apple".to_string(), 3)]);
    }

    #[tokio::test]
    async fn list_inserts_deletes_and_reads_in_order() {
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = crate::Client::start(fabric.endpoint(1), cfg).await.unwrap();
        let list = client.list("todo").await.unwrap();

        list.insert(0, "a").await.unwrap(); // [a]
        list.insert(1, "b").await.unwrap(); // [a, b]
        list.insert(1, "x").await.unwrap(); // [a, x, b]
        assert_eq!(list.to_vec().await.unwrap(), vec!["a", "x", "b"]);
        assert_eq!(list.len().await.unwrap(), 3);
        assert_eq!(list.get(1).await.unwrap(), Some("x".to_string()));

        list.delete(1).await.unwrap(); // [a, b]
        assert_eq!(list.to_vec().await.unwrap(), vec!["a", "b"]);

        // An index past the end is refused. It is not moved to the end.
        assert!(matches!(
            list.insert(9, "z").await,
            Err(crate::SdkError::OutOfBounds { .. })
        ));
        assert!(matches!(
            list.delete(9).await,
            Err(crate::SdkError::OutOfBounds { .. })
        ));

        client.shutdown().await;
    }

    #[tokio::test]
    async fn json_sets_nested_reads_and_removes() {
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = crate::Client::start(fabric.endpoint(1), cfg).await.unwrap();
        let doc = client.json("doc").await.unwrap();

        use orvu_crdt::{JsonValue, Seg};
        doc.set(
            &[Seg::Key("user".into()), Seg::Key("name".into())],
            JsonValue::Str("alice".into()),
        )
        .await
        .unwrap();
        doc.set(&[Seg::Key("tags".into())], JsonValue::Array(vec![])).await.unwrap();
        doc.insert(&[Seg::Key("tags".into())], 0, JsonValue::Str("a".into())).await.unwrap();

        assert_eq!(
            doc.get(&[Seg::Key("user".into()), Seg::Key("name".into())]).await.unwrap(),
            Some(JsonValue::Str("alice".into()))
        );
        assert_eq!(
            doc.get(&[Seg::Key("tags".into())]).await.unwrap(),
            Some(JsonValue::Array(vec![JsonValue::Str("a".into())]))
        );

        doc.remove(&[Seg::Key("user".into()), Seg::Key("name".into())]).await.unwrap();
        assert_eq!(
            doc.get(&[Seg::Key("user".into()), Seg::Key("name".into())]).await.unwrap(),
            None
        );

        // A write that goes through an array index fails.
        assert!(doc
            .set(
                &[Seg::Key("tags".into()), Seg::Index(0), Seg::Key("f".into())],
                JsonValue::Num(1.0)
            )
            .await
            .is_err());

        client.shutdown().await;
    }

    #[tokio::test]
    async fn rich_text_inserts_formats_and_reads_spans() {
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = crate::Client::start(fabric.endpoint(1), cfg).await.unwrap();
        let doc = client.rich_text("post").await.unwrap();

        doc.insert(0, "abc").await.unwrap();
        assert_eq!(doc.text().await.unwrap(), "abc");
        doc.format(0, 1, "bold", "true").await.unwrap();
        let spans = doc.to_spans().await.unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "a");
        assert_eq!(spans[0].marks.get("bold").map(String::as_str), Some("true"));
        assert_eq!(spans[1].text, "bc");

        doc.delete(1, 1).await.unwrap(); // remove "b"
        assert_eq!(doc.text().await.unwrap(), "ac");

        // An empty format value is refused.
        assert!(doc.format(0, 1, "bold", "").await.is_err());

        client.shutdown().await;
    }

    #[tokio::test]
    async fn bounded_counter_enforces_non_negative() {
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = crate::Client::start(fabric.endpoint(1), cfg).await.unwrap();
        let stock = client.bounded_counter("sku").await.unwrap();

        stock.increment(5).await.unwrap();
        assert_eq!(stock.value().await.unwrap(), 5);
        assert_eq!(stock.local_allowance().await.unwrap(), 5);

        stock.try_decrement(3).await.unwrap(); // ok
        assert_eq!(stock.value().await.unwrap(), 2);

        // Taking more than is left is refused with Exhausted, and the value
        // stays the same.
        assert!(matches!(stock.try_decrement(5).await, Err(crate::SdkError::Exhausted)));
        assert_eq!(stock.value().await.unwrap(), 2);

        client.shutdown().await;
    }

    #[tokio::test]
    async fn try_decrement_borrows_from_a_peer_when_exhausted() {
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let mut cfg_a = NodeConfig::new(ReplicaId(1), "a");
        cfg_a.peers = vec![orvu_transport::PeerAddr::new("b")];
        cfg_a.anti_entropy_interval = std::time::Duration::from_millis(30);
        let mut cfg_b = NodeConfig::new(ReplicaId(2), "b");
        cfg_b.peers = vec![orvu_transport::PeerAddr::new("a")];
        cfg_b.anti_entropy_interval = std::time::Duration::from_millis(30);
        let a = crate::Client::start(fabric.endpoint(1), cfg_a).await.unwrap();
        let b = crate::Client::start(fabric.endpoint(2), cfg_b).await.unwrap();
        let sa = a.bounded_counter("sku").await.unwrap();
        let sb = b.bounded_counter("sku").await.unwrap();

        sa.increment(10).await.unwrap(); // A holds all 10 of the allowance. B has 0.
        // Wait for the deposit to reach B, so B can see A's allowance.
        for _ in 0..50 {
            if sb.value().await.unwrap() == 10 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        // B has no allowance of its own, but it borrows 3 from A and succeeds.
        sb.try_decrement(3).await.expect("borrowed from A and decremented");
        // The value every machine sees dropped by 3 and is still >= 0.
        for _ in 0..50 {
            if sa.value().await.unwrap() == 7 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(sb.value().await.unwrap(), 7);
        assert!(sb.value().await.unwrap() >= 0);

        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test]
    async fn try_decrement_still_exhausts_with_no_peer() {
        let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = crate::Client::start(fabric.endpoint(1), cfg).await.unwrap();
        let s = client.bounded_counter("sku").await.unwrap();
        s.increment(2).await.unwrap();
        // There is no other machine to borrow from, so taking too much returns
        // Exhausted straight away.
        assert!(matches!(s.try_decrement(5).await, Err(crate::SdkError::Exhausted)));
        assert_eq!(s.value().await.unwrap(), 2);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn opening_cart_on_a_counter_id_is_rejected() {
        let c = client(1, "a").await;
        let _counter = c.counter("x").await.unwrap();
        let err = c.cart("x").await;
        assert!(matches!(err, Err(crate::SdkError::TypeMismatch { .. })));
    }

    #[tokio::test]
    async fn opening_map_on_a_counter_id_is_rejected() {
        let c = client(1, "a").await;
        let _counter = c.counter("x").await.unwrap();
        let err = c.map("x").await;
        assert!(matches!(err, Err(crate::SdkError::TypeMismatch { .. })));
    }

    #[tokio::test]
    async fn opening_the_wrong_type_is_rejected() {
        let c = client(1, "a").await;
        let _counter = c.counter("x").await.unwrap();
        let err = c.set("x").await;
        assert!(matches!(err, Err(crate::SdkError::TypeMismatch { .. })));
    }

    #[tokio::test]
    async fn concurrent_open_of_different_types_still_rejects_one() {
        let c = client(1, "a").await;
        let c2 = c.clone();
        // Open the same id as two different types at the same time.
        let (a, b) = tokio::join!(
            async move { c.counter("x").await.map(|_| ()) },
            async move { c2.set("x").await.map(|_| ()) },
        );
        // Exactly one succeeds. The other gets a TypeMismatch, never an Ok
        // handle that does not work.
        assert!(a.is_ok() ^ b.is_ok(), "exactly one open wins: a={a:?} b={b:?}");
    }

    #[tokio::test]
    async fn watch_yields_on_local_change() {
        use futures::StreamExt;
        let c = client(1, "a").await;
        let counter = c.counter("v").await.unwrap();
        let mut stream = Box::pin(counter.watch());
        // The first item is the value at the time of subscribing.
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, 0);
        counter.increment(4).await.unwrap();
        let v = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v, 4);
    }

    #[tokio::test]
    async fn watch_yields_on_remote_change() {
        use futures::StreamExt;
        use orvu_transport::PeerAddr;
        let fabric = Arc::new(MemTransport::new());
        let mut ca = NodeConfig::new(ReplicaId(1), "a");
        ca.peers = vec![PeerAddr::new("b")];
        let mut cb = NodeConfig::new(ReplicaId(2), "b");
        cb.peers = vec![PeerAddr::new("a")];
        let a = Client::start(fabric.endpoint(1), ca).await.unwrap();
        let b = Client::start(fabric.endpoint(2), cb).await.unwrap();

        let ca_counter = a.counter("v").await.unwrap();
        let cb_counter = b.counter("v").await.unwrap();
        let mut stream = Box::pin(cb_counter.watch());

        let first = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, 0);
        ca_counter.increment(7).await.unwrap();
        let v = tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v, 7);
    }

    #[tokio::test]
    async fn watch_emits_initial_value() {
        use futures::StreamExt;
        let c = client(1, "a").await;
        let counter = c.counter("v").await.unwrap();
        counter.increment(5).await.unwrap(); // set the value before subscribing
        let mut stream = Box::pin(counter.watch());
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, 5); // the current value arrives first, with no further change needed
    }

    #[tokio::test]
    async fn watch_dedups_unchanged_value() {
        use futures::StreamExt;
        let c = client(1, "a").await;
        let counter = c.counter("v").await.unwrap();
        let mut stream = Box::pin(counter.watch());
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, 0);
        counter.increment(0).await.unwrap(); // fires a change event but the value stays 0, so it is skipped
        counter.increment(3).await.unwrap(); // now 3
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next, 3); // the change that left the value at 0 was not yielded
    }

    #[tokio::test]
    async fn watch_initial_and_change_on_list() {
        use futures::StreamExt;
        let c = client(1, "a").await;
        let list = c.list("l").await.unwrap();
        list.insert(0, "a").await.unwrap();
        let mut stream = Box::pin(list.watch());
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, vec!["a".to_string()]); // the current value arrives first
        list.insert(1, "b").await.unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn tree_moves_converge_across_nodes() {
        use orvu_transport::PeerAddr;
        let fabric = Arc::new(MemTransport::new());
        let mut ca = NodeConfig::new(ReplicaId(1), "a");
        ca.peers = vec![PeerAddr::new("b")];
        let mut cb = NodeConfig::new(ReplicaId(2), "b");
        cb.peers = vec![PeerAddr::new("a")];
        let a = Client::start(fabric.endpoint(1), ca).await.unwrap();
        let b = Client::start(fabric.endpoint(2), cb).await.unwrap();
        let ta = a.tree("t").await.unwrap();
        let tb = b.tree("t").await.unwrap();
        ta.move_node("a", "root").await.unwrap();
        ta.move_node("b", "root").await.unwrap();
        // Check up to 50 times until b sees both nodes under root.
        let mut ok = false;
        for _ in 0..50 {
            if tb.parent("a").await.unwrap().as_deref() == Some("root")
                && tb.parent("b").await.unwrap().as_deref() == Some("root")
            {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(ok, "tree state converged to node b");
    }

    #[tokio::test]
    async fn tree_labels_converge_across_nodes() {
        use orvu_transport::PeerAddr;
        let fabric = Arc::new(MemTransport::new());
        let mut ca = NodeConfig::new(ReplicaId(1), "a");
        ca.peers = vec![PeerAddr::new("b")];
        let mut cb = NodeConfig::new(ReplicaId(2), "b");
        cb.peers = vec![PeerAddr::new("a")];
        let a = Client::start(fabric.endpoint(1), ca).await.unwrap();
        let b = Client::start(fabric.endpoint(2), cb).await.unwrap();
        let ta = a.tree("labelled").await.unwrap();
        let tb = b.tree("labelled").await.unwrap();

        assert_eq!(ta.label("a").await.unwrap(), None, "unset label reads None");
        ta.move_node("a", "root").await.unwrap();
        ta.set_label("a", "Alpha").await.unwrap();

        // Check up to 50 times until b sees both the parent and the label.
        let mut ok = false;
        for _ in 0..50 {
            if tb.parent("a").await.unwrap().as_deref() == Some("root")
                && tb.label("a").await.unwrap().as_deref() == Some("Alpha")
            {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(ok, "the label reached node b");

        // A rename made on the other machine comes back to a.
        tb.set_label("a", "Renamed").await.unwrap();
        let mut back = false;
        for _ in 0..50 {
            if ta.label("a").await.unwrap().as_deref() == Some("Renamed") {
                back = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(back, "the rename reached node a");
    }

    #[tokio::test]
    async fn tree_sibling_order_converges_across_nodes() {
        use orvu_transport::PeerAddr;
        let fabric = Arc::new(MemTransport::new());
        let mut ca = NodeConfig::new(ReplicaId(1), "a");
        ca.peers = vec![PeerAddr::new("b")];
        let mut cb = NodeConfig::new(ReplicaId(2), "b");
        cb.peers = vec![PeerAddr::new("a")];
        let a = Client::start(fabric.endpoint(1), ca).await.unwrap();
        let b = Client::start(fabric.endpoint(2), cb).await.unwrap();
        let ta = a.tree("ordered").await.unwrap();
        let tb = b.tree("ordered").await.unwrap();

        ta.move_node_at("x", "root", 0).await.unwrap();
        ta.move_node_at("y", "root", 1).await.unwrap();
        // This is the order we asked for, not id order: "y" comes before "x".
        ta.move_node_at("y", "root", 0).await.unwrap();
        assert_eq!(ta.children("root").await.unwrap(), vec!["y".to_string(), "x".to_string()]);

        let mut ok = false;
        for _ in 0..50 {
            if tb.children("root").await.unwrap() == vec!["y".to_string(), "x".to_string()] {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(ok, "the sibling order reached node b");

        // A reorder made on the other machine comes back to a.
        tb.move_node_at("x", "root", 0).await.unwrap();
        let mut back = false;
        for _ in 0..50 {
            if ta.children("root").await.unwrap() == vec!["x".to_string(), "y".to_string()] {
                back = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(back, "the reorder reached node a");
    }

    #[tokio::test]
    async fn tree_watch_emits_on_a_rename() {
        use futures::StreamExt;
        let c = client(1, "watch-tree").await;
        let t = c.tree("renamed").await.unwrap();
        t.move_node("a", "root").await.unwrap();
        t.set_label("a", "Alpha").await.unwrap();

        let mut stream = Box::pin(t.watch());
        // The first item is the current value.
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("initial value")
            .expect("stream is live");
        let alpha = first.iter().find(|n| n.id == "a").expect("node a is projected");
        assert_eq!(alpha.parent.as_deref(), Some("root"));
        assert_eq!(alpha.label.as_deref(), Some("Alpha"));

        // Only the label changes here. Nothing moves. A stream of only (child,
        // parent) pairs would yield the same item as before, and `watch_object`
        // would skip it as unchanged, so this would time out.
        t.set_label("a", "Renamed").await.unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("a rename must reach the watcher")
            .expect("stream is live");
        let renamed = next.iter().find(|n| n.id == "a").expect("node a is projected");
        assert_eq!(renamed.label.as_deref(), Some("Renamed"));
        assert_eq!(renamed.parent.as_deref(), Some("root"), "structure unchanged");
    }

    #[tokio::test]
    async fn watch_rereads_on_lagged() {
        use futures::StreamExt;
        // Give the change channel a small capacity, so a burst of changes
        // overflows the watcher's receiver.
        let fabric = std::sync::Arc::new(MemTransport::new());
        let mut cfg = NodeConfig::new(ReplicaId(1), "solo");
        cfg.ship_queue_cap = 4;
        let client = Client::start(fabric.endpoint(1), cfg).await.unwrap();
        let counter = client.counter("v").await.unwrap();
        let mut stream = Box::pin(counter.watch());
        // Read the first value, then stop reading so the receiver falls behind.
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, 0);
        for _ in 0..10 {
            counter.increment(1).await.unwrap(); // more changes than the capacity, so the receiver lags. Nothing reads in between.
        }
        // The next read gets a Lagged error and must read the object again to
        // get the latest value (10), even though no change follows the
        // overflow.
        let v = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(v, 10);
    }
}
