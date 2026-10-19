//! Leaving the team for good.
//!
//! A machine that leaves hands every bit of allowance it holds on a bounded
//! counter to the machines it is connected to, so that none of a budget goes
//! with it. It first adds its number to a set of machines that have left,
//! which the others read before each rebalancing round: they give it no
//! allowance back, and do not count it when they share a budget out.
//!
//! Each hand-over is an ordinary `transfer`, so nothing can be overspent. The
//! machine waits until every machine it handed to has said, in its catch-up
//! digest, that it has everything this one wrote, and only then reports
//! success. The wait is bounded: past it, what is still held is reported.

use crate::node::NodeHandle;
use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{OrSet, ReplicaId};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// Where the machines keep what is theirs alone. The sidecar lets no
/// program read or change anything under it.
pub const MACHINES: &str = "orvu/_machines/";

/// The set of machines that have left for good, by number. Nothing else is
/// kept in it. The name is under `MACHINES`, so no program connected to a
/// sidecar can change it. A machine counts as gone only if the mark says it
/// was put there by that machine itself. That stops a machine that follows
/// the rules from marking another, but not one of the team set on cheating:
/// what is written is passed on by the others, so where a mark came from
/// cannot be checked. The machines of a team are trusted to this extent.
pub const LEFT: &str = "orvu/_machines/left";

/// How often the wait looks again.
const LOOK_EVERY: Duration = Duration::from_millis(20);

/// What leaving did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Leaving {
    /// What was handed over: the counter, the machine it went to, and how
    /// much.
    pub given: Vec<(String, ReplicaId, u64)>,
    /// Allowance this machine still held when it stopped trying, by counter.
    /// Empty when everything was handed over.
    pub kept: BTreeMap<String, u64>,
    /// True if every machine it is connected to is known to have everything
    /// this one wrote, the hand-overs included.
    pub confirmed: bool,
}

impl Leaving {
    /// True if nothing was kept and the others have it all.
    pub fn done(&self) -> bool {
        self.kept.is_empty() && self.confirmed
    }
}

/// Why a machine could not leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaveError {
    /// No other machine was connected within the wait, so there was nobody
    /// to hand anything to. Nothing was changed.
    Alone,
    /// The object that lists the machines that have left is there with
    /// another type, or the node has stopped. Nothing was handed over.
    CannotMark,
}

impl std::fmt::Display for LeaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaveError::Alone => write!(f, "no other machine was connected, so there was nobody to hand anything to"),
            LeaveError::CannotMark => write!(f, "could not note that this machine is leaving, in {LEFT}"),
        }
    }
}

impl std::error::Error for LeaveError {}

/// The machines `set` says have left. A number counts only if the machine
/// with that number put it there itself: each add is tagged with the
/// machine that made it, and a mark tagged with another machine is not
/// believed. The tag is what the writer says, so this holds against
/// programs and honest machines, not against a machine of the team that lies.
/// A member that is not a number is left out.
pub(crate) fn numbers_in(set: &OrSet<String>) -> BTreeSet<ReplicaId> {
    use orvu_crdt::DeltaCrdt;
    let whole = set.snapshot();
    whole
        .elements()
        .filter_map(|(member, dots)| {
            let number = ReplicaId(member.parse::<u64>().ok()?);
            dots.iter().any(|dot| dot.replica == number).then_some(number)
        })
        .collect()
}

/// The machines that have left, as this node knows them.
pub(crate) async fn left(node: &NodeHandle) -> BTreeSet<ReplicaId> {
    match node.snapshot(LEFT).await {
        Some(TypedCrdt::OrSet(set)) => numbers_in(&set),
        _ => BTreeSet::new(),
    }
}

/// How to split `amount` between `n` machines: as evenly as whole numbers
/// allow, the first ones getting one more.
pub(crate) fn shares(amount: u64, n: usize) -> Vec<u64> {
    if n == 0 {
        return Vec::new();
    }
    let n = n as u64;
    (0..n).map(|i| amount / n + u64::from(i < amount % n)).collect()
}

