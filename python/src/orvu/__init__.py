"""Orvu's Python client. It talks to a running `orvud` sidecar.

    import orvu

    with orvu.connect() as client:
        visits = client.counter("visits")
        visits.increment(5)
        print(visits.value())
"""

from ._client import Batch, Client, Pending, connect
from ._handles import (
    BoundedCounter,
    Cart,
    Counter,
    Json,
    List,
    Map,
    MvMap,
    Register,
    RichText,
    Set,
    Tree,
)
from .errors import (
    BadRequest,
    Clash,
    ClassMismatch,
    EngineError,
    JsonError,
    NotAllowed,
    OrvuError,
    OutOfBounds,
    SearchUnavailable,
    SidecarUnavailable,
    TimedOut,
    TooLarge,
    TicketUnavailable,
    TypeMismatch,
    UnknownTask,
    VersionMismatch,
)
from ._documents import Document
from ._inbox import Message
from ._roster import Member
from ._search import Found
from ._tasks import LEASE, Task
from ._team import Team
from ._version import VERSION as __version__

__all__ = [
    "__version__",
    "connect",
    "Team",
    "Task",
    "LEASE",
    "Document",
    "Message",
    "Member",
    "Client",
    "Batch",
    "Pending",
    "Counter",
    "BoundedCounter",
    "Register",
    "Set",
    "Map",
    "MvMap",
    "Cart",
    "List",
    "Json",
    "RichText",
    "Tree",
    "OrvuError",
    "SidecarUnavailable",
    "TimedOut",
    "BadRequest",
    "TypeMismatch",
    "ClassMismatch",
    "OutOfBounds",
    "JsonError",
    "TooLarge",
    "EngineError",
    "VersionMismatch",
    "UnknownTask",
    "TicketUnavailable",
    "NotAllowed",
    "SearchUnavailable",
    "Clash",
    "Found",
]
