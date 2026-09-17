//! Phase 5 bounded storage-engine soak with periodic controlled crashes
//! (operating brief: "add a long-duration storage-engine soak after
//! Manifest integration... add periodic controlled crashes during the
//! Manifest-enabled soak"). **Explicitly bounded, not the multi-hour
//! Phase 3C-style soak** — this exercises sustained writes, periodic
//! MemTable freezes, SSTable creation, Manifest updates, and WAL
//! retention over several minutes with real external-process kills
//! interleaved, not hours; the true long-duration WAL-only soak remains
//! `examples/long_soak_test.rs`'s own separate responsibility
//! (`PHASE5_ADR.md` ADR-P5-0).
//!
//! Reuses `sstable_flush_crash_child.rs` unchanged as the sustained-
//! write workload (tiny memtable/block config -> continuous freeze ->
//! flush -> checkpoint -> purge activity), but lets each cycle run
//! substantially longer than `sstable_flush_crash_test.rs`'s own
//! sub-100ms kills before killing it, so growth/stability trends are
//! actually observable across a cycle, not just crash-timing coverage.
//!
//! After each cycle: opens Manifest, discovers live SSTables, replays
//! WAL from the checkpoint, rebuilds the active MemTable, verifies
//! logical state (the same watermark/checkpoint/bounded-replay
//! invariants `sstable_flush_crash_test.rs` checks), and continues the
//! workload. Tracks WAL directory size, Manifest file size, SSTable
//! count, and checkpoint progression across cycles to demonstrate WAL
//! storage does not grow without bound while checkpointing succeeds.
//!
//! Usage: `manifest_soak_test <num_cycles> <writer_count>
//! [cycle_duration_secs=20]`

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_manifest_soak_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn child_binary_path() -> PathBuf {
    let mut path = env::current_exe().expect("current_exe must resolve");
    path.set_file_name(format!(
        "sstable_flush_crash_child{}",
        std::env::consts::EXE_SUFFIX
    ));
    path
}

fn lsm_config() -> LsmConfig {
    LsmConfig {
        memtable_max_size_bytes: 2048,
        max_immutable_memtables: 16,
        sstable_target_block_size: 256,
        ..LsmConfig::default()
    }
}

fn open_for_verification(dir: &Path) -> rubixdb::error::Result<LsmEngine> {
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
    LsmEngine::open(dir, wal_config, pool_config, lsm_config())
}

fn dir_size_bytes(dir: &Path, predicate: impl Fn(&str) -> bool) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_str().map(&predicate).unwrap_or(false))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn wal_size_bytes(dir: &Path) -> u64 {
    dir_size_bytes(dir, |n| n.starts_with("wal-") && n.ends_with(".log"))
}

fn manifest_size_bytes(dir: &Path) -> u64 {
    fs::metadata(dir.join("MANIFEST"))
        .map(|m| m.len())
        .unwrap_or(0)
}

fn sstables_dir_size_bytes(dir: &Path) -> u64 {
    dir_size_bytes(&dir.join("sstables"), |n| n.ends_with(".sst"))
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: manifest_soak_test <num_cycles> <writer_count> [cycle_duration_secs=20]");
        std::process::exit(2);
    }
    let num_cycles: u32 = args[1].parse().unwrap();
    let writer_count: usize = args[2].parse().unwrap();
    let cycle_duration_secs: u64 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(20);

    let dir = temp_dir("run");
    println!(
        "manifest_soak_test: dir={} num_cycles={num_cycles} writer_count={writer_count} \
         cycle_duration_secs={cycle_duration_secs} (BOUNDED soak, not the multi-hour Phase 3C kind)",
        dir.display()
    );

    let child_path = child_binary_path();
    if !child_path.exists() {
        eprintln!(
            "manifest_soak_test: child binary not found at {} — build it first: \
             cargo build --release --example sstable_flush_crash_child",
            child_path.display()
        );
        std::process::exit(2);
    }

    let mut failures = 0u32;
    let mut last_highest_seq = 0u64;
    let mut last_checkpoint_seq = 0u64;
    let soak_started = Instant::now();

    for cycle in 1..=num_cycles {
        let mut child = Command::new(&child_path)
            .arg(&dir)
            .arg(writer_count.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn sstable_flush_crash_child");

        std::thread::sleep(Duration::from_secs(cycle_duration_secs));
        let _ = child.kill();
        let _ = child.wait();

        match open_for_verification(&dir) {
            Ok(engine) => {
                let stats = engine.pool_stats();
                let highest_seq = stats.committer_stats.highest_sequence;
                let checkpoint_seq = engine.checkpoint_seq();
                let recovery_stats = engine.recovery_stats();
                let active_entries = engine.active_entry_count();
                let expected_active_entries = highest_seq
                    .saturating_sub(checkpoint_seq)
                    .saturating_sub(recovery_stats.checkpoint_markers_replayed);

                let mut ok =
                    highest_seq >= last_highest_seq && checkpoint_seq >= last_checkpoint_seq;
                if active_entries as u64 != expected_active_entries {
                    ok = false;
                    println!(
                        "manifest_soak_test: cycle {cycle} FAIL bounded-replay invariant: \
                         active_entries={active_entries} != expected={expected_active_entries}"
                    );
                }

                let wal_bytes = wal_size_bytes(&dir);
                let manifest_bytes = manifest_size_bytes(&dir);
                let sstables_bytes = sstables_dir_size_bytes(&dir);

                println!(
                    "manifest_soak_test: cycle {cycle} {} t={:.0}s highest_seq={highest_seq} \
                     checkpoint_seq={checkpoint_seq} sstable_count={} wal_bytes={wal_bytes} \
                     manifest_bytes={manifest_bytes} sstables_bytes={sstables_bytes} \
                     recovery_ms={:.1}",
                    if ok { "OK" } else { "FAIL" },
                    soak_started.elapsed().as_secs_f64(),
                    engine.sstable_count(),
                    recovery_stats.recovery_duration.as_secs_f64() * 1000.0,
                );

                if !ok {
                    failures += 1;
                }
                last_highest_seq = highest_seq;
                last_checkpoint_seq = checkpoint_seq;
                engine.shutdown();
            }
            Err(e) => {
                failures += 1;
                println!("manifest_soak_test: cycle {cycle} FAIL LsmEngine::open error: {e}");
            }
        }
    }

    println!(
        "manifest_soak_test: SUMMARY cycles={num_cycles} successful={} failed={failures} \
         final_highest_seq={last_highest_seq} final_checkpoint_seq={last_checkpoint_seq} \
         total_duration_s={:.0}",
        num_cycles - failures,
        soak_started.elapsed().as_secs_f64(),
    );

    let _ = fs::remove_dir_all(&dir);
    if failures > 0 {
        std::process::exit(1);
    }
}
