/**
 * The connection to the sidecar.
 *
 * @module
 */

import * as grpc from "@grpc/grpc-js";
import * as loader from "@grpc/proto-loader";
import { randomBytes } from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { BadRequest, EngineError, NotAllowed, OrvuError, SidecarUnavailable, TimedOut, TooLarge, VersionMismatch } from "./errors.ts";
import { Objects } from "./handles.ts";
import * as names from "./names.ts";
import { Pending } from "./pending.ts";
import * as sidecar from "./sidecar.ts";
import { release } from "./version.ts";
import { failure, jsonList, Op } from "./wire.ts";
import type { Message, Reader } from "./wire.ts";

/** The largest reply a client reads unless told otherwise: 64 MiB. */
export const MAX_REPLY_BYTES = 64 * 1024 * 1024;

// The namespace every local sidecar serves.
const LOCAL = "local";

const CLASSES: Record<string, string> = { ap: "CLASS_AP", escrow: "CLASS_ESCROW" };

// The messages are read from the `.proto` file that comes with the package.
// Field names stay as they are there, 64-bit numbers arrive as text so that
// none is cut short, and enums are given by name.
const definition = loader.loadSync(fileURLToPath(new URL("../proto/dataplane.proto", import.meta.url)), {
  keepCase: true,
  longs: String,
  enums: String,
  defaults: false,
  oneofs: true,
});
const DataPlane = (grpc.loadPackageDefinition(definition) as Message).orvu.dataplane.v1
  .DataPlane as grpc.ServiceClientConstructor;

/** How to connect. */
export interface ConnectOptions {
  /** The namespace to work in. Left out, it is whatever the sidecar serves. */
  namespace?: string;
  /** The agent's key, as `orvud agent add` printed it. A sidecar that names
   * agents lets in only programs that give one, and tells each which agent
   * it is: see `Client.agent`. Left out or `null`, it is `ORVU_KEY` from the
   * environment, if that is set. A key the sidecar does not take throws
   * `NotAllowed`. */
  key?: string | null;
  /** The path of a connection file that `orvud agent add --connection`
   * wrote, for a sidecar on another machine, or outside this container,
   * reached over the network. The file holds where it is, the agent's key,
   * and the certificate that proves the sidecar is the right one; the
   * connection is encrypted. Left out or `null`, it is `ORVU_CONNECT` from
   * the environment, if that is set and no socket is given. */
  connection?: string | null;
  /** How long to wait for the sidecar, starting it included, and for each
   * answer, in milliseconds. The default is ten seconds. */
  timeoutMs?: number;
  /** The largest reply to read. A larger one throws `TooLarge`. */
  maxReplyBytes?: number;
}

/** What a program is told when nothing answers on the socket it named. Such
 * a sidecar is never started by the package, as one that keeps its data on
 * this machine is. */
const START_A_TEAMS_ONE =
  "A sidecar shared with other machines is started by hand: `orvud setup` makes its settings, " +
  "and `orvud --config orvu.toml --socket <socket>` starts it";

/**
 * Connects to a sidecar.
 *
 * With no `socket`, this is the sidecar every program of this user shares. If
 * it is not running yet it is started, and it stops by itself a while after
 * the last program has disconnected.
 *
 * With a `socket`, this is the sidecar that serves on that path, such as a
 * team's sidecar shared with other machines. It has to be running already:
 * `orvud setup` makes the settings for one, and it is started by hand.
 * Throws `SidecarUnavailable` if nothing is listening. Left out, it is
 * `ORVU_SOCKET` from the environment, if that is set and no `connection` is
 * given.
 *
 * A sidecar of another release throws `VersionMismatch`.
 */
