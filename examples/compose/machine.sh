#!/bin/sh
# Runs the sidecar of one machine of the team, from its folder.
#
# The story switches a machine off and on again part of the way through. It
# asks by putting a file `<machine>.off` in the shared folder /ctl, and this
# script stops the sidecar, the way a service manager would, until the file
# is gone. It says how the machine is in `<machine>.state`: "on" once the
# sidecar serves its socket, and "off" once it has stopped.
#
# A sidecar that stops when nobody asked it to ends the container with a
# failure, so the demo cannot carry on as if nothing happened.
set -u
name=$1
cd "/team/crew/$name"

state() {
    printf '%s\n' "$1" >"/ctl/$name.state.new"
    mv "/ctl/$name.state.new" "/ctl/$name.state"
}

pid=
stop() {
    if [ -n "$pid" ]; then
        kill -TERM "$pid" 2>/dev/null
        wait "$pid"
    fi
    exit 0
}
trap stop TERM INT

while :; do
    while [ -e "/ctl/$name.off" ]; do sleep 0.2; done
    orvud --config orvu.toml --socket /sock/orvud.sock &
    pid=$!
    while [ ! -S /sock/orvud.sock ]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "the sidecar of $name did not start" >&2
            exit 1
        fi
        sleep 0.1
    done
    state on
    while kill -0 "$pid" 2>/dev/null; do
        if [ -e "/ctl/$name.off" ]; then
            kill -TERM "$pid"
            wait "$pid"
            pid=
            state off
            break
        fi
        sleep 0.2
    done
    if [ -n "$pid" ]; then
        echo "the sidecar of $name stopped by itself" >&2
        exit 1
    fi
done
