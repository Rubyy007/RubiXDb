//! Compaction — Increment 3 §12/§24 crash-cycle child process. Spawned
//! by `examples/compaction_crash_cycle_test.rs`. Opens (or recovers) an
//! `LsmEngine` with a tiny memtable and `compaction_auto_trigger: true`,
//! `compaction_trigger_count` small, so real automatic compaction
//! cycles happen continuously and rapidly through the real background
//! worker (never a manual `compact_once` call -- that entry point is
//! `pub(crate)` and unreachable from here by design,
//! `ADR-COMPACTION-001` Decision 13), then writes until killed
//! externally.
//!
//! Two modes:
//! - Random (no fault-point args): writers loop puts/deletes forever;
//!   the parent kills at a randomized delay -- broad, secondary crash
//!   endurance (brief §24).
//! - Targeted (`<fault_point_name> <occurrence>` given): installs a
//!   fault hook that, only on the `occurrence`-th firing of exactly
//!   that named `CompactionFaultPoint`, prints a stdout marker
//!   (`FAULT_HIT:<name>:<occurrence>`) and sleeps briefly -- never
//!   panics, never alters production behavior otherwise -- giving the
//!   parent a precise, real, externally-observed window in which to
//!   `Child::kill()` while execution is genuinely inside that exact
//!   compaction crash window (brief §12: "exercise every existing
//!   CompactionFaultPoint through the automatic path").
//!
//! Usage: `compaction_crash_cycle_child <dir> <writer_count>
//! [fault_point_name] [occurrence=1]`

use std::env;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{CompactionFaultPoint, LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

fn fault_point_by_name(name: &str) -> Option<CompactionFaultPoint> {
    match name {
        "BeforeOutputWrite" => Some(CompactionFaultPoint::BeforeOutputWrite),
        "BeforeManifestAdd" => Some(CompactionFaultPoint::BeforeManifestAdd),
        "AfterManifestAdd" => Some(CompactionFaultPoint::AfterManifestAdd),
        "DuringRemoveSequence" => Some(CompactionFaultPoint::DuringRemoveSequence),
        "AfterAllRemoves" => Some(CompactionFaultPoint::AfterAllRemoves),
        "BeforePhysicalDelete" => Some(CompactionFaultPoint::BeforePhysicalDelete),
        _ => None,
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: compaction_crash_cycle_child <dir> <writer_count> [fault_point_name] \
             [occurrence=1]"
        );
        std::process::exit(2);
    }
    let dir = std::path::PathBuf::from(&args[1]);
    let writer_count: usize = args[2].parse().unwrap();
    let target_point = args.get(3).and_then(|s| fault_point_by_name(s));
    let target_occurrence: u64 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(1);

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
    // Tiny memtable + low trigger_count: forces flush and compaction
    // cycles continuously and rapidly, maximizing the chance an
    // externally-timed kill lands inside the compaction pipeline.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 1024,
        max_immutable_memtables: 16,
        sstable_target_block_size: 256,
        compaction_trigger_count: 4,
        compaction_auto_trigger: true,
        storage_pressure_retry_interval: Duration::from_millis(200),
        ..LsmConfig::default()
    };
    let engine = Arc::new(
        LsmEngine::open(&dir, wal_config, pool_config, lsm_config)
            .expect("LsmEngine::open must succeed against whatever this directory already has"),
    );

    if let Some(point) = target_point {
        let occurrence_counter = Arc::new(AtomicU64::new(0));
        engine.install_compaction_fault_hook(move |p| {
            if p == point {
                let n = occurrence_counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == target_occurrence {
                    println!("FAULT_HIT:{point:?}:{n}");
                    let _ = std::io::stdout().flush();
                    thread::sleep(Duration::from_millis(500));
                }
            }
        });
    }

    println!(
        "compaction_crash_cycle_child: pid={} opened OK target_point={target_point:?} \
         target_occurrence={target_occurrence}",
        std::process::id()
    );
    let _ = std::io::stdout().flush();

    let mut handles = Vec::new();
    for t in 0..writer_count {
        let engine = Arc::clone(&engine);
        handles.push(thread::spawn(move || {
            let mut i: u64 = 0;
            loop {
                let key = format!("t{t}-{}", i % 200);
                if i.is_multiple_of(11) {
                    let _ = engine.delete(key.as_bytes());
                } else {
                    let value = vec![b'x'; 48];
                    let _ = engine.put(key.as_bytes(), &value);
                }
                i += 1;
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}
