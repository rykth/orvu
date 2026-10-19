# One team on several machines

A team does not have to live on one machine. Each machine runs a sidecar of
its own with its own copy of the team's notes, tasks and budgets, and the
sidecars keep each other in step. A machine that drops out does not stop the
others, and it catches up when it is back.

This page takes you from one machine to three.

## What you need

- The package on every machine: `pip install orvu`. It brings the sidecar,
  `orvud`.
- A name or an IP address for each machine that the others can reach it by.
- One UDP port open between the machines. It is 7440 unless you say
  otherwise.

## 1. Make the settings

Do this once, on any one of the machines. Give a folder, and then each
machine as a name of your choosing and its address:

```text
orvud setup crew alpha=10.0.0.5 beta=10.0.0.6 gamma=box3.example.com
```

It answers:

```text
Made the settings for 3 machines in crew:

  crew/alpha  machine 1  reached at 10.0.0.5:7440
  crew/beta   machine 2  reached at 10.0.0.6:7440
  crew/gamma  machine 3  reached at box3.example.com:7440

Copy each folder to its machine, as ~/orvu/<name>. Each holds that
machine's secret key, so send it nowhere else. Then start the sidecar on
each machine, from inside its folder:

  orvud --config orvu.toml --socket orvud.sock

or have systemd keep it running: orvud-<name>.service in each folder says
how.

The machines reach each other on UDP port 7440. Open it between them.

The key that signs the certificates is kept in

  crew/authority

Keep it there, and send it nowhere: anyone with it can make a machine the
others trust. With it, a machine can be added later, and the others need
not be stopped:

  orvud setup crew --add <name>=<address>

A machine keeps its number. If you make these settings again, to take a
machine out at once, make them in a new folder from this one, and give
each machine that stays its number:

  orvud setup <new-folder> --from crew alpha=10.0.0.5#1 beta=10.0.0.6#2 gamma=box3.example.com#3
```

Each machine's folder holds five files: its settings (`orvu.toml`), its
certificate and secret key (`node.pem` and `node-key.pem`), the certificate
they were all signed with (`ca.pem`), and a file that lets systemd keep its
sidecar running (`orvud-alpha.service` for alpha). The certificates are how
the machines know each other. A machine without one made here is turned
away. The key they were signed with stays in `crew/authority`, on the
computer where you ran the command.

If a machine's name cannot be looked up on that computer, the command warns
about it, and makes the settings all the same: the machines may know names
it does not.

To use another port, add it to the address: `alpha=10.0.0.5:9000`.

## 2. Copy each folder to its machine

Any way of copying a folder will do. For example:

```text
scp -r crew/beta 10.0.0.6:crew
```

The folder a machine was made for stays on that machine. Whoever has it can
join the team as that machine.

## 3. Start the sidecar on each machine

From inside the folder:

```text
orvud --config orvu.toml --socket orvud.sock
```

It prints one line when it is ready, and it has to keep running. Stop it with
Ctrl-C. The data goes in a `data` folder beside the settings. The machines
can be started in any order, and each finds the others when they are up.

To have systemd keep it running instead, and start it again if it fails,
put the folder at `~/orvu/<name>` and, on that machine:

```text
systemctl --user link ~/orvu/beta/orvud-beta.service
systemctl --user enable --now orvud-beta.service
loginctl enable-linger
```

The file expects `orvud` at `~/.local/bin/orvud`, where `pip install --user`
puts it; if `command -v orvud` says it is elsewhere, change the path in the
file first. The last line keeps the sidecar running while nobody is logged
in. On a Mac, launchd can do the same, but no file is made for it and it has
not been tried.

## 4. Use the team from your programs

A program says which sidecar it means by its socket, the `orvud.sock` in the
folder:

```python
from orvu import Team

team = Team.connect("research", socket="/home/me/crew/orvud.sock", me="researcher-1")
team.notes.put("deploy-date", "Friday")
```

