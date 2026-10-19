/**
 * A team: the one object the building blocks hang off.
 *
 * @module
 */

import { Budget } from "./budget.ts";
import { connect, type Client, type ConnectOptions, type Found } from "./client.ts";
import { Document } from "./document.ts";
import { BadRequest, OrvuError } from "./errors.ts";
import type { TextMap } from "./handles.ts";
import { Inbox } from "./inbox.ts";
import * as names from "./names.ts";
import { Notes } from "./notes.ts";
import { Roster } from "./roster.ts";
import { Tally } from "./tally.ts";
import { Tasks } from "./tasks.ts";

/** How to connect to a team. */
export interface TeamOptions extends ConnectOptions {
  /** A name for the agent that is connecting, such as "researcher-2". What
   * it adds, claims, finishes and writes is marked with it. An agent with a
   * key, given as `key` or in `ORVU_KEY`, needs no `me`: it is the agent the
   * sidecar knows the key by, and so does one given a connection file, as
   * `connection` or in `ORVU_CONNECT`. */
  me?: string;
  /** The sidecar's socket. Left out, it is `ORVU_SOCKET` if that is set, and
   * otherwise this user's own sidecar, which is started if it is not
   * running. */
  socket?: string;
}

/**
 * Everything a team of agents shares. Every agent that connects with the same
 * team name sees the same notes, tasks and budgets, whether it is written in
 * TypeScript or in Python.
 *
 * ```ts
 * import { Team } from "orvu";
 *
 * const team = await Team.connect("default", { me: "researcher-2" });
 * await team.notes.put("deploy-date", "Friday");
 * const task = await team.tasks.add("Summarise the Q3 report");
 * if (await team.budget("tokens").spend(1500)) {
 *   // there was enough
 * }
 * await team.close();
 * ```
 */
export class Team {
  /** The team's name. */
  readonly name: string;
  /** The name of the agent using this object, or `null`. With a key, it is
   * the name the sidecar gave the key. */
  readonly me: string | null;
  /** The plain connection underneath, for anything the blocks do not cover. */
  readonly client: Client;
  /** The team's notes. */
  readonly notes: Notes;
  /** The team's task board. */
  readonly tasks: Tasks;
  /** Messages between the agents. */
  readonly inbox: Inbox;
  /** Who is on the team. */
  readonly roster: Roster;
  readonly #index: TextMap;
  readonly #recorded = new Set<string>();

  /** A team on a client that is connected already. */
  constructor(client: Client, name = "default", me: string | null = null) {
    this.name = names.part(name, "the name of a team");
    // A client that gave a key is the agent the sidecar said it is, and
    // cannot go by another name.
    if (client.agent !== null && me !== null && me !== client.agent) {
      throw new BadRequest(`this key belongs to the agent ${JSON.stringify(client.agent)}, not ${JSON.stringify(me)}`);
    }
    this.me = client.agent ?? me;
    this.client = client;
    this.#index = client.map(names.index(this.name));
    this.notes = new Notes(this);
    this.tasks = new Tasks(this);
    this.inbox = new Inbox(this);
    this.roster = new Roster(this);
  }

  /** Connects to the sidecar and gives the team called `name`. */
  static async connect(name = "default", options: TeamOptions = {}): Promise<Team> {
    // Checked first, so that a bad name does not start or reach a sidecar.
    names.part(name, "the name of a team");
    const { me, socket, ...settings } = options;
    const client = await connect(socket, settings);
    try {
      return new Team(client, name, me ?? null);
    } catch (error) {
      client.close();
      throw error;
    }
  }

  /** Takes this agent off the roster, if it had joined, and closes the
   * connection. */
  async close(): Promise<void> {
    try {
      await this.roster.leave();
    } catch (error) {
      // The sidecar is away. The agent fades from the roster by itself.
      if (!(error instanceof OrvuError)) {
        throw error;
      }
    }
    this.client.close();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    await this.close();
  }

  /** The budget called `name`, such as "tokens". */
  budget(name: string): Budget {
    return new Budget(this, name);
  }

  /** The document called `name`, such as "q3-report". */
  document(name: string): Document {
    return new Document(this, name);
  }

  /** The tally called `name`, such as "tokens-used". */
  tally(name: string): Tally {
    return new Tally(this, name);
  }

  /**
   * The team's notes, structured notes, paragraphs of documents and tasks
   * whose meaning is closest to `query`, the closest first, at most `limit`
   * of them, from 1 to 50. A task is found by its title and notes, under its
   * id. Messages are not searched.
   *
   * ```ts
   * for (const found of await team.search("what is the refund policy?")) {
   *   console.log(found.kind, found.name, found.text);
   * }
   * ```
   *
   * The last of what is found may have little to do with the question: its
   * `score` shows how close each is. A change shows up in what is found
   * within a moment. An agent with rules finds only what it may read. The
   * first search on a sidecar fetches the model it needs from the internet,
   * 30 MB, unless the sidecar was given one with `--model` or `ORVU_MODEL`;
   * `SearchUnavailable` says when it cannot be had.
   */
  search(query: string, limit = 5): Promise<Found[]> {
    return this.client.search(this.name, query, limit);
  }

  /** The names of the documents that have been written to. */
  async documents(): Promise<string[]> {
    return this.under("doc/");
  }

  /** The names of the tallies that have been counted on. */
  async tallies(): Promise<string[]> {
    return this.under("tally/");
  }

  /** The names of the budgets that have been topped up. */
  async budgets(): Promise<string[]> {
    return this.under("budget/");
  }

  /** @internal The short names in the index that start with `prefix`, with
   * the prefix taken off, sorted. */
  async under(prefix: string): Promise<string[]> {
    return Object.keys(await this.index())
      .filter((name) => name.startsWith(prefix))
      .map((name) => name.slice(prefix.length))
      .sort();
  }

  /**
   * What the team has, as an object of short name to kind of object:
   * `{ notes: "map", "budget/tokens": "bounded", ... }`.
   *
   * The sidecar cannot list the objects it holds, so the blocks write each
   * one down here the first time they create it.
   */
  index(): Promise<Record<string, string>> {
    return this.#index.entries();
  }

  /** @internal Writes an object down in the index, once per connection. */
  async record(shortName: string, kind: string): Promise<void> {
    if (!this.#recorded.has(shortName)) {
      await this.#index.put(shortName, kind);
      this.#recorded.add(shortName);
    }
  }

  /** @internal Takes objects that can never be used again off the index.
   * The objects themselves stay where they are: the sidecar cannot remove
   * one. */
  async forget(shortNames: string[]): Promise<void> {
    const batch = this.client.batch();
    for (const shortName of shortNames) {
      batch.map(this.#index.name).remove(shortName);
      this.#recorded.delete(shortName);
    }
    sent(await batch.send());
  }
}

/** Makes sure every write in a batch that was sent went through. A batch does
 * not stop at a write that failed, so each one is looked at, and the first
 * failure is thrown. */
export function sent(results: readonly { value: unknown }[]): void {
  for (const pending of results) {
    void pending.value;
  }
}
