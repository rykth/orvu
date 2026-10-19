"""Each of the eleven kinds of object, method by method."""

import math

import pytest

import orvu


def test_counter(client):
    visits = client.counter("visits")
    assert visits.value() == 0  # nothing written yet
    visits.increment()
    visits.increment(5)
    visits.decrement(2)
    assert visits.value() == 4
    visits.decrement(10)
    assert visits.value() == -6  # a plain counter may go below zero


def test_register(client):
    title = client.register("title")
    assert title.get() is None  # never set
    title.set("Draft")
    title.set("Final")
    assert title.get() == "Final"
    title.set("")
    assert title.get() == ""  # set to empty is not the same as never set


def test_set(client):
    tags = client.set("tags")
    assert tags.elements() == set()
    tags.add("red")
    tags.add("blue")
    tags.add("red")
    assert tags.elements() == {"red", "blue"}
    assert tags.contains("red")
    tags.remove("red")
    assert not tags.contains("red")
    assert tags.elements() == {"blue"}


def test_map(client):
    profile = client.map("profile")
    assert profile.get("name") is None
    assert profile.keys() == []
    assert profile.entries() == {}
    profile.put("name", "alice")
    profile.put("city", "berlin")
    profile.put("city", "paris")
    assert profile.get("city") == "paris"
    assert sorted(profile.keys()) == ["city", "name"]
    profile.remove("city")
    assert profile.entries() == {"name": "alice"}


def test_mv_map(client):
    plans = client.mv_map("plans")
    assert plans.get("launch") == []
    assert plans.keys() == []
    assert plans.entries() == {}
    plans.put("launch", "friday")
    plans.put("launch", "monday")
    plans.put("owner", "")
    # A write replaces what this machine has seen, so there is no clash.
    assert plans.get("launch") == ["monday"]
    assert plans.get("owner") == [""]  # an empty value is a value
    assert sorted(plans.keys()) == ["launch", "owner"]
    plans.remove("owner")
    assert plans.entries() == {"launch": ["monday"]}


def test_cart(client):
    basket = client.cart("basket")
    assert basket.quantity("apple") == 0
    assert basket.items() == {}
    basket.add("apple")
    basket.add("apple", 4)
    basket.add("pear", 2)
    basket.remove_qty("apple", 2)
    assert basket.quantity("apple") == 3
    assert basket.items() == {"apple": 3, "pear": 2}
    basket.remove_item("pear")
    assert basket.items() == {"apple": 3}
    basket.remove_qty("apple", 10)
    assert basket.quantity("apple") == -7  # nothing stops this going below zero


def test_list(client):
    notes = client.list("notes")
    assert notes.len() == 0
    assert notes.to_list() == []
    assert notes.get(0) is None
    notes.insert(0, "first")
    notes.insert(1, "third")
    notes.insert(1, "second")
    assert notes.to_list() == ["first", "second", "third"]
    assert notes.get(1) == "second"
    assert notes.len() == 3
    notes.delete(0)
    assert notes.to_list() == ["second", "third"]
    with pytest.raises(orvu.OutOfBounds):
        notes.insert(9, "too far")
    with pytest.raises(orvu.OutOfBounds):
        notes.delete(9)


def test_json(client):
    doc = client.json("doc")
    assert doc.to_value() is None
    assert doc.get(["title"]) is None
    doc.set(["title"], "Plan")
    doc.set(["meta", "done"], False)  # the object "meta" is created on the way
    doc.set(["tags"], ["a", "b"])
    assert doc.get(["title"]) == "Plan"
    assert doc.get(["tags", 1]) == "b"
    assert doc.to_value() == {"title": "Plan", "meta": {"done": False}, "tags": ["a", "b"]}
    doc.set(["tags"], ["c"])  # setting replaces
    assert doc.get(["tags"]) == ["c"]
    doc.remove(["meta", "done"])
    assert doc.get(["meta"]) == {}
    doc.remove(["title"])
    assert doc.get(["title"]) is None
    # A write may not go through a list position.
    with pytest.raises(orvu.JsonError):
        doc.set(["tags", 0, "inside"], 1)


def test_a_whole_number_a_float_cannot_hold_is_refused_not_rounded(client):
    # A number in a JSON document is a 64-bit float. It holds every whole
    # number up to 2**53 exactly, and only some past it. Those are kept.
    doc = client.json("doc")
    doc.set(["n"], 2**53)
    assert doc.get(["n"]) == 2**53
    for exact in (10**16, 2**60, -(2**60), {"a": [2**70]}):
        doc.set(["exact"], exact)
        assert doc.get(["exact"]) == exact
    doc.remove(["exact"])
    for big in (2**53 + 1, -(2**60) - 1, {"a": [2**60 + 1]}, 10**400):
        with pytest.raises(orvu.JsonError, match="2\\^53"):
            doc.set(["n"], big)
        with pytest.raises(orvu.JsonError, match="2\\^53"):
            doc.insert(["xs"], 0, big)
    assert doc.to_value() == {"n": 2**53}
    # A float is sent as one, so a large float is fine.
    doc.set(["f"], 1.5e300)
    assert doc.get(["f"]) == 1.5e300


