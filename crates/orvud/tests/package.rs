//! The service code is built from `dataplane.proto`, so a copy of this module
//! on its own, as `cargo package` makes it, has to carry that file. A file
//! found by a path outside the module's folder works in this repository and
//! nowhere else.

use std::process::Command;

#[test]
fn the_packaged_module_carries_the_file_it_is_built_from() {
    let out = Command::new(env!("CARGO"))
        .args(["package", "--list", "--allow-dirty", "--offline", "-p", "orvud"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let listed = String::from_utf8(out.stdout).unwrap();
    let path = "proto/orvu/v1/dataplane.proto";
    assert!(listed.lines().any(|l| l == path), "{path} is not in the package:\n{listed}");
}

#[test]
fn the_build_looks_only_inside_this_module() {
    let build = include_str!("../build.rs");
    assert!(!build.contains(".."), "build.rs reaches outside the module's folder");
}
