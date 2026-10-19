/**
 * Turning the sidecar's replies into values and errors, and checking what is
 * about to be sent.
 *
 * Nothing here talks to the sidecar. Each function takes a reply that has
 * already arrived, or a value that is about to be sent.
 *
 * @module
 */

import {
  BadRequest,
  ClassMismatch,
  EngineError,
  JsonError,
  OutOfBounds,
  SearchUnavailable,
  TooLarge,
  TypeMismatch,
} from "./errors.ts";

/** A message as the proto loader reads and writes it: field names as in the
 * `.proto` file, 64-bit numbers as text, enums by name. */
export type Message = Record<string, any>;

const ERRORS: Record<string, new (message: string) => Error> = {
  CODE_TYPE_MISMATCH: TypeMismatch,
  CODE_CLASS_MISMATCH: ClassMismatch,
  CODE_OUT_OF_BOUNDS: OutOfBounds,
  CODE_JSON: JsonError,
  CODE_TOO_LARGE: TooLarge,
  CODE_NO_SEARCH: SearchUnavailable,
};

/** The error for a `Failed` reply. */
export function failure(failed: Message): Error {
  const kind = ERRORS[failed.code] ?? EngineError;
  return new kind(failed.detail ?? "");
}

/** A 64-bit number from a reply, as a JavaScript number. One that cannot be
 * held exactly is an error, not a number that is quietly wrong. */
export function whole(text: string | number | undefined): number {
  const value = Number(text ?? 0);
  if (!Number.isSafeInteger(value)) {
    throw new TooLarge(`${text} is too large for a JavaScript number`);
  }
  return value;
}

/** Reads one value out of a `ReadValue`. A reply of "absent" means the object
 * does not exist or holds nothing, and gives `empty`. */
function value<T>(read: Message | undefined, empty: () => T, convert: (v: Message | string | boolean) => T): T {
  const kind = read?.v;
  if (kind === undefined || kind === "absent") {
    return empty();
  }
  return convert(read![kind]);
}

export type Reader<T> = (read: Message | undefined) => T;

export const number: Reader<number> = (read) => value(read, () => 0, (v) => whole(v as string));
export const textOrNull: Reader<string | null> = (read) => value(read, () => null, (v) => v as string);
export const text: Reader<string> = (read) => value(read, () => "", (v) => v as string);
export const truth: Reader<boolean> = (read) => value(read, () => false, (v) => v as boolean);
export const textList: Reader<string[]> = (read) =>
  value(read, () => [], (v) => [...((v as Message).values ?? [])]);
export const textSet: Reader<Set<string>> = (read) =>
  value(read, () => new Set(), (v) => new Set<string>((v as Message).values ?? []));
export const textRecord: Reader<Record<string, string>> = (read) =>
  value(read, () => ({}), (v) =>
    Object.fromEntries(((v as Message).pairs ?? []).map((pair: Message) => [pair.key ?? "", pair.value ?? ""])),
  );
export const textListRecord: Reader<Record<string, string[]>> = (read) =>
  value(read, () => ({}), (v) =>
    Object.fromEntries(((v as Message).pairs ?? []).map((pair: Message) => [pair.key ?? "", [...(pair.values ?? [])]])),
  );
export const countRecord: Reader<Record<string, number>> = (read) =>
  value(read, () => ({}), (v) =>
    Object.fromEntries(((v as Message).items ?? []).map((entry: Message) => [entry.item ?? "", whole(entry.qty)])),
  );
// The same, with each quantity as the text the sidecar sent, so that one too
// large for a JavaScript number can still be looked at.
export const countTextRecord: Reader<Record<string, string>> = (read) =>
  value(read, () => ({}), (v) =>
    Object.fromEntries(((v as Message).items ?? []).map((entry: Message) => [entry.item ?? "", String(entry.qty ?? 0)])),
  );
// A number in a JSON document is kept by the engine as a 64-bit float, which
// is what a JavaScript number is, so reading one loses nothing.
export const jsonOrNull: Reader<unknown> = (read) => value(read, () => null, (v) => JSON.parse(v as string));
export const jsonList: Reader<unknown[]> = (read) => value(read, () => [], (v) => JSON.parse(v as string));
export const nothing: Reader<void> = () => undefined;

/**
 * One operation: the request to send, and how to read the reply.
 *
 * `field` is the name of the operation in the sidecar's `Request` message.
 * `read` turns a value reply into a value. `refusable` marks the operations
 * the engine may say no to, which answer `true` or `false`.
 */
export class Op<T> {
  readonly field: string;
  readonly message: Message;
  readonly read: Reader<T>;
  readonly refusable: boolean;

  constructor(field: string, message: Message, read: Reader<T>, refusable = false) {
    this.field = field;
    this.message = message;
    this.read = read;
    this.refusable = refusable;
  }

