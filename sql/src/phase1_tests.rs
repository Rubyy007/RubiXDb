//! SQL Phase 1 -- the SQL correctness gaps `docs/SNOWFLAKE_SQL_COMPATIBILITY.md`
//! classified `PARTIALLY_SUPPORTED` / `NOT_IMPLEMENTED`:
//!
//! * `ORDER BY` select-list aliases and positions (L-11, D-7), `MINUS` and
//!   `UNION [ALL] BY NAME` (D-9, D-10);
//! * `COUNT(a, b)` / `COUNT(DISTINCT a, b)`, several `DISTINCT` aggregates over
//!   different columns, `DISTINCT` window aggregates (A-4, A-5, E-10), and the
//!   aggregate overflow paths (A-8);
//! * `GROUP BY ALL` (A-9);
//! * `x <op> ANY | SOME | ALL (subquery)` (B-6);
//! * window `GROUPS` frames and `IGNORE NULLS` (E-9), checked against an
//!   independent oracle written from the definitions.
//!
//! Every execution test runs through the real parse, bind, plan and execute
//! pipeline against a real engine (`mission1_support::Db`).

use crate::mission1_support::*;

use rubixdb::relational::RelationalValue;

use crate::aggregation_tests::{ag_db, seed_ag};
use crate::error::SqlError;
use crate::exec::{ExecLimits, ExecMetrics};
use crate::limits::SqlLimits;
use crate::parse::parse_statement;
use crate::setop_tests::{bind_err, bound};
use crate::window_tests::{col, seed_w, win_db};

fn ids(rows: &[Row]) -> Vec<i32> {
    rows.iter()
        .map(|r| match &r[0] {
            Some(RelationalValue::Integer(n)) => *n,
            other => panic!("expected an INTEGER id, got {other:?}"),
        })
        .collect()
}

fn r(vals: &[V]) -> Row {
    vals.to_vec()
}

fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

/// `w` with five rows:
///
/// | id | g | o | x |
/// |---|---|---|---|
/// | 1 | 1 | 3 | 30 |
/// | 2 | 1 | 1 | 10 |
/// | 3 | 2 | 2 | 20 |
/// | 4 | 2 | NULL | NULL |
/// | 5 | NULL | 1 | 50 |
fn five(tag: &str) -> Db {
    let db = win_db(tag);
    seed_w(
        &db,
        &[
            (1, Some(1), Some(3), Some(30)),
            (2, Some(1), Some(1), Some(10)),
            (3, Some(2), Some(2), Some(20)),
            (4, Some(2), None, None),
            (5, None, Some(1), Some(50)),
        ],
    );
    db
}

// =======================================================================
// ORDER BY: select-list aliases and positions
// =======================================================================

mod order_by {
    use super::*;

    #[test]
    fn a_position_names_the_select_list_item() {
        let db = five("p1_ob_pos");
        // x ascending, NULLs last (the default).
        assert_eq!(
            ids(&db.rows("SELECT id, x FROM w ORDER BY 2")),
            vec![2, 3, 1, 5, 4]
        );
        // descending: NULLs first.
        assert_eq!(
            ids(&db.rows("SELECT id, x FROM w ORDER BY 2 DESC")),
            vec![4, 5, 1, 3, 2]
        );
        // `*` expands first, so 3 is `o`; ties keep input order.
        assert_eq!(
            ids(&db.rows("SELECT * FROM w ORDER BY 3")),
            vec![2, 5, 3, 1, 4]
        );
        // several keys, positions and columns mixed.
        assert_eq!(
            ids(&db.rows("SELECT id, g FROM w ORDER BY 2 DESC, 1")),
            vec![5, 3, 4, 1, 2]
        );
        assert_eq!(
            ids(&db.rows("SELECT id, g FROM w ORDER BY 2 DESC, id DESC")),
            vec![5, 4, 3, 2, 1]
        );
        db.cleanup();
    }

    #[test]
    fn a_position_outside_the_select_list_is_a_bind_error_never_a_constant_key() {
        let db = five("p1_ob_range");
        for sql in [
            "SELECT id, x FROM w ORDER BY 3",
            "SELECT id, x FROM w ORDER BY 0",
            "SELECT id, x FROM w ORDER BY -1",
            "SELECT id FROM w ORDER BY 99999999999999999999",
        ] {
            let err = bind_err(&db, sql);
            assert!(
                matches!(err, SqlError::InvalidIdentifier { .. }),
                "{sql}: {err:?}"
            );
        }
        db.cleanup();
    }

    #[test]
    fn other_constants_stay_constant_sort_keys() {
        let db = five("p1_ob_const");
        // Not an unsigned integer literal: an ordinary (constant) expression, so
        // the input order is kept.
        for sql in [
            "SELECT id FROM w ORDER BY 1 + 1",
            "SELECT id FROM w ORDER BY 1.5",
            "SELECT id FROM w ORDER BY 'a'",
        ] {
            assert_eq!(ids(&db.rows(sql)), vec![1, 2, 3, 4, 5], "{sql}");
        }
        // A parameter is a value, not a position.
        let res = db
            .try_run_with(
                "SELECT id FROM w ORDER BY $1",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[int(2)],
            )
            .unwrap();
        assert_eq!(ids(&res.rows), vec![1, 2, 3, 4, 5]);
        db.cleanup();
    }

    #[test]
    fn an_alias_names_the_select_list_item() {
        let db = five("p1_ob_alias");
        assert_eq!(
            ids(&db.rows("SELECT id, x AS val FROM w ORDER BY val")),
            vec![2, 3, 1, 5, 4]
        );
        assert_eq!(
            ids(&db.rows("SELECT id, x AS val FROM w ORDER BY val DESC")),
            vec![4, 5, 1, 3, 2]
        );
        // an alias inside a larger expression
        assert_eq!(
            ids(&db.rows("SELECT id, x AS val FROM w ORDER BY val + 0 DESC")),
            vec![4, 5, 1, 3, 2]
        );
        db.cleanup();
    }

    #[test]
    fn an_alias_wins_over_a_column_of_the_same_name_in_a_bare_order_by() {
        // The standard's and PostgreSQL's rule: an output name beats an input
        // column in a bare `ORDER BY name`. `-x AS x` sorts by -x (ascending:
        // -50, -30, -20, -10, NULL), not by the column x.
        let db = five("p1_ob_shadow");
        assert_eq!(
            ids(&db.rows("SELECT id, -x AS x FROM w ORDER BY x")),
            vec![5, 1, 3, 2, 4]
        );
        // Qualified, the column is meant.
        assert_eq!(
            ids(&db.rows("SELECT id, -x AS x FROM w ORDER BY w.x")),
            vec![2, 3, 1, 5, 4]
        );
        // Inside an expression the FROM column is seen first.
        assert_eq!(
            ids(&db.rows("SELECT id, -x AS x FROM w ORDER BY x + 0")),
            vec![2, 3, 1, 5, 4]
        );
        db.cleanup();
    }

    #[test]
    fn an_alias_given_twice_is_ambiguous() {
        let db = five("p1_ob_ambiguous");
        let err = bind_err(&db, "SELECT x AS a, o AS a FROM w ORDER BY a");
        assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{err:?}");
        // The same name unaliased is not an alias: the column is meant.
        db.try_bind("SELECT x AS a, o AS b FROM w ORDER BY a")
            .unwrap();
        db.cleanup();
    }

