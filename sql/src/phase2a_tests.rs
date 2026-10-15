//! SQL Phase 2a -- DML extensions: `RETURNING`, `INSERT ... SELECT`,
//! `INSERT ... ON CONFLICT DO NOTHING | DO UPDATE` (with `EXCLUDED`).
//!
//! Every test drives the real parse, bind, plan and write-executor pipeline against
//! a real engine (`mission1_support::Db`); the HTTP tests are in
//! `api/tests/api_sql_phase2a.rs`.
//!
//! What the tests pin, in the words of the mission:
//!
//! * `RETURNING` reports the rows the statement changed -- the count equals
//!   `rows_affected`, the values are the ones written (or deleted), the order is the
//!   order of change;
//! * an upsert is atomic: concurrent same-key upserts end as one row and no lost
//!   update;
//! * the failure behaviour of every multi-row path is defined (see
//!   `statement_atomicity`).

use rubixdb::relational::{RelationalValue, Transaction};

use crate::error::SqlError;
use crate::exec::write::{execute_write, execute_write_autocommit, WriteMetrics, WriteResult};
use crate::exec::{CancellationToken, ExecLimits};
use crate::mission1_support::*;

fn r(vals: &[V]) -> Row {
    vals.to_vec()
}

/// `kv(k INTEGER PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT 7)` with a UNIQUE
/// index on `v` (`kv_v_uq`).
fn kv(tag: &str) -> Db {
    let db = Db::new(tag);
    db.write("CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT, n INTEGER NOT NULL DEFAULT 7)");
    db.write("CREATE UNIQUE INDEX kv_v_uq ON kv (v)");
    db
}

fn returned(res: &WriteResult) -> &Vec<Row> {
    &res.returning
        .as_ref()
        .expect("the statement has RETURNING")
        .rows
}

/// Runs `sql` inside `txn` (the statement is executed, not committed).
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

fn kv_rows(db: &Db) -> Vec<Row> {
    db.rows("SELECT k, v, n FROM kv ORDER BY k")
}

// =======================================================================
// RETURNING
// =======================================================================

mod returning {
    use super::*;

    #[test]
    fn a_multi_row_insert_returns_every_row_it_inserted() {
        let db = kv("p2a_ret_insert");
        let res = db
            .write("INSERT INTO kv (k, v) VALUES (1, 'a'), (2, 'b'), (3, NULL) RETURNING k, v, n");
        assert_eq!(res.rows_affected, 3);
        // the DEFAULT of `n` is part of the row that was written
        assert_eq!(
            returned(&res),
            &vec![
                r(&[int(1), text("a"), int(7)]),
                r(&[int(2), text("b"), int(7)]),
                r(&[int(3), NULL, int(7)]),
            ]
        );
        assert_eq!(returned(&res).len() as u64, res.rows_affected);
        // what came back is what is stored
        assert_eq!(kv_rows(&db), *returned(&res));
        db.cleanup();
    }

    #[test]
    fn returning_names_types_expressions_wildcards_and_parameters() {
        let db = kv("p2a_ret_shapes");
        let res = db.write("INSERT INTO kv (k, v) VALUES (1, 'a') RETURNING *");
        let schema = &res.returning.as_ref().unwrap().schema;
        let names: Vec<_> = schema.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["k", "v", "n"]);
        assert_eq!(returned(&res), &vec![r(&[int(1), text("a"), int(7)])]);

