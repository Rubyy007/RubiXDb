//! SQL Phase 2b + 3 -- `ALTER TABLE` and foreign keys (`docs/adr/ADR-DDL-FK-01.md`).
//!
//! Every test drives the real parse, bind, plan and write-executor pipeline against
//! a real engine (`mission1_support::Db`); the relational-layer concurrency tests are
//! in `src/relational/fk_tests.rs` and the HTTP tests in
//! `api/tests/api_sql_phase2b3.rs`.

use std::sync::Arc;

use rubixdb::relational::{Transaction, TransactionManager, TxnLimits};

use crate::ast::{AlterOperation, ReferentialAction, Statement};
use crate::bound::{BoundAlterOp, BoundStatement};
use crate::error::SqlError;
use crate::exec::write::{execute_write, WriteMetrics, WriteResult};
use crate::exec::{CancellationToken, ExecLimits};
use crate::limits::SqlLimits;
use crate::mission1_support::*;
use crate::parse::parse_statement;
use crate::plan::Plan;

fn r(vals: &[V]) -> Row {
    vals.to_vec()
}

/// Runs `sql` inside `txn` (executed, not committed).
fn in_txn(db: &Db, sql: &str, txn: &mut Transaction) -> crate::Result<WriteResult> {
    let plan = db.try_plan(sql)?;
    execute_write(
        &plan,
        txn,
        &db.store,
        &db.catalog,
        &db.builder,
        &[],
        &ExecLimits::default(),
        &WriteMetrics::default(),
        &CancellationToken::new(),
    )
}

fn is_fk_violation(e: &SqlError) -> bool {
    matches!(e, SqlError::ForeignKeyViolation { .. })
}

fn reopen(db: Db) -> Db {
    let Db {
        f,
        catalog,
        store,
        builder,
        txm,
    } = db;
    drop((catalog, store, builder, txm));
    let f = f.reopen();
    let catalog = Arc::new(rubixdb::catalog::CatalogService::new(Arc::clone(&f.engine)));
    let store = Arc::new(rubixdb::relational::TableStore::new(
        Arc::clone(&f.engine),
        Arc::clone(&catalog),
    ));
    let builder = Arc::new(rubixdb::relational::IndexBuilder::new(
        Arc::clone(&f.engine),
        Arc::clone(&catalog),
        Arc::clone(&store),
    ));
    let txm =
        rubixdb::relational::TransactionManager::new(Arc::clone(&f.engine), Arc::clone(&store));
    Db {
        f,
        catalog,
        store,
        builder,
        txm,
    }
}

/// `parent(id INTEGER PK, name TEXT)` and `child(id INTEGER PK, pid INTEGER, note TEXT)`
/// with `child.pid` referencing `parent.id` with the given actions.
fn fk_db(tag: &str, actions: &str) -> Db {
    let db = Db::new(tag);
    db.write("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)");
    db.write(&format!(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER, note TEXT, \
         CONSTRAINT child_pid_fk FOREIGN KEY (pid) REFERENCES parent (id) {actions})"
    ));
    db
}

fn seed(db: &Db) {
    db.write("INSERT INTO parent VALUES (1, 'one'), (2, 'two'), (3, 'three')");
    db.write("INSERT INTO child VALUES (10, 1, 'a'), (11, 1, 'b'), (20, 2, 'c'), (30, NULL, 'd')");
}

fn child_ids(db: &Db) -> Vec<Row> {
    db.rows("SELECT id FROM child ORDER BY id")
}

fn parent_ids(db: &Db) -> Vec<Row> {
    db.rows("SELECT id FROM parent ORDER BY id")
}

fn parse(sql: &str) -> crate::Result<Statement> {
    parse_statement(sql, &SqlLimits::default())
}

// =======================================================================
// Parser
// =======================================================================

mod parser {
    use super::*;

    fn alter(sql: &str) -> crate::ast::AlterTable {
        match parse(sql).unwrap() {
            Statement::AlterTable(a) => a,
            other => panic!("not an ALTER TABLE: {other:?}"),
        }
    }

    #[test]
    fn every_supported_alter_form_parses_to_one_operation() {
        assert!(matches!(
            alter("ALTER TABLE t ADD COLUMN c INTEGER DEFAULT 5 NOT NULL").operation,
            AlterOperation::AddColumn { .. }
        ));
        assert!(matches!(
            alter("ALTER TABLE t RENAME COLUMN a TO b").operation,
            AlterOperation::RenameColumn { .. }
        ));
        assert!(matches!(
            alter("ALTER TABLE t ALTER COLUMN a SET DEFAULT 3").operation,
            AlterOperation::SetDefault { .. }
        ));
        assert!(matches!(
            alter("ALTER TABLE t ALTER COLUMN a DROP DEFAULT").operation,
            AlterOperation::DropDefault { .. }
        ));
        assert!(matches!(
            alter("ALTER TABLE t ALTER COLUMN a SET NOT NULL").operation,
            AlterOperation::SetNotNull { .. }
        ));
        assert!(matches!(
            alter("ALTER TABLE t ALTER COLUMN a DROP NOT NULL").operation,
            AlterOperation::DropNotNull { .. }
        ));
        assert!(matches!(
            alter("ALTER TABLE t ADD CONSTRAINT u UNIQUE (a, b)").operation,
            AlterOperation::AddUnique { .. }
        ));
        assert!(matches!(
            alter("ALTER TABLE t DROP CONSTRAINT u").operation,
            AlterOperation::DropConstraint { .. }
        ));
        let at = alter("ALTER TABLE IF EXISTS s.t ADD COLUMN IF NOT EXISTS c TEXT");
        assert!(at.if_exists);
        assert!(matches!(
            at.operation,
            AlterOperation::AddColumn { if_not_exists: true, .. }
        ));
    }

    #[test]
    fn a_foreign_key_parses_with_actions_and_columns() {
        let at = alter(
            "ALTER TABLE c ADD CONSTRAINT fk FOREIGN KEY (a, b) REFERENCES p (x, y) \
             ON DELETE CASCADE ON UPDATE SET NULL",
        );
        let AlterOperation::AddForeignKey(fk) = at.operation else {
            panic!()
        };
        assert_eq!(fk.name.unwrap().value, "fk");
        assert_eq!(fk.columns.len(), 2);
        assert_eq!(fk.referred_columns.len(), 2);
        assert_eq!(fk.on_delete, Some(ReferentialAction::Cascade));
        assert_eq!(fk.on_update, Some(ReferentialAction::SetNull));
        // without a column list the referenced key is the primary key
        let AlterOperation::AddForeignKey(fk) =
            alter("ALTER TABLE c ADD FOREIGN KEY (a) REFERENCES p").operation
        else {
            panic!()
        };
        assert!(fk.referred_columns.is_empty());
        assert_eq!(fk.on_delete, None);
    }

    #[test]
    fn create_table_collects_inline_and_table_level_foreign_keys_in_order() {
        let Statement::CreateTable(ct) = parse(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, a INTEGER REFERENCES p (id) ON DELETE CASCADE, \
             b INTEGER, c INTEGER, FOREIGN KEY (b, c) REFERENCES q (x, y) ON UPDATE RESTRICT)",
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(ct.foreign_keys.len(), 2);
        assert_eq!(ct.foreign_keys[0].columns[0].value, "a"); // inline, folded to one column
        assert_eq!(ct.foreign_keys[0].on_delete, Some(ReferentialAction::Cascade));
        assert_eq!(ct.foreign_keys[1].columns.len(), 2);
        assert_eq!(ct.foreign_keys[1].on_update, Some(ReferentialAction::Restrict));
    }

    #[test]
    fn the_forms_rubixdb_cannot_do_safely_are_refused_with_the_reason() {
        let cases = [
            ("ALTER TABLE t DROP COLUMN a", "BLOCKED"),
            ("ALTER TABLE t ALTER COLUMN a TYPE BIGINT", "NOT IMPLEMENTED"),
            ("ALTER TABLE t ADD PRIMARY KEY (a)", "primary key"),
            ("ALTER TABLE t ADD COLUMN a INTEGER CHECK (a > 0)", "CHECK"),
            ("CREATE TABLE c (id INTEGER PRIMARY KEY, CHECK (id > 0) NOT ENFORCED)", "NOT ENFORCED"),
            ("ALTER TABLE t RENAME TO u", "RENAME TO"),
            ("ALTER TABLE t ADD COLUMN a INTEGER, ADD COLUMN b INTEGER", "more than one operation"),
            ("ALTER TABLE t ADD COLUMN a INTEGER REFERENCES p (id)", "REFERENCES"),
            ("ALTER TABLE t ADD COLUMN a INTEGER PRIMARY KEY", "PRIMARY KEY"),
            (
                "ALTER TABLE t ADD FOREIGN KEY (a) REFERENCES p (id) NOT VALID",
                "NOT VALID",
            ),
            (
                "ALTER TABLE t ADD FOREIGN KEY (a) REFERENCES p (id) MATCH FULL",
                "MATCH",
            ),
            (
                "ALTER TABLE t ADD FOREIGN KEY (a) REFERENCES p (id) DEFERRABLE",
                "DEFERRABLE",
            ),
            ("ALTER TABLE t DROP CONSTRAINT c CASCADE", "CASCADE"),
        ];
        for (sql, needle) in cases {
            match parse(sql) {
                Err(SqlError::Unsupported { detail }) => {
                    assert!(detail.contains(needle), "{sql}: {detail}")
                }
                other => panic!("{sql}: expected Unsupported, got {other:?}"),
            }
        }
    }
}

// =======================================================================
// Binder and planner
// =======================================================================

mod bind_and_plan {
    use super::*;

    fn alter_op(db: &Db, sql: &str) -> BoundAlterOp {
        match db.try_bind(sql).unwrap() {
            BoundStatement::AlterTable(a) => a.operation,
            other => panic!("not an ALTER: {other:?}"),
        }
    }

    #[test]
    fn names_resolve_to_ordinals_and_ids() {
        let db = fk_db("p23_bind", "ON DELETE CASCADE");
        match alter_op(&db, "ALTER TABLE child RENAME COLUMN note TO memo") {
            BoundAlterOp::RenameColumn { ordinal, new_name } => {
                assert_eq!(ordinal, 2);
                assert_eq!(new_name, "memo");
            }
            other => panic!("{other:?}"),
        }
        match alter_op(&db, "ALTER TABLE child ALTER COLUMN note SET DEFAULT 'x'") {
            BoundAlterOp::SetDefault { ordinal, default_value } => {
                assert_eq!(ordinal, 2);
                assert!(!default_value.is_empty());
            }
            other => panic!("{other:?}"),
        }
        match alter_op(&db, "ALTER TABLE child ADD COLUMN extra INTEGER DEFAULT 4") {
            BoundAlterOp::AddColumn(c) => {
                assert_eq!(c.name, "extra");
                assert!(c.default_value.is_some() && c.nullable);
            }
            other => panic!("{other:?}"),
        }
        match alter_op(
            &db,
            "ALTER TABLE parent ADD CONSTRAINT p_name_uq UNIQUE (name)",
        ) {
            BoundAlterOp::AddUnique { name, column_ordinals } => {
                assert_eq!(name, "p_name_uq");
                assert_eq!(column_ordinals, vec![1]);
            }
            other => panic!("{other:?}"),
        }
        db.cleanup();
    }

    #[test]
    fn a_foreign_key_resolves_both_tables_and_defaults_to_the_primary_key() {
        let db = fk_db("p23_bind_fk", "");
        db.write("CREATE TABLE other (id INTEGER PRIMARY KEY, pid INTEGER)");
        let BoundAlterOp::AddForeignKey(fk) = alter_op(
            &db,
            "ALTER TABLE other ADD FOREIGN KEY (pid) REFERENCES parent ON DELETE SET NULL",
        ) else {
            panic!()
        };
        assert_eq!(fk.child_ordinals, vec![1]);
        assert_eq!(fk.parent_ordinals, vec![0]); // parent's primary key
        assert_eq!(fk.parent_table_id, db.table_id("parent"));
        assert_eq!(fk.table_id, db.table_id("other"));
        assert_eq!(fk.on_delete, rubixdb::catalog::schema::FkAction::SetNull);
        assert_eq!(fk.on_update, rubixdb::catalog::schema::FkAction::NoAction);
        assert_eq!(fk.name, "other_pid_fkey"); // generated
        db.cleanup();
    }

    #[test]
    fn drop_constraint_finds_a_foreign_key_or_a_unique_index_by_name() {
        let db = fk_db("p23_bind_drop", "");
        db.write("CREATE UNIQUE INDEX parent_name_uq ON parent (name)");
        assert!(matches!(
            alter_op(&db, "ALTER TABLE child DROP CONSTRAINT child_pid_fk"),
            BoundAlterOp::DropForeignKey { .. }
        ));
        assert!(matches!(
            alter_op(&db, "ALTER TABLE parent DROP CONSTRAINT parent_name_uq"),
            BoundAlterOp::DropUnique { .. }
        ));
        assert!(matches!(
            alter_op(&db, "ALTER TABLE parent DROP CONSTRAINT IF EXISTS nope"),
            BoundAlterOp::Noop
        ));
        assert!(matches!(
            db.try_bind("ALTER TABLE parent DROP CONSTRAINT nope").unwrap_err(),
            SqlError::UnknownObject { kind: "constraint", .. }
        ));
        // the primary key cannot be dropped
        assert!(matches!(
            db.try_bind("ALTER TABLE parent DROP CONSTRAINT parent_pkey").unwrap_err(),
            SqlError::Unsupported { .. }
        ));
        db.cleanup();
    }

