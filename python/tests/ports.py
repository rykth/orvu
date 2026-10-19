"""Picking a port for a machine in a test.

A test that starts a machine has to say which port it listens on, and the
port has to be free. Asking the system for a free port and giving it back
does not do: the system may hand the same port to the next program that
asks, and every link a machine dials asks for one.

So a port is taken from where this system never hands any out, and it is
held for as long as the program that picked it runs, by a lock on one byte
of a shared file: the byte as far into the file as the port's number. The
system lets go of a program's locks when it ends, however it ends, so a port
is never left held by a program that is gone. Test programs that run side by
side, in Python or in Rust, lock the same file and are never given the same
port. A program keeps that one file open, however many ports it picks.

The file is `orvu-test-ports.lock` in the folder TMPDIR names, or in /tmp.
Rust finds its temporary folder that way and no other, so this does too.

If the file is taken away while a program holds ports in it, the program
locks them again in the file that is there now the next time it picks a
port, and one another program took in between cannot be got back. Nothing
tells either program before that. So the file should not be deleted while
tests run; `crates/orvu-test-support/README.md` says how to keep tools that
clear out old files away from it.

Nothing here needs anything but Python, so the clean-machine test can use it
too.
"""

import errno
import fcntl
import itertools
import os
import pathlib
import socket
import threading

#: The file whose bytes are locked to hold a port.
HELD = pathlib.Path(os.environ.get("TMPDIR") or "/tmp") / "orvu-test-ports.lock"

#: The ports to pick from when this system is not known to hand them out.
USUAL = range(20000, 30000)

_turns = itertools.count()
#: The ports this program holds.
_mine = set()
#: `HELD`, open, once it is first needed. It is never closed: closing any
#: file open on it would let go of every lock this program has there.
_file = None
#: The system does not stop two threads of one program locking the same
#: byte, so they take turns, and look at `_mine` first.
_turn = threading.Lock()


def _forget():
    """In a copy of this program, made by `os.fork`, which holds none of the
    locks: it starts afresh, and with a turn of its own, as the thread that
    had the turn was not copied."""
    global _mine, _file, _turn
    if _file is not None:
        os.close(_file)  # the copy holds no locks for this to let go of
    _mine, _file, _turn = set(), None, threading.Lock()


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_forget)


def handed_out():
    """The lowest and highest port the system hands out by itself. Linux
    says. Elsewhere the usual range is taken."""
    try:
        low, high = map(int, pathlib.Path("/proc/sys/net/ipv4/ip_local_port_range").read_text().split())
        return low, high
    except (OSError, ValueError):
        return 49152, 65535


def window(system):
    """The ports to pick from on a system that hands out `system`, which is
    its lowest and highest. `None` if it leaves too few to be worth it."""
    low, high = system
    below, above = range(1024, low), range(high + 1, 65536)
    for gap in (below, above):
        if USUAL.start in gap and USUAL[-1] in gap:
            return USUAL
    gap = max(below, above, key=len)
    if len(gap) < 1000:
        return None
    return gap[-10000:]


def _open():
    """`HELD`, open. Anyone may use it, whoever made it."""
    # Each try fails only if another program put the file in place, or took
    # it away, at the same moment. A hundred in a row means something else is
    # wrong.
    for _ in range(100):
        # Opened as it is, if it is there: in a shared temporary folder,
        # Linux may refuse to open someone else's file in a way that would
        # make it if it were not.
        try:
            return os.open(HELD, os.O_RDWR)
        except FileNotFoundError:
            pass
        # Made under another name and put in place only once anyone may use
        # it, so that a program stopped partway leaves nothing in the way.
        # The name is one no other program makes, even one given the same
        # process number later.
        making = HELD.with_name(f"{HELD.name}.{os.getpid()}.{os.urandom(8).hex()}")
        made = os.open(making, os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o666)
        try:
            os.fchmod(made, 0o666)
            os.link(making, HELD)
            return made
        except FileExistsError:
            os.close(made)
        except BaseException:
            os.close(made)
            raise
        finally:
            making.unlink(missing_ok=True)
    raise RuntimeError(f"{HELD} could be neither opened nor made")


def _held():
    """`HELD`, open. If the file was taken away or put back since it was
    opened, the one there now is opened, and this program's ports are locked
    in it again; one another program took in between cannot be got back."""
    global _file
    if _file is not None:
        try:
            if os.path.samestat(os.fstat(_file), os.stat(HELD)):
                return _file
        except FileNotFoundError:
            pass
        os.close(_file)
        _file = None
    _file = _open()
    for port in _mine:
        try:
            fcntl.lockf(_file, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, port)
        except OSError:
            pass
    return _file


def _hold(port):
    """Locks the byte for `port`. False if another program has it locked."""
    try:
        fcntl.lockf(_file if _file is not None else _held(), fcntl.LOCK_EX | fcntl.LOCK_NB, 1, port)
    except OSError as error:
        if error.errno in (errno.EACCES, errno.EAGAIN):
            return False
        raise
    _mine.add(port)
    return True


def _let_go(port):
    fcntl.lockf(_held(), fcntl.LOCK_UN, 1, port)
    _mine.discard(port)


def release(port):
    """Gives a port back before the program ends. One that cannot be given
    back stays held, until the program ends."""
    with _turn:
        if port in _mine:
            try:
                _let_go(port)
            except OSError:
                pass


def _in_use(port):
    """True if something listens on `port`. Any other trouble with looking
    is raised: going on to the next port would not help."""
    looks = [(family, kind, everywhere) for family, everywhere in ((socket.AF_INET, "0.0.0.0"), (socket.AF_INET6, "::")) for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM)]
    for family, kind, everywhere in looks:
        try:
            with socket.socket(family, kind) as probe:
                probe.bind((everywhere, port))
        except OSError as error:
            if error.errno == errno.EADDRINUSE:
                return True
            # A machine with no IPv6 has nothing to look at there.
            if family == socket.AF_INET6 and error.errno in (errno.EAFNOSUPPORT, errno.EADDRNOTAVAIL):
                continue
            raise
    return False


def free_port(among=None):
    """A port nothing is using, for a machine to listen on. It is this
    program's until the program ends. `among` narrows the choice, for the
    tests of this module."""
    if among is None:
        among = window(handed_out()) or USUAL
    if not among:
        raise RuntimeError("no free port: there are none to choose from")
    if min(among) < 1 or max(among) > 65535:
        raise RuntimeError(f"{among} are not ports, which are 1 to 65535")
    start = os.getpid() * 7919 % len(among)
    # Steps of 13 spread the picks out. Every port is still reached, unless
    # the number of ports is itself a multiple of 13.
    step = 1 if len(among) % 13 == 0 else 13
    with _turn:
        _held()  # once, in case the file was taken away since the last pick
        for _ in range(len(among)):
            port = among[(start + next(_turns) * step) % len(among)]
            if port in _mine or not _hold(port):
                continue
            try:
                busy = _in_use(port)
            except OSError:
                try:
                    _let_go(port)
                except OSError:
                    pass  # the reason to say is why the port could not be looked at
                raise
            if busy:
                _let_go(port)
                continue
            return port
    raise RuntimeError(f"no free port from {among[0]} to {among[-1]}: all are in use or held in {HELD}")

