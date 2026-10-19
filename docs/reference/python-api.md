# Python API overview

A map of the `orvu` package. [`python/README.md`](../../python/README.md) is
the full reference, with every rule and limit, and each method has a
docstring: `help(orvu.Team)` shows them.

## Connecting

| Call | What it gives |
|---|---|
| `Team.connect(name="default", *, me=None, socket=None, **options)` | A `Team`. Use it with `with`, or call `close()` |
| `orvu.connect(socket=None, *, connection=None, namespace="", key=None, timeout=10.0, max_reply_bytes=...)` | A `Client`, for plain objects. Use it with `with`, or `async with` |

With no socket, `connection` or `ORVU_SOCKET`/`ORVU_CONNECT`, the local
sidecar is used, and started if it is not running. `Team.connect` passes
`connection`, `key`, `timeout` and the rest on to `orvu.connect`.

## A team

| On `team` | Methods |
|---|---|
| `team.me` | The agent's name |
| `team.notes` | `put`, `get`, `remove`, `all`, `author`, `authors`, `watch`, `structured(name)`, `structured_names` |
| `team.tasks` | `add`, `list`, `get`, `update`, `claim`, `claim_ticket`, `still_working`, `still_holding`, `release`, `finish`, `move`, `children`, `watch` |
| `team.budget(name)` | `top_up`, `spend`, `left`, `spent_by`, `topped_up_by`, `watch` |
| `team.document(name)` | `append`, `insert`, `delete`, `text`, `written_by`, `format`, `unformat`, `to_spans`, `watch` |
| `team.documents()` | The names of the documents |
| `team.inbox` | `send(to, body)`, `send_all(body)`, `unread`, `messages`, `history`, `watch` |
| `team.tally(name)` | `add`, `total`, `by_name`, `watch` |
| `team.tallies()` | The names of the tallies |
| `team.roster` | `join`, `leave`, `here`, `members`, `watch` |
| `team.search(query, limit=5)` | A list of `Found`: `kind`, `name`, `text`, `score`, and `paragraph` or `title` |
| `team.index()` | Every object the team's blocks have made, by short name and kind |
| `team.client` | The `Client` underneath |

Values read back: `Task` (`id`, `title`, `state`, `claimed_by`, `result`,
`notes`, `parent`, `created`, `created_by`, `finished_by`, `updated_by`,
`firm`, `ticket`, `seen`, `lease`, `quiet_since`, `taken_over`, and
`abandoned()`, which says whether its claim has run out), `Message` (`id`, `sender`,
`to`, `body`, `sent`), `Member`, `Found`.

## Plain objects

`client.<kind>(name)` gives a handle. Making one sends nothing; an object is
created by the first write, and reading one that does not exist gives an
empty value.

| Method on `Client` | Kind | Its methods |
|---|---|---|
| `counter` | A number to add to and subtract from | `increment`, `decrement`, `value` |
| `bounded_counter` | A number that never goes below zero | `increment`, `try_decrement`, `transfer`, `value`, `allowance` |
| `register` | One value; the later write wins | `set`, `get` |
| `set` | Unique text items | `add`, `remove`, `contains`, `elements` |
| `map` | Keys with a text value | `put`, `remove`, `get`, `keys`, `entries` |
| `mv_map` | Keys with text; writes at once are all kept | `put`, `remove`, `get`, `keys`, `entries` |
| `cart` | Items with a quantity | `add`, `remove_qty`, `remove_item`, `quantity`, `items` |
| `list` | Text items in order | `insert`, `delete`, `get`, `len`, `to_list` |
| `json` | A JSON document | `set`, `remove`, `insert`, `get`, `to_value` |
| `rich_text` | Text with formatting | `insert`, `delete`, `format`, `unformat`, `text`, `to_spans` |
| `tree` | Items nested under items | `move_node`, `move_node_at`, `set_label`, `parent`, `children`, `to_nodes` |

Every handle has `watch()` and, for `async` code, `awatch()`.
`client.batch()` sends several operations in one message; it is not all or
nothing. `client.replica_id` is this machine's number.

## Errors

All are under `orvu.OrvuError`: `SidecarUnavailable`, `VersionMismatch`,
`TimedOut`, `BadRequest`, `TypeMismatch`, `ClassMismatch`, `OutOfBounds`,
`JsonError`, `TooLarge`, `EngineError`, `SearchUnavailable`,
`TicketUnavailable`, `NotAllowed`, `UnknownTask`, `Clash`. What each means is
in [Troubleshoot](../how-to/troubleshooting.md#a-call-fails).

A refusal is not an error: `spend`, `try_decrement`, `transfer` and `claim`
return `False`.

## Commands

| Command | What it does |
|---|---|
| `orvu mcp` | The MCP server: see [MCP reference](mcp.md) |
| `orvud` | The sidecar: see [`orvud` reference](orvud.md) |
| `python -m orvu.demo` | Three agents share twelve tasks and a budget too small for all of them |
| `python -m orvu.demo_machines` | The same on three machines, one switched off in the middle |
| `python -m orvu.measure` | Measures speed on this computer |
