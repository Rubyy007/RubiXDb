//! `ALTER TABLE` and foreign-key tests at the relational layer
//! (`docs/adr/ADR-DDL-FK-01.md`): the concurrency argument for commit-time
//! validation, savepoints, versioned row decode and the row-lookup access paths.
//! The SQL surface is covered by `sql/src/phase2b3_tests.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::schema::{FkAction, IndexKind};
use crate::catalog::service::{ColumnDef, NewForeignKey};
use crate::catalog::CatalogService;
use crate::execution::batch_coordinator::BatchCoordinatorConfig;
use crate::lsm::{LsmConfig, LsmEngine};
use crate::relational::index::IndexBuilder;
use crate::relational::lookup::Access;
use crate::relational::value::{TYPE_TAG_INTEGER, TYPE_TAG_TEXT};
use crate::relational::{
    RelationalError, RelationalValue, RowChange, TableStore, TransactionManager,
};
use crate::wal::{SyncMode, WalConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("rubixdb_fk_test_{tag}_{nanos}_{n}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn open(dir: &Path) -> Arc<LsmEngine> {
    Arc::new(
        LsmEngine::open(
            dir,
            WalConfig {
                sync_mode: SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(5),
                    max_batch_bytes: 256 * 1024,
                },
                ..WalConfig::default()
            },
            BatchCoordinatorConfig {
                queue_capacity: 256,
                max_queued_bytes: 8 * 1024 * 1024,
                submission_timeout: Duration::from_secs(5),
                shutdown_drain_bound: Duration::from_secs(10),
                await_retry_budget: Duration::from_secs(5),
                max_drain_per_batch: 4096,
            },
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
    builder: Arc<IndexBuilder>,
    schema_id: u32,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = temp_dir(tag);
        Self::over(dir)
    }

    fn over(dir: PathBuf) -> Self {
        let engine = open(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let db = catalog.list_databases().unwrap()[0].database_id;
        let schema_id = catalog.list_schemas(db).unwrap()[0].schema_id;
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
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
            txm,
            builder,
            schema_id,
        }
    }

    /// Shuts the engine down and opens it again over the same directory.
    fn restart(self) -> Fixture {
        let Fixture {
            dir,
            engine,
            catalog,
            store,
            txm,
            builder,
            ..
        } = self;
        drop((catalog, store, txm, builder));
        engine.shutdown();
        drop(engine);
        Fixture::over(dir)
    }

    fn cleanup(self) {
        self.engine.shutdown();
        let _ = fs::remove_dir_all(&self.dir);
    }

    /// `name(id INTEGER PRIMARY KEY, ref INTEGER NULL, label TEXT NULL)`.
    fn table(&self, name: &str) -> u32 {
        let col = |n: &str, t: u8, nullable: bool| ColumnDef {
            name: n.to_string(),
            data_type: t,
            nullable,
            default_value: None,
            type_params: None,
        };
        self.catalog
            .create_table(
                self.schema_id,
                name,
                &[
                    col("id", TYPE_TAG_INTEGER, false),
                    col("ref", TYPE_TAG_INTEGER, true),
                    col("label", TYPE_TAG_TEXT, true),
                ],
                &[0],
            )
            .unwrap()
    }

    fn fk(&self, child: u32, parent: u32, on_delete: FkAction) -> NewForeignKey {
        NewForeignKey {
            table_id: child,
            name: format!("fk_{child}_{parent}"),
            child_ordinals: vec![1],
            parent_table_id: parent,
            parent_ordinals: vec![0],
            on_delete,
            on_update: FkAction::NoAction,
        }
    }
}

fn int(n: i32) -> Option<RelationalValue> {
    Some(RelationalValue::Integer(n))
}

fn row(id: i32, reference: Option<i32>) -> Vec<Option<RelationalValue>> {
    vec![int(id), reference.and_then(|r| int(r).into()), None]
}

fn pk(id: i32) -> Vec<RelationalValue> {
    vec![RelationalValue::Integer(id)]
}

fn ids(f: &Fixture, table: u32) -> Vec<i32> {
    f.store
        .scan_table(table)
        .unwrap()
        .into_iter()
        .map(|(pk, _)| match pk[0] {
            RelationalValue::Integer(n) => n,
            _ => unreachable!(),
        })
        .collect()
}

fn is_fk_violation(e: &RelationalError) -> bool {
    matches!(e, RelationalError::ForeignKeyViolation { .. })
}

// =======================================================================
// Savepoints
// =======================================================================

mod savepoints {
    use super::*;

    #[test]
    fn rollback_to_restores_the_write_set_exactly() {
        let f = Fixture::new("sp_restore");
        let t = f.table("t");
        f.store.put_row(t, &row(1, None)).unwrap();
        f.store.put_row(t, &row(2, None)).unwrap();

        let mut txn = f.txm.begin().unwrap();
        txn.put_row(t, &row(10, None)).unwrap(); // before the savepoint
        let sp = txn.savepoint();
        txn.put_row(t, &row(11, None)).unwrap(); // a new key
        txn.put_row(t, &row(10, Some(5))).unwrap(); // overwrite a pre-savepoint key
        txn.delete_row(t, &pk(1)).unwrap(); // delete a committed row
        txn.rollback_to(sp);

        assert_eq!(txn.get_row(t, &pk(10)).unwrap(), Some(row(10, None)));
        assert_eq!(txn.get_row(t, &pk(11)).unwrap(), None);
        assert_eq!(txn.get_row(t, &pk(1)).unwrap(), Some(row(1, None)));
        txn.commit().unwrap();
        assert_eq!(ids(&f, t), vec![1, 2, 10]);
        f.cleanup();
    }

    #[test]
    fn rollback_to_restores_the_size_accounting() {
        let f = Fixture::new("sp_accounting");
        let t = f.table("t");
        let txm = TransactionManager::with_limits(
            Arc::clone(&f.engine),
            Arc::clone(&f.store),
            crate::relational::TxnLimits {
                max_write_set_ops: 3,
                ..Default::default()
            },
        );
        let mut txn = txm.begin().unwrap();
        txn.put_row(t, &row(1, None)).unwrap();
        let sp = txn.savepoint();
        txn.put_row(t, &row(2, None)).unwrap();
        txn.put_row(t, &row(3, None)).unwrap();
        assert!(txn.put_row(t, &row(4, None)).is_err()); // the cap of 3 holds
        txn.rollback_to(sp);
        // the two rolled-back ops no longer count against the cap
        txn.put_row(t, &row(5, None)).unwrap();
        txn.put_row(t, &row(6, None)).unwrap();
        txn.commit().unwrap();
        assert_eq!(ids(&f, t), vec![1, 5, 6]);
        f.cleanup();
    }

    #[test]
    fn savepoints_nest_and_release_keeps_the_writes() {
        let f = Fixture::new("sp_nest");
        let t = f.table("t");
        let mut txn = f.txm.begin().unwrap();
        let outer = txn.savepoint();
        txn.put_row(t, &row(1, None)).unwrap();
        let inner = txn.savepoint();
        txn.put_row(t, &row(2, None)).unwrap();
        txn.rollback_to(inner); // only row 2 goes
        txn.put_row(t, &row(3, None)).unwrap();
        txn.release_savepoint(outer); // keeps rows 1 and 3
        txn.commit().unwrap();
        assert_eq!(ids(&f, t), vec![1, 3]);
        f.cleanup();
    }

    #[test]
    fn a_rolled_back_savepoint_changes_what_the_transaction_scans() {
        let f = Fixture::new("sp_overlay");
        let t = f.table("t");
        let mut txn = f.txm.begin().unwrap();
        txn.put_row(t, &row(1, None)).unwrap();
        // build (and cache) the overlay, then roll a write back
        assert_eq!(txn.overlay_for(t).unwrap().len(), 1);
        let sp = txn.savepoint();
        txn.put_row(t, &row(2, None)).unwrap();
        assert_eq!(txn.overlay_for(t).unwrap().len(), 2);
        txn.rollback_to(sp);
        assert_eq!(txn.overlay_for(t).unwrap().len(), 1);
        f.cleanup();
    }
}

// =======================================================================
// Versioned row decode (ADD COLUMN)
// =======================================================================

mod versioned_decode {
    use super::*;

    fn column(name: &str, default: Option<i32>, nullable: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            data_type: TYPE_TAG_INTEGER,
            nullable,
            default_value: default.map(|d| {
                crate::relational::value::encode_row(1, &[Some(RelationalValue::Integer(d))])
            }),
            type_params: None,
        }
    }

    #[test]
    fn old_rows_read_the_backfill_and_new_rows_their_own_value() {
        let f = Fixture::new("vd_basic");
        let t = f.table("t");
        f.store.put_row(t, &row(1, Some(7))).unwrap();
        let ord = f.builder.alter_add_column(t, &column("score", Some(5), true)).unwrap();
        assert_eq!(ord, 3);
        // an old row is not rewritten: it reads the backfill
        let mut expected = row(1, Some(7));
        expected.push(int(5));
        assert_eq!(f.store.get_row(t, &pk(1)).unwrap(), Some(expected));
        // a row written afterwards stores its own value, including NULL
        let mut r2 = row(2, None);
        r2.push(None);
        f.store.put_row(t, &r2).unwrap();
        assert_eq!(f.store.get_row(t, &pk(2)).unwrap(), Some(r2));
        // the schema version moved
        assert_eq!(f.catalog.get_table(t).unwrap().unwrap().schema_version, 2);
        f.cleanup();
    }

    #[test]
    fn set_default_does_not_change_what_old_rows_read() {
        let f = Fixture::new("vd_set_default");
        let t = f.table("t");
        f.store.put_row(t, &row(1, None)).unwrap();
        f.builder.alter_add_column(t, &column("score", Some(5), true)).unwrap();
        let ordinal = 3u16;
        let new_default =
            crate::relational::value::encode_row(1, &[Some(RelationalValue::Integer(99))]);
        f.builder
            .alter_set_default(t, ordinal, Some(new_default))
            .unwrap();
        assert_eq!(f.store.get_row(t, &pk(1)).unwrap().unwrap()[3], int(5));
        f.builder.alter_set_default(t, ordinal, None).unwrap();
        assert_eq!(f.store.get_row(t, &pk(1)).unwrap().unwrap()[3], int(5));
        f.cleanup();
    }

    #[test]
    fn a_column_added_without_a_default_reads_null_for_old_rows() {
        let f = Fixture::new("vd_null");
        let t = f.table("t");
        f.store.put_row(t, &row(1, None)).unwrap();
        f.builder.alter_add_column(t, &column("score", None, true)).unwrap();
        assert_eq!(f.store.get_row(t, &pk(1)).unwrap().unwrap()[3], None);
        f.cleanup();
    }

    #[test]
    fn several_columns_added_over_time_decode_every_generation() {
        let f = Fixture::new("vd_generations");
        let t = f.table("t");
        f.store.put_row(t, &row(1, None)).unwrap(); // version 1
        f.builder.alter_add_column(t, &column("a", Some(10), true)).unwrap();
        let mut r2 = row(2, None);
        r2.push(int(20));
        f.store.put_row(t, &r2).unwrap(); // version 2
        f.builder.alter_add_column(t, &column("b", Some(30), true)).unwrap();
        let mut r3 = row(3, None);
        r3.extend([int(40), int(50)]);
        f.store.put_row(t, &r3).unwrap(); // version 3
        let rows: Vec<_> = f.store.scan_table(t).unwrap().into_iter().map(|(_, r)| r).collect();
        assert_eq!(rows[0][3..], [int(10), int(30)]); // v1 row: both from backfill
        assert_eq!(rows[1][3..], [int(20), int(30)]); // v2 row: a stored, b backfilled
        assert_eq!(rows[2][3..], [int(40), int(50)]); // v3 row: both stored
        f.cleanup();
    }

    #[test]
    fn a_not_null_column_needs_a_default_unless_the_table_is_empty() {
        let f = Fixture::new("vd_not_null");
        let t = f.table("t");
        // empty table: allowed without a default
        f.builder.alter_add_column(t, &column("a", None, false)).unwrap();
        let mut r = row(1, None);
        r.push(int(1));
        f.store.put_row(t, &r).unwrap();
        // table with rows: refused, and nothing changes
        let version = f.catalog.get_table(t).unwrap().unwrap().schema_version;
        let columns_before = f.catalog.get_columns(t).unwrap();
        let err = f.builder.alter_add_column(t, &column("b", None, false)).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }), "{err:?}");
        assert_eq!(f.catalog.get_table(t).unwrap().unwrap().schema_version, version);
        assert_eq!(f.catalog.get_columns(t).unwrap(), columns_before);
        // with a default it is allowed on a non-empty table
        f.builder.alter_add_column(t, &column("c", Some(0), false)).unwrap();
        f.cleanup();
    }

    #[test]
    fn an_index_built_after_add_column_indexes_the_backfill_value() {
        let f = Fixture::new("vd_index");
        let t = f.table("t");
        f.store.put_row(t, &row(1, None)).unwrap();
        f.store.put_row(t, &row(2, None)).unwrap();
        f.builder.alter_add_column(t, &column("score", Some(5), true)).unwrap();
        let idx = f
            .builder
            .create_index_online(t, "score_idx", IndexKind::NonUnique, &[3])
            .unwrap();
        let found = f
            .builder
            .index_lookup_as_of(
                idx,
                &[Some(RelationalValue::Integer(5))],
                u64::MAX,
            )
            .unwrap();
        assert_eq!(found.len(), 2);
        // a unique index over the backfilled column finds the duplicates, and the
        // ALTER-shaped entry point leaves no index row behind
        let err = f.builder.alter_add_unique(t, "score_uq", &[3]).unwrap_err();
        assert!(matches!(err, RelationalError::InvalidInput { .. }), "{err:?}");
        assert!(f
            .catalog
            .list_indexes(t)
            .unwrap()
            .iter()
            .all(|i| i.name != "score_uq"));
        f.cleanup();
    }

    #[test]
    fn an_old_version_one_catalog_row_still_decodes() {
        // A `system.columns` row exactly as the catalog wrote it before ALTER TABLE
        // existed (six fields, envelope version 1) must read back with no backfill.
        let f = Fixture::new("vd_compat");
        let t = f.table("t");
        let cols = f.catalog.get_columns(t).unwrap();
        let mut as_v1 = cols[1].clone();
        as_v1.backfill_value = None;
        let v1_fields = as_v1.to_fields()[..crate::catalog::schema::COLUMNS_V1_FIELDS].to_vec();
        let bytes = crate::catalog::encoding::encode_row(1, &v1_fields);
        let mut key = vec![0u8];
        key.extend_from_slice(&crate::catalog::encoding::SYSTEM_TABLE_COLUMNS.to_be_bytes());
        key.extend_from_slice(&t.to_be_bytes());
        key.extend_from_slice(&1u16.to_be_bytes());
        f.engine
            .write_batch(&[crate::lsm::WriteOp::Put { key, value: bytes }])
            .unwrap();
        assert_eq!(f.catalog.get_columns(t).unwrap()[1], as_v1);
        f.cleanup();
    }

    #[test]
    fn schema_changes_survive_a_restart() {
        let f = Fixture::new("vd_restart");
        let t = f.table("t");
        f.store.put_row(t, &row(1, None)).unwrap();
        f.builder.alter_add_column(t, &column("score", Some(5), true)).unwrap();
        f.builder.alter_rename_column(t, 3, "points").unwrap();
        let f = f.restart();
        let columns = f.catalog.get_columns(t).unwrap();
        assert_eq!(columns[3].name, "points");
        assert_eq!(f.store.get_row(t, &pk(1)).unwrap().unwrap()[3], int(5));
        f.cleanup();
    }

    #[test]
    fn a_transaction_that_wrote_before_an_alter_conflicts_at_commit() {
        let f = Fixture::new("vd_txn_conflict");
        let t = f.table("t");
        let mut txn = f.txm.begin().unwrap();
        txn.put_row(t, &row(1, None)).unwrap(); // old shape
        f.builder.alter_add_column(t, &column("score", Some(5), true)).unwrap();
        let err = txn.commit().unwrap_err();
        assert!(matches!(err, RelationalError::Conflict { .. }), "{err:?}");
        assert!(ids(&f, t).is_empty()); // nothing was written
        // a transaction that starts after the ALTER is fine
        let mut txn = f.txm.begin().unwrap();
        let mut r = row(1, None);
        r.push(int(9));
        txn.put_row(t, &r).unwrap();
        txn.commit().unwrap();
        f.cleanup();
    }
}

