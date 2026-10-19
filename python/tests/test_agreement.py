"""Replays the shared test cases through Python and two real sidecars.

`conformance/vectors.json` holds cases every implementation of the data types
must agree on. Each `converge` case has one or two machines making changes
without having seen the other's, and the value everyone must end up with.

This test acts that out for real. Two sidecars run as machines 1 and 2,
connected over the encrypted link, each with its own storage. They are first
started apart, with no way to reach each other, and every case's changes are
made on the machine the case names. Then both are restarted so that they can
reach each other, and the test waits for them to agree and compares what each
one reads with what the case expects.

The cases give every change a clock value. A sidecar stamps changes with the
real time, so the changes are made in the order of those clock values, with a
pause between different values. Where two machines have the same value, the
cases settle it in favour of the machine with the higher number, which is what
making machine 1's change first gives.

The `encode` cases in the file compare exact bytes inside the engine, which a
client never sees. They are not replayed here.
"""

import json
import pathlib
import subprocess
import time

import pytest

import orvu

from conftest import REPO, Sidecar, free_port, short_folder, wait_until

CASES = [
    case
    for case in json.loads((REPO / "conformance" / "vectors.json").read_text())["vectors"]
    if case["kind"] == "converge"
]


def keygen_bin():
    path = REPO / "target" / "debug" / "examples" / "keygen"
    if not path.exists():
        pytest.exit(f"{path} is missing: run `cargo build -p orvu-transport --example keygen` first", returncode=2)
    return path


class Machine(Sidecar):
    """A sidecar started from a settings file, as one numbered machine that
    can be connected to others."""

    def __init__(self, folder, number, port):
        super().__init__(folder)
        self.folder.mkdir()
        self.socket = self.folder / "s"
        self.number = number
        self.port = port
        self.settings = self.folder / "orvu.toml"

    def start(self, peers=()):
        self.settings.write_text(
            f"replica = {self.number}\n"
            f'namespace = "agreement"\n'
            f'listen = "127.0.0.1:{self.port}"\n'
            f"peers = {json.dumps([f'127.0.0.1:{peer.port}' for peer in peers])}\n"
            f"[security]\n"
            f'cert = "../certs/node{self.number}.pem"\n'
            f'key = "../certs/node{self.number}-key.pem"\n'
            f'ca = "../certs/ca.pem"\n'
            f"[storage]\n"
            f'path = "{self.folder / "orvu.redb"}"\n'
            f'fsync = "always"\n'
        )
        return self.launch([orvud_bin_path(), "--config", self.settings, "--socket", self.socket])


def orvud_bin_path():
    from conftest import orvud_bin

    return orvud_bin()


def apply(client, case, op):
    """Makes one of a case's changes."""
    kind, name = case["type"], case["name"]
    what = op["kind"]
    if kind == "counter":
        handle = client.counter(name)
        handle.increment(op["by"]) if op["by"] >= 0 else handle.decrement(-op["by"])
    elif kind == "register":
        client.register(name).set(op["value"])
    elif kind == "set":
        getattr(client.set(name), what)(op["value"])
    elif kind == "map":
        handle = client.map(name)
        handle.put(op["key"], op["value"]) if what == "put" else handle.remove(op["key"])
    elif kind == "mvmap":
        handle = client.mv_map(name)
        handle.put(op["key"], op["value"]) if what == "put" else handle.remove(op["key"])
    elif kind == "cart":
        handle = client.cart(name)
        if what == "remove_item":
            handle.remove_item(op["item"])
        elif op["by"] >= 0:
            handle.add(op["item"], op["by"])
        else:
            handle.remove_qty(op["item"], -op["by"])
    elif kind == "list":
        handle = client.list(name)
        handle.insert(op["index"], op["value"]) if what == "insert" else handle.delete(op["index"])
    elif kind == "json":
        handle = client.json(name)
        path = json.loads(op["path"])
        handle.set(path, json.loads(op["value"])) if what == "set" else handle.remove(path)
    elif kind == "richtext":
        handle = client.rich_text(name)
        if what == "insert_char":
            handle.insert(op["index"], op["ch"])
        elif what == "delete":
            handle.delete(op["index"], op["len"])
        elif what == "format":
            handle.format(op["index"], op["len"], op["attr"], op["value"])
        else:
            handle.unformat(op["index"], op["len"], op["attr"])
    elif kind == "tree":
        handle = client.tree(name)
        if what == "move_node":
            handle.move_node(op["child"], op["new_parent"])
        elif what == "move_node_at":
            handle.move_node_at(op["child"], op["new_parent"], op["index"])
        else:
            handle.set_label(op["node"], op["name"])
    elif kind == "bounded":
        handle = client.bounded_counter(name)
        if what == "increment":
            handle.increment(op["n"])
        elif what == "try_decrement":
            assert handle.try_decrement(op["n"]), f"{name}: the case expects this to be allowed"
        else:
            assert handle.transfer(op["to"], op["n"]), f"{name}: the case expects this to be allowed"
    else:
        raise AssertionError(f"no way to replay a {kind} case")


