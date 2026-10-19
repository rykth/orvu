"""The tools an agent gets over MCP: the notes, tasks, budgets, messages,
documents, tallies and roster of one team, and the plain shared objects
programs name themselves.

Each tool is a small function over a `Team`. Its name, its description and
the description of each argument are what the agent's model reads to decide
when to use it, so they are written for that reader.
"""

import dataclasses
import json

from . import _names, errors
from ._tasks import LEASE, STATES

#: Every tool, by name, in the order they are listed to the agent.
TOOLS = {}

#: The groups of tools that `orvu mcp --tools` picks from, each with the
#: start of the names of its tools. whoami is not in a group: it is always
#: there.
GROUPS = {
    "notes": "note_",
    "tasks": "task_",
    "budgets": "budget_",
    "inbox": "inbox_",
    "documents": "doc_",
    "search": "search",
    "tallies": "tally_",
    "roster": "roster_",
    "objects": "object_",
}


def group(name):
    """The group the tool `name` is in, or `None` for one that is always
    there."""
    return next((group for group, start in GROUPS.items() if name.startswith(start)), None)

_TYPES = {
    "string": (str, "text"),
    "integer": (int, "a whole number"),
    "boolean": (bool, "true or false"),
    "array": (list, 'a list of names and positions, such as ["steps", 0]'),
}


@dataclasses.dataclass(frozen=True)
class Tool:
    name: str
    description: str
    #: The arguments, by name. Each is a piece of JSON Schema.
    arguments: dict
    #: The names of the arguments that cannot be left out.
    required: tuple
    run: object

    def schema(self):
        """The arguments as a JSON Schema, which is how MCP describes them."""
        return {
            "type": "object",
            "properties": self.arguments,
            "required": list(self.required),
            "additionalProperties": False,
        }

    def check(self, given):
        """Returns `given` as the tool is to be run with it. Raises
        `BadRequest` if it is not what the tool takes."""
        if not isinstance(given, dict):
            raise errors.BadRequest(f"{self.name} takes its arguments as an object, not {type(given).__name__}")
        for name in given:
            if name not in self.arguments:
                takes = ", ".join(self.arguments) or "no arguments"
                raise errors.BadRequest(f"{self.name} has no argument {name!r}. It takes: {takes}")
        # Many models fill in what they do not use with null. It means "left
        # out".
        given = {name: value for name, value in given.items() if value is not None}
        for name in self.required:
            if name not in given:
                raise errors.BadRequest(f"{self.name} needs the argument {name!r}")
        for name, value in given.items():
            wanted = self.arguments[name]
            if "type" not in wanted:
                # Any JSON value. It came as JSON, so it is one.
                continue
            kind, called = _TYPES[wanted["type"]]
            # The only lists taken are paths: names and positions.
            if kind is list and isinstance(value, list):
                if not all(isinstance(part, (str, int)) and not isinstance(part, bool) for part in value):
                    raise errors.BadRequest(f"{self.name}: {name!r} has to be {called}, not {value!r}")
                continue
            # JSON has one kind of number, and some programs write 5 as 5.0.
            if kind is int and isinstance(value, float) and value.is_integer():
                value = given[name] = int(value)
            # True and False count as numbers in Python. They are not numbers here.
            if not isinstance(value, kind) or (kind is not bool and isinstance(value, bool)):
                raise errors.BadRequest(f"{self.name}: {name!r} has to be {called}, not {value!r}")
            if "enum" in wanted and value not in wanted["enum"]:
                raise errors.BadRequest(f"{self.name}: {name!r} has to be one of {wanted['enum']}, not {value!r}")
        return given


def text(description):
    return {"type": "string", "description": description}


def number(description):
    return {"type": "integer", "description": description}


def flag(description):
    return {"type": "boolean", "description": description}


def anything(description):
    """An argument that can be any JSON value: text, a number, true or false,
    a list or an object."""
    return {"description": description}


def path(description):
    return {"type": "array", "items": {"type": ["string", "integer"]}, "description": description}


def _one_of(tool, text, data):
    """The one of `text` and `data` that was given. A tool that takes either
    says so with these two."""
    if text is not None and data is not None:
        raise errors.BadRequest(f"{tool}: give 'text' or 'data', not both")
    return text if text is not None else data