// =======================================================================
// Foreign keys at commit: the concurrency argument
// =======================================================================

mod commit_validation {
    use super::*;

    fn setup(tag: &str, on_delete: FkAction) -> (Fixture, u32, u32) {
        let f = Fixture::new(tag);
        let parent = f.table("parent");
        let child = f.table("child");
        f.builder
            .alter_add_foreign_key(&f.fk(child, parent, on_delete))
            .unwrap();
        f.store.put_row(parent, &row(1, None)).unwrap();
        f.store.put_row(parent, &row(2, None)).unwrap();
        (f, parent, child)
    }

    #[test]
    fn a_child_row_without_a_parent_is_refused_at_commit() {
        let (f, _parent, child) = setup("cv_missing", FkAction::NoAction);
        let mut txn = f.txm.begin().unwrap();
        txn.put_row(child, &row(10, Some(99))).unwrap();
        let err = txn.commit().unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert!(ids(&f, child).is_empty());
        f.cleanup();
    }

    #[test]
    fn a_null_reference_is_never_checked() {
        let (f, _parent, child) = setup("cv_null", FkAction::NoAction);
        let mut txn = f.txm.begin().unwrap();
        txn.put_row(child, &row(10, None)).unwrap();
        txn.commit().unwrap();
        assert_eq!(ids(&f, child), vec![10]);
        f.cleanup();
    }

