"""The task board."""

import threading
import time

import pytest

import orvu
from conftest import wait_until
from orvu import _names, _wire


def test_a_task_is_added_and_read_back(team):
    task = team.tasks.add("Summarise the Q3 report", notes="Keep it to one page")
    assert task.id.startswith("t-")
    assert task.title == "Summarise the Q3 report"
    assert task.notes == "Keep it to one page"
    assert task.state == "todo"
    assert task.created_by == "alice"
    assert (task.claimed_by, task.result, task.parent, task.finished_by) == (None, None, None, None)
    assert team.tasks.get(task) == task
    assert team.tasks.get(task.id) == task


def test_tasks_are_listed_oldest_first(team):
    titles = [f"task {number}" for number in range(12)]
    for title in titles:
        team.tasks.add(title)
    assert [task.title for task in team.tasks.list()] == titles
    assert len({task.id for task in team.tasks.list()}) == 12


def test_the_list_can_be_narrowed_to_one_state(team):
    first = team.tasks.add("first")
    second = team.tasks.add("second")
    team.tasks.add("third")
    team.tasks.claim(first)
    team.tasks.finish(second)
    assert [task.title for task in team.tasks.list("todo")] == ["third"]
    assert [task.title for task in team.tasks.list("doing")] == ["first"]
    assert [task.title for task in team.tasks.list("done")] == ["second"]
    with pytest.raises(orvu.BadRequest, match="state"):
        team.tasks.list("finished")


def test_a_task_is_updated_field_by_field(team):
    task = team.tasks.add("Draft", notes="first try")
    changed = team.tasks.update(task, title="Final draft")
    assert (changed.title, changed.notes, changed.state) == ("Final draft", "first try", "todo")
    changed = team.tasks.update(task, notes="second try", state="doing")
    assert (changed.title, changed.notes, changed.state) == ("Final draft", "second try", "doing")
    assert team.tasks.get(task) == changed


# Two agents change different fields of one task. Both changes are kept.
def test_two_agents_updating_different_fields_both_keep_their_change(sidecar, team):
    task = team.tasks.add("Draft")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        team.tasks.update(task, title="Final draft")
        bob.tasks.update(task, notes="checked by bob")
        found = team.tasks.get(task)
        assert (found.title, found.notes) == ("Final draft", "checked by bob")


@pytest.mark.parametrize("change", [{"title": ""}, {"title": "  "}, {"title": 7}, {"notes": 7}, {"state": "finished"}])
def test_an_update_that_makes_no_sense_changes_nothing(team, change):
    task = team.tasks.add("Draft")
    with pytest.raises(orvu.BadRequest):
        team.tasks.update(task, **change)
    assert team.tasks.get(task) == task


@pytest.mark.parametrize("title", ["", "   ", None, 7])
def test_a_task_needs_a_title(team, title):
    with pytest.raises(orvu.BadRequest, match="title"):
        team.tasks.add(title)
    assert team.tasks.list() == []


def test_a_task_that_is_not_on_the_board_is_reported(team):
    team.tasks.add("Draft")
    assert team.tasks.get("t-00000000") is None
    for call in (
        lambda: team.tasks.update("t-00000000", title="x"),
        lambda: team.tasks.claim("t-00000000"),
        lambda: team.tasks.release("t-00000000"),
        lambda: team.tasks.finish("t-00000000"),
        lambda: team.tasks.move("t-00000000"),
        lambda: team.tasks.children("t-00000000"),
        lambda: team.tasks.add("Sub-task", parent="t-00000000"),
    ):
        with pytest.raises(orvu.UnknownTask, match="t-00000000"):
            call()
    assert len(team.tasks.list()) == 1


# ---- claims ------------------------------------------------------------------


def test_a_claimed_task_is_this_agents_and_nobody_elses(sidecar, team):
    task = team.tasks.add("Draft")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert team.tasks.claim(task) is True
        assert bob.tasks.claim(task) is False
        found = bob.tasks.get(task)
        assert (found.state, found.claimed_by) == ("doing", "alice")
        # Claiming what is already yours is fine.
        assert team.tasks.claim(task) is True


