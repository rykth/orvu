"""The inbox: messages between the agents of a team."""

import json
import threading
import time

import pytest

import orvu
from conftest import wait_until


def test_a_message_reaches_the_agent_it_was_sent_to(team, bob, sidecar):
    team.inbox.send("bob", "The Q3 numbers are in the notes.")
    (message,) = bob.inbox.unread()
    assert (message.sender, message.to, message.body) == ("alice", "bob", "The Q3 numbers are in the notes.")
    assert message.id.startswith("m-") and message.sent > 0
    # It was for bob. Nobody else is given it.
    assert team.inbox.unread() == []
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        assert carol.inbox.messages() == []


def test_a_message_to_everyone_reaches_everyone(team, bob, sidecar):
    team.inbox.send_all("Deploy is on Friday.")
    for reader in (team, bob):
        (message,) = reader.inbox.unread()
        assert (message.sender, message.to, message.body) == ("alice", None, "Deploy is on Friday.")
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        assert [message.body for message in nameless.inbox.messages()] == ["Deploy is on Friday."]
        nameless.inbox.send("bob", "from nobody in particular")
    assert bob.inbox.unread()[0].sender is None


def test_unread_gives_each_message_once_and_messages_gives_them_all(team, bob):
    team.inbox.send("bob", "one")
    team.inbox.send_all("two")
    assert [message.body for message in bob.inbox.unread()] == ["one", "two"]
    assert bob.inbox.unread() == []
    team.inbox.send("bob", "three")
    assert [message.body for message in bob.inbox.unread()] == ["three"]
    assert [message.body for message in bob.inbox.messages()] == ["one", "two", "three"]


# A message is whatever JSON can hold, and comes back as it was sent.
@pytest.mark.parametrize("body", ["text", 7, 2.5, True, ["a", 1], {"task": "t-1", "done": False}, "ü 🙂"])
def test_a_message_holds_any_value(team, bob, body):
    team.inbox.send("bob", body)
    (message,) = bob.inbox.unread()
    assert message.body == body and type(message.body) is type(body)


@pytest.mark.parametrize("body", [None, {1, 2}, float("nan")])
def test_what_cannot_be_sent_is_refused(team, bob, body):
    with pytest.raises(orvu.BadRequest):
        team.inbox.send("bob", body)
    with pytest.raises(orvu.BadRequest):
        team.inbox.send_all(body)
    assert bob.inbox.messages() == [] and team.index() == {}


# An agent's name is any text, so an inbox takes names a file would not.
def test_an_agent_with_an_unusual_name_gets_its_messages(team, sidecar):
    with orvu.Team.connect(socket=sidecar.socket, me="ålice smith") as other:
        team.inbox.send("ålice smith", "hello")
        assert [message.body for message in other.inbox.unread()] == ["hello"]


# A full page is left behind and a new one is started. Reading looks at the
# newest two pages, and the whole history is still there for who asks.
def test_a_long_inbox_starts_a_new_page(team, bob):
    team.inbox.PAGE = bob.inbox.PAGE = 5
    for number in range(17):
        team.inbox.send("bob", number)
    assert team.client.counter("orvu/default/inbox-page/bob").value() == 3
    assert [team.client.list(f"orvu/default/inbox/bob/{page}").len() for page in range(4)] == [5, 5, 5, 2]
    # Each page counts the messages written to it.
    assert team.client.cart("orvu/default/inbox-count/bob").items() == {"0": 5, "1": 5, "2": 5, "3": 2}
    assert [message.body for message in bob.inbox.messages()] == list(range(10, 17))
    assert [message.body for message in bob.inbox.history()] == list(range(17))
    assert [message.body for message in bob.inbox.unread()] == list(range(10, 17))


def test_nothing_is_lost_when_many_send_at_once(sidecar, team, bob):
    def agent(name):
        with orvu.Team.connect(socket=sidecar.socket, me=name) as mine:
            mine.inbox.PAGE = 10
            for number in range(10):
                mine.inbox.send("bob", f"{name}-{number}")

    agents = [threading.Thread(target=agent, args=(f"agent-{number}",)) for number in range(6)]
    for thread in agents:
        thread.start()
    for thread in agents:
        thread.join()
    bob.inbox.PAGE = 10
    bodies = [message.body for message in bob.inbox.history()]
    assert len(bodies) == 60 and len(set(bodies)) == 60
    # Each sender's own messages are in the order it sent them.
    for number in range(6):
        own = [body for body in bodies if body.startswith(f"agent-{number}-")]
        assert own == [f"agent-{number}-{n}" for n in range(10)]


