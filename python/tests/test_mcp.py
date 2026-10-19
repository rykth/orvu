"""The MCP server, spoken to the way an agent's program speaks to it: as a
child program, one JSON message a line."""

import json
import os
import queue
import re
import signal
import subprocess
import sys
import threading
import time

import pytest

import orvu
from conftest import sidecars, wait_until


class Agent:
    """One `orvu mcp` program, and the calls an agent's program makes to it."""

    def __init__(self, *arguments, env=None, cwd=None):
        self.process = subprocess.Popen(
            [sys.executable, "-m", "orvu._cli", "mcp", *arguments],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=cwd,
        )
        self.asked = 0
        # The server may write a notification at any time, so its output is
        # read as it comes, and notifications are kept apart from answers.
        self.lines = queue.Queue()
        self.notifications = queue.Queue()
        self.reader = threading.Thread(target=self._take_lines, daemon=True)
        self.reader.start()

    def _take_lines(self):
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def send(self, line):
        self.process.stdin.write(line.encode() + b"\n")
        self.process.stdin.flush()

    def read(self):
        while True:
            line = self.lines.get(timeout=60)
            assert line is not None, "the server ended"
            message = json.loads(line)
            if isinstance(message, dict) and "method" in message and "id" not in message:
                self.notifications.put(message)
                continue
            return message

    def notified(self, seconds=10.0):
        """The next notification the server sends, or `None` if none comes
        within `seconds`. Answers that come meanwhile are kept for `read`."""
        deadline = time.monotonic() + seconds
        while True:
            try:
                return self.notifications.get_nowait()
            except queue.Empty:
                pass
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            try:
                line = self.lines.get(timeout=min(left, 0.1))
            except queue.Empty:
                continue
            message = json.loads(line) if line is not None else None
            if isinstance(message, dict) and "method" in message and "id" not in message:
                return message
            # An answer, or the end: put back for `read`.
            self.lines.put(line)

    def ask(self, method, params=None):
        """Sends a request and returns the whole reply."""
        self.asked += 1
        request = {"jsonrpc": "2.0", "id": self.asked, "method": method}
        if params is not None:
            request["params"] = params
        self.send(json.dumps(request))
        reply = self.read()
        assert reply["id"] == self.asked and reply["jsonrpc"] == "2.0"
        return reply

    def call(self, tool, **arguments):
        """Uses a tool. Returns what it answered, and whether it said it
        failed."""
        result = self.ask("tools/call", {"name": tool, "arguments": arguments})["result"]
        (content,) = result["content"]
        assert content["type"] == "text"
        if result["isError"]:
            return content["text"], True
        return json.loads(content["text"]), False

    def ok(self, tool, **arguments):
        answer, failed = self.call(tool, **arguments)
        assert not failed, answer
        return answer

    def stop(self):
        """Closes the input, which is how an agent's program ends a server.
        Returns what the server wrote for people to read."""
        self.process.stdin.close()
        code = self.process.wait(timeout=10)
        log = self.process.stderr.read().decode()
        self.reader.join(timeout=10)
        rest = ""
        while (line := self.lines.get_nowait()) is not None:
            rest += line.decode()
        self.process.stdout.close()
        self.process.stderr.close()
        assert code == 0, log
        assert rest == "", f"the server wrote more than it was asked for: {rest!r}"
        return log


@pytest.fixture
def agents(sidecar):
    """Starts agents on this test's sidecar: `agents("alice")`."""
    started = []

    def start(me, *more):
        agent = Agent("--socket", str(sidecar.socket), "--me", me, *more)
        started.append(agent)
        return agent

    yield start
    for agent in started:
        if agent.process.poll() is None:
            agent.stop()


@pytest.fixture
def alice(agents):
    return agents("alice")


@pytest.mark.parametrize("version", ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"])
def test_the_server_says_hello_in_the_version_it_was_asked_for(alice, version):
    hello = alice.ask(
        "initialize",
        {"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}},
    )["result"]
    assert hello["protocolVersion"] == version
    assert hello["capabilities"] == {"tools": {}, "resources": {"subscribe": True}, "prompts": {}}
    assert hello["serverInfo"] == {"name": "orvu", "version": orvu.__version__}
    assert "'alice'" in hello["instructions"] and "'default'" in hello["instructions"]
    assert "task_still_working" in hello["instructions"]


# A program that asks for a version this does not know is offered the newest
# one this does know. It is then the program's choice to carry on or not.
def test_an_unknown_version_is_answered_with_the_newest_known_one(alice):
    hello = alice.ask("initialize", {"protocolVersion": "2099-01-01", "capabilities": {}})["result"]
    assert hello["protocolVersion"] == "2025-11-25"


def test_a_ping_is_answered(alice):
    assert alice.ask("ping")["result"] == {}


# A notification tells the server something and expects no answer. If the
# server answered one, every later answer would be read as the wrong one.
def test_a_notification_gets_no_answer(alice):
    alice.send(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}))
    alice.send(json.dumps({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}))
    alice.send("")
    assert alice.ask("ping")["result"] == {}


@pytest.mark.parametrize(
    "line, code",
    [
        ("this is not json", -32700),
        ('"text"', -32600),
        ('{"jsonrpc": "2.0", "id": 1}', None),
    ],
)
def test_a_line_that_is_not_a_request_does_not_stop_the_server(alice, line, code):
    alice.send(line)
    if code is not None:
        reply = alice.read()
        assert reply["error"]["code"] == code and reply["id"] is None
    assert alice.ask("ping")["result"] == {}


def test_what_the_server_does_not_have_is_refused(alice):
    assert alice.ask("sampling/createMessage")["error"]["code"] == -32601
    assert alice.ask("tools/call", {"name": "note_burn", "arguments": {}})["error"]["code"] == -32602
    assert alice.ask("tools/call", ["note_list"])["error"]["code"] == -32602
    assert alice.ask("ping")["result"] == {}


def test_the_server_ends_when_its_input_is_closed(alice):
    alice.ok("whoami")
    log = alice.stop()
    assert "team 'default'" in log and "agent 'alice'" in log


def test_who_am_i(agents):
    assert agents("alice").ok("whoami") == {"team": "default", "you": "alice"}
    assert agents("bob", "--team", "crew").ok("whoami") == {"team": "crew", "you": "bob"}


# Text goes out as UTF-8 whatever the machine the server runs on is set to.
# An agent's program may start it with settings a terminal would not have.
def test_text_that_is_not_plain_ascii_gets_through_on_any_machine(sidecar):
    env = {**os.environ, "PYTHONIOENCODING": "ascii", "PYTHONUTF8": "0"}
    agent = Agent("--socket", str(sidecar.socket), "--me", "ålice", env=env)
    assert agent.ok("whoami")["you"] == "ålice"
    assert "ålice" in agent.ask("initialize", {"protocolVersion": "2025-06-18"})["result"]["instructions"]
    agent.stop()


def test_arguments_can_be_left_out_altogether_for_a_tool_that_takes_none(alice):
    result = alice.ask("tools/call", {"name": "whoami"})["result"]
    assert result["isError"] is False
    assert alice.ask("tools/call", {"name": "whoami", "arguments": None})["result"]["isError"] is False
    wrong, failed = alice.call("whoami", team="other")
    assert failed and "has no argument 'team'. It takes: no arguments" in wrong
    assert alice.ask("tools/call", {"name": "whoami", "arguments": ["x"]})["result"]["isError"] is True


def test_no_sidecar_is_a_tools_answer_and_the_next_call_works(tmp_path, sidecar):
    missing = Agent("--socket", str(tmp_path / "nothing-here.sock"), "--me", "alice")
    answer, failed = missing.call("whoami")
    assert failed and "SidecarUnavailable" in answer
    assert missing.ask("ping")["result"] == {}
    missing.stop()


def test_an_agent_that_was_given_no_name_gets_one_of_its_own(sidecar, tmp_path):
    (tmp_path / "one").mkdir()
    (tmp_path / "two").mkdir()
    first = Agent("--socket", str(sidecar.socket), cwd=tmp_path / "one")
    second = Agent("--socket", str(sidecar.socket), "--me", "", cwd=tmp_path / "two")
    names = [first.ok("whoami")["you"], second.ok("whoami")["you"]]
    assert all(re.fullmatch("agent-[0-9a-f]{8}", name) for name in names) and names[0] != names[1]
    assert f"agent {names[0]!r}" in first.stop()
    second.stop()