def test_a_released_task_can_be_claimed_by_someone_else(sidecar, team):
    task = team.tasks.add("Draft")
    team.tasks.claim(task)
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert bob.tasks.release(task) is False, "bob gave back a task that was not his"
        assert team.tasks.get(task).claimed_by == "alice"

        assert team.tasks.release(task) is True
        found = team.tasks.get(task)
        assert (found.state, found.claimed_by) == ("todo", None)
        assert bob.tasks.claim(task) is True
        assert team.tasks.get(task).claimed_by == "bob"


def test_a_finished_task_cannot_be_claimed(sidecar, team):
    task = team.tasks.add("Draft")
    team.tasks.finish(task, "done by hand")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert bob.tasks.claim(task) is False
    assert team.tasks.get(task).state == "done"


def test_the_agent_can_be_named_per_call(sidecar):
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        task = nameless.tasks.add("Draft")
        assert task.created_by is None
        with pytest.raises(orvu.BadRequest, match="who is asking"):
            nameless.tasks.claim(task)
        assert nameless.tasks.get(task).state == "todo"

        assert nameless.tasks.claim(task, me="carol") is True
        assert nameless.tasks.claim(task, me="dave") is False
        assert nameless.tasks.finish(task, "ok", me="carol") is True
        finished = nameless.tasks.get(task)
        assert (finished.claimed_by, finished.finished_by) == ("carol", "carol")


# ---- finishing ---------------------------------------------------------------


@pytest.mark.parametrize("result", ["It went up.", 42, None, ["a", "b"], {"score": 0.5, "ok": True}])
def test_a_finished_task_keeps_its_result(team, result):
    task = team.tasks.add("Draft")
    team.tasks.claim(task)
    assert team.tasks.finish(task, result) is True
    finished = team.tasks.get(task)
    assert finished.state == "done"
    assert finished.result == result
    assert finished.finished_by == "alice"


def test_a_result_that_cannot_be_kept_is_refused_and_the_task_is_not_done(team):
    task = team.tasks.add("Draft")
    with pytest.raises(orvu.BadRequest):
        team.tasks.finish(task, object())
    assert team.tasks.get(task).state == "todo"


# Finishing a task writes several fields, and other agents read while that is
# going on. A task they see as "done" must already say who did it and what
# came out. Here one agent reads the board as fast as it can while another
# finishes over a hundred tasks.
def test_a_task_seen_as_done_is_whole(sidecar, team):
    tasks = [team.tasks.add(f"task {number}") for number in range(120)]
    half_done = []
    finishing = threading.Event()

    def read():
        with orvu.Team.connect(socket=sidecar.socket, me="reader") as reader:
            while not finishing.is_set():
                half_done.extend(
                    task
                    for task in reader.tasks.list("done")
                    if task.finished_by != "alice" or task.result != {"ok": True}
                )

    thread = threading.Thread(target=read, daemon=True)
    thread.start()
    for task in tasks:
        team.tasks.finish(task, {"ok": True})
    finishing.set()
    thread.join(timeout=10)
    assert half_done == []


# ---- sub-tasks ---------------------------------------------------------------


def test_a_task_can_have_sub_tasks(team):
    report = team.tasks.add("Write the report")
    research = team.tasks.add("Research", parent=report)
    draft = team.tasks.add("Draft", parent=report)
    sources = team.tasks.add("Find sources", parent=research)

    assert (research.parent, draft.parent, sources.parent) == (report.id, report.id, research.id)
    assert team.tasks.get(report).parent is None
    assert [task.title for task in team.tasks.children(report)] == ["Research", "Draft"]
    assert [task.title for task in team.tasks.children(research)] == ["Find sources"]
    assert team.tasks.children(sources) == []
    assert len(team.tasks.list()) == 4


