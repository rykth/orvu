// A change whose answer never came is sent again, with the id it was first
// sent with, and the sidecar makes it once. So a program is not left asking
// itself whether a change it was told failed was made after all.

import * as grpc from "@grpc/grpc-js";
import * as loader from "@grpc/proto-loader";
import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";
import { connect, SidecarUnavailable } from "../src/index.ts";
import { VERSION } from "../src/version.ts";
import { REPO, shortFolder, sidecars, sleep, stopChild, withHome, Sidecar } from "./support.ts";

const DataPlane = (
  grpc.loadPackageDefinition(
    loader.loadSync(path.join(REPO, "typescript/proto/dataplane.proto"), { keepCase: true, enums: String, oneofs: true }),
  ) as any
).orvu.dataplane.v1.DataPlane as grpc.ServiceClientConstructor;

/** Has the next sidecar started stop at once, the next time it saves a
 * change made for a request id, without answering it. Gives the file whose
 * going says that it did. */
function stopAfterSaving(folder: string): string {
  const mark = path.join(folder, "stop-after-saving");
  process.env.ORVUD_TEST_STOP_AFTER_SAVING = mark;
  return mark;
}

// The sidecar this package starts stops after saving a change and before
// answering it. The client starts it again and sends the change again, and
// the change is made once.
test("a change cut off after it was saved is made once", () =>
  withHome(async (home) => {
    const mark = stopAfterSaving(home);
    const client = await connect(undefined, { timeoutMs: 30_000 });
    try {
      const visits = client.counter("visits");
      await visits.increment(1);
      const [first] = sidecars(home);
      fs.writeFileSync(mark, "");
      await visits.increment(1);
      assert.ok(!fs.existsSync(mark), "the sidecar did not stop after saving");
      assert.ok(!sidecars(home).includes(first));
      assert.equal(await visits.value(), 2);

      // A batch is sent again the same way, and made once.
      fs.writeFileSync(mark, "");
      const batch = client.batch();
      batch.counter("visits").increment(10);
      batch.counter("visits").increment(100);
      await batch.send();
      assert.ok(!fs.existsSync(mark));
      assert.equal(await visits.value(), 112);
    } finally {
      client.close();
    }
  }));

// A sidecar started by hand is not started again by the client. It waits for
// it to come back, and sends the change again then.
test("a change cut off is sent again once the sidecar is back", async () => {
  const folder = shortFolder();
  const kept = process.env.ORVUD_TEST_STOP_AFTER_SAVING;
  const sidecar = new Sidecar(folder);
  const mark = stopAfterSaving(folder);
  let back: Promise<unknown> = Promise.resolve();
  let first: ChildProcess | null = null;
  try {
    await sidecar.start();
    first = sidecar.process;
    const client = await connect(sidecar.socket, { timeoutMs: 30_000 });
    try {
      const visits = client.counter("visits");
      fs.writeFileSync(mark, "");
      back = sleep(1000).then(async () => {
        await sidecar.start();
      });
      await visits.increment(1);
      await back;
      assert.ok(!fs.existsSync(mark));
      assert.notEqual(sidecar.process, first);
      assert.equal(await visits.value(), 1);
    } finally {
      client.close();
    }
  } finally {
    await back.catch(() => {});
    await sidecar.stop();
    if (first) {
      await stopChild(first);
    }
    restore(kept);
    fs.rmSync(folder, { recursive: true, force: true });
  }
});

// A sidecar that does not come back is waited for only so long: the change
// then ends in an error, as before.
test("a sidecar that stays away still ends in an error", async () => {
  const folder = shortFolder();
  const kept = process.env.ORVUD_TEST_STOP_AFTER_SAVING;
  const sidecar = new Sidecar(folder);
  const mark = stopAfterSaving(folder);
  try {
    await sidecar.start();
    const client = await connect(sidecar.socket, { timeoutMs: 2_000 });
    try {
      fs.writeFileSync(mark, "");
      const started = Date.now();
      await assert.rejects(client.counter("visits").increment(1), SidecarUnavailable);
      assert.ok(Date.now() - started < 30_000);
      assert.ok(!fs.existsSync(mark));
    } finally {
      client.close();
    }
  } finally {
    await sidecar.stop();
    restore(kept);
    fs.rmSync(folder, { recursive: true, force: true });
  }
});

