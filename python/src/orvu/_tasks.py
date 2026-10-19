"""The task board: what needs doing, who is on it, and how it turned out."""

import dataclasses
import math
import secrets
import time

from . import _follow, _names
from . import errors

#: The states a task can be in.
STATES = ("todo", "doing", "done")

# The top of the tree of tasks. A task with no parent task hangs here.
_ROOT = "root"

# How many tasks are read in one message. A message holds at most 1,024
# operations, and each task takes two.
_AT_ONCE = 500

#: How long a claim lasts, in seconds, without word from the agent that made
#: it, unless the claim says otherwise.
LEASE = 15 * 60

# How long, in milliseconds, a task on the board with no place in the tree is
# kept out of sight after it was added.
_GRACE = 30_000

# The longest a claim can be made to last: a year.
_LONGEST_LEASE = 365 * 24 * 60 * 60

# How far a firm task set back with force jumps past the highest ticket it
# can find: a random number of at least this, and less than twice it. See
# the notes on tickets below.
_JUMP = 2**31


@dataclasses.dataclass(frozen=True)
class Task:
    """One task as it was when it was read. It does not change afterwards:
    read it again to see what happened since."""

    #: The task's id, such as "t-3f9a1c2e5b7d4a60". Made up when the task is
    #: added.
    id: str
    title: str
    #: "todo", "doing" or "done".
    state: str
    #: The agent working on it, or `None`.
    claimed_by: str | None
    #: What `finish` was given, or `None`.
    result: object
    #: Free text about the task, or `None`.
    notes: str | None
    #: The id of the task this is a sub-task of, or `None`.
    parent: str | None
    #: When it was added, in milliseconds since 1970, by the clock of the
    #: machine that added it.
    created: int
    #: The agent that added it, or `None`.
    created_by: str | None
    #: The agent that finished it, or `None`.
    finished_by: str | None
    #: The agent that last changed its title, notes or state by hand with
    #: `update`, or `None`. For an agent with rules, the sidecar records this
    #: itself: the agent that last changed anything in the task.
    updated_by: str | None = None
    #: Whether only one agent at a time can hold it. See `Tasks.add`.
    firm: bool = False
    #: For a firm task, the number of its ticket. It goes up by one each time
    #: the task is given back, taken over or set back by hand.
    ticket: int = 0
    #: When the agent on it last said it was still working, in milliseconds
    #: since 1970 by that agent's clock, or `None`. It is there to be shown.
    #: Whether the claim has run out is not worked out from it: see
    #: `abandoned`.
    seen: int | None = None
    #: How many seconds the claim lasts without word from its agent, or
    #: `None`.
    lease: int | None = None
    #: When this program first saw the claim as it is now, in milliseconds by
    #: this program's own steady clock (`time.monotonic()`), or `None` if
    #: nobody is on the task. Any change to the claim, such as the agent on it
    #: saying it is still working, starts it again. It means nothing to
    #: another program.
    quiet_since: float | None = dataclasses.field(default=None, compare=False)
    #: The last time the task was taken over from an agent that had gone
    #: quiet, as `{"by": ..., "from": ..., "at": ...}`: the agent that took
    #: it, the agent it was taken from, and when, in milliseconds since 1970
    #: by the clock of the agent that took it. `None` if it never was. It is
    #: written on the task, so every agent that reads or watches the board
    #: sees it, and it stays after the task is given back or finished.
    taken_over: dict | None = None

    def abandoned(self, now=None):
        """Whether the agent on this task has gone quiet for longer than its
        claim lasts, so that another agent may take the task over.

        It goes by this program's own clock only. The claim has run out when
        this program has seen no word from the agent on the task, no change
        to its claim at all, for as long as the claim lasts. The time the
        agent wrote is not compared with this machine's clock, so machines
        whose clocks are far apart still agree on how long a claim lasts. The
        price: a program that has just started has seen nothing yet, and
        waits a whole lease before it takes any task over.

        `now` is a reading of the same steady clock as `quiet_since`, in
        milliseconds, and is the moment of asking if left out."""
        if self.state != "doing" or self.claimed_by is None or self.seen is None or self.lease is None:
            return False
        if self.quiet_since is None:
            return False
        now = _steady() if now is None else now
        return now - self.quiet_since > self.lease * 1000


def _id(task):
    """The id, given a `Task` or an id."""
    return task.id if isinstance(task, Task) else task


def _whole_number(value):
    """A whole number read from the board, or `None` if it is not one. JSON
    numbers can come back as 3.0 for 3."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if isinstance(value, float) and (not math.isfinite(value) or not value.is_integer()):
        return None
    return int(value)


# Ticket numbers are below this: fifteen digits at most. Both libraries read
# every such number exactly the same, and a force never goes past it.
_MOST = 10**15


def _ticket_key(key):
    """The number a key under "tickets", or a line of the team's list, names,
    or `None` if it is not one to fifteen plain digits. Anything else is
    left out, so that nothing written into a task can stop it being read."""
    if isinstance(key, str) and 1 <= len(key) <= 15 and all("0" <= c <= "9" for c in key):
        return int(key)
    return None


def _now():
    return int(time.time() * 1000)


def _steady():
    """This program's steady clock, in milliseconds. It never goes back, and
    is not changed when the machine's clock is set."""
    return time.monotonic() * 1000


def _mark():
    """Tells one finish from another, so that an agent can see whether the
    finish kept on the board is its own."""
    return secrets.token_hex(8)


