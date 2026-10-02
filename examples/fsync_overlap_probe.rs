//! **Analysis-only tool, not part of RubiXDB.** Does a concurrent *write* (no
//! sync) slow down an in-flight `sync_all`, and does it matter whether that
//! write targets the SAME file or a DIFFERENT file on the same disk? Answers
//! the pipelined-group-commit question for
//! `PHASE_RUBIXDB_WAL_PERFORMANCE_ARCHITECTURE.md` with no WAL code.
//!
//! Usage: `fsync_overlap_probe <seconds> <dir> <mode>` where mode is
//! `none` (flusher only), `same` (writer appends to the flusher's file),
//! `other` (writer appends to a second file in the same dir). The flusher
//! loops write(32 KiB)+sync_all; the writer appends 32 KiB every ~0.5 ms.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let secs: u64 = a[1].parse().unwrap();
    let dir = PathBuf::from(&a[2]);
    let mode = a[3].as_str();
    fs::create_dir_all(&dir).unwrap();
    let pa = dir.join("flusher.bin");
    let pb = dir.join("other.bin");
    let stop = Arc::new(AtomicBool::new(false));
    let w_path = if mode == "same" {
        pa.clone()
    } else {
        pb.clone()
    };
    let w_stop = stop.clone();
    let writer = if mode == "none" {
        None
    } else {
        Some(thread::spawn(move || {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&w_path)
                .unwrap();
            let buf = vec![0x5Au8; 32768];
            let mut n = 0u64;
            while !w_stop.load(Ordering::Relaxed) {
                f.write_all(&buf).unwrap();
                n += 1;
                thread::sleep(Duration::from_micros(500));
            }
            n
        }))
    };
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&pa)
        .unwrap();
    let buf = vec![0xA5u8; 32768];
    let mut lat = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        let t = Instant::now();
        f.write_all(&buf).unwrap();
        f.sync_all().unwrap();
        lat.push(t.elapsed().as_nanos());
    }
    stop.store(true, Ordering::Relaxed);
    let wrote = writer.map(|h| h.join().unwrap()).unwrap_or(0);
    let _ = fs::remove_file(&pa);
    let _ = fs::remove_file(&pb);
    lat.sort_unstable();
    let p = |q: f64| lat[((lat.len() as f64 * q) as usize).min(lat.len() - 1)] as f64 / 1e6;
    println!(
        "mode={mode:5} flusher: {} flushes, {:.0}/s, ms p50={:.2} p95={:.2} p99={:.2} max={:.2} | concurrent writer wrote {} x 32KiB",
        lat.len(), lat.len() as f64 / t0.elapsed().as_secs_f64(), p(0.5), p(0.95), p(0.99),
        *lat.last().unwrap() as f64 / 1e6, wrote
    );
}
