// The inbox: messages between the agents of a team.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest, ClassMismatch, type Message, type Team } from "../src/index.ts";
import { sleep, teamOn, waitUntil, withTeam } from "./support.ts";

const bodies = (messages: Message[]) => messages.map((message) => message.body);

test("a message reaches the agent it was sent to", () =>
  withTeam(async (team, bob, sidecar) => {
    await team.inbox.send("bob", "The Q3 numbers are in the notes.");
    const [message, ...rest] = await bob.inbox.unread();
    assert.deepEqual(rest, []);
    assert.deepEqual([message.sender, message.to, message.body], ["alice", "bob", "The Q3 numbers are in the notes."]);
    assert.ok(message.id.startsWith("m-") && message.sent > 0);
    // It was for bob. Nobody else is given it.
    assert.deepEqual(await team.inbox.unread(), []);
    const carol = await teamOn(sidecar, "carol");
    assert.deepEqual(await carol.inbox.messages(), []);
    await carol.close();
  }));

test("a message to everyone reaches everyone", () =>
  withTeam(async (team, bob, sidecar) => {
    await team.inbox.sendAll("Deploy is on Friday.");
    for (const reader of [team, bob]) {
      const [message] = await reader.inbox.unread();
      assert.deepEqual([message.sender, message.to, message.body], ["alice", null, "Deploy is on Friday."]);
    }
    const nameless = await teamOn(sidecar);
    assert.deepEqual(bodies(await nameless.inbox.messages()), ["Deploy is on Friday."]);
    await nameless.inbox.send("bob", "from nobody in particular");
    await nameless.close();
    assert.equal((await bob.inbox.unread())[0].sender, null);
  }));

test("unread gives each message once and messages gives them all", () =>
  withTeam(async (team, bob) => {
    await team.inbox.send("bob", "one");
    await team.inbox.sendAll("two");
    assert.deepEqual(bodies(await bob.inbox.unread()), ["one", "two"]);
    assert.deepEqual(await bob.inbox.unread(), []);
    await team.inbox.send("bob", "three");
    assert.deepEqual(bodies(await bob.inbox.unread()), ["three"]);
    assert.deepEqual(bodies(await bob.inbox.messages()), ["one", "two", "three"]);
  }));

// A message is whatever JSON can hold, and comes back as it was sent.
test("a message holds any value", () =>
  withTeam(async (team, bob) => {
    for (const body of ["text", 7, 2.5, true, ["a", 1], { task: "t-1", done: false }, "ü 🙂"]) {
      await team.inbox.send("bob", body);
      const [message] = await bob.inbox.unread();
      assert.deepEqual(message.body, body);
    }
  }));

test("what cannot be sent is refused", () =>
  withTeam(async (team, bob) => {
    for (const body of [null, undefined, new Set([1]), NaN]) {
      await assert.rejects(team.inbox.send("bob", body), BadRequest);
      await assert.rejects(team.inbox.sendAll(body), BadRequest);
    }
    assert.deepEqual(await bob.inbox.messages(), []);
    assert.deepEqual(await team.index(), {});
  }));

// A full page is left behind and a new one is started. Reading looks at the
// newest two pages, and the whole history is still there for who asks.
test("a long inbox starts a new page", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 5;
    for (let number = 0; number < 17; number++) {
      await team.inbox.send("bob", number);
    }
    assert.equal(await team.client.counter("orvu/default/inbox-page/bob").value(), 3);
    const lengths = await Promise.all([0, 1, 2, 3].map((page) => team.client.list(`orvu/default/inbox/bob/${page}`).length()));
    assert.deepEqual(lengths, [5, 5, 5, 2]);
    // Each page counts the messages written to it.
    assert.deepEqual(await team.client.cart("orvu/default/inbox-count/bob").items(), { "0": 5, "1": 5, "2": 5, "3": 2 });
    const range = (from: number, to: number) => Array.from({ length: to - from }, (_, at) => from + at);
    assert.deepEqual(bodies(await bob.inbox.messages()), range(10, 17));
    assert.deepEqual(bodies(await bob.inbox.history()), range(0, 17));
    assert.deepEqual(bodies(await bob.inbox.unread()), range(10, 17));
  }));

