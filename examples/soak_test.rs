//! Phase 3B soak-test harness (operating brief §10-§11, `PHASE3B_TEST_
//! PLAN.md`). Runs the production `execution::batch_coordinator::
//! BatchCoordinatorPool` under a sustained workload for a configurable
//! duration, sampling throughput/latency/queue/sync/WAL counters on a
//! fixed interval, and prints a start/early/mid/end comparison at the
//! end for drift analysis (§11).
//!
//! **Low-contention observability, not just a benchmark.** Per-op
//! latency is *not* pushed into one shared structure from every writer
//! thread on every call — Phase 1's own `batch_timing` module doc
//! comment (`src/wal/group_commit.rs`) documents a real, measured
//! throughput collapse (~63,000 -> ~10,000 ops/sec at M1.3 scale) the
//! last time this codebase tried that. Instead: each writer thread owns
//! one slot in `LatencySlots` (indexed by thread id, `Mutex<Vec<u64>>`
//! per slot) and only ever locks *its own* slot — genuinely uncontended
//! in normal operation, since no other writer thread ever touches it —
//! and only for a sampled subset of operations (`SAMPLE_EVERY`), not
//! every one. A dedicated sampler thread periodically drains every slot
//! (the only cross-thread contention point, and only against one writer
//! at a time, briefly) to compute that window's percentiles. This is
//! exactly the "per-thread/per-worker local counters with periodic
//! aggregation" design the Phase 3B observability brief (§15) asks for,
//! reused here rather than built twice.
//!
//! Usage: `cargo run --release --example soak_test -- <writer_count>
//! <duration_secs> [sample_interval_secs=30]`
//!
//! **Honesty note** (see `PHASE3B_TEST_RESULTS.md`): a multi-hour soak,
//! as the operating brief specifies, was not run interactively in this
//! session — this harness was run for a bounded, explicitly-recorded
//! duration instead, with the shortfall flagged, not hidden.
//!
//! **Known limitation, discovered by an actual run on this project's own
//! development machine, not merely anticipated**: the final recovery-
//! verification step (`FileWal::open_for_recovery`) materializes every
//! recovered record in one `Vec` — there is no streaming recovery API in
//! this crate yet. A real 1,000-writer, 900-second run (~85M records,
//! ~2.9 GiB on disk) exhausted host RAM at *that* step and was killed by
//! the OS — **after** a fully clean, zero-error, no-leak, no-degradation
//! 900-second write-path soak had already completed and been recorded.
//! The write path itself was never implicated; a supplementary shorter
//! 1,000-writer run (90s, ~8.5M records) confirmed the full soak +
//! recovery cycle succeeds cleanly at a scale this host can actually
//! recover. See `PHASE3B_TEST_RESULTS.md` §8 for the full account.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::{BatchCoordinatorConfig, BatchCoordinatorPool};
use rubixdb::wal::{FileWal, SyncMode, Wal, WalConfig, WalOpOwned};
use rubixdb::EngineError;

const SAMPLE_EVERY: u64 = 20;

struct LatencySlots {
    slots: Vec<Mutex<Vec<u64>>>,
}

impl LatencySlots {
    fn new(n: usize) -> Self {
        LatencySlots {
            slots: (0..n).map(|_| Mutex::new(Vec::new())).collect(),
        }
    }

    /// Called only by thread `idx` itself — uncontended in practice.
    fn record(&self, idx: usize, latency_ns: u64) {
        self.slots[idx]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(latency_ns);
    }

    /// Drains every slot and returns the merged, sorted sample for this
    /// window — the only point any cross-thread contention exists, and
    /// only briefly, once per sample interval.
    fn drain_sorted(&self) -> Vec<u64> {
        let mut merged = Vec::new();
        for slot in &self.slots {
            let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
            merged.append(&mut guard);
        }
        merged.sort_unstable();
        merged
    }
}

fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * pct) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

