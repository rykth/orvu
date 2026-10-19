"""Guards against the Python client and the sidecar drifting apart.

If the sidecar gains an operation that Python does not send, or Python's copy
of the message definitions falls behind `dataplane.proto`, a test here fails.
"""

import inspect
import subprocess
import sys
import tempfile

from google.protobuf import descriptor_pb2

import orvu
from orvu import _handles, _wire
from orvu._proto import dataplane_pb2 as pb

from conftest import REPO

HANDLES = [
    orvu.Counter, orvu.Register, orvu.Set, orvu.Map, orvu.Cart,
    orvu.List, orvu.Json, orvu.RichText, orvu.Tree, orvu.BoundedCounter, orvu.MvMap,
]


def every_call(client):
    """One call of every method that sends an operation, by class and method."""
    counter, register, a_set = client.counter("c"), client.register("r"), client.set("s")
    a_map, cart, a_list = client.map("m"), client.cart("k"), client.list("l")
    doc, page, tree, budget = client.json("j"), client.rich_text("t"), client.tree("e"), client.bounded_counter("b")
    clashes = client.mv_map("v")
    return {
        "Client.declare": lambda: client.declare("b", "escrow"),
        "Client.search": lambda: client.search("crew", "refund"),
        "Counter.increment": lambda: counter.increment(2),
        "Counter.decrement": lambda: counter.decrement(1),
        "Counter.value": counter.value,
        "Register.set": lambda: register.set("x"),
        "Register.get": register.get,
        "Set.add": lambda: a_set.add("x"),
        "Set.remove": lambda: a_set.remove("x"),
        "Set.contains": lambda: a_set.contains("x"),
        "Set.elements": a_set.elements,
        "Map.put": lambda: a_map.put("k", "v"),
        "Map.remove": lambda: a_map.remove("k"),
        "Map.get": lambda: a_map.get("k"),
        "Map.keys": a_map.keys,
        "Map.entries": a_map.entries,
        "MvMap.put": lambda: clashes.put("k", "v"),
        "MvMap.remove": lambda: clashes.remove("k"),
        "MvMap.get": lambda: clashes.get("k"),
        "MvMap.keys": clashes.keys,
        "MvMap.entries": clashes.entries,
        "Cart.add": lambda: cart.add("x", 2),
        "Cart.remove_qty": lambda: cart.remove_qty("x", 1),
        "Cart.remove_item": lambda: cart.remove_item("x"),
        "Cart.quantity": lambda: cart.quantity("x"),
        "Cart.items": cart.items,
        "List.insert": lambda: a_list.insert(0, "x"),
        "List.delete": lambda: a_list.delete(0),
        "List.get": lambda: a_list.get(0),
        "List.len": a_list.len,
        "List.to_list": a_list.to_list,
        "Json.set": lambda: doc.set(["a"], 1),
        "Json.insert": lambda: doc.insert(["items"], 0, 1),
        "Json.remove": lambda: doc.remove(["a"]),
        "Json.get": lambda: doc.get(["a"]),
        "Json.to_value": doc.to_value,
        "RichText.insert": lambda: page.insert(0, "abc"),
        "RichText.delete": lambda: page.delete(0, 1),
        "RichText.format": lambda: page.format(0, 1, "bold", "true"),
        "RichText.unformat": lambda: page.unformat(0, 1, "bold"),
        "RichText.text": page.text,
        "RichText.to_spans": page.to_spans,
        "Document.append": lambda: orvu.Team(client, "crew", me="alice").document("d").append("abc"),
        "Tree.move_node": lambda: tree.move_node("a", "root"),
        "Tree.move_node_at": lambda: tree.move_node_at("b", "root", 0),
        "Tree.set_label": lambda: tree.set_label("a", "A"),
        "Tree.parent": lambda: tree.parent("a"),
        "Tree.children": lambda: tree.children("root"),
        "Tree.to_nodes": tree.to_nodes,
        "BoundedCounter.increment": lambda: budget.increment(5),
        "BoundedCounter.try_decrement": lambda: budget.try_decrement(1),
        "BoundedCounter.transfer": lambda: budget.transfer(999, 1),
        "BoundedCounter.value": budget.value,
        "BoundedCounter.allowance": budget.allowance,
    }


# Every operation the sidecar offers is sent by some Python method, against a
# real sidecar, and every Python method is in the list above.
def test_python_sends_every_operation_the_sidecar_offers(client, monkeypatch):
    sent = set()
    build = _wire.Op.request

    def recording(op, *args):
        sent.add(op.field)
        return build(op, *args)

    monkeypatch.setattr(_wire.Op, "request", recording)
    calls = every_call(client)
    for call in calls.values():
        call()

    offered = {field.name for field in pb.Request.DESCRIPTOR.oneofs_by_name["op"].fields}
    assert offered - sent == set(), "the sidecar offers operations Python never sends"
    assert sent - offered == set()

    methods = {
        f"{handle.__name__}.{name}"
        for handle in HANDLES
        for name, member in vars(handle).items()
        if inspect.isfunction(member) and not name.startswith("_")
    }
    methods.update({"Client.declare", "Client.search", "Document.append"})
    assert methods == set(calls), "a method is missing from the list in this test, or the list has one too many"


def test_every_kind_of_object_can_be_watched():
    kinds = {value.number for value in pb.Kind.DESCRIPTOR.values} - {pb.KIND_UNSPECIFIED}
    assert {handle._kind for handle in HANDLES} == kinds
    for handle in HANDLES:
        assert handle._whole is not _handles._Handle._whole, f"{handle.__name__} cannot be watched"


def test_every_failure_code_has_a_python_error():
    for value in pb.Code.DESCRIPTOR.values:
        if value.number == pb.CODE_UNSPECIFIED:
            continue
        error = _wire.failure(pb.Failed(code=value.number, detail="x"))
        assert isinstance(error, orvu.OrvuError)
    # The codes that say exactly what went wrong each have an error of their own.
    named = {
        pb.CODE_TYPE_MISMATCH, pb.CODE_CLASS_MISMATCH, pb.CODE_OUT_OF_BOUNDS, pb.CODE_JSON, pb.CODE_TOO_LARGE,
        pb.CODE_NO_SEARCH,
    }
    for code in named:
        assert type(_wire.failure(pb.Failed(code=code))) is not orvu.EngineError


# The message code kept in the package was made from dataplane.proto. This
# builds a fresh description of that file and compares the two, so a change to
# the .proto that was not followed by `sh scripts/generate.sh` is caught.
def test_the_generated_code_matches_the_proto_file():
    with tempfile.TemporaryDirectory() as folder:
        described = f"{folder}/described.bin"
        subprocess.run(
            [
                sys.executable, "-m", "grpc_tools.protoc",
                f"--proto_path={REPO / 'crates' / 'orvud' / 'proto'}",
                f"--descriptor_set_out={described}",
                "orvu/v1/dataplane.proto",
            ],
            check=True,
        )
        fresh = descriptor_pb2.FileDescriptorSet()
        with open(described, "rb") as file:
            fresh.ParseFromString(file.read())
    kept = descriptor_pb2.FileDescriptorProto()
    pb.DESCRIPTOR.CopyToProto(kept)
    fresh = fresh.file[0]
    # A fresh description spells out each field's JSON name. The kept one
    # leaves them to be worked out, so they are dropped before comparing.
    forget_json_names(fresh.message_type)
    assert fresh == kept, "run `sh scripts/generate.sh` in the python folder"


def forget_json_names(messages):
    for message in messages:
        for field in message.field:
            field.ClearField("json_name")
        forget_json_names(message.nested_type)
