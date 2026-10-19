// The package and the sidecar have to be the same release.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import { test } from "node:test";
import { connect, VERSION, VersionMismatch } from "../src/index.ts";
import { release } from "../src/version.ts";
import { orvudBin, withHome, withSidecar } from "./support.ts";

async function asRelease<T>(version: string, body: () => Promise<T>): Promise<T> {
  release.version = version;
  try {
    return await body();
  } finally {
    release.version = VERSION;
  }
}

test("a sidecar of another release is refused", () =>
  withHome(async () => {
    await asRelease("0.2.0", () =>
      assert.rejects(connect(), (error: Error) => error instanceof VersionMismatch && error.message.includes("0.1.0") && error.message.includes("0.2.0")),
    );
  }));

// The error names the sidecar's process, so the one to stop is plain.
test("the error for a sidecar of another release names its process", () =>
  withSidecar(async (sidecar) => {
    const pid = sidecar.process?.pid;
    assert.ok(pid);
    await asRelease("0.2.0", () =>
      assert.rejects(connect(sidecar.socket), (error: Error) => error instanceof VersionMismatch && error.message.includes(`kill ${pid}`)),
    );
  }));

test("a sidecar that differs only in its last number is accepted", () =>
  withHome(async () => {
    const client = await asRelease("0.1.7", () => connect());
    assert.equal(client.sidecarVersion, "0.1.0");
    client.close();
  }));

test("the package and the sidecar have the same version", () => {
  const said = execFileSync(orvudBin(), ["--version"], { timeout: 60_000, encoding: "utf8" }).trim();
  assert.equal(said, `orvud ${VERSION}`);
  const written = JSON.parse(fs.readFileSync(new URL("../package.json", import.meta.url), "utf8"));
  assert.equal(written.version, VERSION);
});

// The sidecar program goes in `bin/` of the package, so `bin/` has to be one
// of the folders the package is made of. The licence texts go with it.
test("the package is made of what it needs", () => {
  const written = JSON.parse(fs.readFileSync(new URL("../package.json", import.meta.url), "utf8"));
  assert.deepEqual([...written.files].sort(), ["LICENSE", "bin", "dist", "proto"]);
  // The sidecar is also a command of its own, for running it by hand.
  assert.deepEqual(written.bin, { orvud: "bin/orvud" });
});

// The sidecar is replaced by another release while a program is connected.
// Every call after that has to be refused, not only the first.
test("a sidecar replaced by another release stays refused", () =>
  withSidecar(async (sidecar) => {
    const client = await connect(sidecar.socket);
    await client.counter("visits").increment(1);
    await sidecar.restart();
    await asRelease("0.2.0", async () => {
      for (let i = 0; i < 3; i++) {
        await assert.rejects(client.counter("visits").increment(1), VersionMismatch);
      }
    });
    client.close();
    const again = await connect(sidecar.socket);
    assert.equal(await again.counter("visits").value(), 1);
    again.close();
  }));