export async function connect(socket?: string, options: ConnectOptions = {}): Promise<Client> {
  const timeoutMs = options.timeoutMs ?? 10_000;
  const connection = options.connection ?? (socket === undefined ? process.env.ORVU_CONNECT || undefined : undefined);
  if (socket === undefined && options.connection === undefined) {
    socket = process.env.ORVU_SOCKET || undefined;
    if (socket !== undefined && connection !== undefined) {
      throw new BadRequest("ORVU_SOCKET and ORVU_CONNECT are both set: set only one");
    }
  }
  if (connection !== undefined) {
    if (socket !== undefined) {
      throw new BadRequest("give a socket or a connection file, not both");
    }
    const { address, key, certificate } = readConnection(connection);
    if (options.key != null && options.key !== "" && options.key !== key) {
      throw new BadRequest("the connection file holds the agent's key: give no other");
    }
    return Client.open(address, {
      namespace: options.namespace ?? "",
      key,
      timeoutMs,
      maxReplyBytes: options.maxReplyBytes ?? MAX_REPLY_BYTES,
      restart: false,
      readyWithinMs: timeoutMs,
      network: { address, certificate },
    });
  }
  const key = options.key ?? process.env.ORVU_KEY ?? "";
  if (typeof key !== "string") {
    throw new BadRequest(`a key is text, as \`orvud agent add\` printed it, not ${typeof key}`);
  }
  const settings = {
    namespace: options.namespace ?? "",
    key,
    timeoutMs,
    maxReplyBytes: options.maxReplyBytes ?? MAX_REPLY_BYTES,
  };
  if (socket !== undefined) {
    try {
      return await Client.open(socket, { ...settings, restart: false, readyWithinMs: timeoutMs });
    } catch (error) {
      if (error instanceof SidecarUnavailable) {
        throw new SidecarUnavailable(`${error.message}. ${START_A_TEAMS_ONE}`);
      }
      throw error;
    }
  }
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const left = Math.max(deadline - Date.now(), 500);
    const found = await sidecar.ensureRunning(left);
    try {
      // A sidecar in its last moment may still hold its socket open without
      // answering on it. So each try waits only a short while, and the next
      // one finds the socket gone and starts a sidecar.
      return await Client.open(found, {
        ...settings,
        namespace: settings.namespace || LOCAL,
        restart: true,
        readyWithinMs: Math.min(left, 2_000),
      });
    } catch (error) {
      if (error instanceof SidecarUnavailable) {
        if (Date.now() >= deadline) {
          throw error;
        }
        continue;
      }
      if (error instanceof BadRequest && !options.namespace) {
        throw new SidecarUnavailable(
          `the sidecar on ${found} is not a local one (${error.message}). connect() only uses a ` +
            `sidecar that keeps its data on this machine. To use that one, give its socket: ` +
            `connect(${JSON.stringify(found)}), or set ORVU_SOCKET to it`,
        );
      }
      throw error;
    }
  }
}

/** What a connection file says: the address, the key and the certificate. */
function readConnection(file: string): { address: string; key: string; certificate: string } {
  let text: string;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch (error) {
    throw new BadRequest(`cannot read the connection file ${file}: ${(error as Error).message}`);
  }
  let found: unknown;
  try {
    found = JSON.parse(text);
  } catch {
    found = null;
  }
  const fields = found as Record<string, unknown> | null;
  if (
    fields === null ||
    typeof fields !== "object" ||
    !["address", "key", "certificate"].every((name) => typeof fields[name] === "string")
  ) {
    throw new BadRequest(`${file} is not a connection file: \`orvud agent add --connection\` writes one`);
  }
  return { address: fields.address as string, key: fields.key as string, certificate: fields.certificate as string };
}

// The name the certificate of a sidecar reached over the network is made out
// to, whatever its address.
const NETWORK_NAME = "orvud";

/** True if the two versions can work together: the first two numbers are
 * the same. The third is for fixes that change nothing a client can see. */
function sameRelease(ours: string, theirs: string): boolean {
  return ours.split(".").slice(0, 2).join(".") === theirs.split(".").slice(0, 2).join(".");
}

/** True if the call failed because nothing could be reached, so the sidecar
 * cannot have acted on it. A call that was cut off partway is not one of
 * these: the sidecar may have done what it asked. */
function neverArrived(error: grpc.ServiceError): boolean {
  return error.code === grpc.status.UNAVAILABLE && /no connection established/i.test(error.details ?? "");
}

