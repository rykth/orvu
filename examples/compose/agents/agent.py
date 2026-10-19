"""One agent, written in Python, on one machine of the team.

It connects to its machine's sidecar over the local socket, with its own key,
and joins the roster. Then it waits for the story to tell it what to do next:
the story sends it one step at a time over HTTP, as {"do": ..., "with": {...}},
and the agent answers with what it got back. Every step is an ordinary call
of the `orvu` package, the same as a real agent would make.

    AGENT    the agent's name, which is also the name of its key file
    MACHINE  the machine it runs on, for the story to show
"""

import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import orvu
from orvu import Team

AGENT = os.environ["AGENT"]
MACHINE = os.environ["MACHINE"]
TEAM = "crew"
SOCKET = "/sock/orvud.sock"


def key():
    with open(f"/keys/{AGENT}.key") as file:
        return file.read().strip()


def connect():
    # The socket is given, so the package never starts a sidecar of its own.
    return Team.connect(TEAM, socket=SOCKET, key=key())


team = connect()
team.roster.join(every=5)
# One step at a time on the agent's connection. Watching runs on a
# connection of its own, beside it.
turn = threading.Lock()
watched = {}


def shown(value):
    """A note's value as JSON: a clash as what each agent wrote."""
    if isinstance(value, orvu.Clash):
        return {"clash": {"values": value.values, "authors": value.authors}}
    return value


def task_shown(task):
    if task is None:
        return None
    return {
        "id": task.id,
        "title": task.title,
        "state": task.state,
        "claimed_by": task.claimed_by,
        "created_by": task.created_by,
        "finished_by": task.finished_by,
        "result": task.result,
        "firm": task.firm,
        "ticket": task.ticket,
        "taken_over": task.taken_over,
    }


def wait_until(at):
    """Waits until `at`, in milliseconds since 1970, so that two agents can
    act at the same moment."""
    if at is not None:
        time.sleep(max(0.0, at / 1000 - time.time()))


# ---- the steps ---------------------------------------------------------------


def hello(_):
    return {"me": team.me, "machine": MACHINE, "language": "python"}


def note_put(step):
    wait_until(step.get("at"))
    team.notes.put(step["name"], step["value"])


def note_get(step):
    try:
        return {"value": team.notes.get(step["name"]), "author": team.notes.author(step["name"])}
    except orvu.Clash as clash:
        return {"clash": {"values": clash.values, "authors": clash.authors}}


def task_add(step):
    return task_shown(team.tasks.add(step["title"], firm=step.get("firm", False)))


def task_get(step):
    return task_shown(team.tasks.get(step["id"]))


def task_list(step):
    return [task_shown(task) for task in team.tasks.list(step.get("state"))]


def task_claim(step):
    return team.tasks.claim(step["id"], lease=step.get("lease"))


def task_claim_ticket(step):
    return team.tasks.claim_ticket(step["id"], lease=step.get("lease"))


def task_still_working(step):
    return team.tasks.still_working(step["id"])


def task_still_holding(step):
    return team.tasks.still_holding(step["id"], step["ticket"])


def task_finish(step):
    return team.tasks.finish(step["id"], result=step.get("result"))


def task_set_back(step):
    return task_shown(team.tasks.update(step["id"], state="todo", force=step.get("force", False)))


def budget_top_up(step):
    team.budget(step["name"]).top_up(step["n"])


def budget_spend(step):
    return team.budget(step["name"]).spend(step["n"])


def budget_read(step):
    budget = team.budget(step["name"])
    return {"left": budget.left(), "spent_by": budget.spent_by(), "topped_up_by": budget.topped_up_by()}


def inbox_send(step):
    if step.get("to") is None:
        team.inbox.send_all(step["body"])
    else:
        team.inbox.send(step["to"], step["body"])


def inbox_unread(_):
    return [{"sender": message.sender, "to": message.to, "body": message.body} for message in team.inbox.unread()]


def doc_append(step):
    team.document(step["name"]).append(step["text"])


def doc_read(step):
    document = team.document(step["name"])
    return {"text": document.text(), "written_by": document.written_by()}


def tally_add(step):
    team.tally(step["name"]).add(step["n"])


def tally_read(step):
    tally = team.tally(step["name"])
    return {"total": tally.total(), "by_name": tally.by_name()}


def roster_here(_):
    return team.roster.here()


def search(step):
    return [
        {"kind": found.kind, "name": found.name, "title": found.title, "text": found.text, "score": found.score}
        for found in team.search(step["query"], step.get("limit", 5))
    ]


def watch_budget(step):
    """Starts watching a budget in the background, on a connection of its
    own, until what is left is `until`. `watched` keeps every value seen."""
    name, until = step["name"], step["until"]
    seen = watched[f"budget/{name}"] = []

    def follow():
        with connect() as mine:
            for left in mine.budget(name).watch():
                seen.append(left)
                if left == until:
                    return

    threading.Thread(target=follow, daemon=True).start()


def watch_seen(step):
    return watched.get(step["what"])


STEPS = {step.__name__: step for step in (
    hello, note_put, note_get, task_add, task_get, task_list, task_claim, task_claim_ticket,
    task_still_working, task_still_holding, task_finish, task_set_back, budget_top_up, budget_spend,
    budget_read, inbox_send, inbox_unread, doc_append, doc_read, tally_add, tally_read, roster_here,
    search, watch_budget, watch_seen,
)}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        asked = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        step = STEPS[asked["do"]]
        try:
            with turn:
                answer = {"ok": True, "result": step(asked.get("with", {}))}
        except orvu.OrvuError as error:
            answer = {"ok": False, "error": type(error).__name__, "message": str(error)}
        body = json.dumps(answer, default=shown).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