def test_a_task_can_be_moved_under_another_and_back_to_the_top(team):
    report = team.tasks.add("Write the report")
    draft = team.tasks.add("Draft")
    assert team.tasks.move(draft, report) is True
    assert team.tasks.get(draft).parent == report.id
    assert [task.id for task in team.tasks.children(report)] == [draft.id]

    assert team.tasks.move(draft) is True
    assert team.tasks.get(draft).parent is None
    assert team.tasks.children(report) == []


# A task under its own sub-task would be a loop with no way in from the top.
def test_a_task_cannot_be_moved_under_itself_or_its_own_sub_task(team):
    report = team.tasks.add("Write the report")
    research = team.tasks.add("Research", parent=report)
    sources = team.tasks.add("Find sources", parent=research)

    assert team.tasks.move(report, sources) is False
    assert team.tasks.move(report, report) is False

    assert team.tasks.get(report).parent is None
    assert team.tasks.get(research).parent == report.id
    assert team.tasks.get(sources).parent == research.id


# ---- when something goes wrong partway ---------------------------------------


# A result the sidecar will not keep must not leave the task marked done with
# nothing in it.
def test_a_result_the_sidecar_refuses_leaves_the_task_as_it_was(team):
    task = team.tasks.add("Draft")
    team.tasks.claim(task)
    with pytest.raises(orvu.OrvuError):
        team.tasks.finish(task, {"score": float("nan")})
    found = team.tasks.get(task)
    assert (found.state, found.result, found.finished_by) == ("doing", None, None)


# Something else already uses the board's name for another kind of object, so
# no task can be written. The caller has to be told that, and not that the
# task it just added is missing.
def test_a_write_that_fails_is_reported_as_what_it_is(team):
    team.client.counter("orvu/default/tasks").increment(1)
    with pytest.raises(orvu.TypeMismatch):
        team.tasks.add("Draft")


# ---- changes that cross each other -------------------------------------------


def test_finishing_a_task_twice_keeps_the_first_result(sidecar, team):
    task = team.tasks.add("Draft")
    assert team.tasks.finish(task, "first") is True
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert bob.tasks.finish(task, "second") is False
    found = team.tasks.get(task)
    assert (found.state, found.result, found.finished_by) == ("done", "first", "alice")


# Bob looks at a task, and it is free. Before his claim is written, alice
# finishes it. His claim must not bring the task back to life.
def test_a_claim_that_lands_after_the_task_was_finished_does_not_undo_it(sidecar, team, monkeypatch):
    task = team.tasks.add("Draft")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        looked_at = bob.tasks.get(task)
        team.tasks.finish(task, "by alice")

        fresh = bob.tasks._existing
        first_look = [looked_at]
        monkeypatch.setattr(
            bob.tasks, "_existing", lambda asked, **how: first_look.pop() if first_look else fresh(asked, **how)
        )
        assert bob.tasks.claim(task) is False

    found = team.tasks.get(task)
    assert (found.state, found.result, found.finished_by) == ("done", "by alice", "alice")


# Two programs run under one name. The second looks at a task while it is
# free and its claim is slow to land. The first then claims the task too, and
# alice finishes it. When the second program reads back, the claim on the task
# is the first program's, and is left as it is.
def test_undoing_a_late_claim_leaves_another_programs_claim(sidecar, team, monkeypatch):
    task = team.tasks.add("Draft")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as first, orvu.Team.connect(
        socket=sidecar.socket, me="bob"
    ) as second:
        looked_at = second.tasks.get(task)
        fresh = second.tasks._existing
        calls, claimed = [], []

        def existing(asked, **how):
            calls.append(asked)
            if len(calls) == 1:
                return looked_at
            # The second program's claim is written. Before it reads back, the
            # first program claims the task and alice finishes it.
            time.sleep(0.01)
            assert first.tasks.claim(task, lease=600) is True
            claimed.append(first.tasks.get(task))
            team.tasks.finish(task, "by alice")
            return fresh(asked, **how)

        monkeypatch.setattr(second.tasks, "_existing", existing)
        assert second.tasks.claim(task) is False
    found = team.tasks.get(task)
    assert (found.state, found.result, found.finished_by) == ("done", "by alice", "alice")
    assert (found.claimed_by, found.seen, found.lease) == ("bob", claimed[0].seen, 600)