# A made-up name comes from the team and the folder the server was started
# in, so an agent started again in the same place is the same agent, and
# keeps its claims and its place in the inbox.
def test_a_made_up_name_is_the_same_each_time_in_the_same_folder_for_the_same_team(sidecar, tmp_path):
    names = []
    for team in ("default", "default", "crew"):
        agent = Agent("--socket", str(sidecar.socket), "--team", team, cwd=tmp_path)
        names.append(agent.ok("whoami")["you"])
        agent.stop()
    assert names[0] == names[1] and names[0] != names[2]


# With no socket given, the server starts the sidecar itself, the first time a
# tool is used, as `Team.connect()` does.
def test_with_no_socket_the_sidecar_is_started(home):
    agent = Agent("--me", "alice", "--team", "crew")
    assert agent.ask("tools/list")["result"]["tools"]
    assert sidecars(home) == [], "a sidecar was started before any tool was used"
    assert agent.ok("whoami") == {"team": "crew", "you": "alice"}
    assert len(sidecars(home)) == 1
    agent.stop()
    with orvu.Team.connect("crew") as team:
        # The agent was on the roster while its server ran, and is off it.
        assert team.index() == {"roster": "map"}
        assert team.roster.members() == []
    assert len(sidecars(home)) == 1


def test_a_team_name_that_cannot_be_used_stops_the_command(home):
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp", "--team", "two words"],
        capture_output=True,
        text=True,
        stdin=subprocess.DEVNULL,
    )
    assert done.returncode == 1 and "name of a team" in done.stderr and done.stdout == ""
    assert sidecars(home) == []


def test_the_command_says_its_version_and_what_it_can_do():
    version = subprocess.run([sys.executable, "-m", "orvu._cli", "--version"], capture_output=True, text=True)
    assert version.stdout.strip() == f"orvu {orvu.__version__}"
    bare = subprocess.run([sys.executable, "-m", "orvu._cli"], capture_output=True, text=True)
    assert bare.returncode == 2 and "mcp" in bare.stdout


def test_every_tool_is_listed_with_a_description_of_itself_and_its_arguments(alice):
    listed = alice.ask("tools/list")["result"]["tools"]
    assert [tool["name"] for tool in listed] == [
        "whoami",
        "note_put",
        "note_get",
        "note_author",
        "note_list",
        "note_remove",
        "note_structured_get",
        "note_structured_set",
        "note_structured_remove",
        "note_structured_list",
        "task_add",
        "task_list",
        "task_get",
        "task_claim",
        "task_still_working",
        "task_release",
        "task_finish",
        "task_update",
        "task_move",
        "budget_list",
        "budget_left",
        "budget_spend",
        "budget_spent",
        "inbox_send",
        "inbox_read",
        "inbox_recent",
        "doc_list",
        "doc_read",
        "doc_append",
        "doc_insert",
        "doc_remove",
        "search",
        "tally_list",
        "tally_read",
        "tally_add",
        "roster_list",
        "object_read",
        "object_counter_add",
        "object_register_set",
        "object_set_add",
        "object_set_remove",
        "object_map_put",
        "object_map_remove",
        "object_list_insert",
        "object_list_remove",
        "object_json_set",
        "object_json_remove",
    ]
    for tool in listed:
        assert len(tool["description"]) > 30, f"{tool['name']} does not say what it is for"
        schema = tool["inputSchema"]
        assert schema["type"] == "object"
        assert set(schema["required"]) <= set(schema["properties"])
        for name, argument in schema["properties"].items():
            # A value that can be any JSON says no type.
            assert argument.get("type", "any") in ("string", "integer", "boolean", "array", "any")
            assert argument["description"], f"{tool['name']}: {name} is not described"


def test_notes(alice):
    assert alice.ok("note_list") == {"notes": {}}
    assert alice.ok("note_get", name="deploy-date") == {"found": False, "name": "deploy-date"}
    assert alice.ok("note_put", name="deploy-date", text="Friday") == {"saved": "deploy-date"}
    alice.ok("note_put", name="owner", text="ålice 日本語 ✓")
    assert alice.ok("note_get", name="deploy-date") == {"found": True, "name": "deploy-date", "text": "Friday"}
    assert alice.ok("note_list") == {"notes": {"deploy-date": "Friday", "owner": "ålice 日本語 ✓"}}
    assert alice.ok("note_remove", name="deploy-date") == {"removed": "deploy-date"}
    assert alice.ok("note_list") == {"notes": {"owner": "ålice 日本語 ✓"}}


def test_a_task_from_added_to_done(alice):
    assert alice.ok("task_list") == {"tasks": []}
    task = alice.ok("task_add", title="Summarise the Q3 report", notes="Two pages at most")["task"]
    assert task == {
        "id": task["id"],
        "title": "Summarise the Q3 report",
        "state": "todo",
        "notes": "Two pages at most",
        "created_by": "alice",
    }
    step = alice.ok("task_add", title="Find the report", parent=task["id"])["task"]
    assert step["parent"] == task["id"]
    assert alice.ok("task_get", task=task["id"]) == {"task": task, "sub_tasks": [step]}

    claimed = alice.ok("task_claim", task=task["id"])
    assert claimed["claimed"] is True
    assert (claimed["task"]["state"], claimed["task"]["claimed_by"]) == ("doing", "alice")
    assert [found["id"] for found in alice.ok("task_list", state="doing")["tasks"]] == [task["id"]]

    finished = alice.ok("task_finish", task=task["id"], result="Sales rose 4%")
    assert finished["finished"] is True
    assert finished["task"]["state"] == "done"
    assert (finished["task"]["result"], finished["task"]["finished_by"]) == ("Sales rose 4%", "alice")
    again = alice.ok("task_finish", task=task["id"], result="Sales fell")
    assert (again["finished"], again["task"]["result"]) == (False, "Sales rose 4%")
    assert "already done" in again["why"]


def test_a_task_is_updated_and_put_back(alice):
    task = alice.ok("task_add", title="Draft")["task"]
    alice.ok("task_claim", task=task["id"])
    changed = alice.ok("task_update", task=task["id"], title="Final draft", notes="second try")["task"]
    assert (changed["title"], changed["notes"], changed["state"]) == ("Final draft", "second try", "doing")
    freed = alice.ok("task_update", task=task["id"], state="todo")["task"]
    assert freed["state"] == "todo" and "claimed_by" not in freed


def test_budgets(agents):
    alice = agents("alice", "--allow-top-up")
    assert alice.ok("budget_list") == {"budgets": {}}
    assert alice.ok("budget_top_up", budget="tokens", amount=1000) == {"left": 1000}
    assert alice.ok("budget_spend", budget="tokens", amount=400) == {"spent": True, "left": 600}
    assert alice.ok("budget_spend", budget="tokens", amount=601) == {"spent": False, "left": 600}
    assert alice.ok("budget_list") == {"budgets": {"tokens": 600}}


# Topping up is left out unless it was asked for. An agent that has run out
# would otherwise be able to give itself more.
def test_an_agent_cannot_top_up_a_budget_unless_it_was_allowed_to(agents, team):
    alice = agents("alice")
    assert "budget_top_up" not in [tool["name"] for tool in alice.ask("tools/list")["result"]["tools"]]
    refused = alice.ask("tools/call", {"name": "budget_top_up", "arguments": {"budget": "tokens", "amount": 5}})
    assert refused["error"]["code"] == -32602
    assert team.budget("tokens").left() == 0

    banker = agents("banker", "--allow-top-up")
    assert "budget_top_up" in [tool["name"] for tool in banker.ask("tools/list")["result"]["tools"]]
    assert banker.ok("budget_top_up", budget="tokens", amount=5) == {"left": 5}


def test_two_agents_share_the_team(agents):
    alice, bob = agents("alice", "--allow-top-up"), agents("bob")
    alice.ok("note_put", name="deploy-date", text="Friday")
    task = alice.ok("task_add", title="Draft")["task"]
    alice.ok("budget_top_up", budget="tokens", amount=10)

    assert bob.ok("note_get", name="deploy-date")["text"] == "Friday"
    assert bob.ok("task_list")["tasks"] == [task]
    assert bob.ok("budget_spend", budget="tokens", amount=4) == {"spent": True, "left": 6}
    assert alice.ok("budget_left", budget="tokens")["left"] == 6


