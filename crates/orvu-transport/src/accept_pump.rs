//! Limits how many connection handshakes a listener works on at once.
//!
//! A handshake here is the setup that happens before we know who the other
//! side is. The listener runs each handshake in its own task, so one stalled
//! party cannot delay another. The number of tasks has to
//! be limited. Otherwise an attacker gets a task and a socket for every
//! connection attempt, which is a denial of service of its own.
//!
//! The limit is counted per source address, not as one shared total. With one
//! shared budget, a flood of attempts can make an honest peer wait behind it
//! for `ceil(queued / N) * HANDSHAKE_TIMEOUT`. A limit per peer identity would
//! avoid that too, but the handshake is what produces the identity, so it is
//! not known yet. The source address is the only thing we can tell peers apart
//! by at this point, so the limit is per IP address, and also per block of
//! addresses, because one attacker can easily hold a whole block.

use crate::error::{Result, TransportError};
use crate::traits::Conn;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// How many handshakes from one source address may be in progress at once.
///
/// This limits handshakes in progress, not connections and not clients. A
/// handshake finishes in milliseconds. So even hundreds of machines sharing
/// one address (NAT) have only a handful in progress at any moment. The limit stops
/// attackers, who hold handshakes open for the full timeout. Normal traffic
/// does not do that and is not affected.
pub(crate) const PER_IP_HANDSHAKES: usize = 8;

/// The most handshakes in progress across all sources. This only limits memory
/// use. The per-address limit is what protects an honest peer from a flood.
pub(crate) const MAX_HANDSHAKES: usize = 256;

/// How many handshakes from one block of addresses may be in progress at
/// once. A block is the first 24 bits of an IPv4 address or the first 64 bits
/// of an IPv6 one, which is what one network, or one customer of an internet
/// provider, usually holds.
///
/// Without it, one attacker with many addresses, which is cheap with IPv6,
/// could hold `PER_IP_HANDSHAKES` from each until `MAX_HANDSHAKES` is used up,
/// and every honest machine would be refused. With it, filling the overall
/// limit takes addresses in at least `MAX_HANDSHAKES / PER_BLOCK_HANDSHAKES`
/// (8) separate blocks. Machines that share a block in good faith, such as
/// those on one office network, each finish a handshake in milliseconds, so
/// they rarely have more than a few in progress at once.
pub(crate) const PER_BLOCK_HANDSHAKES: usize = 32;

/// The block an address belongs to: the address with everything after its
/// first 24 bits (IPv4) or 64 bits (IPv6) set to zero.
fn block_of(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4((u32::from(v4) & 0xffff_ff00).into()),
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
    }
}

/// How many finished connections can wait for the caller to pick them up.
pub(crate) const ACCEPT_QUEUE_CAP: usize = 64;

/// The longest one handshake may take.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct AdmissionState {
    per_ip: BTreeMap<IpAddr, usize>,
    per_block: BTreeMap<IpAddr, usize>,
    total: usize,
}

/// Counts the handshakes in progress for each source address and decides
/// whether a new one may start.
#[derive(Clone)]
pub(crate) struct HandshakeAdmission {
    state: Arc<Mutex<AdmissionState>>,
    per_ip_limit: usize,
    per_block_limit: usize,
    global_limit: usize,
}

impl HandshakeAdmission {
    pub(crate) fn new(per_ip: usize, per_block: usize, global: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(AdmissionState::default())),
            per_ip_limit: per_ip,
            per_block_limit: per_block,
            global_limit: global,
        }
    }

    /// Takes a handshake slot for `ip`. Returns `None` if this source, or the
    /// block of addresses it is in, is already at its limit, or the overall
    /// limit is reached.
    pub(crate) fn claim(&self, ip: IpAddr) -> Option<AdmissionPermit> {
        // An IPv4 address can arrive written as an IPv6 one. Counted as
        // written, it would get a second share of its own limits.
        let ip = ip.to_canonical();
        let block = block_of(ip);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Check every limit before changing any count. A refused claim creates
        // no permit, so nothing would undo a count raised too early. The same
        // goes for an `or_insert(0)` entry added before the check. If a limit
        // were 0, the maps would grow by one entry for every address an
        // attacker uses.
        if st.total >= self.global_limit {
            return None;
        }
        if st.per_ip.get(&ip).copied().unwrap_or(0) >= self.per_ip_limit {
            return None;
        }
        if st.per_block.get(&block).copied().unwrap_or(0) >= self.per_block_limit {
            return None;
        }
        *st.per_ip.entry(ip).or_insert(0) += 1;
        *st.per_block.entry(block).or_insert(0) += 1;
        st.total += 1;
        Some(AdmissionPermit { ip, block, state: self.state.clone() })
    }

    /// The number of source addresses that hold at least one permit right now.
    /// Only for tests. They use it to check that the map holds addresses with
    /// a handshake in progress, not every address ever seen.
    #[cfg(test)]
    pub(crate) fn live_ips(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).per_ip.len()
    }

    /// The number of address blocks that hold at least one permit right now.
    /// Only for tests, like `live_ips`.
    #[cfg(test)]
    pub(crate) fn live_blocks(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).per_block.len()
    }
}

