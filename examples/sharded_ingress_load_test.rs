//! Phase 2B benchmark harness for `execution::sharded_ingress::
//! ShardedIngressPool` (Approach C). Same methodology as the other
//! Phase 2/2B harnesses.
//!
//! Usage: `cargo run --release --example sharded_ingress_load_test --
//! <writer_count> <shard_count> [per_thread=1000]`

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::sharded_ingress::{
    Completion, ShardedIngressConfig, ShardedIngressPool,
};
use rubixdb::wal::{FileWal, SyncMode, Wal, WalConfig, WalOpOwned};
use rubixdb::EngineError;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_sharded_load_test_{tag}_{nanos}"));
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

fn submit_retrying(pool: &ShardedIngressPool, key: &[u8], value: &[u8]) -> Completion {
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
        eprintln!("usage: sharded_ingress_load_test <writer_count> <shard_count> [per_thread=1000]");
        std::process::exit(2);
    }
    let writer_count: usize = args[1].parse().expect("writer_count must be a usize");
    let shard_count: usize = args[2].parse().expect("shard_count must be a usize");
    let per_thread: usize = args
        .get(3)
        .map(|s| s.parse().expect("per_thread must be a usize"))
        .unwrap_or(1000);

    let dir = temp_dir("run");
    let config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let (wal, _) = FileWal::open_for_recovery(&dir, config).unwrap();
    let committer = rubixdb::wal::GroupCommitter::new(wal).unwrap();

    let pool_config = ShardedIngressConfig {
        shard_count,
        queue_capacity_per_shard: ((writer_count * 4) / shard_count.max(1)).max(64),
        max_queued_bytes_per_shard: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(30),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_shard_per_cycle: (writer_count * 4).max(65536),
    };
    let pool = Arc::new(ShardedIngressPool::new(committer, pool_config).unwrap());

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
    println!("writer_count={writer_count} shard_count={shard_count} per_thread={per_thread} total={total}");
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
        stats.queue_depth_total
    );
    println!(
        "committer: sync_attempts={} sync_successes={} records_total={} max_batch_records={} avg_batch_records={:.2}",
        stats.committer_stats.sync_attempts,
        stats.committer_stats.sync_successes,
        stats.committer_stats.records_total,
        stats.committer_stats.max_batch_records,
        stats.committer_stats.avg_batch_records(),
    );
    println!(
        "drain_batches={} drain_entries_total={} mean_drain_size={:.2}",
        stats.drain_batches,
        stats.drain_entries_total,
        if stats.drain_batches > 0 {
            stats.drain_entries_total as f64 / stats.drain_batches as f64
        } else {
            0.0
        }
    );
    let denom = (stats.completed_ok + stats.completed_err).max(1) as f64;
    println!(
        "mean_queue_wait={:.3}ms mean_processing={:.3}ms",
        (stats.queue_wait_ns_total as f64 / denom) / 1_000_000.0,
        (stats.processing_ns_total as f64 / denom) / 1_000_000.0
    );

    let pool = Arc::try_unwrap(pool)
        .unwrap_or_else(|_| panic!("no other Arc<ShardedIngressPool> reference should remain here"));
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
