// Every operation the sidecar offers is sent by some method, and the
// messages this package reads are the sidecar's own.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";
import { Client, Team } from "../src/index.ts";
import * as handles from "../src/handles.ts";
import type { Op } from "../src/wire.ts";
import { REPO, withClient } from "./support.ts";

const PROTO = fs.readFileSync(path.join(REPO, "typescript/proto/dataplane.proto"), "utf8");

// The package carries a copy of the message definitions, because it is used
// outside this repository. The copy has to be the same as the original.
test("the messages are the sidecar's own", () => {
  assert.equal(PROTO, fs.readFileSync(path.join(REPO, "crates/orvud/proto/orvu/v1/dataplane.proto"), "utf8"));
});

function offered(): string[] {
  const request = PROTO.slice(PROTO.indexOf("message Request {"));
  const ops = request.slice(request.indexOf("oneof op {"), request.indexOf("\n  }"));
  return [...ops.matchAll(/^\s+\w+\s+(\w+)\s*=\s*\d+;/gm)].map((found) => found[1]);
}

test("every operation the sidecar offers is sent by some method", () =>
  withClient(async (client) => {
    const sent = new Set<string>();
    const run = client.run.bind(client);
    client.run = <T>(op: Op<T>) => {
      sent.add(op.field);
      return run(op);
    };
    const calls: Record<string, () => Promise<unknown>> = {
      "Client.declare": () => client.declare("declared", "ap"),
      "Client.search": () => client.search("crew", "refund"),
      "Counter.increment": () => client.counter("c").increment(1),
      "Counter.decrement": () => client.counter("c").decrement(1),
      "Counter.value": () => client.counter("c").value(),
      "Register.set": () => client.register("r").set("x"),
      "Register.get": () => client.register("r").get(),
      "TextSet.add": () => client.set("s").add("x"),
      "TextSet.remove": () => client.set("s").remove("x"),
      "TextSet.contains": () => client.set("s").contains("x"),
      "TextSet.elements": () => client.set("s").elements(),
      "TextMap.put": () => client.map("m").put("k", "v"),
      "TextMap.remove": () => client.map("m").remove("k"),
      "TextMap.get": () => client.map("m").get("k"),
      "TextMap.keys": () => client.map("m").keys(),
      "TextMap.entries": () => client.map("m").entries(),
      "MvMap.put": () => client.mvMap("v").put("k", "v"),
      "MvMap.remove": () => client.mvMap("v").remove("k"),
      "MvMap.get": () => client.mvMap("v").get("k"),
      "MvMap.keys": () => client.mvMap("v").keys(),
      "MvMap.entries": () => client.mvMap("v").entries(),
      "Cart.add": () => client.cart("cart").add("x", 2),
      "Cart.removeQty": () => client.cart("cart").removeQty("x", 1),
      "Cart.removeItem": () => client.cart("cart").removeItem("x"),
      "Cart.quantity": () => client.cart("cart").quantity("x"),
      "Cart.items": () => client.cart("cart").items(),
      "Cart.itemsAsText": () => client.cart("cart").itemsAsText(),
      "TextList.insert": () => client.list("l").insert(0, "x"),
      "TextList.delete": () => client.list("l").delete(0),
      "TextList.get": () => client.list("l").get(0),
      "TextList.length": () => client.list("l").length(),
      "TextList.toArray": () => client.list("l").toArray(),
      "Json.set": () => client.json("j").set(["a"], 1),
      "Json.insert": () => client.json("j").insert(["items"], 0, 1),
      "Json.remove": () => client.json("j").remove(["a"]),
      "Json.get": () => client.json("j").get(["a"]),
      "Json.toValue": () => client.json("j").toValue(),
      "RichText.insert": () => client.richText("p").insert(0, "abc"),
      "RichText.delete": () => client.richText("p").delete(0, 1),
      "RichText.format": () => client.richText("p").format(0, 1, "bold", "true"),
      "RichText.unformat": () => client.richText("p").unformat(0, 1, "bold"),
      "RichText.text": () => client.richText("p").text(),
      "RichText.toSpans": () => client.richText("p").toSpans(),
      "Document.append": () => new Team(client, "crew", "alice").document("d").append("abc"),
      "Tree.moveNode": () => client.tree("t").moveNode("a", "root"),
      "Tree.moveNodeAt": () => client.tree("t").moveNodeAt("b", "root", 0),
      "Tree.setLabel": () => client.tree("t").setLabel("a", "A"),
      "Tree.parent": () => client.tree("t").parent("a"),
      "Tree.children": () => client.tree("t").children("root"),
      "Tree.toNodes": () => client.tree("t").toNodes(),
      "BoundedCounter.increment": () => client.boundedCounter("b").increment(5),
      "BoundedCounter.tryDecrement": () => client.boundedCounter("b").tryDecrement(1),
      "BoundedCounter.transfer": () => client.boundedCounter("b").transfer(999, 1),
      "BoundedCounter.value": () => client.boundedCounter("b").value(),
      "BoundedCounter.allowance": () => client.boundedCounter("b").allowance(),
    };
    for (const call of Object.values(calls)) {
      await call();
    }
    assert.deepEqual([...sent].sort(), offered().sort(), "the sidecar offers operations this package never sends");

    // Every public method of a handle is in the list above.
    const methods = new Set(["Client.declare", "Client.search", "Document.append"]);
    for (const [name, made] of Object.entries(handles)) {
      if (typeof made !== "function" || !/^[A-Z]/.test(name) || name === "Objects") {
        continue;
      }
      for (const method of Object.getOwnPropertyNames(made.prototype)) {
        if (method !== "constructor") {
          methods.add(`${name}.${method}`);
        }
      }
    }
    assert.deepEqual([...methods].sort(), Object.keys(calls).sort());
    assert.ok(Client);
  }));

// Every kind of object the sidecar can watch has a handle that watches it.
test("every kind of object can be watched", () => {
  const block = PROTO.slice(PROTO.indexOf("enum Kind {"));
  const kinds = [...block.slice(0, block.indexOf("}")).matchAll(/^\s+(KIND_\w+)\s*=/gm)]
    .map((found) => found[1])
    .filter((kind) => kind !== "KIND_UNSPECIFIED");
  const watched = new Set<string>();
  for (const [name, made] of Object.entries(handles)) {
    if (typeof made !== "function" || !/^[A-Z]/.test(name) || name === "Objects" || name === "Handle") {
      continue;
    }
    const Made = made as unknown as new (runner: unknown, name: string) => { kind?: string };
    const kind = new Made(null, "x").kind;
    if (kind !== undefined) {
      watched.add(kind);
    }
  }
  assert.deepEqual([...watched].sort(), kinds.sort());
});
