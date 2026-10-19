//! A record of who changed what: one line for each change an agent makes
//! through this sidecar, and one for each thing it was not allowed to do.
//!
//! The record is the file `audit.log`, next to the agents file: in the data
//! folder of a local sidecar, and next to the settings file of one started
//! with `--config`. Each line is a JSON object, such as
//!
//! ```text
//! {"agent":"alice","change":"map_put","key":"deploy","object":"orvu/crew/notes","outcome":"done","time":"2026-10-15T14:03:12.345Z"}
//! ```
//!
//! It keeps the shape of a change, not what was written: the key of a map,
//! the item of a cart, the path in a JSON document, a node of a tree, a place
//! in a list or a text, and amounts, but no value, member of a set, message
//! or text. So it says that alice changed the note called deploy, and not
//! what the note says. Keys, items, paths and nodes are kept as they are,
//! so a program that keeps data in them, and not only names, finds that
//! data in the record. Reads are not recorded, except a read that was not
//! allowed. The reason the engine gave for a failure is not kept either, only
//! its code, since it can quote what was written.
//!
//! Each machine records what the programs connected to it do. A change that
//! comes from another machine is recorded there. The file is cut when it
//! passes 64 MiB: it becomes `audit.log.1`, the one before becomes
//! `audit.log.2`, and an older one is dropped.

use crate::pb;
use pb::request::Op;
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The name of the file.
pub const FILE: &str = "audit.log";

/// How large the file grows before it is cut.
const CUT_AT: u64 = 64 * 1024 * 1024;

/// How many lines can wait to be written. A program that changes things
/// faster than they can be written waits.
const WAITING: usize = 10_000;

/// The longest a label is kept. A path or a key longer than this is cut.
const LONGEST_LABEL: usize = 200;

/// Where the record is for a sidecar started in `mode`: next to its agents
/// file.
pub fn file_for(mode: &crate::cli::Mode, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    Ok(crate::agents::file_for(mode, env)?.with_file_name(FILE))
}

/// One change, before its outcome is known.
#[derive(Clone)]
pub struct Entry {
    line: Map<String, Value>,
}

fn label(text: &str) -> Value {
    if text.len() <= LONGEST_LABEL {
        return Value::from(text);
    }
    let mut end = LONGEST_LABEL;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Value::from(format!("{}...", &text[..end]))
}

