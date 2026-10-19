#!/bin/sh
# Rebuilds the Python code for the sidecar's messages from dataplane.proto.
# Run it from the `python` folder after the .proto file changes:
#
#     sh scripts/generate.sh
#
# The two files it writes are kept in the repository, so that using the
# package does not need the code generator.
set -e
cd "$(dirname "$0")/.."
OUT=src/orvu/_proto
python -m grpc_tools.protoc \
    --proto_path=../crates/orvud/proto \
    --python_out="$OUT" \
    --grpc_python_out="$OUT" \
    orvu/v1/dataplane.proto
# The generator nests its output in folders named after the proto package.
# Flatten it, and make the one import between the two files a relative one so
# that it works inside this package.
mv "$OUT/orvu/v1/dataplane_pb2.py" "$OUT/dataplane_pb2.py"
mv "$OUT/orvu/v1/dataplane_pb2_grpc.py" "$OUT/dataplane_pb2_grpc.py"
rm -r "$OUT/orvu"
sed -i.bak 's/^from orvu\.v1 import dataplane_pb2 as /from . import dataplane_pb2 as /' "$OUT/dataplane_pb2_grpc.py"
rm "$OUT/dataplane_pb2_grpc.py.bak"
