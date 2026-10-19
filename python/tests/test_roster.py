"""The roster: who is on the team, and who is there now."""

import pytest

import orvu
from conftest import wait_until


def test_an_agent_joins_and_leaves(team, bob):
    assert team.roster.members() == [] and team.roster.here() == []
    team.roster.join()
    bob.roster.join()
    assert team.roster.here() == ["alice", "bob"]
    assert [member.name for member in bob.roster.members()] == ["alice", "bob"]
    assert team.index()["roster"] == "map"
    bob.roster.leave()
    assert team.roster.here() == ["alice"]
    bob.roster.leave()  # leaving twice does nothing


# An agent that crashed says nothing more. It stays on the roster and stops
# counting as here.
def test_an_agent_that_went_silent_fades_out(team, sidecar):
    team.roster.join(every=0.2)
    crashed = orvu.Team.connect(socket=sidecar.socket, me="bob")
    crashed.roster.join(every=0.2)
    assert team.roster.here() == ["alice", "bob"]
    # The program ends with no goodbye: its "still here" stops.
    crashed.roster._stop.set()
    assert wait_until(lambda: team.roster.here() == ["alice"], 5)
    names = {member.name: member.here for member in team.roster.members()}
    assert names == {"alice": True, "bob": False}
    crashed.roster._stop = None
    crashed.close()


def test_an_agent_that_joined_keeps_saying_it_is_here(team, bob):
    team.roster.join(every=0.1)
    first = bob.roster.members()[0].last_seen
    assert wait_until(lambda: bob.roster.members()[0].last_seen > first, 5)


def test_closing_the_team_takes_the_agent_off_the_roster(team, sidecar):
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.roster.join()
        assert team.roster.here() == ["bob"]
    assert team.roster.members() == []


def test_joining_needs_a_name_and_a_sensible_time(team, sidecar):
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        with pytest.raises(orvu.BadRequest, match="needs a name"):
            nameless.roster.join()
        assert nameless.roster.members() == []
    for every in (0, -1, "soon", True):
        with pytest.raises(orvu.BadRequest, match="seconds above zero"):
            team.roster.join(every=every)
    assert team.roster.members() == [] and team.index() == {}


def test_joining_again_does_not_start_a_second_heartbeat(team):
    import threading

    def beating():
        return sum(thread.name == "orvu-roster" for thread in threading.enumerate())

    assert beating() == 0
    team.roster.join(every=0.1)
    team.roster.join(every=0.1)
    team.roster.join(every=0.1)
    assert beating() == 1
    team.roster.leave()
    assert beating() == 0


def test_the_roster_is_watched(team, bob):
    watching = team.roster.watch()
    assert next(watching) == []
    bob.roster.join()
    assert next(watching) == ["bob"]
    bob.roster.leave()
    assert next(watching) == []
    watching.close()


# The heartbeat goes on through a sidecar that restarts.
def test_the_agent_is_still_here_after_the_sidecar_restarts(team, sidecar):
    team.roster.join(every=0.1)
    before = team.roster.members()[0].last_seen
    sidecar.restart()

    def last_seen():
        # The sidecar may still be on its way up.
        try:
            return team.roster.members()[0].last_seen
        except (orvu.SidecarUnavailable, IndexError):
            return 0

    assert wait_until(lambda: last_seen() > before, 10)
    # A beat that was on its way when the sidecar stopped is sent again once
    # it is back, with the time it was first sent at. The next one is fresh.
    assert wait_until(lambda: team.roster.here() == ["alice"], 5)


# How long an agent may be silent goes with the agent, not with whoever
# reads the roster. A reader that never joined judges it the same way.
def test_an_agent_is_judged_by_its_own_heartbeat(team, sidecar):
    quick = orvu.Team.connect(socket=sidecar.socket, me="quick")
    quick.roster.join(every=0.2)
    slow = orvu.Team.connect(socket=sidecar.socket, me="slow")
    slow.roster.join(every=1000)
    assert team.roster.here() == ["quick", "slow"]
    quick.roster._stop.set()
    slow.roster._stop.set()
    # Quick is away after three of its beats. Slow has hours yet.
    assert wait_until(lambda: team.roster.here() == ["slow"], 5)
    for crashed in (quick, slow):
        crashed.roster._stop = None
        crashed.close()


