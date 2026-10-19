//! Text with formatting that several machines can edit separately and merge.
//!
//! A document is two pieces side by side. The text is an `Rga<String>` list
//! with one character (one Unicode code point) per element. The formatting is
//! a map of "marks" such as bold, an `OrMap<String, LwwRegister<String>>`. It
//! has one entry per character and attribute, stored under the key
//! `"<dot>|<attr>"`, where the dot is the permanent id the list gave that
//! character.
//!
//! Each mark is a last-writer-wins (LWW) register. When two machines set the
//! same mark on the same character at the same time, the write with the later
//! timestamp wins on every machine.
//!
//! Marks are stored outside the text list. A character is never changed after
//! it is inserted, and formatting merges on its own. Merging a document means
//! merging the two pieces, and since each piece ends up the same on every
//! machine, so does the whole document.
//!
//! A mark is set when its value is not empty. The empty string `""` means
//! "not set", and writing it is how a mark is removed.
//!
//! Marks belong to single characters, not to ranges. A character inserted into
//! the middle of a bold range is not bold, whether it was typed at the same
//! time as the formatting or after it.

use crate::clock::Hlc;
use crate::context::DotContext;
use crate::id::{Dot, ReplicaId};
use crate::error::WriteError;
use crate::{DeltaCrdt, LwwDelta, LwwRegister, OrMap, OrMapDelta, Rga, RgaDelta};
use std::collections::{BTreeMap, BTreeSet};

/// A run of neighbouring visible characters that all have the same marks.
/// `marks` maps each attribute to its value and holds only marks that are set.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Span {
    pub text: String,
    pub marks: BTreeMap<String, String>,
}

/// A rich-text document: the list of characters, plus the marks on each one.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RichText {
    text: Rga<String>,
    marks: OrMap<String, LwwRegister<String>>,
}

/// A change to send to other machines. It carries a text change, a marks
/// change, or both. A piece that was not touched is `None`.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RichTextDelta {
    text: Option<RgaDelta<String>>,
    marks: Option<OrMapDelta<String, LwwRegister<String>>>,
}

impl RichTextDelta {
    /// Builds a delta from its two parts.
    pub fn from_parts(
        text: Option<RgaDelta<String>>,
        marks: Option<OrMapDelta<String, LwwRegister<String>>>,
    ) -> Self {
        Self { text, marks }
    }
    /// The text change in this delta, if it has one.
    pub fn text(&self) -> Option<&RgaDelta<String>> { self.text.as_ref() }
    /// The marks change in this delta, if it has one.
    pub fn marks(&self) -> Option<&OrMapDelta<String, LwwRegister<String>>> { self.marks.as_ref() }
}

fn dot_str(dot: Dot) -> String {
    format!("{}.{}", dot.replica.0, dot.counter)
}

fn mark_key(dot: Dot, attr: &str) -> String {
    format!("{}|{}", dot_str(dot), attr)
}

impl RichText {
    /// The visible text as one string.
    pub fn text(&self) -> String {
        self.text.to_vec().concat()
    }

    /// Inserts one character at visible position `index`. An index past the end
    /// inserts at the end. Refuses with `WriteError::NoFreshDot`, changing
    /// nothing, if the text has no dot left for `replica` (see `Rga::insert_after`).
    pub fn insert_char(&mut self, index: usize, ch: char, replica: ReplicaId) -> Result<RichTextDelta, WriteError> {
        let vis = self.text.visible_dots();
        let idx = index.min(vis.len());
        let pred = if idx == 0 { None } else { Some(vis[idx - 1]) };
        let ed = self.text.insert_after(pred, ch.to_string(), replica)?;
        Ok(RichTextDelta { text: Some(ed), marks: None })
    }

