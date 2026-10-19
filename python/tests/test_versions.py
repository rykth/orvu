"""The package and the sidecar have to be the same release."""

import importlib.metadata
import pathlib
import subprocess

import pytest

import orvu
from orvu import _sidecar

from conftest import orvud_bin, sidecars

PACKAGE = pathlib.Path(__file__).resolve().parents[1]


def test_a_sidecar_of_another_release_is_refused(home, monkeypatch):
    monkeypatch.setattr("orvu._client.VERSION", "0.2.0")
    with pytest.raises(orvu.VersionMismatch) as raised:
        orvu.connect()
    assert "0.1.0" in str(raised.value) and "0.2.0" in str(raised.value)
    assert str(_sidecar.default_socket(_sidecar.default_data_dir())) in str(raised.value)


def test_a_sidecar_that_differs_only_in_its_last_number_is_accepted(home, monkeypatch):
    monkeypatch.setattr("orvu._client.VERSION", "0.1.7")
    with orvu.connect() as client:
        assert client.sidecar_version == "0.1.0"


def test_the_package_and_the_sidecar_have_the_same_version():
    said = subprocess.run([orvud_bin(), "--version"], capture_output=True, text=True).stdout
    assert said.strip() == f"orvud {orvu.__version__}"


# "It stops by itself" is not true while programs keep trying to connect, so
# the error has to say how to stop the sidecar: which process it is.
def test_the_error_says_which_process_to_stop(home, monkeypatch):
    with orvu.connect():
        pass
    (pid,) = sidecars(home)
    monkeypatch.setattr("orvu._client.VERSION", "0.2.0")
    with pytest.raises(orvu.VersionMismatch, match=f"kill {pid}"):
        orvu.connect()


# The sidecar is replaced by another release while a program is connected.
# Every call after that has to be refused, not only the first.
def test_a_sidecar_replaced_by_another_release_stays_refused(sidecar, monkeypatch):
    with orvu.connect(sidecar.socket) as client:
        client.counter("visits").increment(1)
        sidecar.restart()
        monkeypatch.setattr("orvu._client.VERSION", "0.2.0")
        for _ in range(3):
            with pytest.raises(orvu.VersionMismatch):
                client.counter("visits").increment(1)
    monkeypatch.undo()
    with orvu.connect(sidecar.socket) as client:
        assert client.counter("visits").value() == 1


def test_the_installed_package_has_the_version_the_code_says():
    assert importlib.metadata.version("orvu") == orvu.__version__
    # The package's version is written down once, in `_version.py`.
    assert 'dynamic = ["version"]' in (PACKAGE / "pyproject.toml").read_text()
