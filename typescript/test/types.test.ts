// Each of the eleven kinds of object, method by method.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import * as path from "node:path";
import { test } from "node:test";
import { BadRequest, connect, JsonError, OrvuError, OutOfBounds } from "../src/index.ts";
import { json } from "../src/wire.ts";
import { REPO, withClient } from "./support.ts";

test("counter", () =>
  withClient(async (client) => {
    const visits = client.counter("visits");
    assert.equal(await visits.value(), 0); // nothing written yet
    await visits.increment();
    await visits.increment(5);
    await visits.decrement(2);
    assert.equal(await visits.value(), 4);
    await visits.decrement(10);
    assert.equal(await visits.value(), -6); // a plain counter may go below zero
  }));

test("register", () =>
  withClient(async (client) => {
    const title = client.register("title");
    assert.equal(await title.get(), null); // never set
    await title.set("Draft");
    await title.set("Final");
    assert.equal(await title.get(), "Final");
    await title.set("");
    assert.equal(await title.get(), ""); // set to empty is not the same as never set
  }));

test("set", () =>
  withClient(async (client) => {
    const tags = client.set("tags");
    assert.deepEqual(await tags.elements(), new Set());
    await tags.add("red");
    await tags.add("blue");
    await tags.add("red");
    assert.deepEqual(await tags.elements(), new Set(["red", "blue"]));
    assert.equal(await tags.contains("red"), true);
    await tags.remove("red");
    assert.equal(await tags.contains("red"), false);
    assert.deepEqual(await tags.elements(), new Set(["blue"]));
  }));

test("map", () =>
  withClient(async (client) => {
    const profile = client.map("profile");
    assert.equal(await profile.get("name"), null);
    assert.deepEqual(await profile.keys(), []);
    assert.deepEqual(await profile.entries(), {});
    await profile.put("name", "alice");
    await profile.put("city", "berlin");
    await profile.put("city", "paris");
    assert.equal(await profile.get("city"), "paris");
    assert.deepEqual((await profile.keys()).sort(), ["city", "name"]);
    await profile.remove("city");
    assert.deepEqual(await profile.entries(), { name: "alice" });
  }));

test("mvMap", () =>
  withClient(async (client) => {
    const plans = client.mvMap("plans");
    assert.deepEqual(await plans.get("launch"), []);
    assert.deepEqual(await plans.keys(), []);
    assert.deepEqual(await plans.entries(), {});
    await plans.put("launch", "friday");
    await plans.put("launch", "monday");
    await plans.put("owner", "");
    // A write replaces what this machine has seen, so there is no clash.
    assert.deepEqual(await plans.get("launch"), ["monday"]);
    assert.deepEqual(await plans.get("owner"), [""]); // an empty value is a value
    assert.deepEqual((await plans.keys()).sort(), ["launch", "owner"]);
    await plans.remove("owner");
    assert.deepEqual(await plans.entries(), { launch: ["monday"] });
  }));

// A key can be empty text, and comes back as that.
test("an empty key is a key", () =>
  withClient(async (client) => {
    const plain = client.map("plain");
    await plain.put("", "x");
    assert.deepEqual(await plain.entries(), { "": "x" });
    assert.deepEqual(await plain.keys(), [""]);
    const kept = client.mvMap("kept");
    await kept.put("", "x");
    assert.deepEqual(await kept.entries(), { "": ["x"] });
    assert.deepEqual(await kept.keys(), [""]);
    const cart = client.cart("cart");
    await cart.add("", 2);
    assert.deepEqual(await cart.items(), { "": 2 });
  }));

test("cart", () =>
  withClient(async (client) => {
    const basket = client.cart("basket");
    assert.equal(await basket.quantity("apple"), 0);
    assert.deepEqual(await basket.items(), {});
    await basket.add("apple");
    await basket.add("apple", 4);
    await basket.add("pear", 2);
    await basket.removeQty("apple", 2);
    assert.equal(await basket.quantity("apple"), 3);
    assert.deepEqual(await basket.items(), { apple: 3, pear: 2 });
    await basket.removeItem("pear");
    assert.deepEqual(await basket.items(), { apple: 3 });
    await basket.removeQty("apple", 10);
    assert.equal(await basket.quantity("apple"), -7); // nothing stops this going below zero
  }));

