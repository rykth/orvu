//! A pretend network for testing, where every run can be repeated exactly.
//!
//! It runs several `Replica` nodes and decides, step by step, which message is
//! delivered when. All of its random choices come from one `u64` seed, so the
//! same seed always gives the same run. The network reorders, duplicates and
//! delays messages. It also loses them, splits the nodes into two groups that
//! cannot reach each other, and crashes nodes. Nodes catch up by comparing what
//! they have seen and resending what the other lacks. At the end the test
//! checks that every node holds the same data, and that receiving everything a
//! second time changes nothing.

use orvu_core::{Applied, TypedCrdt, TypedDelta, WireDelta};
use orvu_core::replica::Replica;
use orvu_crdt::{DotContext, ReplicaId};
use std::collections::{BTreeMap, BTreeSet};

// ---- settings (fixed, so every run is the same) ----
const CAP: usize = 4096;
const N_NODES: u64 = 4;
const ROUNDS: u64 = 80;
const MAX_DELAY: u64 = 6;
const SEED_COUNT: u64 = 150;
const MAX_QUIESCE: u64 = 200_000;

// One object for each data type. Its id is the name of the type.
const OBJECTS: [&str; 5] = ["counter", "register", "set", "map", "cart"];

// ---- random numbers from a seed (splitmix64) ----
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

fn empty_of(ty: &str) -> TypedCrdt {
    match ty {
        "counter" => TypedCrdt::PnCounter(Default::default()),
        "register" => TypedCrdt::LwwRegister(Default::default()),
        "set" => TypedCrdt::OrSet(Default::default()),
        "map" => TypedCrdt::OrMap(Default::default()),
        "cart" => TypedCrdt::OrMapCounter(Default::default()),
        other => panic!("unknown type '{other}'"),
    }
}

/// Changes a randomly chosen object on `r` and returns the delta it made.
fn gen_op(rng: &mut Rng, r: &mut Replica, tick: u64) -> WireDelta {
    let id = OBJECTS[rng.below(OBJECTS.len() as u64) as usize];
    let wd = match id {
        "counter" => {
            let by = rng.below(7) as i64 - 3;
            r.mutate(id, tick, move |c, _h, rep| match c {
                TypedCrdt::PnCounter(pn) => TypedDelta::PnCounter(if by >= 0 {
                    pn.increment(rep, by as u64)
                } else {
                    pn.decrement(rep, by.unsigned_abs())
                }),
                _ => unreachable!(),
            })
        }
        "register" => {
            let v = format!("v{}", rng.below(3));
            r.mutate(id, tick, move |c, h, rep| match c {
                TypedCrdt::LwwRegister(reg) => TypedDelta::LwwRegister(reg.set(v, h, rep)),
                _ => unreachable!(),
            })
        }
        "set" => {
            let v = format!("e{}", rng.below(3));
            if rng.chance(1, 2) {
                r.mutate(id, tick, move |c, _h, rep| match c {
                    TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.add(v, rep).unwrap()),
                    _ => unreachable!(),
                })
            } else {
                r.mutate(id, tick, move |c, _h, _r| match c {
                    TypedCrdt::OrSet(s) => TypedDelta::OrSet(s.remove(&v)),
                    _ => unreachable!(),
                })
            }
        }
        "map" => {
            let k = format!("k{}", rng.below(2));
            if rng.chance(2, 3) {
                let val = format!("m{}", rng.below(3));
                r.mutate(id, tick, move |c, h, rep| match c {
                    TypedCrdt::OrMap(m) => {
                        TypedDelta::OrMap(m.update(k, rep, |reg| reg.set(val, h, rep)).unwrap())
                    }
                    _ => unreachable!(),
                })
            } else {
                r.mutate(id, tick, move |c, _h, _r| match c {
                    TypedCrdt::OrMap(m) => TypedDelta::OrMap(m.remove(&k)),
                    _ => unreachable!(),
                })
            }
        }
        "cart" => {
            let item = format!("i{}", rng.below(2));
            if rng.chance(2, 3) {
                let by = rng.below(5) as i64 - 2;
                r.mutate(id, tick, move |c, _h, rep| match c {
                    TypedCrdt::OrMapCounter(m) => TypedDelta::OrMapCounter(m.update(item, rep, |pn| {
                        if by >= 0 {
                            pn.increment(rep, by as u64)
                        } else {
                            pn.decrement(rep, by.unsigned_abs())
                        }
                    }).unwrap()),
                    _ => unreachable!(),
                })
            } else {
                r.mutate(id, tick, move |c, _h, _r| match c {
                    TypedCrdt::OrMapCounter(m) => TypedDelta::OrMapCounter(m.remove(&item)),
                    _ => unreachable!(),
                })
            }
        }
        _ => unreachable!(),
    };
    wd.expect("object exists")
}

struct Node {
    replica: Replica,
    log: Vec<WireDelta>, // every delta this node has applied, for resending and for replay after a crash
}

struct Msg {
    deliver_tick: u64,
    from: u64,
    to: u64,
    delta: WireDelta,
}