def test_messages_are_watched_as_they_arrive(team, bob):
    team.inbox.send("bob", "before")
    got = []
    done = threading.Event()

    def listen():
        for message in bob.inbox.watch():
            got.append(message.body)
            if len(got) == 4:
                done.set()
                return

    listener = threading.Thread(target=listen, daemon=True)
    listener.start()
    assert wait_until(lambda: got == ["before"])
    team.inbox.send("bob", "one")
    team.inbox.send_all("two")
    assert wait_until(lambda: got == ["before", "one", "two"])
    team.inbox.send("bob", "three")
    assert done.wait(5) and got == ["before", "one", "two", "three"]
    listener.join(timeout=5)


# The watch goes on when the inbox moves to a new page.
def test_a_watch_follows_the_inbox_onto_a_new_page(team, bob):
    team.inbox.PAGE = bob.inbox.PAGE = 3
    got = []

    def listen():
        for message in bob.inbox.watch():
            got.append(message.body)
            if len(got) == 8:
                return

    listener = threading.Thread(target=listen, daemon=True)
    listener.start()
    for number in range(8):
        team.inbox.send("bob", number)
        assert wait_until(lambda: len(got) == number + 1), (number, got)
    listener.join(timeout=5)
    assert got == list(range(8))


def test_the_index_lists_the_inboxes_that_were_written_to(team):
    team.inbox.messages()
    assert team.index() == {}
    team.inbox.send("bob", "hello")
    team.inbox.send_all("hello")
    assert team.index() == {
        "inbox/bob/0": "list",
        "inbox-count/bob": "cart",
        "inbox/@everyone/0": "list",
        "inbox-count/@everyone": "cart",
    }
    team.inbox.PAGE = 1
    team.inbox.send("bob", "and again")
    assert team.index() == {
        "inbox/bob/0": "list",
        "inbox/bob/1": "list",
        "inbox-page/bob": "counter",
        "inbox-count/bob": "cart",
        "inbox/@everyone/0": "list",
        "inbox-count/@everyone": "cart",
    }


# However many messages arrive between two looks, none is passed over.
def test_a_burst_of_messages_between_two_reads_is_all_given(team, bob):
    team.inbox.PAGE = bob.inbox.PAGE = 5
    for number in range(3):
        team.inbox.send("bob", number)
    assert [message.body for message in bob.inbox.unread()] == [0, 1, 2]
    for number in range(3, 30):
        team.inbox.send("bob", number)
    assert [message.body for message in bob.inbox.unread()] == list(range(3, 30))
    assert bob.inbox.unread() == []


# Two senders that find a page full at the same moment both move on, and a
# page number is skipped. What was unread on the page before must not be
# skipped with it.
def test_a_skipped_page_number_loses_nothing(team, bob):
    team.inbox.PAGE = bob.inbox.PAGE = 5
    for number in range(2):
        team.inbox.send("bob", number)
    assert [message.body for message in bob.inbox.unread()] == [0, 1]
    for number in range(2, 5):
        team.inbox.send("bob", number)
    # The second sender's move, on top of the one the next send makes.
    team.client.counter("orvu/default/inbox-page/bob").increment(2)
    team.inbox.send("bob", 5)
    assert [message.body for message in bob.inbox.unread()] == [2, 3, 4, 5]


# A machine that has not heard of the new page yet still writes on the one
# before. A program that follows the inbox is given that message too.
def test_a_message_on_the_page_before_reaches_a_program_that_follows(team, bob):
    import json

    got = []

    def listen():
        for message in bob.inbox.watch():
            got.append(message.body)
            if len(got) == 3:
                return

    team.inbox.send("bob", "on page 0")
    team.client.counter("orvu/default/inbox-page/bob").increment(1)
    team.inbox.send("bob", "on page 1")
    listener = threading.Thread(target=listen, daemon=True)
    listener.start()
    assert wait_until(lambda: got == ["on page 0", "on page 1"])
    late = {"id": "m-late", "from": "carol", "to": "bob", "body": "late, on page 0", "sent": 2**42}
    team.client.list("orvu/default/inbox/bob/0").insert(1, json.dumps(late))
    assert wait_until(lambda: got == ["on page 0", "on page 1", "late, on page 0"]), got
    listener.join(timeout=5)


