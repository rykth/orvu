# Use the MCP server

`orvu mcp` gives an agent program the team as tools. It comes with the
Python package. The first setup is in
[Your first MCP agent](../getting-started/first-mcp-agent.md), and the full
guide is [`python/MCP.md`](../../python/MCP.md).

## Over stdio

This is the usual way: the agent program starts the server itself and talks
to it over its input and output.

```text
orvu mcp --me researcher-1
```

Useful options:

| Option | Use it to |
|---|---|
| `--team NAME` | Use a team other than `default` |
| `--tools notes,tasks` | Offer only some groups of tools |
| `--allow-top-up` | Let this agent add to budgets |
| `--socket PATH` | Use a team's sidecar from `orvud setup` |

All options are in the [MCP reference](../reference/mcp.md).

Agents started this way must run as the same user on the same machine as
the sidecar. If you moved the data folder with `XDG_DATA_HOME`, give the
server the same setting in the agent program's settings, or it will use a
separate sidecar with separate data.

## Over HTTP, with a token

Use HTTP when the agent's program cannot start the server itself: it runs on
another machine, in a container, or as another user.

```text
pip install "$(ls dist/orvu-*.whl)[http]"   # once published: pip install 'orvu[http]'
head -c 32 /dev/urandom | base64 > ~/.orvu-token
chmod 600 ~/.orvu-token
orvu mcp --me researcher-1 --http 127.0.0.1:8765 --token-file ~/.orvu-token
```

The program connects to `http://127.0.0.1:8765/mcp` (MCP's streamable HTTP)
and sends `Authorization: Bearer <token>` with each request. A request
without it is refused before anything else is looked at.

- **The server is one agent.** Everyone with the token acts as
  `researcher-1`. For two agents, start two servers, each with its own name,
  port and token.
- **It listens on this machine only** at `127.0.0.1`. Any other address, such
  as `0.0.0.0`, needs `--token-file`.
- **The token file must be yours alone:** a plain file you own, `chmod 600`,
  at least 16 characters with no spaces.
- **`--no-token`** serves without a token, on this machine only, and then
  answers only requests that name it as `127.0.0.1`, `localhost` or `[::1]`.
- **It is not encrypted.** Over a network you do not trust, put it behind
  something that provides TLS, or reach it through an SSH tunnel.
- Up to 100 sessions are open at once. A session left unused for half an
  hour is ended.

## With a key

Make a key with `orvud agent add researcher-1 --local`, then give it to the
server and leave `--me` out:

```text
claude mcp add orvu --env ORVU_KEY=orvu-key-... -- orvu mcp
```

In a settings file, the key goes in an `"env"` entry next to `"args"`. Give
the agent rules with `orvud agent allow` to limit what its tools may do.

## For an agent on another machine

Either serve it over HTTP, as above, or give it a connection file and set
`ORVU_CONNECT`: see
[Serve agents on other machines and in containers](remote-agents.md).

## Being told about changes

The server offers `orvu://inbox`, `orvu://tasks` and `orvu://notes` as
resources. A program that subscribes is told when one changes. Many programs
do not pass that on to the model, so the server also tells the agent, in its
opening words, to check its inbox now and then with `inbox_read`.
