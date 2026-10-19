"""A team: the one object the building blocks hang off."""

from . import _client, _names, errors
from ._budget import Budget
from ._documents import Document
from ._inbox import Inbox
from ._notes import Notes
from ._roster import Roster
from ._search import Found
from ._tally import Tally
from ._tasks import Tasks


class Team:
    """Everything a team of agents shares. Every agent that connects with the
    same team name sees the same notes, tasks and budgets.

        from orvu import Team

        with Team.connect(me="researcher-2") as team:
            team.notes.put("deploy-date", "Friday")
            task = team.tasks.add("Summarise the Q3 report")
            if team.budget("tokens").spend(1500):
                ...
    """

    def __init__(self, client, name="default", me=None):
        #: The team's name.
        self.name = _names.part(name, "the name of a team")
        # A client that gave a key is the agent the sidecar said it is, and
        # cannot go by another name.
        if client.agent is not None and me not in (None, client.agent):
            raise errors.BadRequest(f"this key belongs to the agent {client.agent!r}, not {me!r}")
        #: The name of the agent using this object, or `None`. With a key, it
        #: is the name the sidecar gave the key.
        self.me = client.agent or me
        #: The plain connection underneath, for anything the blocks do not cover.
        self.client = client
        self._recorded = set()
        self._index = client.map(_names.index(self.name))
        #: The team's notes.
        self.notes = Notes(self)
        #: The team's task board.
        self.tasks = Tasks(self)
        #: Messages between the agents.
        self.inbox = Inbox(self)
        #: Who is on the team.
        self.roster = Roster(self)

    @classmethod
    def connect(cls, name="default", *, me=None, socket=None, **options):
        """Connects to the sidecar and returns the team called `name`.

        `me` is a name for the agent that is connecting, such as
        "researcher-2". Tasks it adds, claims and finishes are marked with it.
        An agent with a key, given as `key=` or in `ORVU_KEY`, needs no `me`:
        it is the agent the sidecar knows the key by, and so does one given a
        connection file, as `connection=` or in `ORVU_CONNECT`. `socket`,
        `connection` and anything else given are passed on to
        `orvu.connect`; with neither, the socket in `ORVU_SOCKET` is used if
        that is set, such as a team's sidecar shared with other machines.
        """
        # Checked first, so that a bad name does not leave a connection open.
        _names.part(name, "the name of a team")
        client = _client.connect(socket, **options)
        try:
            return cls(client, name, me)
        except BaseException:
            client.close()
            raise

    def close(self):
        """Takes this agent off the roster, if it had joined, and closes the
        connection."""
        try:
            self.roster.leave()
        except errors.OrvuError:
            # The sidecar is away. The agent fades from the roster by itself.
            pass
        self.client.close()

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()

    def __repr__(self):
        return f"Team({self.name!r})"

    def budget(self, name):
        """The budget called `name`, such as "tokens"."""
        return Budget(self, name)

    def document(self, name):
        """The document called `name`, such as "q3-report"."""
        return Document(self, name)

    def documents(self):
        """The names of the documents that have been written to."""
        return sorted(name[len("doc/") :] for name in self.index() if name.startswith("doc/"))

    def tally(self, name):
        """The tally called `name`, such as "tokens-used"."""
        return Tally(self, name)

    def tallies(self):
        """The names of the tallies that have been counted on."""
        return sorted(name[len("tally/") :] for name in self.index() if name.startswith("tally/"))

    def budgets(self):
        """The names of the budgets that have been topped up."""
        return sorted(name[len("budget/") :] for name in self.index() if name.startswith("budget/"))

    def search(self, query, limit=5):
        """The team's notes, structured notes, paragraphs of documents and
        tasks whose meaning is closest to `query`, the closest first: a list
        of at most `limit` `Found`, from 1 to 50.

            for found in team.search("what is the refund policy?"):
                print(found.kind, found.name, found.text)

        The last of what is found may have little to do with the question:
        its `score` shows how close each is. A change shows up in what is
        found within a moment.
        An agent with rules finds only what it may read. The first search
        on a sidecar fetches the model it needs from the internet, 30 MB,
        unless the sidecar was given one with `--model`; `SearchUnavailable`
        says when it cannot be had.
        """
        return [Found._of(found) for found in self.client.search(self.name, query, limit)]

    def index(self):
        """What the team has, as a dict of short name to kind of object:
        `{"notes": "map", "budget/tokens": "bounded", ...}`.

        The sidecar cannot list the objects it holds, so the blocks write
        each one down here the first time they create it.
        """
        return self._index.entries()

    def _record(self, short_name, kind):
        """Writes an object down in the index, once per connection."""
        if short_name not in self._recorded:
            self._index.put(short_name, kind)
            self._recorded.add(short_name)

    def _forget(self, short_names):
        """Takes objects that can never be used again off the index. The
        objects themselves stay where they are: the sidecar cannot remove
        one."""
        with self.client.batch() as batch:
            for short_name in short_names:
                batch.map(self._index.name).remove(short_name)
                self._recorded.discard(short_name)
        for pending in batch.results:
            pending.value
