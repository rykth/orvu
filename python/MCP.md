# Orvu as tools for an agent (MCP)

An agent that speaks MCP can use a team's shared notes, task board, budgets,
messages, documents, tallies, roster and plain shared objects with no code
written for it. You give the agent's program one command, and the agent gets
a set of tools.

MCP, the Model Context Protocol, is the usual way agent programs such as
Claude Code, Claude Desktop, Cursor and VS Code are given tools.

## 1. Install

```text
pip install orvu
```

This installs the `orvu` command. There is nothing to start and nothing to
configure: the sidecar that holds the data is started when the first tool is
used.

## 2. Tell your agent program about it

The command the program has to run is:

```text
orvu mcp --me researcher-1
```

`--me` is the name this agent has on the team. Give each agent a name of its
own: it is how the others see who has taken a task.

Where that command goes depends on the program.

**Claude Code.** Run this once, in the project's folder:

```text
claude mcp add orvu -- orvu mcp --me researcher-1
```

**Claude Desktop, Cursor, and most others.** They keep a settings file with a
list of servers. Add this one to it. In Claude Desktop the file is opened from
Settings, Developer, Edit Config. In Cursor it is `.cursor/mcp.json` in the
project.

```json
{
  "mcpServers": {
    "orvu": {
      "command": "orvu",
      "args": ["mcp", "--me", "researcher-1"]
    }
  }
}
```

**VS Code.** The file is `.vscode/mcp.json`, and the list has another name:

```json
{
  "servers": {
    "orvu": {
      "type": "stdio",
      "command": "orvu",
      "args": ["mcp", "--me", "researcher-1"]
    }
  }
}
```

**Any other program.** Look for "MCP servers" in its settings and give it the
command and its arguments. The server talks over its input and output, which
settings pages call "stdio", or over HTTP when started with `--http`.

If the program says it cannot find `orvu`, give it the whole path. A program
started from the desktop does not always look where your terminal does.
`which orvu` prints the path.

## 3. Give a second agent the same team

Set up another agent the same way with another name, `--me researcher-2`.
Both now see the same notes, tasks and budgets. Ask one to add a task and
the other to list them, or one to send the other a message.

Agents started this way have to run as the same user on the same machine.
Sections 4 and 6 are for an agent that runs elsewhere.

## 4. An agent on another machine, or in a container

One way is to serve it over HTTP from this machine, as section 6 shows.
Another is to give the agent a connection file, made with `orvud agent add <name> --address
<host:port> --connection <file>` on the sidecar's machine, and start the
sidecar there with `--network <address>`. Put the file's path in the agent's
environment as `ORVU_CONNECT`, and leave out `--me` and `--socket`:

```text
claude mcp add orvu --env ORVU_CONNECT=/path/to/alice.orvu -- orvu mcp
```

## 5. Check who each agent is

So far an agent is called whatever `--me` says. To have the sidecar check it,
give each agent a key:

```text
orvud agent add researcher-1 --local
```

It prints the key once. Put it in the agent's environment as `ORVU_KEY` and
leave `--me` out: the agent is then the one the key was given to. For Claude
Code:

```text
claude mcp add orvu --env ORVU_KEY=orvu-key-... -- orvu mcp
```

In a settings file, it goes in an `"env"` list next to `"args"`. From the
first agent on, a server without a key, or with one the sidecar does not
know, is refused, and the agent's tools say so.

## 6. Serve an agent over HTTP

An agent's program that cannot start the server itself, because it runs on
another machine, in a container, or as another user, can reach it over
HTTP. Install the extra it needs, make a token only you can read, and start
the server yourself:

```text
pip install 'orvu[http]'
head -c 32 /dev/urandom | base64 > ~/.orvu-token
chmod 600 ~/.orvu-token
orvu mcp --me researcher-1 --http 127.0.0.1:8765 --token-file ~/.orvu-token
```

The program connects to `http://127.0.0.1:8765/mcp`, MCP's streamable HTTP,
and gives the token in each request as `Authorization: Bearer <token>`. A
request without it, or with another, is refused before anything else is
looked at.

**The server is one agent.** It acts as the agent it was started as, here
`researcher-1`, for every program that connects with the token: they share
its name, its tasks, its inbox and its place on the roster. Giving someone
the token gives them that agent. For two agents, start two servers, each
with its own name, port and token.

- **It listens on this machine only, unless told otherwise.** The address is
  `127.0.0.1` in the example. Any other address, such as `0.0.0.0`, lets
  other machines in, and the server will not start on one without
  `--token-file`.
