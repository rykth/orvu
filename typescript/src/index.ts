/**
 * Orvu's TypeScript client. It talks to an `orvud` sidecar.
 *
 * ```ts
 * import { Team } from "orvu";
 *
 * const team = await Team.connect("default", { me: "researcher-2" });
 * await team.notes.put("deploy-date", "Friday");
 * await team.close();
 * ```
 *
 * `Team` holds the building blocks a team of agents shares. `connect` gives
 * the plain shared objects underneath.
 *
 * @module
 */

export { Team } from "./team.ts";
export type { TeamOptions } from "./team.ts";
export { Notes, StructuredNote } from "./notes.ts";
export { abandoned, LEASE, STATES, Tasks } from "./tasks.ts";
export type { State, Takeover, Task, TaskRef } from "./tasks.ts";
export { Budget } from "./budget.ts";
export { Tally } from "./tally.ts";
export { Document } from "./document.ts";
export { Inbox } from "./inbox.ts";
export type { Message } from "./inbox.ts";
export { Roster } from "./roster.ts";
export type { Member } from "./roster.ts";
export { Batch, Client, connect, MAX_REPLY_BYTES } from "./client.ts";
export type { ConnectOptions } from "./client.ts";
export {
  BoundedCounter,
  Cart,
  Counter,
  Json,
  MvMap,
  RichText,
  Register,
  TextList,
  TextMap,
  TextSet,
  Tree,
} from "./handles.ts";
export type { JsonPath, Mode, Out, Span, TreeNode, WatchOptions } from "./handles.ts";
export { Pending } from "./pending.ts";
export type { Found } from "./client.ts";
export {
  BadRequest,
  Clash,
  ClassMismatch,
  EngineError,
  JsonError,
  NotAllowed,
  OrvuError,
  OutOfBounds,
  SearchUnavailable,
  SidecarUnavailable,
  TicketUnavailable,
  TimedOut,
  TooLarge,
  TypeMismatch,
  UnknownTask,
  VersionMismatch,
} from "./errors.ts";
export { VERSION } from "./version.ts";
