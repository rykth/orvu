"""The story the demo tells, one step at a time.

Four agents work on one team spread over three machines: alice (Python) on
m1, bob (TypeScript) and dave (Python) on m2, and carol (TypeScript) on m3.
A fifth, scout, is the MCP server on m1. The story tells each agent what to
do next, and prints what it did and what it got back. Every step checks its
outcome: if anything is not as it should be, the story says what, and ends
with a failure.

The story also reads each machine's copy of the team itself, as an agent
called referee, to show that the three copies end up the same.
"""

import json
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

import orvu
from orvu import Team

from mcp_client import Client, Refused

TEAM = "crew"
AGENTS = {
    "alice": ("python", "m1"),
    "bob": ("typescript", "m2"),
    "carol": ("typescript", "m3"),
    "dave": ("python", "m2"),
}
MACHINES = ("m1", "m2", "m3")


class Failed(Exception):
    """Something was not as it should be."""


# ---- saying what happens -----------------------------------------------------


def chapter(title):
    print(f"\n== {title} ==", flush=True)


def say(who, text):
    if who in AGENTS:
        language, machine = AGENTS[who]
        who = f"{who}/{language}@{machine}"
    print(f"[{who}] {text}", flush=True)


def check(condition, what):
    if not condition:
        raise Failed(what)
    print(f"    ok: {what}", flush=True)


def eventually(probe, what, within=20.0, every=0.3, ok=bool):
    """Asks `probe` again and again until what it gives is `ok`, and returns
    that. Changes take a moment to reach another machine."""
    deadline = time.monotonic() + within
    while True:
        seen = probe()
        if ok(seen):
            return seen
        if time.monotonic() > deadline:
            raise Failed(f"{what}: still not so after {within:.0f} seconds (last seen: {seen!r})")
        time.sleep(every)


# ---- talking to the agents ---------------------------------------------------


def ask(agent, step, **given):
    """Tells an agent to take one step. Returns its whole answer: `ok`, and
    `result`, or `error` and `message`."""
    body = json.dumps({"do": step, "with": given}).encode()
    request = urllib.request.Request(f"http://{agent}:8000/", body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=90) as answer:
        return json.loads(answer.read())


def do(agent, step, **given):
    """Like `ask`, for a step that has to work. Returns its result."""
    answer = ask(agent, step, **given)
    if not answer["ok"]:
        raise Failed(f"{agent} could not {step}: {answer['error']}: {answer['message']}")
    return answer["result"]


def at_once(*steps):
    """Takes several steps side by side, and waits for them all."""
    failed = []

    def run(step):
        try:
            step()
        except Exception as error:  # noqa: BLE001
            failed.append(error)

    runners = [threading.Thread(target=run, args=(step,)) for step in steps]
    for runner in runners:
        runner.start()
    for runner in runners:
        runner.join()
    if failed:
        raise failed[0]


def ready(agent):
    try:
        return do(agent, "hello")
    except (urllib.error.URLError, ConnectionError, OSError):
        return None


def key(name):
    with open(f"/keys/{name}.key") as file:
        return file.read().strip()


def shown(value):
    if isinstance(value, orvu.Clash):
        return {"clash": sorted(map(str, value.values))}
    raise TypeError(type(value).__name__)


def copy_on(machine):
    """What one machine holds of the team, read by the referee there, or
    `None` if the machine does not answer."""
    try:
        with Team.connect(TEAM, socket=f"/sock/{machine}/orvud.sock", key=key(f"referee-{machine}")) as team:
            report = team.document("release-plan")
            votes = team.tally("votes")
            return {
                "notes": json.loads(json.dumps(team.notes.all(), default=shown)),
                "tasks": sorted(
                    (task.id, task.title, task.state, task.claimed_by, task.finished_by, task.ticket)
                    for task in team.tasks.list()
                ),
                "tokens left": team.budget("tokens").left(),
                "votes": votes.by_name(),
                "release-plan": report.written_by(),
            }
    except orvu.OrvuError:
        return None


