// The roster: who is on the team, and who is there now.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import * as path from "node:path";
import { test } from "node:test";
import { BadRequest, OrvuError, type Team } from "../src/index.ts";
import { REPO, sleep, teamOn, waitUntil, withTeam } from "./support.ts";

test("an agent joins and leaves", () =>
  withTeam(async (team, bob) => {
    assert.deepEqual(await team.roster.members(), []);
    assert.deepEqual(await team.roster.here(), []);
    await team.roster.join();
    await bob.roster.join();
    assert.deepEqual(await team.roster.here(), ["alice", "bob"]);
    assert.deepEqual((await bob.roster.members()).map((member) => member.name), ["alice", "bob"]);
    assert.equal((await team.index()).roster, "map");
    await bob.roster.leave();
    assert.deepEqual(await team.roster.here(), ["alice"]);
    await bob.roster.leave(); // leaving twice does nothing
  }));

/** A Node program that joins the roster as `me`, beating every `every`
 * seconds, and then waits to be killed. */
function agentProgram(socket: string, me: string, every: number) {
  const program = `
    import { Team } from ${JSON.stringify(path.join(REPO, "typescript/src/index.ts"))};
    const team = await Team.connect("default", { socket: ${JSON.stringify(socket)}, me: ${JSON.stringify(me)} });
    await team.roster.join(${every});
    console.log("joined");
    setInterval(() => {}, 1000);
  `;
  const child = spawn(process.execPath, ["--input-type=module", "-e", program], { stdio: ["ignore", "pipe", "inherit"] });
  // A program that ends before it has joined will not join.
  const joined = new Promise((resolve, reject) => {
    child.stdout!.once("data", resolve);
    child.once("exit", (code) => reject(new Error(`the agent program ended with ${code} before it joined`)));
  });
  return { child, joined };
}

// An agent that crashed says nothing more. It stays on the roster and stops
// counting as here. Here the agent is a program of its own, killed outright.
test("an agent that went silent fades out", () =>
  withTeam(async (team, _bob, sidecar) => {
    await team.roster.join(0.2);
    const { child, joined } = agentProgram(sidecar.socket, "bob", 0.2);
    try {
      await joined;
      assert.deepEqual(await team.roster.here(), ["alice", "bob"]);
    } finally {
      child.kill("SIGKILL");
    }
    assert.ok(await waitUntil(async () => (await team.roster.here()).join() === "alice"));
    const names = Object.fromEntries((await team.roster.members()).map((member) => [member.name, member.here]));
    assert.deepEqual(names, { alice: true, bob: false });
  }));

test("an agent that joined keeps saying it is here", () =>
  withTeam(async (team, bob) => {
    await team.roster.join(0.1);
    const first = (await bob.roster.members())[0].lastSeen;
    assert.ok(await waitUntil(async () => (await bob.roster.members())[0].lastSeen > first));
  }));

test("closing the team takes the agent off the roster", () =>
  withTeam(async (team, _bob, sidecar) => {
    const bob = await teamOn(sidecar, "bob");
    await bob.roster.join();
    assert.deepEqual(await team.roster.here(), ["bob"]);
    await bob.close();
    assert.deepEqual(await team.roster.members(), []);
  }));

test("joining needs a name and a sensible time", () =>
  withTeam(async (team, _bob, sidecar) => {
    const nameless = await teamOn(sidecar);
    await assert.rejects(nameless.roster.join(), (error: Error) => error instanceof BadRequest && /needs a name/.test(error.message));
    await nameless.close();
    for (const every of [0, -1, "soon", true, NaN]) {
      await assert.rejects(team.roster.join(every as number), (error: Error) => error instanceof BadRequest && /seconds above zero/.test(error.message));
    }
    assert.deepEqual(await team.roster.members(), []);
    assert.deepEqual(await team.index(), {});
  }));

// Joining again starts the one heartbeat afresh, not a second one beside
// it. Two would leave two entries for one program.
test("joining again does not start a second heartbeat", () =>
  withTeam(async (team) => {
    await team.roster.join(0.05);
    await team.roster.join(0.05);
    await team.roster.join(0.05);
    await sleep(300);
    assert.equal(Object.keys(await team.client.map("orvu/default/roster").entries()).length, 1);
    await team.roster.leave();
    await sleep(200);
    assert.deepEqual(await team.client.map("orvu/default/roster").entries(), {});
  }));