    /// Inserts the text `s` at visible position `index`, one character per
    /// Unicode code point, and returns a single delta for all of it. An index
    /// past the end inserts at the end. Each character is placed after the one
    /// before it, so the text stays in one piece whatever else is inserted
    /// around it.
    ///
    /// Refuses with `WriteError::NoFreshDot` if the text has fewer dots left
    /// for `replica` than `s` has characters. That is checked first, so a
    /// refused insert changes nothing.
    pub fn insert_str(&mut self, index: usize, s: &str, replica: ReplicaId) -> Result<RichTextDelta, WriteError> {
        if self.text.room(replica) < s.chars().count() as u64 {
            return Err(WriteError::NoFreshDot);
        }
        let vis = self.text.visible_dots();
        let idx = index.min(vis.len());
        let mut pred = if idx == 0 { None } else { Some(vis[idx - 1]) };
        let mut elements = BTreeMap::new();
        for ch in s.chars() {
            let one = self.text.insert_after(pred, ch.to_string(), replica)?;
            for (dot, el) in one.elements() {
                pred = Some(*dot);
                elements.insert(*dot, el.clone());
            }
        }
        let ed = RgaDelta::from_parts(elements, BTreeSet::new(), DotContext::new());
        Ok(RichTextDelta { text: Some(ed), marks: None })
    }

    /// Deletes `len` visible characters starting at `index`. A range that runs
    /// past the end stops at the end. Returns one delta that holds only the
    /// dots of the deleted characters.
    pub fn delete(&mut self, index: usize, len: usize, _replica: ReplicaId) -> RichTextDelta {
        let vis = self.text.visible_dots();
        let start = index.min(vis.len());
        let end = index.saturating_add(len).min(vis.len());
        let targets: BTreeSet<Dot> = vis[start..end].iter().copied().collect();
        for &t in &targets {
            self.text.tombstone(t);
        }
        let ed = RgaDelta::from_parts(BTreeMap::new(), targets, DotContext::new());
        RichTextDelta { text: Some(ed), marks: None }
    }

    /// Sets the mark `attr` to `value` on each visible character from `index`
    /// up to, but not including, `index + len`. A range that runs past the end
    /// stops at the end. An empty `value` removes the mark.
    ///
    /// Returns one marks delta that covers only the characters touched. It is
    /// put together from the per-character deltas the marks map hands back.
    ///
    /// Each character's mark takes one dot. If the marks have fewer dots left
    /// for `replica` than there are characters, this refuses with
    /// `WriteError::NoFreshDot` before changing anything.
    pub fn format(&mut self, index: usize, len: usize, attr: &str, value: &str, hlc: Hlc, replica: ReplicaId) -> Result<RichTextDelta, WriteError> {
        let vis = self.text.visible_dots();
        let start = index.min(vis.len());
        let end = index.saturating_add(len).min(vis.len());
        if self.marks.room(replica) < (end - start) as u64 {
            return Err(WriteError::NoFreshDot);
        }

        let md = self.mark(&vis[start..end], attr, value, hlc, replica)?;
        Ok(RichTextDelta { text: None, marks: Some(md) })
    }

    /// Sets the mark `attr` to `value` on each of the characters `dots`, and
    /// returns one marks delta for all of them. The caller has checked that
    /// the marks have a dot left for each.
    fn mark(&mut self, dots: &[Dot], attr: &str, value: &str, hlc: Hlc, replica: ReplicaId) -> Result<OrMapDelta<String, LwwRegister<String>>, WriteError> {
        let mut dots_acc: BTreeMap<String, BTreeSet<Dot>> = BTreeMap::new();
        let mut values_acc: BTreeMap<String, LwwDelta<String>> = BTreeMap::new();
        let mut ctx_acc = DotContext::new();

        for &d in dots {
            let key = mark_key(d, attr);
            let delta = self.marks.update(key, replica, |reg| reg.set(value.to_string(), hlc, replica))?;
            for (k, set) in delta.dots() {
                dots_acc.entry(k.clone()).or_default().extend(set.iter().copied());
            }
            for (k, vd) in delta.values() {
                values_acc.insert(k.clone(), vd.clone());
            }
            ctx_acc.merge(delta.context());
        }
        Ok(OrMapDelta::from_parts(dots_acc, values_acc, ctx_acc))
    }

