//! Converts between the engine's data types (`orvu-crdt`) and the message
//! types (`orvu-proto`).
//!
//! All the converting is done here. That is what lets `orvu-proto` stay
//! independent of `orvu-crdt`.

use crate::error::{Error, Result};
use crate::object::TypedDelta;
use orvu_crdt::{
    BoundedDelta, DeltaCrdt, Dot, DotContext, Element, Generation, Hlc, JsonCrdt, JsonDelta, JsonScalar,
    JsonTag, LwwDelta, MvMapDelta, OrMapDelta, OrSetDelta, OrderedF64, PnDelta, ReplicaId, RgaDelta, Shape,
    TreeDelta, VersionVector,
};
use orvu_proto::v1 as pb;
use prost::Message;
use std::collections::{BTreeMap, BTreeSet};

fn required<T>(value: Option<T>, field: &'static str) -> Result<T> {
    value.ok_or(Error::MissingField(field))
}

pub fn encode_hlc(h: Hlc) -> pb::Hlc {
    pb::Hlc {
        wall: h.wall,
        logical: h.logical,
    }
}

pub fn decode_hlc(h: &pb::Hlc) -> Hlc {
    Hlc {
        wall: h.wall,
        logical: h.logical,
    }
}

pub fn encode_dot(d: Dot) -> pb::Dot {
    pb::Dot {
        replica: d.replica.0,
        counter: d.counter,
    }
}

/// The highest counter a dot may have. See `orvu_crdt::MAX_COUNTER`.
pub use orvu_crdt::MAX_COUNTER;

/// Refuses a counter above `MAX_COUNTER`. Writes here stop at the same limit,
/// so a change made by an honest machine is never refused here.
fn check_counter(counter: u64) -> Result<u64> {
    if counter > MAX_COUNTER {
        return Err(Error::MalformedPayload(format!(
            "counter {counter} is above the highest allowed ({MAX_COUNTER})"
        )));
    }
    Ok(counter)
}

/// Reads a dot. A counter above `MAX_COUNTER` is refused.
pub fn decode_dot(d: &pb::Dot) -> Result<Dot> {
    Ok(Dot::new(ReplicaId(d.replica), check_counter(d.counter)?))
}

fn decode_dot_set(ds: &pb::DotSet) -> Result<BTreeSet<Dot>> {
    ds.dots.iter().map(decode_dot).collect()
}

/// Reads the dots each key of a map or each element of a set is tagged with.
/// A dot names one write to one key, so a dot listed under two keys is
/// refused: a remove of either key would take it from both.
fn decode_key_dots<K: Ord + Clone + std::fmt::Debug>(
    entries: impl IntoIterator<Item = (K, Option<pb::DotSet>)>,
) -> Result<BTreeMap<K, BTreeSet<Dot>>> {
    let mut owner: BTreeMap<Dot, K> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for (k, set) in entries {
        let dots = match set { Some(set) => decode_dot_set(&set)?, None => BTreeSet::new() };
        for &d in &dots {
            one_owner(&mut owner, d, &k)?;
        }
        out.insert(k, dots);
    }
    Ok(out)
}

/// Notes that dot `d` belongs to `k`, and refuses it if another key has it.
fn one_owner<K: Clone + PartialEq + std::fmt::Debug>(owner: &mut BTreeMap<Dot, K>, d: Dot, k: &K) -> Result<()> {
    match owner.get(&d) {
        Some(other) if other != k => Err(Error::MalformedPayload(format!(
            "dot ({}, {}) is listed under both {other:?} and {k:?}",
            d.replica.0, d.counter
        ))),
        Some(_) => Ok(()),
        None => {
            owner.insert(d, k.clone());
            Ok(())
        }
    }
}

/// Reads what a list item was inserted after. It says so in two ways, a flag
/// and whether the dot is there, and the two must agree.
fn decode_predecessor(has: bool, predecessor: Option<&pb::Dot>) -> Result<Option<Dot>> {
    match (has, predecessor) {
        (true, Some(p)) => Ok(Some(decode_dot(p)?)),
        (false, None) => Ok(None),
        (true, None) => Err(Error::MalformedPayload("a list item says it follows another but does not name it".into())),
        (false, Some(_)) => Err(Error::MalformedPayload("a list item says it is at the front but names one it follows".into())),
    }
}

/// Adds a list item, refusing an id that is already there.
fn insert_item<T>(elements: &mut BTreeMap<Dot, Element<T>>, id: Dot, el: Element<T>) -> Result<()> {
    if elements.insert(id, el).is_some() {
        return Err(Error::MalformedPayload(format!(
            "list item ({}, {}) came twice",
            id.replica.0, id.counter
        )));
    }
    Ok(())
}

pub fn encode_context(ctx: &DotContext) -> pb::DotContext {
    pb::DotContext {
        prefix: Some(pb::VersionVector {
            prefix: ctx
                .version_vector()
                .iter()
                .map(|(r, c)| (r.0, c))
                .collect(),
        }),
        cloud: ctx.cloud().map(encode_dot).collect(),
    }
}

/// Reads a context. A counter above `MAX_COUNTER`, in the version vector or
/// in the cloud, is refused.
pub fn decode_context(ctx: &pb::DotContext) -> Result<DotContext> {
    let mut vv = VersionVector::new();
    if let Some(prefix) = &ctx.prefix {
        for (&replica, &counter) in &prefix.prefix {
            vv.observe(ReplicaId(replica), check_counter(counter)?);
        }
    }
    let cloud = ctx.cloud.iter().map(decode_dot).collect::<Result<Vec<_>>>()?;
    Ok(DotContext::from_parts(vv, cloud))
}

fn encode_shape(s: Shape) -> String {
    match s { Shape::Scalar => "S", Shape::Object => "O", Shape::Array => "A", Shape::Absent => "N" }.to_string()
}
fn decode_shape(s: &str) -> Result<Shape> {
    match s {
        "S" => Ok(Shape::Scalar),
        "O" => Ok(Shape::Object),
        "A" => Ok(Shape::Array),
        "N" => Ok(Shape::Absent),
        other => Err(Error::MalformedPayload(format!("unknown JSON shape tag {other:?}"))),
    }
}
fn encode_scalar(v: &JsonScalar) -> String {
    match v {
        JsonScalar::Null => "z".to_string(),
        JsonScalar::Bool(b) => format!("b{}", if *b { 1 } else { 0 }),
        JsonScalar::Num(n) => format!("d{}", n.0.to_bits()),
        JsonScalar::Str(s) => format!("s{s}"),
    }
}
fn decode_scalar(s: &str) -> Result<JsonScalar> {
    let bad = || Error::MalformedPayload(format!("unknown JSON scalar {s:?}"));
    // The first character says what kind of value follows.
    let rest = s.get(1..).unwrap_or("");
    match s.chars().next() {
        Some('z') if rest.is_empty() => Ok(JsonScalar::Null),
        Some('b') if rest == "0" => Ok(JsonScalar::Bool(false)),
        Some('b') if rest == "1" => Ok(JsonScalar::Bool(true)),
        Some('d') => rest
            .parse::<u64>()
            .map(|bits| JsonScalar::Num(OrderedF64(f64::from_bits(bits))))
            .map_err(|_| bad()),
        Some('s') => Ok(JsonScalar::Str(rest.to_string())),
        _ => Err(bad()),
    }
}

