/**
 * Finding the sidecar on this machine, and starting one if none is running.
 *
 * `orvud` is the program that does the real work. One runs per user, in the
 * background, and every program of that user shares it, in whatever language
 * it is written. Nobody has to start it by hand: the first `connect()` does,
 * and the sidecar stops by itself a while after the last program has
 * disconnected.
 *
 * @module
 */

import { spawn } from "node:child_process";
import * as crypto from "node:crypto";
import * as fs from "node:fs";
import * as net from "node:net";
import { createRequire } from "node:module";
import * as path from "node:path";
import { SidecarUnavailable } from "./errors.ts";

/** How many seconds a sidecar started from here stays up after the last
 * program has disconnected, unless `ORVU_IDLE_SECONDS` says otherwise. */
export const IDLE_SECONDS = 300;

/** How large, in bytes, the sidecar's log may grow before the next sidecar
 * started moves it aside, to `orvud.log.1`. One older log is kept. */
export const LOG_LIMIT = 1024 * 1024;

/** How many seconds a sidecar started from here stays up after the last
 * program has disconnected: `ORVU_IDLE_SECONDS`, or `IDLE_SECONDS`. */
export function idleSeconds(env: NodeJS.ProcessEnv = process.env): number {
  const given = env.ORVU_IDLE_SECONDS;
  if (!given) {
    return IDLE_SECONDS;
  }
  if (/^[0-9]+$/.test(given) && Number(given) > 0 && Number.isSafeInteger(Number(given))) {
    return Number(given);
  }
  throw new SidecarUnavailable(
    `ORVU_IDLE_SECONDS has to be a whole number of seconds above 0 and at most ${Number.MAX_SAFE_INTEGER}, ` +
      `not ${JSON.stringify(given)}`,
  );
}

/** The value as a path, if it is there and is an absolute path. */
function absolute(value: string | undefined): string | null {
  return value && path.isAbsolute(value) ? value : null;
}

/** The folder a local sidecar keeps its data in, or `null` if the system
 * gives no home folder to put it in. `orvud --local` uses the same rule. */
export function defaultDataDir(env: NodeJS.ProcessEnv = process.env): string | null {
  const home = absolute(env.HOME);
  if (process.platform === "darwin") {
    return home ? path.join(home, "Library/Application Support/orvu") : null;
  }
  const shared = absolute(env.XDG_DATA_HOME);
  if (shared) {
    return path.join(shared, "orvu");
  }
  return home ? path.join(home, ".local/share/orvu") : null;
}

/** The socket a local sidecar serves on. `orvud --local` uses the same rule. */
export function defaultSocket(dataDir: string, env: NodeJS.ProcessEnv = process.env): string {
  return path.join(absolute(env.XDG_RUNTIME_DIR) ?? dataDir, "orvud.sock");
}

/**
 * The socket the sidecar on this data folder was last started on, or `null`.
 *
 * The socket's place depends on settings a terminal has and a program started
 * some other way may not, such as a server an agent's program starts. The
 * data folder is found the same way by both, so whoever starts the sidecar
 * notes its socket there for the others. A Python program does the same.
 */
function noted(dataDir: string): string | null {
  try {
    return absolute(fs.readFileSync(path.join(dataDir, "orvud.where"), "utf8").trim());
  } catch {
    return null;
  }
}

/** `true` if something answers on the socket. */
export function isListening(socket: string): Promise<boolean> {
  return new Promise((resolve) => {
    const probe = net.createConnection(socket);
    probe.once("connect", () => {
      probe.destroy();
      resolve(true);
    });
    probe.once("error", () => resolve(false));
  });
}

/** The socket a running sidecar answers on: `socket`, or the one noted in the
 * data folder. `null` if neither answers. */
async function found(dataDir: string, socket: string): Promise<string | null> {
  for (const place of [socket, noted(dataDir)]) {
    if (place !== null && (await isListening(place))) {
      return place;
    }
  }
  return null;
}

