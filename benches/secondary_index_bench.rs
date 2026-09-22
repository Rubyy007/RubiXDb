//! `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` items 23–26: backfill
//! throughput, index lookup/range-scan latency vs. a full table scan, and
//! write amplification as indexed-column count grows. Fixture rows are
//! populated via `TableStore::put_rows` (one `write_batch` per chunk),
//! never one `fsync`-bound `put_row` per row — `PHASE_RELATIONAL_ROW_
//! STORAGE_RESULTS.md` §6 already measured that mistake costing ~15
//! minutes for 10,000 rows; this file does not repeat it. Numbers here
//! belong in `PHASE_RELATIONAL_INDEX_INCREMENT5_RESULTS.md` once actually
//! run, never claimed without measurement.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::value::{TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::{RelationalValue, TableStore};
use rubixdb::wal::{SyncMode, WalConfig};
use std::sync::Arc;
use std::time::Duration;

const FIXTURE_CHUNK: usize = 2_000;

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
    let path = std::env::temp_dir().join(format!("rubixdb_index_bench_{tag}_{nanos}"));
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

/// `t(id INTEGER PRIMARY KEY, value TEXT)`, plus `extra_cols` additional
/// nullable TEXT columns — used by the write-amplification benchmark to
/// build tables with 0..N indexable columns without changing the schema
/// shape test-to-test.
fn setup_table(
    dir: &std::path::Path,
    extra_cols: usize,
) -> (Arc<LsmEngine>, Arc<CatalogService>, Arc<TableStore>, u32) {
    let engine = open_engine(dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let mut columns = vec![
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
    for i in 0..extra_cols {
        columns.push(ColumnDef {
            name: format!("col{i}"),
            data_type: TYPE_TAG_TEXT,
            nullable: true,
            default_value: None,
            type_params: None,
        });
    }
    let table_id = catalog
        .create_table(1, "bench_table", &columns, &[0])
        .unwrap();
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    (engine, catalog, store, table_id)
}

/// Populates `row_count` rows via chunked `put_rows` (real `write_batch`
/// fixture construction, not per-row `fsync`).
fn populate(store: &TableStore, table_id: u32, row_count: u32, column_count: usize) {
    let mut chunk = Vec::with_capacity(FIXTURE_CHUNK);
    for i in 0..row_count as i32 {
        let mut row = vec![
            Some(RelationalValue::Integer(i)),
            Some(RelationalValue::Text(format!("value-{i}"))),
        ];
        for c in 0..column_count.saturating_sub(2) {
            row.push(Some(RelationalValue::Text(format!("extra-{c}-{i}"))));
        }
        chunk.push(row);
        if chunk.len() == FIXTURE_CHUNK {
            store.put_rows(table_id, &chunk).unwrap();
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        store.put_rows(table_id, &chunk).unwrap();
    }
}

/// Item 23: backfill throughput (rows/sec) at increasing pre-existing
/// table sizes.
fn bench_backfill_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_backfill_throughput");
    group.sample_size(10);
    for row_count in [1_000u32, 5_000] {
        group.throughput(Throughput::Elements(row_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(row_count),
            &row_count,
            |b, &row_count| {
                b.iter_batched(
                    || {
                        let dir = temp_dir(&format!("backfill_{row_count}"));
                        let (engine, catalog, store, table_id) = setup_table(&dir, 0);
                        populate(&store, table_id, row_count, 2);
                        let builder = IndexBuilder::new(
                            Arc::clone(&engine),
                            Arc::clone(&catalog),
                            Arc::clone(&store),
                        );
                        (dir, engine, builder, table_id)
                    },
                    |(dir, engine, builder, table_id)| {
                        builder
                            .create_index_online(
                                table_id,
                                "value_idx",
                                rubixdb::catalog::IndexKind::NonUnique,
                                &[1],
                            )
                            .unwrap();
                        engine.shutdown();
                        let _ = std::fs::remove_dir_all(&dir);
                    },
                    criterion::BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
}

/// Item 24: indexed equality lookup and range scan vs. an equivalent
/// full-table scan filtered in memory, for a selective predicate (1 row
/// out of `row_count`).
fn bench_lookup_vs_full_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_lookup_vs_full_scan");
    let row_count = 5_000u32;
    let dir = temp_dir("lookup_vs_scan");
    let (engine, catalog, store, table_id) = setup_table(&dir, 0);
    populate(&store, table_id, row_count, 2);
    let builder = IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    );
    let index_id = builder
        .create_index_online(
            table_id,
            "value_idx",
            rubixdb::catalog::IndexKind::NonUnique,
            &[1],
        )
        .unwrap();
    let needle = format!("value-{}", row_count / 2);

    group.bench_function("index_lookup", |b| {
        b.iter(|| {
            let rows = builder
                .index_lookup(index_id, &[Some(RelationalValue::Text(needle.clone()))])
                .unwrap();
            std::hint::black_box(rows.len());
        });
    });

    group.bench_function("full_table_scan_filtered_in_memory", |b| {
        b.iter(|| {
            let rows = store.scan_table(table_id).unwrap();
            let matches = rows
                .iter()
                .filter(
                    |(_, row)| matches!(&row[1], Some(RelationalValue::Text(v)) if *v == needle),
                )
                .count();
            std::hint::black_box(matches);
        });
    });

    group.bench_function("index_range_scan_full", |b| {
        b.iter(|| {
            let rows = builder
                .index_range_scan(
                    index_id,
                    std::ops::Bound::Unbounded,
                    std::ops::Bound::Unbounded,
                )
                .unwrap();
            std::hint::black_box(rows.len());
        });
    });

    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    group.finish();
}

/// Item 25: one-row write latency as the number of maintained indexes
/// grows (0, 1, 2, 5, 10).
fn bench_write_amplification(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_write_amplification");
    for index_count in [0usize, 1, 2, 5, 10] {
        group.bench_with_input(
            BenchmarkId::from_parameter(index_count),
            &index_count,
            |b, &index_count| {
                let dir = temp_dir(&format!("write_amp_{index_count}"));
                let (engine, catalog, store, table_id) = setup_table(&dir, index_count);
                let builder = IndexBuilder::new(
                    Arc::clone(&engine),
                    Arc::clone(&catalog),
                    Arc::clone(&store),
                );
                for c in 0..index_count {
                    builder
                        .create_index_online(
                            table_id,
                            &format!("col{c}_idx"),
                            rubixdb::catalog::IndexKind::NonUnique,
                            &[(c + 2) as u16],
                        )
                        .unwrap();
                }
                let mut i = 0i32;
                b.iter(|| {
                    let mut row = vec![
                        Some(RelationalValue::Integer(i)),
                        Some(RelationalValue::Text(format!("value-{i}"))),
                    ];
                    for c in 0..index_count {
                        row.push(Some(RelationalValue::Text(format!("extra-{c}-{i}"))));
                    }
                    store.put_row(table_id, &row).unwrap();
                    i += 1;
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
    bench_backfill_throughput,
    bench_lookup_vs_full_scan,
    bench_write_amplification
);
criterion_main!(benches);
