//! Search by meaning, over the notes, documents and tasks of each team.
//!
//! A team's index is made the first time the team is searched, if the team
//! has anything: every simple note, structured note and document its index
//! object lists, and its task board, is read, cut into texts, and put in. A
//! task is searched by its title and notes. Messages are not searched: an
//! inbox is read only by its owner. From then on a task of
//! its own follows the changes the engine reports and puts each changed
//! object in again within a moment. Nothing follows the changes until a team
//! is first searched. The engine drops reports when it is very busy, so every
//! minute, and as soon as reports were dropped, each searched team's objects
//! are all read again. Texts that did not change keep their vectors, so that
//! is quick. Every change to the indexes is made by that one task, in the
//! order the changes came, so a slow read never puts an old text back over a
//! new one. Vectors are worked out with nothing locked, so a search does not
//! wait for them, and an object whose texts did not change is left alone.
//!
//! The model is loaded the first time anything is searched: from the folder
//! the sidecar was given, or from one in its data folder, into which it is
//! fetched first if it is not there yet.

use orvu_crdt::JsonValue;
use crate::notes::Moves;
use orvu_sdk::Client;
use orvu_search::{Found, Index, Model};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Notify, OnceCell};
use tokio_util::sync::CancellationToken;

/// The most results one search may ask for.
pub const MOST: u32 = 50;

/// How many results a search that does not say gets.
pub const USUAL: u32 = 5;

/// How long reports of changes are gathered before the objects are read.
const GATHER: Duration = Duration::from_millis(50);

/// How often each searched team's objects are all read again.
const AGAIN: Duration = Duration::from_secs(60);

/// How many reports of changes may wait to be looked at.
const WAITING: usize = 4096;

/// The least time between two readings of everything again because reports
/// were dropped. A sidecar too busy to keep up would otherwise do nothing
/// else.
const RESTING: Duration = Duration::from_secs(5);

/// The most words, and the most characters, of one text the model is given.
/// It reads only the start of a longer one, so a longer text is searched in
/// pieces of this size, each found under the same name and part.
const PIECE_WORDS: usize = 100;
const PIECE_CHARS: usize = 1000;

/// The mark a simple note's value starts with when it is not plain text but
/// JSON, as the Python and TypeScript libraries keep it.
const MARK: char = '\u{1e}';

/// Where the model comes from.
#[derive(Clone, Debug)]
pub enum ModelFrom {
    /// Nowhere: this sidecar does not search.
    Nowhere,
    /// The files in this folder, as they are.
    Folder(PathBuf),
    /// This folder, into which the model is fetched from `source` if it is
    /// not there. Each file is checked against its pin whatever the source.
    Fetched { folder: PathBuf, source: String },
}

/// The indexes of the teams searched, each with a lock of its own: searches
/// of a team read it together, and its follower changes it alone.
type Teams = Arc<Mutex<HashMap<String, Arc<RwLock<Index>>>>>;

/// How long a model that could not be loaded is not tried again. Fetching
/// takes minutes when it fails slowly, and every search would wait for it.
const RETRY: Duration = Duration::from_secs(60);

/// How the indexes keep up. Only tests change it: the sidecar always uses
/// the usual one.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    /// How often each searched team's objects are all read again.
    pub again: Duration,
    /// The least time between two readings of everything again because
    /// reports were dropped.
    pub resting: Duration,
    /// Whether the engine's reports of changes are followed at all. Without
    /// them, changes are found only by reading everything again, as when
    /// every report is dropped.
    pub following: bool,
}

impl Default for Pace {
    fn default() -> Self {
        Self { again: AGAIN, resting: RESTING, following: true }
    }
}

/// What the searcher and the task that changes the indexes share: that
/// reports were dropped, and counts the tests look at.
#[derive(Default)]
struct Shared {
    /// Set when the engine dropped reports of changes.
    dropped: AtomicBool,
    /// Wakes the task when reports were dropped.
    wake: Notify,
    /// How many times an object was read to be searched.
    reads: AtomicU64,
    /// How many times an object's texts were changed in an index after it
    /// was made.
    puts: AtomicU64,
    /// Objects whose reads fail, for the tests.
    failing: Mutex<BTreeSet<String>>,
}

