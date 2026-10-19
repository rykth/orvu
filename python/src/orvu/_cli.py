"""The `orvu` command."""

import argparse
import sys

from . import _mcp, _mcp_http, _names, _tools, errors
from ._version import VERSION


def parser(command=None):
    """What the command takes. With `command`, the part for that command."""
    whole = argparse.ArgumentParser(prog="orvu", description="Shared memory for teams of AI agents.")
    whole.add_argument("--version", action="version", version=f"orvu {VERSION}")
    commands = whole.add_subparsers(dest="command", metavar="command")

    mcp = commands.add_parser(
        "mcp",
        help="serve what a team shares to an agent as MCP tools",
        description="Serves a team's notes, tasks, budgets, messages, documents, tallies, roster and plain shared objects as MCP tools, over this program's input "
        "and output. An agent program starts this itself: put the command in its MCP settings.",
    )
    mcp.add_argument("--team", default="default", help='the team to use (default: "default")')
    mcp.add_argument("--me", help='a name for this agent, such as "researcher-2" (default: a made-up one)')
    mcp.add_argument(
        "--allow-top-up",
        action="store_true",
        help="also give the agent the tool that adds to a budget (default: it can only spend)",
    )
    mcp.add_argument(
        "--tools",
        metavar="GROUPS",
        help="offer only these groups of tools, such as notes,tasks. The groups are "
        f"{', '.join(_tools.GROUPS)} (default: all)",
    )
    mcp.add_argument("--socket", help="the socket of a sidecar that is already running (default: start one)")
    mcp.add_argument(
        "--http",
        metavar="HOST:PORT",
        help="serve over HTTP at this address, such as 127.0.0.1:8765, instead of over input and output. "
        "Every program that connects uses this one agent. Needs the MCP library: pip install 'orvu[http]'",
    )
    mcp.add_argument(
        "--token-file",
        metavar="PATH",
        help="with --http, a file that only you can read holding the token programs have to give",
    )
    mcp.add_argument(
        "--no-token",
        action="store_true",
        help="with --http on this machine only, let in programs that give no token",
    )

    return mcp if command == "mcp" else whole


def main(arguments=None):
    given = parser().parse_args(arguments)
    if given.command is None:
        parser().print_help()
        return 2
    try:
        _names.part(given.team, "the name of a team")
    except errors.BadRequest as error:
        sys.exit(f"orvu mcp: {error}")
    if given.me and _tools.means_everyone(given.me):
        sys.exit(f"orvu mcp: {_mcp.everyone_refused(given.me)}")
    groups = None
    if given.tools is not None:
        groups = [part.strip() for part in given.tools.split(",") if part.strip()]
        unknown = [part for part in groups if part not in _tools.GROUPS]
        if unknown or not groups:
            what = f"there is no group of tools {unknown[0]!r}" if unknown else "--tools names no group"
            sys.exit(f"orvu mcp: {what}. The groups are: {', '.join(_tools.GROUPS)}")
    try:
        http = _mcp_http.settle(given.http, given.token_file, given.no_token)
    except _mcp_http.Refused as refused:
        sys.exit(f"orvu mcp: {refused}")
    if http is not None:
        host, port, token = http
        _mcp.serve_http(host, port, token, given.team, given.me, given.socket, given.allow_top_up, groups)
        return 0
    _mcp.serve(given.team, given.me, given.socket, given.allow_top_up, groups)
    return 0


if __name__ == "__main__":
    sys.exit(main())
