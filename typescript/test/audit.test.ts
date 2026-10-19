// The record of who changed what, as a Node agent's changes reach it. The
// sidecar keeps it; `orvud audit` shows it.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { test } from "node:test";
import { Team } from "../src/index.ts";
import { orvudBin, waitUntil, withSidecar } from "./support.ts";

test("a Node agent's changes are recorded by name and not by what they say", () =>
  withSidecar(async (sidecar) => {
    const team = await Team.connect("default", { socket: sidecar.socket, key: sidecar.addAgent("nora") });
    await team.notes.put("deploy", "the launch code");
    await team.close();
    const audit = () => execFileSync(orvudBin(), ["audit", "--local", "--data-dir", sidecar.data, "--agent", "nora"], { encoding: "utf8" });
    assert.ok(await waitUntil(() => audit().includes("key=deploy")));
    assert.match(audit(), /nora {2}mv_map_put {2}orvu\/default\/notes-mv {2}key=deploy {2}done/);
    assert.doesNotMatch(audit(), /launch code/);
  }));
