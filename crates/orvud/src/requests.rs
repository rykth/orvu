//! Calls that carry a request id. A client gives each call that changes
//! something an id, and sends the same id when it sends the call again
//! because no answer came. Such a call is made once.
//!
//! What says a change was made is the receipt the engine saves with it, in
//! the same step as the change (see `orvu_sdk::with_receipt`). A call sent
//! again whose receipt is there is answered as made and not made again,
//! whether the sidecar restarted in between or not. Receipts are kept for
//! ten minutes, so a call sent again later than that is made again.
//!
//! Over that, the answers of recent calls are kept in memory, so that a call
//! sent again gets the very answer the first one got, a refusal included,
//! and a call sent again while the first is still being made waits for that
//! one. Only the answers to changes are kept, never what a read read: a
//! batch sent again reads afresh. The memory is bounded in time, in ids and
//! in bytes for each agent, and is only a quicker way to the same answer:
//! what is let go of is still covered by the receipts.
//!
//! An id is kept apart for each agent: the receipt's id is made from the
//! agent's name and the id it sent, so one agent cannot be given, or find
//! out about, another's answers.

use crate::pb;
use prost::Message;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// How long an answer is kept in memory: as long as a receipt is kept.
pub const KEEP: Duration = Duration::from_millis(orvu_sdk::RECEIPT_KEEP_MS);

/// The most ids kept in memory for one agent. Past it the oldest finished
/// one is let go.
pub const MOST_PER_AGENT: usize = 10_000;

/// The most bytes of answers kept in memory for one agent. Past it the
/// oldest finished ones are let go.
pub const MOST_BYTES_PER_AGENT: usize = 4 * 1024 * 1024;

/// The longest id a call may carry, in bytes.
pub const MOST_ID_BYTES: usize = 64;

/// Which kind of call an id was sent with. The same id sent with a batch and
/// with a single call names two calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Call {
    One,
    Batch,
}

/// The answer a call was given: for a batch, the answer of each part that
/// changes something, and `None` for each part that reads.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    One(pb::Response),
    Batch(Vec<Option<pb::Response>>),
}

impl Answer {
    fn bytes(&self) -> usize {
        match self {
            Answer::One(response) => response.encoded_len(),
            Answer::Batch(parts) => parts.iter().flatten().map(|response| response.encoded_len()).sum(),
        }
    }
}

type Key = (String, Call, Vec<u8>);
type Slot = watch::Receiver<Option<Answer>>;

/// Says whether a change was made under a receipt id.
pub type Made = Arc<dyn Fn(&[u8; 16]) -> bool + Send + Sync>;

/// The answers kept in memory, and how to ask for receipts.
pub struct Requests {
    remembered: Mutex<Remembered>,
    made: Made,
    most: usize,
    keep: Duration,
    most_bytes: usize,
}

struct Kept {
    at: Instant,
    slot: Slot,
    /// The size of the answer, once there is one.
    bytes: usize,
}

#[derive(Default)]
struct Remembered {
    answers: HashMap<Key, Kept>,
    /// The ids of each agent, oldest first, with when each was first sent.
    /// An entry whose time no longer matches `answers` is left over from an
    /// id that was let go, and is skipped.
    order: HashMap<String, VecDeque<(Instant, Call, Vec<u8>)>>,
    /// The bytes of answers kept for each agent.
    bytes: HashMap<String, usize>,
}

/// What `begin` found.
pub enum Begun {
    /// The id is known. Its answer arrives here, once the first call with it
    /// is done. If that call ends with no answer, the slot closes empty.
    Known(Slot),
    /// The id is not in memory. Whoever holds this makes the call, after
    /// asking the receipts which of its parts were made, and hands its answer
    /// to `Promise::keep`.
    New(Promise),
}

