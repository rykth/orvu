//! What an agent may do, block by block.
//!
//! An agent with no rules may do everything, as every agent could before
//! there were rules. One with rules may do only what they allow, and its own
//! things are its own: it reads only its own inbox, and changes only its own
//! place on the roster and its own lines of what was spent from a budget and
//! who topped it up. The
//! names that say who wrote something are its own too: on a note, as the
//! sender of a message, and as the agent that added a task. Other names a
//! program writes, such as who holds a task, are what the program says; the
//! record of who changed what shows which agent made each change.
//!
//! Each block has a level: `read`, or `write`, which allows reading too.
//! Budgets have three: `read`, `spend`, and `top-up`, which allows spending.
//! Notes and tasks have `own` between `read` and `write`: the agent reads all
//! of them, and changes only its own. With `notes:own`, it adds simple notes
//! and changes or removes a note only when every value the note holds is its
//! own, and changes no structured note, as those keep no writer's name. With
//! `tasks:own`, it adds tasks as itself, claims an open task, which nobody
//! holds and is not done, and changes, finishes, gives back or moves only a
//! task it added or holds; a task another agent holds it does not touch, and
//! a firm task's tickets need `tasks:write`.
//!
//! Whose a note or a task is depends on the object as it is, not only on the
//! request, so for those `guards` reads the notes and tasks a request
//! changes before it runs, and turns the whole request away if any part of
//! it may not be made. Each write is then checked again in the same step in
//! the engine as it is made, so that a task another agent claimed, or a note
//! another agent wrote, after the first look is not changed.
//! The blocks are `notes`, `documents`, `tallies`, `tasks`, `budgets`,
//! `inbox`, `roster`, and `objects` for plain objects a program names itself.
//! A block a rule leaves out may not be used at all.
//!
//! The sidecar sees operations on objects, not calls of the blocks, so each
//! operation is put down to the block its object belongs to, by the object's
//! name. Every name a block uses is `orvu/<team>/<block>...`: see
//! `python/src/orvu/_names.py`. A name under `orvu/` that no block uses is
//! refused to an agent with rules.

use crate::pb;
use orvu_crdt::{parse_path, JsonValue, Seg};
use pb::request::Op;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// The blocks a rule can name, in the order they are written.
const BLOCKS: [&str; 8] = ["notes", "documents", "tallies", "tasks", "budgets", "inbox", "roster", "objects"];

/// The levels, lowest first: 1 is `read`, 2 is `own`, for notes and tasks
/// only, or `spend`, for budgets only, and 3 is `write`, or `top-up` for
/// budgets.
const READ: u8 = 1;
const OWN: u8 = 2;
const WRITE: u8 = 3;
const SPEND: u8 = 2;
const TOP_UP: u8 = 3;

/// What one agent may do. An empty set allows nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    levels: BTreeMap<&'static str, u8>,
}

impl Rules {
    /// Reads rules such as `["notes:write", "budgets:spend"]`. `none` alone
    /// allows nothing.
    pub fn parse<S: AsRef<str>>(words: &[S]) -> Result<Rules, String> {
        let mut rules = Rules::default();
        if let [only] = words {
            if only.as_ref() == "none" {
                return Ok(rules);
            }
        }
        if words.is_empty() {
            return Err("give at least one rule, such as notes:write, or none".into());
        }
        for word in words {
            let word = word.as_ref();
            let wrong = || {
                format!(
                    "{word:?} is not a rule. A rule is a block and a level, such as notes:read or tasks:write. \
                     The blocks are {}. Budgets take read, spend or top-up, and notes and tasks also own",
                    BLOCKS.join(", ")
                )
            };
            let (block, level) = word.split_once(':').ok_or_else(wrong)?;
            let block = *BLOCKS.iter().find(|known| **known == block).ok_or_else(wrong)?;
            let level = match (block, level) {
                (_, "read") => READ,
                ("budgets", "spend") => SPEND,
                ("budgets", "top-up") => TOP_UP,
                ("budgets", _) => {
                    return Err(format!("{word:?} is not a rule: budgets take read, spend or top-up, not {level}"))
                }
                (_, "write") => WRITE,
                ("notes" | "tasks", "own") => OWN,
                _ => return Err(wrong()),
            };
            if rules.levels.insert(block, level).is_some() {
                return Err(format!("{block} is given twice"));
            }
        }
        Ok(rules)
    }

    fn level(&self, block: &str) -> u8 {
        self.levels.get(block).copied().unwrap_or(0)
    }
}

impl std::fmt::Display for Rules {
    /// The rules as they are written, such as `notes:write budgets:spend`, or
    /// `none`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.levels.is_empty() {
            return write!(f, "none");
        }
        let words: Vec<String> = BLOCKS
            .iter()
            .filter_map(|block| {
                let level = self.levels.get(block)?;
                let name = match (*block, *level) {
                    (_, READ) => "read",
                    ("budgets", SPEND) => "spend",
                    ("budgets", _) => "top-up",
                    (_, OWN) => "own",
                    _ => "write",
                };
                Some(format!("{block}:{name}"))
            })
            .collect();
        write!(f, "{}", words.join(" "))
    }
}

/// The object an operation is on.
pub fn object(op: &Op) -> &str {
    match op {
        Op::Declare(o) => &o.object,
        Op::CounterIncrement(o) => &o.object,
        Op::CounterDecrement(o) => &o.object,
        Op::CounterValue(o) => &o.object,
        Op::RegisterSet(o) => &o.object,
        Op::RegisterGet(o) => &o.object,
        Op::SetAdd(o) => &o.object,
        Op::SetRemove(o) => &o.object,
        Op::SetContains(o) => &o.object,
        Op::SetElements(o) => &o.object,
        Op::MapPut(o) => &o.object,
        Op::MapRemove(o) => &o.object,
        Op::MapGet(o) => &o.object,
        Op::MapKeys(o) => &o.object,
        Op::MapEntries(o) => &o.object,
        Op::CartAdd(o) => &o.object,
        Op::CartRemoveQty(o) => &o.object,
        Op::CartRemoveItem(o) => &o.object,
        Op::CartQuantity(o) => &o.object,
        Op::CartItems(o) => &o.object,
        Op::ListInsert(o) => &o.object,
        Op::ListDelete(o) => &o.object,
        Op::ListGet(o) => &o.object,
        Op::ListLen(o) => &o.object,
        Op::ListToVec(o) => &o.object,
        Op::JsonSet(o) => &o.object,
        Op::JsonRemove(o) => &o.object,
        Op::JsonGet(o) => &o.object,
        Op::JsonToValue(o) => &o.object,
        Op::JsonInsert(o) => &o.object,
        Op::RichtextInsert(o) => &o.object,
        Op::RichtextDelete(o) => &o.object,
        Op::RichtextFormat(o) => &o.object,
        Op::RichtextUnformat(o) => &o.object,
        Op::RichtextText(o) => &o.object,
        Op::RichtextToSpans(o) => &o.object,
        Op::RichtextInsertMarked(o) => &o.object,
        Op::TreeMoveNode(o) => &o.object,
        Op::TreeMoveNodeAt(o) => &o.object,
        Op::TreeSetLabel(o) => &o.object,
        Op::TreeChildren(o) => &o.object,
        Op::TreeToNodes(o) => &o.object,
        Op::TreeParent(o) => &o.object,
        Op::BoundedIncrement(o) => &o.object,
        Op::BoundedTryDecrement(o) => &o.object,
        Op::BoundedTransfer(o) => &o.object,
        Op::BoundedValue(o) => &o.object,
        Op::BoundedAllowance(o) => &o.object,
        Op::Search(o) => &o.object,
        Op::MvMapPut(o) => &o.object,
        Op::MvMapRemove(o) => &o.object,
        Op::MvMapGet(o) => &o.object,
        Op::MvMapKeys(o) => &o.object,
        Op::MvMapEntries(o) => &o.object,
    }
}

/// What an operation does to its object.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Does<'a> {
    Read,
    /// Reads how many items a list holds, and nothing of what they say. A
    /// sender needs to know how full an inbox page is.
    Length,
    /// Reads a counter. A sender needs to know which page an inbox is on.
    Number,
    Write,
    /// Adds an item to a list. Sending a message does this. `value` is
    /// `None` for the first write a line in the team's list stands for,
    /// which is not known.
    Insert { value: Option<&'a str> },
    /// Changes a JSON document at `path`: puts `value` there, puts it into
    /// the list there when `insert` is true, or takes away what is there when
    /// `value` is `None`. Both are as they were sent.
    Json { path: &'a str, value: Option<&'a str>, insert: bool },
    /// Adds `n` to a counter. Starting an inbox's next page adds one.
    Step { n: u64 },
    /// Puts a value under a key of a map. `plain` is false for a map that
    /// keeps values written at the same time.
    Put { key: &'a str, value: &'a str, plain: bool },
    /// Takes a key out of a map. `plain` is as for `Put`.
    Take { key: &'a str, plain: bool },
    /// Adds `qty` to an item of a cart.
    Add { item: &'a str, qty: u64 },
    /// Takes from a counter that cannot go below zero.
    Spend,
    /// Adds to a counter that cannot go below zero, or hands some of it to
    /// another machine.
    TopUp,
    /// Lists the object in the team's list of what it has. Only the team's
    /// notes and the task board and tree are listed this way; every other
    /// object is listed by a write.
    Lists,
    /// Moves a node in a tree. Adding a task puts it in the tree of tasks.
    Move,
}

fn does(op: &Op) -> Does<'_> {
    match op {
        Op::ListLen(_) => Does::Length,
        Op::CounterValue(_) => Does::Number,
        Op::ListInsert(o) => Does::Insert { value: Some(&o.value) },
        Op::JsonSet(o) => Does::Json { path: &o.path_json, value: Some(&o.value_json), insert: false },
        Op::JsonInsert(o) => Does::Json { path: &o.path_json, value: Some(&o.value_json), insert: true },
        Op::JsonRemove(o) => Does::Json { path: &o.path_json, value: None, insert: false },
        Op::CounterIncrement(o) => Does::Step { n: o.n },
        Op::MapPut(o) => Does::Put { key: &o.key, value: &o.value, plain: true },
        Op::MapRemove(o) => Does::Take { key: &o.key, plain: true },
        Op::MvMapPut(o) => Does::Put { key: &o.key, value: &o.value, plain: false },
        Op::MvMapRemove(o) => Does::Take { key: &o.key, plain: false },
        Op::CartAdd(o) => Does::Add { item: &o.item, qty: o.qty },
        Op::BoundedTryDecrement(_) => Does::Spend,
        Op::BoundedIncrement(_) | Op::BoundedTransfer(_) => Does::TopUp,
        Op::TreeMoveNode(_) | Op::TreeMoveNodeAt(_) => Does::Move,
        Op::RegisterGet(_)
        | Op::SetContains(_)
        | Op::SetElements(_)
        | Op::MapGet(_)
        | Op::MapKeys(_)
        | Op::MapEntries(_)
        | Op::MvMapGet(_)
        | Op::MvMapKeys(_)
        | Op::MvMapEntries(_)
        | Op::CartQuantity(_)
        | Op::CartItems(_)
        | Op::ListGet(_)
        | Op::ListToVec(_)
        | Op::JsonGet(_)
        | Op::JsonToValue(_)
        | Op::RichtextText(_)
        | Op::RichtextToSpans(_)
        | Op::TreeChildren(_)
        | Op::TreeToNodes(_)
        | Op::TreeParent(_)
        | Op::BoundedValue(_)
        | Op::BoundedAllowance(_)
        | Op::Search(_) => Does::Read,
        _ => Does::Write,
    }
}

/// The blocks a search reads. What it finds is checked against the rules one
/// object at a time, as reads are; this is only for turning away at once an
/// agent that may read none of them.
const SEARCHED: [&str; 3] = ["notes", "documents", "tasks"];

/// Refuses `object` if it is one the machines keep for themselves, such as
/// the list of machines that have left. No program may read or change
/// those, whatever its rules, or with none.
pub fn not_reserved(object: &str) -> Result<(), String> {
    let machines = orvu_runtime::leave::MACHINES;
    if object.starts_with(machines) || object == machines.trim_end_matches('/') {
        return Err(format!(
            "{object} is kept by the machines themselves: no program may read or change anything under {machines}"
        ));
    }
    Ok(())
}

/// Whether `agent`, with `rules`, may run `op`. The answer is a sentence for
/// the agent when it may not.
pub fn check(rules: &Rules, agent: &str, op: &Op) -> Result<(), String> {
    // A search reads notes, documents and tasks. What it finds is checked
    // against the rules one object at a time; an agent that may read none of
    // them has nothing to search, and is told so before anything is read for
    // it.
    if let Op::Search(_) = op {
        if !SEARCHED.iter().any(|block| rules.level(block) >= READ) {
            return Err(format!(
                "{agent} may not read notes, documents or tasks, so it cannot search. Its rules are: {rules}"
            ));
        }
    }
    // An object of a block is changed only as the kind of object it is: a
    // change of another kind would make it one, and the block could not use
    // it any more. Reads of another kind change nothing.
    let reading = matches!(does(op), Does::Read | Does::Length | Does::Number);
    if let (false, Some(kind)) = (reading, block_kind(agent, object(op))) {
        if kind_of(op) != Some(kind) {
            return Err(format!(
                "{agent} may not change {} as another kind of object than the {kind} it is. Its rules are: {rules}",
                object(op)
            ));
        }
    }
    allowed(rules, agent, object(op), does(op))
}

/// The team and the rest of the name of an object a block keeps, as
/// `orvu/<team>/<rest>`, or `None` for a name that is not one: not under
/// `orvu/`, or with a team name no team can have. A team's name is as the
/// libraries take it: letters, digits, dots, dashes and underscores,
/// starting with a letter or a digit, at most 64 characters. This is the one
/// place every check of a name asks which block of which team it is.
pub fn team_and_rest(object: &str) -> Option<(&str, &str)> {
    let (team, rest) = object.strip_prefix("orvu/")?.split_once('/')?;
    let mut chars = team.chars();
    let first = chars.next()?;
    let fits = first.is_ascii_alphanumeric()
        && team.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    fits.then_some((team, rest))
}

/// The kind of object a block keeps under `object`, if it names one.
fn block_kind(agent: &str, object: &str) -> Option<&'static str> {
    let (_, name) = team_and_rest(object)?;
    if name == "index" {
        return Some("map");
    }
    first_write(agent, name).map(|(kind, _)| kind)
}

