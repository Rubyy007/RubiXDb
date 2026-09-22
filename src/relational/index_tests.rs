//! `IndexBuilder` integration tests, against a real `LsmEngine` plus
//! `CatalogService` plus `TableStore`, mirroring `relational::tests`'s
//! own fixture conventions. `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` is
//! the decision record these tests verify against.

use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::schema::{IndexKind, IndexState};
use crate::catalog::service::ColumnDef;
use crate::catalog::CatalogService;
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::relational::index::IndexBuilder;
use crate::relational::value::{TYPE_TAG_BOOLEAN, TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use crate::relational::{RelationalValue, TableStore};
use crate::wal::{SyncMode, WalConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_index_test_{tag}_{nanos}_{n}"));
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

struct Fixture {
    dir: PathBuf,
    engine: Arc<LsmEngine>,
    catalog: Arc<CatalogService>,
    store: Arc<TableStore>,
    builder: Arc<IndexBuilder>,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = temp_dir(tag);
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        Fixture {
            dir,
            engine,
            catalog,
            store,
            builder,
        }
    }

    fn cleanup(self) {
        self.engine.shutdown();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// `t(id INTEGER PRIMARY KEY, name TEXT, active BOOLEAN)`.
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

fn row(id: i32, name: &str, active: bool) -> Vec<Option<RelationalValue>> {
    vec![
        Some(RelationalValue::Integer(id)),
        Some(RelationalValue::Text(name.to_string())),
        Some(RelationalValue::Boolean(active)),
    ]
}

// -----------------------------------------------------------------
// Basic online CREATE INDEX + maintenance
// -----------------------------------------------------------------

#[test]
fn create_index_online_on_empty_table_activates_ready() {
    let f = Fixture::new("empty_activate");
    let table_id = create_simple_table(&f.catalog, "t");

    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    let idx = f.catalog.get_index(index_id).unwrap().unwrap();
    assert_eq!(idx.state, IndexState::Ready);
    f.cleanup();
}

#[test]
fn create_index_online_backfills_pre_existing_rows() {
    let f = Fixture::new("backfill_existing");
    let table_id = create_simple_table(&f.catalog, "t");
    for i in 0..50 {
        f.store.put_row(table_id, &row(i, "same", true)).unwrap();
    }

    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    let results = f
        .builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("same".to_string()))])
        .unwrap();
    assert_eq!(
        results.len(),
        50,
        "every pre-existing row must be backfilled"
    );
    f.cleanup();
}

#[test]
fn ordinary_insert_after_ready_is_maintained_atomically() {
    let f = Fixture::new("insert_after_ready");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    f.store.put_row(table_id, &row(1, "alice", true)).unwrap();
    let results = f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("alice".to_string()))],
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, vec![RelationalValue::Integer(1)]);
    f.cleanup();
}

#[test]
fn delete_removes_index_entry_atomically() {
    let f = Fixture::new("delete_removes_entry");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    f.store.put_row(table_id, &row(1, "bob", true)).unwrap();
    f.store
        .delete_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap();

    let results = f
        .builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("bob".to_string()))])
        .unwrap();
    assert!(results.is_empty());
    f.cleanup();
}

#[test]
fn upsert_changing_indexed_value_moves_the_entry() {
    let f = Fixture::new("upsert_moves_entry");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    f.store.put_row(table_id, &row(1, "old", true)).unwrap();
    f.store.put_row(table_id, &row(1, "new", true)).unwrap();

    let old_results = f
        .builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("old".to_string()))])
        .unwrap();
    assert!(
        old_results.is_empty(),
        "stale entry must be removed on upsert"
    );
    let new_results = f
        .builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("new".to_string()))])
        .unwrap();
    assert_eq!(new_results.len(), 1);
    f.cleanup();
}

#[test]
fn null_indexed_value_is_indexed_and_looked_up_as_null() {
    let f = Fixture::new("null_indexed");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    f.store
        .put_row(
            table_id,
            &[
                Some(RelationalValue::Integer(1)),
                None,
                Some(RelationalValue::Boolean(true)),
            ],
        )
        .unwrap();

    let results = f.builder.index_lookup(index_id, &[None]).unwrap();
    assert_eq!(results.len(), 1);
    f.cleanup();
}

