//! SQL-mission-2, Family A -- window functions.
//!
//! Layout: `parser` (grammar accepted), `binder` (names, types, restrictions),
//! then the execution modules added with each sub-slice (2A: `OVER`, `PARTITION
//! BY`, `ORDER BY`, default frames, `ROW_NUMBER` / `RANK` / `DENSE_RANK`,
//! aggregates as window functions), and the HTTP tests in
//! `api/tests/api_sql_mission2.rs`.
//!
//! Every execution test runs through the real parse, bind, plan and execute
//! pipeline against a real engine (`mission1_support::Db`).

use crate::mission1_support::*;

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::error::SqlError;
use crate::exec::{ExecLimits, ExecMetrics};
use crate::limits::SqlLimits;
use crate::parse::parse_statement;
use crate::setop_tests::{bind_err, bound};

pub(crate) fn dbl(f: f64) -> V {
    Some(RelationalValue::Double(f))
}

/// `w(id INTEGER PK, g INTEGER, o INTEGER, x INTEGER, d DOUBLE)`: `g` is a
/// partition key, `o` an ordering key (both nullable), `x` an integer value and
/// `d` a double value.
pub(crate) fn win_db(tag: &str) -> Db {
    let db = Db::new(tag);
    db.write("CREATE TABLE w (id INTEGER PRIMARY KEY, g INTEGER, o INTEGER, x INTEGER, d DOUBLE)");
    db
}

type Seed = (i32, Option<i32>, Option<i32>, Option<i32>);

fn lit(v: Option<i32>) -> String {
    v.map_or("NULL".to_string(), |n| n.to_string())
}

/// Inserts `(id, g, o, x)` rows (`d` is `x` as a double, or NULL).
pub(crate) fn seed_w(db: &Db, rows: &[Seed]) {
    if rows.is_empty() {
        return;
    }
    let values: Vec<String> = rows
        .iter()
        .map(|(id, g, o, x)| {
            format!(
                "({id}, {}, {}, {}, {})",
                lit(*g),
                lit(*o),
                lit(*x),
                x.map_or("NULL".to_string(), |n| format!("{n}.0"))
            )
        })
        .collect();
    db.write(&format!("INSERT INTO w VALUES {}", values.join(", ")));
}

fn win_db_with(tag: &str, rows: &[Seed]) -> Db {
    let db = win_db(tag);
    seed_w(&db, rows);
    db
}

/// Column `col` of every row of `rows`.
pub(crate) fn col(rows: &[Row], col: usize) -> Vec<V> {
    rows.iter().map(|r| r[col].clone()).collect()
}

pub(crate) fn bigs(ns: &[i64]) -> Vec<V> {
    ns.iter().map(|n| big(*n)).collect()
}

/// `(id, g, o, x)` for the common five-row table: ids 1..5, `x` = 10..50.
fn five() -> Vec<Seed> {
    (1..=5).map(|i| (i, None, Some(i), Some(i * 10))).collect()
}

// =======================================================================
// parser
// =======================================================================

mod parser {
    use super::*;
    use crate::ast::{self, Expr, FrameBound, FrameUnits, WindowOver};

    fn select_of(sql: &str) -> ast::Select {
        match parse_statement(sql, &SqlLimits::default())
            .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
        {
            ast::Statement::Select(s) => *s,
            other => panic!("expected a SELECT, got {other:?}"),
        }
    }

    fn first_expr(sql: &str) -> Expr {
        match &select_of(sql).projection[0] {
            ast::SelectItem::Item(i) => i.expr.clone(),
            other => panic!("expected an expression, got {other:?}"),
        }
    }

    #[test]
    fn over_clause_forms_parse_into_a_window_call() {
        for sql in [
            "SELECT SUM(x) OVER () FROM w",
            "SELECT SUM(x) OVER (PARTITION BY g) FROM w",
            "SELECT SUM(x) OVER (PARTITION BY g, o ORDER BY id) FROM w",
            "SELECT SUM(x) OVER (ORDER BY id DESC NULLS FIRST) FROM w",
            "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM w",
            "SELECT RANK() OVER (PARTITION BY g ORDER BY o DESC) FROM w",
            "SELECT DENSE_RANK() OVER (ORDER BY o) FROM w",
            "SELECT COUNT(*) OVER () FROM w",
            "SELECT SUM(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM w",
            "SELECT SUM(x) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM w",
            "SELECT SUM(x) OVER w FROM w WINDOW w AS (ORDER BY id)",
        ] {
            let e = first_expr(sql);
            assert!(matches!(e, Expr::Window(_)), "{sql}: {e:?}");
        }
    }

    #[test]
    fn an_over_clause_is_never_dropped() {
        // The pre-Slice-D defect: `SUM(x) OVER ()` was converted to the plain
        // aggregate. It must be a window call, not an `Aggregate`.
        let e = first_expr("SELECT SUM(x) OVER () FROM w");
        assert!(!matches!(e, Expr::Aggregate { .. }));
        let Expr::Window(call) = e else {
            panic!("not a window call")
        };
        assert!(matches!(call.over, WindowOver::Spec(_)));
    }

    #[test]
    fn the_spec_is_kept_in_full() {
        let Expr::Window(call) = first_expr(
            "SELECT SUM(x) OVER (PARTITION BY g, o ORDER BY id DESC ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM w",
        ) else {
            panic!()
        };
        let WindowOver::Spec(spec) = call.over else {
            panic!()
        };
        assert_eq!(spec.partition_by.len(), 2);
        assert_eq!(spec.order_by.len(), 1);
        assert!(spec.order_by[0].descending);
        let frame = spec.frame.unwrap();
        assert_eq!(frame.units, FrameUnits::Rows);
        assert!(matches!(frame.start, FrameBound::Preceding(_)));
        assert!(matches!(frame.end, FrameBound::CurrentRow));
    }

    #[test]
    fn named_windows_are_parsed() {
        let s = select_of("SELECT SUM(x) OVER w FROM w WINDOW w AS (PARTITION BY g ORDER BY id)");
        assert_eq!(s.named_windows.len(), 1);
        assert_eq!(s.named_windows[0].0.value, "w");
    }

    #[test]
    fn a_call_that_is_not_a_window_function_is_a_typed_error() {
        let err =
            parse_statement("SELECT abs(x) OVER () FROM w", &SqlLimits::default()).unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
    }

    #[test]
    fn what_is_still_outside_the_grammar_after_mission_2_is_a_typed_error() {
        // `parse_tests::unsupported_grammar_is_a_typed_error_not_a_panic` (left
        // unchanged) stops at its `ROLLUP` case, which Mission 2 supports, and so
        // no longer reaches these; they are Mission 3's (joins) and still refused.
        for sql in [
            "SELECT id FROM t GROUP BY ALL",
            "SELECT * FROM t, orders",
            "SELECT id FROM t RIGHT JOIN orders o ON t.id = o.id",
            "SELECT id FROM t FULL JOIN orders o ON t.id = o.id",
            "SELECT id FROM t NATURAL JOIN orders",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(matches!(err, SqlError::Unsupported { .. }), "{sql}: {err:?}");
        }
    }

    #[test]
    fn what_stays_outside_the_grammar_is_a_typed_error() {
        for sql in [
            "SELECT SUM(x) OVER (ORDER BY id GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
            "SELECT LAG(x) IGNORE NULLS OVER (ORDER BY id) FROM w",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(err, SqlError::Unsupported { .. } | SqlError::Parse { .. }),
                "{sql}: {err:?}"
            );
        }
    }
}

// =======================================================================
// binder
// =======================================================================

mod binder {
    use super::*;

