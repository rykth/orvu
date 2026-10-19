//! Tests for the less happy paths: two things happening at once, a failed
//! open, a client that has been shut down, and borrowing when not every
//! machine can be reached.

use crate::{Client, SdkError};
use futures::StreamExt;
use orvu_core::TypedCrdt;
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::NodeConfig;
use orvu_transport::mem::MemTransport;
use orvu_transport::PeerAddr;
use std::sync::Arc;
use std::time::Duration;

async fn solo() -> Client {
    let network = Arc::new(MemTransport::new());
    Client::start(network.endpoint(1), NodeConfig::new(ReplicaId(1), "solo")).await.unwrap()
}

// Two pieces of text are inserted at the same spot at the same time. Each one
// must stay in one piece, whichever goes first.
#[tokio::test(flavor = "multi_thread")]
async fn two_inserts_at_once_do_not_mix_their_letters() {
    let client = solo().await;
    let text = client.rich_text("doc").await.unwrap();
    let (first, second) = (text.clone(), text.clone());

    let a = tokio::spawn(async move { first.insert(0, "a".repeat(200)).await });
    let b = tokio::spawn(async move { second.insert(0, "b".repeat(200)).await });
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();

    let got = text.text().await.unwrap();
    let a_then_b = "a".repeat(200) + &"b".repeat(200);
    let b_then_a = "b".repeat(200) + &"a".repeat(200);
    assert!(got == a_then_b || got == b_then_a, "the letters were mixed: {got}");
    client.shutdown().await;
}

// "stock" already exists as a plain counter, for example because another
// machine made it. Opening it as a bounded counter fails. That failure must
// not stop it being opened as what it really is.
#[tokio::test]
async fn a_failed_open_does_not_block_the_right_one() {
    let client = solo().await;
    client.node.create("stock", TypedCrdt::PnCounter(PnCounter::default())).await;

    let wrong = client.bounded_counter("stock").await;
    assert!(matches!(wrong, Err(SdkError::TypeMismatch { .. })), "got {:?}", wrong.err());

    client.counter("stock").await.expect("it is a counter, so this open is right");
    client.shutdown().await;
}

#[tokio::test]
async fn opening_after_shutdown_says_the_engine_is_gone() {
    let client = solo().await;
    client.shutdown().await;
    let err = client.counter("visits").await.err().expect("the engine has stopped");
    assert!(matches!(err, SdkError::Engine), "got {err:?}");
}

// A program waiting for the next change must be let go when the client is
// shut down.
#[tokio::test]
async fn a_watch_stream_ends_at_shutdown() {
    let client = solo().await;
    let counter = client.counter("visits").await.unwrap();
    let mut changes = Box::pin(counter.watch());
    assert_eq!(changes.next().await, Some(0));

    client.shutdown().await;
    let next = tokio::time::timeout(Duration::from_secs(2), changes.next()).await;
    assert_eq!(next.expect("the stream did not end"), None);
}

