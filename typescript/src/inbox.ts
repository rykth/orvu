/**
 * The inbox: messages from one agent to another, or to the whole team.
 *
 * @module
 */

import { randomBytes } from "node:crypto";
import { BadRequest } from "./errors.ts";
import type { TextList, WatchOptions } from "./handles.ts";
import { together } from "./follow.ts";
import * as names from "./names.ts";
import { sent, type Team } from "./team.ts";
import { countTextRecord, json } from "./wire.ts";

/** Checks that a message has something in it. */
function body(value: unknown): unknown {
  if (value === null || value === undefined) {
    throw new BadRequest("a message needs something in it");
  }
  return value;
}

/** One message, as it was sent. */
export interface Message {
  /** Made up when the message is sent. No two messages share one. */
  id: string;
  /** The agent that sent it, or `null` if the sender gave no name. */
  sender: string | null;
  /** The agent it was sent to, or `null` if it was sent to everyone. */
  to: string | null;
  /** What was sent: text, or anything else JSON can hold. */
  body: unknown;
  /** When it was sent, in milliseconds since 1970, by the clock of the
   * machine that sent it. */
  sent: number;
}

/**
 * Messages between the agents of a team. Get it from `team.inbox`.
 *
 * ```ts
 * await team.inbox.send("researcher-2", "The Q3 numbers are in the notes.");
 * await team.inbox.sendAll("Deploy is on Friday.");
 * for (const message of await team.inbox.unread()) {
 *   console.log(message.sender, message.body);
 * }
 * ```
 *
 * An agent reads what was sent to it and what was sent to everyone. A
 * message is not taken out by being read. `unread` writes down, under the
 * agent's name, what the agent has been given, so that it is given each
 * message once, also after its program has started again.
 *
 * An inbox is kept in pages, and a new page is started when one is full.
 * Reading starts at the newest pages, so a long history does not make it
 * slow.
 */
export class Inbox {
  /** How many messages go on a page. */
  page = 100;
  /** How many of the newest pages `messages` looks at, and `unread` the
   * first time. */
  recent = 2;
  /** How many older pages `unread` reads again at most on each look, and
   * `watch` follows. The others wait their turn. */
  behind = 8;
  /** How long, in milliseconds, an older page is let be after it was read
   * again and nothing new was found on it. The wait doubles each time nothing
   * is found again, up to `longestWait`. */
  wait = 1000;
  /** The longest wait between two reads of an older page, in milliseconds. */
  longestWait = 60_000;
  /** @internal The clock the waits are measured by, in milliseconds. */
  clock: () => number = () => performance.now();
  readonly #team: Team;
  // The ids of the messages given, by inbox and page. Only the pages `unread`
  // still looks at are kept.
  #seen = new Map<string, { owner: string; number: number; ids: Set<string> }>();
  // For each inbox, the first page `unread` has not finished with.
  readonly #from = new Map<string, number>();
  // The older pages whose count is above how far this agent got on them, by
  // inbox and page: a message was written there that has not arrived here
  // yet, or the count is wrong. When each is read next.
  readonly #later = new Map<string, Later>();
  // Up to `behind` of those pages, as inbox and page number, that `watch`
  // follows, to see at once when the message arrives.
  #waiting: [string, number][] = [];

  /** @internal */
  constructor(team: Team) {
    this.#team = team;
  }

  // ---- where things are kept -----------------------------------------------

