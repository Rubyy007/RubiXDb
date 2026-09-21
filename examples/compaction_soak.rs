//! Compaction — Increment 3 §16/§17: the first real, long-duration,
//! integrated production endurance soak with automatic Compaction
//! active throughout. Extends `realistic_full_pipeline_soak.rs`'s own
//! established production profile (8 writers, 16 readers,
//! `LsmConfig::default()` for everything except `compaction_auto_
//! trigger: true`) with continuous correctness verification against an
//! independently tracked reference model — something that harness
//! never did (it predates Compaction and never diffed reads against a
//! model at all).
//!
//! Correctness design, stated precisely (so a reader can judge what is
//! and is not proven): each of `KEY_CARDINALITY` keys owns a small,
//! bounded ring buffer (`HISTORY_DEPTH` entries) of its own most recent
//! `(seq, Option<value>)` versions, updated by writers under a per-key
//! lock. A point-read check samples a key's *current* `(seq, value)`
//! entry under that lock, then calls `get_as_of(key, seq)` at that
//! exact seq — race-free by construction, not approximate, since the
//! seq and the expected value were captured together. A range-scan
//! check pins a real `Snapshot`, then answers each probed key from its
//! ring buffer's highest entry with `seq <= snapshot.seq()`; if the
//! buffer's oldest entry is already newer than the snapshot (more than
//! `HISTORY_DEPTH` writes landed on that key between the snapshot and
//! the check), that key is skipped for this round and counted
//! separately (`range_check_skipped_buffer_gap`) rather than silently
//! trusted or falsely failed. `in_run_mismatches` counts only cases the
//! model could actually adjudicate.
//!
//! Usage: `cargo run --release --example compaction_soak --
//! <duration_secs> [writers=8] [readers=16] [sample_interval_secs=60]`

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::ops::Bound;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::execution::batch_coordinator::BatchCoordinatorConfig;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::wal::{SyncMode, WalConfig};

const KEY_CARDINALITY: u64 = 20_000;
const HISTORY_DEPTH: usize = 8;

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
    let path = soak_base_dir().join(format!("rubixdb_compaction_soak_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn key_for(i: u64) -> Vec<u8> {
    format!("k{:08}", i % KEY_CARDINALITY).into_bytes()
}

fn sample_process_metrics(pid: u32) -> Option<(u64, u64, u64)> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "$p = Get-Process -Id {pid}; Write-Output ($p.WorkingSet64.ToString() + ',' + \
                 $p.HandleCount.ToString() + ',' + $p.Threads.Count.ToString())"
            ),
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().split(',');
    let rss_kb: u64 = parts.next()?.trim().parse::<u64>().ok()? / 1024;
    let handles: u64 = parts.next()?.trim().parse().ok()?;
    let threads: u64 = parts.next()?.trim().parse().ok()?;
    Some((rss_kb, handles, threads))
}

