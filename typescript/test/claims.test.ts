// Firm claims, and claims that run out.
//
// A firm task has a ticket, and only the agent that takes it gets the task. A
// claim says how long it lasts without word from its agent, and a claim whose
// agent has gone quiet for longer is abandoned: another agent can take the
// task over.

import assert from "node:assert/strict";
import { test } from "node:test";
import { abandoned, BadRequest, Json, LEASE, NotAllowed, Pending, SidecarUnavailable, TicketUnavailable, UnknownTask, type Team } from "../src/index.ts";
import * as names from "../src/names.ts";
import { python, sleep, teamOn, withTeam } from "./support.ts";

/** Each agent claims every task, all at the same time. Gives who won each. */
async function race(agents: Team[], ids: string[]): Promise<Map<string, string[]>> {
  const won = new Map(ids.map((id) => [id, [] as string[]]));
  await Promise.all(
    agents.map(async (agent) => {
      for (const id of ids) {
        if (await agent.tasks.claim(id)) {
          won.get(id)!.push(agent.me!);
        }
      }
    }),
  );
  return won;
}

// ---- firm tasks ----------------------------------------------------------------

test("a firm task is claimed by one agent", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.deepEqual([task.firm, task.ticket], [true, 0]);
    assert.equal((await team.tasks.add("Draft")).firm, false);
    assert.equal(await bob.tasks.claim(task), true);
    assert.equal(await team.tasks.claim(task), false);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy], ["doing", "bob"]);
    // Claiming what is already yours is fine, and takes no second ticket.
    assert.equal(await bob.tasks.claim(task), true);
  }));

// The soft claim's known limit is two agents told yes at the same moment.
// Many agents race for many firm tasks at once, and each task is won once.
test("agents racing for firm tasks win each once", () =>
  withTeam(async (team, _bob, sidecar) => {
    const ids: string[] = [];
    for (let number = 0; number < 20; number++) {
      ids.push((await team.tasks.add(`task ${number}`, { firm: true })).id);
    }
    const agents = await Promise.all([0, 1, 2, 3, 4, 5].map((number) => teamOn(sidecar, `agent-${number}`)));
    const won = await race(agents, ids);
    await Promise.all(agents.map((agent) => agent.close()));
    for (const [id, winners] of won) {
      assert.equal(winners.length, 1, `${id}: ${winners}`);
      assert.equal((await team.tasks.get(id))!.claimedBy, winners[0]);
    }
  }));

test("a firm task given back can be claimed again", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claim(task), true);
    assert.equal(await bob.tasks.release(task), false);
    assert.equal(await team.tasks.release(task), true);
    assert.equal(await team.tasks.release(task), false, "a task was given back twice");
    assert.equal(await bob.tasks.claim(task), true);
    assert.equal(await team.tasks.claim(task), false);
  }));

// Setting a firm task back to "todo" by hand frees it, whoever had it. Its old
// holder is no longer on it, even though its ticket was never given back.
test("a firm task set back by hand is free again", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await bob.tasks.claim(task), true);
    const freed = await team.tasks.update(task, { state: "todo" });
    assert.deepEqual([freed.state, freed.claimedBy, freed.ticket], ["todo", null, 1]);
    assert.equal(await bob.tasks.stillWorking(task), false);
    const carol = await teamOn(sidecar, "carol");
    assert.equal(await carol.tasks.claim(task), true);
    assert.equal(await bob.tasks.claim(task), false);
    await carol.close();
  }));

test("a finished firm task can be opened again", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claim(task), true);
    await team.tasks.finish(task, "sent");
    assert.equal(await bob.tasks.claim(task), false);
    await team.tasks.update(task, { state: "todo" });
    assert.equal(await bob.tasks.claim(task), true);
    assert.equal(await team.tasks.claim(task), false);
  }));

test("a firm task's tickets are in the index", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    const index = await team.index();
    assert.equal(index[`task-ticket/${task.id}/0`], "bounded");
    assert.equal(index[`task-takeover/${task.id}/0`], "bounded");
    assert.equal(names.taskTicket("crew", task.id, 3), `orvu/crew/task-ticket/${task.id}/3`);
    assert.equal(names.taskTakeover("crew", task.id, 3), `orvu/crew/task-takeover/${task.id}/3`);
  }));

test("firm is true or false", () =>
  withTeam(async (team) => {
    for (const firm of ["yes", 1]) {
      await assert.rejects(team.tasks.add("Draft", { firm: firm as unknown as boolean }), (error: Error) => error instanceof BadRequest && /firm/.test(error.message));
    }
    assert.deepEqual(await team.tasks.list(), []);
    // null means none, as everywhere on a task.
    assert.equal((await team.tasks.add("Draft", { firm: null as unknown as boolean })).firm, false);
  }));

// ---- claims that run out -----------------------------------------------------

test("a claim says when its agent was last heard from", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    const before = Date.now();
    assert.equal(await team.tasks.claim(task, undefined, { lease: 60 }), true);
    const found = (await team.tasks.get(task))!;
    assert.ok(before <= found.seen! && found.seen! <= Date.now());
    assert.equal(found.lease, 60);
    assert.equal(abandoned(found), false);
    // Without a lease, a claim lasts a quarter of an hour.
    const other = await team.tasks.add("Check the figures");
    await team.tasks.claim(other);
    assert.equal((await team.tasks.get(other))!.lease, LEASE);
    assert.equal(LEASE, 900);
  }));

test("stillWorking keeps a claim from running out", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 1 });
    for (let i = 0; i < 3; i++) {
      await sleep(500);
      assert.equal(await team.tasks.stillWorking(task), true);
    }
    assert.equal(abandoned((await team.tasks.get(task))!), false);
    await sleep(1200);
    assert.equal(abandoned((await team.tasks.get(task))!), true);
  }));

test("an abandoned claim is taken over", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 1 });
    assert.equal(await bob.tasks.claim(task), false);
    await sleep(1200);
    assert.equal(await bob.tasks.claim(task), true);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy, abandoned(found)], ["doing", "bob", false]);
    // The agent that went quiet finds out when it next says it is working.
    assert.equal(await team.tasks.stillWorking(task), false);
    assert.equal(await team.tasks.release(task), false);
    assert.equal((await team.tasks.get(task))!.claimedBy, "bob");
  }));