        let res = db.write(
            "INSERT INTO kv (k, v) VALUES (2, 'b') RETURNING k + 100 AS shifted, UPPER(v), kv.n",
        );
        let schema = &res.returning.as_ref().unwrap().schema;
        let names: Vec<_> = schema.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names[0], "shifted");
        assert_eq!(returned(&res), &vec![r(&[int(102), text("B"), int(7)])]);

        let res = db.write("INSERT INTO kv (k, v) VALUES (3, 'c') RETURNING kv.*");
        assert_eq!(returned(&res), &vec![r(&[int(3), text("c"), int(7)])]);

        // a parameter in RETURNING
        let plan = db.plan("INSERT INTO kv (k, v) VALUES (4, 'd') RETURNING k, $1");
        let res = execute_write_autocommit(
            &plan,
            &db.txm,
            &db.store,
            &db.catalog,
            &db.builder,
            &[text("p")],
            &ExecLimits::default(),
            &WriteMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(returned(&res), &vec![r(&[int(4), text("p")])]);
        db.cleanup();
    }

    #[test]
    fn update_returning_gives_the_new_values_of_exactly_the_rows_it_updated() {
        let db = kv("p2a_ret_update");
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30)");
        let res = db.write("UPDATE kv SET n = n + 1, v = UPPER(v) WHERE k <= 2 RETURNING k, v, n");
        assert_eq!(res.rows_affected, 2);
        assert_eq!(
            returned(&res),
            &vec![
                r(&[int(1), text("A"), int(11)]),
                r(&[int(2), text("B"), int(21)])
            ]
        );
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(1), text("A"), int(11)]),
                r(&[int(2), text("B"), int(21)]),
                r(&[int(3), text("c"), int(30)]),
            ]
        );
        // a row whose values do not change is still matched, counted and returned
        let res = db.write("UPDATE kv SET n = n WHERE k = 3 RETURNING k, n");
        assert_eq!(res.rows_affected, 1);
        assert_eq!(returned(&res), &vec![r(&[int(3), int(30)])]);
        // nothing matched: an empty result, not no result
        let res = db.write("UPDATE kv SET n = 0 WHERE k = 99 RETURNING k");
        assert_eq!(res.rows_affected, 0);
        assert!(returned(&res).is_empty());
        assert!(res.returning.is_some());
        db.cleanup();
    }

    #[test]
    fn update_with_a_subquery_in_set_still_returns_the_new_rows() {
        let db = kv("p2a_ret_update_sq");
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 10), (2, 'b', 20)");
        let res =
            db.write("UPDATE kv SET n = (SELECT MAX(n) FROM kv) + k WHERE k > 0 RETURNING k, n");
        // every SET sees the table as it was before the statement: MAX(n) = 20
        assert_eq!(
            returned(&res),
            &vec![r(&[int(1), int(21)]), r(&[int(2), int(22)])]
        );
        assert_eq!(
            kv_rows(&db)
                .iter()
                .map(|x| x[2].clone())
                .collect::<Vec<_>>(),
            vec![int(21), int(22)]
        );
        db.cleanup();
    }

    #[test]
    fn delete_returning_gives_the_deleted_rows_as_they_were() {
        let db = kv("p2a_ret_delete");
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', 30)");
        let res = db.write("DELETE FROM kv WHERE n >= 20 RETURNING k, v, n, n * 2 AS twice");
        assert_eq!(res.rows_affected, 2);
        assert_eq!(
            returned(&res),
            &vec![
                r(&[int(2), text("b"), int(20), int(40)]),
                r(&[int(3), text("c"), int(30), int(60)]),
            ]
        );
        assert_eq!(kv_rows(&db), vec![r(&[int(1), text("a"), int(10)])]);
        // none matched
        let res = db.write("DELETE FROM kv WHERE k = 99 RETURNING *");
        assert_eq!(res.rows_affected, 0);
        assert!(returned(&res).is_empty());
        // the whole table
        let res = db.write("DELETE FROM kv RETURNING k");
        assert_eq!(returned(&res), &vec![r(&[int(1)])]);
        assert!(kv_rows(&db).is_empty());
        db.cleanup();
    }

    #[test]
    fn the_returned_count_is_the_changed_count_on_every_path() {
        let db = kv("p2a_ret_count");
        let n = |res: &WriteResult| returned(res).len() as u64;
        let res =
            db.write("INSERT INTO kv (k, v) VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d') RETURNING k");
        assert_eq!((n(&res), res.rows_affected), (4, 4));
        let res = db.write("UPDATE kv SET n = 1 WHERE k % 2 = 0 RETURNING k");
        assert_eq!((n(&res), res.rows_affected), (2, 2));
        let res = db.write(
            "INSERT INTO kv (k, v) VALUES (4,'x'),(5,'e') ON CONFLICT (k) DO NOTHING RETURNING k",
        );
        assert_eq!((n(&res), res.rows_affected), (1, 1));
        let res = db.write(
            "INSERT INTO kv (k, v) VALUES (4,'x'),(6,'f') ON CONFLICT (k) DO UPDATE SET n = 99 RETURNING k, n",
        );
        assert_eq!((n(&res), res.rows_affected), (2, 2));
        let res = db.write("DELETE FROM kv WHERE n = 99 RETURNING k");
        assert_eq!((n(&res), res.rows_affected), (1, 1));
        db.cleanup();
    }

    #[test]
    fn returning_inside_a_transaction_is_returned_at_once_and_follows_the_transaction() {
        let db = kv("p2a_ret_txn");
        let mut txn = db.txm.begin().unwrap();
        let res = in_txn(
            &db,
            "INSERT INTO kv (k, v) VALUES (1, 'a') RETURNING k, v",
            &mut txn,
        )
        .unwrap();
        assert_eq!(returned(&res), &vec![r(&[int(1), text("a")])]);
        // invisible to everyone else until commit
        assert!(kv_rows(&db).is_empty());
        let res = in_txn(&db, "UPDATE kv SET n = 8 WHERE k = 1 RETURNING n", &mut txn).unwrap();
        assert_eq!(returned(&res), &vec![r(&[int(8)])]);
        txn.commit().unwrap();
        assert_eq!(kv_rows(&db), vec![r(&[int(1), text("a"), int(8)])]);

        // after a rollback nothing the RETURNING reported exists
        let mut txn = db.txm.begin().unwrap();
        let res = in_txn(&db, "DELETE FROM kv WHERE k = 1 RETURNING k", &mut txn).unwrap();
        assert_eq!(returned(&res), &vec![r(&[int(1)])]);
        txn.rollback().unwrap();
        assert_eq!(kv_rows(&db).len(), 1);
        db.cleanup();
    }

    #[test]
    fn a_statement_that_fails_returns_no_rows() {
        let db = kv("p2a_ret_fail");
        db.write("INSERT INTO kv (k, v) VALUES (1, 'a')");
        let err = db
            .try_write("INSERT INTO kv (k, v) VALUES (2, 'b'), (1, 'dup') RETURNING k")
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        assert_eq!(kv_rows(&db).len(), 1);
        db.cleanup();
    }

    #[test]
    fn returning_is_bounded() {
        let db = kv("p2a_ret_limit");
        db.write("INSERT INTO kv (k, v) VALUES (1,'a'),(2,'b'),(3,'c')");
        let limits = ExecLimits {
            max_result_rows: 2,
            ..ExecLimits::default()
        };
        let err = db
            .try_write_with("DELETE FROM kv RETURNING k", &limits)
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        // the refused statement changed nothing
        assert_eq!(kv_rows(&db).len(), 3);
        let err = db
            .try_write_with("UPDATE kv SET n = 1 RETURNING k", &limits)
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(kv_rows(&db).iter().all(|row| row[2] == int(7)));
        db.cleanup();
    }

    #[test]
    fn what_a_returning_clause_may_contain() {
        let db = kv("p2a_ret_errors");
        for (sql, what) in [
            (
                "INSERT INTO kv (k) VALUES (1) RETURNING COUNT(*)",
                "aggregate",
            ),
            (
                "INSERT INTO kv (k) VALUES (1) RETURNING ROW_NUMBER() OVER ()",
                "window",
            ),
            (
                "INSERT INTO kv (k) VALUES (1) RETURNING (SELECT 1)",
                "subquery",
            ),
            (
                "INSERT INTO kv (k) VALUES (1) RETURNING nosuch",
                "unknown column",
            ),
            ("DELETE FROM kv RETURNING other.*", "unknown qualifier"),
            (
                "UPDATE kv SET n = 1 RETURNING excluded.k",
                "EXCLUDED outside ON CONFLICT",
            ),
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(
                matches!(
                    err,
                    SqlError::TypeMismatch { .. }
                        | SqlError::UnknownObject { .. }
                        | SqlError::Unsupported { .. }
                        | SqlError::InvalidIdentifier { .. }
                ),
                "{what}: {sql}: {err:?}"
            );
        }
        db.cleanup();
    }

    #[test]
    fn explain_shows_returning() {
        let db = kv("p2a_ret_explain");
        for (sql, needle) in [
            (
                "INSERT INTO kv (k) VALUES (1) RETURNING k, n AS count",
                "returning=[k, count]",
            ),
            ("UPDATE kv SET n = 1 RETURNING k", "returning=[k]"),
            ("DELETE FROM kv RETURNING *", "returning=[k, v, n]"),
        ] {
            let text = crate::plan::explain(&db.plan(sql));
            assert!(text.contains(needle), "{sql}: {text}");
        }
        db.cleanup();
    }
}

// =======================================================================
// INSERT ... SELECT
// =======================================================================

mod insert_select {
    use super::*;

    fn src(tag: &str) -> Db {
        let db = kv(tag);
        db.write("CREATE TABLE src (id INTEGER PRIMARY KEY, label TEXT, qty INTEGER, big BIGINT)");
        db.write("INSERT INTO src VALUES (1, 'one', 10, 100), (2, 'two', 20, 200), (3, 'three', NULL, 300)");
        db
    }