# Any name an agent can have works for its inbox. It only has to be text.
@pytest.mark.parametrize("name", ["ops/alice", "@everyone", "x" * 200, "a b", "../x", "%41"])
def test_an_agent_with_any_name_has_an_inbox(team, sidecar, name):
    with orvu.Team.connect(socket=sidecar.socket, me=name) as other:
        team.inbox.send(name, "hello")
        team.inbox.send_all("to all")
        assert [message.body for message in other.inbox.unread()] == ["hello", "to all"]
        assert [message.to for message in other.inbox.messages()] == [name, None]
    # It went to that agent and to nobody else.
    assert [message.body for message in team.inbox.messages()] == ["to all"]


@pytest.mark.parametrize("to", ["", None, 7])
def test_who_a_message_goes_to_is_some_text(team, to):
    with pytest.raises(orvu.BadRequest, match="agent to send to"):
        team.inbox.send(to, "hello")
    assert team.index() == {}


# Something in an inbox that this block did not write is passed over, and
# does not stop the messages around it from being read.
def test_what_is_not_a_message_is_passed_over(team, bob):
    import json

    team.inbox.send("bob", "one")
    page = team.client.list("orvu/default/inbox/bob/0")
    for junk in ["not json", "[1, 2]", json.dumps({"id": "m-x", "body": "no time", "sent": "soon"}), json.dumps({"id": 7, "body": "x", "sent": 1})]:
        page.insert(page.len(), junk)
    team.inbox.send("bob", "two")
    assert [message.body for message in bob.inbox.unread()] == ["one", "two"]


# What an agent has been given is written down for the team, under the
# agent's name. A program that starts again is not given it a second time,
# and would otherwise act on it a second time.
def test_an_agent_that_starts_again_is_not_given_a_message_twice(team, sidecar):
    team.inbox.send("bob", "please redeploy staging")
    team.inbox.send_all("Deploy is on Friday.")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert [message.body for message in bob.inbox.unread()] == ["please redeploy staging", "Deploy is on Friday."]
    team.inbox.send("bob", "and production")
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert [message.body for message in bob.inbox.unread()] == ["and production"]
        assert bob.inbox.unread() == []
        # Everything is still there to be looked at.
        assert len(bob.inbox.messages()) == 3
    # What bob was given says nothing about carol.
    with orvu.Team.connect(socket=sidecar.socket, me="carol") as carol:
        assert [message.body for message in carol.inbox.unread()] == ["Deploy is on Friday."]


# A program with no name has nothing to write it down under. It remembers
# for as long as it runs.
def test_a_program_with_no_name_remembers_only_while_it_runs(team, sidecar):
    team.inbox.send_all("Deploy is on Friday.")
    for _ in range(2):
        with orvu.Team.connect(socket=sidecar.socket) as nameless:
            assert [message.body for message in nameless.inbox.unread()] == ["Deploy is on Friday."]
            assert nameless.inbox.unread() == []
    assert not any(name.startswith("inbox-given/") for name in team.index())


def test_what_was_given_is_written_down_in_the_index(team, bob):
    team.inbox.send("bob", "hello")
    bob.inbox.messages()
    assert not any(name.startswith("inbox-given/") for name in team.index()), "looking is not being given"
    bob.inbox.unread()
    assert team.index()["inbox-given/bob/bob/0"] == "set"
    assert team.index()["inbox-done/bob/bob"] == "map"


# What an agent was given is written down so that it is not given it again
# after a restart. If that cannot be written, `unread` says so, and gives the
# messages again next time, rather than give them now and lose the record.
def test_an_inbox_that_cannot_write_down_what_it_gave_says_so(team, bob):
    team.inbox.send("bob", "hello")
    # The record's name is taken for other rules. Reading it still works,
    # and writing to it is refused.
    team.client.declare("orvu/default/inbox-given/bob/bob/0", "escrow")
    with pytest.raises(orvu.ClassMismatch):
        bob.inbox.unread()
    with pytest.raises(orvu.ClassMismatch):
        bob.inbox.unread()


