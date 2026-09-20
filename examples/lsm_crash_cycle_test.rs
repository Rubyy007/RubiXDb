//! Phase 4A WAL/MemTable-boundary crash-cycle driver (operating brief
//! §26). Mirrors `examples/crash_cycle_test.rs`'s own proven design
//! (Phase 3C, `PHASE3C_ADR.md` ADR-P3C-3) — a real child process,
//! killed externally (`Child::kill()`, an abrupt, asynchronous,
//! uncooperative termination, not a self-inflicted `abort()`) at a
//! randomized-but-reproducible (seeded, `std`-only PRNG) delay — but
//! points it at `LsmEngine` instead of a bare `FileWal`, so every kill
//! genuinely lands somewhere in the WAL-append -> WAL-durability ->
//! MemTable-apply chain (`PHASE4A_ARCHITECTURE.md` §5), not just the WAL
//! layer alone. After each kill, the parent reopens via `LsmEngine::
//! open` (exercising its own real recovery path, `wal::replay_
//! streaming`) and verifies: recovery itself never panics/errors, and
//! the recovered `durable_through`/`highest_sequence` watermark never
//! goes backward across cycles (no acknowledged durable data lost).
//!
//! Usage: `lsm_crash_cycle_test <num_cycles> <writer_count> [seed=42]
//! [min_delay_ms=100] [max_delay_ms=1500]`

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
    let path = std::env::temp_dir().join(format!("rubixdb_lsm_crash_cycle_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn child_binary_path() -> PathBuf {
    let mut path = env::current_exe().expect("current_exe must resolve");
    path.set_file_name(format!(
        "lsm_crash_cycle_child{}",
        std::env::consts::EXE_SUFFIX
    ));
    path
}

/// Increment 4 (`PHASE_READ_ENGINE_ADR.md`, brief §15/§16) extension:
/// the fixed value every `lsm_crash_cycle_child` write uses, so a
/// present key's value can be checked for *exact* equality, not just
/// existence -- closing the brief's explicit "do not merely verify
/// that startup returned Ok" gap on this already-existing crash-cycle
/// harness, reused rather than duplicated.
const EXPECTED_VALUE: &[u8] = b"lsm-crash-cycle-value";

/// Real `get`/`contains`/`range` verification after a crash+recovery
/// cycle, against the exact fixed value every write in this harness
/// uses. A key that was never durably written is legitimately absent
/// (the crash could have landed before or after any given key's
/// write) -- that is not itself a failure; only a *present* key with
/// the wrong value, or a disagreement between `get` and `contains`,
/// counts as one. Returns the number of real mismatches found.
fn verify_reads_after_recovery(
    engine: &LsmEngine,
    writer_count: usize,
    sample_per_writer: u64,
) -> u64 {
    let mut mismatches = 0u64;
    for w in 0..writer_count {
        for i in 0..sample_per_writer {
            let key = format!("t{w}-{i}");
            match engine.get(key.as_bytes()) {
                Ok(Some(v)) => {
                    if v != EXPECTED_VALUE {
                        mismatches += 1;
                        println!(
                            "lsm_crash_cycle_test: READ MISMATCH get({key}) = {v:?}, expected \
                             {EXPECTED_VALUE:?}"
                        );
                    }
                }
                Ok(None) => {} // legitimately never written before the kill -- not a failure.
                Err(e) => {
                    mismatches += 1;
                    println!("lsm_crash_cycle_test: READ ERROR get({key}): {e}");
                }
            }
            match engine.contains(key.as_bytes(), u64::MAX) {
                Ok(found) => {
                    let expected_found = engine
                        .get(key.as_bytes())
                        .map(|v| v.is_some())
                        .unwrap_or(false);
                    if found != expected_found {
                        mismatches += 1;
                        println!(
                            "lsm_crash_cycle_test: READ MISMATCH contains({key})={found} \
                             disagrees with get().is_some()={expected_found}"
                        );
                    }
                }
                Err(e) => {
                    mismatches += 1;
                    println!("lsm_crash_cycle_test: READ ERROR contains({key}): {e}");
                }
            }
        }
    }

    // A bounded range scan across the first writer's keyspace slice --
    // every row returned must carry exactly the fixed value, and the
    // scan itself must not error or panic against whatever partial
    // state the kill left behind.
    let start = b"t0-".to_vec();
    let end = b"t0-~".to_vec(); // '~' sorts after ASCII digits, bounding the scan.
    match engine
        .range(
            std::ops::Bound::Included(start.as_slice()),
            std::ops::Bound::Excluded(end.as_slice()),
        )
        .collect::<rubixdb::error::Result<Vec<_>>>()
    {
        Ok(rows) => {
            for (k, v) in &rows {
                if v != EXPECTED_VALUE {
                    mismatches += 1;
                    println!(
                        "lsm_crash_cycle_test: READ MISMATCH range() row {k:?} = {v:?}, \
                         expected {EXPECTED_VALUE:?}"
                    );
                }
            }
        }
        Err(e) => {
            mismatches += 1;
            println!("lsm_crash_cycle_test: READ ERROR range() over t0- slice: {e}");
        }
    }

    mismatches
}

fn open_for_verification(dir: &Path) -> rubixdb::error::Result<LsmEngine> {
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
    LsmEngine::open(dir, wal_config, pool_config, LsmConfig::default())
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: lsm_crash_cycle_test <num_cycles> <writer_count> [seed=42] \
             [min_delay_ms=100] [max_delay_ms=1500]"
        );
        std::process::exit(2);
    }
    let num_cycles: u32 = args[1].parse().unwrap();
    let writer_count: usize = args[2].parse().unwrap();
    let seed: u64 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(42);
    let min_delay_ms: u64 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(100);
    let max_delay_ms: u64 = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(1500);

    let dir = temp_dir("run");
    println!(
        "lsm_crash_cycle_test: dir={} num_cycles={num_cycles} writer_count={writer_count} \
         seed={seed}",
        dir.display()
    );

    let child_path = child_binary_path();
    if !child_path.exists() {
        eprintln!(
            "lsm_crash_cycle_test: child binary not found at {} — build it first: \
             cargo build --release --example lsm_crash_cycle_child",
            child_path.display()
        );
        std::process::exit(2);
    }

    let mut rng = Xorshift64::new(seed);
    let mut failures = 0u32;
    let mut last_highest_seq = 0u64;
    let mut last_durable_through = 0u64;
    let mut total_read_mismatches = 0u64;

    for cycle in 1..=num_cycles {
        let kill_delay_ms = rng.range(min_delay_ms, max_delay_ms);

        let mut child = Command::new(&child_path)
            .arg(&dir)
            .arg(writer_count.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn lsm_crash_cycle_child");

        std::thread::sleep(Duration::from_millis(kill_delay_ms));
        let _ = child.kill();
        let _ = child.wait();

        let recovery_started = Instant::now();
        match open_for_verification(&dir) {
            Ok(engine) => {
                let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
                let stats = engine.pool_stats();
                let highest_seq = stats.committer_stats.highest_sequence;
                let durable_through = stats.committer_stats.durable_through;
                let ok = highest_seq >= last_highest_seq && durable_through >= last_durable_through;
                if !ok {
                    failures += 1;
                    println!(
                        "lsm_crash_cycle_test: cycle {cycle} FAIL watermark went backward: \
                         highest_seq {highest_seq} < {last_highest_seq} or durable_through \
                         {durable_through} < {last_durable_through}"
                    );
                }
                // Brief §15/§16: actual reads with exact expected
                // values, not just "open() returned Ok" -- sample up to
                // 5,000 keys per writer (comfortably above what a
                // sub-2-second kill delay could produce per thread).
                let mismatches = verify_reads_after_recovery(&engine, writer_count, 5000);
                total_read_mismatches += mismatches;
                if mismatches > 0 {
                    failures += 1;
                    println!(
                        "lsm_crash_cycle_test: cycle {cycle} FAIL {mismatches} read \
                         mismatch(es) after recovery"
                    );
                } else if ok {
                    println!(
                        "lsm_crash_cycle_test: cycle {cycle} OK kill_delay_ms={kill_delay_ms} \
                         highest_seq={highest_seq} durable_through={durable_through} \
                         active_entries={} immutable_count={} recovery_ms={recovery_ms:.1} \
                         reads_verified_ok=true",
                        engine.active_entry_count(),
                        engine.immutable_count(),
                    );
                }
                last_highest_seq = highest_seq;
                last_durable_through = durable_through;
                engine.shutdown();
            }
            Err(e) => {
                failures += 1;
                println!("lsm_crash_cycle_test: cycle {cycle} FAIL LsmEngine::open error: {e}");
            }
        }
    }

    println!(
        "lsm_crash_cycle_test: SUMMARY cycles={num_cycles} successful={} failed={failures} \
         final_highest_seq={last_highest_seq} final_durable_through={last_durable_through} \
         total_read_mismatches={total_read_mismatches}",
        num_cycles - failures
    );

    let _ = fs::remove_dir_all(&dir);
    if failures > 0 {
        std::process::exit(1);
    }
}