# The same, but the first program finishes the task itself. The task rightly
# names the agent, and keeps its name.
def test_a_late_claim_leaves_the_name_of_an_agent_that_finished_the_task(sidecar, team, monkeypatch):
    task = team.tasks.add("Draft")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as first, orvu.Team.connect(
        socket=sidecar.socket, me="bob"
    ) as second:
        looked_at = second.tasks.get(task)
        fresh = second.tasks._existing
        calls = []

        def existing(asked, **how):
            calls.append(asked)
            if len(calls) == 1:
                return looked_at
            assert first.tasks.finish(task, "by bob") is True
            return fresh(asked, **how)

        monkeypatch.setattr(second.tasks, "_existing", existing)
        assert second.tasks.claim(task) is False
    found = team.tasks.get(task)
    assert (found.state, found.result, found.finished_by, found.claimed_by) == ("done", "by bob", "bob", "bob")


# A task put back to "todo" by hand has to be free for anyone to take.
def test_a_task_set_back_to_todo_can_be_claimed_by_anyone(sidecar, team):
    task = team.tasks.add("Draft")
    team.tasks.claim(task)
    changed = team.tasks.update(task, state="todo")
    assert (changed.state, changed.claimed_by) == ("todo", None)
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert bob.tasks.claim(task) is True


def test_a_finished_task_can_be_opened_again(team):
    task = team.tasks.add("Draft")
    team.tasks.claim(task)
    team.tasks.finish(task, "too early")
    changed = team.tasks.update(task, state="todo")
    assert (changed.state, changed.claimed_by, changed.result, changed.finished_by) == ("todo", None, None, None)
    assert team.tasks.claim(task) is True
    assert team.tasks.finish(task, "now it is right") is True
    assert team.tasks.get(task).result == "now it is right"


def test_a_task_can_be_set_to_done_and_to_doing_by_hand(team):
    task = team.tasks.add("Draft")
    assert team.tasks.update(task, state="done").state == "done"
    assert team.tasks.finish(task, "late") is False
    assert team.tasks.update(task, state="doing").state == "doing"
    assert team.tasks.finish(task, "now") is True


# An id is made up at random. It has to be long enough that a new task does
# not land on one that is already there, even on a very large board.
def test_a_task_id_is_long_enough_not_to_repeat(team):
    task = team.tasks.add("Draft")
    assert len(task.id) == len("t-") + 16


def test_a_task_says_who_changed_it_last_by_hand(team, bob):
    task = team.tasks.add("Summarise the Q3 report")
    assert task.updated_by is None
    assert bob.tasks.update(task, notes="Use the new numbers").updated_by == "bob"
    assert team.tasks.update(task, title="Summarise Q3").updated_by == "alice"
    # Nothing was asked for, so nothing is changed.
    assert bob.tasks.update(task).updated_by == "alice"
    # Taking and finishing a task have names of their own.
    bob.tasks.claim(task)
    bob.tasks.finish(task, "It went up.")
    done = team.tasks.get(task)
    assert (done.claimed_by, done.finished_by, done.updated_by) == ("bob", "bob", "alice")


def test_a_task_changed_by_a_nameless_program_does_not_keep_the_old_name(team, sidecar):
    task = team.tasks.update(team.tasks.add("Draft"), notes="first")
    assert task.updated_by == "alice"
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        assert nameless.tasks.update(task, notes="second").updated_by is None


