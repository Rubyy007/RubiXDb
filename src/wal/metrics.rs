//! Phase 1 (Group Commit): exponential-moving-average `fsync` latency
//! tracking, used by `group_commit::GroupCommitter` to size its leader wait
//! window (`min(200 µs, EMA / 10)`) and a follower's `condvar.wait_timeout`
//! (`10 * EMA`) — see `PROCESS.md` §1 for the full design rationale.
//!
//! All state lives in a single `AtomicU64` (the brief's own requirement:
//! "All state in `AtomicU64`, no locks"). `record` is safe to call from
//! multiple threads concurrently on its own terms — via a `compare_exchange`
//! retry loop — even though `GroupCommitter`'s leader-election protocol
//! happens to guarantee only one thread ever calls it at a time in
//! practice; a general-purpose primitive should not depend on how its one
//! current caller happens to use it.

use std::sync::atomic::{AtomicU64, Ordering};

/// The EMA smoothing factor (`α`), per the brief: `new = α * sample +
/// (1 - α) * old`.
const ALPHA: f64 = 0.1;

/// Tracks a exponential moving average of `fsync` latency, in nanoseconds.
/// Starts at `0` (see `ema_update`'s doc comment for why this is applied
/// via the plain formula rather than treating the first sample specially).
#[derive(Debug, Default)]
pub struct FsyncLatencyTracker {
    ema_ns: AtomicU64,
}

impl FsyncLatencyTracker {
    /// A fresh tracker, EMA `0` (WAL Spec's group-commit extension point
    /// has no prior data to seed from at startup — every `GroupCommitter`
    /// begins with no latency history).
    pub fn new() -> Self {
        FsyncLatencyTracker {
            ema_ns: AtomicU64::new(0),
        }
    }

    /// Folds one `fsync` latency sample (nanoseconds) into the running EMA:
    /// `new = 0.1 * sample + 0.9 * old`. Uses a `compare_exchange_weak`
    /// retry loop rather than a plain load-then-store so this remains
    /// correct even if called concurrently from more than one thread —
    /// see this module's doc comment.
    pub fn record(&self, latency_ns: u64) {
        let mut old = self.ema_ns.load(Ordering::Relaxed);
        loop {
            let new = ema_update(old, latency_ns);
            match self
                .ema_ns
                .compare_exchange_weak(old, new, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(observed) => old = observed,
            }
        }
    }

    /// The current EMA, in nanoseconds. `0` if `record` has never been
    /// called.
    pub fn current_ns(&self) -> u64 {
        self.ema_ns.load(Ordering::Acquire)
    }
}

