"""Live updates on the building blocks."""

import threading
import time

import orvu

from conftest import wait_until


def follow(sidecar, pick, until):
    """Follows `pick(team).watch()` on a connection of its own, in the
    background, until `until(value)` is true. Returns the list the values are
    collected in, and the thread."""
    seen = []

    def loop():
        with orvu.Team.connect(socket=sidecar.socket, me="watcher") as watcher:
            for value in pick(watcher).watch():
                seen.append(value)
                if until(value):
                    return

    # A daemon, so that a loop that never ends fails its test and does not
    # hold up the whole run.
    thread = threading.Thread(target=loop, daemon=True)
    thread.start()
    return seen, thread


# Every block gives a first value at once, even when nothing has been written
# yet, so a program can show "nothing yet" without a special case.
def test_every_block_gives_a_first_value_at_once(team):
    for watched, empty in (
        (team.notes, {}),
        (team.tasks, []),
        (team.budget("tokens"), 0),
    ):
        for value in watched.watch():
            assert value == empty
            break
    assert team.index() == {}, "watching must not create anything"


def test_notes_are_followed_as_they_change(sidecar, team):
    seen, thread = follow(sidecar, lambda t: t.notes, lambda notes: notes.get("b") == "2")
    assert wait_until(lambda: seen)
    team.notes.put("a", "1")
    team.notes.put("b", "2")
    thread.join(timeout=5)
    assert not thread.is_alive(), f"the loop never saw the last note; it saw {seen}"
    assert seen[0] == {}
    assert seen[-1] == {"a": "1", "b": "2"}


def test_a_budget_is_followed_as_it_is_spent(sidecar, team):
    seen, thread = follow(sidecar, lambda t: t.budget("tokens"), lambda left: left == 6)
    assert wait_until(lambda: seen)
    team.budget("tokens").top_up(10)
    team.budget("tokens").spend(4)
    thread.join(timeout=5)
    assert not thread.is_alive(), f"the loop never saw 6; it saw {seen}"
    assert seen[0] == 0 and seen[-1] == 6


def test_the_board_is_followed_as_tasks_are_added_claimed_and_finished(sidecar, team):
    seen, thread = follow(
        sidecar, lambda t: t.tasks, lambda tasks: bool(tasks) and all(task.state == "done" for task in tasks)
    )
    assert wait_until(lambda: seen)
    task = team.tasks.add("Draft")
    team.tasks.claim(task)
    team.tasks.finish(task, "ok")
    thread.join(timeout=5)
    assert not thread.is_alive(), f"the loop never saw the task done; it saw {seen}"
    assert seen[0] == []
    assert seen[-1] == [team.tasks.get(task)]


# Moving a task changes the tree and not the board itself. The loop has to
# see that too.
def test_the_board_is_followed_when_a_task_is_moved(sidecar, team):
    report = team.tasks.add("Write the report")
    draft = team.tasks.add("Draft")
    seen, thread = follow(
        sidecar, lambda t: t.tasks, lambda tasks: any(task.parent == report.id for task in tasks)
    )
    assert wait_until(lambda: seen)
    team.tasks.move(draft, report)
    thread.join(timeout=5)
    assert not thread.is_alive(), f"the loop never saw the move; it saw {seen}"


# Adding a task changes two objects, the board and the tree, and each of them
# reports its change. The loop has to give the new board once, not twice.
def test_a_value_that_did_not_change_is_not_given_twice(sidecar, team):
    seen, thread = follow(sidecar, lambda t: t.tasks, lambda tasks: len(tasks) == 6)
    assert wait_until(lambda: seen)
    for number in range(6):
        team.tasks.add(f"task {number}")
    thread.join(timeout=5)
    assert not thread.is_alive(), f"the loop never saw six tasks; it saw {len(seen)} values"
    assert all(earlier != later for earlier, later in zip(seen, seen[1:]))


# Leaving the loop has to end the background work it started.
def test_leaving_a_loop_cleans_up_after_itself(team):
    before = threading.active_count()
    for _ in range(20):
        for _value in team.tasks.watch():
            break
    assert wait_until(lambda: threading.active_count() <= before + 2), (
        f"{threading.active_count() - before} threads were left behind by 20 loops"
    )
    team.tasks.add("still works")
    assert len(team.tasks.list()) == 1


# Closing the connection has to end a loop that is waiting for the next
# change, as it does for a loop on a plain object.
def test_closing_the_connection_ends_a_loop(sidecar):
    watcher = orvu.Team.connect(socket=sidecar.socket, me="watcher")
    seen = []
    thread = threading.Thread(target=lambda: seen.extend(watcher.tasks.watch()), daemon=True)
    thread.start()
    time.sleep(0.5)  # the loop has its first value and is waiting for a change
    watcher.close()
    thread.join(timeout=5)
    assert not thread.is_alive(), "the loop went on waiting after its connection was closed"


# The sidecar forgets its sessions when it restarts. A loop started after
# that must open a new session by itself and carry on, as a loop on a plain
# object does.
def test_a_loop_started_after_a_restart_goes_on_working(sidecar, team):
    team.tasks.add("first")
    sidecar.restart()
    seen = []

    def loop():
        for tasks in team.tasks.watch():
            seen.append(len(tasks))
            if len(tasks) == 2:
                return

    thread = threading.Thread(target=loop, daemon=True)
    thread.start()
    assert wait_until(lambda: seen)
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.tasks.add("second")
    thread.join(timeout=5)
    assert not thread.is_alive(), f"the loop stopped after the restart; it saw {seen}"
    assert seen == [1, 2]
