// The task board.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest, OrvuError, TypeMismatch, UnknownTask, type Task } from "../src/index.ts";
import type { Message } from "../src/wire.ts";
import * as names from "../src/names.ts";
import { python, sleep, teamOn, waitUntil, withTeam } from "./support.ts";

test("a task is added and read back", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Summarise the Q3 report", { notes: "Keep it to one page" });
    assert.ok(task.id.startsWith("t-"));
    assert.equal(task.id.length, 2 + 16);
    assert.deepEqual([task.title, task.notes, task.state, task.createdBy], ["Summarise the Q3 report", "Keep it to one page", "todo", "alice"]);
    assert.deepEqual([task.claimedBy, task.result, task.parent, task.finishedBy, task.updatedBy], [null, null, null, null, null]);
    assert.deepEqual(await team.tasks.get(task), task);
    assert.deepEqual(await team.tasks.get(task.id), task);
  }));

test("tasks are listed oldest first", () =>
  withTeam(async (team) => {
    const titles = Array.from({ length: 12 }, (_, number) => `task ${number}`);
    for (const title of titles) {
      await team.tasks.add(title);
    }
    assert.deepEqual((await team.tasks.list()).map((task) => task.title), titles);
  }));

test("the list can be narrowed to one state", () =>
  withTeam(async (team) => {
    const first = await team.tasks.add("first");
    const second = await team.tasks.add("second");
    await team.tasks.add("third");
    await team.tasks.claim(first);
    await team.tasks.finish(second);
    assert.deepEqual((await team.tasks.list("todo")).map((task) => task.title), ["third"]);
    assert.deepEqual((await team.tasks.list("doing")).map((task) => task.title), ["first"]);
    assert.deepEqual((await team.tasks.list("done")).map((task) => task.title), ["second"]);
    await assert.rejects(team.tasks.list("finished" as "done"), (error: Error) => error instanceof BadRequest && /state/.test(error.message));
  }));

test("a task is updated field by field", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft", { notes: "first try" });
    let changed = await team.tasks.update(task, { title: "Final draft" });
    assert.deepEqual([changed.title, changed.notes, changed.state], ["Final draft", "first try", "todo"]);
    changed = await team.tasks.update(task, { notes: "second try", state: "doing" });
    assert.deepEqual([changed.title, changed.notes, changed.state], ["Final draft", "second try", "doing"]);
    assert.deepEqual(await team.tasks.get(task), changed);
  }));

// Two agents change different fields of one task. Both changes are kept.
test("two agents updating different fields both keep their change", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.update(task, { title: "Final draft" });
    await bob.tasks.update(task, { notes: "checked by bob" });
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.title, found.notes], ["Final draft", "checked by bob"]);
  }));

test("an update that makes no sense changes nothing", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    for (const change of [{ title: "" }, { title: "  " }, { title: 7 }, { notes: 7 }, { state: "finished" }]) {
      await assert.rejects(team.tasks.update(task, change as object), BadRequest, JSON.stringify(change));
    }
    assert.deepEqual(await team.tasks.get(task), task);
  }));

test("a task needs a title", () =>
  withTeam(async (team) => {
    for (const title of ["", "   ", null, 7]) {
      await assert.rejects(team.tasks.add(title as string), (error: Error) => error instanceof BadRequest && /title/.test(error.message));
    }
    assert.deepEqual(await team.tasks.list(), []);
  }));

test("a task that is not on the board is reported", () =>
  withTeam(async (team) => {
    await team.tasks.add("Draft");
    assert.equal(await team.tasks.get("t-00000000"), null);
    for (const call of [
      () => team.tasks.update("t-00000000", { title: "x" }),
      () => team.tasks.claim("t-00000000"),
      () => team.tasks.release("t-00000000"),
      () => team.tasks.finish("t-00000000"),
      () => team.tasks.move("t-00000000"),
      () => team.tasks.children("t-00000000"),
      () => team.tasks.add("Sub-task", { parent: "t-00000000" }),
    ]) {
      await assert.rejects(call(), (error: Error) => error instanceof UnknownTask && error.message.includes("t-00000000"));
    }
    assert.equal((await team.tasks.list()).length, 1);
  }));

