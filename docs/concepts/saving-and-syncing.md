# Saving and syncing

## Saving to disk

Each sidecar saves its machine's copy to one file in its data folder, with
[redb](../../crates/orvu-store), an embedded database.

When a change counts as saved depends on how the sidecar was started:

| Sidecar | Default | What a crash can lose |
|---|---|---|
| Local, started for you or with `orvud --local` | Each change is on disk before it is answered | Nothing that was answered as done |
| From a settings file, `orvud --config` | Changes are saved together every 100 milliseconds | Up to the last 100 milliseconds of this machine's own changes |

Both can be changed with `[storage] fsync = "always"` or `"interval"` and
`interval_ms` (see [the settings reference](../reference/orvud.md)).

Either way, a change is sent to other machines only once it is on disk, in
the order it was made. So no other machine ever holds a change this one
could lose.

If saving fails, for example because the disk is full, the sidecar stops
serving rather than show data that may not be on disk. It answers every
call with a failure that starts "cannot save changes", then exits with a
failure status so that it can be started again from what is on disk. A
sidecar a library started is started again by the next `connect`.

Stopping a sidecar with Ctrl-C or SIGTERM finishes the requests in progress
and saves what is not on disk yet. Killing it outright skips that.

## Syncing between machines

- **Changes are pushed.** A change is sent to the other machines in the
  background, gathered into small batches for up to a hundredth of a
  second.
- **Machines compare notes.** Now and then each pair of machines compares
  what it has seen and sends what is missing. A machine that was away gets
  what it missed this way. One that is far behind is sent whole objects
  instead of single changes.
- **Links come back by themselves.** A link that went silent is given up
  after about half a minute. A machine keeps dialing the others, waiting
  longer after each failure, up to five seconds, so a machine that comes
  back is found within a few seconds.
- **A cut-off machine keeps working.** Its programs read and write its own
  copy. What they wrote reaches the others when the link is back.
- **Links are encrypted both ways.** Machines talk over QUIC with mutual
  TLS: each shows a certificate signed by the team's own key.

## Machine numbers

Every machine has a number, and no two machines may share one. Each change
is named by the machine's number and a count, so two machines with one
number would overwrite each other's changes in ways that cannot be
repaired. That is why:

- a data folder copied to another computer is refused until you say whether
  it was moved (`--moved`) or copied (`--new-number`);
- a data folder restored from a backup must take a new number
  (`--restored`): see [Back up and restore](../how-to/backup-restore.md);
- a machine made by `orvud setup` keeps its number for as long as it keeps
  its data.

## Where next

- [Back up and restore](../how-to/backup-restore.md)
- [`crates/orvu-runtime`](../../crates/orvu-runtime), the engine in detail
