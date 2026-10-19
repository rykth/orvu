"""Notes: small facts the team shares."""

import pytest

import orvu


def test_a_note_is_put_read_and_removed(team):
    assert team.notes.get("deploy-date") is None
    team.notes.put("deploy-date", "Friday")
    team.notes.put("owner", "alice")
    assert team.notes.get("deploy-date") == "Friday"
    assert team.notes.all() == {"deploy-date": "Friday", "owner": "alice"}

    team.notes.put("deploy-date", "Monday")
    assert team.notes.get("deploy-date") == "Monday"

    team.notes.remove("deploy-date")
    team.notes.remove("never-there")
    assert team.notes.all() == {"owner": "alice"}


# A note's name is only a key inside one object, so it can be any text.
@pytest.mark.parametrize("name", ["deploy date", "a/b", "../x", "ü", "x" * 500, " "])
def test_a_notes_name_can_be_any_text(team, name):
    team.notes.put(name, "kept")
    assert team.notes.get(name) == "kept"
    assert team.notes.all() == {name: "kept"}


@pytest.mark.parametrize("name", ["", None, 7])
def test_a_note_needs_a_name(team, name):
    with pytest.raises(orvu.BadRequest, match="name"):
        team.notes.put(name, "text")
    with pytest.raises(orvu.BadRequest, match="name"):
        team.notes.get(name)
    assert team.notes.all() == {}
    assert team.index() == {}, "a note that was refused was still written down"


# A note takes anything JSON can hold, and gives it back as what it was.
@pytest.mark.parametrize(
    "value",
    [7, -2.5, True, False, 0, "", "Friday", "7", "true", ["a", 1, None], {"a": {"b": [1, 2]}}, [], {}, "ü \x1e x", "\x1e7"],
)
def test_a_note_holds_any_value_and_gives_it_back_as_it_was(team, bob, value):
    team.notes.put("kept", value)
    for reader in (team, bob):
        got = reader.notes.get("kept")
        assert got == value and type(got) is type(value)
        assert reader.notes.all() == {"kept": value}


@pytest.mark.parametrize("value", [None, {1, 2}, float("nan"), object()])
def test_a_note_cannot_hold_what_json_cannot(team, value):
    with pytest.raises(orvu.BadRequest):
        team.notes.put("kept", value)
    assert team.notes.all() == {}
    assert team.index() == {}, "a note that was refused was still written down"


# Notes used to be kept in a plain map, with who wrote each in another. The
# sidecar moves them over the first time the notes are used, keeping who
# wrote each, and the old maps are left as they were.
def test_notes_kept_the_old_way_are_moved_over(team):
    old = team.client.map("orvu/default/notes")
    old.put("deploy-date", "Friday")
    old.put("budget", "\x1e7")
    team.client.map("orvu/default/notes-by").put("deploy-date", "bob")
    assert team.notes.all() == {"deploy-date": "Friday", "budget": 7}
    assert team.notes.authors() == {"deploy-date": "bob", "budget": None}
    assert old.entries() == {"deploy-date": "Friday", "budget": "\x1e7"}
    team.notes.put("deploy-date", "Monday")
    assert old.get("deploy-date") == "Friday"
    assert team.index() == {"notes-mv": "mvmap"}


# The text is kept after the writer's name, as it was in the old map.
def test_a_text_note_is_kept_as_plain_text_after_its_writer(team):
    team.notes.put("deploy-date", "Friday")
    assert team.client.mv_map("orvu/default/notes-mv").get("deploy-date") == ["alice\x1fFriday"]


def test_an_agent_whose_name_holds_the_separator_cannot_write_notes(sidecar):
    with orvu.Team.connect(socket=sidecar.socket, me="a\x1fb") as odd:
        with pytest.raises(orvu.BadRequest, match="cannot write notes"):
            odd.notes.put("deploy-date", "Friday")


