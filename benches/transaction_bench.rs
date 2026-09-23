//! `PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` performance section —
//! Increment 7 items 33–39: `BEGIN` latency, read overhead across the
//! four read paths, commit latency vs. write-set size, conflict-
//! validation cost vs. write-set size (must not scan the whole table —
//! measured directly by holding write-set size fixed while table size
//! grows), catalog-resolution cost vs. catalog size (reusing `sql/`'s
//! own `bench_bind_vs_catalog_size` methodology one crate over, per
//! that file's own "no automatic caching" finding), and commit
//! concurrency scaling. Fixture construction is chunked `put_rows`
//! (one `write_batch` per chunk), never per-row `put_row` in a loop —
//! `PHASE_RELATIONAL_ROW_STORAGE_RESULTS.md` §6's own precedent.

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::value::{TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::{RelationalValue, TableStore, TransactionManager};
use rubixdb::wal::{SyncMode, WalConfig};

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
    let path = std::env::temp_dir().join(format!("rubixdb_txn_bench_{tag}_{nanos}"));
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

fn setup_table(
    dir: &std::path::Path,
) -> (Arc<LsmEngine>, Arc<CatalogService>, Arc<TableStore>, u32) {
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
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    (engine, catalog, store, table_id)
}

fn row_at(i: i32) -> Vec<Option<RelationalValue>> {
    vec![
        Some(RelationalValue::Integer(i)),
        Some(RelationalValue::Text(format!("value-{i}"))),
    ]
}

fn populate(store: &TableStore, table_id: u32, row_count: u32) {
    let mut chunk = Vec::with_capacity(FIXTURE_CHUNK);
    for i in 0..row_count as i32 {
        chunk.push(row_at(i));
        if chunk.len() == FIXTURE_CHUNK {
            store.put_rows(table_id, &chunk).unwrap();
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        store.put_rows(table_id, &chunk).unwrap();
    }
}

/// Item 33: `BEGIN` latency (snapshot capture + registry bookkeeping).
fn bench_begin_latency(c: &mut Criterion) {
    let dir = temp_dir("begin_latency");
    let (engine, _catalog, store, _table_id) = setup_table(&dir);
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));

    c.bench_function("txn_begin", |b| {
        b.iter(|| {
            let tx = txm.begin().unwrap();
            std::hint::black_box(&tx);
            drop(tx); // implicit rollback; never touches the engine
        });
    });

    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 34: read overhead across the four read paths a caller can take
/// to the same committed row: a raw, non-snapshotted `get`; an explicit
/// `get_as_of` at a fixed seq (what every transactional read compiles
/// down to); a `Transaction::get_row` hitting the engine (no local
/// overlay); and a `Transaction::get_row` served entirely from the
/// local write-set (read-your-own-writes, no engine call at all).
fn bench_read_paths(c: &mut Criterion) {
    let dir = temp_dir("read_paths");
    let (engine, _catalog, store, table_id) = setup_table(&dir);
    populate(&store, table_id, 10_000);
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));

    let key = rubixdb::relational::key::table_row_key(
        table_id,
        &rubixdb::relational::key::encode_composite_key(&[RelationalValue::Integer(5_000)])
            .unwrap(),
    );
    let seq = engine.snapshot().seq();

    let mut group = c.benchmark_group("txn_read_overhead");

    group.bench_function("raw_get", |b| {
        b.iter(|| {
            let v = engine.get(&key).unwrap();
            std::hint::black_box(v);
        });
    });

    group.bench_function("get_as_of", |b| {
        b.iter(|| {
            let v = engine.get_as_of(&key, seq).unwrap();
            std::hint::black_box(v);
        });
    });

    group.bench_function("transactional_get_row_engine_hit", |b| {
        let tx = txm.begin().unwrap();
        b.iter(|| {
            let v = tx
                .get_row(table_id, &[RelationalValue::Integer(5_000)])
                .unwrap();
            std::hint::black_box(v);
        });
    });

    group.bench_function("transactional_get_row_local_overlay", |b| {
        let mut tx = txm.begin().unwrap();
        tx.put_row(table_id, &row_at(5_000)).unwrap();
        b.iter(|| {
            let v = tx
                .get_row(table_id, &[RelationalValue::Integer(5_000)])
                .unwrap();
            std::hint::black_box(v);
        });
    });

    group.finish();
    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 35: commit latency (validation + `write_batch` apply) at