def remembered(inbox):
    """How many message ids a program holds in memory for an inbox."""
    seen = inbox._seen
    return sum(len(ids) for ids in seen.values()) if isinstance(seen, dict) else len(seen)


# A program that follows a busy inbox for long keeps in memory only what it
# needs for the pages it still looks at.
@pytest.mark.parametrize("named", [True, False])
def test_what_a_program_remembers_does_not_grow_with_the_inbox(team, sidecar, named):
    team.inbox.PAGE = 5
    with orvu.Team.connect(socket=sidecar.socket, me="bob" if named else None) as reader:
        reader.inbox.PAGE = 5
        for number in range(60):
            team.inbox.send("bob", number) if named else team.inbox.send_all(number)
            assert [message.body for message in reader.inbox.unread()] == [number]
        # The two newest pages, and at most one more being finished.
        assert remembered(reader.inbox) <= 15, remembered(reader.inbox)


# A machine that was cut off for long goes on writing to the page it knew,
# after the inbox has moved on by more than a page. The message is given,
# once, and not again to a program that starts again.
def test_a_message_written_late_to_an_old_page_is_given_once(team, sidecar):
    team.inbox.PAGE = 2
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob, orvu.Team.connect(
        socket=sidecar.socket, me="carol"
    ) as carol:
        bob.inbox.PAGE = 2
        for number in range(2):
            team.inbox.send("bob", number)
        assert [message.body for message in bob.inbox.unread()] == [0, 1]
        for number in range(2, 9):
            team.inbox.send("bob", number)
        assert [message.body for message in bob.inbox.unread()] == list(range(2, 9))
        # Carol's machine has not heard of any page after the first.
        carol.inbox._newest = lambda owner: 0
        carol.inbox.send("bob", "late, on page 0")
        assert team.client.list("orvu/default/inbox/bob/0").len() == 3
        assert [message.body for message in bob.inbox.unread()] == ["late, on page 0"]
        assert bob.inbox.unread() == []
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        assert bob.inbox.unread() == []
        assert "late, on page 0" in [message.body for message in bob.inbox.history()]


# A program that follows the inbox is given such a message too, once.
def test_a_program_that_follows_is_given_a_late_message_once(team, sidecar):
    team.inbox.PAGE = 2
    got = []
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob, orvu.Team.connect(
        socket=sidecar.socket, me="carol"
    ) as carol:
        bob.inbox.PAGE = 2

        def listen():
            for message in bob.inbox.watch():
                got.append(message.body)

        threading.Thread(target=listen, daemon=True).start()
        for number in range(9):
            team.inbox.send("bob", number)
            assert wait_until(lambda: len(got) == number + 1), got
        carol.inbox._newest = lambda owner: 0
        carol.inbox.send("bob", "late, on page 0")
        assert wait_until(lambda: got[9:] == ["late, on page 0"]), got
        team.inbox.send("bob", "after")
        assert wait_until(lambda: got[9:] == ["late, on page 0", "after"]), got


# Following an inbox looks at which page it is on and how long its pages are
# to see that something changed, and does not read every message for that.
def test_following_an_inbox_does_not_read_its_messages_to_see_a_change(team, bob):
    def not_read():
        raise AssertionError("the messages were read to see whether anything changed")

    bob.inbox.messages = not_read
    got = []

    def listen():
        for message in bob.inbox.watch():
            got.append(message.body)

    threading.Thread(target=listen, daemon=True).start()
    team.inbox.send("bob", "one")
    assert wait_until(lambda: got == ["one"]), got
    team.inbox.send_all("two")
    assert wait_until(lambda: got == ["one", "two"]), got


# Once a page is finished, what an agent was given from it is not read again,
# nor added to: a mark for the whole page says how far the agent got.
def test_a_finished_page_is_not_looked_at_again(team, sidecar, monkeypatch):
    team.inbox.PAGE = 2
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        for number in range(8):
            team.inbox.send("bob", number)
            bob.inbox.unread()
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        asked = []
        given = bob.inbox._given
        monkeypatch.setattr(bob.inbox, "_given", lambda owner, number: asked.append(number) or given(owner, number))
        team.inbox.send("bob", "new")
        assert [message.body for message in bob.inbox.unread()] == ["new"]
        assert all(number >= 2 for number in asked), asked
        done = team.client.map("orvu/default/inbox-done/bob/bob").entries()
        assert {page: done[page] for page in ("0", "1")} == {"0": "2", "1": "2"}


