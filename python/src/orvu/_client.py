"""The connection to the sidecar."""

import asyncio
import json
import os
import time

import grpc

from . import _names, _sidecar
from . import _wire as wire
from . import errors
from ._handles import Objects
from ._proto import dataplane_pb2 as pb
from ._proto import dataplane_pb2_grpc as pb_grpc
from ._version import VERSION

_CLASSES = {"ap": pb.CLASS_AP, "escrow": pb.CLASS_ESCROW}

# Over a Unix socket, gRPC would send the socket's path as the name of the
# server it is talking to. A path is not a valid server name, and the sidecar
# drops such a request. Any ordinary name will do.
_AUTHORITY = ("grpc.default_authority", "localhost")

# The namespace every local sidecar serves.
_LOCAL = "local"

# The name the certificate of a sidecar reached over the network is made out
# to, whatever its address.
_NETWORK_NAME = "orvud"

#: The largest reply a client reads unless told otherwise: 64 MiB.
MAX_REPLY_BYTES = 64 * 1024 * 1024

# How long, in seconds, a watch's new session has to last before the watch
# may open another when that one is dropped too. A watch is sent its value as
# soon as it opens, so a value arriving says nothing about whether the new
# session will last.
_SETTLED_AFTER = 10.0


def _key(key):
    """The key to give: `key`, or `ORVU_KEY` from the environment if it is
    left out. Empty means none."""
    key = os.environ.get("ORVU_KEY", "") if key is None else key
    if not isinstance(key, str):
        raise errors.BadRequest(f"a key is text, as `orvud agent add` printed it, not {type(key).__name__}")
    return key


def _connection(path):
    """What a connection file says: the address, the key and the
    certificate."""
    try:
        with open(path, encoding="utf-8") as file:
            found = json.load(file)
    except OSError as error:
        raise errors.BadRequest(f"cannot read the connection file {os.fspath(path)}: {error}") from None
    except ValueError:
        found = None
    fields = ("address", "key", "certificate")
    if not isinstance(found, dict) or not all(isinstance(found.get(field), str) for field in fields):
        raise errors.BadRequest(
            f"{os.fspath(path)} is not a connection file: `orvud agent add --connection` writes one"
        )
    return tuple(found[field] for field in fields)


#: How long a search waits for its answer, in seconds. The first search on a
#: sidecar may fetch the model it needs, 30 MB, from the internet.
SEARCH_WAIT = 600.0


# What a program is told when nothing answers on the socket it named. Such a
# sidecar is never started by the package, as one that keeps its data on this
# machine is.
_START_A_TEAMS_ONE = (
    "A sidecar shared with other machines is started by hand: `orvud setup` makes "
    "its settings, and `orvud --config orvu.toml --socket <socket>` starts it"
)