def tool(description, required=(), **arguments):
    """Adds the function below it to the tools, under the function's name."""

    def add(run):
        TOOLS[run.__name__] = Tool(run.__name__, description, arguments, tuple(required), run)
        return run

    return add


# ---- who and where -----------------------------------------------------------


@tool("Says which team this is and what name you have in it. Other agents see that name on the tasks you take.")
def whoami(team):
    return {"team": team.name, "you": team.me}


def _shown(task):
    """A task as the agent sees it: its fields, without the empty ones."""
    fields = dataclasses.asdict(task)
    # A reading of this program's own clock, which means nothing to the
    # agent. `abandoned` below says what it is for.
    del fields["created"], fields["quiet_since"]
    if not task.firm:
        del fields["firm"], fields["ticket"]
    if task.abandoned():
        fields["abandoned"] = True
    return {name: value for name, value in fields.items() if value is not None}


_TASK = text("The task's id, such as t-3f9a1c2e5b7d4a60. task_list shows the ids.")
_BUDGET = text('The budget\'s name, such as "tokens". budget_list shows the names.')

#: How many a list gives at once when it is not told, and the most it gives.
_PAGE, _MOST = 100, 500
#: About how many characters one list answer holds, however many it may
#: give. A model reads every one of them.
_ROOM = 50_000


def _limit(description):
    return number(f"{description} From 1 to {_MOST}. Without it, up to {_PAGE}.")


def _page(tool, items, limit):
    """The first of `items`, pairs of a key and what is shown for it, that
    fit in one answer. Returns those and what the answer says about the
    rest."""
    if limit is None:
        limit = _PAGE
    elif not 1 <= limit <= _MOST:
        raise errors.BadRequest(f"{tool}: 'limit' is from 1 to {_MOST}, not {limit}")
    taken, size = [], 0
    for key, shown in items:
        size += len(json.dumps(shown, ensure_ascii=False))
        if taken and (len(taken) >= limit or size > _ROOM):
            break
        taken.append((key, shown))
    left = len(items) - len(taken)
    if not left:
        return taken, {}
    last = taken[-1][0]
    return taken, {
        "more": f"{left} more not shown. Call {tool} again with after={json.dumps(last, ensure_ascii=False)} to read on.",
        "next_after": last,
    }


# ---- notes -------------------------------------------------------------------


@tool(
    "Saves a short fact for the whole team under a name, such as a decision or a finding. "
    "Every agent on the team can read it. Writing to a name that exists replaces what was there, "
    "and settles a note that agents wrote at the same time.",
    required=["name", "text"],
    name=text('What to call the note, such as "deploy-date".'),
    text=text("What the note says."),
)
def note_put(team, name, text):
    team.notes.put(name, text)
    return {"saved": name}


#: What an agent is told of a note that agents wrote at the same time.
_SETTLE = (
    "Two or more agents wrote this note at the same time, and it holds what each wrote. "
    "Decide what it should say and write it with note_put, which settles it for everyone."
)


def _clashed(clash):
    return {"clash": True, "texts": clash.values, "written_by": clash.authors, "settle": _SETTLE}


@tool(
    "Reads one of the team's notes by its name. A note that agents wrote at the same time gives "
    "every text written, and who wrote each, until somebody writes it again.",
    required=["name"],
    name=text("The note's name. note_list shows the names."),
)
def note_get(team, name):
    try:
        found = team.notes.get(name)
    except errors.Clash as clash:
        return {"found": True, "name": name, **_clashed(clash)}
    if found is None:
        return {"found": False, "name": name}
    return {"found": True, "name": name, "text": found}


@tool(
    "Says which agent wrote one of the team's notes. A note that agents wrote at the same time gives "
    "who wrote each of its texts.",
    required=["name"],
    name=text("The note's name. note_list shows the names."),
)
def note_author(team, name):
    by = team.notes.author(name)
    if by is None and team.notes.get(name) is None:
        return {"name": name, "found": False}
    # None here is a note that a program which gave no name wrote.
    return {"name": name, "found": True, "by": by}