test("nothing is lost when many send at once", () =>
  withTeam(async (_team, bob, sidecar) => {
    await Promise.all(
      Array.from({ length: 6 }, async (_, number) => {
        const mine = await teamOn(sidecar, `agent-${number}`);
        mine.inbox.page = 10;
        for (let n = 0; n < 10; n++) {
          await mine.inbox.send("bob", `agent-${number}-${n}`);
        }
        await mine.close();
      }),
    );
    bob.inbox.page = 10;
    const all = bodies(await bob.inbox.history()) as string[];
    assert.equal(all.length, 60);
    assert.equal(new Set(all).size, 60);
    // Each sender's own messages are in the order it sent them.
    for (let number = 0; number < 6; number++) {
      const own = all.filter((body) => body.startsWith(`agent-${number}-`));
      assert.deepEqual(own, Array.from({ length: 10 }, (_, n) => `agent-${number}-${n}`));
    }
  }));

test("messages are watched as they arrive", () =>
  withTeam(async (team, bob) => {
    await team.inbox.send("bob", "before");
    const got: unknown[] = [];
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 4) {
          return;
        }
      }
    })();
    assert.ok(await waitUntil(() => got.length === 1));
    await team.inbox.send("bob", "one");
    await team.inbox.sendAll("two");
    assert.ok(await waitUntil(() => got.length === 3));
    await team.inbox.send("bob", "three");
    await listen;
    assert.deepEqual(got, ["before", "one", "two", "three"]);
  }));

// The watch goes on when the inbox moves to a new page.
test("a watch follows the inbox onto a new page", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 3;
    const got: unknown[] = [];
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 8) {
          return;
        }
      }
    })();
    for (let number = 0; number < 8; number++) {
      await team.inbox.send("bob", number);
      assert.ok(await waitUntil(() => got.length === number + 1), `${number}: ${got}`);
    }
    await listen;
    assert.deepEqual(got, [0, 1, 2, 3, 4, 5, 6, 7]);
  }));

test("the index lists the inboxes that were written to", () =>
  withTeam(async (team) => {
    await team.inbox.messages();
    assert.deepEqual(await team.index(), {});
    await team.inbox.send("bob", "hello");
    await team.inbox.sendAll("hello");
    assert.deepEqual(await team.index(), {
      "inbox/bob/0": "list",
      "inbox-count/bob": "cart",
      "inbox/@everyone/0": "list",
      "inbox-count/@everyone": "cart",
    });
    team.inbox.page = 1;
    await team.inbox.send("bob", "and again");
    assert.deepEqual(await team.index(), {
      "inbox/bob/0": "list",
      "inbox/bob/1": "list",
      "inbox-page/bob": "counter",
      "inbox-count/bob": "cart",
      "inbox/@everyone/0": "list",
      "inbox-count/@everyone": "cart",
    });
  }));

// However many messages arrive between two looks, none is passed over.
test("a burst of messages between two reads is all given", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 5;
    for (let n = 0; n < 3; n++) {
      await team.inbox.send("bob", n);
    }
    assert.deepEqual(bodies(await bob.inbox.unread()), [0, 1, 2]);
    for (let n = 3; n < 30; n++) {
      await team.inbox.send("bob", n);
    }
    assert.deepEqual(bodies(await bob.inbox.unread()), Array.from({ length: 27 }, (_, at) => at + 3));
    assert.deepEqual(await bob.inbox.unread(), []);
  }));

// Any name an agent can have works for its inbox.
test("an agent with any name has an inbox", () =>
  withTeam(async (team, _bob, sidecar) => {
    for (const name of ["ops/alice", "@everyone", "x".repeat(200), "a b", "../x", "%41", "ålice smith"]) {
      const other = await teamOn(sidecar, name);
      await team.inbox.send(name, "hello");
      assert.deepEqual(bodies(await other.inbox.unread()), ["hello"], name);
      await other.close();
    }
  }));

test("who a message goes to is some text", () =>
  withTeam(async (team) => {
    for (const to of ["", null, 7]) {
      await assert.rejects(team.inbox.send(to as string, "hello"), (error: Error) => error instanceof BadRequest && /agent to send to/.test(error.message));
    }
    assert.deepEqual(await team.index(), {});
  }));