    #[test]
    fn parent_and_child_inserted_in_one_transaction_commit_together() {
        let (f, parent, child) = setup("cv_both", FkAction::NoAction);
        let mut txn = f.txm.begin().unwrap();
        txn.put_row(child, &row(10, Some(50))).unwrap();
        txn.put_row(parent, &row(50, None)).unwrap();
        txn.commit().unwrap();
        assert_eq!(ids(&f, child), vec![10]);
        f.cleanup();
    }

    #[test]
    fn deleting_a_referenced_parent_is_refused_at_commit() {
        let (f, parent, child) = setup("cv_restrict", FkAction::NoAction);
        f.store.put_row(child, &row(10, Some(1))).unwrap();
        let mut txn = f.txm.begin().unwrap();
        txn.delete_row(parent, &pk(1)).unwrap();
        let err = txn.commit().unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert_eq!(ids(&f, parent), vec![1, 2]);
        f.cleanup();
    }

    #[test]
    fn deleting_the_parent_and_its_child_together_commits() {
        let (f, parent, child) = setup("cv_both_delete", FkAction::NoAction);
        f.store.put_row(child, &row(10, Some(1))).unwrap();
        let mut txn = f.txm.begin().unwrap();
        txn.delete_row(child, &pk(10)).unwrap();
        txn.delete_row(parent, &pk(1)).unwrap();
        txn.commit().unwrap();
        assert_eq!(ids(&f, parent), vec![2]);
        assert!(ids(&f, child).is_empty());
        f.cleanup();
    }

