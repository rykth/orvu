//! A tree that several machines can edit separately and then merge, and that
//! can never end up with a loop in it. The approach is Kleppmann's "move tree".
//!
//! The only way to change the shape of the tree is to move a node under a new
//! parent. The tree does not store its shape as the source of truth. It keeps a
//! log of every move anyone has made, and moves are only ever added to that
//! log. Each move is filed under an id, `OpId = (Hlc, ReplicaId)`: a timestamp
//! plus the id of the machine that made the move. (`Hlc` is a hybrid logical
//! clock, a timestamp that combines wall-clock time with a counter.) Every move
//! must have its own id, and any two ids can be compared, so the whole log has
//! one fixed order.
//!
//! The `parent` map, which says where each node sits right now, is worked out
//! from the log and from nothing else. We start from an empty tree and replay
//! the moves in timestamp order. A move is applied only if it would not create
//! a loop. A move that would put a node under itself, or under one of its own
//! descendants, is skipped.
//!
//! That rule is what keeps the tree free of loops when two machines move things
//! at the same time. Say one machine moves `a` under `b` while another moves
//! `b` under `a`. Each move is fine on its own, but both together would make a
//! loop. Once a machine has both moves, it replays them in id order. The first
//! one is applied. The second is skipped, because by then it would make a loop.
//! Every machine holding the same log replays it in the same order, so they all
//! arrive at the same tree, and that tree has no loop. Nothing needs repairing
//! afterwards.
//!
//! `merge` adds the incoming moves to the log and replays it. The log only
//! grows, and `parent` depends only on the log. So `merge` gives the same
//! result whatever order deltas arrive in, however they are grouped, and even
//! if the same delta arrives twice.
//!
//! A node can also have a label and a position among its siblings. Those are
//! kept apart from the move log and play no part in the replay.

use crate::error::WriteError;
use crate::{DeltaCrdt, Hlc, LwwRegister, OrMap, OrMapDelta, ReplicaId};
use std::collections::{BTreeMap, BTreeSet};

/// The id of a node in the tree.
pub type NodeId = String;

/// Everything the tree knows about one node: where it sits, its label, and its
/// position among its siblings. `parent` is `None` for a root. It is also
/// `None` for an id that has a label or a position but no place in the tree,
/// for example because it has never been moved. `position` is `None` until the
/// node is placed with `move_node_at`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TreeNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub label: Option<String>,
    pub position: Option<String>,
}

/// A single move: `child` becomes a child of `new_parent`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Move {
    child: NodeId,
    new_parent: NodeId,
}

/// The id of one move. Every move has its own id, and any two ids can be
/// compared. The `Hlc` timestamp comes first so that the replay visits moves in
/// timestamp order. The `ReplicaId` decides between two moves whose timestamps
/// are equal in both the wall-clock part and the counter.
type OpId = (Hlc, ReplicaId);

/// A tree that several machines can edit and merge, and that never has a loop.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tree {
    log: BTreeMap<OpId, Move>,
    parent: BTreeMap<NodeId, NodeId>,
    /// Each node's label. If two machines label the same node, the later write
    /// wins (last-writer-wins). Labels are kept apart from the move log and the
    /// replay never reads them, so they cannot cause a loop.
    labels: OrMap<NodeId, LwwRegister<String>>,
    /// Each node's position among its siblings, as a fractional index key (see
    /// `fracindex`). The later write wins here too. Like `labels`, positions
    /// are kept apart from the move log. A position is only used to sort
    /// siblings and nothing relies on it being right, so it cannot affect the
    /// replay or cause a loop.
    positions: OrMap<NodeId, LwwRegister<String>>,
}

/// A set of changes to a `Tree`, to be merged into other copies of it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TreeDelta {
    moves: BTreeMap<OpId, Move>,
    labels: Option<OrMapDelta<NodeId, LwwRegister<String>>>,
    positions: Option<OrMapDelta<NodeId, LwwRegister<String>>>,
}

impl TreeDelta {
    /// Builds a delta from decoded moves, one `(hlc, replica, child,
    /// new_parent)` per move. The delta starts with no label or position
    /// changes. If two different moves share an id, one is kept, by the same
    /// rule the tree's log uses (see `keep_higher`).
    pub fn from_wire(ops: impl IntoIterator<Item = (Hlc, ReplicaId, NodeId, NodeId)>) -> Self {
        let mut moves = BTreeMap::new();
        for (h, r, c, p) in ops {
            keep_higher(&mut moves, (h, r), Move { child: c, new_parent: p });
        }
        Self { moves, labels: None, positions: None }
    }

    /// Attaches label changes that were decoded separately from the moves.
    pub fn with_labels(mut self, labels: Option<OrMapDelta<NodeId, LwwRegister<String>>>) -> Self {
        self.labels = labels;
        self
    }

