"""The measuring program, run with small numbers, and the page of results."""

import os
import pathlib
import re
import subprocess
import sys

from conftest import free_port, orvud_bin

PAGE = pathlib.Path(__file__).resolve().parents[1] / "MEASUREMENTS.md"


# The measuring is run once, with small numbers, and asked to put its result
# into a copy of the page, which is how the page in the repository is made.
def test_the_measuring_runs_and_puts_its_tables_into_the_page(tmp_path):
    page = PAGE.read_text()
    copy = tmp_path / "MEASUREMENTS.md"
    copy.write_text(page)
    ports = ",".join(str(free_port()) for _ in range(3))
    ran = subprocess.run(
        [sys.executable, "-m", "orvu.measure", "--quick", "--ports", ports, "--into", copy],
        capture_output=True,
        text=True,
        timeout=180,
        env={**os.environ, "ORVUD_BIN": str(orvud_bin())},
    )
    assert ran.returncode == 0, ran.stderr
    after = copy.read_text()
    out = after.split("<!-- measured: start -->")[1].split("<!-- measured: end -->")[0]

    titles = re.findall(r"^### (.+)$", out, re.M)
    assert len(titles) == 6
    rows = [line for line in out.splitlines() if line.startswith("| ") and "---" not in line]
    measured = [row for row in rows if re.search(r"\| [\d,.]+ \|$", row)]
    assert len(measured) == 13, out
    assert "--quick" in out and "- **Computer:**" in out

    # Everything around the tables is as it was.
    assert after.split("<!-- measured: start -->")[0] == page.split("<!-- measured: start -->")[0]
    assert after.split("<!-- measured: end -->")[1] == page.split("<!-- measured: end -->")[1]

    # The page in the repository shows the same tables, with the same
    # columns. A table added to the program and not measured again is caught.
    for title in titles:
        assert f"### {title}" in page, title
    for header in re.findall(r"^\| (?:What|Programs|Notes|Tasks) .*\|$", out, re.M):
        assert header in page, header


def test_a_page_with_no_place_for_the_result_is_refused(tmp_path):
    page = tmp_path / "page.md"
    page.write_text("# No marks here\n")
    ran = subprocess.run([sys.executable, "-m", "orvu.measure", "--quick", "--into", page], capture_output=True, text=True)
    assert ran.returncode != 0 and "measured: start" in ran.stderr
    assert page.read_text() == "# No marks here\n"


def test_the_page_says_how_it_was_measured_and_what_it_leaves_out():
    page = PAGE.read_text()
    assert "python -m orvu.measure" in page
    assert "- **Computer:**" in page and "- **Orvu:**" in page
    for heading in ("## How it is measured", "## What the numbers say", "## What they do not say"):
        assert heading in page, heading