    #[test]
    fn an_unknown_name_is_still_an_error() {
        let db = five("p1_ob_unknown");
        let err = bind_err(&db, "SELECT id FROM w ORDER BY nosuch");
        assert!(matches!(err, SqlError::UnknownObject { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn aliases_and_positions_of_aggregates_and_windows() {
        let db = five("p1_ob_agg");
        // SUM(x) per g: g=1 -> 40, g=2 -> 20 (NULL ignored), g=NULL -> 50.
        let rows = db.rows("SELECT g, SUM(x) AS s FROM w GROUP BY g ORDER BY s DESC");
        assert_eq!(
            rows,
            vec![
                r(&[NULL, big(50)]),
                r(&[int(1), big(40)]),
                r(&[int(2), big(20)])
            ]
        );
        let rows = db.rows("SELECT g, SUM(x) FROM w GROUP BY g ORDER BY 2");
        assert_eq!(col(&rows, 1), vec![big(20), big(40), big(50)]);
        // a window function by alias
        let rows =
            db.rows("SELECT id, ROW_NUMBER() OVER (ORDER BY x) AS rn FROM w ORDER BY rn DESC");
        assert_eq!(ids(&rows), vec![4, 5, 1, 3, 2]);
        // a non-grouped column through an alias is still refused
        let err = bind_err(&db, "SELECT g, x AS v FROM w GROUP BY g ORDER BY v");
        assert!(
            matches!(
                err,
                SqlError::TypeMismatch { .. } | SqlError::InvalidIdentifier { .. }
            ),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn with_distinct_limit_offset_and_in_a_subquery() {
        let db = five("p1_ob_misc");
        // g: 1,1,2,2,NULL -> distinct, descending, NULLs first.
        let rows = db.rows("SELECT DISTINCT g FROM w ORDER BY 1 DESC");
        assert_eq!(col(&rows, 0), vec![NULL, int(2), int(1)]);
        let rows = db.rows("SELECT id, x FROM w ORDER BY 2 LIMIT 2 OFFSET 1");
        assert_eq!(ids(&rows), vec![3, 1]);
        let rows =
            db.rows("SELECT id FROM w WHERE x IN (SELECT x FROM w ORDER BY 1 LIMIT 2) ORDER BY id");
        assert_eq!(ids(&rows), vec![2, 3]);
        db.cleanup();
    }

    #[test]
    fn set_operation_positions_and_aliases() {
        let db = five("p1_ob_setop");
        // names come from the first operand
        let rows = db.rows(
            "SELECT id, x AS val FROM w UNION ALL SELECT id + 10, x FROM w ORDER BY 2 DESC, 1",
        );
        assert_eq!(ids(&rows), vec![4, 14, 5, 15, 1, 11, 3, 13, 2, 12]);
        let rows = db.rows("SELECT id, x AS val FROM w UNION ALL SELECT id + 10, x FROM w ORDER BY val, id LIMIT 3 OFFSET 1");
        assert_eq!(ids(&rows), vec![12, 3, 13]);
        for sql in [
            "SELECT id, x FROM w UNION SELECT id, x FROM w ORDER BY 3",
            "SELECT id, x FROM w UNION SELECT id, x FROM w ORDER BY 0",
        ] {
            let err = bind_err(&db, sql);
            assert!(
                matches!(err, SqlError::InvalidIdentifier { .. }),
                "{sql}: {err:?}"
            );
        }
        db.cleanup();
    }
}

// =======================================================================
// Set operations: MINUS, UNION BY NAME
// =======================================================================

mod set_ops {
    use super::*;

    fn two_tables(tag: &str) -> Db {
        let db = five(tag);
        db.write("CREATE TABLE a (p INTEGER PRIMARY KEY, q TEXT, r INTEGER)");
        db.write("CREATE TABLE b (r BIGINT PRIMARY KEY, p INTEGER, s TEXT)");
        db.write("INSERT INTO a VALUES (1, 'x', 10), (2, 'y', 20)");
        db.write("INSERT INTO b VALUES (10, 1, 's1'), (30, 3, 's3')");
        db
    }

    #[test]
    fn minus_is_except() {
        let db = five("p1_minus");
        // g: left {1,1,2,2,NULL}; right (ids 4, 5) {2, NULL}
        let minus = db.rows_sorted("SELECT g FROM w MINUS SELECT g FROM w WHERE id > 3");
        let except = db.rows_sorted("SELECT g FROM w EXCEPT SELECT g FROM w WHERE id > 3");
        assert_eq!(minus, except);
        assert_eq!(minus, vec![r(&[int(1)])]);
        // the bag form: {1,1,2,2,NULL} - {2,NULL} = {1,1,2}
        let all = db.rows_sorted("SELECT g FROM w MINUS ALL SELECT g FROM w WHERE id > 3");
        assert_eq!(all, vec![r(&[int(1)]), r(&[int(1)]), r(&[int(2)])]);
        // precedence: MINUS is left-associative with UNION, INTERSECT binds tighter.
        let rows = db.rows_sorted(
            "SELECT g FROM w WHERE id = 1 UNION SELECT g FROM w WHERE id = 3 MINUS SELECT g FROM w WHERE id = 1",
        );
        assert_eq!(rows, vec![r(&[int(2)])]);
        db.cleanup();
    }

    #[test]
    fn union_by_name_matches_columns_by_name_and_fills_nulls() {
        let db = two_tables("p1_byname");
        // a: (p, q, r) ; b: (r, p, s). Result columns: p, q, r, s.
        let rows = db.rows_sorted("SELECT p, q, r FROM a UNION ALL BY NAME SELECT r, p, s FROM b");
        // Note the right operand's select list is (r, p, s): its r is b.r, p is b.p.
        assert_eq!(
            rows,
            sorted(vec![
                r(&[int(1), text("x"), big(10), NULL]),
                r(&[int(2), text("y"), big(20), NULL]),
                r(&[int(1), NULL, big(10), text("s1")]),
                r(&[int(3), NULL, big(30), text("s3")]),
            ])
        );
        let b = bound(
            &db,
            "SELECT p, q, r FROM a UNION BY NAME SELECT r, p, s FROM b",
        );
        let names: Vec<_> = b.projection.iter().map(|i| i.output_name.clone()).collect();
        assert_eq!(names, vec!["p", "q", "r", "s"]);
        // r is INTEGER on the left, BIGINT on the right -> BIGINT; q and s are
        // nullable because one operand lacks them.
        let types: Vec<_> = b.projection.iter().map(|i| i.expr.ty).collect();
        assert_eq!(types[2], Some(rubixdb::relational::RelationalType::Bigint));
        assert!(b.projection[1].expr.nullable && b.projection[3].expr.nullable);
        db.cleanup();
    }

    #[test]
    fn union_by_name_deduplicates_and_union_all_by_name_keeps_duplicates() {
        let db = two_tables("p1_byname_dup");
        // (p, r): left (1,10) (2,20); right (p=1,r=10) (p=3,r=30)
        let distinct = db.rows_sorted("SELECT p, r FROM a UNION BY NAME SELECT r, p FROM b");
        assert_eq!(
            distinct,
            sorted(vec![
                r(&[int(1), big(10)]),
                r(&[int(2), big(20)]),
                r(&[int(3), big(30)])
            ])
        );
        let all = db.rows_sorted("SELECT p, r FROM a UNION ALL BY NAME SELECT r, p FROM b");
        assert_eq!(all.len(), 4);
        // The position-based UNION of the same lists would pair a.p with b.r.
        let positional = db.rows_sorted("SELECT p, r FROM a UNION SELECT r, p FROM b");
        assert_ne!(positional, distinct);
        db.cleanup();
    }

    #[test]
    fn union_by_name_with_nulls_and_empty_operands() {
        let db = two_tables("p1_byname_null");
        db.write("INSERT INTO a VALUES (3, NULL, NULL)");
        // NULLs compare as equal for deduplication, a filler NULL included.
        let rows = db.rows_sorted(
            "SELECT p, q FROM a WHERE p = 3 UNION BY NAME SELECT p, q FROM a WHERE p = 3",
        );
        assert_eq!(rows, vec![r(&[int(3), NULL])]);
        // empty right operand
        let rows = db.rows_sorted(
            "SELECT p, q FROM a WHERE p = 1 UNION BY NAME SELECT s, p FROM b WHERE p = 99",
        );
        assert_eq!(rows, vec![r(&[int(1), text("x"), NULL])]);
        // empty left operand
        let rows = db.rows_sorted(
            "SELECT p, q FROM a WHERE p = 99 UNION BY NAME SELECT s, p FROM b WHERE p = 1",
        );
        assert_eq!(rows, vec![r(&[int(1), NULL, text("s1")])]);
        db.cleanup();
    }

    #[test]
    fn union_by_name_chains_orders_and_limits() {
        let db = two_tables("p1_byname_chain");
        // a (p,q) UNION b (s, p) UNION a again: the second step adds nothing new
        // and the column list stays (p, q, s).
        let rows = db.rows(
            "SELECT p, q FROM a UNION ALL BY NAME SELECT s, p FROM b UNION ALL BY NAME SELECT q, p FROM a \
             ORDER BY 1, 2, 3 LIMIT 3 OFFSET 2",
        );
        // 2 (a) + 2 (b) + 2 (a) = 6 rows; sorted by p, q, s (NULLs last):
        // (1,'x',NULL) (1,'x',NULL) (1,NULL,'s1') (2,'y',NULL) (2,'y',NULL) (3,NULL,'s3')
        assert_eq!(
            rows,
            vec![
                r(&[int(1), NULL, text("s1")]),
                r(&[int(2), text("y"), NULL]),
                r(&[int(2), text("y"), NULL]),
            ]
        );
        // a set operation as the left operand of BY NAME
        let rows = db.rows_sorted(
            "(SELECT p, q FROM a UNION ALL SELECT p, q FROM a) UNION ALL BY NAME SELECT p, s FROM b",
        );
        assert_eq!(rows.len(), 6);
        assert!(rows.contains(&r(&[int(3), NULL, text("s3")])));
        db.cleanup();
    }

    #[test]
    fn union_by_name_folds_unquoted_names_and_keeps_quoted_ones_exact() {
        let db = two_tables("p1_byname_case");
        // unquoted names fold to lower case: X and x are the same column
        let b = bound(
            &db,
            "SELECT p AS X FROM a UNION BY NAME SELECT r AS x FROM b",
        );
        assert_eq!(b.projection.len(), 1);
        // a quoted name is exact: "X" and x are two columns
        let b = bound(
            &db,
            "SELECT p, q AS \"X\" FROM a UNION BY NAME SELECT p, r AS x FROM b",
        );
        let names: Vec<_> = b
            .projection
            .iter()
            .map(|i| i.output_name.as_str())
            .collect();
        assert_eq!(names, vec!["p", "X", "x"]);
        db.cleanup();
    }

    #[test]
    fn union_by_name_errors() {
        let db = two_tables("p1_byname_err");
        let cases = [
            // no name in common
            (
                "SELECT p FROM a UNION BY NAME SELECT s FROM b",
                "no column name in common",
            ),
            // a name twice in one operand
            (
                "SELECT p, p FROM a UNION BY NAME SELECT p FROM b",
                "more than one column named",
            ),
            (
                "SELECT p FROM a UNION BY NAME SELECT p, p FROM b",
                "more than one column named",
            ),
            // two unaliased expressions are both named ?column?
            (
                "SELECT p + 1, p + 2 FROM a UNION BY NAME SELECT p FROM b",
                "more than one column named",
            ),
            // a type that cannot unify
            (
                "SELECT p FROM a UNION BY NAME SELECT s AS p FROM b",
                "cannot combine",
            ),
        ];
        for (sql, needle) in cases {
            let err = bind_err(&db, sql);
            assert!(err.to_string().contains(needle), "{sql}: {err}");
        }
        // BY NAME has no INTERSECT / EXCEPT form
        for sql in [
            "SELECT p FROM a INTERSECT BY NAME SELECT p FROM b",
            "SELECT p FROM a EXCEPT BY NAME SELECT p FROM b",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(err, SqlError::Unsupported { .. }),
                "{sql}: {err:?}"
            );
        }
        // and none in the body of a recursive CTE
        let err = db
            .try_bind("WITH RECURSIVE c(p) AS (SELECT 1 UNION BY NAME SELECT p + 1 FROM c) SELECT p FROM c")
            .unwrap_err();
        assert!(
            matches!(
                err,
                SqlError::Unsupported { .. }
                    | SqlError::InvalidIdentifier { .. }
                    | SqlError::TypeMismatch { .. }
            ),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn set_operation_basics_still_hold() {
        // column count, NULLs, duplicates, precedence, LIMIT/OFFSET once more through
        // the positional path, so BY NAME did not disturb it.
        let db = five("p1_setop_base");
        let err = bind_err(&db, "SELECT id, g FROM w UNION SELECT id FROM w");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let rows = db.rows_sorted("SELECT g FROM w INTERSECT ALL SELECT g FROM w WHERE id < 4");
        assert_eq!(rows, vec![r(&[int(1)]), r(&[int(1)]), r(&[int(2)])]);
        let rows = db.rows("SELECT g FROM w UNION SELECT g FROM w ORDER BY 1 LIMIT 2 OFFSET 1");
        assert_eq!(col(&rows, 0), vec![int(2), NULL]);
        db.cleanup();
    }
}

// =======================================================================
// COUNT(a, b), COUNT(DISTINCT a, b), several DISTINCT aggregates
// =======================================================================

mod count_distinct {
    use super::*;

    /// `ag(id, a, b, x, y, d)` with (a, b):
    /// (1,1) (1,1) (1,2) (2,NULL) (NULL,3) (NULL,NULL) (2,2)
    fn pairs(tag: &str) -> Db {
        let db = ag_db(tag);
        seed_ag(
            &db,
            &[
                (1, Some(1), Some(1), None, None),
                (2, Some(1), Some(1), None, None),
                (3, Some(1), Some(2), None, None),
                (4, Some(2), None, None, None),
                (5, None, Some(3), None, None),
                (6, None, None, None, None),
                (7, Some(2), Some(2), None, None),
            ],
        );
        db
    }

    fn one(db: &Db, sql: &str) -> Row {
        let rows = db.rows(sql);
        assert_eq!(rows.len(), 1, "{sql}");
        rows.into_iter().next().unwrap()
    }

    #[test]
    fn count_of_several_expressions_counts_the_rows_with_no_null() {
        let db = pairs("p1_cm_plain");
        // both non-NULL: (1,1) (1,1) (1,2) (2,2)
        assert_eq!(one(&db, "SELECT COUNT(a, b) FROM ag"), r(&[big(4)]));
        // and the single-expression forms next to it
        assert_eq!(
            one(&db, "SELECT COUNT(*), COUNT(a), COUNT(b) FROM ag"),
            r(&[big(7), big(5), big(5)])
        );
        db.cleanup();
    }

    #[test]
    fn count_distinct_of_several_expressions_counts_distinct_combinations() {
        let db = pairs("p1_cm_distinct");
        // (1,1) (1,2) (2,2)
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT a, b) FROM ag"),
            r(&[big(3)])
        );
        // (b, a) is the same set of combinations
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT b, a) FROM ag"),
            r(&[big(3)])
        );
        // three expressions, one of them an expression
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT a, b, a + b) FROM ag"),
            r(&[big(3)])
        );
        // per group: a=1 -> {(1,1),(1,2)}; a=2 -> {(2,2)}; a=NULL -> {}
        let rows = db.rows("SELECT a, COUNT(DISTINCT a, b) FROM ag GROUP BY a ORDER BY a");
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(2)]),
                r(&[int(2), big(1)]),
                r(&[NULL, big(0)])
            ]
        );
        db.cleanup();
    }

    #[test]
    fn several_distinct_aggregates_over_different_columns_in_one_select() {
        // A-4: each aggregate owns its seen-set.
        let db = pairs("p1_cm_multi");
        assert_eq!(
            one(
                &db,
                "SELECT COUNT(DISTINCT a), COUNT(DISTINCT b), COUNT(DISTINCT a, b), SUM(DISTINCT a), SUM(DISTINCT b), \
                 MIN(DISTINCT b), MAX(DISTINCT a) FROM ag"
            ),
            r(&[big(2), big(3), big(3), big(3), big(6), int(1), int(2)])
        );
        // grouped, with a plain COUNT beside them
        let rows = db.rows(
            "SELECT a, COUNT(DISTINCT b), SUM(DISTINCT b), COUNT(b), COUNT(DISTINCT id) FROM ag GROUP BY a ORDER BY a",
        );
        assert_eq!(
            rows,
            vec![
                // a=1: b in {1,1,2}
                r(&[int(1), big(2), big(3), big(3), big(3)]),
                // a=2: b in {NULL,2}
                r(&[int(2), big(1), big(2), big(1), big(2)]),
                // a=NULL: b in {3,NULL}
                r(&[NULL, big(1), big(3), big(1), big(2)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn empty_input_all_null_and_single_value() {
        let db = ag_db("p1_cm_empty");
        assert_eq!(
            one(
                &db,
                "SELECT COUNT(DISTINCT a, b), COUNT(a, b), COUNT(DISTINCT a) FROM ag"
            ),
            r(&[big(0), big(0), big(0)])
        );
        // grouped over empty input: no rows
        assert!(db
            .rows("SELECT a, COUNT(DISTINCT a, b) FROM ag GROUP BY a")
            .is_empty());
        seed_ag(
            &db,
            &[(1, None, None, None, None), (2, Some(1), None, None, None)],
        );
        // no row has both
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT a, b), COUNT(a, b) FROM ag"),
            r(&[big(0), big(0)])
        );
        db.write("UPDATE ag SET b = 7");
        db.write("INSERT INTO ag VALUES (3, 1, 7, NULL, NULL, NULL), (4, 1, 7, NULL, NULL, NULL)");
        // one distinct combination (1,7), three rows with it
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT a, b), COUNT(a, b) FROM ag"),
            r(&[big(1), big(3)])
        );
        db.cleanup();
    }

    #[test]
    fn many_distinct_combinations() {
        let db = ag_db("p1_cm_many");
        let rows: Vec<String> = (1..=300)
            .map(|i| format!("({i}, {}, {}, NULL, NULL, NULL)", i % 50, i % 7))
            .collect();
        db.write(&format!("INSERT INTO ag VALUES {}", rows.join(", ")));
        // a = i mod 50, b = i mod 7: by CRT i mod 350 is determined, so all 300
        // pairs are distinct.
        assert_eq!(
            one(
                &db,
                "SELECT COUNT(DISTINCT a, b), COUNT(DISTINCT a), COUNT(DISTINCT b) FROM ag"
            ),
            r(&[big(300), big(50), big(7)])
        );
        db.cleanup();
    }

    #[test]
    fn combinations_do_not_collide() {
        // ('ab','c') and ('a','bc') concatenate alike; a length-prefixed encoding
        // keeps them apart. Also ('', 'x') vs ('x', ''), NULL vs the string 'NULL'.
        let db = Db::new("p1_cm_collide");
        db.write(
            "CREATE TABLE tt (id INTEGER PRIMARY KEY, s1 TEXT, s2 TEXT, n INTEGER, f BOOLEAN)",
        );
        db.write(
            "INSERT INTO tt VALUES (1, 'ab', 'c', 1, TRUE), (2, 'a', 'bc', 1, TRUE), (3, 'ab', 'c', 1, TRUE), \
             (4, '', 'x', 2, FALSE), (5, 'x', '', 2, FALSE), (6, 'NULL', 'c', 3, TRUE), (7, NULL, 'c', 3, TRUE)",
        );
        // distinct (s1,s2) among rows with no NULL: ('ab','c'),('a','bc'),('','x'),('x',''),('NULL','c') = 5
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT s1, s2) FROM tt"),
            r(&[big(5)])
        );
        // mixed types: (s1, n, f)
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT s1, n, f) FROM tt"),
            r(&[big(5)])
        );
        db.cleanup();
    }

    #[test]
    fn floating_point_keys_use_the_grouping_equality() {
        // -0.0 and +0.0 are one value; every NaN is one value.
        let db = ag_db("p1_cm_float");
        db.write("INSERT INTO ag VALUES (1, 1, 1, NULL, NULL, 0.0), (2, 1, 1, NULL, NULL, -0.0), (3, 1, 1, NULL, NULL, 1.5)");
        assert_eq!(
            one(
                &db,
                "SELECT COUNT(DISTINCT a, d), COUNT(DISTINCT d) FROM ag"
            ),
            r(&[big(2), big(2)])
        );
        db.cleanup();
    }

    #[test]
    fn with_filter_having_order_by_and_in_expressions() {
        let db = pairs("p1_cm_filter");
        assert_eq!(
            one(
                &db,
                "SELECT COUNT(DISTINCT a, b) FILTER (WHERE id > 2) FROM ag"
            ),
            // ids 3..7: (1,2) (NULL..) (NULL..) (2,2) -> (1,2),(2,2)
            r(&[big(2)])
        );
        let rows = db.rows(
            "SELECT a FROM ag GROUP BY a HAVING COUNT(DISTINCT a, b) = 2 ORDER BY COUNT(DISTINCT a, b)",
        );
        assert_eq!(rows, vec![r(&[int(1)])]);
        assert_eq!(
            one(&db, "SELECT COUNT(DISTINCT a, b) * 2 + COUNT(a, b) FROM ag"),
            r(&[big(10)])
        );
        db.cleanup();
    }

    #[test]
    fn as_a_window_aggregate() {
        let db = pairs("p1_cm_window");
        // whole relation: every row sees 3 distinct combinations / 4 counted rows
        let rows = db.rows(
            "SELECT id, COUNT(DISTINCT a, b) OVER (), COUNT(a, b) OVER () FROM ag ORDER BY id",
        );
        assert!(rows.iter().all(|r| r[1] == big(3) && r[2] == big(4)));
        // running, by id: (1,1) (1,1) (1,2) skip skip skip (2,2)
        let rows =
            db.rows("SELECT id, COUNT(DISTINCT a, b) OVER (ORDER BY id) FROM ag ORDER BY id");
        assert_eq!(
            col(&rows, 1),
            vec![big(1), big(1), big(2), big(2), big(2), big(2), big(3)]
        );
        db.cleanup();
    }

    #[test]
    fn errors_and_explain() {
        let db = pairs("p1_cm_err");
        for (sql, what) in [
            (
                "SELECT SUM(a, b) FROM ag",
                "only COUNT takes several arguments",
            ),
            (
                "SELECT AVG(DISTINCT a, b) FROM ag",
                "only COUNT takes several arguments",
            ),
            (
                "SELECT COUNT(*, a) FROM ag",
                "COUNT(*) takes no other argument",
            ),
            (
                "SELECT COUNT(DISTINCT *) FROM ag",
                "COUNT(DISTINCT *) is not valid",
            ),
            (
                "SELECT COUNT(a, COUNT(b)) FROM ag",
                "aggregates cannot nest",
            ),
        ] {
            let err = match parse_statement(sql, &SqlLimits::default()) {
                Err(e) => e,
                Ok(_) => db.try_bind(sql).unwrap_err(),
            };
            assert!(
                matches!(
                    err,
                    SqlError::TypeMismatch { .. } | SqlError::Unsupported { .. }
                ),
                "{what}: {sql}: {err:?}"
            );
        }
        let text = crate::plan::explain(&db.plan("SELECT COUNT(DISTINCT a, b) FROM ag"));
        assert!(
            text.contains("count(DISTINCT col(t0.1), col(t0.2))"),
            "{text}"
        );
        db.cleanup();
    }

    #[test]
    fn the_seen_set_is_bounded_by_max_aggregate_state_bytes() {
        let db = ag_db("p1_cm_limit");
        let rows: Vec<String> = (1..=200)
            .map(|i| format!("({i}, {i}, {i}, NULL, NULL, NULL)"))
            .collect();
        db.write(&format!("INSERT INTO ag VALUES {}", rows.join(", ")));
        let limits = ExecLimits {
            max_aggregate_state_bytes: 2_000,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(
                "SELECT COUNT(DISTINCT a, b) FROM ag",
                &limits,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// Aggregate overflow (A-8)
// =======================================================================

mod overflow {
    use super::*;

    #[test]
    fn sum_of_bigint_overflows_with_a_typed_error_never_a_wrapped_value() {
        let db = Db::new("p1_ov_bigint");
        db.write("CREATE TABLE big (id INTEGER PRIMARY KEY, v BIGINT, g INTEGER)");
        db.write("INSERT INTO big VALUES (1, 9223372036854775807, 1), (2, 1, 1), (3, 5, 2)");
        let err = db.try_run("SELECT SUM(v) FROM big").unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
        // grouped: the group without the overflow is not reported as a partial
        // result either -- the statement fails as a whole.
        let err = db
            .try_run("SELECT g, SUM(v) FROM big GROUP BY g")
            .unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
        // as a window aggregate
        let err = db.try_run("SELECT SUM(v) OVER () FROM big").unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
        let err = db
            .try_run("SELECT SUM(v) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM big")
            .unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
        // a negative partner brings it back in range: no false positive
        db.write("UPDATE big SET v = -1 WHERE id = 2");
        assert_eq!(
            db.rows("SELECT SUM(v) FROM big WHERE g = 1"),
            vec![r(&[big(9223372036854775806)])]
        );
        // SUM(DISTINCT) is checked the same way
        db.write("UPDATE big SET v = 1 WHERE id = 2");
        let err = db.try_run("SELECT SUM(DISTINCT v) FROM big").unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err}");
        db.cleanup();
    }

    #[test]
    fn sum_of_integer_widens_instead_of_overflowing() {
        let db = Db::new("p1_ov_int");
        db.write("CREATE TABLE i (id INTEGER PRIMARY KEY, v INTEGER)");
        db.write("INSERT INTO i VALUES (1, 2147483647), (2, 2147483647), (3, -2147483648)");
        assert_eq!(db.rows("SELECT SUM(v) FROM i"), vec![r(&[big(2147483646)])]);
        assert_eq!(
            db.rows("SELECT SUM(v) FROM i WHERE v > 0"),
            vec![r(&[big(4294967294)])]
        );
        // AVG accumulates in floating point
        assert_eq!(
            db.rows("SELECT AVG(v) FROM i WHERE v > 0"),
            vec![r(&[Some(RelationalValue::Double(2147483647.0))])]
        );
        db.cleanup();
    }

    #[test]
    fn sum_of_decimal_overflows_its_precision() {
        let db = Db::new("p1_ov_dec");
        db.write("CREATE TABLE dd (id INTEGER PRIMARY KEY, v DECIMAL(38, 0))");
        let nines = "9".repeat(38);
        db.write(&format!("INSERT INTO dd VALUES (1, {nines}), (2, {nines})"));
        let err = db.try_run("SELECT SUM(v) FROM dd").unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("overflow")
                || err.to_string().to_lowercase().contains("precision"),
            "{err}"
        );
        db.write("UPDATE dd SET v = 1 WHERE id = 2");
        assert_eq!(db.rows("SELECT COUNT(*) FROM dd"), vec![r(&[big(2)])]);
        db.cleanup();
    }
}

// =======================================================================
// GROUP BY ALL
// =======================================================================

mod group_by_all {
    use super::*;

    #[test]
    fn groups_by_the_non_aggregate_select_items() {
        let db = five("p1_gba");
        let all = db.rows_sorted("SELECT g, SUM(x), COUNT(*) FROM w GROUP BY ALL");
        let explicit = db.rows_sorted("SELECT g, SUM(x), COUNT(*) FROM w GROUP BY g");
        assert_eq!(all, explicit);
        assert_eq!(all.len(), 3);
        // two keys, an expression key, an aggregate expression
        let all = db.rows_sorted("SELECT g, o, g + 1, SUM(x) + 1 FROM w GROUP BY ALL");
        let explicit = db.rows_sorted("SELECT g, o, g + 1, SUM(x) + 1 FROM w GROUP BY g, o, g + 1");
        assert_eq!(all, explicit);
        db.cleanup();
    }

    #[test]
    fn with_no_non_aggregate_item_it_is_a_global_aggregate_even_over_no_rows() {
        let db = five("p1_gba_global");
        assert_eq!(
            db.rows("SELECT COUNT(*) FROM w GROUP BY ALL"),
            vec![r(&[big(5)])]
        );
        db.write("DELETE FROM w");
        // one row, count 0 -- not zero groups
        assert_eq!(
            db.rows("SELECT COUNT(*) FROM w GROUP BY ALL"),
            vec![r(&[big(0)])]
        );
        // constants are not grouping keys either
        assert_eq!(
            db.rows("SELECT 7, COUNT(*) FROM w GROUP BY ALL"),
            vec![r(&[int(7), big(0)])]
        );
        db.cleanup();
    }

    #[test]
    fn without_any_aggregate_it_is_distinct() {
        let db = five("p1_gba_distinct");
        let all = db.rows_sorted("SELECT g FROM w GROUP BY ALL");
        let distinct = db.rows_sorted("SELECT DISTINCT g FROM w");
        assert_eq!(all, distinct);
        db.cleanup();
    }

    #[test]
    fn window_functions_are_not_keys_and_clauses_compose() {
        let db = five("p1_gba_compose");
        // the window function over the aggregate is not a key
        let rows = db.rows(
            "SELECT g, SUM(x) AS s, RANK() OVER (ORDER BY SUM(x) DESC) AS rk FROM w GROUP BY ALL ORDER BY rk",
        );
        assert_eq!(
            rows,
            vec![
                r(&[NULL, big(50), big(1)]),
                r(&[int(1), big(40), big(2)]),
                r(&[int(2), big(20), big(3)])
            ]
        );
        let rows = db.rows("SELECT g, COUNT(*) FROM w GROUP BY ALL HAVING COUNT(*) > 1 ORDER BY 1");
        assert_eq!(rows, vec![r(&[int(1), big(2)]), r(&[int(2), big(2)])]);
        let rows =
            db.rows("SELECT id FROM w WHERE g IN (SELECT g FROM w GROUP BY ALL) ORDER BY id");
        assert_eq!(ids(&rows), vec![1, 2, 3, 4]);
        db.cleanup();
    }

    #[test]
    fn refusals() {
        let db = five("p1_gba_err");
        // GROUPING names expressions of a written GROUP BY list; there is none.
        let err = bind_err(&db, "SELECT g, GROUPING(g) FROM w GROUP BY ALL");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = parse_statement(
            "SELECT g FROM w GROUP BY ALL WITH ROLLUP",
            &SqlLimits::default(),
        );
        assert!(err.is_err(), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// x <op> ANY | SOME | ALL (subquery)
// =======================================================================

mod any_all {
    use super::*;

    fn row_ids(db: &Db, sql: &str) -> Vec<i32> {
        ids(&db.rows(sql))
    }

    // S = x of g = 2 = {20, NULL}
    const S: &str = "(SELECT x FROM w WHERE g = 2)";

    #[test]
    fn any_is_a_three_valued_disjunction() {
        let db = five("p1_any");
        // 30 > 20 TRUE; 10 > 20 FALSE, 10 > NULL UNKNOWN -> UNKNOWN; 20 -> UNKNOWN;
        // NULL -> UNKNOWN; 50 > 20 TRUE
        assert_eq!(
            row_ids(
                &db,
                &format!("SELECT id FROM w WHERE x > ANY {S} ORDER BY id")
            ),
            vec![1, 5]
        );
        // SOME is a synonym
        assert_eq!(
            row_ids(
                &db,
                &format!("SELECT id FROM w WHERE x > SOME {S} ORDER BY id")
            ),
            vec![1, 5]
        );
        // the three values in the select list
        let rows = db.rows(&format!("SELECT id, x > ANY {S} FROM w ORDER BY id"));
        assert_eq!(
            col(&rows, 1),
            vec![boolean(true), NULL, NULL, NULL, boolean(true)]
        );
        db.cleanup();
    }

    #[test]
    fn all_is_a_three_valued_conjunction() {
        let db = five("p1_all");
        // 30: 30>20 TRUE, 30>NULL UNKNOWN -> UNKNOWN; 10: 10>20 FALSE -> FALSE;
        // 20: FALSE; NULL: UNKNOWN; 50: UNKNOWN.
        let rows = db.rows(&format!("SELECT id, x > ALL {S} FROM w ORDER BY id"));
        assert_eq!(
            col(&rows, 1),
            vec![NULL, boolean(false), boolean(false), NULL, NULL]
        );
        assert!(row_ids(&db, &format!("SELECT id FROM w WHERE x > ALL {S}")).is_empty());
        // NOT keeps UNKNOWN unknown: only the FALSE rows pass
        assert_eq!(
            row_ids(
                &db,
                &format!("SELECT id FROM w WHERE NOT (x > ALL {S}) ORDER BY id")
            ),
            vec![2, 3]
        );
        db.cleanup();
    }

    #[test]
    fn an_empty_set_is_false_for_any_and_true_for_all_whatever_the_operand() {
        let db = five("p1_anyall_empty");
        let empty = "(SELECT x FROM w WHERE id > 100)";
        assert!(row_ids(&db, &format!("SELECT id FROM w WHERE x > ANY {empty}")).is_empty());
        // NULL operand included: the NULL row (id 4) passes ALL over nothing
        assert_eq!(
            row_ids(
                &db,
                &format!("SELECT id FROM w WHERE x > ALL {empty} ORDER BY id")
            ),
            vec![1, 2, 3, 4, 5]
        );
        db.cleanup();
    }

    #[test]
    fn equals_any_is_in_and_not_equals_all_is_not_in() {
        let db = five("p1_anyall_in");
        for s in [
            "(SELECT x FROM w WHERE g = 2)",
            "(SELECT x FROM w WHERE g = 1)",
            "(SELECT x FROM w WHERE id > 100)",
            "(SELECT x FROM w WHERE x IS NULL)",
            "(SELECT x FROM w)",
        ] {
            let any = db.rows(&format!("SELECT id, x = ANY {s} FROM w ORDER BY id"));
            let inn = db.rows(&format!("SELECT id, x IN {s} FROM w ORDER BY id"));
            assert_eq!(any, inn, "= ANY vs IN over {s}");
            let all = db.rows(&format!("SELECT id, x <> ALL {s} FROM w ORDER BY id"));
            let not_in = db.rows(&format!("SELECT id, x NOT IN {s} FROM w ORDER BY id"));
            assert_eq!(all, not_in, "<> ALL vs NOT IN over {s}");
        }
        db.cleanup();
    }

    #[test]
    fn every_comparison_operator() {
        let db = five("p1_anyall_ops");
        // S2 = {10, 30}
        let s2 = "(SELECT x FROM w WHERE g = 1)";
        let expect = [
            ("= ANY", vec![1, 2]),
            ("<> ANY", vec![1, 2, 3, 5]),
            ("< ANY", vec![2, 3]),
            ("<= ANY", vec![1, 2, 3]),
            ("> ANY", vec![1, 3, 5]),
            (">= ANY", vec![1, 2, 3, 5]),
            ("= ALL", vec![]),
            ("<> ALL", vec![3, 5]),
            ("< ALL", vec![]),
            ("<= ALL", vec![2]),
            ("> ALL", vec![5]),
            (">= ALL", vec![1, 5]),
        ];
        // x in {30, 10, 20, NULL, 50}
        for (op, want) in expect {
            let got = row_ids(
                &db,
                &format!("SELECT id FROM w WHERE x {op} {s2} ORDER BY id"),
            );
            assert_eq!(got, want, "x {op} {{10, 30}}");
        }
        db.cleanup();
    }

    #[test]
    fn correlated_nested_and_in_other_clauses() {
        let db = five("p1_anyall_corr");
        // rows holding the maximum of their g: g=1: id 1; g=2: id 3 is UNKNOWN
        // (20 >= NULL), g NULL: no peers -> ALL over nothing is TRUE.
        assert_eq!(
            row_ids(
                &db,
                "SELECT id FROM w a WHERE x >= ALL (SELECT x FROM w b WHERE b.g = a.g) ORDER BY id"
            ),
            vec![1, 5]
        );
        // nested: the outer ANY's set is itself built with an ALL
        assert_eq!(
            row_ids(
                &db,
                "SELECT id FROM w WHERE x = ANY (SELECT x FROM w WHERE x > ALL (SELECT x FROM w WHERE g = 1 AND x < 30)) ORDER BY id"
            ),
            vec![1, 3, 5]
        );
        // HAVING and ORDER BY
        let rows = db.rows(
            "SELECT g, MAX(x) FROM w GROUP BY g HAVING MAX(x) > ANY (SELECT x FROM w WHERE g = 2) ORDER BY 1",
        );
        assert_eq!(rows, vec![r(&[int(1), int(30)]), r(&[NULL, int(50)])]);
        // DML
        db.write(&format!("DELETE FROM w WHERE x > ANY {S}"));
        assert_eq!(row_ids(&db, "SELECT id FROM w ORDER BY id"), vec![2, 3, 4]);
        db.write("UPDATE w SET o = 0 WHERE x < ALL (SELECT x FROM w WHERE id = 3)");
        assert_eq!(db.rows("SELECT o FROM w WHERE id = 2"), vec![r(&[int(0)])]);
        db.cleanup();
    }

    #[test]
    fn typing_and_other_errors() {
        let db = five("p1_anyall_err");
        // two columns
        let err = bind_err(&db, "SELECT id FROM w WHERE x > ANY (SELECT x, o FROM w)");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // type mismatch (INTEGER against TEXT) is a bind error, as for IN
        db.write("CREATE TABLE s (id INTEGER PRIMARY KEY, t TEXT)");
        let err = bind_err(&db, "SELECT id FROM w WHERE x = ANY (SELECT t FROM s)");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // an array / list on the right is not a subquery
        for sql in [
            "SELECT id FROM w WHERE x = ANY (ARRAY[1, 2])",
            "SELECT id FROM w WHERE x = ANY (1, 2)",
        ] {
            assert!(
                parse_statement(sql, &SqlLimits::default()).is_err(),
                "{sql}"
            );
        }
        // a row value is refused (no row constructor in this grammar)
        assert!(parse_statement(
            "SELECT id FROM w WHERE (x, o) IN (SELECT x, o FROM w)",
            &SqlLimits::default()
        )
        .is_err());
        db.cleanup();
    }

    #[test]
    fn the_materialized_set_is_bounded() {
        let db = five("p1_anyall_limit");
        let limits = ExecLimits {
            max_materialized_rows: 2,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(
                "SELECT id FROM w WHERE x > ANY (SELECT x FROM w)",
                &limits,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn explain_shows_the_quantifier() {
        let db = five("p1_anyall_explain");
        let text = crate::plan::explain(&db.plan(&format!("SELECT id FROM w WHERE x >= ALL {S}")));
        assert!(text.contains(">= ALL subquery"), "{text}");
        db.cleanup();
    }
}

// =======================================================================
// Grouping sets together with the Phase 1 forms
// =======================================================================

mod grouping_sets {
    use super::*;

    /// `ag` rows (id, a, b, x): (1,1,1,1) (2,1,1,2) (3,1,2,4) (4,2,2,8).
    fn db(tag: &str) -> Db {
        let db = ag_db(tag);
        seed_ag(
            &db,
            &[
                (1, Some(1), Some(1), Some(1), None),
                (2, Some(1), Some(1), Some(2), None),
                (3, Some(1), Some(2), Some(4), None),
                (4, Some(2), Some(2), Some(8), None),
            ],
        );
        db
    }

    #[test]
    fn rollup_with_count_distinct_of_a_pair_and_order_by_aliases() {
        let db = db("p1_gs_rollup");
        let rows = db.rows(
            "SELECT a, b, COUNT(DISTINCT a, b) AS c, GROUPING(a) AS ga, GROUPING(b) AS gb \
             FROM ag GROUP BY ROLLUP (a, b) ORDER BY ga, gb, a, b",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), int(1), big(1), int(0), int(0)]),
                r(&[int(1), int(2), big(1), int(0), int(0)]),
                r(&[int(2), int(2), big(1), int(0), int(0)]),
                r(&[int(1), NULL, big(2), int(0), int(1)]),
                r(&[int(2), NULL, big(1), int(0), int(1)]),
                r(&[NULL, NULL, big(3), int(1), int(1)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn cube_by_position_and_grouping_sets_by_grouping_value() {
        let db = db("p1_gs_cube");
        // CUBE(a): the sets (a) and (); sums 7, 8 and the grand total 15.
        let rows = db.rows("SELECT a, SUM(x) FROM ag GROUP BY CUBE (a) ORDER BY 2 DESC");
        assert_eq!(
            rows,
            vec![
                r(&[NULL, big(15)]),
                r(&[int(2), big(8)]),
                r(&[int(1), big(7)])
            ]
        );
        let rows = db.rows(
            "SELECT a, b, COUNT(DISTINCT a, b) FROM ag GROUP BY GROUPING SETS ((a), (b), ()) \
             ORDER BY GROUPING(a), GROUPING(b), a, b",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), NULL, big(2)]),
                r(&[int(2), NULL, big(1)]),
                r(&[NULL, int(1), big(1)]),
                r(&[NULL, int(2), big(2)]),
                r(&[NULL, NULL, big(3)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn a_window_over_grouping_sets_ordered_by_its_alias() {
        let db = db("p1_gs_window");
        let rows = db.rows(
            "SELECT a, COUNT(*) AS n, RANK() OVER (ORDER BY COUNT(*) DESC) AS rk \
             FROM ag GROUP BY ROLLUP (a) ORDER BY rk, 1",
        );
        assert_eq!(
            rows,
            vec![
                r(&[NULL, big(4), big(1)]),
                r(&[int(1), big(3), big(2)]),
                r(&[int(2), big(1), big(3)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn the_empty_set_over_empty_input_still_yields_its_row() {
        let db = db("p1_gs_empty");
        let rows = db.rows(
            "SELECT a, COUNT(DISTINCT a, b), GROUPING(a) FROM ag WHERE id > 100 GROUP BY ROLLUP (a)",
        );
        assert_eq!(rows, vec![r(&[NULL, big(0), int(1)])]);
        db.cleanup();
    }
}

// =======================================================================
// Subquery edge cases (regression pins next to ANY / ALL)
// =======================================================================

mod subquery_edges {
    use super::*;

    fn row_ids(db: &Db, sql: &str) -> Vec<i32> {
        ids(&db.rows(sql))
    }

    #[test]
    fn not_in_and_in_under_three_valued_logic() {
        let db = five("p1_sq_3vl");
        // S = {20, NULL}: no row is decisively outside it.
        assert!(row_ids(
            &db,
            "SELECT id FROM w WHERE x NOT IN (SELECT x FROM w WHERE g = 2)"
        )
        .is_empty());
        // S = {30, 10}, no NULL: the NULL operand (id 4) is UNKNOWN, the rest are outside.
        assert_eq!(
            row_ids(
                &db,
                "SELECT id FROM w WHERE x NOT IN (SELECT x FROM w WHERE g = 1) ORDER BY id"
            ),
            vec![3, 5]
        );
        // an empty set: NOT IN is TRUE for every row, a NULL operand included
        assert_eq!(
            row_ids(
                &db,
                "SELECT id FROM w WHERE x NOT IN (SELECT x FROM w WHERE id > 100) ORDER BY id"
            ),
            vec![1, 2, 3, 4, 5]
        );
        let rows = db.rows("SELECT id, x IN (SELECT x FROM w WHERE g = 2) FROM w ORDER BY id");
        assert_eq!(col(&rows, 1), vec![NULL, NULL, boolean(true), NULL, NULL]);
        db.cleanup();
    }

    #[test]
    fn scalar_subqueries_empty_one_and_many_rows() {
        let db = five("p1_sq_scalar");
        // zero rows -> NULL
        let rows =
            db.rows("SELECT id, (SELECT b.x FROM w b WHERE b.id = a.id + 10) FROM w a ORDER BY id");
        assert!(col(&rows, 1).iter().all(Option::is_none));
        // one row -> its value
        let rows =
            db.rows("SELECT id, (SELECT b.x FROM w b WHERE b.id = a.id) FROM w a ORDER BY id");
        assert_eq!(
            col(&rows, 1),
            vec![int(30), int(10), int(20), NULL, int(50)]
        );
        // an aggregate over nothing is one NULL row
        let rows = db.rows("SELECT (SELECT MAX(x) FROM w WHERE id > 100) FROM w WHERE id = 1");
        assert_eq!(rows, vec![r(&[NULL])]);
        // many rows -> a structured runtime error naming the row, never a panic
        let err = db
            .try_run("SELECT id, (SELECT b.x FROM w b WHERE b.g = a.g) FROM w a ORDER BY id")
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("more than one row"), "{err}");
        let err = db
            .try_run("SELECT (SELECT x FROM w) FROM w WHERE id = 1")
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn exists_with_nulls_and_nested_correlation() {
        let db = five("p1_sq_exists");
        // a NULL never equals anything, itself included
        assert_eq!(
            row_ids(
                &db,
                "SELECT id FROM w a WHERE EXISTS (SELECT 1 FROM w b WHERE b.x = a.x) ORDER BY id"
            ),
            vec![1, 2, 3, 5]
        );
        assert_eq!(
            row_ids(&db, "SELECT id FROM w a WHERE NOT EXISTS (SELECT 1 FROM w b WHERE b.x = a.x) ORDER BY id"),
            vec![4]
        );
        // two levels deep: some pair b, c of a's own group with c.x > b.x
        assert_eq!(
            row_ids(
                &db,
                "SELECT id FROM w a WHERE EXISTS (SELECT 1 FROM w b WHERE b.g = a.g AND \
                 EXISTS (SELECT 1 FROM w c WHERE c.x > b.x AND c.g = a.g)) ORDER BY id"
            ),
            vec![1, 2]
        );
        db.cleanup();
    }
}

mod grouping_sets_correlated {
    use super::*;

    /// `ag` rows (id, a, b, x): (1,1,1,1) (2,1,2,2) (3,2,1,4) (4,NULL,1,8).
    fn db(tag: &str) -> Db {
        let db = ag_db(tag);
        seed_ag(
            &db,
            &[
                (1, Some(1), Some(1), Some(1), None),
                (2, Some(1), Some(2), Some(2), None),
                (3, Some(2), Some(1), Some(4), None),
                (4, None, Some(1), Some(8), None),
            ],
        );
        db
    }

    #[test]
    fn omitted_grouping_columns_read_null_inside_a_correlated_subquery() {
        let db = db("p1_gsc_null");
        // z.a = ag.a with a real grouping value counts the rows of that group; in the
        // grand-total row ag.a reads NULL, so `z.a = NULL` matches nothing.
        let rows = db.rows(
            "SELECT a, (SELECT COUNT(*) FROM ag z WHERE z.a = ag.a), COUNT(*), GROUPING(a) FROM ag GROUP BY ROLLUP (a) ORDER BY GROUPING(a), a",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(2), big(2), int(0)]),
                r(&[int(2), big(1), big(1), int(0)]),
                // the real NULL group: `z.a = NULL` is UNKNOWN, so the inner count is 0
                r(&[NULL, big(0), big(1), int(0)]),
                // the grand-total row: the omitted column reads NULL
                r(&[NULL, big(0), big(4), int(1)]),
            ]
        );
        // `IS NOT DISTINCT`-free way to tell: EXISTS over the same correlation
        let rows = db.rows(
            "SELECT a, EXISTS (SELECT 1 FROM ag z WHERE z.a = ag.a) FROM ag GROUP BY CUBE (a) ORDER BY GROUPING(a), a",
        );
        assert_eq!(
            col(&rows, 1),
            vec![boolean(true), boolean(true), boolean(false), boolean(false)]
        );
        db.cleanup();
    }

    #[test]
    fn two_grouping_columns_and_a_set_that_keeps_one() {
        let db = db("p1_gsc_two");
        // For GROUPING SETS ((a), (b)): in the (a) rows b reads NULL, and the reverse.
        let rows = db.rows(
            "SELECT a, b, (SELECT COUNT(*) FROM ag z WHERE z.a = ag.a AND z.b = ag.b) \
             FROM ag GROUP BY GROUPING SETS ((a), (b)) ORDER BY GROUPING(a), a, b",
        );
        // every inner comparison involves an omitted (NULL) column, so every count is 0
        assert!(rows.iter().all(|r| r[2] == big(0)), "{rows:?}");
        assert_eq!(rows.len(), 3 + 2);
        // reading only the kept column works in every set that keeps it
        let rows = db.rows(
            "SELECT a, b, (SELECT COUNT(*) FROM ag z WHERE z.a = ag.a) \
             FROM ag GROUP BY GROUPING SETS ((a), (b)) ORDER BY GROUPING(a), a, b",
        );
        // (a) sets: a=1 -> 2, a=2 -> 1, a=NULL -> 0 ; (b) sets: a reads NULL -> 0
        assert_eq!(col(&rows, 2), vec![big(2), big(1), big(0), big(0), big(0)]);
        db.cleanup();
    }

    #[test]
    fn a_subquery_may_still_not_read_a_column_that_is_not_grouped() {
        let db = db("p1_gsc_ungrouped");
        let err = bind_err(
            &db,
            "SELECT (SELECT COUNT(*) FROM ag z WHERE z.x = ag.x) FROM ag GROUP BY ROLLUP (a)",
        );
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }
}
