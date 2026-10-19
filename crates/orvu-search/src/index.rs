//! The index: texts, their vectors, and finding the closest.

use crate::Model;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A text that was found.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// The object the text is in.
    pub object: String,
    /// Where in the object it is, as it was put in.
    pub part: String,
    pub text: String,
    /// How close its meaning is to the question's, from 0 to 1.
    pub score: f32,
}

struct Entry {
    part: String,
    text: String,
    vector: Vec<f32>,
}

/// The texts of a number of objects, each in parts, with their vectors.
#[derive(Default)]
pub struct Index {
    /// The texts of each object, by the object's name.
    objects: BTreeMap<String, Vec<Entry>>,
}

impl Index {
    /// Puts the texts of `object` in the index in place of those it had.
    /// Each is a part, which says where in the object the text is, and the
    /// text. A text the object had before keeps its vector, so putting an
    /// object again after a small change is quick.
    pub fn put(&mut self, object: &str, parts: Vec<(String, String)>, model: &Model) {
        let prepared = self.missing(object, &parts).prepare(model);
        self.put_prepared(object, parts, prepared);
    }

    /// Whether `object` has exactly these parts, with these texts, in this
    /// order, so that putting them would change nothing.
    pub fn holds(&self, object: &str, parts: &[(String, String)]) -> bool {
        let entries = self.objects.get(object).map(Vec::as_slice).unwrap_or_default();
        entries.len() == parts.len()
            && entries.iter().zip(parts).all(|(entry, (part, text))| &entry.part == part && &entry.text == text)
    }

    /// The texts among `parts` that `object` has no vector for. With
    /// [`Missing::prepare`] and [`Index::put_prepared`] it does what `put`
    /// does, with the vectors worked out apart from the index, so that
    /// nothing waits for them.
    pub fn missing(&self, object: &str, parts: &[(String, String)]) -> Missing {
        let entries = self.objects.get(object).map(Vec::as_slice).unwrap_or_default();
        let had: std::collections::HashSet<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
        let mut texts: Vec<String> = parts.iter().map(|(_, text)| text.clone()).filter(|text| !had.contains(text.as_str())).collect();
        texts.sort();
        texts.dedup();
        Missing(texts)
    }

    /// Puts `parts` in place of what `object` had: the vectors it had for
    /// their texts are moved over, and `prepared` gives the others.
    pub fn put_prepared(&mut self, object: &str, parts: Vec<(String, String)>, prepared: Prepared) {
        let mut vectors = prepared.0;
        for entry in self.objects.remove(object).unwrap_or_default() {
            vectors.entry(entry.text).or_insert(entry.vector);
        }
        let mut entries: Vec<Entry> = Vec::with_capacity(parts.len());
        let mut first: HashMap<String, usize> = HashMap::new();
        for (part, text) in parts {
            // A text an object has twice takes its vector from the first.
            let vector = match vectors.remove(&text) {
                Some(vector) => vector,
                None => first.get(&text).map(|&n| entries[n].vector.clone()).unwrap_or_default(),
            };
            first.entry(text.clone()).or_insert(entries.len());
            entries.push(Entry { part, text, vector });
        }
        self.objects.insert(object.to_string(), entries);
    }

    /// Takes `object` out of the index.
    pub fn remove(&mut self, object: &str) {
        self.objects.remove(object);
    }

    /// The objects the index holds texts of.
    pub fn objects(&self) -> BTreeSet<String> {
        self.objects.keys().cloned().collect()
    }

    /// How many texts the index holds.
    pub fn len(&self) -> usize {
        self.objects.values().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// At most `limit` texts whose meaning is closest to `question`'s, the
    /// closest first, from the objects `keep` lets through.
    pub fn search(&self, question: &str, model: &Model, limit: usize, keep: impl Fn(&str) -> bool) -> Vec<Found> {
        match model.vectors(&[question.to_string()]).pop() {
            Some(asked) => self.search_vector(&asked, limit, keep),
            None => Vec::new(),
        }
    }

    /// Like `search`, for a question whose vector was worked out already.
    /// `keep` is asked once for each object, and each part is found once,
    /// with the closest of its texts. A text with nothing at all in
    /// common with the question, scored 0, is not found; with a real model
    /// nearly every text has a little, so the last found may be far off.
    pub fn search_vector(&self, asked: &[f32], limit: usize, keep: impl Fn(&str) -> bool) -> Vec<Found> {
        let mut scored = Vec::new();
        for (object, entries) in &self.objects {
            if !keep(object) {
                continue;
            }
            // Each part once, with its closest text: a long text put in as
            // several pieces of one part does not crowd out the rest.
            let mut best: Vec<(f32, &Entry)> = Vec::new();
            let mut at: HashMap<&str, usize> = HashMap::new();
            for entry in entries {
                let score = entry.vector.iter().zip(asked).map(|(a, b)| a * b).sum::<f32>();
                // A score that is not a number is dropped too.
                if score.is_nan() || score <= 0.0 {
                    continue;
                }
                match at.get(entry.part.as_str()) {
                    Some(&n) if score > best[n].0 => best[n] = (score, entry),
                    Some(_) => {}
                    None => {
                        at.insert(&entry.part, best.len());
                        best.push((score, entry));
                    }
                }
            }
            scored.extend(best.into_iter().map(|(score, entry)| (score, object, entry)));
        }
        // The closest first; parts as close as each other in the order of
        // their objects' names and then of each part's first text, so the
        // same question gives the same answer. Only the closest `limit` are
        // put in order, and only those given back are copied.
        let closer = |a: &(usize, (f32, &String, &Entry)), b: &(usize, (f32, &String, &Entry))| {
            b.1 .0.total_cmp(&a.1 .0).then(a.0.cmp(&b.0))
        };
        let mut scored: Vec<(usize, (f32, &String, &Entry))> = scored.into_iter().enumerate().collect();
        if scored.len() > limit && limit > 0 {
            scored.select_nth_unstable_by(limit - 1, closer);
        }
        scored.truncate(limit);
        scored.sort_by(closer);
        scored
            .into_iter()
            .map(|(_, (score, object, entry))| Found {
                object: object.clone(),
                part: entry.part.clone(),
                text: entry.text.clone(),
                score: score.min(1.0),
            })
            .collect()
    }
}

/// Texts an object has no vector for, from [`Index::missing`].
pub struct Missing(Vec<String>);

/// Vectors worked out for texts, ready to be put in an index with
/// [`Index::put_prepared`].
pub struct Prepared(HashMap<String, Vec<f32>>);

impl Missing {
    /// The vectors of the texts, worked out by `model`.
    pub fn prepare(self, model: &Model) -> Prepared {
        let vectors = model.vectors(&self.0);
        Prepared(self.0.into_iter().zip(vectors).collect())
    }
}