@tool(
    "Lists the team's notes, with their text, in order of name. Read this first to learn what the team "
    "already knows. A note that agents wrote at the same time is shown with every text written. "
    "A long list comes a part at a time: the answer then says how to read on.",
    after=text("Only the notes whose names come after this one. The answer before says what to give."),
    limit=_limit("How many notes to give at most."),
    names_only=flag("True to give only the names, which is shorter on a large team."),
)
def note_list(team, after=None, limit=None, names_only=False):
    notes = team.notes.all()
    names = sorted(name for name in notes if after is None or name > after)
    if names_only:
        taken, more = _page("note_list", [(name, name) for name in names], limit)
        return {"names": [name for name, _ in taken], **more}
    shown = [(name, _clashed(notes[name]) if isinstance(notes[name], errors.Clash) else notes[name]) for name in names]
    taken, more = _page("note_list", shown, limit)
    return {"notes": dict(taken), **more}


@tool(
    "Removes one of the team's notes, for every agent.",
    required=["name"],
    name=text("The note's name."),
)
def note_remove(team, name):
    team.notes.remove(name)
    return {"removed": name}


_STRUCTURED = text(
    'The structured note\'s name, such as "plan". note_structured_list shows the names. '
    "Use letters, digits, dots, dashes and underscores."
)
_WHERE = path(
    'Where in the note, as a list of field names and list positions, such as ["steps", 0]. '
    "Leave it out for the whole note."
)


@tool(
    "Reads a structured note: a JSON document with fields and lists that agents fill in together. "
    "Two agents that change different fields both keep their change.",
    required=["name"],
    name=_STRUCTURED,
    path=_WHERE,
)
def note_structured_get(team, name, path=None):
    note = team.notes.structured(name)
    value = note.get(path) if path else note.to_value()
    if value is None and name not in team.notes.structured_names():
        return {"name": name, "found": False}
    return {"name": name, "found": True, "value": value}


@tool(
    "Sets one field of a structured note to any JSON value: text, a number, true or false, a list or an "
    "object. The note, and the objects on the way to the field, are made if they are not there. "
    "Change only the field you mean to, so that what others wrote at the same time is kept.",
    required=["name", "path", "value"],
    name=_STRUCTURED,
    path=path('Where to put the value, as a list of field names, such as ["owner"] or ["checked", "by"].'),
    value=anything("The value to put there."),
)
def note_structured_set(team, name, path, value):
    team.notes.structured(name).set(path, value)
    return {"name": name, "set": path}


@tool(
    "Takes one field out of a structured note.",
    required=["name", "path"],
    name=_STRUCTURED,
    path=path('Where the field is, as a list of field names, such as ["owner"].'),
)
def note_structured_remove(team, name, path):
    team.notes.structured(name).remove(path)
    return {"name": name, "removed": path}


@tool("Lists the names of the team's structured notes.")
def note_structured_list(team):
    return {"names": team.notes.structured_names()}


# ---- tasks -------------------------------------------------------------------


@tool(
    "Adds a task to the team's task board, for any agent to take.",
    required=["title"],
    title=text("What needs doing, in a few words."),
    notes=text("Anything more the agent that takes it should know."),
    parent=text("The id of a task this is one step of, to add it as a sub-task."),
    firm=flag(
        "True for work that must not be done twice by mistake, such as paying or sending something. "
        "Only one agent at a time can have such a task, and only that agent can finish it. "
        "Leave it out for ordinary work."
    ),
)
def task_add(team, title, notes=None, parent=None, firm=False):
    return {"task": _shown(team.tasks.add(title, notes=notes, parent=parent, firm=firm))}


@tool(
    "Lists the tasks on the team's board, oldest first. Each has an id, a title and a state: "
    '"todo" (free to take), "doing" (an agent is on it) or "done". '
    "A task marked abandoned was taken by an agent that has gone quiet: you may claim it. "
    "A long list comes a part at a time: the answer then says how to read on.",
    state={**text("Only list the tasks in this state."), "enum": list(STATES)},
    after=text("Only the tasks that come after the task with this id. The answer before says what to give."),
    limit=_limit("How many tasks to give at most."),
    names_only=flag("True to give only each task's id, title and state, which is shorter on a large team."),
)
def task_list(team, state=None, after=None, limit=None, names_only=False):
    tasks = team.tasks.list(state)
    if after is not None:
        # Found by when it was made, so a task that changed its state since
        # the last part was read still marks the place.
        mark = team.tasks.get(after)
        if mark is None:
            raise errors.UnknownTask(
                f"task_list: there is no task {after!r}. Give the id that next_after said, or leave after out"
            )
        tasks = [task for task in tasks if (task.created, task.id) > (mark.created, mark.id)]
    if names_only:
        shown = [(task.id, {"id": task.id, "title": task.title, "state": task.state}) for task in tasks]
    else:
        shown = [(task.id, _shown(task)) for task in tasks]
    taken, more = _page("task_list", shown, limit)
    return {"tasks": [one for _, one in taken], **more}


