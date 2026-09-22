//! `CatalogService` integration tests, against a real `LsmEngine` — the
//! same test fixtures/conventions `lsm::tests` already establishes
//! (`temp_dir`, `GroupCommit` sync mode), owned locally per this
//! project's own precedent of each test module keeping its own small
//! fixtures rather than sharing them across modules.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::service::ColumnDef;
use crate::catalog::{
    CatalogService, ConstraintKind, IndexKind, ObjectKind, Privilege, TableState,
};
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::wal::{SyncMode, WalConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_catalog_test_{tag}_{nanos}_{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn test_wal_config() -> WalConfig {
    WalConfig {
        sync_mode: SyncMode::GroupCommit {
            max_wait: Duration::from_millis(5),
            max_batch_bytes: 256 * 1024,
        },
        ..WalConfig::default()
    }
}

fn small_pool_config() -> BatchCoordinatorConfig {
    BatchCoordinatorConfig {
        queue_capacity: 256,
        max_queued_bytes: 8 * 1024 * 1024,
        submission_timeout: Duration::from_secs(5),
        shutdown_drain_bound: Duration::from_secs(10),
        await_retry_budget: Duration::from_secs(5),
        max_drain_per_batch: 4096,
    }
}

fn open(dir: &Path) -> Arc<LsmEngine> {
    Arc::new(
        LsmEngine::open(
            dir,
            test_wal_config(),
            small_pool_config(),
            LsmConfig::default(),
        )
        .unwrap(),
    )
}

fn some_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: 2,
            nullable: false,
            default_value: None,
        },
        ColumnDef {
            name: "name".to_string(),
            data_type: 7,
            nullable: true,
            default_value: None,
        },
        ColumnDef {
            name: "amount".to_string(),
            data_type: 3,
            nullable: false,
            default_value: Some(vec![0; 8]),
        },
    ]
}

// -----------------------------------------------------------------
// Bootstrap
// -----------------------------------------------------------------

#[test]
fn bootstrap_creates_default_database_and_public_schema() {
    let dir = temp_dir("bootstrap");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));

    catalog.bootstrap().unwrap();

    let databases = catalog.list_databases().unwrap();
    assert_eq!(databases.len(), 1);
    assert_eq!(databases[0].name, "default");

    let schemas = catalog.list_schemas(databases[0].database_id).unwrap();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "public");
    assert_eq!(schemas[0].database_id, databases[0].database_id);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bootstrap_is_idempotent_no_op_on_second_call() {
    let dir = temp_dir("bootstrap_idempotent");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));

    catalog.bootstrap().unwrap();
    let seq_after_first = engine.snapshot_seq();
    catalog.bootstrap().unwrap();
    let seq_after_second = engine.snapshot_seq();

    assert_eq!(
        seq_after_first, seq_after_second,
        "a second bootstrap must not issue any write_batch call"
    );
    assert_eq!(catalog.list_databases().unwrap().len(), 1);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_bootstrap_never_creates_duplicate_rows() {
    let dir = temp_dir("bootstrap_concurrent");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));

    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let catalog = Arc::clone(&catalog);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                catalog.bootstrap().unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(catalog.list_databases().unwrap().len(), 1);
    assert_eq!(catalog.list_schemas(1).unwrap().len(), 1);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// CREATE TABLE round trip
// -----------------------------------------------------------------

