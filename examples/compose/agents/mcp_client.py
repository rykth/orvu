"""A small MCP client, with no model behind it.

It speaks MCP's streamable HTTP to `orvu mcp --http`: a hello, then tool
calls, each a JSON-RPC request in a POST with the bearer token. The server
answers with JSON, or with a short stream of server-sent events that holds
the answer. This is what an agent program does for its model; here the
calls are written out in the story instead.
"""

import itertools
import json
import urllib.error
import urllib.request

VERSION = "2025-06-18"


class Refused(Exception):
    """The server answered with an HTTP error. `status` says which."""

    def __init__(self, status, text):
        super().__init__(f"HTTP {status}: {text}")
        self.status = status


class Client:
    def __init__(self, url, token):
        self.url = url
        self.token = token
        self.session = None
        self._ids = itertools.count(1)

    def _post(self, message):
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        if self.token is not None:
            headers["Authorization"] = f"Bearer {self.token}"
        if self.session is not None:
            headers["Mcp-Session-Id"] = self.session
            headers["MCP-Protocol-Version"] = VERSION
        request = urllib.request.Request(self.url, json.dumps(message).encode(), headers, method="POST")
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                self.session = response.headers.get("Mcp-Session-Id") or self.session
                kind = response.headers.get("Content-Type", "")
                body = response.read().decode()
        except urllib.error.HTTPError as error:
            raise Refused(error.code, error.read().decode().strip()) from None
        if not body:
            return None
        if kind.startswith("text/event-stream"):
            # The answer is the event whose data is the reply to this request.
            for line in body.splitlines():
                if line.startswith("data:"):
                    said = json.loads(line[len("data:") :])
                    if said.get("id") == message.get("id"):
                        return said
            raise RuntimeError(f"no answer in the stream: {body!r}")
        return json.loads(body)

    def _ask(self, method, params):
        answer = self._post({"jsonrpc": "2.0", "id": next(self._ids), "method": method, "params": params})
        if "error" in answer:
            raise RuntimeError(f"{method}: {answer['error']}")
        return answer["result"]

    def hello(self):
        """Says hello, and returns what the server said about itself."""
        result = self._ask(
            "initialize",
            {"protocolVersion": VERSION, "capabilities": {}, "clientInfo": {"name": "story", "version": "1"}},
        )
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return result

    def tools(self):
        return [tool["name"] for tool in self._ask("tools/list", {})["tools"]]

    def call(self, tool, **arguments):
        """Calls a tool, and returns its answer as JSON."""
        result = self._ask("tools/call", {"name": tool, "arguments": arguments})
        if result.get("isError"):
            raise RuntimeError(f"{tool}: {result['content'][0]['text']}")
        if "structuredContent" in result:
            return result["structuredContent"]
        return json.loads(result["content"][0]["text"])