@tool("Shows one task as it is now, with its sub-tasks.", required=["task"], task=_TASK)
def task_get(team, task):
    # The task and its sub-tasks are read by themselves, not with the whole
    # board.
    found = team.tasks.get(task)
    if found is None:
        raise errors.UnknownTask(f"there is no task {task!r} on the board. task_list shows the ones there are")
    return {"task": _shown(found), "sub_tasks": [_shown(one) for one in team.tasks.children(found)]}


def _span(seconds):
    """A number of seconds in words, such as "5 minutes"."""
    if seconds >= 120:
        return f"{seconds // 60} minutes"
    return "1 second" if seconds == 1 else f"{seconds} seconds"


@tool(
    "Takes a task, so that other agents leave it alone. Always do this before starting work on a task. "
    "If another agent has it already, you are told so: pick a different task. "
    "A task whose agent has gone quiet is taken over.",
    required=["task"],
    task=_TASK,
)
def task_claim(team, task):
    before = team.tasks.get(task)
    claimed = team.tasks.claim(task)
    now = team.tasks.get(task)
    answer = {"claimed": claimed, "task": _shown(now)}
    if claimed:
        lease = now.lease or LEASE
        answer["keep_it"] = (
            f"Call task_still_working every {_span(max(lease // 3, 1))} or so while you work on it. "
            f"Another agent may take over a task you go quiet on for {_span(lease)}."
        )
        if before is not None and before.claimed_by not in (None, now.claimed_by):
            answer["took_over_from"] = before.claimed_by
    elif now.state == "done":
        answer["why"] = "it is already done"
    elif now.claimed_by is None and now.firm:
        answer["why"] = (
            "its ticket could not be had: another agent took it a moment ago, or the machine that holds it "
            "cannot be reached. Try again soon, or pick a different task"
        )
    elif now.claimed_by is None:
        # The task is read again after the claim, and things move on.
        answer["why"] = "another agent had it a moment ago. It is free now: claim it again"
    else:
        answer["why"] = f"{now.claimed_by} has it"
    return answer


@tool(
    "Says that you are still working on a task you took, so that no other agent takes it over. "
    "Call it now and then while you work. If it says the task is no longer yours, stop working on it.",
    required=["task"],
    task=_TASK,
)
def task_still_working(team, task):
    if team.tasks.still_working(task):
        return {"still_yours": True}
    now = team.tasks.get(task)
    if now.state == "done":
        why = "it is done. Stop working on it"
    elif now.claimed_by is None:
        why = "nobody has it now. Claim it again to carry on"
    else:
        why = f"{now.claimed_by} has it. Stop working on it"
    return {"still_yours": False, "why": why}


@tool(
    "Gives back a task you took and cannot finish, so that another agent can take it.",
    required=["task"],
    task=_TASK,
)
def task_release(team, task):
    released = team.tasks.release(task)
    answer = {"released": released}
    if not released:
        answer["why"] = "the task is not yours, or it is done"
    return answer


@tool(
    "Marks a task done and saves what came out of it for the team. Do this when you have finished a task you took.",
    required=["task"],
    task=_TASK,
    result=text("What came out of the work: the answer, a summary, or where the output is."),
    data=anything("What came out of the work as JSON, such as an object with fields, in place of result."),
)
def task_finish(team, task, result=None, data=None):
    result = _one_of("task_finish", result, data)
    finished = team.tasks.finish(task, result)
    now = team.tasks.get(task)
    answer = {"finished": finished, "task": _shown(now)}
    if not finished and now.state == "done":
        answer["why"] = "it was already done, and its result was left as it is"
    elif not finished:
        answer["why"] = "it is a firm task, and only the agent that has it can finish it. Claim it first"
    return answer


