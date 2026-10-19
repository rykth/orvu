#!/bin/sh
# Makes the team, once, before any machine starts: the settings and
# certificates of three machines, a key for each agent on the machine it
# runs on, rules for dave, and the token of the MCP server. It all goes in
# the demo's own volumes, and goes with them on `docker compose down -v`.
#
# It runs as root only to hand the volumes to the user everything else runs
# as. Then it stays, so that the machines can see it has finished.
set -eu

# What an earlier run left behind is not used again: the story starts from
# an empty team.
rm -rf /team/crew /team/ready /keys/* /ctl/*

# The machines reach each other by their names on the demo's network.
orvud setup /team/crew m1=m1 m2=m2 m3=m3

# Gives an agent a key on one machine, and keeps the key where the agent's
# container can read it.
key() {
    machine=$1 name=$2 file=$3
    orvud agent add "$name" --config "/team/crew/$machine/orvu.toml" \
        | grep -o 'orvu-key-[0-9a-f]*' | head -n 1 >"/keys/$file"
    test -s "/keys/$file"
}

key m1 alice alice.key
key m2 bob bob.key
key m3 carol carol.key
key m2 dave dave.key
key m1 scout scout.key
for machine in m1 m2 m3; do
    key "$machine" referee "referee-$machine.key"
done

# dave reads every note and task but changes only his own, and may spend
# from a budget but not add to one.
orvud agent allow dave notes:own tasks:own budgets:spend inbox:write roster:write \
    documents:read tallies:write --config /team/crew/m2/orvu.toml

# The token programs give the MCP server over HTTP.
head -c 32 /dev/urandom | base64 >/keys/scout.token

chown -R 10001:10001 /team /keys /ctl /sock
chmod 700 /keys
chmod 600 /keys/*
touch /team/ready
echo "the team is made: three machines, six agents"
exec sleep infinity