impl Entry {
    /// The change `op` makes, or `None` for an operation that only reads.
    pub fn of(op: &Op) -> Option<Entry> {
        let (change, labels): (&str, Vec<(&str, Value)>) = match op {
            Op::Declare(o) => ("declare", vec![("class", json!(o.object_class))]),
            Op::CounterIncrement(o) => ("counter_increment", vec![("n", json!(o.n))]),
            Op::CounterDecrement(o) => ("counter_decrement", vec![("n", json!(o.n))]),
            Op::RegisterSet(_) => ("register_set", vec![]),
            Op::SetAdd(_) => ("set_add", vec![]),
            Op::SetRemove(_) => ("set_remove", vec![]),
            Op::MapPut(o) => ("map_put", vec![("key", label(&o.key))]),
            Op::MapRemove(o) => ("map_remove", vec![("key", label(&o.key))]),
            Op::MvMapPut(o) => ("mv_map_put", vec![("key", label(&o.key))]),
            Op::MvMapRemove(o) => ("mv_map_remove", vec![("key", label(&o.key))]),
            Op::CartAdd(o) => ("cart_add", vec![("item", label(&o.item)), ("qty", json!(o.qty))]),
            Op::CartRemoveQty(o) => ("cart_remove_qty", vec![("item", label(&o.item)), ("qty", json!(o.qty))]),
            Op::CartRemoveItem(o) => ("cart_remove_item", vec![("item", label(&o.item))]),
            Op::ListInsert(o) => ("list_insert", vec![("index", json!(o.index))]),
            Op::ListDelete(o) => ("list_delete", vec![("index", json!(o.index))]),
            Op::JsonSet(o) => ("json_set", vec![("path", label(&o.path_json))]),
            Op::JsonRemove(o) => ("json_remove", vec![("path", label(&o.path_json))]),
            Op::JsonInsert(o) => ("json_insert", vec![("path", label(&o.path_json)), ("index", json!(o.index))]),
            Op::RichtextInsert(o) => {
                ("richtext_insert", vec![("index", json!(o.index)), ("len", json!(o.text.chars().count()))])
            }
            Op::RichtextInsertMarked(o) => (
                "richtext_insert_marked",
                vec![("index", json!(o.index)), ("len", json!(o.text.chars().count())), ("attr", label(&o.attr))],
            ),
            Op::RichtextDelete(o) => ("richtext_delete", vec![("index", json!(o.index)), ("len", json!(o.len))]),
            Op::RichtextFormat(o) => (
                "richtext_format",
                vec![("index", json!(o.index)), ("len", json!(o.len)), ("attr", label(&o.attr))],
            ),
            Op::RichtextUnformat(o) => (
                "richtext_unformat",
                vec![("index", json!(o.index)), ("len", json!(o.len)), ("attr", label(&o.attr))],
            ),
            Op::TreeMoveNode(o) => ("tree_move_node", vec![("node", label(&o.child)), ("parent", label(&o.new_parent))]),
            Op::TreeMoveNodeAt(o) => (
                "tree_move_node_at",
                vec![("node", label(&o.child)), ("parent", label(&o.new_parent)), ("index", json!(o.index))],
            ),
            Op::TreeSetLabel(o) => ("tree_set_label", vec![("node", label(&o.node))]),
            Op::BoundedIncrement(o) => ("bounded_increment", vec![("n", json!(o.n))]),
            Op::BoundedTryDecrement(o) => ("bounded_try_decrement", vec![("n", json!(o.n))]),
            Op::BoundedTransfer(o) => ("bounded_transfer", vec![("to_replica", json!(o.to_replica)), ("n", json!(o.n))]),
            _ => return None,
        };
        let mut line = Map::new();
        line.insert("change".into(), change.into());
        line.insert("object".into(), label(crate::rules::object(op)));
        for (name, value) in labels {
            line.insert(name.into(), value);
        }
        Some(Entry { line })
    }

    /// A call that was not about one change, such as opening a session or a
    /// watch, on `object` if it names one.
    pub fn call(change: &str, object: Option<&str>) -> Entry {
        let mut line = Map::new();
        line.insert("change".into(), change.into());
        if let Some(object) = object {
            line.insert("object".into(), label(object));
        }
        Entry { line }
    }
}

