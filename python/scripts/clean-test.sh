#!/bin/sh
# Installs a built package into an empty environment, with an empty home
# folder, and runs the quickstart, the two demos, the MCP server and a team
# on two machines. This is what a new user's first commands do, so it is the
# test that the package works by itself.
#
#     sh python/scripts/clean-test.sh python/dist/orvu-*.whl [python]
#
# The second argument is the Python to use. It is `python3` if left out.
set -e
WHEEL="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
PYTHON="${2:-python3}"
EXAMPLES="$(cd "$(dirname "$0")/../examples" && pwd)"
TESTS="$(cd "$(dirname "$0")/../tests" && pwd)"

# A short path, because the sidecar's socket goes inside it.
WORK="$(mktemp -d /tmp/orvu-clean-XXXXXX)"
stop() {
    # Whatever sidecar was started for this home folder is stopped again.
    # Where there is no pkill, it stops by itself once it has been idle.
    if command -v pkill >/dev/null; then
        pkill -f "orvud.*$WORK" || true
    fi
    rm -rf "$WORK"
}
trap stop EXIT

"$PYTHON" -m venv "$WORK/venv"
"$WORK/venv/bin/pip" install --quiet "$WHEEL"

# Nothing of the real user's is visible: no home folder, no running sidecar,
# no orvud on the PATH except the one that came with the package.
run() {
    env -i HOME="$WORK/home" PATH="$WORK/venv/bin:/usr/bin:/bin" "$@"
}
mkdir "$WORK/home"

echo "--- the sidecar that came with the package"
run orvud --version

echo "--- the quickstart, twice"
run python "$EXAMPLES/quickstart.py"
run python "$EXAMPLES/quickstart.py" | tee "$WORK/second"
grep -q "run 2 time" "$WORK/second"

echo "--- the demo: three agents, one task list, one budget"
run python -m orvu.demo | tee "$WORK/demo"
grep -q "^tokens left: " "$WORK/demo"

echo "--- the demo on three machines, one of them switched off and on again"
run python -m orvu.demo_machines | tee "$WORK/machines"
grep -q "^the three machines hold the same board and the same budget: yes" "$WORK/machines"
grep -q "^the budget was overspent: no" "$WORK/machines"

echo "--- the tools an agent gets over MCP"
printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{}}}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"note_put","arguments":{"name":"from","text":"the clean test"}}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"note_list"}}' \
    | run orvu mcp --me clean-test | tee "$WORK/mcp"
test "$(wc -l < "$WORK/mcp")" -eq 3
grep -q '"serverInfo": {"name": "orvu"' "$WORK/mcp"
grep -q 'the clean test' "$WORK/mcp"

echo "--- one team on two machines, set up with the sidecar's own command"
run python - "$WORK/crew" "$TESTS" <<'EOF'
import pathlib, subprocess, sys, time
from orvu import Team

# The tests' own way of picking a port that nothing else is given.
sys.path.insert(0, sys.argv[2])
import ports

crew = pathlib.Path(sys.argv[1])
free_port = ports.free_port
names = ["alpha", "beta"]
subprocess.run(["orvud", "setup", crew, *(f"{name}=127.0.0.1:{free_port()}" for name in names)], check=True)
running = [
    subprocess.Popen(["orvud", "--config", "orvu.toml", "--socket", "orvud.sock"], cwd=crew / name)
    for name in names
]
try:
    def team(name):
        deadline = time.monotonic() + 10
        while not (crew / name / "orvud.sock").exists():
            assert time.monotonic() < deadline, f"{name} did not start"
            time.sleep(0.05)
        return Team.connect("research", socket=crew / name / "orvud.sock", me=name)
    alpha, beta = team("alpha"), team("beta")
    alpha.notes.put("from", "the other machine")
    deadline = time.monotonic() + 10
    while beta.notes.get("from") != "the other machine":
        assert time.monotonic() < deadline, "the note did not reach the second machine"
        time.sleep(0.05)
    print("beta read what alpha wrote:", beta.notes.get("from"))
finally:
    for sidecar in running:
        sidecar.terminate()
        sidecar.wait()
EOF

echo "--- every run shared one sidecar, started from inside the package"
# The sidecar prints one line each time it starts, and that goes into a file
# in its data folder.
LOG="$(find "$WORK/home" -name orvud.log)"
test "$(grep -c "serving on" "$LOG")" -eq 1
run python -c "from orvu import _sidecar; print(_sidecar.program())" | tee "$WORK/program"
grep -q "^$WORK/venv/.*/orvu/_bin/orvud$" "$WORK/program"

echo "clean test passed"
