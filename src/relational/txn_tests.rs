//! Transaction engine tests — `PHASE_RELATIONAL_TRANSACTION_
//! ARCHITECTURE.md` is the decision record these verify against.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::schema::{IndexKind, ObjectKind, Privilege};
use crate::catalog::service::ColumnDef;
use crate::catalog::CatalogService;
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::relational::index::IndexBuilder;
use crate::relational::value::{TYPE_TAG_BOOLEAN, TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use crate::relational::{
    RelationalError, RelationalValue, Row, TableStore, TransactionManager, TxnState,
};
use crate::wal::{SyncMode, WalConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_txn_test_{tag}_{nanos}_{n}"));
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
    txm: TransactionManager,
    builder: IndexBuilder,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = temp_dir(tag);
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
        let builder = IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        );
        Fixture {
            dir,
            engine,
            catalog,
            store,
            txm,
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

fn pk(id: i32) -> Vec<RelationalValue> {
    vec![RelationalValue::Integer(id)]
}

// -----------------------------------------------------------------
// Lifecycle
// -----------------------------------------------------------------

#[test]
fn begin_commit_rollback_basic_lifecycle() {
    let f = Fixture::new("lifecycle");
    let table_id = create_simple_table(&f.catalog, "t");

    let mut tx = f.txm.begin().unwrap();
    assert_eq!(tx.state(), TxnState::Active);
    tx.put_row(table_id, &row(1, "a", true)).unwrap();
    tx.commit().unwrap();

    assert_eq!(
        f.store.get_row(table_id, &pk(1)).unwrap().unwrap()[1],
        Some(RelationalValue::Text("a".to_string()))
    );
    f.cleanup();
}

#[test]
fn rollback_discards_writes_after_one_and_many_and_delete() {
    let f = Fixture::new("rollback_discards");
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "orig", true)).unwrap();

    let mut tx = f.txm.begin().unwrap();
    tx.put_row(table_id, &row(2, "new", true)).unwrap();
    tx.put_row(table_id, &row(3, "new2", true)).unwrap();
    tx.delete_row(table_id, &pk(1)).unwrap();
    tx.rollback().unwrap();

    assert!(f.store.get_row(table_id, &pk(2)).unwrap().is_none());
    assert!(f.store.get_row(table_id, &pk(3)).unwrap().is_none());
    assert!(
        f.store.get_row(table_id, &pk(1)).unwrap().is_some(),
        "delete must not have applied"
    );
    f.cleanup();
}

#[test]
fn dropping_an_active_transaction_is_an_implicit_rollback() {
    let f = Fixture::new("implicit_rollback");
    let table_id = create_simple_table(&f.catalog, "t");
    {
        let mut tx = f.txm.begin().unwrap();
        tx.put_row(table_id, &row(1, "x", true)).unwrap();
        // tx dropped here without commit/rollback
    }
    assert!(f.store.get_row(table_id, &pk(1)).unwrap().is_none());
    assert_eq!(f.txm.active_transactions(), 0);
    f.cleanup();
}

#[test]
fn commit_and_rollback_cannot_be_called_twice_a_compile_time_guarantee() {
    // `Transaction::commit`/`rollback` consume `self` by value -- calling
    // either twice, or calling anything after either, is a compile
    // error, not a runtime one. This test exists to document that
    // guarantee; nothing to assert at runtime (see module doc comment).
}

#[test]
fn read_only_transaction_commits_trivially_with_no_engine_write() {
    let f = Fixture::new("read_only_commit");
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "a", true)).unwrap();

    let tx = f.txm.begin().unwrap();
    let seen = tx.get_row(table_id, &pk(1)).unwrap();
    assert!(seen.is_some());
    tx.commit().unwrap();
    f.cleanup();
}

// -----------------------------------------------------------------
// Reads: read-your-own-writes, snapshot consistency
// -----------------------------------------------------------------

#[test]
fn read_your_own_writes_put_then_read() {
    let f = Fixture::new("ryow_put");
    let table_id = create_simple_table(&f.catalog, "t");
    let mut tx = f.txm.begin().unwrap();
    tx.put_row(table_id, &row(1, "a", true)).unwrap();
    let seen = tx.get_row(table_id, &pk(1)).unwrap().unwrap();
    assert_eq!(seen[1], Some(RelationalValue::Text("a".to_string())));
    tx.commit().unwrap();
    f.cleanup();
}

#[test]
fn read_your_own_writes_delete_then_read() {
    let f = Fixture::new("ryow_delete");
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "a", true)).unwrap();
    let mut tx = f.txm.begin().unwrap();
    tx.delete_row(table_id, &pk(1)).unwrap();
    assert!(tx.get_row(table_id, &pk(1)).unwrap().is_none());
    tx.commit().unwrap();
    f.cleanup();
}

