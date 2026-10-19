// The `orvud` command that comes with the package. It finds the program the
// way the library does and runs it, as if it had been run itself.

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const LAUNCHER = fileURLToPath(new URL("../src/orvud.ts", import.meta.url));

/** A stand-in for orvud, a shell script, in a folder of its own. */
function fake(script: string): { bin: string; done: () => void } {
  const folder = fs.mkdtempSync(path.join(os.tmpdir(), "orvu-launcher-"));
  const bin = path.join(folder, "orvud");
  fs.writeFileSync(bin, `#!/bin/sh\n${script}\n`, { mode: 0o755 });
  return { bin, done: () => fs.rmSync(folder, { recursive: true, force: true }) };
}

test("the command starts with a line that says to run it with node", () => {
  assert.match(fs.readFileSync(LAUNCHER, "utf8"), /^#!\/usr\/bin\/env node\n/);
});

test("the command runs the program with its arguments and gives back its exit code", () => {
  const { bin, done } = fake('for word in "$@"; do echo "[$word]"; done\nexit 3');
  try {
    const ran = spawnSync(process.execPath, [LAUNCHER, "setup", "two words", "--x"], {
      encoding: "utf8",
      env: { PATH: "", ORVUD_BIN: bin },
      timeout: 30_000,
    });
    assert.equal(ran.stdout, "[setup]\n[two words]\n[--x]\n");
    assert.equal(ran.status, 3);
  } finally {
    done();
  }
});

test("a signal sent to the command reaches the program, and the command ends the same way", async () => {
  // The stand-in gives up by itself after ten seconds, so that a signal that
  // never arrives fails the test instead of leaving it running.
  const { bin, done } = fake('trap \'echo "got TERM"; exit 7\' TERM\necho ready\ni=0\nwhile [ $i -lt 200 ]; do sleep 0.05; i=$((i + 1)); done\nexit 0');
  try {
    const child = spawn(process.execPath, [LAUNCHER], { env: { PATH: "/usr/bin:/bin", ORVUD_BIN: bin } });
    let out = "";
    child.stdout.setEncoding("utf8");
    await new Promise<void>((resolve, reject) => {
      child.stdout.on("data", (chunk: string) => {
        out += chunk;
        if (out.includes("ready")) resolve();
      });
      child.once("exit", () => reject(new Error(`the command ended before the program was ready: ${out}`)));
    });
    child.kill("SIGTERM");
    const [code] = await new Promise<[number | null, string | null]>((resolve) => child.on("close", (...ended) => resolve(ended)));
    assert.match(out, /got TERM/);
    assert.equal(code, 7);
  } finally {
    done();
  }
});

test("a program killed by a signal makes the command end by the same signal", async () => {
  const { bin, done } = fake("echo ready\nkill -KILL $$");
  try {
    const child = spawn(process.execPath, [LAUNCHER], { env: { PATH: "/usr/bin:/bin", ORVUD_BIN: bin } });
    const [code, signal] = await new Promise<[number | null, string | null]>((resolve) => child.on("close", (...ended) => resolve(ended)));
    assert.equal(code, null);
    assert.equal(signal, "SIGKILL");
  } finally {
    done();
  }
});

test("with no program to run, the command says what to do and fails", () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "orvu-launcher-"));
  try {
    const ran = spawnSync(process.execPath, [LAUNCHER, "--version"], { encoding: "utf8", env: { PATH: home, HOME: home }, timeout: 30_000 });
    assert.notEqual(ran.status, 0);
    assert.equal(ran.stdout, "");
    assert.match(ran.stderr, /no orvud program/);
    assert.match(ran.stderr, /\$ORVUD_BIN/);
    assert.doesNotMatch(ran.stderr, /at .*\.ts:\d+/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

// Another copy of this command, first on the PATH, would be found as the
// program and would find the next copy in turn. The stand-in here counts how
// deep it has gone and gives up at three, so that the test cannot run away.
test("a copy of the command on the PATH is not run in a loop", () => {
  const folder = fs.mkdtempSync(path.join(os.tmpdir(), "orvu-launcher-"));
  try {
    fs.writeFileSync(
      path.join(folder, "orvud"),
      `#!/bin/sh\nn=\${DEPTH:-0}\nif [ "$n" -ge 3 ]; then echo looped >&2; exit 9; fi\nDEPTH=$((n + 1)) exec "${process.execPath}" "${LAUNCHER}" "$@"\n`,
      { mode: 0o755 },
    );
    const ran = spawnSync(process.execPath, [LAUNCHER, "--version"], { encoding: "utf8", env: { PATH: folder, HOME: folder }, timeout: 30_000 });
    assert.notEqual(ran.status, 0);
    assert.doesNotMatch(ran.stderr, /looped/);
    assert.match(ran.stderr, /another copy of this command/);
  } finally {
    fs.rmSync(folder, { recursive: true, force: true });
  }
});
