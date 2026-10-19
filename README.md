# Orvu

**Shared memory for teams of AI agents.**

Orvu lets many programs read and write the same notes, task lists,
documents and budgets at the same time, from different machines, without
stepping on each other. Every machine keeps its own copy of the data and
works on it directly, with no waiting for a server. Changes travel between
the machines in the background and are merged automatically, so all the
copies end up the same. When two agents write the same note at the same
moment, neither write is lost: both are kept, with who wrote each, until
someone settles it.

<p align="center">
  <img src="docs/assets/write-spreads.svg" alt="A note written on machine alpha travels to beta and gamma; once every machine holds it, the three rings come together as the Orvu mark" width="480">
</p>

## Quick start

### Python

You need Python 3.10 or newer. On Linux:

```text
pip install orvu
```

On a Mac, build the package from this repository for now
([how](docs/getting-started/install.md#on-a-mac-or-on-windows)).

```python
from orvu import Team

with Team.connect(me="alice") as team:
    team.notes.put("deploy-date", "Friday")

    task = team.tasks.add("Summarise the Q3 report")
    if team.tasks.claim(task):
        team.tasks.finish(task, result="It went up.")

    tokens = team.budget("tokens")
    tokens.top_up(1000)
    if tokens.spend(300):
        print("spent 300,", tokens.left(), "left")
```

Run a second program with `me="bob"` and it sees the same note, task and
budget. There is no server to start: the package brings a small background
program, `orvud`, and starts it the first time it is needed.

### TypeScript (Node 20 or later)

```text
npm install @orvuhq/orvu
```

On Linux on Intel/AMD that also brings the sidecar. For other machines, see
[Install](docs/getting-started/install.md#typescript).

```ts
import { Team } from "@orvuhq/orvu";

const team = await Team.connect("default", { me: "bob" });
console.log(await team.notes.get("deploy-date")); // Friday
await team.close();
```

A Python program and a Node program of the same user share one `orvud`, and
see the same data.

### An agent with no code (MCP)

```text
claude mcp add orvu -- orvu mcp --me researcher-1
```

The agent gets tools for the team's notes, tasks, budgets, messages,
documents, tallies and roster. Other agent programs are set up the same way:
see [Your first MCP agent](docs/getting-started/first-mcp-agent.md).

## Documentation

The pages under [`docs`](docs) are in four kinds, so you can find what you
need: **getting started** walks you through your first steps, **concepts**
explain how Orvu works and why, **how-to guides** help you do one job, and
**reference** lists every command, setting and method. Each module of the
code also has a `README.md` of its own (see [What is here](#what-is-here)).
Those are the most detailed reference, and the pages link to them rather than
repeat them.

### Getting started

1. [Install](docs/getting-started/install.md): what you need, and what you get.
2. [Your first agent in Python](docs/getting-started/first-agent-python.md)
3. [Your first agent in TypeScript](docs/getting-started/first-agent-typescript.md)
4. [Your first MCP agent](docs/getting-started/first-mcp-agent.md): tools for an
   agent program such as Claude Code or Cursor, with no code.
5. [See it all at once in Docker](examples/compose/README.md): three
   machines and agents in Python and TypeScript, started with one command.

### Concepts

- [How it works](docs/concepts/how-it-works.md): agents, the library, the
  sidecar, the engine, and other machines.
- [Merging in plain words](docs/concepts/merging.md): what the shared data types
  are, and why copies always end up the same.
- [A clash is kept](docs/concepts/clashes.md): what happens when two agents write
  the same note at once.
- [Firm tasks and tickets](docs/concepts/firm-tasks.md): one holder at a time, and
  how to stop work being done twice.
- [Keys and rules](docs/concepts/keys-and-rules.md): who may connect, and what
  each agent may do.
- [Saving and syncing](docs/concepts/saving-and-syncing.md): what is on disk, and
  how machines catch up.
- [Limits](docs/concepts/limits.md): what Orvu does not do, and what grows.

<p align="center">
  <img src="docs/assets/clash-kept.svg" alt="alice on alpha writes Friday and bob on gamma writes Monday at the same moment; every machine keeps both, with who wrote each; carol settles it with Friday, and the three rings come together as the Orvu mark" width="480">
</p>

### How-to guides

- [Run one team on several machines](docs/how-to/several-machines.md)
- [Give agents keys and rules](docs/how-to/keys-and-rules.md)
- [Serve agents on other machines and in containers](docs/how-to/remote-agents.md)
- [Use the MCP server](docs/how-to/mcp-server.md), over stdio or HTTP
- [Use the building blocks](docs/how-to/building-blocks.md): notes, tasks,
  budgets, documents, inbox, tallies, roster and search
- [Watch for changes](docs/how-to/watch-changes.md)
- [See what the sidecar is doing](docs/how-to/observe.md): logs, numbers and the
  audit record
- [Back up and restore](docs/how-to/backup-restore.md)
- [Troubleshoot](docs/how-to/troubleshooting.md)

### Reference

- [The `orvud` command and its settings](docs/reference/orvud.md)
- [The agents file and rules](docs/reference/agents-file.md)
- [Environment variables](docs/reference/environment.md)
- [Python API overview](docs/reference/python-api.md)
- [TypeScript API overview](docs/reference/typescript-api.md)
- [MCP tools, resources and prompts](docs/reference/mcp.md)
- [Modules](docs/reference/modules.md): the Rust crates and what each one does

## What is here

Each module has a `README.md` of its own that says what it does and shows how
to use it. These are the detailed reference, and the Rust examples in them
are run as tests. They are listed from the bottom up: each builds on the ones
above it.

| Folder | What it does |
|---|---|
| [`crates/orvu-crdt`](crates/orvu-crdt) | The data types that can merge: counters, sets, maps, maps that keep clashing writes, lists, text, JSON documents, trees |
| [`crates/orvu-proto`](crates/orvu-proto) | The messages machines send each other, and the `.proto` files that define them |
| [`crates/orvu-core`](crates/orvu-core) | One machine's copy of the data, and turning changes into messages and back |
| [`crates/orvu-store`](crates/orvu-store) | Saving to disk |
| [`crates/orvu-transport`](crates/orvu-transport) | Encrypted links between machines |
| [`crates/orvu-runtime`](crates/orvu-runtime) | The engine: it sends and receives changes, saves them, and catches up after being away |
| [`crates/orvu-sdk`](crates/orvu-sdk) | The friendly way to use all of this from Rust |
| [`crates/orvu-search`](crates/orvu-search) | Finding text by what it means: the model and the index that search by meaning uses |
| [`crates/orvud`](crates/orvud) | The sidecar: a small background program that lets other languages use Orvu. Its `proto` folder defines what clients send it |
| [`python`](python) | The Python client, the `orvu mcp` server and the demos. It talks to the sidecar |
| [`typescript`](typescript) | The TypeScript client, for Node. It talks to the sidecar |
| [`crates/orvu-test-support`](crates/orvu-test-support) | Helpers for the tests of the other modules. Not part of Orvu itself |
| [`conformance`](conformance) | Test cases that any other implementation of the data types must also pass |
| [`examples/compose`](examples/compose) | A demo in Docker: a team on three machines, with agents in Python and TypeScript and one over MCP, that runs with one command |

## Building and testing

You need Rust 1.89 or newer, and `protoc` (the Protocol Buffers compiler) on
your PATH.

```text
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The Python and TypeScript clients have tests of their own, which start real
sidecars: see [python/README.md](python/README.md#working-on-this-package) and
[typescript/README.md](typescript/README.md#working-on-this-package).

## Licence

Orvu is licensed under the Apache License, Version 2.0
([LICENSE](LICENSE)). Unless you say otherwise, any contribution you send for
inclusion in Orvu is licensed the same way, with no added terms.