def read(client, case):
    """What a machine holds for a case, in the shape of the case's `expect`."""
    kind, name = case["type"], case["name"]
    if kind == "counter":
        return {"value": client.counter(name).value()}
    if kind == "register":
        return {"value": client.register(name).get()}
    if kind == "set":
        return {"elements": sorted(client.set(name).elements())}
    if kind == "map":
        return {"entries": client.map(name).entries()}
    if kind == "mvmap":
        return {"entries": client.mv_map(name).entries()}
    if kind == "cart":
        return {"items": client.cart(name).items()}
    if kind == "list":
        return {"list": client.list(name).to_list()}
    if kind == "json":
        return {"value": client.json(name).to_value()}
    if kind == "richtext":
        handle = client.rich_text(name)
        return {"text": handle.text(), "spans": handle.to_spans()}
    if kind == "bounded":
        return {"value": client.bounded_counter(name).value()}
    if kind == "tree":
        handle = client.tree(name)
        nodes = handle.to_nodes()
        parents = sorted({node["parent"] for node in nodes if node["parent"] is not None})
        return {"nodes": nodes, "children": {parent: handle.children(parent) for parent in parents}}
    raise AssertionError(f"no way to read a {kind} case")


@pytest.fixture(scope="module")
def machines(tmp_path_factory):
    """Machines 1 and 2 after every case has been replayed on them apart and
    they have then been connected."""

    with short_folder() as folder:
        folder = pathlib.Path(folder)
        subprocess.run(
            [keygen_bin(), "--out", folder / "certs", "--replicas", "1,2"],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        one = Machine(folder / "one", 1, free_port())
        two = Machine(folder / "two", 2, free_port())
        by_number = {1: one, 2: two}

        # Apart: neither machine is told about the other.
        one.start()
        two.start()
        clients = {number: orvu.connect(machine.socket) for number, machine in by_number.items()}
        for case in CASES:
            changes = sorted(
                (op["now_wall"], replica["replica"], position, op)
                for replica in case["replicas"]
                for position, op in enumerate(replica["ops"])
            )
            last_clock = None
            for clock, number, _position, op in changes:
                if clock != last_clock:
                    # A later clock value has to be a later real time.
                    time.sleep(0.003)
                    last_clock = clock
                apply(clients[number], case, op)
        for client in clients.values():
            client.close()
        one.stop()
        two.stop()

        # Together: each machine is told where the other is.
        one.start(peers=[two])
        two.start(peers=[one])
        try:
            yield by_number
        finally:
            one.stop()
            two.stop()


@pytest.mark.parametrize("case", CASES, ids=[case["name"] for case in CASES])
def test_both_machines_end_up_with_what_the_case_expects(machines, case):
    for number, machine in machines.items():
        with orvu.connect(machine.socket) as client:
            agreed = wait_until(lambda: read(client, case) == case["expect"], seconds=15)
            assert agreed, (
                f"machine {number} holds {read(client, case)!r}, "
                f"but the case expects {case['expect']!r}"
            )


def test_every_kind_of_case_can_be_replayed():
    kinds = {case["type"] for case in CASES}
    assert kinds == {
        "counter", "register", "set", "map", "cart", "list", "json", "richtext", "tree", "bounded", "mvmap",
    }
