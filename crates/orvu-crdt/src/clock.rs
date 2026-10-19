//! Hybrid logical clock (HLC): a timestamp made of wall-clock time plus a
//! counter that orders events sharing the same wall-clock time.
//! The caller passes the wall time in, so the clock never reads the system
//! time itself. That makes its behaviour repeatable and easy to test.

/// A hybrid logical timestamp. Timestamps compare by `wall` first, then by
/// `logical`.
///
/// `logical` counts events that share one `wall` value. It stops at `u32::MAX`
/// instead of wrapping around.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Hlc {
    pub wall: u64,
    pub logical: u32,
}

impl Hlc {
    pub const ZERO: Hlc = Hlc { wall: 0, logical: 0 };

    /// Moves the clock forward for an event on this machine and returns the new
    /// timestamp. `now_wall` is the current wall-clock time.
    pub fn tick(&mut self, now_wall: u64) -> Hlc {
        if now_wall > self.wall {
            self.wall = now_wall;
            self.logical = 0;
        } else {
            self.logical = self.logical.saturating_add(1);
        }
        *self
    }

    /// How far this timestamp's wall part is ahead of `now_wall`, or 0 if it
    /// is not ahead.
    pub fn lead_over(&self, now_wall: u64) -> u64 {
        self.wall.saturating_sub(now_wall)
    }

    /// Moves the clock forward when the timestamp `remote` arrives from another
    /// machine, and returns the new timestamp. `now_wall` is the current
    /// wall-clock time.
    ///
    /// This takes any `remote` it is given. The clock never goes back, so one
    /// timestamp from far in the future would stay in it for good. Callers that
    /// take timestamps from other machines should first check how far ahead
    /// they are (see `lead_over`) and turn away any that are too far.
    pub fn update(&mut self, now_wall: u64, remote: Hlc) -> Hlc {
        let max_wall = now_wall.max(self.wall).max(remote.wall);
        if max_wall == self.wall && max_wall == remote.wall {
            self.logical = self.logical.max(remote.logical).saturating_add(1);
        } else if max_wall == self.wall {
            self.logical = self.logical.saturating_add(1);
        } else if max_wall == remote.wall {
            self.logical = remote.logical.saturating_add(1);
        } else {
            self.logical = 0;
        }
        self.wall = max_wall;
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_bumps_logical_when_wall_stalls() {
        let mut c = Hlc::ZERO;
        let t1 = c.tick(10);
        let t2 = c.tick(10); // same wall time, so the counter goes up
        assert_eq!(t1, Hlc { wall: 10, logical: 0 });
        assert_eq!(t2, Hlc { wall: 10, logical: 1 });
        assert!(t2 > t1);
    }

    #[test]
    fn tick_resets_logical_when_wall_advances() {
        let mut c = Hlc { wall: 10, logical: 5 };
        let t = c.tick(11);
        assert_eq!(t, Hlc { wall: 11, logical: 0 });
    }

    #[test]
    fn lead_over_counts_only_time_ahead() {
        let t = Hlc { wall: 100, logical: 7 };
        assert_eq!(t.lead_over(40), 60);
        assert_eq!(t.lead_over(100), 0);
        assert_eq!(t.lead_over(500), 0);
    }

    #[test]
    fn update_stays_ahead_of_remote() {
        let mut c = Hlc { wall: 10, logical: 0 };
        let remote = Hlc { wall: 10, logical: 3 };
        let t = c.update(9, remote); // the wall time passed in is behind both clocks
        assert!(t > remote);
        assert_eq!(t, Hlc { wall: 10, logical: 4 });
    }
}
