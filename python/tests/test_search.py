"""Search by meaning. The sidecars here use a made-up model that knows a few
dozen words in groups, such as refund, money and back, and deploy, ship and
friday: words of one group mean the same, and words of different groups have
nothing in common."""

import signal
import socket
import threading
import time

import pytest

import orvu
from conftest import orvud_bin, wait_until


def found(team, query, limit=5):
    return [(f.kind, f.name) for f in team.search(query, limit)]


# Notes, structured notes and documents are found by what they mean, the
# closest first, and each says what it is and where.
def test_notes_and_documents_are_found_by_meaning(team):
    team.notes.put("refund-policy", "customers get their money back")
    team.notes.put("deploy-date", "friday")
    team.notes.structured("q3").set(["sales"], "rose")
    team.document("handbook").append("Ship on friday.\n\nA refund is paid, money back, repay.")
    assert found(team, "refund") == [("document", "handbook"), ("note", "refund-policy")]
    assert found(team, "income") == [("structured", "q3")]
    first = team.search("refund", 1)[0]
    assert first == orvu.Found("document", "handbook", "A refund is paid, money back, repay.", first.score, 2)
    assert 0 < first.score <= 1
    assert team.search("refund")[1].text == "refund-policy: customers get their money back"
    assert team.search("the cat") == []


# A note written after the team was first searched is found within a moment,
# and one that was removed is no longer found.
def test_changes_are_found_within_a_moment(team):
    assert team.search("refund") == []
    team.notes.put("refund-policy", "money back")
    assert wait_until(lambda: found(team, "refund") == [("note", "refund-policy")])
    team.notes.remove("refund-policy")
    assert wait_until(lambda: found(team, "refund") == [])


# What one agent writes, another finds.
def test_what_one_agent_writes_another_finds(team, bob):
    team.document("draft").append("We ship on friday")
    assert wait_until(lambda: found(bob, "deploy") == [("document", "draft")])


# An agent with rules finds only what it may read.
def test_an_agent_finds_only_what_it_may_read(sidecar, team):
    team.notes.put("refund-policy", "money back")
    team.document("handbook").append("refund")
    key = sidecar.add_agent("reader")
    sidecar.allow("reader", "documents:read")
    with orvu.Team.connect(socket=sidecar.socket, key=key) as reader:
        assert found(reader, "refund") == [("document", "handbook")]


# At most as many are found as are asked for, five unless it says, and from
# one to fifty may be asked for.
def test_at_most_as_many_are_found_as_are_asked_for(team):
    for n in range(8):
        team.notes.put(f"refund-{n}", "money back")
    assert len(team.search("refund")) == 5
    assert len(team.search("refund", 2)) == 2
    assert len(team.search("refund", 50)) == 8
    for wrong in [0, 51, True, "5", 2.0]:
        with pytest.raises(orvu.BadRequest, match="from 1 to 50"):
            team.search("refund", wrong)
    with pytest.raises(orvu.BadRequest, match="some text"):
        team.search(None)


# The plain client gives what was found as dicts.
def test_the_plain_client_searches_a_team_by_its_name(client, team):
    team.notes.put("refund-policy", "money back")
    [hit] = client.search(team.name, "refund", 1)
    assert hit == {"kind": "note", "name": "refund-policy", "text": "refund-policy: money back", "score": 1.0}
    with pytest.raises(orvu.BadRequest):
        client.search("no/slashes", "refund")


# The first search may fetch the model, which takes longer than other calls
# may, so a search waits longer than they do: ten minutes.
def test_a_search_waits_longer_than_other_calls(client, team):
    waited = []

    class Watched:
        def __init__(self, stub):
            self.stub = stub

        def Execute(self, request, timeout):
            waited.append((request.WhichOneof("op"), timeout))
            return self.stub.Execute(request, timeout=timeout)

    client._stub = Watched(client._stub)
    client.map("m").get("k")
    client.search(team.name, "refund")
    assert waited == [("map_get", 10.0), ("search", 600.0)]


class Silent:
    """A web server on this machine that takes connections and never
    answers, in place of the one the model is fetched from."""

    def __init__(self):
        self.server = socket.socket()
        self.server.bind(("127.0.0.1", 0))
        self.server.listen(16)
        self.held = []
        self.taking = threading.Thread(target=self._take, daemon=True)
        self.taking.start()

    def _take(self):
        while True:
            try:
                self.held.append(self.server.accept()[0])
            except OSError:
                return

    @property
    def source(self):
        return f"http://127.0.0.1:{self.server.getsockname()[1]}/"

    def close(self):
        # Whatever is taken while closing is closed too.
        self.server.close()
        self.taking.join(timeout=5)
        for held in self.held:
            held.close()


def fetching(sidecar, monkeypatch, source):
    """The sidecar started again with no model, fetching it from `source`."""
    sidecar.stop()
    monkeypatch.delenv("ORVU_MODEL")
    monkeypatch.setenv("ORVU_MODEL_SOURCE", source)
    sidecar.launch([orvud_bin(), "--local", "--data-dir", sidecar.data, "--socket", sidecar.socket])


