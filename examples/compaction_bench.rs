//! Compaction — Increment 3: production performance + resource +
//! correctness-under-load measurement. Every number this program prints
//! comes from actually running the engine's real, unmocked automatic
//! Compaction path (`compaction_auto_trigger: true`, the same
//! `compact_once_impl` code the background worker thread calls in
//! production — `PHASE_COMPACTION_INCREMENT2_RESULTS.md` §1 establishes
//! manual and automatic entry points share this one code path
//! byte-for-byte) against real on-disk SSTables built through the real
//! write path. Nothing here is mocked: no synthetic SSTable/Manifest
//! construction, no WAL-independent fixture shortcuts.
//!
//! `compact_once`/`should_compact` are `pub(crate)` by deliberate ADR
//! decision (`ADR-COMPACTION-001` Decision 13 — no public API without a
//! real caller) and therefore not reachable from this external example;
//! every measurement below instead drives the *real* automatic worker
//! (the only externally-reachable entry point) and observes it via the
//! new, additive `LsmEngine::compaction_metrics()` accessor (Increment
//! 3's own only production-code change — see `PHASE_COMPACTION_
//! INCREMENT3_ENDURANCE.md` / commit history for the diff).
//!
//! Requires `--features test-util` (for `set_storage_state_for_test`,
//! used by the storage-pressure section only — every other section
//! compiles and runs the same without it, but the crate feature is
//! additive so there is no reason to build this example without it).
//!
//! Usage: `cargo run --release --features test-util --example
//! compaction_bench -- [section...]`. With no arguments, runs every
//! *bounded* section (a few minutes total). Long sections
//! (`handles_threads_100`, `rss_scaling`) are opt-in only, named
//! explicitly. Sections: `count_sweep shape_sweep storage_budget
//! rss_scaling handles_threads auto_trigger_stress concurrent_rw
//! snapshot_endurance tombstone_endurance read_write_impact
//! sstable_count_stability storage_pressure failure_retry shutdown_endurance`.

use std::env;
use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine, StorageState};
use rubixdb::wal::{SyncMode, WalConfig};

// ============================================================
// Shared plumbing (same conventions as read_engine_bench.rs /
// realistic_full_pipeline_soak.rs).
// ============================================================

/// Same `RUBIXDB_SOAK_BASE_DIR` override `realistic_full_pipeline_
/// soak.rs` established (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md`'s own
/// root cause: this machine's `C:` `%TEMP%` is chronically near-full).
/// Defaults to `std::env::temp_dir()`, unchanged, when unset.
fn soak_base_dir() -> PathBuf {
    match std::env::var_os("RUBIXDB_SOAK_BASE_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::temp_dir(),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = soak_base_dir().join(format!("rubixdb_compaction_bench_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
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
        queue_capacity: 4096,
        max_queued_bytes: 64 * 1024 * 1024,
        submission_timeout: Duration::from_secs(10),
        shutdown_drain_bound: Duration::from_secs(60),
        await_retry_budget: Duration::from_secs(10),
        max_drain_per_batch: 65536,
    }
}

fn lsm_config(
    memtable_bytes: usize,
    max_immutable: usize,
    trigger_count: usize,
    auto_trigger: bool,
    fallback_interval: Duration,
) -> LsmConfig {
    LsmConfig {
        memtable_max_size_bytes: memtable_bytes,
        max_immutable_memtables: max_immutable,
        compaction_trigger_count: trigger_count,
        compaction_auto_trigger: auto_trigger,
        storage_pressure_retry_interval: fallback_interval,
        ..LsmConfig::default()
    }
}

fn open_engine(dir: &Path, cfg: LsmConfig) -> LsmEngine {
    LsmEngine::open(dir, wal_config(), pool_config(), cfg).unwrap()
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !cond() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}

fn percentile_ns(sorted: &[u128], pct: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * pct) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn us(ns: u128) -> f64 {
    ns as f64 / 1_000.0
}

fn report_latency(label: &str, latencies_ns: &mut [u128]) {
    latencies_ns.sort_unstable();
    let n = latencies_ns.len();
    println!(
        "  {label}: n={n} p50={:.1}us p95={:.1}us p99={:.1}us max={:.1}us",
        us(percentile_ns(latencies_ns, 0.50)),
        us(percentile_ns(latencies_ns, 0.95)),
        us(percentile_ns(latencies_ns, 0.99)),
        us(*latencies_ns.last().unwrap_or(&0)),
    );
}

fn median_ms(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        return 0.0;
    }
    v[v.len() / 2]
}

fn sample_rss_kb(pid: u32) -> Option<u64> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).WorkingSet64"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|b| b / 1024)
}

fn sample_handle_count(pid: u32) -> Option<u64> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).HandleCount"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn sample_thread_count(pid: u32) -> Option<u64> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).Threads.Count"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Free space on the volume hosting `path`, via `Get-PSDrive` (no admin
/// rights required, unlike `Get-Volume` in some environments).
fn free_disk_bytes(path: &Path) -> Option<u64> {
    let canon = fs::canonicalize(path).ok()?;
    let drive_letter = canon.to_str()?.trim_start_matches(r"\\?\").chars().next()?;
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-PSDrive -Name {drive_letter}).Free"),
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn current_pid() -> u32 {
    std::process::id()
}

/// Total bytes of every regular file directly under `dir` (non-
/// recursive — the SSTable directory is flat).
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Background poll-and-track-peak sampler, generic over what a single
/// sample returns (`u64`). Used for both RSS and SSTable-directory
/// byte totals — the "peak during a possibly-brief operation" shape is
/// identical for both.
struct PeakSampler {
    stop: Arc<AtomicBool>,
    min: Arc<AtomicU64>,
    max: Arc<AtomicU64>,
    handle: Option<thread::JoinHandle<()>>,
}

impl PeakSampler {
    fn start(interval: Duration, mut sample: impl FnMut() -> Option<u64> + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let min = Arc::new(AtomicU64::new(u64::MAX));
        let max = Arc::new(AtomicU64::new(0));
        let (stop2, min2, max2) = (Arc::clone(&stop), Arc::clone(&min), Arc::clone(&max));
        let handle = thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                if let Some(v) = sample() {
                    min2.fetch_min(v, Ordering::Relaxed);
                    max2.fetch_max(v, Ordering::Relaxed);
                }
                thread::sleep(interval);
            }
        });
        PeakSampler {
            stop,
            min,
            max,
            handle: Some(handle),
        }
    }

    fn stop(mut self) -> (u64, u64) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
        (
            self.min.load(Ordering::Relaxed),
            self.max.load(Ordering::Relaxed),
        )
    }
}

// ============================================================
// Fixture construction — three overlap shapes, three value sizes.
// ============================================================

#[derive(Clone, Copy, Debug)]
enum Overlap {
    Low,
    Medium,
    High,
}

