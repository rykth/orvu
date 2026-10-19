# orvu (TypeScript)

The TypeScript client for Orvu, for Node.

Orvu keeps shared data, such as counters, lists, documents and budgets, the
same on every machine that uses it. This package is how a Node program reads
and changes that data. It talks to `orvud`, a small program that runs in the
background on the same machine and does the real work. A Python program and a
Node program of the same user share one `orvud`, and see the same data.

It runs in Node 20 or later, on Linux and on a Mac. On Windows, it connects
to an `orvud` on Linux or a Mac over the network: see "From another machine,
or a container" below. It does not run in a browser.

## What is inside

| Piece | What it is |
|---|---|
| `Team` | What a team of agents shares: notes, a task board, budgets, documents, an inbox, tallies and a roster, and search by meaning over its notes, documents and tasks. Start here |
| `connect()` | Connects to this user's `orvud`, starting it if it is not running, and gives a `Client` |
| `connect(socket)` | Connects to an `orvud` you started yourself |
| `Client` | Hands out objects by name, and sends batches |
| `Counter`, `BoundedCounter`, `Register`, `TextSet`, `TextMap`, `MvMap`, `Cart`, `TextList`, `Json`, `RichText`, `Tree` | The eleven kinds of shared object |
| `Batch` and `Pending` | Several operations sent in one message, and their results |
| `OrvuError` and the errors under it | What can go wrong, each with its own name |

## How to use it

```text
npm install @orvuhq/orvu
```

`@orvuhq/orvu` is the library, the same on every machine. `orvud` comes in a
package of its own for each kind of machine: `@orvuhq/orvud-linux-x64`,
`-linux-arm64`, `-darwin-arm64` and `-darwin-x64`. The library names all
four as optional, and npm installs only the one that fits, so on Linux or a
Mac, on Intel or ARM, there is nothing else to set up. So far only
`@orvuhq/orvud-linux-x64` is on npm. On Linux on ARM or on a Mac, build both
from this repository for now, as "Working on this package" below says, and
install the two files together; npm refuses to install a sidecar package on
another kind of machine. On Windows none fits, and the library connects to an
`orvud` elsewhere over the network instead.
The first `connect()` starts `orvud` in the background, and every program you
run after that shares it, whether it is written in TypeScript or in Python.

[`examples/quickstart.mjs`](examples/quickstart.mjs) is the shortest program
that uses Orvu. Run it twice: the second run sees what the first one wrote.

`connect()` looks for `orvud` in `$ORVUD_BIN`, then in the
`@orvuhq/orvud-...` package for this kind of machine, then on the `PATH`. The
package also gives a command, `orvud`, for running it by hand, as in
`npx --no-install orvud setup` for a team on several machines. The command
finds `orvud` the same way and runs it with what you typed after it.
Without `--no-install`, `npx` run outside your project would fetch whatever
package of that name it finds online.

### A team: notes, tasks, a budget and more

`Team` is the friendly layer. Every agent that connects with the same team
name shares the same notes, task board, budgets, documents, inbox, tallies
and roster, whether it is written in TypeScript or in Python.

