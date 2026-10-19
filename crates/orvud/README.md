# orvud

The sidecar: a small background program that lets a program in any language
use Orvu.

Orvu's engine is written in Rust. A Python or TypeScript program cannot call it
directly, so `orvud` runs the engine and answers requests over a socket on the
same machine. Each machine runs one `orvud`. The programs on that machine talk
to it, and the `orvud` processes on different machines keep each other in step.

## What is inside

| Piece | What it is |
|---|---|
| The `orvud` program | Starts the engine, with no setup or from a settings file, and serves the socket |
| `proto/orvu/v1/dataplane.proto` | The definition of every request and reply. Client code for any language is generated from this file. It lives in this module's folder, so a copy of the module on its own has it. This is the one original: the Python and TypeScript clients work from copies that tests keep the same |
| `server::serve_uds` | Starts serving on a Unix socket. The program calls this |
| `server::Dataplane` | The service itself: `Open`, `Execute`, `Batch` and `Watch` |
| `boot::from_toml` | Starts the engine from a settings file. `boot::claim` takes its storage folder first, as the program does |
| `boot::leave` | Takes a machine out of its team for good, handing its share of every budget to the others. `orvud leave` calls this |
| `local::start` | Starts the engine with no setup, from a data folder. `local::claim` and `Claimed::start` do the same in two steps, and say what to do with a folder copied from another computer |
| `setup::write` | Makes the settings file, certificates and service file for each machine of a team, and keeps the key that signed them. `orvud setup` calls this |
| `setup::add` | Adds one machine to a team made earlier, with a certificate signed by the kept key. `orvud setup --add` calls this |
| `agents` | The agents the sidecar lets in, and their keys. `orvud agent` calls this |
| `rules` | What an agent with rules may do |
| `requests` | The answers given to calls that carried a request id, so that a call sent again is not made twice |
| `audit` | The record of who changed what. `orvud audit` reads it |
| `search` | Search by meaning over each team's notes, documents and tasks, kept up to date as they change |
| `notes` | Where a team's notes are kept, and moving notes kept the old way over to it |
| `network` | Serving over the network: the certificate, the connection file, and the encrypted connections |
| `cli::parse` | Reads the command line |
| `say` | Writes to standard error without ever waiting on it, for the program and its log |
| `pb` | The Rust types generated from `dataplane.proto` |

The service has four calls:

| Call | What it does |
|---|---|
| `Open` | Starts a session and returns its id. Every other call needs that id |
| `Execute` | Runs one operation, such as "add 5 to this counter" or "read this list" |
| `Batch` | Runs several operations in one call, in order |
| `Watch` | Sends the value of one object now, and again each time it changes |

## How to use it

### Run it on one machine

```text
orvud --local
```

That is all. It needs no settings file, no certificates and no network. It
keeps its data in a folder of its own and prints the socket it serves on. The
line goes to the error output:

```text
orvud 0.1.0: serving on /run/user/1000/orvud.sock as replica 4090993329555770 in namespace "local"
```

While it runs, it notes where its socket is in the file `orvud.where` in the
data folder, and takes the note away when it stops. The socket's place can
depend on settings your terminal has, such as `$XDG_RUNTIME_DIR`, that a
program started some other way may not have, such as an agent's MCP server.
The data folder is found the same way by both, so the Python and TypeScript
packages read the note to find the sidecar. Only the sidecar that holds the
data folder writes the note, and it takes away only a note that still names
its own socket.

| Flag | What it does |
|---|---|
| `--data-dir <folder>` | Where to keep the data. The default is `$XDG_DATA_HOME/orvu` if that variable is set, and `~/.local/share/orvu` if it is not. On macOS it is `~/Library/Application Support/orvu` |
| `--new-number` | The data folder was copied from another computer that still uses it: keep the data and take a new machine number. See below |
| `--moved` | The data folder was moved from another computer that no longer uses it: keep the data and the number. See below |
| `--restored` | The data folder was restored from a backup, or rolled back in any way: keep the data and take a new machine number. Use it once. See below |
| `--socket <path>` | Where to put the socket. The default is `$XDG_RUNTIME_DIR/orvud.sock`, or `orvud.sock` inside the data folder when that variable is not set |
| `--network <address>` | Also serve agents elsewhere over the network, at this address, such as `0.0.0.0:7441`. See below |
| `--model <folder>` | The folder that holds the model search by meaning uses. See below |
| `--exit-when-idle <seconds>` | Stop once no client has been connected for that long |
| `--version` | Print the version and stop |

### Run it connected to other machines

Each machine needs a settings file and a certificate. One command makes them
for a whole team, a folder per machine:

```text
orvud setup crew alpha=10.0.0.5 beta=10.0.0.6 gamma=box3.example.com
```

A machine is given as a name of your choosing and the address the others
reach it by: an IP address or a name, with `:port` if it is not 7440. Each
folder is copied to its machine, and the sidecar is started there from inside
it:

```text
orvud --config orvu.toml --socket orvud.sock
```

The Python and TypeScript packages never start a sidecar like this one: a
program on that machine is given its socket, in the call or as `ORVU_SOCKET`
in its environment.

Beside the machines' folders, `crew/authority` keeps the key that signed
their certificates, where only you can read it, with the list of the
machines. It never goes to a machine. Anyone who has it can make a machine
the others trust, so keep it where it is.

`orvud setup` looks each machine's name up, and warns about a name this
computer cannot find. It makes the settings all the same: the machines may
know names this one does not.

[`python/MACHINES.md`](../../python/MACHINES.md) goes through this step by
step, and says what happens when a machine drops out.

To run from a settings file you wrote yourself:

```text
orvud --config orvu.toml --socket /run/user/1000/orvud.sock
```

`--socket` can be left out when `$XDG_RUNTIME_DIR` is set. `--exit-when-idle`
works here too.

A client that connects at the very moment `--exit-when-idle` decides nobody
is there is not cut off. The socket file is moved aside first, so no more
clients can find it, and every connection already waiting is taken in. If
there was one, the sidecar keeps serving and the socket file is put back.
A client that tries in between finds no socket, as it would a moment later.

The settings file is the one `orvu-sdk` reads. It names this machine, the
machines to connect to, and the certificate files:

```toml
replica = 1
namespace = "team-a"
listen = "10.0.0.5:7000"
peers = ["10.0.0.6:7000"]

[security]
cert = "certs/node1.pem"
key = "certs/node1-key.pem"
ca = "certs/ca.pem"

[storage]
path = "data/orvu.redb"
```

#### Keep it running

Each machine's folder also holds a systemd user unit, `orvud-<name>.service`,
which starts the sidecar from the settings beside it and starts it again if
it stops with a failure. It expects the folder at `~/orvu/<name>` and the
program at `~/.local/bin/orvud`; change the two paths in it if they are
elsewhere. Then, on that machine:

