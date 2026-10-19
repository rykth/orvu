// `connect()` with no socket: finding this user's sidecar, and starting it
// when it is not running.
//
// Each test has a home folder of its own, so the sidecars started here never
// touch the real one.

import * as grpc from "@grpc/grpc-js";
import * as loader from "@grpc/proto-loader";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import * as fs from "node:fs";
import { createRequire, syncBuiltinESMExports } from "node:module";
import * as net from "node:net";
import * as path from "node:path";
import { test } from "node:test";
import { BadRequest, connect, SidecarUnavailable, Team } from "../src/index.ts";
import type { Client } from "../src/index.ts";
import {
  defaultDataDir,
  defaultSocket,
  ensureRunning,
  giveBack,
  IDLE_SECONDS,
  idleSeconds,
  LOG_LIMIT,
  moveAside,
  takeLock,
} from "../src/sidecar.ts";
import { VERSION } from "../src/version.ts";
import { freePort, orvudBin, python, REPO, sidecars, sleep, stopChild, waitUntil, withHome, withSidecar } from "./support.ts";

const dataDir = () => defaultDataDir()!;

const DataPlane = (
  grpc.loadPackageDefinition(
    loader.loadSync(path.join(REPO, "typescript/proto/dataplane.proto"), { keepCase: true, enums: String, oneofs: true }),
  ) as any
).orvu.dataplane.v1.DataPlane as grpc.ServiceClientConstructor;

test("connect starts a sidecar when none is running", () =>
  withHome(async (home) => {
    assert.deepEqual(sidecars(home), []);
    const client = await connect();
    await client.counter("visits").increment(5);
    assert.equal(await client.counter("visits").value(), 5);
    client.close();
    assert.equal(sidecars(home).length, 1);
  }));

test("connect uses the sidecar that is already running", () =>
  withHome(async (home) => {
    const first = await connect();
    const second = await connect();
    await first.counter("visits").increment(2);
    assert.equal(await second.counter("visits").value(), 2);
    assert.equal(first.replicaId, second.replicaId);
    first.close();
    second.close();
    assert.equal(sidecars(home).length, 1);
  }));

test("what was written is still there after the sidecar stops", () =>
  withHome(async (home) => {
    const client = await connect();
    await client.counter("visits").increment(5);
    client.close();
    const [pid] = sidecars(home);
    process.kill(pid, "SIGTERM");
    assert.ok(await waitUntil(() => sidecars(home).length === 0));
    const again = await connect();
    assert.equal(await again.counter("visits").value(), 5);
    again.close();
  }));

// A sidecar that was killed outright leaves its socket file behind. The next
// connect must not be fooled by it.
test("connect starts a new sidecar after one was killed", () =>
  withHome(async (home) => {
    const client = await connect();
    await client.counter("visits").increment(5);
    client.close();
    // A sidecar saves what it was sent within a tenth of a second.
    await sleep(300);
    const [pid] = sidecars(home);
    process.kill(pid, "SIGKILL");
    assert.ok(await waitUntil(() => sidecars(home).length === 0));
    assert.ok(fs.existsSync(defaultSocket(dataDir())));
    const again = await connect();
    assert.equal(await again.counter("visits").value(), 5);
    again.close();
  }));

// The sidecar refuses a data folder that other users can get into. The
// person who called `connect` has to be told why, in the sidecar's own
// words, and without waiting for the whole timeout.
test("a sidecar that cannot start says why", () =>
  withHome(async (home) => {
    fs.mkdirSync(dataDir(), { recursive: true });
    fs.chmodSync(dataDir(), 0o755);
    const asked = Date.now();
    await assert.rejects(
      connect(undefined, { timeoutMs: 30_000 }),
      (error: Error) => error instanceof SidecarUnavailable && /chmod 700/.test(error.message) && /could not start/.test(error.message),
    );
    assert.ok(Date.now() - asked < 5_000, "it waited instead of saying why at once");
    assert.deepEqual(sidecars(home), []);
  }));

// A data folder that seems to have been copied from another computer is
// refused. Started with no flags, the sidecar cannot be told what happened,
// so the error has to pass on the command that sorts it out, folder and all.
test("a copied data folder is explained with the command to run", () =>
  withHome(async (home) => {
    const client = await connect();
    client.close();
    const [pid] = sidecars(home);
    process.kill(pid, "SIGTERM");
    assert.ok(await waitUntil(() => sidecars(home).length === 0));
    fs.writeFileSync(path.join(dataDir(), "computer"), "somewhere else\n");
    const folder = fs.realpathSync(dataDir());
    await assert.rejects(connect(undefined, { timeoutMs: 30_000 }), (error: Error) => {
      assert.ok(error instanceof SidecarUnavailable, String(error));
      assert.match(error.message, /copied from another computer/);
      for (const flag of ["--moved", "--new-number"]) {
        assert.ok(error.message.includes(`orvud --local --data-dir ${folder} ${flag}`), error.message);
      }
      return true;
    });
    assert.deepEqual(sidecars(home), []);
  }));

