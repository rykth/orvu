# Serve agents on other machines and in containers

An agent that cannot open the sidecar's socket, because it runs on another
computer, in a container, or on Windows, reaches the sidecar over the
network. It needs only a connection file. It runs no sidecar of its own.

## 1. Make the agent and its connection file

On the sidecar's machine:

```text
orvud agent add alice --address 10.0.0.5:7441 --connection alice.orvu --local
```

`--address` is where the agent will reach the sidecar. `alice.orvu` holds
everything alice needs: that address, alice's key, and the sidecar's
certificate. Keep it as secret as a key.

## 2. Start the sidecar listening on the network

```text
orvud --local --network 0.0.0.0:7441
```

For a sidecar started from settings, add `--network` to its usual command.
Over the network, only agents with keys are let in, and the sidecar will not
start with `--network` until there is at least one.

## 3. Connect the agent

Give the agent the file, and nothing else:

```python
from orvu import Team

team = Team.connect(connection="alice.orvu")
```

```ts
import { Team } from "@orvuhq/orvu";

const team = await Team.connect("default", { connection: "alice.orvu" });
```

Or set `ORVU_CONNECT=/path/to/alice.orvu` in its environment. For an MCP
agent:

```text
claude mcp add orvu --env ORVU_CONNECT=/path/to/alice.orvu -- orvu mcp
```

`ORVU_SOCKET` and `ORVU_CONNECT` cannot both be set.

## How it is protected

- The connection is TLS 1.3. The sidecar makes its own certificate the first
  time it is needed, and the agent trusts that certificate and no other.
- The certificate is made out to the name `orvud`, not to an address, so the
  address in the file can be any the agent can reach.
- The agent's rules and the audit record hold over the network as on the
  socket.
- A request over the network may be up to 4 MiB.
- To take access away, remove the agent: `orvud agent remove alice --local`.

## Agents on Windows

There is no sidecar for Windows. The package there has no
sidecar inside, and an agent connects this way. This has been
tested with Python made to look like Windows, on Linux; the build also
installs the package on Windows and checks that it loads.