def _whole(fields):
    """Whether a task has arrived far enough to be shown. A task another
    machine is still in the middle of adding may be on the board without its
    title yet."""
    return isinstance(fields, dict) and "title" in fields


def _finished(fields):
    """The result of a finished task and the agent that finished it. They are
    kept together under the task's "finished" mark, so that two agents that
    finish it at once cannot leave the result of one beside the name of the
    other. A task finished before that was so has them in fields of their
    own."""
    mark = fields.get("finished")
    if isinstance(mark, dict):
        return mark.get("result"), mark.get("by")
    return fields.get("result"), fields.get("finished_by")


def _takeover(value):
    """The record of a takeover read from the board, or `None` if it is not
    one."""
    if not isinstance(value, dict):
        return None
    by, taken_from, at = value.get("by"), value.get("from"), _whole_number(value.get("at"))
    if not isinstance(by, str) or not isinstance(taken_from, str) or at is None:
        return None
    return {"by": by, "from": taken_from, "at": at}


def _marked(value):
    """Whether a task's "finished" mark is there: anything but nothing, null
    or false counts, as it does in the TypeScript library."""
    return value is not None and value is not False


def _text(value):
    """A text read from the board, or `None` if it is not one."""
    return value if isinstance(value, str) else None


def _time(value):
    """When a task was added: any finite number, and 0 for anything else, so
    that tasks can always be put in order."""
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return 0
    return value


def _ticket_number(fields):
    """The number of a firm task's ticket: the highest one ever written on
    it. Each number is written down as a key of its own under "tickets", and
    keys written on different machines are all kept when they meet, so the
    highest can only go up. A number written late, by an agent that had not
    heard the task moved on, lands below it and changes nothing. "ticket" is
    the number as it was last written, kept for display and for tasks
    written before there were keys."""
    highest = _whole_number(fields.get("ticket"))
    highest = highest if highest is not None and 0 <= highest < _MOST else 0
    written = fields.get("tickets")
    if isinstance(written, dict):
        for key in written:
            number = _ticket_key(key)
            if number is not None:
                highest = max(highest, number)
    return highest


def _put_ticket(board, task_id, number):
    """Writes a firm task's new ticket number on the board, in a batch."""
    board.set([task_id, "ticket"], number)
    board.set([task_id, "tickets", str(number)], True)


def _task(task_id, fields, parent, present):
    """A `Task` made from what the board holds for it, or `None` if it is not
    whole yet. Its parent is named only if it is in `present`, the tasks
    known to be whole."""
    if not _whole(fields):
        return None
    result, finished_by = _finished(fields)
    # Any program with the right rules can write anything into a task. What
    # is not what it should be reads as a safe value instead, the same in
    # both libraries, so that reading the board never fails because of it.
    title = fields["title"]
    state = fields.get("state")
    return Task(
        id=task_id,
        title=title if isinstance(title, str) else "",
        # A finished task has a mark of its own, apart from its state. A claim
        # that was on its way while the task was being finished writes the
        # state, and cannot undo the mark.
        # Anything there but nothing, null or false counts as the mark.
        state="done" if _marked(fields.get("finished")) else state if state in STATES else "todo",
        claimed_by=_text(fields.get("claimed_by")),
        result=result,
        notes=_text(fields.get("notes")),
        parent=parent if parent in present else None,
        created=_time(fields.get("created")),
        created_by=_text(fields.get("created_by")),
        finished_by=_text(finished_by),
        updated_by=_text(fields.get("updated_by")),
        firm=fields.get("firm") is True,
        ticket=_ticket_number(fields),
        seen=_whole_number(fields.get("seen")),
        lease=_whole_number(fields.get("lease")),
        taken_over=_takeover(fields.get("taken_over")),
    )


def _sent(batch):
    """Makes sure every write in a batch that was sent went through. A batch
    does not stop at a write that failed, so each one is looked at, and the
    first failure is raised."""
    for pending in batch.results:
        pending.value