/** How many more times a change is sent when its answer did not come. */
const SEND_AGAIN = 3;

/** A new id for one change: 16 random bytes. The change is sent with it every
 * time it is sent, so the sidecar makes it once. */
function requestId(): Buffer {
  return randomBytes(16);
}

/** True if the call may or may not have been made, and no answer came: the
 * sidecar could not be reached, stopped partway, or took too long. */
function unanswered(error: grpc.ServiceError): boolean {
  return error.code === grpc.status.UNAVAILABLE || error.code === grpc.status.DEADLINE_EXCEEDED;
}

/** True if the sidecar turned the call away because it is stopping. It did
 * not run it, so it is as safe to make again as a call that never arrived. */
function stopping(error: grpc.ServiceError): boolean {
  return error.code === grpc.status.UNAVAILABLE && error.details === "the sidecar is stopping";
}

/** The error to throw for a gRPC failure. The sidecar uses these only for a
 * request that is itself wrong, or when it cannot be reached. */
function translate(error: grpc.ServiceError): OrvuError {
  switch (error.code) {
    case grpc.status.INVALID_ARGUMENT:
      return new BadRequest(error.details);
    case grpc.status.PERMISSION_DENIED:
      return new NotAllowed(error.details);
    case grpc.status.UNAVAILABLE:
      return new SidecarUnavailable(error.details || "the sidecar did not answer");
    case grpc.status.DEADLINE_EXCEEDED:
      return new TimedOut("the sidecar did not answer in time. What was asked may still have been done");
    // A reply larger than this client reads, a request larger than the
    // sidecar takes, or more live updates open than the sidecar allows.
    case grpc.status.RESOURCE_EXHAUSTED:
    case grpc.status.OUT_OF_RANGE:
      return new TooLarge(error.details);
    default:
      return new EngineError(`${grpc.status[error.code]}: ${error.details}`);
  }
}

function isServiceError(error: unknown): error is grpc.ServiceError {
  return error instanceof Error && typeof (error as grpc.ServiceError).code === "number";
}

interface Settings {
  namespace: string;
  key: string;
  timeoutMs: number;
  maxReplyBytes: number;
  /** True for a client of the sidecar this package starts. Such a client
   * starts it again when it finds it gone. */
  restart: boolean;
  readyWithinMs: number;
  /** For a sidecar reached over the network: where, and the certificate it
   * has to show. */
  network?: { address: string; certificate: string };
}

/** How long a search waits for its answer. The first search on a sidecar may
 * fetch the model it needs, 30 MB, from the internet. */
const SEARCH_WAIT_MS = 600_000;

/** How long a watch's new session has to last before the watch may open
 * another when that one is dropped too. A watch is sent its value as soon as
 * it opens, so a value arriving says nothing about whether the new session
 * will last. It is timed with `performance.now()`, which only counts time
 * going by, so setting the system clock does not cut it short. */
const SETTLED_AFTER_MS = 10_000;

/**
 * A note, a part of a document or a task that a search found. `kind` is
 * "note" for a simple note, "structured" for a structured note, "document"
 * for a paragraph of a document and "task" for a task. `name` is the note's
 * or the document's name, or the task's id. `paragraph` is the paragraph's
 * number in the document, counted from 1, and `title` the task's title.
 * `text` is what was searched: for a note, its name and what it says, and
 * for a task, its title and its notes. `score` says how close its meaning is
 * to the question's, from 0 to 1.
 */
export interface Found {
  kind: "note" | "structured" | "document" | "task";
  name: string;
  text: string;
  score: number;
  paragraph?: number;
  title?: string;
}

/**
 * A connection to one sidecar. Get objects from it by name:
 *
 * ```ts
 * const client = await connect("/run/user/1000/orvud.sock");
 * await client.counter("visits").increment(5);
 * ```
 *
 * Close it with `close()`, or with `await using`.
 */
