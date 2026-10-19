"""The MCP server over HTTP, for an agent's program that is not started by
the server's own user on the same machine: `orvu mcp --http HOST:PORT`.

It speaks MCP's streamable HTTP, at `/mcp`, through the HTTP transport of
the MCP project's own library, which is installed with `orvu[http]`. What is
said over it is answered by the same server as over input and output.

The server acts as the one agent it was started as. Every program that
connects to it, with the token, uses that agent: its name, its tasks, its
inbox and its place on the roster. So the token is as good as the agent's
own name, and it is let in only with it: anything but a bearer token that
matches is refused before it reaches anything else.
"""

import asyncio
import contextlib
import hmac
import ipaddress
import os
import signal
import stat
import sys
import time
import uuid

from ._mcp import _updated

#: The fewest characters a token may have.
SHORTEST_TOKEN = 16

#: How long, in seconds, a session may go without a request before it is
#: ended, and how many sessions may be open at once.
IDLE_SECONDS = 30 * 60
MOST_SESSIONS = 100

#: The largest request taken, in bytes.
LARGEST_REQUEST = 4 * 1024 * 1024

#: The newest version of MCP that starts with a hello. A request that names
#: a version after it is of the newer way, which has no session.
_LAST_WITH_HELLO = "2025-11-25"


class _Stopped(Exception):
    """The program was asked to stop."""


class Refused(Exception):
    """What the command line asked for cannot be done. Carries why."""


def address(given):
    """`HOST:PORT` as a host and a port. The host is an IP address, in
    brackets for IPv6, or `localhost`."""
    host, colon, port = given.rpartition(":")
    if not colon or not port.isdigit() or not 0 < int(port) < 65536 or not host:
        raise Refused(f"--http takes HOST:PORT, such as 127.0.0.1:8765, not {given!r}")
    if host.startswith("[") and host.endswith("]"):
        host = host[1:-1]
    if host == "localhost":
        host = "127.0.0.1"
    try:
        ipaddress.ip_address(host)
    except ValueError:
        raise Refused(f"--http takes an IP address or localhost as its HOST, not {host!r}") from None
    return host, int(port)


def on_this_machine(host):
    """True if only programs on this machine can reach `host`."""
    return ipaddress.ip_address(host).is_loopback


def read_token(path):
    """The token in the file at `path`. The file has to be a plain file of
    this user's that nobody else can read or change, which is what keeps the
    token the user's own."""
    try:
        # Not followed if it is a link: a link could be swapped for another
        # file after it was looked at.
        handle = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | getattr(os, "O_CLOEXEC", 0))
    except OSError as error:
        if os.path.islink(path):
            raise Refused(f"the token file {path} is a link: give the file itself") from None
        raise Refused(f"cannot read the token file {path}: {error.strerror}") from None
    with os.fdopen(handle, "rb") as file:
        found = os.fstat(file.fileno())
        if not stat.S_ISREG(found.st_mode):
            raise Refused(f"the token file {path} is not a plain file")
        if found.st_uid != os.getuid():
            raise Refused(f"the token file {path} belongs to another user: it has to be yours")
        if found.st_mode & 0o077:
            raise Refused(
                f"others can read or change the token file {path}: make it yours alone with `chmod 600 {path}`"
            )
        text = file.read(4096)
    try:
        token = text.decode("ascii").strip()
    except UnicodeDecodeError:
        raise Refused(f"the token in {path} has to be plain letters, digits and signs") from None
    if len(token) < SHORTEST_TOKEN or any(not 0x21 <= ord(c) <= 0x7E for c in token):
        raise Refused(
            f"the token in {path} has to be at least {SHORTEST_TOKEN} letters, digits and signs, with no spaces"
        )
    return token


def settle(http, token_file, no_token):
    """What `--http`, `--token-file` and `--no-token` ask for, as the host,
    the port, and the token or `None`. Raises `Refused` with why if it
    cannot be done."""
    if http is None:
        if token_file is not None or no_token:
            raise Refused("--token-file and --no-token go with --http")
        return None
    if token_file is not None and no_token:
        raise Refused("give --token-file or --no-token, not both")
    host, port = address(http)
    if token_file is None:
        if not on_this_machine(host):
            if no_token:
                raise Refused(
                    "--no-token is allowed only on this machine, such as 127.0.0.1: anyone who can reach "
                    f"{host} could use the agent. Give a token with --token-file"
                )
            raise Refused(
                f"serving on {host} lets other machines in, so it needs a token: give a file that holds one "
                "with --token-file"
            )
        if not no_token:
            raise Refused(
                "give a token with --token-file, so that only programs that have it can use the agent. "
                "Other users of this machine could reach the server too. --no-token serves with none"
            )
        return host, port, None
    return host, port, read_token(token_file)


