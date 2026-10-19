#!/bin/sh
# Installs the built library and the sidecar package for this kind of
# machine into an empty folder, with an empty home folder, and runs the
# quickstart, a team, and a team on two machines. This is what a new user's
# first commands do, so it is the test that the packages work by themselves.
#
#     sh typescript/scripts/clean-test.sh typescript/release/orvuhq-orvu-0.1.0.tgz \
#         typescript/release/orvuhq-orvud-linux-x64-0.1.0.tgz
#
# It needs Node and npm, and npm has to be able to fetch the two packages
# the library uses.
#
# The library names the sidecar packages for every kind of machine as
# optional. Until they are published, npm finds none of them online. It
# skips an optional package it cannot find, and the one for this machine is
# given to it here as a file, which stands in for it.
set -e
if [ $# -ne 2 ]; then
    echo "usage: clean-test.sh orvuhq-orvu-<version>.tgz orvuhq-orvud-<os>-<cpu>-<version>.tgz" >&2
    exit 2
fi
PACKAGE="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
SIDECAR="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
EXAMPLES="$(cd "$(dirname "$0")/../examples" && pwd)"
# Node itself, not a version manager's stand-in for it, which may need
# settings this test leaves out.
NODE="$(node -p process.execPath)"

# A short path, because the sidecar's socket goes inside it. On a Mac /tmp
# leads to /private/tmp, and the sidecars see the second name, so the folder
# is known by that one.
WORK="$(cd "$(mktemp -d /tmp/orvu-clean-XXXXXX)" && pwd -P)"

# The command line of a running program.
command_of() {
    if [ -r "/proc/$1/cmdline" ]; then
        tr '\0' ' ' < "/proc/$1/cmdline"
    else
        ps -o args= -p "$1"
    fi
}
# The sidecars running for this folder. They are found by their command line,
# which holds the folder, so that only this test's are found. Small Linux
# containers have no pgrep, so there they are looked for one by one.
ours() {
    if command -v pgrep >/dev/null; then
        pgrep -f "orvud.*$WORK" || true
    else
        for folder in /proc/[0-9]*; do
            if command_of "${folder#/proc/}" 2>/dev/null | grep -q "orvud.*$WORK"; then
                echo "${folder#/proc/}"
            fi
        done
    fi
}
stop() {
    for pid in $(ours); do
        kill "$pid" 2>/dev/null || true
    done
    waited=0
    while [ -n "$(ours)" ] && [ "$waited" -lt 50 ]; do
        sleep 0.1
        waited=$((waited + 1))
    done
    rm -rf "$WORK"
}
trap stop EXIT

mkdir "$WORK/app" "$WORK/home"
# npm keeps what it fetches in a folder of this test's own, not in the home
# folder of whoever runs it.
export npm_config_cache="$WORK/npm"
cd "$WORK/app"
npm init -y >/dev/null
npm install --silent "$PACKAGE" "$SIDECAR"

# Nothing of the real user's is visible: no home folder, no running sidecar,
# no orvud on the PATH except the command that came with the package. Node is
# on it, as for anyone who installs with npm: the command is run by Node.
NODE_DIR="$(dirname "$NODE")"
if [ -e "$NODE_DIR/orvud" ]; then
    echo "$NODE_DIR, where Node is, has an orvud in it too" >&2
    exit 1
fi
run() {
    env -i HOME="$WORK/home" PATH="$WORK/app/node_modules/.bin:$NODE_DIR:/usr/bin:/bin" "$@"
}

echo "--- Node $("$NODE" --version)"

MACHINE="$("$NODE" -p 'process.platform + "-" + process.arch')"
echo "--- the library is for every kind of machine, and names every sidecar package"
"$NODE" -e '
const p = require("./node_modules/@orvuhq/orvu/package.json");
if (p.os !== undefined || p.cpu !== undefined) throw new Error("the library says which machines it is for");
const names = ["linux-x64", "linux-arm64", "darwin-arm64", "darwin-x64"].map((m) => "@orvuhq/orvud-" + m).sort();
const optional = p.optionalDependencies ?? {};
if (JSON.stringify(Object.keys(optional).sort()) !== JSON.stringify(names)) throw new Error("it names " + Object.keys(optional));
for (const name of names) if (optional[name] !== p.version) throw new Error(name + " is not at " + p.version);
console.log(names.join(" "), "at", p.version);
'

echo "--- the sidecar package is for this kind of machine only"
SAYS="$("$NODE" -p "const p = require(\"./node_modules/@orvuhq/orvud-$MACHINE/package.json\"); p.os + \"-\" + p.cpu + \" \" + p.version")"
test "$SAYS" = "$MACHINE $("$NODE" -p 'require("./node_modules/@orvuhq/orvu/package.json").version')"
echo "$SAYS"
# The same package, said to be for another kind of machine, is refused.
mkdir "$WORK/other"
tar -xzf "$SIDECAR" -C "$WORK/other"
cd "$WORK/other/package"
npm pkg set "cpu[0]=other"
npm pack --silent --pack-destination .. >/dev/null
cd ..
npm init -y >/dev/null
if npm install ./orvuhq-orvud-*.tgz >/dev/null 2>"$WORK/refused"; then
    echo "npm installed a package for another kind of machine" >&2
    exit 1
fi
grep -q EBADPLATFORM "$WORK/refused"
cd "$WORK/app"

echo "--- the library alone, as people install it"
# npm installs the sidecar package for this kind of machine if it has been
# published, and skips it if not. Either way the library does the right
# thing: runs it, or says which package to install.
mkdir "$WORK/alone"
cd "$WORK/alone"
npm init -y >/dev/null
npm install --silent "$PACKAGE"
if [ -e "node_modules/@orvuhq/orvud-$MACHINE" ]; then
    echo "(the sidecar package for $MACHINE is published, and came along)"
    env -i HOME="$WORK/home" PATH="$WORK/alone/node_modules/.bin:$NODE_DIR:/usr/bin:/bin" orvud --version
else
    if env -i HOME="$WORK/home" PATH="$WORK/alone/node_modules/.bin:$NODE_DIR:/usr/bin:/bin" orvud --version 2>"$WORK/alone.out"; then
        echo "orvud ran with no sidecar package installed" >&2
        exit 1
    fi
    cat "$WORK/alone.out"
    grep -q "@orvuhq/orvud-$MACHINE" "$WORK/alone.out"
fi
cd "$WORK/app"

echo "--- the orvud command, which runs the sidecar from its package"
test "$(run sh -c 'command -v orvud')" = "$WORK/app/node_modules/.bin/orvud"
run orvud --version

echo "--- the quickstart, twice"
cp "$EXAMPLES/quickstart.mjs" .
run "$NODE" quickstart.mjs
run "$NODE" quickstart.mjs | tee "$WORK/second"
grep -q "run 2 time" "$WORK/second"

echo "--- a team: notes, a task, a budget"
cat > team.mjs <<'EOF'
import { Team } from "@orvuhq/orvu";
const team = await Team.connect("default", { me: "clean-test" });
await team.notes.put("deploy-date", "Friday");
const task = await team.tasks.add("Summarise the Q3 report");
await team.tasks.claim(task);
await team.tasks.finish(task, "It went up.");
await team.budget("tokens").topUp(10);
console.log("note:", await team.notes.get("deploy-date"));
console.log("task:", (await team.tasks.get(task)).state);
console.log("spent:", await team.budget("tokens").spend(4), "left:", await team.budget("tokens").left());
await team.close();
EOF
run "$NODE" team.mjs | tee "$WORK/team"
grep -q "^task: done" "$WORK/team"
grep -q "^spent: true left: 6" "$WORK/team"

echo "--- one team on two machines, set up with the sidecar's own command"
cat > machines.mjs <<'EOF'
import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as net from "node:net";
import { Team } from "@orvuhq/orvu";

// A port nothing is using, asked of the system.
const freePort = () =>
  new Promise((resolve) => {
    const server = net.createServer().listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
const crew = process.argv[2];
const names = ["alpha", "beta"];
const ports = [await freePort(), await freePort()];
// Ends when the program does, and fails if it could not be started.
const ended = (child) =>
  new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", resolve);
  });
const setup = spawn("orvud", ["setup", crew, ...names.map((name, at) => `${name}=127.0.0.1:${ports[at]}`)], { stdio: "inherit" });
const code = await ended(setup);
if (code !== 0) throw new Error(`orvud setup ended with ${code}`);
// The full paths are on the command line, so the test can find these
// sidecars if something goes wrong.
const running = names.map((name) =>
  spawn("orvud", ["--config", `${crew}/${name}/orvu.toml`, "--socket", `${crew}/${name}/orvud.sock`], { stdio: "ignore" }),
);
const stopped = running.map(ended);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
try {
  const team = async (name) => {
    for (let waited = 0; !fs.existsSync(`${crew}/${name}/orvud.sock`); waited += 50) {
      if (waited > 10_000) throw new Error(`${name} did not start`);
      await sleep(50);
    }
    return Team.connect("research", { socket: `${crew}/${name}/orvud.sock`, me: name });
  };
  const alpha = await team("alpha");
  const beta = await team("beta");
  await alpha.notes.put("from", "the other machine");
  for (let waited = 0; (await beta.notes.get("from")) !== "the other machine"; waited += 50) {
    if (waited > 10_000) throw new Error("the note did not reach the second machine");
    await sleep(50);
  }
  console.log("beta read what alpha wrote:", await beta.notes.get("from"));
  await alpha.close();
  await beta.close();
} finally {
  for (const sidecar of running) {
    sidecar.kill("SIGTERM");
  }
  await Promise.all(stopped);
}
EOF
run "$NODE" machines.mjs "$WORK/crew" | tee "$WORK/machines"
grep -q "^beta read what alpha wrote: the other machine" "$WORK/machines"

echo "--- every run shared one sidecar, started from the sidecar package"
# The sidecar prints one line each time it starts, and that goes into a file
# in its data folder.
LOG="$(find "$WORK/home" -name orvud.log)"
test "$(grep -c "serving on" "$LOG")" -eq 1
for pid in $(ours); do
    command_of "$pid"
    echo
done | grep -- "--local" | tee "$WORK/sidecar"
grep -q "^$WORK/app/node_modules/@orvuhq/orvud-$MACHINE/bin/orvud " "$WORK/sidecar"

echo "clean test passed"