fn encode_lww_shape(d: &LwwDelta<JsonTag>) -> pb::LwwRegisterDelta {
    pb::LwwRegisterDelta { value: d.value().map(|t| encode_shape(t.shape)), ts: Some(encode_hlc(d.ts())), writer: d.writer().0 }
}
/// The highest `step` a JSON generation may have in a change that arrives.
/// It is the same limit a write here keeps to, so that a change made here is
/// never refused elsewhere.
pub use orvu_crdt::MAX_GENERATION_STEP;

// The zero generation is left out, and reads back as zero.
fn encode_generation(g: Generation) -> Option<pb::JsonGeneration> {
    (g != Generation::default())
        .then(|| pb::JsonGeneration { ts: Some(encode_hlc(g.ts)), writer: g.writer.0, step: g.step })
}
fn decode_generation(g: Option<&pb::JsonGeneration>) -> Result<Generation> {
    match g {
        None => Ok(Generation::default()),
        Some(g) if g.step > MAX_GENERATION_STEP => {
            Err(Error::MalformedPayload(format!("a JSON generation {} steps on is more than a document can reach", g.step)))
        }
        Some(g) => Ok(Generation {
            ts: decode_hlc(required(g.ts.as_ref(), "JsonGeneration.ts")?),
            writer: ReplicaId(g.writer),
            step: g.step,
        }),
    }
}
fn encode_lww_scalar(d: &LwwDelta<JsonScalar>) -> pb::LwwRegisterDelta {
    pb::LwwRegisterDelta { value: d.value().map(encode_scalar), ts: Some(encode_hlc(d.ts())), writer: d.writer().0 }
}

fn encode_json_delta(d: &JsonDelta) -> pb::JsonValueDelta {
    pb::JsonValueDelta {
        tag: d.tag().map(encode_lww_shape),
        generation: d.tag().and_then(|t| t.value()).and_then(|t| encode_generation(t.generation)),
        parent: d.tag().and_then(|t| t.value()).and_then(|t| encode_generation(t.parent)),
        scalar: d.scalar().map(encode_lww_scalar),
        object: d.object().map(|od| pb::JsonObjectDelta {
            dots: od
                .dots()
                .map(|((g, k), dots)| pb::JsonFieldDots {
                    generation: encode_generation(*g),
                    key: k.clone(),
                    dots: Some(pb::DotSet { dots: dots.iter().copied().map(encode_dot).collect() }),
                })
                .collect(),
            values: od
                .values()
                .map(|((g, k), vd)| pb::JsonFieldValue {
                    generation: encode_generation(*g),
                    key: k.clone(),
                    value: Some(encode_json_delta(vd)),
                })
                .collect(),
            ctx: Some(encode_context(od.context())),
        }),
        array: d.array().map(|ad| pb::JsonArrayDelta {
            elements: ad.elements().map(|(dot, el)| pb::JsonArrayElement {
                id: Some(encode_dot(*dot)),
                predecessor: el.predecessor.map(encode_dot),
                has_predecessor: el.predecessor.is_some(),
                value: Some(encode_json_delta(&el.value.snapshot())),
            }).collect(),
            tombstones: Some(pb::DotSet { dots: ad.tombstones().copied().map(encode_dot).collect() }),
            ctx: Some(encode_context(ad.context())),
        }),
    }
}

fn decode_json_delta(d: &pb::JsonValueDelta) -> Result<JsonDelta> {
    // A part's timestamp must be there only if the part itself is there.
    let has_value = d.tag.as_ref().is_some_and(|t| t.value.is_some());
    if !has_value && (d.generation.is_some() || d.parent.is_some()) {
        return Err(Error::MalformedPayload("a JSON generation came without the tag value it belongs to".into()));
    }
    let tag = match d.tag.as_ref() {
        Some(t) => {
            let value = match t.value.as_deref() {
                Some(shape) => Some(JsonTag {
                    shape: decode_shape(shape)?,
                    generation: decode_generation(d.generation.as_ref())?,
                    parent: decode_generation(d.parent.as_ref())?,
                }),
                None => None,
            };
            Some(LwwDelta::from_parts(value, decode_hlc(required(t.ts.as_ref(), "JsonValueDelta.tag.ts")?), ReplicaId(t.writer)))
        }
        None => None,
    };
    let scalar = match d.scalar.as_ref() {
        Some(t) => Some(LwwDelta::from_parts(
            t.value.as_deref().map(decode_scalar).transpose()?, decode_hlc(required(t.ts.as_ref(), "JsonValueDelta.scalar.ts")?), ReplicaId(t.writer),
        )),
        None => None,
    };
    let object = match d.object.as_ref() {
        Some(od) => {
            let ctx = decode_context(required(od.ctx.as_ref(), "JsonObjectDelta.ctx")?)?;
            // A field named twice would leave it unclear which one counts.
            let twice = |k: &(Generation, String)| {
                Error::MalformedPayload(format!("the JSON field {:?} came twice", k.1))
            };
            let mut dots: BTreeMap<(Generation, String), BTreeSet<Dot>> = BTreeMap::new();
            let mut owner = BTreeMap::new();
            for f in &od.dots {
                let k = (decode_generation(f.generation.as_ref())?, f.key.clone());
                let set = match f.dots.as_ref() { Some(ds) => decode_dot_set(ds)?, None => BTreeSet::new() };
                for &d in &set {
                    one_owner(&mut owner, d, &k)?;
                }
                if dots.insert(k.clone(), set).is_some() { return Err(twice(&k)); }
            }
            let mut values = BTreeMap::new();
            for f in &od.values {
                let k = (decode_generation(f.generation.as_ref())?, f.key.clone());
                let vd = decode_json_delta(required(f.value.as_ref(), "JsonFieldValue.value")?)?;
                if values.insert(k.clone(), vd).is_some() { return Err(twice(&k)); }
            }
            Some(OrMapDelta::from_parts(dots, values, ctx))
        }
        None => None,
    };
    let array = match d.array.as_ref() {
        Some(ad) => {
            let ctx = decode_context(required(ad.ctx.as_ref(), "JsonArrayDelta.ctx")?)?;
            let mut elements = BTreeMap::new();
            for el in &ad.elements {
                let id = decode_dot(required(el.id.as_ref(), "JsonArrayElement.id")?)?;
                let predecessor = decode_predecessor(el.has_predecessor, el.predecessor.as_ref())?;
                let mut child = JsonCrdt::default();
                child.merge(decode_json_delta(required(el.value.as_ref(), "JsonArrayElement.value")?)?);
                insert_item(&mut elements, id, Element { predecessor, value: child })?;
            }
            let tombstones = match ad.tombstones.as_ref() { Some(ds) => decode_dot_set(ds)?, None => BTreeSet::new() };
            Some(RgaDelta::from_parts(elements, tombstones, ctx))
        }
        None => None,
    };
    Ok(JsonDelta::from_parts(tag, object, array, scalar))
}

fn encode_rga_string_delta(d: &orvu_crdt::RgaDelta<String>) -> pb::RgaDelta {
    pb::RgaDelta {
        elements: d.elements().map(|(dot, el)| pb::RgaElement {
            id: Some(encode_dot(*dot)),
            predecessor: el.predecessor.map(encode_dot),
            has_predecessor: el.predecessor.is_some(),
            value: el.value.clone(),
        }).collect(),
        tombstones: Some(pb::DotSet { dots: d.tombstones().copied().map(encode_dot).collect() }),
        ctx: Some(encode_context(d.context())),
    }
}

