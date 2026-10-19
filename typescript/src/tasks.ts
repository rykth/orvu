/**
 * The task board: what needs doing, who is on it, and how it turned out.
 *
 * @module
 */

import { randomBytes, randomInt } from "node:crypto";
import { BadRequest, OrvuError, TicketUnavailable, TooLarge, UnknownTask } from "./errors.ts";
import type { WatchOptions } from "./handles.ts";
import { together } from "./follow.ts";
import * as names from "./names.ts";
import type { Pending } from "./pending.ts";
import { sent, type Team } from "./team.ts";

/** The states a task can be in. */
export const STATES = ["todo", "doing", "done"] as const;
export type State = (typeof STATES)[number];

// The top of the tree of tasks. A task with no parent task hangs here.
const ROOT = "root";

// How many tasks are read in one message. A message holds at most 1,024
// operations, and each task takes two.
const AT_ONCE = 500;

/** How long a claim lasts, in seconds, without word from the agent that
 * made it, unless the claim says otherwise. */
export const LEASE = 15 * 60;

// The longest a claim can be made to last: a year.
const LONGEST_LEASE = 365 * 24 * 60 * 60;

// How far a firm task set back with force jumps past the highest ticket it
// can find: a random number of at least this, and less than twice it. See
// the notes on tickets below.
const JUMP = 2 ** 31;

/** One task as it was when it was read. It does not change afterwards: read
 * it again to see what happened since. */
export interface Task {
  /** The task's id, such as "t-3f9a1c2e5b7d4a60". Made up when the task is
   * added. */
  id: string;
  title: string;
  state: State;
  /** The agent working on it, or `null`. */
  claimedBy: string | null;
  /** What `finish` was given, or `null`. */
  result: unknown;
  /** Free text about the task, or `null`. */
  notes: string | null;
  /** The id of the task this is a sub-task of, or `null`. */
  parent: string | null;
  /** When it was added, in milliseconds since 1970, by the clock of the
   * machine that added it. */
  created: number;
  /** The agent that added it, or `null`. */
  createdBy: string | null;
  /** The agent that finished it, or `null`. */
  finishedBy: string | null;
  /** The agent that last changed its title, notes or state by hand with
   * `update`, or `null`. For an agent with rules, the sidecar records this
   * itself: the agent that last changed anything in the task. */
  updatedBy: string | null;
  /** Whether only one agent at a time can hold it. See `Tasks.add`. */
  firm: boolean;
  /** For a firm task, the number of its ticket. It goes up by one each time
   * the task is given back, taken over or set back by hand. */
  ticket: number;
  /** When the agent on it last said it was still working, in milliseconds
   * since 1970 by that agent's clock, or `null`. It is there to be shown.
   * Whether the claim has run out is not worked out from it: see
   * `abandoned`. */
  seen: number | null;
  /** How many seconds the claim lasts without word from its agent, or
   * `null`. */
  lease: number | null;
  /** When this program first saw the claim as it is now, in milliseconds by
   * this program's own steady clock (`performance.now()`), or `null` if
   * nobody is on the task. Any change to the claim, such as the agent on it
   * saying it is still working, starts it again. It means nothing to another
   * program. */
  quietSince: number | null;
  /** The last time the task was taken over from an agent that had gone
   * quiet: the agent that took it, the agent it was taken from, and when, in
   * milliseconds since 1970 by the clock of the agent that took it. `null`
   * if it never was. It is written on the task, so every agent that reads or
   * watches the board sees it, and it stays after the task is given back or
   * finished. */
  takenOver: Takeover | null;
}

/** A record of a task being taken over. See `Task.takenOver`. */
export interface Takeover {
  by: string;
  from: string;
  at: number;
}

/** The record of a takeover read from the board, or `null` if it is not
 * one. */
function takeover(value: unknown): Takeover | null {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return null;
  }
  const { by, from, at } = value as Record<string, unknown>;
  if (typeof by !== "string" || typeof from !== "string" || !Number.isInteger(at)) {
    return null;
  }
  return { by, from, at: at as number };
}

/**
 * Whether the agent on a task has gone quiet for longer than its claim
 * lasts, so that another agent may take the task over.
 *
 * It goes by this program's own clock only. The claim has run out when this
 * program has seen no word from the agent on the task, no change to its
 * claim at all, for as long as the claim lasts. The time the agent wrote is
 * not compared with this machine's clock, so machines whose clocks are far
 * apart still agree on how long a claim lasts. The price: a program that has
 * just started has seen nothing yet, and waits a whole lease before it takes
 * any task over.
 *
 * `now` is a reading of the same steady clock as `quietSince`, in
 * milliseconds, and is the moment of asking if left out.
 */
export function abandoned(task: Task, now: number = performance.now()): boolean {
  if (task.state !== "doing" || task.claimedBy === null || task.seen === null || task.lease === null) {
    return false;
  }
  if (task.quietSince === null || task.quietSince === undefined) {
    return false;
  }
  return now - task.quietSince > task.lease * 1000;
}

/** A task, or its id. */
export type TaskRef = Task | string;

type Fields = Record<string, unknown>;

/** One read of `Tasks.#all`. */
type Read = ["parent" | "get" | "title", string];

const idOf = (task: TaskRef | unknown) =>
  typeof task === "object" && task !== null && "id" in task ? (task as Task).id : task;

/** Whether a task has arrived far enough to be shown. A task another machine
 * is still in the middle of adding may be on the board without its title
 * yet. */
function whole(fields: unknown): fields is Fields {
  return typeof fields === "object" && fields !== null && !Array.isArray(fields) && "title" in fields;
}

const textOrNull = (value: unknown) => (typeof value === "string" ? value : null);
const wholeOrNull = (value: unknown) => (Number.isInteger(value) ? (value as number) : null);

/** The result of a finished task and the agent that finished it. They are
 * kept together under the task's "finished" mark, so that two agents that
 * finish it at once cannot leave the result of one beside the name of the
 * other. A task finished before that was so has them in fields of their
 * own. */
function finished(fields: Fields): [unknown, unknown] {
  const mark = fields.finished;
  if (typeof mark === "object" && mark !== null && !Array.isArray(mark)) {
    const { result, by } = mark as Fields;
    return [result, by];
  }
  return [fields.result, fields.finished_by];
}

