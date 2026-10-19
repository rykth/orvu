// Agents with keys. A sidecar that names agents lets in only programs that
// give one of their keys, and tells each which agent it is. The library then
// uses that name, so one agent's name cannot be put on another's work.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest, connect, NotAllowed, Team } from "../src/index.ts";
import { python, waitUntil, withSidecar } from "./support.ts";

test("with no agents nothing changes", () =>
  withSidecar(async (sidecar) => {
    const client = await connect(sidecar.socket);
    assert.equal(client.agent, null);
    client.close();
    const team = await Team.connect("default", { socket: sidecar.socket, me: "alice" });
    assert.equal(team.me, "alice");
    await team.close();
  }));

test("an agent is who its key says", () =>
  withSidecar(async (sidecar) => {
    const key = sidecar.addAgent("alice");
    const client = await connect(sidecar.socket, { key });
    assert.equal(client.agent, "alice");
    client.close();
    const team = await Team.connect("default", { socket: sidecar.socket, key });
    assert.equal(team.me, "alice");
    assert.equal((await team.tasks.add("Draft")).createdBy, "alice");
    await team.close();
  }));

test("a program without a known key is not let in", () =>
  withSidecar(async (sidecar) => {
    sidecar.addAgent("alice");
    for (const key of [undefined, "orvu-key-0123"]) {
      await assert.rejects(connect(sidecar.socket, { key }), NotAllowed);
    }
  }));

// A key given to a sidecar that checks none is refused, so that a program
// that expects to be checked finds out that it is not.
test("a key for a sidecar with no agents is refused", () =>
  withSidecar(async (sidecar) => {
    await assert.rejects(
      connect(sidecar.socket, { key: "orvu-key-0123" }),
      (error: Error) => error instanceof NotAllowed && /has no agents/.test(error.message),
    );
  }));

test("an agent cannot take another name", () =>
  withSidecar(async (sidecar) => {
    const key = sidecar.addAgent("alice");
    await assert.rejects(
      Team.connect("default", { socket: sidecar.socket, key, me: "bob" }),
      (error: Error) => error instanceof BadRequest && /alice/.test(error.message),
    );
    const team = await Team.connect("default", { socket: sidecar.socket, key, me: "alice" });
    assert.equal(team.me, "alice");
    await team.close();
  }));

test("the key can come from the environment", () =>
  withSidecar(async (sidecar) => {
    process.env.ORVU_KEY = sidecar.addAgent("alice");
    try {
      const team = await Team.connect("default", { socket: sidecar.socket });
      assert.equal(team.me, "alice");
      await team.close();
    } finally {
      delete process.env.ORVU_KEY;
    }
  }));

test("an agent that is removed is turned away", () =>
  withSidecar(async (sidecar) => {
    const alice = await connect(sidecar.socket, { key: sidecar.addAgent("alice") });
    const bob = await connect(sidecar.socket, { key: sidecar.addAgent("bob") });
    await alice.counter("visits").increment(1);
    sidecar.removeAgent("alice");
    await assert.rejects(alice.counter("visits").increment(1), NotAllowed);
    assert.equal(await bob.counter("visits").value(), 1);
    alice.close();
    bob.close();
  }));

// A program that came in before the first agent was added does not keep
// working without a key.
test("adding the first agent turns away programs without a key", () =>
  withSidecar(async (sidecar) => {
    const client = await connect(sidecar.socket);
    await client.counter("visits").increment(1);
    sidecar.addAgent("alice");
    await assert.rejects(client.counter("visits").value(), NotAllowed);
    client.close();
  }));

// After a restart the sidecar has forgotten every session. The client opens
// a new one with the same key, and is the same agent.
// Removing alice then turns it away.
test("an agent is still itself after the sidecar restarts", () =>
  withSidecar(async (sidecar) => {
    sidecar.addAgent("bob");
    const client = await connect(sidecar.socket, { key: sidecar.addAgent("alice") });
    await client.counter("visits").increment(1);
    await sidecar.restart();
    assert.ok(await waitUntil(async () => (await client.counter("visits").value()) === 1));
    assert.equal(client.agent, "alice");
    sidecar.removeAgent("alice");
    await assert.rejects(client.counter("visits").value(), NotAllowed);
    client.close();
  }));

// A watch that is open when its agent is removed ends, even if what it
// watches never changes again.
test("an open watch ends when its agent is removed", () =>
  withSidecar(async (sidecar) => {
    sidecar.addAgent("bob");
    const client = await connect(sidecar.socket, { key: sidecar.addAgent("alice") });
    await client.counter("visits").increment(1);
    const seen: number[] = [];
    await assert.rejects(async () => {
      for await (const value of client.counter("visits").watch()) {
        seen.push(value);
        sidecar.removeAgent("alice");
      }
    }, NotAllowed);
    assert.deepEqual(seen, [1]);
    client.close();
  }));

// Once there have been agents, removing the last one does not open the
// sidecar to everybody again.
test("removing the last agent lets nobody in", () =>
  withSidecar(async (sidecar) => {
    const key = sidecar.addAgent("alice");
    sidecar.removeAgent("alice");
    for (const given of [undefined, key]) {
      await assert.rejects(
        connect(sidecar.socket, { key: given }),
        (error: Error) => error instanceof NotAllowed && /lets nobody in/.test(error.message),
      );
    }
  }));

test("a key is text", () =>
  withSidecar(async (sidecar) => {
    await assert.rejects(connect(sidecar.socket, { key: 3 as unknown as string }), (error: Error) =>
      error instanceof BadRequest && /key/.test(error.message),
    );
  }));

// A Python agent and a Node agent, each with its own key, see each other's
// names on the same board.
test("agents with keys are the same agents in Python", () =>
  withSidecar(async (sidecar) => {
    const pia = sidecar.addAgent("pia");
    const team = await Team.connect("default", { socket: sidecar.socket, key: sidecar.addAgent("nora") });
    await team.tasks.add("Draft");
    const seen = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], key=${JSON.stringify(pia)}) as team:
    print(team.me, [task.created_by for task in team.tasks.list()])`,
    ).trim();
    assert.equal(seen, "pia ['nora']");
    await team.close();
  }));