@tool(
    "Changes a task's title or notes, or puts it in another state by hand. "
    'Setting the state to "todo" frees a task that an agent took and then abandoned.',
    required=["task"],
    task=_TASK,
    title=text("A new title."),
    notes=text("New notes, in place of the old ones."),
    state={**text("A new state."), "enum": list(STATES)},
)
def task_update(team, task, title=None, notes=None, state=None):
    return {"task": _shown(team.tasks.update(task, title=title, notes=notes, state=state))}


@tool(
    "Moves a task to be a sub-task of another, or makes it a top-level task again if you leave out under.",
    required=["task"],
    task=_TASK,
    under=text("The id of the task to put it under."),
)
def task_move(team, task, under=None):
    moved = team.tasks.move(task, under)
    answer = {"moved": moved, "task": _shown(team.tasks.get(task))}
    if not moved:
        answer["why"] = "a task cannot be put under itself or under one of its own sub-tasks"
    return answer


# ---- budgets -----------------------------------------------------------------


@tool("Lists the team's budgets and how much is left in each.")
def budget_list(team):
    names = team.budgets()
    if not names:
        return {"budgets": {}}
    # Every amount is asked for in one message.
    with team.client.batch() as batch:
        left = {name: batch.bounded_counter(_names.budget(team.name, name)).value() for name in names}
    return {"budgets": {name: amount.value for name, amount in left.items()}}


def _known(team, shared):
    """Raises if nothing was ever put in the budget. Such a budget reads as
    0, like one that has run out, and a name typed wrongly would then look
    like a team with nothing left."""
    if shared.name not in team.budgets():
        raise errors.BadRequest(f"there is no budget {shared.name!r}. budget_list shows the ones there are")


@tool("Says how much is left in one of the team's budgets.", required=["budget"], budget=_BUDGET)
def budget_left(team, budget):
    shared = team.budget(budget)
    left = shared.left()
    if left == 0:
        _known(team, shared)
    return {"budget": budget, "left": left}


@tool(
    "Takes an amount out of a budget the team shares, before doing something that costs that much. "
    "If there is not enough left, nothing is taken and you are told so: do not go ahead with the work.",
    required=["budget", "amount"],
    budget=_BUDGET,
    amount=number("How much to take out. A whole number above zero."),
)
def budget_spend(team, budget, amount):
    shared = team.budget(budget)
    spent, left = shared.spend(amount), shared.left()
    if not spent and left == 0:
        _known(team, shared)
    return {"spent": spent, "left": left}


@tool(
    "Says how much each agent has taken out of one of the team's budgets.",
    required=["budget"],
    budget=_BUDGET,
)
def budget_spent(team, budget):
    shared = team.budget(budget)
    spent = shared.spent_by()
    if not spent:
        _known(team, shared)
    return {"budget": budget, "spent_by": spent}


@tool(
    "Adds an amount to a budget the team shares. A budget that does not exist yet is created.",
    required=["budget", "amount"],
    budget=_BUDGET,
    amount=number("How much to add. A whole number above zero."),
)
def budget_top_up(team, budget, amount):
    shared = team.budget(budget)
    shared.top_up(amount)
    return {"left": shared.left()}


# ---- messages ----------------------------------------------------------------


_EVERYONE = "everyone"


def means_everyone(name):
    """True if the tools read `name` as all the agents: "everyone", in any
    case, with spaces around it or not."""
    return isinstance(name, str) and name.strip().casefold() == _EVERYONE


def _letter(message):
    """A message as the agent sees it."""
    shown = {"from": message.sender, "to": message.to or _EVERYONE, "text": message.body, "sent": message.sent}
    return {name: value for name, value in shown.items() if value is not None}


@tool(
    "Sends a message to another agent on the team, or to all of them. It waits in their inbox until "
    "they next read it. Use it to hand something over, to ask, or to say what you found. "
    "Send text, or JSON as data.",
    text=text("What to say."),
    data=anything("What to send as JSON, such as an object with fields, in place of text."),
    to=text(
        "The name of the agent to send it to, as roster_list shows names. "
        'Leave it out, or write "everyone", to send to all of them.'
    ),
)
def inbox_send(team, text=None, to=None, data=None):
    text = _one_of("inbox_send", text, data)
    if text is None:
        raise errors.BadRequest("inbox_send needs 'text' or 'data'")
    # A message to all is shown to agents as sent to "everyone", so an agent
    # that answers one writes that word. It means all of them.
    if to is None or means_everyone(to):
        team.inbox.send_all(text)
        return {"sent_to": _EVERYONE}
    team.inbox.send(to, text)
    answer = {"sent_to": to}
    try:
        members = [member.name for member in team.roster.members()]
    except errors.NotAllowed:
        # An agent that may not read the roster cannot be told whether the
        # name is on it. The message was sent all the same.
        return answer
    if to not in members:
        # It is kept all the same: the agent may start later. But a name
        # typed wrongly should not pass for a message that arrived.
        answer["note"] = (
            f"nobody called {to!r} is on the roster. The message waits in case an agent "
            f"of that name starts. roster_list shows who is there"
        )
    return answer


