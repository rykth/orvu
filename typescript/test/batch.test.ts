// Several operations in one message.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest, TooLarge, TypeMismatch } from "../src/index.ts";
import { MIB, withClient } from "./support.ts";

test("a batch sends everything at once and keeps the order", () =>
  withClient(async (client) => {
    const batch = client.batch();
    batch.counter("visits").increment(5);
    batch.list("notes").insert(0, "a");
    batch.list("notes").insert(1, "b");
    const total = batch.counter("visits").value();
    const notes = batch.list("notes").toArray();
    await batch.send();
    assert.equal(total.value, 5);
    assert.deepEqual(notes.value, ["a", "b"]);
    assert.equal(batch.results.length, 5);
    // What the batch did is there for ordinary calls too.
    assert.equal(await client.counter("visits").value(), 5);
  }));

test("a value is not there before the batch is sent", () =>
  withClient(async (client) => {
    const batch = client.batch();
    const pending = batch.counter("visits").value();
    assert.throws(() => pending.value, BadRequest);
    await batch.send();
    assert.equal(pending.value, 0);
  }));

test("one refusal or failure does not undo the others", () =>
  withClient(async (client) => {
    await client.counter("thing").increment(1);
    const batch = client.batch();
    batch.boundedCounter("budget").increment(3);
    const tooMuch = batch.boundedCounter("budget").tryDecrement(5);
    const wrongType = batch.list("thing").insert(0, "x");
    const fine = batch.boundedCounter("budget").tryDecrement(2);
    await batch.send();
    assert.equal(tooMuch.value, false);
    assert.equal(fine.value, true);
    assert.throws(() => wrongType.value, TypeMismatch);
    assert.equal(await client.boundedCounter("budget").value(), 1);
  }));

// A request that is itself wrong stops the whole batch before anything runs.
test("a bad request in a batch applies nothing", () =>
  withClient(async (client) => {
    const batch = client.batch();
    batch.counter("visits").increment(5);
    batch.richText("page").format(0, 1, "bold", "");
    await assert.rejects(batch.send(), BadRequest);
    assert.equal(await client.counter("visits").value(), 0);
  }));

test("watch cannot be part of a batch", () =>
  withClient(async (client) => {
    assert.throws(() => client.batch().counter("visits").watch(), BadRequest);
  }));

test("sending a batch twice does not run it twice", () =>
  withClient(async (client) => {
    const batch = client.batch();
    batch.counter("visits").increment(1);
    await batch.send();
    assert.equal(await client.counter("visits").value(), 1);
    assert.equal(await batch.send(), batch.results);
    assert.equal(await client.counter("visits").value(), 1);
  }));

test("nothing can be added to a batch that was sent", () =>
  withClient(async (client) => {
    const batch = client.batch();
    batch.counter("visits").increment(1);
    await batch.send();
    assert.throws(() => batch.counter("visits").increment(1), BadRequest);
  }));

// A batch is sent once, even when it is asked twice at the same moment.
test("two sends at the same moment send the batch once", () =>
  withClient(async (client) => {
    const batch = client.batch();
    batch.counter("visits").increment(1);
    const [first, second] = await Promise.all([batch.send(), batch.send()]);
    assert.equal(first, second);
    assert.equal(await client.counter("visits").value(), 1);
  }));

test("nothing can be added while the batch is being sent", () =>
  withClient(async (client) => {
    const batch = client.batch();
    batch.counter("visits").increment(1);
    const sending = batch.send();
    assert.throws(() => batch.counter("visits").increment(1), BadRequest);
    await sending;
    assert.equal(batch.results.length, 1);
    assert.equal(await client.counter("visits").value(), 1);
  }));

// The sidecar stops running a batch's operations once their replies add up to
// more than 4 MiB. The ones it did run keep their results.
test("a batch with too much to read keeps what it did read", () =>
  withClient(async (client) => {
    await client.register("big").set("x".repeat((3 * MIB) / 2));
    const batch = client.batch();
    const reads = Array.from({ length: 6 }, () => batch.register("big").get());
    await batch.send();
    assert.equal(reads[0].value!.length, (3 * MIB) / 2);
    assert.throws(() => reads[5].value, TooLarge);
  }));