def test_a_task_one_agent_took_is_refused_to_the_other(agents):
    alice, bob = agents("alice"), agents("bob")
    task = alice.ok("task_add", title="Draft")["task"]
    assert alice.ok("task_claim", task=task["id"])["claimed"] is True

    refused = bob.ok("task_claim", task=task["id"])
    assert (refused["claimed"], refused["why"]) == (False, "alice has it")
    assert bob.ok("task_release", task=task["id"]) == {
        "released": False,
        "why": "the task is not yours, or it is done",
    }

    assert alice.ok("task_release", task=task["id"]) == {"released": True}
    assert bob.ok("task_claim", task=task["id"])["claimed"] is True
    bob.ok("task_finish", task=task["id"], result="ok")
    late = alice.ok("task_claim", task=task["id"])
    assert (late["claimed"], late["why"]) == (False, "it is already done")


def test_a_firm_task_over_the_tools(agents):
    alice, bob = agents("alice"), agents("bob")
    task = alice.ok("task_add", title="Send the invoice", firm=True)["task"]
    assert (task["firm"], task["ticket"]) == (True, 0)
    assert "firm" not in alice.ok("task_add", title="Draft", firm=False)["task"]
    assert bob.ok("task_claim", task=task["id"])["claimed"] is True
    refused = alice.ok("task_claim", task=task["id"])
    assert (refused["claimed"], refused["why"]) == (False, "bob has it")
    # Only bob can finish it.
    not_hers = alice.ok("task_finish", task=task["id"], result="sent")
    assert not_hers["finished"] is False and "only the agent that has it" in not_hers["why"]
    assert bob.ok("task_finish", task=task["id"], result="sent")["finished"] is True
    wrong, failed = alice.call("task_add", title="Draft", firm="yes")
    assert failed and "true or false" in wrong


# An agent that took a task has to say now and then that it is still on it.
# One that goes quiet for longer than its claim lasts loses the task to the
# next agent that claims it.
def test_a_claim_is_kept_with_still_working_and_lost_without(agents, sidecar):
    alice, bob = agents("alice"), agents("bob")
    task = alice.ok("task_add", title="Draft")["task"]
    claimed = alice.ok("task_claim", task=task["id"])
    assert "task_still_working every 5 minutes" in claimed["keep_it"] and "15 minutes" in claimed["keep_it"]
    assert alice.ok("task_still_working", task=task["id"]) == {"still_yours": True}
    assert bob.ok("task_still_working", task=task["id"]) == {
        "still_yours": False,
        "why": "alice has it. Stop working on it",
    }

    # Carol, a program, takes a task for a second and goes quiet.
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        quiet = carol.tasks.add("Check the figures")
        carol.tasks.claim(quiet, lease=1)
        # Bob has to have seen the claim to know how long it has been quiet.
        assert "abandoned" not in bob.ok("task_get", task=quiet.id)["task"]
        time.sleep(1.2)
        shown = bob.ok("task_get", task=quiet.id)["task"]
        assert shown["abandoned"] is True
        assert "quiet_since" not in shown, "a reading of the server's own clock means nothing to the agent"
        taken = bob.ok("task_claim", task=quiet.id)
        assert (taken["claimed"], taken["took_over_from"]) == (True, "carol")
        assert "abandoned" not in taken["task"]
        assert carol.tasks.still_working(quiet) is False


# What a program wrote with the library is what an agent sees with the tools,
# and the other way round.
def test_the_tools_and_the_library_see_the_same_team(alice, team):
    team.notes.put("deploy-date", "Friday")
    task = team.tasks.add("Draft")
    assert alice.ok("note_get", name="deploy-date")["text"] == "Friday"
    alice.ok("task_claim", task=task.id)
    assert team.tasks.get(task).claimed_by == "alice"


def test_two_teams_on_one_sidecar_share_nothing(agents):
    red, blue = agents("alice", "--team", "red"), agents("alice", "--team", "blue")
    red.ok("note_put", name="deploy-date", text="Friday")
    red.ok("task_add", title="Draft")
    assert blue.ok("note_list") == {"notes": {}}
    assert blue.ok("task_list") == {"tasks": []}


# The agent's model reads what comes back and tries again. So each of these is
# an answer from the tool that says what was wrong, not a failed request.
@pytest.mark.parametrize(
    "tool, arguments, says",
    [
        ("note_put", {"name": "deploy-date"}, "needs the argument 'text'"),
        ("note_put", {"name": "deploy-date", "text": 7}, "'text' has to be text"),
        ("note_put", {"name": "", "text": "Friday"}, "name"),
        ("note_get", {"name": "deploy-date", "team": "other"}, "has no argument 'team'"),
        ("note_list", {"state": "todo"}, "has no argument 'state'"),
        ("task_add", {"title": "  "}, "needs a title"),
        ("task_add", {"title": "Sub", "parent": "t-nope"}, "there is no task 't-nope'"),
        ("task_list", {"state": "finished"}, "has to be one of ['todo', 'doing', 'done']"),
        ("task_get", {"task": "t-nope"}, "there is no task 't-nope'"),
        ("task_claim", {"task": "t-nope"}, "UnknownTask"),
        ("task_finish", {"task": "t-nope"}, "UnknownTask"),
        ("task_update", {"task": "t-nope", "title": "x"}, "UnknownTask"),
        ("budget_spend", {"budget": "tokens", "amount": 0}, "whole number above zero"),
        ("budget_spend", {"budget": "tokens", "amount": "5"}, "'amount' has to be a whole number"),
        ("budget_spend", {"budget": "tokens", "amount": True}, "'amount' has to be a whole number"),
        ("budget_spend", {"budget": "tokens", "amount": 1.5}, "'amount' has to be a whole number"),
        ("budget_spend", {"budget": "a/b", "amount": 5}, "name of a budget"),
        ("budget_left", {"budget": ""}, "name of a budget"),
    ],
)
def test_a_tool_used_wrongly_says_what_was_wrong_and_changes_nothing(alice, tool, arguments, says):
    answer, failed = alice.call(tool, **arguments)
    assert failed and says in answer, answer
    assert alice.ok("note_list") == {"notes": {}}
    assert alice.ok("task_list") == {"tasks": []}
    assert alice.ok("budget_list") == {"budgets": {}}


def test_the_tools_go_on_working_after_the_sidecar_restarts(alice, sidecar):
    alice.ok("note_put", name="deploy-date", text="Friday")
    sidecar.restart()
    assert alice.ok("note_get", name="deploy-date")["text"] == "Friday"


# ---- what an agent's program may send or leave out ---------------------------


def bare(home, *names):
    """The few settings many agent programs start a server with. They pass on
    the home folder and the path, and not much else."""
    return {name: os.environ[name] for name in ("HOME", "PATH", "ORVUD_BIN", *names)}


# A program in a terminal finds the sidecar through settings the terminal
# has. A server started by an agent's program often has none of them, and
# still has to end up at the same sidecar.
def test_a_server_with_few_settings_finds_the_sidecar_a_terminal_started(home):
    with orvu.Team.connect(me="terminal") as team:
        team.notes.put("deploy-date", "Friday")
        agent = Agent("--me", "alice", env=bare(home))
        assert agent.ok("note_get", name="deploy-date")["text"] == "Friday"
        agent.stop()
    assert len(sidecars(home)) == 1


