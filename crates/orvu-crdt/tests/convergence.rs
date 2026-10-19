//! Randomised tests that every type converges: once every replica has received
//! every change, they all hold exactly the same state. This property is called
//! strong eventual consistency.
//!
//! Each test makes random changes on several replicas and collects the deltas
//! they produce. It then delivers every delta to every replica, in a different
//! shuffled order for each replica and with every delta applied twice, and
//! checks that all replicas end up equal in every field.
//!
//! The first test is the odd one out. It checks that a JSON value survives being
//! written as text and read back.

use orvu_crdt::{BoundedCounter, DeltaCrdt, Hlc, JsonCrdt, JsonValue, LwwRegister, MvMap, OrMap, OrSet, PnCounter, ReplicaId, RichText, Rga, Seg};
use proptest::prelude::*;

/// Delivers `deltas` to every replica and checks that all replicas end up equal
/// in every field. Each replica gets the deltas in its own shuffled order,
/// worked out from the generated `keys`, and merges each delta twice. That
/// tests both reordering and duplicates.
fn assert_converges<C>(mut replicas: Vec<C>, deltas: Vec<C::Delta>, keys: &[u64])
where
    C: DeltaCrdt + Clone + PartialEq + std::fmt::Debug,
    C::Delta: Clone,
{
    for (i, r) in replicas.iter_mut().enumerate() {
        let mut order: Vec<usize> = (0..deltas.len()).collect();
        let salt = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        order.sort_by_key(|&j| keys.get(j).copied().unwrap_or(0) ^ salt);
        for j in order {
            r.merge(deltas[j].clone());
            r.merge(deltas[j].clone()); // merging it a second time must change nothing
        }
    }
    let first = &replicas[0];
    for r in &replicas[1..] {
        assert_eq!(first, r, "replicas diverged");
    }
}

// Generates JsonValues nested to a limited depth, with finite numbers only. NaN
// and the infinities are left out because they are written as null, so they
// would not come back the same.
fn json_value_strategy() -> impl Strategy<Value = JsonValue> {
    let leaf = prop_oneof![
        Just(JsonValue::Null),
        any::<bool>().prop_map(JsonValue::Bool),
        (-1.0e9f64..1.0e9).prop_map(JsonValue::Num),
        // Mostly whole numbers far past 2^53, which are written with an exponent.
        (-1.0e300f64..1.0e300).prop_map(JsonValue::Num),
        ".*".prop_map(JsonValue::Str),
    ];
    leaf.prop_recursive(4, 40, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..5).prop_map(JsonValue::Array),
            prop::collection::btree_map(".*", inner, 0..5).prop_map(JsonValue::Object),
        ]
    })
}


// A small JSON value for the document tests: a number, or a short list or
// object of numbers. Small values keep the keys and indexes colliding often.
fn small_json() -> impl Strategy<Value = JsonValue> {
    let num = (0i64..4).prop_map(|n| JsonValue::Num(n as f64));
    prop_oneof![
        num.clone(),
        Just(JsonValue::Null),
        prop::collection::vec(num.clone(), 0..3).prop_map(JsonValue::Array),
        prop::collection::btree_map("[ab]", num, 0..3).prop_map(JsonValue::Object),
    ]
}

// A path of one or two steps, made of the keys "a" and "b" and the index 0 or 1.
fn small_path() -> impl Strategy<Value = Vec<Seg>> {
    let seg = prop_oneof![
        "[ab]".prop_map(Seg::Key),
        (0usize..2).prop_map(Seg::Index),
    ];
    prop::collection::vec(seg, 1..3)
}

#[derive(Clone, Debug)]
enum JsonOp {
    Set(Vec<Seg>, JsonValue),
    Insert(Vec<Seg>, usize, JsonValue),
    Remove(Vec<Seg>),
}

fn json_op() -> impl Strategy<Value = JsonOp> {
    prop_oneof![
        (small_path(), small_json()).prop_map(|(p, v)| JsonOp::Set(p, v)),
        (small_path(), 0usize..2, small_json()).prop_map(|(p, i, v)| JsonOp::Insert(p, i, v)),
        small_path().prop_map(JsonOp::Remove),
    ]
}

