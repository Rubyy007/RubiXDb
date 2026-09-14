//! Phase 1 (Group Commit) load-test harness — §16 of the Phase 1 spec.
//!
//! **Write-only**, by explicit direction: this repository has no read
//! path at all (only the WAL exists — Memtable/SSTable/the LSM facade
//! were never built, per `PROGRESS.md`), and §16 itself says "do not
//! fabricate read performance that the engine does not yet support." So
//! this harness measures 100% writes, not the spec's literal 80/20 split;
//! `reads_configured: 0` is reported explicitly in every level's output
//! rather than silently reinterpreting the requirement.
//!
//! **CPU utilization / RSS are NOT VERIFIED ON THIS PLATFORM** — collecting
//! either would require a new dependency (e.g. `sysinfo`) or
//! platform-specific APIs, neither authorized for Phase 1. See
//! `PHASE1_TEST_RESULTS.md` for the explicit gap record.
//!
//! Usage: `cargo run --release --example group_commit_load_test`
//! Runs the full writer-concurrency matrix (1, 10, 100, 1,000) with a
//! warm-up phase (not measured) before each level's measured phase, and
//! prints a report in the shape §16/§24 ask for. Capture this output and
//! store it under `PHASE1_TEST_RESULTS.md`'s evidence location for the
//! official record — this binary itself does not write a report file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{FileWal, GroupCommitter, SyncMode, Wal, WalConfig, WalOp};
use rubixdb::EngineError;

/// §16: "10,000,000 uniformly distributed keys."
const KEY_SPACE: u64 = 10_000_000;
/// §16: "256 bytes."
const VALUE_SIZE: usize = 256;
/// §16: "writer concurrency: 1, 10, 100, 1,000."
const CONCURRENCY_LEVELS: [usize; 4] = [1, 10, 100, 1_000];
/// Both warm-up and measured op counts are a roughly-constant *total*
/// divided among however many threads are running at that level, not a
/// fixed per-thread count — otherwise the concurrency=1 level (no batching
/// partner, ~3ms per op on this machine's disk — see PHASE1_TEST_RESULTS.md)
/// would dominate total harness runtime, and concurrency=1000's warm-up
/// alone would need 1000x concurrency=1's op count for no added value.
const WARMUP_OPS_TOTAL_TARGET: usize = 3_000;
const MEASURED_OPS_TOTAL_TARGET: usize = 10_000;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_load_test_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn dir_size_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                }
            }
        }
    }
    total
}

fn percentile_ns(sorted: &[u128], pct: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * pct) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn ms(ns: u128) -> f64 {
    (ns as f64) / 1_000_000.0
}