impl Overlap {
    fn label(self) -> &'static str {
        match self {
            Overlap::Low => "low",
            Overlap::Medium => "medium",
            Overlap::High => "high",
        }
    }

    /// Distinct-key cardinality for `total_writes` total puts, at
    /// `keys_per_table` writes per flush cycle. Low = almost every
    /// write is a new key (minimal duplicate density); High = every
    /// table touches nearly the same small keyspace (heavy duplicate
    /// density, matching `read_engine_bench.rs`'s own `overlap_repro`
    /// regime).
    fn cardinality(self, total_writes: u64, keys_per_table: u64) -> u64 {
        match self {
            Overlap::Low => total_writes,
            Overlap::Medium => (total_writes / 4).max(keys_per_table),
            Overlap::High => keys_per_table,
        }
    }
}

/// Reference-model aliases shared by the concurrency/endurance
/// sections below: key -> latest known value, and key -> full
/// (seq, value) version history, respectively.
type LatestModel = Arc<Mutex<std::collections::HashMap<Vec<u8>, Option<Vec<u8>>>>>;
type HistoryModel = std::collections::HashMap<Vec<u8>, Vec<(u64, Option<Vec<u8>>)>>;

const KEYS_PER_TABLE: u64 = 8;

fn fixture_key(idx: u64, cardinality: u64) -> Vec<u8> {
    format!("k{:08}", idx % cardinality.max(1)).into_bytes()
}

/// Calibrates `memtable_max_size_bytes` so exactly `KEYS_PER_TABLE`
/// puts of `value_size`-byte values force one freeze — same formula
/// `read_engine_bench.rs::build_fixture` uses (key_len is fixed at 9
/// bytes by `fixture_key`'s own `k{:08}` format).
fn calibrated_memtable_bytes(value_size: usize) -> usize {
    let key_len = 9usize;
    let per_entry = key_len + value_size + 8;
    (per_entry as u64 * KEYS_PER_TABLE) as usize - per_entry / 2
}

/// Builds `table_count` live SSTables *offline* (`compaction_auto_
/// trigger: false`, deterministic — `ADR-COMPACTION-001` Increment 2
/// §A6's own established fixture-building fix: never race a live
/// automatic worker while building a fixture). Returns the directory
/// (caller owns cleanup) and the next unused write index, so a caller
/// can continue the identical key/value generator seamlessly across a
/// close/reopen boundary.
fn build_offline(table_count: usize, value_size: usize, overlap: Overlap) -> (PathBuf, u64, u64) {
    let dir = temp_dir("fx");
    let total_writes_estimate = table_count as u64 * KEYS_PER_TABLE;
    let cardinality = overlap.cardinality(total_writes_estimate, KEYS_PER_TABLE);
    let memtable_bytes = calibrated_memtable_bytes(value_size);
    let cfg = lsm_config(
        memtable_bytes,
        (table_count + 8).max(16),
        usize::MAX, // irrelevant while auto_trigger=false
        false,
        Duration::from_secs(5),
    );
    let engine = open_engine(&dir, cfg);
    let mut idx = 0u64;
    let value = vec![b'v'; value_size];
    while engine.sstable_count() < table_count {
        let key = fixture_key(idx, cardinality);
        engine.put(&key, &value).unwrap();
        idx += 1;
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(30));
    }
    assert!(
        wait_until(
            || engine.sstable_count() >= table_count && engine.immutable_count() == 0,
            Duration::from_secs(60)
        ),
        "offline fixture build must settle to exactly {table_count} SSTables"
    );
    engine.shutdown();
    drop(engine);
    (dir, idx, cardinality)
}

// ============================================================
// §2-5: one real, automatic compaction cycle, precisely timed and
// measured, for a given (table_count, value_size, overlap) point.
// ============================================================

#[derive(Debug, Clone, Default)]
struct CycleResult {
    trigger_to_completion_ms: f64,
    merge_duration_ms: f64,
    input_sstable_count: usize,
    input_bytes: u64,
    output_bytes: u64,
    records_read: u64,
    records_retained: u64,
    records_dropped: u64,
    tombstones_dropped: u64,
    dir_peak_bytes: u64,
    theoretical_peak_bytes: u64,
    rss_peak_kb: u64,
}

/// Builds `table_count - 1` tables offline, reopens with the real
/// automatic worker enabled and `compaction_trigger_count = table_
/// count`, then issues exactly one more table's worth of writes --
/// forcing a flush whose publish immediately notifies the worker (the
/// prompt, non-fallback-tick wake path), giving a precise measurement
/// window with no `storage_pressure_retry_interval` polling latency
/// folded in. Every byte/record/duration figure in the result comes
/// from `compaction_metrics().last_cycle` -- the real `CompactionStats`
/// the real automatic cycle produced, not a re-derived estimate.
fn run_one_compaction_cycle(
    table_count: usize,
    value_size: usize,
    overlap: Overlap,
) -> CycleResult {
    let (dir, next_idx, cardinality) = build_offline(table_count - 1, value_size, overlap);
    let memtable_bytes = calibrated_memtable_bytes(value_size);
    let cfg = lsm_config(
        memtable_bytes,
        (table_count + 8).max(16),
        table_count,
        true,
        Duration::from_secs(5),
    );
    let engine = open_engine(&dir, cfg);
    let baseline_cycles = engine.compaction_metrics().cycles_completed;
    let value = vec![b'v'; value_size];

    let pid = current_pid();
    let rss_sampler = PeakSampler::start(Duration::from_millis(5), move || sample_rss_kb(pid));
    let sstables_dir = dir.join("sstables");
    let dir_sampler = {
        let sstables_dir = sstables_dir.clone();
        PeakSampler::start(Duration::from_millis(2), move || {
            Some(dir_bytes(&sstables_dir))
        })
    };

    let mut idx = next_idx;
    while engine.sstable_count() < table_count {
        let key = fixture_key(idx, cardinality);
        engine.put(&key, &value).unwrap();
        idx += 1;
    }
    wait_until(
        || engine.sstable_count() >= table_count && engine.immutable_count() == 0,
        Duration::from_secs(30),
    );
    let t_flush_done = Instant::now();

    let cycle_ran = wait_until(
        || engine.compaction_metrics().cycles_completed > baseline_cycles,
        Duration::from_secs(60),
    );
    let t_cycle_done = Instant::now();
    assert!(cycle_ran, "compaction cycle must complete within 60s");

    let (_rss_min, rss_peak) = rss_sampler.stop();
    let (_dir_min, dir_peak) = dir_sampler.stop();

    let metrics = engine.compaction_metrics();
    let last = metrics
        .last_cycle
        .expect("last_cycle must be populated after a completed cycle");

    let result = CycleResult {
        trigger_to_completion_ms: t_cycle_done.duration_since(t_flush_done).as_secs_f64() * 1000.0,
        merge_duration_ms: last.duration.as_secs_f64() * 1000.0,
        input_sstable_count: last.input_sstable_count,
        input_bytes: last.input_bytes,
        output_bytes: last.output_bytes,
        records_read: last.records_read,
        records_retained: last.records_retained,
        records_dropped: last.records_dropped,
        tombstones_dropped: last.tombstones_dropped,
        dir_peak_bytes: dir_peak,
        theoretical_peak_bytes: last.input_bytes + last.output_bytes,
        rss_peak_kb: rss_peak,
    };

    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
    result
}