```ts
import { Team } from "@orvuhq/orvu";

const team = await Team.connect("default", { me: "researcher-2" });

// Notes: small facts by name.
await team.notes.put("deploy-date", "Friday");
console.log(await team.notes.get("deploy-date")); // Friday

// Tasks: add, claim, finish.
const task = await team.tasks.add("Summarise the Q3 report");
if (await team.tasks.claim(task)) {
  await team.tasks.stillWorking(task); // now and then, or the claim runs out
  await team.tasks.finish(task, "It went up.");
}
console.log((await team.tasks.get(task))?.state); // done

// A firm task: one agent at a time can have it.
const invoice = await team.tasks.add("Send the invoice", { firm: true });
console.log(await team.tasks.claim(invoice)); // true

// A budget that cannot be overspent.
const tokens = team.budget("tokens");
await tokens.topUp(2000);
console.log(await tokens.spend(1500)); // true
console.log(await tokens.spend(1500)); // false: not enough left, nothing spent

// A document several agents write together. Each part says who wrote it.
const report = team.document("q3-report");
await report.append("Sales rose 4%.");
console.log(await report.writtenBy()); // [ [ 'Sales rose 4%.', 'researcher-2' ] ]

// Search by meaning: the notes and documents closest to a question.
console.log((await team.search("When do we ship?"))[0].name); // deploy-date

// Messages to one agent, or to everyone.
await team.inbox.send("researcher-1", { task: task.id, say: "please check this" });
await team.inbox.sendAll("Deploy is on Friday.");
console.log((await team.inbox.unread()).map((message) => message.body)); // [ 'Deploy is on Friday.' ]

// Counting, per agent and in total, and who is here.
await team.tally("tokens-used").add(1500);
await team.roster.join();
console.log(await team.roster.here()); // [ 'researcher-2' ]

// React when something changes.
for await (const tasks of team.tasks.watch()) {
  console.log(tasks.length); // 2
  break; // a real agent would carry on here
}

await team.close();
```

| On `team` | What it is | Its methods |
|---|---|---|
| `notes` | Small facts by name. Values are anything JSON holds. `get` throws `Clash` for a note agents wrote at the same time | `put`, `get`, `remove`, `all`, `author`, `authors`, `structured`, `structuredNames`, `watch` |
| `tasks` | The task board | `add`, `list`, `get`, `update`, `claim`, `claimTicket`, `stillWorking`, `stillHolding`, `release`, `finish`, `move`, `children`, `watch`. `abandoned(task)` says whether a task's agent has gone quiet |
| `budget(name)` | An amount that cannot be overspent | `topUp`, `spend`, `left`, `spentBy`, `toppedUpBy`, `watch` |
| `document(name)` | Shared text that marks who wrote what | what a rich text has, and `append`, `writtenBy` |
| `inbox` | Messages to one agent, or to everyone | `send`, `sendAll`, `unread`, `messages`, `history`, `watch` |
| `tally(name)` | A count per agent and in total | `add`, `total`, `byName`, `watch` |
| `roster` | Who is on the team, and who is here | `join`, `leave`, `members`, `here`, `watch` |
| `search(query, limit)` | The notes, structured notes, paragraphs of documents and tasks whose meaning is closest to `query`, the closest first, 5 unless `limit` says, up to 50 | Each `Found` has its `kind`, `name`, `text`, `score` from 0 to 1, for a document its `paragraph`, and for a task its `title`, with its id as `name` |

Every `watch()` of a block gives a value at once, and again each time
something changes. The inbox is the one that does not: it gives each message
as it arrives, so an empty inbox gives nothing until a message comes. Every
`watch()` takes `{ signal }`, an `AbortSignal` that ends the loop.

`search` finds what is closest in meaning to the question even when it shares
no word with it. A task is found by its title and notes; messages are not
searched, as an inbox is read only by its owner. The last of what it finds
may have little to do with the question: the `score` shows how close each is.
An agent with rules finds only what it may read, so tasks only with
`tasks:read`, and what is written shows up in what is found within a moment.
The first search on a sidecar fetches the model it uses, potion-base-8M,
30 MB, from the internet, once, into the sidecar's data folder, so a search
waits ten minutes for its answer, or `timeoutMs` if that is longer. On a
machine with no internet, fetch `config.json`, `tokenizer.json` and
`model.safetensors` from the version the sidecar's README names on another
one into a folder, check them against the SHA-256 there, as the sidecar does
not check a folder it is given, and name the folder in `ORVU_MODEL` before
the sidecar starts, or start it yourself with `orvud --model <folder>`. The
model knows English.

### Objects by name