/// Holds one handshake slot. Dropping it gives the slot back.
///
/// Because the release is in `Drop`, it happens however the handshake ends:
/// success, timeout, failure, panic, or a cancelled task. A permit that was
/// never released would block its source address for good. That would be a
/// worse denial of service than the one this module guards against, and we
/// would have caused it ourselves.
pub(crate) struct AdmissionPermit {
    ip: IpAddr,
    block: IpAddr,
    state: Arc<Mutex<AdmissionState>>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Plain `-=` is used here and for the per-address count below. Neither
        // can go below zero as long as `claim` raises both under one lock. If
        // a permit were ever dropped without having been counted, the
        // underflow panic shows it. `saturating_sub` would hide that bug.
        st.total -= 1;
        if let Some(n) = st.per_ip.get_mut(&self.ip) {
            *n -= 1;
            // Remove the entry at zero. Otherwise the map would grow with
            // every address an attacker uses, and that costs them nothing.
            if *n == 0 {
                st.per_ip.remove(&self.ip);
            }
        }
        if let Some(n) = st.per_block.get_mut(&self.block) {
            *n -= 1;
            if *n == 0 {
                st.per_block.remove(&self.block);
            }
        }
    }
}

/// How many failed handshakes can wait to be reported by `accept()`. When
/// this many are waiting, further failures are dropped.
pub(crate) const ACCEPT_ERROR_CAP: usize = 16;

/// The queue of finished handshakes that a listener's `accept()` reads from.
///
/// Finished connections and failed handshakes wait in two separate queues.
/// Failures come from strangers and cost them nothing, so if both shared one
/// queue, a stream of failures could fill it and push out real connections.
///
/// `Listener::accept` takes `&self`, so the receivers sit behind a mutex.
/// `MemListener` is built the same way, and like it this reports dropped
/// senders as `TransportError::Closed`.
pub(crate) struct AcceptQueue {
    rx: tokio::sync::Mutex<Receivers>,
}

struct Receivers {
    conns: mpsc::Receiver<Box<dyn Conn>>,
    errors: mpsc::Receiver<TransportError>,
}

/// The sending side of an `AcceptQueue`. Handshake tasks hand their result to
/// it.
#[derive(Clone)]
pub(crate) struct AcceptSender {
    conns: mpsc::Sender<Box<dyn Conn>>,
    errors: mpsc::Sender<TransportError>,
}

impl AcceptSender {
    /// Hands over the result of one handshake. This never waits. If the queue
    /// for that kind of result is full, the result is dropped. A task that
    /// waited here would keep its admission permit for as long as nobody
    /// calls `accept()`, and a slow caller would then use up the slots that
    /// are meant to limit attackers.
    pub(crate) fn offer(&self, result: Result<Box<dyn Conn>>) {
        match result {
            Ok(conn) => {
                let _ = self.conns.try_send(conn);
            }
            Err(e) => {
                let _ = self.errors.try_send(e);
            }
        }
    }
}

impl AcceptQueue {
    pub(crate) fn new(cap: usize) -> (AcceptSender, Self) {
        let (conns_tx, conns) = mpsc::channel(cap);
        let (errors_tx, errors) = mpsc::channel(ACCEPT_ERROR_CAP);
        let rx = tokio::sync::Mutex::new(Receivers { conns, errors });
        (AcceptSender { conns: conns_tx, errors: errors_tx }, Self { rx })
    }