- **On this machine a token is wanted too,** as other users of the machine
  can reach the port. `--no-token` serves without one, on this machine
  only. A server without a token answers only requests that name it as
  `127.0.0.1`, `localhost` or `[::1]`, so a web page cannot reach it through
  a name of its own.
- **The token file has to be yours alone:** a plain file, not a link, owned
  by you, that nobody else can read or change (`chmod 600`), holding at
  least 16 characters with no spaces. The server will not start otherwise.
- **The connection is not encrypted.** Over a network you do not trust, put
  the server behind something that adds TLS, or reach it through an SSH
  tunnel.
- **Up to 100 programs can have a session open at once.** A session nobody
  uses for half an hour is ended. When all 100 are open and another program
  says hello, the session used longest ago that is doing nothing, such as
  one a program left behind when it crashed, is ended to make room. One that
  is answering, or has its stream for word of changes open, is never ended
  for this. So a program with the token that keeps a stream open on
  every session can fill the server, and every new hello is then refused
  with 503 until one of them ends.
- The server stops with Ctrl-C or SIGTERM, and takes the agent off the
  roster.

## The tools

| Tool | What the agent can do with it |
|---|---|
| `whoami` | See which team this is and what its own name is |
| `note_put` | Save a short fact for the team under a name |
| `note_get` | Read one note |
| `note_author` | See which agent wrote a note |
| `note_list` | Read the notes, a part at a time on a large team, or only their names |
| `note_remove` | Remove a note |
| `note_structured_get` | Read a structured note, a JSON document agents fill in field by field, or one field of it |
| `note_structured_set` | Set one field of a structured note to any JSON value |
| `note_structured_remove` | Take one field out of a structured note |
| `note_structured_list` | List the structured notes |
| `task_add` | Add a task to the board, or a sub-task to a task |
| `task_list` | List the tasks, all or in one state, a part at a time on a large team, or only their ids, titles and states |
| `task_get` | See one task and its sub-tasks |
| `task_claim` | Take a task, so that the others leave it alone, or take over one whose agent went quiet |
| `task_still_working` | Say you are still on a task, so that nobody takes it over |
| `task_release` | Give a task back |
| `task_finish` | Mark a task done and save its result, as text or as JSON |
| `task_update` | Change a task's title, notes or state |
| `task_move` | Put a task under another, or make it a top-level task again |
| `budget_list` | See the budgets and what is left in each |
| `budget_left` | See what is left in one budget |
| `budget_spend` | Take an amount out of a budget, or be told there is not enough |
| `budget_spent` | See how much each agent has taken out of a budget |
| `budget_top_up` | Add to a budget. Only with `--allow-top-up` |
| `inbox_send` | Send a message to one agent, or to everyone, as text or as JSON |
| `inbox_read` | Read the messages it has not been given yet |
| `inbox_recent` | Read the recent messages again |
| `doc_list` | List the documents the team is writing |
| `doc_read` | Read a document, and who wrote which part |
| `doc_append` | Add text to the end of a document |
| `doc_insert` | Put text into a document, after a piece of its text or at a position |
| `doc_remove` | Take text out of a document |
| `search` | Find the notes, paragraphs of documents and tasks closest in meaning to a question. Messages are not searched |
| `tally_list` | List the things the team keeps count of |
| `tally_read` | See a count, in total and under each name |
| `tally_add` | Count something |
| `roster_list` | See which agents are on the team, and which are here now |
| `object_read` | Read a plain shared object: a counter, register, set, map, cart, list or JSON document |
| `object_counter_add` | Add to a counter, or take away from it |
| `object_register_set` | Set a register |
| `object_set_add` | Add an item to a set |
| `object_set_remove` | Take an item out of a set |
| `object_map_put` | Set a key of a map |
| `object_map_remove` | Take a key out of a map |
| `object_list_insert` | Put an item into a list, at the end or at a position |
| `object_list_remove` | Take the item at a position out of a list |
| `object_json_set` | Set a place in a JSON document |
| `object_json_remove` | Take a place out of a JSON document |

## Resources and prompts

Besides the tools, the server offers three resources, which an agent's
program can read, and subscribe to so that it is told when they change:

| Resource | What it holds |
|---|---|
| `orvu://inbox` | The recent messages to the agent and to everyone, as `inbox_recent` gives them. Reading it does not count as being given them |
| `orvu://tasks` | Every task on the team's board, as `task_list` gives them |
| `orvu://notes` | Every one of the team's notes, as `note_list` gives them |

Each is JSON. A program that subscribes to one is sent
`notifications/resources/updated` each time what it holds changes. It is
told that something changed, not what: it reads the resource again to see.
Whether the agent's model hears of it depends on the program.

