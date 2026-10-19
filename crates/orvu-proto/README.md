# orvu-proto

The messages Orvu machines send each other, as Rust types.

The messages are defined in the `.proto` files in this module's own
`proto/orvu/v1` folder. Those files are the one shared definition for every
programming language. They live inside the module so that a copy of it on
its own, as `cargo package` makes it, can still be built. This module is
the Rust version of them, generated each time it is built. It holds the message
types and nothing else: it does no networking and knows nothing about how the
data types merge.

## What is inside

Everything generated lives in `orvu_proto::v1`.

| File | What it defines |
|---|---|
| `core.proto` | The small shared pieces: `Hlc` (a timestamp), `Dot` (the name of one update), `VersionVector` and `DotContext` (which updates a machine has seen) |
| `crdt.proto` | `CrdtType`, and one delta message for each data type: counter, single value, set, map, a map that keeps clashing writes, list, JSON, formatted text, tree and the counter that cannot go below zero |
| `sync.proto` | What two machines say to each other, and the `SyncMessage` wrapper that carries any one of these messages (see the list below) |
| `snapshot.proto` | `SnapshotObject`, the whole state of one object, used for saving to disk and for catching up a machine that is far behind |

The messages in `sync.proto`:

| Message | What it is for |
|---|---|
| `Hello`, `HelloAck` | The greeting when two machines connect, and the reply to it |
| `Delta`, `DeltaBatch` | One change to one object, and a group of changes sent together |
| `Ack` | "This is what I have seen by now", so the other side can stop resending |
| `ContextDigest` | A request to catch up on anything missing |
| `SnapshotBatch` | The whole state of some objects, for a machine that is far behind |
| `MemberInfo`, `MembershipDigest` | What a machine knows about the others in the group |
| `BorrowRequest` | Asking another machine for allowance on a counter that cannot go below zero |
| `KeepAlive` | "Still here", sent on a connection that has had nothing else to send for about one anti-entropy interval |
| `Nack`, `NackReason` | An error reply, and the kinds of error. A `Nack` about one change names it by `object_id` and `dot` |

Also here:

- `Message`: the trait that gives every message `encode` and `decode`.
- `PROTO_VERSION`: the version number a machine announces when it connects.
  It is 2. Version 2 changed how a JSON document's changes are sent and what
  they mean, so a machine on version 1 reads them wrong.

## How to use it

### Turn a message into bytes and back

```rust
use orvu_proto::v1::Dot;
use orvu_proto::Message;

let dot = Dot { replica: 7, counter: 42 };

let bytes = dot.encode_to_vec();
let back = Dot::decode(&bytes[..]).unwrap();

assert_eq!(back, dot);
```

### Send several messages down one stream

A stream of bytes has no gaps in it, so each message is written with its length
in front. The reader uses the length to know where one message ends and the
next begins.

```rust
use orvu_proto::v1::{sync_message::Payload, Hello, SyncMessage};
use orvu_proto::{Message, PROTO_VERSION};

let hello = SyncMessage {
    payload: Some(Payload::Hello(Hello {
        proto_version: PROTO_VERSION,
        replica: 1,
        namespaces: vec!["default".to_string()],
    })),
};

let mut stream = Vec::new();
hello.encode_length_delimited(&mut stream).unwrap();
hello.encode_length_delimited(&mut stream).unwrap();

let mut rest = &stream[..];
let first = SyncMessage::decode_length_delimited(&mut rest).unwrap();
let second = SyncMessage::decode_length_delimited(&mut rest).unwrap();

assert_eq!(first, hello);
assert_eq!(second, hello);
assert!(rest.is_empty());
```

## Things to know

- **Building needs `protoc`.** The types are generated from the `.proto` files
  during the build. If `protoc` is not installed, the build stops and says so.
- **Change a message in its `.proto` file,** not in Rust. The Rust types are
  written again on every build.
- **Add fields, do not change or reuse them.** A new field with a new number is
  safe: an older machine skips it. Changing what an existing number means
  breaks every machine that has not been updated.
- **A new kind of message in `SyncMessage` is skipped by an older machine.**
  It reads the wrapper with no payload it knows, refuses that one message, and
  carries on with the connection. `KeepAlive` was added this way and needs
  nothing more from an older machine: the bytes arriving are all it is for.
- **Map entries are written in sorted order.** That makes this module's output
  repeatable for map fields. It is not a promise about other languages, which
  may write the same map in a different order.
- **Enum fields arrive as plain numbers.** Use `CrdtType::try_from(number)` to
  read one. It returns an error for a number this version does not know.
- **This module only describes messages.** It does not convert between these
  messages and the data types in `orvu-crdt`.
- **Nothing here checks the version.** `PROTO_VERSION` is the number to send.
  The engine (`orvu-runtime`) compares it with the other machine's number
  when two machines connect, and refuses a machine on another version with a
  `Nack` whose reason is `NACK_REASON_PROTO_VERSION`.
