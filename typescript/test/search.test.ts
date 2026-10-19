// Search by meaning. The sidecars here use a made-up model that knows a few
// dozen words in groups, such as refund, money and back, and deploy, ship and
// friday: words of one group mean the same, and words of different groups
// have nothing in common.

import * as grpc from "@grpc/grpc-js";
import assert from "node:assert/strict";
import { mock, test } from "node:test";
import { BadRequest, SearchUnavailable, Team, type Found } from "../src/index.ts";
import { waitUntil, withSidecar, withTeam } from "./support.ts";

const kinds = (found: Found[]) => found.map((each) => [each.kind, each.name]);

test("notes and documents are found by meaning", () =>
  withTeam(async (team) => {
    await team.notes.put("refund-policy", "customers get their money back");
    await team.notes.put("deploy-date", "friday");
    await team.notes.structured("q3").set(["sales"], "rose");
    await team.document("handbook").append("Ship on friday.\n\nA refund is paid, money back, repay.");
    assert.deepEqual(kinds(await team.search("refund")), [["document", "handbook"], ["note", "refund-policy"]]);
    assert.deepEqual(kinds(await team.search("income")), [["structured", "q3"]]);
    const [first] = await team.search("refund", 1);
    assert.deepEqual(first, { kind: "document", name: "handbook", paragraph: 2, text: "A refund is paid, money back, repay.", score: first.score });
    assert.ok(first.score > 0 && first.score <= 1);
    assert.deepEqual(await team.search("the cat"), []);
  }));

test("what one agent writes, another finds within a moment", () =>
  withTeam(async (team, bob) => {
    assert.deepEqual(await bob.search("deploy"), []);
    await team.document("draft").append("We ship on friday");
    assert.ok(await waitUntil(async () => JSON.stringify(kinds(await bob.search("deploy"))) === '[["document","draft"]]'));
  }));

test("an agent finds only what it may read", () =>
  withSidecar(async (sidecar) => {
    const writer = await Team.connect("default", { socket: sidecar.socket });
    await writer.notes.put("refund-policy", "money back");
    await writer.document("handbook").append("refund");
    const key = sidecar.addAgent("reader");
    sidecar.allow("reader", "documents:read");
    const reader = await Team.connect("default", { socket: sidecar.socket, key });
    assert.deepEqual(kinds(await reader.search("refund")), [["document", "handbook"]]);
    await reader.close();
    await writer.close();
  }));

test("at most as many are found as are asked for, from one to fifty", () =>
  withTeam(async (team) => {
    for (let n = 0; n < 8; n++) {
      await team.notes.put(`refund-${n}`, "money back");
    }
    assert.equal((await team.search("refund")).length, 5);
    assert.equal((await team.search("refund", 2)).length, 2);
    assert.equal((await team.search("refund", 50)).length, 8);
    for (const wrong of [0, 51, 2.5, "5", true]) {
      await assert.rejects(team.search("refund", wrong as number), (error: Error) => error instanceof BadRequest && /from 1 to 50/.test(error.message));
    }
    await assert.rejects(team.search(null as unknown as string), BadRequest);
    await assert.rejects(team.client.search("no/slashes", "refund"), BadRequest);
  }));

test("a search that cannot be done says why", () =>
  withSidecar(async (sidecar) => {
    const team = await Team.connect("default", { socket: sidecar.socket });
    // A team that has nothing needs no model to find nothing.
    assert.deepEqual(await team.search("refund"), []);
    await team.notes.put("refund-policy", "money back");
    await assert.rejects(team.search("refund"), (error: Error) => error instanceof SearchUnavailable && /no-model-here/.test(error.message));
    await team.close();
  }, "--model", "/nowhere/no-model-here"));

// The first search may fetch the model, which takes longer than other calls
// may, so a search waits longer than they do: ten minutes. The clock is held
// still, so that each wait is seen exactly.
test("a search waits longer than other calls", () =>
  withTeam(async (team) => {
    const waited: [string, number][] = [];
    const createCall = grpc.Channel.prototype.createCall;
    grpc.Channel.prototype.createCall = function (this: grpc.Channel, method: string, deadline: grpc.Deadline, ...rest: unknown[]) {
      waited.push([method.split("/").pop()!, Number(deadline) - Date.now()]);
      return (createCall as (...a: unknown[]) => ReturnType<typeof createCall>).call(this, method, deadline, ...rest);
    } as typeof createCall;
    mock.timers.enable({ apis: ["Date"], now: Date.now() });
    try {
      await team.client.map("m").get("k");
      await team.search("refund");
    } finally {
      mock.timers.reset();
      grpc.Channel.prototype.createCall = createCall;
    }
    assert.deepEqual(waited, [["Execute", 10_000], ["Execute", 600_000]]);
  }));

test("tasks are found by their titles and notes", () =>
  withTeam(async (team) => {
    const pay = await team.tasks.add("Pay the refund");
    const write = await team.tasks.add("Write the summary", { notes: "deploy on friday" });
    assert.deepEqual(kinds(await team.search("refund")), [["task", pay.id]]);
    const [hit] = await team.search("ship");
    const title: string | undefined = hit.kind === "task" ? hit.title : undefined;
    assert.equal(title, "Write the summary");
    assert.deepEqual(hit, { kind: "task", name: write.id, title: "Write the summary", text: "Write the summary: deploy on friday", score: hit.score });
    await team.tasks.update(pay, { notes: "ship it" });
    assert.ok(await waitUntil(async () => (await team.search("ship")).length === 2));
  }));

test("tasks are found only by an agent that may read them", () =>
  withSidecar(async (sidecar) => {
    const writer = await Team.connect("default", { socket: sidecar.socket });
    await writer.tasks.add("Pay the refund");
    await writer.notes.put("plan", "money back");
    const key = sidecar.addAgent("reader");
    sidecar.allow("reader", "notes:read");
    const reader = await Team.connect("default", { socket: sidecar.socket, key });
    assert.deepEqual(kinds(await reader.search("refund")), [["note", "plan"]]);
    await reader.close();
    await writer.close();
  }));
