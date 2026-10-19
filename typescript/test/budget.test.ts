// A budget, and a tally.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest, Tally } from "../src/index.ts";
import { teamOn, withTeam } from "./support.ts";

test("a budget is topped up and spent", () =>
  withTeam(async (team) => {
    const tokens = team.budget("tokens");
    assert.equal(await tokens.left(), 0);
    await tokens.topUp(10);
    assert.equal(await tokens.spend(4), true);
    assert.equal(await tokens.left(), 6);
    await tokens.topUp(1);
    assert.equal(await tokens.left(), 7);
  }));

test("spending more than is left is refused and changes nothing", () =>
  withTeam(async (team) => {
    const tokens = team.budget("tokens");
    await tokens.topUp(10);
    assert.equal(await tokens.spend(11), false);
    assert.equal(await tokens.left(), 10);
    assert.equal(await tokens.spend(10), true);
    assert.equal(await tokens.spend(1), false);
    assert.equal(await tokens.left(), 0);
  }));

test("spending from a budget nobody topped up is refused", () =>
  withTeam(async (team) => {
    assert.equal(await team.budget("tokens").spend(1), false);
    assert.equal(await team.budget("tokens").left(), 0);
    assert.deepEqual(await team.index(), {});
  }));

test("budgets with different names are separate", () =>
  withTeam(async (team) => {
    await team.budget("tokens").topUp(10);
    await team.budget("money").topUp(3);
    assert.equal(await team.budget("money").spend(5), false);
    assert.equal(await team.budget("tokens").spend(5), true);
    assert.deepEqual([await team.budget("tokens").left(), await team.budget("money").left()], [5, 3]);
  }));

test("an amount has to be a whole number above zero", () =>
  withTeam(async (team) => {
    const tokens = team.budget("tokens");
    await tokens.topUp(10);
    for (const amount of [0, -1, 1.5, "5", null, true, NaN, Infinity]) {
      await assert.rejects(tokens.topUp(amount as number), (error: Error) => error instanceof BadRequest && /whole number/.test(error.message));
      await assert.rejects(tokens.spend(amount as number), (error: Error) => error instanceof BadRequest && /whole number/.test(error.message));
    }
    await assert.rejects(tokens.topUp(2 ** 60), (error: Error) => error instanceof BadRequest && /too large/.test(error.message));
    assert.equal(await tokens.left(), 10);
  }));

test("a budget needs a plain name", () =>
  withTeam(async (team) => {
    for (const name of ["", "a/b", "two words", null]) {
      assert.throws(() => team.budget(name as string), (error: Error) => error instanceof BadRequest && /name of a budget/.test(error.message));
    }
  }));

// Eight agents spend from one budget as fast as they can. However the
// spends fall, they cannot get more out of it than was put in.
test("agents spending at once cannot overspend", () =>
  withTeam(async (team, _bob, sidecar) => {
    await team.budget("tokens").topUp(100);
    const spent: number[] = [];
    await Promise.all(
      Array.from({ length: 8 }, async (_, number) => {
        const mine = await teamOn(sidecar, `agent-${number}`);
        for (let i = 0; i < 10; i++) {
          if (await mine.budget("tokens").spend(3)) {
            spent.push(3);
          }
        }
        await mine.close();
      }),
    );
    assert.equal(spent.reduce((a, b) => a + b, 0), 99, "33 spends of 3 fit into 100, and no more");
    assert.equal(await team.budget("tokens").left(), 1);
  }));

test("a budget says how much each agent spent", () =>
  withTeam(async (team, bob, sidecar) => {
    const tokens = team.budget("tokens");
    assert.deepEqual(await tokens.spentBy(), {});
    await tokens.topUp(100);
    assert.ok((await tokens.spend(30)) && (await bob.budget("tokens").spend(20)) && (await tokens.spend(5)));
    // What was refused is not counted.
    assert.equal(await bob.budget("tokens").spend(1000), false);
    assert.deepEqual(await tokens.spentBy(), { alice: 35, bob: 20 });
    assert.equal(await bob.budget("tokens").left(), 45);
    const nameless = await teamOn(sidecar);
    assert.equal(await nameless.budget("tokens").spend(5), true);
    await nameless.close();
    assert.deepEqual(await tokens.spentBy(), { alice: 35, bob: 20 });
    assert.equal((await team.index())["budget-spent/tokens"], "cart");
  }));

