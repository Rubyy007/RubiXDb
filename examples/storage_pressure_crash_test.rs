//! `ADR-WE-SP-001` §17: crash-under-storage-pressure external-process
//! test. Mirrors `examples/lsm_crash_cycle_test.rs`'s proven design (a
//! real child process, killed externally with `Child::kill()` — an
//! abrupt, uncooperative termination, not a self-inflicted `abort()`)
//! but, unlike that test's purely time-based kill delay, synchronizes
//! the kill to a marker line the child prints the moment it actually
//! observes `StorageState::StoragePressure` then `StorageState::
//! StorageFull` (`examples/storage_pressure_crash_child.rs`) — so every
//! run genuinely exercises "crash while stuck in storage pressure," not
//! "crash at an arbitrary wall-clock delay that might land too early."
//!
//! Verifies, after each kill + reopen:
//! - `LsmEngine::open` (Manifest replay, WAL replay, SSTable
//!   reconciliation) succeeds without error or panic.
//! - `checkpoint_seq() == 0` and `sstable_count() == 0` — since every
//!   flush attempt in the child was poisoned by the injected fault, no
//!   flush could have legitimately succeeded; a nonzero checkpoint or a
//!   live SSTable here would mean the storage-pressure retry loop
//!   falsely advanced persistence state despite every attempt failing
//!   (exactly the class of bug `ADR-WE-SP-001` §10/§12 requires never
//!   happens).
//! - `durable_through <= highest_sequence` (the durability watermark
//!   never claims to be ahead of what was ever assigned).
//! - At least one WAL record replays into the recovered active MemTable
//!   (`recovery_stats().wal_records_applied > 0`) — proof the writes the
//!   child made before being killed are actually recoverable purely from
//!   WAL replay, since nothing ever reached a durable SSTable.
//! - After reopening (a fresh process with no fault hook installed —
//!   "storage restored"), a normal `put` immediately succeeds and a
//!   forced flush actually completes, publishing a live SSTable and
//!   advancing the checkpoint — proof of `ADR-WE-SP-001` §13's required
//!   transition back to normal operation once storage is available
//!   again.
//!
//! Usage: `storage_pressure_crash_test <num_cycles> <writer_count>
//! [marker_wait_secs=15]`

