# orvu-sdk

The friendly way to use Orvu from Rust.

The other modules deal in deltas, replicas and transports. This one hides all
of that. You start a `Client`, open a counter, a list or a document by name,
and call ordinary methods on it. Changes reach the other machines in the
background, and theirs show up in what you read.

The engine runs inside your program. There is no separate server to start.

## What is inside

| Piece | What it is |
|---|---|
| `Client` | Starts the engine and opens objects by name |
| `Counter` | A number you add to and subtract from |
| `BoundedCounter` | A number that can never go below zero, such as a budget or stock level |
| `Register` | One text value. The later write wins |
| `Set` | A set of unique text items |
| `Map` | Keys and text values |
| `MvMap` | Keys and text values that keep every value written at the same time, until a later write replaces them |
| `Cart` | Items with a quantity each |
| `List` | An ordered list of text items |
| `Json` | A JSON document |
| `RichText` | Text with formatting |
| `Tree` | Items nested under other items |
| `SdkConfig` | Settings read from a TOML file |
| `SdkError` | Everything that can go wrong here |
| `JsonChange` | One write to a `Json` document, for `change_if` |
| `ChangeFollower` | What `on_change` returns. The callback stops when it is dropped |
| `with_receipt` | Runs some work so that each change it makes leaves a receipt with a given id, saved with the change. `Client::receipt_made` says which were made, before a restart or since, in the last ten minutes, so a request sent again is not made twice. `current_receipt` gives the id the work runs under |
| `ConsistencyClass` | Which rules an object follows: `Ap` for the types that always accept a change, `Escrow` for `BoundedCounter` |

Every object has a `watch()` method that gives a stream of its value, so a
program can react when something changes.

## How to use it

### Two programs sharing data

This example runs two clients inside one program, joined by `MemTransport`. On
real machines each client is started from a settings file (see below) and the
rest is the same.

```rust
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::{MemTransport, PeerAddr};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> orvu_sdk::Result<()> {
    let network = Arc::new(MemTransport::new());

    let mut alice_settings = NodeConfig::new(ReplicaId(1), "alice");
    alice_settings.peers = vec![PeerAddr::new("bob")];
    let mut bob_settings = NodeConfig::new(ReplicaId(2), "bob");
    bob_settings.peers = vec![PeerAddr::new("alice")];

    let alice = Client::start(network.endpoint(1), alice_settings).await?;
    let bob = Client::start(network.endpoint(2), bob_settings).await?;

    // Both open the same objects by name.
    let alice_visits = alice.counter("visits").await?;
    let bob_visits = bob.counter("visits").await?;
    let alice_notes = alice.list("notes").await?;
    let bob_notes = bob.list("notes").await?;

    alice_visits.increment(5).await?;
    bob_visits.increment(3).await?;
    alice_notes.insert(0, "buy milk").await?;

    // Changes travel in the background. Wait until both sides have them.
    for _ in 0..200 {
        if alice_visits.value().await? == 8 && bob_notes.len().await? == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(alice_visits.value().await?, 8);
    assert_eq!(bob_visits.value().await?, 8);
    assert_eq!(bob_notes.to_vec().await?, vec!["buy milk".to_string()]);
    assert_eq!(alice_notes.to_vec().await?, bob_notes.to_vec().await?);

    alice.shutdown().await;
    bob.shutdown().await;
    Ok(())
}
```

### A budget that cannot be overspent

```rust
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::{Client, SdkError};
use orvu_transport::MemTransport;
use std::sync::Arc;

#[tokio::main]
async fn main() -> orvu_sdk::Result<()> {
    let network = Arc::new(MemTransport::new());
    let client = Client::start(network.endpoint(1), NodeConfig::new(ReplicaId(1), "alice")).await?;

    let budget = client.bounded_counter("budget").await?;
    budget.increment(10).await?;

    budget.try_decrement(4).await?;
    assert_eq!(budget.value().await?, 6);

    // Asking for more than is left is refused, and nothing changes.
    let refused = budget.try_decrement(100).await;
    assert!(matches!(refused, Err(SdkError::Exhausted)));
    assert_eq!(budget.value().await?, 6);

    client.shutdown().await;
    Ok(())
}
```

### React when something changes

```rust
use futures::StreamExt;
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::MemTransport;
use std::sync::Arc;

#[tokio::main]
async fn main() -> orvu_sdk::Result<()> {
    let network = Arc::new(MemTransport::new());
    let client = Client::start(network.endpoint(1), NodeConfig::new(ReplicaId(1), "alice")).await?;
    let title = client.register("title").await?;

    let mut changes = Box::pin(title.watch());

    // The first item is the value as it is right now.
    assert_eq!(changes.next().await, Some(None));

    title.set("Draft").await?;
    assert_eq!(changes.next().await, Some(Some("Draft".to_string())));

    client.shutdown().await;
    Ok(())
}
```

### Start from a settings file