test("a home folder too deep for a socket is explained", () =>
  withHome(async (home) => {
    const deep = path.join(home, "a".repeat(90));
    fs.mkdirSync(deep);
    process.env.HOME = deep;
    delete process.env.XDG_RUNTIME_DIR;
    await assert.rejects(connect(undefined, { timeoutMs: 30_000 }), (error: Error) => /socket path/.test(error.message) && /could not start/.test(error.message));
  }));

test("the sidecar has a session of its own, so Ctrl-C in the program does not stop it", () =>
  withHome(async (home) => {
    const client = await connect();
    const [pid] = sidecars(home);
    const session = (of: number) => execFileSync("ps", ["-o", "sid=", "-p", String(of)], { timeout: 60_000, encoding: "utf8" }).trim();
    assert.equal(session(pid), String(pid));
    assert.notEqual(session(pid), session(process.pid));
    client.close();
  }));

test("what the sidecar prints is kept in its data folder", () =>
  withHome(async () => {
    (await connect()).close();
    assert.match(fs.readFileSync(path.join(dataDir(), "orvud.log"), "utf8"), /serving on/);
  }));

const PROGRAM = `
import * as fs from "node:fs";
import { connect } from ${JSON.stringify(path.join(REPO, "typescript/src/index.ts"))};
const [here, number] = process.argv.slice(2);
fs.writeFileSync(here + "/ready-" + number, "");
while (!fs.existsSync(here + "/go")) await new Promise((r) => setTimeout(r, 1));
const client = await connect();
await client.counter("visits").increment(1);
client.close();
`;

// Twenty separate programs call `connect()` at the same moment, with no
// sidecar running. They must end up sharing one, and none of them may fail.
test("twenty programs starting at once share one sidecar", () =>
  withHome(async (home) => {
    const script = path.join(home, "program.mts");
    fs.writeFileSync(script, PROGRAM);
    const programs = Array.from({ length: 20 }, (_, number) => {
      const child = spawn(process.execPath, [script, home, String(number)], { stdio: ["ignore", "ignore", "pipe"] });
      let said = "";
      child.stderr!.on("data", (chunk) => (said += chunk));
      return new Promise<[number | null, string]>((resolve) => child.once("exit", (code) => resolve([code, said])));
    });
    assert.ok(await waitUntil(() => fs.readdirSync(home).filter((name) => name.startsWith("ready-")).length === 20, 30));
    fs.writeFileSync(path.join(home, "go"), "");
    const ended = await Promise.all(programs);
    assert.deepEqual(ended.map(([code]) => code), Array(20).fill(0), ended.map(([, said]) => said).join("\n"));
    assert.equal(sidecars(home).length, 1);
    const client = await connect();
    assert.equal(await client.counter("visits").value(), 20);
    client.close();
  }));

// A sidecar that is shutting down has given up its socket but holds its data
// folder for a moment. A program that connects just then has to wait for it,
// not report that the new sidecar could not start. Here a small Python
// program holds the folder the way the old sidecar would.
test("connect waits for a sidecar that is still shutting down", () =>
  withHome(async (home) => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.lock");
    const holder = spawn("python3", [
      "-c",
      "import fcntl, sys, time; f = open(sys.argv[1], 'w'); fcntl.flock(f, fcntl.LOCK_EX); print('held', flush=True); time.sleep(1.0)",
      lock,
    ]);
    await new Promise((resolve) => holder.stdout.once("data", resolve));
    const client = await connect();
    await client.counter("visits").increment(1);
    client.close();
    assert.equal(sidecars(home).length, 1);
  }));

test("connect says so when there is no home folder", () =>
  withHome(async () => {
    delete process.env.HOME;
    await assert.rejects(connect(), (error: Error) => error instanceof SidecarUnavailable && /HOME/.test(error.message));
  }));

test("having no program to start is explained", () =>
  withHome(async (home) => {
    delete process.env.ORVUD_BIN;
    process.env.PATH = home;
    await assert.rejects(connect(), (error: Error) => error instanceof SidecarUnavailable && /no orvud program/.test(error.message));
    assert.deepEqual(sidecars(home), []);
  }));

// A sidecar stops when it has been idle, and a long-running program may well
// be quiet for longer than that. Its next call has to start the sidecar
// again and go through, once.
test("a client starts the sidecar again when it has stopped", () =>
  withHome(async (home) => {
    const client = await connect();
    const visits = client.counter("visits");
    await visits.increment(5);
    const [pid] = sidecars(home);
    process.kill(pid, "SIGTERM");
    assert.ok(await waitUntil(() => sidecars(home).length === 0));
    await visits.increment(1);
    assert.equal(await visits.value(), 6);
    assert.equal(sidecars(home).length, 1);
    client.close();
  }));

test("a client starts the sidecar again when it was killed", () =>
  withHome(async (home) => {
    const client = await connect();
    const visits = client.counter("visits");
    await visits.increment(5);
    await sleep(300);
    const [pid] = sidecars(home);
    process.kill(pid, "SIGKILL");
    assert.ok(await waitUntil(() => sidecars(home).length === 0));
    // A call that was on its way when the sidecar died may have arrived, so
    // it is not sent again. This program was quiet for a moment first.
    await sleep(100);
    assert.equal(await visits.value(), 5);
    client.close();
  }));

