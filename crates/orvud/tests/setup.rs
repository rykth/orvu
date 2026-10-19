//! `orvud setup` end to end: the folders it makes are started from, and the
//! machines find each other.

use orvud::setup::{self, Machine};
use std::path::Path;
use std::time::Duration;

use orvu_test_support::free_port;

/// Three machines on this computer, told apart by their ports.
fn three(host: &str) -> Vec<Machine> {
    ["alpha", "beta", "gamma"]
        .iter()
        .map(|name| Machine::parse(&format!("{name}={host}:{}", free_port())).unwrap())
        .collect()
}

async fn until(mut check: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..500 {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[test]
fn each_machine_gets_a_folder_with_its_own_key_and_the_shared_authority() {
    use std::os::unix::fs::PermissionsExt;
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines = three("127.0.0.1");
    setup::write(&folder, &machines).expect("writes");

    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let read = |machine: &str, file: &str| std::fs::read_to_string(folder.join(machine).join(file)).unwrap();
    assert_eq!(mode(&folder), 0o700);
    for machine in ["alpha", "beta", "gamma"] {
        assert_eq!(mode(&folder.join(machine)), 0o700);
        assert_eq!(mode(&folder.join(machine).join("node-key.pem")), 0o600, "the key of {machine} can be read by others");
        let mut files: Vec<String> = std::fs::read_dir(folder.join(machine))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        let unit = format!("orvud-{machine}.service");
        assert_eq!(files, ["ca.pem", "node-key.pem", "node.pem", "orvu.toml", unit.as_str()]);
        assert_eq!(read(machine, "ca.pem"), read("alpha", "ca.pem"), "the machines do not share an authority");
        assert!(!read(machine, "ca.pem").contains("PRIVATE KEY"), "the authority's key went to {machine}");
    }
    assert_ne!(read("alpha", "node-key.pem"), read("beta", "node-key.pem"));
    // The authority's key is kept beside the machines' folders, for adding a
    // machine later, where only its owner can read it.
    let authority = folder.join("authority");
    assert_eq!(mode(&authority), 0o700);
    assert_eq!(mode(&authority.join("ca-key.pem")), 0o600);
    assert!(std::fs::read_to_string(authority.join("ca-key.pem")).unwrap().contains("PRIVATE KEY"));
    assert_eq!(std::fs::read_to_string(authority.join("ca.pem")).unwrap(), read("alpha", "ca.pem"));
}

/// The value of `peers` in a settings file.
fn peers_of(settings: &Path) -> Vec<String> {
    orvu_sdk::SdkConfig::parse_toml(&std::fs::read_to_string(settings).unwrap()).unwrap().peers
}

// A machine added to a team that is already running is trusted by the
// others, which do not have to be started again: it reaches them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_added_later_joins_the_running_team() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines = three("127.0.0.1");
    setup::write(&folder, &machines[..2]).expect("writes");
    let (alpha, _, _, _alpha) = orvud::boot::from_toml(&folder.join("alpha/orvu.toml")).await.expect("starts");
    let (beta, _, _, _beta) = orvud::boot::from_toml(&folder.join("beta/orvu.toml")).await.expect("starts");
    alpha.counter("visits").await.unwrap().increment(5).await.unwrap();

    let added = setup::add(&folder, machines[2].clone()).expect("adds");
    assert_eq!(added.number, 3);
    let gamma_settings = folder.join("gamma/orvu.toml");
    assert_eq!(orvud::boot::replica_of(&std::fs::read_to_string(&gamma_settings).unwrap()).unwrap().0, 3);
    assert_eq!(peers_of(&gamma_settings), [machines[0].address(), machines[1].address()]);
    // The others' settings now list gamma too, and say nothing else new.
    assert_eq!(peers_of(&folder.join("alpha/orvu.toml")), [machines[1].address(), machines[2].address()]);
    assert_eq!(peers_of(&folder.join("beta/orvu.toml")), [machines[0].address(), machines[2].address()]);
    assert!(folder.join("gamma/orvud-gamma.service").exists());

    let (gamma, replica, _, _gamma) = orvud::boot::from_toml(&gamma_settings).await.expect("starts");
    assert_eq!(replica.0, 3);
    let counter = gamma.counter("visits").await.unwrap();
    assert!(until(async || counter.value().await.unwrap() == 5).await, "gamma was not let in");
    for client in [alpha, beta, gamma] {
        client.shutdown().await;
    }
}

#[test]
fn a_machine_cannot_be_added_twice_or_to_a_setup_without_its_key() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines = three("127.0.0.1");
    setup::write(&folder, &machines[..2]).expect("writes");
    let again = setup::add(&folder, Machine::parse("alpha=10.0.0.9").unwrap()).unwrap_err();
    assert!(again.contains("two machines are called"), "{again}");
    let same_place = setup::add(&folder, Machine::parse(&format!("delta={}", machines[0].address())).unwrap());
    assert!(same_place.unwrap_err().contains("the same address"));
    let named = setup::add(&folder, Machine::parse("authority=10.0.0.9").unwrap()).unwrap_err();
    assert!(named.contains("authority"), "{named}");

    // A number a machine had before stays its own.
    let taken = setup::add(&folder, Machine::parse("delta=10.0.0.9#2").unwrap()).unwrap_err();
    assert!(taken.contains("both have the number 2"), "{taken}");
    assert_eq!(setup::add(&folder, Machine::parse("delta=10.0.0.9#7").unwrap()).unwrap().number, 7);
    assert_eq!(setup::add(&folder, machines[2].clone()).unwrap().number, 8);

    // Settings made before the key was kept cannot be added to.
    std::fs::remove_dir_all(folder.join("authority")).unwrap();
    let refused = setup::add(&folder, Machine::parse("zeta=10.0.0.10").unwrap()).unwrap_err();
    assert!(refused.contains("authority"), "{refused}");
    assert!(!folder.join("zeta").exists());
}

