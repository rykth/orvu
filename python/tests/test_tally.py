"""Tallies: counting things."""

import threading

import pytest

import orvu


def test_a_tally_counts_per_agent_and_in_total(team, bob):
    used = team.tally("tokens-used")
    assert used.total() == 0 and used.by_name() == {}
    used.add(1500)
    used.add()
    bob.tally("tokens-used").add(200)
    assert used.by_name() == {"alice": 1501, "bob": 200}
    assert bob.tally("tokens-used").total() == 1701


def test_a_count_can_go_under_any_name(team, sidecar):
    errors = team.tally("errors")
    errors.add(under="timeout")
    errors.add(2, under="bad reply")
    errors.add(under="timeout")
    assert errors.by_name() == {"timeout": 2, "bad reply": 2}
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        nameless.tally("errors").add(3)
    assert errors.by_name()["(no name)"] == 3 and errors.total() == 7


@pytest.mark.parametrize("n", [0, -1, 1.5, "2", True, None])
def test_a_tally_only_goes_up_by_whole_numbers(team, n):
    with pytest.raises(orvu.BadRequest, match="whole number above zero"):
        team.tally("errors").add(n)
    assert team.tally("errors").total() == 0
    assert team.index() == {}


@pytest.mark.parametrize("under", ["", 7, ["a"]])
def test_what_a_count_goes_under_is_text(team, under):
    with pytest.raises(orvu.BadRequest, match="some text"):
        team.tally("errors").add(1, under=under)


def test_the_team_lists_its_tallies(team):
    assert team.tallies() == []
    team.tally("errors").total()
    assert team.tallies() == []
    team.tally("errors").add()
    team.tally("tokens-used").add(5)
    assert team.tallies() == ["errors", "tokens-used"]
    assert team.index()["tally/errors"] == "cart"
    with pytest.raises(orvu.BadRequest, match="name of a tally"):
        team.tally("two words")


# Many agents counting at the same moment lose nothing.
def test_nothing_is_lost_when_many_count_at_once(sidecar, team):
    def agent(name):
        with orvu.Team.connect(socket=sidecar.socket, me=name) as mine:
            for _ in range(25):
                mine.tally("done").add()

    agents = [threading.Thread(target=agent, args=(f"agent-{number}",)) for number in range(8)]
    for thread in agents:
        thread.start()
    for thread in agents:
        thread.join()
    assert team.tally("done").total() == 200
    assert set(team.tally("done").by_name().values()) == {25}


def test_a_tally_is_watched(team, bob):
    watching = team.tally("done").watch()
    assert next(watching) == {}
    bob.tally("done").add(3)
    assert next(watching) == {"bob": 3}
    watching.close()


# A number too large to be sent is refused like any other that cannot be
# counted, before anything is written down.
@pytest.mark.parametrize("n", [2**63, 2**64, 10**30])
def test_a_count_too_large_to_keep_is_refused(team, n):
    with pytest.raises(orvu.BadRequest, match="too large"):
        team.tally("tokens").add(n)
    assert team.index() == {}