/** Tells one finish from another, so that an agent can see whether the
 * finish kept on the board is its own. */
const finishMark = () => randomBytes(8).toString("hex");

/**
 * The number of a firm task's ticket: the highest one ever written on it.
 * Each number is written down as a key of its own under "tickets", and keys
 * written on different machines are all kept when they meet, so the highest
 * can only go up. A number written late, by an agent that had not heard the
 * task moved on, lands below it and changes nothing. "ticket" is the number
 * as it was last written, kept for display and for tasks written before
 * there were keys.
 */
function ticketNumber(fields: Fields): number {
  const last = wholeOrNull(fields.ticket);
  let highest = last !== null && last >= 0 && last < MOST ? last : 0;
  const written = fields.tickets;
  if (typeof written === "object" && written !== null && !Array.isArray(written)) {
    for (const key of Object.keys(written)) {
      const number = ticketKey(key);
      if (number !== null) {
        highest = Math.max(highest, number);
      }
    }
  }
  return highest;
}

// Ticket numbers are below this: fifteen digits at most. Both libraries read
// every such number exactly the same, and a force never goes past it.
const MOST = 10 ** 15;

/** The number a key under "tickets", or a line of the team's list, names, or
 * `null` if it is not one to fifteen plain digits. Anything else is left
 * out, so that nothing written into a task can stop it being read. */
function ticketKey(key: string): number | null {
  return /^[0-9]{1,15}$/.test(key) ? Number(key) : null;
}

/** Writes a firm task's new ticket number on the board, in a batch. */
function putTicket(board: { set(path: (string | number)[], value: unknown): unknown }, id: string, number: number): void {
  board.set([id, "ticket"], number);
  board.set([id, "tickets", String(number)], true);
}

/** The error for a firm task whose ticket, or ticket for moving it on, was
 * made twice. */
function doubled(found: Task, force: boolean): TicketUnavailable {
  const which = force ? "ticket for moving it on" : `ticket ${found.ticket}`;
  const how = force ? 'update(task, { state: "todo", force: true })' : 'update(task, { state: "todo" })';
  return new TicketUnavailable(
    `task ${JSON.stringify(found.id)} cannot be claimed: its ${which} was made twice, so two agents could both ` +
      `take it. It is never taken. Set the task back with ${how}, which moves it on to a fresh one`,
  );
}

/** A `Task` made from what the board holds for it, or `null` if it is not
 * whole yet. Its parent is named only if it is in `present`, the tasks known
 * to be whole. */
function task(id: string, fields: unknown, parent: string | null, present: Set<string>): Task | null {
  if (!whole(fields)) {
    return null;
  }
  const [result, finishedBy] = finished(fields);
  // Any program with the right rules can write anything into a task. What is
  // not what it should be reads as a safe value instead, the same in both
  // libraries, so that reading the board never fails because of it.
  const state = STATES.includes(fields.state as State) ? (fields.state as State) : "todo";
  return {
    id,
    title: typeof fields.title === "string" ? fields.title : "",
    // A finished task has a mark of its own, apart from its state. A claim
    // that was on its way while the task was being finished writes the
    // state, and cannot undo the mark.
    // Anything there but nothing, null or false counts as the mark.
    state: fields.finished !== undefined && fields.finished !== null && fields.finished !== false ? "done" : state,
    claimedBy: textOrNull(fields.claimed_by),
    result: result ?? null,
    notes: textOrNull(fields.notes),
    parent: parent !== null && present.has(parent) ? parent : null,
    // Any finite number, so that tasks can always be put in order.
    created: typeof fields.created === "number" && Number.isFinite(fields.created) ? fields.created : 0,
    createdBy: textOrNull(fields.created_by),
    finishedBy: textOrNull(finishedBy),
    updatedBy: textOrNull(fields.updated_by),
    firm: fields.firm === true,
    ticket: ticketNumber(fields),
    seen: wholeOrNull(fields.seen),
    lease: wholeOrNull(fields.lease),
    quietSince: null,
    takenOver: takeover(fields.taken_over),
  };
}

function checkLease(lease: unknown): number {
  if (lease === undefined || lease === null) {
    return LEASE;
  }
  if (!Number.isInteger(lease) || (lease as number) <= 0 || (lease as number) > LONGEST_LEASE) {
    throw new BadRequest(`a lease is a whole number of seconds, from 1 to a year, not ${JSON.stringify(lease) ?? String(lease)}`);
  }
  return lease as number;
}

const inOrder = (tasks: Iterable<Task>) =>
  [...tasks].sort((a, b) => a.created - b.created || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));

function checkTitle(title: unknown): string {
  if (typeof title !== "string" || title.trim() === "") {
    throw new BadRequest(`a task needs a title, not ${JSON.stringify(title) ?? String(title)}`);
  }
  return title;
}

function checkNotes(notes: unknown): void {
  if (notes !== undefined && typeof notes !== "string") {
    throw new BadRequest(`a task's notes are text, not ${typeof notes}`);
  }
}

/**
 * The team's task board. Get it from `team.tasks`.
 *
 * ```ts
 * const task = await team.tasks.add("Summarise the Q3 report");
 * if (await team.tasks.claim(task)) {
 *   await team.tasks.finish(task, "It went up.");
 * }
 * ```
 *
 * Wherever a task is asked for, a `Task` or its id will do. The board is
 * the one the Python package keeps, so Python and Node agents share it.
 */
export class Tasks {
  /**
   * How many seconds a task on the board with no place in the tree yet is
   * kept out of sight. A task is put in the tree first and on the board
   * after, but another machine may hear of the two in either order, and shown
   * before its place in the tree arrives it would be at the top and under its
   * parent a moment later. A task that never gets a place, from a program
   * that stopped half way through adding it, is shown at the top once this
   * time is over.
   */
  grace = 30;
  readonly #team: Team;
  // When the next task kept out of sight is to be shown, in milliseconds
  // since 1970, or `null`.
  #due: number | null = null;
  // For each task someone is on, the claim as this program last saw it and
  // when, by the steady clock, it first saw it so. That is what tells whether
  // the claim has run out.
  readonly #heard = new Map<string, { claim: string; since: number }>();

