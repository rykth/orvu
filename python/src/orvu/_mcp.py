"""An MCP server: it lets an agent use what a team shares as tools, with no
code written for it. It also offers the agent's inbox, the team's tasks and
its notes as resources the agent's program can read and subscribe to, and a
few prompts.

The agent's program starts this as a child program and talks to it over its
input and output, one JSON message a line. That is all of MCP this needs, so
it is written out here and no MCP library is installed with the package.
Over HTTP, `_mcp_http` passes what it is sent to the same `Server`, through
the MCP library's own transport.

MCP has two ways of connecting. In the older one the program says hello
first, with `initialize`, and names a version. In the newer one there is no
hello: each request carries its version and what the program can do, in
`params._meta`, and a program may first ask with `server/discover` what the
server speaks. Both are answered here, request by request.
"""

import hashlib
import json
import os
import secrets
import signal
import sys
import threading

from . import _resources, _tools, errors
from ._team import Team
from ._tools import TOOLS
from ._version import VERSION

#: The versions of MCP that start with a hello, oldest first.
PROTOCOLS = ("2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25")

#: The versions of MCP with no hello, where each request says its version.
NEWER_PROTOCOLS = ("2026-07-28",)

# Where a request of the newer kind says its version and what the program
# can do, and where an answer says which server gave it.
_VERSION_KEY = "io.modelcontextprotocol/protocolVersion"
_CLIENT_CAN_KEY = "io.modelcontextprotocol/clientCapabilities"
_SERVER_KEY = "io.modelcontextprotocol/serverInfo"

# The error numbers JSON-RPC, which MCP is built on, gives these mistakes.
_NOT_JSON = -32700
_NOT_A_REQUEST = -32600
_NO_SUCH_METHOD = -32601
_BAD_PARAMS = -32602
_BROKE = -32603
# The number MCP gives a version the server does not speak.
_NO_SUCH_VERSION = -32022
# The number MCP gives a resource that is not there.
_NO_SUCH_RESOURCE = -32002

#: What the server can do, as it says in its hello. Subscribing to a
#: resource is part of the older way only.
_CAN = {"tools": {}, "resources": {"subscribe": True}, "prompts": {}}
_CAN_NEWER = {"tools": {}, "resources": {}, "prompts": {}}


class _Refused(Exception):
    """A request that cannot be answered. It carries the error to send back."""

    def __init__(self, code, message, data=None):
        super().__init__(message)
        self.code = code
        self.data = data


def everyone_refused(name, keyed=False):
    """What an agent called "everyone" is told."""
    whose = "this agent's key was given the name" if keyed else "an agent cannot be called"
    fix = "Give the agent a key under another name, with `orvud agent add`" if keyed else "Give it another name with --me"
    return (
        f"{whose} {name!r}: the tools read the word \"everyone\" as all the agents, so no message could reach "
        f"this one by its name. {fix}"
    )


class Agent:
    """One agent's connection to its team. Over HTTP, the servers of all the
    programs connected share one. `connect` is called for the team the first
    time it is needed, and again after a failure to connect.

    With `joins`, the agent is put on the team's roster once connected. If
    that fails, it is tried again each time the team is needed, and whoami
    and roster_list say so until it works."""

    def __init__(self, connect, joins=False):
        self._connect = connect
        self._team = None
        self._joins = joins
        self._on_roster = False
        #: Why the agent could not be put on the roster, while it is not on it.
        self.roster_trouble = None
        self._said_refused = False
        #: Held while the team is used. Its objects are used from one thread
        #: at a time.
        self.lock = threading.RLock()

    def close(self):
        with self.lock:
            if self._team is not None:
                self._team.close()
                self._team = None

    def ready(self):
        """The team, connected, with this agent on its roster if it can be."""
        with self.lock:
            if self._team is None:
                team = self._connect()
                # With a key, the agent's name is known only now.
                if _tools.means_everyone(team.me):
                    team.close()
                    refused = everyone_refused(team.me, keyed=True)
                    if not self._said_refused:
                        print(f"orvu mcp: {refused}", file=sys.stderr, flush=True)
                        self._said_refused = True
                    raise errors.BadRequest(refused)
                self._team = team
            if self._joins and not self._on_roster and self._team.me is not None:
                try:
                    self._team.roster.join()
                except errors.OrvuError as error:
                    # Being on the roster is a courtesy to the others. Not
                    # getting on it is no reason for this agent's tools to
                    # fail. It is tried again on the next call.
                    self.roster_trouble = f"{type(error).__name__}: {error}"
                else:
                    self._on_roster, self.roster_trouble = True, None
            return self._team


