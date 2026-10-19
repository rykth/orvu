// Live updates, in a `for await` loop.

import * as grpc from "@grpc/grpc-js";
import * as loader from "@grpc/proto-loader";
import assert from "node:assert/strict";
import * as path from "node:path";
import { test } from "node:test";
import { connect, EngineError, TypeMismatch } from "../src/index.ts";
import type { Client } from "../src/index.ts";
import { REPO, sleep, waitUntil, withClient } from "./support.ts";

async function first<T>(values: AsyncIterable<T>): Promise<T> {
  for await (const value of values) {
    return value;
  }
  throw new Error("the loop ended with no value");
}

test("watch gives the value now and then each change", () =>
  withClient(async (client, sidecar) => {
    const visits = client.counter("visits");
    await visits.increment(1);
    const watcher = await connect(sidecar.socket);
    const seen: number[] = [];
    const follow = (async () => {
      for await (const value of watcher.counter("visits").watch()) {
        seen.push(value);
        if (value === 6) {
          return;
        }
      }
    })();
    // Wait for the first value, so the changes below happen while watching.
    assert.ok(await waitUntil(() => seen.length > 0));
    await visits.increment(2);
    await visits.increment(3);
    await follow;
    watcher.close();
    assert.equal(seen[0], 1);
    assert.equal(seen.at(-1), 6);
    assert.deepEqual(seen, [...seen].sort((a, b) => a - b));
  }));

test("watch gives each type its own shape", () =>
  withClient(async (client) => {
    await client.register("title").set("Plan");
    await client.set("tags").add("red");
    await client.map("profile").put("name", "alice");
    await client.mvMap("plans").put("launch", "friday");
    await client.cart("basket").add("apple", 2);
    await client.list("notes").insert(0, "x");
    await client.json("doc").set(["done"], true);
    await client.richText("page").insert(0, "hi");
    await client.tree("outline").moveNode("a", "root");
    await client.boundedCounter("budget").increment(7);

    assert.equal(await first(client.register("title").watch()), "Plan");
    assert.deepEqual(await first(client.set("tags").watch()), new Set(["red"]));
    assert.deepEqual(await first(client.map("profile").watch()), { name: "alice" });
    assert.deepEqual(await first(client.mvMap("plans").watch()), { launch: ["friday"] });
    assert.deepEqual(await first(client.cart("basket").watch()), { apple: 2 });
    assert.deepEqual(await first(client.list("notes").watch()), ["x"]);
    assert.deepEqual(await first(client.json("doc").watch()), { done: true });
    assert.deepEqual(await first(client.richText("page").watch()), [{ text: "hi", marks: {} }]);
    assert.deepEqual(new Set((await first(client.tree("outline").watch())).map((node) => node.id)), new Set(["a", "root"]));
    assert.equal(await first(client.boundedCounter("budget").watch()), 7);
  }));

test("watching the wrong type throws", () =>
  withClient(async (client) => {
    await client.counter("thing").increment(1);
    await assert.rejects(first(client.list("thing").watch()), TypeMismatch);
  }));

test("watch waits for an object that does not exist yet", () =>
  withClient(async (client, sidecar) => {
    const watcher = await connect(sidecar.socket);
    const seen: (string | null)[] = [];
    const follow = (async () => {
      for await (const value of watcher.register("later").watch()) {
        seen.push(value);
        return;
      }
    })();
    // Give the loop time to start watching before anything is written, so it
    // really is watching a name that nothing has been written to.
    await sleep(500);
    assert.deepEqual(seen, []);
    // Watching did not take the name for a register: it can still be read.
    assert.equal(await client.register("later").get(), null);
    await client.register("later").set("here");
    await follow;
    watcher.close();
    assert.deepEqual(seen, ["here"]);
  }));

// The sidecar forgets its sessions when it restarts. A loop started after
// that must open a new session by itself, as an ordinary call does.
test("a watch started after a restart works", () =>
  withClient(async (client, sidecar) => {
    await client.counter("visits").increment(5);
    await sidecar.restart();
    assert.equal(await first(client.counter("visits").watch()), 5);
  }));

test("close ends a watch loop", () =>
  withClient(async (client, sidecar) => {
    const watcher: Client = await connect(sidecar.socket);
    await client.counter("visits").increment(1);
    const seen: number[] = [];
    const follow = (async () => {
      for await (const value of watcher.counter("visits").watch()) {
        seen.push(value);
      }
    })();
    assert.ok(await waitUntil(() => seen.length > 0));
    watcher.close();
    await follow;
    assert.deepEqual(seen, [1]);
  }));

// Leaving a loop early stops the watch on the sidecar too.
test("leaving a loop stops the watch", () =>
  withClient(async (client) => {
    await client.counter("visits").increment(1);
    for (let i = 0; i < 20; i++) {
      assert.equal(await first(client.counter("visits").watch()), 1);
    }
    await client.counter("visits").increment(1);
    assert.equal(await client.counter("visits").value(), 2);
  }));

