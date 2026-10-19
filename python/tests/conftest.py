"""Every test here talks to a real sidecar. Nothing is faked.

The sidecar program is built from this repository. Build it first:

    cargo build -p orvud

Set ORVUD_BIN to use a different one.
"""

import os
import pathlib
import shutil
import signal
import subprocess
import tempfile
import time

import pytest

import orvu
from ports import free_port

REPO = pathlib.Path(__file__).resolve().parents[2]

#: A made-up model for search by meaning, which knows a few dozen words.
#: Every sidecar here is given it, so that no test fetches the real one.
TINY_MODEL = REPO / "crates" / "orvu-search" / "tiny-model"


def short_folder():
    """A new folder of a test's own, taken away at the end of a `with` block.
    A Unix socket path can hold at most 107 bytes, so it is near the top: in
    the folder ORVU_TEST_DIR names, or in /tmp."""
    return tempfile.TemporaryDirectory(prefix="orvu-", dir=os.environ.get("ORVU_TEST_DIR") or "/tmp")


def orvud_bin():
    path = pathlib.Path(os.environ.get("ORVUD_BIN", REPO / "target" / "debug" / "orvud"))
    if not path.exists():
        pytest.exit(f"{path} is missing: run `cargo build -p orvud` first", returncode=2)
    return path


class Sidecar:
    """One running `orvud --local`, with its own data folder and socket."""

    def __init__(self, folder):
        # A Unix socket path can hold at most 107 bytes, so the folder name
        # has to be short. pytest's own temporary folders are too deep.
        self.folder = pathlib.Path(folder)
        self.socket = self.folder / "s"
        self.data = self.folder / "data"
        self.process = None

    def start(self, *extra):
        model = [] if "--model" in extra else ["--model", TINY_MODEL]
        return self.launch([orvud_bin(), "--local", "--data-dir", self.data, "--socket", self.socket, *model, *extra])

    def launch(self, command):
        # What the sidecar prints goes to a file, not a pipe. A pipe that
        # nobody reads fills up, and the sidecar would then stop to wait.
        self.output = self.folder / "output"
        with open(self.output, "w") as output:
            self.process = subprocess.Popen(command, stderr=output)
        # The sidecar prints one line when it is ready to be used.
        ready = wait_until(lambda: "serving on" in self.printed() or self.process.poll() is not None)
        if not ready or self.process.poll() is not None:
            # One that is still on its way is ended here, so that it is not
            # left running, and writing in its folder, once the test is over.
            self.end()
            raise AssertionError(f"orvud did not start: {self.printed()!r}")
        return self

    def printed(self):
        """Everything the sidecar has printed so far."""
        return self.output.read_text()

    def stop(self, seconds=30):
        """Asks the sidecar to stop, and waits until it has. One that has not
        stopped after `seconds` is ended outright, and the test fails. Either
        way it is no longer running when this returns, so its folder can be
        taken away: a sidecar still stopping would go on writing there."""
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=seconds)
            except subprocess.TimeoutExpired:
                self.end()
                raise AssertionError(f"orvud did not stop within {seconds} seconds: {self.printed()!r}") from None
        self.process = None

    def end(self):
        """Ends the sidecar outright, and waits until it has ended."""
        if self.process:
            self.process.kill()
            self.process.wait()
        self.process = None

    def restart(self):
        self.stop()
        return self.start()

    def add_agent(self, name):
        """Lets in the agent `name`, and returns its key."""
        done = subprocess.run(
            [orvud_bin(), "agent", "add", name, "--local", "--data-dir", self.data],
            capture_output=True, text=True, check=True,
        )
        return next(line for line in done.stdout.splitlines() if line.startswith("orvu-key-"))

    def remove_agent(self, name):
        subprocess.run(
            [orvud_bin(), "agent", "remove", name, "--local", "--data-dir", self.data],
            capture_output=True, check=True,
        )

    def add_networked_agent(self, name, address, connection):
        """Lets in the agent `name`, and writes a connection file for it at
        `connection`, for the sidecar at `address`."""
        subprocess.run(
            [orvud_bin(), "agent", "add", name, "--address", address, "--connection", connection,
             "--local", "--data-dir", self.data],
            capture_output=True, check=True,
        )

    def allow(self, name, *rules):
        """Gives the agent `name` rules, such as "notes:write", or takes them
        away with "all"."""
        subprocess.run(
            [orvud_bin(), "agent", "allow", name, *rules, "--local", "--data-dir", self.data],
            capture_output=True, check=True,
        )


@pytest.fixture(autouse=True)
def tiny_model(monkeypatch):
    """A sidecar the package starts by itself searches with the made-up
    model too, so that no test fetches the real one from the internet."""
    monkeypatch.setenv("ORVU_MODEL", str(TINY_MODEL))


@pytest.fixture(autouse=True)
def no_key(monkeypatch):
    """A key, a connection file, a socket or an idle time in the environment
    of whoever runs the tests is not used: every test says which it gives."""
    monkeypatch.delenv("ORVU_KEY", raising=False)
    monkeypatch.delenv("ORVU_CONNECT", raising=False)
    monkeypatch.delenv("ORVU_SOCKET", raising=False)
    monkeypatch.delenv("ORVU_IDLE_SECONDS", raising=False)


@pytest.fixture
def sidecar():
    """A fresh sidecar for one test, with nothing stored in it."""
    with short_folder() as folder:
        running = Sidecar(folder).start()
        try:
            yield running
        finally:
            running.stop()


@pytest.fixture
def client(sidecar):
    """A client connected to this test's sidecar."""
    with orvu.connect(sidecar.socket) as connected:
        yield connected