class Tasks:
    """The team's task board. Get it from `team.tasks`.

        task = team.tasks.add("Summarise the Q3 report")
        if team.tasks.claim(task):
            team.tasks.finish(task, result="It went up.")

    Wherever a task is asked for, a `Task` or its id will do.
    """

    #: How many seconds a task on the board with no place in the tree yet
    #: is kept out of sight. A task is put in the tree first and on the board
    #: after, but another machine may hear of the two in either order, and
    #: shown before its place in the tree arrives it would be at the top and
    #: under its parent a moment later. A task that never gets a place, from
    #: a program that stopped half way through adding it, is shown at the top
    #: once this time is over.
    GRACE = _GRACE // 1000

    def __init__(self, team):
        self._team = team
        self._board = team.client.json(_names.tasks(team.name))
        self._tree = team.client.tree(_names.task_tree(team.name))
        # When the next task kept out of sight is to be shown, in
        # milliseconds since 1970, or `None`.
        self._due = None
        # For each task someone is on, the claim as this program last saw it
        # and when, by the steady clock, it first saw it so. That is what
        # tells whether the claim has run out.
        self._heard = {}

    def _noted(self, task):
        """The task, with when this program first saw its claim as it is
        now."""
        if task.state != "doing" or task.claimed_by is None or task.seen is None or task.lease is None:
            self._heard.pop(task.id, None)
            return task
        claim = (task.claimed_by, task.seen, task.lease, task.ticket)
        heard = self._heard.get(task.id)
        if heard is None or heard[0] != claim:
            heard = (claim, _steady())
            self._heard[task.id] = heard
        return dataclasses.replace(task, quiet_since=heard[1])

    # ---- reading -------------------------------------------------------------

    def _read(self):
        """Every task, by id. Used where every task is wanted."""
        # Two messages and not one batch: a batch's replies together may not
        # be larger than 4 MiB, and a large board is. The tree goes first. A
        # task is put in the tree before it is put on the board, so a task
        # added here is in the tree by the time the board is read.
        parents = {node["id"]: node["parent"] for node in self._tree.to_nodes()}
        board = self._board.to_value()
        # A program without rules may have written the whole board over with
        # something else. What is not an object holds no tasks.
        board = board if isinstance(board, dict) else {}
        present = {task_id for task_id, fields in board.items() if _whole(fields)}
        tasks = {}
        now, due = _now(), None
        for task_id, fields in board.items():
            if task_id not in parents:
                # Being added, and not in the tree yet. Shown now, it would
                # be at the top, and under its parent a moment later. It is
                # kept out of sight only for a while, so that a task whose
                # place in the tree never comes is not lost.
                ends = _whole_number(fields.get("created") if isinstance(fields, dict) else None)
                if ends is not None and abs(now - ends) < self.GRACE * 1000:
                    ends += self.GRACE * 1000
                    due = ends if due is None else min(due, ends)
                    continue
            task = _task(task_id, fields, parents.get(task_id), present)
            if task is not None:
                tasks[task_id] = self._noted(task)
        self._due = due
        return tasks

    def _some(self, ids, *, parents=True, on_board=()):
        """The tasks with these ids, by id, reading only them. An id that is
        not on the board is left out.

        With `parents=False` no parent is read, and every task comes back
        with `parent=None`. That is for a call that only looks at a task's
        own fields. `on_board` names tasks the caller has just read, so that
        they are not read again when they turn out to be a parent.
        """
        ids = [task_id for task_id in dict.fromkeys(ids) if isinstance(task_id, str)]
        # The parents go first. Their replies are small, and a batch stops
        # running operations once its replies pass 4 MiB.
        reads = [("parent", task_id) for task_id in ids] if parents else []
        reads += [("get", task_id) for task_id in ids]
        found = self._all(reads)
        board, above = {}, {}
        for task_id in ids:
            fields = found[("get", task_id)]
            if fields is not None:
                board[task_id] = fields
                above[task_id] = found[("parent", task_id)] if parents else None
        # A parent is named only once it is whole itself. The ones not known
        # to be are looked up.
        present = {task_id for task_id, fields in board.items() if _whole(fields)} | set(on_board)
        unread = [parent for parent in set(above.values()) - present if parent not in (None, _ROOT)]
        titles = self._all([("title", parent) for parent in unread])
        present.update(parent for parent in unread if titles[("title", parent)] is not None)
        tasks = {}
        for task_id, fields in board.items():
            task = _task(task_id, fields, above[task_id], present)
            if task is not None:
                tasks[task_id] = self._noted(task)
        return tasks

    def _all(self, reads):
        """What each read found, by read. A read is ("parent", id) for where
        a task is in the tree, ("get", id) for the task, or ("title", id) for
        its title.

        Up to 500 tasks go in one message. A batch stops running reads once
        its replies pass 4 MiB, and the reads it did not run are sent again
        together in the next message, as many times as it takes. The first
        read of a batch is always run, so each message gets further.
        """
        found = {}
        waiting = list(reads)
        while waiting:
            part, waiting = waiting[: 2 * _AT_ONCE], waiting[2 * _AT_ONCE :]
            with self._team.client.batch() as batch:
                pending = [self._ask(batch, read) for read in part]
            left = []
            for read, answer in zip(part, pending):
                try:
                    found[read] = answer.value
                except errors.TooLarge:
                    if read is part[0]:
                        # The first read of a batch is always run, so this
                        # one is too large even by itself.
                        raise
                    left.append(read)
            waiting = left + waiting
        return found

    def _ask(self, batch, read):
        what, task_id = read
        if what == "parent":
            return batch.tree(self._tree.name).parent(task_id)
        return batch.json(self._board.name).get([task_id] if what == "get" else [task_id, "title"])

    @staticmethod
    def _in_order(tasks):
        return sorted(tasks, key=lambda task: (task.created, task.id))

    def list(self, state=None):
        """Every task, oldest first. With `state`, only the tasks in that
        state."""
        if state is not None and state not in STATES:
            raise errors.BadRequest(f"a state is one of {STATES}, not {state!r}")
        tasks = self._in_order(self._read().values())
        return [task for task in tasks if state is None or task.state == state]

    def get(self, task):
        """The task as it is now, or `None` if there is no such task."""
        task_id = _id(task)
        return self._some([task_id]).get(task_id) if isinstance(task_id, str) else None

    def children(self, task):
        """The sub-tasks of a task, oldest first."""
        # A `Task` was read from the board, and nothing leaves the board, so
        # it is not read again.
        parent = task.id if isinstance(task, Task) else self._existing(task, parents=False).id
        under = self._some(self._tree.children(parent), on_board=[parent]).values()
        return self._in_order(found for found in under if found.parent == parent)

    def _existing(self, task, parents=True):
        """The task, or `UnknownTask`. With `parents=False` its parent is not
        read, and is `None`."""
        task_id = _id(task)
        found = self._some([task_id], parents=parents).get(task_id) if isinstance(task_id, str) else None
        if found is None:
            raise errors.UnknownTask(f"there is no task {_id(task)!r} on the board of team {self._team.name!r}")
        return found

    def watch(self):
        """Every task now, and again each time the board changes. Use it in a
        `for` loop."""
        def later():
            # A task kept out of sight is shown when its time is over, even
            # if nothing on the board changed by then.
            return None if self._due is None else max(self._due - _now(), 0) / 1000

        return _follow.together(self._team.client, [self._board, self._tree], self.list, later)

    # ---- changing ------------------------------------------------------------

    def add(self, title, *, parent=None, notes=None, firm=False):
        """Adds a task and returns it. With `parent`, it is added as a
        sub-task of that task.

        With `firm=True`, only one agent can hold the task at a time.
        The task gets one ticket, and an agent has to take it to claim the
        task. That is for work that must not be done twice by mistake. The
        cost: an agent whose machine cannot reach the machine holding the
        ticket is told no, and claiming may wait up to two seconds to find
        that out.
        """
        if not isinstance(title, str) or not title.strip():
            raise errors.BadRequest(f"a task needs a title, not {title!r}")
        if notes is not None and not isinstance(notes, str):
            raise errors.BadRequest(f"a task's notes are text, not {type(notes).__name__}")
        firm = False if firm is None else firm
        if not isinstance(firm, bool):
            raise errors.BadRequest(f"firm is True or False, not {firm!r}")
        parent_id = self._existing(parent, parents=False).id if parent is not None else None

        # A random id, so that two machines that are cut off from each other
        # can both add tasks without picking the same one. It is long enough
        # that a new task does not land on an old one however large the
        # board gets.
        task_id = "t-" + secrets.token_hex(8)
        fields = {"title": title, "state": "todo", "created": int(time.time() * 1000)}
        if notes is not None:
            fields["notes"] = notes
        if self._team.me is not None:
            fields["created_by"] = self._team.me
        if firm:
            fields["firm"] = True
            # The tickets are made before the task is on the board, so that
            # nobody can find the task without them.
            self._new_tickets(task_id, 0, free=True)

        self._team._record("tasks", "json")
        self._team._record("tasks-tree", "tree")
        # The place in the tree first, in a message of its own, and then the
        # task. A program that stops in between leaves a place in the tree
        # that names nothing, which shows nothing.
        self._tree.move_node(task_id, parent_id or _ROOT)
        self._board.set([task_id], fields)
        return self._existing(task_id)

    # ---- tickets ---------------------------------------------------------------
    #
    # A firm task has one ticket at a time, and whoever takes it has the task.
    # Each ticket is a counter that can never go below zero, holding one, so
    # only one agent can take it, even on machines that cannot reach each
    # other: an agent whose machine does not hold it has to borrow it from the
    # machine that does, and is told no if that machine cannot be reached.
    #
    # A ticket is used once. When the task is given back, taken over or set
    # back by hand, it moves on to a new ticket with the next number, and the
    # old one is never made free again: an agent that has not heard of the
    # move yet would take it. Only one agent may make the new ticket, or there
    # would be two. So beside each ticket is a second one, for moving the task
    # on, and only the agent that takes that makes the next pair.
    #
    # Before a new ticket is made free, the old one must be taken. It is when
    # an agent's name is on the task, because an agent takes the ticket
    # before it writes its name. When no name is on it, the old ticket is
    # taken first, so that nobody else can.
    #
    # On the board, the new number is written last, after the name and time
    # of the agent that had the task are gone. An agent that saw the new
    # number beside an old, run-out time would take the task over while
    # another claimed the new ticket.
    #
    # A number must never go to two holders: a step that must not happen
    # twice tells one holder from the next by it. A pair is safe as long as
    # only one agent ever adds to its counters. Moving on from pair n, that
    # is the one agent that took pair n's ticket for moving on, so it holds
    # as long as pair n was made once. Force is the one path that makes a
    # pair without taking anything first, so it must pick a pair no other
    # path has made or ever will:
    #
    # - It reads the highest number the task has had, from the board and the
    #   team's list, and goes past it. A pair is put on the list before its
    #   counters are made, and taken off only after the task is on a higher
    #   one, so the highest pair made so far is always listed.
    # - It goes past it by a random jump of two to four thousand million.
    #   Moving on goes up one at a time, so the pairs the task had before
    #   would need that many moves to reach the forced pair.
    #
    # So two forces where the second sees the first, such as a second
    # operator, or the same one trying again after its write to the board
    # failed, land on different pairs: the second sees the first one's line
    # and goes past it. A force racing an agent that moves the task on from
    # an older pair, even several times, cannot meet it: that agent climbs
    # one at a time from far below. Two forces that cannot see each other at
    # all, on machines cut off from each other or at the very same moment,
    # each take their own random jump, and land on the same pair, or close
    # enough for one to climb into the other, only by a chance of about one
    # in a thousand million. Nothing that only counts can do better: neither
    # machine can learn of the other.
    #
    # Should a pair be made twice all the same, its ticket holds two, and on
    # two machines cut off from each other each could take one. So a ticket
    # or a ticket for moving on found holding more than one is never taken,
    # and a claim says so with `TicketUnavailable`. A doubled ticket is
    # cleared by setting the task back, which moves it on through its ticket
    # for moving on. A doubled ticket for moving on needs force.
    #
    # The number on the board never goes down, or a newer holder could have
    # a lower number than an older one, and whatever keeps the highest
    # number it has seen would refuse the newer holder. An agent that has
    # not heard the task moved on can still write a lower number late, from
    # any machine, and two machines can each write before hearing of the
    # other, so no check before a write can stop that. Instead each number is
    # written down as a key of its own, and the task's number is the highest
    # key. Keys from every machine are kept when they meet, and none is ever
    # taken off, so the highest only goes up, on every machine. A late, lower
    # number changes nothing: the pair it names is not the task's, and a
    # claim on it reads the board back and loses. An agent moving the task on
    # also looks first, and leaves the board alone when the task has moved
    # past its ticket already, so that it does not take the name of whoever
    # holds the new number off the task.

    def _ticket(self, task_id, number):
        return self._team.client.bounded_counter(_names.task_ticket(self._team.name, task_id, number))

    def _takeover(self, task_id, number):
        return self._team.client.bounded_counter(_names.task_takeover(self._team.name, task_id, number))

    def _new_tickets(self, task_id, number, free):
        """Makes ticket `number` of a task, and the ticket for taking it
        over. With `free=False` the task's ticket is made already taken, by
        the agent making it."""
        self._team._record(f"task-takeover/{task_id}/{number}", "bounded")
        self._team._record(f"task-ticket/{task_id}/{number}", "bounded")
        self._takeover(task_id, number).increment(1)
        if free:
            self._ticket(task_id, number).increment(1)

    def _next_ticket(self, task_id, number, free):
        """Takes the takeover ticket beside ticket `number` and makes the
        next tickets. Returns the new ticket's number, or `None` if another
        agent moved the task on first, or the machine that holds the
        takeover ticket cannot be reached."""
        takeover = self._takeover(task_id, number)
        if takeover.value() > 1 or not takeover.try_decrement(1):
            return None
        self._new_tickets(task_id, number + 1, free)
        return number + 1

    def _spent(self, task_id, *numbers, keep_takeover=False):
        """Takes the tickets with these numbers off the team's list, once
        they can never be had again. The counters stay in the sidecar, which
        cannot remove one, but nothing lists them any more.

        It is done only after the task's new number is on the board, and a
        failure is let go: the list is only a listing, and a sidecar that
        refuses to take anything off it, as an older one with rules does,
        must not leave the task stuck half way.

        A pair is spent once the task has moved on from it. Its takeover
        ticket was taken to move it on, and is never given back. Its own
        ticket was taken too: by the agent whose name was on the task, or by
        whoever set it back when no name was. A ticket that was not, because
        a task was set back with force, is no use either: the board has a
        higher number, and a claim made with it sees that and loses. Nothing
        ever adds to a counter of a pair once the next pair is made, so both
        stay taken.

        With `keep_takeover`, the takeover ticket stays listed. That is for a
        finished task, which is set back to "todo" through it."""
        names = []
        for number in numbers:
            if not keep_takeover:
                names.append(f"task-takeover/{task_id}/{number}")
            names.append(f"task-ticket/{task_id}/{number}")
        try:
            self._team._forget(names)
        except errors.OrvuError:
            pass

    def _highest(self, task_id):
        """The highest ticket number on the team's list for a task, or -1."""
        highest = -1
        for name in self._team.index():
            parts = name.split("/")
            if len(parts) == 3 and parts[0] in ("task-ticket", "task-takeover") and parts[1] == task_id:
                number = _ticket_key(parts[2])
                if number is not None:
                    highest = max(highest, number)
        return highest

    def _let_go(self, task_id, number):
        """Moves a firm task on from ticket `number`, which this agent holds,
        to a free ticket, and shows it as free on the board. Returns `False`
        if another agent moved it on first, or the machine that holds the
        takeover ticket cannot be reached. Then nothing is written."""
        ticket = self._next_ticket(task_id, number, free=True)
        if ticket is None:
            return False
        if self._existing(task_id, parents=False).ticket > number:
            # The task moved on past this ticket another way, as when it was
            # set back with force, and someone may hold the new number. What
            # is on the board is theirs, and is left alone. The pair just
            # made is never put on the board, so nobody can take it.
            return False
        with self._team.client.batch() as batch:
            board = batch.json(self._board.name)
            board.set([task_id, "state"], "todo")
            for field in ("claimed_by", "seen", "lease"):
                board.remove([task_id, field])
            _put_ticket(board, task_id, ticket)
        _sent(batch)
        self._spent(task_id, number)
        return True

    def update(self, task, *, title=None, notes=None, state=None, force=False):
        """Changes a task's title, notes or state, and returns the task as it
        is afterwards. What is left out stays as it was.

        Setting the state by hand is for putting things right. "todo" makes
        the task free again: whoever had it no longer does. "todo" and
        "doing" both drop a result it had. "done" finishes it with no result,
        and leaves a task that is done already as it is.

        A firm task that is set back moves on to a new ticket, and that needs
        the ticket for moving it on. If another agent took that ticket and
        stopped before it wrote what it did, the task is stuck, and this
        raises `TicketUnavailable` every time. `force=True` sets it back all
        the same, on a fresh pair of tickets that no earlier ticket can win.
        Use it only when you know that agent has stopped for good: its
        program ended, or its sidecar failed. Do not use it while a machine
        that may hold the ticket is only cut off; wait for it to come back.
        `force` is only for setting a task back to "todo".
        """
        found = self._existing(task, parents=False)
        task_id = found.id
        if title is not None and (not isinstance(title, str) or not title.strip()):
            raise errors.BadRequest(f"a task needs a title, not {title!r}")
        if notes is not None and not isinstance(notes, str):
            raise errors.BadRequest(f"a task's notes are text, not {type(notes).__name__}")
        if state is not None and state not in STATES:
            raise errors.BadRequest(f"a state is one of {STATES}, not {state!r}")
        if not isinstance(force, bool):
            raise errors.BadRequest(f"force is True or False, not {force!r}")
        if force and state != "todo":
            raise errors.BadRequest('force is only for setting a task back, with state="todo"')
        ticket, spent = None, ()
        if state == "todo" and found.firm:
            # Whoever had the task may still hold its ticket, and cannot be
            # asked to give it back. The task gets a new one.
            ticket, spent = self._set_back(found, force)
        with self._team.client.batch() as batch:
            board = batch.json(self._board.name)
            # Each field is written by itself, so that two agents who change
            # different fields at the same moment both keep their change.
            if title is not None:
                board.set([task_id, "title"], title)
            if notes is not None:
                board.set([task_id, "notes"], notes)
            if state == "done" and found.state != "done":
                board.set([task_id, "finished"], {"result": None, "by": None, "id": _mark()})
            elif state is not None and state != "done":
                board.set([task_id, "state"], state)
                # What no longer fits the new state is taken off.
                # The result and the finisher are part of the "finished" mark,
                # or fields of their own on a task finished the old way.
                gone = ["finished", "result", "finished_by"]
                if state == "todo":
                    gone += ["seen", "lease"]
                for field in gone:
                    board.remove([task_id, field])
            changed = title is not None or notes is not None or state is not None
            if changed and self._team.me is not None:
                board.set([task_id, "updated_by"], self._team.me)
            elif changed and found.updated_by is not None:
                # Changed by a program that gave no name. The task must not
                # go on naming whoever changed it before.
                board.remove([task_id, "updated_by"])
            if state == "todo":
                # Whoever had the task goes after the rest, so that the rest
                # is written while the agent that holds it still does. A
                # firm task's new ticket number goes last of all.
                board.remove([task_id, "claimed_by"])
            if ticket is not None:
                _put_ticket(board, task_id, ticket)
        _sent(batch)
        if spent:
            self._spent(task_id, *spent)
        return self._existing(task_id)

    def _set_back(self, found, force=False):
        """Moves a firm task that is being set back by hand on to a new, free
        ticket. Returns its number, and the numbers of the pairs to take off
        the team's list once it is on the board. With `force`, a task whose
        tickets cannot be had is moved on all the same."""
        unavailable = errors.TicketUnavailable(
            f"task {found.id!r} could not be set back: another agent is claiming it, taking it over or setting it "
            f"back at the same moment, or the machine that holds its ticket cannot be reached. Try again in a "
            f"moment. If it keeps failing and you know the agent that was claiming or moving it on has stopped for "
            f"good (its program ended, or its sidecar failed), set it back with update(task, state=\"todo\", "
            f"force=True). Do not use force while a machine that may hold the ticket is only cut off: wait for it"
        )
        # Nobody's name is on the task, so its ticket may be free. It is
        # taken first, so that it does not stay free beside the new one.
        took_old = False
        if found.claimed_by is None:
            took_old = self._ticket(found.id, found.ticket).try_decrement(1)
            if not took_old and not force:
                raise unavailable
        ticket = self._next_ticket(found.id, found.ticket, free=True)
        if ticket is not None:
            return ticket, (found.ticket,)
        if not force:
            if took_old:
                # Nobody could have the task while this held its ticket, so
                # the ticket is put back as it was.
                self._ticket(found.id, found.ticket).increment(1)
            raise unavailable
        # Forced. The ticket for moving the task on is held by an agent that
        # stopped, or by one moving the task on right now. Either may have
        # made the next pairs already, so the task jumps far past every pair
        # it may have had: see the notes on tickets above. Every ticket an
        # earlier agent may still hold has a lower number, and a claim made
        # with it sees another number on the board and loses. An old ticket
        # this took is kept, so it can never be had again.
        highest = max(found.ticket, self._highest(found.id))
        if highest + 2 * _JUMP >= _MOST:
            raise errors.TicketUnavailable(
                f"task {found.id!r} could not be set back with force: its ticket numbers are used up, as its "
                f"number is already {highest}. Add the task again, and remove this one"
            )
        ticket = highest + _JUMP + secrets.randbelow(_JUMP)
        self._new_tickets(found.id, ticket, free=True)
        return ticket, (found.ticket, found.ticket + 1)

    def _me(self, me):
        me = me if me is not None else self._team.me
        if not isinstance(me, str) or not me:
            raise errors.BadRequest(
                "say who is asking: pass `me=...` here, or give the team a name for this "
                "agent with `Team.connect(me=...)`"
            )
        return me

    @staticmethod
    def _lease(lease):
        if lease is None:
            return LEASE
        if isinstance(lease, bool) or not isinstance(lease, int) or not 0 < lease <= _LONGEST_LEASE:
            raise errors.BadRequest(f"a lease is a whole number of seconds, from 1 to a year, not {lease!r}")
        return lease

    def claim(self, task, me=None, *, lease=None):
        """Says "this task is mine". Returns `True` if it now is, and `False`
        if another agent has it or it is done.

        The claim lasts `lease` seconds, a quarter of an hour if left out.
        Call `still_working` before that runs out to keep it. An agent that
        does not is taken to have gone, and another agent's `claim` takes
        the task over. How long it has been quiet is measured by the clock of
        the agent that wants the task, from when that agent first saw the
        claim as it is: see `Task.abandoned`.

        On a task that is not firm this is a soft claim. It keeps agents off
        each other's tasks nearly all the time, but two agents that claim the
        same task at the very same moment can both be told `True`, and so
        can two agents taking over the same abandoned task. Use it where
        doing a task twice is only wasteful, not where it would do harm. On
        a firm task, only one agent at a time is told `True`.

        Claiming a firm task this agent already has keeps it, as
        `still_working` does, and leaves its lease as it was.

        To learn the number of a firm task's ticket, claim it with
        `claim_ticket` instead.

        A firm task whose ticket was made twice, which two agents could both
        take, is never claimed: this raises `TicketUnavailable`, which says
        how to set the task back.
        """
        return self._claim(task, me, lease) is not None

    def claim_ticket(self, task, me=None, *, lease=None):
        """Claims a firm task as `claim` does, and returns the number of the
        ticket this agent now holds, or `None` if it did not get the task.

        The number goes up each time the task moves on to another agent, so
        it tells one holder from the next. A firm task still goes to one
        agent at a time, but an agent that went quiet, or whose machine is
        cut off, may not have heard yet that the task was taken over, and can
        go on working. So a step that must not happen twice, such as sending
        an invoice, should carry the number to whatever it changes, and that
        side should keep the highest number it has seen and refuse a lower
        one:

            ticket = team.tasks.claim_ticket(task)
            if ticket is not None:
                invoice = write_invoice()
                # The billing system refuses a ticket lower than the
                # highest it has seen for this task.
                billing.send(invoice, task=task.id, ticket=ticket)

        That last refusal is what stops the step from happening twice.
        Nothing here can do it for the other side. `still_holding` tells
        whether the number is still this agent's as far as this machine
        knows, which is a good check before starting, not a promise.
        """
        found = self._existing(task, parents=False)
        if not found.firm:
            raise errors.BadRequest(f"task {found.id!r} is not firm, and only a firm task has a ticket")
        return self._claim(found, me, lease)

    def still_holding(self, task, ticket, me=None):
        """Whether this agent still holds ticket number `ticket` of a firm
        task, as far as this machine knows: the task is not done, and it
        has not been given back, taken over or set back since. See
        `claim_ticket`."""
        me = self._me(me)
        found = self._existing(task, parents=False)
        if not found.firm:
            raise errors.BadRequest(f"task {found.id!r} is not firm, and only a firm task has a ticket")
        return found.claimed_by == me and found.state != "done" and found.ticket == ticket

    def _claim(self, task, me, lease):
        """What `claim` does. Returns the number of the task's ticket if this
        agent has the task now, and `None` if not."""
        me = self._me(me)
        lease = self._lease(lease)
        found = self._existing(task, parents=False)
        if found.state == "done":
            return None
        if found.firm and found.claimed_by == me:
            # Only the time is written. If the task was taken over and this
            # agent has not heard yet, its name must not go back on it.
            if not self.still_working(found, me):
                return None
            # A name and a number read together belong together: a takeover
            # takes the old name off before it writes the new number.
            now = self._existing(found.id, parents=False)
            return now.ticket if now.claimed_by == me and now.state != "done" else None
        taking_over = found.claimed_by not in (None, me)
        if taking_over and not found.abandoned():
            return None
        ticket = found.ticket
        if found.firm:
            if taking_over:
                # The agent that went quiet keeps its ticket. The task moves
                # on to a new one, already taken by this agent.
                if self._takeover(found.id, found.ticket).value() > 1:
                    raise self._doubled(found, force=True)
                ticket = self._next_ticket(found.id, found.ticket, free=False)
                if ticket is None:
                    return None
            else:
                # A ticket holding more than one was made twice, and could
                # go to two agents. It is never taken.
                own = self._ticket(found.id, ticket)
                if own.value() > 1:
                    raise self._doubled(found, force=False)
                if not own.try_decrement(1):
                    return None
        try:
            with self._team.client.batch() as batch:
                board = batch.json(self._board.name)
                # The writes of a batch are not seen all at once. On a task
                # nobody holds, the name goes first, so that the rest is
                # written on a task this agent holds: an agent that may change
                # only its own tasks may write nothing else on another's. On a
                # task being taken over, the time goes first, so that it never
                # looks abandoned with its new ticket number on it: another
                # agent would take that ticket's takeover ticket too.
                seen = _now()
                if not taking_over:
                    board.set([found.id, "claimed_by"], me)
                board.set([found.id, "seen"], seen)
                board.set([found.id, "lease"], lease)
                if ticket != found.ticket:
                    # The old holder's name goes before the new number, so
                    # that the two are never seen side by side: the old
                    # holder would take the new number for its own.
                    board.remove([found.id, "claimed_by"])
                    _put_ticket(board, found.id, ticket)
                if taking_over:
                    board.set([found.id, "claimed_by"], me)
                board.set([found.id, "state"], "doing")
            _sent(batch)
            if ticket != found.ticket:
                self._spent(found.id, found.ticket)
            # Read back, in case another agent claimed the task, or finished
            # it, just after we looked.
            now = self._existing(found.id, parents=False)
        except BaseException:
            if found.firm:
                self._let_go(found.id, ticket)
            raise
        if now.claimed_by == me and now.state != "done" and now.ticket == ticket:
            if taking_over:
                # Written once the task is known to be this agent's, so that
                # it never names an agent that did not get it.
                self._board.set([found.id, "taken_over"], {"by": me, "from": found.claimed_by, "at": seen})
            return ticket
        if now.state == "done" and now.claimed_by == me and (now.seen, now.lease) == (seen, lease) and now.finished_by != me:
            # The task was finished just before this claim was written. What
            # the claim wrote is put back as it was, so that the task does
            # not name an agent that did not work on it. Only if what is on
            # the task is still what this claim wrote: another program under
            # the same name may have claimed it since, rightly, or finished
            # it.
            with self._team.client.batch() as batch:
                board = batch.json(self._board.name)
                # The name last, so that the rest is written while this agent
                # still holds the task.
                for field, before in (("seen", found.seen), ("lease", found.lease), ("claimed_by", found.claimed_by)):
                    if before is None:
                        board.remove([found.id, field])
                    else:
                        board.set([found.id, field], before)
            _sent(batch)
        if found.firm and now.state != "done":
            if now.claimed_by == me and now.ticket != ticket and now.state == "doing":
                # The task moved on past this agent's ticket before it looked,
                # as when it was set back with force, and its name is beside
                # a number that is not its own. It is taken off, so that the
                # task is free for whoever takes the ticket on the board.
                with self._team.client.batch() as batch:
                    board = batch.json(self._board.name)
                    board.set([found.id, "state"], "todo")
                    for field in ("claimed_by", "seen", "lease"):
                        board.remove([found.id, field])
                _sent(batch)
            # This agent holds the ticket but did not get the task. Kept, the
            # ticket would leave the task stuck.
            self._let_go(found.id, ticket)
        return None

    @staticmethod
    def _doubled(found, force):
        """The error for a firm task whose ticket, or ticket for moving it
        on, was made twice."""
        which = "ticket for moving it on" if force else f"ticket {found.ticket}"
        how = 'update(task, state="todo", force=True)' if force else 'update(task, state="todo")'
        return errors.TicketUnavailable(
            f"task {found.id!r} cannot be claimed: its {which} was made twice, so two agents could both take it. "
            f"It is never taken. Set the task back with {how}, which moves it on to a fresh one"
        )

    def still_working(self, task, me=None):
        """Says that this agent is still working on a task it claimed, so
        that the claim does not run out. Returns `False` if the task is no
        longer this agent's: it was taken over, given back, set back by
        hand, or is done. Then stop working on it."""
        me = self._me(me)
        found = self._existing(task, parents=False)
        if found.claimed_by != me or found.state == "done":
            return False
        self._board.set([found.id, "seen"], _now())
        # Read back, in case another agent took the task over just after we
        # looked.
        now = self._existing(found.id, parents=False)
        return now.claimed_by == me and now.state != "done"

    def release(self, task, me=None):
        """Gives a claimed task back, so that another agent can take it.
        Returns `False`, and changes nothing, if the task is not this
        agent's."""
        me = self._me(me)
        found = self._existing(task, parents=False)
        if found.claimed_by != me or found.state == "done":
            return False
        if found.firm:
            # If another agent moved the task on first, it took the task over
            # or set it back, and the task is no longer this agent's to give.
            return self._let_go(found.id, found.ticket)
        with self._team.client.batch() as batch:
            # The state first. If the holder went first, another agent could
            # claim the task in between and then have its state set back. The
            # holder last, so that the rest is written while this agent still
            # holds the task.
            board = batch.json(self._board.name)
            board.set([found.id, "state"], "todo")
            board.remove([found.id, "seen"])
            board.remove([found.id, "lease"])
            board.remove([found.id, "claimed_by"])
        _sent(batch)
        return True

    def finish(self, task, result=None, me=None):
        """Marks a task done and keeps its result, which can be any JSON
        value. Returns `True` if this finished it.

        A task that is already done stays as it is, with the result it has,
        and this returns `False`. The result and the agent that finished the
        task are written together, and read back: if two agents finish the
        same task at the same moment, the one whose result was not kept is
        told `False`. Both can still be told `True` when one has read back
        its own before the other's write arrives, as on two machines that
        are cut off from each other; the result kept is then the later one,
        beside the name of the agent that wrote it.

        A firm task can only be finished by the agent that has it. Any other
        agent is told `False`. To finish it by hand, use `update`.
        """
        found = self._existing(task, parents=False)
        if found.state == "done":
            return False
        who = me if me is not None else self._team.me
        if found.firm and (who is None or found.claimed_by != who):
            return False
        # The mark, the result and the name are one write, so a task seen
        # as done always has its result, and the result is always beside the
        # name of the agent that gave it. If it cannot be kept, this raises,
        # and the task is not marked done.
        mark = _mark()
        self._board.set([found.id, "finished"], {"result": result, "by": who, "id": mark})
        # Read back, in case another agent finished it at the same moment.
        # The later write is the one kept.
        kept = self._board.get([found.id, "finished"])
        finished = isinstance(kept, dict) and kept.get("id") == mark
        if finished and found.firm:
            # The ticket this agent holds is never given back now. The one
            # for moving the task on is kept: setting the task back to
            # "todo" takes it.
            self._spent(found.id, found.ticket, keep_takeover=True)
        return finished

    def move(self, task, parent=None):
        """Makes a task a sub-task of `parent`, or a top-level task again if
        `parent` is left out. Returns `True` if it moved.

        A task cannot be put under itself or under one of its own sub-tasks.
        Such a move is not made, and this returns `False`.
        """
        task_id = self._existing(task, parents=False).id
        parent_id = self._existing(parent, parents=False).id if parent is not None else None
        self._tree.move_node(task_id, parent_id or _ROOT)
        return self._existing(task_id).parent == parent_id
