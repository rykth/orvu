//! Tests of syncing from start to finish.
//!
//! Deltas are turned into bytes and back, shuffled differently for each
//! replica, and delivered twice. That is the path they take over a real
//! network, without the network.

use orvu_core::{
    Applied, Codec, ProtobufCodec, Replica, SyncMessage, TypedCrdt, TypedDelta, WireDelta,
};
use orvu_crdt::{
    BoundedCounter, JsonCrdt, JsonValue, LwwRegister, OrMap, OrSet, PnCounter, ReplicaId, Rga,
    RichText, Seg, Tree,
};
use proptest::prelude::*;

const OBJECT: &str = "obj";
const BUFFER: usize = 4096;

/// Turns a delta into bytes and back, so the tests use the same encoding the
/// network does.
fn through_codec(delta: &WireDelta) -> WireDelta {
    let codec = ProtobufCodec;
    let bytes = codec.encode(&SyncMessage::DeltaBatch {
        namespace: "default".to_string(),
        deltas: vec![delta.clone()],
    });
    match codec.decode(&bytes).expect("round trip") {
        SyncMessage::DeltaBatch { mut deltas, .. } => deltas.remove(0),
        other => panic!("unexpected {other:?}"),
    }
}

/// Delivers `deltas` to `replica` in an order worked out from `keys`. Each one
/// is delivered twice, to check that the second copy is ignored.
///
/// Deltas that had to wait for others are offered again until none are left
/// waiting, the way a network that retries would.
fn deliver(replica: &mut Replica, deltas: &[WireDelta], keys: &[u64], salt: u64) {
    let mut order: Vec<usize> = (0..deltas.len()).collect();
    order.sort_by_key(|&i| keys.get(i).copied().unwrap_or(0) ^ salt);

    for _round in 0..3 {
        for &i in &order {
            let wire = through_codec(&deltas[i]);
            let _ = replica.receive(wire.clone(), 100);
            let _ = replica.receive(wire, 100); // the second copy
        }
        if replica.pending() == 0 {
            break;
        }
    }
}

fn counter_value(r: &Replica) -> i64 {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::PnCounter(pn)) => pn.value(),
        other => panic!("unexpected {other:?}"),
    }
}

fn set_elements(r: &Replica) -> Vec<String> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::OrSet(s)) => s.elements().cloned().collect(),
        other => panic!("unexpected {other:?}"),
    }
}

/// Every key present in the map with its value, in key order.
fn map_entries(r: &Replica) -> Vec<(String, Option<String>)> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::OrMap(m)) => m
            .keys()
            .map(|k| (k.clone(), m.get(k).and_then(|reg| reg.get().cloned())))
            .collect(),
        other => panic!("unexpected {other:?}"),
    }
}

/// Every item present in the cart with its count, in key order.
fn map_cart_entries(r: &Replica) -> Vec<(String, i64)> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::OrMapCounter(m)) => m
            .keys()
            .map(|k| (k.clone(), m.get(k).map(|pn| pn.value()).unwrap_or(0)))
            .collect(),
        other => panic!("unexpected {other:?}"),
    }
}

/// The list's visible items, in order.
fn list_contents(r: &Replica) -> Vec<String> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::Rga(l)) => l.to_vec(),
        other => panic!("unexpected {other:?}"),
    }
}

/// The JSON document as a plain value.
fn json_value(r: &Replica) -> JsonValue {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::Json(j)) => j.to_value(),
        other => panic!("unexpected {other:?}"),
    }
}

fn rich_text_str(r: &Replica) -> String {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::RichText(t)) => t.text(),
        other => panic!("unexpected {other:?}"),
    }
}

fn rich_text_spans(r: &Replica) -> Vec<(String, std::collections::BTreeMap<String, String>)> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::RichText(t)) => t.to_spans().into_iter().map(|s| (s.text, s.marks)).collect(),
        other => panic!("unexpected {other:?}"),
    }
}

fn bounded_value(r: &Replica) -> i64 {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::BoundedCounter(b)) => b.value(),
        other => panic!("unexpected {other:?}"),
    }
}