class Server:
    """The server for one program connected to an agent. `connect` is called
    for the team the first time it is needed, and again after a failure to
    connect. With `agent`, an `Agent` the server shares with others, that
    is used instead, and closing the server leaves it open.

    With `joins`, the agent is put on the team's roster once connected.
    `groups` are the groups of tools to offer, as `_tools.GROUPS` names
    them, or `None` for all."""

    def __init__(self, connect, team_name, me, allow_top_up=False, groups=None, joins=False, agent=None):
        self._owns_agent = agent is None
        self._agent = Agent(connect, joins) if agent is None else agent
        # An agent that has run out could give itself more, so the tool that
        # adds to a budget is only there when it was asked for.
        self._tools = {
            name: tool
            for name, tool in TOOLS.items()
            if (allow_top_up or name != "budget_top_up")
            and (groups is None or _tools.group(name) is None or _tools.group(name) in groups)
        }
        self._team_name = team_name
        self._me = me
        offered = {_tools.group(name) for name in self._tools}
        self._resources = {uri: found for uri, found in _resources.RESOURCES.items() if found.group in offered}
        self._prompts = {name: found for name, found in _resources.PROMPTS.items() if found.group in offered}
        #: What follows each resource subscribed to, by its uri.
        self._following = {}
        #: The resources that changed since the program was last told. A
        #: set, so a program that does not read is owed one word for each
        #: resource however often it changed, and nothing piles up.
        self._owed = set()
        self._owed_changed = threading.Condition()
        self.closed = False
        #: Called, from any thread, when something is owed or the server
        #: closes, if set with `listen`.
        self._listener = None

    def close(self):
        """Stops following every resource and lets go of the team."""
        with self._owed_changed:
            self.closed = True
            following, self._following = list(self._following.values()), {}
            self._owed_changed.notify_all()
            listener = self._listener
        if listener is not None:
            listener()
        for follower in following:
            follower.stop()
        if self._owns_agent:
            self._agent.close()

    def changed(self, timeout=None):
        """The uris of the resources subscribed to that changed since this
        was last asked, each once. Waits up to `timeout` seconds for one, or
        until the server is closed if `None`."""
        with self._owed_changed:
            self._owed_changed.wait_for(lambda: self._owed or self.closed, timeout)
            owed, self._owed = sorted(self._owed), set()
            return owed

    def listen(self, listener):
        """Has `listener()` called, from whatever thread, each time a
        resource changes and when the server closes, so that a program
        waiting for word need not hold a thread to wait with. `changed(0)`
        then says what changed."""
        with self._owed_changed:
            self._listener = listener

    def _changed(self, uri):
        with self._owed_changed:
            # A word from a follower that was just stopped is not passed on.
            if uri not in self._following:
                return
            self._owed.add(uri)
            self._owed_changed.notify_all()
            listener = self._listener
        if listener is not None:
            listener()

    # ---- the messages --------------------------------------------------------

    def answer(self, line):
        """Takes one line the agent's program sent and returns the line to
        send back, or `None` if that message gets no answer."""
        try:
            message = json.loads(line)
        # A line nested too deeply to read is not a `ValueError`.
        except (ValueError, RecursionError):
            return json.dumps(_error(None, _NOT_JSON, "that line is not JSON"))
        if isinstance(message, list):
            # Several messages sent as one. One version of MCP allows it.
            if not message:
                return json.dumps(_error(None, _NOT_A_REQUEST, "a list of messages cannot be empty"))
            replies = [reply for reply in map(self.reply, message) if reply is not None]
            return json.dumps(replies) if replies else None
        reply = self.reply(message)
        return None if reply is None else json.dumps(reply)

    def reply(self, message):
        """The answer to one message, read from JSON already, or `None` if it
        gets none."""
        if not isinstance(message, dict) or not isinstance(message.get("method"), str):
            # An answer to something we asked. We ask nothing, so it is dropped.
            if isinstance(message, dict) and "method" not in message and "id" in message:
                return None
            asked = message.get("id") if isinstance(message, dict) else None
            # Sent back under its id if it has a usable one, so that the
            # program can tell which of its requests this is about.
            if isinstance(asked, bool) or not isinstance(asked, (str, int)):
                asked = None
            return _error(asked, _NOT_A_REQUEST, "a message has to be an object with a method")
        if "id" not in message:
            return None  # a notification: it tells us something and expects nothing
        try:
            result = self._handle(message["method"], message.get("params") or {})
        except _Refused as refused:
            return _error(message["id"], refused.code, str(refused), refused.data)
        except Exception as error:  # noqa: BLE001
            # A fault of ours. The program is told, and the server stays up
            # for the next request.
            return _error(message["id"], _BROKE, f"the server broke: {type(error).__name__}: {error}")
        return {"jsonrpc": "2.0", "id": message["id"], "result": result}

    def _handle(self, method, params):
        if not isinstance(params, dict):
            raise _Refused(_BAD_PARAMS, "params has to be an object")
        # A request of the newer kind carries its version. The hello belongs
        # to the older kind whatever it carries.
        meta = params.get("_meta")
        newer = method == "server/discover" or (
            method != "initialize" and isinstance(meta, dict) and _VERSION_KEY in meta
        )
        if newer:
            _check_newer(meta)
        if method == "initialize":
            return self._hello(params)
        if method == "server/discover":
            result = {
                "supportedVersions": list(NEWER_PROTOCOLS),
                "capabilities": _CAN_NEWER,
                "instructions": self._instructions(),
                # The answer may not be kept for later: it is asked again.
                "ttlMs": 0,
                "cacheScope": "private",
            }
        elif method == "ping":
            result = {}
        elif method == "tools/list":
            result = {
                "tools": [
                    {"name": tool.name, "description": tool.description, "inputSchema": tool.schema()}
                    for tool in self._tools.values()
                ]
            }
            if newer:
                result.update(ttlMs=0, cacheScope="private")
        elif method == "tools/call":
            result = self._call(params)
        elif method in ("resources/list", "resources/templates/list", "prompts/list"):
            result = self._listed(method)
            if newer:
                result.update(ttlMs=0, cacheScope="private")
        elif method == "resources/read":
            found = self._resource(params)
            with self._agent.lock:
                text = json.dumps(found.read(self._ready_or_refused()), ensure_ascii=False)
            result = {"contents": [{"uri": found.uri, "mimeType": "application/json", "text": text}]}
        elif method in ("resources/subscribe", "resources/unsubscribe") and not newer:
            result = self._subscribe(params) if method == "resources/subscribe" else self._unsubscribe(params)
        elif method == "prompts/get":
            result = self._prompt(params)
        else:
            raise _Refused(_NO_SUCH_METHOD, f"this server has no {method!r}")
        if newer:
            # Every answer of the newer kind says what kind of answer it is,
            # and which server gave it.
            result = {**result, "resultType": "complete", "_meta": {_SERVER_KEY: _SERVER}}
        return result

    def _called(self):
        """What the agent is called, for the instructions. An agent with a
        key has the name the key was given, which is known once it has
        connected."""
        if self._me is not None:
            return repr(self._me)
        return "by the name your key was given (whoami says it)"

    def _hello(self, params):
        asked = params.get("protocolVersion")
        return {
            # The version the program asked for if we speak it, and our
            # newest if not. The program then decides whether that will do.
            "protocolVersion": asked if asked in PROTOCOLS else PROTOCOLS[-1],
            "capabilities": _CAN,
            "serverInfo": _SERVER,
            "instructions": self._instructions(),
        }

    def _instructions(self):
        """What the agent's model is told about these tools. It speaks only
        of the tools it has."""
        has = {_tools.group(name) for name in self._tools}
        said = [
            f"These tools are the shared memory of the team {self._team_name!r}. Other agents use it "
            f"at the same time, and here you are called {self._called()}."
        ]
        first = [what for group, what in (("notes", "the notes"), ("tasks", "the task list")) if group in has]
        if first:
            said.append(f"Read {' and '.join(first)} before you start.")
        if "search" in has:
            said.append("Use search to find what the team already knows about your work.")
        if "tasks" in has:
            said.append(
                "Take a task with task_claim before working on it, call task_still_working now and then "
                "while you work on it, and close it with task_finish."
            )
        if "budgets" in has:
            said.append(
                "Before work that costs something, take the cost out with budget_spend, and do not go ahead if it says no."
            )
        if "inbox" in has:
            said.append(
                "Other agents may write to you: read inbox_read when you start and now and then while you work."
            )
        return " ".join(said)

    # ---- resources and prompts -----------------------------------------------

    def _listed(self, method):
        if method == "resources/templates/list":
            return {"resourceTemplates": []}
        if method == "prompts/list":
            return {"prompts": [{"name": p.name, "description": p.description, "arguments": []} for p in self._prompts.values()]}
        return {
            "resources": [
                {
                    "uri": found.uri,
                    "name": found.name,
                    "title": found.title,
                    "description": found.description,
                    "mimeType": "application/json",
                }
                for found in self._resources.values()
            ]
        }

    def _resource(self, params):
        uri = params.get("uri")
        if not isinstance(uri, str):
            raise _Refused(_BAD_PARAMS, "params.uri has to be the resource's uri, as text")
        found = self._resources.get(uri)
        if found is None:
            raise _Refused(
                _NO_SUCH_RESOURCE,
                f"there is no resource {uri!r}. resources/list gives the ones there are",
                {"uri": uri},
            )
        return found

    def _ready_or_refused(self):
        """The team, connected, or the reason it is not as an error to send
        back. Tools tell their agent; a resource is read by its program."""
        try:
            return self._agent.ready()
        except errors.OrvuError as error:
            raise _Refused(_BROKE, f"{type(error).__name__}: {error}") from None

    def _subscribe(self, params):
        found = self._resource(params)
        team = self._ready_or_refused()
        with self._owed_changed:
            if found.uri in self._following or self.closed:
                return {}
        try:
            follower = _resources.Follower(found, team, lambda: self._changed(found.uri))
        except errors.OrvuError as error:
            raise _Refused(_BROKE, f"{type(error).__name__}: {error}") from None
        with self._owed_changed:
            if found.uri in self._following or self.closed:
                # Subscribed to twice at once, or closed meanwhile.
                spare = follower
            else:
                self._following[found.uri], spare = follower, None
        if spare is not None:
            spare.stop()
        return {}

    def _unsubscribe(self, params):
        uri = self._resource(params).uri
        with self._owed_changed:
            follower = self._following.pop(uri, None)
            self._owed.discard(uri)
        if follower is not None:
            follower.stop()
        return {}

    def _prompt(self, params):
        name = params.get("name")
        found = self._prompts.get(name) if isinstance(name, str) else None
        if found is None:
            raise _Refused(_BAD_PARAMS, f"there is no prompt {name!r}. prompts/list gives the ones there are")
        return {
            "description": found.description,
            "messages": [{"role": "user", "content": {"type": "text", "text": found.text}}],
        }

    # ---- the tools -----------------------------------------------------------

    def _call(self, params):
        name = params.get("name")
        tool = self._tools.get(name) if isinstance(name, str) else None
        if tool is None:
            raise _Refused(_BAD_PARAMS, f"there is no tool {name!r}")
        try:
            given = tool.check(params.get("arguments") or {})
            with self._agent.lock:
                answer = tool.run(self._agent.ready(), **given)
            trouble = self._agent.roster_trouble
            if name in ("whoami", "roster_list") and trouble is not None:
                answer = {
                    **answer,
                    "on_roster": False,
                    "roster_note": (
                        f"you are not on the team's roster, so the other agents do not see you there "
                        f"({trouble}). It is tried again each time you use a tool."
                    ),
                }
            return _said(json.dumps(answer, ensure_ascii=False), answer=answer)
        except errors.OrvuError as error:
            # The agent's model reads this and can act on it, so it goes back
            # as the tool's answer and not as a failed request.
            return _said(f"{type(error).__name__}: {error}", failed=True)
        except Exception as error:  # noqa: BLE001
            # A fault of ours. The agent is told, and the server stays up for
            # the next call.
            return _said(f"the tool broke: {type(error).__name__}: {error}", failed=True)



