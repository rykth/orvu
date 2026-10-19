// Where the package looks for the orvud program, and in which order. The
// program comes in a package of its own for each kind of machine, such as
// @orvuhq/orvud-linux-x64, which npm installs beside this one.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { test } from "node:test";
import { SidecarUnavailable } from "../src/index.ts";
import { program, sidecarPackage } from "../src/sidecar.ts";

/** A folder that looks like a project with packages installed in it, and a
 * file inside it standing for this package's code, to look from. */
function project(): { root: string; from: string; done: () => void } {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "orvu-finding-")));
  const from = path.join(root, "node_modules", "orvu", "dist", "sidecar.js");
  fs.mkdirSync(path.dirname(from), { recursive: true });
  return { root, from, done: () => fs.rmSync(root, { recursive: true, force: true }) };
}

/** Puts a sidecar package into the project, the way npm would. */
function installSidecar(root: string, name: string): string {
  const folder = path.join(root, "node_modules", ...name.split("/"));
  fs.mkdirSync(path.join(folder, "bin"), { recursive: true });
  fs.writeFileSync(path.join(folder, "package.json"), JSON.stringify({ name, version: "0.1.0" }));
  const bin = path.join(folder, "bin", "orvud");
  fs.writeFileSync(bin, "#!/bin/sh\n", { mode: 0o755 });
  return bin;
}

function onPath(root: string): string {
  const folder = path.join(root, "on-path");
  fs.mkdirSync(folder);
  fs.writeFileSync(path.join(folder, "orvud"), "#!/bin/sh\n", { mode: 0o755 });
  return folder;
}

const linux = { platform: "linux", arch: "x64" } as const;

test("each kind of machine with a sidecar has a package of its own", () => {
  assert.equal(sidecarPackage("linux", "x64"), "@orvuhq/orvud-linux-x64");
  assert.equal(sidecarPackage("linux", "arm64"), "@orvuhq/orvud-linux-arm64");
  assert.equal(sidecarPackage("darwin", "arm64"), "@orvuhq/orvud-darwin-arm64");
  assert.equal(sidecarPackage("darwin", "x64"), "@orvuhq/orvud-darwin-x64");
  assert.equal(sidecarPackage("win32", "x64"), null);
  assert.equal(sidecarPackage("linux", "ia32"), null);
  assert.equal(sidecarPackage("freebsd", "x64"), null);
});

test("the package for this kind of machine is found", () => {
  const { root, from, done } = project();
  try {
    const bin = installSidecar(root, "@orvuhq/orvud-linux-x64");
    assert.equal(program({ PATH: onPath(root) }, { from, ...linux }), bin);
  } finally {
    done();
  }
});

test("$ORVUD_BIN comes before the package", () => {
  const { root, from, done } = project();
  try {
    installSidecar(root, "@orvuhq/orvud-linux-x64");
    const named = path.join(onPath(root), "orvud");
    assert.equal(program({ ORVUD_BIN: named, PATH: "" }, { from, ...linux }), named);
  } finally {
    done();
  }
});

test("a package for another kind of machine is not used", () => {
  const { root, from, done } = project();
  try {
    installSidecar(root, "@orvuhq/orvud-linux-arm64");
    installSidecar(root, "@orvuhq/orvud-darwin-x64");
    const folder = onPath(root);
    assert.equal(program({ PATH: folder }, { from, ...linux }), path.join(folder, "orvud"));
  } finally {
    done();
  }
});

test("a package whose program cannot be run is passed over", () => {
  const { root, from, done } = project();
  try {
    fs.chmodSync(installSidecar(root, "@orvuhq/orvud-linux-x64"), 0o644);
    const folder = onPath(root);
    assert.equal(program({ PATH: folder }, { from, ...linux }), path.join(folder, "orvud"));
  } finally {
    done();
  }
});

test("with nothing to find, it says which package to install", () => {
  const { from, done } = project();
  try {
    assert.throws(
      () => program({ PATH: "" }, { from, ...linux }),
      (error: Error) =>
        error instanceof SidecarUnavailable &&
        /no orvud program/.test(error.message) &&
        error.message.includes("@orvuhq/orvud-linux-x64") &&
        error.message.includes("$ORVUD_BIN"),
    );
  } finally {
    done();
  }
});

test("on a machine with no sidecar package, it says to connect over the network", () => {
  const { from, done } = project();
  try {
    assert.throws(
      () => program({ PATH: "" }, { from, platform: "win32", arch: "x64" }),
      (error: Error) =>
        error instanceof SidecarUnavailable &&
        /no orvud program/.test(error.message) &&
        error.message.includes("ORVU_CONNECT") &&
        !error.message.includes("@orvuhq"),
    );
  } finally {
    done();
  }
});
