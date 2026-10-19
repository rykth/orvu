//! What the sidecar counts about the calls it answers: each operation and
//! how it ended, the calls it turned away, how long each kind of call took,
//! how many watches were opened. The page of numbers shows them next to the engine's. Counting
//! takes no locks.

use crate::pb;
use orvu_runtime::metrics::{Histogram, LATENCY_BOUNDS};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Every operation, by the name it has in the message definitions. A client
/// sends it by the same name.
pub const OPS: [&str; 54] = [
    "declare",
    "counter_increment",
    "counter_decrement",
    "counter_value",
    "register_set",
    "register_get",
    "set_add",
    "set_remove",
    "set_contains",
    "set_elements",
    "map_put",
    "map_remove",
    "map_get",
    "map_keys",
    "cart_add",
    "cart_remove_qty",
    "cart_remove_item",
    "cart_quantity",
    "cart_items",
    "map_entries",
    "list_insert",
    "list_delete",
    "list_get",
    "list_len",
    "list_to_vec",
    "json_set",
    "json_remove",
    "json_get",
    "json_to_value",
    "json_insert",
    "richtext_insert",
    "richtext_delete",
    "richtext_format",
    "richtext_unformat",
    "richtext_text",
    "richtext_to_spans",
    "tree_move_node",
    "tree_move_node_at",
    "tree_set_label",
    "tree_children",
    "tree_to_nodes",
    "tree_parent",
    "bounded_increment",
    "bounded_try_decrement",
    "bounded_transfer",
    "bounded_value",
    "bounded_allowance",
    "search",
    "mv_map_put",
    "mv_map_remove",
    "mv_map_get",
    "mv_map_keys",
    "mv_map_entries",
    "richtext_insert_marked",
];

/// How an operation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It was carried out, or the value was read.
    Done,
    /// The engine said no and changed nothing, as a bounded counter does
    /// when there is not enough left.
    Refused,
    /// The engine could not do it, for example on a name that holds another
    /// type.
    Failed,
}

const ENDINGS: [&str; 3] = ["done", "refused", "failed"];

impl Ended {
    /// How the operation that got `response` ended.
    pub fn of(response: &pb::Response) -> Ended {
        match response.result {
            Some(pb::response::Result::Refused(_)) => Ended::Refused,
            Some(pb::response::Result::Failed(_)) => Ended::Failed,
            _ => Ended::Done,
        }
    }
}

/// The kinds of call a client makes that are counted. `Execute` and `Batch`
/// are each timed by themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Execute,
    Batch,
    Watch,
}

const CALLS: [&str; 3] = ["execute", "batch", "watch"];

/// The sidecar's counts. Share it through an `Arc`.
#[derive(Debug)]
pub struct Stats {
    operations: [[AtomicU64; 3]; OPS.len()],
    rejected: [AtomicU64; 3],
    execute: Histogram<12>,
    batch: Histogram<12>,
    watches: AtomicU64,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            operations: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
            rejected: std::array::from_fn(|_| AtomicU64::new(0)),
            execute: Histogram::new(LATENCY_BOUNDS, 1e6),
            batch: Histogram::new(LATENCY_BOUNDS, 1e6),
            watches: AtomicU64::new(0),
        }
    }
}

