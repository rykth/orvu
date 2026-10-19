//! Catch-up on a timer, also called anti-entropy. We tell the other machine
//! which updates we have seen, and it resends the ones we are missing. What we
//! send is a `ContextDigest`, passed to the connection's writer through its
//! control channel. The other machine's reader answers with a `DeltaBatch` read
//! from its delta log (see `reader_task`).

use crate::engine::Command;
use orvu_core::SyncMessage;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

/// Asks the engine which updates it has seen for each object, wraps the answer
/// in one `ContextDigest`, and passes it to the writer through `control`.
/// Returns false if either channel is gone.
pub async fn one_round(
    engine: &mpsc::Sender<Command>,
    control: &mpsc::Sender<SyncMessage>,
    namespace: &str,
    metrics: &std::sync::Arc<crate::metrics::Metrics>,
) -> bool {
    let t0 = std::time::Instant::now();
    let (reply, rx) = oneshot::channel();
    if engine.send(Command::Digest { reply }).await.is_err() {
        return false;
    }
    let Ok(entries) = rx.await else { return false };
    let msg = SyncMessage::ContextDigest {
        namespace: namespace.to_string(),
        entries: entries.into_iter().collect(),
    };
    if control.send(msg).await.is_err() {
        return false;
    }
    metrics.record_antientropy_round();
    metrics.antientropy_round.observe(t0.elapsed().as_secs_f64());

    // Also send our current list of members. That way the other machine hears
    // about members it has no connection to, and can count them when working
    // out which updates everyone has.
    let (mreply, mrx) = oneshot::channel();
    if engine.send(Command::MembershipView { reply: mreply }).await.is_err() {
        return false;
    }
    let Ok(members) = mrx.await else { return false };
    control.send(SyncMessage::MembershipDigest { members }).await.is_ok()
}

/// The catch-up loop for one connection. It runs a round every `period` until
/// cancelled or until a round fails.
pub async fn antientropy_task(
    engine: mpsc::Sender<Command>,
    control: mpsc::Sender<SyncMessage>,
    period: Duration,
    namespace: String,
    cancel: CancellationToken,
    metrics: std::sync::Arc<crate::metrics::Metrics>,
) {
    let mut ticker = interval(period);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                if !one_round(&engine, &control, &namespace, &metrics).await {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine;
    use orvu_core::{TypedCrdt, TypedDelta};
    use orvu_crdt::{PnCounter, ReplicaId};

    #[tokio::test]
    async fn one_round_sends_a_context_digest() {
        let eng = engine::spawn(
            ReplicaId(1),
            64,
            std::time::Duration::from_secs(2),
            64,
            crate::engine::Persistence::default(),
            crate::config::Acl::open(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(24 * 3600),
            std::sync::Arc::new(crate::metrics::Metrics::default()),
        );
        let tx = eng.sender();

        // Create an object and change it, so there is something to report.
        let (done, rx) = oneshot::channel();
        tx.send(Command::Create { object_id: "c".into(), crdt: TypedCrdt::PnCounter(PnCounter::default()), done }).await.unwrap();
        rx.await.unwrap();
        let (done, rx) = oneshot::channel();
        tx.send(Command::Mutate {
            object_id: "c".into(),
            f: Box::new(|crdt, _hlc, _replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(ReplicaId(1), 1)),
                _ => unreachable!(),
            }),
            wall: 1,
            receipt: None,
            done,
        }).await.unwrap();
        rx.await.unwrap().unwrap();

        let (control_tx, mut control_rx) = mpsc::channel::<SyncMessage>(16);
        let metrics = std::sync::Arc::new(crate::metrics::Metrics::default());
        assert!(one_round(&tx, &control_tx, "default", &metrics).await);
        match control_rx.recv().await.unwrap() {
            SyncMessage::ContextDigest { entries, .. } => assert!(entries.contains_key("c")),
            other => panic!("unexpected {other:?}"),
        }
    }
}