```text
systemctl --user link ~/orvu/alpha/orvud-alpha.service
systemctl --user enable --now orvud-alpha.service
loginctl enable-linger
```

The last line keeps it running while nobody is logged in. `systemctl --user
status orvud-alpha` shows how it is doing, and `journalctl --user -u
orvud-alpha` what it said. On a Mac, launchd can do the same with a
LaunchAgent that runs the same command with `KeepAlive`; no file is made for
it, and it has not been tried.

#### Add a machine

```text
orvud setup crew --add delta=10.0.0.8
```

This makes `crew/delta`, with a certificate signed by the kept key, and a
number above every number the team has ever given out, so it never takes
the number of a machine in the team or of one taken out of it: the others
keep a machine's data under its number. A number given with `#` that was a
machine's taken out is refused. Two `--add` runs at once take turns, by a
lock on `crew/authority/lock`, so each gets a number of its own. The
machines that are running let the new one in and do
not have to be stopped: it reaches them. So that they also reach it when they
next start, their settings in `crew` gain its address, and the command
prints the new `peers` line for each, to put in the copy on each machine.
Settings made before `orvud setup` kept the key cannot have a machine added
this way. If the peers of a machine's settings in `crew` were written over
several lines, the whole list is put in place of the old one.

#### Shut a machine out at once

```text
orvud setup crew2 --from crew alpha=10.0.0.5#1 gamma=box3.example.com#3
```

This makes the team again in `crew2`, with a new key, for the machines
that stay, each given its number. A machine left out, beta here, cannot
join with its old certificate. `--from` reads the list in `crew/authority`
and carries every number given out there over to `crew2/authority`, the
left-out machines' marked as taken out, so that no number is ever given out
twice: a machine given no number gets one nobody has had, and one given
the number of a machine taken out is refused, here and by a later `--add`.
`crew` is marked as made again in `crew2` (in `crew/authority/replaced`):
first as being made, and once `crew2` is finished, as made. From then on
`--add` to `crew`, or `--from crew` again, is refused and names `crew2`:
only the newest list gives out numbers. This holds even if `crew2` is
moved or renamed later, so look for it wherever it went. The mark keeps
the folder's name exactly as it is, spaces and all, so a folder whose name
has a line break in it is refused before anything is written. `crew2` is
kept on the disk before the mark says it is finished, but if the mark
names a folder that is not there, the message says it may have been moved
or lost, and that the mark can be removed by hand if you are sure it is
lost and no machine was given a number from it. While `crew2` is
being made, `crew/authority/lock` is held, so a machine being added to
`crew` at that moment waits and is then refused. If making `crew2` is
stopped halfway, by Ctrl-C or a crash, the mark in `crew` still says it
was being made. If nothing is at the place it names, or only an empty
folder (one you may have made before the run), the next `--add` to `crew`,
or `--from crew`, says so, removes the mark and goes on as if it had never
been made. If the mark cannot be read for sure, it is kept and `crew` is
refused. If anything else is there, it may be the finished team, so `crew`
is refused and the message says what to do. What is in `crew2` may not be
from `crew` at all, but from another run or put there by someone else, so
first make sure it was made from `crew`, and leave it alone if not. If it
was, and it holds the finished team, use it from now on; if it is not
finished, remove it. Unless you use it, remove the mark, and `crew` is
used as before. A run that fails rather than stops, before it put the team
in place, says nothing was made, takes its own mark away, and `crew` is
used as before. This is so even when something else got to `crew2` first:
another run that put its team there, or a file put there during the run.
If it fails once the team was put in place, as when the folder above
cannot be kept on the disk, the new team may be complete even though that
step failed, so the mark stays unless `crew2` is empty or gone, `crew` is
held back, and the message says what to do, as above. The same goes for a
move into place that says it failed but moved the team even so, as a
network disk can when it tries the move again: a failed move only counts as
putting nothing in place if the team is still in the folder it was made in,
beside `crew2`. A `crew2` with
something in it already is refused before the mark is written.
`orvud setup crew` prints this command, with the numbers written out, when
it has finished.

#### Take a machine out for good

```text
orvud leave --config orvu.toml
```

Run this on the machine that goes, after stopping its sidecar. It starts the
engine from the settings, waits for the others to connect, notes that this
machine has left, hands all the allowance it holds on every budget to the
machines it reached, split evenly, and waits until each of them has said it
has everything this machine wrote. Then it stops, and says what it handed to
whom. The others give a machine that has left no allowance from then on, so
none of a budget goes with it. They believe only a mark that says the
machine wrote it itself. A program cannot mark a machine as gone, since the
sidecar refuses every name under `orvu/_machines/`. The machines of a team
are trusted, though: one with a valid certificate that sets out to cheat
could write a mark that looks like another's, or take a real one away,
since what the machines write is passed on by each other and where it came
from cannot be checked. It waits at most a minute, or `--wait
<seconds>`; if the others were not all heard to have it by then, it says so,
exits with a failure, and can be run again.

Afterwards, do not start that machine again under its number, and take its
address out of the others' `peers` when they next start. Its certificate
does not run out and nothing withdraws it yet: anyone who has its folder can
still join as that machine. Retiring a machine with a signed notice, and
certificates that run out, are to come.

### Let in only the agents you name

Out of the box, every program that can reach the socket is let in, and an
agent's name is whatever it says it is. To check who is connecting, give each
agent a key:

```text
orvud agent add alice --local
```

It prints the key, once. Only a fingerprint of it is kept, in the file
`agents` in the data folder, so the key cannot be read back from there. Give
the key to the agent, for example as `ORVU_KEY` in its environment. The
Python and TypeScript libraries read that variable.

From the first agent on, a program that gives no key, or a key the sidecar
does not know, is refused with `PERMISSION_DENIED`, and one that gives a key
is that agent: `Open` answers with its name. The libraries then use that name
for the agent, so a program that uses them writes the right name on its work.
A program that talks to the sidecar directly can still write any name into
the data: a key says who connects, not what a program writes.

Removing the last agent does not let everybody in again: the sidecar then
lets nobody in, and says so. To go back to letting every program in, delete
the file `agents`.

| Command | What it does |
|---|---|
| `orvud agent add <name> --local` | Adds an agent and prints its key |
| `orvud agent remove <name> --local` | Removes it. A program connected as it is turned away at its next call, and a watch it has open ends within a second. Being turned away ends the session too, once the file has been left alone for two seconds |
| `orvud agent list --local` | Lists the agents, and what each may do |
| `orvud agent allow <name> <rule> ... --local` | Gives an agent rules: see below |