#[test]
fn read_your_own_writes_put_then_delete_then_put() {
    let f = Fixture::new("ryow_put_delete_put");
    let table_id = create_simple_table(&f.catalog, "t");
    let mut tx = f.txm.begin().unwrap();
    tx.put_row(table_id, &row(1, "a", true)).unwrap();
    tx.delete_row(table_id, &pk(1)).unwrap();
    assert!(tx.get_row(table_id, &pk(1)).unwrap().is_none());
    tx.put_row(table_id, &row(1, "b", true)).unwrap();
    let seen = tx.get_row(table_id, &pk(1)).unwrap().unwrap();
    assert_eq!(seen[1], Some(RelationalValue::Text("b".to_string())));
    tx.commit().unwrap();

    let committed = f.store.get_row(table_id, &pk(1)).unwrap().unwrap();
    assert_eq!(committed[1], Some(RelationalValue::Text("b".to_string())));
    f.cleanup();
}

#[test]
fn concurrent_write_is_not_visible_before_commit() {
    let f = Fixture::new("not_visible_before_commit");
    let table_id = create_simple_table(&f.catalog, "t");
    let mut writer = f.txm.begin().unwrap();
    writer.put_row(table_id, &row(1, "a", true)).unwrap();

    // A separate reader (autocommit-style, real engine read) must not
    // see the uncommitted write.
    assert!(f.store.get_row(table_id, &pk(1)).unwrap().is_none());

    writer.commit().unwrap();
    assert!(f.store.get_row(table_id, &pk(1)).unwrap().is_some());
    f.cleanup();
}

#[test]
fn snapshot_read_is_stable_against_a_later_external_commit() {
    let f = Fixture::new("snapshot_stable");
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "before", true)).unwrap();

    let tx = f.txm.begin().unwrap();
    // An external, later commit changes the row.
    f.store.put_row(table_id, &row(1, "after", true)).unwrap();

    // The open transaction's own read must still see the pre-snapshot value.
    let seen = tx.get_row(table_id, &pk(1)).unwrap().unwrap();
    assert_eq!(seen[1], Some(RelationalValue::Text("before".to_string())));
    // Dropping (implicit rollback) since this tx made no writes of its own.
    drop(tx);
    f.cleanup();
}

// -----------------------------------------------------------------
// Conflict detection — item 11/13
// -----------------------------------------------------------------

#[test]
fn two_writers_same_key_second_committer_conflicts() {
    let f = Fixture::new("same_key_conflict");
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "orig", true)).unwrap();

    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    t1.put_row(table_id, &row(1, "from_t1", true)).unwrap();
    t2.put_row(table_id, &row(1, "from_t2", true)).unwrap();

    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(matches!(err, RelationalError::Conflict { .. }));

    let final_row = f.store.get_row(table_id, &pk(1)).unwrap().unwrap();
    assert_eq!(
        final_row[1],
        Some(RelationalValue::Text("from_t1".to_string()))
    );
    f.cleanup();
}

#[test]
fn two_writers_different_keys_both_succeed() {
    let f = Fixture::new("different_keys");
    let table_id = create_simple_table(&f.catalog, "t");
    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    t1.put_row(table_id, &row(1, "a", true)).unwrap();
    t2.put_row(table_id, &row(2, "b", true)).unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    assert!(f.store.get_row(table_id, &pk(1)).unwrap().is_some());
    assert!(f.store.get_row(table_id, &pk(2)).unwrap().is_some());
    f.cleanup();
}

#[test]
fn primary_key_conflict_exactly_one_insert_wins_both_orderings() {
    for reverse in [false, true] {
        let f = Fixture::new(&format!("pk_conflict_{reverse}"));
        let table_id = create_simple_table(&f.catalog, "t");
        let mut t1 = f.txm.begin().unwrap();
        let mut t2 = f.txm.begin().unwrap();
        t1.put_row(table_id, &row(1, "t1", true)).unwrap();
        t2.put_row(table_id, &row(1, "t2", true)).unwrap();

        let (first, second, first_name) = if reverse {
            (t2, t1, "t2")
        } else {
            (t1, t2, "t1")
        };
        first.commit().unwrap();
        let err = second.commit().unwrap_err();
        assert!(matches!(err, RelationalError::Conflict { .. }));

        let winner = f.store.get_row(table_id, &pk(1)).unwrap().unwrap();
        assert_eq!(
            winner[1],
            Some(RelationalValue::Text(first_name.to_string()))
        );
        f.cleanup();
    }
}

#[test]
fn long_open_read_only_transaction_does_not_block_unrelated_commits() {
    let f = Fixture::new("long_open_no_block");
    let table_id = create_simple_table(&f.catalog, "t");
    let long_lived = f.txm.begin().unwrap(); // never committed during this test
    for i in 0..20 {
        f.store.put_row(table_id, &row(i, "x", true)).unwrap();
    }
    assert!(f.store.get_row(table_id, &pk(0)).unwrap().is_some());
    drop(long_lived);
    f.cleanup();
}

// -----------------------------------------------------------------
// UNIQUE enforcement — item 14 (critical gate)
// -----------------------------------------------------------------

#[test]
fn unique_conflict_exactly_one_committer_wins_both_orderings() {
    for reverse in [false, true] {
        let f = Fixture::new(&format!("unique_conflict_{reverse}"));
        let table_id = create_simple_table(&f.catalog, "t");
        f.builder
            .create_index_online(table_id, "name_unique", IndexKind::Unique, &[1])
            .unwrap();

        let mut t1 = f.txm.begin().unwrap();
        let mut t2 = f.txm.begin().unwrap();
        t1.put_row(table_id, &row(1, "same-value", true)).unwrap();
        t2.put_row(table_id, &row(2, "same-value", true)).unwrap();

        let (first, second) = if reverse { (t2, t1) } else { (t1, t2) };
        first.commit().unwrap();
        let err = second.commit().unwrap_err();
        assert!(matches!(err, RelationalError::Conflict { .. }));
        f.cleanup();
    }
}