def all_agree(what):
    """Waits until the three machines hold the same, and says so."""

    def same():
        copies = [copy_on(machine) for machine in MACHINES]
        return copies[0] if copies[0] is not None and all(copy == copies[0] for copy in copies) else None

    held = eventually(same, what, within=60, every=1.0)
    for machine in MACHINES:
        say(f"referee@{machine}", f"holds {len(held['notes'])} notes, {len(held['tasks'])} tasks, "
            f"{held['tokens left']} tokens left, {sum(held['votes'].values())} votes counted")
    check(True, what)


# ---- the story ---------------------------------------------------------------


def start():
    chapter("The team gathers")
    for agent in AGENTS:
        hello = eventually(lambda: ready(agent), f"{agent} is ready", within=180, every=1.0)
        check(hello["me"] == agent, f"{agent} connected with its own key, and the sidecar knows it as {agent}")
        say(agent, f"is here, on machine {hello['machine']}, connected over the local socket")


def notes():
    chapter("1. Two machines write one note at the same moment")
    for attempt in range(1, 6):
        name = "deploy-day" if attempt == 1 else f"deploy-day-{attempt}"
        at = int(time.time() * 1000) + 1000
        at_once(
            lambda: do("alice", "note_put", name=name, value="Friday", at=at),
            lambda: do("bob", "note_put", name=name, value="Monday", at=at),
        )
        say("alice", f'writes the note {name} = "Friday" on m1')
        say("bob", f'writes the note {name} = "Monday" on m2, at the same moment')
        try:
            seen = eventually(
                lambda: do("carol", "note_get", name=name),
                "the clash reaches m3",
                ok=lambda got: "clash" in got,
                within=5,
            )
            break
        except Failed:
            # One write reached the other machine before the second was
            # made, so the second simply replaced it. That is not a clash.
            print("    (one machine heard of the other's write first, so there is no clash: they try again)")
    else:
        raise Failed("five tries in a row, and the two writes never met")

    clash = seen["clash"]
    say("carol", f"reads {name} on m3: a clash, {list(zip(clash['values'], clash['authors']))}")
    check(sorted(clash["values"]) == ["Friday", "Monday"], "both values are kept")
    check(sorted(clash["authors"]) == ["alice", "bob"], "each with the agent that wrote it")
    for agent in ("alice", "dave"):
        eventually(lambda: "clash" in do(agent, "note_get", name=name), f"{agent} sees the clash too")
    check(True, "alice on m1 and dave on m2 see the same clash")

    do("carol", "note_put", name=name, value="Friday")
    say("carol", f'settles it: {name} = "Friday"')
    for agent in ("alice", "bob", "dave"):
        settled = eventually(
            lambda: do(agent, "note_get", name=name),
            f"{agent} sees it settled",
            ok=lambda got: got.get("value") == "Friday",
        )
        check(settled["author"] == "carol", f"{agent} reads Friday, written by carol")
    return name


