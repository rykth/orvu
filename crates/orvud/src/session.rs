//! Keeps track of who is talking to us and what we remember about them.
//!
//! A session is one client program's connection state. It is kept in memory
//! only. It holds nothing that matters after a restart, and a client that
//! comes back after one opens a new session.

use crate::agents::Agent;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// What the sidecar remembers about one session.
pub struct SessionState {
    pub namespace: String,
    /// The agent whose key opened it, or `None` if no key was needed.
    pub agent: Option<Agent>,
    /// How many watches it has open.
    pub watches: std::sync::atomic::AtomicUsize,
}

impl SessionState {
    pub fn new(namespace: &str, agent: Option<Agent>) -> Self {
        Self { namespace: namespace.to_string(), agent, watches: Default::default() }
    }
}

/// The most sessions that can exist at once. It is fixed and cannot be
/// configured.
pub const MAX_SESSIONS: usize = 1024;

/// The most sessions one agent with a key may hold at once: a quarter of all
/// of them. Programs without a key, which only come in on the local socket
/// before any agent is named, are trusted and are not held to a share.
pub const MAX_SESSIONS_PER_AGENT: usize = MAX_SESSIONS / 4;

struct Entry {
    state: Arc<SessionState>,
    token: String,
    /// When the session was last used: a call made on it, or a value sent on
    /// one of its watches. This is a counter that only goes up, not a time,
    /// so two uses never look the same.
    last_used: u64,
}

#[derive(Default)]
struct Live {
    tick: u64,
    by_id: BTreeMap<String, Entry>,
    /// Maps a resume token to its session id.
    by_token: BTreeMap<String, String>,
}

impl Live {
    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }
}

/// All the open sessions, up to a fixed limit.
pub struct Sessions {
    cap: usize,
    per_agent: usize,
    live: Mutex<Live>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

/// A random 128-bit value written as hex. Both secrets are made this way: the
/// session id (sent with every op) and the resume token (sent with Open). Each
/// is generated separately, so someone who learns one cannot use it in place
/// of the other.
fn mint_token() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS entropy source");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Sessions {
    pub fn new() -> Self {
        Self { cap: MAX_SESSIONS, per_agent: MAX_SESSIONS_PER_AGENT, live: Mutex::new(Live::default()) }
    }

    /// For tests only: a smaller limit, so that reaching it is cheap to test.
    /// Each agent still gets a quarter of it.
    #[doc(hidden)]
    pub fn with_cap(cap: usize) -> Self {
        Self { cap, per_agent: (cap / 4).max(1), live: Mutex::new(Live::default()) }
    }

    /// Opens a session for `agent` and returns `(session, resume_token)`. A
    /// `resume` token we do not know, or one that was opened with a different
    /// namespace or by another agent, gives a new session and not an error. After a restart there
    /// is nothing to resume, and a client cannot tell that case from
    /// connecting for the first time. An error would make every client carry
    /// extra code to recover from a restart.
    pub fn open(&self, namespace: &str, resume: &str, agent: Option<Agent>) -> (String, String) {
        let mut live = self.live.lock().expect("sessions lock");
        if !resume.is_empty() {
            if let Some(id) = live.by_token.get(resume).cloned() {
                let tick = live.touch();
                if let Some(e) = live.by_id.get_mut(&id) {
                    if e.state.namespace == namespace && e.state.agent == agent {
                        e.last_used = tick;
                        return (id, resume.to_string());
                    }
                }
            }
        }
        // An agent with a key that already holds its share gives up its own
        // session used longest ago, and never anyone else's. Without this,
        // one agent that keeps many sessions busy, for example by watching
        // an object it writes all the time, would keep them all newer than
        // every other program's, and each new session would push out
        // someone else.
        //
        // The share goes with the key as well as the name. An agent whose
        // key was replaced is someone else under the same name, and the
        // sessions its old key opened are not the new key's to give up.
        let own = |e: &Entry| agent.is_some() && e.state.agent == agent;
        let victim = if agent.is_some() && live.by_id.values().filter(|e| own(e)).count() >= self.per_agent {
            live.by_id.iter().filter(|(_, e)| own(e)).min_by_key(|(_, e)| e.last_used).map(|(id, _)| id.clone())
        } else if live.by_id.len() >= self.cap {
            // Otherwise, at the limit, drop the session that was used
            // longest ago, whoever it belongs to, and nothing else decides
            // which. A client that loses its session
            // calls `Open` again, which is a normal thing to do and not an
            // error. Its watches end with it.
            //
            // A watch that sends a value uses its session, so a watcher
            // whose object keeps changing stays. A quiet watch does not, so
            // sessions that only wait cannot crowd out programs that make
            // calls. And a session opened again is the newest of all, so a
            // dropped watcher coming back pushes out just one more session,
            // the next one used longest ago.
            live.by_id.iter().min_by_key(|(_, e)| e.last_used).map(|(id, _)| id.clone())
        } else {
            None
        };
        if let Some(id) = victim {
            if let Some(e) = live.by_id.remove(&id) {
                live.by_token.remove(&e.token);
            }
        }
        let id = mint_token();
        let token = mint_token();
        let tick = live.touch();
        live.by_id.insert(
            id.clone(),
            Entry {
                state: Arc::new(SessionState::new(namespace, agent)),
                token: token.clone(),
                last_used: tick,
            },
        );
        live.by_token.insert(token.clone(), id.clone());
        (id, token)
    }

    /// Whether the session is still open. Unlike `get`, this does not mark it
    /// as just used: a watch with nothing to send asks this, and waiting is
    /// not using.
    pub fn contains(&self, session: &str) -> bool {
        self.live.lock().expect("sessions lock").by_id.contains_key(session)
    }

    /// Looks up a session by its id without marking it as used. A call asks
    /// this first, and marks the session used only once its agent is found
    /// to be still let in.
    pub fn peek(&self, session: &str) -> Option<Arc<SessionState>> {
        self.live.lock().expect("sessions lock").by_id.get(session).map(|e| e.state.clone())
    }

    /// Ends a session, for example because its agent was removed. Its resume
    /// token goes with it, and its watches end at their next look.
    pub fn end(&self, session: &str) {
        let mut live = self.live.lock().expect("sessions lock");
        if let Some(e) = live.by_id.remove(session) {
            live.by_token.remove(&e.token);
        }
    }

    /// Looks up a session by its id and marks it as just used.
    pub fn get(&self, session: &str) -> Option<Arc<SessionState>> {
        let mut live = self.live.lock().expect("sessions lock");
        let tick = live.touch();
        let e = live.by_id.get_mut(session)?;
        e.last_used = tick;
        Some(e.state.clone())
    }
}