#[test]
fn a_folder_with_something_in_it_is_left_alone() {
    let top = tempfile::tempdir().unwrap();
    std::fs::write(top.path().join("notes.txt"), "mine").unwrap();
    let refused = setup::write(top.path(), &three("127.0.0.1")).unwrap_err();
    assert!(refused.contains("is not empty"), "{refused}");
    assert_eq!(std::fs::read_dir(top.path()).unwrap().count(), 1, "something was written anyway");

    // An empty folder that is already there is fine.
    let empty = tempfile::tempdir().unwrap();
    setup::write(empty.path(), &three("127.0.0.1")).expect("writes into an empty folder");
}

#[test]
fn a_team_of_one_is_refused_before_anything_is_written() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let refused = setup::write(&folder, &three("127.0.0.1")[..1]).unwrap_err();
    assert!(refused.contains("at least two"), "{refused}");
    assert!(!folder.exists());
}

/// Starts the three machines from the folders and checks that a change made
/// on one reaches the other two. The machines are given by `host`.
async fn the_machines_share(host: &str) {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines = three(host);
    setup::write(&folder, &machines).expect("writes");

    let mut clients = Vec::new();
    for (number, machine) in machines.iter().enumerate() {
        let (client, replica, namespace, claim) =
            orvud::boot::from_toml(&folder.join(&machine.name).join("orvu.toml")).await.expect("starts");
        assert_eq!((replica.0, namespace.as_str()), (number as u64 + 1, "team"));
        clients.push((client, claim));
    }
    clients[0].0.counter("visits").await.unwrap().increment(5).await.unwrap();
    for (client, _) in &clients[1..] {
        let counter = client.counter("visits").await.unwrap();
        assert!(
            until(async || counter.value().await.unwrap() == 5).await,
            "a change on alpha did not reach every machine given as {host}"
        );
    }
    // Each keeps its data in its own folder.
    for machine in &machines {
        assert!(folder.join(&machine.name).join("data/orvu.redb").exists());
    }
    for (client, _claim) in clients {
        client.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machines_started_from_the_folders_find_each_other() {
    the_machines_share("127.0.0.1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn machines_given_by_name_find_each_other() {
    the_machines_share("localhost").await;
}

// Beta is taken out of the team. Gamma is given its old number, starts as
// machine 3, and its certificate is the one for machine 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_keeps_its_number_when_another_is_taken_out() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines = [
        Machine::parse(&format!("alpha=127.0.0.1:{}", free_port())).unwrap(),
        Machine::parse(&format!("gamma=127.0.0.1:{}#3", free_port())).unwrap(),
    ];
    setup::write(&folder, &machines).expect("writes");
    let (alpha, first, _, _alpha) = orvud::boot::from_toml(&folder.join("alpha/orvu.toml")).await.expect("starts");
    let (gamma, third, _, _gamma) = orvud::boot::from_toml(&folder.join("gamma/orvu.toml")).await.expect("starts");
    assert_eq!((first.0, third.0), (1, 3));
    alpha.counter("visits").await.unwrap().increment(5).await.unwrap();
    let counter = gamma.counter("visits").await.unwrap();
    assert!(until(async || counter.value().await.unwrap() == 5).await, "the two did not connect");
    alpha.shutdown().await;
    gamma.shutdown().await;
}

// The real program, run the way a person runs it.
#[test]
fn the_command_makes_the_folders_and_says_what_to_do_next() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_orvud")).arg("setup").args(args).output().unwrap()
    };
    let done = run(&[folder.to_str().unwrap(), "alpha=10.0.0.5", "beta=box.example.com:9000"]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    let said = String::from_utf8(done.stdout).unwrap();
    assert!(said.contains("Made the settings for 2 machines"), "{said}");
    assert!(said.contains("orvud --config orvu.toml --socket orvud.sock"), "{said}");
    assert!(folder.join("alpha/orvu.toml").exists() && folder.join("beta/node-key.pem").exists());

    // A machine added later.
    let added = std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
        .args(["setup", folder.to_str().unwrap(), "--add", "gamma=10.0.0.7"])
        .output()
        .unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let said = String::from_utf8(added.stdout).unwrap();
    assert!(said.contains("machine 3"), "{said}");
    assert!(said.contains(r#"peers = ["box.example.com:9000", "10.0.0.7:7440"]"#), "{said}");
    assert!(folder.join("gamma/node-key.pem").exists());

    // A second run does not replace the keys of the first.
    let again = run(&[folder.to_str().unwrap(), "alpha=10.0.0.5", "beta=10.0.0.6"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("is not empty"));

    // Made again from the first, without gamma. The steps name the new
    // numbers, and the command to make it again goes on from this folder.
    let second = top.path().join("crew2");
    let from = [second.to_str().unwrap(), "--from", folder.to_str().unwrap(), "alpha=10.0.0.5#1", "delta=10.0.0.8"];
    let made = run(&from);
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let said = String::from_utf8(made.stdout).unwrap();
    assert!(said.contains("crew2/delta  machine 4"), "{said}");
    assert!(said.contains(&format!("--from {} alpha=10.0.0.5#1 delta=10.0.0.8#4", second.display())), "{said}");
}

// A name that does not resolve here is warned about, and the settings are
// made all the same: the machines may know names this computer does not.
#[test]
fn a_name_that_does_not_resolve_is_warned_about() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let done = std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
        .args(["setup", folder.to_str().unwrap(), "alpha=127.0.0.1", "beta=nothing-here.invalid"])
        .output()
        .unwrap();
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    let warned = String::from_utf8_lossy(&done.stderr);
    assert!(warned.contains("nothing-here.invalid") && warned.contains("beta"), "{warned}");
    assert!(!warned.contains("127.0.0.1"), "{warned}");
    assert!(folder.join("beta/orvu.toml").exists());
}

// Machines added at the same moment, from several programs, each get a
// number of their own, and none of them drops out of the list.
#[test]
fn machines_added_at_the_same_moment_get_numbers_of_their_own() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    setup::write(&folder, &three("127.0.0.1")[..2]).expect("writes");
    let names: Vec<String> = (0..8).map(|i| format!("m{i}")).collect();
    let start = std::sync::Arc::new(std::sync::Barrier::new(names.len()));
    let adding: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let (folder, start) = (folder.clone(), start.clone());
            let machine = Machine::parse(&format!("{name}=10.0.1.{}", i + 1)).unwrap();
            std::thread::spawn(move || {
                start.wait();
                setup::add(&folder, machine).expect("adds").number
            })
        })
        .collect();
    let mut numbers: Vec<u64> = adding.into_iter().map(|one| one.join().unwrap()).collect();
    numbers.sort_unstable();
    assert_eq!(numbers, (3..11).collect::<Vec<u64>>());
    let list = std::fs::read_to_string(folder.join("authority/machines")).unwrap();
    for name in &names {
        assert!(list.lines().any(|line| line.starts_with(&format!("{name}="))), "{name} is not listed:\n{list}");
    }
}