struct Sim {
    ids: Vec<u64>,
    nodes: BTreeMap<u64, Node>,
    inflight: Vec<Msg>,
    partition: Option<BTreeSet<u64>>, // one side of a two-way split; `None` means everyone can reach everyone
    tick: u64,
    rng: Rng,
    trace: Vec<String>,
}

impl Sim {
    fn new(seed: u64) -> Sim {
        let ids: Vec<u64> = (1..=N_NODES).collect();
        let mut nodes = BTreeMap::new();
        for &id in &ids {
            let mut replica = Replica::new(ReplicaId(id), CAP);
            for ty in OBJECTS {
                replica.create(ty, empty_of(ty));
            }
            nodes.insert(id, Node { replica, log: Vec::new() });
        }
        Sim { ids, nodes, inflight: Vec::new(), partition: None, tick: 0, rng: Rng(seed), trace: Vec::new() }
    }

    fn reachable(&self, a: u64, b: u64) -> bool {
        match &self.partition {
            None => true,
            Some(side) => side.contains(&a) == side.contains(&b),
        }
    }

    fn enqueue(&mut self, from: u64, to: u64, delta: WireDelta) {
        let delay = self.rng.below(MAX_DELAY) + 1;
        self.inflight.push(Msg { deliver_tick: self.tick + delay, from, to, delta });
    }

    /// Sends a delta from `from` to every node it can reach, now and then
    /// sending it twice.
    fn broadcast(&mut self, from: u64, delta: WireDelta) {
        for to in self.ids.clone() {
            if to == from || !self.reachable(from, to) {
                continue;
            }
            if self.rng.chance(1, 6) {
                continue; // lost for good; the catch-up round has to bring it back
            }
            self.enqueue(from, to, delta.clone());
            if self.rng.chance(1, 8) {
                self.enqueue(from, to, delta.clone());
            }
        }
    }

    /// One catch-up round. For each pair of nodes that can reach each other,
    /// each side resends the deltas the other has not seen. It works that out
    /// from the other's real `observed_contexts()`. This brings back lost
    /// messages and fills the gaps a split leaves.
    fn anti_entropy(&mut self) {
        let ids = self.ids.clone();
        for &a in &ids {
            for &b in &ids {
                if a >= b || !self.reachable(a, b) {
                    continue;
                }
                let a_ctx = self.nodes[&a].replica.observed_contexts();
                let b_ctx = self.nodes[&b].replica.observed_contexts();
                let lacks = |ctx: &BTreeMap<String, DotContext>, d: &WireDelta| {
                    ctx.get(&d.object_id).is_none_or(|c| !c.contains(d.dot))
                };
                let to_a: Vec<WireDelta> =
                    self.nodes[&b].log.iter().filter(|d| lacks(&a_ctx, d)).cloned().collect();
                let to_b: Vec<WireDelta> =
                    self.nodes[&a].log.iter().filter(|d| lacks(&b_ctx, d)).cloned().collect();
                for d in to_a {
                    self.enqueue(b, a, d);
                }
                for d in to_b {
                    self.enqueue(a, b, d);
                }
            }
        }
    }

    /// Crashes node `id`. Its replica is built again from nothing and its own
    /// log is replayed, which stands in for recovery from disk. Messages on
    /// their way to or from it are lost.
    fn crash(&mut self, id: u64) {
        let log = self.nodes[&id].log.clone();
        let mut fresh = Replica::new(ReplicaId(id), CAP);
        for ty in OBJECTS {
            fresh.create(ty, empty_of(ty));
        }
        let now = self.tick;
        let mut prev = usize::MAX;
        loop {
            for d in &log {
                let _ = fresh.receive(d.clone(), now);
            }
            let p = fresh.pending();
            if p == 0 || p == prev {
                break;
            }
            prev = p;
        }
        self.nodes.get_mut(&id).expect("node").replica = fresh;
        self.inflight.retain(|m| m.to != id && m.from != id);
        self.trace.push(format!("t{} crash n{}", self.tick, id));
    }

    /// Delivers every message on its way whose time has come and whose two
    /// ends can reach each other.
    fn deliver_due(&mut self) {
        let mut keep = Vec::new();
        let mut due = Vec::new();
        for m in std::mem::take(&mut self.inflight) {
            if m.deliver_tick <= self.tick {
                due.push(m);
            } else {
                keep.push(m);
            }
        }
        self.inflight = keep;
        for m in due {
            if !self.reachable(m.from, m.to) {
                continue; // lost on the way because of a split
            }
            let now = self.tick;
            let node = self.nodes.get_mut(&m.to).expect("node exists");
            if let Ok((Applied::Applied, drained)) = node.replica.receive(m.delta.clone(), now) {
                node.log.push(m.delta);
                node.log.extend(drained);
            }
        }
    }

    fn all_pending_zero(&self) -> bool {
        self.nodes.values().all(|n| n.replica.pending() == 0)
    }