// A client that was given a socket is using a sidecar somebody else runs. It
// never starts one.
test("a client given a socket does not start a sidecar", () =>
  withHome(async (home) =>
    withSidecar(async (sidecar) => {
      const client = await connect(sidecar.socket);
      await client.counter("visits").increment(1);
      await sidecar.stop();
      await assert.rejects(client.counter("visits").value(), SidecarUnavailable);
      client.close();
      assert.deepEqual(sidecars(home), []);
    }),
  ));

// Something still holds the socket open but no longer answers on it, as a
// sidecar does in its last moment. Connect must not spend all its time
// waiting on that socket.
test("connect gets past a socket that stopped answering", () =>
  withHome(async (home) => {
    const socket = defaultSocket(dataDir());
    const held = new Set<net.Socket>();
    const dying = net.createServer((connection) => held.add(connection));
    await new Promise<void>((resolve) => dying.listen(socket, resolve));
    // When it ends, everything it held open is closed with it.
    setTimeout(() => {
      dying.close();
      held.forEach((connection) => connection.destroy());
    }, 1000);
    const client = await connect(undefined, { timeoutMs: 10_000 });
    await client.counter("visits").increment(1);
    client.close();
    assert.equal(sidecars(home).length, 1);
  }));

// A Python program and a TypeScript program of the same user share one
// sidecar, whichever starts it.
test("a sidecar started from Node is used from Python", () =>
  withHome(async (home) => {
    const python = path.join(REPO, "python/.venv/bin/python");
    const client = await connect();
    await client.counter("visits").increment(2);
    const said = execFileSync(python, ["-c", "import orvu\nwith orvu.connect() as c: print(c.counter('visits').value())"], {
      timeout: 60_000,
      encoding: "utf8",
      env: process.env,
    }).trim();
    assert.equal(said, "2");
    assert.equal(sidecars(home).length, 1);
    client.close();
  }));

test("a sidecar started from Python is used from Node", () =>
  withHome(async (home) => {
    const python = path.join(REPO, "python/.venv/bin/python");
    execFileSync(python, ["-c", "import orvu\nwith orvu.connect() as c: c.counter('visits').increment(3)"], { timeout: 60_000, env: process.env });
    assert.equal(sidecars(home).length, 1);
    const client = await connect();
    assert.equal(await client.counter("visits").value(), 3);
    client.close();
    assert.equal(sidecars(home).length, 1);
  }));

// A change that was on its way when the sidecar died is sent again, with the
// id it was first sent with, once the sidecar is started again, and is made
// once. A read on its way is not sent again: it throws, and the next call
// starts the sidecar again.
test("a change cut off by a sidecar that died is sent again and made once", () =>
  withHome(async (home) => {
    const client = await connect();
    await client.counter("visits").increment(5);
    await sleep(300);
    const [pid] = sidecars(home);
    process.kill(pid, "SIGSTOP");
    const cut = client.counter("visits").increment(1);
    await sleep(200);
    process.kill(pid, "SIGKILL");
    await cut;
    assert.equal(await client.counter("visits").value(), 6);
    const [again] = sidecars(home);
    process.kill(again, "SIGSTOP");
    const read = client.counter("visits").value();
    await sleep(200);
    process.kill(again, "SIGKILL");
    await assert.rejects(read, SidecarUnavailable);
    assert.deepEqual(sidecars(home), [], "a sidecar was started for a read that may have arrived");
    assert.equal(await client.counter("visits").value(), 6);
    client.close();
  }));

// Many calls at once find the sidecar gone. One sidecar is started for all
// of them, and every one of them goes through.
test("calls at the same moment after the sidecar stopped start one sidecar", () =>
  withHome(async (home) => {
    const client = await connect();
    await client.counter("visits").increment(1);
    const [pid] = sidecars(home);
    process.kill(pid, "SIGTERM");
    assert.ok(await waitUntil(() => sidecars(home).length === 0));
    await sleep(100);
    await Promise.all(Array.from({ length: 20 }, () => client.counter("visits").increment(1)));
    assert.equal(await client.counter("visits").value(), 21);
    assert.equal(sidecars(home).length, 1);
    // One of the calls started it, and the others waited for that one. None
    // started a sidecar of its own that had to give way.
    const refused = fs.readFileSync(path.join(dataDir(), "orvud.log"), "utf8").split("is already using the data folder").length - 1;
    assert.equal(refused, 0, `${refused} more sidecars were started`);
    client.close();
  }));