# A sidecar started by hand in a terminal, with `orvud --local`, puts its
# socket where the terminal's settings say. It notes that in the data folder,
# where a server with few settings looks, as a sidecar the package starts
# does.
def test_a_server_with_few_settings_finds_a_sidecar_started_by_hand(home):
    from conftest import orvud_bin

    by_hand = subprocess.Popen([str(orvud_bin()), "--local"], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        assert b"serving on" in by_hand.stderr.readline()
        agent = Agent("--me", "alice", env=bare(home))
        answer, failed = agent.call("note_put", name="deploy-date", text="Friday")
        assert not failed, answer
        agent.stop()
        # No sidecar of the package's was started beside it.
        assert sidecars(home) == []
    finally:
        by_hand.terminate()
        by_hand.wait(timeout=10)
        by_hand.stderr.close()


def test_a_terminal_finds_the_sidecar_a_server_with_few_settings_started(home):
    agent = Agent("--me", "alice", env=bare(home))
    agent.ok("note_put", name="deploy-date", text="Friday")
    with orvu.Team.connect(me="terminal") as team:
        assert team.notes.get("deploy-date") == "Friday"
    agent.stop()
    assert len(sidecars(home)) == 1


# The sidecar comes back where this server would put it, which is not where
# the terminal had put it. The server has to follow it there.
def test_a_server_with_few_settings_starts_the_sidecar_again_when_it_is_gone(home):
    with orvu.Team.connect(me="terminal") as team:
        team.notes.put("deploy-date", "Friday")
    agent = Agent("--me", "alice", env=bare(home))
    assert agent.ok("note_get", name="deploy-date")["text"] == "Friday"
    for pid in sidecars(home):
        os.kill(pid, signal.SIGKILL)
    wait_until(lambda: not sidecars(home))
    assert agent.ok("note_get", name="deploy-date")["text"] == "Friday"
    with orvu.Team.connect(me="terminal") as team:
        assert team.notes.get("deploy-date") == "Friday"
    agent.stop()
    assert len(sidecars(home)) == 1


# Many models fill in the arguments they do not use with null. That means
# "left out".
def test_an_argument_given_as_null_counts_as_left_out(alice):
    task = alice.ok("task_add", title="Draft", notes=None, parent=None)["task"]
    assert task["title"] == "Draft" and "notes" not in task
    assert [found["id"] for found in alice.ok("task_list", state=None)["tasks"]] == [task["id"]]
    assert alice.ok("task_update", task=task["id"], title="Final", notes=None, state=None)["task"]["title"] == "Final"
    assert alice.ok("task_finish", task=task["id"], result=None)["finished"] is True
    wrong, failed = alice.call("task_add", title=None)
    assert failed and "needs the argument 'title'" in wrong


# JSON has one kind of number, and some programs write 1500 as 1500.0.
def test_a_whole_number_written_with_a_point_is_a_whole_number(agents):
    alice = agents("alice", "--allow-top-up")
    assert alice.ok("budget_top_up", budget="tokens", amount=1000.0) == {"left": 1000}
    assert alice.ok("budget_spend", budget="tokens", amount=400.0) == {"spent": True, "left": 600}


@pytest.mark.parametrize(
    "line, code",
    [
        ('{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": ["note_put"]}}', -32602),
        ('{"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": {"a": 1}}}', -32602),
        ("[" * 200_000, -32700),
    ],
    ids=["a list as the tool's name", "an object as the tool's name", "nested too deeply"],
)
def test_a_request_that_is_badly_wrong_does_not_stop_the_server(alice, line, code):
    alice.send(line)
    assert alice.read()["error"]["code"] == code
    assert alice.ask("ping")["result"] == {}


# The answer to a request that was refused still carries the request's id.
# Without it the program that asked cannot tell which request failed.
def test_a_refused_request_is_answered_under_its_own_id(alice):
    alice.send('{"jsonrpc": "2.0", "id": 7, "method": 5}')
    reply = alice.read()
    assert reply["id"] == 7 and reply["error"]["code"] == -32600


# One version of MCP lets a program send several messages as one list. Each
# request in it gets its answer, in one list back.
def test_several_messages_in_one_line_are_each_answered(alice):
    alice.send(
        json.dumps(
            [
                {"jsonrpc": "2.0", "id": "a", "method": "ping"},
                {"jsonrpc": "2.0", "method": "notifications/initialized"},
                {"jsonrpc": "2.0", "id": "b", "method": "tools/call", "params": {"name": "whoami"}},
                7,
            ]
        )
    )
    replies = alice.read()
    assert [reply["id"] for reply in replies] == ["a", "b", None]
    assert replies[0]["result"] == {} and replies[1]["result"]["isError"] is False
    assert replies[2]["error"]["code"] == -32600

    alice.send("[]")
    assert alice.read()["error"]["code"] == -32600
    # A list of notifications only gets no answer at all.
    alice.send(json.dumps([{"jsonrpc": "2.0", "method": "notifications/initialized"}]))
    assert alice.ask("ping")["result"] == {}


# A budget that was never topped up reads as 0, which looks the same as one
# that has run out. A name typed wrongly would stop an agent for no reason.
def test_a_budget_that_does_not_exist_is_not_taken_for_an_empty_one(agents):
    alice = agents("alice", "--allow-top-up")
    alice.ok("budget_top_up", budget="tokens", amount=10)
    for tool, arguments in [("budget_left", {"budget": "token"}), ("budget_spend", {"budget": "token", "amount": 5})]:
        answer, failed = alice.call(tool, **arguments)
        assert failed and "there is no budget 'token'" in answer and "budget_list" in answer
    assert alice.ok("budget_spend", budget="tokens", amount=10) == {"spent": True, "left": 0}
    assert alice.ok("budget_left", budget="tokens") == {"budget": "tokens", "left": 0}
    assert alice.ok("budget_spend", budget="tokens", amount=1) == {"spent": False, "left": 0}


def test_a_made_up_name_is_long_enough_not_to_be_shared():
    from orvu import _mcp

    assert re.fullmatch("agent-[0-9a-f]{8}", _mcp.made_up_name("default", "/home/a/project"))
    assert _mcp.made_up_name("default", "/home/a/project") == _mcp.made_up_name("default", "/home/a/project")
    assert _mcp.made_up_name("default", "/home/a/project") != _mcp.made_up_name("default", "/home/a/other")
    assert _mcp.made_up_name("default", "/home/a/project") != _mcp.made_up_name("crew", "/home/a/project")


# A note the library wrote as a number or a list comes through the tools as
# what it is, and a task says who changed it.
def test_the_tools_show_what_the_newer_blocks_wrote(alice, team):
    team.notes.put("limit", {"tokens": 5000})
    team.notes.put("retries", 3)
    assert alice.ok("note_get", name="limit") == {"found": True, "name": "limit", "text": {"tokens": 5000}}
    assert alice.ok("note_list")["notes"] == {"limit": {"tokens": 5000}, "retries": 3}
    task = alice.ok("task_add", title="Draft")["task"]
    assert alice.ok("task_update", task=task["id"], notes="Use the new numbers")["task"]["updated_by"] == "alice"
    alice.ok("note_put", name="deploy-date", text="Friday")
    assert team.notes.author("deploy-date") == "alice"


# ---- messages, documents, tallies and the roster ------------------------------


def test_agents_send_each_other_messages(agents):
    alice, bob = agents("alice"), agents("bob")
    assert bob.ok("inbox_read") == {"messages": []}
    assert alice.ok("inbox_send", to="bob", text="The Q3 numbers are in the notes.") == {"sent_to": "bob"}
    assert alice.ok("inbox_send", text="Deploy is on Friday.") == {"sent_to": "everyone"}

    letters = bob.ok("inbox_read")["messages"]
    assert [(m["from"], m["to"], m["text"]) for m in letters] == [
        ("alice", "bob", "The Q3 numbers are in the notes."),
        ("alice", "everyone", "Deploy is on Friday."),
    ]
    assert all(isinstance(m["sent"], int) for m in letters)
    # Each message is given once. Asking for the recent ones gives them again.
    assert bob.ok("inbox_read") == {"messages": []}
    assert [m["text"] for m in bob.ok("inbox_recent")["messages"]] == [
        "The Q3 numbers are in the notes.",
        "Deploy is on Friday.",
    ]
    # What was for bob is not given to alice. What was for everyone is.
    assert [m["text"] for m in alice.ok("inbox_read")["messages"]] == ["Deploy is on Friday."]


def test_a_message_from_the_library_reaches_an_agent_and_back(alice, team, sidecar):
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        carol.inbox.send("alice", {"task": "t-1", "say": "please check"})
        (letter,) = alice.ok("inbox_read")["messages"]
        assert letter["from"] == "carol" and letter["text"] == {"task": "t-1", "say": "please check"}
        alice.ok("inbox_send", to="carol", text="done")
        assert [message.body for message in carol.inbox.unread()] == ["done"]


def test_agents_write_a_document_together(agents):
    alice, bob = agents("alice"), agents("bob")
    assert alice.ok("doc_list") == {"documents": []}
    # A document nobody has written is not an empty one. A name typed
    # wrongly would otherwise look like a report with nothing in it.
    answer, failed = alice.call("doc_read", document="q3-report")
    assert failed and "there is no document 'q3-report'" in answer and "doc_list" in answer
    assert alice.ok("doc_list") == {"documents": []}, "reading a document started it"
    assert alice.ok("doc_append", document="q3-report", text="Sales rose 4%.") == {"document": "q3-report", "added": 14}
    bob.ok("doc_append", document="q3-report", text=" Costs fell.")
    assert bob.ok("doc_read", document="q3-report") == {
        "document": "q3-report",
        "parts": [{"text": "Sales rose 4%.", "by": "alice"}, {"text": " Costs fell.", "by": "bob"}],
    }
    assert bob.ok("doc_list") == {"documents": ["q3-report"]}


# An agent finds what another wrote by what it means, and is told when it
# asks for something it cannot have.
def test_an_agent_finds_what_the_team_knows(agents):
    alice, bob = agents("alice"), agents("bob")
    alice.ok("note_put", name="refund-policy", text="customers get their money back")
    alice.ok("doc_append", document="plan", text="We ship on friday.")
    assert bob.ok("search", query="refund") == {
        "found": [{"kind": "note", "name": "refund-policy", "text": "refund-policy: customers get their money back", "score": 1.0}],
    }
    assert bob.ok("search", query="deploy", limit=1) == {
        "found": [{"kind": "document", "name": "plan", "paragraph": 1, "text": "We ship on friday.", "score": 1.0}],
    }
    assert bob.ok("search", query="the cat") == {"found": []}
    answer, failed = bob.call("search", query="refund", limit=99)
    assert failed and "from 1 to 50" in answer


def test_agents_count_on_a_tally(agents):
    alice, bob = agents("alice"), agents("bob")
    assert alice.ok("tally_list") == {"tallies": []}
    answer, failed = alice.call("tally_read", tally="errors")
    assert failed and "there is no tally 'errors'" in answer and "tally_list" in answer
    assert alice.ok("tally_add", tally="errors") == {"tally": "errors", "total": 1}
    assert bob.ok("tally_add", tally="errors", amount=2, under="timeout") == {"tally": "errors", "total": 3}
    assert bob.ok("tally_add", tally="errors", amount=4.0)["total"] == 7
    assert alice.ok("tally_read", tally="errors") == {
        "tally": "errors",
        "total": 7,
        "by_name": {"alice": 1, "bob": 4, "timeout": 2},
    }
    assert alice.ok("tally_list") == {"tallies": ["errors"]}


def test_an_agent_sees_who_is_on_the_roster(alice, bob):
    bob.roster.join()
    assert {"name": "bob", "here": True} in alice.ok("roster_list")["agents"]
    bob.roster.leave()
    assert {"name": "bob", "here": True} not in alice.ok("roster_list")["agents"]


# An agent that was given a name is on the roster for as long as its server
# runs, without asking. That is what "here" means for an agent.
def test_an_agent_with_a_name_is_on_the_roster_while_its_server_runs(agents, team):
    alice = agents("alice")
    assert team.roster.members() == [], "it joined before it used a tool"
    assert alice.ok("roster_list") == {"agents": [{"name": "alice", "here": True}]}
    bob = agents("bob")
    bob.ok("whoami")
    assert alice.ok("roster_list") == {"agents": [{"name": "alice", "here": True}, {"name": "bob", "here": True}]}
    bob.stop()
    assert alice.ok("roster_list") == {"agents": [{"name": "alice", "here": True}]}


# Agent programs often end a server with SIGTERM rather than by closing its
# input. That is a warning too: the agent leaves the roster as it goes.
def test_an_agent_whose_server_is_asked_to_stop_leaves_the_roster(agents, team):
    alice = agents("alice")
    assert alice.ok("roster_list") == {"agents": [{"name": "alice", "here": True}]}
    alice.process.send_signal(signal.SIGTERM)
    assert alice.process.wait(timeout=10) == 0
    assert team.roster.members() == []


# A made-up name stays the same in the same folder, so it goes on the roster
# like any other. A message to it then finds it there.
def test_an_agent_with_a_made_up_name_is_put_on_the_roster(sidecar, team, tmp_path):
    nameless = Agent("--socket", str(sidecar.socket), cwd=tmp_path)
    me = nameless.ok("whoami")["you"]
    assert nameless.ok("roster_list") == {"agents": [{"name": me, "here": True}]}
    assert team.roster.here() == [me]
    alice = Agent("--socket", str(sidecar.socket), "--me", "alice")
    assert alice.ok("inbox_send", to=me, text="hello") == {"sent_to": me}
    alice.stop()
    nameless.stop()
    assert team.roster.members() == []


@pytest.mark.parametrize(
    "tool, arguments, says",
    [
        ("inbox_send", {"to": "bob"}, "needs 'text' or 'data'"),
        ("inbox_send", {"to": "", "text": "hello"}, "agent to send to"),
        ("inbox_send", {"to": "bob", "text": 7}, "'text' has to be text"),
        ("inbox_read", {"all": "yes"}, "no arguments"),
        ("doc_append", {"document": "two words", "text": "x"}, "name of a document"),
        ("doc_append", {"document": "q3-report"}, "needs the argument 'text'"),
        ("doc_read", {"document": "a/b"}, "name of a document"),
        ("tally_add", {"tally": "errors", "amount": 0}, "whole number above zero"),
        ("tally_add", {"tally": "errors", "amount": "2"}, "'amount' has to be a whole number"),
        ("tally_add", {"tally": "errors", "under": ""}, "some text"),
        ("tally_read", {"tally": "two words"}, "name of a tally"),
        ("tally_add", {"tally": "errors", "amount": 2**64}, "too large"),
        ("doc_append", {"document": "q3-report", "text": ""}, "nothing to add"),
    ],
)
def test_a_newer_tool_used_wrongly_says_what_was_wrong_and_changes_nothing(alice, team, tool, arguments, says):
    answer, failed = alice.call(tool, **arguments)
    assert failed and says in answer, answer
    assert alice.ok("doc_list") == {"documents": []}
    assert alice.ok("tally_list") == {"tallies": []}
    assert alice.ok("inbox_recent") == {"messages": []}
    assert set(team.index()) <= {"roster"}


# The tools show a message to all as sent to "everyone". An agent that
# answers the same way means all of them, not an agent of that name.
def test_a_message_to_everyone_by_that_word_goes_to_everyone(agents):
    alice, bob = agents("alice"), agents("bob")
    assert alice.ok("inbox_send", to="everyone", text="Deploy is on Friday.") == {"sent_to": "everyone"}
    assert [m["text"] for m in bob.ok("inbox_read")["messages"]] == ["Deploy is on Friday."]


# A message to a name nobody has is kept, in case that agent starts later.
# The sender is told that nobody by that name is there now.
def test_a_message_to_a_name_nobody_has_says_so(agents):
    alice, bob = agents("alice"), agents("bob")
    bob.ok("whoami")
    assert alice.ok("inbox_send", to="bob", text="hello") == {"sent_to": "bob"}
    answer = alice.ok("inbox_send", to="bobb", text="hello")
    assert answer["sent_to"] == "bobb" and "nobody called 'bobb' is on the roster" in answer["note"]


# The agent's program starts the server again: a new session, or a crash.
# What the agent was given before is not given again as if it were new.
def test_an_agent_is_not_given_a_message_again_after_its_server_starts_again(agents):
    alice = agents("alice")
    alice.ok("inbox_send", to="bob", text="please redeploy staging")
    bob = agents("bob")
    assert [m["text"] for m in bob.ok("inbox_read")["messages"]] == ["please redeploy staging"]
    bob.stop()
    bob = agents("bob")
    assert bob.ok("inbox_read") == {"messages": []}
    assert [m["text"] for m in bob.ok("inbox_recent")["messages"]] == ["please redeploy staging"]


# The roster is a courtesy. If the agent cannot be put on it, its tools
# work all the same.
def test_the_tools_work_when_the_roster_cannot_be_joined(agents, team):
    team.client.counter("orvu/default/roster").increment(1)
    alice = agents("alice")
    answer = alice.ok("whoami")
    assert (answer["team"], answer["you"], answer["on_roster"]) == ("default", "alice", False)
    assert alice.ok("note_put", name="deploy-date", text="Friday") == {"saved": "deploy-date"}


def test_reading_a_document_reads_it_once(alice, team):
    team.document("q3-report").append("Sales rose 4%.")
    assert alice.ok("doc_read", document="q3-report") == {
        "document": "q3-report",
        "parts": [{"text": "Sales rose 4%.", "by": "alice"}],
    }


# ---- the roster, when it cannot be joined ------------------------------------


# An agent that could not be put on the roster is told so where it looks for
# who it is and who is there, and it is tried again on each call.
def test_an_agent_that_could_not_join_the_roster_is_told_so(agents, team):
    team.client.counter("orvu/default/roster").increment(1)
    alice = agents("alice")
    answer = alice.ok("whoami")
    assert answer["team"] == "default" and answer["you"] == "alice"
    assert answer["on_roster"] is False
    assert "not on the team's roster" in answer["roster_note"] and "tried again" in answer["roster_note"]


# ---- the name "everyone" -----------------------------------------------------


@pytest.mark.parametrize("name", ["everyone", "Everyone", " EVERYONE ", "every one"])
def test_an_agent_cannot_be_called_everyone(sidecar, name):
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp", "--socket", str(sidecar.socket), "--me", name],
        capture_output=True,
        text=True,
        stdin=subprocess.DEVNULL,
    )
    if name == "every one":
        # Two words are a name like any other.
        assert done.returncode == 0, done.stderr
        return
    assert done.returncode == 1 and done.stdout == ""
    assert "everyone" in done.stderr and "all the agents" in done.stderr


