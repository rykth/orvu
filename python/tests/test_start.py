"""`orvu.connect()` with no socket: finding this user's sidecar, and starting
it when it is not running.

Each test has a home folder of its own, so the sidecars started here never
touch the real one.
"""

import fcntl
import os
import pathlib
import signal
import socket as sockets
import subprocess
import sys
import threading
import time
import traceback
from concurrent import futures

import grpc
import pytest

import orvu
from orvu import _sidecar
from orvu._proto import dataplane_pb2 as pb
from orvu._proto import dataplane_pb2_grpc as pb_grpc
from orvu._version import VERSION

from conftest import sidecars, wait_until


def test_connect_starts_a_sidecar_when_none_is_running(home):
    assert sidecars(home) == []
    with orvu.connect() as client:
        client.counter("visits").increment(5)
        assert client.counter("visits").value() == 5
    assert len(sidecars(home)) == 1


def test_connect_uses_the_sidecar_that_is_already_running(home):
    with orvu.connect() as first, orvu.connect() as second:
        first.counter("visits").increment(2)
        assert second.counter("visits").value() == 2
        assert first.replica_id == second.replica_id
    assert len(sidecars(home)) == 1


def test_what_was_written_is_still_there_after_the_sidecar_stops(home):
    with orvu.connect() as client:
        client.counter("visits").increment(5)
    (pid,) = sidecars(home)
    os.kill(pid, signal.SIGTERM)
    assert wait_until(lambda: not sidecars(home))

    with orvu.connect() as client:
        assert client.counter("visits").value() == 5
    assert sidecars(home) != [pid]


# A sidecar that was killed outright leaves its socket file behind. The next
# connect must not be fooled by it.
def test_connect_starts_a_new_sidecar_after_one_was_killed(home):
    with orvu.connect() as client:
        client.counter("visits").increment(5)
    # A sidecar saves what it was sent within a tenth of a second. Killed
    # sooner than that, it loses the write, and this test is not about that.
    time.sleep(0.3)
    (pid,) = sidecars(home)
    os.kill(pid, signal.SIGKILL)
    assert wait_until(lambda: not sidecars(home))
    assert _sidecar.default_socket(_sidecar.default_data_dir()).exists()

    with orvu.connect() as client:
        assert client.counter("visits").value() == 5


# The sidecar refuses a data folder that other users can get into. The person
# who called `connect` has to be told why, in the sidecar's own words, and
# without waiting for the whole timeout.
def test_a_sidecar_that_cannot_start_says_why(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o755)
    data.chmod(0o755)
    asked = time.monotonic()
    with pytest.raises(orvu.SidecarUnavailable, match="chmod 700") as raised:
        orvu.connect(timeout=30)
    assert time.monotonic() - asked < 5, "it waited instead of saying why at once"
    assert "could not start" in str(raised.value)
    assert sidecars(home) == []


# A data folder that seems to have been copied from another computer is
# refused. Started with no flags, the sidecar cannot be told what happened,
# so the error has to pass on the command that sorts it out, folder and all.
def test_a_copied_data_folder_is_explained_with_the_command_to_run(home):
    with orvu.connect():
        (pid,) = sidecars(home)
    os.kill(pid, signal.SIGTERM)
    assert wait_until(lambda: not sidecars(home))
    data = _sidecar.default_data_dir()
    (data / "computer").write_text("somewhere else\n")
    with pytest.raises(orvu.SidecarUnavailable) as raised:
        orvu.connect(timeout=30)
    said = str(raised.value)
    assert "copied from another computer" in said, said
    for flag in ("--moved", "--new-number"):
        assert f"orvud --local --data-dir {quoted(data.resolve())} {flag}" in said, said
    assert sidecars(home) == []


def quoted(folder):
    """`folder` as the sidecar writes it into a command to copy: as it is if
    every character is plain, in single quotes if not, as a folder on a Mac
    is, with the space in "Application Support"."""
    text = str(folder)
    if all(c.isascii() and (c.isalnum() or c in "/._-+,:@%") for c in text):
        return text
    return "'" + text.replace("'", "'\\''") + "'"