fn apply(doc: &mut JsonCrdt, op: &JsonOp, ts: Hlc, rid: ReplicaId) -> Option<orvu_crdt::JsonDelta> {
    match op {
        JsonOp::Set(p, v) => doc.set(p, v, ts, rid),
        JsonOp::Insert(p, i, v) => doc.insert(p, *i, v, ts, rid),
        JsonOp::Remove(p) => doc.remove(p, ts, rid),
    }
    .ok()
}

// What a document should hold, worked out on plain values. A write replaces
// whatever is in its way: a field written into something that is not an
// object makes it an empty object first, and the same for a list. A field
// that is not there is created empty. So nothing that was overwritten or
// removed can come back. `None` means the operation is refused.
fn model_set(v: &mut JsonValue, path: &[Seg], new: &JsonValue) -> Option<()> {
    match path.split_first() {
        None => { *v = new.clone(); Some(()) }
        Some((Seg::Key(k), rest)) => {
            if !matches!(v, JsonValue::Object(_)) { *v = JsonValue::Object(Default::default()); }
            let JsonValue::Object(m) = v else { unreachable!() };
            model_set(m.entry(k.clone()).or_insert(JsonValue::Null), rest, new)
        }
        Some((Seg::Index(i), [])) => {
            if !matches!(v, JsonValue::Array(_)) { *v = JsonValue::Array(Vec::new()); }
            let JsonValue::Array(xs) = v else { unreachable!() };
            match (*i).cmp(&xs.len()) {
                std::cmp::Ordering::Less => xs[*i] = new.clone(),
                std::cmp::Ordering::Equal => xs.push(new.clone()),
                std::cmp::Ordering::Greater => return None,
            }
            Some(())
        }
        Some((Seg::Index(_), _)) => None,
    }
}

fn model_insert(v: &mut JsonValue, path: &[Seg], i: usize, new: &JsonValue) -> Option<()> {
    match path.split_first() {
        None => {
            match v {
                JsonValue::Null => *v = JsonValue::Array(Vec::new()),
                JsonValue::Array(_) => {}
                _ => return None,
            }
            let JsonValue::Array(xs) = v else { unreachable!() };
            if i > xs.len() { return None; }
            xs.insert(i, new.clone());
            Some(())
        }
        Some((Seg::Key(k), rest)) => {
            if !matches!(v, JsonValue::Object(_)) { *v = JsonValue::Object(Default::default()); }
            let JsonValue::Object(m) = v else { unreachable!() };
            model_insert(m.entry(k.clone()).or_insert(JsonValue::Null), rest, i, new)
        }
        Some((Seg::Index(_), _)) => None,
    }
}

fn model_remove(v: &mut JsonValue, path: &[Seg]) -> Option<()> {
    match path.split_first() {
        None => None,
        Some((Seg::Key(k), [])) => {
            if let JsonValue::Object(m) = v { m.remove(k); }
            Some(())
        }
        Some((Seg::Index(i), [])) => match v {
            JsonValue::Array(xs) if *i < xs.len() => { xs.remove(*i); Some(()) }
            _ => None,
        },
        Some((Seg::Key(k), rest)) => match v {
            JsonValue::Object(m) if m.contains_key(k) => model_remove(m.get_mut(k).unwrap(), rest),
            _ => Some(()),
        },
        Some((Seg::Index(_), _)) => None,
    }
}

fn model_get(v: &JsonValue, path: &[Seg]) -> Option<JsonValue> {
    match path.split_first() {
        None => Some(v.clone()),
        Some((Seg::Key(k), rest)) => match v {
            JsonValue::Object(m) => model_get(m.get(k)?, rest),
            _ => None,
        },
        Some((Seg::Index(i), rest)) => match v {
            JsonValue::Array(xs) => model_get(xs.get(*i)?, rest),
            _ => None,
        },
    }
}

fn path_of(op: &JsonOp) -> &[Seg] {
    match op { JsonOp::Set(p, _) | JsonOp::Insert(p, _, _) | JsonOp::Remove(p) => p }
}

