# orvu (Python)

The Python client for Orvu.

Orvu keeps shared data, such as counters, lists, documents and budgets, the
same on every machine that uses it. This package is how a Python program reads
and changes that data. It talks to `orvud`, a small program that runs in the
background on the same machine and does the real work. That program comes
inside the package and is started for you.

## What is inside

| Piece | What it is |
|---|---|
| `Team` | What a team of agents shares: notes, a task board, budgets, documents, an inbox, tallies and a roster, and search by meaning over its notes, documents and tasks. Start here |
| `orvu.connect()` | Connects to this user's `orvud`, starting it if it is not running, and returns a `Client` |
| `orvu.connect(socket)` | Connects to an `orvud` you started yourself |
| The `orvud` command | The sidecar program itself, for running it by hand |
| The `orvu mcp` command | Gives a team's notes, tasks, budgets, messages, documents and tallies to an agent as tools, with no code. See [`MCP.md`](MCP.md) |
| The `orvud setup` command | Makes the settings for a team that lives on several machines. See [`MACHINES.md`](MACHINES.md) |
| `Client` | Hands out objects by name, and sends batches |
| `Counter`, `BoundedCounter`, `Register`, `Set`, `Map`, `MvMap`, `Cart`, `List`, `Json`, `RichText`, `Tree` | The eleven kinds of shared object |
| `Batch` | Several operations sent in one message |
| `OrvuError` and the errors under it | What can go wrong, each with its own name |

## How to use it

```text
pip install orvu
```

There is nothing else to set up. The first `orvu.connect()` starts the
sidecar in the background, and every program you run after that shares it.

New here? [`QUICKSTART.md`](QUICKSTART.md) goes through this step by step.

### A team: notes, tasks, a budget and more

`Team` is the friendly layer. Every agent that connects with the same team
name shares the same notes, task board, budgets, documents, inbox, tallies
and roster.

```python
from orvu import Team

with Team.connect(me="researcher-2") as team:
    # Notes: small facts by name.
    team.notes.put("deploy-date", "Friday")
    assert team.notes.get("deploy-date") == "Friday"

    # Tasks: add, claim, finish.
    task = team.tasks.add("Summarise the Q3 report")
    if team.tasks.claim(task):
        team.tasks.still_working(task)   # now and then, or the claim runs out
        team.tasks.finish(task, result="It went up.")
    assert team.tasks.get(task).state == "done"

    # A firm task: one agent at a time can have it.
    invoice = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim(invoice) is True

    # A budget that cannot be overspent.
    tokens = team.budget("tokens")
    tokens.top_up(2000)
    assert tokens.spend(1500) is True
    assert tokens.spend(1500) is False   # not enough left, nothing spent
    assert tokens.left() == 500

    # React when something changes.
    for tasks in team.tasks.watch():
        assert len(tasks) == 2
        break   # a real agent would carry on here

    # A document several agents write together. Each part says who wrote it.
    report = team.document("q3-report")
    report.append("Sales rose 4%.")
    assert report.written_by() == [("Sales rose 4%.", "researcher-2")]

    # Search by meaning: the notes and documents closest to a question.
    found = team.search("When do we ship?")
    assert found[0].name == "deploy-date"

    # Messages to one agent, or to everyone.
    team.inbox.send("researcher-1", {"task": task.id, "say": "please check this"})
    team.inbox.send_all("Deploy is on Friday.")
    assert [message.body for message in team.inbox.unread()] == ["Deploy is on Friday."]

    # Counting, per agent and in total.
    team.tally("tokens-used").add(1500)
    assert team.tally("tokens-used").by_name() == {"researcher-2": 1500}

    # Who is on the team right now.
    team.roster.join()
    assert team.roster.here() == ["researcher-2"]
```

| On a team | What it does |
|---|---|
| `team.notes` | `put`, `get`, `remove`, `all`, `author`, `authors`, `watch`. A note holds text, a number, true or false, a list or a dict. `get` raises `orvu.Clash` for a note agents wrote at the same time. `structured(name)` gives a note that is a JSON document |
| `team.tasks` | `add`, `list`, `get`, `update`, `claim`, `claim_ticket`, `still_working`, `still_holding`, `release`, `finish`, `move`, `children`, `watch` |
| `team.budget(name)` | `top_up`, `spend`, `left`, `spent_by`, `topped_up_by`, `watch` |
| `team.document(name)` | `append`, `insert`, `delete`, `text`, `written_by`, `format`, `unformat`, `to_spans`, `watch`. `team.documents()` lists them |
| `team.inbox` | `send(to, body)`, `send_all(body)`, `unread`, `messages`, `history`, `watch` |
| `team.tally(name)` | `add`, `total`, `by_name`, `watch`. `team.tallies()` lists them |
| `team.roster` | `join`, `leave`, `here`, `members`, `watch` |
| `team.search(query, limit=5)` | The notes, structured notes, paragraphs of documents and tasks whose meaning is closest to `query`, the closest first, each a `Found` with its `kind`, `name`, `text`, `score` and, for a document, `paragraph`, or for a task, `title`. See "Search by meaning" below |
| `team.index()` | What the team has: every object its blocks have made, but for a firm task's tickets that can never be used again |
| `team.client` | The plain connection underneath, for everything below |

