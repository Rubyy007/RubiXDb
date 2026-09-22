//! `TableStore` integration tests, against a real `LsmEngine` +
//! `CatalogService` — `RELATIONAL ADR AMENDMENT 003`. Fixtures mirror
//! `catalog::tests`'s own local-per-module convention.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::service::ColumnDef;
use crate::catalog::CatalogService;
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::relational::value::{
    TYPE_TAG_BIGINT, TYPE_TAG_BOOLEAN, TYPE_TAG_DECIMAL, TYPE_TAG_INTEGER, TYPE_TAG_TEXT,
};
use crate::relational::{RelationalError, RelationalValue, TableStore};
use crate::wal::{SyncMode, WalConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_relational_test_{tag}_{nanos}_{n}"));
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

/// A table `t(id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN)`.
fn create_simple_table(catalog: &CatalogService, name: &str) -> u32 {
    let columns = vec![
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
    ];
    catalog.create_table(1, name, &columns, &[0]).unwrap()
}

/// A table with a composite primary key:
/// `t(tenant_id BIGINT, code TEXT, amount DECIMAL(10,2), PRIMARY KEY(tenant_id, code))`.
fn create_composite_pk_table(catalog: &CatalogService, name: &str) -> u32 {
    let columns = vec![
        ColumnDef {
            name: "tenant_id".to_string(),
            data_type: TYPE_TAG_BIGINT,
            nullable: false,
            default_value: None,
            type_params: None,
        },
        ColumnDef {
            name: "code".to_string(),
            data_type: TYPE_TAG_TEXT,
            nullable: false,
            default_value: None,
            type_params: None,
        },
        ColumnDef {
            name: "amount".to_string(),
            data_type: TYPE_TAG_DECIMAL,
            nullable: true,
            default_value: None,
            type_params: Some(vec![10, 2]),
        },
    ];
    catalog.create_table(1, name, &columns, &[0, 1]).unwrap()
}

// -----------------------------------------------------------------
// put_row / get_row round trip
// -----------------------------------------------------------------

#[test]
fn put_get_round_trip_simple_table() {
    let dir = temp_dir("put_get");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    store
        .put_row(
            table_id,
            &[
                Some(RelationalValue::Integer(42)),
                Some(RelationalValue::Text("hello".to_string())),
                Some(RelationalValue::Boolean(true)),
            ],
        )
        .unwrap();

    let row = store
        .get_row(table_id, &[RelationalValue::Integer(42)])
        .unwrap()
        .unwrap();
    assert_eq!(row[0], Some(RelationalValue::Integer(42)));
    assert_eq!(row[1], Some(RelationalValue::Text("hello".to_string())));
    assert_eq!(row[2], Some(RelationalValue::Boolean(true)));

    assert!(store
        .get_row(table_id, &[RelationalValue::Integer(999)])
        .unwrap()
        .is_none());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_get_round_trip_with_null_values() {
    let dir = temp_dir("put_get_null");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    store
        .put_row(table_id, &[Some(RelationalValue::Integer(1)), None, None])
        .unwrap();
    let row = store
        .get_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap()
        .unwrap();
    assert_eq!(row, vec![Some(RelationalValue::Integer(1)), None, None]);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_get_round_trip_composite_primary_key() {
    let dir = temp_dir("composite_pk");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_composite_pk_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    store
        .put_row(
            table_id,
            &[
                Some(RelationalValue::Bigint(7)),
                Some(RelationalValue::Text("abc".to_string())),
                Some(RelationalValue::Decimal(12345, 2)),
            ],
        )
        .unwrap();

    let row = store
        .get_row(
            table_id,
            &[
                RelationalValue::Bigint(7),
                RelationalValue::Text("abc".to_string()),
            ],
        )
        .unwrap()
        .unwrap();
    assert_eq!(row[0], Some(RelationalValue::Bigint(7)));
    assert_eq!(row[1], Some(RelationalValue::Text("abc".to_string())));
    assert_eq!(row[2], Some(RelationalValue::Decimal(12345, 2)));

    // A different tenant_id with the same code must be a distinct row.
    assert!(store
        .get_row(
            table_id,
            &[
                RelationalValue::Bigint(8),
                RelationalValue::Text("abc".to_string())
            ]
        )
        .unwrap()
        .is_none());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// put_row / delete_row atomicity (single write_batch call)
// -----------------------------------------------------------------

#[test]
fn put_row_issues_exactly_one_write_batch_call() {
    let dir = temp_dir("put_one_batch");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
    let seq_before = engine.snapshot_seq();

    store
        .put_row(table_id, &[Some(RelationalValue::Integer(1)), None, None])
        .unwrap();

    assert_eq!(engine.snapshot_seq(), seq_before + 1);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn delete_then_get_returns_none() {
    let dir = temp_dir("delete_get");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    store
        .put_row(table_id, &[Some(RelationalValue::Integer(1)), None, None])
        .unwrap();
    assert!(store
        .get_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap()
        .is_some());

    store
        .delete_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap();
    assert!(store
        .get_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap()
        .is_none());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// put_rows — genuine multi-row atomic write_batch visibility
// -----------------------------------------------------------------

#[test]
fn put_rows_multi_row_write_batch_all_visible_together() {
    let dir = temp_dir("put_rows");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
    let seq_before = engine.snapshot_seq();

    let rows: Vec<Vec<Option<RelationalValue>>> = (0..5)
        .map(|i| {
            vec![
                Some(RelationalValue::Integer(i)),
                Some(RelationalValue::Text(format!("row{i}"))),
                None,
            ]
        })
        .collect();
    store.put_rows(table_id, &rows).unwrap();

    // One write_batch call for all 5 rows.
    assert_eq!(engine.snapshot_seq(), seq_before + 1);
    for i in 0..5 {
        let row = store
            .get_row(table_id, &[RelationalValue::Integer(i)])
            .unwrap()
            .unwrap();
        assert_eq!(row[1], Some(RelationalValue::Text(format!("row{i}"))));
    }

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// scan_table + namespace isolation (D2)
// -----------------------------------------------------------------

#[test]
fn scan_table_returns_only_its_own_rows_never_another_tables() {
    let dir = temp_dir("scan_isolation");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_a = create_simple_table(&catalog, "a");
    let table_b = create_simple_table(&catalog, "b");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    for i in 0..10 {
        store
            .put_row(table_a, &[Some(RelationalValue::Integer(i)), None, None])
            .unwrap();
    }
    for i in 100..103 {
        store
            .put_row(table_b, &[Some(RelationalValue::Integer(i)), None, None])
            .unwrap();
    }

    let rows_a = store.scan_table(table_a).unwrap();
    assert_eq!(
        rows_a.len(),
        10,
        "table A's scan must see exactly its own 10 rows"
    );
    for (pk, _) in &rows_a {
        let RelationalValue::Integer(v) = pk[0] else {
            panic!("expected Integer")
        };
        assert!(
            (0..10).contains(&v),
            "table A's scan must never include table B's rows"
        );
    }

    let rows_b = store.scan_table(table_b).unwrap();
    assert_eq!(rows_b.len(), 3);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn scan_table_boundaries_at_min_and_max_and_neighboring_table_ids() {
    // Verifies the *physical* range boundaries directly (via table_id
    // arithmetic), not only via post-filtering: a table whose id is one
    // less/greater than the scanned table's must never appear.
    use crate::relational::key::{table_row_key, table_row_range};
    use std::ops::Bound;

    let table_id = 5u32;
    let (start, end) = table_row_range(table_id);

    let neighbor_below = table_row_key(table_id - 1, &[0xFF; 8]);
    let neighbor_above = table_row_key(table_id + 1, &[0x00; 8]);
    let inside_low = table_row_key(table_id, &[0x00; 0]);
    let inside_high = table_row_key(table_id, &[0xFF; 32]);

    let Bound::Included(start_bytes) = start else {
        panic!("expected Included")
    };
    let Bound::Excluded(end_bytes) = end else {
        panic!("expected Excluded")
    };

    assert!(
        neighbor_below < start_bytes,
        "the previous table_id's rows must sort entirely before this range"
    );
    assert!(
        neighbor_above >= end_bytes,
        "the next table_id's rows must sort entirely at/after this range's end"
    );
    assert!(start_bytes <= inside_low && inside_low < end_bytes);
    assert!(start_bytes <= inside_high && inside_high < end_bytes);

    // table_id = 0 and table_id = u32::MAX (min/max) must not panic and
    // must still produce a well-formed, non-overlapping range.
    let (min_start, min_end) = table_row_range(0);
    let (max_start, max_end) = table_row_range(u32::MAX);
    assert!(matches!(min_start, Bound::Included(_)));
    assert!(matches!(min_end, Bound::Excluded(_)));
    assert!(matches!(max_start, Bound::Included(_)));
    assert!(
        matches!(max_end, Bound::Unbounded),
        "table_id = u32::MAX has no next table_id to bound against"
    );
}

// -----------------------------------------------------------------
// Namespace isolation from catalog (0x00) and pre-existing flat-KV data
// -----------------------------------------------------------------

#[test]
fn table_scan_never_observes_catalog_rows_or_flat_kv_keys() {
    let dir = temp_dir("cross_namespace_isolation");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    store
        .put_row(table_id, &[Some(RelationalValue::Integer(1)), None, None])
        .unwrap();
    // A second table's catalog rows and a raw flat-KV key must never
    // leak into this table's scan.
    let _ = create_simple_table(&catalog, "u");
    engine.put(b"arbitrary-flat-kv-key", b"unrelated").unwrap();

    let rows = store.scan_table(table_id).unwrap();
    assert_eq!(rows.len(), 1);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Invalid input
// -----------------------------------------------------------------

#[test]
fn put_row_rejects_wrong_value_count() {
    let dir = temp_dir("wrong_count");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    let err = store
        .put_row(table_id, &[Some(RelationalValue::Integer(1))])
        .unwrap_err();
    assert!(matches!(err, RelationalError::InvalidInput { .. }));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_row_rejects_null_primary_key() {
    let dir = temp_dir("null_pk");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    let err = store.put_row(table_id, &[None, None, None]).unwrap_err();
    assert!(matches!(err, RelationalError::InvalidInput { .. }));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_row_rejects_null_for_not_null_column() {
    let dir = temp_dir("not_null_violation");
    let engine = open(&dir);
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
            name: "required".to_string(),
            data_type: TYPE_TAG_TEXT,
            nullable: false,
            default_value: None,
            type_params: None,
        },
    ];
    let table_id = catalog.create_table(1, "t", &columns, &[0]).unwrap();
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    let err = store
        .put_row(table_id, &[Some(RelationalValue::Integer(1)), None])
        .unwrap_err();
    assert!(matches!(err, RelationalError::InvalidInput { .. }));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_row_rejects_value_type_mismatch() {
    let dir = temp_dir("type_mismatch");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    // Column 0 ("id") is INTEGER; supplying a Text value must fail.
    let err = store
        .put_row(
            table_id,
            &[Some(RelationalValue::Text("nope".to_string())), None, None],
        )
        .unwrap_err();
    assert!(matches!(err, RelationalError::InvalidInput { .. }));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn put_row_rejects_decimal_exceeding_declared_precision() {
    let dir = temp_dir("decimal_precision");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_composite_pk_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    // amount is DECIMAL(10, 2): max magnitude is 10^10.
    let too_big = 10_000_000_000i128;
    let err = store
        .put_row(
            table_id,
            &[
                Some(RelationalValue::Bigint(1)),
                Some(RelationalValue::Text("x".to_string())),
                Some(RelationalValue::Decimal(too_big, 2)),
            ],
        )
        .unwrap_err();
    assert!(matches!(err, RelationalError::InvalidInput { .. }));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn get_row_against_missing_table_is_not_found() {
    let dir = temp_dir("missing_table");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

    let err = store
        .get_row(999, &[RelationalValue::Integer(1)])
        .unwrap_err();
    assert!(matches!(err, RelationalError::NotFound { .. }));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn oversized_row_is_rejected_before_any_write() {
    let dir = temp_dir("oversized_row");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
    let seq_before = engine.snapshot_seq();

    let huge_text = "x".repeat(crate::relational::MAX_ROW_VALUE_BYTES + 1);
    let err = store
        .put_row(
            table_id,
            &[
                Some(RelationalValue::Integer(1)),
                Some(RelationalValue::Text(huge_text)),
                None,
            ],
        )
        .unwrap_err();
    assert!(matches!(err, RelationalError::InvalidInput { .. }));
    // No WAL record written for the rejected attempt.
    assert_eq!(engine.snapshot_seq(), seq_before);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Persistence / restart
// -----------------------------------------------------------------

#[test]
fn row_survives_restart_insert_then_get() {
    let dir = temp_dir("persist_get");
    let table_id;
    {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        table_id = create_simple_table(&catalog, "t");
        let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
        store
            .put_row(
                table_id,
                &[
                    Some(RelationalValue::Integer(1)),
                    Some(RelationalValue::Text("v1".to_string())),
                    None,
                ],
            )
            .unwrap();
        engine.shutdown();
    }

    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
    let row = store
        .get_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap()
        .unwrap();
    assert_eq!(row[1], Some(RelationalValue::Text("v1".to_string())));

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rows_survive_restart_insert_then_scan() {
    let dir = temp_dir("persist_scan");
    let table_id;
    {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        table_id = create_simple_table(&catalog, "t");
        let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
        for i in 0..5 {
            store
                .put_row(table_id, &[Some(RelationalValue::Integer(i)), None, None])
                .unwrap();
        }
        engine.shutdown();
    }

    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
    assert_eq!(store.scan_table(table_id).unwrap().len(), 5);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn deletion_survives_restart() {
    let dir = temp_dir("persist_delete");
    let table_id;
    {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        table_id = create_simple_table(&catalog, "t");
        let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
        store
            .put_row(table_id, &[Some(RelationalValue::Integer(1)), None, None])
            .unwrap();
        store
            .put_row(table_id, &[Some(RelationalValue::Integer(2)), None, None])
            .unwrap();
        store
            .delete_row(table_id, &[RelationalValue::Integer(1)])
            .unwrap();
        engine.shutdown();
    }

    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));
    assert!(store
        .get_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap()
        .is_none());
    assert!(store
        .get_row(table_id, &[RelationalValue::Integer(2)])
        .unwrap()
        .is_some());

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Concurrency
// -----------------------------------------------------------------

#[test]
fn concurrent_put_row_distinct_keys_all_succeed_and_are_visible() {
    let dir = temp_dir("concurrent_put");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));

    const THREADS: i32 = 16;
    let barrier = Arc::new(Barrier::new(THREADS as usize));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                store
                    .put_row(table_id, &[Some(RelationalValue::Integer(i)), None, None])
                    .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(store.scan_table(table_id).unwrap().len(), THREADS as usize);
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_scan_during_writes_never_returns_a_torn_row() {
    let dir = temp_dir("concurrent_scan");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));

    let writer_store = Arc::clone(&store);
    let writer = thread::spawn(move || {
        for i in 0..200 {
            writer_store
                .put_row(
                    table_id,
                    &[
                        Some(RelationalValue::Integer(i)),
                        Some(RelationalValue::Text(format!("v{i}"))),
                        None,
                    ],
                )
                .unwrap();
        }
    });

    // Every scan, at any point during the writes, must decode cleanly —
    // every row it does see must have a fully-formed, non-corrupt value
    // (never a partial write, per write_batch's own atomicity).
    for _ in 0..50 {
        for (_, row) in store.scan_table(table_id).unwrap() {
            assert!(row[0].is_some(), "id (primary key) must always be present");
            // name may be None only if not yet decoded correctly; since
            // every put_row always sets it, a present row must have it.
            assert!(
                row[1].is_some(),
                "a fully-committed row must have its name field"
            );
        }
    }
    writer.join().unwrap();
    assert_eq!(store.scan_table(table_id).unwrap().len(), 200);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

/// A delete/read race: a reader repeatedly gets a key while a writer
/// deletes it — must observe either the live row or `None`, never an
/// error or a partially-decoded value (`RELATIONAL ADR AMENDMENT 001`
/// AA.3's atomicity, exercised through row storage).
#[test]
fn delete_read_race_never_produces_a_decode_error() {
    let dir = temp_dir("delete_read_race");
    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let table_id = create_simple_table(&catalog, "t");
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));

    for round in 0..30 {
        store
            .put_row(
                table_id,
                &[
                    Some(RelationalValue::Integer(1)),
                    Some(RelationalValue::Text(format!("r{round}"))),
                    None,
                ],
            )
            .unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let deleter_store = Arc::clone(&store);
        let deleter_barrier = Arc::clone(&barrier);
        let deleter = thread::spawn(move || {
            deleter_barrier.wait();
            deleter_store
                .delete_row(table_id, &[RelationalValue::Integer(1)])
                .unwrap();
        });

        barrier.wait();
        for _ in 0..20 {
            // Either Some(fully valid row) or None — never an Err.
            let _ = store
                .get_row(table_id, &[RelationalValue::Integer(1)])
                .unwrap();
        }
        deleter.join().unwrap();
    }

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Differential test: TableStore state matches an independent,
// serialized reference model.
// -----------------------------------------------------------------

mod differential {
    use super::*;
    use proptest::collection::vec as pvec;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    #[derive(Debug, Clone)]
    enum FuzzOp {
        Put { key: i32, text: String },
        Delete { key: i32 },
    }

    fn fuzz_op_strategy() -> impl Strategy<Value = FuzzOp> {
        prop_oneof![
            (0i32..20, ".*").prop_map(|(key, text)| FuzzOp::Put { key, text }),
            (0i32..20).prop_map(|key| FuzzOp::Delete { key }),
        ]
    }

    fn reference_apply(ops: &[FuzzOp]) -> BTreeMap<i32, String> {
        let mut state = BTreeMap::new();
        for op in ops {
            match op {
                FuzzOp::Put { key, text } => {
                    state.insert(*key, text.clone());
                }
                FuzzOp::Delete { key } => {
                    state.remove(key);
                }
            }
        }
        state
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn table_store_matches_independent_serialized_reference_model(
            ops in pvec(fuzz_op_strategy(), 1..40)
        ) {
            let dir = temp_dir("relational_differential");
            let engine = open(&dir);
            let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
            catalog.bootstrap().unwrap();
            let table_id = create_simple_table(&catalog, "t");
            let store = TableStore::new(Arc::clone(&engine), Arc::clone(&catalog));

            for op in &ops {
                match op {
                    FuzzOp::Put { key, text } => {
                        store
                            .put_row(table_id, &[Some(RelationalValue::Integer(*key)), Some(RelationalValue::Text(text.clone())), None])
                            .unwrap();
                    }
                    FuzzOp::Delete { key } => {
                        store.delete_row(table_id, &[RelationalValue::Integer(*key)]).unwrap();
                    }
                }
            }

            let expected = reference_apply(&ops);
            for key in 0..20 {
                let actual = store.get_row(table_id, &[RelationalValue::Integer(key)]).unwrap();
                let want = expected.get(&key).map(|text| {
                    vec![Some(RelationalValue::Integer(key)), Some(RelationalValue::Text(text.clone())), None]
                });
                prop_assert_eq!(actual, want, "mismatch at key {}", key);
            }

            engine.shutdown();
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