# With no runtime folder the socket goes inside the data folder, and a socket
# path has a short limit. A home folder too deep for it has to be reported,
# not left to time out.
def test_a_home_folder_too_deep_for_a_socket_is_explained(home, monkeypatch):
    deep = home / ("a" * 90)
    deep.mkdir()
    monkeypatch.setenv("HOME", str(deep))
    monkeypatch.delenv("XDG_RUNTIME_DIR")
    with pytest.raises(orvu.SidecarUnavailable, match="socket path") as raised:
        orvu.connect(timeout=30)
    assert "could not start" in str(raised.value)


def test_the_sidecar_is_not_stopped_by_ctrl_c_in_the_program_that_started_it(home):
    with orvu.connect():
        (pid,) = sidecars(home)
        # Ctrl-C goes to every process in the terminal's session. The sidecar
        # is in a session of its own.
        assert os.getsid(pid) == pid
        assert os.getsid(pid) != os.getsid(0)


def test_what_the_sidecar_prints_is_kept_in_its_data_folder(home):
    with orvu.connect():
        pass
    assert "serving on" in (_sidecar.default_data_dir() / "orvud.log").read_text()


PROGRAM = """
import pathlib, sys, time
import orvu
here = pathlib.Path(sys.argv[1])
(here / ("ready-" + sys.argv[2])).touch()
while not (here / "go").exists():
    time.sleep(0.001)
with orvu.connect() as client:
    client.counter("visits").increment(1)
"""


# Twenty separate programs call `connect()` at the same moment, with no
# sidecar running. They must end up sharing one, and none of them may fail.
# Each program gets ready and then waits for the same signal, so that they
# really do start together.
def test_twenty_programs_starting_at_once_share_one_sidecar(home):
    programs = [
        subprocess.Popen(
            [sys.executable, "-c", PROGRAM, str(home), str(number)], stderr=subprocess.PIPE, text=True
        )
        for number in range(20)
    ]
    assert wait_until(lambda: len(list(home.glob("ready-*"))) == 20, seconds=30)
    (home / "go").touch()
    failures = [program.communicate(timeout=60)[1] for program in programs]
    assert [program.returncode for program in programs] == [0] * 20, failures

    assert len(sidecars(home)) == 1
    with orvu.connect() as client:
        assert client.counter("visits").value() == 20


# While one program is starting the sidecar, the others wait their turn. They
# do not each start one and leave it to the sidecars to sort out. Here the
# test takes the turn itself, as a program in the middle of starting would.
def test_a_program_waits_while_another_is_starting_the_sidecar(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o700)
    done = []

    def connect():
        with orvu.connect() as client:
            done.append(client.replica_id)

    with open(data / "start.lock", "w") as turn:
        fcntl.flock(turn, fcntl.LOCK_EX)
        thread = threading.Thread(target=connect)
        thread.start()
        time.sleep(0.5)
        assert done == [] and sidecars(home) == [], "it did not wait for its turn"
    thread.join(timeout=10)
    assert len(done) == 1
    assert len(sidecars(home)) == 1


def test_waiting_for_another_program_to_start_the_sidecar_has_a_limit(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o700)
    with open(data / "start.lock", "w") as turn:
        fcntl.flock(turn, fcntl.LOCK_EX)
        with pytest.raises(orvu.SidecarUnavailable, match="another program"):
            orvu.connect(timeout=0.5)
    assert sidecars(home) == []


def test_connect_says_so_when_there_is_no_home_folder(home, monkeypatch):
    monkeypatch.delenv("HOME")
    with pytest.raises(orvu.SidecarUnavailable, match="HOME"):
        orvu.connect()


def test_having_no_program_to_start_is_explained(home, monkeypatch):
    monkeypatch.delenv("ORVUD_BIN")
    monkeypatch.setenv("PATH", str(home))
    with pytest.raises(orvu.SidecarUnavailable, match="no orvud program"):
        orvu.connect()
    assert sidecars(home) == []


# ---- a sidecar that goes away while a program is using it ---------------------


# A sidecar stops when it has been idle, and a long-running program may well
# be quiet for longer than that. Its next call has to start the sidecar again
# and go through, once.
def test_a_client_starts_the_sidecar_again_when_it_has_stopped(home):
    with orvu.connect() as client:
        visits = client.counter("visits")
        visits.increment(5)
        (pid,) = sidecars(home)
        os.kill(pid, signal.SIGTERM)
        assert wait_until(lambda: not sidecars(home))

        visits.increment(1)
        assert visits.value() == 6
        assert len(sidecars(home)) == 1


