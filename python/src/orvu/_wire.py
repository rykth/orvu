"""Turning the sidecar's replies into Python values and errors.

Nothing here talks to the sidecar. Each function takes a reply that has
already arrived.
"""

import json

from . import errors
from ._proto import dataplane_pb2 as pb

_ERRORS = {
    pb.CODE_TYPE_MISMATCH: errors.TypeMismatch,
    pb.CODE_CLASS_MISMATCH: errors.ClassMismatch,
    pb.CODE_OUT_OF_BOUNDS: errors.OutOfBounds,
    pb.CODE_JSON: errors.JsonError,
    pb.CODE_TOO_LARGE: errors.TooLarge,
    pb.CODE_NO_SEARCH: errors.SearchUnavailable,
}


def failure(failed):
    """The error for a `Failed` reply."""
    return _ERRORS.get(failed.code, errors.EngineError)(failed.detail)


def _value(read, empty, convert):
    """Reads one value out of a `ReadValue`. A reply of "absent" means the
    object does not exist or holds nothing, and gives `empty`."""
    kind = read.WhichOneof("v")
    if kind is None or kind == "absent":
        return empty() if callable(empty) else empty
    return convert(getattr(read, kind))


def number(read):
    return _value(read, 0, int)


def text_or_none(read):
    return _value(read, None, str)


def text(read):
    return _value(read, "", str)


def truth(read):
    return _value(read, False, bool)


def text_list(read):
    return _value(read, list, lambda v: list(v.values))


def text_set(read):
    return _value(read, set, lambda v: set(v.values))


def text_dict(read):
    return _value(read, dict, lambda v: {pair.key: pair.value for pair in v.pairs})


def text_lists_dict(read):
    return _value(read, dict, lambda v: {pair.key: list(pair.values) for pair in v.pairs})


def count_dict(read):
    return _value(read, dict, lambda v: {entry.item: entry.qty for entry in v.items})


def json_or_none(read):
    return _value(read, None, json.loads)


def json_list(read):
    return _value(read, list, json.loads)


def nothing(_read):
    return None


class Op:
    """One operation: the request to send, and how to read the reply.

    `field` is the name of the operation in the sidecar's `Request` message.
    `read` turns a value reply into a Python value. `refusable` marks the
    operations the engine may say no to, which answer `True` or `False`.
    """

    __slots__ = ("field", "message", "read", "refusable")

    def __init__(self, field, message, read=nothing, refusable=False):
        self.field = field
        self.message = message
        self.read = read
        self.refusable = refusable

    @property
    def changes(self):
        """True for an operation that changes something: its answer says
        only whether it was done."""
        return self.read is nothing

    def request(self, session="", request_id=b""):
        request = pb.Request(session=session, request_id=request_id)
        getattr(request, self.field).CopyFrom(self.message)
        return request

    def result(self, response):
        """The Python value for this operation's reply. Raises the matching
        error if the reply says the operation failed."""
        kind = response.WhichOneof("result")
        if kind == "failed":
            raise failure(response.failed)
        if kind == "refused":
            if self.refusable:
                return False
            raise errors.EngineError(f"{self.field} was refused: {response.refused.detail}")
        if kind == "ack":
            return True if self.refusable else None
        if kind == "value":
            return self.read(response.value)
        raise errors.EngineError(f"{self.field} got a reply with nothing in it")
