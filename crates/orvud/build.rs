//! Generates the gRPC service code from `proto/orvu/v1/dataplane.proto`, in
//! this module's own folder, so that a copy of the module on its own still has
//! the file.
//!
//! This runs in `orvud` and not in `orvu-proto` because `orvu-core` depends on
//! `orvu-proto`. A program that embeds the engine should not have to pull in
//! gRPC just because the sidecar exists.

fn main() {
    let proto = "proto/orvu/v1/dataplane.proto";
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &["proto"])
        .expect("failed to compile dataplane.proto (is protoc on PATH?)");
    println!("cargo:rerun-if-changed={proto}");
}
