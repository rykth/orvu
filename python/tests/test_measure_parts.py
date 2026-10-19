"""The pieces the measuring is made of, each by itself."""

import json
import os
import subprocess
import sys

import pytest

import orvu
from conftest import free_port, orvud_bin
from orvu import _machines, demo_machines, measure


@pytest.fixture
def machines(monkeypatch):
    monkeypatch.setenv("ORVUD_BIN", str(orvud_bin()))
    with _machines.LocalMachines(["alpha", "beta"], ports=[free_port(), free_port()]) as running:
        running.start()
        yield running


# A time that is published comes from many tries, not from one. One slow
# moment would otherwise be taken for how long the thing takes.
def test_a_time_is_the_middle_one_of_several_tries():
    tries = []
    took = measure.middle(lambda: tries.append(1), times=7)
    assert len(tries) == 7 and took >= 0


# The spending that is timed has to go through. A budget is shared out
# between the machines, so what was topped up here is not all here.
def test_every_spend_that_is_timed_goes_through(machines):
    with orvu.Team.connect("measure", socket=machines.socket("alpha"), me="alpha") as team:
        tokens = measure.budget_to_spend(team, 300)
        assert [tokens.spend(1) for _ in range(300)] == [True] * 300


# The board is measured at the sizes the table says, whatever was on it.
def test_the_board_is_filled_to_exactly_the_size_that_is_named(machines):
    with orvu.Team.connect("measure", socket=machines.socket("alpha"), me="alpha") as team:
        for number in range(7):
            team.tasks.add(f"Already there {number}")
        measure.fill_board(team, 20)
        assert len(team.tasks.list()) == 20
        measure.fill_board(team, 35)
        assert len(team.tasks.list()) == 35


# "Several programs" are programs: each is a process of its own, so that
# what is measured is the sidecar and not one Python waiting for itself.
def test_a_worker_is_a_program_of_its_own_that_reports_its_waits(machines):
    worker = subprocess.Popen(
        [sys.executable, "-m", "orvu.measure", "worker", str(machines.socket("alpha")), "3", "5"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env={**os.environ},
    )
    assert worker.stdout.readline().strip() == "ready"
    worker.stdin.write("go\n")
    worker.stdin.flush()
    said = json.loads(worker.stdout.readline())
    assert worker.wait(timeout=10) == 0
    assert len(said["waits"]) == 5 and said["started"] <= said["finished"]
    with orvu.Team.connect("measure", socket=machines.socket("alpha")) as team:
        assert team.notes.get("status-3") == "step 4"


# ---- the demo's own checks ---------------------------------------------------


# Overspent means that more work was paid for than money left the budget.
# What is left can never be below zero, so that alone says nothing.
@pytest.mark.parametrize(
    "paid_for, left, expected",
    [(8400, 600, False), (9000, 0, False), (9100, 0, True), (8000, 1200, True), (0, 9000, False)],
)
def test_overspent_is_work_paid_for_beyond_what_left_the_budget(paid_for, left, expected):
    assert demo_machines.overspent(paid_for, left) is expected


# An agent is taken to have gone only when it has been missing for several
# looks in a row. One late heartbeat is not an agent that stopped.
def test_an_agent_is_gone_only_after_several_looks_without_it():
    quiet = demo_machines.Quiet(looks=3)
    assert quiet.gone({"alpha", "gamma"}) == set()
    assert quiet.gone({"alpha"}) == set()  # gamma missing once
    assert quiet.gone({"alpha", "gamma"}) == set()  # and back: the count starts over
    assert quiet.gone({"alpha"}) == set()
    assert quiet.gone({"alpha"}) == set()
    assert quiet.gone({"alpha"}) == {"gamma"}
    assert quiet.gone({"alpha"}) == {"gamma"}
    # An agent that was never seen is not "gone".
    assert "beta" not in quiet.gone({"alpha"})
    assert quiet.gone({"alpha", "gamma"}) == set()


# A task that was put back has no result. Adding up what was paid for must
# not fall over it.
def test_a_task_with_no_result_counts_for_nothing():
    class Task:
        result = None

    class Paid:
        result = {"tokens": 300}

    assert demo_machines.tokens(Task()) == 0 and demo_machines.tokens(Paid()) == 300


# An agent that cannot reach its machine has failed, and says so. Only the
# demo itself knows which agent it switched off on purpose.
def test_an_agent_that_cannot_reach_its_machine_fails(tmp_path):
    ran = subprocess.run(
        [sys.executable, "-m", "orvu.demo_machines", "agent", "alpha", str(tmp_path / "nothing-here.sock")],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert ran.returncode != 0