// Machines 1 and 3 both have allowance to spare. Machine 2 is connected to 3
// only, and knows of 1 through 3. When 2 needs to borrow, it must not give up
// because 1 cannot be reached.
#[tokio::test(flavor = "multi_thread")]
async fn borrowing_skips_a_machine_that_is_not_connected() {
    let network = Arc::new(MemTransport::new());
    let settings = |id: u64, name: &str, peers: &[&str]| {
        let mut cfg = NodeConfig::new(ReplicaId(id), name);
        cfg.peers = peers.iter().map(|p| PeerAddr::new(*p)).collect();
        cfg.anti_entropy_interval = Duration::from_millis(30);
        cfg
    };
    let one = Client::start(network.endpoint(1), settings(1, "one", &["three"])).await.unwrap();
    let three = Client::start(network.endpoint(3), settings(3, "three", &[])).await.unwrap();
    let two = Client::start(network.endpoint(2), settings(2, "two", &["three"])).await.unwrap();

    let stock_one = one.bounded_counter("stock").await.unwrap();
    let stock_three = three.bounded_counter("stock").await.unwrap();
    let stock_two = two.bounded_counter("stock").await.unwrap();
    stock_one.increment(10).await.unwrap();
    stock_three.increment(10).await.unwrap();

    // Wait until 2 has both deposits and has heard of machine 1.
    for _ in 0..200 {
        let heard_of_one = two.node.known_members().await.contains(&ReplicaId(1));
        if stock_two.value().await.unwrap() == 20 && heard_of_one {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(stock_two.value().await.unwrap(), 20);

    stock_two.try_decrement(3).await.expect("machine 3 could lend");

    one.shutdown().await;
    two.shutdown().await;
    three.shutdown().await;
}

// Asking whether an object exists, and watching one that does not exist yet,
// must not create it. Otherwise the name would be taken for one type before
// anyone had written anything.
#[tokio::test]
async fn asking_and_watching_do_not_create_the_object() {
    let client = solo().await;
    assert!(!client.exists("basket").await);

    let waiting: crate::Counter = client.attach("basket");
    let mut changes = Box::pin(waiting.watch());
    assert!(!client.exists("basket").await, "attaching created the object");

    // The name is still free for another type.
    client.cart("basket").await.expect("the name was taken");
    assert!(client.exists("basket").await);

    // A watch on a missing name sees the object once it arrives.
    let later: crate::Counter = client.attach("visits");
    let mut arrivals = Box::pin(later.watch());
    client.counter("visits").await.unwrap().increment(5).await.unwrap();
    let mut seen = arrivals.next().await;
    if seen == Some(0) {
        seen = arrivals.next().await;
    }
    assert_eq!(seen, Some(5));

    // The counter watch on "basket" stays quiet, because "basket" is a cart.
    let quiet = tokio::time::timeout(Duration::from_millis(100), changes.next()).await;
    assert!(quiet.is_err(), "a counter watch gave a value for a cart");
    client.shutdown().await;
}

// Several files are read when starting from a settings file. An error has to
// say which one, and a path that is not absolute is looked up next to the
// settings file.
#[tokio::test]
async fn a_missing_certificate_is_named_in_the_error() {
    let dir = temp_folder();
    let folder = dir.path();
    let config = folder.join("orvu.toml");
    std::fs::write(
        &config,
        "replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n\
         [security]\ncert = \"certs/node1.pem\"\nkey = \"k.pem\"\nca = \"ca.pem\"\n",
    )
    .unwrap();

    let err = Client::from_toml(&config).await.err().expect("the certificate is missing");
    let wanted = folder.join("certs/node1.pem");
    let message = err.to_string();
    assert!(message.contains(&wanted.display().to_string()), "the file is not named: {message}");

    let missing = Client::from_toml(folder.join("nope.toml")).await.err().unwrap().to_string();
    assert!(missing.contains("nope.toml"), "the file is not named: {missing}");
}

// The storage path is looked up next to the settings file, like the
// certificates, and its folder is made. A settings folder can then be copied
// to a machine and started from anywhere.
#[tokio::test]
async fn a_storage_path_is_looked_up_next_to_the_settings_file_and_its_folder_is_made() {
    let dir = temp_folder();
    let folder = dir.path();
    let ca = orvu_transport::generate_dev_ca();
    let node = orvu_transport::issue_node_cert(&ca, 1);
    std::fs::write(folder.join("ca.pem"), &ca.cert_pem).unwrap();
    std::fs::write(folder.join("node.pem"), &node.cert_pem).unwrap();
    std::fs::write(folder.join("node-key.pem"), &node.key_pem).unwrap();
    let config = folder.join("orvu.toml");
    std::fs::write(
        &config,
        "replica = 1\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n\
         [security]\ncert = \"node.pem\"\nkey = \"node-key.pem\"\nca = \"ca.pem\"\n\
         [storage]\npath = \"data/deep/orvu.redb\"\n",
    )
    .unwrap();

    let client = Client::from_toml(&config).await.expect("starts");
    client.counter("visits").await.unwrap().increment(5).await.unwrap();
    client.shutdown().await;
    assert!(folder.join("data/deep/orvu.redb").exists(), "the data did not go next to the settings file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(folder.join("data")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the data folder is open to other users");
    }

    // The data is found again from the same file.
    let client = Client::from_toml(&config).await.expect("starts again");
    assert_eq!(client.counter("visits").await.unwrap().value().await.unwrap(), 5);
    client.shutdown().await;
}

// Settings that were read already can be started from, with their paths
// taken from the folder they came from. A program reads the file once, takes
// out what it handles itself, and starts the engine with the rest.
#[tokio::test]
async fn settings_already_read_start_the_same_engine() {
    let dir = settings_folder(1);
    let folder = dir.path();
    let text = std::fs::read_to_string(folder.join("orvu.toml")).unwrap()
        + "[metrics]\naddr = \"127.0.0.1:1\"\n";
    let mut settings = crate::SdkConfig::parse_toml(&text).unwrap();
    // The program serves the page itself, so the engine must not try to.
    settings.metrics.addr = None;
    let client = Client::from_config(settings, folder).await.expect("starts");
    client.counter("visits").await.unwrap().increment(2).await.unwrap();
    client.shutdown().await;
    assert!(folder.join("data/orvu.redb").exists(), "the data did not go next to the settings file");
}

/// A new empty folder, removed again when the value is dropped, even when
/// the test fails.
fn temp_folder() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("orvu-sdk-").tempdir().unwrap()
}

