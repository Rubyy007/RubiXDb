//! M1.3: 1,000 concurrent writers must scale — at least 80,000 ops/sec
//! aggregate throughput. Same structure as M1.2, at 10x the concurrency
//! and total record count.

use rubixdb::wal::{FileWal, Wal, WalConfig};

use crate::support;

const THREADS: usize = 1_000;
const PER_THREAD: usize = 1_000;
const TARGET_OPS_PER_SEC: f64 = 80_000.0;

#[test]
fn thousand_writers_throughput() {
    let result = support::run_throughput_scenario("m1_3", THREADS, PER_THREAD);
    let ops_per_sec = (result.total_records as f64) / result.elapsed.as_secs_f64();

    eprintln!(
        "M1.3 thousand_writers_throughput: {THREADS} threads x {PER_THREAD} records = \
         {} total in {:.3}s => {:.0} ops/sec (target >= {TARGET_OPS_PER_SEC:.0}) \
         (reproduce with: cargo test --release --test group_commit \
         thousand_writers_throughput -- --nocapture)",
        result.total_records,
        result.elapsed.as_secs_f64(),
        ops_per_sec,
    );

    let (_wal, replay) = FileWal::open_for_recovery(&result.dir, WalConfig::default())
        .expect("reopening after a clean shutdown must succeed");
    assert!(replay.corrupted_segments.is_empty());
    assert_eq!(
        replay.records.len(),
        result.total_records,
        "every acknowledged record must be recoverable after reopen"
    );
    for (i, (seq, _)) in replay.records.iter().enumerate() {
        assert_eq!(*seq, (i as u64) + 1, "seq must be gap-free and in order");
    }

    let _ = std::fs::remove_dir_all(&result.dir);

    // Never fabricated: if this misses 80,000 ops/sec, the real number is
    // reported and analyzed in PROCESS.md, not hidden by weakening this
    // assertion.
    assert!(
        ops_per_sec >= TARGET_OPS_PER_SEC,
        "M1.3 threshold miss: {ops_per_sec:.0} ops/sec < {TARGET_OPS_PER_SEC:.0} ops/sec target"
    );
}