    /// Child-before-parent: the child's transaction read the parent in its
    /// snapshot, then the parent was deleted and that delete committed first.
    #[test]
    fn child_insert_loses_to_a_parent_delete_that_committed_first() {
        let (f, parent, child) = setup("cv_race_cp", FkAction::NoAction);
        let mut child_txn = f.txm.begin().unwrap(); // snapshot sees parent 1
        child_txn.put_row(child, &row(10, Some(1))).unwrap();
        // the parent is deleted and the delete commits
        let mut del = f.txm.begin().unwrap();
        del.delete_row(parent, &pk(1)).unwrap();
        del.commit().unwrap();
        let err = child_txn.commit().unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert!(ids(&f, child).is_empty());
        f.cleanup();
    }

    /// Parent-before-child: the child committed first; the parent's delete, whose
    /// snapshot predates it, must not remove a parent that is now referenced.
    #[test]
    fn parent_delete_loses_to_a_child_insert_that_committed_first() {
        let (f, parent, child) = setup("cv_race_pc", FkAction::NoAction);
        let mut del = f.txm.begin().unwrap(); // snapshot sees no child of 1
        del.delete_row(parent, &pk(1)).unwrap();
        let mut ins = f.txm.begin().unwrap();
        ins.put_row(child, &row(10, Some(1))).unwrap();
        ins.commit().unwrap();
        let err = del.commit().unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert_eq!(ids(&f, parent), vec![1, 2]);
        assert_eq!(ids(&f, child), vec![10]);
        f.cleanup();
    }