def connect(
    socket=None, *, connection=None, namespace="", key=None, timeout=10.0, max_reply_bytes=MAX_REPLY_BYTES
):
    """Connects to a sidecar.

    With no `socket`, this is the sidecar every program of this user shares.
    If it is not running yet it is started, and it stops by itself a while
    after the last program has disconnected.

    With a `socket`, this is the sidecar that serves on that path, such as
    a team's sidecar shared with other machines. It has to be running
    already: `orvud setup` makes the settings for one, and it is started by
    hand. Raises `SidecarUnavailable` if nothing is listening. Left out, it
    is `ORVU_SOCKET` from the environment, if that is set and no
    `connection` is given.

    With a `connection`, the path of a connection file that `orvud agent add
    --connection` wrote, this is a sidecar on another machine, or outside
    this container, reached over the network. The file holds where it is, the
    agent's key, and the certificate that proves the sidecar is the right
    one; the connection is encrypted. Left out, it is `ORVU_CONNECT` from the
    environment, if that is set and no `socket` is given.

    `namespace` can be left empty, which means "whatever this sidecar serves".
    `key` is the agent's key, as `orvud agent add` printed it. A sidecar that
    names agents lets in only programs that give one, and tells each which
    agent it is: see `Client.agent`. Left out, it is `ORVU_KEY` from the
    environment, if that is set. A key the sidecar does not take raises
    `NotAllowed`. `timeout` is how many seconds to wait for the sidecar, starting it
    included. `max_reply_bytes` is the largest reply to read: a larger one
    raises `TooLarge`. A sidecar of another version raises `VersionMismatch`.
    """
    if connection is None and socket is None:
        connection = os.environ.get("ORVU_CONNECT") or None
        socket = os.environ.get("ORVU_SOCKET") or None
        if connection is not None and socket is not None:
            raise errors.BadRequest("ORVU_SOCKET and ORVU_CONNECT are both set: set only one")
    if connection is not None:
        if socket is not None:
            raise errors.BadRequest("give a socket or a connection file, not both")
        address, kept, certificate = _connection(connection)
        if key not in (None, "") and key != kept:
            raise errors.BadRequest("the connection file holds the agent's key: give no other")
        return Client(
            None,
            network=(address, certificate),
            namespace=namespace,
            key=kept,
            timeout=timeout,
            max_reply_bytes=max_reply_bytes,
        )
    key = _key(key)
    if socket is not None:
        try:
            return Client(socket, namespace=namespace, key=key, timeout=timeout, max_reply_bytes=max_reply_bytes)
        except errors.SidecarUnavailable as missing:
            raise errors.SidecarUnavailable(f"{missing}. {_START_A_TEAMS_ONE}") from None

    deadline = time.monotonic() + timeout
    while True:
        left = max(deadline - time.monotonic(), 0.5)
        socket = _sidecar.ensure_running(left)
        try:
            # A sidecar in its last moment may still hold its socket open
            # without answering on it. So each try waits only a short while,
            # and the next one finds the socket gone and starts a sidecar.
            return Client(
                socket,
                namespace=namespace or _LOCAL,
                key=key,
                timeout=timeout,
                ready_within=min(left, 2.0),
                max_reply_bytes=max_reply_bytes,
                restart=True,
            )
        except errors.SidecarUnavailable:
            if time.monotonic() >= deadline:
                raise
        except errors.BadRequest as refused:
            if namespace:
                raise
            raise errors.SidecarUnavailable(
                f"the sidecar on {socket} is not a local one ({refused}). `orvu.connect()` only uses "
                f"a sidecar that keeps its data on this machine. To use that one, give its socket: "
                f"orvu.connect({os.fspath(socket)!r}), or set ORVU_SOCKET to it"
            ) from None


#: How many more times a change is sent when its answer did not come.
_SEND_AGAIN = 3


def request_id():
    """A new id for one change: 16 random bytes. The change is sent with it
    every time it is sent, so the sidecar makes it once."""
    return os.urandom(16)


def _unanswered(error):
    """True if the call may or may not have been made, and no answer came:
    the sidecar could not be reached, stopped partway, or took too long."""
    return error.code() in (grpc.StatusCode.UNAVAILABLE, grpc.StatusCode.DEADLINE_EXCEEDED)


def _same_release(ours, theirs):
    """True if the two versions can work together: the first two numbers are
    the same. The third is for fixes that change nothing a client can see."""
    return ours.split(".")[:2] == theirs.split(".")[:2]


def _never_arrived(error):
    """True if the call failed because nothing could be reached, so the
    sidecar cannot have acted on it. A call that was cut off partway is not
    one of these: the sidecar may have done what it asked."""
    return (
        error.code() == grpc.StatusCode.UNAVAILABLE
        and "failed to connect" in (error.details() or "").lower()
    )


def _stopping(error):
    """True if the sidecar turned the call away because it is stopping. It
    did not run it, so it is as safe to make again as a call that never
    arrived."""
    return (
        error.code() == grpc.StatusCode.UNAVAILABLE
        and (error.details() or "") == "the sidecar is stopping"
    )


def _translate(error):
    """The error to raise for a gRPC failure. The sidecar uses these only for
    a request that is itself wrong, or when it cannot be reached."""
    code = error.code()
    if code == grpc.StatusCode.INVALID_ARGUMENT:
        return errors.BadRequest(error.details())
    if code == grpc.StatusCode.PERMISSION_DENIED:
        return errors.NotAllowed(error.details())
    if code == grpc.StatusCode.UNAVAILABLE:
        return errors.SidecarUnavailable(error.details() or "the sidecar did not answer")
    if code == grpc.StatusCode.DEADLINE_EXCEEDED:
        return errors.TimedOut(
            "the sidecar did not answer in time. What was asked may still have been done"
        )
    # A reply larger than this client reads, a request larger than the
    # sidecar takes, or more live updates open than the sidecar allows.
    if code in (grpc.StatusCode.RESOURCE_EXHAUSTED, grpc.StatusCode.OUT_OF_RANGE):
        return errors.TooLarge(error.details())
    return errors.EngineError(f"{code.name}: {error.details()}")