/// A folder with a settings file for machine `replica` and the certificate
/// that goes with it, removed again when the value is dropped.
fn settings_folder(replica: u64) -> tempfile::TempDir {
    let dir = temp_folder();
    write_settings(dir.path(), replica);
    dir
}

fn write_settings(folder: &std::path::Path, replica: u64) {
    let ca = orvu_transport::generate_dev_ca();
    let node = orvu_transport::issue_node_cert(&ca, replica);
    std::fs::write(folder.join("ca.pem"), &ca.cert_pem).unwrap();
    std::fs::write(folder.join("node.pem"), &node.cert_pem).unwrap();
    std::fs::write(folder.join("node-key.pem"), &node.key_pem).unwrap();
    std::fs::write(
        folder.join("orvu.toml"),
        format!(
            "replica = {replica}\nnamespace = \"n\"\nlisten = \"127.0.0.1:0\"\n\
             [security]\ncert = \"node.pem\"\nkey = \"node-key.pem\"\nca = \"ca.pem\"\n\
             [storage]\npath = \"data/orvu.redb\"\n"
        ),
    )
    .unwrap();
}

// Data is saved under the number of the machine that wrote it. Started under
// another number, the machine would count its own earlier writes as somebody
// else's, and nothing could put that right. So the number is kept beside the
// data, and a settings file that says another one is refused.
#[tokio::test]
async fn data_written_as_one_machine_is_not_opened_as_another() {
    let dir = settings_folder(3);
    let folder = dir.path();
    let config = folder.join("orvu.toml");
    let client = Client::from_toml(&config).await.expect("starts");
    client.counter("visits").await.unwrap().increment(5).await.unwrap();
    client.shutdown().await;

    // The same folder, now with settings for machine 2.
    write_settings(folder, 2);
    let refused = Client::from_toml(&config).await.err().expect("another number was accepted").to_string();
    assert!(refused.contains("machine 3") && refused.contains("replica = 2"), "{refused}");

    // With its own number back, it starts and has its data.
    write_settings(folder, 3);
    let client = Client::from_toml(&config).await.expect("starts again");
    assert_eq!(client.counter("visits").await.unwrap().value().await.unwrap(), 5);
    client.shutdown().await;
}

