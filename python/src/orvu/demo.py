"""Three agents work through one task list under one token budget.

    python -m orvu.demo

The agents are three separate programs. They share nothing but the team: a
task board they take work from and a budget they pay from. The budget is too
small for all the work, so the agents stop when it runs out, and it is never
overspent.

Nothing here calls a real model. "Working" is a short wait, and each task has
a made-up price in tokens.

The demo starts a sidecar of its own, in a folder that is removed when it
ends. Nothing it does is kept, so running it again and again does not leave
teams behind.
"""

import os
import pathlib
import random
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

from . import _sidecar, errors
from ._team import Team

AGENTS = ["researcher-1", "researcher-2", "researcher-3"]
CHAPTERS = 12
BUDGET = 3000


def price(task):
    """What a task costs, in tokens: between 200 and 600, made up from the
    chapter number so that every agent works out the same price. All twelve
    together cost 4800, which is more than the budget."""
    chapter = int(task.title.split()[-1])
    return 200 + chapter * 3 % 5 * 100


def agent(name, team_name, socket):
    """One agent. It takes a task nobody has, pays for it, does it, and goes
    on to the next, until the work or the budget runs out."""
    with Team.connect(team_name, me=name, socket=socket) as team:
        tokens = team.budget("tokens")
        while True:
            todo = team.tasks.list("todo")
            if not todo:
                return
            # Any free task will do. Not all going for the first one keeps
            # the agents out of each other's way.
            task = random.choice(todo)
            if not team.tasks.claim(task):
                continue  # another agent got there first
            cost = price(task)
            if not tokens.spend(cost):
                team.tasks.release(task)
                return  # not enough budget left for this one
            time.sleep(cost / 2000)  # the work itself
            team.tasks.finish(task, {"summary": f"{task.title}, in short", "tokens": cost})


def report(team):
    """Prints what happens on the board, as it happens."""
    before = {}
    for tasks in team.tasks.watch():
        for task in tasks:
            was = before.get(task.id)
            before[task.id] = task
            if was is None or was.state == task.state:
                continue
            if task.state == "doing":
                print(f"  {task.claimed_by} took {task.title!r}")
            elif task.state == "done":
                print(f"  {task.finished_by} finished {task.title!r} for {task.result['tokens']} tokens")
            else:
                print(f"  {was.claimed_by} could not pay for {task.title!r} and put it back")


def sidecar(folder):
    """Starts a sidecar that keeps its data in `folder`, and returns it and
    the socket it serves on."""
    socket = folder / "orvud.sock"
    with open(folder / "orvud.log", "ab") as log:
        started = subprocess.Popen(
            # It stops by itself a minute after the last program has gone, in
            # case the demo is ended before it can stop it.
            [_sidecar.program(), "--local", "--data-dir", folder / "data", "--socket", socket, "--exit-when-idle", "60"],
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=log,
        )
    deadline = time.monotonic() + 10
    while not _sidecar.is_listening(socket):
        if started.poll() is not None or time.monotonic() > deadline:
            started.kill()
            raise errors.SidecarUnavailable(f"the demo's sidecar did not start: {(folder / 'orvud.log').read_text()}")
        time.sleep(0.01)
    return started, socket


def main():
    folder = pathlib.Path(tempfile.mkdtemp(prefix="orvu-demo-"))
    started = None
    try:
        started, socket = sidecar(folder)
        run(socket)
    finally:
        if started is not None and started.poll() is None:
            started.send_signal(signal.SIGTERM)
            try:
                started.wait(timeout=10)
            except subprocess.TimeoutExpired:
                started.kill()
                started.wait()
        shutil.rmtree(folder, ignore_errors=True)


def run(socket):
    """The demo itself, on the sidecar at `socket`."""
    team_name = "demo"
    with Team.connect(team_name, me="coordinator", socket=socket) as team:
        team.budget("tokens").top_up(BUDGET)
        for number in range(1, CHAPTERS + 1):
            team.tasks.add(f"Summarise chapter {number}")
        print(f"Team {team_name}: {CHAPTERS} tasks, {BUDGET} tokens, {len(AGENTS)} agents")

        threading.Thread(target=report, args=(team,), daemon=True).start()
        running = [
            subprocess.Popen([sys.executable, "-m", "orvu.demo", "agent", name, team_name, os.fspath(socket)])
            for name in AGENTS
        ]
        failed = [name for name, program in zip(AGENTS, running) if program.wait() != 0]
        time.sleep(0.3)  # let the last lines of the report come out

        tasks = team.tasks.list()
        done = [task for task in tasks if task.state == "done"]
        left = team.budget("tokens").left()
        print()
        for name in AGENTS:
            mine = [task for task in done if task.finished_by == name]
            print(f"{name}: {len(mine)} tasks, {sum(task.result['tokens'] for task in mine)} tokens")
        print(f"tasks done: {len(done)} of {len(tasks)}")
        print(f"tokens spent: {BUDGET - left} of {BUDGET}")
        print(f"tokens left: {left}")
        accounted = sum(task.result["tokens"] for task in done)
        if accounted != BUDGET - left:
            # Claims are soft: now and then two agents take the same task.
            print(f"tokens spent on work done twice: {BUDGET - left - accounted}")
        if failed:
            sys.exit(f"these agents failed: {failed}")


if __name__ == "__main__":
    if sys.argv[1:2] == ["agent"]:
        agent(sys.argv[2], sys.argv[3], sys.argv[4])
    else:
        main()
