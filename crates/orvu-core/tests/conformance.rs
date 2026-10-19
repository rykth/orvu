//! Shared test cases that every implementation of the engine must pass. This
//! is the Rust side.
//!
//! The cases live in `conformance/vectors.json`. This test runs each one
//! through the core and checks the result. There are two kinds. A `converge`
//! case checks the final state against an expected result that a person wrote
//! by hand. An `encode` case checks the exact bytes of each `WireDelta`
//! against bytes recorded in the file. To record the bytes again, run
//! `REGEN_CONFORMANCE=1 cargo test -p orvu-core --test conformance`.

use orvu_core::codec::{decode_wire_delta, encode_wire_delta};
use orvu_core::replica::Replica;
use orvu_core::{TypedCrdt, TypedDelta, WireDelta};
use orvu_crdt::ReplicaId;
use serde_json::Value;

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../conformance/vectors.json");
const BUFFER: usize = 4096;
const RECEIVER_CLOCK: u64 = 1_000_000;

fn empty_of(ty: &str) -> TypedCrdt {
    match ty {
        "counter" => TypedCrdt::PnCounter(Default::default()),
        "register" => TypedCrdt::LwwRegister(Default::default()),
        "set" => TypedCrdt::OrSet(Default::default()),
        "map" => TypedCrdt::OrMap(Default::default()),
        "cart" => TypedCrdt::OrMapCounter(Default::default()),
        "tree" => TypedCrdt::Tree(Default::default()),
        "list" => TypedCrdt::Rga(Default::default()),
        "json" => TypedCrdt::Json(Default::default()),
        "richtext" => TypedCrdt::RichText(Default::default()),
        "bounded" => TypedCrdt::BoundedCounter(Default::default()),
        "mvmap" => TypedCrdt::MvMap(Default::default()),
        other => panic!("unknown crdt type '{other}'"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs one operation on `r`, on the object `id` of type `ty`, and returns the
/// bytes of the delta it made.
fn apply_op(r: &mut Replica, id: &str, ty: &str, op: &Value) -> Vec<u8> {
    let kind = op["kind"].as_str().expect("op.kind");
    let now = op["now_wall"].as_u64().expect("op.now_wall");
    let wd: Option<WireDelta> = match (ty, kind) {
        ("counter", "increment") => {
            let by = op["by"].as_i64().expect("by");
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(if by >= 0 {
                    pn.increment(replica, by as u64)
                } else {
                    pn.decrement(replica, by.unsigned_abs())
                }),
                _ => unreachable!(),
            })
        }
        ("register", "set") => {
            let v = op["value"].as_str().expect("value").to_string();
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::LwwRegister(reg) => TypedDelta::LwwRegister(reg.set(v, hlc, replica)),
                _ => unreachable!(),
            })
        }
        ("set", "add") => {
            let v = op["value"].as_str().expect("value").to_string();
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.add(v, replica).unwrap()),
                _ => unreachable!(),
            })
        }
        ("set", "remove") => {
            let v = op["value"].as_str().expect("value").to_string();
            r.mutate(id, now, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.remove(&v)),
                _ => unreachable!(),
            })
        }
        ("map", "put") => {
            let k = op["key"].as_str().expect("key").to_string();
            let v = op["value"].as_str().expect("value").to_string();
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::OrMap(m) => {
                    TypedDelta::OrMap(m.update(k, replica, |reg| reg.set(v, hlc, replica)).unwrap())
                }
                _ => unreachable!(),
            })
        }
        ("map", "remove") => {
            let k = op["key"].as_str().expect("key").to_string();
            r.mutate(id, now, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::OrMap(m) => TypedDelta::OrMap(m.remove(&k)),
                _ => unreachable!(),
            })
        }
        ("mvmap", "put") => {
            let k = op["key"].as_str().expect("key").to_string();
            let v = op["value"].as_str().expect("value").to_string();
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::MvMap(m) => TypedDelta::MvMap(m.put(k, v, replica).unwrap()),
                _ => unreachable!(),
            })
        }
        ("mvmap", "remove") => {
            let k = op["key"].as_str().expect("key").to_string();
            r.mutate(id, now, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::MvMap(m) => TypedDelta::MvMap(m.remove(&k)),
                _ => unreachable!(),
            })
        }
        ("cart", "add") => {
            let item = op["item"].as_str().expect("item").to_string();
            let by = op["by"].as_i64().expect("by");
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::OrMapCounter(m) => TypedDelta::OrMapCounter(m.update(item, replica, |pn| {
                    if by >= 0 {
                        pn.increment(replica, by as u64)
                    } else {
                        pn.decrement(replica, by.unsigned_abs())
                    }
                }).unwrap()),
                _ => unreachable!(),
            })
        }
        ("cart", "remove_item") => {
            let item = op["item"].as_str().expect("item").to_string();
            r.mutate(id, now, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::OrMapCounter(m) => TypedDelta::OrMapCounter(m.remove(&item)),
                _ => unreachable!(),
            })
        }
        ("tree", "move_node") => {
            let child = op["child"].as_str().expect("child").to_string();
            let new_parent = op["new_parent"].as_str().expect("new_parent").to_string();
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.move_node(child, new_parent, hlc, replica)),
                _ => unreachable!(),
            })
        }
        ("tree", "move_node_at") => {
            let child = op["child"].as_str().expect("child").to_string();
            let new_parent = op["new_parent"].as_str().expect("new_parent").to_string();
            let index = op["index"].as_u64().expect("index") as usize;
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => {
                    TypedDelta::Tree(t.move_node_at(child, new_parent, index, hlc, replica).unwrap())
                }
                _ => unreachable!(),
            })
        }
        ("tree", "set_label") => {
            let node = op["node"].as_str().expect("node").to_string();
            let name = op["name"].as_str().expect("name").to_string();
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::Tree(t) => TypedDelta::Tree(t.set_label(node, name, hlc, replica).unwrap()),
                _ => unreachable!(),
            })
        }
        ("list", "insert") => {
            let index = op["index"].as_u64().expect("index") as usize;
            let v = op["value"].as_str().expect("value").to_string();
            let vis = match r.object(id).map(|o| o.crdt()) {
                Some(TypedCrdt::Rga(rga)) => rga.visible_dots(),
                _ => unreachable!(),
            };
            assert!(index <= vis.len(), "list insert index {index} out of bounds");
            let predecessor = if index == 0 { None } else { Some(vis[index - 1]) };
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::Rga(rga) => TypedDelta::Rga(rga.insert_after(predecessor, v, replica).unwrap()),
                _ => unreachable!(),
            })
        }
        ("list", "remove") => {
            let index = op["index"].as_u64().expect("index") as usize;
            let vis = match r.object(id).map(|o| o.crdt()) {
                Some(TypedCrdt::Rga(rga)) => rga.visible_dots(),
                _ => unreachable!(),
            };
            assert!(index < vis.len(), "list remove index {index} out of bounds");
            let target = vis[index];
            r.mutate(id, now, move |crdt, _hlc, _replica| match crdt {
                TypedCrdt::Rga(rga) => TypedDelta::Rga(rga.tombstone(target)),
                _ => unreachable!(),
            })
        }
        ("json", "set") => {
            let path = orvu_crdt::parse_path(op["path"].as_str().expect("path")).expect("valid path");
            let value = orvu_crdt::JsonValue::from_json_str(op["value"].as_str().expect("value"))
                .expect("valid json value");
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::Json(j) => TypedDelta::Json(j.set(&path, &value, hlc, replica).expect("set")),
                _ => unreachable!(),
            })
        }
        ("json", "remove") => {
            let path = orvu_crdt::parse_path(op["path"].as_str().expect("path")).expect("valid path");
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::Json(j) => TypedDelta::Json(j.remove(&path, hlc, replica).expect("remove")),
                _ => unreachable!(),
            })
        }
        ("richtext", "insert_char") => {
            let index = op["index"].as_u64().expect("index") as usize;
            let s = op["ch"].as_str().expect("ch").to_string();
            let ch = s.chars().next().expect("one character");
            assert_eq!(s.chars().count(), 1, "insert_char takes exactly one character");
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::RichText(rt) => TypedDelta::RichText(rt.insert_char(index, ch, replica).unwrap()),
                _ => unreachable!(),
            })
        }
        ("richtext", "delete") => {
            let index = op["index"].as_u64().expect("index") as usize;
            let len = op["len"].as_u64().expect("len") as usize;
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::RichText(rt) => TypedDelta::RichText(rt.delete(index, len, replica)),
                _ => unreachable!(),
            })
        }
        ("richtext", "format") => {
            let index = op["index"].as_u64().expect("index") as usize;
            let len = op["len"].as_u64().expect("len") as usize;
            let attr = op["attr"].as_str().expect("attr").to_string();
            let value = op["value"].as_str().expect("value").to_string();
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::RichText(rt) => {
                    TypedDelta::RichText(rt.format(index, len, &attr, &value, hlc, replica).unwrap())
                }
                _ => unreachable!(),
            })
        }
        ("richtext", "unformat") => {
            let index = op["index"].as_u64().expect("index") as usize;
            let len = op["len"].as_u64().expect("len") as usize;
            let attr = op["attr"].as_str().expect("attr").to_string();
            r.mutate(id, now, move |crdt, hlc, replica| match crdt {
                TypedCrdt::RichText(rt) => {
                    TypedDelta::RichText(rt.unformat(index, len, &attr, hlc, replica).unwrap())
                }
                _ => unreachable!(),
            })
        }
        ("bounded", "increment") => {
            let n = op["n"].as_u64().expect("n");
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::BoundedCounter(b) => TypedDelta::BoundedCounter(b.increment(n, replica)),
                _ => unreachable!(),
            })
        }
        ("bounded", "try_decrement") => {
            let n = op["n"].as_u64().expect("n");
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::BoundedCounter(b) => {
                    TypedDelta::BoundedCounter(b.try_decrement(n, replica).expect("allowance"))
                }
                _ => unreachable!(),
            })
        }
        ("bounded", "transfer") => {
            let to = ReplicaId(op["to"].as_u64().expect("to"));
            let n = op["n"].as_u64().expect("n");
            r.mutate(id, now, move |crdt, _hlc, replica| match crdt {
                TypedCrdt::BoundedCounter(b) => {
                    TypedDelta::BoundedCounter(b.transfer(to, n, replica).expect("allowance"))
                }
                _ => unreachable!(),
            })
        }
        (t, k) => panic!("unknown op {t}/{k}"),
    };
    encode_wire_delta(&wd.expect("op produced no delta"))
}

