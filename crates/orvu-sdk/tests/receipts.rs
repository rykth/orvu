//! A change made under a request id leaves a receipt, saved with the change,
//! so that a program that sends the request again can be told it was done.

use orvu_crdt::ReplicaId;
use orvu_runtime::{FsyncPolicy, NodeConfig};
use orvu_sdk::Client;
use orvu_store::{MemStore, Store};
use orvu_transport::mem::MemTransport;
use std::sync::Arc;

fn config() -> NodeConfig {
    let mut cfg = NodeConfig::new(ReplicaId(1), "one");
    cfg.fsync = FsyncPolicy::Always;
    cfg
}

async fn start(store: &MemStore) -> Client {
    let network = Arc::new(MemTransport::new());
    Client::start_with_store(network.endpoint(1), config(), Box::new(store.clone())).await.unwrap()
}

#[tokio::test]
async fn a_change_made_under_a_request_id_is_known_after_a_restart() {
    let store = MemStore::new();
    let client = start(&store).await;
    let visits = client.counter("visits").await.unwrap();
    orvu_sdk::with_receipt([9; 16], visits.increment(1)).await.unwrap();
    // A change made without one leaves none.
    visits.increment(1).await.unwrap();
    // A change that was refused was not made, and leaves none either.
    let budget = client.bounded_counter("budget").await.unwrap();
    assert!(orvu_sdk::with_receipt([8; 16], budget.try_decrement(1)).await.is_err());
    let ids: Vec<[u8; 16]> = store.receipts().unwrap().iter().map(|receipt| receipt.id).collect();
    assert_eq!(ids, vec![[9; 16]]);
    // The node knows at once which were made, without a restart.
    assert!(client.receipt_made(&[9; 16]));
    assert!(!client.receipt_made(&[8; 16]));
    client.shutdown().await;

    let again = start(&store.reopened()).await;
    assert!(again.receipt_made(&[9; 16]));
    assert!(!again.receipt_made(&[8; 16]));
    assert_eq!(again.counter("visits").await.unwrap().value().await.unwrap(), 2);
    again.shutdown().await;
}
