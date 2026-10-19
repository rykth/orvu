"""Rules for what each agent may do. An agent with no rules may do
everything. One with rules may use only the blocks they name, at the level
they give, and its own things are its own: only it reads its inbox, and it
changes only its own place on the roster and its own line of what it spent."""

import json
import os
import subprocess
import sys

import pytest

import orvu


def agent(sidecar, name, *rules):
    """A team used by the agent `name`, with `rules` if any are given."""
    key = sidecar.add_agent(name)
    if rules:
        sidecar.allow(name, *rules)
    return orvu.Team.connect(socket=sidecar.socket, key=key)


def test_an_agent_without_rules_may_do_everything(sidecar):
    with agent(sidecar, "alice") as alice:
        alice.notes.put("deploy", "Friday")
        alice.budget("tokens").top_up(10)
        alice.tasks.add("Draft")


def test_an_agent_reads_what_it_may_read_and_changes_nothing_else(sidecar):
    with agent(sidecar, "boss") as boss:
        boss.notes.put("deploy", "Friday")
    with agent(sidecar, "reader", "notes:read") as reader:
        assert reader.notes.get("deploy") == "Friday"
        with pytest.raises(orvu.NotAllowed, match="reader may not change notes. Its rules are: notes:read"):
            reader.notes.put("deploy", "Monday")
        with pytest.raises(orvu.NotAllowed, match="may not read tasks"):
            reader.tasks.list()
        assert reader.notes.get("deploy") == "Friday"


def test_tasks_with_tasks_write_work_from_start_to_end(sidecar):
    with agent(sidecar, "worker", "tasks:write") as worker:
        task = worker.tasks.add("Send the invoice", firm=True)
        assert worker.tasks.claim(task) is True
        assert worker.tasks.still_working(task) is True
        assert worker.tasks.release(task) is True
        assert worker.tasks.claim(task) is True
        assert worker.tasks.finish(task, "sent") is True


# Setting a stuck firm task back with force writes its tickets, so it needs
# tasks:write like any other change to a task.
def test_force_needs_tasks_write(sidecar):
    with agent(sidecar, "boss") as boss:
        task = boss.tasks.add("Send the invoice", firm=True)
        boss.tasks.claim(task)
    with agent(sidecar, "reader", "tasks:read") as reader:
        with pytest.raises(orvu.NotAllowed, match="tasks:read"):
            reader.tasks.update(task, state="todo", force=True)
    with agent(sidecar, "worker", "tasks:write") as worker:
        assert worker.tasks.update(task, state="todo", force=True).ticket == 1


# A firm task's number never goes down: a task written whole by an agent
# with rules keeps the ticket numbers it had.
def test_a_task_written_whole_keeps_its_number(sidecar):
    with agent(sidecar, "worker", "tasks:write") as worker:
        task = worker.tasks.add("Send the invoice", firm=True)
        assert worker.tasks.claim(task) is True
        assert worker.tasks.release(task) is True
        board = worker.client.json(f"orvu/{worker.name}/tasks")
        fields = board.get([task.id])
        board.set([task.id], {key: value for key, value in fields.items() if key not in ("ticket", "tickets")})
        assert worker.tasks.get(task).ticket == 1


def test_budgets_are_read_spent_from_and_topped_up(sidecar):
    with agent(sidecar, "boss", "budgets:top-up") as boss, agent(sidecar, "spender", "budgets:spend") as spender:
        boss.budget("tokens").top_up(100)
        assert spender.budget("tokens").spend(30) is True
        assert spender.budget("tokens").spent_by() == {"spender": 30}
        with pytest.raises(orvu.NotAllowed, match="may not top up budgets"):
            spender.budget("tokens").top_up(5)
        assert boss.budget("tokens").left() == 70
        # What was spent is written under one's own name only.
        with pytest.raises(orvu.NotAllowed, match="another agent's name"):
            spender.client.cart(f"orvu/{spender.name}/budget-spent/tokens").add("boss", 1)


