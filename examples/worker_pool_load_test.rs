//! Phase 2 benchmark harness for `execution::WriteWorkerPool` — mirrors
//! `tests/group_commit/support.rs::run_throughput_scenario`'s exact
//! methodology (same `WalConfig`, same per-thread record count) so its
//! numbers are directly comparable to the Phase 1 M1.2/M1.3 dedicated
//! tests' own numbers, per `PHASE2_TEST_RESULTS.md`'s "do not compare
//! against unrelated historical runs" rule.
//!
//! Usage: `cargo run --release --example worker_pool_load_test --
//! <writer_count> <worker_count> [per_thread=1000]`
//!
//! Logical writer threads submit through the pool and retry `submit()`
//! only on `EngineError::Timeout` (bounded — never appends twice, see
//! `write_pool`'s own doc comment for why this is always safe), then
//! `Completion::wait()` for the durable result — exactly the shape a
//! real caller would use.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::{WriteWorkerPool, WriteWorkerPoolConfig};
use rubixdb::wal::{FileWal, SyncMode, Wal, WalConfig, WalOpOwned};
use rubixdb::EngineError;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_worker_pool_load_test_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn percentile_ns(sorted: &[u128], pct: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * pct) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn ms(ns: u128) -> f64 {
    ns as f64 / 1_000_000.0
}

fn submit_retrying(
    pool: &WriteWorkerPool,
    key: &[u8],
    value: &[u8],
) -> rubixdb::execution::Completion {
    loop {
        let op = WalOpOwned::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        };
        match pool.submit(op) {
            Ok(completion) => return completion,
            Err(EngineError::Timeout { .. }) => continue,
            Err(e) => panic!("submit failed with a non-backpressure error: {e}"),
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: worker_pool_load_test <writer_count> <worker_count> [per_thread=1000]");
        std::process::exit(2);
    }
    let writer_count: usize = args[1].parse().expect("writer_count must be a usize");
    let worker_count: usize = args[2].parse().expect("worker_count must be a usize");
    let per_thread: usize = args
        .get(3)
        .map(|s| s.parse().expect("per_thread must be a usize"))
        .unwrap_or(1000);

    let dir = temp_dir("run");
    // Identical to tests/group_commit/support.rs::group_commit_config().
    let config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let (wal, _) = FileWal::open_for_recovery(&dir, config).unwrap();
    let committer = rubixdb::wal::GroupCommitter::new(wal).unwrap();

    let pool_config = WriteWorkerPoolConfig {
        worker_count,
        queue_capacity: (writer_count * 4).max(64),
        max_queued_bytes: 64 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(30),
        await_retry_budget: Duration::from_secs(5),
    };
    let pool = Arc::new(WriteWorkerPool::new(committer, pool_config).unwrap());

    let total = writer_count * per_thread;
    let started = Instant::now();
    let handles: Vec<_> = (0..writer_count)
        .map(|t| {
            let pool = Arc::clone(&pool);
            thread::spawn(move || {
                let mut latencies_ns = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let key = format!("t{t}-{i}");
                    let op_started = Instant::now();
                    let completion = submit_retrying(&pool, key.as_bytes(), b"v");
                    completion.wait().expect("write must become durable");
                    latencies_ns.push(op_started.elapsed().as_nanos());
                }
                latencies_ns
            })
        })
        .collect();

    let mut all_latencies_ns: Vec<u128> = Vec::with_capacity(total);
    for h in handles {
        all_latencies_ns.extend(h.join().unwrap());
    }
    let elapsed = started.elapsed();
    all_latencies_ns.sort_unstable();

    let stats = pool.stats();
    println!(
        "writer_count={writer_count} worker_count={worker_count} per_thread={per_thread} total={total}"
    );
    println!(
        "elapsed={:.3}s ops_per_sec={:.0}",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64()
    );
    println!(
        "p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms",
        ms(percentile_ns(&all_latencies_ns, 0.50)),
        ms(percentile_ns(&all_latencies_ns, 0.95)),
        ms(percentile_ns(&all_latencies_ns, 0.99)),
        ms(*all_latencies_ns.last().unwrap_or(&0)),
    );
    println!(
        "submitted={} completed_ok={} completed_err={} rejected_backpressure={} queue_depth_end={}",
        stats.submitted,
        stats.completed_ok,
        stats.completed_err,
        stats.rejected_backpressure,
        stats.queue_depth
    );
    println!(
        "committer: sync_attempts={} sync_successes={} records_total={} max_batch_records={} avg_batch_records={:.2}",
        stats.committer_stats.sync_attempts,
        stats.committer_stats.sync_successes,
        stats.committer_stats.records_total,
        stats.committer_stats.max_batch_records,
        stats.committer_stats.avg_batch_records(),
    );
    let mean_queue_wait_ns = if stats.completed_ok + stats.completed_err > 0 {
        stats.queue_wait_ns_total as f64 / (stats.completed_ok + stats.completed_err) as f64
    } else {
        0.0
    };
    let mean_processing_ns = if stats.completed_ok + stats.completed_err > 0 {
        stats.processing_ns_total as f64 / (stats.completed_ok + stats.completed_err) as f64
    } else {
        0.0
    };
    println!(
        "mean_queue_wait={:.3}ms mean_processing={:.3}ms",
        mean_queue_wait_ns / 1_000_000.0,
        mean_processing_ns / 1_000_000.0
    );

    // Triggers the existing RGC_TIMING_REPORT mechanism (if the env var
    // is set) via GroupCommitter::into_inner, then verifies recovery.
    let pool = Arc::try_unwrap(pool)
        .unwrap_or_else(|_| panic!("no other Arc<WriteWorkerPool> reference should remain here"));
    drop(pool.into_inner().unwrap());

    let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
    let recovery_ok = replay.corrupted_segments.is_empty() && replay.records.len() == total;
    println!(
        "recovery: corrupted_segments={} records_recovered={} expected={} => {}",
        replay.corrupted_segments.len(),
        replay.records.len(),
        total,
        if recovery_ok { "OK" } else { "FAILED" }
    );
    let _ = fs::remove_dir_all(&dir);
    if !recovery_ok {
        std::process::exit(1);
    }
}
