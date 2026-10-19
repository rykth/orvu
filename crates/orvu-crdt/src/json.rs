//! A JSON document that several machines can edit separately and merge.
//!
//! A document is a tree of nodes. A node can be an object, an array, or a
//! single value such as a number or a string (a "scalar"). Each node stores
//! all three at once, every one in its own mergeable type, plus a tag that
//! says which of the three the node shows right now. Reads follow the tag.
//!
//! Merging a node merges its four parts one by one. Each part ends up the same
//! on every machine, so the whole node does too.
//!
//! The tag and the scalar are last-writer-wins (LWW): when two machines write
//! at the same time, the write with the later timestamp wins everywhere. So if
//! one machine sets a node to an object while another sets it to a number,
//! the later write decides what the node shows. The other write's data is
//! still stored but not shown. Object keys are held in an `OrMap` and array
//! elements in an `Rga` list.
//!
//! # The rule for writes at the same time
//!
//! Replacing or removing an object or list starts it afresh. A change made at
//! the same time inside what was replaced is dropped. What was replaced never
//! comes back. Two machines that write into a field that was empty, removed
//! or replaced, starting from the same state, both keep their writes.
//!
//! # Generations
//!
//! Old contents are never thrown away, because a machine that never saw them
//! could not agree on what was thrown away. So they have to be kept out of
//! sight instead. For that, the tag also names the node's *generation*. Each
//! object field is filed under the generation of the object it was written
//! into, and each array item records the generation of the array it was
//! written into. A node shows only the children of its current generation.
//!
//! A whole write of a value starts a new generation, named after the write's
//! own timestamp and machine id. So does a removal of a field: it writes the
//! field's tag as `Absent`, which reads as not there. So when two machines
//! both write a whole list (or object) to the same place, the tag picks one
//! of the two generations, and only that list's items show. They are never
//! joined.
//!
//! A write down a path into a node (a `set`, `insert` or `remove` of
//! something inside it) leaves the node's tag alone when the node is shown
//! and already has the shape the write needs. It writes into the current
//! generation. So if another machine replaces or removes the node at the same
//! time, that replacement wins whatever its timestamp, and what this write
//! put inside is dropped with the old contents.
//!
//! When the node is not shown, or does not have the needed shape (writing a
//! field into what is now a number, or into a field that was removed or never
//! written), the write starts the node afresh. The new generation is named
//! after the generation it replaces: the same timestamp and machine id, and a
//! `step` one higher. Two machines that start the same node afresh from the
//! same generation therefore pick the same new generation, and both of their
//! writes show.
//!
//! The tag of such a write is stamped with the timestamp and machine id of
//! the tag it replaces, not with the write's own. The two tags then tie on
//! the stamp, and the new one wins because its generation is later, and a tag
//! compares its generation first. Any whole write or removal of the node made
//! after the replaced tag has a later timestamp, so it beats the fresh start
//! on every machine, whenever the fresh start was made. Every input to that
//! choice comes from the replaced tag, so all machines make it the same way.
//!
//! This file also has a small JSON parser and writer for `JsonValue`, the
//! plain value used to pass data in and out.

use crate::clock::Hlc;
use crate::context::DotContext;
use crate::error::WriteError;
use crate::id::{Dot, ReplicaId};
use crate::{DeltaCrdt, LwwDelta, LwwRegister, OrMap, OrMapDelta, Rga, RgaDelta};
use std::collections::{BTreeMap, BTreeSet};

/// Which of its forms a JSON node shows. The choice is last-writer-wins, and
/// a new node starts as `Scalar`. `Absent` is what a removal writes: the node
/// is not there, and a read does not find it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Shape {
    #[default]
    Scalar,
    Object,
    Array,
    Absent,
}

/// The name of one generation of a node: the contents it had between two
/// replacements of its whole value. See "Generations" at the top of this
/// file.
///
/// A whole write or a removal names the generation after its own timestamp
/// and machine id, with `step` 0. A write down a path that has to start the
/// node afresh names the new generation after the one it replaces: the same
/// timestamp and machine id, and `step` one higher.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Generation {
    pub ts: Hlc,
    pub writer: ReplicaId,
    pub step: u64,
}

impl Default for Generation {
    fn default() -> Self {
        Generation { ts: Hlc::ZERO, writer: ReplicaId(0), step: 0 }
    }
}

/// The most steps a generation may be on from the one it was named after.
/// No real document gets near it: each step is a fresh start of one node
/// from the same generation. A message carrying more is refused, and a write
/// that would go further is refused too, so that nothing written here is
/// ever refused by another machine.
pub const MAX_GENERATION_STEP: u64 = 1 << 32;

impl Generation {
    /// The generation a write down a path starts when it replaces this one,
    /// or `None` when `step` would go past `MAX_GENERATION_STEP`.
    fn next(self) -> Option<Generation> {
        let step = self.step.checked_add(1).filter(|&step| step <= MAX_GENERATION_STEP)?;
        Some(Generation { step, ..self })
    }
}

/// What a node's last-writer-wins tag holds: its current generation, the
/// shape it shows, and the generation of its parent it was written into.
///
/// The generation comes first, so that of two tags written with the same
/// timestamp and machine id, the one with the later generation wins. A write
/// that starts a node afresh relies on that.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct JsonTag {
    pub generation: Generation,
    pub shape: Shape,
    pub parent: Generation,
}

/// What an object's fields are filed under: the generation of the object
/// they were written into, and the key. A field written into one generation
/// of an object is a different field from the one with the same key in
/// another generation, so a replaced object's fields stay out of sight.
pub type FieldKey = (Generation, String);

/// A JSON number (`f64`) that can always be compared with another one. A
/// last-writer-wins value needs `Ord + Eq`, which a plain `f64` does not have
/// because of NaN. This type compares with `f64::total_cmp`, and two values
/// are equal exactly when `total_cmp` says they are.
#[derive(Clone, Copy, Debug, Default)]
pub struct OrderedF64(pub f64);

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool { self.0.total_cmp(&other.0) == std::cmp::Ordering::Equal }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.0.total_cmp(&other.0) }
}

/// A single JSON value that is not an object or an array. The default is `Null`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum JsonScalar {
    #[default]
    Null,
    Bool(bool),
    Num(OrderedF64),
    Str(String),
}

/// A plain JSON value for passing data in and out. It does not merge. A write
/// turns it into `JsonCrdt` nodes, and a read turns nodes back into it. It
/// needs no outside library.
#[derive(Clone, PartialEq, Debug)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

/// The error you get when a string cannot be parsed as JSON, or as a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonParseError(pub String);

impl std::fmt::Display for JsonParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json parse error: {}", self.0)
    }
}

impl JsonValue {
    /// Writes the value as a JSON string. Object keys come out in sorted order
    /// (the order of the `BTreeMap`), so the same value always gives the same
    /// string. NaN and the two infinities are written as `null`, because JSON
    /// has no way to write them. A whole number is written in plain digits,
    /// exactly, below 1e21, and with an exponent from there on, such as
    /// `1.5e300`. Either way it reads back as the same float. Negative zero
    /// is written as `-0.0`.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        use std::fmt::Write;
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            JsonValue::Num(n) => {
                if *n == 0.0 && n.is_sign_negative() {
                    // `-0` would read back as the whole number 0 in a reader
                    // that keeps whole numbers apart from floats.
                    out.push_str("-0.0");
                } else if n.is_finite() && n.fract() == 0.0 && n.abs() > MAX_WHOLE as f64 && n.abs() < 1e21 {
                    // A whole number past 2^53. The usual way of writing it
                    // rounds its last digits, which the parser refuses. Its
                    // exact digits read back as the same float, and a reader
                    // that keeps whole numbers apart from floats reads it as
                    // a whole number, as it was most likely written.
                    if *n < 0.0 { out.push('-'); }
                    out.push_str(&exact_digits(n.abs()));
                } else if n.is_finite() && n.fract() == 0.0 && n.abs() > MAX_WHOLE as f64 {
                    // From 1e21 on the exponent is much shorter, and it
                    // reads back as the same float.
                    let _ = write!(out, "{n:e}");
                } else if n.is_finite() {
                    let _ = write!(out, "{n}");
                } else {
                    out.push_str("null");
                }
            }
            JsonValue::Str(s) => write_json_string(s, out),
            JsonValue::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 { out.push(','); }
                    item.write_json(out);
                }
                out.push(']');
            }
            JsonValue::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 { out.push(','); }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }

    /// Parses a JSON string into a `JsonValue`. Anything but whitespace after
    /// the value is an error. Bad input returns an error and never panics.
    pub fn from_json_str(s: &str) -> Result<JsonValue, JsonParseError> {
        let mut p = JsonParser { c: s.chars().collect(), i: 0 };
        let v = p.value(0)?;
        p.ws();
        if p.i != p.c.len() {
            return Err(p.err("trailing characters"));
        }
        Ok(v)
    }
}