  #pages(owner: string) {
    return this.#team.client.counter(names.inboxPages(this.#team.name, owner));
  }

  #page(owner: string, number: number) {
    return this.#team.client.list(names.inboxPage(this.#team.name, owner, number));
  }

  #counts(owner: string) {
    return this.#team.client.cart(names.inboxCount(this.#team.name, owner));
  }

  #done(owner: string) {
    const reader = names.inboxOwner(this.#team.me!);
    return this.#team.client.map(names.inboxDone(this.#team.name, reader, owner));
  }

  /** The inboxes this agent reads. */
  #mine(): string[] {
    const me = this.#team.me;
    return me === null ? [names.EVERYONE] : [names.inboxOwner(me), names.EVERYONE];
  }

  // ---- sending -------------------------------------------------------------

  async #send(owner: string, to: string | null, body: unknown): Promise<void> {
    const kept = json(
      { id: "m-" + randomBytes(8).toString("hex"), from: this.#team.me, to, body, sent: Date.now() },
      "that message",
    );
    let number = await this.#newest(owner);
    let page = this.#page(owner, number);
    let length = await page.length();
    if (length >= this.page) {
      // This page is full. If two agents find that at the same moment both
      // move on, and a page number is skipped. Reading goes by the numbers
      // and not by a count of pages, so nothing is passed over for it.
      await this.#team.record(`inbox-page/${owner}`, "counter");
      await this.#pages(owner).increment(1);
      number = await this.#pages(owner).value();
      page = this.#page(owner, number);
      length = await page.length();
    }
    await this.#team.record(`inbox/${owner}/${number}`, "list");
    await this.#team.record(`inbox-count/${owner}`, "cart");
    // The page's count goes up with the message, in the same message to the
    // sidecar. A reader that sees the count of a page it has finished go up
    // knows a message was written there late.
    const batch = this.#team.client.batch();
    batch.list(page.name).insert(length, kept);
    batch.cart(this.#counts(owner).name).add(String(number), 1);
    sent(await batch.send());
  }

  /** Sends `body` to the agent called `to`. It is there for that agent
   * whenever it next reads, on any machine. */
  async send(to: string, message: unknown): Promise<void> {
    body(message);
    const name = names.agent(to, "the name of the agent to send to");
    await this.#send(names.inboxOwner(name), name, message);
  }

  /** Sends `body` to every agent on the team. */
  async sendAll(message: unknown): Promise<void> {
    await this.#send(names.EVERYONE, null, body(message));
  }

  // ---- reading -------------------------------------------------------------

  /** The message kept as `kept`, or `null` if that is not one. */
  static #message(kept: string): Message | null {
    let fields: Record<string, unknown>;
    try {
      fields = JSON.parse(kept);
    } catch {
      return null;
    }
    if (typeof fields !== "object" || fields === null || Array.isArray(fields) || !("body" in fields)) {
      return null;
    }
    // Messages are put in order by these two, so they have to be what they
    // should be.
    const { id, sent } = fields;
    if (typeof id !== "string" || typeof sent !== "number" || !Number.isInteger(sent)) {
      return null;
    }
    const text = (value: unknown) => (typeof value === "string" ? value : null);
    return { id, sender: text(fields.from), to: text(fields.to), body: fields.body, sent };
  }

  #newest(owner: string): Promise<number> {
    return this.#pages(owner).value();
  }

  /** The messages on the pages `first` to `newest` of one inbox. What is
   * there and is not something this block wrote is left out. */
  async #read(owner: string, first: number, newest: number): Promise<Message[]> {
    const found: Message[] = [];
    for (let number = Math.max(first, 0); number <= newest; number++) {
      for (const kept of await this.#page(owner, number).toArray()) {
        const message = Inbox.#message(kept);
        if (message !== null) {
          found.push(message);
        }
      }
    }
    return found;
  }

  static #inOrder(messages: Message[]): Message[] {
    return messages.sort((a, b) => a.sent - b.sent || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  }

  /** The recent messages to this agent and to everyone, oldest first: what
   * is on the newest two pages of each. */
  async messages(): Promise<Message[]> {
    const found: Message[] = [];
    for (const owner of this.#mine()) {
      const newest = await this.#newest(owner);
      found.push(...(await this.#read(owner, newest - this.recent + 1, newest)));
    }
    return Inbox.#inOrder(found);
  }

  /** Every message to this agent and to everyone that was ever sent, oldest
   * first. This reads every page. */
  async history(): Promise<Message[]> {
    const found: Message[] = [];
    for (const owner of this.#mine()) {
      found.push(...(await this.#read(owner, 0, await this.#newest(owner))));
    }
    return Inbox.#inOrder(found);
  }

  /** The ids of the messages on one page that this agent has been given, as
   * the team keeps them. */
  #given(owner: string, number: number) {
    const reader = names.inboxOwner(this.#team.me!);
    return this.#team.client.set(names.inboxGiven(this.#team.name, reader, owner, number));
  }

  /**
   * The messages this agent has not been given yet, oldest first. The first
   * time, that is the recent ones. After that it is whatever has arrived
   * since, however much that is.
   *
   * What an agent with a name was given is written down for the team, so a
   * program that starts again under that name is not given it a second time.
   * A program with no name remembers only for as long as it runs.
   *
   * An agent with a name is also given a message that a machine cut off for
   * long wrote late to a page the inbox had moved past, once. Each page keeps
   * a count of the messages written to it, and the agent keeps a mark of how
   * many it was given from each page. The counts are only hints. A page whose
   * count is above the mark, or cannot be right, is read again until the mark
   * catches up: at once, and then, while nothing new is found there, after a
   * wait that starts at `wait` and doubles up to `longestWait`. A change to
   * the page's count starts the waits over. At most `behind` such pages are
   * read on each look, those read longest ago first, so none is passed over
   * for good. A late message is always given, within about `longestWait` at
   * worst, to a program that keeps looking: through `watch`, or by calling
   * `unread` regularly. With more than `behind` such pages waiting, a program
   * that calls `unread` only now and then needs several calls to reach them
   * all, about one call for each `behind` pages. Anyone who may send can add to a page's count without writing
   * there; a count that then stays as it is costs one more read of that page
   * each `longestWait`.
   */
  async unread(): Promise<Message[]> {
    const named = this.#team.me !== null;
    const owners = this.#mine();
    const batch = this.#team.client.batch();
    const newest = owners.map((owner) => batch.counter(this.#pages(owner).name).value());
    const counts = named ? owners.map((owner) => batch.cart(this.#counts(owner).name).itemsAsText()) : [];
    const done = named ? owners.map((owner) => batch.map(this.#done(owner).name).entries()) : [];
    await batch.send();
    const clock = this.clock();
    const looks = owners.map((owner, at) => {
      const last = newest[at].value;
      const recent = last - this.recent + 1;
      // Every page from where the last look left off, so that a burst that
      // filled several pages is not passed over. The newest pages are looked
      // at again each time, because a machine that has not heard of a new
      // page yet still writes on the one before.
      const first = Math.max(this.#from.get(owner) ?? recent, 0);
      const [written, odd] = named ? counted(counts[at].value) : [new Map<number, number>(), new Map<number, string>()];
      const given = named ? numbered(done[at].value) : new Map<number, number>();
      if (named) {
        // Pages behind, written to since this agent was given all it had.
        // Only pages this agent has read are looked at, and not the history
        // from before it first read.
        this.#behind(owner, given, written, odd, first, clock);
      }
      return { owner, last, recent, first, written, odd, given };
    });
    const again = named ? this.#due(clock) : [];
    const fresh: Message[] = [];
    const marks: [string, number, string][] = [];
    const settled: [string, number, number][] = [];
    for (const { owner, last, recent, first, written, odd, given } of looks) {
      const behind = again.filter(([of]) => of === owner).map(([, number]) => number);
      const before = new Map(given);
      const pages = [...behind];
      for (let number = first; number <= last; number++) {
        pages.push(number);
      }
      for (const number of pages) {
        const onPage = await this.#read(owner, number, number);
        const key = `${owner}/${number}`;
        let seen = this.#seen.get(key);
        if (seen === undefined) {
          seen = { owner, number, ids: new Set() };
          this.#seen.set(key, seen);
        }
        const ids = seen.ids;
        let found = onPage.filter((message) => !ids.has(message.id));
        if (found.length > 0 && named) {
          const already = await this.#given(owner, number).elements();
          found.filter((message) => already.has(message.id)).forEach((message) => ids.add(message.id));
          found = found.filter((message) => !already.has(message.id));
          marks.push(...found.map((message): [string, number, string] => [owner, number, message.id]));
        }
        if (named) {
          // How far this agent got on the page: what was written to it, as
          // far as it has arrived here.
          const now = odd.has(number) ? onPage.length : Math.min(written.get(number) ?? 0, onPage.length);
          if (given.get(number) !== now) {
            settled.push([owner, number, now]);
          }
          given.set(number, now);
        }
        found.forEach((message) => ids.add(message.id));
        fresh.push(...found);
      }
      this.#from.set(owner, Math.max(first, recent, 0));
      if (named) {
        for (const number of behind) {
          this.#readAgain(owner, number, given.get(number)! > (before.get(number) ?? 0), clock);
        }
        this.#behind(owner, given, written, odd, this.#from.get(owner)!, clock);
      }
    }
    // What is remembered of pages that are finished is let go.
    this.#seen = new Map([...this.#seen].filter(([, page]) => page.number >= (this.#from.get(page.owner) ?? 0)));
    // The pages `watch` follows: those to be read soonest.
    this.#waiting = [...this.#later.values()]
      .sort((a, b) => a.due - b.due || a.read - b.read || byPage(a, b))
      .slice(0, this.behind)
      .sort(byPage)
      .map((later): [string, number] => [later.owner, later.number]);
    if (marks.length > 0 || settled.length > 0) {
      const reader = names.inboxOwner(this.#team.me!);
      for (const owner of [...new Set(settled.map(([owner]) => owner))].sort()) {
        await this.#team.record(`inbox-done/${reader}/${owner}`, "map");
      }
      const batch = this.#team.client.batch();
      for (const [owner, number, id] of marks) {
        batch.set(this.#given(owner, number).name).add(id);
      }
      for (const [owner, number, now] of settled) {
        batch.map(this.#done(owner).name).put(String(number), String(now));
      }
      // If what was given cannot be written down, the messages are not given
      // now either. They are given next time, rather than given now and given
      // again after a restart.
      try {
        sent(await batch.send());
      } catch (error) {
        for (const [owner, number, id] of marks) {
          this.#seen.get(`${owner}/${number}`)?.ids.delete(id);
        }
        throw error;
      }
      const pages = [...new Set(marks.map(([owner, number]) => `${owner}/${number}`))].sort();
      for (const page of pages) {
        await this.#team.record(`inbox-given/${reader}/${page}`, "set");
      }
    }
    return Inbox.#inOrder(fresh);
  }

  /** Brings up to date which older pages of one inbox, before `below`, are to
   * be read again: those whose count is above how far this agent got on them,
   * and those whose count cannot be right, which may hide a message however
   * far it got. A page new to the list, or whose count changed, is read at
   * once. Pages no longer behind are let go. */
  #behind(owner: string, given: Map<number, number>, written: Map<number, number>, odd: Map<number, string>, below: number, clock: number): void {
    const behind = new Map<number, string>();
    for (const [number, got] of given) {
      if (number < below && (odd.has(number) || (written.get(number) ?? 0) > got)) {
        behind.set(number, odd.get(number) ?? String(written.get(number)));
      }
    }
    for (const [key, later] of this.#later) {
      if (later.owner === owner && !behind.has(later.number)) {
        this.#later.delete(key);
      }
    }
    for (const [number, count] of behind) {
      const later = this.#later.get(`${owner}/${number}`);
      if (later === undefined) {
        this.#later.set(`${owner}/${number}`, { owner, number, count, due: clock, read: -Infinity, step: 0, reads: 0 });
      } else if (later.count !== count) {
        // It keeps when it was last read, so that changing a count does not
        // put a page ahead of the others.
        Object.assign(later, { count, due: clock, step: 0 });
      }
    }
  }

  /** The older pages to read again on this look, as inbox and page: at most
   * `behind` of those whose wait is over, those read longest ago first. */
  #due(clock: number): [string, number][] {
    return [...this.#later.values()]
      .filter((later) => later.due <= clock)
      .sort((a, b) => a.read - b.read || byPage(a, b))
      .slice(0, this.behind)
      .sort(byPage)
      .map((later) => [later.owner, later.number]);
  }

  /** Sets when an older page that was just read is read next: at once if
   * something new was found on it, and after a growing wait if not. */
  #readAgain(owner: string, number: number, found: boolean, clock: number): void {
    const later = this.#later.get(`${owner}/${number}`);
    if (later === undefined) {
      return;
    }
    later.read = clock;
    later.reads++;
    later.step = found ? 0 : later.step + 1;
    later.due = found ? clock : clock + Math.min(this.wait * 2 ** (later.step - 1), this.longestWait);
  }

  /** Has the older pages `pages` read on the next look, as something arrived
   * on them. */
  #soon(pages: [string, number][]): void {
    const clock = this.clock();
    for (const [owner, number] of pages) {
      const later = this.#later.get(`${owner}/${number}`);
      if (later !== undefined) {
        later.due = clock;
        later.step = 0;
      }
    }
  }

  /** For `watch`: the older pages whose wait is over, each with how often it
   * was read, and how long until the next wait is over. */
  #waits(): { due: [string, number, number][]; next: number | null } {
    const clock = this.clock();
    const all = [...this.#later.values()].sort(byPage);
    const due = all.filter((later) => later.due <= clock).map((later): [string, number, number] => [later.owner, later.number, later.reads]);
    const next = all.length === 0 ? null : Math.max(all.reduce((soonest, later) => Math.min(soonest, later.due), Infinity) - clock, 0);
    return { due, next };
  }

  /** Each message this program has not been given yet, and then each new one
   * as it arrives. Use it in a `for await` loop. */
  async *watch(options: WatchOptions = {}): AsyncGenerator<Message> {
    const named = this.#team.me !== null;
    for (;;) {
      const owners = this.#mine();
      const numbers = await Promise.all(owners.map((owner) => this.#newest(owner)));
      const pages = owners.map((owner) => this.#pages(owner));
      // The newest pages of each inbox, as `unread` looks at them, and the
      // older pages whose count says a message was written there that has
      // not arrived here yet. The count can come before the message, and
      // then the page itself has to be followed until it does.
      const behind = this.#waiting;
      const recent: TextList[] = [];
      owners.forEach((owner, at) => {
        for (let number = Math.max(numbers[at] - this.recent + 1, 0); number <= numbers[at]; number++) {
          recent.push(this.#page(owner, number));
        }
      });
      recent.push(...behind.map(([owner, number]) => this.#page(owner, number)));
      // The counts of messages per page. They go up when a message is written
      // to an older page too, which `unread` then reads again.
      const counts = named ? owners.map((owner) => this.#counts(owner)) : [];
      // The page numbers are part of what is followed. A sender moves an
      // inbox to a new page first and writes on it afterwards, and the move
      // by itself has to be noticed, or the new page would never be followed.
      // The length of a page is enough to see that a message came: the
      // messages are read by `unread`.
      const state = async () => {
        const batch = this.#team.client.batch();
        const now = pages.map((page) => batch.counter(page.name).value());
        const lengths = recent.map((page) => batch.list(page.name).length());
        const written = counts.map((count) => batch.cart(count.name).itemsAsText());
        await batch.send();
        // The older pages whose wait is over are part of it too, so that the
        // wait being over is noticed like a change.
        return {
          numbers: now.map((read) => read.value),
          lengths: lengths.map((read) => read.value),
          written: written.map((read) => read.value),
          due: this.#waits().due,
        };
      };
      // The counts are followed as text: a count too large for a JavaScript
      // number, which no sender wrote, must not stop the following.
      const client = this.#team.client;
      const countsAsText = counts.map((count) => ({
        watch: (options: { signal?: AbortSignal }) => client.watch("KIND_CART", count.name, countTextRecord, options.signal),
      }));
      const watched: { watch(options: { signal?: AbortSignal }): AsyncIterable<unknown> }[] = [...pages, ...recent, ...countsAsText];
      let moved = false;
      let grownFrom: number[] | null = null;
      // The first value comes at once, and a new one each time a page or a
      // page number changes, or the wait for an older page is over.
      const later = () => this.#waits().next;
      for await (const now of together(this.#team.client, watched, state, options.signal, later)) {
        // An older page that grew is read on this look, whatever its wait.
        if (grownFrom !== null && behind.length > 0) {
          const was = grownFrom.slice(-behind.length);
          this.#soon(behind.filter((_, at) => was[at] !== now.lengths[now.lengths.length - behind.length + at]));
        }
        grownFrom = now.lengths;
        for (const message of await this.unread()) {
          yield message;
        }
        if (now.numbers.some((number, at) => number !== numbers[at]) || JSON.stringify(this.#waiting) !== JSON.stringify(behind)) {
          // A new page was started, or an older page is waited on or caught
          // up with. What is followed changes from here.
          moved = true;
          break;
        }
      }
      if (!moved) {
        return;
      }
    }
  }
}

/** When an older page that is behind is read next. */
interface Later {
  owner: string;
  number: number;
  /** The page's count as it read, as text, as it may not be a number. */
  count: string;
  /** When it is read next, by `Inbox.clock`. */
  due: number;
  /** When it was last read, or never. */
  read: number;
  /** How many times in a row it was read with nothing new found. */
  step: number;
  /** How many times it was read in all. */
  reads: number;
}

/** Puts pages in order by inbox, then by number. */
function byPage(a: { owner: string; number: number }, b: { owner: string; number: number }): number {
  return a.owner < b.owner ? -1 : a.owner > b.owner ? 1 : a.number - b.number;
}

/** More messages than any page is ever written. A count above it, or below
 * zero, was not made by senders adding one each. */
const MOST = 1_000_000;

/** The counts of messages per page, by page number, and apart from them the
 * counts that cannot be right, as they read. */
function counted(counts: Record<string, string>): [Map<number, number>, Map<number, string>] {
  const good = new Map<number, number>();
  const odd = new Map<number, string>();
  const whole = /^[0-9]+$/;
  for (const [key, value] of Object.entries(counts)) {
    if (!whole.test(key)) {
      continue;
    }
    if (whole.test(value) && Number(value) <= MOST) {
      good.set(Number(key), Number(value));
    } else {
      odd.set(Number(key), value);
    }
  }
  return [good, odd];
}

/** The entries of a map or cart whose keys are page numbers, by number. What
 * is not a number is left out. */
function numbered(entries: Record<string, string | number>): Map<number, number> {
  const found = new Map<number, number>();
  const whole = /^[0-9]+$/;
  for (const [key, value] of Object.entries(entries)) {
    if (whole.test(key) && whole.test(String(value))) {
      found.set(Number(key), Number(value));
    }
  }
  return found;
}