def test_an_inbox_is_read_only_by_its_owner(sidecar):
    with agent(sidecar, "alice", "inbox:write") as alice, agent(sidecar, "bob", "inbox:read") as bob:
        alice.inbox.send("bob", "hello")
        alice.inbox.send_all("to everyone")
        assert sorted(message.body for message in bob.inbox.messages()) == ["hello", "to everyone"]
        assert [message.body for message in bob.inbox.unread()] in (["hello", "to everyone"], ["to everyone", "hello"])
        # Alice can send to bob, but not read what is in his inbox.
        with pytest.raises(orvu.NotAllowed, match="read another agent's inbox"):
            alice.client.list(f"orvu/{alice.name}/inbox/bob/0").to_list()
        with pytest.raises(orvu.NotAllowed, match="may not send messages"):
            bob.inbox.send("alice", "hi back")
        # How many messages were written to each page is for the owner to
        # read, and what the owner was given is its own.
        with pytest.raises(orvu.NotAllowed, match="read another agent's inbox"):
            alice.client.cart(f"orvu/{alice.name}/inbox-count/bob").items()
        with pytest.raises(orvu.NotAllowed, match="read another agent's inbox"):
            alice.client.map(f"orvu/{alice.name}/inbox-done/bob/bob").entries()
        alice.inbox.send("bob", "again")
        assert [message.body for message in bob.inbox.unread()] == ["again"]
        # A sender adds one to a page's count, and nothing else.
        with pytest.raises(orvu.NotAllowed, match="add anything but one to the count of an inbox page"):
            alice.client.cart(f"orvu/{alice.name}/inbox-count/bob").add("0", 2**63)


def test_an_agent_changes_only_its_own_place_on_the_roster(sidecar):
    with agent(sidecar, "alice", "roster:write") as alice:
        alice.roster.join()
        assert alice.roster.here() == ["alice"]
        with pytest.raises(orvu.NotAllowed, match="another agent's place on the roster"):
            alice.client.map(f"orvu/{alice.name}/roster").put("bob\x1f00000000", "1 15")


def test_plain_objects_have_a_rule_of_their_own(sidecar):
    with agent(sidecar, "alice", "objects:read") as alice:
        assert alice.client.counter("visits").value() == 0
        with pytest.raises(orvu.NotAllowed, match="may not change plain objects"):
            alice.client.counter("visits").increment(1)


def test_new_rules_apply_from_the_next_call(sidecar):
    with agent(sidecar, "alice") as alice:
        alice.notes.put("deploy", "Friday")
        sidecar.allow("alice", "tasks:write")
        with pytest.raises(orvu.NotAllowed):
            alice.notes.get("deploy")
        sidecar.allow("alice", "all")
        assert alice.notes.get("deploy") == "Friday"


# An agent's tools say what it may not do, and a message it may send is sent
# even when it may not look at the roster.
def test_the_tools_say_what_an_agent_may_not_do(sidecar):
    sidecar.add_agent("bob")
    env = {**os.environ, "ORVU_KEY": sidecar.add_agent("alice")}
    sidecar.allow("alice", "notes:read", "inbox:write")
    calls = [
        {"name": "note_put", "arguments": {"name": "deploy", "text": "Friday"}},
        {"name": "inbox_send", "arguments": {"to": "bob", "text": "hello"}},
    ]
    lines = "".join(
        json.dumps({"jsonrpc": "2.0", "id": number, "method": "tools/call", "params": call}) + "\n"
        for number, call in enumerate(calls, 1)
    )
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp", "--socket", str(sidecar.socket)],
        input=lines, capture_output=True, text=True, env=env, timeout=60,
    )
    put, sent = (json.loads(line)["result"] for line in done.stdout.splitlines())
    assert put["isError"] is True and "alice may not change notes" in put["content"][0]["text"]
    assert sent["isError"] is False, sent
    assert json.loads(sent["content"][0]["text"])["sent_to"] == "bob"


# A page of an inbox holds 100 messages. A sender with only inbox:write
# starts the next page when one is full.
def test_a_sender_with_rules_fills_a_page_and_starts_the_next(sidecar):
    with agent(sidecar, "alice", "inbox:write") as alice, agent(sidecar, "bob", "inbox:read") as bob:
        for number in range(101):
            alice.inbox.send("bob", f"message {number}")
        assert len(bob.inbox.messages()) == 101