// Something in an inbox that this block did not write is passed over.
test("what is not a message is passed over", () =>
  withTeam(async (team, bob) => {
    await team.inbox.send("bob", "one");
    const page = team.client.list("orvu/default/inbox/bob/0");
    for (const junk of ["not json", "[1, 2]", JSON.stringify({ id: "m-x", body: "no time", sent: "soon" }), JSON.stringify({ id: 7, body: "x", sent: 1 })]) {
      await page.insert(await page.length(), junk);
    }
    await team.inbox.send("bob", "two");
    assert.deepEqual(bodies(await bob.inbox.unread()), ["one", "two"]);
  }));

// What an agent has been given is written down for the team. A program that
// starts again is not given it a second time.
test("an agent that starts again is not given a message twice", () =>
  withTeam(async (team, _bob, sidecar) => {
    await team.inbox.send("carl", "please redeploy staging");
    await team.inbox.sendAll("Deploy is on Friday.");
    let carl = await teamOn(sidecar, "carl");
    assert.deepEqual(bodies(await carl.inbox.unread()), ["please redeploy staging", "Deploy is on Friday."]);
    await carl.close();
    await team.inbox.send("carl", "and production");
    carl = await teamOn(sidecar, "carl");
    assert.deepEqual(bodies(await carl.inbox.unread()), ["and production"]);
    assert.deepEqual(await carl.inbox.unread(), []);
    assert.equal((await carl.inbox.messages()).length, 3);
    await carl.close();
    const dora = await teamOn(sidecar, "dora");
    assert.deepEqual(bodies(await dora.inbox.unread()), ["Deploy is on Friday."]);
    await dora.close();
  }));

// What an agent was given is written down so that it is not given it again
// after a restart. If that cannot be written, `unread` says so, and gives the
// messages again next time, rather than give them now and lose the record.
test("an inbox that cannot write down what it gave says so", () =>
  withTeam(async (team, bob) => {
    await team.inbox.send("bob", "hello");
    // The record's name is taken for other rules. Reading it still works,
    // and writing to it is refused.
    await team.client.declare("orvu/default/inbox-given/bob/bob/0", "escrow");
    await assert.rejects(bob.inbox.unread(), ClassMismatch);
    await assert.rejects(bob.inbox.unread(), ClassMismatch);
  }));

/** Writes a message to a page of bob's inbox as a sender on a machine that
 * has not heard of any later page would: the message, and one more on the
 * page's count, in one batch. Or the message alone, when its count came
 * first. */
async function writeLate(team: Team, page: number, body: string, counted = true): Promise<void> {
  const message = { id: `m-late-${page}-${body.length}`, from: "carol", to: "bob", body, sent: Date.now() };
  const list = team.client.list(`orvu/default/inbox/bob/${page}`);
  const batch = team.client.batch();
  batch.list(list.name).insert(await list.length(), JSON.stringify(message));
  if (counted) {
    batch.cart("orvu/default/inbox-count/bob").add(String(page), 1);
  }
  await batch.send();
}

/** Calls `unread` until it gives something, for at most a few seconds. */
async function unreadSoon(team: Team): Promise<unknown[]> {
  const got: unknown[] = [];
  await waitUntil(async () => {
    got.push(...bodies(await team.inbox.unread()));
    return got.length > 0;
  });
  return got;
}

/** Makes the count of one page of bob's inbox far too big. */
async function forgeBig(team: Team, page: number): Promise<void> {
  const batch = team.client.batch();
  batch.cart("orvu/default/inbox-count/bob").add(String(page), Number.MAX_SAFE_INTEGER);
  batch.cart("orvu/default/inbox-count/bob").add(String(page), Number.MAX_SAFE_INTEGER);
  await batch.send();
}

