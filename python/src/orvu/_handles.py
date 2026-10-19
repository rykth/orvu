"""The eleven kinds of shared object.

Each class is a small handle: it remembers the object's name and how to reach
the sidecar, and each method sends one operation. Making a handle sends
nothing and creates nothing. An object comes into being with the first write
to it, and reading one that does not exist gives an empty value, not an error.
"""

import functools
import json

from . import _wire as wire
from . import errors
from ._proto import dataplane_pb2 as pb


def _checked(method):
    """Wraps a handle's method so that an argument that cannot be sent, such
    as a negative count or a value JSON cannot hold, raises `BadRequest` like
    any other mistake in a request."""

    @functools.wraps(method)
    def checked(*args, **kwargs):
        try:
            return method(*args, **kwargs)
        except (TypeError, ValueError, OverflowError) as error:
            raise errors.BadRequest(f"{method.__name__}: {error}") from None

    return checked


class _Handle:
    """What every handle has in common."""

    _kind = pb.KIND_UNSPECIFIED

    def __init__(self, runner, name):
        self._runner = runner
        self.name = name

    def __repr__(self):
        return f"{type(self).__name__}({self.name!r})"

    def __init_subclass__(cls, **kwargs):
        super().__init_subclass__(**kwargs)
        for name, method in list(vars(cls).items()):
            if callable(method) and not name.startswith("_"):
                setattr(cls, name, _checked(method))

    def _run(self, field, message, read=wire.nothing, refusable=False):
        return self._runner._run(wire.Op(field, message, read, refusable))

    @staticmethod
    def _whole(read):
        """Reads the object's whole value. `watch` uses it for every item."""
        raise NotImplementedError

    def watch(self):
        """The object's value now, and again each time it changes.

        Use it in a `for` loop. The loop runs until you leave it or the
        connection ends. A value that did not change is not given twice. If
        the object does not exist yet, the loop waits for it.
        """
        return self._runner._watch(self._kind, self.name, type(self)._whole)

    def awatch(self):
        """The same as `watch`, for `async for`."""
        return self._runner._awatch(self._kind, self.name, type(self)._whole)


class Counter(_Handle):
    """A number you add to and subtract from. It may go below zero."""

    _kind = pb.KIND_COUNTER
    _whole = staticmethod(wire.number)

    def increment(self, n=1):
        """Adds `n`."""
        return self._run("counter_increment", pb.CounterIncrement(object=self.name, n=n))

    def decrement(self, n=1):
        """Subtracts `n`."""
        return self._run("counter_decrement", pb.CounterDecrement(object=self.name, n=n))

    def value(self):
        """The current value. 0 if nothing has been written."""
        return self._run("counter_value", pb.CounterValue(object=self.name), wire.number)


class Register(_Handle):
    """One text value. If two machines set it at the same time, the later
    write wins."""

    _kind = pb.KIND_REGISTER
    _whole = staticmethod(wire.text_or_none)

    def set(self, value):
        """Sets the value."""
        return self._run("register_set", pb.RegisterSet(object=self.name, value=value))

    def get(self):
        """The current value, or `None` if it was never set."""
        return self._run("register_get", pb.RegisterGet(object=self.name), wire.text_or_none)


class Set(_Handle):
    """A set of text items. Adding wins over a removal made at the same time."""

    _kind = pb.KIND_SET
    _whole = staticmethod(wire.text_set)

    def add(self, member):
        """Adds `member`."""
        return self._run("set_add", pb.SetAdd(object=self.name, member=member))

    def remove(self, member):
        """Removes `member`."""
        return self._run("set_remove", pb.SetRemove(object=self.name, member=member))

    def contains(self, member):
        """True if `member` is in the set."""
        return self._run("set_contains", pb.SetContains(object=self.name, member=member), wire.truth)

    def elements(self):
        """Everything in the set, as a Python `set`."""
        return self._run("set_elements", pb.SetElements(object=self.name), wire.text_set)


class Map(_Handle):
    """Keys with a text value each. For one key, the later write wins."""

    _kind = pb.KIND_MAP
    _whole = staticmethod(wire.text_dict)

    def put(self, key, value):
        """Sets `key` to `value`."""
        return self._run("map_put", pb.MapPut(object=self.name, key=key, value=value))

    def remove(self, key):
        """Removes `key`."""
        return self._run("map_remove", pb.MapRemove(object=self.name, key=key))

    def get(self, key):
        """The value at `key`, or `None` if the key is not there."""
        return self._run("map_get", pb.MapGet(object=self.name, key=key), wire.text_or_none)

    def keys(self):
        """The keys, as a list."""
        return self._run("map_keys", pb.MapKeys(object=self.name), wire.text_list)

    def entries(self):
        """Every key and value, as a `dict`."""
        return self._run("map_entries", pb.MapEntries(object=self.name), wire.text_dict)


