"""Agents on other machines, or in containers, reach a sidecar over the
network with a connection file. Every connection is encrypted, and only
agents with keys are let in."""

import json
import os
import pathlib
import subprocess
import sys

import grpc
import pytest

import orvu
from orvu._proto import dataplane_pb2 as pb
from orvu._proto import dataplane_pb2_grpc as pb_grpc

from conftest import Sidecar, orvud_bin, short_folder, wait_until
from ports import free_port


# The tests that connect with no socket run in a home folder of their own. A
# client that failed to read its connection file would otherwise start this
# user's own sidecar, in the real home folder.


@pytest.fixture
def networked():
    """A sidecar that serves the network, and a connection file for alice,
    an agent it lets in."""
    with short_folder() as folder:
        sidecar = Sidecar(folder)
        address = f"127.0.0.1:{free_port()}"
        connection = pathlib.Path(folder) / "alice.orvu"
        sidecar.add_networked_agent("alice", address, connection)
        sidecar.start("--network", address)
        sidecar.address, sidecar.connection = address, connection
        try:
            yield sidecar
        finally:
            sidecar.stop()


def test_an_agent_connects_over_the_network_with_its_connection_file(networked):
    with orvu.Team.connect(connection=networked.connection) as alice:
        assert alice.me == "alice"
        alice.notes.put("deploy", "Friday")
    # The same team, on the same sidecar, through its socket.
    with orvu.Team.connect(socket=networked.socket, key=networked.add_agent("bob")) as bob:
        assert bob.notes.get("deploy") == "Friday"


def test_the_connection_file_can_come_from_the_environment(networked, monkeypatch, home):
    monkeypatch.setenv("ORVU_CONNECT", str(networked.connection))
    with orvu.connect() as client:
        assert client.agent == "alice"
    # An empty key is no key, and does not clash with the file's.
    with orvu.connect(key="") as client:
        assert client.agent == "alice"


def changed(connection, folder, **fields):
    """A copy of a connection file with some of its fields changed."""
    kept = json.loads(pathlib.Path(connection).read_text())
    kept.update(fields)
    copy = pathlib.Path(folder) / "changed.orvu"
    copy.write_text(json.dumps(kept))
    return copy


def test_a_key_the_sidecar_does_not_know_is_refused(networked, tmp_path):
    with pytest.raises(orvu.NotAllowed):
        orvu.connect(connection=changed(networked.connection, tmp_path, key="orvu-key-0123"))


# A program that trusts another certificate does not connect: the sidecar
# could be anybody.
def test_a_sidecar_with_another_certificate_is_not_trusted(networked, tmp_path):
    subprocess.run(
        [orvud_bin(), "agent", "add", "x", "--address", networked.address, "--connection", tmp_path / "other.orvu",
         "--local", "--data-dir", tmp_path / "other"],
        capture_output=True, check=True,
    )
    other = json.loads((tmp_path / "other.orvu").read_text())["certificate"]
    with pytest.raises(orvu.SidecarUnavailable, match="certificate"):
        orvu.connect(connection=changed(networked.connection, tmp_path, certificate=other), timeout=3)


# The network speaks only encrypted: a program that speaks plain gRPC is not
# answered at all, even with a key the sidecar knows.
def test_a_plain_connection_is_not_answered(networked):
    key = json.loads(networked.connection.read_text())["key"]
    channel = grpc.insecure_channel(networked.address)
    with pytest.raises(grpc.RpcError) as refused:
        pb_grpc.DataPlaneStub(channel).Open(pb.OpenRequest(credential=key.encode()), timeout=3)
    channel.close()
    assert refused.value.code() == grpc.StatusCode.UNAVAILABLE


# With the agents file gone, the socket lets everybody in again, as it did
# before agents had keys. The network does not: it lets in only agents with
# keys, and there are none.
def test_without_agents_the_network_lets_nobody_in(networked):
    with orvu.connect(connection=networked.connection) as client:
        client.counter("visits").increment(1)
        (networked.data / "agents").unlink()
        with pytest.raises(orvu.NotAllowed, match="only agents with keys"):
            client.counter("visits").value()
    with orvu.connect(networked.socket) as local:
        assert local.counter("visits").value() == 1


def test_the_network_needs_an_agent_to_start():
    with short_folder() as folder:
        done = subprocess.run(
            [orvud_bin(), "--local", "--data-dir", pathlib.Path(folder) / "data", "--socket", pathlib.Path(folder) / "s",
             "--network", f"127.0.0.1:{free_port()}"],
            capture_output=True, text=True, timeout=30,
        )
    assert done.returncode != 0
    assert "only agents with keys" in done.stderr


def test_an_agent_over_the_network_is_in_the_record(networked):
    with orvu.Team.connect(connection=networked.connection) as alice:
        alice.notes.put("deploy", "Friday")

    def shown():
        return subprocess.run(
            [orvud_bin(), "audit", "--local", "--data-dir", networked.data, "--agent", "alice"],
            capture_output=True, text=True, check=True,
        ).stdout

    assert wait_until(lambda: "key=deploy" in shown())


# An agent's program gives `orvu mcp` the connection file in its
# environment, and needs nothing else.
def test_the_mcp_server_connects_over_the_network(networked, home):
    env = {**os.environ, "ORVU_CONNECT": str(networked.connection)}
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp"],
        input='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}\n',
        capture_output=True, text=True, env=env, timeout=60,
    )
    assert '\\"you\\": \\"alice\\"' in done.stdout, (done.stdout, done.stderr)


def test_a_connection_file_that_is_not_one_is_explained(tmp_path, home):
    wrong = tmp_path / "wrong.orvu"
    for text in ["hello", '{"address": "h:1", "key": "k", "certificate": null}', '{"address": 7441, "key": "k", "certificate": "c"}']:
        wrong.write_text(text)
        with pytest.raises(orvu.BadRequest, match="is not a connection file"):
            orvu.connect(connection=wrong)
    with pytest.raises(orvu.BadRequest, match="not both"):
        orvu.connect("/tmp/s", connection=wrong)


# A server given a socket uses it, and the connection file left in its
# environment does not count: the agent has a name of its own, made up as for
# any agent with no key.
def test_the_mcp_server_with_a_socket_ignores_a_connection_file(networked, home):
    env = {**os.environ, "ORVU_CONNECT": str(networked.connection)}
    (networked.data / "agents").unlink()
    done = subprocess.run(
        [sys.executable, "-m", "orvu._cli", "mcp", "--socket", str(networked.socket)],
        input='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}\n',
        capture_output=True, text=True, env=env, timeout=60,
    )
    assert '\\"you\\": \\"agent-' in done.stdout, (done.stdout, done.stderr)
