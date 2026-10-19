// Rules for what each agent may do, as Node agents meet them. The sidecar
// checks them, so these are the same rules Python agents are held to.

import assert from "node:assert/strict";
import { test } from "node:test";
import { NotAllowed, Team } from "../src/index.ts";
import { python, type Sidecar, withSidecar } from "./support.ts";

/** A team used by the agent `name`, with `rules` if any are given. */
async function agent(sidecar: Sidecar, name: string, ...rules: string[]): Promise<Team> {
  const key = sidecar.addAgent(name);
  if (rules.length > 0) {
    sidecar.allow(name, ...rules);
  }
  return Team.connect("default", { socket: sidecar.socket, key });
}

const refused = (pattern: RegExp) => (error: Error) => error instanceof NotAllowed && pattern.test(error.message);

// Setting a stuck firm task back with force writes its tickets, so it needs
// tasks:write like any other change to a task.
test("force needs tasks:write", () =>
  withSidecar(async (sidecar) => {
    const boss = await agent(sidecar, "boss");
    const task = await boss.tasks.add("Send the invoice", { firm: true });
    await boss.tasks.claim(task);
    const reader = await agent(sidecar, "reader", "tasks:read");
    await assert.rejects(reader.tasks.update(task, { state: "todo", force: true }), refused(/tasks:read/));
    const worker = await agent(sidecar, "worker", "tasks:write");
    assert.equal((await worker.tasks.update(task, { state: "todo", force: true })).ticket, 1);
    for (const team of [boss, reader, worker]) {
      await team.close();
    }
  }));

test("an agent reads what it may read and changes nothing else", () =>
  withSidecar(async (sidecar) => {
    const boss = await agent(sidecar, "boss");
    await boss.notes.put("deploy", "Friday");
    const reader = await agent(sidecar, "reader", "notes:read");
    assert.equal(await reader.notes.get("deploy"), "Friday");
    await assert.rejects(reader.notes.put("deploy", "Monday"), refused(/^reader may not change notes\. Its rules are: notes:read$/));
    await assert.rejects(reader.tasks.list(), refused(/may not read tasks/));
    await boss.close();
    await reader.close();
  }));

test("budgets are read, spent from and topped up", () =>
  withSidecar(async (sidecar) => {
    const boss = await agent(sidecar, "boss", "budgets:top-up");
    const spender = await agent(sidecar, "spender", "budgets:spend");
    await boss.budget("tokens").topUp(100);
    assert.equal(await spender.budget("tokens").spend(30), true);
    await assert.rejects(spender.budget("tokens").topUp(5), refused(/may not top up budgets/));
    assert.equal(await boss.budget("tokens").left(), 70);
    await boss.close();
    await spender.close();
  }));

