//! Phase 3C periodic-crash-during-soak child process (operating brief
//! §5-§6, `PHASE3C_TEST_PLAN.md`). Spawned by `examples/crash_cycle_
//! test.rs`, one process per crash cycle. Opens/recovers the given WAL
//! directory, runs the production `BatchCoordinatorPool` under
//! concurrent load indefinitely, and prints its own durable_through
//! periodically — it never exits on its own; the parent kills it
//! (`Child::kill()`, `TerminateProcess` on Windows — an abrupt,
//! external, asynchronous kill, not a self-inflicted `abort()` at a
//! code-chosen point, so it can land truly anywhere, including mid-
//! syscall) at a randomized, reproducible point.
//!
//! Usage: `crash_cycle_child <wal_dir> <writer_count>`

use std::env;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rubixdb::execution::batch_coordinator::{BatchCoordinatorConfig, BatchCoordinatorPool};
use rubixdb::wal::{FileWal, SyncMode, Wal, WalConfig, WalOpOwned};
use rubixdb::EngineError;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: crash_cycle_child <wal_dir> <writer_count>");
        std::process::exit(2);
    }
    let wal_dir = &args[1];
    let writer_count: usize = args[2].parse().expect("writer_count must be a usize");

    let config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let (wal, replay) = FileWal::open_for_recovery(std::path::Path::new(wal_dir), config).unwrap();
    println!(
        "crash_cycle_child: pid={} recovered_on_open={} corrupted_segments={}",
        std::process::id(),
        replay.records.len(),
        replay.corrupted_segments.len()
    );
    let committer = rubixdb::wal::GroupCommitter::new(wal).unwrap();
    let pool_config = BatchCoordinatorConfig {
        queue_capacity: (writer_count * 4).max(64),
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: (writer_count * 4).max(4096),
    };
    let pool = Arc::new(BatchCoordinatorPool::new(committer, pool_config).unwrap());

    for t in 0..writer_count {
        let pool = Arc::clone(&pool);
        thread::spawn(move || {
            let mut i: u64 = 0;
            loop {
                let key = format!("t{t}-{i}");
                let op = WalOpOwned::Put {
                    key: key.into_bytes(),
                    value: b"crash-cycle-value".to_vec(),
                };
                match pool.submit(op) {
                    Ok(completion) => {
                        let _ = completion.wait();
                    }
                    Err(EngineError::Timeout { .. }) => {}
                    Err(_) => {}
                }
                i += 1;
            }
        });
    }

    // Main thread: print durable_through periodically so the parent's
    // own log has a rough cross-check of "what this process believed
    // was durable" just before it was killed — not load-bearing for
    // correctness (the parent's post-kill recovery scan is the actual
    // ground truth), purely diagnostic.
    loop {
        thread::sleep(Duration::from_millis(200));
        let stats = pool.stats();
        println!(
            "crash_cycle_child: durable_through={} highest_sequence={} completed_ok={}",
            stats.committer_stats.durable_through,
            stats.committer_stats.highest_sequence,
            stats.completed_ok
        );
    }
}
