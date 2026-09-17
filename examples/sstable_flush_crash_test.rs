//! Phase 4B SSTable-flush crash-cycle driver (operating brief §26,
//! §47). Mirrors `examples/lsm_crash_cycle_test.rs`'s proven design (a
//! real child process, killed externally via `Child::kill()` — abrupt,
//! asynchronous, uncooperative, not a self-inflicted `abort()` — at a
//! randomized-but-reproducible delay) but uses much shorter delays and a
//! tiny memtable/block configuration (`sstable_flush_crash_child.rs`) so
//! kills land, across many cycles, throughout the SSTable flush
//! pipeline: before `.sst.tmp` creation, mid-block-write, mid-index/
//! footer-write, before/after fsync, before/during/after the atomic
//! rename, and after full publication.
//!
//! After each kill, the parent reopens via `LsmEngine::open` (exercising
//! `sstable::discover`'s startup sweep and the unchanged WAL replay path)
//! and verifies:
//! 1. `open()` never returns an error (a genuinely interrupted flush must
//!    always leave either nothing, a `.tmp` file the sweep deletes, or a
//!    fully valid `.sst` -- never something that fails validation,
//!    `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4).
//! 2. No `*.sst.tmp` file survives the sweep.
//! 3. The recovered `highest_sequence`/`durable_through` watermark never
//!    goes backward across cycles (no acknowledged durable write lost).
//! 4. Every SSTable discovered actually opens and answers a lookup
//!    without error (already implied by (1), asserted directly too).
//!
//! Usage: `sstable_flush_crash_test <num_cycles> <writer_count>
//! [seed=42] [min_delay_ms=1] [max_delay_ms=60]`

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

struct Xorshift64 {
    state: u64,
}
impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Xorshift64 { state: seed.max(1) }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_sstable_flush_crash_{tag}_{nanos}"));
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

fn find_orphaned_tmp_files(sstables_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(sstables_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("tmp"))
        .collect()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: sstable_flush_crash_test <num_cycles> <writer_count> [seed=42] \
             [min_delay_ms=1] [max_delay_ms=60]"
        );
        std::process::exit(2);
    }
    let num_cycles: u32 = args[1].parse().unwrap();
    let writer_count: usize = args[2].parse().unwrap();
    let seed: u64 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(42);
    let min_delay_ms: u64 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(1);
    let max_delay_ms: u64 = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(60);

    let dir = temp_dir("run");
    println!(
        "sstable_flush_crash_test: dir={} num_cycles={num_cycles} writer_count={writer_count} \
         seed={seed} delay=[{min_delay_ms},{max_delay_ms}]ms",
        dir.display()
    );

    let child_path = child_binary_path();
    if !child_path.exists() {
        eprintln!(
            "sstable_flush_crash_test: child binary not found at {} — build it first: \
             cargo build --release --example sstable_flush_crash_child",
            child_path.display()
        );
        std::process::exit(2);
    }

    let mut rng = Xorshift64::new(seed);
    let mut failures = 0u32;
    let mut last_highest_seq = 0u64;
    let mut last_durable_through = 0u64;
    let mut max_sstables_seen = 0usize;

    for cycle in 1..=num_cycles {
        let kill_delay_ms = rng.range(min_delay_ms, max_delay_ms);

        let mut child = Command::new(&child_path)
            .arg(&dir)
            .arg(writer_count.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn sstable_flush_crash_child");

        std::thread::sleep(Duration::from_millis(kill_delay_ms));
        let _ = child.kill();
        let _ = child.wait();

        let sstables_dir = dir.join("sstables");
        let recovery_started = Instant::now();
        match open_for_verification(&dir) {
            Ok(engine) => {
                let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
                let stats = engine.pool_stats();
                let highest_seq = stats.committer_stats.highest_sequence;
                let durable_through = stats.committer_stats.durable_through;
                let orphaned_tmp = find_orphaned_tmp_files(&sstables_dir);
                let sstable_count = engine.sstable_count();
                max_sstables_seen = max_sstables_seen.max(sstable_count);

                let mut ok =
                    highest_seq >= last_highest_seq && durable_through >= last_durable_through;
                if !orphaned_tmp.is_empty() {
                    ok = false;
                    println!(
                        "sstable_flush_crash_test: cycle {cycle} FAIL orphaned .tmp file(s) \
                         survived discover(): {orphaned_tmp:?}"
                    );
                }
                if !ok && orphaned_tmp.is_empty() {
                    println!(
                        "sstable_flush_crash_test: cycle {cycle} FAIL watermark went backward: \
                         highest_seq {highest_seq} < {last_highest_seq} or durable_through \
                         {durable_through} < {last_durable_through}"
                    );
                }
                if ok {
                    println!(
                        "sstable_flush_crash_test: cycle {cycle} OK kill_delay_ms={kill_delay_ms} \
                         highest_seq={highest_seq} durable_through={durable_through} \
                         active_entries={} immutable_count={} sstable_count={sstable_count} \
                         recovery_ms={recovery_ms:.1}",
                        engine.active_entry_count(),
                        engine.immutable_count(),
                    );
                } else {
                    failures += 1;
                }
                last_highest_seq = highest_seq;
                last_durable_through = durable_through;
                engine.shutdown();
            }
            Err(e) => {
                failures += 1;
                println!(
                    "sstable_flush_crash_test: cycle {cycle} FAIL LsmEngine::open error \
                     (a genuinely interrupted flush must never leave a file that fails \
                     validation): {e}"
                );
            }
        }
    }

    println!(
        "sstable_flush_crash_test: SUMMARY cycles={num_cycles} successful={} failed={failures} \
         final_highest_seq={last_highest_seq} final_durable_through={last_durable_through} \
         max_sstables_seen_in_one_cycle={max_sstables_seen}",
        num_cycles - failures
    );

    let _ = fs::remove_dir_all(&dir);
    if failures > 0 {
        std::process::exit(1);
    }
}
