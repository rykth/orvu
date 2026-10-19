"""The `orvud` command that is installed with the package."""

import subprocess
import sys

import orvu


def test_the_orvud_command_runs_the_sidecar_program(home):
    said = subprocess.run(
        [sys.executable, "-c", "from orvu._sidecar import main; main()", "--version"],
        capture_output=True,
        text=True,
    )
    assert said.stdout.strip() == f"orvud {orvu.__version__}"
