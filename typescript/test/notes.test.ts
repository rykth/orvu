// Notes: small facts the team shares.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest, Clash } from "../src/index.ts";
import { author, settled } from "../src/notes.ts";
import { teamOn, withTeam } from "./support.ts";

test("a note is put, read and removed", () =>
  withTeam(async (team) => {
    assert.equal(await team.notes.get("deploy-date"), null);
    await team.notes.put("deploy-date", "Friday");
    await team.notes.put("owner", "alice");
    assert.equal(await team.notes.get("deploy-date"), "Friday");
    assert.deepEqual(await team.notes.all(), { "deploy-date": "Friday", owner: "alice" });
    await team.notes.put("deploy-date", "Monday");
    assert.equal(await team.notes.get("deploy-date"), "Monday");
    await team.notes.remove("deploy-date");
    await team.notes.remove("never-there");
    assert.deepEqual(await team.notes.all(), { owner: "alice" });
  }));

// A note's name is only a key inside one object, so it can be any text.
test("a note's name can be any text", () =>
  withTeam(async (team) => {
    for (const name of ["deploy date", "a/b", "../x", "ü", "x".repeat(500), " "]) {
      await team.notes.put(name, "kept");
      assert.equal(await team.notes.get(name), "kept");
      await team.notes.remove(name);
    }
  }));

test("a note needs a name", () =>
  withTeam(async (team) => {
    for (const name of ["", null, 7]) {
      await assert.rejects(team.notes.put(name as string, "text"), (error: Error) => error instanceof BadRequest && /name/.test(error.message));
      await assert.rejects(team.notes.get(name as string), BadRequest);
    }
    assert.deepEqual(await team.notes.all(), {});
    assert.deepEqual(await team.index(), {}, "a note that was refused was still written down");
  }));

// A note takes anything JSON can hold, and gives it back as what it was.
test("a note holds any value and gives it back as it was", () =>
  withTeam(async (team, bob) => {
    for (const value of [7, -2.5, true, false, 0, "", "Friday", "7", "true", ["a", 1, null], { a: { b: [1, 2] } }, [], {}, "ü \x1e x", "\x1e7"]) {
      await team.notes.put("kept", value);
      for (const reader of [team, bob]) {
        assert.deepEqual(await reader.notes.get("kept"), value);
        assert.deepEqual(await reader.notes.all(), { kept: value });
      }
    }
  }));

test("a note cannot hold what JSON cannot", () =>
  withTeam(async (team) => {
    for (const value of [null, undefined, new Set([1]), NaN, () => 1, 10n]) {
      await assert.rejects(team.notes.put("kept", value), BadRequest, String(value));
    }
    assert.deepEqual(await team.notes.all(), {});
    assert.deepEqual(await team.index(), {}, "a note that was refused was still written down");
  }));

// Notes used to be kept in a plain map, with who wrote each in another. The
// sidecar moves them over the first time the notes are used, keeping who
// wrote each, and the old maps are left as they were.
test("notes kept the old way are moved over", () =>
  withTeam(async (team) => {
    const old = team.client.map("orvu/default/notes");
    await old.put("deploy-date", "Friday");
    await old.put("budget", "\x1e7");
    await team.client.map("orvu/default/notes-by").put("deploy-date", "bob");
    assert.deepEqual(await team.notes.all(), { "deploy-date": "Friday", budget: 7 });
    assert.deepEqual(await team.notes.authors(), { "deploy-date": "bob", budget: null });
    await team.notes.put("deploy-date", "Monday");
    assert.equal(await old.get("deploy-date"), "Friday");
    assert.deepEqual(await team.index(), { "notes-mv": "mvmap" });
  }));

// The text is kept after the writer's name, as it was in the old map.
test("a text note is kept as plain text after its writer", () =>
  withTeam(async (team) => {
    await team.notes.put("deploy-date", "Friday");
    assert.deepEqual(await team.client.mvMap("orvu/default/notes-mv").get("deploy-date"), ["alice\x1fFriday"]);
  }));

test("an agent whose name holds the separator cannot write notes", () =>
  withTeam(async (_team, _bob, sidecar) => {
    const odd = await teamOn(sidecar, "a\x1fb");
    await assert.rejects(odd.notes.put("deploy-date", "Friday"), (e: unknown) => e instanceof BadRequest && /cannot write notes/.test(e.message));
    await odd.close();
  }));