fn decode_rga_string_delta(d: pb::RgaDelta) -> Result<orvu_crdt::RgaDelta<String>> {
    let ctx = decode_context(required(d.ctx.as_ref(), "RgaDelta.ctx")?)?;
    let mut elements = BTreeMap::new();
    for el in d.elements {
        let id = decode_dot(required(el.id.as_ref(), "RgaElement.id")?)?;
        let predecessor = decode_predecessor(el.has_predecessor, el.predecessor.as_ref())?;
        insert_item(&mut elements, id, Element { predecessor, value: el.value })?;
    }
    let tombstones = match d.tombstones.as_ref() { Some(ds) => decode_dot_set(ds)?, None => BTreeSet::new() };
    Ok(orvu_crdt::RgaDelta::from_parts(elements, tombstones, ctx))
}

fn encode_ormap_lww_delta(d: &orvu_crdt::OrMapDelta<String, orvu_crdt::LwwRegister<String>>) -> pb::OrMapDelta {
    pb::OrMapDelta {
        dots: d.dots().map(|(k, dots)| (k.clone(), pb::DotSet { dots: dots.iter().copied().map(encode_dot).collect() })).collect(),
        values: d.values().map(|(k, vd)| (k.clone(), pb::LwwRegisterDelta {
            value: vd.value().cloned(), ts: Some(encode_hlc(vd.ts())), writer: vd.writer().0,
        })).collect(),
        ctx: Some(encode_context(d.context())),
    }
}

fn decode_ormap_lww_delta(d: pb::OrMapDelta) -> Result<orvu_crdt::OrMapDelta<String, orvu_crdt::LwwRegister<String>>> {
    let ctx = decode_context(required(d.ctx.as_ref(), "OrMapDelta.ctx")?)?;
    let dots = decode_key_dots(d.dots.into_iter().map(|(k, set)| (k, Some(set))))?;
    let mut values = std::collections::BTreeMap::new();
    for (k, vd) in d.values {
        let ts = decode_hlc(required(vd.ts.as_ref(), "OrMapDelta.values.ts")?);
        values.insert(k, orvu_crdt::LwwDelta::from_parts(vd.value, ts, ReplicaId(vd.writer)));
    }
    Ok(orvu_crdt::OrMapDelta::from_parts(dots, values, ctx))
}

/// Encodes a delta as the bytes that go in `Delta.payload`.
pub fn encode_delta_payload(delta: &TypedDelta) -> Vec<u8> {
    match delta {
        TypedDelta::PnCounter(d) => pb::PnCounterDelta {
            inc: d.inc().iter().map(|(r, &v)| (r.0, v)).collect(),
            dec: d.dec().iter().map(|(r, &v)| (r.0, v)).collect(),
        }
        .encode_to_vec(),

        TypedDelta::LwwRegister(d) => pb::LwwRegisterDelta {
            value: d.value().cloned(),
            ts: Some(encode_hlc(d.ts())),
            writer: d.writer().0,
        }
        .encode_to_vec(),

        TypedDelta::OrSet(d) => pb::OrSetDelta {
            elements: d
                .elements()
                .map(|(e, dots)| {
                    (
                        e.clone(),
                        pb::DotSet {
                            dots: dots.iter().copied().map(encode_dot).collect(),
                        },
                    )
                })
                .collect(),
            ctx: Some(encode_context(d.context())),
        }
        .encode_to_vec(),

        TypedDelta::OrMap(d) => encode_ormap_lww_delta(d).encode_to_vec(),

        TypedDelta::OrMapCounter(d) => pb::OrMapCounterDelta {
            dots: d
                .dots()
                .map(|(k, dots)| {
                    (
                        k.clone(),
                        pb::DotSet {
                            dots: dots.iter().copied().map(encode_dot).collect(),
                        },
                    )
                })
                .collect(),
            values: d
                .values()
                .map(|(k, vd)| {
                    (
                        k.clone(),
                        pb::PnCounterDelta {
                            inc: vd.inc().iter().map(|(r, &v)| (r.0, v)).collect(),
                            dec: vd.dec().iter().map(|(r, &v)| (r.0, v)).collect(),
                        },
                    )
                })
                .collect(),
            ctx: Some(encode_context(d.context())),
        }
        .encode_to_vec(),

        TypedDelta::Rga(d) => encode_rga_string_delta(d).encode_to_vec(),

        TypedDelta::Json(d) => encode_json_delta(d).encode_to_vec(),

        TypedDelta::RichText(d) => pb::RichTextDelta {
            text: d.text().map(encode_rga_string_delta),
            marks: d.marks().map(encode_ormap_lww_delta),
        }.encode_to_vec(),

        TypedDelta::BoundedCounter(d) => pb::BoundedCounterDelta {
            deposits: d.deposits().iter().map(|(r, &v)| (r.0, v)).collect(),
            withdrawals: d.withdrawals().iter().map(|(r, &v)| (r.0, v)).collect(),
            transfers: d.transfers().iter().map(|(&(from, to), &amount)| pb::Transfer {
                from_replica: from.0, to: to.0, amount,
            }).collect(),
        }.encode_to_vec(),

        TypedDelta::Tree(d) => pb::TreeDelta {
            moves: d
                .wire_parts()
                .map(|(hlc, replica, child, new_parent)| pb::TreeMove {
                    hlc: Some(encode_hlc(hlc)),
                    replica: replica.0,
                    child: child.to_string(),
                    new_parent: new_parent.to_string(),
                })
                .collect(),
            labels: d.labels().map(encode_ormap_lww_delta),
            positions: d.positions().map(encode_ormap_lww_delta),
        }
        .encode_to_vec(),

        TypedDelta::MvMap(d) => pb::MvMapDelta {
            entries: d.entries().map(|(k, vals)| (k.clone(), pb::MvValues {
                values: vals.iter().map(|(&dot, v)| pb::MvValue {
                    dot: Some(encode_dot(dot)), value: v.clone(),
                }).collect(),
            })).collect(),
            ctx: Some(encode_context(d.context())),
        }
        .encode_to_vec(),
    }
}