/// The indexes, and what keeps them up to date.
pub struct Searcher {
    from: ModelFrom,
    model: OnceCell<Arc<Model>>,
    /// When the model last could not be loaded, and why.
    failed: Mutex<Option<(Instant, String)>>,
    /// Cancelled when the sidecar stops. A search that waits for the model
    /// then ends, so that the server does not wait for it.
    stopping: CancellationToken,
    teams: Teams,
    /// To the task that changes the indexes. `None` when nothing is searched.
    asks: Option<mpsc::Sender<Make>>,
    client: Option<Arc<Client>>,
    /// Moves teams' old notes over. Searches read notes, as requests do, so
    /// both go through this one, which keeps two moves of a team apart.
    moves: Arc<Moves>,
    shared: Arc<Shared>,
}

/// Asks the task to make a team's index, and says when it is made.
struct Make {
    team: String,
    model: Arc<Model>,
    made: oneshot::Sender<Result<(), String>>,
}

impl Searcher {
    /// A searcher that does not search: every search is answered with why.
    pub fn off() -> Self {
        Self {
            from: ModelFrom::Nowhere,
            model: OnceCell::new(),
            failed: Mutex::new(None),
            stopping: CancellationToken::new(),
            teams: Arc::default(),
            asks: None,
            client: None,
            moves: Arc::default(),
            shared: Arc::default(),
        }
    }

    /// A searcher whose model comes `from` there, and the task that keeps its
    /// indexes up to date with `client` until `stopping`.
    pub fn start(client: Arc<Client>, from: ModelFrom, stopping: CancellationToken) -> Arc<Self> {
        Self::start_paced(client, from, stopping, Pace::default())
    }

    /// As `start`, keeping up at `pace`. For the tests.
    #[doc(hidden)]
    pub fn start_paced(client: Arc<Client>, from: ModelFrom, stopping: CancellationToken, pace: Pace) -> Arc<Self> {
        if matches!(from, ModelFrom::Nowhere) {
            return Arc::new(Self::off());
        }
        let (asks, asked) = mpsc::channel(64);
        let teams: Teams = Arc::default();
        let moves: Arc<Moves> = Arc::default();
        let shared: Arc<Shared> = Arc::default();
        let follower = Follower {
            client: client.clone(),
            teams: teams.clone(),
            lists: HashMap::new(),
            nothing: HashMap::new(),
            model: None,
            stopping: stopping.clone(),
            pace,
            shared: shared.clone(),
        };
        tokio::spawn(follower.run(asked));
        Arc::new(Self {
            from,
            model: OnceCell::new(),
            failed: Mutex::new(None),
            stopping,
            teams,
            asks: Some(asks),
            client: Some(client),
            moves,
            shared,
        })
    }

    /// What moves teams' old notes over.
    pub fn moves(&self) -> &Arc<Moves> {
        &self.moves
    }

    /// The answer to a search of the team whose index is `object`: JSON text,
    /// a list of what was found, or why nothing could be searched. `may` says
    /// whether the one asking may read an object, by the rules reads go by.
    /// A search still waiting when the sidecar stops ends then.
    pub async fn search(
        &self,
        object: &str,
        query: &str,
        limit: u32,
        may: impl Fn(&str) -> bool + Send + 'static,
    ) -> Result<String, String> {
        tokio::select! {
            answered = self.answer(object, query, limit, may) => answered,
            _ = self.stopping.cancelled() => Err("the sidecar is stopping".into()),
        }
    }

    async fn answer(
        &self,
        object: &str,
        query: &str,
        limit: u32,
        may: impl Fn(&str) -> bool + Send + 'static,
    ) -> Result<String, String> {
        let team = team_of_index(object).ok_or_else(|| format!("{object} is not a team's index"))?;
        let (Some(asks), Some(client)) = (&self.asks, &self.client) else {
            return Err("this sidecar does not search".into());
        };
        // Any agent may search under any team name. One that lists nothing
        // that is searched is answered at once, with no model, and nothing
        // is kept for it.
        let kept = lock(&self.teams).contains_key(team);
        // The team's old notes are moved over first, so that they are found.
        if !kept {
            self.moves.team(client, team).await?;
        }
        if !kept && listed(client, team).await?.is_empty() {
            return Ok("[]".into());
        }
        let model = self.model().await?;
        if !kept {
            let (made, done) = oneshot::channel();
            let make = Make { team: team.to_string(), model: model.clone(), made };
            asks.send(make).await.map_err(|_| "the sidecar is stopping")?;
            done.await.map_err(|_| "the sidecar is stopping")??;
        }
        let limit = if limit == 0 { USUAL } else { limit.min(MOST) } as usize;
        let Some(index) = lock(&self.teams).get(team).cloned() else { return Ok("[]".into()) };
        // The question is worked out, and the team searched, where waiting
        // holds nothing else up. The rules are asked about each object found,
        // once, by its name, as every read asks them.
        let question = query.to_string();
        let searched = tokio::task::spawn_blocking(move || {
            let asked = model.vectors(&[question]).pop()?;
            let index = index.read().unwrap_or_else(|e| e.into_inner());
            Some(index.search_vector(&asked, limit, may))
        });
        let Ok(Some(found)) = searched.await else { return Err("the question could not be worked out".into()) };
        Ok(Value::Array(found.iter().filter_map(hit).collect()).to_string())
    }

