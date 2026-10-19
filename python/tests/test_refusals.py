"""Being told no is an ordinary answer. Real failures are named errors."""

import pytest

import orvu


def test_running_out_returns_false_and_changes_nothing(client):
    budget = client.bounded_counter("budget")
    budget.increment(3)
    assert budget.try_decrement(5) is False
    assert budget.value() == 3
    assert budget.allowance() == 3
    assert budget.try_decrement(3) is True
    assert budget.try_decrement(1) is False
    assert budget.value() == 0


def test_handing_over_more_than_you_have_returns_false(client):
    budget = client.bounded_counter("budget")
    budget.increment(3)
    assert budget.transfer(999, 5) is False
    assert budget.allowance() == 3


def test_using_a_name_as_another_type_is_a_type_mismatch(client):
    client.counter("thing").increment(1)
    with pytest.raises(orvu.TypeMismatch) as raised:
        client.list("thing").insert(0, "x")
    assert "thing" in str(raised.value)
    # The counter is untouched.
    assert client.counter("thing").value() == 1


def test_using_a_name_under_other_rules_is_a_class_mismatch(client):
    client.declare("budget", "escrow")
    with pytest.raises(orvu.ClassMismatch):
        client.counter("budget").increment(1)
    # Under the rules it was declared with, it works.
    client.bounded_counter("budget").increment(1)


def test_an_unknown_class_is_caught_before_anything_is_sent(client):
    with pytest.raises(orvu.BadRequest):
        client.declare("thing", "strict")


def test_every_error_is_an_orvu_error():
    for error in (
        orvu.SidecarUnavailable,
        orvu.BadRequest,
        orvu.TypeMismatch,
        orvu.ClassMismatch,
        orvu.OutOfBounds,
        orvu.JsonError,
        orvu.TooLarge,
        orvu.EngineError,
    ):
        assert issubclass(error, orvu.OrvuError)


# Reading never creates anything. A name that has only been read can still be
# used for any type.
def test_reading_does_not_take_the_name(client):
    assert client.counter("thing").value() == 0
    assert client.list("thing").to_list() == []
    client.register("thing").set("now it is a register")
    assert client.register("thing").get() == "now it is a register"


# A number that cannot be sent, such as a negative count, is the caller's
# mistake. It is reported like every other failure.
def test_a_value_that_cannot_be_sent_raises_bad_request(client):
    with pytest.raises(orvu.BadRequest):
        client.counter("visits").increment(-1)
    with pytest.raises(orvu.BadRequest):
        client.list("notes").insert(-1, "x")
    with pytest.raises(orvu.BadRequest):
        client.bounded_counter("budget").try_decrement(-5)
    with pytest.raises(orvu.BadRequest):
        client.counter("visits").increment("many")
    with pytest.raises(orvu.BadRequest):
        client.json("doc").set(["when"], object())
    assert client.counter("visits").value() == 0


def test_a_value_that_cannot_be_sent_stops_a_batch_with_bad_request(client):
    with pytest.raises(orvu.BadRequest):
        with client.batch() as batch:
            batch.counter("visits").increment(1)
            batch.counter("visits").increment(-1)
    assert client.counter("visits").value() == 0