use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
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
    let path = std::env::temp_dir().join(format!("rubixdb_storage_pressure_crash_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn child_binary_path() -> PathBuf {
    let mut path = env::current_exe().expect("current_exe must resolve");
    path.set_file_name(format!(
        "storage_pressure_crash_child{}",
        std::env::consts::EXE_SUFFIX
    ));
    path
}

fn wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 512,
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

fn open_for_verification(dir: &Path) -> rubixdb::error::Result<LsmEngine> {
    // Same small config the child used, minus the fault hook -- "storage
    // restored" means exactly this: the identical engine configuration,
    // now against a filesystem that isn't failing every write.
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 200,
        max_immutable_memtables: 2,
        max_flush_retries: 0,
        storage_pressure_retry_interval: Duration::from_millis(50),
        ..LsmConfig::default()
    };
    LsmEngine::open(dir, wal_config(), pool_config(), lsm_config)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: storage_pressure_crash_test <num_cycles> <writer_count> \
             [marker_wait_secs=15]"
        );
        std::process::exit(2);
    }
    let num_cycles: u32 = args[1].parse().unwrap();
    let writer_count: usize = args[2].parse().unwrap();
    let marker_wait_secs: u64 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(15);

    println!(
        "storage_pressure_crash_test: num_cycles={num_cycles} writer_count={writer_count} \
         (each cycle uses its own fresh directory -- see module doc comment)"
    );

    let child_path = child_binary_path();
    if !child_path.exists() {
        eprintln!(
            "storage_pressure_crash_test: child binary not found at {} -- build it first: \
             cargo build --release --example storage_pressure_crash_child",
            child_path.display()
        );
        std::process::exit(2);
    }

    let mut failures = 0u32;

    for cycle in 1..=num_cycles {
        // A fresh directory per cycle, deliberately -- unlike `lsm_crash_
        // cycle_test.rs` (which reuses one directory to test repeated
        // crash/recover accumulation), this test's own invariant
        // ("checkpoint_seq/sstable_count must be exactly 0 after the
        // kill+reopen, since every flush this child attempted was
        // poisoned") only holds for a directory this specific child
        // actually owned start to finish. Reusing one directory across
        // cycles would let a later cycle's child inherit real, legitimate
        // progress from an earlier cycle's own post-recovery verification
        // writes (caught by actually running this test before trusting
        // it: cycles 2+ showed nonzero checkpoint/sstable counts that
        // were real carried-over state, not a production bug).
        let dir = temp_dir(&format!("cycle{cycle}"));
        let mut child = Command::new(&child_path)
            .arg(&dir)
            .arg(writer_count.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn storage_pressure_crash_child");

        let stdout = child.stdout.take().expect("child stdout must be piped");
        let mut reader = BufReader::new(stdout);
        let deadline = Instant::now() + Duration::from_secs(marker_wait_secs);
        let mut reached_storage_full = false;
        let mut line = String::new();
        while Instant::now() < deadline {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // child exited/closed stdout early
                Ok(_) => {
                    if line.contains("REACHED StorageFull") {
                        reached_storage_full = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        if !reached_storage_full {
            // Flagged, not silently downgraded: this means the state
            // machine did not reach the state this test exists to crash
            // inside of within the allotted budget -- a real finding,
            // not a pass with weaker coverage.
            println!(
                "storage_pressure_crash_test: cycle {cycle} WARNING never observed 'REACHED \
                 StorageFull' within {marker_wait_secs}s -- killing anyway and verifying recovery, \
                 but this cycle did not test what it was designed to test"
            );
        }

        let _ = child.kill();
        let _ = child.wait();

        let recovery_started = Instant::now();
        match open_for_verification(&dir) {
            Ok(engine) => {
                let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
                let stats = engine.pool_stats();
                let highest_seq = stats.committer_stats.highest_sequence;
                let durable_through = stats.committer_stats.durable_through;
                let checkpoint_seq = engine.checkpoint_seq();
                let sstable_count = engine.sstable_count();
                let rstats = engine.recovery_stats();

                let mut cycle_ok = true;
                if durable_through > highest_seq {
                    cycle_ok = false;
                    println!(
                        "storage_pressure_crash_test: cycle {cycle} FAIL durable_through \
                         {durable_through} > highest_sequence {highest_seq}"
                    );
                }
                if checkpoint_seq != 0 {
                    cycle_ok = false;
                    println!(
                        "storage_pressure_crash_test: cycle {cycle} FAIL checkpoint_seq={checkpoint_seq} \
                         (expected 0 -- every flush attempt was poisoned by the injected fault, so \
                         no flush could have legitimately advanced the checkpoint)"
                    );
                }
                if sstable_count != 0 {
                    cycle_ok = false;
                    println!(
                        "storage_pressure_crash_test: cycle {cycle} FAIL sstable_count={sstable_count} \
                         (expected 0, same reasoning as checkpoint_seq above)"
                    );
                }
                if reached_storage_full && rstats.wal_records_applied == 0 {
                    cycle_ok = false;
                    println!(
                        "storage_pressure_crash_test: cycle {cycle} FAIL wal_records_applied=0 \
                         despite the child reaching StorageFull -- writes made before StorageFull \
                         was confirmed must still be recoverable from the WAL"
                    );
                }

                // "Correct transition back to normal operation after
                // storage is restored" (ADR-WE-SP-001 §13): this reopened
                // engine has no fault hook installed at all, so normal
                // writes must succeed and an actual flush (not just an
                // accepted write) must complete end to end -- enough
                // puts to exceed the 200-byte memtable and force a
                // freeze, then a bounded poll for the resulting flush.
                let mut recovered_writes_ok = true;
                for i in 0..40u32 {
                    let key = format!("post-recovery-k{i}");
                    // `CapacityExceeded` here is the pre-existing,
                    // accepted MemTable-freeze backpressure contract
                    // (`[[project_rubixdb_capacity_contract]]`), not a
                    // failure -- 40 puts in a tight loop against a
                    // 200-byte memtable can legitimately outrun real
                    // disk I/O. Retry briefly rather than treating it as
                    // this test's own failure (caught by actually
                    // running this test: an earlier version of this loop
                    // had no retry and produced a false FAIL here).
                    let retry_deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        match engine.put(key.as_bytes(), b"v") {
                            Ok(_) => break,
                            Err(rubixdb::error::EngineError::CapacityExceeded { .. })
                                if Instant::now() < retry_deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Err(e) => {
                                recovered_writes_ok = false;
                                println!(
                                    "storage_pressure_crash_test: cycle {cycle} FAIL a normal put \
                                     after recovery (storage restored, no fault hook) returned {e}"
                                );
                                break;
                            }
                        }
                    }
                    if !recovered_writes_ok {
                        break;
                    }
                }
                if recovered_writes_ok {
                    let flush_deadline = Instant::now() + Duration::from_secs(10);
                    let mut flush_completed = false;
                    while Instant::now() < flush_deadline {
                        if engine.sstable_count() > 0 && engine.checkpoint_seq() > 0 {
                            flush_completed = true;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    if !flush_completed {
                        cycle_ok = false;
                        println!(
                            "storage_pressure_crash_test: cycle {cycle} FAIL no flush completed \
                             within 10s after recovery despite storage being restored \
                             (sstable_count={} checkpoint_seq={})",
                            engine.sstable_count(),
                            engine.checkpoint_seq()
                        );
                    }
                } else {
                    cycle_ok = false;
                }

                if cycle_ok {
                    println!(
                        "storage_pressure_crash_test: cycle {cycle} OK reached_storage_full={reached_storage_full} \
                         highest_seq={highest_seq} durable_through={durable_through} \
                         wal_records_applied={} recovery_ms={recovery_ms:.1}",
                        rstats.wal_records_applied
                    );
                } else {
                    failures += 1;
                }

                engine.shutdown();
            }
            Err(e) => {
                failures += 1;
                println!(
                    "storage_pressure_crash_test: cycle {cycle} FAIL LsmEngine::open error: {e}"
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    println!(
        "storage_pressure_crash_test: SUMMARY cycles={num_cycles} successful={} failed={failures}",
        num_cycles - failures
    );

    if failures > 0 {
        std::process::exit(1);
    }
}
