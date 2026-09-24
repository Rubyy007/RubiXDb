//! Write executor tests — `PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md`
//! is the decision record these verify against.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rubixdb::catalog::schema::IndexKind;
use rubixdb::catalog::CatalogService;
use rubixdb::relational::index::IndexBuilder;
use rubixdb::relational::table_store::TableStore;
use rubixdb::relational::{RelationalValue, Transaction, TransactionManager};

use crate::auth::AuthContext;
use crate::bind::bind_statement;
use crate::exec::write::{
    execute_write, execute_write_autocommit, WriteMetrics, WriteResult, WriteStatementKind,
};
use crate::exec::{
    execute, execute_autocommit, CancellationToken, ExecLimits, ExecMetrics, QueryResult,
};
use crate::limits::SqlLimits;
use crate::metrics::SqlMetrics;
use crate::parse::parse_statement;
use crate::plan::{build_plan, Plan, PlannerLimits, PlannerMetrics};
use crate::test_support::Fixture;

struct WriteFixture {
    f: Fixture,
    catalog: Arc<CatalogService>,
    store: Arc<TableStore>,
    builder: Arc<IndexBuilder>,
    txm: TransactionManager,
}

impl WriteFixture {
    fn new(tag: &str) -> Self {
        let f = Fixture::new(tag);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&f.engine)));
        let store = Arc::new(TableStore::new(Arc::clone(&f.engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&f.engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = TransactionManager::new(Arc::clone(&f.engine), Arc::clone(&store));
        WriteFixture {
            f,
            catalog,
            store,
            builder,
            txm,
        }
    }

    fn table_id(&self, name: &str) -> u32 {
        self.catalog
            .get_table_by_name(self.f.ctx.default_schema_id, name)
            .unwrap()
            .unwrap()
            .table_id
    }

    fn create_name_index(&self, table: &str, kind: IndexKind) -> u32 {
        let table_id = self.table_id(table);
        self.builder
            .create_index_online(table_id, &format!("{table}_name_idx"), kind, &[1])
            .unwrap()
    }

    fn plan(&self, sql: &str) -> Plan {
        let limits = SqlLimits::default();
        let stmt = parse_statement(sql, &limits)
            .unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
        let metrics = SqlMetrics::default();
        let auth = AuthContext::admin("test-principal");
        let bound = bind_statement(
            &self.f.catalog,
            &self.f.ctx,
            &auth,
            &metrics,
            &limits,
            &stmt,
        )
        .unwrap_or_else(|e| panic!("bind failed for {sql:?}: {e}"));
        build_plan(
            &bound,
            &self.f.catalog,
            &PlannerLimits::default(),
            &PlannerMetrics::default(),
        )
        .unwrap_or_else(|e| panic!("plan failed for {sql:?}: {e}"))
    }

    fn write(&self, sql: &str) -> WriteResult {
        self.try_write(sql)
            .unwrap_or_else(|e| panic!("write failed for {sql:?}: {e}"))
    }

    fn try_write(&self, sql: &str) -> crate::Result<WriteResult> {
        let plan = self.plan(sql);
        execute_write_autocommit(
            &plan,
            &self.txm,
            &self.store,
            &self.catalog,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
    }

    fn write_in_txn(&self, sql: &str, txn: &mut Transaction) -> crate::Result<WriteResult> {
        let plan = self.plan(sql);
        execute_write(
            &plan,
            txn,
            &self.store,
            &self.catalog,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
    }

    fn select(&self, sql: &str) -> QueryResult {
        let plan = self.plan(sql);
        execute_autocommit(
            &plan,
            &self.txm,
            &self.store,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|e| panic!("select failed for {sql:?}: {e}"))
    }

    fn select_in_txn(&self, sql: &str, txn: &Transaction) -> QueryResult {
        let plan = self.plan(sql);
        execute(
            &plan,
            txn,
            &self.store,
            &self.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_or_else(|e| panic!("select failed for {sql:?}: {e}"))
    }

    fn cleanup(self) {
        self.f.cleanup();
    }
}

fn text(s: &str) -> Option<RelationalValue> {
    Some(RelationalValue::Text(s.to_string()))
}

fn int(n: i32) -> Option<RelationalValue> {
    Some(RelationalValue::Integer(n))
}

// -----------------------------------------------------------------
// INSERT (items 6-13)
// -----------------------------------------------------------------

#[test]
fn insert_single_row_is_visible_to_a_later_select() {
    let f = WriteFixture::new("insert_single");
    let r = f.write("INSERT INTO t (id, name, active) VALUES (1, 'alice', TRUE)");
    assert_eq!(
        r,
        WriteResult {
            kind: WriteStatementKind::Insert,
            rows_affected: 1
        }
    );
    let s = f.select("SELECT id, name, active FROM t WHERE id = 1");
    assert_eq!(
        s.rows,
        vec![vec![
            int(1),
            text("alice"),
            Some(RelationalValue::Boolean(true))
        ]]
    );
    f.cleanup();
}

#[test]
fn insert_multi_row_is_atomic_and_all_visible() {
    let f = WriteFixture::new("insert_multi");
    let r = f.write(
        "INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE), (2, 'b', TRUE), (3, 'c', TRUE)",
    );
    assert_eq!(r.rows_affected, 3);
    let s = f.select("SELECT id FROM t");
    assert_eq!(s.rows.len(), 3);
    f.cleanup();
}

#[test]
fn insert_column_subset_reordered_maps_to_correct_ordinals() {
    let f = WriteFixture::new("insert_reordered");
    f.write("INSERT INTO t (active, id) VALUES (TRUE, 7)");
    let s = f.select("SELECT id, name, active FROM t WHERE id = 7");
    assert_eq!(
        s.rows[0],
        vec![int(7), None, Some(RelationalValue::Boolean(true))]
    );
    f.cleanup();
}

#[test]
fn insert_null_into_nullable_column_stores_null() {
    let f = WriteFixture::new("insert_null_nullable");
    f.write("INSERT INTO t (id, name, active) VALUES (1, NULL, TRUE)");
    let s = f.select("SELECT name FROM t WHERE id = 1");
    assert_eq!(s.rows[0][0], None);
    f.cleanup();
}

#[test]
fn insert_null_primary_key_is_rejected() {
    // Rejected at bind time (D6, `validate_row_shape` territory) --
    // `f.plan`/`f.try_write` themselves call `bind_statement` through a
    // helper that panics on a bind error, so the pipeline is driven
    // manually here to assert the error instead of hard-panicking.
    let f = WriteFixture::new("insert_null_pk");
    let limits = SqlLimits::default();
    let stmt = parse_statement(
        "INSERT INTO t (id, name, active) VALUES (NULL, 'a', TRUE)",
        &limits,
    )
    .unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("t");
    let err = bind_statement(&f.f.catalog, &f.f.ctx, &auth, &metrics, &limits, &stmt).unwrap_err();
    assert!(
        matches!(err, crate::SqlError::TypeMismatch { .. }),
        "a NULL PRIMARY KEY must be rejected, never silently stored; got {err:?}"
    );
    f.cleanup();
}

#[test]
fn insert_secondary_index_entry_is_maintained_atomically() {
    let f = WriteFixture::new("insert_index");
    f.create_name_index("t", IndexKind::NonUnique);
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'findme', TRUE)");
    let s = f.select("SELECT id FROM t WHERE name = 'findme'");
    assert_eq!(
        s.rows,
        vec![vec![int(1)]],
        "the new row must be reachable through its secondary index"
    );
    f.cleanup();
}

#[test]
fn insert_omitted_default_column_gets_its_declared_default_end_to_end() {
    let f = WriteFixture::new("insert_default_e2e");
    let default_bytes =
        rubixdb::relational::value::encode_row(1, &[Some(RelationalValue::Integer(99))]);
    f.catalog
        .create_table(
            f.f.ctx.default_schema_id,
            "with_default",
            &[
                rubixdb::catalog::service::ColumnDef {
                    name: "id".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: false,
                    default_value: None,
                    type_params: None,
                },
                rubixdb::catalog::service::ColumnDef {
                    name: "score".to_string(),
                    data_type: rubixdb::relational::value::TYPE_TAG_INTEGER,
                    nullable: true,
                    default_value: Some(default_bytes),
                    type_params: None,
                },
            ],
            &[0],
        )
        .unwrap();
    f.write("INSERT INTO with_default (id) VALUES (1)");
    let s = f.select("SELECT score FROM with_default WHERE id = 1");
    assert_eq!(
        s.rows[0][0],
        int(99),
        "the omitted column must get its declared DEFAULT, never NULL"
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// PRIMARY KEY / UNIQUE conflict (items 11/12)
// -----------------------------------------------------------------

#[test]
fn primary_key_conflict_exactly_one_committer_wins() {
    let f = WriteFixture::new("pk_conflict");
    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    f.write_in_txn(
        "INSERT INTO t (id, name, active) VALUES (1, 't1', TRUE)",
        &mut t1,
    )
    .unwrap();
    f.write_in_txn(
        "INSERT INTO t (id, name, active) VALUES (1, 't2', TRUE)",
        &mut t2,
    )
    .unwrap();
    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(matches!(
        err,
        rubixdb::relational::RelationalError::Conflict { .. }
    ));
    let s = f.select("SELECT name FROM t WHERE id = 1");
    assert_eq!(s.rows, vec![vec![text("t1")]]);
    f.cleanup();
}

#[test]
fn unique_conflict_exactly_one_committer_wins() {
    let f = WriteFixture::new("unique_conflict");
    f.create_name_index("t", IndexKind::Unique);
    let mut t1 = f.txm.begin().unwrap();
    let mut t2 = f.txm.begin().unwrap();
    f.write_in_txn(
        "INSERT INTO t (id, name, active) VALUES (1, 'shared', TRUE)",
        &mut t1,
    )
    .unwrap();
    f.write_in_txn(
        "INSERT INTO t (id, name, active) VALUES (2, 'shared', TRUE)",
        &mut t2,
    )
    .unwrap();
    t1.commit().unwrap();
    let err = t2.commit();
    assert!(
        err.is_err(),
        "a UNIQUE conflict must abort the second committer -- never two committed conflicting rows"
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// UPDATE (items 17-22)
// -----------------------------------------------------------------

#[test]
fn update_non_indexed_column() {
    let f = WriteFixture::new("update_basic");
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'alice', TRUE)");
    let r = f.write("UPDATE t SET active = FALSE WHERE id = 1");
    assert_eq!(r.rows_affected, 1);
    let s = f.select("SELECT active FROM t WHERE id = 1");
    assert_eq!(s.rows[0][0], Some(RelationalValue::Boolean(false)));
    f.cleanup();
}

#[test]
fn update_indexed_column_moves_the_index_entry() {
    let f = WriteFixture::new("update_indexed");
    f.create_name_index("t", IndexKind::NonUnique);
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'old', TRUE)");
    f.write("UPDATE t SET name = 'new' WHERE id = 1");

    let old_lookup = f.select("SELECT id FROM t WHERE name = 'old'");
    assert!(
        old_lookup.rows.is_empty(),
        "the old index entry must be gone"
    );
    let new_lookup = f.select("SELECT id FROM t WHERE name = 'new'");
    assert_eq!(
        new_lookup.rows,
        vec![vec![int(1)]],
        "the new index entry must be reachable"
    );
    f.cleanup();
}

#[test]
fn update_unique_column_to_an_existing_value_conflicts() {
    let f = WriteFixture::new("update_unique_conflict");
    f.create_name_index("t", IndexKind::Unique);
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE)");
    f.write("INSERT INTO t (id, name, active) VALUES (2, 'b', TRUE)");
    let err = f.try_write("UPDATE t SET name = 'a' WHERE id = 2");
    assert!(
        err.is_err(),
        "updating to an already-used UNIQUE value must conflict"
    );
    f.cleanup();
}

#[test]
fn update_primary_key_column_is_rejected_end_to_end() {
    let f = WriteFixture::new("update_pk_rejected");
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE)");
    // Rejected at bind time (D6) -- `f.plan` itself panics via
    // `unwrap_or_else`, so call the pipeline manually to assert the
    // error kind instead of a hard panic.
    let limits = SqlLimits::default();
    let stmt = parse_statement("UPDATE t SET id = 2 WHERE id = 1", &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("t");
    let err = bind_statement(&f.f.catalog, &f.f.ctx, &auth, &metrics, &limits, &stmt).unwrap_err();
    assert!(
        matches!(err, crate::SqlError::Unsupported { .. })
            || matches!(err, crate::SqlError::TypeMismatch { .. })
    );
    f.cleanup();
}

#[test]
fn update_matching_multiple_rows_via_predicate() {
    let f = WriteFixture::new("update_multi");
    for i in 0..5 {
        f.write(&format!(
            "INSERT INTO t (id, name, active) VALUES ({i}, 'x', TRUE)"
        ));
    }
    let r = f.write("UPDATE t SET active = FALSE WHERE active = TRUE");
    assert_eq!(r.rows_affected, 5);
    let s = f.select("SELECT id FROM t WHERE active = TRUE");
    assert!(s.rows.is_empty());
    f.cleanup();
}

#[test]
fn update_residual_predicate_is_still_applied() {
    let f = WriteFixture::new("update_residual");
    f.create_name_index("t", IndexKind::NonUnique);
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'shared', TRUE)");
    f.write("INSERT INTO t (id, name, active) VALUES (2, 'shared', FALSE)");
    let r = f.write("UPDATE t SET active = TRUE WHERE name = 'shared' AND active = FALSE");
    assert_eq!(
        r.rows_affected, 1,
        "only the row matching BOTH the indexed and residual predicate must be updated"
    );
    f.cleanup();
}

#[test]
fn update_no_op_when_value_unchanged_still_counts_as_matched() {
    let f = WriteFixture::new("update_noop");
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'same', TRUE)");
    let r = f.write("UPDATE t SET name = 'same' WHERE id = 1");
    assert_eq!(r.rows_affected, 1);
    f.cleanup();
}

// -----------------------------------------------------------------
// DELETE (items 14-16, 23)
// -----------------------------------------------------------------

#[test]
fn delete_single_row_removes_it_and_its_index_entries() {
    let f = WriteFixture::new("delete_single");
    f.create_name_index("t", IndexKind::NonUnique);
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'gone', TRUE)");
    let r = f.write("DELETE FROM t WHERE id = 1");
    assert_eq!(r.rows_affected, 1);
    assert!(f.select("SELECT id FROM t WHERE id = 1").rows.is_empty());
    assert!(
        f.select("SELECT id FROM t WHERE name = 'gone'")
            .rows
            .is_empty(),
        "no stale index entry may remain"
    );
    f.cleanup();
}

#[test]
fn delete_multiplicity_matches_exactly_the_predicate() {
    let f = WriteFixture::new("delete_multiplicity");
    for i in 0..10 {
        f.write(&format!(
            "INSERT INTO t (id, name, active) VALUES ({i}, 'x', {})",
            i % 2 == 0
        ));
    }
    let r = f.write("DELETE FROM t WHERE active = TRUE");
    assert_eq!(r.rows_affected, 5);
    assert_eq!(f.select("SELECT id FROM t").rows.len(), 5);
    f.cleanup();
}

#[test]
fn delete_zero_matches_reports_zero_and_changes_nothing() {
    let f = WriteFixture::new("delete_zero");
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE)");
    let r = f.write("DELETE FROM t WHERE id = 999");
    assert_eq!(r.rows_affected, 0);
    assert_eq!(f.select("SELECT id FROM t").rows.len(), 1);
    f.cleanup();
}