    /// The label changes in this delta, if it has any.
    pub fn labels(&self) -> Option<&OrMapDelta<NodeId, LwwRegister<String>>> {
        self.labels.as_ref()
    }

    /// Attaches position changes that were decoded separately from the moves.
    pub fn with_positions(mut self, positions: Option<OrMapDelta<NodeId, LwwRegister<String>>>) -> Self {
        self.positions = positions;
        self
    }

    /// The sibling position changes in this delta, if it has any.
    pub fn positions(&self) -> Option<&OrMapDelta<NodeId, LwwRegister<String>>> {
        self.positions.as_ref()
    }

    /// The moves as `(hlc, replica, child, new_parent)`, in timestamp order.
    pub fn wire_parts(&self) -> impl Iterator<Item = (Hlc, ReplicaId, &str, &str)> {
        self.moves
            .iter()
            .map(|((h, r), m)| (*h, *r, m.child.as_str(), m.new_parent.as_str()))
    }
}

impl Tree {
    /// Records a move of `child` under `new_parent`, stamped `(hlc, replica)`.
    /// Returns a delta carrying just this move. The move always goes into the
    /// log, but the replay skips it if it would create a loop.
    ///
    /// Give every move its own `(hlc, replica)`. If the log already holds a
    /// different move under that id, only one of the two is kept (see
    /// `keep_higher`).
    pub fn move_node(&mut self, child: NodeId, new_parent: NodeId, hlc: Hlc, replica: ReplicaId) -> TreeDelta {
        let mv = Move { child, new_parent };
        keep_higher(&mut self.log, (hlc, replica), mv.clone());
        self.refold();
        let mut moves = BTreeMap::new();
        moves.insert((hlc, replica), mv);
        TreeDelta { moves, labels: None, positions: None }
    }

    /// Moves `child` under `new_parent` and puts it at `index` among that
    /// parent's other children. It does this by giving `child` a position key
    /// that sorts between the two neighbours at that slot. An `index` past the
    /// end means the end.
    ///
    /// `index` is exact when the siblings on either side of the slot were
    /// themselves placed with `move_node_at` and have different keys. A node
    /// that has never been placed sorts as if its key were empty, and every
    /// generated key sorts after the empty key. So a placed node always lands
    /// after every unplaced sibling, whatever index you ask for. When a
    /// neighbour at the slot is unplaced, the key is made as if there were no
    /// neighbour on that side, and it can come out equal to another sibling's
    /// key. Siblings with equal keys are ordered by id, so every pair of
    /// siblings still has a definite order and it is the same on every machine,
    /// but it may not be the order you asked for. So mixing `move_node` and
    /// `move_node_at` under one parent gives approximate placement, and an app
    /// that always uses `move_node_at` gets exact placement. The one exception
    /// is two machines placing different nodes into the same slot at the same
    /// time. They get equal keys, and a later placement between those two lands
    /// somewhere after both.
    ///
    /// If the move does not take effect because it would create a loop, nothing
    /// is placed and the node keeps the position it had.
    ///
    /// The position takes a dot. If the positions have no dot left for
    /// `replica`, this refuses with `WriteError::NoFreshDot` before it moves
    /// anything.
    pub fn move_node_at(
        &mut self,
        child: NodeId,
        new_parent: NodeId,
        index: usize,
        hlc: Hlc,
        replica: ReplicaId,
    ) -> Result<TreeDelta, WriteError> {
        if self.positions.room(replica) == 0 {
            return Err(WriteError::NoFreshDot);
        }
        let sibs: Vec<NodeId> = self
            .children(&new_parent)
            .into_iter()
            .filter(|c| *c != child)
            .collect();
        let idx = index.min(sibs.len());
        let left = if idx == 0 { None } else { self.position(&sibs[idx - 1]) };
        let right = sibs.get(idx).and_then(|s| self.position(s));
        let key = crate::fracindex::between(left.as_deref(), right.as_deref());

        let mut delta = self.move_node(child.clone(), new_parent.clone(), hlc, replica);
        // The replay may have skipped this move because it would create a loop.
        // A move that did not happen must not change anything else, so only set
        // the position if `child` really is under `new_parent` now.
        if self.parent(&child) == Some(&new_parent) {
            let pd = self.positions.update(child, replica, |reg| reg.set(key, hlc, replica))?;
            delta.positions = Some(pd);
        }
        Ok(delta)
    }

    /// `node`'s sibling position key, if it has been placed.
    pub fn position(&self, node: &str) -> Option<String> {
        self.positions.get(&node.to_string()).and_then(|reg| reg.get().cloned())
    }

