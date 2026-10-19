"""Several operations in one message."""

import pytest

import orvu


def test_a_batch_sends_everything_at_once_and_keeps_the_order(client):
    with client.batch() as batch:
        batch.counter("visits").increment(5)
        batch.list("notes").insert(0, "a")
        batch.list("notes").insert(1, "b")
        total = batch.counter("visits").value()
        notes = batch.list("notes").to_list()
    assert total.value == 5
    assert notes.value == ["a", "b"]
    assert len(batch.results) == 5
    # What the batch did is there for ordinary calls too.
    assert client.counter("visits").value() == 5


def test_a_value_is_not_there_before_the_batch_is_sent(client):
    batch = client.batch()
    pending = batch.counter("visits").value()
    with pytest.raises(orvu.BadRequest):
        pending.value
    batch.send()
    assert pending.value == 0


def test_one_refusal_or_failure_does_not_undo_the_others(client):
    client.counter("thing").increment(1)
    with client.batch() as batch:
        batch.bounded_counter("budget").increment(3)
        too_much = batch.bounded_counter("budget").try_decrement(5)
        wrong_type = batch.list("thing").insert(0, "x")
        fine = batch.bounded_counter("budget").try_decrement(2)
    assert too_much.value is False
    assert fine.value is True
    with pytest.raises(orvu.TypeMismatch):
        wrong_type.value
    assert client.bounded_counter("budget").value() == 1


# A request that is itself wrong stops the whole batch before anything runs.
def test_a_bad_request_in_a_batch_applies_nothing(client):
    batch = client.batch()
    batch.counter("visits").increment(5)
    batch.rich_text("page").format(0, 1, "bold", "")
    with pytest.raises(orvu.BadRequest):
        batch.send()
    assert client.counter("visits").value() == 0


def test_a_block_that_fails_sends_nothing(client):
    with pytest.raises(RuntimeError):
        with client.batch() as batch:
            batch.counter("visits").increment(5)
            raise RuntimeError("something else went wrong")
    assert client.counter("visits").value() == 0


def test_watch_cannot_be_part_of_a_batch(client):
    with pytest.raises(orvu.BadRequest):
        client.batch().counter("visits").watch()


def test_sending_a_batch_twice_does_not_run_it_twice(client):
    with client.batch() as batch:
        batch.counter("visits").increment(1)
        batch.send()
    assert client.counter("visits").value() == 1
    assert batch.send() == batch.results
    assert client.counter("visits").value() == 1


def test_nothing_can_be_added_to_a_batch_that_was_sent(client):
    batch = client.batch()
    batch.counter("visits").increment(1)
    batch.send()
    with pytest.raises(orvu.BadRequest):
        batch.counter("visits").increment(1)


MIB = 1024 * 1024


# The sidecar stops running a batch's operations once their replies add up to
# more than 4 MiB. The ones it did run keep their results.
def test_a_batch_with_too_much_to_read_keeps_what_it_did_read(client):
    client.register("big").set("x" * (3 * MIB // 2))
    with client.batch() as batch:
        reads = [batch.register("big").get() for _ in range(6)]
    assert len(reads[0].value) == 3 * MIB // 2
    with pytest.raises(orvu.TooLarge):
        reads[-1].value