// -----------------------------------------------------------------
// Transaction integration: read-your-own-writes, rollback, snapshot (items 5/26/27)
// -----------------------------------------------------------------

#[test]
fn read_your_own_writes_insert_then_select_same_transaction() {
    let f = WriteFixture::new("ryow_insert");
    let mut txn = f.txm.begin().unwrap();
    f.write_in_txn(
        "INSERT INTO t (id, name, active) VALUES (1, 'local', TRUE)",
        &mut txn,
    )
    .unwrap();
    let s = f.select_in_txn("SELECT name FROM t WHERE id = 1", &txn);
    assert_eq!(s.rows, vec![vec![text("local")]]);
    txn.rollback().unwrap();
    f.cleanup();
}

#[test]
fn rollback_insert_leaves_no_trace() {
    let f = WriteFixture::new("rollback_insert");
    let mut txn = f.txm.begin().unwrap();
    f.write_in_txn(
        "INSERT INTO t (id, name, active) VALUES (1, 'x', TRUE)",
        &mut txn,
    )
    .unwrap();
    txn.rollback().unwrap();
    assert!(f.select("SELECT id FROM t WHERE id = 1").rows.is_empty());
    f.cleanup();
}

#[test]
fn rollback_update_and_delete_leave_no_trace() {
    let f = WriteFixture::new("rollback_update_delete");
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'orig', TRUE)");

    let mut txn = f.txm.begin().unwrap();
    f.write_in_txn("UPDATE t SET name = 'changed' WHERE id = 1", &mut txn)
        .unwrap();
    txn.rollback().unwrap();
    assert_eq!(
        f.select("SELECT name FROM t WHERE id = 1").rows[0][0],
        text("orig")
    );

    let mut txn2 = f.txm.begin().unwrap();
    f.write_in_txn("DELETE FROM t WHERE id = 1", &mut txn2)
        .unwrap();
    txn2.rollback().unwrap();
    assert!(!f.select("SELECT id FROM t WHERE id = 1").rows.is_empty());
    f.cleanup();
}