# A key given to an agent called "everyone" is refused as soon as the server
# learns the name, which is when it first connects.
def test_a_key_for_an_agent_called_everyone_is_refused(sidecar):
    env = {**os.environ, "ORVU_KEY": sidecar.add_agent("Everyone")}
    agent = Agent("--socket", str(sidecar.socket), env=env)
    answer, failed = agent.call("whoami")
    assert failed and "'Everyone'" in answer and "all the agents" in answer
    answer, failed = agent.call("note_put", name="x", text="y")
    assert failed and "all the agents" in answer
    log = agent.stop()
    assert "all the agents" in log


# The tools read "everyone" in any case, and with spaces around it, as all.
def test_everyone_in_any_case_goes_to_everyone(agents):
    alice, bob = agents("alice"), agents("bob")
    assert alice.ok("inbox_send", to=" Everyone ", text="Deploy is on Friday.") == {"sent_to": "everyone"}
    assert [m["text"] for m in bob.ok("inbox_read")["messages"]] == ["Deploy is on Friday."]


# ---- offering some of the tools ----------------------------------------------


def test_an_agent_can_be_given_some_of_the_tools(agents):
    alice = agents("alice", "--tools", "notes,tasks")
    names = [tool["name"] for tool in alice.ask("tools/list")["result"]["tools"]]
    assert names[0] == "whoami"
    assert {name.partition("_")[0] for name in names[1:]} == {"note", "task"}
    assert "task_move" in names and "budget_spend" not in names
    hello = alice.ask("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}})["result"]
    assert "task_claim" in hello["instructions"]
    assert "budget_spend" not in hello["instructions"] and "inbox_read" not in hello["instructions"]
    answer = alice.ask("tools/call", {"name": "budget_spend", "arguments": {"budget": "x", "amount": 1}})
    assert "there is no tool 'budget_spend'" in answer["error"]["message"]
    assert "alice" in alice.stop()


def test_budgets_alone_still_leave_out_the_top_up_unless_it_is_allowed(agents):
    names = lambda agent: [tool["name"] for tool in agent.ask("tools/list")["result"]["tools"]]  # noqa: E731
    assert names(agents("alice", "--tools", "budgets")) == [
        "whoami", "budget_list", "budget_left", "budget_spend", "budget_spent"
    ]
    assert "budget_top_up" in names(agents("bob", "--tools", " budgets , search", "--allow-top-up"))


def test_a_tool_group_that_does_not_exist_stops_the_command(sidecar):
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp", "--socket", str(sidecar.socket), "--tools", "notes,mail"],
        capture_output=True,
        text=True,
        stdin=subprocess.DEVNULL,
    )
    assert done.returncode == 1 and done.stdout == ""
    assert "'mail'" in done.stderr and "notes, tasks, budgets" in done.stderr


