"""MCP.md, the setup page for agent programs, against the real server."""

import json
import pathlib
import re

from orvu import _cli
from orvu._tools import TOOLS

PAGE = pathlib.Path(__file__).resolve().parents[1] / "MCP.md"
TEXT = PAGE.read_text()


def test_the_page_lists_every_tool_and_no_other():
    listed = re.findall(r"^\| `([a-z_]+)` \|", TEXT, flags=re.M)
    assert listed == list(TOOLS)


def test_the_page_lists_every_option_and_no_other():
    listed = re.findall(r"^\| `(--[a-z-]+)", TEXT, flags=re.M)
    real = [option for action in _cli.parser("mcp")._actions for option in action.option_strings if option != "-h"]
    assert sorted(listed) == sorted(set(real) - {"--help"})


# Each settings file on the page is read the way an agent's program reads it,
# and the command in it has to be one the real `orvu` command accepts.
def test_every_settings_file_on_the_page_holds_a_command_that_works():
    files = [json.loads(block) for block in re.findall(r"```json\n(.*?)```", TEXT, flags=re.S)]
    assert len(files) == 2
    for settings in files:
        (servers,) = settings.values()
        entry = servers["orvu"]
        assert entry["command"] == "orvu"
        given = _cli.parser().parse_args(entry["args"])
        assert (given.command, given.me) == ("mcp", "researcher-1")


def test_every_command_on_the_page_is_one_the_real_command_accepts():
    commands = re.findall(r"^(?:claude mcp add orvu -- )?orvu (mcp.*)$", TEXT, flags=re.M)
    assert len(commands) == 3
    for command in commands:
        assert _cli.parser().parse_args(command.split()).command == "mcp"


def test_the_page_points_at_files_that_exist():
    for name in re.findall(r"\]\(([A-Z]+\.md)\)", TEXT):
        assert (PAGE.parent / name).exists(), name