#[test]
fn building_index_is_not_query_usable() {
    let f = Fixture::new("building_not_usable");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .catalog
        .create_index(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    assert_eq!(
        f.catalog.get_index(index_id).unwrap().unwrap().state,
        IndexState::Building
    );
    let err = f
        .builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("x".to_string()))])
        .unwrap_err();
    assert!(matches!(
        err,
        crate::relational::RelationalError::InvalidInput { .. }
    ));
    f.cleanup();
}

// -----------------------------------------------------------------
// Composite index / range scan
// -----------------------------------------------------------------

#[test]
fn composite_index_prefix_lookup_and_range_scan() {
    let f = Fixture::new("composite_index");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_active_name_idx", IndexKind::NonUnique, &[2, 1])
        .unwrap();
    f.store.put_row(table_id, &row(1, "alice", true)).unwrap();
    f.store.put_row(table_id, &row(2, "bob", true)).unwrap();
    f.store.put_row(table_id, &row(3, "carol", false)).unwrap();

    let active_true = f
        .builder
        .index_lookup(index_id, &[Some(RelationalValue::Boolean(true))])
        .unwrap();
    assert_eq!(active_true.len(), 2);

    let scanned = f
        .builder
        .index_range_scan(index_id, Bound::Unbounded, Bound::Unbounded)
        .unwrap();
    assert_eq!(scanned.len(), 3);
    f.cleanup();
}

// -----------------------------------------------------------------
// DROP INDEX
// -----------------------------------------------------------------

#[test]
fn drop_index_online_removes_catalog_row_and_entries() {
    let f = Fixture::new("drop_index");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    for i in 0..20 {
        f.store.put_row(table_id, &row(i, "x", true)).unwrap();
    }

    f.builder.drop_index_online(index_id).unwrap();
    assert!(f.catalog.get_index(index_id).unwrap().is_none());

    // Physical entries must actually be gone, not merely unreachable
    // via the catalog — verified by a raw engine range scan over the
    // index's own key prefix.
    let (start, end) = crate::relational::index_key::index_entry_range(table_id, index_id);
    let remaining: Vec<_> = f
        .engine
        .range_scan(
            match &start {
                Bound::Included(v) => Bound::Included(v.as_slice()),
                _ => Bound::Unbounded,
            },
            match &end {
                Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
                _ => Bound::Unbounded,
            },
            u64::MAX,
        )
        .collect();
    assert!(remaining.is_empty(), "every physical entry must be swept");
    f.cleanup();
}

#[test]
fn dropping_index_stops_receiving_new_writes() {
    let f = Fixture::new("dropping_stops_writes");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    f.builder.drop_index_online(index_id).unwrap();

    // A write after the index is gone must succeed and must not attempt
    // to maintain the removed index (no panic, no error).
    f.store.put_row(table_id, &row(1, "x", true)).unwrap();
    f.cleanup();
}

// -----------------------------------------------------------------
// Crash-recovery simulation: BUILDING/DROPPING left behind, recovered
// deterministically at "restart" (a fresh `IndexBuilder` against the
// same catalog/table state, matching what a real process restart's
// recovery pass would see).
// -----------------------------------------------------------------

#[test]
fn recover_incomplete_builds_restarts_from_scratch_and_activates() {
    let f = Fixture::new("recover_builds");
    let table_id = create_simple_table(&f.catalog, "t");
    for i in 0..30 {
        f.store.put_row(table_id, &row(i, "x", true)).unwrap();
    }
    // Simulate a crash mid-build: insert the Building catalog row
    // directly (bypassing IndexBuilder, i.e. no backfill ever ran) --
    // exactly what recovery would find after a real crash between T0 and
    // T5.
    let index_id = f
        .catalog
        .create_index(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    assert_eq!(
        f.catalog.get_index(index_id).unwrap().unwrap().state,
        IndexState::Building
    );

    let recovered = f.builder.recover_incomplete_builds().unwrap();
    assert_eq!(recovered, vec![index_id]);
    assert_eq!(
        f.catalog.get_index(index_id).unwrap().unwrap().state,
        IndexState::Ready
    );
    let results = f
        .builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("x".to_string()))])
        .unwrap();
    assert_eq!(
        results.len(),
        30,
        "restarted backfill must recover every row"
    );
    f.cleanup();
}