    /// The model, loaded once. One that could not be loaded is not tried
    /// again for `RETRY`, and the reason is given at once in the meantime.
    async fn model(&self) -> Result<Arc<Model>, String> {
        if let Some(model) = self.model.get() {
            return Ok(model.clone());
        }
        // Searches that wait together take turns to load it. Each looks
        // again at whether it just failed, so that only the first fetches.
        let loaded = self
            .model
            .get_or_try_init(|| async {
                if let Some(why) = self.recently_failed() {
                    return Err(why);
                }
                let loaded = load(self.from.clone()).await;
                if let Err(why) = &loaded {
                    *lock(&self.failed) = Some((Instant::now(), why.clone()));
                }
                loaded
            })
            .await;
        loaded.cloned()
    }

    /// Why the model could not be loaded, if that was less than `RETRY` ago.
    fn recently_failed(&self) -> Option<String> {
        let failed = lock(&self.failed).clone();
        let (when, why) = failed?;
        (when.elapsed() < RETRY).then(|| format!("{why}; it is tried again a minute after it failed"))
    }

    /// How many teams have an index, for the tests.
    #[doc(hidden)]
    pub fn teams(&self) -> usize {
        lock(&self.teams).len()
    }

    /// Does what the engine does when it drops reports of changes. For the
    /// tests.
    #[doc(hidden)]
    pub fn missed(&self) {
        self.shared.dropped.store(true, Ordering::Relaxed);
        self.shared.wake.notify_one();
    }

    /// How many times an object was read to be searched, and how many times
    /// an object's texts were changed in an index after it was made. For the
    /// tests.
    #[doc(hidden)]
    pub fn counts(&self) -> (u64, u64) {
        (self.shared.reads.load(Ordering::Relaxed), self.shared.puts.load(Ordering::Relaxed))
    }

    /// Makes reads of `object` fail, or work again. For the tests.
    #[doc(hidden)]
    pub fn fail_reads(&self, object: &str, failing: bool) {
        let mut set = lock(&self.shared.failing);
        if failing {
            set.insert(object.to_string());
        } else {
            set.remove(object);
        }
    }

    /// Holds `team`'s index locked for changing until `release` is sent or
    /// dropped, on a thread of its own, and returns once it is held. Gives
    /// false if the team has no index. For the tests.
    #[doc(hidden)]
    pub fn hold(&self, team: &str, release: std::sync::mpsc::Receiver<()>) -> bool {
        let Some(index) = lock(&self.teams).get(team).cloned() else { return false };
        let (held, holding) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _locked = index.write().unwrap_or_else(|e| e.into_inner());
            let _ = held.send(());
            let _ = release.recv();
        });
        holding.recv().is_ok()
    }
}

/// What `held` holds, even if a thread that held it before failed: nothing
/// it guards is left half changed.
fn lock<T>(held: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    held.lock().unwrap_or_else(|e| e.into_inner())
}

/// The model, loaded from where it comes from, and fetched there first if
/// need be. It is done on a thread that may wait, as fetching takes seconds.
async fn load(from: ModelFrom) -> Result<Arc<Model>, String> {
    let loaded = tokio::task::spawn_blocking(move || match from {
        ModelFrom::Nowhere => Err("this sidecar does not search".to_string()),
        ModelFrom::Folder(folder) => Model::open(&folder),
        ModelFrom::Fetched { folder, source } => {
            orvu_search::fetch(&folder, &source, &orvu_search::POTION)?;
            Model::open_pinned(&folder, &orvu_search::POTION)
        }
    });
    loaded.await.map_err(|e| format!("the model could not be loaded: {e}"))?.map(Arc::new)
}

/// The searchable objects `team`'s index object lists: none if there is no
/// index object, or one of another kind, and an error if it could not be
/// read.
async fn listed(client: &Client, team: &str) -> Result<BTreeSet<String>, String> {
    let index = format!("orvu/{team}/index");
    if !client.exists(&index).await {
        return Ok(BTreeSet::new());
    }
    let entries = match client.map(&index).await {
        Ok(map) => map.entries().await,
        Err(e) => Err(e),
    };
    let entries = match entries {
        Ok(entries) => entries,
        Err(orvu_sdk::SdkError::TypeMismatch { .. }) => return Ok(BTreeSet::new()),
        Err(e) => return Err(e.to_string()),
    };
    Ok(entries.into_iter().map(|(short, _)| format!("orvu/{team}/{short}")).filter(|object| kind(object).is_some()).collect())
}