Each takes `--data-dir <folder>` for a data folder that is not the default,
or `--config <path.toml>` in place of `--local` for a sidecar started from a
settings file. Its agents are kept in the file `agents` next to the settings
file, which has to be there. The sidecar does not have to be stopped: it
reads the file again when it changes. Two commands run at the same moment
take turns.

### Serve agents on other machines and in containers

An agent that runs somewhere else, on another computer or in a container
that cannot see this machine's files, reaches the sidecar over the network.
Add it with an address it can reach the sidecar at, and a file to write:

```text
orvud agent add alice --address 10.0.0.5:7441 --connection alice.orvu --local
```

`alice.orvu` holds all alice needs: the address, its key, and the sidecar's
certificate. Give the agent that file, as `ORVU_CONNECT` in its environment
or as `connection=`; it needs nothing else. Then start the sidecar with the
address to listen on:

```text
orvud --local --network 0.0.0.0:7441
```

The line it prints when it is ready says where it serves:

```text
orvud 0.1.0: serving on /run/user/1000/orvud.sock and on the network at 0.0.0.0:7441 as replica 4090993329555770 in namespace "local"
```

Every connection over the network is encrypted, with TLS 1.3. The sidecar
makes its own certificate the first time it is needed, and keeps it next to
the agents file, as `network-cert.pem` and `network-key.pem`. An agent trusts
that certificate and no other, so nothing has to be bought or signed, and a
program that pretends to be the sidecar cannot read what the agent sends.

Over the network, only agents with keys are let in. `--network` refuses to
start until there is at least one, a program that gives no key is refused,
and so is everybody if the agents file is deleted, which over the socket would
let every program in again. The agent's rules, and the record of who changed
what, hold over the network as over the socket.

Agents kept off this machine cannot reach its files, so they cannot add a key
or rules of their own to the agents file, or change the record.

### Say what each agent may do

An agent with no rules may do everything. Rules narrow that, block by block:

```text
orvud agent allow summariser notes:write tasks:read budgets:spend --local
```

| Block | Levels |
|---|---|
| `notes`, `tasks` | `read`, `own`, or `write`, which allows reading too |
| `documents`, `tallies`, `inbox`, `roster` | `read`, or `write`, which allows reading too |
| `budgets` | `read`, `spend`, or `top-up`, which allows spending too |
| `objects` | `read` or `write`, for the plain objects a program names itself |

`own` lets an agent read every note or task and change only its own:

- With `notes:own`, an agent adds simple notes, and changes or removes a
  note only while every value it holds was written by that agent. A note
  another agent wrote, or a program that gave no name, it leaves alone. It
  changes no structured note, as those keep no writer's name.
- With `tasks:own`, an agent adds tasks as itself, claims a task that is
  open (nobody holds it and it is not done), and changes, finishes, gives
  back or moves in the tree only a task it added or holds. Only the agent
  that added a task removes it. A task another agent holds it does not
  touch at all. On a task it names only itself, or nobody, as the one that
  holds it, finished it or last changed it. A firm task's tickets, and the
  record of a firm task taken over (`taken_over`), need `tasks:write`.

Whose a note or a task is depends on the object as it is, so before a
request from an agent with rules runs, the sidecar reads the notes and tasks
it changes, and turns the whole request away, a batch too, if any part of it
may not be made. A batch is looked at whole, as everything stands before any
of it runs: a claim in a batch allows only what a claim writes on that task
of that team's board. Then each write is checked again in the same step in
the engine as it is made, against the note or the task as it is then: a
note another agent wrote, or a task another agent claimed, after the first
look is not changed, and on an open task only the claim itself is taken, the
rest of what a claim writes only once the claim has landed. The Python and
TypeScript libraries write the name first when they claim a task nobody
holds, and last when they give one back. A write turned away then is
answered as refused and changes nothing, and the rest of the batch is not
made: every write made before it was a change of the agent's own task or
note, or its claim of an open one, as things stood at that moment. A write
another machine made that this one has not heard of yet counts as made at
the same time: a note then keeps both values until somebody settles it, and
of two claims made at once the later is kept, as without rules.

A move in the tree of tasks is looked at only before the request runs, as
the tree and the board are two objects. What it goes by cannot change in
between: the agent that added a task stays its name for as long as the task
is there, so only a task that was not on the board at the first look can
turn out to be another agent's, and moving it changes only where it hangs.

No agent with rules changes the name of the agent that added a task, by
writing the task whole or its `created_by`. An agent with rules changes the
task board one task at a time, by the task's id and its fields' names: a
change of the whole board, or one whose path starts with a position in a
list, is turned away. Who last changed a task (`updated_by`) is written by
the sidecar alone: when an agent with rules changes anything inside a task,
or writes a task whole over one that is there, the sidecar writes the
agent's name there in the same step as the change, sent as one change with
it, and what the agent writes there itself is not kept. Adding a task names
nobody. Who holds a task (`claimed_by`) and who finished it (`finished`'s
`by`, or `finished_by` on a task finished the old way) are checked as
`tasks:own` says above.

A firm task's number is the highest of the ticket numbers written under its
`tickets`, and must never go down. When an agent with rules writes a task
whole over one that is there, the sidecar carries the numbers it had over
into the new one, in the same step, beside any the write brings. Inside a
task, an agent with rules may only add a ticket number that is not there
yet: changing or taking off one that is, or `tickets` itself, is turned
away, and writing the whole of `tickets` as an object adds the numbers it
holds that are not there yet, one by one. A program
without rules that writes raw JSON can still wipe them by writing a task
whole, and so can a whole write made before numbers written on another
machine have arrived. Any agent that may remove a task can also remove it
and add it again whole, which starts its numbers again from nothing: claims
then fail until it is set back with force.

Every object a block keeps is changed by an agent with rules only as the
kind of object it is, the kind the team's list names: a change of another
kind would turn it into that kind, and the block could no longer use it.

A block a rule leaves out may not be used at all, and `none` alone allows
nothing. `orvud agent allow summariser all` takes the rules away again. An
agent with rules is held to them from its next call, and a request it may not
make is refused with `PERMISSION_DENIED`, saying which agent, what it may not
do, and its rules. A batch with one such request is refused whole, before any
of it runs, and a watch it may no longer read ends.

An agent with rules also keeps its own things to itself:

- **Its inbox.** Only it reads its messages. `inbox:write` lets an agent send
  to anyone, which needs only to see which page an inbox is on and how full
  the page is, not what is in it: a sender adds a message to a page, adds
  one to that page's count of messages, and starts the next page when one
  is full, one page at a time. It adds exactly one to a count, never more:
  a count made wrong on purpose would hide messages from the owner or give
  them again. Nobody with rules changes or removes a message. `inbox:read` lets it read its own inbox and the one for everyone,
  with their counts, and keep note of what it was given and how far it got
  on each page.
