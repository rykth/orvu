"""The MCP server, used through the MCP project's own client library.

The server's side of the conversation is written out by hand in this package.
These tests are the check that a client nobody here wrote understands it.
"""

import json
import sys
import warnings

import pytest

mcp = pytest.importorskip("mcp")
from mcp.client.stdio import StdioServerParameters, stdio_client  # noqa: E402


def server(sidecar, me):
    return StdioServerParameters(
        command=sys.executable,
        args=["-m", "orvu._cli", "mcp", "--socket", str(sidecar.socket), "--me", me, "--allow-top-up"],
    )


def either(found, *names):
    """The library spells its fields one way in its first version and another
    way in its second. This reads whichever is there."""
    return next(getattr(found, name) for name in names if hasattr(found, name))


def said(result):
    """What a tool answered, read out of the library's result."""
    (content,) = result.content
    return json.loads(content.text)


async def test_the_library_connects_and_lists_the_tools(sidecar):
    async with stdio_client(server(sidecar, "alice")) as (read, write):
        async with mcp.ClientSession(read, write) as session:
            hello = await session.initialize()
            assert either(hello, "serverInfo", "server_info").name == "orvu"
            assert "alice" in hello.instructions
            await session.send_ping()
            tools = (await session.list_tools()).tools
            assert len(tools) == 48
            assert {"note_put", "task_claim", "budget_spend"} <= {tool.name for tool in tools}


async def test_two_agents_work_through_the_library(sidecar):
    async with stdio_client(server(sidecar, "alice")) as (read, write), mcp.ClientSession(read, write) as alice:
        async with stdio_client(server(sidecar, "bob")) as (read, write), mcp.ClientSession(read, write) as bob:
            await alice.initialize()
            await bob.initialize()

            await alice.call_tool("note_put", {"name": "deploy-date", "text": "Friday"})
            assert said(await bob.call_tool("note_get", {"name": "deploy-date"}))["text"] == "Friday"

            task = said(await alice.call_tool("task_add", {"title": "Draft"}))["task"]
            assert said(await alice.call_tool("task_claim", {"task": task["id"]}))["claimed"] is True
            assert said(await bob.call_tool("task_claim", {"task": task["id"]}))["why"] == "alice has it"

            await alice.call_tool("budget_top_up", {"budget": "tokens", "amount": 10})
            assert said(await bob.call_tool("budget_spend", {"budget": "tokens", "amount": 4})) == {
                "spent": True,
                "left": 6,
            }


async def test_a_tool_used_wrongly_reaches_the_library_as_a_failed_tool(sidecar):
    async with stdio_client(server(sidecar, "alice")) as (read, write), mcp.ClientSession(read, write) as session:
        await session.initialize()
        result = await session.call_tool("budget_spend", {"budget": "tokens", "amount": 0})
        assert either(result, "isError", "is_error") is True
        assert "whole number above zero" in result.content[0].text


# The library's newest client asks the server what it speaks before it says
# hello. This server speaks the newer way, so no hello is needed.
async def test_the_librarys_newest_client_connects_the_newer_way(sidecar):
    if not hasattr(mcp, "Client"):
        pytest.skip("this version of the library has no newer way of connecting")
    async with mcp.Client(server(sidecar, "alice"), mode="auto") as client:
        assert client.protocol_version == "2026-07-28"
        assert "alice" in client.instructions
        assert said(await client.call_tool("whoami", {})) == {"team": "default", "you": "alice"}
        tools = (await client.list_tools()).tools
        assert len(tools) == 48


async def test_the_librarys_client_can_still_say_hello_the_older_way(sidecar):
    if not hasattr(mcp, "Client"):
        pytest.skip("this version of the library has no choice of way")
    async with mcp.Client(server(sidecar, "alice"), mode="legacy") as client:
        assert client.protocol_version == "2025-11-25"
        assert said(await client.call_tool("whoami", {})) == {"team": "default", "you": "alice"}


# Resources, word of their changes, and prompts, through the library.
async def test_the_library_reads_and_subscribes_to_resources(sidecar, team):
    import anyio

    told = anyio.Event()

    async def heard(message):
        # The first version of the library wraps each message in another.
        message = getattr(message, "root", message)
        if type(message).__name__ == "ResourceUpdatedNotification" and str(message.params.uri) == "orvu://notes":
            told.set()

    async with stdio_client(server(sidecar, "alice")) as (read, write):
        async with mcp.ClientSession(read, write, message_handler=heard) as session:
            await session.initialize()
            listed = (await session.list_resources()).resources
            assert [str(found.uri) for found in listed] == ["orvu://inbox", "orvu://tasks", "orvu://notes"]
            team.notes.put("deploy-date", "Friday")
            (content,) = (await session.read_resource("orvu://notes")).contents
            assert json.loads(content.text) == {"notes": {"deploy-date": "Friday"}}
            prompts = (await session.list_prompts()).prompts
            assert [prompt.name for prompt in prompts] == ["check_inbox", "pick_a_task"]
            assert (await session.get_prompt("check_inbox")).messages

            with warnings.catch_warnings():
                # The library would have the newer way followed instead.
                warnings.simplefilter("ignore")
                await session.subscribe_resource("orvu://notes")
            team.notes.put("deploy-date", "Monday")
            with anyio.fail_after(10):
                await told.wait()