/// See `NodeHandle::leave`.
pub(crate) async fn leave(node: &NodeHandle, wait: Duration, grace: Duration) -> Result<Leaving, LeaveError> {
    let deadline = Instant::now() + wait;
    let me = node.replica_id();
    while node.connected_peers().is_empty() {
        if Instant::now() >= deadline {
            return Err(LeaveError::Alone);
        }
        tokio::time::sleep(LOOK_EVERY).await;
    }

    // Marked first, so that the others stop giving this machine allowance
    // before it starts handing its own over.
    if !node.open(LEFT, TypedCrdt::OrSet(OrSet::default())).await {
        return Err(LeaveError::CannotMark);
    }
    let marked = node
        .try_mutate(LEFT, move |crdt, _hlc, r| match crdt {
            // A mark another machine wrote is not believed, so this one's
            // own goes in beside it.
            TypedCrdt::OrSet(set) if !numbers_in(set).contains(&me) => {
                set.add(me.0.to_string(), r).ok().map(TypedDelta::OrSet)
            }
            _ => None,
        })
        .await;
    if !matches!(marked, crate::TryOutcome::Applied | crate::TryOutcome::Refused) {
        return Err(LeaveError::CannotMark);
    }

    let mut given = Vec::new();
    loop {
        let gone = left(node).await;
        let to: Vec<ReplicaId> = node.connected_peers().into_iter().filter(|p| !gone.contains(p)).collect();
        for object_id in node.object_ids().await {
            let held = match node.snapshot(&object_id).await {
                Some(TypedCrdt::BoundedCounter(counter)) => counter.allowance(me),
                _ => continue,
            };
            for (&peer, amount) in to.iter().zip(shares(held, to.len())) {
                if amount == 0 {
                    continue;
                }
                let handed = node
                    .try_mutate(&object_id, move |crdt, _hlc, r| match crdt {
                        TypedCrdt::BoundedCounter(counter) => {
                            counter.transfer(peer, amount, r).map(TypedDelta::BoundedCounter)
                        }
                        _ => None,
                    })
                    .await;
                // A refusal means the allowance moved meanwhile, such as to
                // a machine that asked to borrow. The next pass sees it.
                if handed == crate::TryOutcome::Applied {
                    given.push((object_id.clone(), peer, amount));
                }
            }
        }
        let confirmed = !to.is_empty() && seen_by_all(node, &to, deadline).await;
        // A machine that planned a transfer to this one just before it saw
        // the mark may still send it. It comes within a moment.
        tokio::time::sleep(grace.min(deadline.saturating_duration_since(Instant::now()))).await;
        let kept = still_held(node, me).await;
        if (kept.is_empty() && confirmed) || Instant::now() >= deadline {
            return Ok(Leaving { given, kept, confirmed });
        }
    }
}

/// The allowance `me` still holds, by counter, leaving out counters where it
/// holds none.
async fn still_held(node: &NodeHandle, me: ReplicaId) -> BTreeMap<String, u64> {
    let mut kept = BTreeMap::new();
    for object_id in node.object_ids().await {
        if let Some(TypedCrdt::BoundedCounter(counter)) = node.snapshot(&object_id).await {
            let held = counter.allowance(me);
            if held > 0 {
                kept.insert(object_id, held);
            }
        }
    }
    kept
}