/// Windows-only, external-tool RSS sample — `tasklist` via `std::process::
/// Command`, not a new Cargo dependency (no `windows-sys`/`winapi`
/// added; see `PHASE3B_ADR.md` for why a new crate wasn't justified for
/// this one harness-only diagnostic).
fn sample_rss_kb() -> Option<u64> {
    let pid = std::process::id();
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?;
    let fields: Vec<&str> = line.split("\",\"").collect();
    let mem_field = fields.get(4)?.trim_matches('"').trim();
    let digits: String = mem_field.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<u64>().ok()
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_soak_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

struct Sample {
    t_secs: f64,
    ops_per_sec: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    queue_depth: usize,
    sync_attempts: u64,
    records_total: u64,
    avg_batch_records: f64,
    rss_kb: Option<u64>,
    completed_ok: u64,
    completed_err: u64,
    rejected_backpressure: u64,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: soak_test <writer_count> <duration_secs> [sample_interval_secs=30]");
        std::process::exit(2);
    }
    let writer_count: usize = args[1].parse().expect("writer_count must be a usize");
    let duration_secs: u64 = args[2].parse().expect("duration_secs must be a u64");
    let sample_interval_secs: u64 = args
        .get(3)
        .map(|s| s.parse().expect("sample_interval_secs must be a u64"))
        .unwrap_or(30);

    let dir = temp_dir("run");
    println!("soak_test: dir={}", dir.display());
    let config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let (wal, _) = FileWal::open_for_recovery(&dir, config).unwrap();
    let committer = rubixdb::wal::GroupCommitter::new(wal).unwrap();
    let pool_config = BatchCoordinatorConfig {
        queue_capacity: (writer_count * 4).max(64),
        max_queued_bytes: 64 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(30),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: (writer_count * 4).max(65536),
    };
    let pool = Arc::new(BatchCoordinatorPool::new(committer, pool_config).unwrap());
    let latencies = Arc::new(LatencySlots::new(writer_count));
    let stop = Arc::new(AtomicBool::new(false));
    let total_ops = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_secs);

    let writer_handles: Vec<_> = (0..writer_count)
        .map(|t| {
            let pool = Arc::clone(&pool);
            let latencies = Arc::clone(&latencies);
            let stop = Arc::clone(&stop);
            let total_ops = Arc::clone(&total_ops);
            thread::spawn(move || {
                let mut i: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    let key = format!("t{t}-{i}");
                    let op_started = Instant::now();
                    let op = WalOpOwned::Put {
                        key: key.into_bytes(),
                        value: b"v".to_vec(),
                    };
                    match pool.submit(op) {
                        Ok(completion) => match completion.wait() {
                            Ok(_) => {
                                total_ops.fetch_add(1, Ordering::Relaxed);
                                if i.is_multiple_of(SAMPLE_EVERY) {
                                    let ns = u64::try_from(op_started.elapsed().as_nanos())
                                        .unwrap_or(u64::MAX);
                                    latencies.record(t, ns);
                                }
                            }
                            Err(_) => {
                                // Pool is draining/failed near the soak's own
                                // end — not treated as a soak failure by
                                // itself; the final recovery check is what
                                // actually validates correctness.
                            }
                        },
                        Err(EngineError::Timeout { .. }) => {
                            // Backpressure — retry, same as every other
                            // load-test harness in this project.
                        }
                        Err(_) => {}
                    }
                    i += 1;
                }
            })
        })
        .collect();

    println!(
        "soak_test: writer_count={writer_count} duration_secs={duration_secs} \
         sample_interval_secs={sample_interval_secs} SAMPLE_EVERY={SAMPLE_EVERY}"
    );
    println!(
        "t_secs,ops_per_sec,p50_ms,p95_ms,p99_ms,max_ms,queue_depth,sync_attempts,\
         records_total,avg_batch_records,rss_kb,completed_ok,completed_err,rejected_backpressure"
    );

    let start = Instant::now();
    let mut last_completed_ok: u64 = 0;
    let mut last_t = start;
    let mut samples: Vec<Sample> = Vec::new();
    while Instant::now() < deadline {
        let step = sample_interval_secs.min(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
                .max(1),
        );
        thread::sleep(Duration::from_secs(step));

        let stats = pool.stats();
        let now = Instant::now();
        let window_secs = now.duration_since(last_t).as_secs_f64();
        let ops_per_sec = if window_secs > 0.0 {
            (stats.completed_ok.saturating_sub(last_completed_ok)) as f64 / window_secs
        } else {
            0.0
        };
        last_completed_ok = stats.completed_ok;
        last_t = now;

        let sorted = latencies.drain_sorted();
        let rss_kb = sample_rss_kb();
        let sample = Sample {
            t_secs: now.duration_since(start).as_secs_f64(),
            ops_per_sec,
            p50_ms: ms(percentile(&sorted, 0.50)),
            p95_ms: ms(percentile(&sorted, 0.95)),
            p99_ms: ms(percentile(&sorted, 0.99)),
            max_ms: ms(*sorted.last().unwrap_or(&0)),
            queue_depth: stats.queue_depth,
            sync_attempts: stats.committer_stats.sync_attempts,
            records_total: stats.committer_stats.records_total,
            avg_batch_records: stats.committer_stats.avg_batch_records(),
            rss_kb,
            completed_ok: stats.completed_ok,
            completed_err: stats.completed_err,
            rejected_backpressure: stats.rejected_backpressure,
        };
        println!(
            "{:.1},{:.0},{:.3},{:.3},{:.3},{:.3},{},{},{},{:.2},{},{},{},{}",
            sample.t_secs,
            sample.ops_per_sec,
            sample.p50_ms,
            sample.p95_ms,
            sample.p99_ms,
            sample.max_ms,
            sample.queue_depth,
            sample.sync_attempts,
            sample.records_total,
            sample.avg_batch_records,
            sample
                .rss_kb
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NA".to_string()),
            sample.completed_ok,
            sample.completed_err,
            sample.rejected_backpressure,
        );
        samples.push(sample);
    }

    stop.store(true, Ordering::Relaxed);
    for h in writer_handles {
        let _ = h.join();
    }

    println!(
        "soak_test: total_ops_completed_via_local_counter={}",
        total_ops.load(Ordering::Relaxed)
    );

    if samples.len() >= 2 {
        let start_s = &samples[0];
        let end_s = &samples[samples.len() - 1];
        let mid_s = &samples[samples.len() / 2];
        println!(
            "soak_analysis: start ops/sec={:.0} rss_kb={:?} p99_ms={:.3}",
            start_s.ops_per_sec, start_s.rss_kb, start_s.p99_ms
        );
        println!(
            "soak_analysis: mid   ops/sec={:.0} rss_kb={:?} p99_ms={:.3}",
            mid_s.ops_per_sec, mid_s.rss_kb, mid_s.p99_ms
        );
        println!(
            "soak_analysis: end   ops/sec={:.0} rss_kb={:?} p99_ms={:.3}",
            end_s.ops_per_sec, end_s.rss_kb, end_s.p99_ms
        );
        if let (Some(start_rss), Some(end_rss)) = (start_s.rss_kb, end_s.rss_kb) {
            let growth_pct = if start_rss > 0 {
                100.0 * (end_rss as f64 - start_rss as f64) / start_rss as f64
            } else {
                0.0
            };
            println!(
                "soak_analysis: rss_growth_kb={} rss_growth_pct={:.1}",
                end_rss as i64 - start_rss as i64,
                growth_pct
            );
        }
        let throughput_drop_pct =
            100.0 * (start_s.ops_per_sec - end_s.ops_per_sec) / start_s.ops_per_sec.max(1.0);
        println!("soak_analysis: throughput_drop_pct_start_to_end={throughput_drop_pct:.1}");
    }

    let report = pool.shutdown();
    println!(
        "soak_test: shutdown pool_state={:?} fully_drained={}",
        report.pool_state, report.fully_drained
    );
    let pool = Arc::try_unwrap(pool).unwrap_or_else(|_| panic!("outstanding Arc<Pool> reference"));
    drop(pool.into_inner().unwrap());

    // **Discovered during Phase 3B soak testing, not a write-path defect**
    // (`PHASE3B_TEST_RESULTS.md` §8): `FileWal::open_for_recovery`
    // materializes every recovered record as an owned `(u64, WalOpOwned)`
    // in one `Vec` — there is no streaming/iterator recovery API in this
    // crate yet. For a very large accumulated WAL (tens of millions of
    // records), this step's own memory demand can be substantial
    // (a real ~85M-record run on this project's own development machine
    // exhausted ~16 GiB of host RAM during exactly this call, killing the
    // process *after* a fully clean, zero-error 900s write-path soak had
    // already completed — the write path itself was never at fault).
    // Warn loudly rather than silently risk repeating that on a
    // memory-constrained host; this is a known limitation of the current
    // recovery API surface, not something this harness works around.
    let total_ops_completed = total_ops.load(Ordering::Relaxed);
    if total_ops_completed > 20_000_000 {
        eprintln!(
            "soak_test: WARNING — about to recover {total_ops_completed} records via \
             FileWal::open_for_recovery, which materializes every record in memory at once \
             (no streaming recovery API exists yet). This has been observed to exhaust host \
             RAM on a large run — see PHASE3B_TEST_RESULTS.md §8. Proceeding anyway; if this \
             process is killed, that is this known limitation, not a write-path failure — the \
             soak's own throughput/latency/RSS samples above already recorded a valid result \
             independent of this step."
        );
    }
    let recovery_started = Instant::now();
    let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
    let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
    let recovery_ok = replay.corrupted_segments.is_empty();
    println!(
        "soak_test: recovery corrupted_segments={} records_recovered={} recovery_ms={:.1} => {}",
        replay.corrupted_segments.len(),
        replay.records.len(),
        recovery_ms,
        if recovery_ok { "OK" } else { "FAILED" }
    );
    let mut gap_free = true;
    for (i, (seq, _)) in replay.records.iter().enumerate() {
        if *seq != (i as u64) + 1 {
            gap_free = false;
            println!("soak_test: SEQUENCE GAP at index {i}: seq={seq}");
            break;
        }
    }
    println!("soak_test: sequences_gap_free={gap_free}");

    let _ = fs::remove_dir_all(&dir);
    if !recovery_ok || !gap_free {
        std::process::exit(1);
    }
}
