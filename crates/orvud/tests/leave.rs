//! `orvud leave`: a machine taken out of the team for good hands its share of
//! every budget to the machines that stay.

use orvu_test_support::free_port;
use orvud::setup::{self, Machine};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_that_leaves_hands_its_budget_to_the_others() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines: Vec<Machine> = ["alpha", "beta"]
        .iter()
        .map(|name| Machine::parse(&format!("{name}=127.0.0.1:{}", free_port())).unwrap())
        .collect();
    setup::write(&folder, &machines).expect("writes");
    let alpha_settings = folder.join("alpha/orvu.toml");

    // Alpha tops a budget up and stops, holding its share of it. It runs in
    // a runtime of its own, which lets go of its network address when it is
    // dropped, as a process that stops does.
    let (beta, _, _, _beta) = orvud::boot::from_toml(&folder.join("beta/orvu.toml")).await.expect("starts");
    let tokens = beta.bounded_counter("tokens").await.unwrap();
    let settings = alpha_settings.clone();
    let held = std::thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (alpha, _, _, _claim) = orvud::boot::from_toml(&settings).await.expect("starts");
            let budget = alpha.bounded_counter("tokens").await.unwrap();
            budget.increment(100).await.unwrap();
            let held = budget.local_allowance().await.unwrap();
            alpha.shutdown().await;
            held
        })
    })
    .join()
    .unwrap();
    assert!(held > 0, "alpha holds none of the budget");

    // The real program, as a person runs it on the machine that goes.
    let alpha_settings_text = alpha_settings.to_str().unwrap().to_string();
    let left = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
            .args(["leave", "--config", &alpha_settings_text, "--wait", "20"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let said = format!("{}{}", String::from_utf8_lossy(&left.stdout), String::from_utf8_lossy(&left.stderr));
    assert!(left.status.success(), "{said}");
    assert!(said.contains("tokens"), "{said}");
    assert_eq!(tokens.local_allowance().await.unwrap(), 100, "{said}");
    assert_eq!(tokens.value().await.unwrap(), 100);
    beta.shutdown().await;
}

// While the sidecar runs, it holds the machine's data, and leaving is
// refused before anything is changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_while_the_sidecar_runs_is_refused() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines: Vec<Machine> = ["alpha", "beta"]
        .iter()
        .map(|name| Machine::parse(&format!("{name}=127.0.0.1:{}", free_port())).unwrap())
        .collect();
    setup::write(&folder, &machines).expect("writes");
    let settings = folder.join("alpha/orvu.toml");
    let (alpha, _, _, _claim) = orvud::boot::from_toml(&settings).await.expect("starts");
    let refused = orvud::boot::leave(&settings, Duration::from_secs(1)).await.expect_err("left").to_string();
    assert!(refused.contains("already using"), "{refused}");
    alpha.shutdown().await;
}

// A machine that has never run holds nothing to hand over.
#[tokio::test]
async fn a_machine_that_never_ran_has_nothing_to_hand_over() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines = [Machine::parse("alpha=127.0.0.1:1").unwrap(), Machine::parse("beta=127.0.0.1:2").unwrap()];
    setup::write(&folder, &machines).expect("writes");
    let refused =
        orvud::boot::leave(&folder.join("alpha/orvu.toml"), Duration::from_secs(1)).await.expect_err("left").to_string();
    assert!(refused.contains("never"), "{refused}");
    assert!(!folder.join("alpha/data/replica").exists(), "it wrote something");
}

// The list of machines that have left is the machines' own. A program
// connected to the sidecar, even one held to no rules, may not mark a
// machine as gone, make the list as another kind of object so that leaving
// fails, or watch it.
#[tokio::test]
async fn no_program_may_touch_the_list_of_machines_that_have_left() {
    use orvu_runtime::leave::LEFT;
    use orvud::pb::{self, request::Op};
    let fabric = std::sync::Arc::new(orvu_transport::mem::MemTransport::new());
    let cfg = orvu_runtime::NodeConfig::new(orvu_crdt::ReplicaId(1), "ap-1");
    let client = std::sync::Arc::new(orvu_sdk::Client::start(fabric.endpoint(1), cfg).await.expect("engine"));
    let dp = orvud::server::Dataplane::new(client.clone(), orvu_crdt::ReplicaId(1), "default");
    let session = dp
        .open_for_test(pb::OpenRequest { namespace: "default".into(), resume_token: String::new(), credential: Vec::new() })
        .await
        .expect("open")
        .session;
    let request = |op: Op| pb::Request { session: session.clone(), op: Some(op), request_id: Vec::new() };
    for (object, op) in [
        (LEFT, Op::SetAdd(pb::SetAdd { object: LEFT.into(), member: "2".into() })),
        (LEFT, Op::Declare(pb::Declare { object: LEFT.into(), object_class: pb::Class::Ap as i32 })),
        (LEFT, Op::CounterIncrement(pb::CounterIncrement { object: LEFT.into(), n: 1 })),
        (LEFT, Op::SetElements(pb::SetElements { object: LEFT.into() })),
        ("orvu/_machines/other", Op::RegisterSet(pb::RegisterSet { object: "orvu/_machines/other".into(), value: "x".into() })),
    ] {
        let refused = dp.execute_for_test(request(op.clone())).await.expect_err("was let through");
        assert_eq!(refused.code(), tonic::Code::PermissionDenied, "{refused:?}");
        assert!(refused.message().contains(object) && refused.message().contains("machines"), "{refused:?}");
        let batch = pb::BatchRequest {
            session: session.clone(),
            requests: vec![request(Op::CounterIncrement(pb::CounterIncrement { object: "fine".into(), n: 1 })), request(op)],
            request_id: Vec::new(),
        };
        let refused = dp.batch_for_test(batch).await.expect_err("the batch was let through");
        assert_eq!(refused.code(), tonic::Code::PermissionDenied, "{refused:?}");
    }
    let watch = pb::WatchRequest { session: session.clone(), object: LEFT.into(), kind: pb::Kind::Set as i32 };
    let refused = dp.watch_for_test(watch).await.err().expect("the watch was let through");
    assert_eq!(refused.code(), tonic::Code::PermissionDenied, "{refused:?}");
    assert!(!client.exists(LEFT).await, "the list was made");
    assert!(!client.exists("fine").await, "part of a refused batch was run");
    // Other names are served as before.
    let fine = dp.execute_for_test(request(Op::SetAdd(pb::SetAdd { object: "orvu/machines/left".into(), member: "2".into() }))).await;
    assert!(fine.is_ok(), "{fine:?}");
    client.shutdown().await;
}
