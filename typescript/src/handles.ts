/**
 * The eleven kinds of shared object.
 *
 * Each class is a small handle: it remembers the object's name and how to
 * reach the sidecar, and each method sends one operation. Making a handle
 * sends nothing and creates nothing. An object comes into being with the
 * first write to it, and reading one that does not exist gives an empty
 * value, not an error.
 *
 * A handle from a client sends at once, and each method gives a `Promise`.
 * A handle from a batch only adds to the batch, and each method gives a
 * `Pending`.
 *
 * @module
 */

import type { Pending } from "./pending.ts";
import * as wire from "./wire.ts";
import { Op } from "./wire.ts";
import type { Reader } from "./wire.ts";

/** `"now"` for a handle from a client, `"batch"` for one from a batch. */
export type Mode = "now" | "batch";

/** What a method gives: a `Promise` from a client, a `Pending` from a batch. */
export type Out<M extends Mode, T> = M extends "batch" ? Pending<T> : Promise<T>;

/** What hands handles their operations: a client or a batch. */
export interface Runner<M extends Mode> {
  run<T>(op: Op<T>): Out<M, T>;
  /** What to do with a mistake found before anything was sent. */
  refuse<T>(error: unknown): Out<M, T>;
  watch<T>(kind: string, name: string, read: Reader<T>, signal?: AbortSignal): AsyncIterable<T>;
}

/** How to watch. */
export interface WatchOptions {
  /** Ends the loop when it is aborted, at once, even while it is waiting
   * for the next change. */
  signal?: AbortSignal;
}

/** What every handle has in common. */
abstract class Handle<M extends Mode, W> {
  readonly #runner: Runner<M>;
  /** The object's name. */
  readonly name: string;
  protected abstract readonly kind: string;
  protected abstract readonly whole: Reader<W>;

  constructor(runner: Runner<M>, name: string) {
    this.#runner = runner;
    this.name = name;
  }

  /** Sends the operation `build` makes. A mistake in an argument is found
   * while it is made, and is reported like any other mistake in a request. */
  protected send<T>(build: () => Op<T>): Out<M, T> {
    let op: Op<T>;
    try {
      op = build();
    } catch (error) {
      return this.#runner.refuse(error);
    }
    return this.#runner.run(op);
  }

  /**
   * The object's value now, and again each time it changes.
   *
   * Use it in a `for await` loop. The loop runs until you leave it, until
   * `options.signal` is aborted, or until the client is closed. A value that
   * did not change is not given twice. If the object does not exist yet, the
   * loop waits for it.
   */
  watch(options: WatchOptions = {}): AsyncIterable<W> {
    return this.#runner.watch(this.kind, this.name, this.whole, options.signal);
  }
}

const o = (name: string) => wire.str(name, "the name of an object");

/** A number you add to and subtract from. It may go below zero. */
export class Counter<M extends Mode = "now"> extends Handle<M, number> {
  protected readonly kind = "KIND_COUNTER";
  protected readonly whole = wire.number;

  /** Adds `n`. */
  increment(n = 1): Out<M, void> {
    return this.send(() => new Op("counter_increment", { object: o(this.name), n: wire.count(n, "n") }, wire.nothing));
  }

  /** Subtracts `n`. */
  decrement(n = 1): Out<M, void> {
    return this.send(() => new Op("counter_decrement", { object: o(this.name), n: wire.count(n, "n") }, wire.nothing));
  }

  /** The current value. 0 if nothing has been written. */
  value(): Out<M, number> {
    return this.send(() => new Op("counter_value", { object: o(this.name) }, wire.number));
  }
}

/** One text value. If two machines set it at the same time, the later write
 * wins. */
export class Register<M extends Mode = "now"> extends Handle<M, string | null> {
  protected readonly kind = "KIND_REGISTER";
  protected readonly whole = wire.textOrNull;

  /** Sets the value. */
  set(value: string): Out<M, void> {
    return this.send(() => new Op("register_set", { object: o(this.name), value: wire.str(value, "value") }, wire.nothing));
  }

  /** The current value, or `null` if it was never set. */
  get(): Out<M, string | null> {
    return this.send(() => new Op("register_get", { object: o(this.name) }, wire.textOrNull));
  }
}

