//! `ADR-WE-SP-001` §17 crash-under-storage-pressure child process.
//! Spawned by `examples/storage_pressure_crash_test.rs`. Opens an
//! `LsmEngine` with a tiny memtable (freezes almost immediately) and an
//! injected flush I/O fault hook that unconditionally returns a
//! synthetic ENOSPC-shaped `io::Error` (`LsmEngine::
//! install_flush_io_fault_hook`, `ADR-WE-SP-001` §16) — so every flush
//! attempt fails the same way a real disk-full condition would be
//! classified, deterministically, without ever touching real disk
//! capacity. Writer threads `put` continuously; the main thread polls
//! `engine.storage_state()` and prints one marker line the first time
//! each state is reached, flushed immediately, so the parent can
//! synchronize a kill to land genuinely inside `STORAGE_PRESSURE`/
//! `STORAGE_FULL` instead of at an arbitrary, possibly-too-early wall-
//! clock delay.
//!
//! Usage: `storage_pressure_crash_child <dir> <writer_count>`

use std::env;
use std::io::{self, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine, StorageState};
use rubixdb::wal::{SyncMode, WalConfig};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: storage_pressure_crash_child <dir> <writer_count>");
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
    // Tiny memtable + tiny immutable backlog: freezes almost every put,
    // and reaches the StoragePressure -> StorageFull "safe resource
    // boundary" (ADR-WE-SP-001 §6.3) quickly. `max_flush_retries: 0` so
    // the very first injected failure already exhausts the fast-retry
    // budget and enters STORAGE_PRESSURE immediately, not after a 50ms*N
    // ramp.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 200,
        max_immutable_memtables: 2,
        max_flush_retries: 0,
        storage_pressure_retry_interval: Duration::from_millis(50),
        ..LsmConfig::default()
    };
    let engine = Arc::new(
        LsmEngine::open(&dir, wal_config, pool_config, lsm_config)
            .expect("LsmEngine::open must succeed against a fresh directory"),
    );
    // Unconditional synthetic ENOSPC -- every flush attempt fails the
    // same classified way (`EngineError::is_storage_exhausted`), forever,
    // for the lifetime of this process. This is deliberate: the point of
    // this test is "crash while genuinely stuck in storage pressure,"
    // not "crash during a brief transient blip."
    engine.install_flush_io_fault_hook(|| {
        Some(io::Error::new(
            io::ErrorKind::StorageFull,
            "injected ENOSPC (storage_pressure_crash_child, deterministic fault test)",
        ))
    });
    println!(
        "storage_pressure_crash_child: pid={} opened OK",
        std::process::id()
    );
    io::stdout().flush().ok();

    let mut handles = Vec::new();
    for t in 0..writer_count {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            let mut i: u64 = 0;
            loop {
                let key = format!("t{t}-{i}");
                let _ = engine.put(key.as_bytes(), b"storage-pressure-crash-value");
                i += 1;
            }
        }));
    }

    let marker_engine = Arc::clone(&engine);
    thread::spawn(move || {
        let mut last = StorageState::Healthy;
        loop {
            let current = marker_engine.storage_state();
            if current != last {
                println!("storage_pressure_crash_child: REACHED {current:?}");
                io::stdout().flush().ok();
                last = current;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });

    // Main thread's own job is done -- the writer threads and the marker
    // thread above each hold their own `Arc<LsmEngine>` clone, so the
    // engine stays alive regardless. Just block until externally killed.
    for h in handles {
        let _ = h.join();
    }
}