impl Requests {
    /// Keeps answers for `keep`, at most `most` of them and `most_bytes`
    /// bytes of them for each agent, and asks `made` about receipts.
    pub fn new(made: Made, most: usize, keep: Duration, most_bytes: usize) -> Self {
        Self { remembered: Mutex::default(), made, most, keep, most_bytes }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Remembered> {
        self.remembered.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Looks the id up for `agent`, and takes it as new if it is not known.
    pub fn begin(self: &Arc<Self>, agent: &str, call: Call, id: &[u8]) -> Begun {
        let now = Instant::now();
        let key: Key = (agent.to_string(), call, id.to_vec());
        let mut remembered = self.lock();
        remembered.let_go(agent, now, self);
        if let Some(kept) = remembered.answers.get(&key) {
            return Begun::Known(kept.slot.clone());
        }
        let (tx, rx) = watch::channel(None);
        remembered.answers.insert(key.clone(), Kept { at: now, slot: rx, bytes: 0 });
        remembered.order.entry(agent.to_string()).or_default().push_back((now, call, id.to_vec()));
        remembered.let_go(agent, now, self);
        Begun::New(Promise { requests: self.clone(), key, at: now, tx: Some(tx) })
    }

    /// Whether the receipts say the change with this receipt id was made.
    pub fn made(&self, receipt: &[u8; 16]) -> bool {
        (self.made)(receipt)
    }

    /// The bytes of answers kept in memory, over all agents.
    pub fn bytes(&self) -> usize {
        self.lock().bytes.values().sum()
    }

    /// Takes in the answer of the id `key`, first sent at `at`.
    fn kept(&self, key: &Key, at: Instant, answer: &Answer) {
        let mut remembered = self.lock();
        let bytes = answer.bytes() + key.2.len();
        match remembered.answers.get_mut(key) {
            Some(kept) if kept.at == at => kept.bytes = bytes,
            _ => return,
        }
        *remembered.bytes.entry(key.0.clone()).or_default() += bytes;
        remembered.let_go(&key.0, Instant::now(), self);
    }

    /// Lets go of an id whose call ended with no answer, so that it can be
    /// sent again.
    fn forget(&self, key: &Key, at: Instant) {
        let mut remembered = self.lock();
        if remembered.answers.get(key).is_some_and(|kept| kept.at == at) {
            remembered.answers.remove(key);
        }
    }
}

impl Remembered {
    /// Lets go of the agent's finished ids that are older than the time they
    /// are kept for, and of the oldest finished ones while the agent has more
    /// than the most ids or bytes. An id whose call is still being made is
    /// never let go of, so a call sent again while it runs always waits for
    /// it.
    fn let_go(&mut self, agent: &str, now: Instant, limits: &Requests) {
        let Some(mut order) = self.order.remove(agent) else { return };
        let mut running = Vec::new();
        let mut count = order.len();
        while let Some((when, call, id)) = order.pop_front() {
            let key = (agent.to_string(), call, id);
            let Some(kept) = self.answers.get(&key).filter(|kept| kept.at == when) else {
                count -= 1;
                continue;
            };
            let bytes = self.bytes.get(agent).copied().unwrap_or(0);
            let over = now.duration_since(when) >= limits.keep || count > limits.most || bytes > limits.most_bytes;
            if !over {
                order.push_front((when, key.1, key.2));
                break;
            }
            if kept.slot.borrow().is_none() {
                running.push((when, key.1, key.2));
                continue;
            }
            let freed = kept.bytes;
            self.answers.remove(&key);
            if let Some(total) = self.bytes.get_mut(agent) {
                *total = total.saturating_sub(freed);
            }
            count -= 1;
        }
        for entry in running.into_iter().rev() {
            order.push_front(entry);
        }
        if order.is_empty() {
            self.bytes.remove(agent);
        } else {
            self.order.insert(agent.to_string(), order);
        }
    }
}

/// The right to make a call with an id not in memory. Its answer is handed
/// to `keep`. Dropped without an answer, as when the call was turned away or
/// broke, the id is let go, and a call sent again with it is made, as far as
/// its receipts say it was not.
pub struct Promise {
    requests: Arc<Requests>,
    key: Key,
    at: Instant,
    tx: Option<watch::Sender<Option<Answer>>>,
}

impl Promise {
    /// Keeps `answer` as the one for this id, and hands it to every call
    /// with the same id that is waiting for it.
    pub fn keep(mut self, answer: Answer) {
        if let Some(tx) = self.tx.take() {
            tx.send_replace(Some(answer.clone()));
            self.requests.kept(&self.key, self.at, &answer);
        }
    }

    /// The receipt id of the change the part `index` of this call makes: the
    /// only part of a single call is 0. See `receipt_id`.
    pub fn receipt(&self, index: u32) -> [u8; 16] {
        let (agent, call, id) = &self.key;
        receipt_id(agent, *call, id, index)
    }

    /// Whether the receipts say the part `index` of this call was made.
    pub fn saved(&self, index: u32) -> bool {
        self.requests.made(&self.receipt(index))
    }
}

impl Drop for Promise {
    fn drop(&mut self) {
        if self.tx.is_some() {
            self.requests.forget(&self.key, self.at);
        }
    }
}

/// The id a change is saved under: the first 16 bytes of a SHA-256 of the
/// agent's name, the kind of call, the id it sent and the part of the call.
/// Each agent's are its own, and nobody can tell from one what was sent.
pub fn receipt_id(agent: &str, call: Call, id: &[u8], index: u32) -> [u8; 16] {
    let mut hashed = ring::digest::Context::new(&ring::digest::SHA256);
    hashed.update(b"orvu request\0");
    hashed.update(&(agent.len() as u64).to_be_bytes());
    hashed.update(agent.as_bytes());
    hashed.update(match call {
        Call::One => b"one\0",
        Call::Batch => b"all\0",
    });
    hashed.update(&(id.len() as u64).to_be_bytes());
    hashed.update(id);
    hashed.update(&index.to_be_bytes());
    let mut out = [0; 16];
    out.copy_from_slice(&hashed.finish().as_ref()[..16]);
    out
}

/// The receipt id of the second change of a part that makes two, such as a
/// top-up and its line in the record of who topped the budget up, made from
/// the part's own receipt id.
pub fn second_receipt(receipt: [u8; 16]) -> [u8; 16] {
    let mut hashed = ring::digest::Context::new(&ring::digest::SHA256);
    hashed.update(b"orvu second change\0");
    hashed.update(&receipt);
    let mut out = [0; 16];
    out.copy_from_slice(&hashed.finish().as_ref()[..16]);
    out
}

/// Waits for the answer of a call with a known id. `None` if that call
/// ended with no answer.
pub async fn answer(mut slot: Slot) -> Option<Answer> {
    match slot.wait_for(|answer| answer.is_some()).await {
        Ok(answer) => answer.clone(),
        Err(_) => None,
    }
}

/// For tests of what a client does when the sidecar stops between saving a
/// change and answering it. In a debug build only: if
/// `ORVUD_TEST_STOP_AFTER_SAVING` names a file that is there and says
/// `point`, the file is removed and the sidecar stops at once, without
/// answering. An empty file stands for the point "answer", reached after
/// each call with an id, once its change is saved, which it is by then with
/// `[storage] fsync = "always"`, the default for `--local`. The point
/// "top-up" is reached after a top-up is saved and before its line in the
/// record of who topped up is. Removing the file first means it happens
/// once. A release build never stops here.
pub fn stop_here_if_asked(point: &str) {
    #[cfg(debug_assertions)]
    {
        static ASKED: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();
        let asked = ASKED.get_or_init(|| std::env::var_os("ORVUD_TEST_STOP_AFTER_SAVING"));
        if let Some(file) = asked {
            let said = std::fs::read_to_string(file).unwrap_or_default();
            let said = match said.trim() {
                "" => "answer",
                other => other,
            };
            if said == point && std::fs::remove_file(file).is_ok() {
                std::process::exit(9);
            }
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = point;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requests(most: usize) -> Arc<Requests> {
        Arc::new(Requests::new(Arc::new(|_: &[u8; 16]| false), most, KEEP, usize::MAX))
    }

    // A call that is still being made is never let go of to make room, so a
    // call sent again while it runs waits for it and is not made beside it.
    #[test]
    fn a_call_still_being_made_is_not_let_go_of() {
        let requests = requests(1);
        let Begun::New(running) = requests.begin("a", Call::One, b"running") else { panic!("new") };
        for id in [b"one", b"two", b"six"] {
            match requests.begin("a", Call::One, id) {
                Begun::New(promise) => promise.keep(Answer::One(pb::Response::default())),
                Begun::Known(_) => panic!("{id:?} is new"),
            }
        }
        assert!(matches!(requests.begin("a", Call::One, b"running"), Begun::Known(_)));
        // Once it has its answer, it can go.
        running.keep(Answer::One(pb::Response::default()));
        let Begun::New(_seven) = requests.begin("a", Call::One, b"seven") else { panic!("new") };
        assert!(matches!(requests.begin("a", Call::One, b"running"), Begun::New(_)));
    }
}
