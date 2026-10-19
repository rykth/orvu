"""How the building blocks name the shared objects they keep their data in.

Every object a team uses is called `orvu/<team>/<block>...`. Each block has a
first word of its own after the team's name, and no name a caller gives may
contain a slash, so two blocks can never end up using the same object. Names
that do not start with `orvu/` are left to programs that use the plain
objects directly.
"""

import re
import urllib.parse

from . import errors

PREFIX = "orvu"

#: The largest amount or count that can be sent to the sidecar in one go.
LARGEST = 2**63 - 1

# Letters, digits, dots, dashes and underscores, starting with a letter or a
# digit, and at most 64 characters.
_PART = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}\Z")


def part(text, what):
    """Checks that `text` can be one part of an object's name, and returns it.
    `what` says what it names, for the error message."""
    if not isinstance(text, str) or not _PART.match(text):
        raise errors.BadRequest(
            f"{text!r} cannot be used as {what}. Use letters, digits, dots, dashes and "
            f"underscores, starting with a letter or a digit, and at most 64 characters"
        )
    return text


def index(team):
    """The list of what the team has."""
    return f"{PREFIX}/{team}/index"


def notes(team):
    """The team's simple notes. They are kept in a map that keeps values
    written at once; the sidecar moves notes kept the old way over to it."""
    return f"{PREFIX}/{team}/notes-mv"


def note(team, name):
    """One structured note."""
    return f"{PREFIX}/{team}/note/{name}"


def document(team, name):
    """One document."""
    return f"{PREFIX}/{team}/doc/{name}"


def tally(team, name):
    """One tally."""
    return f"{PREFIX}/{team}/tally/{name}"


def roster(team):
    """Who is on the team, and when each was last seen."""
    return f"{PREFIX}/{team}/roster"


#: The inbox every agent reads. An agent's own inbox is kept under its name
#: with every character that is not a letter or a digit written as a code,
#: "@" among them, so this is never somebody's own inbox.
EVERYONE = "@everyone"


def inbox_owner(agent_name):
    """The part of an object's name that stands for an agent's inbox. An
    agent's name can be any text, and this is the same text made safe to be
    one part of a name."""
    return urllib.parse.quote(agent_name, safe="")


def inbox_page(team, owner, page):
    """One page of an inbox. `owner` is what `inbox_owner` gives for an
    agent, or `EVERYONE`."""
    return f"{PREFIX}/{team}/inbox/{owner}/{page}"


def inbox_given(team, reader, owner, page):
    """The messages on one page of an inbox that one agent has been given.
    `reader` and `owner` are what `inbox_owner` gives, or `EVERYONE`."""
    return f"{PREFIX}/{team}/inbox-given/{reader}/{owner}/{page}"


def inbox_count(team, owner):
    """How many messages were written to each page of an inbox, by page
    number. A sender adds one in the same message as it writes."""
    return f"{PREFIX}/{team}/inbox-count/{owner}"


def inbox_done(team, reader, owner):
    """How many messages one agent has been given from each page of an
    inbox, by page number."""
    return f"{PREFIX}/{team}/inbox-done/{reader}/{owner}"


def inbox_pages(team, owner):
    """The number of the page an inbox is on."""
    return f"{PREFIX}/{team}/inbox-page/{owner}"


def spent(team, name):
    """How much of one budget each agent has spent."""
    return f"{PREFIX}/{team}/budget-spent/{name}"


def topped(team, name):
    """How much each agent has topped one budget up by."""
    return f"{PREFIX}/{team}/budget-topped/{name}"


def agent(text, what="the name of an agent"):
    """Checks that `text` can be an agent's name, and returns it. Any text
    will do, as for `Team.connect(me=...)`."""
    if not isinstance(text, str) or not text:
        raise errors.BadRequest(f"{text!r} cannot be used as {what}. It has to be some text")
    return text


def tasks(team):
    """The team's task board."""
    return f"{PREFIX}/{team}/tasks"


def task_tree(team):
    """Which task is a sub-task of which."""
    return f"{PREFIX}/{team}/tasks-tree"


def task_ticket(team, task_id, number):
    """The ticket of a firm task. Whoever takes it has the task. A task gets
    a new ticket, with the next number, each time it is taken over or set
    back by hand."""
    return f"{PREFIX}/{team}/task-ticket/{task_id}/{number}"


def task_takeover(team, task_id, number):
    """The ticket for taking a firm task over from an agent that went quiet,
    or setting it back by hand, while it is on ticket `number`. Only one
    agent can, and that agent makes the next ticket."""
    return f"{PREFIX}/{team}/task-takeover/{task_id}/{number}"


def budget(team, name):
    """One budget."""
    return f"{PREFIX}/{team}/budget/{name}"