// A sidecar that is stopping turns away calls that reach it in its last
// moments, before it has stopped taking them, with "the sidecar is stopping".
// Such a call never ran, so a client of the sidecar this package starts waits
// for that one to go, starts it again, and makes the call there. Calls keep
// being made while the sidecar is stopped a few times; none of them fails,
// and every one of them is counted once.
//
// A call only meets that refusal if it arrives in the moment between the
// sidecar being told to stop and it no longer taking calls, and with a real
// sidecar that cannot be arranged. So this test meets it only now and then.
// The next test is the one that always does.
test("calls carry on while the sidecar is stopped and started again", { timeout: 120_000 }, () =>
  withHome(async (home) => {
    // Several programs, each with a connection of its own.
    const clients = await Promise.all(Array.from({ length: 4 }, () => connect(undefined, { timeoutMs: 30_000 })));
    let stop = false;
    let made = 0;
    const failed: unknown[] = [];
    const keepCalling = async (client: Client) => {
      try {
        while (!stop) {
          await client.counter("visits").increment(1);
          made += 1;
        }
      } catch (error) {
        failed.push(error);
      }
    };
    const callers = clients.flatMap((client) => [keepCalling(client), keepCalling(client)]);
    try {
      for (let turn = 0; turn < 6 && failed.length === 0; turn++) {
        assert.ok(await waitUntil(() => sidecars(home).length === 1 || failed.length > 0, 20), `${sidecars(home)}`);
        // Long enough for every caller to be calling again.
        await sleep(300);
        if (failed.length > 0) {
          break;
        }
        const [pid] = sidecars(home);
        process.kill(pid, "SIGTERM");
        assert.ok(await waitUntil(() => !sidecars(home).includes(pid), 20));
      }
    } finally {
      stop = true;
      await Promise.all(callers);
    }
    assert.deepEqual(failed, []);
    assert.equal(await clients[0].counter("visits").value(), made);
    clients.forEach((client) => client.close());
  }));

// The same, made certain: a stand-in for a sidecar answers the session, then
// turns the call away as one that is stopping does, and goes on holding its
// socket for a moment without answering on it.
test("a call turned away by a sidecar that is stopping is made on the next one", () =>
  withHome(async (home) => {
    const socket = defaultSocket(dataDir());
    const held = new Set<net.Socket>();
    const dying = net.createServer((connection) => held.add(connection));
    const stopping = new grpc.Server();
    let refused = 0;
    const turnAway = (_call: unknown, answer: grpc.sendUnaryData<unknown>) => {
      refused += 1;
      answer({ code: grpc.status.UNAVAILABLE, details: "the sidecar is stopping" });
      // From now on nothing answers on the socket, and a moment later it is
      // gone.
      fs.renameSync(`${socket}.dying`, socket);
      setTimeout(() => {
        stopping.forceShutdown();
        dying.close();
        held.forEach((connection) => connection.destroy());
        fs.rmSync(socket, { force: true });
      }, 1000);
    };
    stopping.addService(DataPlane.service, {
      Open: (_call: unknown, answer: grpc.sendUnaryData<unknown>) =>
        answer(null, { session: "s", resume_token: "r", replica_id: "1", version: VERSION, agent: "" }),
      Execute: turnAway,
      Batch: turnAway,
    });
    await new Promise<void>((resolve, reject) =>
      stopping.bindAsync(`unix:${socket}`, grpc.ServerCredentials.createInsecure(), (error) => (error ? reject(error) : resolve())),
    );
    await new Promise<void>((resolve) => dying.listen(`${socket}.dying`, resolve));
    const client = await connect(undefined, { timeoutMs: 20_000 });
    await client.counter("visits").increment(1);
    assert.equal(refused, 1);
    assert.equal(await client.counter("visits").value(), 1);
    assert.equal(sidecars(home).length, 1);
    client.close();
  }));

// A call can meet a sidecar that is stopping again when it is made the second
// time: the sidecar started for it may be told to stop too before the call
// gets there. It is made again then too, for as long as the client waits.
// Here the stand-in turns the call away twice, and only then goes.
test("a call turned away again by a sidecar that is stopping is made once more", () =>
  withHome(async (home) => {
    const socket = defaultSocket(dataDir());
    const held = new Set<net.Socket>();
    const dying = net.createServer((connection) => held.add(connection));
    const stopping = new grpc.Server();
    let refused = 0;
    const turnAway = (_call: unknown, answer: grpc.sendUnaryData<unknown>) => {
      refused += 1;
      answer({ code: grpc.status.UNAVAILABLE, details: "the sidecar is stopping" });
      if (refused === 2) {
        fs.renameSync(`${socket}.dying`, socket);
        setTimeout(() => {
          stopping.forceShutdown();
          dying.close();
          held.forEach((connection) => connection.destroy());
          fs.rmSync(socket, { force: true });
        }, 1000);
      }
    };
    stopping.addService(DataPlane.service, {
      Open: (_call: unknown, answer: grpc.sendUnaryData<unknown>) =>
        answer(null, { session: "s", resume_token: "r", replica_id: "1", version: VERSION, agent: "" }),
      Execute: turnAway,
      Batch: turnAway,
    });
    await new Promise<void>((resolve, reject) =>
      stopping.bindAsync(`unix:${socket}`, grpc.ServerCredentials.createInsecure(), (error) => (error ? reject(error) : resolve())),
    );
    await new Promise<void>((resolve) => dying.listen(`${socket}.dying`, resolve));
    const client = await connect(undefined, { timeoutMs: 20_000 });
    await client.counter("visits").increment(1);
    assert.equal(refused, 2);
    assert.equal(await client.counter("visits").value(), 1);
    assert.equal(sidecars(home).length, 1);
    client.close();
  }));

test("a closed client starts nothing", () =>
  withHome(async (home) => {
    const client = await connect();
    await client.counter("visits").increment(1);
    const [pid] = sidecars(home);
    process.kill(pid, "SIGTERM");
    assert.ok(await waitUntil(() => sidecars(home).length === 0));
    client.close();
    await assert.rejects(client.counter("visits").value(), BadRequest);
    assert.deepEqual(sidecars(home), []);
  }));

