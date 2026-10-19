/**
 * The roster: which agents are on the team, and which are there now.
 *
 * @module
 */

import { randomBytes } from "node:crypto";
import { BadRequest } from "./errors.ts";
import type { WatchOptions } from "./handles.ts";
import { together } from "./follow.ts";
import * as names from "./names.ts";
import type { Team } from "./team.ts";

// Stands between an agent's name and the word for one of its programs. It is
// a character no name is typed with.
const OWN = "\x1f";

// An entry silent for longer than this, in milliseconds, is not listed at
// all. It was left by a program that ended with no goodbye.
const FORGOTTEN_AFTER = 24 * 60 * 60 * 1000;

// The longest a Node timer waits. A longer one would go off at once.
const LONGEST_WAIT = 2 ** 31 - 1;

/** One agent on the roster, as it was when the roster was read. */
export interface Member {
  name: string;
  /** When it last said it was there, in milliseconds since 1970, by the
   * clock of its own machine. */
  lastSeen: number;
  /** True if that was a short while ago. */
  here: boolean;
}

/**
 * Who is on the team. Get it from `team.roster`.
 *
 * ```ts
 * await team.roster.join();
 * await team.roster.here(); // ["researcher-1", "researcher-2"]
 * ```
 *
 * An agent that has joined says "still here" every few seconds, in the
 * background. One that stopped saying so is still on the roster and is no
 * longer counted as here. That is how an agent that crashed fades out:
 * nothing else would take it off. What it left behind is cleared the next
 * time a program joins under its name, and after a day of silence it is not
 * listed at all.
 */
export class Roster {
  readonly #team: Team;
  #beat: NodeJS.Timeout | null = null;
  #mine: string | null = null;
  /** The "still here" on its way, if one is. */
  #writing: Promise<void> | null = null;
  /** The last join or leave asked for. Each waits for the one before, so that
   * a leave asked for while a join is still on its way undoes that join. */
  #turn: Promise<unknown> = Promise.resolve();
  /** How many seconds an agent may be silent and still count as here, if its
   * entry does not say how often it beats. */
  awayAfter = 45;

  /** @internal */
  constructor(team: Team) {
    this.#team = team;
  }

