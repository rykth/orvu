//! What a client can and cannot make the sidecar do: reads that must not
//! create anything, batches that are too big, a batch whose caller gives up,
//! and a request with a field left out.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use orvud::pb::request::Op;
use std::sync::Arc;
use std::time::Duration;

async fn opened() -> (Arc<orvud::server::Dataplane>, String) {
    let fabric = Arc::new(MemTransport::new());
    let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    let client = Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine"));
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default");
    let session = dp
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open")
        .session;
    (Arc::new(dp), session)
}

async fn exec(dp: &orvud::server::Dataplane, session: &str, op: Op) -> pb::Response {
    dp.execute_for_test(pb::Request { session: session.into(), op: Some(op), request_id: Vec::new() })
        .await
        .expect("the request itself is fine")
}

fn is_ack(r: &pb::Response) -> bool {
    matches!(r.result, Some(pb::response::Result::Ack(_)))
}

fn is_absent(r: &pb::Response) -> bool {
    matches!(
        r.result,
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) }))
    )
}

fn int_of(r: pb::Response) -> i64 {
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => i,
        other => panic!("expected a number, got {other:?}"),
    }
}

fn failed_code(r: &pb::Response) -> pb::Code {
    match &r.result {
        Some(pb::response::Result::Failed(f)) => f.code(),
        other => panic!("expected Failed, got {other:?}"),
    }
}

fn add(object: &str, n: u64) -> Op {
    Op::CounterIncrement(pb::CounterIncrement { object: object.into(), n })
}

fn value(object: &str) -> Op {
    Op::CounterValue(pb::CounterValue { object: object.into() })
}

fn cart_add(object: &str) -> Op {
    Op::CartAdd(pb::CartAdd { object: object.into(), item: "apple".into(), qty: 1 })
}

// Reading an object nobody has written answers "not there" and creates
// nothing. If the read made an empty counter, the name could never be used for
// a cart afterwards.
#[tokio::test]
async fn reading_a_missing_object_says_absent_and_creates_nothing() {
    let (dp, s) = opened().await;
    assert!(is_absent(&exec(&dp, &s, value("basket")).await));
    assert!(is_ack(&exec(&dp, &s, cart_add("basket")).await), "the read took the name");

    // Once the object exists, a read gives its value as before.
    assert!(is_ack(&exec(&dp, &s, add("visits", 5)).await));
    assert_eq!(int_of(exec(&dp, &s, value("visits")).await), 5);
}

// The same for a watch: it waits for the object and does not create it.
#[tokio::test]
async fn watching_a_missing_object_creates_nothing_and_sees_it_arrive() {
    use futures::StreamExt;
    let (dp, s) = opened().await;

    let watch = |object: &str| pb::WatchRequest {
        session: s.clone(),
        object: object.into(),
        kind: pb::Kind::Counter as i32,
    };
    let (_unused, _) = dp.watch_for_test(watch("basket")).await.expect("the watch starts");
    assert!(is_ack(&exec(&dp, &s, cart_add("basket")).await), "the watch took the name");

    let (mut stream, _) = dp.watch_for_test(watch("visits")).await.expect("the watch starts");
    assert!(is_ack(&exec(&dp, &s, add("visits", 5)).await));
    // The counter is made and then added to, so the watch may see 0 before 5.
    let mut seen = Vec::new();
    while seen.last() != Some(&5) {
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("the watch never saw the object arrive")
            .expect("the stream ended")
            .expect("no status");
        match next.event {
            Some(pb::watch_event::Event::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(n)) })) => {
                seen.push(n)
            }
            other => panic!("expected a number, got {other:?}"),
        }
    }
    assert!(seen == [5] || seen == [0, 5], "saw {seen:?}");
}

#[tokio::test]
async fn a_batch_with_too_many_operations_is_rejected_and_applies_nothing() {
    let (dp, s) = opened().await;
    let requests = (0..orvud::exec::MAX_BATCH_OPS + 1)
        .map(|_| pb::Request { session: String::new(), op: Some(add("visits", 1)), request_id: Vec::new() })
        .collect();
    let err = dp
        .batch_for_test(pb::BatchRequest { session: s.clone(), requests, request_id: Vec::new() })
        .await
        .expect_err("the batch is too long");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(is_absent(&exec(&dp, &s, value("visits")).await), "part of the batch was applied");
}

