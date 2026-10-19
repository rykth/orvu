# Limits

What Orvu does not do, and what grows over time. Read this before you build
on it.

## Nothing is deleted for good

- **Removed items leave a small record behind.** Sets, maps, lists and text
  remember what was removed, so that a change arriving late cannot bring it
  back. Nothing cleans those records up.
- **Overwritten parts of a JSON document stay in memory,** out of sight.
  They could only be dropped once every machine is known to have the write
  that replaced them, and nothing knows that.
- **An inbox never shrinks.** Reading a message does not remove it. What an
  agent was given is recorded too, so that it is not given again. Messages
  are kept in pages; a finished page is no longer read, but it is kept.
- **A firm task's old tickets stay in the sidecar.** Each move of a firm
  task leaves two small counters behind, and puts one more small number on the
  task.
- **A tally only goes up.**
- **A roster keeps an agent that crashed,** as not here, for up to a day.

So a team that runs for a long time, with many messages or many removals,
grows. How much was not measured over weeks of use.

## Consistency

- **Copies agree eventually, not at once.** A change reaches the other
  machines a moment later. A reader on another machine can see the old
  value in between.
- **"The later write wins" goes by the machines' clocks.** Keep them right.
  The earlier write is dropped without a word.
- **A soft task claim can be given twice.** Two agents that claim at the
  same moment, or on machines cut off from each other, can both be told yes.
  Use a firm task where that matters.
- **A firm task gives one holder at a time, not one doer.** An agent that
  lost its task without knowing can still act on it. Carry the ticket
  number to what it changes: see [Firm tasks and tickets](firm-tasks.md).
- **A budget is cautious across machines.** A machine that is cut off can
  spend only what it holds, so it may be told no while there is budget
  elsewhere. `left()` counts every machine's share, the unreachable ones
  too. It is never overspent.
- **A document's merge is mechanical.** Two agents that rewrite the same
  sentence at once get both versions run together.
- **Messages are put in order by the sender's clock.**

## Size and count

- A request may be up to 64 MiB on the local socket, and 4 MiB over the
  network. A reply may be up to 64 MiB.
- A batch holds up to 1,024 operations, and its replies up to 4 MiB.
- One client can have 256 `watch()` loops open, and a sidecar 4,096 in all.
- A sidecar keeps 1,024 sessions. One agent may hold 256 of them.
- An object takes changes from at most 1,024 machines.
- A JSON document nests at most 32 levels deep. Its numbers are 64-bit
  floats.
- Names of teams, budgets, documents, tallies and structured notes are
  letters, digits, dots, dashes and underscores, at most 64 characters.

## Security

- **Keys keep agents apart only if they cannot change the sidecar's
  files.** See [Keys and rules](keys-and-rules.md).
- **Machines of a team trust each other.** A machine with a valid
  certificate is believed.
- **Certificates do not run out,** and there is no way yet to withdraw one.
  To shut a machine out, make the team's settings again without it.
- **The MCP server over HTTP is not encrypted.** Put it behind something
  that provides TLS, or use an SSH tunnel, on a network you do not trust.
- **The page of numbers has no password.** Serve it on `127.0.0.1`.

## Platforms

- The sidecar runs on Linux and macOS. There is no sidecar for Windows: a
  program on Windows connects to one elsewhere over the network. Windows on
  ARM is not supported.
- The TypeScript package runs in Node, not in a browser, and is not
  published to npm yet.
- On a Mac, keeping the sidecar running with launchd has not been tried.

## Search

- The first search fetches a 30 MB model from the internet, once.
- The model knows English, and goes by the words of a text, not their
  order.
- Messages are not searched.

## Where next

- [Troubleshoot](../how-to/troubleshooting.md)
- Each module's README ends with a "Things to know" list with every limit of
  that module.
