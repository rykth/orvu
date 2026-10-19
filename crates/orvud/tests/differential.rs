//! Runs every sidecar operation next to the same call made directly on
//! `orvu-sdk`, and checks that both give the same answer.
//!
//! There are two engines on one in-memory network. They are never connected
//! to each other. Each gets the same operations, one through `orvud` and one
//! through `orvu-sdk` directly. The engine code underneath is the same, so if
//! the answers differ, the sidecar translated something wrongly.
//!
//! Client libraries for every other language are generated from the messages
//! this file uses, so they all depend on these tests passing.
//!
//! These tests do not check how the data types merge. That is checked one
//! layer down by `conformance/vectors.json`. Those cases pass in a clock value
//! chosen by the caller, and the sidecar's messages have no field for one.
//! What can go wrong here is the translation: a different way of counting
//! positions, mixing up "absent" and "empty", a refusal that arrives as a
//! failure, or a signed number that arrives unsigned.
//!
//! The expected values are built from the typed results of the direct
//! `orvu-sdk` calls. They are never built with `orvud`'s own response helpers
//! in `errors.rs`. If they were, a bug in those helpers would be on both sides
//! of the comparison and would go unnoticed.

use orvu_crdt::{parse_path, JsonValue, ReplicaId};
use orvu_runtime::NodeConfig;
use orvu_sdk::{Client, ConsistencyClass, SdkError};
use orvu_transport::mem::MemTransport;
use orvud::pb;
use std::collections::BTreeMap;
use std::sync::Arc;

struct Pair {
    sidecar: orvud::server::Dataplane,
    session: String,
    embedded: Client,
}

async fn pair() -> Pair {
    let fabric = Arc::new(MemTransport::new());
    let a = Arc::new(
        Client::start(fabric.endpoint(1), NodeConfig::new(ReplicaId(1), "ap-1"))
            .await
            .expect("engine a"),
    );
    let embedded = Client::start(fabric.endpoint(2), NodeConfig::new(ReplicaId(2), "ap-2"))
        .await
        .expect("engine b");
    let sidecar = orvud::server::Dataplane::new(a, ReplicaId(1), "default");
    let session = sidecar
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open")
        .session;
    Pair { sidecar, session, embedded }
}

async fn via_sidecar(p: &Pair, op: pb::request::Op) -> pb::Response {
    p.sidecar
        .execute_for_test(pb::Request { session: p.session.clone(), op: Some(op), request_id: Vec::new() })
        .await
        .expect("boundary")
}

// ---- reading the sidecar's answers -------------------------------------

/// Sends a write and checks that the engine accepted it.
async fn ack(p: &Pair, op: pb::request::Op) {
    let r = via_sidecar(p, op).await;
    assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "expected ack, got {r:?}");
}

async fn read(p: &Pair, op: pb::request::Op) -> pb::read_value::V {
    match via_sidecar(p, op).await.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(v) })) => v,
        other => panic!("expected a value, got {other:?}"),
    }
}

async fn int(p: &Pair, op: pb::request::Op) -> i64 {
    match read(p, op).await {
        pb::read_value::V::IntValue(i) => i,
        other => panic!("expected an int, got {other:?}"),
    }
}

async fn boolean(p: &Pair, op: pb::request::Op) -> bool {
    match read(p, op).await {
        pb::read_value::V::BoolValue(b) => b,
        other => panic!("expected a bool, got {other:?}"),
    }
}

/// `Absent` becomes `None` and the empty string becomes `Some("")`. The two
/// must stay different.
async fn opt_string(p: &Pair, op: pb::request::Op) -> Option<String> {
    match read(p, op).await {
        pb::read_value::V::StringValue(s) => Some(s),
        pb::read_value::V::Absent(_) => None,
        other => panic!("expected a string or absent, got {other:?}"),
    }
}

async fn string(p: &Pair, op: pb::request::Op) -> String {
    match read(p, op).await {
        pb::read_value::V::StringValue(s) => s,
        other => panic!("expected a string, got {other:?}"),
    }
}

