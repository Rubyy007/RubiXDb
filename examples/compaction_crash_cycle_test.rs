//! Compaction — Increment 3 §12/§24 crash-cycle driver. Mirrors
//! `examples/sstable_flush_crash_test.rs`'s proven design (a real child
//! process, killed externally via `Child::kill()` -- abrupt,
//! asynchronous, uncooperative) but targets the real, automatic
//! Compaction background worker specifically, via `compaction_crash_
//! cycle_child.rs`.
//!
//! Two modes:
//! - `fault-sweep [reps_per_point=3] [writer_count=2]`: for each of the
//!   6 `CompactionFaultPoint` variants, spawns `reps_per_point` child
//!   processes each targeted (via the child's own fault-hook marker) to
//!   be killed while execution is genuinely inside that exact window --
//!   §12's own explicit requirement ("exercise every existing
//!   CompactionFaultPoint through the automatic path").
//! - `random <num_cycles> [seed=42] [min_delay_ms=1] [max_delay_ms=200]
//!   [writer_count=2]`: broad, untargeted randomized-delay kills --
//!   §24's secondary crash endurance (10, then 20, cycles if stable).
//!
//! After every kill, the parent reopens via `LsmEngine::open`
//! (`compaction_auto_trigger: false` for the verification engine --
//! deterministic inspection, not a race against a live worker) and
//! verifies: `open()` never errors; no `*.sst.tmp` survives; every
//! live SSTable id the Manifest reports has a corresponding physical
//! file and vice versa (no orphaned live file, no incorrectly-deleted
//! input -- `open()`'s own reconciliation sweep is the mechanism this
//! checks the *outcome* of); `get`/`contains`/`range_scan` never error
//! across a bounded probe; `highest_sequence`/`durable_through`/
//! `checkpoint_seq` never regress across cycles.
//!
//! Usage: `compaction_crash_cycle_test fault-sweep [reps_per_point=3]
//! [writer_count=2]`
//!        `compaction_crash_cycle_test random <num_cycles> [seed=42]
//! [min_delay_ms=1] [max_delay_ms=200] [writer_count=2]`

use std::env;
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

const FAULT_POINTS: [&str; 6] = [
    "BeforeOutputWrite",
    "BeforeManifestAdd",
    "AfterManifestAdd",
    "DuringRemoveSequence",
    "AfterAllRemoves",
    "BeforePhysicalDelete",
];

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
    let base = match std::env::var_os("RUBIXDB_SOAK_BASE_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = base.join(format!("rubixdb_compaction_crash_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn child_binary_path() -> PathBuf {
    let mut path = env::current_exe().expect("current_exe must resolve");
    path.set_file_name(format!(
        "compaction_crash_cycle_child{}",
        std::env::consts::EXE_SUFFIX
    ));
    path
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
    let lsm_config = LsmConfig {
        memtable_max_size_bytes: 1024,
        max_immutable_memtables: 16,
        sstable_target_block_size: 256,
        compaction_trigger_count: 4,
        compaction_auto_trigger: false,
        ..LsmConfig::default()
    };
    LsmEngine::open(dir, wal_config, pool_config, lsm_config)
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

/// Every physically-present `.sst` file's id must be in the live set
/// (no orphaned live file survives `open()`'s reconciliation), and
/// every live id must have a physical file (no incorrectly-deleted
/// input / missing output).
fn check_directory_matches_live_set(sstables_dir: &Path, live_ids: &[u64]) -> Vec<String> {
    let mut problems = Vec::new();
    let live: std::collections::HashSet<u64> = live_ids.iter().copied().collect();
    let Ok(entries) = fs::read_dir(sstables_dir) else {
        return problems;
    };
    let mut on_disk: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for e in entries.filter_map(|e| e.ok()) {
        let path = e.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sst") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Ok(id) = stem.parse::<u64>() {
                    on_disk.insert(id);
                }
            }
        }
    }
    for id in &on_disk {
        if !live.contains(id) {
            problems.push(format!(
                "orphaned live file on disk, not in live set: id={id}"
            ));
        }
    }
    for id in &live {
        if !on_disk.contains(id) {
            problems.push(format!(
                "live SSTable id={id} has no corresponding physical file (missing output / \
                 incorrectly-deleted input)"
            ));
        }
    }
    problems
}