// A machine that was cut off for long goes on writing to the page it knew,
// after the inbox has moved on by more than a page. The message is given,
// once, and not again to a program that starts again.
test("a message written late to an old page is given once", () =>
  withTeam(async (team, bob, sidecar) => {
    team.inbox.page = bob.inbox.page = 2;
    for (let number = 0; number < 2; number++) {
      await team.inbox.send("bob", number);
    }
    assert.deepEqual(bodies(await bob.inbox.unread()), [0, 1]);
    for (let number = 2; number < 9; number++) {
      await team.inbox.send("bob", number);
    }
    assert.deepEqual(bodies(await bob.inbox.unread()), [2, 3, 4, 5, 6, 7, 8]);
    await writeLate(team, 0, "late, on page 0");
    assert.deepEqual(bodies(await bob.inbox.unread()), ["late, on page 0"]);
    assert.deepEqual(await bob.inbox.unread(), []);
    const again = await teamOn(sidecar, "bob");
    assert.deepEqual(await again.inbox.unread(), []);
    assert.ok(bodies(await again.inbox.history()).includes("late, on page 0"));
    await again.close();
  }));

// A program that follows the inbox is given such a message too, once.
test("a program that follows is given a late message once", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    const got: unknown[] = [];
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 11) {
          return;
        }
      }
    })();
    for (let number = 0; number < 9; number++) {
      await team.inbox.send("bob", number);
      assert.ok(await waitUntil(() => got.length === number + 1), `${number}: ${got}`);
    }
    await writeLate(team, 0, "late, on page 0");
    assert.ok(await waitUntil(() => got.length === 10), String(got));
    await team.inbox.send("bob", "after");
    await listen;
    assert.deepEqual(got.slice(9), ["late, on page 0", "after"]);
  }));

// Following an inbox looks at which page it is on and how long its pages are
// to see that something changed, and does not read every message for that.
test("following an inbox does not read its messages to see a change", () =>
  withTeam(async (team, bob) => {
    bob.inbox.messages = () => {
      throw new Error("the messages were read to see whether anything changed");
    };
    const got: unknown[] = [];
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 2) {
          return;
        }
      }
    })();
    await team.inbox.send("bob", "one");
    assert.ok(await waitUntil(() => got.length === 1), String(got));
    await team.inbox.sendAll("two");
    await listen;
    assert.deepEqual(got, ["one", "two"]);
  }));

// Once a page is finished, what an agent was given from it is not read again,
// nor added to: a mark for the whole page says how far the agent got.
test("a finished page is not looked at again", () =>
  withTeam(async (team, _bob, sidecar) => {
    team.inbox.page = 2;
    let bob = await teamOn(sidecar, "bob");
    bob.inbox.page = 2;
    for (let number = 0; number < 8; number++) {
      await team.inbox.send("bob", number);
      await bob.inbox.unread();
    }
    await bob.close();
    bob = await teamOn(sidecar, "bob");
    bob.inbox.page = 2;
    const asked: string[] = [];
    const client = bob.client as unknown as { call: (method: string, build: (session: string) => Record<string, unknown>) => Promise<unknown> };
    const call = client.call.bind(client);
    client.call = (method, build) => {
      const request = build("");
      for (const one of (method === "Batch" ? request.requests : [request]) as Record<string, { object?: string }>[]) {
        asked.push(...Object.values(one).map((op) => (typeof op === "object" && op !== null ? (op.object ?? "") : "")));
      }
      return call(method, build);
    };
    await team.inbox.send("bob", "new");
    assert.deepEqual(bodies(await bob.inbox.unread()), ["new"]);
    const finished = asked.filter((name) => /inbox-given\/bob\/bob\/[01]$/.test(name));
    assert.deepEqual(finished, []);
    await bob.close();
    const done = await team.client.map("orvu/default/inbox-done/bob/bob").entries();
    assert.deepEqual([done["0"], done["1"]], ["2", "2"]);
  }));

/** The names of the objects `team`'s client asks about, as it asks. */
function spy(team: Team): string[] {
  const asked: string[] = [];
  const client = team.client as unknown as { call: (method: string, build: (session: string) => Record<string, unknown>) => Promise<unknown> };
  const call = client.call.bind(client);
  client.call = (method, build) => {
    const request = build("");
    for (const one of (method === "Batch" ? request.requests : [request]) as Record<string, { object?: string }>[]) {
      asked.push(...Object.values(one).map((op) => (typeof op === "object" && op !== null ? (op.object ?? "") : "")));
    }
    return call(method, build);
  };
  return asked;
}

