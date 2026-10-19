# MCP tools, resources and prompts

`orvu mcp` is an MCP server that gives an agent a team as tools.
[`python/MCP.md`](../../python/MCP.md) is the full guide, with setup for
each agent program and every detail of how the tools behave.

## Options

| Option | What it does |
|---|---|
| `--me NAME` | The agent's name on the team. Without it, a name is made from the team and the folder the server starts in. With a key it can be left out, and if given must be the key's name. It cannot be "everyone" |
| `--team NAME` | The team. Default `default` |
| `--allow-top-up` | Also offers `budget_top_up` |
| `--tools GROUPS` | Offers only these groups: `notes`, `tasks`, `budgets`, `inbox`, `documents`, `search`, `tallies`, `roster`, `objects`. `whoami` is always offered |
| `--http HOST:PORT` | Serves MCP's streamable HTTP at `/mcp` instead of stdio. Needs `pip install 'orvu[http]'` |
| `--token-file PATH` | With `--http`: a file only you can read, holding the token programs must send as `Authorization: Bearer <token>` |
| `--no-token` | With `--http` on this machine only: no token |
| `--socket PATH` | Uses the sidecar at that socket |

It also reads `ORVU_KEY` and `ORVU_CONNECT` from its environment.

## Tools

| Group | Tools |
|---|---|
| (always) | `whoami` |
| `notes` | `note_put`, `note_get`, `note_author`, `note_list`, `note_remove`, `note_structured_get`, `note_structured_set`, `note_structured_remove`, `note_structured_list` |
| `tasks` | `task_add`, `task_list`, `task_get`, `task_claim`, `task_still_working`, `task_release`, `task_finish`, `task_update`, `task_move` |
| `budgets` | `budget_list`, `budget_left`, `budget_spend`, `budget_spent`, and `budget_top_up` with `--allow-top-up` |
| `inbox` | `inbox_send`, `inbox_read`, `inbox_recent` |
| `documents` | `doc_list`, `doc_read`, `doc_append`, `doc_insert`, `doc_remove` |
| `search` | `search` |
| `tallies` | `tally_list`, `tally_read`, `tally_add` |
| `roster` | `roster_list` |
| `objects` | `object_read`, `object_counter_add`, `object_register_set`, `object_set_add`, `object_set_remove`, `object_map_put`, `object_map_remove`, `object_list_insert`, `object_list_remove`, `object_json_set`, `object_json_remove` |

What each tool does is in the table in [`python/MCP.md`](../../python/MCP.md#the-tools).

Every answer is also given as MCP structured content, the same JSON as the
text. A tool used wrongly answers with what was wrong, and changes nothing.
`note_list` and `task_list` give up to 100 at a time and say what to pass as
`after` to read on.

## Resources

| Resource | What it holds |
|---|---|
| `orvu://inbox` | The recent messages to the agent and to everyone. Reading it does not count as being given them |
| `orvu://tasks` | Every task on the board |
| `orvu://notes` | Every note |

Each is JSON. A program that subscribes is sent
`notifications/resources/updated` when it changes. A resource is offered
only with its group of tools.

## Prompts

| Prompt | What it asks the agent to do |
|---|---|
| `check_inbox` | Read its messages and act on them |
| `pick_a_task` | Take a free task and start on it |

## Protocol versions

The server speaks the older way of connecting, with a hello, in versions
`2024-11-05` to `2025-11-25`, and the newer one, `2026-07-28`, where each
request says its version. Resources can be subscribed to only the older way.
