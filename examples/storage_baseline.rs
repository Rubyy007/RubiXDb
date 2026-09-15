//! **Analysis-only tool, not part of RubiXDB.** Measures raw storage
//! durability behavior (`fsync`/`FlushFileBuffers` latency, sequential
//! write throughput) using nothing but `std::fs` — no WAL code, no
//! `GroupCommitter`, no dependency on anything under `src/`. Exists to
//! establish an independent "durability floor" baseline for
//! `FINAL_WAL_ANALYSIS.md` §5, so that WAL-level measurements can be
//! compared against the raw hardware/filesystem behavior instead of only
//! against themselves.
//!
//! Usage: `cargo run --release --example storage_baseline -- <dir> [label]`
//! `<dir>` must already exist and be writable. Every file this program
//! creates is removed before it exits (best-effort on panic, guaranteed
//! on a normal run) — this tool must never leave files behind on a
//! caller-specified directory, especially one on a production/near-full
//! volume.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn percentile(sorted_ns: &[u128], pct: f64) -> u128 {
    if sorted_ns.is_empty() {
        return 0;
    }
    let idx = ((sorted_ns.len() as f64) * pct) as usize;
    sorted_ns[idx.min(sorted_ns.len() - 1)]
}

fn stats_line(name: &str, mut samples_ns: Vec<u128>) {
    samples_ns.sort_unstable();
    let n = samples_ns.len();
    let sum: u128 = samples_ns.iter().sum();
    let mean = sum as f64 / n as f64;
    let p50 = percentile(&samples_ns, 0.50);
    let p95 = percentile(&samples_ns, 0.95);
    let p99 = percentile(&samples_ns, 0.99);
    let max = *samples_ns.last().unwrap();
    let min = *samples_ns.first().unwrap();
    println!(
        "{name}: n={n} min={min}ns mean={mean:.0}ns p50={p50}ns p95={p95}ns p99={p99}ns max={max}ns"
    );
}

/// One `write` + one full durability flush (`sync_all`, which calls
/// `FlushFileBuffers` on Windows / `fsync` on Unix), timed as a single
/// unit — the same unit of work `FileWal`'s `sync()` and `GroupCommitter`'s
/// leader `fsync` call both pay once per physical sync.
fn measure_sync_write(dir: &Path, label: &str, payload_len: usize, iterations: usize) {
    let path = dir.join(format!("storage_baseline_{label}.tmp"));
    let payload = vec![0xABu8; payload_len];
    let mut samples = Vec::with_capacity(iterations);

    // One untimed warm-up write+sync so the file exists and the first
    // real sample isn't paying for directory-entry creation too.
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("warm-up open");
        f.write_all(&payload).expect("warm-up write");
        f.sync_all().expect("warm-up sync");
    }

    let mut f = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("reopen for measured loop");
    for _ in 0..iterations {
        // Positional write at offset 0 each time (like a fixed-size
        // record being overwritten) so this isolates flush latency, not
        // file-growth/allocation cost.
        use std::io::{Seek, SeekFrom};
        f.seek(SeekFrom::Start(0)).expect("seek");
        let start = Instant::now();
        f.write_all(&payload).expect("write");
        f.sync_all().expect("sync_all (fsync / FlushFileBuffers)");
        samples.push(start.elapsed().as_nanos());
    }
    drop(f);
    let _ = fs::remove_file(&path);

    stats_line(
        &format!("sync_write[{label}, {payload_len}B, n={iterations}]"),
        samples,
    );
}

/// Sequential write throughput with a single `sync_all` at the end (not
/// per-record) — isolates raw sequential write bandwidth from per-call
/// flush overhead.
fn measure_sequential_throughput(dir: &Path, total_bytes: usize, chunk_len: usize) {
    let path = dir.join("storage_baseline_seq.tmp");
    let chunk = vec![0xCDu8; chunk_len];
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("open for sequential write");

    let start = Instant::now();
    let mut written = 0usize;
    while written < total_bytes {
        f.write_all(&chunk).expect("sequential write");
        written += chunk_len;
    }
    f.sync_all().expect("final sync");
    let elapsed = start.elapsed();
    drop(f);
    let _ = fs::remove_file(&path);

    let mb = written as f64 / (1024.0 * 1024.0);
    let secs = elapsed.as_secs_f64();
    println!(
        "sequential_write: {written} bytes ({mb:.1} MiB) in {:.3}s => {:.1} MiB/s (chunk={chunk_len}B)",
        secs,
        mb / secs
    );
}

/// Repeated `fsync`-only latency (no new bytes each time) — isolates the
/// pure flush-call cost from write cost, closest to what `GroupCommitter`
/// pays when many records share one `fsync`.
fn measure_fsync_only(dir: &Path, iterations: usize) {
    let path = dir.join("storage_baseline_fsync_only.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("create");
        f.write_all(&[0u8; 4096]).expect("initial write");
        f.sync_all().expect("initial sync");
    }
    let f = OpenOptions::new().write(true).open(&path).expect("reopen");
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        f.sync_all().expect("fsync-only");
        samples.push(start.elapsed().as_nanos());
    }
    drop(f);
    let _ = fs::remove_file(&path);
    stats_line(&format!("fsync_only[n={iterations}]"), samples);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: storage_baseline <existing-writable-dir> [label]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let label = args.get(2).cloned().unwrap_or_else(|| "run".to_string());
    assert!(
        dir.is_dir(),
        "{dir:?} must already exist and be a directory"
    );

    println!("=== storage_baseline: {label} ({dir:?}) ===");

    // Small synchronous writes at sizes relevant to WAL frames: a 24-byte
    // segment header, a small ~40-byte record (matching this crate's
    // crash-consistency test records), and 4 KiB (the brief's requested
    // point, also a common filesystem block size).
    measure_sync_write(&dir, "24B", 24, 200);
    measure_sync_write(&dir, "256B", 256, 200);
    measure_sync_write(&dir, "4KiB", 4096, 200);

    measure_fsync_only(&dir, 200);

    // Sequential throughput: 32 MiB in 64 KiB chunks (matches this
    // crate's `max_batch_bytes` default), one sync at the end.
    measure_sequential_throughput(&dir, 32 * 1024 * 1024, 64 * 1024);

    // A tiny explicit file existence check at the end: this tool must
    // leave nothing behind.
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("storage_baseline_")
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "storage_baseline left files behind: {leftovers:?}"
    );
    println!("=== done, no files left behind ===");
}
