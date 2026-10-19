# Install

## What you need

- **Linux or a Mac** for the sidecar, the small background program that
  holds the data. There is no sidecar for Windows yet: a program on Windows
  connects to a sidecar on Linux or a Mac over the network
  ([how](../how-to/remote-agents.md)).
- **Python 3.10 or newer** for the Python package, or **Node 20 or newer**
  for the TypeScript package.

## Python

On Linux, on Intel/AMD or ARM:

```text
pip install orvu
```

That is the whole setup. The package brings:

- the `orvu` library, with `Team` and the plain shared objects;
- the `orvud` command, the sidecar itself;
- the `orvu` command, whose `orvu mcp` gives an agent program the team as
  tools.

The first `Team.connect()` or `orvu.connect()` starts the sidecar in the
background. Every program you run after that shares it. It stops by itself
five minutes after the last program has gone.

To serve the MCP tools over HTTP as well, install the extra:

```text
pip install 'orvu[http]'
```

### On a Mac or on Windows

PyPI has the package for Linux only for now. On a Mac, build it from this
repository, with the sidecar inside. That needs Rust and `protoc` (see [the
Python package's
README](../../python/README.md#building-the-package-people-install)):

```text
cd python
sh scripts/build-wheel.sh
pip install dist/orvu-*.whl
```

On Windows, the package has no sidecar inside: `sh scripts/build-wheel.sh
--without-sidecar` builds it, on any machine. It connects to a sidecar
elsewhere with a connection file.

## TypeScript

The npm package is not published yet. Build it, with the sidecar inside,
from this repository. That needs Rust, `protoc`, Node and npm, and on Linux
also `cargo zigbuild` (see [the TypeScript package's README](../../typescript/README.md#working-on-this-package)):

```text
sh typescript/scripts/build-package.sh
npm install typescript/release/orvu-0.1.0-linux-x64.tgz
```

The file is named for the kind of machine it was built on, and works on that
kind only: Linux or a Mac, on Intel or ARM.

The package gives the command `orvud` too. Run it with
`npx --no-install orvud ...`, so that `npx` never fetches some other package
of that name from the internet.

## Where things are kept

| What | Linux | Mac |
|---|---|---|
| The data folder | `$XDG_DATA_HOME/orvu`, or `~/.local/share/orvu` | `~/Library/Application Support/orvu` |
| What the sidecar prints | `orvud.log` in the data folder | the same |
| The socket | `$XDG_RUNTIME_DIR/orvud.sock`, or `orvud.sock` in the data folder | `orvud.sock` in the data folder unless `$XDG_RUNTIME_DIR` is set |

A Python program and a Node program of the same user use the same data folder
and the same sidecar, so they see the same data. They have to be the same
release as the sidecar: a sidecar of another release is refused with
`VersionMismatch`, which names the process to stop.

## Next

- [Your first agent in Python](first-agent-python.md)
- [Your first agent in TypeScript](first-agent-typescript.md)
- [Your first MCP agent](first-mcp-agent.md)
