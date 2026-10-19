// Connecting, sessions, and what happens when the sidecar goes away.

import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";
import { BadRequest, connect, SidecarUnavailable, TimedOut, TooLarge } from "../src/index.ts";
import { freePort, holdStill, MIB, orvudBin, REPO, Sidecar, shortFolder, sleep, stopChild, waitUntil, withClient, withSidecar } from "./support.ts";

test("connecting tells you who answered", () =>
  withClient(async (client) => {
    assert.equal(client.sidecarVersion, "0.1.0");
    assert.ok(client.replicaId > 0);
  }));

test("no sidecar is a clear error", async () => {
  await assert.rejects(
    connect("/tmp/nobody-here.sock", { timeoutMs: 500 }),
    (error: Error) => error instanceof SidecarUnavailable && error.message.includes("nobody-here.sock"),
  );
});

test("a namespace the sidecar does not serve is refused", () =>
  withSidecar(async (sidecar) => {
    await assert.rejects(connect(sidecar.socket, { namespace: "someone-else" }), BadRequest);
  }));

// The sidecar forgets its sessions when it restarts. The client must notice,
// open a new one, and carry on, and what was saved must still be there.
test("the client carries on after the sidecar restarts", () =>
  withClient(async (client, sidecar) => {
    const visits = client.counter("visits");
    await visits.increment(5);
    const first = client.session;
    await sidecar.restart();
    assert.equal(await visits.value(), 5);
    await visits.increment(1);
    assert.equal(await visits.value(), 6);
    assert.notEqual(client.session, first);
  }));

test("two clients see each other's changes", () =>
  withSidecar(async (sidecar) => {
    const first = await connect(sidecar.socket);
    const second = await connect(sidecar.socket);
    await first.counter("visits").increment(2);
    await second.counter("visits").increment(3);
    assert.equal(await first.counter("visits").value(), 5);
    assert.equal(await second.counter("visits").value(), 5);
    first.close();
    second.close();
  }));

test("a reply larger than 4 MiB can be read", () =>
  withClient(async (client) => {
    const notes = client.list("notes");
    for (let i = 0; i < 5; i++) {
      await notes.insert(0, "x".repeat(MIB));
    }
    const all = await notes.toArray();
    assert.equal(all.reduce((sum, note) => sum + note.length, 0), 5 * MIB);
  }));

test("a reply over the caller's limit throws TooLarge", () =>
  withClient(async (client, sidecar) => {
    await client.register("big").set("x".repeat(MIB));
    const small = await connect(sidecar.socket, { maxReplyBytes: 1024 });
    await assert.rejects(small.register("big").get(), TooLarge);
    small.close();
  }));

test("a request of up to 64 MiB is taken", () =>
  withClient(async (client) => {
    assert.equal(await client.set("big").contains("x".repeat(60 * MIB)), false);
  }));

test("a request the sidecar will not take throws TooLarge", () =>
  withClient(async (client) => {
    await assert.rejects(client.register("big").set("x".repeat(65 * MIB)), TooLarge);
    assert.equal(await client.register("big").get(), null);
  }));

// Running out of time does not mean nothing happened, so it must not look
// like "the sidecar is not there". Here the sidecar is held still while a
// write is on its way. Once it is let go, the write may go through or not:
// the client stopped waiting and told the sidecar so, and the sidecar may
// hear that before it starts on the write. Either way the write is done at
// most once, and the client goes on working.
test("running out of time has an error of its own", () =>
  withSidecar(async (sidecar) => {
    const client = await connect(sidecar.socket, { timeoutMs: 500 });
    try {
      await holdStill(sidecar.process!);
      await assert.rejects(client.counter("visits").increment(1), (error: Error) => error instanceof TimedOut && !(error instanceof SidecarUnavailable));
    } finally {
      sidecar.process!.kill("SIGCONT");
    }
    // On a busy machine an answer can take longer than half a second, so a
    // read that runs out of time is tried again.
    const visits = async () => {
      let value: number | undefined;
      let last: unknown;
      const read = await waitUntil(async () => {
        try {
          value = await client.counter("visits").value();
          return true;
        } catch (error) {
          if (error instanceof TimedOut) {
            last = error;
            return false;
          }
          throw error;
        }
      }, 30);
      assert.ok(read, `every read ran out of time: ${last}`);
      return value!;
    };
    // Read twice, a moment apart, so that a write done twice shows even if
    // the second time comes late.
    const first = await visits();
    await sleep(500);
    const second = await visits();
    assert.ok([0, 1].includes(first) && [0, 1].includes(second) && second >= first, `${first}, then ${second}`);
    client.close();
  }));