// An id that is not text cannot be on the board.
test("an id that is not text is no task", () =>
  withTeam(async (team) => {
    for (const ref of [123, null, ["t-1"]]) {
      assert.equal(await team.tasks.get(ref as unknown as string), null);
      await assert.rejects(team.tasks.claim(ref as unknown as string), UnknownTask);
    }
  }));

test("a claimed task is this agent's and nobody else's", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    assert.equal(await team.tasks.claim(task), true);
    assert.equal(await bob.tasks.claim(task), false);
    const found = (await bob.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy], ["doing", "alice"]);
    // Claiming what is already yours is fine.
    assert.equal(await team.tasks.claim(task), true);
  }));

test("a released task can be claimed by someone else", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task);
    assert.equal(await bob.tasks.release(task), false, "bob gave back a task that was not his");
    assert.equal((await team.tasks.get(task))!.claimedBy, "alice");
    assert.equal(await team.tasks.release(task), true);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy], ["todo", null]);
    assert.equal(await bob.tasks.claim(task), true);
    assert.equal((await team.tasks.get(task))!.claimedBy, "bob");
  }));

test("a finished task cannot be claimed", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.finish(task, "done by hand");
    assert.equal(await bob.tasks.claim(task), false);
    assert.equal((await team.tasks.get(task))!.state, "done");
  }));

test("the agent can be named per call", () =>
  withTeam(async (_team, _bob, sidecar) => {
    const nameless = await teamOn(sidecar);
    const task = await nameless.tasks.add("Draft");
    assert.equal(task.createdBy, null);
    await assert.rejects(nameless.tasks.claim(task), (error: Error) => error instanceof BadRequest && /who is asking/.test(error.message));
    assert.equal((await nameless.tasks.get(task))!.state, "todo");
    assert.equal(await nameless.tasks.claim(task, "carol"), true);
    assert.equal(await nameless.tasks.claim(task, "dave"), false);
    assert.equal(await nameless.tasks.finish(task, "ok", "carol"), true);
    const finished = (await nameless.tasks.get(task))!;
    assert.deepEqual([finished.claimedBy, finished.finishedBy], ["carol", "carol"]);
    await nameless.close();
  }));

test("a finished task keeps its result", () =>
  withTeam(async (team) => {
    for (const result of ["It went up.", 42, null, ["a", "b"], { score: 0.5, ok: true }]) {
      const task = await team.tasks.add("Draft");
      await team.tasks.claim(task);
      assert.equal(await team.tasks.finish(task, result), true);
      const finished = (await team.tasks.get(task))!;
      assert.deepEqual([finished.state, finished.result, finished.finishedBy], ["done", result, "alice"]);
    }
  }));

test("a result that cannot be kept is refused and the task is not done", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task);
    for (const result of [new Date(0), { score: NaN }, () => 1]) {
      await assert.rejects(team.tasks.finish(task, result), BadRequest);
    }
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.finishedBy], ["doing", null, null]);
  }));

// Finishing a task writes several fields, and other agents read while that
// is going on. A task they see as "done" must already say who did it and
// what came out.
test("a task seen as done is whole", () =>
  withTeam(async (team, _bob, sidecar) => {
    const tasks: Task[] = [];
    for (let number = 0; number < 120; number++) {
      tasks.push(await team.tasks.add(`task ${number}`));
    }
    const reader = await teamOn(sidecar, "reader");
    const halfDone: Task[] = [];
    let finishing = true;
    const reading = (async () => {
      while (finishing) {
        for (const task of await reader.tasks.list("done")) {
          if (task.finishedBy !== "alice" || JSON.stringify(task.result) !== '{"ok":true}') {
            halfDone.push(task);
          }
        }
      }
    })();
    for (const task of tasks) {
      await team.tasks.finish(task, { ok: true });
    }
    finishing = false;
    await reading;
    await reader.close();
    assert.deepEqual(halfDone, []);
  }));

