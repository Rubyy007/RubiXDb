//! Phase 4A MemTable-only performance harness (operating brief §32):
//! Put throughput, Get latency, ordered iteration, and mixed read/write
//! — in isolation from the WAL, so MemTable's own overhead is never
//! hidden inside a WAL benchmark's own numbers (operating brief §32's
//! explicit instruction).
//!
//! Usage: `cargo run --release --example memtable_bench -- [n=100000]`

use std::env;
use std::ops::Bound;
use std::time::Instant;

use rubixdb::memtable::MemTable;

fn main() {
    let args: Vec<String> = env::args().collect();
    let n: u64 = args.get(1).map(|s| s.parse().unwrap()).unwrap_or(100_000);

    // Put throughput.
    let mut m = MemTable::new(usize::MAX);
    let started = Instant::now();
    for i in 0..n {
        m.put(
            format!("key-{i:010}").as_bytes(),
            i + 1,
            b"a-modest-sized-value-payload",
        );
    }
    let put_elapsed = started.elapsed();
    println!(
        "put: n={n} elapsed={:.3}s ops_per_sec={:.0}",
        put_elapsed.as_secs_f64(),
        n as f64 / put_elapsed.as_secs_f64()
    );
    println!(
        "size_bytes={} entry_count={}",
        m.size_bytes(),
        m.entry_count()
    );

    // Get latency (point lookups, uniformly across the key space).
    let mut latencies_ns: Vec<u128> = Vec::with_capacity(n as usize);
    for i in 0..n {
        let key = format!("key-{i:010}");
        let started = Instant::now();
        let _ = std::hint::black_box(m.get_value(key.as_bytes(), u64::MAX));
        latencies_ns.push(started.elapsed().as_nanos());
    }
    latencies_ns.sort_unstable();
    let p50 = latencies_ns[latencies_ns.len() / 2];
    let p99 = latencies_ns[(latencies_ns.len() * 99) / 100];
    println!(
        "get: n={n} p50_ns={p50} p99_ns={p99} max_ns={}",
        latencies_ns.last().unwrap_or(&0)
    );

    // Ordered iteration throughput.
    let started = Instant::now();
    let count = m.range(Bound::Unbounded, Bound::Unbounded).count();
    let iter_elapsed = started.elapsed();
    println!(
        "range_iteration: entries={count} elapsed={:.3}s entries_per_sec={:.0}",
        iter_elapsed.as_secs_f64(),
        count as f64 / iter_elapsed.as_secs_f64()
    );

    // Mixed read/write (50/50, single-threaded — MemTable itself has no
    // internal synchronization to benchmark concurrently in isolation;
    // concurrent read/write throughput through the real coordinator is
    // covered by examples/lsm_load_test.rs instead).
    let mut m2 = MemTable::new(usize::MAX);
    let started = Instant::now();
    for i in 0..n {
        if i.is_multiple_of(2) {
            m2.put(format!("key-{i:010}").as_bytes(), i + 1, b"value");
        } else {
            let _ = std::hint::black_box(
                m2.get_value(format!("key-{:010}", i - 1).as_bytes(), u64::MAX),
            );
        }
    }
    let mixed_elapsed = started.elapsed();
    println!(
        "mixed_50_50: n={n} elapsed={:.3}s ops_per_sec={:.0}",
        mixed_elapsed.as_secs_f64(),
        n as f64 / mixed_elapsed.as_secs_f64()
    );
}