// A count a sender could not have written, far too big or below zero, hides
// no message. A count that big may also be stuck at the counter's limit, so
// that a late message does not change it: the page is read again, now and
// then, for as long as its count cannot be right.
for (const [what, forge] of [
  ["below zero", (team: Team) => team.client.cart("orvu/default/inbox-count/bob").removeQty("0", 1000)],
  ["far too big", async (team: Team) => {
    const batch = team.client.batch();
    batch.cart("orvu/default/inbox-count/bob").add("0", Number.MAX_SAFE_INTEGER);
    batch.cart("orvu/default/inbox-count/bob").add("0", Number.MAX_SAFE_INTEGER);
    await batch.send();
  }],
  ["at the counter's limit", forgeAtTheLimit],
] as const) {
  test(`a count ${what} hides no message`, () =>
    withTeam(async (team, bob) => {
      team.inbox.page = bob.inbox.page = 2;
      bob.inbox.wait = bob.inbox.longestWait = 50;
      for (let number = 0; number < 9; number++) {
        await team.inbox.send("bob", number);
        await bob.inbox.unread();
      }
      await forge(team);
      await bob.inbox.unread();
      await writeLate(team, 0, "late, on page 0");
      assert.deepEqual(await unreadSoon(bob), ["late, on page 0"]);
      const asked = spy(bob);
      assert.deepEqual(await bob.inbox.unread(), []);
      assert.ok(asked.includes("orvu/default/inbox/bob/0"), String(asked));
      // A second late message on the page, whatever its count does.
      const later = { id: "m-later", from: "carol", to: "bob", body: "later, on page 0", sent: Date.now() };
      const page = team.client.list("orvu/default/inbox/bob/0");
      await page.insert(await page.length(), JSON.stringify(later));
      assert.deepEqual(await unreadSoon(bob), ["later, on page 0"]);
      assert.deepEqual(await bob.inbox.unread(), []);
    }));
}

/** Adds to the count of page 0 until it stops at the counter's limit. */
async function forgeAtTheLimit(team: Team): Promise<void> {
  for (let round = 0; round < 2; round++) {
    const batch = team.client.batch();
    for (let i = 0; i < 600; i++) {
      batch.cart("orvu/default/inbox-count/bob").add("0", Number.MAX_SAFE_INTEGER);
    }
    await batch.send();
  }
}

// A program that follows the inbox is given a late message on a page whose
// count cannot be right, even when the message leaves that count as it was.
test("a program that follows is given a late message on a page with a wrong count", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    const got: unknown[] = [];
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 10) {
          return;
        }
      }
    })();
    for (let number = 0; number < 9; number++) {
      await team.inbox.send("bob", number);
      assert.ok(await waitUntil(() => got.length === number + 1), `${number}: ${got}`);
    }
    await forgeAtTheLimit(team);
    await sleep(300);
    const message = { id: "m-late", from: "carol", to: "bob", body: "late, on page 0", sent: 1 };
    const page = team.client.list("orvu/default/inbox/bob/0");
    await page.insert(await page.length(), JSON.stringify(message));
    assert.ok(await waitUntil(() => got.length === 10), String(got));
    await listen;
    assert.deepEqual(got.slice(9), ["late, on page 0"]);
  }));

// A late message's count can arrive before the message does. However many
// times the page is read again before the message lands, it is given when it
// does.
test("a late message whose count came first is given after many looks", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    bob.inbox.wait = 50;
    bob.inbox.longestWait = 200;
    for (let number = 0; number < 9; number++) {
      await team.inbox.send("bob", number);
      await bob.inbox.unread();
    }
    await team.client.cart("orvu/default/inbox-count/bob").add("0", 1);
    for (let look = 0; look < 6; look++) {
      assert.deepEqual(await bob.inbox.unread(), []);
      await sleep(60);
    }
    await writeLate(team, 0, "late, on page 0", false);
    assert.deepEqual(await unreadSoon(bob), ["late, on page 0"]);
    assert.deepEqual(await bob.inbox.unread(), []);
  }));