// Replies are collected in memory before they are sent. Once they add up to
// more than the limit, the reads that are left are not run.
#[tokio::test]
async fn a_batch_stops_reading_once_its_replies_are_too_large() {
    let (dp, s) = opened().await;
    let big = "x".repeat(orvud::exec::MAX_BATCH_REPLY_BYTES / 3);
    let set = Op::RegisterSet(pb::RegisterSet { object: "blob".into(), value: big });
    assert!(is_ack(&exec(&dp, &s, set).await));

    let get = || pb::Request {
        session: String::new(),
        op: Some(Op::RegisterGet(pb::RegisterGet { object: "blob".into() })),
        request_id: Vec::new(),
    };
    let replies = dp
        .batch_for_test(pb::BatchRequest { session: s.clone(), requests: (0..10).map(|_| get()).collect(), request_id: Vec::new() })
        .await
        .expect("the batch itself is fine")
        .responses;

    assert_eq!(replies.len(), 10, "every operation still gets a reply");
    let too_large = replies
        .iter()
        .filter(|r| matches!(&r.result, Some(pb::response::Result::Failed(f)) if f.code() == pb::Code::TooLarge))
        .count();
    // Each reply is a little over a third of the limit, so three fit.
    assert_eq!(too_large, 7, "three reads fit, the other seven were not run");
    assert_eq!(failed_code(&replies[9]), pb::Code::TooLarge);
}

// The caller gives up almost at once. The batch must still run to the end, so
// that it is never left half applied.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_finishes_even_if_the_caller_gives_up() {
    let (dp, s) = opened().await;
    let requests = (0..1000)
        .map(|_| pb::Request { session: String::new(), op: Some(add("visits", 1)), request_id: Vec::new() })
        .collect();
    let batch = dp.batch_for_test(pb::BatchRequest { session: s.clone(), requests, request_id: Vec::new() });
    let gave_up = tokio::time::timeout(Duration::from_micros(50), batch).await;
    assert!(gave_up.is_err(), "the batch finished before the caller could give up");

    let mut total = 0;
    for _ in 0..200 {
        total = match exec(&dp, &s, value("visits")).await.result {
            Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => i,
            _ => 0,
        };
        if total == 1000 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(total, 1000, "the batch was left half applied");
}

// proto3 cannot tell a field that was left out from an empty one. A format
// request with no value is a mistake in the request, and must not be reported
// as the engine being down.
#[tokio::test]
async fn formatting_with_no_value_is_a_bad_request() {
    let (dp, s) = opened().await;
    let format = Op::RichtextFormat(pb::RichtextFormat {
        object: "doc".into(),
        index: 0,
        len: 1,
        attr: "bold".into(),
        value: String::new(),
    });
    let err = dp
        .execute_for_test(pb::Request { session: s, op: Some(format), request_id: Vec::new() })
        .await
        .expect_err("a format with no value is a bad request");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// A gRPC client connected to the socket at `path`, that sends requests of
/// any size.
async fn connect(path: &std::path::Path) -> pb::data_plane_client::DataPlaneClient<tonic::transport::Channel> {
    let path = path.to_path_buf();
    let channel = tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .expect("connects");
    pb::data_plane_client::DataPlaneClient::new(channel).max_encoding_message_size(usize::MAX)
}

/// A read whose request is exactly `size` bytes.
fn request_of(session: &str, size: usize) -> pb::Request {
    use prost::Message;
    let with = |member: String| pb::Request {
        session: session.into(),
        op: Some(Op::SetContains(pb::SetContains { object: "s".into(), member })),
        request_id: Vec::new(),
    };
    // The lengths written in front of the text grow with it, so it is cut to
    // fit.
    let mut member = "x".repeat(size);
    while with(member.clone()).encoded_len() > size {
        member.truncate(member.len() - 1);
    }
    let req = with(member);
    assert_eq!(req.encoded_len(), size, "cannot make a request of exactly {size} bytes");
    req
}

// A request up to 64 MiB is taken, and one byte more is not.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_up_to_the_size_limit_is_taken() {
    let folder = tempfile::tempdir().unwrap();
    let socket = folder.path().join("orvud.sock");
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    let bound = orvud::server::bind_uds(&socket).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        orvud::server::serve_bound(bound, client, ReplicaId(1), "default", async {
            let _ = stopped.await;
        })
        .await
    });
    let mut grpc = connect(&socket).await;
    let open = pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() };
    let session = grpc.open(open).await.unwrap().into_inner().session;
    assert_eq!(orvud::exec::MAX_REQUEST_BYTES, 64 * 1024 * 1024);

    let largest = request_of(&session, orvud::exec::MAX_REQUEST_BYTES);
    let answer = grpc.execute(largest).await.expect("the largest request is taken").into_inner();
    assert!(matches!(answer.result, Some(pb::response::Result::Value(_))), "{answer:?}");

    let over = request_of(&session, orvud::exec::MAX_REQUEST_BYTES + 1);
    let status = grpc.execute(over).await.expect_err("a request over the limit was taken");
    assert_eq!(status.code(), tonic::Code::OutOfRange, "{status:?}");

    drop(grpc);
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