class Client(Objects):
    """A connection to one sidecar. Get objects from it by name:

        client = orvu.connect("/run/user/1000/orvud.sock")
        visits = client.counter("visits")
        visits.increment(5)

    Close it with `close()`, or use it in a `with` or `async with` block.
    """

    def __init__(
        self,
        socket,
        *,
        namespace="",
        key="",
        timeout=10.0,
        max_reply_bytes=MAX_REPLY_BYTES,
        ready_within=None,
        restart=False,
        network=None,
    ):
        # A sidecar on this machine is reached by its socket, and one on
        # another by its address, over an encrypted connection that trusts
        # only the sidecar's own certificate.
        self._network = network
        if network is None:
            self._socket = os.path.abspath(os.fspath(socket))
            self._target = "unix:" + self._socket
            self._where = self._socket
        else:
            self._socket = None
            self._target = network[0]
            self._where = network[0]
        self._namespace = namespace
        self._key = _key(key)
        self._timeout = timeout
        # True for a client of the sidecar this package starts. Such a client
        # starts it again when it finds it gone.
        self._restart = restart
        self._max_reply_bytes = max_reply_bytes
        if network is None:
            self._options = [_AUTHORITY, ("grpc.max_receive_message_length", max_reply_bytes)]
        else:
            # The certificate is made out to a name and not an address, so
            # that it does for whatever address the sidecar is reached by.
            self._options = [
                ("grpc.ssl_target_name_override", _NETWORK_NAME),
                ("grpc.default_authority", _NETWORK_NAME),
                ("grpc.max_receive_message_length", max_reply_bytes),
            ]
        self._channel = self._new_channel()
        self._stub = pb_grpc.DataPlaneStub(self._channel)
        # The connection `awatch()` uses, and the event loop it belongs to.
        self._aio_channel = None
        self._aio_loop = None
        self._session = ""
        self._resume_token = ""
        #: The number of the machine the sidecar runs as.
        self.replica_id = 0
        #: The version of the sidecar, such as "0.1.0".
        self.sidecar_version = ""
        #: The agent whose key this client gave, as the sidecar checked it, or
        #: `None` if it gave none.
        self.agent = None
        try:
            grpc.channel_ready_future(self._channel).result(timeout=ready_within or timeout)
        except grpc.FutureTimeoutError:
            self._channel.close()
            # Over the network, a sidecar that shows another certificate looks
            # the same as one that is not there.
            also = (
                ", or the one there showed a certificate that is not the one in the connection file"
                if network is not None
                else ""
            )
            raise errors.SidecarUnavailable(
                f"no sidecar answered on {self._where} within {timeout} seconds{also}"
            ) from None
        try:
            self._open()
        except BaseException:
            self._channel.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()

    async def __aenter__(self):
        return self

    async def __aexit__(self, *_exc):
        await self.aclose()

    def another(self):
        """A second connection to the same sidecar, with the same settings.
        Something that runs in the background uses one of its own, so that it
        and the program do not share one connection from two threads."""
        return Client(
            self._socket,
            namespace=self._namespace,
            key=self._key,
            timeout=self._timeout,
            max_reply_bytes=self._max_reply_bytes,
            restart=self._restart,
            network=self._network,
        )

    def _new_channel(self, aio=False):
        """A connection to the sidecar: plain over the socket, and encrypted
        over the network."""
        grpc_or_aio = grpc.aio if aio else grpc
        if self._network is None:
            return grpc_or_aio.insecure_channel(self._target, options=self._options)
        trusted = grpc.ssl_channel_credentials(root_certificates=self._network[1].encode())
        return grpc_or_aio.secure_channel(self._target, trusted, options=self._options)

    def close(self):
        """Closes the connection. Loops over `watch()` and `awatch()` end."""
        self._channel.close()
        channel, loop = self._aio_channel, self._aio_loop
        self._aio_channel = self._aio_loop = None
        if channel is None or loop.is_closed():
            return
        # The `awatch()` connection can only be closed inside its own event
        # loop, so the closing is handed to that loop.
        try:
            running = asyncio.get_running_loop()
        except RuntimeError:
            running = None
        if running is loop:
            loop.create_task(channel.close())
        elif loop.is_running():
            asyncio.run_coroutine_threadsafe(channel.close(), loop)

    # ---- sessions ----------------------------------------------------------

    def _open(self):
        """Starts a session, or picks the old one up again if the sidecar still
        knows it."""
        try:
            self._open_session()
        except grpc.RpcError as error:
            raise _translate(error) from None

    def _open_session(self):
        """What `_open` does, with a failed call raised as it came."""
        request = pb.OpenRequest(
            namespace=self._namespace, resume_token=self._resume_token, credential=self._key.encode()
        )
        opened = self._stub.Open(request, timeout=self._timeout)
        # Checked before the session is kept, so that a sidecar of another
        # release is refused on every call and not only on this one.
        if not _same_release(VERSION, opened.version):
            self._session = self._resume_token = ""
            pid = _sidecar.listener(self._socket) if self._socket else None
            stop = f"Stop that sidecar with `kill {pid}`" if pid else "Stop that sidecar"
            raise errors.VersionMismatch(
                f"the sidecar on {self._where} is version {opened.version or 'unknown'}, and this "
                f"package is version {VERSION}. They have to be the same release. {stop} and connect "
                f"again, or install the matching version of this package. It will not stop by "
                f"itself while programs keep trying to connect to it"
            )
        self._session = opened.session
        self._resume_token = opened.resume_token
        self.replica_id = opened.replica_id
        self.sidecar_version = opened.version
        self.agent = opened.agent or None

    def _call(self, send, again=False):
        """Makes one call. If the sidecar no longer knows our session, which
        happens after it restarts, a new session is opened and the call is made
        once more. A client of the sidecar this package starts also starts it
        again if it has stopped, or once the one that is stopping has gone.
        The sidecar started for the call may itself be told to stop before
        the call gets there, so that is done as often as it takes, for as long
        as the client waits for an answer.

        With `again`, the call carries a request id, and the sidecar makes it
        once however often it arrives. So it is sent again, with the same id,
        whenever its answer did not come, once the sidecar is back."""
        if again:
            return self._call_again(send)
        try:
            return send()
        except grpc.RpcError as error:
            restart = self._restart and (_never_arrived(error) or _stopping(error))
            if not restart and error.code() != grpc.StatusCode.UNAUTHENTICATED:
                raise _translate(error) from None
        deadline = time.monotonic() + self._timeout
        while True:
            if restart:
                self._start_again()
            try:
                self._open_session()
                return send()
            except grpc.RpcError as error:
                restart = self._restart and (_never_arrived(error) or _stopping(error))
                if not restart or time.monotonic() >= deadline:
                    raise _translate(error) from None

    def _call_again(self, send):
        """Sends a call that carries a request id, and sends it again when its
        answer did not come."""
        failed, left = None, _SEND_AGAIN
        deadline = time.monotonic() + self._timeout
        while True:
            try:
                if failed is not None:
                    self._carry_on(failed)
                # Every send is the same request, with the same id.
                return send()
            except grpc.RpcError as error:
                if not _unanswered(error) and error.code() != grpc.StatusCode.UNAUTHENTICATED:
                    raise _translate(error) from None
                # A call that never got to the sidecar, or that a sidecar
                # which was stopping turned away, was not made. It is tried
                # again for as long as the client waits, as any call is. Any
                # other is sent again a few times at most.
                if self._restart and (_never_arrived(error) or _stopping(error)):
                    if time.monotonic() >= deadline:
                        raise _translate(error) from None
                elif left == 0:
                    raise _translate(error) from None
                else:
                    left -= 1
                failed = error

    def _carry_on(self, error):
        """Gets ready to send a call again after `error`: a new session, and
        before that, if the sidecar could not be reached, the sidecar back."""
        if error.code() == grpc.StatusCode.UNAVAILABLE:
            if self._restart:
                self._start_again()
            else:
                self._wait_for_sidecar()
        if error.code() != grpc.StatusCode.DEADLINE_EXCEEDED:
            # A failed call is raised as it came, so that a sidecar that was
            # stopping when the session was opened is tried again too.
            self._open_session()

    def _wait_for_sidecar(self):
        """Waits for a sidecar this package does not start to answer again,
        for the client's timeout at most."""
        deadline = time.monotonic() + self._timeout
        while True:
            left = max(deadline - time.monotonic(), 0.1)
            # As in `_start_again`, a new connection tries at once.
            self._channel.close()
            self._channel = self._new_channel()
            self._stub = pb_grpc.DataPlaneStub(self._channel)
            try:
                grpc.channel_ready_future(self._channel).result(timeout=min(left, 1.0))
                return
            except grpc.FutureTimeoutError:
                if time.monotonic() >= deadline:
                    raise errors.SidecarUnavailable(
                        f"the sidecar on {self._where} stopped and did not come back within {self._timeout} "
                        "seconds. What was asked may have been done"
                    ) from None

    def _start_again(self):
        """Starts the sidecar again and connects to it afresh."""
        deadline = time.monotonic() + self._timeout
        while True:
            left = max(deadline - time.monotonic(), 0.5)
            # It may come back on another socket than the one it was found on.
            self._socket = os.path.abspath(os.fspath(_sidecar.ensure_running(left)))
            self._target = "unix:" + self._socket
            # The old connection has learnt that nothing is there and would
            # wait a while before trying again. A new one tries at once.
            self._channel.close()
            self._channel = self._new_channel()
            self._stub = pb_grpc.DataPlaneStub(self._channel)
            # A sidecar that is stopping holds its socket open without
            # answering on it until it has finished what it was doing, for a
            # few seconds at most. So each try waits only a short while, as
            # `connect()` does, and a later one finds the socket gone and
            # starts a new sidecar.
            try:
                grpc.channel_ready_future(self._channel).result(timeout=min(left, 2.0))
                return
            except grpc.FutureTimeoutError:
                if time.monotonic() >= deadline:
                    raise errors.SidecarUnavailable(
                        f"the sidecar was started again but did not answer on {self._socket}"
                    ) from None

    # ---- operations --------------------------------------------------------

    def _run(self, op, wait=None):
        """Sends `op`, and waits for the answer for the client's timeout, or
        for `wait` seconds if that is longer."""
        timeout = max(self._timeout, wait or 0)
        # A change is sent with an id, the same each time it is sent.
        sent_as = request_id() if op.changes else b""
        response = self._call(
            lambda: self._stub.Execute(op.request(self._session, sent_as), timeout=timeout),
            again=op.changes,
        )
        return op.result(response)

    def search(self, team, query, limit=5):
        """The notes and documents of the team called `team` whose meaning is
        closest to `query`, the closest first, at most `limit` of them, from 1
        to 50. Each is a dict with its "kind" ("note", "structured" or
        "document"), "name", "text" and "score", from 0 to 1, and a
        document's "paragraph", counted from 1. An agent with rules finds
        only what it may read. `Team.search` gives the same as `Found`.

        A search waits ten minutes for its answer, or the client's timeout
        if that is longer, as the first one on a sidecar may fetch the model."""
        if not isinstance(query, str):
            raise errors.BadRequest(f"a search is for some text, not {query!r}")
        if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= 50:
            raise errors.BadRequest(f"a search finds from 1 to 50, not {limit!r}")
        index = _names.index(_names.part(team, "the name of a team"))
        op = wire.Op("search", pb.Search(object=index, query=query, limit=limit), wire.json_list)
        return self._run(op, wait=SEARCH_WAIT)

    def declare(self, name, object_class):
        """Says up front which rules the object `name` follows: `"ap"` for the
        types that always accept a change, `"escrow"` for a counter that cannot
        go below zero. Using the name the other way afterwards raises
        `ClassMismatch`. This is optional: a name is given its class by the
        first operation on it anyway."""
        try:
            wanted = _CLASSES[object_class]
        except KeyError:
            raise errors.BadRequest(f'object_class must be "ap" or "escrow", not {object_class!r}') from None
        return self._run(wire.Op("declare", pb.Declare(object=name, object_class=wanted)))

    def batch(self):
        """Several operations sent in one message. See `Batch`."""
        return Batch(self)

    # ---- watching ----------------------------------------------------------

    def _watch(self, kind, name, read):
        # When a session was last opened again for this watch, or None.
        reopened = None
        while True:
            request = pb.WatchRequest(session=self._session, object=name, kind=kind)
            stream = self._stub.Watch(request)
            try:
                for event in stream:
                    if event.WhichOneof("event") == "failed":
                        raise wire.failure(event.failed)
                    yield read(event.value)
                return
            except grpc.RpcError as error:
                # The loop ends quietly when we closed the connection ourselves.
                if error.code() == grpc.StatusCode.CANCELLED:
                    return
                # The sidecar no longer knows our session. That happens after
                # it restarts, and on a busy sidecar that had to drop it to
                # make room. Open a new one and start watching again, as often
                # as that happens, but not when the last new one was dropped
                # too within a few seconds: on a sidecar with more watches
                # than places, that would go round for ever.
                if error.code() != grpc.StatusCode.UNAUTHENTICATED or self._too_soon(reopened):
                    raise _translate(error) from None
            finally:
                stream.cancel()
            self._open()
            reopened = time.monotonic()

    def _aio_stub(self):
        """The stub `awatch()` uses. Its connection belongs to one event loop,
        so a new loop gets a new connection."""
        loop = asyncio.get_running_loop()
        if self._aio_loop is not loop:
            self._aio_channel = self._new_channel(aio=True)
            self._aio_loop = loop
        return pb_grpc.DataPlaneStub(self._aio_channel)

    @staticmethod
    def _too_soon(reopened):
        """True if a watch's session was opened again too recently to open
        another."""
        return reopened is not None and time.monotonic() - reopened < _SETTLED_AFTER

    async def _awatch(self, kind, name, read):
        # As in `_watch`.
        reopened = None
        while True:
            request = pb.WatchRequest(session=self._session, object=name, kind=kind)
            stream = self._aio_stub().Watch(request)
            try:
                async for event in stream:
                    if event.WhichOneof("event") == "failed":
                        raise wire.failure(event.failed)
                    yield read(event.value)
                return
            except grpc.aio.AioRpcError as error:
                if error.code() == grpc.StatusCode.CANCELLED:
                    return
                if error.code() != grpc.StatusCode.UNAUTHENTICATED or self._too_soon(reopened):
                    raise _translate(error) from None
            except asyncio.CancelledError:
                # Closing the connection cancels the loop. That is a quiet
                # end. Any other cancelling is passed on as usual.
                if self._aio_loop is None:
                    return
                raise
            finally:
                stream.cancel()
            # Opening a session is a blocking call, so it is moved off the
            # event loop.
            await asyncio.to_thread(self._open)
            reopened = time.monotonic()

    async def aclose(self):
        """Closes the connection from `async` code, and waits until the part
        `awatch()` uses is closed too."""
        channel, loop = self._aio_channel, self._aio_loop
        self._aio_channel = self._aio_loop = None
        if channel is not None and loop is asyncio.get_running_loop():
            await channel.close()
        self.close()


