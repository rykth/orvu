# orvu-crdt

Data types that several machines can change at the same time and still agree on.

Each machine keeps its own copy and edits it freely, with no waiting and no
locks. Every edit produces a small record of what changed, called a *delta*.
Send that delta to the other machines, they merge it in, and everyone ends up
with the same value. Deltas can arrive late, out of order, or twice, and the
result is still the same.

This module is only the data types. It does no networking and saves nothing to
disk. Other Orvu modules do that.

## What is inside

| Type | What it holds | When two machines disagree |
|---|---|---|
| `PnCounter` | A number you add to and subtract from | Both changes count |
| `BoundedCounter` | A number that can never go below zero | Each machine can only spend its own share |
| `LwwRegister` | One value | The later write wins |
| `OrSet` | A set of unique items | Adding beats a removal made at the same time |
| `OrMap` | Keys pointing to other mergeable values | Values merge; adding a key beats removing it |
| `MvMap` | Keys pointing to text | Both writes are kept until a later write replaces them |
| `Rga` | An ordered list | Both inserts are kept, in the same order everywhere |
| `RichText` | Text with formatting | All typing is kept; the later format wins |
| `JsonCrdt` | A JSON document | Different fields merge; for the same field the later write wins, a whole list or object included; replacing or removing something drops what was written inside it at the same time |
| `Tree` | Items nested under other items | Moves that would create a loop are skipped |

Supporting pieces:

- `ReplicaId`: a number that identifies one machine. Every machine needs its
  own.
- `Hlc`: a clock that produces timestamps. Types that pick a "later" write use
  these to decide which one is later.
- `DeltaCrdt`: what all the types have in common, `merge` and `snapshot`.
- `WriteError`: why a write was refused. A refused write changes nothing.
  `TooManyWriters` is never returned by the types here: the engine returns
  it when an object already holds changes from the most machines it takes
  and none from the one writing.
- `RichText::insert_marked` puts text in and sets a mark on exactly those
  characters as one delta. The mark is tied to the characters' own ids, so
  text another machine puts in at the same place never takes it.
- `MAX_COUNTER`: the highest counter a dot may have, 2^62. Writes here stop
  at it, and `orvu-core` refuses anything higher when it reads bytes.

## How to use it

### A counter on two machines

```rust
use orvu_crdt::{DeltaCrdt, PnCounter, ReplicaId};

let (alice, bob) = (ReplicaId(1), ReplicaId(2));
let mut on_alice = PnCounter::default();
let mut on_bob = PnCounter::default();

// Each machine changes its own copy and gets a delta back.
let from_alice = on_alice.increment(alice, 5);
let from_bob = on_bob.decrement(bob, 2);

// Send each delta to the other machine and merge it in.
on_bob.merge(from_alice);
on_alice.merge(from_bob);

assert_eq!(on_alice.value(), 3);
assert_eq!(on_bob.value(), 3);
```

### Merging the same delta twice is harmless

```rust
use orvu_crdt::{DeltaCrdt, PnCounter, ReplicaId};

let mut sender = PnCounter::default();
let mut receiver = PnCounter::default();

let delta = sender.increment(ReplicaId(1), 5);
receiver.merge(delta.clone());
receiver.merge(delta);

assert_eq!(receiver.value(), 5);
```

### A set, where adding beats removing

```rust
use orvu_crdt::{DeltaCrdt, OrSet, ReplicaId};

let mut on_alice: OrSet<String> = OrSet::default();
let mut on_bob: OrSet<String> = OrSet::default();

let added = on_alice.add("milk".to_string(), ReplicaId(1)).unwrap();
on_bob.merge(added);

// At the same moment, Alice removes it and Bob adds it again.
let removed = on_alice.remove(&"milk".to_string());
let added_again = on_bob.add("milk".to_string(), ReplicaId(2)).unwrap();

on_alice.merge(added_again);
on_bob.merge(removed);

// Bob's add was not seen by Alice's remove, so it survives on both.
assert!(on_alice.contains(&"milk".to_string()));
assert!(on_bob.contains(&"milk".to_string()));
```

### A single value, where the later write wins

Types that choose a "later" write need a timestamp. Keep one `Hlc` per machine
and call `tick` with the current time in milliseconds for each write.

```rust
use orvu_crdt::{Hlc, LwwRegister, ReplicaId};

let me = ReplicaId(1);
let mut clock = Hlc::ZERO;
let mut title: LwwRegister<String> = LwwRegister::default();

title.set("Draft".to_string(), clock.tick(1_000), me);
title.set("Final".to_string(), clock.tick(1_000), me);

assert_eq!(title.get(), Some(&"Final".to_string()));
```

### A budget that cannot be overspent

