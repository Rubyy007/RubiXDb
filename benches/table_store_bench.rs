//! `RELATIONAL ADR AMENDMENT 003`: measures the encoding overhead
//! `TableStore` adds on top of the already-benchmarked `LsmEngine::put`/
//! `get`/`write_batch` (`write_batch_bench.rs`) — row/key encode+decode,
//! catalog metadata resolution, primary-key reconstruction. Numbers here
//! belong in `PHASE_RELATIONAL_TRANSACTION_STORAGE_RESULTS.md`-style
//! results docs once actually run, never claimed without measurement
//! (mirroring `wal_bench.rs`/`write_batch_bench.rs`'s own rationale).

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::value::{TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::{RelationalValue, TableStore};
use rubixdb::wal::{SyncMode, WalConfig};
use std::sync::Arc;
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

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("rubixdb_table_store_bench_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn open_engine(dir: &std::path::Path) -> Arc<LsmEngine> {
    Arc::new(
        LsmEngine::open(
            dir,
            bench_wal_config(),
            Default::default(),
            LsmConfig::default(),
        )
        .unwrap(),
    )
}

fn setup_table(dir: &std::path::Path) -> (Arc<LsmEngine>, Arc<CatalogService>, TableStore, u32) {
    let engine = open_engine(dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let columns = vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: TYPE_TAG_INTEGER,
            nullable: false,
            default_value: None,
            type_params: None,
        },
        ColumnDef {
            name: "value".to_string(),
            data_type: TYPE_TAG_TEXT,
            nullable: true,
            default_value: None,
            type_params: None,
        },
    ];
    let table_id = catalog
        .create_table(1, "bench_table", &columns, &[0])
        .unwrap();
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
    (engine, catalog, store, table_id)
}

/// Encoding overhead on the write path: raw `put` vs. `put_row`.
fn bench_put_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("table_store_put_overhead");

    group.bench_function("raw_engine_put", |b| {
        let dir = temp_dir("raw_put");
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

    group.bench_function("table_store_put_row", |b| {
        let dir = temp_dir("put_row");
        let (engine, _catalog, store, table_id) = setup_table(&dir);
        let mut i = 0i32;
        b.iter(|| {
            store
                .put_row(
                    table_id,
                    &[
                        Some(RelationalValue::Integer(i)),
                        Some(RelationalValue::Text("benchmark-value".to_string())),
                    ],
                )
                .unwrap();
            i += 1;
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    });

    group.finish();
}

/// Encoding overhead on the read path: raw `get` vs. `get_row`.
fn bench_get_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("table_store_get_overhead");

    group.bench_function("raw_engine_get", |b| {
        let dir = temp_dir("raw_get");
        let engine = open_engine(&dir);
        engine.put(b"k0", b"benchmark-value").unwrap();
        b.iter(|| {
            let _ = engine.get(b"k0").unwrap();
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    });

    group.bench_function("table_store_get_row", |b| {
        let dir = temp_dir("get_row");
        let (engine, _catalog, store, table_id) = setup_table(&dir);
        store
            .put_row(
                table_id,
                &[
                    Some(RelationalValue::Integer(0)),
                    Some(RelationalValue::Text("benchmark-value".to_string())),
                ],
            )
            .unwrap();
        b.iter(|| {
            let _ = store
                .get_row(table_id, &[RelationalValue::Integer(0)])
                .unwrap();
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    });

    group.finish();
}

/// `scan_table` throughput at increasing row counts.
fn bench_scan_table(c: &mut Criterion) {
    let mut group = c.benchmark_group("table_store_scan_table");
    for row_count in [100u32, 1_000] {
        group.throughput(Throughput::Elements(row_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(row_count),
            &row_count,
            |b, &row_count| {
                let dir = temp_dir(&format!("scan_{row_count}"));
                let (engine, _catalog, store, table_id) = setup_table(&dir);
                for i in 0..row_count as i32 {
                    store
                        .put_row(table_id, &[Some(RelationalValue::Integer(i)), None])
                        .unwrap();
                }
                b.iter(|| {
                    let rows = store.scan_table(table_id).unwrap();
                    std::hint::black_box(rows.len());
                });
                engine.shutdown();
                let _ = std::fs::remove_dir_all(&dir);
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_put_overhead,
    bench_get_overhead,
    bench_scan_table
);
criterion_main!(benches);
