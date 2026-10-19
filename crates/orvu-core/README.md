# orvu-core

The part of Orvu that keeps copies of data in step between machines.

`orvu-crdt` provides data types that can merge. This module puts them to work.
It holds one machine's objects, turns each change into a small message for the
others (a *delta*), and takes in the deltas they send. A delta that arrives
before the changes it was built on waits until those arrive.

This module does no networking and saves nothing to disk. It hands you deltas
and bytes. Getting them to the other machines is someone else's job.

## What is inside

| Piece | What it is |
|---|---|
| `Replica` | One machine's copy of the data: its objects, its clock, and a buffer for deltas that arrived too early |
| `Object` | One shared object: its data, plus the record of which updates it has seen |
| `TypedCrdt`, `TypedDelta` | Wrappers that can hold any of the data types, or a change to any of them |
| `WireDelta` | One change, ready to send: which object, which update, what it depends on, and the change itself |
| `Applied` | What happened to a delta you received: applied, buffered, or a duplicate |
| `MAX_CLOCK_LEAD_MS` | How far ahead of this machine's clock a timestamp from another machine may be: five minutes |
| `MAX_WRITERS_PER_OBJECT` | How many machines' changes one object holds: 1024 |
| `MutateOutcome` | What came of a change made here: applied, refused by the change itself, turned away by the object (`Unwritable`), or no such object |
| `DEFAULT_WAITING_LIMIT_MS` | How long a delta waits for what it depends on before it is dropped, unless you set another limit: two seconds |
| `CausalBuffer` | The waiting room for deltas that arrived too early. It has a size limit |
| `SyncMessage`, `Codec`, `ProtobufCodec` | The messages two machines exchange, and how they become bytes. `decode` takes exactly one message and refuses bytes after it |
| `NackReason` | Why a machine said no. A reason this version does not know is kept as its number. A `Nack` about one change also carries that change's `object_id` and `dot` |
| `nack_for_unreadable` | For a batch that `decode` refused because one change in it cannot be read, the `Nack` that names that change. `None` for anything else, such as a message of a kind this version does not know |
| `encode_wire_delta`, `decode_wire_delta` | One delta to bytes and back, for storing on disk |
| `SnapshotEntry`, `fold_into_snapshot` | The whole state of an object, and a way to fold a batch of deltas into it |
| `TraceId` | An id that follows one change across machines in tracing tools |
| `Error` | Everything that can go wrong here |

## How to use it

### Two machines sharing a counter

```rust
use orvu_core::{Applied, Replica, TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};

// Each machine has its own replica. 64 is the size of the waiting buffer.
let mut alice = Replica::new(ReplicaId(1), 64);
let mut bob = Replica::new(ReplicaId(2), 64);
alice.create("visits", TypedCrdt::PnCounter(PnCounter::default()));

// Alice adds 5. The second argument is the current time in milliseconds.
let delta = alice
    .mutate("visits", 1_000, |data, _time, me| match data {
        TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, 5)),
        _ => unreachable!("visits is a counter"),
    })
    .expect("the object exists");

// Send the delta to Bob. He did not have the object, so it is created.
let (what_happened, _) = bob.receive(delta, 1_001).unwrap();
assert_eq!(what_happened, Applied::Applied);

match bob.object("visits").unwrap().crdt() {
    TypedCrdt::PnCounter(counter) => assert_eq!(counter.value(), 5),
    _ => unreachable!(),
}
```

### Deltas that arrive in the wrong order

```rust
use orvu_core::{Applied, Replica, TypedCrdt, TypedDelta, WireDelta};
use orvu_crdt::{PnCounter, ReplicaId};

fn add(replica: &mut Replica, amount: u64) -> WireDelta {
    replica
        .mutate("visits", 1_000, |data, _time, me| match data {
            TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, amount)),
            _ => unreachable!(),
        })
        .unwrap()
}

let mut alice = Replica::new(ReplicaId(1), 64);
let mut bob = Replica::new(ReplicaId(2), 64);
alice.create("visits", TypedCrdt::PnCounter(PnCounter::default()));

let first = add(&mut alice, 1);
let second = add(&mut alice, 2);

// The second change reaches Bob first. It was built on the first, so it waits.
let (what_happened, _) = bob.receive(second, 1_001).unwrap();
assert_eq!(what_happened, Applied::Buffered);
assert_eq!(bob.pending(), 1);

// The first change arrives. It is applied, and it lets the second one through.
let (what_happened, let_through) = bob.receive(first, 1_002).unwrap();
assert_eq!(what_happened, Applied::Applied);
assert_eq!(let_through.len(), 1);
assert_eq!(bob.pending(), 0);
```