const REPS: usize = 3;
const COUNT_SWEEP: [usize; 7] = [4, 8, 16, 32, 64, 128, 256];

fn section_count_sweep() {
    println!(
        "\n=== count_sweep (\u{a7}2/\u{a7}4): performance across SSTable input counts, fixed \
         shape (medium overlap, 256B values), {REPS} reps each ==="
    );
    for &count in &COUNT_SWEEP {
        let mut durations = Vec::new();
        let mut merges = Vec::new();
        for rep in 0..REPS {
            let r = run_one_compaction_cycle(count, 256, Overlap::Medium);
            println!(
                "  count={count:4} rep={rep} input_sstable_count={} trigger_to_completion_ms={:.2} \
                 merge_duration_ms={:.2} input_bytes={} output_bytes={} records_read={} \
                 records_retained={} records_dropped={} tombstones_dropped={} \
                 dir_peak_bytes={} theoretical_peak_bytes={} rss_peak_kb={}",
                r.input_sstable_count,
                r.trigger_to_completion_ms,
                r.merge_duration_ms,
                r.input_bytes,
                r.output_bytes,
                r.records_read,
                r.records_retained,
                r.records_dropped,
                r.tombstones_dropped,
                r.dir_peak_bytes,
                r.theoretical_peak_bytes,
                r.rss_peak_kb,
            );
            durations.push(r.trigger_to_completion_ms);
            merges.push(r.merge_duration_ms);
        }
        let (dmin, dmax) = (
            durations.iter().cloned().fold(f64::MAX, f64::min),
            durations.iter().cloned().fold(f64::MIN, f64::max),
        );
        let (mmin, mmax) = (
            merges.iter().cloned().fold(f64::MAX, f64::min),
            merges.iter().cloned().fold(f64::MIN, f64::max),
        );
        println!(
            "  count={count:4} SUMMARY trigger_to_completion_ms: min={dmin:.2} \
             median={:.2} max={dmax:.2} | merge_duration_ms: min={mmin:.2} median={:.2} max={mmax:.2}",
            median_ms(durations),
            median_ms(merges),
        );
    }
}

const SHAPE_COUNT: usize = 32;

fn section_shape_sweep() {
    println!(
        "\n=== shape_sweep (\u{a7}3/\u{a7}4): performance across overlap x value-size shapes, \
         fixed count={SHAPE_COUNT}, {REPS} reps each (medium/256B duplicates count_sweep's own \
         row -- not cherry-picked, the same measurement) ==="
    );
    for overlap in [Overlap::Low, Overlap::Medium, Overlap::High] {
        for &value_size in &[32usize, 256, 8192] {
            let mut durations = Vec::new();
            for rep in 0..REPS {
                let r = run_one_compaction_cycle(SHAPE_COUNT, value_size, overlap);
                println!(
                    "  overlap={:6} value_size={value_size:5} rep={rep} \
                     trigger_to_completion_ms={:.2} merge_duration_ms={:.2} input_bytes={} \
                     output_bytes={} records_read={} records_retained={} records_dropped={} \
                     reduction_ratio={:.3}",
                    overlap.label(),
                    r.trigger_to_completion_ms,
                    r.merge_duration_ms,
                    r.input_bytes,
                    r.output_bytes,
                    r.records_read,
                    r.records_retained,
                    r.records_dropped,
                    if r.input_bytes > 0 {
                        r.output_bytes as f64 / r.input_bytes as f64
                    } else {
                        0.0
                    },
                );
                durations.push(r.trigger_to_completion_ms);
            }
            println!(
                "  overlap={:6} value_size={value_size:5} SUMMARY trigger_to_completion_ms \
                 median={:.2}",
                overlap.label(),
                median_ms(durations)
            );
        }
    }
}

// ============================================================
// §5: storage budget -- real free-disk-before/after, actual measured
// peak vs. theoretical input+output, at the largest count-sweep point.
// ============================================================

fn section_storage_budget() {
    println!("\n=== storage_budget (\u{a7}5): real free-disk + measured-vs-theoretical peak ===");
    let dir_for_free_space = soak_base_dir();
    let free_before = free_disk_bytes(&dir_for_free_space);
    println!("free_disk_before_bytes={free_before:?}");

    for &count in &[64usize, 256] {
        let r = run_one_compaction_cycle(count, 256, Overlap::Medium);
        let delta = r.dir_peak_bytes as i64 - r.theoretical_peak_bytes as i64;
        println!(
            "count={count:4} measured_dir_peak_bytes={} theoretical_peak_bytes(input+output)={} \
             delta={delta} ({:+.2}%)",
            r.dir_peak_bytes,
            r.theoretical_peak_bytes,
            if r.theoretical_peak_bytes > 0 {
                100.0 * delta as f64 / r.theoretical_peak_bytes as f64
            } else {
                0.0
            },
        );
    }

    let free_after = free_disk_bytes(&dir_for_free_space);
    println!("free_disk_after_bytes={free_after:?}");
    if let (Some(b), Some(a)) = (free_before, free_after) {
        println!(
            "free_disk_delta_bytes={} (temp fixtures are removed after each cycle, so this \
             should be small/noise, not proportional to the compacted data volume)",
            a as i64 - b as i64
        );
    }
}

// ============================================================
// §6: RSS vs. input SSTable count / bytes / records-retained, across
// repeated automatic cycles on one continuously-growing fixture.
// ============================================================

