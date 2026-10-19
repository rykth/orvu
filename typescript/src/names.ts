/**
 * How the building blocks name the shared objects they keep their data in.
 *
 * Every object a team uses is called `orvu/<team>/<block>...`. Each block has
 * a first word of its own after the team's name, and no name a caller gives
 * may contain a slash, so two blocks can never end up using the same object.
 * The names are the ones the Python package uses, so a Python agent and a
 * Node agent on one team share the same objects.
 *
 * @module
 */

import { BadRequest } from "./errors.ts";

export const PREFIX = "orvu";

/** The largest amount or count that can be sent in one go. */
export const LARGEST = Number.MAX_SAFE_INTEGER;

// Letters, digits, dots, dashes and underscores, starting with a letter or a
// digit, and at most 64 characters.
const PART = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

/** Checks that `text` can be one part of an object's name, and gives it.
 * `what` says what it names, for the error message. */
export function part(text: unknown, what: string): string {
  if (typeof text !== "string" || !PART.test(text)) {
    throw new BadRequest(
      `${JSON.stringify(text) ?? String(text)} cannot be used as ${what}. Use letters, digits, dots, dashes and ` +
        `underscores, starting with a letter or a digit, and at most 64 characters`,
    );
  }
  return text;
}

/** Checks that `text` can be an agent's name, and gives it. Any text will
 * do. */
export function agent(text: unknown, what = "the name of an agent"): string {
  if (typeof text !== "string" || text === "") {
    throw new BadRequest(`${JSON.stringify(text) ?? String(text)} cannot be used as ${what}. It has to be some text`);
  }
  return text;
}

export const index = (team: string) => `${PREFIX}/${team}/index`;
/** The team's simple notes. They are kept in a map that keeps values written
 * at once; the sidecar moves notes kept the old way over to it. */
export const notes = (team: string) => `${PREFIX}/${team}/notes-mv`;
export const note = (team: string, name: string) => `${PREFIX}/${team}/note/${name}`;
export const document = (team: string, name: string) => `${PREFIX}/${team}/doc/${name}`;
export const tally = (team: string, name: string) => `${PREFIX}/${team}/tally/${name}`;
export const roster = (team: string) => `${PREFIX}/${team}/roster`;
export const tasks = (team: string) => `${PREFIX}/${team}/tasks`;
export const taskTree = (team: string) => `${PREFIX}/${team}/tasks-tree`;
/** The ticket of a firm task. Whoever takes it has the task. A task gets a
 * new ticket, with the next number, each time it is taken over or set back
 * by hand. */
export const taskTicket = (team: string, id: string, number: number) => `${PREFIX}/${team}/task-ticket/${id}/${number}`;
/** The ticket for taking a firm task over from an agent that went quiet, or
 * setting it back by hand, while it is on ticket `number`. Only one agent
 * can, and that agent makes the next ticket. */
export const taskTakeover = (team: string, id: string, number: number) => `${PREFIX}/${team}/task-takeover/${id}/${number}`;
export const budget = (team: string, name: string) => `${PREFIX}/${team}/budget/${name}`;
export const spent = (team: string, name: string) => `${PREFIX}/${team}/budget-spent/${name}`;
/** How much each agent has topped one budget up by. */
export const topped = (team: string, name: string) => `${PREFIX}/${team}/budget-topped/${name}`;

/** The inbox every agent reads. An agent's own inbox is kept under its name
 * with every character that is not a letter or a digit written as a code,
 * "@" among them, so this is never somebody's own inbox. */
export const EVERYONE = "@everyone";

/** The part of an object's name that stands for an agent's inbox: the
 * agent's name, made safe to be one part of a name the way Python's
 * `urllib.parse.quote(name, safe="")` makes it, so that a Python agent and a
 * Node agent find the same inbox. */
export function inboxOwner(agentName: string): string {
  return encodeURIComponent(agentName).replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
}

export const inboxPage = (team: string, owner: string, page: number) => `${PREFIX}/${team}/inbox/${owner}/${page}`;
export const inboxGiven = (team: string, reader: string, owner: string, page: number) =>
  `${PREFIX}/${team}/inbox-given/${reader}/${owner}/${page}`;
export const inboxPages = (team: string, owner: string) => `${PREFIX}/${team}/inbox-page/${owner}`;
/** How many messages were written to each page of an inbox, by page number.
 * A sender adds one in the same message as it writes. */
export const inboxCount = (team: string, owner: string) => `${PREFIX}/${team}/inbox-count/${owner}`;
/** How many messages one agent has been given from each page of an inbox, by
 * page number. */
export const inboxDone = (team: string, reader: string, owner: string) => `${PREFIX}/${team}/inbox-done/${reader}/${owner}`;
