/**
 * Notes: small facts the team shares, each under a name.
 *
 * @module
 */

import { BadRequest, Clash } from "./errors.ts";
import { Json, type JsonPath, type WatchOptions } from "./handles.ts";
import { together } from "./follow.ts";
import * as names from "./names.ts";
import type { Team } from "./team.ts";
import { fromText, toText } from "./values.ts";

/** What comes between the writer's name and the note's text, in each value. */
const SEPARATOR = "\x1f";

/**
 * The team's notes. Get it from `team.notes`.
 *
 * ```ts
 * await team.notes.put("deploy-date", "Friday");
 * await team.notes.get("deploy-date"); // "Friday"
 * ```
 *
 * A simple note is a name and a value: text, a number, true or false, or an
 * array or object of those. If two agents put the same name at the same
 * moment, neither having seen the other's, the note keeps both values and
 * says so: `get` throws `Clash`, which holds them, until the next `put`
 * settles it. For a note with several fields that different agents fill in,
 * use `structured`.
 */
export class Notes {
  readonly #team: Team;

  /** @internal */
  constructor(team: Team) {
    this.#team = team;
  }

  get #map() {
    return this.#team.client.mvMap(names.notes(this.#team.name));
  }

  static #name(name: unknown): string {
    return names.agent(name, "a note's name");
  }

  /** Writes a note. `name` can be any text, and `value` anything JSON can
   * hold. It comes back from `get` as what it was: a number as a number, an
   * array as an array. It replaces every value the note holds, so it settles
   * a clash. */
  async put(name: string, value: unknown): Promise<void> {
    if (value === null || value === undefined) {
      throw new BadRequest("a note cannot hold nothing. Use `remove` to take a note away");
    }
    const me = this.#team.me ?? "";
    if (me.includes(SEPARATOR)) {
      throw new BadRequest(`an agent whose name holds ${JSON.stringify(SEPARATOR)} cannot write notes`);
    }
    const key = Notes.#name(name);
    const kept = toText(value);
    await this.#team.record("notes-mv", "mvmap");
    await this.#map.put(key, me + SEPARATOR + kept);
  }

  /** The note's value, or `null` if there is no such note. Throws `Clash` if
   * agents wrote it at the same time and nothing has settled it. */
  async get(name: string): Promise<unknown> {
    const key = Notes.#name(name);
    const found = settled(key, await this.#map.get(key));
    if (found instanceof Clash) {
      throw found;
    }
    return found;
  }

  /** Removes a note, every value it holds. Removing one that is not there
   * does nothing. */
  async remove(name: string): Promise<void> {
    await this.#map.remove(Notes.#name(name));
  }

  /** Every simple note, as an object of name to value. A note that agents
   * wrote at the same time is given as a `Clash`, which holds its values. */
  async all(): Promise<Record<string, unknown>> {
    const notes = await this.#notes();
    return Object.fromEntries(Object.entries(notes).map(([name, values]) => [name, settled(name, values)]));
  }

  /** The agent that wrote the note, or `null` if there is no such note or it
   * was written by a program that gave no name. For a note that clashes, an
   * array of who wrote each of its values. */
  async author(name: string): Promise<string | null | (string | null)[]> {
    return author(await this.#map.get(Notes.#name(name)));
  }

  /** Who wrote each note, as an object of note name to agent, or to an array
   * of agents for a note that clashes. */
  async authors(): Promise<Record<string, string | null | (string | null)[]>> {
    const notes = await this.#notes();
    return Object.fromEntries(Object.entries(notes).map(([name, values]) => [name, author(values)]));
  }

  /** All the simple notes now, and again each time one changes. Use it in a
   * `for await` loop. A note that clashes is given as a `Clash`. */
  watch(options: WatchOptions = {}): AsyncGenerator<Record<string, unknown>> {
    return together(this.#team.client, [this.#map], () => this.all(), options.signal);
  }

  async #notes(): Promise<Record<string, string[]>> {
    // The empty name marks that notes kept the old way were moved over.
    const entries = await this.#map.entries();
    return Object.fromEntries(Object.entries(entries).filter(([name]) => name !== ""));
  }

  /**
   * A note that is a JSON document, with fields and lists inside it. Two
   * agents that change different fields both keep their change.
   *
   * ```ts
   * const plan = team.notes.structured("plan");
   * await plan.set(["owner"], "researcher-2");
   * await plan.get(["owner"]); // "researcher-2"
   * ```
   *
   * What comes back works as an ordinary `Json` does.
   */
  structured(name: string): StructuredNote {
    return new StructuredNote(this.#team, names.part(name, "the name of a structured note"));
  }

  /** The names of the structured notes that have been written to. */
  structuredNames(): Promise<string[]> {
    return this.#team.under("note/");
  }
}

/** A structured note. It is a JSON document that is written down in the
 * team's index the first time something is written to it, so that only
 * looking at a note leaves no trace. */
export class StructuredNote extends Json {
  readonly #team: Team;
  readonly #shortName: string;

  /** @internal */
  constructor(team: Team, name: string) {
    super(team.client, names.note(team.name, name));
    this.#team = team;
    this.#shortName = `note/${name}`;
  }

  override async set(path: JsonPath, value: unknown): Promise<void> {
    await this.#team.record(this.#shortName, "json");
    return super.set(path, value);
  }

  override async remove(path: JsonPath): Promise<void> {
    await this.#team.record(this.#shortName, "json");
    return super.remove(path);
  }

  override async insert(path: JsonPath, index: number, value: unknown): Promise<void> {
    await this.#team.record(this.#shortName, "json");
    return super.insert(path, index, value);
  }
}

/** Each value of a note as it is kept, as who wrote it and its text, with one
 * entry for each different text, in the order the sidecar gives them. */
function written(values: string[]): [string | null, string][] {
  const seen = new Map<string, string | null>();
  for (const kept of values) {
    const at = kept.indexOf(SEPARATOR);
    const [by, text] = at < 0 ? ["", kept] : [kept.slice(0, at), kept.slice(at + 1)];
    if (!seen.has(text)) {
      seen.set(text, by === "" ? null : by);
    }
  }
  return [...seen].map(([text, by]) => [by, text]);
}

/** The note's value, `null` if it has none, or the `Clash` it is.
 * @internal */
export function settled(name: string, values: string[]): unknown {
  const found = written(values);
  if (found.length === 0) {
    return null;
  }
  if (found.length === 1) {
    return fromText(found[0]![1]);
  }
  return new Clash(name, found.map(([, text]) => fromText(text)), found.map(([by]) => by));
}

/** @internal */
export function author(values: string[]): string | null | (string | null)[] {
  const found = written(values);
  if (found.length === 0) {
    return null;
  }
  if (found.length === 1) {
    return found[0]![0];
  }
  return found.map(([by]) => by);
}