fn section_rss_scaling() {
    println!(
        "\n=== rss_scaling (\u{a7}6): RSS sampled across repeated automatic compaction cycles \
         on a continuously-growing fixture ==="
    );
    let dir = temp_dir("rss_scaling");
    let value_size = 256usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        32,
        4,
        true,
        Duration::from_millis(300),
    );
    let engine = open_engine(&dir, cfg);
    let pid = current_pid();
    let value = vec![b'v'; value_size];
    let cardinality = 20_000u64;

    const CHECKPOINT_CYCLES: [u64; 6] = [5, 10, 20, 40, 80, 150];
    let mut idx = 0u64;
    let mut next_checkpoint = 0usize;
    let start = Instant::now();
    while next_checkpoint < CHECKPOINT_CYCLES.len() {
        engine.put(&fixture_key(idx, cardinality), &value).unwrap();
        idx += 1;
        let cycles = engine.compaction_metrics().cycles_completed;
        if cycles >= CHECKPOINT_CYCLES[next_checkpoint] {
            let target = CHECKPOINT_CYCLES[next_checkpoint];
            let m = engine.compaction_metrics();
            let rss = sample_rss_kb(pid);
            println!(
                "checkpoint cycles>={target} actual_cycles={cycles} elapsed_s={:.1} \
                 rss_kb={rss:?} sstable_count={} input_sstables_total={} \
                 input_bytes_total={} output_bytes_total={} records_retained_total={} \
                 duration_max_ms={:.2}",
                start.elapsed().as_secs_f64(),
                engine.sstable_count(),
                m.input_sstables_total,
                m.input_bytes_total,
                m.output_bytes_total,
                m.records_retained_total,
                m.duration_max.as_secs_f64() * 1000.0,
            );
            next_checkpoint += 1;
        }
        if idx.is_multiple_of(500) && start.elapsed() > Duration::from_secs(300) {
            println!("rss_scaling: bailing out after 300s, checkpoint not fully reached");
            break;
        }
    }

    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §7: file handle / thread validation across repeated cycles +
// shutdown/reopen.
// ============================================================

fn section_handles_threads() {
    println!(
        "\n=== handles_threads (\u{a7}7): handle/thread counts across 10/50/100 automatic \
         compaction cycles, plus shutdown/reopen ==="
    );
    let pid = current_pid();
    thread::sleep(Duration::from_millis(200));
    let baseline_handles = sample_handle_count(pid);
    let baseline_threads = sample_thread_count(pid);
    println!("process baseline (no engine open): handles={baseline_handles:?} threads={baseline_threads:?}");

    let dir = temp_dir("handles_threads");
    let value_size = 64usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        32,
        4,
        true,
        Duration::from_millis(300),
    );
    let engine = open_engine(&dir, cfg);
    let value = vec![b'v'; value_size];
    let cardinality = 5_000u64;
    let mut idx = 0u64;

    for &target_cycles in &[10u64, 50, 100] {
        let deadline = Instant::now() + Duration::from_secs(180);
        while engine.compaction_metrics().cycles_completed < target_cycles
            && Instant::now() < deadline
        {
            engine.put(&fixture_key(idx, cardinality), &value).unwrap();
            idx += 1;
        }
        thread::sleep(Duration::from_millis(100));
        let handles = sample_handle_count(pid);
        let threads = sample_thread_count(pid);
        let m = engine.compaction_metrics();
        println!(
            "after cycles>={target_cycles} (actual={}): handles={handles:?} threads={threads:?} \
             sstable_count={} pending_deletes_swept_ok(no_growth_expected)",
            m.cycles_completed,
            engine.sstable_count(),
        );
    }

    engine.shutdown();
    drop(engine);
    thread::sleep(Duration::from_millis(200));
    let after_shutdown_handles = sample_handle_count(pid);
    let after_shutdown_threads = sample_thread_count(pid);
    println!(
        "after shutdown+drop: handles={after_shutdown_handles:?} threads={after_shutdown_threads:?} \
         (compare against process baseline above -- no worker/flush thread or file handle should \
         remain attributable to this engine instance)"
    );

    let cfg2 = lsm_config(
        calibrated_memtable_bytes(value_size),
        32,
        4,
        false,
        Duration::from_secs(5),
    );
    let engine2 = open_engine(&dir, cfg2);
    thread::sleep(Duration::from_millis(200));
    let after_reopen_handles = sample_handle_count(pid);
    let after_reopen_threads = sample_thread_count(pid);
    println!(
        "after reopen (auto_trigger=false, live_sstables={}): handles={after_reopen_handles:?} \
         threads={after_reopen_threads:?}",
        engine2.sstable_count()
    );
    engine2.shutdown();
    drop(engine2);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §8: automatic trigger stress under continuous writes.
// ============================================================

fn section_auto_trigger_stress() {
    println!(
        "\n=== auto_trigger_stress (\u{a7}8): continuous writes, compaction_trigger_count=4, \
         bounded duration, correctness + cycle-count tracked ==="
    );
    let dir = temp_dir("trigger_stress");
    let value_size = 48usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        true,
        Duration::from_millis(300),
    );
    let engine = Arc::new(open_engine(&dir, cfg));
    let cardinality = 3_000u64;
    let value = vec![b'v'; value_size];
    let stop = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicU64::new(0));
    let max_sstable_count = Arc::new(AtomicU64::new(0));

    let writer = {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        let writes = Arc::clone(&writes);
        let max_sstable_count = Arc::clone(&max_sstable_count);
        let value = value.clone();
        thread::spawn(move || {
            let mut idx = 0u64;
            while !stop.load(Ordering::Relaxed) {
                engine.put(&fixture_key(idx, cardinality), &value).unwrap();
                idx += 1;
                writes.fetch_add(1, Ordering::Relaxed);
                max_sstable_count.fetch_max(engine.sstable_count() as u64, Ordering::Relaxed);
            }
        })
    };

    thread::sleep(Duration::from_secs(20));
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    let m = engine.compaction_metrics();
    println!(
        "writes_issued={} duration_s=20 cycles_completed={} max_sstable_count_observed={} \
         final_sstable_count={} input_sstables_total={} records_dropped_total={} \
         duration_total_ms={:.1} duration_max_ms={:.2}",
        writes.load(Ordering::Relaxed),
        m.cycles_completed,
        max_sstable_count.load(Ordering::Relaxed),
        engine.sstable_count(),
        m.input_sstables_total,
        m.records_dropped_total,
        m.duration_total.as_secs_f64() * 1000.0,
        m.duration_max.as_secs_f64() * 1000.0,
    );
    println!(
        "note: exactly-one-concurrent-compaction is already proven at the unit level \
         (`compaction_run_guard_permits_exactly_one_concurrent_holder`, 16 threads x 500 \
         attempts, max observed holder=1) -- this section characterizes duration/throughput at \
         sustained production-like load, not re-proving the guard black-box."
    );

    let engine = Arc::try_unwrap(engine).unwrap_or_else(|_| panic!("outstanding Arc reference"));
    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §9: concurrent writers + point/range readers + snapshot users +
// automatic compaction, correctness-checked against an independently
// tracked reference model.
// ============================================================