fn group_commit_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            // 5ms, post-window-size-sweep default — see
            // PHASE1_TEST_RESULTS.md and PHASE1_ADR.md ADR-12.
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn run_level(concurrency: usize) {
    let dir = temp_dir(&format!("c{concurrency}"));
    let (wal, _) =
        FileWal::open_for_recovery(&dir, group_commit_config()).expect("open a fresh WAL");
    let committer = GroupCommitter::new(wal).expect("construct GroupCommitter");

    // Warm-up phase: not measured, lets the EMA settle and pages get
    // touched before the timed run.
    let warmup_ops_per_thread = (WARMUP_OPS_TOTAL_TARGET / concurrency.max(1)).max(10);
    thread::scope(|scope| {
        for t in 0..concurrency {
            let committer = &committer;
            scope.spawn(move || {
                for i in 0..warmup_ops_per_thread {
                    let key = ((t as u64 * 1_000_003 + i as u64) % KEY_SPACE).to_le_bytes();
                    let value = [0u8; VALUE_SIZE];
                    let _ = committer.append_durable(WalOp::Put {
                        key: &key,
                        value: &value,
                    });
                }
            });
        }
    });
    let stats_after_warmup = committer.stats();

    let ops_per_thread = (MEASURED_OPS_TOTAL_TARGET / concurrency.max(1)).max(20);
    let successful = AtomicU64::new(0);
    let failed_timeout = AtomicU64::new(0);
    let failed_other = AtomicU64::new(0);
    let samples: Mutex<Vec<u128>> = Mutex::new(Vec::with_capacity(ops_per_thread * concurrency));

    let started = Instant::now();
    thread::scope(|scope| {
        for t in 0..concurrency {
            let committer = &committer;
            let successful = &successful;
            let failed_timeout = &failed_timeout;
            let failed_other = &failed_other;
            let samples = &samples;
            scope.spawn(move || {
                let mut local = Vec::with_capacity(ops_per_thread);
                for i in 0..ops_per_thread {
                    let key = ((t as u64 * 7_919 + i as u64 + 1) % KEY_SPACE).to_le_bytes();
                    let value = [0xABu8; VALUE_SIZE];
                    let op_started = Instant::now();
                    match committer.append_durable(WalOp::Put {
                        key: &key,
                        value: &value,
                    }) {
                        Ok(_) => {
                            successful.fetch_add(1, Ordering::Relaxed);
                            local.push(op_started.elapsed().as_nanos());
                        }
                        // A Timeout here means "this caller's bounded wait
                        // expired," not "the write was lost" — the append()
                        // half of append_durable already landed in the WAL
                        // and remains eligible to become durable via a
                        // later batch; it is the caller's own wait that
                        // gave up. Recorded as a failed *operation* (this
                        // harness never retries), per §16's "do not report
                        // throughput without recording failures" — but
                        // distinguished from a real error so the report
                        // doesn't conflate the two.
                        Err(EngineError::Timeout { .. }) => {
                            failed_timeout.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            failed_other.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                samples.lock().unwrap().extend(local);
            });
        }
    });
    let duration = started.elapsed();

    let successful = successful.load(Ordering::Relaxed);
    let failed_timeout = failed_timeout.load(Ordering::Relaxed);
    let failed_other = failed_other.load(Ordering::Relaxed);
    let failed = failed_timeout + failed_other;
    let total = successful + failed;
    let mut sorted_samples = samples.into_inner().unwrap();
    sorted_samples.sort_unstable();
    let max_ns = sorted_samples.last().copied().unwrap_or(0);

    let stats_after_measured = committer.stats();
    let batches_this_level = stats_after_measured
        .sync_successes
        .saturating_sub(stats_after_warmup.sync_successes);
    let records_this_level = stats_after_measured
        .records_total
        .saturating_sub(stats_after_warmup.records_total);
    let durable_ops_per_sync = if batches_this_level == 0 {
        0.0
    } else {
        (records_this_level as f64) / (batches_this_level as f64)
    };

    let wal_bytes = dir_size_bytes(&dir.join("wal"));

    let wal = committer.into_inner();
    drop(wal);
    let (_wal, replay) =
        FileWal::open_for_recovery(&dir, WalConfig::default()).expect("reopen for recovery check");
    let recovery_ok = replay.corrupted_segments.is_empty()
        && replay
            .records
            .iter()
            .enumerate()
            .all(|(i, (seq, _))| *seq == (i as u64) + 1);

    println!(
        "--- concurrency = {concurrency} writer(s), 0 reader(s) (write-only, see module doc) ---"
    );
    println!("total_operations:        {total}");
    println!("successful_operations:   {successful}");
    println!("failed_operations:       {failed} (timeout={failed_timeout}, other={failed_other})");
    println!("duration:                {:.3} s", duration.as_secs_f64());
    println!(
        "writes/sec (=total throughput): {:.0}",
        (successful as f64) / duration.as_secs_f64().max(1e-9)
    );
    println!("reads/sec:                0 (no read path exists in this repository)");
    println!(
        "p50 latency:              {:.3} ms",
        ms(percentile_ns(&sorted_samples, 0.50))
    );
    println!(
        "p95 latency:              {:.3} ms",
        ms(percentile_ns(&sorted_samples, 0.95))
    );
    println!(
        "p99 latency:              {:.3} ms",
        ms(percentile_ns(&sorted_samples, 0.99))
    );
    println!("max latency:              {:.3} ms", ms(max_ns));
    println!("batch_count (this level): {batches_this_level}");
    println!(
        "avg_batch_size (this level): {:.2}",
        if batches_this_level == 0 {
            0.0
        } else {
            (records_this_level as f64) / (batches_this_level as f64)
        }
    );
    println!(
        "max_batch_size (cumulative since committer construction): {}",
        stats_after_measured.max_batch_records
    );
    println!(
        "avg_wait_time (leader batch window, cumulative):    {:.3} us",
        stats_after_measured.avg_window_wait_ns() / 1000.0
    );
    println!("sync_count (this level):  {batches_this_level}");
    println!("durable_ops_per_sync (this level): {durable_ops_per_sync:.2}");
    println!("wal_bytes_written:        {wal_bytes}");
    println!("cpu_utilization:          NOT VERIFIED ON THIS PLATFORM (no profiler/sysinfo dependency authorized)");
    println!("memory_usage / process_rss: NOT VERIFIED ON THIS PLATFORM (no profiler/sysinfo dependency authorized)");
    println!(
        "recovery_correctness_after_run: {}",
        if recovery_ok { "OK" } else { "FAILED" }
    );
    println!();

    let _ = fs::remove_dir_all(&dir);
}

fn main() {
    println!("RubixDB Phase 1 Group Commit — load-test harness (§16)");
    println!(
        "key_space={KEY_SPACE} value_size={VALUE_SIZE}B warmup_ops_total_target={WARMUP_OPS_TOTAL_TARGET}"
    );
    println!(
        "measured_ops_total_target={MEASURED_OPS_TOTAL_TARGET} (divided across the thread count at each level)"
    );
    println!();
    for &concurrency in &CONCURRENCY_LEVELS {
        run_level(concurrency);
    }
}
