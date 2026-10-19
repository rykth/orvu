# orvu-runtime

The engine: it runs one machine's copy of the data and keeps it in step with
the other machines.

The other modules are parts. `orvu-core` knows how to apply changes,
`orvu-transport` knows how to connect two machines, and `orvu-store` knows how
to save to disk. This module puts them to work together. You start a node, tell
it which machines to connect to, and change data on it. The node sends each
change to the others, takes in theirs, saves to disk, and catches up on
anything it missed.

## What is inside

| Piece | What it is |
|---|---|
| `Node` | Starts a node with `Node::spawn`, or over a store you give it with `Node::spawn_with_store` |
| `SpawnError` | Why a node could not start: its store could not be opened or read (`Storage`), or the network could not be set up (`Transport`) |
| `NodeHandle` | What you hold once it is running: create objects, change them, read them, shut down |
| `NodeConfig` | The settings: this machine's number, where it listens, which machines to connect to, where to save, and timings |
| `Acl`, `Action` | Which machines may sync and which may write. Open to every connected machine unless you set a list |
| `TryOutcome` | The result of a change that is allowed to be refused. `Unwritable` says the object turned the write away before it was made |
| `Leaving`, `LeaveError` | What `NodeHandle::leave` did, or why it could not |
| `FsyncPolicy` | How soon a change must be safely on disk |
| `Store`, `MemStore` | What a node saves to, and a store kept in memory that a test can make fail |
| `with_receipt`, `Receipt` | Runs some work so that each change it makes leaves a receipt with a given id, saved in the same step as the change. `NodeHandle::receipt_made` says whether a change was made under an id in the last ten minutes, before the node started or since |
| `Metrics`, `MetricsSnapshot`, `render_prometheus` | Counters about what the node is doing, and a text form that Prometheus can read |

## How to use it

### Two nodes sharing a counter

This example connects two nodes inside one program with `MemTransport`. On real
machines you would give each node a `QuicTransport` and real addresses, and
nothing else would change.

```rust
use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{Node, NodeConfig, NodeHandle};
use orvu_transport::{MemTransport, PeerAddr};
use std::sync::Arc;
use std::time::Duration;

async fn visits(node: &NodeHandle) -> i64 {
    match node.snapshot("visits").await {
        Some(TypedCrdt::PnCounter(counter)) => counter.value(),
        _ => 0,
    }
}

#[tokio::main]
async fn main() {
    let network = Arc::new(MemTransport::new());

    // Each node gets its own number, an address to listen on, and the
    // addresses of the machines it should connect to.
    let mut alice_settings = NodeConfig::new(ReplicaId(1), "alice");
    alice_settings.peers = vec![PeerAddr::new("bob")];
    let mut bob_settings = NodeConfig::new(ReplicaId(2), "bob");
    bob_settings.peers = vec![PeerAddr::new("alice")];

    let alice = Node::spawn(network.endpoint(1), alice_settings).await.unwrap();
    let bob = Node::spawn(network.endpoint(2), bob_settings).await.unwrap();

    alice.create("visits", TypedCrdt::PnCounter(PnCounter::default())).await;
    bob.create("visits", TypedCrdt::PnCounter(PnCounter::default())).await;

    // Each node changes its own copy.
    alice
        .mutate("visits", |data, _time, me| match data {
            TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, 5)),
            _ => unreachable!(),
        })
        .await;
    bob.mutate("visits", |data, _time, me| match data {
        TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, 3)),
        _ => unreachable!(),
    })
    .await;

    // The changes travel in the background. Wait until both nodes have both.
    for _ in 0..200 {
        if visits(&alice).await == 8 && visits(&bob).await == 8 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(visits(&alice).await, 8);
    assert_eq!(visits(&bob).await, 8);

    alice.shutdown().await;
    bob.shutdown().await;
}
```

### Keep the data across restarts

Give the node a file to save to. When a node is started again with the same
file, it loads what was saved before it does anything else.

```rust
use orvu_core::{TypedCrdt, TypedDelta};
use orvu_crdt::{PnCounter, ReplicaId};
use orvu_runtime::{FsyncPolicy, Node, NodeConfig};
use orvu_transport::MemTransport;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let folder = tempfile::tempdir().unwrap();
    let network = Arc::new(MemTransport::new());

    let mut settings = NodeConfig::new(ReplicaId(1), "alice");
    settings.store_path = Some(folder.path().join("alice.redb"));
    // Save every change to disk before reporting it as done.
    settings.fsync = FsyncPolicy::Always;

    let node = Node::spawn(network.endpoint(1), settings.clone()).await.unwrap();
    node.create("visits", TypedCrdt::PnCounter(PnCounter::default())).await;
    node.mutate("visits", |data, _time, me| match data {
        TypedCrdt::PnCounter(counter) => TypedDelta::PnCounter(counter.increment(me, 5)),
        _ => unreachable!(),
    })
    .await;
    node.shutdown().await;

    // Start again from the same file.
    let node = Node::spawn(network.endpoint(1), settings).await.unwrap();
    match node.snapshot("visits").await {
        Some(TypedCrdt::PnCounter(counter)) => assert_eq!(counter.value(), 5),
        _ => panic!("the counter was not restored"),
    }
    node.shutdown().await;
}
```

