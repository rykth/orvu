#!/bin/sh
# scout is an agent that has no code of its own: it is the MCP server, served
# over HTTP with a token on machine m1, for an agent program elsewhere. In
# the demo the program is the story's small scripted client, not a model.
set -eu
ORVU_KEY=$(cat /keys/scout.key)
export ORVU_KEY
exec orvu mcp --team crew --socket /sock/orvud.sock \
    --http 0.0.0.0:8765 --token-file /keys/scout.token
