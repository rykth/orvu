# Your first agent in TypeScript

Two Node programs sharing notes, a task board and a budget. You need the
package built and installed: see [Install](install.md).

The TypeScript package and the Python package are two doors to the same
data. A Node program and a Python program on the same team see the same
notes, tasks and budgets.

## 1. Share a note

Save this as `alice.mjs` and run it with `node alice.mjs`:

```ts
import { Team } from "@orvuhq/orvu";

const team = await Team.connect("default", { me: "alice" });
await team.notes.put("deploy-date", "Friday");
console.log(await team.notes.all());
await team.close();
```

And this as `bob.mjs`, from a second terminal:

```ts
import { Team } from "@orvuhq/orvu";

const team = await Team.connect("default", { me: "bob" });
console.log(await team.notes.get("deploy-date")); // Friday
await team.close();
```

The first `connect` starts the sidecar in the background. A team is closed
with `close()`, or with `await using`.

## 2. Tasks and a budget

```ts
import { Team } from "@orvuhq/orvu";

const team = await Team.connect("default", { me: "alice" });

const task = await team.tasks.add("Summarise the Q3 report");
if (await team.tasks.claim(task)) {
  await team.tasks.stillWorking(task); // now and then, or the claim runs out
  await team.tasks.finish(task, "It went up.");
}

const tokens = team.budget("tokens");
await tokens.topUp(2000);
console.log(await tokens.spend(1500)); // true
console.log(await tokens.spend(1500)); // false: not enough left, nothing spent

await team.close();
```

## 3. React to changes

```ts
import { Team } from "@orvuhq/orvu";

const team = await Team.connect("default", { me: "alice" });
for await (const tasks of team.tasks.watch()) {
  console.log(tasks.length, "tasks on the board");
  break; // a real agent would carry on here
}
await team.close();
```

Every `watch()` takes `{ signal }`, an `AbortSignal` that ends the loop.

## Things that differ from Python

- Every call is `async`.
- The set, map and list handles are called `TextSet`, `TextMap` and
  `TextList`, so they do not hide JavaScript's own `Set`, `Map` and `Array`.
- Method names are in camel case: `stillWorking`, `topUp`, `writtenBy`.
- Positions in text count characters as `[...text].length` does, not as a
  string's `length`, which counts some emoji as two.

## Next

- [`typescript/examples/quickstart.mjs`](../../typescript/examples/quickstart.mjs),
  the shortest program that uses Orvu.
- [TypeScript API overview](../reference/typescript-api.md)
- [Use the building blocks](../how-to/building-blocks.md)