- **Its place on the roster.** It joins and leaves under its own name only.
- **What it spent and topped up.** It adds to a budget's record of who spent
  what (`orvu/<team>/budget-spent/<budget>`) under its own name only, and to
  its record of who topped it up by how much
  (`orvu/<team>/budget-topped/<budget>`) under its own name only and only
  with `budgets:top-up`. A line goes only with that top-up, of the same
  budget and amount, in the same request, and a top-up only with its line.
  The sidecar writes that line itself, right after the top-up, as part of
  making it, so a caller that stops waiting cannot leave one without the
  other. A sidecar that stops between the two, as a crash can make it, has
  made the top-up and not the line; the request sent again with its id makes
  the line, and only it. A
  top-up the budget could not hold whole is turned away, so that the line
  never says more was added than was. That is checked on each machine as the
  top-up is made, so top-ups made at once on several machines that together
  pass the most a budget holds, more than 2^63, can still record more than it
  keeps. Nobody with rules takes anything away from either record. A
  structured note and a tally's total keep no writer's name: the record of
  who changed what, below, shows it.
- **Its messages and tasks.** A message it sends carries its own name as
  the sender (`from`), and a task it adds carries its own name as the agent
  that added it (`created_by`), and that name is not changed or taken off
  afterwards. The sidecar reads these from what is written, as the Python
  and TypeScript libraries write them, and writes who last changed a task
  (`updated_by`) itself. Other names a program writes, such as who holds a
  task (`claimed_by`) or who finished it with `tasks:write`, or who wrote a
  part of a document, are what the program says: `orvud audit` shows which
  agent made each change.
- **Its notes.** A note it writes carries its own name, not another's. An
  agent with `notes:write` may still change or remove any note, and the
  note's author changes or goes with it; one with `notes:own` changes only
  its own. Notes are changed only as the kind of map they are kept in, so
  that no agent can make them another kind of object.

The team's list of what it has (`orvu/<team>/index`) can be read by any
agent, whatever its rules, as every block needs it. It gains a line only for
an object the agent may write, and only with that object's kind. The only
lines that can be taken off it are those of a firm task's tickets that can
never be had again, by an agent that may change tasks. Its lines are the
short names of the team's objects, so every agent can see the names of the
team's documents, structured notes, budgets and tallies, which inboxes and
pages have been written to, and so who has been sent messages, even an agent
that may not use those blocks. What is in them stays behind its rules. Keep
anything secret out of those names. A name under `orvu/` that belongs to no
block is refused to an agent with rules, so it is no way round them, and so
is one whose team name is empty or not one a team can have: letters,
digits, dots, dashes and underscores, starting with a letter or a digit, at
most 64 characters.

Names under `orvu/_machines/` are the machines' own, such as the list of
machines that have left. No program may read, change, make or watch
anything there, whatever its rules or with none: a call that names one is
refused with `PERMISSION_DENIED`, and a batch with one is refused whole. The
list is written only by `orvud leave`, through the engine, and a machine is
taken for gone only if the mark says it put its own number there. No
program can mark a machine as gone; a machine of the team could, since the
machines are trusted and where a mark came from cannot be checked.

### Notes written at the same time

A team's notes are kept in a map that keeps values written at once
(`orvu/<team>/notes-mv`). When two agents write one note at the same time,
neither having seen the other's, both values stay until a later write settles
it. Each value carries the name of the agent that wrote it: it is the
writer's name, the character U+001F, and then the note. A note that is text
is kept as it is. Any other JSON value is kept as the character U+001E and
its JSON text, and so is a text that itself starts with U+001E, so that it
reads back as it was. A program that gave no name writes an empty one. The
Python and TypeScript libraries keep notes this way, search reads them this
way, and a program in another language that uses the map directly has to do
the same.

Notes used to be kept in a plain map, with who wrote each in a second one. The
first time a team's notes are used, by a request, a watch or a search, the
sidecar moves the old ones over, each with the name of the agent that wrote
it, and leaves the old maps as they were. It does this once for each team.
A team that has no old notes is looked at again each time, as another
machine may still send some. A note written since is not changed by the
move. If the move fails, the request is turned away with `INTERNAL` and
why, until the move can be made whole. The old maps can still be read, and
no agent with rules may change them.

### See who changed what

Every change a program makes through the sidecar is recorded, with the agent
that made it, and so is everything an agent was not allowed to do. The record
is the file `audit.log`, next to the agents file, one JSON line per change:

```text
{"agent":"alice","change":"map_put","key":"deploy","object":"orvu/crew/notes","outcome":"done","time":"2026-10-15T14:03:12.345Z"}
```

`orvud audit` shows it, oldest first:

```text
orvud audit --local --agent alice --since 2h
2026-10-15 14:03:12  alice  map_put  orvu/crew/notes  key=deploy  done
```

| Flag | Shows only |
|---|---|
| `--agent <name>` | One agent's changes. `-` is those made with no key |
| `--object <start>` | Changes to objects whose names start with this, such as `orvu/crew/` for one team |
| `--since <time>` | Changes from this long ago or later, such as `30m`, `2h` or `7d` |
| `--not-allowed` | What agents were not allowed to do |
| `--json` | The lines as they are kept, for other tools |

The record keeps the shape of a change, not what was written: the object,
the kind of change, a key, an item, a path in a document, a place and an
amount, but no value, member of a set, message or text. So it shows that
alice changed the note called deploy, not what the note now says. Keys,
items and paths are kept as they are: a program that keeps data in them, and
not only names, finds that data in the record. Reads are not recorded, apart
from a read that was not allowed. The outcome is `done`, `refused` (such as a
budget that had run out), `failed` (with the engine's code for why, but not
its words, which can quote what was written), or `not allowed` (with what the
agent was told). A batch that was not allowed is recorded by the request
that was refused, since none of it ran. A change is recorded even if the
program that asked for it stopped waiting, and one the sidecar broke off
partway, because of a fault of its own, is recorded as `failed`.

Text that an agent chose, such as a key, is shown with line breaks and
terminal codes written out, so that it cannot pass for another line.

### Search by meaning

The `Search` operation finds the notes, structured notes, paragraphs of
documents and tasks of a team whose meaning is closest to a question, even
when they share no word with it. It names the team's index,
`orvu/<team>/index`, and the answer is JSON text, the closest first:

```text
[{"kind":"note","name":"refund-policy","text":"refund-policy: money back within 30 days","score":0.42},
 {"kind":"document","name":"handbook","paragraph":3,"text":"A refund is paid ...","score":0.31},
 {"kind":"task","name":"t-3f9a1c2e5b7d4a60","title":"Pay the refund","text":"Pay the refund: ...","score":0.3}]
```