async fn strings(p: &Pair, op: pb::request::Op) -> Vec<String> {
    match read(p, op).await {
        pb::read_value::V::Strings(l) => l.values,
        other => panic!("expected strings, got {other:?}"),
    }
}

async fn pairs(p: &Pair, op: pb::request::Op) -> Vec<(String, String)> {
    match read(p, op).await {
        pb::read_value::V::Pairs(l) => l.pairs.into_iter().map(|x| (x.key, x.value)).collect(),
        other => panic!("expected pairs, got {other:?}"),
    }
}

async fn multi_pairs(p: &Pair, op: pb::request::Op) -> Vec<(String, Vec<String>)> {
    match read(p, op).await {
        pb::read_value::V::MultiPairs(l) => l.pairs.into_iter().map(|e| (e.key, e.values)).collect(),
        other => panic!("expected multi_pairs, got {other:?}"),
    }
}

async fn counted(p: &Pair, op: pb::request::Op) -> Vec<(String, i64)> {
    match read(p, op).await {
        pb::read_value::V::Counted(l) => l.items.into_iter().map(|x| (x.item, x.qty)).collect(),
        other => panic!("expected counted items, got {other:?}"),
    }
}

/// Reads JSON text and parses it with `JsonValue::from_json_str`, so values
/// are compared and not text.
async fn json(p: &Pair, op: pb::request::Op) -> JsonValue {
    match read(p, op).await {
        pb::read_value::V::Json(t) => JsonValue::from_json_str(&t).expect("the sidecar emits valid json"),
        other => panic!("expected json text, got {other:?}"),
    }
}

async fn failed(p: &Pair, op: pb::request::Op) -> pb::Code {
    match via_sidecar(p, op).await.result {
        Some(pb::response::Result::Failed(f)) => f.code(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// Returns `true` if the operation was refused and `false` if it was
/// accepted. Any other answer is a bug.
async fn refused(p: &Pair, op: pb::request::Op) -> bool {
    match via_sidecar(p, op).await.result {
        Some(pb::response::Result::Refused(_)) => true,
        Some(pb::response::Result::Ack(_)) => false,
        other => panic!("expected ack or refusal, got {other:?}"),
    }
}

fn s(x: &str) -> JsonValue {
    JsonValue::Str(x.into())
}

fn obj(fields: Vec<(&str, JsonValue)>) -> JsonValue {
    JsonValue::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<BTreeMap<_, _>>())
}

fn opt(x: Option<String>) -> JsonValue {
    x.map(JsonValue::Str).unwrap_or(JsonValue::Null)
}

// ---- counter -----------------------------------------------------------

#[tokio::test]
async fn counter_agrees() {
    let p = pair().await;
    for n in [1u64, 2, 3] {
        via_sidecar(&p, pb::request::Op::CounterIncrement(pb::CounterIncrement {
            object: "c".into(), n,
        })).await;
        p.embedded.counter("c").await.unwrap().increment(n).await.unwrap();
    }
    ack(&p, pb::request::Op::CounterDecrement(pb::CounterDecrement { object: "c".into(), n: 10 })).await;
    p.embedded.counter("c").await.unwrap().decrement(10).await.unwrap();

    let through_sidecar = via_sidecar(&p, pb::request::Op::CounterValue(pb::CounterValue {
        object: "c".into(),
    })).await;
    let embedded = p.embedded.counter("c").await.unwrap().value().await.unwrap();
    match through_sidecar.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(i)) })) => {
            assert_eq!(i, embedded);
        }
        other => panic!("expected an int, got {other:?}"),
    }
    // The counter is taken below zero on purpose, to check that the minus
    // sign comes through the sidecar.
    assert_eq!(embedded, -4);

    // Declaring a class is one of the operations too. Declaring the same class
    // again is fine. Declaring a different one fails with a class mismatch on
    // both sides.
    ack(&p, pb::request::Op::Declare(pb::Declare { object: "c".into(), object_class: pb::Class::Ap as i32 })).await;
    p.embedded.declare("c", ConsistencyClass::Ap).unwrap();
    let code = failed(&p, pb::request::Op::Declare(pb::Declare {
        object: "c".into(), object_class: pb::Class::Escrow as i32,
    })).await;
    assert_eq!(code, pb::Code::ClassMismatch);
    assert!(matches!(
        p.embedded.declare("c", ConsistencyClass::Escrow),
        Err(SdkError::ClassMismatch { .. })
    ));
}

