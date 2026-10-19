"""Live updates, as a plain loop and as an `async` loop."""

import asyncio
import threading
import time

import grpc

import orvu
from orvu import _client
from orvu._proto import dataplane_pb2 as pb
from orvu._proto import dataplane_pb2_grpc as pb_grpc

from conftest import wait_until


def test_watch_gives_the_value_now_and_then_each_change(sidecar, client):
    visits = client.counter("visits")
    visits.increment(1)
    seen = []

    def follow():
        with orvu.connect(sidecar.socket) as watcher:
            for value in watcher.counter("visits").watch():
                seen.append(value)
                if value == 6:
                    return

    thread = threading.Thread(target=follow)
    thread.start()
    # Wait for the first value, so the changes below happen while watching.
    assert wait_until(lambda: seen)
    visits.increment(2)
    visits.increment(3)
    thread.join(timeout=5)
    assert not thread.is_alive(), f"the loop never saw 6; it saw {seen}"
    assert seen[0] == 1
    assert seen[-1] == 6
    assert seen == sorted(seen)


def test_watch_gives_each_type_its_own_shape(client):
    def first(handle):
        for value in handle.watch():
            return value

    client.register("title").set("Plan")
    client.set("tags").add("red")
    client.map("profile").put("name", "alice")
    client.mv_map("plans").put("launch", "friday")
    client.cart("basket").add("apple", 2)
    client.list("notes").insert(0, "x")
    client.json("doc").set(["done"], True)
    client.rich_text("page").insert(0, "hi")
    client.tree("outline").move_node("a", "root")
    client.bounded_counter("budget").increment(7)

    assert first(client.register("title")) == "Plan"
    assert first(client.set("tags")) == {"red"}
    assert first(client.map("profile")) == {"name": "alice"}
    assert first(client.mv_map("plans")) == {"launch": ["friday"]}
    assert first(client.cart("basket")) == {"apple": 2}
    assert first(client.list("notes")) == ["x"]
    assert first(client.json("doc")) == {"done": True}
    assert first(client.rich_text("page")) == [{"text": "hi", "marks": {}}]
    assert {node["id"] for node in first(client.tree("outline"))} == {"a", "root"}
    assert first(client.bounded_counter("budget")) == 7


def test_watching_the_wrong_type_raises(client):
    client.counter("thing").increment(1)
    try:
        for _ in client.list("thing").watch():
            raise AssertionError("a list watch gave a value for a counter")
    except orvu.TypeMismatch:
        pass
    else:
        raise AssertionError("no error was raised")


def test_watch_waits_for_an_object_that_does_not_exist_yet(sidecar, client):
    seen = []

    def follow():
        with orvu.connect(sidecar.socket) as watcher:
            for value in watcher.register("later").watch():
                seen.append(value)
                return

    thread = threading.Thread(target=follow)
    thread.start()
    # Give the loop time to start watching before anything is written, so it
    # really is watching a name that nothing has been written to.
    time.sleep(0.5)
    assert seen == []
    # Watching did not take the name for a register: it can still be read.
    assert client.register("later").get() is None
    client.register("later").set("here")
    thread.join(timeout=5)
    assert seen == ["here"]


async def test_awatch_works_in_async_code(sidecar):
    client = orvu.connect(sidecar.socket)
    visits = client.counter("visits")
    visits.increment(1)
    seen = []

    async def follow():
        async for value in visits.awatch():
            seen.append(value)
            if value == 4:
                return

    task = asyncio.create_task(follow())
    # Wait for the first value, but not for ever: if the loop failed, stop and
    # let the failure show.
    for _ in range(500):
        if seen or task.done():
            break
        await asyncio.sleep(0.01)
    assert seen, "the loop gave no value"
    # The blocking call is moved off the event loop, as any blocking call
    # in async code should be.
    await asyncio.to_thread(visits.increment, 3)
    await asyncio.wait_for(task, timeout=5)
    assert seen[0] == 1 and seen[-1] == 4
    await client.aclose()


# The sidecar forgets its sessions when it restarts. A loop started after
# that must open a new session by itself, as an ordinary call does.
def test_a_watch_started_after_a_restart_works(sidecar):
    with orvu.connect(sidecar.socket) as client:
        visits = client.counter("visits")
        visits.increment(5)
        sidecar.restart()
        for value in visits.watch():
            assert value == 5
            break


async def test_an_awatch_started_after_a_restart_works(sidecar):
    async with orvu.connect(sidecar.socket) as client:
        visits = client.counter("visits")
        visits.increment(5)
        sidecar.restart()
        async for value in visits.awatch():
            assert value == 5
            break


def first_value(client):
    async def read():
        async for value in client.counter("visits").awatch():
            return value

    return asyncio.run(read())


# Each `asyncio.run` has an event loop of its own. A client used in one must
# still work in the next.
def test_awatch_works_in_one_event_loop_after_another(client):
    client.counter("visits").increment(2)
    assert first_value(client) == 2
    assert first_value(client) == 2


async def test_close_ends_an_awatch_loop(sidecar):
    client = orvu.connect(sidecar.socket)
    visits = client.counter("visits")
    visits.increment(1)
    seen = []

    async def follow():
        async for value in visits.awatch():
            seen.append(value)

    task = asyncio.create_task(follow())
    for _ in range(500):
        if seen or task.done():
            break
        await asyncio.sleep(0.01)
    assert seen == [1]
    client.close()
    await asyncio.wait_for(task, timeout=5)


# ---- a loop that only watches, on a busy sidecar ------------------------------

