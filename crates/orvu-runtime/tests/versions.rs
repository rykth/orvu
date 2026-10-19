//! Two machines that speak different protocol versions must not sync. A
//! newer version can mean the same bytes say something else, so the greeting
//! compares the numbers, refuses the other machine with a `Nack` that says
//! why, and both sides log which versions met.

use orvu_core::{Codec, NackReason, ProtobufCodec, SyncMessage, TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_proto::v1::NackReason as Reason;
use orvu_proto::PROTO_VERSION;
use orvu_runtime::config::Acl;
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::frame::{read_frame, write_frame};
use orvu_transport::{MemTransport, PeerAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;

/// How long one test may take before it fails as stuck. Far more than any
/// of them needs, even on a busy machine.
const TEST_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Every log line written while the tests run, as text with its fields.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

struct Text(String);
impl Visit for Text {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={:?}", field.name(), value));
    }
}

impl<S: tracing::Subscriber> Layer<S> for Lines {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut text = Text(String::new());
        event.record(&mut text);
        self.0.lock().unwrap().push(text.0);
    }
}

fn lines() -> &'static Lines {
    static LINES: OnceLock<Lines> = OnceLock::new();
    LINES.get_or_init(|| {
        let lines = Lines::default();
        let _ = tracing_subscriber::registry().with(lines.clone()).try_init();
        lines
    })
}

/// Whether some log line holds every one of `parts`.
fn logged(parts: &[&str]) -> bool {
    lines().0.lock().unwrap().iter().any(|l| parts.iter().all(|p| l.contains(p)))
}

async fn count(node: &NodeHandle, name: &str) -> Option<i64> {
    match node.snapshot(name).await {
        Some(TypedCrdt::PnCounter(c)) => Some(c.value()),
        _ => None,
    }
}

async fn bump(node: &NodeHandle, name: &str) {
    node.create(name, TypedCrdt::PnCounter(PnCounter::default())).await;
    node.mutate(name, |data, _time, me| match data {
        TypedCrdt::PnCounter(c) => TypedDelta::PnCounter(c.increment(me, 1)),
        _ => unreachable!(),
    })
    .await;
}

// Machine 3 greets the node with a version one higher than its own. The node
// answers with a Nack that names the reason and both numbers, and nothing
// more.
#[tokio::test(flavor = "multi_thread")]
async fn a_greeting_with_another_version_is_answered_with_a_nack() {
    orvu_test_support::within("a greeting with another version is answered with a nack", TEST_LIMIT, async {
        lines();
        let network = Arc::new(MemTransport::new());
        let node = Node::spawn(network.endpoint(1), NodeConfig::new(ReplicaId(1), "a")).await.unwrap();

        let conn = network.endpoint(3).dial(PeerAddr::new("a"), CancellationToken::new()).await.unwrap();
        let (mut write, mut read) = conn.open_stream().await.unwrap();
        let hello = SyncMessage::Hello {
            proto_version: PROTO_VERSION + 1,
            replica: ReplicaId(3),
            namespaces: vec!["default".into()],
        };
        write_frame(&mut write, &ProtobufCodec.encode(&hello)).await.unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut read))
            .await
            .expect("the node did not answer")
            .expect("the node closed the stream without a word");
        match ProtobufCodec.decode(&answer).unwrap() {
            SyncMessage::Nack { reason, detail, .. } => {
                assert_eq!(reason, NackReason::Known(Reason::ProtoVersion));
                assert!(detail.contains(&PROTO_VERSION.to_string()), "{detail}");
                assert!(detail.contains(&(PROTO_VERSION + 1).to_string()), "{detail}");
            }
            other => panic!("expected a Nack, got {other:?}"),
        }
        let after = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut read)).await;
        assert!(matches!(after, Ok(Err(_))), "the node kept the stream open after refusing: {after:?}");
        node.shutdown().await;
    })
    .await
}

// The node dials a machine that does not check versions and answers with
// its own. The node is the one that refuses, and says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_reply_with_another_version_is_refused_by_the_dialing_node() {
    orvu_test_support::within("a reply with another version is refused by the dialing node", TEST_LIMIT, async {
        lines();
        let network = Arc::new(MemTransport::new());
        let listener = network
            .endpoint(4)
            .listen("old", CancellationToken::new())
            .await
            .unwrap();
        let mut cfg = NodeConfig::new(ReplicaId(1), "a");
        cfg.peers = vec![PeerAddr::new("old")];
        let node = Node::spawn(network.endpoint(1), cfg).await.unwrap();

        let conn = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
        let (mut write, mut read) = conn.accept_stream().await.unwrap();
        let hello = ProtobufCodec.decode(&read_frame(&mut read).await.unwrap()).unwrap();
        assert!(matches!(hello, SyncMessage::Hello { proto_version, .. } if proto_version == PROTO_VERSION));
        let ack = SyncMessage::HelloAck {
            proto_version: PROTO_VERSION + 7,
            replica: ReplicaId(4),
            accepted: vec!["default".into()],
        };
        write_frame(&mut write, &ProtobufCodec.encode(&ack)).await.unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut read))
            .await
            .expect("the node did not answer")
            .expect("the node closed the stream without a word");
        assert!(
            matches!(
                ProtobufCodec.decode(&answer).unwrap(),
                SyncMessage::Nack { reason: NackReason::Known(Reason::ProtoVersion), .. }
            ),
            "the node did not refuse the other version"
        );
        node.shutdown().await;
    })
    .await
}

