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

// The sidecar program is not in this package: it comes in a package of its
// own for each kind of machine. This one is the code, the message
// definitions, the licence, and the `orvud` command that finds and runs the
// program.
test("the package is made of what it needs", () => {
  const written = JSON.parse(fs.readFileSync(new URL("../package.json", import.meta.url), "utf8"));
  assert.deepEqual([...written.files].sort(), ["LICENSE", "dist", "proto"]);
  assert.deepEqual(written.bin, { orvud: "dist/orvud.js" });
  // It says nothing of which machines it is for: it is for all of them.
  assert.equal(written.os, undefined);
  assert.equal(written.cpu, undefined);
  // The sidecar packages are named only in the package that is published,
  // by the script that builds it, so that installing this folder for work on
  // it never asks for them.
  assert.equal(written.optionalDependencies, undefined);
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
