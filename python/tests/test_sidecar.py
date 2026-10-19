"""The test setup itself: a real sidecar starts, and the message code loads."""

import pathlib
import stat

import pytest

from conftest import Sidecar, short_folder
from orvu._proto import dataplane_pb2 as pb


def test_a_sidecar_starts_and_serves_on_its_socket(sidecar):
    assert stat.S_ISSOCK(sidecar.socket.stat().st_mode)
    assert (sidecar.data / "replica").exists()


def test_a_sidecar_can_be_restarted_on_the_same_folder(sidecar):
    number = (sidecar.data / "replica").read_text()
    sidecar.restart()
    assert stat.S_ISSOCK(sidecar.socket.stat().st_mode)
    assert (sidecar.data / "replica").read_text() == number


def test_the_message_code_knows_the_sidecars_operations():
    operations = {field.name for field in pb.Request.DESCRIPTOR.oneofs_by_name["op"].fields}
    assert "counter_increment" in operations
    assert len(operations) == 54


# A sidecar is never left running when its test is over. Its folder is
# taken away after the test, and a sidecar still running would go on writing
# there, and leave the folder behind.
def test_a_sidecar_that_does_not_stop_when_asked_is_ended_before_the_test_is_over(tmp_path):
    running = Sidecar(tmp_path)
    running.launch(["sh", "-c", "trap '' TERM; echo serving on >&2; while :; do sleep 0.1; done"])
    process = running.process
    with pytest.raises(AssertionError, match="did not stop"):
        running.stop(seconds=1)
    assert process.poll() is not None, "the sidecar was left running"


def test_a_sidecar_that_does_not_get_ready_is_ended(tmp_path):
    running = Sidecar(tmp_path)
    with pytest.raises(AssertionError, match="did not start"):
        running.launch(["sh", "-c", "trap '' TERM; while :; do sleep 0.1; done"])
    assert running.process is None or running.process.poll() is not None, "the sidecar was left running"


# The tests' folders go where ORVU_TEST_DIR says, for a machine where /tmp
# may not be used.
def test_the_tests_folders_go_where_they_are_told(monkeypatch):
    with short_folder() as there:
        monkeypatch.setenv("ORVU_TEST_DIR", there)
        with short_folder() as folder:
            assert pathlib.Path(folder).parent == pathlib.Path(there)
