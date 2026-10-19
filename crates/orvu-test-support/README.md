# orvu-test-support

Helpers for the tests of the other modules. Nothing here is part of Orvu
itself, and nothing outside tests uses it.

## What is inside

| Piece | What it is |
|---|---|
| `free_port` | A port for a machine in a test to listen on, that no other test is given |
| `release` | Gives a port back before the program ends |
| `window` | Which ports those are taken from on a given system |
| `within` | Waits for a step of a test, and fails the test, naming the step, if it takes too long |

## How to use it

A test that starts a machine has to say which port it listens on.

```rust
let port = orvu_test_support::free_port();
let address = format!("127.0.0.1:{port}");
assert_ne!(port, orvu_test_support::free_port());
# let _ = address;
```

A test that waits on a machine or on the network waits through `within`, so
a step that hangs fails the test with a message saying which step it was.

```rust,ignore
let node = orvu_test_support::within("starting the node", Duration::from_secs(30), Node::spawn(net, cfg)).await;
```

## Things to know

- **The port is not asked of the system.** A port the system gave and got
  back can be handed to the next program that asks, and every link a machine
  dials asks for one. Ports are taken from where this system hands none out.
- **A port is held until the program that picked it ends,** by a lock on one
  byte of `orvu-test-ports.lock`, the byte as far in as the port's number.
  The system lets go of a program's locks when it ends, however it ends. A
  program keeps that one file open, however many ports it picks, and anyone
  may use the file, whoever made it.
- **The file is in the folder `TMPDIR` names, or in `/tmp`.** No other name
  for the temporary folder is looked at.
- **If the file is taken away while a program holds ports in it,** the
  program locks them again in the file that is there now the next time it
  picks a port. One that another program took in between cannot be got
  back. Nothing tells either program: until it picks again, a program does
  not know that its ports are no longer held where others look. A person
  should not delete the file while tests run. A tool that clears out old
  files in `/tmp`, such as `systemd-tmpfiles`, usually takes only files left
  untouched for days, far longer than a test run; to keep it away from the
  file altogether, give it a rule such as `x /tmp/orvu-test-ports.lock`, or
  point `TMPDIR` at a folder it does not clear.
- **The Python tests pick the same way and lock the same file**
  (`python/tests/ports.py`), so Rust and Python tests that run side by side
  are never given the same port.
- **If no port is left, or a port cannot be looked at, it panics and says
  why.** It never goes on trying for ever.