/// How a change ended.
pub enum Outcome<'a> {
    /// The engine's answer.
    Answered(&'a pb::Response),
    /// The agent was not allowed to make it. `why` is what it was told.
    NotAllowed(&'a str),
    /// The sidecar stopped working on it partway, because of a fault of its
    /// own. Whether the change was made is not known.
    Broke,
}

/// The record a running sidecar keeps. Lines are handed to a thread of its
/// own, which writes them, so a call does not wait for the disk unless very
/// many are waiting.
pub struct Audit {
    lines: std::sync::Mutex<Option<tokio::sync::mpsc::Sender<String>>>,
    writer: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Audit {
    /// Keeps no record. For tests and for programs that embed the service.
    pub fn off() -> Self {
        Self { lines: std::sync::Mutex::new(None), writer: std::sync::Mutex::new(None) }
    }

    /// Keeps the record in the file at `path`, which is made if it is not
    /// there. Only its owner can read it.
    pub fn to_file(path: PathBuf) -> std::io::Result<Self> {
        let file = open(&path)?;
        let (lines, mut waiting) = tokio::sync::mpsc::channel::<String>(WAITING);
        let writer = std::thread::Builder::new().name("audit".into()).spawn(move || {
            let mut out = std::io::BufWriter::new(file);
            let mut written = out.get_ref().metadata().map(|found| found.len()).unwrap_or(0);
            while let Some(line) = waiting.blocking_recv() {
                let mut next = Some(line);
                // Every line waiting now is written, and then they go to
                // the disk together.
                while let Some(line) = next {
                    if written >= CUT_AT {
                        let _ = out.flush();
                        match cut(&path) {
                            Ok(file) => out = std::io::BufWriter::new(file),
                            // The file goes on being written, wherever it now
                            // is, and the next try is 64 MiB later. Trying on
                            // every line would move the older files round and
                            // round.
                            Err(e) => tracing::warn!(error = %e, "cannot start a new audit file"),
                        }
                        written = 0;
                    }
                    if let Err(e) = out.write_all(line.as_bytes()).and_then(|()| out.write_all(b"\n")) {
                        tracing::warn!(error = %e, "cannot write to the audit file");
                    }
                    written += line.len() as u64 + 1;
                    next = waiting.try_recv().ok();
                }
                if let Err(e) = out.flush() {
                    tracing::warn!(error = %e, "cannot write to the audit file");
                }
            }
        })?;
        Ok(Self { lines: std::sync::Mutex::new(Some(lines)), writer: std::sync::Mutex::new(Some(writer)) })
    }

    /// Records `entry`, made by `agent` (`None` if no key was given), and how
    /// it ended.
    pub async fn record(&self, entry: Entry, agent: Option<&str>, outcome: Outcome<'_>) {
        let Some(lines) = self.lines.lock().expect("audit lock").clone() else {
            return;
        };
        let mut line = Map::new();
        line.insert("time".into(), now().into());
        line.insert("agent".into(), agent.map_or(Value::Null, Value::from));
        line.extend(entry.line);
        let (outcome, why): (&str, Option<String>) = match outcome {
            Outcome::Answered(response) => match &response.result {
                Some(pb::response::Result::Refused(refused)) => ("refused", Some(refused.detail.clone())),
                Some(pb::response::Result::Failed(failed)) => {
                    let code = pb::Code::try_from(failed.code).map_or("CODE_UNKNOWN", |code| code.as_str_name());
                    ("failed", Some(code.to_string()))
                }
                _ => ("done", None),
            },
            Outcome::NotAllowed(why) => ("not allowed", Some(why.to_string())),
            Outcome::Broke => ("failed", Some("the sidecar stopped working on it partway".to_string())),
        };
        line.insert("outcome".into(), outcome.into());
        if let Some(why) = why {
            line.insert("why".into(), label(&why));
        }
        let _ = lines.send(Value::Object(line).to_string()).await;
    }

    /// Records `entry` without waiting, from code that cannot wait.
    pub fn record_soon(self: &std::sync::Arc<Self>, entry: Entry, agent: Option<String>, why: String) {
        let audit = self.clone();
        tokio::spawn(async move { audit.record(entry, agent.as_deref(), Outcome::NotAllowed(&why)).await });
    }

    /// Writes every line that is waiting, and stops. Called when the sidecar
    /// stops.
    pub fn close(&self) {
        self.lines.lock().expect("audit lock").take();
        if let Some(writer) = self.writer.lock().expect("audit lock").take() {
            let _ = writer.join();
        }
    }
}

fn open(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path)
}

fn older(path: &Path, number: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{number}"));
    PathBuf::from(name)
}

/// Starts a new file: the full one becomes `.1`, and `.1` becomes `.2`.
fn cut(path: &Path) -> std::io::Result<std::fs::File> {
    if older(path, 1).exists() {
        std::fs::rename(older(path, 1), older(path, 2))?;
    }
    std::fs::rename(path, older(path, 1))?;
    open(path)
}

/// The time now, as `2026-10-15T14:03:12.345Z`.
fn now() -> String {
    time_text(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default())
}

/// A time since 1970 as `2026-10-15T14:03:12.345Z`.
fn time_text(since: Duration) -> String {
    let seconds = since.as_secs();
    let (days, of_day) = ((seconds / 86_400) as i64, seconds % 86_400);
    // The date of a day count, by Howard Hinnant's method for the Gregorian
    // calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let of_era = z - era * 146_097;
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let day_of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 { month_part + 3 } else { month_part - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        since.subsec_millis()
    )
}

/// Which lines `orvud audit` shows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only this agent's. `-` is the changes made with no key.
    pub agent: Option<String>,
    /// Only objects whose names start with this.
    pub object: Option<String>,
    /// Only lines from this long ago or later.
    pub since: Option<Duration>,
    /// Only what was not allowed.
    pub not_allowed: bool,
}