test("a task can have sub-tasks", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Write the report");
    const research = await team.tasks.add("Research", { parent: report });
    const draft = await team.tasks.add("Draft", { parent: report });
    const sources = await team.tasks.add("Find sources", { parent: research });
    assert.deepEqual([research.parent, draft.parent, sources.parent], [report.id, report.id, research.id]);
    assert.equal((await team.tasks.get(report))!.parent, null);
    assert.deepEqual((await team.tasks.children(report)).map((task) => task.title), ["Research", "Draft"]);
    assert.deepEqual((await team.tasks.children(research)).map((task) => task.title), ["Find sources"]);
    assert.deepEqual(await team.tasks.children(sources), []);
    assert.equal((await team.tasks.list()).length, 4);
  }));

test("a task can be moved under another and back to the top", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Write the report");
    const draft = await team.tasks.add("Draft");
    assert.equal(await team.tasks.move(draft, report), true);
    assert.equal((await team.tasks.get(draft))!.parent, report.id);
    assert.deepEqual((await team.tasks.children(report)).map((task) => task.id), [draft.id]);
    assert.equal(await team.tasks.move(draft), true);
    assert.equal((await team.tasks.get(draft))!.parent, null);
    assert.deepEqual(await team.tasks.children(report), []);
  }));

// A task under its own sub-task would be a loop with no way in from the top.
test("a task cannot be moved under itself or its own sub-task", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Write the report");
    const research = await team.tasks.add("Research", { parent: report });
    const sources = await team.tasks.add("Find sources", { parent: research });
    assert.equal(await team.tasks.move(report, sources), false);
    assert.equal(await team.tasks.move(report, report), false);
    assert.equal((await team.tasks.get(report))!.parent, null);
    assert.equal((await team.tasks.get(research))!.parent, report.id);
    assert.equal((await team.tasks.get(sources))!.parent, research.id);
  }));

// Something else already uses the board's name for another kind of object,
// so no task can be written. The caller has to be told that.
test("a write that fails is reported as what it is", () =>
  withTeam(async (team) => {
    await team.client.counter("orvu/default/tasks").increment(1);
    await assert.rejects(team.tasks.add("Draft"), TypeMismatch);
  }));

test("finishing a task twice keeps the first result", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    assert.equal(await team.tasks.finish(task, "first"), true);
    assert.equal(await bob.tasks.finish(task, "second"), false);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.finishedBy], ["done", "first", "alice"]);
  }));

test("a task set back to todo can be claimed by anyone", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task);
    const changed = await team.tasks.update(task, { state: "todo" });
    assert.deepEqual([changed.state, changed.claimedBy], ["todo", null]);
    assert.equal(await bob.tasks.claim(task), true);
  }));

test("a finished task can be opened again", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task);
    await team.tasks.finish(task, "too early");
    const changed = await team.tasks.update(task, { state: "todo" });
    assert.deepEqual([changed.state, changed.claimedBy, changed.result, changed.finishedBy], ["todo", null, null, null]);
    assert.equal(await team.tasks.claim(task), true);
    assert.equal(await team.tasks.finish(task, "now it is right"), true);
    assert.equal((await team.tasks.get(task))!.result, "now it is right");
  }));

test("a task can be set to done and to doing by hand", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    assert.equal((await team.tasks.update(task, { state: "done" })).state, "done");
    assert.equal(await team.tasks.finish(task, "late"), false);
    assert.equal((await team.tasks.update(task, { state: "doing" })).state, "doing");
    assert.equal(await team.tasks.finish(task, "now"), true);
  }));

test("a task says who changed it last by hand", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Summarise the Q3 report");
    assert.equal((await bob.tasks.update(task, { notes: "Use the new numbers" })).updatedBy, "bob");
    assert.equal((await team.tasks.update(task, { title: "Summarise Q3" })).updatedBy, "alice");
    // Nothing was asked for, so nothing is changed.
    assert.equal((await bob.tasks.update(task, {})).updatedBy, "alice");
    await bob.tasks.claim(task);
    await bob.tasks.finish(task, "It went up.");
    const done = (await team.tasks.get(task))!;
    assert.deepEqual([done.claimedBy, done.finishedBy, done.updatedBy], ["bob", "bob", "alice"]);
    const nameless = await teamOn(sidecar);
    assert.equal((await nameless.tasks.update(task, { notes: "second" })).updatedBy, null);
    await nameless.close();
  }));

