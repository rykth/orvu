//! Development setup: creates a certificate authority (CA) and one node
//! certificate per replica.
//! Usage: keygen --out <dir> --replicas 1,2,3
use std::path::PathBuf;

use orvu_transport::{generate_dev_ca, issue_node_cert};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut out_dir: Option<PathBuf> = None;
    let mut replicas: Vec<u64> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                let val = args.get(i + 1).expect("--out requires a value");
                out_dir = Some(PathBuf::from(val));
                i += 2;
            }
            "--replicas" => {
                let val = args.get(i + 1).expect("--replicas requires a value");
                replicas = val
                    .split(',')
                    .map(|s| s.trim().parse::<u64>().expect("replica ids must be integers"))
                    .collect();
                i += 2;
            }
            other => panic!("unrecognized argument: {other}"),
        }
    }

    let out_dir = out_dir.expect("--out <dir> is required");
    assert!(!replicas.is_empty(), "--replicas <ids> is required");

    std::fs::create_dir_all(&out_dir).expect("failed to create output directory");

    let ca = generate_dev_ca();
    let ca_path = out_dir.join("ca.pem");
    std::fs::write(&ca_path, &ca.cert_pem).expect("failed to write ca.pem");
    println!("{}", ca_path.display());

    for replica in replicas {
        let node = issue_node_cert(&ca, replica);

        let cert_path = out_dir.join(format!("node{replica}.pem"));
        std::fs::write(&cert_path, &node.cert_pem).expect("failed to write node cert");
        println!("{}", cert_path.display());

        let key_path = out_dir.join(format!("node{replica}-key.pem"));
        std::fs::write(&key_path, &node.key_pem).expect("failed to write node key");
        println!("{}", key_path.display());
    }
}
