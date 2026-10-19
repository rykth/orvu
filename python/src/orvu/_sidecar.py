"""Finding the sidecar on this machine, and starting one if none is running.

`orvud` is the program that does the real work. One runs per user, in the
background, and every program of that user shares it. Nobody has to start it
by hand: the first `orvu.connect()` does, and the sidecar stops by itself a
while after the last program has disconnected.
"""

import os
import pathlib
import secrets
import socket as sockets
import struct
import subprocess
import sys
import sysconfig
import time

from . import errors

#: How many seconds a sidecar started from here stays up after the last
#: program has disconnected, unless `ORVU_IDLE_SECONDS` says otherwise.
IDLE_SECONDS = 300

#: How large, in bytes, the sidecar's log may grow before the next sidecar
#: started moves it aside, to `orvud.log.1`. One older log is kept.
LOG_LIMIT = 1024 * 1024


def idle_seconds(env=None):
    """How many seconds a sidecar started from here stays up after the last
    program has disconnected: `ORVU_IDLE_SECONDS`, or `IDLE_SECONDS`."""
    env = os.environ if env is None else env
    given = env.get("ORVU_IDLE_SECONDS")
    if not given:
        return IDLE_SECONDS
    # At most the largest whole number every language holds exactly, which
    # is the most the TypeScript library takes too. A much larger one orvud
    # could not read at all, and would not start.
    if given.isascii() and given.isdigit() and 0 < int(given) <= _MOST_SECONDS:
        return int(given)
    raise errors.SidecarUnavailable(
        "ORVU_IDLE_SECONDS has to be a whole number of seconds above 0 and at most "
        f"{_MOST_SECONDS}, not {given!r}"
    )


_MOST_SECONDS = 2**53 - 1

# The sidecars started by this process. Keeping them here stops Python from
# complaining, when one is forgotten, that it is still running. It is meant to.
_started = []


def _absolute(value):
    """The value as a path, if it is there and is an absolute path."""
    if value and os.path.isabs(value):
        return pathlib.Path(value)
    return None


def default_data_dir(env=None):
    """The folder a local sidecar keeps its data in, or `None` if the system
    gives no home folder to put it in. `orvud --local` uses the same rule."""
    env = os.environ if env is None else env
    home = _absolute(env.get("HOME"))
    if sys.platform == "darwin":
        return home / "Library/Application Support/orvu" if home else None
    shared = _absolute(env.get("XDG_DATA_HOME"))
    if shared:
        return shared / "orvu"
    return home / ".local/share/orvu" if home else None


def default_socket(data_dir, env=None):
    """The socket a local sidecar serves on. `orvud --local` uses the same
    rule."""
    env = os.environ if env is None else env
    runtime = _absolute(env.get("XDG_RUNTIME_DIR"))
    return (runtime or pathlib.Path(data_dir)) / "orvud.sock"


def _noted(data_dir):
    """The socket the sidecar on this data folder was last started on, or
    `None`.

    The socket's place depends on settings a terminal has and a program
    started some other way may not, such as a server an agent's program
    starts. The data folder is found the same way by both, so the sidecar
    notes its socket there for the others while it runs. A program that
    starts a sidecar writes the note too.
    """
    try:
        noted = (data_dir / "orvud.where").read_text().strip()
    except (OSError, ValueError):
        return None
    return _absolute(noted)


def _found(data_dir, socket):
    """The socket a running sidecar answers on: `socket`, or the one noted in
    the data folder. `None` if neither answers."""
    for place in (socket, _noted(data_dir)):
        if place is not None and is_listening(place):
            return place
    return None


def _own_command(path):
    """True if `path` is the `orvud` command installed with this package. It
    only runs the program inside the package, so it is never a sidecar to
    start: started from here it would call itself, or report that the package
    has no program inside."""
    command = os.path.join(sysconfig.get_path("scripts"), "orvud")
    return os.path.exists(command) and os.path.realpath(path) == os.path.realpath(command)


