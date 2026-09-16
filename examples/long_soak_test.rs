//! Phase 3C true long-duration soak harness (operating brief §3-§4,
//! `PHASE3C_TEST_PLAN.md`). Extends `examples/soak_test.rs` (Phase 3B)
//! with the two things a genuinely multi-hour run needs that a
//! 900-second run does not:
//!
//! 1. **Periodic checkpointing (`GroupCommitter::purge_before`,
//!    Phase 3C-new)**, so the live WAL's own on-disk/recoverable
//!    footprint stays bounded for the run's entire duration — a raw,
//!    unbounded multi-hour run at full throughput would accumulate far
//!    more records than the recovery-memory limitation
//!    (`PHASE3B_ADR.md` ADR-P3B-5) can safely recover in one
//!    `open_for_recovery` call at the end. This mirrors how a real
//!    deployment bounds its own WAL via checkpointing, not a workaround
//!    invented only for this test.
//! 2. **CPU sampling** alongside RSS, via one `powershell.exe`
//!    `Get-Process` call per sample (no new Cargo dependency).
//!
//! Usage: `cargo run --release --example long_soak_test --
//! <writer_count> <duration_secs> [sample_interval_secs=120]
//! [retention_records=5000000] [purge_interval_secs=120]`
//!
//! Every metric operating brief §3 asks for is sampled: ops/sec, p50/
//! p95/p99/max latency, queue depth, records/batch, records/sync, sync
//! latency (via `sync_attempts` delta), CPU, RSS, submitted, completed,
//! failed, timed out (approximated as `rejected_backpressure`, the only
//! submission-time-timeout counter this codebase has — see
//! `PHASE3C_TEST_RESULTS.md` for why a distinct post-accept-timeout
//! counter still does not exist, an honestly-carried-forward Phase 3B
//! gap), backpressure, segment rotations, highest sequence,
//! `durable_through`.

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

const SAMPLE_EVERY: u64 = 50;

struct LatencySlots {
    slots: Vec<Mutex<Vec<u64>>>,
}

impl LatencySlots {
    fn new(n: usize) -> Self {
        LatencySlots {
            slots: (0..n).map(|_| Mutex::new(Vec::new())).collect(),
        }
    }
    fn record(&self, idx: usize, latency_ns: u64) {
        self.slots[idx]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(latency_ns);
    }
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

/// One `Get-Process` call returns both RSS (bytes) and cumulative CPU
/// time (seconds, all cores combined) — cheaper than two separate
/// external-process spawns per sample. `powershell.exe` via
/// `std::process::Command`, not a new Cargo dependency.
fn sample_process_metrics(pid: u32) -> Option<(u64, f64)> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p = Get-Process -Id {pid}; Write-Output ($p.WorkingSet64.ToString() + ',' + \
                 $p.CPU.ToString())"
            ),
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.trim();
    let mut parts = line.split(',');
    let rss_bytes: u64 = parts.next()?.trim().parse().ok()?;
    let cpu_secs: f64 = parts.next()?.trim().parse().ok()?;
    Some((rss_bytes / 1024, cpu_secs))
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_long_soak_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