def _check_newer(meta):
    """Refuses a request of the newer kind that does not say its version and
    what the program can do, or names a version this does not speak."""
    if not isinstance(meta, dict):
        raise _Refused(_BAD_PARAMS, f"params._meta has to be an object with {_VERSION_KEY!r} and {_CLIENT_CAN_KEY!r}")
    missing = [key for key in (_VERSION_KEY, _CLIENT_CAN_KEY) if key not in meta]
    if missing:
        raise _Refused(_BAD_PARAMS, f"params._meta is missing {', '.join(map(repr, missing))}")
    version = meta[_VERSION_KEY]
    if not isinstance(version, str):
        raise _Refused(_BAD_PARAMS, f"{_VERSION_KEY!r} has to be text")
    if version not in NEWER_PROTOCOLS:
        raise _Refused(
            _NO_SUCH_VERSION,
            f"this server does not speak MCP {version!r}",
            {"supported": list(NEWER_PROTOCOLS), "requested": version},
        )


#: Which server this is, as MCP has it said.
_SERVER = {"name": "orvu", "version": VERSION}


def _said(text, failed=False, answer=None):
    said = {"content": [{"type": "text", "text": text}], "isError": failed}
    if answer is not None:
        # The same answer as JSON, for a program that reads it that way. One
        # that does not know of it reads the text.
        said["structuredContent"] = answer
    return said