export class Client extends Objects<"now"> {
  #socket: string;
  readonly #settings: Settings;
  #stub: grpc.Client & Message;
  #session = "";
  #resumeToken = "";
  readonly #watches = new Set<grpc.ClientReadableStream<Message>>();
  #closed = false;
  /** What is being done to carry on after the sidecar restarted or stopped,
   * shared by every call that finds it so. */
  #recovering: Promise<void> | null = null;
  /** The number of the machine the sidecar runs as. It is a `bigint`
   * because a machine of a team may have any number up to 2^63 - 1. */
  replicaId = 0n;
  /** The version of the sidecar, such as "0.1.0". */
  sidecarVersion = "";
  /** The agent whose key this client gave, as the sidecar checked it, or
   * `null` if it gave none. */
  agent: string | null = null;

  private constructor(socket: string, settings: Settings) {
    super();
    // A sidecar on this machine is known by its socket, and one on another by
    // its address.
    this.#socket = settings.network ? socket : path.resolve(socket);
    this.#settings = settings;
    this.#stub = this.#newStub();
  }

  /** @internal Connects and opens a session. */
  static async open(socket: string, settings: Settings): Promise<Client> {
    const client = new Client(socket, settings);
    try {
      // Over the network, a sidecar that shows another certificate looks the
      // same as one that is not there.
      const also = settings.network
        ? ", or the one there showed a certificate that is not the one in the connection file"
        : "";
      await client.#ready(settings.readyWithinMs, `no sidecar answered on ${socket} within ${settings.timeoutMs / 1000} seconds${also}`);
      await client.#open();
    } catch (error) {
      client.close();
      throw error;
    }
    return client;
  }

