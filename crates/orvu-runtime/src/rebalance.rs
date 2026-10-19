//! Rebalancing for bounded counters, also called the "Escrow Manager". A
//! `BoundedCounter` cannot go below zero, and each machine has an allowance it
//! may spend. One loop per node runs on a timer and hands this machine's spare
//! allowance to the connected machine that needs it most. That way a
//! decrement usually succeeds locally and does not have to borrow first.
//!
//! The counter itself keeps the value at zero or above and prevents
//! overspending. Every move here is a `transfer`, which refuses to give more
//! than the local allowance. This module only decides how well the allowance is
//! spread out.

use crate::engine::Command;
use crate::node::PeerRegistry;
use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{BoundedCounter, ReplicaId};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

/// Works out which transfers this node should make for one bounded counter.
/// It only computes: it does no I/O and reads no clock. `members` is everyone
/// taking part, this node included. `demand` is how much each member spent
/// since the previous round.
///
/// Each member's target is a share of `value` in proportion to its demand, so
/// allowance follows spending. When nothing has been spent (just after startup,
/// or when the system is idle) every demand is zero and the target is an equal
/// share, `value / N`. We give what we hold above our own target to the member
/// that is furthest below its own target. The amount is capped by both our
/// surplus and their shortfall, so neither side goes past its target. Each call
/// plans at most one transfer.
///
/// Demand changes every round, so the targets keep moving and the allowances
/// never settle on a final spread. The only promises are that each round moves
/// a limited amount and that every move is a transfer the counter allows.
pub(crate) fn plan_transfers(
    counter: &BoundedCounter,
    me: ReplicaId,
    members: &[ReplicaId],
    demand: &BTreeMap<ReplicaId, u64>,
) -> Vec<(ReplicaId, u64)> {
    let n = members.len() as u64;
    if n < 2 {
        return Vec::new();
    }
    let value = counter.value();
    if value <= 0 {
        return Vec::new();
    }
    let value = value as u64;

    // Only demand from current members is added up, so an old entry for a
    // replica that has left does not distort the total.
    let total: u128 = members
        .iter()
        .map(|p| demand.get(p).copied().unwrap_or(0) as u128)
        .sum();
    let target = |p: ReplicaId| -> u64 {
        let d = demand.get(&p).copied().unwrap_or(0) as u128;
        // `checked_div` returns `None` only when the total demand is zero, and
        // then we use the equal share. Clippy's `manual_checked_ops` lint
        // wants this form instead of an `if total == 0` check. Both mean the
        // same thing.
        (value as u128 * d).checked_div(total).map_or(value / n, |t| t as u64)
    };

    let my_target = target(me);
    let my_allow = counter.allowance(me);
    if my_allow <= my_target {
        return Vec::new();
    }
    let surplus = my_allow - my_target;

    // Pick the member furthest below its own target. A tie goes to the lowest
    // ReplicaId. With zero demand every target is equal, so this picks the
    // member with the least allowance, and a tie still goes to the lowest id.
    let neediest = members
        .iter()
        .copied()
        .filter(|&p| p != me)
        .filter_map(|p| {
            // Use `checked_sub` here, not `(a < t).then_some((t - a, p))`.
            // `then_some` works out its argument even when the test is false.
            // An idle member holding more than its target of zero would then
            // make `t - a` underflow, which panics in debug builds and stops
            // the rebalance loop. `None` here means the member is not short.
            let t = target(p);
            t.checked_sub(counter.allowance(p)).filter(|&d| d > 0).map(|d| (d, p))
        })
        .max_by_key(|&(deficit, p)| (deficit, std::cmp::Reverse(p.0)));
    let Some((deficit, neediest)) = neediest else {
        return Vec::new();
    };

    let amount = surplus.min(deficit);
    if amount == 0 {
        return Vec::new();
    }
    vec![(neediest, amount)]
}

/// The total each replica had withdrawn from each object when we last looked.
/// The next demand estimate is measured against these numbers. The loop in
/// `rebalance_task` owns it.
type Samples = std::collections::HashMap<String, BTreeMap<ReplicaId, u64>>;