A program on another machine does the same with the socket on its own
machine, and reads the note a moment later. An agent that uses the MCP
server is given the socket too: `orvu mcp --socket /home/me/crew/orvud.sock
--me researcher-1`.

A program can also be given the socket in its environment, with nothing in
its code: `ORVU_SOCKET=/home/me/crew/orvud.sock`. Without either,
`Team.connect()` uses the sidecar that keeps its data on this one machine and
shares it with nobody, as before. The two do not mix. The package starts
only that one by itself: the team's sidecar is started by you, or by
systemd, and if nothing answers on its socket, `Team.connect` says so.

To check who connects, give each agent a key on the machine it runs on:

```text
orvud agent add researcher-1 --config orvu.toml
```

The list of agents is kept next to the settings file, and each machine has
its own: an agent connects to the sidecar on its own machine, and only that
sidecar's list counts. So does the record of who changed what, `audit.log`:
each machine records what its own agents did, and `orvud audit --config
orvu.toml` shows it.

An agent with no sidecar of its own, in a container or on a computer that is
not one of the team's machines, can connect to one of them over the network
instead: see "Serve agents on other machines and in containers" in the
`orvud` README.

## See it first

```text
python -m orvu.demo_machines
```

This runs three machines on this computer, with an agent on each, switches
one of them off in the middle and on again, and shows what the others do
meanwhile. Nothing has to be set up for it.

## Try it on one computer first

Three sidecars can stand for three machines on one computer, if each gets a
port of its own:

```text
orvud setup crew alpha=127.0.0.1:7441 beta=127.0.0.1:7442 gamma=127.0.0.1:7443
```

Start each in a terminal of its own, from inside its folder, and point one
program at each socket.

## The settings file

This is `crew/alpha/orvu.toml` from the first example. You can change it by
hand. A setting it does not know stops the sidecar with a message.