@tool(
    "Reads the messages that were sent to you, or to everyone, and that you have not been given yet. "
    "Each is given once, also if you start again later. Read this when you start and now and then "
    "while you work."
)
def inbox_read(team):
    return {"messages": [_letter(message) for message in team.inbox.unread()]}


@tool(
    "Reads the recent messages to you and to everyone again, the ones you have already been given "
    "included, oldest first."
)
def inbox_recent(team):
    return {"messages": [_letter(message) for message in team.inbox.messages()]}


# ---- documents ---------------------------------------------------------------

_DOCUMENT = text('The document\'s name, such as "q3-report". doc_list shows the names. '
                 "Use letters, digits, dots, dashes and underscores.")


@tool("Lists the documents the team is writing together.")
def doc_list(team):
    return {"documents": team.documents()}


@tool(
    "Reads a document the team is writing together. It comes in parts, in order, each with the name "
    "of the agent that wrote it. Read one after the other, the parts are the whole text.",
    required=["document"],
    document=_DOCUMENT,
)
def doc_read(team, document):
    page = team.document(document)
    parts = [{"text": part, "by": who} if who else {"text": part} for part, who in page.written_by()]
    if not parts and document not in team.documents():
        # A document nobody has written reads as empty. A name typed wrongly
        # would look like a report with nothing in it.
        raise errors.BadRequest(f"there is no document {document!r}. doc_list shows the ones there are")
    return {"document": document, "parts": parts}


@tool(
    "Adds text to the end of a document the team is writing together. A document that does not exist "
    "yet is started. What you add is marked with your name. Start with a new line if it is a new paragraph.",
    required=["document", "text"],
    document=_DOCUMENT,
    text=text("The text to add."),
)
def doc_append(team, document, text):
    page = team.document(document)
    if text == "":
        raise errors.BadRequest("doc_append: there is nothing to add: 'text' is empty")
    page.append(text)
    return {"document": document, "added": len(text)}


def _place(tool, document, now, piece):
    """Where `piece` starts in the text `now`. It has to be there once. Two
    places count as two even where they overlap, as "aba" does in "ababa"."""
    found = sum(now.startswith(piece, start) for start in range(len(now))) if piece else 0
    if found == 0:
        raise errors.BadRequest(f"{tool}: the document {document!r} does not hold {piece!r}. doc_read shows what it holds")
    if found > 1:
        raise errors.BadRequest(
            f"{tool}: {piece!r} is in the document {found} times. Give more of the text around it, or say where with 'at'"
        )
    return now.index(piece)


@tool(
    "Puts text into a document the team is writing together, after a piece of text that is in it or at a "
    "position. What you put in is marked with your name.",
    required=["document", "text"],
    document=_DOCUMENT,
    text=text("The text to put in."),
    after=text("Text in the document to put it right after. It has to be in the document once."),
    at=number("Where to put it, counted in characters from the start. 0 is the start."),
)
def doc_insert(team, document, text, after=None, at=None):
    if text == "":
        raise errors.BadRequest("doc_insert: there is nothing to put in: 'text' is empty")
    if after is not None and at is not None:
        raise errors.BadRequest("doc_insert: give 'at' or 'after', not both")
    if after is None and at is None:
        raise errors.BadRequest("doc_insert: say where the text goes, with 'after' or with 'at'")
    page = team.document(document)
    if after is not None:
        at = _place("doc_insert", document, page.text(), after) + len(after)
    elif at < 0:
        raise errors.BadRequest(f"doc_insert: 'at' is 0 or more, not {at}")
    page.insert(at, text)
    return {"document": document, "added": len(text)}