def tasks():
    chapter("2. Tasks: added, claimed, finished")
    write = do("alice", "task_add", title="Write the release notes")
    count = do("alice", "task_add", title="Check the numbers")
    say("alice", f"adds two tasks: {write['title']!r} and {count['title']!r}")
    for agent in ("bob", "dave"):
        for task in (write, count):
            eventually(lambda: do(agent, "task_get", id=task["id"]), f"{agent} sees {task['title']!r}")
    check(do("bob", "task_claim", id=write["id"]) is True, "bob claims 'Write the release notes'")
    say("bob", "claims 'Write the release notes'")
    check(do("dave", "task_claim", id=write["id"]) is False, "dave is told no: bob has it")
    say("dave", "tries to claim it too, and is told no")
    check(do("dave", "task_claim", id=count["id"]) is True, "dave claims 'Check the numbers'")
    check(do("dave", "task_finish", id=count["id"], result="The numbers add up.") is True, "dave finishes it")
    say("dave", "claims 'Check the numbers' and finishes it")
    check(do("bob", "task_finish", id=write["id"], result="Drafted.") is True, "bob finishes his")
    say("bob", "finishes 'Write the release notes'")

    def board():
        tasks = {task["title"]: task for task in do("carol", "task_list")}
        done = [tasks.get(title, {}).get("state") for title in ("Write the release notes", "Check the numbers")]
        return tasks if done == ["done", "done"] else None

    seen = eventually(board, "both tasks show as done on m3")
    say("carol", "reads the board on m3: both done, "
        f"by {seen['Write the release notes']['finished_by']} and {seen['Check the numbers']['finished_by']}")
    check(seen["Write the release notes"]["finished_by"] == "bob", "m3 shows bob finished his")
    check(seen["Check the numbers"]["finished_by"] == "dave", "m3 shows dave finished his")


def firm_task():
    chapter("3. A firm task: one holder, a ticket, a takeover")
    task = do("alice", "task_add", title="Send the invoice", firm=True)
    say("alice", "adds a firm task, 'Send the invoice': only one agent at a time can hold it")
    eventually(lambda: do("bob", "task_get", id=task["id"]), "bob sees the firm task")

    # The first claim on another machine may wait a moment for the ticket.
    first = eventually(
        lambda: do("bob", "task_claim_ticket", id=task["id"], lease=4),
        "bob gets the ticket",
        ok=lambda got: got is not None,
    )
    say("bob", f"claims it with ticket {first}, for 4 seconds at a time, and then goes quiet")

    eventually(
        lambda: (do("carol", "task_get", id=task["id"]) or {}).get("claimed_by") == "bob",
        "carol sees bob on the task",
    )
    say("carol", "sees that bob has it, and starts counting how long he has been quiet")
    check(do("carol", "task_claim_ticket", id=task["id"]) is None, "carol is told no while bob's claim is fresh")
    second = eventually(
        lambda: do("carol", "task_claim_ticket", id=task["id"], lease=60),
        "carol takes the task over once bob's claim has run out",
        within=30,
        every=0.5,
        ok=lambda got: got is not None,
    )
    say("carol", f"bob has been quiet for longer than his claim: she takes it over, with ticket {second}")
    check(second > first, f"carol's ticket {second} is higher than bob's {first}")
    taken = do("carol", "task_get", id=task["id"])["taken_over"]
    check(taken["by"] == "carol" and taken["from"] == "bob", "the task says carol took it over from bob")

    eventually(
        lambda: do("bob", "task_still_holding", id=task["id"], ticket=first) is False,
        "bob's machine hears of the takeover",
    )
    say("bob", f"wakes up and asks: do I still hold ticket {first}? No")
    check(True, "still_holding is false for the old holder")
    check(do("bob", "task_still_working", id=task["id"]) is False, "bob's still_working says the task is not his")

    eventually(
        lambda: (do("alice", "task_get", id=task["id"]) or {}).get("claimed_by") == "carol",
        "alice sees the takeover",
    )
    back = do("alice", "task_set_back", id=task["id"], force=True)
    say("alice", f"knows carol's work was dropped, and sets the task back by force: ticket {back['ticket']}")
    check(back["state"] == "todo" and back["claimed_by"] is None, "the task is free again")
    check(back["ticket"] > second, "on a ticket above every one it had")
    eventually(
        lambda: do("carol", "task_still_holding", id=task["id"], ticket=second) is False,
        "carol's machine hears it was set back",
    )
    check(True, "carol no longer holds her ticket either")