To see it at work, run `python -m orvu.demo`: three agents share a task list
and a budget that is too small for all the work. It runs on a sidecar of its
own and leaves nothing behind. `python -m orvu.demo_machines`
does the same on three machines, switches one of them off in the middle and
on again, and shows that the three end up the same.

How fast it is, and how to measure it on your own computer, is in
[`MEASUREMENTS.md`](MEASUREMENTS.md).

The rest of this page is about the plain shared objects the blocks are built
on. Use them directly when no block fits.

### Search by meaning

`team.search("what is the refund policy?")` finds the team's notes,
structured notes, paragraphs of documents and tasks whose meaning is closest
to the question, even when they share no word with it, the closest first.
Each `Found` says what it is (`kind` is `"note"`, `"structured"`,
`"document"` or `"task"`), its `name`, the `text` that was found, its
`paragraph` in a document, and a `score` from 0 to 1. A task is found by its
title and notes; its `name` is the task's id and its `title` the title.
Messages are not searched, as an inbox is read only by its owner. Up to five are found, or as many as
`limit` says, so the last of them may have little to do with the question:
the score shows how close each is.

- **The first search fetches the model**, potion-base-8M, 30 MB, from the
  internet, once. The sidecar keeps it in its data folder and checks it
  against what it should be. After that nothing goes over the internet. A
  search waits ten minutes for its answer, or the client's `timeout` if that
  is longer.
- **On a machine with no internet,** fetch the three files on another one,
  put them in a folder, and give the sidecar that folder with
  `orvud --model <folder>`, or with `ORVU_MODEL=<folder>` in the environment
  of the program that starts it. The sidecar does not check a folder it is
  given, so check the files yourself, with `sha256sum`, against the table in
  the sidecar's README, and fetch them from the version it names:
  `https://huggingface.co/minishlab/potion-base-8M/resolve/bf8b056651a2c21b8d2565580b8569da283cab23/<file>`.
- **A team's first search reads all its notes, documents and tasks,** which takes a
  moment on a large team. After that, what is written shows up in what is
  found within a moment.
- **An agent with rules finds only what it may read.** Tasks are found only
  by an agent that may read tasks.
- **The model knows English.** It goes by the words of a text, not by their
  order, so "the dog bit the man" and "the man bit the dog" mean the same to
  it.

### Tools for an agent, with no code

An agent program that speaks MCP, such as Claude Code or Cursor, can use a
team without any Python being written. Give the program this command, and the
agent gets tools for the team's notes, structured notes, tasks, budgets,
messages, documents, tallies and plain shared objects, and is on the team's
roster while it runs:

```text
orvu mcp --me researcher-1
```

[`MCP.md`](MCP.md) has the setup for the common programs and the list of
tools. `--tools notes,tasks` offers only some of them. The agent's inbox,
the tasks and the notes are also offered as resources, which the agent's
program can subscribe to and be told when they change. With
`--http 127.0.0.1:8765` and a token, the server is reached over HTTP
instead, by a program elsewhere; that needs `pip install 'orvu[http]'`. The server speaks both
ways MCP has of connecting: the older one with a hello, and the newer one of
version `2026-07-28` without.

### One team on several machines

Each machine runs a sidecar of its own, and they keep each other in step. A
machine that drops out catches up when it is back. One command makes the
settings and certificates for all of them:

```text
orvud setup crew alpha=10.0.0.5 beta=10.0.0.6 gamma=box3.example.com
```

[`MACHINES.md`](MACHINES.md) goes from one machine to three, step by step.
The sidecar of a team is started by hand on each machine, or kept running by
systemd, and a program reaches it by its socket: `Team.connect(socket=...)`,
or `ORVU_SOCKET` in the program's environment.

### Agents with keys

Out of the box, an agent is called whatever it says it is, and any program
of yours can connect. To have the sidecar check who is connecting, give each
agent a key:

```text
orvud agent add alice --local
```

It prints the key once. Put it in the agent's environment as `ORVU_KEY`, or
pass it as `key=`. The agent then needs no name of its own: it is the agent
the key was given to, and its tasks, notes and messages carry that name.

```python
from orvu import Team

# ORVU_KEY holds the key `orvud agent add alice` printed.
with Team.connect() as team:
    print(team.me)  # alice
```