test("a budget says how much each agent topped it up by", () =>
  withTeam(async (team, bob, sidecar) => {
    const tokens = team.budget("tokens");
    assert.deepEqual(await tokens.toppedUpBy(), {});
    await tokens.topUp(100);
    await bob.budget("tokens").topUp(20);
    await tokens.topUp(5);
    assert.deepEqual(await tokens.toppedUpBy(), { alice: 105, bob: 20 });
    const nameless = await teamOn(sidecar);
    await nameless.budget("tokens").topUp(7);
    await nameless.close();
    assert.deepEqual(await tokens.toppedUpBy(), { alice: 105, bob: 20 });
    assert.equal(await tokens.left(), 132);
    assert.equal((await team.index())["budget-topped/tokens"], "cart");
  }));

test("a tally counts per agent and in total", () =>
  withTeam(async (team, bob) => {
    const used = team.tally("tokens-used");
    assert.equal(await used.total(), 0);
    assert.deepEqual(await used.byName(), {});
    await used.add(1500);
    await used.add();
    await bob.tally("tokens-used").add(200);
    assert.deepEqual(await used.byName(), { alice: 1501, bob: 200 });
    assert.equal(await bob.tally("tokens-used").total(), 1701);
  }));

test("a count can go under any name", () =>
  withTeam(async (team, _bob, sidecar) => {
    const errors = team.tally("errors");
    await errors.add(1, "timeout");
    await errors.add(2, "bad reply");
    await errors.add(1, "timeout");
    assert.deepEqual(await errors.byName(), { timeout: 2, "bad reply": 2 });
    const nameless = await teamOn(sidecar);
    await nameless.tally("errors").add(3);
    await nameless.close();
    assert.equal((await errors.byName())[Tally.NOBODY], 3);
    assert.equal(await errors.total(), 7);
  }));

test("a tally only goes up by whole numbers, under some text", () =>
  withTeam(async (team) => {
    for (const n of [0, -1, 1.5, "2", true, null]) {
      await assert.rejects(team.tally("errors").add(n as number), (error: Error) => error instanceof BadRequest && /whole number above zero/.test(error.message));
    }
    for (const under of ["", 7, ["a"]]) {
      await assert.rejects(team.tally("errors").add(1, under as string), (error: Error) => error instanceof BadRequest && /some text/.test(error.message));
    }
    assert.equal(await team.tally("errors").total(), 0);
    assert.deepEqual(await team.index(), {});
  }));

test("the team lists its tallies", () =>
  withTeam(async (team) => {
    assert.deepEqual(await team.tallies(), []);
    await team.tally("errors").total();
    assert.deepEqual(await team.tallies(), []);
    await team.tally("errors").add();
    await team.tally("tokens-used").add(5);
    assert.deepEqual(await team.tallies(), ["errors", "tokens-used"]);
    assert.throws(() => team.tally("two words"), (error: Error) => error instanceof BadRequest && /name of a tally/.test(error.message));
  }));

// Many agents counting at the same moment lose nothing.
test("nothing is lost when many count at once", () =>
  withTeam(async (team, _bob, sidecar) => {
    await Promise.all(
      Array.from({ length: 8 }, async (_, number) => {
        const mine = await teamOn(sidecar, `agent-${number}`);
        await Promise.all(Array.from({ length: 25 }, () => mine.tally("done").add()));
        await mine.close();
      }),
    );
    assert.equal(await team.tally("done").total(), 200);
    assert.deepEqual(new Set(Object.values(await team.tally("done").byName())), new Set([25]));
  }));