```rust
use orvu_crdt::{BoundedCounter, ReplicaId};

let (alice, bob) = (ReplicaId(1), ReplicaId(2));
let mut budget = BoundedCounter::default();

// Alice puts in 10. That whole share is hers to spend.
budget.increment(10, alice);
assert!(budget.try_decrement(4, alice).is_some());

// Bob has no share yet, so he is refused.
assert!(budget.try_decrement(1, bob).is_none());

// Alice hands 3 of her share to Bob. Now he can spend it.
budget.transfer(bob, 3, alice).expect("Alice has 6 left");
assert!(budget.try_decrement(3, bob).is_some());

assert_eq!(budget.value(), 3);
assert_eq!(budget.allowance(alice), 3);
assert_eq!(budget.allowance(bob), 0);
// What others ever handed Bob, whatever he has spent since. A transfer
// a machine makes to itself does not count.
assert_eq!(budget.received(bob), 3);
```

### A JSON document

```rust
use orvu_crdt::{Hlc, JsonCrdt, JsonValue, ReplicaId, Seg};

let me = ReplicaId(1);
let mut clock = Hlc::ZERO;
let mut doc = JsonCrdt::default();

let title = [Seg::Key("title".to_string())];
let done = [Seg::Key("done".to_string())];

doc.set(&title, &JsonValue::Str("Plan".to_string()), clock.tick(1), me).unwrap();
doc.set(&done, &JsonValue::Bool(false), clock.tick(2), me).unwrap();

assert_eq!(doc.get(&title), Some(JsonValue::Str("Plan".to_string())));
assert_eq!(doc.get(&done), Some(JsonValue::Bool(false)));
```

### Writes made at the same time are both kept

```rust
use orvu_crdt::{DeltaCrdt, MvMap, ReplicaId};

let mut on_alice: MvMap<String, String> = MvMap::default();
let mut on_bob: MvMap<String, String> = MvMap::default();

// Both write the same key before either has seen the other's write.
let from_alice = on_alice.put("plan".into(), "ship it".into(), ReplicaId(1)).unwrap();
let from_bob = on_bob.put("plan".into(), "wait".into(), ReplicaId(2)).unwrap();
on_alice.merge(from_bob);
on_bob.merge(from_alice);

// Both values are kept, so a reader can see there was a clash.
assert_eq!(on_alice.get(&"plan".to_string()), vec!["ship it", "wait"]);

// A write made after seeing both replaces them.
let settled = on_alice.put("plan".into(), "ship friday".into(), ReplicaId(1)).unwrap();
on_bob.merge(settled);
assert_eq!(on_bob.get(&"plan".to_string()), vec!["ship friday"]);
```

### A tree

```rust
use orvu_crdt::{Hlc, ReplicaId, Tree};

let me = ReplicaId(1);
let mut clock = Hlc::ZERO;
let mut tree = Tree::default();

tree.move_node("report".to_string(), "root".to_string(), clock.tick(1), me);
tree.move_node("intro".to_string(), "report".to_string(), clock.tick(2), me);
tree.set_label("intro".to_string(), "Introduction".to_string(), clock.tick(3), me).unwrap();

assert_eq!(tree.children("report"), vec!["intro".to_string()]);
assert_eq!(tree.label("intro"), Some("Introduction".to_string()));

// Putting "report" under its own child would make a loop, so it is skipped.
tree.move_node("report".to_string(), "intro".to_string(), clock.tick(4), me);
assert_eq!(tree.parent("report"), Some(&"root".to_string()));
```

## Things to know

- **Every machine needs its own `ReplicaId`.** Two machines sharing one can
  lose each other's changes.
- **Timestamps must keep moving forward.** Use one `Hlc` per machine and keep
  it for as long as the machine runs. When a delta arrives from elsewhere, pass
  its timestamp to `Hlc::update` so your clock never falls behind. `update`
  takes any timestamp, and the clock never goes back, so check one from
  another machine with `lead_over` first and turn it away if it is too far
  ahead of your wall clock.
- **Every delta has to get there eventually.** Order and repeats do not matter,
  but a delta that is never delivered is a change the other machine never sees.
- **A new machine catches up with `snapshot`.** It returns one delta holding
  everything. Merge it into an empty value to get an identical copy.
- **A change to a set or a map looks only at what it touches.** Each keeps a
  table of which key every dot tags, so that a change, a removal included,
  finds its keys without going through all of them. Merging a whole
  `snapshot` still goes through everything, as it has to. The table takes
  memory: about one more copy of each key.
- **Removed items leave a small record behind.** Sets, maps, lists and text
  remember what was removed so that a late-arriving delta cannot bring it back.
  This module does not clean those records up.
- **Two inserts at the same spot are both kept.** An insert lands exactly
  where you asked on your own machine. If another machine inserted at that same
  spot before seeing yours, a fixed rule decides which of the two comes first,
  and every machine ends up with the same order. The same applies to text and
  to lists inside a JSON document.
- **A JSON document can nest 32 levels deep.** A `set` or `insert` that would
  go deeper returns `TooDeep`. A deeper document could not be sent to another
  machine.