/// The task that changes the indexes.
struct Follower {
    client: Arc<Client>,
    teams: Teams,
    /// What each searched team's index object lists, as it was last read.
    lists: HashMap<String, BTreeSet<String>>,
    /// Teams found to have nothing to search, and when. A search of one is
    /// answered at once for `RETRY`, not by reading everything it lists
    /// again; teams found longer ago are forgotten.
    nothing: HashMap<String, Instant>,
    /// The model, from the first index made.
    model: Option<Arc<Model>>,
    stopping: CancellationToken,
    pace: Pace,
    shared: Arc<Shared>,
}

impl Follower {
    async fn run(mut self, mut asked: mpsc::Receiver<Make>) {
        let stopping = self.stopping.clone();
        let mut again = tokio::time::interval(self.pace.again);
        again.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        again.tick().await;
        // The changes are followed from the first search on: those to notes,
        // documents and task boards, and to teams' index objects, which say what is
        // searched. When reports are dropped, the loop is woken to read
        // everything again, though at most once in `RESTING`.
        let (report, mut reported) = mpsc::channel(WAITING);
        let shared = self.shared.clone();
        let mut report = self.pace.following.then_some(report);
        // Following stops when this is dropped, so it is kept until the loop
        // ends.
        let mut _following = None;
        let now = tokio::time::Instant::now();
        let resting = self.pace.resting;
        let mut rested = now.checked_sub(resting).unwrap_or(now);
        loop {
            tokio::select! {
                _ = stopping.cancelled() => return,
                Some(make) = asked.recv() => {
                    if let Some(report) = report.take() {
                        let (full, missed) = (shared.clone(), shared.clone());
                        _following = Some(self.client.follow_changes(
                            move |object| {
                                let followed = kind(&object).is_some() || team_of_index(&object).is_some();
                                if followed && report.try_send(object).is_err() {
                                    full.dropped.store(true, Ordering::Relaxed);
                                    full.wake.notify_one();
                                }
                            },
                            move || {
                                missed.dropped.store(true, Ordering::Relaxed);
                                missed.wake.notify_one();
                            },
                        ));
                    }
                    self.model = Some(make.model);
                    self.nothing.retain(|_, when| when.elapsed() < RETRY);
                    let kept = lock(&self.teams).contains_key(&make.team) || self.nothing.contains_key(&make.team);
                    let made = if kept { Ok(()) } else { self.make(&make.team).await };
                    let _ = make.made.send(made);
                }
                Some(first) = reported.recv() => {
                    let mut changed = BTreeSet::from([first]);
                    let until = tokio::time::Instant::now() + GATHER;
                    while let Ok(Some(object)) = tokio::time::timeout_at(until, reported.recv()).await {
                        changed.insert(object);
                    }
                    for object in changed {
                        // A team found to have nothing may have something now.
                        if let Some(team) = team_of(&object) {
                            self.nothing.remove(team);
                        }
                        match team_of_index(&object) {
                            Some(team) => {
                                self.relist(team).await;
                            }
                            None => self.update(&object).await,
                        }
                    }
                }
                _ = again.tick() => self.read_again().await,
                _ = shared.wake.notified() => {}
                _ = tokio::time::sleep_until(rested + resting), if shared.dropped.load(Ordering::Relaxed) => {}
            }
            if rested.elapsed() >= resting && shared.dropped.swap(false, Ordering::Relaxed) {
                // A dropped report may have been what would have told a team
                // found to have nothing that it has something now.
                self.nothing.clear();
                self.read_again().await;
                rested = tokio::time::Instant::now();
            }
        }
    }

