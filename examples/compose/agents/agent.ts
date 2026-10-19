// One agent, written in TypeScript, on one machine of the team.
//
// It connects to its machine's sidecar over the local socket, with its own
// key, and joins the roster. Then it waits for the story to tell it what to
// do next: the story sends it one step at a time over HTTP, as
// {"do": ..., "with": {...}}, and the agent answers with what it got back.
// Every step is an ordinary call of the `@orvuhq/orvu` package, the same as a real
// agent would make. The steps have the same names as the Python agent's.
//
//     AGENT    the agent's name, which is also the name of its key file
//     MACHINE  the machine it runs on, for the story to show

import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { Clash, OrvuError, Team } from "@orvuhq/orvu";
import type { Task } from "@orvuhq/orvu";

const AGENT = process.env.AGENT!;
const MACHINE = process.env.MACHINE!;
const TEAM = "crew";
const SOCKET = "/sock/orvud.sock";

function connect(): Promise<Team> {
  const key = readFileSync(`/keys/${AGENT}.key`, "utf8").trim();
  // The socket is given, so the package never starts a sidecar of its own.
  return Team.connect(TEAM, { socket: SOCKET, key });
}

const team = await connect();
await team.roster.join(5);
const watched = new Map<string, unknown[]>();

type Step = Record<string, any>;

/** A note's value as JSON: a clash as what each agent wrote. */
function shown(value: unknown): unknown {
  return value instanceof Clash ? { clash: { values: value.values, authors: value.authors } } : value;
}

function taskShown(task: Task | null) {
  if (task === null) {
    return null;
  }
  return {
    id: task.id,
    title: task.title,
    state: task.state,
    claimed_by: task.claimedBy,
    created_by: task.createdBy,
    finished_by: task.finishedBy,
    result: task.result,
    firm: task.firm,
    ticket: task.ticket,
    taken_over: task.takenOver,
  };
}

/** Waits until `at`, in milliseconds since 1970, so that two agents can act
 * at the same moment. */
async function waitUntil(at: number | undefined) {
  if (at !== undefined) {
    await new Promise((done) => setTimeout(done, Math.max(0, at - Date.now())));
  }
}

const steps: Record<string, (step: Step) => Promise<unknown>> = {
  hello: async () => ({ me: team.me, machine: MACHINE, language: "typescript" }),

  note_put: async (step) => {
    await waitUntil(step.at);
    await team.notes.put(step.name, step.value);
  },
  note_get: async (step) => {
    try {
      return { value: await team.notes.get(step.name), author: await team.notes.author(step.name) };
    } catch (error) {
      if (error instanceof Clash) {
        return shown(error);
      }
      throw error;
    }
  },

  task_add: async (step) => taskShown(await team.tasks.add(step.title, { firm: step.firm ?? false })),
  task_get: async (step) => taskShown(await team.tasks.get(step.id)),
  task_list: async (step) => (await team.tasks.list(step.state)).map(taskShown),
  task_claim: async (step) => team.tasks.claim(step.id, undefined, { lease: step.lease }),
  task_claim_ticket: async (step) => team.tasks.claimTicket(step.id, undefined, { lease: step.lease }),
  task_still_working: async (step) => team.tasks.stillWorking(step.id),
  task_still_holding: async (step) => team.tasks.stillHolding(step.id, step.ticket),
  task_finish: async (step) => team.tasks.finish(step.id, step.result ?? null),
  task_set_back: async (step) =>
    taskShown(await team.tasks.update(step.id, { state: "todo", force: step.force ?? false })),

  budget_top_up: async (step) => team.budget(step.name).topUp(step.n),
  budget_spend: async (step) => team.budget(step.name).spend(step.n),
  budget_read: async (step) => {
    const budget = team.budget(step.name);
    return { left: await budget.left(), spent_by: await budget.spentBy(), topped_up_by: await budget.toppedUpBy() };
  },

  inbox_send: async (step) =>
    step.to === undefined || step.to === null ? team.inbox.sendAll(step.body) : team.inbox.send(step.to, step.body),
  inbox_unread: async () =>
    (await team.inbox.unread()).map((message) => ({ sender: message.sender, to: message.to, body: message.body })),

  doc_append: async (step) => team.document(step.name).append(step.text),
  doc_read: async (step) => {
    const document = team.document(step.name);
    return { text: await document.text(), written_by: await document.writtenBy() };
  },

  tally_add: async (step) => team.tally(step.name).add(step.n),
  tally_read: async (step) => {
    const tally = team.tally(step.name);
    return { total: await tally.total(), by_name: await tally.byName() };
  },

  roster_here: async () => team.roster.here(),

  search: async (step) =>
    (await team.search(step.query, step.limit ?? 5)).map((found) => ({
      kind: found.kind,
      name: found.name,
      title: found.title ?? null,
      text: found.text,
      score: found.score,
    })),

  // Starts watching the team's notes in the background, on a connection of
  // its own, until the note `name` says `until`. `watched` keeps every value
  // of that note seen on the way.
  watch_note: async (step) => {
    const seen: unknown[] = [];
    watched.set(`note/${step.name}`, seen);
    const mine = await connect();
    (async () => {
      try {
        for await (const notes of mine.notes.watch()) {
          seen.push(shown(notes[step.name] ?? null));
          if (notes[step.name] === step.until) {
            break;
          }
        }
      } finally {
        await mine.close();
      }
    })().catch((error) => seen.push({ error: String(error) }));
  },
  watch_seen: async (step) => watched.get(step.what) ?? null,
};

// One step at a time on the agent's connection.
let turn: Promise<unknown> = Promise.resolve();

createServer((request, response) => {
  const chunks: Buffer[] = [];
  request.on("data", (chunk) => chunks.push(chunk));
  request.on("end", () => {
    const asked = JSON.parse(Buffer.concat(chunks).toString());
    const run = turn.then(async () => {
      try {
        return { ok: true, result: (await steps[asked.do](asked.with ?? {})) ?? null };
      } catch (error) {
        if (error instanceof OrvuError) {
          return { ok: false, error: error.constructor.name, message: error.message };
        }
        throw error;
      }
    });
    turn = run.catch(() => {});
    run.then(
      (answer) => {
        response.setHeader("Content-Type", "application/json");
        response.end(JSON.stringify(answer));
      },
      (error) => {
        response.statusCode = 500;
        response.end(String(error?.stack ?? error));
      },
    );
  });
}).listen(8000);