def test_a_client_starts_the_sidecar_again_when_it_was_killed(home):
    with orvu.connect() as client:
        visits = client.counter("visits")
        visits.increment(5)
        # A sidecar saves what it was sent within a tenth of a second. Killed
        # sooner than that, it loses the write, and this test is not about that.
        time.sleep(0.3)
        (pid,) = sidecars(home)
        os.kill(pid, signal.SIGKILL)
        assert wait_until(lambda: not sidecars(home))

        assert visits.value() == 5


# A client that was given a socket is using a sidecar somebody else runs. It
# never starts one.
def test_a_client_given_a_socket_does_not_start_a_sidecar(home, sidecar):
    with orvu.connect(sidecar.socket) as client:
        client.counter("visits").increment(1)
        sidecar.stop()
        with pytest.raises(orvu.SidecarUnavailable):
            client.counter("visits").value()
    assert sidecars(home) == []


def test_sidecars_that_have_stopped_are_not_kept_track_of(home):
    with orvu.connect():
        pass
    running = sidecars(home)
    assert len(running) == 1, running
    os.kill(running[0], signal.SIGTERM)
    # A sidecar finishes what it was doing before it stops, which on a busy
    # machine can take a while.
    assert wait_until(lambda: not sidecars(home), seconds=20), sidecars(home)
    with orvu.connect():
        pass
    ours = [child for child in _sidecar._started if str(home) in " ".join(map(str, child.args))]
    assert len(ours) == 1 and ours[0].poll() is None, [(child.pid, child.poll()) for child in ours]


# ---- a sidecar that is on its way out -----------------------------------------


# A sidecar that is shutting down has given up its socket but still holds its
# data folder for a moment. A program that connects just then has to wait for
# it, not report that the new sidecar could not start. Here the test holds
# the folder the way the old sidecar would.
def test_connect_waits_for_a_sidecar_that_is_still_shutting_down(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o700)
    done = []

    def connect():
        with orvu.connect() as client:
            done.append(client.replica_id)

    with open(data / "orvud.lock", "w") as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        thread = threading.Thread(target=connect)
        thread.start()
        time.sleep(0.5)
        assert thread.is_alive(), "it gave up while the old sidecar still held the folder"
    thread.join(timeout=10)
    assert len(done) == 1


# Something still holds the socket open but no longer answers on it, as a
# sidecar does in its last moment. Connect must not spend all its time
# waiting on that socket.
def test_connect_gets_past_a_socket_that_stopped_answering(home):
    path = _sidecar.default_socket(_sidecar.default_data_dir())
    dying = sockets.socket(sockets.AF_UNIX, sockets.SOCK_STREAM)
    dying.bind(str(path))
    dying.listen(8)

    def let_go():
        time.sleep(1.0)
        dying.close()
        path.unlink()

    threading.Thread(target=let_go).start()
    with orvu.connect(timeout=10) as client:
        client.counter("visits").increment(1)
    assert len(sidecars(home)) == 1


# ---- a sidecar that is not a local one ---------------------------------------


# A sidecar connected to other machines, started by hand, serves on the same
# default socket. `connect()` promises a private store on this machine, so it
# must not quietly use that one and send the program's writes to the others.
def test_connect_does_not_use_a_sidecar_that_is_not_a_local_one(home):
    from test_agreement import Machine, free_port, keygen_bin

    subprocess.run(
        [keygen_bin(), "--out", home / "certs", "--replicas", "1"], check=True, stdout=subprocess.DEVNULL
    )
    networked = Machine(home / "one", 1, free_port())
    networked.socket = _sidecar.default_socket(_sidecar.default_data_dir())
    networked.start()
    try:
        with pytest.raises(orvu.SidecarUnavailable, match="not a local one") as raised:
            orvu.connect()
        assert str(networked.socket) in str(raised.value)
        assert "ORVU_SOCKET" in str(raised.value)
        # Asked for by its socket, it is used as before.
        with orvu.connect(networked.socket) as client:
            client.counter("visits").increment(1)
    finally:
        networked.stop()