test("an abandoned firm claim is taken over by one agent", () =>
  withTeam(async (team, _bob, sidecar) => {
    const ids: string[] = [];
    for (let number = 0; number < 10; number++) {
      const task = await team.tasks.add(`task ${number}`, { firm: true });
      assert.equal(await team.tasks.claim(task, undefined, { lease: 1 }), true);
      ids.push(task.id);
    }
    const agents = await Promise.all([0, 1, 2, 3].map((number) => teamOn(sidecar, `agent-${number}`)));
    // Each agent sees the claims, and then they go quiet for longer than
    // they last.
    for (const agent of agents) {
      await agent.tasks.list();
    }
    await sleep(1200);
    const won = await race(agents, ids);
    await Promise.all(agents.map((agent) => agent.close()));
    for (const [id, winners] of won) {
      assert.equal(winners.length, 1, `${id}: ${winners}`);
      const found = (await team.tasks.get(id))!;
      assert.deepEqual([found.claimedBy, found.ticket], [winners[0], 1]);
      assert.equal(await team.tasks.stillWorking(id), false);
    }
  }));

// The writes of a batch are not seen all at once, so another agent can read
// a task halfway through a takeover. It must not see the new ticket number
// with the old, run-out time beside it: it would take the new ticket's
// takeover ticket, and two agents would have the task. So the fresh time is
// written first. Which order the writes go in is checked here, because a
// race shows it only now and then.
test("a takeover writes the fresh time before the new ticket", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    await team.tasks.claim(task, undefined, { lease: 1 });
    await bob.tasks.get(task);
    await sleep(1200);
    const written: unknown[] = [];
    const plain = Json.prototype.set;
    Json.prototype.set = function (this: Json, path, value) {
      written.push(path.at(-1));
      return plain.call(this, path, value);
    } as typeof plain;
    try {
      assert.equal(await bob.tasks.claim(task), true);
    } finally {
      Json.prototype.set = plain;
    }
    assert.ok(written.indexOf("seen") < written.indexOf("ticket"), String(written));
    assert.ok(written.indexOf("seen") < written.indexOf("claimed_by"), String(written));
  }));

// ---- the number of the ticket -------------------------------------------------
//
// Whoever holds a firm task holds one numbered ticket, and the number goes up
// each time the task moves on. A step that must not happen twice carries the
// number to whatever it changes, and that side refuses a number lower than
// the highest it has seen.

test("a firm claim gives the number of its ticket", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claimTicket(task), 0);
    // Claiming it again keeps it, and gives the same number.
    assert.equal(await team.tasks.claimTicket(task), 0);
    assert.equal(await bob.tasks.claimTicket(task), null);
    assert.equal(await team.tasks.stillHolding(task, 0), true);
    assert.equal(await bob.tasks.stillHolding(task, 0), false);
    assert.equal(await team.tasks.release(task), true);
    assert.equal(await team.tasks.stillHolding(task, 0), false);
    assert.equal(await bob.tasks.claimTicket(task), 1);
    assert.equal(await bob.tasks.stillHolding(task, 1), true);
    assert.equal(await bob.tasks.stillHolding(task, 0), false);
    assert.equal(await bob.tasks.finish(task, "sent"), true);
    assert.equal(await bob.tasks.stillHolding(task, 1), false);
  }));

test("only a firm task has a ticket", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    const notFirm = (error: Error) => error instanceof BadRequest && /firm/.test(error.message);
    await assert.rejects(team.tasks.claimTicket(task), notFirm);
    await assert.rejects(team.tasks.stillHolding(task, 0), notFirm);
    assert.equal((await team.tasks.get(task))!.state, "todo");
  }));

// The agent that takes a task over gets a higher number than the one that
// went quiet. Whatever the old holder still does carries the lower number,
// and a side that keeps the highest number it has seen refuses it.
test("a takeover gets a higher number and the old one is refused", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claimTicket(task, undefined, { lease: 1 }), 0);
    await bob.tasks.get(task);
    await sleep(1200);
    assert.equal(await bob.tasks.claimTicket(task), 1);
    assert.equal(await team.tasks.stillHolding(task, 0), false);
    assert.equal(await bob.tasks.stillHolding(task, 1), true);
    let highest = -1;
    const sendInvoice = (ticket: number) => {
      if (ticket < highest) {
        return false;
      }
      highest = ticket;
      return true;
    };
    assert.equal(sendInvoice(1), true);
    assert.equal(sendInvoice(0), false, "the agent that lost the task sent the invoice again");
  }));

// The writes of a batch are not seen all at once. In a takeover, the old
// holder's name is taken off before the new number is written, so the name
// and the number seen together always belong together: an agent that went
// quiet can never be shown beside the new number and think it holds it.
test("a takeover takes the old name off before the new number", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    await team.tasks.claim(task, undefined, { lease: 1 });
    await bob.tasks.get(task);
    await sleep(1200);
    const written: string[] = [];
    const set = Json.prototype.set;
    const remove = Json.prototype.remove;
    Json.prototype.set = function (this: Json, path, value) {
      written.push(`set ${String(path.at(-1))}`);
      return set.call(this, path, value);
    } as typeof set;
    Json.prototype.remove = function (this: Json, path) {
      written.push(`remove ${String(path.at(-1))}`);
      return remove.call(this, path);
    } as typeof remove;
    try {
      assert.equal(await bob.tasks.claim(task), true);
    } finally {
      Json.prototype.set = set;
      Json.prototype.remove = remove;
    }
    const at = (what: string) => written.indexOf(what);
    assert.ok(at("remove claimed_by") >= 0 && at("remove claimed_by") < at("set ticket"), String(written));
    assert.ok(at("set ticket") < at("set claimed_by"), String(written));
  }));

// The agent that took a firm task over gives it back, and the next agent can
// take it. The old holder, which never gave back its own ticket, cannot.
test("a firm task taken over and given back goes to one agent", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    await team.tasks.claim(task, undefined, { lease: 1 });
    await bob.tasks.get(task);
    await sleep(1200);
    assert.equal(await bob.tasks.claim(task), true);
    assert.equal(await bob.tasks.release(task), true);
    const carol = await teamOn(sidecar, "carol");
    assert.equal(await carol.tasks.claim(task), true);
    assert.equal(await team.tasks.claim(task), false);
    assert.equal(await bob.tasks.claim(task), false);
    await carol.close();
  }));