# ---- reading a little at a time ----------------------------------------------


def test_notes_are_read_a_page_at_a_time(alice, team):
    for number in range(7):
        team.notes.put(f"n{number}", f"text {number}")
    assert alice.ok("note_list") == {"notes": {f"n{number}": f"text {number}" for number in range(7)}}
    first = alice.ok("note_list", limit=3)
    assert first["notes"] == {"n0": "text 0", "n1": "text 1", "n2": "text 2"}
    assert first["next_after"] == "n2" and "after" in first["more"] and "4 more" in first["more"]
    second = alice.ok("note_list", limit=3, after="n2")
    assert list(second["notes"]) == ["n3", "n4", "n5"]
    last = alice.ok("note_list", limit=3, after="n5")
    assert last == {"notes": {"n6": "text 6"}}
    assert alice.ok("note_list", names_only=True) == {"names": [f"n{number}" for number in range(7)]}
    answer, failed = alice.call("note_list", limit=0)
    assert failed and "limit" in answer


def test_tasks_are_read_a_page_at_a_time(alice, team):
    made = [team.tasks.add(f"Job {number}", notes="x" * 50) for number in range(5)]
    assert len(alice.ok("task_list")["tasks"]) == 5
    first = alice.ok("task_list", limit=2)
    assert [task["title"] for task in first["tasks"]] == ["Job 0", "Job 1"]
    assert first["next_after"] == made[1].id and "3 more" in first["more"]
    second = alice.ok("task_list", limit=2, after=made[1].id)
    assert [task["title"] for task in second["tasks"]] == ["Job 2", "Job 3"]
    short = alice.ok("task_list", names_only=True, state="todo")
    assert short["tasks"][0] == {"id": made[0].id, "title": "Job 0", "state": "todo"}
    answer, failed = alice.call("task_list", after="t-nothing")
    assert failed and "t-nothing" in answer


# However many there are, one answer stays a size a model can read.
def test_one_answer_is_never_too_large(alice, team):
    for number in range(300):
        team.notes.put(f"n{number:03}", "x" * 2000)
    answer = alice.ok("note_list")
    assert 0 < len(answer["notes"]) < 100
    assert len(json.dumps(answer)) < 60_000
    assert answer["next_after"] == max(answer["notes"])


# ---- more of what the library has --------------------------------------------


def test_a_task_is_moved_under_another(alice):
    report = alice.ok("task_add", title="Report")["task"]
    draft = alice.ok("task_add", title="Draft")["task"]
    assert alice.ok("task_move", task=draft["id"], under=report["id"])["moved"] is True
    assert [one["title"] for one in alice.ok("task_get", task=report["id"])["sub_tasks"]] == ["Draft"]
    answer = alice.ok("task_move", task=report["id"], under=draft["id"])
    assert answer["moved"] is False and "its own sub-tasks" in answer["why"]
    assert alice.ok("task_move", task=draft["id"])["moved"] is True
    assert alice.ok("task_get", task=report["id"])["sub_tasks"] == []


def test_text_is_put_into_and_taken_out_of_a_document(alice, team):
    alice.ok("doc_append", document="plan", text="Ship on Friday.")
    assert alice.ok("doc_insert", document="plan", text=" the app", after="Ship")["document"] == "plan"
    assert team.document("plan").text() == "Ship the app on Friday."
    alice.ok("doc_insert", document="plan", text="Plan: ", at=0)
    assert team.document("plan").text() == "Plan: Ship the app on Friday."
    alice.ok("doc_remove", document="plan", text=" on Friday")
    assert team.document("plan").text() == "Plan: Ship the app."
    alice.ok("doc_remove", document="plan", at=0, length=6)
    assert team.document("plan").text() == "Ship the app."
    for arguments, says in [
        ({"text": "x", "after": "nowhere"}, "does not hold"),
        ({"text": "x"}, "where"),
        ({"text": "x", "at": 0, "after": "Ship"}, "not both"),
    ]:
        answer, failed = alice.call("doc_insert", document="plan", **arguments)
        assert failed and says in answer, answer
    alice.ok("doc_append", document="plan", text=" Ship it.")
    answer, failed = alice.call("doc_remove", document="plan", text="Ship")
    assert failed and "2 times" in answer
    answer, failed = alice.call("doc_remove", document="missing", text="x")
    assert failed and "no document 'missing'" in answer
    assert team.document("plan").text() == "Ship the app. Ship it."


