/**
 * The result of one operation in a batch.
 *
 * @module
 */

import { BadRequest } from "./errors.ts";
import type { Message, Op } from "./wire.ts";

/** The result of one operation in a batch. It has no value until the batch
 * has been sent. */
export class Pending<T> {
  readonly #op: Op<T>;
  #response: Message | undefined;

  constructor(op: Op<T>) {
    this.#op = op;
  }

  /** @internal */
  get op(): Op<T> {
    return this.#op;
  }

  /** @internal */
  arrived(response: Message): void {
    this.#response = response;
  }

  /** What the operation returned. Throws the operation's error if it failed.
   * One operation failing does not undo the others in the batch. */
  get value(): T {
    if (this.#response === undefined) {
      throw new BadRequest("the batch has not been sent yet");
    }
    return this.#op.result(this.#response);
  }
}
