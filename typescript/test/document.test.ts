// Documents: longer text that several agents write together.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BadRequest } from "../src/index.ts";
import { teamOn, withTeam } from "./support.ts";

test("a document is written and read", () =>
  withTeam(async (team) => {
    const report = team.document("q3-report");
    assert.equal(await report.text(), "");
    await report.append("Sales rose 4%.");
    await report.append(" Costs fell.");
    await report.insert(0, "Q3: ");
    assert.equal(await report.text(), "Q3: Sales rose 4%. Costs fell.");
    await report.delete(0, 4);
    assert.equal(await team.document("q3-report").text(), "Sales rose 4%. Costs fell.");
  }));

test("what an agent writes is marked with its name", () =>
  withTeam(async (team, bob, sidecar) => {
    const report = team.document("q3-report");
    await report.append("Sales rose 4%.");
    await bob.document("q3-report").append(" Costs fell.");
    await report.insert(0, "Q3: ");
    assert.deepEqual(await report.writtenBy(), [["Q3: Sales rose 4%.", "alice"], [" Costs fell.", "bob"]]);
    // Text put in the middle of another agent's is the newcomer's.
    await bob.document("q3-report").insert(4, "(draft) ");
    assert.deepEqual(await report.writtenBy(), [
      ["Q3: ", "alice"],
      ["(draft) ", "bob"],
      ["Sales rose 4%.", "alice"],
      [" Costs fell.", "bob"],
    ]);
    const nameless = await teamOn(sidecar);
    await nameless.document("q3-report").append(" The end.");
    await nameless.close();
    assert.deepEqual((await report.writtenBy()).at(-1), [" The end.", null]);
    assert.equal(await report.text(), "Q3: (draft) Sales rose 4%. Costs fell. The end.");
  }));

// Positions count characters, and an emoji is two units of a JavaScript
// string. The mark has to land on exactly what was written.
test("the mark lands on exactly what was written", () =>
  withTeam(async (team, bob) => {
    const page = team.document("notes");
    await page.append("héllo 🙂 ");
    await bob.document("notes").append("wörld 🌍");
    await page.insert(999, "!");
    assert.deepEqual(await page.writtenBy(), [["héllo 🙂 ", "alice"], ["wörld 🌍", "bob"], ["!", "alice"]]);
  }));

test("other marks work as on plain rich text", () =>
  withTeam(async (team) => {
    const page = team.document("notes");
    await page.append("Sales rose 4%.");
    await page.format(0, 5, "bold", "true");
    assert.deepEqual((await page.toSpans())[0], { text: "Sales", marks: { bold: "true", by: "alice" } });
    await page.unformat(0, 5, "bold");
    assert.deepEqual(await page.writtenBy(), [["Sales rose 4%.", "alice"]]);
  }));

test("the team lists its documents once they are written to", () =>
  withTeam(async (team, bob) => {
    await team.document("empty").text();
    assert.deepEqual(await team.documents(), []);
    assert.deepEqual(await team.index(), {});
    await team.document("q3-report").append("x");
    await bob.document("plan").append("y");
    assert.deepEqual(await team.documents(), ["plan", "q3-report"]);
    assert.equal((await team.index())["doc/plan"], "richtext");
  }));

test("a document needs a name that can be used", () =>
  withTeam(async (team) => {
    for (const name of ["", "a/b", "two words", null]) {
      assert.throws(() => team.document(name as string), (error: Error) => error instanceof BadRequest && /name of a document/.test(error.message));
    }
  }));

test("a document is watched", () =>
  withTeam(async (team, bob) => {
    await bob.document("q3-report").append("Sales rose 4%.");
    for await (const spans of team.document("q3-report").watch()) {
      assert.equal(spans.map((span) => span.text).join(""), "Sales rose 4%.");
      break;
    }
  }));

// Nothing to add is nothing done. It does not start a document.
test("adding no text does nothing", () =>
  withTeam(async (team) => {
    await team.document("q3-report").append("");
    await team.document("q3-report").insert(0, "");
    assert.deepEqual(await team.documents(), []);
    assert.deepEqual(await team.index(), {});
  }));

// Another agent puts text in at the moment this one adds to the end. The new
// text still goes at the end, and the mark still lands on exactly it, and it
// is one message: nothing is read first to find the end.
test("the mark holds when another agent writes at the same moment", () =>
  withTeam(async (team, bob) => {
    const report = team.document("q3-report");
    await report.append("Sales rose 4%.");
    const client = team.client;
    const real = client.call.bind(client);
    const sent: string[] = [];
    let raced = false;
    const writes = (message: any) =>
      [message, ...(message.requests ?? [])].some((one) => one.richtext_insert || one.richtext_insert_marked);
    client.call = async (method, request, waitMs) => {
      sent.push(method);
      if (!raced && writes(request(""))) {
        raced = true;
        await bob.document("q3-report").insert(0, "Draft: ");
      }
      return real(method, request, waitMs);
    };
    await report.append(" Costs fell.");
    client.call = real;
    assert.ok(raced);
    assert.equal(sent.length, 1, `${sent}`);
    assert.equal(await report.text(), "Draft: Sales rose 4%. Costs fell.");
    assert.deepEqual(await report.writtenBy(), [["Draft: ", "bob"], ["Sales rose 4%. Costs fell.", "alice"]]);
  }));