## Things to know

- **Changes reach other machines a moment later, not at once.** `mutate`
  returns as soon as the change is applied here. Sending happens in the
  background, in small batches.
- **`read` looks at an object where it is. `snapshot` copies it.** A copy
  costs more the more the object holds, so use `read` to look at one thing
  in it. What is given to `read` runs inside the engine and holds everything
  else up while it does: it should look, return, and do nothing else. An
  answer that takes longer to work out than the object takes to copy is
  better worked out from a `snapshot`, outside the engine. If what is given
  to `read` panics, the caller gets the panic and the engine carries on.
  The counter `objects_copied` says how many whole copies were made.
- **A panic is only caught where panics unwind,** which is Rust's default.
  A program built with `panic = "abort"` is ended by a panic in what it
  gives to `read`, `mutate`, `write` or `try_mutate`, engine and all, as by
  any other panic in it. Nothing can catch a panic there.
- **A machine that was away catches up by itself.** Nodes regularly compare
  what they have seen and send each other what is missing. A machine that is
  far behind is sent whole objects in place of the individual changes.
- **A lost connection is made again.** A node keeps dialing each machine on
  its list until it connects, and starts again whenever that connection ends.
  A machine that was restarted, or cut off for a while, is found again
  without either side being restarted. Each failed try is followed by a
  wait twice as long as the one before, from a quarter of a second up to
  `longest_dial_wait` (five seconds unless set), so a machine that is gone
  for good costs little, and one that comes back is found within that. A
  connection that lasted starts the waits over.
- **A connected machine that goes silent is let go.** Each connection sends
  something at least about once an `anti_entropy_interval`: changes, a
  catch-up summary, or else a small `KeepAlive`. The keep-alive comes from
  the connection's own writer and does not wait on the engine, so a node whose
  engine is held up, saving to a slow disk or building a large snapshot for
  catch-up, is not let go for it. A machine whose process has stopped
  running, or whose cable was pulled, does not close the connection; it just
  stops sending. After `quiet_limit` with nothing heard (five intervals by
  default, five seconds with the default interval) the connection is
  closed, which the other end notices too, and the machine is dialed again.
  Only time spent waiting counts, not time spent handling what arrived.
  Machines that talk to each other should use the same
  `anti_entropy_interval`. If their intervals differ, set `quiet_limit` on
  every one of them to at least three times the longest interval among
  them: a machine with a long interval can be quiet that long, and one with
  a short interval would otherwise let it go. A `quiet_limit` below three
  intervals, or below one second, is raised to the higher of the two, with
  a warning in the log, since a healthy link that has nothing to say would
  otherwise be closed and dialed again now and then.
- **`NodeConfig::link` holds the time limits of a QUIC link** (`LinkTimes`
  from `orvu-transport`): when a link on which nothing is heard is given up,
  how often a quiet link shows it is alive, and how long a dial may take.
  The node does not build its link, so it only carries these for whoever
  does. `orvu-sdk` builds its `QuicTransport` with them.
- **Every machine needs its own number,** and over an encrypted link that
  number must match the one in its certificate. A machine that claims a
  different number is refused.
- **A node started on data restored from a backup needs a new number.** The
  data was rolled back, so the node has lost changes it may already have
  sent, and it cannot tell. Under its old number its next writes would
  reuse numbers the others have seen, and they would ignore them as
  repeats. Started under a new number it takes the lost changes back from
  the others by the usual catch-up.
- **`write` says why a change was not made.** It is `mutate` with a reason:
  `Err(Some(why))` when the object turned the write away, because it has no
  fresh dot left for this machine or already has changes from the most
  machines it takes, and `Err(None)` otherwise. `try_mutate` gives the same
  reason as `TryOutcome::Unwritable`.
- **Without `store_path` nothing is saved.** The data lives in memory and is
  gone when the node stops.
- **`FsyncPolicy` decides how much a crash can lose.** `Always` saves each
  change to disk before reporting it as done. It is the slowest. The default
  writes to disk every 100 milliseconds, so a crash can lose up to that much
  of this machine's own changes. Either way, a change made here is only sent
  to other machines once it is on disk, in the order it was made, so no other
  machine ever holds a change this one could lose. Under the default that
  holds a change back from the others for up to 100 milliseconds, but the
  caller still gets its answer at once.