// Every agent that reads the board sees that a task was taken over, from
// whom, by whom and when, not only the two agents on it.
for (const firm of [false, true]) {
  test(`a takeover is written on the task for everyone${firm ? ", firm" : ""}`, () =>
    withTeam(async (team, bob, sidecar) => {
      const task = await team.tasks.add("Draft", { firm });
      await team.tasks.claim(task, undefined, { lease: 1 });
      assert.equal((await team.tasks.get(task))!.takenOver, null);
      await bob.tasks.get(task);
      await sleep(1200);
      const before = Date.now();
      assert.equal(await bob.tasks.claim(task), true);
      const after = Date.now();
      const carol = await teamOn(sidecar, "carol");
      const found = (await carol.tasks.get(task))!;
      try {
        assert.deepEqual([found.takenOver?.by, found.takenOver?.from], ["bob", "alice"]);
        assert.ok(before <= found.takenOver!.at && found.takenOver!.at <= after);
        assert.deepEqual((await carol.tasks.list()).map((listed) => listed.takenOver), [found.takenOver]);
        const watching = carol.tasks.watch();
        assert.deepEqual((await watching.next()).value![0].takenOver, found.takenOver);
        await watching.return(undefined);
      } finally {
        await carol.close();
      }
      // It stays as a record of what happened when the task is given back.
      assert.equal(await bob.tasks.release(task), true);
      assert.deepEqual((await team.tasks.get(task))!.takenOver, found.takenOver);
    }));
}

test("a claim that takes over nothing writes no takeover", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    assert.equal(await team.tasks.claim(task), true);
    assert.equal(await team.tasks.release(task), true);
    assert.equal(await bob.tasks.claim(task), true);
    assert.equal((await bob.tasks.get(task))!.takenOver, null);
    assert.equal(await team.client.json(names.tasks(team.name)).get([task.id, "taken_over"]), null);
  }));

test("a finished task is never abandoned", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 1 });
    await team.tasks.finish(task, "done");
    await sleep(1200);
    assert.equal(abandoned((await team.tasks.get(task))!), false);
    assert.equal(await bob.tasks.claim(task), false);
  }));

// A claim written before claims ran out has no time on it. It is left alone.
test("a claim with no time on it does not run out", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.client.json(names.tasks(team.name)).set([task.id, "claimed_by"], "alice");
    await team.client.json(names.tasks(team.name)).set([task.id, "state"], "doing");
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.seen, found.lease, abandoned(found)], [null, null, false]);
    assert.equal(await bob.tasks.claim(task), false);
  }));

test("abandoned can be asked about any moment", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 10 });
    const found = (await team.tasks.get(task))!;
    // Just inside and just past the lease, not on it: the steady clock reads
    // in fractions of a millisecond, and adding the lease to such a reading
    // and taking it off again can be off by a hair either way.
    assert.equal(abandoned(found, found.quietSince! + 9_999.9), false);
    assert.equal(abandoned(found, found.quietSince! + 10_000.1), true);
    assert.equal(abandoned((await team.tasks.get(await team.tasks.add("Check the figures")))!, found.quietSince! + 1e9), false);
  }));

// ---- clocks that do not agree -------------------------------------------------
//
// Whether a claim has run out goes by the clock of the agent that wants the
// task: it has run out when that agent has seen no change to it for as long
// as it lasts. The time the agent on the task wrote, by its own clock, is
// never compared with another machine's clock.

/** Writes `seen` on the task as its last word, as an agent whose clock is
 * wrong would. */
const writtenAt = (team: Team, id: string, seen: number) => team.client.json(names.tasks(team.name)).set([id, "seen"], seen);

test("a claim written by a clock far behind does not run out at once", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 60 });
    await writtenAt(team, task.id, Date.now() - 3_600_000);
    assert.equal(abandoned((await bob.tasks.get(task))!), false);
    assert.equal(await bob.tasks.claim(task), false);
    assert.equal((await team.tasks.get(task))!.claimedBy, "alice");
  }));

test("a claim written by a clock far ahead still runs out", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 1 });
    await writtenAt(team, task.id, Date.now() + 3_600_000);
    assert.equal(abandoned((await bob.tasks.get(task))!), false);
    await sleep(1200);
    assert.equal(abandoned((await bob.tasks.get(task))!), true);
    assert.equal(await bob.tasks.claim(task), true);
  }));

// An agent that has only just started has not seen the claim before, and
// waits a whole lease from now, however old the time written on it is.
test("an agent that has just started waits a whole lease", () =>
  withTeam(async (team, _bob, sidecar) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 1 });
    await sleep(1200);
    const carol = await teamOn(sidecar, "carol");
    try {
      assert.equal(abandoned((await carol.tasks.get(task))!), false);
      assert.equal(await carol.tasks.claim(task), false);
      await sleep(1200);
      assert.equal(await carol.tasks.claim(task), true);
    } finally {
      await carol.close();
    }
  }));

// A claim that moved on to another ticket under the same name, with the same
// time and lease, is a new claim, and the wait for it starts again.
test("a claim on a new ticket starts the wait again", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claim(task, undefined, { lease: 1 }), true);
    assert.equal(abandoned((await bob.tasks.get(task))!), false);
    await sleep(1200);
    assert.equal(abandoned((await bob.tasks.get(task))!), true);
    await team.client.json(names.tasks(team.name)).set([task.id, "ticket"], 7);
    assert.equal(abandoned((await bob.tasks.get(task))!), false);
  }));

// Word from the agent on the task starts the wait again, whatever time it
// wrote.
test("any word from the agent starts the wait again", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    await team.tasks.claim(task, undefined, { lease: 1 });
    await writtenAt(team, task.id, 5);
    await bob.tasks.get(task);
    await sleep(700);
    await writtenAt(team, task.id, 4);
    assert.equal(abandoned((await bob.tasks.get(task))!), false);
    await sleep(700);
    assert.equal(abandoned((await bob.tasks.get(task))!), false);
    await sleep(500);
    assert.equal(abandoned((await bob.tasks.get(task))!), true);
  }));

test("a lease is a whole number of seconds", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    for (const lease of [0, -1, 1.5, true, "60", 1e9, Number.NaN]) {
      await assert.rejects(
        team.tasks.claim(task, undefined, { lease: lease as number }),
        (error: Error) => error instanceof BadRequest && /lease/.test(error.message),
        String(lease),
      );
    }
    assert.equal((await team.tasks.get(task))!.state, "todo");
  }));