class MvMap(_Handle):
    """Keys with text values that keeps every value written to a key at the
    same time. Where `Map` keeps the later of two clashing writes, this keeps
    both until a write that has seen them replaces them. More than one value at
    a key means a clash nothing has settled yet."""

    _kind = pb.KIND_MV_MAP
    _whole = staticmethod(wire.text_lists_dict)

    def put(self, key, value):
        """Sets `key` to `value`, replacing every value this machine has seen
        there."""
        return self._run("mv_map_put", pb.MvMapPut(object=self.name, key=key, value=value))

    def remove(self, key):
        """Removes every value this machine has seen at `key`."""
        return self._run("mv_map_remove", pb.MvMapRemove(object=self.name, key=key))

    def get(self, key):
        """The values at `key`, as a list: empty if the key is not there, one
        value normally, more while writes clash."""
        return self._run("mv_map_get", pb.MvMapGet(object=self.name, key=key), wire.text_list)

    def keys(self):
        """The keys, as a list."""
        return self._run("mv_map_keys", pb.MvMapKeys(object=self.name), wire.text_list)

    def entries(self):
        """Every key with its values, as a `dict` of lists."""
        return self._run("mv_map_entries", pb.MvMapEntries(object=self.name), wire.text_lists_dict)


class Cart(_Handle):
    """Items with a quantity each. Quantities add up across machines, and a
    quantity can go below zero: nothing stops more being taken than was put."""

    _kind = pb.KIND_CART
    _whole = staticmethod(wire.count_dict)

    def add(self, item, qty=1):
        """Adds `qty` of `item`."""
        return self._run("cart_add", pb.CartAdd(object=self.name, item=item, qty=qty))

    def remove_qty(self, item, qty=1):
        """Takes `qty` of `item` away."""
        return self._run("cart_remove_qty", pb.CartRemoveQty(object=self.name, item=item, qty=qty))

    def remove_item(self, item):
        """Removes `item` altogether."""
        return self._run("cart_remove_item", pb.CartRemoveItem(object=self.name, item=item))

    def quantity(self, item):
        """How many of `item` there are. 0 if it is not in the cart."""
        return self._run("cart_quantity", pb.CartQuantity(object=self.name, item=item), wire.number)

    def items(self):
        """Every item and its quantity, as a `dict`."""
        return self._run("cart_items", pb.CartItems(object=self.name), wire.count_dict)


class List(_Handle):
    """An ordered list of text items."""

    _kind = pb.KIND_LIST
    _whole = staticmethod(wire.text_list)

    def insert(self, index, value):
        """Puts `value` at position `index`. Raises `OutOfBounds` if `index`
        is past the end."""
        return self._run("list_insert", pb.ListInsert(object=self.name, index=index, value=value))

    def delete(self, index):
        """Removes the item at `index`. Raises `OutOfBounds` if there is none."""
        return self._run("list_delete", pb.ListDelete(object=self.name, index=index))

    def get(self, index):
        """The item at `index`, or `None` if there is none."""
        return self._run("list_get", pb.ListGet(object=self.name, index=index), wire.text_or_none)

    def len(self):
        """How many items there are."""
        return self._run("list_len", pb.ListLen(object=self.name), wire.number)

    def to_list(self):
        """Every item, in order."""
        return self._run("list_to_vec", pb.ListToVec(object=self.name), wire.text_list)


class Json(_Handle):
    """A JSON document. A path is a list of keys and positions, such as
    `["users", 0]`. A write may go down through keys but not through a list
    position: to change something inside a list item, replace the whole item.
    """

    _kind = pb.KIND_JSON
    _whole = staticmethod(wire.json_or_none)

    def set(self, path, value):
        """Sets what is at `path` to `value`, which is any JSON value. Objects
        that are missing along the way are created."""
        message = pb.JsonSet(object=self.name, path_json=json.dumps(path), value_json=json.dumps(value))
        return self._run("json_set", message)

    def remove(self, path):
        """Removes what is at `path`."""
        return self._run("json_remove", pb.JsonRemove(object=self.name, path_json=json.dumps(path)))

    def insert(self, path, index, value):
        """Puts `value` into the list at `path`, at position `index`. What
        was there and after it moves up by one, and `index` may be the
        list's length, to add at the end. Where there is nothing yet,
        position 0 starts a list. Where there is something that is not a
        list, this raises `JsonError` and changes nothing.

        Two programs that insert at the same moment both keep their item.
        Setting the whole list again would keep only one of the two lists.
        """
        message = pb.JsonInsert(
            object=self.name, path_json=json.dumps(path), index=index, value_json=json.dumps(value)
        )
        return self._run("json_insert", message)

    def get(self, path):
        """What is at `path`, or `None` if nothing is there."""
        message = pb.JsonGet(object=self.name, path_json=json.dumps(path))
        return self._run("json_get", message, wire.json_or_none)

    def to_value(self):
        """The whole document. `None` if nothing has been written."""
        return self._run("json_to_value", pb.JsonToValue(object=self.name), wire.json_or_none)


