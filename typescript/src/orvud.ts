#!/usr/bin/env node
/**
 * The `orvud` command that comes with the package, for running the sidecar
 * by hand, as in `npx --no-install orvud setup`.
 *
 * The program itself is not in this package: it comes in a package of its own
 * for each kind of machine. This finds it the same way `connect()` does and
 * runs it with the same arguments. Whatever the program prints, the code it
 * ends with, and the signals sent to this command all pass straight through,
 * so a script cannot tell the two apart.
 *
 * @module
 */

import { spawn } from "node:child_process";
import { SidecarUnavailable } from "./errors.ts";
import { program } from "./sidecar.ts";

// Set for the program this command runs. If it is set already, the program
// found was another copy of this command, such as one installed elsewhere
// and first on the PATH, and running it would only find the next copy.
const LAUNCHED = "ORVUD_LAUNCHED_BY";
if (process.env[LAUNCHED]) {
  console.error(
    `orvud: the orvud found was another copy of this command (run from ${process.env[LAUNCHED]}), not the program. ` +
      "Set $ORVUD_BIN to the program, or install the sidecar package for this machine",
  );
  process.exit(1);
}

let file: string;
try {
  file = program();
} catch (error) {
  if (!(error instanceof SidecarUnavailable)) {
    throw error;
  }
  console.error(`orvud: ${error.message}`);
  process.exit(1);
}

const child = spawn(file, process.argv.slice(2), {
  stdio: "inherit",
  env: { ...process.env, [LAUNCHED]: process.argv[1] },
});

// A signal sent to this command is passed on to the program, which decides
// what to do with it. Ctrl-C reaches the program by itself as well; a second
// one does no harm, as the program is already stopping.
const PASSED_ON = ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT", "SIGUSR1", "SIGUSR2"] as const;
const passOn = (signal: NodeJS.Signals) => child.kill(signal);
for (const signal of PASSED_ON) {
  process.on(signal, passOn);
}

child.once("error", (error) => {
  console.error(`orvud: could not run ${file}: ${error.message}`);
  process.exit(1);
});

child.once("exit", (code, signal) => {
  if (signal !== null) {
    // Ended by a signal, the program is copied: this command ends by the same
    // one, once it no longer catches it.
    for (const caught of PASSED_ON) {
      process.off(caught, passOn);
    }
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
