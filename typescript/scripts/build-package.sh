#!/bin/sh
# Builds the npm package with the sidecar inside it, for the kind of machine
# this is run on. Run it from anywhere:
#
#     sh typescript/scripts/build-package.sh
#
# The result is one .tgz file in typescript/release, named for the kind of
# machine, such as orvu-0.1.0-linux-x64.tgz. Install it with
# `npm install path/to/orvu-0.1.0-linux-x64.tgz`.
#
# It needs Rust, protoc, Node and npm. On Linux it also needs
# `cargo zigbuild`, which comes with the Python package's tools for a
# release: `pip install -e "python[release]"`.
#
# With --without-sidecar it builds the package with no sidecar inside, for
# any kind of machine, Windows among them: release/orvu-0.1.0.tgz. Such a
# package connects to a sidecar elsewhere over the network, with a
# connection file. It needs only Node and npm.
set -e
cd "$(dirname "$0")/.."

WITHOUT_SIDECAR=""
if [ "$1" = --without-sidecar ]; then
    WITHOUT_SIDECAR=yes
fi

if [ -z "$WITHOUT_SIDECAR" ]; then
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

# The package is put together in a folder of its own, so that a build that
# stops half way leaves nothing behind beside the code.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R package.json README.md ../LICENSE dist proto "$STAGE"
if [ -n "$WITHOUT_SIDECAR" ]; then
    # It is for Windows only, so that on another machine npm never takes it
    # in place of the package with the sidecar inside. It has no orvud
    # command either: there is no sidecar inside for it to run.
    (cd "$STAGE" && npm pkg delete bin files && npm pkg set "files[]=dist" "files[]=proto" "files[]=LICENSE" "os[]=win32")
    mkdir -p release
    rm -f "release/orvu-$VERSION.tgz"
    (cd "$STAGE" && npm pack --silent >/dev/null)
    mv "$STAGE/orvu-$VERSION.tgz" "release/orvu-$VERSION.tgz"
    ls "release/orvu-$VERSION.tgz"
    exit 0
fi
mkdir "$STAGE/bin"
cp "../target/$TARGET/release/orvud" "$STAGE/bin/orvud"
# The package says which kind of machine it is for, so that npm refuses to
# install it on another kind.
(cd "$STAGE" && npm pkg set "os[]=${MACHINE%-*}" "cpu[]=${MACHINE#*-}")

mkdir -p release
rm -f "release/orvu-$VERSION-$MACHINE.tgz"
(cd "$STAGE" && npm pack --silent >/dev/null)
mv "$STAGE/orvu-$VERSION.tgz" "release/orvu-$VERSION-$MACHINE.tgz"
ls "release/orvu-$VERSION-$MACHINE.tgz"