/** Records the operations `team` sends, by name. */
function recording(client: { call: (method: string, build: (session: string) => Message) => Promise<Message> }): string[] {
  const sent: string[] = [];
  const call = client.call.bind(client);
  client.call = (method, build) => {
    const request = build("");
    for (const one of method === "Batch" ? request.requests : [request]) {
      sent.push(...Object.keys(one).filter((key) => key !== "session"));
    }
    return call(method, build);
  };
  return sent;
}

// Work on one task reads that task, and not the whole board.
test("work on one task does not read the whole board", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Report");
    const draft = await team.tasks.add("Draft", { parent: report });
    const sent = recording(team.client);
    assert.equal((await team.tasks.get(draft))!.parent, report.id);
    assert.equal((await team.tasks.add("Sources", { parent: report })).parent, report.id);
    assert.equal((await team.tasks.update(draft, { notes: "Use the new numbers" })).notes, "Use the new numbers");
    assert.equal(await team.tasks.claim(draft), true);
    assert.equal(await team.tasks.release(draft), true);
    assert.equal(await team.tasks.finish(draft, "It went up."), true);
    assert.equal(await team.tasks.move(draft), true);
    assert.deepEqual((await team.tasks.children(report)).map((task) => task.title), ["Sources"]);
    assert.ok(sent.length > 0, "nothing was recorded");
    assert.ok(!sent.includes("json_to_value") && !sent.includes("tree_to_nodes"), sent.join());
  }));

// Another machine's task can arrive in pieces. One that has no title yet is
// not shown, and a parent that is not whole yet is not named.
test("a task that has only partly arrived is shown once it is whole", () =>
  withTeam(async (team) => {
    const board = team.client.json("orvu/default/tasks");
    const tree = team.client.tree("orvu/default/tasks-tree");
    await board.set(["t-0000000000000001"], { state: "todo" });
    await tree.moveNode("t-0000000000000001", "root");
    assert.equal(await team.tasks.get("t-0000000000000001"), null);
    assert.deepEqual(await team.tasks.list(), []);
    await board.set(["t-0000000000000002"], { title: "Draft", state: "todo", created: 1 });
    await tree.moveNode("t-0000000000000003", "root");
    await tree.moveNode("t-0000000000000002", "t-0000000000000003");
    assert.equal((await team.tasks.get("t-0000000000000002"))!.parent, null);
    assert.equal((await team.tasks.list())[0].parent, null);
    await board.set(["t-0000000000000003"], { state: "todo" });
    assert.equal((await team.tasks.get("t-0000000000000002"))!.parent, null);
    assert.equal((await team.tasks.list())[0].parent, null);
    await board.set(["t-0000000000000003"], { title: "Report", state: "todo", created: 0 });
    assert.equal((await team.tasks.get("t-0000000000000002"))!.parent, "t-0000000000000003");
    assert.equal((await team.tasks.list())[1].parent, "t-0000000000000003");
    assert.deepEqual((await team.tasks.children("t-0000000000000003")).map((task) => task.id), ["t-0000000000000002"]);
  }));

// One message holds at most 1,024 operations, and a batch's replies at most
// 4 MiB, so many sub-tasks and large ones are read in parts.
test("very many and very large sub-tasks are all read", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Report");
    const titles = Array.from({ length: 600 }, (_, number) => `part ${number}`);
    for (const title of titles) {
      await team.tasks.add(title, { parent: report });
    }
    assert.deepEqual((await team.tasks.children(report)).map((task) => task.title), titles);

    const big = await team.tasks.add("Big");
    await team.tasks.update(big, { notes: "n".repeat(3_000_000) });
    assert.equal(await team.tasks.finish(big, "r".repeat(2_000_000)), true);
    const found = (await team.tasks.get(big))!;
    assert.deepEqual([found.notes!.length, (found.result as string).length, found.state], [3_000_000, 2_000_000, "done"]);
  }));

// The board of a team can be larger than one batch reply.
test("a board larger than one batch can hold is listed", () =>
  withTeam(async (team) => {
    for (let number = 0; number < 450; number++) {
      await team.tasks.add(`task ${number}`, { notes: "n".repeat(10_000) });
    }
    assert.equal((await team.tasks.list()).length, 450);
    for await (const tasks of team.tasks.watch()) {
      assert.equal(tasks.length, 450);
      break;
    }
  }));