/// Reads the object's current state as a value shaped like a case's `expect`.
fn read_state(r: &Replica, id: &str) -> Value {
    match r.object(id).map(|o| o.crdt()) {
        Some(TypedCrdt::PnCounter(pn)) => serde_json::json!({ "value": pn.value() }),
        Some(TypedCrdt::LwwRegister(reg)) => serde_json::json!({ "value": reg.get().cloned() }),
        Some(TypedCrdt::OrSet(s)) => {
            let mut els: Vec<String> = s.elements().cloned().collect();
            els.sort();
            serde_json::json!({ "elements": els })
        }
        Some(TypedCrdt::OrMap(m)) => {
            let mut entries = serde_json::Map::new();
            for k in m.keys() {
                if let Some(v) = m.get(k).and_then(|reg| reg.get().cloned()) {
                    entries.insert(k.clone(), Value::from(v));
                }
            }
            serde_json::json!({ "entries": Value::Object(entries) })
        }
        Some(TypedCrdt::OrMapCounter(m)) => {
            let mut items = serde_json::Map::new();
            for k in m.keys() {
                if let Some(pn) = m.get(k) {
                    items.insert(k.clone(), Value::from(pn.value()));
                }
            }
            serde_json::json!({ "items": Value::Object(items) })
        }
        Some(TypedCrdt::Rga(l)) => serde_json::json!({ "list": l.to_vec() }),
        Some(TypedCrdt::Json(j)) => {
            let doc: Value = serde_json::from_str(&j.to_value().to_json_string())
                .expect("the in-house serializer emits valid JSON");
            serde_json::json!({ "value": doc })
        }
        Some(TypedCrdt::RichText(rt)) => {
            let spans: Vec<Value> = rt
                .to_spans()
                .into_iter()
                .map(|s| {
                    let marks: serde_json::Map<String, Value> =
                        s.marks.into_iter().map(|(k, v)| (k, Value::from(v))).collect();
                    serde_json::json!({ "text": s.text, "marks": Value::Object(marks) })
                })
                .collect();
            serde_json::json!({ "text": rt.text(), "spans": spans })
        }
        Some(TypedCrdt::BoundedCounter(b)) => serde_json::json!({ "value": b.value() }),
        Some(TypedCrdt::Tree(t)) => {
            let nodes: Vec<Value> = t
                .to_nodes()
                .into_iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.id,
                        "parent": n.parent,
                        "label": n.label,
                        "position": n.position,
                    })
                })
                .collect();
            // Every id that is some node's parent, with its children in order.
            // The ids come from the tree itself, so no list of them has to be
            // kept by hand.
            let mut parents: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for n in t.to_nodes() {
                if let Some(p) = n.parent {
                    parents.insert(p);
                }
            }
            let mut children = serde_json::Map::new();
            for p in parents {
                children.insert(p.clone(), Value::from(t.children(&p)));
            }
            serde_json::json!({ "nodes": nodes, "children": Value::Object(children) })
        }
        Some(TypedCrdt::MvMap(m)) => {
            let entries: serde_json::Map<String, Value> = m
                .entries()
                .map(|(k, vals)| (k.clone(), Value::from(vals.into_iter().cloned().collect::<Vec<_>>())))
                .collect();
            serde_json::json!({ "entries": Value::Object(entries) })
        }
        None => panic!("object '{id}' missing after replay"),
    }
}

