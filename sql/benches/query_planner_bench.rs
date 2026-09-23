//! Planner performance — items 32-35: end-to-end plan-build latency
//! across statement shapes, catalog-resolution cost vs. catalog size
//! (reusing `sql_binder_bench.rs`'s own methodology and its own "no
//! automatic caching" finding one layer up), optimizer complexity vs.
//! predicate/join count. Fixture setup (catalog/table/index
//! construction) is always outside the timed closure — `PHASE_
//! RELATIONAL_ROW_STORAGE_RESULTS.md` §6's own "separate fixture setup
//! from the measured operation" precedent. **No query is ever
//! executed** — every benchmark here measures `build_plan` alone.

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
use rubixdb::wal::{SyncMode, WalConfig};
use rubixdb_sql::auth::AuthContext;
use rubixdb_sql::bind::{bind_statement, BindContext};
use rubixdb_sql::limits::SqlLimits;
use rubixdb_sql::metrics::SqlMetrics;
use rubixdb_sql::parse::parse_statement;
use rubixdb_sql::plan::{build_plan, PlannerLimits, PlannerMetrics};

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
    let path = std::env::temp_dir().join(format!("rubixdb_planner_bench_{tag}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

struct Env {
    engine: Arc<LsmEngine>,
    catalog: CatalogService,
    ctx: BindContext,
}

fn setup_env(dir: &std::path::Path) -> Env {
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
    let default_schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    catalog
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
    catalog
        .create_table(
            default_schema_id,
            "orders",
            &[
                ColumnDef {
                    name: "id".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "customer".to_string(),
                    data_type: TYPE_TAG_TEXT,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "amount".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
                ColumnDef {
                    name: "t_id".to_string(),
                    data_type: TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: None,
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();
    Env {
        engine,
        catalog,
        ctx: BindContext {
            database_id,
            default_schema_id,
        },
    }
}

fn plan_one(env: &Env, sql: &str) {
    let limits = SqlLimits::default();
    let stmt = parse_statement(sql, &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("bench");
    let bound = bind_statement(&env.catalog, &env.ctx, &auth, &metrics, &limits, &stmt).unwrap();
    let plimits = PlannerLimits::default();
    let pmetrics = PlannerMetrics::default();
    let plan = build_plan(&bound, &env.catalog, &plimits, &pmetrics).unwrap();
    std::hint::black_box(plan);
}

/// Item 32: plan-build latency across representative statement shapes.
fn bench_plan_shapes(c: &mut Criterion) {
    let dir = temp_dir("shapes");
    let env = setup_env(&dir);
    let name_idx_dir = temp_dir("shapes_idx");
    let idx_env = setup_env(&name_idx_dir);
    {
        let catalog = Arc::new(CatalogService::new(Arc::clone(&idx_env.engine)));
        let store = Arc::new(TableStore::new(
            Arc::clone(&idx_env.engine),
            Arc::clone(&catalog),
        ));
        let builder = IndexBuilder::new(
            Arc::clone(&idx_env.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        );
        let table_id = idx_env
            .catalog
            .get_table_by_name(idx_env.ctx.default_schema_id, "t")
            .unwrap()
            .unwrap()
            .table_id;
        builder
            .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
            .unwrap();
    }

    let mut group = c.benchmark_group("query_planner_shapes");
    let cases: &[(&str, &Env, &str)] = &[
        ("point_lookup_pk", &env, "SELECT name FROM t WHERE id = 5"),
        ("table_scan", &env, "SELECT id FROM t WHERE active = TRUE"),
        ("indexed_lookup", &idx_env, "SELECT id FROM t WHERE name = 'alice'"),
        (
            "range_query",
            &idx_env,
            "SELECT id FROM t WHERE name >= 'a' AND name < 'm'",
        ),
        (
            "join",
            &env,
            "SELECT t.id, orders.customer FROM t INNER JOIN orders ON orders.t_id = t.id",
        ),
        (
            "left_join",
            &env,
            "SELECT t.id, orders.customer FROM t LEFT JOIN orders ON orders.t_id = t.id WHERE orders.amount = 100",
        ),
        (
            "large_projection",
            &env,
            "SELECT t.id, t.name, t.active, orders.id, orders.customer, orders.amount, orders.t_id FROM t INNER JOIN orders ON orders.t_id = t.id",
        ),
    ];

    for (name, env, sql) in cases {
        group.bench_function(*name, |b| {
            b.iter(|| plan_one(env, sql));
        });
    }
    group.finish();

    idx_env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&name_idx_dir);
    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 33: catalog-resolution cost vs. catalog size — the planner's
/// own `catalog.get_table`/`list_indexes` calls (`crate::plan::access`),
/// not the binder's (already measured by `sql_binder_bench.rs`). No
/// caching is added by this increment; this bench makes the resulting
/// per-call cost visible, matching that file's own finding one layer up.
fn bench_plan_vs_catalog_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("query_planner_vs_catalog_size");
    group.sample_size(20);
    for table_count in [100usize, 1_000, 5_000, 10_000] {
        let dir = temp_dir(&format!("catalog_size_{table_count}"));
        let env = setup_env(&dir);
        for i in 0..table_count {
            env.catalog
                .create_table(
                    env.ctx.default_schema_id,
                    &format!("filler_{i}"),
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
                b.iter(|| plan_one(&env, "SELECT name FROM t WHERE id = 5"));
            },
        );

        env.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 34: optimizer complexity vs. predicate-conjunct count — must
/// scale linearly, never exponentially (the exact class of bug this
/// increment found and fixed in `crate::bind::expr::bind_shared`,
/// `PHASE_RELATIONAL_QUERY_PLANNER_INCREMENT8_RESULTS.md` §6 has the
/// full account).
fn bench_plan_vs_predicate_count(c: &mut Criterion) {
    let dir = temp_dir("predicates");
    let env = setup_env(&dir);
    // 500 is not reachable: `SqlLimits::max_expression_depth` (128, the
    // Increment 6 stack-overflow-guard boundary — unrelated to this
    // increment) rejects a flat `AND` chain that long before it ever
    // reaches the planner; 120 is the largest round number still inside
    // that bound for this benchmark's own one-predicate-per-`AND`-level
    // shape.
    let mut group = c.benchmark_group("query_planner_vs_predicate_count");
    for count in [1u32, 10, 50, 100, 120] {
        let mut sql = "SELECT id FROM t WHERE active = TRUE".to_string();
        for i in 0..count {
            sql.push_str(&format!(" AND id = {i}"));
        }
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| plan_one(&env, &sql));
        });
    }
    group.finish();
    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 34 (continued): optimizer complexity vs. join count.
fn bench_plan_vs_join_count(c: &mut Criterion) {
    let dir = temp_dir("joins");
    let env = setup_env(&dir);
    for i in 0..=8 {
        env.catalog
            .create_table(
                env.ctx.default_schema_id,
                &format!("j{i}"),
                &[
                    ColumnDef {
                        name: "id".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: false,
                        default_value: None,
                        type_params: None,
                    },
                    ColumnDef {
                        name: "link".to_string(),
                        data_type: TYPE_TAG_INTEGER,
                        nullable: true,
                        default_value: None,
                        type_params: None,
                    },
                ],
                &[0],
            )
            .unwrap();
    }

    let mut group = c.benchmark_group("query_planner_vs_join_count");
    for join_count in [1u32, 2, 4, 8] {
        let mut sql = "SELECT j0.id FROM j0".to_string();
        for i in 1..=join_count {
            sql.push_str(&format!(" INNER JOIN j{i} ON j{i}.link = j{}.id", i - 1));
        }
        group.bench_with_input(
            BenchmarkId::from_parameter(join_count),
            &join_count,
            |b, _| {
                b.iter(|| plan_one(&env, &sql));
            },
        );
    }
    group.finish();
    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(
    benches,
    bench_plan_shapes,
    bench_plan_vs_catalog_size,
    bench_plan_vs_predicate_count,
    bench_plan_vs_join_count
);
criterion_main!(benches);
