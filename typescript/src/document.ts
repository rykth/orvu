/**
 * Documents: longer text that several agents write together.
 *
 * @module
 */

import { RichText } from "./handles.ts";
import * as names from "./names.ts";
import type { Team } from "./team.ts";
import * as wire from "./wire.ts";
import { Op } from "./wire.ts";

/**
 * One shared text, such as a report or a plan. Get it from
 * `team.document(name)`.
 *
 * ```ts
 * const report = team.document("q3-report");
 * await report.append("Sales rose 4%.\n");
 * await report.text(); // "Sales rose 4%.\n"
 * ```
 *
 * Agents can insert and delete at any position at the same moment, and every
 * edit is kept. The merge is mechanical: two agents that rewrite the same
 * sentence get both versions tangled together.
 *
 * It works as an ordinary `RichText` does, with two things added. What an
 * agent inserts is marked with its name, and the document is written down in
 * the team's index the first time it is written to. The text and its mark go
 * in together, so the mark lands on exactly that text even when another agent
 * changes the same document at the same moment.
 */
export class Document extends RichText {
  /** The mark that says which agent wrote a piece of the text. */
  static readonly BY = "by";
  // Past the end of any text, which the engine takes to mean "at the end".
  static readonly #END = 2 ** 31;
  readonly #team: Team;
  readonly #shortName: string;

  /** @internal */
  constructor(team: Team, name: string) {
    const checked = names.part(name, "the name of a document");
    super(team.client, names.document(team.name, checked));
    this.#team = team;
    this.#shortName = `doc/${checked}`;
  }

  /** Inserts `text` at position `index`. Past the end means at the end. The
   * text is marked as written by this agent, if it has a name. */
  override insert(index: number, text: string): Promise<void> {
    return this.#put(index, text);
  }

  /** Adds `text` at the end. */
  append(text: string): Promise<void> {
    return this.#put(null, text);
  }

  async #put(index: number | null, text: string): Promise<void> {
    if (text === "") {
      // Nothing to put in. It does not start a document either.
      return;
    }
    if (this.#team.me === null) {
      await this.#team.record(this.#shortName, "richtext");
      return super.insert(index ?? Document.#END, text);
    }
    // The text and its mark go in as one change, and the mark is tied to the
    // characters themselves, so another agent writing at the same moment can
    // neither take the mark nor leave this text without it. Nothing is read
    // first: the end is found where the change is made.
    await this.#team.record(this.#shortName, "richtext");
    const message = {
      object: this.name,
      index: wire.count(index ?? Document.#END, "index"),
      text: wire.str(text, "text"),
      attr: Document.BY,
      value: this.#team.me,
    };
    await this.#team.client.run(new Op("richtext_insert_marked", message, wire.nothing));
  }

  override async delete(index: number, count: number): Promise<void> {
    await this.#team.record(this.#shortName, "richtext");
    return super.delete(index, count);
  }

  override async format(index: number, count: number, attr: string, value: string): Promise<void> {
    await this.#team.record(this.#shortName, "richtext");
    return super.format(index, count, attr, value);
  }

  override async unformat(index: number, count: number, attr: string): Promise<void> {
    await this.#team.record(this.#shortName, "richtext");
    return super.unformat(index, count, attr);
  }

  /** The text in pieces, each with the agent that wrote it:
   * `[["Sales rose 4%.", "researcher-2"], [" Costs fell.", null]]`. `null`
   * is text from a program that gave no name. */
  async writtenBy(): Promise<[string, string | null][]> {
    const pieces: [string, string | null][] = [];
    for (const span of await this.toSpans()) {
      const who = span.marks[Document.BY] ?? null;
      const last = pieces.at(-1);
      if (last !== undefined && last[1] === who) {
        last[0] += span.text;
      } else {
        pieces.push([span.text, who]);
      }
    }
    return pieces;
  }
}
