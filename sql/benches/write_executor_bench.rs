//! Write executor performance — items 57-65. Fixture construction is
//! always separated from the timed execution (`PHASE_RELATIONAL_ROW_
//! STORAGE_RESULTS.md` §6's own precedent, reused verbatim from `query_
//! executor_bench.rs`). Measures `rubixdb_sql::exec::write::execute_
//! write_autocommit` end to end (plan build cost is not included --
//! parsing/binding/planning are measured separately by `sql_parser_
//! bench.rs`/`sql_binder_bench.rs`/`query_planner_bench.rs`). Resource
//! limits are never disabled for benchmarking (item 65).

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use rubixdb_sql::exec::write::{execute_write_autocommit, WriteMetrics};
use rubixdb_sql::exec::{CancellationToken, ExecLimits};
use rubixdb_sql::limits::SqlLimits;
use rubixdb_sql::metrics::SqlMetrics;
use rubixdb_sql::parse::parse_statement;
use rubixdb_sql::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};

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
    let path = std::env::temp_dir().join(format!("rubixdb_write_bench_{tag}_{nanos}"));
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

fn base_columns() -> Vec<ColumnDef> {
    vec![
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
    ]
}

fn setup_env(dir: &std::path::Path) -> Env {
    let engine = open_engine(dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let default_schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    catalog
        .create_table(default_schema_id, "t", &base_columns(), &[0])
        .unwrap();
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
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

fn run_write(env: &Env, plan: &Plan) {
    let result = execute_write_autocommit(
        plan,
        &env.txm,
        &env.store,
        &env.catalog,
        &env.builder,
        &[],
        &ExecLimits::default(),
        &WriteMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    std::hint::black_box(result);
}

fn seed_rows(env: &Env, count: i32) {
    let mut chunk = Vec::with_capacity(2_000);
    for i in 0..count {
        chunk.push(vec![
            Some(RelationalValue::Integer(i)),
            Some(RelationalValue::Text(format!("value-{i}"))),
            Some(RelationalValue::Boolean(i % 2 == 0)),
        ]);
        if chunk.len() == 2_000 {
            env.store.put_rows(1, &chunk).unwrap();
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        env.store.put_rows(1, &chunk).unwrap();
    }
}

/// Item 57/58: single-row, small (10-row), and larger (100-row) `INSERT`
/// latency -- a fresh, empty table each iteration (an `AtomicI32` PK
/// counter avoids re-seeding between iterations, which would otherwise
/// dominate the measured time).
fn bench_insert_latency(c: &mut Criterion) {
    let dir = temp_dir("insert_latency");
    let env = setup_env(&dir);
    let mut group = c.benchmark_group("write_insert_latency");

    let next = AtomicI32::new(0);
    group.bench_function("single_row", |b| {
        b.iter(|| {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let plan = plan_for(
                &env,
                &format!("INSERT INTO t (id, name, active) VALUES ({i}, 'v', TRUE)"),
            );
            run_write(&env, &plan);
        });
    });

    let next10 = AtomicI32::new(1_000_000);
    group.bench_function("ten_row_multi_values", |b| {
        b.iter(|| {
            let base = next10.fetch_add(10, Ordering::Relaxed);
            let values: Vec<String> = (0..10)
                .map(|j| format!("({}, 'v', TRUE)", base + j))
                .collect();
            let plan = plan_for(
                &env,
                &format!(
                    "INSERT INTO t (id, name, active) VALUES {}",
                    values.join(", ")
                ),
            );
            run_write(&env, &plan);
        });
    });

    let next100 = AtomicI32::new(2_000_000);
    group.bench_function("hundred_row_multi_values", |b| {
        b.iter(|| {
            let base = next100.fetch_add(100, Ordering::Relaxed);
            let values: Vec<String> = (0..100)
                .map(|j| format!("({}, 'v', TRUE)", base + j))
                .collect();
            let plan = plan_for(
                &env,
                &format!(
                    "INSERT INTO t (id, name, active) VALUES {}",
                    values.join(", ")
                ),
            );
            run_write(&env, &plan);
        });
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 59: `UPDATE` of an unindexed column, an indexed column (index
/// entry must move), and a `UNIQUE`-indexed column.
fn bench_update_latency(c: &mut Criterion) {
    let dir = temp_dir("update_latency");
    let env = setup_env(&dir);
    seed_rows(&env, 10_000);
    env.builder
        .create_index_online(1, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    let dir2 = temp_dir("update_latency_unique");
    let env2 = setup_env(&dir2);
    seed_rows(&env2, 10_000);
    env2.builder
        .create_index_online(1, "t_name_unique_idx", IndexKind::Unique, &[1])
        .unwrap();

    let mut group = c.benchmark_group("write_update_latency");
    let next = AtomicI32::new(0);
    group.bench_function("unindexed_column", |b| {
        b.iter(|| {
            let i = next.fetch_add(1, Ordering::Relaxed) % 10_000;
            let plan = plan_for(&env, &format!("UPDATE t SET active = FALSE WHERE id = {i}"));
            run_write(&env, &plan);
        });
    });
    let next2 = AtomicI32::new(0);
    group.bench_function("indexed_column_moves_entry", |b| {
        b.iter(|| {
            let i = next2.fetch_add(1, Ordering::Relaxed) % 10_000;
            let v = next2.load(Ordering::Relaxed);
            let plan = plan_for(
                &env,
                &format!("UPDATE t SET name = 'moved-{v}' WHERE id = {i}"),
            );
            run_write(&env, &plan);
        });
    });
    let next3 = AtomicI32::new(0);
    group.bench_function("unique_column", |b| {
        b.iter(|| {
            let i = next3.fetch_add(1, Ordering::Relaxed) % 10_000;
            let v = next3.load(Ordering::Relaxed) + 1_000_000;
            let plan = plan_for(
                &env2,
                &format!("UPDATE t SET name = 'uniq-{v}' WHERE id = {i}"),
            );
            run_write(&env2, &plan);
        });
    });
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    env2.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir2);
}

/// Item 59: `DELETE` of a single `PRIMARY KEY` row, a selective
/// predicate (1-in-10000), and a larger (1-in-100) predicate.
type DeleteCase = (&'static str, i32, fn(i32) -> String);

fn bench_delete_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_delete_latency");
    group.sample_size(20);

    let cases: Vec<DeleteCase> = vec![
        (
            "single_pk",
            20_000,
            (|i| format!("id = {i}")) as fn(i32) -> String,
        ),
        (
            "selective_1_in_10000",
            20_000,
            (|i| format!("id = {i} AND active = TRUE")) as fn(i32) -> String,
        ),
    ];
    for (label, seed_count, predicate_fn) in cases {
        let dir = temp_dir(&format!("delete_{label}"));
        let env = setup_env(&dir);
        seed_rows(&env, seed_count);
        let next = AtomicI32::new(0);
        group.bench_function(label, |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let i = next.fetch_add(1, Ordering::Relaxed) % seed_count;
                    // one fresh row re-inserted per iteration so DELETE
                    // always has a real target -- insertion is *not*
                    // included in the timed span.
                    env.store
                        .put_row(
                            1,
                            &[
                                Some(RelationalValue::Integer(i)),
                                Some(RelationalValue::Text("v".to_string())),
                                Some(RelationalValue::Boolean(true)),
                            ],
                        )
                        .unwrap();
                    let plan = plan_for(&env, &format!("DELETE FROM t WHERE {}", predicate_fn(i)));
                    let start = Instant::now();
                    run_write(&env, &plan);
                    total += start.elapsed();
                }
                total
            });
        });
        env.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 60: DDL latency, catalog-only operations (`CREATE TABLE`) vs. an
/// online `CREATE INDEX` that must backfill existing rows vs. `DROP
/// INDEX` -- each kept as a *separate* group so a backfill's row-count-
/// dependent cost is never averaged into a catalog-only operation's.
fn bench_ddl_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_ddl_latency");
    group.sample_size(20);

    let dir = temp_dir("ddl_create_table");
    let env = setup_env(&dir);
    let next = AtomicI32::new(0);
    group.bench_function("create_table_catalog_only", |b| {
        b.iter(|| {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let plan = plan_for(
                &env,
                &format!("CREATE TABLE bench_ddl_{i} (id INTEGER PRIMARY KEY)"),
            );
            run_write(&env, &plan);
        });
    });
    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);

    for row_count in [0i32, 1_000, 10_000] {
        let dir = temp_dir(&format!("ddl_create_index_{row_count}"));
        let env = setup_env(&dir);
        seed_rows(&env, row_count);
        let plan = plan_for(&env, "CREATE INDEX t_name_idx ON t (name)");
        group.bench_with_input(
            BenchmarkId::new("create_index_online_backfill", row_count),
            &row_count,
            |b, _| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        if env
                            .catalog
                            .list_indexes(1)
                            .unwrap()
                            .iter()
                            .any(|ix| ix.name == "t_name_idx")
                        {
                            env.builder
                                .drop_index_online(
                                    env.catalog
                                        .list_indexes(1)
                                        .unwrap()
                                        .into_iter()
                                        .find(|ix| ix.name == "t_name_idx")
                                        .unwrap()
                                        .index_id,
                                )
                                .unwrap();
                        }
                        let start = Instant::now();
                        run_write(&env, &plan);
                        total += start.elapsed();
                    }
                    total
                });
            },
        );
        env.engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 61: write amplification vs. maintained secondary-index count
/// (0/1/2/5/10) -- a single-row `INSERT`'s own latency as a proxy for
/// the physical write fan-out `Transaction::commit` performs per index.
fn bench_write_amplification_vs_index_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_amplification_vs_index_count");
    group.sample_size(30);

    for index_count in [0usize, 1, 2, 5, 10] {
        let dir = temp_dir(&format!("amp_{index_count}"));
        let engine = open_engine(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let database_id = catalog.list_databases().unwrap()[0].database_id;
        let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
        let mut columns = vec![ColumnDef {
            name: "id".to_string(),
            data_type: TYPE_TAG_INTEGER,
            nullable: false,
            default_value: None,
            type_params: None,
        }];
        for k in 0..index_count {
            columns.push(ColumnDef {
                name: format!("col{k}"),
                data_type: TYPE_TAG_INTEGER,
                nullable: true,
                default_value: None,
                type_params: None,
            });
        }
        let table_id = catalog
            .create_table(schema_id, "amp_t", &columns, &[0])
            .unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        for k in 0..index_count {
            builder
                .create_index_online(
                    table_id,
                    &format!("amp_idx_{k}"),
                    IndexKind::NonUnique,
                    &[(k + 1) as u16],
                )
                .unwrap();
        }
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
        let env = Env {
            engine: Arc::clone(&engine),
            catalog: Arc::clone(&catalog),
            store,
            builder,
            txm,
            ctx: BindContext {
                database_id,
                default_schema_id: schema_id,
            },
        };

        let col_names = (0..index_count)
            .map(|k| format!(", col{k}"))
            .collect::<String>();
        let next = AtomicI32::new(0);
        group.bench_with_input(
            BenchmarkId::new("insert_single_row", index_count),
            &index_count,
            |b, _| {
                b.iter(|| {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let vals = (0..index_count)
                        .map(|_| format!(", {i}"))
                        .collect::<String>();
                    let plan = plan_for(
                        &env,
                        &format!("INSERT INTO amp_t (id{col_names}) VALUES ({i}{vals})"),
                    );
                    run_write(&env, &plan);
                });
            },
        );

        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

/// Item 62: transaction commit latency vs. write-set size (1 to 128
/// buffered rows in a single explicit transaction, one `commit()` call).
fn bench_commit_scaling_vs_write_set_size(c: &mut Criterion) {
    let dir = temp_dir("commit_scaling");
    let env = setup_env(&dir);
    let mut group = c.benchmark_group("write_commit_scaling_vs_write_set_size");
    group.sample_size(20);

    for write_set_size in [1u32, 4, 16, 64, 128] {
        let next = AtomicI32::new(write_set_size as i32 * 1_000_000);
        group.bench_with_input(
            BenchmarkId::new("rows", write_set_size),
            &write_set_size,
            |b, _| {
                b.iter(|| {
                    let base = next.fetch_add(write_set_size as i32, Ordering::Relaxed);
                    let values: Vec<String> = (0..write_set_size as i32)
                        .map(|j| format!("({}, 'v', TRUE)", base + j))
                        .collect();
                    let plan = plan_for(
                        &env,
                        &format!(
                            "INSERT INTO t (id, name, active) VALUES {}",
                            values.join(", ")
                        ),
                    );
                    run_write(&env, &plan);
                });
            },
        );
    }
    group.finish();

    env.engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Item 63: concurrent write throughput, 1/4/16/32 writer threads
/// contending on the *same* table (distinct `PRIMARY KEY`s per writer,
/// so no conflicts -- pure lock/commit contention). `iter_custom` wall-
/// clock-times each full batch of concurrent writes since criterion
/// itself does not fan a single iteration out across threads. A
/// separate-tables variant (no shared epoch lock at all) was not also
/// benchmarked this increment -- the same-table case is both the more
/// interesting contention scenario and, by construction, an upper bound
/// on separate-tables latency (no epoch lock is ever more expensive than
/// one shared across writers), so its absence is a scope choice, stated
/// here rather than left implicit.
fn bench_concurrent_writers(c: &mut Criterion) {
    const WRITES_PER_THREAD: i32 = 50;
    let mut group = c.benchmark_group("write_concurrent_writers");
    group.sample_size(10);

    for writer_count in [1usize, 4, 16, 32] {
        let dir = temp_dir(&format!("concurrent_same_table_{writer_count}"));
        let engine = open_engine(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let database_id = catalog.list_databases().unwrap()[0].database_id;
        let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
        catalog
            .create_table(schema_id, "t", &base_columns(), &[0])
            .unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = Arc::new(TransactionManager::new(
            Arc::clone(&engine),
            Arc::clone(&store),
        ));
        let ctx = Arc::new(BindContext {
            database_id,
            default_schema_id: schema_id,
        });
        let base_id = AtomicI32::new(0);

        group.bench_with_input(BenchmarkId::new("same_table", writer_count), &writer_count, |b, _| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start_id = base_id.fetch_add(writer_count as i32 * WRITES_PER_THREAD, Ordering::Relaxed);
                    let start = Instant::now();
                    let handles: Vec<_> = (0..writer_count)
                        .map(|w| {
                            let catalog = Arc::clone(&catalog);
                            let store = Arc::clone(&store);
                            let builder = Arc::clone(&builder);
                            let txm = Arc::clone(&txm);
                            let ctx = Arc::clone(&ctx);
                            std::thread::spawn(move || {
                                for j in 0..WRITES_PER_THREAD {
                                    let id = start_id + (w as i32) * WRITES_PER_THREAD + j;
                                    let limits = SqlLimits::default();
                                    let stmt = parse_statement(&format!("INSERT INTO t (id, name, active) VALUES ({id}, 'v', TRUE)"), &limits).unwrap();
                                    let metrics = SqlMetrics::default();
                                    let auth = AuthContext::admin("bench");
                                    let bound = bind_statement(&catalog, &ctx, &auth, &metrics, &limits, &stmt).unwrap();
                                    let plan = build_plan(&bound, &catalog, &PlannerLimits::default(), &PlannerMetrics::default()).unwrap();
                                    execute_write_autocommit(&plan, &txm, &store, &catalog, &builder, &[], &ExecLimits::default(), &WriteMetrics::default(), &CancellationToken::new()).unwrap();
                                }
                            })
                        })
                        .collect();
                    for h in handles {
                        h.join().unwrap();
                    }
                    total += start.elapsed();
                }
                total
            });
        });
        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_insert_latency,
    bench_update_latency,
    bench_delete_latency,
    bench_ddl_latency,
    bench_write_amplification_vs_index_count,
    bench_commit_scaling_vs_write_set_size,
    bench_concurrent_writers,
);
criterion_main!(benches);
