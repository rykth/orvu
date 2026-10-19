# See what the sidecar is doing

The sidecar can keep a log and serve a page of numbers for Prometheus. Both
are off unless you ask for them. It always keeps an audit record of changes.

## Turn on the log and the numbers

For the local sidecar, put a file `orvud.toml` in its data folder
(`~/.local/share/orvu` on Linux, `~/Library/Application Support/orvu` on a
Mac):

```toml
[logs]
level = "info"

[metrics]
addr = "127.0.0.1:9464"
```

For a sidecar from `orvud setup`, the same two sections are already in its
`orvu.toml`, behind a `# ` to take off.

It is read when the sidecar starts. With no `file` in `[logs]`, the local
sidecar's log goes to `orvud.log` in the data folder. The numbers are at
`http://127.0.0.1:9464/metrics`. The page has no password: serve it on
`127.0.0.1` or a network only you use.

The numbers include each operation by name and outcome
(`orvu_sidecar_operations_total`), how long calls take, open watches and
connected clients, and the engine's own counters. The full list is in the
[`orvud` README](../../crates/orvud).

## Read the audit record

Every change made through the sidecar is recorded with the agent that made
it, and so is everything an agent was not allowed to do:

```text
orvud audit --local --agent alice --since 2h
orvud audit --local --object orvu/research/ --not-allowed
orvud audit --config orvu.toml --json
```

The record keeps the shape of a change, not what was written. It is the file
`audit.log`, next to the agents file, cut into `audit.log.1` and
`audit.log.2` as it passes 64 MiB. Each machine records only what its own
programs did.

## Check that it is running

- The local sidecar prints one line to `orvud.log` when it is ready, naming
  its socket, and `orvud: stopped` when it stops.
- `orvud.where` in the data folder names the socket of the local sidecar
  that is running now.
- For a sidecar under systemd: `systemctl --user status orvud-alpha` and
  `journalctl --user -u orvud-alpha`.

## Measure it

```text
python -m orvu.measure
```

It sets up three machines on this computer and prints how fast writes, reads,
tasks and syncing are. [`python/MEASUREMENTS.md`](../../python/MEASUREMENTS.md)
has one run's numbers and what they do and do not say.