    /// Makes `team`'s index from every object its index object lists, or
    /// says why it could not. A team none of whose listed objects has
    /// anything to search is not kept.
    async fn make(&mut self, team: &str) -> Result<(), String> {
        let Some(model) = self.model.clone() else { return Err("there is no model".into()) };
        // Read here, not by the search that asked: it may have waited a long
        // while for the model, and the list may have changed since.
        let list = listed(&self.client, team).await?;
        let (mut read, mut failed) = (Vec::new(), false);
        for object in &list {
            if self.stopping.is_cancelled() {
                return Err("the sidecar is stopping".into());
            }
            match parts(&self.client, &self.shared, object).await {
                Read::Texts(parts) => read.push((object.clone(), parts)),
                Read::Gone => {}
                Read::Failed(why) => {
                    tracing::warn!(object, "an object could not be read to be searched: {why}");
                    failed = true;
                }
            }
        }
        // Working out many vectors takes a while, so it is done where waiting
        // holds nothing else up.
        let made = tokio::task::spawn_blocking(move || {
            let mut index = Index::default();
            for (object, parts) in read {
                index.put(&object, parts, &model);
            }
            index
        });
        let index = made.await.map_err(|e| format!("the team's index could not be made: {e}"))?;
        // A team is known to have nothing only if everything it lists was
        // read; one whose reads failed is tried again by the next search.
        if index.is_empty() {
            if !failed {
                self.nothing.insert(team.to_string(), Instant::now());
            }
        } else {
            lock(&self.teams).insert(team.to_string(), Arc::new(RwLock::new(index)));
            self.lists.insert(team.to_string(), list);
        }
        Ok(())
    }

    fn index(&self, team: &str) -> Option<Arc<RwLock<Index>>> {
        lock(&self.teams).get(team).cloned()
    }

    /// Reads what `team`'s index object lists again, if the team is searched:
    /// what it newly lists is put in, and what it no longer lists is taken
    /// out. A list that cannot be read leaves everything as it was. Gives the
    /// objects it put in.
    async fn relist(&mut self, team: &str) -> BTreeSet<String> {
        let Some(index) = self.index(team) else { return BTreeSet::new() };
        let list = match listed(&self.client, team).await {
            Ok(list) => list,
            Err(why) => {
                tracing::warn!(team, "a team's list could not be read: {why}");
                return BTreeSet::new();
            }
        };
        let held = index.read().unwrap_or_else(|e| e.into_inner()).objects();
        let before = self.lists.get(team).cloned().unwrap_or_default();
        let gone: Vec<String> = held.difference(&list).cloned().collect();
        let new: BTreeSet<String> = list.difference(&before).cloned().collect();
        self.lists.insert(team.to_string(), list);
        if !gone.is_empty() {
            let index = index.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let mut index = index.write().unwrap_or_else(|e| e.into_inner());
                for object in &gone {
                    index.remove(object);
                }
            })
            .await;
        }
        for object in &new {
            if self.stopping.is_cancelled() {
                break;
            }
            self.update(object).await;
        }
        new
    }

    /// Puts `object` in its team's index again, if the team has been
    /// searched and the object is one that is searched, and listed: what is
    /// searched is what the team lists, as when the index is made.
    async fn update(&self, object: &str) {
        let (Some(model), Some(team), Some(_)) = (&self.model, team_of(object), kind(object)) else { return };
        let Some(index) = self.index(team) else { return };
        if !self.lists.get(team).is_some_and(|list| list.contains(object)) {
            return;
        }
        // Only an object that is no longer there is taken out. One that could
        // not be read this time keeps what it had.
        let parts = match parts(&self.client, &self.shared, object).await {
            Read::Texts(parts) => parts,
            Read::Gone => {
                let (index, object) = (index.clone(), object.to_string());
                let _ = tokio::task::spawn_blocking(move || index.write().unwrap_or_else(|e| e.into_inner()).remove(&object)).await;
                return;
            }
            Read::Failed(why) => {
                tracing::warn!(object, "a changed object could not be read to be searched again: {why}");
                return;
            }
        };
        // An object whose texts did not change is left as it is. Only the
        // vectors of new texts are worked out, and the index is changed, where
        // waiting holds nothing else up; the other vectors are moved over.
        // This task is the only one that changes the indexes, so what is
        // missing cannot change in between.
        let missing = {
            let index = index.read().unwrap_or_else(|e| e.into_inner());
            if index.holds(object, &parts) {
                return;
            }
            index.missing(object, &parts)
        };
        let (model, object) = (model.clone(), object.to_string());
        self.shared.puts.fetch_add(1, Ordering::Relaxed);
        let put = tokio::task::spawn_blocking(move || {
            let prepared = missing.prepare(&model);
            index.write().unwrap_or_else(|e| e.into_inner()).put_prepared(&object, parts, prepared);
        });
        if let Err(e) = put.await {
            tracing::warn!("a changed object could not be searched again: {e}");
        }
    }

    /// Reads every searched team's list and objects again.
    async fn read_again(&mut self) {
        let teams: Vec<String> = lock(&self.teams).keys().cloned().collect();
        for team in teams {
            let read = self.relist(&team).await;
            let objects: Vec<String> =
                self.lists.get(&team).map(|list| list.difference(&read).cloned().collect()).unwrap_or_default();
            for object in objects {
                // A sidecar that is stopping does not read on.
                if self.stopping.is_cancelled() {
                    return;
                }
                self.update(&object).await;
            }
        }
    }
}