# A count a sender could not have written, far too big or below zero, hides
# no message. A count that big may also be stuck at the counter's limit, so
# that a late message does not change it: the page is read again, now and
# then, for as long as its count cannot be right.
@pytest.mark.parametrize("forged", [2**63, 2**62])
def test_a_count_that_cannot_be_right_hides_no_message(team, sidecar, monkeypatch, forged):
    team.inbox.PAGE = 2
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob, orvu.Team.connect(
        socket=sidecar.socket, me="carol"
    ) as carol:
        bob.inbox.PAGE = 2
        bob.inbox.WAIT = bob.inbox.LONGEST_WAIT = 0.05
        for number in range(9):
            team.inbox.send("bob", number)
            bob.inbox.unread()
        team.client.cart("orvu/default/inbox-count/bob").add("0", forged)
        bob.inbox.unread()
        carol.inbox._newest = lambda owner: 0
        carol.inbox.send("bob", "late, on page 0")
        got = []
        assert wait_until(lambda: got.extend(m.body for m in bob.inbox.unread()) or got), got
        assert got == ["late, on page 0"]
        read = []
        reading = bob.inbox._read
        monkeypatch.setattr(bob.inbox, "_read", lambda owner, first, newest: read.append((owner, first)) or reading(owner, first, newest))
        assert bob.inbox.unread() == []
        assert ("bob", 0) in read, read
        # A second late message on the page, whatever its count does.
        late = {"id": "m-late-2", "from": "carol", "to": "bob", "body": "later, on page 0", "sent": 2}
        page = team.client.list("orvu/default/inbox/bob/0")
        page.insert(page.len(), json.dumps(late))
        got = []
        assert wait_until(lambda: got.extend(m.body for m in bob.inbox.unread()) or got), got
        assert got == ["later, on page 0"]
        assert bob.inbox.unread() == []


# A late message's count can arrive before the message does. A program that
# follows the inbox is given the message when it comes.
def test_a_program_that_follows_is_given_a_late_message_whose_count_came_first(team, sidecar):
    team.inbox.PAGE = 2
    got, looks = [], []
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        # The page is followed, so the message is given when it lands, not
        # when the wait is over.
        bob.inbox.WAIT = bob.inbox.LONGEST_WAIT = 30
        unread = bob.inbox.unread
        bob.inbox.unread = lambda: looks.append(1) or unread()

        def listen():
            for message in bob.inbox.watch():
                got.append(message.body)
                if len(got) == 11:
                    return

        listener = threading.Thread(target=listen, daemon=True)
        listener.start()
        for number in range(9):
            team.inbox.send("bob", number)
            assert wait_until(lambda: len(got) == number + 1), got
        # The count goes up, and the program looks and finds nothing yet.
        before = len(looks)
        team.client.cart("orvu/default/inbox-count/bob").add("0", 1)
        assert wait_until(lambda: len(looks) > before)
        time.sleep(0.3)
        late = {"id": "m-late", "from": "carol", "to": "bob", "body": "late, on page 0", "sent": 1}
        page = team.client.list("orvu/default/inbox/bob/0")
        page.insert(page.len(), json.dumps(late))
        assert wait_until(lambda: got[9:] == ["late, on page 0"]), got
        team.inbox.send("bob", "after")
        listener.join(timeout=5)
        assert got[9:] == ["late, on page 0", "after"], got


def _late(team, page, body, counted=True):
    """Writes a message late to an old page of bob's inbox, the way a machine
    that was cut off writes it: with its count, or without it when the count
    came first."""
    late = {"id": f"m-late-{page}-{len(body)}", "from": "carol", "to": "bob", "body": body, "sent": 1}
    name = f"orvu/default/inbox/bob/{page}"
    with team.client.batch() as batch:
        batch.list(name).insert(team.client.list(name).len(), json.dumps(late))
        if counted:
            batch.cart("orvu/default/inbox-count/bob").add(str(page), 1)


