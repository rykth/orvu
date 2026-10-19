# syntax=docker/dockerfile:1.7
#
# Everything the demo runs, built from this repository. Build it from the top
# of the repository: compose.yaml does that.
#
#   orvud-build   builds the sidecar
#   model         fetches the model search by meaning uses, and checks it
#   machine       a machine of the team: the sidecar and nothing else
#   python        the Python package, for the Python agents, the MCP server
#                 and the story
#   node-build    builds the TypeScript package
#   node          the TypeScript package, for the TypeScript agents

FROM rust:1.97-slim-bookworm AS orvud-build
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler g++ \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
ENV CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_STRIP=symbols
RUN --mount=type=cache,id=orvu-demo-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=orvu-demo-cargo-target,target=/src/target \
    cargo build --release --locked -p orvud \
    && cp target/release/orvud /orvud

FROM python:3.14-slim AS model
# The fixed version of the model the sidecar would fetch by itself, checked
# against the same SHA-256 sums as the sidecar's README gives. Fetched here,
# once, so that the machines need no internet.
ARG MODEL=https://huggingface.co/minishlab/potion-base-8M/resolve/bf8b056651a2c21b8d2565580b8569da283cab23
RUN mkdir /model && cd /model \
    && python -c "import sys, urllib.request; [urllib.request.urlretrieve(f'$MODEL/{f}', f) for f in sys.argv[1:]]" \
        config.json tokenizer.json model.safetensors \
    && printf '%s  %s\n' \
        2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553 config.json \
        e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50 tokenizer.json \
        f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2 model.safetensors \
        | sha256sum -c -

FROM debian:bookworm-slim AS machine
LABEL com.orvu.demo=1
RUN useradd --uid 10001 --create-home orvu
COPY --from=orvud-build /orvud /usr/local/bin/orvud
COPY --from=model /model /opt/orvu/model
COPY examples/compose/machine.sh examples/compose/setup.sh /usr/local/bin/
ENV ORVU_MODEL=/opt/orvu/model
USER orvu

FROM python:3.14-slim AS python
LABEL com.orvu.demo=1
RUN useradd --uid 10001 --create-home orvu
# The package with the extra that MCP over HTTP needs. It is installed from
# the repository, as plain Python: the sidecar it would start by itself is
# not put inside, as every program here is given a machine's socket.
RUN --mount=type=bind,source=python,target=/src/python,rw \
    --mount=type=cache,id=orvu-demo-pip,target=/root/.cache/pip \
    pip install --root-user-action=ignore "/src/python[http]"
# `orvud audit` reads a machine's record of who changed what. The package's
# own `orvud` command runs the program named here.
COPY --from=orvud-build /orvud /opt/orvu/bin/orvud
ENV ORVUD_BIN=/opt/orvu/bin/orvud \
    PYTHONUNBUFFERED=1
COPY examples/compose/agents/agent.py examples/compose/agents/story.py \
     examples/compose/agents/mcp_client.py examples/compose/agents/scout.sh /app/
WORKDIR /app
USER orvu

FROM node:24-slim AS node-build
WORKDIR /src/typescript
COPY typescript/package.json typescript/package-lock.json ./
RUN --mount=type=cache,id=orvu-demo-npm,target=/root/.npm npm ci --no-audit --no-fund
COPY typescript/tsconfig.json typescript/README.md ./
COPY typescript/src src
COPY typescript/proto proto
# The package without the sidecar inside: the agents are given a machine's
# socket, and never start a sidecar of their own.
RUN npm run -s build \
    && npm pkg delete bin \
    && mkdir /out \
    && npm pack --silent --pack-destination /out

FROM node:24-slim AS node
LABEL com.orvu.demo=1
WORKDIR /app
RUN --mount=type=bind,from=node-build,source=/out,target=/pkg \
    --mount=type=cache,id=orvu-demo-npm,target=/root/.npm \
    npm install --omit=dev --no-audit --no-fund /pkg/orvu-0.1.0.tgz
COPY examples/compose/agents/agent.ts /app/
RUN useradd --uid 10001 --create-home orvu
USER orvu
