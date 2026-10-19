// Runs the examples in README.md, so that they cannot go stale.

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";
import { REPO, withHome } from "./support.ts";

const README = fs.readFileSync(path.join(REPO, "typescript/README.md"), "utf8");
const EXAMPLES = [...README.matchAll(/```ts\n(.*?)```/gs)].map((found) => found[1]);

test("the readme has examples", () => {
  assert.equal(EXAMPLES.length, 4);
  for (const example of EXAMPLES) {
    assert.match(example, /from "orvu"/);
  }
});

// Each example is run exactly as it is written, as a program of its own, in a
// home folder of its own with no sidecar running. Only where "orvu" comes
// from is changed, to this repository's copy.
EXAMPLES.forEach((example, at) =>
  test(`readme example ${at + 1}`, () =>
    withHome(async (home) => {
      const program = path.join(home, `example-${at + 1}.mts`);
      fs.writeFileSync(program, example.replace('from "orvu"', `from ${JSON.stringify(path.join(REPO, "typescript/src/index.ts"))}`));
      const printed = execFileSync(process.execPath, [program], { timeout: 60_000, encoding: "utf8", env: process.env });
      const expected = [
        "Friday\ndone\ntrue\ntrue\nfalse\n[ [ 'Sales rose 4%.', 'researcher-2' ] ]\ndeploy-date\n[ 'Deploy is on Friday.' ]\n[ 'researcher-2' ]\n2\n",
        "5\ntrue\nfalse\n[ 'research', 'draft', 'publish' ]\n",
        "false []\n",
        "starting\n",
      ];
      assert.equal(printed, expected[at]);
    }),
  ),
);

// The quickstart is run as a new user runs it: twice, and the second run
// sees what the first one wrote.
test("the quickstart runs twice and remembers", () =>
  withHome(async (home) => {
    const program = path.join(home, "quickstart.mjs");
    const text = fs.readFileSync(path.join(REPO, "typescript/examples/quickstart.mjs"), "utf8");
    fs.writeFileSync(program, text.replace('from "orvu"', `from ${JSON.stringify(path.join(REPO, "typescript/src/index.ts"))}`));
    const run = () => execFileSync(process.execPath, [program], { timeout: 60_000, encoding: "utf8", env: process.env });
    assert.match(run(), /run 1 time/);
    const second = run();
    assert.match(second, /run 2 time/);
    assert.match(second, /notes so far: \[ 'run number 1', 'run number 2' \]/);
  }));