/// The kind of object `op` reads or changes, as the team's list names kinds,
/// or `None` for one that is of no kind.
fn kind_of(op: &Op) -> Option<&'static str> {
    Some(match op {
        Op::CounterIncrement(_) | Op::CounterDecrement(_) | Op::CounterValue(_) => "counter",
        Op::RegisterSet(_) | Op::RegisterGet(_) => "register",
        Op::SetAdd(_) | Op::SetRemove(_) | Op::SetContains(_) | Op::SetElements(_) => "set",
        Op::MapPut(_) | Op::MapRemove(_) | Op::MapGet(_) | Op::MapKeys(_) | Op::MapEntries(_) => "map",
        Op::MvMapPut(_) | Op::MvMapRemove(_) | Op::MvMapGet(_) | Op::MvMapKeys(_) | Op::MvMapEntries(_) => "mvmap",
        Op::CartAdd(_) | Op::CartRemoveQty(_) | Op::CartRemoveItem(_) | Op::CartQuantity(_) | Op::CartItems(_) => "cart",
        Op::ListInsert(_) | Op::ListDelete(_) | Op::ListGet(_) | Op::ListLen(_) | Op::ListToVec(_) => "list",
        Op::JsonSet(_) | Op::JsonRemove(_) | Op::JsonGet(_) | Op::JsonToValue(_) | Op::JsonInsert(_) => "json",
        Op::RichtextInsert(_)
        | Op::RichtextDelete(_)
        | Op::RichtextFormat(_)
        | Op::RichtextUnformat(_)
        | Op::RichtextText(_)
        | Op::RichtextToSpans(_)
        | Op::RichtextInsertMarked(_) => "richtext",
        Op::TreeMoveNode(_) | Op::TreeMoveNodeAt(_) | Op::TreeSetLabel(_) | Op::TreeChildren(_) | Op::TreeToNodes(_) | Op::TreeParent(_) => {
            "tree"
        }
        Op::BoundedIncrement(_)
        | Op::BoundedTryDecrement(_)
        | Op::BoundedTransfer(_)
        | Op::BoundedValue(_)
        | Op::BoundedAllowance(_) => "bounded",
        Op::Declare(_) | Op::Search(_) => return None,
    })
}

/// Whether an agent with rules may make `ops` together, as one request: a
/// top-up of a budget goes with its line in the record of who topped it up,
/// for the same amount, and that line only with that top-up. Each top-up is
/// paired with one line. `Err` gives the number of the first operation left
/// without its pair, and why, for the agent called `agent`.
pub fn check_together(agent: &str, ops: &[Option<&Op>]) -> Result<(), (usize, String)> {
    let mut lines: Vec<(usize, (String, String), u64)> = ops
        .iter()
        .enumerate()
        .filter_map(|(i, op)| match op {
            Some(Op::CartAdd(o)) => budget(&o.object, "budget-topped").map(|which| (i, which, o.qty)),
            _ => None,
        })
        .collect();
    for (i, op) in ops.iter().enumerate() {
        if let Some(Op::BoundedIncrement(o)) = op {
            if let Some(which) = budget(&o.object, "budget") {
                match lines.iter().position(|(_, line, qty)| *line == which && *qty == o.n) {
                    Some(at) => {
                        lines.remove(at);
                    }
                    None => {
                        return Err((
                            i,
                            format!(
                                "{agent} may not make a top-up without its line in the record of who topped the budget up, \
                                 for the same amount in the same request"
                            ),
                        ))
                    }
                }
            }
        }
    }
    match lines.first() {
        Some((i, _, _)) => Err((
            *i,
            format!("{agent} may not write a line of who topped up a budget without that top-up, of the same amount, in the same request"),
        )),
        None => Ok(()),
    }
}

/// The team and the budget an object of `block` (`budget` or
/// `budget-topped`) belongs to, if it is one.
fn budget(object: &str, block: &str) -> Option<(String, String)> {
    let (team, name) = team_and_rest(object)?;
    let name = name.strip_prefix(block)?.strip_prefix('/')?;
    (!name.is_empty() && !name.contains('/')).then(|| (team.to_string(), name.to_string()))
}

/// The record of who topped up the budget `object`, if it is a budget.
pub fn topped_record(object: &str) -> Option<String> {
    budget(object, "budget").map(|(team, name)| format!("orvu/{team}/budget-topped/{name}"))
}

/// Whether `object` is a record of who topped up a budget.
pub fn is_topped_record(object: &str) -> bool {
    budget(object, "budget-topped").is_some()
}

/// Whether `agent`, with `rules`, may watch `object`. A watch reads.
pub fn check_watch(rules: &Rules, agent: &str, object: &str) -> Result<(), String> {
    allowed(rules, agent, object, Does::Read)
}

fn allowed(rules: &Rules, agent: &str, object: &str, does: Does) -> Result<(), String> {
    let refuse = |what: &str| Err(format!("{agent} may not {what}. Its rules are: {rules}"));
    let need = |block: &str, level: u8, what: &str| if rules.level(block) >= level { Ok(()) } else { refuse(what) };
    let reading = matches!(does, Does::Read | Does::Length | Does::Number);
    if !object.starts_with("orvu/") {
        return if reading { need("objects", READ, "read plain objects") } else { need("objects", WRITE, "change plain objects") };
    }
    let Some((team, name)) = team_and_rest(object) else {
        return refuse(&format!("use {object}, which belongs to no building block"));
    };
    let mut parts = name.split('/');
    let first = parts.next().unwrap_or("");
    // Plain reads and writes of the blocks that have nothing of their own.
    let plain = |block: &str, what: &str| {
        if reading { need(block, READ, &format!("read {what}")) } else { need(block, WRITE, &format!("change {what}")) }
    };
    match first {
        // The list of what the team has. Anybody may read it, as every block
        // needs it, so the names of what the team has, such as its documents,
        // budgets and the inboxes written to, are seen by every agent. A line
        // is added to it by a block the first time it writes an object, and
        // may be added only by an agent that may make that first write, and
        // only with the kind that object is. The list is a plain map, and a
        // put of the other kind of map would make it one of those. The only
        // lines taken off it are a firm task's spent tickets.
        "index" if parts.next().is_none() => match does {
            Does::Read | Does::Length | Does::Number => Ok(()),
            Does::Put { key, value, plain: true } => {
                let named = format!("orvu/{team}/{key}");
                let Some((kind, first_write)) = first_write(agent, key) else {
                    return allowed(rules, agent, &named, Does::Write);
                };
                if value != kind {
                    return refuse(&format!("list {key} in the team's list as another kind of object than the {kind} it is"));
                }
                allowed(rules, agent, &named, first_write)
            }
            // A firm task's tickets are taken off the list once they can never
            // be had again, by an agent that may change tasks.
            Does::Take { key, plain: true } if matches!(key.split('/').next(), Some("task-ticket" | "task-takeover")) => {
                need("tasks", WRITE, "change tasks")
            }
            _ => refuse("change the team's list of what it has"),
        },
        // Structured notes keep no writer's name, so nothing in a change
        // shows whose the note is.
        "note" if reading => need("notes", READ, "read notes"),
        "note" if rules.level("notes") == OWN => {
            refuse("change structured notes, which keep no writer's name, so that needs notes:write")
        }
        "note" => need("notes", WRITE, "change notes"),
        // Notes kept the old way, and who wrote each. The sidecar moves them
        // over and nothing uses them after that, so they are only read: what
        // was written there would be moved over under the name already on it.
        "notes" | "notes-by" => match does {
            Does::Read | Does::Length | Does::Number => need("notes", READ, "read notes"),
            _ => refuse("change notes kept the old way, which are no longer used"),
        },
        // The team's notes. Each value starts with the name of the agent
        // that wrote it, which may only be its own. They are kept in a map
        // that keeps values written at once, and only that kind of change is
        // taken: any other would make them another kind of object. The empty
        // name marks that the old notes were moved over, and only the sidecar
        // writes it.
        "notes-mv" if parts.next().is_none() => match does {
            Does::Read | Does::Length | Does::Number => need("notes", READ, "read notes"),
            Does::Put { key, .. } | Does::Take { key, .. } if key == crate::notes::MOVED => {
                refuse("change the mark that the team's old notes were moved over")
            }
            Does::Put { value, plain: false, .. } => match value.split_once(crate::notes::SEPARATOR) {
                Some((by, _)) if by == agent => need("notes", OWN, "change notes"),
                _ => refuse("write another agent's name on a note"),
            },
            // With notes:own, whose the note is now is checked by `guards`.
            Does::Lists | Does::Take { plain: false, .. } => need("notes", OWN, "change notes"),
            _ => refuse("change the team's notes other than by writing or removing a note"),
        },
        "doc" => plain("documents", "documents"),
        "tally" => plain("tallies", "tallies"),
        // The task board. A task carries the name of the agent that added
        // it, which may only be its own. Whose a task is now, which tasks:own
        // goes by, and that the name on it does not change, are checked by
        // `guards`.
        "tasks" if parts.next().is_none() => match does {
            Does::Read | Does::Length | Does::Number => need("tasks", READ, "read tasks"),
            Does::Lists => need("tasks", OWN, "change tasks"),
            Does::Json { path, value, insert } => {
                need("tasks", OWN, "change tasks")?;
                added_by(agent, path, value, insert).or_else(&refuse)
            }
            _ => need("tasks", WRITE, "change tasks"),
        },
        // Where each task hangs. Adding a task puts it there.
        "tasks-tree" => match does {
            Does::Read | Does::Length | Does::Number => need("tasks", READ, "read tasks"),
            Does::Lists | Does::Move => need("tasks", OWN, "change tasks"),
            _ => need("tasks", WRITE, "change tasks"),
        },
        // A firm task's tickets are counters that are spent from and topped
        // up, and are part of the task. They carry no name.
        "task-ticket" | "task-takeover" => plain("tasks", "tasks"),
        // An agent's place on the roster is kept under its name, then a
        // separator, then a few letters that tell its programs apart.
        "roster" => match does {
            Does::Read | Does::Length | Does::Number => need("roster", READ, "read the roster"),
            Does::Put { key, .. } | Does::Take { key, .. } if !key.starts_with(&format!("{agent}\u{1f}")) => {
                refuse("change another agent's place on the roster")
            }
            _ => need("roster", WRITE, "change its place on the roster"),
        },
        "budget" => match does {
            Does::Read | Does::Length | Does::Number => need("budgets", READ, "read budgets"),
            Does::Spend => need("budgets", SPEND, "spend from budgets"),
            _ => need("budgets", TOP_UP, "top up budgets"),
        },
        // What each agent spent: only one's own line, and only added to,
        // whatever the agent's level.
        "budget-spent" => match does {
            Does::Read | Does::Length | Does::Number => need("budgets", READ, "read budgets"),
            Does::Add { item, .. } if item == agent => need("budgets", SPEND, "spend from budgets"),
            Does::Add { .. } => refuse("write another agent's name on what was spent"),
            _ => refuse("change what was spent from a budget, which is only ever added to"),
        },
        // Who topped up a budget, and by how much: only one's own line, only
        // added to, and only by an agent that may top up.
        "budget-topped" => match does {
            Does::Read | Does::Length | Does::Number => need("budgets", READ, "read budgets"),
            Does::Add { item, .. } if item == agent => need("budgets", TOP_UP, "top up budgets"),
            Does::Add { .. } => refuse("write another agent's name on who topped up a budget"),
            _ => refuse("change who topped up a budget, which is only ever added to"),
        },
        "inbox" | "inbox-page" | "inbox-given" | "inbox-count" | "inbox-done" => {
            inbox(rules, agent, first, parts.collect(), does, &refuse)
        }
        _ => refuse(&format!("use {object}, which belongs to no building block")),
    }
}