fn verify_reads_dont_error(engine: &LsmEngine) -> bool {
    let mut ok = true;
    for t in 0..2usize {
        for i in 0..200u64 {
            let key = format!("t{t}-{}", i % 200);
            if engine.get(key.as_bytes()).is_err() {
                ok = false;
            }
            if engine.contains(key.as_bytes(), u64::MAX).is_err() {
                ok = false;
            }
        }
    }
    let rows: Result<Vec<_>, _> = engine.range(Bound::Unbounded, Bound::Unbounded).collect();
    match rows {
        Ok(rows) => {
            let mut keys: Vec<_> = rows.iter().map(|(k, _)| k.clone()).collect();
            let sorted_copy = {
                let mut s = keys.clone();
                s.sort();
                s
            };
            if keys != sorted_copy {
                ok = false;
            }
            keys.dedup();
            if keys.len() != rows.len() {
                ok = false;
            }
        }
        Err(_) => ok = false,
    }
    ok
}

struct RunState {
    last_highest_seq: u64,
    last_durable_through: u64,
    last_checkpoint_seq: u64,
    failures: u32,
    cycles: u32,
}

impl RunState {
    fn new() -> Self {
        RunState {
            last_highest_seq: 0,
            last_durable_through: 0,
            last_checkpoint_seq: 0,
            failures: 0,
            cycles: 0,
        }
    }

    fn verify_after_kill(&mut self, dir: &Path, label: &str) {
        self.cycles += 1;
        let sstables_dir = dir.join("sstables");
        let recovery_started = Instant::now();
        match open_for_verification(dir) {
            Ok(engine) => {
                let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
                let stats = engine.pool_stats();
                let highest_seq = stats.committer_stats.highest_sequence;
                let durable_through = stats.committer_stats.durable_through;
                let checkpoint_seq = engine.checkpoint_seq();
                let orphaned_tmp = find_orphaned_tmp_files(&sstables_dir);
                let live_ids = engine.live_sstable_ids();
                let dir_problems = check_directory_matches_live_set(&sstables_dir, &live_ids);
                let reads_ok = verify_reads_dont_error(&engine);

                let mut ok = highest_seq >= self.last_highest_seq
                    && durable_through >= self.last_durable_through
                    && checkpoint_seq >= self.last_checkpoint_seq
                    && orphaned_tmp.is_empty()
                    && dir_problems.is_empty()
                    && reads_ok;

                if !orphaned_tmp.is_empty() {
                    println!("{label} FAIL orphaned .tmp files survived: {orphaned_tmp:?}");
                    ok = false;
                }
                for p in &dir_problems {
                    println!("{label} FAIL directory/live-set mismatch: {p}");
                }
                if !reads_ok {
                    println!("{label} FAIL a read (get/contains/range_scan) errored or returned unsorted/duplicate data");
                }
                if checkpoint_seq < self.last_checkpoint_seq {
                    println!(
                        "{label} FAIL checkpoint_seq regressed: {checkpoint_seq} < {}",
                        self.last_checkpoint_seq
                    );
                }
                if highest_seq < self.last_highest_seq
                    || durable_through < self.last_durable_through
                {
                    println!(
                        "{label} FAIL watermark regressed: highest_seq={highest_seq} \
                         (was {}) durable_through={durable_through} (was {})",
                        self.last_highest_seq, self.last_durable_through
                    );
                }

                if ok {
                    println!(
                        "{label} OK recovery_ms={recovery_ms:.1} highest_seq={highest_seq} \
                         durable_through={durable_through} checkpoint_seq={checkpoint_seq} \
                         live_sstables={} sstable_count={}",
                        live_ids.len(),
                        engine.sstable_count(),
                    );
                } else {
                    self.failures += 1;
                }
                self.last_highest_seq = highest_seq;
                self.last_durable_through = durable_through;
                self.last_checkpoint_seq = checkpoint_seq;
                engine.shutdown();
            }
            Err(e) => {
                self.failures += 1;
                println!("{label} FAIL LsmEngine::open error: {e}");
            }
        }
    }
}

fn spawn_child(child_path: &Path, dir: &Path, writer_count: usize, extra_args: &[String]) -> Child {
    let mut cmd = Command::new(child_path);
    cmd.arg(dir).arg(writer_count.to_string());
    for a in extra_args {
        cmd.arg(a);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn compaction_crash_cycle_child")
}

/// Waits (bounded) for the child's stdout to print the
/// `FAULT_HIT:<point>:<occurrence>` marker line, reading in a
/// background thread so a slow/absent marker cannot hang the parent
/// forever. Returns `true` if the marker was observed.
fn wait_for_marker(child: &mut Child, expect_prefix: &str, timeout: Duration) -> bool {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;
    let stdout = child.stdout.take().expect("child stdout must be piped");
    let (tx, rx) = mpsc::channel();
    let expect_prefix = expect_prefix.to_string();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if line.starts_with(&expect_prefix) {
                let _ = tx.send(());
                return;
            }
        }
    });
    rx.recv_timeout(timeout).is_ok()
}