There are two prompts, which a program may offer its user as commands:
`check_inbox` asks the agent to read its messages and act on them, and
`pick_a_task` asks it to take a free task and start on it.

A resource and a prompt are offered only with the tools they go with, so
`--tools notes` leaves only `orvu://notes` and no prompt.

## Options

| Option | What it does |
|---|---|
| `--me NAME` | The agent's name on the team. Without it the agent gets a made-up name, made from the team and the folder the server is started in. With a key it can be left out, and if given must be the key's name. It cannot be "everyone" |
| `--team NAME` | The team to use. Without it, the team called `default` |
| `--allow-top-up` | Also gives the agent `budget_top_up` |
| `--tools GROUPS` | Offers only some of the tools, by group, such as `--tools notes,tasks`. The groups are `notes`, `tasks`, `budgets`, `inbox`, `documents`, `search`, `tallies`, `roster` and `objects`. `whoami` is always there. Without it, every tool |
| `--http HOST:PORT` | Serves over HTTP at that address instead of over input and output. Needs `pip install 'orvu[http]'`. See "Serve an agent over HTTP" |
| `--token-file PATH` | With `--http`, a file only you can read that holds the token programs have to give |
| `--no-token` | With `--http` on this machine only, lets in programs that give no token |
| `--socket PATH` | Uses the sidecar at that socket. Not needed for one you started with `orvud --local`, which notes its socket in the data folder where the server looks |

The server also reads `ORVU_KEY`, the agent's key, from its environment.

## Things to know

- **The first `search` fetches the model it uses from the internet,** 30 MB,
  once, into the sidecar's data folder. On a machine with no internet, see
  "Search by meaning" in [`README.md`](README.md).
- **An agent can spend from a budget but cannot add to it,** unless you start
  its server with `--allow-top-up`. An agent that has run out would otherwise
  be able to give itself more. Put the money in from a program of your own,
  with `team.budget("tokens").top_up(10_000)`, or give one trusted agent the
  option.
- **A budget has to be topped up before it exists.** Until then the agent
  is told there is no such budget, so that a name typed wrongly is not taken
  for a team that has run out.
- **A claim is soft, unless the task is firm.** `task_claim` keeps agents
  off each other's tasks nearly all the time, but two agents that claim the
  same task at the very same moment can both be told yes. Use it where doing
  a task twice is only wasteful. A task added with `firm` set to true is
  given to one agent at a time, and only that agent can finish it. An agent
  whose machine cannot reach the one that holds the task's ticket is told
  no.
- **A claim runs out after a quarter of an hour** without a call to
  `task_still_working`. Another agent's `task_claim` then takes the task
  over, and the agent that went quiet is told the task is no longer its own
  the next time it calls `task_still_working`, once the news has reached
  its machine. Every agent sees the takeover on the task, as `taken_over`:
  who took it, from whom, and when. Until then it is told yes, so even a firm task can be worked
  on twice by an agent that went quiet or was cut off. `task_claim` reminds
  the agent each time to call `task_still_working` well before its claim
  runs out. The quarter of an hour is counted by the server of the agent
  that wants the task, from when it first saw the claim as it is, not from
  the time written on it, so clocks that disagree do not matter. A server
  that has just started waits a whole quarter of an hour before it lets its
  agent take a task over.
- **Without keys, the name is taken on trust.** Nothing stops two agents
  being started with the same `--me`. They would then count as one, and each
  could take over the other's tasks. With keys, an agent is the one its key
  was given to, and cannot take another's name.
- **Keys keep agents apart only if they cannot change the sidecar's files.**
  An agent running as the user the sidecar runs as can write a key of its
  own into the agents file. To keep an agent from doing that, run it where
  it can reach the socket but not the data folder, for example in a
  container that is given only the socket.
- **What each agent changed is recorded** by the sidecar, with its name, and
  `orvud audit --local --agent researcher-1` shows it. The record says what
  was changed, not what was written.
- **An agent's rules decide what its tools may do.** Give it rules with
  `orvud agent allow`, such as `notes:write tasks:write inbox:write`. A tool
  that does something the rules do not allow answers with an error that says
  so, and the agent can tell the person. Without rules, an agent with a key
  may do everything.
- **Give every agent a name.** A made-up name is made from the team and the
  folder the server was started in, such as `agent-3f9a1c2e`, so an agent
  started again in the same folder gets the same name back and keeps its
  tasks. But two servers started in the same folder for the same team get
  the same name too, and count as one agent: each can take over the other's
  tasks, and each message to the name is given to one of them.
- **The tools and the Python library see the same data.** A program that uses
  `Team.connect()` and an agent that uses these tools, on the same team, work
  on the same notes, tasks and budgets.