test("a task board is watched as tasks are added, claimed and finished", () =>
  withTeam(async (team, bob) => {
    const seen: Task[][] = [];
    const follow = (async () => {
      for await (const tasks of bob.tasks.watch()) {
        seen.push(tasks);
        if (tasks.length > 0 && tasks.every((task) => task.state === "done")) {
          return;
        }
      }
    })();
    assert.ok(await waitUntil(() => seen.length > 0));
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task);
    await team.tasks.finish(task, "ok");
    await follow;
    assert.deepEqual(seen[0], []);
    assert.deepEqual(seen.at(-1), [await team.tasks.get(task)]);
  }));

test("an error from the sidecar is an OrvuError", () =>
  withTeam(async (team) => {
    await team.client.list("orvu/default/tasks-tree").insert(0, "x");
    await assert.rejects(team.tasks.add("Draft"), OrvuError);
  }));

// A task's own fields say null for "none". Given back where "none" is
// meant, null means the same as leaving it out.
test("null means none, where a task is added, moved or changed", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Write the report");
    const draft = await team.tasks.add("Draft", { parent: report });
    const top = await team.tasks.add("Top", { parent: report.parent, notes: report.notes ?? undefined });
    assert.equal(top.parent, null);
    const again = await team.tasks.add("Again", { parent: null, notes: null });
    assert.deepEqual([again.parent, again.notes], [null, null]);
    assert.equal(await team.tasks.move(draft, report.parent), true);
    assert.equal((await team.tasks.get(draft))!.parent, null);
    const changed = await team.tasks.update(draft, { title: "Draft 2", notes: draft.notes, state: null });
    assert.deepEqual([changed.title, changed.notes, changed.state], ["Draft 2", null, "todo"]);
  }));

type Calls = { call: (method: string, build: (session: string) => Message, waitMs?: number) => Promise<Message> };

/** Calls `around` for each message the client sends, with whether it writes
 * to the board and a function that sends it. */
function hold(client: Calls, around: (writes: boolean, send: () => Promise<Message>) => Promise<Message>): void {
  const call = client.call.bind(client);
  client.call = (method, build, waitMs) => {
    const request = build("");
    const keys = (method === "Batch" ? request.requests : [request]).flatMap((one: Message) => Object.keys(one));
    return around(keys.includes("json_set"), () => call(method, build, waitMs));
  };
}

/** A promise, and the function that keeps it. */
function signal(): [Promise<void>, () => void] {
  let keep!: () => void;
  const promise = new Promise<void>((resolve) => (keep = resolve));
  return [promise, keep];
}

// Two agents finish the same task at the same moment: both looked, saw it not
// done, and wrote their result before either read it back. Only one is told
// yes, and the result kept is that one's, beside its name.
test("of two finishes at once only one is told yes", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    const [bobLooked, bobHasLooked] = signal();
    const [aliceWrote, aliceHasWritten] = signal();
    const [bobFinished, bobHasFinished] = signal();
    let bobCalls = 0;
    let aliceWrites = 0;
    // Bob looks first, writes just after alice, and is done before alice
    // reads back.
    hold(bob.client, async (writes, send) => {
      if (bobCalls++ === 0) {
        const reply = await send();
        bobHasLooked();
        return reply;
      }
      if (writes) {
        await aliceWrote;
      }
      return send();
    });
    hold(team.client, async (writes, send) => {
      if (writes && aliceWrites++ === 0) {
        await bobLooked;
        const reply = await send();
        aliceHasWritten();
        return reply;
      }
      if (aliceWrites > 0) {
        await bobFinished;
      }
      return send();
    });
    const bobTold = bob.tasks.finish(task, "by bob").finally(bobHasFinished);
    const told = { alice: await team.tasks.finish(task, "by alice"), bob: await bobTold };
    assert.deepEqual([told.alice, told.bob].sort(), [false, true], JSON.stringify(told));
    const winner = told.alice ? "alice" : "bob";
    const found = (await bob.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.finishedBy], ["done", `by ${winner}`, winner]);
  }));