From the first agent on, a program with no key, or with a key the sidecar
does not know, is refused with `orvu.NotAllowed`. `orvud agent remove alice
--local` turns alice away at its next call, and `orvud agent list --local`
shows who is let in. Removing the last agent lets nobody in, until the file
`agents` in the sidecar's data folder is deleted.

A key keeps agents apart only if they cannot change the sidecar's files: a
program running as your user can add a key of its own. Run an agent you do
not trust where it can reach the socket but not the data folder, such as in
a container given only the socket.

Each agent may do everything unless it is given rules, block by block:

```text
orvud agent allow summariser notes:write tasks:read budgets:spend --local
```

A block is `notes`, `documents`, `tallies`, `tasks`, `budgets`, `inbox`,
`roster`, or `objects` for plain objects, and its level is `read` or
`write`; budgets have `read`, `spend` and `top-up`. Notes and tasks also
take `own`, between the two: with `notes:own` an agent reads every note and
changes or removes only notes it wrote, and with `tasks:own` it reads every
task, adds tasks, claims open ones, and changes, finishes or gives back only
tasks it added or holds. The sidecar's README says exactly what each allows.
A block left out may not be used, and `orvud agent allow summariser all`
takes the rules away. An agent with rules reads only its own inbox, though
it may send to anyone with `inbox:write`, and changes only its own place on
the roster. The names that say who wrote something are checked to be its
own: on a note, as the sender of a message, as the agent that added a task,
and on what it spent from or topped up a budget by. Other names a program
writes, such as who holds or finished a task, are what the program says;
`orvud audit` shows which agent made each change. Every agent can read the
team's list of what it has, as every block needs it, so the names of the
team's documents, structured notes, budgets and tallies, and who has been
sent messages, are seen by all of them, whatever their rules. What is in
them is not. Keep anything secret out of those names. What it may not do
raises `orvu.NotAllowed`, with a message that says why.

The sidecar records every change, with the agent that made it, and what an
agent was not allowed to do. `orvud audit --local --agent alice --since 2h`
shows it. The record keeps what was changed and how, not what was written:
that alice changed the note called deploy, not what the note says.

### From another machine, or a container

An agent that runs somewhere else reaches the sidecar over the network, with
a connection file the sidecar's owner gives it:

```text
orvud agent add alice --address 10.0.0.5:7441 --connection alice.orvu --local
orvud --local --network 0.0.0.0:7441
```

The agent connects with the file, as `connection=` or in `ORVU_CONNECT`, and
needs nothing else: no sidecar of its own, no key apart from the one in the
file. The connection is encrypted, and trusts only that sidecar.

```text
team = Team.connect(connection="alice.orvu")
```

This is also how agents on Windows connect: the sidecar runs on Linux or
macOS, and the agents reach it over the network. On Windows the package
installs with no sidecar inside, and asking it for a sidecar of its own says
to connect over the network instead.

### Read and change shared data

```python
import orvu

with orvu.connect() as client:
    visits = client.counter("visits")
    visits.increment(5)
    assert visits.value() == 5

    notes = client.list("notes")
    notes.insert(0, "buy milk")
    assert notes.to_list() == ["buy milk"]

    doc = client.json("plan")
    doc.set(["title"], "Launch")
    doc.set(["steps"], ["write", "test"])
    assert doc.to_value() == {"title": "Launch", "steps": ["write", "test"]}
```

An object is created by the first write to it. Reading one that does not exist
gives an empty value: `0`, `None`, an empty list and so on.

### A budget that cannot be overspent

Being told no is an ordinary answer, not an error.

```python
import orvu

with orvu.connect() as client:
    budget = client.bounded_counter("tokens")
    budget.increment(10)

    assert budget.try_decrement(4) is True
    assert budget.try_decrement(100) is False  # not enough left, nothing changed
    assert budget.value() == 6
```

### Follow changes as they happen

`watch()` gives the value now, and again each time it changes.

```python
import orvu

with orvu.connect() as client:
    title = client.register("title")
    title.set("Draft")

    for value in title.watch():
        assert value == "Draft"
        break  # a real program would carry on here
```

In `async` code, use `awatch()` with `async for`, and open the client with
`async with`.

### Send several operations in one message

```python
import orvu

with orvu.connect() as client:
    with client.batch() as batch:
        batch.counter("visits").increment(1)
        batch.set("tags").add("urgent")
        total = batch.counter("visits").value()

    assert total.value == 1
```

## Things to know

- **A claim is soft, unless the task is firm.** `team.tasks.claim` keeps
  agents off each other's tasks nearly all the time. Two agents that claim
  the same task at the very same moment, or on two machines that cannot
  reach each other, can both be told yes. Use it where doing a task twice is
  only wasteful. A task added with `firm=True` has one ticket, and only the
  agent that takes it gets the task, whatever the timing and even on
  machines that cannot reach each other. The cost: an agent whose machine
  cannot reach the machine holding the ticket is told no, after waiting up
  to two seconds. Only the agent that has a firm task can finish it.
