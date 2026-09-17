//! Phase 4B SSTable-flush crash-cycle child process (operating brief
//! §26). Spawned by `examples/sstable_flush_crash_test.rs`. Opens (or
//! recovers) an `LsmEngine` configured with a tiny memtable/block size so
//! freezes and background SSTable flushes happen continuously and
//! rapidly, then writes until killed externally — maximizing the chance
//! that an externally-timed kill lands somewhere inside the flush
//! pipeline (`PHASE4B_ARCHITECTURE.md` §5-§7): before `.sst.tmp`
//! creation, mid-block-write, mid-index/footer-write, before/after
//! fsync, before/during/after the atomic rename, or after full
//! publication.
//!
//! Usage: `sstable_flush_crash_child <dir> <writer_count>`

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
        eprintln!("usage: sstable_flush_crash_child <dir> <writer_count>");
        std::process::exit(2);
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let writer_count: usize = args[2].parse().unwrap();

    let wal_config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(2),
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
    // Deliberately tiny: forces a freeze (and therefore a flush attempt)
    // roughly every handful of writes, and a tiny block size forces many
    // small data blocks per SSTable -- both maximize how much of the
    // flush pipeline's own state machine an externally-timed kill has a
    // chance of landing inside.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 2048,
        max_immutable_memtables: 16,
        sstable_target_block_size: 256,
        ..LsmConfig::default()
    };
    let engine = Arc::new(
        LsmEngine::open(&dir, wal_config, pool_config, lsm_config)
            .expect("LsmEngine::open must succeed against whatever this directory already has"),
    );
    println!(
        "sstable_flush_crash_child: pid={} opened OK",
        std::process::id()
    );

    let mut handles = Vec::new();
    for t in 0..writer_count {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            let mut i: u64 = 0;
            loop {
                let key = format!("t{t}-{i}");
                let value = vec![b'x'; 64]; // big enough to fill blocks quickly
                let _ = engine.put(key.as_bytes(), &value);
                i += 1;
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}