    /// Changing the referenced value of a parent is also a vacate.
    #[test]
    fn an_unrelated_parent_write_and_child_insert_both_commit() {
        let (f, parent, child) = setup("cv_unrelated", FkAction::NoAction);
        let mut a = f.txm.begin().unwrap();
        a.put_row(parent, &row(2, Some(5))).unwrap(); // touches parent 2's other column
        let mut b = f.txm.begin().unwrap();
        b.put_row(child, &row(10, Some(1))).unwrap();
        b.commit().unwrap();
        a.commit().unwrap();
        f.cleanup();
    }

    /// Many rounds of the two orderings racing from a barrier. Whatever interleaving
    /// happens, the result must hold the invariant: no child references a missing
    /// parent, and exactly one of the two transactions won.
    #[test]
    fn racing_child_insert_and_parent_delete_never_leave_an_orphan() {
        let (f, parent, child) = setup("cv_stress", FkAction::NoAction);
        let f = Arc::new(f);
        let rounds = 60;
        for round in 0..rounds {
            let key = 100 + round;
            f.store.put_row(parent, &row(key, None)).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let (fa, ba) = (Arc::clone(&f), Arc::clone(&barrier));
            let inserter = thread::spawn(move || {
                let mut txn = fa.txm.begin().unwrap();
                txn.put_row(child, &row(1000 + key, Some(key))).unwrap();
                ba.wait();
                txn.commit()
            });
            let (fb, bb) = (Arc::clone(&f), Arc::clone(&barrier));
            let deleter = thread::spawn(move || {
                let mut txn = fb.txm.begin().unwrap();
                txn.delete_row(parent, &pk(key)).unwrap();
                bb.wait();
                txn.commit()
            });
            let ins = inserter.join().unwrap();
            let del = deleter.join().unwrap();
            // at most one of them can have committed...
            assert!(
                ins.is_err() || del.is_err(),
                "round {round}: both committed (child {ins:?}, delete {del:?})"
            );
            // ... and no orphan exists.
            let parents = ids(&f, parent);
            for c in f.store.scan_table(child).unwrap() {
                if let Some(RelationalValue::Integer(p)) = &c.1[1] {
                    assert!(parents.contains(p), "round {round}: orphan child references {p}");
                }
            }
            // a failure is a foreign-key violation (never anything else)
            for r in [&ins, &del] {
                if let Err(e) = r {
                    assert!(is_fk_violation(e), "round {round}: {e:?}");
                }
            }
        }
    }