/// Returns the demand since the previous round, which is how much each
/// member's total withdrawals rose. Also returns the new totals to keep for the
/// next round. It only computes: no I/O and no clock.
///
/// A replica with no earlier total to compare against counts as zero demand.
/// With no earlier totals at all (`prev == None`) every demand is zero, so the
/// first round after startup uses the equal share. A replica that joined or
/// rejoined the member list after sampling began has no earlier entry either,
/// so it counts as zero for that round. Otherwise everything it ever withdrew
/// would be counted as one round's demand.
///
/// Withdrawal totals only grow, because a merge keeps the larger number, so a
/// rise is never negative. `saturating_sub` is a guard in case a total ever
/// appears to shrink, which should not happen in normal operation.
fn sample_demand(
    counter: &BoundedCounter,
    members: &[ReplicaId],
    prev: Option<&BTreeMap<ReplicaId, u64>>,
) -> (BTreeMap<ReplicaId, u64>, BTreeMap<ReplicaId, u64>) {
    let now: BTreeMap<ReplicaId, u64> =
        members.iter().map(|&p| (p, counter.withdrawn(p))).collect();
    let demand = match prev {
        None => BTreeMap::new(),
        Some(prev) => now
            .iter()
            .map(|(&p, &t)| (p, prev.get(&p).map_or(0, |&b| t.saturating_sub(b))))
            .collect(),
    };
    (demand, now)
}

/// Runs one rebalancing pass. It reads which machines are connected and
/// every bounded counter, then applies each planned transfer. Returns false if
/// the engine channel has closed.
async fn one_round(
    engine: &mpsc::Sender<Command>,
    peers: &PeerRegistry,
    me: ReplicaId,
    wall: &Arc<AtomicU64>,
    samples: &mut Samples,
) -> bool {
    // The members: this node and the machines it has a connection to right
    // now. A machine that has died, or that is only heard about through
    // others, is left out. Allowance handed to it could not be spent or lent
    // back by anyone we can reach.
    let mut members = peers.connected();
    members.push(me);
    members.sort();
    members.dedup();
    // A machine that has left the team for good is given nothing, and its
    // share is not counted. Once this machine has left, it hands its
    // allowance over itself (see `crate::leave`) and plans nothing here. A
    // machine counts as gone only by its own mark, so another cannot cut it
    // off.
    let (reply, rx) = oneshot::channel();
    if engine.send(Command::Snapshot { object_id: crate::leave::LEFT.into(), reply }).await.is_err() {
        return false;
    }
    if let Ok(Some(TypedCrdt::OrSet(set))) = rx.await {
        let gone = crate::leave::numbers_in(&set);
        if gone.contains(&me) {
            return true;
        }
        members.retain(|member| !gone.contains(member));
    }
    if members.len() < 2 {
        return true; // there is nobody else to give allowance to
    }

    // The object ids. The digest used for catch-up between machines lists
    // every object, so we use it to get the ids.
    let (reply, rx) = oneshot::channel();
    if engine.send(Command::Digest { reply }).await.is_err() {
        return false;
    }
    let Ok(entries) = rx.await else { return false };

    let ids: Vec<String> = entries.into_iter().map(|(id, _)| id).collect();
    // Forget the stored totals for objects the digest no longer lists, so a
    // removed object does not leave an entry behind forever.
    samples.retain(|k, _| ids.iter().any(|id| id == k));

    for object_id in ids {
        // Read the object. Skip it if it is not a bounded counter.
        let (reply, rx) = oneshot::channel();
        if engine.send(Command::Snapshot { object_id: object_id.clone(), reply }).await.is_err() {
            return false;
        }
        let Ok(Some(TypedCrdt::BoundedCounter(b))) = rx.await else { continue };

        let (demand, now) = sample_demand(&b, &members, samples.get(&object_id));
        samples.insert(object_id.clone(), now);

        for (peer, amount) in plan_transfers(&b, me, &members, &demand) {
            let (done, drx) = oneshot::channel();
            let f: crate::engine::TryMutateFn = Box::new(move |crdt, _hlc, r| match crdt {
                TypedCrdt::BoundedCounter(bc) => bc.transfer(peer, amount, r).map(TypedDelta::BoundedCounter),
                _ => None,
            });
            let wall_v = wall.fetch_add(1, Ordering::Relaxed);
            if engine.send(Command::TryMutate { object_id: object_id.clone(), f, wall: wall_v, receipt: None, done }).await.is_err() {
                return false;
            }
            let _ = drx.await; // Refused or NoObject is fine: our view was out of date, and nothing can be overspent.
        }
    }
    true
}

