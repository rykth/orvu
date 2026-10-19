# Watch for changes

Every block and every plain object has `watch()`. It gives the value now,
and again each time it changes, whoever changed it, on any machine.

## Python

```python
from orvu import Team

with Team.connect(me="alice") as team:
    for tasks in team.tasks.watch():
        waiting = [task for task in tasks if task.state == "todo"]
        print(len(waiting), "tasks waiting")
        # a real agent would pick one up here
```

In `async` code, open the client with `async with` and use `awatch()` with
`async for`. Every other call blocks, so run it with `asyncio.to_thread`.

```python
import orvu

async def follow():
    async with orvu.connect() as client:
        async for value in client.register("status").awatch():
            print(value)
```

## TypeScript

```ts
import { Team } from "orvu";

const team = await Team.connect("default", { me: "alice" });
const stop = new AbortController();
for await (const tasks of team.tasks.watch({ signal: stop.signal })) {
  console.log(tasks.length, "tasks on the board");
}
```

Every `watch()` takes `{ signal }`, an `AbortSignal` that ends the loop.

## MCP

The server offers `orvu://inbox`, `orvu://tasks` and `orvu://notes` as
resources. A program that subscribes is sent
`notifications/resources/updated` when one changes, and reads it again to
see what changed.

## What to expect

- **The inbox is different.** Its `watch()` gives each new message as it
  arrives, not the whole inbox, so an empty inbox gives nothing until a
  message comes.
- **You may not see every step.** If a value changes several times quickly,
  a watch may give only the latest. It always ends on the current value.
- **Watches survive a busy sidecar.** A watch whose session was dropped opens
  a new one and carries on. It gives up with `EngineError` only if that
  happens twice within 10 seconds.
- **A watch ends if the sidecar restarts.** It ends with
  `SidecarUnavailable`; start it again.
- **There are limits:** 256 watches per client and 4,096 per sidecar. A
  watch that ends gives its place back.
- **A watch on an object that does not exist yet** stays quiet until the
  object is first written. A block's watch gives its empty value at once.