// While another sidecar holds the data folder, a program waits for it. It
// does not start one sidecar after another while it waits.
test("waiting for a sidecar that holds the folder starts few others", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const holder = spawn("python3", [
      "-c",
      "import fcntl, sys, time; f = open(sys.argv[1], 'w'); fcntl.flock(f, fcntl.LOCK_EX); print('held', flush=True); time.sleep(1.5)",
      path.join(dataDir(), "orvud.lock"),
    ]);
    await new Promise((resolve) => holder.stdout.once("data", resolve));
    const client = await connect();
    client.close();
    const refused = fs.readFileSync(path.join(dataDir(), "orvud.log"), "utf8").split("is already using the data folder").length - 1;
    assert.ok(refused <= 3, `${refused} sidecars were started while it waited`);
  }));

// A sidecar connected to other machines, started by hand, can serve on the
// same socket a local one would. `connect()` promises a store of this
// machine's own, so it must not quietly use that one and send the program's
// writes to the others. Given the socket, it uses it.
test("connect does not use a sidecar that is not a local one", () =>
  withHome(async (home) => {
    execFileSync(orvudBin(), ["setup", path.join(home, "crew"), `one=127.0.0.1:${await freePort()}`, `two=127.0.0.1:${await freePort()}`], {
      timeout: 60_000,
    });
    const socket = defaultSocket(dataDir());
    const output = fs.openSync(path.join(home, "networked"), "w");
    const networked = spawn(orvudBin(), ["--config", path.join(home, "crew/one/orvu.toml"), "--socket", socket], {
      stdio: ["ignore", "ignore", output],
    });
    fs.closeSync(output);
    try {
      assert.ok(await waitUntil(() => fs.readFileSync(path.join(home, "networked"), "utf8").includes("serving on")));
      await assert.rejects(
        connect(),
        (error: Error) =>
          error instanceof SidecarUnavailable &&
          /not a local one/.test(error.message) &&
          error.message.includes(socket) &&
          error.message.includes("ORVU_SOCKET"),
      );
      const client = await connect(socket);
      await client.counter("visits").increment(1);
      client.close();
    } finally {
      await stopChild(networked);
    }
  }));

// How long a sidecar started from here stays up after the last program has
// gone can be set in the environment.
test("the idle time can be set in the environment", () =>
  withHome(async (home) => {
    process.env.ORVU_IDLE_SECONDS = "1";
    const client = await connect();
    await client.counter("visits").increment(1);
    const [pid] = sidecars(home);
    const command = execFileSync("ps", ["-ww", "-o", "args=", "-p", String(pid)], { encoding: "utf8" });
    assert.ok(command.includes("--exit-when-idle 1"), command);
    client.close();
    assert.ok(await waitUntil(() => sidecars(home).length === 0, 15));
  }));

test("the idle time is read from the environment", () => {
  assert.equal(IDLE_SECONDS, 300);
  assert.equal(idleSeconds({}), 300);
  assert.equal(idleSeconds({ ORVU_IDLE_SECONDS: "" }), 300);
  assert.equal(idleSeconds({ ORVU_IDLE_SECONDS: "3600" }), 3600);
  for (const wrong of ["0", "-5", "soon", "1.5", "1e3"]) {
    assert.throws(
      () => idleSeconds({ ORVU_IDLE_SECONDS: wrong }),
      (error: Error) => error instanceof SidecarUnavailable && error.message.includes("ORVU_IDLE_SECONDS"),
    );
  }
  assert.equal(idleSeconds({ ORVU_IDLE_SECONDS: "9007199254740991" }), Number.MAX_SAFE_INTEGER);
  for (const tooBig of ["9007199254740992", "18446744073709551616"]) {
    assert.throws(
      () => idleSeconds({ ORVU_IDLE_SECONDS: tooBig }),
      (error: Error) => error instanceof SidecarUnavailable && error.message.includes("at most 9007199254740991"),
    );
  }
});

test("a wrong idle time is explained before anything starts", () =>
  withHome(async (home) => {
    process.env.ORVU_IDLE_SECONDS = "soon";
    await assert.rejects(connect(), (error: Error) => error instanceof SidecarUnavailable && /whole number of seconds/.test(error.message));
    assert.deepEqual(sidecars(home), []);
  }));

// The log the sidecar writes is moved aside once it has grown past a
// mebibyte, when the next sidecar starts, so it does not grow for ever. One
// older log is kept.
test("a long log is moved aside when the sidecar starts", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const log = path.join(dataDir(), "orvud.log");
    fs.writeFileSync(log + ".1", "the oldest\n");
    fs.writeFileSync(log, "x".repeat(LOG_LIMIT + 1));
    (await connect()).close();
    assert.equal(fs.readFileSync(log + ".1", "utf8"), "x".repeat(LOG_LIMIT + 1));
    const now = fs.readFileSync(log, "utf8");
    assert.match(now, /serving on/);
    assert.ok(now.length < 10_000);
  }));

