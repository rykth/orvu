"""A budget: a shared amount that cannot be overspent."""

import threading

import pytest

import orvu


def test_a_budget_is_topped_up_and_spent(team):
    tokens = team.budget("tokens")
    assert tokens.left() == 0
    tokens.top_up(10)
    assert tokens.spend(4) is True
    assert tokens.left() == 6
    tokens.top_up(1)
    assert tokens.left() == 7


def test_spending_more_than_is_left_is_refused_and_changes_nothing(team):
    tokens = team.budget("tokens")
    tokens.top_up(10)
    assert tokens.spend(11) is False
    assert tokens.left() == 10
    assert tokens.spend(10) is True
    assert tokens.spend(1) is False
    assert tokens.left() == 0


def test_spending_from_a_budget_nobody_topped_up_is_refused(team):
    assert team.budget("tokens").spend(1) is False
    assert team.budget("tokens").left() == 0
    assert team.index() == {}


def test_budgets_with_different_names_are_separate(team):
    team.budget("tokens").top_up(10)
    team.budget("money").top_up(3)
    assert team.budget("money").spend(5) is False
    assert team.budget("tokens").spend(5) is True
    assert (team.budget("tokens").left(), team.budget("money").left()) == (5, 3)


@pytest.mark.parametrize("amount", [0, -1, 1.5, "5", None, True])
def test_an_amount_has_to_be_a_whole_number_above_zero(team, amount):
    tokens = team.budget("tokens")
    tokens.top_up(10)
    with pytest.raises(orvu.BadRequest, match="whole number"):
        tokens.top_up(amount)
    with pytest.raises(orvu.BadRequest, match="whole number"):
        tokens.spend(amount)
    assert tokens.left() == 10


@pytest.mark.parametrize("name", ["", "a/b", "two words", None])
def test_a_budget_needs_a_plain_name(team, name):
    with pytest.raises(orvu.BadRequest, match="name of a budget"):
        team.budget(name)


# Eight agents spend from one budget as fast as they can. However the spends
# fall, they cannot get more out of it than was put in.
def test_agents_spending_at_once_cannot_overspend(sidecar, team):
    team.budget("tokens").top_up(100)
    spent = []

    def agent(name):
        with orvu.Team.connect(socket=sidecar.socket, me=name) as mine:
            tokens = mine.budget("tokens")
            for _ in range(10):
                if tokens.spend(3):
                    spent.append(3)

    agents = [threading.Thread(target=agent, args=(f"agent-{number}",)) for number in range(8)]
    for thread in agents:
        thread.start()
    for thread in agents:
        thread.join(timeout=30)

    assert sum(spent) == 99, "33 spends of 3 fit into 100, and no more"
    assert team.budget("tokens").left() == 1


def test_a_budget_says_how_much_each_agent_spent(team, bob, sidecar):
    tokens = team.budget("tokens")
    assert tokens.spent_by() == {}
    tokens.top_up(100)
    assert tokens.spend(30) and bob.budget("tokens").spend(20) and tokens.spend(5)
    # What was refused is not counted.
    assert not bob.budget("tokens").spend(1000)
    assert tokens.spent_by() == {"alice": 35, "bob": 20}
    assert bob.budget("tokens").left() == 45

    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        assert nameless.budget("tokens").spend(5)
    assert tokens.spent_by() == {"alice": 35, "bob": 20}
    assert team.index()["budget-spent/tokens"] == "cart"


# A budget says how much each agent topped it up by. What a program that
# gave no name topped up is not counted for anyone.
def test_a_budget_says_how_much_each_agent_topped_up(team, bob, sidecar):
    tokens = team.budget("tokens")
    assert tokens.topped_up_by() == {}
    tokens.top_up(100)
    bob.budget("tokens").top_up(20)
    tokens.top_up(5)
    assert tokens.topped_up_by() == {"alice": 105, "bob": 20}
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        nameless.budget("tokens").top_up(7)
    assert tokens.topped_up_by() == {"alice": 105, "bob": 20}
    assert tokens.left() == 132
    assert team.index()["budget-topped/tokens"] == "cart"


# Spending and writing down who spent are two steps. If the second cannot
# be done, the money is still spent, and the caller has to be told so. Told
# of an error, it would spend again for the same work.
def test_a_spend_that_went_through_says_so_even_if_the_name_could_not_be_written(team):
    tokens = team.budget("tokens")
    tokens.top_up(100)

    def broken(*_):
        raise orvu.SidecarUnavailable("gone for a moment")

    tokens._spent.add = broken
    assert tokens.spend(30) is True
    assert tokens.left() == 70 and tokens.spent_by() == {}


@pytest.mark.parametrize("n", [2**63, 10**30])
def test_an_amount_too_large_to_keep_is_refused(team, n):
    with pytest.raises(orvu.BadRequest, match="too large"):
        team.budget("tokens").top_up(n)
    with pytest.raises(orvu.BadRequest, match="too large"):
        team.budget("tokens").spend(n)
    assert team.index() == {}
