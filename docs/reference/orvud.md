# The `orvud` command and its settings

`orvud` is the sidecar. This page lists its commands and settings in one
place. The [`orvud` README](../../crates/orvud) explains each in full, with
every edge case.

## Commands

```text
orvud --local [--data-dir <folder>] [--new-number | --moved | --restored] [--socket <path>] [--network <address>] [--model <folder>] [--exit-when-idle <seconds>]
orvud --config <path.toml> [--socket <path>] [--network <address>] [--model <folder>] [--exit-when-idle <seconds>]
orvud setup <folder> [--from <earlier folder>] <name>=<address> <name>=<address> ...
orvud setup <folder> --add <name>=<address>
orvud leave --config <path.toml> [--wait <seconds>]
orvud agent add|remove <name> (--local [--data-dir <folder>] | --config <path.toml>)
orvud agent add <name> --address <host:port> --connection <file> (--local [--data-dir <folder>] | --config <path.toml>)
orvud agent list (--local [--data-dir <folder>] | --config <path.toml>)
orvud agent allow <name> <rule> <rule> ... | all (--local [--data-dir <folder>] | --config <path.toml>)
orvud audit (--local [--data-dir <folder>] | --config <path.toml>) [--agent <name>] [--object <start>] [--since <time>] [--not-allowed] [--json]
orvud --version | --help
```

## Running the sidecar

| Flag | What it does |
|---|---|
| `--local` | Run on this machine only: no settings file, no certificates, no other machines |
| `--config <path.toml>` | Run from a settings file, connected to other machines |
| `--data-dir <folder>` | With `--local`, where the data is kept. Default `$XDG_DATA_HOME/orvu`, else `~/.local/share/orvu`; on a Mac `~/Library/Application Support/orvu` |
| `--socket <path>` | Where to serve. Default `$XDG_RUNTIME_DIR/orvud.sock`; with `--local` and no such variable, `orvud.sock` in the data folder; with `--config` and no such variable it must be given |
| `--network <address>` | Also serve agents elsewhere, over TLS, such as `0.0.0.0:7441`. Needs at least one agent with a key |
| `--model <folder>` | The folder holding the search model, instead of fetching it |
| `--exit-when-idle <seconds>` | Stop once no client has been connected for that long |
| `--moved` | With `--local`: the folder was moved from another computer that no longer uses it. Keep its number |
| `--new-number` | With `--local`: the folder was copied from a computer that still uses it. Take a new number |
| `--restored` | With `--local`: the folder was restored from a backup or rolled back. Take a new number. Use once |

Each flag may be given once. Stop the sidecar with Ctrl-C or SIGTERM.

## `orvud setup`

| Form | What it does |
|---|---|
| `orvud setup crew alpha=10.0.0.5 beta=...` | Makes a folder per machine in `crew`, and keeps the signing key in `crew/authority`. Addresses take `:port` (default 7440). Machines are numbered from 1 |
| `orvud setup crew --add delta=10.0.0.8` | Makes one more machine, with a number nobody has had. The running machines let it in at once |
| `orvud setup crew2 --from crew alpha=10.0.0.5#1 ...` | Makes the team again with a new key, for the machines listed, each with its old number after `#`. A machine left out is shut out |

It never writes into a folder that has something in it.

## `orvud leave`

Run on a machine that is leaving for good, after stopping its sidecar. It
hands that machine's share of every budget to the others, waits until they
have everything it wrote (60 seconds, or `--wait`), and stops.

## `orvud agent` and `orvud audit`

See [The agents file and rules](agents-file.md) and
[Give agents keys and rules](../how-to/keys-and-rules.md).

| `orvud audit` flag | Shows only |
|---|---|
| `--agent <name>` | One agent's changes. `-` is changes made with no key |
| `--object <start>` | Objects whose names start with this, such as `orvu/crew/` |
| `--since <time>` | Changes from this long ago, such as `30m`, `2h`, `7d` |
| `--not-allowed` | What agents were not allowed to do |
| `--json` | The lines as they are kept |

## The settings file, `orvu.toml`

`orvud setup` writes one per machine. You can change it by hand. A setting
it does not know stops the sidecar with a message.

```toml
replica = 1                     # this machine's number; never shared, never changed
namespace = "team"              # cannot be "local"
listen = "0.0.0.0:7440"         # where this machine listens for the others
peers = ["10.0.0.6:7440", "box3.example.com:7440"]

[security]
cert = "node.pem"               # this machine's certificate
key = "node-key.pem"            # its secret key
ca = "ca.pem"                   # the certificate they were all signed with

[storage]
path = "data/orvu.redb"         # relative paths are from this file's folder
# fsync = "interval"            # or "always": save before answering
# interval_ms = 100

[budget]
rebalance_ms = 1000             # how often budget shares move between machines

# [logs]
# level = "info"                # error, warn, info, debug or trace
# file = "orvud.log"            # left out: the log goes to the screen

# [metrics]
# addr = "127.0.0.1:9464"       # a page for Prometheus at /metrics
```

## The local settings file, `orvud.toml`

A sidecar started with `--local` has no settings file. To turn on a log or
the page of numbers, or change how it saves, put `orvud.toml` in its data
folder. It may hold only these sections:

| Setting | What it does |
|---|---|
| `[logs] level` | `error`, `warn`, `info`, `debug` or `trace`. Left out, no log |
| `[logs] file` | Where the log goes. Left out, the log goes where the sidecar's output goes: `orvud.log` for one a library started |
| `[metrics] addr` | Where to serve `/metrics`. Left out, no page |
| `[storage] fsync` | `"always"` (the default here): each change is on disk before it is answered. `"interval"`: saved together every so often, faster, but the last moment's changes can be lost |
| `[storage] interval_ms` | With `"interval"`, how often to save. Default 100 |

## Files in the data folder

| File | What it is |
|---|---|
| `orvu.redb` (local) or the `[storage] path` | The data |
| `replica` | This machine's number. Keep it with the data |
| `computer` | Which computer the folder is used on (local only) |
| `agents` | The agents let in, and their rules |
| `audit.log` | Who changed what |
| `network-cert.pem`, `network-key.pem` | The certificate for `--network` |
| `orvud.log` | What a sidecar started by a library prints |
| `orvud.where` | The socket of the local sidecar running now |
| `models/` | The search model, once fetched |

## Environment

See [Environment variables](environment.md).
