//! Realistic, multi-hour, full-pipeline soak — write-engine
//! certification requirement not previously covered by any phase.
//!
//! `examples/long_soak_test.rs` (the completed Phase 3C soak) exercises
//! WAL + Group Commit + Dedicated Batch Coordinator only; it never
//! constructs an `LsmEngine` and never touches MemTable, freeze,
//! SSTable flush, or Manifest checkpoint/purge. `examples/
//! manifest_soak_test.rs` exercises the full pipeline but is explicitly
//! bounded (minutes, not hours) and uses a deliberately tiny
//! `memtable_max_size_bytes: 2048` stress config, not a realistic
//! production profile. Neither satisfies "sustained multi-hour
//! operation of the real production write path."
//!
//! This harness closes that gap: a single long-running `LsmEngine`
//! (real `PUT`/`DELETE`, `LsmConfig::default()` — the same 4 MiB
//! memtable, default block size, default bloom bits, default
//! `max_flush_retries` used everywhere else in this project as "the
//! realistic profile"), driven by many writer threads for
//! `duration_secs`, with freeze/flush/checkpoint/purge happening
//! entirely through the engine's own unmodified background flush
//! thread — no manual checkpoint-thread workaround the way
//! `long_soak_test.rs` needs for the WAL-only layer, since
//! `LsmEngine`'s flush thread already checkpoints and purges on its
//! own.
//!
//! Every `WRITER_DELETE_EVERY`th write issues a `delete` of an earlier
//! key from the same thread instead of a `put`, so tombstones flow
//! through WAL -> MemTable -> SSTable -> Manifest exactly like a real
//! workload, not a put-only proxy.
//!
//! Crash-safety at this exact realistic config is intentionally out of
//! scope for *this* harness (real crash cycles at the full-pipeline
//! layer are separately covered by `sstable_flush_crash_test.rs`/
//! `lsm_crash_cycle_test.rs`, 205/205 clean in this certification) —
//! this harness's own job is sustained-duration stability and bounded
//! resource behavior specifically, not re-proving crash safety.
//!
//! Usage: `cargo run --release --example realistic_full_pipeline_soak --
//! <writer_count> <duration_secs> [sample_interval_secs=120]`

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

const SAMPLE_EVERY: u64 = 50;
const WRITER_DELETE_EVERY: u64 = 17;

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
    let path = std::env::temp_dir().join(format!("rubixdb_realistic_soak_{tag}_{nanos}"));
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
    rss_kb: Option<u64>,
    cpu_pct: Option<f64>,
    submitted: u64,
    completed_ok: u64,
    completed_err: u64,
    rejected_backpressure: u64,
    highest_sequence: u64,
    durable_through: u64,
    checkpoint_seq: u64,
    sstable_count: usize,
    immutable_count: usize,
    capacity_pressure_events: u64,
    manifest_size_bytes: u64,
    wal_bytes: u64,
}

