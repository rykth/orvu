// A Python agent and a Node agent on one team share everything, each reading
// what the other wrote, as if both were in the same language.

import assert from "node:assert/strict";
import { test } from "node:test";
import { python, teamOn, withSidecar } from "./support.ts";

/** Runs `body` as a Python agent called `me` on the team, and gives what it
 * printed, read as JSON. */
function asPython(socket: string, me: string | null, body: string): unknown {
  const lines = body.trim().split("\n").map((line) => "    " + line).join("\n");
  const program = `import json, sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me=${me === null ? "None" : JSON.stringify(me)}) as team:
${lines}`;
  const printed = python(socket, program).trim();
  return printed === "" ? null : JSON.parse(printed.split("\n").at(-1)!);
}

test("notes, and who wrote them", () =>
  withSidecar(async (sidecar) => {
    asPython(sidecar.socket, "pia", `
team.notes.put("text", "Friday")
team.notes.put("number", 7)
team.notes.put("list", ["a", 1, None, {"b": True}])
team.notes.put("marked", "\\x1e7")
team.notes.structured("plan").set(["owner"], "pia")`);
    const node = await teamOn(sidecar, "nora");
    assert.deepEqual(await node.notes.all(), { text: "Friday", number: 7, list: ["a", 1, null, { b: true }], marked: "\x1e7" });
    assert.equal(await node.notes.author("number"), "pia");
    assert.deepEqual(await node.notes.structured("plan").toValue(), { owner: "pia" });
    await node.notes.put("from-node", { n: 2.5, ok: false });
    await node.notes.put("odd", "\x1eodd");
    await node.notes.structured("plan").set(["checker"], "nora");
    await node.close();
    const seen = asPython(sidecar.socket, null, `
print(json.dumps([team.notes.get("from-node"), team.notes.get("odd"), team.notes.author("odd"), team.notes.structured("plan").to_value(), sorted(team.notes.structured_names())]))`);
    assert.deepEqual(seen, [{ n: 2.5, ok: false }, "\x1eodd", "nora", { owner: "pia", checker: "nora" }, ["plan"]]);
  }));

test("the task board", () =>
  withSidecar(async (sidecar) => {
    const ids = asPython(sidecar.socket, "pia", `
report = team.tasks.add("Write the report", notes="by Friday")
draft = team.tasks.add("Draft", parent=report)
team.tasks.claim(draft)
print(json.dumps([report.id, draft.id]))`) as string[];
    const node = await teamOn(sidecar, "nora");
    const [report, draft] = await node.tasks.list();
    assert.deepEqual([report.id, report.title, report.notes, report.createdBy], [ids[0], "Write the report", "by Friday", "pia"]);
    assert.deepEqual([draft.id, draft.parent, draft.state, draft.claimedBy], [ids[1], ids[0], "doing", "pia"]);
    assert.equal(await node.tasks.claim(draft), false);
    const extra = await node.tasks.add("Check", { parent: report });
    assert.equal(await node.tasks.claim(extra), true);
    assert.equal(await node.tasks.finish(extra, { ok: true }), true);
    await node.close();
    const seen = asPython(sidecar.socket, "pia", `
check = [t for t in team.tasks.children(${JSON.stringify(ids[0])}) if t.title == "Check"][0]
print(json.dumps([check.state, check.claimed_by, check.finished_by, check.result, check.created_by, check.parent]))`);
    assert.deepEqual(seen, ["done", "nora", "nora", { ok: true }, "nora", ids[0]]);
  }));

test("a budget and a tally", () =>
  withSidecar(async (sidecar) => {
    asPython(sidecar.socket, "pia", `
team.budget("tokens").top_up(100)
assert team.budget("tokens").spend(30)
team.tally("errors").add(2, under="timeout")`);
    const node = await teamOn(sidecar, "nora");
    assert.equal(await node.budget("tokens").spend(20), true);
    assert.equal(await node.budget("tokens").spend(100), false);
    assert.deepEqual(await node.budget("tokens").spentBy(), { pia: 30, nora: 20 });
    await node.tally("errors").add(1, "timeout");
    await node.tally("errors").add();
    await node.close();
    const seen = asPython(sidecar.socket, null, `
print(json.dumps([team.budget("tokens").left(), team.budget("tokens").spent_by(), team.tally("errors").by_name(), team.budgets(), team.tallies()]))`);
    assert.deepEqual(seen, [50, { pia: 30, nora: 20 }, { timeout: 3, nora: 1 }, ["tokens"], ["errors"]]);
  }));

