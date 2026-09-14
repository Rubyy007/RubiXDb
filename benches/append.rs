//! Group 6.1: single-`append` latency, isolated from `sync`/`fsync` (unlike
//! `wal_bench.rs`'s `append_sync` benchmark), to measure the actual cost of
//! `SegmentIo::append`'s write path — including, on Unix, the
//! `write_all_at` (`pwrite`) fast path that eliminates the separate `seek`
//! syscall `append_sync` benchmark elsewhere still pays for indirectly.
//!
//! Gated behind the `bench` feature (`cargo bench --features bench`) per
//! `Cargo.toml`'s `required-features` — kept out of the default `cargo
//! bench` run so it doesn't slow down routine benchmarking of the rest of
//! the crate.

#![cfg(feature = "bench")]

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rubixdb::wal::{FileWal, Wal, WalConfig, WalOp};

fn temp_wal_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_append_bench_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn bench_append_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_append_only_no_sync");
    for payload_size in [16usize, 256, 4096] {
        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(payload_size),
            &payload_size,
            |b, &size| {
                let dir = temp_wal_dir(&format!("append_{size}"));
                // A large segment size so this benchmark measures pure
                // append cost, not occasional rotation overhead.
                let config = WalConfig {
                    max_segment_size: 1024 * 1024 * 1024,
                    ..WalConfig::default()
                };
                let (mut wal, _) = FileWal::open_for_recovery(&dir, config).unwrap();
                let value = vec![0xCDu8; size];
                b.iter(|| {
                    wal.append(WalOp::Put {
                        key: b"benchmark-key",
                        value: &value,
                    })
                    .unwrap();
                });
                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_append_only);
criterion_main!(benches);