    /// Children of one parent, committed from many threads at once, share the parent
    /// (read) lock and never deadlock or fail.
    #[test]
    fn many_concurrent_child_inserts_on_one_parent_all_commit() {
        let (f, _parent, child) = setup("cv_many_children", FkAction::NoAction);
        let f = Arc::new(f);
        let handles: Vec<_> = (0..8)
            .map(|w| {
                let f = Arc::clone(&f);
                thread::spawn(move || {
                    for i in 0..25 {
                        let mut txn = f.txm.begin().unwrap();
                        txn.put_row(child, &row(w * 1000 + i, Some(1))).unwrap();
                        txn.commit().unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(ids(&f, child).len(), 200);
        Arc::try_unwrap(f).ok().unwrap().cleanup();
    }

    /// Locks are taken in table-id order whichever side a table is on, so two
    /// foreign keys pointing in opposite directions cannot deadlock.
    #[test]
    fn foreign_keys_in_opposite_directions_do_not_deadlock() {
        let f = Fixture::new("cv_cycle");
        let a = f.table("a");
        let b = f.table("b");
        f.builder
            .alter_add_foreign_key(&f.fk(a, b, FkAction::NoAction))
            .unwrap();
        f.builder
            .alter_add_foreign_key(&f.fk(b, a, FkAction::NoAction))
            .unwrap();
        // each table holds one NULL-referencing row to start from
        f.store.put_row(a, &row(1, None)).unwrap();
        f.store.put_row(b, &row(1, None)).unwrap();
        let f = Arc::new(f);
        let handles: Vec<_> = (0..4)
            .map(|w| {
                let f = Arc::clone(&f);
                thread::spawn(move || {
                    for i in 0..30 {
                        let mut txn = f.txm.begin().unwrap();
                        let (t, other) = if w % 2 == 0 { (a, b) } else { (b, a) };
                        let _ = other;
                        txn.put_row(t, &row(100 + w * 100 + i, Some(1))).unwrap();
                        // a conflict or violation is acceptable; a hang is not
                        let _ = txn.commit();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        Arc::try_unwrap(f).ok().unwrap().cleanup();
    }

    /// `ADD FOREIGN KEY` racing child inserts: afterwards, if the key exists, there
    /// is no orphan.
    #[test]
    fn adding_a_foreign_key_while_children_are_inserted_leaves_no_orphan() {
        for round in 0..15 {
            let f = Fixture::new(&format!("cv_add_race_{round}"));
            let parent = f.table("parent");
            let child = f.table("child");
            f.store.put_row(parent, &row(1, None)).unwrap();
            let f = Arc::new(f);
            let barrier = Arc::new(Barrier::new(2));
            let (fa, ba) = (Arc::clone(&f), Arc::clone(&barrier));
            let writer = thread::spawn(move || {
                let mut results = Vec::new();
                ba.wait();
                for i in 0..40 {
                    let mut txn = fa.txm.begin().unwrap();
                    // every third row references a parent that does not exist
                    let reference = if i % 3 == 0 { 999 } else { 1 };
                    txn.put_row(child, &row(i, Some(reference))).unwrap();
                    results.push(txn.commit().is_ok());
                }
                results
            });
            let (fb, bb) = (Arc::clone(&f), Arc::clone(&barrier));
            let alter = thread::spawn(move || {
                bb.wait();
                fb.builder
                    .alter_add_foreign_key(&fb.fk(child, parent, FkAction::NoAction))
            });
            writer.join().unwrap();
            let added = alter.join().unwrap();
            let parents = ids(&f, parent);
            let orphans = f
                .store
                .scan_table(child)
                .unwrap()
                .into_iter()
                .filter(|(_, r)| matches!(&r[1], Some(RelationalValue::Integer(p)) if !parents.contains(p)))
                .count();
            match added {
                // The key was added: no orphan may exist, whether the row was written
                // before (and validated by the scan) or after (and checked at commit).
                Ok(_) => assert_eq!(orphans, 0, "round {round}: key added but {orphans} orphan(s)"),
                // The scan found an orphan already written: refused, nothing recorded.
                Err(e) => {
                    assert!(is_fk_violation(&e), "round {round}: {e:?}");
                    assert!(f.catalog.foreign_keys().unwrap().is_empty());
                }
            }
            let f = Arc::try_unwrap(f).ok().unwrap();
            f.cleanup();
        }
    }
}

// =======================================================================
// Statement-time enforcement and cascades (relational API)
// =======================================================================

mod enforcement {
    use super::*;

    #[test]
    fn enforce_cascades_a_delete_and_a_set_null() {
        let f = Fixture::new("en_cascade");
        let parent = f.table("parent");
        let c_cascade = f.table("c_cascade");
        let c_null = f.table("c_null");
        f.builder
            .alter_add_foreign_key(&f.fk(c_cascade, parent, FkAction::Cascade))
            .unwrap();
        f.builder
            .alter_add_foreign_key(&f.fk(c_null, parent, FkAction::SetNull))
            .unwrap();
        f.store.put_row(parent, &row(1, None)).unwrap();
        f.store.put_row(c_cascade, &row(10, Some(1))).unwrap();
        f.store.put_row(c_null, &row(20, Some(1))).unwrap();

        let mut txn = f.txm.begin().unwrap();
        let old = txn.get_row(parent, &pk(1)).unwrap().unwrap();
        txn.delete_row(parent, &pk(1)).unwrap();
        txn.enforce_foreign_keys(
            parent,
            &[RowChange {
                old: Some(old),
                new: None,
            }],
        )
        .unwrap();
        txn.commit().unwrap();
        assert!(ids(&f, parent).is_empty());
        assert!(ids(&f, c_cascade).is_empty());
        assert_eq!(f.store.get_row(c_null, &pk(20)).unwrap(), Some(row(20, None)));
        f.cleanup();
    }

    #[test]
    fn a_cascade_past_the_write_set_cap_fails_closed_and_leaves_nothing() {
        let f = Fixture::new("en_cap");
        let parent = f.table("parent");
        let child = f.table("child");
        f.builder
            .alter_add_foreign_key(&f.fk(child, parent, FkAction::Cascade))
            .unwrap();
        f.store.put_row(parent, &row(1, None)).unwrap();
        for i in 0..20 {
            f.store.put_row(child, &row(i, Some(1))).unwrap();
        }
        let txm = TransactionManager::with_limits(
            Arc::clone(&f.engine),
            Arc::clone(&f.store),
            crate::relational::TxnLimits {
                max_write_set_ops: 10,
                ..Default::default()
            },
        );
        let mut txn = txm.begin().unwrap();
        let sp = txn.savepoint();
        let old = txn.get_row(parent, &pk(1)).unwrap().unwrap();
        txn.delete_row(parent, &pk(1)).unwrap();
        let err = txn
            .enforce_foreign_keys(
                parent,
                &[RowChange {
                    old: Some(old),
                    new: None,
                }],
            )
            .unwrap_err();
        assert!(matches!(err, RelationalError::ResourceLimit { .. }), "{err:?}");
        txn.rollback_to(sp);
        txn.commit().unwrap(); // an empty write-set
        assert_eq!(ids(&f, parent), vec![1]);
        assert_eq!(ids(&f, child).len(), 20);
        f.cleanup();
    }

    #[test]
    fn a_self_referencing_chain_cascades_to_the_bottom_and_stops_at_the_depth_limit() {
        let f = Fixture::new("en_self");
        let t = f.table("tree");
        // ref -> id of the same table
        f.builder
            .alter_add_foreign_key(&f.fk(t, t, FkAction::Cascade))
            .unwrap();
        f.store.put_row(t, &row(1, None)).unwrap();
        for i in 2..=10 {
            f.store.put_row(t, &row(i, Some(i - 1))).unwrap();
        }
        let mut txn = f.txm.begin().unwrap();
        let old = txn.get_row(t, &pk(1)).unwrap().unwrap();
        txn.delete_row(t, &pk(1)).unwrap();
        txn.enforce_foreign_keys(
            t,
            &[RowChange {
                old: Some(old),
                new: None,
            }],
        )
        .unwrap();
        txn.commit().unwrap();
        assert!(ids(&f, t).is_empty());

        // a chain deeper than MAX_CASCADE_DEPTH fails closed
        let depth = crate::relational::txn::MAX_CASCADE_DEPTH as i32 + 5;
        f.store.put_row(t, &row(1, None)).unwrap();
        for i in 2..=depth {
            f.store.put_row(t, &row(i, Some(i - 1))).unwrap();
        }
        let mut txn = f.txm.begin().unwrap();
        let old = txn.get_row(t, &pk(1)).unwrap().unwrap();
        txn.delete_row(t, &pk(1)).unwrap();
        let err = txn
            .enforce_foreign_keys(
                t,
                &[RowChange {
                    old: Some(old),
                    new: None,
                }],
            )
            .unwrap_err();
        assert!(matches!(err, RelationalError::ResourceLimit { .. }), "{err:?}");
        f.cleanup();
    }
}

// =======================================================================
// Row lookup access paths
// =======================================================================

mod lookup_paths {
    use super::*;

    #[test]
    fn every_access_path_finds_the_same_rows() {
        let f = Fixture::new("lk_paths");
        // composite primary key (a, b); a secondary index on c; plain column d
        let col = |n: &str, nullable: bool| ColumnDef {
            name: n.to_string(),
            data_type: TYPE_TAG_INTEGER,
            nullable,
            default_value: None,
            type_params: None,
        };
        let t = f
            .catalog
            .create_table(
                f.schema_id,
                "t",
                &[col("a", false), col("b", false), col("c", true), col("d", true)],
                &[0, 1],
            )
            .unwrap();
        f.builder
            .create_index_online(t, "t_c", IndexKind::NonUnique, &[2])
            .unwrap();
        for a in 0..4 {
            for b in 0..4 {
                f.store
                    .put_row(t, &[int(a), int(b), int(a * 10 + b), int(a % 2)])
                    .unwrap();
            }
        }
        let (table, _) = f.store.resolve_table(t).unwrap();
        let ints = |v: &[i32]| v.iter().map(|n| RelationalValue::Integer(*n)).collect::<Vec<_>>();

        // PK point, in either column order
        assert!(matches!(f.store.plan_access(&table, &[0, 1]).unwrap(), Access::PkPoint));
        assert!(matches!(f.store.plan_access(&table, &[1, 0]).unwrap(), Access::PkPoint));
        let point = f
            .store
            .find_rows_with_access(t, &[1, 0], &ints(&[2, 3]), &Access::PkPoint, u64::MAX, 10)
            .unwrap();
        assert_eq!(point.len(), 1);
        assert_eq!(point[0].1[0], int(3));
        assert_eq!(point[0].1[1], int(2));

        // PK prefix
        let prefix_access = f.store.plan_access(&table, &[0]).unwrap();
        assert!(matches!(prefix_access, Access::PkPrefix { len: 1 }));
        let prefix = f
            .store
            .find_rows_with_access(t, &[0], &ints(&[2]), &prefix_access, u64::MAX, 100)
            .unwrap();
        assert_eq!(prefix.len(), 4);
        assert!(prefix.iter().all(|(_, r)| r[0] == int(2)));

        // index
        let index_access = f.store.plan_access(&table, &[2]).unwrap();
        assert!(matches!(index_access, Access::Index(_)));
        let by_index = f
            .store
            .find_rows_with_access(t, &[2], &ints(&[23]), &index_access, u64::MAX, 100)
            .unwrap();
        assert_eq!(by_index.len(), 1);

        // scan
        let scan_access = f.store.plan_access(&table, &[3]).unwrap();
        assert!(matches!(scan_access, Access::Scan));
        let by_scan = f
            .store
            .find_rows_with_access(t, &[3], &ints(&[1]), &scan_access, u64::MAX, 100)
            .unwrap();
        assert_eq!(by_scan.len(), 8);

        // the limit is honoured
        let limited = f
            .store
            .find_rows_with_access(t, &[3], &ints(&[1]), &scan_access, u64::MAX, 3)
            .unwrap();
        assert_eq!(limited.len(), 3);
        f.cleanup();
    }
}
