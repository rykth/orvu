/**
 * Following several shared objects at once, as one loop.
 *
 * @module
 */

import { isDeepStrictEqual } from "node:util";

/** Anything that can be watched. */
interface Watchable {
  watch(options: { signal?: AbortSignal }): AsyncIterable<unknown>;
}

/**
 * Gives `snapshot()` now, and again each time any of `watched` changes and
 * the snapshot is different from the last one given.
 *
 * A plain `watch()` on an object that does not exist yet stays quiet until
 * the object appears. This always gives a first value at once, so a program
 * can show "nothing yet" without a special case.
 *
 * `later`, if given, says after how many milliseconds to take a new
 * snapshot even if nothing changed, or `null` for not until something does.
 *
 * The loop ends when it is left, when `signal` is aborted, or when `client` is
 * closed. Leaving it stops the watches underneath at once.
 */
export async function* together<T>(
  client: { readonly closed: boolean },
  watched: Watchable[],
  snapshot: () => Promise<T>,
  signal?: AbortSignal,
  later?: () => number | null,
): AsyncGenerator<T> {
  const stop = new AbortController();
  const ended = () => stop.abort();
  signal?.addEventListener("abort", ended, { once: true });
  let news = false;
  let over = false;
  let failure: unknown = null;
  let wake: (() => void) | null = null;
  const tell = () => {
    wake?.();
    wake = null;
  };
  // Each watch only says that something changed. Several changes that come
  // in while the caller is busy are all covered by one new snapshot.
  const pumps = watched.map(async (one) => {
    try {
      for await (const _ of one.watch({ signal: stop.signal })) {
        news = true;
        tell();
      }
      over = true;
    } catch (error) {
      failure ??= error;
    }
    tell();
  });
  const nothingYet = Symbol("nothing yet");
  let last: T | typeof nothingYet = nothingYet;
  try {
    for (;;) {
      // A closed client has no connection to read with. Its watches are
      // cancelled at once, and say they have ended only a moment later, so
      // the client is asked itself.
      if (signal?.aborted || client.closed) {
        return;
      }
      const value = await snapshot();
      if (last === nothingYet || !isDeepStrictEqual(value, last)) {
        last = value;
        yield value;
      }
      const wait = later?.() ?? null;
      const timer = wait === null ? null : setTimeout(() => {
        news = true;
        tell();
      }, wait);
      try {
        while (!news && !over && failure === null && !signal?.aborted) {
          await new Promise<void>((resolve) => (wake = resolve));
        }
      } finally {
        if (timer !== null) {
          clearTimeout(timer);
        }
      }
      if (failure !== null) {
        throw failure;
      }
      if (over || !news) {
        return;
      }
      news = false;
    }
  } finally {
    signal?.removeEventListener("abort", ended);
    stop.abort();
    await Promise.all(pumps);
  }
}