    #[test]
    fn invalid_statements_fail_to_bind() {
        let db = fk_db("p23_bind_err", "");
        // unknown table / column
        assert!(matches!(
            db.try_bind("ALTER TABLE nope ADD COLUMN a INTEGER").unwrap_err(),
            SqlError::UnknownObject { kind: "table", .. }
        ));
        assert!(matches!(
            db.try_bind("ALTER TABLE parent RENAME COLUMN nope TO x").unwrap_err(),
            SqlError::UnknownObject { kind: "column", .. }
        ));
        assert!(matches!(
            db.try_bind("ALTER TABLE parent ALTER COLUMN nope SET NOT NULL").unwrap_err(),
            SqlError::UnknownObject { kind: "column", .. }
        ));
        // duplicates
        assert!(matches!(
            db.try_bind("ALTER TABLE parent ADD COLUMN name TEXT").unwrap_err(),
            SqlError::AlreadyExists { .. }
        ));
        assert!(matches!(
            db.try_bind("ALTER TABLE parent RENAME COLUMN id TO name").unwrap_err(),
            SqlError::AlreadyExists { .. }
        ));
        // a default must be a literal of the column's type
        assert!(db
            .try_bind("ALTER TABLE parent ALTER COLUMN id SET DEFAULT 'text'")
            .is_err());
        assert!(matches!(
            db.try_bind("ALTER TABLE parent ADD COLUMN c INTEGER DEFAULT 1 + id").unwrap_err(),
            SqlError::Unsupported { .. } | SqlError::UnknownObject { .. }
        ));
        // an unknown referenced table / column
        assert!(matches!(
            db.try_bind("ALTER TABLE child ADD FOREIGN KEY (note) REFERENCES nope (id)")
                .unwrap_err(),
            SqlError::UnknownObject { kind: "table", .. }
        ));
        assert!(matches!(
            db.try_bind("ALTER TABLE child ADD FOREIGN KEY (note) REFERENCES parent (nope)")
                .unwrap_err(),
            SqlError::UnknownObject { kind: "column", .. }
        ));
        // IF EXISTS makes a missing table a no-op
        assert!(matches!(
            db.try_bind("ALTER TABLE IF EXISTS nope ADD COLUMN a INTEGER").unwrap(),
            BoundStatement::AlterTable(a) if a.table_id.is_none()
        ));
        db.cleanup();
    }

