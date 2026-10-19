//! One operation per type. Each is chosen so that a wrong translation gives
//! an answer that is clearly wrong, not one that looks right. They cover the
//! results that have structure (pairs, counted items, JSON text) and the two
//! ways of counting positions that differ between languages.

use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use std::sync::Arc;

async fn opened() -> (orvud::server::Dataplane, String) {
    let fabric = Arc::new(MemTransport::new());
    let cfg = NodeConfig::new(ReplicaId(1), "ap-1");
    let client = Arc::new(Client::start(fabric.endpoint(1), cfg).await.expect("engine"));
    let dp = orvud::server::Dataplane::new(client, ReplicaId(1), "default");
    let session = dp
        .open_for_test(pb::OpenRequest {
            namespace: "default".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .expect("open")
        .session;
    (dp, session)
}

async fn exec(dp: &orvud::server::Dataplane, s: &str, op: pb::request::Op) -> pb::Response {
    dp.execute_for_test(pb::Request { session: s.into(), op: Some(op), request_id: Vec::new() })
        .await
        .expect("boundary")
}

#[tokio::test]
async fn a_map_round_trips_its_entries_as_pairs() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::MapPut(pb::MapPut {
        object: "cfg".into(), key: "k".into(), value: "v".into(),
    })).await;
    let r = exec(&dp, &s, pb::request::Op::MapEntries(pb::MapEntries { object: "cfg".into() })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Pairs(p)) })) => {
            assert_eq!(p.pairs.len(), 1);
            assert_eq!(p.pairs[0].key, "k");
            assert_eq!(p.pairs[0].value, "v");
        }
        other => panic!("expected pairs, got {other:?}"),
    }
}

#[tokio::test]
async fn an_mv_map_round_trips_its_entries_with_every_value() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::MvMapPut(pb::MvMapPut {
        object: "notes".into(), key: "k".into(), value: "v".into(),
    })).await;
    let r = exec(&dp, &s, pb::request::Op::MvMapEntries(pb::MvMapEntries { object: "notes".into() })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::MultiPairs(p)) })) => {
            assert_eq!(p.pairs.len(), 1);
            assert_eq!(p.pairs[0].key, "k");
            assert_eq!(p.pairs[0].values, vec!["v".to_string()]);
        }
        other => panic!("expected multi_pairs, got {other:?}"),
    }
    // The same name used as a plain map is the wrong type.
    let r = exec(&dp, &s, pb::request::Op::MapGet(pb::MapGet { object: "notes".into(), key: "k".into() })).await;
    assert!(
        matches!(r.result, Some(pb::response::Result::Failed(ref f)) if f.code == pb::Code::TypeMismatch as i32),
        "got {r:?}"
    );
}

// Reading a map that is not there answers that it is absent, and makes
// nothing: the name can still become another kind of object.
#[tokio::test]
async fn reading_an_mv_map_that_is_not_there_makes_nothing() {
    let (dp, s) = opened().await;
    for op in [
        pb::request::Op::MvMapGet(pb::MvMapGet { object: "later".into(), key: "k".into() }),
        pb::request::Op::MvMapKeys(pb::MvMapKeys { object: "later".into() }),
        pb::request::Op::MvMapEntries(pb::MvMapEntries { object: "later".into() }),
    ] {
        let r = exec(&dp, &s, op).await;
        assert!(
            matches!(r.result, Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) }))),
            "got {r:?}"
        );
    }
    let r = exec(&dp, &s, pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "later".into(), n: 1 })).await;
    assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "got {r:?}");
}

#[tokio::test]
async fn a_cart_quantity_is_signed_on_the_wire() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::CartAdd(pb::CartAdd {
        object: "cart".into(), item: "apple".into(), qty: 5,
    })).await;
    exec(&dp, &s, pb::request::Op::CartRemoveQty(pb::CartRemoveQty {
        object: "cart".into(), item: "apple".into(), qty: 9,
    })).await;
    let r = exec(&dp, &s, pb::request::Op::CartQuantity(pb::CartQuantity {
        object: "cart".into(), item: "apple".into(),
    })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(q)) })) => {
            assert_eq!(q, -4, "a negative quantity is a real convergence outcome, not an error to clamp");
        }
        other => panic!("expected an int, got {other:?}"),
    }
}

#[tokio::test]
async fn an_out_of_range_list_index_is_a_code_not_a_panic() {
    let (dp, s) = opened().await;
    let r = exec(&dp, &s, pb::request::Op::ListInsert(pb::ListInsert {
        object: "items".into(), index: 7, value: "x".into(),
    })).await;
    match r.result {
        Some(pb::response::Result::Failed(f)) => assert_eq!(f.code(), pb::Code::OutOfBounds),
        other => panic!("expected OutOfBounds, got {other:?}"),
    }
}