    /// Inserts `s` at visible position `index`, as `insert_str` does, and
    /// sets the mark `attr` to `value` on exactly the characters it put in,
    /// all as one change. The marks are tied to the new characters' own ids
    /// and not to positions, so text another machine puts in at the same
    /// place never takes the mark, and the new text never goes without it.
    ///
    /// Refuses with `WriteError::NoFreshDot`, changing nothing, if the text
    /// or the marks have fewer dots left for `replica` than `s` has
    /// characters.
    pub fn insert_marked(&mut self, index: usize, s: &str, attr: &str, value: &str, hlc: Hlc, replica: ReplicaId) -> Result<RichTextDelta, WriteError> {
        let count = s.chars().count() as u64;
        if self.text.room(replica) < count || self.marks.room(replica) < count {
            return Err(WriteError::NoFreshDot);
        }
        let inserted = self.insert_str(index, s, replica)?;
        let dots: Vec<Dot> = inserted.text.iter().flat_map(|text| text.elements().map(|(dot, _)| *dot)).collect();
        let marks = self.mark(&dots, attr, value, hlc, replica)?;
        Ok(RichTextDelta { text: inserted.text, marks: Some(marks) })
    }

    /// Removes the mark `attr` from the same range `format` would cover. It
    /// does this by writing the empty string, so it is a last-writer-wins write
    /// like any other and a `format` with a later timestamp beats it.
    pub fn unformat(&mut self, index: usize, len: usize, attr: &str, hlc: Hlc, replica: ReplicaId) -> Result<RichTextDelta, WriteError> {
        self.format(index, len, attr, "", hlc, replica)
    }

    /// Returns the document as spans: the longest runs of neighbouring visible
    /// characters whose set marks are exactly the same. Marks with an empty
    /// value are left out.
    pub fn to_spans(&self) -> Vec<Span> {
        // Collect the marks that are set, grouped by the character's dot (as text).
        let mut by_dot: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for key in self.marks.keys() {
            if let Some(v) = self.marks.get(key).and_then(|reg| reg.get()) {
                if !v.is_empty() {
                    if let Some((ds, attr)) = key.split_once('|') {
                        by_dot.entry(ds.to_string()).or_default().insert(attr.to_string(), v.clone());
                    }
                }
            }
        }
        let vis = self.text.visible_dots();
        let chars = self.text.to_vec();
        let mut spans: Vec<Span> = Vec::new();
        for (i, dot) in vis.iter().enumerate() {
            let marks = by_dot.get(&dot_str(*dot)).cloned().unwrap_or_default();
            let ch = &chars[i];
            match spans.last_mut() {
                Some(last) if last.marks == marks => last.text.push_str(ch),
                _ => spans.push(Span { text: ch.clone(), marks }),
            }
        }
        spans
    }
}

impl DeltaCrdt for RichText {
    type Delta = RichTextDelta;

    fn merge(&mut self, delta: RichTextDelta) {
        if let Some(d) = delta.text { self.text.merge(d); }
        if let Some(d) = delta.marks { self.marks.merge(d); }
    }