test("list", () =>
  withClient(async (client) => {
    const notes = client.list("notes");
    assert.equal(await notes.length(), 0);
    assert.deepEqual(await notes.toArray(), []);
    assert.equal(await notes.get(0), null);
    await notes.insert(0, "first");
    await notes.insert(1, "third");
    await notes.insert(1, "second");
    assert.deepEqual(await notes.toArray(), ["first", "second", "third"]);
    assert.equal(await notes.get(1), "second");
    assert.equal(await notes.length(), 3);
    await notes.delete(0);
    assert.deepEqual(await notes.toArray(), ["second", "third"]);
    await assert.rejects(notes.insert(9, "too far"), OutOfBounds);
    await assert.rejects(notes.delete(9), OutOfBounds);
  }));

test("json", () =>
  withClient(async (client) => {
    const doc = client.json("doc");
    assert.equal(await doc.toValue(), null);
    assert.equal(await doc.get(["title"]), null);
    await doc.set(["title"], "Plan");
    await doc.set(["meta", "done"], false); // the object "meta" is created on the way
    await doc.set(["tags"], ["a", "b"]);
    assert.equal(await doc.get(["title"]), "Plan");
    assert.equal(await doc.get(["tags", 1]), "b");
    assert.deepEqual(await doc.toValue(), { title: "Plan", meta: { done: false }, tags: ["a", "b"] });
    await doc.set(["tags"], ["c"]); // setting replaces
    assert.deepEqual(await doc.get(["tags"]), ["c"]);
    await doc.remove(["meta", "done"]);
    assert.deepEqual(await doc.get(["meta"]), {});
    await doc.remove(["title"]);
    assert.equal(await doc.get(["title"]), null);
    // A write may not go through a list position.
    await assert.rejects(doc.set(["tags", 0, "inside"], 1), JsonError);
  }));

test("an item is put into a list inside a json document", () =>
  withClient(async (client, sidecar) => {
    const doc = client.json("doc");
    await doc.set(["steps"], ["b"]);
    await doc.insert(["steps"], 0, "a");
    await doc.insert(["steps"], 2, { then: "d" });
    await doc.insert(["steps"], 2, "c");
    assert.deepEqual(await doc.get(["steps"]), ["a", "b", "c", { then: "d" }]);
    // Where there is nothing yet, position 0 starts a list.
    await doc.insert(["log"], 0, "first");
    await doc.insert(["log"], 1, "second");
    assert.deepEqual(await doc.get(["log"]), ["first", "second"]);
    for (const [path, index] of [[["steps"], 9], [["steps", 0], 0], [["nothing-here"], 1]] as const) {
      await assert.rejects(doc.insert([...path], index, "x"), OrvuError);
    }
    await assert.rejects(doc.insert(["steps"], -1, "x"), BadRequest);
    await assert.rejects(doc.insert(["steps"], 0, 10n), BadRequest);
    assert.deepEqual(await doc.get(["steps"]), ["a", "b", "c", { then: "d" }]);

    // What is there and is not a list is left as it is.
    await doc.set(["cfg"], { a: 1 });
    await doc.set(["title"], "Plan");
    for (const path of [["cfg"], ["title"]]) {
      await assert.rejects(doc.insert(path, 0, "x"), (error: Error) => error instanceof JsonError && /not a list/.test(error.message));
    }
    assert.deepEqual(await doc.get(["cfg"]), { a: 1 });

    // Two programs that add to the same list both keep what they added.
    const other = await connect(sidecar.socket);
    await other.json("doc").insert(["log"], 2, "from the other");
    other.close();
    await doc.insert(["log"], 3, "and mine");
    assert.deepEqual(await doc.get(["log"]), ["first", "second", "from the other", "and mine"]);
    const batch = client.batch();
    batch.json("doc").insert(["log"], 0, "in a batch");
    await batch.send();
    assert.equal(((await doc.get(["log"])) as string[])[0], "in a batch");
  }));

