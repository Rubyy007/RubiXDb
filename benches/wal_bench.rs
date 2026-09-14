//! WAL Spec §12: `append_sync` latency (p50/p99, via Criterion's own
//! statistical reporting) at a few payload sizes under `Immediate` mode,
//! achieved throughput, and recovery replay throughput. Numbers this
//! produces, once actually run, belong in `PROGRESS.md` — never asserted
//! without having been measured (Architecture Spec §11.2).

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rubixdb::wal::{FileWal, Wal, WalConfig, WalOp};

fn temp_wal_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_wal_bench_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn bench_append_sync(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_append_sync");
    for payload_size in [16usize, 256, 4096] {
        group.throughput(Throughput::Bytes(payload_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(payload_size),
            &payload_size,
            |b, &size| {
                let dir = temp_wal_dir(&format!("append_sync_{size}"));
                let (mut wal, _) = FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
                let value = vec![0xABu8; size];
                b.iter(|| {
                    wal.append_sync(WalOp::Put {
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

fn bench_recovery_replay(c: &mut Criterion) {
    let mut group = c.benchmark_group("wal_recovery_replay");
    for record_count in [100u64, 1_000, 10_000] {
        group.throughput(Throughput::Elements(record_count));
        group.bench_with_input(
            BenchmarkId::from_parameter(record_count),
            &record_count,
            |b, &count| {
                let dir = temp_wal_dir(&format!("recovery_{count}"));
                {
                    let (mut wal, _) =
                        FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
                    for i in 0..count {
                        wal.append(WalOp::Put {
                            key: format!("key-{i}").as_bytes(),
                            value: b"benchmark-value",
                        })
                        .unwrap();
                    }
                    wal.sync().unwrap();
                }
                b.iter(|| {
                    let (_, result) =
                        FileWal::open_for_recovery(&dir, WalConfig::default()).unwrap();
                    std::hint::black_box(result.records.len());
                });
                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_append_sync, bench_recovery_replay);
criterion_main!(benches);
