#!/bin/sh
# Installs the package built with no sidecar inside into an empty
# environment, with an empty home folder, and connects to a sidecar over the
# network with a connection file. That is how an agent on Windows uses a
# team.
#
#     sh python/scripts/clean-test-network.sh python/dist/orvu-*-py3-none-win*.whl [python]
#
# The package is made out to Windows, and pip installs it only there. Here it
# is installed under a name pip takes on any machine; what is inside is the
# same.
#
# The sidecar it connects to is the one built from this repository,
# target/debug/orvud, or $ORVUD_BIN. The second argument is the Python to use.
# It is `python3` if left out.
set -e
WHEEL="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
PYTHON="${2:-python3}"
ORVUD="${ORVUD_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/target/debug/orvud}"

# A short path, because the sidecar's socket goes inside it.
WORK="$(mktemp -d /tmp/orvu-clean-XXXXXX)"
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

# pip keeps what it fetches in a folder of this test's own, and reads no
# settings of whoever runs it.
export PIP_CACHE_DIR="$WORK/pip" PIP_CONFIG_FILE=/dev/null
"$PYTHON" -m venv "$WORK/venv"
ANY="$WORK/$(basename "$WHEEL" | sed 's/-py3-none-.*\.whl$/-py3-none-any.whl/')"
cp "$WHEEL" "$ANY"
"$WORK/venv/bin/pip" install --quiet "$ANY"

echo "--- the package has no sidecar inside"
"$WORK/venv/bin/python" -c "import orvu, pathlib; assert not (pathlib.Path(orvu.__file__).parent / '_bin').exists()"
echo "none"

echo "--- a sidecar elsewhere, serving the network"
PORT="$("$PYTHON" -c "import socket; s = socket.socket(); s.bind(('127.0.0.1', 0)); print(s.getsockname()[1])")"
"$ORVUD" agent add alice --address "127.0.0.1:$PORT" --connection "$WORK/alice.orvu" --local --data-dir "$WORK/sidecar" >/dev/null
"$ORVUD" --local --data-dir "$WORK/sidecar" --socket "$WORK/sidecar/s" --network "127.0.0.1:$PORT" 2>"$WORK/sidecar.out" &
SIDECAR=$!
i=0
until grep -q "serving on" "$WORK/sidecar.out"; do
    i=$((i + 1))
    # A sidecar that stopped, because the port was taken in the meantime or
    # for any other reason, is reported at once.
    if [ $i -gt 100 ] || ! kill -0 "$SIDECAR" 2>/dev/null; then cat "$WORK/sidecar.out"; exit 1; fi
    sleep 0.1
done
echo "up"

# Nothing of the real user's is visible: no home folder, no sidecar of its
# own, nothing but the connection file.
run() {
    env -i HOME="$WORK/home" PATH="$WORK/venv/bin:/usr/bin:/bin" ORVU_CONNECT="$WORK/alice.orvu" "$@"
}
mkdir "$WORK/home"

echo "--- an agent with only a connection file"
run python -c "
from orvu import Team
with Team.connect() as team:
    team.notes.put('deploy', 'Friday')
    print(team.me, team.notes.get('deploy'))
"

echo "--- the package's orvud command, with no sidecar to run"
run orvud --version >"$WORK/orvud.out" 2>&1 || true
grep -q ORVU_CONNECT "$WORK/orvud.out" || { cat "$WORK/orvud.out"; exit 1; }
echo "it says to connect with ORVU_CONNECT"