function runnable(file: string): boolean {
  try {
    fs.accessSync(file, fs.constants.X_OK);
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}

/** The kinds of machine there is a sidecar package for, by Node's names for
 * the system and the processor. */
const MACHINES: Record<string, readonly string[]> = {
  linux: ["x64", "arm64"],
  darwin: ["x64", "arm64"],
};

/** The npm package that holds the `orvud` program for this kind of machine,
 * such as `@orvuhq/orvud-linux-x64`. npm installs it beside this package, and
 * only on the kind of machine it is for. `null` where there is none, as on
 * Windows. */
export function sidecarPackage(platform: string = process.platform, arch: string = process.arch): string | null {
  return MACHINES[platform]?.includes(arch) ? `@orvuhq/orvud-${platform}-${arch}` : null;
}

/** The program inside the sidecar package for this kind of machine, if that
 * package is installed where this package can see it. */
function fromPackage(name: string, from: string | URL): string | null {
  let manifest: string;
  try {
    manifest = createRequire(from).resolve(`${name}/package.json`);
  } catch {
    return null;
  }
  const bin = path.join(path.dirname(manifest), "bin", "orvud");
  return runnable(bin) ? bin : null;
}

/** Where to look from, and for which kind of machine. Tests change these. */
export interface Looking {
  from?: string | URL;
  platform?: string;
  arch?: string;
}

/** The `orvud` program to start: the one named in `$ORVUD_BIN`, or the one in
 * the sidecar package for this kind of machine, or one on the `PATH`. */
export function program(env: NodeJS.ProcessEnv = process.env, looking: Looking = {}): string {
  const named = env.ORVUD_BIN;
  if (named) {
    if (!runnable(named)) {
      throw new SidecarUnavailable(`$ORVUD_BIN is ${named}, which is not a program that can be run`);
    }
    return named;
  }
  const name = sidecarPackage(looking.platform, looking.arch);
  const packaged = name === null ? null : fromPackage(name, looking.from ?? import.meta.url);
  if (packaged !== null) {
    return packaged;
  }
  for (const folder of (env.PATH ?? "").split(path.delimiter)) {
    const candidate = path.join(folder, "orvud");
    if (folder && runnable(candidate)) {
      return candidate;
    }
  }
  if (name === null) {
    throw new SidecarUnavailable(
      "no sidecar is running and there is no orvud program to start. There is none for this kind of " +
        "machine yet: connect to one on Linux or a Mac over the network, with the connection file " +
        "`orvud agent add --connection` makes there (pass it as `connection`, or set ORVU_CONNECT), " +
        "or set $ORVUD_BIN to an orvud you have",
    );
  }
  throw new SidecarUnavailable(
    `no sidecar is running and there is no orvud program to start. It comes in the package ${name}, ` +
      "which npm installs with this one unless optional packages are left out: install it, put orvud " +
      "on the PATH, or set $ORVUD_BIN to where it is",
  );
}

/** What a sidecar says when another one holds its data folder. That one is
 * starting or stopping, and is waited for. */
const FOLDER_IN_USE = "is already using the data folder";

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Makes sure this user's local sidecar is running, starting it if it is not,
 * and gives the socket it serves on.
 *
 * Any number of programs can call this at the same moment. Each may start a
 * sidecar, but a sidecar takes its data folder for itself, so only one of
 * them runs. The others stop at once, saying the folder is in use, and the
 * program that started them waits for the one that runs. While it waits it
 * starts another at most once a second, in case the one holding the folder
 * was a sidecar on its way out.
 */
export async function ensureRunning(timeoutMs = 10_000): Promise<string> {
  if (process.platform === "win32") {
    throw new SidecarUnavailable(
      "there is no sidecar for Windows yet. Connect to one on Linux or a Mac over the network, with the " +
        "connection file `orvud agent add --connection` makes there: pass it as `connection`, or set ORVU_CONNECT",
    );
  }
  const dataDir = defaultDataDir();
  if (dataDir === null) {
    throw new SidecarUnavailable("there is no home folder to keep data in: set $HOME");
  }
  // Read first, so that a wrong value is told before anything is started.
  const idle = idleSeconds();
  const socket = defaultSocket(dataDir);
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const running = await found(dataDir, socket);
    if (running !== null) {
      return running;
    }
    const said = await start(program(), dataDir, socket, deadline, timeoutMs, idle);
    if (said === null) {
      fs.writeFileSync(path.join(dataDir, "orvud.where"), socket);
      return socket;
    }
    // Another sidecar holds the folder: one is starting, or one that is
    // shutting down has given up its socket and is still saving. Wait for
    // it, looking for it often, and start another only now and then.
    if (said.includes(FOLDER_IN_USE) && Date.now() < deadline) {
      const tryAgain = Math.min(Date.now() + 1_000, deadline);
      while (Date.now() < tryAgain) {
        const appeared = await found(dataDir, socket);
        if (appeared !== null) {
          return appeared;
        }
        await sleep(50);
      }
      continue;
    }
    throw new SidecarUnavailable(`the sidecar could not start. It said: ${said}`);
  }
}