fn wal_dir_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir.join("wal")) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("wal-") && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: realistic_full_pipeline_soak <writer_count> <duration_secs> \
             [sample_interval_secs=120]"
        );
        std::process::exit(2);
    }
    let writer_count: usize = args[1].parse().expect("writer_count must be a usize");
    let duration_secs: u64 = args[2].parse().expect("duration_secs must be a u64");
    let sample_interval_secs: u64 = args
        .get(3)
        .map(|s| s.parse().expect("sample_interval_secs must be a u64"))
        .unwrap_or(120);

    let dir = temp_dir("run");
    println!("realistic_full_pipeline_soak: dir={}", dir.display());

    let wal_config = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let pool_config = BatchCoordinatorConfig {
        queue_capacity: (writer_count * 4).max(64),
        max_queued_bytes: 64 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(30),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: (writer_count * 4).max(65536),
    };
    // The realistic production profile: LsmConfig::default() is exactly
    // what `examples/lsm_flush_load_test.rs` calls "the LSM spec
    // default" (4 MiB memtable) -- not the tiny 2048-byte stress config
    // `manifest_soak_test.rs`/`sstable_flush_crash_child.rs` use to force
    // rapid freeze/flush cycling for crash-timing coverage.
    let lsm_config = LsmConfig::default();

    let engine = Arc::new(
        LsmEngine::open(&dir, wal_config, pool_config, lsm_config)
            .expect("LsmEngine::open must succeed on a fresh directory"),
    );
    let latencies = Arc::new(LatencySlots::new(writer_count));
    let stop = Arc::new(AtomicBool::new(false));
    let total_ops = Arc::new(AtomicU64::new(0));
    let total_deletes = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let pid = std::process::id();

    let writer_handles: Vec<_> = (0..writer_count)
        .map(|t| {
            let engine = Arc::clone(&engine);
            let latencies = Arc::clone(&latencies);
            let stop = Arc::clone(&stop);
            let total_ops = Arc::clone(&total_ops);
            let total_deletes = Arc::clone(&total_deletes);
            thread::spawn(move || {
                let mut i: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    let op_started = Instant::now();
                    let result = if i > 0 && i.is_multiple_of(WRITER_DELETE_EVERY) {
                        let key = format!("t{t}-{}", i - 1);
                        let r = engine.delete(key.as_bytes());
                        if r.is_ok() {
                            total_deletes.fetch_add(1, Ordering::Relaxed);
                        }
                        r
                    } else {
                        let key = format!("t{t}-{i}");
                        engine.put(key.as_bytes(), b"v")
                    };
                    if result.is_ok() {
                        total_ops.fetch_add(1, Ordering::Relaxed);
                        if i.is_multiple_of(SAMPLE_EVERY) {
                            let ns =
                                u64::try_from(op_started.elapsed().as_nanos()).unwrap_or(u64::MAX);
                            latencies.record(t, ns);
                        }
                    }
                    i += 1;
                }
            })
        })
        .collect();

    println!(
        "realistic_full_pipeline_soak: writer_count={writer_count} duration_secs={duration_secs} \
         sample_interval_secs={sample_interval_secs} lsm_config={{memtable_max_size_bytes={}, \
         max_immutable_memtables={}, sstable_target_block_size={}, max_flush_retries={}}} \
         SAMPLE_EVERY={SAMPLE_EVERY} WRITER_DELETE_EVERY={WRITER_DELETE_EVERY}",
        LsmConfig::default().memtable_max_size_bytes,
        LsmConfig::default().max_immutable_memtables,
        LsmConfig::default().sstable_target_block_size,
        LsmConfig::default().max_flush_retries,
    );
    println!(
        "t_secs,ops_per_sec,p50_ms,p95_ms,p99_ms,max_ms,queue_depth,queue_capacity,queued_bytes,\
         sync_attempts,sync_failures,rss_kb,cpu_pct,submitted,completed_ok,completed_err,\
         rejected_backpressure,highest_sequence,durable_through,checkpoint_seq,sstable_count,\
         immutable_count,capacity_pressure_events,manifest_size_bytes,wal_bytes"
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

        let stats = engine.pool_stats();
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
            rss_kb,
            cpu_pct,
            submitted: stats.submitted,
            completed_ok: stats.completed_ok,
            completed_err: stats.completed_err,
            rejected_backpressure: stats.rejected_backpressure,
            highest_sequence: stats.committer_stats.highest_sequence,
            durable_through: stats.committer_stats.durable_through,
            checkpoint_seq: engine.checkpoint_seq(),
            sstable_count: engine.sstable_count(),
            immutable_count: engine.immutable_count(),
            capacity_pressure_events: engine.capacity_pressure_events(),
            manifest_size_bytes: engine.manifest_size_bytes().unwrap_or(0),
            wal_bytes: wal_dir_bytes(&dir),
        };
        println!(
            "{:.1},{:.0},{:.3},{:.3},{:.3},{:.3},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
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
            sample.highest_sequence,
            sample.durable_through,
            sample.checkpoint_seq,
            sample.sstable_count,
            sample.immutable_count,
            sample.capacity_pressure_events,
            sample.manifest_size_bytes,
            sample.wal_bytes,
        );
        samples.push(sample);
    }

    stop.store(true, Ordering::Relaxed);
    for h in writer_handles {
        let _ = h.join();
    }

    println!(
        "realistic_full_pipeline_soak: total_ops_completed_via_local_counter={} \
         total_deletes_via_local_counter={}",
        total_ops.load(Ordering::Relaxed),
        total_deletes.load(Ordering::Relaxed)
    );

    if samples.len() >= 2 {
        let start_s = &samples[0];
        let end_s = &samples[samples.len() - 1];
        let mid_s = &samples[samples.len() / 2];
        for (label, s) in [("start", start_s), ("mid", mid_s), ("end", end_s)] {
            println!(
                "realistic_soak_analysis: {label:<5} ops/sec={:.0} rss_kb={:?} cpu_pct={:?} \
                 p99_ms={:.3} queue_depth={} durable_through={} checkpoint_seq={} \
                 sstable_count={} manifest_size_bytes={} wal_bytes={}",
                s.ops_per_sec,
                s.rss_kb,
                s.cpu_pct,
                s.p99_ms,
                s.queue_depth,
                s.durable_through,
                s.checkpoint_seq,
                s.sstable_count,
                s.manifest_size_bytes,
                s.wal_bytes
            );
        }
        if let (Some(start_rss), Some(end_rss)) = (start_s.rss_kb, end_s.rss_kb) {
            let growth_pct = if start_rss > 0 {
                100.0 * (end_rss as f64 - start_rss as f64) / start_rss as f64
            } else {
                0.0
            };
            println!(
                "realistic_soak_analysis: rss_growth_kb={} rss_growth_pct={:.1}",
                end_rss as i64 - start_rss as i64,
                growth_pct
            );
        }
        let max_queue_depth = samples.iter().map(|s| s.queue_depth).max().unwrap_or(0);
        let max_wal_bytes = samples.iter().map(|s| s.wal_bytes).max().unwrap_or(0);
        let max_immutable = samples.iter().map(|s| s.immutable_count).max().unwrap_or(0);
        println!(
            "realistic_soak_analysis: max_queue_depth_observed={max_queue_depth} \
             max_wal_bytes_observed={max_wal_bytes} max_immutable_count_observed={max_immutable}"
        );
        let throughput_drop_pct =
            100.0 * (start_s.ops_per_sec - end_s.ops_per_sec) / start_s.ops_per_sec.max(1.0);
        println!(
            "realistic_soak_analysis: throughput_drop_pct_start_to_end={throughput_drop_pct:.1}"
        );
        let final_s = samples.last().unwrap();
        println!(
            "realistic_soak_analysis: final completed_err={} sync_failures={} \
             capacity_pressure_events={} sstable_count={} checkpoint_seq={} highest_sequence={}",
            final_s.completed_err,
            final_s.sync_failures,
            final_s.capacity_pressure_events,
            final_s.sstable_count,
            final_s.checkpoint_seq,
            final_s.highest_sequence
        );
    }

    let recovery_stats_before_shutdown = engine.recovery_stats();
    let report = engine.shutdown();
    println!(
        "realistic_full_pipeline_soak: shutdown pool_state={:?} fully_drained={}",
        report.pool_state, report.fully_drained
    );
    println!(
        "realistic_full_pipeline_soak: open-time recovery_stats={recovery_stats_before_shutdown:?}"
    );
    // Drop the engine fully (releasing the WAL's exclusive lock) before
    // reopening below -- `shutdown()` takes `&self` and does not itself
    // consume/drop the engine, matching `long_soak_test.rs`'s identical
    // `Arc::try_unwrap` + drop pattern for the same reason.
    let engine = Arc::try_unwrap(engine).unwrap_or_else(|_| panic!("outstanding Arc<LsmEngine> reference"));
    drop(engine);

    // Recovery check: reopen fresh against the same directory. This
    // proves (a) the Manifest + bounded-streaming WAL replay path
    // reconstructs cleanly after a real multi-hour run, and (b) the live
    // SSTable set the Manifest reports is actually valid -- both fail
    // closed (`LsmEngine::open` returns `Err`) on any corruption,
    // missing live SSTable, or invalid checkpoint per
    // `PHASE5_FAILURE_MODEL.md`.
    let wal_config_reopen = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let pool_config_reopen = BatchCoordinatorConfig {
        queue_capacity: 64,
        max_queued_bytes: 16 * 1024 * 1024,
        submission_timeout: Duration::from_secs(2),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    };
    let recovery_started = Instant::now();
    let reopened = LsmEngine::open(
        &dir,
        wal_config_reopen,
        pool_config_reopen,
        LsmConfig::default(),
    );
    let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
    match reopened {
        Ok(engine2) => {
            let rstats = engine2.recovery_stats();
            println!(
                "realistic_full_pipeline_soak: recovery OK recovery_ms={recovery_ms:.1} \
                 stats={rstats:?} live_sstable_ids_count={} manifest_record_count={}",
                engine2.live_sstable_ids().len(),
                engine2.manifest_record_count()
            );
            engine2.shutdown();
            let _ = fs::remove_dir_all(&dir);
        }
        Err(e) => {
            println!(
                "realistic_full_pipeline_soak: recovery FAILED recovery_ms={recovery_ms:.1} \
                 error={e}"
            );
            eprintln!("realistic_full_pipeline_soak: NOT deleting {} for inspection", dir.display());
            std::process::exit(1);
        }
    }
}