def budget():
    chapter("4. A budget: topped up, spent, never overspent")
    do("alice", "budget_top_up", name="tokens", n=1000)
    say("alice", "tops up the budget 'tokens' by 1000 on m1")
    do("bob", "budget_top_up", name="tokens", n=500)
    say("bob", "tops it up by 500 on m2")
    # m3's share of the budget reaches it within a second or so.
    eventually(lambda: do("carol", "budget_spend", name="tokens", n=300), "carol can spend 300 on m3")
    say("carol", "spends 300 on m3")
    check(do("dave", "budget_spend", name="tokens", n=5000) is False, "dave asks for 5000 and is told no")
    say("dave", "asks for 5000: not enough, nothing is spent")
    read = eventually(
        lambda: do("alice", "budget_read", name="tokens"),
        "alice sees 1200 left",
        ok=lambda got: got["left"] == 1200 and got["spent_by"],
    )
    say("alice", f"reads it: {read['left']} left, topped up by {read['topped_up_by']}, spent by {read['spent_by']}")
    check(read["topped_up_by"] == {"alice": 1000, "bob": 500}, "who topped it up, and by how much, is recorded")
    check(read["spent_by"] == {"carol": 300}, "who spent from it is recorded")


def inbox():
    chapter("5. Messages between Python and TypeScript")
    got = {}

    def arrived(agent, sender):
        for message in do(agent, "inbox_unread"):
            got.setdefault(agent, []).append(message)
        return next((message for message in got.get(agent, []) if message["sender"] == sender), None)

    do("alice", "inbox_send", to="carol", body={"say": "The invoice is free again, if you want it.", "task": "invoice"})
    say("alice", "sends carol a message, from Python on m1")
    message = eventually(lambda: arrived("carol", "alice"), "carol gets alice's message")
    say("carol", f"reads it in TypeScript on m3: {message['body']}")
    check(message["body"]["task"] == "invoice" and message["to"] == "carol", "it arrives as sent, to carol")

    do("carol", "inbox_send", to="alice", body="Thanks, I will take it after lunch.")
    message = eventually(lambda: arrived("alice", "carol"), "alice gets carol's answer")
    say("alice", f"gets carol's answer: {message['body']!r}")
    check(message["body"] == "Thanks, I will take it after lunch.", "the answer arrives")

    do("bob", "inbox_send", body="Stand-up in five minutes.")
    say("bob", "sends a message to everyone")
    message = eventually(lambda: arrived("dave", "bob"), "dave gets bob's message to everyone")
    say("dave", f"gets it: {message['body']!r}")
    check(message["to"] is None, "it was sent to everyone")


def document():
    chapter("6. A document written by two agents")
    do("alice", "doc_append", name="release-plan", text="Ship on Friday. ")
    say("alice", "writes in the document 'release-plan': \"Ship on Friday. \"")
    eventually(lambda: "Friday" in do("bob", "doc_read", name="release-plan")["text"], "bob sees alice's words")
    do("bob", "doc_append", name="release-plan", text="Bob drafted the notes.")
    say("bob", "adds: \"Bob drafted the notes.\"")
    want = [["Ship on Friday. ", "alice"], ["Bob drafted the notes.", "bob"]]
    read = eventually(
        lambda: do("carol", "doc_read", name="release-plan"),
        "carol sees who wrote which part",
        ok=lambda got: got["written_by"] == want,
    )
    say("carol", f"reads it with the marks of who wrote what: {read['written_by']}")
    check(True, "each part is marked with its author")


def tally():
    chapter("7. A tally")
    for agent, n in (("alice", 3), ("bob", 2), ("carol", 1)):
        do(agent, "tally_add", name="votes", n=n)
        say(agent, f"counts {n} vote{'s' if n > 1 else ''}")
    read = eventually(
        lambda: do("dave", "tally_read", name="votes"),
        "dave sees 6 votes in all",
        ok=lambda got: got["total"] == 6,
    )
    say("dave", f"reads the tally: {read['total']} in all, {read['by_name']}")
    check(read["by_name"] == {"alice": 3, "bob": 2, "carol": 1}, "each agent's count is kept")


