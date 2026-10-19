"""The MCP server over HTTP, `orvu mcp --http`, spoken to with the MCP
project's own client library and with plain requests."""

import json
import os
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

import pytest

mcp = pytest.importorskip("mcp")
httpx = pytest.importorskip("httpx2")
from mcp.client.streamable_http import streamable_http_client  # noqa: E402

from ports import free_port  # noqa: E402

TOKEN = "a-token-long-enough-to-guess-at-never"


def private_file(folder, text=TOKEN, mode=0o600):
    path = folder / "token"
    path.write_text(text + "\n")
    path.chmod(mode)
    return path


class Http:
    """One `orvu mcp --http` program."""

    def __init__(self, sidecar, *arguments, me="alice"):
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}/mcp"
        self.process = subprocess.Popen(
            [sys.executable, "-m", "orvu._cli", "mcp", "--socket", str(sidecar.socket), "--me", me,
             "--http", f"127.0.0.1:{self.port}", *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + 20
        while not self.listening():
            assert self.process.poll() is None, self.process.stderr.read().decode()
            assert time.monotonic() < deadline, "the server did not start listening"
            time.sleep(0.05)

    def listening(self):
        with socket.socket() as probe:
            return probe.connect_ex(("127.0.0.1", self.port)) == 0

    def stop(self):
        self.process.terminate()
        code = self.process.wait(timeout=10)
        log = self.process.stderr.read().decode()
        self.process.stdout.close()
        self.process.stderr.close()
        assert code == 0, log
        return log


@pytest.fixture
def http(sidecar, tmp_path):
    started = []

    def start(*arguments, me="alice"):
        server = Http(sidecar, *arguments, me=me)
        started.append(server)
        return server

    yield start
    for server in started:
        if server.process.poll() is None:
            server.stop()


def said(result):
    (content,) = result.content
    return json.loads(content.text)


def session(url, token=TOKEN):
    headers = {"Authorization": f"Bearer {token}"} if token is not None else {}
    return httpx.AsyncClient(headers=headers, timeout=httpx.Timeout(30, read=60))


def post(url, headers=None, body=None):
    """A plain request, as anything on the network could send. The status
    it was answered with."""
    body = body or {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                               "clientInfo": {"name": "test", "version": "1"}}}
    request = urllib.request.Request(
        url, json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream", **(headers or {})},
    )
    try:
        with urllib.request.urlopen(request, timeout=10) as answer:
            return answer.status
    except urllib.error.HTTPError as refused:
        return refused.code


def refused_to_start(sidecar, *arguments):
    """What `orvu mcp` said when it would not start with these arguments."""
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp", "--socket", str(sidecar.socket), "--me", "alice", *arguments],
        stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=30,
    )
    assert done.returncode != 0, done.stderr
    return done.stderr


async def test_a_tool_is_called_over_http_with_the_token(http, tmp_path, team):
    server = http("--token-file", str(private_file(tmp_path)))
    async with session(server.url) as client, streamable_http_client(server.url, http_client=client) as (read, write):
        async with mcp.ClientSession(read, write) as agent:
            hello = await agent.initialize()
            assert "alice" in hello.instructions
            assert said(await agent.call_tool("whoami", {})) == {"team": "default", "you": "alice"}
            await agent.call_tool("note_put", {"name": "deploy-date", "text": "Friday"})
    assert team.notes.get("deploy-date") == "Friday"
    assert team.roster.here() == ["alice"]
    assert "http://127.0.0.1" in server.stop()
    # Stopped as asked, it took the agent off the roster.
    assert team.roster.members() == []


# The newer way of MCP has no hello and no session: each request stands on
# its own.
async def test_the_librarys_newest_client_connects_over_http(http, tmp_path):
    server = http("--token-file", str(private_file(tmp_path)))
    async with session(server.url) as client:
        async with mcp.Client(streamable_http_client(server.url, http_client=client), mode="auto") as agent:
            assert agent.protocol_version == "2026-07-28"
            assert said(await agent.call_tool("whoami", {})) == {"team": "default", "you": "alice"}