/// Reads `Delta.payload` back into a delta of the type the message names.
pub fn decode_delta_payload(kind: pb::CrdtType, bytes: &[u8]) -> Result<TypedDelta> {
    let malformed = |e: prost::DecodeError| Error::MalformedPayload(e.to_string());

    match kind {
        pb::CrdtType::PnCounter => {
            let d = pb::PnCounterDelta::decode(bytes).map_err(malformed)?;
            Ok(TypedDelta::PnCounter(PnDelta::from_parts(
                d.inc.into_iter().map(|(r, v)| (ReplicaId(r), v)).collect(),
                d.dec.into_iter().map(|(r, v)| (ReplicaId(r), v)).collect(),
            )))
        }

        pb::CrdtType::LwwRegister => {
            let d = pb::LwwRegisterDelta::decode(bytes).map_err(malformed)?;
            let ts = decode_hlc(required(d.ts.as_ref(), "LwwRegisterDelta.ts")?);
            Ok(TypedDelta::LwwRegister(LwwDelta::from_parts(
                d.value,
                ts,
                ReplicaId(d.writer),
            )))
        }

        pb::CrdtType::OrSet => {
            let d = pb::OrSetDelta::decode(bytes).map_err(malformed)?;
            let ctx = decode_context(required(d.ctx.as_ref(), "OrSetDelta.ctx")?)?;
            let dots = decode_key_dots(d.elements.into_iter().map(|(e, set)| (e, Some(set))))?;
            Ok(TypedDelta::OrSet(OrSetDelta::from_parts(dots, ctx)))
        }

        pb::CrdtType::OrMap => {
            let d = pb::OrMapDelta::decode(bytes).map_err(malformed)?;
            Ok(TypedDelta::OrMap(decode_ormap_lww_delta(d)?))
        }

        pb::CrdtType::OrMapCounter => {
            let d = pb::OrMapCounterDelta::decode(bytes).map_err(malformed)?;
            let ctx = decode_context(required(d.ctx.as_ref(), "OrMapCounterDelta.ctx")?)?;
            let dots = decode_key_dots(d.dots.into_iter().map(|(k, set)| (k, Some(set))))?;
            let mut values = BTreeMap::new();
            for (k, vd) in d.values {
                values.insert(
                    k,
                    PnDelta::from_parts(
                        vd.inc.into_iter().map(|(r, v)| (ReplicaId(r), v)).collect(),
                        vd.dec.into_iter().map(|(r, v)| (ReplicaId(r), v)).collect(),
                    ),
                );
            }
            Ok(TypedDelta::OrMapCounter(OrMapDelta::from_parts(dots, values, ctx)))
        }

        pb::CrdtType::RgaList => {
            let d = pb::RgaDelta::decode(bytes).map_err(malformed)?;
            Ok(TypedDelta::Rga(decode_rga_string_delta(d)?))
        }

        pb::CrdtType::Json => {
            let d = pb::JsonValueDelta::decode(bytes).map_err(malformed)?;
            Ok(TypedDelta::Json(decode_json_delta(&d)?))
        }

        pb::CrdtType::RichText => {
            let d = pb::RichTextDelta::decode(bytes).map_err(malformed)?;
            let text = match d.text { Some(t) => Some(decode_rga_string_delta(t)?), None => None };
            let marks = match d.marks { Some(m) => Some(decode_ormap_lww_delta(m)?), None => None };
            Ok(TypedDelta::RichText(orvu_crdt::RichTextDelta::from_parts(text, marks)))
        }

        pb::CrdtType::BoundedCounter => {
            let d = pb::BoundedCounterDelta::decode(bytes).map_err(malformed)?;
            let deposits = d.deposits.into_iter().map(|(r, v)| (ReplicaId(r), v)).collect();
            let withdrawals = d.withdrawals.into_iter().map(|(r, v)| (ReplicaId(r), v)).collect();
            let transfers = d.transfers.into_iter()
                .map(|t| ((ReplicaId(t.from_replica), ReplicaId(t.to)), t.amount))
                .collect();
            Ok(TypedDelta::BoundedCounter(BoundedDelta::from_parts(deposits, withdrawals, transfers)))
        }

        pb::CrdtType::Tree => {
            let d = pb::TreeDelta::decode(bytes).map_err(malformed)?;
            let mut ops = Vec::with_capacity(d.moves.len());
            for m in d.moves {
                let hlc = decode_hlc(&required(m.hlc, "TreeMove.hlc")?);
                ops.push((hlc, ReplicaId(m.replica), m.child, m.new_parent));
            }
            let labels = match d.labels { Some(m) => Some(decode_ormap_lww_delta(m)?), None => None };
            let positions = match d.positions { Some(m) => Some(decode_ormap_lww_delta(m)?), None => None };
            Ok(TypedDelta::Tree(
                TreeDelta::from_wire(ops).with_labels(labels).with_positions(positions),
            ))
        }

        pb::CrdtType::MvMap => {
            let d = pb::MvMapDelta::decode(bytes).map_err(malformed)?;
            let ctx = decode_context(required(d.ctx.as_ref(), "MvMapDelta.ctx")?)?;
            let mut delta = MvMapDelta::from_parts(BTreeMap::new(), ctx);
            let mut owner = BTreeMap::new();
            for (k, vals) in d.entries {
                for mv in vals.values {
                    let dot = decode_dot(required(mv.dot.as_ref(), "MvValue.dot")?)?;
                    one_owner(&mut owner, dot, &k)?;
                    delta.add(k.clone(), dot, mv.value);
                }
            }
            Ok(TypedDelta::MvMap(delta))
        }

        pb::CrdtType::Unspecified => Err(Error::UnknownCrdtType(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::WireDelta;
    use orvu_crdt::{
        BoundedCounter, DeltaCrdt, JsonCrdt, JsonValue, LwwRegister, MvMap, OrMap, OrSet, PnCounter, Rga,
        RichText, Seg, Tree, MAX_COUNTER,
    };

    // A JSON document nested as deep as `orvu-crdt` allows must still decode,
    // both as a delta and as a snapshot of the whole document.
    #[test]
    fn json_at_the_nesting_limit_round_trips() {
        let mut value = JsonValue::Num(1.0);
        for _ in 0..orvu_crdt::MAX_JSON_NESTING {
            value = JsonValue::Array(vec![value]);
        }
        let mut doc = JsonCrdt::default();
        let delta = doc.set(&[], &value, Hlc { wall: 1, logical: 0 }, ReplicaId(1)).unwrap();
        round_trip(TypedDelta::Json(delta));
        round_trip(TypedDelta::Json(doc.snapshot()));
    }

    fn pdot(replica: u64, counter: u64) -> pb::Dot {
        pb::Dot { replica, counter }
    }

    fn pdots(dots: &[(u64, u64)]) -> pb::DotSet {
        pb::DotSet { dots: dots.iter().map(|&(r, c)| pdot(r, c)).collect() }
    }

    fn malformed(r: Result<TypedDelta>) -> bool {
        matches!(r, Err(Error::MalformedPayload(_)))
    }

    // Another machine chooses these numbers. A counter above the cap is one
    // no machine reaches by writing, so it is refused wherever it appears: in
    // a context's version vector or cloud, a dot, or an item's id.
    #[test]
    fn a_counter_above_the_cap_is_refused() {
        let top = MAX_COUNTER;
        let ctx_with = |prefix: u64, cloud: u64| pb::DotContext {
            prefix: Some(pb::VersionVector { prefix: [(1u64, prefix)].into_iter().collect() }),
            cloud: vec![pdot(2, cloud)],
        };
        let set = |dot: u64, ctx: pb::DotContext| {
            let d = pb::OrSetDelta { elements: [("a".to_string(), pdots(&[(1, dot)]))].into_iter().collect(), ctx: Some(ctx) };
            decode_delta_payload(pb::CrdtType::OrSet, &d.encode_to_vec())
        };
        assert!(set(top, ctx_with(top, top)).is_ok(), "the highest allowed is fine");
        assert!(malformed(set(MAX_COUNTER + 1, ctx_with(1, 3))), "a dot");
        assert!(malformed(set(1, ctx_with(MAX_COUNTER + 1, 3))), "a version vector");
        assert!(malformed(set(1, ctx_with(u64::MAX, 3))), "the very top");
        assert!(malformed(set(1, ctx_with(1, MAX_COUNTER + 1))), "a cloud dot");

        let mv = |dot: u64| {
            let d = pb::MvMapDelta {
                entries: [("k".to_string(), pb::MvValues { values: vec![pb::MvValue { dot: Some(pdot(1, dot)), value: "v".into() }] })].into_iter().collect(),
                ctx: Some(pb::DotContext::default()),
            };
            decode_delta_payload(pb::CrdtType::MvMap, &d.encode_to_vec())
        };
        assert!(mv(top).is_ok());
        assert!(malformed(mv(MAX_COUNTER + 1)));

        let list = |id: u64, pred: u64, tomb: u64| {
            let d = pb::RgaDelta {
                elements: vec![pb::RgaElement { id: Some(pdot(1, id)), predecessor: Some(pdot(1, pred)), has_predecessor: true, value: "v".into() }],
                tombstones: Some(pdots(&[(1, tomb)])),
                ctx: Some(pb::DotContext::default()),
            };
            decode_delta_payload(pb::CrdtType::RgaList, &d.encode_to_vec())
        };
        assert!(list(top, top, top).is_ok());
        assert!(malformed(list(MAX_COUNTER + 1, 1, 1)), "an item's id");
        assert!(malformed(list(2, MAX_COUNTER + 1, 1)), "the item it goes after");
        assert!(malformed(list(2, 1, MAX_COUNTER + 1)), "a tombstone");

        // The dot and the context a delta travels with.
        let mut wd = pb::Delta::decode(crate::codec::encode_wire_delta(&WireDelta {
            object_id: "o".into(),
            dot: Dot::new(ReplicaId(1), 1),
            hlc: Hlc::ZERO,
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(PnCounter::default().increment(ReplicaId(1), 1)),
            trace_id: crate::trace::TraceId::none(),
        }).as_slice()).unwrap();
        wd.dot = Some(pdot(1, MAX_COUNTER + 1));
        assert!(matches!(crate::codec::decode_wire_delta(&wd.encode_to_vec()), Err(Error::MalformedPayload(_))));
        wd.dot = Some(pdot(1, 1));
        wd.deps = Some(ctx_with(u64::MAX, 3));
        assert!(matches!(crate::codec::decode_wire_delta(&wd.encode_to_vec()), Err(Error::MalformedPayload(_))));
    }

    // Whatever another machine sends, a write made here is either refused
    // here, loudly, or taken in by every other machine. A list inserts after
    // the highest counter it holds from anyone, and a set goes on from the
    // highest of its own counters it has seen. So if a received counter
    // could sit just under a lower limit than the one writes stop at, the
    // write after it would succeed here and be refused everywhere else, and
    // the copies would drift apart without anyone being told.
    #[test]
    fn a_write_made_here_is_never_refused_elsewhere() {
        use crate::codec::{decode_wire_delta, encode_wire_delta};
        use crate::object::{Object, TypedCrdt};

        // Takes in `received` from another machine, then tries one write
        // here as machine 1. `None` if the write was refused here.
        let write_after = |kind: pb::CrdtType, received: TypedDelta| -> Option<WireDelta> {
            let mut o = Object::new("o", TypedCrdt::empty(kind).unwrap());
            o.merge_snapshot(received, DotContext::new()).unwrap();
            o.try_mutate(ReplicaId(1), Hlc::ZERO, |crdt, _, r| match crdt {
                TypedCrdt::Rga(l) => l.insert_after(None, "mine".into(), r).ok().map(TypedDelta::Rga),
                TypedCrdt::OrSet(s) => s.add("mine".into(), r).ok().map(TypedDelta::OrSet),
                other => panic!("unexpected {other:?}"),
            })
            .ok()
            .flatten()
        };
        let list = |id: u64| {
            let d = pb::RgaDelta {
                elements: vec![pb::RgaElement { id: Some(pdot(9, id)), predecessor: None, has_predecessor: false, value: "v".into() }],
                tombstones: Some(pdots(&[])),
                ctx: Some(pb::DotContext::default()),
            };
            decode_delta_payload(pb::CrdtType::RgaList, &d.encode_to_vec())
        };
        let set = |own: u64| {
            let d = pb::OrSetDelta {
                elements: Default::default(),
                ctx: Some(pb::DotContext {
                    prefix: Some(pb::VersionVector { prefix: [(1u64, own)].into_iter().collect() }),
                    cloud: vec![],
                }),
            };
            decode_delta_payload(pb::CrdtType::OrSet, &d.encode_to_vec())
        };

        let near_top = [1 << 20, MAX_COUNTER - (1 << 40) - 1, MAX_COUNTER - (1 << 40), MAX_COUNTER - 2, MAX_COUNTER - 1, MAX_COUNTER, MAX_COUNTER + 1];
        for c in near_top {
            for (kind, received) in [(pb::CrdtType::RgaList, list(c)), (pb::CrdtType::OrSet, set(c))] {
                let Ok(received) = received else { continue };
                if let Some(wd) = write_after(kind, received) {
                    assert!(
                        decode_wire_delta(&encode_wire_delta(&wd)).is_ok(),
                        "{kind:?}: a write after taking in counter {c} works here but is refused elsewhere"
                    );
                }
            }
        }

        // Every counter up to the limit is taken in, and a write that would
        // go past it is refused here, so the writer is told.
        let Ok(at_limit) = list(MAX_COUNTER) else { panic!("a list item at the limit was refused") };
        assert!(write_after(pb::CrdtType::RgaList, at_limit).is_none());
        let Ok(at_limit) = set(MAX_COUNTER) else { panic!("a set context at the limit was refused") };
        assert!(write_after(pb::CrdtType::OrSet, at_limit).is_none());
        let Ok(below) = list(MAX_COUNTER - 1) else { panic!("a list item below the limit was refused") };
        assert!(write_after(pb::CrdtType::RgaList, below).is_some());
    }

    // A dot names one write to one key. A change that lists it under two
    // keys could only come from a broken or hostile sender, and a remove of
    // either key would take it from both, so it is refused.
    #[test]
    fn one_dot_under_two_keys_is_refused() {
        let two = |a: &[(u64, u64)], b: &[(u64, u64)]| -> BTreeMap<String, pb::DotSet> {
            [("a".to_string(), pdots(a)), ("b".to_string(), pdots(b))].into_iter().collect()
        };
        let set = |a: &[(u64, u64)], b: &[(u64, u64)]| {
            let d = pb::OrSetDelta { elements: two(a, b), ctx: Some(pb::DotContext::default()) };
            decode_delta_payload(pb::CrdtType::OrSet, &d.encode_to_vec())
        };
        assert!(set(&[(1, 1)], &[(1, 2)]).is_ok());
        assert!(malformed(set(&[(1, 1)], &[(1, 1)])));
        assert!(malformed(set(&[(1, 1), (2, 5)], &[(1, 3), (2, 5)])));

        let map = |a: &[(u64, u64)], b: &[(u64, u64)]| {
            let d = pb::OrMapDelta { dots: two(a, b), values: Default::default(), ctx: Some(pb::DotContext::default()) };
            decode_delta_payload(pb::CrdtType::OrMap, &d.encode_to_vec())
        };
        assert!(map(&[(1, 1)], &[(1, 2)]).is_ok());
        assert!(malformed(map(&[(1, 1)], &[(1, 1)])));
        // Rich text and the tree carry their marks, labels and positions as
        // such a map too.
        let marks = pb::OrMapDelta { dots: two(&[(1, 1)], &[(1, 1)]), values: Default::default(), ctx: Some(pb::DotContext::default()) };
        let rt = pb::RichTextDelta { text: None, marks: Some(marks.clone()) };
        assert!(malformed(decode_delta_payload(pb::CrdtType::RichText, &rt.encode_to_vec())));
        let tree = pb::TreeDelta { moves: vec![], labels: None, positions: Some(marks) };
        assert!(malformed(decode_delta_payload(pb::CrdtType::Tree, &tree.encode_to_vec())));

        let cart = |a: &[(u64, u64)], b: &[(u64, u64)]| {
            let d = pb::OrMapCounterDelta { dots: two(a, b), values: Default::default(), ctx: Some(pb::DotContext::default()) };
            decode_delta_payload(pb::CrdtType::OrMapCounter, &d.encode_to_vec())
        };
        assert!(cart(&[(1, 1)], &[(1, 2)]).is_ok());
        assert!(malformed(cart(&[(1, 1)], &[(1, 1)])));

        let mv = |b: u64| {
            let value = |c: u64| pb::MvValues { values: vec![pb::MvValue { dot: Some(pdot(1, c)), value: "v".into() }] };
            let d = pb::MvMapDelta {
                entries: [("a".to_string(), value(1)), ("b".to_string(), value(b))].into_iter().collect(),
                ctx: Some(pb::DotContext::default()),
            };
            decode_delta_payload(pb::CrdtType::MvMap, &d.encode_to_vec())
        };
        assert!(mv(2).is_ok());
        assert!(malformed(mv(1)));

        let json = |b: u64| {
            let field = |key: &str, c: u64| pb::JsonFieldDots { generation: None, key: key.into(), dots: Some(pdots(&[(1, c)])) };
            let object = pb::JsonObjectDelta { dots: vec![field("a", 1), field("b", b)], values: vec![], ctx: Some(pb::DotContext::default()) };
            let d = pb::JsonValueDelta { object: Some(object), ..Default::default() };
            decode_delta_payload(pb::CrdtType::Json, &d.encode_to_vec())
        };
        assert!(json(2).is_ok());
        assert!(malformed(json(1)));

        // A list item listed twice is one dot naming two items.
        let item = |value: &str| pb::RgaElement { id: Some(pdot(1, 1)), predecessor: None, has_predecessor: false, value: value.into() };
        let d = pb::RgaDelta { elements: vec![item("x"), item("y")], tombstones: None, ctx: Some(pb::DotContext::default()) };
        assert!(malformed(decode_delta_payload(pb::CrdtType::RgaList, &d.encode_to_vec())));
    }

    // A list item says it was put at the front in two ways: `has_predecessor`
    // and whether `predecessor` is there. If they disagree, two programs
    // could read it differently, so it is refused.
    #[test]
    fn a_list_item_whose_two_front_markers_disagree_is_refused() {
        let list = |has: bool, pred: Option<pb::Dot>| {
            let el = pb::RgaElement { id: Some(pdot(1, 2)), predecessor: pred, has_predecessor: has, value: "v".into() };
            let d = pb::RgaDelta { elements: vec![el], tombstones: None, ctx: Some(pb::DotContext::default()) };
            decode_delta_payload(pb::CrdtType::RgaList, &d.encode_to_vec())
        };
        assert!(list(false, None).is_ok());
        assert!(list(true, Some(pdot(1, 1))).is_ok());
        assert!(malformed(list(false, Some(pdot(1, 1)))));
        assert!(malformed(list(true, None)));

        let json = |has: bool, pred: Option<pb::Dot>| {
            let el = pb::JsonArrayElement { id: Some(pdot(1, 2)), predecessor: pred, has_predecessor: has, value: Some(pb::JsonValueDelta::default()) };
            let array = pb::JsonArrayDelta { elements: vec![el], tombstones: None, ctx: Some(pb::DotContext::default()) };
            let d = pb::JsonValueDelta { array: Some(array), ..Default::default() };
            decode_delta_payload(pb::CrdtType::Json, &d.encode_to_vec())
        };
        assert!(json(false, None).is_ok());
        assert!(json(true, Some(pdot(1, 1))).is_ok());
        assert!(malformed(json(false, Some(pdot(1, 1)))));
        assert!(malformed(json(true, None)));
    }

    fn json_payload(tag: Option<&str>, scalar: Option<&str>) -> Vec<u8> {
        let reg = |v: &str| pb::LwwRegisterDelta {
            value: Some(v.to_string()),
            ts: Some(pb::Hlc { wall: 1, logical: 0 }),
            writer: 1,
        };
        pb::JsonValueDelta { tag: tag.map(reg), scalar: scalar.map(reg), object: None, array: None, generation: None, parent: None }
            .encode_to_vec()
    }

    #[test]
    fn json_values_this_version_does_not_know_are_errors() {
        let decode = |tag, scalar| decode_delta_payload(pb::CrdtType::Json, &json_payload(tag, scalar));
        assert!(decode(Some("S"), Some("z")).is_ok());
        assert!(decode(Some("O"), Some("b1")).is_ok());
        assert!(decode(Some("A"), Some("sabc")).is_ok());
        assert!(decode(Some("N"), None).is_ok());
        for bad in ["Q", "", "s"] {
            assert!(matches!(decode(Some(bad), None), Err(Error::MalformedPayload(_))), "tag {bad:?}");
        }
        for bad in ["", "x", "zz", "b", "b2", "d", "d12x"] {
            assert!(matches!(decode(None, Some(bad)), Err(Error::MalformedPayload(_))), "scalar {bad:?}");
        }
    }

    #[test]
    fn a_json_generation_without_a_tag_value_is_an_error() {
        let generation = pb::JsonGeneration { ts: Some(pb::Hlc { wall: 1, logical: 0 }), writer: 1, step: 0 };
        let without_value = pb::LwwRegisterDelta { value: None, ts: Some(pb::Hlc { wall: 1, logical: 0 }), writer: 1 };
        for tag in [None, Some(without_value)] {
            for (g, p) in [(Some(generation), None), (None, Some(generation))] {
                let bytes = pb::JsonValueDelta { tag: tag.clone(), generation: g, parent: p, ..Default::default() }.encode_to_vec();
                let r = decode_delta_payload(pb::CrdtType::Json, &bytes);
                assert!(matches!(r, Err(Error::MalformedPayload(_))), "{r:?}");
            }
        }
        // A generation with no timestamp is a missing field.
        let tag = pb::LwwRegisterDelta { value: Some("O".into()), ts: Some(pb::Hlc { wall: 1, logical: 0 }), writer: 1 };
        let bytes = pb::JsonValueDelta { tag: Some(tag), generation: Some(pb::JsonGeneration { ts: None, writer: 1, step: 1 }), ..Default::default() }.encode_to_vec();
        assert!(matches!(decode_delta_payload(pb::CrdtType::Json, &bytes), Err(Error::MissingField(_))));
    }

    #[test]
    fn a_json_generation_too_many_steps_on_is_an_error() {
        let tag = pb::LwwRegisterDelta { value: Some("O".into()), ts: Some(pb::Hlc { wall: 1, logical: 0 }), writer: 1 };
        let with_step = |step| {
            let g = pb::JsonGeneration { ts: Some(pb::Hlc { wall: 1, logical: 0 }), writer: 1, step };
            let bytes = pb::JsonValueDelta { tag: Some(tag.clone()), generation: Some(g), ..Default::default() }.encode_to_vec();
            decode_delta_payload(pb::CrdtType::Json, &bytes)
        };
        assert!(with_step(MAX_GENERATION_STEP).is_ok());
        for step in [MAX_GENERATION_STEP + 1, u64::MAX] {
            assert!(matches!(with_step(step), Err(Error::MalformedPayload(_))), "step {step}");
        }
        // The same for a parent's generation, and a field's.
        let g = pb::JsonGeneration { ts: Some(pb::Hlc { wall: 1, logical: 0 }), writer: 1, step: u64::MAX };
        let bytes = pb::JsonValueDelta { tag: Some(tag.clone()), parent: Some(g), ..Default::default() }.encode_to_vec();
        assert!(matches!(decode_delta_payload(pb::CrdtType::Json, &bytes), Err(Error::MalformedPayload(_))));
        let field = pb::JsonFieldValue { generation: Some(g), key: "k".into(), value: Some(pb::JsonValueDelta::default()) };
        let object = pb::JsonObjectDelta { values: vec![field], dots: vec![], ctx: Some(pb::DotContext::default()) };
        let bytes = pb::JsonValueDelta { object: Some(object), ..Default::default() }.encode_to_vec();
        assert!(matches!(decode_delta_payload(pb::CrdtType::Json, &bytes), Err(Error::MalformedPayload(_))));
    }

    // Writing the same value over and over must not make each change bigger
    // than the one before: what a write hides is not sent again.
    #[test]
    fn rewriting_a_json_value_keeps_the_change_the_same_size() {
        let cfg: JsonValue = JsonValue::from_json_str(r#"{"a":1,"b":2,"c":{"d":[1,2,{"e":3}]}}"#).unwrap();
        let path = |p: &str| orvu_crdt::parse_path(p).unwrap();
        for (at, value) in [("[\"cfg\"]", cfg.clone()), ("[\"xs\"]", JsonValue::Array(vec![cfg.clone(), JsonValue::Num(1.0)])), ("[]", cfg.clone())] {
            let mut j = JsonCrdt::default();
            let mut sizes = Vec::new();
            for i in 1..=200u64 {
                // A large clock, so that every timestamp takes the same room.
                let d = j.set(&path(at), &value, Hlc { wall: (1 << 40) + i, logical: 0 }, ReplicaId(1)).unwrap();
                sizes.push(encode_delta_payload(&TypedDelta::Json(d)).len());
            }
            // The counters of the field and item ids grow a byte longer,
            // nothing more.
            assert!(sizes[199] <= sizes[0] + 32, "{at}: write 1 was {} bytes, write 200 was {}", sizes[0], sizes[199]);
        }
    }

    fn round_trip(delta: TypedDelta) {
        let kind = delta.crdt_type();
        let bytes = encode_delta_payload(&delta);
        let decoded = decode_delta_payload(kind, &bytes).unwrap();
        assert_eq!(decoded, delta);
    }

    #[test]
    fn pn_counter_delta_round_trips() {
        let mut c = PnCounter::default();
        round_trip(TypedDelta::PnCounter(c.increment(ReplicaId(1), 9)));
        round_trip(TypedDelta::PnCounter(c.decrement(ReplicaId(2), 4)));
    }

    #[test]
    fn lww_register_delta_round_trips() {
        let mut r = LwwRegister::<String>::default();
        let d = r.set("v".to_string(), Hlc { wall: 4, logical: 2 }, ReplicaId(3));
        round_trip(TypedDelta::LwwRegister(d));
    }

    #[test]
    fn or_set_delta_round_trips() {
        let mut s = OrSet::<String>::default();
        round_trip(TypedDelta::OrSet(s.add("a".to_string(), ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::OrSet(s.add("b".to_string(), ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::OrSet(s.remove(&"a".to_string())));
    }

    #[test]
    fn or_map_delta_round_trips() {
        let mut m: OrMap<String, LwwRegister<String>> = OrMap::default();
        let _ = m.update("a".to_string(), ReplicaId(1), |r| {
            r.set("x".to_string(), Hlc::ZERO, ReplicaId(1))
        }).unwrap();
        let d = m.update("b".to_string(), ReplicaId(2), |r| {
            r.set("y".to_string(), Hlc { wall: 3, logical: 1 }, ReplicaId(2))
        }).unwrap();
        round_trip(TypedDelta::OrMap(d));
        round_trip(TypedDelta::OrMap(m.remove(&"a".to_string())));
        round_trip(TypedDelta::OrMap(m.snapshot()));
    }

    #[test]
    fn or_map_counter_delta_round_trips() {
        let mut m: OrMap<String, PnCounter> = OrMap::default();
        let _ = m.update("apple".to_string(), ReplicaId(1), |pn| pn.increment(ReplicaId(1), 3)).unwrap();
        // One key with both an increment and a decrement, and a second item.
        let _ = m.update("pear".to_string(), ReplicaId(2), |pn| pn.increment(ReplicaId(2), 4)).unwrap();
        let d = m.update("pear".to_string(), ReplicaId(2), |pn| pn.decrement(ReplicaId(2), 1)).unwrap();
        round_trip(TypedDelta::OrMapCounter(d));
        round_trip(TypedDelta::OrMapCounter(m.remove(&"apple".to_string())));
        round_trip(TypedDelta::OrMapCounter(m.snapshot()));
    }

    #[test]
    fn rga_delta_round_trips() {
        let mut r: Rga<String> = Rga::default();
        // An insert at the front, an insert after an element, a tombstone, and
        // a snapshot.
        let d0 = r.insert_after(None, "a".to_string(), ReplicaId(1)).unwrap();
        round_trip(TypedDelta::Rga(d0));
        let a = r.visible_dots()[0];
        let d1 = r.insert_after(Some(a), "b".to_string(), ReplicaId(2)).unwrap();
        round_trip(TypedDelta::Rga(d1));
        let d2 = r.tombstone(a);
        round_trip(TypedDelta::Rga(d2));
        round_trip(TypedDelta::Rga(r.snapshot()));
    }

    #[test]
    fn json_delta_round_trips() {
        let mut j = JsonCrdt::default();
        // A single value, a nested object and an array of objects, so that
        // every branch is used.
        round_trip(TypedDelta::Json(j.set(&[Seg::Key("n".into())], &JsonValue::Num(1.5), Hlc { wall: 1, logical: 0 }, ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::Json(j.set(&[Seg::Key("o".into()), Seg::Key("p".into())], &JsonValue::Str("v".into()), Hlc { wall: 2, logical: 0 }, ReplicaId(1)).unwrap()));
        let arr = JsonValue::Array(vec![JsonValue::Bool(true), JsonValue::Null]);
        round_trip(TypedDelta::Json(j.set(&[Seg::Key("a".into())], &arr, Hlc { wall: 3, logical: 0 }, ReplicaId(1)).unwrap()));
        // A removal, and a write into the removed field, which starts it
        // afresh in a generation one step on.
        round_trip(TypedDelta::Json(j.remove(&[Seg::Key("o".into())], Hlc { wall: 4, logical: 0 }, ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::Json(j.set(&[Seg::Key("o".into()), Seg::Key("q".into())], &JsonValue::Null, Hlc { wall: 5, logical: 0 }, ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::Json(j.snapshot()));
        let TypedDelta::Json(whole) = decode_delta_payload(pb::CrdtType::Json, &encode_delta_payload(&TypedDelta::Json(j.snapshot()))).unwrap() else {
            unreachable!()
        };
        let mut copy = JsonCrdt::default();
        copy.merge(whole);
        assert_eq!(copy, j);
        assert_eq!(copy.to_value().to_json_string(), r#"{"a":[true,null],"n":1.5,"o":{"q":null}}"#);
    }

    // A field named twice in one change would leave it unclear which one
    // counts, so the change is refused.
    #[test]
    fn a_json_field_named_twice_is_an_error() {
        let field = || pb::JsonFieldValue { generation: None, key: "k".into(), value: Some(pb::JsonValueDelta::default()) };
        let dots = || pb::JsonFieldDots { generation: None, key: "k".into(), dots: None };
        let ctx = Some(pb::DotContext::default());
        let cases = [
            pb::JsonObjectDelta { values: vec![field(), field()], dots: vec![], ctx: ctx.clone() },
            pb::JsonObjectDelta { values: vec![], dots: vec![dots(), dots()], ctx: ctx.clone() },
        ];
        for object in cases {
            let bytes = pb::JsonValueDelta { object: Some(object), ..Default::default() }.encode_to_vec();
            let r = decode_delta_payload(pb::CrdtType::Json, &bytes);
            assert!(matches!(r, Err(Error::MalformedPayload(_))), "{r:?}");
        }
        // The same key in two generations is two fields.
        let other = pb::JsonFieldValue {
            generation: Some(pb::JsonGeneration { ts: Some(pb::Hlc { wall: 1, logical: 0 }), writer: 1, step: 0 }),
            ..field()
        };
        let object = pb::JsonObjectDelta { values: vec![field(), other], dots: vec![], ctx };
        let bytes = pb::JsonValueDelta { object: Some(object), ..Default::default() }.encode_to_vec();
        assert!(decode_delta_payload(pb::CrdtType::Json, &bytes).is_ok());
    }

    #[test]
    fn rich_text_delta_round_trips() {
        let mut rt = RichText::default();
        // A delta with only text, one with only marks, and a snapshot.
        round_trip(TypedDelta::RichText(rt.insert_char(0, 'a', ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::RichText(rt.insert_char(1, 'b', ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::RichText(rt.format(0, 2, "bold", "true", Hlc { wall: 1, logical: 0 }, ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::RichText(rt.delete(1, 1, ReplicaId(1))));
        round_trip(TypedDelta::RichText(rt.snapshot()));
    }

    #[test]
    fn bounded_counter_delta_round_trips() {
        let mut b = BoundedCounter::default();
        round_trip(TypedDelta::BoundedCounter(b.increment(10, ReplicaId(1))));
        round_trip(TypedDelta::BoundedCounter(b.transfer(ReplicaId(2), 4, ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::BoundedCounter(b.try_decrement(3, ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::BoundedCounter(b.snapshot()));
    }

    #[test]
    fn mv_map_delta_round_trips() {
        let (mut a, mut b) = (MvMap::<String, String>::default(), MvMap::<String, String>::default());
        let from_a = a.put("k".into(), "x".into(), ReplicaId(1)).unwrap();
        round_trip(TypedDelta::MvMap(from_a.clone()));
        b.put("k".into(), "y".into(), ReplicaId(2)).unwrap();
        b.merge(from_a);
        round_trip(TypedDelta::MvMap(b.snapshot()));
        round_trip(TypedDelta::MvMap(b.remove(&"k".to_string())));
    }

    // A dot listed twice under one key is something only a broken or hostile
    // sender writes. It has to decode to the value a merge would keep.
    #[test]
    fn a_dot_listed_twice_keeps_the_smaller_value() {
        let dot = Some(encode_dot(Dot::new(ReplicaId(1), 1)));
        let bytes = pb::MvMapDelta {
            entries: BTreeMap::from([("k".to_string(), pb::MvValues { values: vec![
                pb::MvValue { dot, value: "y".into() },
                pb::MvValue { dot, value: "x".into() },
            ] })]),
            ctx: Some(encode_context(&DotContext::new())),
        }.encode_to_vec();
        let TypedDelta::MvMap(d) = decode_delta_payload(pb::CrdtType::MvMap, &bytes).unwrap() else { panic!() };
        let vals: Vec<&String> = d.entries().flat_map(|(_, v)| v.values()).collect();
        assert_eq!(vals, vec!["x"]);
    }

    #[test]
    fn an_mv_map_value_without_its_dot_is_refused() {
        let bytes = pb::MvMapDelta {
            entries: BTreeMap::from([("k".to_string(), pb::MvValues { values: vec![
                pb::MvValue { dot: None, value: "x".into() },
            ] })]),
            ctx: Some(encode_context(&DotContext::new())),
        }.encode_to_vec();
        assert!(matches!(decode_delta_payload(pb::CrdtType::MvMap, &bytes), Err(Error::MissingField("MvValue.dot"))));
    }

    #[test]
    fn tree_delta_round_trips() {
        let mut t = Tree::default();
        round_trip(TypedDelta::Tree(t.move_node("a".into(), "root".into(), Hlc { wall: 1, logical: 0 }, ReplicaId(1))));
        round_trip(TypedDelta::Tree(t.move_node("b".into(), "a".into(), Hlc { wall: 2, logical: 0 }, ReplicaId(2))));
        round_trip(TypedDelta::Tree(t.set_label("a".into(), "Alpha".into(), Hlc { wall: 3, logical: 0 }, ReplicaId(1)).unwrap()));
        // A delta that carries a position, and a snapshot that carries all
        // three parts.
        round_trip(TypedDelta::Tree(t.move_node_at("c".into(), "root".into(), 0, Hlc { wall: 4, logical: 0 }, ReplicaId(1)).unwrap()));
        round_trip(TypedDelta::Tree(t.snapshot()));
    }

    #[test]
    fn tree_sibling_order_survives_the_codec() {
        let mut src = Tree::default();
        let deltas = vec![
            src.move_node_at("a".into(), "root".into(), 0, Hlc { wall: 1, logical: 0 }, ReplicaId(1)).unwrap(),
            src.move_node_at("b".into(), "root".into(), 0, Hlc { wall: 2, logical: 0 }, ReplicaId(1)).unwrap(),
        ];
        assert_eq!(src.children("root"), vec!["b".to_string(), "a".to_string()]);

        let mut dst = Tree::default();
        for d in deltas {
            let bytes = encode_delta_payload(&TypedDelta::Tree(d));
            match decode_delta_payload(pb::CrdtType::Tree, &bytes).expect("decodes") {
                TypedDelta::Tree(d) => dst.merge(d),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(dst.children("root"), vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn a_tree_label_survives_the_codec() {
        let mut src = Tree::default();
        src.move_node("a".into(), "root".into(), Hlc { wall: 1, logical: 0 }, ReplicaId(1));
        let d = src.set_label("a".into(), "Alpha".into(), Hlc { wall: 2, logical: 0 }, ReplicaId(1)).unwrap();
        let bytes = encode_delta_payload(&TypedDelta::Tree(d));
        let back = decode_delta_payload(pb::CrdtType::Tree, &bytes).expect("decodes");
        let mut dst = Tree::default();
        match back {
            TypedDelta::Tree(d) => dst.merge(d),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(dst.label("a").as_deref(), Some("Alpha"));
    }

    #[test]
    fn context_round_trips_including_cloud() {
        let mut ctx = DotContext::new();
        ctx.insert(Dot::new(ReplicaId(1), 1));
        ctx.insert(Dot::new(ReplicaId(1), 2));
        ctx.insert(Dot::new(ReplicaId(2), 7)); // gap -> cloud
        assert_eq!(decode_context(&encode_context(&ctx)).unwrap(), ctx);
    }

    #[test]
    fn decode_rejects_a_malformed_payload() {
        // Another machine chooses these bytes, so nonsense must come back as
        // an error and never crash the program. Protobuf bytes do not say what
        // type they are, so a valid encoding of the wrong type can still
        // decode. That gives a wrong value, which is harmless. This test only
        // checks that cut-off or nonsense input is refused without a crash.
        let err = decode_delta_payload(pb::CrdtType::LwwRegister, &[0xff, 0xff, 0xff]);
        assert!(matches!(err, Err(Error::MalformedPayload(_))), "got {err:?}");
    }

    #[test]
    fn decode_rejects_the_unspecified_type() {
        assert_eq!(
            decode_delta_payload(pb::CrdtType::Unspecified, &[]).unwrap_err(),
            Error::UnknownCrdtType(0)
        );
    }
}