const lockFiles = (folder: string) => fs.readdirSync(folder).filter((name) => name.startsWith("orvud.log.lock")).sort();

const leaveStale = (file: string, text: string) => {
  fs.writeFileSync(file, text);
  const longAgo = new Date(Date.now() - 3600_000);
  fs.utimesSync(file, longAgo, longAgo);
};

// Another program is moving the log aside at this moment: it holds the
// lock. This one leaves the log to it, so that the two do not each move a
// log aside and the second throw the first one's away.
test("a log another program is moving aside is left to it", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const log = path.join(dataDir(), "orvud.log");
    fs.writeFileSync(log, "x".repeat(LOG_LIMIT + 1));
    fs.writeFileSync(log + ".lock.7", "");
    (await connect()).close();
    assert.equal(fs.existsSync(log + ".1"), false, "the log was moved aside while another program held the lock");
    assert.ok(fs.readFileSync(log, "utf8").startsWith("x".repeat(LOG_LIMIT + 1)));
    assert.deepEqual(lockFiles(dataDir()), ["orvud.log.lock.7"], "another program's lock was taken away");
  }));

// A lock left behind by a program that stopped halfway does not keep the log
// from ever being moved aside again.
test("a lock left behind long ago does not stop the log being moved aside", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const log = path.join(dataDir(), "orvud.log");
    fs.writeFileSync(log, "x".repeat(LOG_LIMIT + 1));
    leaveStale(log + ".lock.7", "");
    (await connect()).close();
    assert.equal(fs.readFileSync(log + ".1", "utf8"), "x".repeat(LOG_LIMIT + 1));
    assert.deepEqual(lockFiles(dataDir()), ["orvud.log.lock.9"]);
    assert.equal(fs.readFileSync(log + ".lock.9", "utf8"), "free");
  }));

// The lock is what keeps two programs from both finding the log long and the
// second moving the first one's new, short log over the long one. While
// another program holds it, the log is left as it is.
test("a log whose lock another program holds is not moved aside", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const log = path.join(dataDir(), "orvud.log");
    const long = "x".repeat(LOG_LIMIT + 1);
    fs.writeFileSync(log, long);
    fs.writeFileSync(log + ".lock.3", "another program");
    moveAside(log);
    assert.equal(fs.existsSync(log + ".1"), false, "the log was moved aside while another program held the lock");
    assert.equal(fs.readFileSync(log, "utf8"), long);
    assert.deepEqual(lockFiles(dataDir()), ["orvud.log.lock.3"]);
    assert.equal(fs.readFileSync(log + ".lock.3", "utf8"), "another program");
  }));

// With the lock free, the long log is moved aside, and the lock given back.
// A second look finds the new log short and leaves it.
test("a long log is moved aside once and the lock given back", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const log = path.join(dataDir(), "orvud.log");
    const long = "x".repeat(LOG_LIMIT + 1);
    fs.writeFileSync(log, long);
    moveAside(log);
    assert.equal(fs.readFileSync(log + ".1", "utf8"), long);
    assert.equal(fs.readFileSync(log + ".lock.2", "utf8"), "free");
    fs.writeFileSync(log, "short\n");
    moveAside(log);
    assert.equal(fs.readFileSync(log + ".1", "utf8"), long);
    assert.equal(fs.readFileSync(log, "utf8"), "short\n");
    assert.deepEqual(fs.readdirSync(dataDir()).sort(), ["orvud.log", "orvud.log.1", "orvud.log.lock.4"]);
    assert.equal(fs.readFileSync(log + ".lock.4", "utf8"), "free");
  }));

// A program held up for longer than a lock is trusted, after it looked at
// the log's size, may find another program has taken the lock over as left
// behind. It looks again before moving the log, and leaves it to the other.
for (const takenOver of ["a higher lock file", "its own file written over"]) {
  test(`a program whose lock was taken over meanwhile does not move the log (${takenOver})`, () =>
    withHome(async () => {
      fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
      const log = path.join(dataDir(), "orvud.log");
      const long = "x".repeat(LOG_LIMIT + 1);
      fs.writeFileSync(log, long);
      const real = nodeFs.statSync as (...args: unknown[]) => unknown;
      nodeFs.statSync = (...args: unknown[]) => {
        const found = real(...args);
        if (args[0] === log) {
          const taken = takenOver === "a higher lock file" ? ".lock.2" : ".lock.1";
          fs.writeFileSync(log + taken, "another program");
        }
        return found;
      };
      syncBuiltinESMExports();
      try {
        moveAside(log);
      } finally {
        nodeFs.statSync = real;
        syncBuiltinESMExports();
      }
      assert.equal(fs.existsSync(log + ".1"), false, "the log was moved by a program that no longer held the lock");
      assert.equal(fs.readFileSync(log, "utf8"), long);
    }));
}

// A lock left behind is taken over by one program, which makes the next lock
// file with its own mark in it. The next program finds that one held.
test("a lock left behind is taken over by one program only", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    leaveStale(lock + ".4", "a program that stopped");
    assert.equal(takeLock(lock), 5);
    assert.ok(fs.readFileSync(lock + ".5", "utf8").startsWith(`${process.pid}-`));
    assert.equal(takeLock(lock), null);
    assert.deepEqual(lockFiles(dataDir()), ["orvud.log.lock.5"]);
  }));