/** A set of text items. Adding wins over a removal made at the same time. */
export class TextSet<M extends Mode = "now"> extends Handle<M, Set<string>> {
  protected readonly kind = "KIND_SET";
  protected readonly whole = wire.textSet;

  /** Adds `member`. */
  add(member: string): Out<M, void> {
    return this.send(() => new Op("set_add", { object: o(this.name), member: wire.str(member, "member") }, wire.nothing));
  }

  /** Removes `member`. */
  remove(member: string): Out<M, void> {
    return this.send(() => new Op("set_remove", { object: o(this.name), member: wire.str(member, "member") }, wire.nothing));
  }

  /** `true` if `member` is in the set. */
  contains(member: string): Out<M, boolean> {
    return this.send(() => new Op("set_contains", { object: o(this.name), member: wire.str(member, "member") }, wire.truth));
  }

  /** Everything in the set. */
  elements(): Out<M, Set<string>> {
    return this.send(() => new Op("set_elements", { object: o(this.name) }, wire.textSet));
  }
}

/** Keys with a text value each. For one key, the later write wins. */
export class TextMap<M extends Mode = "now"> extends Handle<M, Record<string, string>> {
  protected readonly kind = "KIND_MAP";
  protected readonly whole = wire.textRecord;

  /** Sets `key` to `value`. */
  put(key: string, value: string): Out<M, void> {
    return this.send(
      () => new Op("map_put", { object: o(this.name), key: wire.str(key, "key"), value: wire.str(value, "value") }, wire.nothing),
    );
  }

  /** Removes `key`. */
  remove(key: string): Out<M, void> {
    return this.send(() => new Op("map_remove", { object: o(this.name), key: wire.str(key, "key") }, wire.nothing));
  }

  /** The value at `key`, or `null` if the key is not there. */
  get(key: string): Out<M, string | null> {
    return this.send(() => new Op("map_get", { object: o(this.name), key: wire.str(key, "key") }, wire.textOrNull));
  }

  /** The keys. */
  keys(): Out<M, string[]> {
    return this.send(() => new Op("map_keys", { object: o(this.name) }, wire.textList));
  }

  /** Every key and value. */
  entries(): Out<M, Record<string, string>> {
    return this.send(() => new Op("map_entries", { object: o(this.name) }, wire.textRecord));
  }
}

/** Keys with text values that keeps every value written to a key at the
 * same time. Where `TextMap` keeps the later of two clashing writes, this
 * keeps both until a write that has seen them replaces them. More than one
 * value at a key means a clash nothing has settled yet. */
export class MvMap<M extends Mode = "now"> extends Handle<M, Record<string, string[]>> {
  protected readonly kind = "KIND_MV_MAP";
  protected readonly whole = wire.textListRecord;

  /** Sets `key` to `value`, replacing every value this machine has seen
   * there. */
  put(key: string, value: string): Out<M, void> {
    return this.send(
      () => new Op("mv_map_put", { object: o(this.name), key: wire.str(key, "key"), value: wire.str(value, "value") }, wire.nothing),
    );
  }

  /** Removes every value this machine has seen at `key`. */
  remove(key: string): Out<M, void> {
    return this.send(() => new Op("mv_map_remove", { object: o(this.name), key: wire.str(key, "key") }, wire.nothing));
  }

  /** The values at `key`: none if the key is not there, one normally, more
   * while writes clash. */
  get(key: string): Out<M, string[]> {
    return this.send(() => new Op("mv_map_get", { object: o(this.name), key: wire.str(key, "key") }, wire.textList));
  }

  /** The keys. */
  keys(): Out<M, string[]> {
    return this.send(() => new Op("mv_map_keys", { object: o(this.name) }, wire.textList));
  }

  /** Every key with its values. */
  entries(): Out<M, Record<string, string[]>> {
    return this.send(() => new Op("mv_map_entries", { object: o(this.name) }, wire.textListRecord));
  }
}

/** Items with a quantity each. Quantities add up across machines, and a
 * quantity can go below zero: nothing stops more being taken than was put. */
export class Cart<M extends Mode = "now"> extends Handle<M, Record<string, number>> {
  protected readonly kind = "KIND_CART";
  protected readonly whole = wire.countRecord;

  /** Adds `qty` of `item`. */
  add(item: string, qty = 1): Out<M, void> {
    return this.send(
      () => new Op("cart_add", { object: o(this.name), item: wire.str(item, "item"), qty: wire.count(qty, "qty") }, wire.nothing),
    );
  }

