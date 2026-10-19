# Keys and rules

Out of the box, every program that can open the sidecar's socket is let in,
and an agent is called whatever it says it is. That suits one person running
their own agents on their own computer. When you want to know who did what,
or keep an agent to its own corner, give agents keys and rules.

## Keys say who connects

`orvud agent add alice --local` makes a key for alice and prints it once. The
sidecar keeps only a fingerprint of it, in a file called `agents`, so the
key cannot be read back from there.

- **From the first agent on, a key is needed.** A program with no key, or one
  the sidecar does not know, is refused with `NotAllowed`.
- **The key is the name.** A program that gives alice's key is alice. It
  needs no `me`, and its notes, tasks and messages carry that name.
- **Removing an agent turns it away at its next call.** Its open watches end
  within a second.
- **Removing the last agent lets nobody in,** not everybody. Delete the
  `agents` file to let every program in again.
- **Over the network, a key is always needed.** A sidecar started with
  `--network` refuses to start until there is at least one agent.

## Rules say what an agent may do

An agent with no rules may do everything. Rules narrow that, block by block:

```text
orvud agent allow summariser notes:write tasks:read budgets:spend --local
```

| Block | Levels |
|---|---|
| `notes`, `tasks` | `read`, `own`, or `write` |
| `documents`, `tallies`, `inbox`, `roster` | `read` or `write` |
| `budgets` | `read`, `spend`, or `top-up` |
| `objects` (plain objects a program names itself) | `read` or `write` |

Each level allows the ones before it. `own` sits between reading and
writing: with `notes:own` an agent reads every note and changes only notes
it wrote; with `tasks:own` it reads every task, can add tasks and claim open ones,
and changes only tasks it added or holds. A block left out may not be used.
`orvud agent allow summariser all` takes the rules away.

An agent with rules also keeps its own things to itself: only it reads its
inbox, it changes only its own place on the roster, and the names that say
who wrote something (a note's author, a message's sender, who added a task,
who spent from a budget) are checked to be its own.

## The audit record

The sidecar records every change, with the agent that made it, and
everything an agent was not allowed to do. It keeps the shape of a change,
not what was written: that alice changed the note called deploy, not what
the note now says. `orvud audit` shows it.

## What keys and rules do not do

Be clear about these before you rely on keys:

- **Keys keep agents apart only if they cannot change the sidecar's files.**
  A program running as your user can add a key of its own to the `agents`
  file. Run an agent you do not trust where it can reach the socket but not
  the data folder, such as in a container given only the socket, or on
  another machine with a connection file.
- **Rules go by block, not by object.** An agent that may write tasks may
  change any task, not only its own, unless it has `tasks:own`.
- **The sidecar checks operations, not what they write.** Apart from the
  names it checks, an agent that may write a block can write anything into
  it.
- **Names are seen by all.** Every agent can read the team's list of what it
  has, so the names of documents, structured notes, budgets and tallies, and
  who has been sent messages, are seen by every agent whatever its rules.
  Keep secrets out of names.
- **Each machine has its own agents.** The machines of a team do not share
  their agents files or their audit records.
- **Machines trust each other.** A machine with a valid certificate is
  trusted with everything it sends.

## Where next

- [Give agents keys and rules](../how-to/keys-and-rules.md), step by step
- [The agents file and rules](../reference/agents-file.md), every detail
