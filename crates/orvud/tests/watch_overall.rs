//! The most watches a sidecar has open is one count for the whole process,
//! over the Unix socket and the network alike, which each have a service of
//! their own. This is a test binary of its own because that count is shared
//! by every service in the process, and other tests would add to it.

use futures::StreamExt;
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use std::sync::Arc;
use std::time::Duration;

async fn open_on(dp: &orvud::server::Dataplane) -> String {
    dp.open_for_test(pb::OpenRequest { namespace: String::new(), resume_token: String::new(), credential: Vec::new() })
        .await
        .expect("open")
        .session
}

fn watch_req(s: &str) -> pb::WatchRequest {
    pb::WatchRequest { session: s.into(), object: "visits".into(), kind: pb::Kind::Counter as i32 }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_and_the_network_share_one_limit_on_watches() {
    let fabric = Arc::new(MemTransport::new());
    let client = Arc::new(Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1")).await.unwrap());
    // Two services on one engine, as the sidecar has for the socket and the
    // network.
    let local = orvud::server::Dataplane::new(client.clone(), ReplicaId(1), "default");
    let network = orvud::server::Dataplane::new(client, ReplicaId(1), "default");
    let per_session = orvud::watch::MAX_WATCHES_PER_SESSION;
    let sessions = orvud::watch::MAX_WATCHES / per_session;
    let mut open = Vec::new();
    // Half the watches the sidecar may have are opened on each.
    for i in 0..sessions {
        let dp = if i % 2 == 0 { &local } else { &network };
        let s = open_on(dp).await;
        for _ in 0..per_session {
            open.push(dp.watch_for_test(watch_req(&s)).await.expect("a watch within the limit"));
        }
    }
    for dp in [&local, &network] {
        let s = open_on(dp).await;
        match dp.watch_for_test(watch_req(&s)).await {
            Err(status) => assert_eq!(status.code(), tonic::Code::ResourceExhausted, "{status:?}"),
            Ok(_) => panic!("more watches than the sidecar may have were opened"),
        }
    }
    // One that ends makes room, on either.
    let (stream, task) = open.pop().unwrap();
    drop(stream);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while task.upgrade().is_some() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let s = open_on(&local).await;
    let (mut again, _) = local.watch_for_test(watch_req(&s)).await.expect("room once one ended");
    // A watch on an object that is not there yet sends nothing, so the
    // stream has nothing to say.
    assert!(tokio::time::timeout(Duration::from_millis(100), again.next()).await.is_err());
}