- **Firm means one holder at a time.** It does not stop work from being
  done twice by an agent that has lost its task without knowing. An agent
  that went quiet, or whose machine is cut off, hears that its task was
  taken over only when the news reaches its machine, and until then
  `still_working` tells it yes. What does stop it is a number. Each holder
  of a firm task holds a numbered ticket, and the number goes up each time
  the task moves on. `claim_ticket` claims the task and returns the number.
  A step that must not happen twice should carry it to whatever it changes,
  and that side should keep the highest number it has seen and refuse a
  lower one:

  ```py
  ticket = team.tasks.claim_ticket(task)
  if ticket is not None:
      invoice = write_invoice()
      # The billing system refuses a ticket lower than the highest it has
      # seen for this task, so an agent that lost the task cannot send it.
      billing.send(invoice, task=task.id, ticket=ticket)
  ```

  That refusal has to be made by the side that is changed: nothing in orvu
  can make it for it. `still_holding(task, ticket)` tells whether the
  number is still this agent's as far as its machine knows, which is a good
  check before starting a step, not a promise.
- **A claim runs out.** It lasts a quarter of an hour after the agent last
  said it was still working, or as many seconds as `claim(task, lease=...)`
  says. An agent that crashed stops saying so, and another agent's `claim`
  then takes the task over. The agent that went quiet finds out when its
  next `still_working` returns `False`. Two agents taking over the same task
  at once can both be told yes, unless it is firm. A takeover is written on
  the task itself, as `task.taken_over`, `{"by": ..., "from": ..., "at":
  ...}`, so every agent that reads or watches the board sees who took the
  task from whom and when. It keeps the last takeover, also after the task is
  given back or finished.
- **A firm task's old tickets are taken off the team's list.** Each time a
  firm task is given back, taken over or set back it moves on to a new pair
  of tickets, and the old pair, which can never be had again, is taken off
  `team.index()`. A finished task keeps one line there, for the ticket that
  sets it back to "todo". The tickets themselves stay in the sidecar, which
  cannot remove anything for good, so each move still leaves two small
  counters behind. Taking lines off is only tidying: a sidecar that refuses
  it leaves them there, and the task moves on all the same.
- **Whatever was written, reading never fails.** A program with the right
  rules can write anything into a task, a note or a message. What is not
  what it should be reads as a safe value, the same in both libraries: a
  title that is not text as "", a state other than the three as "todo",
  names and notes that are not text as `None`, and a time added that is not
  a finite number as 0. A note kept in a form that cannot be read back is
  given as the text it is, and a message that is not one is left out.
- **A firm task's number never goes down.** An agent that has not heard
  the task moved on, on any machine, can still write its lower number late.
  Each number is kept on the task, and the task's number is the highest of
  them, so a late, lower one changes nothing and nobody gets it. A task
  removed and added again whole starts its numbers again, and is claimed
  only once it is set back with force. A number
  counts only if it is one to fifteen plain digits; anything else written
  there is left out, and never stops the board from being read. Each move
  adds one small number to the task, and none is ever taken off, so a task
  that moves on very often grows a little each time. A task whose numbers
  come near the fifteen-digit limit cannot be set back with force any more:
  `TicketUnavailable` says to add it again.
- **A firm task can be stuck,** if the agent claiming it stops between
  taking the ticket and writing its name, or the agent moving it on stops
  between taking the ticket for that and writing the new number. Setting it
  back with `update(task, state="todo")` then raises `TicketUnavailable`
  every time, because a ticket that was taken cannot be told apart from one
  on a machine that cannot be reached. `update(task, state="todo",
  force=True)` sets it back all the same, on a fresh pair of tickets with a
  higher number, so no ticket from before can win it back. The number
  jumps by a random two to four thousand million, past every pair the task
  has had. A second operator forcing after the first, or one trying again,
  never hands one number to two agents. Two operators forcing at the very
  same moment, or on machines cut off from each other, land on the same
  number only by a chance of about one in a thousand million. A ticket
  found made twice anyway is never taken: claiming the task raises
  `TicketUnavailable`, which says to set it back. `update(task, state="todo")` is
  enough for a doubled ticket; a doubled ticket for moving the task on
  needs `force=True`. It needs
  `tasks:write`, as any change to a task does. Use it only when you know the
  agent that was claiming or moving the task on has stopped for good: its
  program ended, or its sidecar failed. Do not use it while a machine that
  may hold the ticket is only cut off. Wait for it to come back.