# The names a program writes for an agent with rules are its own: the sender
# of a message, and the agent that added a task. The blocks write them so.
def test_a_message_and_a_task_carry_the_agents_own_name(sidecar):
    with agent(sidecar, "alice", "inbox:write", "tasks:write") as alice:
        alice.inbox.send("bob", "hello")
        task = alice.tasks.add("Draft")
        assert task.created_by == "alice"
        page = alice.client.list(f"orvu/{alice.name}/inbox/bob/0")
        forged = json.dumps({"id": "m-1", "from": "bob", "to": "bob", "body": "hi", "sent": 1})
        with pytest.raises(orvu.NotAllowed, match="send a message under another name than its own"):
            page.insert(0, forged)
        board = alice.client.json(f"orvu/{alice.name}/tasks")
        with pytest.raises(orvu.NotAllowed, match="add a task under another name than its own"):
            board.set(["t-1"], {"title": "Draft", "state": "todo", "created": 1, "created_by": "bob"})
        with pytest.raises(orvu.NotAllowed, match="add a task under another name than its own"):
            board.set([task.id, "created_by"], "bob")
        assert alice.tasks.get(task).created_by == "alice"


# With notes:own and tasks:own an agent reads everything there, and changes
# only what is its own as it is now: notes all of whose values it wrote, and
# tasks it added or holds. It claims an open task, and leaves alone a task
# another agent holds.
def test_an_agent_with_own_rules_changes_only_its_own(sidecar):
    with agent(sidecar, "boss") as boss, agent(sidecar, "alice", "notes:own", "tasks:own") as alice:
        boss.notes.put("deploy", "Friday")
        boss.notes.structured("plan").set(["owner"], "boss")
        open_task = boss.tasks.add("Open task")
        held = boss.tasks.add("Held task")
        assert boss.tasks.claim(held) is True
        assert alice.notes.get("deploy") == "Friday"
        alice.notes.put("mine", "Monday")
        alice.notes.put("mine", "Tuesday")
        assert alice.notes.author("mine") == "alice"
        with pytest.raises(orvu.NotAllowed, match="change another agent's note"):
            alice.notes.put("deploy", "Monday")
        with pytest.raises(orvu.NotAllowed, match="change another agent's note"):
            alice.notes.remove("deploy")
        alice.notes.remove("mine")
        assert alice.notes.get("mine") is None
        with pytest.raises(orvu.NotAllowed, match="structured notes, which keep no writer's name"):
            alice.notes.structured("plan").set(["owner"], "alice")
        task = alice.tasks.add("My task")
        assert task.created_by == "alice"
        alice.tasks.update(task, title="Still mine")
        assert alice.tasks.finish(task, "done") is True
        # An open task of another's is claimed, worked on and given back.
        assert alice.tasks.claim(open_task) is True
        assert alice.tasks.still_working(open_task) is True
        assert alice.tasks.release(open_task) is True
        with pytest.raises(orvu.NotAllowed, match="alice may not change a task it did not add"):
            alice.tasks.update(open_task, title="Mine now")
        with pytest.raises(orvu.NotAllowed, match="alice may not change a task it did not add"):
            alice.tasks.finish(open_task, "done")
        # A task another agent holds is left alone.
        with pytest.raises(orvu.NotAllowed, match="change a task another agent holds"):
            alice.tasks.finish(held, "done")
        with pytest.raises(orvu.NotAllowed, match="change a task another agent holds"):
            alice.tasks.update(held, title="Mine now")
        # A batch that touches its own task and another's is turned away
        # whole.
        with pytest.raises(orvu.NotAllowed, match="nothing was applied"):
            with alice.client.batch() as batch:
                board = batch.json(f"orvu/{alice.name}/tasks")
                board.set([task.id, "notes"], "mine")
                board.set([held.id, "notes"], "mine")
        assert alice.tasks.get(task).notes is None
        assert boss.tasks.get(held).title == "Held task"
        assert boss.tasks.get(held).claimed_by == "boss"
        assert boss.tasks.get(open_task).title == "Open task"