/// What kind of object the team's list names with `key`, and what the first
/// write a block makes to it does, or `None` for a name no block uses.
fn first_write<'a>(agent: &'a str, key: &str) -> Option<(&'static str, Does<'a>)> {
    Some(match key.split('/').next().unwrap_or("") {
        "notes" | "notes-by" | "roster" => ("map", Does::Write),
        "notes-mv" => ("mvmap", Does::Lists),
        "note" => ("json", Does::Write),
        "tasks" => ("json", Does::Lists),
        "doc" => ("richtext", Does::Write),
        "tally" => ("cart", Does::Write),
        "tasks-tree" => ("tree", Does::Lists),
        "task-ticket" | "task-takeover" => ("bounded", Does::Write),
        // A budget is first written by topping it up, and what was spent and
        // who topped it up by the agent's own line.
        "budget" => ("bounded", Does::TopUp),
        "budget-spent" | "budget-topped" => ("cart", Does::Add { item: agent, qty: 1 }),
        "inbox" => ("list", Does::Insert { value: None }),
        "inbox-page" => ("counter", Does::Step { n: 1 }),
        "inbox-given" => ("set", Does::Write),
        "inbox-count" => ("cart", Does::Add { item: "0", qty: 1 }),
        "inbox-done" => ("map", Does::Write),
        _ => return None,
    })
}

/// Whether a change to the task board at `path` keeps the name of the agent
/// that added each task to the agent's own, as `Does::Json` gives it. A path
/// that does not parse changes nothing, as the engine refuses it too. The
/// name and the value are read as the engine reads them. `Err` says what the
/// agent may not do.
fn added_by(agent: &str, path: &str, value: Option<&str>, insert: bool) -> Result<(), &'static str> {
    // The board is changed one task at a time, and inside a task one field at
    // a time: a path that starts anywhere else, or does not parse, is turned
    // away, whatever it would do.
    let Ok(path) = parse_path(path) else { return Err(ONE_TASK) };
    if !matches!(path.as_slice(), [Seg::Key(_)] | [Seg::Key(_), Seg::Key(_), ..]) {
        return Err(ONE_TASK);
    }
    let sent = value.is_some();
    let value = || value.and_then(|value| JsonValue::from_json_str(value).ok());
    let named = |task: &JsonValue| matches!(task, JsonValue::Object(fields) if matches!(fields.get("created_by"), Some(JsonValue::Str(by)) if by == agent));
    let created = |field: &str| field == "created_by";
    let ok = match (path.as_slice(), sent, insert) {
        // A whole task, added: it carries the agent's name. A list put where
        // a task is would have nobody's name.
        ([Seg::Key(_)], true, false) => value().is_some_and(|task| named(&task)),
        ([Seg::Key(_)], true, true) => false,
        ([Seg::Key(_)], false, _) => true,
        ([Seg::Key(_), Seg::Key(field)], true, false) if created(field) => matches!(value(), Some(JsonValue::Str(by)) if by == agent),
        ([Seg::Key(_), Seg::Key(field), ..], _, _) if created(field) => false,
        ([Seg::Key(_), Seg::Key(_), ..], _, _) => true,
        _ => false,
    };
    if ok { Ok(()) } else { Err("add a task under another name than its own, or take the name off a task") }
}

/// Why a change of the task board that is not of one task is turned away.
const ONE_TASK: &str = "change the task board other than one task at a time, by its id and its fields' names";

/// The fields a claim writes. With tasks:own, an agent may write them on an
/// open task when the same request claims it under its own name: at the first
/// look, and, as each is written, once the claim has landed.
const CLAIM_FIELDS: [&str; 5] = ["seen", "lease", "ticket", "claimed_by", "state"];

/// What a change to the task board or to the notes is checked against: the
/// task or the note as it is, once before the request runs and again as the
/// write is made. One is made by `guards` for each such change of a request.
#[derive(Clone, Debug)]
pub struct Guard {
    agent: String,
    /// The agent's rules as they are written, for the reason it is given.
    rules: String,
    own_tasks: bool,
    own_notes: bool,
    /// The tasks the request claims under the agent's own name, each as
    /// its board and its id.
    claims: Arc<BTreeSet<(String, String)>>,
}

/// The name of the agent that added `task`, and of the one that holds it.
fn names(task: &JsonValue) -> (Option<&str>, Option<&str>) {
    let JsonValue::Object(fields) = task else { return (None, None) };
    let name = |field: &str| match fields.get(field) {
        Some(JsonValue::Str(name)) if !name.is_empty() => Some(name.as_str()),
        _ => None,
    };
    (name("created_by"), name("claimed_by"))
}

/// Why a change under a task's "tickets" is turned away, or `None` if it may
/// be made. `under` is the path below "tickets", and `task` the task as it
/// is. Only a number not written yet may be added, and the whole of them may
/// be written as an object, which adds the new ones.
fn tickets_change(under: &[Seg], value: Option<&JsonValue>, insert: bool, task: Option<&JsonValue>) -> Option<&'static str> {
    let taken = "change or take off a ticket number written on a task";
    if insert || value.is_none() {
        return Some(taken);
    }
    match under {
        [] => (!matches!(value, Some(JsonValue::Object(_)))).then_some(taken),
        [Seg::Key(number)] => {
            let written = match task {
                Some(JsonValue::Object(fields)) => match fields.get("tickets") {
                    Some(JsonValue::Object(numbers)) => numbers.contains_key(number),
                    _ => false,
                },
                _ => false,
            };
            written.then_some(taken)
        }
        _ => Some(taken),
    }
}

/// Whether `task` is open: nobody holds it and it is not done.
fn open(task: &JsonValue) -> bool {
    let JsonValue::Object(fields) = task else { return false };
    names(task).1.is_none()
        && !matches!(fields.get("state"), Some(JsonValue::Str(state)) if state == "done")
        // As the libraries read it: a "finished" mark that is not there, null
        // or false means not finished.
        && matches!(fields.get("finished"), None | Some(JsonValue::Null) | Some(JsonValue::Bool(false)))
}

impl Guard {
    /// The agent the guard is for.
    pub fn agent(&self) -> &str {
        &self.agent
    }

    fn refuse(&self, what: &str) -> Result<(), String> {
        Err(format!("{} may not {what}. Its rules are: {}", self.agent, self.rules))
    }

    /// Whether a change of the task board at `path` may be made: putting
    /// `value` there, or into the list there when `insert` is true, or
    /// taking away what is there when `value` is `None`, on the task board
    /// `board`. `now` is what is at the path's first step as it is: the task,
    /// or the whole board for an empty path. `first` is true for the look
    /// before the request runs, and false as the write is made.
    ///
    /// The name of the agent that added a task never changes. With
    /// tasks:own, a task another agent holds is not touched at all, and the
    /// rest has to be the agent's own as it is: a task it added or holds. An
    /// open task may be claimed: at the first look, the request may write
    /// what a claim writes on an open task it claims; as each write is made,
    /// only the claim itself is taken on an open task, and the rest once the
    /// task is the agent's.
    pub fn board(
        &self,
        board: &str,
        path: &[Seg],
        value: Option<&JsonValue>,
        insert: bool,
        now: Option<&JsonValue>,
        first: bool,
    ) -> Result<(), String> {
        let set = value.is_some() && !insert;
        let renamed = "change the name of the agent that added a task";
        // One task at a time, and inside it one field at a time; anything
        // else is turned away, as `added_by` turns it away before.
        if !matches!(path, [Seg::Key(_)] | [Seg::Key(_), Seg::Key(_), ..]) {
            return self.refuse(ONE_TASK);
        }
        // What is not an object is no task, whatever sits there.
        let now = now.filter(|task| matches!(task, JsonValue::Object(_)));
        match path {
            [Seg::Key(id), rest @ ..] => {
                let field = match rest.first() {
                    Some(Seg::Key(field)) => Some(field.as_str()),
                    _ => None,
                };
                let (added, held) = now.map(names).unwrap_or_default();
                if let Some(by) = added {
                    let same = match (rest, value) {
                        ([], Some(task)) if set => names(task).0 == Some(by),
                        ([_], Some(JsonValue::Str(name))) if set && field == Some("created_by") => name == by,
                        _ => field != Some("created_by"),
                    };
                    if !same {
                        return self.refuse(renamed);
                    }
                }
                // A firm task's number is the highest of the ticket numbers
                // written under its "tickets", and must never go down. So
                // they are only ever added to: one that is there is never
                // changed or taken off. Writing them whole adds the new ones,
                // as the write is made.
                if field == Some("tickets") {
                    if let Some(why) = tickets_change(&rest[1..], value, insert, now) {
                        return self.refuse(why);
                    }
                }
                // Inside a task that is not there nothing is written, by any
                // agent with rules: it would leave a task behind that nobody
                // added. A task is added whole.
                if !rest.is_empty() && now.is_none() {
                    return self.refuse("change a task that is not there");
                }
                if !self.own_tasks {
                    return Ok(());
                }
                if !self.names_only_itself(rest, value, insert) {
                    return self.refuse("name another agent on a task as the one that holds it, finished it or changed it");
                }
                let Some(task) = now else {
                    // Nothing is there: the task may be added, whole, and
                    // removing it is removing nothing.
                    return if rest.is_empty() { Ok(()) } else { self.refuse("change a task that is not there") };
                };
                if held.is_some_and(|holder| holder != self.agent) {
                    return self.refuse("change a task another agent holds");
                }
                let mine = added == Some(&self.agent) || held == Some(&self.agent);
                if rest.is_empty() && added != Some(&self.agent) {
                    return self.refuse("replace or remove a task it did not add");
                }
                let by_me = matches!(value, Some(JsonValue::Str(name)) if *name == self.agent);
                let claiming = if first {
                    self.claims.contains(&(board.to_string(), id.clone()))
                        && field.is_some_and(|field| CLAIM_FIELDS.contains(&field))
                        && (field != Some("claimed_by") || by_me)
                } else {
                    field == Some("claimed_by") && rest.len() == 1 && set && by_me
                };
                if mine || (open(task) && claiming) {
                    Ok(())
                } else {
                    self.refuse("change a task it did not add and does not hold, other than by claiming it while it is open")
                }
            }
            _ => self.refuse(ONE_TASK),
        }
    }