// Gamma, machine 3, was taken out by making the settings again. A machine
// added to the new team never gets its number, which its data and its
// mark as gone are kept under on the others.
#[test]
fn a_number_given_out_once_is_never_given_out_again() {
    let top = tempfile::tempdir().unwrap();
    let (first, second) = (top.path().join("crew"), top.path().join("crew2"));
    setup::write(&first, &three("127.0.0.1")).expect("writes");
    let stay = [Machine::parse("alpha=10.0.0.5#1").unwrap(), Machine::parse("beta=10.0.0.6#2").unwrap()];
    setup::write_after(&second, &stay, &first).expect("writes the new team");
    let delta = setup::add(&second, Machine::parse("delta=10.0.0.8").unwrap()).expect("adds");
    assert_eq!(delta.number, 4, "delta was given the number of a machine taken out");
    let refused = setup::add(&second, Machine::parse("zeta=10.0.0.9#3").unwrap()).unwrap_err();
    assert!(refused.contains("gamma") && refused.contains('3'), "{refused}");
    assert!(!second.join("zeta").exists());

    // The new team carries every number on, so a third one does too.
    let third = top.path().join("crew3");
    let again = [Machine::parse("alpha=10.0.0.5#1").unwrap(), Machine::parse("delta=10.0.0.8#4").unwrap()];
    setup::write_after(&third, &again, &second).expect("writes the third team");
    assert_eq!(setup::add(&third, Machine::parse("eta=10.0.0.10").unwrap()).unwrap().number, 5);
    // A machine given no number in the new team gets one never used before.
    let fourth = top.path().join("crew4");
    let numbered = [Machine::parse("alpha=10.0.0.5#1").unwrap(), Machine::parse("theta=10.0.0.11").unwrap()];
    setup::write_after(&fourth, &numbered, &third).expect("writes the fourth team");
    let theta = std::fs::read_to_string(fourth.join("theta/orvu.toml")).unwrap();
    assert_eq!(orvud::boot::replica_of(&theta).unwrap().0, 6);
    // And a number used before cannot be given in the new team either.
    let reused = [Machine::parse("alpha=10.0.0.5#1").unwrap(), Machine::parse("iota=10.0.0.12#2").unwrap()];
    let refused = setup::write_after(&top.path().join("crew5"), &reused, &fourth).unwrap_err();
    assert!(refused.contains("beta") && refused.contains('2'), "{refused}");
    assert!(!top.path().join("crew5").exists());
}