// The same for a program that follows the inbox, while other messages keep it
// looking.
test("a program that follows is given a late message after many wakes", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    bob.inbox.wait = 50;
    bob.inbox.longestWait = 200;
    const got: unknown[] = [];
    let looks = 0;
    const unread = bob.inbox.unread.bind(bob.inbox);
    bob.inbox.unread = () => {
      looks++;
      return unread();
    };
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (message.body === "late, on page 0") {
          return;
        }
      }
    })();
    for (let number = 0; number < 9; number++) {
      await team.inbox.send("bob", number);
      assert.ok(await waitUntil(() => got.length === number + 1), `${number}: ${got}`);
    }
    await team.client.cart("orvu/default/inbox-count/bob").add("0", 1);
    // Other messages wake the program again and again.
    for (let number = 0; number < 8; number++) {
      const before = looks;
      await team.inbox.sendAll(`busy ${number}`);
      assert.ok(await waitUntil(() => looks > before));
      await sleep(100);
    }
    await writeLate(team, 0, "late, on page 0", false);
    assert.ok(await waitUntil(() => got.includes("late, on page 0")), String(got));
    await listen;
    assert.equal(got.filter((body) => body === "late, on page 0").length, 1);
  }));

// Anyone who may send can make the count of any page wrong. Wrong counts on
// many newer pages do not keep a real late message on an older page from
// being given.
for (const [what, forge] of [
  ["far too big", forgeBig],
  ["one too many", (team: Team, page: number) => team.client.cart("orvu/default/inbox-count/bob").add(String(page), 1)],
] as const) {
  test(`counts ${what} on newer pages do not hold back a late message`, () =>
    withTeam(async (team, bob) => {
      team.inbox.page = bob.inbox.page = 2;
      bob.inbox.wait = 50;
      bob.inbox.longestWait = 200;
      for (let number = 0; number < 24; number++) {
        await team.inbox.send("bob", number);
        await bob.inbox.unread();
      }
      await writeLate(team, 1, "late, on page 1");
      for (let page = 2; page < 11; page++) {
        await forge(team, page);
      }
      assert.deepEqual(await unreadSoon(bob), ["late, on page 1"]);
    }));
}

test("a program that follows is not held back by wrong counts on newer pages", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    bob.inbox.wait = 50;
    bob.inbox.longestWait = 200;
    const got: unknown[] = [];
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 25) {
          return;
        }
      }
    })();
    for (let number = 0; number < 24; number++) {
      await team.inbox.send("bob", number);
      assert.ok(await waitUntil(() => got.length === number + 1), `${number}: ${got}`);
    }
    for (let page = 2; page < 11; page++) {
      await forgeBig(team, page);
    }
    await sleep(300);
    await writeLate(team, 1, "late, on page 1", false);
    await team.client.cart("orvu/default/inbox-count/bob").add("1", 1);
    assert.ok(await waitUntil(() => got.length === 25), String(got));
    await listen;
    assert.deepEqual(got.slice(24), ["late, on page 1"]);
  }));

// A program that follows the inbox follows at most `behind` older pages. A
// late message on another one is given when its wait is over, with nothing
// else to wake the program.
test("a program that follows reads the pages it does not follow when their wait is over", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    bob.inbox.behind = 1;
    bob.inbox.wait = 50;
    bob.inbox.longestWait = 200;
    // Which older pages are followed, as the program starts following them.
    const followed: string[] = [];
    const client = bob.client as unknown as { watch: (kind: string, name: string, ...rest: unknown[]) => AsyncGenerator<unknown> };
    const watch = client.watch.bind(client);
    client.watch = (kind, name, ...rest) => {
      if (/^orvu\/default\/inbox\/bob\/[01]$/.test(name)) {
        followed.push(name);
      }
      return watch(kind, name, ...rest);
    };
    const got: unknown[] = [];
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 10) {
          return;
        }
      }
    })();
    for (let number = 0; number < 9; number++) {
      await team.inbox.send("bob", number);
      assert.ok(await waitUntil(() => got.length === number + 1), `${number}: ${got}`);
    }
    const counts = team.client.cart("orvu/default/inbox-count/bob");
    await counts.add("0", 1);
    await counts.add("1", 1);
    assert.ok(await waitUntil(() => followed.length > 0));
    await sleep(300);
    // The message lands on the page not followed just now.
    const page = followed.at(-1)!.endsWith("/0") ? 1 : 0;
    await writeLate(team, page, "late", false);
    assert.ok(await waitUntil(() => got.length === 10), String(got));
    await listen;
    assert.deepEqual(got.slice(9), ["late"]);
  }));