- **A claim runs out by the clock of the agent that wants the task.** It has
  run out when that program has seen no change at all to the claim, no word
  from the agent on it, for as long as the claim lasts, measured on its own
  steady clock from when it first saw the claim as it is. The time the agent
  on the task writes (`seen`) is only there to be shown, and is never
  compared with another machine's clock, so machines whose clocks are far
  apart still agree on how long a claim lasts. The price: a program that has
  just started has seen nothing yet, and waits a whole lease before it takes
  a task over, however long the agent on it has been quiet.
- **A finished task stays finished.** `team.tasks.finish` returns `False` for
  a task that is already done and leaves its result alone, and a claim that
  arrives late cannot bring it back or leave its name on it. When an agent
  runs as several programs under one name, a late claim from one leaves a
  claim another of them made since as it is. To open it
  again, set its state to "todo" with `update`. The result and the name of
  the agent that finished the task are written as one, so one is never kept
  beside the other's. Of two agents that finish a task at the same moment,
  the one whose result was not kept is told `False`. Both can still be told
  yes on two machines that cannot reach each other, or when one has already
  read back its own before the other's write arrives; the later result is
  then kept.
- **A task being added is not listed until it is whole.** It is put in the
  tree of sub-tasks and then on the board. Another machine can hear of the
  two in either order, so `list` and `watch` leave a task out until it is in
  the tree, and it never shows at the top for a moment before it shows under
  its parent. They wait only half a minute after the task was added, by the
  clock of the machine that added it (`team.tasks.GRACE`). A task that has
  no place in the tree by then, because the program adding it stopped half
  way, is shown at the top. `watch` shows it when that time is over, also
  if nothing else changed.
- **Changing, claiming and finishing a task do not slow down as the board
  grows.** They read that task alone. Adding a task does slow down: among
  5,000 tasks it takes a few thousandths of a second, most of it spent
  placing the new task in the tree of sub-tasks. Listing and watching the
  board read all of it, and take longer the more tasks it has.
- **The sidecar can keep a log and serve a page of numbers.** Both are off.
  To turn them on for the sidecar a program starts by itself, put a file
  `orvud.toml` in its data folder. On Linux that is `$XDG_DATA_HOME/orvu` if
  that variable is set, and `~/.local/share/orvu` if not. On a Mac it is
  `~/Library/Application Support/orvu`:

  ```toml
  [logs]
  level = "info"

  [metrics]
  addr = "127.0.0.1:9464"
  ```

  It is read when the sidecar starts. With no `file` in `[logs]`, the log
  goes where the sidecar's output already goes: `orvud.log` in the same
  folder. The page is at `http://127.0.0.1:9464/metrics`. What is on it is
  in the sidecar's own README.
- **Names for teams, budgets, documents, tallies and structured notes are
  plain:** letters, digits, dots, dashes and underscores, at most 64
  characters. The name of a simple note can be any text.
- **A simple note is replaced whole, and a clash is kept.** It can hold a
  list or a dict. Two agents that put the same note at the same moment,
  neither having seen the other's, leave both values, not a mix: `get`
  raises `orvu.Clash`, whose `values` and `authors` say what each wrote, and
  `all` and `watch` give the note as that `Clash`. The next `put` settles it
  for everyone. Two agents that put the same value read as one. Use
  `structured` where different agents fill in different fields.
- **How a simple note is kept.** The notes are the map
  `orvu/<team>/notes-mv`, which keeps values written at once. Each value is
  the writer's name, the character `\x1f`, and then the note: text as it is,
  or anything else as the character `\x1e` and its JSON form, such as
  `"alice\x1f\x1e{\"day\": \"Friday\"}"`. A text that itself starts with
  `\x1e` is kept in the JSON form too, so that it reads back as it was. A
  program that gave no name writes an empty one. The TypeScript package
  keeps notes the same way, the sidecar's search reads them so, and a
  program in another language that uses the map directly has to as well.
- **Notes kept the old way are moved over once.** Notes used to be kept in a
  plain map. The first time a team's notes are used, the sidecar moves them
  over, with who wrote each, and leaves the old maps as they were.
- **Names on writes are taken on trust from an agent without rules,** like
  the name itself. `author`, `spent_by`, `topped_up_by`, `written_by`,
  `updated_by`, a task's `created_by` and a message's `sender` say what the
  writing program called itself. For an agent with rules, the sidecar checks
  that a note's `author`, its lines in `spent_by` and `topped_up_by`, a
  task's `created_by` and a message's `sender` are its own name, and records
  a task's `updated_by` itself, as the agent that last changed anything in
  the task; `orvud audit` shows which agent really made every change. A
  structured note and a tally's total keep no writer's name: `orvud audit`
  shows who changed them. A program that gave no name leaves no name. The
  name is written a moment after the write it belongs to, so a program that
  is ended in between leaves the write without it.
