//! **Analysis-only tool, not part of RubiXDB.** The exact M1.2/M1.3 workload
//! (N threads x K records of `Put{t<t>-<i>, "v"}` through `GroupCommitter::
//! append_durable`, the test's `group_commit_config`) with per-commit latency
//! recording, so tail latency can be compared between two builds over a
//! LONG run (the load-test harness's 1,000-writer level lasts only ~0.4 s,
//! which a single disk stall dominates).
//!
//! Usage: `wal_commit_latency <writers> <records_per_writer>`; honours TMP/TEMP.
use std::sync::Arc;
use std::time::{Duration, Instant};

use rubixdb::wal::{FileWal, GroupCommitter, SyncMode, Wal, WalConfig, WalOp};
use rubixdb::EngineError;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let threads: usize = a[1].parse().unwrap();
    let per: usize = a[2].parse().unwrap();
    let dir = std::env::temp_dir().join(format!("wal_commit_latency_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    };
    let (wal, _) = FileWal::open_for_recovery(&dir, cfg).unwrap();
    let c = Arc::new(GroupCommitter::new(wal).unwrap());
    let start = Instant::now();
    let hs: Vec<_> = (0..threads)
        .map(|t| {
            let c = Arc::clone(&c);
            std::thread::spawn(move || {
                let mut lat = Vec::with_capacity(per);
                for i in 0..per {
                    let key = format!("t{t}-{i}");
                    let t0 = Instant::now();
                    let pos = c
                        .append(WalOp::Put {
                            key: key.as_bytes(),
                            value: b"v",
                        })
                        .unwrap_or_else(|e| panic!("append: {e}"));
                    loop {
                        match c.await_durable(pos.seq) {
                            Ok(()) => break,
                            Err(EngineError::Timeout { .. }) => continue,
                            Err(e) => panic!("await: {e}"),
                        }
                    }
                    lat.push(t0.elapsed().as_micros() as u64);
                }
                lat
            })
        })
        .collect();
    let mut all: Vec<u64> = Vec::new();
    for h in hs {
        all.extend(h.join().unwrap());
    }
    let el = start.elapsed().as_secs_f64();
    all.sort_unstable();
    let p = |q: f64| all[((all.len() as f64 * q) as usize).min(all.len() - 1)] as f64 / 1000.0;
    let s = c.stats();
    println!(
        "writers={threads} records={} {:.0} ops/s | commit ms p50={:.2} p95={:.2} p99={:.2} p99.9={:.2} max={:.1} | syncs={} rec/sync={:.0}",
        all.len(), all.len() as f64 / el, p(0.5), p(0.95), p(0.99), p(0.999), *all.last().unwrap() as f64 / 1000.0,
        s.sync_successes, s.records_total as f64 / s.sync_successes.max(1) as f64
    );
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}