/// increasing write-set sizes, against a fixed, small table.
fn bench_commit_latency_vs_write_set_size(c: &mut Criterion) {
    let dir = temp_dir("commit_vs_write_set");
    let (engine, _catalog, store, table_id) = setup_table(&dir);
    populate(&store, table_id, 1_000);
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));

    let mut group = c.benchmark_group("txn_commit_vs_write_set_size");
    for write_set_size in [1u32, 2, 4, 8, 16, 32, 64, 128] {
        group.bench_with_input(
            BenchmarkId::from_parameter(write_set_size),
            &write_set_size,
            |b, &write_set_size| {
                let mut base = 1_000_000i32;
                b.iter_batched(
                    || {
                        let mut tx = txm.begin().unwrap();
                        for i in 0..write_set_size {
                            tx.put_row(table_id, &row_at(base + i as i32)).unwrap();
                        }
                        base += write_set_size as i32;
                        tx
                    },
                    |tx| {
                        tx.commit().unwrap();
                    },
                    criterion::BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 35 (continued): conflict-validation cost must scale with
/// write-set size, *not* table size — a fixed 4-row write-set, measured
/// against tables of growing size, should show flat commit latency.
fn bench_commit_latency_vs_table_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("txn_commit_vs_table_size_fixed_write_set");
    for table_size in [100u32, 1_000, 10_000, 50_000] {
        let dir = temp_dir(&format!("commit_vs_table_{table_size}"));
        let (engine, _catalog, store, table_id) = setup_table(&dir);
        populate(&store, table_id, table_size);
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
        let mut base = 10_000_000i32;

        group.bench_with_input(
            BenchmarkId::from_parameter(table_size),
            &table_size,
            |b, _| {
                b.iter_batched(
                    || {
                        let mut tx = txm.begin().unwrap();
                        for i in 0..4 {
                            tx.put_row(table_id, &row_at(base + i)).unwrap();
                        }
                        base += 4;
                        tx
                    },
                    |tx| {
                        tx.commit().unwrap();
                    },
                    criterion::BatchSize::PerIteration,
                );
            },
        );
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 36: catalog-resolution cost — the exact two calls `TableStore::
/// resolve_table` makes (`CatalogService::get_table` + `get_columns`),
/// which every `Transaction::put_row`/`delete_row`/commit-time
/// validation pays once per touched table — vs. catalog size. Same
/// methodology and the same underlying finding as `sql/benches/
/// sql_binder_bench.rs`'s own `bench_bind_vs_catalog_size`, measured
/// one layer lower (`resolve_table` itself is `pub(crate)`, so this
/// benchmark, an external crate, calls the two catalog methods it
/// wraps directly — identical cost, since neither call is memoized).
/// No caching is added by this increment; this bench exists to make
/// that consequence visible, not to justify adding one.
fn bench_resolve_table_vs_catalog_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("txn_resolve_table_vs_catalog_size");
    group.sample_size(20);
    for table_count in [100usize, 1_000, 5_000, 10_000] {
        let dir = temp_dir(&format!("catalog_size_{table_count}"));
        let engine = open_engine(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let mut last_id = 0u32;
        for i in 0..table_count {
            last_id = catalog
                .create_table(
                    1,
                    &format!("table_{i}"),
                    &[ColumnDef {
                        name: "id".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    }],
                    &[0],
                )
                .unwrap();
        }

        group.bench_with_input(
            BenchmarkId::from_parameter(table_count),
            &table_count,
            |b, _| {
                b.iter(|| {
                    let table = catalog.get_table(last_id).unwrap();
                    let columns = catalog.get_columns(last_id).unwrap();
                    std::hint::black_box((table, columns));
                });
            },
        );

        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 37: commit throughput scaling with concurrent transactions on
/// disjoint keys (no conflicts, so this isolates lock/validation
/// overhead from conflict-retry cost).
fn bench_concurrent_commits(c: &mut Criterion) {
    let mut group = c.benchmark_group("txn_concurrent_commits_disjoint_keys");
    group.sample_size(10);
    for n_threads in [1usize, 2, 4, 8, 16, 32] {
        let dir = temp_dir(&format!("concurrency_{n_threads}"));
        let (engine, _catalog, store, table_id) = setup_table(&dir);
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
        let mut base = 0i32;

        group.throughput(criterion::Throughput::Elements(n_threads as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(n_threads),
            &n_threads,
            |b, &n_threads| {
                b.iter_batched(
                    || {
                        let start = base;
                        base += n_threads as i32;
                        start
                    },
                    |start| {
                        let barrier = Arc::new(Barrier::new(n_threads));
                        let handles: Vec<_> = (0..n_threads)
                            .map(|t| {
                                let txm = txm.clone();
                                let barrier = Arc::clone(&barrier);
                                thread::spawn(move || {
                                    let mut tx = txm.begin().unwrap();
                                    tx.put_row(table_id, &row_at(start + t as i32)).unwrap();
                                    barrier.wait();
                                    tx.commit().unwrap();
                                })
                            })
                            .collect();
                        for h in handles {
                            h.join().unwrap();
                        }
                    },
                    criterion::BatchSize::PerIteration,
                );
            },
        );
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_begin_latency,
    bench_read_paths,
    bench_commit_latency_vs_write_set_size,
    bench_commit_latency_vs_table_size,
    bench_resolve_table_vs_catalog_size,
    bench_concurrent_commits
);
criterion_main!(benches);
