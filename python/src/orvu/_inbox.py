"""The inbox: messages from one agent to another, or to the whole team."""

import dataclasses
import json
import re
import secrets
import time

from . import _follow, _names
from . import errors
from ._tasks import _sent


@dataclasses.dataclass(frozen=True)
class Message:
    """One message, as it was sent."""

    #: Made up when the message is sent. No two messages share one.
    id: str
    #: The agent that sent it, or `None` if the sender gave no name.
    sender: str | None
    #: The agent it was sent to, or `None` if it was sent to everyone.
    to: str | None
    #: What was sent: text, or anything else JSON can hold.
    body: object
    #: When it was sent, in milliseconds since 1970, by the clock of the
    #: machine that sent it.
    sent: int


class Inbox:
    """Messages between the agents of a team. Get it from `team.inbox`.

        team.inbox.send("researcher-2", "The Q3 numbers are in the notes.")
        team.inbox.send_all("Deploy is on Friday.")

        for message in team.inbox.unread():
            print(message.sender, message.body)

    An agent reads what was sent to it and what was sent to everyone. A
    message is not taken out by being read. `unread` writes down, under the
    agent's name, what the agent has been given, so that it is given each
    message once, also after its program has started again.

    An inbox is kept in pages, and a new page is started when one is full.
    Reading starts at the newest pages, so a long history does not make it
    slow.
    """

    #: How many messages go on a page.
    PAGE = 100
    #: How many of the newest pages `messages` looks at, and `unread` the
    #: first time.
    RECENT = 2
    #: How many older pages `unread` reads again at most on each look, and
    #: `watch` follows. The others wait their turn.
    BEHIND = 8
    #: How long, in seconds, an older page is let be after it was read again
    #: and nothing new was found on it. The wait doubles each time nothing
    #: is found again, up to `LONGEST_WAIT`.
    WAIT = 1.0
    #: The longest wait between two reads of an older page.
    LONGEST_WAIT = 60.0

    def __init__(self, team):
        self._team = team
        # The ids of the messages given, by inbox and page. Only the pages
        # `unread` still looks at are kept.
        self._seen = {}
        # For each inbox, the first page `unread` has not finished with.
        self._from = {}
        # The older pages whose count is above how far this agent got on
        # them, by inbox and page: a message was written there that has not
        # arrived here yet, or the count is wrong. When each is read next.
        self._later = {}
        # Up to `BEHIND` of those pages, as (inbox, page number), that
        # `watch` follows, to see at once when the message arrives.
        self._waiting = []
        # The clock the waits are measured by, in seconds.
        self._now = time.monotonic

    # ---- where things are kept -----------------------------------------------

    def _pages(self, owner):
        return self._team.client.counter(_names.inbox_pages(self._team.name, owner))

    def _page(self, owner, number):
        return self._team.client.list(_names.inbox_page(self._team.name, owner, number))

    def _counts(self, owner):
        return self._team.client.cart(_names.inbox_count(self._team.name, owner))

    def _done(self, owner):
        reader = _names.inbox_owner(self._team.me)
        return self._team.client.map(_names.inbox_done(self._team.name, reader, owner))

    def _mine(self):
        """The inboxes this agent reads."""
        me = self._team.me
        return [_names.EVERYONE] if me is None else [_names.inbox_owner(me), _names.EVERYONE]

    # ---- sending -------------------------------------------------------------

    def _send(self, owner, to, body):
        try:
            kept = json.dumps(
                {
                    "id": "m-" + secrets.token_hex(8),
                    "from": self._team.me,
                    "to": to,
                    "body": body,
                    "sent": int(time.time() * 1000),
                },
                ensure_ascii=False,
                allow_nan=False,
            )
        except (TypeError, ValueError) as error:
            raise errors.BadRequest(f"that cannot be sent: {error}") from None
        number = self._newest(owner)
        page = self._page(owner, number)
        length = page.len()
        if length >= self.PAGE:
            # This page is full. If two agents find that at the same moment
            # both move on, and a page number is skipped. Reading goes by
            # the numbers and not by a count of pages, so nothing is passed
            # over for it.
            self._team._record(f"inbox-page/{owner}", "counter")
            self._pages(owner).increment(1)
            number = self._pages(owner).value()
            page = self._page(owner, number)
            length = page.len()
        self._team._record(f"inbox/{owner}/{number}", "list")
        self._team._record(f"inbox-count/{owner}", "cart")
        # The page's count goes up with the message, in the same message to
        # the sidecar. A reader that sees the count of a page it has
        # finished go up knows a message was written there late.
        with self._team.client.batch() as batch:
            batch.list(page.name).insert(length, kept)
            batch.cart(self._counts(owner).name).add(str(number), 1)
        _sent(batch)

    def send(self, to, body):
        """Sends `body` to the agent called `to`. It is there for that agent
        whenever it next reads, on any machine."""
        if body is None:
            raise errors.BadRequest("a message needs something in it")
        to = _names.agent(to, "the name of the agent to send to")
        self._send(_names.inbox_owner(to), to, body)

    def send_all(self, body):
        """Sends `body` to every agent on the team."""
        if body is None:
            raise errors.BadRequest("a message needs something in it")
        self._send(_names.EVERYONE, None, body)

    # ---- reading -------------------------------------------------------------

    @staticmethod
    def _message(kept):
        """The message kept as `kept`, or `None` if that is not one."""
        try:
            fields = json.loads(kept)
            found = Message(fields["id"], fields.get("from"), fields.get("to"), fields["body"], fields["sent"])
        except (ValueError, KeyError, TypeError, RecursionError):
            return None
        # Messages are put in order by these two, so they have to be what
        # they should be. True and False count as numbers in Python.
        if not isinstance(found.id, str) or not isinstance(found.sent, int) or isinstance(found.sent, bool):
            return None
        # Who sent it and to whom are names, or nobody.
        sender = found.sender if isinstance(found.sender, str) else None
        to = found.to if isinstance(found.to, str) else None
        return Message(found.id, sender, to, found.body, found.sent)

    def _newest(self, owner):
        return self._pages(owner).value()

    def _read(self, owner, first, newest):
        """The messages on the pages `first` to `newest` of one inbox. What
        is there and is not something this block wrote is left out."""
        found = []
        for number in range(max(first, 0), newest + 1):
            for kept in self._page(owner, number).to_list():
                message = self._message(kept)
                if message is not None:
                    found.append(message)
        return found

    @staticmethod
    def _in_order(messages):
        return sorted(messages, key=lambda message: (message.sent, message.id))

    def messages(self):
        """The recent messages to this agent and to everyone, oldest first:
        what is on the newest two pages of each."""
        found = []
        for owner in self._mine():
            newest = self._newest(owner)
            found += self._read(owner, newest - self.RECENT + 1, newest)
        return self._in_order(found)

    def history(self):
        """Every message to this agent and to everyone that was ever sent,
        oldest first. This reads every page."""
        return self._in_order([m for owner in self._mine() for m in self._read(owner, 0, self._newest(owner))])

    def _given(self, owner, number):
        """The ids of the messages on one page that this agent has been
        given, as the team keeps them."""
        reader = _names.inbox_owner(self._team.me)
        return self._team.client.set(_names.inbox_given(self._team.name, reader, owner, number))

    def unread(self):
        """The messages this agent has not been given yet, oldest first. The
        first time, that is the recent ones. After that it is whatever has
        arrived since, however much that is.

        What an agent with a name was given is written down for the team, so
        a program that starts again under that name is not given it a second
        time. A program with no name remembers only for as long as it runs.

        An agent with a name is also given a message that a machine cut off
        for long wrote late to a page the inbox had moved past, once. Each
        page keeps a count of the messages written to it, and the agent
        keeps a mark of how many it was given from each page. The counts
        are only hints. A page whose count is above the mark, or cannot be
        right, is read again until the mark catches up: at once, and then,
        while nothing new is found there, after a wait that starts at `WAIT`
        and doubles up to `LONGEST_WAIT`. A change to the page's count starts
        the waits over. At most `BEHIND` such pages are read on each look,
        those read longest ago first, so none is passed over for good. A
        late message is always given, within about `LONGEST_WAIT` at worst,
        to a program that keeps looking: through `watch`, or by calling
        `unread` regularly. With more than `BEHIND` such pages waiting, a
        program that calls `unread` only now and then needs several calls to
        reach them all, about one call for each `BEHIND` pages.
        Anyone who may send can add to a page's count without writing there;
        a count that then stays as it is costs one more read of that page
        each `LONGEST_WAIT`.
        """
        named = self._team.me is not None
        owners = self._mine()
        with self._team.client.batch() as batch:
            newest = [batch.counter(self._pages(owner).name).value() for owner in owners]
            if named:
                counts = [batch.cart(self._counts(owner).name).items() for owner in owners]
                done = [batch.map(self._done(owner).name).entries() for owner in owners]
        clock = self._now()
        looks = []
        for at, owner in enumerate(owners):
            last = newest[at].value
            recent = last - self.RECENT + 1
            # Every page from where the last look left off, so that a burst
            # that filled several pages is not passed over. The newest pages
            # are looked at again each time, because a machine that has not
            # heard of a new page yet still writes on the one before.
            first = max(self._from.get(owner, recent), 0)
            written, odd, given = {}, {}, {}
            if named:
                written, odd = _counted(counts[at].value)
                given = _numbered(done[at].value)
                # Pages behind, written to since this agent was given all it
                # had. Only pages this agent has read are looked at, and not
                # the history from before it first read.
                self._behind(owner, given, written, odd, first, clock)
            looks.append((owner, last, recent, first, written, odd, given))
        again = self._due(clock) if named else []
        fresh, marks, settled = [], [], []
        for owner, last, recent, first, written, odd, given in looks:
            behind = [n for o, n in again if o == owner]
            before = dict(given)
            for number in behind + list(range(first, last + 1)):
                on_page = self._read(owner, number, number)
                seen = self._seen.setdefault((owner, number), set())
                found = [m for m in on_page if m.id not in seen]
                if found and named:
                    already = set(self._given(owner, number).elements())
                    seen.update(m.id for m in found if m.id in already)
                    found = [m for m in found if m.id not in already]
                    marks += [(owner, number, m.id) for m in found]
                if named:
                    # How far this agent got on the page: what was written to
                    # it, as far as it has arrived here.
                    now = len(on_page) if number in odd else min(written.get(number, 0), len(on_page))
                    if given.get(number) != now:
                        settled.append((owner, number, now))
                    given[number] = now
                seen.update(m.id for m in found)
                fresh += found
            self._from[owner] = max(first, recent, 0)
            if named:
                for number in behind:
                    self._read_again(owner, number, given[number] > before.get(number, 0), clock)
                self._behind(owner, given, written, odd, self._from[owner], clock)
        # What is remembered of pages that are finished is let go.
        self._seen = {key: ids for key, ids in self._seen.items() if key[1] >= self._from[key[0]]}
        # The pages `watch` follows: those to be read soonest.
        soonest = sorted(self._later.items(), key=lambda item: (item[1].due, item[1].read, item[0]))
        self._waiting = sorted(key for key, _ in soonest[: self.BEHIND])
        if marks or settled:
            reader = _names.inbox_owner(self._team.me)
            for owner in sorted({owner for owner, _, _ in settled}):
                self._team._record(f"inbox-done/{reader}/{owner}", "map")
            with self._team.client.batch() as batch:
                for owner, number, message_id in marks:
                    batch.set(self._given(owner, number).name).add(message_id)
                for owner, number, now in settled:
                    batch.map(self._done(owner).name).put(str(number), str(now))
            # If what was given cannot be written down, the messages are not
            # given now either. They are given next time, rather than given
            # now and given again after a restart.
            try:
                _sent(batch)
            except BaseException:
                for owner, number, message_id in marks:
                    self._seen.get((owner, number), set()).discard(message_id)
                raise
            for owner, number in sorted({(owner, number) for owner, number, _ in marks}):
                self._team._record(f"inbox-given/{reader}/{owner}/{number}", "set")
        return self._in_order(fresh)

    def _behind(self, owner, given, written, odd, below, clock):
        """Brings up to date which older pages of one inbox, before `below`,
        are to be read again: those whose count is above how far this agent
        got on them, and those whose count cannot be right, which may hide a
        message however far it got. A page new to the list, or whose count
        changed, is read at once. Pages no longer behind are let go."""
        behind = {}
        for n, got in given.items():
            if n < below and (n in odd or written.get(n, 0) > got):
                behind[n] = odd[n] if n in odd else str(written[n])
        for key in [key for key in self._later if key[0] == owner and key[1] not in behind]:
            del self._later[key]
        for n, count in behind.items():
            later = self._later.get((owner, n))
            if later is None:
                self._later[(owner, n)] = _Later(count, clock)
            elif later.count != count:
                # It keeps when it was last read, so that changing a count
                # does not put a page ahead of the others.
                later.count, later.due, later.step = count, clock, 0

    def _due(self, clock):
        """The older pages to read again on this look, as (inbox, page): at
        most `BEHIND` of those whose wait is over, those read longest ago
        first."""
        due = [(later.read, key) for key, later in self._later.items() if later.due <= clock]
        return sorted(key for _, key in sorted(due)[: self.BEHIND])

    def _read_again(self, owner, number, found, clock):
        """Sets when an older page that was just read is read next: at once
        if something new was found on it, and after a growing wait if not."""
        later = self._later.get((owner, number))
        if later is None:
            return
        later.read = clock
        later.reads += 1
        later.step = 0 if found else later.step + 1
        later.due = clock if found else clock + min(self.WAIT * 2 ** (later.step - 1), self.LONGEST_WAIT)

    def _soon(self, keys):
        """Has the older pages `keys` read on the next look, as something
        arrived on them."""
        clock = self._now()
        for key in keys:
            if key in self._later:
                self._later[key].due, self._later[key].step = clock, 0

    def _waits(self):
        """For `watch`: the older pages whose wait is over, each with how
        often it was read, and how long until the next wait is over."""
        clock = self._now()
        due = tuple((key, later.reads) for key, later in sorted(self._later.items()) if later.due <= clock)
        nearest = min((later.due for later in self._later.values()), default=None)
        return due, None if nearest is None else max(nearest - clock, 0)

    def watch(self):
        """Each message this program has not been given yet, and then each
        new one as it arrives. Use it in a `for` loop."""
        client = self._team.client
        named = self._team.me is not None
        while True:
            owners = self._mine()
            numbers = [self._newest(owner) for owner in owners]
            pages = [self._pages(owner) for owner in owners]
            behind = self._waiting
            # The newest pages of each inbox, as `unread` looks at them, and
            # the older pages whose count says a message was written there
            # that has not arrived here yet. The count can come before the
            # message, and then the page itself has to be followed until it
            # does.
            recent = [
                self._page(owner, n) for owner, newest in zip(owners, numbers)
                for n in range(max(newest - self.RECENT + 1, 0), newest + 1)
            ] + [self._page(owner, n) for owner, n in behind]
            # The counts of messages per page. They go up when a message is
            # written to an older page too, which `unread` then reads again.
            counts = [self._counts(owner) for owner in owners] if named else []
            ended = True
            grown_from = None

            def state():
                # The page numbers are part of it. A sender moves an inbox to
                # a new page first and writes on it afterwards, and the move
                # by itself has to be noticed, or the new page would never
                # be followed. The length of a page is enough to see that a
                # message came: the messages are read by `unread`.
                with client.batch() as batch:
                    now = [batch.counter(page.name).value() for page in pages]
                    lengths = [batch.list(page.name).len() for page in recent]
                    written = [batch.cart(count.name).items() for count in counts]
                # The older pages whose wait is over are part of it too, so
                # that the wait being over is noticed like a change.
                return (
                    [read.value for read in now],
                    [read.value for read in lengths],
                    [read.value for read in written],
                    self._waits()[0],
                )

            def later():
                return self._waits()[1]

            # The first value comes at once, and a new one each time a page
            # or a page number changes, or the wait for an older page is
            # over.
            for now, lengths, _, _ in _follow.together(client, pages + recent + counts, state, later):
                # An older page that grew is read on this look, whatever its
                # wait.
                if grown_from is not None and behind:
                    grown = zip(behind, grown_from[-len(behind):], lengths[-len(behind):])
                    self._soon([key for key, was, now_is in grown if was != now_is])
                grown_from = lengths
                for message in self.unread():
                    yield message
                if now != numbers or self._waiting != behind:
                    # A new page was started, or an older page is waited on
                    # or caught up with. What is followed changes from here.
                    ended = False
                    break
            if ended:
                return


@dataclasses.dataclass
class _Later:
    """When an older page that is behind is read next."""

    #: The page's count as it read, as text, as it may not be a number.
    count: str
    #: When it is read next, by `Inbox._now`.
    due: float
    #: When it was last read, or never.
    read: float = float("-inf")
    #: How many times in a row it was read with nothing new found.
    step: int = 0
    #: How many times it was read in all.
    reads: int = 0


#: More messages than any page is ever written. A count above it, or below
#: zero, was not made by senders adding one each.
_MOST = 1_000_000


def _counted(counts):
    """The counts of messages per page, by page number, and apart from them
    the counts that cannot be right, as they read."""
    good, odd = {}, {}
    for key, value in counts.items():
        if not re.fullmatch("[0-9]+", str(key)):
            continue
        if re.fullmatch("[0-9]+", str(value)) and int(value) <= _MOST:
            good[int(key)] = int(value)
        else:
            odd[int(key)] = str(value)
    return good, odd


def _numbered(entries):
    """The entries of a map or cart whose keys are page numbers, by number.
    What is not a number is left out."""
    return {
        int(key): int(value)
        for key, value in entries.items()
        if re.fullmatch("[0-9]+", str(key)) and re.fullmatch("[0-9]+", str(value))
    }