# A sidecar that is stopping turns away calls that reach it in its last
# moments, before it has stopped taking them, with "the sidecar is stopping".
# Such a call never ran, so a client of the sidecar this package starts waits
# for that one to go, starts it again, and makes the call there. Several
# programs keep calling while the sidecar is stopped a few times; none of
# their calls fails, and every one of them is counted once.
#
# A call only meets that refusal if it arrives in the moment between the
# sidecar being told to stop and it no longer taking calls, and with a real
# sidecar that cannot be arranged. So this test meets it only now and then.
# The next test is the one that always does.
def test_calls_carry_on_while_the_sidecar_is_stopped_and_started_again(home):
    stopping, failed, made = threading.Event(), [], []

    def keep_calling():
        with orvu.connect(timeout=30) as client:
            visits = client.counter("visits")
            count = 0
            try:
                while not stopping.is_set():
                    visits.increment(1)
                    count += 1
            except Exception:  # noqa: BLE001 - reported below
                failed.append(traceback.format_exc())
            made.append(count)

    callers = [threading.Thread(target=keep_calling) for _ in range(4)]
    for caller in callers:
        caller.start()
    try:
        for _ in range(6):
            assert wait_until(lambda: len(sidecars(home)) == 1 or failed, seconds=20), sidecars(home)
            # Long enough for every program to be calling again.
            time.sleep(0.3)
            if failed:
                break
            (pid,) = sidecars(home)
            os.kill(pid, signal.SIGTERM)
            assert wait_until(lambda: pid not in sidecars(home), seconds=20)
    finally:
        stopping.set()
        for caller in callers:
            caller.join(timeout=60)
    assert not failed, failed
    with orvu.connect(timeout=30) as client:
        assert client.counter("visits").value() == sum(made)


# The same, made certain: a stand-in for a sidecar answers the session, then
# turns the call away as one that is stopping does, and goes on holding its
# socket for a moment without answering on it.
def test_a_call_turned_away_by_a_sidecar_that_is_stopping_is_made_on_the_next_one(home):
    path = _sidecar.default_socket(_sidecar.default_data_dir())
    dying = sockets.socket(sockets.AF_UNIX, sockets.SOCK_STREAM)
    dying.bind(f"{path}.dying")
    dying.listen(8)
    refused = []

    def gone():
        stopping.stop(None)
        dying.close()
        path.unlink(missing_ok=True)

    class Stopping(pb_grpc.DataPlaneServicer):
        def Open(self, request, context):
            return pb.OpenResponse(session="s", resume_token="r", replica_id=1, version=VERSION)

        def Execute(self, request, context):
            refused.append(request)
            # From now on nothing answers on the socket, and a moment later
            # it is gone.
            os.rename(f"{path}.dying", path)
            threading.Timer(1.0, gone).start()
            context.abort(grpc.StatusCode.UNAVAILABLE, "the sidecar is stopping")

    stopping = grpc.server(futures.ThreadPoolExecutor(max_workers=2))
    pb_grpc.add_DataPlaneServicer_to_server(Stopping(), stopping)
    stopping.add_insecure_port(f"unix:{path}")
    stopping.start()
    with orvu.connect(timeout=20) as client:
        client.counter("visits").increment(1)
        assert len(refused) == 1
        assert client.counter("visits").value() == 1
    assert len(sidecars(home)) == 1


# A call can meet a sidecar that is stopping again when it is made the second
# time: the sidecar started for it may be told to stop too before the call
# gets there. It is made again then too, for as long as the client waits.
# Here the stand-in turns the call away twice, and only then goes.
def test_a_call_turned_away_again_by_a_sidecar_that_is_stopping_is_made_once_more(home):
    path = _sidecar.default_socket(_sidecar.default_data_dir())
    dying = sockets.socket(sockets.AF_UNIX, sockets.SOCK_STREAM)
    dying.bind(f"{path}.dying")
    dying.listen(8)
    refused = []

    def gone():
        stopping.stop(None)
        dying.close()
        path.unlink(missing_ok=True)

    class Stopping(pb_grpc.DataPlaneServicer):
        def Open(self, request, context):
            return pb.OpenResponse(session="s", resume_token="r", replica_id=1, version=VERSION)

        def Execute(self, request, context):
            refused.append(request)
            if len(refused) == 2:
                os.rename(f"{path}.dying", path)
                threading.Timer(1.0, gone).start()
            context.abort(grpc.StatusCode.UNAVAILABLE, "the sidecar is stopping")

    stopping = grpc.server(futures.ThreadPoolExecutor(max_workers=2))
    pb_grpc.add_DataPlaneServicer_to_server(Stopping(), stopping)
    stopping.add_insecure_port(f"unix:{path}")
    stopping.start()
    with orvu.connect(timeout=20) as client:
        client.counter("visits").increment(1)
        assert len(refused) == 2
        assert client.counter("visits").value() == 1
    assert len(sidecars(home)) == 1


