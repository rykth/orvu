"""The MCP tools, called directly on a team that counts what it is asked."""

import dataclasses

from orvu import _tools, _wire
from orvu._tasks import Task


def test_a_refused_claim_never_says_that_nobody_has_the_task():
    free = Task("t-1", "Draft", "todo", None, None, None, None, 0, None, None)

    class Tasks:
        # The other agent gave the task back between the claim and the look.
        def claim(self, task):
            return False

        def get(self, task):
            return free

    @dataclasses.dataclass
    class Team:
        tasks: object

    answer = _tools.TOOLS["task_claim"].run(Team(Tasks()), task="t-1")
    assert answer["claimed"] is False
    assert "None" not in answer["why"] and "free" in answer["why"]


# A firm task's ticket can be out of reach. The agent is told that, and not
# that another agent has the task.
def test_a_refused_firm_claim_says_the_ticket_could_not_be_had():
    free = Task("t-1", "Send the invoice", "todo", None, None, None, None, 0, None, None, firm=True)

    class Tasks:
        def claim(self, task):
            return False

        def get(self, task):
            return free

    @dataclasses.dataclass
    class Team:
        tasks: object

    answer = _tools.TOOLS["task_claim"].run(Team(Tasks()), task="t-1")
    assert answer["claimed"] is False
    assert "ticket" in answer["why"] and "cannot be reached" in answer["why"]


# The reminder asks for word well before the claim runs out, however short
# the claim is.
def test_a_claim_is_kept_with_word_well_before_it_runs_out():
    def claimed(lease):
        mine = Task("t-1", "Draft", "doing", "alice", None, None, None, 0, None, None, seen=0, lease=lease)

        class Tasks:
            def claim(self, task):
                return True

            def get(self, task):
                return mine

        @dataclasses.dataclass
        class Team:
            tasks: object

        return _tools.TOOLS["task_claim"].run(Team(Tasks()), task="t-1")["keep_it"]

    assert "every 5 minutes or so" in claimed(900) and "for 15 minutes" in claimed(900)
    assert "every 1 second or so" in claimed(1) and "for 1 second" in claimed(1)
    assert "every 30 seconds or so" in claimed(90) and "for 90 seconds" in claimed(90)


class Counting:
    """Wraps a client and counts the messages sent to the sidecar."""

    def __init__(self, team):
        self.sent = 0
        real = team.client._call

        def call(send, **options):
            self.sent += 1
            return real(send, **options)

        team.client._call = call


# Showing a task reads it and its sub-tasks, and not the whole board. It
# takes as many messages for thirty sub-tasks as for one.
def test_showing_a_task_reads_it_and_its_sub_tasks_alone(team, monkeypatch):
    report, book = team.tasks.add("Report"), team.tasks.add("Book")
    team.tasks.add("Draft", parent=report)
    for number in range(30):
        team.tasks.add(f"Chapter {number}", parent=book)
    sent = []
    build = _wire.Op.request

    def recording(op, *args):
        sent.append(op.field)
        return build(op, *args)

    monkeypatch.setattr(_wire.Op, "request", recording)
    counting = Counting(team)
    shown = _tools.TOOLS["task_get"].run(team, task=report.id)
    assert [child["title"] for child in shown["sub_tasks"]] == ["Draft"]
    for_one = counting.sent
    assert for_one == 3  # the task, the ids under it, and those tasks
    shown = _tools.TOOLS["task_get"].run(team, task=book.id)
    assert [child["title"] for child in shown["sub_tasks"]] == [f"Chapter {number}" for number in range(30)]
    assert counting.sent == 2 * for_one
    assert "json_to_value" not in sent and "tree_to_nodes" not in sent


def test_listing_the_budgets_is_one_message_however_many_there_are(team):
    for name in ("tokens", "money", "calls"):
        team.budget(name).top_up(5)
    counting = Counting(team)
    assert _tools.TOOLS["budget_list"].run(team) == {"budgets": {"calls": 5, "money": 5, "tokens": 5}}
    assert counting.sent == 2  # the list of names, then every amount at once


# A note two agents wrote at the same time is shown with every text and who
# wrote each, and the agent is told how to settle it.
def test_a_note_that_clashes_is_shown_with_everything_written():
    import orvu

    clash = orvu.Clash("deploy-date", ["Friday", "Monday"], ["alice", None])

    class Notes:
        def get(self, name):
            raise clash

        def all(self):
            return {"deploy-date": clash, "owner": "alice"}

    @dataclasses.dataclass
    class Team:
        notes: object

    shown = {"clash": True, "texts": ["Friday", "Monday"], "written_by": ["alice", None], "settle": _tools._SETTLE}
    assert _tools.TOOLS["note_get"].run(Team(Notes()), name="deploy-date") == {"found": True, "name": "deploy-date", **shown}
    assert _tools.TOOLS["note_list"].run(Team(Notes())) == {"notes": {"deploy-date": shown, "owner": "alice"}}
    assert "note_put" in _tools._SETTLE


# An agent that could not be put on the roster is tried again on each call,
# and told so by whoami and roster_list until it is on.
def test_a_failed_join_is_remembered_and_tried_again():
    import json

    import orvu
    from orvu import _mcp

    class Roster:
        tries = 0

        def join(self):
            Roster.tries += 1
            if Roster.tries < 3:
                raise orvu.SidecarUnavailable("the sidecar went away")

        def members(self):
            return []

    class Team:
        name, me, roster = "default", "alice", Roster()

        def close(self):
            pass

    server = _mcp.Server(lambda: Team(), "default", "alice", joins=True)

    def call(tool):
        line = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool}})
        return json.loads(json.loads(server.answer(line))["result"]["content"][0]["text"])

    first = call("whoami")
    assert first["on_roster"] is False and "SidecarUnavailable" in first["roster_note"]
    second = call("roster_list")
    assert second["agents"] == [] and second["on_roster"] is False
    assert call("whoami") == {"team": "default", "you": "alice"}
    assert call("roster_list") == {"agents": []}
    assert Roster.tries == 3