class RichText(_Handle):
    """Text with formatting. Positions count characters, as Python strings do."""

    _kind = pb.KIND_RICHTEXT
    _whole = staticmethod(wire.json_list)

    def insert(self, index, text):
        """Inserts `text` at position `index`. Past the end means at the end."""
        return self._run("richtext_insert", pb.RichtextInsert(object=self.name, index=index, text=text))

    def delete(self, index, length):
        """Deletes `length` characters starting at `index`."""
        return self._run("richtext_delete", pb.RichtextDelete(object=self.name, index=index, len=length))

    def format(self, index, length, attr, value):
        """Sets the mark `attr` to `value` on `length` characters from `index`.
        `value` must not be empty. Use `unformat` to remove a mark."""
        message = pb.RichtextFormat(object=self.name, index=index, len=length, attr=attr, value=value)
        return self._run("richtext_format", message)

    def unformat(self, index, length, attr):
        """Removes the mark `attr` from `length` characters from `index`."""
        message = pb.RichtextUnformat(object=self.name, index=index, len=length, attr=attr)
        return self._run("richtext_unformat", message)

    def text(self):
        """The text with no formatting."""
        return self._run("richtext_text", pb.RichtextText(object=self.name), wire.text)

    def to_spans(self):
        """The text as runs that share the same marks: a list of
        `{"text": ..., "marks": {...}}`."""
        return self._run("richtext_to_spans", pb.RichtextToSpans(object=self.name), wire.json_list)


class Tree(_Handle):
    """Items nested under other items. A move that would put an item under
    itself is skipped, so the tree never has a loop in it."""

    _kind = pb.KIND_TREE
    _whole = staticmethod(wire.json_list)

    def move_node(self, child, new_parent):
        """Puts `child` under `new_parent`."""
        message = pb.TreeMoveNode(object=self.name, child=child, new_parent=new_parent)
        return self._run("tree_move_node", message)

    def move_node_at(self, child, new_parent, index):
        """Puts `child` under `new_parent`, at position `index` among its
        children."""
        message = pb.TreeMoveNodeAt(object=self.name, child=child, new_parent=new_parent, index=index)
        return self._run("tree_move_node_at", message)

    def set_label(self, node, label):
        """Gives `node` a label."""
        return self._run("tree_set_label", pb.TreeSetLabel(object=self.name, node=node, label=label))

    def parent(self, node):
        """The parent of `node`, or `None` for the top and for a node that
        was never placed."""
        return self._run("tree_parent", pb.TreeParent(object=self.name, node=node), wire.text_or_none)

    def children(self, node):
        """The children of `node`, in order."""
        return self._run("tree_children", pb.TreeChildren(object=self.name, node=node), wire.text_list)

    def to_nodes(self):
        """Every node: a list of
        `{"id": ..., "parent": ..., "label": ..., "position": ...}`."""
        return self._run("tree_to_nodes", pb.TreeToNodes(object=self.name), wire.json_list)


class BoundedCounter(_Handle):
    """A number that can never go below zero, such as a budget.

    Each machine has an allowance: its share of the total, which it may spend
    without asking anyone. Whoever adds to the counter gets that amount as
    allowance.
    """

    _kind = pb.KIND_BOUNDED
    _whole = staticmethod(wire.number)

    def increment(self, n):
        """Adds `n`. This machine gets it as allowance."""
        return self._run("bounded_increment", pb.BoundedIncrement(object=self.name, n=n))

    def try_decrement(self, n):
        """Takes `n` away if this machine's allowance covers it.

        Returns `True` if it did. Returns `False` if there was not enough, in
        which case nothing changed. Running out is not an error.
        """
        message = pb.BoundedTryDecrement(object=self.name, n=n)
        return self._run("bounded_try_decrement", message, refusable=True)

    def transfer(self, to_replica, n):
        """Hands `n` of this machine's allowance to the machine numbered
        `to_replica`. Returns `False`, changing nothing, if this machine has
        less than `n`."""
        message = pb.BoundedTransfer(object=self.name, to_replica=to_replica, n=n)
        return self._run("bounded_transfer", message, refusable=True)

    def value(self):
        """The current total."""
        return self._run("bounded_value", pb.BoundedValue(object=self.name), wire.number)

    def allowance(self):
        """How much this machine may still take away."""
        return self._run("bounded_allowance", pb.BoundedAllowance(object=self.name), wire.number)


class Objects:
    """The methods that hand out handles. `Client` and `Batch` both have them."""

    def counter(self, name):
        """A number you add to and subtract from."""
        return Counter(self, name)

    def bounded_counter(self, name):
        """A number that can never go below zero."""
        return BoundedCounter(self, name)

    def register(self, name):
        """One text value."""
        return Register(self, name)

    def set(self, name):
        """A set of text items."""
        return Set(self, name)

    def map(self, name):
        """Keys with a text value each."""
        return Map(self, name)

    def mv_map(self, name):
        """Keys with text values that keeps clashing writes."""
        return MvMap(self, name)

    def cart(self, name):
        """Items with a quantity each."""
        return Cart(self, name)

    def list(self, name):
        """An ordered list of text items."""
        return List(self, name)

    def json(self, name):
        """A JSON document."""
        return Json(self, name)

    def rich_text(self, name):
        """Text with formatting."""
        return RichText(self, name)

    def tree(self, name):
        """Items nested under other items."""
        return Tree(self, name)