### Turning a delta into bytes

```rust
use orvu_core::{decode_wire_delta, encode_wire_delta, Replica, TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};

let mut alice = Replica::new(ReplicaId(1), 64);
alice.create("visits", TypedCrdt::PnCounter(PnCounter::default()));
let delta = alice
    .mutate("visits", 1_000, |data, _time, me| match data {
        TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, 5)),
        _ => unreachable!(),
    })
    .unwrap();

let bytes = encode_wire_delta(&delta);
let back = decode_wire_delta(&bytes).unwrap();

assert_eq!(back, delta);
```

## Things to know

- **You deliver the deltas.** `mutate` returns a delta and `receive` takes one.
  Nothing in between is done for you.
- **A delta waits until what it depends on has arrived.** That keeps a reader
  from seeing an effect before its cause. The final result would be the same
  either way.
- **The waiting buffer has a size limit.** When it is full, `receive` returns
  `CausalBufferFull` and the delta is not kept. The machine has to catch up
  another way, for example from a snapshot.
- **A delta does not wait for ever.** One still waiting after the waiting
  limit (`Replica::with_waiting_limit`, two seconds by default) is dropped,
  so deltas whose earlier changes never come cannot fill the buffer for good.
  The limit is measured in the wall times you pass to `receive` and
  `merge_snapshot`, and old deltas are dropped when one of those is called.
  Sending a waiting delta again does not start its wait over.
  `waiting_dropped` counts the deltas dropped this way. The machine gets them
  again from catch-up, so set the limit longer than a catch-up round.
- **A delta received twice is ignored.** `receive` reports it as a duplicate.
- **A delta that could not have been made honestly is refused.** Each machine
  numbers its own changes in order. A delta whose number skips ahead gets
  `MalformedPayload`. So does a snapshot whose record of what it has seen has
  gaps, which a machine taking changes in order never has.
- **A machine restored from a backup must take a new machine number.** A
  machine whose data was restored from a backup, or rolled back in any other
  way, has lost changes it may already have sent, and it cannot tell. Its
  next change gets a number the others have already seen for one of the
  lost ones, so they ignore it as a repeat, and nothing ever repairs it: the
  copies hold different values for good. So start it as a new machine (a new
  `ReplicaId`) before it writes. What is still taken in by itself: a
  snapshot or delta from another machine that holds the lost changes is
  taken in like any other, so they come back, and a change made after that
  is numbered after them.
- **One object takes changes from at most 1024 machines**
  (`MAX_WRITERS_PER_OBJECT`). Every change carries one number for each
  machine that has written to the object, so made-up machine names would
  otherwise make every later change large. A delta or snapshot from another
  machine that would bring an object a writer it does not have yet and take
  it past the limit, counting the machines it depends on, gets
  `MalformedPayload`. A change made here by a machine
  that would be one writer too many is refused the same way, before it is
  made: `mutate` returns `None`, and `write` and `try_mutate` return
  `MutateOutcome::Unwritable(WriteError::TooManyWriters)`, and the
  machine goes on taking in the changes of the machines already writing.
  A machine already among an object's writers may always write to it, even
  to an object saved past the limit under older rules: one rule holds
  everywhere, that only a writer new to the machine checking is turned
  away. The check counts the writers this machine has seen, so near or past
  the limit an object can stop syncing: a new machine cannot start writing
  to it, a machine that has not yet seen one of its writers refuses that
  writer's changes and stays cut off from them, and two machines that each
  let in a different new writer at the same time refuse each other's
  changes. The limit is only there to bound what a hostile sender can do,
  and real teams stay far below it. One loaded back with `replay` is not
  checked again. A team of
  more than 1024 machines all writing to one object is not supported. A waiting
  delta dropped because the object filled up while it waited is counted in
  `waiting_dropped`. A machine allowed to write can make up machine names
  to fill an object's 1024 places, and then no new machine can write to
  that object anywhere it spread to. Like other harm a writer can do on
  purpose, the only fix is not to let that machine write.
