//! The stable frontier: the updates that every known member has seen.
//!
//! A delta (one small change) is "causally stable" once every member has it.
//! After that it can leave the log used for catch-up. A machine that falls
//! behind later is brought up to date with a snapshot, the whole current state
//! of an object.
//!
//! For each object, the frontier is the part that all members have seen in
//! common. This node's own replica is left out. With no other members the
//! frontier is empty, so a node on its own treats nothing as stable.

use orvu_crdt::{DotContext, ReplicaId, VersionVector};
use std::collections::BTreeMap;

/// Whether this node currently thinks a member is alive. There is no `Dead`
/// value. A dead member is removed from the map, and once it is gone it no
/// longer holds the frontier back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberStatus {
    Alive,
    Suspect,
}

/// What this node knows about one member.
pub struct MemberEntry {
    /// How many live direct connections we have to this replica. 0 means we
    /// only know of it through others. This is a count because two machines
    /// that each list the other open two connections between them.
    pub refs: usize,
    /// For each object, which updates the member had seen when we last heard,
    /// taken from its digests.
    pub contexts: BTreeMap<String, DotContext>,
    /// The member's own counter, which only goes up. It is passed around
    /// between machines.
    pub heartbeat: u64,
    /// Our local time when this entry was last refreshed. Used to notice a
    /// member that has gone quiet. Not sent to other machines.
    pub updated_at: std::time::Instant,
    /// Whether we think the member is alive. Never sent to other machines.
    pub status: MemberStatus,
    /// When the member became `Suspect`. `dead_timeout` is measured from here.
    /// Means nothing while the member is `Alive`.
    pub suspect_since: std::time::Instant,
}

/// The stable frontier for one object. `frontier.get(r)` is the highest counter
/// of replica `r` that every other member has seen. Only an unbroken run
/// of counters from 1 counts, so an update seen after a gap does not. The
/// result is empty when there are no other members.
pub fn stable_frontier(
    members: &BTreeMap<ReplicaId, MemberEntry>,
    self_id: ReplicaId,
    object_id: &str,
) -> VersionVector {
    let mut frontier = VersionVector::new();
    // Only the other members count. What we have seen ourselves says nothing
    // about what the rest of the cluster received.
    let others: Vec<&MemberEntry> = members
        .iter()
        .filter(|(id, _)| **id != self_id)
        .map(|(_, e)| e)
        .collect();
    // With no other members, nobody has confirmed receiving anything, so the
    // frontier is empty.
    let Some(first) = others.first() else { return frontier };
    // A member that has never seen the object counts as 0 for every replica.
    // So if the first member does not have it, the frontier is empty.
    let Some(first_ctx) = first.contexts.get(object_id) else { return frontier };
    // We only look at replicas the first member knows about. For any other
    // replica the first member counts as 0, so the minimum is 0 anyway and
    // skipping it is correct.
    for (r, first_c) in first_ctx.version_vector().iter() {
        let mut m = first_c;
        for e in &others {
            let c = e.contexts.get(object_id).map_or(0, |ctx| ctx.version_vector().get(r));
            m = m.min(c);
            if m == 0 {
                break;
            }
        }
        frontier.observe(r, m); // observe(_, 0) does nothing
    }
    frontier
}

/// Merges what another machine told us about a member. The report with the
/// highest heartbeat wins. Never call this for this node's own entry.
pub fn merge_member(
    members: &mut BTreeMap<ReplicaId, MemberEntry>,
    id: ReplicaId,
    heartbeat: u64,
    contexts: Vec<(String, DotContext)>,
    now: std::time::Instant,
) {
    let e = members.entry(id).or_insert_with(|| MemberEntry {
        refs: 0,
        contexts: BTreeMap::new(),
        heartbeat: 0,
        updated_at: now,
        status: MemberStatus::Alive,
        suspect_since: now,
    });
    if heartbeat > e.heartbeat {
        // A strictly higher heartbeat proves the member is alive. Take the new
        // data and clear any suspicion. The heartbeat does the job that the
        // incarnation number does in the SWIM membership protocol.
        e.contexts = contexts.into_iter().collect();
        e.heartbeat = heartbeat;
        e.updated_at = now;
        e.status = MemberStatus::Alive;
    } else if heartbeat == e.heartbeat && e.contexts.is_empty() {
        // Only for a new entry that is still empty: fill it in. This never
        // lowers a heartbeat and never clears suspicion. Only a higher
        // heartbeat can do that.
        e.contexts = contexts.into_iter().collect();
        e.updated_at = now;
    }
}

/// What one `sweep_membership` pass changed, so the caller can record it in
/// its metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepOutcome {
    pub suspected: u64,
    pub evicted: u64,
}