/// What kind of searched object `object` is, if it is one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The map of a team's simple notes, which keeps values written at once.
    Notes,
    /// One structured note.
    Structured,
    Document,
    /// A team's task board.
    Tasks,
}

/// The team an object of a block belongs to.
fn team_of(object: &str) -> Option<&str> {
    crate::rules::team_and_rest(object).map(|(team, _)| team)
}

/// The team whose index is `object`.
pub fn team_of_index(object: &str) -> Option<&str> {
    crate::rules::team_and_rest(object).and_then(|(team, rest)| (rest == "index").then_some(team))
}

fn kind(object: &str) -> Option<Kind> {
    let (_, rest) = crate::rules::team_and_rest(object)?;
    match rest.split_once('/') {
        None if rest == "notes-mv" => Some(Kind::Notes),
        None if rest == "tasks" => Some(Kind::Tasks),
        Some(("note", name)) if !name.is_empty() && !name.contains('/') => Some(Kind::Structured),
        Some(("doc", name)) if !name.is_empty() && !name.contains('/') => Some(Kind::Document),
        _ => None,
    }
}

/// What reading a searched object gave.
enum Read {
    /// There is no such object.
    Gone,
    /// It could not be read, and why.
    Failed(String),
    /// Its texts, each with where in the object it is. A long text is given
    /// in pieces, each with the same part, and each starting with the name
    /// of the note it is from.
    Texts(Vec<(String, String)>),
}

async fn parts(client: &Client, shared: &Shared, object: &str) -> Read {
    shared.reads.fetch_add(1, Ordering::Relaxed);
    if lock(&shared.failing).contains(object) {
        return Read::Failed("made to fail".into());
    }
    if !client.exists(object).await {
        return Read::Gone;
    }
    let (Some(kind), name) = (kind(object), object.rsplit('/').next().unwrap_or_default()) else { return Read::Gone };
    // Each is a part, what every piece of it starts with, and its text.
    let read: Result<Vec<(String, String, String)>, orvu_sdk::SdkError> = match kind {
        // Each of a note's values is searched, under the note's name.
        Kind::Notes => match client.mv_map(object).await {
            Ok(map) => map.entries().await.map(|entries| {
                entries
                    .into_iter()
                    .filter(|(key, _)| key != crate::notes::MOVED)
                    .flat_map(|(key, values)| {
                        values.into_iter().map(move |value| {
                            (key.clone(), format!("{key}: "), plain(crate::notes::split(&value).1))
                        })
                    })
                    .collect()
            }),
            Err(e) => Err(e),
        },
        Kind::Structured => match client.json(object).await {
            Ok(json) => json.to_value().await.map(|value| {
                let mut said = Vec::new();
                words(&value, &mut said);
                vec![(String::new(), format!("{name}: "), said.join(" "))]
            }),
            Err(e) => Err(e),
        },
        Kind::Document => match client.rich_text(object).await {
            Ok(text) => text.text().await.map(|text| {
                paragraphs(&text).into_iter().map(|(part, text)| (part, String::new(), text)).collect()
            }),
            Err(e) => Err(e),
        },
        // Each task is searched by its title and notes, under its id and
        // title. What is on the board and has no title is not a task.
        Kind::Tasks => match client.json(object).await {
            Ok(json) => json.to_value().await.map(|board| match board {
                JsonValue::Object(tasks) => tasks
                    .into_iter()
                    .filter_map(|(id, task)| {
                        let JsonValue::Object(fields) = task else { return None };
                        let Some(JsonValue::Str(title)) = fields.get("title") else { return None };
                        let notes = match fields.get("notes") {
                            Some(JsonValue::Str(notes)) => notes.clone(),
                            _ => String::new(),
                        };
                        Some((json!([id, title]).to_string(), format!("{title}: "), notes))
                    })
                    .collect(),
                _ => Vec::new(),
            }),
            Err(e) => Err(e),
        },
    };
    match read {
        Ok(texts) => Read::Texts(
            texts
                .into_iter()
                .flat_map(|(part, start, text)| {
                    pieces_after(&start, &text).into_iter().map(move |piece| (part.clone(), piece))
                })
                .collect(),
        ),
        // An object of another kind under a searched name is not one that is
        // searched; it is not read again and again.
        Err(orvu_sdk::SdkError::TypeMismatch { .. }) => Read::Gone,
        Err(why) => Read::Failed(why.to_string()),
    }
}