fn run_fault_sweep(child_path: &Path, reps_per_point: u32, writer_count: usize) {
    // Each (point, rep) gets its own fresh directory (needed to
    // deterministically target the first occurrence of each fault
    // point, per `compaction_crash_cycle_child.rs`'s own design) --
    // and therefore its own fresh `RunState`, so watermark/checkpoint
    // monotonicity is only ever compared *within* one directory's own
    // history, never across two unrelated fresh databases (which
    // would be a meaningless "regression").
    let mut total_cycles = 0u32;
    let mut total_failures = 0u32;
    for &point in &FAULT_POINTS {
        for rep in 0..reps_per_point {
            let mut state = RunState::new();
            let dir = temp_dir(&format!("sweep_{point}_{rep}"));
            let mut child = spawn_child(
                child_path,
                &dir,
                writer_count,
                &[point.to_string(), "1".to_string()],
            );
            let marker_seen = wait_for_marker(
                &mut child,
                &format!("FAULT_HIT:{point}"),
                Duration::from_secs(30),
            );
            let _ = child.kill();
            let _ = child.wait();
            let label = format!("fault_sweep point={point} rep={rep} marker_seen={marker_seen}");
            state.verify_after_kill(&dir, &label);
            let _ = fs::remove_dir_all(&dir);
            total_cycles += state.cycles;
            total_failures += state.failures;
        }
    }
    println!(
        "compaction_crash_cycle_test: FAULT-SWEEP SUMMARY points={} reps_per_point={reps_per_point} \
         cycles={total_cycles} successful={} failed={total_failures}",
        FAULT_POINTS.len(),
        total_cycles - total_failures,
    );
    if total_failures > 0 {
        std::process::exit(1);
    }
}

fn run_random(
    child_path: &Path,
    num_cycles: u32,
    seed: u64,
    min_delay_ms: u64,
    max_delay_ms: u64,
    writer_count: usize,
) {
    let mut rng = Xorshift64::new(seed);
    let mut state = RunState::new();
    let dir = temp_dir("random");
    for cycle in 1..=num_cycles {
        let kill_delay_ms = rng.range(min_delay_ms, max_delay_ms);
        let mut child = spawn_child(child_path, &dir, writer_count, &[]);
        thread::sleep(Duration::from_millis(kill_delay_ms));
        let _ = child.kill();
        let _ = child.wait();
        let label = format!("random cycle={cycle} kill_delay_ms={kill_delay_ms}");
        state.verify_after_kill(&dir, &label);
    }
    println!(
        "compaction_crash_cycle_test: RANDOM SUMMARY cycles={num_cycles} successful={} failed={}",
        state.cycles - state.failures,
        state.failures
    );
    let _ = fs::remove_dir_all(&dir);
    if state.failures > 0 {
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: compaction_crash_cycle_test fault-sweep [reps_per_point=3] [writer_count=2]\n\
             usage: compaction_crash_cycle_test random <num_cycles> [seed=42] [min_delay_ms=1] \
             [max_delay_ms=200] [writer_count=2]"
        );
        std::process::exit(2);
    }
    let child_path = child_binary_path();
    if !child_path.exists() {
        eprintln!(
            "compaction_crash_cycle_test: child binary not found at {} — build it first: cargo \
             build --release --example compaction_crash_cycle_child",
            child_path.display()
        );
        std::process::exit(2);
    }

    match args[1].as_str() {
        "fault-sweep" => {
            let reps_per_point: u32 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(3);
            let writer_count: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(2);
            println!(
                "compaction_crash_cycle_test: fault-sweep reps_per_point={reps_per_point} \
                 writer_count={writer_count}"
            );
            run_fault_sweep(&child_path, reps_per_point, writer_count);
        }
        "random" => {
            let num_cycles: u32 = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(10);
            let seed: u64 = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(42);
            let min_delay_ms: u64 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(1);
            let max_delay_ms: u64 = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(200);
            let writer_count: usize = args.get(6).map(|s| s.parse().unwrap()).unwrap_or(2);
            println!(
                "compaction_crash_cycle_test: random num_cycles={num_cycles} seed={seed} \
                 delay=[{min_delay_ms},{max_delay_ms}]ms writer_count={writer_count}"
            );
            run_random(
                &child_path,
                num_cycles,
                seed,
                min_delay_ms,
                max_delay_ms,
                writer_count,
            );
        }
        other => {
            eprintln!("unknown mode: {other} (expected fault-sweep or random)");
            std::process::exit(2);
        }
    }
}
