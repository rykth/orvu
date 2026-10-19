"""The rules for the names the building blocks give their objects."""

import pytest

import orvu
from orvu import _names


def test_every_object_of_a_team_is_under_the_teams_own_prefix():
    names = [
        _names.index("crew"),
        _names.notes("crew"),
        _names.note("crew", "plan"),
        _names.tasks("crew"),
        _names.task_tree("crew"),
        _names.budget("crew", "tokens"),
    ]
    assert all(name.startswith("orvu/crew/") for name in names)
    assert len(set(names)) == len(names)


# A block's objects all start with a word of its own, and a caller's name
# cannot contain a slash. So whatever a note or a budget is called, it cannot
# land on another block's object.
@pytest.mark.parametrize("name", ["notes", "tasks", "tasks-tree", "index", "budget", "note"])
def test_no_name_a_caller_picks_lands_on_another_blocks_object(name):
    fixed = {_names.index("crew"), _names.notes("crew"), _names.tasks("crew"), _names.task_tree("crew")}
    assert _names.note("crew", _names.part(name, "a note")) not in fixed
    assert _names.budget("crew", _names.part(name, "a budget")) not in fixed
    assert _names.note("crew", name) != _names.budget("crew", name)


@pytest.mark.parametrize("name", ["plan", "Q3-report", "v1.2", "a", "tokens_left", "7", "x" * 64])
def test_ordinary_names_are_accepted(name):
    assert _names.part(name, "a name") == name


@pytest.mark.parametrize(
    "name", ["", "a/b", "../x", " plan", "plan ", "-plan", ".plan", "a b", "ü", "x" * 65, None, 7, "a\nb"]
)
def test_names_that_could_cause_trouble_are_refused(name):
    with pytest.raises(orvu.BadRequest, match="cannot be used as a budget"):
        _names.part(name, "a budget")