# A sidecar asked to stop while it fetches the model stops at once, and does
# not wait for the fetch.
def test_a_sidecar_fetching_the_model_stops_when_asked(sidecar, monkeypatch):
    silent = Silent()
    try:
        fetching(sidecar, monkeypatch, silent.source)
        with orvu.Team.connect(socket=sidecar.socket) as team:
            team.notes.put("refund-policy", "money back")

            def ask():
                try:
                    team.search("refund")
                except orvu.OrvuError:
                    pass

            threading.Thread(target=ask, daemon=True).start()
            assert wait_until(lambda: silent.held)
            running = sidecar.process
            started = time.monotonic()
            running.send_signal(signal.SIGTERM)
            try:
                running.wait(timeout=15)
            finally:
                if running.poll() is None:
                    running.kill()
                    running.wait()
            # Without the fix it waits for the fetch, minutes; a sidecar that
            # stops as it should takes a few seconds even on a busy machine.
            assert time.monotonic() - started < 10
            sidecar.process = None
    finally:
        silent.close()


# A model that could not be fetched is not tried again on every search: for
# a minute, a search is told why at once. The port is kept bound and never
# listened on, so nothing else can take it and every connection is refused.
def test_a_model_that_could_not_be_fetched_is_not_fetched_again_at_once(sidecar, monkeypatch):
    nobody = socket.socket()
    nobody.bind(("127.0.0.1", 0))
    try:
        fetching(sidecar, monkeypatch, f"http://127.0.0.1:{nobody.getsockname()[1]}/")
        with orvu.Team.connect(socket=sidecar.socket) as team:
            team.notes.put("refund-policy", "money back")
            with pytest.raises(orvu.SearchUnavailable, match="cannot fetch") as first:
                team.search("refund")
            with pytest.raises(orvu.SearchUnavailable) as again:
                team.search("refund")
            assert str(first.value) in str(again.value) and "again" in str(again.value)
    finally:
        nobody.close()


class Refusing(Silent):
    """A web server that closes each connection at once, and counts them."""

    def _take(self):
        while True:
            try:
                taken = self.server.accept()[0]
            except OSError:
                return
            self.held.append(None)
            taken.close()

    def close(self):
        self.server.close()
        self.taking.join(timeout=5)


# Searches that wait for the model together fetch it once, and are all told
# why, when it cannot be fetched.
def test_searches_that_wait_together_fetch_the_model_once(sidecar, monkeypatch):
    refusing = Refusing()
    try:
        fetching(sidecar, monkeypatch, refusing.source)
        with orvu.Team.connect(socket=sidecar.socket) as team:
            team.notes.put("refund-policy", "money back")
        told = []

        def ask():
            with orvu.Team.connect(socket=sidecar.socket) as mine:
                try:
                    mine.search("refund")
                except orvu.SearchUnavailable as why:
                    told.append(str(why))

        asking = [threading.Thread(target=ask) for _ in range(5)]
        for each in asking:
            each.start()
        for each in asking:
            each.join(timeout=60)
        assert len(told) == 5
        assert len(refusing.held) == 1, f"the model was fetched {len(refusing.held)} times"
    finally:
        refusing.close()


# A sidecar whose model cannot be read says so, and why.
def test_a_search_that_cannot_be_done_says_why(sidecar, tmp_path):
    sidecar.stop()
    sidecar.start("--model", tmp_path / "no-model-here")
    with orvu.Team.connect(socket=sidecar.socket) as team:
        # A team that has nothing needs no model to find nothing.
        assert team.search("refund") == []
        team.notes.put("refund-policy", "money back")
        with pytest.raises(orvu.SearchUnavailable, match="no-model-here"):
            team.search("refund")


# Tasks are found by their titles and notes, each with its id as its name and
# its title.
def test_tasks_are_found_by_their_titles_and_notes(team):
    pay = team.tasks.add("Pay the refund")
    write = team.tasks.add("Write the summary", notes="deploy on friday")
    assert found(team, "refund") == [("task", pay.id)]
    [hit] = team.search("ship")
    assert hit == orvu.Found("task", write.id, "Write the summary: deploy on friday", hit.score, title="Write the summary")
    team.tasks.update(pay, notes="ship it")
    assert wait_until(lambda: sorted(found(team, "ship")) == sorted([("task", pay.id), ("task", write.id)]))


# An agent that may not read tasks does not find them.
def test_tasks_are_found_only_by_an_agent_that_may_read_them(sidecar):
    with orvu.Team.connect("crew", socket=sidecar.socket) as writer:
        writer.tasks.add("Pay the refund")
        writer.notes.put("plan", "money back")
        key = sidecar.add_agent("reader")
        sidecar.allow("reader", "notes:read")
        with orvu.Team.connect("crew", socket=sidecar.socket, key=key) as reader:
            assert found(reader, "refund") == [("note", "plan")]