#[tokio::test]
async fn richtext_indices_are_code_points_not_utf16_units() {
    let (dp, s) = opened().await;
    // U+1F600 is one code point but two UTF-16 units. Inserting at code-point
    // index 1 must put the text after it, not between its two units.
    exec(&dp, &s, pb::request::Op::RichtextInsert(pb::RichtextInsert {
        object: "doc".into(), index: 0, text: "\u{1F600}".into(),
    })).await;
    exec(&dp, &s, pb::request::Op::RichtextInsert(pb::RichtextInsert {
        object: "doc".into(), index: 1, text: "!".into(),
    })).await;
    let r = exec(&dp, &s, pb::request::Op::RichtextText(pb::RichtextText { object: "doc".into() })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::StringValue(t)) })) => {
            assert_eq!(t, "\u{1F600}!");
        }
        other => panic!("expected text, got {other:?}"),
    }
}

// Text put in with a mark carries the mark on exactly its own characters,
// put in at the place asked for, in one operation.
#[tokio::test]
async fn text_inserted_with_a_mark_carries_it_on_itself() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::RichtextInsert(pb::RichtextInsert {
        object: "doc".into(), index: 0, text: "ab".into(),
    })).await;
    let r = exec(&dp, &s, pb::request::Op::RichtextInsertMarked(pb::RichtextInsertMarked {
        object: "doc".into(), index: 1, text: "\u{1F600}x".into(), attr: "by".into(), value: "alice".into(),
    })).await;
    assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "{r:?}");
    let r = exec(&dp, &s, pb::request::Op::RichtextToSpans(pb::RichtextToSpans { object: "doc".into() })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Json(j)) })) => {
            let marked = r#"[{"marks":{},"text":"a"},{"marks":{"by":"alice"},"text":"?x"},{"marks":{},"text":"b"}]"#;
            assert_eq!(j, marked.replace('?', "\u{1F600}"));
        }
        other => panic!("expected spans, got {other:?}"),
    }
    // A mark with no value is a mistake in the request, as for a format.
    let refused = dp
        .execute_for_test(pb::Request {
            session: s.clone(),
            op: Some(pb::request::Op::RichtextInsertMarked(pb::RichtextInsertMarked {
                object: "doc".into(), index: 0, text: "z".into(), attr: "by".into(), value: String::new(),
            })),
            request_id: Vec::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn a_json_document_travels_as_json_text_both_ways() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::JsonSet(pb::JsonSet {
        object: "doc".into(),
        path_json: "[\"a\"]".into(),
        value_json: "42".into(),
    })).await;
    let r = exec(&dp, &s, pb::request::Op::JsonToValue(pb::JsonToValue { object: "doc".into() })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Json(t)) })) => {
            assert_eq!(t, r#"{"a":42}"#);
        }
        other => panic!("expected json text, got {other:?}"),
    }
}