fn run_converge(v: &Value) {
    let ty = v["type"].as_str().expect("type");
    let id = v["object"].as_str().expect("object");
    let mut deltas: Vec<Vec<u8>> = Vec::new();
    for rep in v["replicas"].as_array().expect("replicas") {
        let rid = rep["replica"].as_u64().expect("replica");
        let mut r = Replica::new(ReplicaId(rid), BUFFER);
        r.create(id, empty_of(ty));
        for op in rep["ops"].as_array().expect("ops") {
            deltas.push(apply_op(&mut r, id, ty, op));
        }
    }
    let mut obs = Replica::new(ReplicaId(9_999), BUFFER);
    obs.create(id, empty_of(ty));
    for bytes in &deltas {
        let d = decode_wire_delta(bytes).expect("decode");
        obs.receive(d, RECEIVER_CLOCK).expect("receive");
    }
    let got = read_state(&obs, id);
    assert_eq!(
        got,
        v["expect"],
        "converge vector '{}'",
        v["name"].as_str().unwrap_or("?")
    );
}

/// Works out the list of hex strings for an `encode` case.
fn encode_hexes(v: &Value) -> Vec<String> {
    let ty = v["type"].as_str().expect("type");
    let id = v["object"].as_str().expect("object");
    let rid = v["replica"].as_u64().expect("replica");
    let mut r = Replica::new(ReplicaId(rid), BUFFER);
    r.create(id, empty_of(ty));
    v["ops"]
        .as_array()
        .expect("ops")
        .iter()
        .map(|op| hex(&apply_op(&mut r, id, ty, op)))
        .collect()
}