```toml
# Settings for the machine "alpha", made by `orvud setup`.
# Start the sidecar on that machine with:
#     orvud --config orvu.toml --socket orvud.sock

# This machine's number. No two machines of a team may share one, and
# a machine keeps its number for as long as it keeps its data.
replica = 1
namespace = "team"

# Where this machine listens for the others, and where it finds them.
listen = "0.0.0.0:7440"
peers = ["10.0.0.6:7440", "box3.example.com:7440"]

# The files that let the machines trust each other. `node-key.pem` is
# this machine's own secret.
[security]
cert = "node.pem"
key = "node-key.pem"
ca = "ca.pem"

# Where the data is kept, next to this file.
[storage]
path = "data/orvu.redb"

# A budget is shared out between the machines once a second, so that
# each can spend from it without asking, and most of it can still be
# spent when one machine is away.
[budget]
rebalance_ms = 1000

# The sidecar keeps no log and serves no page of numbers unless asked.
# To have a log, take the `# ` off the three lines of [logs] below.
# The level is one of error, warn, info, debug or trace. Leave `file`
# out to have the log on the screen.
# [logs]
# level = "info"
# file = "orvud.log"
#
# To have a page that Prometheus can collect, at /metrics, take the
# `# ` off the two lines of [metrics] below. Anyone who can reach the
# page's address can read it.
# [metrics]
# addr = "127.0.0.1:9464"
```

## When a machine drops out

- **The others go on.** They read and write as before, and keep each other
  in step.
- **It catches up when it is back.** Whether it was restarted or only cut
  off, the machines find each other again by themselves and exchange what
  each missed. Nothing has to be restarted. A link that went silent is given
  up after about half a minute, and from then on it is tried again every
  few seconds.
- **It can go on working while it is cut off.** Its programs read and write
  its own copy. What they wrote reaches the others when the link is back,
  and the copies end up the same.
- **Its share of a budget waits for it.** A budget is shared out between the
  machines. With three machines and one away, the other two can spend their
  two thirds. The last third is with the machine that is away, and can be
  spent when it is back. A budget is never overspent.
- **`left()` counts every machine's share,** the ones that cannot be reached
  too. So `spend` can say no while `left()` still shows an amount.
- **Two agents on machines that are cut off from each other can both claim
  the same task.** Each is told yes, because neither can see the other. When
  the link is back, one of them is shown as the holder. A claim is soft, as
  on one machine. A firm task, added with `firm=True`, goes to one agent
  at a time: an agent whose machine cannot reach the machine that holds the
  task's ticket is told no. But an agent on a machine that is cut off keeps
  hearing yes from `still_working` while the others take its task over,
  once its claim has run out.

## Things to know

- **The data of a one-machine sidecar is not carried over.** A team made
  here starts empty. What you kept with a plain `Team.connect()` stays where
  it was.
- **A machine keeps its number.** The command numbers the machines from 1
  and tells you each one's number. The data a machine saves is saved under
  its number, so it has to have the same one every time its settings are
  made. A sidecar refuses to start on data that was written under another
  number.
- **To add a machine,** run the command again with `--add`, on the computer
  that has `crew/authority`:

  ```text
  orvud setup crew --add delta=10.0.0.8
  ```

  It makes `crew/delta`, with the next number, and prints the new `peers`
  line for each of the other machines. The sidecars that are running let
  delta in at once and need not be stopped. Put the new line in each
  machine's `orvu.toml` when it is next started, so that it reaches delta by
  itself too.
- **To take a machine out for good,** stop its sidecar and run, on that
  machine, from inside its folder:

  ```text
  orvud leave --config orvu.toml
  ```

  It connects to the others, hands them its share of every budget, waits
  until they have it, and stops. The others give it nothing from then on.
  Do not start it again, and take its address out of the others' `peers`.
  Its certificate still works: whoever holds its folder can still join as
  that machine. Retiring a machine with a signed notice, and certificates
  that run out, are to come.
- **To shut a machine out at once,** make the settings again for the ones
  that stay, in a new folder, from the old one with `--from`, with each
  machine's number after its address. The command prints that line for you
  when it has finished. To take `beta` out of the team above:

  ```text
  orvud setup crew2 --from crew alpha=10.0.0.5#1 gamma=box3.example.com#3
  ```

  `--from` carries every number the old team gave out over to the new one,
  beta's with them, so that no machine added later is ever given beta's
  number: the others keep beta's data under it, and a newcomer would
  inherit it. A machine given no number gets one nobody has had, and a
  number that was a machine's taken out is refused.

  On each machine, replace the files and keep the `data` folder. The old
  certificates stop working, so every sidecar has to be started again with
  its new files. A share of a budget that beta held goes with it: run
  `orvud leave` on beta first if it can still be reached.
- **The certificates do not run out.**
- **Never run one machine's folder on two machines.** Two machines with the
  same number damage the data in ways that cannot be repaired.
- **Keep the machines' clocks right.** When two machines change the same
  thing, the later change wins, and "later" is by their clocks.
- **A machine can be given by name.** The name is looked up each time a
  machine is dialed, so a machine whose address changed is found again.
- **A machine given by name listens for both kinds of address,** IPv4 and
  IPv6, because the name can stand for either. On a machine with IPv6
  switched off it listens for IPv4 only, and says so when it starts.
- **A machine that cannot be reached is tried less and less often,** up to
  once every five seconds. So a machine that comes back is found within a
  few seconds, not at once.
- **The socket is for programs of the same user on the same machine.** A
  program on another machine uses the sidecar on its own machine.
- **Linux and Mac.** There is no Windows version yet.

## Where next

- [`QUICKSTART.md`](QUICKSTART.md) shows notes, tasks and budgets from the
  start.
- [`MCP.md`](MCP.md) gives an agent the team as tools.
- [`README.md`](README.md) lists everything the package can do.