/// One pass of the check for members that have stopped responding. For each
/// member other than this node:
/// - one with a live connection (`refs > 0`) is set to `Alive` and is never
///   suspected;
/// - an `Alive` member with no direct connection, not refreshed for longer
///   than `suspect_timeout`, becomes `Suspect`;
/// - a `Suspect` member whose `suspect_since` is more than `dead_timeout` ago
///   is removed.
///
/// This node's own entry is never touched.
pub fn sweep_membership(
    members: &mut BTreeMap<ReplicaId, MemberEntry>,
    self_id: ReplicaId,
    now: std::time::Instant,
    suspect_timeout: std::time::Duration,
    dead_timeout: std::time::Duration,
) -> SweepOutcome {
    let mut evict: Vec<ReplicaId> = Vec::new();
    let mut suspected: u64 = 0;
    for (id, e) in members.iter_mut() {
        if *id == self_id {
            continue;
        }
        if e.refs > 0 {
            e.status = MemberStatus::Alive; // a live connection means it is alive
            continue;
        }
        match e.status {
            MemberStatus::Alive => {
                if now.duration_since(e.updated_at) > suspect_timeout {
                    e.status = MemberStatus::Suspect;
                    e.suspect_since = now;
                    suspected += 1;
                }
            }
            MemberStatus::Suspect => {
                if now.duration_since(e.suspect_since) > dead_timeout {
                    evict.push(*id);
                }
            }
        }
    }
    let evicted = evict.len() as u64;
    for id in evict {
        members.remove(&id);
    }
    SweepOutcome { suspected, evicted }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orvu_crdt::Dot;

    fn ctx(dots: &[(u64, u64)]) -> DotContext {
        let mut c = DotContext::new();
        for &(r, n) in dots {
            c.insert(Dot::new(ReplicaId(r), n));
        }
        c
    }

    fn member(contexts: &[(&str, DotContext)]) -> MemberEntry {
        let mut e = MemberEntry {
            refs: 1,
            contexts: BTreeMap::new(),
            heartbeat: 0,
            updated_at: std::time::Instant::now(),
            status: MemberStatus::Alive,
            suspect_since: std::time::Instant::now(),
        };
        for (id, c) in contexts {
            e.contexts.insert((*id).to_string(), c.clone());
        }
        e
    }

    #[test]
    fn empty_without_members() {
        let members: BTreeMap<ReplicaId, MemberEntry> = BTreeMap::new();
        assert_eq!(stable_frontier(&members, ReplicaId(1), "o"), VersionVector::new());
    }

    #[test]
    fn frontier_is_the_min_across_members() {
        let mut members = BTreeMap::new();
        members.insert(ReplicaId(2), member(&[("o", ctx(&[(1, 1), (1, 2), (1, 3), (1, 4), (1, 5)]))]));
        members.insert(ReplicaId(3), member(&[("o", ctx(&[(1, 1), (1, 2), (1, 3)]))]));
        let f = stable_frontier(&members, ReplicaId(1), "o");
        assert_eq!(f.get(ReplicaId(1)), 3, "min of 5 and 3");
    }

    #[test]
    fn a_member_missing_the_object_holds_the_frontier_at_zero() {
        let mut members = BTreeMap::new();
        members.insert(ReplicaId(2), member(&[("o", ctx(&[(1, 1), (1, 2), (1, 3)]))]));
        members.insert(ReplicaId(3), member(&[])); // knows no objects
        let f = stable_frontier(&members, ReplicaId(1), "o");
        assert_eq!(f.get(ReplicaId(1)), 0, "one member hasn't seen the object -> nothing stable");
    }

    #[test]
    fn gap_dots_are_not_counted_stable() {
        let mut members = BTreeMap::new();
        // (1,1), then a gap. (1,3) was seen out of order, so the unbroken run
        // of counters only reaches 1.
        members.insert(ReplicaId(2), member(&[("o", ctx(&[(1, 1), (1, 3)]))]));
        let f = stable_frontier(&members, ReplicaId(1), "o");
        assert_eq!(f.get(ReplicaId(1)), 1, "clouded (1,3) is not stable until the gap fills");
    }

    #[test]
    fn frontier_excludes_self() {
        let mut members = BTreeMap::new();
        // This node has seen less than the other member. If its own entry were
        // counted the result would be 1, so expecting 3 checks that it is left
        // out.
        members.insert(ReplicaId(1), member(&[("o", ctx(&[(1, 1)]))])); // this node
        members.insert(ReplicaId(2), member(&[("o", ctx(&[(1, 1), (1, 2), (1, 3)]))]));
        let f = stable_frontier(&members, ReplicaId(1), "o");
        assert_eq!(f.get(ReplicaId(1)), 3, "self's lower value must be ignored");
    }

    #[test]
    fn merge_member_takes_the_freshest_heartbeat() {
        let mut members = BTreeMap::new();
        // An existing entry at heartbeat 5.
        let mut e = member(&[("o", ctx(&[(3, 1)]))]);
        e.heartbeat = 5;
        members.insert(ReplicaId(3), e);
        // An older update (heartbeat 4) is ignored.
        merge_member(
            &mut members,
            ReplicaId(3),
            4,
            vec![("o".into(), ctx(&[(3, 1), (3, 2)]))],
            std::time::Instant::now(),
        );
        assert_eq!(members[&ReplicaId(3)].contexts["o"].version_vector().get(ReplicaId(3)), 1);
        // A newer update (heartbeat 6) is applied.
        merge_member(
            &mut members,
            ReplicaId(3),
            6,
            vec![("o".into(), ctx(&[(3, 1), (3, 2)]))],
            std::time::Instant::now(),
        );
        assert_eq!(members[&ReplicaId(3)].contexts["o"].version_vector().get(ReplicaId(3)), 2);
        assert_eq!(members[&ReplicaId(3)].heartbeat, 6);
    }

    #[test]
    fn merge_member_does_not_regress_heartbeat_for_object_less_member() {
        // A member is known with a high heartbeat but no objects (empty contexts).
        let mut members = BTreeMap::new();
        let mut e = member(&[]);
        e.heartbeat = 5;
        members.insert(ReplicaId(3), e);
        // An older message with a lower heartbeat that carries contexts must be
        // rejected, even though the stored contexts are empty.
        merge_member(
            &mut members,
            ReplicaId(3),
            3,
            vec![("o".into(), ctx(&[(3, 1)]))],
            std::time::Instant::now(),
        );
        assert_eq!(members[&ReplicaId(3)].heartbeat, 5, "lower heartbeat must not win");
        assert!(members[&ReplicaId(3)].contexts.is_empty(), "stale contexts must not be applied");
    }

    #[test]
    fn merge_member_populates_a_fresh_entry_at_equal_heartbeat() {
        // A new member starts at heartbeat 0 (from or_insert). A report about
        // it at heartbeat 0 must still fill the entry in. This is the case
        // where an equal heartbeat fills an empty entry.
        let mut members = BTreeMap::new();
        merge_member(
            &mut members,
            ReplicaId(3),
            0,
            vec![("o".into(), ctx(&[(3, 1)]))],
            std::time::Instant::now(),
        );
        assert_eq!(members[&ReplicaId(3)].contexts["o"].version_vector().get(ReplicaId(3)), 1);
    }

    #[test]
    fn sweep_suspects_a_stale_indirect_member_then_evicts_it() {
        use std::time::Duration;
        let now = std::time::Instant::now();
        let suspect_timeout = Duration::from_secs(2);
        let dead_timeout = Duration::from_secs(2);
        let mut members = BTreeMap::new();
        // No direct connection (refs 0) and last refreshed 3s ago, so it should
        // be suspected.
        let mut m = member(&[("o", ctx(&[(3, 1)]))]);
        m.refs = 0;
        m.updated_at = now - Duration::from_secs(3);
        members.insert(ReplicaId(3), m);

        // First sweep: past suspect_timeout (3s > 2s), so it becomes Suspect
        // with suspect_since = now.
        sweep_membership(&mut members, ReplicaId(1), now, suspect_timeout, dead_timeout);
        assert_eq!(members[&ReplicaId(3)].status, MemberStatus::Suspect);
        assert!(members.contains_key(&ReplicaId(3)), "not yet evicted");

        // Later sweep: dead_timeout has passed since suspect_since, so it is removed.
        let later = now + Duration::from_secs(3);
        sweep_membership(&mut members, ReplicaId(1), later, suspect_timeout, dead_timeout);
        assert!(!members.contains_key(&ReplicaId(3)), "evicted after dead_timeout");
    }

    #[test]
    fn sweep_reports_suspected_and_evicted_counts() {
        use std::time::Duration;
        let now = std::time::Instant::now();
        let suspect_timeout = Duration::from_secs(2);
        let dead_timeout = Duration::from_secs(2);
        let mut members = BTreeMap::new();
        // One Alive member with no connection that has gone quiet. This sweep
        // will suspect it.
        let mut a = member(&[("o", ctx(&[(2, 1)]))]);
        a.refs = 0;
        a.updated_at = now - Duration::from_secs(3);
        members.insert(ReplicaId(2), a);
        // One member that is already Suspect and past dead_timeout. This sweep
        // will remove it.
        let mut b = member(&[("o", ctx(&[(3, 1)]))]);
        b.refs = 0;
        b.status = MemberStatus::Suspect;
        b.suspect_since = now - Duration::from_secs(3);
        members.insert(ReplicaId(3), b);

        let outcome = sweep_membership(&mut members, ReplicaId(1), now, suspect_timeout, dead_timeout);
        assert_eq!(outcome.suspected, 1, "member 2 newly suspected");
        assert_eq!(outcome.evicted, 1, "member 3 evicted");
        assert_eq!(members[&ReplicaId(2)].status, MemberStatus::Suspect);
        assert!(!members.contains_key(&ReplicaId(3)));
    }

    #[test]
    fn a_connected_member_is_never_suspected() {
        use std::time::Duration;
        let now = std::time::Instant::now();
        let mut members = BTreeMap::new();
        // refs > 0 with a very old updated_at. It stays Alive because the
        // connection is live.
        let mut m = member(&[("o", ctx(&[(2, 1)]))]);
        m.refs = 1;
        m.updated_at = now - Duration::from_secs(60);
        members.insert(ReplicaId(2), m);
        sweep_membership(&mut members, ReplicaId(1), now, Duration::from_secs(2), Duration::from_secs(2));
        assert_eq!(members[&ReplicaId(2)].status, MemberStatus::Alive);
        assert!(members.contains_key(&ReplicaId(2)));
    }

    #[test]
    fn self_is_never_suspected_or_evicted() {
        use std::time::Duration;
        let now = std::time::Instant::now();
        let mut members = BTreeMap::new();
        let mut me = member(&[("o", ctx(&[(1, 1)]))]);
        me.refs = 0;
        me.updated_at = now - Duration::from_secs(60);
        members.insert(ReplicaId(1), me);
        sweep_membership(&mut members, ReplicaId(1), now, Duration::from_secs(2), Duration::from_secs(2));
        assert!(members.contains_key(&ReplicaId(1)));
        assert_eq!(members[&ReplicaId(1)].status, MemberStatus::Alive);
    }

    #[test]
    fn a_higher_heartbeat_refutes_suspicion() {
        let now = std::time::Instant::now();
        let mut members = BTreeMap::new();
        // A member already marked Suspect at heartbeat 5.
        let mut m = member(&[("o", ctx(&[(3, 1)]))]);
        m.refs = 0;
        m.heartbeat = 5;
        m.status = MemberStatus::Suspect;
        m.suspect_since = now;
        members.insert(ReplicaId(3), m);
        // A strictly higher heartbeat (6) arrives, so it goes back to Alive.
        merge_member(
            &mut members,
            ReplicaId(3),
            6,
            vec![("o".into(), ctx(&[(3, 1), (3, 2)]))],
            now,
        );
        assert_eq!(members[&ReplicaId(3)].status, MemberStatus::Alive);
        assert_eq!(members[&ReplicaId(3)].heartbeat, 6);
    }

    #[test]
    fn an_equal_heartbeat_does_not_refute_suspicion() {
        let now = std::time::Instant::now();
        let mut members = BTreeMap::new();
        let mut m = member(&[("o", ctx(&[(3, 1)]))]); // contexts are not empty
        m.refs = 0;
        m.heartbeat = 5;
        m.status = MemberStatus::Suspect;
        m.suspect_since = now;
        members.insert(ReplicaId(3), m);
        // An equal heartbeat when the stored contexts are not empty is ignored,
        // so the member stays Suspect.
        merge_member(&mut members, ReplicaId(3), 5, vec![("o".into(), ctx(&[(3, 9)]))], now);
        assert_eq!(members[&ReplicaId(3)].status, MemberStatus::Suspect);
    }

    #[test]
    fn a_suspect_member_still_counts_in_the_frontier() {
        let now = std::time::Instant::now();
        let mut members = BTreeMap::new();
        let mut m = member(&[("o", ctx(&[(1, 1), (1, 2), (1, 3)]))]);
        m.status = MemberStatus::Suspect;
        m.suspect_since = now;
        members.insert(ReplicaId(2), m);
        // The frontier for node 1 still takes the Suspect member into account.
        let f = stable_frontier(&members, ReplicaId(1), "o");
        assert_eq!(f.get(ReplicaId(1)), 3, "suspect member still constrains GC");
    }

    #[test]
    fn frontier_empty_with_only_self() {
        let mut members = BTreeMap::new();
        members.insert(ReplicaId(1), member(&[("o", ctx(&[(1, 1)]))]));
        assert_eq!(stable_frontier(&members, ReplicaId(1), "o"), VersionVector::new());
    }
}
