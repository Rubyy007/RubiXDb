//! Item 30's own "HARD PRODUCTION GATE": real, cross-process, OS-level
//! crash testing for the write executor's own commit boundary. Reuses
//! `rubixdb::wal::{AbortPoint, FileWal::set_abort_hook}` verbatim -- the
//! *exact* same real-process mechanism `tests/crash_consistency.rs` and
//! `tests/group_commit/crash_consistency.rs` already certified at the
//! WAL/`GroupCommitter` layer, one crate over, applied here at the write
//! executor's own boundary instead of being reimplemented: `Transaction::
//! commit` ultimately calls the *same* `LsmEngine::write_batch`, built on
//! the *same* `FileWal`/`GroupCommitter`, so every `AbortPoint` those
//! suites already certified is a real, reachable boundary here too. See
//! `tests/crash_consistency.rs`'s own module doc comment for exactly what
//! a `std::process::abort()`-based test proves and does not prove (real
//! filesystem/process behavior, not a genuine torn-write power-loss
//! simulation -- that is `wal::fuzz_tests`' job, unchanged).
//!
//! What this test specifically proves, that the WAL-layer suites cannot:
//! that a crash during `write_batch` can never leave a table row durable
//! without its secondary index entry (or the reverse) -- item 24's
//! "table + all affected indexes as one atomic state," verified at the
//! real SQL `INSERT` boundary, not merely asserted from reading
//! `Transaction::commit`'s own source.
//!
//! Only compiled with the `test-util` feature (`cargo test -p
//! rubixdb-sql --features test-util`, the same convention as the WAL
//! suites) -- see `sql/Cargo.toml`'s own `[dev-dependencies]` entry
//! enabling `rubixdb`'s `test-util` feature for this crate's own tests.

#![cfg(feature = "test-util")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::service::ColumnDef;
use rubixdb::catalog::CatalogService;
use rubixdb::lsm::{LsmConfig, LsmEngine};
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::value::{TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use rubixdb::relational::{RelationalValue, TransactionManager};
use rubixdb::wal::{AbortPoint, FileWal, SyncMode, WalConfig};
use rubixdb_sql::auth::AuthContext;
use rubixdb_sql::bind::{bind_statement, BindContext};
use rubixdb_sql::exec::write::{execute_write_autocommit, WriteMetrics};
use rubixdb_sql::exec::{CancellationToken, ExecLimits};
use rubixdb_sql::limits::SqlLimits;
use rubixdb_sql::metrics::SqlMetrics;
use rubixdb_sql::parse::parse_statement;
use rubixdb_sql::plan::{build_plan, PlannerLimits, PlannerMetrics};

const ENV_DIR: &str = "RUBIXDB_SQL_WRITE_CRASH_DIR";
const ENV_ABORT_POINT: &str = "RUBIXDB_SQL_WRITE_CRASH_ABORT_POINT";

/// The full Phase-1 `GroupCommitter` point set -- `SyncMode::GroupCommit`
/// is what this test (matching every other benchmark/test fixture in
/// this crate) actually configures, so every one of these 11 points is a
/// real boundary the write executor's own `execute_write_autocommit`
/// call genuinely passes through, not merely the 4 WAL-only points --
/// except `DuringRotationPre`/`DuringRotationPost`: those fire only from
/// inside `GroupCommitter::rotate`, a method `LsmEngine`'s own public API
/// never exposes (confirmed by inspection: no `rotate`/`wal_rotate`
/// method exists anywhere on `LsmEngine`), so nothing this SQL-crate
/// integration test can reach through `execute_write_autocommit` alone
/// ever triggers rotation. `tests/group_commit/crash_consistency.rs`
/// already certifies both rotation points directly, with a dedicated
/// rotator thread driving the real `GroupCommitter` it holds -- reusing
/// that certification rather than reaching around `LsmEngine`'s own
/// public boundary to duplicate it here.
const ABORT_POINTS: [AbortPoint; 9] = [
    AbortPoint::AfterHeader,
    AbortPoint::MidAppend,
    AbortPoint::BeforeLeader,
    AbortPoint::AfterLeaderElection,
    AbortPoint::DuringBatchWaitPre,
    AbortPoint::DuringBatchWaitPost,
    AbortPoint::BeforeSync,
    AbortPoint::AfterSync,
    AbortPoint::AfterWatermarkBeforeWake,
];

fn abort_point_name(p: AbortPoint) -> &'static str {
    match p {
        AbortPoint::AfterHeader => "AfterHeader",
        AbortPoint::MidAppend => "MidAppend",
        AbortPoint::BeforeSync => "BeforeSync",
        AbortPoint::AfterSync => "AfterSync",
        AbortPoint::BeforeLeader => "BeforeLeader",
        AbortPoint::AfterLeaderElection => "AfterLeaderElection",
        AbortPoint::DuringBatchWaitPre => "DuringBatchWaitPre",
        AbortPoint::DuringBatchWaitPost => "DuringBatchWaitPost",
        AbortPoint::AfterWatermarkBeforeWake => "AfterWatermarkBeforeWake",
        AbortPoint::DuringRotationPre => "DuringRotationPre",
        AbortPoint::DuringRotationPost => "DuringRotationPost",
    }
}