async def test_a_program_without_the_token_or_with_another_is_refused(http, tmp_path):
    server = http("--token-file", str(private_file(tmp_path)))
    assert post(server.url) == 401
    assert post(server.url, {"Authorization": "Bearer not-the-token"}) == 401
    assert post(server.url, {"Authorization": f"Basic {TOKEN}"}) == 401
    assert post(server.url, {"Authorization": f"Bearer {TOKEN}"}) == 200
    async with session(server.url, "not-the-token") as client:
        with pytest.raises(BaseException):
            async with streamable_http_client(server.url, http_client=client) as (read, write):
                async with mcp.ClientSession(read, write) as agent:
                    await agent.initialize()


async def test_the_agent_is_told_of_a_change_over_http(http, tmp_path, team):
    import anyio

    server = http("--token-file", str(private_file(tmp_path)))
    told = anyio.Event()

    async def heard(message):
        message = getattr(message, "root", message)
        if type(message).__name__ == "ResourceUpdatedNotification":
            told.set()

    async with session(server.url) as client, streamable_http_client(server.url, http_client=client) as (read, write):
        async with mcp.ClientSession(read, write, message_handler=heard) as agent:
            await agent.initialize()
            import warnings

            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                await agent.subscribe_resource("orvu://notes")
            # The library opens the stream the word comes on once the
            # session has started. Changes go on until one is heard.
            with anyio.fail_after(20):
                n = 0
                while not told.is_set():
                    n += 1
                    team.notes.put("count", n)
                    with anyio.move_on_after(0.5):
                        await told.wait()


# Every program that connects is the one agent the server was started as.
async def test_two_programs_over_http_are_the_same_agent(http, tmp_path, team):
    server = http("--token-file", str(private_file(tmp_path)))
    async with session(server.url) as one, session(server.url) as two:
        async with streamable_http_client(server.url, http_client=one) as (read, write), mcp.ClientSession(read, write) as first:
            async with streamable_http_client(server.url, http_client=two) as (read2, write2), mcp.ClientSession(read2, write2) as second:
                await first.initialize()
                await second.initialize()
                assert said(await first.call_tool("whoami", {}))["you"] == "alice"
                assert said(await second.call_tool("whoami", {}))["you"] == "alice"
                assert said(await second.call_tool("roster_list", {})) == {"agents": [{"name": "alice", "here": True}]}


def test_binding_beyond_this_machine_needs_a_token_file(sidecar):
    said = refused_to_start(sidecar, "--http", f"0.0.0.0:{free_port()}")
    assert "--token-file" in said
    said = refused_to_start(sidecar, "--http", f"0.0.0.0:{free_port()}", "--no-token")
    assert "--no-token" in said and "only on this machine" in said


def test_this_machine_needs_a_token_too_unless_told_otherwise(http, sidecar):
    said = refused_to_start(sidecar, "--http", f"127.0.0.1:{free_port()}")
    assert "--token-file" in said and "--no-token" in said
    server = http("--no-token")
    assert post(server.url) == 200


# A web page the person opens cannot reach a server with no token on this
# machine by giving its own name for it.
def test_a_server_with_no_token_answers_only_to_this_machines_names(http):
    server = http("--no-token")
    assert post(server.url, {"Host": f"evil.example:{server.port}"}) == 421
    assert post(server.url, {"Origin": "http://evil.example"}) == 403


def test_a_token_file_others_can_read_is_refused(sidecar, tmp_path):
    loose = private_file(tmp_path, mode=0o644)
    said = refused_to_start(sidecar, "--http", f"127.0.0.1:{free_port()}", "--token-file", str(loose))
    assert "chmod 600" in said
    short = private_file(tmp_path, text="short")
    said = refused_to_start(sidecar, "--http", f"127.0.0.1:{free_port()}", "--token-file", str(short))
    assert "at least" in said
    said = refused_to_start(sidecar, "--http", f"127.0.0.1:{free_port()}", "--token-file", str(tmp_path / "missing"))
    assert "cannot read" in said
    link = tmp_path / "link"
    os.symlink(private_file(tmp_path), link)
    said = refused_to_start(sidecar, "--http", f"127.0.0.1:{free_port()}", "--token-file", str(link))
    assert "link" in said