# Work on one task reads that task, and not the whole board. Reading all of
# the board for each one is what made a large board slow.
def test_work_on_one_task_does_not_read_the_whole_board(team, monkeypatch):
    report = team.tasks.add("Report")
    draft = team.tasks.add("Draft", parent=report)
    sent = []
    build = _wire.Op.request

    def recording(op, *args):
        sent.append(op.field)
        return build(op, *args)

    monkeypatch.setattr(_wire.Op, "request", recording)
    assert team.tasks.get(draft).parent == report.id
    assert team.tasks.add("Sources", parent=report).parent == report.id
    assert team.tasks.update(draft, notes="Use the new numbers").notes == "Use the new numbers"
    assert team.tasks.claim(draft) is True
    assert team.tasks.release(draft) is True
    assert team.tasks.finish(draft, "It went up.") is True
    assert team.tasks.move(draft) is True
    assert [task.title for task in team.tasks.children(report)] == ["Sources"]
    assert sent, "nothing was recorded"
    assert "json_to_value" not in sent and "tree_to_nodes" not in sent


# Another machine's task can arrive in pieces. One that has no title yet is
# not shown, and a parent that is not on the board yet is not named, whether
# the task is looked up by itself or listed with the rest.
def test_a_task_that_has_only_partly_arrived_is_shown_once_it_is_whole(team):
    board = team.client.json(_names.tasks(team.name))
    tree = team.client.tree(_names.task_tree(team.name))
    board.set(["t-0000000000000001"], {"state": "todo"})
    tree.move_node("t-0000000000000001", "root")
    assert team.tasks.get("t-0000000000000001") is None
    assert team.tasks.list() == []

    board.set(["t-0000000000000002"], {"title": "Draft", "state": "todo", "created": 1})
    tree.move_node("t-0000000000000003", "root")
    tree.move_node("t-0000000000000002", "t-0000000000000003")
    assert team.tasks.get("t-0000000000000002").parent is None
    assert team.tasks.list()[0].parent is None
    # Nor while its parent has arrived only in part.
    board.set(["t-0000000000000003"], {"state": "todo"})
    assert team.tasks.get("t-0000000000000002").parent is None
    assert team.tasks.list()[0].parent is None
    # Once its parent is on the board, it is named.
    board.set(["t-0000000000000003"], {"title": "Report", "state": "todo", "created": 0})
    assert team.tasks.get("t-0000000000000002").parent == "t-0000000000000003"
    assert team.tasks.list()[1].parent == "t-0000000000000003"
    assert [task.id for task in team.tasks.children("t-0000000000000003")] == ["t-0000000000000002"]


# One message holds at most 1,024 operations, so many sub-tasks are read in
# parts.
def test_a_task_with_very_many_sub_tasks_lists_them_all(team):
    report = team.tasks.add("Report")
    titles = [f"part {number}" for number in range(600)]
    for title in titles:
        team.tasks.add(title, parent=report)
    assert [task.title for task in team.tasks.children(report)] == titles


# A batch's replies together may not be larger than 4 MiB. A board larger
# than that can still be listed and watched.
def test_a_board_larger_than_one_batch_can_hold_is_listed(team):
    notes = "n" * 10_000
    for number in range(450):
        team.tasks.add(f"task {number}", notes=notes)
    assert len(team.tasks.list()) == 450
    assert len(next(iter(team.tasks.watch()))) == 450


def counting(team):
    """Counts the messages sent to the sidecar from now on."""
    sent = [0]
    real = team.client._call

    def call(send, **options):
        sent[0] += 1
        return real(send, **options)

    team.client._call = call
    return sent


# Each task is read with its parent first, and a batch stops running its
# operations once its replies pass 4 MiB. A large task, and many tasks that
# are large together, still have to be read.
def test_tasks_larger_than_one_batch_can_hold_are_read(team):
    task = team.tasks.add("Report")
    team.tasks.update(task, notes="n" * 3_000_000)
    assert team.tasks.finish(task, "r" * 2_000_000) is True
    found = team.tasks.get(task)
    assert (len(found.notes), len(found.result), found.state) == (3_000_000, 2_000_000, "done")

    notes = "n" * 10_000
    for number in range(450):
        team.tasks.add(f"part {number}", parent=task, notes=notes)
    assert len(team.tasks.children(task)) == 450


