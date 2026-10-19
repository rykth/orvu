"""Several machines on one computer, for the demo and for measuring.

Each "machine" is a sidecar of its own with its own data folder, set up by
`orvud setup` exactly as a real machine is, and they reach each other over
the real encrypted link. Only the network between them is missing: they all
run on this computer.
"""

import os
import pathlib
import shutil
import signal
import socket
import subprocess
import tempfile
import time

from . import _client, _sidecar, errors

#: The first port tried for a machine. The ones after it follow.
FIRST_PORT = 7441


def free_ports(count, first=FIRST_PORT):
    """`count` ports nothing listens on, the lowest from `first` up."""
    found, port = [], first
    while len(found) < count:
        if port > 65535:
            raise errors.SidecarUnavailable(f"there are not {count} free ports from {first} up")
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
            try:
                probe.bind(("0.0.0.0", port))
                found.append(port)
            except OSError:
                pass
        port += 1
    return found


class LocalMachines:
    """Machines of one team, all on this computer. Use it in a `with` block:
    the sidecars are stopped and their folders removed at the end.

        with LocalMachines(["alpha", "beta"]) as machines:
            machines.start()
            team = Team.connect("crew", socket=machines.socket("alpha"))
    """

    def __init__(self, names=("alpha", "beta", "gamma"), ports=None, idle=60):
        self.names = list(names)
        # How many seconds a machine stays up with no program connected to
        # it. It is how the machines stop if this program is ended before it
        # can stop them.
        self._idle = idle
        self._held = {}
        ports = list(ports) if ports else free_ports(len(self.names))
        if len(ports) != len(self.names):
            raise errors.BadRequest(f"{len(self.names)} machines need {len(self.names)} ports, not {len(ports)}")
        self._orvud = _sidecar.program()
        # A short path, because each sidecar's socket goes inside it.
        self.folder = pathlib.Path(tempfile.mkdtemp(prefix="orvu-"))
        self._running = {}
        made = subprocess.run(
            [self._orvud, "setup", self.folder / "crew", *(f"{name}=127.0.0.1:{port}" for name, port in zip(self.names, ports))],
            capture_output=True,
            text=True,
        )
        if made.returncode != 0:
            shutil.rmtree(self.folder, ignore_errors=True)
            raise errors.SidecarUnavailable(f"the machines could not be set up: {made.stderr.strip()}")

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()

    def home(self, name):
        """The folder of one machine: its settings, its certificate and its
        data."""
        return self.folder / "crew" / name

    def socket(self, name):
        """The socket a program gives to use that machine."""
        return self.home(name) / "orvud.sock"

    def pid(self, name):
        """The process number of that machine's sidecar, or `None` if it is
        not running."""
        running = self._running.get(name)
        return running.pid if running is not None and running.poll() is None else None

    def start(self, *names, timeout=10.0):
        """Starts these machines, or all of them, and waits until each
        answers."""
        for name in names or self.names:
            if self.pid(name) is not None:
                continue
            log_file = self.home(name) / "orvud.log"
            with open(log_file, "ab") as log:
                already_there = log.tell()
                self._running[name] = subprocess.Popen(
                    # It stops by itself a while after the last program has
                    # gone, in case this one is ended before it can stop it.
                    [self._orvud, "--config", "orvu.toml", "--socket", "orvud.sock", "--exit-when-idle", str(self._idle)],
                    cwd=self.home(name),
                    stdin=subprocess.DEVNULL,
                    stdout=log,
                    stderr=log,
                )

            def said():
                with open(log_file, "rb") as log:
                    log.seek(already_there)
                    return log.read().decode(errors="replace").strip()

            # The sidecar puts its socket in place before it has joined the
            # others, so the socket being there is not yet "up". It prints
            # one line when it is.
            deadline = time.monotonic() + timeout
            while "serving on" not in said():
                if self.pid(name) is None or time.monotonic() > deadline:
                    raise errors.SidecarUnavailable(f"the machine {name} did not start. It said: {said()}")
                time.sleep(0.002)
            # A connection that is kept for as long as the machine is meant
            # to run. Without it a machine that no program happens to use
            # would count as idle and stop by itself partway through.
            self._held[name] = _client.Client(self.socket(name))

    def stop(self, *names):
        """Stops these machines, or all of them, the polite way."""
        self._end(names, signal.SIGTERM)

    def switch_off(self, *names):
        """Ends these machines with no warning, as pulling the plug does."""
        self._end(names, signal.SIGKILL)

    def _end(self, names, how):
        for name in names or self.names:
            held = self._held.pop(name, None)
            if held is not None:
                held.close()
            running = self._running.pop(name, None)
            if running is None or running.poll() is not None:
                continue
            running.send_signal(how)
            try:
                running.wait(timeout=10)
            except subprocess.TimeoutExpired:
                running.kill()
                running.wait()
            # A sidecar that was ended with no warning leaves its socket
            # file behind. Nothing listens on it.
            try:
                os.unlink(self.socket(name))
            except FileNotFoundError:
                pass

    def close(self):
        """Stops every machine and removes their folders, data included."""
        self.stop()
        shutil.rmtree(self.folder, ignore_errors=True)