// Settings written by hand may spread the list of peers over several lines.
// Adding a machine puts the whole new list in place of the old one.
#[test]
fn a_list_of_peers_over_several_lines_is_replaced_whole() {
    let top = tempfile::tempdir().unwrap();
    let folder = top.path().join("crew");
    let machines = three("127.0.0.1");
    setup::write(&folder, &machines[..2]).expect("writes");
    let alpha = folder.join("alpha/orvu.toml");
    let text = std::fs::read_to_string(&alpha).unwrap();
    let line = text.lines().find(|line| line.starts_with("peers")).unwrap().to_string();
    let spread = format!("peers = [\n    \"{}\",  # beta\n]", machines[1].address());
    std::fs::write(&alpha, text.replace(&line, &spread)).unwrap();
    assert_eq!(peers_of(&alpha), [machines[1].address()]);

    let added = setup::add(&folder, machines[2].clone()).expect("adds");
    assert!(added.not_changed.is_empty(), "{:?}", added.not_changed);
    assert_eq!(peers_of(&alpha), [machines[1].address(), machines[2].address()]);
    let after = std::fs::read_to_string(&alpha).unwrap();
    assert!(after.contains("[security]") && after.contains("replica = 1"), "{after}");
}

// Once a team is made again from a folder, that folder's list is no longer
// the one numbers are taken from. Adding a machine to it, or making a team
// from it again, is refused and points at the new folder, so that two
// machines never get the same number from the two lists.
#[test]
fn a_folder_made_again_elsewhere_gives_out_no_more_numbers() {
    let top = tempfile::tempdir().unwrap();
    let (first, second) = (top.path().join("crew"), top.path().join("crew2"));
    setup::write(&first, &three("127.0.0.1")).expect("writes");
    let stay = [Machine::parse("alpha=10.0.0.5#1").unwrap(), Machine::parse("beta=10.0.0.6#2").unwrap()];
    // Making it again where something is already in the way changes nothing.
    std::fs::create_dir(&second).unwrap();
    std::fs::write(second.join("in-the-way"), "").unwrap();
    assert!(setup::write_after(&second, &stay, &first).is_err());
    assert!(setup::add(&first, Machine::parse("delta=10.0.0.8").unwrap()).is_ok());
    std::fs::remove_dir_all(&second).unwrap();
    setup::write_after(&second, &stay, &first).expect("writes the new team");

    let refused = setup::add(&first, Machine::parse("zeta=10.0.0.8").unwrap()).unwrap_err();
    assert!(refused.contains("crew2"), "{refused}");
    assert!(!first.join("zeta").exists());
    assert_eq!(setup::add(&second, Machine::parse("eps=10.0.0.9").unwrap()).unwrap().number, 5);

    let again = top.path().join("crew3");
    let refused = setup::write_after(&again, &stay, &first).unwrap_err();
    assert!(refused.contains("crew2"), "{refused}");
    assert!(!again.exists());
}

