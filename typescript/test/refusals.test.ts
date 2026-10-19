// Being told no is an ordinary answer. Real failures are named errors.

import assert from "node:assert/strict";
import { test } from "node:test";
import * as orvu from "../src/index.ts";
import { withClient } from "./support.ts";

test("running out gives false and changes nothing", () =>
  withClient(async (client) => {
    const budget = client.boundedCounter("budget");
    await budget.increment(3);
    assert.equal(await budget.tryDecrement(5), false);
    assert.equal(await budget.value(), 3);
    assert.equal(await budget.allowance(), 3);
    assert.equal(await budget.tryDecrement(3), true);
    assert.equal(await budget.tryDecrement(1), false);
    assert.equal(await budget.value(), 0);
  }));

test("handing over more than you have gives false", () =>
  withClient(async (client) => {
    const budget = client.boundedCounter("budget");
    await budget.increment(3);
    assert.equal(await budget.transfer(999, 5), false);
    assert.equal(await budget.allowance(), 3);
  }));

test("using a name as another type is a type mismatch", () =>
  withClient(async (client) => {
    await client.counter("thing").increment(1);
    await assert.rejects(client.list("thing").insert(0, "x"), (error: Error) => error instanceof orvu.TypeMismatch && error.message.includes("thing"));
    // The counter is untouched.
    assert.equal(await client.counter("thing").value(), 1);
  }));

test("using a name under other rules is a class mismatch", () =>
  withClient(async (client) => {
    await client.declare("budget", "escrow");
    await assert.rejects(client.counter("budget").increment(1), orvu.ClassMismatch);
    // Under the rules it was declared with, it works.
    await client.boundedCounter("budget").increment(1);
  }));

test("an unknown class is caught before anything is sent", () =>
  withClient(async (client) => {
    await assert.rejects(client.declare("thing", "strict" as "ap"), orvu.BadRequest);
  }));

test("every error is an orvu error", () => {
  for (const error of [
    orvu.SidecarUnavailable,
    orvu.VersionMismatch,
    orvu.TimedOut,
    orvu.BadRequest,
    orvu.TypeMismatch,
    orvu.ClassMismatch,
    orvu.OutOfBounds,
    orvu.JsonError,
    orvu.TooLarge,
    orvu.EngineError,
  ]) {
    const made = new error("x");
    assert.ok(made instanceof orvu.OrvuError && made instanceof Error);
    assert.equal(made.name, error.name);
  }
});

// Reading never creates anything. A name that has only been read can still
// be used for any type.
test("reading does not take the name", () =>
  withClient(async (client) => {
    assert.equal(await client.counter("thing").value(), 0);
    assert.deepEqual(await client.list("thing").toArray(), []);
    await client.register("thing").set("now it is a register");
    assert.equal(await client.register("thing").get(), "now it is a register");
  }));

// A value that cannot be sent, such as a negative count, is the caller's
// mistake. It is reported like every other failure, and nothing is sent.
test("a value that cannot be sent is a bad request", () =>
  withClient(async (client) => {
    await assert.rejects(client.counter("visits").increment(-1), orvu.BadRequest);
    await assert.rejects(client.counter("visits").increment(1.5), orvu.BadRequest);
    await assert.rejects(client.list("notes").insert(-1, "x"), orvu.BadRequest);
    await assert.rejects(client.boundedCounter("budget").tryDecrement(-5), orvu.BadRequest);
    await assert.rejects(client.counter("visits").increment("many" as unknown as number), orvu.BadRequest);
    await assert.rejects(client.register("title").set(7 as unknown as string), orvu.BadRequest);
    await assert.rejects(client.json("doc").set(["when"], 10n), orvu.BadRequest);
    await assert.rejects(client.json("doc").set(["when"], undefined), orvu.BadRequest);
    assert.equal(await client.counter("visits").value(), 0);
  }));

// A mistake while a batch is filled is thrown at once, and the batch is
// then not sent at all: what was added before the mistake is not sent
// without what was meant to follow it.
test("a value that cannot be sent stops a batch", () =>
  withClient(async (client) => {
    const batch = client.batch();
    batch.counter("visits").increment(1);
    assert.throws(() => batch.counter("visits").increment(-1), orvu.BadRequest);
    await assert.rejects(batch.send(), (error: Error) => error instanceof orvu.BadRequest && /not sent/.test(error.message));
    assert.equal(await client.counter("visits").value(), 0);
  }));
