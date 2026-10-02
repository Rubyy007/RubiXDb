//! **Analysis/certification tool, not part of RubiXDB.** Sustained WAL-heavy
//! workload for the long-duration check (Rule 36 of the WAL optimization
//! mandate): `writers` threads loop `append` + `await_durable` through the real
//! `GroupCommitter` for `seconds`, and every `interval` seconds one line is
//! printed with that interval's ops/s and commit-latency p50/p95/p99/max plus
//! the WAL directory size and live segment count. Process RSS / threads /
//! handles are sampled externally (see `scripts/wal_soak_monitor.ps1`).
//!
//! Usage: `wal_soak <writers> <seconds> <interval_secs>`; honours TMP/TEMP.
//! The WAL uses the default segment size so rotation, sealing and (via
//! `purge_before` driven by the soak's rolling checkpoint) purging all occur.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rubixdb::wal::{FileWal, GroupCommitter, SyncMode, Wal, WalConfig, WalOp};
use rubixdb::EngineError;

fn dir_stats(dir: &Path) -> (u64, usize) {
    let mut bytes = 0;
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_file() && e.file_name().to_string_lossy().starts_with("wal-") {
                    bytes += m.len();
                    n += 1;
                }
            }
        }
    }
    (bytes, n)
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let writers: usize = a[1].parse().unwrap();
    let seconds: u64 = a[2].parse().unwrap();
    let interval: u64 = a[3].parse().unwrap();
    let dir = std::env::temp_dir().join(format!("wal_soak_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = WalConfig {
        // small segments so a soak exercises many rotations and purges
        max_segment_size: 8 * 1024 * 1024,
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let (wal, _) = FileWal::open_for_recovery(&dir, cfg).unwrap();
    let committer = Arc::new(GroupCommitter::new(wal).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let lat: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let mut hs = Vec::new();
    for t in 0..writers {
        let c = Arc::clone(&committer);
        let stop = Arc::clone(&stop);
        let lat = Arc::clone(&lat);
        hs.push(std::thread::spawn(move || {
            let mut i = 0u64;
            let mut local: Vec<u64> = Vec::with_capacity(4096);
            let value = vec![b'v'; 100];
            while !stop.load(Ordering::Relaxed) {
                let key = format!("t{t}-{i}");
                let t0 = Instant::now();
                let pos = c
                    .append(WalOp::Put {
                        key: key.as_bytes(),
                        value: &value,
                    })
                    .expect("append");
                loop {
                    match c.await_durable(pos.seq) {
                        Ok(()) => break,
                        Err(EngineError::Timeout { .. }) => continue,
                        Err(e) => panic!("await: {e}"),
                    }
                }
                local.push(t0.elapsed().as_micros() as u64);
                i += 1;
                if local.len() >= 2048 {
                    lat.lock().unwrap().append(&mut local);
                }
            }
            lat.lock().unwrap().append(&mut local);
        }));
    }
    let start = Instant::now();
    let mut last_tick = Instant::now();
    let mut next_checkpoint_seq = 0u64;
    println!("t_s,ops_s,p50_ms,p95_ms,p99_ms,max_ms,wal_mb,segments,highest_seq,durable_through");
    while start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_secs(interval));
        let el = last_tick.elapsed().as_secs_f64();
        last_tick = Instant::now();
        let mut v = std::mem::take(&mut *lat.lock().unwrap());
        v.sort_unstable();
        let p = |q: f64| {
            if v.is_empty() {
                0.0
            } else {
                v[((v.len() as f64 * q) as usize).min(v.len() - 1)] as f64 / 1000.0
            }
        };
        let s = committer.stats();
        // Roll a checkpoint forward so old segments are purged, as the engine does.
        if s.durable_through > next_checkpoint_seq + 200_000 {
            next_checkpoint_seq = s.durable_through - 100_000;
            let _ = committer.purge_before(next_checkpoint_seq);
        }
        let (bytes, segs) = dir_stats(&dir);
        println!(
            "{:.0},{:.0},{:.2},{:.2},{:.2},{:.1},{:.1},{},{},{}",
            start.elapsed().as_secs_f64(),
            v.len() as f64 / el,
            p(0.5),
            p(0.95),
            p(0.99),
            v.last().copied().unwrap_or(0) as f64 / 1000.0,
            bytes as f64 / 1e6,
            segs,
            s.highest_sequence,
            s.durable_through
        );
    }
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().unwrap();
    }
    let s = committer.stats();
    println!(
        "FINAL highest_seq={} durable_through={} sync_successes={} records_total={}",
        s.highest_sequence, s.durable_through, s.sync_successes, s.records_total
    );
    drop(committer);
    let _ = std::fs::remove_dir_all(&dir);
}