On real machines a client is started from a TOML file with
`Client::from_toml("orvu.toml")`. It connects over an encrypted link, so it
needs a certificate.

```toml
replica = 1
namespace = "team-a"
listen = "10.0.0.5:7000"
peers = ["10.0.0.6:7000", "10.0.0.7:7000"]

[security]
cert = "certs/node1.pem"
key = "certs/node1-key.pem"
ca = "certs/ca.pem"

[storage]
path = "data/orvu.redb"
fsync = "always"

[budget]
rebalance_ms = 1000
```

`peers` takes names as well as addresses. `orvud setup` writes such a file,
and the certificates, for each machine of a team.

`[link]` sets the time limits of the links to other machines, each in
milliseconds, and each one left out keeps its default:

```toml
[link]
idle_ms = 30000              # a link on which nothing is heard is given up
keep_alive_ms = 5000         # a quiet link sends a sign of life this often
dial_ms = 10000              # the longest one dial may take
quiet_ms = 5000              # a connected machine that sends nothing is let go
longest_dial_wait_ms = 5000  # the longest wait between dials to a machine
```

`keep_alive_ms` must be less than `idle_ms`. `quiet_ms` is five times
`anti_entropy_interval_ms` in `[sync]` if left out, and it must be at least
three times that interval and at least 1000. Machines with different
intervals should each set it to at least three times the longest of them.
A connection with nothing to carry sends a small message about once an
interval, so a shorter `quiet_ms` would close healthy links now and then for
no reason and dial them again. A shorter `idle_ms` or
`quiet_ms` notices a machine whose cable was pulled sooner, at the cost of a
little more traffic on quiet links.

`[membership]` sets, in milliseconds, how long a machine heard of only
through others may stay quiet before it is suspected (`suspect_ms`), how
long it stays suspected before it is dropped (`dead_ms`), both twice the
catch-up interval if left out, and how long a dropped machine is remembered
(`forget_ms`, 24 hours if left out). While it is remembered, a machine
restarted with its clock behind the last heartbeat it sent is not taken
back by news from others until it connects directly or this time has
passed. A zero for any of the three is refused. With a `suspect_ms` or
`dead_ms` of zero a machine would be suspected, or dropped, as soon as it
was looked at, and with a `forget_ms` of zero it would be forgotten at once,
and old news about it would bring it straight back.

`[metrics] addr` serves the engine's numbers at that address, at `/metrics`.
`[logs]` is read but not used by the engine. It is there for the program
that runs the engine, such as `orvud`. A program that handles part of the
settings itself reads the file with `SdkConfig::parse_toml`, takes that part
out, and starts the engine with `Client::from_config`.

## Things to know

- **Reads are of this machine's copy.** They are instant and work offline. A
  change made elsewhere shows up a moment later.
- **Looking one thing up does not get slower as the object grows:** a key
  of a map, a member of a set, a field of a JSON document, a node's parent
  or label, a counter's value. Reading all of an object does, as it must.
- **A list, a text and the shape of a tree are different.** Any read of
  them goes through the whole object, one item of a list included, and is
  done on a copy so that the engine is not held up for it.
- **A write to one key of a map, or one member of a set, does not get
  slower as the object grows.** A write to a list, a text, a tree or a JSON
  document with many fields at one level still does.
- **Opening an object creates it.** `client.counter("visits")` makes an empty
  counter if there is none. To find out whether something exists without
  creating it, use `client.exists(name)`. To watch a name that may not exist
  yet, use `client.attach(name)`.
- **An object's name decides what it is.** Opening `"visits"` as a counter and
  then as a list returns `TypeMismatch`.
- **Every machine needs its own `replica` number.** With a settings file it
  must match the number in the machine's certificate.
- **A `BoundedCounter` can only spend this machine's share.** Whoever calls
  `increment` gets that amount as their share. `transfer` hands some of it to
  another machine. When `try_decrement` needs more than the local share, it
  asks one machine it is connected to right now that has enough for the
  difference, and waits up to 2 seconds before giving up with `Exhausted`.
  It does not split the request across several machines. The wait is
  `borrow_wait_ms` in the `[budget]` section of a settings file.
- **Withdrawals from one `BoundedCounter` on one machine that have to borrow
  take turns.** Two that both have to borrow would otherwise both wake on the
  first grant, and the one that lost would be refused while its own grant was
  still on the way. So a withdrawal that has to borrow waits while another
  one on the same counter borrows, for up to that one's borrowing wait. A
  withdrawal the local share covers does not wait; it goes straight through.
  If it spends part of a grant meant for a borrower, the borrower asks
  again, within its own wait. A borrower notices its grant by what other
  machines have handed to this one, so share this machine gives away while
  it waits, by `transfer` or evening out, does not hide the grant.
- **Shares can be evened out ahead of need.** With `rebalance_ms` in the
  `[budget]` section, each machine hands part of its share to the machines
  it is connected to on that timer, so that a machine that spends finds its share already there,
  and so that most of a budget can still be spent when one machine is away.
  A machine that has stopped, or that is only heard of through others, gets
  nothing, since nobody here could reach what it was given. Without the
  setting there is no evening out.
