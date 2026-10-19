#!/bin/sh
# Installs the package built with no sidecar inside, which is for Windows,
# into an empty folder, with an empty home folder, and connects to a sidecar
# over the network with a connection file. That is how an agent on Windows
# uses a team. It also asks the package, with Node told it is on Windows, for
# a sidecar of its own, and checks it is told to connect over the network.
#
#     sh typescript/scripts/clean-test-network.sh typescript/release/orvu-0.1.0.tgz
#
# The sidecar it connects to is the one built from this repository,
# target/debug/orvud, or $ORVUD_BIN. npm has to be able to fetch the two
# packages this one uses.
set -e
PACKAGE="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
ORVUD="${ORVUD_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/target/debug/orvud}"
# Node itself, not a version manager's stand-in for it.
NODE="$(node -p process.execPath)"

# A short path, because the sidecar's socket goes inside it.
WORK="$(cd "$(mktemp -d /tmp/orvu-clean-XXXXXX)" && pwd -P)"
SIDECAR=""
stop() {
    # Only the sidecar this started is stopped.
    if [ -n "$SIDECAR" ]; then
        kill "$SIDECAR" 2>/dev/null || true
        wait "$SIDECAR" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
# Ctrl-C, or being told to stop, cleans up too.
trap stop EXIT
trap 'exit 130' INT TERM

mkdir "$WORK/app" "$WORK/home"
# npm keeps what it fetches in a folder of this test's own.
export npm_config_cache="$WORK/npm"
cd "$WORK/app"
npm init -y >/dev/null
# The package is for Windows, and npm is told to install it here all the same.
npm install --silent --force "$PACKAGE" 2>"$WORK/npm.out" || { cat "$WORK/npm.out"; exit 1; }

echo "--- the package has no sidecar inside, and no orvud command"
test ! -e node_modules/orvu/bin
test ! -e node_modules/.bin/orvud
echo "none"

echo "--- a sidecar elsewhere, serving the network"
PORT="$("$NODE" -e 'const s = require("node:net").createServer().listen(0, "127.0.0.1", () => { console.log(s.address().port); s.close(); })')"
"$ORVUD" agent add alice --address "127.0.0.1:$PORT" --connection "$WORK/alice.orvu" --local --data-dir "$WORK/sidecar" >/dev/null
"$ORVUD" --local --data-dir "$WORK/sidecar" --socket "$WORK/sidecar/s" --network "127.0.0.1:$PORT" 2>"$WORK/sidecar.out" &
SIDECAR=$!
i=0
until grep -q "serving on" "$WORK/sidecar.out"; do
    i=$((i + 1))
    if [ $i -gt 100 ] || ! kill -0 "$SIDECAR" 2>/dev/null; then cat "$WORK/sidecar.out"; exit 1; fi
    sleep 0.1
done
echo "up"

# Nothing of the real user's is visible: no home folder, no sidecar of its
# own, nothing but the connection file.
run() {
    env -i HOME="$WORK/home" PATH="/usr/bin:/bin" ORVU_CONNECT="$WORK/alice.orvu" "$NODE" --input-type=module -e "$1"
}

echo "--- an agent with only a connection file"
run '
import { Team } from "orvu";
const team = await Team.connect();
await team.notes.put("deploy", "Friday");
console.log(team.me, await team.notes.get("deploy"));
await team.close();
'

echo "--- on Windows, asked for a sidecar of its own"
run '
Object.defineProperty(process, "platform", { value: "win32" });
const orvu = await import("orvu");
delete process.env.ORVU_CONNECT;
try {
  await orvu.connect();
  process.exit(1);
} catch (error) {
  if (!(error instanceof orvu.SidecarUnavailable) || !error.message.includes("ORVU_CONNECT")) {
    throw error;
  }
  console.log("it says to connect with ORVU_CONNECT");
}
'