/// The rebalancing loop. A node runs one of these, and only when
/// `NodeConfig.rebalance_interval` is `Some`. It stops on shutdown.
pub(crate) async fn rebalance_task(
    engine: mpsc::Sender<Command>,
    peers: PeerRegistry,
    me: ReplicaId,
    wall: Arc<AtomicU64>,
    period: Duration,
    cancel: CancellationToken,
) {
    let mut ticker = interval(period);
    let mut samples = Samples::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                if !one_round(&engine, &peers, me, &wall, &mut samples).await {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Gives replica `r` an allowance of `n` by depositing that much.
    fn deposit(counter: &mut BoundedCounter, r: u64, n: u64) {
        counter.increment(n, ReplicaId(r));
    }

    fn members(ids: &[u64]) -> Vec<ReplicaId> {
        ids.iter().map(|&i| ReplicaId(i)).collect()
    }

    // Applies a planned transfer to the counter, the way the giver would.
    fn apply(counter: &mut BoundedCounter, from: u64, to: ReplicaId, amount: u64) {
        counter.transfer(to, amount, ReplicaId(from)).expect("giver has surplus");
    }

    fn no_demand() -> BTreeMap<ReplicaId, u64> {
        BTreeMap::new()
    }

    fn demand(pairs: &[(u64, u64)]) -> BTreeMap<ReplicaId, u64> {
        pairs.iter().map(|&(r, d)| (ReplicaId(r), d)).collect()
    }

    #[test]
    fn two_nodes_gives_half() {
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10); // A=10, B=0, value=10, target=5
        let plan = plan_transfers(&c, ReplicaId(1), &members(&[1, 2]), &no_demand());
        assert_eq!(plan, vec![(ReplicaId(2), 5)]);
        apply(&mut c, 1, ReplicaId(2), 5);
        // Now A holds 5, which is its target, so it has nothing spare.
        assert!(plan_transfers(&c, ReplicaId(1), &members(&[1, 2]), &no_demand()).is_empty());
    }

    #[test]
    fn three_nodes_converge_over_rounds() {
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 9); // A=9,B=0,C=0 ; value=9, target=3
        let mem = members(&[1, 2, 3]);
        // Run rounds until A plans nothing, up to ten rounds.
        for _ in 0..10 {
            let plan = plan_transfers(&c, ReplicaId(1), &mem, &no_demand());
            if plan.is_empty() {
                break;
            }
            let (to, amt) = plan[0];
            apply(&mut c, 1, to, amt);
        }
        assert_eq!(c.allowance(ReplicaId(1)), 3);
        assert_eq!(c.allowance(ReplicaId(2)), 3);
        assert_eq!(c.allowance(ReplicaId(3)), 3);
        assert_eq!(c.value(), 9);
    }

    #[test]
    fn remainder_settles_without_thrash() {
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10); // value=10, N=3, target=3
        let mem = members(&[1, 2, 3]);
        for _ in 0..10 {
            let plan = plan_transfers(&c, ReplicaId(1), &mem, &no_demand());
            if plan.is_empty() {
                break;
            }
            let (to, amt) = plan[0];
            apply(&mut c, 1, to, amt);
        }
        // A keeps the remainder. The others are at their target, so none is below it and A plans nothing.
        assert!(plan_transfers(&c, ReplicaId(1), &mem, &no_demand()).is_empty());
        assert_eq!(c.allowance(ReplicaId(2)), 3);
        assert_eq!(c.allowance(ReplicaId(3)), 3);
        assert_eq!(c.allowance(ReplicaId(1)), 4);
    }

    #[test]
    fn no_surplus_or_no_needy_or_solo_is_empty() {
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10);
        // Only one member (N < 2).
        assert!(plan_transfers(&c, ReplicaId(1), &members(&[1]), &no_demand()).is_empty());
        // The counter's value is zero.
        let empty = BoundedCounter::default();
        assert!(plan_transfers(&empty, ReplicaId(1), &members(&[1, 2]), &no_demand()).is_empty());
        // B has nothing spare: it holds 0, which is not above its target, so it plans nothing.
        assert!(plan_transfers(&c, ReplicaId(2), &members(&[1, 2]), &no_demand()).is_empty());
    }

    #[test]
    fn never_plans_more_than_surplus() {
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 100); // value=100, N=2, target=50, surplus=50
        let plan = plan_transfers(&c, ReplicaId(1), &members(&[1, 2]), &no_demand());
        let (_, amount) = plan[0];
        assert!(amount <= 50, "transfer never drives self below target");
    }

    #[test]
    fn demand_splits_in_proportion() {
        // value 12; B spent 3, C spent 1, A spent nothing.
        // Targets: A = 12*0/4 = 0, B = 12*3/4 = 9, C = 12*1/4 = 3.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 12);
        let mem = members(&[1, 2, 3]);
        let d = demand(&[(2, 3), (3, 1)]);

        // A gives to the member furthest below its own target. That is B (short by 9), not C (short by 3).
        assert_eq!(plan_transfers(&c, ReplicaId(1), &mem, &d), vec![(ReplicaId(2), 9)]);
        apply(&mut c, 1, ReplicaId(2), 9);
        assert_eq!(plan_transfers(&c, ReplicaId(1), &mem, &d), vec![(ReplicaId(3), 3)]);
        apply(&mut c, 1, ReplicaId(3), 3);

        assert_eq!(c.allowance(ReplicaId(1)), 0);
        assert_eq!(c.allowance(ReplicaId(2)), 9);
        assert_eq!(c.allowance(ReplicaId(3)), 3);
        assert_eq!(c.value(), 12, "transfers never change value");
    }

    #[test]
    fn zero_demand_is_exactly_the_equal_spread() {
        // With no demand the target is value/N, the receiver is the member
        // with the least allowance, and a tie goes to the lowest ReplicaId.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 9);
        let mem = members(&[1, 2, 3]);
        assert_eq!(
            plan_transfers(&c, ReplicaId(1), &mem, &no_demand()),
            vec![(ReplicaId(2), 3)]
        );
    }

    #[test]
    fn an_idle_node_hands_its_balance_to_the_spender() {
        // C holds everything but has never spent, so its target is 0 and B's is
        // the whole value. This is what weighting by demand is for.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 3, 8);
        let mem = members(&[1, 2, 3]);
        let d = demand(&[(2, 5)]);
        assert_eq!(plan_transfers(&c, ReplicaId(3), &mem, &d), vec![(ReplicaId(2), 8)]);
    }

    #[test]
    fn picks_the_peer_furthest_below_its_own_target_not_the_poorest() {
        // C holds less than B (0 against 2), but its target is low. B is
        // further below its own target, so B is picked. An equal share would
        // pick C, so this test tells the two rules apart.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 20);
        deposit(&mut c, 2, 2);
        let mem = members(&[1, 2, 3]);
        // value 22, total demand 10 -> targets A=0, B=22*9/10=19, C=22*1/10=2.
        // B is short by 19-2=17, C by 2-0=2.
        let d = demand(&[(2, 9), (3, 1)]);
        assert_eq!(plan_transfers(&c, ReplicaId(1), &mem, &d)[0].0, ReplicaId(2));
    }

    #[test]
    fn never_overshoots_either_side() {
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 100);
        let mem = members(&[1, 2]);
        let d = demand(&[(1, 1), (2, 1)]); // equal demand -> equal targets of 50
        let plan = plan_transfers(&c, ReplicaId(1), &mem, &d);
        let (to, amount) = plan[0];
        assert_eq!(to, ReplicaId(2));
        assert!(amount <= 50, "giver never drops below its own target");
        apply(&mut c, 1, to, amount);
        assert!(c.allowance(ReplicaId(2)) <= 50, "receiver never exceeds its target");
    }

    #[test]
    fn demand_for_an_absent_member_is_ignored() {
        // A demand entry for a replica that is not in `members` must not change the total demand.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10);
        let mem = members(&[1, 2]);
        let d = demand(&[(2, 1), (99, 1_000_000)]);
        // The total demand of the members is 1, so B's target is the whole value.
        assert_eq!(plan_transfers(&c, ReplicaId(1), &mem, &d), vec![(ReplicaId(2), 10)]);
    }

    #[test]
    fn a_rich_idle_peer_never_panics_the_planner() {
        // C holds far more than its target of 0. The planner must skip it as
        // not being short. Working out `t - a` for it would underflow and
        // panic in debug builds, which stops the rebalance loop.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10);
        deposit(&mut c, 3, 90);
        let mem = members(&[1, 2, 3]);
        let d = demand(&[(2, 5)]); // targets: A=0, B=100, C=0
        assert_eq!(plan_transfers(&c, ReplicaId(1), &mem, &d), vec![(ReplicaId(2), 10)]);
    }

    #[test]
    fn the_first_sample_reports_no_demand() {
        // There are no earlier totals to compare against, so demand is zero for
        // everyone and the equal share is used. The totals are still recorded.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10);
        c.try_decrement(4, ReplicaId(1)).expect("allowance");
        let mem = members(&[1, 2]);
        let (demand, now) = sample_demand(&c, &mem, None);
        assert!(demand.values().all(|&d| d == 0), "no baseline -> no demand");
        assert_eq!(now.get(&ReplicaId(1)).copied(), Some(4));
        assert_eq!(now.get(&ReplicaId(2)).copied(), Some(0));
    }

    #[test]
    fn the_second_sample_sees_the_rise() {
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10);
        let mem = members(&[1, 2]);
        let (_, first) = sample_demand(&c, &mem, None);
        c.try_decrement(3, ReplicaId(1)).expect("allowance");
        let (demand, second) = sample_demand(&c, &mem, Some(&first));
        assert_eq!(demand.get(&ReplicaId(1)).copied(), Some(3), "the rise since the baseline");
        assert_eq!(demand.get(&ReplicaId(2)).copied(), Some(0));
        // A third sample with no further spending reports no demand again.
        let (demand, _) = sample_demand(&c, &mem, Some(&second));
        assert!(demand.values().all(|&d| d == 0), "idle round -> no demand");
    }

    #[test]
    fn a_late_joining_member_contributes_no_demand_on_its_first_round() {
        // A replica that shows up in the member list after sampling has begun
        // has no earlier total. Everything it ever withdrew must not be counted
        // as one round of demand. That would hand all the spare allowance to an
        // idle newcomer at the moment it joins.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 3, 900);
        c.try_decrement(900, ReplicaId(3)).expect("allowance");
        deposit(&mut c, 1, 100);

        // Round 1: only r1 and r2 are known.
        let (_, first) = sample_demand(&c, &members(&[1, 2]), None);
        // Round 2: r3 joins, having withdrawn 900 in total before.
        let (demand, second) = sample_demand(&c, &members(&[1, 2, 3]), Some(&first));
        assert_eq!(
            demand.get(&ReplicaId(3)).copied(),
            Some(0),
            "a newcomer's lifetime history is not this round's demand"
        );
        // From its second round on it is sampled like any other member.
        deposit(&mut c, 3, 7);
        c.try_decrement(7, ReplicaId(3)).expect("allowance");
        let (demand, _) = sample_demand(&c, &members(&[1, 2, 3]), Some(&second));
        assert_eq!(demand.get(&ReplicaId(3)).copied(), Some(7));
    }

    #[test]
    fn enormous_demand_does_not_overflow_the_denominator() {
        // The total demand is added up in a type wide enough that huge
        // withdrawal counts, whether from a fault or from an attacker, cannot
        // overflow. An overflow would wrap around, or panic in a debug build
        // and stop the rebalancing loop.
        let mut c = BoundedCounter::default();
        deposit(&mut c, 1, 10);
        let mem = members(&[1, 2, 3]);
        let d = demand(&[(2, u64::MAX), (3, u64::MAX)]);
        // Equal, enormous demand -> equal targets of 5; A holds 10, target 0.
        assert_eq!(plan_transfers(&c, ReplicaId(1), &mem, &d), vec![(ReplicaId(2), 5)]);
    }
}