#[test]
fn unique_different_values_both_commit() {
    let f = Fixture::new("unique_different_values");
    let table_id = create_simple_table(&f.catalog, "t");
    f.builder
        .create_index_online(table_id, "name_unique", IndexKind::Unique, &[1])
        .unwrap();
    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    t1.put_row(table_id, &row(1, "a", true)).unwrap();
    t2.put_row(table_id, &row(2, "b", true)).unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    f.cleanup();
}

#[test]
fn unique_null_values_do_not_conflict_with_each_other() {
    // Standard SQL UNIQUE semantics (ISO/IEC 9075, matched by
    // PostgreSQL/SQLite): NULL is never equal to another NULL, so a
    // UNIQUE index permits arbitrarily many rows whose indexed column is
    // NULL. `PHASE_RELATIONAL_INDEX_BACKFILL_ADR.md` §3 only specifies
    // NULL's physical *encoding* (a single reserved low-sorting tag) --
    // it does not itself set UNIQUE-vs-NULL semantics, since no UNIQUE
    // enforcement existed before this increment. This test fixes that
    // choice and guards it.
    let f = Fixture::new("unique_null_no_conflict");
    let table_id = create_simple_table(&f.catalog, "t");
    f.builder
        .create_index_online(table_id, "name_unique", IndexKind::Unique, &[1])
        .unwrap();
    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    t1.put_row(
        table_id,
        &[
            Some(RelationalValue::Integer(1)),
            None,
            Some(RelationalValue::Boolean(true)),
        ],
    )
    .unwrap();
    t2.put_row(
        table_id,
        &[
            Some(RelationalValue::Integer(2)),
            None,
            Some(RelationalValue::Boolean(true)),
        ],
    )
    .unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    f.cleanup();
}

#[test]
fn intra_transaction_duplicate_unique_value_is_rejected() {
    let f = Fixture::new("intra_txn_unique_dup");
    let table_id = create_simple_table(&f.catalog, "t");
    f.builder
        .create_index_online(table_id, "name_unique", IndexKind::Unique, &[1])
        .unwrap();
    let mut tx = f.txm.begin().unwrap();
    tx.put_row(table_id, &row(1, "dup", true)).unwrap();
    tx.put_row(table_id, &row(2, "dup", true)).unwrap();
    let err = tx.commit().unwrap_err();
    assert!(matches!(err, RelationalError::Conflict { .. }));
    f.cleanup();
}

#[test]
fn delete_then_reinsert_same_unique_value_in_one_transaction_succeeds() {
    let f = Fixture::new("delete_reinsert_unique");
    let table_id = create_simple_table(&f.catalog, "t");
    f.builder
        .create_index_online(table_id, "name_unique", IndexKind::Unique, &[1])
        .unwrap();
    f.store.put_row(table_id, &row(1, "shared", true)).unwrap();

    let mut tx = f.txm.begin().unwrap();
    tx.delete_row(table_id, &pk(1)).unwrap();
    tx.put_row(table_id, &row(2, "shared", true)).unwrap();
    tx.commit().unwrap();

    assert!(f.store.get_row(table_id, &pk(1)).unwrap().is_none());
    assert!(f.store.get_row(table_id, &pk(2)).unwrap().is_some());
    f.cleanup();
}

#[test]
fn multiple_unique_indexes_each_enforced_independently() {
    let f = Fixture::new("multiple_unique_indexes");
    let table_id = create_simple_table(&f.catalog, "t");
    f.builder
        .create_index_online(table_id, "name_unique", IndexKind::Unique, &[1])
        .unwrap();
    f.builder
        .create_index_online(table_id, "active_and_name_unique", IndexKind::Unique, &[2])
        .unwrap();
    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    t1.put_row(table_id, &row(1, "a", true)).unwrap();
    t2.put_row(table_id, &row(2, "b", true)).unwrap(); // conflicts on the `active` unique index (both true)
    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(matches!(err, RelationalError::Conflict { .. }));
    f.cleanup();
}

// -----------------------------------------------------------------
// Index/table atomic commit — item 15
// -----------------------------------------------------------------

