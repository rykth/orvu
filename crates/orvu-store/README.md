# orvu-store

Saves a machine's changes to disk, so they are still there after a restart.

Every change a machine applies is a small record called a *delta*. This module
keeps those deltas in a log, in the order they were applied. After a restart
the log is read back and the machine is where it left off. So that the log does
not grow for ever, it can be swapped for a *snapshot*, the whole current state,
and then started again empty.

## What is inside

| Piece | What it is |
|---|---|
| `Store` | The trait every store implements: append, flush, replay, compact |
| `RedbStore` | The standard store. It keeps everything in one database file |
| `MemStore` | A store that lives in memory, for tests. `fail_saving` makes its saves fail, as a full disk would |
| `FsyncPolicy` | A setting for the caller: flush after every delta, or every so many milliseconds |
| `Receipt` | A note that a change asked for under a request id was made. `receipt` records one, and the next `flush` saves it in the same step as the deltas before it. Receipts are kept for ten minutes, `RECEIPT_KEEP_MS`, and survive compaction |
| `StoreError` | What can go wrong, by kind: the file is already open, reading or writing failed (a full disk is one of these), the data is damaged, or something else |

## How to use it

The deltas in these examples are made with `orvu-core`, which is where they
come from in real use.

### Save deltas and read them back after a restart

```rust
use orvu_core::{Replica, TypedCrdt, TypedDelta, WireDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_store::{RedbStore, Store};

fn add(replica: &mut Replica, amount: u64) -> WireDelta {
    replica
        .mutate("visits", 1_000, |data, _time, me| match data {
            TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, amount)),
            _ => unreachable!(),
        })
        .unwrap()
}

let mut replica = Replica::new(ReplicaId(1), 64);
replica.create("visits", TypedCrdt::PnCounter(PnCounter::default()));

let folder = tempfile::tempdir().unwrap();
let path = folder.path().join("orvu.redb");

{
    let mut store = RedbStore::open(&path).unwrap();
    store.append(&add(&mut replica, 1)).unwrap();
    store.append(&add(&mut replica, 2)).unwrap();
    // Nothing is safely on disk until this returns.
    store.flush().unwrap();
}

// Later, after a restart: open the same file and read the log back.
let store = RedbStore::open(&path).unwrap();
let saved = store.replay().unwrap();
assert_eq!(saved.len(), 2);
```

### Swap the log for a snapshot

```rust
use orvu_core::{fold_into_snapshot, Replica, TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_store::{MemStore, Store};

let mut replica = Replica::new(ReplicaId(1), 64);
replica.create("visits", TypedCrdt::PnCounter(PnCounter::default()));

let mut store = MemStore::new();
for amount in [1, 2, 3] {
    let delta = replica
        .mutate("visits", 1_000, |data, _time, me| match data {
            TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, amount)),
            _ => unreachable!(),
        })
        .unwrap();
    store.append(&delta).unwrap();
}
assert_eq!(store.replay().unwrap().len(), 3);

// Fold the old snapshot and the log into a new snapshot, and store that.
let snapshot = fold_into_snapshot(store.load_snapshot().unwrap(), store.replay().unwrap());
store.compact(&snapshot).unwrap();

assert_eq!(store.load_snapshot().unwrap().len(), 1); // one object
assert!(store.replay().unwrap().is_empty()); // the log starts again
```

## Things to know

- **`append` alone does not save anything.** With `RedbStore` a delta sits in
  memory until `flush`, or until so many are waiting that the next `append`
  flushes them (see below). A crash before the flush loses it.
- **You decide when to flush.** `FsyncPolicy` only names the choice. Nothing in
  this module reads it or flushes on a timer. Flushing after every delta is
  safest and slowest.
- **`replay` returns every appended delta, flushed or not.** So does
  `deltas_peer_lacks`. After a restart only the flushed ones are left.
- **Compacting drops the whole log, unflushed deltas included.** Build the new
  snapshot from the live data or from `replay`, so that nothing is left out.
- **`MemStore` loses what `RedbStore` would lose.** Use `reopened()` in a test
  to get the store a restart would find.
- **Errors say what kind of trouble it is.** `AlreadyOpen` means another
  program has the file. `Io` means reading or writing failed; a full disk is
  an `Io` whose kind is `StorageFull`, and trying again later may work.
  `Corrupted` means the file is damaged and trying again will not help.
  `Other` is anything else the database reported.
- **Unsaved deltas are capped.** `RedbStore` holds at most `UNSAVED_LIMIT`
  deltas in memory waiting for a flush. An `append` that would go past that
  flushes first, so a big batch of changes or a slow flush timer only means
  an earlier save. `append` fails only if that flush fails, with the flush's
  own error.
- **Calls block.** Reading and writing wait for the disk. From async code, call
  the store on a thread set aside for blocking work.
- **A store has one owner.** Its writing methods need `&mut self`, and one
  database file should be opened by one `RedbStore` at a time.
- **To recover after a restart,** load the snapshot first and then replay the
  log on top of it.
- **Compacting is all or nothing.** The new snapshot and the new log replace
  the old ones together. A crash in the middle leaves the old ones in place.