/// Reads a length of time such as `30s`, `15m`, `2h` or `7d`.
pub fn duration(text: &str) -> Result<Duration, String> {
    let wrong = || format!("{text:?} is not a length of time: write a number and s, m, h or d, such as 2h");
    let unit = text.chars().last().ok_or_else(wrong)?;
    let number = &text[..text.len() - unit.len_utf8()];
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(wrong());
    }
    let number: u64 = number.parse().map_err(|_| wrong())?;
    let seconds = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(wrong()),
    };
    Ok(Duration::from_secs(number.saturating_mul(seconds)))
}

/// The lines of the record at `path` that `filter` lets through, oldest
/// first, from the older files too.
pub fn read(path: &Path, filter: &Filter) -> Result<Vec<Value>, String> {
    let mut found = Vec::new();
    each(path, filter, |line| found.push(line))?;
    Ok(found)
}

/// Hands each line `read` would give to `then`, one at a time, so that a
/// large record is not held in memory whole.
pub fn each(path: &Path, filter: &Filter, mut then: impl FnMut(Value)) -> Result<(), String> {
    let since = filter
        .since
        .map(|since| time_text(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().saturating_sub(since)));
    for name in [older(path, 2), older(path, 1), path.to_path_buf()] {
        let wrong = |e: std::io::Error| format!("cannot read the audit file {}: {e}", name.display());
        let file = match std::fs::File::open(&name) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(wrong(e)),
        };
        for line in std::io::BufReader::new(file).lines() {
            let line = line.map_err(wrong)?;
            // A line cut short by a sidecar that was killed while writing it
            // is passed over.
            let Ok(Value::Object(line)) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let text = |name: &str| line.get(name).and_then(Value::as_str);
            let agent = text("agent").unwrap_or("-");
            if filter.agent.as_deref().is_some_and(|wanted| wanted != agent)
                || filter.object.as_deref().is_some_and(|start| !text("object").unwrap_or("").starts_with(start))
                || since.as_deref().is_some_and(|since| text("time").unwrap_or("") < since)
                || (filter.not_allowed && text("outcome") != Some("not allowed"))
            {
                continue;
            }
            then(Value::Object(line));
        }
    }
    Ok(())
}

/// Text from the record as it can be shown on a terminal: a line break, or a
/// code that moves the cursor, is written out as an escape and not obeyed.
fn plain(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { c.escape_default().to_string() } else { c.to_string() }).collect()
}

