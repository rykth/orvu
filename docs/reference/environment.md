# Environment variables

## Read by the Python and TypeScript libraries

| Variable | What it does |
|---|---|
| `ORVU_SOCKET` | The socket of a sidecar you started yourself, such as a team's sidecar from `orvud setup`. The library then never starts one |
| `ORVU_CONNECT` | A connection file, for a sidecar elsewhere over the network. Cannot be set together with `ORVU_SOCKET` |
| `ORVU_KEY` | The agent's key, from `orvud agent add` |
| `ORVU_IDLE_SECONDS` | For a sidecar the library starts: how many seconds it waits after the last program has gone before it stops. Default 300 |
| `ORVUD_BIN` | Which `orvud` program to start, in place of the one inside the package |
| `XDG_DATA_HOME` | Moves the data folder to `$XDG_DATA_HOME/orvu`, on Linux |
| `XDG_RUNTIME_DIR` | Where the socket goes by default, as `orvud.sock` |

Arguments given in code (`socket=`, `connection=`, `key=` in Python;
`{ socket, connection, key }` in TypeScript) are used in place of the
variables.

## Read by the sidecar

| Variable | What it does |
|---|---|
| `ORVU_MODEL` | A folder holding the search model's three files. Nothing is fetched or checked. The same as `--model` |
| `ORVU_MODEL_SOURCE` | Another place to fetch the model from, such as a copy inside a network with no internet. Each file is still checked |

## Read by `orvu mcp`

The MCP server reads `ORVU_KEY`, `ORVU_SOCKET` and `ORVU_CONNECT`, as the
library does. Agent programs start it with few of your terminal's settings,
so put the ones it needs in the agent program's settings.