// ---- a loop that only watches, on a busy sidecar ----------------------------

// The sidecar keeps only so many sessions. When they are all taken, the one
// used longest ago is dropped and its watch ends. A loop that only watches has to carry on however often that happens,
// opening a new session each time, as long as each new session lasts a while.
// One that is dropped again soon after has no place to keep, and gives up.
const SESSIONS = 1024;

const DataPlane = (
  grpc.loadPackageDefinition(
    loader.loadSync(path.join(REPO, "typescript/proto/dataplane.proto"), { keepCase: true, enums: String, oneofs: true }),
  ) as any
).orvu.dataplane.v1.DataPlane as grpc.ServiceClientConstructor;

/** Many other programs, each with a session and a watch open. */
class Crowd {
  readonly #stub: any;
  readonly #calls: grpc.ClientReadableStream<unknown>[] = [];

  constructor(socket: string) {
    this.#stub = new DataPlane(`unix:${socket}`, grpc.credentials.createInsecure(), { "grpc.default_authority": "localhost" });
  }

  async arrive(count: number): Promise<void> {
    for (let i = 0; i < count; i++) {
      const session = await new Promise<string>((resolve, reject) =>
        this.#stub.Open({}, (error: Error | null, opened: { session: string }) => (error ? reject(error) : resolve(opened.session))),
      );
      const call: grpc.ClientReadableStream<unknown> = this.#stub.Watch({ session, object: "visits", kind: "KIND_COUNTER" });
      // The first value says the watch is open.
      await new Promise((resolve, reject) => {
        call.once("data", resolve);
        call.once("error", reject);
      });
      // Its session may be dropped in turn later, which ends it.
      call.on("error", () => {});
      this.#calls.push(call);
    }
  }

  close(): void {
    this.#calls.forEach((call) => call.cancel());
    this.#stub.close();
  }
}

test("a watch-only loop carries on however often its session is dropped", { timeout: 120_000 }, () =>
  withClient(async (client, sidecar) => {
    await client.counter("visits").increment(1);
    const watcher = await connect(sidecar.socket);
    // A while is ten seconds; the test waits out a shorter one.
    watcher.settledAfterMs = 500;
    const seen: number[] = [];
    let failed: unknown = null;
    const follow = (async () => {
      for await (const value of watcher.counter("visits").watch()) {
        seen.push(value);
        if (value === 2) {
          return;
        }
      }
    })().catch((error) => {
      failed = error;
    });
    assert.ok(await waitUntil(() => seen.length > 0));
    const crowd = new Crowd(sidecar.socket);
    try {
      for (let turn = 1; turn <= 2; turn++) {
        await sleep(600);
        const before = watcher.session;
        await crowd.arrive(SESSIONS);
        assert.ok(await waitUntil(() => failed !== null || watcher.session !== before, 10), "the session was not dropped");
        assert.equal(failed, null, `the loop ended time ${turn} its session was dropped`);
      }
      await client.counter("visits").increment(1);
      await follow;
      assert.equal(failed, null);
      assert.equal(seen.at(-1), 2);
    } finally {
      crowd.close();
      watcher.close();
    }
  }));

// A watch is sent its value as soon as it opens, so that value does not count
// as the new session having lasted. Nor does the wall clock jumping ahead,
// as it does when the system clock is set: the while is timed on a clock
// that only counts time going by.
test("a watch-only loop gives up when its new session is dropped at once", { timeout: 120_000 }, () =>
  withClient(async (client, sidecar) => {
    await client.counter("visits").increment(1);
    const watcher = await connect(sidecar.socket);
    // Opening a thousand sessions can take longer than ten seconds on a busy
    // machine, which would count as the new session having lasted.
    watcher.settledAfterMs = 600_000;
    const seen: number[] = [];
    let failed: unknown = null;
    const follow = (async () => {
      for await (const value of watcher.counter("visits").watch()) {
        seen.push(value);
      }
    })().catch((error) => {
      failed = error;
    });
    assert.ok(await waitUntil(() => seen.length > 0));
    const crowd = new Crowd(sidecar.socket);
    try {
      const before = watcher.session;
      await crowd.arrive(SESSIONS);
      assert.ok(await waitUntil(() => failed !== null || watcher.session !== before, 10), "the session was not dropped");
      assert.equal(failed, null);
      // The value is sent again on the new session.
      assert.ok(await waitUntil(() => seen.length >= 2), `${seen}`);
      // The wall clock jumps a day ahead.
      const realNow = Date.now;
      Date.now = () => realNow() + 86_400_000;
      try {
        await crowd.arrive(SESSIONS);
        assert.ok(await waitUntil(() => failed !== null, 10), "the loop went on after being dropped again at once");
      } finally {
        Date.now = realNow;
      }
      assert.ok((failed as unknown) instanceof EngineError, `${failed}`);
      assert.match((failed as unknown as Error).message, /UNAUTHENTICATED/);
      await follow;
    } finally {
      crowd.close();
      watcher.close();
    }
  }));