# How long a sidecar started from here stays up after the last program has
# gone can be set in the environment.
def test_the_idle_time_can_be_set_in_the_environment(home, monkeypatch):
    monkeypatch.setenv("ORVU_IDLE_SECONDS", "1")
    with orvu.connect() as client:
        client.counter("visits").increment(1)
        (pid,) = sidecars(home)
        command = subprocess.run(["ps", "-ww", "-o", "args=", "-p", str(pid)], capture_output=True, text=True).stdout
        assert "--exit-when-idle 1" in command, command
    assert wait_until(lambda: not sidecars(home), seconds=15)


def test_the_idle_time_is_read_from_the_environment():
    assert _sidecar.idle_seconds({}) == _sidecar.IDLE_SECONDS == 300
    assert _sidecar.idle_seconds({"ORVU_IDLE_SECONDS": ""}) == 300
    assert _sidecar.idle_seconds({"ORVU_IDLE_SECONDS": "3600"}) == 3600
    for wrong in ["0", "-5", "soon", "1.5"]:
        with pytest.raises(orvu.SidecarUnavailable, match="ORVU_IDLE_SECONDS"):
            _sidecar.idle_seconds({"ORVU_IDLE_SECONDS": wrong})
    # The largest number a program in any language can hold exactly, which
    # is what the TypeScript library takes too. orvud could not read some
    # of the larger ones at all.
    assert _sidecar.idle_seconds({"ORVU_IDLE_SECONDS": "9007199254740991"}) == 2**53 - 1
    for too_big in ["9007199254740992", "18446744073709551616", "1" + "0" * 40]:
        with pytest.raises(orvu.SidecarUnavailable, match="at most 9007199254740991"):
            _sidecar.idle_seconds({"ORVU_IDLE_SECONDS": too_big})


def test_a_wrong_idle_time_is_explained_before_anything_starts(home, monkeypatch):
    monkeypatch.setenv("ORVU_IDLE_SECONDS", "soon")
    with pytest.raises(orvu.SidecarUnavailable, match="whole number of seconds"):
        orvu.connect()
    assert sidecars(home) == []


# The log the sidecar writes is moved aside once it has grown past a
# mebibyte, when the next sidecar starts, so it does not grow for ever. One
# older log is kept.
def test_a_long_log_is_moved_aside_when_the_sidecar_starts(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o700)
    (data / "orvud.log.1").write_text("the oldest\n")
    (data / "orvud.log").write_text("x" * (_sidecar.LOG_LIMIT + 1))
    with orvu.connect():
        pass
    assert (data / "orvud.log.1").read_text() == "x" * (_sidecar.LOG_LIMIT + 1)
    assert "serving on" in (data / "orvud.log").read_text()
    assert len((data / "orvud.log").read_text()) < 10_000


def _lock_files(folder):
    return sorted(p.name for p in folder.iterdir() if p.name.startswith("orvud.log.lock"))


# Another program, in this language or another, is moving the log aside at
# this moment: it holds the lock. This one leaves the log to it, so the two
# do not each move a log aside and the second throw the first one's away.
def test_a_log_another_program_is_moving_aside_is_left_to_it(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o700)
    (data / "orvud.log").write_text("x" * (_sidecar.LOG_LIMIT + 1))
    (data / "orvud.log.lock.7").write_text("")
    with orvu.connect():
        pass
    assert not (data / "orvud.log.1").exists()
    assert (data / "orvud.log").read_text().startswith("x" * (_sidecar.LOG_LIMIT + 1))
    assert _lock_files(data) == ["orvud.log.lock.7"]