// Tasks finished before the result and the finisher were kept together are
// still read as they were.
test("a task finished the old way is still read", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    const board = team.client.json("orvu/default/tasks");
    await board.set([task.id, "result"], { score: 1 });
    await board.set([task.id, "finished_by"], "carol");
    await board.set([task.id, "finished"], true);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.finishedBy], ["done", { score: 1 }, "carol"]);
    assert.equal(await team.tasks.finish(task, "again"), false);
    const opened = await team.tasks.update(task, { state: "todo" });
    assert.deepEqual([opened.state, opened.result, opened.finishedBy], ["todo", null, null]);
  }));

// A claim that lands just after the task was finished is told no, and must
// not leave its name, or its time, on the finished task.
test("a claim that lands after the task was finished leaves no name", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    let finished = false;
    hold(bob.client, async (writes, send) => {
      if (writes && !finished) {
        finished = true;
        assert.equal(await team.tasks.finish(task, "by alice"), true);
      }
      return send();
    });
    assert.equal(await bob.tasks.claim(task), false);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.claimedBy, found.seen, found.lease], ["done", "by alice", null, null, null]);
  }));

// A task is added with two writes: its place in the tree, then the task.
// Another machine may hear of them in either order. One that is on the board
// and not yet in the tree is not shown for a while, rather than shown at the
// top for an instant and then under its parent.
test("a task not yet in the tree is not listed", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Report");
    await team.client.json("orvu/default/tasks").set(["t-0000000000000009"], { title: "Draft", state: "todo", created: Date.now() });
    assert.deepEqual((await team.tasks.list()).map((task) => task.title), ["Report"]);
    for await (const tasks of team.tasks.watch()) {
      assert.deepEqual(tasks.map((task) => task.title), ["Report"]);
      break;
    }
    await team.client.tree("orvu/default/tasks-tree").moveNode("t-0000000000000009", report.id);
    assert.deepEqual(
      (await team.tasks.list()).map((task) => [task.title, task.parent]),
      [["Report", null], ["Draft", report.id]],
    );
  }));

// The tree is read before the board. A task added here is put in the tree
// first, so it is found in the tree by the time it is on the board.
test("the list reads the tree before the board", () =>
  withTeam(async (team) => {
    await team.tasks.add("Report");
    const sent = recording(team.client);
    await team.tasks.list();
    assert.ok(sent.indexOf("tree_to_nodes") < sent.indexOf("json_to_value"), sent.join());
  }));

// Reads that a batch did not run because its replies had passed 4 MiB are
// sent again together, in as few messages as they fit in, and not one by
// one.
test("reads past the batch limit are sent again together", () =>
  withTeam(async (team) => {
    const report = await team.tasks.add("Report");
    for (let number = 0; number < 900; number++) {
      await team.tasks.add(`part ${number}`, { parent: report, notes: "n".repeat(10_000) });
    }
    let sent = 0;
    hold(team.client, (_writes, send) => {
      sent++;
      return send();
    });
    assert.equal((await team.tasks.children(report)).length, 900);
    // The ids under it, then 900 tasks of about 9 MiB: two messages for the
    // first 500, and two or three for the rest.
    assert.ok(sent <= 6, String(sent));
  }));

// Two programs run under one name. The second looks at a task while it is
// free and its claim is slow to land. The first then claims the task too, and
// alice finishes it. When the second program reads back, the claim on the task
// is the first program's, and is left as it is.
test("undoing a late claim leaves another program's claim", () =>
  withTeam(async (team, first, sidecar) => {
    const task = await team.tasks.add("Draft");
    const second = await teamOn(sidecar, "bob");
    let claimed: Task | null = null;
    hold(second.client, async (writes, send) => {
      const reply = await send();
      if (writes && claimed === null) {
        await sleep(10);
        assert.equal(await first.tasks.claim(task, undefined, { lease: 600 }), true);
        claimed = (await first.tasks.get(task))!;
        assert.equal(await team.tasks.finish(task, "by alice"), true);
      }
      return reply;
    });
    assert.equal(await second.tasks.claim(task), false);
    await second.close();
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.finishedBy], ["done", "by alice", "alice"]);
    assert.deepEqual([found.claimedBy, found.seen, found.lease], ["bob", claimed!.seen, 600]);
  }));

