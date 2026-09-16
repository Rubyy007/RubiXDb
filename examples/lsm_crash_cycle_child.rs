//! Phase 4A WAL/MemTable-boundary crash-cycle child process (operating
//! brief §26). Spawned by `examples/lsm_crash_cycle_test.rs`. Opens (or
//! recovers) an `LsmEngine` at the given directory and runs `put` calls
//! continuously across several threads — each `put` exercises the full
//! WAL-append -> WAL-durability -> MemTable-apply chain
//! (`PHASE4A_ARCHITECTURE.md` §5) — until killed externally.
//!
//! Usage: `lsm_crash_cycle_child <dir> <writer_count>`

use std::env;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: lsm_crash_cycle_child <dir> <writer_count>");
        std::process::exit(2);
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let writer_count: usize = args[2].parse().unwrap();

    let wal_config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let pool_config = BatchCoordinatorConfig {
        queue_capacity: 512,
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    };
    let engine = Arc::new(
        LsmEngine::open(&dir, wal_config, pool_config, LsmConfig::default())
            .expect("LsmEngine::open must succeed against whatever this directory already has"),
    );
    println!(
        "lsm_crash_cycle_child: pid={} opened OK",
        std::process::id()
    );

    let mut handles = Vec::new();
    for t in 0..writer_count {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            let mut i: u64 = 0;
            loop {
                let key = format!("t{t}-{i}");
                let _ = engine.put(key.as_bytes(), b"lsm-crash-cycle-value");
                i += 1;
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}