/// Waits until each of `peers` has said it has every change this node has
/// made, to every object. False if the deadline came first.
async fn seen_by_all(node: &NodeHandle, peers: &[ReplicaId], deadline: Instant) -> bool {
    let me = node.replica_id();
    let own: Vec<(String, u64)> = node
        .own_contexts()
        .await
        .into_iter()
        .map(|(id, ctx)| (id, ctx.version_vector().get(me)))
        .filter(|(_, made)| *made > 0)
        .collect();
    loop {
        let mut all = true;
        for &peer in peers {
            let seen = node.seen_by(peer).await;
            if !own.iter().all(|(id, made)| seen.get(id).is_some_and(|vv| vv.get(me) >= *made)) {
                all = false;
                break;
            }
        }
        if all {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(LOOK_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Node, NodeConfig};
    use orvu_transport::{MemTransport, PeerAddr};
    use std::sync::Arc;

    #[test]
    fn a_share_is_as_even_as_whole_numbers_allow() {
        assert_eq!(shares(10, 3), [4, 3, 3]);
        assert_eq!(shares(2, 3), [1, 1, 0]);
        assert_eq!(shares(9, 1), [9]);
        assert!(shares(9, 0).is_empty());
        assert_eq!(shares(u64::MAX, 2).iter().map(|&s| s as u128).sum::<u128>(), u64::MAX as u128);
    }

    fn config(id: u64, name: &str, peers: &[&str], rebalance: bool) -> NodeConfig {
        let mut cfg = NodeConfig::new(ReplicaId(id), name);
        cfg.peers = peers.iter().map(|p| PeerAddr::new(*p)).collect();
        cfg.anti_entropy_interval = Duration::from_millis(30);
        if rebalance {
            cfg.rebalance_interval = Some(Duration::from_millis(30));
        }
        cfg
    }

    async fn deposit(node: &NodeHandle, object: &str, n: u64) {
        node.open(object, TypedCrdt::BoundedCounter(Default::default())).await;
        node.mutate(object, move |crdt, _h, r| match crdt {
            TypedCrdt::BoundedCounter(c) => TypedDelta::BoundedCounter(c.increment(n, r)),
            other => panic!("unexpected {other:?}"),
        })
        .await;
    }

    async fn allowance(node: &NodeHandle, object: &str, of: u64) -> u64 {
        match node.snapshot(object).await {
            Some(TypedCrdt::BoundedCounter(c)) => c.allowance(ReplicaId(of)),
            _ => 0,
        }
    }

    async fn until(mut check: impl AsyncFnMut() -> bool) -> bool {
        for _ in 0..250 {
            if check().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    // Both machines share budgets out between them all the time. Once A has
    // left, B holds the whole budget, and its own rebalancing gives none of
    // it back to A, which would take it away when it stops.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_machine_that_leaves_hands_everything_over_and_gets_none_back() {
        let fabric = Arc::new(MemTransport::new());
        let a = Node::spawn(fabric.endpoint(1), config(1, "a", &["b"], true)).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), config(2, "b", &["a"], true)).await.unwrap();
        deposit(&a, "tokens", 100).await;
        deposit(&b, "seats", 7).await;
        assert!(until(async || allowance(&b, "tokens", 2).await >= 40).await, "the budget was never shared out");
        assert!(until(async || allowance(&a, "seats", 1).await >= 3).await, "the seats were never shared out");

        let leaving = a.leave(Duration::from_secs(5)).await.expect("leaves");
        assert!(leaving.done(), "{leaving:?}");
        assert!(leaving.given.iter().any(|(id, to, _)| id == "seats" && *to == ReplicaId(2)), "{leaving:?}");
        assert_eq!((allowance(&b, "tokens", 2).await, allowance(&b, "tokens", 1).await), (100, 0));
        assert_eq!((allowance(&b, "seats", 2).await, allowance(&b, "seats", 1).await), (7, 0));

        // Several rounds of rebalancing later, A still holds nothing.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(allowance(&b, "tokens", 1).await, 0, "B gave allowance back to a machine that left");
        assert_eq!(allowance(&a, "tokens", 1).await, 0);
        assert_eq!(left(&b).await, BTreeSet::from([ReplicaId(1)]));
        a.shutdown().await;
        b.shutdown().await;
    }

    // With two machines to hand to, each gets half.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_allowance_is_split_between_the_machines_connected() {
        let fabric = Arc::new(MemTransport::new());
        let a = Node::spawn(fabric.endpoint(1), config(1, "a", &["b", "c"], false)).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), config(2, "b", &["c"], false)).await.unwrap();
        let c = Node::spawn(fabric.endpoint(3), config(3, "c", &[], false)).await.unwrap();
        deposit(&a, "tokens", 91).await;
        assert!(until(async || a.connected_peers().len() == 2).await);

        let leaving = a.leave(Duration::from_secs(5)).await.expect("leaves");
        assert!(leaving.done(), "{leaving:?}");
        assert!(until(async || allowance(&c, "tokens", 3).await == 45).await);
        assert_eq!(allowance(&b, "tokens", 2).await, 46);
        assert_eq!(allowance(&b, "tokens", 1).await, 0);
        for node in [a, b, c] {
            node.shutdown().await;
        }
    }

    // What A sends is lost on the way, so B never says it has the hand-over.
    // A does not take that for success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hand_over_nobody_was_heard_to_have_is_not_reported_done() {
        use orvu_transport::{Cuts, FaultTransport};
        let fabric = Arc::new(MemTransport::new());
        let cuts = Cuts::new();
        let a = Node::spawn(FaultTransport::wrap(fabric.endpoint(1), &cuts, 1), config(1, "a", &["b"], false))
            .await
            .unwrap();
        let b = Node::spawn(FaultTransport::wrap(fabric.endpoint(2), &cuts, 2), config(2, "b", &[], false))
            .await
            .unwrap();
        deposit(&a, "tokens", 10).await;
        assert!(until(async || allowance(&b, "tokens", 1).await == 10).await, "the two never connected");

        cuts.cut_one_way(1, 2);
        let leaving = a.leave(Duration::from_millis(600)).await.expect("starts leaving");
        assert!(cuts.poisoned_count() > 0, "the cut cut nothing");
        assert!(!leaving.confirmed && !leaving.done(), "{leaving:?}");
        assert_eq!(allowance(&b, "tokens", 2).await, 0);
        a.shutdown().await;
        b.shutdown().await;
    }

    // Only a machine can say it has left. B writes A's number into the list,
    // as a machine that is not A, and A goes on being given its share.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_machine_marked_as_gone_by_another_is_not_taken_for_gone() {
        let fabric = Arc::new(MemTransport::new());
        let a = Node::spawn(fabric.endpoint(1), config(1, "a", &["b"], true)).await.unwrap();
        let b = Node::spawn(fabric.endpoint(2), config(2, "b", &["a"], true)).await.unwrap();
        assert!(until(async || a.connected_peers().len() == 1).await, "the two never connected");
        b.open(LEFT, TypedCrdt::OrSet(OrSet::default())).await;
        b.mutate(LEFT, |crdt, _h, r| match crdt {
            TypedCrdt::OrSet(set) => TypedDelta::OrSet(set.add("1".to_string(), r).unwrap()),
            other => panic!("unexpected {other:?}"),
        })
        .await;
        assert!(until(async || matches!(a.snapshot(LEFT).await, Some(TypedCrdt::OrSet(set)) if set.contains(&"1".to_string()))).await);
        assert!(left(&a).await.is_empty() && left(&b).await.is_empty(), "a mark B wrote was believed");

        deposit(&b, "seats", 10).await;
        assert!(until(async || allowance(&a, "seats", 1).await >= 4).await, "B gave A nothing");
        deposit(&a, "tokens", 10).await;
        assert!(until(async || allowance(&b, "tokens", 2).await >= 4).await, "A took itself for gone");

        // When A does leave, its own mark goes in beside the one B wrote.
        assert!(a.leave(Duration::from_secs(5)).await.expect("leaves").done());
        assert_eq!(left(&b).await, BTreeSet::from([ReplicaId(1)]));
        a.shutdown().await;
        b.shutdown().await;
    }

    #[test]
    fn a_number_counts_only_when_its_own_machine_wrote_it() {
        let mut set = OrSet::default();
        set.add("1".to_string(), ReplicaId(1)).unwrap();
        set.add("3".to_string(), ReplicaId(2)).unwrap();
        set.add("not a number".to_string(), ReplicaId(4)).unwrap();
        assert_eq!(numbers_in(&set), BTreeSet::from([ReplicaId(1)]));
        // Written by both: the machine's own write is enough.
        set.add("3".to_string(), ReplicaId(3)).unwrap();
        let mut other = OrSet::default();
        other.add("3".to_string(), ReplicaId(5)).unwrap();
        orvu_crdt::DeltaCrdt::merge(&mut set, orvu_crdt::DeltaCrdt::snapshot(&other));
        assert_eq!(numbers_in(&set), BTreeSet::from([ReplicaId(1), ReplicaId(3)]));
    }

    #[test]
    fn the_list_of_machines_gone_is_under_the_machines_own_names() {
        assert!(LEFT.starts_with(MACHINES));
    }

    // Nobody to hand to: nothing is changed, not even the mark.
    #[tokio::test]
    async fn a_machine_alone_cannot_leave() {
        let fabric = Arc::new(MemTransport::new());
        let a = Node::spawn(fabric.endpoint(1), config(1, "a", &[], false)).await.unwrap();
        deposit(&a, "tokens", 5).await;
        assert_eq!(a.leave(Duration::from_millis(100)).await, Err(LeaveError::Alone));
        assert!(a.snapshot(LEFT).await.is_none(), "it was marked as gone");
        assert_eq!(allowance(&a, "tokens", 1).await, 5);
        a.shutdown().await;
    }
}
