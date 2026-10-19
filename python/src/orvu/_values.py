"""Keeping any JSON value where the engine keeps text.

Several kinds of shared object hold text and nothing else: the values of a
map, the items of a list. The blocks let a program put a number, true or
false, a list or a dict there as well. Text is kept as it is. Anything else
is kept as its JSON form behind a mark that ordinary text does not start
with, so that reading can tell the two apart. This is the form notes are
kept in for every program, whatever its language: the TypeScript package and
the sidecar's search read it too.
"""

import json

from . import errors

# The "record separator" character. A text that does start with it is kept
# in the marked form too, so that it comes back as it was.
_MARK = "\x1e"


def to_text(value):
    """The text to keep `value` as. Raises `BadRequest` for a value JSON
    cannot hold."""
    if isinstance(value, str) and not value.startswith(_MARK):
        return value
    try:
        return _MARK + json.dumps(value, ensure_ascii=False, allow_nan=False)
    except (TypeError, ValueError) as error:
        raise errors.BadRequest(f"that value cannot be kept: {error}") from None


def from_text(text):
    """The value that was kept as `text`."""
    if text is not None and text.startswith(_MARK):
        try:
            return json.loads(text[len(_MARK) :])
        except (ValueError, RecursionError):
            # Not something a library wrote. It is given as the text it is.
            return text
    return text