class Pending:
    """The result of one operation in a batch. It has no value until the batch
    has been sent."""

    _NOT_SENT = object()

    def __init__(self, op):
        self._op = op
        self._response = Pending._NOT_SENT

    @property
    def value(self):
        """What the operation returned. Raises the operation's error if it
        failed. One operation failing does not undo the others in the batch."""
        if self._response is Pending._NOT_SENT:
            raise errors.BadRequest("the batch has not been sent yet")
        return self._op.result(self._response)


class Batch(Objects):
    """Several operations sent to the sidecar in one message.

        with client.batch() as batch:
            batch.counter("visits").increment(1)
            left = batch.bounded_counter("budget").try_decrement(3)
        print(left.value)

    Inside the block each method returns a `Pending`. The batch is sent when
    the block ends, and after that each `Pending` has its `value`.

    A batch is not all or nothing. The operations run in order and each has
    its own result. The one exception is a request that is itself wrong: then
    the whole batch raises `BadRequest` and nothing has run.

    A batch is sent once. Sending it again does nothing, and nothing can be
    added to it afterwards.
    """

    def __init__(self, client):
        self._client = client
        self._pending = []
        self._sent = False
        #: The `Pending` results, in the order the operations were added.
        self.results = self._pending

    def _run(self, op):
        if self._sent:
            raise errors.BadRequest("the batch has been sent already: start a new one")
        pending = Pending(op)
        self._pending.append(pending)
        return pending

    def _watch(self, *_args):
        raise errors.BadRequest("watch() cannot be part of a batch")

    _awatch = _watch

    def __enter__(self):
        return self

    def __exit__(self, exc_type, *_exc):
        # If the block failed partway, what was collected is not sent.
        if exc_type is None:
            self.send()

    def send(self):
        """Sends the batch and returns the list of `Pending` results."""
        if self._sent:
            return self._pending
        client = self._client
        requests = [pending._op.request() for pending in self._pending]
        # A batch that changes something is sent with an id, as one change
        # is.
        changes = any(pending._op.changes for pending in self._pending)
        sent_as = request_id() if changes else b""
        reply = client._call(
            lambda: client._stub.Batch(
                pb.BatchRequest(session=client._session, requests=requests, request_id=sent_as),
                timeout=client._timeout,
            ),
            again=changes,
        )
        self._sent = True
        for pending, response in zip(self._pending, reply.responses):
            pending._response = response
        return self._pending