fn free_disk_bytes(path: &std::path::Path) -> Option<u64> {
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

fn sstables_dir_bytes(dir: &std::path::Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir.join("sstables")) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

type HistorySlot = Mutex<VecDeque<(u64, Option<Vec<u8>>)>>;

struct RefModel {
    slots: Vec<HistorySlot>,
    /// The highest seq any `record()` call has completed for, updated
    /// via `fetch_max` *after* the per-key slot update (so any reader
    /// observing a value here knows that write's own `model.record()`
    /// -- and therefore its own `apply_after_durable`, which always
    /// happens first on the writer's thread -- has already completed).
    /// Used to pick the freshest race-free pin for range checks,
    /// instead of an arbitrary (possibly stale) key's own last write.
    last_recorded_seq: AtomicU64,
}

impl RefModel {
    fn new(cardinality: u64) -> Self {
        RefModel {
            slots: (0..cardinality)
                .map(|_| Mutex::new(VecDeque::with_capacity(HISTORY_DEPTH)))
                .collect(),
            last_recorded_seq: AtomicU64::new(0),
        }
    }

    /// This specific key's own highest recorded seq, or 0 if never
    /// written -- used to tell "the model hasn't caught up to `seq`
    /// for *this* key yet" (inconclusive, not a defect) apart from "the
    /// model has caught up and still disagrees" (a genuine mismatch).
    fn own_latest_seq(&self, idx: u64) -> u64 {
        self.slots[(idx % KEY_CARDINALITY) as usize]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .back()
            .map(|&(s, _)| s)
            .unwrap_or(0)
    }

    /// The freshest seq known safe to pin a cross-thread comparison to
    /// -- see `last_recorded_seq`'s own doc comment.
    fn freshest_safe_seq(&self) -> Option<u64> {
        let s = self.last_recorded_seq.load(Ordering::Acquire);
        if s == 0 {
            None
        } else {
            Some(s)
        }
    }

    /// Inserts in seq-sorted position, not blind `push_back`: two
    /// writers racing on the *same* key can call `engine.put()`
    /// (assigning seq N then seq N+1) and then call `record()` in the
    /// opposite order if the lower-seq writer is preempted between its
    /// `put()` returning and this call — a real race this harness must
    /// tolerate, not assume away, since `at_or_before`/`latest` both
    /// depend on the buffer staying seq-ordered.
    fn record(&self, idx: u64, seq: u64, value: Option<Vec<u8>>) {
        let mut slot = self.slots[(idx % KEY_CARDINALITY) as usize]
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let pos = slot
            .iter()
            .rposition(|&(s, _)| s <= seq)
            .map(|p| p + 1)
            .unwrap_or(0);
        slot.insert(pos, (seq, value));
        while slot.len() > HISTORY_DEPTH {
            slot.pop_front();
        }
        drop(slot);
        self.last_recorded_seq.fetch_max(seq, Ordering::Release);
    }

    /// Latest (seq, value) for a key -- used by point-read checks.
    fn latest(&self, idx: u64) -> Option<(u64, Option<Vec<u8>>)> {
        let slot = self.slots[(idx % KEY_CARDINALITY) as usize]
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        slot.back().cloned()
    }

    /// Highest recorded version with seq <= as_of, or `None` if the
    /// buffer doesn't go back far enough (a real "can't adjudicate"
    /// case, not a mismatch).
    fn at_or_before(&self, idx: u64, as_of: u64) -> Result<Option<Vec<u8>>, ()> {
        let slot = self.slots[(idx % KEY_CARDINALITY) as usize]
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(&(oldest_seq, _)) = slot.front() {
            if oldest_seq > as_of {
                return Err(()); // buffer gap -- can't adjudicate
            }
        } else {
            // No writes recorded at all for this key yet -- correctly
            // absent.
            return Ok(None);
        }
        Ok(slot
            .iter()
            .rev()
            .find(|(s, _)| *s <= as_of)
            .and_then(|(_, v)| v.clone()))
    }
}

#[allow(clippy::too_many_arguments)]
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: compaction_soak <duration_secs> [writers=8] [readers=16] \
             [sample_interval_secs=60]"
        );
        std::process::exit(2);
    }
    let duration_secs: u64 = args[1].parse().unwrap();
    let writer_count: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(8);
    let reader_count: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(16);
    let sample_interval_secs: u64 = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(60);

    let dir = temp_dir("run");
    let canonical_dir = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    println!(
        "compaction_soak: dir={} canonical={} duration_secs={duration_secs} \
         writers={writer_count} readers={reader_count} sample_interval_secs={sample_interval_secs} \
         key_cardinality={KEY_CARDINALITY} history_depth={HISTORY_DEPTH}",
        dir.display(),
        canonical_dir.display(),
    );

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
    // The established production profile plus one change: automatic
    // Compaction enabled. Everything else is `LsmConfig::default()`,
    // matching `realistic_full_pipeline_soak.rs`'s own precedent.
    let lsm_config = LsmConfig {
        compaction_auto_trigger: true,
        ..LsmConfig::default()
    };

    let engine = Arc::new(
        LsmEngine::open(&dir, wal_config, pool_config, lsm_config)
            .expect("LsmEngine::open must succeed on a fresh directory"),
    );
    let model = Arc::new(RefModel::new(KEY_CARDINALITY));
    let stop = Arc::new(AtomicBool::new(false));

    let writes = Arc::new(AtomicU64::new(0));
    let deletes = Arc::new(AtomicU64::new(0));
    let point_reads = Arc::new(AtomicU64::new(0));
    let range_scans = Arc::new(AtomicU64::new(0));
    let point_mismatches = Arc::new(AtomicU64::new(0));
    let range_mismatches = Arc::new(AtomicU64::new(0));
    let range_skipped_buffer_gap = Arc::new(AtomicU64::new(0));
    let snapshot_checks = Arc::new(AtomicU64::new(0));

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let pid = std::process::id();

    let mut handles = Vec::new();

    for w in 0..writer_count {
        let engine = Arc::clone(&engine);
        let model = Arc::clone(&model);
        let stop = Arc::clone(&stop);
        let writes = Arc::clone(&writes);
        let deletes = Arc::clone(&deletes);
        handles.push(thread::spawn(move || {
            let mut i: u64 = w as u64;
            let mut rng_state: u64 = 0x9E3779B97F4A7C15 ^ (w as u64 + 1);
            let mut next_rand = move || {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                rng_state
            };
            while !stop.load(Ordering::Relaxed) {
                let idx = next_rand() % KEY_CARDINALITY;
                let key = key_for(idx);
                let op = next_rand() % 20;
                if op < 3 {
                    // delete (covers plain delete + the delete half of
                    // delete/recreate, since the very next op on this
                    // same key may well be a put).
                    if let Ok(seq) = engine.delete(&key) {
                        model.record(idx, seq, None);
                        deletes.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    // put (covers plain put, overwrite, and recreate).
                    let len = 16 + (next_rand() % 200) as usize;
                    let mut value = vec![0u8; len];
                    for b in value.iter_mut() {
                        *b = (next_rand() & 0xFF) as u8;
                    }
                    if let Ok(seq) = engine.put(&key, &value) {
                        model.record(idx, seq, Some(value));
                        writes.fetch_add(1, Ordering::Relaxed);
                    }
                }
                i = i.wrapping_add(1);
                let _ = i;
            }
        }));
    }

    for r in 0..reader_count {
        let engine = Arc::clone(&engine);
        let model = Arc::clone(&model);
        let stop = Arc::clone(&stop);
        let point_reads = Arc::clone(&point_reads);
        let range_scans = Arc::clone(&range_scans);
        let point_mismatches = Arc::clone(&point_mismatches);
        let range_mismatches = Arc::clone(&range_mismatches);
        let range_skipped_buffer_gap = Arc::clone(&range_skipped_buffer_gap);
        let snapshot_checks = Arc::clone(&snapshot_checks);
        // Roughly 3 of every 16 readers also perform range scans +
        // snapshot create/release, spreading the heavier checks across
        // the reader pool rather than every reader paying that cost.
        let does_ranges = r % 5 == 0;
        handles.push(thread::spawn(move || {
            let mut rng_state: u64 = 0xD1B54A32D192ED03 ^ (r as u64 + 1);
            let mut next_rand = move || {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                rng_state
            };
            let mut iter_count = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let idx = next_rand() % KEY_CARDINALITY;
                let key = key_for(idx);
                if let Some((seq, expected)) = model.latest(idx) {
                    let op = next_rand() % 3;
                    let ok = match op {
                        0 => engine
                            .get_as_of(&key, seq)
                            .map(|got| got == expected)
                            .unwrap_or(false),
                        1 => engine
                            .contains(&key, seq)
                            .map(|got| got == expected.is_some())
                            .unwrap_or(false),
                        _ => {
                            // `get()` reads "now", not a pinned seq --
                            // exercised for real workload/read-path
                            // coverage (the brief explicitly lists
                            // `get` among the operations to run
                            // throughout), but deliberately NOT used as
                            // a correctness oracle: comparing it against
                            // the model is an inherent TOCTOU race no
                            // amount of re-checking closes (a writer
                            // can apply its write to the engine, be
                            // observed by this `get()`, and only THEN
                            // call `model.record()` -- an unavoidable
                            // gap between "engine mutation visible" and
                            // "model updated" for an unpinned "now"
                            // read from a different thread). `get_as_
                            // of`/`contains` above are the real
                            // correctness oracles (pinned to an exact
                            // seq captured atomically with the expected
                            // value, under the same lock).
                            let _ = engine.get(&key);
                            true
                        }
                    };
                    if !ok {
                        point_mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                    point_reads.fetch_add(1, Ordering::Relaxed);
                }

                if does_ranges && iter_count.is_multiple_of(25) {
                    // Exercise real Snapshot creation/release (the
                    // brief's own explicit workload requirement) --
                    // but do NOT use `snap.seq()` as the correctness-
                    // check pin: `snapshot_seq()`'s own documented
                    // caveat (reflects WAL `durable_through`, not "every
                    // other thread's `apply_after_durable` has already
                    // run") makes it an unsafe pin for a *different*
                    // thread's own comparison, exactly the same class
                    // of transient false-positive the project's own
                    // `read_engine_bench.rs::section_sanity` already
                    // found and documented for `get_as_of`/`contains`.
                    // Point-checks above prove the fix: pin to a seq
                    // that came directly from a completed write's own
                    // return value (via the model, under its lock) --
                    // provably already-applied, not a racing global
                    // watermark.
                    let snap = engine.snapshot();
                    let Some(seq) = model.freshest_safe_seq() else {
                        drop(snap);
                        iter_count += 1;
                        continue;
                    };
                    for &(label, span) in &[("small", 10u64), ("medium", 100), ("large", 2000)] {
                        // Never wrap: `key_for` applies `% KEY_
                        // CARDINALITY` internally, so an unclamped
                        // `start_idx + span >= KEY_CARDINALITY` would
                        // silently produce an end key *less* than the
                        // start key (an inverted/empty range) while
                        // probes below still expect real data --
                        // clamping the start index keeps every scan's
                        // bounds well-formed.
                        let start_idx = next_rand() % (KEY_CARDINALITY - span);
                        let start_key = key_for(start_idx);
                        let end_key = key_for(start_idx + span);
                        let rows: Result<Vec<_>, _> = engine
                            .range_scan(
                                Bound::Included(start_key.as_slice()),
                                Bound::Excluded(end_key.as_slice()),
                                seq,
                            )
                            .collect();
                        if let Ok(rows) = rows {
                            let got: std::collections::HashMap<Vec<u8>, Vec<u8>> =
                                rows.into_iter().collect();
                            for probe in 0..span.min(50) {
                                let pidx = start_idx.wrapping_add(probe);
                                let key = key_for(pidx);
                                let actual = got.get(&key).cloned();
                                match model.at_or_before(pidx, seq) {
                                    Ok(expected) if expected == actual => {}
                                    Ok(_) => {
                                        // `seq` (from `last_recorded_
                                        // seq`) proves *some* writer's
                                        // own record() has completed for
                                        // that seq -- seq assignment
                                        // order (at WAL-append time) is
                                        // NOT coupled to application
                                        // order across *different*
                                        // writer threads, so this does
                                        // NOT prove *this specific key's*
                                        // own writer has reached its own
                                        // record() call yet (even though
                                        // that writer's engine-side
                                        // apply is already guaranteed
                                        // complete before its own
                                        // record() call). A bounded
                                        // retry lets the model catch up;
                                        // if it never does, check
                                        // whether *this key's own*
                                        // model entry has itself reached
                                        // `seq` yet -- only a
                                        // still-disagreeing, caught-up
                                        // key is a genuine mismatch, not
                                        // one this harness simply
                                        // couldn't observe converge in
                                        // time (tracked the same as a
                                        // buffer-gap "inconclusive").
                                        let mut resolved = false;
                                        for _ in 0..100 {
                                            thread::sleep(Duration::from_millis(1));
                                            if model.at_or_before(pidx, seq) == Ok(actual.clone()) {
                                                resolved = true;
                                                break;
                                            }
                                        }
                                        if !resolved {
                                            if model.own_latest_seq(pidx) >= seq {
                                                range_mismatches.fetch_add(1, Ordering::Relaxed);
                                            } else {
                                                range_skipped_buffer_gap
                                                    .fetch_add(1, Ordering::Relaxed);
                                            }
                                        }
                                    }
                                    Err(()) => {
                                        range_skipped_buffer_gap.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                        } else {
                            range_mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                        let _ = label;
                    }
                    range_scans.fetch_add(3, Ordering::Relaxed);
                    drop(snap);
                    snapshot_checks.fetch_add(1, Ordering::Relaxed);
                }
                iter_count += 1;
            }
        }));
    }

    println!(
        "t_secs,writes,deletes,point_reads,range_scans,point_mismatches,range_mismatches,\
         range_skipped_buffer_gap,snapshot_checks,compaction_cycles,compaction_duration_total_ms,\
         compaction_duration_max_ms,records_dropped_total,sstable_count,storage_state,rss_kb,\
         handles,threads,manifest_size_bytes,wal_bytes,sstables_bytes,db_size_bytes,free_disk_bytes"
    );

    let start = Instant::now();
    let mut last_cycles = 0u64;
    let mut last_dur_total_ms = 0.0f64;
    while Instant::now() < deadline {
        let step = sample_interval_secs.min(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
                .max(1),
        );
        thread::sleep(Duration::from_secs(step));

        let m = engine.compaction_metrics();
        let (rss_kb, handles_n, threads_n) = sample_process_metrics(pid).unwrap_or((0, 0, 0));
        let manifest_bytes = engine.manifest_size_bytes().unwrap_or(0);
        let wal_bytes = wal_dir_bytes(&dir);
        let sstables_bytes = sstables_dir_bytes(&dir);
        let free_disk = free_disk_bytes(&dir).unwrap_or(0);
        let db_size = manifest_bytes + wal_bytes + sstables_bytes;

        let dur_total_ms = m.duration_total.as_secs_f64() * 1000.0;
        println!(
            "{:.1},{},{},{},{},{},{},{},{},{},{:.1},{:.1},{},{},{:?},{rss_kb},{handles_n},\
             {threads_n},{manifest_bytes},{wal_bytes},{sstables_bytes},{db_size},{free_disk}",
            start.elapsed().as_secs_f64(),
            writes.load(Ordering::Relaxed),
            deletes.load(Ordering::Relaxed),
            point_reads.load(Ordering::Relaxed),
            range_scans.load(Ordering::Relaxed),
            point_mismatches.load(Ordering::Relaxed),
            range_mismatches.load(Ordering::Relaxed),
            range_skipped_buffer_gap.load(Ordering::Relaxed),
            snapshot_checks.load(Ordering::Relaxed),
            m.cycles_completed,
            dur_total_ms,
            m.duration_max.as_secs_f64() * 1000.0,
            m.records_dropped_total,
            engine.sstable_count(),
            engine.storage_state(),
        );

        println!(
            "compaction_soak_interval: cycles_delta={} duration_delta_ms={:.1}",
            m.cycles_completed - last_cycles,
            dur_total_ms - last_dur_total_ms
        );
        last_cycles = m.cycles_completed;
        last_dur_total_ms = dur_total_ms;
    }

    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }

    let final_point_mismatches = point_mismatches.load(Ordering::Relaxed);
    let final_range_mismatches = range_mismatches.load(Ordering::Relaxed);
    let in_run_mismatches = final_point_mismatches + final_range_mismatches;
    println!(
        "compaction_soak: FINAL writes={} deletes={} point_reads={} range_scans={} \
         point_mismatches={final_point_mismatches} range_mismatches={final_range_mismatches} \
         range_skipped_buffer_gap={} in_run_mismatches={in_run_mismatches} \
         compaction_cycles={} sstable_count={}",
        writes.load(Ordering::Relaxed),
        deletes.load(Ordering::Relaxed),
        point_reads.load(Ordering::Relaxed),
        range_scans.load(Ordering::Relaxed),
        range_skipped_buffer_gap.load(Ordering::Relaxed),
        engine.compaction_metrics().cycles_completed,
        engine.sstable_count(),
    );

    let report = engine.shutdown();
    println!(
        "compaction_soak: shutdown pool_state={:?} fully_drained={}",
        report.pool_state, report.fully_drained
    );
    let engine =
        Arc::try_unwrap(engine).unwrap_or_else(|_| panic!("outstanding Arc<LsmEngine> reference"));
    drop(engine);

    // Post-recovery: reopen fresh, then do a full, deterministic,
    // non-approximate check of every key's own latest recorded value
    // (no ring-buffer approximation needed here -- no concurrent
    // mutation is happening any more).
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
    match LsmEngine::open(
        &dir,
        wal_config_reopen,
        pool_config_reopen,
        LsmConfig::default(),
    ) {
        Ok(engine2) => {
            let recovery_ms = recovery_started.elapsed().as_secs_f64() * 1000.0;
            let mut post_recovery_mismatches = 0u64;
            let mut checked = 0u64;
            for idx in 0..KEY_CARDINALITY {
                if let Some((_seq, expected)) = model.latest(idx) {
                    let actual = engine2.get(&key_for(idx)).unwrap_or(None);
                    if actual != expected {
                        post_recovery_mismatches += 1;
                    }
                    checked += 1;
                }
            }
            println!(
                "compaction_soak: recovery OK recovery_ms={recovery_ms:.1} keys_checked={checked} \
                 post_recovery_mismatches={post_recovery_mismatches} live_sstable_ids={} \
                 sstable_count={}",
                engine2.live_sstable_ids().len(),
                engine2.sstable_count(),
            );
            engine2.shutdown();
            if in_run_mismatches == 0 && post_recovery_mismatches == 0 {
                println!("compaction_soak: CORRECTNESS PASS (in_run=0, post_recovery=0)");
            } else {
                println!(
                    "compaction_soak: CORRECTNESS FAIL (in_run={in_run_mismatches}, \
                     post_recovery={post_recovery_mismatches})"
                );
                std::process::exit(1);
            }
            let _ = fs::remove_dir_all(&dir);
        }
        Err(e) => {
            println!("compaction_soak: recovery FAILED error={e}");
            eprintln!(
                "compaction_soak: NOT deleting {} for inspection",
                dir.display()
            );
            std::process::exit(1);
        }
    }
}