// A run making a team again marks the earlier folder as being made again
// before it makes the new one, and as made again once that is finished.
// If the run was stopped in between and nothing is at the place the mark
// names, the mark is a leftover: it is removed, and the earlier folder is
// used as before, once the person is told.
#[test]
fn a_mark_left_by_a_run_stopped_halfway_does_not_lock_the_folder_out() {
    let top = tempfile::tempdir().unwrap();
    let first = top.path().join("crew");
    setup::write(&first, &three("127.0.0.1")).expect("writes");
    let marker = first.join("authority/replaced");

    // The new folder was never made.
    std::fs::write(&marker, format!("pending {}\n", top.path().join("crew2").display())).unwrap();
    let added = setup::add(&first, Machine::parse("delta=10.0.0.8").unwrap()).expect("adds");
    assert_eq!(added.number, 4);
    assert!(!marker.exists(), "the leftover mark was kept");

    let stay = [Machine::parse("alpha=10.0.0.5#1").unwrap(), Machine::parse("delta=10.0.0.8#4").unwrap()];
    let fourth = top.path().join("crew4");
    setup::write_after(&fourth, &stay, &first).expect("makes the team again");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), format!("finished {}", fourth.display()));
    let refused = setup::add(&first, Machine::parse("zeta=10.0.0.9").unwrap()).unwrap_err();
    assert!(refused.contains("crew4"), "{refused}");
}

// The run was stopped after the new folder was made but before the mark
// said so. Something is at the place the mark names, so it cannot be told
// apart from a finished team: the earlier folder is refused, and the person
// is told how to sort it out.
#[test]
fn a_mark_naming_a_folder_that_is_there_is_never_taken_for_a_leftover() {
    let top = tempfile::tempdir().unwrap();
    let first = top.path().join("crew");
    setup::write(&first, &three("127.0.0.1")).expect("writes");
    let marker = first.join("authority/replaced");
    let second = top.path().join("crew2");
    setup::write(&second, &three("127.0.0.1")[..2]).expect("writes");
    std::fs::write(&marker, format!("pending {}\n", second.display())).unwrap();
    let refused = setup::add(&first, Machine::parse("delta=10.0.0.8").unwrap()).unwrap_err();
    assert!(refused.contains("crew2") && refused.contains("interrupted"), "{refused}");
    assert!(marker.exists() && !first.join("delta").exists());

    // Half a folder is there just the same.
    let half = top.path().join("crew3");
    std::fs::create_dir_all(half.join("authority")).unwrap();
    std::fs::write(&marker, format!("pending {}\n", half.display())).unwrap();
    let stay = [Machine::parse("alpha=10.0.0.5#1").unwrap()];
    assert!(setup::write_after(&top.path().join("crew4"), &stay, &first).is_err());
    assert!(marker.exists() && !top.path().join("crew4").exists());
}

