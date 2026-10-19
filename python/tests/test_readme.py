"""Runs the examples in README.md, so that they cannot go stale."""

import pathlib
import re
import subprocess
import sys

from conftest import orvud_bin

README = pathlib.Path(__file__).resolve().parents[1] / "README.md"
EXAMPLES = re.findall(r"```python\n(.*?)```", README.read_text(), flags=re.S)


def test_the_readme_has_examples():
    assert len(EXAMPLES) == 6
    assert "Team.connect(" in EXAMPLES[0] and "Team.connect(" in EXAMPLES[1]
    for example in EXAMPLES[2:]:
        assert "orvu.connect()" in example


# Each example is run exactly as it is written, in a home folder of its own
# with no sidecar running.
def test_readme_example_1(home):
    exec(EXAMPLES[0], {})


# The agent was given a key, and the key is in its environment.
def test_readme_example_2(home, monkeypatch, capsys):
    added = subprocess.run([orvud_bin(), "agent", "add", "alice", "--local"], capture_output=True, text=True, check=True)
    monkeypatch.setenv("ORVU_KEY", next(line for line in added.stdout.splitlines() if line.startswith("orvu-key-")))
    exec(EXAMPLES[1], {})
    assert capsys.readouterr().out == "alice\n"


def test_readme_example_3(home):
    exec(EXAMPLES[2], {})


def test_readme_example_4(home):
    exec(EXAMPLES[3], {})


def test_readme_example_5(home):
    exec(EXAMPLES[4], {})


def test_readme_example_6(home):
    exec(EXAMPLES[5], {})


def test_the_quickstart_runs_twice_and_remembers(home):
    quickstart = README.parent / "examples" / "quickstart.py"
    first = subprocess.run([sys.executable, quickstart], capture_output=True, text=True)
    second = subprocess.run([sys.executable, quickstart], capture_output=True, text=True)
    assert "run 1 time" in first.stdout, first.stderr
    assert "run 2 time" in second.stdout, second.stderr