- **Catch-up only hands out what is on disk.** Before another machine is sent
  missing changes or a snapshot, everything waiting to be saved is saved.
- **A node that cannot save stops serving.** If saving fails, for example
  because the disk is full, the failure is logged as an error and
  `NodeHandle::storage_failure` says why. Memory may then hold a change that
  is not on disk, so nobody is shown anything from then on. Every change is
  refused: `mutate` returns `None`, `write` returns `Err(None)` and
  `try_mutate` returns `TryOutcome::StorageFailed`. Every read finds nothing: `read` and
  `snapshot` return `None`. Other machines get nothing more from this node,
  no changes, no catch-up and no snapshots; what they send is dropped. The
  node also closes every connection to other machines, stops dialing them
  and turns new ones away, so they see it as gone and stop waiting for it.
  Were it to stay connected but silent, they would keep it as a member whose
  view never moves, and their logs would grow without end.
  `NodeHandle::saving_failed` finishes at that moment, so code that waits for
  changes can stop. Under `Always` the change whose save failed is not
  reported as done; under the default the caller was already answered.
  Restarting the node loads what is on disk and is the way back, once the
  disk is fixed.
- **A compaction that cannot read the store does nothing.** It would rewrite
  the store from what it read, so it never goes on with a failed read. That
  counts as a failed save.
- **A node without `store_path` saves to memory with every change,** since
  that costs nothing, so its changes go out at once.
- **"Later write wins" goes by the time of day.** Each change is stamped with
  the machine's clock, so keep the machines' clocks in step. A machine whose
  clock runs ahead wins more often than it should. A change or snapshot
  stamped more than five minutes ahead of this node's clock is refused and
  counted in `changes_refused_clock_ahead`; one more than a second ahead is
  taken in and counted in `clock_skew_seen`. Each is logged as a warning, at
  most once a minute for each peer. A refusal names two machines: `writer`,
  the one whose timestamp it is, and `from`, the one that passed it on.
  `refused_clock_ahead_last_writer` shows the writer of the last one refused.
  `from` is the machine the change actually came from, over the connection.
  `writer` is only what the change itself says, and another machine could
  put any name there, so treat it as a lead, not as proof.
- **A timestamp a node takes in becomes its own clock,** so the five minutes
  apply across the whole team, not between two machines. If one machine's
  clock is too fast, every machine that takes in its changes ends up with a
  fast clock as well, and machines whose clocks are right then refuse their
  changes too, naming them as the writer, until real time catches up. The
  fix is to correct the clock of the machine that is fast. To help find it,
  each node watches its own clock: `clock_ahead_of_wall_ms` says how far its
  clock runs ahead of the wall clock at the moment the metrics are read. The
  node keeps the time on its clock (the latest timestamp it gave or took in)
  and the lead is worked out on each read, so it shrinks on its own as real
  time catches up, even on an idle node, and it is right straight after
  start-up when what was loaded was stamped by a fast clock. The warning is
  checked at start-up, after each write and after each batch of changes
  taken in. When the lead is more than half the limit, the node logs
  a warning, at most once a minute, that its clock is ahead of its own time,
  probably because another machine's clock is fast or its own time was set
  back, and that machines whose clocks are behind will refuse its changes.
- **A change waits at most two catch-up rounds for what it depends on.** A
  change from another machine that arrives before an earlier one it depends
  on is held back. If it is still waiting after twice
  `anti_entropy_interval`, it is dropped and counted in
  `waiting_changes_dropped`; catch-up brings it again. So the waiting room
  (`causal_buffer_cap`) cannot be filled for good by changes whose earlier
  ones never come. A change that only waits does not create its object.
  A waiting change is also dropped, and counted the same way, if by the time
  it is ready its object already has changes from 1024 machines and its
  writer is not one of them.
- **Changes no honest machine could send are refused** and counted in
  `changes_refused_malformed`: a number that skips ahead, a snapshot whose
  record has gaps, and anything that would leave one object with changes
  from more than 1024 machines. A team of more than 1024 machines writing to
  one object is not supported. Each is logged as a warning naming the
  machine it came from.
- **A refused change is answered with a `Nack` that names it.** A change
  refused as one no honest machine could send (reason `MALFORMED_PAYLOAD`),
  or for a timestamp too far ahead (`CLOCK_AHEAD`), gets a `Nack` with the
  change's `object_id` and `dot`, so the sender can tell which change of a
  batch it was. So does a batch that cannot be read at all because one
  change in it is broken: the whole batch is dropped, and the `Nack` names
  that change. One connection sends at most `NACKS_PER_SECOND` (10) a
  second; the rest are counted in `nacks_held_back` and `nacks_sent` counts
  the ones that went out. A `Nack` received is counted in `nacks_received`
  and logged as a warning, at most ten a second per connection. Nothing is
  sent back for it.