/// Every node with its current parent, in node order.
fn tree_parents(r: &Replica) -> Vec<(String, String)> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::Tree(t)) => t
            .nodes()
            .into_iter()
            .filter_map(|n| t.parent(&n).map(|p| (n.clone(), p.clone())))
            .collect(),
        other => panic!("unexpected {other:?}"),
    }
}

/// A tree has no loop exactly when walking up from every node comes to an end.
fn tree_has_cycle(r: &Replica) -> bool {
    let parents: std::collections::BTreeMap<String, String> =
        tree_parents(r).into_iter().collect();
    for start in parents.keys() {
        let mut cur = start.as_str();
        let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        seen.insert(cur);
        while let Some(p) = parents.get(cur) {
            if !seen.insert(p.as_str()) {
                return true;
            }
            cur = p.as_str();
        }
    }
    false
}

/// One node's current label.
fn tree_label(r: &Replica, node: &str) -> Option<String> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::Tree(t)) => t.label(node),
        other => panic!("unexpected {other:?}"),
    }
}

/// One node's children, in sibling order.
fn tree_children(r: &Replica, node: &str) -> Vec<String> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::Tree(t)) => t.children(node),
        other => panic!("unexpected {other:?}"),
    }
}

/// Everything the tree knows about every node, including its position among
/// its siblings.
fn tree_projection(r: &Replica) -> Vec<orvu_crdt::TreeNode> {
    match r.object(OBJECT).map(|o| o.crdt()) {
        Some(TypedCrdt::Tree(t)) => t.to_nodes(),
        other => panic!("unexpected {other:?}"),
    }
}

