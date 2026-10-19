// Live updates on the building blocks.

import assert from "node:assert/strict";
import { test } from "node:test";
import { sleep, teamOn, waitUntil, withTeam } from "./support.ts";

// Every block gives a first value at once, even when nothing has been
// written yet, so a program can show "nothing yet" without a special case.
test("every block gives a first value at once", () =>
  withTeam(async (team) => {
    for (const [watched, empty] of [
      [team.notes.watch(), {}],
      [team.tasks.watch(), []],
      [team.budget("tokens").watch(), 0],
      [team.tally("done").watch(), {}],
      [team.roster.watch(), []],
    ] as const) {
      const first = await watched.next();
      assert.deepEqual(first.value, empty);
      await watched.return(undefined);
    }
    assert.deepEqual(await team.index(), {}, "watching must not create anything");
  }));

test("notes are followed as they change", () =>
  withTeam(async (team, bob) => {
    const seen: Record<string, unknown>[] = [];
    const follow = (async () => {
      for await (const notes of bob.notes.watch()) {
        seen.push(notes);
        if (notes.b === "2") {
          return;
        }
      }
    })();
    assert.ok(await waitUntil(() => seen.length > 0));
    await team.notes.put("a", "1");
    await team.notes.put("b", "2");
    await follow;
    assert.deepEqual(seen[0], {});
    assert.deepEqual(seen.at(-1), { a: "1", b: "2" });
  }));

test("a budget is followed as it is spent", () =>
  withTeam(async (team, bob) => {
    const seen: number[] = [];
    const follow = (async () => {
      for await (const left of bob.budget("tokens").watch()) {
        seen.push(left);
        if (left === 6) {
          return;
        }
      }
    })();
    assert.ok(await waitUntil(() => seen.length > 0));
    await team.budget("tokens").topUp(10);
    await team.budget("tokens").spend(4);
    await follow;
    assert.deepEqual([seen[0], seen.at(-1)], [0, 6]);
  }));

// Moving a task changes the tree and not the board itself. The loop has to
// see that too.
test("the board is followed when a task is moved", () =>
  withTeam(async (team, bob) => {
    const report = await team.tasks.add("Write the report");
    const draft = await team.tasks.add("Draft");
    const follow = (async () => {
      for await (const tasks of bob.tasks.watch()) {
        if (tasks.some((task) => task.parent === report.id)) {
          return;
        }
      }
    })();
    await sleep(200);
    await team.tasks.move(draft, report);
    await follow;
  }));

// Adding a task changes two objects, and each reports its change. The loop
// gives the new board once, not twice.
test("a value that did not change is not given twice", () =>
  withTeam(async (team, bob) => {
    const seen: number[] = [];
    const follow = (async () => {
      for await (const tasks of bob.tasks.watch()) {
        seen.push(tasks.length);
        if (tasks.length === 6) {
          return;
        }
      }
    })();
    assert.ok(await waitUntil(() => seen.length > 0));
    for (let number = 0; number < 6; number++) {
      await team.tasks.add(`task ${number}`);
    }
    await follow;
    assert.ok(seen.every((count, at) => at === 0 || count !== seen[at - 1]), seen.join());
  }));

// Leaving a loop stops the watches underneath it at once. Twenty loops left
// one after another leave nothing behind, and the client still works.
test("leaving a loop cleans up after itself", () =>
  withTeam(async (team) => {
    for (let i = 0; i < 20; i++) {
      for await (const _ of team.tasks.watch()) {
        break;
      }
    }
    assert.equal(team.client.watching, 0, "watches were left open");
    await team.tasks.add("still works");
    assert.equal((await team.tasks.list()).length, 1);
  }));

// An abort signal ends a loop that is waiting for the next change.
test("a signal ends a loop that is waiting", () =>
  withTeam(async (team) => {
    const stop = new AbortController();
    const seen: unknown[] = [];
    const follow = (async () => {
      for await (const tasks of team.tasks.watch({ signal: stop.signal })) {
        seen.push(tasks);
      }
    })();
    assert.ok(await waitUntil(() => seen.length === 1));
    assert.equal(team.client.watching, 2, "the board and the tree are watched");
    stop.abort();
    await follow;
    assert.equal(seen.length, 1);
    assert.equal(team.client.watching, 0, "watches were left open");
  }));

// Closing the team ends a loop that is waiting for the next change, as it
// does for a loop on a plain object.
test("closing the team ends a loop", () =>
  withTeam(async (_team, _bob, sidecar) => {
    const watcher = await teamOn(sidecar, "watcher");
    const seen: unknown[] = [];
    const follow = (async () => {
      for await (const tasks of watcher.tasks.watch()) {
        seen.push(tasks);
      }
    })();
    assert.ok(await waitUntil(() => seen.length === 1));
    await watcher.close();
    await follow;
  }));

// The sidecar forgets its sessions when it restarts. A loop started after
// that opens a new session by itself and carries on.
test("a loop started after a restart goes on working", () =>
  withTeam(async (team, _bob, sidecar) => {
    await team.tasks.add("first");
    await sidecar.restart();
    const seen: number[] = [];
    const follow = (async () => {
      for await (const tasks of team.tasks.watch()) {
        seen.push(tasks.length);
        if (tasks.length === 2) {
          return;
        }
      }
    })();
    assert.ok(await waitUntil(() => seen.length > 0));
    const other = await teamOn(sidecar, "bob");
    await other.tasks.add("second");
    await other.close();
    await follow;
    assert.deepEqual(seen, [1, 2]);
  }));

// A change can be waiting to be looked at when the team is closed. The loop
// then ends quietly, as it does when nothing is waiting.
test("closing the team with a change waiting ends a loop quietly", () =>
  withTeam(async (team, bob) => {
    const watching = team.tasks.watch();
    assert.deepEqual((await watching.next()).value, []);
    await bob.tasks.add("Draft");
    await sleep(200);
    await team.close();
    assert.deepEqual(await watching.next(), { value: undefined, done: true });
  }));

// The inbox gives each message as it arrives, so an empty inbox gives
// nothing at first. Every other block gives a value at once.
test("an empty inbox gives nothing at first", () =>
  withTeam(async (team) => {
    const stop = new AbortController();
    const got: unknown[] = [];
    const follow = (async () => {
      for await (const message of team.inbox.watch({ signal: stop.signal })) {
        got.push(message);
      }
    })();
    await sleep(300);
    stop.abort();
    await follow;
    assert.deepEqual(got, []);
  }));