def allowed(header, token):
    """True if the `Authorization` header, as bytes, gives `token`. The two
    are compared in a time that does not depend on how much of them agrees,
    so the token cannot be guessed a character at a time."""
    if header is None:
        return False
    scheme, _, given = header.partition(b" ")
    if scheme.lower() != b"bearer":
        return False
    return hmac.compare_digest(given.strip(), token.encode())


def serve(host, port, token, agent, make_server):
    """Serves MCP over HTTP at `host` and `port` until the program is asked
    to stop. `make_server()` gives the server for each new session, on the
    shared `agent`."""
    try:
        import anyio
        import uvicorn
        from mcp.server.streamable_http import MCP_SESSION_ID_HEADER, StreamableHTTPServerTransport
        from mcp.server.transport_security import RequestBodyLimitMiddleware, TransportSecuritySettings
        from mcp.shared.message import SessionMessage
        from mcp_types import jsonrpc_message_adapter
        from starlette.responses import JSONResponse, Response
    except ImportError:
        sys.exit(
            "orvu mcp: serving over HTTP needs the MCP library. Install it with: pip install 'orvu[http]'"
        )

    if on_this_machine(host):
        # A page in the person's browser could otherwise reach a server on
        # this machine through a name of its own that it points here.
        named = [f"{name}:{port}" for name in ("127.0.0.1", "localhost", "[::1]")]
        security = TransportSecuritySettings(
            enable_dns_rebinding_protection=True,
            allowed_hosts=named,
            allowed_origins=[f"http://{name}" for name in named],
        )
    else:
        # Elsewhere the token keeps everyone else out, and the server may
        # be reached by any name.
        security = TransportSecuritySettings(enable_dns_rebinding_protection=False)

    sessions = {}
    # For each session, how many of its requests are being answered now, an
    # open stream among them, and when it was last used.
    busy = {}
    used = {}
    stateless = make_server()
    tasks = None

    def to_message(answer):
        return SessionMessage(jsonrpc_message_adapter.validate_python(answer))

    async def tell(server, write):
        # Word of the resources that changed, on the stream the program
        # opened for it. The server owes at most one word per resource, so
        # a program that is slow to read makes nothing pile up here. A
        # session waits for word without holding a thread: the server wakes
        # it, and the threads are kept for answering.
        loop = asyncio.get_running_loop()
        woken = asyncio.Event()

        def wake():
            try:
                loop.call_soon_threadsafe(woken.set)
            except RuntimeError:
                pass  # the loop has ended, and the session with it

        server.listen(wake)
        # Anything owed from before it listened.
        woken.set()
        while not server.closed:
            await woken.wait()
            woken.clear()
            for uri in server.changed(0):
                await write.send(to_message(_updated(uri)))

    async def answer(server, read, write):
        async for message in read:
            if isinstance(message, Exception):
                continue
            asked = message.message.model_dump(mode="json", by_alias=True, exclude_unset=True)
            reply = await anyio.to_thread.run_sync(server.reply, asked)
            if reply is not None:
                await write.send(to_message(reply))

    async def run_session(transport, server, *, task_status=anyio.TASK_STATUS_IGNORED):
        try:
            async with transport.connect() as (read, write):
                task_status.started()
                idle = transport.idle_scope or anyio.CancelScope()
                with idle:
                    async with anyio.create_task_group() as both:
                        both.start_soon(tell, server, write)
                        await answer(server, read, write)
                        both.cancel_scope.cancel()
        except Exception as error:  # noqa: BLE001
            # One session that fails, such as one whose program went away
            # while it was told something, ends alone. The others go on.
            print(f"orvu mcp: a session ended: {type(error).__name__}: {error}", file=sys.stderr, flush=True)
        finally:
            sessions.pop(transport.mcp_session_id, None)
            busy.pop(transport.mcp_session_id, None)
            used.pop(transport.mcp_session_id, None)
            with anyio.CancelScope(shield=True):
                if not transport.is_terminated:
                    await transport.terminate()
                await anyio.to_thread.run_sync(server.close)

    async def one_request(scope, receive, send):
        """A request of the newer way, which has no session."""
        transport = StreamableHTTPServerTransport(None, is_json_response_enabled=True, security_settings=security)
        async with transport.connect() as (read, write), anyio.create_task_group() as both:
            both.start_soon(answer, stateless, read, write)
            try:
                await transport.handle_request(scope, receive, send)
            finally:
                both.cancel_scope.cancel()
                with anyio.CancelScope(shield=True):
                    await transport.terminate()

    async def handle(transport, scope, receive, send):
        named = transport.mcp_session_id
        busy[named] = busy.get(named, 0) + 1
        used[named] = time.monotonic()
        try:
            await transport.handle_request(scope, receive, send)
        finally:
            if named in busy:
                busy[named] -= 1
                used[named] = time.monotonic()

    async def make_room():
        """Ends the session used longest ago that is doing nothing, as one
        whose program went away without ending it would be. A session that
        is answering, or has its stream open, is never ended for this.
        False if every one is in use."""
        idle = [named for named in sessions if not busy.get(named)]
        if not idle:
            return False
        oldest = min(idle, key=lambda named: used.get(named, 0))
        transport = sessions.pop(oldest)
        with anyio.CancelScope(shield=True):
            await transport.terminate()
        return True

    async def mcp_request(scope, receive, send):
        headers = dict(scope["headers"])
        session = headers.get(MCP_SESSION_ID_HEADER.encode())
        if session is not None:
            transport = sessions.get(session.decode("latin-1"))
            if transport is None:
                await JSONResponse({"error": "there is no such session"}, 404)(scope, receive, send)
                return
            await handle(transport, scope, receive, send)
            return
        version = headers.get(b"mcp-protocol-version", b"").decode("latin-1")
        if version > _LAST_WITH_HELLO:
            await one_request(scope, receive, send)
            return
        if len(sessions) >= MOST_SESSIONS and not await make_room():
            await JSONResponse({"error": "too many sessions are open"}, 503)(scope, receive, send)
            return
        transport = StreamableHTTPServerTransport(
            uuid.uuid4().hex, is_json_response_enabled=True, security_settings=security, idle_timeout=IDLE_SECONDS
        )
        named = transport.mcp_session_id
        # In use from the start, before anything is waited for, so that it
        # cannot be ended to make room for another before its own hello is
        # answered.
        busy[named], used[named] = 1, time.monotonic()
        sessions[named] = transport
        answered = None

        async def watched(message):
            nonlocal answered
            if message["type"] == "http.response.start":
                answered = message["status"]
            await send(message)

        try:
            await tasks.start(run_session, transport, make_server())
            await handle(transport, scope, receive, watched)
        finally:
            if named in busy:
                busy[named] -= 1
                used[named] = time.monotonic()
            # Only a hello opens a session. One that was refused, or failed,
            # opened none, and is not kept waiting for its next request.
            if answered is None or answered >= 400:
                with anyio.CancelScope(shield=True):
                    await transport.terminate()

    async def app(scope, receive, send):
        nonlocal tasks
        if scope["type"] == "lifespan":
            async with anyio.create_task_group() as running:
                tasks = running
                await receive()
                await send({"type": "lifespan.startup.complete"})
                await receive()
                for transport in list(sessions.values()):
                    await transport.terminate()
                running.cancel_scope.cancel()
            await send({"type": "lifespan.shutdown.complete"})
            return
        if scope["type"] != "http":
            return
        # Nothing is answered to a program without the token, not even
        # whether the path is right.
        if token is not None and not allowed(dict(scope["headers"]).get(b"authorization"), token):
            refused = Response("a bearer token is needed", 401, headers={"WWW-Authenticate": "Bearer"})
            await refused(scope, receive, send)
            return
        if scope["path"].rstrip("/") != "/mcp":
            await Response("MCP is served at /mcp", 404)(scope, receive, send)
            return
        await mcp_request(scope, receive, send)

    print(f"orvu mcp: serving MCP at http://{_shown(host)}:{port}/mcp", file=sys.stderr, flush=True)
    config = uvicorn.Config(
        RequestBodyLimitMiddleware(app, LARGEST_REQUEST),
        host=host,
        port=port,
        lifespan="on",
        log_level="warning",
        access_log=False,
        timeout_graceful_shutdown=5,
    )
    # The HTTP server stops on Ctrl-C or SIGTERM, and then passes the signal
    # on to what was there before it. That is this, so the agent is still
    # taken off the roster and the program ends as asked.
    def stopped(*_):
        raise _Stopped

    for asked in (signal.SIGINT, signal.SIGTERM):
        signal.signal(asked, stopped)
    try:
        uvicorn.Server(config).run()
    except _Stopped:
        pass
    finally:
        with contextlib.suppress(Exception):
            stateless.close()
        agent.close()


def _shown(host):
    return f"[{host}]" if ":" in host else host