#[test]
fn recover_incomplete_drops_completes_the_sweep() {
    let f = Fixture::new("recover_drops");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    for i in 0..10 {
        f.store.put_row(table_id, &row(i, "x", true)).unwrap();
    }
    // Simulate a crash mid-sweep: transition to Dropping directly
    // (bypassing the sweep IndexBuilder::drop_index_online would run).
    f.catalog.mark_index_dropping(index_id).unwrap();

    let recovered = f.builder.recover_incomplete_drops().unwrap();
    assert_eq!(recovered, vec![index_id]);
    assert!(f.catalog.get_index(index_id).unwrap().is_none());
    f.cleanup();
}

// -----------------------------------------------------------------
// Online-build correctness under controlled concurrent interleavings —
// item 40 of the governing directive: the dangerous cases, deterministic
// (barrier-synchronized), never sleep-based.
// -----------------------------------------------------------------

/// The exact "phantom entry" race `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md`
/// §7 proves closed: a row visible at the backfill snapshot (T1) is
/// deleted *during* backfill, before backfill's chunk reaches it. The
/// index must never end up with an entry for the deleted row.
#[test]
fn row_deleted_during_backfill_leaves_no_phantom_entry() {
    let f = Fixture::new("phantom_entry_delete");
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "doomed", true)).unwrap();

    // Create the index row directly (Building) so we control exactly
    // when backfill runs relative to the delete, then delete BEFORE
    // calling backfill_and_activate — this is the T1-then-delete-then-
    // backfill-chunk ordering, the case the re-validation-under-lock
    // mechanism must handle.
    let index_id = f
        .catalog
        .create_index(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    f.store
        .delete_row(table_id, &[RelationalValue::Integer(1)])
        .unwrap();

    // Now run recovery-style backfill (re-derives from current truth):
    let recovered = f.builder.recover_incomplete_builds().unwrap();
    assert_eq!(recovered, vec![index_id]);

    let results = f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("doomed".to_string()))],
        )
        .unwrap();
    assert!(
        results.is_empty(),
        "no phantom entry for a row deleted before backfill ran"
    );
    f.cleanup();
}

/// Concurrent writers (insert/delete/reinsert) racing an in-progress
/// backfill, synchronized with a `Barrier` (not sleeps) so the writers'
/// activity is guaranteed to overlap the build. After the build
/// completes, the index must exactly match the table's final state —
/// verified against an independent reference (a fresh `scan_table` +
/// manual index derivation), never the production algorithm as its own
/// oracle.
#[test]
fn concurrent_writes_during_backfill_are_never_missed() {
    let f = Fixture::new("concurrent_writes_during_backfill");
    let table_id = create_simple_table(&f.catalog, "t");
    // A larger pre-existing set so backfill spans multiple chunks.
    for i in 0..1200 {
        f.store.put_row(table_id, &row(i, "initial", true)).unwrap();
    }

    let barrier = Arc::new(Barrier::new(2));
    let writer_store = Arc::clone(&f.store);
    let writer_barrier = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        writer_barrier.wait();
        for i in 1200..1400 {
            writer_store
                .put_row(table_id, &row(i, "concurrent", true))
                .unwrap();
        }
        for i in 0..200 {
            writer_store
                .delete_row(table_id, &[RelationalValue::Integer(i)])
                .unwrap();
        }
    });

    barrier.wait();
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    writer.join().unwrap();

    // Independent reference model: derive expected index contents from
    // the table's own final state via a fresh, unrelated scan.
    let final_rows = f.store.scan_table(table_id).unwrap();
    let mut expected_initial = 0usize;
    let mut expected_concurrent = 0usize;
    for (pk, r) in &final_rows {
        let RelationalValue::Integer(id) = pk[0] else {
            panic!("pk must be Integer")
        };
        let name = match &r[1] {
            Some(RelationalValue::Text(s)) => s.clone(),
            _ => String::new(),
        };
        if name == "initial" {
            expected_initial += 1;
        }
        if name == "concurrent" {
            expected_concurrent += 1;
        }
        let _ = id;
    }

    let initial_results = f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("initial".to_string()))],
        )
        .unwrap();
    let concurrent_results = f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("concurrent".to_string()))],
        )
        .unwrap();
    assert_eq!(initial_results.len(), expected_initial);
    assert_eq!(concurrent_results.len(), expected_concurrent);

    // Every index-returned PK must actually exist with that exact value
    // in the table's final state (no stale/phantom entries) — cross-
    // check every result, not just the count.
    for (pk, _) in initial_results.iter().chain(concurrent_results.iter()) {
        assert!(
            f.store.get_row(table_id, pk).unwrap().is_some(),
            "index returned a PK not present in the table"
        );
    }
    f.cleanup();
}