    #[test]
    fn rows_are_copied_with_a_column_list_defaults_and_returning() {
        let db = src("p2a_is_basic");
        let res = db.write(
            "INSERT INTO kv (k, v) SELECT id, label FROM src WHERE id < 3 RETURNING k, v, n",
        );
        assert_eq!(res.rows_affected, 2);
        assert_eq!(
            returned(&res),
            &vec![
                r(&[int(1), text("one"), int(7)]),
                r(&[int(2), text("two"), int(7)])
            ]
        );
        // reordered target columns, a different source shape
        db.write("DELETE FROM kv");
        db.write("INSERT INTO kv (v, k) SELECT label, id + 10 FROM src");
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(11), text("one"), int(7)]),
                r(&[int(12), text("two"), int(7)]),
                r(&[int(13), text("three"), int(7)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn the_source_is_read_before_anything_is_written() {
        let db = src("p2a_is_self");
        // a statement that reads the table it inserts into sees it as it was
        let res = db.write("INSERT INTO src SELECT id + 10, label, qty, big FROM src");
        assert_eq!(res.rows_affected, 3);
        assert_eq!(db.rows("SELECT COUNT(*) FROM src"), vec![r(&[big(6)])]);
        db.cleanup();
    }

    #[test]
    fn source_query_forms() {
        let db = src("p2a_is_forms");
        // ORDER BY / LIMIT / OFFSET inside the source
        db.write(
            "INSERT INTO kv (k, v) SELECT id, label FROM src ORDER BY id DESC LIMIT 2 OFFSET 1",
        );
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(1), text("one"), int(7)]),
                r(&[int(2), text("two"), int(7)])
            ]
        );
        db.write("DELETE FROM kv");
        // an aggregate, a set operation, a CTE written before the statement, one inside it
        db.write("INSERT INTO kv (k, n) SELECT 1, SUM(qty) FROM src");
        assert_eq!(db.rows("SELECT n FROM kv WHERE k = 1"), vec![r(&[int(30)])]);
        db.write("DELETE FROM kv");
        db.write("INSERT INTO kv (k, v) SELECT id, label FROM src WHERE id = 1 UNION ALL SELECT id + 100, label FROM src WHERE id = 2");
        assert_eq!(
            db.rows("SELECT k FROM kv ORDER BY k"),
            vec![r(&[int(1)]), r(&[int(102)])]
        );
        db.write("DELETE FROM kv");
        db.write("WITH s AS (SELECT id, label FROM src WHERE id <= 2) INSERT INTO kv (k, v) SELECT id, label FROM s");
        assert_eq!(
            db.rows("SELECT k FROM kv ORDER BY k"),
            vec![r(&[int(1)]), r(&[int(2)])]
        );
        db.write("DELETE FROM kv");
        db.write("INSERT INTO kv (k, v) WITH s AS (SELECT id, label FROM src WHERE id = 3) SELECT id, label FROM s");
        assert_eq!(
            db.rows("SELECT k, v FROM kv"),
            vec![r(&[int(3), text("three")])]
        );
        // empty source: nothing, and RETURNING gives an empty result
        let res =
            db.write("INSERT INTO kv (k, v) SELECT id, label FROM src WHERE id > 99 RETURNING k");
        assert_eq!(res.rows_affected, 0);
        assert!(returned(&res).is_empty());
        db.cleanup();
    }

    #[test]
    fn numeric_widening_is_allowed_and_nothing_else_converts() {
        let db = src("p2a_is_types");
        db.write("CREATE TABLE wide (id BIGINT PRIMARY KEY, d DOUBLE, bg BIGINT)");
        // INTEGER -> BIGINT, INTEGER -> DOUBLE, BIGINT stays
        let res = db.write(
            "INSERT INTO wide SELECT id, qty, big FROM src WHERE id < 3 RETURNING id, d, bg",
        );
        assert_eq!(
            returned(&res),
            &vec![
                r(&[big(1), Some(RelationalValue::Double(10.0)), big(100)]),
                r(&[big(2), Some(RelationalValue::Double(20.0)), big(200)]),
            ]
        );
        // BIGINT -> INTEGER is the one narrowing, checked per value: 100, 200, 300 fit ...
        db.write("INSERT INTO kv (k) SELECT big FROM src");
        assert_eq!(
            db.rows("SELECT k FROM kv ORDER BY k"),
            vec![r(&[int(100)]), r(&[int(200)]), r(&[int(300)])]
        );
        // ... a value that does not is an error and nothing is stored
        db.write("DELETE FROM kv");
        db.write("INSERT INTO src VALUES (4, 'huge', 1, 9000000000)");
        let err = db
            .try_write("INSERT INTO kv (k) SELECT big FROM src")
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        assert!(db.rows("SELECT k FROM kv").is_empty());
        // DOUBLE -> INTEGER and DOUBLE -> BIGINT never convert
        let err = db
            .try_bind("INSERT INTO wide (id) SELECT d FROM wide")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // TEXT -> INTEGER never converts
        let err = db
            .try_bind("INSERT INTO kv (k) SELECT label FROM src")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn column_count_and_not_null_errors_change_nothing() {
        let db = src("p2a_is_errors");
        let err = db
            .try_bind("INSERT INTO kv (k, v) SELECT id FROM src")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = db
            .try_bind("INSERT INTO kv (k, v) SELECT id, label, qty FROM src")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // a column without a value and without a default
        db.write("CREATE TABLE strict (a INTEGER PRIMARY KEY, b INTEGER NOT NULL)");
        let err = db
            .try_bind("INSERT INTO strict (a) SELECT id FROM src")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // a NULL arriving in a NOT NULL column at run time (qty of id 3)
        let err = db
            .try_write("INSERT INTO strict (a, b) SELECT id, qty FROM src")
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        assert!(
            db.rows("SELECT a FROM strict").is_empty(),
            "no row of the failed statement exists"
        );
        db.cleanup();
    }

    #[test]
    fn the_row_count_is_bounded() {
        let db = src("p2a_is_limit");
        let limits = ExecLimits {
            max_dml_target_rows: 2,
            ..ExecLimits::default()
        };
        let err = db
            .try_write_with("INSERT INTO kv (k, v) SELECT id, label FROM src", &limits)
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(kv_rows(&db).is_empty());
        db.cleanup();
    }

    #[test]
    fn explain_shows_the_source_plan() {
        let db = src("p2a_is_explain");
        let text = crate::plan::explain(
            &db.plan("INSERT INTO kv (k, v) SELECT id, label FROM src WHERE id = 1"),
        );
        assert!(text.contains("source=select"), "{text}");
        assert!(text.contains("PkLookup"), "{text}");
        db.cleanup();
    }
}