// Data with no note of its machine number could have been written under any
// number. Taking it on under whatever the settings say could make this machine
// take its own earlier writes for somebody else's, so it is refused unless the
// settings say to take it on.
#[tokio::test]
async fn data_with_no_note_of_its_number_is_taken_on_only_when_asked() {
    let dir = settings_folder(4);
    let folder = dir.path();
    let config = folder.join("orvu.toml");
    let client = Client::from_toml(&config).await.expect("starts");
    client.counter("visits").await.unwrap().increment(3).await.unwrap();
    client.shutdown().await;
    let note = folder.join("data/orvu.redb.replica");
    std::fs::remove_file(&note).unwrap();

    let refused = Client::from_toml(&config).await.err().expect("data with no note was taken on").to_string();
    assert!(refused.contains("adopt_data"), "{refused}");
    assert!(!note.exists(), "a note was written anyway");

    let mut settings = std::fs::OpenOptions::new().append(true).open(&config).unwrap();
    std::io::Write::write_all(&mut settings, b"adopt_data = true\n").unwrap();
    drop(settings);
    let client = Client::from_toml(&config).await.expect("taken on when asked");
    assert_eq!(client.counter("visits").await.unwrap().value().await.unwrap(), 3);
    client.shutdown().await;
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "4\n");
}

// An empty storage file holds nothing written under any number, so it is
// taken on and noted, as a new one is.
#[tokio::test]
async fn an_empty_storage_file_needs_no_note() {
    let dir = settings_folder(5);
    let folder = dir.path();
    std::fs::create_dir_all(folder.join("data")).unwrap();
    std::fs::write(folder.join("data/orvu.redb"), b"").unwrap();
    let client = Client::from_toml(folder.join("orvu.toml")).await.expect("starts");
    client.shutdown().await;
    assert_eq!(std::fs::read_to_string(folder.join("data/orvu.redb.replica")).unwrap(), "5\n");
}

// Which reads copy the whole object, and which do not. Looking up one thing
// must never copy: that is what made a read slower the more an object held.
// A few reads do copy, on purpose: working their answer out takes longer than
// a copy does, and it is better done outside the engine than inside it.
#[tokio::test]
async fn a_read_of_one_thing_never_copies_the_object() {
    use orvu_crdt::{JsonValue, Seg};
    let client = solo().await;
    let copies = || client.node().metrics().objects_copied;

    let counter = client.counter("counter").await.unwrap();
    let register = client.register("register").await.unwrap();
    let set = client.set("set").await.unwrap();
    let map = client.map("map").await.unwrap();
    let mv = client.mv_map("mv").await.unwrap();
    let cart = client.cart("cart").await.unwrap();
    let json = client.json("json").await.unwrap();
    let tree = client.tree("tree").await.unwrap();
    let budget = client.bounded_counter("budget").await.unwrap();
    let list = client.list("list").await.unwrap();
    let text = client.rich_text("text").await.unwrap();
    counter.increment(1).await.unwrap();
    register.set("x").await.unwrap();
    set.add("a").await.unwrap();
    map.put("k", "v").await.unwrap();
    mv.put("k", "v").await.unwrap();
    cart.add("item", 2).await.unwrap();
    json.set(&[Seg::Key("a".into())], JsonValue::Num(1.0)).await.unwrap();
    tree.move_node("child", "root").await.unwrap();
    tree.set_label("child", "A").await.unwrap();
    budget.increment(5).await.unwrap();
    text.insert(0, "hello").await.unwrap();
    assert_eq!(copies(), 0, "a write copied the object");

    assert_eq!(counter.value().await.unwrap(), 1);
    assert_eq!(register.get().await.unwrap().as_deref(), Some("x"));
    assert!(set.contains("a").await.unwrap());
    assert_eq!(set.elements().await.unwrap(), ["a"]);
    assert_eq!(map.get("k").await.unwrap().as_deref(), Some("v"));
    assert_eq!(map.keys().await.unwrap(), ["k"]);
    assert_eq!(map.entries().await.unwrap().len(), 1);
    assert_eq!(mv.get("k").await.unwrap(), ["v"]);
    assert_eq!(mv.keys().await.unwrap(), ["k"]);
    assert_eq!(mv.entries().await.unwrap().len(), 1);
    assert_eq!(cart.quantity("item").await.unwrap(), 2);
    assert_eq!(json.get(&[Seg::Key("a".into())]).await.unwrap(), Some(JsonValue::Num(1.0)));
    assert!(matches!(json.to_value().await.unwrap(), JsonValue::Object(_)));
    assert_eq!(tree.parent("child").await.unwrap().as_deref(), Some("root"));
    assert_eq!(tree.label("child").await.unwrap().as_deref(), Some("A"));
    assert_eq!(tree.children("root").await.unwrap(), ["child"]);
    assert_eq!(budget.value().await.unwrap(), 5);
    assert_eq!(budget.local_allowance().await.unwrap(), 5);
    assert!(client.exists("map").await && !client.exists("nothing-here").await);
    assert!(matches!(client.list("map").await, Err(SdkError::TypeMismatch { .. })));
    assert_eq!(copies(), 0, "a read of one thing copied the object");

    // These work an answer out of the whole object, and that takes longer
    // than copying it. Each takes one copy and does its work outside.
    let mut expected = 0;
    list.insert(0, "first").await.unwrap(); // looks up where to insert
    expected += 1;
    assert_eq!(copies(), expected);
    assert_eq!(list.get(0).await.unwrap().as_deref(), Some("first"));
    assert_eq!(list.len().await.unwrap(), 1);
    assert_eq!(list.to_vec().await.unwrap(), ["first"]);
    list.delete(0).await.unwrap();
    expected += 4;
    assert_eq!(copies(), expected);
    assert_eq!(text.text().await.unwrap(), "hello");
    assert_eq!(text.to_spans().await.unwrap().len(), 1);
    assert!(tree.to_nodes().await.unwrap().iter().any(|node| node.id == "child"));
    assert!(!tree.nodes().await.unwrap().is_empty());
    assert_eq!(cart.items().await.unwrap(), [("item".to_string(), 2)]);
    expected += 5;
    assert_eq!(copies(), expected);
    client.shutdown().await;
}