// A write that several machines make at the same time, for the test of the
// rule below: a set or a removal at a path of object keys. A set may write
// the whole document.
#[derive(Clone, Debug)]
enum KeyOp {
    Set(Vec<Seg>, JsonValue),
    Remove(Vec<Seg>),
}

fn key_path(min: usize) -> impl Strategy<Value = Vec<Seg>> {
    prop::collection::vec("[ab]".prop_map(Seg::Key), min..3)
}

fn key_op() -> impl Strategy<Value = KeyOp> {
    prop_oneof![
        (key_path(0), small_json()).prop_map(|(p, v)| KeyOp::Set(p, v)),
        key_path(1).prop_map(KeyOp::Remove),
    ]
}

fn key_path_of(op: &KeyOp) -> &[Seg] {
    match op { KeyOp::Set(p, _) | KeyOp::Remove(p) => p }
}

// Where a write made at the same time as others replaces something whole:
// a set at its path, and a removal at its path when there is something
// there to remove. A removal of nothing changes nothing.
fn replaces_at<'a>(op: &'a KeyOp, start: &JsonValue) -> Option<&'a [Seg]> {
    match op {
        KeyOp::Set(p, _) => Some(p),
        KeyOp::Remove(p) => model_get(start, p).is_some().then_some(p.as_slice()),
    }
}