  get #map() {
    return this.#team.client.map(names.roster(this.#team.name));
  }

  /**
   * Puts this agent on the roster and keeps saying "still here" every `every`
   * seconds until `leave` or until the team is closed. An agent counts as
   * here for three times that long after it was last heard, whoever reads
   * the roster.
   *
   * The "still here" does not keep a program running: a program that has
   * nothing else to do ends, and fades from the roster.
   */
  join(every = 15): Promise<void> {
    return this.#inTurn(() => this.#join(every));
  }

  /** Runs `step` once every join and leave asked for before it is done. */
  #inTurn(step: () => Promise<void>): Promise<void> {
    const run = this.#turn.then(step, step);
    this.#turn = run.catch(() => {});
    return run;
  }

  async #join(every: number): Promise<void> {
    if (this.#team.me === null) {
      throw new BadRequest("an agent needs a name to join the roster: Team.connect(name, { me })");
    }
    if (typeof every !== "number" || !Number.isFinite(every) || every <= 0) {
      throw new BadRequest(`every is a number of seconds above zero, not ${JSON.stringify(every)}`);
    }
    await this.#leave();
    await this.#team.record("roster", "map");
    // Each program has an entry of its own, under the agent's name and a word
    // made up for this join. Two programs can run under one name, and one of
    // them leaving must not take the other off the roster.
    const mine = this.#team.me + OWN + randomBytes(4).toString("hex");
    // When, and how often. A reader needs the second to tell how long this
    // agent may be silent.
    const write = () => this.#map.put(mine, `${Date.now()} ${every}`);
    await write();
    this.#mine = mine;
    await this.#clearLeftBehind(mine);
    this.#beat = setInterval(() => {
      // One at a time: a beat that is still on its way is not doubled.
      if (this.#writing !== null) {
        return;
      }
      // The sidecar is away, or something else went wrong this once. The next
      // beat tries again. A heartbeat that stopped would make the agent look
      // gone while it runs.
      this.#writing = write()
        .catch(() => {})
        .finally(() => {
          this.#writing = null;
        });
      // A beat further apart than a timer can wait comes as late as one can.
      // It still says how often it beats, so readers judge it by that.
    }, Math.min(every * 1000, LONGEST_WAIT));
    this.#beat.unref();
  }

  /** Takes off the entries this agent's name has that are no longer here:
   * programs under this name that ended with no goodbye. Another program
   * under the name that still runs is left alone. */
  async #clearLeftBehind(mine: string): Promise<void> {
    const now = Date.now();
    try {
      for (const [key, kept] of Object.entries(await this.#map.entries())) {
        const cut = key.lastIndexOf(OWN);
        if (key !== mine && cut >= 0 && key.slice(0, cut) === this.#team.me && silent(now, kept, this.awayAfter)) {
          await this.#map.remove(key);
        }
      }
    } catch {
      // The entries are left for the next join. This agent is on the roster
      // all the same.
    }
  }

  /** Stops saying "still here" and takes this agent off the roster. Does
   * nothing if it had not joined. A join that is still on its way is
   * finished first, and then undone. */
  leave(): Promise<void> {
    return this.#inTurn(() => this.#leave());
  }

  async #leave(): Promise<void> {
    if (this.#beat === null) {
      return;
    }
    clearInterval(this.#beat);
    const mine = this.#mine!;
    this.#beat = this.#mine = null;
    // A "still here" on its way would put the agent back after it was taken
    // off, so it is waited for first.
    await this.#writing;
    await this.#map.remove(mine);
  }

  /** Everyone on the roster, by name. An agent that runs as several programs
   * is one member, and is here if any of them is. An agent not heard from for
   * more than a day is left out. */
  async members(): Promise<Member[]> {
    const now = Date.now();
    const found = new Map<string, Member>();
    for (const [key, kept] of Object.entries(await this.#map.entries())) {
      const cut = key.lastIndexOf(OWN);
      const name = cut >= 0 ? key.slice(0, cut) : key;
      const [seenText, everyText] = kept.split(" ", 2);
      const seen = Number(seenText);
      const silent = everyText ? 3 * Number(everyText) : this.awayAfter;
      if (!Number.isInteger(seen) || !Number.isFinite(silent)) {
        continue;
      }
      const here = now - seen <= silent * 1000;
      if (!here && now - seen > FORGOTTEN_AFTER) {
        continue;
      }
      let member: Member = { name, lastSeen: seen, here };
      const before = found.get(name);
      if (before !== undefined) {
        member = { name, lastSeen: Math.max(seen, before.lastSeen), here: member.here || before.here };
      }
      found.set(name, member);
    }
    return [...found.keys()].sort().map((name) => found.get(name)!);
  }

  /** The names of the agents that were heard from a short while ago. */
  async here(): Promise<string[]> {
    return (await this.members()).filter((member) => member.here).map((member) => member.name);
  }

  /** The names on the roster now, and again each time an agent joins or
   * leaves. Use it in a `for await` loop. An agent going silent does not show
   * here: read `here()` for that. */
  watch(options: WatchOptions = {}): AsyncGenerator<string[]> {
    return together(this.#team.client, [this.#map], async () => (await this.members()).map((member) => member.name), options.signal);
  }
}

/** True if the entry `kept` has not been heard from for longer than it may be
 * and still count as here. */
function silent(now: number, kept: string, awayAfter: number): boolean {
  const [seenText, everyText] = kept.split(" ", 2);
  const seen = Number(seenText);
  const allowed = everyText ? 3 * Number(everyText) : awayAfter;
  return Number.isInteger(seen) && Number.isFinite(allowed) && now - seen > allowed * 1000;
}