def _old_pages_read(bob, monkeypatch, below):
    """Spies on which older pages, below `below`, bob reads."""
    read = []
    reading = bob.inbox._read
    monkeypatch.setattr(
        bob.inbox, "_read", lambda owner, first, newest: read.append((owner, first)) or reading(owner, first, newest)
    )
    return lambda: [page for owner, page in read if owner == "bob" and page < below]


# A late message's count can arrive before the message does. However many
# times the page is read again before the message lands, it is given when it
# does.
def test_a_late_message_whose_count_came_first_is_given_after_many_looks(team, sidecar):
    team.inbox.PAGE = 2
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        bob.inbox.WAIT, bob.inbox.LONGEST_WAIT = 0.05, 0.2
        for number in range(9):
            team.inbox.send("bob", number)
            bob.inbox.unread()
        team.client.cart("orvu/default/inbox-count/bob").add("0", 1)
        for _ in range(6):
            assert bob.inbox.unread() == []
            time.sleep(0.06)
        _late(team, 0, "late, on page 0", counted=False)
        got = []
        assert wait_until(lambda: got.extend(m.body for m in bob.inbox.unread()) or got), got
        assert got == ["late, on page 0"]
        assert bob.inbox.unread() == []


# The same for a program that follows the inbox, while other messages keep
# it looking.
def test_a_program_that_follows_is_given_a_late_message_after_many_wakes(team, sidecar):
    team.inbox.PAGE = 2
    got, looks = [], []
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        bob.inbox.WAIT, bob.inbox.LONGEST_WAIT = 0.05, 0.2
        unread = bob.inbox.unread
        bob.inbox.unread = lambda: looks.append(1) or unread()

        def listen():
            for message in bob.inbox.watch():
                got.append(message.body)
                if message.body == "late, on page 0":
                    return

        threading.Thread(target=listen, daemon=True).start()
        for number in range(9):
            team.inbox.send("bob", number)
            assert wait_until(lambda: len(got) == number + 1), got
        team.client.cart("orvu/default/inbox-count/bob").add("0", 1)
        # Other messages wake the program again and again.
        for number in range(8):
            before = len(looks)
            team.inbox.send_all(f"busy {number}")
            assert wait_until(lambda: len(looks) > before)
            time.sleep(0.1)
        _late(team, 0, "late, on page 0", counted=False)
        assert wait_until(lambda: "late, on page 0" in got), got
        assert got.count("late, on page 0") == 1, got


# Anyone who may send can make the count of any page wrong. Wrong counts on
# many newer pages do not keep a real late message on an older page from
# being given.
@pytest.mark.parametrize("forged", [2**63, 1])
def test_wrong_counts_on_newer_pages_do_not_hold_back_a_late_message(team, sidecar, forged):
    team.inbox.PAGE = 2
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        bob.inbox.WAIT, bob.inbox.LONGEST_WAIT = 0.05, 0.2
        for number in range(24):
            team.inbox.send("bob", number)
            bob.inbox.unread()
        counts = team.client.cart("orvu/default/inbox-count/bob")
        _late(team, 1, "late, on page 1")
        for page in range(2, 11):
            counts.add(str(page), forged)
        got = []
        assert wait_until(lambda: got.extend(m.body for m in bob.inbox.unread()) or got), got
        assert got == ["late, on page 1"]


def test_a_program_that_follows_is_not_held_back_by_wrong_counts_on_newer_pages(team, sidecar):
    team.inbox.PAGE = 2
    got = []
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        bob.inbox.WAIT, bob.inbox.LONGEST_WAIT = 0.05, 0.2

        def listen():
            for message in bob.inbox.watch():
                got.append(message.body)
                if len(got) == 25:
                    return

        threading.Thread(target=listen, daemon=True).start()
        for number in range(24):
            team.inbox.send("bob", number)
            assert wait_until(lambda: len(got) == number + 1), got
        counts = team.client.cart("orvu/default/inbox-count/bob")
        for page in range(2, 11):
            counts.add(str(page), 2**63)
        time.sleep(0.3)
        _late(team, 1, "late, on page 1", counted=False)
        counts.add("1", 1)
        assert wait_until(lambda: got[24:] == ["late, on page 1"]), got


