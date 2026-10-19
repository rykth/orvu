"""Runs the code in QUICKSTART.md, in order, the way a reader would."""

import contextlib
import io
import pathlib
import re

QUICKSTART = pathlib.Path(__file__).resolve().parents[1] / "QUICKSTART.md"
STEPS = re.findall(r"```python\n(.*?)```", QUICKSTART.read_text(), flags=re.S)


def run(step):
    """Runs one piece of code and returns what it printed."""
    printed = io.StringIO()
    with contextlib.redirect_stdout(printed):
        exec(step, {})
    return printed.getvalue()


# One home folder for all the steps, because each builds on the ones before.
def test_the_quickstart_works_from_top_to_bottom(home):
    assert len(STEPS) == 6
    first, second, tasks, budget, watch, more = (run(step) for step in STEPS)

    assert first.strip() == "{'deploy-date': 'Friday'}"
    assert second.strip() == "Friday"
    assert tasks.splitlines() == [
        "done Summarise the Q3 report alice",
        "done Check the numbers alice",
    ]
    assert budget.splitlines() == ["spent 300, 700 left", "not enough left, nothing was spent"]
    assert watch.strip() == "2 tasks on the board"
    assert more.splitlines() == [
        "[('Sales rose 4%.', 'alice'), (' Costs fell.', 'bob')]",
        "alice says: The report is started.",
        "{'alice': 1200}",
    ]


def test_the_quickstart_points_at_files_that_exist():
    folder = QUICKSTART.parent
    assert (folder / "README.md").exists()
    assert (folder / "src" / "orvu" / "demo.py").exists()
    assert (folder.parent / "crates" / "orvud" / "README.md").exists()