@pytest.fixture
def team(sidecar):
    """A team on this test's sidecar, used by an agent called alice."""
    with orvu.Team.connect(socket=sidecar.socket, me="alice") as connected:
        yield connected


@pytest.fixture
def bob(sidecar):
    """The same team on the same sidecar, used by a second agent."""
    with orvu.Team.connect(socket=sidecar.socket, me="bob") as connected:
        yield connected


def wait_until(check, seconds=5.0):
    """Waits until `check()` is true. Used where a change arrives a moment
    after the call that caused it."""
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if check():
            return True
        time.sleep(0.02)
    return check()


def _states(pid):
    """The state of each thread of a process, such as "S" for sleeping or "T"
    for stopped. Where the system does not show threads, as on a Mac, it is
    the state of the process as a whole. Raises if the process has ended."""
    if not os.path.exists("/proc/self"):
        # ps says nothing about a process that is gone.
        state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        if not state or state.startswith("Z"):
            raise ProcessLookupError(f"process {pid} has ended")
        return [state[0]]
    try:
        threads = list(pathlib.Path(f"/proc/{pid}/task").iterdir())
    except OSError:
        threads = []
    found = []
    for thread in threads:
        try:
            stat = (thread / "stat").read_text()
        except OSError:
            continue  # the thread has ended
        # The state follows the name, which is in brackets and may hold
        # anything, so it is looked for after the last bracket.
        found.append(stat[stat.rindex(") ") + 2])
    if not found or all(state in "ZX" for state in found):
        raise ProcessLookupError(f"process {pid} has ended")
    return found


def hold_still(process, seconds=30):
    """Holds a process still, and waits until it is. Told to stop, a process
    can go on running for a moment, and so can each of its threads. A thread
    held by a debugger counts as still, and so does one that has ended."""
    if process.poll() is not None:
        raise ProcessLookupError(f"process {process.pid} has ended")
    process.send_signal(signal.SIGSTOP)
    if not wait_until(lambda: all(state in "TtZX" for state in _states(process.pid)), seconds=seconds):
        raise AssertionError(f"process {process.pid} did not stop within {seconds} seconds: {''.join(_states(process.pid))}")


class Machines:
    """A team of sidecars that stand for separate machines. They run on this
    computer, each from a folder `orvud setup` made, and reach each other
    over the real encrypted link."""

    def __init__(self, folder, names=("alpha", "beta", "gamma"), host="127.0.0.1"):
        self.folder = pathlib.Path(folder) / "crew"
        self.names = names
        made = subprocess.run(
            [orvud_bin(), "setup", self.folder, *(f"{name}={host}:{free_port()}" for name in names)],
            capture_output=True,
            text=True,
        )
        assert made.returncode == 0, made.stderr
        #: What the command told the person who ran it.
        self.said = made.stdout
        self.sidecars = {name: Sidecar(self.folder / name) for name in names}
        self.teams = []

    def settings(self, name):
        """The settings file of one machine."""
        return self.folder / name / "orvu.toml"

    def start(self, *names):
        """Starts these machines, or all of them."""
        for name in names or self.names:
            running = self.sidecars[name]
            running.launch([orvud_bin(), "--config", self.settings(name), "--socket", running.socket])

    def team(self, name):
        """The team as an agent on that machine sees it. The agent is called
        after its machine."""
        connected = orvu.Team.connect("crew", socket=self.sidecars[name].socket, me=name)
        self.teams.append(connected)
        return connected

    def stop(self, name):
        """Stops a machine the polite way. It tells the others it is going."""
        self.sidecars[name].stop()

    def kill(self, name):
        """Stops a machine with no warning, as a power cut does."""
        process = self.sidecars[name].process
        process.kill()
        process.wait(timeout=10)
        self.sidecars[name].process = None

    def freeze(self, name):
        """Stops a machine in its tracks without ending it. To the others it
        looks as if its network cable was pulled out."""
        hold_still(self.sidecars[name].process)

    def thaw(self, name):
        self.sidecars[name].process.send_signal(signal.SIGCONT)

    def close(self):
        for connected in self.teams:
            connected.close()
        # Every machine is stopped, even when one of them would not stop.
        problems = []
        for running in self.sidecars.values():
            if running.process is not None:
                # A frozen program does not hear the request to stop.
                running.process.send_signal(signal.SIGCONT)
            try:
                running.stop()
            except AssertionError as problem:
                problems.append(str(problem))
        assert not problems, problems


@pytest.fixture
def machines():
    """Three machines of one team, set up and not started yet."""
    with short_folder() as folder:
        crew = Machines(folder)
        try:
            yield crew
        finally:
            crew.close()


# Found now, because one test empties the PATH.
PGREP = shutil.which("pgrep")


def sidecars(folder):
    """The process ids of the sidecars that were started for `folder`."""
    found = subprocess.run([PGREP, "-f", f"orvud.*{folder}"], capture_output=True, text=True)
    return [int(pid) for pid in found.stdout.split()]


@pytest.fixture
def home(monkeypatch):
    """A home folder for one test. The sidecar's data and socket go under it,
    and whatever was started for it is stopped afterwards."""
    with short_folder() as folder:
        folder = pathlib.Path(folder)
        (folder / "run").mkdir(mode=0o700)
        monkeypatch.setenv("HOME", str(folder))
        monkeypatch.setenv("XDG_RUNTIME_DIR", str(folder / "run"))
        monkeypatch.delenv("XDG_DATA_HOME", raising=False)
        monkeypatch.setenv("ORVUD_BIN", str(orvud_bin()))
        try:
            yield folder
        finally:
            for pid in sidecars(folder):
                os.kill(pid, signal.SIGKILL)
            wait_until(lambda: not sidecars(folder))