#[derive(Clone, Copy)]
struct Sample {
    t_secs: f64,
    ops_per_sec: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    queue_depth: usize,
    queue_capacity: usize,
    queued_bytes: usize,
    sync_attempts: u64,
    sync_failures: u64,
    records_total: u64,
    avg_batch_records: f64,
    rss_kb: Option<u64>,
    cpu_pct: Option<f64>,
    submitted: u64,
    completed_ok: u64,
    completed_err: u64,
    rejected_backpressure: u64,
    segment_rotations: u64,
    highest_sequence: u64,
    durable_through: u64,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: long_soak_test <writer_count> <duration_secs> \
             [sample_interval_secs=120] [retention_records=5000000] [purge_interval_secs=120]"
        );
        std::process::exit(2);
    }
    let writer_count: usize = args[1].parse().expect("writer_count must be a usize");
    let duration_secs: u64 = args[2].parse().expect("duration_secs must be a u64");
    let sample_interval_secs: u64 = args
        .get(3)
        .map(|s| s.parse().expect("sample_interval_secs must be a u64"))
        .unwrap_or(120);
    let retention_records: u64 = args
        .get(4)
        .map(|s| s.parse().expect("retention_records must be a u64"))
        .unwrap_or(5_000_000);
    let purge_interval_secs: u64 = args
        .get(5)
        .map(|s| s.parse().expect("purge_interval_secs must be a u64"))
        .unwrap_or(120);

    let dir = temp_dir("run");
    println!("long_soak_test: dir={}", dir.display());
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
    let pid = std::process::id();

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
                        Ok(completion) => {
                            if completion.wait().is_ok() {
                                total_ops.fetch_add(1, Ordering::Relaxed);
                                if i.is_multiple_of(SAMPLE_EVERY) {
                                    let ns = u64::try_from(op_started.elapsed().as_nanos())
                                        .unwrap_or(u64::MAX);
                                    latencies.record(t, ns);
                                }
                            }
                        }
                        Err(EngineError::Timeout { .. }) => {}
                        Err(_) => {}
                    }
                    i += 1;
                }
            })
        })
        .collect();

    // Dedicated checkpoint thread: periodically purges everything below
    // (durable_through - retention_records), keeping the live,
    // recoverable WAL bounded for the run's entire duration — see this
    // module's doc comment for why.
    let checkpoint_pool = Arc::clone(&pool);
    let checkpoint_stop = Arc::clone(&stop);
    let checkpoint_handle = thread::spawn(move || {
        let mut total_purged_segments = 0usize;
        while !checkpoint_stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_secs(purge_interval_secs));
            if checkpoint_stop.load(Ordering::Relaxed) {
                break;
            }
            let durable_through = checkpoint_pool.stats().committer_stats.durable_through;
            let watermark = durable_through.saturating_sub(retention_records);
            if watermark == 0 {
                continue;
            }
            match checkpoint_pool.purge_before(watermark) {
                Ok(removed) => {
                    total_purged_segments += removed.len();
                    if !removed.is_empty() {
                        println!(
                            "long_soak_test: checkpoint purged {} segment(s) below seq {} \
                             (cumulative segments purged: {total_purged_segments})",
                            removed.len(),
                            watermark
                        );
                    }
                }
                Err(e) => {
                    eprintln!("long_soak_test: checkpoint purge_before failed: {e}");
                }
            }
        }
        total_purged_segments
    });

    println!(
        "long_soak_test: writer_count={writer_count} duration_secs={duration_secs} \
         sample_interval_secs={sample_interval_secs} retention_records={retention_records} \
         purge_interval_secs={purge_interval_secs} SAMPLE_EVERY={SAMPLE_EVERY}"
    );
    println!(
        "t_secs,ops_per_sec,p50_ms,p95_ms,p99_ms,max_ms,queue_depth,queue_capacity,queued_bytes,\
         sync_attempts,sync_failures,records_total,avg_batch_records,rss_kb,cpu_pct,submitted,\
         completed_ok,completed_err,rejected_backpressure,segment_rotations,highest_sequence,\
         durable_through"
    );

    let start = Instant::now();
    let mut last_completed_ok: u64 = 0;
    let mut last_t = start;
    let mut last_cpu_secs: Option<f64> = None;
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
        let proc_metrics = sample_process_metrics(pid);
        let rss_kb = proc_metrics.map(|(rss, _)| rss);
        let cpu_pct = proc_metrics.and_then(|(_, cpu_secs)| {
            let pct = last_cpu_secs.map(|prev| 100.0 * (cpu_secs - prev) / window_secs.max(0.001));
            last_cpu_secs = Some(cpu_secs);
            pct
        });

        let sample = Sample {
            t_secs: now.duration_since(start).as_secs_f64(),
            ops_per_sec,
            p50_ms: ms(percentile(&sorted, 0.50)),
            p95_ms: ms(percentile(&sorted, 0.95)),
            p99_ms: ms(percentile(&sorted, 0.99)),
            max_ms: ms(*sorted.last().unwrap_or(&0)),
            queue_depth: stats.queue_depth,
            queue_capacity: stats.queue_capacity,
            queued_bytes: stats.queued_bytes,
            sync_attempts: stats.committer_stats.sync_attempts,
            sync_failures: stats.committer_stats.sync_failures(),
            records_total: stats.committer_stats.records_total,
            avg_batch_records: stats.committer_stats.avg_batch_records(),
            rss_kb,
            cpu_pct,
            submitted: stats.submitted,
            completed_ok: stats.completed_ok,
            completed_err: stats.completed_err,
            rejected_backpressure: stats.rejected_backpressure,
            segment_rotations: stats.committer_stats.segment_rotations,
            highest_sequence: stats.committer_stats.highest_sequence,
            durable_through: stats.committer_stats.durable_through,
        };
        println!(
            "{:.1},{:.0},{:.3},{:.3},{:.3},{:.3},{},{},{},{},{},{},{:.2},{},{},{},{},{},{},{},{},{}",
            sample.t_secs,
            sample.ops_per_sec,
            sample.p50_ms,
            sample.p95_ms,
            sample.p99_ms,
            sample.max_ms,
            sample.queue_depth,
            sample.queue_capacity,
            sample.queued_bytes,
            sample.sync_attempts,
            sample.sync_failures,
            sample.records_total,
            sample.avg_batch_records,
            sample
                .rss_kb
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NA".to_string()),
            sample
                .cpu_pct
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "NA".to_string()),
            sample.submitted,
            sample.completed_ok,
            sample.completed_err,
            sample.rejected_backpressure,
            sample.segment_rotations,
            sample.highest_sequence,
            sample.durable_through,
        );
        samples.push(sample);
    }

    stop.store(true, Ordering::Relaxed);
    for h in writer_handles {
        let _ = h.join();
    }
    let total_purged_segments = checkpoint_handle.join().unwrap_or(0);

    println!(
        "long_soak_test: total_ops_completed_via_local_counter={} total_segments_purged={}",
        total_ops.load(Ordering::Relaxed),
        total_purged_segments
    );

    if samples.len() >= 2 {
        let start_s = &samples[0];
        let end_s = &samples[samples.len() - 1];
        let mid_s = &samples[samples.len() / 2];
        for (label, s) in [("start", start_s), ("mid", mid_s), ("end", end_s)] {
            println!(
                "long_soak_analysis: {label:<5} ops/sec={:.0} rss_kb={:?} cpu_pct={:?} \
                 p99_ms={:.3} queue_depth={} durable_through={}",
                s.ops_per_sec, s.rss_kb, s.cpu_pct, s.p99_ms, s.queue_depth, s.durable_through
            );
        }
        if let (Some(start_rss), Some(end_rss)) = (start_s.rss_kb, end_s.rss_kb) {
            let growth_pct = if start_rss > 0 {
                100.0 * (end_rss as f64 - start_rss as f64) / start_rss as f64
            } else {
                0.0
            };
            println!(
                "long_soak_analysis: rss_growth_kb={} rss_growth_pct={:.1}",
                end_rss as i64 - start_rss as i64,
                growth_pct
            );
        }
        let max_queue_depth = samples.iter().map(|s| s.queue_depth).max().unwrap_or(0);
        println!("long_soak_analysis: max_queue_depth_observed={max_queue_depth}");
        let throughput_drop_pct =
            100.0 * (start_s.ops_per_sec - end_s.ops_per_sec) / start_s.ops_per_sec.max(1.0);
        println!("long_soak_analysis: throughput_drop_pct_start_to_end={throughput_drop_pct:.1}");
        let total_completed_err: u64 = samples.last().map(|s| s.completed_err).unwrap_or(0);
        let total_sync_failures: u64 = samples.last().map(|s| s.sync_failures).unwrap_or(0);
        println!(
            "long_soak_analysis: final completed_err={total_completed_err} \
             sync_failures={total_sync_failures}"
        );
    }

    let report = pool.shutdown();
    println!(
        "long_soak_test: shutdown pool_state={:?} fully_drained={}",
        report.pool_state, report.fully_drained
    );
    let pool = Arc::try_unwrap(pool).unwrap_or_else(|_| panic!("outstanding Arc<Pool> reference"));
    drop(pool.into_inner().unwrap());

    // Thanks to periodic checkpointing above, only the live (unpurged)
    // tail of the WAL should remain by now — safe to fully materialize
    // via the existing recovery API, unlike Phase 3B's raw, unbounded run.
    let recovery_started = Instant::now();
    let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
    let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
    let recovery_ok = replay.corrupted_segments.is_empty();
    println!(
        "long_soak_test: recovery corrupted_segments={} records_recovered={} recovery_ms={:.1} \
         => {}",
        replay.corrupted_segments.len(),
        replay.records.len(),
        recovery_ms,
        if recovery_ok { "OK" } else { "FAILED" }
    );
    // Gap-free *from the first recovered record*, not necessarily from
    // seq=1 — checkpointing deliberately purged everything below the
    // retention watermark, so a gap at the very start (everything before
    // the first surviving segment) is expected, not a corruption signal.
    let mut gap_free = true;
    if let Some((first_seq, _)) = replay.records.first() {
        for (i, (seq, _)) in replay.records.iter().enumerate() {
            if *seq != first_seq + i as u64 {
                gap_free = false;
                println!("long_soak_test: SEQUENCE GAP at index {i}: seq={seq}");
                break;
            }
        }
    }
    println!("long_soak_test: sequences_gap_free_from_first_surviving_record={gap_free}");

    let _ = fs::remove_dir_all(&dir);
    if !recovery_ok || !gap_free {
        std::process::exit(1);
    }
}