```ts
import { connect } from "@orvuhq/orvu";

const client = await connect();

const visits = client.counter("visits");
await visits.increment(5);
console.log(await visits.value()); // 5

// A budget that cannot be overspent. Running out is an answer, not an error.
const tokens = client.boundedCounter("tokens");
await tokens.increment(2000);
console.log(await tokens.tryDecrement(1500)); // true
console.log(await tokens.tryDecrement(1500)); // false: not enough left, nothing spent

const doc = client.json("plan");
await doc.set(["steps"], ["research", "draft"]);
await doc.insert(["steps"], 2, "publish");
console.log(await doc.get(["steps"])); // ["research", "draft", "publish"]

client.close();
```

### Several operations in one message

```ts
import { connect } from "@orvuhq/orvu";

const client = await connect();
const batch = client.batch();
batch.counter("visits").increment(1);
const left = batch.boundedCounter("tokens").tryDecrement(3);
const notes = batch.list("notes").toArray();
await batch.send();
console.log(left.value, notes.value);
client.close();
```

Each method of a handle from a batch gives a `Pending`. Its `value` is there
once the batch has been sent.

### React when something changes

```ts
import { connect } from "@orvuhq/orvu";

const client = await connect();
await client.register("status").set("starting");
for await (const status of client.register("status").watch()) {
  console.log(status); // the value now, and again on every change
  break; // a real program would carry on here
}
client.close();
```

### Agents with keys

Out of the box, an agent is called whatever it says it is, and any program
of yours can connect. To have the sidecar check who is connecting, give each
agent a key:

```text
npx --no-install orvud agent add alice --local
```

It prints the key once. Put it in the agent's environment as `ORVU_KEY`, or
pass it as `Team.connect("default", { key })`. The agent then needs no `me`:
`team.me` is the agent the key was given to, and its tasks, notes and
messages carry that name. From the first agent on, a program with no key, or
with a key the sidecar does not know, gets `NotAllowed`. `orvud agent remove
alice --local` turns alice away at its next call. Removing the last agent
lets nobody in, until the file `agents` in the sidecar's data folder is
deleted.

A key keeps agents apart only if they cannot change the sidecar's files: a
program running as your user can add a key of its own. Run an agent you do
not trust where it can reach the socket but not the data folder, such as in
a container given only the socket.

Each agent may do everything unless it is given rules, block by block:

