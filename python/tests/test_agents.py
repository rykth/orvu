"""Agents with keys. A sidecar that names agents lets in only programs that
give one of their keys, and tells each which agent it is. The library then
uses that name, so one agent's name cannot be put on another's work."""

import os
import subprocess
import sys

import pytest

import orvu

from conftest import wait_until


def test_with_no_agents_nothing_changes(sidecar):
    with orvu.connect(sidecar.socket) as client:
        assert client.agent is None
    with orvu.Team.connect(socket=sidecar.socket, me="alice") as team:
        assert team.me == "alice"


def test_an_agent_is_who_its_key_says(sidecar):
    key = sidecar.add_agent("alice")
    with orvu.connect(sidecar.socket, key=key) as client:
        assert client.agent == "alice"
    with orvu.Team.connect(socket=sidecar.socket, key=key) as team:
        assert team.me == "alice"
        assert team.tasks.add("Draft").created_by == "alice"
        # The roster keeps a connection of its own, with the same key.
        team.roster.join()
        assert "alice" in team.roster.here()


def test_a_program_without_a_known_key_is_not_let_in(sidecar):
    sidecar.add_agent("alice")
    for key in (None, "orvu-key-0123"):
        with pytest.raises(orvu.NotAllowed):
            orvu.connect(sidecar.socket, key=key)


# A key given to a sidecar that checks none is refused, so that a program
# that expects to be checked finds out that it is not.
def test_a_key_for_a_sidecar_with_no_agents_is_refused(sidecar):
    with pytest.raises(orvu.NotAllowed, match="has no agents"):
        orvu.connect(sidecar.socket, key="orvu-key-0123")


def test_an_agent_cannot_take_another_name(sidecar):
    key = sidecar.add_agent("alice")
    with pytest.raises(orvu.BadRequest, match="alice"):
        orvu.Team.connect(socket=sidecar.socket, key=key, me="bob")
    with orvu.Team.connect(socket=sidecar.socket, key=key, me="alice") as team:
        assert team.me == "alice"


def test_the_key_can_come_from_the_environment(sidecar, monkeypatch):
    monkeypatch.setenv("ORVU_KEY", sidecar.add_agent("alice"))
    with orvu.Team.connect(socket=sidecar.socket) as team:
        assert team.me == "alice"


def test_an_agent_that_is_removed_is_turned_away(sidecar):
    alice = orvu.connect(sidecar.socket, key=sidecar.add_agent("alice"))
    bob = orvu.connect(sidecar.socket, key=sidecar.add_agent("bob"))
    alice.counter("visits").increment(1)
    sidecar.remove_agent("alice")
    with pytest.raises(orvu.NotAllowed):
        alice.counter("visits").increment(1)
    assert bob.counter("visits").value() == 1
    alice.close()
    bob.close()


# A program that came in before the first agent was added does not keep
# working without a key.
def test_adding_the_first_agent_turns_away_programs_without_a_key(sidecar):
    with orvu.connect(sidecar.socket) as client:
        client.counter("visits").increment(1)
        sidecar.add_agent("alice")
        with pytest.raises(orvu.NotAllowed):
            client.counter("visits").value()


# After a restart the sidecar has forgotten every session. The client opens
# a new one with the same key, and is the same agent: removing alice turns
# it away.
def test_an_agent_is_still_itself_after_the_sidecar_restarts(sidecar):
    sidecar.add_agent("bob")
    with orvu.connect(sidecar.socket, key=sidecar.add_agent("alice")) as client:
        client.counter("visits").increment(1)
        sidecar.restart()
        assert wait_until(lambda: client.counter("visits").value() == 1)
        assert client.agent == "alice"
        sidecar.remove_agent("alice")
        with pytest.raises(orvu.NotAllowed):
            client.counter("visits").value()


# A watch that is open when its agent is removed ends, even if what it
# watches never changes again.
def test_an_open_watch_ends_when_its_agent_is_removed(sidecar):
    sidecar.add_agent("bob")
    with orvu.connect(sidecar.socket, key=sidecar.add_agent("alice")) as client:
        client.counter("visits").increment(1)
        seen = []
        with pytest.raises(orvu.NotAllowed):
            for value in client.counter("visits").watch():
                seen.append(value)
                sidecar.remove_agent("alice")
        assert seen == [1]


# Once there have been agents, removing the last one does not open the
# sidecar to everybody again.
def test_removing_the_last_agent_lets_nobody_in(sidecar):
    key = sidecar.add_agent("alice")
    sidecar.remove_agent("alice")
    for given in (None, key):
        with pytest.raises(orvu.NotAllowed, match="lets nobody in"):
            orvu.connect(sidecar.socket, key=given)


@pytest.mark.parametrize("key", [3, b"orvu-key-00"])
def test_a_key_is_text(sidecar, key):
    with pytest.raises(orvu.BadRequest, match="key"):
        orvu.connect(sidecar.socket, key=key)


# An agent's program gives the key to `orvu mcp` in its environment, and
# needs no --me: its name is the one the key was given.
def test_the_mcp_server_takes_its_name_from_the_key(sidecar):
    env = {**os.environ, "ORVU_KEY": sidecar.add_agent("alice")}
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp", "--socket", str(sidecar.socket)],
        input='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}\n',
        capture_output=True, text=True, env=env, timeout=60,
    )
    assert '\\"you\\": \\"alice\\"' in done.stdout, (done.stdout, done.stderr)
