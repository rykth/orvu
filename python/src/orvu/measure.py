"""Measures how fast Orvu is and how much room it takes, on this computer.

    python -m orvu.measure                  # a minute or two
    python -m orvu.measure --quick          # a few seconds, with small numbers
    python -m orvu.measure --into PAGE.md   # puts the result into a page

Three sidecars are started on this computer and stand for three machines of
one team. Everything is measured through the Python library, as a program
that uses Orvu would see it. What is printed is a page of tables, with the
computer it was measured on at the top.

What the numbers leave out is said on the page: the three machines are on
one computer, so nothing here says how long a network takes, and the
programs are Python, so part of every time is Python's own.
"""

import json
import os
import platform
import statistics
import subprocess
import sys
import time

from . import _machines
from ._team import Team
from ._version import VERSION

TEAM = "measure"


def per_second(count, seconds):
    return f"{count / seconds:,.0f}"


def milliseconds(seconds):
    return f"{seconds * 1000:.2f}"


def timed(work):
    started = time.perf_counter()
    work()
    return time.perf_counter() - started


def middle(work, times=15):
    """How long `work` takes, in seconds: the middle one of several tries.
    One slow moment is then not taken for how long the thing takes."""
    return statistics.median(timed(work) for _ in range(times))


def table(title, columns, rows):
    print(f"\n### {title}\n")
    print("| " + " | ".join(columns) + " |")
    print("|" + "---|" * len(columns))
    for row in rows:
        print("| " + " | ".join(str(cell) for cell in row) + " |")
    sys.stdout.flush()


def kilobytes(path):
    """How much room a folder takes on disk."""
    return sum(file.stat().st_size for file in path.rglob("*") if file.is_file()) // 1024


def memory(pid):
    """How much memory a program holds, in kilobytes, or `None` where that
    cannot be read."""
    try:
        with open(f"/proc/{pid}/status") as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


def megabytes(kb):
    return "?" if kb is None else f"{kb / 1024:.1f}"


def computer():
    cpu = platform.processor() or platform.machine()
    try:
        with open("/proc/cpuinfo") as info:
            cpu = next(line.split(":", 1)[1].strip() for line in info if line.startswith("model name"))
    except (OSError, StopIteration):
        pass
    return f"{cpu}, {os.cpu_count()} cores, {platform.system()} {platform.release()}, Python {platform.python_version()}"


# ---- what is measured --------------------------------------------------------


def budget_to_spend(team, n):
    """A budget this machine can spend `n` from, one at a time, without
    having to ask another machine.

    A budget is shared out between the machines, so what is topped up here
    does not all stay here. Far more than is needed is put in, and the
    sharing out is given time to settle, so that this machine's own share
    covers all that will be spent.
    """
    tokens = team.budget("speed")
    tokens.top_up(n * 30)
    time.sleep(3.5)
    return tokens


