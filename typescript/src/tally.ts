/**
 * Tallies: counting things, per agent and in total.
 *
 * @module
 */

import { amount } from "./budget.ts";
import type { WatchOptions } from "./handles.ts";
import { together } from "./follow.ts";
import * as names from "./names.ts";
import type { Team } from "./team.ts";

/**
 * One count the team keeps, such as tokens used or errors seen. Get it from
 * `team.tally(name)`.
 *
 * ```ts
 * const used = team.tally("tokens-used");
 * await used.add(1500);
 * await used.total(); // 1500
 * await used.byName(); // { "researcher-2": 1500 }
 * ```
 *
 * Every machine counts by itself and the counts add up. Nothing is lost when
 * two agents count at the same moment, or while machines are cut off from
 * each other. A tally only goes up. For an amount that is spent and must not
 * run out, use a budget.
 */
export class Tally {
  /** What is counted under when no name is given and the agent has none. */
  static readonly NOBODY = "(no name)";
  readonly #team: Team;
  /** The tally's name. */
  readonly name: string;

  /** @internal */
  constructor(team: Team, name: string) {
    this.#team = team;
    this.name = names.part(name, "the name of a tally");
  }

  get #cart() {
    return this.#team.client.cart(names.tally(this.#team.name, this.name));
  }

  /** Counts `n` more. It is counted under this agent's name, or under
   * `under` if that is given: any text, such as the kind of error. */
  async add(n = 1, under?: string): Promise<void> {
    amount(n, "what a tally counts up by");
    if (under !== undefined) {
      names.agent(under, "what a count goes under");
    }
    await this.#team.record(`tally/${this.name}`, "cart");
    await this.#cart.add(under || this.#team.me || Tally.NOBODY, n);
  }

  /** Everything counted so far. */
  async total(): Promise<number> {
    return Object.values(await this.#cart.items()).reduce((sum, n) => sum + n, 0);
  }

  /** What was counted under each name. */
  byName(): Promise<Record<string, number>> {
    return this.#cart.items();
  }

  /** The counts by name now, and again each time one changes. Use it in a
   * `for await` loop. */
  watch(options: WatchOptions = {}): AsyncGenerator<Record<string, number>> {
    return together(this.#team.client, [this.#cart], () => this.byName(), options.signal);
  }
}