# A program that follows the inbox follows at most `BEHIND` older pages. A
# late message on another one is given when its wait is over, with nothing
# else to wake the program.
def test_a_program_that_follows_reads_the_pages_it_does_not_follow_when_their_wait_is_over(team, sidecar):
    team.inbox.PAGE = 2
    got = []
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        bob.inbox.BEHIND = 1
        bob.inbox.WAIT, bob.inbox.LONGEST_WAIT = 0.05, 0.2

        def listen():
            for message in bob.inbox.watch():
                got.append(message.body)
                if len(got) == 10:
                    return

        threading.Thread(target=listen, daemon=True).start()
        for number in range(9):
            team.inbox.send("bob", number)
            assert wait_until(lambda: len(got) == number + 1), got
        counts = team.client.cart("orvu/default/inbox-count/bob")
        counts.add("0", 1)
        counts.add("1", 1)
        assert wait_until(lambda: len(bob.inbox._later) == 2 and len(bob.inbox._waiting) == 1)
        time.sleep(0.3)
        # The message lands on the page not followed just now.
        page = 1 - bob.inbox._waiting[0][1]
        _late(team, page, "late", counted=False)
        assert wait_until(lambda: got[9:] == ["late"]), got


# A page whose count says more than has arrived, or cannot be right, is read
# again, but with a growing wait between reads while nothing new is found
# there: at once, then after 1, 2 and 4 waits, and so on up to the longest
# wait. A change to its count starts it over. At most `BEHIND` pages are read again on each look,
# those read longest ago first.
def test_an_old_page_with_nothing_new_is_read_again_only_now_and_then(team, sidecar, monkeypatch):
    team.inbox.PAGE = 2
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2
        # The clock goes by whole ticks, so that nothing hangs on rounding.
        clock = [1000]
        bob.inbox._now = lambda: clock[0]
        bob.inbox.WAIT, bob.inbox.LONGEST_WAIT = 10, 40
        for number in range(24):
            team.inbox.send("bob", number)
            bob.inbox.unread()
        # Half the counts one too many, half far too big.
        counts = team.client.cart("orvu/default/inbox-count/bob")
        for page in range(10):
            counts.add(str(page), 1 if page % 2 else 2**63)
        old = _old_pages_read(bob, monkeypatch, 10)
        reads = {page: [] for page in range(10)}
        looks = []
        for tick in range(200):
            clock[0] = 1000 + tick
            before = len(old())
            assert bob.inbox.unread() == []
            assert len(bob.inbox._waiting) <= bob.inbox.BEHIND, bob.inbox._waiting
            now = old()[before:]
            looks.append(now)
            for page in now:
                reads[page].append(tick)
        # Eight at the first look, and the two left over at the next.
        assert looks[0] == list(range(8)) and looks[1] == [8, 9], looks[:2]
        for page in range(8):
            assert reads[page] == [0, 10, 30, 70, 110, 150, 190], (page, reads[page])
        for page in range(8, 10):
            assert reads[page] == [1, 11, 31, 71, 111, 151, 191], (page, reads[page])
        # A new count starts the page over.
        counts.add("3", 1)
        before = len(old())
        bob.inbox.unread()
        assert old()[before:] == [3]


# A program that follows the inbox is given a late message on a page whose
# count cannot be right, even when the message leaves that count as it was.
def test_a_program_that_follows_is_given_a_late_message_on_a_page_with_a_wrong_count(team, sidecar):
    team.inbox.PAGE = 2
    got = []
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        bob.inbox.PAGE = 2

        def listen():
            for message in bob.inbox.watch():
                got.append(message.body)
                if len(got) == 10:
                    return

        threading.Thread(target=listen, daemon=True).start()
        for number in range(9):
            team.inbox.send("bob", number)
            assert wait_until(lambda: len(got) == number + 1), got
        team.client.cart("orvu/default/inbox-count/bob").add("0", 2**63)
        assert wait_until(lambda: ("bob", 0) in bob.inbox._waiting)
        late = {"id": "m-late", "from": "carol", "to": "bob", "body": "late, on page 0", "sent": 1}
        page = team.client.list("orvu/default/inbox/bob/0")
        page.insert(page.len(), json.dumps(late))
        assert wait_until(lambda: got[9:] == ["late, on page 0"]), got