// Only the highest-numbered lock file counts: a free one below a held one
// does not let a program in, and a held one below a free one does not keep
// it out.
test("only the highest-numbered lock file counts", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    fs.writeFileSync(lock + ".8", "free");
    fs.writeFileSync(lock + ".10", "another program");
    assert.equal(takeLock(lock), null);
    fs.writeFileSync(lock + ".11", "free");
    assert.equal(takeLock(lock), 12);
  }));

// A program so slow that the lock moved on past it while it was busy finds,
// once it has made its file, a higher one there, and does not hold the lock.
test("a program the lock has moved on past does not hold it", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    fs.writeFileSync(lock + ".3", "free");
    let busy = false;
    const got = watchingFiles(
      () => {
        if (!busy && fs.existsSync(lock + ".4") && !fs.existsSync(lock + ".9")) {
          busy = true;
          fs.writeFileSync(lock + ".9", "another program");
        }
      },
      () => takeLock(lock),
    );
    assert.equal(got, null);
    assert.equal(fs.readFileSync(lock + ".9", "utf8"), "another program");
  }));

// A program that was slow enough to have its lock taken over gives back only
// its own: the next lock file is another program's.
test("a lock is given back only by the program that holds it", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    const mine = takeLock(lock);
    assert.equal(mine, 1);
    fs.writeFileSync(lock + ".2", "another program");
    giveBack(lock, mine);
    assert.equal(fs.readFileSync(lock + ".2", "utf8"), "another program");
    assert.equal(takeLock(lock), null);
    fs.unlinkSync(lock + ".2");
    giveBack(lock, mine);
    assert.deepEqual(lockFiles(dataDir()), ["orvud.log.lock.2"]);
    assert.equal(fs.readFileSync(lock + ".2", "utf8"), "free");
  }));

// Programs that stopped halfway, again and again, and programs that took the
// lock and gave it back: the lock files they leave do not pile up.
test("lock files left over do not pile up", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    for (let i = 0; i < 20; i++) {
      assert.notEqual(takeLock(lock), null);
      ageEverything(dataDir());
    }
    fs.writeFileSync(lock + ".5", "a slow program");
    for (let i = 0; i < 20; i++) {
      giveBack(lock, takeLock(lock)!);
    }
    assert.deepEqual(lockFiles(dataDir()), ["orvud.log.lock.60"]);
  }));

// The lock is the same in Python and here: one taken in either language
// keeps the other out until it is given back.
test("a lock taken in Python keeps this language out, and the other way round", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    const inPython = (body: string) =>
      python(lock, `import pathlib, sys\nfrom orvu import _sidecar\nlock = pathlib.Path(sys.argv[1])\n${body}`).trim();
    assert.equal(inPython("print(_sidecar._take_lock(lock))"), "1");
    assert.equal(takeLock(lock), null);
    assert.equal(inPython("_sidecar._give_back(lock, 1)"), "");
    const mine = takeLock(lock);
    assert.equal(mine, 3);
    assert.equal(inPython("print(_sidecar._take_lock(lock))"), "None");
    giveBack(lock, mine!);
    assert.equal(inPython("print(_sidecar._take_lock(lock))"), "5");
    assert.deepEqual(lockFiles(dataDir()), ["orvud.log.lock.5"]);
  }));

const ageEverything = (folder: string) => {
  const longAgo = new Date(Date.now() - 3600_000);
  for (const name of fs.readdirSync(folder)) {
    fs.utimesSync(path.join(folder, name), longAgo, longAgo);
  }
};

const nodeFs = createRequire(import.meta.url)("node:fs") as Record<string, unknown>;

/** Runs `body` with every step it takes on files watched: `afterEach` is
 * called before and after each one, so a test can let other programs act
 * between any two of them. */
function watchingFiles<T>(afterEach: () => void, body: () => T): T {
  const kept: Record<string, unknown> = {};
  for (const name of [
    "closeSync", "fstatSync", "linkSync", "openSync", "readFileSync", "readSync", "readdirSync",
    "renameSync", "statSync", "unlinkSync", "writeFileSync", "writeSync",
  ]) {
    const real = nodeFs[name] as (...args: unknown[]) => unknown;
    kept[name] = real;
    nodeFs[name] = (...args: unknown[]) => {
      afterEach();
      try {
        return real(...args);
      } finally {
        afterEach();
      }
    };
  }
  syncBuiltinESMExports();
  try {
    return body();
  } finally {
    Object.assign(nodeFs, kept);
    syncBuiltinESMExports();
  }
}

