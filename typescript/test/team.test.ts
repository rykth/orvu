// The team: the one object the building blocks hang off.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest, Team } from "../src/index.ts";
import { sidecars, teamOn, withHome, withSidecar, withTeam } from "./support.ts";

test("a team gives out its blocks", () =>
  withTeam(async (team) => {
    assert.equal(team.name, "default");
    assert.equal(team.me, "alice");
    await team.notes.put("deploy-date", "Friday");
    assert.equal(await team.notes.get("deploy-date"), "Friday");
    assert.deepEqual(await team.tasks.list(), []);
    assert.equal(await team.budget("tokens").left(), 0);
  }));

test("two agents on one team share everything", () =>
  withTeam(async (alice, bob) => {
    await alice.notes.put("deploy-date", "Friday");
    const task = await alice.tasks.add("Write the report");
    await alice.budget("tokens").topUp(10);
    assert.equal(await bob.notes.get("deploy-date"), "Friday");
    assert.deepEqual((await bob.tasks.list()).map((found) => found.id), [task.id]);
    assert.equal(await bob.budget("tokens").left(), 10);
  }));

test("two teams on one sidecar share nothing", () =>
  withSidecar(async (sidecar) => {
    const red = await Team.connect("red", { socket: sidecar.socket });
    const blue = await Team.connect("blue", { socket: sidecar.socket });
    await red.notes.put("deploy-date", "Friday");
    await red.tasks.add("Write the report");
    await red.budget("tokens").topUp(10);
    assert.deepEqual(await blue.notes.all(), {});
    assert.deepEqual(await blue.tasks.list(), []);
    assert.equal(await blue.budget("tokens").left(), 0);
    assert.deepEqual(await blue.index(), {});
    await red.close();
    await blue.close();
  }));

// The sidecar cannot list what it holds, so the team writes each object
// down the first time a block creates it.
test("the index lists what the team has", () =>
  withTeam(async (team) => {
    assert.deepEqual(await team.index(), {});
    await team.notes.get("nothing");
    await team.budget("tokens").left();
    await team.tasks.list();
    await team.document("q3-report").text();
    await team.tally("errors").total();
    await team.roster.members();
    await team.inbox.messages();
    await team.budget("tokens").spentBy();
    await team.budget("tokens").toppedUpBy();
    await team.notes.authors();
    assert.deepEqual(await team.index(), {}, "reading must not put anything in the index");

    await team.notes.put("deploy-date", "Friday");
    await team.notes.structured("plan").set(["owner"], "alice");
    await team.budget("tokens").topUp(5);
    await team.budget("money").topUp(5);
    await team.tasks.add("Write the report");
    await team.budget("tokens").spend(1);
    await team.document("q3-report").append("Sales rose 4%.");
    await team.tally("errors").add();
    await team.roster.join();
    await team.inbox.send("bob", "hello");
    await team.inbox.sendAll("hello");

    assert.deepEqual(await team.index(), {
      "notes-mv": "mvmap",
      "note/plan": "json",
      "budget/tokens": "bounded",
      "budget/money": "bounded",
      "budget-spent/tokens": "cart",
      "budget-topped/tokens": "cart",
      "budget-topped/money": "cart",
      tasks: "json",
      "tasks-tree": "tree",
      "doc/q3-report": "richtext",
      "tally/errors": "cart",
      roster: "map",
      "inbox/bob/0": "list",
      "inbox/@everyone/0": "list",
      "inbox-count/bob": "cart",
      "inbox-count/@everyone": "cart",
    });
    assert.deepEqual(await team.documents(), ["q3-report"]);
    assert.deepEqual(await team.tallies(), ["errors"]);
    assert.deepEqual(await team.budgets(), ["money", "tokens"]);
    assert.deepEqual(await team.notes.structuredNames(), ["plan"]);
  }));

test("the index is the same for an agent that connects later", () =>
  withTeam(async (team, _bob, sidecar) => {
    await team.budget("tokens").topUp(5);
    const later = await teamOn(sidecar);
    assert.deepEqual(await later.budgets(), ["tokens"]);
    await later.close();
  }));

// What the index says an object is has to be what it is, or the index is no
// use for finding things.
test("what the index says matches the real objects", () =>
  withTeam(async (team) => {
    await team.notes.put("deploy-date", "Friday");
    await team.notes.structured("plan").set(["owner"], "alice");
    await team.budget("tokens").topUp(5);
    await team.tasks.add("Write the report");
    await team.budget("tokens").spend(1);
    await team.document("q3-report").append("Sales rose 4%.");
    await team.tally("errors").add();
    await team.roster.join();
    team.inbox.page = 1;
    await team.inbox.send("bob", "hello");
    await team.inbox.send("bob", "again");
    await team.inbox.sendAll("hello");
    const c = team.client;
    const read: Record<string, (name: string) => Promise<unknown>> = {
      map: (name) => c.map(name).entries(),
      mvmap: (name) => c.mvMap(name).entries(),
      json: (name) => c.json(name).toValue(),
      bounded: (name) => c.boundedCounter(name).value(),
      tree: (name) => c.tree(name).toNodes(),
      cart: (name) => c.cart(name).items(),
      richtext: (name) => c.richText(name).text(),
      list: (name) => c.list(name).toArray(),
      counter: (name) => c.counter(name).value(),
    };
    const index = await team.index();
    assert.deepEqual(new Set(Object.values(index)), new Set(Object.keys(read)), "a kind of object is not checked here");
    for (const [shortName, kind] of Object.entries(index)) {
      const value = await read[kind](`orvu/${team.name}/${shortName}`);
      const empty = value === 0 || value === "" || value === null || (typeof value === "object" && Object.keys(value as object).length === 0);
      assert.ok(!empty, `${shortName} is empty or is not a ${kind}`);
    }
  }));

test("a team name that cannot be used is refused before connecting", () =>
  withHome(async (home) => {
    for (const name of ["", "a/b", "two words", "x".repeat(65)]) {
      await assert.rejects(Team.connect(name), (error: Error) => error instanceof BadRequest && /name of a team/.test(error.message));
    }
    assert.deepEqual(sidecars(home), [], "a sidecar was started for a team that cannot exist");
  }));

test("a team connects with no setup", () =>
  withHome(async (home) => {
    const alice = await Team.connect("default", { me: "alice" });
    await alice.notes.put("deploy-date", "Friday");
    await alice.close();
    const bob = await Team.connect("default", { me: "bob" });
    assert.equal(await bob.notes.get("deploy-date"), "Friday");
    await bob.close();
    assert.equal(sidecars(home).length, 1);
  }));

// The blocks keep their data under `orvu/`. A program that also uses plain
// objects through `team.client` is not in their way.
test("plain objects live beside the blocks", () =>
  withTeam(async (team) => {
    await team.client.counter("visits").increment(3);
    await team.notes.put("visits", "see the counter");
    assert.equal(await team.client.counter("visits").value(), 3);
    assert.equal(await team.notes.get("visits"), "see the counter");
  }));

test("a team can be closed with await using", () =>
  withSidecar(async (sidecar) => {
    {
      await using team = await teamOn(sidecar, "alice");
      await team.roster.join();
    }
    const later = await teamOn(sidecar);
    assert.deepEqual(await later.roster.members(), []);
    await later.close();
  }));
