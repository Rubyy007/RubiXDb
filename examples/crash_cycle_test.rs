//! Phase 3C periodic forced-crash-during-soak driver (operating brief
//! §5-§6). Repeatedly: spawns `crash_cycle_child` against one
//! accumulating WAL directory, lets it run under real concurrent load
//! for a randomized (seeded, reproducible) duration, then forcibly
//! kills it (`Child::kill()` — `TerminateProcess` on Windows, an
//! external, asynchronous, abrupt kill with zero cooperation from the
//! child, unlike a self-inflicted `abort()` at a code-chosen point —
//! this can land truly anywhere, including mid-syscall). After each
//! kill, reopens the same WAL directory in *this* process and verifies
//! the WAL recovery contract: no corruption, gap-free sequences, the
//! durable prefix survives. No fixed "always after 60s" point — delays
//! are drawn from a small, seeded, `std`-only PRNG (no new dependency)
//! so the exact sequence is reproducible given the same seed, without
//! ever landing on the same instant twice across cycles.
//!
//! Usage: `crash_cycle_test <num_cycles> <writer_count> [seed=42]
//! [min_delay_ms=300] [max_delay_ms=3000]`

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{FileWal, Wal, WalConfig};

/// Minimal, `std`-only, seeded PRNG — deliberately not the `rand` crate:
/// this is a test-harness-only need (a reproducible sequence of kill
/// delays), not a case for a new dependency (`PHASE3C_ADR.md`).
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
    let path = std::env::temp_dir().join(format!("rubixdb_crash_cycle_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn child_binary_path() -> PathBuf {
    let mut path = env::current_exe().expect("current_exe must resolve");
    path.set_file_name(format!("crash_cycle_child{}", std::env::consts::EXE_SUFFIX));
    path
}

struct CycleResult {
    cycle: u32,
    kill_delay_ms: u64,
    recovered_records: usize,
    highest_seq: u64,
    corrupted_segments: usize,
    recovery_ms: f64,
    gap_free: bool,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: crash_cycle_test <num_cycles> <writer_count> [seed=42] \
             [min_delay_ms=300] [max_delay_ms=3000]"
        );
        std::process::exit(2);
    }
    let num_cycles: u32 = args[1].parse().expect("num_cycles must be a u32");
    let writer_count: usize = args[2].parse().expect("writer_count must be a usize");
    let seed: u64 = args
        .get(3)
        .map(|s| s.parse().expect("seed must be a u64"))
        .unwrap_or(42);
    let min_delay_ms: u64 = args
        .get(4)
        .map(|s| s.parse().expect("min_delay_ms must be a u64"))
        .unwrap_or(300);
    let max_delay_ms: u64 = args
        .get(5)
        .map(|s| s.parse().expect("max_delay_ms must be a u64"))
        .unwrap_or(3000);

    let wal_dir = temp_dir("run");
    println!(
        "crash_cycle_test: wal_dir={} num_cycles={num_cycles} writer_count={writer_count} \
         seed={seed} delay_range_ms=[{min_delay_ms},{max_delay_ms}]",
        wal_dir.display()
    );

    let child_path = child_binary_path();
    if !child_path.exists() {
        eprintln!(
            "crash_cycle_test: child binary not found at {} — build it first: \
             cargo build --release --example crash_cycle_child",
            child_path.display()
        );
        std::process::exit(2);
    }

    let mut rng = Xorshift64::new(seed);
    let mut results: Vec<CycleResult> = Vec::with_capacity(num_cycles as usize);
    let mut failures = 0u32;
    let mut expected_highest_seq: u64 = 0;

    for cycle in 1..=num_cycles {
        let kill_delay_ms = rng.range(min_delay_ms, max_delay_ms);

        let mut child = Command::new(&child_path)
            .arg(&wal_dir)
            .arg(writer_count.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn crash_cycle_child");

        std::thread::sleep(Duration::from_millis(kill_delay_ms));

        // The abrupt, external, asynchronous kill this whole exercise is
        // for — no cooperation from the child, unlike `abort()`.
        let _ = child.kill();
        let _ = child.wait();

        // The child held an exclusive OS-level advisory lock on wal_dir
        // for its whole lifetime (`ARCHITECTURE.md`'s "Cross-process file
        // locking" section) — released automatically by the OS the
        // instant it died, per that mechanism's own documented design
        // goal. This call is itself a real test of that guarantee: if it
        // hung or failed to acquire the lock, that would be a genuine
        // finding, not merely a harness inconvenience.
        let recovery_started = Instant::now();
        let open_result = FileWal::open_for_recovery(&wal_dir, WalConfig::default());
        let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;

        match open_result {
            Ok((wal, replay)) => {
                drop(wal); // release the lock before the next cycle's child needs it
                let corrupted = replay.corrupted_segments.len();
                let highest_seq = replay.records.last().map(|(seq, _)| *seq).unwrap_or(0);
                let mut gap_free = true;
                for (i, (seq, _)) in replay.records.iter().enumerate() {
                    if *seq != (i as u64) + 1 {
                        gap_free = false;
                        break;
                    }
                }
                let ok = corrupted == 0 && gap_free && highest_seq >= expected_highest_seq;
                if !ok {
                    failures += 1;
                    println!(
                        "crash_cycle_test: cycle {cycle} FAIL corrupted_segments={corrupted} \
                         gap_free={gap_free} highest_seq={highest_seq} \
                         (expected >= {expected_highest_seq})"
                    );
                } else {
                    println!(
                        "crash_cycle_test: cycle {cycle} OK kill_delay_ms={kill_delay_ms} \
                         recovered_records={} highest_seq={highest_seq} recovery_ms={recovery_ms:.1}",
                        replay.records.len()
                    );
                }
                expected_highest_seq = highest_seq;
                results.push(CycleResult {
                    cycle,
                    kill_delay_ms,
                    recovered_records: replay.records.len(),
                    highest_seq,
                    corrupted_segments: corrupted,
                    recovery_ms,
                    gap_free,
                });
            }
            Err(e) => {
                failures += 1;
                println!("crash_cycle_test: cycle {cycle} FAIL open_for_recovery error: {e}");
                results.push(CycleResult {
                    cycle,
                    kill_delay_ms,
                    recovered_records: 0,
                    highest_seq: expected_highest_seq,
                    corrupted_segments: usize::MAX, // sentinel: open itself failed
                    recovery_ms,
                    gap_free: false,
                });
            }
        }
    }

    println!(
        "crash_cycle_test: SUMMARY cycles={num_cycles} successful_recoveries={} \
         failed_recoveries={failures} final_highest_seq={expected_highest_seq}",
        num_cycles - failures
    );
    for r in &results {
        println!(
            "  cycle={} kill_delay_ms={} recovered={} highest_seq={} corrupted_segments={} \
             recovery_ms={:.1} gap_free={}",
            r.cycle,
            r.kill_delay_ms,
            r.recovered_records,
            r.highest_seq,
            r.corrupted_segments,
            r.recovery_ms,
            r.gap_free
        );
    }

    let _ = fs::remove_dir_all(&wal_dir);
    if failures > 0 {
        std::process::exit(1);
    }
}