// A task is written to the tree first and to the board after. If a program
// stops between the two, or the tree is slow to arrive from another machine,
// the task is not shown for a short while, in case its place in the tree is
// on its way. After that it is shown at the top.
test("a task with no place in the tree is shown at the top after a while", () =>
  withTeam(async (team) => {
    const board = team.client.json("orvu/default/tasks");
    await board.set(["t-0000000000000001"], { title: "Lost", state: "todo", created: Date.now() - 60_000 });
    await board.set(["t-0000000000000002"], { title: "On its way", state: "todo", created: Date.now() });
    assert.deepEqual((await team.tasks.list()).map((task) => [task.title, task.parent]), [["Lost", null]]);
    assert.equal((await team.tasks.get("t-0000000000000001"))!.title, "Lost");
  }));

test("a watch shows a task with no place in the tree once the wait is over", () =>
  withTeam(async (team) => {
    team.tasks.grace = 1;
    await team.client.json("orvu/default/tasks").set(["t-0000000000000002"], { title: "On its way", state: "todo", created: Date.now() });
    const seen: string[][] = [];
    for await (const tasks of team.tasks.watch()) {
      seen.push(tasks.map((task) => task.title));
      if (tasks.length > 0) {
        break;
      }
    }
    assert.deepEqual(seen, [[], ["On its way"]]);
  }));

// Adding writes the task's place in the tree before the task itself. A task
// whose write to the board fails leaves only a place in the tree, which shows
// nothing.
test("adding puts the task in the tree first", () =>
  withTeam(async (team) => {
    await team.tasks.list();
    hold(team.client as unknown as Calls, async (writes, send) => {
      if (writes) {
        throw new Error("the board could not be written");
      }
      return send();
    });
    await assert.rejects(team.tasks.add("Draft"), /the board could not be written/);
    const nodes = await team.client.tree("orvu/default/tasks-tree").toNodes();
    assert.equal(nodes.filter((node) => node.parent === "root").length, 1);
  }));

// The same, but the first program finishes the task itself. The task rightly
// names the agent, and keeps its name.
test("a late claim leaves the name of an agent that finished the task", () =>
  withTeam(async (team, first, sidecar) => {
    const task = await team.tasks.add("Draft");
    const second = await teamOn(sidecar, "bob");
    let finished = false;
    hold(second.client, async (writes, send) => {
      const reply = await send();
      if (writes && !finished) {
        finished = true;
        assert.equal(await first.tasks.finish(task, "by bob"), true);
      }
      return reply;
    });
    assert.equal(await second.tasks.claim(task), false);
    await second.close();
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.finishedBy, found.claimedBy], ["done", "by bob", "bob", "bob"]);
  }));

// ---- what anyone may have written -------------------------------------------
//
// The board, the notes and the inbox hold whatever any program wrote, and a
// program with the right rules can write anything into them. Reading never
// fails because of it: a field that is not what it should be reads as a safe
// value instead, the same in both libraries.

const ODD_VALUES: unknown[] = [
  { toString: 1 },
  { valueOf: 1 },
  JSON.parse('{"toString": "x", "valueOf": "y", "__proto__": {"a": 1}}'),
  [1, "a"],
  [],
  7,
  2.5,
  -1e300,
  null,
  true,
  false,
  "",
  "doing",
];
const TASK_FIELDS = [
  "title", "state", "claimed_by", "notes", "created", "created_by", "finished_by", "updated_by", "firm",
  "ticket", "tickets", "seen", "lease", "taken_over", "finished", "result",
];