test("stillWorking is only for the agent on the task", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    assert.equal(await team.tasks.stillWorking(task), false);
    await team.tasks.claim(task);
    assert.equal(await bob.tasks.stillWorking(task), false);
    await team.tasks.finish(task);
    assert.equal(await team.tasks.stillWorking(task), false);
    await assert.rejects(team.tasks.stillWorking("t-00000000"), UnknownTask);
  }));

// ---- an agent that is behind ----------------------------------------------------
//
// What other agents do reaches an agent a moment later, and on another
// machine it can be much later. What it does then must not give a firm task
// two holders, or leave it stuck.

/** Runs `then` and gives the last field of the path of each write it made to
 * a JSON document, in order. */
async function writes(then: () => Promise<unknown>): Promise<unknown[]> {
  const written: unknown[] = [];
  const set = Json.prototype.set;
  const remove = Json.prototype.remove;
  Json.prototype.set = function (this: Json, path, value) {
    written.push(path.at(-1));
    return set.call(this, path, value);
  } as typeof set;
  Json.prototype.remove = function (this: Json, path) {
    written.push(path.at(-1));
    return remove.call(this, path);
  } as typeof remove;
  try {
    await then();
  } finally {
    Json.prototype.set = set;
    Json.prototype.remove = remove;
  }
  return written;
}

// An agent that claims a firm task it has does not write its name again. If
// the task was taken over and it has not heard, its name would go back on.
test("claiming a firm task again does not write the name again", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claim(task), true);
    const written = await writes(async () => assert.equal(await team.tasks.claim(task), true));
    assert.deepEqual(written, ["seen"]);
  }));

// A firm task that nobody had is set back by hand. Its old ticket must not
// stay free beside the new one, or an agent that has not heard of the new
// one takes the old.
test("a free firm task set back by hand has one free ticket", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal((await team.tasks.update(task, { state: "todo" })).ticket, 1);
    assert.equal(await team.client.boundedCounter(names.taskTicket(team.name, task.id, 0)).tryDecrement(1), false);
    assert.equal(await bob.tasks.claim(task), true);
  }));

// The agent giving a firm task back finds its takeover ticket taken: another
// agent took the task over or set it back first. Nothing changes.
test("a firm task given back too late stays with whoever moved it on", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claim(task), true);
    assert.equal(await team.client.boundedCounter(names.taskTakeover(team.name, task.id, 0)).tryDecrement(1), true);
    assert.equal(await team.tasks.release(task), false);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy, found.ticket], ["doing", "alice", 0]);
  }));

// A claim that took the ticket and then failed to write its name does not
// keep the ticket: the task would be stuck.
test("a firm claim that fails halfway lets the task go", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    const plain = Json.prototype.set;
    Json.prototype.set = function (this: Json, path, value) {
      if (path.at(-1) === "claimed_by") {
        throw new SidecarUnavailable("the line went down");
      }
      return plain.call(this, path, value);
    } as typeof plain;
    try {
      await assert.rejects(team.tasks.claim(task), SidecarUnavailable);
    } finally {
      Json.prototype.set = plain;
    }
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy, found.ticket], ["todo", null, 1]);
    assert.equal(await bob.tasks.claim(task), true);
  }));

test("only the agent on a firm task can finish it", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await bob.tasks.finish(task, "sent"), false);
    assert.equal(await team.tasks.claim(task), true);
    assert.equal(await bob.tasks.finish(task, "sent"), false);
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result], ["doing", null]);
    assert.equal(await team.tasks.finish(task, "sent"), true);
    // Any agent can finish a task that is not firm, as before.
    assert.equal(await bob.tasks.finish(await team.tasks.add("Draft"), "ok"), true);
  }));

// A firm task given back, or set back by hand, moves on to a new ticket. Its
// new number is written last, after the name and time of the agent that had
// it are gone.
for (const how of ["given back", "set back"]) {
  test(`a firm task ${how} writes its new ticket last`, () =>
    withTeam(async (team) => {
      const task = await team.tasks.add("Send the invoice", { firm: true });
      await team.tasks.claim(task);
      const written = await writes(() =>
        how === "given back" ? team.tasks.release(task) : team.tasks.update(task, { state: "todo" }),
      );
      assert.ok(written.includes("ticket"), String(written));
      for (const field of ["state", "claimed_by", "seen", "lease"]) {
        assert.ok(written.indexOf(field) < written.indexOf("ticket"), String(written));
      }
      assert.equal((await team.tasks.get(task))!.ticket, 1);
    }));
}

/** The lines for a task's tickets in the team's list of what it has. */
async function ticketLines(team: Team, id: string): Promise<string[]> {
  return Object.keys(await team.index())
    .filter((name) => ["task-ticket", "task-takeover"].includes(name.split("/")[0]) && name.split("/")[1] === id)
    .sort();
}

const pair = (id: string, number: number) => [`task-takeover/${id}/${number}`, `task-ticket/${id}/${number}`];

// A ticket that can never be had again is taken off the team's list: only
// the pair the task is on now is listed, however often it moved on.
test("only the tickets a firm task is on are in the index", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.deepEqual(await ticketLines(team, task.id), pair(task.id, 0));
    await team.tasks.claim(task, undefined, { lease: 1 });
    await bob.tasks.get(task);
    await sleep(1200);
    assert.equal(await bob.tasks.claim(task), true);
    assert.deepEqual(await ticketLines(team, task.id), pair(task.id, 1));
    assert.equal(await bob.tasks.release(task), true);
    assert.deepEqual(await ticketLines(team, task.id), pair(task.id, 2));
    assert.equal((await team.index())[`task-ticket/${task.id}/2`], "bounded");
  }));