// ---- register ----------------------------------------------------------

#[tokio::test]
async fn register_agrees() {
    let p = pair().await;
    let get = || pb::request::Op::RegisterGet(pb::RegisterGet { object: "r".into() });
    let reg = p.embedded.register("r").await.unwrap();

    // A register that was never set reads as absent, which is different from
    // one set to the empty string.
    assert_eq!(opt_string(&p, get()).await, reg.get().await.unwrap());
    assert_eq!(reg.get().await.unwrap(), None);

    for v in ["first", "", "ünï ✓"] {
        ack(&p, pb::request::Op::RegisterSet(pb::RegisterSet { object: "r".into(), value: v.into() })).await;
        reg.set(v).await.unwrap();
        assert_eq!(opt_string(&p, get()).await, reg.get().await.unwrap(), "after setting {v:?}");
    }
    assert_eq!(reg.get().await.unwrap(), Some("ünï ✓".to_string()));

    // Opening an object as another type fails the same way on both sides.
    ack(&p, pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "k".into(), n: 1 })).await;
    p.embedded.counter("k").await.unwrap().increment(1).await.unwrap();
    let code = failed(&p, pb::request::Op::RegisterGet(pb::RegisterGet { object: "k".into() })).await;
    assert_eq!(code, pb::Code::TypeMismatch);
    assert!(matches!(p.embedded.register("k").await, Err(SdkError::TypeMismatch { .. })));
}

// ---- set ---------------------------------------------------------------

#[tokio::test]
async fn set_agrees() {
    let p = pair().await;
    let set = p.embedded.set("m").await.unwrap();
    for m in ["pear", "apple", "fig", "apple"] {
        ack(&p, pb::request::Op::SetAdd(pb::SetAdd { object: "m".into(), member: m.into() })).await;
        set.add(m).await.unwrap();
    }
    ack(&p, pb::request::Op::SetRemove(pb::SetRemove { object: "m".into(), member: "fig".into() })).await;
    set.remove("fig").await.unwrap();

    for m in ["pear", "apple", "fig", "never-added"] {
        let got = boolean(&p, pb::request::Op::SetContains(pb::SetContains { object: "m".into(), member: m.into() })).await;
        assert_eq!(got, set.contains(m).await.unwrap(), "contains {m}");
    }
    let got = strings(&p, pb::request::Op::SetElements(pb::SetElements { object: "m".into() })).await;
    let want = set.elements().await.unwrap();
    assert_eq!(got, want);
    assert_eq!(want.len(), 2);
}

// ---- map ---------------------------------------------------------------

#[tokio::test]
async fn map_agrees() {
    let p = pair().await;
    let map = p.embedded.map("cfg").await.unwrap();
    for (k, v) in [("a", "1"), ("b", ""), ("c", "3"), ("a", "overwritten")] {
        ack(&p, pb::request::Op::MapPut(pb::MapPut { object: "cfg".into(), key: k.into(), value: v.into() })).await;
        map.put(k, v).await.unwrap();
    }
    ack(&p, pb::request::Op::MapRemove(pb::MapRemove { object: "cfg".into(), key: "c".into() })).await;
    map.remove("c").await.unwrap();

    // A key that is present, one present with an empty value, one that was
    // removed, and one that was never there.
    for k in ["a", "b", "c", "zzz"] {
        let got = opt_string(&p, pb::request::Op::MapGet(pb::MapGet { object: "cfg".into(), key: k.into() })).await;
        assert_eq!(got, map.get(k).await.unwrap(), "get {k}");
    }
    assert_eq!(map.get("b").await.unwrap(), Some(String::new()));
    assert_eq!(map.get("zzz").await.unwrap(), None);

    let keys = strings(&p, pb::request::Op::MapKeys(pb::MapKeys { object: "cfg".into() })).await;
    assert_eq!(keys, map.keys().await.unwrap());
    let entries = pairs(&p, pb::request::Op::MapEntries(pb::MapEntries { object: "cfg".into() })).await;
    assert_eq!(entries, map.entries().await.unwrap());
    assert_eq!(entries.len(), 2);
}