fn reopen(db: Db) -> Db {
    use std::sync::Arc;
    let Db {
        f,
        catalog,
        store,
        builder,
        txm,
    } = db;
    // Drop everything layered on the old engine so its WAL lock is released.
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

// =======================================================================
// ON CONFLICT
// =======================================================================

mod on_conflict {
    use super::*;

    fn seeded(tag: &str) -> Db {
        let db = kv(tag);
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 10), (2, 'b', 20)");
        db
    }

    #[test]
    fn do_nothing_skips_the_conflicting_row_and_inserts_the_rest() {
        let db = seeded("p2a_oc_nothing");
        let res = db.write(
            "INSERT INTO kv (k, v, n) VALUES (1, 'zz', 99), (3, 'c', 30) ON CONFLICT (k) DO NOTHING RETURNING k",
        );
        // the skipped row is neither counted nor returned
        assert_eq!(res.rows_affected, 1);
        assert_eq!(returned(&res), &vec![r(&[int(3)])]);
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(1), text("a"), int(10)]),
                r(&[int(2), text("b"), int(20)]),
                r(&[int(3), text("c"), int(30)]),
            ]
        );
        // everything skipped: zero rows, an empty (not absent) RETURNING
        let res =
            db.write("INSERT INTO kv (k, v) VALUES (1, 'q') ON CONFLICT DO NOTHING RETURNING k");
        assert_eq!(res.rows_affected, 0);
        assert!(returned(&res).is_empty());
        db.cleanup();
    }

    #[test]
    fn do_nothing_without_a_target_arbitrates_on_every_unique_constraint() {
        let db = seeded("p2a_oc_nothing_any");
        // `v = 'a'` collides on the unique index, not the primary key
        let res = db.write("INSERT INTO kv (k, v) VALUES (9, 'a') ON CONFLICT DO NOTHING");
        assert_eq!(res.rows_affected, 0);
        assert_eq!(kv_rows(&db).len(), 2);
        // naming only the PK does not cover the unique index: that violation is an error
        let err = db
            .try_write("INSERT INTO kv (k, v) VALUES (9, 'a') ON CONFLICT (k) DO NOTHING")
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        assert_eq!(kv_rows(&db).len(), 2);
        db.cleanup();
    }

    #[test]
    fn do_update_replaces_the_row_and_excluded_is_the_proposed_row() {
        let db = seeded("p2a_oc_update");
        let res = db.write(
            "INSERT INTO kv (k, v, n) VALUES (1, 'a', 500), (4, 'd', 40) \
             ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n + kv.n, v = UPPER(EXCLUDED.v) \
             RETURNING k, v, n",
        );
        assert_eq!(res.rows_affected, 2);
        assert_eq!(
            returned(&res),
            &vec![
                // existing n (10) + proposed n (500); v from the proposed row
                r(&[int(1), text("A"), int(510)]),
                r(&[int(4), text("d"), int(40)]),
            ]
        );
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(1), text("A"), int(510)]),
                r(&[int(2), text("b"), int(20)]),
                r(&[int(4), text("d"), int(40)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn excluded_carries_the_defaults_of_omitted_columns() {
        let db = seeded("p2a_oc_excluded_default");
        // `n` is not in the column list: EXCLUDED.n is its DEFAULT (7), not NULL
        db.write(
            "INSERT INTO kv (k, v) VALUES (2, 'x') ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n",
        );
        assert_eq!(db.rows("SELECT n FROM kv WHERE k = 2"), vec![r(&[int(7)])]);
        db.cleanup();
    }

    #[test]
    fn do_update_where_filters_which_conflicts_update() {
        let db = seeded("p2a_oc_where");
        let res = db.write(
            "INSERT INTO kv (k, v, n) VALUES (1, 'a', 0), (2, 'b', 0) \
             ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n WHERE kv.n > 15 RETURNING k",
        );
        // only k = 2 passes (n = 20 > 15); the filtered row is not counted or returned
        assert_eq!(res.rows_affected, 1);
        assert_eq!(returned(&res), &vec![r(&[int(2)])]);
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(1), text("a"), int(10)]),
                r(&[int(2), text("b"), int(0)])
            ]
        );
        db.cleanup();
    }

    #[test]
    fn a_unique_index_can_be_the_conflict_target() {
        let db = seeded("p2a_oc_index_target");
        let res = db.write(
            "INSERT INTO kv (k, v, n) VALUES (99, 'b', 1) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n RETURNING k, v, n",
        );
        // the row that conflicted on `v` is the one updated; its key is kept
        assert_eq!(returned(&res), &vec![r(&[int(2), text("b"), int(1)])]);
        assert_eq!(kv_rows(&db).len(), 2);
        // by constraint name
        db.write(
            "INSERT INTO kv (k, v, n) VALUES (99, 'a', 2) ON CONFLICT ON CONSTRAINT kv_v_uq DO UPDATE SET n = EXCLUDED.n",
        );
        assert_eq!(db.rows("SELECT n FROM kv WHERE k = 1"), vec![r(&[int(2)])]);
        db.cleanup();
    }

    #[test]
    fn a_target_that_is_not_a_unique_constraint_is_refused_at_bind_time() {
        let db = seeded("p2a_oc_bad_target");
        for sql in [
            // `n` has no unique index
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (n) DO NOTHING",
            // a superset of a unique constraint is not that constraint
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k, v) DO NOTHING",
            // unknown column / constraint
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (nope) DO NOTHING",
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT ON CONSTRAINT nope DO NOTHING",
            // DO UPDATE needs a target
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT DO UPDATE SET n = 1",
            // the same column twice
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k, k) DO NOTHING",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(
                matches!(
                    err,
                    SqlError::TypeMismatch { .. }
                        | SqlError::UnknownObject { .. }
                        | SqlError::InvalidIdentifier { .. }
                        | SqlError::Parse { .. }
                ),
                "{sql}: {err:?}"
            );
        }
        // nothing was written by any of them
        assert_eq!(kv_rows(&db).len(), 2);
        db.cleanup();
    }

    #[test]
    fn do_update_set_is_validated_at_bind_time() {
        let db = seeded("p2a_oc_bad_set");
        for sql in [
            // PK assignment is refused (v1: PK columns are immutable)
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET k = 5",
            // unknown column
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET zz = 5",
            // an unqualified column is ambiguous between the table and EXCLUDED
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET n = n + 1",
            // wrong type
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET n = 'text'",
            // NOT NULL
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET n = NULL",
            // the same target twice
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET n = 1, n = 2",
            // no subqueries in the conflict action
            "INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET n = (SELECT 1)",
        ] {
            assert!(db.try_bind(sql).is_err(), "{sql} must not bind");
        }
        // the qualified form binds
        db.try_bind("INSERT INTO kv (k) VALUES (1) ON CONFLICT (k) DO UPDATE SET n = kv.n + 1")
            .unwrap();
        db.cleanup();
    }

    #[test]
    fn an_insert_target_alias_is_refused_by_the_parser_dialect() {
        // PostgreSQL's `INSERT INTO t AS alias`: sqlparser enables it for the PostgreSQL
        // dialect only, and this crate parses with `GenericDialect` (D14). The refusal is
        // a clean parse error; the existing row is reachable as `<table>.<column>`.
        let db = seeded("p2a_oc_alias");
        let err = db
            .try_write("INSERT INTO kv AS e (k, v, n) VALUES (1, 'a', 5) ON CONFLICT (k) DO UPDATE SET n = e.n + EXCLUDED.n")
            .unwrap_err();
        assert!(matches!(err, SqlError::Parse { .. }), "{err:?}");
        assert_eq!(db.rows("SELECT n FROM kv WHERE k = 1"), vec![r(&[int(10)])]);
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 5) ON CONFLICT (k) DO UPDATE SET n = kv.n + EXCLUDED.n");
        assert_eq!(db.rows("SELECT n FROM kv WHERE k = 1"), vec![r(&[int(15)])]);
        db.cleanup();
    }

    #[test]
    fn one_statement_cannot_update_the_same_row_twice() {
        let db = seeded("p2a_oc_cardinality");
        let err = db
            .try_write(
                "INSERT INTO kv (k, v, n) VALUES (1, 'a', 1), (1, 'a', 2) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n",
            )
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        // all or nothing
        assert_eq!(db.rows("SELECT n FROM kv WHERE k = 1"), vec![r(&[int(10)])]);
        // DO NOTHING has no such problem: the first wins, the repeat is skipped
        let res = db.write(
            "INSERT INTO kv (k, v, n) VALUES (5, 'e', 1), (5, 'e', 2) ON CONFLICT (k) DO NOTHING RETURNING n",
        );
        assert_eq!(res.rows_affected, 1);
        assert_eq!(returned(&res), &vec![r(&[int(1)])]);
        db.cleanup();
    }

    #[test]
    fn rows_proposed_earlier_in_the_same_statement_are_seen_by_later_ones() {
        let db = kv("p2a_oc_within");
        // a repeated key inside one statement without ON CONFLICT is a duplicate
        let err = db
            .try_write("INSERT INTO kv (k, v) VALUES (1, 'a'), (1, 'b')")
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        assert!(kv_rows(&db).is_empty());
        // ... and a repeat on a unique INDEX column is skipped by DO NOTHING without a target
        let res =
            db.write("INSERT INTO kv (k, v) VALUES (1, 'a'), (2, 'a') ON CONFLICT DO NOTHING");
        assert_eq!(res.rows_affected, 1);
        assert_eq!(kv_rows(&db), vec![r(&[int(1), text("a"), int(7)])]);
        db.cleanup();
    }

    #[test]
    fn insert_select_with_on_conflict() {
        let db = seeded("p2a_oc_select");
        db.write("CREATE TABLE src (k INTEGER PRIMARY KEY, v TEXT, n INTEGER)");
        db.write("INSERT INTO src VALUES (1, 'zz', 1), (3, 'c', 3)");
        let res = db.write(
            "INSERT INTO kv (k, v, n) SELECT k, v, n FROM src \
             ON CONFLICT (k) DO UPDATE SET n = kv.n + EXCLUDED.n RETURNING k, n",
        );
        assert_eq!(res.rows_affected, 2);
        assert_eq!(
            returned(&res),
            &vec![r(&[int(1), int(11)]), r(&[int(3), int(3)])]
        );
        db.cleanup();
    }

    #[test]
    fn upsert_is_idempotent() {
        let db = kv("p2a_oc_idem");
        let sql = "INSERT INTO kv (k, v, n) VALUES (1, 'a', 5), (2, 'b', 6) \
                   ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v, n = EXCLUDED.n";
        db.write(sql);
        let once = kv_rows(&db);
        for _ in 0..3 {
            db.write(sql);
        }
        assert_eq!(kv_rows(&db), once);
        db.cleanup();
    }

    #[test]
    fn an_update_that_changes_nothing_is_still_counted_and_returned() {
        let db = seeded("p2a_oc_noop");
        let res = db.write(
            "INSERT INTO kv (k, v, n) VALUES (1, 'a', 10) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n RETURNING k, n",
        );
        // PostgreSQL counts a DO UPDATE row whether or not a value changed
        assert_eq!(res.rows_affected, 1);
        assert_eq!(returned(&res), &vec![r(&[int(1), int(10)])]);
        db.cleanup();
    }

    #[test]
    fn metrics_count_skipped_conflict_rows() {
        let db = seeded("p2a_oc_metrics");
        let metrics = WriteMetrics::default();
        let plan = db.plan(
            "INSERT INTO kv (k, v) VALUES (1, 'q'), (2, 'r'), (3, 's') ON CONFLICT (k) DO NOTHING",
        );
        execute_write_autocommit(
            &plan,
            &db.txm,
            &db.store,
            &db.catalog,
            &db.builder,
            &[],
            &ExecLimits::default(),
            &metrics,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(metrics.snapshot().upsert_conflict_rows_skipped, 2);
        db.cleanup();
    }
}