# A lock left behind by a program that stopped halfway does not keep the log
# from ever being moved aside again.
def test_a_lock_left_behind_long_ago_does_not_stop_the_log_being_moved_aside(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o700)
    (data / "orvud.log").write_text("x" * (_sidecar.LOG_LIMIT + 1))
    _leave_stale(data / "orvud.log.lock.7", "")
    with orvu.connect():
        pass
    assert (data / "orvud.log.1").read_text() == "x" * (_sidecar.LOG_LIMIT + 1)
    assert _lock_files(data) == ["orvud.log.lock.9"]
    assert (data / "orvud.log.lock.9").read_text() == "free"


# The lock is what keeps two programs from both finding the log long and the
# second moving the first one's new, short log over the long one. While
# another program holds it, the log is left as it is.
def test_a_log_whose_lock_another_program_holds_is_not_moved_aside(tmp_path):
    log = tmp_path / "orvud.log"
    long = "x" * (_sidecar.LOG_LIMIT + 1)
    log.write_text(long)
    (tmp_path / "orvud.log.lock.3").write_text("another program")
    _sidecar._move_aside(log)
    assert not (tmp_path / "orvud.log.1").exists(), "the log was moved aside while another program held the lock"
    assert log.read_text() == long
    assert _lock_files(tmp_path) == ["orvud.log.lock.3"]
    assert (tmp_path / "orvud.log.lock.3").read_text() == "another program"


# With the lock free, the long log is moved aside, and the lock given back.
# A second look finds the new log short and leaves it.
def test_a_long_log_is_moved_aside_once_and_the_lock_given_back(tmp_path):
    log = tmp_path / "orvud.log"
    long = "x" * (_sidecar.LOG_LIMIT + 1)
    log.write_text(long)
    _sidecar._move_aside(log)
    assert (tmp_path / "orvud.log.1").read_text() == long
    assert (tmp_path / "orvud.log.lock.2").read_text() == "free"
    log.write_text("short\n")
    _sidecar._move_aside(log)
    assert (tmp_path / "orvud.log.1").read_text() == long
    assert log.read_text() == "short\n"
    assert sorted(p.name for p in tmp_path.iterdir()) == ["orvud.log", "orvud.log.1", "orvud.log.lock.4"]
    assert (tmp_path / "orvud.log.lock.4").read_text() == "free"


# A program held up for longer than a lock is trusted, after it looked at
# the log's size, may find another program has taken the lock over as left
# behind. It looks again before moving the log, and leaves it to the other.
@pytest.mark.parametrize("taken_over", ["a higher lock file", "its own file written over"])
def test_a_program_whose_lock_was_taken_over_meanwhile_does_not_move_the_log(tmp_path, monkeypatch, taken_over):
    log = tmp_path / "orvud.log"
    long = "x" * (_sidecar.LOG_LIMIT + 1)
    log.write_text(long)
    real = pathlib.Path.stat

    def held_up(self, *args, **kwargs):
        found = real(self, *args, **kwargs)
        if self == log:
            if taken_over == "a higher lock file":
                (tmp_path / "orvud.log.lock.2").write_text("another program")
            else:
                (tmp_path / "orvud.log.lock.1").write_text("another program")
        return found

    monkeypatch.setattr(pathlib.Path, "stat", held_up)
    _sidecar._move_aside(log)
    monkeypatch.undo()
    assert not (tmp_path / "orvud.log.1").exists(), "the log was moved by a program that no longer held the lock"
    assert log.read_text() == long


def _leave_stale(lock, text):
    lock.write_text(text)
    long_ago = time.time() - 3600
    os.utime(lock, (long_ago, long_ago))


# A lock left behind is taken over by one program, which makes the next lock
# file with its own mark in it. The next program finds that one held.
def test_a_lock_left_behind_is_taken_over_by_one_program_only(tmp_path):
    lock = tmp_path / "orvud.log.lock"
    _leave_stale(tmp_path / "orvud.log.lock.4", "a program that stopped")
    first = _sidecar._take_lock(lock)
    assert first == 5
    assert (tmp_path / "orvud.log.lock.5").read_text().startswith(f"{os.getpid()}-")
    assert _sidecar._take_lock(lock) is None
    assert _lock_files(tmp_path) == ["orvud.log.lock.5"]


