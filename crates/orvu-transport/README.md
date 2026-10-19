# orvu-transport

The links between machines: how one machine connects to another and sends it
bytes.

This module opens connections, checks who is on the other end, and carries
messages across. It never looks inside a message. It knows nothing about the
data types or about syncing, and it does not depend on `orvu-core`.

## What is inside

| Piece | What it is |
|---|---|
| `Transport` | The trait every kind of link implements: `dial` to connect out, `listen` to accept connections |
| `Listener`, `Conn` | A listening address, and one open connection. A connection gives you streams, each with a write half and a read half |
| `QuicTransport` | The real link, over QUIC. Always encrypted, and both sides must show a certificate |
| `LinkTimes` | The time limits of a QUIC link: when a silent link is given up, how often a quiet one shows it is alive, and how long a dial may take |
| `MemTransport` | A link that stays inside one program, for tests. No sockets |
| `FaultTransport`, `Cuts` | A test tool that wraps another link and cuts it between chosen machines |
| `frame` | `write_frame` and `read_frame`: one message at a time over a stream |
| `Identity`, `PeerAddr` | Who is on the other end, and where to reach a machine |
| `generate_dev_ca`, `issue_node_cert` | Make certificates for development and tests |
| `TransportError` | Everything that can go wrong here |

## How to use it

### Two machines in one program

`MemTransport` connects code inside one program, which is what tests want.

```rust
use orvu_transport::frame::{read_frame, write_frame};
use orvu_transport::{MemTransport, PeerAddr};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    // One shared network, and an endpoint on it for each machine.
    let network = Arc::new(MemTransport::new());
    let alice = network.endpoint(1);
    let bob = network.endpoint(2);

    // Bob listens. Alice connects to him.
    let listener = bob.listen("bob", CancellationToken::new()).await.unwrap();
    let to_bob = alice.dial(PeerAddr::new("bob"), CancellationToken::new()).await.unwrap();
    let from_alice = listener.accept().await.unwrap();

    // Each side can see who the other is.
    assert_eq!(to_bob.identity().uri.as_deref(), Some("orvu://replica/2"));
    assert_eq!(from_alice.identity().uri.as_deref(), Some("orvu://replica/1"));

    // Alice opens a stream and sends one message. Bob reads it.
    let (mut alice_writes, _alice_reads) = to_bob.open_stream().await.unwrap();
    let (_bob_writes, mut bob_reads) = from_alice.accept_stream().await.unwrap();
    write_frame(&mut alice_writes, b"hello").await.unwrap();
    assert_eq!(read_frame(&mut bob_reads).await.unwrap(), b"hello");
}
```

### Two machines over an encrypted link

Every machine needs its own certificate, signed by an authority that all the
machines trust. The helpers below make both, for development and tests.

```rust
use orvu_transport::frame::{read_frame, write_frame};
use orvu_transport::{
    generate_dev_ca, install_default_crypto, issue_node_cert, PeerAddr, QuicTransport, Transport,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    // Call this once when the program starts.
    install_default_crypto();

    // One authority, and a certificate from it for each machine.
    let authority = generate_dev_ca();
    let alice_cert = issue_node_cert(&authority, 1);
    let bob_cert = issue_node_cert(&authority, 2);

    let alice =
        QuicTransport::new(&alice_cert.cert_pem, &alice_cert.key_pem, &authority.cert_pem).unwrap();
    let bob =
        QuicTransport::new(&bob_cert.cert_pem, &bob_cert.key_pem, &authority.cert_pem).unwrap();

    // Port 0 lets the system pick a free port. Ask the listener which one.
    let listener = bob.listen("127.0.0.1:0", CancellationToken::new()).await.unwrap();
    let address = PeerAddr::new(listener.local_addr());

    // Connecting takes both sides, so run the two halves together.
    let (to_bob, from_alice) =
        tokio::join!(alice.dial(address, CancellationToken::new()), listener.accept());
    let (to_bob, from_alice) = (to_bob.unwrap(), from_alice.unwrap());

    // The identity comes from the certificate the other side showed.
    assert_eq!(from_alice.identity().uri.as_deref(), Some("orvu://replica/1"));

    let (mut alice_writes, _alice_reads) = to_bob.open_stream().await.unwrap();
    write_frame(&mut alice_writes, b"hello").await.unwrap();
    let (_bob_writes, mut bob_reads) = from_alice.accept_stream().await.unwrap();
    assert_eq!(read_frame(&mut bob_reads).await.unwrap(), b"hello");
}
```

### Make certificates from the command line

```text
cargo run -p orvu-transport --example keygen -- --out certs --replicas 1,2,3
```

This writes an authority certificate and one certificate and key for each
machine number into the `certs` folder.

## Things to know

- **A link carries bytes and nothing more.** What the bytes mean is up to the
  code that uses the link.
- **Over QUIC, both sides must show a certificate** signed by the authority
  they were set up to trust. A machine without one gets no usable connection.
- **A machine's identity is in its certificate,** as `orvu://replica/<number>`.
  Read it with `Conn::identity()`. Any machine with a valid certificate can
  connect. Deciding which machines are welcome is up to the caller.
- **The certificate helpers are for development and tests.** They are not a
  service for handing out certificates in production.
- **A message can be at most 64 MiB** (`MAX_FRAME`). `write_frame` refuses a
  larger one and `read_frame` refuses to read one. Check the size before
  writing, because a failed write usually ends the connection.
- **`read_frame` waits as long as the other side takes.** Add your own time
  limit if you need one.
- **A listener limits connections that are still being set up.** At most 8
  at once from one address, 32 from one block of addresses (the first 24
  bits of an IPv4 address, or the first 64 bits of an IPv6 one), and 256 in
  all. An attempt over a limit is refused at once. Set-up takes milliseconds,
  so honest machines rarely come near these. They stop one attacker, even
  one with many addresses, from taking every place.
- **`accept()` also reports handshakes that failed.** An error from it is
  about one stranger's attempt, not about the listener. Keep calling it.
- **Over QUIC the other side only sees a new stream once something has been
  written to it.** Write first, then expect `accept_stream` to return.
- **A machine is dialed by address or by name,** such as `10.0.0.5:7000` or
  `box3.example.com:7000`. A name is looked up on every dial, so a machine
  whose address changed is found again. If a name has several addresses,
  they are tried in turn, each a quarter of a second after the one before
  and without waiting for it to give up, and the first to connect is used. Listening takes an
  address, not a name.
- **A QUIC link that hears nothing for 30 seconds is given up,** and a link
  with nothing to send sends a tiny packet every 5 seconds so that a healthy
  quiet link is not. A machine whose cable was pulled, or that froze, is
  noticed this way. Both ends ask for an idle time, and the shorter one is
  used. Change these with `QuicTransport::with_times` and `LinkTimes`.
- **A dial gives up after 10 seconds** (`LinkTimes::dial`), and at once when
  its cancel token fires. A machine that is switched off does not refuse a
  dial, it never answers, so without a limit the dial would wait until the
  idle time had passed.
- **`FaultTransport` is for tests only.** It makes messages vanish without
  telling either side, to act out a network split.