// =======================================================================
// Statement atomicity: what a failing statement leaves behind
// =======================================================================

mod statement_atomicity {
    use super::*;

    fn seeded(tag: &str) -> Db {
        let db = kv(tag);
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 10), (2, 'b', 20)");
        db
    }

    fn untouched(db: &Db) {
        assert_eq!(
            kv_rows(db),
            vec![
                r(&[int(1), text("a"), int(10)]),
                r(&[int(2), text("b"), int(20)])
            ]
        );
    }

    #[test]
    fn a_secondary_unique_violation_in_a_multi_row_insert_aborts_the_whole_statement() {
        let db = seeded("p2a_at_unique");
        // row 1 is fine, row 2 reuses v = 'a'
        let err = db
            .try_write("INSERT INTO kv (k, v) VALUES (3, 'c'), (4, 'a') RETURNING k")
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn a_primary_key_duplicate_in_the_middle_of_a_multi_row_insert_aborts_the_whole_statement() {
        let db = seeded("p2a_at_pk");
        let err = db
            .try_write("INSERT INTO kv (k, v) VALUES (3, 'c'), (2, 'dup'), (5, 'e')")
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn a_failure_while_evaluating_do_update_aborts_the_whole_statement() {
        let db = seeded("p2a_at_do_update_fail");
        // k = 3 is a plain insert and k = 1 conflicts and then divides by zero
        let err = db
            .try_write(
                "INSERT INTO kv (k, v, n) VALUES (3, 'c', 1), (1, 'a', 0) \
                 ON CONFLICT (k) DO UPDATE SET n = 10 / EXCLUDED.n RETURNING k",
            )
            .unwrap_err();
        assert!(!matches!(err, SqlError::Conflict { .. }), "{err:?}");
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn a_do_update_that_violates_a_unique_index_aborts_the_whole_statement() {
        let db = seeded("p2a_at_do_update_unique");
        // k = 1 would take v = 'b', which k = 2 holds
        let err = db
            .try_write(
                "INSERT INTO kv (k, v, n) VALUES (3, 'c', 1), (1, 'x', 1) \
                 ON CONFLICT (k) DO UPDATE SET v = 'b'",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn a_do_update_that_nulls_a_not_null_column_aborts_the_whole_statement() {
        let db = seeded("p2a_at_do_update_notnull");
        // the proposed row itself violates NOT NULL (EXCLUDED.n would be NULL); the
        // check fires before any conflict is looked at
        let err = db
            .try_write(
                "INSERT INTO kv (k, v, n) VALUES (3, 'c', 1), (1, 'a', NULL) \
                 ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n",
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                SqlError::TypeMismatch { .. } | SqlError::ExecutionParameter { .. }
            ),
            "{err:?}"
        );
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn a_do_update_expression_that_yields_null_for_a_not_null_column_aborts_the_whole_statement() {
        let db = seeded("p2a_at_do_update_runtime_null");
        let err = db
            .try_write(
                "INSERT INTO kv (k, v, n) VALUES (3, 'c', 1), (1, 'a', 5)                  ON CONFLICT (k) DO UPDATE SET n = CASE WHEN EXCLUDED.n = 5 THEN NULL ELSE EXCLUDED.n END",
            )
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn insert_select_failing_part_way_changes_nothing() {
        let db = seeded("p2a_at_select");
        db.write("CREATE TABLE src (k INTEGER PRIMARY KEY, v TEXT)");
        db.write("INSERT INTO src VALUES (3, 'c'), (4, 'a')");
        let err = db
            .try_write("INSERT INTO kv (k, v) SELECT k, v FROM src")
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn a_failed_statement_inside_a_transaction_leaves_the_earlier_statements_intact_and_itself_nothing(
    ) {
        let db = kv("p2a_at_txn");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO kv (k, v) VALUES (1, 'a')", &mut txn).unwrap();
        // fails on the repeated key after having decided row (2)
        let err = in_txn(
            &db,
            "INSERT INTO kv (k, v) VALUES (2, 'b'), (1, 'dup')",
            &mut txn,
        )
        .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        // and a DO UPDATE that fails after a row was decided
        let err = in_txn(
            &db,
            "INSERT INTO kv (k, v, n) VALUES (3, 'c', 1), (1, 'a', 0) ON CONFLICT (k) DO UPDATE SET n = 1 / EXCLUDED.n",
            &mut txn,
        )
        .unwrap_err();
        assert!(!matches!(err, SqlError::Conflict { .. }), "{err:?}");
        // the transaction is still usable and holds exactly what succeeded
        let seen = in_txn(&db, "UPDATE kv SET n = 1 WHERE k = 1 RETURNING k", &mut txn).unwrap();
        assert_eq!(returned(&seen), &vec![r(&[int(1)])]);
        txn.commit().unwrap();
        assert_eq!(kv_rows(&db), vec![r(&[int(1), text("a"), int(1)])]);
        db.cleanup();
    }

    #[test]
    fn a_unique_violation_between_two_statements_of_a_transaction_fails_the_commit_and_nothing_survives(
    ) {
        let db = seeded("p2a_at_txn_unique");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO kv (k, v) VALUES (3, 'c')", &mut txn).unwrap();
        // the statement cannot see the violation without ON CONFLICT: the unique index is
        // enforced when the transaction commits (D10), as for a plain INSERT
        in_txn(&db, "INSERT INTO kv (k, v) VALUES (4, 'a')", &mut txn).unwrap();
        assert!(txn.commit().is_err());
        untouched(&db);
        db.cleanup();
    }

    #[test]
    fn on_conflict_do_nothing_sees_the_transactions_own_uncommitted_rows() {
        let db = kv("p2a_at_overlay");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "INSERT INTO kv (k, v) VALUES (1, 'a')", &mut txn).unwrap();
        // the row exists only in this transaction's write-set, by PK and by unique index
        let res = in_txn(
            &db,
            "INSERT INTO kv (k, v) VALUES (1, 'x'), (2, 'a'), (3, 'c') ON CONFLICT DO NOTHING RETURNING k",
            &mut txn,
        )
        .unwrap();
        assert_eq!(returned(&res), &vec![r(&[int(3)])]);
        let res = in_txn(
            &db,
            "INSERT INTO kv (k, v, n) VALUES (1, 'x', 5) ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n RETURNING k, v, n",
            &mut txn,
        )
        .unwrap();
        assert_eq!(returned(&res), &vec![r(&[int(1), text("a"), int(5)])]);
        txn.commit().unwrap();
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(1), text("a"), int(5)]),
                r(&[int(3), text("c"), int(7)])
            ]
        );
        db.cleanup();
    }

    #[test]
    fn an_upsert_over_a_row_deleted_in_the_same_transaction_inserts_it_afresh() {
        let db = seeded("p2a_at_deleted");
        let mut txn = db.txm.begin().unwrap();
        in_txn(&db, "DELETE FROM kv WHERE k = 1", &mut txn).unwrap();
        let res = in_txn(
            &db,
            "INSERT INTO kv (k, v, n) VALUES (1, 'a', 99) ON CONFLICT (k) DO UPDATE SET n = 0 RETURNING k, n",
            &mut txn,
        )
        .unwrap();
        // the old row is gone for this transaction, so this is an insert: n is the proposed 99
        assert_eq!(returned(&res), &vec![r(&[int(1), int(99)])]);
        txn.commit().unwrap();
        assert_eq!(db.rows("SELECT n FROM kv WHERE k = 1"), vec![r(&[int(99)])]);
        db.cleanup();
    }
}

// =======================================================================
// Concurrency
// =======================================================================

mod concurrency {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn upsert_limits() -> ExecLimits {
        // a thread may lose the commit race to each of its peers once per round; give it
        // room to win eventually
        ExecLimits {
            max_upsert_attempts: 400,
            ..ExecLimits::default()
        }
    }

    fn run(db: &Db, sql: &str, metrics: &WriteMetrics) -> crate::Result<WriteResult> {
        let plan = db.try_plan(sql)?;
        execute_write_autocommit(
            &plan,
            &db.txm,
            &db.store,
            &db.catalog,
            &db.builder,
            &[],
            &upsert_limits(),
            metrics,
            &CancellationToken::new(),
        )
    }

    #[test]
    fn two_transactions_inserting_the_same_key_leave_exactly_one_row() {
        let db = kv("p2a_cc_two_txn");
        let mut t1 = db.txm.begin().unwrap();
        let mut t2 = db.txm.begin().unwrap();
        in_txn(
            &db,
            "INSERT INTO kv (k, v) VALUES (1, 'one') ON CONFLICT (k) DO NOTHING",
            &mut t1,
        )
        .unwrap();
        in_txn(
            &db,
            "INSERT INTO kv (k, v) VALUES (1, 'two') ON CONFLICT (k) DO NOTHING",
            &mut t2,
        )
        .unwrap();
        t1.commit().unwrap();
        // inside an explicit transaction there is no automatic retry: the loser is told
        assert!(t2.commit().is_err());
        assert_eq!(kv_rows(&db), vec![r(&[int(1), text("one"), int(7)])]);
        db.cleanup();
    }

    #[test]
    fn two_transactions_upserting_the_same_key_lose_no_update_on_retry() {
        let db = kv("p2a_cc_two_txn_retry");
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 0)");
        let mut t1 = db.txm.begin().unwrap();
        let mut t2 = db.txm.begin().unwrap();
        let sql = "INSERT INTO kv (k, v, n) VALUES (1, 'a', 1) ON CONFLICT (k) DO UPDATE SET n = kv.n + EXCLUDED.n";
        in_txn(&db, sql, &mut t1).unwrap();
        in_txn(&db, sql, &mut t2).unwrap();
        t1.commit().unwrap();
        assert!(
            t2.commit().is_err(),
            "the second committer must not overwrite the first with a stale n"
        );
        assert_eq!(db.rows("SELECT n FROM kv"), vec![r(&[int(1)])]);
        // the caller's retry, now seeing the winner's row, applies on top of it
        let mut t3 = db.txm.begin().unwrap();
        in_txn(&db, sql, &mut t3).unwrap();
        t3.commit().unwrap();
        assert_eq!(db.rows("SELECT n FROM kv"), vec![r(&[int(2)])]);
        db.cleanup();
    }

    #[test]
    fn concurrent_autocommit_upserts_of_one_new_key_make_one_row_and_lose_no_update() {
        const THREADS: usize = 4;
        const PER_THREAD: i32 = 10;
        let db = Arc::new(kv("p2a_cc_counter"));
        let barrier = Arc::new(Barrier::new(THREADS));
        let metrics = Arc::new(WriteMetrics::default());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let (db, barrier, metrics) =
                    (Arc::clone(&db), Arc::clone(&barrier), Arc::clone(&metrics));
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..PER_THREAD {
                        run(
                            &db,
                            "INSERT INTO kv (k, v, n) VALUES (1, 'a', 1) \
                             ON CONFLICT (k) DO UPDATE SET n = kv.n + EXCLUDED.n",
                            &metrics,
                        )
                        .expect("an upsert must not fail because of a race");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // exactly one row; every one of the 40 increments counted (the first creates the row
        // with n = 1, the other 39 add one each)
        assert_eq!(
            kv_rows(&db),
            vec![r(&[int(1), text("a"), int(THREADS as i32 * PER_THREAD)])]
        );
        let db = Arc::try_unwrap(db).ok().expect("threads joined");
        db.cleanup();
    }

    #[test]
    fn concurrent_do_nothing_inserts_of_one_key_insert_it_exactly_once() {
        const THREADS: usize = 8;
        let db = Arc::new(kv("p2a_cc_nothing"));
        let barrier = Arc::new(Barrier::new(THREADS));
        let inserted: Vec<_> = (0..THREADS)
            .map(|i| {
                let (db, barrier) = (Arc::clone(&db), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    run(
                        &db,
                        &format!("INSERT INTO kv (k, v) VALUES (1, 'w{i}') ON CONFLICT (k) DO NOTHING RETURNING k"),
                        &WriteMetrics::default(),
                    )
                    .unwrap()
                    .rows_affected
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        // one writer inserted; every other one saw it and skipped
        assert_eq!(inserted.iter().sum::<u64>(), 1, "{inserted:?}");
        assert_eq!(db.rows("SELECT COUNT(*) FROM kv"), vec![r(&[big(1)])]);
        let db = Arc::try_unwrap(db).ok().expect("threads joined");
        db.cleanup();
    }

    #[test]
    fn concurrent_upserts_on_a_unique_index_key_make_one_row() {
        const THREADS: usize = 4;
        let db = Arc::new(kv("p2a_cc_index"));
        let barrier = Arc::new(Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let (db, barrier) = (Arc::clone(&db), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    // different primary keys, the same unique value: the unique index decides
                    run(
                        &db,
                        &format!(
                            "INSERT INTO kv (k, v, n) VALUES ({}, 'same', 1) \
                             ON CONFLICT (v) DO UPDATE SET n = kv.n + 1",
                            100 + i
                        ),
                        &WriteMetrics::default(),
                    )
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let rows = db.rows("SELECT v, n FROM kv");
        assert_eq!(rows, vec![r(&[text("same"), int(THREADS as i32)])]);
        let db = Arc::try_unwrap(db).ok().expect("threads joined");
        db.cleanup();
    }

    #[test]
    fn a_retry_is_bounded_and_reports_the_conflict_when_it_cannot_win() {
        let db = kv("p2a_cc_bounded");
        db.write("INSERT INTO kv (k, v) VALUES (1, 'a'), (2, 'b')");
        // a DO UPDATE that can never commit (it takes the value another row holds)
        let metrics = WriteMetrics::default();
        let plan =
            db.plan("INSERT INTO kv (k, v) VALUES (1, 'x') ON CONFLICT (k) DO UPDATE SET v = 'b'");
        let limits = ExecLimits {
            max_upsert_attempts: 3,
            ..ExecLimits::default()
        };
        let err = execute_write_autocommit(
            &plan,
            &db.txm,
            &db.store,
            &db.catalog,
            &db.builder,
            &[],
            &limits,
            &metrics,
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        assert_eq!(metrics.snapshot().upsert_retries, 2);
        assert_eq!(
            db.rows("SELECT v FROM kv WHERE k = 1"),
            vec![r(&[text("a")])]
        );
        db.cleanup();
    }
}

// =======================================================================
// Restart
// =======================================================================

mod restart {
    use super::*;

    #[test]
    fn returned_rows_are_durable_and_the_upsert_is_idempotent_across_a_restart() {
        let db = kv("p2a_restart");
        let sql = "INSERT INTO kv (k, v, n) VALUES (1, 'a', 5), (2, 'b', 6) \
                   ON CONFLICT (k) DO UPDATE SET n = EXCLUDED.n RETURNING k, v, n";
        let first = db.write(sql);
        let promised = returned(&first).clone();
        let upd = db.write("UPDATE kv SET n = n + 100 WHERE k = 2 RETURNING k, n");
        assert_eq!(returned(&upd), &vec![r(&[int(2), int(106)])]);
        let del = db.write("DELETE FROM kv WHERE k = 1 RETURNING k, v, n");
        assert_eq!(returned(&del), &vec![r(&[int(1), text("a"), int(5)])]);

        let db = reopen(db);
        // what RETURNING reported as committed is what survived, no more and no less
        assert_eq!(kv_rows(&db), vec![r(&[int(2), text("b"), int(106)])]);
        assert_eq!(promised.len(), 2);

        // the same upsert after the restart behaves like the one before it
        let again = db.write(sql);
        assert_eq!(again.rows_affected, 2);
        assert_eq!(returned(&again), &promised);
        assert_eq!(
            kv_rows(&db),
            vec![
                r(&[int(1), text("a"), int(5)]),
                r(&[int(2), text("b"), int(6)])
            ]
        );
        let db = reopen(db);
        let third = db.write(sql);
        assert_eq!(returned(&third), &promised);
        assert_eq!(kv_rows(&db).len(), 2);
        db.cleanup();
    }

    #[test]
    fn the_unique_index_still_arbitrates_after_a_restart() {
        let db = kv("p2a_restart_index");
        db.write("INSERT INTO kv (k, v, n) VALUES (1, 'a', 1)");
        let db = reopen(db);
        let res = db.write("INSERT INTO kv (k, v, n) VALUES (9, 'a', 2) ON CONFLICT (v) DO UPDATE SET n = EXCLUDED.n RETURNING k, n");
        assert_eq!(returned(&res), &vec![r(&[int(1), int(2)])]);
        let err = db
            .try_write("INSERT INTO kv (k, v) VALUES (9, 'a')")
            .unwrap_err();
        assert!(matches!(err, SqlError::Conflict { .. }), "{err:?}");
        assert_eq!(kv_rows(&db), vec![r(&[int(1), text("a"), int(2)])]);
        db.cleanup();
    }

    #[test]
    fn an_aborted_statement_leaves_nothing_behind_after_a_restart() {
        let db = kv("p2a_restart_abort");
        db.write("INSERT INTO kv (k, v) VALUES (1, 'a')");
        db.try_write("INSERT INTO kv (k, v) VALUES (2, 'b'), (1, 'dup') RETURNING k")
            .unwrap_err();
        db.try_write("INSERT INTO kv (k, v) VALUES (3, 'c'), (4, 'a')")
            .unwrap_err();
        let db = reopen(db);
        assert_eq!(kv_rows(&db), vec![r(&[int(1), text("a"), int(7)])]);
        db.cleanup();
    }
}

// =======================================================================
// Authorization
// =======================================================================

mod authorization {
    use super::*;
    use rubixdb::catalog::schema::{ObjectKind, Privilege};

    fn grant(db: &Db, who: &str, privilege: Privilege) {
        let table = db.table_id("kv");
        db.f.catalog
            .grant(who, ObjectKind::Table, table, privilege)
            .unwrap();
    }

    fn bind_as(db: &Db, who: &str, sql: &str) -> crate::Result<crate::bound::BoundStatement> {
        db.try_bind_with(
            sql,
            &crate::auth::AuthContext::none(who),
            &crate::SqlLimits::default(),
        )
    }

    #[test]
    fn each_clause_needs_the_privilege_it_actually_exercises() {
        let db = kv("p2a_auth");
        let plain = "INSERT INTO kv (k, v) VALUES (1, 'a')";
        let returning = "INSERT INTO kv (k, v) VALUES (1, 'a') RETURNING k";
        let conflict = "INSERT INTO kv (k, v) VALUES (1, 'a') ON CONFLICT (k) DO NOTHING";
        let upsert = "INSERT INTO kv (k, v) VALUES (1, 'a') ON CONFLICT (k) DO UPDATE SET n = 1";
        let update_ret = "UPDATE kv SET n = 1 RETURNING n";
        let delete_ret = "DELETE FROM kv RETURNING k";

        // nothing granted: not even visible
        for sql in [plain, returning, conflict, upsert, update_ret, delete_ret] {
            assert!(
                matches!(bind_as(&db, "p", sql), Err(SqlError::UnknownObject { .. })),
                "{sql}"
            );
        }

        grant(&db, "p", Privilege::Insert);
        assert!(bind_as(&db, "p", plain).is_ok());
        // reading rows back or arbitrating on existing rows is a SELECT
        assert!(bind_as(&db, "p", returning).is_err());
        assert!(bind_as(&db, "p", conflict).is_err());
        assert!(bind_as(&db, "p", upsert).is_err());

        grant(&db, "p", Privilege::Select);
        assert!(bind_as(&db, "p", returning).is_ok());
        assert!(bind_as(&db, "p", conflict).is_ok());
        // DO UPDATE is an UPDATE
        assert!(bind_as(&db, "p", upsert).is_err());

        grant(&db, "p", Privilege::Update);
        assert!(bind_as(&db, "p", upsert).is_ok());
        assert!(bind_as(&db, "p", update_ret).is_ok());
        // DELETE ... RETURNING still needs DELETE
        assert!(bind_as(&db, "p", delete_ret).is_err());
        grant(&db, "p", Privilege::Delete);
        assert!(bind_as(&db, "p", delete_ret).is_ok());
        db.cleanup();
    }

    #[test]
    fn update_and_delete_returning_need_select_as_well() {
        let db = kv("p2a_auth_upd_del");
        grant(&db, "q", Privilege::Update);
        grant(&db, "q", Privilege::Delete);
        assert!(bind_as(&db, "q", "UPDATE kv SET n = 1").is_ok());
        assert!(bind_as(&db, "q", "DELETE FROM kv").is_ok());
        assert!(bind_as(&db, "q", "UPDATE kv SET n = 1 RETURNING n").is_err());
        assert!(bind_as(&db, "q", "DELETE FROM kv RETURNING k").is_err());
        grant(&db, "q", Privilege::Select);
        assert!(bind_as(&db, "q", "UPDATE kv SET n = 1 RETURNING n").is_ok());
        assert!(bind_as(&db, "q", "DELETE FROM kv RETURNING k").is_ok());
        db.cleanup();
    }

    #[test]
    fn insert_select_needs_select_on_its_source() {
        let db = kv("p2a_auth_select");
        db.write("CREATE TABLE secret (k INTEGER PRIMARY KEY, v TEXT)");
        grant(&db, "p", Privilege::Insert);
        let err = bind_as(&db, "p", "INSERT INTO kv (k, v) SELECT k, v FROM secret").unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// Parser
// =======================================================================

mod parser {
    use super::*;
    use crate::ast::{ConflictAction, ConflictTarget, Statement};
    use crate::parse::parse_statement;

    fn parse(sql: &str) -> crate::Result<Statement> {
        parse_statement(sql, &crate::SqlLimits::default())
    }

    fn insert(sql: &str) -> crate::ast::Insert {
        match parse(sql).unwrap() {
            Statement::Insert(i) => i,
            other => panic!("not an INSERT: {other:?}"),
        }
    }

    #[test]
    fn on_conflict_forms_parse_to_the_ast() {
        let i = insert("INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING");
        let oc = i.on_conflict.unwrap();
        assert!(oc.target.is_none());
        assert_eq!(oc.action, ConflictAction::DoNothing);

        let i = insert("INSERT INTO t (a) VALUES (1) ON CONFLICT (a, b) DO NOTHING");
        match i.on_conflict.unwrap().target.unwrap() {
            ConflictTarget::Columns(cols) => assert_eq!(cols.len(), 2),
            other => panic!("{other:?}"),
        }

        let i = insert("INSERT INTO t (a) VALUES (1) ON CONFLICT ON CONSTRAINT c DO NOTHING");
        assert!(matches!(
            i.on_conflict.unwrap().target,
            Some(ConflictTarget::Constraint(_))
        ));

        let i = insert(
            "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b, c = 2 WHERE t.b < 5 RETURNING a",
        );
        assert!(i.returning.is_some());
        match i.on_conflict.unwrap().action {
            ConflictAction::DoUpdate {
                assignments,
                selection,
            } => {
                assert_eq!(assignments.len(), 2);
                assert!(selection.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn returning_forms_parse_on_all_three_statements() {
        assert!(insert("INSERT INTO t (a) VALUES (1) RETURNING *")
            .returning
            .is_some());
        match parse("UPDATE t SET a = 1 WHERE b = 2 RETURNING a, b + 1 AS c").unwrap() {
            Statement::Update(u) => assert_eq!(u.returning.unwrap().len(), 2),
            other => panic!("{other:?}"),
        }
        match parse("DELETE FROM t WHERE b = 2 RETURNING t.*").unwrap() {
            Statement::Delete(d) => assert!(d.returning.is_some()),
            other => panic!("{other:?}"),
        }
        // not asked for: absent
        assert!(insert("INSERT INTO t (a) VALUES (1)").returning.is_none());
    }

    #[test]
    fn insert_select_forms_parse() {
        let i = insert("INSERT INTO t (a, b) SELECT x, y FROM s WHERE x > 1");
        assert!(i.query.is_some());
        assert!(i.rows.is_empty());
        let i = insert("INSERT INTO t SELECT * FROM s UNION ALL SELECT * FROM u");
        assert!(i.query.is_some());
        let i = insert("INSERT INTO t (a) VALUES (1), (2)");
        assert!(i.query.is_none());
        assert_eq!(i.rows.len(), 2);
    }

    #[test]
    fn what_stays_unsupported_is_refused_with_a_parse_error() {
        for sql in [
            // MySQL / MSSQL spellings
            "INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE a = 1",
            "INSERT INTO t (a) OUTPUT inserted.a VALUES (1)",
            // a second RETURNING-bearing clause the dialect does not have
            "UPDATE t SET a = 1 FROM s RETURNING a",
            // malformed
            "INSERT INTO t (a) VALUES (1) ON CONFLICT",
            "INSERT INTO t (a) VALUES (1) ON CONFLICT (a)",
            "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE",
            "INSERT INTO t (a) VALUES (1) RETURNING",
        ] {
            let err = parse(sql).unwrap_err();
            assert!(
                matches!(err, SqlError::Parse { .. } | SqlError::Unsupported { .. }),
                "{sql}: {err:?}"
            );
        }
    }
}