    #[test]
    fn the_column_limit_applies_to_add_column() {
        let db = Db::new("p23_bind_limit");
        db.write("CREATE TABLE lim_t (id INTEGER PRIMARY KEY, a INTEGER)");
        let limits = SqlLimits {
            max_columns: 2,
            ..SqlLimits::default()
        };
        let err = db
            .try_bind_with(
                "ALTER TABLE lim_t ADD COLUMN b INTEGER",
                &crate::auth::AuthContext::admin("x"),
                &limits,
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn alter_plans_as_ddl_and_never_as_a_query() {
        let db = fk_db("p23_plan", "");
        let plan = db.plan("ALTER TABLE parent ADD COLUMN c INTEGER");
        assert!(matches!(plan, Plan::Ddl(BoundStatement::AlterTable(_))));
        db.cleanup();
    }

    #[test]
    fn create_table_binds_foreign_keys_including_a_self_reference() {
        let db = fk_db("p23_bind_ct", "");
        let BoundStatement::CreateTable(ct) = db
            .try_bind(
                "CREATE TABLE tree (id INTEGER PRIMARY KEY, up INTEGER REFERENCES tree (id), \
                 p INTEGER, FOREIGN KEY (p) REFERENCES parent (id) ON DELETE CASCADE)",
            )
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(ct.foreign_keys.len(), 2);
        assert_eq!(ct.foreign_keys[0].parent_table_id, None); // itself
        assert_eq!(ct.foreign_keys[0].name, "tree_up_fkey");
        assert_eq!(ct.foreign_keys[1].parent_table_id, Some(db.table_id("parent")));
        db.cleanup();
    }
}

// =======================================================================
// ALTER TABLE
// =======================================================================

mod alter {
    use super::*;

    fn people(tag: &str) -> Db {
        let db = Db::new(tag);
        db.write("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)");
        db.write("INSERT INTO people VALUES (1, 'ann', 30), (2, 'bob', NULL), (3, 'cy', 25)");
        db
    }

    #[test]
    fn add_column_with_a_default_is_applied_to_existing_rows_everywhere_they_are_read() {
        let db = people("p23_add_default");
        db.write("ALTER TABLE people ADD COLUMN country TEXT DEFAULT 'none'");
        assert_eq!(
            db.rows("SELECT id, country FROM people ORDER BY id"),
            vec![
                r(&[int(1), text("none")]),
                r(&[int(2), text("none")]),
                r(&[int(3), text("none")]),
            ]
        );
        // in WHERE, GROUP BY, ORDER BY and an aggregate
        assert_eq!(db.rows("SELECT COUNT(*) FROM people WHERE country = 'none'"), vec![r(&[big(3)])]);
        assert_eq!(
            db.rows("SELECT country, COUNT(*) FROM people GROUP BY country"),
            vec![r(&[text("none"), big(3)])]
        );
        // an index built afterwards sees the same values
        db.write("CREATE INDEX people_country ON people (country)");
        assert_eq!(
            db.rows("SELECT id FROM people WHERE country = 'none' ORDER BY id"),
            vec![r(&[int(1)]), r(&[int(2)]), r(&[int(3)])]
        );
        db.cleanup();
    }

    #[test]
    fn add_column_without_a_default_reads_null_for_existing_rows() {
        let db = people("p23_add_null");
        db.write("ALTER TABLE people ADD COLUMN email TEXT");
        assert_eq!(
            db.rows("SELECT id, email FROM people ORDER BY id"),
            vec![r(&[int(1), NULL]), r(&[int(2), NULL]), r(&[int(3), NULL])]
        );
        db.cleanup();
    }

    #[test]
    fn rows_written_after_the_alter_store_the_column_and_old_rows_can_be_updated() {
        let db = people("p23_add_mixed");
        db.write("ALTER TABLE people ADD COLUMN score INTEGER DEFAULT 5");
        db.write("INSERT INTO people (id, name, age, score) VALUES (4, 'dee', 40, 99)");
        db.write("INSERT INTO people (id, name, age) VALUES (5, 'eve', 41)"); // omitted: the default
        db.write("UPDATE people SET score = score + 1 WHERE id IN (1, 4)");
        assert_eq!(
            db.rows("SELECT id, score FROM people ORDER BY id"),
            vec![
                r(&[int(1), int(6)]), // an old row, rewritten at the new version
                r(&[int(2), int(5)]), // an old row, untouched
                r(&[int(3), int(5)]),
                r(&[int(4), int(100)]),
                r(&[int(5), int(5)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn set_default_changes_new_rows_only() {
        let db = people("p23_set_default");
        db.write("ALTER TABLE people ADD COLUMN score INTEGER DEFAULT 5");
        db.write("ALTER TABLE people ALTER COLUMN score SET DEFAULT 77");
        db.write("INSERT INTO people (id, name, age) VALUES (4, 'dee', 1)");
        db.write("ALTER TABLE people ALTER COLUMN score DROP DEFAULT");
        db.write("INSERT INTO people (id, name, age) VALUES (5, 'eve', 2)");
        assert_eq!(
            db.rows("SELECT id, score FROM people ORDER BY id"),
            vec![
                r(&[int(1), int(5)]),
                r(&[int(2), int(5)]),
                r(&[int(3), int(5)]),
                r(&[int(4), int(77)]),
                r(&[int(5), NULL]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn add_column_not_null_needs_a_default_unless_the_table_is_empty() {
        let db = people("p23_add_not_null");
        let before = db.catalog.get_columns(db.table_id("people")).unwrap();
        let err = db
            .try_write("ALTER TABLE people ADD COLUMN must INTEGER NOT NULL")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        // nothing changed
        assert_eq!(db.catalog.get_columns(db.table_id("people")).unwrap(), before);
        // with a default it works
        db.write("ALTER TABLE people ADD COLUMN must INTEGER NOT NULL DEFAULT 3");
        assert_eq!(db.rows("SELECT SUM(must) FROM people"), vec![r(&[big(9)])]);
        // and the new column is NOT NULL for new rows
        assert!(db.try_write("INSERT INTO people (id, name, age, must) VALUES (9, 'x', 1, NULL)").is_err());
        // an empty table may take one without a default
        db.write("CREATE TABLE empty_t (id INTEGER PRIMARY KEY)");
        db.write("ALTER TABLE empty_t ADD COLUMN must INTEGER NOT NULL");
        db.cleanup();
    }

    #[test]
    fn add_column_if_not_exists_and_duplicates() {
        let db = people("p23_add_dup");
        db.write("ALTER TABLE people ADD COLUMN IF NOT EXISTS name TEXT"); // a no-op
        assert!(matches!(
            db.try_write("ALTER TABLE people ADD COLUMN name TEXT").unwrap_err(),
            SqlError::AlreadyExists { .. }
        ));
        db.write("ALTER TABLE IF EXISTS no_such ADD COLUMN x INTEGER");
        db.cleanup();
    }

    #[test]
    fn drop_column_is_refused_as_blocked_and_changes_nothing() {
        let db = people("p23_drop_col");
        let err = db.try_write("ALTER TABLE people DROP COLUMN age").unwrap_err();
        match err {
            SqlError::Unsupported { detail } => assert!(detail.contains("BLOCKED"), "{detail}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(db.catalog.get_columns(db.table_id("people")).unwrap().len(), 3);
        db.cleanup();
    }

    #[test]
    fn rename_column_is_visible_everywhere_and_old_name_stops_working() {
        let db = people("p23_rename");
        db.write("CREATE INDEX people_name ON people (name)");
        db.write("ALTER TABLE people RENAME COLUMN name TO full_name");
        assert_eq!(
            db.rows("SELECT full_name FROM people WHERE full_name = 'bob'"),
            vec![r(&[text("bob")])]
        );
        assert!(matches!(
            db.try_run("SELECT name FROM people").unwrap_err(),
            SqlError::UnknownObject { .. }
        ));
        db.write("INSERT INTO people (id, full_name, age) VALUES (4, 'dee', 1)");
        // the index (by ordinal) still serves lookups on the renamed column
        assert_eq!(
            db.rows("SELECT id FROM people WHERE full_name = 'dee'"),
            vec![r(&[int(4)])]
        );
        // a primary-key column can be renamed too
        db.write("ALTER TABLE people RENAME COLUMN id TO person_id");
        assert_eq!(db.rows("SELECT COUNT(*) FROM people WHERE person_id >= 1"), vec![r(&[big(4)])]);
        db.cleanup();
    }

    #[test]
    fn set_not_null_validates_existing_rows_and_leaves_nothing_on_failure() {
        let db = people("p23_set_not_null");
        let id = db.table_id("people");
        let before_cols = db.catalog.get_columns(id).unwrap();
        let before_version = db.catalog.get_table(id).unwrap().unwrap().schema_version;
        // row 2 has a NULL age
        let err = db
            .try_write("ALTER TABLE people ALTER COLUMN age SET NOT NULL")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert_eq!(db.catalog.get_columns(id).unwrap(), before_cols);
        assert_eq!(
            db.catalog.get_table(id).unwrap().unwrap().schema_version,
            before_version
        );
        // NULL still insertable: the failed ALTER changed nothing
        db.write("INSERT INTO people VALUES (4, 'dee', NULL)");
        db.write("DELETE FROM people WHERE age IS NULL");
        db.write("ALTER TABLE people ALTER COLUMN age SET NOT NULL");
        assert!(db.try_write("INSERT INTO people VALUES (5, 'eve', NULL)").is_err());
        assert!(db.try_write("UPDATE people SET age = NULL WHERE id = 1").is_err());
        // drop it again
        db.write("ALTER TABLE people ALTER COLUMN age DROP NOT NULL");
        db.write("INSERT INTO people VALUES (5, 'eve', NULL)");
        // a primary-key column cannot become nullable
        assert!(db
            .try_write("ALTER TABLE people ALTER COLUMN id DROP NOT NULL")
            .is_err());
        db.cleanup();
    }

    #[test]
    fn add_unique_constraint_validates_existing_rows_and_enforces_afterwards() {
        let db = people("p23_add_unique");
        db.write("INSERT INTO people VALUES (4, 'ann', 1)"); // a duplicate name
        let err = db
            .try_write("ALTER TABLE people ADD CONSTRAINT people_name_uq UNIQUE (name)")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        // no index row was left behind, so the name is free to use again
        assert!(db
            .catalog
            .list_indexes(db.table_id("people"))
            .unwrap()
            .iter()
            .all(|i| i.name != "people_name_uq"));
        db.write("DELETE FROM people WHERE id = 4");
        db.write("ALTER TABLE people ADD CONSTRAINT people_name_uq UNIQUE (name)");
        assert!(matches!(
            db.try_write("INSERT INTO people VALUES (5, 'ann', 1)").unwrap_err(),
            SqlError::Conflict { .. }
        ));
        // NULLs never conflict
        db.write("INSERT INTO people VALUES (6, NULL, 1), (7, NULL, 1)");
        db.write("ALTER TABLE people DROP CONSTRAINT people_name_uq");
        db.write("INSERT INTO people VALUES (5, 'ann', 1)");
        db.cleanup();
    }

    #[test]
    fn invalid_alters_are_rejected_without_partial_state() {
        let db = Db::new("p23_invalid");
        db.write("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)");
        db.write(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER, other INTEGER, note TEXT,              CONSTRAINT child_pid_fk FOREIGN KEY (pid) REFERENCES parent (id))",
        );
        db.write("INSERT INTO parent VALUES (1, 'a')");
        db.write("INSERT INTO child VALUES (10, 1, 7, 'orphan in other')"); // other = 7: no such parent
        let snapshot = |db: &Db| {
            (
                db.catalog.get_columns(db.table_id("child")).unwrap(),
                db.catalog.get_table(db.table_id("child")).unwrap().unwrap().schema_version,
                db.catalog.foreign_keys().unwrap().len(),
                db.catalog.list_indexes(db.table_id("child")).unwrap().len(),
            )
        };
        let before = snapshot(&db);
        for sql in [
            // an orphan exists, so the key fails its validation of the existing rows
            "ALTER TABLE child ADD CONSTRAINT second FOREIGN KEY (other) REFERENCES parent (id)",
            // referenced columns that are not unique
            "ALTER TABLE child ADD CONSTRAINT bad FOREIGN KEY (pid) REFERENCES parent (name)",
            // different types
            "ALTER TABLE child ADD CONSTRAINT bad2 FOREIGN KEY (note) REFERENCES parent (id)",
            // duplicate constraint name
            "ALTER TABLE child ADD CONSTRAINT child_pid_fk FOREIGN KEY (other) REFERENCES parent (id)",
            // not null with no default on a table with rows
            "ALTER TABLE child ADD COLUMN c INTEGER NOT NULL",
            // a duplicate column
            "ALTER TABLE child ADD COLUMN note TEXT",
            // a unique constraint over values that repeat
            "ALTER TABLE child ADD CONSTRAINT dup UNIQUE (pid, other)",
        ] {
            let result = db.try_write(sql);
            // the unique constraint over (1, 7) has no duplicate: that one succeeds, the rest fail
            if sql.contains("dup UNIQUE") {
                assert!(result.is_ok(), "{sql}");
                db.write("ALTER TABLE child DROP CONSTRAINT dup");
                continue;
            }
            assert!(result.is_err(), "{sql} should fail");
            assert_eq!(snapshot(&db), before, "{sql} left state behind");
        }
        // set not null where NULLs exist
        db.write("INSERT INTO child VALUES (11, 1, NULL, 'x')");
        let before = snapshot(&db);
        assert!(db
            .try_write("ALTER TABLE child ALTER COLUMN other SET NOT NULL")
            .is_err());
        assert_eq!(snapshot(&db), before);
        db.cleanup();
    }

    #[test]
    fn alter_survives_a_restart_catalog_and_data() {
        let db = people("p23_alter_restart");
        db.write("ALTER TABLE people ADD COLUMN score INTEGER DEFAULT 5");
        db.write("INSERT INTO people (id, name, age, score) VALUES (4, 'dee', 1, 8)");
        db.write("ALTER TABLE people RENAME COLUMN name TO full_name");
        db.write("ALTER TABLE people ALTER COLUMN age SET DEFAULT 18");
        db.write("ALTER TABLE people ADD CONSTRAINT people_full_name_uq UNIQUE (full_name)");
        let db = reopen(db);
        assert_eq!(
            db.rows("SELECT id, full_name, score FROM people ORDER BY id"),
            vec![
                r(&[int(1), text("ann"), int(5)]),
                r(&[int(2), text("bob"), int(5)]),
                r(&[int(3), text("cy"), int(5)]),
                r(&[int(4), text("dee"), int(8)]),
            ]
        );
        // the default set before the restart is still in force, and so is the unique constraint
        db.write("INSERT INTO people (id, full_name, score) VALUES (5, 'eve', 1)");
        assert_eq!(db.rows("SELECT age FROM people WHERE id = 5"), vec![r(&[int(18)])]);
        assert!(db.try_write("INSERT INTO people (id, full_name) VALUES (6, 'eve')").is_err());
        db.cleanup();
    }

    #[test]
    fn a_transaction_open_across_an_alter_conflicts_rather_than_writing_an_old_shape() {
        let db = people("p23_alter_txn");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO people VALUES (9, 'zed', 1)", &mut txn).unwrap();
        db.write("ALTER TABLE people ADD COLUMN score INTEGER DEFAULT 5");
        let err = txn.commit().unwrap_err();
        assert!(
            matches!(err, rubixdb::relational::RelationalError::Conflict { .. }),
            "{err:?}"
        );
        assert_eq!(db.rows("SELECT COUNT(*) FROM people"), vec![r(&[big(3)])]);
        db.cleanup();
    }

    #[test]
    fn alter_during_concurrent_writes_loses_and_corrupts_nothing() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::thread;
        let db = Arc::new(people("p23_alter_concurrent"));
        let stop = Arc::new(AtomicBool::new(false));
        let committed = Arc::new(AtomicUsize::new(0));
        let writers: Vec<_> = (0..3)
            .map(|w| {
                let (db, stop, committed) = (Arc::clone(&db), Arc::clone(&stop), Arc::clone(&committed));
                thread::spawn(move || {
                    let mut i = 0;
                    while !stop.load(Ordering::Relaxed) {
                        i += 1;
                        let id = 1000 + w * 100_000 + i;
                        // a statement bound before the ALTER but run after it may fail; one that
                        // succeeds must have been committed whole
                        if db
                            .try_write(&format!("INSERT INTO people (id, name, age) VALUES ({id}, 'w', 1)"))
                            .is_ok()
                        {
                            committed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        std::thread::sleep(std::time::Duration::from_millis(30));
        db.write("ALTER TABLE people ADD COLUMN score INTEGER DEFAULT 5");
        std::thread::sleep(std::time::Duration::from_millis(30));
        db.write("ALTER TABLE people ALTER COLUMN age SET DEFAULT 2");
        stop.store(true, Ordering::Relaxed);
        for w in writers {
            w.join().unwrap();
        }
        // every committed row decodes, in both shapes, and the count matches
        let n = db.rows("SELECT COUNT(*) FROM people");
        assert_eq!(
            n,
            vec![r(&[big(3 + committed.load(Ordering::Relaxed) as i64)])]
        );
        let scores = db.rows("SELECT score FROM people WHERE score <> 5");
        assert!(scores.is_empty(), "every row holds the default or the backfill: {scores:?}");
        Arc::try_unwrap(db).ok().unwrap().cleanup();
    }
}

// =======================================================================
// UPDATE of a primary key (G-8)
// =======================================================================

mod primary_key_update {
    use super::*;

    fn t(tag: &str) -> Db {
        let db = Db::new(tag);
        db.write("CREATE TABLE kt (id INTEGER PRIMARY KEY, name TEXT, other INTEGER)");
        db.write("CREATE UNIQUE INDEX kt_name ON kt (name)");
        db.write("INSERT INTO kt VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30)");
        db
    }

    #[test]
    fn a_key_can_change_and_the_row_moves() {
        let db = t("p23_pk_move");
        let res = db.write("UPDATE kt SET id = 10 WHERE id = 1 RETURNING id, name");
        assert_eq!(res.rows_affected, 1);
        assert_eq!(res.returning.unwrap().rows, vec![r(&[int(10), text("a")])]);
        assert_eq!(
            db.rows("SELECT id FROM kt ORDER BY id"),
            vec![r(&[int(2)]), r(&[int(3)]), r(&[int(10)])]
        );
        // the secondary index follows the row
        assert_eq!(db.rows("SELECT id FROM kt WHERE name = 'a'"), vec![r(&[int(10)])]);
        db.cleanup();
    }

    #[test]
    fn keys_can_shift_among_the_rows_of_one_statement() {
        let db = t("p23_pk_shift");
        db.write("UPDATE kt SET id = id + 1");
        assert_eq!(
            db.rows("SELECT id, name FROM kt ORDER BY id"),
            vec![
                r(&[int(2), text("a")]),
                r(&[int(3), text("b")]),
                r(&[int(4), text("c")]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn a_collision_with_an_untouched_row_fails_and_changes_nothing() {
        let db = t("p23_pk_collide");
        let err = db.try_write("UPDATE kt SET id = 3 WHERE id = 1").unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        assert_eq!(
            db.rows("SELECT id, name FROM kt ORDER BY id"),
            vec![
                r(&[int(1), text("a")]),
                r(&[int(2), text("b")]),
                r(&[int(3), text("c")]),
            ]
        );
        // two rows moved onto the same new key
        let err = db.try_write("UPDATE kt SET id = 50 WHERE id <= 2").unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        assert_eq!(db.rows("SELECT COUNT(*) FROM kt"), vec![r(&[big(3)])]);
        db.cleanup();
    }

    #[test]
    fn a_null_key_is_refused() {
        let db = t("p23_pk_null");
        assert!(db.try_write("UPDATE kt SET id = NULL WHERE id = 1").is_err());
        assert_eq!(db.rows("SELECT COUNT(*) FROM kt"), vec![r(&[big(3)])]);
        db.cleanup();
    }

    #[test]
    fn on_conflict_do_update_still_refuses_to_change_the_key() {
        let db = t("p23_pk_upsert");
        let err = db
            .try_write("INSERT INTO kt VALUES (1, 'x', 1) ON CONFLICT (id) DO UPDATE SET id = 9")
            .unwrap_err();
        assert!(matches!(err, SqlError::Unsupported { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_key_change_inside_a_transaction_is_visible_to_it_and_not_to_others_until_commit() {
        let db = t("p23_pk_txn");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "UPDATE kt SET id = 100 WHERE id = 1", &mut txn).unwrap();
        let inside = db.run_in_txn("SELECT id FROM kt ORDER BY id", &txn).unwrap().rows;
        assert_eq!(inside, vec![r(&[int(2)]), r(&[int(3)]), r(&[int(100)])]);
        assert_eq!(db.rows("SELECT COUNT(*) FROM kt WHERE id = 1"), vec![r(&[big(1)])]);
        txn.commit().unwrap();
        assert_eq!(db.rows("SELECT COUNT(*) FROM kt WHERE id = 1"), vec![r(&[big(0)])]);
        db.cleanup();
    }

    #[test]
    fn two_transactions_moving_a_row_to_the_same_key_one_wins() {
        let db = t("p23_pk_race");
        let mut a = db.txm.begin().unwrap();
        let mut b = db.txm.begin().unwrap();
        in_txn(&db, "UPDATE kt SET id = 77 WHERE id = 1", &mut a).unwrap();
        in_txn(&db, "UPDATE kt SET id = 77 WHERE id = 2", &mut b).unwrap();
        a.commit().unwrap();
        assert!(b.commit().is_err());
        assert_eq!(db.rows("SELECT COUNT(*) FROM kt WHERE id = 77"), vec![r(&[big(1)])]);
        db.cleanup();
    }
}

// =======================================================================
// Foreign keys: definition
// =======================================================================

mod fk_definition {
    use super::*;

    #[test]
    fn inline_and_table_level_declarations_both_record_a_key() {
        let db = Db::new("p23_def");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY)");
        db.write("CREATE TABLE inline_c (id INTEGER PRIMARY KEY, p INTEGER REFERENCES p (id))");
        db.write(
            "CREATE TABLE table_c (id INTEGER PRIMARY KEY, p INTEGER, \
             FOREIGN KEY (p) REFERENCES p (id) ON DELETE SET NULL ON UPDATE CASCADE)",
        );
        let fks = db.catalog.foreign_keys().unwrap();
        assert_eq!(fks.len(), 2);
        let by_table = |name: &str| fks.iter().find(|f| f.table_id == db.table_id(name)).unwrap();
        assert_eq!(by_table("inline_c").name, "inline_c_p_fkey");
        use rubixdb::catalog::schema::FkAction;
        assert_eq!(by_table("table_c").on_delete, FkAction::SetNull);
        assert_eq!(by_table("table_c").on_update, FkAction::Cascade);
        db.cleanup();
    }

    #[test]
    fn the_referenced_columns_must_be_the_primary_key_or_a_ready_unique_index() {
        let db = Db::new("p23_def_unique");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT, other TEXT)");
        db.write("CREATE TABLE c (id INTEGER PRIMARY KEY, code TEXT)");
        // not unique
        let err = db
            .try_write("ALTER TABLE c ADD FOREIGN KEY (code) REFERENCES p (code)")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        // a non-unique index does not make it unique
        db.write("CREATE INDEX p_code ON p (code)");
        assert!(db
            .try_write("ALTER TABLE c ADD FOREIGN KEY (code) REFERENCES p (code)")
            .is_err());
        // a unique index does
        db.write("CREATE UNIQUE INDEX p_other ON p (other)");
        db.write("ALTER TABLE c ADD COLUMN other TEXT");
        db.write("ALTER TABLE c ADD FOREIGN KEY (other) REFERENCES p (other)");
        // ... and cannot then be dropped from under the key
        let err = db.try_write("DROP INDEX p_other ON p").unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn composite_keys_need_matching_arity_and_types() {
        let db = Db::new("p23_def_composite");
        db.write("CREATE TABLE p (a INTEGER NOT NULL, b TEXT NOT NULL, PRIMARY KEY (a, b))");
        db.write("CREATE TABLE c (id INTEGER PRIMARY KEY, x INTEGER, y TEXT, z INTEGER)");
        // arity
        assert!(db
            .try_write("ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES p (a, b)")
            .is_err());
        // a prefix of the key is not unique
        assert!(db
            .try_write("ALTER TABLE c ADD FOREIGN KEY (x) REFERENCES p (a)")
            .is_err());
        // types
        assert!(db
            .try_write("ALTER TABLE c ADD FOREIGN KEY (z, y) REFERENCES p (a, b) ")
            .is_ok());
        assert!(db
            .try_write("ALTER TABLE c ADD CONSTRAINT bad FOREIGN KEY (y, x) REFERENCES p (a, b)")
            .is_err());
        db.cleanup();
    }

    #[test]
    fn set_null_needs_a_nullable_column_outside_the_primary_key() {
        let db = Db::new("p23_def_actions");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY)");
        let err = db
            .try_write(
                "CREATE TABLE c (id INTEGER PRIMARY KEY REFERENCES p (id) ON DELETE SET NULL)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        let err = db
            .try_write(
                "CREATE TABLE c (id INTEGER PRIMARY KEY, q INTEGER NOT NULL REFERENCES p (id) ON UPDATE SET NULL)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        // SET DEFAULT needs a default or a nullable column
        let err = db
            .try_write(
                "CREATE TABLE c (id INTEGER PRIMARY KEY, q INTEGER NOT NULL REFERENCES p (id) ON DELETE SET DEFAULT)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        // a failed CREATE TABLE leaves no table behind
        assert!(db.catalog.get_table_by_name(db.f.ctx.default_schema_id, "c").unwrap().is_none());
        // once a key sets a column to NULL, that column cannot become NOT NULL
        db.write("CREATE TABLE c2 (id INTEGER PRIMARY KEY, q INTEGER REFERENCES p (id) ON DELETE SET NULL)");
        assert!(db
            .try_write("ALTER TABLE c2 ALTER COLUMN q SET NOT NULL")
            .is_err());
        db.cleanup();
    }

    #[test]
    fn dropping_a_referenced_table_is_refused_and_dropping_the_child_removes_its_keys() {
        let db = fk_db("p23_def_drop", "");
        let err = db.try_write("DROP TABLE parent").unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert_eq!(db.catalog.foreign_keys().unwrap().len(), 1);
        db.write("DROP TABLE child");
        assert!(db.catalog.foreign_keys().unwrap().is_empty());
        db.write("DROP TABLE parent");
        db.cleanup();
    }

    #[test]
    fn drop_constraint_removes_the_key_and_orphans_become_possible() {
        let db = fk_db("p23_def_drop_fk", "");
        db.write("INSERT INTO parent VALUES (1, 'a')");
        assert!(db.try_write("INSERT INTO child VALUES (1, 99, 'x')").is_err());
        db.write("ALTER TABLE child DROP CONSTRAINT child_pid_fk");
        db.write("INSERT INTO child VALUES (1, 99, 'x')");
        // and the parent can be dropped now
        db.write("DROP TABLE child");
        db.write("DROP TABLE parent");
        db.cleanup();
    }
}

// =======================================================================
// Foreign keys: enforcement
// =======================================================================

mod fk_enforcement {
    use super::*;

    #[test]
    fn insert_of_a_valid_parent_reference_succeeds_and_a_missing_parent_is_refused() {
        let db = fk_db("p23_fk_insert", "");
        db.write("INSERT INTO parent VALUES (1, 'one')");
        db.write("INSERT INTO child VALUES (10, 1, 'ok')");
        let err = db.try_write("INSERT INTO child VALUES (11, 99, 'secret-note')").unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        let msg = err.to_string();
        // the message names the constraint and tables, never a value
        assert!(msg.contains("child_pid_fk") && msg.contains("child") && msg.contains("parent"), "{msg}");
        assert!(!msg.contains("99") && !msg.contains("secret-note"), "{msg}");
        assert_eq!(child_ids(&db), vec![r(&[int(10)])]);
        db.cleanup();
    }

    #[test]
    fn a_multi_row_insert_with_one_orphan_inserts_nothing() {
        let db = fk_db("p23_fk_multi", "");
        db.write("INSERT INTO parent VALUES (1, 'one')");
        let err = db
            .try_write("INSERT INTO child VALUES (10, 1, 'a'), (11, 1, 'b'), (12, 42, 'c')")
            .unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert!(child_ids(&db).is_empty());
        db.cleanup();
    }

    #[test]
    fn a_null_reference_is_not_checked() {
        let db = fk_db("p23_fk_null", "");
        db.write("INSERT INTO child VALUES (10, NULL, 'x')");
        db.write("INSERT INTO parent VALUES (1, 'one')");
        db.write("UPDATE child SET pid = 1 WHERE id = 10");
        db.write("UPDATE child SET pid = NULL WHERE id = 10");
        assert_eq!(db.rows("SELECT pid FROM child"), vec![r(&[NULL])]);
        db.cleanup();
    }

    #[test]
    fn update_of_the_reference_is_enforced() {
        let db = fk_db("p23_fk_update", "");
        seed(&db);
        db.write("UPDATE child SET pid = 3 WHERE id = 10");
        let err = db.try_write("UPDATE child SET pid = 77 WHERE id = 10").unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert_eq!(db.rows("SELECT pid FROM child WHERE id = 10"), vec![r(&[int(3)])]);
        // an update that does not touch the reference is never checked
        db.write("UPDATE child SET note = 'edited' WHERE id = 10");
        // a multi-row update with one bad row changes none
        let err = db
            .try_write("UPDATE child SET pid = CASE WHEN id = 11 THEN 88 ELSE 1 END WHERE id IN (10, 11)")
            .unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert_eq!(db.rows("SELECT pid FROM child WHERE id IN (10, 11) ORDER BY id"), vec![r(&[int(3)]), r(&[int(1)])]);
        db.cleanup();
    }

    #[test]
    fn upsert_is_enforced_on_the_insert_path_and_the_update_path() {
        let db = fk_db("p23_fk_upsert", "");
        seed(&db);
        // insert path
        assert!(is_fk_violation(
            &db.try_write("INSERT INTO child VALUES (40, 55, 'x') ON CONFLICT (id) DO NOTHING").unwrap_err()
        ));
        // update path: the row exists, DO UPDATE points it at a missing parent
        assert!(is_fk_violation(
            &db.try_write(
                "INSERT INTO child VALUES (10, 66, 'x') ON CONFLICT (id) DO UPDATE SET pid = EXCLUDED.pid"
            )
            .unwrap_err()
        ));
        assert_eq!(db.rows("SELECT pid FROM child WHERE id = 10"), vec![r(&[int(1)])]);
        // a valid upsert works on both paths
        db.write("INSERT INTO child VALUES (10, 2, 'x') ON CONFLICT (id) DO UPDATE SET pid = EXCLUDED.pid");
        db.write("INSERT INTO child VALUES (41, 2, 'y') ON CONFLICT (id) DO UPDATE SET pid = EXCLUDED.pid");
        assert_eq!(db.rows("SELECT pid FROM child WHERE id IN (10, 41) ORDER BY id"), vec![r(&[int(2)]), r(&[int(2)])]);
        // a row DO NOTHING skips is not checked
        db.write("INSERT INTO child VALUES (10, 999, 'skipped') ON CONFLICT (id) DO NOTHING");
        db.cleanup();
    }

    #[test]
    fn insert_select_is_enforced() {
        let db = fk_db("p23_fk_insert_select", "");
        db.write("INSERT INTO parent VALUES (1, 'a'), (2, 'b')");
        db.write("INSERT INTO child SELECT id + 100, id, name FROM parent");
        assert_eq!(child_ids(&db), vec![r(&[int(101)]), r(&[int(102)])]);
        assert!(is_fk_violation(
            &db.try_write("INSERT INTO child SELECT id + 200, id + 1000, name FROM parent").unwrap_err()
        ));
        assert_eq!(child_ids(&db).len(), 2);
        db.cleanup();
    }

    #[test]
    fn delete_of_a_referenced_parent_is_refused_under_no_action_and_restrict() {
        for actions in ["", "ON DELETE NO ACTION", "ON DELETE RESTRICT"] {
            let db = fk_db("p23_fk_restrict", actions);
            seed(&db);
            let err = db.try_write("DELETE FROM parent WHERE id = 1").unwrap_err();
            assert!(is_fk_violation(&err), "{actions}: {err:?}");
            assert_eq!(parent_ids(&db).len(), 3);
            // an unreferenced parent deletes fine
            db.write("DELETE FROM parent WHERE id = 3");
            // deleting the children first lets the parent go
            db.write("DELETE FROM child WHERE pid = 1");
            db.write("DELETE FROM parent WHERE id = 1");
            // and a statement that removes parent and child together is fine
            db.write("DELETE FROM child");
            db.cleanup();
        }
    }

    #[test]
    fn delete_cascades_through_several_levels() {
        let db = fk_db("p23_fk_cascade", "ON DELETE CASCADE");
        db.write(
            "CREATE TABLE grandchild (id INTEGER PRIMARY KEY, cid INTEGER REFERENCES child (id) ON DELETE CASCADE)",
        );
        seed(&db);
        db.write("INSERT INTO grandchild VALUES (100, 10), (101, 10), (102, 20)");
        let res = db.write("DELETE FROM parent WHERE id = 1");
        assert_eq!(res.rows_affected, 1); // the rows the statement named, not the cascade
        assert_eq!(parent_ids(&db), vec![r(&[int(2)]), r(&[int(3)])]);
        assert_eq!(child_ids(&db), vec![r(&[int(20)]), r(&[int(30)])]);
        assert_eq!(db.rows("SELECT id FROM grandchild"), vec![r(&[int(102)])]);
        db.cleanup();
    }

    #[test]
    fn delete_sets_null_in_the_children() {
        let db = fk_db("p23_fk_set_null", "ON DELETE SET NULL");
        seed(&db);
        db.write("DELETE FROM parent WHERE id = 1");
        assert_eq!(
            db.rows("SELECT id, pid FROM child ORDER BY id"),
            vec![
                r(&[int(10), NULL]),
                r(&[int(11), NULL]),
                r(&[int(20), int(2)]),
                r(&[int(30), NULL]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn delete_sets_the_default_in_the_children_and_the_default_must_itself_be_valid() {
        let db = Db::new("p23_fk_set_default");
        db.write("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)");
        db.write(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER DEFAULT 0, \
             FOREIGN KEY (pid) REFERENCES parent (id) ON DELETE SET DEFAULT)",
        );
        db.write("INSERT INTO parent VALUES (1, 'a'), (2, 'b')");
        db.write("INSERT INTO child VALUES (10, 1), (11, 2)");
        // the default (0) names no parent: the delete is refused and nothing changes
        let err = db.try_write("DELETE FROM parent WHERE id = 1").unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert_eq!(parent_ids(&db).len(), 2);
        assert_eq!(db.rows("SELECT pid FROM child WHERE id = 10"), vec![r(&[int(1)])]);
        // with a parent 0 it works
        db.write("INSERT INTO parent VALUES (0, 'default')");
        db.write("DELETE FROM parent WHERE id = 1");
        assert_eq!(db.rows("SELECT pid FROM child WHERE id = 10"), vec![r(&[int(0)])]);
        db.cleanup();
    }

    #[test]
    fn update_of_a_parent_key_cascades_to_the_children() {
        let db = fk_db("p23_fk_upd_cascade", "ON UPDATE CASCADE");
        db.write(
            "CREATE TABLE grandchild (id INTEGER PRIMARY KEY, cid INTEGER REFERENCES child (id) ON UPDATE CASCADE)",
        );
        seed(&db);
        db.write("INSERT INTO grandchild VALUES (100, 10)");
        db.write("UPDATE parent SET id = 10 WHERE id = 1");
        assert_eq!(
            db.rows("SELECT id, pid FROM child ORDER BY id"),
            vec![
                r(&[int(10), int(10)]),
                r(&[int(11), int(10)]),
                r(&[int(20), int(2)]),
                r(&[int(30), NULL]),
            ]
        );
        // the child's own key is referenced by the grandchild; changing the child key cascades too
        db.write("UPDATE child SET id = 500 WHERE id = 10");
        assert_eq!(db.rows("SELECT cid FROM grandchild"), vec![r(&[int(500)])]);
        db.cleanup();
    }

    #[test]
    fn update_of_a_parent_key_sets_null_or_is_refused() {
        let db = fk_db("p23_fk_upd_set_null", "ON UPDATE SET NULL");
        seed(&db);
        db.write("UPDATE parent SET id = 10 WHERE id = 1");
        assert_eq!(
            db.rows("SELECT id, pid FROM child WHERE id IN (10, 11) ORDER BY id"),
            vec![r(&[int(10), NULL]), r(&[int(11), NULL])]
        );
        let db2 = fk_db("p23_fk_upd_restrict", "ON UPDATE NO ACTION");
        seed(&db2);
        let err = db2.try_write("UPDATE parent SET id = 10 WHERE id = 1").unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        assert_eq!(parent_ids(&db2).len(), 3);
        // an unreferenced parent key may change; so may a non-key column of a referenced one
        db2.write("UPDATE parent SET id = 30 WHERE id = 3");
        db2.write("UPDATE parent SET name = 'renamed' WHERE id = 1");
        db.cleanup();
        db2.cleanup();
    }

    #[test]
    fn composite_keys_are_enforced_and_a_partial_null_is_exempt() {
        let db = Db::new("p23_fk_composite");
        db.write("CREATE TABLE p (a INTEGER NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (a, b))");
        db.write(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER, \
             FOREIGN KEY (x, y) REFERENCES p (a, b) ON DELETE CASCADE ON UPDATE CASCADE)",
        );
        db.write("INSERT INTO p VALUES (1, 1), (1, 2), (2, 1)");
        db.write("INSERT INTO c VALUES (10, 1, 2), (11, 2, 1), (12, 1, NULL), (13, NULL, NULL)");
        // (2, 2) is not a parent even though 2 and 2 each appear in one
        assert!(is_fk_violation(&db.try_write("INSERT INTO c VALUES (14, 2, 2)").unwrap_err()));
        // MATCH SIMPLE: any NULL exempts the row (12 references no row)
        db.write("INSERT INTO c VALUES (15, 99, NULL)");
        // cascade on delete and update
        db.write("UPDATE p SET b = 20 WHERE a = 1 AND b = 2");
        assert_eq!(db.rows("SELECT y FROM c WHERE id = 10"), vec![r(&[int(20)])]);
        db.write("DELETE FROM p WHERE a = 2 AND b = 1");
        assert_eq!(db.rows("SELECT COUNT(*) FROM c WHERE id = 11"), vec![r(&[big(0)])]);
        // the surviving rows are untouched
        assert_eq!(db.rows("SELECT COUNT(*) FROM c"), vec![r(&[big(4)])]);
        db.cleanup();
    }

    #[test]
    fn a_composite_key_over_a_unique_index_matches_columns_in_any_order() {
        let db = Db::new("p23_fk_unique_order");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY, a TEXT, b TEXT)");
        db.write("CREATE UNIQUE INDEX p_ab ON p (a, b)");
        db.write("INSERT INTO p VALUES (1, 'x', 'y')");
        db.write("CREATE TABLE c (id INTEGER PRIMARY KEY, q TEXT, r TEXT)");
        // referenced in the opposite order to the index
        db.write("ALTER TABLE c ADD FOREIGN KEY (q, r) REFERENCES p (b, a)");
        db.write("INSERT INTO c VALUES (1, 'y', 'x')"); // q=b='y', r=a='x'
        assert!(is_fk_violation(&db.try_write("INSERT INTO c VALUES (2, 'x', 'y')").unwrap_err()));
        db.cleanup();
    }

    #[test]
    fn a_key_over_a_unique_non_pk_column_is_enforced_and_cascades() {
        let db = Db::new("p23_fk_unique_ref");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT)");
        db.write("CREATE UNIQUE INDEX p_code ON p (code)");
        db.write(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, code TEXT REFERENCES p (code) ON UPDATE CASCADE ON DELETE CASCADE)",
        );
        db.write("INSERT INTO p VALUES (1, 'AA'), (2, 'BB')");
        db.write("INSERT INTO c VALUES (10, 'AA'), (11, 'BB')");
        assert!(is_fk_violation(&db.try_write("INSERT INTO c VALUES (12, 'ZZ')").unwrap_err()));
        db.write("UPDATE p SET code = 'AB' WHERE id = 1");
        assert_eq!(db.rows("SELECT code FROM c WHERE id = 10"), vec![r(&[text("AB")])]);
        db.write("DELETE FROM p WHERE code = 'BB'");
        assert_eq!(db.rows("SELECT COUNT(*) FROM c"), vec![r(&[big(1)])]);
        db.cleanup();
    }

    #[test]
    fn a_self_referencing_table_inserts_a_tree_in_one_statement_and_cascades_down() {
        let db = Db::new("p23_fk_self");
        db.write(
            "CREATE TABLE node (id INTEGER PRIMARY KEY, up INTEGER REFERENCES node (id) ON DELETE CASCADE)",
        );
        // parent and children in one statement, in either order
        db.write("INSERT INTO node VALUES (3, 2), (2, 1), (1, NULL)");
        assert!(is_fk_violation(&db.try_write("INSERT INTO node VALUES (9, 8)").unwrap_err()));
        db.write("INSERT INTO node VALUES (4, 3), (5, 1)");
        db.write("DELETE FROM node WHERE id = 2");
        assert_eq!(
            db.rows("SELECT id FROM node ORDER BY id"),
            vec![r(&[int(1)]), r(&[int(5)])]
        );
        db.cleanup();
    }

    #[test]
    fn several_keys_on_one_table_and_two_children_of_one_parent() {
        let db = Db::new("p23_fk_several");
        db.write("CREATE TABLE a (id INTEGER PRIMARY KEY)");
        db.write("CREATE TABLE b (id INTEGER PRIMARY KEY)");
        db.write(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a (id) ON DELETE CASCADE, \
             b_id INTEGER REFERENCES b (id) ON DELETE SET NULL)",
        );
        db.write("INSERT INTO a VALUES (1)");
        db.write("INSERT INTO b VALUES (1)");
        db.write("INSERT INTO c VALUES (10, 1, 1)");
        assert!(is_fk_violation(&db.try_write("INSERT INTO c VALUES (11, 1, 5)").unwrap_err()));
        assert!(is_fk_violation(&db.try_write("INSERT INTO c VALUES (11, 5, 1)").unwrap_err()));
        db.write("DELETE FROM b WHERE id = 1");
        assert_eq!(db.rows("SELECT a_id, b_id FROM c"), vec![r(&[int(1), NULL])]);
        db.write("DELETE FROM a WHERE id = 1");
        assert_eq!(db.rows("SELECT COUNT(*) FROM c"), vec![r(&[big(0)])]);
        db.cleanup();
    }

    #[test]
    fn a_foreign_key_can_be_added_to_a_table_with_valid_rows_and_not_to_one_with_orphans() {
        let db = Db::new("p23_fk_add");
        db.write("CREATE TABLE parent (id INTEGER PRIMARY KEY)");
        db.write("CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER)");
        db.write("INSERT INTO parent VALUES (1), (2)");
        db.write("INSERT INTO child VALUES (10, 1), (11, 2), (12, NULL)");
        db.write("ALTER TABLE child ADD CONSTRAINT child_fk FOREIGN KEY (pid) REFERENCES parent (id)");
        assert!(is_fk_violation(&db.try_write("INSERT INTO child VALUES (13, 9)").unwrap_err()));

        let db2 = Db::new("p23_fk_add_bad");
        db2.write("CREATE TABLE parent (id INTEGER PRIMARY KEY)");
        db2.write("CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER)");
        db2.write("INSERT INTO parent VALUES (1)");
        db2.write("INSERT INTO child VALUES (10, 1), (11, 77)");
        let err = db2
            .try_write("ALTER TABLE child ADD CONSTRAINT child_fk FOREIGN KEY (pid) REFERENCES parent (id)")
            .unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        // nothing was recorded, so the orphan insert still works
        assert!(db2.catalog.foreign_keys().unwrap().is_empty());
        db2.write("INSERT INTO child VALUES (12, 78)");
        db.cleanup();
        db2.cleanup();
    }

    #[test]
    fn indexes_on_the_child_do_not_change_what_is_enforced() {
        let db = fk_db("p23_fk_indexed", "ON DELETE CASCADE");
        db.write("CREATE INDEX child_pid ON child (pid)");
        seed(&db);
        db.write("DELETE FROM parent WHERE id = 1");
        assert_eq!(child_ids(&db), vec![r(&[int(20)]), r(&[int(30)])]);
        db.cleanup();
    }

    #[test]
    fn a_child_keyed_by_the_parent_reference_uses_the_key_prefix() {
        let db = Db::new("p23_fk_pk_prefix");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY)");
        db.write(
            "CREATE TABLE c (pid INTEGER NOT NULL, seq INTEGER NOT NULL, PRIMARY KEY (pid, seq), \
             FOREIGN KEY (pid) REFERENCES p (id) ON DELETE CASCADE ON UPDATE CASCADE)",
        );
        db.write("INSERT INTO p VALUES (1), (2)");
        db.write("INSERT INTO c VALUES (1, 1), (1, 2), (2, 1)");
        // the child's key contains the reference: an ON UPDATE CASCADE rewrites the keys
        db.write("UPDATE p SET id = 10 WHERE id = 1");
        assert_eq!(
            db.rows("SELECT pid, seq FROM c ORDER BY pid, seq"),
            vec![r(&[int(2), int(1)]), r(&[int(10), int(1)]), r(&[int(10), int(2)])]
        );
        db.write("DELETE FROM p WHERE id = 10");
        assert_eq!(db.rows("SELECT COUNT(*) FROM c"), vec![r(&[big(1)])]);
        db.cleanup();
    }
}

// =======================================================================
// Foreign keys: statement atomicity, cascades that fail, explicit transactions
// =======================================================================

mod fk_atomicity {
    use super::*;

    #[test]
    fn a_failing_statement_inside_a_transaction_undoes_only_itself() {
        let db = fk_db("p23_atomic_stmt", "");
        seed(&db);
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO child VALUES (40, 3, 'kept')", &mut txn).unwrap();
        // this statement buffers a good row, then trips over an orphan
        let err = in_txn(
            &db,
            "INSERT INTO child VALUES (41, 3, 'good'), (42, 99, 'bad')",
            &mut txn,
        )
        .unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        // a restrict failure after the statement already buffered its delete
        let err = in_txn(&db, "DELETE FROM parent WHERE id = 2", &mut txn).unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        let rows = db
            .run_in_txn("SELECT id FROM child WHERE id >= 40 ORDER BY id", &txn)
            .unwrap()
            .rows;
        assert_eq!(rows, vec![r(&[int(40)])]); // the first statement's row, nothing of the others
        assert_eq!(
            db.run_in_txn("SELECT COUNT(*) FROM parent", &txn).unwrap().rows,
            vec![r(&[big(3)])]
        );
        txn.commit().unwrap();
        assert_eq!(db.rows("SELECT COUNT(*) FROM child WHERE id >= 40"), vec![r(&[big(1)])]);
        db.cleanup();
    }

    #[test]
    fn a_cascade_that_exceeds_the_write_set_cap_fails_closed_and_leaves_the_transaction_untouched() {
        let db = fk_db("p23_atomic_cap", "ON DELETE CASCADE");
        db.write("INSERT INTO parent VALUES (1, 'a'), (2, 'b')");
        for chunk in 0..3 {
            let values: Vec<String> = (0..10)
                .map(|i| format!("({}, 1, 'x')", chunk * 10 + i + 100))
                .collect();
            db.write(&format!("INSERT INTO child VALUES {}", values.join(", ")));
        }
        db.write("INSERT INTO child VALUES (500, 2, 'y')");
        let txm = TransactionManager::with_limits(
            Arc::clone(&db.f.engine),
            Arc::clone(&db.store),
            TxnLimits {
                max_write_set_ops: 12,
                ..TxnLimits::default()
            },
        );
        let mut txn = txm.begin().unwrap();
        in_txn(&db, "DELETE FROM child WHERE id = 500", &mut txn).unwrap(); // 1 op
        let err = in_txn(&db, "DELETE FROM parent WHERE id = 1", &mut txn).unwrap_err(); // 31 ops
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        // the earlier statement is intact; the failed one left nothing
        assert_eq!(
            db.run_in_txn("SELECT COUNT(*) FROM parent", &txn).unwrap().rows,
            vec![r(&[big(2)])]
        );
        assert_eq!(
            db.run_in_txn("SELECT COUNT(*) FROM child", &txn).unwrap().rows,
            vec![r(&[big(30)])]
        );
        txn.commit().unwrap();
        assert_eq!(parent_ids(&db).len(), 2);
        assert_eq!(db.rows("SELECT COUNT(*) FROM child"), vec![r(&[big(30)])]);
        db.cleanup();
    }

    #[test]
    fn an_autocommit_cascade_past_the_cap_changes_nothing_at_all() {
        // the same cascade as an autocommit statement, run through a manager with a small cap
        let db = fk_db("p23_atomic_cap_auto", "ON DELETE CASCADE");
        db.write("INSERT INTO parent VALUES (1, 'a')");
        let values: Vec<String> = (0..30).map(|i| format!("({}, 1, 'x')", i + 100)).collect();
        db.write(&format!("INSERT INTO child VALUES {}", values.join(", ")));
        let txm = TransactionManager::with_limits(
            Arc::clone(&db.f.engine),
            Arc::clone(&db.store),
            TxnLimits {
                max_write_set_ops: 10,
                ..TxnLimits::default()
            },
        );
        let plan = db.plan("DELETE FROM parent WHERE id = 1");
        let err = crate::exec::write::execute_write_autocommit(
            &plan,
            &txm,
            &db.store,
            &db.catalog,
            &db.builder,
            &[],
            &ExecLimits::default(),
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert_eq!(parent_ids(&db).len(), 1);
        assert_eq!(db.rows("SELECT COUNT(*) FROM child"), vec![r(&[big(30)])]);
        db.cleanup();
    }

    #[test]
    fn rows_a_transaction_inserted_can_be_referenced_and_deleted_within_it() {
        let db = fk_db("p23_atomic_in_txn", "ON DELETE CASCADE ON UPDATE CASCADE");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO parent VALUES (7, 'new')", &mut txn).unwrap();
        in_txn(&db, "INSERT INTO child VALUES (70, 7, 'c')", &mut txn).unwrap();
        in_txn(&db, "UPDATE parent SET id = 8 WHERE id = 7", &mut txn).unwrap(); // cascades in the txn
        assert_eq!(
            db.run_in_txn("SELECT pid FROM child", &txn).unwrap().rows,
            vec![r(&[int(8)])]
        );
        in_txn(&db, "INSERT INTO parent VALUES (9, 'gone')", &mut txn).unwrap();
        in_txn(&db, "INSERT INTO child VALUES (90, 9, 'c')", &mut txn).unwrap();
        in_txn(&db, "DELETE FROM parent WHERE id = 9", &mut txn).unwrap();
        txn.commit().unwrap();
        assert_eq!(db.rows("SELECT id, pid FROM child"), vec![r(&[int(70), int(8)])]);
        assert_eq!(parent_ids(&db), vec![r(&[int(8)])]);
        db.cleanup();
    }

    #[test]
    fn a_rolled_back_cascade_leaves_the_data_alone() {
        let db = fk_db("p23_atomic_rollback", "ON DELETE CASCADE");
        seed(&db);
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "DELETE FROM parent WHERE id = 1", &mut txn).unwrap();
        txn.rollback().unwrap();
        assert_eq!(parent_ids(&db).len(), 3);
        assert_eq!(child_ids(&db).len(), 4);
        db.cleanup();
    }
}

// =======================================================================
// Foreign keys: concurrency
// =======================================================================

mod fk_concurrency {
    use super::*;

    /// Child-before-parent: the child transaction is open (its statement saw the
    /// parent) when the parent's deletion commits.
    #[test]
    fn child_insert_open_while_the_parent_is_deleted_cannot_commit() {
        let db = fk_db("p23_conc_cp", "");
        db.write("INSERT INTO parent VALUES (1, 'a')");
        let mut t1 = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO child VALUES (10, 1, 'x')", &mut t1).unwrap();
        db.write("DELETE FROM parent WHERE id = 1"); // commits: no child is visible to it
        let err = t1.commit().unwrap_err();
        assert!(
            matches!(err, rubixdb::relational::RelationalError::ForeignKeyViolation { .. }),
            "{err:?}"
        );
        assert!(child_ids(&db).is_empty());
        assert!(parent_ids(&db).is_empty());
        db.cleanup();
    }

    /// Parent-before-child: the child committed after the parent's transaction began.
    #[test]
    fn parent_delete_open_while_a_child_is_inserted_cannot_commit() {
        let db = fk_db("p23_conc_pc", "");
        db.write("INSERT INTO parent VALUES (1, 'a')");
        let mut t1 = db.txm.begin().unwrap();
        in_txn(&db, "DELETE FROM parent WHERE id = 1", &mut t1).unwrap(); // sees no child
        db.write("INSERT INTO child VALUES (10, 1, 'x')"); // commits first
        let err = t1.commit().unwrap_err();
        assert!(
            matches!(err, rubixdb::relational::RelationalError::ForeignKeyViolation { .. }),
            "{err:?}"
        );
        assert_eq!(child_ids(&db), vec![r(&[int(10)])]);
        assert_eq!(parent_ids(&db), vec![r(&[int(1)])]);
        db.cleanup();
    }

    /// With ON DELETE CASCADE the same race must not leave an orphan either: the
    /// delete, which did not see the new child, cannot commit over it.
    #[test]
    fn a_cascading_delete_does_not_orphan_a_child_inserted_meanwhile() {
        let db = fk_db("p23_conc_cascade", "ON DELETE CASCADE");
        db.write("INSERT INTO parent VALUES (1, 'a')");
        let mut t1 = db.txm.begin().unwrap();
        in_txn(&db, "DELETE FROM parent WHERE id = 1", &mut t1).unwrap();
        db.write("INSERT INTO child VALUES (10, 1, 'x')");
        assert!(t1.commit().is_err());
        assert_eq!(child_ids(&db).len(), 1);
        assert_eq!(parent_ids(&db).len(), 1);
        // retrying the delete now sees the child and cascades it
        db.write("DELETE FROM parent WHERE id = 1");
        assert!(child_ids(&db).is_empty());
        db.cleanup();
    }

    #[test]
    fn an_update_that_moves_a_reference_races_a_parent_delete_safely() {
        let db = fk_db("p23_conc_update", "");
        db.write("INSERT INTO parent VALUES (1, 'a'), (2, 'b')");
        db.write("INSERT INTO child VALUES (10, 1, 'x')");
        let mut t1 = db.txm.begin().unwrap();
        in_txn(&db, "UPDATE child SET pid = 2 WHERE id = 10", &mut t1).unwrap();
        db.write("DELETE FROM parent WHERE id = 2"); // parent 2 has no committed child
        assert!(t1.commit().is_err());
        assert_eq!(db.rows("SELECT pid FROM child"), vec![r(&[int(1)])]);
        db.cleanup();
    }

    /// Threads racing inserts of children against deletes of their parents, through
    /// the SQL layer: whatever interleaves, no child may reference a missing parent.
    #[test]
    fn racing_sql_writers_never_leave_an_orphan() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        let db = Arc::new(fk_db("p23_conc_stress", ""));
        for p in 0..20 {
            db.write(&format!("INSERT INTO parent VALUES ({p}, 'p')"));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for w in 0..3 {
            let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
            handles.push(thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    i += 1;
                    let p = (i * 7 + w) % 20;
                    let _ = db.try_write(&format!(
                        "INSERT INTO child VALUES ({}, {p}, 'c')",
                        w * 1_000_000 + i
                    ));
                }
            }));
        }
        {
            let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
            handles.push(thread::spawn(move || {
                for round in 0..60 {
                    let p = (round * 3) % 20;
                    let _ = db.try_write(&format!("DELETE FROM parent WHERE id = {p}"));
                    let _ = db.try_write(&format!("INSERT INTO parent VALUES ({p}, 'again')"));
                }
                stop.store(true, Ordering::Relaxed);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let orphans = db.rows(
            "SELECT COUNT(*) FROM child WHERE pid IS NOT NULL AND pid NOT IN (SELECT id FROM parent)",
        );
        assert_eq!(orphans, vec![r(&[big(0)])], "an orphan child exists");
        Arc::try_unwrap(db).ok().unwrap().cleanup();
    }

    #[test]
    fn adding_a_key_while_children_are_inserted_leaves_no_orphan() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        for round in 0..6 {
            let db = Arc::new(Db::new(&format!("p23_conc_addfk_{round}")));
            db.write("CREATE TABLE parent (id INTEGER PRIMARY KEY)");
            db.write("CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER)");
            db.write("INSERT INTO parent VALUES (1)");
            let stop = Arc::new(AtomicBool::new(false));
            let writer = {
                let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
                thread::spawn(move || {
                    let mut i = 0;
                    while !stop.load(Ordering::Relaxed) {
                        i += 1;
                        let pid = if i % 4 == 0 { 999 } else { 1 };
                        let _ = db.try_write(&format!("INSERT INTO child VALUES ({i}, {pid})"));
                    }
                })
            };
            std::thread::sleep(std::time::Duration::from_millis(5));
            let added = db.try_write(
                "ALTER TABLE child ADD CONSTRAINT fk FOREIGN KEY (pid) REFERENCES parent (id)",
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
            let orphans = db.rows("SELECT COUNT(*) FROM child WHERE pid NOT IN (SELECT id FROM parent)");
            match added {
                Ok(_) => assert_eq!(orphans, vec![r(&[big(0)])], "round {round}"),
                Err(e) => {
                    // refused because orphans were already there
                    assert!(is_fk_violation(&e), "round {round}: {e:?}");
                }
            }
            Arc::try_unwrap(db).ok().unwrap().cleanup();
        }
    }
}

// =======================================================================
// Restart and recovery
// =======================================================================

mod fk_restart {
    use super::*;

    #[test]
    fn definitions_and_enforcement_survive_a_restart() {
        let db = fk_db("p23_restart_defs", "ON DELETE CASCADE ON UPDATE CASCADE");
        seed(&db);
        let db = reopen(db);
        assert_eq!(db.catalog.foreign_keys().unwrap().len(), 1);
        assert!(is_fk_violation(&db.try_write("INSERT INTO child VALUES (99, 77, 'x')").unwrap_err()));
        db.write("UPDATE parent SET id = 10 WHERE id = 2");
        assert_eq!(db.rows("SELECT pid FROM child WHERE id = 20"), vec![r(&[int(10)])]);
        db.cleanup();
    }

    #[test]
    fn a_committed_cascade_is_whole_after_a_restart() {
        let db = fk_db("p23_restart_cascade", "ON DELETE CASCADE");
        db.write(
            "CREATE TABLE grandchild (id INTEGER PRIMARY KEY, cid INTEGER REFERENCES child (id) ON DELETE CASCADE)",
        );
        seed(&db);
        db.write("INSERT INTO grandchild VALUES (100, 10), (101, 20)");
        db.write("DELETE FROM parent WHERE id = 1");
        let db = reopen(db);
        assert_eq!(parent_ids(&db), vec![r(&[int(2)]), r(&[int(3)])]);
        assert_eq!(child_ids(&db), vec![r(&[int(20)]), r(&[int(30)])]);
        assert_eq!(db.rows("SELECT id FROM grandchild"), vec![r(&[int(101)])]);
        // the state is consistent: nothing references a missing parent
        assert_eq!(
            db.rows("SELECT COUNT(*) FROM child WHERE pid IS NOT NULL AND pid NOT IN (SELECT id FROM parent)"),
            vec![r(&[big(0)])]
        );
        db.cleanup();
    }

    #[test]
    fn an_uncommitted_cascade_is_nowhere_after_a_restart() {
        let db = fk_db("p23_restart_uncommitted", "ON DELETE CASCADE");
        seed(&db);
        {
            let mut txn = db.txm.begin().unwrap();
            in_txn(&db, "DELETE FROM parent WHERE id = 1", &mut txn).unwrap();
            assert_eq!(
                db.run_in_txn("SELECT COUNT(*) FROM child", &txn).unwrap().rows,
                vec![r(&[big(2)])]
            );
            // the process ends here: the transaction is never committed (dropping it is
            // an implicit rollback; either way nothing was ever sent to the engine)
            drop(txn);
        }
        let db = reopen(db);
        assert_eq!(parent_ids(&db).len(), 3);
        assert_eq!(child_ids(&db).len(), 4);
        db.cleanup();
    }

    #[test]
    fn a_dropped_key_stays_dropped_and_a_dropped_table_takes_its_keys() {
        let db = fk_db("p23_restart_drop", "");
        db.write("ALTER TABLE child DROP CONSTRAINT child_pid_fk");
        let db = reopen(db);
        assert!(db.catalog.foreign_keys().unwrap().is_empty());
        db.write("INSERT INTO child VALUES (1, 55, 'orphan ok')");
        db.cleanup();
    }
}

// =======================================================================
// Authorization
// =======================================================================

mod authorization {
    use super::*;
    use rubixdb::catalog::schema::{ObjectKind, Privilege};

    #[test]
    fn alter_needs_ddl_and_a_foreign_key_needs_select_on_the_referenced_table() {
        let db = fk_db("p23_auth", "");
        let who = crate::auth::AuthContext::none("worker");
        let limits = SqlLimits::default();
        let child = db.table_id("child");
        let parent = db.table_id("parent");

        // no privilege at all: the table does not even appear to exist
        assert!(matches!(
            db.try_bind_with("ALTER TABLE child ADD COLUMN x INTEGER", &who, &limits).unwrap_err(),
            SqlError::UnknownObject { .. }
        ));
        // Select is not enough to alter
        db.catalog.grant("worker", ObjectKind::Table, child, Privilege::Select).unwrap();
        assert!(db
            .try_bind_with("ALTER TABLE child ADD COLUMN x INTEGER", &who, &limits)
            .is_err());
        // Ddl on the child allows ALTER ... but a key needs to read the parent
        db.catalog.grant("worker", ObjectKind::Table, child, Privilege::Ddl).unwrap();
        assert!(db
            .try_bind_with("ALTER TABLE child ADD COLUMN x INTEGER", &who, &limits)
            .is_ok());
        db.write("ALTER TABLE child DROP CONSTRAINT child_pid_fk");
        assert!(db
            .try_bind_with(
                "ALTER TABLE child ADD FOREIGN KEY (pid) REFERENCES parent (id)",
                &who,
                &limits
            )
            .is_err());
        db.catalog.grant("worker", ObjectKind::Table, parent, Privilege::Select).unwrap();
        assert!(db
            .try_bind_with(
                "ALTER TABLE child ADD FOREIGN KEY (pid) REFERENCES parent (id)",
                &who,
                &limits
            )
            .is_ok());
        // a read-only principal cannot alter at all
        let reader = crate::auth::AuthContext::reader("ro");
        assert!(db
            .try_bind_with("ALTER TABLE child ADD COLUMN x INTEGER", &reader, &limits)
            .is_err());
        db.cleanup();
    }

    #[test]
    fn a_write_to_a_child_needs_no_privilege_on_the_parent_it_references() {
        // The foreign-key check runs as the engine, not as the caller (as in PostgreSQL):
        // inserting a child row reveals only whether the parent exists, which the
        // violation error already says, and never any value of the parent.
        let db = fk_db("p23_auth_child", "");
        db.write("INSERT INTO parent VALUES (1, 'secret parent name')");
        let who = crate::auth::AuthContext::none("worker");
        db.catalog
            .grant("worker", ObjectKind::Table, db.table_id("child"), Privilege::Insert)
            .unwrap();
        let bound = db.try_bind_with("INSERT INTO child VALUES (1, 1, 'x')", &who, &SqlLimits::default());
        assert!(bound.is_ok(), "{bound:?}");
        db.cleanup();
    }
}

// =======================================================================
// More enforcement paths
// =======================================================================

mod fk_paths {
    use super::*;

    #[test]
    fn a_statement_touching_many_parents_finds_their_children_in_one_pass_when_the_child_has_no_index() {
        // no index on child.pid and pid is not a prefix of the child key: the multi-parent
        // statement reads the child table once for all the changed keys
        let db = fk_db("p23_paths_batch", "ON DELETE CASCADE ON UPDATE CASCADE");
        seed(&db);
        db.write("UPDATE parent SET id = id + 100");
        assert_eq!(
            db.rows("SELECT id, pid FROM child ORDER BY id"),
            vec![
                r(&[int(10), int(101)]),
                r(&[int(11), int(101)]),
                r(&[int(20), int(102)]),
                r(&[int(30), NULL]),
            ]
        );
        let res = db.write("DELETE FROM parent");
        assert_eq!(res.rows_affected, 3);
        assert_eq!(child_ids(&db), vec![r(&[int(30)])]);
        db.cleanup();
    }

    #[test]
    fn a_large_cascade_is_complete() {
        let db = fk_db("p23_paths_large", "ON DELETE CASCADE");
        db.write("INSERT INTO parent VALUES (1, 'a'), (2, 'b')");
        for chunk in 0..6 {
            let values: Vec<String> = (0..100)
                .map(|i| format!("({}, {}, 'x')", chunk * 100 + i, 1 + (i % 2)))
                .collect();
            db.write(&format!("INSERT INTO child VALUES {}", values.join(", ")));
        }
        db.write("DELETE FROM parent WHERE id = 1");
        assert_eq!(db.rows("SELECT COUNT(*) FROM child"), vec![r(&[big(300)])]);
        assert_eq!(db.rows("SELECT COUNT(*) FROM child WHERE pid <> 2"), vec![r(&[big(0)])]);
        db.cleanup();
    }

    #[test]
    fn text_and_decimal_keys_are_matched_exactly() {
        let db = Db::new("p23_paths_types");
        db.write("CREATE TABLE p (code TEXT PRIMARY KEY, amount DECIMAL(10, 2) NOT NULL)");
        db.write("CREATE UNIQUE INDEX p_amount ON p (amount)");
        db.write(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, code TEXT REFERENCES p (code) ON DELETE CASCADE, \
             amount DECIMAL(10, 2) REFERENCES p (amount) ON DELETE SET NULL)",
        );
        db.write("INSERT INTO p VALUES ('AB', 12.50), ('CD', 7.25)");
        db.write("INSERT INTO c VALUES (1, 'AB', 12.50), (2, 'CD', NULL)");
        assert!(is_fk_violation(&db.try_write("INSERT INTO c VALUES (3, 'ab', NULL)").unwrap_err())); // case matters
        assert!(is_fk_violation(&db.try_write("INSERT INTO c VALUES (3, NULL, 12.51)").unwrap_err()));
        db.write("DELETE FROM p WHERE code = 'CD'");
        assert_eq!(db.rows("SELECT COUNT(*) FROM c"), vec![r(&[big(1)])]);
        db.cleanup();
    }

    #[test]
    fn an_upsert_that_changes_a_referenced_unique_column_applies_on_update() {
        let db = Db::new("p23_paths_upsert_parent");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY, code TEXT)");
        db.write("CREATE UNIQUE INDEX p_code ON p (code)");
        db.write("CREATE TABLE c (id INTEGER PRIMARY KEY, code TEXT REFERENCES p (code) ON UPDATE CASCADE)");
        db.write("INSERT INTO p VALUES (1, 'old')");
        db.write("INSERT INTO c VALUES (10, 'old')");
        db.write("INSERT INTO p VALUES (1, 'new') ON CONFLICT (id) DO UPDATE SET code = EXCLUDED.code");
        assert_eq!(db.rows("SELECT code FROM c"), vec![r(&[text("new")])]);
        db.cleanup();
    }

    #[test]
    fn deleting_from_a_table_that_is_only_a_child_and_the_if_exists_forms_work() {
        let db = fk_db("p23_paths_child_only", "");
        seed(&db);
        let res = db.write("DELETE FROM child WHERE pid = 1");
        assert_eq!(res.rows_affected, 2);
        db.write("ALTER TABLE child DROP CONSTRAINT IF EXISTS never_existed");
        db.write("DROP TABLE IF EXISTS no_such_table");
        db.cleanup();
    }

    #[test]
    fn rows_affected_counts_the_statement_rows_not_the_cascade() {
        let db = fk_db("p23_paths_counts", "ON DELETE CASCADE ON UPDATE CASCADE");
        seed(&db);
        assert_eq!(db.write("UPDATE parent SET id = 11 WHERE id = 1").rows_affected, 1);
        assert_eq!(db.write("DELETE FROM parent WHERE id = 11").rows_affected, 1);
        db.cleanup();
    }

    #[test]
    fn a_delete_in_a_transaction_sees_the_children_the_transaction_itself_inserted() {
        let db = fk_db("p23_paths_own_children", "ON DELETE CASCADE");
        db.write("INSERT INTO parent VALUES (1, 'a')");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO child VALUES (10, 1, 'mine')", &mut txn).unwrap();
        in_txn(&db, "DELETE FROM parent WHERE id = 1", &mut txn).unwrap(); // cascades the txn's own row
        assert_eq!(
            db.run_in_txn("SELECT COUNT(*) FROM child", &txn).unwrap().rows,
            vec![r(&[big(0)])]
        );
        txn.commit().unwrap();
        assert!(child_ids(&db).is_empty());
        db.cleanup();
    }

    #[test]
    fn a_table_with_a_key_to_itself_and_to_another_table_cascades_both_ways() {
        let db = Db::new("p23_paths_mixed");
        db.write("CREATE TABLE org (id INTEGER PRIMARY KEY)");
        db.write(
            "CREATE TABLE emp (id INTEGER PRIMARY KEY, boss INTEGER REFERENCES emp (id) ON DELETE SET NULL, \
             org INTEGER REFERENCES org (id) ON DELETE CASCADE)",
        );
        db.write("INSERT INTO org VALUES (1), (2)");
        db.write("INSERT INTO emp VALUES (1, NULL, 1), (2, 1, 1), (3, 2, 2)");
        db.write("DELETE FROM org WHERE id = 1"); // removes emp 1 and 2; emp 3 boss (2) goes null
        assert_eq!(
            db.rows("SELECT id, boss FROM emp ORDER BY id"),
            vec![r(&[int(3), NULL])]
        );
        db.cleanup();
    }
}

// =======================================================================
// Catalog-level definition checks
// =======================================================================

mod catalog_level {
    use super::*;
    use rubixdb::catalog::schema::FkAction;
    use rubixdb::catalog::service::NewForeignKey;

    #[test]
    fn the_foreign_key_cache_follows_every_change_and_the_generation_moves() {
        let db = fk_db("p23_cat_cache", "");
        let g0 = db.catalog.foreign_key_generation();
        assert_eq!(db.catalog.foreign_keys().unwrap().len(), 1);
        db.write("CREATE TABLE other (id INTEGER PRIMARY KEY, pid INTEGER)");
        db.write("ALTER TABLE other ADD FOREIGN KEY (pid) REFERENCES parent (id)");
        assert!(db.catalog.foreign_key_generation() > g0);
        assert_eq!(db.catalog.foreign_keys().unwrap().len(), 2);
        let g1 = db.catalog.foreign_key_generation();
        db.write("ALTER TABLE other DROP CONSTRAINT other_pid_fkey");
        assert!(db.catalog.foreign_key_generation() > g1);
        assert_eq!(db.catalog.foreign_keys().unwrap().len(), 1);
        db.write("DROP TABLE child");
        assert!(db.catalog.foreign_keys().unwrap().is_empty());
        db.cleanup();
    }

    #[test]
    fn check_foreign_key_validates_the_definition() {
        let db = fk_db("p23_cat_defs", "");
        let child = db.table_id("child");
        let parent = db.table_id("parent");
        let key = |name: &str, child_ordinals: Vec<u16>, parent_ordinals: Vec<u16>| NewForeignKey {
            table_id: child,
            name: name.to_string(),
            child_ordinals,
            parent_table_id: parent,
            parent_ordinals,
            on_delete: FkAction::NoAction,
            on_update: FkAction::NoAction,
        };
        assert!(db.catalog.check_foreign_key(&key("k", vec![], vec![])).is_err()); // empty
        assert!(db.catalog.check_foreign_key(&key("k", vec![1], vec![0, 1])).is_err()); // arity
        assert!(db.catalog.check_foreign_key(&key("k", vec![1, 1], vec![0, 0])).is_err()); // repeated
        assert!(db.catalog.check_foreign_key(&key("k", vec![9], vec![0])).is_err()); // no such column
        assert!(db.catalog.check_foreign_key(&key("k", vec![2], vec![0])).is_err()); // TEXT vs INTEGER
        assert!(db.catalog.check_foreign_key(&key("k", vec![1], vec![1])).is_err()); // TEXT vs INTEGER, not unique
        assert!(db.catalog.check_foreign_key(&key("child_pid_fk", vec![1], vec![0])).is_err()); // name taken
        // pid -> id is a valid definition under a free name, and resolves to the primary key
        assert_eq!(db.catalog.check_foreign_key(&key("k", vec![1], vec![0])).unwrap(), None);
        db.cleanup();
    }
}

mod fk_snapshot_views {
    use super::*;

    /// The statement-time check reads the latest committed parents as well as the
    /// transaction's snapshot: a parent committed after BEGIN can be referenced, and
    /// the commit (the authority) confirms it.
    #[test]
    fn a_parent_committed_after_begin_can_be_referenced_inside_the_transaction() {
        let db = fk_db("p23_view_late_parent", "");
        let mut txn = db.txm.begin().unwrap();
        db.write("INSERT INTO parent VALUES (5, 'late')"); // committed after the snapshot
        in_txn(&db, "INSERT INTO child VALUES (10, 5, 'x')", &mut txn).unwrap();
        txn.commit().unwrap();
        assert_eq!(db.rows("SELECT pid FROM child"), vec![r(&[int(5)])]);
        db.cleanup();
    }

    /// ... but a parent that exists nowhere is still refused at the statement.
    #[test]
    fn a_parent_that_exists_nowhere_is_refused_at_the_statement() {
        let db = fk_db("p23_view_no_parent", "");
        let mut txn = db.txm.begin().unwrap();
        let err = in_txn(&db, "INSERT INTO child VALUES (10, 5, 'x')", &mut txn).unwrap_err();
        assert!(is_fk_violation(&err), "{err:?}");
        db.cleanup();
    }

    /// A child committed by someone else after BEGIN is invisible to the deleting
    /// transaction's snapshot but protects its parent at commit.
    #[test]
    fn a_child_committed_after_begin_still_protects_its_parent_at_commit() {
        let db = fk_db("p23_view_late_child", "");
        db.write("INSERT INTO parent VALUES (1, 'a')");
        let mut txn = db.txm.begin().unwrap();
        db.write("INSERT INTO child VALUES (10, 1, 'x')");
        in_txn(&db, "DELETE FROM parent WHERE id = 1", &mut txn).unwrap(); // snapshot sees no child
        assert!(txn.commit().is_err());
        assert_eq!(parent_ids(&db).len(), 1);
        db.cleanup();
    }
}

// =======================================================================
// CHECK constraints
// =======================================================================

mod check_constraints {
    use super::*;

    fn is_check_violation(e: &SqlError) -> bool {
        matches!(e, SqlError::CheckViolation { .. })
    }

    /// `acct(id PK, balance INTEGER, kind TEXT)` with `CHECK (balance >= 0)` (column-level, named
    /// by the engine) and a named table-level `CHECK (kind IN ('a', 'b') OR kind IS NULL)`.
    fn acct(tag: &str) -> Db {
        let db = Db::new(tag);
        db.write(
            "CREATE TABLE acct (id INTEGER PRIMARY KEY, balance INTEGER CHECK (balance >= 0), kind TEXT, \
             CONSTRAINT acct_kind_ck CHECK (kind IN ('a', 'b') OR kind IS NULL))",
        );
        db
    }

    #[test]
    fn declarations_are_recorded_with_names_and_the_columns_they_read() {
        let db = acct("p23_ck_decl");
        let checks = db.catalog.check_constraints_for(db.table_id("acct")).unwrap();
        assert_eq!(checks.len(), 2);
        let by_name = |n: &str| checks.iter().find(|c| c.name == n).unwrap();
        assert_eq!(by_name("acct_balance_check").column_ordinals, vec![1]);
        assert_eq!(by_name("acct_kind_ck").column_ordinals, vec![2]);
        assert!(by_name("acct_kind_ck")
            .check_expression
            .as_deref()
            .unwrap()
            .contains("kind"));
        db.cleanup();
    }

    #[test]
    fn insert_is_checked_and_a_violation_inserts_nothing_and_names_no_value() {
        let db = acct("p23_ck_insert");
        db.write("INSERT INTO acct VALUES (1, 10, 'a'), (2, 0, NULL)");
        // a NULL makes the condition unknown, which passes
        db.write("INSERT INTO acct VALUES (3, NULL, 'b')");
        let err = db
            .try_write("INSERT INTO acct VALUES (4, 5, 'a'), (5, -777, 'a')")
            .unwrap_err();
        assert!(is_check_violation(&err), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("acct_balance_check") && msg.contains("acct"), "{msg}");
        assert!(!msg.contains("777"), "{msg}");
        let err = db.try_write("INSERT INTO acct VALUES (6, 1, 'zzz')").unwrap_err();
        assert!(is_check_violation(&err) && err.to_string().contains("acct_kind_ck"), "{err:?}");
        assert_eq!(db.rows("SELECT COUNT(*) FROM acct"), vec![r(&[big(3)])]);
        db.cleanup();
    }

    #[test]
    fn update_upsert_and_insert_select_are_checked_too() {
        let db = acct("p23_ck_paths");
        db.write("INSERT INTO acct VALUES (1, 10, 'a'), (2, 20, 'b')");
        // UPDATE: one bad row fails the whole statement
        let err = db.try_write("UPDATE acct SET balance = balance - 15").unwrap_err();
        assert!(is_check_violation(&err), "{err:?}");
        assert_eq!(db.rows("SELECT balance FROM acct ORDER BY id"), vec![r(&[int(10)]), r(&[int(20)])]);
        db.write("UPDATE acct SET balance = balance - 5");
        // UPSERT, both paths
        assert!(is_check_violation(
            &db.try_write("INSERT INTO acct VALUES (9, -1, 'a') ON CONFLICT (id) DO NOTHING").unwrap_err()
        ));
        assert!(is_check_violation(
            &db.try_write(
                "INSERT INTO acct VALUES (1, -1, 'a') ON CONFLICT (id) DO UPDATE SET balance = EXCLUDED.balance"
            )
            .unwrap_err()
        ));
        // a row DO NOTHING skips is not checked, and neither is an unchanged DO UPDATE
        db.write("INSERT INTO acct VALUES (1, -1, 'a') ON CONFLICT (id) DO NOTHING");
        // INSERT ... SELECT
        db.write("CREATE TABLE src (id INTEGER PRIMARY KEY, b INTEGER)");
        db.write("INSERT INTO src VALUES (100, 1), (101, -1)");
        assert!(is_check_violation(
            &db.try_write("INSERT INTO acct (id, balance) SELECT id, b FROM src").unwrap_err()
        ));
        db.write("INSERT INTO acct (id, balance) SELECT id, b FROM src WHERE b > 0");
        assert_eq!(db.rows("SELECT COUNT(*) FROM acct"), vec![r(&[big(3)])]);
        db.cleanup();
    }

    #[test]
    fn add_check_validates_every_existing_row_and_records_nothing_on_failure() {
        let db = Db::new("p23_ck_add");
        db.write("CREATE TABLE people (id INTEGER PRIMARY KEY, age INTEGER)");
        db.write("INSERT INTO people VALUES (1, 30), (2, NULL), (3, -4)");
        let id = db.table_id("people");
        let before = (
            db.catalog.get_table(id).unwrap().unwrap().schema_version,
            db.catalog.check_constraints_for(id).unwrap().len(),
        );
        let err = db
            .try_write("ALTER TABLE people ADD CONSTRAINT age_ok CHECK (age >= 0)")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert_eq!(
            (
                db.catalog.get_table(id).unwrap().unwrap().schema_version,
                db.catalog.check_constraints_for(id).unwrap().len()
            ),
            before
        );
        db.write("DELETE FROM people WHERE id = 3");
        db.write("ALTER TABLE people ADD CONSTRAINT age_ok CHECK (age >= 0)"); // the NULL row passes
        assert!(is_check_violation(&db.try_write("INSERT INTO people VALUES (4, -1)").unwrap_err()));
        // the same name twice
        assert!(db
            .try_write("ALTER TABLE people ADD CONSTRAINT age_ok CHECK (age < 200)")
            .is_err());
        // DROP removes it
        db.write("ALTER TABLE people DROP CONSTRAINT age_ok");
        db.write("INSERT INTO people VALUES (4, -1)");
        db.cleanup();
    }

    #[test]
    fn what_a_constraint_may_contain_is_limited() {
        let db = Db::new("p23_ck_forms");
        db.write("CREATE TABLE t2 (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)");
        for (sql, why) in [
            ("ALTER TABLE t2 ADD CHECK (a IN (SELECT id FROM t2))", "subquery"),
            ("ALTER TABLE t2 ADD CHECK (COUNT(*) > 0)", "aggregate"),
            ("ALTER TABLE t2 ADD CHECK (a > $1)", "parameter"),
            ("ALTER TABLE t2 ADD CHECK (a + 1)", "not boolean"),
            ("ALTER TABLE t2 ADD CHECK (nope > 1)", "unknown column"),
        ] {
            assert!(db.try_write(sql).is_err(), "{why}: {sql}");
        }
        assert!(db.catalog.check_constraints().unwrap().is_empty());
        // a failing CREATE TABLE leaves no table
        assert!(db
            .try_write("CREATE TABLE t3 (id INTEGER PRIMARY KEY, CHECK (zzz > 0))")
            .is_err());
        assert!(db.catalog.get_table_by_name(db.f.ctx.default_schema_id, "t3").unwrap().is_none());
        // allowed: operators, functions, CASE, BETWEEN, LIKE, IN, IS NULL, several columns
        db.write(
            "ALTER TABLE t2 ADD CONSTRAINT rich CHECK (a BETWEEN 0 AND 100 AND (b LIKE 'x%' OR b IS NULL) \
             AND CASE WHEN a > 50 THEN b IS NOT NULL ELSE TRUE END AND (LENGTH(b) < 10 OR b IS NULL))",
        );
        db.write("INSERT INTO t2 VALUES (1, 10, NULL), (2, 60, 'xy')");
        assert!(db.try_write("INSERT INTO t2 VALUES (3, 60, NULL)").is_err());
        assert!(db.try_write("INSERT INTO t2 VALUES (3, 5, 'nope')").is_err());
        db.cleanup();
    }

    #[test]
    fn a_column_a_check_reads_cannot_be_renamed_but_others_can() {
        let db = acct("p23_ck_rename");
        let err = db
            .try_write("ALTER TABLE acct RENAME COLUMN balance TO funds")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert!(err.to_string().contains("acct_balance_check"), "{err}");
        db.write("ALTER TABLE acct DROP CONSTRAINT acct_balance_check");
        db.write("ALTER TABLE acct RENAME COLUMN balance TO funds");
        db.write("ALTER TABLE acct ADD CONSTRAINT funds_ok CHECK (funds >= 0)");
        db.cleanup();
    }

    #[test]
    fn a_cascaded_foreign_key_update_must_satisfy_the_childs_checks() {
        let db = Db::new("p23_ck_cascade");
        db.write("CREATE TABLE p (id INTEGER PRIMARY KEY)");
        db.write(
            "CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER CHECK (pid IS NOT NULL), \
             FOREIGN KEY (pid) REFERENCES p (id) ON DELETE SET NULL)",
        );
        db.write("INSERT INTO p VALUES (1), (2)");
        db.write("INSERT INTO c VALUES (10, 1), (20, 2)");
        // SET NULL would write a row the CHECK forbids: the statement fails, nothing changes
        let err = db.try_write("DELETE FROM p WHERE id = 1").unwrap_err();
        assert!(is_check_violation(&err), "{err:?}");
        assert_eq!(db.rows("SELECT COUNT(*) FROM p"), vec![r(&[big(2)])]);
        assert_eq!(db.rows("SELECT pid FROM c WHERE id = 10"), vec![r(&[int(1)])]);
        db.cleanup();
    }

    #[test]
    fn a_runtime_error_in_a_check_fails_the_statement_and_writes_nothing() {
        let db = Db::new("p23_ck_runtime");
        db.write("CREATE TABLE d (id INTEGER PRIMARY KEY, x INTEGER, CHECK (10 / x > 1 OR x IS NULL))");
        db.write("INSERT INTO d VALUES (1, 2)");
        let err = db.try_write("INSERT INTO d VALUES (2, 3), (3, 0)").unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert_eq!(db.rows("SELECT COUNT(*) FROM d"), vec![r(&[big(1)])]);
        db.cleanup();
    }

    #[test]
    fn checks_survive_a_restart_and_a_dropped_table_takes_its_checks() {
        let db = acct("p23_ck_restart");
        db.write("INSERT INTO acct VALUES (1, 10, 'a')");
        db.write("ALTER TABLE acct ADD CONSTRAINT id_pos CHECK (id > 0)");
        let db = reopen(db);
        assert_eq!(db.catalog.check_constraints().unwrap().len(), 3);
        assert!(is_check_violation(&db.try_write("INSERT INTO acct VALUES (2, -5, 'a')").unwrap_err()));
        assert!(is_check_violation(&db.try_write("INSERT INTO acct VALUES (-2, 5, 'a')").unwrap_err()));
        db.write("DROP TABLE acct");
        assert!(db.catalog.check_constraints().unwrap().is_empty());
        db.cleanup();
    }

    #[test]
    fn a_transaction_open_across_add_check_conflicts_and_a_racing_insert_never_slips_past() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        let db = Db::new("p23_ck_race");
        db.write("CREATE TABLE q (id INTEGER PRIMARY KEY, v INTEGER)");
        // the open-transaction case
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO q VALUES (1, -5)", &mut txn).unwrap();
        db.write("ALTER TABLE q ADD CONSTRAINT v_ok CHECK (v >= 0)");
        assert!(txn.commit().is_err());
        assert_eq!(db.rows("SELECT COUNT(*) FROM q"), vec![r(&[big(0)])]);
        db.write("ALTER TABLE q DROP CONSTRAINT v_ok");
        // the racing case: after ADD CHECK succeeds, no violating row exists
        let db = Arc::new(db);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
            thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    i += 1;
                    let v = if i % 3 == 0 { -1 } else { 1 };
                    let _ = db.try_write(&format!("INSERT INTO q VALUES ({i}, {v})"));
                }
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(10));
        let added = db.try_write("ALTER TABLE q ADD CONSTRAINT v_ok CHECK (v >= 0)");
        std::thread::sleep(std::time::Duration::from_millis(10));
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        if added.is_ok() {
            assert_eq!(db.rows("SELECT COUNT(*) FROM q WHERE v < 0"), vec![r(&[big(0)])]);
        }
        Arc::try_unwrap(db).ok().unwrap().cleanup();
    }
}