proptest! {
    /// Three replicas each change a counter on their own. Once every delta
    /// has reached everyone, in any order, they all agree.
    #[test]
    fn counters_converge_over_a_reordering_channel(
        ops in prop::collection::vec((0usize..3, 1u64..10, any::<u64>()), 1..30)
    ) {
        let mut authors: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(i as u64 + 1), BUFFER);
                r.create(OBJECT, TypedCrdt::PnCounter(PnCounter::default()));
                r
            })
            .collect();

        let mut deltas = Vec::new();
        let mut keys = Vec::new();
        for (who, amount, key) in &ops {
            let author = &mut authors[*who];
            let id = author.id();
            let d = author
                .mutate(OBJECT, 100, |crdt, _hlc, _replica| match crdt {
                    TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(id, *amount)),
                    other => panic!("unexpected {other:?}"),
                })
                .expect("object exists");
            deltas.push(d);
            keys.push(*key);
        }

        // New replicas each receive every delta, each in a different order.
        let mut observers: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
                r.create(OBJECT, TypedCrdt::PnCounter(PnCounter::default()));
                r
            })
            .collect();

        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let expected: i64 = ops.iter().map(|(_, amount, _)| *amount as i64).sum();
        for o in &observers {
            prop_assert_eq!(o.pending(), 0, "every delta was eventually applied");
            prop_assert_eq!(counter_value(o), expected);
        }
    }

    /// The same for an add-wins set, where the order of merging really matters.
    #[test]
    fn or_sets_converge_over_a_reordering_channel(
        ops in prop::collection::vec((0usize..3, 0u8..4, any::<bool>(), any::<u64>()), 1..30)
    ) {
        let mut authors: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(i as u64 + 1), BUFFER);
                r.create(OBJECT, TypedCrdt::OrSet(OrSet::default()));
                r
            })
            .collect();

        let mut deltas = Vec::new();
        let mut keys = Vec::new();
        for (who, elem, is_add, key) in &ops {
            let author = &mut authors[*who];
            let id = author.id();
            let element = format!("e{elem}");
            let d = author
                .mutate(OBJECT, 100, |crdt, _hlc, _replica| match crdt {
                    TypedCrdt::OrSet(s) => TypedDelta::OrSet(if *is_add {
                        s.add(element.clone(), id).unwrap()
                    } else {
                        s.remove(&element)
                    }),
                    other => panic!("unexpected {other:?}"),
                })
                .expect("object exists");
            deltas.push(d);
            keys.push(*key);
        }

        let mut observers: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
                r.create(OBJECT, TypedCrdt::OrSet(OrSet::default()));
                r
            })
            .collect();

        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let first = set_elements(&observers[0]);
        for o in &observers[1..] {
            prop_assert_eq!(o.pending(), 0);
            prop_assert_eq!(set_elements(o), first.clone());
        }
    }

    /// The same for a map of last-writer-wins values. Writes to the same key
    /// at the same time make it pick a winner. Writes to different keys must
    /// all survive.
    #[test]
    fn or_map_converges_over_a_reordering_channel(
        ops in prop::collection::vec((0usize..3, 0u8..3, 0u8..4, any::<u64>()), 1..30)
    ) {
        let mut authors: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(i as u64 + 1), BUFFER);
                r.create(OBJECT, TypedCrdt::OrMap(OrMap::<String, LwwRegister<String>>::default()));
                r
            })
            .collect();

        let mut deltas = Vec::new();
        let mut keys = Vec::new();
        for (who, key, val, shuffle) in &ops {
            let author = &mut authors[*who];
            let id = author.id();
            // There are only 3 keys for 3 replicas, so some writes hit the
            // same key and some hit different ones.
            let k = format!("k{key}");
            let v = format!("v{who}-{val}");
            let d = author
                .mutate(OBJECT, 100, |crdt, hlc, replica| match crdt {
                    TypedCrdt::OrMap(m) => TypedDelta::OrMap(
                        m.update(k.clone(), id, |reg| reg.set(v.clone(), hlc, replica)).unwrap(),
                    ),
                    other => panic!("unexpected {other:?}"),
                })
                .expect("object exists");
            deltas.push(d);
            keys.push(*shuffle);
        }

        let mut observers: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
                r.create(OBJECT, TypedCrdt::OrMap(OrMap::<String, LwwRegister<String>>::default()));
                r
            })
            .collect();

        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let first = map_entries(&observers[0]);
        prop_assert!(!first.is_empty(), "every op was a put, so keys exist");
        prop_assert!(first.iter().all(|(_, v)| v.is_some()), "every live key has a value");
        for o in &observers[1..] {
            prop_assert_eq!(o.pending(), 0, "every delta was eventually applied");
            prop_assert_eq!(map_entries(o), first.clone());
        }
    }

    /// Three replicas change a cart (`OrMap<String, PnCounter>`) at the same
    /// time, adding to and taking from the same items and different ones. Once
    /// every delta has gone through the encoding and reached everyone, in any
    /// order, they all agree.
    #[test]
    fn or_map_counter_converges_over_a_reordering_channel(
        ops in prop::collection::vec((0usize..3, 0u8..3, prop::bool::ANY, 1u64..5, any::<u64>()), 1..30)
    ) {
        let mut authors: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(i as u64 + 1), BUFFER);
                r.create(OBJECT, TypedCrdt::OrMapCounter(OrMap::<String, PnCounter>::default()));
                r
            })
            .collect();

        let mut deltas = Vec::new();
        let mut keys = Vec::new();
        for (who, item, is_dec, qty, shuffle) in &ops {
            let author = &mut authors[*who];
            // There are only 3 items, so some changes hit the same item and
            // some hit different ones.
            let k = format!("item{item}");
            let dec = *is_dec;
            let n = *qty;
            let d = author
                .mutate(OBJECT, 100, |crdt, _hlc, replica| match crdt {
                    TypedCrdt::OrMapCounter(m) => TypedDelta::OrMapCounter(m.update(
                        k.clone(),
                        replica,
                        |pn| if dec { pn.decrement(replica, n) } else { pn.increment(replica, n) },
                    ).unwrap()),
                    other => panic!("unexpected {other:?}"),
                })
                .expect("object exists");
            deltas.push(d);
            keys.push(*shuffle);
        }

        let mut observers: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
                r.create(OBJECT, TypedCrdt::OrMapCounter(OrMap::<String, PnCounter>::default()));
                r
            })
            .collect();

        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let first = map_cart_entries(&observers[0]);
        for o in &observers[1..] {
            prop_assert_eq!(o.pending(), 0, "every delta was eventually applied");
            prop_assert_eq!(map_cart_entries(o), first.clone());
        }
    }

    /// Three replicas insert into and delete from a list (`Rga<String>`) at
    /// the same time. Once every delta has gone through the encoding and
    /// reached everyone, in any order, they all show the same items in the
    /// same order.
    #[test]
    fn rga_converges_over_a_reordering_channel(
        ops in prop::collection::vec((0usize..3, 0usize..6, prop::bool::ANY, any::<u64>()), 1..30)
    ) {
        let mut authors: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(i as u64 + 1), BUFFER);
                r.create(OBJECT, TypedCrdt::Rga(Rga::<String>::default()));
                r
            })
            .collect();

        let mut deltas = Vec::new();
        let mut keys = Vec::new();
        for (who, pos, is_delete, shuffle) in &ops {
            let author = &mut authors[*who];
            // Turn the index into a dot inside the closure, where the list is
            // the author's own current copy.
            let (pos, is_delete) = (*pos, *is_delete);
            let d = author
                .mutate(OBJECT, 100, |crdt, _hlc, replica| match crdt {
                    TypedCrdt::Rga(l) => {
                        let vis = l.visible_dots();
                        TypedDelta::Rga(if is_delete && !vis.is_empty() {
                            l.tombstone(vis[pos % vis.len()])
                        } else {
                            let pred = if vis.is_empty() || pos == 0 {
                                None
                            } else {
                                Some(vis[(pos - 1) % vis.len()])
                            };
                            l.insert_after(pred, format!("v{}-{}", replica.0, pos), replica).unwrap()
                        })
                    }
                    other => panic!("unexpected {other:?}"),
                })
                .expect("object exists");
            deltas.push(d);
            keys.push(*shuffle);
        }

        let mut observers: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
                r.create(OBJECT, TypedCrdt::Rga(Rga::<String>::default()));
                r
            })
            .collect();

        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let first = list_contents(&observers[0]);
        for o in &observers[1..] {
            prop_assert_eq!(o.pending(), 0, "every delta was eventually applied");
            prop_assert_eq!(list_contents(o), first.clone());
        }
    }

    /// Three replicas set and insert nested values in a JSON document at the
    /// same time. Once every delta has gone through the encoding and reached
    /// everyone, in any order, they all read the same document.
    #[test]
    fn json_converges_over_a_reordering_channel(
        ops in prop::collection::vec((0usize..3, 0u8..3, 0u8..2, 0i64..5, any::<u64>()), 1..25)
    ) {
        let mut authors: Vec<Replica> = (0..3).map(|i| {
            let mut r = Replica::new(ReplicaId(i as u64 + 1), BUFFER);
            r.create(OBJECT, TypedCrdt::Json(JsonCrdt::default()));
            r
        }).collect();

        let mut deltas = Vec::new();
        let mut keys = Vec::new();
        for (idx, (who, k, op, val, shuffle)) in ops.iter().enumerate() {
            let author = &mut authors[*who];
            let d = author.mutate(OBJECT, idx as u64 + 1, |crdt, hlc, replica| match crdt {
                TypedCrdt::Json(j) => {
                    let seg = Seg::Key(format!("k{k}"));
                    let d = if *op == 0 {
                        j.set(&[seg], &JsonValue::Num(*val as f64), hlc, replica)
                    } else {
                        j.set(&[seg.clone(), Seg::Key("n".into())], &JsonValue::Str(format!("v{val}")), hlc, replica)
                    };
                    TypedDelta::Json(d.expect("shallow set never errors"))
                }
                other => panic!("unexpected {other:?}"),
            }).expect("object exists");
            deltas.push(d);
            keys.push(*shuffle);
        }

        let mut observers: Vec<Replica> = (0..3).map(|i| {
            let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
            r.create(OBJECT, TypedCrdt::Json(JsonCrdt::default()));
            r
        }).collect();

        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let first = json_value(&observers[0]);
        for o in &observers[1..] {
            prop_assert_eq!(o.pending(), 0);
            prop_assert_eq!(json_value(o), first.clone());
        }
    }

    /// Three replicas type into and format a rich-text document. Once every
    /// delta has gone through the encoding and reached everyone, in any order,
    /// they all show the same spans.
    #[test]
    fn rich_text_converges_over_a_reordering_channel(
        ops in prop::collection::vec((0usize..3, 0usize..5, 0u8..2, 0u8..2, any::<u64>()), 1..25)
    ) {
        let mut authors: Vec<Replica> = (0..3).map(|i| {
            let mut r = Replica::new(ReplicaId(i as u64 + 1), BUFFER);
            r.create(OBJECT, TypedCrdt::RichText(RichText::default()));
            r
        }).collect();

        let mut deltas = Vec::new();
        let mut keys = Vec::new();
        for (idx, (who, pos, op, attr, shuffle)) in ops.iter().enumerate() {
            let author = &mut authors[*who];
            let a = format!("m{attr}");
            let d = author.mutate(OBJECT, idx as u64 + 1, |crdt, hlc, replica| match crdt {
                TypedCrdt::RichText(t) => TypedDelta::RichText(if *op == 0 {
                    t.insert_char(*pos, char::from(b'a' + (*pos as u8)), replica).unwrap()
                } else {
                    t.format(*pos, 2, &a, "1", hlc, replica).unwrap()
                }),
                other => panic!("unexpected {other:?}"),
            }).expect("object exists");
            deltas.push(d);
            keys.push(*shuffle);
        }

        let mut observers: Vec<Replica> = (0..3).map(|i| {
            let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
            r.create(OBJECT, TypedCrdt::RichText(RichText::default()));
            r
        }).collect();
        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let first_text = rich_text_str(&observers[0]);
        let first_spans = rich_text_spans(&observers[0]);
        for o in &observers[1..] {
            prop_assert_eq!(o.pending(), 0);
            prop_assert_eq!(rich_text_str(o), first_text.clone());
            prop_assert_eq!(rich_text_spans(o), first_spans.clone());
        }
    }

    /// When deltas are applied only after the ones they depend on, a bounded
    /// counter never reads below zero at any step, and all replicas end up the
    /// same. Replica r1 puts in the stock and hands some allowance to r2. Then
    /// r1 and r2 each withdraw within their own allowance.
    #[test]
    fn bounded_counter_stays_non_negative_over_a_reordering_channel(
        seed in 0u64..64, w1 in 0u64..6, w2 in 0u64..6, grant in 0u64..6, key in any::<u64>()
    ) {
        let stock = 8;
        let mut author = Replica::new(ReplicaId(1), BUFFER);
        author.create(OBJECT, TypedCrdt::BoundedCounter(BoundedCounter::default()));
        let _id = author.id();

        // Build a chain of deltas where each depends on the one before:
        // deposit the stock, hand some to r2, then r1 withdraws min(w1, what
        // it has left). r2's withdrawal is made on a second replica that has
        // received the transfer first.
        let mut deltas = Vec::new();
        let d_dep = author.mutate(OBJECT, 1, |c, _h, r| match c {
            TypedCrdt::BoundedCounter(b) => TypedDelta::BoundedCounter(b.increment(stock, r)),
            other => panic!("unexpected {other:?}"),
        }).unwrap();
        deltas.push(d_dep);
        let g = grant.min(stock);
        if g > 0 {
            let d_tr = author.mutate(OBJECT, 2, |c, _h, r| match c {
                TypedCrdt::BoundedCounter(b) => TypedDelta::BoundedCounter(b.transfer(ReplicaId(2), g, r).expect("has allowance")),
                other => panic!("unexpected {other:?}"),
            }).unwrap();
            deltas.push(d_tr);
        }
        // r1 withdraws within the allowance it has left. This is done on
        // `author`, which has applied its own deposit and transfer.
        let a1 = w1.min(stock - g);
        if a1 > 0 {
            let d_w1 = author.mutate(OBJECT, 3, |c, _h, r| match c {
                TypedCrdt::BoundedCounter(b) => TypedDelta::BoundedCounter(b.try_decrement(a1, r).expect("within allowance")),
                other => panic!("unexpected {other:?}"),
            }).unwrap();
            deltas.push(d_w1);
        }
        // r2 makes its withdrawal on a replica that has received the transfer.
        let a2 = w2.min(g);
        if a2 > 0 {
            let mut r2 = Replica::new(ReplicaId(2), BUFFER);
            r2.create(OBJECT, TypedCrdt::BoundedCounter(BoundedCounter::default()));
            for d in &deltas { let _ = r2.receive(through_codec(d), 100); } // r2 receives the deposit and the transfer
            let d_w2 = r2.mutate(OBJECT, 4, |c, _h, r| match c {
                TypedCrdt::BoundedCounter(b) => TypedDelta::BoundedCounter(b.try_decrement(a2, r).expect("within transferred allowance")),
                other => panic!("unexpected {other:?}"),
            }).unwrap();
            deltas.push(d_w2);
        }

        // Deliver everything to a new replica, letting early deltas wait in
        // the buffer, and check value >= 0 after every delta that is applied.
        let mut obs = Replica::new(ReplicaId(100), BUFFER);
        obs.create(OBJECT, TypedCrdt::BoundedCounter(BoundedCounter::default()));
        let mut order: Vec<usize> = (0..deltas.len()).collect();
        order.sort_by_key(|&i| key.rotate_left(i as u32));
        let _ = seed;
        for _round in 0..4 {
            for &i in &order {
                let _ = obs.receive(through_codec(&deltas[i]), 100);
                prop_assert!(bounded_value(&obs) >= 0, "never negative under causal delivery");
            }
            if obs.pending() == 0 { break; }
        }
        prop_assert_eq!(obs.pending(), 0);
        prop_assert_eq!(bounded_value(&obs), (stock as i64) - (a1 as i64) - (a2 as i64));
    }

    /// Two replicas swap parents at the same time: r1 moves a under b while
    /// r2, which has not seen that, moves b under a. The moves go through the
    /// real encoding, shuffled and delivered twice, and every replica ends up
    /// with the same tree, with no loop in it. A simple "latest write wins per
    /// node" rule would apply both moves, since they change different nodes,
    /// and make a loop.
    #[test]
    fn tree_converges_over_a_reordering_channel(
        keys in prop::collection::vec(any::<u64>(), 4)
    ) {
        let mut r1 = Replica::new(ReplicaId(1), BUFFER);
        r1.create(OBJECT, TypedCrdt::Tree(Tree::default()));
        let mut deltas = Vec::new();

        // r1 starts things off: a and b under root.
        for (i, node) in ["a", "b"].iter().enumerate() {
            let d = r1
                .mutate(OBJECT, i as u64 + 1, |crdt, hlc, replica| match crdt {
                    TypedCrdt::Tree(t) => {
                        TypedDelta::Tree(t.move_node(node.to_string(), "root".into(), hlc, replica))
                    }
                    other => panic!("unexpected {other:?}"),
                })
                .expect("object exists");
            deltas.push(d);
        }

        // r2 has seen the starting moves but not r1's swap move, and r1 has
        // not seen r2's.
        let mut r2 = Replica::new(ReplicaId(2), BUFFER);
        r2.create(OBJECT, TypedCrdt::Tree(Tree::default()));
        for d in &deltas {
            let _ = r2.receive(through_codec(d), 100);
        }

        // The two swap moves, made at the same time.
        let d1 = r1
            .mutate(OBJECT, 5, |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.move_node("a".into(), "b".into(), hlc, replica)),
                other => panic!("unexpected {other:?}"),
            })
            .expect("object exists");
        let d2 = r2
            .mutate(OBJECT, 5, |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.move_node("b".into(), "a".into(), hlc, replica)),
                other => panic!("unexpected {other:?}"),
            })
            .expect("object exists");
        deltas.push(d1);
        deltas.push(d2);

        // At the same time each replica labels a node. Labels travel the same
        // way and must end up the same whichever move wins.
        let l1 = r1
            .mutate(OBJECT, 6, |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.set_label("a".into(), "Alpha".into(), hlc, replica).unwrap()),
                other => panic!("unexpected {other:?}"),
            })
            .expect("object exists");
        let l2 = r2
            .mutate(OBJECT, 6, |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.set_label("b".into(), "Bravo".into(), hlc, replica).unwrap()),
                other => panic!("unexpected {other:?}"),
            })
            .expect("object exists");
        deltas.push(l1);
        deltas.push(l2);

        // Each replica also puts a node at a chosen position. The order of
        // siblings must end up the same after the same rough delivery.
        let p1 = r1
            .mutate(OBJECT, 7, |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.move_node_at("a".into(), "root".into(), 0, hlc, replica).unwrap()),
                other => panic!("unexpected {other:?}"),
            })
            .expect("object exists");
        let p2 = r2
            .mutate(OBJECT, 7, |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.move_node_at("b".into(), "root".into(), 0, hlc, replica).unwrap()),
                other => panic!("unexpected {other:?}"),
            })
            .expect("object exists");
        deltas.push(p1);
        deltas.push(p2);

        // New replicas each receive every delta twice, each in a different
        // order.
        let mut observers: Vec<Replica> = (0..3)
            .map(|i| {
                let mut r = Replica::new(ReplicaId(100 + i as u64), BUFFER);
                r.create(OBJECT, TypedCrdt::Tree(Tree::default()));
                r
            })
            .collect();
        for (i, o) in observers.iter_mut().enumerate() {
            deliver(o, &deltas, &keys, i as u64 * 0x9E37_79B9);
        }

        let first = tree_parents(&observers[0]);
        prop_assert!(!first.is_empty(), "seed moves materialized");
        for o in &observers {
            prop_assert_eq!(o.pending(), 0, "every delta was eventually applied");
            prop_assert_eq!(tree_parents(o), first.clone());
            prop_assert!(!tree_has_cycle(o), "no cycle after the concurrent swap");
            let (la, lb) = (tree_label(o, "a"), tree_label(o, "b"));
            prop_assert_eq!(la.as_deref(), Some("Alpha"));
            prop_assert_eq!(lb.as_deref(), Some("Bravo"));
            prop_assert_eq!(tree_children(o, "root"), tree_children(&observers[0], "root"));
            prop_assert_eq!(tree_projection(o), tree_projection(&observers[0]));
            prop_assert!(
                tree_projection(o).iter().any(|n| n.position.is_some()),
                "sibling positions actually crossed the channel"
            );
        }
    }

    /// A delta is never applied before the deltas it depends on. Here a chain
    /// of deltas, each depending on the one before, is delivered backwards.
    /// Every one has to wait until the first of the chain arrives.
    #[test]
    fn effects_never_precede_their_causes(
        amounts in prop::collection::vec(1u64..5, 2..8)
    ) {
        let mut author = Replica::new(ReplicaId(1), BUFFER);
        author.create(OBJECT, TypedCrdt::PnCounter(PnCounter::default()));

        let id = author.id();
        let deltas: Vec<WireDelta> = amounts
            .iter()
            .map(|a| {
                author
                    .mutate(OBJECT, 100, |crdt, _hlc, _replica| match crdt {
                        TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(pn.increment(id, *a)),
                        other => panic!("unexpected {other:?}"),
                    })
                    .expect("object exists")
            })
            .collect();

        let mut observer = Replica::new(ReplicaId(2), BUFFER);
        observer.create(OBJECT, TypedCrdt::PnCounter(PnCounter::default()));

        // Deliver backwards. Every delta except the first has to wait.
        for d in deltas.iter().skip(1).rev() {
            let (outcome, _) = observer.receive(through_codec(d), 100).unwrap();
            prop_assert_eq!(outcome, Applied::Buffered);
            prop_assert_eq!(counter_value(&observer), 0, "nothing visible yet");
        }

        // The first delta of the chain lets all the others through at once.
        let (outcome, _) = observer.receive(through_codec(&deltas[0]), 100).unwrap();
        prop_assert_eq!(outcome, Applied::Applied);
        prop_assert_eq!(observer.pending(), 0);
        prop_assert_eq!(counter_value(&observer), amounts.iter().sum::<u64>() as i64);
    }
}