- **A tool that is used wrongly answers with what was wrong.** The agent
  reads that and tries again. Nothing is changed by a call that was refused.
- **An agent is on the roster while its server runs,** with a made-up name
  as with a given one. It joins the first time it uses a tool and is taken
  off when the server ends: when its input is closed, or when it is sent
  SIGTERM or Ctrl-C. A server that is killed, or whose machine stops, cannot
  say goodbye. Its agent stops counting as here after 45 seconds, and is
  listed as not here for up to a day, so that the others can see that an
  agent went away, perhaps in the middle of a task. What it left is cleared
  the next time a server starts under the same name.
- **An agent that cannot be put on the roster is told so.** Its tools work
  all the same. `whoami` and `roster_list` say it is not on the roster and
  why, and joining is tried again each time it uses a tool.
- **An agent's program can be told when a message arrives,** by
  subscribing to `orvu://inbox`, and so for the tasks and the notes. Many
  programs do not subscribe, or do not pass the news on to the model. The
  agent then reads its inbox with `inbox_read`, as the server says in its
  opening words: when it starts, and now and then while it works.
- **Following changes costs little and cannot pile up.** Each resource
  subscribed to is followed on a connection of its own, which is let go of
  when the program unsubscribes or goes away. A program that is slow to
  read is owed at most one word for each resource, however often it
  changed.
- **A message is given to an agent once.** What an agent with a name was
  given is written down for the team, so it is not given again after its
  server starts again. `inbox_recent` shows the recent messages whether they
  were given or not. An agent with a made-up name that is started in another
  folder has another name, and so is given the messages to everyone again.
  A message that a machine cut off for long wrote late to an old page is
  given too, once, also when its count arrives before the message itself:
  within about a minute at worst for an agent that keeps checking its
  inbox. With more than 8 such old pages waiting, an agent that checks only
  now and then needs several checks to reach them all, about one for each 8
  pages.
- **A message to a name nobody has is kept,** in case an agent of that name
  starts later, and the sender is told that nobody by that name is on the
  roster. To send to all, the agent leaves the name out or writes
  "everyone", in any case. So no agent can be called "everyone": the server
  will not start with `--me everyone`, and an agent whose key was given that
  name has every tool refused, with a message that says why.
- **Two programs can run under one name.** The agent is here while either
  runs. They share an inbox, and each message is given to one of them.
- **A note two agents wrote at the same time shows both.** `note_get` and
  `note_list` give every text written and who wrote each, with a line telling
  the agent to decide and write the note again, which settles it for
  everyone.
- **Text is put into a document after a piece of its text, or at a
  position.** The piece has to be in the document once, which is how a
  model best says where. A note or a message that a program wrote as a
  number or a list is shown as that.
- **A message and a task's result can be JSON.** `inbox_send` and
  `task_finish` take `data`, any JSON value, in place of text. The message
  is read back with what was sent in its `text`, as for a message a program
  sent. Every answer is also given as MCP's structured content, the same
  JSON as the text, for programs that read that.
- **The plain shared objects are the ones programs name themselves,** such
  as a counter called `visits`. Names that start with `orvu/` belong to the
  team's notes, tasks and the rest and are refused by the `object_` tools.
  An agent with rules needs `objects:read` to read them and
  `objects:write` to change them, and `notes:write` to change a structured
  note. A tool the rules do not allow answers with an error that says so.
- **A long list comes a part at a time.** `note_list` and `task_list` give
  up to 100 at once, or what fits in about 50,000 characters, and then say
  how many more there are and what to give as `after` to read on.
  `names_only` gives a shorter list.
- **A data folder you moved has to be moved for the server too.** Agent
  programs start a server with few of the settings your terminal has. The
  server still finds the sidecar your other programs use, because both look
  in the same data folder. If you set `XDG_DATA_HOME` to move that folder,
  give the server the same setting in the agent program's settings, or the
  two will keep separate data.
- **Both ways MCP has of connecting are spoken.** The older one starts with
  a hello, in versions `2024-11-05` to `2025-11-25`. The newer one,
  `2026-07-28`, has no hello: each request says its version. The newer way
  was built from the protocol's description and checked against the MCP
  project's own client library, not against a real agent program. Resources
  can be subscribed to only the older way: the newer one follows changes
  with a request of its own that this server does not offer, and its
  programs read the resources again instead.
- **Linux and Mac.** There is no Windows version yet.

## Where next

- [`QUICKSTART.md`](QUICKSTART.md) does the same from Python code.
- [`README.md`](README.md) lists everything the package can do.
