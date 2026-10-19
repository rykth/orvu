//! LWW-Register: a single value where the last write wins ("last-writer-wins").
//! Writes are ordered by `(Hlc, ReplicaId, value)`: timestamp first, then the
//! writer's id, then the value itself.
//!
//! The writer id settles equal timestamps, and the value settles equal
//! timestamps from the same writer. So any two different writes have a clear
//! winner, and `merge` gives the same result whatever order writes arrive in.
//! That holds even when two replicas produce the same `(timestamp, writer)`
//! pair. This can happen after a restart that loses the clock's state, when
//! `Hlc::logical` reaches its maximum, or when a `ReplicaId` is reused.
//!
//! Callers should still give every write its own `(timestamp, writer)` pair.
//! Comparing values is only a fallback. It does not replace a clock that
//! always moves forward and a `ReplicaId` that belongs to one replica only.

use crate::clock::Hlc;
use crate::id::ReplicaId;
use crate::DeltaCrdt;

/// Holds one value. When writes compete, the latest one wins.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LwwRegister<V> {
    value: Option<V>,
    ts: Hlc,
    writer: ReplicaId,
}

/// One write to an `LwwRegister`, to send to other replicas.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LwwDelta<V> {
    value: Option<V>,
    ts: Hlc,
    writer: ReplicaId,
}

impl<V> LwwDelta<V> {
    /// Rebuilds a delta from its parts. Used when decoding a received message.
    pub fn from_parts(value: Option<V>, ts: Hlc, writer: ReplicaId) -> Self {
        Self { value, ts, writer }
    }

    /// The value this delta carries. It is `None` when the delta holds no
    /// value, as the snapshot of a register that was never written does.
    pub fn value(&self) -> Option<&V> {
        self.value.as_ref()
    }

    /// The timestamp this write claims.
    pub fn ts(&self) -> Hlc {
        self.ts
    }

    /// The replica that authored this write.
    pub fn writer(&self) -> ReplicaId {
        self.writer
    }
}

impl<V> Default for LwwRegister<V> {
    fn default() -> Self {
        Self { value: None, ts: Hlc::ZERO, writer: ReplicaId(0) }
    }
}

impl<V: Clone + Ord> LwwRegister<V> {
    /// Writes `value` with the given timestamp and writer. The stored value
    /// changes only if this write beats the one already there.
    pub fn set(&mut self, value: V, ts: Hlc, writer: ReplicaId) -> LwwDelta<V> {
        let delta = LwwDelta { value: Some(value), ts, writer };
        self.merge(delta.clone());
        delta
    }

    /// The current value, or `None` if nothing has been written.
    pub fn get(&self) -> Option<&V> {
        self.value.as_ref()
    }

    /// True if the given write beats the stored one. It compares timestamp,
    /// then writer, then value, as explained at the top of this file.
    fn beats(&self, ts: Hlc, writer: ReplicaId, value: &Option<V>) -> bool {
        (ts, writer) > (self.ts, self.writer)
            || ((ts, writer) == (self.ts, self.writer) && *value > self.value)
    }
}

impl<V: Clone + Ord> DeltaCrdt for LwwRegister<V> {
    type Delta = LwwDelta<V>;

    fn merge(&mut self, delta: LwwDelta<V>) {
        if self.beats(delta.ts, delta.writer, &delta.value) {
            self.value = delta.value;
            self.ts = delta.ts;
            self.writer = delta.writer;
        }
    }

    fn snapshot(&self) -> LwwDelta<V> {
        LwwDelta { value: self.value.clone(), ts: self.ts, writer: self.writer }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hlc(w: u64) -> Hlc {
        Hlc { wall: w, logical: 0 }
    }

    #[test]
    fn later_timestamp_wins() {
        let mut r = LwwRegister::<&str>::default();
        r.set("a", hlc(1), ReplicaId(1));
        r.set("b", hlc(2), ReplicaId(1));
        assert_eq!(r.get(), Some(&"b"));
    }

    #[test]
    fn equal_timestamp_breaks_by_writer_id() {
        let mut r1 = LwwRegister::<&str>::default();
        let d_low = r1.set("low", hlc(5), ReplicaId(1));
        let mut r2 = LwwRegister::<&str>::default();
        let d_high = r2.set("high", hlc(5), ReplicaId(2));

        r1.merge(d_high);
        r2.merge(d_low);
        // Higher writer id wins on both, regardless of merge order.
        assert_eq!(r1.get(), Some(&"high"));
        assert_eq!(r1, r2);
    }

    #[test]
    fn equal_timestamp_and_writer_breaks_by_value() {
        let ts = hlc(5);
        let w = ReplicaId(7);
        let mut a = LwwRegister::<&str>::default();
        let da = a.set("A", ts, w);
        let mut b = LwwRegister::<&str>::default();
        let db = b.set("B", ts, w);
        a.merge(db);
        b.merge(da);
        assert_eq!(a, b, "same (ts, writer) with different values must converge");
        assert_eq!(a.get(), Some(&"B")); // "B" > "A"
    }

    #[test]
    fn lww_delta_round_trips_through_parts() {
        let mut r = LwwRegister::<String>::default();
        let d = r.set("hello".to_string(), hlc(3), ReplicaId(2));
        let rebuilt = LwwDelta::from_parts(d.value().cloned(), d.ts(), d.writer());
        assert_eq!(rebuilt, d);

        let mut applied = LwwRegister::<String>::default();
        applied.merge(rebuilt);
        assert_eq!(applied.get().map(String::as_str), Some("hello"));
    }
}