// ---- mv_map ------------------------------------------------------------

#[tokio::test]
async fn mv_map_agrees() {
    let p = pair().await;
    let map = p.embedded.mv_map("notes").await.unwrap();
    for (k, v) in [("a", "1"), ("b", ""), ("c", "3"), ("a", "overwritten")] {
        ack(&p, pb::request::Op::MvMapPut(pb::MvMapPut { object: "notes".into(), key: k.into(), value: v.into() })).await;
        map.put(k, v).await.unwrap();
    }
    ack(&p, pb::request::Op::MvMapRemove(pb::MvMapRemove { object: "notes".into(), key: "c".into() })).await;
    map.remove("c").await.unwrap();

    // A key that is present, one present with an empty value, one that was
    // removed, and one that was never there. An empty value is one value, not
    // none.
    for k in ["a", "b", "c", "zzz"] {
        let got = strings(&p, pb::request::Op::MvMapGet(pb::MvMapGet { object: "notes".into(), key: k.into() })).await;
        assert_eq!(got, map.get(k).await.unwrap(), "get {k}");
    }
    assert_eq!(map.get("b").await.unwrap(), vec![String::new()]);
    assert!(map.get("zzz").await.unwrap().is_empty());

    let keys = strings(&p, pb::request::Op::MvMapKeys(pb::MvMapKeys { object: "notes".into() })).await;
    assert_eq!(keys, map.keys().await.unwrap());
    let entries = multi_pairs(&p, pb::request::Op::MvMapEntries(pb::MvMapEntries { object: "notes".into() })).await;
    assert_eq!(entries, map.entries().await.unwrap());
    assert_eq!(entries.len(), 2);
}

// ---- cart --------------------------------------------------------------

#[tokio::test]
async fn cart_agrees() {
    let p = pair().await;
    let cart = p.embedded.cart("cart").await.unwrap();
    let steps: Vec<(&str, u64)> = vec![("apple", 5), ("pear", 2), ("fig", 1)];
    for (item, qty) in steps {
        ack(&p, pb::request::Op::CartAdd(pb::CartAdd { object: "cart".into(), item: item.into(), qty })).await;
        cart.add(item, qty).await.unwrap();
    }
    // Remove more than was added. A negative quantity is a valid result.
    ack(&p, pb::request::Op::CartRemoveQty(pb::CartRemoveQty { object: "cart".into(), item: "apple".into(), qty: 9 })).await;
    cart.remove_qty("apple", 9).await.unwrap();
    ack(&p, pb::request::Op::CartRemoveItem(pb::CartRemoveItem { object: "cart".into(), item: "pear".into() })).await;
    cart.remove_item("pear").await.unwrap();

    for item in ["apple", "pear", "fig", "never"] {
        let got = int(&p, pb::request::Op::CartQuantity(pb::CartQuantity { object: "cart".into(), item: item.into() })).await;
        assert_eq!(got, cart.quantity(item).await.unwrap(), "quantity {item}");
    }
    assert_eq!(cart.quantity("apple").await.unwrap(), -4);

    let items = counted(&p, pb::request::Op::CartItems(pb::CartItems { object: "cart".into() })).await;
    assert_eq!(items, cart.items().await.unwrap());
}

// ---- list --------------------------------------------------------------