# The heartbeat has a connection of its own. Sharing the program's would
# let the two get in each other's way when the sidecar restarts.
def test_the_heartbeat_does_not_use_the_programs_own_connection(team):
    import threading
    import time

    callers = set()
    real = team.client._call

    def call(send, **options):
        callers.add(threading.current_thread().name)
        return real(send, **options)

    team.client._call = call
    team.roster.join(every=0.05)
    first = team.roster.members()[0].last_seen
    assert wait_until(lambda: team.roster.members()[0].last_seen > first, 5)
    time.sleep(0.2)
    assert callers == {"MainThread"}, callers
    team.roster.leave()


# Two programs can run under one name. When one of them leaves, the agent is
# still here, because the other still is.
def test_one_of_two_programs_under_a_name_leaving_does_not_take_the_name_off(team, sidecar):
    first = orvu.Team.connect(socket=sidecar.socket, me="bob")
    second = orvu.Team.connect(socket=sidecar.socket, me="bob")
    first.roster.join()
    second.roster.join()
    assert team.roster.here() == ["bob"]
    assert [member.name for member in team.roster.members()] == ["bob"]
    first.close()
    assert team.roster.here() == ["bob"], "the name went with the first program to leave"
    second.close()
    assert team.roster.members() == []


# A join that cannot be finished leaves nothing running and nothing open.
def test_a_join_that_fails_leaves_nothing_behind(team, sidecar):
    import threading

    # Something else is kept where the roster goes, so the first write fails.
    team.client.counter("orvu/default/roster").increment(1)
    made = []
    real = team.client.another

    def another():
        made.append(real())
        return made[-1]

    team.client.another = another
    with pytest.raises(orvu.OrvuError):
        team.roster.join()
    assert not any(thread.name == "orvu-roster" for thread in threading.enumerate())
    with pytest.raises(Exception):
        made[0].map("x").get("y")  # the connection it opened is closed again
    team.roster.leave()  # and there is nothing to leave


def _left_behind(team, name, minutes_ago, every=15):
    """Writes an entry the way a program that ended with no goodbye leaves
    it: under the agent's name, last heard `minutes_ago`."""
    import time

    seen = int(time.time() * 1000 - minutes_ago * 60_000)
    team.client.map("orvu/default/roster").put(f"{name}\x1fdeadbeef", f"{seen} {every}")


# A program that ended with no goodbye leaves its entry behind. The next
# program under that name clears it when it joins.
def test_joining_clears_what_an_ended_program_of_the_same_name_left(team, bob):
    _left_behind(team, "bob", minutes_ago=10)
    _left_behind(team, "carol", minutes_ago=10)
    bob.roster.join()
    keys = sorted(team.client.map("orvu/default/roster").entries())
    assert [key.partition("\x1f")[0] for key in keys] == ["bob", "carol"], keys
    assert "bob\x1fdeadbeef" not in keys, "the entry the ended program left is still there"
    assert [(member.name, member.here) for member in team.roster.members()] == [("bob", True), ("carol", False)]


# An entry that has been silent for more than a day is not listed at all.
def test_an_agent_silent_for_more_than_a_day_is_not_listed(team):
    _left_behind(team, "bob", minutes_ago=2 * 24 * 60)
    _left_behind(team, "carol", minutes_ago=2 * 60)
    assert [(member.name, member.here) for member in team.roster.members()] == [("carol", False)]
    assert team.roster.here() == []


# Clearing what was left behind leaves alone another program under the same
# name that still runs, and what it was told was the way to beat.
def test_joining_leaves_another_running_program_of_the_same_name_alone(team, sidecar):
    first = orvu.Team.connect(socket=sidecar.socket, me="bob")
    second = orvu.Team.connect(socket=sidecar.socket, me="bob")
    first.roster.join()
    second.roster.join()
    keys = team.client.map("orvu/default/roster").entries()
    assert [key.partition("\x1f")[0] for key in keys] == ["bob", "bob"], keys
    first.close()
    second.close()
