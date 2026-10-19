//! A counter that holds one ticket, raced for by three machines. Whoever takes
//! the ticket has the thing it stands for, such as a task. Whatever happens to
//! the links between the machines, a ticket is never taken twice.

use crate::{BoundedCounter, Client, SdkError};
use orvu_crdt::ReplicaId;
use orvu_runtime::NodeConfig;
use orvu_transport::{Cuts, FaultTransport};
use orvu_transport::mem::MemTransport;
use orvu_transport::PeerAddr;
use std::sync::Arc;
use std::time::Duration;

const NAMES: [&str; 3] = ["one", "two", "three"];

/// The same numbers every run for the same seed.
struct Dice(u64);

impl Dice {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % n
    }
}

/// Three machines, each connected to the other two through `cuts`. Allowance
/// is moved between them every 40 milliseconds, so a ticket is often on its
/// way from one machine to another while they race for it.
async fn three(cuts: &Arc<Cuts>) -> Vec<Client> {
    let network = Arc::new(MemTransport::new());
    let mut clients = Vec::new();
    for (at, name) in NAMES.iter().enumerate() {
        let id = at as u64 + 1;
        let mut cfg = NodeConfig::new(ReplicaId(id), *name);
        cfg.peers = NAMES.iter().filter(|other| *other != name).map(|other| PeerAddr::new(*other)).collect();
        cfg.anti_entropy_interval = Duration::from_millis(30);
        cfg.borrow_timeout = Duration::from_millis(150);
        cfg.rebalance_interval = Some(Duration::from_millis(40));
        let transport = FaultTransport::wrap(network.endpoint(id), cuts, id);
        clients.push(Client::start(transport, cfg).await.unwrap());
    }
    clients
}

/// All three machines try to take the ticket at the same moment. Gives how
/// many did.
async fn race(tickets: &[BoundedCounter]) -> usize {
    let tries: Vec<_> = tickets
        .iter()
        .map(|ticket| {
            let ticket = ticket.clone();
            tokio::spawn(async move { ticket.try_decrement(1).await })
        })
        .collect();
    let mut taken = 0;
    for one in tries {
        match one.await.unwrap() {
            Ok(()) => taken += 1,
            Err(SdkError::Exhausted) => {}
            Err(other) => panic!("a try failed: {other:?}"),
        }
    }
    taken
}

async fn everyone_sees(tickets: &[BoundedCounter], value: i64) -> bool {
    for _ in 0..200 {
        let mut all = true;
        for ticket in tickets {
            all &= ticket.value().await.unwrap() == value;
        }
        if all {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

// Each seed starts three machines and makes tickets on them, one at a time.
// A ticket is raced for, given back, and raced for again. Some races happen
// before every machine has heard of the ticket, and some while links are cut:
// one machine cut off, two machines cut from each other, or a link that
// carries traffic one way only. Some tickets are given back while a link is
// cut, and every cut is healed and the ticket raced for again, because what
// was held on both sides of a cut meets when it heals. However it goes, a
// ticket is taken at most as often as it was given.
//
// Once nothing is cut, someone always gets the ticket in the end. A test in
// which every machine was always refused would prove nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_ticket_is_never_taken_twice_whatever_the_links_do() {
    let mut cut_races = 0;
    let mut won_while_cut = 0;
    for seed in 0..16 {
        let mut dice = Dice(seed);
        let cuts = Cuts::new();
        let clients = three(&cuts).await;
        for round in 0..6 {
            let name = format!("ticket-{round}");
            let mut tickets = Vec::new();
            for client in &clients {
                tickets.push(client.bounded_counter(&name).await.unwrap());
            }
            tickets[dice.below(3) as usize].increment(1).await.unwrap();
            if dice.below(4) != 0 {
                assert!(everyone_sees(&tickets, 1).await, "seed {seed}: the ticket did not reach everyone");
            }

            let cut = dice.below(4);
            let (a, b) = (dice.below(3) + 1, dice.below(2));
            let b = [1, 2, 3].into_iter().filter(|id| *id != a).nth(b as usize).unwrap();
            match cut {
                1 => cuts.isolate(a, &[1, 2, 3]),
                2 => cuts.cut(a, b),
                3 => cuts.cut_one_way(a, b),
                _ => {}
            }

            let mut given = 1;
            let mut taken = race(&tickets).await;
            if cut != 0 {
                cut_races += 1;
                won_while_cut += taken;
                if dice.below(2) == 0 {
                    // Given back on some machine while the link is still cut,
                    // and raced for again.
                    tickets[dice.below(3) as usize].increment(1).await.unwrap();
                    given += 1;
                    let more = race(&tickets).await;
                    won_while_cut += more;
                    taken += more;
                }
            }
            assert!(taken <= given, "seed {seed}, round {round}: taken {taken} times, given {given}");
            cuts.heal_all();

            // Nothing is cut now, so no ticket is lost. One may be on its way
            // between machines, and then nobody gets it this time. Someone
            // does soon.
            for _ in 0..40 {
                if taken >= given {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                taken += race(&tickets).await;
            }
            assert_eq!(taken, given, "seed {seed}, round {round}: taken {taken} times, given {given}");
            // Given back on some machine, and raced for once more.
            tickets[dice.below(3) as usize].increment(1).await.unwrap();
            given += 1;
            taken += race(&tickets).await;
            assert!(taken <= given, "seed {seed}, round {round}: taken {taken} times, given {given}");
        }
        for client in clients {
            client.shutdown().await;
        }
    }
    // The cuts were real: some races were run with them, and the machine
    // holding the ticket could still take it while cut off.
    assert!(cut_races > 20, "only {cut_races} races were run with a link cut");
    assert!(won_while_cut > 0, "nobody took a ticket while a link was cut");
}
