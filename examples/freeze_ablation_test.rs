//! Phase 5 release-gate ablation (operating brief: "the 100-writer
//! flush-versus-no-flush anomaly should either be conclusively explained
//! by a controlled freeze-frequency ablation or be explicitly documented
//! as an unresolved non-blocking observation").
//!
//! `PHASE4B_PERFORMANCE.md` §3.3 recorded an unexplained oddity: at 100
//! writers, `lsm_flush_load_test.rs` (small memtable, real SSTable
//! flush activity) measured *faster* than `lsm_load_test.rs` (large
//! memtable, never freezes) — the opposite of the naive "flush adds
//! overhead" expectation, and a candidate explanation floated but not
//! tested was "a small, frequently-replaced `BTreeMap` has bounded
//! insert depth, while one giant ever-growing `BTreeMap` does not."
//!
//! This harness isolates that one variable: it exercises the identical
//! write path (`BatchCoordinatorPool::submit` -> `Completion::wait()` ->
//! `MemTable::insert`) as `lsm_load_test.rs`/`lsm_flush_load_test.rs`,
//! with a small memtable that freezes just as often as the flush
//! variant does -- but a frozen memtable here is immediately **dropped**
//! (no SSTable write, no background thread, no disk I/O of any kind for
//! the frozen data). If this measures close to the *no-freeze* baseline,
//! freeze frequency itself is not the explanation, and the earlier
//! oddity remains genuinely unexplained. If it measures close to the
//! *real-flush* variant, freeze frequency (bounded `BTreeMap` size) is
//! the dominant effect, not flush I/O.
//!
//! Usage: `cargo run --release --example freeze_ablation_test --
//! <writer_count> [per_thread=1000] [memtable_bytes=65536]`

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::{BatchCoordinatorConfig, BatchCoordinatorPool};
use rubixdb::memtable::MemTable;
use rubixdb::wal::{FileWal, SyncMode, Wal, WalConfig, WalOpOwned};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_freeze_ablation_{tag}_{nanos}"));
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
            "usage: freeze_ablation_test <writer_count> [per_thread=1000] [memtable_bytes=65536]"
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

    let (file_wal, _replay) = FileWal::open_for_recovery(&dir, wal_config).unwrap();
    let committer = rubixdb::wal::GroupCommitter::new(file_wal).unwrap();
    let pool = Arc::new(BatchCoordinatorPool::new(committer, pool_config).unwrap());
    let active: Arc<RwLock<MemTable>> = Arc::new(RwLock::new(MemTable::new(memtable_bytes)));
    let freeze_count = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let total = writer_count * per_thread;
    let started = Instant::now();
    let handles: Vec<_> = (0..writer_count)
        .map(|t| {
            let pool = Arc::clone(&pool);
            let active = Arc::clone(&active);
            let freeze_count = Arc::clone(&freeze_count);
            thread::spawn(move || {
                let mut latencies_ns = Vec::with_capacity(per_thread);
                for i in 0..per_thread {
                    let key = format!("t{t}-{i}");
                    let op_started = Instant::now();
                    let completion = pool
                        .submit(WalOpOwned::Put {
                            key: key.into_bytes(),
                            value: b"v".to_vec(),
                        })
                        .expect("submit must succeed");
                    let position = completion.wait().expect("write must become durable");

                    let mut guard = active.write().unwrap_or_else(|p| p.into_inner());
                    guard.put(format!("t{t}-{i}").as_bytes(), position.seq, b"v");
                    if guard.is_full() {
                        // Freeze-and-discard: no SSTable, no background
                        // thread, no disk I/O for the frozen data -- the
                        // one variable this harness isolates.
                        *guard = MemTable::new(memtable_bytes);
                        freeze_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    drop(guard);

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

    println!(
        "writer_count={writer_count} per_thread={per_thread} total={total} memtable_bytes={memtable_bytes}"
    );
    println!(
        "elapsed={:.3}s ops_per_sec={:.0} freezes={}",
        elapsed.as_secs_f64(),
        total as f64 / elapsed.as_secs_f64(),
        freeze_count.load(std::sync::atomic::Ordering::Relaxed),
    );
    println!(
        "p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms",
        ms(percentile_ns(&all_latencies_ns, 0.50)),
        ms(percentile_ns(&all_latencies_ns, 0.95)),
        ms(percentile_ns(&all_latencies_ns, 0.99)),
        ms(*all_latencies_ns.last().unwrap_or(&0)),
    );

    pool.shutdown();
    drop(pool);
    let _ = fs::remove_dir_all(&dir);
}