#[tokio::test]
async fn list_agrees() {
    let p = pair().await;
    let list = p.embedded.list("l").await.unwrap();
    // Insert at the front, at the back and in the middle, the three kinds of
    // position an index can name.
    for (index, value) in [(0u64, "b"), (1, "d"), (0, "a"), (2, "c"), (4, "e")] {
        ack(&p, pb::request::Op::ListInsert(pb::ListInsert { object: "l".into(), index, value: value.into() })).await;
        list.insert(index as usize, value).await.unwrap();
    }
    ack(&p, pb::request::Op::ListDelete(pb::ListDelete { object: "l".into(), index: 1 })).await;
    list.delete(1).await.unwrap();

    let got = strings(&p, pb::request::Op::ListToVec(pb::ListToVec { object: "l".into() })).await;
    assert_eq!(got, list.to_vec().await.unwrap());
    assert_eq!(got, ["a", "c", "d", "e"]);
    let len = int(&p, pb::request::Op::ListLen(pb::ListLen { object: "l".into() })).await;
    assert_eq!(len as usize, list.len().await.unwrap());

    // Indexes inside the list, one past the end, and far past it.
    for index in [0u64, 1, 3, 4, 5, 1_000] {
        let got = opt_string(&p, pb::request::Op::ListGet(pb::ListGet { object: "l".into(), index })).await;
        assert_eq!(got, list.get(index as usize).await.unwrap(), "get {index}");
    }

    // An index out of bounds is the same failure on both sides, for insert
    // and for delete.
    let code = failed(&p, pb::request::Op::ListInsert(pb::ListInsert { object: "l".into(), index: 5, value: "x".into() })).await;
    assert_eq!(code, pb::Code::OutOfBounds);
    assert!(matches!(list.insert(5, "x").await, Err(SdkError::OutOfBounds { .. })));
    let code = failed(&p, pb::request::Op::ListDelete(pb::ListDelete { object: "l".into(), index: 4 })).await;
    assert_eq!(code, pb::Code::OutOfBounds);
    assert!(matches!(list.delete(4).await, Err(SdkError::OutOfBounds { .. })));
    // A failed write changed nothing.
    let after = strings(&p, pb::request::Op::ListToVec(pb::ListToVec { object: "l".into() })).await;
    assert_eq!(after, list.to_vec().await.unwrap());
}

// ---- json --------------------------------------------------------------