def program(look_on_path=True):
    """The `orvud` program to start: the one named in `$ORVUD_BIN`, or the one
    that came inside this package, or one on the PATH."""
    named = os.environ.get("ORVUD_BIN")
    if named:
        if not (os.path.isfile(named) and os.access(named, os.X_OK)):
            raise errors.SidecarUnavailable(f"$ORVUD_BIN is {named}, which is not a program that can be run")
        if _own_command(named):
            raise errors.SidecarUnavailable(
                f"$ORVUD_BIN is {named}, which is this package's own orvud command and not the "
                f"sidecar program itself"
            )
        return pathlib.Path(named)
    bundled = pathlib.Path(__file__).parent / "_bin" / "orvud"
    if bundled.exists():
        return bundled
    if look_on_path:
        for folder in os.get_exec_path():
            found = os.path.join(folder, "orvud")
            if os.path.isfile(found) and os.access(found, os.X_OK) and not _own_command(found):
                return pathlib.Path(found)
    raise errors.SidecarUnavailable(
        "no sidecar is running and there is no orvud program to start. This copy of the "
        "package came without one: put orvud on the PATH, set $ORVUD_BIN to where it is, or "
        "connect to a sidecar elsewhere over the network with a connection file (ORVU_CONNECT)"
    )


def is_listening(socket):
    """True if something answers on the socket."""
    probe = sockets.socket(sockets.AF_UNIX, sockets.SOCK_STREAM)
    try:
        probe.connect(os.fspath(socket))
        return True
    except OSError:
        return False
    finally:
        probe.close()


def listener(socket):
    """The process id of whatever serves on the socket, or `None` if that
    cannot be found out."""
    probe = sockets.socket(sockets.AF_UNIX, sockets.SOCK_STREAM)
    try:
        probe.connect(os.fspath(socket))
        if sys.platform == "darwin":
            # SOL_LOCAL and LOCAL_PEERPID, which Python has no names for.
            return struct.unpack("i", probe.getsockopt(0, 2, struct.calcsize("i")))[0] or None
        peer = probe.getsockopt(sockets.SOL_SOCKET, sockets.SO_PEERCRED, struct.calcsize("3i"))
        return struct.unpack("3i", peer)[0] or None
    except OSError:
        return None
    finally:
        probe.close()


class _CouldNotStart(Exception):
    """The sidecar was started and stopped again at once. Carries what it
    said."""


# What a program on Windows is told when it asks for a sidecar of its own.
_NOT_ON_WINDOWS = (
    "there is no sidecar for Windows yet. Connect to one on Linux or a Mac over the network, "
    "with the connection file `orvud agent add --connection` makes there: pass it as "
    "connection=, or set ORVU_CONNECT"
)