    /// Whether a change at `rest`, the path inside one task, names only this
    /// agent, or nobody, as the one that holds the task, finished it or last
    /// changed it. A name taken away names nobody.
    fn names_only_itself(&self, rest: &[Seg], value: Option<&JsonValue>, insert: bool) -> bool {
        let itself = |name: Option<&JsonValue>| match name {
            None | Some(JsonValue::Null) => true,
            Some(JsonValue::Str(name)) => *name == self.agent,
            _ => false,
        };
        let finished = |mark: &JsonValue| match mark {
            JsonValue::Object(fields) => itself(fields.get("by")),
            JsonValue::Null => true,
            _ => false,
        };
        let key = |at: usize| match rest.get(at) {
            Some(Seg::Key(key)) => Some(key.as_str()),
            _ => None,
        };
        let Some(value) = value else { return true };
        match (key(0), rest.len()) {
            (None, 0) => match value {
                JsonValue::Object(fields) => {
                    itself(fields.get("claimed_by"))
                        && itself(fields.get("finished_by"))
                        && fields.get("finished").is_none_or(finished)
                        && fields.get("taken_over").is_none_or(|record| matches!(record, JsonValue::Null))
                }
                _ => false,
            },
            (Some("claimed_by" | "finished_by"), 1) => !insert && itself(Some(value)),
            (Some("claimed_by" | "finished_by"), _) => false,
            // Who last changed a task is written by the sidecar alone; what
            // the agent writes there is not kept.
            (Some("updated_by"), _) => true,
            // The record of a firm task taken over names two agents, and an
            // agent with tasks:own takes no firm task over.
            (Some("taken_over"), _) => false,
            (Some("finished"), 1) => !insert && finished(value),
            (Some("finished"), 2) if key(1) == Some("by") => !insert && itself(Some(value)),
            (Some("finished"), _) => key(1) != Some("by"),
            // Any other field names nobody.
            (Some(_), _) => true,
            (None, _) => false,
        }
    }

    /// Whether a note whose values are `now` may be written or removed: with
    /// notes:own, only if every value is the agent's own, or there is none.
    pub fn note(&self, now: &[String]) -> Result<(), String> {
        if !self.own_notes || now.iter().all(|value| crate::notes::split(value).0 == self.agent && value.contains(crate::notes::SEPARATOR)) {
            return Ok(());
        }
        self.refuse("change another agent's note")
    }

    /// Whether the task `now` may be moved in the tree of tasks: with
    /// tasks:own, only one the agent added, or one that is not on the board
    /// yet, as a task being added is put in the tree first.
    pub fn tree(&self, now: Option<&JsonValue>) -> Result<(), String> {
        if !self.own_tasks {
            return Ok(());
        }
        match now.map(names) {
            // Not on the board yet: being added.
            None => Ok(()),
            Some((Some(added), _)) if added == self.agent => Ok(()),
            Some(_) => self.refuse("move a task it did not add"),
        }
    }
}

/// What a request changes that a guard looks at.
enum Watched {
    /// A change of a team's task board at a path.
    Board { board: String, path: Vec<Seg>, value: Option<JsonValue>, insert: bool },
    /// A write or a removal of a team's note.
    Note { notes: String, key: String },
    /// A move of a task in a team's tree of tasks, and that team's board.
    Tree { board: String, task: String },
    /// A top-up of a budget, or a line in its record of who topped it up.
    /// The sidecar writes the line itself, with the top-up.
    TopUp,
}

fn watched(op: &Op) -> Option<Watched> {
    let team = |object: &str, block: &str| {
        let (team, rest) = team_and_rest(object)?;
        (rest == block).then(|| team.to_string())
    };
    let board = |object: &str, path: &str, value: Option<&str>, insert: bool| {
        team(object, "tasks")?;
        let path = parse_path(path).ok()?;
        let value = match value {
            Some(value) => Some(JsonValue::from_json_str(value).ok()?),
            None => None,
        };
        Some(Watched::Board { board: object.to_string(), path, value, insert })
    };
    match op {
        Op::JsonSet(o) => board(&o.object, &o.path_json, Some(&o.value_json), false),
        Op::JsonInsert(o) => board(&o.object, &o.path_json, Some(&o.value_json), true),
        Op::JsonRemove(o) => board(&o.object, &o.path_json, None, false),
        Op::MvMapPut(pb::MvMapPut { object, key, .. }) | Op::MvMapRemove(pb::MvMapRemove { object, key }) => {
            team(object, "notes-mv").map(|_| Watched::Note { notes: object.clone(), key: key.clone() })
        }
        Op::TreeMoveNode(pb::TreeMoveNode { object, child, .. }) | Op::TreeMoveNodeAt(pb::TreeMoveNodeAt { object, child, .. }) => {
            team(object, "tasks-tree").map(|team| Watched::Tree { board: format!("orvu/{team}/tasks"), task: child.clone() })
        }
        Op::BoundedIncrement(o) if topped_record(&o.object).is_some() => Some(Watched::TopUp),
        Op::CartAdd(o) if is_topped_record(&o.object) => Some(Watched::TopUp),
        _ => None,
    }
}

/// What is at `path` of the JSON document `object` now, if there is one.
async fn json_at(client: &orvu_sdk::Client, object: &str, path: &[Seg]) -> Option<JsonValue> {
    if !client.exists(object).await {
        return None;
    }
    client.json(object).await.ok()?.get(path).await.ok().flatten()
}

/// Where a guard looks for a change of the board at `path`: the task, or
/// the whole board.
pub fn looked_at(path: &[Seg]) -> Vec<Seg> {
    path.iter().take(1).cloned().collect()
}

/// For an agent with rules, the guard of each operation of a request, read
/// from the tasks and notes they change as those are now; `None` for an
/// operation that needs none. The whole request is looked at at once, so
/// that one part cannot make another look like the agent's own. `Err` gives
/// the number of the operation that may not be made, and why.
pub async fn guards(client: &orvu_sdk::Client, rules: &Rules, agent: &str, ops: &[&Op]) -> Result<Vec<Option<Guard>>, (usize, String)> {
    let watched: Vec<Option<Watched>> = ops.iter().map(|op| watched(op)).collect();
    let own_tasks = rules.level("tasks") == OWN;
    let own_notes = rules.level("notes") == OWN;
    let mut claims = BTreeSet::new();
    let mut now = Vec::with_capacity(ops.len());
    for one in &watched {
        now.push(match one {
            Some(Watched::Board { board, path, value, .. }) => {
                if let ([Seg::Key(id), Seg::Key(field)], Some(JsonValue::Str(name))) = (path.as_slice(), value) {
                    if field == "claimed_by" && name == agent {
                        claims.insert((board.clone(), id.clone()));
                    }
                }
                Looked::Json(json_at(client, board, &looked_at(path)).await)
            }
            Some(Watched::Tree { board, task }) if own_tasks => Looked::Json(json_at(client, board, &[Seg::Key(task.clone())]).await),
            Some(Watched::Note { notes, key }) if own_notes && client.exists(notes).await => {
                Looked::Note(match client.mv_map(notes).await {
                    Ok(map) => map.get(key).await.unwrap_or_default(),
                    Err(_) => Vec::new(),
                })
            }
            _ => Looked::Nothing,
        });
    }
    let guard = Guard {
        agent: agent.to_string(),
        rules: rules.to_string(),
        own_tasks,
        own_notes,
        claims: Arc::new(claims),
    };
    let mut guards = Vec::with_capacity(ops.len());
    for (i, (one, now)) in watched.iter().zip(&now).enumerate() {
        let checked = match (one, now) {
            (Some(Watched::Board { board, path, value, insert }), Looked::Json(now)) => {
                guard.board(board, path, value.as_ref(), *insert, now.as_ref(), true).map(|()| Some(guard.clone()))
            }
            (Some(Watched::Note { .. }), Looked::Note(now)) => guard.note(now).map(|()| Some(guard.clone())),
            (Some(Watched::Note { .. }), Looked::Nothing) if own_notes => Ok(Some(guard.clone())),
            (Some(Watched::Tree { .. }), Looked::Json(now)) => guard.tree(now.as_ref()).map(|()| None),
            (Some(Watched::TopUp), _) => Ok(Some(guard.clone())),
            _ => Ok(None),
        };
        guards.push(checked.map_err(|why| (i, why))?);
    }
    Ok(guards)
}

/// What a guard read of an object before a request runs.
enum Looked {
    Nothing,
    Json(Option<JsonValue>),
    Note(Vec<String>),
}