/// `new = α * sample + (1 - α) * old`, per the brief's formula, applied
/// uniformly including when `old == 0` (a fresh tracker's initial state) —
/// this is a deliberate choice, not an oversight: the brief's own required
/// test coverage ("zero initial value handling") is a test *of* the plain
/// formula starting from `old = 0`, not a request to special-case the first
/// sample into `new = sample` outright. The practical effect is that the
/// EMA warms up gradually over roughly the first ~10 samples rather than
/// snapping to the first observed latency — consistent with treating an
/// isolated first `fsync` (which may be unrepresentative, e.g. cold
/// filesystem caches) with the same weight the formula gives any other
/// single sample.
///
/// Computed in `f64` (the brief's formula is stated in real-number terms,
/// `0.1`/`0.9`) and rounded back to `u64` nanoseconds; both operands are
/// non-negative `u64` values converted to `f64`, so the result is always
/// non-negative and representable, and `.round()` before the final `as u64`
/// avoids silently truncating a value like `99.9` down to `99`.
fn ema_update(old_ns: u64, sample_ns: u64) -> u64 {
    let new_ns = ALPHA * (sample_ns as f64) + (1.0 - ALPHA) * (old_ns as f64);
    new_ns.round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering as AtomicOrdering;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn starts_at_zero() {
        let t = FsyncLatencyTracker::new();
        assert_eq!(t.current_ns(), 0);
    }

    #[test]
    fn default_also_starts_at_zero() {
        let t = FsyncLatencyTracker::default();
        assert_eq!(t.current_ns(), 0);
    }

    /// "Zero initial value handling": the very first sample is folded into
    /// `old = 0` via the plain formula, not snapped to the sample value.
    #[test]
    fn first_sample_applies_the_plain_formula_against_a_zero_old_value() {
        let t = FsyncLatencyTracker::new();
        t.record(1_000_000);
        // 0.1 * 1_000_000 + 0.9 * 0 = 100_000, not 1_000_000.
        assert_eq!(t.current_ns(), 100_000);
    }

    #[test]
    fn second_sample_matches_hand_computed_ema() {
        let t = FsyncLatencyTracker::new();
        t.record(1_000_000); // ema = 100_000
        t.record(2_000_000); // ema = 0.1*2_000_000 + 0.9*100_000 = 290_000
        assert_eq!(t.current_ns(), 290_000);
    }

    #[test]
    fn ema_update_matches_the_formula_directly() {
        assert_eq!(ema_update(0, 1_000), 100);
        assert_eq!(ema_update(1_000, 1_000), 1_000);
        assert_eq!(ema_update(2_000, 1_000), 1_900);
    }

    #[test]
    fn monotonically_converges_upward_toward_a_larger_constant_sample() {
        let t = FsyncLatencyTracker::new();
        let mut prev = t.current_ns();
        for _ in 0..200 {
            t.record(1_000_000);
            let cur = t.current_ns();
            assert!(
                cur >= prev,
                "EMA must not decrease while converging up toward a sample above it: {prev} -> {cur}"
            );
            prev = cur;
        }
        assert!(
            prev > 990_000,
            "expected convergence close to steady state 1_000_000, got {prev}"
        );
    }

    #[test]
    fn monotonically_converges_downward_toward_a_smaller_constant_sample() {
        let t = FsyncLatencyTracker::new();
        for _ in 0..50 {
            t.record(1_000_000);
        }
        let mut prev = t.current_ns();
        for _ in 0..200 {
            t.record(100_000);
            let cur = t.current_ns();
            assert!(
                cur <= prev,
                "EMA must not increase while converging down toward a sample below it: {prev} -> {cur}"
            );
            prev = cur;
        }
        assert!(
            prev < 110_000,
            "expected convergence close to steady state 100_000, got {prev}"
        );
    }

    #[test]
    fn record_is_safe_under_concurrent_calls_from_multiple_threads() {
        let tracker = Arc::new(FsyncLatencyTracker::new());
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let tracker = Arc::clone(&tracker);
                thread::spawn(move || {
                    for _ in 0..2_000 {
                        tracker.record(500_000);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Every sample fed in was exactly 500_000, so regardless of
        // interleaving the EMA must converge to (and never exceed) it.
        let v = tracker.current_ns();
        assert!(
            v <= 500_000,
            "EMA {v} exceeds the constant sample fed to it"
        );
        assert!(
            v > 490_000,
            "EMA {v} did not converge close enough after 32,000 samples"
        );
    }

    #[test]
    fn current_ns_uses_acquire_and_observes_a_release_store_from_another_thread() {
        // Not a torture test for the memory model (impossible to prove
        // absence of reordering with a unit test); just a straightforward
        // cross-thread visibility check exercising the Acquire/Release pair.
        let tracker = Arc::new(FsyncLatencyTracker::new());
        let writer = {
            let tracker = Arc::clone(&tracker);
            thread::spawn(move || tracker.record(42_000))
        };
        writer.join().unwrap();
        assert_eq!(tracker.current_ns(), 4_200);
        // Sanity: the underlying atomic really did change (not comparing
        // against a stale local read).
        assert_ne!(tracker.ema_ns.load(AtomicOrdering::Relaxed), 0);
    }
}