- **An inbox is not emptied by reading.** `unread` gives each message once.
  What an agent with a name was given is written down for the team, so a
  program that starts again under that name is not given it again. A program
  with no name remembers only while it runs. The first look takes in the
  newest two pages of 100 messages. After that, `unread` gives everything
  that arrives, however much. `messages` shows the recent ones whether they
  were given or not, and `history` reads every page. A program keeps in
  memory only what it needs for the pages it still looks at.
- **An agent can run as several programs under one name.** It is on the
  roster while any of them is, and each message is given to one of them.
- **A message written late to an old page is given once.** A machine that
  was cut off for long goes on writing to the page it knew, after the inbox
  has moved on. Each page keeps a count of the messages written to it, which
  goes up in the same message to the sidecar as the write, in
  `orvu/<team>/inbox-count/<inbox>`. An agent with a name keeps a mark of how
  many it was given from each page, in
  `orvu/<team>/inbox-done/<agent>/<inbox>`. A page whose count is above the
  mark, or whose count no sender could have written (below zero or far too
  big, maybe stuck at the counter's limit), is read again by `unread` until
  the mark catches up. The counts are only hints. Such a page is read at
  once, then, while nothing new turns up there, after 1 second, 2, 4 and so
  on up to a minute (`Inbox.WAIT`, `Inbox.LONGEST_WAIT`), and from the start
  again when its count changes. At most 8 such pages are read on each look
  (`Inbox.BEHIND`), those read longest ago first, so none is passed over for
  good. `watch` follows up to 8 of them, to see at once when a message lands
  there, and wakes by itself when a wait is over. So a late message is
  always given, also when its count arrives before the message itself:
  within about a minute at worst, and at once when `watch` follows its
  page. That holds for a program that keeps looking, through `watch` or
  regular calls to `unread`. With more than 8 such pages waiting, a program
  that calls `unread` only now and then needs several calls to reach them
  all, about one call for each 8 pages. Anyone who may send can add to an old page's count without writing
  there; a count that then stays as it is costs at most one more read of
  that page a minute. A program with no name does not look back at pages it has
  finished; `history` shows such a message.
- **What an agent was given stops growing once a page is finished.** The
  mark for the page is what `unread` looks at from then on. The record of
  each message given from the page is not read or added to again, unless a
  message is written to the page late, but it is kept: it is what tells the
  late message apart from the ones given before.
- **Following an inbox reads only what changed.** `watch` looks at which
  page each inbox is on, how long its newest pages are and the counts, in one
  message, and reads messages only when one of those changed.
- **Messages are put in order by the sender's clock,** so two machines whose
  clocks disagree can show two messages the other way round.
- **"Here" on the roster goes by clocks too.** An agent says "still here"
  every 15 seconds unless `join(every=...)` says otherwise, and counts as
  here for three times that long after it was last heard, by its own
  machine's clock compared with the reader's. An agent that crashed stays on the roster and stops counting
  as here. `leave`, or closing the team, takes it off. What a crashed
  program left is cleared the next time a program joins under the same
  name, and an agent not heard from for more than a day is not listed at
  all. A machine whose clock is a day behind the reader's looks gone too.
- **A document's merge is mechanical.** Two agents that rewrite the same
  sentence at the same moment get both versions run together. The mark that
  says who wrote a part goes in with the text, as one change, so it always
  lands on exactly that text.
- **`Json.insert` only goes into a list, or where nothing is yet.** Where an
  object, a text or a number is, it raises `JsonError`.
- **A number in a JSON document is a 64-bit float.** A float holds every
  whole number up to 2**53 exactly, and only some past it, such as 10**16 or
  2**60. Writing one it cannot hold, such as 2**53 + 1, raises `JsonError`
  and changes nothing. A float, however large, is fine. A whole number below
  10**21 comes back as an `int`, even one written as a float, such as `2.0`.
  A number with a fraction, or from 10**21 on, comes back as a `float`, and
  `-0.0` comes back as `-0.0`.
- **A later write of a whole list or object to a `Json` field wins outright.**
  When two programs set the same field to a list at the same moment, every
  program ends up with one of the two lists, never both run together. Use
  `insert` to add to a list from several programs at once.
- **Replacing or removing something in a `Json` document starts it afresh.**
  A change made at the same time inside what was replaced is dropped, even if
  it came later. What was replaced never comes back. Two programs that write
  into a field that was empty, removed or replaced, starting from the same
  state, both keep their writes.
- **A tally only goes up.**
- **The blocks keep their data in objects called `orvu/<team>/...`.** Leave
  names that start with `orvu/` to them.
- **A budget is cautious across machines.** A machine can only spend what it
  holds or can get from a machine it can reach. Cut off, it is told no even
  if there is budget elsewhere. It is never overspent.

- **The sidecar is started for you and stops by itself.** It runs in the
  background, shared by all your programs, and stops five minutes after the
  last one has disconnected. `ORVU_IDLE_SECONDS`, in the environment of the
  program that starts it, sets another time, in seconds, at most
  9007199254740991. Starting it again
  takes a moment and loses nothing. A program that was quiet for longer than
  that does not notice: its next call starts the sidecar again.
- **Your data is kept on disk,** in `~/.local/share/orvu` on Linux and in
  `~/Library/Application Support/orvu` on a Mac. What the sidecar prints goes
  to `orvud.log` in that folder. When a sidecar is started and the log has
  grown past 1 MiB, it is moved to `orvud.log.1` first, in place of the one
  there, so the two together never grow much past 2 MiB. Only the program
  holding the lock on it moves it, whatever language it is in, so that two
  starting at once do not lose the long log. The lock is the highest-numbered
  of the files `orvud.log.lock.1`, `.2`, and so on: a program takes it by
  making the next number, which only one program can do, and gives it back
  by making the one after that, saying `free`. A lock older than 30 seconds
  was left behind and is taken over the same way.
  A program stopped for more than 30 seconds while holding the lock, paused
  or put to sleep, may lose it that way. It looks again just before moving
  the log, and leaves it if the lock is no longer its own. That makes the
  gap small but does not close it: a program stopped for more than 30
  seconds between looking and moving can still move the log while the lock
  is no longer its own.
  No program ever removes
  or changes the highest-numbered file, so none can take away a lock
  another has just taken; the ones below it are cleared away by the next
  program to take the lock. A plain `orvud.log.lock` left by an earlier
  release is not looked at, and can be deleted. The socket it was
  started on is noted in `orvud.where` there, so that a program started with
  other settings than your terminal has, such as a server an agent's program
  starts, finds the same sidecar. A sidecar started by hand with
  `orvud --local` writes the same note while it runs, and takes it away when
  it stops.
- **The package and the sidecar have to be the same release.** If a sidecar of
  another release is already running, `connect` raises `VersionMismatch` and
  names the process to stop. This can happen right after upgrading the
  package. The old sidecar does not go away by itself while programs keep
  trying to connect to it.
- **`connect()` only uses a local sidecar,** one that keeps its data on this
  machine. If a sidecar connected to other machines is serving on the same
  socket, `connect()` raises `SidecarUnavailable` and does not use it.
- **To use a sidecar you run yourself,** give `connect` its socket path, or
  set `ORVU_SOCKET` to it in the program's environment. That is how a program
  reaches a sidecar that is connected to other machines: `orvud setup` makes
  the settings for one on each machine, and it is started by hand, or by
  systemd, with `orvud --config` (see [`MACHINES.md`](MACHINES.md)). The
  package never starts such a sidecar: if nothing answers on the socket,
  `connect` raises `SidecarUnavailable`, and says so. `ORVU_SOCKET` and
  `ORVU_CONNECT` cannot both be set.
- **A data folder copied from another computer is not started.** The sidecar
  this package starts refuses a data folder that was last used on another
  computer, as two computers must never run as one machine. `connect` then
  raises `SidecarUnavailable` with the sidecar's own message, which gives
  the command to run once by hand, with the folder in it:
  `orvud --local --data-dir <folder> --moved` if the folder was moved here,
  or `--new-number` if the other computer still uses its copy.
- **The sidecar runs on Linux and macOS.** There is no sidecar for Windows
  yet. An agent on Windows installs the package with no sidecar inside and
  connects to one elsewhere over the network. It has been tried only on
  Linux, with Python made to look like Windows. Windows on ARM is not supported: gRPC is not ready-built for it.
- **Calls wait for the answer.** Every method is an ordinary blocking call. In
  `async` code, run them with `asyncio.to_thread`. Only `awatch()` is `async`.
- **Refusals return `False`.** `try_decrement` and `transfer` on a
  `BoundedCounter` return `False` when there is not enough, and nothing
  changes. They do not raise.
- **Real failures raise a named error.** Using a name as the wrong type raises
  `TypeMismatch`, a position past the end of a list raises `OutOfBounds`, and
  so on. Every one of them is an `OrvuError`.
- **A change whose answer did not come is sent again, and made once.** Each
  change, and each batch with a change in it, is sent with an id of its own,
  and with the same id each time it is sent. If no answer comes, because the
  sidecar took longer than the `timeout` given to `connect` or stopped
  partway, the client sends it again, up to three more times, once the
  sidecar is back. The sidecar gives a change it already made the answer it
  gave it, and does not make it again. A client from `connect()` with no
  socket starts the sidecar again; one given a socket waits up to `timeout`
  for it to come back. Only if all that fails does the change raise
  `TimedOut` or `SidecarUnavailable`, and then it may have been made. The
  sidecar remembers ids for ten minutes; see its README.
- **Running out of time is not a "no".** A read that the sidecar does not
  answer within the `timeout` raises `TimedOut`, and is not sent again.
- **A mistake in an argument raises `BadRequest`** before anything is sent,
  such as a negative count or a value JSON cannot hold.
- **A batch is not all or nothing.** Each operation in it has its own result,
  and one failing does not undo the others.
- **A batch is sent once.** It goes out when the `with` block ends. Calling
  `send()` a second time does nothing.
- **Size limits.** The sidecar does not take one request over 64 MiB, or
  over 4 MiB from a program that reaches it over the network. A
  reply may be up to 64 MiB, which `connect(..., max_reply_bytes=...)` changes.
  Past either limit the call raises `TooLarge`. In a batch, operations are no
  longer run once the replies have passed 4 MiB, and each of those raises
  `TooLarge` when its `value` is read.
- **Live updates have limits.** One client can have 256 `watch()` loops open
  at once, and a sidecar 4,096 over all the programs it serves, near and over
  the network. Starting one more raises `TooLarge`. A loop that has ended
  gives its place back.
- **The client survives a sidecar restart.** It opens a new session by itself
  and carries on. A `watch()` loop that was running when the sidecar went away
  ends with `SidecarUnavailable` and has to be started again.
- **A call the sidecar turned away because it was stopping is made again.**
  It did not run. A client from `connect()` with no socket waits for that
  sidecar to go, starts a new one, and makes the call there. Should that one
  be stopping too by the time the call gets there, the same is done again,
  for as long as the client's `timeout`. One given a socket raises
  `SidecarUnavailable`, as for a sidecar that is not there.
- **A `watch()` loop carries on when its session is dropped.** A busy
  sidecar keeps 1,024 sessions, and when they are all taken it drops the one
  used longest ago. An agent with a key that opens more than 256 drops its
  own session used longest ago instead. A call counts as using a session, and
  so does each value a watch sends, so a watch whose object keeps changing
  keeps its place and a quiet one can be dropped. The loop then opens a new
  session and watches again, as often as that happens. It gives up, with
  `EngineError`, if its session is dropped twice within 10 seconds: the new
  session it opened is dropped too before 10 seconds have passed. That
  happens when many other sessions on the sidecar are busier than the
  loop's and new ones keep opening: a quiet loop's new session can become the one used
  longest ago within a second. Trying again would then go on for ever. The
  answer is fewer sessions, since the limit of 1,024 is fixed.
- **Give each program its own agent and key.** One agent may hold at most
  256 sessions on the sidecar at once, and each client holds one. When more
  than 256 programs use the same key at the same time, each one that opens a
  session drops another's, that one opens a new session and drops the next,
  and so they keep dropping each other's sessions. So give each program, or
  each small group of them, an agent and key of its own.
- **Positions in text count characters,** the same way Python strings do.
- **The machine's number is in `client.replica_id`.** `BoundedCounter.transfer`
  takes the number of the machine to hand allowance to.

## Working on this package

```text
cargo build -p orvud
cargo build -p orvu-transport --example keygen
cd python
python -m venv .venv
.venv/bin/pip install -e ".[dev,release]"
.venv/bin/pytest
```

The tests start real sidecars. The only stand-ins are for a sidecar in its
last moment, which a real one is in for too short a time to test. Installed this way the
package has no sidecar inside it, so the tests use the one Cargo built. Set
`ORVUD_BIN` to use a different one.

Each sidecar a test starts gets a folder of its own in `/tmp`, or in the
folder `ORVU_TEST_DIR` names. A socket's path can hold at most 107 bytes, so
that folder's path should be short. The tests that link machines also listen
on network ports of this computer, on `127.0.0.1`, and cannot run where that
is not allowed.

The files in `src/orvu/_proto` are made from `crates/orvud/proto/orvu/v1/dataplane.proto`.
After changing that file, run `sh scripts/generate.sh` here. A test fails if
they fall out of step.

### Building the package people install

```text
sh scripts/build-wheel.sh
sh scripts/clean-test.sh dist/orvu-*.whl
```

The first builds the sidecar for this kind of machine and puts it inside one
`.whl` file in `dist`. The second installs that file into an empty
environment with an empty home folder and runs `examples/quickstart.py`.

The package with no sidecar inside, for Windows, is built and tested the
same way:

```text
sh scripts/build-wheel.sh --without-sidecar
sh scripts/clean-test-network.sh dist/orvu-*-py3-none-win*.whl
```

It needs no Rust to build. It is made out to Windows only, so that pip on
any other machine never takes it in place of a package with the sidecar
inside. The test installs it here under a name pip takes on any machine, and
connects it to the sidecar built from this repository, over the network,
with nothing but a connection file.

The version number is in two places that have to agree:
`src/orvu/_version.py` and `crates/orvud/Cargo.toml`. A test fails if they do
not.
