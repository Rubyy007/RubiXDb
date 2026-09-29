//! Aggregation performance — item 55/56/57/58/59: fixture construction
//! always separated from the timed execution (`query_executor_bench.rs`'s
//! own precedent, reused verbatim). Measures `crate::exec::
//! execute_autocommit` end to end for `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`,
//! `GROUP BY` at low/high/composite cardinality, `HAVING`, and `GROUP
//! BY` combined with `ORDER BY`/`LIMIT` — plan-build cost is *not*
//! included (`query_planner_bench.rs` already measures that
//! separately).

use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::TYPE_TAG_INTEGER;
use rubixdb::relational::{RelationalValue, TransactionManager};
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_sql::auth::AuthContext;
use rubixdb_sql::bind::{bind_statement, BindContext};
use rubixdb_sql::exec::{execute_autocommit, CancellationToken, ExecLimits, ExecMetrics};
use rubixdb_sql::limits::SqlLimits;
use rubixdb_sql::metrics::SqlMetrics;
use rubixdb_sql::parse::parse_statement;
use rubixdb_sql::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};

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
    let path = std::env::temp_dir().join(format!("rubixdb_agg_bench_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

struct Env {
    engine: Arc<LsmEngine>,
    catalog: Arc<CatalogService>,
    store: Arc<TableStore>,
    builder: Arc<IndexBuilder>,
    txm: TransactionManager,
    ctx: BindContext,
}

/// `agg_t(id INTEGER PK, grp INTEGER NULL, grp2 INTEGER NULL, val
/// INTEGER NULL)`. `grp = i % group_cardinality` (`grp2 = i %
/// (group_cardinality * 2)`, for the composite-key benchmark's own
/// second dimension), `val = i`.
fn setup_env(dir: &std::path::Path, row_count: u32, group_cardinality: u32) -> Env {
    let engine = Arc::new(
        LsmEngine::open(
            dir,
            bench_wal_config(),
            Default::default(),
            LsmConfig::default(),
        )
        .unwrap(),
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let default_schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let table_id = catalog
        .create_table(
            default_schema_id,
            "agg_t",
            &[
                ColumnDef {
                    name: "id".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "grp".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "grp2".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "val".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));

    let gc = group_cardinality.max(1);
    let mut chunk = Vec::with_capacity(FIXTURE_CHUNK);
    for i in 0..row_count as i32 {
        chunk.push(vec![
            Some(RelationalValue::Integer(i)),
            Some(RelationalValue::Integer(i % gc as i32)),
            Some(RelationalValue::Integer(i % (gc as i32 * 2))),
            Some(RelationalValue::Integer(i)),
        ]);
        if chunk.len() == FIXTURE_CHUNK {
            store.put_rows(table_id, &chunk).unwrap();
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        store.put_rows(table_id, &chunk).unwrap();
    }

    Env {
        engine,
        catalog,
        store,
        builder,
        txm,
        ctx: BindContext {
            database_id,
            default_schema_id,
        },
    }
}

fn plan_for(env: &Env, sql: &str) -> Plan {
    let limits = SqlLimits::default();
    let stmt = parse_statement(sql, &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("bench");
    let bound = bind_statement(&env.catalog, &env.ctx, &auth, &metrics, &limits, &stmt).unwrap();
    build_plan(
        &bound,
        &env.catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap()
}

fn run(env: &Env, plan: &Plan) {
    let result = execute_autocommit(
        plan,
        &env.txm,
        &env.store,
        &env.builder,
        &[],
        &ExecLimits::default(),
        &ExecMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    std::hint::black_box(result);
}

/// Per-function cost of each aggregate with no `GROUP BY` (a single
/// implicit group) — compared against a plain `SELECT id FROM agg_t`
/// full scan to show the aggregate operator adds only the expected
/// per-row state-update cost on top of the scan itself (item 57).
fn bench_aggregate_functions(c: &mut Criterion) {
    let dir = temp_dir("functions");
    let env = setup_env(&dir, 50_000, 1);

    let mut group = c.benchmark_group("agg_functions_50k_rows_single_group");
    let cases: &[(&str, &str)] = &[
        ("baseline_seq_scan", "SELECT id FROM agg_t"),
        ("count_star", "SELECT COUNT(*) FROM agg_t"),
        ("count_val", "SELECT COUNT(val) FROM agg_t"),
        ("sum_val", "SELECT SUM(val) FROM agg_t"),
        ("avg_val", "SELECT AVG(val) FROM agg_t"),
        ("min_val", "SELECT MIN(val) FROM agg_t"),
        ("max_val", "SELECT MAX(val) FROM agg_t"),
        (
            "all_five_together",
            "SELECT COUNT(*), SUM(val), AVG(val), MIN(val), MAX(val) FROM agg_t",
        ),
    ];
    for (name, sql) in cases {
        let plan = plan_for(&env, sql);
        group.bench_function(*name, |b| {
            b.iter(|| run(&env, &plan));
        });
    }
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 56: `GROUP BY` at low vs. high cardinality, independent of row
/// count — a 50,000-row scan with 10 groups is a very different memory/
/// hashing shape than the same 50,000 rows with 10,000 groups.
fn bench_group_by_cardinality(c: &mut Criterion) {
    let mut group = c.benchmark_group("agg_group_by_cardinality_50k_rows");
    group.sample_size(20);
    for cardinality in [10u32, 1_000, 10_000] {
        let dir = temp_dir(&format!("cardinality_{cardinality}"));
        let env = setup_env(&dir, 50_000, cardinality);
        let plan = plan_for(
            &env,
            "SELECT grp, COUNT(*), SUM(val) FROM agg_t GROUP BY grp",
        );
        group.bench_with_input(
            BenchmarkId::new("group_by", cardinality),
            &cardinality,
            |b, _| {
                b.iter(|| run(&env, &plan));
            },
        );
        env.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 56: composite (two-column) `GROUP BY` key cost.
fn bench_composite_group_by(c: &mut Criterion) {
    let dir = temp_dir("composite");
    let env = setup_env(&dir, 50_000, 1_000);

    let mut group = c.benchmark_group("agg_composite_group_by_50k_rows");
    let single = plan_for(&env, "SELECT grp, COUNT(*) FROM agg_t GROUP BY grp");
    group.bench_function("single_column_key", |b| {
        b.iter(|| run(&env, &single));
    });
    let composite = plan_for(
        &env,
        "SELECT grp, grp2, COUNT(*) FROM agg_t GROUP BY grp, grp2",
    );
    group.bench_function("two_column_composite_key", |b| {
        b.iter(|| run(&env, &composite));
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 45/58: `HAVING`'s own added cost above plain `GROUP BY` — should
/// be small and independent of row count (it evaluates once per group,
/// never once per input row).
fn bench_having(c: &mut Criterion) {
    let dir = temp_dir("having");
    let env = setup_env(&dir, 50_000, 1_000);

    let mut group = c.benchmark_group("agg_having_50k_rows_1k_groups");
    let no_having = plan_for(&env, "SELECT grp, COUNT(*) FROM agg_t GROUP BY grp");
    group.bench_function("group_by_without_having", |b| {
        b.iter(|| run(&env, &no_having));
    });
    let with_having = plan_for(
        &env,
        "SELECT grp, COUNT(*) FROM agg_t GROUP BY grp HAVING COUNT(*) > 10",
    );
    group.bench_function("group_by_with_having", |b| {
        b.iter(|| run(&env, &with_having));
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 22/23/56: `GROUP BY` combined with `ORDER BY` (over the
/// aggregate result) and with `LIMIT`.
fn bench_group_by_order_and_limit(c: &mut Criterion) {
    let dir = temp_dir("order_limit");
    let env = setup_env(&dir, 50_000, 5_000);

    let mut group = c.benchmark_group("agg_group_by_order_limit_50k_rows_5k_groups");
    let plain = plan_for(&env, "SELECT grp, COUNT(*) FROM agg_t GROUP BY grp");
    group.bench_function("group_by_only", |b| {
        b.iter(|| run(&env, &plain));
    });
    let ordered = plan_for(
        &env,
        "SELECT grp, COUNT(*) FROM agg_t GROUP BY grp ORDER BY COUNT(*) DESC",
    );
    group.bench_function("group_by_plus_order_by_aggregate", |b| {
        b.iter(|| run(&env, &ordered));
    });
    let limited = plan_for(
        &env,
        "SELECT grp, COUNT(*) FROM agg_t GROUP BY grp ORDER BY COUNT(*) DESC LIMIT 10",
    );
    group.bench_function("group_by_plus_order_by_plus_limit_10", |b| {
        b.iter(|| run(&env, &limited));
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 56: rows/sec and groups/sec vary independently -- fixed group
/// cardinality (100) at increasing row counts vs. fixed row count
/// (50,000) at increasing group cardinality (the latter is
/// `bench_group_by_cardinality` above); this benchmark is the row-count
/// axis.
fn bench_vs_row_count_fixed_cardinality(c: &mut Criterion) {
    let mut group = c.benchmark_group("agg_vs_row_count_100_groups");
    group.sample_size(10);
    for row_count in [1_000u32, 10_000, 100_000] {
        let dir = temp_dir(&format!("rows_{row_count}"));
        let env = setup_env(&dir, row_count, 100);
        let plan = plan_for(
            &env,
            "SELECT grp, COUNT(*), SUM(val) FROM agg_t GROUP BY grp",
        );
        group.bench_with_input(
            BenchmarkId::new("group_by_count_sum", row_count),
            &row_count,
            |b, _| {
                b.iter(|| run(&env, &plan));
            },
        );
        env.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_aggregate_functions,
    bench_group_by_cardinality,
    bench_composite_group_by,
    bench_having,
    bench_group_by_order_and_limit,
    bench_vs_row_count_fixed_cardinality
);
criterion_main!(benches);
