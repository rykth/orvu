# Your first MCP agent

An agent program that speaks MCP, the Model Context Protocol, such as Claude
Code, Claude Desktop, Cursor or VS Code, can use a team with no code written
for it. You give the program one command, and the agent gets tools for the
team's notes, tasks, budgets, messages, documents, tallies, roster, search
and plain shared objects.

You need the Python package ([how to install it](install.md)). It brings the `orvu`
command.

## 1. Tell your agent program about it

The command is:

```text
orvu mcp --me researcher-1
```

`--me` is the agent's name on the team. Give each agent its own.

**Claude Code.** Run this once, in the project's folder:

```text
claude mcp add orvu -- orvu mcp --me researcher-1
```

**Claude Desktop, Cursor and most others** keep a settings file with a list
of servers. Add this to it:

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

**VS Code** uses `.vscode/mcp.json`, where the list is called `servers` and
each entry says `"type": "stdio"`.

If the program cannot find `orvu`, give it the whole path, which
`which orvu` prints.

## 2. Add a second agent

Set up a second agent the same way, with `--me researcher-2`. Ask one to add
a task and the other to list the tasks, or one to send the other a message.
Both see the same board.

## 3. Mix code and agents

A Python or Node program that uses `Team.connect()` on the same team sees
the same data as the MCP agents. For example, top up the budget the agents
spend from:

```python
from orvu import Team

with Team.connect(me="operator") as team:
    team.budget("tokens").top_up(10_000)
```

An MCP agent can spend from a budget but not add to it, unless its server
was started with `--allow-top-up`.

## Next

- [Use the MCP server](../how-to/mcp-server.md): HTTP with a token, keys,
  agents elsewhere.
- [MCP tools, resources and prompts](../reference/mcp.md)
- [`python/MCP.md`](../../python/MCP.md), the full guide.