/// A gRPC client of the network service at `address`, trusting the
/// certificate in `folder`, that sends requests of any size.
async fn connect_network(
    address: std::net::SocketAddr,
    folder: &std::path::Path,
) -> pb::data_plane_client::DataPlaneClient<tonic::transport::Channel> {
    use rustls::pki_types::pem::PemObject;
    let (cert, _) = orvud::network::certificate(folder).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(cert.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config));
    let channel = tonic::transport::Endpoint::from_static("http://orvud")
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let tls = tls.clone();
            async move {
                let stream = tokio::net::TcpStream::connect(address).await?;
                let name = rustls::pki_types::ServerName::try_from(orvud::network::NAME).unwrap();
                let stream = tls.connect(name, stream).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .expect("connects");
    pb::data_plane_client::DataPlaneClient::new(channel).max_encoding_message_size(usize::MAX)
}

// Over the network, where anyone who can reach the port can send, a request
// is read in full before its key is checked. So the network takes requests
// of only up to 4 MiB, gRPC's usual limit; the 64 MiB limit is for the Unix
// socket alone.
#[tokio::test(flavor = "multi_thread")]
async fn the_network_takes_requests_of_only_up_to_4_mib() {
    let folder = tempfile::tempdir().unwrap();
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    let acceptor = orvud::network::acceptor(folder.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let agents = Arc::new(orvud::agents::Agents::from_file(folder.path().join(orvud::agents::FILE)));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        orvud::server::serve_network(
            listener,
            acceptor,
            client,
            ReplicaId(1),
            "default",
            orvud::server::Connections::default(),
            Arc::default(),
            agents,
            Arc::new(orvud::audit::Audit::off()),
            Arc::new(orvud::search::Searcher::off()),
            async {
                let _ = stopped.await;
            },
        )
        .await
    });
    assert_eq!(orvud::server::MAX_NETWORK_REQUEST_BYTES, 4 * 1024 * 1024);
    let mut grpc = connect_network(address, folder.path()).await;

    // Nobody has a session here, so a request that is read is refused for
    // that, and one that is too large is refused before.
    let largest = request_of("no-such-session", orvud::server::MAX_NETWORK_REQUEST_BYTES);
    let status = grpc.execute(largest).await.expect_err("there is no session");
    assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");

    let over = request_of("no-such-session", orvud::server::MAX_NETWORK_REQUEST_BYTES + 1);
    let status = grpc.execute(over).await.expect_err("a request over the limit was taken");
    assert_eq!(status.code(), tonic::Code::OutOfRange, "{status:?}");

    drop(grpc);
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

