//! Where a developer starts: start a node, then open typed handles on it.

use crate::config::SdkConfig;
use crate::error::{Result, SdkError};
use crate::handle::{BoundedCounter, Cart, Counter, Json, List, Map, MvMap, Register, RichText, Set, Tree};
use crate::ConsistencyClass;
use orvu_core::TypedCrdt;
use orvu_crdt::{LwwRegister, OrMap, OrSet, PnCounter};
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::{QuicTransport, Transport};
use std::path::Path;
use std::sync::Arc;

/// What `on_change` and `follow_changes` return. The callback stops once this
/// is dropped. A callback already running finishes first.
#[must_use = "the callback stops as soon as this is dropped"]
pub struct ChangeFollower(tokio::task::JoinHandle<()>);

impl Drop for ChangeFollower {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A running node. Open typed handles on it with methods such as `counter` and
/// `set`. Cloning is cheap, and every clone shares the same node.
#[derive(Clone)]
pub struct Client {
    pub(crate) node: Arc<NodeHandle>,
    /// The consistency class each opened id belongs to, so one id cannot be
    /// opened under two classes.
    pub(crate) classes: Arc<std::sync::Mutex<std::collections::BTreeMap<String, ConsistencyClass>>>,
}

impl Client {
    /// Starts a node from a transport and a config you have already built.
    /// Tests use this with `MemTransport`.
    pub async fn start(transport: Arc<dyn Transport>, config: NodeConfig) -> Result<Client> {
        let node = Node::spawn(transport, config).await?;
        Ok(Client {
            node: Arc::new(node),
            classes: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
        })
    }

    /// Like `start`, but the node saves to `store` instead of the file at
    /// `config.store_path`. Tests use this with a `MemStore` they can make
    /// fail, to see what a client is told when its node cannot save.
    pub async fn start_with_store(
        transport: Arc<dyn Transport>,
        config: NodeConfig,
        store: Box<dyn orvu_runtime::Store>,
    ) -> Result<Client> {
        let node = Node::spawn_with_store(transport, config, store).await?;
        Ok(Client {
            node: Arc::new(node),
            classes: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
        })
    }

    /// Whether a change was made under the receipt `id`, before this node
    /// started or since, within the last ten minutes, `RECEIPT_KEEP_MS`. See
    /// `with_receipt`.
    pub fn receipt_made(&self, id: &[u8; 16]) -> bool {
        self.node.receipt_made(id)
    }

    /// Why this machine can no longer save changes to disk, or `None` while
    /// it can. Once it cannot, every read and write fails with
    /// `SdkError::Storage` and every `watch()` stream ends, until the
    /// program is restarted.
    pub fn storage_failure(&self) -> Option<String> {
        self.node.storage_failure()
    }

    /// Finishes once this machine can no longer save changes to disk, at
    /// once if it already cannot. A program that cannot do anything useful
    /// after that, such as a sidecar, can wait on it and stop, so that it is
    /// started again from what is on disk.
    pub async fn saving_failed(&self) {
        self.node.saving_failed().await
    }