test("odd values written anywhere never stop reading", () =>
  withTeam(async (team, bob, sidecar) => {
    const board = team.client.json(names.tasks(team.name));
    const plain = await team.tasks.add("Check the figures");
    const odd = await team.tasks.add("Send the invoice", { firm: true });
    const ids = [plain.id, odd.id].sort();
    for (const value of ODD_VALUES) {
      for (const field of TASK_FIELDS) {
        await board.set([odd.id, field], value);
      }
      const tasks = await team.tasks.list();
      assert.deepEqual(tasks.map((found) => found.id).sort(), ids);
      const found = (await team.tasks.get(odd))!;
      assert.equal(typeof found.title, "string");
      assert.ok(["todo", "doing", "done"].includes(found.state));
      for (const text of [found.claimedBy, found.notes, found.createdBy, found.finishedBy, found.updatedBy]) {
        assert.ok(text === null || typeof text === "string");
      }
      assert.ok(Number.isFinite(found.created) && Number.isInteger(found.ticket));
      const watching = team.tasks.watch();
      assert.deepEqual(((await watching.next()).value as Task[]).map((task) => task.id).sort(), ids);
      await watching.return(undefined);
      // Python reads the same task the same way.
      const seen = JSON.parse(
        python(
          sidecar.socket,
          `import sys, json, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    t = team.tasks.get(${JSON.stringify(odd.id)})
    print(json.dumps([t.title, t.state, t.claimed_by, t.notes, t.created, t.created_by, t.finished_by, t.updated_by, t.firm, t.ticket]))`,
        ),
      );
      assert.deepEqual(seen, [
        found.title, found.state, found.claimedBy, found.notes, found.created, found.createdBy, found.finishedBy,
        found.updatedBy, found.firm, found.ticket,
      ], JSON.stringify(value));
    }
    // A note, and messages, that are not what this package writes.
    const notes = team.client.mvMap(names.notes(team.name));
    await team.notes.put("plain", 1);
    for (const kept of ["\x1enot json", "\x1e" + "1".repeat(5000), "\x1e" + "[".repeat(100_000), "bob\x1f\x1e{", "\x1f"]) {
      await notes.put("odd", kept);
      await team.notes.get("odd");
      assert.equal((await team.notes.all()).plain, 1);
      await team.notes.author("odd");
    }
    const page = team.client.list(names.inboxPage(team.name, names.EVERYONE, 0));
    for (const kept of [
      '{"id":"m-1","sent":1,"body":"hi","from":{"toString":1},"to":[1]}',
      '{"id":5,"sent":1,"body":1}',
      '{"id":"m-2","sent":2.5,"body":1}',
      '{"id":"m-3","sent":' + "1".repeat(5000) + ',"body":1}',
      '{"id":"m-4","sent":1e400,"body":1}',
      "[1,2]",
      "null",
      "\x1enot json",
      "[".repeat(100_000),
    ]) {
      await page.insert(0, kept);
    }
    await bob.inbox.sendAll("hello");
    for (const reader of [team, bob]) {
      const messages = await reader.inbox.messages();
      assert.deepEqual(messages.filter((message) => message.id !== "m-1").map((message) => message.body), ["hello"]);
      const oddOne = messages.find((message) => message.id === "m-1")!;
      assert.deepEqual([oddOne.sender, oddOne.to], [null, null]);
      await reader.inbox.unread();
    }
  }));

// A time a task was added at is any finite number, and both libraries put
// tasks in the same order by it.
test("tasks are put in the same order by any finite time", () =>
  withTeam(async (team, _bob, sidecar) => {
    const board = team.client.json(names.tasks(team.name));
    const order: string[] = [];
    for (const [title, created] of [["c", 3], ["a", 2.5], ["b", 2], ["z", "soon"]] as const) {
      const added = await team.tasks.add(title);
      await board.set([added.id, "created"], created);
    }
    order.push(...(await team.tasks.list()).map((found) => found.title));
    assert.deepEqual(order, ["z", "b", "a", "c"]);
    const seen = python(
      sidecar.socket,
      `import sys, json, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    print(json.dumps([t.title for t in team.tasks.list()]))`,
    );
    assert.deepEqual(JSON.parse(seen), order);
  }));

// The whole board may have been written over by a program without rules.
// What is not an object holds no tasks.
test("a board that is not an object holds no tasks", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Check the figures");
    const board = team.client.json(names.tasks(team.name));
    for (const value of [[1, 2], [], "board", 7, true, null]) {
      await board.set([], value);
      assert.deepEqual(await team.tasks.list(), []);
      assert.equal(await team.tasks.get(task), null);
      const watching = team.tasks.watch();
      assert.deepEqual((await watching.next()).value, []);
      await watching.return(undefined);
    }
  }));