def ensure_running(timeout=10.0):
    """Makes sure this user's local sidecar is running, starting it if it is
    not, and returns the socket it serves on.

    Any number of programs can call this at the same moment. One of them
    starts the sidecar and the rest wait for it.
    """
    if sys.platform == "win32":
        raise errors.SidecarUnavailable(_NOT_ON_WINDOWS)
    # Not at the top of the file: this module does not exist on Windows, and
    # the package should still load there.
    import fcntl

    data_dir = default_data_dir()
    if data_dir is None:
        raise errors.SidecarUnavailable("there is no home folder to keep data in: set $HOME")
    # Read first, so that a wrong value is told before anything is started.
    idle = idle_seconds()
    socket = default_socket(data_dir)
    found = _found(data_dir, socket)
    if found is not None:
        return found

    deadline = time.monotonic() + timeout
    # The folder is made the way the sidecar would make it: for this user only.
    data_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    # Only one program at a time gets past this lock. The others wait here,
    # and when their turn comes they find the sidecar running.
    with open(data_dir / "start.lock", "w") as turn:
        while True:
            try:
                fcntl.flock(turn, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise errors.SidecarUnavailable(
                        f"another program has been starting the sidecar for {timeout} seconds "
                        f"and it is still not up. Its output is in {data_dir / 'orvud.log'}"
                    ) from None
                time.sleep(0.01)
        last_try = False
        while True:
            found = _found(data_dir, socket)
            if found is not None:
                return found
            try:
                _start(program(), data_dir, socket, deadline, timeout, idle)
                (data_dir / "orvud.where").write_text(os.fspath(socket))
            except _CouldNotStart as failed:
                # A sidecar that is shutting down has given up its socket but
                # holds its data folder until it has saved everything. A new
                # one cannot start until then, so wait for the old one.
                if _in_use(data_dir) and time.monotonic() < deadline:
                    time.sleep(0.05)
                    continue
                # The old one may have let go between our try and our look,
                # so one more try is made before the failure is believed.
                if not last_try:
                    last_try = True
                    continue
                raise errors.SidecarUnavailable(f"the sidecar could not start. It said: {failed}") from None


def _in_use(data_dir):
    """True if a sidecar holds the data folder right now."""
    import fcntl

    try:
        with open(data_dir / "orvud.lock") as held:
            # A sidecar holds this lock for as long as it runs. Asking for it
            # without waiting shows whether one does. It is given back at once.
            fcntl.flock(held, fcntl.LOCK_SH | fcntl.LOCK_NB)
            fcntl.flock(held, fcntl.LOCK_UN)
        return False
    except BlockingIOError:
        return True
    except OSError:
        return False


def _start(orvud, data_dir, socket, deadline, timeout, idle=IDLE_SECONDS):
    """Starts a sidecar and waits until it answers on its socket. The caller
    holds the start lock."""
    # Sidecars started earlier that have stopped since are let go of here.
    _started[:] = [child for child in _started if child.poll() is None]
    log = data_dir / "orvud.log"
    _move_aside(log)
    # Kept open until the sidecar answers or has stopped, and what it said is
    # read through it: another program may move the log aside meanwhile, and
    # the file this one's sidecar writes to is then no longer `orvud.log`.
    with open(log, "a+b") as output:
        _run(orvud, data_dir, socket, deadline, timeout, idle, output)


def _run(orvud, data_dir, socket, deadline, timeout, idle, output):
    """What `_start` does once the log is open as `output`."""
    output.seek(0, os.SEEK_END)
    already_there = output.tell()
    child = subprocess.Popen(
        [
            os.fspath(orvud),
            "--local",
            "--data-dir",
            os.fspath(data_dir),
            "--socket",
            os.fspath(socket),
            "--exit-when-idle",
            str(idle),
        ],
        stdin=subprocess.DEVNULL,
        stdout=output,
        stderr=output,
        # A session of its own, so that Ctrl-C in the program that started
        # it does not stop the sidecar under everyone else.
        start_new_session=True,
    )
    _started.append(child)

    def said():
        size = max(os.fstat(output.fileno()).st_size - already_there, 0)
        return os.pread(output.fileno(), size, already_there).decode(errors="replace").strip()

    while not is_listening(socket):
        if child.poll() is not None:
            raise _CouldNotStart(said())
        if time.monotonic() >= deadline:
            raise errors.SidecarUnavailable(
                f"the sidecar was started but did not answer on {socket} within {timeout} seconds. "
                f"It said: {said()}"
            )
        time.sleep(0.01)


#: How old, in seconds, a lock on moving the log aside has to be before it is
#: taken for one left behind by a program that stopped halfway.
_LOG_LOCK_STALE = 30

#: What a lock file that nobody holds says.
_LOCK_FREE = "free"


def _move_aside(log):
    """Moves the log to `orvud.log.1` if it has grown past `LOG_LIMIT`,
    putting the one there before out of the way. A sidecar that is still
    stopping goes on writing to the file it had, wherever it now is.

    Programs in other languages start sidecars too, and only the one holding
    the lock on the log looks at it and moves it, so that two never both
    find it long and the second moves the first one's new, short log over
    the long one. A program that finds the lock held leaves the log to
    whoever holds it.

    The lock counts as left behind once it is 30 seconds old, so a program
    held up longer than that, stopped or asleep, may find another has taken
    it meanwhile. It looks again just before moving the log, and leaves the
    log if the lock is no longer its own."""
    lock = log.with_name(log.name + ".lock")
    mark = f"{os.getpid()}-{secrets.token_hex(8)}"
    number = _take_lock(lock, mark)
    if number is None:
        return
    try:
        if log.stat().st_size > LOG_LIMIT and _still_held(lock, number, mark):
            os.replace(log, log.with_name(log.name + ".1"))
    except OSError:
        # No log yet, or it cannot be moved: it is added to, as before.
        pass
    finally:
        _give_back(lock, number)


# The lock is a row of numbered files, `orvud.log.lock.1`, `.2`, and so on,
# and only the highest-numbered one counts. It says `free`, or holds the mark
# of the program holding the lock. To take the lock, a program makes the file
# with the next number, which only one program can make; to give it back, it
# makes the one after that, saying `free`. Nothing ever removes, renames or
# writes over the highest-numbered file, so no program can take away a lock
# another has just taken. Files below the highest are left over, and are
# removed by whoever holds the lock next.


def _numbers(lock):
    """The numbers of the lock files there are now, lowest first."""
    prefix = lock.name + "."
    found = []
    for name in os.listdir(lock.parent):
        rest = name[len(prefix):] if name.startswith(prefix) else ""
        if rest.isascii() and rest.isdigit() and rest == str(int(rest)):
            found.append(int(rest))
    return sorted(found)


def _numbered(lock, number):
    return lock.with_name(f"{lock.name}.{number}")


def _can_be_taken(lock, number):
    """True if the lock file numbered `number` says it is free, or has been
    there so long that the program holding it stopped halfway. A file just
    made, whose mark is not written yet, is held."""
    try:
        made = os.open(_numbered(lock, number), os.O_RDONLY)
    except OSError:
        # It went away meanwhile: someone else is busy with the lock.
        return False
    try:
        said = os.read(made, 64)
        changed = os.fstat(made).st_mtime
    except OSError:
        return False
    finally:
        os.close(made)
    return said == _LOCK_FREE.encode() or time.time() - changed >= _LOG_LOCK_STALE


def _make(lock, number, text):
    """Makes the lock file numbered `number`, saying `text`. False if it is
    there already, or cannot be made or written; a file this program made but
    could not write is removed again."""
    path = _numbered(lock, number)
    try:
        made = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    except OSError:
        return False
    try:
        written = os.write(made, text.encode()) == len(text.encode())
    except OSError:
        written = False
    finally:
        os.close(made)
    if not written:
        try:
            os.unlink(path)
        except OSError:
            pass
    return written


def _take_lock(lock, mark=None):
    """Takes the lock on the log, `lock` being `orvud.log.lock`, writing
    `mark` in the file it makes, or a new mark if none is given. Gives back
    the number of the lock file this program made, or None if another
    program holds the lock, or it cannot be taken at all."""
    try:
        before = _numbers(lock)
    except OSError:
        return None
    last = before[-1] if before else 0
    if last and not _can_be_taken(lock, last):
        return None
    mine = last + 1
    if mark is None:
        mark = f"{os.getpid()}-{secrets.token_hex(8)}"
    if not _make(lock, mine, mark):
        # Another program made it first, or it cannot be made.
        return None
    try:
        after = _numbers(lock)
    except OSError:
        after = []
    if after[-1:] != [mine]:
        # This program was so slow that the lock has moved on past it, and
        # what it made counts for nothing.
        return None
    for old in after[:-1]:
        try:
            os.unlink(_numbered(lock, old))
        except OSError:
            pass
    return mine


def _still_held(lock, number, mark):
    """True if lock file `number` is still the highest, and still says
    `mark`: no other program has taken the lock over meanwhile.

    This only makes the gap small. If this program is stopped for more than
    30 seconds after looking and before moving the log, another can take
    the lock over in between, and the log is moved all the same."""
    try:
        if _numbers(lock)[-1:] != [number]:
            return False
        with open(_numbered(lock, number), "rb") as made:
            return made.read(64) == mark.encode()
    except OSError:
        return False


def _give_back(lock, number):
    """Gives back the lock this program took as lock file `number`. If this
    program took so long that another took the lock over, the next number is
    that program's already, and is left."""
    if _make(lock, number + 1, _LOCK_FREE):
        try:
            os.unlink(_numbered(lock, number))
        except OSError:
            pass


def main():
    """Runs the `orvud` program with the arguments given, as if it had been
    called directly. This is what the `orvud` command installed with the
    package does."""
    if sys.platform == "win32":
        # There is no sidecar for Windows to run, and starting a program on
        # Windows does not replace this one, as it does elsewhere.
        sys.exit(f"orvud: {_NOT_ON_WINDOWS}")
    try:
        # The `orvud` on the PATH may be this very command, so it is not
        # looked for there.
        orvud = program(look_on_path=False)
    except errors.SidecarUnavailable as error:
        sys.exit(f"orvud: {error}")
    os.execv(orvud, [os.fspath(orvud), *sys.argv[1:]])