/** How old, in milliseconds, a lock on moving the log aside has to be before
 * it is taken for one left behind by a program that stopped halfway. Moving
 * the log takes a moment. */
const LOG_LOCK_STALE_MS = 30_000;

/** What a lock file that nobody holds says. */
const LOCK_FREE = "free";

/**
 * Moves the log to `orvud.log.1` if it has grown past `LOG_LIMIT`, putting
 * the one there before out of the way. A sidecar that is still stopping goes
 * on writing to the file it had, wherever it now is.
 *
 * Several programs may start a sidecar at once, in any language. Only the
 * one holding the lock on the log looks at it and moves it, so that two
 * never both find it long and the second moves the first one's new, short
 * log over the long one. A program that finds the lock held leaves the log
 * to whoever holds it.
 *
 * The lock counts as left behind once it is 30 seconds old, so a program
 * held up longer than that, stopped or asleep, may find another has taken it
 * meanwhile. It looks again just before moving the log, and leaves the log
 * if the lock is no longer its own.
 */
export function moveAside(log: string): void {
  const lock = log + ".lock";
  const mark = newMark();
  const number = takeLock(lock, mark);
  if (number === null) {
    return;
  }
  try {
    if (fs.statSync(log).size > LOG_LIMIT && stillHeld(lock, number, mark)) {
      fs.renameSync(log, log + ".1");
    }
  } catch {
    // No log yet, or it cannot be moved: it is added to, as before.
  } finally {
    giveBack(lock, number);
  }
}

// The lock is a row of numbered files, `orvud.log.lock.1`, `.2`, and so on,
// and only the highest-numbered one counts. It says `free`, or holds the mark
// of the program holding the lock. To take the lock, a program makes the file
// with the next number, which only one program can make; to give it back, it
// makes the one after that, saying `free`. Nothing ever removes, renames or
// writes over the highest-numbered file, so no program can take away a lock
// another has just taken. Files below the highest are left over, and are
// removed by whoever holds the lock next. The Python package does the same,
// with the same files.

/** The numbers of the lock files there are now, lowest first. */
function lockNumbers(lock: string): number[] {
  const prefix = path.basename(lock) + ".";
  const found: number[] = [];
  for (const name of fs.readdirSync(path.dirname(lock))) {
    const rest = name.startsWith(prefix) ? name.slice(prefix.length) : "";
    if (/^[0-9]+$/.test(rest) && String(Number(rest)) === rest && Number.isSafeInteger(Number(rest))) {
      found.push(Number(rest));
    }
  }
  return found.sort((a, b) => a - b);
}

const numbered = (lock: string, number: number) => `${lock}.${number}`;

/** `true` if the lock file numbered `number` says it is free, or has been
 * there so long that the program holding it stopped halfway. A file just
 * made, whose mark is not written yet, is held. */
function canBeTaken(lock: string, number: number): boolean {
  let made: number;
  try {
    made = fs.openSync(numbered(lock, number), "r");
  } catch {
    // It went away meanwhile: someone else is busy with the lock.
    return false;
  }
  try {
    const said = Buffer.alloc(64);
    const read = fs.readSync(made, said, 0, said.length, 0);
    const changed = fs.fstatSync(made).mtimeMs;
    return said.subarray(0, read).toString("utf8") === LOCK_FREE || Date.now() - changed >= LOG_LOCK_STALE_MS;
  } catch {
    return false;
  } finally {
    fs.closeSync(made);
  }
}

/** Makes the lock file numbered `number`, saying `text`. `false` if it is
 * there already, or cannot be made or written; a file this program made but
 * could not write is removed again. */
function makeLockFile(lock: string, number: number, text: string): boolean {
  const file = numbered(lock, number);
  let made: number;
  try {
    made = fs.openSync(file, "wx", 0o600);
  } catch {
    return false;
  }
  let written = false;
  try {
    const bytes = Buffer.from(text, "utf8");
    written = fs.writeSync(made, bytes) === bytes.length;
  } catch {
    written = false;
  } finally {
    fs.closeSync(made);
  }
  if (!written) {
    try {
      fs.unlinkSync(file);
    } catch {
      // Already gone.
    }
  }
  return written;
}