def test_the_token_options_go_with_http_only(sidecar, tmp_path):
    assert "--http" in refused_to_start(sidecar, "--no-token")
    assert "--http" in refused_to_start(sidecar, "--token-file", str(private_file(tmp_path)))
    said = refused_to_start(
        sidecar, "--http", f"127.0.0.1:{free_port()}", "--no-token", "--token-file", str(private_file(tmp_path))
    )
    assert "not both" in said
    assert "HOST:PORT" in refused_to_start(sidecar, "--http", "nowhere")


def test_only_the_bearer_token_is_let_in():
    from orvu import _mcp_http

    assert _mcp_http.allowed(f"Bearer {TOKEN}".encode(), TOKEN)
    assert not _mcp_http.allowed(b"Bearer wrong", TOKEN)
    assert not _mcp_http.allowed(None, TOKEN)
    assert not _mcp_http.allowed(b"bearer", TOKEN)


def hello(url, token=TOKEN):
    """Opens a session with a plain request, as a program would, and leaves
    it open. Its id."""
    request = urllib.request.Request(
        url,
        json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                               "clientInfo": {"name": "test", "version": "1"}}}).encode(),
        method="POST",
        headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                 "Authorization": f"Bearer {token}"},
    )
    with urllib.request.urlopen(request, timeout=30) as answer:
        assert answer.status == 200
        return answer.headers["mcp-session-id"]


def ping(url, session, token=TOKEN):
    """Pings in a session. The status it was answered with."""
    request = urllib.request.Request(
        url, json.dumps({"jsonrpc": "2.0", "id": 2, "method": "ping"}).encode(), method="POST",
        headers={"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                 "Authorization": f"Bearer {token}", "mcp-session-id": session,
                 "mcp-protocol-version": "2025-11-25"},
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as answer:
            return answer.status
    except urllib.error.HTTPError as refused:
        return refused.code


# Sessions that do nothing cost nothing that the others wait for.
def test_many_idle_sessions_do_not_slow_the_others(http, tmp_path):
    server = http("--token-file", str(private_file(tmp_path)))
    for _ in range(99):
        hello(server.url)
    mine = hello(server.url)
    took = []
    for _ in range(10):
        started = time.monotonic()
        assert ping(server.url, mine) == 200
        took.append(time.monotonic() - started)
    took.sort()
    assert took[len(took) // 2] < 0.3, took


# A program that went away without ending its session leaves it open. When
# there is no room for another, the one used longest ago that is doing
# nothing makes room, and one in use is never ended for it.
async def test_sessions_left_open_make_room_for_new_ones(http, tmp_path):
    server = http("--token-file", str(private_file(tmp_path)))
    first = hello(server.url)
    async with session(server.url) as client:
        # A session with its stream open, waiting for word of changes.
        listening = hello(server.url)
        headers = {"mcp-session-id": listening, "mcp-protocol-version": "2025-11-25",
                   "Accept": "text/event-stream"}
        async with client.stream("GET", server.url, headers=headers) as stream:
            assert stream.status_code == 200
            abandoned = [hello(server.url) for _ in range(150)]
            assert ping(server.url, abandoned[-1]) == 200
            assert ping(server.url, listening) == 200, "a session in use was ended"
            assert ping(server.url, first) == 404, "the oldest unused session was kept"


# A session is never ended to make room before its own hello is answered,
# however many programs say hello at once.
def test_hellos_at_once_on_a_full_server_are_all_answered(http, tmp_path):
    from concurrent.futures import ThreadPoolExecutor

    server = http("--token-file", str(private_file(tmp_path)))
    for _ in range(100):
        hello(server.url)

    def status():
        try:
            hello(server.url)
            return 200
        except urllib.error.HTTPError as refused:
            return refused.code

    with ThreadPoolExecutor(30) as pool:
        for _ in range(2):
            assert set(pool.map(lambda _: status(), range(30))) == {200}
