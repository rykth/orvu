"""The building blocks on two machines.

Two real sidecars run as machines 1 and 2 over the encrypted link. Each test
makes its changes while the machines cannot reach each other, connects them,
and checks what both end up with.
"""

import pathlib
import subprocess

import pytest

import orvu

from conftest import short_folder, wait_until
from test_agreement import Machine, free_port, keygen_bin


@pytest.fixture
def machines():
    """Machines 1 and 2, running and not yet told about each other."""
    with short_folder() as folder:
        folder = pathlib.Path(folder)
        subprocess.run(
            [keygen_bin(), "--out", folder / "certs", "--replicas", "1,2"],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        one = Machine(folder / "one", 1, free_port())
        two = Machine(folder / "two", 2, free_port())
        one.start()
        two.start()
        try:
            yield one, two
        finally:
            one.stop()
            two.stop()


def connect(one, two):
    """Stops both machines and starts them again knowing about each other."""
    one.stop()
    two.stop()
    one.start(peers=[two])
    two.start(peers=[one])


def teams(one, two):
    return (
        orvu.Team.connect(socket=one.socket, me="alice"),
        orvu.Team.connect(socket=two.socket, me="bob"),
    )


def agree(alice, bob, read, seconds=20):
    """Waits until both machines read the same, and returns what they read."""
    assert wait_until(lambda: read(alice) == read(bob), seconds=seconds), (
        f"the machines still disagree: {read(alice)!r} and {read(bob)!r}"
    )
    return read(alice)


def test_tasks_added_on_two_machines_all_end_up_on_both(machines):
    one, two = machines
    alice, bob = teams(one, two)
    alice.tasks.add("from machine one")
    bob.tasks.add("from machine two")
    assert len(alice.tasks.list()) == 1 and len(bob.tasks.list()) == 1
    alice.close(), bob.close()

    connect(one, two)
    alice, bob = teams(one, two)
    assert wait_until(lambda: len(alice.tasks.list()) == 2 and len(bob.tasks.list()) == 2, seconds=20)
    tasks = agree(alice, bob, lambda team: team.tasks.list())
    assert sorted(task.title for task in tasks) == ["from machine one", "from machine two"]
    assert alice.index() == bob.index()
    alice.close(), bob.close()


# The soft claim's known limit: two agents that cannot see each other both
# get the task. Once the machines are connected, they have to agree on which
# of the two holds it, so that at least everyone sees the same thing.
def test_a_task_claimed_on_two_machines_ends_up_with_one_holder(machines):
    one, two = machines
    connect(one, two)
    alice, bob = teams(one, two)
    task = alice.tasks.add("Draft")
    assert wait_until(lambda: bob.tasks.get(task) is not None, seconds=20)
    alice.close(), bob.close()

    # Apart again, each claims the task.
    one.stop(), two.stop()
    one.start(), two.start()
    alice, bob = teams(one, two)
    assert alice.tasks.claim(task) is True
    assert bob.tasks.claim(task) is True
    alice.close(), bob.close()

    connect(one, two)
    alice, bob = teams(one, two)
    holder = agree(alice, bob, lambda team: team.tasks.get(task).claimed_by)
    assert holder in ("alice", "bob")
    assert agree(alice, bob, lambda team: team.tasks.get(task).state) == "doing"
    alice.close(), bob.close()


def test_notes_put_on_two_machines_all_end_up_on_both(machines):
    one, two = machines
    alice, bob = teams(one, two)
    alice.notes.put("from-one", "1")
    bob.notes.put("from-two", "2")
    alice.notes.structured("plan").set(["owner"], "alice")
    bob.notes.structured("plan").set(["reviewer"], "bob")
    alice.close(), bob.close()

    connect(one, two)
    alice, bob = teams(one, two)
    assert wait_until(lambda: len(alice.notes.all()) == 2 and len(bob.notes.all()) == 2, seconds=20)
    assert agree(alice, bob, lambda team: team.notes.all()) == {"from-one": "1", "from-two": "2"}
    plan = agree(alice, bob, lambda team: team.notes.structured("plan").to_value())
    assert plan == {"owner": "alice", "reviewer": "bob"}
    alice.close(), bob.close()


# Two agents write one note while their machines cannot reach each other.
# Once they can, both see both values and who wrote each, and the next write
# settles it for both.
def test_a_note_written_on_two_machines_at_once_clashes_until_written_again(machines):
    one, two = machines
    alice, bob = teams(one, two)
    alice.notes.put("deploy-date", "Friday")
    bob.notes.put("deploy-date", "Monday")
    alice.notes.put("owner", "alice")
    alice.close(), bob.close()

    connect(one, two)
    alice, bob = teams(one, two)
    clash = orvu.Clash("deploy-date", ["Friday", "Monday"], ["alice", "bob"])
    assert agree(alice, bob, lambda team: team.notes.all()) == {"deploy-date": clash, "owner": "alice"}
    for team in (alice, bob):
        with pytest.raises(orvu.Clash) as raised:
            team.notes.get("deploy-date")
        assert raised.value.values == ["Friday", "Monday"]
        assert raised.value.authors == ["alice", "bob"]
        assert "Write it again" in str(raised.value)
        assert team.notes.author("deploy-date") == ["alice", "bob"]
        assert team.notes.get("owner") == "alice"

    bob.notes.put("deploy-date", "Tuesday")
    assert agree(alice, bob, lambda team: team.notes.all()) == {"deploy-date": "Tuesday", "owner": "alice"}
    assert alice.notes.author("deploy-date") == "bob"
    alice.close(), bob.close()


# A machine that is cut off cannot spend what another machine put in. That is
# cautious, and it is what keeps the budget from being overspent.
def test_a_machine_that_is_cut_off_cannot_spend_another_machines_money(machines):
    one, two = machines
    connect(one, two)
    alice, bob = teams(one, two)
    alice.budget("tokens").top_up(100)
    assert wait_until(lambda: bob.budget("tokens").left() == 100, seconds=20)
    alice.close(), bob.close()

    one.stop(), two.stop()
    one.start(), two.start()
    alice, bob = teams(one, two)
    assert bob.budget("tokens").left() == 100
    assert bob.budget("tokens").spend(10) is False
    assert alice.budget("tokens").spend(10) is True
    alice.close(), bob.close()


# Connected, the machine that has none asks the other for some. Both spend
# until they are refused. Together they cannot get out more than went in.
def test_two_machines_spending_from_one_budget_cannot_overspend(machines):
    one, two = machines
    connect(one, two)
    alice, bob = teams(one, two)
    alice.budget("tokens").top_up(100)
    assert wait_until(lambda: bob.budget("tokens").left() == 100, seconds=20)

    spent = {"alice": 0, "bob": 0}
    assert bob.budget("tokens").spend(30) is True, "bob could not get any from the machine that had it"
    spent["bob"] += 30
    for _ in range(20):
        for name, team in (("alice", alice), ("bob", bob)):
            if team.budget("tokens").spend(7):
                spent[name] += 7

    total = spent["alice"] + spent["bob"]
    assert total <= 100
    left = agree(alice, bob, lambda team: team.budget("tokens").left())
    assert left == 100 - total
    assert left < 7, f"{left} was left over that nobody could spend"
    alice.close(), bob.close()