def roster():
    chapter("8. Who is here")
    here = eventually(
        lambda: do("dave", "roster_here"),
        "every agent is on the roster",
        ok=lambda got: set(AGENTS) <= set(got),
    )
    say("dave", f"looks at the roster: {sorted(here)}")
    check(True, "all four agents are here, from all three machines")


def search(note):
    chapter("9. Search by meaning")
    question = "which day do we deploy, and who writes the release notes?"

    def found():
        hits = do("bob", "search", query=question, limit=10)
        kinds = {(hit["kind"], hit["title"] or hit["name"]) for hit in hits}
        return hits if ("note", note) in kinds and ("task", "Write the release notes") in kinds else None

    hits = eventually(found, "the search finds the note and the task", within=60, every=1.0)
    say("bob", f"asks {question!r}")
    for hit in hits[:4]:
        print(f"      {hit['kind']:<9} {hit['text'][:60]!r}  score {hit['score']:.2f}")
    check(True, f"it finds the note {note!r} and the task 'Write the release notes'")


def watch():
    chapter("10. Watching for changes")
    do("carol", "watch_note", name="status", until="green")
    say("carol", "watches the team's notes, in TypeScript on m3, for the note 'status'")
    eventually(lambda: do("carol", "watch_seen", what="note/status"), "carol's watch has started")
    do("alice", "note_put", name="status", value="green")
    say("alice", "writes status = green on m1")
    seen = eventually(
        lambda: do("carol", "watch_seen", what="note/status"),
        "carol's watch sees the change",
        ok=lambda got: got and got[-1] == "green",
    )
    say("carol", f"her watch saw: {seen}")
    check(True, "a watch in TypeScript on m3 reacted to a write in Python on m1")

    do("alice", "watch_budget", name="tokens", until=1100)
    say("alice", "watches the budget 'tokens', in Python on m1")
    eventually(lambda: do("alice", "watch_seen", what="budget/tokens"), "alice's watch has started")
    eventually(lambda: do("bob", "budget_spend", name="tokens", n=100), "bob can spend 100")
    say("bob", "spends 100 on m2")
    seen = eventually(
        lambda: do("alice", "watch_seen", what="budget/tokens"),
        "alice's watch sees the spend",
        ok=lambda got: got and got[-1] == 1100,
    )
    say("alice", f"her watch saw: {seen}")
    check(True, "a watch in Python on m1 reacted to a spend in TypeScript on m2")


def machine_off_and_on():
    chapter("11. A machine is switched off, and on again")
    open("/ctl/m3.off", "w").close()
    eventually(lambda: machine_is("m3", "off"), "m3 stops")
    print("\n*** m3 is switched off ***\n", flush=True)
    answer = ask("carol", "note_get", name="status")
    check(not answer["ok"], f"carol cannot reach her machine: {answer.get('error')}")
    say("carol", "cannot reach the sidecar on m3")

    do("alice", "note_put", name="while-m3-was-off", value="m1 and m2 carried on")
    say("alice", "writes a note while m3 is off")
    task = do("alice", "task_add", title="Written while m3 was off")
    say("alice", "adds a task while m3 is off")
    do("bob", "tally_add", name="votes", n=4)
    say("bob", "counts 4 more votes while m3 is off")

    os.remove("/ctl/m3.off")
    eventually(lambda: machine_is("m3", "on"), "m3 starts again")
    print("\n*** m3 is switched on again ***\n", flush=True)
    # Her first calls may still find the sidecar starting.
    note = eventually(
        lambda: ask("carol", "note_get", name="while-m3-was-off").get("result", {}).get("value"),
        "carol sees what was written while m3 was off",
        within=60,
        every=1.0,
    )
    say("carol", f"is back, and reads the note written while she was away: {note!r}")
    eventually(lambda: do("carol", "task_get", id=task["id"]), "carol sees the task added while m3 was off")
    say("carol", f"sees the task {task['title']!r}")
    all_agree("m1, m2 and m3 hold the same notes, tasks, budget, tally and document")