// Two real nodes, one set to a newer version. They do not sync, and each
// one's log says which two versions met. A third node on the same version as
// the first syncs with it as usual, so the refusal is about the version and
// nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_on_different_versions_do_not_sync_and_both_say_why() {
    orvu_test_support::within("two nodes on different versions do not sync and both say why", TEST_LIMIT, async {
        lines();
        let network = Arc::new(MemTransport::new());
        let newer = PROTO_VERSION + 1;

        let a = Node::spawn(network.endpoint(11), NodeConfig::new(ReplicaId(11), "v-a")).await.unwrap();
        let mut b_cfg = NodeConfig::new(ReplicaId(12), "v-b");
        b_cfg.peers = vec![PeerAddr::new("v-a")];
        b_cfg.proto_version = newer;
        let b = Node::spawn(network.endpoint(12), b_cfg).await.unwrap();
        let mut c_cfg = NodeConfig::new(ReplicaId(13), "v-c");
        c_cfg.peers = vec![PeerAddr::new("v-a")];
        let c = Node::spawn(network.endpoint(13), c_cfg).await.unwrap();

        bump(&b, "from-b").await;
        bump(&c, "from-c").await;
        for _ in 0..100 {
            if count(&a, "from-c").await == Some(1) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert_eq!(count(&a, "from-c").await, Some(1), "a node on the same version did not sync");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(count(&a, "from-b").await, None, "a node on another version synced");
        assert!(!a.known_members().await.contains(&ReplicaId(12)), "the newer node was taken as a member");

        let ours = format!("ours={PROTO_VERSION}");
        let theirs = format!("theirs={newer}");
        assert!(logged(&["me=11", &ours, &theirs]), "the accepting node did not log both versions");
        let ours = format!("ours={newer}");
        assert!(
            logged(&["me=12", &ours, &format!("version {PROTO_VERSION}")]),
            "the dialing node did not log both versions"
        );
        a.shutdown().await;
        b.shutdown().await;
        c.shutdown().await;
    })
    .await
}

// The hand-made handshake in the other tests uses the version this build
// speaks.
#[tokio::test(flavor = "multi_thread")]
async fn the_plain_handshake_speaks_this_builds_version() {
    orvu_test_support::within("the plain handshake speaks this builds version", TEST_LIMIT, async {
        let network = Arc::new(MemTransport::new());
        let node = Node::spawn(network.endpoint(1), NodeConfig::new(ReplicaId(1), "a")).await.unwrap();
        let conn = network.endpoint(3).dial(PeerAddr::new("a"), CancellationToken::new()).await.unwrap();
        let done = orvu_runtime::peer::handshake(conn.as_ref(), ReplicaId(3), "default", true, &Acl::open()).await;
        assert!(done.is_some());
        node.shutdown().await;
    })
    .await
}

// Over real QUIC the refusing node must not drop the connection before its
// Nack is on the way: closing a QUIC connection throws away what is still
// waiting to be sent.
#[tokio::test(flavor = "multi_thread")]
async fn the_nack_reaches_the_other_machine_over_quic() {
    orvu_test_support::within("the nack reaches the other machine over quic", TEST_LIMIT, async {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = orvu_transport::generate_dev_ca();
        let node_cert = orvu_transport::issue_node_cert(&ca, 1);
        let other_cert = orvu_transport::issue_node_cert(&ca, 3);
        let node_net = Arc::new(
            orvu_transport::QuicTransport::new(&node_cert.cert_pem, &node_cert.key_pem, &ca.cert_pem).unwrap(),
        );
        let other_net =
            orvu_transport::QuicTransport::new(&other_cert.cert_pem, &other_cert.key_pem, &ca.cert_pem).unwrap();
        let addr = &format!("127.0.0.1:{}", orvu_test_support::free_port());
        let node = Node::spawn(node_net, NodeConfig::new(ReplicaId(1), addr.as_str())).await.unwrap();

        use orvu_transport::Transport;
        let conn = other_net.dial(PeerAddr::new(addr), CancellationToken::new()).await.unwrap();
        let (mut write, mut read) = conn.open_stream().await.unwrap();
        let hello = SyncMessage::Hello {
            proto_version: PROTO_VERSION + 1,
            replica: ReplicaId(3),
            namespaces: vec!["default".into()],
        };
        write_frame(&mut write, &ProtobufCodec.encode(&hello)).await.unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut read))
            .await
            .expect("the node did not answer")
            .expect("the connection ended before the Nack arrived");
        assert!(matches!(
            ProtobufCodec.decode(&answer).unwrap(),
            SyncMessage::Nack { reason: NackReason::Known(Reason::ProtoVersion), .. }
        ));
        node.shutdown().await;
    })
    .await
}