# A piece of text that is in a document twice, the second time starting
# inside the first, is in it twice: the tool cannot tell which is meant.
def test_text_that_overlaps_itself_is_found_twice(alice, team):
    alice.ok("doc_append", document="steps", text="ababa")
    answer, failed = alice.call("doc_remove", document="steps", text="aba")
    assert failed and "2 times" in answer, answer
    answer, failed = alice.call("doc_insert", document="steps", text="!", after="aba")
    assert failed and "2 times" in answer, answer
    assert team.document("steps").text() == "ababa"


def test_who_wrote_a_note(agents):
    alice, bob = agents("alice"), agents("bob")
    alice.ok("note_put", name="deploy-date", text="Friday")
    assert bob.ok("note_author", name="deploy-date") == {"name": "deploy-date", "found": True, "by": "alice"}
    assert bob.ok("note_author", name="nothing") == {"name": "nothing", "found": False}


def test_what_each_agent_spent(agents, team):
    team.budget("tokens").top_up(100)
    alice, bob = agents("alice"), agents("bob")
    alice.ok("budget_spend", budget="tokens", amount=10)
    bob.ok("budget_spend", budget="tokens", amount=5)
    alice.ok("budget_spend", budget="tokens", amount=1)
    assert bob.ok("budget_spent", budget="tokens") == {"budget": "tokens", "spent_by": {"alice": 11, "bob": 5}}
    answer, failed = bob.call("budget_spent", budget="token")
    assert failed and "there is no budget 'token'" in answer


# ---- the newer way of connecting ---------------------------------------------

MODERN = "2026-07-28"


def envelope(version=MODERN, **more):
    return {
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientCapabilities": {},
            "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
        },
        **more,
    }


# A newer client asks the server what it speaks, then sends each request with
# its version and what it can do, and never says hello.
def test_a_newer_client_connects_without_a_hello(alice):
    found = alice.ask("server/discover", envelope())["result"]
    assert found["supportedVersions"] == [MODERN]
    # The newer way has no subscribing to a resource as the older one does.
    assert found["capabilities"] == {"tools": {}, "resources": {}, "prompts": {}}
    assert found["resultType"] == "complete"
    assert found["ttlMs"] == 0 and found["cacheScope"] == "private"
    assert "'alice'" in found["instructions"]
    assert found["_meta"]["io.modelcontextprotocol/serverInfo"] == {"name": "orvu", "version": orvu.__version__}
    listed = alice.ask("tools/list", envelope())["result"]
    assert listed["resultType"] == "complete" and "ttlMs" in listed and "cacheScope" in listed
    assert "whoami" in [tool["name"] for tool in listed["tools"]]
    called = alice.ask("tools/call", envelope(name="whoami", arguments={}))["result"]
    assert called["resultType"] == "complete" and called["isError"] is False
    assert json.loads(called["content"][0]["text"])["you"] == "alice"


def test_a_newer_client_asking_for_a_version_this_does_not_speak_is_told_which_it_does(alice):
    refused = alice.ask("server/discover", envelope("2099-01-01"))["error"]
    assert refused["code"] == -32022
    assert refused["data"] == {"supported": [MODERN], "requested": "2099-01-01"}
    refused = alice.ask("tools/list", envelope("2099-01-01"))["error"]
    assert refused["code"] == -32022


def test_a_newer_request_that_leaves_out_what_it_must_say_is_refused(alice):
    assert alice.ask("server/discover")["error"]["code"] == -32602
    half = {"_meta": {"io.modelcontextprotocol/protocolVersion": MODERN}}
    refused = alice.ask("tools/list", half)["error"]
    assert refused["code"] == -32602 and "clientCapabilities" in refused["message"]


