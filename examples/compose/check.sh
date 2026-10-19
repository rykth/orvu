#!/bin/sh
# Runs the whole demo with no one watching, and exits with the story's exit
# code: 0 when every check passed. It starts from nothing and cleans up
# after itself, removing the demo's containers, network and volumes. The
# images are kept, so a second run does not build them again.
#
#     sh examples/compose/check.sh [seconds]
#
# The run is stopped after that many seconds, 1200 unless given. When it
# fails, the last lines of every container's log are printed.
set -u
here=$(cd "$(dirname "$0")" && pwd)
limit=${1:-1200}
compose() { docker compose -f "$here/compose.yaml" "$@"; }

compose down -v --remove-orphans >/dev/null 2>&1
timeout --foreground "$limit" \
    docker compose -f "$here/compose.yaml" up --build --abort-on-container-exit --exit-code-from story
code=$?
if [ "$code" -eq 124 ]; then
    echo "check.sh: the demo did not finish within $limit seconds" >&2
fi
if [ "$code" -ne 0 ]; then
    compose logs --no-color --tail 40 >&2
fi
compose down -v --remove-orphans >/dev/null 2>&1
exit "$code"