  /** Takes `qty` of `item` away. */
  removeQty(item: string, qty = 1): Out<M, void> {
    return this.send(
      () => new Op("cart_remove_qty", { object: o(this.name), item: wire.str(item, "item"), qty: wire.count(qty, "qty") }, wire.nothing),
    );
  }

  /** Removes `item` altogether. */
  removeItem(item: string): Out<M, void> {
    return this.send(() => new Op("cart_remove_item", { object: o(this.name), item: wire.str(item, "item") }, wire.nothing));
  }

  /** How many of `item` there are. 0 if it is not in the cart. */
  quantity(item: string): Out<M, number> {
    return this.send(() => new Op("cart_quantity", { object: o(this.name), item: wire.str(item, "item") }, wire.number));
  }

  /** Every item and its quantity. */
  items(): Out<M, Record<string, number>> {
    return this.send(() => new Op("cart_items", { object: o(this.name) }, wire.countRecord));
  }

  /** Every item and its quantity as text, also a quantity too large for a
   * JavaScript number.
   * @internal */
  itemsAsText(): Out<M, Record<string, string>> {
    return this.send(() => new Op("cart_items", { object: o(this.name) }, wire.countTextRecord));
  }
}

/** An ordered list of text items. */
export class TextList<M extends Mode = "now"> extends Handle<M, string[]> {
  protected readonly kind = "KIND_LIST";
  protected readonly whole = wire.textList;

  /** Puts `value` at position `index`. Throws `OutOfBounds` if `index` is
   * past the end. */
  insert(index: number, value: string): Out<M, void> {
    return this.send(
      () => new Op("list_insert", { object: o(this.name), index: wire.count(index, "index"), value: wire.str(value, "value") }, wire.nothing),
    );
  }

  /** Removes the item at `index`. Throws `OutOfBounds` if there is none. */
  delete(index: number): Out<M, void> {
    return this.send(() => new Op("list_delete", { object: o(this.name), index: wire.count(index, "index") }, wire.nothing));
  }

  /** The item at `index`, or `null` if there is none. */
  get(index: number): Out<M, string | null> {
    return this.send(() => new Op("list_get", { object: o(this.name), index: wire.count(index, "index") }, wire.textOrNull));
  }

  /** How many items there are. */
  length(): Out<M, number> {
    return this.send(() => new Op("list_len", { object: o(this.name) }, wire.number));
  }

  /** Every item, in order. */
  toArray(): Out<M, string[]> {
    return this.send(() => new Op("list_to_vec", { object: o(this.name) }, wire.textList));
  }
}

/** A path into a JSON document: keys and positions, such as `["users", 0]`. */
export type JsonPath = (string | number)[];

/**
 * A JSON document. A write may go down through keys but not through a list
 * position: to change something inside a list item, replace the whole item.
 */
export class Json<M extends Mode = "now"> extends Handle<M, unknown> {
  protected readonly kind = "KIND_JSON";
  protected readonly whole = wire.jsonOrNull;

  /** Sets what is at `path` to `value`, which is any JSON value. Objects that
   * are missing along the way are created. */
  set(path: JsonPath, value: unknown): Out<M, void> {
    return this.send(
      () => new Op("json_set", { object: o(this.name), path_json: wire.json(path, "path"), value_json: wire.json(value, "value") }, wire.nothing),
    );
  }

  /** Removes what is at `path`. */
  remove(path: JsonPath): Out<M, void> {
    return this.send(() => new Op("json_remove", { object: o(this.name), path_json: wire.json(path, "path") }, wire.nothing));
  }

  /**
   * Puts `value` into the list at `path`, at position `index`. What was there
   * and after it moves up by one, and `index` may be the list's length, to
   * add at the end. Where there is nothing yet, position 0 starts a list.
   * Where there is something that is not a list, this throws `JsonError` and
   * changes nothing.
   *
   * Two programs that insert at the same moment both keep their item.
   * Setting the whole list again would keep only one of the two lists.
   */
  insert(path: JsonPath, index: number, value: unknown): Out<M, void> {
    return this.send(
      () =>
        new Op(
          "json_insert",
          { object: o(this.name), path_json: wire.json(path, "path"), index: wire.count(index, "index"), value_json: wire.json(value, "value") },
          wire.nothing,
        ),
    );
  }

