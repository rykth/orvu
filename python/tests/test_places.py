"""Where the sidecar keeps its data and its socket, and which `orvud` program
is the one to start."""

import os
import pathlib
import subprocess
import sys
import sysconfig
import tempfile

import pytest

import orvu
from orvu import _sidecar

from conftest import orvud_bin, wait_until


def started_by_hand(env):
    """Starts `orvud --local` with nothing but this environment and returns
    the socket it says it serves on."""
    with tempfile.TemporaryFile("w+") as output:
        process = subprocess.Popen([orvud_bin(), "--local"], env=env, stderr=output)
        try:

            def printed():
                output.seek(0)
                return output.read()

            assert wait_until(lambda: "serving on" in printed() or process.poll() is not None)
            assert process.poll() is None, printed()
            return pathlib.Path(printed().split("serving on ")[1].split(" as replica")[0])
        finally:
            process.kill()
            process.wait()


# The package has to look in the same places the sidecar uses when it is
# started by hand, or it would not find a sidecar that is already running.
@pytest.mark.parametrize("case", ["runtime folder", "no runtime folder", "data home"])
def test_the_package_looks_where_the_sidecar_puts_things(home, case):
    env = {"HOME": str(home), "XDG_RUNTIME_DIR": str(home / "run")}
    if case == "no runtime folder":
        del env["XDG_RUNTIME_DIR"]
    if case == "data home":
        env["XDG_DATA_HOME"] = str(home / "elsewhere")

    data_dir = _sidecar.default_data_dir(env)
    socket = _sidecar.default_socket(data_dir, env)

    assert started_by_hand(env) == socket
    assert (data_dir / "replica").exists()


def test_a_folder_that_is_not_an_absolute_path_counts_as_not_set():
    env = {"HOME": "/home/alice", "XDG_DATA_HOME": "data", "XDG_RUNTIME_DIR": ""}
    data_dir = _sidecar.default_data_dir(env)
    if sys.platform == "darwin":
        assert data_dir == pathlib.Path("/home/alice/Library/Application Support/orvu")
    else:
        assert data_dir == pathlib.Path("/home/alice/.local/share/orvu")
    assert _sidecar.default_socket(data_dir, env) == data_dir / "orvud.sock"
    assert _sidecar.default_data_dir({}) is None


def test_the_program_named_in_the_environment_is_the_one_used(home, monkeypatch):
    assert _sidecar.program() == orvud_bin()
    monkeypatch.setenv("ORVUD_BIN", str(home / "missing"))
    with pytest.raises(orvu.SidecarUnavailable, match="ORVUD_BIN"):
        _sidecar.program()


# Installing the package also installs an `orvud` command, which only runs the
# program inside the package. When there is no program inside, that command
# must not be taken for a sidecar: a real one further along the PATH is.
def test_the_packages_own_command_is_not_taken_for_a_sidecar(home, monkeypatch):
    own = pathlib.Path(sysconfig.get_path("scripts"))
    assert (own / "orvud").exists(), "the package is not installed with its command"
    monkeypatch.delenv("ORVUD_BIN")

    monkeypatch.setenv("PATH", f"{own}{os.pathsep}{orvud_bin().parent}")
    assert _sidecar.program() == orvud_bin()

    monkeypatch.setenv("PATH", str(own))
    with pytest.raises(orvu.SidecarUnavailable, match="no orvud program"):
        _sidecar.program()


def test_a_program_named_in_the_environment_has_to_be_a_real_one(home, monkeypatch):
    monkeypatch.setenv("ORVUD_BIN", str(home))
    with pytest.raises(orvu.SidecarUnavailable, match="ORVUD_BIN"):
        _sidecar.program()
    # The package's own command would only call itself, for ever.
    monkeypatch.setenv("ORVUD_BIN", str(pathlib.Path(sysconfig.get_path("scripts")) / "orvud"))
    with pytest.raises(orvu.SidecarUnavailable, match="ORVUD_BIN"):
        _sidecar.program()


# There is no sidecar for Windows. The package must still load there, and say
# so when it is asked to start one.
def test_the_package_loads_where_there_is_no_file_locking():
    loaded = subprocess.run(
        [sys.executable, "-c", "import sys; sys.modules['fcntl'] = None; import orvu"],
        capture_output=True,
        text=True,
    )
    assert loaded.returncode == 0, loaded.stderr


def test_connect_says_so_on_windows(home, monkeypatch):
    monkeypatch.setattr(sys, "platform", "win32")
    with pytest.raises(orvu.SidecarUnavailable, match="Windows"):
        orvu.connect()