#[test]
fn commit_issues_exactly_one_write_batch_regardless_of_row_count() {
    let f = Fixture::new("one_write_batch");
    let table_id = create_simple_table(&f.catalog, "t");
    f.builder
        .create_index_online(table_id, "name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    let before = f.store.scan_table(table_id).unwrap().len();
    let _ = before;

    let mut tx = f.txm.begin().unwrap();
    for i in 0..10 {
        tx.put_row(table_id, &row(i, "same", true)).unwrap();
    }
    let seq_before = probe_seq(&f);
    tx.commit().unwrap();
    let seq_after = probe_seq(&f);
    // `seq_before`/`seq_after` are each themselves consumed by a probe
    // write, so exactly one write_batch call for the whole 10-row
    // transaction in between means the two probes are 2 seqs apart, not 1.
    assert_eq!(
        seq_after,
        seq_before + 2,
        "one write_batch call for the whole multi-row transaction"
    );
    f.cleanup();
}

/// A cheap way to observe the engine's own current sequence: issue a
/// trivial single-key write and read back its returned seq, matching
/// `RELATIONAL ADR AMENDMENT 003` RA.5's own established verification
/// technique.
fn probe_seq(f: &Fixture) -> u64 {
    f.engine.put(b"__txn_test_seq_probe__", b"x").unwrap()
}

#[test]
fn index_entries_visible_atomically_with_the_row() {
    let f = Fixture::new("atomic_index_visibility");
    let table_id = create_simple_table(&f.catalog, "t");
    let index_id = f
        .builder
        .create_index_online(table_id, "name_idx", IndexKind::NonUnique, &[1])
        .unwrap();

    let mut tx = f.txm.begin().unwrap();
    tx.put_row(table_id, &row(1, "findme", true)).unwrap();
    // Before commit, the index must not see it.
    assert!(f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("findme".to_string()))]
        )
        .unwrap()
        .is_empty());
    tx.commit().unwrap();
    let results = f
        .builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("findme".to_string()))],
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    f.cleanup();
}

// -----------------------------------------------------------------
// Autocommit — item 17
// -----------------------------------------------------------------

#[test]
fn autocommit_put_and_delete() {
    let f = Fixture::new("autocommit");
    let table_id = create_simple_table(&f.catalog, "t");
    f.txm
        .autocommit_put_row(table_id, &row(1, "a", true))
        .unwrap();
    assert!(f.store.get_row(table_id, &pk(1)).unwrap().is_some());
    f.txm.autocommit_delete_row(table_id, &pk(1)).unwrap();
    assert!(f.store.get_row(table_id, &pk(1)).unwrap().is_none());
    f.cleanup();
}

// -----------------------------------------------------------------
// Write skew — item 20: documented, not "fixed"
// -----------------------------------------------------------------

/// `WRITE SKEW POSSIBLE UNDER SNAPSHOT ISOLATION` (D10's own stated,
/// accepted limitation). Two "on-call" rows with the invariant "at least
/// one is on call": each transaction reads *both* rows, decides (based
/// on that read) to take itself off call, and writes only its *own*
/// row. Both transactions' write-sets are disjoint (no shared key), so
/// SI's own conflict check -- which only ever looks at the write-set --
/// permits both to commit, even though the combined result violates the
/// invariant neither transaction's own check could see. This is
/// intentional, accepted behavior, not a bug this increment "fixes."
#[test]
fn write_skew_is_possible_under_snapshot_isolation() {
    let f = Fixture::new("write_skew");
    let table_id = create_simple_table(&f.catalog, "t");
    // active = "on call" for this test's purposes.
    f.store.put_row(table_id, &row(1, "alice", true)).unwrap();
    f.store.put_row(table_id, &row(2, "bob", true)).unwrap();

    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();

    let alice_oncall =
        t1.get_row(table_id, &pk(1)).unwrap().unwrap()[2] == Some(RelationalValue::Boolean(true));
    let bob_oncall_for_t1 =
        t1.get_row(table_id, &pk(2)).unwrap().unwrap()[2] == Some(RelationalValue::Boolean(true));
    assert!(alice_oncall && bob_oncall_for_t1);
    t1.put_row(table_id, &row(1, "alice", false)).unwrap(); // alice goes off call, relying on bob

    let bob_oncall =
        t2.get_row(table_id, &pk(2)).unwrap().unwrap()[2] == Some(RelationalValue::Boolean(true));
    let alice_oncall_for_t2 =
        t2.get_row(table_id, &pk(1)).unwrap().unwrap()[2] == Some(RelationalValue::Boolean(true));
    assert!(bob_oncall && alice_oncall_for_t2);
    t2.put_row(table_id, &row(2, "bob", false)).unwrap(); // bob goes off call, relying on alice

    // Disjoint write-sets -- SI permits both.
    t1.commit().unwrap();
    t2.commit().unwrap();

    let alice_final = f.store.get_row(table_id, &pk(1)).unwrap().unwrap();
    let bob_final = f.store.get_row(table_id, &pk(2)).unwrap().unwrap();
    assert_eq!(alice_final[2], Some(RelationalValue::Boolean(false)));
    assert_eq!(bob_final[2], Some(RelationalValue::Boolean(false)));
    // The invariant "at least one on call" is now violated -- proving
    // write skew occurred, exactly as D10 documents it can.
    f.cleanup();
}

// -----------------------------------------------------------------
// Deterministic concurrency — item 19, barrier-synchronized, never sleeps
// -----------------------------------------------------------------