  /** What is at `path`, or `null` if nothing is there. */
  get(path: JsonPath): Out<M, unknown> {
    return this.send(() => new Op("json_get", { object: o(this.name), path_json: wire.json(path, "path") }, wire.jsonOrNull));
  }

  /** The whole document. `null` if nothing has been written. */
  toValue(): Out<M, unknown> {
    return this.send(() => new Op("json_to_value", { object: o(this.name) }, wire.jsonOrNull));
  }
}

/** A run of text that has the same marks throughout. */
export interface Span {
  text: string;
  marks: Record<string, string>;
}

/**
 * Text with formatting.
 *
 * Positions and lengths count characters as `[...text].length` does, which
 * is how the sidecar counts them. A JavaScript string's own `length` counts
 * an emoji and some other characters as two.
 */
export class RichText<M extends Mode = "now"> extends Handle<M, Span[]> {
  protected readonly kind = "KIND_RICHTEXT";
  protected readonly whole = wire.jsonList as Reader<Span[]>;

  /** Inserts `text` at position `index`. Past the end means at the end. */
  insert(index: number, text: string): Out<M, void> {
    return this.send(
      () => new Op("richtext_insert", { object: o(this.name), index: wire.count(index, "index"), text: wire.str(text, "text") }, wire.nothing),
    );
  }

  /** Deletes `length` characters starting at `index`. */
  delete(index: number, length: number): Out<M, void> {
    return this.send(
      () => new Op("richtext_delete", { object: o(this.name), index: wire.count(index, "index"), len: wire.count(length, "length") }, wire.nothing),
    );
  }

  /** Sets the mark `attr` to `value` on `length` characters from `index`.
   * `value` must not be empty. Use `unformat` to remove a mark. */
  format(index: number, length: number, attr: string, value: string): Out<M, void> {
    return this.send(
      () =>
        new Op(
          "richtext_format",
          {
            object: o(this.name),
            index: wire.count(index, "index"),
            len: wire.count(length, "length"),
            attr: wire.str(attr, "attr"),
            value: wire.str(value, "value"),
          },
          wire.nothing,
        ),
    );
  }

  /** Removes the mark `attr` from `length` characters from `index`. */
  unformat(index: number, length: number, attr: string): Out<M, void> {
    return this.send(
      () =>
        new Op(
          "richtext_unformat",
          { object: o(this.name), index: wire.count(index, "index"), len: wire.count(length, "length"), attr: wire.str(attr, "attr") },
          wire.nothing,
        ),
    );
  }

  /** The text with no formatting. */
  text(): Out<M, string> {
    return this.send(() => new Op("richtext_text", { object: o(this.name) }, wire.text));
  }

  /** The text as runs that share the same marks. */
  toSpans(): Out<M, Span[]> {
    return this.send(() => new Op("richtext_to_spans", { object: o(this.name) }, wire.jsonList as Reader<Span[]>));
  }
}

/** One node of a tree, as `toNodes` gives it. */
export interface TreeNode {
  id: string;
  parent: string | null;
  label: string | null;
  position: string | null;
}

/** Items nested under other items. A move that would put an item under
 * itself is skipped, so the tree never has a loop in it. */
export class Tree<M extends Mode = "now"> extends Handle<M, TreeNode[]> {
  protected readonly kind = "KIND_TREE";
  protected readonly whole = wire.jsonList as Reader<TreeNode[]>;

  /** Puts `child` under `newParent`. */
  moveNode(child: string, newParent: string): Out<M, void> {
    return this.send(
      () => new Op("tree_move_node", { object: o(this.name), child: wire.str(child, "child"), new_parent: wire.str(newParent, "newParent") }, wire.nothing),
    );
  }

  /** Puts `child` under `newParent`, at position `index` among its children. */
  moveNodeAt(child: string, newParent: string, index: number): Out<M, void> {
    return this.send(
      () =>
        new Op(
          "tree_move_node_at",
          { object: o(this.name), child: wire.str(child, "child"), new_parent: wire.str(newParent, "newParent"), index: wire.count(index, "index") },
          wire.nothing,
        ),
    );
  }

  /** Gives `node` a label. */
  setLabel(node: string, label: string): Out<M, void> {
    return this.send(
      () => new Op("tree_set_label", { object: o(this.name), node: wire.str(node, "node"), label: wire.str(label, "label") }, wire.nothing),
    );
  }

