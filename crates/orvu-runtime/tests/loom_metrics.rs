//! Checks the lock-free metrics with loom, a tool that runs a test under every
//! possible ordering of its threads.
//!
//! The whole file is only compiled with `--cfg orvu_loom`. In a normal build
//! it compiles to nothing and needs no `loom` dependency. The flag is private
//! to this crate. A global `--cfg loom` would switch tokio into its own loom
//! mode and break the build of the dependencies. Run with:
//!   RUSTFLAGS="--cfg orvu_loom" cargo test -p orvu-runtime --test loom_metrics --release
#![cfg(orvu_loom)]

use orvu_runtime::metrics::Histogram;
use orvu_runtime::Metrics;
use loom::sync::atomic::Ordering;
use loom::sync::Arc;

/// Two threads each call `observe(0.5)` twice. loom tries every ordering of the
/// bucket `fetch_add`s and the compare-and-swap loops on `sum_bits`. The running
/// sum must not lose an update. `0.5` is exact in f64 and `<= 1.0`, so it lands
/// in bucket 0 and every partial sum (0.5, 1.0, 1.5, 2.0) is exact. That makes
/// `assert_eq!` on floats safe here.
#[test]
fn histogram_observe_has_no_lost_update() {
    loom::model(|| {
        let h = Arc::new(Histogram::<2>::new(&[1.0], 1.0));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let h = h.clone();
                loom::thread::spawn(move || {
                    h.observe(0.5);
                    h.observe(0.5);
                })
            })
            .collect();
        for hd in handles {
            hd.join().unwrap();
        }
        let snap = h.snapshot();
        assert_eq!(snap.count, 4, "every observation counted");
        assert_eq!(snap.sum, 2.0, "CAS accumulator lost no update");
        assert_eq!(snap.buckets[0], 4, "all four landed in the le=1.0 bucket");
        assert_eq!(snap.buckets[1], 0, "nothing in +Inf");
    });
}

/// Two threads each call `fetch_add(1)` twice on the same counter. A counter is
/// a plain atomic, and it must add up correctly when threads run at once.
#[test]
fn counters_fetch_add_totals_correctly() {
    loom::model(|| {
        let m = Arc::new(Metrics::default());
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let m = m.clone();
                loom::thread::spawn(move || {
                    m.deltas_applied_local.fetch_add(1, Ordering::Relaxed);
                    m.deltas_applied_local.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();
        for hd in handles {
            hd.join().unwrap();
        }
        assert_eq!(m.deltas_applied_local.load(Ordering::Relaxed), 4);
    });
}
