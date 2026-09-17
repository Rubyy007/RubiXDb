//! Phase 4B performance harness (operating brief §40-41): measures the
//! production write path with a small enough `memtable_max_size_bytes`
//! that background SSTable flushing is continuously active throughout
//! the run — the "WAL + MemTable + SSTable flush" leg of the three-way
//! comparison against `examples/batch_coordinator_load_test.rs`
//! (WAL-only) and `examples/lsm_load_test.rs` (WAL + MemTable, flush
//! never triggered). Same methodology (writer-count/per-thread
//! arguments, percentile reporting) as both, so all three are directly
//! comparable.
//!
//! Usage: `cargo run --release --example lsm_flush_load_test --
//! <writer_count> [per_thread=1000] [memtable_bytes=65536]`

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_lsm_flush_load_test_{tag}_{nanos}"));
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

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: lsm_flush_load_test <writer_count> [per_thread=1000] [memtable_bytes=65536]"
        );
        std::process::exit(2);
    }
    let writer_count: usize = args[1].parse().unwrap();
    let per_thread: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(1000);
    let memtable_bytes: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(65536);

    let dir = temp_dir("run");
    let wal_config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let pool_config = BatchCoordinatorConfig {
        queue_capacity: (writer_count * 4).max(64),
        max_queued_bytes: 64 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(30),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: (writer_count * 4).max(65536),
    };
    // Deliberately small: continuously triggers freeze + background
    // flush throughout the run, so this harness measures the
    // *incremental* cost of real, concurrent SSTable-flush disk I/O
    // contending with the write path -- unlike `lsm_load_test.rs`'s
    // large memtable, which never exercises flush at all.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: memtable_bytes,
        max_immutable_memtables: 64,
        ..LsmConfig::default()
    };
    let engine = Arc::new(LsmEngine::open(&dir, wal_config, pool_config, lsm_config).unwrap());

    let total = writer_count * per_thread;
    let started = Instant::now();
    let handles: Vec<_> = (0..writer_count)
        .map(|t| {
            let engine = Arc::clone(&engine);
            thread::spawn(move || {
                let mut latencies_ns = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let key = format!("t{t}-{i}");
                    let op_started = Instant::now();
                    engine
                        .put(key.as_bytes(), b"v")
                        .expect("write must become durable and applied");
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

    println!("writer_count={writer_count} per_thread={per_thread} total={total} memtable_bytes={memtable_bytes}");
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

    // Wait for the flush thread to fully drain so the final sstable_count
    // reflects the true total rather than a mid-drain snapshot.
    let drain_deadline = Instant::now() + Duration::from_secs(60);
    while engine.immutable_count() > 0 && Instant::now() < drain_deadline {
        thread::sleep(Duration::from_millis(20));
    }

    println!(
        "active_entries={} immutable_count={} sstable_count={} next_sstable_id={}",
        engine.active_entry_count(),
        engine.immutable_count(),
        engine.sstable_count(),
        engine.next_sstable_id(),
    );

    let stats = engine.pool_stats();
    println!(
        "committer: sync_attempts={} records_total={} avg_batch_records={:.2}",
        stats.committer_stats.sync_attempts,
        stats.committer_stats.records_total,
        stats.committer_stats.avg_batch_records(),
    );

    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}