fn write_json_string(s: &str, out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => { let _ = write!(out, "\\u{:04x}", c as u32); }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// How deeply arrays and objects may be nested in what the parser reads. Deeper
/// input returns `Err`. Without the limit, deep nesting could overflow the
/// stack, and that crashes the program in a way no caller can catch. It
/// matters because the strings can come from untrusted callers, such as the
/// host of a WebAssembly build.
const MAX_JSON_DEPTH: usize = 128;

/// 2^53. A 64-bit float holds every whole number from -2^53 to 2^53 exactly,
/// but not every one past that.
const MAX_WHOLE: u64 = 1 << 53;

/// True if a 64-bit float holds the whole number written as `digits` (plain
/// decimal digits, no sign, no leading zeros) exactly.
fn held_exactly(digits: &str) -> bool {
    // Fewer than 16 digits is below 10^15, and so below 2^53.
    if digits.len() < 16 {
        return true;
    }
    // The largest float is below 10^309.
    if digits.len() > 309 {
        return false;
    }
    match digits.parse::<f64>() {
        Ok(f) if f.is_finite() => exact_digits(f) == digits,
        _ => false,
    }
}

/// The exact decimal digits of `f`, a whole number of at least 2^53.
fn exact_digits(f: f64) -> String {
    // A float is a whole number times a power of two. Past 2^53 that power
    // is not negative.
    let bits = f.to_bits();
    let mantissa = (bits & ((1 << 52) - 1)) | (1 << 52);
    let exponent = ((bits >> 52) & 0x7ff) as i64 - 1075;
    let (mut n, doublings) = if exponent < 0 { (mantissa >> -exponent, 0) } else { (mantissa, exponent) };
    // The digits, lowest first.
    let mut out = Vec::new();
    while n > 0 {
        out.push((n % 10) as u8);
        n /= 10;
    }
    for _ in 0..doublings {
        let mut carry = 0;
        for d in out.iter_mut() {
            let v = *d * 2 + carry;
            *d = v % 10;
            carry = v / 10;
        }
        if carry > 0 {
            out.push(carry);
        }
    }
    out.iter().rev().map(|d| char::from(b'0' + d)).collect()
}

struct JsonParser {
    c: Vec<char>,
    i: usize,
}

impl JsonParser {
    fn peek(&self) -> Option<char> { self.c.get(self.i).copied() }
    fn bump(&mut self) -> Option<char> {
        let ch = self.c.get(self.i).copied();
        if ch.is_some() { self.i += 1; }
        ch
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) { self.i += 1; }
    }
    fn err(&self, m: &str) -> JsonParseError {
        JsonParseError(format!("{m} at position {}", self.i))
    }

    fn value(&mut self, depth: usize) -> Result<JsonValue, JsonParseError> {
        self.ws();
        match self.peek() {
            Some('{') => self.object(depth),
            Some('[') => self.array(depth),
            Some('"') => Ok(JsonValue::Str(self.string()?)),
            Some('t') => self.lit("true", JsonValue::Bool(true)),
            Some('f') => self.lit("false", JsonValue::Bool(false)),
            Some('n') => self.lit("null", JsonValue::Null),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            _ => Err(self.err("unexpected character")),
        }
    }

    fn lit(&mut self, word: &str, val: JsonValue) -> Result<JsonValue, JsonParseError> {
        for expected in word.chars() {
            if self.bump() != Some(expected) { return Err(self.err("invalid literal")); }
        }
        Ok(val)
    }

    fn string(&mut self) -> Result<String, JsonParseError> {
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{08}'),
                    Some('f') => out.push('\u{0C}'),
                    Some('u') => {
                        let cp = self.hex4()?;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            if self.bump() != Some('\\') || self.bump() != Some('u') {
                                return Err(self.err("expected low surrogate"));
                            }
                            let lo = self.hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&lo) {
                                return Err(self.err("invalid low surrogate"));
                            }
                            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(c).ok_or_else(|| self.err("invalid codepoint"))?);
                        } else {
                            out.push(char::from_u32(cp).ok_or_else(|| self.err("invalid codepoint"))?);
                        }
                    }
                    _ => return Err(self.err("invalid escape")),
                },
                Some(c) => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonParseError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.bump().ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = c.to_digit(16).ok_or_else(|| self.err("invalid hex digit"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn number(&mut self) -> Result<JsonValue, JsonParseError> {
        let start = self.i;
        if self.peek() == Some('-') { self.i += 1; }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) { self.i += 1; }
        if self.peek() == Some('.') {
            self.i += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) { self.i += 1; }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.i += 1;
            if matches!(self.peek(), Some('+' | '-')) { self.i += 1; }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) { self.i += 1; }
        }
        let text: String = self.c[start..self.i].iter().collect();
        // A number with no fraction and no exponent is a whole number. A
        // 64-bit float holds every whole number up to 2^53 exactly, and past
        // that only some of them. It would round the others without a word.
        // Refuse those, so what is read back is always what was written.
        if !text.contains(['.', 'e', 'E']) {
            let digits = text.trim_start_matches('-').trim_start_matches('0');
            if !held_exactly(digits) {
                return Err(self.err("a 64-bit float cannot hold this whole number past 2^53 exactly"));
            }
        }
        text.parse::<f64>().map(JsonValue::Num).map_err(|_| self.err("invalid number"))
    }

    fn array(&mut self, depth: usize) -> Result<JsonValue, JsonParseError> {
        if depth >= MAX_JSON_DEPTH { return Err(self.err("nesting too deep")); }
        self.bump(); // [
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(']') { self.bump(); return Ok(JsonValue::Array(items)); }
        loop {
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.bump() {
                Some(',') => {}
                Some(']') => return Ok(JsonValue::Array(items)),
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<JsonValue, JsonParseError> {
        if depth >= MAX_JSON_DEPTH { return Err(self.err("nesting too deep")); }
        self.bump(); // {
        let mut map = BTreeMap::new();
        self.ws();
        if self.peek() == Some('}') { self.bump(); return Ok(JsonValue::Object(map)); }
        loop {
            self.ws();
            if self.peek() != Some('"') { return Err(self.err("expected string key")); }
            let k = self.string()?;
            self.ws();
            if self.bump() != Some(':') { return Err(self.err("expected ':'")); }
            let v = self.value(depth + 1)?;
            map.insert(k, v);
            self.ws();
            match self.bump() {
                Some(',') => {}
                Some('}') => return Ok(JsonValue::Object(map)),
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }
}

/// Parses a JSON array such as `["users",0,"name"]` into a path. Strings become
/// `Seg::Key`, and whole numbers of zero or more become `Seg::Index`. Anything
/// else is an error.
pub fn parse_path(s: &str) -> Result<Vec<Seg>, JsonParseError> {
    match JsonValue::from_json_str(s)? {
        JsonValue::Array(items) => items
            .into_iter()
            .map(|item| match item {
                JsonValue::Str(k) => Ok(Seg::Key(k)),
                JsonValue::Num(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 => {
                    Ok(Seg::Index(n as usize))
                }
                _ => Err(JsonParseError("path element must be a string or non-negative integer".into())),
            })
            .collect(),
        _ => Err(JsonParseError("path must be a JSON array".into())),
    }
}

/// One node of a JSON document. It holds an object part, an array part and a
/// scalar part, plus a last-writer-wins tag that picks which one it shows and
/// which of its children are current.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct JsonCrdt {
    tag: LwwRegister<JsonTag>,
    object: OrMap<FieldKey, JsonCrdt>,
    array: Rga<JsonCrdt>,
    scalar: LwwRegister<JsonScalar>,
}

/// A change to a node, to send to other machines. It has one optional piece for
/// each of the node's four parts, and only the parts that were touched are
/// filled in.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct JsonDelta {
    tag: Option<LwwDelta<JsonTag>>,
    object: Option<OrMapDelta<FieldKey, JsonCrdt>>,
    array: Option<RgaDelta<JsonCrdt>>,
    scalar: Option<LwwDelta<JsonScalar>>,
}

impl JsonDelta {
    /// Builds a delta from its four parts.
    pub fn from_parts(
        tag: Option<LwwDelta<JsonTag>>,
        object: Option<OrMapDelta<FieldKey, JsonCrdt>>,
        array: Option<RgaDelta<JsonCrdt>>,
        scalar: Option<LwwDelta<JsonScalar>>,
    ) -> Self {
        Self { tag, object, array, scalar }
    }
    /// The change to the tag, if there is one.
    pub fn tag(&self) -> Option<&LwwDelta<JsonTag>> { self.tag.as_ref() }
    /// The change to the object part, if there is one.
    pub fn object(&self) -> Option<&OrMapDelta<FieldKey, JsonCrdt>> { self.object.as_ref() }
    /// The change to the array part, if there is one.
    pub fn array(&self) -> Option<&RgaDelta<JsonCrdt>> { self.array.as_ref() }
    /// The change to the scalar part, if there is one.
    pub fn scalar(&self) -> Option<&LwwDelta<JsonScalar>> { self.scalar.as_ref() }

    /// One delta that does what this one and `other` do together, in either
    /// order: their join, worked out by merging both into an empty document.
    /// Only the parts either of them touches are filled in.
    pub fn join(self, other: JsonDelta) -> JsonDelta {
        let touched = (
            self.tag.is_some() || other.tag.is_some(),
            self.object.is_some() || other.object.is_some(),
            self.array.is_some() || other.array.is_some(),
            self.scalar.is_some() || other.scalar.is_some(),
        );
        let mut both = JsonCrdt::default();
        both.merge(self);
        both.merge(other);
        let whole = both.snapshot();
        JsonDelta {
            tag: whole.tag.filter(|_| touched.0),
            object: whole.object.filter(|_| touched.1),
            array: whole.array.filter(|_| touched.2),
            scalar: whole.scalar.filter(|_| touched.3),
        }
    }
}

fn scalar_of(v: &JsonValue) -> JsonScalar {
    match v {
        JsonValue::Null => JsonScalar::Null,
        JsonValue::Bool(b) => JsonScalar::Bool(*b),
        JsonValue::Num(n) => JsonScalar::Num(OrderedF64(*n)),
        JsonValue::Str(s) => JsonScalar::Str(s.clone()),
        _ => JsonScalar::Null, // callers pass only scalars here
    }
}

impl JsonCrdt {
    /// What the tag holds now. A node that was never written holds the
    /// default: a scalar of generation zero.
    fn tag_now(&self) -> JsonTag {
        self.tag.get().copied().unwrap_or_default()
    }

    /// The field `k` of the generation `generation`, unless it was removed.
    fn field(&self, generation: Generation, k: &str) -> Option<&JsonCrdt> {
        self.object
            .get(&(generation, k.to_string()))
            .filter(|c| c.tag_now().shape != Shape::Absent)
    }

    /// The field `k`, if this node is an object and the field belongs to its
    /// current generation.
    fn child(&self, k: &str) -> Option<&JsonCrdt> {
        let now = self.tag_now();
        if now.shape != Shape::Object {
            return None;
        }
        self.field(now.generation, k)
    }

    /// The array items a reader sees, with their dots: none unless this node is
    /// an array, and then only the items of its current generation.
    fn items(&self) -> Vec<(Dot, &JsonCrdt)> {
        let now = self.tag_now();
        if now.shape != Shape::Array {
            return Vec::new();
        }
        self.array.visible().into_iter().filter(|(_, c)| c.tag_now().parent == now.generation).collect()
    }

    /// True if a write down a path that needs this node to have `shape` can
    /// go on in the node's current generation: the node is shown where it is
    /// (`shown`) and already has that shape. Otherwise the write has to start
    /// the node afresh.
    fn kept(&self, shape: Shape, shown: bool) -> bool {
        shown && self.tag_now().shape == shape
    }

    /// Checks that a write down a path that needs this node to have `shape`
    /// can go on: either the node is kept, or there is a generation to start
    /// it afresh in.
    fn check_start(&self, shape: Shape, shown: bool) -> Result<(), JsonError> {
        if !self.kept(shape, shown) && self.tag_now().generation.next().is_none() {
            return Err(JsonError::NoFreshStart);
        }
        Ok(())
    }

    /// Gets this node ready for a write down a path that needs it to have
    /// `shape`, and returns the generation to write its children into, with
    /// the change to the tag if there is one.
    ///
    /// When the node is kept (see `kept`), nothing changes: the children go
    /// into the current generation, and the tag is not written. So a whole
    /// write or a removal of this node made at the same time wins, and what
    /// this write puts inside is dropped with the old contents.
    ///
    /// Otherwise the node starts afresh, in the generation that follows the
    /// current one (`Generation::next`). It is named only after the
    /// generation it replaces, so every machine that starts this node afresh
    /// from the same generation picks the same one, and all their writes
    /// show. The tag is written with the same timestamp and machine id as the
    /// tag it replaces. That beats the old tag, because the new generation is
    /// later and is compared first, but it loses to any whole write or
    /// removal of this node made after the old tag, which has a later
    /// timestamp. It loses the same way on every machine, because it does
    /// not depend on when or where the write was made.
    fn start(&mut self, shape: Shape, shown: bool, parent: Generation) -> (Generation, Option<LwwDelta<JsonTag>>) {
        let now = self.tag_now();
        if self.kept(shape, shown) {
            return (now.generation, None);
        }
        // `check_start` refuses the write before this point when there is no
        // next generation. Staying in the current one is only a fallback.
        let generation = now.generation.next().unwrap_or(now.generation);
        let last = self.tag.snapshot();
        let td = self.tag.set(JsonTag { generation, shape, parent }, last.ts(), last.writer());
        (generation, Some(td))
    }

    /// Returns the node's current value as a plain `JsonValue`, reading
    /// whichever part the shape tag picks. A removed node reads as `null`,
    /// though a read through its parent does not find it at all.
    pub fn to_value(&self) -> JsonValue {
        match self.tag_now().shape {
            Shape::Scalar => match self.scalar.get().cloned().unwrap_or_default() {
                JsonScalar::Null => JsonValue::Null,
                JsonScalar::Bool(b) => JsonValue::Bool(b),
                JsonScalar::Num(n) => JsonValue::Num(n.0),
                JsonScalar::Str(s) => JsonValue::Str(s),
            },
            Shape::Object => {
                let generation = self.tag_now().generation;
                JsonValue::Object(
                    self.object
                        .keys()
                        .filter(|(g, _)| *g == generation)
                        .filter_map(|(_, k)| Some((k.clone(), self.field(generation, k)?.to_value())))
                        .collect(),
                )
            }
            Shape::Array => JsonValue::Array(self.items().into_iter().map(|(_, c)| c.to_value()).collect()),
            Shape::Absent => JsonValue::Null,
        }
    }

    /// Builds a new node that holds `value`. Every tag and scalar in it is
    /// written with the timestamp `hlc` and the machine id `replica`.
    pub fn from_value(value: &JsonValue, hlc: Hlc, replica: ReplicaId) -> Self {
        // A new node has seen no dots, so it never runs out of them and this
        // cannot fail.
        Self::made(value, hlc, replica, Generation::default()).unwrap_or_default()
    }

    /// Like `from_value`, for a node that goes into the parent generation
    /// `parent`.
    fn made(value: &JsonValue, hlc: Hlc, replica: ReplicaId, parent: Generation) -> Result<Self, JsonError> {
        let mut node = JsonCrdt::default();
        node.assign(value, hlc, replica, parent)?;
        Ok(node)
    }

    /// Makes this node hold `value`, written into the parent generation
    /// `parent`. This starts a new generation named after the write, so
    /// nothing the node held before shows any more, and the new contents go
    /// into that generation.
    ///
    /// Returns the delta that was applied. It carries only what this write
    /// added: the new tag and the new generation's fields or items. The
    /// generations it hides stay in the node, but are not sent again.
    ///
    /// It needs one fresh dot here for each field or item of `value` (see
    /// `check_room`). The writes check that first, so this never fails part
    /// way through for them.
    pub(crate) fn assign(&mut self, value: &JsonValue, hlc: Hlc, replica: ReplicaId, parent: Generation) -> Result<JsonDelta, JsonError> {
        let generation = Generation { ts: hlc, writer: replica, step: 0 };
        let tag = |shape| JsonTag { generation, shape, parent };
        match value {
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Num(_) | JsonValue::Str(_) => {
                let sd = self.scalar.set(scalar_of(value), hlc, replica);
                let td = self.tag.set(tag(Shape::Scalar), hlc, replica);
                Ok(JsonDelta { tag: Some(td), scalar: Some(sd), object: None, array: None })
            }
            JsonValue::Object(map) => {
                let td = self.tag.set(tag(Shape::Object), hlc, replica);
                let (mut dots, mut values, mut ctx) = (BTreeMap::new(), BTreeMap::new(), DotContext::new());
                for (k, v) in map {
                    // A new field holds a new node, which cannot fail.
                    let od = self.object.update((generation, k.clone()), replica, |child| {
                        child.assign(v, hlc, replica, generation).unwrap_or_default()
                    })?;
                    dots.extend(od.dots().map(|(k, d)| (k.clone(), d.clone())));
                    values.extend(od.values().map(|(k, d)| (k.clone(), d.clone())));
                    ctx.merge(od.context());
                }
                let od = OrMapDelta::from_parts(dots, values, ctx);
                Ok(JsonDelta { tag: Some(td), object: Some(od), array: None, scalar: None })
            }
            JsonValue::Array(items) => {
                let td = self.tag.set(tag(Shape::Array), hlc, replica);
                let mut pred = None;
                let mut elements = BTreeMap::new();
                for item in items {
                    let child = JsonCrdt::made(item, hlc, replica, generation)?;
                    let ed = self.array.insert_after(pred, child, replica)?;
                    pred = ed.elements().next().map(|(dot, _)| *dot);
                    elements.extend(ed.elements().map(|(dot, el)| (*dot, el.clone())));
                }
                let ad = RgaDelta::from_parts(elements, BTreeSet::new(), DotContext::new());
                Ok(JsonDelta { tag: Some(td), array: Some(ad), object: None, scalar: None })
            }
        }
    }

    /// Checks that this node has the fresh dots a write here needs: `fields`
    /// for its object, and `items` for its array. A node can only run out if
    /// a broken or hostile sender gave it a dot with this machine's id and a
    /// counter at `MAX_COUNTER`.
    fn check_room(&self, replica: ReplicaId, fields: usize, items: usize) -> Result<(), JsonError> {
        if self.object.room(replica) < fields as u64 || self.array.room(replica) < items as u64 {
            return Err(JsonError::Write(WriteError::NoFreshDot));
        }
        Ok(())
    }

    /// The fresh dots assigning `value` to this node takes: one per field for
    /// an object, one per item for an array.
    fn room_for(&self, value: &JsonValue, replica: ReplicaId) -> Result<(), JsonError> {
        match value {
            JsonValue::Object(map) => self.check_room(replica, map.len(), 0),
            JsonValue::Array(items) => self.check_room(replica, 0, items.len()),
            _ => Ok(()),
        }
    }
}

/// How many levels of arrays and objects a document may nest. A write that
/// would go deeper is refused with `JsonError::TooDeep`. The limit is here
/// because a document nested deeper than this cannot be sent to another
/// machine: the message format stops decoding at a fixed depth.
pub const MAX_NESTING: usize = 32;

/// One step of a path into a document: an object key or an array index.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// Why a `set`, `insert` or `remove` at a path failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum JsonError {
    /// The path goes through an array index to change something inside that
    /// element. This is refused. Merging an array keeps the first copy of an
    /// element it gets and ignores later copies, so the change would never
    /// reach a machine that already has the element, and the machines would
    /// end up different. Replace the whole element instead.
    MutateThroughIndex,
    /// An array index was too large for the operation. `len` is the number of
    /// visible elements.
    IndexOutOfBounds { index: usize, len: usize },
    /// `remove` was called with an empty path, which means the root node.
    EmptyPath,
    /// The write would nest arrays and objects more than `max` levels deep,
    /// counting the levels in the path. `depth` is how deep it would go.
    TooDeep { depth: usize, max: usize },
    /// `insert` was given a path where something other than a list is: an
    /// object, a text, a number or a true/false. Putting a list there would
    /// lose what it holds. Nothing, and `null`, are fine: an insert there
    /// starts a list.
    NotAList,
    /// The write had to start a node afresh, and the node's generation has no
    /// step left to start a new one. This cannot happen with what this
    /// program writes or accepts; it is here so that such a write is refused
    /// rather than lost.
    NoFreshStart,
    /// The write was refused by the object or list it goes into. See
    /// `WriteError`.
    Write(WriteError),
}

impl From<WriteError> for JsonError {
    fn from(e: WriteError) -> Self {
        JsonError::Write(e)
    }
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JsonError::MutateThroughIndex => write!(f, "cannot mutate through an array index; replace the whole element"),
            JsonError::IndexOutOfBounds { index, len } => write!(f, "array index {index} out of bounds (len {len})"),
            JsonError::EmptyPath => write!(f, "empty path"),
            JsonError::TooDeep { depth, max } => write!(f, "nested {depth} levels deep; the limit is {max}"),
            JsonError::NotAList => write!(f, "there is something at that path already, and it is not a list"),
            JsonError::NoFreshStart => write!(f, "the value at that path cannot be started afresh"),
            JsonError::Write(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JsonError {}

/// How many levels of arrays and objects `value` nests. A single value such as
/// a number is 0, and an empty array or object is 1.
fn levels(value: &JsonValue) -> usize {
    match value {
        JsonValue::Array(items) => 1 + items.iter().map(levels).max().unwrap_or(0),
        JsonValue::Object(map) => 1 + map.values().map(levels).max().unwrap_or(0),
        _ => 0,
    }
}

fn check_depth(depth: usize) -> Result<(), JsonError> {
    if depth > MAX_NESTING {
        return Err(JsonError::TooDeep { depth, max: MAX_NESTING });
    }
    Ok(())
}

impl JsonCrdt {
    /// Sets the value at `path`. Objects missing along the way are created. A
    /// path that ends in an `Index` replaces that whole array element, or
    /// appends when the index equals the array's length. A path that goes on
    /// past an `Index` is rejected with `MutateThroughIndex`.
    ///
    /// The value at the end of the path replaces what was there, and starts
    /// afresh. A field that is written into after it was removed, or after it
    /// was set to something that is not an object, also starts out empty:
    /// what it held before does not come back. See "Generations" at the top
    /// of this file for what happens when other machines write at the same
    /// time.
    ///
    /// A write that would nest deeper than `MAX_NESTING` is rejected with
    /// `TooDeep`.
    ///
    /// The path is checked before anything is changed, so a call that fails
    /// leaves the node exactly as it was.
    pub fn set(&mut self, path: &[Seg], value: &JsonValue, hlc: Hlc, replica: ReplicaId) -> Result<JsonDelta, JsonError> {
        check_depth(path.len() + levels(value))?;
        self.check_set(path, true, value, replica)?;
        let parent = self.tag_now().parent;
        self.set_walk(path, value, hlc, replica, parent, true)
    }

    /// Checks a `set` path without changing anything, and returns the error
    /// `set_walk` would hit. `shown` is as for `kept`. A node that is not
    /// kept is checked as an empty one, because that is what the write makes
    /// of it.
    fn check_set(&self, path: &[Seg], shown: bool, value: &JsonValue, replica: ReplicaId) -> Result<(), JsonError> {
        match path.split_first() {
            None => self.room_for(value, replica),
            Some((Seg::Key(k), rest)) => {
                self.check_start(Shape::Object, shown)?;
                self.check_room(replica, 1, 0)?;
                match self.child(k).filter(|_| self.kept(Shape::Object, shown)) {
                    Some(child) => child.check_set(rest, true, value, replica),
                    None => JsonCrdt::default().check_set(rest, false, value, replica),
                }
            }
            Some((Seg::Index(i), [])) => {
                self.check_start(Shape::Array, shown)?;
                self.check_room(replica, 0, 1)?;
                let len = if self.kept(Shape::Array, shown) { self.items().len() } else { 0 };
                if *i > len {
                    return Err(JsonError::IndexOutOfBounds { index: *i, len });
                }
                Ok(())
            }
            Some((Seg::Index(_), _)) => Err(JsonError::MutateThroughIndex),
        }
    }

    /// Does the work of `set`. Always run `check_set` first: this assumes the
    /// path is fine. `parent` is the generation of the parent this node is
    /// written into, and `shown` is as for `kept`.
    fn set_walk(&mut self, path: &[Seg], value: &JsonValue, hlc: Hlc, replica: ReplicaId, parent: Generation, shown: bool) -> Result<JsonDelta, JsonError> {
        match path.split_first() {
            None => self.assign(value, hlc, replica, parent),
            Some((Seg::Key(k), rest)) => {
                let child_shown = self.kept(Shape::Object, shown) && self.child(k).is_some();
                let (generation, td) = self.start(Shape::Object, shown, parent);
                let mut failed = None;
                let od = self.object.update((generation, k.clone()), replica, |child| {
                    child.set_walk(rest, value, hlc, replica, generation, child_shown).unwrap_or_else(|e| {
                        failed = Some(e);
                        JsonDelta::default()
                    })
                })?;
                if let Some(e) = failed {
                    return Err(e);
                }
                Ok(JsonDelta { tag: td, object: Some(od), array: None, scalar: None })
            }
            Some((Seg::Index(i), _)) => {
                // Replace the whole element at i: insert the new one, then
                // tombstone the old one if there was one.
                let vis: Vec<Dot> =
                    if self.kept(Shape::Array, shown) { self.items().into_iter().map(|(d, _)| d).collect() } else { Vec::new() };
                let (generation, td) = self.start(Shape::Array, shown, parent);
                let pred = if *i == 0 { None } else { Some(vis[*i - 1]) };
                let child = JsonCrdt::made(value, hlc, replica, generation)?;
                let ed = self.array.insert_after(pred, child, replica)?;
                let elements = ed.elements().map(|(dot, el)| (*dot, el.clone())).collect();
                let mut tombstones = BTreeSet::new();
                if *i < vis.len() {
                    self.array.tombstone(vis[*i]);
                    tombstones.insert(vis[*i]);
                }
                let ad = RgaDelta::from_parts(elements, tombstones, DotContext::new());
                Ok(JsonDelta { tag: td, array: Some(ad), object: None, scalar: None })
            }
        }
    }

    /// Inserts `value` at position `index` of the array at `path`. Where there
    /// is nothing yet, or `null`, position 0 starts a new array. Where there
    /// is something else, an object, a text, a number or a true/false, the
    /// insert is refused with `NotAList`. A path that goes through an array
    /// index is rejected.
    ///
    /// The path and index are checked before anything is changed, so a call
    /// that fails leaves the node exactly as it was.
    pub fn insert(&mut self, path: &[Seg], index: usize, value: &JsonValue, hlc: Hlc, replica: ReplicaId) -> Result<JsonDelta, JsonError> {
        // The array at `path` is one level, and the new item sits inside it.
        check_depth(path.len() + 1 + levels(value))?;
        self.check_insert(path, index, true, replica)?;
        let parent = self.tag_now().parent;
        self.insert_walk(path, index, value, hlc, replica, parent, true)
    }

    /// Checks an `insert` without changing anything. Works like `check_set`.
    fn check_insert(&self, path: &[Seg], index: usize, shown: bool, replica: ReplicaId) -> Result<(), JsonError> {
        match path.split_first() {
            None => {
                if shown {
                    let in_the_way = match self.tag_now().shape {
                        Shape::Array | Shape::Absent => false,
                        Shape::Object => true,
                        Shape::Scalar => !matches!(self.scalar.get(), None | Some(JsonScalar::Null)),
                    };
                    if in_the_way {
                        return Err(JsonError::NotAList);
                    }
                }
                self.check_start(Shape::Array, shown)?;
                let len = if self.kept(Shape::Array, shown) { self.items().len() } else { 0 };
                if index > len {
                    return Err(JsonError::IndexOutOfBounds { index, len });
                }
                self.check_room(replica, 0, 1)
            }
            Some((Seg::Key(k), rest)) => {
                self.check_start(Shape::Object, shown)?;
                self.check_room(replica, 1, 0)?;
                match self.child(k).filter(|_| self.kept(Shape::Object, shown)) {
                    Some(child) => child.check_insert(rest, index, true, replica),
                    None => JsonCrdt::default().check_insert(rest, index, false, replica),
                }
            }
            Some((Seg::Index(_), _)) => Err(JsonError::MutateThroughIndex),
        }
    }

    /// Does the work of `insert`. Always run `check_insert` first. The
    /// arguments are as for `set_walk`.
    #[allow(clippy::too_many_arguments)]
    fn insert_walk(&mut self, path: &[Seg], index: usize, value: &JsonValue, hlc: Hlc, replica: ReplicaId, parent: Generation, shown: bool) -> Result<JsonDelta, JsonError> {
        match path.split_first() {
            None => {
                let vis: Vec<Dot> =
                    if self.kept(Shape::Array, shown) { self.items().into_iter().map(|(d, _)| d).collect() } else { Vec::new() };
                let (generation, td) = self.start(Shape::Array, shown, parent);
                let pred = if index == 0 { None } else { Some(vis[index - 1]) };
                let child = JsonCrdt::made(value, hlc, replica, generation)?;
                let ed = self.array.insert_after(pred, child, replica)?;
                Ok(JsonDelta { tag: td, array: Some(ed), object: None, scalar: None })
            }
            Some((Seg::Key(k), rest)) => {
                let child_shown = self.kept(Shape::Object, shown) && self.child(k).is_some();
                let (generation, td) = self.start(Shape::Object, shown, parent);
                let mut failed = None;
                let od = self.object.update((generation, k.clone()), replica, |child| {
                    child.insert_walk(rest, index, value, hlc, replica, generation, child_shown).unwrap_or_else(|e| {
                        failed = Some(e);
                        JsonDelta::default()
                    })
                })?;
                if let Some(e) = failed {
                    return Err(e);
                }
                Ok(JsonDelta { tag: td, object: Some(od), array: None, scalar: None })
            }
            // `check_insert` refuses this, so it is never reached.
            Some((Seg::Index(_), _)) => Ok(JsonDelta::default()),
        }
    }

    /// Removes the value at `path`. A path that ends in a `Key` removes that
    /// field from its object. That replaces the field with nothing: it writes
    /// the field's tag as `Absent`, in a new generation named after this
    /// change, so a write made inside the field at the same time is dropped,
    /// and a later write into it starts afresh. A path that ends in an `Index`
    /// tombstones that array element, which hides it for good. A path that
    /// goes through an array index is rejected. If a key along the way is not
    /// there, nothing is removed and nothing is created.
    ///
    /// The path is checked before anything is changed, so a call that fails
    /// leaves the node exactly as it was.
    pub fn remove(&mut self, path: &[Seg], hlc: Hlc, replica: ReplicaId) -> Result<JsonDelta, JsonError> {
        self.check_remove(path)?;
        self.remove_walk(path, hlc, replica)
    }

    /// Checks a `remove` path without changing anything. Works like
    /// `check_set`, except that a key that is not there ends the check: there
    /// is nothing to remove, and that is fine.
    fn check_remove(&self, path: &[Seg]) -> Result<(), JsonError> {
        if path.iter().rev().skip(1).any(|s| matches!(s, Seg::Index(_))) {
            return Err(JsonError::MutateThroughIndex);
        }
        match path.split_first() {
            None => Err(JsonError::EmptyPath),
            Some((Seg::Key(_), [])) => Ok(()),
            Some((Seg::Index(i), [])) => {
                let len = self.items().len();
                if *i >= len {
                    return Err(JsonError::IndexOutOfBounds { index: *i, len });
                }
                Ok(())
            }
            Some((Seg::Key(k), rest)) => match self.child(k) {
                Some(child) => child.check_remove(rest),
                None => Ok(()),
            },
            Some((Seg::Index(_), _)) => Err(JsonError::MutateThroughIndex),
        }
    }

    /// Does the work of `remove`. Always run `check_remove` first.
    fn remove_walk(&mut self, path: &[Seg], hlc: Hlc, replica: ReplicaId) -> Result<JsonDelta, JsonError> {
        let here = self.tag_now().generation;
        match path.split_first() {
            None => Err(JsonError::EmptyPath),
            Some((Seg::Key(k), [])) => {
                if self.child(k).is_none() {
                    return Ok(JsonDelta::default());
                }
                let at = (here, k.clone());
                let gone = JsonTag { generation: Generation { ts: hlc, writer: replica, step: 0 }, shape: Shape::Absent, parent: here };
                let written = self.object.update_present(&at, |child| JsonDelta {
                    tag: Some(child.tag.set(gone, hlc, replica)),
                    ..JsonDelta::default()
                });
                let removed = self.object.remove(&at);
                let values = written.iter().flat_map(|d| d.values()).map(|(k, v)| (k.clone(), v.clone())).collect();
                let od = OrMapDelta::from_parts(BTreeMap::new(), values, removed.context().clone());
                Ok(JsonDelta { tag: None, object: Some(od), array: None, scalar: None })
            }
            Some((Seg::Index(i), [])) => {
                let vis = self.items();
                if *i >= vis.len() {
                    return Err(JsonError::IndexOutOfBounds { index: *i, len: vis.len() });
                }
                let dot = vis[*i].0;
                let ed = self.array.tombstone(dot);
                Ok(JsonDelta { tag: None, array: Some(ed), object: None, scalar: None })
            }
            Some((Seg::Key(k), rest)) => {
                if self.child(k).is_none() {
                    return Ok(JsonDelta::default());
                }
                let mut err = None;
                // Change the child in place, without marking `k` present again.
                let od = self.object.update_present(&(here, k.clone()), |child| match child.remove_walk(rest, hlc, replica) {
                    Ok(d) => d,
                    Err(e) => { err = Some(e); JsonDelta::default() }
                });
                if let Some(e) = err { return Err(e); }
                Ok(JsonDelta { tag: None, object: od, array: None, scalar: None })
            }
            Some((Seg::Index(_), _)) => Err(JsonError::MutateThroughIndex),
        }
    }

    /// Reads the value at `path`, or returns `None` if nothing is there. A read
    /// changes nothing, so unlike the writes it may go through array indexes.
    /// It goes into a field only where the node is an object now, and into an
    /// item only where it is an array now, so what was overwritten or removed
    /// is never found.
    pub fn get(&self, path: &[Seg]) -> Option<JsonValue> {
        match path.split_first() {
            None => Some(self.to_value()),
            Some((Seg::Key(k), rest)) => self.child(k)?.get(rest),
            Some((Seg::Index(i), rest)) => self.items().get(*i)?.1.get(rest),
        }
    }
}

impl DeltaCrdt for JsonCrdt {
    type Delta = JsonDelta;

    fn merge(&mut self, delta: JsonDelta) {
        if let Some(d) = delta.tag { self.tag.merge(d); }
        if let Some(d) = delta.object { self.object.merge(d); }
        if let Some(d) = delta.array { self.array.merge(d); }
        if let Some(d) = delta.scalar { self.scalar.merge(d); }
    }

    fn snapshot(&self) -> JsonDelta {
        JsonDelta {
            tag: Some(self.tag.snapshot()),
            object: Some(self.object.snapshot()),
            array: Some(self.array.snapshot()),
            scalar: Some(self.scalar.snapshot()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hlc(w: u64) -> Hlc { Hlc { wall: w, logical: 0 } }

    // Two changes sent as their join do on another machine what the two do
    // sent one after the other, whatever was there before.
    #[test]
    fn two_changes_joined_do_what_both_do() {
        let (r1, r2) = (ReplicaId(1), ReplicaId(2));
        let key = |k: &str| Seg::Key(k.into());
        let task = JsonValue::from_json_str(r#"{"title":"x","notes":"n","tags":["a"]}"#).unwrap();
        let mut a = JsonCrdt::default();
        let start = a.set(&[key("t")], &task, hlc(1), r1).unwrap();
        let mut b = JsonCrdt::default();
        b.merge(start.clone());
        let mut c = JsonCrdt::default();
        c.merge(start);
        let pairs = [
            (vec![key("t"), key("title")], Some(JsonValue::Str("y".into())), vec![key("t"), key("updated_by")]),
            (vec![key("t"), key("notes")], None, vec![key("t"), key("updated_by")]),
            (vec![key("t"), key("tags")], Some(JsonValue::Null), vec![key("t"), key("seen")]),
        ];
        for (at, (path, value, named)) in pairs.into_iter().enumerate() {
            let w = 10 + at as u64 * 2;
            let first = match value {
                Some(value) => a.set(&path, &value, hlc(w), r2).unwrap(),
                None => a.remove(&path, hlc(w), r2).unwrap(),
            };
            let second = a.set(&named, &JsonValue::Str("bob".into()), hlc(w), r2).unwrap();
            b.merge(first.clone());
            b.merge(second.clone());
            c.merge(first.join(second));
            assert_eq!(b.to_value(), a.to_value());
            assert_eq!(c.to_value(), a.to_value());
        }
    }

    #[test]
    fn fresh_node_reads_null() {
        assert_eq!(JsonCrdt::default().to_value(), JsonValue::Null);
    }

    #[test]
    fn scalar_round_trips_through_value() {
        for v in [JsonValue::Null, JsonValue::Bool(true), JsonValue::Num(1.5), JsonValue::Str("hi".into())] {
            let n = JsonCrdt::from_value(&v, hlc(1), ReplicaId(1));
            assert_eq!(n.to_value(), v);
        }
    }

    #[test]
    fn object_and_array_round_trip() {
        let mut obj = BTreeMap::new();
        obj.insert("k".to_string(), JsonValue::Num(2.0));
        obj.insert("a".to_string(), JsonValue::Array(vec![JsonValue::Bool(false), JsonValue::Str("x".into())]));
        let v = JsonValue::Object(obj);
        let n = JsonCrdt::from_value(&v, hlc(1), ReplicaId(1));
        assert_eq!(n.to_value(), v);
    }

    #[test]
    fn product_merge_converges_regardless_of_order() {
        // Two machines write different keys. Merging in either order must agree.
        let mut a = JsonCrdt::default();
        let da = a.assign(&{ let mut m = BTreeMap::new(); m.insert("x".into(), JsonValue::Num(1.0)); JsonValue::Object(m) }, hlc(1), ReplicaId(1), Generation::default()).unwrap();
        let mut b = JsonCrdt::default();
        let db = b.assign(&{ let mut m = BTreeMap::new(); m.insert("y".into(), JsonValue::Num(2.0)); JsonValue::Object(m) }, hlc(1), ReplicaId(2), Generation::default()).unwrap();
        let mut ab = a.clone(); ab.merge(db.clone());
        let mut ba = b.clone(); ba.merge(da.clone());
        assert_eq!(ab, ba, "merge is commutative");
        // merging the same deltas again changes nothing
        let mut twice = ab.clone(); twice.merge(db); twice.merge(da);
        assert_eq!(twice, ab);
    }

    #[test]
    fn cross_type_conflict_resolves_by_tag_lww() {
        // Two machines set the same node without seeing each other: one to a
        // number at timestamp 1, the other to an object at timestamp 2.
        let mut a = JsonCrdt::default();
        let da = a.assign(&JsonValue::Num(9.0), hlc(1), ReplicaId(1), Generation::default()).unwrap();
        let mut b = JsonCrdt::default();
        let db = b.assign(&{ let mut m = BTreeMap::new(); m.insert("k".into(), JsonValue::Bool(true)); JsonValue::Object(m) }, hlc(2), ReplicaId(2), Generation::default()).unwrap();
        let mut ab = a.clone(); ab.merge(db.clone());
        let mut ba = b.clone(); ba.merge(da.clone());
        assert_eq!(ab, ba);
        // The later timestamp (the object, at 2) wins the tag.
        assert!(matches!(ab.to_value(), JsonValue::Object(_)), "later write's shape presented");
    }

    fn key(k: &str) -> Seg { Seg::Key(k.to_string()) }

    #[test]
    fn set_nested_path_auto_vivifies_objects() {
        let mut n = JsonCrdt::default();
        n.set(&[key("a"), key("b")], &JsonValue::Num(5.0), hlc(1), ReplicaId(1)).unwrap();
        assert_eq!(n.get(&[key("a"), key("b")]), Some(JsonValue::Num(5.0)));
        // intermediate "a" is an object
        assert!(matches!(n.get(&[key("a")]), Some(JsonValue::Object(_))));
    }

    #[test]
    fn array_insert_remove_and_read() {
        let mut n = JsonCrdt::default();
        n.set(&[key("xs")], &JsonValue::Array(vec![]), hlc(1), ReplicaId(1)).unwrap();
        n.insert(&[key("xs")], 0, &JsonValue::Str("a".into()), hlc(2), ReplicaId(1)).unwrap();
        n.insert(&[key("xs")], 1, &JsonValue::Str("b".into()), hlc(3), ReplicaId(1)).unwrap();
        n.insert(&[key("xs")], 1, &JsonValue::Str("x".into()), hlc(4), ReplicaId(1)).unwrap();
        assert_eq!(
            n.get(&[key("xs")]),
            Some(JsonValue::Array(vec![JsonValue::Str("a".into()), JsonValue::Str("x".into()), JsonValue::Str("b".into())]))
        );
        n.remove(&[key("xs"), Seg::Index(1)], hlc(5), ReplicaId(1)).unwrap();
        assert_eq!(
            n.get(&[key("xs")]),
            Some(JsonValue::Array(vec![JsonValue::Str("a".into()), JsonValue::Str("b".into())]))
        );
    }

    #[test]
    fn mutating_through_an_array_index_is_rejected() {
        let mut n = JsonCrdt::default();
        n.set(&[key("xs")], &JsonValue::Array(vec![JsonValue::Object(BTreeMap::new())]), hlc(1), ReplicaId(1)).unwrap();
        // Setting a field inside element 0 must fail. Array merge keeps the
        // first copy of an element, so the machines would end up different.
        let err = n.set(&[key("xs"), Seg::Index(0), key("f")], &JsonValue::Num(1.0), hlc(2), ReplicaId(1));
        assert!(matches!(err, Err(JsonError::MutateThroughIndex)));
    }

    #[test]
    fn set_ending_in_index_replaces_whole_element() {
        let mut n = JsonCrdt::default();
        n.set(&[key("xs")], &JsonValue::Array(vec![JsonValue::Num(1.0), JsonValue::Num(2.0)]), hlc(1), ReplicaId(1)).unwrap();
        n.set(&[key("xs"), Seg::Index(0)], &JsonValue::Num(9.0), hlc(2), ReplicaId(1)).unwrap();
        assert_eq!(
            n.get(&[key("xs")]),
            Some(JsonValue::Array(vec![JsonValue::Num(9.0), JsonValue::Num(2.0)]))
        );
    }

    #[test]
    fn failing_ops_leave_state_untouched() {
        let mut n = JsonCrdt::default();
        n.set(&[key("xs")], &JsonValue::Array(vec![JsonValue::Num(1.0)]), hlc(1), ReplicaId(1)).unwrap();
        let before = n.clone();
        // a path that goes through an array index
        assert!(matches!(
            n.set(&[key("xs"), Seg::Index(0), key("f")], &JsonValue::Num(2.0), hlc(2), ReplicaId(1)),
            Err(JsonError::MutateThroughIndex)
        ));
        assert_eq!(n, before);
        // out-of-bounds set / insert / remove
        assert!(matches!(
            n.set(&[key("xs"), Seg::Index(5)], &JsonValue::Num(2.0), hlc(2), ReplicaId(1)),
            Err(JsonError::IndexOutOfBounds { .. })
        ));
        assert_eq!(n, before);
        assert!(matches!(
            n.insert(&[key("xs")], 5, &JsonValue::Num(2.0), hlc(2), ReplicaId(1)),
            Err(JsonError::IndexOutOfBounds { .. })
        ));
        assert_eq!(n, before);
        assert!(matches!(
            n.remove(&[key("xs"), Seg::Index(5)], hlc(2), ReplicaId(1)),
            Err(JsonError::IndexOutOfBounds { .. })
        ));
        assert_eq!(n, before);
    }

    #[test]
    fn failed_op_does_not_diverge_replicas() {
        let mut a = JsonCrdt::default();
        let mut b = JsonCrdt::default();
        let d = a.set(&[key("xs")], &JsonValue::Array(vec![JsonValue::Num(1.0)]), hlc(1), ReplicaId(1)).unwrap();
        b.merge(d);
        assert_eq!(a, b);
        // A tries an operation that fails and B does nothing. They must stay equal.
        assert!(a.set(&[key("xs"), Seg::Index(0), key("f")], &JsonValue::Num(2.0), hlc(2), ReplicaId(1)).is_err());
        assert_eq!(a, b);
        // A then makes a change that works and sends it to B. Still equal.
        let d2 = a.set(&[key("y")], &JsonValue::Bool(true), hlc(3), ReplicaId(1)).unwrap();
        b.merge(d2);
        assert_eq!(a, b);
    }

    fn nums(xs: &[f64]) -> JsonValue {
        JsonValue::Array(xs.iter().map(|x| JsonValue::Num(*x)).collect())
    }

    fn obj(pairs: &[(&str, f64)]) -> JsonValue {
        JsonValue::Object(pairs.iter().map(|(k, v)| (k.to_string(), JsonValue::Num(*v))).collect())
    }

    // Returns a second copy of `n` that belongs to another machine.
    fn copy_of(n: &JsonCrdt) -> JsonCrdt {
        let mut other = JsonCrdt::default();
        other.merge(n.snapshot());
        other
    }

    #[test]
    fn array_writes_by_another_machine_land_where_asked() {
        let mut a = JsonCrdt::default();
        a.set(&[key("xs")], &nums(&[1.0, 2.0, 3.0]), hlc(1), ReplicaId(1)).unwrap();

        let mut b = copy_of(&a);
        let d = b.insert(&[key("xs")], 1, &JsonValue::Num(7.0), hlc(2), ReplicaId(2)).unwrap();
        assert_eq!(b.get(&[key("xs")]), Some(nums(&[1.0, 7.0, 2.0, 3.0])));
        let mut a2 = a.clone();
        a2.merge(d);
        assert_eq!(a2, b);

        let mut c = copy_of(&a);
        let d = c.set(&[key("xs"), Seg::Index(1)], &JsonValue::Num(9.0), hlc(2), ReplicaId(2)).unwrap();
        assert_eq!(c.get(&[key("xs")]), Some(nums(&[1.0, 9.0, 3.0])));
        a.merge(d);
        assert_eq!(a, c);
    }

    #[test]
    fn set_replaces_an_array() {
        let mut a = JsonCrdt::default();
        let d1 = a.set(&[key("xs")], &nums(&[1.0, 2.0, 3.0]), hlc(1), ReplicaId(1)).unwrap();
        let mut b = JsonCrdt::default();
        b.merge(d1);

        let d2 = a.set(&[key("xs")], &nums(&[4.0]), hlc(2), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("xs")]), Some(nums(&[4.0])));
        b.merge(d2);
        assert_eq!(a, b);
    }

    #[test]
    fn set_replaces_an_object() {
        let mut a = JsonCrdt::default();
        let d1 = a.set(&[key("o")], &obj(&[("a", 1.0), ("b", 2.0)]), hlc(1), ReplicaId(1)).unwrap();
        let mut b = JsonCrdt::default();
        b.merge(d1);

        let d2 = a.set(&[key("o")], &obj(&[("b", 5.0), ("c", 3.0)]), hlc(2), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("o")]), Some(obj(&[("b", 5.0), ("c", 3.0)])));
        b.merge(d2);
        assert_eq!(a, b);
    }

    #[test]
    fn set_after_remove_does_not_bring_old_contents_back() {
        let mut a = JsonCrdt::default();
        a.set(&[key("xs")], &nums(&[1.0, 2.0, 3.0]), hlc(1), ReplicaId(1)).unwrap();
        a.set(&[key("o")], &obj(&[("a", 1.0)]), hlc(2), ReplicaId(1)).unwrap();
        a.remove(&[key("xs")], hlc(3), ReplicaId(1)).unwrap();
        a.remove(&[key("o")], hlc(4), ReplicaId(1)).unwrap();

        a.set(&[key("xs")], &nums(&[5.0]), hlc(5), ReplicaId(1)).unwrap();
        a.set(&[key("o")], &obj(&[("b", 2.0)]), hlc(6), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("xs")]), Some(nums(&[5.0])));
        assert_eq!(a.get(&[key("o")]), Some(obj(&[("b", 2.0)])));
        assert_eq!(copy_of(&a), a);
    }

    #[test]
    fn set_replaces_what_another_machine_wrote_earlier() {
        // B has seen A's array, then replaces it. A's items must go everywhere.
        let mut a = JsonCrdt::default();
        a.set(&[key("xs")], &nums(&[1.0, 2.0]), hlc(1), ReplicaId(1)).unwrap();
        let mut b = copy_of(&a);
        let d = b.set(&[key("xs")], &nums(&[9.0]), hlc(2), ReplicaId(2)).unwrap();
        a.merge(d);
        assert_eq!(a.get(&[key("xs")]), Some(nums(&[9.0])));
        assert_eq!(a, b);
    }

    #[test]
    fn remove_of_a_missing_nested_path_changes_nothing() {
        let mut n = JsonCrdt::default();
        n.set(&[key("keep")], &JsonValue::Num(1.0), hlc(1), ReplicaId(1)).unwrap();
        let before = n.clone();

        let d = n.remove(&[key("ghost"), key("b")], hlc(2), ReplicaId(1)).unwrap();
        assert_eq!(n, before);
        assert_eq!(n.get(&[key("ghost")]), None);

        // The delta changes nothing on another machine either.
        let mut other = copy_of(&before);
        let other_before = other.clone();
        other.merge(d);
        assert_eq!(other, other_before);
    }

    #[test]
    fn remove_inside_a_removed_parent_does_not_bring_it_back() {
        let mut n = JsonCrdt::default();
        n.set(&[key("o")], &obj(&[("a", 1.0), ("b", 2.0)]), hlc(1), ReplicaId(1)).unwrap();
        n.remove(&[key("o")], hlc(2), ReplicaId(1)).unwrap();
        n.remove(&[key("o"), key("a")], hlc(3), ReplicaId(1)).unwrap();
        assert_eq!(n.get(&[key("o")]), None);
    }

    #[test]
    fn remove_of_a_nested_key_reaches_other_machines() {
        let mut a = JsonCrdt::default();
        a.set(&[key("o")], &obj(&[("a", 1.0), ("b", 2.0)]), hlc(1), ReplicaId(1)).unwrap();
        let mut b = copy_of(&a);
        let d = a.remove(&[key("o"), key("a")], hlc(2), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("o")]), Some(obj(&[("b", 2.0)])));
        b.merge(d);
        assert_eq!(a, b);
    }

    // One machine removes a nested key while another removes its parent. The
    // nested removal must not keep the parent alive.
    #[test]
    fn nested_remove_does_not_undo_a_removal_of_the_parent() {
        let mut a = JsonCrdt::default();
        a.set(&[key("o")], &obj(&[("a", 1.0), ("b", 2.0)]), hlc(1), ReplicaId(1)).unwrap();
        let mut b = copy_of(&a);
        let da = a.remove(&[key("o"), key("a")], hlc(2), ReplicaId(1)).unwrap();
        let db = b.remove(&[key("o")], hlc(2), ReplicaId(2)).unwrap();
        a.merge(db);
        b.merge(da);
        assert_eq!(a, b);
        assert_eq!(a.get(&[key("o")]), None);
    }

    fn nested(levels: usize) -> JsonValue {
        let mut v = JsonValue::Num(1.0);
        for _ in 0..levels {
            v = JsonValue::Array(vec![v]);
        }
        v
    }

    #[test]
    fn nesting_past_the_limit_is_refused() {
        let mut n = JsonCrdt::default();
        assert!(n.set(&[], &nested(MAX_NESTING), hlc(1), ReplicaId(1)).is_ok());

        let before = n.clone();
        let too_deep = Err(JsonError::TooDeep { depth: MAX_NESTING + 1, max: MAX_NESTING });
        assert_eq!(n.set(&[], &nested(MAX_NESTING + 1), hlc(2), ReplicaId(1)), too_deep);
        // The path counts too: one key, then a value at the limit.
        assert_eq!(n.set(&[key("k")], &nested(MAX_NESTING), hlc(2), ReplicaId(1)), too_deep);
        assert_eq!(n.insert(&[], 0, &nested(MAX_NESTING), hlc(2), ReplicaId(1)), too_deep);
        assert_eq!(n, before);
    }

    #[test]
    fn ordered_f64_total_orders_nan() {
        let a = OrderedF64(f64::NAN);
        let b = OrderedF64(1.0);
        assert_eq!(a.cmp(&a), std::cmp::Ordering::Equal);
        assert_ne!(a.cmp(&b), std::cmp::Ordering::Equal);
    }

    #[test]
    fn scalars_round_trip() {
        for v in [JsonValue::Null, JsonValue::Bool(true), JsonValue::Bool(false),
                  JsonValue::Num(2.0), JsonValue::Num(1.5), JsonValue::Num(-3.25),
                  JsonValue::Str("hi".into())] {
            assert_eq!(JsonValue::from_json_str(&v.to_json_string()).unwrap(), v);
        }
    }

    #[test]
    fn integer_number_serializes_without_point() {
        assert_eq!(JsonValue::Num(2.0).to_json_string(), "2");
        assert_eq!(JsonValue::from_json_str("2").unwrap(), JsonValue::Num(2.0));
    }

    #[test]
    fn strings_escape_and_round_trip() {
        let s = JsonValue::Str("a\"b\\c\nd\te\u{0}f".into());
        let json = s.to_json_string();
        assert!(json.contains("\\\""), "quote escaped");
        assert!(json.contains("\\n"), "newline escaped");
        assert!(json.contains("\\u0000"), "control char escaped");
        assert_eq!(JsonValue::from_json_str(&json).unwrap(), s);
    }

    #[test]
    fn unicode_passthrough_and_u_escape_parse() {
        let s = JsonValue::Str("héllo→世界".into());
        assert_eq!(JsonValue::from_json_str(&s.to_json_string()).unwrap(), s);
        // Characters beyond ASCII written straight into the input, including
        // an emoji, parse as themselves.
        assert_eq!(JsonValue::from_json_str(r#""é""#).unwrap(), JsonValue::Str("é".into()));
        assert_eq!(JsonValue::from_json_str(r#""😀""#).unwrap(), JsonValue::Str("😀".into()));
    }

    #[test]
    fn nested_object_and_array_round_trip() {
        let json = r#"{"a":[1,2,{"b":null}],"c":{"d":true}}"#;
        let v = JsonValue::from_json_str(json).unwrap();
        // Keys are written in sorted order, so writing it back gives the same string.
        assert_eq!(v.to_json_string(), json);
    }

    #[test]
    fn empty_containers_and_whitespace() {
        assert_eq!(JsonValue::from_json_str("  {\n}\t").unwrap(), JsonValue::Object(BTreeMap::new()));
        assert_eq!(JsonValue::from_json_str("[ ]").unwrap(), JsonValue::Array(vec![]));
        assert_eq!(JsonValue::from_json_str(" [ 1 , 2 ] ").unwrap(),
                   JsonValue::Array(vec![JsonValue::Num(1.0), JsonValue::Num(2.0)]));
    }

    #[test]
    fn malformed_input_errs_never_panics() {
        for bad in ["", "{", "[1,]", "{\"a\"}", "nul", "\"unterminated", "1 2", "{a:1}", "[1 2]"] {
            assert!(JsonValue::from_json_str(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn non_finite_number_serializes_as_null() {
        assert_eq!(JsonValue::Num(f64::NAN).to_json_string(), "null");
        assert_eq!(JsonValue::Num(f64::INFINITY).to_json_string(), "null");
    }

    #[test]
    fn deeply_nested_input_errs_not_overflows() {
        // Nested far deeper than the limit. This must return an error, not
        // crash the process.
        assert!(JsonValue::from_json_str(&"[".repeat(1000)).is_err());
        assert!(JsonValue::from_json_str(&("[".repeat(1000) + &"]".repeat(1000))).is_err());
        assert!(JsonValue::from_json_str(&"{\"a\":".repeat(1000)).is_err());
        // A valid document nested well under the limit still parses.
        let ok = "[".repeat(50) + &"]".repeat(50);
        assert!(JsonValue::from_json_str(&ok).is_ok());
    }

    #[test]
    fn parse_path_keys_and_indices() {
        assert_eq!(parse_path(r#"["users",0,"name"]"#).unwrap(),
                   vec![Seg::Key("users".into()), Seg::Index(0), Seg::Key("name".into())]);
        assert_eq!(parse_path("[]").unwrap(), Vec::<Seg>::new());
        assert!(parse_path(r#"["a",-1]"#).is_err(), "negative index");
        assert!(parse_path(r#"["a",1.5]"#).is_err(), "non-integer index");
        assert!(parse_path(r#"{"a":1}"#).is_err(), "path must be an array");
    }

    #[test]
    fn reading_inside_an_overwritten_field_finds_nothing() {
        let mut n = JsonCrdt::default();
        n.set(&[key("o")], &obj(&[("a", 1.0)]), hlc(1), ReplicaId(1)).unwrap();
        n.set(&[key("o")], &JsonValue::Num(5.0), hlc(2), ReplicaId(1)).unwrap();
        assert_eq!(n.get(&[key("o"), key("a")]), None);
        n.set(&[key("xs")], &nums(&[1.0]), hlc(3), ReplicaId(1)).unwrap();
        n.set(&[key("xs")], &JsonValue::Str("t".into()), hlc(4), ReplicaId(1)).unwrap();
        assert_eq!(n.get(&[key("xs"), Seg::Index(0)]), None);
        // An object is not a list, and a list is not an object.
        n.set(&[key("p")], &obj(&[("a", 1.0)]), hlc(5), ReplicaId(1)).unwrap();
        n.set(&[key("p")], &nums(&[7.0]), hlc(6), ReplicaId(1)).unwrap();
        assert_eq!(n.get(&[key("p"), key("a")]), None);
        assert_eq!(n.get(&[key("p"), Seg::Index(0)]), Some(JsonValue::Num(7.0)));
    }

    #[test]
    fn writing_inside_an_overwritten_field_does_not_bring_old_keys_back() {
        let mut a = JsonCrdt::default();
        a.set(&[key("o")], &obj(&[("a", 1.0)]), hlc(1), ReplicaId(1)).unwrap();
        a.set(&[key("o")], &JsonValue::Num(5.0), hlc(2), ReplicaId(1)).unwrap();
        a.set(&[key("o"), key("b")], &JsonValue::Num(2.0), hlc(3), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("o")]), Some(obj(&[("b", 2.0)])));

        a.set(&[key("xs")], &nums(&[1.0, 2.0]), hlc(4), ReplicaId(1)).unwrap();
        a.set(&[key("xs")], &JsonValue::Null, hlc(5), ReplicaId(1)).unwrap();
        a.insert(&[key("xs")], 0, &JsonValue::Num(9.0), hlc(6), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("xs")]), Some(nums(&[9.0])));
        assert_eq!(copy_of(&a).to_value(), a.to_value());
    }

    #[test]
    fn writing_inside_a_removed_field_does_not_bring_old_contents_back() {
        let mut a = JsonCrdt::default();
        a.set(&[key("xs")], &nums(&[1.0, 2.0]), hlc(1), ReplicaId(1)).unwrap();
        a.remove(&[key("xs")], hlc(2), ReplicaId(1)).unwrap();
        a.insert(&[key("xs")], 0, &JsonValue::Num(3.0), hlc(3), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("xs")]), Some(nums(&[3.0])));

        a.set(&[key("o")], &obj(&[("a", 1.0)]), hlc(4), ReplicaId(1)).unwrap();
        a.remove(&[key("o")], hlc(5), ReplicaId(1)).unwrap();
        a.set(&[key("o"), key("b")], &JsonValue::Num(2.0), hlc(6), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("o")]), Some(obj(&[("b", 2.0)])));

        // Deeper too: the old contents of a removed parent stay gone.
        a.set(&[key("d")], &JsonValue::Object([("e".to_string(), obj(&[("f", 1.0)]))].into()), hlc(7), ReplicaId(1)).unwrap();
        a.remove(&[key("d")], hlc(8), ReplicaId(1)).unwrap();
        a.set(&[key("d"), key("e"), key("g")], &JsonValue::Num(2.0), hlc(9), ReplicaId(1)).unwrap();
        assert_eq!(a.get(&[key("d"), key("e")]), Some(obj(&[("g", 2.0)])));

        // Another machine that gets the same changes shows the same.
        let b = copy_of(&a);
        assert_eq!(b.to_value(), a.to_value());
    }

    #[test]
    fn a_later_whole_list_or_object_wins_outright() {
        for (first, second) in [(nums(&[1.0, 2.0]), nums(&[3.0, 4.0])), (obj(&[("a", 1.0)]), obj(&[("b", 2.0)]))] {
            let mut a = JsonCrdt::default();
            let mut b = JsonCrdt::default();
            let da = a.set(&[key("v")], &first, hlc(1), ReplicaId(1)).unwrap();
            let db = b.set(&[key("v")], &second, hlc(2), ReplicaId(2)).unwrap();
            a.merge(db.clone());
            b.merge(da.clone());
            assert_eq!(a, b);
            assert_eq!(a.get(&[key("v")]), Some(second.clone()));

            // At the same moment, the higher machine id wins, as for any
            // other last-writer-wins value.
            let mut a = JsonCrdt::default();
            let mut b = JsonCrdt::default();
            let da = a.set(&[key("v")], &second, hlc(1), ReplicaId(1)).unwrap();
            let db = b.set(&[key("v")], &first, hlc(1), ReplicaId(2)).unwrap();
            a.merge(db);
            b.merge(da);
            assert_eq!(a, b);
            assert_eq!(a.get(&[key("v")]), Some(first.clone()));
        }
    }

    // Two machines that write into the same place from the same starting
    // point land in the same new generation, so both writes show.
    #[test]
    fn writes_into_a_fresh_place_from_two_machines_both_show() {
        let mut start = JsonCrdt::default();
        start.set(&[key("o")], &obj(&[("old", 1.0)]), hlc(1), ReplicaId(1)).unwrap();
        start.set(&[key("n")], &JsonValue::Num(1.0), hlc(2), ReplicaId(1)).unwrap();
        start.remove(&[key("o")], hlc(3), ReplicaId(1)).unwrap();
        let mut a = copy_of(&start);
        let mut b = copy_of(&start);
        let da = [
            a.set(&[key("o"), key("a")], &JsonValue::Num(1.0), hlc(4), ReplicaId(1)).unwrap(),
            a.set(&[key("n"), key("a")], &JsonValue::Num(1.0), hlc(5), ReplicaId(1)).unwrap(),
            a.insert(&[key("new")], 0, &JsonValue::Num(1.0), hlc(6), ReplicaId(1)).unwrap(),
        ];
        let db = [
            b.set(&[key("o"), key("b")], &JsonValue::Num(2.0), hlc(4), ReplicaId(2)).unwrap(),
            b.set(&[key("n"), key("b")], &JsonValue::Num(2.0), hlc(5), ReplicaId(2)).unwrap(),
            b.insert(&[key("new")], 0, &JsonValue::Num(2.0), hlc(6), ReplicaId(2)).unwrap(),
        ];
        for d in db { a.merge(d); }
        for d in da { b.merge(d); }
        assert_eq!(a, b);
        assert_eq!(a.get(&[key("o")]), Some(obj(&[("a", 1.0), ("b", 2.0)])));
        assert_eq!(a.get(&[key("n")]), Some(obj(&[("a", 1.0), ("b", 2.0)])));
        assert_eq!(a.get(&[key("new")]).map(|v| match v { JsonValue::Array(xs) => xs.len(), _ => 0 }), Some(2));
    }

    fn at(wall: u64, logical: u32) -> Hlc { Hlc { wall, logical } }

    // Hands each machine's changes to the other, and to two fresh copies of
    // `base` in both orders, each change twice. A fourth copy is built from
    // whole snapshots. All of them must hold exactly the same, and what they
    // read is returned.
    fn settle(base: &JsonCrdt, a: &JsonCrdt, da: &[JsonDelta], b: &JsonCrdt, db: &[JsonDelta]) -> JsonValue {
        let mut a2 = a.clone();
        let mut b2 = b.clone();
        for d in db { a2.merge(d.clone()); a2.merge(d.clone()); }
        for d in da { b2.merge(d.clone()); b2.merge(d.clone()); }
        let mut ab = copy_of(base);
        for d in da.iter().chain(db) { ab.merge(d.clone()); ab.merge(d.clone()); }
        let mut ba = copy_of(base);
        for d in db.iter().chain(da) { ba.merge(d.clone()); }
        let mut whole = JsonCrdt::default();
        whole.merge(b.snapshot());
        whole.merge(a.snapshot());
        whole.merge(base.snapshot());
        for other in [&b2, &ab, &ba, &whole] {
            assert_eq!(&a2, other);
        }
        a2.to_value()
    }

    fn doc(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::Object(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    // A path write that both machines make into a removed field lands in
    // the same fresh start, even when one of them had written inside the
    // field before it saw the removal. That earlier write is dropped.
    #[test]
    fn a_write_inside_before_a_removal_does_not_split_the_fresh_start() {
        let mut base = JsonCrdt::default();
        base.set(&[key("o")], &obj(&[("old", 1.0)]), at(1, 0), ReplicaId(3)).unwrap();
        let mut a = copy_of(&base);
        let mut b = copy_of(&base);
        let mut c = copy_of(&base);
        let mut db = vec![b.set(&[key("o"), key("z")], &JsonValue::Num(9.0), at(3, 0), ReplicaId(2)).unwrap()];
        let removal = c.remove(&[key("o")], at(2, 0), ReplicaId(3)).unwrap();
        a.merge(removal.clone());
        b.merge(removal.clone());
        let da = vec![removal, a.set(&[key("o"), key("a")], &JsonValue::Num(1.0), at(5, 0), ReplicaId(1)).unwrap()];
        db.push(b.set(&[key("o"), key("b")], &JsonValue::Num(2.0), at(6, 0), ReplicaId(2)).unwrap());
        assert_eq!(settle(&base, &a, &da, &b, &db), doc(&[("o", obj(&[("a", 1.0), ("b", 2.0)]))]));
    }

    // Two machines each remove a field and write into it again. Each
    // removal starts the field afresh, so the later removal wins, with what
    // its machine wrote after it, and the other machine's fresh start is
    // dropped.
    #[test]
    fn of_two_removals_the_later_one_starts_the_field_afresh() {
        for (a_removes, expected) in [(at(2, 0), obj(&[("b", 2.0)])), (at(4, 1), obj(&[("a", 1.0)]))] {
            let mut base = JsonCrdt::default();
            base.set(&[key("o")], &obj(&[("old", 1.0)]), at(1, 0), ReplicaId(3)).unwrap();
            let mut a = copy_of(&base);
            let mut b = copy_of(&base);
            let da = vec![
                a.remove(&[key("o")], a_removes, ReplicaId(1)).unwrap(),
                a.set(&[key("o"), key("a")], &JsonValue::Num(1.0), at(5, 0), ReplicaId(1)).unwrap(),
            ];
            let db = vec![
                b.set(&[key("o"), key("z")], &JsonValue::Num(9.0), at(3, 0), ReplicaId(2)).unwrap(),
                b.remove(&[key("o")], at(4, 0), ReplicaId(2)).unwrap(),
                b.set(&[key("o"), key("b")], &JsonValue::Num(2.0), at(6, 0), ReplicaId(2)).unwrap(),
            ];
            assert_eq!(settle(&base, &a, &da, &b, &db), doc(&[("o", expected)]));
        }
    }

    // A removes a field and writes into it again, while B writes into the
    // old field. B's write is inside what A replaced, so it is dropped,
    // whichever of the two came later. The old contents never come back.
    #[test]
    fn a_write_into_a_field_that_was_removed_and_started_again_is_dropped() {
        for (a_times, b_time) in [((2, 3), 4), ((3, 4), 2)] {
            let mut base = JsonCrdt::default();
            base.set(&[key("o")], &obj(&[("old", 1.0)]), at(1, 0), ReplicaId(3)).unwrap();
            let mut a = copy_of(&base);
            let mut b = copy_of(&base);
            let da = vec![
                a.remove(&[key("o")], at(a_times.0, 0), ReplicaId(1)).unwrap(),
                a.set(&[key("o"), key("a")], &JsonValue::Num(1.0), at(a_times.1, 0), ReplicaId(1)).unwrap(),
            ];
            let db = vec![b.set(&[key("o"), key("b")], &JsonValue::Num(2.0), at(b_time, 0), ReplicaId(2)).unwrap()];
            assert_eq!(settle(&base, &a, &da, &b, &db), doc(&[("o", obj(&[("a", 1.0)]))]), "{a_times:?} {b_time}");
        }
    }

    // A removal and a write inside the removed field, at the same time: the
    // field stays removed, whichever came later, and the old contents do not
    // come back.
    #[test]
    fn a_removal_beats_a_write_inside_made_at_the_same_time() {
        for (a_time, b_time) in [(2, 3), (3, 2)] {
            let mut base = JsonCrdt::default();
            base.set(&[key("o")], &obj(&[("old", 1.0)]), at(1, 0), ReplicaId(3)).unwrap();
            let mut a = copy_of(&base);
            let mut b = copy_of(&base);
            let da = vec![a.remove(&[key("o")], at(a_time, 0), ReplicaId(1)).unwrap()];
            let db = vec![b.set(&[key("o"), key("b")], &JsonValue::Num(2.0), at(b_time, 0), ReplicaId(2)).unwrap()];
            assert_eq!(settle(&base, &a, &da, &b, &db), doc(&[]), "{a_time} {b_time}");
        }
    }

    // A whole write of an object and a write inside the old one at the same
    // time: the whole write wins, whichever came later, and the old fields
    // do not come back.
    #[test]
    fn a_whole_write_beats_a_write_inside_made_at_the_same_time() {
        for b_time in [6, 4] {
            let mut base = JsonCrdt::default();
            base.set(&[key("cfg")], &obj(&[("x", 1.0), ("y", 2.0)]), at(1, 0), ReplicaId(3)).unwrap();
            let mut a = copy_of(&base);
            let mut b = copy_of(&base);
            let da = vec![a.set(&[key("cfg")], &obj(&[("z", 3.0)]), at(5, 0), ReplicaId(1)).unwrap()];
            let db = vec![b.set(&[key("cfg"), key("x")], &JsonValue::Num(10.0), at(b_time, 0), ReplicaId(2)).unwrap()];
            assert_eq!(settle(&base, &a, &da, &b, &db), doc(&[("cfg", obj(&[("z", 3.0)]))]), "{b_time}");
        }
    }

    // An insert into a list, or a removal inside an object, made at the same
    // time as a whole write of that list or object, is dropped with the old
    // value, whichever came later.
    #[test]
    fn a_whole_write_beats_an_insert_or_removal_inside_made_at_the_same_time() {
        for b_time in [6, 4] {
            let mut base = JsonCrdt::default();
            base.set(&[key("xs")], &nums(&[1.0]), at(1, 0), ReplicaId(3)).unwrap();
            base.set(&[key("o")], &obj(&[("a", 1.0), ("b", 2.0)]), at(2, 0), ReplicaId(3)).unwrap();
            let mut a = copy_of(&base);
            let mut b = copy_of(&base);
            let da = vec![
                a.set(&[key("xs")], &nums(&[9.0]), at(5, 0), ReplicaId(1)).unwrap(),
                a.set(&[key("o")], &obj(&[("a", 5.0)]), at(5, 1), ReplicaId(1)).unwrap(),
            ];
            let db = vec![
                b.insert(&[key("xs")], 0, &JsonValue::Num(7.0), at(b_time, 0), ReplicaId(2)).unwrap(),
                b.remove(&[key("o"), key("a")], at(b_time, 1), ReplicaId(2)).unwrap(),
            ];
            assert_eq!(settle(&base, &a, &da, &b, &db), doc(&[("o", obj(&[("a", 5.0)])), ("xs", nums(&[9.0]))]), "{b_time}");
        }
    }

    // A write into a number has to turn it into an object. A whole write or
    // a removal of that number made at the same time wins, even when it has
    // the earlier timestamp.
    #[test]
    fn a_fresh_start_loses_to_a_whole_write_or_removal_made_at_the_same_time() {
        let mut base = JsonCrdt::default();
        base.set(&[key("n")], &JsonValue::Num(5.0), at(1, 0), ReplicaId(3)).unwrap();
        for whole in [Some(obj(&[("b", 2.0)])), None] {
            let mut a = copy_of(&base);
            let mut b = copy_of(&base);
            let da = vec![a.set(&[key("n"), key("a")], &JsonValue::Num(1.0), at(9, 0), ReplicaId(1)).unwrap()];
            let db = vec![match &whole {
                Some(v) => b.set(&[key("n")], v, at(3, 0), ReplicaId(2)).unwrap(),
                None => b.remove(&[key("n")], at(3, 0), ReplicaId(2)).unwrap(),
            }];
            let expected = match whole { Some(v) => doc(&[("n", v)]), None => doc(&[]) };
            assert_eq!(settle(&base, &a, &da, &b, &db), expected);
        }
    }

    // A field created by a write into it loses to a whole write of the same
    // field made at the same time.
    #[test]
    fn a_field_created_by_a_write_inside_loses_to_a_whole_write() {
        let base = JsonCrdt::default();
        let mut a = copy_of(&base);
        let mut b = copy_of(&base);
        let da = vec![a.set(&[key("o"), key("b")], &JsonValue::Num(2.0), at(3, 0), ReplicaId(1)).unwrap()];
        let db = vec![b.set(&[key("o")], &obj(&[("old", 1.0)]), at(1, 0), ReplicaId(2)).unwrap()];
        assert_eq!(settle(&base, &a, &da, &b, &db), doc(&[("o", obj(&[("old", 1.0)]))]));
    }

    // A field inside a removed object is written into after the removal,
    // while another machine, which has not seen the removal, writes that
    // field whole into the old object with a later timestamp. The late
    // write is inside what was removed, so it is dropped, and it must not
    // hide the write made into the fresh start.
    #[test]
    fn a_late_write_into_a_removed_object_does_not_hide_its_fresh_start() {
        let mut base = JsonCrdt::default();
        base.set(&[key("p")], &JsonValue::Object([("n".to_string(), obj(&[("x", 1.0)]))].into()), at(1, 0), ReplicaId(3)).unwrap();
        let mut a = copy_of(&base);
        let mut b = copy_of(&base);
        let da = vec![
            a.remove(&[key("p")], at(5, 0), ReplicaId(1)).unwrap(),
            a.set(&[key("p"), key("n"), key("z")], &JsonValue::Num(1.0), at(6, 0), ReplicaId(1)).unwrap(),
        ];
        let db = vec![b.set(&[key("p"), key("n")], &obj(&[("y", 1.0)]), at(9, 0), ReplicaId(2)).unwrap()];
        assert_eq!(
            settle(&base, &a, &da, &b, &db),
            doc(&[("p", JsonValue::Object([("n".to_string(), obj(&[("z", 1.0)]))].into()))])
        );
    }

    #[test]
    fn removing_a_list_item_under_a_missing_key_does_nothing() {
        let mut n = JsonCrdt::default();
        n.set(&[key("keep")], &JsonValue::Num(1.0), hlc(1), ReplicaId(1)).unwrap();
        let before = n.clone();
        assert!(n.remove(&[key("ghost"), Seg::Index(0)], hlc(2), ReplicaId(1)).is_ok());
        assert!(n.remove(&[key("ghost"), key("b")], hlc(2), ReplicaId(1)).is_ok());
        assert_eq!(n, before);
    }

    #[test]
    fn insert_refuses_what_is_not_a_list() {
        let mut n = JsonCrdt::default();
        n.set(&[key("o")], &obj(&[("a", 1.0)]), hlc(1), ReplicaId(1)).unwrap();
        n.set(&[key("s")], &JsonValue::Str("t".into()), hlc(2), ReplicaId(1)).unwrap();
        n.set(&[key("n")], &JsonValue::Num(1.0), hlc(3), ReplicaId(1)).unwrap();
        n.set(&[key("b")], &JsonValue::Bool(false), hlc(4), ReplicaId(1)).unwrap();
        n.set(&[key("z")], &JsonValue::Null, hlc(5), ReplicaId(1)).unwrap();
        let before = n.clone();
        for k in ["o", "s", "n", "b"] {
            assert_eq!(n.insert(&[key(k)], 0, &JsonValue::Num(1.0), hlc(6), ReplicaId(1)), Err(JsonError::NotAList), "{k}");
        }
        assert_eq!(n.insert(&[], 0, &JsonValue::Num(1.0), hlc(6), ReplicaId(1)), Err(JsonError::NotAList));
        assert_eq!(n, before);
        // Null and nothing at all start a list.
        n.insert(&[key("z")], 0, &JsonValue::Num(1.0), hlc(7), ReplicaId(1)).unwrap();
        n.insert(&[key("new")], 0, &JsonValue::Num(2.0), hlc(8), ReplicaId(1)).unwrap();
        assert_eq!(n.get(&[key("z")]), Some(nums(&[1.0])));
        assert_eq!(n.get(&[key("new")]), Some(nums(&[2.0])));
    }

    #[test]
    fn large_floats_are_written_so_they_read_back() {
        for n in [1.5e300, -1.5e300, 1e20, 2f64.powi(60), 9007199254740994.0, f64::MAX, f64::MIN] {
            let text = JsonValue::Num(n).to_json_string();
            assert_eq!(JsonValue::from_json_str(&text), Ok(JsonValue::Num(n)), "{n} as {text}");
        }
        assert_eq!(JsonValue::Num(9007199254740992.0).to_json_string(), "9007199254740992");
        assert_eq!(JsonValue::Num(-3.0).to_json_string(), "-3");
    }

    #[test]
    fn whole_numbers_a_float_cannot_hold_exactly_are_refused() {
        for ok in ["9007199254740992", "-9007199254740992", "1e300", "9007199254740993.0", "1.5e300", "-0"] {
            assert!(JsonValue::from_json_str(ok).is_ok(), "{ok}");
        }
        // Past 2^53 a float holds some whole numbers exactly, and those are
        // read as they are written, in plain digits.
        for (text, n) in [
            ("10000000000000000", 1e16),
            ("1152921504606846976", 2f64.powi(60)),
            ("-1152921504606846976", -(2f64.powi(60))),
            ("9007199254740994", 9007199254740994.0),
            ("100000000000000000000", 1e20),
            ("340282366920938463463374607431768211456", 2f64.powi(128)),
        ] {
            assert_eq!(JsonValue::from_json_str(text), Ok(JsonValue::Num(n)), "{text}");
        }
        let two_to_the_1000 = "10715086071862673209484250490600018105614048117055336074437503883703510511249361224931983788156958581275946729175531468251871452856923140435984577574698574803934567774824230985421074605062371141877954182153046474983581941267398767559165543946077062914571196477686542167660429831652624386837205668069376";
        assert_eq!(JsonValue::from_json_str(two_to_the_1000), Ok(JsonValue::Num(2f64.powi(1000))));
        let too_far = format!("1{}", "0".repeat(400));
        for bad in [
            "9007199254740993", "-9007199254740993", "1152921504606846977", "123456789012345678901234567890",
            "[1,9007199254740993]", "1152921504606847000", too_far.as_str(),
        ] {
            let err = JsonValue::from_json_str(bad).unwrap_err();
            assert!(err.0.contains("2^53"), "{bad}: {err}");
        }
    }

    // Roughly how much a delta holds: one for the node, one for each field
    // dot and each tombstone, and the same again for every child it carries.
    fn weight(d: &JsonDelta) -> usize {
        let mut n = 1;
        if let Some(o) = d.object() {
            n += o.dots().map(|(_, dots)| dots.len()).sum::<usize>();
            n += o.values().map(|(_, v)| weight(v)).sum::<usize>();
        }
        if let Some(a) = d.array() {
            n += a.tombstones().count();
            n += a.elements().map(|(_, el)| weight(&el.value.snapshot())).sum::<usize>();
        }
        n
    }

    #[test]
    fn rewriting_a_value_again_and_again_sends_no_more_each_time() {
        let inner = JsonValue::Object([("deep".to_string(), nums(&[1.0, 2.0]))].into());
        let mut whole = obj(&[("a", 1.0), ("b", 2.0)]);
        if let JsonValue::Object(m) = &mut whole { m.insert("c".into(), inner); }
        let list = JsonValue::Array(vec![whole.clone(), JsonValue::Num(3.0)]);
        // Each case writes the same thing 200 times. The last write must send
        // no more than the first.
        type Write<'a> = Box<dyn Fn(&mut JsonCrdt, Hlc) -> JsonDelta + 'a>;
        let cases: Vec<(&str, Write)> = vec![
            ("object", Box::new(|n, t| n.set(&[key("cfg")], &whole, t, ReplicaId(1)).unwrap())),
            ("list", Box::new(|n, t| n.set(&[key("xs")], &list, t, ReplicaId(1)).unwrap())),
            ("root", Box::new(|n, t| n.set(&[], &whole, t, ReplicaId(1)).unwrap())),
            ("field inside", Box::new(|n, t| n.set(&[key("cfg"), key("a")], &whole, t, ReplicaId(1)).unwrap())),
            ("list item", Box::new(|n, t| n.set(&[key("ys"), Seg::Index(0)], &whole, t, ReplicaId(1)).unwrap())),
        ];
        for (what, write) in cases {
            let mut n = JsonCrdt::default();
            let mut other = JsonCrdt::default();
            let mut sizes = Vec::new();
            other.merge(n.set(&[key("ys")], &nums(&[1.0, 2.0]), hlc(0), ReplicaId(1)).unwrap());
            for i in 1..=200 {
                let d = write(&mut n, hlc(i));
                sizes.push(weight(&d));
                other.merge(d);
            }
            assert_eq!(other.to_value(), n.to_value(), "{what}: the deltas alone carry the document");
            assert!(sizes[199] <= sizes[1], "{what}: write 2 sent {}, write 200 sent {}", sizes[1], sizes[199]);
        }
    }

    #[test]
    fn a_generation_with_no_step_left_is_refused_not_lost() {
        // A node whose generation has used up every step cannot start afresh.
        // The write is refused, and nothing changes. The last step a message
        // may carry is the one checked, as that is what another machine can
        // send: a write that went one step further would be refused by every
        // machine it was sent to.
        let worn = Generation { ts: hlc(1), writer: ReplicaId(2), step: MAX_GENERATION_STEP };
        let mut n = JsonCrdt::default();
        n.merge(JsonDelta {
            tag: Some(LwwDelta::from_parts(Some(JsonTag { generation: worn, shape: Shape::Scalar, parent: Generation::default() }), hlc(1), ReplicaId(2))),
            ..JsonDelta::default()
        });
        let before = n.clone();
        assert_eq!(n.set(&[key("a")], &JsonValue::Num(1.0), hlc(2), ReplicaId(1)), Err(JsonError::NoFreshStart));
        assert_eq!(n.insert(&[], 0, &JsonValue::Num(1.0), hlc(2), ReplicaId(1)), Err(JsonError::NoFreshStart));
        assert_eq!(n, before);
        // A whole write still works: it starts its own generation.
        assert!(n.set(&[], &obj(&[("a", 1.0)]), hlc(3), ReplicaId(1)).is_ok());
        assert_eq!(n.get(&[key("a")]), Some(JsonValue::Num(1.0)));
    }

    // Below 1e21, a whole number is written in plain digits, so that a reader
    // that keeps whole numbers apart from floats, as Python does, reads it
    // back as a whole number. Negative zero is written as `-0.0`, so that
    // such a reader keeps its sign.
    #[test]
    fn whole_floats_below_1e21_are_written_in_plain_digits() {
        for (n, text) in [
            (2f64.powi(60), "1152921504606846976"),
            (-(2f64.powi(60)), "-1152921504606846976"),
            (1e16, "10000000000000000"),
            (9007199254740994.0, "9007199254740994"),
            (1e20, "100000000000000000000"),
            (999999999999999868928.0, "999999999999999868928"),
            (-0.0, "-0.0"),
            (0.0, "0"),
        ] {
            assert_eq!(JsonValue::Num(n).to_json_string(), text);
            let back = JsonValue::from_json_str(text).unwrap();
            assert!(matches!(back, JsonValue::Num(b) if b.to_bits() == n.to_bits()), "{text}");
        }
        // From 1e21 on the exponent stays, as it is shorter.
        assert_eq!(JsonValue::Num(1e21).to_json_string(), "1e21");
        assert_eq!(JsonValue::Num(1e22).to_json_string(), "1e22");
        assert_eq!(JsonValue::Num(1.5e300).to_json_string(), "1.5e300");
    }

    // A write into an object or a list with no fresh dot left for this
    // machine is refused before anything changes.
    #[test]
    fn a_write_with_no_fresh_dot_is_refused_and_changes_nothing() {
        let r = ReplicaId(1);
        let key = |k: &str| Seg::Key(k.to_string());
        let mut doc = JsonCrdt::default();
        doc.set(&[key("list")], &JsonValue::Array(vec![]), hlc(1), ReplicaId(2)).unwrap();
        let full = crate::context::seen_up_to(r, crate::MAX_COUNTER);
        // The root object, and the list inside it, have both seen this
        // machine's last dot.
        let list = doc.child("list").unwrap().clone();
        let mut list_full = list.clone();
        list_full.merge(JsonDelta::from_parts(None, None, Some(RgaDelta::from_parts(BTreeMap::new(), BTreeSet::new(), full.clone())), None));
        let before = doc.clone();
        let mut root_full = doc.clone();
        root_full.merge(JsonDelta::from_parts(None, Some(OrMapDelta::from_parts(BTreeMap::new(), BTreeMap::new(), full)), None, None));
        let mut d = root_full.clone();
        assert_eq!(d.set(&[key("x")], &JsonValue::Num(1.0), hlc(2), r), Err(JsonError::Write(WriteError::NoFreshDot)));
        assert_eq!(d, root_full);
        let obj = JsonValue::Object(BTreeMap::from([("y".to_string(), JsonValue::Num(1.0))]));
        assert_eq!(d.set(&[], &obj, hlc(2), r), Err(JsonError::Write(WriteError::NoFreshDot)));
        assert_eq!(d, root_full);
        // Another machine can still write.
        assert!(d.set(&[key("x")], &JsonValue::Num(1.0), hlc(2), ReplicaId(3)).is_ok());

        // A list that has seen this machine's last dot refuses an insert.
        let mut d = before.clone();
        let generation = d.tag_now().generation;
        d.merge(JsonDelta::from_parts(None, Some(OrMapDelta::from_parts(
            BTreeMap::new(),
            BTreeMap::from([((generation, "list".to_string()), list_full.snapshot())]),
            DotContext::new(),
        )), None, None));
        let held = d.clone();
        assert_eq!(d.insert(&[key("list")], 0, &JsonValue::Num(1.0), hlc(3), r), Err(JsonError::Write(WriteError::NoFreshDot)));
        assert_eq!(d, held);
        assert_eq!(d.set(&[key("list"), Seg::Index(0)], &JsonValue::Num(1.0), hlc(3), r), Err(JsonError::Write(WriteError::NoFreshDot)));
        assert_eq!(d, held);
        assert!(d.insert(&[key("list")], 0, &JsonValue::Num(1.0), hlc(3), ReplicaId(3)).is_ok());
    }
}
