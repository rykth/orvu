/**
 * Everything that can go wrong, as named errors.
 *
 * A refusal is not one of these. When a counter that cannot go below zero has
 * nothing left, `tryDecrement` gives `false`. Nothing was changed and nothing
 * is broken, so it is an ordinary answer and not an error.
 *
 * @module
 */

/** The base of every error this package throws. */
export class OrvuError extends Error {
  constructor(message: string) {
    super(message);
    this.name = new.target.name;
  }
}

/**
 * The sidecar cannot be reached. It may not be running, or the socket path
 * may be wrong.
 */
export class SidecarUnavailable extends OrvuError {}

/**
 * The sidecar that answered is a different release from this package, so the
 * two may not understand each other. No operation was sent to it.
 */
export class VersionMismatch extends OrvuError {}

/**
 * The sidecar did not answer in time. A change is sent again a few times,
 * with the same id, before this is thrown, so the sidecar makes it at most
 * once. Thrown for a change, it does not say whether the change was made:
 * read the value before making it again.
 */
export class TimedOut extends OrvuError {}

/**
 * The request was wrong and was turned away before anything was done, either
 * by this package or by the sidecar. Nothing was changed.
 */
export class BadRequest extends OrvuError {}

/**
 * The name is already used by an object of another type. For example, it was
 * written as a counter and is now being used as a list.
 */
export class TypeMismatch extends OrvuError {}

/**
 * The name was declared under one set of rules and is being used under
 * another. A plain counter and a counter that cannot go below zero follow
 * different rules, so one name cannot be both.
 */
export class ClassMismatch extends OrvuError {}

/** A position was past the end of a list. */
export class OutOfBounds extends OrvuError {}

/**
 * A JSON path or value could not be used. For example, the path goes through
 * a list position to change something inside that item.
 */
export class JsonError extends OrvuError {}

/**
 * Something was too large to send or to read, or there were too many. A
 * request over 64 MiB is not taken by the sidecar, and neither is a live
 * update past the most a client or a sidecar may have open. A reply over the
 * client's `maxReplyBytes` is not read. In a batch, an operation is not run
 * once the replies before it have grown past 4 MiB. A number too large for a
 * JavaScript number is not read either.
 */
export class TooLarge extends OrvuError {}

/**
 * The engine could not do what was asked, for a reason that has no more
 * specific error.
 */
export class EngineError extends OrvuError {}

/**
 * Search by meaning could not be done. The model it needs could not be
 * fetched or read, or the sidecar was started not to search. The message
 * says which. The first search fetches the model from the internet, 30 MB,
 * unless the sidecar was given one with `--model` or `ORVU_MODEL`.
 */
export class SearchUnavailable extends OrvuError {}

/**
 * A firm task could not be set back by hand, because its ticket for that
 * could not be had. Another agent took the task over or set it back at the
 * same moment, or the machine that holds the ticket cannot be reached.
 * Nothing was changed. If it keeps happening because an agent stopped for
 * good half way through, `update(task, { state: "todo", force: true })` sets
 * the task back all the same.
 */
export class TicketUnavailable extends OrvuError {}

/**
 * The sidecar did not let this program in, or this agent's rules do not allow
 * what it asked. A sidecar that names agents lets in only programs that give
 * one of their keys, and an agent with rules may do only what they allow. The
 * message says which. An agent that was removed is told this at its next
 * call. Nothing was changed.
 */
export class NotAllowed extends OrvuError {}

/** There is no task with that id on the team's board. */
export class UnknownTask extends OrvuError {}

/**
 * Two or more agents wrote a note at the same time, neither having seen the
 * other's, and nothing has settled it yet. `values` holds every value
 * written, and `authors` who wrote each, in the same order: `null` for a
 * program that gave no name. Writing the note again settles it.
 *
 * `notes.all()` and `notes.watch()` give a note that clashes as one of these,
 * and `notes.get` throws it.
 */
export class Clash extends OrvuError {
  readonly note: string;
  readonly values: unknown[];
  readonly authors: (string | null)[];

  constructor(note: string, values: unknown[], authors: (string | null)[]) {
    const shown = values.map((value) => JSON.stringify(value)).join(", ");
    super(
      `the note ${JSON.stringify(note)} was written at the same time by more than one agent, and holds ${shown}. ` +
        "Write it again to settle it",
    );
    this.note = note;
    this.values = values;
    this.authors = authors;
  }
}
