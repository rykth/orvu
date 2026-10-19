//! A waiting room for deltas that arrived before the updates they were built
//! on.
//!
//! The data types end up the same on every machine whatever order deltas
//! arrive in, so this buffer is not what makes the final result right. What it
//! does is keep the steps along the way sensible. Without it, a reader could
//! see an effect before its cause for a moment, even though everything would
//! come out right in the end.
//!
//! The buffer has a size limit. When it is full, `push` returns an error and
//! does not crash. The caller then drops the delta, and the regular catch-up
//! between machines brings this one up to date later.
//!
//! A delta also does not wait for ever. Each one remembers when it came in,
//! and `expire` drops those that have waited too long. Without that, deltas
//! whose earlier changes never arrive would fill the buffer for good, and
//! every out-of-order delta after them would be turned away.

use crate::error::{Error, Result};
use crate::object::WireDelta;

#[derive(Clone, Debug, Default)]
pub struct CausalBuffer {
    /// Each waiting delta, with the wall time in milliseconds it came in.
    pending: Vec<(WireDelta, u64)>,
    capacity: usize,
}

impl CausalBuffer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            pending: Vec::new(),
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// True if this update of this object is already waiting here.
    pub fn contains(&self, object_id: &str, dot: orvu_crdt::Dot) -> bool {
        self.pending.iter().any(|(d, _)| d.dot == dot && d.object_id == object_id)
    }

    /// Keeps a delta until the updates it depends on arrive. `now_wall` is
    /// the wall time it came in, in milliseconds. Returns an error if the
    /// buffer is full.
    pub fn push(&mut self, delta: WireDelta, now_wall: u64) -> Result<()> {
        if self.pending.len() >= self.capacity {
            return Err(Error::CausalBufferFull(self.capacity));
        }
        self.pending.push((delta, now_wall));
        Ok(())
    }

    /// Drops every delta that came in more than `limit_ms` milliseconds
    /// before `now_wall`, and returns how many were dropped.
    pub fn expire(&mut self, now_wall: u64, limit_ms: u64) -> usize {
        let before = self.pending.len();
        self.pending.retain(|(_, came)| now_wall.saturating_sub(*came) <= limit_ms);
        before - self.pending.len()
    }

    /// Takes out and returns every waiting delta that `ready` says yes to.
    ///
    /// They come back in the order they were put in, so the ones that have
    /// waited longest are applied first.
    pub fn take_ready<F>(&mut self, ready: F) -> Vec<WireDelta>
    where
        F: Fn(&WireDelta) -> bool,
    {
        let mut taken = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if ready(&self.pending[i].0) {
                taken.push(self.pending.remove(i).0);
            } else {
                i += 1;
            }
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::TypedDelta;
    use orvu_crdt::{Dot, DotContext, Hlc, PnCounter, ReplicaId};

    fn delta(counter: u64) -> WireDelta {
        let mut c = PnCounter::default();
        WireDelta {
            object_id: "o".to_string(),
            dot: Dot::new(ReplicaId(1), counter),
            hlc: Hlc::ZERO,
            deps: DotContext::new(),
            delta: TypedDelta::PnCounter(c.increment(ReplicaId(1), 1)),
            trace_id: crate::trace::TraceId::none(),
        }
    }

    #[test]
    fn push_parks_until_taken() {
        let mut b = CausalBuffer::with_capacity(4);
        assert!(b.is_empty());
        b.push(delta(1), 0).unwrap();
        b.push(delta(2), 0).unwrap();
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn push_rejects_beyond_capacity() {
        let mut b = CausalBuffer::with_capacity(1);
        b.push(delta(1), 0).unwrap();
        assert_eq!(b.push(delta(2), 0).unwrap_err(), Error::CausalBufferFull(1));
        assert_eq!(b.len(), 1, "the rejected delta is not stored");
    }

    #[test]
    fn take_ready_removes_only_matching_and_preserves_order() {
        let mut b = CausalBuffer::with_capacity(8);
        b.push(delta(1), 0).unwrap();
        b.push(delta(2), 0).unwrap();
        b.push(delta(3), 0).unwrap();

        let taken = b.take_ready(|d| d.dot.counter != 2);
        assert_eq!(
            taken.iter().map(|d| d.dot.counter).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(b.len(), 1);
        assert_eq!(b.take_ready(|_| true)[0].dot.counter, 2);
    }

    #[test]
    fn take_ready_on_empty_buffer_is_empty() {
        let mut b = CausalBuffer::with_capacity(2);
        assert!(b.take_ready(|_| true).is_empty());
    }

    #[test]
    fn expire_drops_only_what_waited_longer_than_the_limit() {
        let mut b = CausalBuffer::with_capacity(8);
        b.push(delta(1), 100).unwrap();
        b.push(delta(2), 150).unwrap();
        assert_eq!(b.expire(200, 100), 0);
        assert_eq!(b.expire(201, 100), 1);
        assert_eq!(b.take_ready(|_| true)[0].dot.counter, 2);
        // A wall time that went back drops nothing.
        b.push(delta(3), 500).unwrap();
        assert_eq!(b.expire(10, 100), 0);
    }
}