# A line of who topped a budget up goes only with that top-up, of the same
# budget and amount, in the same request, and a top-up carries its line.
def test_who_topped_up_is_written_only_with_the_top_up(sidecar):
    with agent(sidecar, "boss", "budgets:top-up") as boss:
        boss.budget("tokens").top_up(10)
        assert boss.budget("tokens").topped_up_by() == {"boss": 10}
        topped = boss.client.cart(f"orvu/{boss.name}/budget-topped/tokens")
        budget = boss.client.bounded_counter(f"orvu/{boss.name}/budget/tokens")
        with pytest.raises(orvu.NotAllowed, match="top-up"):
            topped.add("boss", 5)
        with pytest.raises(orvu.NotAllowed, match="top-up"):
            budget.increment(5)
        with pytest.raises(orvu.NotAllowed, match="top-up"):
            with boss.client.batch() as batch:
                batch.bounded_counter(budget.name).increment(5)
                batch.cart(topped.name).add("boss", 6)
        assert boss.budget("tokens").topped_up_by() == {"boss": 10}
        assert boss.budget("tokens").left() == 10


# An agent with tasks:own writes what a claim writes only once its claim has
# landed, and a claim on one team's board opens no task on another's.
def test_own_rules_take_a_claim_only_as_a_claim(sidecar):
    with agent(sidecar, "boss") as boss, agent(sidecar, "alice", "tasks:own") as alice:
        task = {"title": "Open task", "state": "todo", "created": 1, "created_by": "boss"}
        for team in (boss.name, "other"):
            boss.client.json(f"orvu/{team}/tasks").set(["t-1"], task)
        for first in (["state", "done"], ["lease", 1]):
            try:
                with alice.client.batch() as batch:
                    board = batch.json(f"orvu/{alice.name}/tasks")
                    board.set(["t-1", first[0]], first[1])
                    board.set(["t-1", "claimed_by"], "alice")
            except orvu.OrvuError:
                pass
            assert boss.client.json(f"orvu/{boss.name}/tasks").get(["t-1"]) == task
        try:
            with alice.client.batch() as batch:
                batch.json(f"orvu/{alice.name}/tasks").set(["t-1", "claimed_by"], "alice")
                batch.json("orvu/other/tasks").set(["t-1", "state"], "done")
        except orvu.OrvuError:
            pass
        assert boss.client.json("orvu/other/tasks").get(["t-1"]) == task
        mine = boss.tasks.add("Claimed and given back")
        assert alice.tasks.claim(mine) is True
        assert alice.tasks.update(mine, state="todo").claimed_by is None


# A holder with tasks:own names only itself on the task, or nobody, and a
# top-up and its line land together however the batch is cut short.
def test_own_rules_forge_no_names(sidecar):
    with agent(sidecar, "boss") as boss, agent(sidecar, "alice", "tasks:own", "budgets:top-up") as alice:
        task = boss.tasks.add("Open task")
        assert alice.tasks.claim(task) is True
        board = alice.client.json(f"orvu/{alice.name}/tasks")
        for name in ("ghost", "boss"):
            with pytest.raises(orvu.NotAllowed, match="name another agent on a task"):
                board.set([task.id, "claimed_by"], name)
        assert alice.tasks.release(task) is True
        mine = alice.tasks.add("Mine")
        try:
            with alice.client.batch() as batch:
                batch.cart(f"orvu/{alice.name}/budget-topped/b1").add("alice", 100)
                batch.json(board.name).set([task.id, "state"], "doing")
                batch.json(board.name).set([task.id, "claimed_by"], "alice")
                batch.bounded_counter(f"orvu/{alice.name}/budget/b1").increment(100)
        except orvu.OrvuError:
            pass
        assert alice.budget("b1").topped_up_by().get("alice", 0) == alice.budget("b1").left()
        assert alice.tasks.get(mine).claimed_by is None