// A note two agents wrote at the same time gives every value and who wrote
// each; the same value written twice reads once. A clash can only be made
// between machines that cannot reach each other, which the Python tests do;
// here the values are read as the sidecar gives them.
test("a note that clashes gives every value and who wrote each", () => {
  const clash = settled("deploy-date", ["alice\x1fFriday", "\x1f\x1e[1]", "bob\x1fFriday"]);
  assert.ok(clash instanceof Clash);
  assert.deepEqual(clash.values, ["Friday", [1]]);
  assert.deepEqual(clash.authors, ["alice", null]);
  assert.match(clash.message, /Write it again/);
  assert.deepEqual(author(["alice\x1fFriday", "\x1f\x1e[1]"]), ["alice", null]);
  assert.equal(settled("x", ["alice\x1fFriday", "bob\x1fFriday"]), "Friday");
  assert.equal(author(["alice\x1fFriday", "bob\x1fFriday"]), "alice");
  assert.equal(settled("x", []), null);
  assert.equal(settled("x", ["no name"]), "no name");
});

test("a note says who wrote it", () =>
  withTeam(async (team, bob, sidecar) => {
    assert.equal(await team.notes.author("deploy-date"), null);
    await team.notes.put("deploy-date", "Friday");
    assert.equal(await team.notes.author("deploy-date"), "alice");
    await bob.notes.put("deploy-date", "Monday");
    await bob.notes.put("owner", "bob");
    assert.equal(await team.notes.author("deploy-date"), "bob");
    assert.deepEqual(await team.notes.authors(), { "deploy-date": "bob", owner: "bob" });
    // A program that gave no name has no name to leave.
    const nameless = await teamOn(sidecar);
    await nameless.notes.put("quiet", "x");
    assert.equal(await nameless.notes.author("quiet"), null);
    await nameless.close();
    await team.notes.remove("deploy-date");
    assert.deepEqual(await team.notes.authors(), { owner: "bob", quiet: null });
  }));

test("a structured note keeps fields", () =>
  withTeam(async (team) => {
    const plan = team.notes.structured("plan");
    assert.equal(await plan.toValue(), null);
    await plan.set(["owner"], "alice");
    await plan.set(["steps"], ["write", "test"]);
    await plan.set(["checked", "by"], "bob");
    assert.deepEqual(await plan.toValue(), { owner: "alice", steps: ["write", "test"], checked: { by: "bob" } });
    assert.equal(await team.notes.structured("plan").get(["checked", "by"]), "bob");
  }));

// Two agents fill in different fields of the same structured note. Both
// fields have to be there afterwards.
test("two agents changing different fields both keep their change", () =>
  withTeam(async (team, bob) => {
    await team.notes.structured("plan").set(["owner"], "alice");
    await bob.notes.structured("plan").set(["checker"], "bob");
    assert.deepEqual(await team.notes.structured("plan").toValue(), { owner: "alice", checker: "bob" });
  }));

test("a structured note needs a plain name", () =>
  withTeam(async (team) => {
    for (const name of ["", "a/b", "two words", null]) {
      assert.throws(() => team.notes.structured(name as string), (error: Error) => error instanceof BadRequest && /structured note/.test(error.message));
    }
    assert.deepEqual(await team.index(), {});
  }));

test("a simple and a structured note can share a name", () =>
  withTeam(async (team) => {
    await team.notes.put("plan", "see the structured one");
    await team.notes.structured("plan").set(["owner"], "alice");
    assert.equal(await team.notes.get("plan"), "see the structured one");
    assert.deepEqual(await team.notes.structured("plan").toValue(), { owner: "alice" });
  }));

// Reading writes nothing down in the team's index.
test("notes are written down in the team's index", () =>
  withTeam(async (team) => {
    await team.notes.get("deploy-date");
    await team.notes.all();
    assert.deepEqual(await team.index(), {});
    await team.notes.put("deploy-date", "Friday");
    assert.deepEqual(await team.index(), { "notes-mv": "mvmap" });
    await team.notes.structured("plan").get(["owner"]);
    await team.notes.structured("draft").toValue();
    assert.deepEqual(await team.index(), { "notes-mv": "mvmap" }, "reading a structured note must not write it down");
    await team.notes.structured("plan").set(["owner"], "alice");
    assert.deepEqual(await team.index(), { "notes-mv": "mvmap", "note/plan": "json" });
    assert.deepEqual(await team.notes.structuredNames(), ["plan"]);
  }));

// A note written by a program with no name has no author. It must not keep
// the name of whoever wrote the value before.
test("a note rewritten by a nameless program loses its author", () =>
  withTeam(async (team, _bob, sidecar) => {
    await team.notes.put("deploy-date", "Friday");
    const nameless = await teamOn(sidecar);
    await nameless.notes.put("deploy-date", "Monday");
    await nameless.close();
    assert.equal(await team.notes.get("deploy-date"), "Monday");
    assert.equal(await team.notes.author("deploy-date"), null);
  }));
