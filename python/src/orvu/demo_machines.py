"""Three machines share one team's work, and one of them is switched off.

    python -m orvu.demo_machines

Three sidecars run on this computer and stand for three machines: alpha,
beta and gamma. An agent on each takes tasks from one board and pays for
them from one budget. Part of the way through, gamma is switched off with
no warning. The other two go on, and put back the task gamma had taken.
Later gamma is switched on again, catches up, and carries on.

At the end the three machines hold the same board, and the budget was never
overspent.

Nothing here calls a real model. "Working" is a short wait, and each task
has a made-up price in tokens.
"""

import random
import subprocess
import sys
import threading
import time

from ._machines import LocalMachines
from ._team import Team
from .demo import price

MACHINES = ["alpha", "beta", "gamma"]
TEAM = "crew"
# Each task has a price between 200 and 600 tokens. The twenty-four together
# cost 9800, which is more than the budget.
CHAPTERS = 24
BUDGET = 9000


def tokens(task):
    """What was paid for a finished task. A task that was put back, or
    caught in the middle of being finished, has no result and counts for
    nothing."""
    return (task.result or {}).get("tokens", 0)


def overspent(paid_for, left):
    """True if more work was paid for than money has left the budget. What
    is left can never be below zero, so that alone would say nothing."""
    return paid_for > BUDGET - left


class Quiet:
    """Tells which agents have gone: the ones that were here once and have
    now been missing for several looks in a row. One late heartbeat is not
    an agent that stopped."""

    def __init__(self, looks=3):
        self._looks = looks
        self._missed = {}

    def gone(self, here):
        """Takes who is here now. Returns who has gone."""
        for name in here:
            self._missed[name] = 0
        for name in self._missed:
            if name not in here:
                self._missed[name] += 1
        return {name for name, missed in self._missed.items() if missed >= self._looks}


def agent(name, socket):
    """One agent, on one machine. It takes a task nobody has, pays for it,
    does it, and goes on to the next, until the work or its money runs out."""
    with Team.connect(TEAM, me=name, socket=socket) as team:
        team.roster.join(every=1.0)
        tokens = team.budget("tokens")
        refused = 0
        while True:
            todo = team.tasks.list("todo")
            if not todo:
                # Another agent may be about to put a task back.
                if not team.tasks.list("doing"):
                    say(f"  {name} stops: there is no work left")
                    return
                time.sleep(0.3)
                continue
            task = random.choice(todo)
            if not team.tasks.claim(task):
                continue  # another agent got there first
            cost = price(task)
            if not tokens.spend(cost):
                team.tasks.release(task)
                # This machine's share of the budget may be used up while
                # another machine still holds some. It is asked a few times.
                refused += 1
                if refused == 3:
                    say(f"  {name} stops: its machine has nothing left to spend, and cannot get more")
                    return
                time.sleep(0.5)
                continue
            refused = 0
            time.sleep(cost / 400)  # the work itself
            team.tasks.finish(task, {"summary": f"{task.title}, in short", "tokens": cost})


def say(text):
    print(text, flush=True)


def report(team, stop, told):
    """Prints what happens on the board, as it happens, and notes in `told`
    each task it has said is finished."""
    before = {}
    for tasks in team.tasks.watch():
        if stop.is_set():
            return
        for task in tasks:
            was = before.get(task.id)
            before[task.id] = task
            if was is None or was.state == task.state:
                continue
            if task.state == "doing":
                say(f"  {task.claimed_by} took {task.title!r}")
            elif task.state == "done":
                say(f"  {task.finished_by} finished {task.title!r} for {tokens(task)} tokens")
                told.add(task.id)
            else:
                say(f"  {task.title!r} is back on the board")


def put_back(team, stop):
    """Now and then, looks for a task held by an agent that has gone, and
    puts it back on the board for the others."""
    quiet = Quiet()
    while not stop.wait(0.5):
        gone = quiet.gone(set(team.roster.here()))
        for task in team.tasks.list("doing"):
            if task.claimed_by in gone:
                team.tasks.update(task, state="todo")
                say(f"  {task.claimed_by} has gone quiet, so the task it had is put back")


