"""The roster: which agents are on the team, and which are there now."""

import dataclasses
import secrets
import threading
import time

from . import _follow, _names
from . import errors


# Stands between an agent's name and the word for one of its programs. It
# is a character no name is typed with.
_OWN = "\x1f"

#: An entry silent for longer than this, in milliseconds, is not listed at
#: all. It was left by a program that ended with no goodbye.
_FORGOTTEN_AFTER = 24 * 60 * 60 * 1000


@dataclasses.dataclass(frozen=True)
class Member:
    """One agent on the roster, as it was when the roster was read."""

    name: str
    #: When it last said it was there, in milliseconds since 1970, by the
    #: clock of its own machine.
    last_seen: int
    #: True if that was a short while ago.
    here: bool


class Roster:
    """Who is on the team. Get it from `team.roster`.

        team.roster.join()
        team.roster.here()   # ["researcher-1", "researcher-2"]

    An agent that has joined says "still here" every few seconds, in the
    background. One that stopped saying so is still on the roster and is no
    longer counted as here. That is how an agent that crashed fades out:
    nothing else would take it off. What it left behind is cleared the next
    time a program joins under its name, and after a day of silence it is
    not listed at all.
    """

    def __init__(self, team):
        self._team = team
        self._map = team.client.map(_names.roster(team.name))
        self._stop = None
        self._beat = None
        self._beats = None
        self._mine = None
        #: How many seconds an agent may be silent and still count as here,
        #: if its entry does not say how often it beats.
        self.away_after = 45.0

    def join(self, every=15.0):
        """Puts this agent on the roster and keeps saying "still here" every
        `every` seconds until `leave` or until the team is closed. An agent
        counts as here for three times that long after it was last heard,
        whoever reads the roster."""
        if self._team.me is None:
            raise errors.BadRequest("an agent needs a name to join the roster: `Team.connect(me=...)`")
        if not isinstance(every, (int, float)) or isinstance(every, bool) or every <= 0:
            raise errors.BadRequest(f"`every` is a number of seconds above zero, not {every!r}")
        self.leave()
        self._team._record("roster", "map")
        # The heartbeat runs in the background, so it has a connection of
        # its own. Two threads on one connection get in each other's way
        # when the sidecar restarts and both start it again.
        beats = self._team.client.another()
        # Each program has an entry of its own, under the agent's name and a
        # word made up for this join. Two programs can run under one name,
        # and one of them leaving must not take the other off the roster.
        entry, mine = beats.map(self._map.name), self._team.me + _OWN + secrets.token_hex(4)

        def write():
            # When, and how often. A reader needs the second to tell how
            # long this agent may be silent.
            entry.put(mine, f"{int(time.time() * 1000)} {every}")

        try:
            write()
        except BaseException:
            # Nothing was started, so nothing is left open.
            beats.close()
            raise
        self._beats, self._mine = beats, mine
        self._clear_left_behind(mine)
        stop = self._stop = threading.Event()

        def beat():
            while not stop.wait(every):
                try:
                    write()
                except Exception:  # noqa: BLE001
                    # The sidecar is away, or something else went wrong this
                    # once. The next beat tries again. A heartbeat that
                    # stopped would make the agent look gone while it runs.
                    pass

        self._beat = threading.Thread(target=beat, name="orvu-roster", daemon=True)
        self._beat.start()

    def leave(self, remove=True):
        """Stops saying "still here" and takes this agent off the roster. Does
        nothing if it had not joined."""
        if self._stop is None:
            return
        self._stop.set()
        self._beat.join(timeout=5)
        self._beats.close()
        mine, self._mine = self._mine, None
        self._stop = self._beat = self._beats = None
        if remove:
            self._map.remove(mine)

    def _clear_left_behind(self, mine):
        """Takes off the entries this agent's name has that are no longer
        here: programs under this name that ended with no goodbye. Another
        program under the name that still runs is left alone."""
        now = time.time() * 1000
        try:
            for key, kept in self._map.entries().items():
                if key != mine and key.rpartition(_OWN)[0] == self._team.me and _silent(now, kept, self.away_after):
                    self._map.remove(key)
        except errors.OrvuError:
            # The entries are left for the next join. This agent is on the
            # roster all the same.
            pass

    def members(self):
        """Everyone on the roster, by name. An agent that runs as several
        programs is one member, and is here if any of them is. An agent not
        heard from for more than a day is left out."""
        now = time.time() * 1000
        found = {}
        for key, kept in self._map.entries().items():
            name = key.rpartition(_OWN)[0] or key
            seen, _, every = kept.partition(" ")
            try:
                seen = int(seen)
                silent = 3 * float(every) if every else self.away_after
            except ValueError:
                continue
            here = now - seen <= silent * 1000
            if not here and now - seen > _FORGOTTEN_AFTER:
                continue
            before = found.get(name)
            if before is not None:
                seen, here = max(seen, before.last_seen), here or before.here
            found[name] = Member(name, seen, here)
        return [found[name] for name in sorted(found)]

    def here(self):
        """The names of the agents that were heard from a short while ago."""
        return [member.name for member in self.members() if member.here]

    def watch(self):
        """The names on the roster now, and again each time an agent joins
        or leaves. Use it in a `for` loop. An agent going silent does not
        show here: read `here()` for that."""
        return _follow.together(self._team.client, [self._map], lambda: [member.name for member in self.members()])


def _silent(now, kept, away_after):
    """True if the entry `kept` has not been heard from for longer than it
    may be and still count as here."""
    seen, _, every = kept.partition(" ")
    try:
        return now - int(seen) > (3 * float(every) if every else away_after) * 1000
    except ValueError:
        return False