// A Node agent sends to a Python agent, which reads it. Neither can read the
// other's inbox.
test("an inbox is read only by its owner, in Python too", () =>
  withSidecar(async (sidecar) => {
    const nora = await agent(sidecar, "nora", "inbox:write");
    const pia = sidecar.addAgent("pia");
    sidecar.allow("pia", "inbox:read");
    await nora.inbox.send("pia", "hello");
    await assert.rejects(nora.client.list("orvu/default/inbox/pia/0").toArray(), refused(/read another agent's inbox/));
    await assert.rejects(nora.client.cart("orvu/default/inbox-count/pia").items(), refused(/read another agent's inbox/));
    // A sender adds one to a page's count, and nothing else.
    await assert.rejects(
      nora.client.cart("orvu/default/inbox-count/pia").add("0", 2),
      refused(/add anything but one to the count of an inbox page/),
    );
    const seen = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], key=${JSON.stringify(pia)}) as team:
    print([message.body for message in team.inbox.messages()], [message.body for message in team.inbox.unread()])`,
    ).trim();
    assert.equal(seen, "['hello'] ['hello']");
    // A Node agent that may only read its inbox is given what it was sent.
    const quin = await agent(sidecar, "quin", "inbox:read");
    await nora.inbox.send("quin", "hi");
    assert.deepEqual((await quin.inbox.unread()).map((message) => message.body), ["hi"]);
    await assert.rejects(quin.client.map("orvu/default/inbox-done/pia/pia").entries(), refused(/read another agent's inbox/));
    await quin.close();
    await nora.close();
  }));

// The names a program writes for an agent with rules are its own: the sender
// of a message, and the agent that added a task. The blocks write them so.
test("a message and a task carry the agent's own name", () =>
  withSidecar(async (sidecar) => {
    const alice = await agent(sidecar, "alice", "inbox:write", "tasks:write");
    await alice.inbox.send("bob", "hello");
    const task = await alice.tasks.add("Draft");
    assert.equal(task.createdBy, "alice");
    const forged = JSON.stringify({ id: "m-1", from: "bob", to: "bob", body: "hi", sent: 1 });
    await assert.rejects(
      alice.client.list("orvu/default/inbox/bob/0").insert(0, forged),
      refused(/send a message under another name than its own/),
    );
    const board = alice.client.json("orvu/default/tasks");
    await assert.rejects(
      board.set(["t-1"], { title: "Draft", state: "todo", created: 1, created_by: "bob" }),
      refused(/add a task under another name than its own/),
    );
    await assert.rejects(board.set([task.id, "created_by"], "bob"), refused(/add a task under another name than its own/));
    assert.equal((await alice.tasks.get(task))?.createdBy, "alice");
    await alice.close();
  }));

// With notes:own and tasks:own an agent reads everything there, and changes
// only what is its own as it is now. It claims an open task, and leaves alone
// a task another agent holds.
test("an agent with own rules changes only its own", () =>
  withSidecar(async (sidecar) => {
    const boss = await agent(sidecar, "boss");
    const alice = await agent(sidecar, "alice", "notes:own", "tasks:own");
    await boss.notes.put("deploy", "Friday");
    const open = await boss.tasks.add("Open task");
    const held = await boss.tasks.add("Held task");
    assert.equal(await boss.tasks.claim(held), true);
    await alice.notes.put("mine", "Monday");
    await alice.notes.put("mine", "Tuesday");
    await assert.rejects(alice.notes.put("deploy", "Monday"), refused(/change another agent's note/));
    await assert.rejects(alice.notes.remove("deploy"), refused(/change another agent's note/));
    await alice.notes.remove("mine");
    await assert.rejects(alice.notes.structured("plan").set(["owner"], "alice"), refused(/structured notes, which keep no writer's name/));
    const task = await alice.tasks.add("My task");
    await alice.tasks.update(task, { title: "Still mine" });
    assert.equal(await alice.tasks.finish(task, "done"), true);
    assert.equal(await alice.tasks.claim(open), true);
    assert.equal(await alice.tasks.release(open), true);
    await assert.rejects(alice.tasks.update(open, { title: "Mine now" }), refused(/alice may not change a task it did not add/));
    await assert.rejects(alice.tasks.finish(held, "done"), refused(/change a task another agent holds/));
    const batch = alice.client.batch();
    batch.json("orvu/default/tasks").set([task.id, "notes"], "mine");
    batch.json("orvu/default/tasks").set([held.id, "notes"], "mine");
    await assert.rejects(batch.send(), refused(/nothing was applied/));
    assert.equal((await alice.tasks.get(task))?.notes, null);
    assert.equal((await boss.tasks.get(held))?.claimedBy, "boss");
    assert.equal((await boss.tasks.get(open))?.title, "Open task");
    await boss.close();
    await alice.close();
  }));

// An agent with tasks:own writes what a claim writes only once its claim has
// landed, and a claim on one team's board opens no task on another's.
test("own rules take a claim only as a claim", () =>
  withSidecar(async (sidecar) => {
    const boss = await agent(sidecar, "boss");
    const alice = await agent(sidecar, "alice", "tasks:own");
    const task = { title: "Open task", state: "todo", created: 1, created_by: "boss" };
    for (const team of ["default", "other"]) {
      await boss.client.json(`orvu/${team}/tasks`).set(["t-1"], task);
    }
    for (const [field, value] of [["state", "done"], ["lease", 1]] as const) {
      const batch = alice.client.batch();
      batch.json("orvu/default/tasks").set(["t-1", field], value);
      batch.json("orvu/default/tasks").set(["t-1", "claimed_by"], "alice");
      await batch.send().catch(() => undefined);
      assert.deepEqual(await boss.client.json("orvu/default/tasks").get(["t-1"]), task);
    }
    const crossed = alice.client.batch();
    crossed.json("orvu/default/tasks").set(["t-1", "claimed_by"], "alice");
    crossed.json("orvu/other/tasks").set(["t-1", "state"], "done");
    await crossed.send().catch(() => undefined);
    assert.deepEqual(await boss.client.json("orvu/other/tasks").get(["t-1"]), task);
    const mine = await boss.tasks.add("Claimed and given back");
    assert.equal(await alice.tasks.claim(mine), true);
    assert.equal((await alice.tasks.update(mine, { state: "todo" })).claimedBy, null);
    await boss.close();
    await alice.close();
  }));

// A holder with tasks:own names only itself on the task, or nobody.
test("own rules forge no names on a task", () =>
  withSidecar(async (sidecar) => {
    const boss = await agent(sidecar, "boss");
    const alice = await agent(sidecar, "alice", "tasks:own");
    const task = await boss.tasks.add("Open task");
    assert.equal(await alice.tasks.claim(task), true);
    for (const name of ["ghost", "boss"]) {
      await assert.rejects(alice.client.json("orvu/default/tasks").set([task.id, "claimed_by"], name), refused(/name another agent on a task/));
    }
    assert.equal(await alice.tasks.release(task), true);
    assert.equal((await boss.tasks.get(task))?.claimedBy, null);
    await boss.close();
    await alice.close();
  }));
