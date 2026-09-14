//! Phase 1 diagnostic, kept as a permanent, reproducible artifact (not a
//! one-off scratch script): measures raw `Mutex<FileWal>::append()`
//! throughput under 100-way contention, with **no `fsync`/batching at
//! all**, to isolate whether the append path/lock is itself a throughput
//! ceiling, independent of `GroupCommitter`'s batching window. See
//! `PHASE1_TEST_RESULTS.md`'s performance section for how this number is
//! used in the root-cause analysis of the M1.2/M1.3 throughput shortfall.
//!
//! Usage: `cargo run --release --example append_only_benchmark`

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use rubixdb::wal::{FileWal, Wal, WalConfig, WalOp};

const THREADS: usize = 100;
const PER_THREAD: usize = 1_000;

fn main() {
    let dir = std::env::temp_dir().join(format!(
        "rubixdb_append_only_benchmark_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let (wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
    let wal = Arc::new(Mutex::new(wal));

    let started = Instant::now();
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let wal = Arc::clone(&wal);
            thread::spawn(move || {
                for i in 0..PER_THREAD {
                    let key = format!("t{t}-{i}");
                    let mut w = wal.lock().unwrap();
                    w.append(WalOp::Put {
                        key: key.as_bytes(),
                        value: b"v",
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let elapsed = started.elapsed();
    let total = THREADS * PER_THREAD;
    println!(
        "append-only (Mutex<FileWal>::append, no fsync, {THREADS} threads): \
         {total} ops in {:.3}s => {:.0} ops/sec",
        elapsed.as_secs_f64(),
        (total as f64) / elapsed.as_secs_f64()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