# Only the highest-numbered lock file counts: a free one below a held one
# does not let a program in, and a held one below a free one does not keep
# it out.
def test_only_the_highest_numbered_lock_file_counts(tmp_path):
    lock = tmp_path / "orvud.log.lock"
    (tmp_path / "orvud.log.lock.8").write_text("free")
    (tmp_path / "orvud.log.lock.10").write_text("another program")
    assert _sidecar._take_lock(lock) is None
    (tmp_path / "orvud.log.lock.11").write_text("free")
    assert _sidecar._take_lock(lock) == 12


# A program so slow that the lock moved on past it while it was busy finds,
# once it has made its file, a higher one there, and does not hold the lock.
def test_a_program_the_lock_has_moved_on_past_does_not_hold_it(tmp_path, monkeypatch):
    lock = tmp_path / "orvud.log.lock"
    (tmp_path / "orvud.log.lock.3").write_text("free")
    real = _sidecar._make

    def slow(lock, number, text):
        made = real(lock, number, text)
        (tmp_path / "orvud.log.lock.9").write_text("another program")
        return made

    monkeypatch.setattr(_sidecar, "_make", slow)
    assert _sidecar._take_lock(lock) is None
    assert (tmp_path / "orvud.log.lock.9").read_text() == "another program"


# A program that was slow enough to have its lock taken over gives back only
# its own: the next lock file is another program's.
def test_a_lock_is_given_back_only_by_the_program_that_holds_it(tmp_path):
    lock = tmp_path / "orvud.log.lock"
    mine = _sidecar._take_lock(lock)
    assert mine == 1
    (tmp_path / "orvud.log.lock.2").write_text("another program")
    _sidecar._give_back(lock, mine)
    assert (tmp_path / "orvud.log.lock.2").read_text() == "another program"
    assert _sidecar._take_lock(lock) is None
    (tmp_path / "orvud.log.lock.2").unlink()
    _sidecar._give_back(lock, mine)
    assert _lock_files(tmp_path) == ["orvud.log.lock.2"]
    assert (tmp_path / "orvud.log.lock.2").read_text() == "free"


# Programs that stopped halfway, again and again, and programs that took the
# lock and gave it back: the lock files they leave do not pile up.
def test_lock_files_left_over_do_not_pile_up(tmp_path):
    lock = tmp_path / "orvud.log.lock"
    for _ in range(20):
        assert _sidecar._take_lock(lock) is not None
        _age_everything(tmp_path)
    (tmp_path / "orvud.log.lock.5").write_text("a slow program")
    for _ in range(20):
        _sidecar._give_back(lock, _sidecar._take_lock(lock))
    assert _lock_files(tmp_path) == ["orvud.log.lock.60"]


def _age_everything(folder):
    long_ago = time.time() - 3600
    for path in folder.iterdir():
        os.utime(path, (long_ago, long_ago))


class _Watched:
    """Stands in for the `os` module in `_sidecar`, so a test can let other
    programs act between any two steps of the one it watches."""

    def __init__(self, after_each):
        self._after_each = after_each

    def __getattr__(self, name):
        real = getattr(os, name)
        if not callable(real) or name in ("fspath", "getpid"):
            return real

        def step(*args, **kwargs):
            self._after_each()
            try:
                return real(*args, **kwargs)
            finally:
                self._after_each()

        return step


# A lock was left behind. Program C looks at it and is held up. Meanwhile B
# takes it over and holds it. C, going on from what it saw, must not leave
# a gap in which a third program, D, can take the lock as well: whatever C
# does, D is let try between every two of its steps.
def test_a_lock_taken_over_while_another_program_was_held_up_has_one_holder(tmp_path, monkeypatch):
    lock = tmp_path / "orvud.log.lock"
    assert _sidecar._take_lock(lock) is not None
    _age_everything(tmp_path)
    held = {}
    busy = []

    def as_other(name):
        busy.append(name)
        try:
            got = _sidecar._take_lock(lock)
            if got is not None:
                held[name] = got
        finally:
            busy.pop()

    def after_each():
        if not busy and "B" in held and "D" not in held:
            as_other("D")

    def clock():
        if not busy and "B" not in held:
            as_other("B")
        return time.time()

    monkeypatch.setattr(_sidecar, "os", _Watched(after_each))
    monkeypatch.setattr(_sidecar, "time", type("clock", (), {"time": staticmethod(clock)}))
    got = _sidecar._take_lock(lock)
    if got is not None:
        held["C"] = got
    assert "B" in held, "B never got to take the lock over"
    assert sorted(held) == ["B"], f"more than one program holds the lock: {sorted(held)}"


