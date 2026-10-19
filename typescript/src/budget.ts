/**
 * A budget: a shared amount the team can never spend more of than there is.
 *
 * @module
 */

import { BadRequest, OrvuError } from "./errors.ts";
import type { WatchOptions } from "./handles.ts";
import { together } from "./follow.ts";
import * as names from "./names.ts";
import { sent, type Team } from "./team.ts";

/** An amount: a whole number above zero that can be sent. */
export function amount(n: unknown, what = "an amount"): number {
  if (typeof n !== "number" || !Number.isInteger(n) || n <= 0) {
    throw new BadRequest(`${what} has to be a whole number above zero, not ${JSON.stringify(n) ?? String(n)}`);
  }
  if (n > names.LARGEST) {
    throw new BadRequest(`${n} is too large to be ${what}`);
  }
  return n;
}

/**
 * One budget, such as tokens or money. Get it from `team.budget(name)`.
 *
 * ```ts
 * const tokens = team.budget("tokens");
 * await tokens.topUp(10_000);
 * if (await tokens.spend(1_500)) {
 *   // there was enough, and it is now spent
 * }
 * ```
 *
 * What is left can never go below zero, even when agents on several machines
 * spend at the same moment.
 */
export class Budget {
  readonly #team: Team;
  /** The budget's name. */
  readonly name: string;

  /** @internal */
  constructor(team: Team, name: string) {
    this.#team = team;
    this.name = names.part(name, "the name of a budget");
  }

  get #counter() {
    return this.#team.client.boundedCounter(names.budget(this.#team.name, this.name));
  }

  get #spent() {
    return this.#team.client.cart(names.spent(this.#team.name, this.name));
  }

  get #topped() {
    return this.#team.client.cart(names.topped(this.#team.name, this.name));
  }

  /** Adds `n` to the budget. An agent with a name is counted as having topped
   * it up by `n`, in the same message to the sidecar. */
  async topUp(n: number): Promise<void> {
    amount(n);
    await this.#team.record(`budget/${this.name}`, "bounded");
    const me = this.#team.me;
    if (me === null) {
      await this.#counter.increment(n);
      return;
    }
    await this.#team.record(`budget-topped/${this.name}`, "cart");
    const batch = this.#team.client.batch();
    batch.boundedCounter(this.#counter.name).increment(n);
    batch.cart(this.#topped.name).add(me, n);
    sent(await batch.send());
  }

  /** Takes `n` out of the budget. Gives `true` if there was enough. If there
   * was not, gives `false` and nothing is spent. */
  async spend(n: number): Promise<boolean> {
    if (!(await this.#counter.tryDecrement(amount(n)))) {
      return false;
    }
    // Written after the spend, and only if it went through. If the program
    // ends right here, or the sidecar goes away, the amount is spent and not
    // counted for anyone.
    if (this.#team.me !== null) {
      try {
        await this.#team.record(`budget-spent/${this.name}`, "cart");
        await this.#spent.add(this.#team.me, n);
      } catch (error) {
        // The amount is spent whether or not the name could be written.
        // Throwing here would tell the caller that the spend failed, and it
        // would spend again for the same work.
        if (!(error instanceof OrvuError)) {
          throw error;
        }
      }
    }
    return true;
  }

  /** How much each agent has spent from this budget. What a program that
   * gave no name spent is not in it. */
  spentBy(): Promise<Record<string, number>> {
    return this.#spent.items();
  }

  /** How much each agent has topped this budget up by. What a program that
   * gave no name added is not in it. */
  toppedUpBy(): Promise<Record<string, number>> {
    return this.#topped.items();
  }

  /** How much is left. */
  left(): Promise<number> {
    return this.#counter.value();
  }

  /** What is left now, and again each time it changes. Use it in a
   * `for await` loop. */
  watch(options: WatchOptions = {}): AsyncGenerator<number> {
    return together(this.#team.client, [this.#counter], () => this.left(), options.signal);
  }
}