// A list inside a document takes a new item at any position, and two items
// put in keep both. Setting the whole list again would drop what another
// program had added in the meantime.
#[tokio::test]
async fn an_item_is_put_into_a_list_inside_a_json_document() {
    let (dp, s) = opened().await;
    let set = |path: &str, value: &str| pb::request::Op::JsonSet(pb::JsonSet {
        object: "doc".into(), path_json: path.into(), value_json: value.into(),
    });
    let insert = |path: &str, index: u64, value: &str| pb::request::Op::JsonInsert(pb::JsonInsert {
        object: "doc".into(), path_json: path.into(), index, value_json: value.into(),
    });
    let whole = |r: pb::Response| match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Json(t)) })) => t,
        other => panic!("expected json text, got {other:?}"),
    };
    exec(&dp, &s, set(r#"["steps"]"#, r#"["b"]"#)).await;
    for (index, value) in [(0, r#""a""#), (2, r#"{"then":"d"}"#), (2, r#""c""#)] {
        let r = exec(&dp, &s, insert(r#"["steps"]"#, index, value)).await;
        assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "{r:?}");
    }
    let r = exec(&dp, &s, pb::request::Op::JsonToValue(pb::JsonToValue { object: "doc".into() })).await;
    assert_eq!(whole(r), r#"{"steps":["a","b","c",{"then":"d"}]}"#);

    // Past the end, or into something that is not a list, is a failure with
    // a code, and nothing changes.
    for (path, index) in [(r#"["steps"]"#, 9), (r#"["steps",0]"#, 0), (r#"["nothing-here"]"#, 1)] {
        let r = exec(&dp, &s, insert(path, index, "1")).await;
        assert!(matches!(r.result, Some(pb::response::Result::Failed(_))), "{path} {index}: {r:?}");
    }
    let r = exec(&dp, &s, insert(r#"["steps""#, 0, "1")).await;
    match r.result {
        Some(pb::response::Result::Failed(f)) => assert_eq!(f.code(), pb::Code::Json),
        other => panic!("expected Failed(Json), got {other:?}"),
    }
    let r = exec(&dp, &s, pb::request::Op::JsonToValue(pb::JsonToValue { object: "doc".into() })).await;
    assert_eq!(whole(r), r#"{"steps":["a","b","c",{"then":"d"}]}"#);

    // Something that is there and is not a list is left alone. Position 0
    // would otherwise turn it into a list and lose what it held.
    exec(&dp, &s, set(r#"["cfg"]"#, r#"{"a":1}"#)).await;
    exec(&dp, &s, set(r#"["title"]"#, r#""Plan""#)).await;
    exec(&dp, &s, set(r#"["count"]"#, "0")).await;
    for path in [r#"["cfg"]"#, r#"["title"]"#, r#"["count"]"#, "[]"] {
        let r = exec(&dp, &s, insert(path, 0, r#""x""#)).await;
        match r.result {
            Some(pb::response::Result::Failed(f)) => {
                assert_eq!(f.code(), pb::Code::Json, "{path}");
                assert!(f.detail.contains("not a list"), "{path}: {}", f.detail);
            }
            other => panic!("{path}: expected Failed(Json), got {other:?}"),
        }
    }
    let r = exec(&dp, &s, insert(r#"["steps"]"#, u64::MAX, "1")).await;
    assert!(matches!(r.result, Some(pb::response::Result::Failed(_))), "{r:?}");
    let r = exec(&dp, &s, pb::request::Op::JsonToValue(pb::JsonToValue { object: "doc".into() })).await;
    assert_eq!(whole(r), r#"{"cfg":{"a":1},"count":0,"steps":["a","b","c",{"then":"d"}],"title":"Plan"}"#);

    // Where there is nothing yet, position 0 starts a list. A program can
    // add to a list without first finding out whether it exists.
    exec(&dp, &s, insert(r#"["log"]"#, 0, r#""first""#)).await;
    let r = exec(&dp, &s, pb::request::Op::JsonGet(pb::JsonGet { object: "doc".into(), path_json: r#"["log"]"#.into() })).await;
    assert_eq!(whole(r), r#"["first"]"#);
}

// A float holds every whole number up to 2^53 exactly, and only some past it.
// A whole number past it is refused rather than quietly rounded.
#[tokio::test]
async fn a_whole_number_past_two_to_the_53_is_refused() {
    let (dp, s) = opened().await;
    let set = |value: &str| pb::request::Op::JsonSet(pb::JsonSet {
        object: "doc".into(), path_json: r#"["n"]"#.into(), value_json: value.into(),
    });
    for bad in ["9007199254740993", "-1152921504606846977", r#"{"a":[9007199254740993]}"#] {
        match exec(&dp, &s, set(bad)).await.result {
            Some(pb::response::Result::Failed(f)) => {
                assert_eq!(f.code(), pb::Code::Json, "{bad}");
                assert!(f.detail.contains("2^53"), "{bad}: {}", f.detail);
            }
            other => panic!("{bad}: expected Failed(Json), got {other:?}"),
        }
    }
    let r = exec(&dp, &s, set("9007199254740992")).await;
    assert!(matches!(r.result, Some(pb::response::Result::Ack(_))), "{r:?}");
    let r = exec(&dp, &s, pb::request::Op::JsonToValue(pb::JsonToValue { object: "doc".into() })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Json(t)) })) => {
            assert_eq!(t, r#"{"n":9007199254740992}"#)
        }
        other => panic!("expected json text, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_json_text_is_a_json_code_not_a_status() {
    let (dp, s) = opened().await;
    let r = exec(&dp, &s, pb::request::Op::JsonSet(pb::JsonSet {
        object: "doc".into(), path_json: "[\"a\"".into(), value_json: "1".into(),
    })).await;
    match r.result {
        Some(pb::response::Result::Failed(f)) => assert_eq!(f.code(), pb::Code::Json),
        other => panic!("expected Failed(Json), got {other:?}"),
    }
}

#[tokio::test]
async fn a_tree_reports_the_parent_of_one_node() {
    let (dp, s) = opened().await;
    exec(&dp, &s, pb::request::Op::TreeMoveNode(pb::TreeMoveNode {
        object: "t".into(), child: "a".into(), new_parent: "root".into(),
    })).await;
    let parent = |node: &str| pb::request::Op::TreeParent(pb::TreeParent { object: "t".into(), node: node.into() });
    let r = exec(&dp, &s, parent("a")).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::StringValue(p)) })) => {
            assert_eq!(p, "root");
        }
        other => panic!("expected a parent, got {other:?}"),
    }
    // A node that was never placed has none, and says so.
    for node in ["root", "nobody"] {
        let r = exec(&dp, &s, parent(node)).await;
        assert!(
            matches!(r.result, Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Absent(_)) }))),
            "{node}: {r:?}"
        );
    }
}

#[tokio::test]
async fn a_tree_reports_its_children_in_order() {
    let (dp, s) = opened().await;
    for (child, idx) in [("b", 0u64), ("a", 0)] {
        exec(&dp, &s, pb::request::Op::TreeMoveNodeAt(pb::TreeMoveNodeAt {
            object: "t".into(), child: child.into(), new_parent: "root".into(), index: idx,
        })).await;
    }
    let r = exec(&dp, &s, pb::request::Op::TreeChildren(pb::TreeChildren {
        object: "t".into(), node: "root".into(),
    })).await;
    match r.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::Strings(l)) })) => {
            assert_eq!(l.values, vec!["a".to_string(), "b".to_string()],
                       "ordered children: 'a' was inserted at index 0, ahead of 'b'");
        }
        other => panic!("expected children, got {other:?}"),
    }
}
