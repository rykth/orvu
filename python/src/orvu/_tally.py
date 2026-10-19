"""Tallies: counting things, per agent and in total."""

from . import _follow, _names
from . import errors


class Tally:
    """One count the team keeps, such as tokens used or errors seen. Get it
    from `team.tally(name)`.

        used = team.tally("tokens-used")
        used.add(1500)
        used.total()      # 1500
        used.by_name()    # {"researcher-2": 1500}

    Every machine counts by itself and the counts add up. Nothing is lost
    when two agents count at the same moment, or while machines are cut off
    from each other. A tally only goes up. For an amount that is spent and
    must not run out, use a budget.
    """

    #: What is counted under when no name is given and the agent has none.
    NOBODY = "(no name)"

    def __init__(self, team, name):
        self._team = team
        self.name = _names.part(name, "the name of a tally")
        self._cart = team.client.cart(_names.tally(team.name, self.name))

    def add(self, n=1, under=None):
        """Counts `n` more. It is counted under this agent's name, or under
        `under` if that is given: any text, such as the kind of error."""
        # True and False count as numbers in Python. They are not counts.
        if isinstance(n, bool) or not isinstance(n, int) or n <= 0:
            raise errors.BadRequest(f"a tally counts up by a whole number above zero, not {n!r}")
        if n > _names.LARGEST:
            raise errors.BadRequest(f"{n} is too large to count in one go")
        if under is not None and (not isinstance(under, str) or not under):
            raise errors.BadRequest(f"what a count goes under has to be some text, not {under!r}")
        self._team._record(f"tally/{self.name}", "cart")
        self._cart.add(under or self._team.me or self.NOBODY, n)

    def total(self):
        """Everything counted so far."""
        return sum(self._cart.items().values())

    def by_name(self):
        """What was counted under each name, as a dict."""
        return self._cart.items()

    def watch(self):
        """The counts by name now, and again each time one changes. Use it
        in a `for` loop."""
        return _follow.together(self._team.client, [self._cart], self.by_name)