# The mark cannot be written, because the disk is full. The lock is not
# taken, and nothing is left behind to keep the next program out.
def test_a_lock_whose_mark_cannot_be_written_is_not_taken_and_not_left(tmp_path, monkeypatch):
    import errno

    lock = tmp_path / "orvud.log.lock"

    def full(*args):
        raise OSError(errno.ENOSPC, "No space left on device")

    monkeypatch.setattr(os, "write", full)
    assert _sidecar._take_lock(lock) is None
    monkeypatch.undo()
    assert _sidecar._take_lock(lock) is not None


# The sidecar this program started says why it stopped, but another program
# moved the log aside meanwhile. What it said is still read, from the file
# it went to, and the program goes on to start the sidecar.
def test_what_the_sidecar_said_is_read_even_when_the_log_was_moved_aside(home, monkeypatch):
    once = home / "orvud-once"
    once.write_text(
        "#!/bin/sh\n"
        'if [ ! -e "$3/moved" ]; then\n'
        '  touch "$3/moved"\n'
        '  mv "$3/orvud.log" "$3/orvud.log.1"\n'
        '  echo "orvud: another sidecar is already using the data folder" >&2\n'
        "  exit 1\n"
        "fi\n"
        f'exec "{os.environ["ORVUD_BIN"]}" "$@"\n'
    )
    once.chmod(0o755)
    monkeypatch.setenv("ORVUD_BIN", str(once))
    with orvu.connect() as client:
        client.counter("visits").increment(1)
    assert (_sidecar.default_data_dir() / "moved").exists()


def test_a_short_log_is_added_to(home):
    data = _sidecar.default_data_dir()
    data.mkdir(parents=True, mode=0o700)
    (data / "orvud.log").write_text("from before\n")
    with orvu.connect():
        pass
    assert (data / "orvud.log").read_text().startswith("from before\n")
    assert not (data / "orvud.log.1").exists()


# A program that is meant to use a team's sidecar can be pointed at it by
# the environment, with nothing in its code. Nothing is started for it.
def test_the_socket_can_be_given_in_the_environment(home, sidecar, monkeypatch):
    with orvu.connect(sidecar.socket) as direct:
        direct.counter("visits").increment(3)
    monkeypatch.setenv("ORVU_SOCKET", str(sidecar.socket))
    with orvu.connect() as client:
        assert client.counter("visits").value() == 3
    with orvu.Team.connect("research", me="alice") as team:
        assert team.client.counter("visits").value() == 3
    assert sidecars(home) == []
    # A socket given in the call is the one used.
    with orvu.connect(_sidecar.ensure_running()) as local:
        assert local.counter("visits").value() == 0


def test_a_socket_and_a_connection_file_in_the_environment_are_refused(home, sidecar, monkeypatch):
    monkeypatch.setenv("ORVU_SOCKET", str(sidecar.socket))
    monkeypatch.setenv("ORVU_CONNECT", str(home / "alice.orvu"))
    with pytest.raises(orvu.BadRequest, match="ORVU_SOCKET and ORVU_CONNECT"):
        orvu.connect()


# Nothing answers on the socket given. A team's sidecar is never started by
# the package, so the message says how one is made and started.
def test_no_sidecar_on_the_socket_given_says_how_to_start_a_teams_one(home, monkeypatch):
    with pytest.raises(orvu.SidecarUnavailable) as raised:
        orvu.connect(home / "crew.sock", timeout=0.5)
    assert "crew.sock" in str(raised.value)
    assert "orvud setup" in str(raised.value) and "orvud --config" in str(raised.value)
    monkeypatch.setenv("ORVU_SOCKET", str(home / "crew.sock"))
    with pytest.raises(orvu.SidecarUnavailable, match="orvud setup"):
        orvu.connect(timeout=0.5)