  /** @internal */
  constructor(team: Team) {
    this.#team = team;
  }

  /** The task, with when this program first saw its claim as it is now. */
  #noted(found: Task): Task {
    if (found.state !== "doing" || found.claimedBy === null || found.seen === null || found.lease === null) {
      this.#heard.delete(found.id);
      return found;
    }
    const claim = JSON.stringify([found.claimedBy, found.seen, found.lease, found.ticket]);
    let heard = this.#heard.get(found.id);
    if (heard === undefined || heard.claim !== claim) {
      heard = { claim, since: performance.now() };
      this.#heard.set(found.id, heard);
    }
    return { ...found, quietSince: heard.since };
  }

  get #board() {
    return this.#team.client.json(names.tasks(this.#team.name));
  }

  get #tree() {
    return this.#team.client.tree(names.taskTree(this.#team.name));
  }

  // ---- reading -------------------------------------------------------------

  /** Every task, by id. Used where every task is wanted. */
  async #read(): Promise<Map<string, Task>> {
    // Two messages and not one batch: a batch's replies together may not be
    // larger than 4 MiB, and a large board is. The tree goes first. A task is
    // put in the tree before it is put on the board, so a task added here is
    // in the tree by the time the board is read.
    const parents = new Map((await this.#tree.toNodes()).map((node) => [node.id, node.parent]));
    const board = ((await this.#board.toValue()) ?? {}) as Record<string, unknown>;
    const present = new Set(Object.keys(board).filter((id) => whole(board[id])));
    const tasks = new Map<string, Task>();
    const now = Date.now();
    let due: number | null = null;
    for (const [id, fields] of Object.entries(board)) {
      if (!parents.has(id)) {
        // Being added, and not in the tree yet. Shown now, it would be at the
        // top, and under its parent a moment later. It is kept out of sight
        // only for a while, so that a task whose place in the tree never
        // comes is not lost.
        const created = typeof fields === "object" && fields !== null ? wholeOrNull((fields as Fields).created) : null;
        if (created !== null && Math.abs(now - created) < this.grace * 1000) {
          const ends = created + this.grace * 1000;
          due = due === null ? ends : Math.min(due, ends);
          continue;
        }
      }
      const found = task(id, fields, parents.get(id) ?? null, present);
      if (found !== null) {
        tasks.set(id, this.#noted(found));
      }
    }
    this.#due = due;
    return tasks;
  }

  /**
   * The tasks with these ids, by id, reading only them. An id that is not on
   * the board is left out.
   *
   * With `parents: false` no parent is read, and every task comes back with
   * `parent: null`. That is for a call that only looks at a task's own
   * fields. `onBoard` names tasks the caller has just read, so that they are
   * not read again when they turn out to be a parent.
   */
  async #some(ids: unknown[], { parents = true, onBoard = [] as string[] } = {}): Promise<Map<string, Task>> {
    const wanted = [...new Set(ids)].filter((id): id is string => typeof id === "string");
    // The parents go first. Their replies are small, and a batch stops
    // running operations once its replies pass 4 MiB.
    const reads: Read[] = parents ? wanted.map((id) => ["parent", id]) : [];
    reads.push(...wanted.map((id): Read => ["get", id]));
    const found = await this.#all(reads);
    const board = new Map<string, unknown>();
    const above = new Map<string, string | null>();
    for (const id of wanted) {
      const fields = found.get(`get/${id}`);
      if (fields !== null && fields !== undefined) {
        board.set(id, fields);
        above.set(id, parents ? ((found.get(`parent/${id}`) as string | null) ?? null) : null);
      }
    }
    // A parent is named only once it is whole itself. The ones not known to
    // be are looked up.
    const present = new Set([...board].filter(([, fields]) => whole(fields)).map(([id]) => id).concat(onBoard));
    const unread = [...new Set(above.values())].filter(
      (parent): parent is string => parent !== null && parent !== ROOT && !present.has(parent),
    );
    const titles = await this.#all(unread.map((parent): Read => ["title", parent]));
    for (const parent of unread) {
      const title = titles.get(`title/${parent}`);
      if (title !== null && title !== undefined) {
        present.add(parent);
      }
    }
    const tasks = new Map<string, Task>();
    for (const [id, fields] of board) {
      const found = task(id, fields, above.get(id) ?? null, present);
      if (found !== null) {
        tasks.set(id, this.#noted(found));
      }
    }
    return tasks;
  }

  /**
   * What each read found, by `"<kind>/<id>"`. A read is `["parent", id]` for
   * where a task is in the tree, `["get", id]` for the task, or `["title",
   * id]` for its title.
   *
   * Up to 500 tasks go in one message. A batch stops running reads once its
   * replies pass 4 MiB, and the reads it did not run are sent again together
   * in the next message, as many times as it takes. The first read of a
   * batch is always run, so each message gets further.
   */
  async #all(reads: Read[]): Promise<Map<string, unknown>> {
    const found = new Map<string, unknown>();
    let waiting = reads;
    while (waiting.length > 0) {
      const part = waiting.slice(0, 2 * AT_ONCE);
      const rest = waiting.slice(2 * AT_ONCE);
      const batch = this.#team.client.batch();
      const pending: Pending<unknown>[] = part.map(([kind, id]) =>
        kind === "parent"
          ? batch.tree(this.#tree.name).parent(id)
          : batch.json(this.#board.name).get(kind === "get" ? [id] : [id, "title"]),
      );
      await batch.send();
      const left: Read[] = [];
      part.forEach((read, at) => {
        try {
          found.set(`${read[0]}/${read[1]}`, pending[at].value);
        } catch (error) {
          // The first read of a batch is always run, so one that is too
          // large there is too large even by itself.
          if (!(error instanceof TooLarge) || at === 0) {
            throw error;
          }
          left.push(read);
        }
      });
      waiting = left.concat(rest);
    }
    return found;
  }

  /** Every task, oldest first. With `state`, only the tasks in that state. */
  async list(state?: State): Promise<Task[]> {
    if (state !== undefined && !STATES.includes(state)) {
      throw new BadRequest(`a state is one of ${STATES.join(", ")}, not ${JSON.stringify(state)}`);
    }
    const tasks = inOrder((await this.#read()).values());
    return tasks.filter((found) => state === undefined || found.state === state);
  }

  /** The task as it is now, or `null` if there is no such task. */
  async get(ref: TaskRef): Promise<Task | null> {
    const id = idOf(ref);
    return typeof id === "string" ? ((await this.#some([id])).get(id) ?? null) : null;
  }

  /** The sub-tasks of a task, oldest first. */
  async children(ref: TaskRef): Promise<Task[]> {
    // A `Task` was read from the board, and nothing leaves the board, so it
    // is not read again.
    const parent = typeof ref === "object" && ref !== null ? ref.id : (await this.#existing(ref, false)).id;
    const under = await this.#some(await this.#tree.children(parent), { onBoard: [parent] });
    return inOrder([...under.values()].filter((found) => found.parent === parent));
  }

  /** The task, or `UnknownTask`. With `parents` false its parent is not
   * read, and is `null`. */
  async #existing(ref: TaskRef, parents = true): Promise<Task> {
    const id = idOf(ref);
    const found = typeof id === "string" ? (await this.#some([id], { parents })).get(id) : undefined;
    if (found === undefined) {
      throw new UnknownTask(`there is no task ${JSON.stringify(id) ?? String(id)} on the board of team ${JSON.stringify(this.#team.name)}`);
    }
    return found;
  }

  /** Every task now, and again each time the board changes. Use it in a
   * `for await` loop. */
  watch(options: WatchOptions = {}): AsyncGenerator<Task[]> {
    // A task kept out of sight is shown when its time is over, even if
    // nothing on the board changed by then.
    const later = () => (this.#due === null ? null : Math.max(this.#due - Date.now(), 0));
    return together(this.#team.client, [this.#board, this.#tree], () => this.list(), options.signal, later);
  }

  // ---- changing ------------------------------------------------------------

  /**
   * Adds a task and gives it back. With `parent`, it is added as a sub-task
   * of that task. `null` for `parent` or `notes` means none, as it does on a
   * `Task`.
   *
   * With `firm: true`, only one agent can hold the task at a time. The
   * task gets one ticket, and an agent has to take it to claim the task.
   * That is for work that must not be done twice by mistake. The cost: an
   * agent whose machine cannot reach the machine holding the ticket is told
   * no, and claiming may wait up to two seconds to find that out.
   */
  async add(title: string, options: { parent?: TaskRef | null; notes?: string | null; firm?: boolean } = {}): Promise<Task> {
    checkTitle(title);
    const notes = options.notes ?? undefined;
    checkNotes(notes);
    const firm = options.firm ?? false;
    if (typeof firm !== "boolean") {
      throw new BadRequest(`firm is true or false, not ${JSON.stringify(firm) ?? String(firm)}`);
    }
    const parent = options.parent != null ? (await this.#existing(options.parent, false)).id : null;
    // A random id, so that two machines that are cut off from each other can
    // both add tasks without picking the same one. It is long enough that a
    // new task does not land on an old one however large the board gets.
    const id = "t-" + randomBytes(8).toString("hex");
    const fields: Fields = { title, state: "todo", created: Date.now() };
    if (notes !== undefined) {
      fields.notes = notes;
    }
    if (this.#team.me !== null) {
      fields.created_by = this.#team.me;
    }
    if (firm) {
      fields.firm = true;
      // The tickets are made before the task is on the board, so that nobody
      // can find the task without them.
      await this.#newTickets(id, 0, true);
    }
    await this.#team.record("tasks", "json");
    await this.#team.record("tasks-tree", "tree");
    // The place in the tree first, in a message of its own, and then the
    // task. A program that stops in between leaves a place in the tree that
    // names nothing, which shows nothing.
    await this.#tree.moveNode(id, parent ?? ROOT);
    await this.#board.set([id], fields);
    return this.#existing(id);
  }

  // ---- tickets -------------------------------------------------------------
  //
  // A firm task has one ticket at a time, and whoever takes it has the task.
  // Each ticket is a counter that can never go below zero, holding one, so
  // only one agent can take it, even on machines that cannot reach each
  // other: an agent whose machine does not hold it has to borrow it from the
  // machine that does, and is told no if that machine cannot be reached.
  //
  // A ticket is used once. When the task is given back, taken over or set
  // back by hand, it moves on to a new ticket with the next number, and the
  // old one is never made free again: an agent that has not heard of the
  // move yet would take it. Only one agent may make the new ticket, or there
  // would be two. So beside each ticket is a second one, for moving the task
  // on, and only the agent that takes that makes the next pair.
  //
  // Before a new ticket is made free, the old one must be taken. It is when
  // an agent's name is on the task, because an agent takes the ticket before
  // it writes its name. When no name is on it, the old ticket is taken
  // first, so that nobody else can.
  //
  // On the board, the new number is written last, after the name and time of
  // the agent that had the task are gone. An agent that saw the new number
  // beside an old, run-out time would take the task over while another
  // claimed the new ticket.
  //
  // A number must never go to two holders: a step that must not happen twice
  // tells one holder from the next by it. A pair is safe as long as only one
  // agent ever adds to its counters. Moving on from pair n, that is the one
  // agent that took pair n's ticket for moving on, so it holds as long as
  // pair n was made once. Force is the one path that makes a pair without
  // taking anything first, so it must pick a pair no other path has made or
  // ever will:
  //
  // - It reads the highest number the task has had, from the board and the
  //   team's list, and goes past it. A pair is put on the list before its
  //   counters are made, and taken off only after the task is on a higher
  //   one, so the highest pair made so far is always listed.
  // - It goes past it by a random jump of two to four thousand million.
  //   Moving on goes up one at a time, so the pairs the task had before
  //   would need that many moves to reach the forced pair.
  //
  // So two forces where the second sees the first, such as a second
  // operator, or the same one trying again after its write to the board
  // failed, land on different pairs: the second sees the first one's line
  // and goes past it. A force racing an agent that moves the task on from an
  // older pair, even several times, cannot meet it: that agent climbs one at
  // a time from far below. Two forces that cannot see each other at all, on
  // machines cut off from each other or at the very same moment, each take
  // their own random jump, and land on the same pair, or close enough for
  // one to climb into the other, only by a chance of about one in a thousand
  // million. Nothing that only counts can do better: neither machine can
  // learn of the other.
  //
  // Should a pair be made twice all the same, its ticket holds two, and on
  // two machines cut off from each other each could take one. So a ticket or
  // a ticket for moving on found holding more than one is never taken, and a
  // claim says so with `TicketUnavailable`. A doubled ticket is cleared by
  // setting the task back, which moves it on through its ticket for moving
  // on. A doubled ticket for moving on needs force.
  //
  // The number on the board never goes down, or a newer holder could have a
  // lower number than an older one, and whatever keeps the highest number it
  // has seen would refuse the newer holder. An agent that has not heard the
  // task moved on can still write a lower number late, from any machine, and
  // two machines can each write before hearing of the other, so no check
  // before a write can stop that. Instead each number is written down as a
  // key of its own, and the task's number is the highest key. Keys from every
  // machine are kept when they meet, and none is ever taken off, so the
  // highest only goes up, on every machine. A late, lower number changes
  // nothing: the pair it names is not the task's, and a claim on it reads the
  // board back and loses. An agent moving the task on also looks first, and
  // leaves the board alone when the task has moved past its ticket already,
  // so that it does not take the name of whoever holds the new number off
  // the task.

  #ticket(id: string, number: number) {
    return this.#team.client.boundedCounter(names.taskTicket(this.#team.name, id, number));
  }

  #takeover(id: string, number: number) {
    return this.#team.client.boundedCounter(names.taskTakeover(this.#team.name, id, number));
  }

  /** Makes ticket `number` of a task, and the ticket for taking it over.
   * With `free` false the task's ticket is made already taken, by the agent
   * making it. */
  async #newTickets(id: string, number: number, free: boolean): Promise<void> {
    await this.#team.record(`task-takeover/${id}/${number}`, "bounded");
    await this.#team.record(`task-ticket/${id}/${number}`, "bounded");
    await this.#takeover(id, number).increment(1);
    if (free) {
      await this.#ticket(id, number).increment(1);
    }
  }

  /** Takes the takeover ticket beside ticket `number` and makes the next
   * tickets. Gives the new ticket's number, or `null` if another agent moved
   * the task on first, or the machine that holds the takeover ticket cannot
   * be reached. */
  async #nextTicket(id: string, number: number, free: boolean): Promise<number | null> {
    const takeover = this.#takeover(id, number);
    if ((await takeover.value()) > 1 || !(await takeover.tryDecrement(1))) {
      return null;
    }
    await this.#newTickets(id, number + 1, free);
    return number + 1;
  }

  /**
   * Takes the tickets with these numbers off the team's list, once they can
   * never be had again. The counters stay in the sidecar, which cannot remove
   * one, but nothing lists them any more.
   *
   * It is done only after the task's new number is on the board, and a
   * failure is let go: the list is only a listing, and a sidecar that refuses
   * to take anything off it, as an older one with rules does, must not leave
   * the task stuck half way.
   *
   * A pair is spent once the task has moved on from it. Its takeover ticket
   * was taken to move it on, and is never given back. Its own ticket was
   * taken too: by the agent whose name was on the task, or by whoever set it
   * back when no name was. A ticket that was not, because a task was set back
   * with force, is no use either: the board has a higher number, and a claim
   * made with it sees that and loses. Nothing ever adds to a counter of a
   * pair once the next pair is made, so both stay taken.
   *
   * With `keepTakeover`, the takeover ticket stays listed. That is for a
   * finished task, which is set back to "todo" through it.
   */
  async #spent(id: string, numbers: number[], keepTakeover = false): Promise<void> {
    const gone: string[] = [];
    for (const number of numbers) {
      if (!keepTakeover) {
        gone.push(`task-takeover/${id}/${number}`);
      }
      gone.push(`task-ticket/${id}/${number}`);
    }
    try {
      await this.#team.forget(gone);
    } catch (error) {
      if (!(error instanceof OrvuError)) {
        throw error;
      }
    }
  }

  /** The highest ticket number on the team's list for a task, or -1. */
  async #highest(id: string): Promise<number> {
    let highest = -1;
    for (const name of Object.keys(await this.#team.index())) {
      const parts = name.split("/");
      const number = parts.length === 3 && ["task-ticket", "task-takeover"].includes(parts[0]) && parts[1] === id ? ticketKey(parts[2]) : null;
      if (number !== null) {
        highest = Math.max(highest, number);
      }
    }
    return highest;
  }

  /** Moves a firm task on from ticket `number`, which this agent holds, to a
   * free ticket, and shows it as free on the board. Gives `false` if another
   * agent moved it on first, or the machine that holds the takeover ticket
   * cannot be reached. Then nothing is written. */
  async #letGo(id: string, number: number): Promise<boolean> {
    const ticket = await this.#nextTicket(id, number, true);
    if (ticket === null) {
      return false;
    }
    if ((await this.#existing(id, false)).ticket > number) {
      // The task moved on past this ticket another way, as when it was set
      // back with force, and someone may hold the new number. What is on the
      // board is theirs, and is left alone. The pair just made is never put
      // on the board, so nobody can take it.
      return false;
    }
    const batch = this.#team.client.batch();
    const board = batch.json(this.#board.name);
    board.set([id, "state"], "todo");
    for (const field of ["claimed_by", "seen", "lease"]) {
      board.remove([id, field]);
    }
    putTicket(board, id, ticket);
    sent(await batch.send());
    await this.#spent(id, [number]);
    return true;
  }

  /** Moves a firm task that is being set back by hand on to a new, free
   * ticket. Gives its number, and the numbers of the pairs to take off the
   * team's list once it is on the board. With `force`, a task whose tickets
   * cannot be had is moved on all the same. */
  async #setBack(found: Task, force: boolean): Promise<[number, number[]]> {
    const unavailable = () =>
      new TicketUnavailable(
        `task ${JSON.stringify(found.id)} could not be set back: another agent is claiming it, taking it over or ` +
          `setting it back at the same moment, or the machine that holds its ticket cannot be reached. Try again ` +
          `in a moment. If it keeps failing and you know the agent that was claiming or moving it on has stopped ` +
          `for good (its program ended, or its sidecar failed), set it back with update(task, { state: "todo", ` +
          `force: true }). Do not use force while a machine that may hold the ticket is only cut off: wait for it`,
      );
    // Nobody's name is on the task, so its ticket may be free. It is taken
    // first, so that it does not stay free beside the new one.
    let tookOld = false;
    if (found.claimedBy === null) {
      tookOld = await this.#ticket(found.id, found.ticket).tryDecrement(1);
      if (!tookOld && !force) {
        throw unavailable();
      }
    }
    const ticket = await this.#nextTicket(found.id, found.ticket, true);
    if (ticket !== null) {
      return [ticket, [found.ticket]];
    }
    if (!force) {
      if (tookOld) {
        // Nobody could have the task while this held its ticket, so the
        // ticket is put back as it was.
        await this.#ticket(found.id, found.ticket).increment(1);
      }
      throw unavailable();
    }
    // Forced. The ticket for moving the task on is held by an agent that
    // stopped, or by one moving the task on right now. Either may have made
    // the next pairs already, so the task jumps far past every pair it may
    // have had: see the notes on tickets above. Every ticket an earlier agent
    // may still hold has a lower number, and a claim made with it sees
    // another number on the board and loses. An old ticket this took is
    // kept, so it can never be had again.
    const highest = Math.max(found.ticket, await this.#highest(found.id));
    if (highest + 2 * JUMP >= MOST) {
      throw new TicketUnavailable(
        `task ${JSON.stringify(found.id)} could not be set back with force: its ticket numbers are used up, as its ` +
          `number is already ${highest}. Add the task again`,
      );
    }
    const fresh = highest + JUMP + randomInt(JUMP);
    await this.#newTickets(found.id, fresh, true);
    return [fresh, [found.ticket, found.ticket + 1]];
  }

  /**
   * Changes a task's title, notes or state, and gives the task as it is
   * afterwards. What is left out stays as it was.
   *
   * Setting the state by hand is for putting things right. "todo" makes the
   * task free again: whoever had it no longer does. "todo" and "doing" both
   * drop a result it had. "done" finishes it with no result, and leaves a
   * task that is done already as it is.
   *
   * `null` means the same as leaving a field out, as it does on a `Task`:
   * notes that are `null` are left as they are.
   *
   * A firm task that is set back moves on to a new ticket, and that needs
   * the ticket for moving it on. If another agent took that ticket and
   * stopped before it wrote what it did, the task is stuck, and this throws
   * `TicketUnavailable` every time. `force: true` sets it back all the same,
   * on a fresh pair of tickets that no earlier ticket can win. Use it only
   * when you know that agent has stopped for good: its program ended, or its
   * sidecar failed. Do not use it while a machine that may hold the ticket is
   * only cut off; wait for it to come back. `force` is only for setting a
   * task back to "todo".
   */
  async update(
    ref: TaskRef,
    change: { title?: string | null; notes?: string | null; state?: State | null; force?: boolean | null },
  ): Promise<Task> {
    const found = await this.#existing(ref, false);
    const title = change.title ?? undefined;
    const notes = change.notes ?? undefined;
    const state = change.state ?? undefined;
    const force = change.force ?? false;
    if (title !== undefined) {
      checkTitle(title);
    }
    checkNotes(notes);
    if (state !== undefined && !STATES.includes(state)) {
      throw new BadRequest(`a state is one of ${STATES.join(", ")}, not ${JSON.stringify(state)}`);
    }
    if (typeof force !== "boolean") {
      throw new BadRequest(`force is true or false, not ${JSON.stringify(force) ?? String(force)}`);
    }
    if (force && state !== "todo") {
      throw new BadRequest('force is only for setting a task back, with state "todo"');
    }
    let ticket: number | null = null;
    let spent: number[] = [];
    if (state === "todo" && found.firm) {
      // Whoever had the task may still hold its ticket, and cannot be asked
      // to give it back. The task gets a new one.
      [ticket, spent] = await this.#setBack(found, force);
    }
    const batch = this.#team.client.batch();
    const board = batch.json(this.#board.name);
    // Each field is written by itself, so that two agents who change
    // different fields at the same moment both keep their change.
    if (title !== undefined) {
      board.set([found.id, "title"], title);
    }
    if (notes !== undefined) {
      board.set([found.id, "notes"], notes);
    }
    if (state === "done") {
      if (found.state !== "done") {
        board.set([found.id, "finished"], { result: null, by: null, id: finishMark() });
      }
    } else if (state !== undefined) {
      board.set([found.id, "state"], state);
      // What no longer fits the new state is taken off. The result and the
      // finisher are part of the "finished" mark, or fields of their own on
      // a task finished the old way.
      const gone = ["finished", "result", "finished_by"];
      if (state === "todo") {
        gone.push("seen", "lease");
      }
      for (const field of gone) {
        board.remove([found.id, field]);
      }
    }
    const changed = title !== undefined || notes !== undefined || state !== undefined;
    if (changed && this.#team.me !== null) {
      board.set([found.id, "updated_by"], this.#team.me);
    } else if (changed && found.updatedBy !== null) {
      // Changed by a program that gave no name. The task must not go on
      // naming whoever changed it before.
      board.remove([found.id, "updated_by"]);
    }
    if (state === "todo") {
      // Whoever had the task goes after the rest, so that the rest is written
      // while the agent that holds it still does. A firm task's new ticket
      // number goes last of all.
      board.remove([found.id, "claimed_by"]);
    }
    if (ticket !== null) {
      putTicket(board, found.id, ticket);
    }
    sent(await batch.send());
    if (spent.length > 0) {
      await this.#spent(found.id, spent);
    }
    return this.#existing(found.id);
  }

  #me(me: string | undefined): string {
    const who = me ?? this.#team.me;
    if (typeof who !== "string" || who === "") {
      throw new BadRequest(
        "say who is asking: pass `me` here, or give the team a name for this agent with Team.connect(name, { me })",
      );
    }
    return who;
  }

  /**
   * Says "this task is mine". Gives `true` if it now is, and `false` if
   * another agent has it or it is done.
   *
   * The claim lasts `lease` seconds, a quarter of an hour if left out. Call
   * `stillWorking` before that runs out to keep it. An agent that does not
   * is taken to have gone, and another agent's `claim` takes the task over.
   * How long it has been quiet is measured by the clock of the agent that
   * wants the task, from when that agent first saw the claim as it is: see
   * `abandoned`.
   *
   * On a task that is not firm this is a soft claim. It keeps agents off
   * each other's tasks nearly all the time, but two agents that claim the
   * same task at the very same moment can both be told `true`, and so can
   * two agents taking over the same abandoned task. Use it where doing a
   * task twice is only wasteful, not where it would do harm. On a firm task,
   * only one agent at a time is told `true`.
   *
   * Claiming a firm task this agent already has keeps it, as `stillWorking`
   * does, and leaves its lease as it was. `null` for `lease` means the usual
   * lease, as if it were left out.
   *
   * To learn the number of a firm task's ticket, claim it with `claimTicket`
   * instead.
   *
   * A firm task whose ticket was made twice, which two agents could both
   * take, is never claimed: this throws `TicketUnavailable`, which says how to
   * set the task back.
   */
  async claim(ref: TaskRef, me?: string, options: { lease?: number | null } = {}): Promise<boolean> {
    return (await this.#claim(ref, me, options)) !== null;
  }

  /**
   * Claims a firm task as `claim` does, and gives the number of the ticket
   * this agent now holds, or `null` if it did not get the task.
   *
   * The number goes up each time the task moves on to another agent, so it
   * tells one holder from the next. A firm task still goes to one agent at a
   * time, but an agent that went quiet, or whose machine is cut off, may not
   * have heard yet that the task was taken over, and can go on working. So a
   * step that must not happen twice, such as sending an invoice, should
   * carry the number to whatever it changes, and that side should keep the
   * highest number it has seen and refuse a lower one:
   *
   * ```ts
   * const ticket = await team.tasks.claimTicket(task);
   * if (ticket !== null) {
   *   const invoice = await writeInvoice();
   *   // The billing system refuses a ticket lower than the highest it has
   *   // seen for this task.
   *   await billing.send(invoice, { task: task.id, ticket });
   * }
   * ```
   *
   * That last refusal is what stops the step from happening twice. Nothing
   * here can do it for the other side. `stillHolding` tells whether the
   * number is still this agent's as far as this machine knows, which is a
   * good check before starting, not a promise.
   */
  async claimTicket(ref: TaskRef, me?: string, options: { lease?: number | null } = {}): Promise<number | null> {
    const found = await this.#existing(ref, false);
    if (!found.firm) {
      throw new BadRequest(`task ${JSON.stringify(found.id)} is not firm, and only a firm task has a ticket`);
    }
    return this.#claim(found, me, options);
  }

  /** Whether this agent still holds ticket number `ticket` of a firm task, as
   * far as this machine knows: the task is not done, and it has not been
   * given back, taken over or set back since. See `claimTicket`. */
  async stillHolding(ref: TaskRef, ticket: number, me?: string): Promise<boolean> {
    const who = this.#me(me);
    const found = await this.#existing(ref, false);
    if (!found.firm) {
      throw new BadRequest(`task ${JSON.stringify(found.id)} is not firm, and only a firm task has a ticket`);
    }
    return found.claimedBy === who && found.state !== "done" && found.ticket === ticket;
  }

  /** What `claim` does. Gives the number of the task's ticket if this agent
   * has the task now, and `null` if not. */
  async #claim(ref: TaskRef, me: string | undefined, options: { lease?: number | null }): Promise<number | null> {
    const who = this.#me(me);
    const lease = checkLease(options.lease);
    const found = await this.#existing(ref, false);
    if (found.state === "done") {
      return null;
    }
    if (found.firm && found.claimedBy === who) {
      // Only the time is written. If the task was taken over and this agent
      // has not heard yet, its name must not go back on it.
      if (!(await this.stillWorking(found, who))) {
        return null;
      }
      // A name and a number read together belong together: a takeover takes
      // the old name off before it writes the new number.
      const now = await this.#existing(found.id, false);
      return now.claimedBy === who && now.state !== "done" ? now.ticket : null;
    }
    const takingOver = found.claimedBy !== null && found.claimedBy !== who;
    if (takingOver && !abandoned(found)) {
      return null;
    }
    let ticket = found.ticket;
    if (found.firm) {
      if (takingOver) {
        // The agent that went quiet keeps its ticket. The task moves on to a
        // new one, already taken by this agent.
        if ((await this.#takeover(found.id, found.ticket).value()) > 1) {
          throw doubled(found, true);
        }
        const next = await this.#nextTicket(found.id, found.ticket, false);
        if (next === null) {
          return null;
        }
        ticket = next;
      } else {
        // A ticket holding more than one was made twice, and could go to two
        // agents. It is never taken.
        const own = this.#ticket(found.id, ticket);
        if ((await own.value()) > 1) {
          throw doubled(found, false);
        }
        if (!(await own.tryDecrement(1))) {
          return null;
        }
      }
    }
    let now: Task;
    let seen = 0;
    try {
      const batch = this.#team.client.batch();
      const board = batch.json(this.#board.name);
      // The writes of a batch are not seen all at once. On a task nobody
      // holds, the name goes first, so that the rest is written on a task this
      // agent holds: an agent that may change only its own tasks may write
      // nothing else on another's. On a task being taken over, the time goes
      // first, so that it never looks abandoned with its new ticket number on
      // it: another agent would take that ticket's takeover ticket too.
      seen = Date.now();
      if (!takingOver) {
        board.set([found.id, "claimed_by"], who);
      }
      board.set([found.id, "seen"], seen);
      board.set([found.id, "lease"], lease);
      if (ticket !== found.ticket) {
        // The old holder's name goes before the new number, so that the two
        // are never seen side by side: the old holder would take the new
        // number for its own.
        board.remove([found.id, "claimed_by"]);
        putTicket(board, found.id, ticket);
      }
      if (takingOver) {
        board.set([found.id, "claimed_by"], who);
      }
      board.set([found.id, "state"], "doing");
      sent(await batch.send());
      if (ticket !== found.ticket) {
        await this.#spent(found.id, [found.ticket]);
      }
      // Read back, in case another agent claimed the task, or finished it,
      // just after we looked.
      now = await this.#existing(found.id, false);
    } catch (error) {
      if (found.firm) {
        await this.#letGo(found.id, ticket);
      }
      throw error;
    }
    if (now.claimedBy === who && now.state !== "done" && now.ticket === ticket) {
      if (takingOver) {
        // Written once the task is known to be this agent's, so that it
        // never names an agent that did not get it.
        await this.#board.set([found.id, "taken_over"], { by: who, from: found.claimedBy, at: seen });
      }
      return ticket;
    }
    if (now.state === "done" && now.claimedBy === who && now.seen === seen && now.lease === lease && now.finishedBy !== who) {
      // The task was finished just before this claim was written. What the
      // claim wrote is put back as it was, so that the task does not name an
      // agent that did not work on it. Only if what is on the task is still
      // what this claim wrote: another program under the same name may have
      // claimed it since, rightly, or finished it.
      const batch = this.#team.client.batch();
      const board = batch.json(this.#board.name);
      // The name last, so that the rest is written while this agent still
      // holds the task.
      for (const [field, before] of [["seen", found.seen], ["lease", found.lease], ["claimed_by", found.claimedBy]] as const) {
        if (before === null) {
          board.remove([found.id, field]);
        } else {
          board.set([found.id, field], before);
        }
      }
      sent(await batch.send());
    }
    if (found.firm && now.state !== "done") {
      if (now.claimedBy === who && now.ticket !== ticket && now.state === "doing") {
        // The task moved on past this agent's ticket before it looked, as
        // when it was set back with force, and its name is beside a number
        // that is not its own. It is taken off, so that the task is free for
        // whoever takes the ticket on the board.
        const batch = this.#team.client.batch();
        const board = batch.json(this.#board.name);
        board.set([found.id, "state"], "todo");
        for (const field of ["claimed_by", "seen", "lease"]) {
          board.remove([found.id, field]);
        }
        sent(await batch.send());
      }
      // This agent holds the ticket but did not get the task. Kept, the
      // ticket would leave the task stuck.
      await this.#letGo(found.id, ticket);
    }
    return null;
  }

  /** Says that this agent is still working on a task it claimed, so that the
   * claim does not run out. Gives `false` if the task is no longer this
   * agent's: it was taken over, given back, set back by hand, or is done.
   * Then stop working on it. */
  async stillWorking(ref: TaskRef, me?: string): Promise<boolean> {
    const who = this.#me(me);
    const found = await this.#existing(ref, false);
    if (found.claimedBy !== who || found.state === "done") {
      return false;
    }
    await this.#board.set([found.id, "seen"], Date.now());
    // Read back, in case another agent took the task over just after we
    // looked.
    const now = await this.#existing(found.id, false);
    return now.claimedBy === who && now.state !== "done";
  }

  /** Gives a claimed task back, so that another agent can take it. Gives
   * `false`, and changes nothing, if the task is not this agent's. */
  async release(ref: TaskRef, me?: string): Promise<boolean> {
    const who = this.#me(me);
    const found = await this.#existing(ref, false);
    if (found.claimedBy !== who || found.state === "done") {
      return false;
    }
    if (found.firm) {
      // If another agent moved the task on first, it took the task over or
      // set it back, and the task is no longer this agent's to give.
      return this.#letGo(found.id, found.ticket);
    }
    const batch = this.#team.client.batch();
    const board = batch.json(this.#board.name);
    // The state first. If the holder went first, another agent could claim
    // the task in between and then have its state set back. The holder last,
    // so that the rest is written while this agent still holds the task.
    board.set([found.id, "state"], "todo");
    board.remove([found.id, "seen"]);
    board.remove([found.id, "lease"]);
    board.remove([found.id, "claimed_by"]);
    sent(await batch.send());
    return true;
  }

  /**
   * Marks a task done and keeps its result, which can be any JSON value.
   * Gives `true` if this finished it.
   *
   * A task that is already done stays as it is, with the result it has, and
   * this gives `false`. The result and the agent that finished the task are
   * written together, and read back: if two agents finish the same task at
   * the same moment, the one whose result was not kept is told `false`. Both
   * can still be told `true` when one has read back its own before the
   * other's write arrives, as on two machines that are cut off from each
   * other; the result kept is then the later one, beside the name of the
   * agent that wrote it.
   *
   * A firm task can only be finished by the agent that has it. Any other
   * agent is told `false`. To finish it by hand, use `update`.
   */
  async finish(ref: TaskRef, result: unknown = null, me?: string): Promise<boolean> {
    const found = await this.#existing(ref, false);
    if (found.state === "done") {
      return false;
    }
    const who = me ?? this.#team.me;
    if (found.firm && (who === null || found.claimedBy !== who)) {
      return false;
    }
    // The mark, the result and the name are one write, so a task seen as
    // done always has its result, and the result is always beside the name
    // of the agent that gave it. If it cannot be kept, this throws, and the
    // task is not marked done.
    const mark = finishMark();
    await this.#board.set([found.id, "finished"], { result, by: who, id: mark });
    // Read back, in case another agent finished it at the same moment. The
    // later write is the one kept.
    const kept = await this.#board.get([found.id, "finished"]);
    const done = typeof kept === "object" && kept !== null && (kept as Fields).id === mark;
    if (done && found.firm) {
      // The ticket this agent holds is never given back now. The one for
      // moving the task on is kept: setting the task back to "todo" takes it.
      await this.#spent(found.id, [found.ticket], true);
    }
    return done;
  }

  /**
   * Makes a task a sub-task of `parent`, or a top-level task again if
   * `parent` is left out or `null`. Gives `true` if it moved.
   *
   * A task cannot be put under itself or under one of its own sub-tasks.
   * Such a move is not made, and this gives `false`.
   */
  async move(ref: TaskRef, parent?: TaskRef | null): Promise<boolean> {
    const id = (await this.#existing(ref, false)).id;
    const parentId = parent != null ? (await this.#existing(parent, false)).id : null;
    await this.#tree.moveNode(id, parentId ?? ROOT);
    return (await this.#existing(id)).parent === parentId;
  }
}