@tool(
    "Takes text out of a document the team is writing together: a piece of text that is in it once, or a "
    "number of characters from a position.",
    required=["document"],
    document=_DOCUMENT,
    text=text("The text to take out. It has to be in the document once."),
    at=number("Where to start taking out, counted in characters from the start. Give it with length."),
    length=number("How many characters to take out from at."),
)
def doc_remove(team, document, text=None, at=None, length=None):
    if document not in team.documents():
        raise errors.BadRequest(f"there is no document {document!r}. doc_list shows the ones there are")
    page = team.document(document)
    now = page.text()
    if text is not None:
        if at is not None or length is not None:
            raise errors.BadRequest("doc_remove: give the text to take out, or 'at' and 'length', not both")
        if text == "":
            raise errors.BadRequest("doc_remove: there is nothing to take out: 'text' is empty")
        at, length = _place("doc_remove", document, now, text), len(text)
    elif at is None or length is None:
        raise errors.BadRequest("doc_remove: say what to take out, with 'text', or with 'at' and 'length'")
    elif at < 0 or length < 1 or at + length > len(now):
        raise errors.BadRequest(
            f"doc_remove: the document is {len(now)} characters long, so it has no {length} characters from {at}"
        )
    page.delete(at, length)
    return {"document": document, "removed": now[at : at + length]}


@tool(
    "Finds what the team already knows about something: the notes, the paragraphs of documents and the tasks "
    "whose meaning is closest to what you ask, the closest first, each with a score from 0 to 1. A task is "
    "found by its title and notes, with its id as its name. Messages are not searched. It goes by meaning, "
    "not by exact words, so ask the way you would ask a colleague. Use it before starting work, to build on "
    "what others found.",
    required=["query"],
    query=text('What to look for, as a question or a few words, such as "what is the refund policy?".'),
    limit=number("How many to find, from 1 to 50. Without it, 5."),
)
def search(team, query, limit=5):
    found = [dataclasses.asdict(each) for each in team.search(query, limit)]
    return {"found": [{name: value for name, value in each.items() if value is not None} for each in found]}


# ---- tallies -----------------------------------------------------------------

_TALLY = text('The tally\'s name, such as "errors". tally_list shows the names. '
              "Use letters, digits, dots, dashes and underscores.")


@tool("Lists the things the team keeps count of.")
def tally_list(team):
    return {"tallies": team.tallies()}


@tool(
    "Says how much has been counted on one of the team's tallies, in total and under each name.",
    required=["tally"],
    tally=_TALLY,
)
def tally_read(team, tally):
    counted = team.tally(tally)
    by_name = counted.by_name()
    if not by_name and tally not in team.tallies():
        raise errors.BadRequest(f"there is no tally {tally!r}. tally_list shows the ones there are")
    return {"tally": tally, "total": sum(by_name.values()), "by_name": by_name}


@tool(
    "Counts something on one of the team's tallies, such as an error seen or tokens used. A tally only "
    "goes up. It is counted under your name unless you say what to count it under.",
    required=["tally"],
    tally=_TALLY,
    amount=number("How many to count. A whole number above zero. 1 if left out."),
    under=text('What to count it under in place of your name, such as the kind of error: "timeout".'),
)
def tally_add(team, tally, amount=1, under=None):
    counted = team.tally(tally)
    counted.add(amount, under=under)
    return {"tally": tally, "total": counted.total()}


# ---- who is here -------------------------------------------------------------


@tool(
    "Lists the agents on the team's roster and says which of them are here now. An agent that is "
    "not here may have stopped: a task it took may need to be put back."
)
def roster_list(team):
    return {"agents": [{"name": member.name, "here": member.here} for member in team.roster.members()]}



# ---- plain shared objects ----------------------------------------------------

_OBJECT = text(
    'The object\'s name, as the programs that use it call it, such as "visits". Names that start with '
    '"orvu/" belong to the team\'s notes, tasks and the rest, and are reached through their own tools.'
)

#: How each kind of plain object is read, by the name object_read takes.
_READS = {
    "counter": lambda client, name: client.counter(name).value(),
    "register": lambda client, name: client.register(name).get(),
    "set": lambda client, name: sorted(client.set(name).elements()),
    "map": lambda client, name: client.map(name).entries(),
    "cart": lambda client, name: client.cart(name).items(),
    "list": lambda client, name: client.list(name).to_list(),
    "json": lambda client, name: client.json(name).to_value(),
}