test("rich text", () =>
  withClient(async (client) => {
    const page = client.richText("page");
    assert.equal(await page.text(), "");
    assert.deepEqual(await page.toSpans(), []);
    await page.insert(0, "héllo wörld"); // positions count characters, not bytes
    await page.insert(5, " ✓");
    assert.equal(await page.text(), "héllo ✓ wörld");
    await page.format(0, 5, "bold", "true");
    assert.deepEqual((await page.toSpans())[0], { text: "héllo", marks: { bold: "true" } });
    await page.unformat(0, 2, "bold");
    const spans = await page.toSpans();
    assert.deepEqual(spans[0], { text: "hé", marks: {} });
    assert.deepEqual(spans[1], { text: "llo", marks: { bold: "true" } });
    await page.delete(0, 6);
    assert.equal(await page.text(), "✓ wörld");
    // A format with no value is a mistake in the request.
    await assert.rejects(page.format(0, 1, "bold", ""), BadRequest);
  }));

// Positions count characters, as the sidecar does, and not the units a
// JavaScript string counts, in which an emoji is two.
test("rich text positions count characters", () =>
  withClient(async (client) => {
    const page = client.richText("page");
    await page.insert(0, "a😀b");
    await page.insert(2, "!");
    assert.equal(await page.text(), "a😀!b");
    assert.equal([...(await page.text())].length, 4);
  }));

test("tree", () =>
  withClient(async (client) => {
    const outline = client.tree("outline");
    assert.deepEqual(await outline.children("root"), []);
    assert.deepEqual(await outline.toNodes(), []);
    await outline.moveNode("report", "root");
    await outline.moveNode("intro", "report");
    await outline.moveNodeAt("summary", "report", 0);
    await outline.setLabel("intro", "Introduction");
    assert.deepEqual(await outline.children("root"), ["report"]);
    assert.deepEqual(new Set(await outline.children("report")), new Set(["intro", "summary"]));
    assert.equal(await outline.parent("intro"), "report");
    assert.equal(await outline.parent("root"), null);
    assert.equal(await outline.parent("nobody"), null);
    const nodes = Object.fromEntries((await outline.toNodes()).map((node) => [node.id, node]));
    assert.equal(nodes.intro.parent, "report");
    assert.equal(nodes.intro.label, "Introduction");
    assert.equal(nodes.root.parent, null);
    // Putting "report" under its own child would make a loop, so it is skipped.
    await outline.moveNode("report", "intro");
    assert.deepEqual(await outline.children("root"), ["report"]);
  }));

test("bounded counter", () =>
  withClient(async (client) => {
    const budget = client.boundedCounter("budget");
    assert.equal(await budget.value(), 0);
    assert.equal(await budget.allowance(), 0);
    await budget.increment(10);
    assert.equal(await budget.value(), 10);
    assert.equal(await budget.allowance(), 10);
    assert.equal(await budget.tryDecrement(4), true);
    assert.equal(await budget.value(), 6);
    // Hand 2 to another machine. The total stays, our share shrinks.
    assert.equal(await budget.transfer(999, 2), true);
    assert.equal(await budget.value(), 6);
    assert.equal(await budget.allowance(), 4);
  }));

// A value JSON cannot hold is refused, and not changed into one it can:
// JSON.stringify would turn NaN into null and leave undefined out.
test("a value JSON cannot hold is refused, not changed", () =>
  withClient(async (client) => {
    const doc = client.json("doc");
    const cycle: Record<string, unknown> = {};
    cycle.self = cycle;
    for (const value of [NaN, Infinity, -Infinity, undefined, { a: undefined }, [1, undefined], () => 1, Symbol("s"), 10n, new Date(0), new Map(), new Set(), cycle, [NaN]]) {
      await assert.rejects(doc.set(["x"], value), BadRequest, String(value));
    }
    await assert.rejects(doc.insert(["xs"], 0, [1, NaN]), BadRequest);
    assert.equal(await doc.toValue(), null);
    // Plain values, and objects with no prototype, are fine.
    const bare = Object.assign(Object.create(null), { a: 1 });
    await doc.set(["x"], { n: 1.5, big: 1e300, list: [null, true, "s"], bare });
    assert.deepEqual(await doc.get(["x"]), { n: 1.5, big: 1e300, list: [null, true, "s"], bare: { a: 1 } });
  }));