// A "still here" that was on its way when the agent left must not put the
// agent back on the roster after it was taken off.
test("leaving while a heartbeat is on its way leaves nothing behind", () =>
  withTeam(async (team) => {
    for (let i = 0; i < 20; i++) {
      await team.roster.join(0.001);
      await sleep(5);
      await team.roster.leave();
    }
    await sleep(200);
    assert.deepEqual(await team.client.map("orvu/default/roster").entries(), {});
  }));

// A leave asked for while a join is still on its way waits for the join,
// and then undoes it. Nothing is left on the roster, and no heartbeat goes on.
test("a leave asked for while a join is on its way undoes it", () =>
  withTeam(async (team, bob) => {
    const joining = team.roster.join(0.05);
    const leaving = team.roster.leave();
    await Promise.all([joining, leaving]);
    await sleep(300);
    assert.deepEqual(await bob.roster.members(), []);
  }));

// Two joins at the same moment leave one entry, which one leave takes off.
test("two joins at once leave one entry", () =>
  withTeam(async (team) => {
    await Promise.all([team.roster.join(0.05), team.roster.join(0.05)]);
    await sleep(200);
    assert.equal(Object.keys(await team.client.map("orvu/default/roster").entries()).length, 1);
    await team.roster.leave();
    await sleep(200);
    assert.deepEqual(await team.client.map("orvu/default/roster").entries(), {});
  }));

// A heartbeat a month apart is still a heartbeat a month apart. A timer
// that long would otherwise be taken by Node to mean "at once".
test("a very slow heartbeat does not beat fast", () =>
  withTeam(async (team) => {
    await team.roster.join(3_000_000);
    const first = await team.client.map("orvu/default/roster").entries();
    await sleep(300);
    assert.deepEqual(await team.client.map("orvu/default/roster").entries(), first);
    assert.equal((await team.roster.members())[0].here, true);
  }));

// The heartbeat does not keep a program running. A program that is done
// ends, though it never left the roster.
test("the heartbeat does not keep a program running", () =>
  withTeam(async (_team, _bob, sidecar) => {
    const program = `
      import { Team } from ${JSON.stringify(path.join(REPO, "typescript/src/index.ts"))};
      const team = await Team.connect("default", { socket: ${JSON.stringify(sidecar.socket)}, me: "short" });
      await team.roster.join(0.1);
      team.client.close();
    `;
    const child = spawn(process.execPath, ["--input-type=module", "-e", program], { stdio: "inherit" });
    const ended = await Promise.race([new Promise((resolve) => child.once("exit", resolve)), sleep(5000).then(() => "still running")]);
    if (ended === "still running") {
      child.kill("SIGKILL");
    }
    assert.equal(ended, 0);
  }));

test("the roster is watched", () =>
  withTeam(async (team, bob) => {
    const watching = team.roster.watch();
    assert.deepEqual((await watching.next()).value, []);
    await bob.roster.join();
    assert.deepEqual((await watching.next()).value, ["bob"]);
    await bob.roster.leave();
    assert.deepEqual((await watching.next()).value, []);
    await watching.return(undefined);
  }));

// The heartbeat goes on through a sidecar that restarts.
test("the agent is still here after the sidecar restarts", () =>
  withTeam(async (team, _bob, sidecar) => {
    await team.roster.join(0.1);
    const before = (await team.roster.members())[0].lastSeen;
    await sidecar.restart();
    const lastSeen = async () => {
      try {
        return (await team.roster.members())[0]?.lastSeen ?? 0;
      } catch (error) {
        if (error instanceof OrvuError) {
          return 0;
        }
        throw error;
      }
    };
    assert.ok(await waitUntil(async () => (await lastSeen()) > before, 10));
    // A beat that was on its way when the sidecar stopped is sent again once
    // it is back, with the time it was first sent at. The next one is fresh.
    assert.ok(await waitUntil(async () => JSON.stringify(await team.roster.here()) === '["alice"]', 5));
  }));