// A sidecar told to stop when idle stops only when nobody is connected. A
// connect that failed must not go on counting as somebody.
test("a failed connect leaves no connection behind", async () => {
  const folder = shortFolder();
  const sidecar = new Sidecar(folder);
  try {
    await sidecar.start("--exit-when-idle", "1");
    await assert.rejects(connect(sidecar.socket, { namespace: "someone-else" }), BadRequest);
    assert.ok(await waitUntil(() => sidecar.process!.exitCode !== null, 8), "the sidecar still has a connection open");
  } finally {
    await sidecar.stop();
    fs.rmSync(folder, { recursive: true, force: true });
  }
});

test("a client can be closed with await using", () =>
  withSidecar(async (sidecar) => {
    {
      await using client = await connect(sidecar.socket);
      await client.counter("visits").increment(1);
    }
    const again = await connect(sidecar.socket);
    assert.equal(await again.counter("visits").value(), 1);
    again.close();
  }));

// Many calls at once find the sidecar restarted. They open one new session
// between them, and every one of them goes through.
test("calls at the same moment after a restart all go through", () =>
  withClient(async (client, sidecar) => {
    await client.counter("visits").increment(1);
    await sidecar.restart();
    await Promise.all(Array.from({ length: 10 }, () => client.counter("visits").increment(1)));
    assert.equal(await client.counter("visits").value(), 11);
  }));

test("a closed client says so", () =>
  withClient(async (client) => {
    await client.counter("visits").increment(1);
    client.close();
    await assert.rejects(client.counter("visits").value(), (error: Error) => error instanceof BadRequest && /closed/.test(error.message));
    await assert.rejects(
      (async () => {
        for await (const _ of client.counter("visits").watch()) {
          break;
        }
      })(),
      BadRequest,
    );
  }));

// A machine of a team can be given a number far larger than a JavaScript
// number holds exactly. Its number is read exactly, and allowance can be
// handed to it. 2^63 - 1 is the largest a settings file holds. `orvud setup`
// gives no number past 2^53 - 1, so the settings and the certificate for
// this one are made by hand.
test("a machine number of any size is read exactly", async () => {
  const top = shortFolder();
  const big = 9223372036854775807n;
  try {
    execFileSync(orvudBin(), ["setup", path.join(top, "crew"), `alpha=127.0.0.1:${await freePort()}#3`, `beta=127.0.0.1:${await freePort()}#2`], {
      timeout: 60_000,
    });
    const alpha = path.join(top, "crew/alpha");
    const keys = path.join(top, "keys");
    execFileSync(path.join(REPO, "target/debug/examples/keygen"), ["--out", keys, "--replicas", `${big}`], { timeout: 60_000 });
    fs.copyFileSync(path.join(keys, "ca.pem"), path.join(alpha, "ca.pem"));
    fs.copyFileSync(path.join(keys, `node${big}.pem`), path.join(alpha, "node.pem"));
    fs.copyFileSync(path.join(keys, `node${big}-key.pem`), path.join(alpha, "node-key.pem"));
    const settings = path.join(alpha, "orvu.toml");
    fs.writeFileSync(settings, fs.readFileSync(settings, "utf8").replace(/^replica = 3$/m, `replica = ${big}`));
    const output = fs.openSync(path.join(top, "output"), "w");
    const child = spawn(orvudBin(), ["--config", path.join(alpha, "orvu.toml"), "--socket", path.join(top, "s")], { stdio: ["ignore", "ignore", output] });
    fs.closeSync(output);
    try {
      assert.ok(await waitUntil(() => fs.readFileSync(path.join(top, "output"), "utf8").includes("serving on")));
      const client = await connect(path.join(top, "s"));
      assert.equal(client.replicaId, big);
      const budget = client.boundedCounter("budget");
      await budget.increment(5);
      assert.equal(await budget.transfer(client.replicaId, 2), true);
      assert.equal(await budget.transfer(2n, 1), true);
      assert.equal(await budget.allowance(), 4);
      client.close();
    } finally {
      await stopChild(child);
    }
  } finally {
    fs.rmSync(top, { recursive: true, force: true });
  }
});