# An id that is not text cannot be on the board.
@pytest.mark.parametrize("task", [123, None, ["t-1"]])
def test_an_id_that_is_not_text_is_no_task(team, task):
    assert team.tasks.get(task) is None
    with pytest.raises(orvu.UnknownTask):
        team.tasks.claim(task)


# A call reads what it needs once, and no more. Taking a sub-task costs no
# more than taking a task at the top, because taking one does not look at
# its parent.
def test_a_call_on_one_task_reads_what_it_needs_once(team):
    report = team.tasks.add("Report")
    draft = team.tasks.add("Draft", parent=report)
    sent = counting(team)
    team.tasks.claim(report)
    at_the_top = sent[0]
    sent[0] = 0
    team.tasks.claim(draft)
    assert sent[0] == at_the_top == 3  # look, write, look again
    sent[0] = 0
    assert [task.title for task in team.tasks.children(report.id)] == ["Draft"]
    assert sent[0] == 3  # the task, the ids under it, and those tasks
    sent[0] = 0
    # A `Task` that was read already is not read again.
    assert [task.title for task in team.tasks.children(report)] == ["Draft"]
    assert sent[0] == 2


# Two agents finish the same task at the same moment: both looked, saw it
# not done, and wrote their result before either read it back. Only one is
# told yes, and the result kept is that one's, beside its name.
def test_of_two_finishes_at_once_only_one_is_told_yes(sidecar, team, monkeypatch):
    task = team.tasks.add("Draft")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        looked_at = bob.tasks.get(task)
        told = {}
        board = team.tasks._board
        write = board.set

        def alice_writes(path, value):
            # Bob looked before alice wrote, and writes just after her.
            write(path, value)
            monkeypatch.setattr(board, "set", write)
            fresh = bob.tasks._existing
            first_look = [looked_at]
            monkeypatch.setattr(bob.tasks, "_existing", lambda a, **h: first_look.pop() if first_look else fresh(a, **h))
            told["bob"] = bob.tasks.finish(task, "by bob")

        monkeypatch.setattr(board, "set", alice_writes)
        told["alice"] = team.tasks.finish(task, "by alice")
    assert sorted(told.values()) == [False, True], told
    winner = "alice" if told["alice"] else "bob"
    found = team.tasks.get(task)
    assert (found.state, found.result, found.finished_by) == ("done", f"by {winner}", winner)


# Tasks finished before the result and the finisher were kept together are
# still read as they were.
def test_a_task_finished_the_old_way_is_still_read(team):
    task = team.tasks.add("Draft")
    board = team.client.json(_names.tasks(team.name))
    board.set([task.id, "result"], {"score": 1})
    board.set([task.id, "finished_by"], "carol")
    board.set([task.id, "finished"], True)
    found = team.tasks.get(task)
    assert (found.state, found.result, found.finished_by) == ("done", {"score": 1}, "carol")
    assert team.tasks.finish(task, "again") is False
    opened = team.tasks.update(task, state="todo")
    assert (opened.state, opened.result, opened.finished_by) == ("todo", None, None)


# A claim that lands just after the task was finished is told no, and must
# not leave its name, or its time, on the finished task.
def test_a_claim_that_lands_after_the_task_was_finished_leaves_no_name(sidecar, team, monkeypatch):
    task = team.tasks.add("Draft")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        looked_at = bob.tasks.get(task)
        team.tasks.finish(task, "by alice")
        fresh = bob.tasks._existing
        first_look = [looked_at]
        monkeypatch.setattr(
            bob.tasks, "_existing", lambda asked, **how: first_look.pop() if first_look else fresh(asked, **how)
        )
        assert bob.tasks.claim(task) is False
    found = team.tasks.get(task)
    assert (found.state, found.claimed_by, found.seen, found.lease) == ("done", None, None, None)


