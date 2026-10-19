# Troubleshoot

Errors have names, and each says what went wrong. In Python they are all
under `orvu.OrvuError`; the TypeScript package has the same names.

## The sidecar does not start, or cannot be reached

**`SidecarUnavailable`.** Read the message: it passes on what the sidecar
said. Then:

- Look at `orvud.log` in the data folder.
- With a socket given (`socket=` or `ORVU_SOCKET`), the libraries never start
  a sidecar. Start that one yourself, with `orvud --config ...`.
- `connect()` with no socket uses only a local sidecar. If a sidecar serving
  other machines holds the same socket, it is not used.
- On Windows there is no sidecar: connect to one elsewhere with a connection
  file.

**`VersionMismatch`.** A sidecar of another release is running, often right
after an upgrade. The message names the process. Stop it, and the next
`connect` starts the new one.

**"the data folder must be private".** The folder exists and other users can
get into it. The message gives the `chmod` command that fixes it.

**Another sidecar already uses the folder or socket.** One sidecar per data
folder, and one per socket. Stop the other one.

## A data folder copied from another computer is refused

The folder notes which computer it is used on, because two computers must
never run as one machine. Run once, by hand:

```text
orvud --local --data-dir <folder> --moved        # moved here; the old computer no longer uses it
orvud --local --data-dir <folder> --new-number   # copied; the old computer still uses it
```

The message gives the command with the folder filled in. A folder restored
from a backup takes `--restored` instead: see
[Back up and restore](backup-restore.md).

## A call fails

| Error | What it means |
|---|---|
| `NotAllowed` | No key, an unknown key, or the agent's rules do not allow this. The message says which |
| `TimedOut` | No answer in time. A write may still have gone through: read before writing again |
| `TypeMismatch` | The name is already used by another kind of object |
| `ClassMismatch` | The name was made under other rules: a plain counter and one that cannot go below zero cannot share a name |
| `OutOfBounds` | A position past the end of a list or text |
| `BadRequest` | A wrong argument, caught before anything was sent |
| `TooLarge` | A request, reply or number of watches over the limit |
| `JsonError` | A JSON write the document cannot take, such as a whole number a float cannot hold |
| `UnknownTask` | No task with that id on the team's board |
| `SearchUnavailable` | Search by meaning could not be done: the model could not be fetched or read, or the sidecar was started not to search. The message says which |
| `Clash` | A note two agents wrote at once. Write it again to settle it |
| `TicketUnavailable` | A firm task's ticket cannot be taken. See [stuck firm tasks](../concepts/firm-tasks.md#a-stuck-firm-task) |
| `EngineError` | The engine refused the write, or a watch lost its session twice in 10 seconds. The message says why |

A budget saying no is not an error: `spend` returns `False`.

## Machines do not sync

- Is the UDP port (7440 unless chosen) open between them?
- Did each machine start? Each prints one line when ready.
- Are the clocks right? A change stamped more than five minutes ahead is
  refused.
- A machine that comes back is found within a few seconds, not at once.
- Two sidecars sync only within one namespace, made by one `orvud setup`.

## An agent sees the wrong data

- An MCP agent program starts the server with few of your terminal's
  settings. If you set `XDG_DATA_HOME`, set it for the agent program too.
- Two MCP servers started in the same folder for the same team, with no
  `--me`, get the same made-up name and count as one agent. Give each a
  name.
- A team from `orvud setup` starts empty. Your local sidecar's data stays
  where it was.

## A watch keeps ending with `EngineError`

The sidecar keeps 1,024 sessions and drops the one used longest ago. Many
busy programs push a quiet watch out. Use fewer sessions: give each program
one client, and each program its own agent and key.

## Search fails

The first search fetches a 30 MB model. On a machine with no internet, fetch
the three files elsewhere, check them against the SHA-256 table in the
[`orvud` README](../../crates/orvud), and start the sidecar with
`--model <folder>`, or set `ORVU_MODEL=<folder>`.

## Still stuck

Turn on the log at `debug` level (see
[See what the sidecar is doing](observe.md)), and open an issue with what
you ran, what you expected, and the log.
