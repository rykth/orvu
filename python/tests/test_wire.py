"""Reading the sidecar's replies. These tests need no sidecar."""

import pytest

import orvu
from orvu import _wire as wire
from orvu._proto import dataplane_pb2 as pb


def value(**kind):
    return pb.Response(value=pb.ReadValue(**kind))


def test_an_ack_is_none_and_for_a_refusable_operation_true():
    ack = pb.Response(ack=pb.Ack())
    assert wire.Op("counter_increment", pb.CounterIncrement()).result(ack) is None
    assert wire.Op("bounded_try_decrement", pb.BoundedTryDecrement(), refusable=True).result(ack) is True


def test_a_refusal_is_false_for_an_operation_that_may_be_refused():
    refused = pb.Response(refused=pb.Refused(detail="nothing left"))
    op = wire.Op("bounded_try_decrement", pb.BoundedTryDecrement(), refusable=True)
    assert op.result(refused) is False
    # Any other operation should never be refused. If one is, that is an error.
    with pytest.raises(orvu.EngineError):
        wire.Op("counter_increment", pb.CounterIncrement()).result(refused)


def test_each_failure_code_raises_its_own_error():
    for code, error in [
        (pb.CODE_TYPE_MISMATCH, orvu.TypeMismatch),
        (pb.CODE_CLASS_MISMATCH, orvu.ClassMismatch),
        (pb.CODE_OUT_OF_BOUNDS, orvu.OutOfBounds),
        (pb.CODE_JSON, orvu.JsonError),
        (pb.CODE_TOO_LARGE, orvu.TooLarge),
        (pb.CODE_ENGINE, orvu.EngineError),
        (pb.CODE_CONFIG, orvu.EngineError),
    ]:
        failed = pb.Response(failed=pb.Failed(code=code, detail="what went wrong"))
        with pytest.raises(error) as raised:
            wire.Op("counter_value", pb.CounterValue()).result(failed)
        assert "what went wrong" in str(raised.value)


def test_values_are_read_into_python_types():
    assert wire.number(value(int_value=-3).value) == -3
    assert wire.text_or_none(value(string_value="").value) == ""
    assert wire.truth(value(bool_value=True).value) is True
    assert wire.text_list(value(strings=pb.StringList(values=["a", "b"])).value) == ["a", "b"]
    assert wire.text_set(value(strings=pb.StringList(values=["a", "a"])).value) == {"a"}
    pairs = pb.PairList(pairs=[pb.Pair(key="k", value="v")])
    assert wire.text_dict(value(pairs=pairs).value) == {"k": "v"}
    counted = pb.CountedList(items=[pb.CountedItem(item="x", qty=-2)])
    assert wire.count_dict(value(counted=counted).value) == {"x": -2}
    assert wire.json_or_none(value(json='{"a": [1, null]}').value) == {"a": [1, None]}


def test_absent_reads_as_an_empty_value_of_the_right_kind():
    absent = value(absent=pb.Absent()).value
    assert wire.number(absent) == 0
    assert wire.text_or_none(absent) is None
    assert wire.text(absent) == ""
    assert wire.truth(absent) is False
    assert wire.text_list(absent) == []
    assert wire.text_set(absent) == set()
    assert wire.text_dict(absent) == {}
    assert wire.count_dict(absent) == {}
    assert wire.json_or_none(absent) is None
    assert wire.json_list(absent) == []
    # Each call gets a list of its own, so changing one does not change another.
    assert wire.text_list(absent) is not wire.text_list(absent)


def test_a_request_carries_the_operation_and_the_session():
    request = wire.Op("counter_increment", pb.CounterIncrement(object="visits", n=5)).request("abc")
    assert request.session == "abc"
    assert request.WhichOneof("op") == "counter_increment"
    assert request.counter_increment.n == 5