/// Machine 1 ("one") with plenty to lend, and machine 2 ("two") with nothing,
/// connected to each other. `tweak` changes machine 2's settings.
async fn lender_and_spender(tweak: impl FnOnce(&mut NodeConfig)) -> (Client, Client) {
    let network = Arc::new(MemTransport::new());
    let mut cfg_one = NodeConfig::new(ReplicaId(1), "one");
    cfg_one.peers = vec![PeerAddr::new("two")];
    cfg_one.anti_entropy_interval = Duration::from_millis(30);
    let mut cfg_two = NodeConfig::new(ReplicaId(2), "two");
    cfg_two.anti_entropy_interval = Duration::from_millis(30);
    tweak(&mut cfg_two);
    let one = Client::start(network.endpoint(1), cfg_one).await.unwrap();
    let two = Client::start(network.endpoint(2), cfg_two).await.unwrap();
    (one, two)
}

// Two withdrawals start at nearly the same time on one machine, and both have
// to borrow. The first grant to arrive must not be taken by one of them while
// the other is refused with its own grant still on the way.
#[tokio::test(flavor = "multi_thread")]
async fn two_withdrawals_that_borrow_at_once_are_both_served() {
    let (one, two) = lender_and_spender(|cfg| cfg.borrow_timeout = Duration::from_secs(1)).await;
    let lender = one.bounded_counter("stock").await.unwrap();
    let spender = two.bounded_counter("stock").await.unwrap();
    lender.increment(1000).await.unwrap();
    for _ in 0..200 {
        if spender.value().await.unwrap() == 1000 && two.node.metrics().peers_up > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(spender.value().await.unwrap(), 1000);

    // Each round, machine 2 has no allowance of its own, so both withdrawals
    // borrow. The second starts a little later each round, so in some rounds
    // the two grants arrive in different batches.
    for round in 0..20u64 {
        assert_eq!(spender.local_allowance().await.unwrap(), 0);
        let (first, second) = (spender.clone(), spender.clone());
        let a = tokio::spawn(async move { first.try_decrement(5).await });
        let b = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(round)).await;
            second.try_decrement(5).await
        });
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert!(a.is_ok() && b.is_ok(), "round {round}: {a:?} {b:?}");
    }
    assert_eq!(spender.value().await.unwrap(), 800);
    one.shutdown().await;
    two.shutdown().await;
}