// Finished, a firm task keeps only the ticket it needs to be set back again,
// so finishing many tasks leaves no more behind than one line each, however
// often each of them moved on.
test("finishing many firm tasks does not grow what is kept for them", () =>
  withTeam(async (team, bob) => {
    const tasks = [];
    for (let number = 0; number < 8; number++) {
      tasks.push(await team.tasks.add(`task ${number}`, { firm: true }));
    }
    for (const task of tasks) {
      for (let round = 0; round < 3; round++) {
        assert.equal(await team.tasks.claim(task), true);
        assert.equal(await team.tasks.release(task), true);
      }
      assert.equal(await bob.tasks.claim(task), true);
      await team.tasks.update(task, { state: "todo" });
      assert.equal(await bob.tasks.claim(task), true);
      assert.equal(await bob.tasks.finish(task, "sent"), true);
    }
    for (const task of tasks) {
      assert.deepEqual(await ticketLines(team, task.id), [`task-takeover/${task.id}/4`]);
    }
    assert.equal(Object.keys(await team.index()).filter((name) => name.startsWith("task-")).length, tasks.length);
    // It can still be opened again, and claimed.
    assert.equal((await team.tasks.update(tasks[0], { state: "todo" })).ticket, 5);
    assert.deepEqual(await ticketLines(team, tasks[0].id), pair(tasks[0].id, 5));
    assert.equal(await team.tasks.claim(tasks[0]), true);
  }));

// null means none, as everywhere on a task: a claim with a lease of null
// lasts as long as one with none.
test("a lease of null is the usual lease", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Draft");
    assert.equal(await team.tasks.claim(task, undefined, { lease: null }), true);
    assert.equal((await team.tasks.get(task))!.lease, LEASE);
  }));

// Taking spent tickets off the team's list is only tidying: the list is not
// what keeps a task firm. A sidecar that refuses it, as an older one with
// rules does, leaves the lines there, and the task goes on all the same. The
// lines are only taken off once the task's new number is on the board.
test("a firm task moves on when its old tickets cannot be taken off the list", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    const seen: number[] = [];
    const refused = async () => {
      seen.push((await team.tasks.get(task))!.ticket);
      throw new NotAllowed("this sidecar does not let anything be taken off the index");
    };
    team.forget = refused;
    bob.forget = refused;
    assert.equal(await team.tasks.claimTicket(task, undefined, { lease: 1 }), 0);
    assert.equal(await team.tasks.release(task), true);
    assert.deepEqual(seen, [1]);
    assert.equal((await team.tasks.update(task, { state: "todo" })).ticket, 2);
    assert.equal(seen.at(-1), 2);
    assert.equal(await team.tasks.claimTicket(task, undefined, { lease: 1 }), 2);
    // A takeover, from another program whose tidying is refused too.
    await bob.tasks.get(task);
    await sleep(1200);
    assert.equal(await bob.tasks.claimTicket(task), 3);
    assert.equal(seen.at(-1), 3);
    await stoppedHolding(team, names.taskTakeover(team.name, task.id, 3));
    const forced = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    assert.equal(seen.at(-1), forced);
    assert.equal(await team.tasks.claimTicket(task), forced);
    assert.equal(await team.tasks.finish(task, "sent"), true);
  }));

// ---- a firm task that is stuck --------------------------------------------------
//
// An agent that stops between taking a ticket and writing what it did leaves
// the ticket taken for good. Setting the task back by hand then fails every
// time, because a taken ticket cannot be told apart from one on a machine that
// cannot be reached. An operator who knows that agent has stopped for good
// sets the task back with force, which starts a fresh pair of tickets.

/** Takes a ticket as an agent that stopped right after would have. */
async function stoppedHolding(team: Team, name: string): Promise<void> {
  assert.equal(await team.client.boundedCounter(name).tryDecrement(1), true);
}

/** Runs `then`, in which the first `reads` reads of the task with this id
 * are shown `fields` instead of what is on the board, as if what changed
 * since had not reached that agent yet. */
async function behind(id: string, fields: unknown, then: () => Promise<unknown>, reads = 1): Promise<void> {
  const plain = Object.getOwnPropertyDescriptor(Pending.prototype, "value")!;
  let shown = 0;
  Object.defineProperty(Pending.prototype, "value", {
    configurable: true,
    get(this: Pending<unknown>) {
      const value = plain.get!.call(this);
      const op = this.op;
      if (shown < reads && op.field === "json_get" && op.message.path_json === JSON.stringify([id])) {
        shown += 1;
        return fields;
      }
      return value;
    },
  });
  try {
    await then();
  } finally {
    Object.defineProperty(Pending.prototype, "value", plain);
  }
  assert.ok(shown > 0, "the old view was never shown");
}

/** What the board holds for a task, as an agent that reads it now sees it. */
const onBoard = (team: Team, id: string) => team.client.json(names.tasks(team.name)).get([id]);

// An agent claims a firm task it holds, but first sees it as it was before
// it gave the task back and claimed it again. The number it is told is the
// one on the task now, not the old one it was shown.
test("claiming a firm task again gives the number it holds now", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claimTicket(task), 0);
    const old = await onBoard(team, task.id);
    assert.equal(await team.tasks.release(task), true);
    assert.equal(await team.tasks.claimTicket(task), 1);
    await behind(task.id, old, async () => assert.equal(await team.tasks.claimTicket(task), 1), 2);
  }));

// Bob looks at a task, and it is free. Before his claim is written, alice
// finishes it. His claim must not bring the task back to life.
test("a claim that lands after the task was finished does not undo it", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Draft");
    const lookedAt = await onBoard(bob, task.id);
    assert.equal(await team.tasks.finish(task, "by alice"), true);
    await behind(task.id, lookedAt, async () => assert.equal(await bob.tasks.claim(task), false));
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.result, found.finishedBy], ["done", "by alice", "alice"]);
  }));

test("a stuck firm task is set back only with force", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await bob.tasks.claimTicket(task), 0);
    // Another agent took the ticket for moving the task on, and stopped.
    await stoppedHolding(team, names.taskTakeover(team.name, task.id, 0));
    await assert.rejects(
      team.tasks.update(task, { state: "todo" }),
      (error: Error) => error instanceof TicketUnavailable && error.message.includes("force: true"),
    );
    assert.equal((await team.tasks.get(task))!.claimedBy, "bob");
    const freed = await team.tasks.update(task, { state: "todo", force: true });
    assert.deepEqual([freed.state, freed.claimedBy], ["todo", null]);
    // The fresh pair is far past any pair the task had, or that the agent
    // that stopped may have made before it did.
    assert.ok(freed.ticket > 2, String(freed.ticket));
    assert.deepEqual(await ticketLines(team, task.id), pair(task.id, freed.ticket));
    assert.equal(await bob.tasks.stillWorking(task), false);
    const carol = await teamOn(sidecar, "carol");
    try {
      assert.equal(await carol.tasks.claimTicket(task), freed.ticket);
      assert.equal(await bob.tasks.claim(task), false);
      assert.equal(await team.tasks.claim(task), false);
    } finally {
      await carol.close();
    }
  }));

