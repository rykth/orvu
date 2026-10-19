# Orvu on three machines, in Docker

One command starts a team spread over three machines, with agents in Python
and TypeScript working on it, and tells you what each one does as it goes.
Every step checks what came of it, so the demo stops and says what went
wrong if anything is not as it should be.

You need Docker with Compose. Nothing else: Rust, Python and Node are only
used inside the images.

## Run it

From the top of the repository:

```text
docker compose -f examples/compose/compose.yaml up --build
```

The first build takes several minutes, most of it building the sidecar. It
also fetches the 30 MB model that search by meaning uses, once, so the
machines need no internet.

The story ends with `all checks passed`. The rest keep running until you
press Ctrl-C.

To run it with no one watching, as a test, use `check.sh`. It starts from
nothing, stops after 20 minutes at most, cleans up, and exits with the
story's exit code, 0 when every check passed:

```text
sh examples/compose/check.sh
```

## What you will see

Each line says which agent did it, in which language, on which machine,
such as `[alice/python@m1]`.

| Who | Where | Written in |
|---|---|---|
| alice | m1 | Python |
| bob | m2 | TypeScript |
| dave | m2 | Python, with rules: he may change only his own notes and tasks, and may spend but not top up |
| carol | m3 | TypeScript |
| scout | m1 | no code: the MCP server over HTTP, used by a small scripted client |

1. alice and bob write the same note at the same moment on two machines.
   Both values are kept, each with its author, until carol settles it.
2. Tasks are added, claimed and finished. dave is told no for a task bob
   holds.
3. A firm task: bob claims it with a ticket and goes quiet. carol takes it
   over once his claim runs out, and bob's `still_holding` says no. alice
   sets it back by force, on a higher ticket.
4. A budget is topped up on two machines and spent on a third. A spend that
   is too large is told no, and who topped up and who spent is recorded.
5. Messages go between Python and TypeScript, and one to everyone.
6. alice and bob write one document; carol reads who wrote which part.
7. Three agents count votes on a tally.
8. The roster shows everybody here.
9. A search by meaning finds a note and a task.
10. A watch in TypeScript sees a write made in Python, and the other way
    round.
11. m3 is switched off. The others carry on, and when m3 is back it
    catches up, and all three machines hold the same.
12. A scripted MCP client is turned away without the token, and with it
    reads a note and adds a task as scout.
13. dave tries to change carol's note and is refused, and m2's audit log
    shows it.

## How it is put together

| File | What it is |
|---|---|
| `compose.yaml` | The machines, the agents and the story |
| `Dockerfile` | Builds the sidecar, the Python package and the TypeScript package from this repository |
| `setup.sh` | Runs `orvud setup` for the three machines, gives each agent a key on its machine, and gives dave his rules |
| `machine.sh` | Runs a machine's sidecar, and switches it off and on when the story asks |
| `agents/agent.py`, `agents/agent.ts` | An agent. It does what the story tells it, one step at a time, with the `orvu` package |
| `agents/scout.sh` | Starts the MCP server over HTTP as scout |
| `agents/mcp_client.py` | The small MCP client the story uses in place of an agent program |
| `agents/story.py` | The story: tells the agents what to do and checks every outcome |
| `check.sh` | Runs it all with no one watching |

The certificates, keys and token are made when the demo starts, in a volume
of the demo's own, and are never kept anywhere else. Each agent's container
is given only its machine's socket and the keys, not the machine's folder.

## Stop and clean up

Press Ctrl-C, then remove the containers, the network and the volumes:

```text
docker compose -f examples/compose/compose.yaml down -v
```

The images stay, so the next run starts at once. To remove them too:

```text
docker compose -f examples/compose/compose.yaml down -v --rmi all
```

The containers, network, volumes and images the demo makes are labelled
`com.orvu.demo=1`. What the build caches, the sidecar's compiled
dependencies most of all, about 1 GB, is in Docker's build cache under ids
that start with `orvu-demo`: `docker builder du --verbose` shows it.