    #[test]
    fn result_types_and_nullability() {
        let db = win_db("win_bind_types");
        let b = bound(
            &db,
            "SELECT ROW_NUMBER() OVER (ORDER BY id), RANK() OVER (ORDER BY id), DENSE_RANK() OVER (ORDER BY id), \
             SUM(x) OVER (), COUNT(x) OVER (), AVG(x) OVER (), MIN(x) OVER (), SUM(d) OVER () FROM w",
        );
        let ty: Vec<_> = b
            .projection
            .iter()
            .map(|i| (i.expr.ty, i.expr.nullable))
            .collect();
        assert_eq!(
            ty,
            vec![
                (Some(RelationalType::Bigint), false),
                (Some(RelationalType::Bigint), false),
                (Some(RelationalType::Bigint), false),
                (Some(RelationalType::Bigint), true),
                (Some(RelationalType::Bigint), false),
                (Some(RelationalType::Double), true),
                (Some(RelationalType::Integer), true),
                (Some(RelationalType::Double), true),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn calls_are_collected_into_the_windows_list() {
        let db = win_db("win_bind_collect");
        let b = bound(&db, "SELECT SUM(x) OVER (ORDER BY id), SUM(x) OVER (ORDER BY id), RANK() OVER (ORDER BY id) FROM w");
        // Identical calls share a slot.
        assert_eq!(b.windows.len(), 2);
        assert!(b
            .projection
            .iter()
            .all(|i| matches!(i.expr.kind, crate::bound::BoundExprKind::WindowRef(_))));
        db.cleanup();
    }

    #[test]
    fn the_default_frame_is_resolved_at_bind_time() {
        use crate::ast::FrameUnits;
        use crate::bound::BoundFrameBound as B;
        let db = win_db("win_bind_frame_default");
        let with_order = bound(&db, "SELECT SUM(x) OVER (ORDER BY id) FROM w");
        let f = &with_order.windows[0].spec.frame;
        assert_eq!(f.units, FrameUnits::Range);
        assert_eq!((&f.start, &f.end), (&B::UnboundedPreceding, &B::CurrentRow));
        assert!(!f.explicit);
        let without = bound(&db, "SELECT SUM(x) OVER () FROM w");
        let f = &without.windows[0].spec.frame;
        assert_eq!(f.units, FrameUnits::Range);
        assert_eq!(
            (&f.start, &f.end),
            (&B::UnboundedPreceding, &B::UnboundedFollowing)
        );
        db.cleanup();
    }

    #[test]
    fn an_order_by_default_follows_d5() {
        use crate::bound::NullsOrder;
        let db = win_db("win_bind_nulls");
        let b = bound(
            &db,
            "SELECT SUM(x) OVER (ORDER BY o), SUM(x) OVER (ORDER BY o DESC), SUM(x) OVER (ORDER BY o NULLS FIRST) FROM w",
        );
        let n: Vec<_> = b.windows.iter().map(|c| c.spec.order_by[0].nulls).collect();
        assert_eq!(
            n,
            vec![NullsOrder::Last, NullsOrder::First, NullsOrder::First]
        );
        db.cleanup();
    }

    #[test]
    fn a_window_function_is_refused_outside_the_select_list_qualify_and_order_by() {
        let db = win_db("win_bind_refused");
        for sql in [
            "SELECT id FROM w WHERE SUM(x) OVER () > 1",
            "SELECT id FROM w WHERE ROW_NUMBER() OVER (ORDER BY id) = 1",
            "SELECT g FROM w GROUP BY g HAVING SUM(x) OVER () > 1",
            "SELECT id FROM w GROUP BY ROW_NUMBER() OVER (ORDER BY id)",
            "SELECT a.id FROM w a JOIN w b ON ROW_NUMBER() OVER (ORDER BY a.id) = b.id",
        ] {
            let err = bind_err(&db, sql);
            assert!(
                matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("window functions are not allowed")),
                "{sql}: {err:?}"
            );
        }
        db.cleanup();
    }

    #[test]
    fn a_window_function_may_not_contain_another() {
        let db = win_db("win_bind_nested");
        let err = bind_err(
            &db,
            "SELECT SUM(ROW_NUMBER() OVER (ORDER BY id)) OVER () FROM w",
        );
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn rank_functions_need_an_order_by() {
        let db = win_db("win_bind_order_required");
        for f in ["RANK()", "DENSE_RANK()"] {
            let err = bind_err(&db, &format!("SELECT {f} OVER () FROM w"));
            assert!(
                matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("ORDER BY")),
                "{f}: {err:?}"
            );
        }
        // ROW_NUMBER does not.
        bound(&db, "SELECT ROW_NUMBER() OVER () FROM w");
        db.cleanup();
    }

    #[test]
    fn unknown_columns_and_windows_are_errors() {
        let db = win_db("win_bind_unknown");
        assert!(matches!(
            bind_err(&db, "SELECT SUM(nope) OVER () FROM w"),
            SqlError::UnknownObject { .. }
        ));
        assert!(matches!(
            bind_err(&db, "SELECT SUM(x) OVER (PARTITION BY nope) FROM w"),
            SqlError::UnknownObject { .. }
        ));
        assert!(matches!(
            bind_err(&db, "SELECT SUM(x) OVER nowindow FROM w"),
            SqlError::InvalidIdentifier { .. }
        ));
        db.cleanup();
    }

    #[test]
    fn argument_types_are_checked_like_the_plain_aggregate() {
        let db = Db::new("win_bind_types_check");
        let err = bind_err(&db, "SELECT SUM(name) OVER () FROM t");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_window_over_a_table_the_principal_cannot_read_is_unknown_object() {
        // Authorization is the binder's, before and independent of windows.
        use crate::auth::AuthContext;
        let db = win_db("win_bind_auth");
        let err = db
            .try_bind_with(
                "SELECT SUM(x) OVER () FROM w",
                &AuthContext::none("nobody"),
                &SqlLimits::default(),
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// 2A -- execution: OVER, PARTITION BY, ORDER BY, default frames
// =======================================================================

mod exec_2a {
    use super::*;

    #[test]
    fn running_total_is_a_sum_from_the_first_row_to_the_current_one() {
        let db = win_db_with("win_running_total", &five());
        let rows = db.rows("SELECT id, SUM(x) OVER (ORDER BY id) FROM w ORDER BY id");
        assert_eq!(col(&rows, 1), bigs(&[10, 30, 60, 100, 150]));
        db.cleanup();
    }

    #[test]
    fn over_empty_is_the_whole_relation_as_one_partition() {
        let db = win_db_with("win_over_empty", &five());
        let rows = db
            .rows("SELECT SUM(x) OVER (), COUNT(*) OVER (), MIN(x) OVER (), MAX(x) OVER () FROM w");
        assert_eq!(rows.len(), 5);
        for r in rows {
            assert_eq!(r, vec![big(150), big(5), int(10), int(50)]);
        }
        db.cleanup();
    }

    #[test]
    fn partition_by_restarts_the_window_per_group() {
        let db = win_db_with(
            "win_partition",
            &[
                (1, Some(1), Some(1), Some(10)),
                (2, Some(1), Some(2), Some(20)),
                (3, Some(2), Some(1), Some(30)),
                (4, Some(2), Some(2), Some(40)),
                (5, Some(2), Some(3), Some(50)),
            ],
        );
        let whole = db.rows("SELECT id, SUM(x) OVER (PARTITION BY g) FROM w ORDER BY id");
        assert_eq!(col(&whole, 1), bigs(&[30, 30, 120, 120, 120]));
        let running =
            db.rows("SELECT id, SUM(x) OVER (PARTITION BY g ORDER BY id) FROM w ORDER BY id");
        assert_eq!(col(&running, 1), bigs(&[10, 30, 30, 70, 120]));
        db.cleanup();
    }

    #[test]
    fn rank_functions_on_tied_data() {
        // o: 10, 20, 20, 30 -> two rows tie at the second place.
        let db = win_db_with(
            "win_rank_ties",
            &[
                (1, None, Some(10), Some(1)),
                (2, None, Some(20), Some(2)),
                (3, None, Some(20), Some(3)),
                (4, None, Some(30), Some(4)),
            ],
        );
        let rows = db.rows(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY o), RANK() OVER (ORDER BY o), DENSE_RANK() OVER (ORDER BY o) \
             FROM w ORDER BY id",
        );
        assert_eq!(col(&rows, 1), bigs(&[1, 2, 3, 4])); // distinct (ties broken by input order)
        assert_eq!(col(&rows, 2), bigs(&[1, 2, 2, 4])); // gap after the tie
        assert_eq!(col(&rows, 3), bigs(&[1, 2, 2, 3])); // no gap
        db.cleanup();
    }

    #[test]
    fn rank_restarts_per_group() {
        // The canonical example: RANK() OVER (PARTITION BY g ORDER BY x DESC).
        let db = win_db_with(
            "win_rank_group",
            &[
                (1, Some(1), None, Some(5)),
                (2, Some(1), None, Some(9)),
                (3, Some(1), None, Some(9)),
                (4, Some(2), None, Some(1)),
                (5, Some(2), None, Some(2)),
            ],
        );
        let rows =
            db.rows("SELECT g, x, RANK() OVER (PARTITION BY g ORDER BY x DESC) FROM w ORDER BY id");
        assert_eq!(col(&rows, 2), bigs(&[3, 1, 1, 2, 1]));
        db.cleanup();
    }

    #[test]
    fn default_frame_with_an_order_by_runs_to_the_current_row_and_includes_peers() {
        // o: 10, 20, 20, 30; x: 1..4. RANGE (not ROWS): the two o=20 rows are
        // peers, so each sees both.
        let db = win_db_with(
            "win_default_frame_order",
            &[
                (1, None, Some(10), Some(1)),
                (2, None, Some(20), Some(2)),
                (3, None, Some(20), Some(3)),
                (4, None, Some(30), Some(4)),
            ],
        );
        let rows = db.rows("SELECT id, SUM(x) OVER (ORDER BY o) FROM w ORDER BY id");
        assert_eq!(col(&rows, 1), bigs(&[1, 6, 6, 10]));
        db.cleanup();
    }

    #[test]
    fn default_frame_without_an_order_by_is_the_whole_partition() {
        let db = win_db_with(
            "win_default_frame_no_order",
            &[
                (1, Some(1), None, Some(1)),
                (2, Some(1), None, Some(2)),
                (3, Some(2), None, Some(4)),
            ],
        );
        let rows = db.rows("SELECT id, SUM(x) OVER (PARTITION BY g) FROM w ORDER BY id");
        assert_eq!(col(&rows, 1), bigs(&[3, 3, 4]));
        db.cleanup();
    }

    #[test]
    fn null_partition_keys_form_one_partition() {
        let db = win_db_with(
            "win_null_partition",
            &[
                (1, None, None, Some(1)),
                (2, None, None, Some(2)),
                (3, Some(7), None, Some(4)),
            ],
        );
        let rows = db.rows("SELECT id, COUNT(*) OVER (PARTITION BY g), SUM(x) OVER (PARTITION BY g) FROM w ORDER BY id");
        assert_eq!(col(&rows, 1), bigs(&[2, 2, 1]));
        assert_eq!(col(&rows, 2), bigs(&[3, 3, 4]));
        db.cleanup();
    }

    #[test]
    fn nulls_in_the_window_order_follow_d5() {
        // o: 5, NULL, 3.
        let db = win_db_with(
            "win_null_order",
            &[
                (1, None, Some(5), Some(1)),
                (2, None, None, Some(2)),
                (3, None, Some(3), Some(3)),
            ],
        );
        let n = |sql: &str| col(&db.rows(sql), 1);
        // ascending: NULLS LAST.
        assert_eq!(
            n("SELECT id, ROW_NUMBER() OVER (ORDER BY o) FROM w ORDER BY id"),
            bigs(&[2, 3, 1])
        );
        // descending: NULLS FIRST.
        assert_eq!(
            n("SELECT id, ROW_NUMBER() OVER (ORDER BY o DESC) FROM w ORDER BY id"),
            bigs(&[2, 1, 3])
        );
        // explicit placement.
        assert_eq!(
            n("SELECT id, ROW_NUMBER() OVER (ORDER BY o NULLS FIRST) FROM w ORDER BY id"),
            bigs(&[3, 1, 2])
        );
        assert_eq!(
            n("SELECT id, ROW_NUMBER() OVER (ORDER BY o DESC NULLS LAST) FROM w ORDER BY id"),
            bigs(&[1, 3, 2])
        );
        db.cleanup();
    }

    #[test]
    fn an_all_null_order_column_makes_every_row_a_peer() {
        let db = win_db_with(
            "win_all_null_order",
            &[
                (1, None, None, Some(1)),
                (2, None, None, Some(2)),
                (3, None, None, Some(4)),
            ],
        );
        let rows = db.rows(
            "SELECT id, RANK() OVER (ORDER BY o), SUM(x) OVER (ORDER BY o) FROM w ORDER BY id",
        );
        assert_eq!(col(&rows, 1), bigs(&[1, 1, 1]));
        assert_eq!(col(&rows, 2), bigs(&[7, 7, 7]));
        db.cleanup();
    }

    #[test]
    fn aggregates_ignore_nulls_and_an_all_null_frame_is_null_but_count_is_zero() {
        let db = win_db_with(
            "win_agg_nulls",
            &[
                (1, Some(1), None, None),
                (2, Some(1), None, None),
                (3, Some(2), None, Some(5)),
            ],
        );
        let rows = db.rows(
            "SELECT id, SUM(x) OVER (PARTITION BY g), COUNT(x) OVER (PARTITION BY g), COUNT(*) OVER (PARTITION BY g), \
             AVG(x) OVER (PARTITION BY g), MIN(x) OVER (PARTITION BY g), MAX(x) OVER (PARTITION BY g) FROM w ORDER BY id",
        );
        assert_eq!(rows[0][1..], [NULL, big(0), big(2), NULL, NULL, NULL]);
        assert_eq!(
            rows[2][1..],
            [big(5), big(1), big(1), dbl(5.0), int(5), int(5)]
        );
        db.cleanup();
    }

    #[test]
    fn two_different_windows_in_one_select_are_not_merged() {
        // If the two windows were merged one of them would be wrong.
        let db = win_db_with("win_two_windows", &five());
        let rows =
            db.rows("SELECT id, SUM(x) OVER (ORDER BY id), SUM(x) OVER () FROM w ORDER BY id");
        assert_eq!(col(&rows, 1), bigs(&[10, 30, 60, 100, 150]));
        assert_eq!(col(&rows, 2), bigs(&[150, 150, 150, 150, 150]));
        let plan = crate::plan::explain(
            &db.plan("SELECT id, SUM(x) OVER (ORDER BY id), SUM(x) OVER () FROM w"),
        );
        assert_eq!(plan.matches("Window ").count(), 2, "{plan}");
        db.cleanup();
    }

    #[test]
    fn provably_identical_windows_share_one_operator() {
        let db = win_db_with("win_shared_window", &five());
        let sql = "SELECT SUM(x) OVER (PARTITION BY g ORDER BY id), COUNT(*) OVER (PARTITION BY g ORDER BY id), \
                   RANK() OVER (PARTITION BY g ORDER BY id) FROM w";
        let plan = crate::plan::explain(&db.plan(sql));
        assert_eq!(plan.matches("Window ").count(), 1, "{plan}");
        // ... and the explicit spelling of the default frame is the same window.
        let sql = "SELECT SUM(x) OVER (ORDER BY id), COUNT(*) OVER (ORDER BY id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM w";
        let plan = crate::plan::explain(&db.plan(sql));
        assert_eq!(plan.matches("Window ").count(), 1, "{plan}");
        db.cleanup();
    }

    #[test]
    fn a_window_over_an_aggregated_query_runs_after_the_grouping() {
        let db = win_db_with(
            "win_over_groups",
            &[
                (1, Some(1), None, Some(10)),
                (2, Some(1), None, Some(20)),
                (3, Some(2), None, Some(5)),
                (4, Some(3), None, Some(100)),
            ],
        );
        let rows = db.rows(
            "SELECT g, SUM(x), RANK() OVER (ORDER BY SUM(x) DESC), SUM(SUM(x)) OVER () FROM w GROUP BY g ORDER BY g",
        );
        assert_eq!(col(&rows, 1), bigs(&[30, 5, 100]));
        assert_eq!(col(&rows, 2), bigs(&[2, 3, 1]));
        assert_eq!(col(&rows, 3), bigs(&[135, 135, 135]));
        db.cleanup();
    }

    #[test]
    fn an_ungrouped_column_inside_a_window_of_an_aggregated_query_is_refused() {
        let db = win_db_with("win_ungrouped", &five());
        let err = db
            .try_run("SELECT g, SUM(x), SUM(o) OVER () FROM w GROUP BY g")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn empty_input_yields_no_rows() {
        let db = win_db("win_empty");
        let m = ExecMetrics::default();
        let r = db
            .try_run_with(
                "SELECT SUM(x) OVER (), ROW_NUMBER() OVER (ORDER BY id) FROM w",
                &ExecLimits::default(),
                &m,
                &[],
            )
            .unwrap();
        assert!(r.rows.is_empty());
        assert_eq!(m.snapshot().window_partitions_created_total, 0);
        db.cleanup();
    }

    #[test]
    fn a_single_row_partition() {
        let db = win_db_with("win_single", &[(1, Some(1), Some(1), Some(7))]);
        let rows = db.rows("SELECT SUM(x) OVER (PARTITION BY g ORDER BY o), RANK() OVER (ORDER BY o), ROW_NUMBER() OVER () FROM w");
        assert_eq!(rows, vec![vec![big(7), big(1), big(1)]]);
        db.cleanup();
    }

    #[test]
    fn the_input_row_order_is_preserved_without_an_outer_order_by() {
        // The window's ORDER BY decides what is computed, not the output order.
        let db = win_db_with(
            "win_input_order",
            &[
                (1, None, Some(30), Some(1)),
                (2, None, Some(10), Some(2)),
                (3, None, Some(20), Some(3)),
            ],
        );
        let rows = db.rows("SELECT id, ROW_NUMBER() OVER (ORDER BY o DESC) FROM w");
        assert_eq!(col(&rows, 0), vec![int(1), int(2), int(3)]);
        assert_eq!(col(&rows, 1), bigs(&[1, 3, 2]));
        db.cleanup();
    }

    #[test]
    fn a_window_function_in_the_order_by_clause() {
        let db = win_db_with(
            "win_in_order_by",
            &[
                (1, None, Some(30), Some(1)),
                (2, None, Some(10), Some(2)),
                (3, None, Some(20), Some(3)),
            ],
        );
        let rows = db.rows("SELECT id FROM w ORDER BY ROW_NUMBER() OVER (ORDER BY o DESC) DESC");
        assert_eq!(col(&rows, 0), vec![int(2), int(3), int(1)]);
        db.cleanup();
    }

    #[test]
    fn distinct_limit_and_where_compose_with_windows() {
        let db = win_db_with("win_compose", &five());
        // WHERE runs before the window: the running total skips the filtered rows.
        let rows = db.rows("SELECT id, SUM(x) OVER (ORDER BY id) FROM w WHERE id > 2 ORDER BY id");
        assert_eq!(col(&rows, 1), bigs(&[30, 70, 120]));
        // LIMIT runs after it: the totals are those of the whole relation.
        let rows = db.rows("SELECT id, SUM(x) OVER (ORDER BY id) FROM w ORDER BY id LIMIT 2");
        assert_eq!(col(&rows, 1), bigs(&[10, 30]));
        // DISTINCT sees the window results.
        let rows = db.rows("SELECT DISTINCT SUM(x) OVER () FROM w");
        assert_eq!(rows, vec![vec![big(150)]]);
        db.cleanup();
    }

    #[test]
    fn a_window_over_a_join_derived_table_and_cte() {
        let db = win_db_with("win_sources", &five());
        let rows = db.rows(
            "SELECT a.id, SUM(b.x) OVER (ORDER BY a.id) FROM w a JOIN w b ON a.id = b.id ORDER BY a.id",
        );
        assert_eq!(col(&rows, 1), bigs(&[10, 30, 60, 100, 150]));
        let rows = db.rows("SELECT id, SUM(x) OVER (ORDER BY id) FROM (SELECT id, x FROM w WHERE id <= 3) q ORDER BY id");
        assert_eq!(col(&rows, 1), bigs(&[10, 30, 60]));
        let rows = db.rows(
            "WITH c AS (SELECT id, x FROM w) SELECT id, SUM(x) OVER (ORDER BY id) FROM c ORDER BY id",
        );
        assert_eq!(col(&rows, 1), bigs(&[10, 30, 60, 100, 150]));
        db.cleanup();
    }

    #[test]
    fn a_window_in_a_subquery_and_a_set_operation_branch() {
        let db = win_db_with("win_subquery", &five());
        let rows = db.rows(
            "SELECT id FROM w WHERE id IN (SELECT id FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY id DESC) AS rn FROM w) q WHERE rn <= 2) ORDER BY id",
        );
        assert_eq!(col(&rows, 0), vec![int(4), int(5)]);
        let rows = db.rows(
            "SELECT SUM(x) OVER () FROM w WHERE id = 1 UNION ALL SELECT SUM(x) OVER () FROM w WHERE id = 2",
        );
        assert_eq!(rows, vec![vec![big(10)], vec![big(20)]]);
        db.cleanup();
    }

    // ---- snapshot consistency (hazard 10)

    #[test]
    fn a_window_reads_the_statement_snapshot() {
        let db = win_db_with("win_snapshot", &five());
        let txn = db.txm.begin().unwrap();
        let before = db
            .run_in_txn("SELECT SUM(x) OVER () FROM w", &txn)
            .unwrap()
            .rows;
        assert_eq!(before.len(), 5);
        // Another session commits a row after the transaction's snapshot.
        db.write("INSERT INTO w VALUES (6, NULL, 6, 60, 60.0)");
        let again = db
            .run_in_txn("SELECT SUM(x) OVER () FROM w", &txn)
            .unwrap()
            .rows;
        assert_eq!(
            again, before,
            "the row written after the snapshot must not appear"
        );
        assert_eq!(again[0], vec![big(150)]);
        let _ = txn.rollback();
        // A new statement sees it.
        let now = db.rows("SELECT SUM(x) OVER () FROM w");
        assert_eq!(now.len(), 6);
        assert_eq!(now[0], vec![big(210)]);
        db.cleanup();
    }

    // ---- limits and metrics

    #[test]
    fn a_partition_larger_than_the_limit_is_a_resource_limit() {
        let db = win_db_with("win_limit_partition", &five());
        let limits = ExecLimits {
            max_window_partition_rows: 4,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(
                "SELECT SUM(x) OVER () FROM w",
                &limits,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::ResourceLimit { detail } if detail.contains("max_window_partition_rows")),
            "{err:?}"
        );
        // Partitioned into pieces of at most 4 it runs.
        let ok = ExecLimits {
            max_window_partition_rows: 5,
            ..ExecLimits::default()
        };
        assert_eq!(
            db.try_run_with(
                "SELECT SUM(x) OVER () FROM w",
                &ok,
                &ExecMetrics::default(),
                &[]
            )
            .unwrap()
            .rows
            .len(),
            5
        );
        db.cleanup();
    }

    #[test]
    fn the_buffer_is_bounded_by_the_materialization_limits() {
        let db = win_db_with("win_limit_materialized", &five());
        let rows = ExecLimits {
            max_materialized_rows: 3,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(
                "SELECT SUM(x) OVER () FROM w",
                &rows,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::ResourceLimit { detail } if detail.contains("max_materialized_rows")),
            "{err:?}"
        );
        let bytes = ExecLimits {
            max_materialized_bytes: 100,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(
                "SELECT SUM(x) OVER () FROM w",
                &bytes,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::ResourceLimit { detail } if detail.contains("max_materialized_bytes")),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn too_many_window_functions_is_a_resource_limit() {
        let db = win_db_with("win_limit_functions", &five());
        let limits = ExecLimits {
            max_window_functions_per_statement: 2,
            ..ExecLimits::default()
        };
        let sql3 = "SELECT SUM(x) OVER (), COUNT(*) OVER (), MIN(x) OVER () FROM w";
        let err = db
            .try_run_with(sql3, &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::ResourceLimit { detail } if detail.contains("max_window_functions_per_statement")),
            "{err:?}"
        );
        let sql2 = "SELECT SUM(x) OVER (), COUNT(*) OVER () FROM w";
        assert!(db
            .try_run_with(sql2, &limits, &ExecMetrics::default(), &[])
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn metrics_count_partitions_and_function_evaluations() {
        let db = win_db_with(
            "win_metrics",
            &[
                (1, Some(1), Some(1), Some(1)),
                (2, Some(1), Some(2), Some(2)),
                (3, Some(2), Some(1), Some(3)),
                (4, None, Some(1), Some(4)),
            ],
        );
        let m = ExecMetrics::default();
        db.try_run_with(
            "SELECT SUM(x) OVER (PARTITION BY g), RANK() OVER (PARTITION BY g ORDER BY o) FROM w",
            &ExecLimits::default(),
            &m,
            &[],
        )
        .unwrap();
        let s = m.snapshot();
        // Two windows (different ORDER BY): 3 partitions each (g = 1, 2, NULL).
        assert_eq!(s.window_partitions_created_total, 6);
        // 2 functions x 4 rows.
        assert_eq!(s.window_functions_evaluated_total, 8);
        db.cleanup();
    }

    // ---- EXPLAIN

    #[test]
    fn explain_shows_the_window() {
        let db = win_db_with("win_explain", &five());
        let plan = crate::plan::explain(&db.plan(
            "SELECT SUM(x) OVER (PARTITION BY g ORDER BY id), ROW_NUMBER() OVER (PARTITION BY g ORDER BY id) FROM w",
        ));
        assert!(plan.contains("Window partition_by=[col("), "{plan}");
        assert!(plan.contains("order_by=[col("), "{plan}");
        assert!(plan.contains("ASC NULLS LAST"), "{plan}");
        assert!(
            plan.contains("frame=RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW (default)"),
            "{plan}"
        );
        assert!(
            plan.contains("sum(") && plan.contains("row_number()"),
            "{plan}"
        );
        db.cleanup();
    }

    #[test]
    fn the_window_is_in_the_physical_plan_not_dropped() {
        // The pre-Slice-D defect ran `SUM(x) OVER ()` as the plain `SUM(x)`;
        // an implementation that accepted OVER and ignored it would plan an
        // Aggregate (or nothing) instead of a Window.
        let db = win_db_with("win_plan_has_window", &five());
        let plan = crate::plan::explain(&db.plan("SELECT SUM(x) OVER () FROM w"));
        assert!(plan.contains("Window "), "{plan}");
        assert!(!plan.contains("Aggregate "), "{plan}");
        db.cleanup();
    }
}

// =======================================================================
// 2B -- explicit frames: ROWS and RANGE, every boundary
// =======================================================================

mod exec_2b {
    use super::*;

    /// ids 1..6, `o` = 1 2 2 5 6 6 (ties at 2 and at 6, gaps between values),
    /// `x` = 1 2 4 8 16 32 (every subset sum is distinct).
    fn frames_db(tag: &str) -> Db {
        win_db_with(
            tag,
            &[
                (1, None, Some(1), Some(1)),
                (2, None, Some(2), Some(2)),
                (3, None, Some(2), Some(4)),
                (4, None, Some(5), Some(8)),
                (5, None, Some(6), Some(16)),
                (6, None, Some(6), Some(32)),
            ],
        )
    }

    fn sums(db: &Db, over: &str) -> Vec<V> {
        col(
            &db.rows(&format!(
                "SELECT id, SUM(x) OVER ({over}) FROM w ORDER BY id"
            )),
            1,
        )
    }

    fn counts(db: &Db, over: &str) -> Vec<V> {
        col(
            &db.rows(&format!(
                "SELECT id, COUNT(*) OVER ({over}) FROM w ORDER BY id"
            )),
            1,
        )
    }

    fn opt(ns: &[Option<i64>]) -> Vec<V> {
        ns.iter().map(|n| n.map(RelationalValue::Bigint)).collect()
    }

    // ---- ROWS

    #[test]
    fn rows_frames_count_individual_rows() {
        let db = frames_db("win_rows_frames");
        let s = |f: &str| sums(&db, &format!("ORDER BY id ROWS {f}"));
        assert_eq!(
            s("BETWEEN 1 PRECEDING AND 1 FOLLOWING"),
            bigs(&[3, 7, 14, 28, 56, 48])
        );
        assert_eq!(
            s("BETWEEN 2 PRECEDING AND CURRENT ROW"),
            bigs(&[1, 3, 7, 14, 28, 56])
        );
        assert_eq!(
            s("BETWEEN CURRENT ROW AND 2 FOLLOWING"),
            bigs(&[7, 14, 28, 56, 48, 32])
        );
        assert_eq!(
            s("BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
            bigs(&[1, 3, 7, 15, 31, 63])
        );
        assert_eq!(
            s("BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING"),
            bigs(&[63, 62, 60, 56, 48, 32])
        );
        assert_eq!(
            s("BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING"),
            bigs(&[63; 6])
        );
        assert_eq!(
            s("BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING"),
            opt(&[None, Some(1), Some(3), Some(7), Some(15), Some(31)])
        );
        assert_eq!(
            s("BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING"),
            opt(&[Some(62), Some(60), Some(56), Some(48), Some(32), None])
        );
        db.cleanup();
    }

    #[test]
    fn the_shorthand_frame_ends_at_the_current_row() {
        let db = frames_db("win_rows_shorthand");
        assert_eq!(
            sums(&db, "ORDER BY id ROWS 2 PRECEDING"),
            sums(&db, "ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW")
        );
        assert_eq!(
            sums(&db, "ORDER BY id ROWS UNBOUNDED PRECEDING"),
            bigs(&[1, 3, 7, 15, 31, 63])
        );
        db.cleanup();
    }

    #[test]
    fn a_zero_width_frame_is_the_current_row_only() {
        let db = frames_db("win_rows_zero");
        assert_eq!(
            sums(&db, "ORDER BY id ROWS BETWEEN 0 PRECEDING AND 0 FOLLOWING"),
            bigs(&[1, 2, 4, 8, 16, 32])
        );
        assert_eq!(
            sums(&db, "ORDER BY id ROWS BETWEEN CURRENT ROW AND CURRENT ROW"),
            bigs(&[1, 2, 4, 8, 16, 32])
        );
        db.cleanup();
    }

    #[test]
    fn a_frame_that_falls_off_the_partition_is_clamped_or_empty() {
        let db = frames_db("win_rows_clamp");
        // Offsets beyond the partition are clamped to it.
        assert_eq!(
            sums(
                &db,
                "ORDER BY id ROWS BETWEEN 100 PRECEDING AND 100 FOLLOWING"
            ),
            bigs(&[63; 6])
        );
        // A frame wholly beyond the partition is empty for the rows it misses:
        // SUM is NULL, COUNT is 0.
        let f = "ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING";
        assert_eq!(
            sums(&db, f),
            opt(&[Some(6), Some(12), Some(24), Some(48), Some(32), None])
        );
        assert_eq!(counts(&db, f), bigs(&[2, 2, 2, 2, 1, 0]));
        let f = "ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING";
        assert_eq!(
            sums(&db, f),
            opt(&[None, Some(1), Some(3), Some(6), Some(12), Some(24)])
        );
        assert_eq!(counts(&db, f), bigs(&[0, 1, 2, 2, 2, 2]));
        db.cleanup();
    }

    #[test]
    fn rows_frames_restart_in_every_partition() {
        let db = win_db_with(
            "win_rows_partition",
            &[
                (1, Some(1), None, Some(1)),
                (2, Some(1), None, Some(2)),
                (3, Some(1), None, Some(4)),
                (4, Some(2), None, Some(8)),
                (5, Some(2), None, Some(16)),
            ],
        );
        assert_eq!(
            sums(
                &db,
                "PARTITION BY g ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW"
            ),
            bigs(&[1, 3, 6, 8, 24])
        );
        db.cleanup();
    }

    #[test]
    fn a_moving_average_uses_the_rows_that_exist_at_the_ends() {
        // The canonical example: a 3-row moving average, shorter at the ends.
        let db = win_db_with("win_moving_avg", &five());
        let rows = db.rows("SELECT id, AVG(x) OVER (ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) FROM w ORDER BY id");
        assert_eq!(
            col(&rows, 1),
            vec![dbl(10.0), dbl(15.0), dbl(20.0), dbl(30.0), dbl(40.0)]
        );
        db.cleanup();
    }

    #[test]
    fn a_sliding_sum_matches_a_hand_computed_reference() {
        // SUM over ROWS BETWEEN 2 PRECEDING AND CURRENT ROW on 200 rows.
        let rows: Vec<Seed> = (1..=200)
            .map(|i| (i, None, Some(i), Some((i * 7) % 13 - 6)))
            .collect();
        let db = win_db_with("win_sliding_reference", &rows);
        let xs: Vec<i64> = rows.iter().map(|r| i64::from(r.3.unwrap())).collect();
        let want: Vec<V> = (0..xs.len())
            .map(|i| big(xs[i.saturating_sub(2)..=i].iter().sum()))
            .collect();
        assert_eq!(
            sums(&db, "ORDER BY id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW"),
            want
        );
        db.cleanup();
    }

    // ---- RANGE

    #[test]
    fn range_frames_count_value_distance_and_include_peers() {
        let db = frames_db("win_range_frames");
        let s = |f: &str| sums(&db, &format!("ORDER BY o RANGE {f}"));
        // o = 1 2 2 5 6 6.
        assert_eq!(
            s("BETWEEN 1 PRECEDING AND 1 FOLLOWING"),
            bigs(&[7, 7, 7, 56, 56, 56])
        );
        assert_eq!(
            s("BETWEEN 1 PRECEDING AND CURRENT ROW"),
            bigs(&[1, 7, 7, 8, 56, 56])
        );
        assert_eq!(
            s("BETWEEN CURRENT ROW AND 1 FOLLOWING"),
            bigs(&[7, 6, 6, 56, 48, 48])
        );
        assert_eq!(
            s("BETWEEN 3 PRECEDING AND 3 FOLLOWING"),
            bigs(&[7, 15, 15, 62, 56, 56])
        );
        assert_eq!(
            s("BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"),
            bigs(&[1, 7, 7, 8 + 7, 63, 63])
        );
        assert_eq!(
            s("BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING"),
            bigs(&[63, 62, 62, 56, 48, 48])
        );
        assert_eq!(
            s("BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING"),
            opt(&[None, Some(1), Some(1), Some(7), Some(15), Some(15)])
        );
        assert_eq!(
            s("BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING"),
            opt(&[Some(62), Some(56), Some(56), Some(48), None, None])
        );
        assert_eq!(
            s("BETWEEN 1 PRECEDING AND 1 PRECEDING"),
            opt(&[None, Some(1), Some(1), None, Some(8), Some(8)])
        );
        db.cleanup();
    }

    #[test]
    fn a_rows_frame_ignores_peers_even_when_the_order_by_has_ties() {
        // ORDER BY o alone has ties (o = 1 2 2 5 6 6). ROWS counts rows: a frame
        // that ends at CURRENT ROW ends at the row itself, not at its last peer,
        // and the rows of a tie are taken in input order.
        let db = frames_db("win_rows_ignores_peers");
        assert_eq!(
            sums(
                &db,
                "ORDER BY o ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"
            ),
            bigs(&[1, 3, 7, 15, 31, 63])
        );
        assert_eq!(
            sums(
                &db,
                "ORDER BY o ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING"
            ),
            bigs(&[63, 62, 60, 56, 48, 32])
        );
        assert_eq!(
            counts(&db, "ORDER BY o ROWS BETWEEN CURRENT ROW AND CURRENT ROW"),
            bigs(&[1; 6])
        );
        // RANGE over the same ORDER BY takes the peers.
        assert_eq!(
            sums(
                &db,
                "ORDER BY o RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"
            ),
            bigs(&[1, 7, 7, 15, 63, 63])
        );
        db.cleanup();
    }

    #[test]
    fn rows_and_range_differ_on_the_same_tie_containing_data() {
        let db = frames_db("win_rows_vs_range");
        let rows = sums(
            &db,
            "ORDER BY o, id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING",
        );
        let range = sums(&db, "ORDER BY o RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING");
        assert_eq!(rows, bigs(&[3, 7, 14, 28, 56, 48]));
        assert_eq!(range, bigs(&[7, 7, 7, 56, 56, 56]));
        assert_ne!(rows, range);
        // With CURRENT ROW as the end bound RANGE takes the peers, ROWS does not.
        let rows = sums(
            &db,
            "ORDER BY o, id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
        );
        let range = sums(
            &db,
            "ORDER BY o RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW",
        );
        assert_eq!(rows, bigs(&[1, 3, 7, 15, 31, 63]));
        assert_eq!(range, bigs(&[1, 7, 7, 15, 63, 63]));
        db.cleanup();
    }

    #[test]
    fn a_descending_range_counts_distance_toward_larger_values_as_preceding() {
        let db = frames_db("win_range_desc");
        let s = |f: &str| sums(&db, &format!("ORDER BY o DESC RANGE {f}"));
        // PRECEDING is toward the start of the (descending) order: larger o.
        assert_eq!(
            s("BETWEEN 1 PRECEDING AND CURRENT ROW"),
            bigs(&[7, 6, 6, 56, 48, 48])
        );
        assert_eq!(
            s("BETWEEN CURRENT ROW AND 1 FOLLOWING"),
            bigs(&[1, 7, 7, 8, 56, 56])
        );
        assert_eq!(
            s("BETWEEN 1 PRECEDING AND 1 FOLLOWING"),
            bigs(&[7, 7, 7, 56, 56, 56])
        );
        db.cleanup();
    }

    #[test]
    fn null_order_keys_are_peers_and_outside_every_value_offset() {
        // Two rows with a NULL key, sorted last (NULLS LAST ascending default).
        let db = win_db_with(
            "win_range_nulls",
            &[
                (1, None, Some(1), Some(1)),
                (2, None, Some(2), Some(2)),
                (3, None, Some(2), Some(4)),
                (4, None, Some(5), Some(8)),
                (5, None, None, Some(16)),
                (6, None, None, Some(32)),
            ],
        );
        // A value offset never reaches a NULL; a NULL row's frame is its peers.
        assert_eq!(
            sums(&db, "ORDER BY o RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING"),
            bigs(&[7, 7, 7, 8, 48, 48])
        );
        // UNBOUNDED reaches across the NULL block: it is last, so it ends the frame.
        assert_eq!(
            sums(
                &db,
                "ORDER BY o RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"
            ),
            bigs(&[1, 7, 7, 15, 63, 63])
        );
        // NULLS FIRST puts them at the front.
        assert_eq!(
            sums(
                &db,
                "ORDER BY o NULLS FIRST RANGE BETWEEN 1 PRECEDING AND 1 FOLLOWING"
            ),
            bigs(&[7, 7, 7, 8, 48, 48])
        );
        assert_eq!(
            sums(
                &db,
                "ORDER BY o NULLS FIRST RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW"
            ),
            bigs(&[49, 55, 55, 63, 48, 48])
        );
        db.cleanup();
    }

    #[test]
    fn range_offsets_work_on_every_numeric_key_type() {
        let db = Db::new("win_range_types");
        db.write("CREATE TABLE k (id INTEGER PRIMARY KEY, b BIGINT, r REAL, dc DECIMAL(10,2), d DOUBLE, v INTEGER)");
        db.write(
            "INSERT INTO k VALUES (1, 10, 1.0, 1.00, 1.0, 1), (2, 11, 1.5, 1.50, 1.5, 2), (3, 20, 3.0, 3.00, 3.0, 4), (4, 21, 3.25, 3.25, 3.25, 8)",
        );
        let want = bigs(&[3, 3, 12, 12]);
        for (key, off) in [("b", "1"), ("r", "0.5"), ("dc", "0.50"), ("d", "0.5")] {
            let rows = db.rows(&format!(
                "SELECT id, SUM(v) OVER (ORDER BY {key} RANGE BETWEEN {off} PRECEDING AND {off} FOLLOWING) FROM k ORDER BY id"
            ));
            // The key distances: b: 10,11 | 20,21 ; r/dc/d: 1.0,1.5 | 3.0,3.25 within 0.5/1 -- the second pair
            // is 0.25 apart, so every key type puts rows 1,2 together and rows 3,4 together.
            assert_eq!(col(&rows, 1), want, "key {key}");
        }
        db.cleanup();
    }

    #[test]
    fn a_range_offset_needs_exactly_one_numeric_order_by() {
        let db = frames_db("win_range_bind");
        for (sql, needle) in [
            ("SELECT SUM(x) OVER (ORDER BY o, id RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w", "exactly one ORDER BY"),
            ("SELECT SUM(x) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w", "exactly one ORDER BY"),
        ] {
            let err = bind_err(&db, sql);
            assert!(matches!(&err, SqlError::TypeMismatch { detail } if detail.contains(needle)), "{sql}: {err:?}");
        }
        let t = Db::new("win_range_bind_text");
        let err = bind_err(
            &t,
            "SELECT SUM(id) OVER (ORDER BY name RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
        );
        assert!(
            matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("numeric")),
            "{err:?}"
        );
        t.cleanup();
        // Without offsets RANGE accepts any number of keys, of any type.
        bound(&db, "SELECT SUM(x) OVER (ORDER BY o, id RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM w");
        db.cleanup();
    }

    #[test]
    fn frames_the_standard_forbids_are_refused() {
        let db = frames_db("win_frame_refused");
        for (frame, needle) in [
            (
                "ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING",
                "must end at N FOLLOWING",
            ),
            (
                "ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW",
                "must end at N FOLLOWING",
            ),
            (
                "ROWS BETWEEN CURRENT ROW AND 1 PRECEDING",
                "cannot end at N PRECEDING",
            ),
            (
                "ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW",
                "cannot start at UNBOUNDED FOLLOWING",
            ),
            (
                "ROWS BETWEEN CURRENT ROW AND UNBOUNDED PRECEDING",
                "cannot end at UNBOUNDED PRECEDING",
            ),
            (
                "ROWS BETWEEN -1 PRECEDING AND CURRENT ROW",
                "must not be negative",
            ),
            ("ROWS BETWEEN id PRECEDING AND CURRENT ROW", "constant"),
            ("ROWS BETWEEN 1.5 PRECEDING AND CURRENT ROW", ""),
        ] {
            let sql = format!("SELECT SUM(x) OVER (ORDER BY id {frame}) FROM w");
            let err = bind_err(&db, &sql);
            assert!(
                matches!(&err, SqlError::TypeMismatch { detail } if detail.contains(needle)),
                "{sql}: {err:?}"
            );
        }
        db.cleanup();
    }

    #[test]
    fn offsets_may_be_parameters_and_are_checked_when_they_are_known() {
        let db = frames_db("win_frame_params");
        let r = db
            .try_run_with(
                "SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN $1 PRECEDING AND CURRENT ROW) FROM w ORDER BY id",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[big(1)],
            )
            .unwrap();
        assert_eq!(col(&r.rows, 1), bigs(&[1, 3, 6, 12, 24, 48]));
        let err = db
            .try_run_with(
                "SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN $1 PRECEDING AND CURRENT ROW) FROM w",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[big(-1)],
            )
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn frame_aggregates_use_the_same_state_machine_for_min_max_avg_count() {
        let db = frames_db("win_frame_aggs");
        let rows = db.rows(
            "SELECT id, MIN(x) OVER w, MAX(x) OVER w, COUNT(x) OVER w, AVG(x) OVER w \
             FROM w WINDOW w AS (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) ORDER BY id",
        );
        assert_eq!(
            col(&rows, 1),
            vec![int(1), int(1), int(2), int(4), int(8), int(16)]
        );
        assert_eq!(
            col(&rows, 2),
            vec![int(2), int(4), int(8), int(16), int(32), int(32)]
        );
        assert_eq!(col(&rows, 3), bigs(&[2, 3, 3, 3, 3, 2]));
        assert_eq!(col(&rows, 4)[1], dbl(7.0 / 3.0));
        db.cleanup();
    }

    #[test]
    fn first_value_sees_the_whole_partition_only_under_an_explicit_frame() {
        // Hazard 6: the same function with the same argument answers differently
        // under different frames.
        let db = frames_db("win_frame_first_value");
        let default = col(
            &db.rows("SELECT id, FIRST_VALUE(x) OVER (ORDER BY id DESC) FROM w ORDER BY id"),
            1,
        );
        let whole = col(
            &db.rows("SELECT id, FIRST_VALUE(x) OVER (ORDER BY id DESC ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM w ORDER BY id"),
            1,
        );
        let sliding = col(
            &db.rows("SELECT id, FIRST_VALUE(x) OVER (ORDER BY id DESC ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING) FROM w ORDER BY id"),
            1,
        );
        // Descending by id the first row is id 6 (x = 32).
        assert_eq!(default, vec![int(32); 6]);
        assert_eq!(whole, vec![int(32); 6]);
        // Starting one row after the current one (the next-lower id): its x, and
        // NULL for id 1, whose frame is empty.
        assert_eq!(sliding, vec![NULL, int(1), int(2), int(4), int(8), int(16)]);
        db.cleanup();
    }
}

// =======================================================================
// 2C -- navigation and distribution functions
// =======================================================================

mod exec_2c {
    use super::*;

    fn one(db: &Db, expr: &str) -> Vec<V> {
        col(
            &db.rows(&format!("SELECT id, {expr} FROM w ORDER BY id")),
            1,
        )
    }

    // ---- LAG / LEAD

    #[test]
    fn previous_value_is_null_for_the_first_row() {
        // The canonical example: LAG(x) OVER (ORDER BY id).
        let db = win_db_with("win_lag_basic", &five());
        assert_eq!(
            one(&db, "LAG(x) OVER (ORDER BY id)"),
            vec![NULL, int(10), int(20), int(30), int(40)]
        );
        assert_eq!(
            one(&db, "LEAD(x) OVER (ORDER BY id)"),
            vec![int(20), int(30), int(40), int(50), NULL]
        );
        db.cleanup();
    }

    #[test]
    fn lag_and_lead_take_an_offset_and_a_default() {
        let db = win_db_with("win_lag_offset", &five());
        assert_eq!(
            one(&db, "LAG(x, 2) OVER (ORDER BY id)"),
            vec![NULL, NULL, int(10), int(20), int(30)]
        );
        assert_eq!(
            one(&db, "LEAD(x, 2, 0) OVER (ORDER BY id)"),
            vec![int(30), int(40), int(50), int(0), int(0)]
        );
        assert_eq!(
            one(&db, "LAG(x, 1, -1) OVER (ORDER BY id)"),
            vec![int(-1), int(10), int(20), int(30), int(40)]
        );
        // Offset 0 is the current row.
        assert_eq!(
            one(&db, "LAG(x, 0) OVER (ORDER BY id)"),
            vec![int(10), int(20), int(30), int(40), int(50)]
        );
        // An offset beyond the partition: always the default.
        assert_eq!(
            one(&db, "LEAD(x, 100, 7) OVER (ORDER BY id)"),
            vec![int(7); 5]
        );
        // The default is evaluated for the current row.
        assert_eq!(
            one(&db, "LAG(x, 1, id) OVER (ORDER BY id)"),
            vec![int(1), int(10), int(20), int(30), int(40)]
        );
        db.cleanup();
    }

    #[test]
    fn lag_does_not_skip_nulls_and_restarts_per_partition() {
        let db = win_db_with(
            "win_lag_partition",
            &[
                (1, Some(1), None, Some(10)),
                (2, Some(1), None, None),
                (3, Some(1), None, Some(30)),
                (4, Some(2), None, Some(40)),
                (5, Some(2), None, Some(50)),
            ],
        );
        assert_eq!(
            one(&db, "LAG(x) OVER (PARTITION BY g ORDER BY id)"),
            vec![NULL, int(10), NULL, NULL, int(40)]
        );
        db.cleanup();
    }

    #[test]
    fn lag_follows_the_window_order_not_the_input_order() {
        let db = win_db_with("win_lag_order", &five());
        assert_eq!(
            one(&db, "LAG(x) OVER (ORDER BY id DESC)"),
            vec![int(20), int(30), int(40), int(50), NULL]
        );
        db.cleanup();
    }

    #[test]
    fn lag_and_lead_need_an_order_by_and_take_no_frame() {
        let db = win_db_with("win_lag_bind", &five());
        let err = bind_err(&db, "SELECT LAG(x) OVER () FROM w");
        assert!(
            matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("ORDER BY")),
            "{err:?}"
        );
        let err = bind_err(
            &db,
            "SELECT LAG(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
        );
        assert!(
            matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("does not take a window frame")),
            "{err:?}"
        );
        // The default must have the value's type; the offset must be a constant.
        let err = bind_err(&db, "SELECT LAG(x, 1, 'a') OVER (ORDER BY id) FROM w");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = bind_err(&db, "SELECT LAG(x, id) OVER (ORDER BY id) FROM w");
        assert!(
            matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("constant")),
            "{err:?}"
        );
        let err = bind_err(&db, "SELECT LAG(x, -1) OVER (ORDER BY id) FROM w");
        assert!(
            matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("negative")),
            "{err:?}"
        );
        db.cleanup();
    }

    // ---- FIRST_VALUE / LAST_VALUE / NTH_VALUE

    #[test]
    fn first_value_is_the_frames_first_row() {
        let db = win_db_with("win_first_value", &five());
        assert_eq!(
            one(&db, "FIRST_VALUE(x) OVER (ORDER BY id)"),
            vec![int(10); 5]
        );
        assert_eq!(
            one(
                &db,
                "FIRST_VALUE(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)"
            ),
            vec![int(10), int(10), int(20), int(30), int(40)]
        );
        db.cleanup();
    }

    /// LAST_VALUE with the default frame returns the current row's value, not the
    /// partition's last: the default frame ends at the current row.
    #[test]
    fn last_value_with_the_default_frame_is_the_current_row_not_the_last_row() {
        let db = win_db_with("win_last_value_default", &five());
        assert_eq!(
            one(&db, "LAST_VALUE(x) OVER (ORDER BY id)"),
            vec![int(10), int(20), int(30), int(40), int(50)]
        );
        // The partition's last row needs an explicit frame.
        assert_eq!(
            one(&db, "LAST_VALUE(x) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"),
            vec![int(50); 5]
        );
        // ... and without an ORDER BY the default frame is the whole partition.
        assert_eq!(one(&db, "LAST_VALUE(x) OVER ()"), vec![int(50); 5]);
        db.cleanup();
    }

    #[test]
    fn last_value_with_the_default_range_frame_is_the_last_peer() {
        // o: 10, 20, 20, 30 -- under RANGE the two o=20 rows are peers, so the
        // frame of each ends at the second of them.
        let db = win_db_with(
            "win_last_value_peers",
            &[
                (1, None, Some(10), Some(1)),
                (2, None, Some(20), Some(2)),
                (3, None, Some(20), Some(3)),
                (4, None, Some(30), Some(4)),
            ],
        );
        assert_eq!(
            one(&db, "LAST_VALUE(x) OVER (ORDER BY o)"),
            vec![int(1), int(3), int(3), int(4)]
        );
        // Under ROWS the frame ends at the row itself.
        assert_eq!(
            one(&db, "LAST_VALUE(x) OVER (ORDER BY o, id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)"),
            vec![int(1), int(2), int(3), int(4)]
        );
        db.cleanup();
    }

    #[test]
    fn nth_value_counts_within_the_frame() {
        let db = win_db_with("win_nth_value", &five());
        assert_eq!(
            one(&db, "NTH_VALUE(x, 2) OVER (ORDER BY id)"),
            vec![NULL, int(20), int(20), int(20), int(20)]
        );
        assert_eq!(
            one(&db, "NTH_VALUE(x, 2) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"),
            vec![int(20); 5]
        );
        // n beyond the frame / partition: NULL.
        assert_eq!(
            one(&db, "NTH_VALUE(x, 10) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)"),
            vec![NULL; 5]
        );
        assert_eq!(
            one(
                &db,
                "NTH_VALUE(x, 1) OVER (ORDER BY id ROWS BETWEEN 1 FOLLOWING AND 2 FOLLOWING)"
            ),
            vec![int(20), int(30), int(40), int(50), NULL]
        );
        db.cleanup();
    }

    #[test]
    fn nth_value_position_must_be_a_positive_constant() {
        let db = win_db_with("win_nth_bind", &five());
        let err = bind_err(&db, "SELECT NTH_VALUE(x, 0) OVER (ORDER BY id) FROM w");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = bind_err(&db, "SELECT NTH_VALUE(x, id) OVER (ORDER BY id) FROM w");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = db
            .try_run_with(
                "SELECT NTH_VALUE(x, $1) OVER (ORDER BY id) FROM w",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[big(0)],
            )
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn value_functions_work_on_text_and_return_the_arguments_type() {
        let db = Db::new("win_value_text");
        db.seed_t(&[(1, Some("a"), None), (2, Some("b"), None), (3, None, None)]);
        let rows = db.rows(
            "SELECT id, FIRST_VALUE(name) OVER (ORDER BY id), LAG(name) OVER (ORDER BY id), LAST_VALUE(name) OVER (ORDER BY id) FROM t ORDER BY id",
        );
        assert_eq!(col(&rows, 1), vec![text("a"); 3]);
        assert_eq!(col(&rows, 2), vec![NULL, text("a"), text("b")]);
        assert_eq!(col(&rows, 3), vec![text("a"), text("b"), NULL]);
        db.cleanup();
    }

    // ---- NTILE

    #[test]
    fn ntile_splits_the_partition_into_buckets() {
        let db = win_db_with("win_ntile", &five());
        assert_eq!(
            one(&db, "NTILE(1) OVER (ORDER BY id)"),
            bigs(&[1, 1, 1, 1, 1])
        );
        // 5 rows in 2 buckets: the first bucket takes the extra row.
        assert_eq!(
            one(&db, "NTILE(2) OVER (ORDER BY id)"),
            bigs(&[1, 1, 1, 2, 2])
        );
        assert_eq!(
            one(&db, "NTILE(3) OVER (ORDER BY id)"),
            bigs(&[1, 1, 2, 2, 3])
        );
        assert_eq!(
            one(&db, "NTILE(5) OVER (ORDER BY id)"),
            bigs(&[1, 2, 3, 4, 5])
        );
        // More buckets than rows: one row per bucket, the last buckets stay empty.
        assert_eq!(
            one(&db, "NTILE(10) OVER (ORDER BY id)"),
            bigs(&[1, 2, 3, 4, 5])
        );
        db.cleanup();
    }

    #[test]
    fn ntile_works_per_partition_and_follows_the_order() {
        let db = win_db_with(
            "win_ntile_partition",
            &[
                (1, Some(1), None, Some(5)),
                (2, Some(1), None, Some(4)),
                (3, Some(1), None, Some(3)),
                (4, Some(2), None, Some(2)),
                (5, Some(2), None, Some(1)),
            ],
        );
        assert_eq!(
            one(&db, "NTILE(2) OVER (PARTITION BY g ORDER BY x)"),
            bigs(&[2, 1, 1, 2, 1])
        );
        db.cleanup();
    }

    #[test]
    fn ntile_needs_a_positive_integer_constant() {
        let db = win_db_with("win_ntile_bind", &five());
        for sql in [
            "SELECT NTILE(x) OVER (ORDER BY id) FROM w",
            "SELECT NTILE(-1) OVER (ORDER BY id) FROM w",
            "SELECT NTILE('a') OVER (ORDER BY id) FROM w",
            "SELECT NTILE() OVER (ORDER BY id) FROM w",
        ] {
            let err = bind_err(&db, sql);
            assert!(
                matches!(err, SqlError::TypeMismatch { .. }),
                "{sql}: {err:?}"
            );
        }
        let err = db
            .try_run("SELECT NTILE(0) OVER (ORDER BY id) FROM w")
            .unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "{err:?}"
        );
        db.cleanup();
    }

    // ---- PERCENT_RANK / CUME_DIST

    fn tied_db(tag: &str) -> Db {
        win_db_with(
            tag,
            &[
                (1, None, Some(10), Some(1)),
                (2, None, Some(20), Some(2)),
                (3, None, Some(20), Some(3)),
                (4, None, Some(30), Some(4)),
            ],
        )
    }

    #[test]
    fn percent_rank_and_cume_dist_on_tied_data() {
        let db = tied_db("win_percent_cume");
        // rank = 1 2 2 4 over 4 rows: (rank - 1) / (rows - 1).
        assert_eq!(
            one(&db, "PERCENT_RANK() OVER (ORDER BY o)"),
            vec![dbl(0.0), dbl(1.0 / 3.0), dbl(1.0 / 3.0), dbl(1.0)]
        );
        // rows with a value <= the current one (peers included) / rows.
        assert_eq!(
            one(&db, "CUME_DIST() OVER (ORDER BY o)"),
            vec![dbl(0.25), dbl(0.75), dbl(0.75), dbl(1.0)]
        );
        db.cleanup();
    }

    #[test]
    fn percent_rank_of_a_single_row_partition_is_zero() {
        let db = win_db_with(
            "win_percent_single",
            &[
                (1, Some(1), Some(1), Some(1)),
                (2, Some(2), Some(1), Some(2)),
                (3, Some(2), Some(2), Some(3)),
            ],
        );
        assert_eq!(
            one(&db, "PERCENT_RANK() OVER (PARTITION BY g ORDER BY o)"),
            vec![dbl(0.0), dbl(0.0), dbl(1.0)]
        );
        assert_eq!(
            one(&db, "CUME_DIST() OVER (PARTITION BY g ORDER BY o)"),
            vec![dbl(1.0), dbl(0.5), dbl(1.0)]
        );
        db.cleanup();
    }

    #[test]
    fn percent_rank_uses_the_rank_not_the_row_number() {
        // With ties, (rank - 1) and (row_number - 1) differ.
        let db = tied_db("win_percent_rank_vs_rn");
        let pr = one(&db, "PERCENT_RANK() OVER (ORDER BY o)");
        assert_eq!(pr[2], dbl(1.0 / 3.0));
        assert_ne!(pr[2], dbl(2.0 / 3.0));
        db.cleanup();
    }

    // ---- the remaining canonical examples

    #[test]
    fn percent_of_group_total() {
        // x / SUM(x) OVER (PARTITION BY g) * 100, on a DOUBLE column (the
        // integer / integer division would truncate).
        let db = win_db_with(
            "win_percent_of_group",
            &[
                (1, Some(1), None, Some(10)),
                (2, Some(1), None, Some(30)),
                (3, Some(2), None, Some(50)),
            ],
        );
        let rows =
            db.rows("SELECT g, d, d / SUM(d) OVER (PARTITION BY g) * 100.0 FROM w ORDER BY id");
        assert_eq!(col(&rows, 2), vec![dbl(25.0), dbl(75.0), dbl(100.0)]);
        db.cleanup();
    }

    #[test]
    fn several_functions_of_different_kinds_in_one_select() {
        let db = tied_db("win_mixed_functions");
        let rows = db.rows(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY o), RANK() OVER (ORDER BY o), LAG(x) OVER (ORDER BY o, id), \
             SUM(x) OVER (ORDER BY o, id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), NTILE(2) OVER (ORDER BY o, id) FROM w ORDER BY id",
        );
        assert_eq!(col(&rows, 1), bigs(&[1, 2, 3, 4]));
        assert_eq!(col(&rows, 2), bigs(&[1, 2, 2, 4]));
        assert_eq!(col(&rows, 3), vec![NULL, int(1), int(2), int(3)]);
        assert_eq!(col(&rows, 4), bigs(&[1, 3, 5, 7]));
        assert_eq!(col(&rows, 5), bigs(&[1, 1, 2, 2]));
        db.cleanup();
    }
}

// =======================================================================
// 2D -- QUALIFY
// =======================================================================

mod exec_2d {
    use super::*;

    /// g=1: x 5, 9, 9 ; g=2: x 1, 2.
    fn q_db(tag: &str) -> Db {
        win_db_with(
            tag,
            &[
                (1, Some(1), None, Some(5)),
                (2, Some(1), None, Some(9)),
                (3, Some(1), None, Some(9)),
                (4, Some(2), None, Some(1)),
                (5, Some(2), None, Some(2)),
            ],
        )
    }

    fn ids(db: &Db, sql: &str) -> Vec<V> {
        col(&db.rows(sql), 0)
    }

    #[test]
    fn qualify_keeps_the_rows_a_window_predicate_selects() {
        let db = q_db("win_qualify_basic");
        // The top row per group.
        assert_eq!(
            ids(&db, "SELECT id FROM w QUALIFY ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC, id) = 1 ORDER BY id"),
            vec![int(2), int(5)]
        );
        // Ties kept: RANK, not ROW_NUMBER.
        assert_eq!(
            ids(&db, "SELECT id FROM w QUALIFY RANK() OVER (PARTITION BY g ORDER BY x DESC) = 1 ORDER BY id"),
            vec![int(2), int(3), int(5)]
        );
        db.cleanup();
    }

    #[test]
    fn qualify_may_use_a_select_list_alias() {
        let db = q_db("win_qualify_alias");
        let rows = db.rows(
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY g ORDER BY x DESC, id) AS rn FROM w QUALIFY rn <= 2 ORDER BY id",
        );
        assert_eq!(col(&rows, 0), vec![int(2), int(3), int(4), int(5)]);
        db.cleanup();
    }

    #[test]
    fn qualify_runs_after_the_window_functions_and_before_order_by_and_limit() {
        let db = q_db("win_qualify_order");
        // Window functions see the rows WHERE kept, QUALIFY then filters their results ...
        assert_eq!(
            ids(
                &db,
                "SELECT id FROM w WHERE id > 1 QUALIFY ROW_NUMBER() OVER (ORDER BY id) = 1"
            ),
            vec![int(2)]
        );
        // ... LIMIT counts the rows QUALIFY kept (if LIMIT ran first, the first
        // two rows would be cut before QUALIFY looked at them).
        assert_eq!(
            ids(
                &db,
                "SELECT id FROM w QUALIFY ROW_NUMBER() OVER (ORDER BY id) > 2 ORDER BY id LIMIT 2"
            ),
            vec![int(3), int(4)]
        );
        // ... and ORDER BY orders the survivors.
        assert_eq!(
            ids(
                &db,
                "SELECT id FROM w QUALIFY ROW_NUMBER() OVER (ORDER BY id) > 2 ORDER BY id DESC"
            ),
            vec![int(5), int(4), int(3)]
        );
        db.cleanup();
    }

    #[test]
    fn qualify_is_neither_where_nor_having() {
        let db = q_db("win_qualify_not_where");
        // WHERE filters before the window: the ranking restarts on what is left.
        let where_ = ids(&db, "SELECT id FROM w WHERE x < 9 ORDER BY id");
        assert_eq!(where_, vec![int(1), int(4), int(5)]);
        let qualify = ids(
            &db,
            "SELECT id FROM w QUALIFY MAX(x) OVER (PARTITION BY g) < 9 ORDER BY id",
        );
        assert_eq!(qualify, vec![int(4), int(5)]);
        // A window function is an error in WHERE (the point of QUALIFY).
        let err = db
            .try_run("SELECT id FROM w WHERE MAX(x) OVER (PARTITION BY g) < 9")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn qualify_without_any_window_function_is_refused() {
        let db = q_db("win_qualify_refused");
        let err = bind_err(&db, "SELECT id FROM w QUALIFY id > 2");
        assert!(
            matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("requires a window function")),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn qualify_may_filter_on_a_window_in_the_select_list_through_an_alias_or_expression() {
        let db = q_db("win_qualify_select_window");
        assert_eq!(
            ids(
                &db,
                "SELECT id, SUM(x) OVER (PARTITION BY g) AS s FROM w QUALIFY s > 10 ORDER BY id"
            ),
            vec![int(1), int(2), int(3)]
        );
        db.cleanup();
    }

    #[test]
    fn qualify_on_an_aggregated_query() {
        let db = q_db("win_qualify_aggregated");
        let rows = db.rows(
            "SELECT g, SUM(x) FROM w GROUP BY g QUALIFY RANK() OVER (ORDER BY SUM(x) DESC) = 1",
        );
        assert_eq!(rows, vec![vec![int(1), big(23)]]);
        db.cleanup();
    }

    #[test]
    fn qualify_composes_with_distinct_and_subqueries() {
        let db = q_db("win_qualify_compose");
        let rows = db.rows("SELECT DISTINCT g FROM w QUALIFY ROW_NUMBER() OVER (PARTITION BY g ORDER BY id) = 1 ORDER BY g");
        assert_eq!(col(&rows, 0), vec![int(1), int(2)]);
        let rows = db.rows(
            "SELECT id FROM w QUALIFY ROW_NUMBER() OVER (ORDER BY id) IN (SELECT COUNT(*) FROM w WHERE x > 5) ORDER BY id",
        );
        // Two rows have x > 5, so the subquery is {2}: the row numbered 2.
        assert_eq!(col(&rows, 0), vec![int(2)]);
        db.cleanup();
    }
}

// =======================================================================
// composition: named windows, subqueries, DML, recursion, types, cancellation
// =======================================================================

mod composition {
    use super::*;

    fn ids_x(db: &Db, sql: &str) -> Vec<V> {
        col(&db.rows(sql), 1)
    }

    #[test]
    fn a_named_window_is_shared_by_several_functions_and_extended_by_others() {
        let db = win_db_with("win_named", &five());
        let rows = db.rows(
            "SELECT id, SUM(x) OVER w, COUNT(*) OVER w, SUM(x) OVER (w ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
             FROM w WINDOW w AS (ORDER BY id) ORDER BY id",
        );
        assert_eq!(col(&rows, 1), bigs(&[10, 30, 60, 100, 150]));
        assert_eq!(col(&rows, 2), bigs(&[1, 2, 3, 4, 5]));
        assert_eq!(col(&rows, 3), bigs(&[10, 30, 50, 70, 90]));
        // The named and the spelled-out window are the same window: one operator.
        let plan = crate::plan::explain(&db.plan(
            "SELECT SUM(x) OVER w, COUNT(*) OVER (ORDER BY id) FROM w WINDOW w AS (ORDER BY id)",
        ));
        assert_eq!(plan.matches("Window ").count(), 1, "{plan}");
        // A window defined from another.
        let rows = db.rows(
            "SELECT id, SUM(x) OVER w2 FROM w WINDOW w1 AS (PARTITION BY g), w2 AS (w1 ORDER BY id) ORDER BY id",
        );
        assert_eq!(col(&rows, 1), bigs(&[10, 30, 60, 100, 150]));
        db.cleanup();
    }

    #[test]
    fn named_window_misuse_is_a_typed_error() {
        let db = win_db_with("win_named_bad", &five());
        for (sql, needle) in [
            ("SELECT SUM(x) OVER nope FROM w", "not defined"),
            ("SELECT SUM(x) OVER w FROM w WINDOW w AS (ORDER BY id), w AS (ORDER BY o)", "more than once"),
            ("SELECT SUM(x) OVER (w PARTITION BY g) FROM w WINDOW w AS (ORDER BY id)", "PARTITION BY"),
            ("SELECT SUM(x) OVER (w ORDER BY o) FROM w WINDOW w AS (ORDER BY id)", "ORDER BY"),
            ("SELECT SUM(x) OVER (w ROWS 1 PRECEDING) FROM w WINDOW w AS (ORDER BY id ROWS 2 PRECEDING)", "frame"),
            ("SELECT SUM(x) OVER a FROM w WINDOW a AS b, b AS a", "cycle"),
        ] {
            let err = bind_err(&db, sql);
            let text = format!("{err:?}");
            assert!(text.contains(needle), "{sql}: {text}");
        }
        db.cleanup();
    }

    #[test]
    fn scalar_expressions_and_functions_inside_window_calls() {
        let db = win_db_with("win_exprs", &five());
        // The argument is an expression; PARTITION BY / ORDER BY take expressions too.
        assert_eq!(
            ids_x(
                &db,
                "SELECT id, SUM(x + 1) OVER (PARTITION BY id % 2 ORDER BY id) FROM w ORDER BY id"
            ),
            bigs(&[11, 21, 42, 62, 93])
        );
        // The result is usable in an expression and as an argument of a function.
        assert_eq!(
            ids_x(
                &db,
                "SELECT id, abs(0 - SUM(x) OVER (ORDER BY id)) FROM w ORDER BY id"
            ),
            bigs(&[10, 30, 60, 100, 150])
        );
        db.cleanup();
    }

    #[test]
    fn a_window_in_a_correlated_subquery_reads_the_outer_row() {
        let db = win_db_with(
            "win_correlated",
            &[
                (1, Some(1), None, Some(10)),
                (2, Some(1), None, Some(20)),
                (3, Some(2), None, Some(30)),
            ],
        );
        // PARTITION BY an outer column is one constant per outer row; the inner
        // relation is the single row z.id = o.id.
        let rows = db.rows(
            "SELECT o.id, (SELECT SUM(z.x) OVER (PARTITION BY o.g) FROM w z WHERE z.id = o.id) FROM w o ORDER BY o.id",
        );
        assert_eq!(col(&rows, 1), bigs(&[10, 20, 30]));
        // The window runs over the inner rows of the outer row's group.
        let rows = db.rows(
            "SELECT o.id, (SELECT COUNT(*) OVER () FROM w z WHERE z.g = o.g AND z.id <= o.id ORDER BY z.id DESC LIMIT 1) FROM w o ORDER BY o.id",
        );
        assert_eq!(col(&rows, 1), bigs(&[1, 2, 1]));
        db.cleanup();
    }

    #[test]
    fn window_functions_drive_update_and_delete_through_a_subquery() {
        let db = win_db_with("win_dml", &five());
        // Delete every row after the third, found with ROW_NUMBER().
        db.write("DELETE FROM w WHERE id IN (SELECT id FROM (SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM w) q WHERE rn > 3)");
        assert_eq!(
            col(&db.rows("SELECT id FROM w ORDER BY id"), 0),
            vec![int(1), int(2), int(3)]
        );
        // Set every remaining x to the smallest one, computed with a window.
        db.write("UPDATE w SET x = (SELECT q.m FROM (SELECT id, MIN(x) OVER () AS m FROM w) q WHERE q.id = w.id)");
        assert_eq!(
            col(&db.rows("SELECT x FROM w ORDER BY id"), 0),
            vec![int(10), int(10), int(10)]
        );
        db.cleanup();
    }

    #[test]
    fn a_window_in_the_seed_of_a_recursive_cte_is_fine_in_the_term_it_is_refused() {
        let db = win_db_with("win_recursive", &five());
        let rows = db.rows(
            "WITH RECURSIVE r(n, s) AS (SELECT 1, SUM(x) OVER () FROM w WHERE id = 1 UNION ALL SELECT n + 1, s FROM r WHERE n < 3) SELECT n, s FROM r ORDER BY n",
        );
        assert_eq!(rows.len(), 3);
        let err = db
            .try_bind("WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT ROW_NUMBER() OVER (ORDER BY n) FROM r WHERE n < 3) SELECT n FROM r")
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::InvalidIdentifier { detail } if detail.contains("window functions")),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn window_aggregates_work_over_every_type() {
        let db = crate::setop_tests::typed_db("win_types");
        db.write(
            "INSERT INTO nums (id, i, b, r, d, dec2, txt) VALUES (1, 1, 10, 1.5, 1.25, 1.10, 'b'), (2, 2, 20, 2.5, 2.25, 2.20, 'a'), (3, NULL, NULL, NULL, NULL, NULL, NULL)",
        );
        let rows = db.rows(
            "SELECT id, SUM(i) OVER (), SUM(b) OVER (), SUM(r) OVER (), SUM(d) OVER (), SUM(dec2) OVER (), MIN(txt) OVER (), MAX(txt) OVER (), \
             MIN(dec2) OVER (ORDER BY id), AVG(r) OVER (), COUNT(dec2) OVER () FROM nums ORDER BY id",
        );
        let first = &rows[0];
        assert_eq!(first[1], big(3));
        assert_eq!(first[2], big(30));
        assert_eq!(first[3], Some(RelationalValue::Real(4.0)));
        assert_eq!(first[4], dbl(3.5));
        assert_eq!(first[5], Some(RelationalValue::Decimal(330, 2)));
        assert_eq!(first[6], text("a"));
        assert_eq!(first[7], text("b"));
        assert_eq!(first[8], Some(RelationalValue::Decimal(110, 2)));
        assert_eq!(first[9], Some(RelationalValue::Real(2.0)));
        assert_eq!(first[10], big(2));
        assert_eq!(rows[2][8], Some(RelationalValue::Decimal(110, 2)));
        db.cleanup();
    }

    #[test]
    fn an_outer_join_null_extension_flows_through_a_window() {
        let db = Db::new("win_left_join");
        db.seed_t(&[
            (1, Some("a"), None),
            (2, Some("b"), None),
            (3, Some("c"), None),
        ]);
        db.seed_orders(&[(10, "x", Some(5), Some(1)), (11, "y", Some(7), Some(1))]);
        let b = bound(&db, "SELECT t.id, SUM(o.amount) OVER (PARTITION BY t.id) FROM t LEFT JOIN orders o ON o.t_id = t.id");
        assert!(b.projection[1].expr.nullable);
        let rows = db.rows(
            "SELECT t.id, SUM(o.amount) OVER (PARTITION BY t.id), COUNT(o.id) OVER (PARTITION BY t.id) FROM t LEFT JOIN orders o ON o.t_id = t.id ORDER BY t.id, o.id",
        );
        assert_eq!(col(&rows, 1), vec![big(12), big(12), NULL, NULL]);
        assert_eq!(col(&rows, 2), bigs(&[2, 2, 0, 0]));
        db.cleanup();
    }

    #[test]
    fn a_cancelled_statement_stops_a_window_operator() {
        use crate::exec::{execute_autocommit, CancellationToken};
        let db = win_db_with("win_cancel", &five());
        let plan = db.plan("SELECT SUM(x) OVER (ORDER BY id) FROM w");
        let token = CancellationToken::new();
        token.cancel();
        let err = execute_autocommit(
            &plan,
            &db.txm,
            &db.store,
            &db.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &token,
        )
        .unwrap_err();
        assert!(matches!(err, SqlError::Cancelled), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_large_partition_is_correct_and_the_running_frame_is_linear() {
        // 3000 rows in one partition: a running total, a ranking and a sliding frame.
        let rows: Vec<Seed> = (1..=3000)
            .map(|i| (i, None, Some(i / 3), Some(i % 10)))
            .collect();
        let db = win_db_with("win_large", &rows);
        let r = db.rows(
            "SELECT id, SUM(x) OVER (ORDER BY id), RANK() OVER (ORDER BY o), SUM(x) OVER (ORDER BY id ROWS BETWEEN 20 PRECEDING AND CURRENT ROW) FROM w ORDER BY id",
        );
        let mut running = 0i64;
        for (i, row) in r.iter().enumerate() {
            running += (i as i64 + 1) % 10;
            assert_eq!(row[1], big(running), "row {i}");
            let o = (i as i64 + 1) / 3;
            let rank = (0..=i).find(|&j| (j as i64 + 1) / 3 == o).unwrap() as i64 + 1;
            assert_eq!(row[2], big(rank), "rank row {i}");
            let lo = i.saturating_sub(20);
            let want: i64 = (lo..=i).map(|j| (j as i64 + 1) % 10).sum();
            assert_eq!(row[3], big(want), "sliding row {i}");
        }
        db.cleanup();
    }
}