    /// Starts a node from a TOML config file, using QUIC for the network. The
    /// file must have a `[security]` section with `cert`, `key` and `ca`.
    ///
    /// A `cert`, `key`, `ca` or storage path that is not absolute is looked up
    /// from the folder the config file is in, not from wherever the program
    /// was started. The folder the storage file goes in is made if it is not
    /// there.
    pub async fn from_toml(path: impl AsRef<Path>) -> Result<Client> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| SdkError::Config(format!("cannot read the config file {}: {e}", path.display())))?;
        let sdk = SdkConfig::parse_toml(&text)?;
        Client::from_config(sdk, path.parent().unwrap_or(Path::new(""))).await
    }

    /// Starts a node from settings that were read already, as `from_toml`
    /// does. `folder` is the one the settings file is in: paths that are not
    /// absolute are taken from it.
    ///
    /// A program that handles part of the settings itself reads the file,
    /// takes that part out, and starts the node with the rest.
    pub async fn from_config(sdk: SdkConfig, folder: &Path) -> Result<Client> {
        // Says which file could not be read. Three files are read here, and
        // the system's own message does not name the one that failed.
        let read = |what: &str, file: &Path| {
            std::fs::read_to_string(file)
                .map_err(|e| SdkError::Config(format!("cannot read {what} {}: {e}", file.display())))
        };
        let tls = sdk
            .security
            .as_ref()
            .ok_or_else(|| SdkError::Config("[security] cert/key/ca required for QUIC".into()))?;
        let cert = read("the certificate", &folder.join(&tls.cert))?;
        let key = read("the key", &folder.join(&tls.key))?;
        let ca = read("the CA certificate", &folder.join(&tls.ca))?;
        // rustls needs a crypto provider set for the whole process before QUIC
        // is built.
        orvu_transport::install_default_crypto();
        let adopt = sdk.storage.adopt_data;
        let mut config = sdk.into_node_config()?;
        let transport = Arc::new(QuicTransport::with_times(&cert, &key, &ca, config.link)?);
        if let Some(store) = config.store_path.take() {
            let store = folder.join(store);
            if let Some(holder) = store.parent().filter(|holder| !holder.as_os_str().is_empty()) {
                make_private_folder(holder).map_err(|e| {
                    SdkError::Config(format!("cannot make the storage folder {}: {e}", holder.display()))
                })?;
            }
            keep_machine_number(&store, config.replica.0, adopt)?;
            config.store_path = Some(store);
        }
        Client::start(transport, config).await
    }

    /// Records that `id` belongs to `class`. Returns `ClassMismatch` if the id
    /// is already open under a different class.
    pub fn declare(&self, id: &str, class: ConsistencyClass) -> Result<()> {
        let mut classes = self.classes.lock().expect("class map poisoned");
        match classes.get(id) {
            Some(&declared) if declared != class => {
                Err(SdkError::ClassMismatch { id: id.to_string(), declared })
            }
            _ => {
                classes.insert(id.to_string(), class);
                Ok(())
            }
        }
    }

    pub(crate) fn node(&self) -> &Arc<NodeHandle> {
        &self.node
    }

    /// The engine's numbers as they are now. Reading them does not go
    /// through the engine.
    pub fn metrics(&self) -> orvu_runtime::MetricsSnapshot {
        self.node.metrics()
    }

    /// Stops the node.
    pub async fn shutdown(&self) {
        self.node.shutdown().await;
    }

    /// Opens the counter named `id`, creating it if it does not exist yet.
    /// Fails if the id already names another type of object.
    pub async fn counter(&self, id: &str) -> Result<Counter> {
        self.open(id, "counter", ConsistencyClass::Ap, TypedCrdt::PnCounter(PnCounter::default()))
            .await?;
        Ok(Counter { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the bounded counter named `id`, creating it if it does not exist
    /// yet. A bounded counter cannot go below zero. Fails if the id already
    /// names another type of object.
    pub async fn bounded_counter(&self, id: &str) -> Result<BoundedCounter> {
        self.open(
            id,
            "bounded",
            ConsistencyClass::Escrow,
            TypedCrdt::BoundedCounter(orvu_crdt::BoundedCounter::default()),
        )
        .await?;
        Ok(BoundedCounter { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the set named `id`, creating it if it does not exist yet. Fails if
    /// the id already names another type of object.
    pub async fn set(&self, id: &str) -> Result<Set> {
        self.open(id, "set", ConsistencyClass::Ap, TypedCrdt::OrSet(OrSet::default())).await?;
        Ok(Set { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the register named `id`, creating it if it does not exist yet.
    /// Fails if the id already names another type of object.
    pub async fn register(&self, id: &str) -> Result<Register> {
        self.open(id, "register", ConsistencyClass::Ap, TypedCrdt::LwwRegister(LwwRegister::default()))
            .await?;
        Ok(Register { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the map named `id`, creating it if it does not exist yet. Fails if
    /// the id already names another type of object.
    pub async fn map(&self, id: &str) -> Result<Map> {
        self.open(id, "map", ConsistencyClass::Ap, TypedCrdt::OrMap(orvu_crdt::OrMap::default()))
            .await?;
        Ok(Map { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the map named `id` that keeps clashing writes, creating it if it
    /// does not exist yet. Fails if the id already names another type of
    /// object.
    pub async fn mv_map(&self, id: &str) -> Result<MvMap> {
        self.open(id, "mvmap", ConsistencyClass::Ap, TypedCrdt::MvMap(orvu_crdt::MvMap::default()))
            .await?;
        Ok(MvMap { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the cart named `id`, creating it if it does not exist yet. Fails
    /// if the id already names another type of object.
    pub async fn cart(&self, id: &str) -> Result<Cart> {
        self.open(id, "cart", ConsistencyClass::Ap, TypedCrdt::OrMapCounter(OrMap::default()))
            .await?;
        Ok(Cart { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the list named `id`, creating it if it does not exist yet. Fails
    /// if the id already names another type of object.
    pub async fn list(&self, id: &str) -> Result<List> {
        self.open(id, "list", ConsistencyClass::Ap, TypedCrdt::Rga(orvu_crdt::Rga::default()))
            .await?;
        Ok(List { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the tree named `id`, creating it if it does not exist yet. Fails
    /// if the id already names another type of object.
    pub async fn tree(&self, id: &str) -> Result<Tree> {
        self.open(id, "tree", ConsistencyClass::Ap, TypedCrdt::Tree(orvu_crdt::Tree::default()))
            .await?;
        Ok(Tree { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the JSON document named `id`, creating it if it does not exist
    /// yet. Fails if the id already names another type of object.
    pub async fn json(&self, id: &str) -> Result<Json> {
        self.open(id, "json", ConsistencyClass::Ap, TypedCrdt::Json(orvu_crdt::JsonCrdt::default()))
            .await?;
        Ok(Json { node: self.node().clone(), id: id.to_string() })
    }

    /// Opens the rich-text document named `id`, creating it if it does not
    /// exist yet. Fails if the id already names another type of object.
    pub async fn rich_text(&self, id: &str) -> Result<RichText> {
        self.open(id, "richtext", ConsistencyClass::Ap, TypedCrdt::RichText(orvu_crdt::RichText::default()))
            .await?;
        Ok(RichText { node: self.node().clone(), id: id.to_string() })
    }

    /// True if an object named `id` exists on this machine, of whatever type.
    /// Unlike opening it, asking does not create it.
    pub async fn exists(&self, id: &str) -> bool {
        self.node.read(id, |_| ()).await.is_some()
    }

    /// Makes a handle for `id` without creating the object and without
    /// checking its type. This is for watching an object that may not exist
    /// yet: the `watch()` stream stays quiet until an object of the handle's
    /// type appears under that name. Reads and writes through the handle fail
    /// until then. To create the object, open it with the method for its type,
    /// such as `counter`.
    pub fn attach<H: crate::handle::Handle>(&self, id: &str) -> H {
        H::attach(self.node.clone(), id)
    }

    /// Calls `cb` with the object's id each time any object changes, until
    /// the returned `ChangeFollower` is dropped or the client is shut down.
    /// Keep it for as long as the callback should run.
    ///
    /// The callbacks run one after another on one background task, so a slow
    /// callback holds up the ones after it. If it falls far enough behind,
    /// the events it missed are skipped. Use `follow_changes` to be told when
    /// that happens.
    pub fn on_change(&self, cb: impl Fn(String) + Send + 'static) -> ChangeFollower {
        self.follow_changes(cb, || {})
    }

    /// Like `on_change`, and calls `missed` each time the callback fell so
    /// far behind that changes were skipped. Which objects they were is not
    /// known, so a follower that must not miss any looks at everything again.
    pub fn follow_changes(
        &self,
        cb: impl Fn(String) + Send + 'static,
        missed: impl Fn() + Send + 'static,
    ) -> ChangeFollower {
        let mut rx = self.node.subscribe();
        let node = self.node.clone();
        let task = tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = node.stopped() => break,
                    event = rx.recv() => event,
                };
                match event {
                    Ok(id) => cb(id),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => missed(),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        ChangeFollower(task)
    }

    async fn open(
        &self,
        id: &str,
        expected: &'static str,
        class: ConsistencyClass,
        empty: TypedCrdt,
    ) -> Result<()> {
        // Refuse a class clash first, without recording anything. The class
        // is only recorded once the engine has opened the object. Recording
        // it before a failed open would leave the wrong class on the id.
        self.check_class(id, class)?;
        if self.node().open(id, empty).await {
            return self.declare(id, class);
        }
        // `open` says no both when the id holds another type and when the
        // engine has stopped. An id that still reads back holds another type.
        if self.node().read(id, |_| ()).await.is_some() {
            Err(SdkError::TypeMismatch { id: id.to_string(), expected })
        } else {
            Err(crate::handle::not_done(self.node()))
        }
    }

    /// Fails if `id` is already recorded under a class other than `class`.
    /// Unlike `declare`, this records nothing.
    fn check_class(&self, id: &str, class: ConsistencyClass) -> Result<()> {
        let classes = self.classes.lock().expect("class map poisoned");
        match classes.get(id) {
            Some(&declared) if declared != class => {
                Err(SdkError::ClassMismatch { id: id.to_string(), declared })
            }
            _ => Ok(()),
        }
    }
}

/// Checks that the data in `store` was written under the machine number
/// `replica`, and notes the number beside it the first time.
///
/// Data is saved under the number of the machine that wrote it. Opened under
/// another number, the machine would take its own earlier writes for somebody
/// else's, and two machines could end up sharing a number. Neither can be put
/// right afterwards, so a number that does not match is refused. So is saved
/// data with no note at all, unless `adopt` says to take it on.
fn keep_machine_number(store: &Path, replica: u64, adopt: bool) -> Result<()> {
    let mut note = store.as_os_str().to_owned();
    note.push(".replica");
    let note = std::path::PathBuf::from(note);
    match std::fs::read_to_string(&note) {
        Ok(kept) => match kept.trim().parse::<u64>() {
            Ok(kept) if kept == replica => Ok(()),
            Ok(kept) => Err(SdkError::Config(format!(
                "the data in {} was written as machine {kept}, and the settings say replica = {replica}. \
                 A machine keeps its number for as long as it keeps its data: put {kept} back in the \
                 settings, with the certificate for it, or start this machine with no data",
                store.display()
            ))),
            Err(_) => Err(SdkError::Config(format!(
                "{} should hold this machine's number and does not",
                note.display()
            ))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // A storage file that is missing or empty holds nothing written
            // under any number, so it is this machine's from the start.
            let holds_data = match std::fs::metadata(store) {
                Ok(file) => file.len() > 0,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(SdkError::Config(format!("cannot look at {}: {e}", store.display()))),
            };
            if holds_data && !adopt {
                return Err(SdkError::Config(format!(
                    "{} holds saved data but there is no note of the machine number it was written \
                     under ({} is missing), so it is not known whose writes it holds. Restore that \
                     file, or, if the data is known to have been written as machine {replica}, add \
                     adopt_data = true to [storage] once to take it on",
                    store.display(),
                    note.display()
                )));
            }
            std::fs::write(&note, format!("{replica}\n"))
                .map_err(|e| SdkError::Config(format!("cannot write {}: {e}", note.display())))
        }
        Err(e) => Err(SdkError::Config(format!("cannot read {}: {e}", note.display()))),
    }
}

/// Makes `folder` and whatever is missing above it. What is made is for this
/// user only, because it will hold data or keys. A folder that is already
/// there is left as it is.
pub fn make_private_folder(folder: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_crdt::ReplicaId;
    use orvu_transport::mem::MemTransport;

    #[tokio::test]
    async fn client_starts_over_mem_transport() {
        let fabric = Arc::new(MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = Client::start(fabric.endpoint(1), cfg).await.unwrap();
        client.shutdown().await;
    }

    // A program that runs the engine can read its numbers without going
    // through the engine, to show them next to its own.
    #[tokio::test]
    async fn the_engine_numbers_can_be_read() {
        let fabric = Arc::new(MemTransport::new());
        let client = Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "solo")).await.unwrap();
        assert_eq!(client.metrics().deltas_applied_local, 0);
        client.counter("visits").await.unwrap().increment(1).await.unwrap();
        assert_eq!(client.metrics().deltas_applied_local, 1);
        assert_eq!(client.metrics().objects, 1);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn reopening_an_id_under_a_different_tier_is_refused() {
        let fabric = Arc::new(MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = Client::start(fabric.endpoint(1), cfg).await.unwrap();
        client.counter("shared").await.expect("an AP counter opens");
        let err = client
            .declare("shared", ConsistencyClass::Escrow)
            .expect_err("the same id cannot be two tiers at once");
        assert!(
            matches!(err, SdkError::ClassMismatch { ref id, declared: ConsistencyClass::Ap } if id == "shared"),
            "got {err:?}"
        );
        client.shutdown().await;
    }

    /// The check also works the other way round. An id declared `Escrow` first
    /// refuses an `Ap` open, and the error names the class that was recorded
    /// first, not the one the caller just asked for.
    #[tokio::test]
    async fn an_ap_open_on_an_id_declared_escrow_is_refused_naming_escrow() {
        let fabric = Arc::new(MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = Client::start(fabric.endpoint(1), cfg).await.unwrap();
        client.declare("shared", ConsistencyClass::Escrow).expect("the first declaration records");
        let Err(err) = client.counter("shared").await else {
            panic!("an AP open under an Escrow id must be refused");
        };
        assert!(
            matches!(err, SdkError::ClassMismatch { ref id, declared: ConsistencyClass::Escrow } if id == "shared"),
            "got {err:?}"
        );
        client.shutdown().await;
    }

    /// A bounded counter is the one handle whose class is `Escrow`. So
    /// declaring its id as `Ap` afterwards must be refused, and the error names
    /// `Escrow`.
    #[tokio::test]
    async fn a_bounded_counter_is_recorded_as_escrow_not_ap() {
        let fabric = Arc::new(MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = Client::start(fabric.endpoint(1), cfg).await.unwrap();
        client.bounded_counter("budget").await.expect("opens");
        let err = client.declare("budget", ConsistencyClass::Ap).expect_err("wrong tier");
        assert!(
            matches!(err, SdkError::ClassMismatch { declared: ConsistencyClass::Escrow, .. }),
            "got {err:?}"
        );
        client.shutdown().await;
    }

    // A follower too slow to keep up is told that it missed changes, so it
    // can look at everything again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_follower_that_falls_behind_is_told() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let fabric = Arc::new(MemTransport::new());
        let mut cfg = NodeConfig::new(ReplicaId(1), "solo");
        cfg.ship_queue_cap = 4;
        let client = Client::start(fabric.endpoint(1), cfg).await.unwrap();
        let missed = Arc::new(AtomicBool::new(false));
        let told = missed.clone();
        let _follower = client.follow_changes(
            |_| std::thread::sleep(std::time::Duration::from_millis(100)),
            move || told.store(true, Ordering::SeqCst),
        );
        let visits = client.counter("visits").await.unwrap();
        for _ in 0..50 {
            visits.increment(1).await.unwrap();
        }
        for _ in 0..100 {
            if missed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(missed.load(Ordering::SeqCst), "the follower was not told it missed changes");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn reopening_under_the_same_class_is_not_a_mismatch() {
        let fabric = Arc::new(MemTransport::new());
        let cfg = NodeConfig::new(ReplicaId(1), "solo");
        let client = Client::start(fabric.endpoint(1), cfg).await.unwrap();
        client.counter("shared").await.expect("opens");
        client.counter("shared").await.expect("the same id under the same class reopens");
        client.shutdown().await;
    }
}
