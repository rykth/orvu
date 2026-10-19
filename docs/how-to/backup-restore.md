# Back up and restore

There is no backup command. A backup is a copy of the data folder, taken
while the sidecar is stopped. Restoring one needs care, because of machine
numbers: read the whole page before you restore anything.

## Why restoring is special

Every change is named by the machine's number and a count that goes up.
A machine rolled back to a backup has forgotten changes it may already have
passed on, and nothing tells it so. Under its old number, its next writes
would reuse names the other machines already hold for the lost changes. They
would take the new writes for ones they have, and the copies would disagree
for good.

So **a restored data folder must take a new machine number.** This holds for
any roll-back: a backup, a disk snapshot, a copy put back over the folder.

## What to keep

| What | Where |
|---|---|
| The data | The data folder: `~/.local/share/orvu` on Linux, `~/Library/Application Support/orvu` on a Mac, or the `data` folder beside a machine's `orvu.toml` |
| Agents and their rules | `agents`, in the data folder, or next to `orvu.toml` |
| The audit record | `audit.log` and its older parts, beside `agents` |
| A machine's settings and certificates | The machine's folder from `orvud setup` |
| The key that signs machine certificates | `crew/authority`, on the computer where you ran `orvud setup`. Without it you cannot add a machine later. Keep it as secret as any key |

## Back up a local sidecar

1. Stop it. The libraries stop it by themselves five minutes after the last
   program has gone. A sidecar you started by hand stops with Ctrl-C.
2. Copy the data folder somewhere safe.

Do not copy the folder while the sidecar runs: the copy may be half
written.

## Restore a local sidecar

1. Make sure no sidecar is running on the folder.
2. Put the copy back.
3. Start it once by hand with a new number:

   ```text
   orvud --local --data-dir <folder> --restored
   ```

   The data is kept, and new writes go under a new number. Later starts need
   no flag. Stop it, and the libraries start it as usual from then on.

Use `--restored` once per restore. Each use takes yet another number, and an
object keeps changes from at most 1,024 machines.

## A team on several machines

The other machines are the best backup: they hold every change the lost
machine had passed on. To bring a machine back after losing its disk:

1. Make it a new machine, with a new number and a certificate for it:
   `orvud setup crew --add alpha2=10.0.0.5`.
2. Start it with **empty** storage. It gets the team's data from the
   others.
3. If the old machine might still be running anywhere, shut it out:
   see "Add, retire or shut out a machine" in
   [Run one team on several machines](several-machines.md).

Do not start a machine from settings on a restored copy of its old storage
under its old number.

## Moving a data folder to another computer

That is not a restore. Start it once with `--moved` if the old computer no
longer uses it, or `--new-number` if it still does. See
[Troubleshoot](troubleshooting.md#a-data-folder-copied-from-another-computer-is-refused).
