# Quickstart

Three agents sharing a task list and a token budget, in about ten minutes.

You need Python 3.10 or newer, on Linux or a Mac.

## 1. Install

```text
pip install orvu
```

That is the whole setup. There is no server to start and nothing to
configure. A small program that keeps the shared data comes with the package
and starts by itself the first time it is needed.

## 2. Share a note

Save this as `first.py` and run it with `python first.py`.

```python
from orvu import Team

with Team.connect(me="alice") as team:
    team.notes.put("deploy-date", "Friday")
    print(team.notes.all())
```

Now run this from a second terminal, as a second program:

```python
from orvu import Team

with Team.connect(me="bob") as team:
    print(team.notes.get("deploy-date"))   # Friday
```

Two programs, one shared memory. The note is still there tomorrow, and after
a restart.

## 3. Share a task list

Any agent can add tasks, and any agent can take one. `claim` tells an agent
whether the task is now its own.

```python
from orvu import Team

with Team.connect(me="alice") as team:
    team.tasks.add("Summarise the Q3 report")
    team.tasks.add("Check the numbers")

    for task in team.tasks.list("todo"):
        if team.tasks.claim(task):
            # ... do the work ...
            team.tasks.finish(task, result=f"{task.title}: done")

    for task in team.tasks.list():
        print(task.state, task.title, task.finished_by)
```

## 4. Share a budget

A budget is an amount that can never be overspent, however many agents spend
from it at once. `spend` answers yes or no.

```python
from orvu import Team

with Team.connect(me="alice") as team:
    tokens = team.budget("tokens")
    tokens.top_up(1000)

    if tokens.spend(300):
        print("spent 300,", tokens.left(), "left")
    if not tokens.spend(5000):
        print("not enough left, nothing was spent")
```

## 5. React when something changes

`watch()` gives the current value at once and then again each time it
changes, whoever changed it.

```python
from orvu import Team

with Team.connect(me="alice") as team:
    for tasks in team.tasks.watch():
        print(len(tasks), "tasks on the board")
        break   # a real agent would carry on here
```

## 6. Write together, send messages, count, and see who is here

```python
from orvu import Team

with Team.connect(me="alice") as team:
    report = team.document("q3-report")
    report.append("Sales rose 4%.")

    team.inbox.send("bob", "The report is started.")
    team.tally("tokens-used").add(1200)
    team.roster.join()

with Team.connect(me="bob") as team:
    team.document("q3-report").append(" Costs fell.")
    print(team.document("q3-report").written_by())
    for message in team.inbox.unread():
        print(message.sender, "says:", message.body)
    print(team.tally("tokens-used").by_name())
```

Bob sees who wrote which part of the report, the message that was waiting
for him, and what has been counted so far.

## 7. See three agents work together

The package comes with a demo in which three agents, as three separate
programs, work through twelve tasks under a budget that is too small for all
of them.

```text
python -m orvu.demo
```

You will see each agent take tasks, pay for them and finish them, until the
budget runs out. It is never overspent. The demo starts a sidecar of its own
in a folder that is removed when it ends, so it leaves nothing behind, and
nothing in your own sidecar is touched. The demo is one short file,
`orvu/demo.py`, and is meant to be read.

A second demo puts the three agents on three machines, all on this
computer, and switches one machine off in the middle:

```text
python -m orvu.demo_machines
```

The other two go on, the task the third had taken is put back, and when it
is switched on again it catches up. At the end the three machines hold the
same board, and the budget was not overspent.

## Where next

- [`README.md`](README.md) lists everything the package can do, including the
  plain shared objects underneath the blocks used here.
- To share between machines, each machine runs its own `orvud` and they are
  told about each other. [`MACHINES.md`](MACHINES.md) goes through it.