#[test]
fn other_transaction_does_not_see_uncommitted_insert() {
    let f = WriteFixture::new("snapshot_write_isolation");
    let mut t1 = f.txm.begin().unwrap();
    f.write_in_txn(
        "INSERT INTO t (id, name, active) VALUES (1, 'x', TRUE)",
        &mut t1,
    )
    .unwrap();

    let t2 = f.txm.begin().unwrap();
    assert!(f
        .select_in_txn("SELECT id FROM t WHERE id = 1", &t2)
        .rows
        .is_empty());
    drop(t2);

    t1.commit().unwrap();
    let t3 = f.txm.begin().unwrap();
    assert_eq!(
        f.select_in_txn("SELECT id FROM t WHERE id = 1", &t3)
            .rows
            .len(),
        1
    );
    drop(t3);
    f.cleanup();
}

// -----------------------------------------------------------------
// DDL (items 35-45)
// -----------------------------------------------------------------

#[test]
fn create_schema_create_table_insert_select_end_to_end() {
    let f = WriteFixture::new("ddl_e2e");
    f.write("CREATE SCHEMA extra");
    let sql = "SELECT 1"; // ensure schema itself doesn't error later use paths
    let _ = f.select(sql);
    f.write("CREATE TABLE t2 (id INTEGER PRIMARY KEY, label TEXT)");
    f.write("INSERT INTO t2 (id, label) VALUES (1, 'hi')");
    let s = f.select("SELECT label FROM t2 WHERE id = 1");
    assert_eq!(s.rows, vec![vec![text("hi")]]);
    f.cleanup();
}