// How long an agent may be silent goes with the agent, not with whoever
// reads the roster.
test("an agent is judged by its own heartbeat", () =>
  withTeam(async (team, _bob, sidecar) => {
    const quick = agentProgram(sidecar.socket, "quick", 0.2);
    const slow = agentProgram(sidecar.socket, "slow", 1000);
    try {
      await Promise.all([quick.joined, slow.joined]);
      assert.deepEqual(await team.roster.here(), ["quick", "slow"]);
    } finally {
      quick.child.kill("SIGKILL");
      slow.child.kill("SIGKILL");
    }
    // Quick is away after three of its beats. Slow has hours yet.
    assert.ok(await waitUntil(async () => (await team.roster.here()).join() === "slow"));
  }));

// Two programs can run under one name. When one of them leaves, the agent
// is still here, because the other still is.
test("one of two programs under a name leaving does not take the name off", () =>
  withTeam(async (team, _bob, sidecar) => {
    const first = await teamOn(sidecar, "bob");
    const second = await teamOn(sidecar, "bob");
    await first.roster.join();
    await second.roster.join();
    assert.deepEqual(await team.roster.here(), ["bob"]);
    assert.deepEqual((await team.roster.members()).map((member) => member.name), ["bob"]);
    await first.close();
    assert.deepEqual(await team.roster.here(), ["bob"], "the name went with the first program to leave");
    await second.close();
    assert.deepEqual(await team.roster.members(), []);
  }));

// A join that cannot be finished leaves no heartbeat running.
test("a join that fails leaves nothing behind", () =>
  withTeam(async (team) => {
    // Something else is kept where the roster goes, so the first write fails.
    await team.client.counter("orvu/default/roster").increment(1);
    await assert.rejects(team.roster.join(0.05), OrvuError);
    await team.roster.leave(); // and there is nothing to leave
  }));

/** Writes an entry the way a program that ended with no goodbye leaves it:
 * under the agent's name, last heard `minutesAgo`. */
async function leftBehind(team: Team, name: string, minutesAgo: number, every = 15) {
  const seen = Math.round(Date.now() - minutesAgo * 60_000);
  await team.client.map("orvu/default/roster").put(`${name}\x1fdeadbeef`, `${seen} ${every}`);
}

// A program that ended with no goodbye leaves its entry behind. The next
// program under that name clears it when it joins.
test("joining clears what an ended program of the same name left", () =>
  withTeam(async (team, bob) => {
    await leftBehind(team, "bob", 10);
    await leftBehind(team, "carol", 10);
    await bob.roster.join();
    const keys = Object.keys(await team.client.map("orvu/default/roster").entries()).sort();
    assert.deepEqual(keys.map((key) => key.split("\x1f")[0]), ["bob", "carol"], keys.join());
    assert.ok(!keys.includes("bob\x1fdeadbeef"), "the entry the ended program left is still there");
    const members = (await team.roster.members()).map((member) => [member.name, member.here]);
    assert.deepEqual(members, [["bob", true], ["carol", false]]);
  }));

// An entry that has been silent for more than a day is not listed at all.
test("an agent silent for more than a day is not listed", () =>
  withTeam(async (team) => {
    await leftBehind(team, "bob", 2 * 24 * 60);
    await leftBehind(team, "carol", 2 * 60);
    const members = (await team.roster.members()).map((member) => [member.name, member.here]);
    assert.deepEqual(members, [["carol", false]]);
    assert.deepEqual(await team.roster.here(), []);
  }));

// Clearing what was left behind leaves alone another program under the same
// name that still runs.
test("joining leaves another running program of the same name alone", () =>
  withTeam(async (team, _bob, sidecar) => {
    const first = await teamOn(sidecar, "bob");
    const second = await teamOn(sidecar, "bob");
    try {
      await first.roster.join();
      await second.roster.join();
      const keys = Object.keys(await team.client.map("orvu/default/roster").entries());
      assert.deepEqual(keys.map((key) => key.split("\x1f")[0]), ["bob", "bob"], keys.join());
    } finally {
      await first.close();
      await second.close();
    }
  }));