    fn snapshot(&self) -> RichTextDelta {
        RichTextDelta {
            text: Some(self.text.snapshot()),
            marks: Some(self.marks.snapshot()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hlc(w: u64) -> Hlc { Hlc { wall: w, logical: 0 } }

    // Inserts the characters of `s` one after another, starting at `index`.
    fn insert_str(rt: &mut RichText, index: usize, s: &str, r: ReplicaId) {
        for (i, ch) in s.chars().enumerate() {
            rt.insert_char(index + i, ch, r).unwrap();
        }
    }

    #[test]
    fn text_reads_inserted_characters_in_order() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "helo", ReplicaId(1));
        rt.insert_char(2, 'l', ReplicaId(1)).unwrap(); // "hello"
        assert_eq!(rt.text(), "hello");
    }

    #[test]
    fn format_produces_a_single_uniform_span() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "abc", ReplicaId(1));
        rt.format(0, 3, "bold", "true", hlc(1), ReplicaId(1)).unwrap();
        let spans = rt.to_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "abc");
        assert_eq!(spans[0].marks.get("bold").map(String::as_str), Some("true"));
    }

    #[test]
    fn partial_format_splits_into_spans() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "abc", ReplicaId(1));
        rt.format(0, 1, "bold", "true", hlc(1), ReplicaId(1)).unwrap(); // only "a"
        let spans = rt.to_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "a");
        assert!(spans[0].marks.contains_key("bold"));
        assert_eq!(spans[1].text, "bc");
        assert!(spans[1].marks.is_empty());
    }

    #[test]
    fn concurrent_format_unformat_resolves_by_hlc() {
        let mut a = RichText::default();
        insert_str(&mut a, 0, "x", ReplicaId(1));
        let mut b = a.clone();
        let d_fmt = a.format(0, 1, "bold", "true", hlc(2), ReplicaId(1)).unwrap(); // later timestamp, wins
        let d_unf = b.unformat(0, 1, "bold", hlc(1), ReplicaId(2)).unwrap();       // earlier timestamp, loses
        a.merge(d_unf);
        b.merge(d_fmt);
        assert_eq!(a, b, "converged");
        assert_eq!(a.to_spans()[0].marks.get("bold").map(String::as_str), Some("true"));
    }

    #[test]
    fn distinct_attributes_coexist_on_one_char() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "x", ReplicaId(1));
        rt.format(0, 1, "bold", "true", hlc(1), ReplicaId(1)).unwrap();
        rt.format(0, 1, "italic", "true", hlc(2), ReplicaId(1)).unwrap();
        let m = &rt.to_spans()[0].marks;
        assert_eq!(m.get("bold").map(String::as_str), Some("true"));
        assert_eq!(m.get("italic").map(String::as_str), Some("true"));
    }

    #[test]
    fn delete_hides_text_and_its_marks() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "abc", ReplicaId(1));
        rt.format(0, 3, "bold", "true", hlc(1), ReplicaId(1)).unwrap();
        rt.delete(1, 1, ReplicaId(1)); // remove "b"
        assert_eq!(rt.text(), "ac");
        // The marks on "b" are still stored, but they never show up in the spans.
        let joined: String = rt.to_spans().iter().map(|s| s.text.clone()).collect();
        assert_eq!(joined, "ac");
    }

    #[test]
    fn insert_into_formatted_range_is_not_inherited() {
        // A known limit: a character inserted into a bold run is not bold.
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "ac", ReplicaId(1));
        rt.format(0, 2, "bold", "true", hlc(1), ReplicaId(1)).unwrap();
        rt.insert_char(1, 'b', ReplicaId(1)).unwrap(); // between a and c, unformatted
        let spans = rt.to_spans();
        assert_eq!(rt.text(), "abc");
        // a(bold) b(plain) c(bold) -> 3 spans
        assert_eq!(spans.len(), 3);
        assert!(spans[1].marks.is_empty(), "inserted char not inherited");
    }

    #[test]
    fn delta_from_parts_round_trips_via_accessors() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "a", ReplicaId(1));
        let d = rt.format(0, 1, "bold", "true", hlc(1), ReplicaId(1)).unwrap();
        let rebuilt = RichTextDelta::from_parts(d.text().cloned(), d.marks().cloned());
        assert_eq!(rebuilt, d);
    }

    #[test]
    fn insert_str_puts_a_whole_text_in_as_one_delta() {
        let mut a = RichText::default();
        insert_str(&mut a, 0, "ad", ReplicaId(1));
        let mut b = RichText::default();
        b.merge(a.snapshot());

        let d = a.insert_str(1, "bc", ReplicaId(1)).unwrap();
        assert_eq!(a.text(), "abcd");
        b.merge(d);
        assert_eq!(b.text(), "abcd", "the one delta carries every character");

        // Past the end means at the end, and an empty text changes nothing.
        a.insert_str(99, "!", ReplicaId(1)).unwrap();
        a.insert_str(0, "", ReplicaId(1)).unwrap();
        assert_eq!(a.text(), "abcd!");
    }

    #[test]
    fn typing_by_another_machine_lands_where_asked() {
        let mut a = RichText::default();
        insert_str(&mut a, 0, "abc", ReplicaId(1));
        let mut b = RichText::default();
        b.merge(a.snapshot());
        let d = b.insert_char(1, 'X', ReplicaId(2)).unwrap();
        a.merge(d);
        assert_eq!(b.text(), "aXbc");
        assert_eq!(a.text(), "aXbc");
    }

    #[test]
    fn delete_with_a_huge_length_stops_at_the_end() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "abc", ReplicaId(1));
        rt.delete(1, usize::MAX, ReplicaId(1));
        assert_eq!(rt.text(), "a");
    }

    #[test]
    fn format_with_a_huge_length_stops_at_the_end() {
        let mut rt = RichText::default();
        insert_str(&mut rt, 0, "abc", ReplicaId(1));
        rt.format(1, usize::MAX, "bold", "true", hlc(1), ReplicaId(1)).unwrap();
        let spans = rt.to_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].text, "bc");
        assert!(spans[1].marks.contains_key("bold"));
    }

    // Text with room for one more character refuses a two-character insert
    // whole, rather than put in the first character and lose the second.
    #[test]
    fn an_insert_that_runs_out_of_dots_is_refused_whole() {
        let r = ReplicaId(1);
        let mut rt = RichText::default();
        let near = RgaDelta::from_parts(BTreeMap::new(), BTreeSet::new(), crate::context::seen_up_to(r, crate::MAX_COUNTER - 1));
        rt.merge(RichTextDelta::from_parts(Some(near), None));
        let before = rt.clone();
        assert_eq!(rt.insert_str(0, "ab", r), Err(WriteError::NoFreshDot));
        assert_eq!(rt, before);
        rt.insert_str(0, "a", r).unwrap();
        assert_eq!(rt.text(), "a");
        assert_eq!(rt.insert_char(1, 'b', r), Err(WriteError::NoFreshDot));
    }

    // The same for formatting: each character's mark takes a dot.
    #[test]
    fn a_format_that_runs_out_of_dots_is_refused_whole() {
        let r = ReplicaId(1);
        let mut rt = RichText::default();
        rt.insert_str(0, "ab", ReplicaId(2)).unwrap();
        let near = OrMapDelta::from_parts(BTreeMap::new(), BTreeMap::new(), crate::context::seen_up_to(r, crate::MAX_COUNTER - 1));
        rt.merge(RichTextDelta::from_parts(None, Some(near)));
        let before = rt.clone();
        assert_eq!(rt.format(0, 2, "bold", "1", hlc(1), r), Err(WriteError::NoFreshDot));
        assert_eq!(rt, before);
        rt.format(0, 1, "bold", "1", hlc(1), r).unwrap();
        assert_eq!(rt.format(1, 1, "bold", "1", hlc(2), r), Err(WriteError::NoFreshDot));
    }

    // Text put in together with its mark keeps the mark on exactly its own
    // characters, whatever another machine puts in at the same place.
    #[test]
    fn text_inserted_with_a_mark_keeps_the_mark_on_itself() {
        let (a, b) = (ReplicaId(1), ReplicaId(2));
        let mut one = RichText::default();
        let base = one.insert_str(0, "ab", a).unwrap();
        let mut two = RichText::default();
        two.merge(base);
        let mine = one.insert_marked(2, "new", "by", "alice", hlc(1), a).unwrap();
        let theirs = two.insert_str(2, "XY", b).unwrap();
        one.merge(theirs);
        two.merge(mine);
        assert_eq!(one, two);
        let marked: String = one
            .to_spans()
            .into_iter()
            .filter(|span| span.marks.get("by").map(String::as_str) == Some("alice"))
            .map(|span| span.text)
            .collect();
        assert_eq!(marked, "new");
        assert!(one.text().contains("new"), "{}", one.text());
    }

    // An insert whose marks would run out of dots is refused whole: neither
    // the text nor any mark goes in.
    #[test]
    fn an_insert_with_a_mark_is_refused_whole() {
        let r = ReplicaId(1);
        let mut rt = RichText::default();
        let near = OrMapDelta::from_parts(BTreeMap::new(), BTreeMap::new(), crate::context::seen_up_to(r, crate::MAX_COUNTER - 1));
        rt.merge(RichTextDelta::from_parts(None, Some(near)));
        let before = rt.clone();
        assert_eq!(rt.insert_marked(0, "ab", "by", "x", hlc(1), r), Err(WriteError::NoFreshDot));
        assert_eq!(rt, before);
    }
}