  /** The parent of `node`, or `null` for the top and for a node that was
   * never placed. */
  parent(node: string): Out<M, string | null> {
    return this.send(() => new Op("tree_parent", { object: o(this.name), node: wire.str(node, "node") }, wire.textOrNull));
  }

  /** The children of `node`, in order. */
  children(node: string): Out<M, string[]> {
    return this.send(() => new Op("tree_children", { object: o(this.name), node: wire.str(node, "node") }, wire.textList));
  }

  /** Every node. */
  toNodes(): Out<M, TreeNode[]> {
    return this.send(() => new Op("tree_to_nodes", { object: o(this.name) }, wire.jsonList as Reader<TreeNode[]>));
  }
}

/**
 * A number that can never go below zero, such as a budget.
 *
 * Each machine has an allowance: its share of the total, which it may spend
 * without asking anyone. Whoever adds to the counter gets that amount as
 * allowance.
 */
export class BoundedCounter<M extends Mode = "now"> extends Handle<M, number> {
  protected readonly kind = "KIND_BOUNDED";
  protected readonly whole = wire.number;

  /** Adds `n`. This machine gets it as allowance. */
  increment(n: number): Out<M, void> {
    return this.send(() => new Op("bounded_increment", { object: o(this.name), n: wire.count(n, "n") }, wire.nothing));
  }

  /**
   * Takes `n` away if this machine's allowance covers it.
   *
   * Gives `true` if it did, and `false` if there was not enough, in which
   * case nothing changed. Running out is not an error.
   */
  tryDecrement(n: number): Out<M, boolean> {
    return this.send(
      () => new Op<boolean>("bounded_try_decrement", { object: o(this.name), n: wire.count(n, "n") }, () => true, true),
    );
  }

  /** Hands `n` of this machine's allowance to the machine numbered
   * `toReplica`, such as another client's `replicaId`. Gives `false`,
   * changing nothing, if this machine has less than `n`. */
  transfer(toReplica: number | bigint, n: number): Out<M, boolean> {
    return this.send(
      () =>
        new Op<boolean>(
          "bounded_transfer",
          { object: o(this.name), to_replica: wire.replica(toReplica, "toReplica"), n: wire.count(n, "n") },
          () => true,
          true,
        ),
    );
  }

  /** The current total. */
  value(): Out<M, number> {
    return this.send(() => new Op("bounded_value", { object: o(this.name) }, wire.number));
  }

  /** How much this machine may still take away. */
  allowance(): Out<M, number> {
    return this.send(() => new Op("bounded_allowance", { object: o(this.name) }, wire.number));
  }
}

/** The methods that hand out handles. A client and a batch both have them. */
export abstract class Objects<M extends Mode> implements Runner<M> {
  abstract run<T>(op: Op<T>): Out<M, T>;
  abstract refuse<T>(error: unknown): Out<M, T>;
  abstract watch<T>(kind: string, name: string, read: Reader<T>, signal?: AbortSignal): AsyncIterable<T>;

  /** A number you add to and subtract from. */
  counter(name: string): Counter<M> {
    return new Counter(this, name);
  }

  /** A number that can never go below zero. */
  boundedCounter(name: string): BoundedCounter<M> {
    return new BoundedCounter(this, name);
  }

  /** One text value. */
  register(name: string): Register<M> {
    return new Register(this, name);
  }

  /** A set of text items. */
  set(name: string): TextSet<M> {
    return new TextSet(this, name);
  }

  /** Keys with a text value each. */
  map(name: string): TextMap<M> {
    return new TextMap(this, name);
  }

  /** Keys with text values that keeps clashing writes. */
  mvMap(name: string): MvMap<M> {
    return new MvMap(this, name);
  }

  /** Items with a quantity each. */
  cart(name: string): Cart<M> {
    return new Cart(this, name);
  }

  /** An ordered list of text items. */
  list(name: string): TextList<M> {
    return new TextList(this, name);
  }

  /** A JSON document. */
  json(name: string): Json<M> {
    return new Json(this, name);
  }

  /** Text with formatting. */
  richText(name: string): RichText<M> {
    return new RichText(this, name);
  }

  /** Items nested under other items. */
  tree(name: string): Tree<M> {
    return new Tree(this, name);
  }
}
