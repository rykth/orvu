"""The package on Windows, as near as this machine can come to it: Python
told it is on Windows, with what Windows does not have taken away. An agent
there connects to a sidecar elsewhere over the network, and is told so if it
asks for one of its own."""

import json
import os
import subprocess
import sys

from test_network import networked  # noqa: F401  (a fixture)

# Run first: what Windows has not got is taken away, and Python says it is
# on Windows. gRPC and Python's own modules are loaded before: they are
# Windows' own there, and this is about the package's code.
LIKE_WINDOWS = """
import os, signal, socket, sys
import argparse, asyncio, dataclasses, json, pathlib, secrets, shutil, subprocess, tempfile, threading, urllib.parse
import grpc
sys.modules["fcntl"] = None
for module, names in ((socket, ["AF_UNIX"]), (os, ["getuid", "geteuid", "setsid", "fork"]), (signal, ["SIGKILL"])):
    for name in names:
        if hasattr(module, name):
            delattr(module, name)
sys.platform = "win32"
"""


def like_windows(program, env=None):
    """Runs `program` in a Python that looks like one on Windows, and gives
    what it printed."""
    done = subprocess.run(
        [sys.executable, "-c", LIKE_WINDOWS + program],
        capture_output=True, text=True, env=env, timeout=60,
    )
    assert done.returncode == 0, done.stderr
    return done.stdout


def test_an_agent_on_windows_connects_over_the_network(networked, home):  # noqa: F811
    printed = like_windows(f"""
import orvu
with orvu.Team.connect(connection={str(networked.connection)!r}) as team:
    team.notes.put("from", "windows")
    print(team.me, team.notes.get("from"))
""")
    assert printed == "alice windows\n"


# A program that asks for a sidecar of its own is told to connect to one
# elsewhere, not that something is broken.
def test_an_agent_on_windows_is_told_to_connect_over_the_network(home):
    printed = like_windows("""
import orvu
try:
    orvu.connect()
except orvu.SidecarUnavailable as error:
    print(error)
""")
    assert "no sidecar for Windows" in printed and "ORVU_CONNECT" in printed


# The package's `orvud` command has no sidecar to run on Windows, and says
# so, with what to do instead. It does not try to start one: starting a
# program there does not replace this one, and it would go round and round.
def test_the_orvud_command_on_windows_says_what_to_do(home):
    printed = like_windows("""
from orvu._sidecar import main
try:
    main()
except SystemExit as stopped:
    print(stopped.code)
""")
    assert printed.startswith("orvud: there is no sidecar for Windows yet") and "ORVU_CONNECT" in printed


def test_the_tools_work_on_windows_over_the_network(networked, home):  # noqa: F811
    env = {**os.environ, "ORVU_CONNECT": str(networked.connection)}
    printed = like_windows("""
import io, sys
sys.stdin = io.TextIOWrapper(io.BytesIO(b'{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}\\n'))
from orvu._cli import main
main(["mcp"])
""", env=env)
    answer = json.loads(printed.splitlines()[0])
    assert json.loads(answer["result"]["content"][0]["text"])["you"] == "alice"
