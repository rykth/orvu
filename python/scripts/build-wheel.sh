#!/bin/sh
# Builds the package with the sidecar inside it, for the kind of machine this
# is run on. Run it from anywhere:
#
#     sh python/scripts/build-wheel.sh
#
# The result is one .whl file in python/dist.
#
# With --without-sidecar it builds the package with no sidecar inside, for
# Windows on Intel and AMD processors, 64-bit and 32-bit:
# python/dist/orvu-0.1.0-py3-none-win_amd64.win32.whl. Windows on ARM is
# left out: gRPC, which the package needs, is not ready-built for it.
# Such a package connects to a sidecar elsewhere over the network, with a
# connection file. It is made out to Windows only, so that no other machine
# takes it in place of a package with the sidecar inside. It needs only
# Python and `build`: no Rust.
#
# It needs Rust, protoc, and the tools in the "release" extra:
#
#     pip install -e ".[release]"
set -e
cd "$(dirname "$0")/.."

if [ "$1" = --without-sidecar ]; then
    # A package built before, with the sidecar inside, is left where it is.
    rm -f dist/orvu-*-py3-none-win*.whl
    env -u ORVU_BUNDLE_ORVUD ORVU_WHEEL_PLATFORM=win_amd64.win32 python -m build --wheel
    ls dist/orvu-*-py3-none-win*.whl
    exit 0
fi

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)
        TARGET=x86_64-unknown-linux-gnu
        PLATFORM=manylinux_2_17_x86_64 ;;
    Linux-aarch64)
        TARGET=aarch64-unknown-linux-gnu
        PLATFORM=manylinux_2_17_aarch64 ;;
    Darwin-arm64)
        TARGET=aarch64-apple-darwin
        PLATFORM=macosx_11_0_arm64
        export MACOSX_DEPLOYMENT_TARGET=11.0 ;;
    Darwin-x86_64)
        TARGET=x86_64-apple-darwin
        PLATFORM=macosx_10_12_x86_64
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
    # on any Linux since then. That is what "manylinux_2_17" promises.
    cargo zigbuild --release -p orvud --target "$TARGET.2.17"
else
    cargo build --release -p orvud --target "$TARGET"
fi

# The package for Windows, if one was built before, is left where it is.
for wheel in dist/orvu-*.whl; do
    case "$wheel" in
        *-py3-none-win*) ;;
        *) rm -f "$wheel" ;;
    esac
done
ORVU_BUNDLE_ORVUD="$(cd ../target/"$TARGET"/release && pwd)/orvud" \
ORVU_WHEEL_PLATFORM="$PLATFORM" \
    python -m build --wheel
ls dist