/// The inbox. A sender adds a message to another agent's inbox, and one to the
/// count of messages on that page, and needs to know only which page it is on
/// and how full the page is. Only the owner reads what is in it and the
/// counts, and keeps note of what it has been given.
fn inbox(
    rules: &Rules,
    agent: &str,
    first: &str,
    parts: Vec<&str>,
    does: Does,
    refuse: &dyn Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    let level = rules.level("inbox");
    // Every agent reads the inbox for everyone. An agent's own inbox is
    // under its name, which for a name this sidecar takes is the name as it
    // is.
    let owner = parts.first().copied();
    let mine = owner.is_some_and(|owner| owner == agent || owner == "@everyone");
    let may = |ok: bool, what: &str| if ok { Ok(()) } else { refuse(what) };
    // What an agent lacks to read its own inbox is the rule for it.
    let reading = if mine { "read its inbox, which needs inbox:read" } else { "read another agent's inbox" };
    match (first, does) {
        // Seeing how full a page is, or which page an inbox is on, which a
        // sender needs, and the owner too.
        ("inbox", Does::Length) | ("inbox-page", Does::Number) => may(
            level >= WRITE || (level >= READ && mine),
            if mine && level < READ { reading } else { "send messages, which needs inbox:write" },
        ),
        ("inbox" | "inbox-page", Does::Read) => may(level >= READ && mine, reading),
        // Sending: a message added to a page, one added to the count of
        // messages on that page, or the next page begun.
        ("inbox", Does::Insert { value }) => {
            may(level >= WRITE, "send messages, which needs inbox:write")?;
            // A message carries the name of the agent that sent it, which may
            // only be its own. It is read as the libraries read it.
            let sender = value.map(|value| match serde_json::from_str::<serde_json::Value>(value) {
                Ok(serde_json::Value::Object(fields)) => fields.get("from").and_then(|from| from.as_str()) == Some(agent),
                _ => false,
            });
            may(sender != Some(false), "send a message under another name than its own")
        }
        ("inbox-count", Does::Add { qty: 1, .. }) | ("inbox-page", Does::Step { n: 1 }) => {
            may(level >= WRITE, "send messages, which needs inbox:write")
        }
        // A count is only ever one more per message. Any other number would
        // make the owner read the count wrong, and miss messages or be given
        // them again.
        ("inbox-count", Does::Add { .. }) => refuse("add anything but one to the count of an inbox page"),
        ("inbox-count", Does::Read) => may(level >= READ && mine, reading),
        ("inbox-page", Does::Step { .. }) => refuse("move an inbox on by more than one page"),
        // What an agent has been given, and how far it got on each page, is
        // its own.
        ("inbox-given" | "inbox-done", _) => {
            let own = owner == Some(agent);
            may(level >= READ && own, if own { "read its inbox, which needs inbox:read" } else { "read another agent's inbox" })
        }
        _ if mine => refuse("change the messages in an inbox, which nobody may"),
        _ => refuse("change another agent's inbox"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(words: &str) -> Rules {
        Rules::parse(&words.split_whitespace().collect::<Vec<_>>()).unwrap()
    }

    fn may(rules_text: &str, op: Op) -> bool {
        check(&rules(rules_text), "alice", &op).is_ok()
    }

    fn put(object: &str, key: &str, value: &str) -> Op {
        Op::MapPut(pb::MapPut { object: object.into(), key: key.into(), value: value.into() })
    }
    fn entries(object: &str) -> Op {
        Op::MapEntries(pb::MapEntries { object: object.into() })
    }
    fn json_set(object: &str) -> Op {
        Op::JsonSet(pb::JsonSet { object: object.into(), ..Default::default() })
    }
    fn json_get(object: &str) -> Op {
        Op::JsonGet(pb::JsonGet { object: object.into(), ..Default::default() })
    }
    fn spend(object: &str) -> Op {
        Op::BoundedTryDecrement(pb::BoundedTryDecrement { object: object.into(), n: 1 })
    }
    fn top_up(object: &str) -> Op {
        Op::BoundedIncrement(pb::BoundedIncrement { object: object.into(), n: 1 })
    }
    fn cart_add(object: &str, item: &str) -> Op {
        Op::CartAdd(pb::CartAdd { object: object.into(), item: item.into(), qty: 1 })
    }
    fn list_len(object: &str) -> Op {
        Op::ListLen(pb::ListLen { object: object.into() })
    }
    fn list_all(object: &str) -> Op {
        Op::ListToVec(pb::ListToVec { object: object.into() })
    }
    fn list_insert(object: &str) -> Op {
        let value = r#"{"id":"m-1","from":"alice","body":"hi","sent":1}"#.into();
        Op::ListInsert(pb::ListInsert { object: object.into(), index: 0, value })
    }
    fn counter_value(object: &str) -> Op {
        Op::CounterValue(pb::CounterValue { object: object.into() })
    }
    fn set_add(object: &str) -> Op {
        Op::SetAdd(pb::SetAdd { object: object.into(), member: "0".into() })
    }

    #[test]
    fn rules_are_read_and_written_the_same_way() {
        let read = rules("budgets:spend notes:write tasks:read");
        assert_eq!(read.to_string(), "notes:write tasks:read budgets:spend");
        assert_eq!(Rules::parse(&["none"]).unwrap().to_string(), "none");
        assert_eq!(rules("budgets:top-up objects:read").to_string(), "budgets:top-up objects:read");
    }

    #[test]
    fn a_wrong_rule_is_explained() {
        for (words, says) in [
            (vec!["notes"], "is not a rule"),
            (vec!["notes:admin"], "is not a rule"),
            (vec!["mail:read"], "is not a rule"),
            (vec!["budgets:write"], "budgets take read, spend or top-up, not write"),
            (vec!["notes:spend"], "is not a rule"),
            (vec!["notes:read", "notes:write"], "given twice"),
            (vec![], "at least one rule"),
        ] {
            let wrong = Rules::parse(&words).unwrap_err();
            assert!(wrong.contains(says), "{words:?} gave {wrong}");
        }
    }

    #[test]
    fn read_and_write_go_by_block() {
        assert!(may("notes:read", json_get("orvu/crew/note/plan")));
        assert!(!may("notes:read", json_set("orvu/crew/note/plan")));
        assert!(may("notes:write", json_set("orvu/crew/note/plan")));
        assert!(!may("tasks:write", json_get("orvu/crew/note/plan")));
        assert!(may("tasks:read", json_get("orvu/crew/tasks")));
        assert!(!may("tasks:read", json_set("orvu/crew/tasks")));
        assert!(may("documents:write", Op::RichtextInsert(pb::RichtextInsert { object: "orvu/crew/doc/r".into(), ..Default::default() })));
        assert!(!may("notes:write", Op::RichtextText(pb::RichtextText { object: "orvu/crew/doc/r".into() })));
        assert!(may("tallies:write", cart_add("orvu/crew/tally/votes", "bob")));
        assert!(!may("none", entries("orvu/crew/notes")));
    }

    // A map that keeps clashing writes is read and written as a map is: its
    // reads are reads, and a put or a remove names its key, so a block that
    // checks the key checks it here too.
    #[test]
    fn a_map_that_keeps_clashes_reads_and_writes_as_a_map_does() {
        let mv_put = |o: &str, k: &str| Op::MvMapPut(pb::MvMapPut { object: o.into(), key: k.into(), value: "map".into() });
        let mv_remove = |o: &str, k: &str| Op::MvMapRemove(pb::MvMapRemove { object: o.into(), key: k.into() });
        let reads = [
            Op::MvMapGet(pb::MvMapGet { object: "plain".into(), key: "k".into() }),
            Op::MvMapKeys(pb::MvMapKeys { object: "plain".into() }),
            Op::MvMapEntries(pb::MvMapEntries { object: "plain".into() }),
        ];
        for read in reads {
            assert!(may("objects:read", read.clone()));
            assert!(!may("notes:write", read));
        }
        assert!(!may("objects:read", mv_put("plain", "k")));
        assert!(!may("objects:read", mv_remove("plain", "k")));
        assert!(may("objects:write", mv_put("plain", "k")));
        // The roster is a plain map, so a put of the other kind is turned
        // away, whoever's key it names.
        assert!(!may("roster:write", mv_put("orvu/crew/roster", "alice\u{1f}a")));
        assert!(!may("roster:write", mv_put("orvu/crew/roster", "bob\u{1f}a")));
        assert!(!may("roster:write", mv_remove("orvu/crew/roster", "bob\u{1f}a")));
        // The team's list is a plain map. Writing it as the other kind would
        // make it one, and every block that lists what it writes would fail.
        assert!(may("notes:write", put("orvu/crew/index", "note/plan", "json")));
        assert!(!may("notes:write", mv_put("orvu/crew/index", "note/plan")));
    }

    // The team's notes keep the name of each value's writer, which may only
    // be the agent's own, and are changed only as the map they are.
    #[test]
    fn a_note_carries_its_writers_name() {
        let notes = "orvu/crew/notes-mv";
        let mv_put = |k: &str, v: &str| Op::MvMapPut(pb::MvMapPut { object: notes.into(), key: k.into(), value: v.into() });
        let mv_remove = |k: &str| Op::MvMapRemove(pb::MvMapRemove { object: notes.into(), key: k.into() });
        let mv_get = Op::MvMapEntries(pb::MvMapEntries { object: notes.into() });
        assert!(may("notes:read", mv_get.clone()));
        assert!(!may("tasks:write", mv_get));
        assert!(may("notes:write", mv_put("plan", "alice\u{1f}ship it")));
        assert!(!may("notes:read", mv_put("plan", "alice\u{1f}ship it")));
        assert!(!may("notes:write", mv_put("plan", "bob\u{1f}ship it")));
        assert!(!may("notes:write", mv_put("plan", "\u{1f}ship it")));
        assert!(!may("notes:write", mv_put("plan", "ship it")));
        assert!(may("notes:write", mv_remove("plan")));
        assert!(!may("notes:read", mv_remove("plan")));
        // Only the sidecar marks that the old notes were moved over.
        assert!(!may("notes:write", mv_put("", "alice\u{1f}moved")));
        assert!(!may("notes:write", mv_remove("")));
        // Any other change would make the notes another kind of object.
        assert!(!may("notes:write", put(notes, "plan", "alice\u{1f}ship it")));
        assert!(!may("notes:write", Op::MapRemove(pb::MapRemove { object: notes.into(), key: "plan".into() })));
        assert!(!may("notes:write", Op::CounterIncrement(pb::CounterIncrement { object: notes.into(), n: 1 })));
        assert!(!may("notes:write", set_add(notes)));
        // An agent that may write notes may list them in the team's list, as
        // the kind they are.
        assert!(may("notes:write", put("orvu/crew/index", "notes-mv", "mvmap")));
        assert!(!may("notes:read", put("orvu/crew/index", "notes-mv", "mvmap")));
        assert!(!may("notes:write", put("orvu/crew/index", "notes-mv", "map")));
    }

    // Notes kept the old way are moved over by the sidecar and used no more.
    // They can be read, and not changed: what was written there would be
    // moved over under the name already on it.
    #[test]
    fn notes_kept_the_old_way_are_only_read() {
        for old in ["orvu/crew/notes", "orvu/crew/notes-by"] {
            assert!(may("notes:read", entries(old)));
            assert!(!may("notes:write", put(old, "plan", "alice")));
            assert!(!may("notes:write", Op::MapRemove(pb::MapRemove { object: old.into(), key: "plan".into() })));
        }
        assert!(!may("notes:write", put("orvu/crew/index", "notes", "map")));
        assert!(!may("notes:write", put("orvu/crew/index", "notes-by", "map")));
        // Structured notes are written as before.
        assert!(may("notes:write", json_set("orvu/crew/note/plan")));
    }

    #[test]
    fn a_firm_tasks_tickets_go_with_the_tasks() {
        assert!(may("tasks:write", spend("orvu/crew/task-ticket/t-1/0")));
        assert!(may("tasks:write", top_up("orvu/crew/task-takeover/t-1/1")));
        assert!(!may("tasks:read budgets:top-up", spend("orvu/crew/task-ticket/t-1/0")));
    }

    #[test]
    fn budgets_are_read_spent_from_and_topped_up() {
        let budget = "orvu/crew/budget/tokens";
        assert!(may("budgets:read", Op::BoundedValue(pb::BoundedValue { object: budget.into() })));
        assert!(!may("budgets:read", spend(budget)));
        assert!(may("budgets:spend", spend(budget)));
        assert!(!may("budgets:spend", top_up(budget)));
        assert!(may("budgets:top-up", top_up(budget)));
        assert!(may("budgets:top-up", spend(budget)));
        // What was spent is written under one's own name only.
        assert!(may("budgets:spend", cart_add("orvu/crew/budget-spent/tokens", "alice")));
        assert!(!may("budgets:top-up", cart_add("orvu/crew/budget-spent/tokens", "bob")));
    }

    #[test]
    fn an_inbox_is_read_only_by_its_owner() {
        let others = "orvu/crew/inbox/bob/0";
        assert!(may("inbox:read", list_all("orvu/crew/inbox/alice/0")));
        assert!(may("inbox:read", list_all("orvu/crew/inbox/@everyone/3")));
        assert!(!may("inbox:write", list_all(others)));
        assert!(!may("inbox:write", Op::ListGet(pb::ListGet { object: others.into(), index: 0 })));
        assert!(!may("inbox:write", Op::Declare(pb::Declare { object: "orvu/crew/inbox-given/bob/bob/0".into(), object_class: 1 })));
        assert!(may("inbox:read", set_add("orvu/crew/inbox-given/alice/@everyone/0")));
        assert!(!may("inbox:write", set_add("orvu/crew/inbox-given/bob/bob/0")));
        assert!(!may("inbox:write", Op::SetElements(pb::SetElements { object: "orvu/crew/inbox-given/bob/bob/0".into() })));
    }

    // A sender learns which page an inbox is on and how full it is, and adds
    // to it, without reading what is in it.
    #[test]
    fn a_sender_sees_how_full_an_inbox_is_and_nothing_more() {
        assert!(may("inbox:write", counter_value("orvu/crew/inbox-page/bob")));
        assert!(may("inbox:write", list_len("orvu/crew/inbox/bob/0")));
        assert!(may("inbox:write", list_insert("orvu/crew/inbox/bob/0")));
        assert!(may("inbox:write", Op::CounterIncrement(pb::CounterIncrement { object: "orvu/crew/inbox-page/bob".into(), n: 1 })));
        assert!(!may("inbox:read", list_insert("orvu/crew/inbox/bob/0")));
        assert!(!may("inbox:read", list_len("orvu/crew/inbox/bob/0")));
        assert!(may("inbox:read", counter_value("orvu/crew/inbox-page/alice")));
        assert!(!may("inbox:write", Op::ListDelete(pb::ListDelete { object: "orvu/crew/inbox/bob/0".into(), index: 0 })));
    }

    #[test]
    fn an_agent_changes_only_its_own_place_on_the_roster() {
        let roster = "orvu/crew/roster";
        assert!(may("roster:write", put(roster, "alice\u{1f}0a1b2c3d", "1 15")));
        assert!(!may("roster:write", put(roster, "bob\u{1f}0a1b2c3d", "1 15")));
        assert!(!may("roster:write", put(roster, "alice-2\u{1f}0a1b2c3d", "1 15")));
        assert!(!may("roster:write", Op::MapRemove(pb::MapRemove { object: roster.into(), key: "bob\u{1f}00".into() })));
        assert!(!may("roster:read", put(roster, "alice\u{1f}0a1b2c3d", "1 15")));
        assert!(may("roster:read", entries(roster)));
    }

    // Anybody may read the team's list of what it has. A line goes into it
    // only for an object the agent may write.
    #[test]
    fn the_teams_list_follows_the_object_it_names() {
        let index = "orvu/crew/index";
        assert!(may("none", entries(index)));
        assert!(may("notes:write", put(index, "note/plan", "json")));
        assert!(!may("notes:read", put(index, "note/plan", "json")));
        assert!(may("budgets:spend", put(index, "budget-spent/tokens", "cart")));
        assert!(may("inbox:write", put(index, "inbox/bob/0", "list")));
        assert!(!may("inbox:write", put(index, "inbox-given/bob/bob/0", "set")));
        assert!(may("inbox:read", put(index, "inbox-given/alice/alice/0", "set")));
        assert!(!may("objects:write", put(index, "../../x", "map")));
        // Refused, it says why as a write of the object would.
        let why = check(&rules("notes:read"), "alice", &put(index, "note/plan", "json")).unwrap_err();
        assert_eq!(why, "alice may not change notes. Its rules are: notes:read");
        assert!(!may("notes:write", Op::MapRemove(pb::MapRemove { object: index.into(), key: "tasks".into() })));
    }

    // A firm task's tickets that can never be had again are taken off the
    // list, by an agent that may change tasks. Nothing else is.
    #[test]
    fn spent_tickets_are_taken_off_the_teams_list() {
        let index = "orvu/crew/index";
        let take = |key: &str| Op::MapRemove(pb::MapRemove { object: index.into(), key: key.into() });
        assert!(may("tasks:write", take("task-ticket/t-1/0")));
        assert!(may("tasks:write", take("task-takeover/t-1/3")));
        assert!(!may("tasks:read", take("task-ticket/t-1/0")));
        assert!(!may("notes:write budgets:top-up", take("task-takeover/t-1/0")));
        assert!(!may("tasks:write", take("tasks")));
        assert!(!may("tasks:write", take("tasks-tree")));
        assert!(!may("tasks:write", take("budget/tokens")));
        let why = check(&rules("tasks:read"), "alice", &take("task-ticket/t-1/0")).unwrap_err();
        assert_eq!(why, "alice may not change tasks. Its rules are: tasks:read");
    }

    #[test]
    fn plain_objects_have_a_rule_of_their_own() {
        assert!(may("objects:read", counter_value("visits")));
        assert!(!may("objects:read", Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 1 })));
        assert!(may("objects:write", Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 1 })));
        assert!(!may("notes:write", counter_value("visits")));
    }

    // A name whose team is empty, or not a name a team can have, belongs to
    // no team, and so to no block, wherever a name is looked at.
    #[test]
    fn a_name_with_no_proper_team_belongs_to_no_block() {
        assert_eq!(team_and_rest("orvu/crew/tasks"), Some(("crew", "tasks")));
        assert_eq!(team_and_rest("orvu/a.b_c-9/doc/x"), Some(("a.b_c-9", "doc/x")));
        let long = format!("orvu/{}/tasks", "a".repeat(65));
        for object in ["orvu//tasks", "orvu/.x/tasks", "orvu/-x/tasks", "orvu/a b/tasks", "orvu/é/tasks", "orvu/_machines/x", "orvu/crew", "tasks", &long] {
            assert_eq!(team_and_rest(object), None, "{object}");
        }
        for (words, op) in [
            ("tasks:write", json_set("orvu//tasks")),
            ("tasks:write", json_get("orvu//tasks")),
            ("notes:write", Op::MvMapRemove(pb::MvMapRemove { object: "orvu//notes-mv".into(), key: "plan".into() })),
            ("budgets:top-up", top_up("orvu//budget/b")),
        ] {
            assert!(check(&rules(words), "alice", &op).unwrap_err().contains("belongs to no building block"), "{op:?}");
        }
        assert!(crate::search::team_of_index("orvu//index").is_none());
        assert!(crate::notes::team_of("orvu//notes-mv").is_none());
    }

    // A name under orvu/ that no block uses is not a way round the rules.
    #[test]
    fn a_name_no_block_uses_is_refused() {
        assert!(!may("objects:write notes:write", counter_value("orvu/crew/secret")));
        assert!(!may("objects:write", counter_value("orvu/crew")));
        assert!(!may("objects:write", counter_value("orvu/")));
    }

    // A sender may only add a message to a page, and move an inbox on by one
    // page. Another kind of object under an inbox's name would stop anybody
    // sending to it, and a page number far ahead would stop its owner
    // reading it.
    #[test]
    fn a_sender_cannot_spoil_an_inbox() {
        let step = |object: &str, n| Op::CounterIncrement(pb::CounterIncrement { object: object.into(), n });
        assert!(!may("inbox:write", step("orvu/crew/inbox/bob/1", 1)));
        assert!(!may("inbox:write", list_insert("orvu/crew/inbox-page/bob")));
        assert!(!may("inbox:write", list_len("orvu/crew/inbox-page/bob")));
        assert!(!may("inbox:write", counter_value("orvu/crew/inbox/bob/0")));
        assert!(!may("inbox:write", step("orvu/crew/inbox-page/bob", 2)));
        assert!(may("inbox:write", step("orvu/crew/inbox-page/bob", 1)));
    }

    // A sender adds one to the count of the page it wrote to, and only the
    // owner reads the counts. What an agent was given from each page is its
    // own.
    #[test]
    fn an_inbox_counts_its_messages_for_its_owner() {
        let items = |object: &str| Op::CartItems(pb::CartItems { object: object.into() });
        let entries_put = |object: &str| put(object, "0", "2");
        assert!(may("inbox:write", cart_add("orvu/crew/inbox-count/bob", "0")));
        assert!(!may("inbox:read", cart_add("orvu/crew/inbox-count/bob", "0")));
        assert!(may("inbox:read", items("orvu/crew/inbox-count/alice")));
        assert!(may("inbox:read", items("orvu/crew/inbox-count/@everyone")));
        assert!(!may("inbox:write", items("orvu/crew/inbox-count/bob")));
        assert!(!may("inbox:write", Op::CartRemoveItem(pb::CartRemoveItem { object: "orvu/crew/inbox-count/bob".into(), item: "0".into() })));
        assert!(may("inbox:read", entries("orvu/crew/inbox-done/alice/@everyone")));
        assert!(may("inbox:read", entries_put("orvu/crew/inbox-done/alice/alice")));
        assert!(!may("inbox:write", entries("orvu/crew/inbox-done/bob/bob")));
        assert!(!may("inbox:write", entries_put("orvu/crew/inbox-done/bob/bob")));
        assert!(check_watch(&rules("inbox:read"), "alice", "orvu/crew/inbox-count/alice").is_ok());
        assert!(check_watch(&rules("inbox:write"), "alice", "orvu/crew/inbox-count/bob").is_err());
        assert!(may("inbox:write", put("orvu/crew/index", "inbox-count/bob", "cart")));
        assert!(!may("inbox:write", put("orvu/crew/index", "inbox-count/bob", "list")));
        assert!(may("inbox:read", put("orvu/crew/index", "inbox-done/alice/alice", "map")));
        assert!(!may("inbox:read", put("orvu/crew/index", "inbox-done/alice/alice", "set")));
        assert!(!may("inbox:write", put("orvu/crew/index", "inbox-done/bob/bob", "map")));
    }

    // A sender adds exactly one to a page's count. A bigger number would make
    // the count read wrong, and so hide messages or give them again.
    #[test]
    fn a_sender_adds_exactly_one_to_a_count() {
        let add = |qty| Op::CartAdd(pb::CartAdd { object: "orvu/crew/inbox-count/bob".into(), item: "0".into(), qty });
        assert!(may("inbox:write", add(1)));
        assert!(!may("inbox:write", add(2)));
        assert!(!may("inbox:write", add(1 << 63)));
        assert!(!may("inbox:write", add(0)));
        let why = check(&rules("inbox:write"), "alice", &add(1 << 63)).unwrap_err();
        assert!(why.contains("add anything but one to the count of an inbox page"), "{why}");
    }

    // The team's list is one object, `orvu/<team>/index`, and a line in it
    // names an object of that team, of the kind that object is.
    #[test]
    fn a_line_in_the_teams_list_is_checked_whole() {
        assert!(!may("notes:write inbox:write", put("orvu/c/index/ééé", "notes", "map")));
        assert!(!may("inbox:read", put("orvu/crew/index/a", "box-given/alice/x", "set")));
        assert!(!may("notes:write", put("orvu/crew/index", "notes", "counter")));
        assert!(!may("budgets:spend", put("orvu/crew/index", "budget/phantom", "bounded")));
        assert!(may("budgets:top-up", put("orvu/crew/index", "budget/tokens", "bounded")));
        assert!(may("budgets:spend", put("orvu/crew/index", "budget-spent/tokens", "cart")));
        assert!(may("inbox:write", put("orvu/crew/index", "inbox-page/bob", "counter")));
        assert!(!may("inbox:write", put("orvu/crew/index", "inbox-page/bob", "list")));
        // What the agent wrote is not repeated in the reason, which is
        // recorded.
        let why = check(&rules("notes:write"), "alice", &put("orvu/crew/index", "notes", "the launch code")).unwrap_err();
        assert!(!why.contains("launch code"), "{why}");
    }

    // Who topped up a budget is written under one's own name only, by an
    // agent that may top up, and only ever added to.
    #[test]
    fn who_topped_up_is_written_under_ones_own_name() {
        let topped = "orvu/crew/budget-topped/tokens";
        assert!(may("budgets:top-up", cart_add(topped, "alice")));
        assert!(!may("budgets:spend", cart_add(topped, "alice")));
        assert!(!may("budgets:top-up", cart_add(topped, "bob")));
        assert!(may("budgets:read", Op::CartItems(pb::CartItems { object: topped.into() })));
        assert!(!may("notes:write", Op::CartItems(pb::CartItems { object: topped.into() })));
        assert!(!may("budgets:top-up", Op::CartRemoveItem(pb::CartRemoveItem { object: topped.into(), item: "alice".into() })));
        assert!(!may("budgets:top-up", Op::CartRemoveQty(pb::CartRemoveQty { object: topped.into(), item: "alice".into(), qty: 1 })));
        assert!(!may("budgets:top-up", set_add(topped)));
        assert!(may("budgets:top-up", put("orvu/crew/index", "budget-topped/tokens", "cart")));
        assert!(!may("budgets:spend", put("orvu/crew/index", "budget-topped/tokens", "cart")));
        assert!(!may("budgets:top-up", put("orvu/crew/index", "budget-topped/tokens", "map")));
        let why = check(&rules("budgets:top-up"), "alice", &cart_add(topped, "bob")).unwrap_err();
        assert!(why.contains("write another agent's name on who topped up a budget"), "{why}");
    }

    // A top-up goes with its line of who topped up, for the same budget and
    // amount, in the same request, and the line only with its top-up.
    #[test]
    fn a_top_up_and_its_line_go_together() {
        let inc = |name: &str, n| Op::BoundedIncrement(pb::BoundedIncrement { object: format!("orvu/crew/budget/{name}"), n });
        let line = |name: &str, qty| {
            Op::CartAdd(pb::CartAdd { object: format!("orvu/crew/budget-topped/{name}"), item: "alice".into(), qty })
        };
        let together = |ops: &[Op]| check_together("alice", &ops.iter().map(Some).collect::<Vec<_>>()).map_err(|(i, _)| i);
        assert_eq!(together(&[inc("tokens", 5), line("tokens", 5)]), Ok(()));
        assert_eq!(together(&[line("tokens", 5), inc("tokens", 5)]), Ok(()));
        assert_eq!(together(&[inc("tokens", 5)]), Err(0));
        assert_eq!(together(&[line("tokens", 5)]), Err(0));
        assert_eq!(together(&[inc("tokens", 5), line("tokens", 6)]), Err(0));
        assert_eq!(together(&[inc("tokens", 5), line("money", 5)]), Err(0));
        assert_eq!(together(&[inc("tokens", 5), line("tokens", 5), line("tokens", 5)]), Err(2));
        assert_eq!(together(&[inc("tokens", 5), inc("tokens", 5), line("tokens", 5)]), Err(1));
        // Tickets, transfers and other counters go their own way.
        let ticket = Op::BoundedIncrement(pb::BoundedIncrement { object: "orvu/crew/task-ticket/t/0".into(), n: 1 });
        let transfer = Op::BoundedTransfer(pb::BoundedTransfer { object: "orvu/crew/budget/tokens".into(), ..Default::default() });
        assert_eq!(together(&[ticket, transfer]), Ok(()));
    }

    // What was spent is only ever added to, under one's own name, whatever
    // the agent's level.
    #[test]
    fn what_was_spent_is_only_added_to() {
        let spent = "orvu/crew/budget-spent/tokens";
        assert!(!may("budgets:top-up", Op::CartRemoveItem(pb::CartRemoveItem { object: spent.into(), item: "bob".into() })));
        assert!(!may("budgets:top-up", Op::CartRemoveQty(pb::CartRemoveQty { object: spent.into(), item: "alice".into(), qty: 1 })));
    }

    // Every object of a block is changed only as the kind of object it is:
    // a change of another kind would make it one, and the block could not
    // use it any more.
    #[test]
    fn a_blocks_object_is_changed_only_as_its_own_kind() {
        let counter = |object: &str| Op::CounterIncrement(pb::CounterIncrement { object: object.into(), n: 1 });
        let declare = |object: &str| Op::Declare(pb::Declare { object: object.into(), object_class: 2 });
        for (words, op) in [
            ("tasks:write", counter("orvu/crew/tasks")),
            ("tasks:write", json_set("orvu/crew/tasks-tree")),
            ("tasks:write", counter("orvu/crew/task-ticket/t/0")),
            ("roster:write", Op::MvMapPut(pb::MvMapPut { object: "orvu/crew/roster".into(), key: "alice\u{1f}a".into(), value: "1".into() })),
            ("roster:write", json_set("orvu/crew/roster")),
            ("documents:write", Op::RegisterSet(pb::RegisterSet { object: "orvu/crew/doc/d".into(), value: "x".into() })),
            ("tallies:write", counter("orvu/crew/tally/t")),
            ("budgets:top-up", declare("orvu/crew/budget/b")),
            ("budgets:top-up", counter("orvu/crew/budget/b")),
            ("budgets:spend", json_set("orvu/crew/budget-spent/b")),
            ("inbox:write", set_add("orvu/crew/inbox/bob/0")),
            ("notes:write", json_set("orvu/crew/notes-mv")),
            ("notes:write", counter("orvu/crew/note/plan")),
        ] {
            let why = check(&rules(words), "alice", &op).unwrap_err();
            assert!(why.contains("another kind of object"), "{words} {op:?}: {why}");
        }
        // Reads of another kind change nothing, and are left to the engine.
        assert!(may("tasks:read", counter_value("orvu/crew/tasks")));
        // Its own kind is taken as before.
        assert!(may("roster:write", put("orvu/crew/roster", "alice\u{1f}a", "1")));
        assert!(may("tallies:write", cart_add("orvu/crew/tally/t", "alice")));
    }

    // A refusal names what the agent lacks, not something else.
    #[test]
    fn a_refusal_about_ones_own_inbox_says_what_is_missing() {
        let why = check(&rules("notes:read"), "alice", &list_all("orvu/crew/inbox/alice/0")).unwrap_err();
        assert!(why.contains("read its inbox, which needs inbox:read"), "{why}");
        let why = check(&rules("notes:read"), "alice", &list_len("orvu/crew/inbox/bob/0")).unwrap_err();
        assert!(why.contains("send messages, which needs inbox:write"), "{why}");
        let take = Op::CartRemoveQty(pb::CartRemoveQty { object: "orvu/crew/budget-spent/tokens".into(), item: "alice".into(), qty: 1 });
        let why = check(&rules("budgets:spend"), "alice", &take).unwrap_err();
        assert!(why.contains("only ever added to"), "{why}");
    }

    fn board_set(path: &str, value: &str) -> Op {
        Op::JsonSet(pb::JsonSet { object: "orvu/crew/tasks".into(), path_json: path.into(), value_json: value.into() })
    }
    fn message(from: &str) -> Op {
        let value = format!(r#"{{"id":"m-1","from":{from},"to":"bob","body":"hi","sent":1}}"#);
        Op::ListInsert(pb::ListInsert { object: "orvu/crew/inbox/bob/0".into(), index: 0, value })
    }

    // A message carries the name of the agent that sent it, which may only be
    // the agent's own.
    #[test]
    fn a_message_carries_its_senders_own_name() {
        assert!(may("inbox:write", message(r#""alice""#)));
        assert!(!may("inbox:write", message(r#""bob""#)));
        assert!(!may("inbox:write", message("null")));
        assert!(!may("inbox:write", message(r#"["alice"]"#)));
        let plain = Op::ListInsert(pb::ListInsert { object: "orvu/crew/inbox/bob/0".into(), index: 0, value: "hi".into() });
        assert!(!may("inbox:write", plain));
        let no_name = Op::ListInsert(pb::ListInsert {
            object: "orvu/crew/inbox/@everyone/0".into(),
            index: 0,
            value: r#"{"id":"m-1","body":"hi","sent":1}"#.into(),
        });
        assert!(!may("inbox:write", no_name));
        let why = check(&rules("inbox:write"), "alice", &message(r#""bob""#)).unwrap_err();
        assert!(why.contains("send a message under another name than its own"), "{why}");
        // Without inbox:write, it is the sending that is refused.
        let why = check(&rules("inbox:read"), "alice", &message(r#""alice""#)).unwrap_err();
        assert!(why.contains("send messages, which needs inbox:write"), "{why}");
        // The line in the team's list is still taken.
        assert!(may("inbox:write", put("orvu/crew/index", "inbox/bob/0", "list")));
    }

    // A task carries the name of the agent that added it, which may only be
    // the agent's own.
    #[test]
    fn a_task_carries_the_name_of_the_agent_that_added_it() {
        assert!(may("tasks:write", board_set(r#"["t-1"]"#, r#"{"title":"x","created_by":"alice"}"#)));
        assert!(!may("tasks:write", board_set(r#"["t-1"]"#, r#"{"title":"x","created_by":"bob"}"#)));
        assert!(!may("tasks:write", board_set(r#"["t-1"]"#, r#"{"title":"x"}"#)));
        assert!(!may("tasks:write", board_set(r#"["t-1"]"#, r#""x""#)));
        assert!(may("tasks:write", board_set(r#"["t-1","created_by"]"#, r#""alice""#)));
        assert!(!may("tasks:write", board_set(r#"["t-1","created_by"]"#, r#""bob""#)));
        assert!(!may("tasks:write", board_set(r#"["t-1","created_by","x"]"#, r#""alice""#)));
        // The board is written one task at a time, never whole.
        assert!(!may("tasks:write", board_set("[]", r#"{"t-1":{"title":"x","created_by":"alice"}}"#)));
        assert!(!may("tasks:write", board_set("[0]", r#""x""#)));
        assert!(!may("tasks:write", board_set(r#"["t-1",0]"#, r#""x""#)));
        assert!(!may("tasks:write", board_set("not a path", r#""x""#)));
        assert!(!may("tasks:write", Op::JsonRemove(pb::JsonRemove { object: "orvu/crew/tasks".into(), path_json: "[0]".into() })));
        assert!(!may("tasks:write", board_set("[]", r#"{"t-1":{"title":"x","created_by":"bob"}}"#)));
        // A name kept twice is read as the engine keeps it.
        assert!(!may("tasks:write", board_set(r#"["t-1"]"#, r#"{"created_by":"alice","created_by":"bob"}"#)));
        let insert = |path: &str| {
            Op::JsonInsert(pb::JsonInsert { object: "orvu/crew/tasks".into(), path_json: path.into(), index: 0, value_json: r#""bob""#.into() })
        };
        let remove = |path: &str| Op::JsonRemove(pb::JsonRemove { object: "orvu/crew/tasks".into(), path_json: path.into() });
        assert!(!may("tasks:write", insert(r#"["t-1","created_by"]"#)));
        assert!(!may("tasks:write", insert(r#"["t-1"]"#)));
        assert!(!may("tasks:write", remove(r#"["t-1","created_by"]"#)));
        // What does not name the agent that added it is as it was.
        assert!(may("tasks:write", board_set(r#"["t-1","title"]"#, r#""y""#)));
        assert!(may("tasks:write", board_set(r#"["t-1","claimed_by"]"#, r#""bob""#)));
        assert!(may("tasks:write", remove(r#"["t-1"]"#)));
        assert!(may("tasks:write", insert(r#"["t-1","tags"]"#)));
        let why = check(&rules("tasks:write"), "alice", &board_set(r#"["t-1"]"#, r#"{"created_by":"bob"}"#)).unwrap_err();
        assert!(why.contains("add a task under another name than its own"), "{why}");
        assert!(!may("tasks:read", board_set(r#"["t-1"]"#, r#"{"title":"x","created_by":"alice"}"#)));
    }

    #[test]
    fn notes_and_tasks_take_an_own_level() {
        assert_eq!(rules("tasks:own notes:own").to_string(), "notes:own tasks:own");
        for words in [vec!["documents:own"], vec!["inbox:own"], vec!["objects:own"]] {
            assert!(Rules::parse(&words).unwrap_err().contains("is not a rule"), "{words:?}");
        }
        assert!(Rules::parse(&["budgets:own"]).unwrap_err().contains("budgets take read, spend or top-up"));
        assert!(Rules::parse(&["notes:own", "notes:write"]).unwrap_err().contains("given twice"));
    }

    // With notes:own an agent reads every note and writes simple notes under
    // its own name; whose a note is now is left to its guard. It changes no
    // structured note, as those keep no writer's name.
    #[test]
    fn notes_own_writes_under_ones_own_name() {
        let notes = "orvu/crew/notes-mv";
        let mv_put = |v: &str| Op::MvMapPut(pb::MvMapPut { object: notes.into(), key: "plan".into(), value: v.into() });
        let mv_remove = Op::MvMapRemove(pb::MvMapRemove { object: notes.into(), key: "plan".into() });
        assert!(may("notes:own", Op::MvMapEntries(pb::MvMapEntries { object: notes.into() })));
        assert!(may("notes:own", json_get("orvu/crew/note/plan")));
        assert!(may("notes:own", mv_put("alice\u{1f}ship it")));
        assert!(!may("notes:own", mv_put("bob\u{1f}ship it")));
        assert!(may("notes:own", mv_remove.clone()));
        assert!(!may("notes:read", mv_remove));
        assert!(!may("notes:own", json_set("orvu/crew/note/plan")));
        let why = check(&rules("notes:own"), "alice", &json_set("orvu/crew/note/plan")).unwrap_err();
        assert!(why.contains("structured notes, which keep no writer's name"), "{why}");
        assert!(may("notes:own", put("orvu/crew/index", "notes-mv", "mvmap")));
        assert!(!may("notes:own", put("orvu/crew/index", "note/plan", "json")));
        assert!(may("notes:own", Op::Search(pb::Search { object: "orvu/crew/index".into(), ..Default::default() })));
    }

    const BOARD: &str = "orvu/crew/tasks";

    fn guard(words: &str, claims: &[&str]) -> Guard {
        let rules = rules(words);
        Guard {
            agent: "alice".into(),
            own_tasks: rules.level("tasks") == OWN,
            own_notes: rules.level("notes") == OWN,
            rules: rules.to_string(),
            claims: Arc::new(claims.iter().map(|id| (BOARD.to_string(), id.to_string())).collect()),
        }
    }
    fn path(text: &str) -> Vec<Seg> {
        parse_path(text).unwrap()
    }
    fn value(text: &str) -> JsonValue {
        JsonValue::from_json_str(text).unwrap()
    }

    // A note may be changed under notes:own only while every value it holds
    // is the agent's own.
    #[test]
    fn a_notes_guard_goes_by_who_wrote_each_value() {
        let own = guard("notes:own", &[]);
        assert!(own.note(&[]).is_ok());
        assert!(own.note(&["alice\u{1f}a".into(), "alice\u{1f}b".into()]).is_ok());
        assert!(own.note(&["alice\u{1f}a".into(), "bob\u{1f}b".into()]).is_err());
        assert!(own.note(&["\u{1f}a".into()]).is_err());
        assert!(own.note(&["alice".into()]).is_err());
        assert!(guard("notes:write", &[]).note(&["bob\u{1f}b".into()]).is_ok());
    }

    // Under tasks:own a task another agent holds is not touched, an open one
    // is claimed, and the rest has to be the agent's own; as a write is made,
    // a task it was given back in between is still its own to finish with.
    #[test]
    fn a_task_guard_goes_by_who_added_and_who_holds_the_task() {
        let task = |by: &str, holder: Option<&str>| {
            let mut fields = format!(r#"{{"title":"x","state":"todo","created_by":"{by}""#);
            if let Some(holder) = holder {
                fields += &format!(r#","claimed_by":"{holder}","state":"doing""#);
            }
            value(&(fields + "}"))
        };
        let own = guard("tasks:own", &[]);
        let check = |g: &Guard, p: &str, v: Option<&str>, now: Option<&JsonValue>, first: bool| {
            g.board(BOARD, &path(p), v.map(value).as_ref(), false, now, first)
        };
        let (mine, held_by_me, theirs, held, open) =
            (task("alice", None), task("bob", Some("alice")), task("bob", Some("bob")), task("alice", Some("bob")), task("bob", None));
        // Adding a task, whole, as itself.
        assert!(check(&own, r#"["t"]"#, Some(r#"{"created_by":"alice"}"#), None, true).is_ok());
        assert!(check(&own, r#"["t","title"]"#, Some(r#""x""#), None, true).is_err());
        // Its own, added or held.
        for task in [&mine, &held_by_me] {
            assert!(check(&own, r#"["t","title"]"#, Some(r#""y""#), Some(task), true).is_ok());
            assert!(check(&own, r#"["t","finished"]"#, Some(r#"{"by":"alice"}"#), Some(task), true).is_ok());
            assert!(check(&own, r#"["t","claimed_by"]"#, None, Some(task), true).is_ok());
        }
        assert!(check(&own, r#"["t"]"#, None, Some(&mine), true).is_ok());
        assert!(check(&own, r#"["t"]"#, None, Some(&held_by_me), true).is_err(), "removes only what it added");
        // Held by another: nothing, whoever added it.
        for task in [&theirs, &held] {
            for (p, v) in [(r#"["t","title"]"#, Some(r#""y""#)), (r#"["t","seen"]"#, Some("1")), (r#"["t"]"#, None)] {
                assert!(check(&own, p, v, Some(task), true).is_err(), "{p}");
                assert!(check(&own, p, v, Some(task), false).is_err(), "{p}");
            }
        }
        // Open, another's: only what a claim writes, and only claimed by this
        // request under its own name.
        let claiming = guard("tasks:own", &["t"]);
        for field in CLAIM_FIELDS {
            let v = if field == "claimed_by" { r#""alice""# } else { "1" };
            assert!(check(&claiming, &format!(r#"["t",{field:?}]"#), Some(v), Some(&open), true).is_ok(), "{field}");
            assert!(check(&own, &format!(r#"["t",{field:?}]"#), Some(v), Some(&open), true).is_err(), "{field}");
        }
        assert!(check(&claiming, r#"["t","claimed_by"]"#, Some(r#""bob""#), Some(&open), true).is_err());
        assert!(check(&claiming, r#"["t","title"]"#, Some(r#""y""#), Some(&open), true).is_err());
        assert!(check(&claiming, r#"["t","finished"]"#, Some(r#"{"by":"alice"}"#), Some(&open), true).is_err());
        let done = value(r#"{"title":"x","state":"done","created_by":"bob"}"#);
        assert!(check(&claiming, r#"["t","state"]"#, Some(r#""doing""#), Some(&done), true).is_err());
        // A claim on another board opens nothing here.
        let elsewhere = Guard { claims: Arc::new([("orvu/other/tasks".to_string(), "t".to_string())].into()), ..own.clone() };
        assert!(check(&elsewhere, r#"["t","state"]"#, Some(r#""done""#), Some(&open), true).is_err());
        // As the write is made: on an open task only the claim itself, and
        // the rest once the task is the agent's.
        for g in [&own, &claiming] {
            assert!(check(g, r#"["t","claimed_by"]"#, Some(r#""alice""#), Some(&open), false).is_ok());
            assert!(check(g, r#"["t","claimed_by"]"#, Some(r#""bob""#), Some(&open), false).is_err());
            for field in ["seen", "lease", "ticket", "state"] {
                assert!(check(g, &format!(r#"["t",{field:?}]"#), Some("1"), Some(&open), false).is_err(), "{field}");
                assert!(check(g, &format!(r#"["t",{field:?}]"#), Some("1"), Some(&held_by_me), false).is_ok(), "{field}");
            }
            assert!(check(g, r#"["t","claimed_by"]"#, Some(r#""alice""#), Some(&done), false).is_err());
        }
        // The whole board holds others' tasks.
        assert!(check(&own, "[]", Some("{}"), Some(&value("{}")), true).is_err());
        // The tree: its own tasks, and one not on the board yet.
        assert!(own.tree(None).is_ok());
        assert!(own.tree(Some(&mine)).is_ok());
        assert!(own.tree(Some(&held_by_me)).is_err());
        assert!(guard("tasks:write", &[]).tree(Some(&theirs)).is_ok());
    }

    // Who added a task never changes, whatever the agent's level.
    #[test]
    fn the_name_of_the_agent_that_added_a_task_does_not_change() {
        let write = guard("tasks:write", &[]);
        let theirs = value(r#"{"title":"x","created_by":"bob"}"#);
        let check = |p: &str, v: Option<&str>, now: Option<&JsonValue>| write.board(BOARD, &path(p), v.map(value).as_ref(), false, now, true);
        assert!(check(r#"["t"]"#, Some(r#"{"created_by":"alice"}"#), Some(&theirs)).is_err());
        assert!(check(r#"["t"]"#, Some(r#"{"created_by":"bob","title":"y"}"#), Some(&theirs)).is_ok());
        assert!(check(r#"["t","created_by"]"#, Some(r#""alice""#), Some(&theirs)).is_err());
        assert!(check(r#"["t","created_by"]"#, Some(r#""bob""#), Some(&theirs)).is_ok());
        assert!(check(r#"["t","title"]"#, Some(r#""y""#), Some(&theirs)).is_ok());
        assert!(check(r#"["t"]"#, None, Some(&theirs)).is_ok());
        let board = value(r#"{"t":{"created_by":"bob"}}"#);
        assert!(check("[]", Some(r#"{"t":{"created_by":"alice"}}"#), Some(&board)).is_err());
        assert!(check("[]", Some(r#"{"t":{"created_by":"bob"},"u":{"created_by":"alice"}}"#), Some(&board)).is_err());
        assert!(check("[0]", Some(r#""x""#), Some(&board)).is_err());
        assert!(check("[0]", None, Some(&board)).is_err());
    }

    // With tasks:own an agent reads every task and changes the board; whose
    // a task is now is left to its guard. A firm task's tickets need
    // tasks:write.
    #[test]
    fn tasks_own_changes_the_board_through_its_guard() {
        let tree = |op: fn(String) -> Op| op("orvu/crew/tasks-tree".into());
        assert!(may("tasks:own", json_get("orvu/crew/tasks")));
        assert!(may("tasks:own", board_set(r#"["t-1"]"#, r#"{"title":"x","created_by":"alice"}"#)));
        assert!(!may("tasks:own", board_set(r#"["t-1"]"#, r#"{"title":"x","created_by":"bob"}"#)));
        assert!(may("tasks:own", board_set(r#"["t-1","title"]"#, r#""y""#)));
        assert!(!may("tasks:read", board_set(r#"["t-1","title"]"#, r#""y""#)));
        assert!(may("tasks:own", tree(|object| Op::TreeMoveNode(pb::TreeMoveNode { object, child: "t-1".into(), new_parent: "root".into() }))));
        assert!(!may("tasks:own", tree(|object| Op::TreeSetLabel(pb::TreeSetLabel { object, ..Default::default() }))));
        assert!(!may("tasks:read", tree(|object| Op::TreeMoveNode(pb::TreeMoveNode { object, ..Default::default() }))));
        assert!(!may("tasks:own", spend("orvu/crew/task-ticket/t-1/0")));
        assert!(may("tasks:own", put("orvu/crew/index", "tasks", "json")));
        assert!(may("tasks:own", put("orvu/crew/index", "tasks-tree", "tree")));
        assert!(!may("tasks:own", put("orvu/crew/index", "task-ticket/t-1/0", "bounded")));
        assert!(!may("tasks:read", put("orvu/crew/index", "tasks", "json")));
        assert!(may("tasks:own", Op::Search(pb::Search { object: "orvu/crew/index".into(), ..Default::default() })));
    }

    #[test]
    fn a_refusal_says_who_what_and_the_rules() {
        let why = check(&rules("notes:read"), "alice", &json_set("orvu/crew/note/plan")).unwrap_err();
        assert_eq!(why, "alice may not change notes. Its rules are: notes:read");
    }
}
