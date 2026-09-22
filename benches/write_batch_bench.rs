//! `RELATIONAL ADR AMENDMENT 001` AA.5's required, testable performance
//! properties for `LsmEngine::write_batch` — measured here, not assumed.
//! Numbers this produces, once actually run, belong in
//! `PHASE_RELATIONAL_TRANSACTION_STORAGE_RESULTS.md`, per this project's
//! own "measure everything, never claim a performance improvement until
//! measured" discipline (mirrors `wal_bench.rs`'s identical rationale).
//!
//! Two groups:
//! 1. **N=1 parity**: `write_batch([Put])`/`write_batch([Delete])` against
//!    today's `put`/`delete` — must show no material regression.
//! 2. **N>1 throughput**: `write_batch` at N ∈ {2,4,8,16,32,64} against an
//!    equivalent *serialized* baseline (N sequential `put`/`delete` calls
//!    from one caller) — the purpose is to prove one WAL fsync per batch
//!    actually reduces durability overhead relative to N independent
//!    single-key calls from the same caller, not merely to confirm
//!    correctness.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rubixdb::lsm::{LsmConfig, LsmEngine, WriteOp};
use rubixdb::wal::{SyncMode, WalConfig};
use std::time::Duration;

fn bench_wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn temp_engine_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_write_batch_bench_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn open_engine(dir: &std::path::Path) -> LsmEngine {
    LsmEngine::open(
        dir,
        bench_wal_config(),
        Default::default(),
        LsmConfig::default(),
    )
    .unwrap()
}

/// AA.5 property 1: N=1 parity.
fn bench_n1_parity(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_batch_n1_parity");

    group.bench_function("put", |b| {
        let dir = temp_engine_dir("n1_put");
        let engine = open_engine(&dir);
        let mut i = 0u64;
        b.iter(|| {
            engine
                .put(format!("k{i}").as_bytes(), b"benchmark-value")
                .unwrap();
            i += 1;
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    });

    group.bench_function("write_batch_single_put", |b| {
        let dir = temp_engine_dir("n1_wb_put");
        let engine = open_engine(&dir);
        let mut i = 0u64;
        b.iter(|| {
            engine
                .write_batch(&[WriteOp::Put {
                    key: format!("k{i}").into_bytes(),
                    value: b"benchmark-value".to_vec(),
                }])
                .unwrap();
            i += 1;
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    });

    group.bench_function("delete", |b| {
        let dir = temp_engine_dir("n1_delete");
        let engine = open_engine(&dir);
        let mut i = 0u64;
        b.iter(|| {
            let key = format!("k{i}");
            engine.put(key.as_bytes(), b"v").unwrap();
            engine.delete(key.as_bytes()).unwrap();
            i += 1;
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    });

    group.bench_function("write_batch_single_delete", |b| {
        let dir = temp_engine_dir("n1_wb_delete");
        let engine = open_engine(&dir);
        let mut i = 0u64;
        b.iter(|| {
            let key = format!("k{i}");
            engine.put(key.as_bytes(), b"v").unwrap();
            engine
                .write_batch(&[WriteOp::Delete {
                    key: key.into_bytes(),
                }])
                .unwrap();
            i += 1;
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    });

    group.finish();
}

/// AA.5 property 2: multi-op throughput vs. an equivalent serialized
/// baseline, at N ∈ {2,4,8,16,32,64}.
fn bench_multi_op_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_batch_multi_op_vs_serialized");
    for n in [2usize, 4, 8, 16, 32, 64] {
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("serialized_put", n), &n, |b, &n| {
            let dir = temp_engine_dir(&format!("multi_serialized_{n}"));
            let engine = open_engine(&dir);
            let mut round = 0u64;
            b.iter(|| {
                for j in 0..n {
                    engine
                        .put(format!("r{round}k{j}").as_bytes(), b"benchmark-value")
                        .unwrap();
                }
                round += 1;
            });
            engine.shutdown();
            let _ = std::fs::remove_dir_all(&dir);
        });

        group.bench_with_input(BenchmarkId::new("write_batch", n), &n, |b, &n| {
            let dir = temp_engine_dir(&format!("multi_batch_{n}"));
            let engine = open_engine(&dir);
            let mut round = 0u64;
            b.iter(|| {
                let ops: Vec<WriteOp> = (0..n)
                    .map(|j| WriteOp::Put {
                        key: format!("r{round}k{j}").into_bytes(),
                        value: b"benchmark-value".to_vec(),
                    })
                    .collect();
                engine.write_batch(&ops).unwrap();
                round += 1;
            });
            engine.shutdown();
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
    group.finish();
}

criterion_group!(benches, bench_n1_parity, bench_multi_op_throughput);
criterion_main!(benches);