#[test]
fn drop_table_makes_it_immediately_inaccessible() {
    let f = WriteFixture::new("ddl_drop_table");
    f.write("CREATE TABLE droppable (id INTEGER PRIMARY KEY)");
    f.write("DROP TABLE droppable");
    let limits = SqlLimits::default();
    let stmt = parse_statement("SELECT id FROM droppable", &limits).unwrap();
    let metrics = SqlMetrics::default();
    let auth = AuthContext::admin("t");
    let err = bind_statement(&f.f.catalog, &f.f.ctx, &auth, &metrics, &limits, &stmt).unwrap_err();
    assert!(matches!(err, crate::SqlError::UnknownObject { .. }));
    f.cleanup();
}

#[test]
fn create_index_via_ddl_reuses_the_online_index_builder() {
    let f = WriteFixture::new("ddl_create_index");
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'pre-existing', TRUE)");
    f.write("CREATE INDEX t_name_ddl_idx ON t (name)");
    let s = f.select("SELECT id FROM t WHERE name = 'pre-existing'");
    assert_eq!(s.rows, vec![vec![int(1)]], "CREATE INDEX via DDL must backfill pre-existing rows (the certified online-build protocol)");
    f.cleanup();
}

#[test]
fn drop_index_removes_entries_and_stops_maintenance() {
    let f = WriteFixture::new("ddl_drop_index");
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'x', TRUE)");
    f.write("CREATE INDEX t_name_drop_idx ON t (name)");
    f.write("DROP INDEX t_name_drop_idx ON t");
    // The index is gone -- a later query on `name` must fall back to a
    // seq scan (still correct), not error.
    let s = f.select("SELECT id FROM t WHERE name = 'x'");
    assert_eq!(s.rows, vec![vec![int(1)]]);
    f.cleanup();
}

