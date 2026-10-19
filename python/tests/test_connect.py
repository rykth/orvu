"""Connecting, sessions, and what happens when the sidecar goes away."""

import signal

import pytest

import orvu

from conftest import Sidecar, hold_still, short_folder, wait_until


def test_connecting_tells_you_who_answered(client):
    assert client.sidecar_version == "0.1.0"
    assert client.replica_id > 0


def test_a_value_written_can_be_read_back(client):
    visits = client.counter("visits")
    visits.increment(5)
    assert visits.value() == 5


def test_no_sidecar_is_a_clear_error(tmp_path):
    with pytest.raises(orvu.SidecarUnavailable) as raised:
        orvu.connect(tmp_path / "nobody-here.sock", timeout=0.5)
    assert "nobody-here.sock" in str(raised.value)


def test_a_namespace_the_sidecar_does_not_serve_is_refused(sidecar):
    with pytest.raises(orvu.BadRequest):
        orvu.connect(sidecar.socket, namespace="someone-else")


# The sidecar forgets its sessions when it restarts. The client must notice,
# open a new one, and carry on, and what was saved must still be there.
def test_the_client_carries_on_after_the_sidecar_restarts(sidecar):
    with orvu.connect(sidecar.socket) as client:
        visits = client.counter("visits")
        visits.increment(5)
        first_session = client._session

        sidecar.restart()

        assert visits.value() == 5
        visits.increment(1)
        assert visits.value() == 6
        assert client._session != first_session


def test_two_clients_see_each_others_changes(sidecar):
    with orvu.connect(sidecar.socket) as first, orvu.connect(sidecar.socket) as second:
        first.counter("visits").increment(2)
        second.counter("visits").increment(3)
        assert first.counter("visits").value() == 5
        assert second.counter("visits").value() == 5


MIB = 1024 * 1024


def test_a_reply_larger_than_4_mib_can_be_read(client):
    notes = client.list("notes")
    for _ in range(5):
        notes.insert(0, "x" * MIB)
    assert sum(len(note) for note in notes.to_list()) == 5 * MIB


def test_a_reply_over_the_callers_limit_raises_too_large(sidecar, client):
    client.register("big").set("x" * MIB)
    with orvu.connect(sidecar.socket, max_reply_bytes=1024) as small:
        with pytest.raises(orvu.TooLarge):
            small.register("big").get()


def test_a_request_of_up_to_64_mib_is_taken(client):
    assert client.set("big").contains("x" * (60 * MIB)) is False


def test_a_request_the_sidecar_will_not_take_raises_too_large(client):
    with pytest.raises(orvu.TooLarge):
        client.register("big").set("x" * (65 * MIB))
    assert client.register("big").get() is None


# Running out of time does not mean nothing happened, so it must not look like
# "the sidecar is not there". Here the sidecar is held still while a write is
# on its way. Once it is let go, the write may go through or not: the client
# stopped waiting and told the sidecar so, and the sidecar may hear that before
# it starts on the write.
def test_running_out_of_time_has_an_error_of_its_own(sidecar):
    with orvu.connect(sidecar.socket, timeout=0.5) as client:
        try:
            hold_still(sidecar.process)
            with pytest.raises(orvu.TimedOut) as raised:
                client.counter("visits").increment(1)
        finally:
            sidecar.process.send_signal(signal.SIGCONT)
        assert not isinstance(raised.value, orvu.SidecarUnavailable)


# A sidecar told to stop when idle stops only when nobody is connected. A
# connect that failed must not go on counting as somebody.
def test_a_failed_connect_leaves_no_connection_behind():
    with short_folder() as folder:
        sidecar = Sidecar(folder).start("--exit-when-idle", "1")
        try:
            try:
                orvu.connect(sidecar.socket, namespace="someone-else")
            except orvu.BadRequest as error:
                kept = error  # as a program that logs its errors would
            assert wait_until(lambda: sidecar.process.poll() is not None, seconds=8), (
                "the sidecar still has a connection open"
            )
            del kept
        finally:
            sidecar.stop()