// A page whose count says more than has arrived, or cannot be right, is read
// again, but with a growing wait between reads while nothing new is found
// there: at once, then after 1, 2 and 4 waits, and so on up to the longest
// wait. A change to its count starts it over. At most `behind` pages are read
// again on each look, those read longest ago first.
test("an old page with nothing new is read again only now and then", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    let tick = 0;
    bob.inbox.clock = () => 1000 + tick;
    bob.inbox.wait = 10;
    bob.inbox.longestWait = 40;
    for (let number = 0; number < 24; number++) {
      await team.inbox.send("bob", number);
      await bob.inbox.unread();
    }
    // Half the counts one too many, half far too big.
    for (let page = 0; page < 10; page++) {
      if (page % 2) {
        await team.client.cart("orvu/default/inbox-count/bob").add(String(page), 1);
      } else {
        await forgeBig(team, page);
      }
    }
    const asked = spy(bob);
    const oldPagesRead = async () => {
      asked.length = 0;
      assert.deepEqual(await bob.inbox.unread(), []);
      return asked
        .map((name) => /^orvu\/default\/inbox\/bob\/(\d+)$/.exec(name))
        .filter((found) => found !== null)
        .map((found) => Number(found![1]))
        .filter((page) => page < 10);
    };
    const looks: number[][] = [];
    const reads = new Map<number, number[]>();
    for (tick = 0; tick < 200; tick++) {
      const look = await oldPagesRead();
      looks.push(look);
      for (const page of look) {
        reads.set(page, [...(reads.get(page) ?? []), tick]);
      }
    }
    // Eight at the first look, and the two left over at the next.
    assert.deepEqual(looks.slice(0, 2), [[0, 1, 2, 3, 4, 5, 6, 7], [8, 9]]);
    for (let page = 0; page < 10; page++) {
      const first = page < 8 ? 0 : 1;
      assert.deepEqual(reads.get(page), [0, 10, 30, 70, 110, 150, 190].map((at) => at + first), `page ${page}`);
    }
    // A new count starts the page over.
    await team.client.cart("orvu/default/inbox-count/bob").add("3", 1);
    assert.deepEqual(await oldPagesRead(), [3]);
  }));

// A late message's count can arrive before the message does. A program that
// follows the inbox is given the message when it comes.
test("a program that follows is given a late message whose count came first", () =>
  withTeam(async (team, bob) => {
    team.inbox.page = bob.inbox.page = 2;
    // The page is followed, so the message is given when it lands, not when
    // the wait is over.
    bob.inbox.wait = bob.inbox.longestWait = 30_000;
    const got: unknown[] = [];
    let looks = 0;
    const unread = bob.inbox.unread.bind(bob.inbox);
    bob.inbox.unread = () => {
      looks++;
      return unread();
    };
    const listen = (async () => {
      for await (const message of bob.inbox.watch()) {
        got.push(message.body);
        if (got.length === 11) {
          return;
        }
      }
    })();
    for (let number = 0; number < 9; number++) {
      await team.inbox.send("bob", number);
      assert.ok(await waitUntil(() => got.length === number + 1), `${number}: ${got}`);
    }
    // The count goes up, and the program looks and finds nothing yet.
    const before = looks;
    await team.client.cart("orvu/default/inbox-count/bob").add("0", 1);
    assert.ok(await waitUntil(() => looks > before));
    await sleep(300);
    const message = { id: "m-late", from: "carol", to: "bob", body: "late, on page 0", sent: 1 };
    const page = team.client.list("orvu/default/inbox/bob/0");
    await page.insert(await page.length(), JSON.stringify(message));
    assert.ok(await waitUntil(() => got.length === 10), String(got));
    await team.inbox.send("bob", "after");
    await listen;
    assert.deepEqual(got.slice(9), ["late, on page 0", "after"]);
  }));