// A hole in a list is sent as null, as JSON.stringify sends it, and negative
// zero keeps its sign, as it does from Python.
test("a hole in a list is null, and negative zero stays negative", () =>
  withClient(async (client, sidecar) => {
    const sparse = [, 1];
    assert.equal(json(sparse, "x"), "[null,1]");
    assert.equal(json({ a: new Array(2) }, "x"), '{"a":[null,null]}');
    assert.equal(json(-0, "x"), "-0.0");
    assert.equal(json([0, -0], "x"), "[0,-0.0]");
    const doc = client.json("doc");
    await doc.set(["x"], sparse);
    assert.deepEqual(await doc.get(["x"]), [null, 1]);
    await doc.insert(["xs"], 0, { list: new Array(1) });
    assert.deepEqual(await doc.get(["xs"]), [{ list: [null] }]);
    await doc.set(["z"], -0);
    assert.ok(Object.is(await doc.get(["z"]), -0));
    const python = path.join(REPO, "python/.venv/bin/python");
    const script = [
      "import orvu, sys, math",
      "with orvu.connect(sys.argv[1]) as c:",
      "  z = c.json('doc').get(['z'])",
      "  print(type(z).__name__, math.copysign(1, z))",
    ].join("\n");
    assert.equal(execFileSync(python, ["-c", script, sidecar.socket], { timeout: 60_000, encoding: "utf8" }).trim(), "float -1.0");
  }));

// A number in a JSON document is kept as a 64-bit float, which is what a
// JavaScript number is. So Node reads exactly what Python wrote, a large
// fraction included, and every JavaScript number is kept exactly. A whole
// number from Python that a float would round, such as 2**60 + 1, is refused.
test("a number in a json document reads the same from node as from python", () =>
  withClient(async (client, sidecar) => {
    const python = path.join(REPO, "python/.venv/bin/python");
    const script = [
      "import orvu, sys, json",
      "with orvu.connect(sys.argv[1]) as c:",
      "  doc = c.json('ids')",
      "  try:",
      "    doc.set(['big'], 2**60 + 1); refused = False",
      "  except orvu.JsonError:",
      "    refused = True",
      "  doc.set(['edge'], 2**53); doc.set(['float'], 1.5e300); doc.set(['exact'], 2**60)",
      "  print(json.dumps({'refused': refused, 'value': doc.to_value()}))",
    ].join("\n");
    const read = JSON.parse(execFileSync(python, ["-c", script, sidecar.socket], { timeout: 60_000, encoding: "utf8" }));
    assert.equal(read.refused, true);
    assert.deepEqual(await client.json("ids").toValue(), read.value);
    assert.equal(await client.json("ids").get(["float"]), 1.5e300);
    assert.equal(await client.json("ids").get(["edge"]), 2 ** 53);
    assert.equal(await client.json("ids").get(["exact"]), 2 ** 60);
    // JSON.stringify writes 2 ** 60 as 1152921504606847000, which is not
    // the number it is. It is sent as the number it is.
    for (const big of [1e16, 2 ** 60, -(2 ** 60), 2 ** 53 + 2, 1.2345678901234567e19, 2 ** 70, 1e21]) {
      await client.json("ids").set(["big"], big);
      assert.equal(await client.json("ids").get(["big"]), big);
      await client.json("ids").set(["deep"], { list: [big] });
      assert.deepEqual(await client.json("ids").get(["deep"]), { list: [big] });
    }
  }));