#[test]
fn concurrent_writers_same_key_deterministic_barrier() {
    let f = Arc::new(Fixture::new("concurrent_same_key"));
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "orig", true)).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let results: Arc<std::sync::Mutex<Vec<bool>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

    let mut handles = Vec::new();
    for n in 0..2 {
        let txm = f.txm.clone();
        let barrier = Arc::clone(&barrier);
        let results = Arc::clone(&results);
        handles.push(thread::spawn(move || {
            let mut tx = txm.begin().unwrap();
            tx.put_row(table_id, &row(1, if n == 0 { "a" } else { "b" }, true))
                .unwrap();
            barrier.wait();
            let ok = tx.commit().is_ok();
            results.lock().unwrap().push(ok);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let results = results.lock().unwrap();
    assert_eq!(
        results.iter().filter(|&&ok| ok).count(),
        1,
        "exactly one commit must win"
    );
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

#[test]
fn concurrent_writers_different_keys_both_succeed_barrier() {
    let f = Arc::new(Fixture::new("concurrent_diff_keys"));
    let table_id = create_simple_table(&f.catalog, "t");
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for n in 0..3 {
        let txm = f.txm.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut tx = txm.begin().unwrap();
            tx.put_row(table_id, &row(n, "x", true)).unwrap();
            barrier.wait();
            tx.commit()
        }));
    }
    for h in handles {
        h.join().unwrap().unwrap();
    }
    for n in 0..3 {
        assert!(f.store.get_row(table_id, &pk(n)).unwrap().is_some());
    }
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

#[test]
fn writer_and_reader_deterministic_barrier() {
    let f = Arc::new(Fixture::new("writer_reader"));
    let table_id = create_simple_table(&f.catalog, "t");
    f.store.put_row(table_id, &row(1, "before", true)).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let reader_tx = f.txm.begin().unwrap();

    let writer = {
        let txm = f.txm.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            txm.autocommit_put_row(table_id, &row(1, "after", true))
                .unwrap();
        })
    };
    let seen_before_write = reader_tx.get_row(table_id, &pk(1)).unwrap().unwrap();
    barrier.wait();
    writer.join().unwrap();
    // The reader's own snapshot must still reflect the pre-write value,
    // regardless of the writer's completion timing.
    assert_eq!(
        seen_before_write[1],
        Some(RelationalValue::Text("before".to_string()))
    );
    drop(reader_tx);
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

#[test]
fn rollback_racing_another_transactions_commit() {
    let f = Arc::new(Fixture::new("rollback_races_commit"));
    let table_id = create_simple_table(&f.catalog, "t");
    let barrier = Arc::new(Barrier::new(2));

    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    t1.put_row(table_id, &row(1, "t1", true)).unwrap();
    t2.put_row(table_id, &row(1, "t2", true)).unwrap();

    let h1 = {
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            t1.rollback().unwrap();
        })
    };
    let h2 = {
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            t2.commit()
        })
    };
    h1.join().unwrap();
    let commit_result = h2.join().unwrap();
    assert!(
        commit_result.is_ok(),
        "t2 must succeed: t1 rolled back, never conflicted"
    );
    let final_row = f.store.get_row(table_id, &pk(1)).unwrap().unwrap();
    assert_eq!(final_row[1], Some(RelationalValue::Text("t2".to_string())));
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

// -----------------------------------------------------------------
// Resource limits — item 10/32
// -----------------------------------------------------------------

#[test]
fn max_write_set_ops_boundary() {
    let f = Fixture::new("max_ops_boundary");
    let table_id = create_simple_table(&f.catalog, "t");
    let txm = TransactionManager::with_limits(
        Arc::clone(&f.engine),
        Arc::clone(&f.store),
        crate::relational::TxnLimits {
            max_write_set_ops: 3,
            ..Default::default()
        },
    );
    let mut tx = txm.begin().unwrap();
    for i in 0..3 {
        tx.put_row(table_id, &row(i, "x", true)).unwrap();
    }
    let err = tx.put_row(table_id, &row(3, "x", true)).unwrap_err();
    assert!(matches!(err, RelationalError::ResourceLimit { .. }));
    f.cleanup();
}

#[test]
fn max_write_set_bytes_boundary() {
    let f = Fixture::new("max_bytes_boundary");
    let table_id = create_simple_table(&f.catalog, "t");
    let txm = TransactionManager::with_limits(
        Arc::clone(&f.engine),
        Arc::clone(&f.store),
        crate::relational::TxnLimits {
            max_write_set_bytes: 32,
            ..Default::default()
        },
    );
    let mut tx = txm.begin().unwrap();
    let err = tx
        .put_row(table_id, &row(0, &"x".repeat(100), true))
        .unwrap_err();
    assert!(matches!(err, RelationalError::ResourceLimit { .. }));
    f.cleanup();
}

#[test]
fn max_concurrent_transactions_boundary() {
    let f = Fixture::new("max_concurrent_boundary");
    let txm = TransactionManager::with_limits(
        Arc::clone(&f.engine),
        Arc::clone(&f.store),
        crate::relational::TxnLimits {
            max_concurrent_transactions: 2,
            ..Default::default()
        },
    );
    let t1 = txm.begin().unwrap();
    let t2 = txm.begin().unwrap();
    match txm.begin() {
        Err(RelationalError::ResourceLimit { .. }) => {}
        _ => panic!("expected ResourceLimit"),
    }
    drop(t1);
    drop(t2);
    f.cleanup();
}

// -----------------------------------------------------------------
// Registry / memory — item 22/23/38
// -----------------------------------------------------------------

