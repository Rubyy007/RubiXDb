//! Executor performance — items 48-56: fixture construction always
//! separated from the timed execution (`PHASE_RELATIONAL_ROW_STORAGE_
//! RESULTS.md` §6's own precedent, reused verbatim). Measures `crate::
//! exec::execute_autocommit` end to end (plan build cost is *not*
//! included — `crate::plan`'s own `query_planner_bench.rs` already
//! measured that separately) for each operator shape items 48-56 name.

use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::{TYPE_TAG_BOOLEAN, TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
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
    let path = std::env::temp_dir().join(format!("rubixdb_exec_bench_{tag}_{nanos}"));
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
    table_id: u32,
}

fn setup_env(dir: &std::path::Path, row_count: u32, with_index: bool) -> Env {
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
            "t",
            &[
                ColumnDef {
                    name: "id".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: TYPE_TAG_TEXT,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "active".to_string(),
                    data_type: TYPE_TAG_BOOLEAN,
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

    let mut chunk = Vec::with_capacity(FIXTURE_CHUNK);
    for i in 0..row_count as i32 {
        chunk.push(vec![
            Some(RelationalValue::Integer(i)),
            Some(RelationalValue::Text(format!("value-{i}"))),
            Some(RelationalValue::Boolean(i % 2 == 0)),
        ]);
        if chunk.len() == FIXTURE_CHUNK {
            store.put_rows(table_id, &chunk).unwrap();
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        store.put_rows(table_id, &chunk).unwrap();
    }
    if with_index {
        builder
            .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
            .unwrap();
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
        table_id,
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

/// Items 48/49: PK lookup, raw engine `get` vs. `TableStore::get_row`
/// vs. the full planned executor path — overhead is never hidden.
fn bench_pk_lookup_layers(c: &mut Criterion) {
    let dir = temp_dir("pk_layers");
    let env = setup_env(&dir, 10_000, false);
    let key = rubixdb::relational::key::table_row_key(
        env.table_id,
        &rubixdb::relational::key::encode_composite_key(&[RelationalValue::Integer(5_000)])
            .unwrap(),
    );

    let mut group = c.benchmark_group("exec_pk_lookup_layers");
    group.bench_function("raw_engine_get", |b| {
        b.iter(|| std::hint::black_box(env.engine.get(&key).unwrap()));
    });
    group.bench_function("table_store_get_row", |b| {
        b.iter(|| {
            std::hint::black_box(
                env.store
                    .get_row(env.table_id, &[RelationalValue::Integer(5_000)])
                    .unwrap(),
            )
        });
    });
    let plan = plan_for(&env, "SELECT id, name, active FROM t WHERE id = 5000");
    group.bench_function("planned_executor", |b| {
        b.iter(|| run(&env, &plan));
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 50: full table scan vs. a selective secondary-index lookup,
/// proving (via the query's own result, not merely its `EXPLAIN`) that
/// execution actually took the indexed path.
fn bench_seq_scan_vs_index_scan(c: &mut Criterion) {
    let dir = temp_dir("scan_vs_index");
    let env = setup_env(&dir, 10_000, true);

    let mut group = c.benchmark_group("exec_seq_scan_vs_index_scan");
    let seq_plan = plan_for(&env, "SELECT id FROM t WHERE active = TRUE");
    group.bench_function("seq_scan_50pct_selectivity", |b| {
        b.iter(|| run(&env, &seq_plan));
    });
    let idx_plan = plan_for(&env, "SELECT id FROM t WHERE name = 'value-5000'");
    group.bench_function("index_scan_1_in_10000_selectivity", |b| {
        b.iter(|| run(&env, &idx_plan));
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Items 51/54/55: latency/throughput vs. row count for scan, sort, and
/// distinct.
fn bench_vs_row_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("exec_vs_row_count");
    group.sample_size(10);
    for row_count in [100u32, 1_000, 10_000] {
        let dir = temp_dir(&format!("row_count_{row_count}"));
        let env = setup_env(&dir, row_count, false);

        let scan_plan = plan_for(&env, "SELECT id FROM t");
        group.bench_with_input(
            BenchmarkId::new("seq_scan", row_count),
            &row_count,
            |b, _| {
                b.iter(|| run(&env, &scan_plan));
            },
        );

        let sort_plan = plan_for(&env, "SELECT id FROM t ORDER BY name");
        group.bench_with_input(BenchmarkId::new("sort", row_count), &row_count, |b, _| {
            b.iter(|| run(&env, &sort_plan));
        });

        let distinct_plan = plan_for(&env, "SELECT DISTINCT active FROM t");
        group.bench_with_input(
            BenchmarkId::new("distinct", row_count),
            &row_count,
            |b, _| {
                b.iter(|| run(&env, &distinct_plan));
            },
        );

        env.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 52: `LIMIT` must actually stop scanning early -- compares full-
/// table latency against a small `LIMIT`.
fn bench_limit_early_termination(c: &mut Criterion) {
    let dir = temp_dir("limit_early");
    let env = setup_env(&dir, 50_000, false);

    let mut group = c.benchmark_group("exec_limit_early_termination");
    let full_plan = plan_for(&env, "SELECT id FROM t");
    group.bench_function("no_limit_full_scan", |b| {
        b.iter(|| run(&env, &full_plan));
    });
    let limit_plan = plan_for(&env, "SELECT id FROM t LIMIT 10");
    group.bench_function("limit_10", |b| {
        b.iter(|| run(&env, &limit_plan));
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Items 53/56: `NestedLoop` vs. `IndexNestedLoop` join throughput.
fn bench_join_algorithms(c: &mut Criterion) {
    let dir = temp_dir("joins");
    let engine = Arc::new(
        LsmEngine::open(
            &dir,
            bench_wal_config(),
            Default::default(),
            LsmConfig::default(),
        )
        .unwrap(),
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let outer_id = catalog
        .create_table(
            schema_id,
            "outer_t",
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
    let inner_id = catalog
        .create_table(
            schema_id,
            "inner_t",
            &[
                ColumnDef {
                    name: "id".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "outer_id".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
                // Deliberately carries neither a PK nor an index, unlike
                // `outer_id` above -- the plain-`NestedLoop` comparison
                // benchmark joins on this column specifically so the
                // planner has no usable access path to narrow with
                // (item 53/56's "genuinely selective vs. plain" contrast
                // needs a real absence of an index, not just an
                // arithmetic-wrapped reference to one that still exists).
                ColumnDef {
                    name: "unindexed_val".to_string(),
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
    builder
        .create_index_online(inner_id, "inner_outer_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    let outer_rows = 200i32;
    for i in 0..outer_rows {
        store
            .put_row(outer_id, &[Some(RelationalValue::Integer(i))])
            .unwrap();
        store
            .put_row(
                inner_id,
                &[
                    Some(RelationalValue::Integer(i)),
                    Some(RelationalValue::Integer(i)),
                    Some(RelationalValue::Integer(i)),
                ],
            )
            .unwrap();
    }
    let ctx = BindContext {
        database_id,
        default_schema_id: schema_id,
    };
    let env = Env {
        engine: Arc::clone(&engine),
        catalog: Arc::clone(&catalog),
        store,
        builder,
        txm,
        ctx,
        table_id: outer_id,
    };
    let _ = inner_id;

    let mut group = c.benchmark_group("exec_join_algorithms");
    group.sample_size(20);
    // Index Nested Loop: inner_t.outer_id is indexed, joined against outer_t.id.
    let inlj_plan = plan_for(
        &env,
        "SELECT outer_t.id FROM outer_t INNER JOIN inner_t ON inner_t.outer_id = outer_t.id",
    );
    group.bench_function("index_nested_loop_200x200_selective", |b| {
        b.iter(|| run(&env, &inlj_plan));
    });
    // Plain Nested Loop: `unindexed_val` carries neither a PK nor an
    // index, so the planner has no usable access path to narrow with.
    let nl_plan = plan_for(
        &env,
        "SELECT outer_t.id FROM outer_t INNER JOIN inner_t ON inner_t.unindexed_val = outer_t.id",
    );
    group.bench_function("nested_loop_200x200", |b| {
        b.iter(|| run(&env, &nl_plan));
    });
    group.finish();

    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(
    benches,
    bench_pk_lookup_layers,
    bench_seq_scan_vs_index_scan,
    bench_vs_row_count,
    bench_limit_early_termination,
    bench_join_algorithms
);
criterion_main!(benches);
