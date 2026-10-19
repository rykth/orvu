// The rules for the names the building blocks give their objects.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest } from "../src/index.ts";
import * as names from "../src/names.ts";

test("every object of a team is under the team's own prefix", () => {
  const all = [names.index("crew"), names.notes("crew"), names.note("crew", "plan"), names.tasks("crew"), names.taskTree("crew"), names.budget("crew", "tokens")];
  assert.ok(all.every((name) => name.startsWith("orvu/crew/")));
  assert.equal(new Set(all).size, all.length);
});

// A block's objects all start with a word of its own, and a caller's name
// cannot contain a slash. So whatever a note or a budget is called, it
// cannot land on another block's object.
for (const name of ["notes", "tasks", "tasks-tree", "index", "budget", "note"]) {
  test(`no name a caller picks lands on another block's object: ${name}`, () => {
    const fixed = new Set([names.index("crew"), names.notes("crew"), names.tasks("crew"), names.taskTree("crew")]);
    assert.ok(!fixed.has(names.note("crew", names.part(name, "a note"))));
    assert.ok(!fixed.has(names.budget("crew", names.part(name, "a budget"))));
    assert.notEqual(names.note("crew", name), names.budget("crew", name));
  });
}

test("ordinary names are accepted", () => {
  for (const name of ["plan", "Q3-report", "v1.2", "a", "tokens_left", "7", "x".repeat(64)]) {
    assert.equal(names.part(name, "a name"), name);
  }
});

test("names that could cause trouble are refused", () => {
  for (const name of ["", "a/b", "../x", " plan", "plan ", "-plan", ".plan", "a b", "ü", "x".repeat(65), null, 7, "a\nb"]) {
    assert.throws(() => names.part(name, "a budget"), (error: Error) => error instanceof BadRequest && /cannot be used as a budget/.test(error.message), String(name));
  }
});

// An agent's inbox is kept under its name made safe the way the Python
// package makes it, so that both find the same inbox.
test("an inbox is named as the Python package names it", () => {
  for (const [name, safe] of [
    ["alice", "alice"],
    ["ålice smith", "%C3%A5lice%20smith"],
    ["ops/alice", "ops%2Falice"],
    ["@everyone", "%40everyone"],
    ["%41", "%2541"],
    ["a!b'c(d)e*f~g", "a%21b%27c%28d%29e%2Af~g"],
    ["x.y_z-w", "x.y_z-w"],
  ]) {
    assert.equal(names.inboxOwner(name), safe);
  }
});