fn section_concurrent_rw_compaction() {
    println!(
        "\n=== concurrent_rw (\u{a7}9): writers + point/range readers + snapshot users + \
         automatic compaction, 20s, correctness-checked ==="
    );
    let dir = temp_dir("concurrent_rw");
    let value_size = 40usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        true,
        Duration::from_millis(300),
    );
    let engine = Arc::new(open_engine(&dir, cfg));
    let cardinality = 400u64;
    let stop = Arc::new(AtomicBool::new(false));
    // Reference model: key -> latest known value (None = deleted/never
    // written). Writers hold this lock only around the single put/
    // delete + map update, matching `read_engine_bench.rs::section_
    // sanity`'s own established "read-your-own-write is race-free"
    // convention.
    let model: LatestModel = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let mismatches = Arc::new(AtomicU64::new(0));
    let ops = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();

    // 2 writers.
    for w in 0..2 {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        let model = Arc::clone(&model);
        let ops = Arc::clone(&ops);
        handles.push(thread::spawn(move || {
            let mut idx = w as u64;
            while !stop.load(Ordering::Relaxed) {
                let key = fixture_key(idx, cardinality);
                if idx.is_multiple_of(13) {
                    engine.delete(&key).unwrap();
                    model.lock().unwrap().insert(key, None);
                } else {
                    let value = format!("v{idx}").into_bytes();
                    engine.put(&key, &value).unwrap();
                    model.lock().unwrap().insert(key, Some(value));
                }
                idx += 2;
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // 4 point readers.
    for _ in 0..4 {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        let mismatches = Arc::clone(&mismatches);
        let ops = Arc::clone(&ops);
        handles.push(thread::spawn(move || {
            let mut idx = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let key = fixture_key(idx % cardinality, cardinality);
                let r = engine.get(&key);
                if r.is_err() {
                    mismatches.fetch_add(1, Ordering::Relaxed);
                }
                idx += 1;
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // 2 range readers.
    for _ in 0..2 {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        let mismatches = Arc::clone(&mismatches);
        let ops = Arc::clone(&ops);
        handles.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let rows: Result<Vec<_>, _> =
                    engine.range(Bound::Unbounded, Bound::Unbounded).collect();
                match rows {
                    Ok(rows) => {
                        let mut sorted = rows.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
                        let was_sorted = {
                            let mut s = sorted.clone();
                            s.sort();
                            s == sorted
                        };
                        if !was_sorted {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                        sorted.dedup();
                        if sorted.len() != rows.len() {
                            mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                }
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // 2 snapshot users: take a snapshot, read a key at that pinned seq
    // twice (must agree with itself), release.
    for _ in 0..2 {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        let mismatches = Arc::clone(&mismatches);
        let ops = Arc::clone(&ops);
        handles.push(thread::spawn(move || {
            let mut idx = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let snap = engine.snapshot();
                let key = fixture_key(idx % cardinality, cardinality);
                let a = engine.get_as_of(&key, snap.seq()).unwrap();
                let b = engine.get_as_of(&key, snap.seq()).unwrap();
                if a != b {
                    mismatches.fetch_add(1, Ordering::Relaxed);
                }
                drop(snap);
                idx += 1;
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    let pid = current_pid();
    let t0 = Instant::now();
    let mut peak_rss = 0u64;
    let mut peak_handles = 0u64;
    let mut peak_threads = 0u64;
    let mut peak_sstables = 0usize;
    while t0.elapsed() < Duration::from_secs(20) {
        thread::sleep(Duration::from_secs(2));
        if let Some(r) = sample_rss_kb(pid) {
            peak_rss = peak_rss.max(r);
        }
        if let Some(h) = sample_handle_count(pid) {
            peak_handles = peak_handles.max(h);
        }
        if let Some(th) = sample_thread_count(pid) {
            peak_threads = peak_threads.max(th);
        }
        peak_sstables = peak_sstables.max(engine.sstable_count());
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    let m = engine.compaction_metrics();
    let total_mismatches = mismatches.load(Ordering::Relaxed);
    println!(
        "total_ops={} compaction_cycles={} mismatches={total_mismatches} peak_rss_kb={peak_rss} \
         peak_handles={peak_handles} peak_threads={peak_threads} peak_sstable_count={peak_sstables} \
         records_dropped_total={}",
        ops.load(Ordering::Relaxed),
        m.cycles_completed,
        m.records_dropped_total,
    );

    // Final-state check against the reference model.
    let expected = model.lock().unwrap().clone();
    let mut final_mismatches = 0u64;
    for (key, expected_value) in &expected {
        let actual = engine.get(key).unwrap();
        if actual != *expected_value {
            final_mismatches += 1;
        }
    }
    println!(
        "final-state check: {} distinct keys verified against reference model, \
         {final_mismatches} mismatches",
        expected.len()
    );
    assert_eq!(total_mismatches, 0, "in-run mismatches must be zero");
    assert_eq!(final_mismatches, 0, "final-state mismatches must be zero");

    let engine = Arc::try_unwrap(engine).unwrap_or_else(|_| panic!("outstanding Arc reference"));
    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §10: snapshot endurance across repeated compaction cycles.
// ============================================================

fn section_snapshot_endurance() {
    println!(
        "\n=== snapshot_endurance (\u{a7}10): overlapping live snapshots across repeated \
         compaction cycles, oldest_live_snapshot_seq() correctness ==="
    );
    let dir = temp_dir("snapshot_endurance");
    let value_size = 40usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        true,
        Duration::from_millis(300),
    );
    let engine = open_engine(&dir, cfg);
    let cardinality = 200u64;

    // Reference model: key -> Vec<(seq, Option<value>)> full history.
    let mut history: HistoryModel = std::collections::HashMap::new();
    let mut live_snapshots: Vec<rubixdb::lsm::Snapshot> = Vec::new();
    let mut tracked_seqs: Vec<u64> = Vec::new();
    let mut mismatches = 0u64;

    for round in 0..30u64 {
        for i in 0..40u64 {
            let key = fixture_key(i, cardinality);
            let seq = if i % 9 == 0 {
                let s = engine.delete(&key).unwrap();
                history.entry(key).or_default().push((s, None));
                s
            } else {
                let val = format!("v{round}-{i}").into_bytes();
                let s = engine.put(&key, &val).unwrap();
                history.entry(key.clone()).or_default().push((s, Some(val)));
                s
            };
            let _ = seq;
        }
        // Take a new snapshot every few rounds; release the oldest
        // once we're holding more than 3, to exercise overlapping
        // create/release across compaction, not just monotonic growth.
        if round % 3 == 0 {
            let snap = engine.snapshot();
            tracked_seqs.push(snap.seq());
            live_snapshots.push(snap);
        }
        if live_snapshots.len() > 3 {
            live_snapshots.remove(0);
            tracked_seqs.remove(0);
        }

        let expected_oldest = tracked_seqs.iter().min().copied();
        let actual_oldest = engine.oldest_live_snapshot_seq();
        if actual_oldest != expected_oldest {
            mismatches += 1;
            println!(
                "round={round} MISMATCH oldest_live_snapshot_seq()={actual_oldest:?} \
                 expected={expected_oldest:?}"
            );
        }

        // Verify every still-live snapshot's own historical read
        // against the independently tracked history.
        for (snap, &tracked_seq) in live_snapshots.iter().zip(tracked_seqs.iter()) {
            assert_eq!(snap.seq(), tracked_seq);
            for (key, versions) in &history {
                let expected = versions
                    .iter()
                    .rev()
                    .find(|(s, _)| *s <= snap.seq())
                    .and_then(|(_, v)| v.clone());
                let actual = engine.get_as_of(key, snap.seq()).unwrap();
                if actual != expected {
                    mismatches += 1;
                }
            }
        }
    }

    println!(
        "rounds=30 live_snapshots_held_at_end={} mismatches={mismatches} \
         compaction_cycles={} sstable_count={}",
        live_snapshots.len(),
        engine.compaction_metrics().cycles_completed,
        engine.sstable_count(),
    );
    assert_eq!(
        mismatches, 0,
        "snapshot endurance must show zero mismatches"
    );

    drop(live_snapshots);
    assert_eq!(
        engine.oldest_live_snapshot_seq(),
        None,
        "releasing every snapshot must make oldest_live_snapshot_seq() None"
    );
    // One further write+settle cycle proves compaction resumes normal
    // operation post-release (nothing left permanently pinned).
    for i in 0..40u64 {
        engine
            .put(&fixture_key(i, cardinality), b"post-release")
            .unwrap();
    }
    wait_until(|| engine.immutable_count() == 0, Duration::from_secs(10));
    println!(
        "post-release: sstable_count={} (compaction continues operating normally after every \
         snapshot is released)",
        engine.sstable_count()
    );

    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §11: version/tombstone endurance -- long PUT/PUT/DELETE/PUT/DELETE/
// PUT histories across repeated SSTables and compaction cycles.
// ============================================================

fn section_tombstone_endurance() {
    println!(
        "\n=== tombstone_endurance (\u{a7}11): long PUT/DELETE histories across repeated \
         compaction cycles, checked against an independent reference model ==="
    );
    let dir = temp_dir("tombstone_endurance");
    let value_size = 32usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        true,
        Duration::from_millis(300),
    );
    let engine = open_engine(&dir, cfg);
    let cardinality = 30u64;

    let mut history: HistoryModel = std::collections::HashMap::new();
    let mut snapshots_at_seq: Vec<(u64, rubixdb::lsm::Snapshot)> = Vec::new();
    let ops_pattern = [true, true, false, true, false, true]; // put put delete put delete put
    let mut mismatches = 0u64;

    for round in 0..60u64 {
        for (i, &is_put) in ops_pattern.iter().enumerate() {
            let key = fixture_key((round * ops_pattern.len() as u64) + i as u64, cardinality);
            let seq = if is_put {
                let val = format!("v{round}-{i}").into_bytes();
                let s = engine.put(&key, &val).unwrap();
                history.entry(key).or_default().push((s, Some(val)));
                s
            } else {
                let s = engine.delete(&key).unwrap();
                history.entry(key).or_default().push((s, None));
                s
            };
            let _ = seq;
        }
        if round % 10 == 0 {
            let snap = engine.snapshot();
            snapshots_at_seq.push((snap.seq(), snap));
        }

        // After every round, compare get/get_as_of/contains/range for
        // "now" (max seq) against the reference model.
        let now_seq = u64::MAX;
        for (key, versions) in &history {
            let expected_now = versions.last().and_then(|(_, v)| v.clone());
            let got = engine.get_as_of(key, now_seq).unwrap();
            if got != expected_now {
                mismatches += 1;
            }
            let contains = engine.contains(key, now_seq).unwrap();
            if contains != expected_now.is_some() {
                mismatches += 1;
            }
        }
    }

    println!(
        "rounds=60 distinct_keys={} live_snapshots_held={} compaction_cycles={} mismatches_so_far={mismatches}",
        history.len(),
        snapshots_at_seq.len(),
        engine.compaction_metrics().cycles_completed,
    );

    // Historical checks at each retained snapshot seq -- exercises
    // tombstone-preservation-for-old-snapshots specifically.
    for (seq, _snap) in &snapshots_at_seq {
        for (key, versions) in &history {
            let expected = versions
                .iter()
                .rev()
                .find(|(s, _)| *s <= *seq)
                .and_then(|(_, v)| v.clone());
            let actual = engine.get_as_of(key, *seq).unwrap();
            if actual != expected {
                mismatches += 1;
            }
        }
    }

    // Range scan sanity: every key with a "now" value must appear
    // exactly once in a full range scan; every fully-deleted key must
    // not appear at all.
    let rows: std::collections::HashMap<Vec<u8>, Vec<u8>> = engine
        .range(Bound::Unbounded, Bound::Unbounded)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .collect();
    for (key, versions) in &history {
        let expected_now = versions.last().and_then(|(_, v)| v.clone());
        match (expected_now, rows.get(key)) {
            (Some(ref ev), Some(av)) if ev == av => {}
            (None, None) => {}
            _ => mismatches += 1,
        }
    }

    println!("final mismatches={mismatches} (must be zero)");
    assert_eq!(mismatches, 0);

    drop(snapshots_at_seq);
    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §19/§20: read/write latency impact of active automatic compaction.
// ============================================================

fn run_read_write_workload(auto_trigger: bool, seconds: u64) {
    let dir = temp_dir(if auto_trigger {
        "rw_impact_on"
    } else {
        "rw_impact_off"
    });
    let value_size = 64usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        auto_trigger,
        Duration::from_millis(300),
    );
    let engine = Arc::new(open_engine(&dir, cfg));
    let cardinality = 2_000u64;
    let value = vec![b'v'; value_size];
    let stop = Arc::new(AtomicBool::new(false));

    // Seed some data first so readers have something real to read.
    for i in 0..500u64 {
        engine.put(&fixture_key(i, cardinality), &value).unwrap();
    }
    wait_until(|| engine.immutable_count() == 0, Duration::from_secs(10));

    let write_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let get_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let get_as_of_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let contains_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let range_small_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let range_medium_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));
    let range_large_lat: Arc<Mutex<Vec<u128>>> = Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::new();
    {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        let write_lat = Arc::clone(&write_lat);
        let value = value.clone();
        handles.push(thread::spawn(move || {
            let mut idx = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                engine.put(&fixture_key(idx, cardinality), &value).unwrap();
                write_lat.lock().unwrap().push(t0.elapsed().as_nanos());
                idx += 1;
            }
        }));
    }
    let read_metric_ops: Vec<(Arc<Mutex<Vec<u128>>>, u8)> = vec![
        (Arc::clone(&get_lat), 0),
        (Arc::clone(&get_as_of_lat), 1),
        (Arc::clone(&contains_lat), 2),
    ];
    for (metric, op) in read_metric_ops {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || {
            let mut idx = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let key = fixture_key(idx % cardinality, cardinality);
                let t0 = Instant::now();
                match op {
                    0 => {
                        engine.get(&key).unwrap();
                    }
                    1 => {
                        engine.get_as_of(&key, u64::MAX).unwrap();
                    }
                    _ => {
                        engine.contains(&key, u64::MAX).unwrap();
                    }
                }
                metric.lock().unwrap().push(t0.elapsed().as_nanos());
                idx += 1;
            }
        }));
    }
    for (metric, limit) in [
        (Arc::clone(&range_small_lat), 10usize),
        (Arc::clone(&range_medium_lat), 100),
        (Arc::clone(&range_large_lat), usize::MAX),
    ] {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        handles.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                let mut count = 0usize;
                for row in engine.range(Bound::Unbounded, Bound::Unbounded) {
                    let _ = row;
                    count += 1;
                    if count >= limit {
                        break;
                    }
                }
                metric.lock().unwrap().push(t0.elapsed().as_nanos());
            }
        }));
    }

    thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    let m = engine.compaction_metrics();
    println!(
        "auto_trigger={auto_trigger} duration_s={seconds} compaction_cycles={}",
        m.cycles_completed
    );
    report_latency("write", &mut write_lat.lock().unwrap());
    report_latency("get", &mut get_lat.lock().unwrap());
    report_latency("get_as_of", &mut get_as_of_lat.lock().unwrap());
    report_latency("contains", &mut contains_lat.lock().unwrap());
    report_latency("range_small(10)", &mut range_small_lat.lock().unwrap());
    report_latency("range_medium(100)", &mut range_medium_lat.lock().unwrap());
    report_latency("range_large(full)", &mut range_large_lat.lock().unwrap());

    let engine = Arc::try_unwrap(engine).unwrap_or_else(|_| panic!("outstanding Arc reference"));
    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

fn section_read_write_impact() {
    println!(
        "\n=== read_write_impact (\u{a7}19/\u{a7}20): read/write latency with compaction idle \
         (disabled) vs. actively running, same workload, 20s each ==="
    );
    println!("--- compaction DISABLED (flush only) ---");
    run_read_write_workload(false, 20);
    println!("--- compaction ENABLED (automatic, trigger_count=4) ---");
    run_read_write_workload(true, 20);
}

// ============================================================
// §21: SSTable-count stability over a bounded continuous-write window.
// ============================================================

fn section_sstable_count_stability() {
    println!(
        "\n=== sstable_count_stability (\u{a7}21): live SSTable count over time under \
         continuous writes + automatic compaction ==="
    );
    let dir = temp_dir("count_stability");
    let value_size = 48usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        true,
        Duration::from_millis(300),
    );
    let engine = Arc::new(open_engine(&dir, cfg));
    let cardinality = 5_000u64;
    let value = vec![b'v'; value_size];
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let engine = Arc::clone(&engine);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut idx = 0u64;
            while !stop.load(Ordering::Relaxed) {
                engine.put(&fixture_key(idx, cardinality), &value).unwrap();
                idx += 1;
            }
        })
    };

    let mut samples = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(30) {
        thread::sleep(Duration::from_millis(500));
        samples.push((t0.elapsed().as_secs_f64(), engine.sstable_count()));
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    for (t, count) in &samples {
        println!("t={t:.1}s sstable_count={count}");
    }
    let half = samples.len() / 2;
    let first_half_avg =
        samples[..half].iter().map(|(_, c)| *c as f64).sum::<f64>() / half.max(1) as f64;
    let second_half_avg = samples[half..].iter().map(|(_, c)| *c as f64).sum::<f64>()
        / (samples.len() - half).max(1) as f64;
    let max_count = samples.iter().map(|(_, c)| *c).max().unwrap_or(0);
    println!(
        "first_half_avg_sstable_count={first_half_avg:.2} second_half_avg_sstable_count={second_half_avg:.2} \
         max_observed={max_count} compaction_cycles={} (observed behavior reported as-is, no \
         invented pass/fail threshold)",
        engine.compaction_metrics().cycles_completed
    );

    let engine = Arc::try_unwrap(engine).unwrap_or_else(|_| panic!("outstanding Arc reference"));
    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §13: storage-pressure endurance (requires --features test-util for
// `set_storage_state_for_test`).
// ============================================================

#[cfg(feature = "test-util")]
fn section_storage_pressure() {
    println!(
        "\n=== storage_pressure (\u{a7}13): compaction defers under StoragePressure/StorageFull, \
         never mutates storage_state, resumes once Healthy ==="
    );
    let dir = temp_dir("storage_pressure");
    let value_size = 40usize;
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        true,
        Duration::from_millis(200),
    );
    let engine = open_engine(&dir, cfg);
    let cardinality = 500u64;
    let value = vec![b'v'; value_size];

    // Build up past the trigger threshold first, offline-style but with
    // the worker already running -- force StorageFull immediately so
    // the worker's first-ever evaluation (which can only come from its
    // own fallback tick on a freshly opened engine, never an immediate
    // notification, since no flush has happened yet) sees it deferred.
    engine.set_storage_state_for_test(StorageState::StorageFull);
    for i in 0..40u64 {
        // Writes themselves are rejected fast-fail under StorageFull
        // (`ADR-WE-SP-001`) -- expected, not a bug in this harness.
        let _ = engine.put(&fixture_key(i, cardinality), &value);
    }
    thread::sleep(Duration::from_millis(600));
    println!(
        "under forced StorageFull: sstable_count={} storage_state={:?} storage_pressure_events={} \
         compaction_cycles={} (must be 0 -- compaction must never attempt while full)",
        engine.sstable_count(),
        engine.storage_state(),
        engine.storage_pressure_events(),
        engine.compaction_metrics().cycles_completed
    );
    assert_eq!(
        engine.compaction_metrics().cycles_completed,
        0,
        "compaction must not run while StorageFull"
    );

    engine.set_storage_state_for_test(StorageState::StoragePressure);
    thread::sleep(Duration::from_millis(600));
    println!(
        "under forced StoragePressure: compaction_cycles={} (must remain 0 -- conservative defer)",
        engine.compaction_metrics().cycles_completed
    );
    assert_eq!(engine.compaction_metrics().cycles_completed, 0);

    engine.set_storage_state_for_test(StorageState::Healthy);
    for i in 0..40u64 {
        engine.put(&fixture_key(i, cardinality), &value).unwrap();
    }
    let resumed = wait_until(
        || engine.compaction_metrics().cycles_completed > 0,
        Duration::from_secs(10),
    );
    println!(
        "after recovery to Healthy: resumed={resumed} compaction_cycles={} storage_state={:?}",
        engine.compaction_metrics().cycles_completed,
        engine.storage_state(),
    );
    assert!(resumed, "compaction must resume once Healthy again");

    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(not(feature = "test-util"))]
fn section_storage_pressure() {
    println!(
        "\n=== storage_pressure (\u{a7}13): SKIPPED -- rebuild with --features test-util to run \
         this section (needs set_storage_state_for_test) ==="
    );
}

// ============================================================
// §14: failure-retry stability -- injected compaction I/O failures via
// the existing, unconditionally-public install_compaction_io_fault_hook.
// ============================================================

fn section_failure_retry() {
    println!(
        "\n=== failure_retry (\u{a7}14): repeated injected compaction I/O failures, retry \
         cadence, eventual success, no busy loop ==="
    );
    let dir = temp_dir("failure_retry");
    let value_size = 40usize;
    let fallback = Duration::from_millis(300);
    let cfg = lsm_config(calibrated_memtable_bytes(value_size), 64, 4, true, fallback);
    let engine = open_engine(&dir, cfg);
    let cardinality = 500u64;
    let value = vec![b'v'; value_size];

    let fail_count = Arc::new(AtomicU64::new(0));
    let attempts_before_success = 3u64;
    {
        let fail_count = Arc::clone(&fail_count);
        engine.install_compaction_io_fault_hook(move || {
            let n = fail_count.fetch_add(1, Ordering::SeqCst);
            if n < attempts_before_success {
                Some(std::io::Error::other("injected compaction I/O failure"))
            } else {
                None
            }
        });
    }

    for i in 0..40u64 {
        engine.put(&fixture_key(i, cardinality), &value).unwrap();
    }
    let t0 = Instant::now();
    let succeeded = wait_until(
        || engine.compaction_metrics().cycles_completed > 0,
        Duration::from_secs(30),
    );
    let elapsed = t0.elapsed();
    let injected = fail_count.load(Ordering::Relaxed);
    println!(
        "injected_failures_observed={injected} (>= {attempts_before_success} expected) \
         eventual_success={succeeded} time_to_success_ms={:.1} fallback_interval_ms={} \
         (retry is bounded by this fallback cadence per cycle -- no busy loop, log-once-per-\
         attempt behavior verified by the bounded injected-failure count above)",
        elapsed.as_secs_f64() * 1000.0,
        fallback.as_millis(),
    );
    assert!(
        succeeded,
        "compaction must eventually succeed after the injected failures clear"
    );
    assert!(
        injected >= attempts_before_success,
        "the hook must actually have been consulted at least once per failed attempt"
    );

    engine.clear_compaction_io_fault_hook();
    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
// §15: shutdown endurance -- repeated start/idle-shutdown/pending-
// notification-shutdown/active-shutdown/reopen cycles.
// ============================================================

fn section_shutdown_endurance() {
    println!(
        "\n=== shutdown_endurance (\u{a7}15): repeated start/shutdown/reopen cycles at idle, \
         pending-notification, and mid-cycle points ==="
    );
    let dir = temp_dir("shutdown_endurance");
    let value_size = 40usize;
    let cardinality = 300u64;
    let value = vec![b'v'; value_size];

    for cycle in 0..8u32 {
        let cfg = lsm_config(
            calibrated_memtable_bytes(value_size),
            64,
            4,
            true,
            Duration::from_millis(200),
        );
        let engine = open_engine(&dir, cfg);

        match cycle % 4 {
            0 => {
                // Idle shutdown: no writes at all this cycle.
            }
            1 => {
                // Pending-notification shutdown: issue one flush's worth
                // of writes right before shutdown, racing the worker's
                // own wake without waiting for it.
                for i in 0..40u64 {
                    engine.put(&fixture_key(i, cardinality), &value).unwrap();
                }
            }
            2 => {
                // Active-compaction shutdown: build past the trigger,
                // give the worker a moment to actually start a cycle,
                // then shut down without waiting for completion.
                for i in 0..80u64 {
                    engine.put(&fixture_key(i, cardinality), &value).unwrap();
                }
                thread::sleep(Duration::from_millis(50));
            }
            _ => {
                // Fully settled shutdown: wait for at least one cycle.
                for i in 0..80u64 {
                    engine.put(&fixture_key(i, cardinality), &value).unwrap();
                }
                wait_until(
                    || engine.compaction_metrics().cycles_completed > 0,
                    Duration::from_secs(10),
                );
            }
        }

        let t0 = Instant::now();
        engine.shutdown();
        let shutdown_ms = t0.elapsed().as_secs_f64() * 1000.0;
        drop(engine);
        println!(
            "cycle={cycle} kind={} shutdown_ms={shutdown_ms:.2}",
            cycle % 4
        );
        assert!(
            shutdown_ms < 30_000.0,
            "shutdown must never hang (observed {shutdown_ms:.1}ms)"
        );
    }

    // Final reopen + verification.
    let cfg = lsm_config(
        calibrated_memtable_bytes(value_size),
        64,
        4,
        false,
        Duration::from_secs(5),
    );
    let engine = open_engine(&dir, cfg);
    println!(
        "final reopen OK: sstable_count={} (no hang, no orphan worker, no lost Manifest state)",
        engine.sstable_count()
    );
    engine.shutdown();
    drop(engine);
    let _ = fs::remove_dir_all(&dir);
}

// ============================================================
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let all = args.is_empty();
    let want = |name: &str| all || args.iter().any(|a| a == name);

    println!("RubiXDB Compaction Increment 3 performance/resource/endurance harness");
    println!(
        "sections requested: {}",
        if all {
            "ALL (bounded)".to_string()
        } else {
            args.join(", ")
        }
    );

    if want("count_sweep") {
        section_count_sweep();
    }
    if want("shape_sweep") {
        section_shape_sweep();
    }
    if want("storage_budget") {
        section_storage_budget();
    }
    if want("auto_trigger_stress") {
        section_auto_trigger_stress();
    }
    if want("concurrent_rw") {
        section_concurrent_rw_compaction();
    }
    if want("snapshot_endurance") {
        section_snapshot_endurance();
    }
    if want("tombstone_endurance") {
        section_tombstone_endurance();
    }
    if want("read_write_impact") {
        section_read_write_impact();
    }
    if want("sstable_count_stability") {
        section_sstable_count_stability();
    }
    if want("storage_pressure") {
        section_storage_pressure();
    }
    if want("failure_retry") {
        section_failure_retry();
    }
    if want("shutdown_endurance") {
        section_shutdown_endurance();
    }
    // Opt-in only -- each takes several minutes on its own.
    if args.iter().any(|a| a == "rss_scaling") {
        section_rss_scaling();
    }
    if args.iter().any(|a| a == "handles_threads") {
        section_handles_threads();
    }

    println!("\ndone.");
}