// One connection can carry many watches at once. What is limited on each
// connection is how many calls are being read and answered at the same
// moment, not how many watches are open, which stay open long after their
// call was answered.
#[tokio::test(flavor = "multi_thread")]
async fn one_connection_carries_more_watches_than_calls_at_once() {
    let folder = tempfile::tempdir().unwrap();
    let socket = folder.path().join("orvud.sock");
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    let bound = orvud::server::bind_uds(&socket).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        orvud::server::serve_bound(bound, client, ReplicaId(1), "default", async {
            let _ = stopped.await;
        })
        .await
    });
    let grpc = connect(&socket).await;
    let open = pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() };
    let session = grpc.clone().open(open).await.unwrap().into_inner().session;
    let made = pb::Request { session: session.clone(), op: Some(add("visits", 1)), request_id: Vec::new() };
    grpc.clone().execute(made).await.unwrap();
    let many = orvud::server::MAX_CALLS_PER_CONNECTION * 3;
    let mut streams = Vec::new();
    for _ in 0..many {
        let req = pb::WatchRequest { session: session.clone(), object: "visits".into(), kind: pb::Kind::Counter as i32 };
        let mut stream = tokio::time::timeout(Duration::from_secs(5), grpc.clone().watch(req))
            .await
            .expect("a watch past the calls limit waited")
            .expect("watch opens")
            .into_inner();
        // Each sees the current value, so each is really open.
        tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("no value arrived")
            .expect("live")
            .expect("a value");
        streams.push(stream);
    }
    // And a plain call still goes through beside them.
    let read = pb::Request { session: session.clone(), op: Some(value("visits")), request_id: Vec::new() };
    tokio::time::timeout(Duration::from_secs(5), grpc.clone().execute(read)).await.expect("a call waited").unwrap();

    drop((streams, grpc));
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}

// One connection has only so many calls being read and answered at once. A
// call whose request has not finished arriving holds its place, and the next
// call on that connection waits for one to be given back. Another connection
// is not held up.
#[tokio::test(flavor = "multi_thread")]
async fn one_connection_has_only_so_many_calls_at_once() {
    use prost::Message;
    let folder = tempfile::tempdir().unwrap();
    let socket = folder.path().join("orvud.sock");
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    let bound = orvud::server::bind_uds(&socket).await.unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        orvud::server::serve_bound(bound, client, ReplicaId(1), "default", async {
            let _ = stopped.await;
        })
        .await
    });
    let open = pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() };
    let session = connect(&socket).await.open(open).await.unwrap().into_inner().session;

    // A plain HTTP/2 connection, so that a request can be left unfinished.
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (h2, connection) = h2::client::handshake(stream).await.unwrap();
    tokio::spawn(connection);
    let mut h2 = h2.ready().await.unwrap();
    let call = || {
        http::Request::builder()
            .method("POST")
            .uri("http://localhost/orvu.dataplane.v1.DataPlane/Execute")
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(())
            .unwrap()
    };
    // A whole request: its length, then the message.
    let read = pb::Request { session: session.clone(), op: Some(value("visits")), request_id: Vec::new() }.encode_to_vec();
    let mut whole = vec![0u8];
    whole.extend_from_slice(&(read.len() as u32).to_be_bytes());
    whole.extend_from_slice(&read);

    // Calls that say how long their request is, and never send the rest.
    let mut unfinished = Vec::new();
    for _ in 0..orvud::server::MAX_CALLS_PER_CONNECTION {
        h2 = h2.ready().await.unwrap();
        let (answer, mut body) = h2.send_request(call(), false).unwrap();
        body.send_data(bytes::Bytes::copy_from_slice(&whole[..5]), false).unwrap();
        unfinished.push((answer, body));
    }
    // Each call is taken up by a task of its own, in no set order, so the
    // unfinished ones are given a moment to take their places first.
    tokio::time::sleep(Duration::from_millis(300)).await;
    // One more, whole, waits.
    h2 = h2.ready().await.unwrap();
    let (waiting, mut body) = h2.send_request(call(), false).unwrap();
    body.send_data(bytes::Bytes::from(whole.clone()), true).unwrap();
    let mut waiting = tokio::spawn(waiting);
    if let Ok(early) = tokio::time::timeout(Duration::from_millis(500), &mut waiting).await {
        panic!("a call past the limit was answered at once: {early:?}");
    }
    // Another connection is not held up.
    let other = pb::Request { session: session.clone(), op: Some(value("visits")), request_id: Vec::new() };
    tokio::time::timeout(Duration::from_secs(5), connect(&socket).await.execute(other))
        .await
        .expect("another connection waited")
        .unwrap();
    // Once one of the unfinished calls is finished and answered, the waiting
    // one goes through.
    let (answer, mut body) = unfinished.pop().unwrap();
    body.send_data(bytes::Bytes::copy_from_slice(&whole[5..]), true).unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(5), answer).await.expect("the finished call waited").unwrap();
    assert_eq!(answered.status(), 200);
    let answered = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("the waiting call still waits");
    assert_eq!(answered.unwrap().unwrap().status(), 200);

    drop(unfinished);
    stop.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), serving).await;
}