// The agent claiming the task took its ticket and stopped before it wrote its
// name. With force, the task moves on, and that old ticket can never win it
// back, even if it turns up free again on a machine that was away.
test("an old ticket never wins a task set back with force", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    const old = await team.client.json(names.tasks(team.name)).get([task.id]);
    await stoppedHolding(team, names.taskTicket(team.name, task.id, 0));
    await assert.rejects(team.tasks.update(task, { state: "todo" }), TicketUnavailable);
    const forced = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    assert.ok(forced > 0);
    await team.client.boundedCounter(names.taskTicket(team.name, task.id, 0)).increment(1);
    await behind(task.id, old, async () => assert.equal(await bob.tasks.claim(task), false));
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy, found.ticket], ["todo", null, forced]);
    assert.equal(await team.tasks.claimTicket(task), forced);
  }));

/** The numbers carol, and then dave seeing the task as `daveSees`, are told
 * when they claim it. */
async function twoClaims(sidecar: Parameters<typeof teamOn>[0], id: string, daveSees: unknown): Promise<(number | null)[]> {
  const carol = await teamOn(sidecar, "carol");
  const dave = await teamOn(sidecar, "dave");
  try {
    const told = [await carol.tasks.claimTicket(id)];
    await behind(id, daveSees, async () => told.push(await dave.tasks.claimTicket(id)), 2);
    return told;
  } finally {
    await carol.close();
    await dave.close();
  }
}

const noneTwice = (told: (number | null)[]) => {
  const numbers = told.filter((number) => number !== null);
  assert.equal(new Set(numbers).size, numbers.length, String(told));
};

// Two operators set the same stuck task back with force at the same moment,
// both from what they read before either wrote, or one tries again after its
// write to the board failed. Each force lands on a pair of its own, so two
// agents claiming the task can never both be told the same number.
test("two forces from one view never give a number two holders", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await bob.tasks.claimTicket(task), 0);
    const stale = await onBoard(team, task.id);
    await stoppedHolding(team, names.taskTakeover(team.name, task.id, 0));
    const first = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    const free = await onBoard(team, task.id);
    let second = 0;
    await behind(task.id, stale, async () => {
      second = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    });
    // It saw the first one's pair, and jumped from there, not from the old
    // number it was shown.
    assert.ok(second - first >= 2 ** 31, `${second} after ${first}`);
    // Dave has not heard of the second force yet, and sees the first.
    noneTwice(await twoClaims(sidecar, task.id, free));
  }));

// The same, when the second force cannot see the first at all, as on a
// machine cut off from the other operator's: it still picks another pair.
test("a force that cannot see another still picks its own pair", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await bob.tasks.claimTicket(task), 0);
    const stale = await onBoard(team, task.id);
    const nothingYet = await team.index();
    await stoppedHolding(team, names.taskTakeover(team.name, task.id, 0));
    const first = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    team.index = async () => nothingYet;
    await behind(task.id, stale, () => team.tasks.update(task, { state: "todo", force: true }));
    // Each force wrote its own number down, and they differ. The task's
    // number is the higher of the two, and each pair holds one.
    const written = Object.keys((await team.client.json(names.tasks(team.name)).get([task.id, "tickets"])) as object).map(Number);
    assert.equal(written.length, 2, String(written));
    assert.ok(written.includes(first), String(written));
    assert.equal((await team.tasks.get(task))!.ticket, Math.max(...written));
    for (const number of written) {
      assert.equal(await team.client.boundedCounter(names.taskTicket(team.name, task.id, number)).value(), 1);
    }
  }));

// A force made from an old view races an agent that took the task over and
// gave it back, which moved the task on twice. The force must not land on the
// pair the agent gave back.
test("a force racing a takeover never gives a number two holders", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    await team.tasks.claim(task, undefined, { lease: 1 });
    const stale = await onBoard(team, task.id);
    await bob.tasks.get(task);
    await sleep(1200);
    assert.equal(await bob.tasks.claimTicket(task), 1);
    assert.equal(await bob.tasks.release(task), true);
    const givenBack = await onBoard(team, task.id);
    assert.equal((givenBack as { ticket: number }).ticket, 2);
    let forced = 0;
    await behind(task.id, stale, async () => {
      forced = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    });
    assert.ok(forced > 2, String(forced));
    // Dave has not heard of the force, and sees the task as bob left it.
    noneTwice(await twoClaims(sidecar, task.id, givenBack));
  }));

// What is written into a task can never stop the board from being read. A
// ticket number counts only if it is one to fifteen plain digits: anything
// else under "tickets", or a "ticket" that is not such a number, is left out.
// Both libraries read the same number from the same task.
const ODD_KEYS = ["²", "1".repeat(5000), "１２", "1e3", "-5", " 7", "7 ", "", "0x10", "1000000000000000", "9".repeat(30)];