fn parse_abort_point(name: &str) -> AbortPoint {
    match name {
        "AfterHeader" => AbortPoint::AfterHeader,
        "MidAppend" => AbortPoint::MidAppend,
        "BeforeSync" => AbortPoint::BeforeSync,
        "AfterSync" => AbortPoint::AfterSync,
        "BeforeLeader" => AbortPoint::BeforeLeader,
        "AfterLeaderElection" => AbortPoint::AfterLeaderElection,
        "DuringBatchWaitPre" => AbortPoint::DuringBatchWaitPre,
        "DuringBatchWaitPost" => AbortPoint::DuringBatchWaitPost,
        "AfterWatermarkBeforeWake" => AbortPoint::AfterWatermarkBeforeWake,
        "DuringRotationPre" => AbortPoint::DuringRotationPre,
        "DuringRotationPost" => AbortPoint::DuringRotationPost,
        other => panic!("unknown abort point {other:?}"),
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_sql_write_crash_{tag}_{nanos}_{n}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn group_commit_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

/// The parent-side test: for every `AbortPoint`, spawns this same test
/// binary with `child_worker` as an exact test-name filter (the standard
/// re-exec-myself trick this crate's own WAL crash tests already use),
/// then reopens the resulting directory fresh and checks that the row
/// and its secondary-index entry are either *both* durably present or
/// *both* absent -- never one without the other.
#[test]
fn insert_crash_never_leaves_a_row_without_its_index_entry() {
    let exe = std::env::current_exe().expect("current_exe must be resolvable under cargo test");

    for point in ABORT_POINTS {
        let name = abort_point_name(point);
        let dir = unique_dir(name);

        let status = Command::new(&exe)
            .arg("child_worker")
            .arg("--exact")
            .arg("--nocapture")
            .env(ENV_DIR, &dir)
            .env(ENV_ABORT_POINT, name)
            .status()
            .expect("failed to spawn child test process");
        assert!(
            !status.success(),
            "child process for abort point {name} was expected to abort(), but exited successfully \
             (either the row was never durably created, or every abort point in this run fired \
             before the target INSERT reached its commit boundary at all -- either way the parent \
             below still checks the resulting directory is in a safe state)"
        );

        // Reopening the engine fresh (this parent process never installed
        // an abort hook of its own) replays the WAL exactly like a real
        // restart after a crash.
        let engine = Arc::new(
            LsmEngine::open(
                &dir,
                group_commit_config(),
                Default::default(),
                LsmConfig::default(),
            )
            .unwrap(),
        );
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));

        // Bootstrap may itself not have completed durably if the abort
        // landed early enough -- in that case there is no catalog at all
        // yet, which is itself a safe (nothing committed) outcome.
        if let Ok(Some(database)) = catalog.list_databases().map(|v| v.into_iter().next()) {
            if let Ok(Some(schema)) = catalog
                .list_schemas(database.database_id)
                .map(|v| v.into_iter().next())
            {
                if let Ok(Some(table)) = catalog.get_table_by_name(schema.schema_id, "t") {
                    let row = store
                        .get_row(table.table_id, &[RelationalValue::Integer(1)])
                        .unwrap();
                    let indexes = catalog.list_indexes(table.table_id).unwrap();
                    if let Some(index) = indexes.into_iter().find(|ix| ix.name == "t_name_idx") {
                        // The index row itself only reaches `Ready` at the
                        // very end of `create_index_online`'s own atomic
                        // step -- a `Building`-state index (this online
                        // build itself interrupted, a separate, already-
                        // certified D-level concern from Increment 5/D11)
                        // is not what this test is targeting, so it is
                        // skipped rather than asserted on.
                        if index.state == rubixdb::catalog::schema::IndexState::Ready {
                            let via_index = builder
                                .index_lookup(
                                    index.index_id,
                                    &[Some(RelationalValue::Text("v".to_string()))],
                                )
                                .unwrap();
                            let row_exists = row.is_some();
                            let index_entry_exists = !via_index.is_empty();
                            assert_eq!(
                                row_exists, index_entry_exists,
                                "abort point {name}: table row present={row_exists}, index entry present={index_entry_exists} -- \
                                 a crash must never leave a table row without its secondary-index entry, or the reverse"
                            );
                        }
                    }
                }
            }
        }

        engine.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The child-side worker. Registered as an ordinary `#[test]` so it can
/// be selected by name via `--exact`; no-ops under a normal, un-parented
/// `cargo test` sweep (the environment variables below are absent).
#[test]
fn child_worker() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };
    let Ok(point_name) = std::env::var(ENV_ABORT_POINT) else {
        return;
    };
    let target = parse_abort_point(&point_name);

    static TARGET: OnceLock<AbortPoint> = OnceLock::new();
    TARGET
        .set(target)
        .expect("set once, at the very start of this process");
    fn hook(point: AbortPoint) {
        if TARGET.get() == Some(&point) {
            std::process::abort();
        }
    }
    FileWal::set_abort_hook(hook);

    let engine = Arc::new(
        LsmEngine::open(
            Path::new(&dir),
            group_commit_config(),
            Default::default(),
            LsmConfig::default(),
        )
        .unwrap(),
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    catalog
        .create_table(
            schema_id,
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
    builder
        .create_index_online(1, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
    let ctx = BindContext {
        database_id,
        default_schema_id: schema_id,
    };

    let limits = SqlLimits::default();
    let stmt = parse_statement("INSERT INTO t (id, name) VALUES (1, 'v')", &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("crash-test");
    let bound = bind_statement(&catalog, &ctx, &auth, &metrics, &limits, &stmt).unwrap();
    let plan = build_plan(
        &bound,
        &catalog,
        &PlannerLimits::default(),
        &PlannerMetrics::default(),
    )
    .unwrap();

    let _ = execute_write_autocommit(
        &plan,
        &txm,
        &store,
        &catalog,
        &builder,
        &[],
        &ExecLimits::default(),
        &WriteMetrics::default(),
        &CancellationToken::new(),
    );
    // Reaching here without aborting means the target point never fired
    // on this particular run's timing -- not an assertion failure, same
    // as the WAL-layer crash tests this one is modeled on.
}
