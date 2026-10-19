# Run one team on several machines

Each machine runs a sidecar of its own with its own copy of the team's data,
and the sidecars keep each other in step. A machine that drops out does not
stop the others, and catches up when it is back.

This page is the short version. [`python/MACHINES.md`](../../python/MACHINES.md)
goes from one machine to three in full, and the
[`orvud` README](../../crates/orvud) has every detail.

## Before you start

- The `orvu` package on every machine ([install](../getting-started/install.md)). It brings `orvud`.
- A name or an IP address for each machine that the others can reach.
- One UDP port open between the machines: 7440 unless you choose another.
- Clocks kept right on every machine.

To see it first without any setup, run `python -m orvu.demo_machines`.

## 1. Make the settings

On any one computer:

```text
orvud setup crew alpha=10.0.0.5 beta=10.0.0.6 gamma=box3.example.com
```

This makes a folder per machine in `crew`: its settings `orvu.toml`, its
certificate and secret key, the certificate they were all signed with, and a
systemd unit. The key that signed them stays in `crew/authority`. Keep it
there and send it nowhere: anyone with it can make a machine the others
trust. Add `:port` to an address for another port, as in
`alpha=10.0.0.5:9000`.

## 2. Copy each folder to its machine

```text
scp -r crew/beta 10.0.0.6:crew
```

Each folder holds that machine's secret key. Send it only to that machine.

## 3. Start the sidecar on each machine

From inside the folder:

```text
orvud --config orvu.toml --socket orvud.sock
```

Or let systemd keep it running (put the folder at `~/orvu/<name>` first):

```text
systemctl --user link ~/orvu/beta/orvud-beta.service
systemctl --user enable --now orvud-beta.service
loginctl enable-linger
```

The machines can be started in any order.

## 4. Point your programs at it

The libraries never start a sidecar like this one. Give a program the socket:

```python
from orvu import Team

team = Team.connect("research", socket="/home/me/crew/orvud.sock", me="researcher-1")
```

or set `ORVU_SOCKET=/home/me/crew/orvud.sock` in its environment. For an MCP
agent: `orvu mcp --socket /home/me/crew/orvud.sock --me researcher-1`.

## Add, retire or shut out a machine

| To | Run |
|---|---|
| Add a machine | `orvud setup crew --add delta=10.0.0.8`, on the computer with `crew/authority`. The running machines let it in at once. Put the `peers` line it prints into the others' `orvu.toml` for their next start |
| Take one out for good | Stop its sidecar, then on that machine `orvud leave --config orvu.toml`. It hands its share of every budget to the others first |
| Shut one out at once | `orvud setup crew2 --from crew alpha=10.0.0.5#1 gamma=box3.example.com#3`, leaving it out, then give every other machine its new files and keep each `data` folder |

## Good to know

- A team made this way starts empty. Data in your local sidecar stays there.
- A machine keeps its number for as long as it keeps its data. Never run one
  machine's folder on two machines.
- Each machine has its own agents file and audit record.
- Two agents on machines cut off from each other can both claim the same
  soft task. A [firm task](../concepts/firm-tasks.md) goes to one.
- With three machines and one away, the other two can spend their two thirds
  of a budget. The last third waits for the machine that holds it.