    fn assert_converged(&self, seed: u64) {
        for id in OBJECTS {
            let states: Vec<Option<&TypedCrdt>> =
                self.ids.iter().map(|i| self.nodes[i].replica.object(id).map(|o| o.crdt())).collect();
            for (k, s) in states.iter().enumerate().skip(1) {
                if *s != states[0] {
                    let tail: Vec<&String> = self.trace.iter().rev().take(12).collect();
                    panic!(
                        "seed {seed}: divergence on object '{id}'\n node {} = {:?}\n node {} = {:?}\n recent trace: {:?}",
                        self.ids[0], states[0], self.ids[k], s, tail
                    );
                }
            }
        }
        for i in &self.ids {
            assert_eq!(self.nodes[i].replica.pending(), 0, "seed {seed}: node {i} left deltas pending");
        }
    }

    /// A new node that receives every delta any node applied ends up with the
    /// same data as the others. Receiving them all a second time changes
    /// nothing.
    fn assert_idempotent(&self, seed: u64) {
        // Collect the deltas from every node, each one once, by (object_id, dot).
        let mut union: BTreeMap<(String, u64, u64), WireDelta> = BTreeMap::new();
        for n in self.nodes.values() {
            for d in &n.log {
                union.insert((d.object_id.clone(), d.dot.replica.0, d.dot.counter), d.clone());
            }
        }
        let deltas: Vec<WireDelta> = union.into_values().collect();

        let mut obs = Replica::new(ReplicaId(999_999), CAP);
        for ty in OBJECTS {
            obs.create(ty, empty_of(ty));
        }
        let drain = |obs: &mut Replica| {
            let mut prev = usize::MAX;
            loop {
                for d in &deltas {
                    let _ = obs.receive(d.clone(), 1);
                }
                let p = obs.pending();
                if p == 0 || p == prev {
                    break;
                }
                prev = p;
            }
        };
        drain(&mut obs);
        let node0 = &self.nodes[&self.ids[0]].replica;
        for id in OBJECTS {
            assert_eq!(
                obs.object(id).map(|o| o.crdt()),
                node0.object(id).map(|o| o.crdt()),
                "seed {seed}: observer disagrees with node {} on '{id}'",
                self.ids[0]
            );
        }
        // Deliver the whole set again. The state must not change.
        let before: Vec<Option<TypedCrdt>> =
            OBJECTS.iter().map(|id| obs.object(id).map(|o| o.crdt().clone())).collect();
        drain(&mut obs);
        let after: Vec<Option<TypedCrdt>> =
            OBJECTS.iter().map(|id| obs.object(id).map(|o| o.crdt().clone())).collect();
        assert_eq!(before, after, "seed {seed}: re-delivery changed state (not idempotent)");
    }
}

fn run_scenario(seed: u64) {
    let mut sim = Sim::new(seed);
    for round in 0..ROUNDS {
        sim.tick += 1;

        // Splits: now and then start a two-way split, and end one more often
        // than that.
        if sim.partition.is_none() && sim.rng.chance(1, 40) {
            let mut side = BTreeSet::new();
            for &id in &sim.ids.clone() {
                if sim.rng.chance(1, 2) {
                    side.insert(id);
                }
            }
            // Skip a split with nobody, or everybody, on one side. It would
            // do nothing.
            if !side.is_empty() && side.len() < sim.ids.len() {
                sim.trace.push(format!("t{} partition {:?}", sim.tick, side));
                sim.partition = Some(side);
            }
        } else if sim.partition.is_some() && sim.rng.chance(1, 8) {
            sim.trace.push(format!("t{} heal", sim.tick));
            sim.partition = None;
        }

        // Now and then, a crash.
        if sim.rng.chance(1, 50) {
            let victim = sim.ids[sim.rng.below(sim.ids.len() as u64) as usize];
            sim.crash(victim);
        }

        for id in sim.ids.clone() {
            if sim.rng.chance(1, 2) {
                let wd = {
                    let tick = sim.tick;
                    let node = sim.nodes.get_mut(&id).expect("node");
                    let wd = gen_op(&mut sim.rng, &mut node.replica, tick);
                    node.log.push(wd.clone());
                    wd
                };
                sim.trace.push(format!("t{} n{} op {}", sim.tick, id, wd.object_id));
                sim.broadcast(id, wd);
            }
        }

        // A catch-up round every few steps. During a split it only helps
        // nodes on the same side.
        if round % 5 == 0 {
            sim.anti_entropy();
        }
        sim.deliver_due();
    }

    // End any split, then run catch-up rounds and deliveries until every node
    // holds the same data.
    sim.partition = None;
    let mut guard = 0;
    loop {
        sim.tick += 1;
        sim.anti_entropy();
        sim.deliver_due();
        guard += 1;
        if sim.inflight.is_empty() && sim.all_pending_zero() && guard > 3 {
            break;
        }
        if guard > MAX_QUIESCE {
            panic!("seed {seed}: failed to quiesce after loss/partition (stuck)");
        }
    }

    sim.assert_converged(seed);
    sim.assert_idempotent(seed);
}

#[test]
fn deterministic_convergence_sweep() {
    for seed in 0..SEED_COUNT {
        run_scenario(seed);
    }
}