# The sidecar keeps only so many sessions. When they are all taken, the one
# used longest ago is dropped and its watch ends.
# A loop that only watches has to carry on however often that happens,
# opening a new session each time, as long as each new session lasts a while.
# One that is dropped again soon after has no place to keep, and gives up.
SESSIONS = 1024


class _Crowd:
    """Many other programs, each with a session and a watch open."""

    def __init__(self, socket):
        self._channel = grpc.insecure_channel(f"unix:{socket}", options=[("grpc.default_authority", "localhost")])
        self._stub = pb_grpc.DataPlaneStub(self._channel)
        self._streams = []

    def arrive(self, count):
        for _ in range(count):
            session = self._stub.Open(pb.OpenRequest()).session
            request = pb.WatchRequest(session=session, object="visits", kind=pb.KIND_COUNTER)
            stream = self._stub.Watch(request)
            # The first value says the watch is open.
            next(stream)
            self._streams.append(stream)

    def close(self):
        for stream in self._streams:
            stream.cancel()
        self._channel.close()


def _dropped_twice(sidecar, watch, monkeypatch):
    """Runs `watch(seen, sessions)` in a thread while the sidecar drops its
    session twice, a while apart, and checks it carried on."""
    # A while is ten seconds; the test waits out a shorter one.
    monkeypatch.setattr(_client, "_SETTLED_AFTER", 0.5)
    with orvu.connect(sidecar.socket) as writer:
        writer.counter("visits").increment(1)
    seen, sessions, failed = [], [], []

    def follow():
        try:
            watch(seen, sessions)
        except Exception as error:  # noqa: BLE001 - reported below
            failed.append(error)

    thread = threading.Thread(target=follow, daemon=True)
    thread.start()
    assert wait_until(lambda: seen and sessions)
    crowd = _Crowd(sidecar.socket)
    try:
        for turn in range(2):
            time.sleep(0.6)
            before = sessions[-1]()
            crowd.arrive(SESSIONS)
            assert wait_until(lambda: failed or sessions[-1]() != before, seconds=10), "the session was not dropped"
            assert not failed, f"the loop ended time {turn + 1} its session was dropped: {failed!r}"
        with orvu.connect(sidecar.socket) as writer:
            writer.counter("visits").increment(1)
        assert wait_until(lambda: failed or seen[-1] == 2), seen
        assert not failed, failed
    finally:
        crowd.close()


def test_a_watch_only_loop_carries_on_however_often_its_session_is_dropped(sidecar, monkeypatch):
    def watch(seen, sessions):
        with orvu.connect(sidecar.socket) as watcher:
            sessions.append(lambda: watcher._session)
            for value in watcher.counter("visits").watch():
                seen.append(value)
                if value == 2:
                    return

    _dropped_twice(sidecar, watch, monkeypatch)


def test_an_awatch_only_loop_carries_on_however_often_its_session_is_dropped(sidecar, monkeypatch):
    def watch(seen, sessions):
        async def follow():
            async with orvu.connect(sidecar.socket) as watcher:
                sessions.append(lambda: watcher._session)
                async for value in watcher.counter("visits").awatch():
                    seen.append(value)
                    if value == 2:
                        return

        asyncio.run(follow())

    _dropped_twice(sidecar, watch, monkeypatch)


def _dropped_soon_again(sidecar, watch, monkeypatch):
    """Runs `watch(seen, sessions)` in a thread while the sidecar drops its
    session, and drops the new one straight away, and checks it gave up. A
    watch sends its value as soon as it opens, so that value does not count
    as the new session having lasted."""
    # Opening a thousand sessions can take longer than ten seconds on a busy
    # machine, which would count as the new session having lasted.
    monkeypatch.setattr(_client, "_SETTLED_AFTER", 600.0)
    with orvu.connect(sidecar.socket) as writer:
        writer.counter("visits").increment(1)
    seen, sessions, failed = [], [], []

    def follow():
        try:
            watch(seen, sessions)
        except Exception as error:  # noqa: BLE001 - checked below
            failed.append(error)

    thread = threading.Thread(target=follow, daemon=True)
    thread.start()
    assert wait_until(lambda: seen and sessions)
    crowd = _Crowd(sidecar.socket)
    try:
        before = sessions[-1]()
        crowd.arrive(SESSIONS)
        assert wait_until(lambda: failed or sessions[-1]() != before, seconds=10), "the session was not dropped"
        assert not failed, failed
        # The value is sent again on the new session.
        assert wait_until(lambda: len(seen) >= 2), seen
        crowd.arrive(SESSIONS)
        assert wait_until(lambda: failed, seconds=10), "the loop went on after being dropped again at once"
        assert isinstance(failed[0], orvu.EngineError), failed
        assert "UNAUTHENTICATED" in str(failed[0])
    finally:
        crowd.close()
    thread.join(timeout=10)


def test_a_watch_only_loop_gives_up_when_its_new_session_is_dropped_at_once(sidecar, monkeypatch):
    def watch(seen, sessions):
        with orvu.connect(sidecar.socket) as watcher:
            sessions.append(lambda: watcher._session)
            for value in watcher.counter("visits").watch():
                seen.append(value)

    _dropped_soon_again(sidecar, watch, monkeypatch)


def test_an_awatch_only_loop_gives_up_when_its_new_session_is_dropped_at_once(sidecar, monkeypatch):
    def watch(seen, sessions):
        async def follow():
            async with orvu.connect(sidecar.socket) as watcher:
                sessions.append(lambda: watcher._session)
                async for value in watcher.counter("visits").awatch():
                    seen.append(value)

        asyncio.run(follow())

    _dropped_soon_again(sidecar, watch, monkeypatch)