def machine_is(machine, state):
    try:
        with open(f"/ctl/{machine}.state") as file:
            return file.read().strip() == state
    except FileNotFoundError:
        return False


def mcp(note):
    chapter("12. An agent program over MCP, with a token")
    url = "http://scout:8765/mcp"
    try:
        Client(url, token=None).hello()
        raise Failed("the MCP server let in a program with no token")
    except Refused as refused:
        check(refused.status == 401, "a program with no token is turned away (HTTP 401)")
    with open("/keys/scout.token") as file:
        client = Client(url, token=file.read().strip())
    server = client.hello()
    say("scout/mcp@m1", f"a scripted MCP client says hello to {server['serverInfo']['name']}")
    tools = client.tools()
    check({"note_get", "task_add", "inbox_send", "search"} <= set(tools), f"it is offered {len(tools)} tools")
    who = client.call("whoami")
    check(who == {"team": TEAM, "you": "scout"}, "whoami says it is scout, on the team crew")
    read = client.call("note_get", name=note)
    say("scout/mcp@m1", f"note_get {note} -> {read.get('text')!r}")
    check(read.get("text") == "Friday", "it reads the note the agents settled")
    added = client.call("task_add", title="Tidy the changelog")["task"]
    say("scout/mcp@m1", f"task_add 'Tidy the changelog' -> {added['id']}")
    seen = eventually(lambda: do("bob", "task_get", id=added["id"]), "bob sees scout's task on m2")
    say("bob", f"sees the task on m2, added by {seen['created_by']}")
    check(seen["created_by"] == "scout", "the task carries scout's name")


def rules(note):
    chapter("13. Rules: dave may change only his own notes")
    answer = ask("dave", "note_put", name=note, value="Sunday")
    check(not answer["ok"] and answer["error"] == "NotAllowed", "dave is not allowed to change carol's note")
    say("dave", f"tries to change {note!r}, which carol wrote: refused")
    print(f"      {answer['message']}")
    do("dave", "note_put", name="dave-was-here", value="hello")
    say("dave", "writes a note of his own: allowed")
    check(do("alice", "note_get", name=note)["value"] == "Friday", f"{note} still says Friday")

    config = "/team/crew/m2/orvu.toml"

    def recorded():
        found = subprocess.run(
            ["orvud", "audit", "--config", config, "--agent", "dave", "--not-allowed", "--json"],
            capture_output=True, text=True, check=True,
        ).stdout
        lines = [json.loads(line) for line in found.splitlines() if line.strip()]
        return [line for line in lines if line.get("key") == note] or None

    line = eventually(recorded, "the refusal is in m2's audit log")[-1]
    shown = subprocess.run(
        ["orvud", "audit", "--config", config, "--agent", "dave", "--not-allowed"],
        capture_output=True, text=True, check=True,
    ).stdout.strip().splitlines()
    say("referee@m2", "runs: orvud audit --config orvu.toml --agent dave --not-allowed")
    for text in shown:
        print(f"      {text}")
    check(line["agent"] == "dave" and line["outcome"].startswith("not allowed"), "m2's audit log records it as not allowed")


def main():
    print("Orvu on three machines: four agents, in Python and TypeScript, and one over MCP.", flush=True)
    try:
        start()
        note = notes()
        tasks()
        firm_task()
        budget()
        inbox()
        document()
        tally()
        roster()
        search(note)
        watch()
        machine_off_and_on()
        mcp(note)
        rules(note)
        chapter("The end")
        all_agree("the three machines still agree")
    except Failed as failed:
        print(f"\nFAILED: {failed}", flush=True)
        sys.exit(1)
    print("\nall checks passed", flush=True)


if __name__ == "__main__":
    main()
