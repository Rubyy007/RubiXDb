//! Phase 1 window-size sweep experiment harness — **not part of the
//! production surface**. Requires `--features phase1-window-experiment`
//! (see `src/wal/group_commit.rs`'s `phase1_window_experiment` module and
//! `PHASE1_ADR.md`). Answers, empirically: does `GroupCommitter` throughput
//! scale with the leader's batch-window size, or is this machine's `fsync`
//! latency the binding constraint independent of window configuration?
//!
//! Unlike `examples/group_commit_load_test.rs` (the production-shaped
//! §16 harness), this prints one single-line, machine-parseable report per
//! run and does not average or retry — every invocation is one data point,
//! to be run three times per configuration and tabulated by hand into
//! `PHASE1_TEST_RESULTS.md`, per that experiment's own "no averages,
//! report the full range" rule.
//!
//! Usage:
//! ```text
//! PHASE1_EXPERIMENT_MAX_WAIT_US=<n or unset> \
//! PHASE1_EXPERIMENT_EMA_DIVISOR=<n, or 0 for "no EMA cap", or unset> \
//! cargo run --release --features phase1-window-experiment --example window_sweep -- <writers> <records_per_writer>
//! ```
//! Leaving both env vars unset reproduces the exact production baseline
//! formula (`min(200µs, EMA/10)`).

use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{FileWal, GroupCommitter, SyncMode, Wal, WalConfig, WalOp};

fn percentile_ms(sorted_ns: &[u128], pct: f64) -> f64 {
    if sorted_ns.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_ns.len() as f64) * pct) as usize;
    (sorted_ns[idx.min(sorted_ns.len() - 1)] as f64) / 1_000_000.0
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let writers: usize = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .expect("usage: window_sweep <writers> <records_per_writer>");
    let per_writer: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .expect("usage: window_sweep <writers> <records_per_writer>");

    let dir = std::env::temp_dir().join(format!(
        "rubixdb_window_sweep_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // The base SyncMode::GroupCommit config still carries the production
    // 200us/256KiB defaults; PHASE1_EXPERIMENT_MAX_WAIT_US, when set,
    // overrides max_wait_cap *inside* GroupCommitter's window formula
    // (see phase1_window_experiment::effective_window) — not here — so
    // the two knobs stay independently attributable in the results.
    let config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_micros(200),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let (wal, _) = FileWal::open_for_recovery(&dir, config).expect("open WAL");
    let committer = GroupCommitter::new(wal).expect("construct GroupCommitter");

    let successful = AtomicU64::new(0);
    let failed = AtomicU64::new(0);
    let samples: Mutex<Vec<u128>> = Mutex::new(Vec::with_capacity(writers * per_writer));

    let started = Instant::now();
    std::thread::scope(|scope| {
        for t in 0..writers {
            let committer = &committer;
            let successful = &successful;
            let failed = &failed;
            let samples = &samples;
            scope.spawn(move || {
                let mut local = Vec::with_capacity(per_writer);
                for i in 0..per_writer {
                    let key = format!("t{t}-{i}");
                    let op_started = Instant::now();
                    match committer.append_durable(WalOp::Put {
                        key: key.as_bytes(),
                        value: b"v",
                    }) {
                        Ok(_) => {
                            successful.fetch_add(1, Ordering::Relaxed);
                            local.push(op_started.elapsed().as_nanos());
                        }
                        Err(_) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                samples.lock().unwrap().extend(local);
            });
        }
    });
    let elapsed = started.elapsed();

    let successful = successful.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    let mut sorted = samples.into_inner().unwrap();
    sorted.sort_unstable();
    let max_ms = sorted
        .last()
        .map(|&ns| (ns as f64) / 1_000_000.0)
        .unwrap_or(0.0);

    let stats = committer.stats();
    let ops_per_sec = (successful as f64) / elapsed.as_secs_f64();

    let wal = committer.into_inner();
    drop(wal);
    let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).expect("reopen");
    // Compared against `total` (every append attempt), not `successful`:
    // a "failed" op here means await_durable timed out, not that append()
    // itself failed — the record was still written and is still expected
    // to be recoverable after a clean shutdown (no fault was injected, no
    // crash occurred). See PHASE1_FAILURE_MODEL.md §3: Timeout is not
    // data loss.
    let recovery_ok = replay.corrupted_segments.is_empty()
        && replay.records.len() as u64 == (writers * per_writer) as u64
        && replay
            .records
            .iter()
            .enumerate()
            .all(|(i, (seq, _))| *seq == (i as u64) + 1);

    let max_wait_env = env::var("PHASE1_EXPERIMENT_MAX_WAIT_US").unwrap_or_else(|_| "unset".into());
    let divisor_env = env::var("PHASE1_EXPERIMENT_EMA_DIVISOR").unwrap_or_else(|_| "unset".into());

    println!(
        "max_wait_us_env={max_wait_env} ema_divisor_env={divisor_env} writers={writers} \
         per_writer={per_writer} total={} successful={successful} failed={failed} \
         elapsed_s={:.3} ops_per_sec={ops_per_sec:.0} sync_count={} avg_batch_size={:.2} \
         max_batch_size={} durable_ops_per_sync={:.2} p50_ms={:.3} p99_ms={:.3} max_ms={max_ms:.3} \
         recovery={}",
        writers * per_writer,
        elapsed.as_secs_f64(),
        stats.sync_successes,
        stats.avg_batch_records(),
        stats.max_batch_records,
        stats.avg_batch_records(),
        percentile_ms(&sorted, 0.50),
        percentile_ms(&sorted, 0.99),
        if recovery_ok { "OK" } else { "FAILED" },
    );

    let _ = std::fs::remove_dir_all(&dir);
}
