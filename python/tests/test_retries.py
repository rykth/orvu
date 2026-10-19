"""A change whose answer never came is sent again, with the id it was first
sent with, and the sidecar makes it once. So a program is not left asking
itself whether a change it was told failed was made after all."""

import os
import threading
import time
from concurrent import futures

import grpc
import pytest

import orvu
from orvu._proto import dataplane_pb2 as pb
from orvu._proto import dataplane_pb2_grpc as pb_grpc
from orvu._version import VERSION

from conftest import Sidecar, short_folder, sidecars


def stop_after_saving(monkeypatch, folder):
    """Has the next sidecar started stop at once, the next time it saves a
    change made for a request id, without answering it. Returns the file
    whose going says that it did."""
    mark = folder / "stop-after-saving"
    monkeypatch.setenv("ORVUD_TEST_STOP_AFTER_SAVING", str(mark))
    return mark


# The sidecar this package starts stops after saving a change and before
# answering it. The client starts it again and sends the change again, and
# the change is made once.
def test_a_change_cut_off_after_it_was_saved_is_made_once(home, monkeypatch):
    mark = stop_after_saving(monkeypatch, home)
    with orvu.connect(timeout=30) as client:
        visits = client.counter("visits")
        visits.increment(1)
        (first,) = sidecars(home)
        mark.touch()
        visits.increment(1)
        assert not mark.exists(), "the sidecar did not stop after saving"
        assert first not in sidecars(home)
        assert visits.value() == 2

        # A batch is sent again the same way, and made once.
        mark.touch()
        with client.batch() as batch:
            batch.counter("visits").increment(10)
            batch.counter("visits").increment(100)
        assert not mark.exists()
        assert [pending.value for pending in batch.results] == [None, None]
        assert visits.value() == 112


# A sidecar started by hand is not started again by the client. It waits
# for it to come back, and sends the change again then.
def test_a_change_cut_off_is_sent_again_once_the_sidecar_is_back(monkeypatch):
    with short_folder() as folder:
        sidecar = Sidecar(folder)
        mark = stop_after_saving(monkeypatch, sidecar.folder)
        first = sidecar.start().process
        back = threading.Timer(1.0, sidecar.start)
        try:
            with orvu.connect(str(sidecar.socket), timeout=30) as client:
                visits = client.counter("visits")
                mark.touch()
                back.start()
                visits.increment(1)
                back.join()
                assert not mark.exists()
                assert visits.value() == 1
        finally:
            back.cancel()
            if back.is_alive():
                back.join()
            sidecar.stop()
            if first.poll() is None:
                first.kill()
                first.wait()


# The sidecar stops after a top-up is saved and before its line in the record
# of who topped the budget up is. The top-up sent again makes only the line:
# the budget is topped up once, and says by whom.
def test_a_top_up_cut_off_before_its_line_makes_only_the_line(monkeypatch):
    with short_folder() as folder:
        sidecar = Sidecar(folder)
        mark = stop_after_saving(monkeypatch, sidecar.folder)
        key = sidecar.add_agent("boss")
        sidecar.allow("boss", "budgets:top-up")
        first = sidecar.start().process
        back = threading.Timer(1.0, sidecar.start)
        try:
            with orvu.Team.connect(socket=str(sidecar.socket), key=key, timeout=30) as boss:
                mark.write_text("top-up")
                back.start()
                boss.budget("tokens").top_up(10)
                back.join()
                assert not mark.exists(), "the sidecar did not stop after the top-up"
                assert boss.budget("tokens").left() == 10
                assert boss.budget("tokens").topped_up_by() == {"boss": 10}
        finally:
            back.cancel()
            if back.is_alive():
                back.join()
            sidecar.stop()
            if first.poll() is None:
                first.kill()
                first.wait()


class Slow(pb_grpc.DataPlaneServicer):
    """Stands in for a sidecar that takes longer than the client waits over
    the first try of each change, and remembers what was sent."""

    def __init__(self):
        self.sent = []

    def Open(self, request, context):
        return pb.OpenResponse(session="s", resume_token="r", replica_id=1, version=VERSION)

    def Execute(self, request, context):
        self.sent.append(request)
        if request.request_id and len(self.sent) == 1:
            time.sleep(1.5)
        return pb.Response(ack=pb.Ack())


@pytest.fixture
def slow():
    with short_folder() as folder:
        servicer = Slow()
        server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
        pb_grpc.add_DataPlaneServicer_to_server(servicer, server)
        socket = os.path.join(folder, "s")
        server.add_insecure_port(f"unix:{socket}")
        server.start()
        try:
            yield servicer, socket
        finally:
            server.stop(None)


# A change that ran out of time is sent again with the same id, and the
# caller gets its answer and not `TimedOut`.
def test_a_change_that_ran_out_of_time_is_sent_again_with_its_id(slow):
    servicer, socket = slow
    with orvu.connect(socket, timeout=1.0) as client:
        client.counter("visits").increment(1)
    first, again = servicer.sent
    assert len(first.request_id) == 16
    assert again.request_id == first.request_id
    assert again.counter_increment.n == 1


# Each change has an id of its own, and a read has none: a read that fails
# is not sent again, as it never was.
def test_each_change_has_an_id_of_its_own_and_a_read_has_none(slow):
    servicer, socket = slow
    servicer.sent.append(None)  # the first change is not held up
    with orvu.connect(socket, timeout=5.0) as client:
        client.counter("visits").increment(1)
        client.counter("visits").increment(1)
        client.counter("visits").value()
    _, one, two, read = servicer.sent
    assert len(one.request_id) == 16 and len(two.request_id) == 16
    assert one.request_id != two.request_id
    assert read.request_id == b""


# A sidecar that does not come back is waited for only so long: the change
# then ends in an error, as before.
def test_a_sidecar_that_stays_away_still_ends_in_an_error(monkeypatch):
    with short_folder() as folder:
        sidecar = Sidecar(folder)
        mark = stop_after_saving(monkeypatch, sidecar.folder)
        sidecar.start()
        try:
            with orvu.connect(str(sidecar.socket), timeout=2.0) as client:
                mark.touch()
                started = time.monotonic()
                with pytest.raises(orvu.SidecarUnavailable):
                    client.counter("visits").increment(1)
                assert time.monotonic() - started < 30
                assert not mark.exists()
        finally:
            sidecar.stop()