- **A restart never makes a new write lose to an old one.** When the node
  starts, its clock is moved past every timestamp in what it loads, from the
  snapshot and from the saved changes, even if the wall clock is now behind.
- **A member that has gone is not brought back by old news.** Once a node
  has dropped a quiet member, it takes that member back only when it hears a
  newer heartbeat from it or connects to it again, not when another machine
  that has not dropped it yet still passes it on. A heartbeat is the wall
  clock in milliseconds, raised by one when the clock has not moved, so a
  machine restarted under the same id, even with an empty store, is
  usually newer at once and is taken back within a round or two. That
  holds only when its clock is now past the last heartbeat it sent before
  it stopped. A machine whose clock went back, or was ahead before, is not
  taken back on news from others until `forget_removed_after` has passed
  (24 hours by default), unless it connects directly. After that any news
  about it brings it back, so it is not shut out for good. `orvu-sdk` sets
  this with `forget_ms` in `[membership]`.
- **Machines only sync within one namespace.** Two nodes set up with different
  `namespace` values refuse to connect to each other.
- **Machines only sync on the same protocol version.** Each machine says its
  version (`orvu_proto::PROTO_VERSION`) when it connects. A newer version can
  mean the same bytes say something else, so a node that meets another
  version refuses it: it answers with a `Nack` whose reason is
  `PROTO_VERSION` and whose text names both numbers, logs a warning with
  `ours` and `theirs`, and drops the connection. The other node logs the
  `Nack` it got. This holds whichever side dialed, and also against a
  machine from before this check, which does not compare numbers itself. So
  upgrade every machine of a team to the same version. `NodeConfig` has a
  `proto_version` field only so tests can stand in for a machine on another
  version; leave it as it is.
- **Rebalancing gives allowance only to machines connected right now.** A
  machine that has stopped, or that this node only hears about through
  others, gets nothing: what it was given could not be spent or lent back by
  anyone this node can reach. `NodeHandle::connected_peers` lists the
  machines with a live connection. Asking changes nothing, unlike
  `known_members`, which refreshes this node's heartbeat and drops members
  that have gone quiet.
- **A machine can leave for good with `NodeHandle::leave`.** It notes its
  number in the set `orvu/_machines/left`, hands all the allowance it holds,
  on every bounded counter, to the machines it is connected to, split evenly,
  and waits until each has said, in its catch-up digest, that it has
  everything this machine wrote. The others read that set before each
  rebalancing round: a machine in it is given nothing and its share is not
  counted, so the allowance does not flow back to it. A number in the set
  counts only if the mark says the machine with that number wrote it. That
  stops a program, which cannot write there at all, and a machine that
  follows the rules. It does not stop a machine of the team that sets out to
  cheat: what machines write is passed on by the others, so where a mark
  came from cannot be checked, and a machine holding a valid certificate
  could write a mark that looks like another's, or take a real one away.
  The machines of a team are trusted; programs are not. `MACHINES`, `orvu/_machines/`, is where
  the machines keep what is theirs alone, and `orvud` lets no program read or
  change anything under it. The wait is bounded;
  `Leaving` says what was handed over, what was still held, and whether the
  others were heard to have it. With no machine connected it fails and
  changes nothing. A machine that has left should not be started again
  under its number: the others would never give it allowance again.
- **A node lends allowance only to the machine that asks for it,** and only if
  that machine is allowed to write.
- **Every connected machine may sync and write unless you set an `Acl`.** With
  a list, a machine that is not on it may do nothing.
- **`shutdown` sends what is left before it closes.** Changes made before it
  are saved, then sent to the machines still connected, and only then are
  the connections closed. Sending waits at most two seconds, so a machine
  that does not read cannot hold the shutdown up. More changes can be
  waiting than `ship_queue_cap` has room for; they are handed to the
  connections as those take them, so none is pushed out. A connection that
  still falls behind sends what it has left, logs how many it missed, and
  catch-up brings those over next time the two machines are connected.
- **`shutdown` does not wait behind queued calls.** The engine works
  through a queue of at most 1,024 calls, and the stop goes ahead of it.
  From the moment `shutdown` is called, nothing new is put in the queue.
  Calls still in it, or still waiting for room, are not run and get the
  answer an engine that has stopped gives: no change made, nothing read.
  That holds for a call that slips in at the very moment of the stop too,
  so no caller, and no connection, is left waiting for an answer that never
  comes. Only the call the engine is in the middle of is finished first.
- **Call `shutdown` before starting another node on the same file.** It waits
  until the file has been released.
- **`Node::spawn_with_store` takes any `Store`** instead of the file at
  `store_path`. With a `MemStore` a test can make saving fail and see what
  the node does.
- **Start nodes inside a Tokio runtime.** A node runs as background tasks.