  /** True for an operation that changes something: its answer says only
   * whether it was done. */
  get changes(): boolean {
    return (this.read as Reader<unknown>) === nothing;
  }

  request(session = "", requestId?: Buffer): Message {
    return requestId ? { session, request_id: requestId, [this.field]: this.message } : { session, [this.field]: this.message };
  }

  /** The value for this operation's reply. Throws the matching error if the
   * reply says the operation failed. */
  result(response: Message): T {
    switch (response.result) {
      case "failed":
        throw failure(response.failed);
      case "refused":
        if (this.refusable) {
          return false as T;
        }
        throw new EngineError(`${this.field} was refused: ${response.refused?.detail ?? ""}`);
      case "ack":
        return (this.refusable ? true : undefined) as T;
      case "value":
        return this.read(response.value);
      default:
        throw new EngineError(`${this.field} got a reply with nothing in it`);
    }
  }
}

/** A count, position or machine number to send. It has to be a whole number
 * that is not negative. */
export function count(n: unknown, what: string): number {
  if (typeof n !== "number" || !Number.isSafeInteger(n) || n < 0) {
    throw new BadRequest(`${what} has to be a whole number that is not negative, not ${String(n)}`);
  }
  return n;
}

/** Text to send. */
export function str(value: unknown, what: string): string {
  if (typeof value !== "string") {
    throw new BadRequest(`${what} has to be text, not ${typeof value}`);
  }
  return value;
}

/** A machine number to send. A `bigint` reaches every number a machine can
 * have. */
export function replica(n: unknown, what: string): string | number {
  if (typeof n === "bigint") {
    if (n < 0n || n >= 2n ** 64n) {
      throw new BadRequest(`${what} has to be a machine number from 0 to 2^64 - 1, not ${n}`);
    }
    return n.toString();
  }
  return count(n, what);
}

/**
 * A value as JSON text. Only what JSON holds exactly is sent: `null`, `true`
 * and `false`, text, finite numbers, arrays and plain objects. Anything else
 * is refused, where `JSON.stringify` would change it without a word: NaN
 * into `null`, a property that is `undefined` into nothing, a `Date` into
 * text.
 *
 * Every number is written as exactly the number it is. `JSON.stringify`
 * writes a whole number from 2^53 up to 10^21 as its shortest digits with
 * zeros after, so `2 ** 60` comes out as 1152921504606847000, which is a
 * different whole number. Such a number is written with all its digits here.
 * `JSON.stringify` also writes -0 as 0. It is written as -0.0 here, so it
 * stays negative zero, as it does from Python.
 *
 * A hole in a list is written as `null`, as `JSON.stringify` writes it.
 */
export function json(value: unknown, what: string): string {
  const seen = new Set<object>();
  const check = (item: unknown, at: string): void => {
    if (item === null || typeof item === "string" || typeof item === "boolean") {
      return;
    }
    if (typeof item === "number") {
      if (!Number.isFinite(item)) {
        throw new BadRequest(`${what} cannot be sent as JSON: ${at} is ${item}`);
      }
      return;
    }
    if (typeof item !== "object") {
      throw new BadRequest(`${what} cannot be sent as JSON: ${at} is ${typeof item === "undefined" ? "undefined" : `a ${typeof item}`}`);
    }
    if (seen.has(item)) {
      throw new BadRequest(`${what} cannot be sent as JSON: ${at} holds itself`);
    }
    seen.add(item);
    if (Array.isArray(item)) {
      item.forEach((inner, index) => check(inner, `${at}[${index}]`));
    } else {
      const kind = Object.getPrototypeOf(item);
      if (kind !== Object.prototype && kind !== null) {
        throw new BadRequest(`${what} cannot be sent as JSON: ${at} is a ${item.constructor?.name ?? "object"}, not a plain object`);
      }
      for (const [key, inner] of Object.entries(item)) {
        check(inner, `${at}.${key}`);
      }
    }
    seen.delete(item);
  };
  check(value, what);
  const write = (item: unknown): string => {
    if (typeof item === "number" && Number.isInteger(item) && Math.abs(item) > Number.MAX_SAFE_INTEGER && Math.abs(item) < 1e21) {
      return BigInt(item).toString();
    }
    if (Object.is(item, -0)) {
      return "-0.0";
    }
    if (Array.isArray(item)) {
      // A hole in a list is written as null, as JSON.stringify writes it.
      // `map` would skip it and leave an empty place, which is not JSON.
      return "[" + Array.from(item, (inner) => (inner === undefined ? "null" : write(inner))).join(",") + "]";
    }
    if (item !== null && typeof item === "object") {
      return "{" + Object.entries(item).map(([key, inner]) => JSON.stringify(key) + ":" + write(inner)).join(",") + "}";
    }
    return JSON.stringify(item);
  };
  return write(value);
}