proptest! {
    #[test]
    fn json_codec_round_trips(v in json_value_strategy()) {
        let s = v.to_json_string();
        let back = JsonValue::from_json_str(&s).expect("serialized JSON must parse");
        prop_assert_eq!(back, v);
    }

    #[test]
    fn pn_counter_converges(
        ops in prop::collection::vec((0u64..3, 0u64..10, any::<u64>()), 0..40)
    ) {
        let mut r: Vec<PnCounter> = vec![PnCounter::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (i, (who, amt, key)) in ops.iter().enumerate() {
            let rid = ReplicaId(*who);
            let target = &mut r[(*who as usize) % 3];
            let d = if i % 2 == 0 {
                target.increment(rid, *amt)
            } else {
                target.decrement(rid, *amt % 5)
            };
            deltas.push(d);
            keys.push(*key);
        }
        assert_converges(r, deltas, &keys);
    }

    #[test]
    fn or_set_converges(
        ops in prop::collection::vec((0u64..3, 0u8..5, any::<bool>(), any::<u64>()), 0..40)
    ) {
        let mut r: Vec<OrSet<u8>> = vec![OrSet::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (who, elem, is_add, key) in ops {
            let rid = ReplicaId(who);
            let target = &mut r[(who as usize) % 3];
            let d = if is_add { target.add(elem, rid).unwrap() } else { target.remove(&elem) };
            deltas.push(d);
            keys.push(key);
        }
        assert_converges(r, deltas, &keys);
    }

    /// Generates writes that share the same `(wall, logical, writer)` but carry
    /// different values. These converge only because the register breaks such a
    /// tie by comparing the values.
    #[test]
    fn lww_register_converges(
        ops in prop::collection::vec((0u64..3, 0u8..8, 0u64..5, 0u32..3, any::<u64>()), 0..40)
    ) {
        let mut r: Vec<LwwRegister<u8>> = vec![LwwRegister::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (who, val, wall, logical, key) in ops {
            let rid = ReplicaId(who);
            let target = &mut r[(who as usize) % 3];
            let d = target.set(val, Hlc { wall, logical }, rid);
            deltas.push(d);
            keys.push(key);
        }
        assert_converges(r, deltas, &keys);
    }

    #[test]
    fn or_map_converges(
        ops in prop::collection::vec((0u64..3, 0u8..4, 0u64..5, any::<bool>(), any::<u64>()), 0..40)
    ) {
        let mut r: Vec<OrMap<u8, PnCounter>> = vec![OrMap::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (who, k, amt, is_update, key) in ops {
            let rid = ReplicaId(who);
            let target = &mut r[(who as usize) % 3];
            let d = if is_update {
                target.update(k, rid, |c| c.increment(rid, amt)).unwrap()
            } else {
                target.remove(&k)
            };
            deltas.push(d);
            keys.push(key);
        }
        assert_converges(r, deltas, &keys);
    }

    #[test]
    fn mv_map_converges(
        ops in prop::collection::vec((0u64..3, 0u8..4, 0u8..4, any::<bool>(), any::<u64>()), 0..40)
    ) {
        let mut r: Vec<MvMap<u8, u8>> = vec![MvMap::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (who, k, v, is_put, key) in ops {
            let rid = ReplicaId(who);
            let target = &mut r[(who as usize) % 3];
            let d = if is_put { target.put(k, v, rid).unwrap() } else { target.remove(&k) };
            deltas.push(d);
            keys.push(key);
        }
        assert_converges(r, deltas, &keys);
    }

    /// Three replicas insert at random positions and delete random visible
    /// elements, all at the same time. After every delta is delivered to every
    /// replica in a shuffled, duplicated order, the replicas are equal in every
    /// field, so the sequences they show agree too.
    #[test]
    fn rga_converges(
        ops in prop::collection::vec((0u64..3, 0usize..8, any::<bool>(), any::<u64>()), 0..40)
    ) {
        let mut r: Vec<Rga<String>> = vec![Rga::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (who, pos, is_delete, key) in ops {
            let rid = ReplicaId(who + 1);
            let target = &mut r[(who as usize) % 3];
            let vis = target.visible_dots();
            let d = if is_delete && !vis.is_empty() {
                target.tombstone(vis[pos % vis.len()])
            } else {
                // Insert after the element at pos-1. If pos is 0 or nothing is
                // visible, insert at the front.
                let pred = if vis.is_empty() || pos == 0 {
                    None
                } else {
                    Some(vis[(pos - 1) % vis.len()])
                };
                target.insert_after(pred, format!("v{who}-{pos}"), rid).unwrap()
            };
            deltas.push(d);
            keys.push(key);
        }
        assert_converges(r, deltas, &keys);
    }

    /// One machine makes a run of changes, and its document must always read
    /// the same as a plain value changed the same way. That pins down that
    /// what was overwritten or removed never shows again, not in a read of
    /// the whole document and not in a read of one path. A second machine that
    /// gets the deltas in any order, each twice, must read the same at the end.
    #[test]
    fn json_reads_like_a_plain_value(
        ops in prop::collection::vec(json_op(), 0..30),
        keys in prop::collection::vec(any::<u64>(), 30),
    ) {
        let mut doc = JsonCrdt::default();
        let mut model = JsonValue::Null;
        let mut deltas = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let ts = Hlc { wall: i as u64 + 1, logical: 0 };
            let before = doc.clone();
            let d = apply(&mut doc, op, ts, ReplicaId(1));
            let mut next = model.clone();
            let ok = match op {
                JsonOp::Set(p, v) => model_set(&mut next, p, v),
                JsonOp::Insert(p, i, v) => model_insert(&mut next, p, *i, v),
                JsonOp::Remove(p) => model_remove(&mut next, p),
            };
            prop_assert_eq!(d.is_some(), ok.is_some(), "{:?} on {:?}", op, model);
            if ok.is_some() { model = next; } else { prop_assert_eq!(&doc, &before); }
            prop_assert_eq!(doc.to_value(), model.clone(), "after {:?}", op);
            for o in &ops {
                prop_assert_eq!(doc.get(path_of(o)), model_get(&model, path_of(o)), "get {:?}", path_of(o));
            }
            deltas.extend(d);
        }
        let mut other = JsonCrdt::default();
        let mut order: Vec<usize> = (0..deltas.len()).collect();
        order.sort_by_key(|&j| keys[j % keys.len()].rotate_left(j as u32));
        for j in order {
            other.merge(deltas[j].clone());
            other.merge(deltas[j].clone());
        }
        prop_assert_eq!(other.to_value(), model);
    }

    /// Several machines start from the same document and each makes one
    /// write at the same time. What they all read in the end must follow
    /// the rule: replacing or removing something starts it afresh, a write
    /// made at the same time inside what was replaced is dropped, of two
    /// replacements of the same thing the later one wins, and every other
    /// write shows. Writes into the same empty, removed or replaced field
    /// all show, because they start afresh in the same way. Nothing that was
    /// replaced comes back. The changes reach each machine in its own order,
    /// each twice, and a copy built from whole snapshots must agree.
    #[test]
    fn json_writes_at_the_same_time_follow_the_rule(
        base_ops in prop::collection::vec(json_op(), 0..12),
        writes in prop::collection::vec((key_op(), any::<u64>()), 1..4),
        keys in prop::collection::vec(any::<u64>(), 4),
    ) {
        let mut base = JsonCrdt::default();
        let mut start = JsonValue::Null;
        for (i, op) in base_ops.iter().enumerate() {
            let ts = Hlc { wall: i as u64 + 1, logical: 0 };
            if apply(&mut base, op, ts, ReplicaId(9)).is_some() {
                let ok = match op {
                    JsonOp::Set(p, v) => model_set(&mut start, p, v),
                    JsonOp::Insert(p, i, v) => model_insert(&mut start, p, *i, v),
                    JsonOp::Remove(p) => model_remove(&mut start, p),
                };
                prop_assert!(ok.is_some());
            }
        }
        prop_assert_eq!(base.to_value(), start.clone());

        // Each write gets its own machine and its own moment after the base.
        let mut order: Vec<usize> = (0..writes.len()).collect();
        order.sort_by_key(|&i| (writes[i].1, i));
        let mut when = vec![0u64; writes.len()];
        for (rank, &i) in order.iter().enumerate() {
            when[i] = base_ops.len() as u64 + 1 + rank as u64;
        }
        let mut machines: Vec<JsonCrdt> = Vec::new();
        let mut deltas = Vec::new();
        for (i, (op, _)) in writes.iter().enumerate() {
            let mut doc = JsonCrdt::default();
            doc.merge(base.snapshot());
            let ts = Hlc { wall: when[i], logical: 0 };
            let rid = ReplicaId(i as u64 + 1);
            let d = match op {
                KeyOp::Set(p, v) => doc.set(p, v, ts, rid),
                KeyOp::Remove(p) => doc.remove(p, ts, rid),
            };
            prop_assert!(d.is_ok(), "{:?}", op);
            deltas.push(d.unwrap());
            machines.push(doc);
        }

        // The rule, worked out on plain values.
        let killed = |y: usize| {
            let py = key_path_of(&writes[y].0);
            (0..writes.len()).any(|x| {
                x != y && replaces_at(&writes[x].0, &start).is_some_and(|px| {
                    (px.len() < py.len() && py.starts_with(px))
                        || (px == py && when[x] > when[y] && replaces_at(&writes[y].0, &start).is_some())
                })
            })
        };
        let mut expected = start.clone();
        for &i in &order {
            if killed(i) || replaces_at(&writes[i].0, &start).is_none() {
                continue;
            }
            match &writes[i].0 {
                KeyOp::Set(p, v) => { model_set(&mut expected, p, v); }
                KeyOp::Remove(p) => { model_remove(&mut expected, p); }
            }
        }

        let mut whole = JsonCrdt::default();
        for m in &machines { whole.merge(m.snapshot()); }
        for (n, m) in machines.iter_mut().enumerate() {
            let mut order: Vec<usize> = (0..deltas.len()).collect();
            order.sort_by_key(|&j| keys[j % keys.len()].rotate_left((j + n) as u32));
            for j in order {
                m.merge(deltas[j].clone());
                m.merge(deltas[j].clone());
            }
        }
        for m in &machines {
            prop_assert_eq!(m, &whole);
        }
        prop_assert_eq!(whole.to_value(), expected.clone(), "from {:?} with {:?}", start, writes);
        for (op, _) in &writes {
            let p = key_path_of(op);
            prop_assert_eq!(whole.get(p), model_get(&expected, p), "get {:?}", p);
        }
    }

    /// Three machines make changes, and now and then one of them takes in a
    /// change another has made, so a change can build on others. Writes of
    /// whole lists and objects, writes into fields that were overwritten or
    /// removed, and inserts all mix. Every machine must end up the same in
    /// every field, whatever order the changes arrive in.
    #[test]
    fn json_converges_when_changes_build_on_each_other(
        steps in prop::collection::vec((0usize..3, json_op(), any::<bool>(), any::<u64>()), 0..40)
    ) {
        let mut r: Vec<JsonCrdt> = vec![JsonCrdt::default(); 3];
        let (mut deltas, mut keys): (Vec<orvu_crdt::JsonDelta>, Vec<u64>) = (Vec::new(), Vec::new());
        for (i, (who, op, sync, shuffle)) in steps.iter().enumerate() {
            if *sync && !deltas.is_empty() {
                let pick = (*shuffle as usize) % deltas.len();
                r[*who].merge(deltas[pick].clone());
            }
            let ts = Hlc { wall: i as u64 + 1, logical: 0 };
            if let Some(d) = apply(&mut r[*who], op, ts, ReplicaId(*who as u64 + 1)) {
                deltas.push(d);
                keys.push(*shuffle);
            }
        }
        assert_converges(r, deltas, &keys);
    }

    /// Several machines write a whole value to the same field at the same
    /// time. Every machine must end up with exactly one of those values, the
    /// one with the latest timestamp and then the highest machine id, and
    /// never a mix of two lists or two objects.
    #[test]
    fn json_whole_writes_at_once_keep_exactly_one(
        writes in prop::collection::vec((small_json(), 0u64..3), 1..4)
    ) {
        let mut r: Vec<JsonCrdt> = vec![JsonCrdt::default(); writes.len()];
        let field = [Seg::Key("f".into())];
        let mut deltas = Vec::new();
        for (i, (v, wall)) in writes.iter().enumerate() {
            let d = r[i].set(&field, v, Hlc { wall: *wall + 1, logical: 0 }, ReplicaId(i as u64 + 1)).unwrap();
            deltas.push(d);
        }
        let winner = writes
            .iter()
            .enumerate()
            .max_by_key(|(i, (_, wall))| (*wall, *i))
            .map(|(_, (v, _))| v.clone())
            .unwrap();
        for doc in &mut r {
            for d in &deltas { doc.merge(d.clone()); }
            prop_assert_eq!(doc.get(&field), Some(winner.clone()));
        }
        let first = r[0].clone();
        for doc in &r[1..] { prop_assert_eq!(doc, &first); }
    }

    /// Three replicas set, insert and remove at the same time, and end up equal
    /// in every field. The operations use nested keys, array indexes at the end
    /// of a path, and some that are invalid on purpose: changing something
    /// through an array index, and indexes that are out of bounds. An invalid
    /// operation must return `Err` and change nothing. Only the deltas from
    /// `Ok` results are delivered, so a failed operation that had changed local
    /// state would leave that replica different for good. That makes this a
    /// test that operations check their input before they change anything.
    #[test]
    fn json_converges(
        ops in prop::collection::vec((0u64..3, 0u8..3, 0u8..7, 0i64..5, any::<u64>()), 0..40)
    ) {
        let mut r: Vec<JsonCrdt> = vec![JsonCrdt::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (i, (who, k, op, val, shuffle)) in ops.iter().enumerate() {
            let rid = ReplicaId(*who + 1);
            let target = &mut r[(*who as usize) % 3];
            let ts = Hlc { wall: i as u64 + 1, logical: 0 };
            let key = Seg::Key(format!("k{k}"));
            let d = match op {
                0 => target.set(&[key], &JsonValue::Num(*val as f64), ts, rid),
                1 => target.set(&[key.clone(), Seg::Key("nested".into())], &JsonValue::Str(format!("v{val}")), ts, rid),
                2 => {
                    // Make sure an array exists, then insert into it. The set
                    // that creates the array changes local state (it adds and
                    // retires the dots that record a key is present), so its
                    // delta has to be delivered too. Otherwise the replicas end
                    // up with different metadata.
                    if let Ok(d0) = target.set(std::slice::from_ref(&key), &JsonValue::Array(vec![]), ts, rid) {
                        deltas.push(d0);
                        keys.push(*shuffle);
                    }
                    target.insert(&[key], 0, &JsonValue::Num(*val as f64), ts, rid)
                }
                3 => target.remove(&[key], ts, rid),
                // replace a whole element at an index that is often out of bounds
                4 => target.set(&[key, Seg::Index(*val as usize)], &JsonValue::Num(*val as f64), ts, rid),
                // remove at an index that is often out of bounds
                5 => target.remove(&[key, Seg::Index(*val as usize)], ts, rid),
                // invalid on purpose: change something through an index. It must change nothing.
                _ => target.set(&[key, Seg::Index(0), Seg::Key("f".into())], &JsonValue::Bool(true), ts, rid),
            };
            if let Ok(d) = d { deltas.push(d); keys.push(*shuffle); }
        }
        assert_converges(r, deltas, &keys);
    }

    /// Three replicas deposit, withdraw and transfer at the same time. Once
    /// every delta has reached every replica, the replicas must all be equal
    /// and the value must be at least 0. The value is only checked at that
    /// point. Merging deltas in an arbitrary order can take it below 0 for a
    /// while. Keeping it at 0 or above after every single merge needs causal
    /// delivery, where a delta only arrives after the ones it builds on, and
    /// this test does not deliver that way.
    #[test]
    fn bounded_counter_converges_and_is_non_negative_at_quiescence(
        ops in prop::collection::vec((0u64..3, 0u8..3, 1u64..6, 0u64..3, any::<u64>()), 0..40)
    ) {
        let mut r: Vec<BoundedCounter> = vec![BoundedCounter::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (who, op, amt, other, key) in ops {
            let rid = ReplicaId(who + 1);
            let target = &mut r[(who as usize) % 3];
            let d = match op {
                0 => Some(target.increment(amt, rid)),
                1 => target.try_decrement(amt, rid),          // None if refused
                _ => target.transfer(ReplicaId((other % 3) + 1), amt, rid),
            };
            if let Some(d) = d { deltas.push(d); keys.push(key); }
        }
        // Deliver every delta to every replica, shuffled and duplicated.
        for (i, rep) in r.iter_mut().enumerate() {
            let mut order: Vec<usize> = (0..deltas.len()).collect();
            let salt = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            order.sort_by_key(|&j| keys.get(j).copied().unwrap_or(0) ^ salt);
            for j in order { rep.merge(deltas[j].clone()); rep.merge(deltas[j].clone()); }
        }
        let first = &r[0];
        for rep in &r[1..] {
            prop_assert_eq!(first, rep, "replicas diverged");
        }
        prop_assert!(r[0].value() >= 0, "value non-negative at quiescence");
    }

    /// Three replicas insert, delete, format and unformat at the same time.
    /// They end up equal in every field, so `to_spans` gives the same result on
    /// each.
    #[test]
    fn rich_text_converges(
        ops in prop::collection::vec((0u64..3, 0usize..6, 0u8..4, 0u8..2, any::<u64>()), 0..40)
    ) {
        let mut r: Vec<RichText> = vec![RichText::default(); 3];
        let (mut deltas, mut keys) = (Vec::new(), Vec::new());
        for (i, (who, pos, op, attr, shuffle)) in ops.iter().enumerate() {
            let rid = ReplicaId(*who + 1);
            let target = &mut r[(*who as usize) % 3];
            let ts = Hlc { wall: i as u64 + 1, logical: 0 };
            let a = format!("m{attr}");
            let d = match op {
                0 => target.insert_char(*pos, char::from(b'a' + (*pos as u8 % 26)), rid).unwrap(),
                1 => target.delete(*pos, 1, rid),
                2 => target.format(*pos, 2, &a, "1", ts, rid).unwrap(),
                _ => target.unformat(*pos, 2, &a, ts, rid).unwrap(),
            };
            deltas.push(d);
            keys.push(*shuffle);
        }
        assert_converges(r, deltas, &keys);
    }
}
