# TypeScript API overview

A map of the `orvu` package for Node. [`typescript/README.md`](../../typescript/README.md)
is the full reference, with every rule and limit.

## Connecting

| Call | What it gives |
|---|---|
| `await Team.connect(name = "default", { me, socket, key, connection, namespace, ... })` | A `Team`. Close it with `close()`, or `await using` |
| `await connect(socket?, { key, connection, namespace, timeoutMs, maxReplyBytes, ... })` | A `Client`, for plain objects. Close it with `close()`, or `await using` |

With no socket, connection file or `ORVU_SOCKET`/`ORVU_CONNECT`, the local
sidecar is used, and started if it is not running. `connect()` looks for the
`orvud` program in `$ORVUD_BIN`, then inside the package, then on the `PATH`.

## A team

| On `team` | Methods |
|---|---|
| `notes` | `put`, `get`, `remove`, `all`, `author`, `authors`, `structured`, `structuredNames`, `watch` |
| `tasks` | `add`, `list`, `get`, `update`, `claim`, `claimTicket`, `stillWorking`, `stillHolding`, `release`, `finish`, `move`, `children`, `watch` |
| `budget(name)` | `topUp`, `spend`, `left`, `spentBy`, `toppedUpBy`, `watch` |
| `document(name)` | What a rich text has, and `append`, `writtenBy` |
| `inbox` | `send`, `sendAll`, `unread`, `messages`, `history`, `watch` |
| `tally(name)` | `add`, `total`, `byName`, `watch` |
| `roster` | `join`, `leave`, `members`, `here`, `watch` |
| `search(query, limit)` | A list of `Found`: `kind`, `name`, `text`, `score`, and `paragraph` or `title`. 5 unless `limit` says, up to 50 |

Whether a claim has run out is asked with the function `abandoned(task, now?)`,
exported from the package next to `Team`; it is not a method on `tasks`.

Every `watch()` is an async iterator and takes `{ signal }`, an
`AbortSignal` that ends the loop.

## Plain objects

| Method on `Client` | Handle | Its methods |
|---|---|---|
| `counter(name)` | `Counter` | `increment`, `decrement`, `value` |
| `boundedCounter(name)` | `BoundedCounter` | `increment`, `tryDecrement`, `transfer`, `value`, `allowance` |
| `register(name)` | `Register` | `set`, `get` |
| `set(name)` | `TextSet` | `add`, `remove`, `contains`, `elements` |
| `map(name)` | `TextMap` | `put`, `remove`, `get`, `keys`, `entries` |
| `mvMap(name)` | `MvMap` | `put`, `remove`, `get`, `keys`, `entries` |
| `cart(name)` | `Cart` | `add`, `removeQty`, `removeItem`, `quantity`, `items` |
| `list(name)` | `TextList` | `insert`, `delete`, `get`, `length`, `toArray` |
| `json(name)` | `Json` | `set`, `remove`, `insert`, `get`, `toValue` |
| `richText(name)` | `RichText` | `insert`, `delete`, `format`, `unformat`, `text`, `toSpans` |
| `tree(name)` | `Tree` | `moveNode`, `moveNodeAt`, `setLabel`, `parent`, `children`, `toNodes` |

Every handle also has `watch()`. `client.batch()` gives a `Batch`; each
method on a batch handle gives a `Pending` whose `value` is there once
`await batch.send()` has returned.

## Numbers

Positions are whole numbers from 0 to `Number.MAX_SAFE_INTEGER`, and counts
are whole numbers in the safe range either side of 0 (a counter can go below 0
with `decrement`). A number outside the safe range throws `TooLarge` rather
than come back wrong. A
machine number can be larger, so `replicaId` is a `bigint`.

## Errors

The same names as in Python: `OrvuError` and, under it,
`SidecarUnavailable`, `VersionMismatch`, `TimedOut`, `BadRequest`,
`TypeMismatch`, `OutOfBounds`, `JsonError`, `TooLarge`, `EngineError`,
`TicketUnavailable`, `NotAllowed`, `Clash` and the rest. See
[Troubleshoot](../how-to/troubleshooting.md#a-call-fails).

## The `orvud` command

The package gives `orvud` as a command. Run it with
`npx --no-install orvud ...`, as in `npx --no-install orvud agent add alice --local`.