/// One line of the record for a person to read, such as
/// `2026-10-15 14:03:12  alice  map_put  orvu/crew/notes  key=deploy  done`.
pub fn shown(line: &Value) -> String {
    let text = |name: &str| plain(line.get(name).and_then(Value::as_str).unwrap_or(""));
    let time = text("time").replace('T', " ");
    let time = time.get(..19).unwrap_or(&time);
    let agent = line.get("agent").and_then(Value::as_str).map_or_else(|| "-".to_string(), plain);
    let mut out = format!("{time}  {agent}  {}", text("change"));
    if !text("object").is_empty() {
        out.push_str(&format!("  {}", text("object")));
    }
    if let Some(fields) = line.as_object() {
        for (name, value) in fields {
            if ["time", "agent", "change", "object", "outcome", "why"].contains(&name.as_str()) {
                continue;
            }
            match value {
                Value::String(value) => out.push_str(&format!("  {}={}", plain(name), plain(value))),
                other => out.push_str(&format!("  {}={other}", plain(name))),
            }
        }
    }
    out.push_str(&format!("  {}", text("outcome")));
    if !text("why").is_empty() {
        out.push_str(&format!(": {}", text("why")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(object: &str, key: &str, value: &str) -> Op {
        Op::MapPut(pb::MapPut { object: object.into(), key: key.into(), value: value.into() })
    }

    fn done() -> pb::Response {
        pb::Response { result: Some(pb::response::Result::Ack(pb::Ack {})) }
    }

    async fn recorded(entries: Vec<(Entry, Option<&str>, Outcome<'_>)>) -> Vec<Value> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let audit = Audit::to_file(path.clone()).unwrap();
        for (entry, agent, outcome) in entries {
            audit.record(entry, agent, outcome).await;
        }
        audit.close();
        read(&path, &Filter::default()).unwrap()
    }

    #[tokio::test]
    async fn a_change_is_recorded_by_its_shape_and_not_its_value() {
        let lines = recorded(vec![(Entry::of(&put("orvu/crew/notes", "deploy", "Friday")).unwrap(), Some("alice"), Outcome::Answered(&done()))]).await;
        let [line] = lines.as_slice() else { panic!("{lines:?}") };
        assert_eq!(line["agent"], "alice");
        assert_eq!(line["change"], "map_put");
        assert_eq!(line["object"], "orvu/crew/notes");
        assert_eq!(line["key"], "deploy");
        assert_eq!(line["outcome"], "done");
        assert!(!line.to_string().contains("Friday"), "{line}");
        assert!(line["time"].as_str().unwrap().ends_with('Z'));
    }

    // A write to a map that keeps clashes is recorded with its key, as a
    // write to a map is.
    #[test]
    fn a_write_to_a_map_that_keeps_clashes_is_recorded() {
        let put = Entry::of(&Op::MvMapPut(pb::MvMapPut { object: "m".into(), key: "plan".into(), value: "v".into() })).unwrap();
        assert_eq!(put.line["change"], "mv_map_put");
        assert_eq!(put.line["key"], "plan");
        let removed = Entry::of(&Op::MvMapRemove(pb::MvMapRemove { object: "m".into(), key: "plan".into() })).unwrap();
        assert_eq!(removed.line["change"], "mv_map_remove");
        assert!(Entry::of(&Op::MvMapEntries(pb::MvMapEntries { object: "m".into() })).is_none());
    }

    // No text that was written is kept: not a register's value, a set's
    // member, a list's item, a JSON value or the text put into a document.
    #[test]
    fn nothing_written_is_kept() {
        let secret = "the launch code";
        for op in [
            Op::RegisterSet(pb::RegisterSet { object: "r".into(), value: secret.into() }),
            Op::SetAdd(pb::SetAdd { object: "s".into(), member: secret.into() }),
            Op::ListInsert(pb::ListInsert { object: "l".into(), index: 0, value: secret.into() }),
            Op::JsonSet(pb::JsonSet { object: "j".into(), path_json: "[\"a\"]".into(), value_json: format!("\"{secret}\"") }),
            Op::JsonInsert(pb::JsonInsert { object: "j".into(), path_json: "[]".into(), index: 0, value_json: format!("\"{secret}\"") }),
            Op::RichtextInsert(pb::RichtextInsert { object: "d".into(), index: 0, text: secret.into() }),
            Op::RichtextFormat(pb::RichtextFormat { object: "d".into(), index: 0, len: 3, attr: "by".into(), value: secret.into() }),
            Op::MapPut(pb::MapPut { object: "m".into(), key: "k".into(), value: secret.into() }),
            Op::MvMapPut(pb::MvMapPut { object: "m".into(), key: "k".into(), value: secret.into() }),
            Op::TreeSetLabel(pb::TreeSetLabel { object: "t".into(), node: "n".into(), label: secret.into() }),
        ] {
            let line = Value::Object(Entry::of(&op).unwrap().line).to_string();
            assert!(!line.contains(secret), "{line}");
        }
    }

    #[test]
    fn reads_are_not_recorded() {
        assert!(Entry::of(&Op::MapGet(pb::MapGet { object: "m".into(), key: "k".into() })).is_none());
        assert!(Entry::of(&Op::BoundedValue(pb::BoundedValue { object: "b".into() })).is_none());
    }

    #[tokio::test]
    async fn refusals_and_what_was_not_allowed_say_why() {
        let refused = pb::Response { result: Some(pb::response::Result::Refused(pb::Refused { detail: "exhausted".into() })) };
        let spend = Op::BoundedTryDecrement(pb::BoundedTryDecrement { object: "orvu/crew/budget/tokens".into(), n: 30 });
        let lines = recorded(vec![
            (Entry::of(&spend).unwrap(), Some("bob"), Outcome::Answered(&refused)),
            (Entry::call("open", None), None, Outcome::NotAllowed("this key is not one this sidecar knows")),
        ])
        .await;
        assert_eq!((lines[0]["outcome"].as_str(), lines[0]["why"].as_str(), lines[0]["n"].as_u64()), (Some("refused"), Some("exhausted"), Some(30)));
        assert_eq!(lines[1]["agent"], Value::Null);
        assert_eq!(lines[1]["outcome"], "not allowed");
        assert_eq!(shown(&lines[1])[19..], *"  -  open  not allowed: this key is not one this sidecar knows");
    }

    #[tokio::test]
    async fn a_line_is_shown_for_a_person_to_read() {
        let lines = recorded(vec![(Entry::of(&put("orvu/crew/notes", "deploy", "Friday")).unwrap(), Some("alice"), Outcome::Answered(&done()))]).await;
        assert_eq!(shown(&lines[0])[19..], *"  alice  map_put  orvu/crew/notes  key=deploy  done");
    }

    #[tokio::test]
    async fn lines_are_picked_by_agent_object_and_what_was_not_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let audit = Audit::to_file(path.clone()).unwrap();
        audit.record(Entry::of(&put("orvu/crew/notes", "a", "1")).unwrap(), Some("alice"), Outcome::Answered(&done())).await;
        audit.record(Entry::of(&put("orvu/crew/roster", "b", "1")).unwrap(), Some("bob"), Outcome::Answered(&done())).await;
        audit.record(Entry::of(&put("visits", "c", "1")).unwrap(), None, Outcome::NotAllowed("no")).await;
        audit.close();
        let picked = |filter: Filter| read(&path, &filter).unwrap().iter().map(|line| line["object"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(picked(Filter { agent: Some("bob".into()), ..Filter::default() }), ["orvu/crew/roster"]);
        assert_eq!(picked(Filter { agent: Some("-".into()), ..Filter::default() }), ["visits"]);
        assert_eq!(picked(Filter { object: Some("orvu/crew/".into()), ..Filter::default() }), ["orvu/crew/notes", "orvu/crew/roster"]);
        assert_eq!(picked(Filter { not_allowed: true, ..Filter::default() }), ["visits"]);
        assert_eq!(picked(Filter { since: Some(Duration::from_secs(3600)), ..Filter::default() }).len(), 3);
    }

    // A line cut short, by a sidecar killed while writing it, does not stop
    // the rest from being read.
    #[test]
    fn a_line_cut_short_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "{\"time\":\"2026-10-15T14:00:00.000Z\",\"agent\":\"a\",\"change\":\"map_put\"}\n{\"time\":\"20").unwrap();
        assert_eq!(read(&path, &Filter::default()).unwrap().len(), 1);
    }

    #[test]
    fn a_full_file_moves_aside_and_the_oldest_goes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        for (number, text) in ["one", "two", "three"].into_iter().enumerate() {
            if number > 0 {
                cut(&path).unwrap();
            }
            std::fs::write(&path, format!("{{\"time\":\"2026-10-15T14:00:0{number}.000Z\",\"change\":\"{text}\"}}\n")).unwrap();
        }
        let changes: Vec<_> = read(&path, &Filter::default()).unwrap().iter().map(|line| line["change"].as_str().unwrap().to_string()).collect();
        assert_eq!(changes, ["one", "two", "three"]);
        cut(&path).unwrap();
        let changes: Vec<_> = read(&path, &Filter::default()).unwrap().iter().map(|line| line["change"].as_str().unwrap().to_string()).collect();
        assert_eq!(changes, ["two", "three"]);
    }

    #[test]
    fn times_are_written_as_dates() {
        assert_eq!(time_text(Duration::from_millis(0)), "1970-01-01T00:00:00.000Z");
        assert_eq!(time_text(Duration::from_millis(1_792_073_000_123)), "2026-10-15T14:03:20.123Z");
        assert_eq!(time_text(Duration::from_secs(951_782_400)), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn lengths_of_time_are_read() {
        assert_eq!(duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(duration("7d"), Ok(Duration::from_secs(604_800)));
        for wrong in ["", "h", "2", "2w", "-1h", "1.5h", "+2h", "2é", "5ч"] {
            assert!(duration(wrong).is_err(), "{wrong}");
        }
    }

    // The cut falls in the middle of a two-byte letter, and moves back to
    // the start of it.
    #[test]
    fn a_long_label_is_cut() {
        let long = format!("a{}", "é".repeat(300));
        let cut = label(&long);
        assert!(cut.as_str().unwrap().ends_with("..."));
        assert!(cut.as_str().unwrap().len() <= LONGEST_LABEL + 3);
    }

    // What an agent wrote can hold line breaks and the codes that move a
    // terminal's cursor. Shown as they are, they could make a line look like
    // another agent's, or hide one.
    #[test]
    fn a_line_shown_cannot_pass_for_another() {
        let line = json!({"time": "2026-10-15T14:00:00.000Z", "agent": "mallory", "change": "map_put",
            "object": "orvu/crew/notes\u{1b}[2K", "key": "x  done\n2026-10-15 14:00:00  alice  map_put", "outcome": "done"});
        let out = shown(&line);
        assert!(!out.contains('\n') && !out.contains('\u{1b}'), "{out:?}");
        assert!(out.contains("\\n") && out.contains("\\u{1b}"), "{out:?}");
    }

    // A failure's reason from the engine can quote what was written. Only its
    // code is kept.
    #[tokio::test]
    async fn a_failure_keeps_its_code_and_not_its_reason() {
        let failed = pb::Response {
            result: Some(pb::response::Result::Failed(pb::Failed { code: pb::Code::Json as i32, detail: "bad JSON near the launch code".into() })),
        };
        let lines = recorded(vec![(Entry::of(&put("j", "k", "v")).unwrap(), None, Outcome::Answered(&failed))]).await;
        assert_eq!(lines[0]["why"], "CODE_JSON");
    }

    // Only the lines from the time asked for are shown.
    #[test]
    fn lines_are_picked_by_how_long_ago() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let recent = time_text(SystemTime::now().duration_since(UNIX_EPOCH).unwrap() - Duration::from_secs(60));
        std::fs::write(&path, format!("{{\"time\":\"2020-01-01T00:00:00.000Z\",\"change\":\"old\"}}\n{{\"time\":\"{recent}\",\"change\":\"new\"}}\n")).unwrap();
        let picked = read(&path, &Filter { since: Some(Duration::from_secs(3600)), ..Filter::default() }).unwrap();
        assert_eq!(picked.iter().map(|line| line["change"].as_str().unwrap()).collect::<Vec<_>>(), ["new"]);
    }

    // A file that cannot be read is named, whichever of the three it is.
    #[test]
    fn a_file_that_cannot_be_read_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::create_dir(older(&path, 2)).unwrap();
        let wrong = read(&path, &Filter::default()).unwrap_err();
        assert!(wrong.contains("audit.log.2"), "{wrong}");
    }
}
