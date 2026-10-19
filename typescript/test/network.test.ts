// Node agents on other machines, or in containers, reach a sidecar over the
// network with a connection file. Every connection is encrypted, and only
// agents with keys are let in.

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";
import { connect, NotAllowed, SidecarUnavailable, Team } from "../src/index.ts";
import { freePort, python, Sidecar, shortFolder, withHome } from "./support.ts";

/** Runs `body` with a sidecar that serves the network, and a connection file
 * for alice, an agent it lets in. */
async function withNetworked(body: (sidecar: Sidecar, connection: string, folder: string) => Promise<void>): Promise<void> {
  const folder = shortFolder();
  const sidecar = new Sidecar(folder);
  try {
    const address = `127.0.0.1:${await freePort()}`;
    const connection = path.join(folder, "alice.orvu");
    sidecar.addNetworkedAgent("alice", address, connection);
    await sidecar.start("--network", address);
    await body(sidecar, connection, folder);
  } finally {
    await sidecar.stop();
    fs.rmSync(folder, { recursive: true, force: true });
  }
}

test("an agent connects over the network with its connection file", () =>
  withNetworked(async (_sidecar, connection) => {
    const alice = await Team.connect("default", { connection });
    assert.equal(alice.me, "alice");
    await alice.notes.put("deploy", "Friday");
    assert.equal(await alice.notes.get("deploy"), "Friday");
    await alice.close();
  }));

// It runs in a home folder of its own. A client that failed to read the
// connection file would otherwise start this user's own sidecar, in the real
// home folder.
test("the connection file can come from the environment", () =>
  withNetworked((_sidecar, connection) =>
    withHome(async () => {
      process.env.ORVU_CONNECT = connection;
      try {
        const client = await connect();
        assert.equal(client.agent, "alice");
        client.close();
        // An empty key is no key, and does not clash with the file's.
        const again = await connect(undefined, { key: "" });
        assert.equal(again.agent, "alice");
        again.close();
      } finally {
        delete process.env.ORVU_CONNECT;
      }
    }),
  ));

// A program that trusts another certificate does not connect, and a key the
// sidecar does not know is refused.
test("another certificate is not trusted, and an unknown key is refused", () =>
  withNetworked(async (sidecar, connection, folder) => {
    const kept = JSON.parse(fs.readFileSync(connection, "utf8"));
    const other = new Sidecar(path.join(folder, "other"));
    fs.mkdirSync(other.folder);
    other.addNetworkedAgent("x", kept.address, path.join(folder, "other.orvu"));
    const theirs = JSON.parse(fs.readFileSync(path.join(folder, "other.orvu"), "utf8")).certificate;
    const changed = path.join(folder, "changed.orvu");
    fs.writeFileSync(changed, JSON.stringify({ ...kept, certificate: theirs }));
    await assert.rejects(
      connect(undefined, { connection: changed, timeoutMs: 3000 }),
      (error: Error) => error instanceof SidecarUnavailable && /certificate/.test(error.message),
    );
    fs.writeFileSync(changed, JSON.stringify({ ...kept, key: "orvu-key-0123" }));
    await assert.rejects(connect(undefined, { connection: changed }), NotAllowed);
    assert.ok(sidecar.process);
  }));

// A Node agent and a Python agent, both over the network, share one team.
test("Node and Python agents meet over the network", () =>
  withNetworked(async (sidecar, connection, folder) => {
    const pia = path.join(folder, "pia.orvu");
    sidecar.addNetworkedAgent("pia", JSON.parse(fs.readFileSync(connection, "utf8")).address, pia);
    const alice = await Team.connect("default", { connection });
    await alice.notes.put("deploy", "Friday");
    const seen = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(connection=${JSON.stringify(pia)}) as team:
    print(team.me, team.notes.get("deploy"))`,
    ).trim();
    assert.equal(seen, "pia Friday");
    await alice.close();
  }));