#[test]
fn conformance_corpus() {
    let text = std::fs::read_to_string(CORPUS).expect("read corpus");
    let mut corpus: Value = serde_json::from_str(&text).expect("parse corpus");
    let regen = std::env::var("REGEN_CONFORMANCE").is_ok();

    for v in corpus["vectors"].as_array_mut().expect("vectors array") {
        match v["kind"].as_str().expect("kind") {
            "converge" => run_converge(v),
            "encode" => {
                let computed = encode_hexes(v);
                if regen {
                    v["wire_hex"] = Value::from(computed);
                } else {
                    let golden: Vec<String> = v["wire_hex"]
                        .as_array()
                        .unwrap_or_else(|| panic!("encode vector '{}' missing wire_hex — run REGEN_CONFORMANCE=1", v["name"].as_str().unwrap_or("?")))
                        .iter()
                        .map(|h| h.as_str().expect("hex string").to_string())
                        .collect();
                    assert_eq!(
                        computed,
                        golden,
                        "encode vector '{}'",
                        v["name"].as_str().unwrap_or("?")
                    );
                }
            }
            other => panic!("unknown vector kind '{other}'"),
        }
    }

    if regen {
        let pretty = serde_json::to_string_pretty(&corpus).expect("serialize");
        std::fs::write(CORPUS, pretty + "\n").expect("write corpus");
    }
}