/// The pieces of `text`, each starting with `start`, such as a note's name,
/// and no longer than the model reads with it. A note with nothing in it is
/// one piece, its name, so that it is found by that; a paragraph with
/// nothing in it is none.
fn pieces_after(start: &str, text: &str) -> Vec<String> {
    // A start that would take more than half of a piece is shortened to
    // half, so that every piece has room for its text.
    let start = match start.strip_suffix(": ") {
        Some(name) if start.split_whitespace().count() > PIECE_WORDS / 2 || start.chars().count() > PIECE_CHARS / 2 => {
            format!("{}: ", cut(name, PIECE_WORDS / 2 - 1, PIECE_CHARS / 2 - 2).into_iter().next().unwrap_or_default())
        }
        _ => start.to_string(),
    };
    if text.trim().is_empty() {
        return if start.is_empty() { Vec::new() } else { vec![start] };
    }
    let words = PIECE_WORDS - start.split_whitespace().count();
    let chars = PIECE_CHARS - start.chars().count();
    cut(text, words, chars).into_iter().map(|piece| format!("{start}{piece}")).collect()
}

fn cut(text: &str, most_words: usize, most_chars: usize) -> Vec<String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= most_words && text.chars().count() <= most_chars {
        return vec![text.to_string()];
    }
    let (mut found, mut piece, mut counted, mut chars) = (Vec::new(), String::new(), 0, 0);
    for word in words {
        let mut word: Vec<char> = word.chars().collect();
        while !word.is_empty() {
            if counted == most_words || (counted > 0 && chars + 1 + word.len() > most_chars) {
                found.push(std::mem::take(&mut piece));
                (counted, chars) = (0, 0);
            }
            if counted > 0 {
                piece.push(' ');
                chars += 1;
            }
            let taken: String = word.drain(..word.len().min(most_chars)).collect();
            chars += taken.chars().count();
            piece.push_str(&taken);
            counted += 1;
        }
    }
    if !piece.is_empty() {
        found.push(piece);
    }
    found
}

/// A simple note's value as text: plain text as it is, and JSON as the
/// words in it.
fn plain(value: &str) -> String {
    match value.strip_prefix(MARK) {
        None => value.to_string(),
        Some(kept) => match JsonValue::from_json_str(kept) {
            Ok(value) => {
                let mut said = Vec::new();
                words(&value, &mut said);
                said.join(" ")
            }
            Err(_) => kept.to_string(),
        },
    }
}

/// The words of a JSON value: its texts, numbers, and the names of its
/// fields, in order.
fn words(value: &JsonValue, said: &mut Vec<String>) {
    match value {
        JsonValue::Null => {}
        JsonValue::Bool(b) => said.push(b.to_string()),
        JsonValue::Num(_) => said.push(value.to_json_string()),
        JsonValue::Str(s) => said.push(s.clone()),
        JsonValue::Array(items) => items.iter().for_each(|item| words(item, said)),
        JsonValue::Object(fields) => {
            for (field, value) in fields {
                said.push(field.clone());
                words(value, said);
            }
        }
    }
}

/// A document's paragraphs, numbered from 1: the runs of lines between
/// empty ones.
fn paragraphs(text: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if !lines.is_empty() {
                found.push(((found.len() + 1).to_string(), lines.join("\n")));
                lines.clear();
            }
        } else {
            lines.push(line.trim_end());
        }
    }
    found
}