  #newStub(): grpc.Client & Message {
    const network = this.#settings.network;
    if (network) {
      // Encrypted, and trusting only the sidecar's own certificate. It is made
      // out to a name and not an address, so that it does for whatever
      // address the sidecar is reached by.
      return new DataPlane(network.address, grpc.credentials.createSsl(Buffer.from(network.certificate)), {
        "grpc.ssl_target_name_override": NETWORK_NAME,
        "grpc.default_authority": NETWORK_NAME,
        "grpc.max_receive_message_length": this.#settings.maxReplyBytes,
      }) as grpc.Client & Message;
    }
    return new DataPlane(`unix:${this.#socket}`, grpc.credentials.createInsecure(), {
      // Over a Unix socket, gRPC would send the socket's path as the name of
      // the server it is talking to. A path is not a valid server name, and
      // the sidecar drops such a request. Any ordinary name will do.
      "grpc.default_authority": "localhost",
      "grpc.max_receive_message_length": this.#settings.maxReplyBytes,
    }) as grpc.Client & Message;
  }

  #ready(withinMs: number, message: string): Promise<void> {
    const stub = this.#stub;
    return new Promise((resolve, reject) => {
      stub.waitForReady(Date.now() + withinMs, (error?: Error) =>
        error ? reject(new SidecarUnavailable(message)) : resolve(),
      );
    });
  }

  /** One call, which waits for the client's timeout, or for `waitMs` if that
   * is longer. */
  #unary(method: string, request: Message, stub = this.#stub, waitMs = 0): Promise<Message> {
    this.#checkOpen();
    return new Promise((resolve, reject) => {
      stub[method](
        request,
        { deadline: Date.now() + Math.max(this.#settings.timeoutMs, waitMs) },
        (error: grpc.ServiceError | null, reply: Message) => (error ? reject(error) : resolve(reply)),
      );
    });
  }

  /** Closes the connection. Loops over `watch()` end, and every call after
   * this throws `BadRequest`. */
  close(): void {
    this.#closed = true;
    for (const call of this.#watches) {
      call.cancel();
    }
    this.#stub.close();
  }

  #checkOpen(): void {
    if (this.#closed) {
      throw new BadRequest("the client is closed: connect again");
    }
  }

  async [Symbol.asyncDispose](): Promise<void> {
    this.close();
  }

  // ---- sessions ------------------------------------------------------------

  /** Starts a session, or picks the old one up again if the sidecar still
   * knows it. */
  async #open(raw = false): Promise<void> {
    let opened: Message;
    try {
      opened = await this.#unary("Open", {
        namespace: this.#settings.namespace,
        resume_token: this.#resumeToken,
        credential: Buffer.from(this.#settings.key),
      });
    } catch (error) {
      // With `raw`, a failed call is thrown as it came, for `call` to look at.
      throw isServiceError(error) && !raw ? translate(error) : error;
    }
    // Checked before the session is kept, so that a sidecar of another
    // release is refused on every call and not only on this one.
    const theirs = opened.version ?? "";
    if (!sameRelease(release.version, theirs)) {
      this.#session = this.#resumeToken = "";
      // The process id is the sidecar's own word, and means something only
      // on this machine, so it is named only for a sidecar on a socket.
      const pid = this.#settings.network ? 0 : Number(opened.pid ?? 0);
      const stop = pid > 0 ? `Stop that sidecar with \`kill ${pid}\`` : "Stop that sidecar";
      throw new VersionMismatch(
        `the sidecar on ${this.#socket} is version ${theirs || "unknown"}, and this package is version ` +
          `${release.version}. They have to be the same release. ${stop} and connect again, or ` +
          `install the matching version of this package. It will not stop by itself while programs keep ` +
          `trying to connect to it`,
      );
    }
    this.#session = opened.session ?? "";
    this.#resumeToken = opened.resume_token ?? "";
    this.replicaId = BigInt(opened.replica_id ?? 0);
    this.sidecarVersion = theirs;
    this.agent = opened.agent || null;
  }

  /** Whether `close()` was called. */
  get closed(): boolean {
    return this.#closed;
  }

  /** @internal How many watches are open, for tests. */
  get watching(): number {
    return this.#watches.size;
  }

  /** @internal How long a watch's new session has to last, changed by tests
   * so as not to wait the whole while. */
  settledAfterMs = SETTLED_AFTER_MS;

  /** @internal The session in use, for tests. */
  get session(): string {
    return this.#session;
  }

  /**
   * @internal Makes one call. If the sidecar no longer knows our session,
   * which happens after it restarts, a new session is opened and the call is
   * made once more. A client of the sidecar this package starts also starts
   * it again if it has stopped, or once the one that is stopping has gone.
   *
   * Calls made at the same moment may all find this. They share one new
   * session, and one start of the sidecar. The sidecar started for the call
   * may itself be told to stop before the call gets there, so that is done
   * as often as it takes, for as long as the client waits for an answer.
   *
   * With `again`, the call carries a request id, and the sidecar makes it
   * once however often it arrives. So it is sent again, with the same id,
   * whenever its answer did not come, once the sidecar is back.
   */
  async call(method: string, request: (session: string) => Message, waitMs?: number, again = false): Promise<Message> {
    if (again) {
      return this.#callAgain(method, request, waitMs);
    }
    let [stub, session] = [this.#stub, this.#session];
    let restart: boolean;
    try {
      return await this.#unary(method, request(session), stub, waitMs);
    } catch (error) {
      if (!isServiceError(error)) {
        throw error;
      }
      restart = this.#settings.restart && (neverArrived(error) || stopping(error));
      if (!restart && error.code !== grpc.status.UNAUTHENTICATED) {
        throw translate(error);
      }
    }
    const deadline = Date.now() + this.#settings.timeoutMs;
    for (;;) {
      try {
        if (restart) {
          const failed = stub;
          await this.#recover(
            async () => {
              await this.#startAgain();
              await this.#open(true);
            },
            () => this.#stub !== failed,
          );
        } else {
          const failed = session;
          await this.#recover(() => this.#open(true), () => this.#session !== failed);
        }
        [stub, session] = [this.#stub, this.#session];
        return await this.#unary(method, request(session), stub, waitMs);
      } catch (error) {
        if (!isServiceError(error)) {
          throw error;
        }
        restart = this.#settings.restart && (neverArrived(error) || stopping(error));
        if (!restart || Date.now() >= deadline) {
          throw translate(error);
        }
        // What failed is what is in use now, the new session too if its
        // opening is what failed.
        [stub, session] = [this.#stub, this.#session];
      }
    }
  }

  /** Sends a call that carries a request id, and sends it again when its
   * answer did not come. */
  async #callAgain(method: string, request: (session: string) => Message, waitMs?: number): Promise<Message> {
    let failed: { error: grpc.ServiceError; stub: grpc.Client; session: string } | null = null;
    let left = SEND_AGAIN;
    const deadline = Date.now() + this.#settings.timeoutMs;
    for (;;) {
      let [stub, session] = [this.#stub, this.#session];
      try {
        if (failed !== null) {
          await this.#carryOn(failed.error, failed.stub, failed.session);
          [stub, session] = [this.#stub, this.#session];
        }
        // Every send is the same request, with the same id.
        return await this.#unary(method, request(session), stub, waitMs);
      } catch (error) {
        if (!isServiceError(error)) {
          throw error;
        }
        if (!unanswered(error) && error.code !== grpc.status.UNAUTHENTICATED) {
          throw translate(error);
        }
        // A call that never got to the sidecar, or that a sidecar which was
        // stopping turned away, was not made. It is tried again for as long
        // as the client waits, as any call is. Any other is sent again a few
        // times at most.
        if (this.#settings.restart && (neverArrived(error) || stopping(error))) {
          if (Date.now() >= deadline) {
            throw translate(error);
          }
        } else if (left === 0) {
          throw translate(error);
        } else {
          left -= 1;
        }
        // What failed is what is in use now, the new session too if its
        // opening is what failed.
        failed = { error, stub: this.#stub, session: this.#session };
      }
    }
  }

  /** Gets ready to send a call again after `error`, which a call made on
   * `stub` with `session` met: a new session, and before that, if the
   * sidecar could not be reached, the sidecar back. */
  async #carryOn(error: grpc.ServiceError, stub: grpc.Client, session: string): Promise<void> {
    if (error.code === grpc.status.UNAVAILABLE) {
      const back = this.#settings.restart ? () => this.#startAgain() : () => this.#waitForSidecar();
      await this.#recover(
        async () => {
          await back();
          await this.#open(true);
        },
        () => this.#stub !== stub,
      );
    } else if (error.code === grpc.status.UNAUTHENTICATED) {
      await this.#recover(() => this.#open(true), () => this.#session !== session);
    }
  }

  /** Waits for a sidecar this package does not start to answer again, for
   * the client's timeout at most. */
  async #waitForSidecar(): Promise<void> {
    const deadline = Date.now() + this.#settings.timeoutMs;
    for (;;) {
      const left = Math.max(deadline - Date.now(), 100);
      this.#checkOpen();
      // As in `#startAgain`, a new connection tries at once.
      this.#stub.close();
      this.#stub = this.#newStub();
      try {
        await this.#ready(
          Math.min(left, 1_000),
          `the sidecar on ${this.#socket} stopped and did not come back within ${this.#settings.timeoutMs / 1000} ` +
            "seconds. What was asked may have been done",
        );
        return;
      } catch (error) {
        if (!(error instanceof SidecarUnavailable) || Date.now() >= deadline) {
          throw error;
        }
      }
    }
  }

  /** Runs `fix`, unless another call is running it already, in which case
   * this waits for that one. `done` says whether the fix has been made since
   * this call failed, so that it is not made twice. */
  async #recover(fix: () => Promise<void>, done: () => boolean): Promise<void> {
    while (this.#recovering !== null) {
      await this.#recovering.catch(() => {});
    }
    if (done()) {
      return;
    }
    this.#recovering = fix();
    try {
      await this.#recovering;
    } finally {
      this.#recovering = null;
    }
  }

  /** Starts the sidecar again and connects to it afresh. */
  async #startAgain(): Promise<void> {
    const deadline = Date.now() + this.#settings.timeoutMs;
    for (;;) {
      const left = Math.max(deadline - Date.now(), 500);
      // It may come back on another socket than the one it was found on.
      this.#socket = path.resolve(await sidecar.ensureRunning(left));
      // Closed while the sidecar was being started: no new connection.
      this.#checkOpen();
      // The old connection has learnt that nothing is there and would wait
      // a while before trying again. A new one tries at once.
      this.#stub.close();
      this.#stub = this.#newStub();
      // A sidecar that is stopping holds its socket open without answering
      // on it until it has finished what it was doing, for a few seconds at
      // most. So each try waits only a short while, as `connect()` does, and
      // a later one finds the socket gone and starts a new sidecar.
      try {
        await this.#ready(Math.min(left, 2_000), `the sidecar was started again but did not answer on ${this.#socket}`);
        return;
      } catch (error) {
        if (!(error instanceof SidecarUnavailable) || Date.now() >= deadline) {
          throw error;
        }
      }
    }
  }

  // ---- operations ----------------------------------------------------------

  /** @internal */
  async run<T>(op: Op<T>, waitMs?: number): Promise<T> {
    // A change is sent with an id, the same each time it is sent.
    const sentAs = op.changes ? requestId() : undefined;
    return op.result(await this.call("Execute", (session) => op.request(session, sentAs), waitMs, op.changes));
  }

  /** @internal */
  refuse<T>(error: unknown): Promise<T> {
    return Promise.reject(error);
  }

  /**
   * Says up front which rules the object `name` follows: `"ap"` for the types
   * that always accept a change, `"escrow"` for a counter that cannot go below
   * zero. Using the name the other way afterwards throws `ClassMismatch`. This
   * is optional: a name is given its class by the first operation on it
   * anyway.
   */
  async declare(name: string, objectClass: "ap" | "escrow"): Promise<void> {
    const wanted = CLASSES[objectClass];
    if (wanted === undefined) {
      throw new BadRequest(`objectClass must be "ap" or "escrow", not ${JSON.stringify(objectClass)}`);
    }
    await this.run(new Op("declare", { object: name, object_class: wanted }, () => undefined));
  }

  /**
   * The notes, documents and tasks of the team called `team` whose meaning is
   * closest to `query`, the closest first, at most `limit` of them, from 1 to
   * 50. An agent with rules finds only what it may read. `Team.search` does
   * the same for a team. A search waits ten minutes for its answer,
   * or `timeoutMs` if that is longer, as the first one on a sidecar may fetch
   * the model.
   */
  async search(team: string, query: string, limit = 5): Promise<Found[]> {
    if (typeof query !== "string") {
      throw new BadRequest(`a search is for some text, not ${JSON.stringify(query)}`);
    }
    if (!Number.isInteger(limit) || limit < 1 || limit > 50) {
      throw new BadRequest(`a search finds from 1 to 50, not ${JSON.stringify(limit)}`);
    }
    const index = names.index(names.part(team, "the name of a team"));
    return (await this.run(new Op("search", { object: index, query, limit }, jsonList), SEARCH_WAIT_MS)) as Found[];
  }

  /** Several operations sent in one message. See `Batch`. */
  batch(): Batch {
    return new Batch(this);
  }

  // ---- watching ------------------------------------------------------------

  /** @internal */
  async *watch<T>(kind: string, name: string, read: Reader<T>, signal?: AbortSignal): AsyncGenerator<T> {
    // When a session was last opened again for this watch, or null.
    let reopened: number | null = null;
    for (;;) {
      this.#checkOpen();
      if (signal?.aborted) {
        return;
      }
      const call: grpc.ClientReadableStream<Message> = this.#stub.Watch({ session: this.#session, object: name, kind });
      this.#watches.add(call);
      const stop = () => call.cancel();
      signal?.addEventListener("abort", stop, { once: true });
      try {
        for await (const event of call) {
          if (event.event === "failed") {
            throw failure(event.failed);
          }
          yield read(event.value);
        }
        return;
      } catch (error) {
        if (!isServiceError(error)) {
          throw error;
        }
        // The loop ends quietly when the client was closed.
        if (error.code === grpc.status.CANCELLED) {
          return;
        }
        // The sidecar no longer knows our session. That happens after it
        // restarts, and on a busy sidecar that had to drop it to make room.
        // Open a new one and start watching again, as often as that
        // happens, but not when the last new one was dropped too within a
        // few seconds: on a sidecar with more watches than places, that would
        // go round for ever.
        const tooSoon = reopened !== null && performance.now() - reopened < this.settledAfterMs;
        if (error.code !== grpc.status.UNAUTHENTICATED || tooSoon) {
          throw translate(error);
        }
      } finally {
        signal?.removeEventListener("abort", stop);
        this.#watches.delete(call);
        call.cancel();
      }
      const session = this.#session;
      await this.#recover(() => this.#open(), () => this.#session !== session);
      reopened = performance.now();
    }
  }
}