def _updated(uri):
    """Word to the program that a resource it subscribed to changed."""
    return {"jsonrpc": "2.0", "method": "notifications/resources/updated", "params": {"uri": uri}}


def _error(request_id, code, message, data=None):
    error = {"code": code, "message": message}
    if data is not None:
        error["data"] = data
    return {"jsonrpc": "2.0", "id": request_id, "error": error}


def made_up_name(team_name, folder):
    """A name for an agent that was not given one: the same for the same
    team and folder, so that an agent started again where it was is the
    same agent, and keeps its tasks and its place in the inbox."""
    # Long enough for two made-up names to be the same only by bad luck.
    made = hashlib.sha256(f"{team_name}\n{folder}".encode("utf-8", "surrogateescape")).hexdigest()
    return "agent-" + made[:8]


def _folder():
    """The folder this program was started in."""
    try:
        return os.getcwd()
    except OSError:
        # The folder is gone. A name of its own is the best there is.
        return secrets.token_hex(8)


def _agent(team_name, me, socket):
    """The agent's name, or `None` for one named by its key, and how to
    connect it to its team."""
    # Every agent goes on the team's roster for as long as its server runs.
    # One with a key is named by it, and has its name once it has connected.
    # A connection file in the environment counts only when no socket is
    # given, as for `orvu.connect`.
    keyed = bool(os.environ.get("ORVU_KEY") or (os.environ.get("ORVU_CONNECT") and socket is None))
    me = me or (None if keyed else made_up_name(team_name, _folder()))

    def connect():
        return Team.connect(team_name, me=me, socket=socket)

    return me, connect