def test_a_whole_number_comes_back_as_a_whole_number(client):
    # Past 2**53 too, up to 10**21: it comes back as an int, not a float.
    doc = client.json("doc")
    for exact in (2**53, 10**16, 2**60, -(2**60), 10**20, [2**60]):
        doc.set(["n"], exact)
        got = doc.get(["n"])
        assert got == exact
        assert type(got) is type(exact)
        assert type(doc.to_value()["n"]) is type(exact)
    # A float stays a float, and negative zero keeps its sign.
    for f in (1.5e300, 2.0**70, -0.0, 0.5):
        doc.set(["f"], f)
        got = doc.get(["f"])
        assert type(got) is float and got == f
        assert math.copysign(1, got) == math.copysign(1, f)


def test_an_item_is_put_into_a_list_inside_a_json_document(client, sidecar):
    doc = client.json("doc")
    doc.set(["steps"], ["b"])
    doc.insert(["steps"], 0, "a")
    doc.insert(["steps"], 2, {"then": "d"})
    doc.insert(["steps"], 2, "c")
    assert doc.get(["steps"]) == ["a", "b", "c", {"then": "d"}]
    # Where there is nothing yet, position 0 starts a list.
    doc.insert(["log"], 0, "first")
    doc.insert(["log"], 1, "second")
    assert doc.get(["log"]) == ["first", "second"]
    for path, index in [(["steps"], 9), (["steps", 0], 0), (["nothing-here"], 1)]:
        with pytest.raises(orvu.OrvuError):
            doc.insert(path, index, "x")
    with pytest.raises(orvu.BadRequest):
        doc.insert(["steps"], -1, "x")
    with pytest.raises(orvu.BadRequest):
        doc.insert(["steps"], 0, {1, 2})
    assert doc.get(["steps"]) == ["a", "b", "c", {"then": "d"}]

    # What is there and is not a list is left as it is.
    doc.set(["cfg"], {"a": 1})
    doc.set(["title"], "Plan")
    for path in (["cfg"], ["title"]):
        with pytest.raises(orvu.JsonError, match="not a list"):
            doc.insert(path, 0, "x")
    assert doc.get(["cfg"]) == {"a": 1} and doc.get(["title"]) == "Plan"

    # Two programs that add to the same list both keep what they added.
    with orvu.connect(sidecar.socket) as other:
        other.json("doc").insert(["log"], 2, "from the other")
    doc.insert(["log"], 3, "and mine")
    assert doc.get(["log"]) == ["first", "second", "from the other", "and mine"]
    with client.batch() as batch:
        batch.json("doc").insert(["log"], 0, "in a batch")
    assert doc.get(["log"])[0] == "in a batch"


def test_rich_text(client):
    page = client.rich_text("page")
    assert page.text() == ""
    assert page.to_spans() == []
    page.insert(0, "héllo wörld")  # positions count characters, not bytes
    page.insert(5, " ✓")
    assert page.text() == "héllo ✓ wörld"
    page.format(0, 5, "bold", "true")
    assert page.to_spans()[0] == {"text": "héllo", "marks": {"bold": "true"}}
    page.unformat(0, 2, "bold")
    spans = page.to_spans()
    assert spans[0] == {"text": "hé", "marks": {}}
    assert spans[1] == {"text": "llo", "marks": {"bold": "true"}}
    page.delete(0, 6)
    assert page.text() == "✓ wörld"
    # A format with no value is a mistake in the request.
    with pytest.raises(orvu.BadRequest):
        page.format(0, 1, "bold", "")


def test_tree(client):
    outline = client.tree("outline")
    assert outline.children("root") == []
    assert outline.to_nodes() == []
    outline.move_node("report", "root")
    outline.move_node("intro", "report")
    outline.move_node_at("summary", "report", 0)
    outline.set_label("intro", "Introduction")
    assert outline.children("root") == ["report"]
    assert set(outline.children("report")) == {"intro", "summary"}
    assert outline.parent("intro") == "report"
    assert outline.parent("report") == "root"
    # The top has no parent, and neither has a node that was never placed.
    assert outline.parent("root") is None
    assert outline.parent("nobody") is None
    nodes = {node["id"]: node for node in outline.to_nodes()}
    assert nodes["intro"]["parent"] == "report"
    assert nodes["intro"]["label"] == "Introduction"
    assert nodes["root"]["parent"] is None
    # Putting "report" under its own child would make a loop, so it is skipped.
    outline.move_node("report", "intro")
    assert outline.children("root") == ["report"]


def test_bounded_counter(client):
    budget = client.bounded_counter("budget")
    assert budget.value() == 0
    assert budget.allowance() == 0
    budget.increment(10)
    assert budget.value() == 10
    assert budget.allowance() == 10
    assert budget.try_decrement(4) is True
    assert budget.value() == 6
    # Hand 2 to another machine. The total stays, our share shrinks.
    assert budget.transfer(999, 2) is True
    assert budget.value() == 6
    assert budget.allowance() == 4