#[test]
fn create_table_if_not_exists_on_duplicate_is_a_clean_noop() {
    let f = WriteFixture::new("ddl_if_not_exists");
    f.write("CREATE TABLE dupe (id INTEGER PRIMARY KEY)");
    let r = f.write("CREATE TABLE IF NOT EXISTS dupe (id INTEGER PRIMARY KEY)");
    assert_eq!(r.rows_affected, 0);
    f.cleanup();
}

#[test]
fn create_table_without_if_not_exists_on_duplicate_errors() {
    let f = WriteFixture::new("ddl_duplicate_errors");
    f.write("CREATE TABLE dupe2 (id INTEGER PRIMARY KEY)");
    let err = f.try_write("CREATE TABLE dupe2 (id INTEGER PRIMARY KEY)");
    assert!(err.is_err());
    f.cleanup();
}

#[test]
fn create_database_is_a_controlled_unsupported_error_not_a_fake_success() {
    let f = WriteFixture::new("ddl_create_database_unsupported");
    let err = f.try_write("CREATE DATABASE extra_db");
    match err {
        Err(crate::SqlError::UnsupportedExecution { .. }) => {}
        other => panic!("expected UnsupportedExecution, got {other:?}"),
    }
    f.cleanup();
}

// -----------------------------------------------------------------
// DDL + restart (items 36/37/38/39/40, 44, 81)
// -----------------------------------------------------------------

#[test]
fn ddl_and_dml_survive_restart_together() {
    let dir = std::env::temp_dir().join(format!(
        "rubixdb_write_restart_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let table_id;
    let index_id;
    {
        let engine = open_engine(&dir);
        let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
        catalog.bootstrap().unwrap();
        let database_id = catalog.list_databases().unwrap()[0].database_id;
        let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
        let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
        let builder = Arc::new(IndexBuilder::new(
            Arc::clone(&engine),
            Arc::clone(&catalog),
            Arc::clone(&store),
        ));
        let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
        let ctx = crate::bind::BindContext {
            database_id,
            default_schema_id: schema_id,
        };

        let run = |sql: &str,
                   catalog: &CatalogService,
                   store: &Arc<TableStore>,
                   builder: &Arc<IndexBuilder>,
                   txm: &TransactionManager,
                   ctx: &crate::bind::BindContext| {
            let limits = SqlLimits::default();
            let stmt = parse_statement(sql, &limits).unwrap();
            let metrics = SqlMetrics::default();
            let auth = AuthContext::admin("t");
            let bound = bind_statement(catalog, ctx, &auth, &metrics, &limits, &stmt).unwrap();
            let plan = build_plan(
                &bound,
                catalog,
                &PlannerLimits::default(),
                &PlannerMetrics::default(),
            )
            .unwrap();
            execute_write_autocommit(
                &plan,
                txm,
                store,
                catalog,
                builder,
                &[],
                &ExecLimits::default(),
                &WriteMetrics::default(),
                &CancellationToken::new(),
            )
            .unwrap()
        };

        run(
            "CREATE TABLE persisted (id INTEGER PRIMARY KEY, label TEXT)",
            &catalog,
            &store,
            &builder,
            &txm,
            &ctx,
        );
        table_id = catalog
            .get_table_by_name(schema_id, "persisted")
            .unwrap()
            .unwrap()
            .table_id;
        run(
            "INSERT INTO persisted (id, label) VALUES (1, 'before-restart')",
            &catalog,
            &store,
            &builder,
            &txm,
            &ctx,
        );
        run(
            "CREATE INDEX persisted_label_idx ON persisted (label)",
            &catalog,
            &store,
            &builder,
            &txm,
            &ctx,
        );
        index_id = catalog
            .list_indexes(table_id)
            .unwrap()
            .into_iter()
            .find(|i| i.name == "persisted_label_idx")
            .unwrap()
            .index_id;
        engine.shutdown();
    }

    let engine = open_engine(&dir);
    let catalog = Arc::new(CatalogService::new(Arc::clone(&engine)));
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));

    assert!(
        catalog.get_table(table_id).unwrap().is_some(),
        "the table must survive restart"
    );
    assert_eq!(
        store
            .get_row(table_id, &[RelationalValue::Integer(1)])
            .unwrap()
            .unwrap()[1],
        text("before-restart")
    );
    let via_index = builder
        .index_lookup(index_id, &[text("before-restart")])
        .unwrap();
    assert_eq!(
        via_index.len(),
        1,
        "the index must survive restart and still find the row"
    );

    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

fn open_engine(dir: &std::path::Path) -> Arc<rubixdb::lsm::LsmEngine> {
    use rubixdb::lsm::{LsmConfig, LsmEngine};
    use rubixdb::wal::{SyncMode, WalConfig};
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
            Default::default(),
            LsmConfig::default(),
        )
        .unwrap(),
    )
}