def serve(team_name="default", me=None, socket=None, allow_top_up=False, groups=None):
    """Serves one agent's program over this program's input and output, until
    the program closes the input. `groups` are the groups of tools to offer,
    or `None` for all."""
    me, connect = _agent(team_name, me, socket)
    server = Server(connect, team_name, me, allow_top_up, groups, joins=True)
    # The output carries the messages and nothing else. Anything for a person
    # goes to the error output, where the agent's program keeps it as a log.
    agent = repr(me) if me is not None else "named by its key"
    print(f"orvu mcp: team {team_name!r}, agent {agent}, {len(server._tools)} tools", file=sys.stderr, flush=True)
    writing = threading.Lock()

    def write(line):
        with writing:
            sys.stdout.write(line + "\n")
            sys.stdout.flush()

    def tell():
        # Word of the resources that changed, written between answers. A
        # program that is slow to read holds up only this thread, and is
        # owed at most one word for each resource.
        try:
            while not server.closed:
                for uri in server.changed():
                    write(json.dumps(_updated(uri)))
        except (BrokenPipeError, ValueError, OSError):
            # The program has gone. The input ends soon after.
            pass

    teller = threading.Thread(target=tell, name="orvu-mcp-tell", daemon=True)
    teller.start()

    # Many agent programs end the server with SIGTERM. It ends as when its
    # input is closed, and so takes the agent off the roster as it goes.
    def asked_to_stop(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, asked_to_stop)
    try:
        for line in sys.stdin.buffer:
            line = line.decode("utf-8", errors="replace").strip()
            if not line:
                continue
            reply = server.answer(line)
            if reply is not None:
                write(reply)
    except (KeyboardInterrupt, BrokenPipeError):
        pass
    finally:
        server.close()
        teller.join(timeout=5)


def serve_http(host, port, token, team_name="default", me=None, socket=None, allow_top_up=False, groups=None):
    """Serves one agent over HTTP until this program is asked to stop. Every
    program that connects with `token`, or any if it is `None`, uses that
    one agent."""
    from . import _mcp_http

    me, connect = _agent(team_name, me, socket)
    agent = Agent(connect, joins=True)

    def server():
        return Server(connect, team_name, me, allow_top_up, groups, agent=agent)

    named = repr(me) if me is not None else "named by its key"
    print(f"orvu mcp: team {team_name!r}, agent {named}, {len(server()._tools)} tools", file=sys.stderr, flush=True)
    _mcp_http.serve(host, port, token, agent, server)