def boards(machines):
    """The board and the budget as each machine holds them."""
    held = {}
    for name in MACHINES:
        with Team.connect(TEAM, socket=machines.socket(name)) as there:
            held[name] = ([(t.title, t.state, t.finished_by) for t in there.tasks.list()], there.budget("tokens").left())
    return held


def agree(machines, within=15.0):
    """Waits until the three machines hold the same, and returns whether
    they came to. A change usually crosses in a hundredth of a second, and
    now and then takes much longer."""
    deadline = time.monotonic() + within
    while True:
        held = boards(machines)
        if all(board == held["alpha"] for board in held.values()):
            return True
        if time.monotonic() > deadline:
            return False
        time.sleep(0.2)


def start_agent(machines, name):
    return subprocess.Popen([sys.executable, "-m", "orvu.demo_machines", "agent", name, str(machines.socket(name))])


def main(ports=None):
    with LocalMachines(MACHINES, ports=ports) as machines:
        machines.start()
        say(f"Three machines are up: {', '.join(MACHINES)}. Each is a sidecar of its own, with its own copy of the data.")
        with Team.connect(TEAM, me="coordinator", socket=machines.socket("alpha")) as team:
            team.budget("tokens").top_up(BUDGET)
            for number in range(1, CHAPTERS + 1):
                team.tasks.add(f"Summarise chapter {number}")
            say(f"On alpha: {CHAPTERS} tasks and a budget of {BUDGET} tokens. An agent on each machine starts work.")
            # Time for the budget to be shared out between the machines.
            time.sleep(3)

            stop, told = threading.Event(), set()
            threading.Thread(target=report, args=(team, stop, told), daemon=True).start()
            threading.Thread(target=put_back, args=(team, stop), daemon=True).start()
            agents = {name: start_agent(machines, name) for name in MACHINES}

            time.sleep(3)
            agents["gamma"].kill()
            agents["gamma"].wait()
            # What gamma finished before it went is told before it is said to
            # be off, so that none of it seems to happen while it is away. A
            # change takes a moment to reach alpha, where the board is watched.
            with Team.connect(TEAM, socket=machines.socket("gamma")) as there:
                finished = {task.id for task in there.tasks.list("done") if task.finished_by == "gamma"}
            deadline = time.monotonic() + 10
            while not finished <= told and time.monotonic() < deadline:
                time.sleep(0.05)
            say("\n*** gamma is switched off, with no warning ***\n")
            machines.switch_off("gamma")

            time.sleep(6)
            say("\n*** gamma is switched on again. It catches up and carries on ***\n")
            machines.start("gamma")
            agents["gamma"] = start_agent(machines, "gamma")

            failed = [name for name, program in agents.items() if program.wait() != 0]
            stop.set()
            same = agree(machines)
            tasks = team.tasks.list()
            done = [task for task in tasks if task.state == "done"]
            left = team.budget("tokens").left()
            spent_by = team.budget("tokens").spent_by()

            say("")
            for name in MACHINES:
                mine = [task for task in done if task.finished_by == name]
                say(f"{name}: {len(mine)} tasks finished, {spent_by.get(name, 0)} tokens spent")
            say(f"tasks done: {len(done)} of {len(tasks)}")
            say(f"tokens spent: {BUDGET - left} of {BUDGET}")
            say(f"tokens left: {left}")
            paid_for = sum(tokens(task) for task in done)
            unfinished = BUDGET - left - paid_for
            if unfinished > 0:
                # Gamma had paid for a task when it was switched off, and a
                # soft claim now and then lets two agents take one task.
                say(f"tokens spent on work that was cut short or done twice: {unfinished}")
            say(f"the three machines hold the same board and the same budget: {'yes' if same else 'NO'}")
            say(f"the budget was overspent: {'YES' if overspent(paid_for, left) else 'no'}")
            if failed or not same or overspent(paid_for, left):
                sys.exit(f"something went wrong. Agents that failed: {failed}")


if __name__ == "__main__":
    if sys.argv[1:2] == ["agent"]:
        # An agent that loses its machine fails. The one whose machine is
        # switched off on purpose is ended by the demo itself, before that.
        agent(sys.argv[2], sys.argv[3])
    elif sys.argv[1:2] == ["--ports"]:
        main([int(port) for port in sys.argv[2].split(",")])
    else:
        main()