#[tokio::test]
async fn json_agrees() {
    let p = pair().await;
    let doc = p.embedded.json("doc").await.unwrap();
    let writes = [
        (r#"["user"]"#, r#"{"name":"Ada","tags":["x","y"]}"#),
        (r#"["user","age"]"#, "36"),
        (r#"["user","tags",1]"#, r#""z""#),
        (r#"["flag"]"#, "true"),
        (r#"["nothing"]"#, "null"),
        (r#"["scratch"]"#, "1.5"),
    ];
    for (path, value) in writes {
        ack(&p, pb::request::Op::JsonSet(pb::JsonSet {
            object: "doc".into(), path_json: path.into(), value_json: value.into(),
        })).await;
        doc.set(&parse_path(path).unwrap(), JsonValue::from_json_str(value).unwrap()).await.unwrap();
    }
    ack(&p, pb::request::Op::JsonRemove(pb::JsonRemove { object: "doc".into(), path_json: r#"["scratch"]"#.into() })).await;
    doc.remove(&parse_path(r#"["scratch"]"#).unwrap()).await.unwrap();

    // A plain value, an object, an array element, a removed path, a path that
    // was never there, and the whole document.
    for path in [r#"["user","age"]"#, r#"["user"]"#, r#"["user","tags",0]"#, r#"["scratch"]"#, r#"["nope","deeper"]"#, "[]"] {
        let want = doc.get(&parse_path(path).unwrap()).await.unwrap();
        let r = via_sidecar(&p, pb::request::Op::JsonGet(pb::JsonGet { object: "doc".into(), path_json: path.into() })).await;
        match (r.result, want) {
            (Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Json(t)) })), Some(w)) => {
                assert_eq!(JsonValue::from_json_str(&t).unwrap(), w, "get {path}");
            }
            (Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) })), None) => {}
            (got, want) => panic!("get {path}: sidecar {got:?} vs embedded {want:?}"),
        }
    }

    let whole = json(&p, pb::request::Op::JsonToValue(pb::JsonToValue { object: "doc".into() })).await;
    assert_eq!(whole, doc.to_value().await.unwrap());
    // Check the actual contents, so two empty documents cannot pass as equal.
    assert_eq!(
        whole,
        obj(vec![
            ("user", obj(vec![("age", JsonValue::Num(36.0)), ("name", s("Ada")), ("tags", JsonValue::Array(vec![s("x"), s("z")]))])),
            ("flag", JsonValue::Bool(true)),
            ("nothing", JsonValue::Null),
        ])
    );

    // Text that does not parse comes back from the sidecar as a `Json`
    // failure in the response, never as an error for the whole call.
    let code = failed(&p, pb::request::Op::JsonSet(pb::JsonSet {
        object: "doc".into(), path_json: r#"["a"]"#.into(), value_json: "{not json".into(),
    })).await;
    assert_eq!(code, pb::Code::Json);
    assert!(JsonValue::from_json_str("{not json").is_err());
    let code = failed(&p, pb::request::Op::JsonGet(pb::JsonGet { object: "doc".into(), path_json: r#"{"a":1}"#.into() })).await;
    assert_eq!(code, pb::Code::Json);
    assert!(parse_path(r#"{"a":1}"#).is_err());
}

// ---- richtext ----------------------------------------------------------

#[tokio::test]
async fn richtext_agrees() {
    let p = pair().await;
    let doc = p.embedded.rich_text("rt").await.unwrap();
    // The text has non-ASCII characters on purpose. Positions count code
    // points, not bytes.
    ack(&p, pb::request::Op::RichtextInsert(pb::RichtextInsert { object: "rt".into(), index: 0, text: "héllo wörld".into() })).await;
    doc.insert(0, "héllo wörld").await.unwrap();
    ack(&p, pb::request::Op::RichtextInsert(pb::RichtextInsert { object: "rt".into(), index: 5, text: " ✓".into() })).await;
    doc.insert(5, " ✓").await.unwrap();
    ack(&p, pb::request::Op::RichtextFormat(pb::RichtextFormat {
        object: "rt".into(), index: 1, len: 4, attr: "bold".into(), value: "1".into(),
    })).await;
    doc.format(1, 4, "bold", "1").await.unwrap();
    ack(&p, pb::request::Op::RichtextFormat(pb::RichtextFormat {
        object: "rt".into(), index: 3, len: 5, attr: "color".into(), value: "red".into(),
    })).await;
    doc.format(3, 5, "color", "red").await.unwrap();
    ack(&p, pb::request::Op::RichtextUnformat(pb::RichtextUnformat {
        object: "rt".into(), index: 2, len: 1, attr: "bold".into(),
    })).await;
    doc.unformat(2, 1, "bold").await.unwrap();
    ack(&p, pb::request::Op::RichtextDelete(pb::RichtextDelete { object: "rt".into(), index: 0, len: 1 })).await;
    doc.delete(0, 1).await.unwrap();

    let text = string(&p, pb::request::Op::RichtextText(pb::RichtextText { object: "rt".into() })).await;
    assert_eq!(text, doc.text().await.unwrap());

    // Build the expected spans by hand from the typed result of the direct
    // `orvu-sdk` call.
    let want = JsonValue::Array(
        doc.to_spans()
            .await
            .unwrap()
            .into_iter()
            .map(|sp| {
                obj(vec![
                    ("text", s(&sp.text)),
                    ("marks", JsonValue::Object(sp.marks.into_iter().map(|(k, v)| (k, s(&v))).collect())),
                ])
            })
            .collect(),
    );
    let got = json(&p, pb::request::Op::RichtextToSpans(pb::RichtextToSpans { object: "rt".into() })).await;
    assert_eq!(got, want);
    // Check that there really are marks. Otherwise this could be comparing
    // two empty lists of marks.
    match &got {
        JsonValue::Array(spans) => assert!(spans.len() >= 3, "expected several mark runs, got {spans:?}"),
        other => panic!("expected an array, got {other:?}"),
    }

    // Both refuse an empty format value. The sidecar turns the request away
    // before it reaches the engine, because the mistake is in the request.
    let refused = p
        .sidecar
        .execute_for_test(pb::Request {
            session: p.session.clone(),
            op: Some(pb::request::Op::RichtextFormat(pb::RichtextFormat {
                object: "rt".into(), index: 0, len: 1, attr: "bold".into(), value: String::new(),
            })),
            request_id: Vec::new(),
        })
        .await
        .expect_err("an empty format value is a bad request");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(matches!(doc.format(0, 1, "bold", "").await, Err(SdkError::EmptyValue)));
}

// ---- tree --------------------------------------------------------------

#[tokio::test]
async fn tree_agrees() {
    let p = pair().await;
    let tree = p.embedded.tree("t").await.unwrap();
    let moves = [("a", "root"), ("b", "root"), ("c", "a"), ("d", "root")];
    for (child, parent) in moves {
        ack(&p, pb::request::Op::TreeMoveNode(pb::TreeMoveNode {
            object: "t".into(), child: child.into(), new_parent: parent.into(),
        })).await;
        tree.move_node(child, parent).await.unwrap();
    }
    // Place a node at a position. Index 0 puts `d` before its siblings.
    ack(&p, pb::request::Op::TreeMoveNodeAt(pb::TreeMoveNodeAt {
        object: "t".into(), child: "d".into(), new_parent: "root".into(), index: 0,
    })).await;
    tree.move_node_at("d", "root", 0).await.unwrap();
    // Moving `a` under its own child would make a loop. The tree skips that
    // move, and the write is still accepted.
    ack(&p, pb::request::Op::TreeMoveNode(pb::TreeMoveNode {
        object: "t".into(), child: "a".into(), new_parent: "c".into(),
    })).await;
    tree.move_node("a", "c").await.unwrap();
    // Rename a node, and label a node that was never moved.
    for (node, label) in [("a", "Alpha"), ("a", "Alpha 2"), ("ghost", "Boo")] {
        ack(&p, pb::request::Op::TreeSetLabel(pb::TreeSetLabel {
            object: "t".into(), node: node.into(), label: label.into(),
        })).await;
        tree.set_label(node, label).await.unwrap();
    }

    for node in ["root", "a", "c", "ghost", "unknown"] {
        let got = strings(&p, pb::request::Op::TreeChildren(pb::TreeChildren { object: "t".into(), node: node.into() })).await;
        assert_eq!(got, tree.children(node).await.unwrap(), "children of {node}");
        let got = opt_string(&p, pb::request::Op::TreeParent(pb::TreeParent { object: "t".into(), node: node.into() })).await;
        assert_eq!(got, tree.parent(node).await.unwrap(), "parent of {node}");
    }

    let want = JsonValue::Array(
        tree.to_nodes()
            .await
            .unwrap()
            .into_iter()
            .map(|n| {
                obj(vec![
                    ("id", s(&n.id)),
                    ("parent", opt(n.parent)),
                    ("label", opt(n.label)),
                    ("position", opt(n.position)),
                ])
            })
            .collect(),
    );
    let got = json(&p, pb::request::Op::TreeToNodes(pb::TreeToNodes { object: "t".into() })).await;
    assert_eq!(got, want);
    // The main check is that both sides agree. But both could agree on a wrong
    // answer, so two results are also checked against fixed values. The move
    // that would make a loop (`a` under its own child `c`) was skipped, so `a`
    // is still under root. The rename worked, so its label is the second one.
    match &got {
        JsonValue::Array(nodes) => {
            assert_eq!(nodes.len(), 6, "a b c d root ghost");
            let a = nodes
                .iter()
                .find(|n| matches!(n, JsonValue::Object(o) if o.get("id") == Some(&s("a"))))
                .expect("a is in the node list");
            match a {
                JsonValue::Object(o) => {
                    assert_eq!(o.get("parent"), Some(&s("root")), "the cycle was skipped");
                    assert_eq!(o.get("label"), Some(&s("Alpha 2")), "the rename took");
                }
                other => panic!("expected an object, got {other:?}"),
            }
        }
        other => panic!("expected an array, got {other:?}"),
    }
}

// ---- bounded counter ---------------------------------------------------

#[tokio::test]
async fn bounded_agrees() {
    let p = pair().await;
    let b = p.embedded.bounded_counter("stock").await.unwrap();
    let value = || pb::request::Op::BoundedValue(pb::BoundedValue { object: "stock".into() });
    let allowance = || pb::request::Op::BoundedAllowance(pb::BoundedAllowance { object: "stock".into() });

    ack(&p, pb::request::Op::BoundedIncrement(pb::BoundedIncrement { object: "stock".into(), n: 10 })).await;
    b.increment(10).await.unwrap();

    // Both sides get the same inputs. The two engines are never connected, so
    // neither can borrow allowance from the other. A refusal here can only
    // come from the allowance check. The whole list of answers is compared,
    // not only whether some refusal happened. That is what shows both sides
    // refuse on the same input. 4 fits. 7 is more than the 6 left, so it does
    // not. 6 is exactly what is left and fits. 1 is more than 0, so it does not.
    let inputs = [4u64, 7, 6, 1];
    let mut through_sidecar = Vec::new();
    let mut through_embedded = Vec::new();
    for n in inputs {
        through_sidecar.push(
            refused(&p, pb::request::Op::BoundedTryDecrement(pb::BoundedTryDecrement { object: "stock".into(), n })).await,
        );
        through_embedded.push(match b.try_decrement(n).await {
            Ok(()) => false,
            Err(SdkError::Exhausted) => true,
            Err(e) => panic!("try_decrement({n}) failed rather than refused: {e}"),
        });
        // Both sides hold the same value and allowance after every step, so a
        // refusal changed nothing on either side.
        assert_eq!(int(&p, value()).await, b.value().await.unwrap(), "value after {n}");
        assert_eq!(int(&p, allowance()).await as u64, b.local_allowance().await.unwrap(), "allowance after {n}");
    }
    assert_eq!(through_sidecar, through_embedded);
    assert_eq!(through_sidecar, [false, true, false, true]);
    assert_eq!(b.value().await.unwrap(), 0);

    // Transfer refuses on the same input too. With 5 added, moving 3 out
    // fits and moving 3 more does not.
    ack(&p, pb::request::Op::BoundedIncrement(pb::BoundedIncrement { object: "stock".into(), n: 5 })).await;
    b.increment(5).await.unwrap();
    let mut through_sidecar = Vec::new();
    let mut through_embedded = Vec::new();
    for n in [3u64, 3, 2] {
        through_sidecar.push(
            refused(&p, pb::request::Op::BoundedTransfer(pb::BoundedTransfer { object: "stock".into(), to_replica: 9, n })).await,
        );
        through_embedded.push(match b.transfer(ReplicaId(9), n).await {
            Ok(()) => false,
            Err(SdkError::Exhausted) => true,
            Err(e) => panic!("transfer({n}) failed rather than refused: {e}"),
        });
    }
    assert_eq!(through_sidecar, through_embedded);
    assert_eq!(through_sidecar, [false, true, false]);
    assert_eq!(int(&p, allowance()).await as u64, b.local_allowance().await.unwrap());
    assert_eq!(int(&p, value()).await, b.value().await.unwrap());
    // Check fixed numbers too, so both sides agreeing on wrong numbers cannot
    // pass. 5 added, then 3 and 2 moved out, leaves an allowance of 0. A
    // transfer moves allowance and not units, so it never changes `value()`
    // and the value is still 5.
    assert_eq!(int(&p, value()).await, 5, "a transfer does not change the value");
    assert_eq!(int(&p, allowance()).await, 0, "5 - 3 - 2 of allowance moved out");
}