- **A timestamp from far in the future is refused.** A delta or snapshot
  carrying a timestamp more than five minutes (`MAX_CLOCK_LEAD_MS`) ahead of
  the wall time you pass in, in its own stamp or anywhere inside a value
  (a JSON node's generations included), gets
  `TooFarAhead` and changes nothing. Taking it in would drag this machine's
  clock forward for good. Anything closer is taken in, and `last_lead` says how
  far ahead it was, so you can warn about clocks that disagree. The error
  names the machine that wrote the timestamp (`writer`), which is not always
  the machine that sent it to you. `writer` is only what the change itself
  claims, and another machine could put any name there. The machine it
  actually came from is the one your connection knows (`from` in the
  runtime's warnings).
- **A timestamp a machine takes in becomes its clock.** So the five minutes
  are not measured against each machine's own clock alone: they apply across
  the whole team. Say machine A's clock is six minutes fast and C's is two
  minutes fast. C takes in A's change, which is only four minutes ahead of
  C, and C's clock is now six minutes fast too. C's next change, to any
  object, is refused by a machine whose clock is right, and that refusal
  names C as the writer, though the fast clock is A's. One machine with a
  clock that is too fast can get other machines' changes refused like this
  until real time catches up. The fix is to correct that machine's clock.
  `clock_lead` says how far this machine's own clock runs ahead of the wall
  time you pass in, so a machine can notice it is in C's place.
- **The clock moves past everything taken in.** That includes snapshots merged
  with `merge_snapshot`, and state loaded after a restart with `install` and
  `replay`. So a change made here afterwards is newer than all of it, even if
  the wall clock has been set back. `replay` is for deltas this machine saved
  itself: it takes them in however far ahead they are.
- **An object you have not created appears on its own** when a delta for it
  is applied, or a snapshot holding at least one change is merged, with the
  type that delta or snapshot says. A delta that only waits, or is refused,
  creates nothing, and neither does a snapshot holding no change.
- **`mutate` on an object that does not exist returns `None`.** Create it
  first. `write` does the same change and says why there is none:
  `NoObject`, or `Unwritable` with the reason when the object turned the
  write away, because it has no fresh dot left for this machine
  (`WriteError::NoFreshDot`) or no room for one more writer
  (`WriteError::TooManyWriters`).
- **The function you pass to `mutate` must return the delta for the change it
  made.** With `try_mutate` it may return `None` to refuse, and then it must not
  have changed anything.
- **Bytes from another machine are never trusted.** Decoding nonsense returns
  an error. It does not crash. That includes JSON values this version does not
  recognise, a JSON field named twice in one change, and a JSON generation
  more than 2^32 steps on (`MAX_GENERATION_STEP`), which no real document
  reaches and which would leave no room to start that place afresh again.
  Other things only a broken or hostile sender writes are refused with
  `MalformedPayload` too:
  - a dot counter, or a version vector entry, above 2^62 (`MAX_COUNTER`).
    Changes made here stop at the same number, so a change that succeeds
    here is never refused elsewhere. A number taken in can use up the
    numbers left to this machine's writes: a hostile sender can send one at
    2^62, and from then on every write to that object here is refused with
    an error. That is the worst it can do. The writer is told, and the
    copies never quietly drift apart;
  - one dot listed under two keys or two set elements in one change;
  - a list item whose two ways of saying "at the front" disagree
    (`has_predecessor` and whether `predecessor` is there), or a list item id
    listed twice in one change;
  - bytes left over after a message (`ProtobufCodec::decode`).
- **A message of a kind this version does not know is an error
  (`MissingField`),** not some other message. A machine skips it and keeps
  the connection, which is how an older machine handles a `KeepAlive` from a
  newer one.
- **A reason this version does not know, in a refusal from a newer machine,
  is kept as `NackReason::Unknown`** with its number, not read as
  "unspecified".
- **`conformance/vectors.json` holds shared test cases.** Any other
  implementation of the engine should give the same results and the same bytes
  for them.
