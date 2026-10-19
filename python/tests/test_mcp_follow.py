"""The MCP server following changes for an agent that subscribed, run in this
program so that what it leaves behind can be seen."""

import json
import threading

import orvu
from conftest import wait_until
from orvu import _mcp

URIS = ("orvu://inbox", "orvu://tasks", "orvu://notes")


def server_for(sidecar):
    server = _mcp.Server(lambda: orvu.Team.connect(socket=sidecar.socket, me="alice"), "default", "alice")
    asked = 0

    def ask(method, **params):
        nonlocal asked
        asked += 1
        return json.loads(server.answer(json.dumps({"jsonrpc": "2.0", "id": asked, "method": method, "params": params})))

    ask("initialize", protocolVersion="2025-11-25", capabilities={})
    return server, ask


def others(before):
    """The threads running now that were not running before."""
    return [thread for thread in threading.enumerate() if thread not in before and thread.is_alive()]


def test_unsubscribing_lets_go_of_what_followed_the_changes(sidecar):
    before = set(threading.enumerate())
    server, ask = server_for(sidecar)
    for uri in URIS:
        assert ask("resources/subscribe", uri=uri)["result"] == {}
    assert others(before), "nothing follows the changes"
    for uri in URIS:
        assert ask("resources/unsubscribe", uri=uri)["result"] == {}
    assert wait_until(lambda: not others(before)), others(before)
    server.close()


def test_closing_the_server_lets_go_of_what_followed_the_changes(sidecar):
    before = set(threading.enumerate())
    server, ask = server_for(sidecar)
    for uri in URIS:
        ask("resources/subscribe", uri=uri)
    server.close()
    assert wait_until(lambda: not others(before)), others(before)


# A program that does not read what the server writes is owed one word for
# each resource that changed, however many changes there were. Nothing piles
# up for it.
def test_a_program_that_does_not_read_is_owed_one_word_per_resource(sidecar, team):
    server, ask = server_for(sidecar)
    ask("resources/subscribe", uri="orvu://notes")
    ask("resources/subscribe", uri="orvu://tasks")
    for n in range(30):
        team.notes.put("count", n)
    team.tasks.add("Draft")
    owed = []
    assert wait_until(lambda: owed.extend(server.changed(0.1)) or set(owed) == {"orvu://notes", "orvu://tasks"})
    assert len(owed) <= 4, owed
    server.close()


# A fault of ours while a resource is read is answered as an error, and the
# server carries on.
def test_a_resource_that_cannot_be_read_is_answered_with_an_error():
    class Notes:
        def all(self):
            raise RuntimeError("the notes broke")

    class Team:
        me = "alice"
        notes = Notes()

        def close(self):
            pass

    server = _mcp.Server(Team, "default", "alice")
    asked = json.loads(server.answer(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "resources/read",
                                                 "params": {"uri": "orvu://notes"}})))
    assert asked["error"]["code"] == -32603 and "the notes broke" in asked["error"]["message"]
    assert json.loads(server.answer('{"jsonrpc": "2.0", "id": 2, "method": "ping"}'))["result"] == {}
    server.close()


# Each subscription follows on a connection of its own, which is closed when
# it ends, so subscribing and unsubscribing again and again leaves none open.
def test_every_connection_a_subscription_opened_is_closed(sidecar, monkeypatch):
    from orvu import _client

    opened, closed = [], []
    another, close = _client.Client.another, _client.Client.close

    def counted_another(self):
        made = another(self)
        opened.append(made)
        return made

    def counted_close(self):
        if any(self is made for made in opened):
            closed.append(self)
        return close(self)

    monkeypatch.setattr(_client.Client, "another", counted_another)
    monkeypatch.setattr(_client.Client, "close", counted_close)
    server, ask = server_for(sidecar)
    for _ in range(3):
        for uri in URIS:
            ask("resources/subscribe", uri=uri)
        for uri in URIS:
            ask("resources/unsubscribe", uri=uri)
    assert len(opened) == 9
    assert wait_until(lambda: len(closed) == len(opened)), (len(closed), len(opened))
    server.close()