test("odd ticket numbers are left out and never break reading", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claimTicket(task), 0);
    assert.equal(await team.tasks.release(task), true);
    const board = team.client.json(names.tasks(team.name));
    for (const key of ODD_KEYS) {
      await board.set([task.id, "tickets", key], true);
    }
    await board.set([task.id, "tickets", "000000000000001"], true);
    for (const odd of [1e300, -3, 2.5, "9", 2 ** 53 + 2]) {
      await board.set([task.id, "ticket"], odd);
      assert.deepEqual((await team.tasks.list()).map((found) => found.ticket), [1]);
      assert.equal((await team.tasks.get(task))!.ticket, 1);
    }
    // Python reads the same number from the same task.
    const seen = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    print([found.ticket for found in team.tasks.list()])`,
    ).trim();
    assert.equal(seen, "[1]");
    assert.equal(await bob.tasks.claimTicket(task), 1);
  }));

// Numbers are compared as numbers, not as text: 10 is past 9.
test("ticket numbers compare as numbers", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    const board = team.client.json(names.tasks(team.name));
    await board.set([task.id, "tickets", "9"], true);
    await board.set([task.id, "tickets", "10"], true);
    assert.equal((await team.tasks.get(task))!.ticket, 10);
  }));

// A force never goes past the highest number both libraries read the same. A
// task whose numbers are that high is told to be added again.
test("a force never goes past the highest number", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await bob.tasks.claimTicket(task), 0);
    await stoppedHolding(team, names.taskTakeover(team.name, task.id, 0));
    const board = team.client.json(names.tasks(team.name));
    // Its ticket for moving on was never made, so only force moves it on.
    await board.set([task.id, "tickets", "999990000000000"], true);
    const forced = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    assert.ok(forced > 999990000000000 && forced < 10 ** 15, String(forced));
    await board.set([task.id, "tickets", "999999999999999"], true);
    await assert.rejects(
      team.tasks.update(task, { state: "todo", force: true }),
      (error: Error) => error instanceof TicketUnavailable && /used up.*Add the task again/.test(error.message),
    );
    assert.equal((await team.tasks.get(task))!.ticket, 999999999999999);
  }));

// A ticket holding more than one can be taken by more than one agent, so it
// is never taken: claiming the task says to set it back.
test("a doubled ticket is never taken", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    const ticket = team.client.boundedCounter(names.taskTicket(team.name, task.id, 0));
    await ticket.increment(1);
    // The claim says so, and what to do: a plain set-back is enough.
    const doubled = (pattern: RegExp) => (error: Error) => error instanceof TicketUnavailable && pattern.test(error.message);
    await assert.rejects(team.tasks.claim(task), doubled(/made twice.*update\(task, \{ state: "todo" \}\)/));
    await assert.rejects(bob.tasks.claimTicket(task), doubled(/made twice/));
    assert.equal(await ticket.value(), 2);
    assert.equal((await team.tasks.update(task, { state: "todo" })).ticket, 1);
    assert.equal(await team.tasks.claimTicket(task), 1);
  }));

// The same for the ticket for moving a task on: two agents could both take
// it, and both make the next pair.
test("a doubled takeover ticket is never taken", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await bob.tasks.claim(task, undefined, { lease: 1 }), true);
    const takeover = team.client.boundedCounter(names.taskTakeover(team.name, task.id, 0));
    await takeover.increment(1);
    assert.equal(await bob.tasks.release(task), false);
    await assert.rejects(team.tasks.update(task, { state: "todo" }), TicketUnavailable);
    // Taking the task over is refused, and says that only force helps.
    await team.tasks.get(task);
    await sleep(1200);
    await assert.rejects(
      team.tasks.claim(task),
      (error: Error) => error instanceof TicketUnavailable && /made twice.*force: true/.test(error.message),
    );
    assert.equal(await takeover.value(), 2);
    assert.ok((await team.tasks.update(task, { state: "todo", force: true })).ticket > 1);
  }));

// The number on a firm task never goes down, whatever reaches the board late.
// Bob holds ticket 0 on a machine that is cut off, so its ticket for moving on
// cannot be had, and the task is set back with force. Carol claims the forced
// number. Bob's machine comes back, and bob, who has not heard, gives the
// task back.
async function forcedWhileBobWasAway(team: Team, bob: Team): Promise<{ id: string; bobsView: unknown; forced: number }> {
  const task = await team.tasks.add("Send the invoice", { firm: true });
  assert.equal(await bob.tasks.claimTicket(task), 0);
  const bobsView = await onBoard(team, task.id);
  const takeover = team.client.boundedCounter(names.taskTakeover(team.name, task.id, 0));
  assert.equal(await takeover.tryDecrement(1), true);
  const forced = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
  await takeover.increment(1);
  return { id: task.id, bobsView, forced };
}

test("a late release never takes a forced task back down", () =>
  withTeam(async (team, bob, sidecar) => {
    const { id, bobsView, forced } = await forcedWhileBobWasAway(team, bob);
    const carol = await teamOn(sidecar, "carol");
    const dave = await teamOn(sidecar, "dave");
    try {
      assert.equal(await carol.tasks.claimTicket(id), forced);
      await behind(id, bobsView, async () => assert.equal(await bob.tasks.release(id), false));
      assert.equal((await team.tasks.get(id))!.ticket, forced);
      assert.equal(await carol.tasks.stillHolding(id, forced), true);
      assert.equal(await dave.tasks.claimTicket(id), null);
    } finally {
      await carol.close();
      await dave.close();
    }
  }));

// The same, when bob's machine writes before it can hear of the force at all:
// its lower number lands on the board, and is not the task's number.
test("a lower number written late is not the task's number", () =>
  withTeam(async (team, bob, sidecar) => {
    const { id, bobsView, forced } = await forcedWhileBobWasAway(team, bob);
    const carol = await teamOn(sidecar, "carol");
    const dave = await teamOn(sidecar, "dave");
    try {
      assert.equal(await carol.tasks.claimTicket(id), forced);
      await behind(id, bobsView, () => bob.tasks.release(id), 2);
      assert.equal((await team.tasks.get(id))!.ticket, forced);
      assert.ok([null, forced].includes(await dave.tasks.claimTicket(id)));
      assert.ok([null, forced].includes(await carol.tasks.claimTicket(id)));
    } finally {
      await carol.close();
      await dave.close();
    }
  }));

// Bob takes the task over. Between bob writing the new number and reading it
// back, the task is set back with force and carol claims it. Bob lost, and
// moving on from his ticket must not put a lower number on the task.
test("a takeover that loses to a force never takes the number down", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claim(task, undefined, { lease: 1 }), true);
    await bob.tasks.get(task);
    await sleep(1200);
    const carol = await teamOn(sidecar, "carol");
    const dave = await teamOn(sidecar, "dave");
    let forced = 0;
    const batch = bob.client.batch;
    bob.client.batch = function (this: typeof bob.client) {
      const made = batch.call(this);
      const send = made.send;
      made.send = async function (this: typeof made) {
        const answer = await send.call(this);
        if (forced === 0 && ((await onBoard(team, task.id)) as { claimed_by?: string }).claimed_by === "bob") {
          // Bob has written ticket 1 and is about to read it back. Its ticket
          // for moving on is out of reach for a moment.
          const takeover = team.client.boundedCounter(names.taskTakeover(team.name, task.id, 1));
          assert.equal(await takeover.tryDecrement(1), true);
          forced = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
          await takeover.increment(1);
          assert.equal(await carol.tasks.claimTicket(task), forced);
        }
        return answer;
      } as typeof send;
      return made;
    } as typeof batch;
    try {
      assert.equal(await bob.tasks.claimTicket(task), null);
      bob.client.batch = batch;
      assert.ok(forced > 1);
      assert.equal((await team.tasks.get(task))!.ticket, forced);
      assert.equal(await carol.tasks.stillHolding(task, forced), true);
      assert.equal(await dave.tasks.claimTicket(task), null);
    } finally {
      bob.client.batch = batch;
      await carol.close();
      await dave.close();
    }
  }));

// The same across languages: the lower number a Node agent wrote late is not
// the task's number to a Python agent either, and is not handed out.
test("a lower number written late in Node is not the task's number in Python", () =>
  withTeam(async (team, bob, sidecar) => {
    const { id, bobsView, forced } = await forcedWhileBobWasAway(team, bob);
    const carol = await teamOn(sidecar, "carol");
    try {
      assert.equal(await carol.tasks.claimTicket(id), forced);
      await behind(id, bobsView, () => bob.tasks.release(id), 2);
    } finally {
      await carol.close();
    }
    const seen = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    print(team.tasks.get(${JSON.stringify(id)}).ticket, team.tasks.claim_ticket(${JSON.stringify(id)}) in (None, ${forced}))`,
    ).trim();
    assert.equal(seen, `${forced} True`);
  }));

