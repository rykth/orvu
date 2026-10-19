/**
 * What the tests share. Every test talks to a real sidecar. The only
 * stand-ins are for a sidecar in its last moment, which a real one is in for
 * too short a time to test.
 *
 * The sidecar program is built from this repository. Build it first:
 *
 *     cargo build -p orvud
 *
 * Set ORVUD_BIN to use a different one.
 */

import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { connect, Team, type Client, type ConnectOptions } from "../src/index.ts";

export const REPO = fileURLToPath(new URL("../../", import.meta.url));

// A key or a connection file in the environment of whoever runs the tests is
// not used: every test says which it gives.
delete process.env.ORVU_KEY;
delete process.env.ORVU_CONNECT;
delete process.env.ORVU_SOCKET;
delete process.env.ORVU_IDLE_SECONDS;
// The tests compare what other Node programs print. A terminal that asks for
// colour everywhere (FORCE_COLOR) would put colour codes into that output.
delete process.env.FORCE_COLOR;
process.env.NO_COLOR = "1";

/** A made-up model for search by meaning, which knows a few dozen words.
 * Every sidecar the tests start is given it, so that no test fetches the real
 * one from the internet. */
export const TINY_MODEL = path.join(REPO, "crates/orvu-search/tiny-model");
process.env.ORVU_MODEL = TINY_MODEL;

export function orvudBin(): string {
  const bin = process.env.ORVUD_BIN ?? path.join(REPO, "target/debug/orvud");
  if (!fs.existsSync(bin)) {
    throw new Error(`${bin} is missing: run \`cargo build -p orvud\` first`);
  }
  return bin;
}

export const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Waits until `check` is true, for at most `seconds`. Gives whether it was. */
export async function waitUntil(check: () => boolean | Promise<boolean>, seconds = 5): Promise<boolean> {
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    if (await check()) {
      return true;
    }
    await sleep(10);
  }
  return Boolean(await check());
}

/** A port nothing is using, asked of the system. */
export function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address() as net.AddressInfo;
      server.close(() => resolve(port));
    });
  });
}

/** Stops a program this test started, and waits until it has. A program
 * that has already ended is left as it is. */
export async function stopChild(child: ChildProcess): Promise<void> {
  if (child.exitCode === null && child.signalCode === null) {
    const ended = new Promise((resolve) => child.once("exit", resolve));
    child.kill("SIGTERM");
    await ended;
  }
}

/** A short folder of its own. A Unix socket path can hold at most 107
 * bytes, so the folder has to be near the top: in the folder ORVU_TEST_DIR
 * names, or in /tmp. */
export function shortFolder(): string {
  return fs.mkdtempSync(path.join(process.env.ORVU_TEST_DIR || "/tmp", "orvu-"));
}

/** The state of each thread of a process, such as "S" for sleeping or "T"
 * for stopped. Where the system does not show threads, as on a Mac, it is
 * the state of the process as a whole. Throws if the process has ended. */
function states(pid: number): string[] {
  if (!fs.existsSync("/proc/self")) {
    let state = "";
    try {
      state = execFileSync("ps", ["-o", "stat=", "-p", String(pid)], { encoding: "utf8" }).trim();
    } catch {
      // ps says nothing about a process that is gone.
    }
    if (state === "" || state.startsWith("Z")) {
      throw new Error(`process ${pid} has ended`);
    }
    return [state[0]];
  }
  const threads = `/proc/${pid}/task`;
  const found: string[] = [];
  for (const thread of fs.existsSync(threads) ? fs.readdirSync(threads) : []) {
    try {
      // The state follows the name, which is in brackets and may hold
      // anything, so it is looked for after the last bracket.
      const stat = fs.readFileSync(path.join(threads, thread, "stat"), "utf8");
      found.push(stat[stat.lastIndexOf(") ") + 2]);
    } catch {
      // The thread has ended.
    }
  }
  if (found.length === 0 || found.every((state) => state === "Z" || state === "X")) {
    throw new Error(`process ${pid} has ended`);
  }
  return found;
}

/** Holds a process still, and waits until it is. Told to stop, a process
 * can go on running for a moment, and so can each of its threads. A thread
 * held by a debugger counts as still, and so does one that has ended. */
export async function holdStill(child: ChildProcess, seconds = 30): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null || !child.kill("SIGSTOP")) {
    throw new Error(`process ${child.pid} has ended`);
  }
  const still = () => states(child.pid!).every((state) => "TtZX".includes(state));
  if (!(await waitUntil(still, seconds))) {
    throw new Error(`process ${child.pid} did not stop within ${seconds} seconds: ${states(child.pid!).join("")}`);
  }
}

/** One running `orvud --local`, with its own data folder and socket. */
export class Sidecar {
  readonly folder: string;
  readonly socket: string;
  readonly data: string;
  process: ChildProcess | null = null;
  #output: string;

  constructor(folder: string) {
    this.folder = folder;
    this.socket = path.join(folder, "s");
    this.data = path.join(folder, "data");
    this.#output = path.join(folder, "output");
  }

