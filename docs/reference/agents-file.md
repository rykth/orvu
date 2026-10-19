# The agents file and rules

## Where it is

A file called `agents`:

- for a local sidecar, in its data folder;
- for a sidecar started with `--config`, next to the settings file.

While there is no such file, every program that can reach the socket is let
in. Once there is one, a program must give one of its keys. A file with no
agents left in it lets nobody in. A file that cannot be read lets nobody in
either, and the refusal says what is wrong with it.

## Its lines

Change it with `orvud agent`, not by hand. Each line is:

```text
<name> <fingerprint> [<rules>]
```

- `name` is the agent's name.
- `fingerprint` is the SHA-256 of the agent's key, as 64 hex digits. The key
  itself is not kept, so it cannot be read back from the file.
- `rules`, if there are any, are separated by spaces (commas work too), as
  on the command line. No rules means the agent may do everything.

Blank lines and lines starting with `#` are skipped. A name may appear only
once. A key starts with `orvu-key-`, so it is easy to spot as a secret.

The running sidecar reads the file again whenever it changes. A session is
ended only once the file has been left alone for two seconds, so an editor
that saves in two steps does not cut agents off.

## Rules

A rule is `block:level`.

| Block | Levels, each allowing the ones before it | Covers |
|---|---|---|
| `notes` | `read`, `own`, `write` | Simple and structured notes |
| `tasks` | `read`, `own`, `write` | The task board and firm tasks' tickets |
| `documents` | `read`, `write` | Documents |
| `tallies` | `read`, `write` | Tallies |
| `inbox` | `read`, `write` | Its own inbox, and sending to anyone |
| `roster` | `read`, `write` | The roster |
| `budgets` | `read`, `spend`, `top-up` | Budgets |
| `objects` | `read`, `write` | Plain objects a program names itself |

Also:

- `none` alone allows nothing.
- `all` takes the rules away: the agent may do everything again.
- A block left out may not be used at all.

### What `own` allows

- **`notes:own`**: read every note; add simple notes; change or remove a
  note only while every value it holds was written by this agent. It changes
  no structured note, as those keep no writer's name.
- **`tasks:own`**: read every task; add tasks as itself; claim an open task;
  change, finish, give back or move only a task it added or holds. Only the
  agent that added a task removes it. A firm task's tickets, and the record
  of a takeover, need `tasks:write`.

### What an agent with rules always keeps to itself

- It reads only its own inbox, and the one for everyone.
- It joins and leaves the roster under its own name only.
- A note's author, a message's sender, the agent that added a task, and its
  lines in a budget's spent and topped-up records, must be its own name.
- The sidecar writes who last changed a task (`updated_by`) itself.

### What every agent can see

The team's list of what it has, `orvu/<team>/index`, is readable by every
agent whatever its rules, because every block needs it. So the names of
documents, structured notes, budgets and tallies, and who has been sent
messages, are seen by all. What is in them is not. Keep secrets out of
names.

Names under `orvu/_machines/` belong to the machines and are refused to
every program.

## Commands

| Command | What it does |
|---|---|
| `orvud agent add <name> --local` | Makes an agent and prints its key once |
| `orvud agent add <name> --address <host:port> --connection <file> --local` | Also writes a connection file for an agent elsewhere |
| `orvud agent remove <name> --local` | Turns it away at its next call |
| `orvud agent list --local` | Lists the agents and their rules |
| `orvud agent allow <name> <rule> ... --local` | Sets its rules. `all` takes them away |

Use `--config <path.toml>` in place of `--local` for a sidecar started from
settings. Two commands run at the same moment take turns.

The [`orvud` README](../../crates/orvud) says exactly how each rule is
checked, batches and the task tree included.
