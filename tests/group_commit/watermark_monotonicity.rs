//! `watermark_monotonicity`: a proptest over `GroupCommitter`'s core
//! safety property — `durable_through` never decreases under any
//! interleaving, and every waiter whose `await_durable` observed a
//! satisfied condition is genuinely durable after a clean reopen.
//!
//! Unlike `wal::fuzz_tests`' own proptest (pure in-memory, ≥1,000 cases
//! per WAL Spec §11 test #14), each case here drives a real concurrent
//! scenario against real files with real `fsync` calls — a fundamentally
//! more expensive operation per case than the WAL's own byte-level
//! recovery-algorithm fuzz test. `ProptestConfig::with_cases(30)` is used
//! instead of 1,000+ for that reason, documented here rather than silently
//! picked: the brief's ≥1,000-run rule (WAL Spec §11 test #14) is scoped
//! to that specific, purely in-memory invariant; it does not by itself
//! mandate the same case count for a real-I/O concurrency property, and
//! 1,000 cases x real `fsync` calls would make this one test take minutes
//! to tens of minutes depending on the environment's disk latency (this
//! environment's own measured `fsync` latency is ~2.8ms — see
//! `PROCESS.md`'s M1.1-M1.3 benchmark entry).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use proptest::prelude::*;

use rubixdb::wal::{FileWal, GroupCommitter, SyncMode, Wal, WalConfig, WalOp};

use crate::support;

const NO_HANG_BOUND: Duration = Duration::from_secs(15);

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Random arrival order (via a per-waiter stagger derived from the
    /// generated `stagger_micros`) and random batch windows (via generated
    /// `max_wait_micros`/`max_batch_bytes`), driving a real concurrent
    /// `GroupCommitter` scenario.
    #[test]
    fn durable_through_is_monotone_and_acknowledged_waiters_are_genuinely_durable(
        waiter_count in 3usize..40,
        max_wait_micros in 20u64..500,
        max_batch_bytes in 64usize..(64 * 1024),
        stagger_micros in 0u64..200,
    ) {
        let dir = support::temp_dir("watermark_monotonicity");
        let config = WalConfig {
            sync_mode: SyncMode::GroupCommit {
                max_wait: Duration::from_micros(max_wait_micros),
                max_batch_bytes,
            },
            ..WalConfig::default()
        };
        let (wal, _) = FileWal::open_for_recovery(&dir, config).unwrap();
        let committer = Arc::new(GroupCommitter::new(wal).unwrap());

        // Background monitor: samples durable_through repeatedly while the
        // waiters run, recording every observed value in arrival order —
        // this is what "never decreases under any interleaving" is
        // actually checked against, not just the before/after values.
        let monitor_committer = Arc::clone(&committer);
        let stop = Arc::new(AtomicBool::new(false));
        let monitor_stop = Arc::clone(&stop);
        let monitor = thread::spawn(move || {
            let mut observed = Vec::new();
            while !monitor_stop.load(Ordering::Relaxed) {
                observed.push(monitor_committer.durable_through());
                thread::yield_now();
            }
            observed.push(monitor_committer.durable_through());
            observed
        });

        let (tx, rx) = mpsc::channel();
        let handles: Vec<_> = (0..waiter_count).map(|i| {
            let committer = Arc::clone(&committer);
            let tx = tx.clone();
            thread::spawn(move || {
                // "Random arrival order": a small per-waiter stagger, scaled
                // by the waiter's own index modulo a small constant, so
                // waiters genuinely arrive at different times rather than
                // all racing simultaneously in lockstep.
                thread::sleep(Duration::from_micros(stagger_micros * ((i as u64) % 7)));
                let key = format!("k{i}");
                let position = committer
                    .append(WalOp::Put { key: key.as_bytes(), value: b"v" })
                    .expect("append itself never fails in this scenario: no fault injected");
                support::await_durable_retrying_on_timeout(&committer, position.seq);
                let _ = tx.send(position.seq);
            })
        }).collect();
        drop(tx);

        let mut acknowledged = Vec::with_capacity(waiter_count);
        for _ in 0..waiter_count {
            let seq = rx
                .recv_timeout(NO_HANG_BOUND)
                .expect("a waiter's result must arrive within the no-hang bound");
            acknowledged.push(seq);
        }
        for h in handles {
            h.join().expect("writer thread must not panic");
        }
        stop.store(true, Ordering::Relaxed);
        let observed_watermarks = monitor.join().expect("monitor thread must not panic");

        // The core safety property: durable_through must never decrease,
        // under any interleaving this run happened to produce.
        for pair in observed_watermarks.windows(2) {
            prop_assert!(
                pair[1] >= pair[0],
                "durable_through decreased: {} then {}",
                pair[0],
                pair[1]
            );
        }

        let committer = Arc::into_inner(committer)
            .expect("no outstanding Arc clones remain after all threads joined");
        let wal = committer.into_inner();
        drop(wal);

        let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        let recovered: HashSet<u64> = replay.records.iter().map(|(seq, _)| *seq).collect();
        for seq in &acknowledged {
            prop_assert!(
                recovered.contains(seq),
                "acknowledged seq {seq} not found after a clean reopen — \
                 durable_through must never lie about durability"
            );
        }
        prop_assert!(replay.corrupted_segments.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