- **Setting a JSON field replaces it.** Writing a list or an object to a field
  drops what the field held before. To add to a list, use `insert`.
- **A whole list or object written at the same moment on two machines is not
  joined.** Every machine ends up with one of the two, picked as for any
  last-writer-wins value: the later timestamp, then the higher machine id.
- **Replacing or removing an object or list starts it afresh.** A change made
  at the same time inside what was replaced is dropped. What was replaced
  never comes back. Two machines that write into a field that was empty,
  removed or replaced, starting from the same state, both keep their writes.
  So a whole write or a removal wins over a write made at the same time inside
  the old value, even when that write has the later timestamp, and a write
  that has to create the place it writes into loses to a whole write or
  removal of that place made at the same time. Two removals, or two whole
  writes, of the same place are two replacements, and the later one wins with
  everything written after it.
- **What was overwritten or removed in a JSON document stays gone.** A read
  of a path inside it finds nothing, and a later write inside it starts from
  empty. Inside, each node keeps every old part out of sight by generation
  rather than throwing it away, because a machine that never saw a part could
  not agree on what was thrown away. Those old parts take memory and are never
  cleaned up. A removed field is kept too, marked as gone. They are not sent
  again: the delta of a write holds only what the write added, so writing the
  same object a thousand times sends a delta the size of that object each
  time. Old parts could only be dropped once every machine is known to have
  the write that replaced them, and nothing here knows that, so they stay.
- **A write that has to start a place afresh, and cannot, is refused.** Each
  fresh start of the same place counts one step up, and a step count at its
  very top would leave no room for another. Such a write returns
  `NoFreshStart` and changes nothing. A real document never gets near it.
- **`insert` only goes into a list, or where nothing or `null` is.** Where an
  object, a text, a number or a true/false is, it returns `NotAList` and
  changes nothing. Removing something under a key that is not there does
  nothing and is not an error.
- **A JSON number is a 64-bit float.** The parser refuses a whole number,
  written without a point or an exponent, that a float cannot hold exactly,
  such as 2^53 + 1, because a float would round it. One that a float does
  hold, such as 10^16 or 2^60 written out in full, is read as it is. The
  writer puts a whole float below 10^21 in plain digits, all of them exact,
  such as `1152921504606846976` for 2^60, and one from 10^21 on with an
  exponent (`1.5e300`). Either way it reads back the same, and a reader that
  tells whole numbers from floats reads a whole number. Negative zero is
  written as `-0.0`, so such a reader keeps its sign.
- **A write that cannot get a fresh dot is refused.** Each write in a set, a
  map, a list or text is named by a dot, a counter that goes up by one per
  write. Counters stop at 2^62 (`MAX_COUNTER`). No machine gets there by
  writing, but a broken or hostile sender can hand a machine a dot with its
  own id and a counter at the cap. From then on its writes to that object
  return `WriteError::NoFreshDot` and change nothing, rather than reuse the
  last dot and lose a write. A list also refuses an insert once it holds an
  item, from any machine, with a counter at the cap, because a new item has
  to sort in front of it and cannot. Text and JSON check before they start,
  so a write that needs several dots is refused whole, never half made.
  Counters read from another machine are refused above the same cap, so a
  write that succeeds here is never refused by the others for its counter. A hostile
  sender that pushes an object to the cap gets its later writes refused
  here, out loud; it can never make the copies quietly drift apart.
- **Counters stop at their limits instead of crashing or wrapping.** A
  counter's per-machine totals stop at the largest `u64`, and `value()` is
  worked out exactly and then stops at the ends of an `i64`. So a huge
  number, even one a broken or hostile machine sent, never makes a budget
  read as negative. A `BoundedCounter` withdrawal or transfer whose total
  would pass the largest `u64` is refused, because it could not be recorded
  in full. `try_increment` refuses, and changes nothing, where `increment`
  would keep less than it was given.
- **Two JSON changes can be sent as one.** `JsonDelta::join` gives one delta
  that does what both do, in either order.
- **A dot a change carries counts as seen.** A set, a map or an `MvMap`
  takes in every dot a change holds as seen, even one its context leaves
  out. Otherwise the next write here could be given that same dot, and the
  value that came first would be dropped.
- **A dot under two keys is a hostile sender's mark, and is only partly
  caught.** A dot names one write to one key. One change that lists a dot
  under two keys is refused when it is decoded. But two changes can each
  carry the same dot under a different key, and nothing can tell which one
  is honest. Then the dot is dropped from both keys, the same way on every
  machine, so the copies still agree; what those keys held under that dot
  is lost. Only a broken or hostile sender can do this.
- **A list insert must name an item the list has.** `insert_after` with an
  item that is not in the list returns `WriteError::UnknownPredecessor`. A
  deleted item is still in the list and is fine to name.
- **"Later write wins" drops the earlier write.** Nothing tells you it
  happened. If both values matter, use an `MvMap`, which keeps both until a
  later write replaces them.