def one_program(team, n):
    """One program doing one thing after another."""
    notes, tokens = team.notes, budget_to_spend(team, n)
    for number in range(20):
        notes.put(f"warm-{number}", "x")  # the first calls set things up
    # A hundred notes, written over and over. How the number of notes
    # changes things is measured further down, by itself.
    rows = [
        ("Write a note", per_second(n, timed(lambda: [notes.put(f"note-{i % 100}", "a short fact") for i in range(n)]))),
        ("Read a note", per_second(n, timed(lambda: [notes.get(f"note-{i % 100}") for i in range(n)]))),
    ]

    def spend():
        if not all(tokens.spend(1) for _ in range(n)):
            sys.exit("a spend was refused while spending was being timed, so the time would mean nothing")

    rows.append(("Spend from a budget", per_second(n, timed(spend))))

    def in_batches():
        for start in range(0, n, 100):
            with team.client.batch() as batch:
                held = batch.map("orvu/measure/batched")
                for i in range(100):
                    held.put(f"k-{i}", "a short fact")

    rows.append(("Write a note, 100 in one message", per_second(n, timed(in_batches))))
    cycles = max(n // 50, 5)

    def tasks():
        for i in range(cycles):
            task = team.tasks.add(f"Task {i}")
            team.tasks.claim(task)
            team.tasks.finish(task, "done")

    rows.append(("Add, take and finish a task", per_second(cycles, timed(tasks))))
    table("One program, one thing after another", ["What", "Each second"], rows)


def worker(socket, number, each):
    """One of several programs that write at the same moment. It is run as a
    program of its own, says when it is ready, waits to be told to go, and
    then says how long each of its writes took."""
    with Team.connect(TEAM, socket=socket, me=f"agent-{number}") as team:
        team.notes.put(f"status-{number}", "ready")  # the first call sets things up
        print("ready", flush=True)
        sys.stdin.readline()
        waits, started = [], time.time()
        for i in range(each):
            before = time.perf_counter()
            team.notes.put(f"status-{number}", f"step {i}")
            waits.append(time.perf_counter() - before)
        print(json.dumps({"started": started, "finished": time.time(), "waits": waits}), flush=True)


def many_programs(socket, counts, each):
    """Several programs writing at the same moment, each a process of its
    own with a connection of its own, and each writing its own note over
    and over."""
    rows = []
    for count in counts:
        programs = [
            subprocess.Popen(
                [sys.executable, "-m", "orvu.measure", "worker", str(socket), str(number), str(each)],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                text=True,
            )
            for number in range(count)
        ]
        try:
            for program in programs:
                if program.stdout.readline().strip() != "ready":
                    sys.exit("a program that was to write did not start")
            # All are connected and waiting. They are let go together.
            for program in programs:
                program.stdin.write("go\n")
                program.stdin.flush()
            said = [json.loads(program.stdout.readline()) for program in programs]
        finally:
            for program in programs:
                program.stdin.close()
                program.wait()
        waits = sorted(wait for one in said for wait in one["waits"])
        took = max(one["finished"] for one in said) - min(one["started"] for one in said)
        rows.append(
            (
                count,
                per_second(count * each, took),
                milliseconds(statistics.median(waits)),
                milliseconds(waits[int(len(waits) * 0.99) - 1]),
            )
        )
    table(
        "Several programs on one machine, each writing its own note over and over",
        ["Programs", "Writes each second, all together", "Usual wait for one write (ms)", "Slowest 1 in 100 (ms)"],
        rows,
    )


def between_machines(here, there, n):
    """How long a note written on one machine takes to be read on another."""
    waits = []
    for i in range(n):
        name = f"sent-{i}"
        started = time.perf_counter()
        here.notes.put(name, "x")
        while there.notes.get(name) is None:
            if time.perf_counter() - started > 30:
                sys.exit("a note did not reach the other machine in half a minute")
        waits.append(time.perf_counter() - started)
    waits.sort()
    table(
        "From one machine to another",
        ["What", "Usual (ms)", "Slowest 1 in 20 (ms)", "Slowest (ms)"],
        [
            (
                "A note written on alpha can be read on beta",
                milliseconds(statistics.median(waits)),
                milliseconds(waits[int(len(waits) * 0.95) - 1]),
                milliseconds(waits[-1]),
            )
        ],
    )


def growth(team, machines, sizes):
    """Room on disk, memory, and the time things take, as the notes grow."""
    rows, written = [], 0
    notes = team.client.map("orvu/measure/grown")
    for size in sizes:
        while written < size:
            with team.client.batch() as batch:
                held = batch.map(notes.name)
                for i in range(written, min(written + 500, size)):
                    held.put(f"note-{i:07}", "a fact of about fifty characters, as notes often are")
            written = min(written + 500, size)
        time.sleep(1.5)  # the sidecar saves to disk a little after it answers
        rows.append(
            (
                f"{size:,}",
                megabytes(kilobytes(machines.home("alpha") / "data")),
                megabytes(memory(machines.pid("alpha"))),
                milliseconds(middle(lambda: notes.put("note-0000000", "written again"))),
                milliseconds(middle(lambda: notes.get("note-0000001"))),
                milliseconds(middle(notes.entries, times=5)),
            )
        )
    table(
        "As the notes of a team grow (one machine of three)",
        ["Notes", "On disk (MB)", "Memory of the sidecar (MB)", "Write one (ms)", "Read one (ms)", "Read them all (ms)"],
        rows,
    )


def fill_board(team, size):
    """Adds tasks until there are exactly `size` on the board."""
    for number in range(len(team.tasks.list()), size):
        team.tasks.add(f"Task {number}")


def board(team, sizes):
    """Work on one task reads that task alone, and listing reads every task.
    `team` has to be one with nothing on its board yet."""
    rows = []
    for size in sizes:
        fill_board(team, size)
        task = team.tasks.list()[0]
        change = middle(lambda: team.tasks.update(task, notes="changed"), times=9)
        listing = middle(team.tasks.list, times=9)
        # Adding is timed last, and the tasks it adds are not counted in.
        adding = middle(lambda: team.tasks.add("One more"), times=5)
        rows.append((f"{size:,}", milliseconds(adding), milliseconds(change), milliseconds(listing)))
    table("As the task board grows", ["Tasks", "Add one (ms)", "Change one (ms)", "List them all (ms)"], rows)


def main(quick=False, ports=None):
    n = 200 if quick else 2000
    print("## The numbers\n")
    print(f"- **Computer:** {computer()}")
    # The end of the sidecar's path says which build it is. The start of it
    # says where somebody keeps their files, and is left out.
    sidecar = "/".join(_machines._sidecar.program().parts[-3:])
    print(f"- **Orvu:** {VERSION}, sidecar `.../{sidecar}`")
    print(f"- **How:** `python -m orvu.measure{' --quick' if quick else ''}`, three machines on this one computer")
    sys.stdout.flush()

    with _machines.LocalMachines(ports=ports) as machines:
        # Each sidecar is started by itself and timed, from nothing to the
        # moment it says it is serving.
        starts = [timed(lambda name=name: machines.start(name)) for name in machines.names]
        with Team.connect(TEAM, socket=machines.socket("alpha"), me="alpha") as alpha, Team.connect(
            TEAM, socket=machines.socket("beta"), me="beta"
        ) as beta, Team.connect(TEAM + "-board", socket=machines.socket("alpha"), me="alpha") as boards:
            one_program(alpha, n)
            many_programs(machines.socket("alpha"), [1, 4] if quick else [1, 4, 16, 64], 50 if quick else 300)
            between_machines(alpha, beta, 30 if quick else 300)
            growth(alpha, machines, [500, 1000] if quick else [1000, 5000, 10000, 20000])
            board(boards, [20, 50] if quick else [100, 1000, 5000])
        still = [name for name in machines.names if machines.pid(name) is None]
        if still:
            sys.exit(f"these machines stopped while they were being measured: {still}")
        table(
            "Starting up",
            ["What", "Thousandths of a second"],
            [("One sidecar, from nothing to answering", milliseconds(statistics.median(starts)))],
        )


START, END = "<!-- measured: start -->", "<!-- measured: end -->"


def into(page, measure):
    """Runs `measure` and puts what it prints into the file `page`, between
    the two marks, in place of what was there."""
    import contextlib
    import io
    import pathlib

    page = pathlib.Path(page)
    text = page.read_text()
    if text.count(START) != 1 or text.count(END) != 1:
        sys.exit(f"{page} needs one line {START} and one line {END}: the result goes between them")
    printed = io.StringIO()
    with contextlib.redirect_stdout(printed):
        measure()
    before, rest = text.split(START)
    page.write_text(f"{before}{START}\n{printed.getvalue().strip()}\n{END}{rest.split(END)[1]}")
    print(f"the result is in {page}")


if __name__ == "__main__":
    arguments = sys.argv[1:]
    if arguments[:1] == ["worker"]:
        worker(arguments[1], int(arguments[2]), int(arguments[3]))
        sys.exit(0)
    ports = None
    if "--ports" in arguments:
        ports = [int(port) for port in arguments[arguments.index("--ports") + 1].split(",")]

    def measure():
        main(quick="--quick" in arguments, ports=ports)

    if "--into" in arguments:
        into(arguments[arguments.index("--into") + 1], measure)
    else:
        measure()
