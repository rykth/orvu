# Give agents keys and rules

Why you would, and what keys do not protect against, is in
[Keys and rules](../concepts/keys-and-rules.md).

Every `orvud agent` command takes `--local` for the sidecar the libraries
start (add `--data-dir <folder>` if it is not the default), or
`--config <path>/orvu.toml` for a sidecar started from settings. The sidecar
does not need to be stopped: it reads the agents file again when it changes.
From the TypeScript package, run the same commands as
`npx --no-install orvud agent ...`.

## 1. Give each agent a key

```text
orvud agent add alice --local
```

It prints the key once. Put it in the agent's environment:

```text
ORVU_KEY=orvu-key-... python my_agent.py
```

or pass it in code, `Team.connect(key=...)` in Python and
`Team.connect("default", { key })` in TypeScript. The agent needs no `me`:

```python
from orvu import Team

with Team.connect() as team:   # ORVU_KEY is set
    print(team.me)             # alice
```

For an MCP agent: `claude mcp add orvu --env ORVU_KEY=orvu-key-... -- orvu mcp`.

Give each program its own agent and key. One agent may hold at most 256
sessions, and many programs sharing one key push each other out.

## 2. Give an agent rules

```text
orvud agent allow summariser notes:write tasks:read budgets:spend --local
```

The blocks are `notes`, `tasks`, `documents`, `tallies`, `inbox`, `roster`,
`budgets` and `objects`. The levels are in the
[agents file reference](../reference/agents-file.md). Some useful sets:

| Agent | Rules |
|---|---|
| Reads everything, changes nothing | `notes:read tasks:read documents:read tallies:read inbox:read roster:read budgets:read` |
| A worker that takes tasks and reports | `notes:own tasks:own inbox:write roster:write budgets:spend tallies:write` |
| A writer on shared documents | `documents:write notes:read inbox:write roster:write` |
| The one that tops up budgets | `budgets:top-up` |

Rules apply from the agent's next call. A call it may not make raises
`NotAllowed` (MCP tools answer with an error that says why).
`orvud agent allow summariser all` takes the rules away.

## 3. Check who is let in

```text
orvud agent list --local
```

## 4. See who did what

```text
orvud audit --local --agent alice --since 2h
orvud audit --local --not-allowed
```

## 5. Turn an agent away

```text
orvud agent remove alice --local
```

It is turned away at its next call. Removing the last agent lets nobody in;
delete the `agents` file in the data folder to let every program in again.

## Keep an untrusted agent away from the files

A key is only as safe as the `agents` file. Run an agent you do not trust:

- in a container that is given the socket but not the data folder, or
- on another machine, or in a container, with a connection file: see
  [Serve agents on other machines and in containers](remote-agents.md).