test("force does nothing more when it is not needed", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    await bob.tasks.claim(task);
    assert.equal((await team.tasks.update(task, { state: "todo", force: true })).ticket, 1);
    const draft = await team.tasks.add("Draft");
    assert.equal((await team.tasks.update(draft, { state: "todo", force: true })).state, "todo");
  }));

test("force is only for setting a task back, and is true or false", () =>
  withTeam(async (team) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    const wrong = (error: Error) => error instanceof BadRequest && /force/.test(error.message);
    await assert.rejects(team.tasks.update(task, { state: "todo", force: "yes" as unknown as boolean }), wrong);
    await assert.rejects(team.tasks.update(task, { notes: "again", force: true }), wrong);
    await assert.rejects(team.tasks.update(task, { state: "done", force: true }), wrong);
    assert.equal((await team.tasks.get(task))!.notes, null);
  }));

// ---- with Python agents ------------------------------------------------------

// A firm task added by a Python agent is firm for a Node agent too, and a
// claim one of them abandons is taken over by the other.
test("firm tasks and claims that run out are shared with Python", () =>
  withTeam(async (team, bob, sidecar) => {
    const id = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    task = team.tasks.add("Send the invoice", firm=True)
    team.tasks.claim(task, lease=1)
    print(task.id)`,
    ).trim();
    // What bob sees, said with a failure, so that one that happens only now
    // and then can be told apart next time.
    const seen = async () => JSON.stringify(await bob.tasks.get(id));
    const found = (await team.tasks.get(id))!;
    assert.deepEqual([found.firm, found.claimedBy, found.lease], [true, "pia", 1]);
    assert.equal(await bob.tasks.claim(id), false, await seen());
    await sleep(1200);
    assert.equal(await bob.tasks.claim(id), true, await seen());
    assert.equal((await team.tasks.get(id))!.ticket, 1, await seen());
    const record = python(
      sidecar.socket,
      `import json, sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    print(json.dumps(team.tasks.get(${JSON.stringify(id)}).taken_over))`,
    ).trim();
    assert.deepEqual(JSON.parse(record), (await team.tasks.get(id))!.takenOver);
    assert.deepEqual([JSON.parse(record).by, JSON.parse(record).from], ["bob", "pia"]);
    assert.equal(await bob.tasks.stillHolding(id, 1), true, await seen());
    const holding = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    print(team.tasks.still_holding(${JSON.stringify(id)}, 0), team.tasks.still_holding(${JSON.stringify(id)}, 1))`,
    ).trim();
    assert.equal(holding, "False False", await seen());
    await bob.tasks.release(id);
    const working = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    print(team.tasks.still_working(${JSON.stringify(id)}), team.tasks.claim(${JSON.stringify(id)}))`,
    ).trim();
    assert.equal(working, "False True", await seen());
    assert.equal(await bob.tasks.claim(id), false, await seen());
  }));

// A Python operator sets a stuck firm task back with force, and a Node agent
// claims it on the fresh ticket. A Node operator who forces it again from
// what it read before lands past the Python one's pair, whichever language
// made it.
test("a firm task set back with force in Python is free for Node", () =>
  withTeam(async (team, bob, sidecar) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    assert.equal(await team.tasks.claim(task), true);
    const stale = await onBoard(team, task.id);
    await stoppedHolding(team, names.taskTakeover(team.name, task.id, 0));
    const seen = python(
      sidecar.socket,
      `import sys, orvu
with orvu.Team.connect(socket=sys.argv[1], me="pia") as team:
    try:
        team.tasks.update(${JSON.stringify(task.id)}, state="todo")
    except orvu.TicketUnavailable:
        print(team.tasks.update(${JSON.stringify(task.id)}, state="todo", force=True).ticket)`,
    ).trim();
    const forced = Number(seen);
    assert.ok(forced > 2, seen);
    assert.equal(await team.tasks.stillWorking(task), false);
    const free = await onBoard(team, task.id);
    let again = 0;
    await behind(task.id, stale, async () => {
      again = (await team.tasks.update(task, { state: "todo", force: true })).ticket;
    });
    assert.ok(again - forced >= 2 ** 31, `${again} after ${forced}`);
    assert.equal(await bob.tasks.claimTicket(task), again);
    noneTwice([again, ...(await twoClaims(sidecar, task.id, free))]);
  }));

// Another agent is setting a firm task back at the same moment, and has its
// takeover ticket. This one is told, and nothing changes.
test("a firm task whose takeover ticket is taken cannot be set back", () =>
  withTeam(async (team, bob) => {
    const task = await team.tasks.add("Send the invoice", { firm: true });
    await bob.tasks.claim(task);
    assert.equal(await team.client.boundedCounter(names.taskTakeover(team.name, task.id, 0)).tryDecrement(1), true);
    await assert.rejects(
      team.tasks.update(task, { state: "todo", notes: "again" }),
      (error: Error) => error instanceof TicketUnavailable && error.message.includes(task.id),
    );
    const found = (await team.tasks.get(task))!;
    assert.deepEqual([found.state, found.claimedBy, found.notes, found.ticket], ["doing", "bob", null, 0]);
  }));