/**
 * Several operations sent to the sidecar in one message.
 *
 * ```ts
 * const batch = client.batch();
 * batch.counter("visits").increment(1);
 * const left = batch.boundedCounter("budget").tryDecrement(3);
 * await batch.send();
 * console.log(left.value);
 * ```
 *
 * Each method gives a `Pending`. After `send()`, each `Pending` has its
 * `value`.
 *
 * A batch is not all or nothing. The operations run in order and each has its
 * own result. The one exception is a request that is itself wrong: then the
 * whole batch throws `BadRequest` and nothing has run.
 *
 * A batch is sent once. Sending it again does nothing, and nothing can be
 * added to it afterwards. A mistake in the arguments of an operation is
 * thrown at once, and the batch is then not sent at all.
 */
export class Batch extends Objects<"batch"> {
  readonly #client: Client;
  readonly #pending: Pending<unknown>[] = [];
  #sent = false;
  #sending: Promise<readonly Pending<unknown>[]> | null = null;
  /** A mistake made while the batch was filled. Such a batch is not sent. */
  #spoiled: Error | null = null;

  /** @internal */
  constructor(client: Client) {
    super();
    this.#client = client;
  }

  /** The `Pending` results, in the order the operations were added. */
  get results(): readonly Pending<unknown>[] {
    return this.#pending;
  }

