"""Firm claims, and claims that run out.

A firm task has a ticket, and only the agent that takes it gets the task. A
claim says how long it lasts without word from its agent, and a claim whose
agent has gone quiet for longer is abandoned: another agent can take the task
over.
"""

import threading
import time

import pytest

import orvu
from orvu import _names

from conftest import wait_until


# ---- firm tasks ----------------------------------------------------------------


def test_a_firm_task_is_claimed_by_one_agent(sidecar, team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert (task.firm, task.ticket) == (True, 0)
    assert team.tasks.add("Draft").firm is False
    assert bob.tasks.claim(task) is True
    assert team.tasks.claim(task) is False
    found = team.tasks.get(task)
    assert (found.state, found.claimed_by) == ("doing", "bob")
    # Claiming what is already yours is fine, and takes no second ticket.
    assert bob.tasks.claim(task) is True


# The soft claim's known limit is two agents told yes at the same moment.
# Many agents race for many firm tasks at once, and each task is won once.
def test_agents_racing_for_firm_tasks_win_each_once(sidecar, team):
    tasks = [team.tasks.add(f"task {number}", firm=True) for number in range(20)]
    agents = [orvu.Team.connect(socket=sidecar.socket, me=f"agent-{number}") for number in range(6)]
    won = {task.id: [] for task in tasks}
    start = threading.Barrier(len(agents))

    def race(agent):
        start.wait()
        for task in tasks:
            if agent.tasks.claim(task):
                won[task.id].append(agent.me)

    threads = [threading.Thread(target=race, args=(agent,)) for agent in agents]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    for agent in agents:
        agent.close()
    assert all(len(winners) == 1 for winners in won.values()), won
    assert all(team.tasks.get(task).claimed_by == won[task.id][0] for task in tasks)


def test_a_firm_task_given_back_can_be_claimed_again(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim(task) is True
    assert bob.tasks.release(task) is False
    assert team.tasks.release(task) is True
    assert team.tasks.release(task) is False, "a task was given back twice"
    assert bob.tasks.claim(task) is True
    assert team.tasks.claim(task) is False


# Setting a firm task back to "todo" by hand frees it, whoever had it. Its old
# holder is no longer on it, even though its ticket was never given back.
def test_a_firm_task_set_back_by_hand_is_free_again(sidecar, team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.claim(task) is True
    freed = team.tasks.update(task, state="todo")
    assert (freed.state, freed.claimed_by, freed.ticket) == ("todo", None, 1)
    assert bob.tasks.still_working(task) is False
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        assert carol.tasks.claim(task) is True
        assert bob.tasks.claim(task) is False


# Another agent is setting a firm task back at the same moment, and has its
# takeover ticket. This one is told, and nothing changes.
def test_a_firm_task_whose_takeover_ticket_is_taken_cannot_be_set_back(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    bob.tasks.claim(task)
    assert team.client.bounded_counter(_names.task_takeover(team.name, task.id, 0)).try_decrement(1) is True
    with pytest.raises(orvu.TicketUnavailable, match=task.id):
        team.tasks.update(task, state="todo", notes="again")
    found = team.tasks.get(task)
    assert (found.state, found.claimed_by, found.notes, found.ticket) == ("doing", "bob", None, 0)


def test_a_finished_firm_task_can_be_opened_again(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim(task) is True
    team.tasks.finish(task, "sent")
    assert bob.tasks.claim(task) is False
    team.tasks.update(task, state="todo")
    assert bob.tasks.claim(task) is True
    assert team.tasks.claim(task) is False


def test_a_firm_tasks_tickets_are_in_the_index(team):
    task = team.tasks.add("Send the invoice", firm=True)
    index = team.index()
    assert index[f"task-ticket/{task.id}/0"] == "bounded"
    assert index[f"task-takeover/{task.id}/0"] == "bounded"
    assert _names.task_ticket("crew", task.id, 3) == f"orvu/crew/task-ticket/{task.id}/3"
    assert _names.task_takeover("crew", task.id, 3) == f"orvu/crew/task-takeover/{task.id}/3"


@pytest.mark.parametrize("firm", ["yes", 1])
def test_firm_is_true_or_false(team, firm):
    with pytest.raises(orvu.BadRequest, match="firm"):
        team.tasks.add("Draft", firm=firm)
    assert team.tasks.list() == []
    # None means none, as everywhere on a task.
    assert team.tasks.add("Draft", firm=None).firm is False


# ---- claims that run out -----------------------------------------------------


def test_a_claim_says_when_its_agent_was_last_heard_from(team):
    task = team.tasks.add("Draft")
    before = int(time.time() * 1000)
    assert team.tasks.claim(task, lease=60) is True
    found = team.tasks.get(task)
    assert before <= found.seen <= int(time.time() * 1000)
    assert found.lease == 60
    assert found.abandoned() is False
    # Without a lease, a claim lasts a quarter of an hour.
    other = team.tasks.add("Check the figures")
    team.tasks.claim(other)
    assert team.tasks.get(other).lease == orvu.LEASE == 900


# A claim that moved on to another ticket under the same name, with the same
# time and lease, is a new claim, and the wait for it starts again.
def test_a_claim_on_a_new_ticket_starts_the_wait_again(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim(task, lease=1) is True
    assert bob.tasks.get(task).abandoned() is False
    time.sleep(1.2)
    assert bob.tasks.get(task).abandoned() is True
    team.client.json(_names.tasks(team.name)).set([task.id, "ticket"], 7)
    assert bob.tasks.get(task).abandoned() is False


def test_still_working_keeps_a_claim_from_running_out(team):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=1)
    for _ in range(3):
        time.sleep(0.5)
        assert team.tasks.still_working(task) is True
    assert team.tasks.get(task).abandoned() is False
    time.sleep(1.2)
    assert team.tasks.get(task).abandoned() is True


def test_an_abandoned_claim_is_taken_over(team, bob):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=1)
    assert bob.tasks.claim(task) is False
    time.sleep(1.2)
    assert bob.tasks.claim(task) is True
    found = team.tasks.get(task)
    assert (found.state, found.claimed_by, found.abandoned()) == ("doing", "bob", False)
    # The agent that went quiet finds out when it next says it is working.
    assert team.tasks.still_working(task) is False
    assert team.tasks.release(task) is False
    assert team.tasks.get(task).claimed_by == "bob"


def test_an_abandoned_firm_claim_is_taken_over_by_one_agent(sidecar, team):
    tasks = [team.tasks.add(f"task {number}", firm=True) for number in range(10)]
    for task in tasks:
        assert team.tasks.claim(task, lease=1) is True
    agents = [orvu.Team.connect(socket=sidecar.socket, me=f"agent-{number}") for number in range(4)]
    # Each agent sees the claims, and then they go quiet for longer than they
    # last.
    for agent in agents:
        agent.tasks.list()
    time.sleep(1.2)
    won = {task.id: [] for task in tasks}
    start = threading.Barrier(len(agents))

    def race(agent):
        start.wait()
        for task in tasks:
            if agent.tasks.claim(task):
                won[task.id].append(agent.me)

    threads = [threading.Thread(target=race, args=(agent,)) for agent in agents]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    for agent in agents:
        agent.close()
    assert all(len(winners) == 1 for winners in won.values()), won
    for task in tasks:
        found = team.tasks.get(task)
        assert (found.claimed_by, found.ticket) == (won[task.id][0], 1)
        assert team.tasks.still_working(task) is False


# The writes of a batch are not seen all at once, so another agent can read
# a task halfway through a takeover. It must not see the new ticket number
# with the old, run-out time beside it: it would take the new ticket's
# takeover ticket, and two agents would have the task. So the fresh time is
# written first. Which order the writes go in is checked here, because a
# race shows it only now and then.
def test_a_takeover_writes_the_fresh_time_before_the_new_ticket(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task, lease=1)
    bob.tasks.get(task)
    time.sleep(1.2)
    written = []
    plain = orvu.Json.set
    monkeypatch.setattr(orvu.Json, "set", lambda handle, path, value: written.append(path[-1]) or plain(handle, path, value))
    assert bob.tasks.claim(task) is True
    assert written.index("seen") < written.index("ticket")
    assert written.index("seen") < written.index("claimed_by")


# ---- the number of the ticket -------------------------------------------------
#
# Whoever holds a firm task holds one numbered ticket, and the number goes up
# each time the task moves on. A step that must not happen twice carries the
# number to whatever it changes, and that side refuses a number lower than
# the highest it has seen.


def test_a_firm_claim_gives_the_number_of_its_ticket(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim_ticket(task) == 0
    # Claiming it again keeps it, and gives the same number.
    assert team.tasks.claim_ticket(task) == 0
    assert bob.tasks.claim_ticket(task) is None
    assert team.tasks.still_holding(task, 0) is True
    assert bob.tasks.still_holding(task, 0) is False
    assert team.tasks.release(task) is True
    assert team.tasks.still_holding(task, 0) is False
    assert bob.tasks.claim_ticket(task) == 1
    assert bob.tasks.still_holding(task, 1) is True
    assert bob.tasks.still_holding(task, 0) is False
    assert bob.tasks.finish(task, "sent") is True
    assert bob.tasks.still_holding(task, 1) is False


def test_only_a_firm_task_has_a_ticket(team):
    task = team.tasks.add("Draft")
    with pytest.raises(orvu.BadRequest, match="firm"):
        team.tasks.claim_ticket(task)
    with pytest.raises(orvu.BadRequest, match="firm"):
        team.tasks.still_holding(task, 0)
    assert team.tasks.get(task).state == "todo"


# The agent that takes a task over gets a higher number than the one that
# went quiet. Whatever the old holder still does carries the lower number,
# and a side that keeps the highest number it has seen refuses it.
def test_a_takeover_gets_a_higher_number_and_the_old_one_is_refused(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim_ticket(task, lease=1) == 0
    bob.tasks.get(task)
    time.sleep(1.2)
    assert bob.tasks.claim_ticket(task) == 1
    assert team.tasks.still_holding(task, 0) is False
    assert bob.tasks.still_holding(task, 1) is True

    highest = {}

    def send_invoice(ticket):
        if ticket < highest.get(task.id, -1):
            return False
        highest[task.id] = ticket
        return True

    assert send_invoice(1) is True
    assert send_invoice(0) is False, "the agent that lost the task sent the invoice again"


# The writes of a batch are not seen all at once. In a takeover, the old
# holder's name is taken off before the new number is written, so the name
# and the number seen together always belong together: an agent that went
# quiet can never be shown beside the new number and think it holds it.
def test_a_takeover_takes_the_old_name_off_before_the_new_number(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task, lease=1)
    bob.tasks.get(task)
    time.sleep(1.2)
    written = []
    plain_set, plain_remove = orvu.Json.set, orvu.Json.remove
    monkeypatch.setattr(orvu.Json, "set", lambda handle, path, value: written.append(("set", path[-1])) or plain_set(handle, path, value))
    monkeypatch.setattr(orvu.Json, "remove", lambda handle, path: written.append(("remove", path[-1])) or plain_remove(handle, path))
    assert bob.tasks.claim(task) is True
    assert written.index(("remove", "claimed_by")) < written.index(("set", "ticket")) < written.index(("set", "claimed_by")), written


# The agent that took a firm task over gives it back, and the next agent can
# take it. The old holder, which never gave back its own ticket, cannot.
def test_a_firm_task_taken_over_and_given_back_goes_to_one_agent(sidecar, team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task, lease=1)
    bob.tasks.get(task)
    time.sleep(1.2)
    assert bob.tasks.claim(task) is True
    assert bob.tasks.release(task) is True
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        assert carol.tasks.claim(task) is True
        assert team.tasks.claim(task) is False
        assert bob.tasks.claim(task) is False


# Every agent that reads the board sees that a task was taken over, from
# whom, by whom and when, not only the two agents on it.
@pytest.mark.parametrize("firm", [False, True])
def test_a_takeover_is_written_on_the_task_for_everyone(sidecar, team, bob, firm):
    task = team.tasks.add("Draft", firm=firm)
    team.tasks.claim(task, lease=1)
    assert team.tasks.get(task).taken_over is None
    bob.tasks.get(task)
    time.sleep(1.2)
    before = int(time.time() * 1000)
    assert bob.tasks.claim(task) is True
    after = int(time.time() * 1000)
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        found = carol.tasks.get(task)
        assert (found.taken_over["by"], found.taken_over["from"]) == ("bob", "alice")
        assert before <= found.taken_over["at"] <= after
        assert [listed.taken_over for listed in carol.tasks.list()] == [found.taken_over]
        assert next(iter(carol.tasks.watch()))[0].taken_over == found.taken_over
    # It stays as a record of what happened when the task is given back.
    assert bob.tasks.release(task) is True
    assert team.tasks.get(task).taken_over == found.taken_over


def test_a_claim_that_takes_over_nothing_writes_no_takeover(team, bob):
    task = team.tasks.add("Draft")
    assert team.tasks.claim(task) is True
    assert team.tasks.release(task) is True
    assert bob.tasks.claim(task) is True
    assert bob.tasks.get(task).taken_over is None
    assert team.client.json(_names.tasks(team.name)).get([task.id, "taken_over"]) is None


def test_a_finished_task_is_never_abandoned(team, bob):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=1)
    team.tasks.finish(task, "done")
    time.sleep(1.2)
    assert team.tasks.get(task).abandoned() is False
    assert bob.tasks.claim(task) is False


# A claim written before claims ran out has no time on it. It is left alone.
def test_a_claim_with_no_time_on_it_does_not_run_out(team, bob):
    task = team.tasks.add("Draft")
    team.client.json(_names.tasks(team.name)).set([task.id, "claimed_by"], "alice")
    team.client.json(_names.tasks(team.name)).set([task.id, "state"], "doing")
    found = team.tasks.get(task)
    assert (found.seen, found.lease, found.abandoned()) == (None, None, False)
    assert bob.tasks.claim(task) is False


def test_abandoned_can_be_asked_about_any_moment(team):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=10)
    found = team.tasks.get(task)
    # Just inside and just past the lease, not on it: the steady clock reads
    # in fractions of a millisecond, and adding the lease to such a reading
    # and taking it off again can be off by a hair either way.
    assert found.abandoned(now=found.quiet_since + 9_999.9) is False
    assert found.abandoned(now=found.quiet_since + 10_000.1) is True
    assert team.tasks.get(team.tasks.add("Check the figures")).abandoned(now=found.quiet_since + 10**9) is False


# ---- clocks that do not agree -------------------------------------------------
#
# Whether a claim has run out goes by the clock of the agent that wants the
# task: it has run out when that agent has seen no change to it for as long
# as it lasts. The time the agent on the task wrote, by its own clock, is
# never compared with another machine's clock.


def _written_at(team, task, seen):
    """Writes `seen` on the task as its last word, as an agent whose clock is
    wrong would."""
    team.client.json(_names.tasks(team.name)).set([_id(task), "seen"], seen)


def _id(task):
    return task if isinstance(task, str) else task.id


def test_a_claim_written_by_a_clock_far_behind_does_not_run_out_at_once(team, bob):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=60)
    _written_at(team, task, int(time.time() * 1000) - 3_600_000)
    assert bob.tasks.get(task).abandoned() is False
    assert bob.tasks.claim(task) is False
    assert team.tasks.get(task).claimed_by == "alice"


def test_a_claim_written_by_a_clock_far_ahead_still_runs_out(team, bob):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=1)
    _written_at(team, task, int(time.time() * 1000) + 3_600_000)
    assert bob.tasks.get(task).abandoned() is False
    time.sleep(1.2)
    assert bob.tasks.get(task).abandoned() is True
    assert bob.tasks.claim(task) is True


# An agent that has only just started has not seen the claim before, and
# waits a whole lease from now, however old the time written on it is.
def test_an_agent_that_has_just_started_waits_a_whole_lease(sidecar, team):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=1)
    time.sleep(1.2)
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        assert carol.tasks.get(task).abandoned() is False
        assert carol.tasks.claim(task) is False
        time.sleep(1.2)
        assert carol.tasks.claim(task) is True


# Word from the agent on the task starts the wait again, whatever time it
# wrote.
def test_any_word_from_the_agent_starts_the_wait_again(team, bob):
    task = team.tasks.add("Draft")
    team.tasks.claim(task, lease=1)
    _written_at(team, task, 5)
    bob.tasks.get(task)
    time.sleep(0.7)
    _written_at(team, task, 4)
    assert bob.tasks.get(task).abandoned() is False
    time.sleep(0.7)
    assert bob.tasks.get(task).abandoned() is False
    time.sleep(0.5)
    assert bob.tasks.get(task).abandoned() is True


@pytest.mark.parametrize("lease", [0, -1, 1.5, True, "60", 10**9])
def test_a_lease_is_a_whole_number_of_seconds(team, lease):
    task = team.tasks.add("Draft")
    with pytest.raises(orvu.BadRequest, match="lease"):
        team.tasks.claim(task, lease=lease)
    assert team.tasks.get(task).state == "todo"


def test_still_working_is_only_for_the_agent_on_the_task(team, bob):
    task = team.tasks.add("Draft")
    assert team.tasks.still_working(task) is False
    team.tasks.claim(task)
    assert bob.tasks.still_working(task) is False
    team.tasks.finish(task)
    assert team.tasks.still_working(task) is False
    with pytest.raises(orvu.UnknownTask):
        team.tasks.still_working("t-00000000")


# ---- an agent that is behind ----------------------------------------------------
#
# What other agents do reaches an agent a moment later, and on another machine
# it can be much later. These tests show an agent a task as it was before the
# last change, and check that what it does then cannot give a firm task two
# holders, or leave it stuck.


def _behind(monkeypatch, agent, view, reads=1):
    """Shows `agent` the task as `view` the next `reads` times it reads it,
    as if what changed since had not reached it yet."""
    real = agent.tasks._existing
    shown = []

    def existing(task, parents=True):
        if len(shown) < reads:
            shown.append(view)
            return view
        return real(task, parents)

    monkeypatch.setattr(agent.tasks, "_existing", existing)


def test_an_agent_that_is_behind_cannot_claim_back_a_firm_task_it_lost(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task, lease=1)
    old = team.tasks.get(task)
    bob.tasks.get(task)
    time.sleep(1.2)
    assert bob.tasks.claim(task) is True
    _behind(monkeypatch, team, old)
    assert team.tasks.claim(task) is False
    found = team.tasks.get(task)
    assert (found.claimed_by, found.ticket) == ("bob", 1)


# An agent claims a firm task it holds, but first sees it as it was before
# it gave the task back and claimed it again. The number it is told is the
# one on the task now, not the old one it was shown.
def test_claiming_a_firm_task_again_gives_the_number_it_holds_now(team, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim_ticket(task) == 0
    old = team.tasks.get(task)
    assert team.tasks.release(task) is True
    assert team.tasks.claim_ticket(task) == 1
    _behind(monkeypatch, team, old, reads=2)
    assert team.tasks.claim_ticket(task) == 1


# A firm task that nobody had is set back by hand. Its old ticket must not
# stay free beside the new one, or an agent that has not heard of the new one
# takes the old.
def test_a_free_firm_task_set_back_by_hand_has_one_free_ticket(sidecar, team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    old = team.tasks.get(task)
    assert team.tasks.update(task, state="todo").ticket == 1
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        assert carol.tasks.claim(task) is True
    _behind(monkeypatch, bob, old)
    assert bob.tasks.claim(task) is False
    assert team.tasks.get(task).claimed_by == "carol"


def test_a_firm_task_given_back_too_late_stays_with_whoever_took_it_over(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task, lease=1)
    old = team.tasks.get(task)
    bob.tasks.get(task)
    time.sleep(1.2)
    assert bob.tasks.claim(task) is True
    _behind(monkeypatch, team, old)
    assert team.tasks.release(task) is False
    found = team.tasks.get(task)
    assert (found.state, found.claimed_by, found.ticket) == ("doing", "bob", 1)
    assert bob.tasks.still_working(task) is True


# A claim that took the ticket and then failed to write its name does not keep
# the ticket: the task would be stuck, with nobody on it and nobody able to
# claim it.
def test_a_firm_claim_that_fails_halfway_lets_the_task_go(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    plain = orvu.Json.set

    def broken(handle, path, value):
        if path[-1] == "claimed_by":
            raise orvu.SidecarUnavailable("the line went down")
        return plain(handle, path, value)

    monkeypatch.setattr(orvu.Json, "set", broken)
    with pytest.raises(orvu.SidecarUnavailable):
        team.tasks.claim(task)
    monkeypatch.setattr(orvu.Json, "set", plain)
    found = team.tasks.get(task)
    assert (found.state, found.claimed_by, found.ticket) == ("todo", None, 1)
    assert bob.tasks.claim(task) is True


def test_only_the_agent_on_a_firm_task_can_finish_it(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.finish(task, "sent") is False
    assert team.tasks.claim(task) is True
    assert bob.tasks.finish(task, "sent") is False
    found = team.tasks.get(task)
    assert (found.state, found.result) == ("doing", None)
    assert team.tasks.finish(task, "sent") is True
    # Any agent can finish a task that is not firm, as before.
    draft = team.tasks.add("Draft")
    assert bob.tasks.finish(draft, "ok") is True


# A firm task given back, or set back by hand, moves on to a new ticket. Its
# new number is written last, after the name and time of the agent that had
# it are gone: an agent that saw the new number beside an old, run-out time
# would take the task over while another claims the new ticket.
@pytest.mark.parametrize("how", ["given back", "set back"])
def test_a_firm_task_that_moves_on_writes_its_new_ticket_last(team, monkeypatch, how):
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task)
    written = []
    plain_set, plain_remove = orvu.Json.set, orvu.Json.remove
    monkeypatch.setattr(orvu.Json, "set", lambda handle, path, value: written.append(path[-1]) or plain_set(handle, path, value))
    monkeypatch.setattr(orvu.Json, "remove", lambda handle, path: written.append(path[-1]) or plain_remove(handle, path))
    if how == "given back":
        assert team.tasks.release(task) is True
    else:
        team.tasks.update(task, state="todo")
    assert "ticket" in written, written
    assert all(written.index(field) < written.index("ticket") for field in ("state", "claimed_by", "seen", "lease")), written
    assert team.tasks.get(task).ticket == 1


def _ticket_lines(team, task):
    """The lines for a task's tickets in the team's list of what it has."""
    return sorted(name for name in team.index() if name.split("/")[0] in ("task-ticket", "task-takeover") and name.split("/")[1] == task.id)


def _pair(task, number):
    return [f"task-takeover/{task.id}/{number}", f"task-ticket/{task.id}/{number}"]


# A ticket that can never be had again is taken off the team's list: only the
# pair the task is on now is listed, however often it moved on.
def test_only_the_tickets_a_firm_task_is_on_are_in_the_index(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert _ticket_lines(team, task) == _pair(task, 0)
    team.tasks.claim(task, lease=1)
    bob.tasks.get(task)
    time.sleep(1.2)
    assert bob.tasks.claim(task) is True
    assert _ticket_lines(team, task) == _pair(task, 1)
    assert bob.tasks.release(task) is True
    assert _ticket_lines(team, task) == _pair(task, 2)
    assert team.index()[f"task-ticket/{task.id}/2"] == "bounded"


# Finished, a firm task keeps only the ticket it needs to be set back again,
# so finishing many tasks leaves no more behind than one line each, however
# often each of them moved on.
def test_finishing_many_firm_tasks_does_not_grow_what_is_kept_for_them(team, bob):
    tasks = [team.tasks.add(f"task {number}", firm=True) for number in range(8)]
    for task in tasks:
        for _ in range(3):
            assert team.tasks.claim(task) is True
            assert team.tasks.release(task) is True
        assert bob.tasks.claim(task) is True
        team.tasks.update(task, state="todo")
        assert bob.tasks.claim(task) is True
        assert bob.tasks.finish(task, "sent") is True
    for task in tasks:
        assert _ticket_lines(team, task) == [f"task-takeover/{task.id}/4"]
    assert len([name for name in team.index() if name.startswith("task-")]) == len(tasks)
    # It can still be opened again, and claimed.
    assert team.tasks.update(tasks[0], state="todo").ticket == 5
    assert _ticket_lines(team, tasks[0]) == _pair(tasks[0], 5)
    assert team.tasks.claim(tasks[0]) is True


# Taking spent tickets off the team's list is only tidying: the list is not
# what keeps a task firm. A sidecar that refuses it, as an older one with
# rules does, leaves the lines there, and the task goes on all the same. The
# lines are only taken off once the task's new number is on the board.
def test_a_firm_task_moves_on_when_its_old_tickets_cannot_be_taken_off_the_list(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    on_board = []

    def refused(short_names):
        on_board.append(team.tasks.get(task).ticket)
        raise orvu.NotAllowed("this sidecar does not let anything be taken off the index")

    monkeypatch.setattr(team, "_forget", refused)
    assert team.tasks.claim_ticket(task, lease=1) == 0
    assert team.tasks.release(task) is True
    assert on_board == [1]
    assert team.tasks.update(task, state="todo").ticket == 2
    assert on_board[-1] == 2
    assert team.tasks.claim_ticket(task, lease=1) == 2
    # A takeover, from another program whose tidying is refused too.
    monkeypatch.setattr(bob, "_forget", refused)
    bob.tasks.get(task)
    time.sleep(1.2)
    assert bob.tasks.claim_ticket(task) == 3
    assert on_board[-1] == 3
    _stopped_holding(team, _names.task_takeover(team.name, task.id, 3))
    forced = team.tasks.update(task, state="todo", force=True).ticket
    assert on_board[-1] == forced
    assert team.tasks.claim_ticket(task) == forced
    assert team.tasks.finish(task, "sent") is True


# ---- a firm task that is stuck --------------------------------------------------
#
# An agent that stops between taking a ticket and writing what it did leaves
# the ticket taken for good. Setting the task back by hand then fails every
# time, because a taken ticket cannot be told apart from one on a machine that
# cannot be reached. An operator who knows that agent has stopped for good
# sets the task back with force, which starts a fresh pair of tickets.


def _stopped_holding(team, name):
    """Takes a ticket as an agent that stopped right after would have."""
    assert team.client.bounded_counter(name).try_decrement(1) is True


def test_a_stuck_firm_task_is_set_back_only_with_force(sidecar, team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.claim_ticket(task) == 0
    # Another agent took the ticket for moving the task on, and stopped.
    _stopped_holding(team, _names.task_takeover(team.name, task.id, 0))
    with pytest.raises(orvu.TicketUnavailable, match="force=True"):
        team.tasks.update(task, state="todo")
    assert team.tasks.get(task).claimed_by == "bob"
    freed = team.tasks.update(task, state="todo", force=True)
    assert (freed.state, freed.claimed_by) == ("todo", None)
    # The fresh pair is far past any pair the task had, or that the agent
    # that stopped may have made before it did.
    assert freed.ticket > 2
    assert _ticket_lines(team, task) == _pair(task, freed.ticket)
    assert bob.tasks.still_working(task) is False
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        assert carol.tasks.claim_ticket(task) == freed.ticket
        assert bob.tasks.claim(task) is False
        assert team.tasks.claim(task) is False


# The agent claiming the task took its ticket and stopped before it wrote its
# name. With force, the task moves on, and that old ticket can never win it
# back, even if it turns up free again on a machine that was away.
def test_an_old_ticket_never_wins_a_task_set_back_with_force(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    old = team.tasks.get(task)
    _stopped_holding(team, _names.task_ticket(team.name, task.id, 0))
    with pytest.raises(orvu.TicketUnavailable):
        team.tasks.update(task, state="todo")
    forced = team.tasks.update(task, state="todo", force=True).ticket
    assert forced > 0
    team.client.bounded_counter(_names.task_ticket(team.name, task.id, 0)).increment(1)
    _behind(monkeypatch, bob, old)
    assert bob.tasks.claim(task) is False
    found = team.tasks.get(task)
    assert (found.state, found.claimed_by, found.ticket) == ("todo", None, forced)
    assert team.tasks.claim_ticket(task) == forced


def _claimers(sidecar, *names):
    return [orvu.Team.connect(socket=sidecar.socket, me=name) for name in names]


# Two operators set the same stuck task back with force at the same moment,
# both from what they read before either wrote, or one tries again after its
# write to the board failed. Each force lands on a pair of its own, so two
# agents claiming the task can never both be told the same number.
def test_two_forces_from_one_view_never_give_a_number_two_holders(sidecar, team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.claim_ticket(task) == 0
    stale = team.tasks.get(task)
    _stopped_holding(team, _names.task_takeover(team.name, task.id, 0))
    first = team.tasks.update(task, state="todo", force=True).ticket
    free = team.tasks.get(task)
    _behind(monkeypatch, team, stale)
    second = team.tasks.update(task, state="todo", force=True).ticket
    # It saw the first one's pair, and jumped from there, not from the old
    # number it was shown.
    assert second - first >= 2**31
    carol, dave = _claimers(sidecar, "carol", "dave")
    with carol, dave:
        told = [carol.tasks.claim_ticket(task)]
        # Dave has not heard of the second force yet, and sees the first.
        _behind(monkeypatch, dave, free, reads=2)
        told.append(dave.tasks.claim_ticket(task))
    numbers = [number for number in told if number is not None]
    assert len(numbers) == len(set(numbers)), told


# The same, when the second force cannot see the first at all, as on a
# machine cut off from the other operator's: it still picks another pair.
def test_a_force_that_cannot_see_another_still_picks_its_own_pair(team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.claim_ticket(task) == 0
    stale = team.tasks.get(task)
    nothing_yet = team.index()
    _stopped_holding(team, _names.task_takeover(team.name, task.id, 0))
    first = team.tasks.update(task, state="todo", force=True).ticket
    _behind(monkeypatch, team, stale)
    monkeypatch.setattr(team, "index", lambda: nothing_yet)
    team.tasks.update(task, state="todo", force=True)
    # Each force wrote its own number down, and they differ. The task's
    # number is the higher of the two, and each pair holds one.
    written = sorted(int(key) for key in team.client.json(_names.tasks(team.name)).get([task.id, "tickets"]))
    assert len(written) == 2 and first in written, written
    assert team.tasks.get(task).ticket == max(written)
    for number in written:
        assert team.client.bounded_counter(_names.task_ticket(team.name, task.id, number)).value() == 1


# A force made from an old view races an agent that took the task over and
# gave it back, which moved the task on twice. The force must not land on
# the pair the agent gave back.
def test_a_force_racing_a_takeover_never_gives_a_number_two_holders(sidecar, team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task, lease=1)
    stale = team.tasks.get(task)
    bob.tasks.get(task)
    time.sleep(1.2)
    assert bob.tasks.claim_ticket(task) == 1
    assert bob.tasks.release(task) is True
    given_back = team.tasks.get(task)
    assert given_back.ticket == 2
    _behind(monkeypatch, team, stale)
    forced = team.tasks.update(task, state="todo", force=True).ticket
    assert forced > 2
    carol, dave = _claimers(sidecar, "carol", "dave")
    with carol, dave:
        told = [carol.tasks.claim_ticket(task)]
        # Dave has not heard of the force, and sees the task as bob left it.
        _behind(monkeypatch, dave, given_back, reads=2)
        told.append(dave.tasks.claim_ticket(task))
    numbers = [number for number in told if number is not None]
    assert len(numbers) == len(set(numbers)), told


# What is written into a task can never stop the board from being read. A
# ticket number counts only if it is one to fifteen plain digits: anything
# else under "tickets", or a "ticket" that is not such a number, is left out.
# Both libraries read the same number from the same task.
ODD_KEYS = ["²", "1" * 5000, "１２", "1e3", "-5", " 7", "7 ", "", "0x10", "1000000000000000", "9" * 30]


def test_odd_ticket_numbers_are_left_out_and_never_break_reading(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim_ticket(task) == 0
    assert team.tasks.release(task) is True
    board = team.client.json(_names.tasks(team.name))
    for key in ODD_KEYS:
        board.set([task.id, "tickets", key], True)
    board.set([task.id, "tickets", "000000000000001"], True)
    for odd in (1e300, -3, 2.5, "9", 2**53 + 2):
        board.set([task.id, "ticket"], odd)
        assert [found.ticket for found in team.tasks.list()] == [1]
        assert team.tasks.get(task).ticket == 1
    assert bob.tasks.claim_ticket(task) == 1
    # Nor can any other field: tasks are put in order by when they were
    # added, and a time that is not a number counts as none.
    other = team.tasks.add("Check the figures")
    for odd in ("yesterday", None, [1], 1e300):
        board.set([task.id, "created"], odd)
        assert {found.id for found in team.tasks.list()} == {task.id, other.id}


# Numbers are compared as numbers, not as text: 10 is past 9.
def test_ticket_numbers_compare_as_numbers(team):
    task = team.tasks.add("Send the invoice", firm=True)
    board = team.client.json(_names.tasks(team.name))
    board.set([task.id, "tickets", "9"], True)
    board.set([task.id, "tickets", "10"], True)
    assert team.tasks.get(task).ticket == 10


# A force never goes past the highest number both libraries read the same.
# A task whose numbers are that high is told to be added again.
def test_a_force_never_goes_past_the_highest_number(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.claim_ticket(task) == 0
    _stopped_holding(team, _names.task_takeover(team.name, task.id, 0))
    board = team.client.json(_names.tasks(team.name))
    board.set([task.id, "tickets", "999990000000000"], True)
    # Its ticket for moving on was never made, so only force moves it on.
    forced = team.tasks.update(task, state="todo", force=True).ticket
    assert 999990000000000 < forced < 10**15
    board.set([task.id, "tickets", "999999999999999"], True)
    with pytest.raises(orvu.TicketUnavailable, match="used up.*Add the task again"):
        team.tasks.update(task, state="todo", force=True)
    assert team.tasks.get(task).ticket == 999999999999999


# A ticket holding more than one can be taken by more than one agent, so it
# is never taken: claiming the task says to set it back.
def test_a_doubled_ticket_is_never_taken(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    ticket = team.client.bounded_counter(_names.task_ticket(team.name, task.id, 0))
    ticket.increment(1)
    # The claim says so, and what to do: a plain set-back is enough.
    with pytest.raises(orvu.TicketUnavailable, match=r'made twice.*update\(task, state="todo"\)'):
        team.tasks.claim(task)
    with pytest.raises(orvu.TicketUnavailable, match="made twice"):
        bob.tasks.claim_ticket(task)
    assert ticket.value() == 2
    freed = team.tasks.update(task, state="todo")
    assert freed.ticket == 1
    assert team.tasks.claim_ticket(task) == 1


# The same for the ticket for moving a task on: two agents could both take
# it, and both make the next pair.
def test_a_doubled_takeover_ticket_is_never_taken(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.claim(task, lease=1) is True
    takeover = team.client.bounded_counter(_names.task_takeover(team.name, task.id, 0))
    takeover.increment(1)
    assert bob.tasks.release(task) is False
    with pytest.raises(orvu.TicketUnavailable):
        team.tasks.update(task, state="todo")
    # Taking the task over is refused, and says that only force helps.
    team.tasks.get(task)
    time.sleep(1.2)
    with pytest.raises(orvu.TicketUnavailable, match="made twice.*force=True"):
        team.tasks.claim(task)
    assert takeover.value() == 2
    assert team.tasks.update(task, state="todo", force=True).ticket > 1


# The number on a firm task never goes down, whatever reaches the board
# late. Bob holds ticket 0 on a machine that is cut off, so its ticket for
# moving on cannot be had, and the task is set back with force. Carol claims
# the forced number. Bob's machine comes back, and bob, who has not heard,
# gives the task back.
def _forced_while_bob_was_away(sidecar, team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    assert bob.tasks.claim_ticket(task) == 0
    bobs_view = bob.tasks.get(task)
    takeover = team.client.bounded_counter(_names.task_takeover(team.name, task.id, 0))
    assert takeover.try_decrement(1) is True
    forced = team.tasks.update(task, state="todo", force=True).ticket
    takeover.increment(1)
    return task, bobs_view, forced


def test_a_late_release_never_takes_a_forced_task_back_down(sidecar, team, bob, monkeypatch):
    task, bobs_view, forced = _forced_while_bob_was_away(sidecar, team, bob)
    carol, dave = _claimers(sidecar, "carol", "dave")
    with carol, dave:
        assert carol.tasks.claim_ticket(task) == forced
        _behind(monkeypatch, bob, bobs_view)
        assert bob.tasks.release(task) is False
        assert team.tasks.get(task).ticket == forced
        assert carol.tasks.still_holding(task, forced) is True
        assert dave.tasks.claim_ticket(task) is None


# The same, when bob's machine writes before it can hear of the force at
# all: its lower number lands on the board, and is not the task's number.
def test_a_lower_number_written_late_is_not_the_tasks_number(sidecar, team, bob, monkeypatch):
    task, bobs_view, forced = _forced_while_bob_was_away(sidecar, team, bob)
    carol, dave = _claimers(sidecar, "carol", "dave")
    with carol, dave:
        assert carol.tasks.claim_ticket(task) == forced
        _behind(monkeypatch, bob, bobs_view, reads=10)
        bob.tasks.release(task)
        assert team.tasks.get(task).ticket == forced
        assert dave.tasks.claim_ticket(task) in (None, forced)
        assert carol.tasks.claim_ticket(task) in (None, forced)


# Bob takes the task over. Between bob writing the new number and reading it
# back, the task is set back with force and carol claims it. Bob lost, and
# moving on from his ticket must not put a lower number on the task.
def test_a_takeover_that_loses_to_a_force_never_takes_the_number_down(sidecar, team, bob, monkeypatch):
    task = team.tasks.add("Send the invoice", firm=True)
    assert team.tasks.claim(task, lease=1) is True
    bob.tasks.get(task)
    time.sleep(1.2)
    carol, dave = _claimers(sidecar, "carol", "dave")
    with carol, dave:
        real = bob.tasks._existing
        calls, forced = [], []

        def existing(task, parents=True):
            calls.append(1)
            if len(calls) == 3:
                # Bob has written ticket 1 and is about to read it back. Its
                # ticket for moving on is out of reach for a moment.
                takeover = team.client.bounded_counter(_names.task_takeover(team.name, task_id, 1))
                assert takeover.try_decrement(1) is True
                forced.append(team.tasks.update(task_id, state="todo", force=True).ticket)
                takeover.increment(1)
                assert carol.tasks.claim_ticket(task_id) == forced[0]
            return real(task, parents)

        task_id = task.id
        monkeypatch.setattr(bob.tasks, "_existing", existing)
        assert bob.tasks.claim_ticket(task) is None
        monkeypatch.setattr(bob.tasks, "_existing", real)
        assert team.tasks.get(task).ticket == forced[0]
        assert carol.tasks.still_holding(task, forced[0]) is True
        assert dave.tasks.claim_ticket(task) is None


def test_force_does_nothing_more_when_it_is_not_needed(team, bob):
    task = team.tasks.add("Send the invoice", firm=True)
    bob.tasks.claim(task)
    assert team.tasks.update(task, state="todo", force=True).ticket == 1
    draft = team.tasks.add("Draft")
    assert team.tasks.update(draft, state="todo", force=True).state == "todo"


@pytest.mark.parametrize("force", ["yes", 1])
def test_force_is_true_or_false(team, force):
    task = team.tasks.add("Send the invoice", firm=True)
    with pytest.raises(orvu.BadRequest, match="force"):
        team.tasks.update(task, state="todo", force=force)


def test_force_is_only_for_setting_a_task_back(team):
    task = team.tasks.add("Send the invoice", firm=True)
    with pytest.raises(orvu.BadRequest, match="force"):
        team.tasks.update(task, notes="again", force=True)
    with pytest.raises(orvu.BadRequest, match="force"):
        team.tasks.update(task, state="done", force=True)
    assert team.tasks.get(task).notes is None


# ---- on several machines -------------------------------------------------------


# Agents on two machines claim the same firm tasks at the same moment. Each
# task is won once, and everyone agrees by whom.
def test_firm_tasks_raced_for_on_two_machines_are_won_once(machines):
    machines.start()
    alpha, beta, gamma = (machines.team(name) for name in machines.names)
    tasks = [alpha.tasks.add(f"task {number}", firm=True) for number in range(10)]
    assert wait_until(lambda: all(beta.tasks.get(task) and gamma.tasks.get(task) for task in tasks), seconds=20)
    won = {task.id: [] for task in tasks}
    start = threading.Barrier(2)

    def race(agent):
        start.wait()
        for task in tasks:
            if agent.tasks.claim(task):
                won[task.id].append(agent.me)

    threads = [threading.Thread(target=race, args=(agent,)) for agent in (beta, gamma)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert all(len(winners) == 1 for winners in won.values()), won
    for task in tasks:
        holder = won[task.id][0]
        assert wait_until(lambda: {team.tasks.get(task).claimed_by for team in (alpha, beta, gamma)} == {holder})


# The machine that holds a firm task's ticket is cut off. Nobody else can
# claim the task, or set it back, until it is back. That is the price of a
# firm claim, and what keeps it firm.
def test_a_firm_task_cannot_be_claimed_while_its_ticket_is_cut_off(machines):
    machines.start()
    alpha, beta, gamma = (machines.team(name) for name in machines.names)
    task = alpha.tasks.add("Send the invoice", firm=True)
    assert wait_until(lambda: beta.tasks.get(task) is not None, seconds=20)

    machines.freeze("alpha")
    try:
        assert beta.tasks.claim(task) is False
        with pytest.raises(orvu.TicketUnavailable, match=task.id):
            beta.tasks.update(task, state="todo")
        assert beta.tasks.get(task).state == "todo"
    finally:
        machines.thaw("alpha")
    assert wait_until(lambda: beta.tasks.claim(task), seconds=20)
    assert gamma.tasks.claim(task) is False
