"""The team: the one object the building blocks hang off."""

import pytest

import orvu

from conftest import sidecars


def test_a_team_gives_out_its_blocks(team):
    assert team.name == "default"
    assert team.me == "alice"
    team.notes.put("deploy-date", "Friday")
    assert team.notes.get("deploy-date") == "Friday"
    assert team.tasks.list() == []
    assert team.budget("tokens").left() == 0


def test_two_agents_on_one_team_share_everything(sidecar):
    with orvu.Team.connect(socket=sidecar.socket, me="alice") as alice:
        with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
            alice.notes.put("deploy-date", "Friday")
            task = alice.tasks.add("Write the report")
            alice.budget("tokens").top_up(10)

            assert bob.notes.get("deploy-date") == "Friday"
            assert [found.id for found in bob.tasks.list()] == [task.id]
            assert bob.budget("tokens").left() == 10


def test_two_teams_on_one_sidecar_share_nothing(sidecar):
    with orvu.Team.connect("red", socket=sidecar.socket) as red:
        with orvu.Team.connect("blue", socket=sidecar.socket) as blue:
            red.notes.put("deploy-date", "Friday")
            red.tasks.add("Write the report")
            red.budget("tokens").top_up(10)

            assert blue.notes.all() == {}
            assert blue.tasks.list() == []
            assert blue.budget("tokens").left() == 0
            assert blue.index() == {}


# The sidecar cannot list what it holds, so the team writes each object down
# the first time a block creates it.
def test_the_index_lists_what_the_team_has(team):
    assert team.index() == {}
    team.notes.get("nothing")
    team.budget("tokens").left()
    team.tasks.list()
    team.document("q3-report").text()
    team.tally("errors").total()
    team.roster.members()
    team.inbox.messages()
    team.budget("tokens").spent_by()
    team.budget("tokens").topped_up_by()
    team.notes.authors()
    assert team.index() == {}, "reading must not put anything in the index"

    team.notes.put("deploy-date", "Friday")
    team.notes.structured("plan").set(["owner"], "alice")
    team.budget("tokens").top_up(5)
    team.budget("money").top_up(5)
    team.tasks.add("Write the report")
    team.budget("tokens").spend(1)
    team.document("q3-report").append("Sales rose 4%.")
    team.tally("errors").add()
    team.roster.join()
    team.inbox.send("bob", "hello")
    team.inbox.send_all("hello")

    assert team.index() == {
        "notes-mv": "mvmap",
        "note/plan": "json",
        "budget/tokens": "bounded",
        "budget/money": "bounded",
        "budget-spent/tokens": "cart",
        "budget-topped/tokens": "cart",
        "budget-topped/money": "cart",
        "tasks": "json",
        "tasks-tree": "tree",
        "doc/q3-report": "richtext",
        "tally/errors": "cart",
        "roster": "map",
        "inbox/bob/0": "list",
        "inbox/@everyone/0": "list",
        "inbox-count/bob": "cart",
        "inbox-count/@everyone": "cart",
    }
    assert team.documents() == ["q3-report"] and team.tallies() == ["errors"]
    assert team.budgets() == ["money", "tokens"]
    assert team.notes.structured_names() == ["plan"]


def test_the_index_is_the_same_for_an_agent_that_connects_later(sidecar, team):
    team.budget("tokens").top_up(5)
    with orvu.Team.connect(socket=sidecar.socket) as later:
        assert later.budgets() == ["tokens"]


# What the index says an object is has to be what it is, or the index is no
# use for finding things.
def test_what_the_index_says_matches_the_real_objects(team):
    team.notes.put("deploy-date", "Friday")
    team.notes.structured("plan").set(["owner"], "alice")
    team.budget("tokens").top_up(5)
    team.tasks.add("Write the report")
    team.budget("tokens").spend(1)
    team.document("q3-report").append("Sales rose 4%.")
    team.tally("errors").add()
    team.roster.join()
    team.inbox.PAGE = 1
    team.inbox.send("bob", "hello")
    team.inbox.send("bob", "again")
    team.inbox.send_all("hello")
    open_as = {
        "map": team.client.map,
        "mvmap": team.client.mv_map,
        "json": team.client.json,
        "bounded": team.client.bounded_counter,
        "tree": team.client.tree,
        "cart": team.client.cart,
        "richtext": team.client.rich_text,
        "list": team.client.list,
        "counter": team.client.counter,
    }
    read = {
        "map": "entries",
        "mvmap": "entries",
        "json": "to_value",
        "bounded": "value",
        "tree": "to_nodes",
        "cart": "items",
        "richtext": "text",
        "list": "to_list",
        "counter": "value",
    }
    assert set(team.index().values()) == set(open_as), "a kind of object is not checked here"
    for short_name, kind in team.index().items():
        handle = open_as[kind](f"orvu/{team.name}/{short_name}")
        assert getattr(handle, read[kind])(), f"{short_name} is empty or is not a {kind}"


@pytest.mark.parametrize("name", ["", "a/b", "two words", "x" * 65])
def test_a_team_name_that_cannot_be_used_is_refused_before_connecting(home, name):
    with pytest.raises(orvu.BadRequest, match="name of a team"):
        orvu.Team.connect(name)
    assert sidecars(home) == [], "a sidecar was started for a team that cannot exist"


def test_a_team_connects_with_no_setup(home):
    with orvu.Team.connect(me="alice") as team:
        team.notes.put("deploy-date", "Friday")
    with orvu.Team.connect(me="bob") as team:
        assert team.notes.get("deploy-date") == "Friday"
    assert len(sidecars(home)) == 1


# The blocks keep their data under `orvu/`. A program that also uses plain
# objects through `team.client` is not in their way.
def test_plain_objects_live_beside_the_blocks(team):
    team.client.counter("visits").increment(3)
    team.notes.put("visits", "see the counter")
    assert team.client.counter("visits").value() == 3
    assert team.notes.get("visits") == "see the counter"