/// What a client is told of a text that was found.
fn hit(found: &Found) -> Option<Value> {
    let name = found.object.rsplit('/').next().unwrap_or_default();
    // A match is never shown as close as nothing.
    let score = ((f64::from(found.score) * 1000.0).round() / 1000.0).max(0.001);
    Some(match kind(&found.object)? {
        Kind::Notes => json!({"kind": "note", "name": found.part, "text": found.text, "score": score}),
        Kind::Structured => json!({"kind": "structured", "name": name, "text": found.text, "score": score}),
        Kind::Document => json!({
            "kind": "document",
            "name": name,
            "paragraph": found.part.parse::<u64>().unwrap_or(0),
            "text": found.text,
            "score": score,
        }),
        // The part is the task's id and title.
        Kind::Tasks => {
            let (id, title): (String, String) = serde_json::from_str(&found.part).ok()?;
            json!({"kind": "task", "name": id, "title": title, "text": found.text, "score": score})
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_objects_searched_are_known_by_their_names() {
        assert!(kind("orvu/t/notes-mv") == Some(Kind::Notes));
        assert!(kind("orvu/t/note/plan") == Some(Kind::Structured));
        assert!(kind("orvu/t/doc/report") == Some(Kind::Document));
        assert!(kind("orvu/t/tasks") == Some(Kind::Tasks));
        for other in [
            "orvu/t/notes",
            "orvu/t/notes-by",
            "orvu/t/note/",
            "orvu/t/tasks-tree",
            "orvu/t/tasks/t-1",
            "orvu/t/inbox/bob/0",
            "orvu/t/index",
            "plain",
            "orvu//notes-mv",
            "orvu/t/doc/report/comments",
            "orvu/t/note/plan/old",
        ] {
            assert!(kind(other).is_none(), "{other}");
        }
        assert_eq!(team_of_index("orvu/t/index"), Some("t"));
        assert_eq!(team_of_index("orvu/t/x/index"), None);
        assert_eq!(team_of_index("orvu//index"), None);
    }

    #[test]
    fn a_document_is_searched_by_paragraph() {
        let text = "First line\nsecond line\n\n\n  \nAnother one\n";
        assert_eq!(
            paragraphs(text),
            [("1".to_string(), "First line\nsecond line".to_string()), ("2".to_string(), "Another one".to_string())]
        );
        assert!(paragraphs("\n\n").is_empty());
    }

    #[test]
    fn a_long_text_is_searched_in_pieces() {
        assert_eq!(pieces_after("", "short\ntext"), ["short\ntext"]);
        let long = vec!["word"; 250].join(" ");
        let cut = pieces_after("", &long);
        assert_eq!(cut.iter().map(|piece| piece.split(' ').count()).collect::<Vec<_>>(), [100, 100, 50]);
        let wide = vec!["w".repeat(300); 5].join(" ");
        assert!(pieces_after("", &wide).iter().all(|piece| piece.len() <= PIECE_CHARS));
        assert_eq!(pieces_after("", &wide).concat().replace(' ', "").len(), 1500);
    }

    #[test]
    fn pieces_are_counted_in_characters_and_cut_where_there_are_no_spaces() {
        let cyrillic = vec!["слово"; 150].join(" ");
        assert!(pieces_after("", &cyrillic).iter().all(|piece| piece.chars().count() <= PIECE_CHARS));
        assert_eq!(pieces_after("", &cyrillic).len(), 2, "cut by words, not by bytes");
        let blob = "x".repeat(2500);
        let cut = pieces_after("", &blob);
        assert_eq!(cut.iter().map(|piece| piece.chars().count()).collect::<Vec<_>>(), [1000, 1000, 500]);
    }

    #[test]
    fn a_piece_with_its_start_is_no_longer_than_the_model_reads() {
        let start = format!("{}: ", "k".repeat(200));
        for text in [vec!["word"; 300].join(" "), "x".repeat(5000)] {
            for piece in pieces_after(&start, &text) {
                assert!(piece.chars().count() <= PIECE_CHARS, "{}", piece.chars().count());
                assert!(piece.split_whitespace().count() <= PIECE_WORDS);
                assert!(piece.starts_with(&start));
            }
        }
        assert_eq!(pieces_after("k: ", &" ".repeat(2000)), ["k: "], "found by its name");
        assert_eq!(pieces_after("k: ", ""), ["k: "]);
        assert!(pieces_after("", "  ").is_empty(), "a paragraph has something in it");
    }

    #[test]
    fn a_very_long_name_still_leaves_pieces_the_model_reads_whole() {
        for start in [format!("{}: ", vec!["name"; 95].join(" ")), format!("{}: ", "n".repeat(1500))] {
            for piece in pieces_after(&start, &vec!["word"; 300].join(" ")) {
                assert!(piece.chars().count() <= PIECE_CHARS, "{}", piece.chars().count());
                assert!(piece.split_whitespace().count() <= PIECE_WORDS);
                assert!(piece.contains("word"));
            }
        }
    }

    #[test]
    fn a_name_of_nothing_but_spaces_is_no_trouble() {
        let start = format!("{}: ", " ".repeat(600));
        assert!(!pieces_after(&start, "refund").is_empty());
        assert!(!pieces_after(&start, "").is_empty());
    }

    #[test]
    fn a_note_kept_as_json_is_searched_by_its_words() {
        assert_eq!(plain("Friday"), "Friday");
        assert_eq!(plain("\u{1e}{\"day\":\"Friday\",\"hour\":3,\"sure\":true}"), "day Friday hour 3 sure true");
        assert_eq!(plain("\u{1e}not json"), "not json");
    }
}
