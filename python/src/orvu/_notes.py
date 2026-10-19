"""Notes: small facts the team shares, each under a name."""

from . import _follow, _names, _values
from . import errors
from ._handles import Json

# What comes between the writer's name and the note's text, in each value.
_SEPARATOR = "\x1f"


class Notes:
    """The team's notes. Get it from `team.notes`.

        team.notes.put("deploy-date", "Friday")
        team.notes.get("deploy-date")   # "Friday"

    A simple note is a name and a value: text, a number, true or false, or
    a list or dict of those. If two agents put the same name at the same
    moment, neither having seen the other's, the note keeps both values and
    says so: `get` raises `orvu.Clash`, which holds them, until the next `put`
    settles it. For a note with several fields that different agents fill
    in, use `structured`.
    """

    def __init__(self, team):
        self._team = team
        self._map = team.client.mv_map(_names.notes(team.name))

    @staticmethod
    def _name(name):
        if not isinstance(name, str) or not name:
            raise errors.BadRequest(f"a note's name has to be some text, not {name!r}")
        return name

    def put(self, name, value):
        """Writes a note. `name` can be any text, and `value` anything JSON
        can hold. It comes back from `get` as what it was: a number as a
        number, a list as a list. It replaces every value the note holds, so
        it settles a clash."""
        if value is None:
            raise errors.BadRequest("a note cannot hold None. Use `remove` to take a note away")
        me = self._team.me or ""
        if _SEPARATOR in me:
            raise errors.BadRequest(f"an agent whose name holds {_SEPARATOR!r} cannot write notes")
        name, kept = self._name(name), _values.to_text(value)
        self._team._record("notes-mv", "mvmap")
        self._map.put(name, me + _SEPARATOR + kept)

    def get(self, name):
        """The note's value, or `None` if there is no such note. Raises
        `orvu.Clash` if agents wrote it at the same time and nothing has
        settled it."""
        name = self._name(name)
        found = _settled(name, self._map.get(name))
        if isinstance(found, errors.Clash):
            raise found
        return found

    def remove(self, name):
        """Removes a note, every value it holds. Removing one that is not
        there does nothing."""
        self._map.remove(self._name(name))

    def all(self):
        """Every simple note, as a dict of name to value. A note that agents
        wrote at the same time is given as an `orvu.Clash`, which holds its
        values."""
        return {name: _settled(name, written) for name, written in self._notes().items()}

    def author(self, name):
        """The agent that wrote the note, or `None` if there is no such note
        or it was written by a program that gave no name. For a note that
        clashes, a list of who wrote each of its values."""
        name = self._name(name)
        return _author(name, self._map.get(name))

    def authors(self):
        """Who wrote each note, as a dict of note name to agent, or to a list
        of agents for a note that clashes."""
        return {name: _author(name, written) for name, written in self._notes().items()}

    def watch(self):
        """All the simple notes now, and again each time one changes. Use it
        in a `for` loop. A note that clashes is given as an `orvu.Clash`."""
        return _follow.together(self._team.client, [self._map], self.all)

    def _notes(self):
        # The empty name marks that notes kept the old way were moved over.
        return {name: written for name, written in self._map.entries().items() if name}

    def structured(self, name):
        """A note that is a JSON document, with fields and lists inside it.
        Two agents that change different fields both keep their change.

            plan = team.notes.structured("plan")
            plan.set(["owner"], "researcher-2")
            plan.get(["owner"])   # "researcher-2"

        What comes back works as an ordinary `orvu.Json` does.
        """
        name = _names.part(name, "the name of a structured note")
        return _Structured(self._team, name)

    def structured_names(self):
        """The names of the structured notes that have been written to."""
        return sorted(name[len("note/") :] for name in self._team.index() if name.startswith("note/"))


class _Structured(Json):
    """A structured note. It is a JSON document that is written down in the
    team's index the first time something is written to it, so that only
    looking at a note leaves no trace."""

    def __init__(self, team, name):
        super().__init__(team.client, _names.note(team.name, name))
        self._team = team
        self._short_name = f"note/{name}"

    def set(self, path, value):
        self._team._record(self._short_name, "json")
        return Json.set(self, path, value)

    def remove(self, path):
        self._team._record(self._short_name, "json")
        return Json.remove(self, path)

    set.__doc__ = Json.set.__doc__
    remove.__doc__ = Json.remove.__doc__


def _written(values):
    """Each value of a note as it is kept, as who wrote it and its text,
    with one entry for each different text, in the order the sidecar gives
    them."""
    seen = {}
    for kept in values:
        by, found, text = kept.partition(_SEPARATOR)
        if not found:
            by, text = "", kept
        seen.setdefault(text, by or None)
    return [(by, text) for text, by in seen.items()]


def _settled(name, values):
    """The note's value, `None` if it has none, or the `Clash` it is."""
    written = _written(values)
    if not written:
        return None
    if len(written) == 1:
        return _values.from_text(written[0][1])
    return errors.Clash(name, [_values.from_text(text) for _, text in written], [by for by, _ in written])


def _author(name, values):
    written = _written(values)
    if not written:
        return None
    if len(written) == 1:
        return written[0][0]
    return [by for by, _ in written]
