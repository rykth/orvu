# Use the building blocks

A `Team` gives every agent that connects with the same team name the same
notes, task board, budgets, documents, inbox, tallies and roster, and search
over them. The examples are in Python. TypeScript has the same blocks with
camel-case names and `await`: see the
[TypeScript API overview](../reference/typescript-api.md).

```python
from orvu import Team

team = Team.connect("research", me="researcher-1")
```

## Notes

Small facts by name. A note holds text, a number, true or false, a list or a
dict, and is replaced whole.

```python
team.notes.put("deploy-date", "Friday")
team.notes.get("deploy-date")        # "Friday"
team.notes.author("deploy-date")     # "researcher-1"
team.notes.all()                     # {"deploy-date": "Friday"}
team.notes.remove("deploy-date")
```

Two agents that write a note at the same moment leave both values: `get`
raises `orvu.Clash`. See [A clash is kept](../concepts/clashes.md).

### Structured notes

A note that is a JSON document, where different agents fill in different
fields without clashing:

```python
launch = team.notes.structured("launch")
launch.set(["owner"], "researcher-2")
launch.set(["steps"], ["collect", "summarise"])
launch.get(["owner"])                  # "researcher-2"
team.notes.structured_names()        # ["launch"]
```

## Tasks

```python
task = team.tasks.add("Summarise the Q3 report")
sub = team.tasks.add("Fetch the figures", parent=task)

if team.tasks.claim(task):
    team.tasks.still_working(task)   # now and then, or the claim runs out
    team.tasks.finish(task, result="It went up.")

team.tasks.list("todo")              # tasks in one state: todo, doing, done
team.tasks.children(task)            # its sub-tasks
team.tasks.update(task, notes="Use the audited figures")
team.tasks.release(task)             # give a claimed task back
team.tasks.move(sub, parent=None)    # make it a top-level task
```

- A claim lasts a quarter of an hour after the last `still_working`, or
  `claim(task, lease=seconds)`. Then another agent's `claim` takes it over,
  and the takeover is shown on the task as `taken_over`.
- A finished task stays finished. Set its state to `"todo"` with `update` to
  open it again.
- For a task only one agent may ever hold at a time, add it with
  `firm=True`, and use `claim_ticket` for steps that must not happen twice.
  See [Firm tasks and tickets](../concepts/firm-tasks.md).

## Budgets

An amount that can never be overspent, however many agents spend at once.

```python
tokens = team.budget("tokens")
tokens.top_up(2000)
tokens.spend(1500)                   # True
tokens.spend(1500)                   # False: not enough left, nothing spent
tokens.left()                        # 500
tokens.spent_by()                    # {"researcher-1": 1500}
```

Across machines a budget is cautious: a machine that is cut off can spend
only its own share.

## Documents

Text several agents write together. Each part remembers who wrote it.

```python
report = team.document("q3-report")
report.append("Sales rose 4%.")
report.insert(0, "Summary: ")
report.text()                        # "Summary: Sales rose 4%."
report.written_by()                  # [("Summary: Sales rose 4%.", "researcher-1")]
team.documents()                     # ["q3-report"]
```

Positions count characters, as Python strings do. `delete(index, length)`
takes text out, and `format` and `unformat` mark a stretch, such as bold.

## Inbox

Messages to one agent, or to everyone.

```python
team.inbox.send("researcher-2", {"task": task.id, "say": "please check this"})
team.inbox.send_all("Deploy is on Friday.")

for message in team.inbox.unread():  # each message once
    print(message.sender, message.body)

team.inbox.messages()                # the recent ones, given or not
```

Reading does not remove a message. What an agent with a name was given is
written down, so a program that starts again under that name is not given it
again.

## Tallies

Counting, per agent and in total. A tally only goes up.

```python
used = team.tally("tokens-used")
used.add(1500)
used.total()                         # 1500
used.by_name()                       # {"researcher-1": 1500}
```

## Roster

Who is on the team, and who is here now.

```python
team.roster.join()                   # says "still here" every 15 seconds
team.roster.here()                   # ["researcher-1"]
team.roster.members()
team.roster.leave()
```

Closing the team takes the agent off the roster too.

## Search by meaning

```python
for found in team.search("When do we ship?"):
    print(found.kind, found.name, found.score, found.text)
```

It finds the notes, structured notes, paragraphs of documents and tasks
closest in meaning to the question, even with no word in common. The first
search fetches a 30 MB model from the internet, once. On a machine with no
internet, see "Search by meaning" in [`python/README.md`](../../python/README.md).

## Plain shared objects

Under the blocks are eleven kinds of plain object, which you can use
directly when no block fits:

```python
import orvu

with orvu.connect() as client:
    client.counter("visits").increment(5)
    client.list("queue").insert(0, "first")
    client.json("launch").set(["title"], "Launch")
```

Leave names that start with `orvu/` to the blocks. The kinds are in the
[Python API overview](../reference/python-api.md).