// A lock was left behind. Program C looks at it and is held up. Meanwhile B
// takes it over and holds it. C, going on from what it saw, must not leave a
// gap in which a third program, D, can take the lock as well: whatever C
// does, D is let try between every two of its steps.
test("a lock taken over while another program was held up has one holder", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    assert.notEqual(takeLock(lock), null);
    ageEverything(dataDir());
    const held: Record<string, unknown> = {};
    const busy: string[] = [];
    const asOther = (name: string) => {
      busy.push(name);
      try {
        const got = takeLock(lock);
        if (got !== null) {
          held[name] = got;
        }
      } finally {
        busy.pop();
      }
    };
    const realNow = Date.now;
    Date.now = () => {
      if (busy.length === 0 && !("B" in held)) {
        asOther("B");
      }
      return realNow();
    };
    try {
      const got = watchingFiles(
        () => {
          if (busy.length === 0 && "B" in held && !("D" in held)) {
            asOther("D");
          }
        },
        () => takeLock(lock),
      );
      if (got !== null) {
        held.C = got;
      }
    } finally {
      Date.now = realNow;
    }
    assert.ok("B" in held, "B never got to take the lock over");
    assert.deepEqual(Object.keys(held).sort(), ["B"], "more than one program holds the lock");
  }));

// The mark cannot be written, because the disk is full. The lock is not
// taken, and nothing is left behind to keep the next program out.
test("a lock whose mark cannot be written is not taken and not left", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const lock = path.join(dataDir(), "orvud.log.lock");
    const real = { writeSync: nodeFs.writeSync, writeFileSync: nodeFs.writeFileSync };
    const full = () => {
      throw Object.assign(new Error("ENOSPC: no space left on device"), { code: "ENOSPC" });
    };
    nodeFs.writeSync = full;
    nodeFs.writeFileSync = (file: unknown, data: unknown, options: unknown) => {
      // Made, as the real one would, and then the writing fails.
      (real.writeFileSync as (...args: unknown[]) => void)(file, "", options);
      full();
    };
    syncBuiltinESMExports();
    let got: unknown;
    try {
      got = takeLock(lock);
    } finally {
      Object.assign(nodeFs, real);
      syncBuiltinESMExports();
    }
    assert.equal(got, null);
    assert.notEqual(takeLock(lock), null);
  }));

// The sidecar this program started says why it stopped, but another program
// moved the log aside meanwhile. What it said is still read, from the file
// it went to, and the program waits for the sidecar holding the folder as
// it should.
test("what the sidecar said is read even when the log was moved aside meanwhile", () =>
  withHome(async (home) => {
    const real = process.env.ORVUD_BIN!;
    const once = path.join(home, "orvud-once");
    fs.writeFileSync(
      once,
      "#!/bin/sh\n" +
        `if [ ! -e "$3/moved" ]; then\n` +
        `  touch "$3/moved"\n` +
        `  mv "$3/orvud.log" "$3/orvud.log.1"\n` +
        `  echo "orvud: another sidecar is already using the data folder" >&2\n` +
        `  exit 1\n` +
        `fi\n` +
        `exec ${JSON.stringify(real)} "$@"\n`,
      { mode: 0o755 },
    );
    process.env.ORVUD_BIN = once;
    const client = await connect();
    await client.counter("visits").increment(1);
    client.close();
    assert.ok(fs.existsSync(path.join(dataDir(), "moved")), "the stand-in was not run");
  }));

test("a short log is added to", () =>
  withHome(async () => {
    fs.mkdirSync(dataDir(), { recursive: true, mode: 0o700 });
    const log = path.join(dataDir(), "orvud.log");
    fs.writeFileSync(log, "from before\n");
    (await connect()).close();
    assert.ok(fs.readFileSync(log, "utf8").startsWith("from before\n"));
    assert.equal(fs.existsSync(log + ".1"), false);
  }));

// A program that is meant to use a team's sidecar can be pointed at it by
// the environment, with nothing in its code. Nothing is started for it.
test("the socket can be given in the environment", () =>
  withHome(async (home) =>
    withSidecar(async (sidecar) => {
      const direct = await connect(sidecar.socket);
      await direct.counter("visits").increment(3);
      direct.close();
      process.env.ORVU_SOCKET = sidecar.socket;
      const client = await connect();
      assert.equal(await client.counter("visits").value(), 3);
      client.close();
      const team = await Team.connect("research", { me: "alice" });
      assert.equal(await team.client.counter("visits").value(), 3);
      await team.close();
      assert.deepEqual(sidecars(home), []);
      // A socket given in the call is the one used.
      const local = await connect(await ensureRunning());
      assert.equal(await local.counter("visits").value(), 0);
      local.close();
    }),
  ));

test("a socket and a connection file in the environment are refused", () =>
  withHome(async (home) => {
    process.env.ORVU_SOCKET = path.join(home, "crew.sock");
    process.env.ORVU_CONNECT = path.join(home, "alice.orvu");
    await assert.rejects(connect(), (error: Error) => error instanceof BadRequest && /ORVU_SOCKET and ORVU_CONNECT/.test(error.message));
  }));

// Nothing answers on the socket given. A team's sidecar is never started by
// the package, so the message says how one is made and started.
test("no sidecar on the socket given says how to start a team's one", () =>
  withHome(async (home) => {
    const named = (error: Error) =>
      error instanceof SidecarUnavailable &&
      error.message.includes("crew.sock") &&
      error.message.includes("orvud setup") &&
      error.message.includes("orvud --config");
    await assert.rejects(connect(path.join(home, "crew.sock"), { timeoutMs: 500 }), named);
    process.env.ORVU_SOCKET = path.join(home, "crew.sock");
    await assert.rejects(connect(undefined, { timeoutMs: 500 }), named);
  }));