/** A mark no other program writes in a lock file. */
const newMark = () => `${process.pid}-${crypto.randomBytes(8).toString("hex")}`;

/** Takes the lock on the log, `lock` being `orvud.log.lock`, writing `mark`
 * in the file it makes. Gives back the number of the lock file this program
 * made, or `null` if another program holds the lock, or it cannot be taken
 * at all. */
export function takeLock(lock: string, mark: string = newMark()): number | null {
  let before: number[];
  try {
    before = lockNumbers(lock);
  } catch {
    return null;
  }
  const last = before.at(-1) ?? 0;
  if (last > 0 && !canBeTaken(lock, last)) {
    return null;
  }
  const mine = last + 1;
  if (!makeLockFile(lock, mine, mark)) {
    // Another program made it first, or it cannot be made.
    return null;
  }
  let after: number[] = [];
  try {
    after = lockNumbers(lock);
  } catch {
    // Not known to be this program's: it is not taken.
  }
  if (after.at(-1) !== mine) {
    // This program was so slow that the lock has moved on past it, and what
    // it made counts for nothing.
    return null;
  }
  for (const old of after.slice(0, -1)) {
    try {
      fs.unlinkSync(numbered(lock, old));
    } catch {
      // Already gone.
    }
  }
  return mine;
}

/** `true` if lock file `number` is still the highest, and still says
 * `mark`: no other program has taken the lock over meanwhile.
 *
 * This only makes the gap small. If this program is stopped for more than
 * 30 seconds after looking and before moving the log, another can take the
 * lock over in between, and the log is moved all the same. */
function stillHeld(lock: string, number: number, mark: string): boolean {
  try {
    if (lockNumbers(lock).at(-1) !== number) {
      return false;
    }
    return fs.readFileSync(numbered(lock, number), "utf8") === mark;
  } catch {
    return false;
  }
}

/** Gives back the lock this program took as lock file `number`. If this
 * program took so long that another took the lock over, the next number is
 * that program's already, and is left. */
export function giveBack(lock: string, number: number): void {
  if (makeLockFile(lock, number + 1, LOCK_FREE)) {
    try {
      fs.unlinkSync(numbered(lock, number));
    } catch {
      // Already gone.
    }
  }
}

/** Starts a sidecar and waits until it answers on its socket. Gives `null`
 * once it does, or what it said if it stopped at once. */
async function start(
  orvud: string,
  dataDir: string,
  socket: string,
  deadline: number,
  timeoutMs: number,
  idle: number,
): Promise<string | null> {
  // The folder is made the way the sidecar would make it: for this user only.
  fs.mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  const log = path.join(dataDir, "orvud.log");
  moveAside(log);
  // Kept open until the sidecar answers or has stopped, and what it said is
  // read through it: another program may move the log aside meanwhile, and
  // the file this one's sidecar writes to is then no longer `orvud.log`.
  const output = fs.openSync(log, "a+");
  try {
    const alreadyThere = fs.fstatSync(output).size;
    let ended = false;
    const child = spawn(
      orvud,
      ["--local", "--data-dir", dataDir, "--socket", socket, "--exit-when-idle", String(idle)],
      // A session of its own, so that Ctrl-C in the program that started it
      // does not stop the sidecar under everyone else.
      { detached: true, stdio: ["ignore", output, output] },
    );
    child.once("exit", () => {
      ended = true;
    });
    child.once("error", () => {
      ended = true;
    });
    // The sidecar is meant to outlive this program.
    child.unref();
    const said = () => {
      const size = Math.max(fs.fstatSync(output).size - alreadyThere, 0);
      const text = Buffer.alloc(size);
      const read = fs.readSync(output, text, 0, size, alreadyThere);
      return text.subarray(0, read).toString("utf8").trim();
    };
    for (;;) {
      if (await isListening(socket)) {
        return null;
      }
      if (ended) {
        return said() || "nothing";
      }
      if (Date.now() >= deadline) {
        throw new SidecarUnavailable(
          `the sidecar was started but did not answer on ${socket} within ${timeoutMs / 1000} seconds. ` +
            `It said: ${said()}`,
        );
      }
      await sleep(10);
    }
  } finally {
    fs.closeSync(output);
  }
}
