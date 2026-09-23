//! Binder performance vs. catalog size — item 36. Measures `bind_
//! statement` latency (p50/p95/p99/max via Criterion's own sample
//! statistics) as the number of tables in scope grows. Catalog fixture
//! construction (up to several thousand individual `CREATE TABLE`
//! calls, each its own `fsync`-bound `write_batch`) is done once per
//! group, outside the timed loop — `PHASE_RELATIONAL_ROW_STORAGE_
//! RESULTS.md` §6's own "separate fixture setup from the measured
//! operation" precedent, reused here one crate over.

use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::value::TYPE_TAG_INTEGER;
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_sql::auth::AuthContext;
use rubixdb_sql::bind::{bind_statement, BindContext};
use rubixdb_sql::limits::SqlLimits;
use rubixdb_sql::metrics::SqlMetrics;
use rubixdb_sql::parse::parse_statement;

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
    let path = std::env::temp_dir().join(format!("rubixdb_sql_binder_bench_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Builds a schema with `table_count` single-column tables
/// (`table_0`..`table_{N-1}`) and returns the engine/catalog/context
/// plus the *last* table's name — a point lookup for the
/// most-recently-created table is the worst case for `CatalogService::
/// list_tables`'s own documented full-scan-filtered-by-schema shape
/// (`CA.2`), so it is the fair, not favorable, case to measure.
fn build_catalog_with_tables(
    dir: &std::path::Path,
    table_count: usize,
) -> (Arc<LsmEngine>, CatalogService, BindContext, String) {
    let engine = Arc::new(
        LsmEngine::open(
            dir,
            bench_wal_config(),
            Default::default(),
            LsmConfig::default(),
        )
        .unwrap(),
    );
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;

    let mut last_name = String::new();
    for i in 0..table_count {
        let name = format!("table_{i}");
        catalog
            .create_table(
                schema_id,
                &name,
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
        last_name = name;
    }

    (
        engine,
        catalog,
        BindContext {
            database_id,
            default_schema_id: schema_id,
        },
        last_name,
    )
}

fn bench_bind_vs_catalog_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("sql_binder_vs_catalog_size");
    group.sample_size(20);
    for table_count in [100usize, 1_000, 5_000, 10_000] {
        let dir = temp_dir(&format!("size_{table_count}"));
        let (engine, catalog, ctx, last_table) = build_catalog_with_tables(&dir, table_count);
        let sql = format!("SELECT id FROM {last_table}");
        let limits = SqlLimits::default();
        let auth = AuthContext::admin("bench");
        let metrics = SqlMetrics::default();
        let stmt = parse_statement(&sql, &limits).unwrap();

        group.bench_with_input(
            BenchmarkId::from_parameter(table_count),
            &table_count,
            |b, _| {
                b.iter(|| {
                    let _ = std::hint::black_box(bind_statement(
                        &catalog, &ctx, &auth, &metrics, &limits, &stmt,
                    ));
                });
            },
        );

        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

criterion_group!(benches, bench_bind_vs_catalog_size);
criterion_main!(benches);