impl Stats {
    /// Counts one operation, by its place in `OPS` from `op_index`, and how
    /// it ended.
    pub fn operation(&self, index: usize, ended: Ended) {
        self.operations[index][ended as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a call the sidecar turned away: one with an unknown session,
    /// one that is itself broken, or a batch that is too large. It is counted
    /// once, whatever it held, and none of its operations is counted.
    pub fn rejected(&self, call: Call) {
        self.rejected[call as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Times one call. Only `Execute` and `Batch` are timed: a watch lasts
    /// for as long as the client keeps it.
    pub fn call(&self, call: Call, took: Duration) {
        match call {
            Call::Execute => self.execute.observe(took.as_secs_f64()),
            Call::Batch => self.batch.observe(took.as_secs_f64()),
            Call::Watch => {}
        }
    }

    /// Counts a watch that was opened.
    pub fn watch(&self) {
        self.watches.fetch_add(1, Ordering::Relaxed);
    }

    /// The counts as Prometheus text, with `connections`, the clients
    /// connected now. An operation that never happened has no line.
    pub fn render(&self, connections: usize) -> String {
        let mut out = String::from("# TYPE orvu_sidecar_operations_total counter\n");
        for (name, counts) in OPS.iter().zip(&self.operations) {
            for (ended, count) in ENDINGS.iter().zip(counts) {
                let count = count.load(Ordering::Relaxed);
                if count > 0 {
                    out.push_str(&format!(
                        "orvu_sidecar_operations_total{{op=\"{name}\",result=\"{ended}\"}} {count}\n"
                    ));
                }
            }
        }
        out.push_str("# TYPE orvu_sidecar_rejected_total counter\n");
        for (call, count) in CALLS.iter().zip(&self.rejected) {
            out.push_str(&format!(
                "orvu_sidecar_rejected_total{{call=\"{call}\"}} {}\n",
                count.load(Ordering::Relaxed)
            ));
        }
        out.push_str(&format!(
            "# TYPE orvu_sidecar_watches_total counter\norvu_sidecar_watches_total {}\n",
            self.watches.load(Ordering::Relaxed)
        ));
        out.push_str(&format!("# TYPE orvu_sidecar_connections gauge\norvu_sidecar_connections {connections}\n"));
        orvu_runtime::render_histogram(&mut out, "sidecar_execute_seconds", &self.execute.snapshot(), LATENCY_BOUNDS);
        orvu_runtime::render_histogram(&mut out, "sidecar_batch_seconds", &self.batch.snapshot(), LATENCY_BOUNDS);
        out
    }
}

/// The name of an operation, as in `OPS`.
pub fn op_name(op: &pb::request::Op) -> &'static str {
    OPS[op_index(op)]
}

/// The place of an operation in `OPS`. A `match` that left one out would
/// not build, and a test checks each place against the name.
pub fn op_index(op: &pb::request::Op) -> usize {
    use pb::request::Op;
    match op {
        Op::Declare(_) => 0,
        Op::CounterIncrement(_) => 1,
        Op::CounterDecrement(_) => 2,
        Op::CounterValue(_) => 3,
        Op::RegisterSet(_) => 4,
        Op::RegisterGet(_) => 5,
        Op::SetAdd(_) => 6,
        Op::SetRemove(_) => 7,
        Op::SetContains(_) => 8,
        Op::SetElements(_) => 9,
        Op::MapPut(_) => 10,
        Op::MapRemove(_) => 11,
        Op::MapGet(_) => 12,
        Op::MapKeys(_) => 13,
        Op::CartAdd(_) => 14,
        Op::CartRemoveQty(_) => 15,
        Op::CartRemoveItem(_) => 16,
        Op::CartQuantity(_) => 17,
        Op::CartItems(_) => 18,
        Op::MapEntries(_) => 19,
        Op::ListInsert(_) => 20,
        Op::ListDelete(_) => 21,
        Op::ListGet(_) => 22,
        Op::ListLen(_) => 23,
        Op::ListToVec(_) => 24,
        Op::JsonSet(_) => 25,
        Op::JsonRemove(_) => 26,
        Op::JsonGet(_) => 27,
        Op::JsonToValue(_) => 28,
        Op::JsonInsert(_) => 29,
        Op::RichtextInsert(_) => 30,
        Op::RichtextDelete(_) => 31,
        Op::RichtextFormat(_) => 32,
        Op::RichtextUnformat(_) => 33,
        Op::RichtextText(_) => 34,
        Op::RichtextToSpans(_) => 35,
        Op::TreeMoveNode(_) => 36,
        Op::TreeMoveNodeAt(_) => 37,
        Op::TreeSetLabel(_) => 38,
        Op::TreeChildren(_) => 39,
        Op::TreeToNodes(_) => 40,
        Op::TreeParent(_) => 41,
        Op::BoundedIncrement(_) => 42,
        Op::BoundedTryDecrement(_) => 43,
        Op::BoundedTransfer(_) => 44,
        Op::BoundedValue(_) => 45,
        Op::BoundedAllowance(_) => 46,
        Op::Search(_) => 47,
        Op::MvMapPut(_) => 48,
        Op::MvMapRemove(_) => 49,
        Op::MvMapGet(_) => 50,
        Op::MvMapKeys(_) => 51,
        Op::MvMapEntries(_) => 52,
        Op::RichtextInsertMarked(_) => 53,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each operation is counted under its own name, and not its neighbour's.
    #[test]
    fn each_operation_has_its_own_place() {
        use pb::request::Op;
        let every = [
            (Op::Declare(Default::default()), "declare"),
            (Op::CounterIncrement(Default::default()), "counter_increment"),
            (Op::CounterDecrement(Default::default()), "counter_decrement"),
            (Op::CounterValue(Default::default()), "counter_value"),
            (Op::RegisterSet(Default::default()), "register_set"),
            (Op::RegisterGet(Default::default()), "register_get"),
            (Op::SetAdd(Default::default()), "set_add"),
            (Op::SetRemove(Default::default()), "set_remove"),
            (Op::SetContains(Default::default()), "set_contains"),
            (Op::SetElements(Default::default()), "set_elements"),
            (Op::MapPut(Default::default()), "map_put"),
            (Op::MapRemove(Default::default()), "map_remove"),
            (Op::MapGet(Default::default()), "map_get"),
            (Op::MapKeys(Default::default()), "map_keys"),
            (Op::CartAdd(Default::default()), "cart_add"),
            (Op::CartRemoveQty(Default::default()), "cart_remove_qty"),
            (Op::CartRemoveItem(Default::default()), "cart_remove_item"),
            (Op::CartQuantity(Default::default()), "cart_quantity"),
            (Op::CartItems(Default::default()), "cart_items"),
            (Op::MapEntries(Default::default()), "map_entries"),
            (Op::ListInsert(Default::default()), "list_insert"),
            (Op::ListDelete(Default::default()), "list_delete"),
            (Op::ListGet(Default::default()), "list_get"),
            (Op::ListLen(Default::default()), "list_len"),
            (Op::ListToVec(Default::default()), "list_to_vec"),
            (Op::JsonSet(Default::default()), "json_set"),
            (Op::JsonRemove(Default::default()), "json_remove"),
            (Op::JsonGet(Default::default()), "json_get"),
            (Op::JsonToValue(Default::default()), "json_to_value"),
            (Op::JsonInsert(Default::default()), "json_insert"),
            (Op::RichtextInsert(Default::default()), "richtext_insert"),
            (Op::RichtextDelete(Default::default()), "richtext_delete"),
            (Op::RichtextFormat(Default::default()), "richtext_format"),
            (Op::RichtextUnformat(Default::default()), "richtext_unformat"),
            (Op::RichtextText(Default::default()), "richtext_text"),
            (Op::RichtextToSpans(Default::default()), "richtext_to_spans"),
            (Op::TreeMoveNode(Default::default()), "tree_move_node"),
            (Op::TreeMoveNodeAt(Default::default()), "tree_move_node_at"),
            (Op::TreeSetLabel(Default::default()), "tree_set_label"),
            (Op::TreeChildren(Default::default()), "tree_children"),
            (Op::TreeToNodes(Default::default()), "tree_to_nodes"),
            (Op::TreeParent(Default::default()), "tree_parent"),
            (Op::BoundedIncrement(Default::default()), "bounded_increment"),
            (Op::BoundedTryDecrement(Default::default()), "bounded_try_decrement"),
            (Op::BoundedTransfer(Default::default()), "bounded_transfer"),
            (Op::BoundedValue(Default::default()), "bounded_value"),
            (Op::BoundedAllowance(Default::default()), "bounded_allowance"),
            (Op::Search(Default::default()), "search"),
            (Op::MvMapPut(Default::default()), "mv_map_put"),
            (Op::MvMapRemove(Default::default()), "mv_map_remove"),
            (Op::MvMapGet(Default::default()), "mv_map_get"),
            (Op::MvMapKeys(Default::default()), "mv_map_keys"),
            (Op::MvMapEntries(Default::default()), "mv_map_entries"),
            (Op::RichtextInsertMarked(Default::default()), "richtext_insert_marked"),
        ];
        for (op, name) in &every {
            assert_eq!(op_name(op), *name);
        }
        assert_eq!(every.len(), OPS.len());
    }
}