# An older client on the same server sees nothing of the newer way.
def test_the_older_hello_is_answered_as_before_next_to_the_newer_way(alice):
    hello = alice.ask("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}})["result"]
    assert hello["protocolVersion"] == "2025-11-25" and "resultType" not in hello
    listed = alice.ask("tools/list")["result"]
    assert "resultType" not in listed and "ttlMs" not in listed


# ---- structured notes, plain objects, and JSON in messages and results -------


def test_agents_fill_in_a_structured_note(agents, team):
    alice, bob = agents("alice"), agents("bob")
    assert alice.ok("note_structured_list") == {"names": []}
    assert alice.ok("note_structured_get", name="plan") == {"name": "plan", "found": False}
    assert alice.ok("note_structured_set", name="plan", path=["owner"], value="alice") == {"name": "plan", "set": ["owner"]}
    bob.ok("note_structured_set", name="plan", path=["steps"], value=["draft", "check"])
    assert alice.ok("note_structured_get", name="plan") == {
        "name": "plan",
        "found": True,
        "value": {"owner": "alice", "steps": ["draft", "check"]},
    }
    assert alice.ok("note_structured_get", name="plan", path=["steps", 1])["value"] == "check"
    assert team.notes.structured("plan").get(["owner"]) == "alice"
    assert bob.ok("note_structured_list") == {"names": ["plan"]}
    assert alice.ok("note_structured_remove", name="plan", path=["owner"]) == {"name": "plan", "removed": ["owner"]}
    assert team.notes.structured("plan").to_value() == {"steps": ["draft", "check"]}
    wrong, failed = alice.call("note_structured_set", name="plan", path="owner", value=1)
    assert failed and "'path' has to be a list" in wrong
    wrong, failed = alice.call("note_structured_set", name="plan", path=[True], value=1)
    assert failed and "'path' has to be a list" in wrong


def test_an_agent_reads_and_writes_plain_objects(alice, team):
    assert alice.ok("object_counter_add", name="visits", amount=3) == {"name": "visits", "value": 3}
    assert alice.ok("object_counter_add", name="visits", amount=-1) == {"name": "visits", "value": 2}
    assert team.client.counter("visits").value() == 2
    assert alice.ok("object_read", kind="counter", name="visits") == {"kind": "counter", "name": "visits", "value": 2}

    alice.ok("object_register_set", name="status", value="green")
    assert team.client.register("status").get() == "green"

    alice.ok("object_set_add", name="tags", member="urgent")
    alice.ok("object_set_add", name="tags", member="billing")
    alice.ok("object_set_remove", name="tags", member="urgent")
    assert alice.ok("object_read", kind="set", name="tags")["value"] == ["billing"]

    alice.ok("object_map_put", name="owners", key="api", value="alice")
    alice.ok("object_map_put", name="owners", key="web", value="bob")
    alice.ok("object_map_remove", name="owners", key="web")
    assert alice.ok("object_read", kind="map", name="owners")["value"] == {"api": "alice"}

    alice.ok("object_list_insert", name="queue", value="b")
    alice.ok("object_list_insert", name="queue", value="a", at=0)
    alice.ok("object_list_insert", name="queue", value="c")
    alice.ok("object_list_remove", name="queue", at=1)
    assert team.client.list("queue").to_list() == ["a", "c"]
    wrong, failed = alice.call("object_list_remove", name="queue", at=7)
    assert failed and "OutOfBounds" in wrong

    alice.ok("object_json_set", name="config", path=["retries"], value=3)
    alice.ok("object_json_set", name="config", path=["hosts"], value=["a", "b"])
    alice.ok("object_json_remove", name="config", path=["retries"])
    assert alice.ok("object_read", kind="json", name="config")["value"] == {"hosts": ["a", "b"]}

    # What a program wrote is read the same way.
    team.client.cart("stock").add("apples", 4)
    assert alice.ok("object_read", kind="cart", name="stock")["value"] == {"apples": 4}
    assert alice.ok("object_read", kind="register", name="status")["value"] == "green"
    assert alice.ok("object_read", kind="list", name="queue")["value"] == ["a", "c"]

    # The team's own objects are reached through their tools, not as plain
    # objects.
    wrong, failed = alice.call("object_counter_add", name="orvu/default/roster", amount=1)
    assert failed and "orvu/" in wrong
    wrong, failed = alice.call("object_read", kind="map", name="orvu/default/notes-mv")
    assert failed and "orvu/" in wrong


# What the sidecar's rules do not allow comes back as a failed tool that says
# so, and nothing is changed.
def test_a_write_the_rules_do_not_allow_is_refused_with_a_reason(sidecar):
    key = sidecar.add_agent("reader")
    sidecar.allow("reader", "notes:read", "objects:read")
    reader = Agent("--socket", str(sidecar.socket), env={**os.environ, "ORVU_KEY": key})
    try:
        wrong, failed = reader.call("note_structured_set", name="plan", path=["owner"], value="reader")
        assert failed and wrong.startswith("NotAllowed")
        wrong, failed = reader.call("object_counter_add", name="visits", amount=1)
        assert failed and wrong.startswith("NotAllowed")
        assert reader.ok("object_read", kind="counter", name="visits")["value"] == 0
        assert reader.ok("note_structured_list") == {"names": []}
    finally:
        reader.stop()


def test_a_message_and_a_result_can_carry_json(agents, team):
    alice, bob = agents("alice"), agents("bob")
    bob.ok("whoami")
    assert alice.ok("inbox_send", to="bob", data={"task": "t-1", "rows": [1, 2]}) == {"sent_to": "bob"}
    (letter,) = bob.ok("inbox_read")["messages"]
    assert letter["text"] == {"task": "t-1", "rows": [1, 2]}
    wrong, failed = alice.call("inbox_send", to="bob", text="hi", data=[1])
    assert failed and "not both" in wrong
    wrong, failed = alice.call("inbox_send", to="bob")
    assert failed and "'text' or 'data'" in wrong

    task = alice.ok("task_add", title="Count the rows")["task"]
    assert alice.ok("task_finish", task=task["id"], data={"rows": 3})["finished"] is True
    assert team.tasks.get(task["id"]).result == {"rows": 3}
    assert alice.ok("task_get", task=task["id"])["task"]["result"] == {"rows": 3}
    other = alice.ok("task_add", title="Two answers")["task"]
    wrong, failed = alice.call("task_finish", task=other["id"], result="done", data=1)
    assert failed and "not both" in wrong


# An answer that is a JSON object is also given as MCP's structured content,
# for programs that read that and not the text.
def test_an_answer_is_given_as_structured_content_too(alice):
    result = alice.ask("tools/call", {"name": "whoami", "arguments": {}})["result"]
    assert result["structuredContent"] == {"team": "default", "you": "alice"}
    assert json.loads(result["content"][0]["text"]) == result["structuredContent"]
    failed = alice.ask("tools/call", {"name": "task_get", "arguments": {"task": "t-0"}})["result"]
    assert failed["isError"] is True and "structuredContent" not in failed


# ---- resources and prompts ---------------------------------------------------


def resource(agent, uri):
    """A resource as the agent reads it, as JSON."""
    (content,) = agent.ask("resources/read", {"uri": uri})["result"]["contents"]
    assert content["uri"] == uri and content["mimeType"] == "application/json"
    return json.loads(content["text"])


def test_the_inbox_tasks_and_notes_can_be_read_as_resources(agents, team):
    alice, bob = agents("alice"), agents("bob")
    listed = alice.ask("resources/list")["result"]["resources"]
    assert [found["uri"] for found in listed] == ["orvu://inbox", "orvu://tasks", "orvu://notes"]
    assert all(found["mimeType"] == "application/json" and found["description"] for found in listed)

    team.notes.put("deploy-date", "Friday")
    assert resource(alice, "orvu://notes") == {"notes": {"deploy-date": "Friday"}}
    task = team.tasks.add("Draft")
    assert [found["id"] for found in resource(alice, "orvu://tasks")["tasks"]] == [task.id]
    bob.ok("inbox_send", to="alice", text="hello")
    assert [m["text"] for m in resource(alice, "orvu://inbox")["messages"]] == ["hello"]
    # Reading the inbox as a resource does not count as being given it.
    assert [m["text"] for m in alice.ok("inbox_read")["messages"]] == ["hello"]

    refused = alice.ask("resources/read", {"uri": "orvu://budgets"})["error"]
    assert refused["code"] == -32002 and "orvu://budgets" in refused["message"]
    assert alice.ask("resources/templates/list")["result"] == {"resourceTemplates": []}


def test_only_the_resources_of_the_tools_offered_are_there(agents):
    alice = agents("alice", "--tools", "notes")
    assert [found["uri"] for found in alice.ask("resources/list")["result"]["resources"]] == ["orvu://notes"]
    assert alice.ask("resources/read", {"uri": "orvu://tasks"})["error"]["code"] == -32002
    assert alice.ask("resources/subscribe", {"uri": "orvu://tasks"})["error"]["code"] == -32002


# An agent that subscribes is told when what it subscribed to changes, and
# no more once it unsubscribes.
def test_a_subscribed_agent_is_told_of_each_change(agents, team, sidecar):
    alice = agents("alice")
    alice.ok("whoami")
    assert alice.ask("resources/subscribe", {"uri": "orvu://notes"})["result"] == {}
    assert alice.ask("resources/subscribe", {"uri": "orvu://inbox"})["result"] == {}
    assert alice.notified(1) is None, "told of a change before anything changed"

    team.notes.put("deploy-date", "Friday")
    told = alice.notified()
    assert told == {"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": "orvu://notes"}}

    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.send("alice", "the numbers are in")
    assert alice.notified()["params"] == {"uri": "orvu://inbox"}
    # A task was not subscribed to.
    team.tasks.add("Draft")
    assert alice.notified(1) is None

    assert alice.ask("resources/unsubscribe", {"uri": "orvu://notes"})["result"] == {}
    team.notes.put("deploy-date", "Monday")
    assert alice.notified(1) is None
    assert "alice" in alice.stop()


def test_a_task_change_is_told_to_an_agent_that_subscribed_to_tasks(agents, team):
    alice = agents("alice")
    alice.ask("resources/subscribe", {"uri": "orvu://tasks"})
    task = team.tasks.add("Draft")
    assert alice.notified()["params"] == {"uri": "orvu://tasks"}
    team.tasks.claim(task)
    assert alice.notified()["params"] == {"uri": "orvu://tasks"}


def test_subscribing_to_something_that_is_not_there_is_refused(alice):
    assert alice.ask("resources/subscribe", {"uri": "orvu://nothing"})["error"]["code"] == -32002
    assert alice.ask("resources/subscribe", {})["error"]["code"] == -32602
    # Unsubscribing from what was never subscribed to is no mistake.
    assert alice.ask("resources/unsubscribe", {"uri": "orvu://notes"})["result"] == {}


# Subscribing is part of the older way only. The newer way of MCP follows
# changes some other way, which this server does not offer.
def test_a_newer_client_reads_resources_but_cannot_subscribe(alice):
    listed = alice.ask("resources/list", envelope())["result"]
    assert listed["resultType"] == "complete" and len(listed["resources"]) == 3
    assert alice.ask("resources/subscribe", envelope(uri="orvu://notes"))["error"]["code"] == -32601


def test_the_prompts_ask_the_agent_to_look_at_its_inbox_and_the_tasks(agents):
    alice = agents("alice")
    prompts = alice.ask("prompts/list")["result"]["prompts"]
    assert [prompt["name"] for prompt in prompts] == ["check_inbox", "pick_a_task"]
    assert all(prompt["description"] for prompt in prompts)
    got = alice.ask("prompts/get", {"name": "check_inbox"})["result"]
    (message,) = got["messages"]
    assert message["role"] == "user" and "inbox_read" in message["content"]["text"]
    assert alice.ask("prompts/get", {"name": "write_a_poem"})["error"]["code"] == -32602
    only_notes = agents("bob", "--tools", "notes")
    assert only_notes.ask("prompts/list")["result"] == {"prompts": []}


# A server whose program goes away while it follows changes ends, and lets go
# of what it followed with.
def test_a_server_that_follows_changes_ends_when_its_input_is_closed(agents):
    alice = agents("alice")
    for uri in ("orvu://inbox", "orvu://tasks", "orvu://notes"):
        assert alice.ask("resources/subscribe", {"uri": uri})["result"] == {}
    started = time.monotonic()
    assert "alice" in alice.stop()
    assert time.monotonic() - started < 5
