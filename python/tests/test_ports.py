"""The ports the tests put machines on."""

import os
import pathlib
import signal
import socket
import subprocess
import sys

import pytest

import conftest
import ports
import test_agreement

HERE = pathlib.Path(__file__).resolve().parent


def handed_out():
    """The range of ports the system hands out by itself, read here apart
    from the code under test. `None` if it cannot be read."""
    try:
        low, high = map(int, pathlib.Path("/proc/sys/net/ipv4/ip_local_port_range").read_text().split())
    except (OSError, ValueError):
        return None
    return range(low, high + 1)


# A port that was asked of the system and given back can be handed to the
# next program that asks, before the test has put a machine on it. Every
# link a machine dials asks for one. So the ports for machines are taken from
# where the system never hands any out, and no two are the same.
def test_ports_for_machines_are_not_ones_the_system_hands_out():
    system = handed_out()
    if system is None or ports.window((system[0], system[-1])) is None:
        pytest.skip("this system hands out ports from everywhere, or does not say from where")
    picked = [conftest.free_port() for _ in range(200)]
    assert not [port for port in picked if port in system]
    assert len(set(picked)) == len(picked), "the same port was picked twice"


# Where the ports are taken from follows what this system hands out. A
# place that is right on one machine is wrong on one set up another way.
@pytest.mark.parametrize(
    "system, expected",
    [
        ((32768, 60999), range(20000, 30000)),  # Linux as it comes
        ((49152, 65535), range(20000, 30000)),  # a Mac
        ((10000, 65535), range(1024, 10000)),  # only the bottom is left
        ((1024, 40000), range(55536, 65536)),  # only the top is left
        ((1024, 65535), None),  # nothing is left
        ((1024, 65000), None),  # too little is left to be worth it
    ],
)
def test_where_ports_are_taken_from_follows_what_the_system_hands_out(system, expected):
    assert ports.window(system) == expected


