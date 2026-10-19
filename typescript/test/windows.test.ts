// The package on Windows, as near as this machine can come to it: Node told
// it is on Windows. An agent there connects to a sidecar elsewhere over the
// network, and is told so if it asks for one of its own.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";
import { freePort, REPO, Sidecar, shortFolder, withHome } from "./support.ts";

/** Runs `program` in a Node that says it is on Windows, and gives what it
 * printed. `orvu` there is this repository's copy of the library. */
function likeWindows(program: string): string {
  const preamble = `Object.defineProperty(process, "platform", { value: "win32" });
const orvu = await import(${JSON.stringify(path.join(REPO, "typescript/src/index.ts"))});
`;
  return execFileSync(process.execPath, ["--input-type=module", "-e", preamble + program], {
    encoding: "utf8",
    timeout: 60_000,
  });
}

test("an agent on Windows connects over the network", () =>
  withHome(async () => {
    const folder = shortFolder();
    const sidecar = new Sidecar(folder);
    try {
      const address = `127.0.0.1:${await freePort()}`;
      const connection = path.join(folder, "alice.orvu");
      sidecar.addNetworkedAgent("alice", address, connection);
      await sidecar.start("--network", address);
      const printed = likeWindows(`
const team = await orvu.Team.connect("default", { connection: ${JSON.stringify(connection)} });
await team.notes.put("from", "windows");
console.log(team.me, await team.notes.get("from"));
await team.close();
`);
      assert.equal(printed, "alice windows\n");
    } finally {
      await sidecar.stop();
      fs.rmSync(folder, { recursive: true, force: true });
    }
  }));

// A program that asks for a sidecar of its own is told to connect to one
// elsewhere, not that something is broken.
test("an agent on Windows is told to connect over the network", () =>
  withHome(async () => {
    const printed = likeWindows(`
try {
  await orvu.connect();
} catch (error) {
  console.log(error instanceof orvu.SidecarUnavailable, error.message);
}
`);
    assert.match(printed, /^true there is no sidecar for Windows yet/);
    assert.match(printed, /ORVU_CONNECT/);
  }));
