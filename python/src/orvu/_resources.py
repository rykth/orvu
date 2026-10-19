"""What the MCP server offers besides tools: the agent's inbox, the team's
tasks and its notes as resources to read, word when one of them changes, and
a few prompts.

A resource is read as JSON, the same as the tool that reads it answers. An
agent's program that subscribes to one is told each time it changes. Each
subscription is followed in a thread of its own, on a connection of its own,
and ends at once when the program unsubscribes or goes away.
"""

import dataclasses
import threading

from . import _follow, _tasks, _tools, errors
from ._team import Team


@dataclasses.dataclass(frozen=True)
class Resource:
    uri: str
    name: str
    title: str
    description: str
    #: The group of tools this belongs with. It is offered when they are.
    group: str
    #: The resource as JSON, read from a team.
    read: object
    #: For following it: what is followed, as `(key, handles)`. When the key
    #: read in the snapshot is no longer the one the handles were picked
    #: by, they are picked again.
    handles: object
    #: How long to wait before looking again with no news, or `None`.
    later: object = None


def _inbox(team):
    return _tools.inbox_recent(team)


def _inbox_handles(team):
    """What the inbox is followed by: the page numbers it was picked by,
    and the handles."""
    # The newest pages of each inbox the agent reads, as `messages` reads
    # them, and the page numbers, which go up when an inbox moves on to a
    # new page.
    inbox = team.inbox
    owners = inbox._mine()
    numbers = tuple(inbox._newest(owner) for owner in owners)
    handles = [inbox._pages(owner) for owner in owners]
    for owner, newest in zip(owners, numbers):
        handles += [inbox._page(owner, n) for n in range(max(newest - inbox.RECENT + 1, 0), newest + 1)]
    return numbers, handles


def _inbox_key(team):
    return tuple(team.inbox._newest(owner) for owner in team.inbox._mine())


def _tasks_read(team):
    return {"tasks": [_tools._shown(task) for task in team.tasks.list()]}


def _tasks_later(team):
    # A task kept out of sight is shown when its time is over, even if
    # nothing on the board changed by then.
    due = team.tasks._due
    return None if due is None else max(due - _tasks._now(), 0) / 1000


def _notes_read(team):
    notes = team.notes.all()
    return {
        "notes": {
            name: _tools._clashed(value) if isinstance(value, errors.Clash) else value
            for name, value in sorted(notes.items())
        }
    }


RESOURCES = {
    resource.uri: resource
    for resource in (
        Resource(
            "orvu://inbox",
            "inbox",
            "Your inbox",
            "The recent messages to you and to everyone, oldest first, as inbox_recent gives them. Reading "
            "this does not count as being given them: inbox_read still gives each once.",
            "inbox",
            _inbox,
            _inbox_handles,
        ),
        Resource(
            "orvu://tasks",
            "tasks",
            "The team's tasks",
            "Every task on the team's board, oldest first, as task_list gives them.",
            "tasks",
            _tasks_read,
            lambda team: (None, [team.tasks._board, team.tasks._tree]),
            _tasks_later,
        ),
        Resource(
            "orvu://notes",
            "notes",
            "The team's notes",
            "Every one of the team's notes, by name, as note_list gives them.",
            "notes",
            _notes_read,
            lambda team: (None, [team.notes._map]),
        ),
    )
}


def _key(resource, team):
    """What the handles of `resource` are picked by, as it is now."""
    return _inbox_key(team) if resource.uri == "orvu://inbox" else None


@dataclasses.dataclass(frozen=True)
class Prompt:
    name: str
    description: str
    group: str
    text: str


PROMPTS = {
    prompt.name: prompt
    for prompt in (
        Prompt(
            "check_inbox",
            "Read the messages other agents sent you, and act on them.",
            "inbox",
            "Read your messages with inbox_read. For each one, decide whether it asks something of you, "
            "and answer or act on it. Use inbox_send to reply to its sender.",
        ),
        Prompt(
            "pick_a_task",
            "Find a task on the team's board that nobody has, take it, and start on it.",
            "tasks",
            "List the tasks that are free with task_list and state todo, and pick the one you are best "
            "placed to do. Take it with task_claim before you start. If another agent has it, pick "
            "another. While you work, call task_still_working now and then, and close the task with "
            "task_finish when you are done.",
        ),
    )
}


class Follower:
    """Follows one resource for one subscription, in a thread of its own,
    and calls `changed()` each time what it reads is different."""

    def __init__(self, resource, team, changed):
        self._resource = resource
        # A connection of its own: two threads on one connection get in
        # each other's way when the sidecar restarts and both start it again.
        self._team = Team(team.client.another(), team.name, team.me)
        self._changed = changed
        self._stop = _follow.Stop()
        # What it reads now, so that a change from here on is told even if
        # it comes before the thread has started.
        try:
            self._last = resource.read(self._team)
        except BaseException:
            self._team.client.close()
            raise
        self._thread = threading.Thread(target=self._run, name=f"orvu-follow {resource.uri}", daemon=True)
        self._thread.start()

    def _run(self):
        resource, team = self._resource, self._team
        try:
            while not self._stop.is_set():
                try:
                    key, handles = resource.handles(team)

                    def snapshot():
                        return resource.read(team), _key(resource, team)

                    later = (lambda: resource.later(team)) if resource.later else None
                    for value, now_key in _follow.together(team.client, handles, snapshot, later, self._stop):
                        if value != self._last:
                            self._last = value
                            self._changed()
                        if now_key != key:
                            break
                    else:
                        # The connection ended, or this was stopped. Started
                        # again after a moment, unless it was stopped.
                        self._stop.wait(1.0)
                except Exception:  # noqa: BLE001
                    # The sidecar is away, or restarting. Tried again after a
                    # moment: a follower that gave up would never tell again.
                    self._stop.wait(1.0)
        finally:
            team.client.close()

    def stop(self, wait=5.0):
        """Ends the following and lets go of its connection."""
        self._stop.set()
        if self._thread is not threading.current_thread():
            self._thread.join(timeout=wait)
