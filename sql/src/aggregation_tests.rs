//! SQL-mission-2, Family B -- advanced aggregation.
//!
//! 2E: `GROUPING SETS`, `ROLLUP`, `CUBE`, `GROUPING()`.
//! 2F: aggregate `FILTER (WHERE ...)` and `DISTINCT` aggregates.
//!
//! Every execution test runs through the real parse, bind, plan and execute
//! pipeline against a real engine; the HTTP tests are in
//! `api/tests/api_sql_mission2.rs`.

use crate::mission1_support::*;

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::error::SqlError;
use crate::exec::{ExecLimits, ExecMetrics};
use crate::limits::SqlLimits;
use crate::parse::parse_statement;
use crate::setop_tests::{bind_err, bound};
use crate::window_tests::{col, dbl};

/// `ag(id INTEGER PK, a INTEGER, b INTEGER, x INTEGER, y INTEGER, d DOUBLE)`.
pub(crate) fn ag_db(tag: &str) -> Db {
    let db = Db::new(tag);
    db.write("CREATE TABLE ag (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER, x INTEGER, y INTEGER, d DOUBLE)");
    db
}

type AgRow = (i32, Option<i32>, Option<i32>, Option<i32>, Option<i32>);

fn lit(v: Option<i32>) -> String {
    v.map_or("NULL".to_string(), |n| n.to_string())
}

pub(crate) fn seed_ag(db: &Db, rows: &[AgRow]) {
    if rows.is_empty() {
        return;
    }
    let values: Vec<String> = rows
        .iter()
        .map(|(id, a, b, x, y)| {
            format!(
                "({id}, {}, {}, {}, {}, {})",
                lit(*a),
                lit(*b),
                lit(*x),
                lit(*y),
                x.map_or("NULL".to_string(), |n| format!("{n}.0"))
            )
        })
        .collect();
    db.write(&format!("INSERT INTO ag VALUES {}", values.join(", ")));
}

/// The grouping fixture: `a` has a NULL in the data, so a NULL that is real
/// can be told from one that stands for "aggregated away".
///
/// | id | a | b | x |
/// |---|---|---|---|
/// | 1 | 1 | 1 | 1 |
/// | 2 | 1 | 2 | 2 |
/// | 3 | 2 | 1 | 4 |
/// | 4 | 2 | 1 | 8 |
/// | 5 | NULL | 1 | 16 |
fn grouped(tag: &str) -> Db {
    let db = ag_db(tag);
    seed_ag(
        &db,
        &[
            (1, Some(1), Some(1), Some(1), None),
            (2, Some(1), Some(2), Some(2), None),
            (3, Some(2), Some(1), Some(4), None),
            (4, Some(2), Some(1), Some(8), None),
            (5, None, Some(1), Some(16), None),
        ],
    );
    db
}

fn r(vals: &[V]) -> Row {
    vals.to_vec()
}

fn ib(a: Option<i32>, b: Option<i32>, sum: Option<i64>) -> Row {
    r(&[
        a.map(RelationalValue::Integer),
        b.map(RelationalValue::Integer),
        sum.map(RelationalValue::Bigint),
    ])
}

fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

// =======================================================================
// parser
// =======================================================================

mod parser {
    use super::*;
    use crate::ast::{self, Expr, GroupElement};

    fn select_of(sql: &str) -> ast::Select {
        match parse_statement(sql, &SqlLimits::default())
            .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
        {
            ast::Statement::Select(s) => *s,
            other => panic!("expected a SELECT, got {other:?}"),
        }
    }

    #[test]
    fn grouping_extensions_parse_into_elements() {
        let s = select_of("SELECT a, b, SUM(x) FROM ag GROUP BY GROUPING SETS ((a, b), (a), ())");
        assert!(s.group_by.is_empty());
        let Some(el) = s.grouping else {
            panic!("no grouping")
        };
        let [GroupElement::Sets(sets)] = el.as_slice() else {
            panic!("{el:?}")
        };
        assert_eq!(sets.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1, 0]);