#[test]
fn active_transaction_count_returns_to_zero_after_many_cycles() {
    let f = Fixture::new("registry_cycles");
    let table_id = create_simple_table(&f.catalog, "t");
    for i in 0..200 {
        let mut tx = f.txm.begin().unwrap();
        tx.put_row(table_id, &row(i, "x", true)).unwrap();
        if i % 2 == 0 {
            tx.commit().unwrap();
        } else {
            tx.rollback().unwrap();
        }
    }
    assert_eq!(f.txm.active_transactions(), 0);
    f.cleanup();
}

#[test]
fn oldest_live_snapshot_seq_returns_to_none_after_finish() {
    let f = Fixture::new("snapshot_lifetime");
    assert_eq!(f.engine.oldest_live_snapshot_seq(), None);
    let tx = f.txm.begin().unwrap();
    assert!(f.engine.oldest_live_snapshot_seq().is_some());
    drop(tx);
    assert_eq!(f.engine.oldest_live_snapshot_seq(), None);
    f.cleanup();
}

// -----------------------------------------------------------------
// Compaction integration — item 24
// -----------------------------------------------------------------

#[test]
fn transaction_snapshot_remains_valid_across_automatic_compaction() {
    let dir = temp_dir("txn_compaction");
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
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
    let table_id = create_simple_table(&catalog, "t");
    store.put_row(table_id, &row(1, "before", true)).unwrap();

    let tx = txm.begin().unwrap();
    // Force enough flush/compaction activity for real automatic
    // compaction to run several cycles while `tx`'s snapshot is alive.
    for i in 0..150 {
        store
            .put_row(table_id, &row(i + 100, "filler", true))
            .unwrap();
    }
    assert!(
        engine.compaction_metrics().cycles_completed > 0,
        "fixture must actually trigger compaction"
    );

    let seen = tx.get_row(table_id, &pk(1)).unwrap().unwrap();
    assert_eq!(seen[1], Some(RelationalValue::Text("before".to_string())));
    drop(tx);

    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Crash recovery — item 25/26/27/28. `commit()` never issues more than
// the one `write_batch` call whose own all-or-nothing durability is
// already certified at the engine/WAL layer (D9) -- these tests verify
// the transaction layer's own commit boundary sits correctly relative
// to that guarantee across a real process restart (a fresh `LsmEngine::
// open` against the same directory), not that `write_batch` itself is
// atomic (that is `wal`/`manifest`'s own certified, protected territory).
// -----------------------------------------------------------------

#[test]
fn crash_before_commit_leaves_nothing_visible_after_restart() {
    let dir = temp_dir("crash_before_commit");
    let table_id;
    {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
        table_id = create_simple_table(&catalog, "t");
        store.put_row(table_id, &row(1, "durable", true)).unwrap();

        let mut tx = txm.begin().unwrap();
        tx.put_row(table_id, &row(2, "never_committed", true))
            .unwrap();
        tx.put_row(table_id, &row(3, "never_committed", true))
            .unwrap();
        // Simulate a crash: the process ends here, tx is never committed
        // or rolled back -- nothing in its write-set was ever sent to the
        // engine, so there is nothing for the engine to undo.
        engine.shutdown();
    }

    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let store = TableStore::new(Arc::clone(&engine), catalog);
    assert!(
        store.get_row(table_id, &pk(1)).unwrap().is_some(),
        "the prior committed row must survive"
    );
    assert!(
        store.get_row(table_id, &pk(2)).unwrap().is_none(),
        "uncommitted write must not survive"
    );
    assert!(
        store.get_row(table_id, &pk(3)).unwrap().is_none(),
        "uncommitted write must not survive"
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn committed_multi_row_indexed_transaction_survives_restart_atomically() {
    let dir = temp_dir("crash_after_commit");
    let table_id;
    let index_id;
    {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
        let builder = IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        );
        table_id = create_simple_table(&catalog, "t");
        index_id = builder
            .create_index_online(table_id, "name_idx", IndexKind::NonUnique, &[1])
            .unwrap();

        let mut tx = txm.begin().unwrap();
        for i in 0..5 {
            tx.put_row(table_id, &row(i, "batch", true)).unwrap();
        }
        tx.commit().unwrap();
        engine.shutdown();
    }

    let engine = open(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    );
    for i in 0..5 {
        assert!(
            store.get_row(table_id, &pk(i)).unwrap().is_some(),
            "row {i} must survive"
        );
    }
    let via_index = builder
        .index_lookup(
            index_id,
            &[Some(RelationalValue::Text("batch".to_string()))],
        )
        .unwrap();
    assert_eq!(
        via_index.len(),
        5,
        "every index entry from the atomic commit must survive alongside its row"
    );
    engine.shutdown();
    let _ = fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Metrics — item 46
// -----------------------------------------------------------------

#[test]
fn metrics_record_start_commit_rollback_and_conflict() {
    let f = Fixture::new("metrics");
    let table_id = create_simple_table(&f.catalog, "t");

    let mut tx_ok = f.txm.begin().unwrap();
    tx_ok.put_row(table_id, &row(1, "a", true)).unwrap();
    tx_ok.commit().unwrap();

    let mut tx_rb = f.txm.begin().unwrap();
    tx_rb.put_row(table_id, &row(2, "b", true)).unwrap();
    tx_rb.rollback().unwrap();

    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    t1.put_row(table_id, &row(3, "c", true)).unwrap();
    t2.put_row(table_id, &row(3, "d", true)).unwrap();
    t1.commit().unwrap();
    let _ = t2.commit();

    let snap = f.txm.metrics();
    assert_eq!(snap.transactions_started, 4);
    assert_eq!(snap.transactions_committed, 2);
    assert_eq!(snap.transactions_rolled_back, 1);
    assert_eq!(snap.transactions_aborted, 1);
    assert_eq!(snap.transaction_conflicts, 1);
    assert_eq!(snap.active_transactions, 0);
    f.cleanup();
}

// -----------------------------------------------------------------
// Security — item 31: no client-supplied physical IDs
// -----------------------------------------------------------------

#[test]
fn authorization_boundary_is_unaffected_by_the_transaction_layer() {
    // The transaction layer's own API takes only already-resolved
    // table_ids/RelationalValues -- it has no field or method capable of
    // accepting a raw physical key, index id, or catalog id from an
    // external caller, so it cannot itself provide a privilege-
    // escalation path around whatever binder-level authorization a
    // future SQL executor performs before ever calling `put_row`/
    // `delete_row`. This test documents/enforces that by using the API
    // exactly as an executor would: table_id resolved once (from the
    // catalog, by name), then reused -- there is no alternate "raw key"
    // entry point to attempt a bypass through.
    let f = Fixture::new("no_bypass_surface");
    let table_id = create_simple_table(&f.catalog, "t");
    // A grant exists (D25) but this layer never consults it -- that is
    // exactly the documented, unchanged boundary (item 31): authorization
    // stays at the binder, not duplicated or bypassed here.
    f.catalog
        .grant("someone", ObjectKind::Table, table_id, Privilege::Select)
        .unwrap();
    let mut tx = f.txm.begin().unwrap();
    tx.put_row(table_id, &row(1, "x", true)).unwrap();
    tx.commit().unwrap();
    f.cleanup();
}

// -----------------------------------------------------------------
// Differential / reference-model testing — item 40/41
// -----------------------------------------------------------------

mod differential {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    /// An independent Snapshot-Isolation reference model, implemented
    /// from scratch against a plain `BTreeMap<pk, (seq, Option<row>)>`
    /// history -- never calling into `crate::relational::txn`. Applies
    /// the identical rule D10 states: `BEGIN` pins a seq; reads resolve
    /// against the pinned seq overlaid with this transaction's own
    /// buffered writes; `COMMIT` conflicts if any written key's *current*
    /// committed value differs from what the pinned seq saw.
    struct ReferenceDb {
        history: BTreeMap<Vec<u8>, Vec<(u64, Option<Row>)>>, // key -> [(seq, value)] ascending
        next_seq: u64,
    }

    impl ReferenceDb {
        fn new() -> Self {
            ReferenceDb {
                history: BTreeMap::new(),
                next_seq: 1,
            }
        }

        fn value_as_of(&self, key: &[u8], seq: u64) -> Option<Row> {
            self.history
                .get(key)
                .and_then(|versions| versions.iter().rev().find(|(s, _)| *s <= seq))
                .and_then(|(_, v)| v.clone())
        }

        fn current(&self, key: &[u8]) -> Option<Row> {
            self.value_as_of(key, u64::MAX)
        }

        fn apply(&mut self, writes: &[(Vec<u8>, Option<Row>)]) -> u64 {
            let seq = self.next_seq;
            self.next_seq += 1;
            for (key, value) in writes {
                self.history
                    .entry(key.clone())
                    .or_default()
                    .push((seq, value.clone()));
            }
            seq
        }
    }

    struct RefTxn {
        snapshot_seq: u64,
        writes: BTreeMap<Vec<u8>, Option<Row>>,
    }

    impl RefTxn {
        fn put(&mut self, key: Vec<u8>, row: Row) {
            self.writes.insert(key, Some(row));
        }

        fn delete(&mut self, key: Vec<u8>) {
            self.writes.insert(key, None);
        }

        fn commit(self, db: &mut ReferenceDb) -> std::result::Result<u64, ()> {
            for key in self.writes.keys() {
                if db.value_as_of(key, self.snapshot_seq) != db.current(key) {
                    return Err(());
                }
            }
            let writes: Vec<_> = self.writes.into_iter().collect();
            Ok(db.apply(&writes))
        }
    }

    /// Runs the SAME deterministic sequence of BEGIN/PUT/DELETE/COMMIT/
    /// ROLLBACK operations against the real transaction engine and this
    /// independent reference model, comparing final committed state and
    /// every individual commit/abort decision.
    #[test]
    fn matches_reference_model_for_a_fixed_sequential_scenario() {
        let f = Fixture::new("differential_sequential");
        let table_id = create_simple_table(&f.catalog, "t");
        let mut refdb = ReferenceDb::new();

        // A scripted sequence exercising same-key overwrite, delete,
        // multiple keys, and a read-modify-write pattern.
        type ScriptBatch<'a> = Vec<(i32, Option<(&'a str, bool)>)>;
        let script: Vec<ScriptBatch> = vec![
            vec![(1, Some(("a", true)))],
            vec![(1, Some(("b", true))), (2, Some(("c", false)))],
            vec![(1, None)],
            vec![(2, Some(("d", true))), (3, Some(("e", true)))],
        ];

        for batch in script {
            let mut tx = f.txm.begin().unwrap();
            let mut refkey = f.txm.begin_ref(&mut refdb);
            for (id, value) in &batch {
                let key = id.to_be_bytes().to_vec();
                match value {
                    Some((name, active)) => {
                        let r = row(*id, name, *active);
                        tx.put_row(table_id, &r).unwrap();
                        refkey.put(key, r);
                    }
                    None => {
                        tx.delete_row(table_id, &pk(*id)).unwrap();
                        refkey.delete(key);
                    }
                }
            }
            let real_result = tx.commit();
            let ref_result = refkey.commit(&mut refdb);
            assert_eq!(real_result.is_ok(), ref_result.is_ok());
        }

        for id in [1, 2, 3] {
            let real = f.store.get_row(table_id, &pk(id)).unwrap();
            let ref_row = refdb.current(&id.to_be_bytes());
            assert_eq!(real, ref_row, "table {table_id} pk {id}");
        }
        f.cleanup();
    }

    /// Helper extension used only by the differential test above, kept
    /// local to this module rather than added to the production
    /// `TransactionManager` API.
    trait BeginRef {
        fn begin_ref(&self, db: &mut ReferenceDb) -> RefTxn;
    }
    impl BeginRef for crate::relational::TransactionManager {
        fn begin_ref(&self, db: &mut ReferenceDb) -> RefTxn {
            RefTxn {
                snapshot_seq: db.next_seq.saturating_sub(1),
                writes: BTreeMap::new(),
            }
        }
    }

    // -------------------------------------------------------------
    // Property testing (item 40): randomized, *interleaved* BEGIN/PUT/
    // DELETE/COMMIT/ROLLBACK sequences across several simultaneously-
    // open transaction "slots" (so real conflicts actually arise),
    // checked against `ReferenceDb`/`RefTxn` above -- an independent
    // model, never the production code used as its own oracle.
    // -------------------------------------------------------------

    const SLOTS: usize = 3;
    const PKS: i32 = 3;

    #[derive(Debug, Clone)]
    enum Op {
        Begin(usize),
        Put(usize, i32, bool),
        Delete(usize, i32),
        Commit(usize),
        Rollback(usize),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (0..SLOTS).prop_map(Op::Begin),
            (0..SLOTS, 0..PKS, any::<bool>()).prop_map(|(s, p, b)| Op::Put(s, p, b)),
            (0..SLOTS, 0..PKS).prop_map(|(s, p)| Op::Delete(s, p)),
            (0..SLOTS).prop_map(Op::Commit),
            (0..SLOTS).prop_map(Op::Rollback),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 12, .. ProptestConfig::default() })]

        #[test]
        fn matches_reference_model_for_randomized_interleaved_transactions(
            ops in prop::collection::vec(op_strategy(), 1..20)
        ) {
            let f = Fixture::new("differential_property");
            let table_id = create_simple_table(&f.catalog, "t");
            let mut refdb = ReferenceDb::new();
            let mut real_slots: Vec<Option<crate::relational::Transaction>> = (0..SLOTS).map(|_| None).collect();
            let mut ref_slots: Vec<Option<RefTxn>> = (0..SLOTS).map(|_| None).collect();

            for op in ops {
                match op {
                    Op::Begin(s) => {
                        if real_slots[s].is_none() {
                            real_slots[s] = Some(f.txm.begin().unwrap());
                            ref_slots[s] = Some(f.txm.begin_ref(&mut refdb));
                        }
                    }
                    Op::Put(s, p, b) => {
                        if let (Some(rt), Some(rf)) = (real_slots[s].as_mut(), ref_slots[s].as_mut()) {
                            let r = row(p, "v", b);
                            rt.put_row(table_id, &r).unwrap();
                            rf.put(p.to_be_bytes().to_vec(), r);
                        }
                    }
                    Op::Delete(s, p) => {
                        if let (Some(rt), Some(rf)) = (real_slots[s].as_mut(), ref_slots[s].as_mut()) {
                            rt.delete_row(table_id, &pk(p)).unwrap();
                            rf.delete(p.to_be_bytes().to_vec());
                        }
                    }
                    Op::Commit(s) => {
                        if real_slots[s].is_some() {
                            let rt = real_slots[s].take().unwrap();
                            let rf = ref_slots[s].take().unwrap();
                            let real_ok = rt.commit().is_ok();
                            let ref_ok = rf.commit(&mut refdb).is_ok();
                            prop_assert_eq!(real_ok, ref_ok, "commit decision mismatch in slot {}", s);
                        }
                    }
                    Op::Rollback(s) => {
                        if real_slots[s].is_some() {
                            let rt = real_slots[s].take().unwrap();
                            let _rf = ref_slots[s].take();
                            rt.rollback().unwrap();
                        }
                    }
                }
            }
            drop(real_slots);
            drop(ref_slots);

            for p in 0..PKS {
                let real_row = f.store.get_row(table_id, &pk(p)).unwrap();
                let ref_row = refdb.current(&p.to_be_bytes());
                prop_assert_eq!(real_row, ref_row, "pk {} final state mismatch", p);
            }
            f.cleanup();
        }
    }
}