    /// The next finished connection, or the next failed handshake if no
    /// connection is waiting.
    pub(crate) async fn accept(&self) -> Result<Box<dyn Conn>> {
        let mut rx = self.rx.lock().await;
        let Receivers { conns, errors } = &mut *rx;
        tokio::select! {
            biased;
            conn = conns.recv() => conn.ok_or(TransportError::Closed),
            Some(e) = errors.recv() => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    // Failed handshakes come from strangers and cost them nothing. However
    // many arrive, a finished connection must still get through.
    #[tokio::test]
    async fn failed_handshakes_do_not_crowd_out_a_connection() {
        use crate::traits::Transport;
        use tokio_util::sync::CancellationToken;

        let (tx, queue) = AcceptQueue::new(4);
        for _ in 0..100 {
            tx.offer(Err(TransportError::Listen("bad handshake".into())));
        }

        let mem = crate::MemTransport::new();
        let _listener = mem.listen("a", CancellationToken::new()).await.unwrap();
        let conn = mem.dial(crate::PeerAddr::new("a"), CancellationToken::new()).await.unwrap();
        tx.offer(Ok(conn));

        // The connection comes out first, and the failures are still reported.
        assert!(queue.accept().await.is_ok(), "the connection was dropped");
        assert!(queue.accept().await.is_err());
    }

    #[test]
    fn a_source_ip_is_capped_and_other_ips_are_unaffected() {
        let adm = HandshakeAdmission::new(2, 100, 100);
        let a1 = adm.claim(ip(1)).expect("first from .1 admitted");
        let a2 = adm.claim(ip(1)).expect("second from .1 admitted");
        assert!(
            adm.claim(ip(1)).is_none(),
            "a third concurrent handshake from the same source must be refused"
        );
        // This is the main point of the limit: a different source is not
        // affected.
        let b1 = adm.claim(ip(2)).expect("a different source must still be admitted");
        drop((a1, a2, b1));
    }

    #[test]
    fn releasing_a_permit_frees_the_slot() {
        let adm = HandshakeAdmission::new(1, 100, 100);
        let p = adm.claim(ip(1)).unwrap();
        assert!(adm.claim(ip(1)).is_none());
        drop(p);
        assert!(
            adm.claim(ip(1)).is_some(),
            "the slot must be reusable once the permit is dropped"
        );
    }

    #[test]
    fn the_global_ceiling_bounds_total_work_across_sources() {
        let adm = HandshakeAdmission::new(10, 100, 3);
        let _a = adm.claim(ip(1)).unwrap();
        let _b = adm.claim(ip(2)).unwrap();
        let _c = adm.claim(ip(3)).unwrap();
        assert!(
            adm.claim(ip(4)).is_none(),
            "the global ceiling must bound total in-flight handshakes"
        );
    }

    #[test]
    fn a_permit_is_released_even_when_the_handshake_panics() {
        let adm = HandshakeAdmission::new(1, 100, 100);
        let a = adm.clone();
        let handle = std::thread::spawn(move || {
            let _permit = a.claim(ip(1)).expect("admitted");
            panic!("handshake exploded");
        });
        assert!(handle.join().is_err(), "the thread must have panicked");
        assert!(
            adm.claim(ip(1)).is_some(),
            "a leaked permit would bar this source permanently — worse than the DoS being fixed"
        );
    }

    #[test]
    fn a_global_ceiling_rejection_does_not_consume_a_per_ip_slot() {
        let adm = HandshakeAdmission::new(10, 100, 1);
        let _a = adm.claim(ip(1)).unwrap(); // fills the global ceiling
        assert!(adm.claim(ip(2)).is_none(), "global ceiling rejects ip 2");
        assert_eq!(adm.live_ips(), 1, "ip(2) must not have created live state");
    }

    #[test]
    fn a_zero_per_ip_limit_admits_nothing_and_leaves_no_state() {
        let adm = HandshakeAdmission::new(0, 100, 100);
        assert!(adm.claim(ip(1)).is_none(), "a zero per-ip limit must admit nothing");
        assert_eq!(
            adm.live_ips(),
            0,
            "a rejected fresh ip must not leave a phantom map entry — with no \
             permit to drop it, the map would grow per attacker-controlled ip forever"
        );
    }

    #[test]
    fn the_per_ip_map_does_not_grow_with_ips_ever_seen() {
        let adm = HandshakeAdmission::new(4, 100, 100);
        for i in 1..=50u8 {
            let p = adm.claim(ip(i)).expect("admitted");
            drop(p);
        }
        assert_eq!(
            adm.live_ips(),
            0,
            "entries must be removed at zero: keying growth on attacker-controlled \
             input is a memory leak the attacker fills for free"
        );
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn v6(net: u16, host: u16) -> IpAddr {
        IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, net, 0, 0, 0, host))
    }

    // One attacker can easily have a whole block of addresses. Each address on
    // its own stays under its limit, but together they must not take more than
    // the block's share, or they could fill the overall limit and shut every
    // honest machine out.
    #[test]
    fn many_addresses_in_one_ipv4_block_share_one_limit() {
        let adm = HandshakeAdmission::new(2, 4, 100);
        let held: Vec<_> = (1..=4).map(|i| adm.claim(v4(10, 0, 0, i)).expect("admitted")).collect();
        assert!(
            adm.claim(v4(10, 0, 0, 200)).is_none(),
            "a fifth address from the same /24 must be refused"
        );
        assert!(adm.claim(v4(10, 0, 1, 1)).is_some(), "the next /24 is another block");
        drop(held);
        assert!(adm.claim(v4(10, 0, 0, 200)).is_some(), "the block's slots come back");
    }

    #[test]
    fn many_addresses_in_one_ipv6_block_share_one_limit() {
        let adm = HandshakeAdmission::new(2, 4, 100);
        let _held: Vec<_> = (1..=4).map(|i| adm.claim(v6(7, i)).expect("admitted")).collect();
        assert!(adm.claim(v6(7, 999)).is_none(), "a fifth address from the same /64 must be refused");
        assert!(adm.claim(v6(8, 1)).is_some(), "the next /64 is another block");
    }

    // An IPv4 address can also arrive written as an IPv6 one. It is still the
    // same block.
    #[test]
    fn an_ipv4_address_written_as_ipv6_counts_in_its_ipv4_block() {
        let adm = HandshakeAdmission::new(2, 2, 100);
        let _a = adm.claim(v4(10, 0, 0, 1)).unwrap();
        let mapped = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 2).to_ipv6_mapped());
        let _b = adm.claim(mapped).unwrap();
        assert!(adm.claim(v4(10, 0, 0, 3)).is_none());
    }

    // With the real limits, one block of 256 addresses cannot fill the
    // overall limit, so a machine elsewhere still gets in.
    #[test]
    fn a_flood_from_one_block_leaves_room_for_everyone_else() {
        let adm = HandshakeAdmission::new(PER_IP_HANDSHAKES, PER_BLOCK_HANDSHAKES, MAX_HANDSHAKES);
        let mut held = Vec::new();
        for host in 0..=255u8 {
            for _ in 0..PER_IP_HANDSHAKES {
                held.extend(adm.claim(v4(203, 0, 113, host)));
            }
        }
        assert_eq!(held.len(), PER_BLOCK_HANDSHAKES);
        assert!(adm.claim(v4(198, 51, 100, 7)).is_some(), "an honest machine was shut out");
    }

    #[test]
    fn the_block_map_does_not_grow_with_blocks_ever_seen() {
        let adm = HandshakeAdmission::new(4, 4, 100);
        for i in 1..=50u8 {
            drop(adm.claim(v4(10, i, 0, 1)).expect("admitted"));
        }
        assert_eq!(adm.live_blocks(), 0);
    }

    #[test]
    fn a_block_rejection_does_not_consume_an_address_slot() {
        let adm = HandshakeAdmission::new(4, 1, 100);
        let _a = adm.claim(v4(10, 0, 0, 1)).unwrap();
        assert!(adm.claim(v4(10, 0, 0, 2)).is_none());
        assert_eq!(adm.live_ips(), 1, "the refused address must not have left state behind");
    }

    #[tokio::test]
    async fn the_queue_yields_conns_and_errors_in_order() {
        let (tx, q) = AcceptQueue::new(4);
        tx.offer(Err(TransportError::Listen("boom".into())));
        assert!(matches!(q.accept().await, Err(TransportError::Listen(_))));
        drop(tx);
        assert!(
            matches!(q.accept().await, Err(TransportError::Closed)),
            "a dropped sender must surface as Closed, matching MemListener"
        );
    }
}
