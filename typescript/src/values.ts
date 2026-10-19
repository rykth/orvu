/**
 * Keeping any JSON value where the engine keeps text.
 *
 * Several kinds of shared object hold text and nothing else: the values of a
 * map, the items of a list. The blocks let a program put a number, true or
 * false, an array or an object there as well. Text is kept as it is.
 * Anything else is kept as its JSON form behind a mark that ordinary text
 * does not start with, so that reading can tell the two apart. This is the
 * form notes are kept in for every program, whatever its language: the
 * Python package and the sidecar's search read it too.
 *
 * @module
 */

import { json } from "./wire.ts";

// The "record separator" character. A text that does start with it is kept
// in the marked form too, so that it comes back as it was.
const MARK = "\x1e";

/** The text to keep `value` as. Throws `BadRequest` for a value JSON cannot
 * hold. */
export function toText(value: unknown): string {
  if (typeof value === "string" && !value.startsWith(MARK)) {
    return value;
  }
  return MARK + json(value, "that value");
}

/** The value that was kept as `text`. */
export function fromText(text: string | null): unknown {
  if (text !== null && text.startsWith(MARK)) {
    try {
      return JSON.parse(text.slice(MARK.length));
    } catch {
      // Not something a library wrote. It is given as the text it is.
      return text;
    }
  }
  return text;
}
