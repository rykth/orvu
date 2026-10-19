//! Turns the message formats in this module's own `proto` folder into Rust
//! types when the module is built. The files live inside the module so that a
//! copy of it on its own still has them.
//!
//! Maps become `BTreeMap`, which keeps its keys sorted, so this module always
//! writes a map's entries in the same order. A `HashMap` has no fixed order,
//! and tests that compare encoded bytes would pass or fail at random.

fn main() {
    let protos = [
        "proto/orvu/v1/core.proto",
        "proto/orvu/v1/crdt.proto",
        "proto/orvu/v1/sync.proto",
        "proto/orvu/v1/snapshot.proto",
    ];

    prost_build::Config::new()
        .btree_map(["."])
        .compile_protos(&protos, &["proto"])
        .expect("failed to compile orvu protos (is protoc on PATH?)");

    for proto in protos {
        println!("cargo:rerun-if-changed={proto}");
    }
}
