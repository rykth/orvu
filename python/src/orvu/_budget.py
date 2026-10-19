"""A budget: a shared amount the team can never spend more of than there is."""

from . import _follow, _names
from . import errors
from ._tasks import _sent


class Budget:
    """One budget, such as tokens or money. Get it from `team.budget(name)`.

        tokens = team.budget("tokens")
        tokens.top_up(10_000)
        if tokens.spend(1_500):
            ...   # there was enough, and it is now spent
        else:
            ...   # there was not enough, and nothing was spent

    What is left can never go below zero, even when agents on several
    machines spend at the same moment.
    """

    def __init__(self, team, name):
        self._team = team
        self.name = _names.part(name, "the name of a budget")
        self._counter = team.client.bounded_counter(_names.budget(team.name, self.name))
        self._spent = team.client.cart(_names.spent(team.name, self.name))
        self._topped = team.client.cart(_names.topped(team.name, self.name))

    @staticmethod
    def _amount(n):
        # True and False count as numbers in Python. They are not amounts.
        if isinstance(n, bool) or not isinstance(n, int) or n <= 0:
            raise errors.BadRequest(f"an amount has to be a whole number above zero, not {n!r}")
        if n > _names.LARGEST:
            raise errors.BadRequest(f"{n} is too large to be an amount")
        return n

    def top_up(self, n):
        """Adds `n` to the budget. An agent with a name is counted as having
        topped it up by `n`, in the same message to the sidecar."""
        n = self._amount(n)
        self._team._record(f"budget/{self.name}", "bounded")
        if self._team.me is None:
            self._counter.increment(n)
            return
        self._team._record(f"budget-topped/{self.name}", "cart")
        with self._team.client.batch() as batch:
            batch.bounded_counter(self._counter.name).increment(n)
            batch.cart(self._topped.name).add(self._team.me, n)
        _sent(batch)

    def spend(self, n):
        """Takes `n` out of the budget. Returns `True` if there was enough.
        If there was not, returns `False` and nothing is spent."""
        if not self._counter.try_decrement(self._amount(n)):
            return False
        # Written after the spend, and only if it went through. If the
        # program ends right here, or the sidecar goes away, the amount is
        # spent and not counted for anyone.
        if self._team.me is not None:
            try:
                self._team._record(f"budget-spent/{self.name}", "cart")
                self._spent.add(self._team.me, n)
            except errors.OrvuError:
                # The amount is spent whether or not the name could be
                # written. Raising here would tell the caller that the
                # spend failed, and it would spend again for the same work.
                pass
        return True

    def spent_by(self):
        """How much each agent has spent from this budget, as a dict of
        agent to amount. What a program that gave no name spent is not in
        it."""
        return self._spent.items()

    def topped_up_by(self):
        """How much each agent has topped this budget up by, as a dict of
        agent to amount. What a program that gave no name added is not in
        it."""
        return self._topped.items()

    def left(self):
        """How much is left."""
        return self._counter.value()

    def watch(self):
        """What is left now, and again each time it changes. Use it in a
        `for` loop."""
        return _follow.together(self._team.client, [self._counter], self.left)