function restore(kept: string | undefined): void {
  if (kept === undefined) {
    delete process.env.ORVUD_TEST_STOP_AFTER_SAVING;
  } else {
    process.env.ORVUD_TEST_STOP_AFTER_SAVING = kept;
  }
}

/** Runs `body` with a stand-in for a sidecar that takes longer than the
 * client waits over the first try of the first change it is sent, and keeps
 * what it was sent. */
async function withSlow(body: (socket: string, sent: any[]) => Promise<void>, holdUpFirst = true): Promise<void> {
  const folder = shortFolder();
  const socket = path.join(folder, "s");
  const sent: any[] = [];
  const server = new grpc.Server();
  server.addService(DataPlane.service, {
    Open: (_call: unknown, answer: grpc.sendUnaryData<unknown>) =>
      answer(null, { session: "s", resume_token: "r", replica_id: "1", version: VERSION, agent: "" }),
    Execute: (call: any, answer: grpc.sendUnaryData<unknown>) => {
      sent.push(call.request);
      const wait = holdUpFirst && sent.length === 1 ? 1500 : 0;
      setTimeout(() => answer(null, { ack: {} }), wait);
    },
  });
  await new Promise<void>((resolve, reject) =>
    server.bindAsync(`unix:${socket}`, grpc.ServerCredentials.createInsecure(), (error) => (error ? reject(error) : resolve())),
  );
  try {
    await body(socket, sent);
  } finally {
    server.forceShutdown();
    fs.rmSync(folder, { recursive: true, force: true });
  }
}

// A change that ran out of time is sent again with the same id, and the
// caller gets its answer and not `TimedOut`.
test("a change that ran out of time is sent again with its id", () =>
  withSlow(async (socket, sent) => {
    const client = await connect(socket, { timeoutMs: 1_000 });
    try {
      await client.counter("visits").increment(1);
    } finally {
      client.close();
    }
    const [first, again] = sent;
    assert.equal(sent.length, 2);
    assert.equal(first.request_id.length, 16);
    assert.deepEqual(again.request_id, first.request_id);
  }));

// Each change has an id of its own, and a read has none.
test("each change has an id of its own and a read has none", () =>
  withSlow(async (socket, sent) => {
    const client = await connect(socket, { timeoutMs: 5_000 });
    try {
      await client.counter("visits").increment(1);
      await client.counter("visits").increment(1);
      await client.counter("visits").value();
    } finally {
      client.close();
    }
    const [one, two, read] = sent;
    assert.equal(one.request_id.length, 16);
    assert.equal(two.request_id.length, 16);
    assert.notDeepEqual(one.request_id, two.request_id);
    assert.equal(read.request_id?.length ?? 0, 0);
  }, false));

// A program that closes its client while a change waits for the sidecar to
// come back is not held open by that wait: the change ends at once, and
// nothing left behind keeps the program running.
test("closing the client ends a change that waits for the sidecar", async () => {
  const folder = shortFolder();
  const kept = process.env.ORVUD_TEST_STOP_AFTER_SAVING;
  const sidecar = new Sidecar(folder);
  const mark = stopAfterSaving(folder);
  try {
    await sidecar.start();
    const program = `
      import { connect } from ${JSON.stringify(path.join(REPO, "typescript/src/index.ts"))};
      import * as fs from "node:fs";
      const client = await connect(${JSON.stringify(sidecar.socket)}, { timeoutMs: 30_000 });
      fs.writeFileSync(${JSON.stringify(mark)}, "");
      const change = client.counter("visits").increment(1);
      setTimeout(() => client.close(), 500);
      const started = Date.now();
      await change.then(() => console.log("made"), (error) => console.log(error.constructor.name));
      console.log("ended after", Date.now() - started);
    `;
    const child = spawn(process.execPath, ["--input-type=module", "-e", program], { stdio: ["ignore", "pipe", "inherit"] });
    let said = "";
    child.stdout.on("data", (chunk) => (said += chunk));
    const ended = await Promise.race([new Promise((resolve) => child.once("exit", resolve)), sleep(5000).then(() => "still running")]);
    if (ended === "still running") {
      child.kill("SIGKILL");
    }
    assert.equal(ended, 0, said);
    assert.match(said, /BadRequest/);
    assert.ok(!fs.existsSync(mark));
  } finally {
    await sidecar.stop();
    restore(kept);
    fs.rmSync(folder, { recursive: true, force: true });
  }
});