// A withdrawal that does not need to borrow may spend part of a grant the
// moment it lands, before the borrower it was meant for gets to it. The
// borrower must then ask again, not wait out its whole borrowing time and be
// refused while the lender still has plenty.
#[tokio::test(flavor = "multi_thread")]
async fn a_borrower_whose_grant_is_spent_by_another_asks_again() {
    let (one, two) = lender_and_spender(|cfg| cfg.borrow_timeout = Duration::from_secs(1)).await;
    let lender = one.bounded_counter("stock").await.unwrap();
    let spender = two.bounded_counter("stock").await.unwrap();
    lender.increment(1000).await.unwrap();
    for _ in 0..200 {
        if spender.value().await.unwrap() == 1000 && two.node.metrics().peers_up > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(spender.value().await.unwrap(), 1000);

    // Another task watches the allowance and takes 1 as soon as there is
    // any, so it often spends from a grant of 5 the moment it lands. It only
    // asks when it has just seen allowance, so it rarely borrows itself, but
    // it can: the borrower may spend that allowance between the look and the
    // withdrawal, and then this one borrows like any other. It stops after
    // 10: one that never stopped could take every grant, and the borrower
    // would then run out of time however often it asked.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (nibbler, stop) = (spender.clone(), done.clone());
    let nibbling = tokio::spawn(async move {
        let mut taken = 0u64;
        while taken < 10 && !stop.load(std::sync::atomic::Ordering::SeqCst) {
            if nibbler.local_allowance().await.unwrap_or(0) > 0 && nibbler.try_decrement(1).await.is_ok() {
                taken += 1;
            }
            tokio::task::yield_now().await;
        }
        taken
    });
    let started = std::time::Instant::now();
    for round in 0..20 {
        let result = spender.try_decrement(5).await;
        assert!(result.is_ok(), "round {round}: {result:?} after {:?}", started.elapsed());
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let nibbled = nibbling.await.unwrap();
    assert!(nibbled > 0, "the other task took nothing, so nothing was tested");
    one.shutdown().await;
    two.shutdown().await;
}

/// A store that can be made to take a long time over each save.
struct Slow(orvu_store::MemStore, Arc<std::sync::atomic::AtomicU64>);

impl orvu_store::Store for Slow {
    fn append(&mut self, delta: &orvu_core::WireDelta) -> orvu_store::Result<()> {
        std::thread::sleep(Duration::from_millis(self.1.load(std::sync::atomic::Ordering::SeqCst)));
        self.0.append(delta)
    }
    fn receipt(&mut self, receipt: orvu_store::Receipt) -> orvu_store::Result<()> {
        self.0.receipt(receipt)
    }
    fn receipts(&self) -> orvu_store::Result<Vec<orvu_store::Receipt>> {
        self.0.receipts()
    }
    fn replay(&self) -> orvu_store::Result<Vec<orvu_core::WireDelta>> {
        self.0.replay()
    }
    fn flush(&mut self) -> orvu_store::Result<()> {
        self.0.flush()
    }
    fn compact_to(&mut self, snapshot: &[orvu_core::SnapshotEntry], tail: &[orvu_core::WireDelta]) -> orvu_store::Result<()> {
        self.0.compact_to(snapshot, tail)
    }
    fn load_snapshot(&self) -> orvu_store::Result<Vec<orvu_core::SnapshotEntry>> {
        self.0.load_snapshot()
    }
    fn deltas_peer_lacks(
        &self,
        peer: &std::collections::BTreeMap<String, orvu_crdt::DotContext>,
    ) -> orvu_store::Result<Vec<orvu_core::WireDelta>> {
        self.0.deltas_peer_lacks(peer)
    }
    fn snapshot_contexts(&self) -> orvu_store::Result<std::collections::BTreeMap<String, orvu_crdt::DotContext>> {
        self.0.snapshot_contexts()
    }
}

// A borrower gives some of its own allowance away while it waits for a
// grant. The grant must still be noticed when it lands, though the allowance
// it leaves is no higher than before, and the withdrawal goes through
// instead of waiting out the whole borrowing time.
#[tokio::test(flavor = "multi_thread")]
async fn a_grant_is_noticed_though_allowance_was_given_away_meanwhile() {
    let network = Arc::new(MemTransport::new());
    let delay = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut cfg_one = NodeConfig::new(ReplicaId(1), "one");
    cfg_one.peers = vec![PeerAddr::new("two")];
    cfg_one.anti_entropy_interval = Duration::from_millis(30);
    cfg_one.fsync = orvu_runtime::FsyncPolicy::Always;
    let mut cfg_two = NodeConfig::new(ReplicaId(2), "two");
    cfg_two.anti_entropy_interval = Duration::from_millis(30);
    cfg_two.borrow_timeout = Duration::from_secs(4);
    let one = Client::start_with_store(
        network.endpoint(1),
        cfg_one,
        Box::new(Slow(orvu_store::MemStore::new(), delay.clone())),
    )
    .await
    .unwrap();
    let two = Client::start(network.endpoint(2), cfg_two).await.unwrap();
    let lender = one.bounded_counter("stock").await.unwrap();
    let spender = two.bounded_counter("stock").await.unwrap();
    lender.increment(1000).await.unwrap();
    lender.transfer(ReplicaId(2), 3).await.unwrap();
    for _ in 0..200 {
        let ready = spender.local_allowance().await.unwrap() == 3 && two.node.metrics().peers_up > 0;
        if ready && spender.value().await.unwrap() == 1000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(spender.local_allowance().await.unwrap(), 3);

    // The lender now takes a while to save, and so to send, each grant.
    delay.store(400, std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    let borrower = spender.clone();
    let borrowing = tokio::spawn(async move { borrower.try_decrement(10).await });
    // It asked for 7. Before that arrives, the 3 it had go to another machine.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!borrowing.is_finished());
    spender.transfer(ReplicaId(9), 3).await.unwrap();
    let result = borrowing.await.unwrap();
    let took = started.elapsed();
    assert!(result.is_ok(), "{result:?} after {took:?}");
    assert!(took < Duration::from_secs(3), "the borrower waited out its time: {took:?}");
    delay.store(0, std::sync::atomic::Ordering::SeqCst);
    one.shutdown().await;
    two.shutdown().await;
}

// Machine 2 has a little allowance of its own. One withdrawal there needs
// more and borrows from machine 1, but the link is cut, so the grant never
// comes. A small withdrawal its own allowance covers must not wait behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_covered_withdrawal_does_not_wait_behind_one_that_borrows() {
    let network = Arc::new(MemTransport::new());
    let cuts = orvu_transport::Cuts::new();
    let mut cfg_one = NodeConfig::new(ReplicaId(1), "one");
    cfg_one.peers = vec![PeerAddr::new("two")];
    cfg_one.anti_entropy_interval = Duration::from_millis(30);
    let mut cfg_two = NodeConfig::new(ReplicaId(2), "two");
    cfg_two.anti_entropy_interval = Duration::from_millis(30);
    cfg_two.borrow_timeout = Duration::from_secs(2);
    let wrap = |id| orvu_transport::FaultTransport::wrap(network.endpoint(id), &cuts, id);
    let one = Client::start(wrap(1), cfg_one).await.unwrap();
    let two = Client::start(wrap(2), cfg_two).await.unwrap();
    let lender = one.bounded_counter("stock").await.unwrap();
    let spender = two.bounded_counter("stock").await.unwrap();
    lender.increment(1000).await.unwrap();
    lender.transfer(ReplicaId(2), 10).await.unwrap();
    for _ in 0..200 {
        let ready = spender.local_allowance().await.unwrap() == 10 && two.node.metrics().peers_up > 0;
        if ready && spender.value().await.unwrap() == 1000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(spender.local_allowance().await.unwrap(), 10);
    cuts.cut(1, 2);

    let big = spender.clone();
    let borrowing = tokio::spawn(async move { big.try_decrement(100).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!borrowing.is_finished(), "the big withdrawal should still be waiting for its grant");

    let started = std::time::Instant::now();
    spender.try_decrement(1).await.expect("the allowance covers it");
    let waited = started.elapsed();
    assert!(waited < Duration::from_millis(200), "waited {waited:?} behind the borrower");

    let refused = borrowing.await.unwrap();
    assert!(matches!(refused, Err(SdkError::Exhausted)), "got {refused:?}");
    assert!(cuts.poisoned_count() > 0, "the cut touched nothing");
    assert_eq!(spender.local_allowance().await.unwrap(), 9);
    one.shutdown().await;
    two.shutdown().await;
}

// Borrowing must not run the membership check. That check refreshes this
// machine's own heartbeat and drops members that have gone quiet, and it is
// meant for occasional use. Here machine 1 has gone, and machine 2, with no
// connection left, has nothing else that would run the check.
#[tokio::test(flavor = "multi_thread")]
async fn borrowing_does_not_run_the_membership_check() {
    let quick = Duration::from_millis(20);
    let (one, two) = lender_and_spender(|cfg| {
        cfg.suspect_timeout = quick;
        cfg.dead_timeout = quick;
    })
    .await;
    let lender = one.bounded_counter("stock").await.unwrap();
    let spender = two.bounded_counter("stock").await.unwrap();
    lender.increment(10).await.unwrap();
    for _ in 0..200 {
        if spender.value().await.unwrap() == 10 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    one.shutdown().await;
    for _ in 0..200 {
        if two.node.metrics().peers_up == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(quick * 5).await;

    let evicted_before = two.node.metrics().members_evicted;
    let refused = spender.try_decrement(3).await;
    assert!(matches!(refused, Err(SdkError::Exhausted)), "got {refused:?}");
    assert_eq!(two.node.metrics().members_evicted, evicted_before, "borrowing ran the membership check");
    two.shutdown().await;
}

// An empty mark value is a mistake in the call, not a failure of the engine,
// and the error says so.
#[tokio::test]
async fn formatting_with_an_empty_value_has_its_own_error() {
    let client = solo().await;
    let text = client.rich_text("doc").await.unwrap();
    text.insert(0, "hello").await.unwrap();
    let err = text.format(0, 2, "bold", "").await.expect_err("an empty value was taken");
    assert!(matches!(err, SdkError::EmptyValue), "got {err:?}");
    assert!(err.to_string().contains("unformat"), "{err}");
    client.shutdown().await;
}

// Dropping what `on_change` returns stops the callback, while the client
// goes on running.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_change_follower_stops_the_callback() {
    let client = solo().await;
    let counter = client.counter("visits").await.unwrap();
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = seen.clone();
    let follower = client.on_change(move |_| {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    counter.increment(1).await.unwrap();
    for _ in 0..100 {
        if seen.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);

    drop(follower);
    tokio::time::sleep(Duration::from_millis(20)).await;
    counter.increment(1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1, "the callback ran after it was dropped");
    client.shutdown().await;
}

// A storage file that cannot be opened at start is a storage problem, and
// must not be reported as a network one.
#[tokio::test]
async fn a_store_that_cannot_be_opened_is_a_storage_error() {
    let dir = temp_folder();
    let mut cfg = NodeConfig::new(ReplicaId(1), "bad-store");
    // A folder where the storage file should be.
    cfg.store_path = Some(dir.path().to_path_buf());
    let network = Arc::new(MemTransport::new());
    let err = Client::start(network.endpoint(1), cfg).await.err().expect("a folder was opened as a store");
    assert!(matches!(err, SdkError::Storage(_)), "got {err:?}");
}