#[test]
fn create_table_round_trip_with_columns_and_default_primary_index() {
    let dir = temp_dir("create_table");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let schema_id = catalog.list_schemas(1).unwrap()[0].schema_id;

    let table_id = catalog
        .create_table(schema_id, "orders", &some_columns(), &[0])
        .unwrap();

    let table = catalog.get_table(table_id).unwrap().unwrap();
    assert_eq!(table.name, "orders");
    assert_eq!(table.schema_id, schema_id);
    assert_eq!(table.pk_ordinals, vec![0]);
    assert_eq!(table.state, TableState::Active);
    assert_eq!(table.schema_version, 1);

    let columns = catalog.get_columns(table_id).unwrap();
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0].name, "id");
    assert_eq!(columns[0].ordinal, 0);
    assert!(!columns[0].nullable);
    assert_eq!(columns[1].name, "name");
    assert!(columns[1].nullable);
    assert_eq!(columns[2].default_value, Some(vec![0; 8]));

    let indexes = catalog.list_indexes(table_id).unwrap();
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].kind, IndexKind::Primary);
    assert_eq!(indexes[0].column_ordinals, vec![0]);

    assert_eq!(
        catalog
            .get_table_by_name(schema_id, "orders")
            .unwrap()
            .unwrap()
            .table_id,
        table_id
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// Proves `create_table` uses exactly one `write_batch` call for its
/// multi-row mutation (table row + N column rows + PK index row + two
/// ID counters), not several independent `put`/`delete` calls — the
/// review directive's own explicit requirement. `write_batch` advances
/// the engine's sequence counter by exactly one per call regardless of
/// its member count (`RELATIONAL ADR AMENDMENT 001` AA.1); a
/// multi-`write_batch`-call implementation would advance it by more
/// than one.
#[test]
fn create_table_issues_exactly_one_write_batch_call() {
    let dir = temp_dir("create_table_one_batch");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let seq_before = engine.snapshot_seq();

    catalog
        .create_table(1, "orders", &some_columns(), &[0])
        .unwrap();

    let seq_after = engine.snapshot_seq();
    assert_eq!(
        seq_after,
        seq_before + 1,
        "create_table must be exactly one write_batch call"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_table_name_in_same_schema_is_rejected() {
    let dir = temp_dir("dup_table");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();

    catalog
        .create_table(1, "orders", &some_columns(), &[0])
        .unwrap();
    let err = catalog
        .create_table(1, "orders", &some_columns(), &[0])
        .unwrap_err();
    assert!(matches!(
        err,
        crate::catalog::CatalogError::AlreadyExists { .. }
    ));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn create_table_against_missing_schema_is_not_found() {
    let dir = temp_dir("missing_schema");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));

    let err = catalog
        .create_table(999, "orders", &some_columns(), &[0])
        .unwrap_err();
    assert!(matches!(err, crate::catalog::CatalogError::NotFound { .. }));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Invalid input
// -----------------------------------------------------------------

#[test]
fn create_table_rejects_empty_name_empty_columns_and_missing_pk() {
    let dir = temp_dir("invalid_input");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();

    assert!(matches!(
        catalog
            .create_table(1, "", &some_columns(), &[0])
            .unwrap_err(),
        crate::catalog::CatalogError::InvalidInput { .. }
    ));
    assert!(matches!(
        catalog.create_table(1, "t", &[], &[0]).unwrap_err(),
        crate::catalog::CatalogError::InvalidInput { .. }
    ));
    assert!(matches!(
        catalog
            .create_table(1, "t", &some_columns(), &[])
            .unwrap_err(),
        crate::catalog::CatalogError::InvalidInput { .. }
    ));
    assert!(matches!(
        catalog
            .create_table(1, "t", &some_columns(), &[99])
            .unwrap_err(),
        crate::catalog::CatalogError::InvalidInput { .. }
    ));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn create_table_rejects_nullable_primary_key_column() {
    let dir = temp_dir("nullable_pk");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();

    let columns = vec![ColumnDef {
        name: "id".to_string(),
        data_type: 2,
        nullable: true,
        default_value: None,
    }];
    let err = catalog.create_table(1, "t", &columns, &[0]).unwrap_err();
    assert!(matches!(
        err,
        crate::catalog::CatalogError::InvalidInput { .. }
    ));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn create_table_rejects_duplicate_column_names() {
    let dir = temp_dir("dup_columns");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();

    let columns = vec![
        ColumnDef {
            name: "id".to_string(),
            data_type: 2,
            nullable: false,
            default_value: None,
        },
        ColumnDef {
            name: "id".to_string(),
            data_type: 2,
            nullable: false,
            default_value: None,
        },
    ];
    let err = catalog.create_table(1, "t", &columns, &[0]).unwrap_err();
    assert!(matches!(
        err,
        crate::catalog::CatalogError::InvalidInput { .. }
    ));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// DROP TABLE — cascades to columns/indexes/constraints, isolates
// other tables (D2 namespace-isolation property, applied to catalog
// rows specifically)
// -----------------------------------------------------------------

#[test]
fn drop_table_removes_all_its_rows_and_leaves_other_tables_untouched() {
    let dir = temp_dir("drop_table");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();

    let table_a = catalog.create_table(1, "a", &some_columns(), &[0]).unwrap();
    let table_b = catalog.create_table(1, "b", &some_columns(), &[0]).unwrap();
    catalog
        .create_index(table_a, "a_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    catalog
        .create_constraint(
            table_a,
            "a_amount_check",
            ConstraintKind::Check,
            &[],
            Some("amount > 0".to_string()),
        )
        .unwrap();

    catalog.drop_table(table_a).unwrap();

    assert!(catalog.get_table(table_a).unwrap().is_none());
    assert!(catalog.get_columns(table_a).unwrap().is_empty());
    assert!(catalog.list_indexes(table_a).unwrap().is_empty());
    assert!(catalog.list_constraints(table_a).unwrap().is_empty());

    // Table B (and its own default PK index) must be completely
    // unaffected — the namespace-isolation property (D2), applied here
    // to catalog rows.
    let table_b_row = catalog.get_table(table_b).unwrap().unwrap();
    assert_eq!(table_b_row.name, "b");
    assert_eq!(catalog.get_columns(table_b).unwrap().len(), 3);
    assert_eq!(catalog.list_indexes(table_b).unwrap().len(), 1);

    // The dropped name is immediately available for reuse.
    let reused = catalog.create_table(1, "a", &some_columns(), &[0]).unwrap();
    assert_ne!(
        reused, table_a,
        "a fresh table_id must be allocated, never reusing a dropped one"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn drop_table_on_missing_table_is_not_found() {
    let dir = temp_dir("drop_missing");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    let err = catalog.drop_table(999).unwrap_err();
    assert!(matches!(err, crate::catalog::CatalogError::NotFound { .. }));
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn drop_schema_with_tables_is_rejected_without_tables_succeeds() {
    let dir = temp_dir("drop_schema");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let empty_schema = catalog.create_schema(1, "empty").unwrap();
    let busy_schema = catalog.create_schema(1, "busy").unwrap();
    catalog
        .create_table(busy_schema, "t", &some_columns(), &[0])
        .unwrap();

    assert!(matches!(
        catalog.drop_schema(busy_schema).unwrap_err(),
        crate::catalog::CatalogError::InvalidInput { .. }
    ));
    catalog.drop_schema(empty_schema).unwrap();
    assert!(catalog.get_schema(empty_schema).unwrap().is_none());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// CREATE INDEX
// -----------------------------------------------------------------

#[test]
fn create_index_unique_and_non_unique_round_trip_and_drop() {
    let dir = temp_dir("create_index");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let table_id = catalog.create_table(1, "t", &some_columns(), &[0]).unwrap();

    let unique_id = catalog
        .create_index(table_id, "t_name_uq", IndexKind::Unique, &[1])
        .unwrap();
    let nonunique_id = catalog
        .create_index(table_id, "t_amount_idx", IndexKind::NonUnique, &[2])
        .unwrap();

    let indexes = catalog.list_indexes(table_id).unwrap();
    assert_eq!(indexes.len(), 3); // PRIMARY + the two just created

    catalog.drop_index(unique_id).unwrap();
    assert_eq!(catalog.list_indexes(table_id).unwrap().len(), 2);
    assert!(catalog.get_index(nonunique_id).unwrap().is_some());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn create_index_rejects_primary_kind_and_out_of_range_column() {
    let dir = temp_dir("create_index_invalid");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let table_id = catalog.create_table(1, "t", &some_columns(), &[0]).unwrap();

    assert!(matches!(
        catalog
            .create_index(table_id, "bad", IndexKind::Primary, &[0])
            .unwrap_err(),
        crate::catalog::CatalogError::InvalidInput { .. }
    ));
    assert!(matches!(
        catalog
            .create_index(table_id, "bad2", IndexKind::Unique, &[99])
            .unwrap_err(),
        crate::catalog::CatalogError::InvalidInput { .. }
    ));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn drop_index_rejects_dropping_the_primary_index_directly() {
    let dir = temp_dir("drop_primary_index");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let table_id = catalog.create_table(1, "t", &some_columns(), &[0]).unwrap();
    let pk_index_id = catalog.list_indexes(table_id).unwrap()[0].index_id;

    let err = catalog.drop_index(pk_index_id).unwrap_err();
    assert!(matches!(
        err,
        crate::catalog::CatalogError::InvalidInput { .. }
    ));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Grants
// -----------------------------------------------------------------

#[test]
fn grant_revoke_round_trip_and_duplicate_rejected() {
    let dir = temp_dir("grants");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    let table_id = catalog.create_table(1, "t", &some_columns(), &[0]).unwrap();

    catalog
        .grant("reader-key", ObjectKind::Table, table_id, Privilege::Select)
        .unwrap();
    let grants = catalog.list_grants_for_principal("reader-key").unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].privilege, Privilege::Select);

    let err = catalog
        .grant("reader-key", ObjectKind::Table, table_id, Privilege::Select)
        .unwrap_err();
    assert!(matches!(
        err,
        crate::catalog::CatalogError::AlreadyExists { .. }
    ));

    let revoked = catalog
        .revoke("reader-key", ObjectKind::Table, table_id, Privilege::Select)
        .unwrap();
    assert_eq!(revoked, 1);
    assert!(catalog
        .list_grants_for_principal("reader-key")
        .unwrap()
        .is_empty());

    let revoked_again = catalog
        .revoke("reader-key", ObjectKind::Table, table_id, Privilege::Select)
        .unwrap();
    assert_eq!(
        revoked_again, 0,
        "revoking a non-existent grant is a no-op, not an error"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Namespace isolation from pre-existing flat-KV data (D2/D32)
// -----------------------------------------------------------------

#[test]
fn catalog_scans_never_observe_flat_kv_keys_outside_the_reserved_namespace() {
    let dir = temp_dir("namespace_isolation");
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    catalog.bootstrap().unwrap();
    catalog.create_table(1, "t", &some_columns(), &[0]).unwrap();

    // Simulate a pre-existing flat-KV client writing arbitrary keys
    // outside the reserved 0x00/0x01 namespace (D32) — must never be
    // visible to a catalog scan.
    engine
        .put(b"arbitrary-flat-kv-key", b"unrelated value")
        .unwrap();
    engine
        .put(&[0x02, 0x00, 0x00, 0x00, 0x01], b"also unrelated")
        .unwrap();

    let tables = catalog.list_tables(1).unwrap();
    assert_eq!(
        tables.len(),
        1,
        "flat-KV keys outside 0x00 must never appear in a catalog scan"
    );
    assert_eq!(tables[0].name, "t");

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Crash recovery: catalog rows inherit the certified engine's own
// durability/recovery path — write, restart, re-read.
// -----------------------------------------------------------------

#[test]
fn catalog_survives_a_real_engine_restart() {
    let dir = temp_dir("recovery");
    let (schema_id, table_id) = {
        let engine = open(&dir);
        let catalog = CatalogService::new(Arc::clone(&engine));
        catalog.bootstrap().unwrap();
        let schema_id = catalog.list_schemas(1).unwrap()[0].schema_id;
        let table_id = catalog
            .create_table(schema_id, "orders", &some_columns(), &[0])
            .unwrap();
        catalog
            .create_index(table_id, "orders_name_idx", IndexKind::NonUnique, &[1])
            .unwrap();
        catalog
            .grant("admin-key", ObjectKind::Table, table_id, Privilege::Ddl)
            .unwrap();
        engine.shutdown();
        (schema_id, table_id)
    };

    // Reopen — WAL replay must reconstruct every catalog row exactly.
    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));

    let table = catalog.get_table(table_id).unwrap().unwrap();
    assert_eq!(table.name, "orders");
    assert_eq!(table.schema_id, schema_id);
    assert_eq!(catalog.get_columns(table_id).unwrap().len(), 3);
    assert_eq!(catalog.list_indexes(table_id).unwrap().len(), 2); // PRIMARY + the secondary
    assert_eq!(
        catalog
            .list_grants_for_principal("admin-key")
            .unwrap()
            .len(),
        1
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// The review directive's own explicit requirement: IDs must be
/// durable and restart-safe, never a process-local counter that resets.
#[test]
fn table_id_allocation_continues_from_its_durable_value_after_restart() {
    let dir = temp_dir("id_persistence");
    let first_table_id = {
        let engine = open(&dir);
        let catalog = CatalogService::new(Arc::clone(&engine));
        catalog.bootstrap().unwrap();
        let id = catalog
            .create_table(1, "first", &some_columns(), &[0])
            .unwrap();
        engine.shutdown();
        id
    };

    let engine = open(&dir);
    let catalog = CatalogService::new(Arc::clone(&engine));
    let second_table_id = catalog
        .create_table(1, "second", &some_columns(), &[0])
        .unwrap();

    assert!(
        second_table_id > first_table_id,
        "table_id allocation must continue from its durable value after restart, never reset \
         (first={first_table_id}, second={second_table_id})"
    );

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Concurrency: concurrent CREATE TABLE must never collide on table_id
// -----------------------------------------------------------------

#[test]
fn concurrent_create_table_never_allocates_a_duplicate_table_id() {
    let dir = temp_dir("concurrent_create");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();

    const THREADS: usize = 16;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let catalog = Arc::clone(&catalog);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                catalog
                    .create_table(1, &format!("t{i}"), &some_columns(), &[0])
                    .unwrap()
            })
        })
        .collect();

    let mut table_ids: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    table_ids.sort_unstable();
    let mut deduped = table_ids.clone();
    deduped.dedup();
    assert_eq!(
        table_ids.len(),
        deduped.len(),
        "every concurrent CREATE TABLE must receive a distinct table_id: got {table_ids:?}"
    );
    assert_eq!(catalog.list_tables(1).unwrap().len(), THREADS);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_create_table_with_the_same_name_only_one_succeeds() {
    let dir = temp_dir("concurrent_duplicate_name");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();

    const THREADS: usize = 8;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let catalog = Arc::clone(&catalog);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                catalog.create_table(1, "same_name", &some_columns(), &[0])
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent CREATE TABLE with the same name may succeed"
    );
    assert_eq!(catalog.list_tables(1).unwrap().len(), 1);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Concurrent reads during a concurrent mutation must never observe a
// half-created table (AMENDMENT 001 AA.3's atomicity proof, exercised
// through the catalog layer specifically).
// -----------------------------------------------------------------

#[test]
fn concurrent_catalog_scan_never_observes_a_half_created_table() {
    let dir = temp_dir("concurrent_scan");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();

    const ROUNDS: usize = 25;
    let mut inconsistent_observations = 0usize;
    for i in 0..ROUNDS {
        let barrier = Arc::new(Barrier::new(2));
        let writer_catalog = Arc::clone(&catalog);
        let writer_barrier = Arc::clone(&barrier);
        let name = format!("round{i}");
        let writer_name = name.clone();
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            writer_catalog
                .create_table(1, &writer_name, &some_columns(), &[0])
                .unwrap()
        });

        barrier.wait();
        if let Some(table) = catalog.get_table_by_name(1, &name).unwrap() {
            // If the table is visible at all, its columns and PK index
            // must ALSO already be visible — a half-created table
            // (table row present, columns/index missing) would be
            // exactly the partial-batch defect AMENDMENT 001 forbids.
            let columns_ok = catalog.get_columns(table.table_id).unwrap().len() == 3;
            let index_ok = !catalog.list_indexes(table.table_id).unwrap().is_empty();
            if !columns_ok || !index_ok {
                inconsistent_observations += 1;
            }
        }
        writer.join().unwrap();
    }

    assert_eq!(inconsistent_observations, 0);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}
