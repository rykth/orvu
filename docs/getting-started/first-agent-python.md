# Your first agent in Python

Two programs sharing notes, a task board and a budget, in about ten minutes.
You need the package installed ([how](install.md)).

[`python/QUICKSTART.md`](../../python/QUICKSTART.md) goes through the same
ground at a slower pace, step by step.

## 1. Share a note

Save this as `alice.py` and run it with `python alice.py`:

```python
from orvu import Team

with Team.connect(me="alice") as team:
    team.notes.put("deploy-date", "Friday")
    print(team.notes.all())
```

Now this, as `bob.py`, from a second terminal:

```python
from orvu import Team

with Team.connect(me="bob") as team:
    print(team.notes.get("deploy-date"))   # Friday
```

Two programs, one shared memory. The note is still there after a restart.

`Team.connect()` joins the team called `default`. Give another name, as in
`Team.connect("research", me="alice")`, for a team of its own. `me` is the
agent's name: it is written on the notes, tasks and messages it makes.

## 2. Share a task board

```python
from orvu import Team

with Team.connect(me="alice") as team:
    team.tasks.add("Summarise the Q3 report")
    team.tasks.add("Check the numbers")

    for task in team.tasks.list("todo"):
        if team.tasks.claim(task):
            # ... do the work ...
            team.tasks.finish(task, result=f"{task.title}: done")
```

`claim` says whether the task is now this agent's. A claim runs out after a
quarter of an hour unless the agent calls `team.tasks.still_working(task)`,
so a task held by an agent that crashed goes back to the others.

## 3. Share a budget

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

A budget can never be overspent, however many agents spend from it at once.
Being told no is an ordinary answer, `False`, not an error.

## 4. React to changes

```python
from orvu import Team

with Team.connect(me="alice") as team:
    for tasks in team.tasks.watch():
        print(len(tasks), "tasks on the board")
        break   # a real agent would carry on here
```

`watch()` gives the value now, and again each time anyone changes it.

## 5. Watch three agents work together

```text
python -m orvu.demo
```

Three agents, as three separate programs, work through twelve tasks under a
budget that is too small for all of them. It runs on a sidecar of its own
and leaves nothing behind. `python -m orvu.demo_machines` does the same on
three machines, all on this computer, and switches one off in the middle.

## Next

- [Use the building blocks](../how-to/building-blocks.md): documents, the
  inbox, tallies, the roster and search.
- [Python API overview](../reference/python-api.md)
- [Run one team on several machines](../how-to/several-machines.md)
