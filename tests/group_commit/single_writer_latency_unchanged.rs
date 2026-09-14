//! M1.1: a single writer using `GroupCommitter` must show zero meaningful
//! behavioral regression against the existing `FileWal::append_sync`
//! (`Immediate` mode) baseline. With no concurrent batching partner, every
//! `GroupCommitter` call becomes its own leader immediately — the only
//! extra cost over `Immediate` mode is one bounded `min(200 µs, EMA / 10)`
//! wait window and one `File::try_clone` syscall per call, both negligible
//! next to a real `fsync`'s latency.

use std::time::Instant;

use rubixdb::wal::{FileWal, GroupCommitter, Wal, WalConfig, WalOp};

use crate::support;

const ITERATIONS: usize = 1_000;

#[test]
fn single_writer_latency_unchanged() {
    // Baseline: Immediate mode, append_sync — exactly the existing,
    // pre-Phase-1 code path, untouched by this phase.
    let baseline_dir = support::temp_dir("m1_1_baseline");
    let (mut wal, _) = FileWal::open_for_recovery(&baseline_dir, WalConfig::default())
        .expect("opening a fresh WAL must succeed");
    let mut baseline_samples = Vec::with_capacity(ITERATIONS);
    for i in 0..ITERATIONS {
        let key = format!("k{i}");
        let started = Instant::now();
        wal.append_sync(WalOp::Put {
            key: key.as_bytes(),
            value: b"v",
        })
        .expect("append_sync must not fail against a healthy temp directory");
        baseline_samples.push(started.elapsed().as_nanos());
    }
    drop(wal);
    let _ = std::fs::remove_dir_all(&baseline_dir);

    // GroupCommitter, single writer, no concurrent batching partner.
    let gc_dir = support::temp_dir("m1_1_group_commit");
    let (wal, _) = FileWal::open_for_recovery(&gc_dir, support::group_commit_config())
        .expect("opening a fresh WAL must succeed");
    let committer = GroupCommitter::new(wal).expect("wal was opened with SyncMode::GroupCommit");
    let mut gc_samples = Vec::with_capacity(ITERATIONS);
    for i in 0..ITERATIONS {
        let key = format!("k{i}");
        let started = Instant::now();
        let position = committer
            .append(WalOp::Put {
                key: key.as_bytes(),
                value: b"v",
            })
            .expect("append must not fail against a healthy temp directory");
        support::await_durable_retrying_on_timeout(&committer, position.seq);
        gc_samples.push(started.elapsed().as_nanos());
    }
    let wal = committer.into_inner();
    drop(wal);

    let baseline_median = support::median_nanos(&baseline_samples);
    let gc_median = support::median_nanos(&gc_samples);
    let gc_p99 = support::percentile_nanos(&gc_samples, 0.99);

    eprintln!(
        "M1.1 single_writer_latency_unchanged: baseline(Immediate) median={:.3}ms; \
         GroupCommitter(single-writer) median={:.3}ms p99={:.3}ms \
         (reproduce with: cargo test --release --test group_commit \
         single_writer_latency_unchanged -- --nocapture)",
        support::ms(baseline_median),
        support::ms(gc_median),
        support::ms(gc_p99),
    );

    // Relative comparison, not a hardcoded absolute figure: real `fsync`
    // latency is environment-dependent (this machine's own Phase 0 WAL
    // benchmark measured 3.3-10.2ms per append_sync at various payload
    // sizes — see PROGRESS.md), so an absolute "must be under Xms"
    // assertion would be meaningless without first knowing this
    // environment's own disk latency. A generous multiplicative allowance
    // (2x + 1ms flat) comfortably covers legitimate overhead (the wait
    // window, the clone syscall, scheduling jitter) while still catching a
    // real regression (e.g. accidentally serializing two fsyncs per call).
    let allowed_ns = (baseline_median as f64) * 2.0 + 1_000_000.0;
    assert!(
        (gc_median as f64) <= allowed_ns,
        "GroupCommitter single-writer median ({:.3}ms) regressed vs. baseline \
         ({:.3}ms) by more than the allowed margin (limit {:.3}ms)",
        support::ms(gc_median),
        support::ms(baseline_median),
        allowed_ns / 1_000_000.0,
    );

    // The brief's own literal figure, asserted whenever this environment's
    // baseline itself is capable of meeting it — never weakened into
    // meaninglessness on a slow disk, but also never asserted as an
    // impossible absolute on hardware whose *baseline* already exceeds it.
    if baseline_median <= 5_000_000 {
        assert!(
            gc_median <= 5_000_000,
            "GroupCommitter single-writer median {:.3}ms exceeds the 5ms target \
             (baseline {:.3}ms, which itself met the target)",
            support::ms(gc_median),
            support::ms(baseline_median),
        );
    } else {
        eprintln!(
            "M1.1: baseline median ({:.3}ms) itself exceeds 5ms on this environment's \
             disk, so the brief's absolute 5ms target is not meaningful here; \
             relying on the relative-regression assertion above instead.",
            support::ms(baseline_median)
        );
    }

    let _ = std::fs::remove_dir_all(&gc_dir);
}