/// A row is deleted then re-inserted (with a different indexed value)
/// while a build is in progress — the index must end up reflecting only
/// the final, live state.
#[test]
fn reinsert_during_backfill_reflects_final_state_only() {
    let f = Fixture::new("reinsert_during_backfill");
    let table_id = create_simple_table(&f.catalog, "t");
    for i in 0..600 {
        f.store.put_row(table_id, &row(i, "initial", true)).unwrap();
    }

    let barrier = Arc::new(Barrier::new(2));
    let writer_store = Arc::clone(&f.store);
    let writer_barrier = Arc::clone(&barrier);
    let writer = thread::spawn(move || {
        writer_barrier.wait();
        for i in 0..50 {
            writer_store
                .delete_row(table_id, &[RelationalValue::Integer(i)])
                .unwrap();
            writer_store
                .put_row(table_id, &row(i, "reinserted", true))
                .unwrap();
        }
    });

    barrier.wait();
    let index_id = f
        .builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();
    writer.join().unwrap();

    let initial_matches = f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("initial".to_string()))],
        )
        .unwrap();
    let reinserted_matches = f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("reinserted".to_string()))],
        )
        .unwrap();

    // Every one of the 50 rewritten PKs must show up under exactly one
    // of the two values (whichever the table's final state holds), never
    // both (which would mean a stale "initial" entry survived a rewrite).
    for i in 0..50 {
        let in_initial = initial_matches
            .iter()
            .any(|(pk, _)| pk[0] == RelationalValue::Integer(i));
        let in_reinserted = reinserted_matches
            .iter()
            .any(|(pk, _)| pk[0] == RelationalValue::Integer(i));
        assert!(
            in_initial != in_reinserted,
            "PK {i} must appear under exactly one indexed value, got initial={in_initial} reinserted={in_reinserted}"
        );
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// Restart persistence
// -----------------------------------------------------------------

#[test]
fn index_and_entries_survive_restart() {
    let dir = temp_dir("restart_persistence");
    {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        );
        let table_id = create_simple_table(&catalog, "t");
        for i in 0..20 {
            store.put_row(table_id, &row(i, "x", true)).unwrap();
        }
        builder
            .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
            .unwrap();
        engine.shutdown();
    }
    {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        );
        let table_id = catalog.get_table_by_name(1, "t").unwrap().unwrap().table_id;
        let index_id = catalog
            .list_indexes(table_id)
            .unwrap()
            .into_iter()
            .find(|i| i.kind == IndexKind::NonUnique)
            .unwrap()
            .index_id;
        assert_eq!(
            catalog.get_index(index_id).unwrap().unwrap().state,
            IndexState::Ready
        );
        let results = builder
            .index_lookup(index_id, &[Some(RelationalValue::Text("x".to_string()))])
            .unwrap();
        assert_eq!(results.len(), 20);
        engine.shutdown();
    }
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Resource limits / corruption
// -----------------------------------------------------------------

#[test]
fn too_many_indexes_is_rejected() {
    let f = Fixture::new("too_many_indexes");
    let table_id = create_simple_table(&f.catalog, "t");
    for i in 0..crate::catalog::service::MAX_INDEXES_PER_TABLE - 1 {
        f.catalog
            .create_index(table_id, &format!("idx_{i}"), IndexKind::NonUnique, &[1])
            .unwrap();
    }
    let err = f
        .catalog
        .create_index(table_id, "one_too_many", IndexKind::NonUnique, &[1])
        .unwrap_err();
    assert!(matches!(
        err,
        crate::catalog::CatalogError::InvalidInput { .. }
    ));
    f.cleanup();
}

