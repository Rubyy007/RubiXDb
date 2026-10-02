//! **Analysis-only tool, not part of RubiXDB.** Answers one hardware question
//! for `PHASE_RUBIXDB_WAL_PERFORMANCE_ARCHITECTURE.md`: *can N independent
//! durable-flush lanes (separate files, each `write` + `sync_all`) run in
//! parallel on this storage, or does the device/filesystem serialize them?*
//! No WAL code, no dependency on `src/`; uses only `std::fs`.
//!
//! Usage: `fsync_lanes_probe <seconds> <write_bytes> <dirA>[,<dirB>...] <lanes_per_dir>`
//! Each lane thread owns one file in one dir and loops write(write_bytes) +
//! sync_all, recording every flush latency. Prints aggregate flushes/s and
//! per-flush p50/p95/p99/max. Every file is removed before exit.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn pct(v: &[u128], p: f64) -> f64 {
    v[(((v.len() as f64) * p) as usize).min(v.len() - 1)] as f64 / 1e6
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let secs: u64 = a[1].parse().unwrap();
    let bytes: usize = a[2].parse().unwrap();
    let dirs: Vec<PathBuf> = a[3].split(',').map(PathBuf::from).collect();
    let per_dir: usize = a[4].parse().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    let mut files = Vec::new();
    for (di, d) in dirs.iter().enumerate() {
        fs::create_dir_all(d).unwrap();
        for l in 0..per_dir {
            let path = d.join(format!("lane_{di}_{l}.bin"));
            files.push(path.clone());
            let stop = stop.clone();
            handles.push(thread::spawn(move || {
                let mut f = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&path)
                    .unwrap();
                let buf = vec![0xA5u8; bytes];
                let mut lat = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    f.write_all(&buf).unwrap();
                    f.sync_all().unwrap();
                    lat.push(t.elapsed().as_nanos());
                }
                lat
            }));
        }
    }
    let start = Instant::now();
    thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    let wall = start.elapsed().as_secs_f64();
    for f in files {
        let _ = fs::remove_file(f);
    }
    all.sort_unstable();
    println!(
        "lanes={} ({} dir(s) x {}) bytes={} => {:.0} flushes/s aggregate | per-flush ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        dirs.len() * per_dir,
        dirs.len(),
        per_dir,
        bytes,
        all.len() as f64 / wall,
        pct(&all, 0.5),
        pct(&all, 0.95),
        pct(&all, 0.99),
        *all.last().unwrap() as f64 / 1e6
    );
}