    /// Sets `node`'s label to `name`, stamped `(hlc, replica)`. If two machines
    /// label the same node at the same time, the write with the higher `(hlc,
    /// replica)` wins (last-writer-wins). You can label an id that has never
    /// been moved, because a label does not depend on the node being in the
    /// tree.
    ///
    /// Refuses with `WriteError::NoFreshDot`, changing nothing, if the labels
    /// have no dot left for `replica`.
    pub fn set_label(&mut self, node: NodeId, name: String, hlc: Hlc, replica: ReplicaId) -> Result<TreeDelta, WriteError> {
        let d = self.labels.update(node, replica, |reg| reg.set(name, hlc, replica))?;
        Ok(TreeDelta { moves: BTreeMap::new(), labels: Some(d), positions: None })
    }

    /// `node`'s current label, if one was ever set.
    pub fn label(&self, node: &str) -> Option<String> {
        self.labels.get(&node.to_string()).and_then(|reg| reg.get().cloned())
    }

    /// The current parent of `node`, if any.
    pub fn parent(&self, node: &str) -> Option<&NodeId> {
        self.parent.get(node)
    }

    /// The children of `node` in sibling order: sorted by position key, and by
    /// id where the keys are equal. A child that has never been placed sorts as
    /// if its key were empty. That is before every generated key, because a
    /// generated key is never empty. So a tree that has never used
    /// `move_node_at` lists children in plain id order.
    pub fn children(&self, node: &str) -> Vec<NodeId> {
        let mut kids: Vec<(String, NodeId)> = self
            .parent
            .iter()
            .filter(|(_, p)| p.as_str() == node)
            .map(|(c, _)| (self.position(c).unwrap_or_default(), c.clone()))
            .collect();
        kids.sort();
        kids.into_iter().map(|(_, c)| c).collect()
    }

    /// Every node id that is in the tree, as a child or as a parent. Sorted,
    /// with each id listed once.
    pub fn nodes(&self) -> Vec<NodeId> {
        let mut set: BTreeSet<NodeId> = BTreeSet::new();
        for (c, p) in &self.parent {
            set.insert(c.clone());
            set.insert(p.clone());
        }
        set.into_iter().collect()
    }

    /// Every node the tree knows about, sorted by id. That is every id that is
    /// in the tree, has a label, or has a position. Unlike `nodes()` and
    /// `parent()`, the result changes when a label or a position changes. So
    /// comparing two results tells you whether anything a reader can see has
    /// changed, which is what code watching the tree needs.
    pub fn to_nodes(&self) -> Vec<TreeNode> {
        let mut ids: BTreeSet<NodeId> = self.nodes().into_iter().collect();
        ids.extend(self.labels.keys().cloned());
        ids.extend(self.positions.keys().cloned());
        ids.into_iter()
            .map(|id| TreeNode {
                parent: self.parent(&id).cloned(),
                label: self.label(&id),
                position: self.position(&id),
                id,
            })
            .collect()
    }

    /// Whether `a` is an ancestor of `b` in the tree as it stands now.
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        is_ancestor(&self.parent, a, b)
    }

    /// Rebuilds `parent` from scratch by replaying `log` in timestamp order.
    /// Skips any move that would create a loop: a node under itself, or under
    /// one of its own descendants.
    fn refold(&mut self) {
        self.parent.clear();
        for mv in self.log.values() {
            if mv.child == mv.new_parent {
                continue;
            }
            if is_ancestor(&self.parent, &mv.child, &mv.new_parent) {
                continue; // `child` is above `new_parent`, so this move would make a loop
            }
            self.parent.insert(mv.child.clone(), mv.new_parent.clone());
        }
    }
}

/// Puts a move into a log. Two different moves should never share an id, but a
/// clock that was reset or a machine id used twice can make it happen. The log
/// then keeps the one that compares higher by child and then by new parent.
/// Every machine applies the same rule, so they all keep the same move
/// whichever one reached them first.
fn keep_higher(log: &mut BTreeMap<OpId, Move>, id: OpId, mv: Move) {
    match log.get_mut(&id) {
        Some(held) => {
            if mv > *held {
                *held = mv;
            }
        }
        None => {
            log.insert(id, mv);
        }
    }
}

/// Walks up from `b` one parent at a time and returns true if it reaches `a`,
/// which means `a` is an ancestor of `b`. The walk always ends, because
/// `parent` never contains a loop. The `seen` set is an extra safeguard that
/// stops the walk if the map were ever malformed.
fn is_ancestor(parent: &BTreeMap<NodeId, NodeId>, a: &str, b: &str) -> bool {
    let mut cur = b;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    while let Some(p) = parent.get(cur) {
        if p.as_str() == a {
            return true;
        }
        if !seen.insert(p.as_str()) {
            break;
        }
        cur = p.as_str();
    }
    false
}

impl DeltaCrdt for Tree {
    type Delta = TreeDelta;