  /** @internal */
  run<T>(op: Op<T>): Pending<T> {
    if (this.#sent || this.#sending !== null) {
      throw new BadRequest("the batch has been sent already: start a new one");
    }
    const pending = new Pending(op);
    this.#pending.push(pending as Pending<unknown>);
    return pending;
  }

  /** @internal */
  refuse<T>(error: unknown): Pending<T> {
    this.#spoiled ??= error as Error;
    throw error;
  }

  /** @internal */
  watch<T>(): AsyncIterable<T> {
    throw new BadRequest("watch() cannot be part of a batch");
  }

  /**
   * Sends the batch and gives the `Pending` results. Asked again, while it is
   * being sent or afterwards, it gives the same results and sends nothing.
   *
   * A batch in which an operation could not be added, because of a mistake
   * in its arguments, is not sent at all: this throws `BadRequest`.
   */
  send(): Promise<readonly Pending<unknown>[]> {
    if (this.#sent) {
      return Promise.resolve(this.#pending);
    }
    if (this.#spoiled !== null) {
      return Promise.reject(
        new BadRequest(`an operation could not be added to this batch, so it is not sent: ${this.#spoiled.message}`),
      );
    }
    if (this.#sending === null) {
      this.#sending = this.#send().finally(() => {
        this.#sending = null;
      });
    }
    return this.#sending;
  }

  async #send(): Promise<readonly Pending<unknown>[]> {
    const requests = this.#pending.map((pending) => pending.op.request());
    // A batch that changes something is sent with an id, as one change is.
    const changes = this.#pending.some((pending) => pending.op.changes);
    const sentAs = changes ? { request_id: requestId() } : {};
    const reply = await this.#client.call("Batch", (session) => ({ session, requests, ...sentAs }), undefined, changes);
    this.#sent = true;
    const responses: Message[] = reply.responses ?? [];
    this.#pending.forEach((pending, at) => pending.arrived(responses[at] ?? {}));
    return this.#pending;
  }
}