- **A `BoundedCounter` holding one is a ticket.** Only one machine can ever
  take it, whatever the links between the machines do, because a machine can
  only take what its own share holds. A machine cut off from the one that
  holds it is refused. A test races three machines for tickets while links
  between them are cut and healed again, and checks that no ticket is taken
  more often than it was given.
- **A write that cannot get a fresh name is refused with `Write`.** Each
  write is named by a counter that stops at 2^62. Only a broken or hostile
  machine can push this machine's counter for an object to the top. From
  then on its writes to that object, counters included, return
  `SdkError::Write` and change nothing, rather than reuse a name and lose a
  write. The message says the object has no change numbers left for this
  machine and can only be read.
- **A machine new to an object with 1024 writers cannot write to it.** Every
  other machine would refuse the change, so it is refused here too, with
  `SdkError::Write` and a message that says the object already has changes
  from the most machines it takes. The machine still reads the object and
  takes in the changes of the machines writing to it.
- **A machine restored from a backup must take a new machine number.** It
  has lost changes it may already have sent and cannot tell. Writing under
  its old number would reuse numbers the others have seen, and they would
  ignore those writes. Before it writes, give it a new `replica` number, a
  certificate for that number if it is on a network, and empty storage: it
  gets the data back from the others. Settings that name a new number for
  storage that already holds data are refused, as they always are.
- **`BoundedCounter::try_increment` adds all or nothing.** Where
  `increment` would stop at the counter's limit and keep less than it was
  given, it adds nothing and returns `NotAllowed`.
- **Several writes to a `Json` document can be made as one.**
  `Json::change_with` makes the writes it decides on, given what is at a
  path, in one step, and sends them to the other machines as one change.
- **A write can be made only if the object allows it at that moment.**
  `MvMap::put_if` and `MvMap::remove_if` are given the values at the key,
  and `Json::change_if` what is at a path, as they are when the write is
  made, in the same step, so nothing can change them in between. A check
  that says no changes nothing, and the write returns `NotAllowed` with its
  reason. A change another machine made that this one has not heard of yet
  is not seen.
- **A `Cart` quantity can go below zero.** `remove_qty` does not check. Use a
  `BoundedCounter` where that must not happen.
- **A mark needs a value.** `RichText::format` with an empty value returns
  `EmptyValue` and changes nothing. `unformat` removes a mark.
- **Text and its mark can go in together.** `RichText::insert_marked` puts
  text in and marks exactly those characters in one change. The mark is tied
  to the characters, not to their place, so another machine's text put in at
  the same moment never takes it.
- **`on_change` runs until what it returns is dropped.** Keep the
  `ChangeFollower` for as long as the callback should run; it also stops when
  the client is shut down. The callbacks run one after another, so a slow one
  holds up the ones after it. One that falls far enough behind misses
  changes, and `follow_changes` takes a second callback that is told when
  that happens.
- **`watch()` starts with the current value** and then gives a new item each
  time the value changes. It does not repeat a value that stayed the same. The
  stream ends when the client is shut down.
- **A setting the file gets wrong is an error.** A mistyped name, an `fsync`
  other than `"always"` or `"interval"`, or a zero where a time or a size is
  needed is reported and never quietly ignored.
- **A machine's number is kept beside its data,** in a file named after the
  storage file with `.replica` added. `Client::from_toml` refuses to open
  data that was written under another `replica` number. Opened under a new
  number, a machine would take its own earlier writes for another machine's.
  Saved data with no such note is refused too, since it is not known whose
  writes it holds. If it is known to have been written under this machine's
  number, `adopt_data = true` in `[storage]` takes it on and writes the note.
  A new or empty storage file is noted without asking.
- **Paths in a settings file are looked up next to that file,** unless they
  are absolute: the certificates and the storage path alike. The folder for
  the storage file is made if it is not there.
- **A machine that cannot save stops serving.** If saving to disk fails,
  for example because the disk is full, memory may hold a change that is not
  on disk, so nothing is shown from then on. Every change and every read
  returns `Storage` with the reason, and so does a change whose own save
  failed under `fsync = "always"`. Every `watch()` stream ends, and
  `Client::storage_failure` says why. `Client::saving_failed` finishes at
  that moment, for a program that would rather stop. Other machines get
  nothing more from it and see it as gone. Restart once the disk is fixed:
  the restart loads what is on disk.
- **Call `shutdown` when you are done.** It saves what is left, sends it to
  the machines still connected (waiting at most two seconds), stops the
  engine and releases the storage file.
- **A storage file that cannot be opened stops the start with `Storage`,**
  not with `Transport`, so the two can be told apart.
- **`Client::start_with_store` takes any store** in place of the storage
  file. Tests use it with a `MemStore` they can make fail.
- **Start a client inside a Tokio runtime.** The engine runs as background
  tasks.