A task, on the team's board `orvu/<team>/tasks`, is searched by its title and
its notes; it is found under its id, with its title. Messages are not
searched, as an inbox is read only by its owner.

The model is potion-base-8M, from Minish Lab, 30 MB, under the MIT licence.
The first time a team searches, the sidecar fetches it from Hugging Face
into `models/potion-base-8M` in its data folder (with `--config`, the
settings file's folder), checks each file against the SHA-256 it should
have, and loads it from the bytes it checked. After that nothing goes over
the internet. With `--model <folder>`, or `ORVU_MODEL=<folder>` in its
environment, it uses the three files in that folder instead, fetches
nothing, and checks nothing: whatever can write to that folder, or set that
variable, chooses the model. On a machine with no internet, fetch the files
on another one from the fixed version the sidecar fetches,
`https://huggingface.co/minishlab/potion-base-8M/resolve/bf8b056651a2c21b8d2565580b8569da283cab23/<file>`, and check them with `sha256sum`:

| File | SHA-256 |
|---|---|
| `config.json` | `2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553` |
| `tokenizer.json` | `e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50` |
| `model.safetensors` | `f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2` |

`ORVU_MODEL_SOURCE=<address>` fetches the same files from another place,
such as a copy inside a network with no internet; each is checked against
the table all the same. A model that could not be fetched or loaded is not
tried again for a minute, and a search in that minute is told why at once. A
sidecar asked to stop while it fetches stops within a few seconds.

A long text is searched in pieces of at most a hundred words and a thousand
characters, as the model reads only the start of a text, and each piece of a
note starts with the note's name. A note or a paragraph is found once, with
its closest piece. An agent that may read none of notes, documents and tasks
is not let search, and tasks are found only by an agent that may read tasks.

A team's index is made the first time it is searched, from everything its
index object lists, and kept in memory. A team that has nothing yet is
answered with nothing, and nothing is kept for it. The sidecar follows the
changes from the first search on: they show up within a moment, and each
searched team's objects are read again every minute, and at once when the
engine says changes were missed. An agent with rules finds only what it may read,
by its rules at the time it searches. A search that cannot be done, because
the model cannot be fetched or read, is answered with `CODE_NO_SEARCH` and
why. A search cannot be part of a batch.

### See what it is doing

`orvud` can keep a log, and serve a page of numbers that a tool such as
Prometheus collects. Both are off unless they are asked for.

From a settings file, they are two more sections of it. The settings `orvud
setup` writes have them, behind a `# ` to take off:

```toml
[logs]
level = "info"
file = "orvud.log"

[metrics]
addr = "127.0.0.1:9464"
```

With `--local` there is no settings file. The same two sections go in a file
of their own, `orvud.toml`, in the data folder. The only other section it may
hold is `[storage]`, below. It is read when `orvud` starts.

| Setting | What it does |
|---|---|
| `[logs] level` | How much to write: `error`, `warn`, `info`, `debug` or `trace`. Left out, there is no log. The libraries `orvud` is built on write only their warnings and errors, whatever the level |
| `[logs] file` | The file the log is added to. A path that is not absolute is taken from the folder the settings are in. Left out, the log goes to the error output. A file with no level is a mistake |
| `[metrics] addr` | Where to serve the page, at `/metrics`. Left out, there is no page |
| `[storage] fsync` | `--local` only. When a change is saved to disk. `"always"`, the default: before the change is answered, so a change reported as done survives the sidecar being killed or the power going off. `"interval"`: changes are saved together every so often, which is faster, but the last moment's changes can be lost that way |
| `[storage] interval_ms` | `--local` only. How often to save, in milliseconds, with `fsync = "interval"`. The default is 100 |

For example, to save every 100 milliseconds instead of before each answer:

```toml
[storage]
fsync = "interval"
```

The page has the engine's numbers, such as how many objects it holds and how
many changes it took in, and the sidecar's own:

| Number | What it counts |
|---|---|
| `orvu_sidecar_operations_total{op, result}` | Each operation by its name, and how it ended: `done`, `refused` or `failed`. An operation that never happened has no line |
| `orvu_sidecar_rejected_total{call}` | Calls the sidecar turned away, by kind of call: `execute`, `batch` or `watch`. One with an unknown session, one that is itself broken, or a batch that is too large. Each counts once, and none of its operations is counted |
| `orvu_sidecar_execute_seconds`, `orvu_sidecar_batch_seconds` | How long calls took, as histograms |
| `orvu_sidecar_watches_total` | Watches opened |
| `orvu_sidecar_connections` | Clients connected now |

A setting that is not right stops `orvud` before it starts, with a message
that names it. So does a page address that is already in use: it is found
before anything is written and before the machine joins the others.

### Talk to it

A client is generated from `dataplane.proto` with the gRPC tools for its
language, and connects to the socket. It calls `Open` once and then sends
operations with the session id it got back.

The example below sends the same requests from Rust, without a socket, the way
this module's own tests do. A generated client in another language builds the
same messages.

```rust
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use orvud::server::Dataplane;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Start an engine and put the service in front of it.
    let network = Arc::new(MemTransport::new());
    let engine = Client::start(network.endpoint(1), NodeConfig::new(ReplicaId(1), "alice"))
        .await
        .unwrap();
    let service = Dataplane::new(Arc::new(engine), ReplicaId(1), "team-a");

    // Open a session.
    let opened = service
        .open_for_test(pb::OpenRequest {
            namespace: "team-a".into(),
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .unwrap();
    let session = opened.session;

    // Add 5 to a counter.
    let add = pb::request::Op::CounterIncrement(pb::CounterIncrement { object: "visits".into(), n: 5 });
    let reply = service
        .execute_for_test(pb::Request { session: session.clone(), op: Some(add), request_id: Vec::new() })
        .await
        .unwrap();
    assert!(matches!(reply.result, Some(pb::response::Result::Ack(_))));

    // Read it back.
    let read = pb::request::Op::CounterValue(pb::CounterValue { object: "visits".into() });
    let reply = service
        .execute_for_test(pb::Request { session, op: Some(read), request_id: Vec::new() })
        .await
        .unwrap();
    match reply.result {
        Some(pb::response::Result::Value(pb::ReadValue { v: Some(pb::read_value::V::IntValue(n)) })) => {
            assert_eq!(n, 5)
        }
        other => panic!("expected a number, got {other:?}"),
    }
}
```

### A refusal is an answer, not an error

A counter that cannot go below zero says no when there is not enough left. That
comes back as an ordinary reply with `refused` set, and everything is as it was
before. It is not reported as a failed call.

```rust
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_sdk::Client;
use orvu_transport::mem::MemTransport;
use orvud::pb;
use orvud::server::Dataplane;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let network = Arc::new(MemTransport::new());
    let engine = Client::start(network.endpoint(1), NodeConfig::new(ReplicaId(1), "alice"))
        .await
        .unwrap();
    let service = Dataplane::new(Arc::new(engine), ReplicaId(1), "team-a");
    let session = service
        .open_for_test(pb::OpenRequest {
            namespace: String::new(), // empty means "whatever this sidecar serves"
            resume_token: String::new(),
            credential: Vec::new(),
        })
        .await
        .unwrap()
        .session;

    let put_in = pb::request::Op::BoundedIncrement(pb::BoundedIncrement { object: "budget".into(), n: 10 });
    let take_out =
        pb::request::Op::BoundedTryDecrement(pb::BoundedTryDecrement { object: "budget".into(), n: 100 });

    // Both operations go in one call.
    let replies = service
        .batch_for_test(pb::BatchRequest {
            session,
            requests: vec![
                pb::Request { session: String::new(), op: Some(put_in), request_id: Vec::new() },
                pb::Request { session: String::new(), op: Some(take_out), request_id: Vec::new() },
            ],
            request_id: Vec::new(),
        })
        .await
        .unwrap()
        .responses;

    assert!(matches!(replies[0].result, Some(pb::response::Result::Ack(_))));
    assert!(matches!(replies[1].result, Some(pb::response::Result::Refused(_))));
}
```

## Things to know

- **`orvud setup` keeps the key that signs the certificates,** in
  `authority` beside the machines' folders, readable only by you. It is what
  lets `--add` add a machine. Whoever has it can make a machine the others
  trust. To take a machine out without `orvud leave`, run `orvud setup`
  again for the ones that stay, in a new folder `--from` the old one, with
  their numbers, and keep each machine's `data` folder.
- **A machine keeps its number.** `orvud setup` numbers the machines from 1
  in the order given. Write `#number` after an address to give a machine the
  number it had before, as in `gamma=10.0.0.7#3`. The command prints the
  whole line with the numbers in it when it has finished. A sidecar refuses
  to start on data that was written under another number. A number can be at
  most 9007199254740991 (2^53 - 1), the largest a program that keeps numbers
  as floating-point values, such as JavaScript, holds exactly.
- **`orvud setup` never writes into a folder that has something in it,** and
  if it cannot finish it leaves nothing behind.
- **A machine given by name listens on `[::]`,** which takes IPv4 and IPv6
  connections. One given by an IPv4 address listens on `0.0.0.0`. On a
  machine with IPv6 switched off `[::]` cannot be used: the sidecar then
  listens on `0.0.0.0`, IPv4 only, and says so when it starts.
- **Settings cannot use the namespace `local`.** It is the namespace of a
  sidecar started with `--local`, and a program would take a sidecar that
  serves it for one. `--config` refuses such settings, saying why.
- **Without `--network`, only programs on the same machine can connect.**
  `orvud` listens on a Unix socket, which only the user who started it can
  open. With `--network`, agents elsewhere connect too, encrypted and with
  keys.
- **The sidecar itself runs on Linux and macOS.** Agents on Windows reach
  one there over the network.
- **The address in a connection file is the one an agent dials,** which is
  not always the one the sidecar listens on: a sidecar that listens on
  `0.0.0.0:7441` is reached at the machine's own address. The certificate is
  made out to the name `orvud`, not to an address, so it does for any.
- **A connection file holds a key.** Keep it as secret as the key. To take an
  agent's access away, remove the agent, as for any agent.
- **The network takes a limited number of connections:** 256 open at once,
  and 64 agreeing on encryption at once. More wait their turn, so a flood of
  connections cannot take all the sidecar has. A connection whose other end
  has gone without a word is closed once it fails to answer a ping, sent every
  half a minute. While connections are open, `--exit-when-idle` waits.
- **A watch over the network ends when its agent is no longer let in,** as
  over the socket, and when the agents file is deleted.
- **The certificate is made once,** by whichever program needs it first, the
  others waiting their turn. A key found without its certificate, as a
  program stopped halfway would leave, is made again with a new certificate.
- **Keys keep agents apart only if they cannot change the sidecar's files.**
  A program running as the user the sidecar runs as can write a key of its
  own into the agents file. To keep an agent from doing that, run it where
  it can reach the socket but not the data folder, for example in a
  container that is given only the socket.
- **Each machine keeps its own record,** of what the programs connected to
  it did. A change that comes from another machine is in that machine's
  record. The file is cut when it passes 64 MiB: it becomes `audit.log.1`,
  the one before becomes `audit.log.2`, and an older one is dropped. A
  program running as the sidecar's own user can change the file, as it can
  the agents file.
- **Rules go by block, not by object.** An agent that may write tasks may
  change any task, not only its own. A firm task's tickets count as part of
  its task.
- **The sidecar checks operations, not what they write.** Apart from the
  names checked above, an agent that may write a block can write anything
  into it, such as another agent's name on a task.
- **Each machine has its own agents.** An agent connects to the sidecar on its
  own machine, and only that sidecar's list matters. The machines of a team
  do not share their lists.
- **A key given to a sidecar with no agents is refused.** The program expects
  to be checked, and is told that nothing would check it.
- **An agents file that cannot be read lets nobody in.** A program is refused
  with a message that says what is wrong with the file.
- **One `orvud` runs one engine and serves one namespace.** `Open` with a
  different namespace is refused.
- **Three kinds of reply.** `ack` or `value` means it worked. `refused` means
  the engine said no and nothing changed. `failed` carries a code that says
  what was wrong, such as the wrong type for that object. A write the engine
  could not give a fresh name, which only a broken or hostile machine can
  bring about, is `failed` with `CODE_ENGINE`. A gRPC error is only
  used for a request that is itself broken, such as an unknown session, or a
  `richtext_format` or `richtext_insert_marked` with an empty value.
- **A sidecar that cannot save stops serving.** If saving to disk fails, for
  example because the disk is full, memory may hold a change that is not on
  disk, so nothing is shown from then on. Every operation, reads and searches
  included, is answered `failed` with `CODE_ENGINE` and a detail that starts
  with "cannot save changes" and says why. An open watch ends with a `failed`
  event that says the same, and a new one gets only that event. A change
  whose save failed is never answered as done, and nothing more is sent to
  other machines, which see it as gone. Then it stops: it answers the
  requests already in progress, closes its socket, prints "orvud: cannot
  save changes: ...; stopping so that it can be started again from what is
  on disk" and exits with a failure status. A service manager set to restart
  it on failure starts it again, and a sidecar the Python or TypeScript
  library started is started again by the next `connect`. A restart loads
  what is on disk; if the disk is still full, it fails again the same way.
- **A batch is not all-or-nothing.** Each operation gets its own reply, and one
  being refused does not undo the others. The one exception is a request that
  is itself broken: then the whole batch is rejected before anything runs.
- **A request on the Unix socket may be up to 64 MiB.** A larger one is
  refused with `OUT_OF_RANGE` before it is read. gRPC's own limit, 4 MiB,
  would leave out a large value or a large batch of writes.
- **A request over the network may be up to 4 MiB.** A request is read in
  full before its key is checked, so anyone who can reach the port could
  otherwise make the sidecar hold 64 MiB for each request they send. A larger
  one is refused with `OUT_OF_RANGE`.
- **One connection has at most 32 calls being read and answered at once,** on
  the socket and the network alike. More wait their turn on that connection;
  other connections are not held up. Together with the size limit this bounds
  the memory one connection can take. A watch counts only until it starts
  sending, so a connection can have many more watches open than that.
- **A batch has limits.** It may hold 1,024 operations. Its replies may add up
  to 4 MiB, and operations past that point are not run and come back as
  `CODE_TOO_LARGE`. Once started, a batch runs to the end even if the caller
  stops waiting, unless the sidecar starts stopping: then the operations not
  yet run are not run, and each comes back as `CODE_ENGINE` saying so.
- **Watches have limits.** One session may have 256 watches open, and the
  sidecar 4,096 over all sessions, on the socket and the network together.
  Past that a new watch is refused with `RESOURCE_EXHAUSTED`. A watch that
  ends gives its place back.
- **Sessions have a limit too:** 1,024. Past that, opening one drops the
  session used longest ago, and nothing else decides which. A call counts as
  using a session, and so does each value one of its watches sends. A watch
  with nothing to send does not, so sessions that only wait cannot crowd out
  programs that make calls. A dropped session's watches end with
  `UNAUTHENTICATED`, so a session's watches never outlive it. A client
  answers that by opening a new session and watching again. That new session
  is the newest of all, so it pushes out just one more session, the next one
  used longest ago.
- **One agent may hold at most 256 sessions,** a quarter of them. An agent
  that opens one more drops its own session used longest ago, never anyone
  else's, even while places are free. So an agent that keeps many sessions
  busy, for example by watching an object it writes all the time, cannot push
  out other agents' programs. Over the network every session belongs to an
  agent with a key, so no one agent can fill the table there. Programs without
  a key, which only come in on the Unix socket while no agent is named, are
  trusted local programs: they are not held to a share and use the places
  that are left, as before.
- **The share belongs to the key, not only the name.** An agent whose key
  was replaced, by removing it and adding it again, is held to a share of its
  own, apart from the sessions its old key opened. A call on one of those old
  sessions is turned away with `PERMISSION_DENIED` and ends the session, so
  whoever still has the old key cannot keep them in use. The same goes for any
  session whose agent is no longer let in. A session is only ended once the
  agents file has been left alone for two seconds, though. A file that was
  changed a moment ago may still be half-written, such as by an editor that
  empties it before writing it again, so a call turned away then is refused
  but leaves its session open, and the session works again once the file is
  whole. An editor that saves by moving the old file aside and writing a new
  one leaves no file at all for a moment, and that does not end sessions
  either: a missing file only counts once its folder has also been left
  alone for two seconds. An agents file that cannot be looked at at all, or a
  missing one whose folder cannot be, never ends a session. A call turned
  away like this does not count as using its session.
- **Reading does not create anything.** A read of an object that does not
  exist answers `absent`. A watch on one stays quiet until the object appears.
  An object is created by the first write to it.
- **A client must send an ordinary server name.** Over a Unix socket, some
  gRPC libraries send the socket's path as the name of the server they are
  talking to. That is not a valid name and the request is dropped with a
  protocol error. Set the name to something plain, such as `localhost`. In
  Python that is the channel option `grpc.default_authority`.
- **Sessions live in memory.** After `orvud` restarts, a client calls `Open`
  again and gets a new session.
- **A change sent again with the same request id is made once.** A client
  gives each call that changes something an id, 16 random bytes, and sends
  the call again with the same id when no answer came, as after a time limit
  or when the sidecar stopped partway. A batch is one call. Ids are each
  agent's own: another agent's call with the same id is made, and is not
  told the first one's answer. Programs that came in without a key share
  one set of ids. The id of a call that changes nothing is not used: such a
  call is simply made again.
- **What says a change was made is its receipt.** Each change made for an
  id leaves a receipt, saved in the same step as the change, so the two are
  on disk together or not at all. A call sent again whose receipt is there is
  answered `ack` and not made again, whether the sidecar restarted in
  between or not; in a batch, each part that changes something has a
  receipt of its own, and the parts that were not made are made then. A top-up
  of a budget and its line in the record of who topped it up have a receipt
  each, so a call sent again after only the top-up was saved makes only the
  line. Receipts are kept for ten minutes: a call sent again later than that
  is made again. With `[storage] fsync = "interval"` a change and its
  receipt can be lost together, and the call sent again is then made, once.
- **Recent answers are kept in memory too.** For ten minutes, and for each
  agent at most its last 10,000 ids and 4 MiB of answers, a call sent again
  gets the very answer the first got, a refusal included, and one sent while
  the first is still being made waits for it. Only the answers of changes
  are kept: a batch sent again reads its reads afresh. A call whose answer
  was let go of, or that came after a restart, is answered from its
  receipts, so it is still not made twice. A call with no id is made each
  time it arrives, as before.
- **`ORVUD_TEST_STOP_AFTER_SAVING` is for tests,** and only a debug build
  looks at it. If it names a file that is there, the sidecar removes the
  file and stops at once after the next call with a request id is made and
  saved, without answering it; if the file says `top-up`, it stops instead
  right after a top-up is saved and before its line is. Tests use it to see
  that a client sends such a call again and that it is made once.
- **Positions in text count characters, not bytes.** A language whose strings
  count differently, such as Java or JavaScript, has to convert.
- **A large read comes back as one message.** gRPC clients refuse messages over
  about 4 MiB unless told otherwise. Raise that limit on the client for large
  objects.
- **One `orvud` per data folder.** A second one pointed at the same folder is
  refused. The folder is locked for as long as the first one runs, and the
  lock is released when it ends, however it ends. Started from a settings
  file, it locks the folder its storage file is in, whichever settings file
  names it, or the settings file's own folder if it keeps nothing on disk.
- **Keep that folder on a local disk.** On a network file system (NFS, SMB,
  9P, SSHFS and the like) the lock may not hold, and two sidecars using one
  folder can damage its data. On Linux `orvud` tells such a file system by
  the number the system gives its kind, and prints a warning when it starts
  on one. It still starts.
- **A storage folder keeps its machine number.** Started from a settings
  file, `orvud` writes the number into a file `replica` in the storage folder
  the first time the engine starts on it, and refuses to start if the
  settings later name another number for that folder. A start that is
  refused writes nothing, so starting again, by hand or by a service
  manager, is refused the same way. The saved data also notes its own
  number, in a file beside it (`orvu.redb.replica` for `orvu.redb`). If the
  two files name different machines, `orvud` says so and names the file to
  put right: the data's own note is the one to go by. The `replica` file is
  never taken as leave to take on data that has no note of its own: only
  `adopt_data = true` in the settings is. A local sidecar's data folder can
  be run from a settings file this way: give the settings its number, from
  its `replica` file, and a certificate made out to that number. A folder
  last run by an older `orvud --local` has no note on its data yet; start it
  once with `orvud --local` to make one.
- **A local sidecar has a machine number of its own,** picked at random the
  first time and kept in the file `replica` in the data folder. Two machines
  must never share a number, so the folder also notes which computer it is
  used on, in the file `computer`: a value worked out from the computer's
  id, never the id itself. On Linux that id is `/etc/machine-id`; on a Mac
  it is the hardware id the system gives, the one System Information shows
  as "Hardware UUID". A folder whose note names another computer was copied
  or moved, and `orvud --local` refuses it. The message gives the command to
  run, with the folder spelled out; a folder whose name is not valid text
  is spelled byte by byte in `$'...'` quotes, as bash and zsh read them. If
  the computer it came from still uses its copy, start it once with
  `orvud --local --data-dir <folder> --new-number`: the data is kept, what
  was saved keeps the number it was written under, and this computer writes
  under a new number of its own from then on. Taking the new number writes
  the note beside the data first and the note of the computer last, so if
  it stops partway the folder still looks copied: the next start is refused
  as before, and `--new-number` can simply be run again. If the folder was
  moved and is no longer used where it came from, start it once with
  `orvud --local --data-dir <folder> --moved` to keep the number. Either way the note is then made out to this computer, so later
  starts need no flag, and on a folder that is already this computer's the
  two flags change nothing. A folder from before notes were kept gets one
  the next time it starts. The Python and TypeScript libraries start the
  sidecar with no flag, so when it refuses a folder their error passes on
  its message, command and all, and the command is run by hand once.
- **A data folder restored from a backup must take a new machine number.**
  This holds for a folder rolled back in any way: a backup, a disk
  snapshot, a copy put back over it. The machine has lost changes it may
  already have passed on, and nothing tells it so. Under its old number its
  next writes reuse numbers that other machines, or another copy of the
  folder, already hold for the lost changes; they take the new writes for
  ones they have and keep them out, and the copies disagree for good. So
  start a local sidecar on a restored folder once with
  `orvud --local --data-dir <folder> --restored`: the data is kept, what was
  saved keeps the number it was written under, and new writes go under a new
  number, on this computer as on any other. Later starts need no flag;
  passing it again only takes yet another number, and each object keeps
  changes from at most 1024 machines. (`--new-number` does not do this: on
  the folder's own computer it changes nothing.) A sidecar run from settings
  keeps its number for as long as it keeps its data, so restoring its
  storage means starting it as a new machine: a new `replica` number, a
  certificate made out to it, and empty storage. It then gets the data back
  from the others. What still happens by itself: once caught up, a restored
  machine has its lost changes back from the others, and a change it makes
  after that is numbered after them. Only the writes it makes before then
  are at risk, and a new number takes that risk away.
- **A write the engine cannot make says why.** A machine new to an object
  that already has changes from 1024 machines, the most one takes, is
  refused a write to it, because every other machine would refuse that
  change. A machine whose change numbers for an object a hostile machine
  has used up can only read that object. Either way the reply is a failure
  whose detail says so, and the Python and TypeScript libraries raise it as
  an `EngineError` with that message.
- **Only the computer's id tells computers apart.** A host name is not used:
  it can change under a computer that stays the same. Where the id cannot be
  read, as in a Linux container that has no `/etc/machine-id`, no note is
  written and none is checked, so a copied folder is not caught there. Keep
  one copy of a data folder in use at a time on such a computer. A folder
  made before its computer's id could be read has no note, and is taken to
  belong to the computer it is next started on.
- **A computer whose machine id changes** looks like another computer to its
  own data folder, for example a container with its own `/etc/machine-id`
  that is made again over the same folder. Start it once with `--moved`; the
  note is then made out to the new id.
- **The data folder must be private.** A folder `orvud` creates is. One that
  already exists and that other users can get into is refused, and the message
  gives the `chmod` command that fixes it.
- **Keep the `replica` file with the data.** If the folder holds saved data
  but that file is missing, `orvud` refuses to start. The data was saved under
  one machine number, and starting under a new one would strand part of it.
- **A flag may be given once.** Giving `--data-dir` or `--socket` twice is an
  error, so a script cannot quietly redirect where the data goes.
- **`Open` tells the client which version of `orvud` answered,** and its
  process id, so a client can name the process to stop when the two are of
  different releases.
- **Local mode is the same engine.** The differences are that it has nobody
  to sync with, that its namespace is called `local`, and that it saves each
  change to disk before it answers, unless `orvud.toml` says otherwise.
- **`orvud` prints one line when it is ready,** saying which socket it serves
  on, and `orvud: stopped` when it has shut down. It prints them whether or
  not it keeps a log. Nothing it prints to standard error can hold it up: a
  line that cannot be written, because whoever read it went away or stopped
  reading, is dropped.
- **The page of numbers has no password.** Anyone who can reach its address
  can read it. Serve it on `127.0.0.1`, or on a network only you use.
- **Stop it with Ctrl-C or SIGTERM.** It finishes the requests in progress,
  saves what is not on disk yet and removes its socket file. Killing it
  outright (SIGKILL) skips all of that. From the moment it is asked, it takes
  no new work: a new request is refused with `UNAVAILABLE` and the message
  "the sidecar is stopping", which means it did not run and can be made again
  once a sidecar is back, a batch runs no
  more of its operations, and an open watch ends with `UNAVAILABLE` within a
  second. Requests still in progress after five seconds are cut off.
- **A second `orvud` on the same socket path is refused.** It never takes over
  a socket that another one is serving.
- **A sidecar killed outright while starting can leave a folder behind**
  next to its socket, named `.orvud-` and eight letters and digits, with
  only a socket that nobody serves in it. A start in the same place removes
  it once it has been left alone for a minute; one changed more recently may
  belong to a sidecar starting at that moment. A folder of that name that holds anything else, or a socket
  that answers, is left alone.