        let s = select_of("SELECT a FROM ag GROUP BY ROLLUP (a, b)");
        assert!(matches!(s.grouping.as_deref(), Some([GroupElement::Rollup(m)]) if m.len() == 2));
        let s = select_of("SELECT a FROM ag GROUP BY CUBE (a, b)");
        assert!(matches!(s.grouping.as_deref(), Some([GroupElement::Cube(m)]) if m.len() == 2));
        // Plain expressions mix with the extensions, in written order.
        let s = select_of("SELECT a FROM ag GROUP BY a, ROLLUP (b)");
        let Some(el) = s.grouping else { panic!() };
        assert!(matches!(
            el.as_slice(),
            [GroupElement::Expr(_), GroupElement::Rollup(_)]
        ));
        // A composite member.
        let s = select_of("SELECT a FROM ag GROUP BY ROLLUP ((a, b), x)");
        let Some(el) = s.grouping else { panic!() };
        let [GroupElement::Rollup(m)] = el.as_slice() else {
            panic!()
        };
        assert_eq!(m.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn a_plain_group_by_is_unchanged() {
        let s = select_of("SELECT a FROM ag GROUP BY a, b");
        assert_eq!(s.group_by.len(), 2);
        assert!(s.grouping.is_none());
    }

    #[test]
    fn grouping_function_parses() {
        let s = select_of("SELECT GROUPING(a), GROUPING(a, b) FROM ag GROUP BY ROLLUP (a, b)");
        let ast::SelectItem::Item(i) = &s.projection[1] else {
            panic!()
        };
        assert!(matches!(&i.expr, Expr::Grouping(args) if args.len() == 2));
    }

    #[test]
    fn filter_and_distinct_parse_on_aggregates() {
        let s = select_of("SELECT SUM(x) FILTER (WHERE y > 0), COUNT(DISTINCT x), COUNT(*) FILTER (WHERE y IS NULL) FROM ag");
        let items: Vec<&Expr> = s
            .projection
            .iter()
            .map(|i| match i {
                ast::SelectItem::Item(i) => &i.expr,
                _ => panic!(),
            })
            .collect();
        assert!(matches!(
            items[0],
            Expr::Aggregate {
                filter: Some(_),
                distinct: false,
                ..
            }
        ));
        assert!(matches!(
            items[1],
            Expr::Aggregate {
                filter: None,
                distinct: true,
                ..
            }
        ));
        assert!(matches!(
            items[2],
            Expr::Aggregate {
                filter: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn what_stays_outside_the_grammar_is_a_typed_error() {
        for sql in [
            "SELECT a FROM ag GROUP BY ALL",
            "SELECT a FROM ag GROUP BY a WITH ROLLUP",
            "SELECT COUNT(DISTINCT *) FROM ag",
            "SELECT abs(x) FILTER (WHERE x > 0) FROM ag",
            "SELECT SUM(x ORDER BY y) FROM ag",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(
                    err,
                    SqlError::Unsupported { .. }
                        | SqlError::Parse { .. }
                        | SqlError::TypeMismatch { .. }
                ),
                "{sql}: {err:?}"
            );
        }
    }
}

// =======================================================================
// 2E -- GROUPING SETS / ROLLUP / CUBE / GROUPING()
// =======================================================================

mod exec_2e {
    use super::*;
    use crate::bound::{BoundGroupElement, BoundGrouping};

    fn run(db: &Db, sql: &str) -> Vec<Row> {
        db.rows(sql)
    }

    /// The expansion of the clause the binder produced (for the structure tests).
    fn sets_of(db: &Db, sql: &str) -> Vec<Vec<usize>> {
        bound(db, sql)
            .grouping
            .as_ref()
            .expect("grouping")
            .expand(1_000)
            .unwrap()
    }

    #[test]
    fn grouping_sets_one_pass_three_groupings() {
        // The canonical example. Rows come out grouping set by grouping set, the
        // sets in the order written, the groups of a set in first-seen order.
        let db = grouped("agg_sets_basic");
        let rows = run(
            &db,
            "SELECT a, b, SUM(x) FROM ag GROUP BY GROUPING SETS ((a, b), (a), ())",
        );
        assert_eq!(
            rows,
            vec![
                ib(Some(1), Some(1), Some(1)),
                ib(Some(1), Some(2), Some(2)),
                ib(Some(2), Some(1), Some(12)),
                ib(None, Some(1), Some(16)),
                ib(Some(1), None, Some(3)),
                ib(Some(2), None, Some(12)),
                ib(None, None, Some(16)),
                ib(None, None, Some(31)),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn grouping_sets_are_one_operator_and_one_pass() {
        let db = grouped("agg_sets_one_pass");
        let sql = "SELECT a, b, SUM(x) FROM ag GROUP BY GROUPING SETS ((a, b), (a), ())";
        let plan = crate::plan::explain(&db.plan(sql));
        assert_eq!(plan.matches("Aggregate ").count(), 1, "{plan}");
        assert!(plan.contains("grouping_sets=3"), "{plan}");
        let m = ExecMetrics::default();
        db.try_run_with(sql, &ExecLimits::default(), &m, &[])
            .unwrap();
        let s = m.snapshot();
        // The table is read once (5 rows), not once per grouping set.
        assert_eq!(s.rows_scanned, 5);
        assert_eq!(s.aggregate_rows_processed, 5);
        assert_eq!(s.grouping_sets_evaluated_total, 3);
        assert_eq!(s.groups_emitted, 8);
        db.cleanup();
    }

    #[test]
    fn rollup_a_b_is_grouping_sets_ab_a_and_the_empty_set() {
        let db = grouped("agg_rollup");
        let rollup = run(&db, "SELECT a, b, SUM(x) FROM ag GROUP BY ROLLUP (a, b)");
        let sets = run(
            &db,
            "SELECT a, b, SUM(x) FROM ag GROUP BY GROUPING SETS ((a, b), (a), ())",
        );
        assert_eq!(rollup, sets);
        assert_eq!(
            sets_of(&db, "SELECT a FROM ag GROUP BY ROLLUP (a, b)"),
            vec![vec![0, 1], vec![0], vec![]]
        );
        assert_eq!(
            sets_of(&db, "SELECT a FROM ag GROUP BY ROLLUP (a, b, x)"),
            vec![vec![0, 1, 2], vec![0, 1], vec![0], vec![]]
        );
        db.cleanup();
    }

    #[test]
    fn cube_a_b_adds_the_set_of_b_alone() {
        let db = grouped("agg_cube");
        let rows = run(&db, "SELECT a, b, SUM(x) FROM ag GROUP BY CUBE (a, b)");
        assert_eq!(
            rows,
            vec![
                ib(Some(1), Some(1), Some(1)),
                ib(Some(1), Some(2), Some(2)),
                ib(Some(2), Some(1), Some(12)),
                ib(None, Some(1), Some(16)),
                ib(Some(1), None, Some(3)),
                ib(Some(2), None, Some(12)),
                ib(None, None, Some(16)),
                ib(None, Some(1), Some(29)),
                ib(None, Some(2), Some(2)),
                ib(None, None, Some(31)),
            ]
        );
        assert_eq!(
            sets_of(&db, "SELECT a FROM ag GROUP BY CUBE (a, b)"),
            vec![vec![0, 1], vec![0], vec![1], vec![]]
        );
        // CUBE (a, b, c) has all 2^3 subsets.
        let sets = sets_of(&db, "SELECT a FROM ag GROUP BY CUBE (a, b, x)");
        assert_eq!(sets.len(), 8);
        assert_eq!(sets[0], vec![0, 1, 2]);
        assert_eq!(sets[7], Vec::<usize>::new());
        let mut distinct = sets.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 8);
        db.cleanup();
    }

    #[test]
    fn cube_is_not_rollup() {
        let db = grouped("agg_cube_vs_rollup");
        let cube = run(&db, "SELECT a, b, SUM(x) FROM ag GROUP BY CUBE (a, b)");
        let rollup = run(&db, "SELECT a, b, SUM(x) FROM ag GROUP BY ROLLUP (a, b)");
        assert_eq!(cube.len(), rollup.len() + 2);
        db.cleanup();
    }

    #[test]
    fn grouping_is_zero_for_a_real_null_and_one_for_an_aggregated_away_column() {
        // `a` contains a NULL in the data (row 5). GROUPING must tell that NULL
        // (0) from the NULL of the total row (1).
        let db = grouped("agg_grouping_fn");
        let rows = run(
            &db,
            "SELECT a, GROUPING(a) AS g, SUM(x) FROM ag GROUP BY ROLLUP (a)",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), int(0), big(3)]),
                r(&[int(2), int(0), big(12)]),
                r(&[NULL, int(0), big(16)]), // the NULL that is in the data
                r(&[NULL, int(1), big(31)]), // the total
            ]
        );
        db.cleanup();
    }

    #[test]
    fn grouping_with_several_arguments_is_a_bit_string() {
        let db = grouped("agg_grouping_bits");
        let rows = run(
            &db,
            "SELECT a, b, GROUPING(a, b), GROUPING(b), GROUPING(a) FROM ag GROUP BY ROLLUP (a, b)",
        );
        // (a, b): 00 ; (a): b left out -> 01 ; (): 11. First argument most significant.
        assert_eq!(rows.len(), 8);
        assert_eq!(
            col(&rows, 2),
            vec![
                int(0),
                int(0),
                int(0),
                int(0),
                int(1),
                int(1),
                int(1),
                int(3)
            ]
        );
        assert_eq!(
            col(&rows, 3),
            vec![
                int(0),
                int(0),
                int(0),
                int(0),
                int(1),
                int(1),
                int(1),
                int(1)
            ]
        );
        assert_eq!(
            col(&rows, 4),
            vec![
                int(0),
                int(0),
                int(0),
                int(0),
                int(0),
                int(0),
                int(0),
                int(1)
            ]
        );
        db.cleanup();
    }

    #[test]
    fn grouping_in_having_and_order_by() {
        let db = grouped("agg_grouping_having");
        let rows = run(
            &db,
            "SELECT a, SUM(x) FROM ag GROUP BY ROLLUP (a) HAVING GROUPING(a) = 1",
        );
        assert_eq!(rows, vec![r(&[NULL, big(31)])]);
        // ORDER BY GROUPING(a) puts the real rows before the total.
        let rows = run(
            &db,
            "SELECT a, SUM(x) FROM ag GROUP BY ROLLUP (a) ORDER BY GROUPING(a), a",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(3)]),
                r(&[int(2), big(12)]),
                r(&[NULL, big(16)]),
                r(&[NULL, big(31)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn grouping_on_a_plain_group_by_is_always_zero() {
        let db = grouped("agg_grouping_plain");
        let rows = run(&db, "SELECT a, GROUPING(a) FROM ag GROUP BY a ORDER BY a");
        assert!(rows.iter().all(|r| r[1] == int(0)));
        db.cleanup();
    }

    #[test]
    fn grouping_of_something_not_in_the_group_by_is_a_bind_error() {
        let db = grouped("agg_grouping_bad");
        for sql in [
            "SELECT GROUPING(b) FROM ag GROUP BY ROLLUP (a)",
            "SELECT GROUPING(x) FROM ag",
            "SELECT GROUPING(a + 1) FROM ag GROUP BY ROLLUP (a)",
            "SELECT a FROM ag WHERE GROUPING(a) = 0 GROUP BY ROLLUP (a)",
        ] {
            let err = bind_err(&db, sql);
            assert!(
                matches!(err, SqlError::TypeMismatch { .. }),
                "{sql}: {err:?}"
            );
        }
        // An expression that is a grouping expression is fine.
        bound(
            &db,
            "SELECT GROUPING(a + 1) FROM ag GROUP BY ROLLUP (a + 1)",
        );
        db.cleanup();
    }

    #[test]
    fn the_empty_grouping_set_alone_is_a_global_aggregate() {
        let db = grouped("agg_empty_set");
        let rows = run(
            &db,
            "SELECT SUM(x), COUNT(*) FROM ag GROUP BY GROUPING SETS (())",
        );
        assert_eq!(rows, vec![r(&[big(31), big(5)])]);
        let rows = run(&db, "SELECT SUM(x) FROM ag GROUP BY ()");
        assert_eq!(rows, vec![r(&[big(31)])]);
        db.cleanup();
    }

    #[test]
    fn duplicate_grouping_sets_are_kept_as_the_standard_says() {
        // ISO/IEC 9075-2 7.9: the default set quantifier of GROUP BY is ALL, so a
        // grouping set listed twice produces its groups twice.
        let db = grouped("agg_dup_sets");
        let rows = run(
            &db,
            "SELECT a, SUM(x) FROM ag GROUP BY GROUPING SETS ((a), (a))",
        );
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[..3], rows[3..]);
        db.cleanup();
    }

    #[test]
    fn rollup_and_cube_of_one_column() {
        let db = grouped("agg_one_column");
        let rollup = run(&db, "SELECT a, SUM(x) FROM ag GROUP BY ROLLUP (a)");
        let cube = run(&db, "SELECT a, SUM(x) FROM ag GROUP BY CUBE (a)");
        assert_eq!(rollup, cube);
        assert_eq!(rollup.len(), 4);
        db.cleanup();
    }

    #[test]
    fn mixing_plain_columns_with_an_extension_multiplies_the_sets() {
        let db = grouped("agg_mixed");
        // a, ROLLUP (b)  =  (a, b), (a)
        assert_eq!(
            sets_of(&db, "SELECT a FROM ag GROUP BY a, ROLLUP (b)"),
            vec![vec![0, 1], vec![0]]
        );
        // ROLLUP (a), ROLLUP (b)  =  (a,b) (a) (b) ()   in cross-product order
        assert_eq!(
            sets_of(&db, "SELECT a FROM ag GROUP BY ROLLUP (a), ROLLUP (b)"),
            vec![vec![0, 1], vec![0], vec![1], vec![]]
        );
        // A composite member is rolled up as a unit.
        assert_eq!(
            sets_of(&db, "SELECT a FROM ag GROUP BY ROLLUP ((a, b), x)"),
            vec![vec![0, 1, 2], vec![0, 1], vec![]]
        );
        let rows = run(&db, "SELECT a, b, SUM(x) FROM ag GROUP BY a, ROLLUP (b)");
        assert_eq!(rows.len(), 4 + 3);
        db.cleanup();
    }

    #[test]
    fn grouping_expressions_may_be_expressions_and_appear_in_the_select_list() {
        let db = grouped("agg_expr_keys");
        let rows = run(&db, "SELECT a + 1, SUM(x) FROM ag GROUP BY ROLLUP (a + 1)");
        assert_eq!(
            rows,
            vec![
                r(&[int(2), big(3)]),
                r(&[int(3), big(12)]),
                r(&[NULL, big(16)]),
                r(&[NULL, big(31)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn a_grouping_column_reads_null_in_a_set_that_leaves_it_out() {
        // Row 1 has a = 1; in the () row the value of `a` must be NULL, not 1.
        let db = grouped("agg_null_when_out");
        let rows = run(
            &db,
            "SELECT a, COUNT(*) FROM ag GROUP BY GROUPING SETS ((), (a)) HAVING GROUPING(a) = 1",
        );
        assert_eq!(rows, vec![r(&[NULL, big(5)])]);
        db.cleanup();
    }

    #[test]
    fn empty_input_still_yields_the_empty_sets_row() {
        let db = ag_db("agg_empty_input");
        let rows = run(
            &db,
            "SELECT a, SUM(x), COUNT(*), GROUPING(a) FROM ag GROUP BY ROLLUP (a)",
        );
        assert_eq!(rows, vec![r(&[NULL, NULL, big(0), int(1)])]);
        let rows = run(&db, "SELECT a, SUM(x) FROM ag GROUP BY GROUPING SETS ((a))");
        assert!(rows.is_empty());
        db.cleanup();
    }

    #[test]
    fn a_column_outside_the_group_by_is_refused() {
        let db = grouped("agg_ungrouped");
        let err = bind_err(&db, "SELECT x FROM ag GROUP BY ROLLUP (a)");
        assert!(
            matches!(&err, SqlError::TypeMismatch { detail } if detail.contains("GROUP BY")),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn grouping_sets_compose_with_where_having_order_by_distinct_and_limit() {
        let db = grouped("agg_compose");
        let rows = run(
            &db,
            "SELECT a, SUM(x) AS s FROM ag WHERE id > 1 GROUP BY ROLLUP (a) HAVING SUM(x) > 10 ORDER BY SUM(x) DESC LIMIT 2",
        );
        assert_eq!(rows, vec![r(&[NULL, big(30)]), r(&[NULL, big(16)])]);
        db.cleanup();
    }

    #[test]
    fn a_window_function_over_grouping_sets() {
        let db = grouped("agg_window_over_sets");
        let rows = run(
            &db,
            "SELECT a, GROUPING(a), SUM(x), RANK() OVER (ORDER BY SUM(x) DESC), SUM(SUM(x)) OVER (PARTITION BY GROUPING(a))              FROM ag GROUP BY ROLLUP (a)",
        );
        assert_eq!(
            rows,
            vec![
                r(&[int(1), int(0), big(3), big(4), big(31)]),
                r(&[int(2), int(0), big(12), big(3), big(31)]),
                r(&[NULL, int(0), big(16), big(2), big(31)]),
                r(&[NULL, int(1), big(31), big(1), big(31)]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn grouping_sets_work_in_a_subquery_cte_and_set_operation() {
        let db = grouped("agg_nested");
        let rows = run(
            &db,
            "SELECT s FROM (SELECT a, SUM(x) AS s FROM ag GROUP BY ROLLUP (a)) q WHERE a IS NULL ORDER BY s",
        );
        assert_eq!(col(&rows, 0), vec![big(16), big(31)]);
        let rows = run(
            &db,
            "WITH c AS (SELECT a, GROUPING(a) AS g, SUM(x) AS s FROM ag GROUP BY ROLLUP (a)) SELECT s FROM c WHERE g = 1",
        );
        assert_eq!(rows, vec![r(&[big(31)])]);
        let rows = run(
            &db,
            "SELECT SUM(x) FROM ag GROUP BY ROLLUP (a) INTERSECT SELECT SUM(x) FROM ag GROUP BY a",
        );
        assert_eq!(
            sorted(rows),
            sorted(vec![r(&[big(3)]), r(&[big(12)]), r(&[big(16)])])
        );
        db.cleanup();
    }

    #[test]
    fn a_correlated_subquery_over_a_grouped_column_is_refused_with_grouping_sets() {
        let db = grouped("agg_correlated_refused");
        let err = bind_err(
            &db,
            "SELECT (SELECT COUNT(*) FROM ag z WHERE z.a = ag.a) FROM ag GROUP BY ROLLUP (a)",
        );
        assert!(
            matches!(&err, SqlError::Unsupported { detail } if detail.contains("correlated subquery")),
            "{err:?}"
        );
        db.cleanup();
    }

    // ---- limits and metrics

    #[test]
    fn the_number_of_grouping_sets_is_bounded() {
        let db = grouped("agg_limit_sets");
        // CUBE of six expressions is 64 sets: at the limit.
        let six = "SELECT COUNT(*) FROM ag GROUP BY CUBE (a, b, x, a + 1, b + 1, x + 1)";
        assert!(db.try_run(six).is_ok());
        // Seven is 128: over it, before any work.
        let seven = "SELECT COUNT(*) FROM ag GROUP BY CUBE (a, b, x, a + 1, b + 1, x + 1, a + 2)";
        let m = ExecMetrics::default();
        let err = db
            .try_run_with(seven, &ExecLimits::default(), &m, &[])
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::ResourceLimit { detail } if detail.contains("max_grouping_sets_per_statement")),
            "{err:?}"
        );
        assert_eq!(
            m.snapshot().rows_scanned,
            0,
            "refused before reading anything"
        );
        // A giant CUBE is refused without being expanded.
        let cols: Vec<String> = (0..60).map(|i| format!("x + {i}")).collect();
        let sql = format!(
            "SELECT COUNT(*) FROM ag GROUP BY CUBE ({})",
            cols.join(", ")
        );
        let err = db.try_run(&sql).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        // The limit is configurable and counts the statement's sets in total.
        let two = "SELECT COUNT(*) FROM ag GROUP BY ROLLUP (a, b) UNION ALL SELECT COUNT(*) FROM ag GROUP BY ROLLUP (a, b)";
        let tight = ExecLimits {
            max_grouping_sets_per_statement: 5,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(two, &tight, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        let roomy = ExecLimits {
            max_grouping_sets_per_statement: 6,
            ..ExecLimits::default()
        };
        assert!(db
            .try_run_with(two, &roomy, &ExecMetrics::default(), &[])
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn more_than_64_grouping_expressions_is_refused_at_bind_time() {
        let db = grouped("agg_limit_exprs");
        let cols: Vec<String> = (0..65).map(|i| format!("x + {i}")).collect();
        let sql = format!(
            "SELECT COUNT(*) FROM ag GROUP BY ROLLUP ({})",
            cols.join(", ")
        );
        let err = bind_err(&db, &sql);
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn groups_of_all_sets_count_against_max_group_count() {
        let db = grouped("agg_limit_groups");
        let limits = ExecLimits {
            max_group_count: 7,
            ..ExecLimits::default()
        };
        let sql = "SELECT a, b, SUM(x) FROM ag GROUP BY GROUPING SETS ((a, b), (a), ())"; // 8 groups
        let err = db
            .try_run_with(sql, &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::ResourceLimit { detail } if detail.contains("max_group_count")),
            "{err:?}"
        );
        let limits = ExecLimits {
            max_group_count: 8,
            ..ExecLimits::default()
        };
        assert_eq!(
            db.try_run_with(sql, &limits, &ExecMetrics::default(), &[])
                .unwrap()
                .rows
                .len(),
            8
        );
        db.cleanup();
    }

    #[test]
    fn explain_shows_the_grouping_structure() {
        let db = grouped("agg_explain");
        let plan =
            crate::plan::explain(&db.plan("SELECT a, b, SUM(x) FROM ag GROUP BY a, ROLLUP (b)"));
        assert!(plan.contains("grouping=[key[0], ROLLUP(key[1])]"), "{plan}");
        assert!(plan.contains("grouping_sets=2"), "{plan}");
        let plan = crate::plan::explain(&db.plan("SELECT a FROM ag GROUP BY CUBE (a, b)"));
        assert!(
            plan.contains("CUBE(key[0], key[1])") && plan.contains("grouping_sets=4"),
            "{plan}"
        );
        db.cleanup();
    }

    #[test]
    fn grouping_set_expansion_unit() {
        let g = BoundGrouping {
            elements: vec![
                BoundGroupElement::Expr(0),
                BoundGroupElement::Rollup(vec![vec![1], vec![2]]),
            ],
        };
        assert_eq!(g.set_count(), 3);
        assert_eq!(
            g.expand(10).unwrap(),
            vec![vec![0, 1, 2], vec![0, 1], vec![0]]
        );
        let big_cube = BoundGrouping {
            elements: vec![BoundGroupElement::Cube((0..130).map(|i| vec![i]).collect())],
        };
        assert_eq!(big_cube.set_count(), u128::MAX);
        assert!(matches!(
            big_cube.expand(64),
            Err(SqlError::ResourceLimit { .. })
        ));
        let _ = RelationalType::Integer;
    }
}

// =======================================================================
// 2F -- aggregate FILTER and DISTINCT aggregates
// =======================================================================

mod exec_2f {
    use super::*;

    /// | id | a | x | y |
    /// |---|---|---|---|
    /// | 1 | 1 | 1 | 5 |
    /// | 2 | 1 | 1 | -5 |
    /// | 3 | 1 | 2 | 5 |
    /// | 4 | 2 | 1 | -5 |
    /// | 5 | 2 | 3 | 5 |
    /// | 6 | 2 | NULL | 5 |
    /// | 7 | 2 | 3 | NULL |
    fn fd(tag: &str) -> Db {
        let db = ag_db(tag);
        seed_ag(
            &db,
            &[
                (1, Some(1), None, Some(1), Some(5)),
                (2, Some(1), None, Some(1), Some(-5)),
                (3, Some(1), None, Some(2), Some(5)),
                (4, Some(2), None, Some(1), Some(-5)),
                (5, Some(2), None, Some(3), Some(5)),
                (6, Some(2), None, None, Some(5)),
                (7, Some(2), None, Some(3), None),
            ],
        );
        db
    }

    // ---- FILTER

    #[test]
    fn filter_applies_to_one_aggregate_not_the_whole_select() {
        // SUM sees only y > 0 rows; COUNT(*) sees every row.
        let db = fd("agg_filter_one");
        let rows = db.rows("SELECT SUM(x) FILTER (WHERE y > 0), COUNT(*) FROM ag");
        // y > 0: ids 1,3,5,6 -> x = 1+2+3+NULL = 6 ; COUNT(*) = 7.
        assert_eq!(rows, vec![r(&[big(6), big(7)])]);
        db.cleanup();
    }

    #[test]
    fn two_aggregates_with_different_filters_in_one_select() {
        let db = fd("agg_filter_two");
        let rows = db.rows(
            "SELECT SUM(x) FILTER (WHERE y > 0), SUM(x) FILTER (WHERE y < 0), COUNT(*) FILTER (WHERE y IS NULL), COUNT(x), COUNT(*) FROM ag",
        );
        // y < 0: ids 2, 4 -> 1 + 1 = 2 ; y IS NULL: id 7.
        assert_eq!(rows, vec![r(&[big(6), big(2), big(1), big(6), big(7)])]);
        db.cleanup();
    }

    #[test]
    fn filter_per_group() {
        let db = fd("agg_filter_groups");
        let rows = db.rows("SELECT a, COUNT(*) FILTER (WHERE y > 0), SUM(x) FILTER (WHERE y > 0) FROM ag GROUP BY a ORDER BY a");
        assert_eq!(
            rows,
            vec![r(&[int(1), big(2), big(3)]), r(&[int(2), big(2), big(3)])]
        );
        db.cleanup();
    }

    #[test]
    fn a_filter_that_is_always_false_or_always_true() {
        let db = fd("agg_filter_const");
        let rows = db.rows(
            "SELECT SUM(x) FILTER (WHERE FALSE), COUNT(*) FILTER (WHERE FALSE), COUNT(*) FILTER (WHERE TRUE), MIN(x) FILTER (WHERE 1 = 2) FROM ag",
        );
        assert_eq!(rows, vec![r(&[NULL, big(0), big(7), NULL])]);
        // Per group, an all-false filter gives the empty-input value too.
        let rows = db.rows("SELECT a, SUM(x) FILTER (WHERE y > 100) FROM ag GROUP BY a ORDER BY a");
        assert_eq!(rows, vec![r(&[int(1), NULL]), r(&[int(2), NULL])]);
        db.cleanup();
    }

    #[test]
    fn a_null_filter_condition_excludes_the_row() {
        let db = fd("agg_filter_null");
        // Row 7 has y NULL: `y > 0` is UNKNOWN, `NOT (y > 0)` is UNKNOWN too.
        let rows = db.rows(
            "SELECT COUNT(*) FILTER (WHERE y > 0), COUNT(*) FILTER (WHERE NOT (y > 0)) FROM ag",
        );
        assert_eq!(rows, vec![r(&[big(4), big(2)])]);
        db.cleanup();
    }

    #[test]
    fn filter_on_a_global_aggregate_over_no_rows() {
        let db = ag_db("agg_filter_empty");
        let rows =
            db.rows("SELECT COUNT(*) FILTER (WHERE y > 0), SUM(x) FILTER (WHERE y > 0) FROM ag");
        assert_eq!(rows, vec![r(&[big(0), NULL])]);
        db.cleanup();
    }

    #[test]
    fn filter_in_having_and_order_by() {
        let db = fd("agg_filter_having");
        let rows = db.rows(
            "SELECT a FROM ag GROUP BY a HAVING SUM(x) FILTER (WHERE y < 0) > 1 ORDER BY SUM(x) FILTER (WHERE y < 0) DESC",
        );
        // group 1: 1 (id 2) ; group 2: 1 (id 4): neither exceeds 1.
        assert!(rows.is_empty());
        let rows = db.rows(
            "SELECT a FROM ag GROUP BY a HAVING COUNT(*) FILTER (WHERE y < 0) = 1 ORDER BY a",
        );
        assert_eq!(col(&rows, 0), vec![int(1), int(2)]);
        db.cleanup();
    }

    #[test]
    fn filter_with_grouping_sets_applies_in_every_set() {
        let db = fd("agg_filter_sets");
        let rows = db.rows("SELECT a, COUNT(*) FILTER (WHERE y > 0) FROM ag GROUP BY ROLLUP (a)");
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(2)]),
                r(&[int(2), big(2)]),
                r(&[NULL, big(4)])
            ]
        );
        db.cleanup();
    }

    #[test]
    fn the_filter_condition_is_typed_and_cannot_contain_aggregates() {
        let db = fd("agg_filter_bind");
        for sql in [
            "SELECT SUM(x) FILTER (WHERE x) FROM ag",
            "SELECT SUM(x) FILTER (WHERE x + 1) FROM ag",
            "SELECT SUM(x) FILTER (WHERE COUNT(*) > 1) FROM ag",
            "SELECT SUM(x) FILTER (WHERE ROW_NUMBER() OVER () > 1) FROM ag",
        ] {
            let err = bind_err(&db, sql);
            assert!(
                matches!(err, SqlError::TypeMismatch { .. }),
                "{sql}: {err:?}"
            );
        }
        let err = bind_err(&db, "SELECT SUM(x) FILTER (WHERE nope > 1) FROM ag");
        assert!(matches!(err, SqlError::UnknownObject { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn filter_skips_are_counted() {
        let db = fd("agg_filter_metric");
        let m = ExecMetrics::default();
        db.try_run_with(
            "SELECT SUM(x) FILTER (WHERE y > 0), COUNT(*) FILTER (WHERE y < 0), COUNT(*) FROM ag",
            &ExecLimits::default(),
            &m,
            &[],
        )
        .unwrap();
        // First filter excludes the 3 rows with y <= 0 or NULL; the second the 5 that are not negative.
        assert_eq!(m.snapshot().aggregate_filter_rows_skipped_total, 3 + 5);
        db.cleanup();
    }

    #[test]
    fn filter_on_a_window_aggregate() {
        let db = fd("agg_filter_window");
        let rows = db
            .rows("SELECT id, SUM(x) FILTER (WHERE y > 0) OVER (ORDER BY id) FROM ag ORDER BY id");
        assert_eq!(
            col(&rows, 1),
            vec![big(1), big(1), big(3), big(3), big(6), big(6), big(6)]
        );
        db.cleanup();
    }

    #[test]
    fn an_aggregate_inside_a_filtered_aggregate_group_compat_still_applies() {
        let db = fd("agg_filter_compat");
        let err = bind_err(
            &db,
            "SELECT a, SUM(x) FILTER (WHERE y > 0), id FROM ag GROUP BY a",
        );
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    // ---- DISTINCT

    #[test]
    fn count_distinct_per_group() {
        let db = fd("agg_distinct_count");
        let rows = db
            .rows("SELECT a, COUNT(DISTINCT x), COUNT(x), COUNT(*) FROM ag GROUP BY a ORDER BY a");
        // group 1: x = 1,1,2 -> 2 distinct ; group 2: x = 1,3,NULL,3 -> 2 distinct (the NULL is ignored).
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(2), big(3), big(3)]),
                r(&[int(2), big(2), big(3), big(4)])
            ]
        );
        db.cleanup();
    }

    #[test]
    fn distinct_is_per_group_not_across_groups() {
        // x = 1 appears in both groups: it counts once in each, not once overall.
        let db = fd("agg_distinct_scope");
        let rows = db.rows("SELECT a, COUNT(DISTINCT x) FROM ag GROUP BY a ORDER BY a");
        assert_eq!(col(&rows, 1), vec![big(2), big(2)]);
        let rows = db.rows("SELECT COUNT(DISTINCT x) FROM ag");
        assert_eq!(rows, vec![r(&[big(3)])]);
        db.cleanup();
    }

    #[test]
    fn sum_and_avg_distinct() {
        let db = fd("agg_distinct_sum");
        let rows = db.rows(
            "SELECT a, SUM(DISTINCT x), SUM(x), AVG(DISTINCT x), AVG(x), MIN(DISTINCT x), MAX(DISTINCT x) FROM ag GROUP BY a ORDER BY a",
        );
        // group 1: x = 1,1,2 ; group 2: x = 1,3,NULL,3.
        assert_eq!(
            rows,
            vec![
                r(&[
                    int(1),
                    big(3),
                    big(4),
                    dbl(1.5),
                    dbl(4.0 / 3.0),
                    int(1),
                    int(2)
                ]),
                r(&[
                    int(2),
                    big(4),
                    big(7),
                    dbl(2.0),
                    dbl(7.0 / 3.0),
                    int(1),
                    int(3)
                ]),
            ]
        );
        db.cleanup();
    }

    #[test]
    fn every_standard_aggregate_accepts_distinct() {
        let db = fd("agg_distinct_all");
        let rows = db.rows("SELECT COUNT(DISTINCT x), SUM(DISTINCT x), AVG(DISTINCT x), MIN(DISTINCT x), MAX(DISTINCT x) FROM ag");
        assert_eq!(rows, vec![r(&[big(3), big(6), dbl(2.0), int(1), int(3)])]);
        db.cleanup();
    }

    #[test]
    fn distinct_on_a_null_only_column_and_on_empty_input() {
        let db = ag_db("agg_distinct_nulls");
        seed_ag(
            &db,
            &[
                (1, Some(1), None, None, None),
                (2, Some(1), None, None, None),
            ],
        );
        let rows = db.rows("SELECT COUNT(DISTINCT x), SUM(DISTINCT x), MIN(DISTINCT x) FROM ag");
        assert_eq!(rows, vec![r(&[big(0), NULL, NULL])]);
        db.write("DELETE FROM ag WHERE id > 0");
        let rows = db.rows("SELECT COUNT(DISTINCT x), SUM(DISTINCT x) FROM ag");
        assert_eq!(rows, vec![r(&[big(0), NULL])]);
        db.cleanup();
    }

    #[test]
    fn distinct_treats_negative_zero_and_zero_as_one_value() {
        let db = ag_db("agg_distinct_float");
        db.write("INSERT INTO ag VALUES (1, 1, 1, 1, 1, 0.0), (2, 1, 1, 1, 1, -0.0), (3, 1, 1, 1, 1, 1.5)");
        let rows = db.rows("SELECT COUNT(DISTINCT d), SUM(DISTINCT d) FROM ag");
        assert_eq!(rows, vec![r(&[big(2), dbl(1.5)])]);
        db.cleanup();
    }

    #[test]
    fn distinct_with_filter_and_with_grouping_sets() {
        let db = fd("agg_distinct_combo");
        let rows = db.rows("SELECT COUNT(DISTINCT x) FILTER (WHERE y > 0) FROM ag");
        // y > 0: x = 1 (id1), 2, 3, NULL -> {1,2,3}
        assert_eq!(rows, vec![r(&[big(3)])]);
        let rows = db.rows("SELECT a, COUNT(DISTINCT x) FROM ag GROUP BY ROLLUP (a)");
        assert_eq!(
            rows,
            vec![
                r(&[int(1), big(2)]),
                r(&[int(2), big(2)]),
                r(&[NULL, big(3)])
            ]
        );
        db.cleanup();
    }

    #[test]
    fn distinct_in_having_and_order_by_and_alongside_plain_aggregates() {
        let db = fd("agg_distinct_having");
        let rows = db.rows("SELECT a FROM ag GROUP BY a HAVING COUNT(DISTINCT x) = 2 ORDER BY SUM(DISTINCT x) DESC");
        assert_eq!(col(&rows, 0), vec![int(2), int(1)]);
        db.cleanup();
    }

    #[test]
    fn distinct_is_not_allowed_on_the_wildcard_nor_on_a_window_aggregate() {
        let db = fd("agg_distinct_bind");
        let err =
            parse_statement("SELECT COUNT(DISTINCT *) FROM ag", &SqlLimits::default()).unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = db
            .try_bind("SELECT COUNT(DISTINCT x) OVER () FROM ag")
            .unwrap_err();
        assert!(matches!(err, SqlError::Unsupported { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn distinct_state_is_bounded_by_max_aggregate_state_bytes() {
        let db = ag_db("agg_distinct_limit");
        let values: Vec<String> = (1..=200)
            .map(|i| format!("({i}, 1, 1, {i}, 1, 1.0)"))
            .collect();
        db.write(&format!("INSERT INTO ag VALUES {}", values.join(", ")));
        // 200 distinct values need ~200 * (5 + 32 + state) bytes; a 2 KiB budget cannot hold them.
        let limits = ExecLimits {
            max_aggregate_state_bytes: 2 * 1024,
            ..ExecLimits::default()
        };
        let m = ExecMetrics::default();
        let err = db
            .try_run_with("SELECT COUNT(DISTINCT x) FROM ag", &limits, &m, &[])
            .unwrap_err();
        assert!(
            matches!(&err, SqlError::ResourceLimit { detail } if detail.contains("max_aggregate_state_bytes")),
            "{err:?}"
        );
        assert_eq!(m.snapshot().aggregate_resource_limit_hits, 1);
        // The same statement without DISTINCT needs a few bytes only: no limit hit.
        assert!(db
            .try_run_with(
                "SELECT COUNT(x) FROM ag",
                &limits,
                &ExecMetrics::default(),
                &[]
            )
            .is_ok());
        // With the default budget it completes, with the right answer.
        assert_eq!(
            db.rows("SELECT COUNT(DISTINCT x) FROM ag"),
            vec![r(&[big(200)])]
        );
        db.cleanup();
    }

    #[test]
    fn explain_shows_filter_and_distinct() {
        let db = fd("agg_explain");
        let plan = crate::plan::explain(
            &db.plan("SELECT COUNT(DISTINCT x), SUM(x) FILTER (WHERE y > 0) FROM ag"),
        );
        assert!(plan.contains("count(DISTINCT col("), "{plan}");
        assert!(plan.contains("FILTER (WHERE"), "{plan}");
        db.cleanup();
    }

    #[test]
    fn result_types_of_distinct_and_filtered_aggregates() {
        let db = fd("agg_types");
        let b = bound(&db, "SELECT COUNT(DISTINCT x), SUM(DISTINCT x), AVG(DISTINCT x), SUM(x) FILTER (WHERE y > 0) FROM ag");
        let ty: Vec<_> = b
            .projection
            .iter()
            .map(|i| (i.expr.ty, i.expr.nullable))
            .collect();
        assert_eq!(
            ty,
            vec![
                (Some(RelationalType::Bigint), false),
                (Some(RelationalType::Bigint), true),
                (Some(RelationalType::Double), true),
                (Some(RelationalType::Bigint), true),
            ]
        );
        db.cleanup();
    }
}

mod composition {
    use super::*;

    #[test]
    fn filter_conditions_and_grouping_accept_parameters() {
        let db = grouped("agg_params");
        let r = db
            .try_run_with(
                "SELECT COUNT(*) FILTER (WHERE x > $1), SUM(x) FILTER (WHERE a = $2) FROM ag",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[int(3), int(2)],
            )
            .unwrap();
        // x > 3: 4, 8, 16 -> 3 rows ; a = 2: 4 + 8.
        assert_eq!(r.rows, vec![vec![big(3), big(12)]]);
        db.cleanup();
    }

    #[test]
    fn a_parenthesised_group_by_tuple_is_the_same_as_the_list() {
        let db = grouped("agg_tuple");
        let tuple = db.rows("SELECT a, b, SUM(x) FROM ag GROUP BY (a, b)");
        let list = db.rows("SELECT a, b, SUM(x) FROM ag GROUP BY a, b");
        assert_eq!(tuple, list);
        db.cleanup();
    }

    #[test]
    fn grouping_sets_and_filters_in_update_and_delete_subqueries() {
        let db = grouped("agg_dml");
        db.write("DELETE FROM ag WHERE x IN (SELECT MAX(x) FILTER (WHERE b = 1) FROM ag GROUP BY ROLLUP (a))");
        // The rollup groups' maxima over b = 1 rows are 1, 8, 16 and the total 16: rows with x in {1, 8, 16} go.
        assert_eq!(
            col(&db.rows("SELECT id FROM ag ORDER BY id"), 0),
            vec![int(2), int(3)]
        );
        db.cleanup();
    }
}
