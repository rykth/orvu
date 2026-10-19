"""Several machines on one computer, as the demo and the measuring use them."""

import os
import socket

import pytest

import orvu
from conftest import free_port, orvud_bin, wait_until
from orvu import _machines


@pytest.fixture
def machines(monkeypatch):
    monkeypatch.setenv("ORVUD_BIN", str(orvud_bin()))
    with _machines.LocalMachines(["alpha", "beta"], ports=[free_port(), free_port()]) as running:
        yield running


def test_the_machines_start_and_share(machines):
    machines.start()
    with orvu.Team.connect("crew", socket=machines.socket("alpha")) as alpha:
        alpha.notes.put("deploy-date", "Friday")
    with orvu.Team.connect("crew", socket=machines.socket("beta")) as beta:
        assert wait_until(lambda: beta.notes.get("deploy-date") == "Friday")
    assert machines.pid("alpha") != machines.pid("beta")


def test_a_machine_that_was_switched_off_has_its_data_when_it_is_back(machines):
    machines.start()
    with orvu.Team.connect("crew", socket=machines.socket("beta")) as beta:
        beta.notes.put("deploy-date", "Friday")
        with orvu.Team.connect("crew", socket=machines.socket("alpha")) as alpha:
            assert wait_until(lambda: alpha.notes.get("deploy-date") == "Friday")
    machines.switch_off("beta")
    assert machines.pid("beta") is None and not machines.socket("beta").exists()
    machines.start("beta")
    with orvu.Team.connect("crew", socket=machines.socket("beta")) as beta:
        assert wait_until(lambda: beta.notes.get("deploy-date") == "Friday")


def test_closing_stops_the_sidecars_and_removes_their_folders(monkeypatch):
    monkeypatch.setenv("ORVUD_BIN", str(orvud_bin()))
    with _machines.LocalMachines(["alpha", "beta"], ports=[free_port(), free_port()]) as machines:
        machines.start()
        folder, pids = machines.folder, [machines.pid("alpha"), machines.pid("beta")]
        assert folder.exists() and all(pids)
    assert not folder.exists()
    for pid in pids:
        with pytest.raises(ProcessLookupError):
            os.kill(pid, 0)


def test_ports_that_are_in_use_are_passed_over():
    first = free_port()
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as busy:
        busy.bind(("0.0.0.0", first))
        found = _machines.free_ports(2, first=first)
    assert len(found) == 2 and first not in found and all(port > first for port in found)


def test_what_cannot_be_set_up_is_said(monkeypatch):
    monkeypatch.setenv("ORVUD_BIN", str(orvud_bin()))
    with pytest.raises(orvu.BadRequest, match="2 machines need 2 ports"):
        _machines.LocalMachines(["alpha", "beta"], ports=[free_port()])
    with pytest.raises(orvu.SidecarUnavailable, match="could not be set up"):
        _machines.LocalMachines(["alpha", "two words"], ports=[free_port(), free_port()])


def test_a_machine_whose_port_is_taken_says_so_and_does_not_wait(machines):
    port = int(machines.home("alpha").joinpath("orvu.toml").read_text().split('listen = "0.0.0.0:')[1].split('"')[0])
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as busy:
        busy.bind(("0.0.0.0", port))
        with pytest.raises(orvu.SidecarUnavailable, match="alpha did not start"):
            machines.start("alpha")


# A machine that no program is connected to must not stop by itself while
# the one that started it is still there. A long measurement would otherwise
# lose a machine partway and go on measuring the rest.
def test_a_machine_nobody_talks_to_stays_up_while_its_owner_is_there(monkeypatch):
    import time

    monkeypatch.setenv("ORVUD_BIN", str(orvud_bin()))
    with _machines.LocalMachines(["alpha", "beta"], ports=[free_port(), free_port()], idle=1) as machines:
        machines.start()
        pids = [machines.pid("alpha"), machines.pid("beta")]
        time.sleep(3.5)
        assert [machines.pid("alpha"), machines.pid("beta")] == pids, "a machine stopped by itself"


# If the program that started the machines is ended before it can stop them,
# they stop by themselves a while later.
def test_machines_whose_owner_is_gone_stop_by_themselves(monkeypatch):
    monkeypatch.setenv("ORVUD_BIN", str(orvud_bin()))
    machines = _machines.LocalMachines(["alpha", "beta"], ports=[free_port(), free_port()], idle=1)
    try:
        machines.start()
        running = dict(machines._running)
        # What the owner going away looks like to the sidecars.
        for held in machines._held.values():
            held.close()
        assert wait_until(lambda: all(program.poll() is not None for program in running.values()), 10)
    finally:
        machines.close()
