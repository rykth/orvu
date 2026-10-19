"""The demo: three agents, one task list, one budget."""

import re
import subprocess
import sys


def number(text, label):
    return int(re.search(rf"^{label}: (\d+)", text, re.M).group(1))


def test_the_demo_runs_and_its_sums_add_up(home):
    import os

    temporary = home / "tmp"
    temporary.mkdir()
    ran = subprocess.run(
        [sys.executable, "-m", "orvu.demo"],
        capture_output=True,
        text=True,
        timeout=120,
        env={**os.environ, "TMPDIR": str(temporary)},
    )
    assert ran.returncode == 0, ran.stderr
    out = ran.stdout

    # The demo works in a sidecar of its own, in a folder that is removed when
    # it ends, so runs do not pile up teams anywhere. The sidecar a program
    # starts by itself was never started.
    assert list(temporary.iterdir()) == []
    assert not (home / ".local" / "share" / "orvu").exists()

    spent = number(out, "tokens spent")
    left = number(out, "tokens left")
    done = number(out, "tasks done")
    assert spent + left == 3000, "the budget does not add up"
    assert left < 600, "the agents stopped while there was budget for any task"
    assert 5 <= done < 12, "the budget is meant to run out before the work does"

    # Every agent's line, and together they account for the tasks done.
    per_agent = re.findall(r"^researcher-\d: (\d+) tasks, (\d+) tokens$", out, re.M)
    assert len(per_agent) == 3
    assert sum(int(tasks) for tasks, _ in per_agent) == done

    # What was paid is what the finished work cost, plus any work done twice.
    paid_for = sum(int(tokens) for _, tokens in per_agent)
    twice = re.search(r"^tokens spent on work done twice: (\d+)", out, re.M)
    assert paid_for + (int(twice.group(1)) if twice else 0) == spent

    # The live report said who took and who finished each task that got done.
    assert len(re.findall(r"^  researcher-\d finished 'Summarise chapter \d+' for \d+ tokens$", out, re.M)) == done


# Three machines on this computer, and one of them is switched off part of
# the way through and on again later.
def test_the_demo_with_a_machine_switched_off_ends_with_every_machine_agreeing():
    import os

    from conftest import free_port, orvud_bin

    ports = ",".join(str(free_port()) for _ in range(3))
    ran = subprocess.run(
        [sys.executable, "-m", "orvu.demo_machines", "--ports", ports],
        capture_output=True,
        text=True,
        timeout=180,
        env={**os.environ, "ORVUD_BIN": str(orvud_bin())},
    )
    assert ran.returncode == 0, ran.stdout + ran.stderr
    out = ran.stdout
    assert out.index("gamma is switched off") < out.index("gamma is switched on again")
    assert "the three machines hold the same board and the same budget: yes" in out
    assert "the budget was overspent: no" in out

    spent, left, done = number(out, "tokens spent"), number(out, "tokens left"), number(out, "tasks done")
    per_machine = re.findall(r"^(alpha|beta|gamma): (\d+) tasks finished, (\d+) tokens spent$", out, re.M)
    assert [name for name, _, _ in per_machine] == ["alpha", "beta", "gamma"]
    assert sum(int(tasks) for _, tasks, _ in per_machine) == done
    # What the agents wrote down as spent is counted apart from the budget
    # itself, and the two have to tell the same story. The one way they may
    # differ is gamma having paid just as it was switched off, before it
    # could write that down: a task's price at most.
    written_down = sum(int(tokens) for _, _, tokens in per_machine)
    assert spent - 600 <= written_down <= spent, "what was spent and what was written down disagree"
    # The finished work was paid for: its prices come to no more than was spent.
    finished = sum(int(price) for price in re.findall(r" finished 'Summarise chapter \d+' for (\d+) tokens$", out, re.M))
    assert finished <= spent and spent + left == 9000
    # Each of the three got work done, gamma too, before or after it was away.
    assert all(int(tasks) > 0 for _, tasks, _ in per_machine)
    # While gamma was away the other two went on finishing tasks.
    away = out[out.index("gamma is switched off") : out.index("gamma is switched on again")]
    assert re.search(r"(alpha|beta) finished", away) and "gamma finished" not in away