def test_a_note_says_who_wrote_it(team, bob, sidecar):
    assert team.notes.author("deploy-date") is None
    team.notes.put("deploy-date", "Friday")
    assert team.notes.author("deploy-date") == "alice"
    bob.notes.put("deploy-date", "Monday")
    bob.notes.put("owner", "bob")
    assert team.notes.author("deploy-date") == "bob"
    assert team.notes.authors() == {"deploy-date": "bob", "owner": "bob"}

    # A program that gave no name cannot say who it is.
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        nameless.notes.put("quiet", "x")
        assert nameless.notes.author("quiet") is None
    team.notes.remove("deploy-date")
    assert team.notes.authors() == {"owner": "bob", "quiet": None}


def test_a_structured_note_keeps_fields(team):
    plan = team.notes.structured("plan")
    assert plan.to_value() is None
    plan.set(["owner"], "alice")
    plan.set(["steps"], ["write", "test"])
    plan.set(["review", "by"], "bob")
    assert plan.to_value() == {"owner": "alice", "steps": ["write", "test"], "review": {"by": "bob"}}
    assert team.notes.structured("plan").get(["review", "by"]) == "bob"


# Two agents fill in different fields of the same structured note. Both
# fields have to be there afterwards.
def test_two_agents_changing_different_fields_both_keep_their_change(sidecar, team):
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as bob:
        team.notes.structured("plan").set(["owner"], "alice")
        bob.notes.structured("plan").set(["reviewer"], "bob")
        assert team.notes.structured("plan").to_value() == {"owner": "alice", "reviewer": "bob"}


@pytest.mark.parametrize("name", ["", "a/b", "two words", None])
def test_a_structured_note_needs_a_plain_name(team, name):
    with pytest.raises(orvu.BadRequest, match="structured note"):
        team.notes.structured(name)
    assert team.index() == {}


# A simple note and a structured note may have the same name. They are
# different notes.
def test_a_simple_and_a_structured_note_can_share_a_name(team):
    team.notes.put("plan", "see the structured one")
    team.notes.structured("plan").set(["owner"], "alice")
    assert team.notes.get("plan") == "see the structured one"
    assert team.notes.structured("plan").to_value() == {"owner": "alice"}


# The sidecar cannot list what it holds, so the team writes each object down
# the first time a block creates it. Reading writes nothing down.
def test_notes_are_written_down_in_the_teams_index(team):
    team.notes.get("deploy-date")
    team.notes.all()
    assert team.index() == {}
    team.notes.put("deploy-date", "Friday")
    assert team.index() == {"notes-mv": "mvmap"}
    team.notes.structured("plan").get(["owner"])
    team.notes.structured("draft").to_value()
    assert team.index() == {"notes-mv": "mvmap"}, "reading a structured note must not write it down"
    team.notes.structured("plan").set(["owner"], "alice")
    assert team.index() == {"notes-mv": "mvmap", "note/plan": "json"}
    assert team.notes.structured_names() == ["plan"]


# A note written by a program with no name has no author. It must not keep
# the name of whoever wrote the value before.
def test_a_note_rewritten_by_a_nameless_program_loses_its_author(team, sidecar):
    team.notes.put("deploy-date", "Friday")
    with orvu.Team.connect(socket=sidecar.socket) as nameless:
        nameless.notes.put("deploy-date", "Monday")
    assert team.notes.get("deploy-date") == "Monday"
    assert team.notes.author("deploy-date") is None


# A note is kept as the writer's name, the character \x1f, and the note:
# text as it is, and anything else as the mark \x1e and its JSON form, which
# programs in other languages read too.
def test_a_note_is_kept_in_the_form_every_language_reads(team):
    team.notes.put("plan", {"day": "Friday"})
    team.notes.put("deploy", "Friday")
    team.notes.put("odd", "\x1eodd")
    kept = team.client.mv_map(f"orvu/{team.name}/notes-mv").entries()
    assert kept["plan"] == ['alice\x1f\x1e{"day": "Friday"}']
    assert kept["deploy"] == ["alice\x1fFriday"]
    assert kept["odd"] == ['alice\x1f\x1e"\\u001eodd"']