// The new folder was moved after it was made. The mark still names where it
// was, but the team was finished, so the earlier folder stays refused and no
// number is given out twice.
#[test]
fn a_team_made_again_and_moved_still_refuses_the_earlier_folder() {
    let top = tempfile::tempdir().unwrap();
    let (first, second) = (top.path().join("crew"), top.path().join("crew2"));
    setup::write(&first, &three("127.0.0.1")).expect("writes");
    let stay = [Machine::parse("alpha=10.0.0.5#1").unwrap(), Machine::parse("beta=10.0.0.6#2").unwrap()];
    setup::write_after(&second, &stay, &first).expect("writes the new team");
    assert_eq!(setup::add(&second, Machine::parse("delta=10.0.0.8").unwrap()).unwrap().number, 4);
    std::fs::rename(&second, top.path().join("crew-new")).unwrap();

    let refused = setup::add(&first, Machine::parse("zeta=10.0.0.9").unwrap()).unwrap_err();
    assert!(refused.contains("crew2") && refused.contains("moved"), "{refused}");
    assert!(!first.join("zeta").exists());
    assert!(first.join("authority/replaced").exists());
    // A mark written by the earlier version of the program held the folder
    // alone, and was only written once the new team was there.
    std::fs::write(first.join("authority/replaced"), format!("{}\n", second.display())).unwrap();
    let refused = setup::add(&first, Machine::parse("zeta=10.0.0.9").unwrap()).unwrap_err();
    assert!(refused.contains("crew2"), "{refused}");
}

// A mark whose folder cannot be looked at, here because the folder above it
// may not be read, is refused, whether the team was finished or not.
#[test]
fn a_mark_naming_a_folder_that_cannot_be_looked_at_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let top = tempfile::tempdir().unwrap();
    let first = top.path().join("crew");
    setup::write(&first, &three("127.0.0.1")).expect("writes");
    let marker = first.join("authority/replaced");
    let shut = top.path().join("shut");
    let second = shut.join("crew2");
    setup::write(&second, &three("127.0.0.1")[..2]).expect("writes");
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
    // A mark written by the earlier version of the program: one line, the
    // folder alone.
    let mut found = Vec::new();
    for mark in [format!("{}\n", second.display()), format!("pending {}\n", second.display())] {
        std::fs::write(&marker, &mark).unwrap();
        let answer = setup::add(&first, Machine::parse("delta=10.0.0.8").unwrap()).map(|added| added.number);
        found.push((mark, answer, marker.exists()));
    }
    // Opened again before anything is checked, so the folder can be cleared.
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (mark, answer, kept) in found {
        let refused = answer.unwrap_err();
        assert!(refused.contains("crew2"), "{mark}: {refused}");
        assert!(kept, "{mark}: the mark was removed");
    }
    assert!(!first.join("delta").exists());
}

// The real program says plainly that it found such a leftover mark.
#[test]
fn the_command_says_when_it_clears_a_leftover_mark() {
    let top = tempfile::tempdir().unwrap();
    let first = top.path().join("crew");
    setup::write(&first, &three("127.0.0.1")).expect("writes");
    let missing = top.path().join("crew2");
    std::fs::write(first.join("authority/replaced"), format!("pending {}\n", missing.display())).unwrap();
    let added = std::process::Command::new(env!("CARGO_BIN_EXE_orvud"))
        .args(["setup", first.to_str().unwrap(), "--add", "delta=10.0.0.8"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&added.stderr);
    assert!(added.status.success(), "{said}");
    assert!(said.contains(&missing.display().to_string()) && said.contains("stopped"), "{said}");
}