test("a document, and who wrote which part", () =>
  withSidecar(async (sidecar) => {
    asPython(sidecar.socket, "pia", `team.document("q3-report").append("Sales rose 4%. 🙂")`);
    const node = await teamOn(sidecar, "nora");
    await node.document("q3-report").append(" Costs fell. 🌍");
    await node.document("q3-report").insert(0, "Q3: ");
    await node.close();
    const seen = asPython(sidecar.socket, null, `print(json.dumps(team.document("q3-report").written_by()))`);
    assert.deepEqual(seen, [["Q3: ", "nora"], ["Sales rose 4%. 🙂", "pia"], [" Costs fell. 🌍", "nora"]]);
  }));

// Each finds the other's inbox, whatever the agent is called, and what one
// was given the other is not given again under the same name.
test("messages, both ways, and what was given", () =>
  withSidecar(async (sidecar) => {
    asPython(sidecar.socket, "pia", `
team.inbox.send("ålice smith", {"task": "t-1"})
team.inbox.send("ops/bob", "deploy")
team.inbox.send_all("Deploy is on Friday.")`);
    const alice = await teamOn(sidecar, "ålice smith");
    const got = await alice.inbox.unread();
    assert.deepEqual(got.map((message) => [message.sender, message.to, message.body]), [
      ["pia", "ålice smith", { task: "t-1" }],
      ["pia", null, "Deploy is on Friday."],
    ]);
    await alice.inbox.send("pia", "on it");
    await alice.close();
    const seen = asPython(sidecar.socket, "ålice smith", `print(json.dumps([m.body for m in team.inbox.unread()]))`);
    assert.deepEqual(seen, [], "the Python program was given again what the Node program had been given");
    const piaGot = asPython(sidecar.socket, "pia", `print(json.dumps([[m.sender, m.body] for m in team.inbox.unread()]))`);
    assert.deepEqual(piaGot, [["pia", "Deploy is on Friday."], ["ålice smith", "on it"]]);
    const bob = await teamOn(sidecar, "ops/bob");
    assert.deepEqual((await bob.inbox.unread()).map((message) => message.body), ["deploy", "Deploy is on Friday."]);
    await bob.close();
  }));

test("the roster", () =>
  withSidecar(async (sidecar) => {
    const node = await teamOn(sidecar, "nora");
    await node.roster.join();
    const seen = asPython(sidecar.socket, "pia", `
team.roster.join()
print(json.dumps(team.roster.here()))`);
    assert.deepEqual(seen, ["nora", "pia"]);
    // The Python program left when it closed.
    assert.deepEqual(await node.roster.here(), ["nora"]);
    await node.close();
  }));

// Whichever language writes something first, it is written down in the
// team's index under the same name.
test("the index is the same", () =>
  withSidecar(async (sidecar) => {
    const node = await teamOn(sidecar, "nora");
    await node.notes.put("a", 1);
    await node.budget("tokens").topUp(5);
    await node.document("plan").append("x");
    // A firm task that moved on and was finished keeps one line for its
    // tickets, whichever language did it.
    const task = await node.tasks.add("Send the invoice", { firm: true });
    await node.tasks.claim(task);
    await node.tasks.release(task);
    await node.tasks.claim(task);
    await node.tasks.finish(task, "sent");
    const index = await node.index();
    await node.close();
    const seen = asPython(sidecar.socket, null, `print(json.dumps(team.index()))`);
    assert.deepEqual(seen, index);
    assert.deepEqual(Object.keys(index).sort(), [
      "budget-topped/tokens",
      "budget/tokens",
      "doc/plan",
      "notes-mv",
      `task-takeover/${task.id}/1`,
      "tasks",
      "tasks-tree",
    ]);
    const fromPython = asPython(sidecar.socket, "pia", `
task = team.tasks.add("Check the figures", firm=True)
team.tasks.claim(task)
team.tasks.release(task)
team.tasks.claim(task)
team.tasks.finish(task, "ok")
print(json.dumps([task.id, sorted(name for name in team.index() if name.startswith("task-"))]))`) as [string, string[]];
    assert.deepEqual(fromPython[1].sort(), [`task-takeover/${fromPython[0]}/1`, `task-takeover/${task.id}/1`].sort());
  }));