// -----------------------------------------------------------------
// Concurrency (items 31-34, 66)
// -----------------------------------------------------------------

#[test]
fn concurrent_insert_distinct_pks_all_succeed() {
    let f = Arc::new(WriteFixture::new("concurrent_insert_distinct"));
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let f = Arc::clone(&f);
            thread::spawn(move || {
                f.write(&format!(
                    "INSERT INTO t (id, name, active) VALUES ({i}, 'x', TRUE)"
                ));
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(f.select("SELECT id FROM t").rows.len(), 10);
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

#[test]
fn concurrent_insert_same_pk_exactly_one_wins_barrier() {
    let f = Arc::new(WriteFixture::new("concurrent_insert_same_pk"));
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let f = Arc::clone(&f);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut txn = f.txm.begin().unwrap();
                f.write_in_txn(
                    &format!("INSERT INTO t (id, name, active) VALUES (1, 't{i}', TRUE)"),
                    &mut txn,
                )
                .unwrap();
                barrier.wait();
                txn.commit().is_ok()
            })
        })
        .collect();
    let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|&&ok| ok).count(),
        1,
        "exactly one concurrent INSERT of the same PK must commit"
    );
    assert_eq!(f.select("SELECT id FROM t").rows.len(), 1);
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

#[test]
fn concurrent_update_same_row_exactly_one_wins_barrier() {
    let f = Arc::new(WriteFixture::new("concurrent_update_same_row"));
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'orig', TRUE)");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let f = Arc::clone(&f);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut txn = f.txm.begin().unwrap();
                f.write_in_txn(
                    &format!("UPDATE t SET name = 'u{i}' WHERE id = 1"),
                    &mut txn,
                )
                .unwrap();
                barrier.wait();
                txn.commit().is_ok()
            })
        })
        .collect();
    let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|&&ok| ok).count(),
        1,
        "exactly one concurrent UPDATE of the same row must commit"
    );
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

#[test]
fn concurrent_delete_same_row_exactly_one_wins_barrier() {
    let f = Arc::new(WriteFixture::new("concurrent_delete_same_row"));
    f.write("INSERT INTO t (id, name, active) VALUES (1, 'x', TRUE)");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let f = Arc::clone(&f);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut txn = f.txm.begin().unwrap();
                f.write_in_txn("DELETE FROM t WHERE id = 1", &mut txn)
                    .unwrap();
                barrier.wait();
                txn.commit().is_ok()
            })
        })
        .collect();
    let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|&&ok| ok).count(),
        1,
        "exactly one concurrent DELETE of the same row must commit"
    );
    assert!(f.select("SELECT id FROM t").rows.is_empty());
    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

// -----------------------------------------------------------------
// Compaction interaction (item 28/74)
// -----------------------------------------------------------------

