"""Following several shared objects at once, as one loop."""

import queue
import threading

import grpc

from . import _client
from . import _wire as wire
from ._proto import dataplane_pb2 as pb

# Put in the queue when a stream is over and nothing went wrong.
_ENDED = object()


def _pump(stream, events):
    """Tells the loop about everything one stream brings: `None` for a
    change, `_ENDED` when it is over, and an error when it failed."""
    try:
        for event in stream:
            if event.WhichOneof("event") == "failed":
                events.put(wire.failure(event.failed))
                return
            events.put(None)
        events.put(_ENDED)
    except grpc.RpcError as error:
        # Leaving the loop, or closing the connection, cancels the streams.
        # That is not a failure.
        events.put(_ENDED if error.code() == grpc.StatusCode.CANCELLED else error)


class Stop:
    """Ends a `together` loop from another thread, at once, even while it
    waits for news. Once set it stays set."""

    def __init__(self):
        self._lock = threading.Lock()
        self._set = threading.Event()
        self._events = None

    def set(self):
        with self._lock:
            self._set.set()
            if self._events is not None:
                self._events.put(_ENDED)

    def is_set(self):
        return self._set.is_set()

    def wait(self, seconds):
        """Waits `seconds`, or less if it is set meanwhile. True if it is set."""
        return self._set.wait(seconds)

    def _watching(self, events):
        """Tells this where to put word that the loop is to end. True if it
        is set already."""
        with self._lock:
            self._events = events
            return self._set.is_set()


def together(client, handles, snapshot, later=None, stop=None):
    """Gives `snapshot()` now, and again each time any of the objects behind
    `handles` changes and the snapshot is different from the last one given.

    `later`, if given, says after how many seconds to take a new snapshot
    even if nothing changed, or `None` for not until something does.

    A plain `watch()` on an object that does not exist yet stays quiet until
    the object appears. This always gives a first value at once, so a program
    can show "nothing yet" without a special case.

    The loop ends when the connection is closed, or when `stop`, a `Stop`,
    is set.
    """
    nothing_yet = object()
    last = nothing_yet
    reopened = False
    while True:
        # A queue of its own for each try, so that nothing left over from
        # streams that were given up is taken for news.
        events = queue.Queue()
        if stop is not None and stop._watching(events):
            return
        streams = [
            client._stub.Watch(pb.WatchRequest(session=client._session, object=handle.name, kind=handle._kind))
            for handle in handles
        ]
        for stream in streams:
            threading.Thread(target=_pump, args=(stream, events), daemon=True).start()
        try:
            event = None
            while event is None:
                value = snapshot()
                if last is nothing_yet or value != last:
                    last = value
                    yield value
                wait = later() if later is not None else None
                try:
                    event = events.get(timeout=wait)
                except queue.Empty:
                    event = None
                # Several changes may have come in while the caller was busy.
                # They are all covered by taking one new snapshot.
                while event is None and not events.empty():
                    event = events.get()
        finally:
            for stream in streams:
                stream.cancel()
        if event is _ENDED:
            return
        if not isinstance(event, grpc.RpcError):
            raise event
        # The sidecar no longer knows our session, which happens after it
        # restarts. Open a new one and start following again, once.
        if event.code() != grpc.StatusCode.UNAUTHENTICATED or reopened:
            raise _client._translate(event) from None
        reopened = True
        client._open()