def _plain(name):
    """`name`, if it can be a plain object's. The team's own objects are
    under "orvu/", and changing them by hand could break the notes, tasks
    and the rest, so they are kept out of reach here."""
    if name == "" or name.startswith("orvu/"):
        raise errors.BadRequest(
            f"{name!r} is not a plain object's name: names that start with \"orvu/\" belong to the team's "
            "notes, tasks and the rest. Use their tools"
        )
    return name


@tool(
    "Reads a plain shared object, one that programs on the team keep under a name of their own. "
    "Reading one that was never written gives an empty value.",
    required=["kind", "name"],
    kind={**text("What kind of object it is."), "enum": list(_READS)},
    name=_OBJECT,
)
def object_read(team, kind, name):
    return {"kind": kind, "name": name, "value": _READS[kind](team.client, _plain(name))}


@tool(
    "Adds to a shared counter, or takes away from it with a number below zero. Says what it is after.",
    required=["name", "amount"],
    name=_OBJECT,
    amount=number("How much to add. Below zero to take away."),
)
def object_counter_add(team, name, amount):
    counter = team.client.counter(_plain(name))
    if amount == 0:
        raise errors.BadRequest("object_counter_add: 'amount' cannot be 0")
    if amount > 0:
        counter.increment(amount)
    else:
        counter.decrement(-amount)
    return {"name": name, "value": counter.value()}


@tool(
    "Sets a shared register, one text value. If two agents set it at the same time, the later one wins.",
    required=["name", "value"],
    name=_OBJECT,
    value=text("The new value."),
)
def object_register_set(team, name, value):
    team.client.register(_plain(name)).set(value)
    return {"name": name, "value": value}


@tool(
    "Adds a text item to a shared set.",
    required=["name", "member"],
    name=_OBJECT,
    member=text("The item to add."),
)
def object_set_add(team, name, member):
    team.client.set(_plain(name)).add(member)
    return {"name": name, "added": member}


@tool(
    "Takes a text item out of a shared set.",
    required=["name", "member"],
    name=_OBJECT,
    member=text("The item to take out."),
)
def object_set_remove(team, name, member):
    team.client.set(_plain(name)).remove(member)
    return {"name": name, "removed": member}


@tool(
    "Sets a key of a shared map to a text value. For one key, the later write wins.",
    required=["name", "key", "value"],
    name=_OBJECT,
    key=text("The key."),
    value=text("The value."),
)
def object_map_put(team, name, key, value):
    team.client.map(_plain(name)).put(key, value)
    return {"name": name, "key": key}


@tool(
    "Takes a key out of a shared map.",
    required=["name", "key"],
    name=_OBJECT,
    key=text("The key."),
)
def object_map_remove(team, name, key):
    team.client.map(_plain(name)).remove(key)
    return {"name": name, "removed": key}


@tool(
    "Puts a text item into a shared list, at the end or at a position.",
    required=["name", "value"],
    name=_OBJECT,
    value=text("The item."),
    at=number("Where to put it, counted from 0 at the start. Leave it out to add at the end."),
)
def object_list_insert(team, name, value, at=None):
    shared = team.client.list(_plain(name))
    if at is None:
        at = shared.len()
    shared.insert(at, value)
    return {"name": name, "at": at}


@tool(
    "Takes the item at a position out of a shared list.",
    required=["name", "at"],
    name=_OBJECT,
    at=number("The item's position, counted from 0 at the start."),
)
def object_list_remove(team, name, at):
    team.client.list(_plain(name)).delete(at)
    return {"name": name, "removed_at": at}


@tool(
    "Sets what is at a place in a shared JSON document to any JSON value. Objects on the way are made if "
    "they are not there.",
    required=["name", "path", "value"],
    name=_OBJECT,
    path=path('Where, as a list of field names, such as ["limits", "tokens"].'),
    value=anything("The value to put there."),
)
def object_json_set(team, name, path, value):
    team.client.json(_plain(name)).set(path, value)
    return {"name": name, "set": path}


@tool(
    "Takes what is at a place out of a shared JSON document.",
    required=["name", "path"],
    name=_OBJECT,
    path=path('Where, as a list of field names, such as ["limits", "tokens"].'),
)
def object_json_remove(team, name, path):
    team.client.json(_plain(name)).remove(path)
    return {"name": name, "removed": path}