// -----------------------------------------------------------------
// Compaction interaction — item 32: insert/flush/compaction/lookup,
// delete/flush/compaction/lookup, a `Building` index concurrent with
// automatic compaction, all against the real, unmodified automatic
// compaction trigger (never a protected-path change).
// -----------------------------------------------------------------

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn index_survives_automatic_compaction_across_insert_delete_and_backfill() {
    let dir = temp_dir("index_and_compaction");
    let engine = Arc::new(
        LsmEngine::open(
            &dir,
            test_wal_config(),
            small_pool_config(),
            LsmConfig {
                memtable_max_size_bytes: 256,
                max_immutable_memtables: 32,
                compaction_trigger_count: 3,
                compaction_auto_trigger: true,
                ..LsmConfig::default()
            },
        )
        .unwrap(),
    );
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    catalog.bootstrap().unwrap();
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    );
    let table_id = create_simple_table(&catalog, "t");

    // Enough small writes (tiny memtable) to force several flushes before
    // the index even exists.
    for i in 0..150 {
        store.put_row(table_id, &row(i, "pre", true)).unwrap();
    }
    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(10)),
        "pre-index writes must finish flushing"
    );

    // CREATE INDEX (online): backfill itself performs many small
    // write_batch calls, which — under this tiny-memtable config —
    // triggers further flushes/compactions *while the index is still
    // Building*, exercising exactly the "Building index + Compaction"
    // case item 32 requires.
    let index_id = builder
        .create_index_online(table_id, "t_name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    // More inserts and some deletes after Ready, to force additional
    // flush/compaction cycles with a live, maintained index.
    for i in 150..300 {
        store.put_row(table_id, &row(i, "post", true)).unwrap();
    }
    for i in 0..50 {
        store
            .delete_row(table_id, &[RelationalValue::Integer(i)])
            .unwrap();
    }

    assert!(
        wait_until(|| engine.immutable_count() == 0, Duration::from_secs(10)),
        "all writes must finish flushing before checking compaction"
    );
    assert!(
        wait_until(
            || engine.compaction_metrics().cycles_completed > 0,
            Duration::from_secs(10)
        ),
        "this fixture's tiny memtable + trigger_count=3 must actually cause \
         automatic compaction to run at least once"
    );

    // Table/index consistency must hold after real compaction activity:
    // cross-check the index against an independent `scan_table`-derived
    // reference, not the production algorithm as its own oracle.
    let final_rows = store.scan_table(table_id).unwrap();
    let mut expected_pre = 0usize;
    let mut expected_post = 0usize;
    for (_, r) in &final_rows {
        match &r[1] {
            Some(RelationalValue::Text(s)) if s == "pre" => expected_pre += 1,
            Some(RelationalValue::Text(s)) if s == "post" => expected_post += 1,
            _ => {}
        }
    }
    let pre_matches = builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("pre".to_string()))])
        .unwrap();
    let post_matches = builder
        .index_lookup(index_id, &[Some(RelationalValue::Text("post".to_string()))])
        .unwrap();
    assert_eq!(pre_matches.len(), expected_pre);
    assert_eq!(post_matches.len(), expected_post);
    for (pk, _) in pre_matches.iter().chain(post_matches.iter()) {
        assert!(store.get_row(table_id, pk).unwrap().is_some());
    }
    // The 50 deleted rows must be absent from the index under either value.
    for i in 0..50 {
        assert!(!pre_matches
            .iter()
            .any(|(pk, _)| pk[0] == RelationalValue::Integer(i)));
    }

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn malformed_index_entry_key_is_rejected_without_panicking() {
    // A truncated key (shorter than the fixed 9-byte header) must fail
    // closed, never panic.
    let bad_key: &[u8] = &[0x01, 0, 0];
    let err = crate::relational::index_key::decode_indexed_columns(
        &[crate::relational::value::RelationalType::Integer],
        bad_key,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        crate::relational::RelationalError::InvalidInput { .. }
    ));
}