```text
npx --no-install orvud agent allow summariser notes:write tasks:read budgets:spend --local
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
raises `NotAllowed`, with a message that says why.

The sidecar records every change, with the agent that made it, and what an
agent was not allowed to do. `npx --no-install orvud audit --local --agent alice --since 2h`
shows it. The record keeps what was changed and how, not what was written:
that alice changed the note called deploy, not what the note says.

### From another machine, or a container

An agent that runs somewhere else reaches the sidecar over the network, with
a connection file the sidecar's owner made with `orvud agent add alice
--address 10.0.0.5:7441 --connection alice.orvu --local`, and a sidecar
started with `--network 0.0.0.0:7441`. The agent connects with
`Team.connect("default", { connection: "alice.orvu" })`, or with the path in
`ORVU_CONNECT`, and needs nothing else. The connection is encrypted, and
trusts only that sidecar. Agents on Windows connect this way: `npm install
orvu` there brings the library and no `orvud`, as there is none for Windows
yet.

## The eleven kinds of object

| Method on `Client` | What it is | Its methods |
|---|---|---|
| `counter(name)` | A number you add to and subtract from. It may go below zero | `increment`, `decrement`, `value` |
| `boundedCounter(name)` | A number that can never go below zero, such as a budget | `increment`, `tryDecrement`, `transfer`, `value`, `allowance` |
| `register(name)` | One text value. The later write wins | `set`, `get` |
| `set(name)` | A set of text items | `add`, `remove`, `contains`, `elements` |
| `map(name)` | Keys with a text value each | `put`, `remove`, `get`, `keys`, `entries` |
| `mvMap(name)` | Keys with text values. Writes made at the same time are all kept, until a later write replaces them | `put`, `remove`, `get`, `keys`, `entries` |
| `cart(name)` | Items with a quantity each | `add`, `removeQty`, `removeItem`, `quantity`, `items` |
| `list(name)` | An ordered list of text items | `insert`, `delete`, `get`, `length`, `toArray` |
| `json(name)` | A JSON document | `set`, `remove`, `insert`, `get`, `toValue` |
| `richText(name)` | Text with formatting | `insert`, `delete`, `format`, `unformat`, `text`, `toSpans` |
| `tree(name)` | Items nested under other items | `moveNode`, `moveNodeAt`, `setLabel`, `parent`, `children`, `toNodes` |

Every handle also has `watch()`.

## Things to know

- **Making a handle sends nothing.** An object comes into being with the
  first write to it. Reading one that does not exist gives an empty value:
  0, `null`, an empty list.
- **An object's name decides what it is.** Using `"visits"` as a counter and
  then as a list throws `TypeMismatch`.
- **Being told no is not an error.** `tryDecrement` and `transfer` give
  `false` when there is not enough, and nothing changes.
- **A mistake in an argument is a `BadRequest`,** such as a negative count or
  a value JSON cannot hold. Nothing is sent. From a batch it is thrown at
  once, and the batch is not sent.
- **Numbers are JavaScript numbers.** Counts and positions have to be whole
  numbers from 0 to `Number.MAX_SAFE_INTEGER`. A counter read back that is
  larger than that throws `TooLarge`, so that no count is ever quietly wrong.
  A machine number can be larger, so `replicaId` is a `bigint`, and
  `transfer` takes a `bigint` or a number.
- **Size limits.** The sidecar does not take one request over 64 MiB, or
  over 4 MiB from a program that reaches it over the network. A
  reply may be up to 64 MiB, which `connect(..., { maxReplyBytes })` changes.
  Past either limit the call throws `TooLarge`. In a batch, operations are no
  longer run once the replies have passed 4 MiB.
- **Live updates have limits.** One client can have 256 `watch()` loops open
  at once, and a sidecar 4,096 over all the programs it serves, near and over
  the network. Starting one more throws `TooLarge`. A loop that has ended
  gives its place back.
- **A number in a JSON document is a 64-bit float,** for every program that
  uses it, as a JavaScript number is, so every JavaScript number is kept
  exactly. A whole number is sent with all its digits, also where
  `JSON.stringify` would round its digits, as it does for `2 ** 60`, and
  `-0` is sent as negative zero, where `JSON.stringify` would send `0`. From
  Python, a whole number a float cannot hold, such as `2**53 + 1`, throws
  `JsonError` and changes nothing.
- **A hole in a list, as in `[, 1]`, is sent as `null`,** as
  `JSON.stringify` sends it.
- **Replacing or removing something in a JSON document starts it afresh.** A
  change made at the same time inside what was replaced is dropped, even if
  it came later. What was replaced never comes back. Two programs that write
  into a field that was empty, removed or replaced, starting from the same
  state, both keep their writes.
- **Only what JSON holds exactly can be written to a JSON document:** text,
  finite numbers, `true`, `false`, `null`, arrays and plain objects. `NaN`, a
  property that is `undefined`, a `Date` or a `Map` throws `BadRequest`,
  where `JSON.stringify` would change it without a word.
- **Positions in rich text count characters,** as `[...text].length` does,
  which is how `orvud` counts them. A JavaScript string's own `length` counts
  an emoji and some other characters as two.
- **Class names are not JavaScript's own.** The set, map and list handles are
  `TextSet`, `TextMap` and `TextList`, so that they do not hide `Set`, `Map`
  and `Array`.
- **A batch is not all or nothing.** Each operation has its own result, and
  one failing does not undo the others. A request that is itself wrong stops
  the whole batch before anything runs.
- **A change whose answer did not come is sent again, and made once.** Each
  change, and each batch with a change in it, is sent with an id of its own,
  and with the same id each time it is sent. If no answer comes, because
  `orvud` took longer than `timeoutMs` or stopped partway, the client sends
  it again, up to three more times, once `orvud` is back. `orvud` gives a
  change it already made the answer it gave it, and does not make it again.
  Only if all that fails does the change throw `TimedOut` or
  `SidecarUnavailable`, and then it may have been made. `orvud` remembers
  ids for ten minutes; see its README.
- **Running out of time is not a "no".** A read that `orvud` does not answer
  within `timeoutMs` throws `TimedOut`, and is not sent again.
- **The client carries on after `orvud` restarts.** It opens a new session
  by itself. A client from `connect()` with no socket also starts `orvud`
  again if it has stopped; one given a socket waits up to `timeoutMs` for it
  to come back before it sends a change again. A read that was on its way
  when `orvud` stopped is not sent again: it throws `SidecarUnavailable`, and
  the next call goes through. A call that `orvud` turned away because it was
  stopping did not run, so that one is made again, once the old `orvud` has
  gone and a new one has started. Should that one be stopping too by the
  time the call gets there, the same is done again, for as long as
  `timeoutMs`. A change is sent again with the id it was first sent with,
  however it came to be sent again.
- **A `watch()` loop carries on when its session is dropped.** A busy
  `orvud` keeps 1,024 sessions, and when they are all taken it drops the one
  used longest ago. An agent with a key that opens more than 256 drops its
  own session used longest ago instead. A call counts as using a session, and
  so does each value a watch sends, so a watch whose object keeps changing
  keeps its place and a quiet one can be dropped. The loop then opens a new
  session and watches again, as often as that happens. It gives up, with
  `EngineError`, if its session is dropped twice within 10 seconds: the new
  session it opened is dropped too before 10 seconds have passed. That
  happens when many other sessions on `orvud` are busier than the loop's and
  new ones keep opening: a quiet loop's new session can become the one used
  longest ago within a second. Trying again would then go on for ever. The
  answer is fewer sessions, since the limit of 1,024 is fixed.
- **Give each program its own agent and key.** One agent may hold at most
  256 sessions on the `orvud` at once, and each client holds one. When more
  than 256 programs use the same key at the same time, each one that opens a
  session drops another's, that one opens a new session and drops the next,
  and so they keep dropping each other's sessions. So give each program, or
  each small group of them, an agent and key of its own.
- **A note two agents wrote at the same time keeps both values.** When
  neither had seen the other's, `get` throws `Clash`, whose `values` and
  `authors` say what each wrote, and `all` and `watch` give the note as that
  `Clash`. The next `put` settles it for everyone. Two agents that put the
  same value read as one. Notes kept the old way, in a plain map, are moved
  over by `orvud` the first time a team's notes are used.
- **How a simple note is kept.** The notes are the map
  `orvu/<team>/notes-mv`, which keeps values written at once. Each value is
  the writer's name, the character `\x1f`, and then the note: text as it is,
  or anything else as the character `\x1e` and its JSON form, such as
  `"alice\x1f\x1e{\"day\":\"Friday\"}"`. A text that itself starts with
  `\x1e` is kept in the JSON form too, so that it reads back as it was. A
  program that gave no name writes an empty one. The Python package keeps
  notes the same way, the sidecar's search reads them so, and a program in
  another language that uses the map directly has to as well.
- **Names on writes are taken on trust from an agent without rules.** A
  note's `author`, `spentBy`, `toppedUpBy`, `writtenBy`, `updatedBy`, a
  task's `createdBy` and a message's `sender` say what the writing program
  called itself. For an agent with rules, the sidecar checks that a note's
  author, its lines in `spentBy` and `toppedUpBy`, a task's `createdBy` and
  a message's `sender` are its own name, and records a task's `updatedBy`
  itself, as the agent that last changed anything in the task; `orvud audit`
  shows which agent really made every change. A structured note and a
  tally's total keep no writer's name: `orvud audit` shows who changed them.
- **A claim is soft, unless the task is firm.** Two agents that claim the
  same task at the very same moment, or on two machines that cannot reach
  each other, can both be told `true`. A task added with `{ firm: true }`
  has one ticket, and only the agent that takes it gets the task. An agent
  whose machine cannot reach the machine holding the ticket is told `false`,
  after waiting up to two seconds. Only the agent that has a firm task can
  finish it.
- **Firm means one holder at a time.** It does not stop work from being
  done twice by an agent that has lost its task without knowing. An agent
  that went quiet, or whose machine is cut off, hears that its task was
  taken over only when the news reaches its machine, and until then
  `stillWorking` gives it `true`. What does stop it is a number. Each
  holder of a firm task holds a numbered ticket, and the number goes up each
  time the task moves on. `claimTicket` claims the task and gives the
  number. A step that must not happen twice should carry it to whatever it
  changes, and that side should keep the highest number it has seen and
  refuse a lower one:

  ```typescript
  const ticket = await team.tasks.claimTicket(task);
  if (ticket !== null) {
    const invoice = await writeInvoice();
    // The billing system refuses a ticket lower than the highest it has
    // seen for this task, so an agent that lost the task cannot send it.
    await billing.send(invoice, { task: task.id, ticket });
  }
  ```

  That refusal has to be made by the side that is changed: nothing in orvu
  can make it for it. `stillHolding(task, ticket)` tells whether the number
  is still this agent's as far as its machine knows, which is a good check
  before starting a step, not a promise.
- **A claim runs out.** It lasts a quarter of an hour after the agent last
  called `stillWorking`, or as many seconds as `claim(task, me, { lease })`
  says. Another agent's `claim` then takes the task over, and the agent that
  went quiet gets `false` from its next `stillWorking`. A takeover is
  written on the task itself, as `task.takenOver`, `{ by, from, at }`, so
  every agent that reads or watches the board sees who took the task from
  whom and when. It keeps the last takeover, also after the task is given
  back or finished.
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
  names and notes that are not text as `null`, and a time added that is not
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
  back with `update(task, { state: "todo" })` then throws `TicketUnavailable`
  every time, because a ticket that was taken cannot be told apart from one
  on a machine that cannot be reached. `update(task, { state: "todo", force:
  true })` sets it back all the same, on a fresh pair of tickets with a
  higher number, so no ticket from before can win it back. The number
  jumps by a random two to four thousand million, past every pair the task
  has had. A second operator forcing after the first, or one trying again,
  never hands one number to two agents. Two operators forcing at the very
  same moment, or on machines cut off from each other, land on the same
  number only by a chance of about one in a thousand million. A ticket
  found made twice anyway is never taken: claiming the task throws
  `TicketUnavailable`, which says to set it back. `update(task, { state: "todo" })` is
  enough for a doubled ticket; a doubled ticket for moving the task on
  needs `force: true`. It needs
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
- **A finished task stays finished.** `finish` gives `false` for a task that
  is already done, and a claim that arrives late cannot bring it back or
  leave its name on it. When an agent runs as several programs under one
  name, a late claim from one leaves a claim another of them made since as
  it is. The result and the name of the agent that finished
  the task are written as one. Of two agents that finish a task at the same
  moment, the one whose result was not kept is told `false`. Both can still
  be told `true` on two machines that cannot reach each other, or when one
  has already read back its own before the other's write arrives; the later
  result is then kept.
- **A task being added is not listed until it is whole.** It is put in the
  tree of sub-tasks and then on the board. Another machine can hear of the
  two in either order, so `list` and `watch` leave a task out until it is in
  the tree, and it never shows at the top for a moment before it shows under
  its parent. They wait only half a minute after the task was added, by the
  clock of the machine that added it (`team.tasks.grace`). A task that has
  no place in the tree by then, because the program adding it stopped half
  way, is shown at the top. `watch` shows it when that time is over, also if
  nothing else changed.
- **An inbox gives each message once,** also a message that a machine cut
  off for long wrote late to a page the inbox had moved past. Each page
  counts the messages written to it, and an agent with a name keeps a mark
  of how many it was given from each page; a page whose count is above the
  mark, or whose count no sender could have written (below zero or far too
  big, maybe stuck at the counter's limit), is read again by `unread` until
  the mark catches up. The counts are only hints. Such a page is read at
  once, then, while nothing new turns up there, after 1 second, 2, 4 and so
  on up to a minute (`inbox.wait`, `inbox.longestWait`), and from the start
  again when its count changes. At most 8 such pages are read on each look
  (`inbox.behind`), those read longest ago first, so none is passed over for
  good. `watch` follows up to 8 of them, to see at once when a message lands
  there, and wakes by itself when a wait is over. So a late message is
  always given, also when its count arrives before the message itself:
  within about a minute at worst, and at once when `watch` follows its
  page. That holds for a program that keeps looking, through `watch` or
  regular calls to `unread`. With more than 8 such pages waiting, a program
  that calls `unread` only now and then needs several calls to reach them
  all, about one call for each 8 pages. Anyone who may send can add to an old page's count without writing
  there; a count that then stays as it is costs at most one more read of
  that page a minute. A program with no name
  remembers only while it runs, keeps in memory only the pages it still
  looks at, and does not look back at pages it has finished. The record of
  what an agent was given from a page is not read or added to once the page
  is finished, and is kept to tell a late message apart. `watch` looks at
  page numbers, page lengths and counts to see a change, and reads messages
  only then.
- **The roster's "still here" does not keep a program running.** A program
  that has nothing else to do ends, and its agent fades from the roster.
  What it left is cleared the next time a program joins under the same
  name, and an agent not heard from for more than a day is not listed at
  all.
- **A `Team` is closed with `close()`,** or with `await using`. It takes the
  agent off the roster and closes the connection.
- **Close a client with `close()`,** or with `await using`. Closing ends the
  `watch()` loops it has open, and every call after it throws `BadRequest`.
- **Calls made at the same moment are fine.** When several find that `orvud`
  restarted or stopped, they open one new session, and start one `orvud`,
  between them.
- **`orvud` is started for you and stops by itself,** five minutes after the
  last program has disconnected. `ORVU_IDLE_SECONDS`, in the environment of
  the program that starts it, sets another time, in seconds. What it prints
  goes to `orvud.log` in its data folder. When one is started and the log has
  grown past 1 MiB, it is moved to `orvud.log.1` first, in place of the one
  there. Only the program holding the lock on it moves it, whatever
  language it is in, so that two starting at once do not lose the long log.
  The lock is the highest-numbered of the files `orvud.log.lock.1`, `.2`, and
  so on: a program takes it by making the next number, which only one
  program can do, and gives it back by making the one after that, saying
  `free`. A lock older than 30 seconds was left behind and is taken over the
  same way.
  A program stopped for more than 30 seconds while holding the lock, paused
  or put to sleep, may lose it that way. It looks again just before moving
  the log, and leaves it if the lock is no longer its own. That makes the
  gap small but does not close it: a program stopped for more than 30
  seconds between looking and moving can still move the log while the lock
  is no longer its own.
  No program ever removes or changes the highest-numbered file, so
  none can take away a lock another has just taken; the ones below it are
  cleared away by the next program to take the lock. A plain
  `orvud.log.lock` left by an earlier release is not looked at, and can be
  deleted.
  `ORVU_IDLE_SECONDS` can be at most 9007199254740991.
- **To use an `orvud` you run yourself,** give `connect` its socket path, or
  set `ORVU_SOCKET` to it in the program's environment. That is how a program
  reaches the `orvud` of a team on several machines: `orvud setup` makes the
  settings for one on each machine, and it is started by hand, or by systemd,
  with `orvud --config` (see the `orvud` README). The package never starts
  such an `orvud`: if nothing answers on the socket, `connect` throws
  `SidecarUnavailable`, and says so. `ORVU_SOCKET` and `ORVU_CONNECT` cannot
  both be set.
- **A Python program and a Node program share one `orvud`.** Whichever starts
  first, the other uses it. They have to be the same release: a sidecar of
  another release throws `VersionMismatch`, which names the process to stop.
- **A data folder copied from another computer is not started.** The sidecar
  this package starts refuses a data folder that was last used on another
  computer, as two computers must never run as one machine. `connect` then
  throws `SidecarUnavailable` with the sidecar's own message, which gives
  the command to run once by hand, with the folder in it:
  `orvud --local --data-dir <folder> --moved` if the folder was moved here,
  or `--new-number` if the other computer still uses its copy.
- **A connect that gives up on an `orvud` that holds its socket but does not
  answer keeps one connection open until that `orvud` ends.** That happens
  only in an `orvud`'s last moment, and the program can only end once that
  `orvud` has.

## Working on this package

```text
cargo build -p orvud
cargo build -p orvu-transport --example keygen
cd typescript
npm install
npm run check
npm test
```

The tests need Node 22.18 or later, which runs the `.ts` files as they are.
The package itself runs on Node 20. The tests start their own `orvud`, from
`target/debug/orvud`, with a home folder of their own, so they never touch
your real data. Some tests also use the Python package, from `python/.venv`.
Each sidecar a test starts gets a folder of its own in `/tmp`, or in the
folder `ORVU_TEST_DIR` names; a socket's path can hold at most 107 bytes, so
that folder's path should be short. The tests that link machines also listen
on network ports of this computer, on `127.0.0.1`, and cannot run where that
is not allowed.

`scripts/build-package.sh` builds the packages for release, in `release/`:
the library, `orvuhq-orvu-0.1.0.tgz`, and the sidecar package for the kind of
machine it runs on, such as `orvuhq-orvud-linux-x64-0.1.0.tgz`. A release
needs the sidecar package built on each of the four kinds of machine, and
the library once. Only the library that is built names the sidecar packages,
as optional, at its own version: the `package.json` here does not, so that
`npm install` and `npm ci` here never ask for them.

`scripts/clean-test.sh` installs the two files into an empty folder, with an
empty home folder, and runs what a new user would: the quickstart twice, a
team, and a team on two machines. It also installs the library alone, and
checks that it says which sidecar package to install. Until the sidecar
packages are published, npm looks for them online, finds none, and skips
them, as it does any optional package it cannot get; the file given for this
machine stands in for its package. Run it in a container too, to test it on
Node 20:

```text
sh typescript/scripts/clean-test.sh typescript/release/orvuhq-orvu-0.1.0.tgz \
    typescript/release/orvuhq-orvud-linux-x64-0.1.0.tgz
docker run --rm -v "$PWD/typescript":/src:ro node:20-slim \
    sh /src/scripts/clean-test.sh /src/release/orvuhq-orvu-0.1.0.tgz \
    /src/release/orvuhq-orvud-linux-x64-0.1.0.tgz
```

`scripts/build-package.sh --main-only` builds only the library, and needs no
Rust. `scripts/clean-test-network.sh` installs it into an empty folder, with
an empty home folder and npm told it is on Windows, so that no sidecar
package comes with it. It connects the library to the sidecar built from
this repository over the network with nothing but a connection file, and,
with Node told it is on Windows, checks that asking for a sidecar of its own,
or running the `orvud` command, says to connect over the network. It has not
been tried on Windows itself.

```text
sh typescript/scripts/build-package.sh --main-only
sh typescript/scripts/clean-test-network.sh typescript/release/orvuhq-orvu-0.1.0.tgz
```

The message definitions are copied from `crates/orvud/proto/orvu/v1/dataplane.proto` into
`proto/` here, because the package is used outside this repository. A test
checks that the copy is the same as the original. After changing the
original, copy it again.
