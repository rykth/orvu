//! Identifiers that never change: one for a replica, and one (a "dot") for a
//! single update that a replica made.

/// Identifies one replica: a device, process or engine instance that holds its
/// own copy of the data. A replica keeps the same id for its whole life.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ReplicaId(pub u64);

/// A "dot": a unique tag for one update. It names the replica that made the
/// update and carries a counter that goes up with each update from that
/// replica, so no two updates anywhere share a dot.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Dot {
    pub replica: ReplicaId,
    pub counter: u64,
}

impl Dot {
    /// Builds the dot for update number `counter` from `replica`.
    pub fn new(replica: ReplicaId, counter: u64) -> Self {
        Self { replica, counter }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_order_by_replica_then_counter() {
        let a = Dot::new(ReplicaId(1), 5);
        let b = Dot::new(ReplicaId(1), 6);
        let c = Dot::new(ReplicaId(2), 1);
        assert!(a < b);
        assert!(b < c); // replica 1 < replica 2 regardless of counter
    }
}