    fn merge(&mut self, delta: TreeDelta) {
        for (id, mv) in delta.moves {
            keep_higher(&mut self.log, id, mv);
        }
        self.refold();
        if let Some(d) = delta.labels {
            self.labels.merge(d);
        }
        if let Some(d) = delta.positions {
            self.positions.merge(d);
        }
    }

    fn snapshot(&self) -> TreeDelta {
        TreeDelta {
            moves: self.log.clone(),
            labels: Some(self.labels.snapshot()),
            positions: Some(self.positions.snapshot()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn hlc(w: u64, l: u32) -> Hlc {
        Hlc { wall: w, logical: l }
    }

    // A tree has no loop exactly when walking up from every node comes to an end.
    fn has_cycle(t: &Tree) -> bool {
        for start in t.nodes() {
            let mut cur = start.as_str();
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            seen.insert(cur);
            while let Some(p) = t.parent(cur) {
                if !seen.insert(p.as_str()) {
                    return true;
                }
                cur = p.as_str();
            }
        }
        false
    }

    #[test]
    fn concurrent_swap_converges_cycle_free() {
        // Put a and b under root on both replicas.
        let mut r1 = Tree::default();
        let mut r2 = Tree::default();
        let da = r1.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        let db = r1.move_node("b".into(), "root".into(), hlc(2, 0), ReplicaId(1));
        r2.merge(da);
        r2.merge(db);
        // Two conflicting moves made at the same time. The timestamps are
        // equal, so the replica id decides which one is replayed first.
        let m1 = r1.move_node("a".into(), "b".into(), hlc(5, 0), ReplicaId(1));
        let m2 = r2.move_node("b".into(), "a".into(), hlc(5, 0), ReplicaId(2));
        r1.merge(m2);
        r2.merge(m1);
        assert_eq!(r1, r2, "replicas converge");
        assert!(!has_cycle(&r1), "no cycle");
        // Replay order: (5,R1) a->b is applied first. Then (5,R2) b->a would
        // put b under a while a is already under b. That is a loop, so it is
        // skipped, and a->b wins.
        assert_eq!(r1.parent("a"), Some(&"b".to_string()));
        assert_eq!(r1.parent("b"), Some(&"root".to_string()));
    }

    #[test]
    fn self_parent_is_rejected() {
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.move_node("a".into(), "a".into(), hlc(2, 0), ReplicaId(1));
        assert_eq!(t.parent("a"), Some(&"root".to_string()));
    }

    #[test]
    fn moving_under_own_descendant_is_rejected() {
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.move_node("b".into(), "a".into(), hlc(2, 0), ReplicaId(1));
        t.move_node("a".into(), "b".into(), hlc(3, 0), ReplicaId(1)); // a under its own child b, so it is skipped
        assert_eq!(t.parent("a"), Some(&"root".to_string()));
        assert_eq!(t.parent("b"), Some(&"a".to_string()));
        assert!(!has_cycle(&t));
    }

    #[test]
    fn label_round_trips() {
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.set_label("a".into(), "Alpha".into(), hlc(2, 0), ReplicaId(1)).unwrap();
        assert_eq!(t.label("a").as_deref(), Some("Alpha"));
        assert_eq!(t.label("missing"), None);
    }

    #[test]
    fn concurrent_labels_resolve_lww() {
        // Same timestamp, different replicas. The writer's replica id breaks
        // the tie, so the higher ReplicaId wins on both replicas.
        let mut r1 = Tree::default();
        let mut r2 = Tree::default();
        let d1 = r1.set_label("a".into(), "Alpha".into(), hlc(5, 0), ReplicaId(1)).unwrap();
        let d2 = r2.set_label("a".into(), "Beta".into(), hlc(5, 0), ReplicaId(2)).unwrap();
        r1.merge(d2);
        r2.merge(d1);
        assert_eq!(r1.label("a").as_deref(), Some("Beta"));
        assert_eq!(r2.label("a").as_deref(), Some("Beta"));
        assert_eq!(r1, r2, "replicas converge");
    }

    #[test]
    fn a_move_and_a_label_on_one_node_are_independent() {
        // r1 moves x to a new parent while r2, which has not seen that move,
        // names it. Both changes survive the merge. Moves and labels are stored
        // separately, so neither overwrites the other.
        let mut r1 = Tree::default();
        let seed = r1.move_node("x".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        let mut r2 = Tree::default();
        r2.merge(seed);

        let mv = r1.move_node("x".into(), "box".into(), hlc(5, 0), ReplicaId(1));
        let lb = r2.set_label("x".into(), "Notes".into(), hlc(5, 0), ReplicaId(2)).unwrap();
        r1.merge(lb);
        r2.merge(mv);

        assert_eq!(r1, r2, "replicas converge");
        assert_eq!(r1.parent("x"), Some(&"box".to_string()));
        assert_eq!(r1.label("x").as_deref(), Some("Notes"));
        assert!(!has_cycle(&r1));
    }

    #[test]
    fn a_label_survives_a_reparent() {
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.set_label("a".into(), "Alpha".into(), hlc(2, 0), ReplicaId(1)).unwrap();
        t.move_node("a".into(), "other".into(), hlc(3, 0), ReplicaId(1));
        assert_eq!(t.parent("a"), Some(&"other".to_string()));
        assert_eq!(t.label("a").as_deref(), Some("Alpha"), "the label is not move-derived");
    }

    #[test]
    fn a_label_needs_no_node_and_nodes_stays_structural() {
        // A label does not depend on the node being in the tree. You can label
        // an id that has never been moved and read the label back, but that
        // does not add a node to the tree.
        let mut t = Tree::default();
        t.set_label("ghost".into(), "Ghost".into(), hlc(1, 0), ReplicaId(1)).unwrap();
        assert_eq!(t.label("ghost").as_deref(), Some("Ghost"));
        assert!(t.nodes().is_empty(), "nodes() is move-derived");
    }

    #[test]
    fn snapshot_carries_labels_and_merge_is_idempotent() {
        let mut src = Tree::default();
        src.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        src.set_label("a".into(), "Alpha".into(), hlc(2, 0), ReplicaId(1)).unwrap();
        let snap = src.snapshot();
        let mut dst = Tree::default();
        dst.merge(snap.clone());
        assert_eq!(dst, src, "snapshot reconstructs the whole product");
        dst.merge(snap);
        assert_eq!(dst, src, "merge is idempotent");
    }

    #[test]
    fn to_nodes_projects_structure_and_labels() {
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.move_node("b".into(), "a".into(), hlc(2, 0), ReplicaId(1));
        t.set_label("a".into(), "Alpha".into(), hlc(3, 0), ReplicaId(1)).unwrap();
        let nodes = t.to_nodes();
        // Sorted by id, with exactly one entry per id even when a node has been
        // both moved and labelled.
        assert_eq!(
            nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "root"]
        );
        assert_eq!(
            nodes[0],
            TreeNode {
                id: "a".into(),
                parent: Some("root".into()),
                label: Some("Alpha".into()),
                position: None
            }
        );
        assert_eq!(
            nodes[1],
            TreeNode { id: "b".into(), parent: Some("a".into()), label: None, position: None }
        );
        assert_eq!(
            nodes[2],
            TreeNode { id: "root".into(), parent: None, label: None, position: None },
            "a root is projected with no parent"
        );
    }

    #[test]
    fn to_nodes_changes_on_a_rename() {
        // A write that only changes a label must change the result of to_nodes,
        // even though the shape of the tree stays the same. Code that watches
        // the tree by comparing results relies on this. A list of (child,
        // parent) pairs would not show the change.
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.set_label("a".into(), "Alpha".into(), hlc(2, 0), ReplicaId(1)).unwrap();
        let before = t.to_nodes();
        t.set_label("a".into(), "Renamed".into(), hlc(3, 0), ReplicaId(1)).unwrap();
        let after = t.to_nodes();
        assert_ne!(before, after, "a rename must change the projection");
        assert_eq!(t.children("root"), vec!["a".to_string()], "structure is unchanged");
    }

    #[test]
    fn to_nodes_includes_label_only_ids() {
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.set_label("ghost".into(), "Ghost".into(), hlc(2, 0), ReplicaId(1)).unwrap();
        let nodes = t.to_nodes();
        assert_eq!(
            nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "ghost", "root"]
        );
        let ghost = nodes.iter().find(|n| n.id == "ghost").expect("a label-only id is projected");
        assert_eq!(ghost.parent, None, "never moved → no parent");
        assert_eq!(ghost.label.as_deref(), Some("Ghost"));
        assert!(
            !t.nodes().contains(&"ghost".to_string()),
            "nodes() stays structural — to_nodes() is the richer view"
        );
    }

    #[test]
    fn move_node_at_places_front_middle_and_end() {
        let mut t = Tree::default();
        t.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        t.move_node_at("b".into(), "root".into(), 1, hlc(2, 0), ReplicaId(1)).unwrap();
        assert_eq!(t.children("root"), vec!["a".to_string(), "b".to_string()]);
        // Front.
        t.move_node_at("c".into(), "root".into(), 0, hlc(3, 0), ReplicaId(1)).unwrap();
        assert_eq!(t.children("root"), vec!["c".to_string(), "a".to_string(), "b".to_string()]);
        // Middle.
        t.move_node_at("d".into(), "root".into(), 2, hlc(4, 0), ReplicaId(1)).unwrap();
        assert_eq!(
            t.children("root"),
            vec!["c".to_string(), "a".to_string(), "d".to_string(), "b".to_string()]
        );
        // End, via an index past the end (clamped).
        t.move_node_at("e".into(), "root".into(), 99, hlc(5, 0), ReplicaId(1)).unwrap();
        assert_eq!(
            t.children("root"),
            vec!["c".to_string(), "a".to_string(), "d".to_string(), "b".to_string(), "e".to_string()]
        );
    }

    #[test]
    fn reordering_changes_order_without_changing_parents() {
        let mut t = Tree::default();
        t.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        t.move_node_at("b".into(), "root".into(), 1, hlc(2, 0), ReplicaId(1)).unwrap();
        t.move_node_at("c".into(), "root".into(), 2, hlc(3, 0), ReplicaId(1)).unwrap();
        // Send c to the front. Its parent does not change.
        t.move_node_at("c".into(), "root".into(), 0, hlc(4, 0), ReplicaId(1)).unwrap();
        assert_eq!(t.children("root"), vec!["c".to_string(), "a".to_string(), "b".to_string()]);
        assert_eq!(t.parent("c"), Some(&"root".to_string()));
        assert_eq!(t.parent("a"), Some(&"root".to_string()));
    }

    #[test]
    fn a_tree_that_never_orders_keeps_id_order() {
        // Ordering is opt-in. Plain move_node assigns no position, so siblings
        // come back in plain id order.
        let mut t = Tree::default();
        t.move_node("c".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.move_node("a".into(), "root".into(), hlc(2, 0), ReplicaId(1));
        t.move_node("b".into(), "root".into(), hlc(3, 0), ReplicaId(1));
        assert_eq!(t.children("root"), vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(t.position("a"), None, "move_node assigns no position");
    }

    #[test]
    fn concurrent_reorders_of_one_node_converge_by_lww() {
        let mut r1 = Tree::default();
        let seed_a = r1.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        let seed_b = r1.move_node_at("b".into(), "root".into(), 1, hlc(2, 0), ReplicaId(1)).unwrap();
        let seed_c = r1.move_node_at("c".into(), "root".into(), 2, hlc(3, 0), ReplicaId(1)).unwrap();
        let mut r2 = Tree::default();
        r2.merge(seed_a);
        r2.merge(seed_b);
        r2.merge(seed_c);
        // Same node, two different destinations, same timestamp.
        let d1 = r1.move_node_at("c".into(), "root".into(), 0, hlc(9, 0), ReplicaId(1)).unwrap();
        let d2 = r2.move_node_at("c".into(), "root".into(), 1, hlc(9, 0), ReplicaId(2)).unwrap();
        r1.merge(d2);
        r2.merge(d1);
        assert_eq!(r1, r2, "replicas converge");
        assert_eq!(r1.children("root"), r2.children("root"), "on one sibling order");
    }

    #[test]
    fn concurrent_placements_into_the_same_gap_converge() {
        let mut r1 = Tree::default();
        let sa = r1.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        let sb = r1.move_node_at("b".into(), "root".into(), 1, hlc(2, 0), ReplicaId(1)).unwrap();
        let mut r2 = Tree::default();
        r2.merge(sa);
        r2.merge(sb);
        // Two different nodes dropped into the same slot, unaware of each other.
        let d1 = r1.move_node_at("x".into(), "root".into(), 1, hlc(5, 0), ReplicaId(1)).unwrap();
        let d2 = r2.move_node_at("y".into(), "root".into(), 1, hlc(5, 0), ReplicaId(2)).unwrap();
        r1.merge(d2);
        r2.merge(d1);
        assert_eq!(r1, r2, "replicas converge");
        let order = r1.children("root");
        assert_eq!(order.len(), 4, "both placements survive: {order:?}");
        assert_eq!(order.first(), Some(&"a".to_string()));
        assert_eq!(order.last(), Some(&"b".to_string()));
    }

    #[test]
    fn a_position_survives_a_reparent() {
        let mut t = Tree::default();
        t.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        let pos = t.position("a").expect("placed nodes carry a position");
        t.move_node("a".into(), "other".into(), hlc(2, 0), ReplicaId(1));
        assert_eq!(t.parent("a"), Some(&"other".to_string()));
        assert_eq!(t.position("a").as_deref(), Some(pos.as_str()), "position is not move-derived");
    }

    #[test]
    fn to_nodes_reflects_a_reorder() {
        // A move that only reorders siblings must change the result of
        // to_nodes. Code that watches the tree and reports only when the result
        // differs would otherwise miss the reorder.
        let mut t = Tree::default();
        t.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        t.move_node_at("b".into(), "root".into(), 1, hlc(2, 0), ReplicaId(1)).unwrap();
        let before = t.to_nodes();
        t.move_node_at("b".into(), "root".into(), 0, hlc(3, 0), ReplicaId(1)).unwrap();
        let after = t.to_nodes();
        assert_ne!(before, after, "a reorder must change the projection");
        assert_eq!(t.children("root"), vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn placing_among_unplaced_siblings_lands_after_them() {
        // An unplaced sibling sorts as the key "" and a generated key is never
        // empty, so a placed node cannot sort before an unplaced one. This
        // follows from how keys work. The requested index is approximate
        // whenever move_node and move_node_at are mixed.
        let mut t = Tree::default();
        t.move_node("z".into(), "root".into(), hlc(1, 0), ReplicaId(1));
        t.move_node_at("a".into(), "root".into(), 0, hlc(2, 0), ReplicaId(1)).unwrap();
        assert_eq!(
            t.children("root"),
            vec!["z".to_string(), "a".to_string()],
            "the placed node lands after the unplaced one despite index 0"
        );
        // Ask for the front again. The neighbour at index 0 is z, which has no
        // position, so the key is made as if there were no neighbour on that
        // side and comes out equal to a's. Equal keys are ordered by id, so the
        // order is still definite and repeatable, but it is not the one
        // requested.
        t.move_node_at("b".into(), "root".into(), 0, hlc(3, 0), ReplicaId(1)).unwrap();
        assert_eq!(
            t.children("root"),
            vec!["z".to_string(), "a".to_string(), "b".to_string()],
            "an unplaced boundary neighbor makes the index approximate"
        );
        assert_eq!(t.position("a"), t.position("b"), "both keys were generated against an open boundary");
    }

    #[test]
    fn reordering_a_sibling_to_the_end_excludes_its_own_stale_position() {
        // Moving b to the last slot must work out its new key from c, the last
        // of the other siblings, and not from b's own current position. If b
        // were not left out of the sibling list, the slot would be between b
        // and c, and b would land back in the middle.
        let mut t = Tree::default();
        t.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        t.move_node_at("b".into(), "root".into(), 1, hlc(2, 0), ReplicaId(1)).unwrap();
        t.move_node_at("c".into(), "root".into(), 2, hlc(3, 0), ReplicaId(1)).unwrap();
        t.move_node_at("b".into(), "root".into(), 2, hlc(4, 0), ReplicaId(1)).unwrap();
        assert_eq!(
            t.children("root"),
            vec!["a".to_string(), "c".to_string(), "b".to_string()],
            "b moved to the end, not back to the middle"
        );
    }

    #[test]
    fn a_cycle_rejected_placement_has_no_side_effect() {
        let mut t = Tree::default();
        t.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        t.move_node_at("b".into(), "root".into(), 1, hlc(2, 0), ReplicaId(1)).unwrap();
        t.move_node_at("c".into(), "root".into(), 2, hlc(3, 0), ReplicaId(1)).unwrap();
        t.move_node_at("d".into(), "a".into(), 0, hlc(4, 0), ReplicaId(1)).unwrap();
        let before_position = t.position("a");
        let before_order = t.children("root");
        // Moving a under its own descendant d would create a loop, so the
        // replay skips it. The position must not be set either.
        let delta = t.move_node_at("a".into(), "d".into(), 0, hlc(5, 0), ReplicaId(1)).unwrap();
        assert_eq!(t.parent("a"), Some(&"root".to_string()), "the move is rejected");
        assert_eq!(t.position("a"), before_position, "the position is untouched");
        assert_eq!(t.children("root"), before_order, "no silent reorder");
        assert!(delta.positions().is_none(), "the delta carries no position write");
    }

    #[test]
    fn to_nodes_includes_position_only_ids() {
        // Position changes can arrive without the move they belong to, when a
        // peer sends only part of a delta. That leaves an id this tree knows
        // only by its position. to_nodes must still list it.
        let mut src = Tree::default();
        let placed = src.move_node_at("ghost".into(), "elsewhere".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(2, 0), ReplicaId(1));
        t.merge(TreeDelta::default().with_positions(placed.positions().cloned()));
        let nodes = t.to_nodes();
        assert_eq!(
            nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "ghost", "root"]
        );
        let ghost = nodes.iter().find(|n| n.id == "ghost").expect("a position-only id is projected");
        assert_eq!(ghost.parent, None, "the move never arrived → no parent");
        assert!(ghost.position.is_some(), "the position did arrive");
        assert!(
            !t.nodes().contains(&"ghost".to_string()),
            "nodes() stays structural — to_nodes() is the richer view"
        );
    }

    #[test]
    fn snapshot_carries_positions() {
        let mut src = Tree::default();
        src.move_node_at("a".into(), "root".into(), 0, hlc(1, 0), ReplicaId(1)).unwrap();
        src.move_node_at("b".into(), "root".into(), 0, hlc(2, 0), ReplicaId(1)).unwrap();
        let snap = src.snapshot();
        let mut dst = Tree::default();
        dst.merge(snap.clone());
        assert_eq!(dst, src, "snapshot reconstructs the whole product");
        assert_eq!(dst.children("root"), vec!["b".to_string(), "a".to_string()]);
        dst.merge(snap);
        assert_eq!(dst, src, "merge is idempotent");
    }

    // Two different moves should never share an id, but a clock that was reset
    // or a machine id used twice can make it happen. The machines must still
    // end up with the same tree, whichever move reaches them first.
    #[test]
    fn two_moves_with_the_same_id_still_converge() {
        let one = |child: &str, parent: &str| {
            TreeDelta::from_wire([(hlc(5, 0), ReplicaId(1), child.to_string(), parent.to_string())])
        };
        let mut r1 = Tree::default();
        r1.merge(one("a", "x"));
        r1.merge(one("a", "y"));
        let mut r2 = Tree::default();
        r2.merge(one("a", "y"));
        r2.merge(one("a", "x"));
        assert_eq!(r1, r2, "replicas converge");

        // The same holds when the clash comes from a local move.
        let mut r3 = Tree::default();
        r3.move_node("a".into(), "x".into(), hlc(5, 0), ReplicaId(1));
        r3.move_node("a".into(), "y".into(), hlc(5, 0), ReplicaId(1));
        let mut r4 = Tree::default();
        r4.move_node("a".into(), "y".into(), hlc(5, 0), ReplicaId(1));
        r4.move_node("a".into(), "x".into(), hlc(5, 0), ReplicaId(1));
        assert_eq!(r3, r4);
        assert_eq!(r3, r1);
    }

    // The same clash, with both moves packed into one delta.
    #[test]
    fn two_moves_with_the_same_id_in_one_delta_still_converge() {
        let op = |child: &str| (hlc(5, 0), ReplicaId(1), child.to_string(), "p".to_string());
        let mut together = Tree::default();
        together.merge(TreeDelta::from_wire([op("z"), op("a")]));
        let mut flipped = Tree::default();
        flipped.merge(TreeDelta::from_wire([op("a"), op("z")]));
        let mut apart = Tree::default();
        apart.merge(TreeDelta::from_wire([op("z")]));
        apart.merge(TreeDelta::from_wire([op("a")]));
        assert_eq!(together, apart);
        assert_eq!(flipped, apart);
    }

    proptest! {
        // Merging the same set of moves in opposite orders gives the same tree,
        // and that tree never has a loop.
        #[test]
        fn order_independent_and_acyclic(indices in prop::collection::vec(0usize..6, 1..40)) {
            let pool: Vec<(OpId, Move)> = (0..6u64).map(|i| {
                let child = format!("n{}", i % 4);
                let new_parent = format!("n{}", (i + 1) % 4);
                ((hlc(i + 1, 0), ReplicaId(i % 3 + 1)), Move { child, new_parent })
            }).collect();
            let mut chosen: BTreeMap<OpId, Move> = BTreeMap::new();
            for &idx in &indices {
                let (id, mv) = pool[idx].clone();
                chosen.insert(id, mv);
            }
            let one = |id: OpId, mv: Move| TreeDelta { moves: [(id, mv)].into_iter().collect(), labels: None, positions: None };
            let mut fwd = Tree::default();
            for (id, mv) in chosen.iter() { fwd.merge(one(*id, mv.clone())); }
            let mut rev = Tree::default();
            for (id, mv) in chosen.iter().rev() { rev.merge(one(*id, mv.clone())); }
            prop_assert_eq!(&fwd, &rev);
            prop_assert!(!has_cycle(&fwd));
        }
    }

    // A placed move needs a dot for its position. With none left it is
    // refused before the node moves, so the move does not happen without
    // its position.
    #[test]
    fn a_placed_move_with_no_fresh_dot_is_refused_and_moves_nothing() {
        let r = ReplicaId(1);
        let mut t = Tree::default();
        t.move_node("a".into(), "root".into(), hlc(1, 0), r);
        let full = OrMapDelta::from_parts(BTreeMap::new(), BTreeMap::new(), crate::context::seen_up_to(r, crate::MAX_COUNTER));
        t.merge(TreeDelta { moves: BTreeMap::new(), labels: Some(full.clone()), positions: Some(full) });
        let before = t.clone();
        assert_eq!(t.move_node_at("b".into(), "root".into(), 0, hlc(2, 0), r), Err(WriteError::NoFreshDot));
        assert_eq!(t.set_label("a".into(), "A".into(), hlc(3, 0), r), Err(WriteError::NoFreshDot));
        assert_eq!(t, before);
        assert_eq!(t.parent("b"), None);
    }
}
