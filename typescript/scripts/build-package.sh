#!/bin/sh
# Builds the npm packages. Run it from anywhere:
#
#     sh typescript/scripts/build-package.sh
#
# There are two kinds of package. `@orvuhq/orvu` is the library, the same on every
# machine, with no sidecar inside. The sidecar program comes in a package of
# its own for each kind of machine, such as @orvuhq/orvud-linux-x64, and
# `@orvuhq/orvu` names all four as optional, so that npm installs only the one that
# fits the machine it runs on. On Windows none fits, and the library connects
# to a sidecar elsewhere over the network instead.
#
# This builds, in typescript/release, the library and the sidecar package for
# the kind of machine it is run on, such as:
#
#     release/orvuhq-orvu-0.1.0.tgz
#     release/orvuhq-orvud-linux-x64-0.1.0.tgz
#
# Install the two together: `npm install` with both files.
#
# It needs Rust, protoc, Node and npm. On Linux it also needs
# `cargo zigbuild`, which comes with the Python package's tools for a
# release: `pip install -e "python[release]"`.
#
# With --main-only it builds only the library, release/orvuhq-orvu-0.1.0.tgz, and
# needs only Node and npm.
set -e
cd "$(dirname "$0")/.."

MAIN_ONLY=""
case "$1" in
    "") ;;
    --main-only) MAIN_ONLY=yes ;;
    *)
        echo "usage: build-package.sh [--main-only]" >&2
        exit 2 ;;
esac

if [ -z "$MAIN_ONLY" ]; then
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        TARGET=x86_64-unknown-linux-gnu
        MACHINE=linux-x64 ;;
    Linux-aarch64)
        TARGET=aarch64-unknown-linux-gnu
        MACHINE=linux-arm64 ;;
    Darwin-arm64)
        TARGET=aarch64-apple-darwin
        MACHINE=darwin-arm64
        export MACOSX_DEPLOYMENT_TARGET=11.0 ;;
    Darwin-x86_64)
        TARGET=x86_64-apple-darwin
        MACHINE=darwin-x64
        export MACOSX_DEPLOYMENT_TARGET=10.12 ;;
    *)
        echo "there is no sidecar for $(uname -s) on $(uname -m)" >&2
        exit 1 ;;
esac

# Debugging symbols are left out. They make the program several times larger.
export CARGO_PROFILE_RELEASE_STRIP=symbols

if [ "$(uname -s)" = Linux ]; then
    # A program built here would need a C library as new as this machine's.
    # zigbuild links against an old one (2.17, from 2012), so the result runs
    # on any Linux since then.
    cargo zigbuild --release -p orvud --target "$TARGET.2.17"
else
    cargo build --release -p orvud --target "$TARGET"
fi
fi

VERSION="$(node -p 'require("./package.json").version')"
npm ci --silent
rm -rf dist
npm run -s build

# Each package is put together in a folder of its own, so that a build that
# stops half way leaves nothing behind beside the code.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p release

# The library. Only the copy that is published names the sidecar packages,
# each at exactly its own version. The package.json in this folder does not,
# so that `npm install` here, for work on the package, never asks for them.
mkdir "$STAGE/main"
cp -R package.json README.md ../LICENSE dist proto "$STAGE/main"
(cd "$STAGE/main" && npm pkg set \
    "optionalDependencies.@orvuhq/orvud-linux-x64=$VERSION" \
    "optionalDependencies.@orvuhq/orvud-linux-arm64=$VERSION" \
    "optionalDependencies.@orvuhq/orvud-darwin-arm64=$VERSION" \
    "optionalDependencies.@orvuhq/orvud-darwin-x64=$VERSION")
rm -f "release/orvuhq-orvu-$VERSION.tgz"
(cd "$STAGE/main" && npm pack --silent >/dev/null)
mv "$STAGE/main/orvuhq-orvu-$VERSION.tgz" "release/orvuhq-orvu-$VERSION.tgz"
ls "release/orvuhq-orvu-$VERSION.tgz"

if [ -n "$MAIN_ONLY" ]; then
    exit 0
fi

# The sidecar package for this kind of machine: the program, the licence and
# a few lines saying what it is. It says which kind of machine it is for, so
# that npm installs it there and nowhere else.
OS="${MACHINE%-*}"
CPU="${MACHINE#*-}"
SIDECAR="$STAGE/sidecar"
mkdir -p "$SIDECAR/bin"
cp "../target/$TARGET/release/orvud" "$SIDECAR/bin/orvud"
chmod 755 "$SIDECAR/bin/orvud"
cp ../LICENSE "$SIDECAR"
cat > "$SIDECAR/README.md" <<EOF
# @orvuhq/orvud-$MACHINE

The \`orvud\` program of Orvu, for $OS on $CPU. There is no need to install
it by hand: \`npm install @orvuhq/orvu\` brings it on the kind of machine it is for,
and the \`orvu\` package finds it and starts it when it is needed.
EOF
cat > "$SIDECAR/package.json" <<EOF
{
  "name": "@orvuhq/orvud-$MACHINE",
  "version": "$VERSION",
  "description": "The orvud program of Orvu, for $OS on $CPU",
  "license": "Apache-2.0",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/rykth/orvu.git",
    "directory": "typescript"
  },
  "os": ["$OS"],
  "cpu": ["$CPU"],
  "files": ["bin/orvud", "LICENSE", "README.md"]
}
EOF
rm -f "release/orvuhq-orvud-$MACHINE-$VERSION.tgz"
(cd "$SIDECAR" && npm pack --silent >/dev/null)
mv "$SIDECAR/orvuhq-orvud-$MACHINE-$VERSION.tgz" "release/orvuhq-orvud-$MACHINE-$VERSION.tgz"
ls "release/orvuhq-orvud-$MACHINE-$VERSION.tgz"