# Several test programs run side by side: the Rust tests, these tests, the
# clean-machine test. None may be given a port another one holds.
def test_programs_running_side_by_side_never_get_the_same_port():
    pick = (
        "import sys; sys.path.insert(0, sys.argv[1]); import ports\n"
        "print(' '.join(str(ports.free_port()) for _ in range(300)), flush=True)\n"
        "sys.stdin.read()\n"  # holds its ports until the others have picked theirs
    )
    programs = [
        subprocess.Popen([sys.executable, "-c", pick, str(HERE)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        for _ in range(4)
    ]
    picked = [int(port) for program in programs for port in program.stdout.readline().split()]
    for program in programs:
        program.stdin.close()
        program.wait(timeout=10)
    assert len(picked) == 1200 and len(set(picked)) == 1200, "two programs were given the same port"
    mine = [conftest.free_port() for _ in range(50)]
    assert len(set(mine)) == 50


# A program that picks ports when it is told to, with its temporary folder
# in `folder`, so that it and the tests' own picking never meet.
PICKER = """
import os, resource, sys
if len(sys.argv) > 2:
    resource.setrlimit(resource.RLIMIT_NOFILE, (int(sys.argv[2]), resource.getrlimit(resource.RLIMIT_NOFILE)[1]))
os.umask(0o077)
sys.path.insert(0, sys.argv[1])
import ports
for line in sys.stdin:
    what, *numbers = line.split()
    try:
        if what == "pick":
            low, high = map(int, numbers)
            print(ports.free_port(among=range(low, high)), flush=True)
        elif what == "where":
            print(ports.HELD, flush=True)
        else:
            ports.release(int(numbers[0]))
            print("given back", flush=True)
    except RuntimeError:
        print("none", flush=True)
"""


class Picker:
    def __init__(self, folder, open_files=None, env=None):
        command = [sys.executable, "-c", PICKER, str(HERE)] + ([str(open_files)] if open_files else [])
        env = env if env is not None else {**os.environ, "TMPDIR": str(folder)}
        self.program = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env)

    def ask(self, line):
        self.program.stdin.write(line + "\n")
        self.program.stdin.flush()
        return self.program.stdout.readline().strip()

    def end(self):
        self.program.kill()
        self.program.wait(timeout=10)


# A port is held for as long as the program that picked it runs, however it
# ends: even one killed outright gives its ports back.
def test_a_port_is_held_until_its_program_ends_however_it_ends(tmp_path):
    port = conftest.free_port()  # held here, so no other program is given it
    pick = f"pick {port} {port + 1}"
    first, second = Picker(tmp_path), Picker(tmp_path)
    try:
        assert first.ask(pick) == str(port)
        assert second.ask(pick) == "none"
        first.end()
        assert second.ask(pick) == str(port)
    finally:
        first.end()
        second.end()


# A port given back can be picked by another program while the first runs.
def test_a_port_given_back_can_be_picked_by_another_program(tmp_path):
    port = conftest.free_port()  # held here, so no other program is given it
    pick = f"pick {port} {port + 1}"
    first, second = Picker(tmp_path), Picker(tmp_path)
    try:
        assert first.ask(pick) == str(port)
        assert first.ask(f"release {port}") == "given back"
        assert second.ask(pick) == str(port)
        assert first.ask(pick) == "none"
    finally:
        first.end()
        second.end()


def given_to_nobody(count):
    """`count` ports next to those `free_port` gives, on the side where the
    system hands none out either, so no program is given any of them."""
    system = ports.handed_out()
    given = ports.window(system) or ports.USUAL
    if given.stop + count <= system[0]:
        return range(given.stop, given.stop + count)
    assert given.start >= 1024 + count, f"no room beside {given}"
    return range(given.start - count, given.start)


# A program may be allowed only a few open files. Picking many ports must not
# use one up for each.
#
# The picker holds its ports in a file of its own, where no other program
# looks, and it looks at each port it picks by listening on it for a moment.
# So it picks from ports no other program is given: one given to a test
# elsewhere could be found in use, at that moment, by that test.
def test_picking_many_ports_does_not_use_up_open_files(tmp_path):
    among = given_to_nobody(500)
    picker = Picker(tmp_path, open_files=64)
    try:
        picked = [picker.ask(f"pick {among.start} {among.stop}") for _ in range(300)]
    finally:
        picker.end()
    assert "none" not in picked and len(set(picked)) == 300, picked[-3:]


# Programs of different people on one machine share where ports are held.
# What one of them makes there, another can use, whatever its own settings.
def test_what_holds_the_ports_can_be_used_by_everyone(tmp_path):
    port = conftest.free_port()  # held here, so no other program is given it
    pick = f"pick {port} {port + 1}"
    picker = Picker(tmp_path)
    try:
        assert picker.ask(pick) == str(port)
    finally:
        picker.end()
    made = list(tmp_path.iterdir())
    assert made and all(path.stat().st_mode & 0o777 in (0o666, 0o1777, 0o777) for path in made), [(path.name, oct(path.stat().st_mode)) for path in made]


# Where ports are held is what a person or another program can find: one
# byte, as far in as the port's number, of `orvu-test-ports.lock` in the
# folder named by TMPDIR, or /tmp. The Rust tests look there too. Here a
# program that knows nothing of how ports are picked looks for itself.
LOOK = """
import fcntl, os, sys
held = os.path.join(os.environ.get("TMPDIR") or "/tmp", "orvu-test-ports.lock")
for port in map(int, sys.argv[1:]):
    looked = os.open(held, os.O_RDWR)
    try:
        fcntl.lockf(looked, fcntl.LOCK_EX | fcntl.LOCK_NB, 1, port)
        print("free", end=" ")
    except OSError:
        print("held", end=" ")
    os.close(looked)
"""


def look(*numbers, env=None):
    said = subprocess.run([sys.executable, "-c", LOOK, *map(str, numbers)], capture_output=True, text=True, check=True, env=env)
    return said.stdout.split()


def test_a_port_held_here_is_held_where_anyone_can_see():
    port = conftest.free_port()
    assert look(port) == ["held"]
    ports.release(port)
    assert look(port) == ["free"]


def test_a_port_is_held_by_one_byte_as_far_in_as_its_number(tmp_path):
    port = conftest.free_port()  # held here, so no other program is given it
    picker = Picker(tmp_path)
    try:
        assert picker.ask(f"pick {port} {port + 1}") == str(port)
        assert look(port - 1, port, port + 1, env={**os.environ, "TMPDIR": str(tmp_path)}) == ["free", "held", "free"]
    finally:
        picker.end()


# Rust has no other names for the temporary folder, so neither does this.
def test_where_ports_are_held_is_found_as_rust_finds_it(tmp_path):
    plain = {name: value for name, value in os.environ.items() if name not in ("TMPDIR", "TMP", "TEMP", "TMP")}
    picker = Picker(tmp_path, env={**plain, "TMP": str(tmp_path), "TEMP": str(tmp_path)})
    try:
        assert picker.ask("where") == "/tmp/orvu-test-ports.lock"
    finally:
        picker.end()


# The file can be taken away while programs hold ports in it, by a person or
# by whatever clears out old files. A program that held a port there takes
# it again in the file that is there now, the next time it picks.
def test_a_port_is_still_held_after_the_file_was_taken_away(tmp_path):
    port, other = conftest.free_port(), conftest.free_port()  # held here
    first, second = Picker(tmp_path), Picker(tmp_path)
    try:
        assert first.ask(f"pick {port} {port + 1}") == str(port)
        (tmp_path / "orvu-test-ports.lock").unlink()
        assert first.ask(f"pick {other} {other + 1}") == str(other)
        assert second.ask(f"pick {port} {port + 1}") == "none"
    finally:
        first.end()
        second.end()


# A program that stops partway through making the file must not leave one
# that others cannot use.
def test_making_the_file_partway_leaves_none_others_cannot_use(tmp_path):
    stops = (
        "import os, sys; sys.path.insert(0, sys.argv[1]); os.umask(0o077); import ports\n"
        "def stop(*_): raise SystemExit(3)\n"
        "os.fchmod = stop\n"
        "ports.free_port(among=range(int(sys.argv[2]), int(sys.argv[2]) + 1))\n"
    )
    port = conftest.free_port()  # held here, so no other program is given it
    stopped = subprocess.run([sys.executable, "-c", stops, str(HERE), str(port)], env={**os.environ, "TMPDIR": str(tmp_path)})
    assert stopped.returncode == 3
    held = tmp_path / "orvu-test-ports.lock"
    assert not held.exists() or held.stat().st_mode & 0o777 == 0o666


# A program can copy itself, as some ways of running work in parallel do.
# The copy holds none of the ports, and can pick, even if the program was in
# the middle of picking when it was copied.
def test_a_copy_of_the_program_holds_nothing_and_can_pick(tmp_path):
    copies = (
        "import os, sys; sys.path.insert(0, sys.argv[1]); import ports\n"
        "low = int(sys.argv[2])\n"
        "ports.free_port(among=range(low, low + 1))\n"
        "ports._turn.acquire()\n"  # as if another thread were picking
        "child = os.fork()\n"
        "if child == 0:\n"
        "    print(sorted(ports._mine), ports.free_port(among=range(low + 1, low + 2)), flush=True)\n"
        "    os._exit(0)\n"
        "ports._turn.release()\n"
        "os.waitpid(child, 0)\n"
    )
    port = conftest.free_port()  # held here with the next one, so no other
    while not ports._hold(port + 1):  # program is given either
        ports.release(port)
        port = conftest.free_port()
    # In a group of its own, so that a copy stuck waiting is stopped with it.
    program = subprocess.Popen([sys.executable, "-c", copies, str(HERE), str(port)], stdout=subprocess.PIPE, text=True, start_new_session=True, env={**os.environ, "TMPDIR": str(tmp_path)})
    try:
        said, _ = program.communicate(timeout=20)
    finally:
        try:
            os.killpg(program.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass  # all of it ended
        program.wait()
    ports.release(port + 1)
    assert said.split() == ["[]", str(port + 1)], said


# A program killed while making the file leaves what it was making behind.
# A program given the same number later must not be stopped by it.
def test_what_a_program_was_making_when_it_ended_does_not_stop_another(tmp_path):
    blocked = (
        "import os, sys; sys.path.insert(0, sys.argv[1]); import ports\n"
        "ports.HELD.with_name(f'{ports.HELD.name}.{os.getpid()}').mkdir()\n"
        "print(ports.free_port(among=range(int(sys.argv[2]), int(sys.argv[2]) + 1)))\n"
    )
    port = conftest.free_port()  # held here, so no other program is given it
    picked = subprocess.run([sys.executable, "-c", blocked, str(HERE), str(port)], capture_output=True, text=True, env={**os.environ, "TMPDIR": str(tmp_path)})
    assert picked.stdout.strip() == str(port), picked.stderr


# A port that cannot be given back is still held, and the program goes on.
def test_a_port_that_cannot_be_given_back_is_still_held(monkeypatch):
    port = conftest.free_port()
    real = ports.fcntl.lockf

    def unlocking_fails(held, how, *where):
        if how == ports.fcntl.LOCK_UN:
            raise OSError(37, "no locks left")
        return real(held, how, *where)

    monkeypatch.setattr(ports.fcntl, "lockf", unlocking_fails)
    ports.release(port)
    assert port in ports._mine
    monkeypatch.undo()
    ports.release(port)
    assert port not in ports._mine


# Ports that are none, or not ports at all, are said to be so.
@pytest.mark.parametrize("among", [range(25000, 25000), range(65530, 65540), range(-5, 5)])
def test_a_choice_of_no_ports_or_not_ports_is_refused(among):
    with pytest.raises(RuntimeError, match="no free port|not ports"):
        ports.free_port(among=among)


# A port something listens on over TCP is in use, though nothing listens on
# it over UDP.
def test_a_port_listened_on_over_tcp_is_not_given():
    port = conftest.free_port()
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listening:
        listening.bind(("127.0.0.1", port))
        listening.listen()
        ports.release(port)
        with pytest.raises(RuntimeError, match="no free port"):
            ports.free_port(among=range(port, port + 1))


def test_when_no_port_is_left_it_says_so_and_does_not_go_on_for_ever():
    port = conftest.free_port()  # held by this program
    with pytest.raises(RuntimeError, match="no free port"):
        ports.free_port(among=range(port, port + 1))


# Not being allowed to look at a port is not the same as the port being in
# use. Going on to the next one would never end.
def test_a_port_that_cannot_be_looked_at_stops_the_picking_with_the_reason(monkeypatch):
    def forbidden(self, address):
        raise PermissionError(13, "not allowed here")

    monkeypatch.setattr(socket.socket, "bind", forbidden)
    with pytest.raises(OSError, match="not allowed here"):
        ports.free_port(among=range(25000, 25010))


# If giving the port back fails as well, the reason picking stopped is still
# the one said.
def test_the_reason_is_said_even_if_giving_the_port_back_fails(monkeypatch):
    def forbidden(self, address):
        raise PermissionError(13, "not allowed here")

    real = ports.fcntl.lockf

    def unlocking_fails(held, how, *where):
        if how == ports.fcntl.LOCK_UN:
            raise OSError(37, "no locks left")
        return real(held, how, *where)

    monkeypatch.setattr(socket.socket, "bind", forbidden)
    monkeypatch.setattr(ports.fcntl, "lockf", unlocking_fails)
    with pytest.raises(OSError, match="not allowed here"):
        ports.free_port(among=range(25000, 25010))


# One picker, used by these tests and by the clean-machine test alike.
def test_there_is_one_way_of_picking_a_port():
    assert test_agreement.free_port is ports.free_port and conftest.free_port is ports.free_port
    script = (HERE.parent / "scripts" / "clean-test.sh").read_text()
    assert "def free_port" not in script and "import ports" in script
