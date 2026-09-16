//! Phase 3C recovery-memory scaling analysis (operating brief §8-§9,
//! `PHASE3C_TEST_PLAN.md`). Builds WAL fixtures at progressively larger
//! record counts and measures `FileWal::open_for_recovery`'s own time
//! and memory cost at each size — the direct, controlled experiment
//! `PHASE3B_ADR.md` ADR-P3B-5 named but did not run (that finding came
//! from one real ~85M-record soak run hitting a host memory limit, not
//! a swept measurement).
//!
//! Fixture construction uses `Wal::append` (memory-speed, no `fsync`
//! per call) plus one final `sync()` — this is about measuring
//! *recovery*'s cost, not re-measuring write-path throughput (already
//! covered by `examples/batch_coordinator_load_test.rs`), so building
//! the fixture itself is deliberately not on any durability-sensitive
//! path.
//!
//! Usage: `cargo run --release --example recovery_memory_scaling --
//! [sizes=1000000,5000000,10000000,15000000]`
//!
//! **Deliberately does not exceed the sizes the operating brief itself
//! names** ("test 1M, 5M, 10M, 15M... a larger size only if the machine
//! can do so safely" — and this project's own Phase 3B finding already
//! showed ~85M is unsafe on this host, so no larger size is attempted
//! here without a machine known to have more headroom).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rubixdb::wal::{FileWal, Wal, WalConfig, WalOp};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_recovery_scaling_{tag}_{nanos}"));
    fs::create_dir_all(&path).unwrap();
    path
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
    let bytes: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(bytes / 1024)
}

fn dir_size_bytes(dir: &std::path::Path) -> u64 {
    fs::read_dir(dir.join("wal"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn build_fixture(dir: &std::path::Path, records: u64) -> Duration {
    let started = Instant::now();
    let (mut wal, _) = FileWal::open_for_recovery(dir, WalConfig::default()).unwrap();
    for i in 0..records {
        wal.append(WalOp::Put {
            key: format!("k{i:010}").as_bytes(),
            value: b"recovery-memory-scaling-fixture-value",
        })
        .unwrap();
    }
    wal.sync().unwrap();
    started.elapsed()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let sizes: Vec<u64> = args
        .get(1)
        .map(|s| s.split(',').map(|v| v.parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1_000_000, 5_000_000, 10_000_000, 15_000_000]);

    println!(
        "size,build_secs,rss_before_kb,rss_after_kb,peak_rss_kb,recovery_ms,records_per_sec,\
         bytes_on_disk,bytes_per_sec,corrupted_segments,records_recovered"
    );

    for &size in &sizes {
        let dir = temp_dir(&size.to_string());
        let build_elapsed = build_fixture(&dir, size);
        eprintln!(
            "recovery_memory_scaling: built {size} records in {:.1}s, starting recovery \
             measurement",
            build_elapsed.as_secs_f64()
        );

        let pid = std::process::id();
        let rss_before = sample_rss_kb(pid);

        // A dedicated sampler thread captures a peak RSS reading while
        // the main thread is blocked inside the single, synchronous
        // `open_for_recovery` call — otherwise only before/after
        // snapshots would be visible, missing any transient peak above
        // the final resting value.
        let peak_rss = Arc::new(AtomicU64::new(rss_before.unwrap_or(0)));
        let sampling = Arc::new(AtomicBool::new(true));
        let sampler_handle = {
            let peak_rss = Arc::clone(&peak_rss);
            let sampling = Arc::clone(&sampling);
            thread::spawn(move || {
                while sampling.load(Ordering::Relaxed) {
                    if let Some(rss) = sample_rss_kb(pid) {
                        peak_rss.fetch_max(rss, Ordering::Relaxed);
                    }
                    thread::sleep(Duration::from_millis(300));
                }
            })
        };

        let recovery_started = Instant::now();
        let (_wal, replay) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
        let recovery_elapsed = recovery_started.elapsed();

        sampling.store(false, Ordering::Relaxed);
        let _ = sampler_handle.join();
        let rss_after = sample_rss_kb(pid);
        let peak = peak_rss.load(Ordering::Relaxed);

        let bytes_on_disk = dir_size_bytes(&dir);
        let records_per_sec =
            replay.records.len() as f64 / recovery_elapsed.as_secs_f64().max(0.001);
        let bytes_per_sec = bytes_on_disk as f64 / recovery_elapsed.as_secs_f64().max(0.001);

        println!(
            "{size},{:.1},{},{},{},{:.1},{:.0},{bytes_on_disk},{:.0},{},{}",
            build_elapsed.as_secs_f64(),
            rss_before
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NA".into()),
            rss_after
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NA".into()),
            peak,
            recovery_elapsed.as_secs_f64() * 1000.0,
            records_per_sec,
            bytes_per_sec,
            replay.corrupted_segments.len(),
            replay.records.len(),
        );

        assert_eq!(
            replay.records.len() as u64,
            size,
            "recovery must return exactly the {size} records that were written"
        );
        assert!(replay.corrupted_segments.is_empty());

        drop(replay);
        // Free the disk before the next (potentially larger) size, per
        // this exercise's own "do not risk exhausting the machine's
        // primary filesystem" instruction.
        let _ = fs::remove_dir_all(&dir);
    }
}