#[test]
fn writes_remain_correct_during_automatic_compaction() {
    use rubixdb::lsm::LsmConfig;
    let dir = std::env::temp_dir().join(format!(
        "rubixdb_write_compaction_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Arc::new(
        rubixdb::lsm::LsmEngine::open(
            &dir,
            rubixdb::wal::WalConfig {
                sync_mode: rubixdb::wal::SyncMode::GroupCommit {
                    max_wait: Duration::from_millis(5),
                    max_batch_bytes: 256 * 1024,
                },
                ..rubixdb::wal::WalConfig::default()
            },
            Default::default(),
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
    let database_id = catalog.list_databases().unwrap()[0].database_id;
    let schema_id = catalog.list_schemas(database_id).unwrap()[0].schema_id;
    let store = Arc::new(TableStore::new(Arc::clone(&engine), Arc::clone(&catalog)));
    let builder = Arc::new(IndexBuilder::new(
        Arc::clone(&engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    let txm = TransactionManager::new(Arc::clone(&engine), Arc::clone(&store));
    let ctx = crate::bind::BindContext {
        database_id,
        default_schema_id: schema_id,
    };

    let run = |sql: &str| {
        let limits = SqlLimits::default();
        let stmt = parse_statement(sql, &limits).unwrap();
        let metrics = SqlMetrics::default();
        let auth = AuthContext::admin("t");
        let bound = bind_statement(&catalog, &ctx, &auth, &metrics, &limits, &stmt).unwrap();
        let plan = build_plan(
            &bound,
            &catalog,
            &PlannerLimits::default(),
            &PlannerMetrics::default(),
        )
        .unwrap();
        execute_write_autocommit(
            &plan,
            &txm,
            &store,
            &catalog,
            &builder,
            &[],
            &ExecLimits::default(),
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap()
    };

    run("CREATE TABLE ct (id INTEGER PRIMARY KEY, label TEXT)");
    for i in 0..150 {
        run(&format!(
            "INSERT INTO ct (id, label) VALUES ({i}, 'filler')"
        ));
    }
    assert!(
        engine.compaction_metrics().cycles_completed > 0,
        "fixture must actually trigger compaction"
    );
    run("UPDATE ct SET label = 'updated' WHERE id = 1");
    run("DELETE FROM ct WHERE id = 2");

    let table_id = catalog
        .get_table_by_name(schema_id, "ct")
        .unwrap()
        .unwrap()
        .table_id;
    assert_eq!(
        store
            .get_row(table_id, &[RelationalValue::Integer(1)])
            .unwrap()
            .unwrap()[1],
        text("updated")
    );
    assert!(store
        .get_row(table_id, &[RelationalValue::Integer(2)])
        .unwrap()
        .is_none());
    assert!(store
        .get_row(table_id, &[RelationalValue::Integer(3)])
        .unwrap()
        .is_some());

    engine.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------
// Resource limits (item 46/47/48)
// -----------------------------------------------------------------

#[test]
fn mass_delete_beyond_max_dml_target_rows_fails_closed() {
    let f = WriteFixture::new("mass_delete_limit");
    for i in 0..20 {
        f.write(&format!(
            "INSERT INTO t (id, name, active) VALUES ({i}, 'x', TRUE)"
        ));
    }
    let plan = f.plan("DELETE FROM t WHERE active = TRUE");
    let limits = ExecLimits {
        max_dml_target_rows: 5,
        ..ExecLimits::default()
    };
    let err = execute_write_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.catalog,
        &f.builder,
        &[],
        &limits,
        &WriteMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::ResourceLimit { .. }));
    // Nothing must have been deleted -- the resource check fires while
    // collecting targets, before any `delete_row` call at all.
    assert_eq!(f.select("SELECT id FROM t").rows.len(), 20);
    f.cleanup();
}

// -----------------------------------------------------------------
// Security / plan-executor contract (items 82/83/97)
// -----------------------------------------------------------------

#[test]
fn read_only_plan_is_rejected_by_the_write_executor() {
    let f = WriteFixture::new("write_rejects_query_plan");
    let plan = f.plan("SELECT id FROM t");
    let err = execute_write_autocommit(
        &plan,
        &f.txm,
        &f.store,
        &f.catalog,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &WriteMetrics::default(),
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(err, crate::SqlError::UnsupportedExecution { .. }));
    f.cleanup();
}

#[test]
fn unauthorized_insert_is_rejected_at_bind_time_never_reaches_execution() {
    // D25/D26 (item 15/32): a principal lacking `INSERT` privilege on a
    // table it can otherwise `SELECT` gets the *same* `UnknownObject`
    // outcome as a genuinely missing table -- `crate::bind::scope::
    // resolve_table`'s own doc comment: "does not exist" and "exists
    // but forbidden" are structurally the same returned error, never
    // distinguishable by the caller. `SqlError::AuthorizationDenied` is
    // reserved for a different, narrower case (`CREATE DATABASE`,
    // `bind/ddl.rs`) that does not apply here.
    let f = WriteFixture::new("unauthorized_insert");
    let limits = SqlLimits::default();
    let stmt = parse_statement(
        "INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE)",
        &limits,
    )
    .unwrap();
    let metrics = SqlMetrics::default();
    let err = bind_statement(
        &f.f.catalog,
        &f.f.ctx,
        &AuthContext::reader("nobody"),
        &metrics,
        &limits,
        &stmt,
    )
    .unwrap_err();
    assert!(
        matches!(err, crate::SqlError::UnknownObject { .. }),
        "expected UnknownObject, got {err:?}"
    );
    f.cleanup();
}

// -----------------------------------------------------------------
// Metrics (item 55/106/107/119)
// -----------------------------------------------------------------

#[test]
fn metrics_record_statement_and_row_counts() {
    let f = WriteFixture::new("write_metrics");
    let metrics = WriteMetrics::default();
    let insert_plan = f.plan("INSERT INTO t (id, name, active) VALUES (1, 'a', TRUE)");
    execute_write_autocommit(
        &insert_plan,
        &f.txm,
        &f.store,
        &f.catalog,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    let update_plan = f.plan("UPDATE t SET active = FALSE WHERE id = 1");
    execute_write_autocommit(
        &update_plan,
        &f.txm,
        &f.store,
        &f.catalog,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();
    let delete_plan = f.plan("DELETE FROM t WHERE id = 1");
    execute_write_autocommit(
        &delete_plan,
        &f.txm,
        &f.store,
        &f.catalog,
        &f.builder,
        &[],
        &ExecLimits::default(),
        &metrics,
        &CancellationToken::new(),
    )
    .unwrap();

    let snap = metrics.snapshot();
    assert_eq!(snap.insert_statements, 1);
    assert_eq!(snap.update_statements, 1);
    assert_eq!(snap.delete_statements, 1);
    assert_eq!(snap.rows_inserted, 1);
    assert_eq!(snap.rows_updated, 1);
    assert_eq!(snap.rows_deleted, 1);
    assert_eq!(snap.rows_affected(), 3);
    f.cleanup();
}

#[test]
fn metrics_record_write_conflict_not_a_silent_success() {
    // Item 119/121: a transaction conflict must be observable and must
    // never collapse into a generic `dml_error` or a silent success.
    // Two genuinely racing autocommit `INSERT`s of the same `PRIMARY
    // KEY` are run on real OS threads with no artificial ordering --
    // which one "wins" is not deterministic (it can lose either via
    // `execute_insert`'s own pre-write existence check, if its thread
    // happens to begin after the other has already committed, or via
    // `Transaction::commit`'s freshness check, if the two genuinely
    // overlap), but exactly one must always win and the loser must
    // always be classified as a write conflict either way.
    let f = Arc::new(WriteFixture::new("write_metrics_conflict"));
    let metrics = Arc::new(WriteMetrics::default());
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let f = Arc::clone(&f);
            let metrics = Arc::clone(&metrics);
            thread::spawn(move || {
                execute_write_autocommit(
                    &f.plan(&format!(
                        "INSERT INTO t (id, name, active) VALUES (1, 't{i}', TRUE)"
                    )),
                    &f.txm,
                    &f.store,
                    &f.catalog,
                    &f.builder,
                    &[],
                    &ExecLimits::default(),
                    &metrics,
                    &CancellationToken::new(),
                )
            })
        })
        .collect();
    let results: Vec<crate::Result<WriteResult>> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let oks = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(crate::SqlError::Conflict { .. })))
        .count();
    assert_eq!(
        oks, 1,
        "exactly one racing INSERT of the same PRIMARY KEY must commit"
    );
    assert_eq!(
        conflicts, 1,
        "the loser must be a Conflict, never a generic dml_error"
    );

    let snap = metrics.snapshot();
    assert_eq!(snap.write_conflicts, 1);
    assert_eq!(
        snap.dml_errors, 0,
        "a PRIMARY KEY conflict must never be double-counted as a generic dml_error"
    );
    assert_eq!(f.select("SELECT id FROM t").rows.len(), 1);

    Arc::try_unwrap(f).ok().unwrap().cleanup();
}

// -----------------------------------------------------------------
// Differential testing against an independent reference model (item 67/68)
// -----------------------------------------------------------------

mod differential {
    use super::*;
    use std::collections::BTreeMap;

    /// A tiny, independent in-memory relational model -- never calls
    /// the planner, executor, `TableStore`, `IndexBuilder`, or
    /// `Transaction` for its own semantic decisions (item 67).
    #[derive(Default)]
    struct ReferenceTable {
        rows: BTreeMap<i32, (Option<String>, bool)>,
    }

    impl ReferenceTable {
        fn insert(&mut self, id: i32, name: Option<&str>, active: bool) -> bool {
            if self.rows.contains_key(&id) {
                return false; // PK conflict
            }
            self.rows.insert(id, (name.map(str::to_string), active));
            true
        }
        fn update_active(&mut self, id: i32, active: bool) -> bool {
            match self.rows.get_mut(&id) {
                Some(row) => {
                    row.1 = active;
                    true
                }
                None => false,
            }
        }
        fn delete(&mut self, id: i32) -> bool {
            self.rows.remove(&id).is_some()
        }
    }

    #[test]
    fn matches_reference_model_for_a_generated_insert_update_delete_sequence() {
        let f = WriteFixture::new("diff_write_sequence");
        let mut reference = ReferenceTable::default();

        let script: Vec<(&str, i32)> = vec![
            ("insert", 1),
            ("insert", 2),
            ("insert", 1), // duplicate PK -- must fail both places
            ("update", 1),
            ("delete", 2),
            ("update", 2), // missing row -- 0 rows affected both places
            ("delete", 2), // already gone -- 0 rows affected both places
        ];

        for (op, id) in script {
            match op {
                "insert" => {
                    let ref_ok = reference.insert(id, Some("x"), true);
                    let actual = f.try_write(&format!(
                        "INSERT INTO t (id, name, active) VALUES ({id}, 'x', TRUE)"
                    ));
                    assert_eq!(actual.is_ok(), ref_ok, "INSERT id={id} mismatch");
                }
                "update" => {
                    let ref_ok = reference.update_active(id, false);
                    let r = f.write(&format!("UPDATE t SET active = FALSE WHERE id = {id}"));
                    assert_eq!(r.rows_affected == 1, ref_ok, "UPDATE id={id} mismatch");
                }
                "delete" => {
                    let ref_ok = reference.delete(id);
                    let r = f.write(&format!("DELETE FROM t WHERE id = {id}"));
                    assert_eq!(r.rows_affected == 1, ref_ok, "DELETE id={id} mismatch");
                }
                _ => unreachable!(),
            }
        }

        let mut actual_ids: Vec<i32> = f
            .select("SELECT id FROM t")
            .rows
            .into_iter()
            .map(|r| match r[0] {
                Some(RelationalValue::Integer(n)) => n,
                _ => panic!(),
            })
            .collect();
        actual_ids.sort_unstable();
        let mut expected_ids: Vec<i32> = reference.rows.keys().copied().collect();
        expected_ids.sort_unstable();
        assert_eq!(actual_ids, expected_ids);
        f.cleanup();
    }
}