  async start(...extra: string[]): Promise<this> {
    // What the sidecar prints goes to a file, not a pipe. A pipe that nobody
    // reads fills up, and the sidecar would then stop to wait.
    const output = fs.openSync(this.#output, "w");
    this.process = spawn(orvudBin(), ["--local", "--data-dir", this.data, "--socket", this.socket, ...extra], {
      stdio: ["ignore", "ignore", output],
    });
    fs.closeSync(output);
    const child = this.process;
    const ready = await waitUntil(() => this.printed().includes("serving on") || child.exitCode !== null);
    if (!ready || child.exitCode !== null) {
      throw new Error(`orvud did not start: ${this.printed()}`);
    }
    return this;
  }

  /** Everything the sidecar has printed so far. */
  printed(): string {
    return fs.existsSync(this.#output) ? fs.readFileSync(this.#output, "utf8") : "";
  }

  async stop(): Promise<void> {
    const child = this.process;
    this.process = null;
    if (child) {
      await stopChild(child);
    }
  }

  async restart(): Promise<this> {
    await this.stop();
    return this.start();
  }

  /** Lets in the agent `name`, and gives its key. */
  addAgent(name: string): string {
    const said = execFileSync(orvudBin(), ["agent", "add", name, "--local", "--data-dir", this.data], { encoding: "utf8" });
    return said.split("\n").find((line) => line.startsWith("orvu-key-"))!;
  }

  removeAgent(name: string): void {
    execFileSync(orvudBin(), ["agent", "remove", name, "--local", "--data-dir", this.data]);
  }

  /** Lets in the agent `name`, and writes a connection file for it at
   * `connection`, for the sidecar at `address`. */
  addNetworkedAgent(name: string, address: string, connection: string): void {
    execFileSync(orvudBin(), ["agent", "add", name, "--address", address, "--connection", connection, "--local", "--data-dir", this.data]);
  }

  /** Gives the agent `name` rules, such as "notes:write", or takes them away
   * with "all". */
  allow(name: string, ...rules: string[]): void {
    execFileSync(orvudBin(), ["agent", "allow", name, ...rules, "--local", "--data-dir", this.data]);
  }
}

/** Runs `body` with a fresh sidecar that has nothing stored in it. */
export async function withSidecar(body: (sidecar: Sidecar) => Promise<void>, ...extra: string[]): Promise<void> {
  const folder = shortFolder();
  const sidecar = new Sidecar(folder);
  try {
    await sidecar.start(...extra);
    await body(sidecar);
  } finally {
    await sidecar.stop();
    fs.rmSync(folder, { recursive: true, force: true });
  }
}

/** Runs `body` with a client of a fresh sidecar. */
export async function withClient(body: (client: Client, sidecar: Sidecar) => Promise<void>, options: ConnectOptions = {}): Promise<void> {
  await withSidecar(async (sidecar) => {
    const client = await connect(sidecar.socket, options);
    try {
      await body(client, sidecar);
    } finally {
      client.close();
    }
  });
}

/** The process ids of the sidecars running for `folder`. */
export function sidecars(folder: string): number[] {
  try {
    const found = execFileSync("pgrep", ["-f", `orvud.*${folder}`], { timeout: 60_000, encoding: "utf8" });
    return found.split("\n").filter(Boolean).map(Number);
  } catch {
    return [];
  }
}

/**
 * Runs `body` with a home folder of its own. The sidecar's data and socket
 * go under it, and whatever was started for it is stopped afterwards. The
 * real home folder is never touched.
 */
export async function withHome(body: (home: string) => Promise<void>): Promise<void> {
  const home = shortFolder();
  fs.mkdirSync(path.join(home, "run"), { mode: 0o700 });
  const kept = { ...process.env };
  process.env.HOME = home;
  process.env.XDG_RUNTIME_DIR = path.join(home, "run");
  delete process.env.XDG_DATA_HOME;
  process.env.ORVUD_BIN = orvudBin();
  try {
    await body(home);
  } finally {
    for (const key of Object.keys(process.env)) {
      if (!(key in kept)) {
        delete process.env[key];
      }
    }
    Object.assign(process.env, kept);
    for (const pid of sidecars(home)) {
      process.kill(pid, "SIGKILL");
    }
    await waitUntil(() => sidecars(home).length === 0);
    fs.rmSync(home, { recursive: true, force: true });
  }
}

export const MIB = 1024 * 1024;
export const tmp = os.tmpdir();

/** Runs `body` with a team on a fresh sidecar, used by an agent called alice,
 * and the same team used by a second agent, bob. */
export async function withTeam(body: (team: Team, bob: Team, sidecar: Sidecar) => Promise<void>): Promise<void> {
  await withSidecar(async (sidecar) => {
    const team = await Team.connect("default", { socket: sidecar.socket, me: "alice" });
    const bob = await Team.connect("default", { socket: sidecar.socket, me: "bob" });
    try {
      await body(team, bob, sidecar);
    } finally {
      await bob.close().catch(() => {});
      await team.close().catch(() => {});
    }
  });
}

/** A team on `sidecar`, used by the agent `me`, or by a program that gave no
 * name. */
export function teamOn(sidecar: Sidecar, me?: string): Promise<Team> {
  return Team.connect("default", { socket: sidecar.socket, me });
}

/** Runs a short Python program against `socket`, with the Python package of
 * this repository, and gives what it printed. */
export function python(socket: string, program: string): string {
  return execFileSync(path.join(REPO, "python/.venv/bin/python"), ["-c", program, socket], { timeout: 60_000, encoding: "utf8" });
}