# A task is added with two writes: its place in the tree, then the task.
# Another machine may hear of them in either order. One that is on the board
# and not yet in the tree is not shown for a while, rather than shown at the
# top for an instant and then under its parent.
def test_a_task_not_yet_in_the_tree_is_not_listed(team):
    report = team.tasks.add("Report")
    board = team.client.json(_names.tasks(team.name))
    board.set(["t-0000000000000009"], {"title": "Draft", "state": "todo", "created": int(time.time() * 1000)})
    assert [task.title for task in team.tasks.list()] == ["Report"]
    assert [[task.title for task in tasks] for tasks in [next(iter(team.tasks.watch()))]] == [["Report"]]
    team.client.tree(_names.task_tree(team.name)).move_node("t-0000000000000009", report.id)
    assert [(task.title, task.parent) for task in team.tasks.list()] == [("Report", None), ("Draft", report.id)]


# The tree is read before the board. A task added here is put in the tree
# first, so it is found in the tree by the time it is on the board.
def test_the_list_reads_the_tree_before_the_board(team, monkeypatch):
    team.tasks.add("Report")
    sent = []
    build = _wire.Op.request

    def recording(op, *args):
        sent.append(op.field)
        return build(op, *args)

    monkeypatch.setattr(_wire.Op, "request", recording)
    team.tasks.list()
    assert sent.index("tree_to_nodes") < sent.index("json_to_value"), sent


# Reads that a batch did not run because its replies had passed 4 MiB are
# sent again together, in as few messages as they fit in, and not one by
# one.
def test_reads_past_the_batch_limit_are_sent_again_together(team):
    report = team.tasks.add("Report")
    notes = "n" * 10_000
    for number in range(900):
        team.tasks.add(f"part {number}", parent=report, notes=notes)
    sent = counting(team)
    assert len(team.tasks.children(report)) == 900
    # The ids under it, then 900 tasks of about 9 MiB: two messages for the
    # first 500, and two or three for the rest.
    assert sent[0] <= 6, sent[0]


# A task is written to the tree first and to the board after. If a program
# stops between the two, or the tree is slow to arrive from another machine,
# the task is not shown for a short while, in case its place in the tree is
# on its way. After that it is shown at the top.
def test_a_task_with_no_place_in_the_tree_is_shown_at_the_top_after_a_while(team):
    long_ago = int(time.time() * 1000) - 60_000
    team.client.json("orvu/default/tasks").set(["t-0000000000000001"], {"title": "Lost", "state": "todo", "created": long_ago})
    team.client.json("orvu/default/tasks").set(
        ["t-0000000000000002"], {"title": "On its way", "state": "todo", "created": int(time.time() * 1000)}
    )
    assert [(task.title, task.parent) for task in team.tasks.list()] == [("Lost", None)]
    assert team.tasks.get("t-0000000000000001").title == "Lost"


def test_a_watch_shows_a_task_with_no_place_in_the_tree_once_the_wait_is_over(team):
    team.tasks.GRACE = 1
    team.client.json("orvu/default/tasks").set(
        ["t-0000000000000002"], {"title": "On its way", "state": "todo", "created": int(time.time() * 1000)}
    )
    seen = []

    def listen():
        for tasks in team.tasks.watch():
            seen.append([task.title for task in tasks])
            if tasks:
                return

    threading.Thread(target=listen, daemon=True).start()
    assert wait_until(lambda: seen[-1:] == [["On its way"]], 5), seen
    assert seen[0] == []


# Adding writes the task's place in the tree before the task itself. A task
# whose write to the board fails leaves only a place in the tree, which shows
# nothing.
def test_adding_puts_the_task_in_the_tree_first(team, monkeypatch):
    class Refusing:
        def __init__(self, board):
            self.name = board.name
            self._board = board

        def set(self, path, value):
            raise orvu.EngineError("the board could not be written")

        def __getattr__(self, name):
            return getattr(self._board, name)

    monkeypatch.setattr(team.tasks, "_board", Refusing(team.tasks._board))
    with pytest.raises(orvu.EngineError):
        team.tasks.add("Draft")
    monkeypatch.undo()
    assert [node["parent"] for node in team.client.tree("orvu/default/tasks-tree").to_nodes()].count("root") == 1
    assert team.tasks.list() == []


# ---- what anyone may have written -------------------------------------------
#
# The board, the notes and the inbox hold whatever any program wrote, and a
# program with the right rules can write anything into them. Reading never
# fails because of it: a field that is not what it should be reads as a safe
# value instead.

ODD_VALUES = [
    {"toString": 1},
    {"valueOf": 1},
    {"toString": "x", "valueOf": "y", "__proto__": {"a": 1}},
    [1, "a"],
    [],
    7,
    2.5,
    -1e300,
    None,
    True,
    False,
    "",
    "doing",
]
TASK_FIELDS = [
    "title", "state", "claimed_by", "notes", "created", "created_by", "finished_by", "updated_by", "firm",
    "ticket", "tickets", "seen", "lease", "taken_over", "finished", "result",
]


def test_odd_values_written_anywhere_never_stop_reading(team, bob):
    board = team.client.json(_names.tasks(team.name))
    plain = team.tasks.add("Check the figures")
    odd = team.tasks.add("Send the invoice", firm=True)
    for value in ODD_VALUES:
        for field in TASK_FIELDS:
            board.set([odd.id, field], value)
        tasks = {found.id: found for found in team.tasks.list()}
        assert set(tasks) == {plain.id, odd.id}
        found = team.tasks.get(odd)
        assert found == tasks[odd.id]
        assert isinstance(found.title, str) and found.state in ("todo", "doing", "done")
        for text in (found.claimed_by, found.notes, found.created_by, found.finished_by, found.updated_by):
            assert text is None or isinstance(text, str)
        assert isinstance(found.created, (int, float)) and isinstance(found.ticket, int)
        watching = team.tasks.watch()
        assert {task.id for task in next(watching)} == {plain.id, odd.id}
        watching.close()
        found.abandoned()
    # A note, and messages, that are not what this package writes.
    notes = team.client.mv_map(_names.notes(team.name))
    team.notes.put("plain", 1)
    for kept in ["\x1enot json", "\x1e" + "1" * 5000, "\x1e" + "[" * 100_000, "bob\x1f\x1e{", "\x1f"]:
        notes.put("odd", kept)
        team.notes.get("odd")
        assert team.notes.all()["plain"] == 1
        team.notes.author("odd")
    page = team.client.list(_names.inbox_page(team.name, _names.EVERYONE, 0))
    for kept in [
        '{"id":"m-1","sent":1,"body":"hi","from":{"toString":1},"to":[1]}',
        '{"id":5,"sent":1,"body":1}',
        '{"id":"m-2","sent":2.5,"body":1}',
        '{"id":"m-3","sent":' + "1" * 5000 + ',"body":1}',
        '{"id":"m-4","sent":1e400,"body":1}',
        "[1,2]",
        "null",
        "\x1enot json",
        "[" * 100_000,
    ]:
        page.insert(0, kept)
    bob.inbox.send_all("hello")
    for reader in (team, bob):
        messages = reader.inbox.messages()
        assert [message.body for message in messages if message.id != "m-1"] == ["hello"]
        odd_one = next(message for message in messages if message.id == "m-1")
        assert (odd_one.sender, odd_one.to) == (None, None)
        reader.inbox.unread()


# The whole board may have been written over by a program without rules.
# What is not an object holds no tasks.
def test_a_board_that_is_not_an_object_holds_no_tasks(team):
    task = team.tasks.add("Check the figures")
    board = team.client.json(_names.tasks(team.name))
    for value in ([1, 2], [], "board", 7, True, None):
        board.set([], value)
        assert team.tasks.list() == []
        assert team.tasks.get(task) is None
        watching = team.tasks.watch()
        assert next(watching) == []
        watching.close()
