//! SQL-mission-1, Slice A -- scalar / `IN` / `EXISTS` subqueries
//! (uncorrelated), and their use in `UPDATE`/`DELETE ... WHERE`.
//!
//! Layout: `parser` (grammar -> AST), `binder` (resolution, typing, arity,
//! authorization, context restrictions, limits), `exec` (real tables, the
//! real pipeline), `differential` (randomized comparison against an
//! independent reference model).

use crate::mission1_support::*;

// =======================================================================
// Shared fixture data
// =======================================================================

/// `t`: (1 alice true) (2 bob false) (3 carol NULL) (4 NULL true) (5 erin true)
/// `orders`: (10 c1 100 ->1) (11 c2 50 ->1) (12 c3 75 ->2) (13 c4 NULL ->3)
///           (14 c5 20 ->NULL)
fn seeded(tag: &str) -> Db {
    let db = Db::new(tag);
    db.seed_t(&[
        (1, Some("alice"), Some(true)),
        (2, Some("bob"), Some(false)),
        (3, Some("carol"), None),
        (4, None, Some(true)),
        (5, Some("erin"), Some(true)),
    ]);
    db.seed_orders(&[
        (10, "c1", Some(100), Some(1)),
        (11, "c2", Some(50), Some(1)),
        (12, "c3", Some(75), Some(2)),
        (13, "c4", None, Some(3)),
        (14, "c5", Some(20), None),
    ]);
    db
}

fn ids(rows: Vec<Row>) -> Vec<i32> {
    rows.into_iter()
        .map(|r| match &r[0] {
            Some(rubixdb::relational::RelationalValue::Integer(n)) => *n,
            other => panic!("expected an INTEGER id, got {other:?}"),
        })
        .collect()
}

// =======================================================================
// Parser
// =======================================================================

mod parser {
    use crate::ast::*;
    use crate::limits::SqlLimits;
    use crate::parse::parse_statement;

    fn parse(sql: &str) -> Statement {
        parse_statement(sql, &SqlLimits::default())
            .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
    }

    fn select(sql: &str) -> Box<Select> {
        match parse(sql) {
            Statement::Select(s) => s,
            other => panic!("expected a SELECT, got {other:?}"),
        }
    }

    fn inner(q: &QueryExpr) -> &Select {
        match q {
            QueryExpr::Select(s) => s,
        }
    }

    #[test]
    fn scalar_subquery_in_the_select_list_parses_to_a_scalar_subquery_node() {
        let s = select("SELECT id, (SELECT MAX(amount) FROM orders) AS m FROM t");
        assert_eq!(s.projection.len(), 2);
        let SelectItem::Item(item) = &s.projection[1] else {
            panic!("expected an expression item");
        };
        assert_eq!(item.alias.as_ref().unwrap().value, "m");
        let Expr::ScalarSubquery(q) = &item.expr else {
            panic!("expected ScalarSubquery, got {:?}", item.expr);
        };
        let q = inner(q);
        assert_eq!(q.from.as_ref().unwrap().first.name.last().value, "orders");
    }

    #[test]
    fn exists_and_not_exists_parse_with_the_right_polarity() {
        let s = select("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders)");
        assert!(matches!(
            s.selection,
            Some(Expr::Exists { negated: false, .. })
        ));
        let s = select("SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders)");
        // sqlparser parses `NOT EXISTS (..)` as `Exists { negated: true }`
        // or as `NOT (Exists)`; either way exactly one negation results.
        match s.selection {
            Some(Expr::Exists { negated: true, .. }) => {}
            Some(Expr::UnaryOp {
                op: UnaryOp::Not,
                expr,
            }) => assert!(matches!(*expr, Expr::Exists { negated: false, .. })),
            other => panic!("unexpected NOT EXISTS shape: {other:?}"),
        }
    }

    #[test]
    fn in_and_not_in_subquery_parse_with_the_right_polarity() {
        let s = select("SELECT id FROM t WHERE id IN (SELECT t_id FROM orders)");
        assert!(matches!(
            s.selection,
            Some(Expr::InSubquery { negated: false, .. })
        ));
        let s = select("SELECT id FROM t WHERE id NOT IN (SELECT t_id FROM orders)");
        assert!(matches!(
            s.selection,
            Some(Expr::InSubquery { negated: true, .. })
        ));
    }

    #[test]
    fn a_subquery_is_an_ordinary_operand_of_comparisons_and_arithmetic() {
        let s = select("SELECT id FROM t WHERE id = (SELECT MIN(t_id) FROM orders) + 1");
        let Some(Expr::BinaryOp { op, right, .. }) = &s.selection else {
            panic!("expected a comparison");
        };
        assert_eq!(*op, BinaryOp::Eq);
        let Expr::BinaryOp { left, op, .. } = right.as_ref() else {
            panic!("expected the + expression");
        };
        assert_eq!(*op, BinaryOp::Add);
        assert!(matches!(left.as_ref(), Expr::ScalarSubquery(_)));
    }

    #[test]
    fn subqueries_nest_three_levels_deep() {
        let s = select(
            "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount > \
             (SELECT MIN(amount) FROM orders WHERE t_id IN (SELECT id FROM t)))",
        );
        let Some(Expr::InSubquery { subquery, .. }) = &s.selection else {
            panic!("level 1");
        };
        let l2 = inner(subquery);
        let Some(Expr::BinaryOp { right, .. }) = &l2.selection else {
            panic!("level 2 predicate");
        };
        let Expr::ScalarSubquery(q3) = right.as_ref() else {
            panic!("level 2 scalar");
        };
        let l3 = inner(q3);
        assert!(matches!(l3.selection, Some(Expr::InSubquery { .. })));
    }

    #[test]
    fn subquery_keeps_its_own_order_by_and_limit() {
        let s = select("SELECT (SELECT amount FROM orders ORDER BY amount DESC LIMIT 1)");
        let SelectItem::Item(item) = &s.projection[0] else {
            panic!()
        };
        let Expr::ScalarSubquery(q) = &item.expr else {
            panic!()
        };
        let q = inner(q);
        assert_eq!(q.order_by.len(), 1);
        assert!(q.order_by[0].descending);
        assert!(q.limit.is_some());
    }

    #[test]
    fn subquery_in_update_and_delete_where_parses() {
        assert!(matches!(
            parse("UPDATE t SET name = 'x' WHERE id IN (SELECT t_id FROM orders)"),
            Statement::Update(_)
        ));
        assert!(matches!(
            parse("DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders)"),
            Statement::Delete(_)
        ));
    }

    #[test]
    fn pathologically_deep_subquery_nesting_is_a_controlled_limit_error() {
        let mut sql = String::from("SELECT 1");
        for _ in 0..400 {
            sql = format!("SELECT ({sql})");
        }
        let err = parse_statement(&sql, &SqlLimits::default()).unwrap_err();
        assert!(
            matches!(err, crate::error::SqlError::ResourceLimit { .. }),
            "expected ResourceLimit, got {err:?}"
        );
    }
}

// =======================================================================
// Binder
// =======================================================================

mod binder {
    use super::*;
    use crate::auth::AuthContext;
    use crate::bound::{BoundExprKind, BoundStatement, SubqueryKind};
    use crate::error::SqlError;
    use crate::limits::SqlLimits;

    fn bound_select(db: &Db, sql: &str) -> Box<crate::bound::BoundSelect> {
        match db.try_bind(sql).unwrap_or_else(|e| panic!("{sql:?}: {e}")) {
            BoundStatement::Select(s) => s,
            other => panic!("expected a SELECT, got {other:?}"),
        }
    }

    #[test]
    fn scalar_subquery_binds_with_the_inner_columns_type_and_is_nullable() {
        let db = Db::new("bind_scalar_ty");
        let s = bound_select(&db, "SELECT (SELECT MAX(amount) FROM orders)");
        let e = &s.projection[0].expr;
        assert_eq!(e.ty, Some(rubixdb::relational::RelationalType::Integer));
        assert!(e.nullable, "a scalar subquery can yield NULL on zero rows");
        let BoundExprKind::Subquery(sq) = &e.kind else {
            panic!("expected a subquery node");
        };
        assert_eq!(sq.kind, SubqueryKind::Scalar);
        assert!(sq.outer_refs.is_empty());
        db.cleanup();
    }

    #[test]
    fn exists_binds_as_a_non_nullable_boolean_and_in_as_a_nullable_one() {
        let db = Db::new("bind_exists_in");
        let s = bound_select(
            &db,
            "SELECT EXISTS (SELECT 1 FROM orders), 3 IN (SELECT t_id FROM orders)",
        );
        let bool_ty = Some(rubixdb::relational::RelationalType::Boolean);
        assert_eq!(s.projection[0].expr.ty, bool_ty);
        assert!(!s.projection[0].expr.nullable);
        assert_eq!(s.projection[1].expr.ty, bool_ty);
        assert!(s.projection[1].expr.nullable);
        db.cleanup();
    }

    #[test]
    fn scalar_and_in_subqueries_must_return_exactly_one_column() {
        let db = Db::new("bind_arity");
        for sql in [
            "SELECT (SELECT id, amount FROM orders WHERE id = 10)",
            "SELECT (SELECT * FROM t)",
            "SELECT id FROM t WHERE id IN (SELECT id, t_id FROM orders)",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(
                matches!(err, SqlError::TypeMismatch { .. }),
                "{sql:?} -> {err:?}"
            );
        }
        // EXISTS has no arity requirement.
        assert!(db.try_bind("SELECT EXISTS (SELECT * FROM t)").is_ok());
        db.cleanup();
    }

    #[test]
    fn in_subquery_operand_type_must_match_the_subquery_column() {
        let db = Db::new("bind_in_ty");
        let err = db
            .try_bind("SELECT id FROM t WHERE id IN (SELECT customer FROM orders)")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // The id column is INTEGER, orders.id is BIGINT: no implicit coercion (D21).
        let err = db
            .try_bind("SELECT id FROM t WHERE id IN (SELECT id FROM orders)")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_comparison_operand_type_mismatch_is_a_bind_error() {
        let db = Db::new("bind_scalar_ty_mismatch");
        let err = db
            .try_bind("SELECT id FROM t WHERE id = (SELECT customer FROM orders WHERE id = 10)")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_literal_operand_adopts_the_subquery_columns_type() {
        let db = Db::new("bind_literal_adopts");
        assert!(db
            .try_bind("SELECT id FROM t WHERE 3 = (SELECT MIN(t_id) FROM orders)")
            .is_ok());
        assert!(db
            .try_bind("SELECT id FROM t WHERE NULL IN (SELECT t_id FROM orders)")
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn known_limitation_a_literal_inside_a_subquery_does_not_adapt_to_the_outer_type() {
        // D21 (no implicit coercion): the subquery column `5` is an INTEGER
        // literal fixed inside its own SELECT list; comparing it with the
        // BIGINT `orders.id` is a bind-time TypeMismatch rather than a
        // silent widening. (`id IN (5)` -- a list, not a subquery -- adapts
        // the literal; that asymmetry is recorded in the feature matrix.)
        let db = Db::new("bind_literal_limitation");
        let err = db
            .try_bind("SELECT id FROM orders WHERE id IN (SELECT 5)")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        assert!(db.try_bind("SELECT id FROM orders WHERE id IN (5)").is_ok());
        db.cleanup();
    }

    #[test]
    fn unknown_columns_and_tables_inside_a_subquery_are_bind_errors() {
        let db = Db::new("bind_unknown_inner");
        let err = db
            .try_bind("SELECT (SELECT nope FROM orders)")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "column", .. }), "{err:?}");
        let err = db
            .try_bind("SELECT id FROM t WHERE id IN (SELECT x FROM no_such_table)")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn an_outer_column_is_visible_inside_a_subquery_scope_since_slice_b() {
        // SLICE B SUPERSESSION (flagged in sql/MISSION1_FEATURE_MATRIX.md):
        // this test used to be `an_outer_column_is_not_visible_inside_an_
        // uncorrelated_subquery_scope` and asserted `UnknownObject` here,
        // pinning Slice A's limitation ("correlation arrives in Slice B").
        // Slice B is that feature, so the old assertion is now exactly what
        // must NOT hold. The Slice A guarantee it protected -- an outer
        // reference never silently binds to the *wrong* table -- is kept:
        // the reference must resolve to the enclosing `t` and be recorded.
        let db = Db::new("bind_outer_visibility");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE t_id = t.id)",
        );
        let BoundExprKind::Subquery(sq) = &s.selection.as_ref().unwrap().kind else {
            panic!("expected the WHERE to be the subquery");
        };
        assert_eq!(sq.outer_refs.len(), 1);
        assert_eq!(sq.outer_refs[0].table_ref, s.from[0].table.id, "resolved to the enclosing t");
        db.cleanup();
    }

    #[test]
    fn table_reference_ids_are_unique_across_a_statement_including_its_subqueries() {
        let db = Db::new("bind_unique_refs");
        let s = bound_select(&db, "SELECT id FROM t WHERE id > (SELECT MIN(id) FROM t)");
        let outer_ref = s.from[0].table.id.0;
        let BoundExprKind::BinaryOp { right, .. } = &s.selection.as_ref().unwrap().kind else {
            panic!("expected a comparison");
        };
        let BoundExprKind::Subquery(sq) = &right.kind else {
            panic!("expected the subquery");
        };
        let inner_ref = sq.query.from[0].table.id.0;
        assert_ne!(
            outer_ref, inner_ref,
            "the same table used in an outer query and its subquery must get distinct refs"
        );
        db.cleanup();
    }

    #[test]
    fn single_select_table_ref_numbering_is_unchanged() {
        let db = Db::new("bind_numbering");
        let s = bound_select(
            &db,
            "SELECT t.id FROM t JOIN orders ON orders.t_id = t.id",
        );
        assert_eq!(s.from[0].table.id.0, 0);
        assert_eq!(s.from[1].table.id.0, 1);
        db.cleanup();
    }

    #[test]
    fn parameters_inside_a_subquery_count_toward_the_statements_max_parameter() {
        let db = Db::new("bind_params");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount > $2) AND name = $1",
        );
        assert_eq!(s.max_parameter, 2);
        db.cleanup();
    }

    #[test]
    fn subquery_in_aggregated_statement_projection_and_having_binds() {
        let db = Db::new("bind_agg_ctx");
        assert!(db
            .try_bind("SELECT COUNT(*), (SELECT MAX(amount) FROM orders) FROM t")
            .is_ok());
        assert!(db
            .try_bind(
                "SELECT active, COUNT(*) FROM t GROUP BY active \
                 HAVING COUNT(*) > (SELECT COUNT(*) FROM orders WHERE amount IS NULL)"
            )
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn subqueries_fail_closed_in_clauses_that_cannot_run_them() {
        let db = Db::new("bind_unsupported_ctx");
        for sql in [
            "INSERT INTO t VALUES ((SELECT 1), 'x', true)",
            // SLICE B (flagged in sql/MISSION1_FEATURE_MATRIX.md): `UPDATE ... SET
            // x = (SELECT ...)` used to be refused here. Slice B implements it
            // (snapshot-consistent two-phase write, `correlated_tests::dml`), so
            // the case no longer belongs in this "cannot run a subquery" list.
            "SELECT id FROM t LIMIT (SELECT 1)",
            "SELECT id FROM t GROUP BY (SELECT 1)",
            "SELECT t.id FROM t JOIN orders ON t.id = (SELECT 1)",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(
                matches!(err, SqlError::Unsupported { .. }),
                "{sql:?} -> {err:?}"
            );
        }
        db.cleanup();
    }

    #[test]
    fn a_table_the_principal_cannot_see_is_indistinguishable_inside_a_subquery() {
        let db = Db::new("bind_authz");
        // `limited` may SELECT from t only.
        let t = db.table_id("t");
        db.catalog
            .grant(
                "limited",
                rubixdb::catalog::schema::ObjectKind::Table,
                t,
                rubixdb::catalog::schema::Privilege::Select,
            )
            .unwrap();
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        let direct = db
            .try_bind_with("SELECT t_id FROM orders", &auth, &limits)
            .unwrap_err();
        let nested = db
            .try_bind_with(
                "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders)",
                &auth,
                &limits,
            )
            .unwrap_err();
        let missing = db
            .try_bind_with(
                "SELECT id FROM t WHERE id IN (SELECT t_id FROM no_such_table)",
                &auth,
                &limits,
            )
            .unwrap_err();
        assert_eq!(direct, nested, "a forbidden table must fail identically when nested");
        assert!(matches!(
            (&nested, &missing),
            (
                SqlError::UnknownObject { kind: "table", .. },
                SqlError::UnknownObject { kind: "table", .. }
            )
        ));
        // And the permitted table still works.
        assert!(db
            .try_bind_with("SELECT id FROM t WHERE id IN (SELECT id FROM t)", &auth, &limits)
            .is_ok());
        db.cleanup();
    }

    fn nested(levels: usize) -> String {
        let mut sql = String::from("SELECT 1");
        for _ in 0..levels {
            sql = format!("SELECT ({sql})");
        }
        sql
    }

    #[test]
    fn subquery_nesting_depth_is_bounded_exactly_at_max_subquery_depth() {
        let db = Db::new("bind_depth");
        // Raise the parser's own bound so the subquery bound is what fires.
        let limits = SqlLimits {
            max_expression_depth: 4096,
            max_subquery_depth: 4,
            ..SqlLimits::default()
        };
        let auth = AuthContext::admin("p");
        assert!(db.try_bind_with(&nested(4), &auth, &limits).is_ok());
        let err = db.try_bind_with(&nested(5), &auth, &limits).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn subquery_count_per_statement_is_bounded_exactly_at_the_limit() {
        let db = Db::new("bind_count");
        let limits = SqlLimits {
            max_subqueries_per_statement: 8,
            ..SqlLimits::default()
        };
        let auth = AuthContext::admin("p");
        let many = |n: usize| format!("SELECT {}", vec!["(SELECT 1)"; n].join(", "));
        assert!(db.try_bind_with(&many(8), &auth, &limits).is_ok());
        let err = db.try_bind_with(&many(9), &auth, &limits).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// Execution against real tables
// =======================================================================

mod exec {
    use super::*;
    use crate::error::SqlError;
    use crate::exec::{CancellationToken, ExecLimits, ExecMetrics};

    fn one(db: &Db, sql: &str) -> V {
        let mut rows = db.rows(sql);
        assert_eq!(rows.len(), 1, "{sql:?} -> {rows:?}");
        let mut row = rows.pop().unwrap();
        assert_eq!(row.len(), 1);
        row.pop().unwrap()
    }

    // ---- scalar -------------------------------------------------------

    #[test]
    fn scalar_subquery_in_a_from_less_select_list() {
        let db = seeded("x_scalar_fromless");
        assert_eq!(one(&db, "SELECT (SELECT MAX(amount) FROM orders)"), int(100));
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_in_the_select_list_of_a_table_query() {
        let db = seeded("x_scalar_select_list");
        let rows = db.rows("SELECT id, (SELECT COUNT(*) FROM orders) FROM t ORDER BY id");
        assert_eq!(rows.len(), 5);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r[0], int(i as i32 + 1));
            assert_eq!(r[1], big(5));
        }
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_in_where_drives_a_pk_lookup() {
        let db = seeded("x_scalar_where_pk");
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE id = (SELECT MIN(t_id) FROM orders)")),
            vec![1]
        );
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_in_where_comparison() {
        let db = seeded("x_scalar_where_cmp");
        assert_eq!(
            ids(db.rows(
                "SELECT id FROM t WHERE id > (SELECT MIN(id) FROM t) ORDER BY id"
            )),
            vec![2, 3, 4, 5],
            "the inner `t` and the outer `t` are different table references"
        );
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_in_having() {
        let db = seeded("x_scalar_having");
        let rows = db.rows(
            "SELECT active, COUNT(*) FROM t GROUP BY active \
             HAVING COUNT(*) > (SELECT COUNT(*) FROM orders WHERE amount IS NULL)",
        );
        assert_eq!(rows, vec![vec![boolean(true), big(3)]]);
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_as_an_arithmetic_and_comparison_operand() {
        let db = seeded("x_scalar_operand");
        assert_eq!(one(&db, "SELECT (SELECT MAX(amount) FROM orders) + 1"), int(101));
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE 4 > (SELECT MIN(id) FROM t) ORDER BY id")),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE (SELECT MAX(amount) FROM orders) > 1000")),
            Vec::<i32>::new()
        );
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_over_zero_rows_is_null() {
        let db = seeded("x_scalar_zero");
        assert_eq!(one(&db, "SELECT (SELECT amount FROM orders WHERE id = 999)"), NULL);
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_over_exactly_one_row_is_that_value() {
        let db = seeded("x_scalar_one");
        assert_eq!(
            one(&db, "SELECT (SELECT customer FROM orders WHERE id = 12)"),
            text("c3")
        );
        // A single row whose value is NULL is NULL, not an error.
        assert_eq!(one(&db, "SELECT (SELECT amount FROM orders WHERE id = 13)"), NULL);
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_over_multiple_rows_is_a_runtime_error_not_a_bind_error() {
        let db = seeded("x_scalar_many");
        let sql = "SELECT (SELECT amount FROM orders)";
        // It binds and plans: the SQL standard makes this a runtime error.
        assert!(db.try_plan(sql).is_ok());
        let err = db.try_run(sql).unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { ref detail } if detail.contains("more than one row")),
            "{err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_with_order_by_and_limit_picks_one_row() {
        let db = seeded("x_scalar_limit");
        assert_eq!(
            one(
                &db,
                "SELECT (SELECT amount FROM orders WHERE amount IS NOT NULL ORDER BY amount DESC LIMIT 1)"
            ),
            int(100)
        );
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_in_order_by_is_a_constant_sort_key() {
        let db = seeded("x_scalar_order_by");
        assert_eq!(
            ids(db.rows(
                "SELECT id FROM t ORDER BY (SELECT MAX(amount) FROM orders), id DESC"
            )),
            vec![5, 4, 3, 2, 1]
        );
        db.cleanup();
    }

    #[test]
    fn scalar_subquery_in_an_aggregated_select_list_and_distinct() {
        let db = seeded("x_scalar_agg_distinct");
        assert_eq!(
            db.rows("SELECT COUNT(*), (SELECT MAX(amount) FROM orders) FROM t"),
            vec![vec![big(5), int(100)]]
        );
        assert_eq!(
            db.rows("SELECT DISTINCT (SELECT MAX(amount) FROM orders) FROM t"),
            vec![vec![int(100)]]
        );
        db.cleanup();
    }

    #[test]
    fn subquery_with_its_own_group_by_and_having() {
        let db = seeded("x_scalar_group");
        // t_id groups: 1 -> 150, 2 -> 75, 3 -> NULL-sum, NULL -> 20
        assert_eq!(
            one(
                &db,
                "SELECT (SELECT t_id FROM orders GROUP BY t_id HAVING SUM(amount) > 100)"
            ),
            int(1)
        );
        db.cleanup();
    }

    #[test]
    fn subquery_parameter_is_bound_at_execution() {
        let db = seeded("x_param");
        let rows = db
            .try_run_with(
                "SELECT id FROM t WHERE id = (SELECT t_id FROM orders WHERE id = $1)",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[big(12)],
            )
            .unwrap()
            .rows;
        assert_eq!(ids(rows), vec![2]);
        let plan = db.plan("SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount > $1)");
        let crate::plan::Plan::Query { max_parameter, .. } = plan else {
            panic!("expected a query plan");
        };
        assert_eq!(max_parameter, 1);
        db.cleanup();
    }

    #[test]
    fn a_missing_parameter_inside_a_subquery_is_a_controlled_error() {
        let db = seeded("x_param_missing");
        let err = db
            .try_run("SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount > $1)")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        db.cleanup();
    }

    // ---- IN / NOT IN: the three-valued-logic truth tables -----------------

    /// The three subquery sets the truth table is run against.
    const S1: &str = "SELECT id FROM t WHERE id <= 2"; // {1, 2}
    const S2: &str = "SELECT t_id FROM orders"; // {1, 1, 2, 3, NULL}
    const S3: &str = "SELECT id FROM t WHERE id > 100"; // {} (empty)

    fn tri(db: &Db, lhs: &str, neg: &str, set: &str) -> V {
        one(db, &format!("SELECT {lhs} {neg}IN ({set})"))
    }

    #[test]
    fn in_subquery_truth_table() {
        let db = seeded("x_in_truth");
        let t = boolean(true);
        let f = boolean(false);
        // lhs:        1 (member)  9 (non-member)  NULL
        // S1 {1,2}:   T           F               U
        // S2 {1,2,3,NULL}: T      U               U
        // S3 {}:      F           F               F
        let table: [(&str, [V; 3]); 3] = [
            (S1, [t.clone(), f.clone(), NULL]),
            (S2, [t.clone(), NULL, NULL]),
            (S3, [f.clone(), f.clone(), f.clone()]),
        ];
        for (set, expected) in &table {
            for (lhs, want) in ["1", "9", "NULL"].iter().zip(expected) {
                assert_eq!(&tri(&db, lhs, "", set), want, "{lhs} IN ({set})");
            }
        }
        db.cleanup();
    }

    #[test]
    fn not_in_subquery_truth_table_never_true_when_the_set_contains_null() {
        let db = seeded("x_notin_truth");
        let t = boolean(true);
        let f = boolean(false);
        // lhs:        1 (member)  9 (non-member)  NULL
        // S1 {1,2}:   F           T               U
        // S2 {1,2,3,NULL}: F      U  <-- never TRUE   U
        // S3 {}:      T           T               T   (NULL NOT IN {} is TRUE)
        let table: [(&str, [V; 3]); 3] = [
            (S1, [f.clone(), t.clone(), NULL]),
            (S2, [f.clone(), NULL, NULL]),
            (S3, [t.clone(), t.clone(), t.clone()]),
        ];
        for (set, expected) in &table {
            for (lhs, want) in ["1", "9", "NULL"].iter().zip(expected) {
                assert_eq!(&tri(&db, lhs, "NOT ", set), want, "{lhs} NOT IN ({set})");
            }
        }
        db.cleanup();
    }

    #[test]
    fn not_in_with_a_null_in_the_subquery_returns_no_rows_the_classic_trap() {
        let db = seeded("x_notin_trap");
        // orders.t_id contains a NULL, so NOT IN can never be TRUE.
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE id NOT IN (SELECT t_id FROM orders)")),
            Vec::<i32>::new()
        );
        // Filtering the NULLs out of the subquery restores the expected rows.
        assert_eq!(
            ids(db.rows(
                "SELECT id FROM t WHERE id NOT IN (SELECT t_id FROM orders WHERE t_id IS NOT NULL) ORDER BY id"
            )),
            vec![4, 5]
        );
        db.cleanup();
    }

    #[test]
    fn in_subquery_filters_rows_and_ignores_the_null_member() {
        let db = seeded("x_in_filter");
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE id IN (SELECT t_id FROM orders) ORDER BY id")),
            vec![1, 2, 3]
        );
        db.cleanup();
    }

    #[test]
    fn in_subquery_with_a_null_outer_value_is_unknown_not_true_or_false() {
        let db = seeded("x_in_null_lhs");
        // t.name IS NULL for id 4: `name IN (...)` and `name NOT IN (...)`
        // are both UNKNOWN, so id 4 appears in neither result.
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE name IN (SELECT 'alice') ORDER BY id")),
            vec![1]
        );
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE name NOT IN (SELECT 'alice') ORDER BY id")),
            vec![2, 3, 5]
        );
        db.cleanup();
    }

    #[test]
    fn in_subquery_over_text_and_boolean_columns() {
        let db = seeded("x_in_types");
        assert_eq!(
            ids(db.rows(
                "SELECT id FROM t WHERE name IN (SELECT customer FROM orders WHERE id = 10 OR id = 12) ORDER BY id"
            )),
            Vec::<i32>::new(),
            "names are alice..., customers are c1..: no overlap"
        );
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE active IN (SELECT active FROM t WHERE id = 2) ORDER BY id")),
            vec![2]
        );
        db.cleanup();
    }

    // ---- EXISTS / NOT EXISTS ------------------------------------------------

    #[test]
    fn exists_and_not_exists_in_where() {
        let db = seeded("x_exists_where");
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 90) ORDER BY id")),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 1000)")),
            Vec::<i32>::new()
        );
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE amount > 1000) ORDER BY id")),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE amount > 90)")),
            Vec::<i32>::new()
        );
        db.cleanup();
    }

    #[test]
    fn exists_is_never_null_and_a_null_only_row_still_exists() {
        let db = seeded("x_exists_null");
        // Order 14 has t_id NULL: the row exists even though its value is NULL.
        assert_eq!(one(&db, "SELECT EXISTS (SELECT t_id FROM orders WHERE id = 14)"), boolean(true));
        assert_eq!(one(&db, "SELECT EXISTS (SELECT 1 FROM orders WHERE id = 999)"), boolean(false));
        assert_eq!(one(&db, "SELECT NOT EXISTS (SELECT 1 FROM orders WHERE id = 999)"), boolean(true));
        db.cleanup();
    }

    #[test]
    fn exists_over_an_empty_table_is_false() {
        let db = Db::new("x_exists_empty_table");
        assert_eq!(one(&db, "SELECT EXISTS (SELECT * FROM t)"), boolean(false));
        assert_eq!(one(&db, "SELECT (SELECT MAX(id) FROM t)"), NULL);
        assert_eq!(one(&db, "SELECT 1 IN (SELECT id FROM t)"), boolean(false));
        db.cleanup();
    }

    // ---- nesting, composition ----------------------------------------------

    #[test]
    fn three_levels_of_nested_subqueries() {
        let db = seeded("x_nested3");
        // innermost: active t ids {1,4,5}; middle: MIN(amount) of their orders = 50;
        // outer: t_ids of orders with amount > 50 = {1, 2}.
        assert_eq!(
            ids(db.rows(
                "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount > \
                 (SELECT MIN(amount) FROM orders WHERE t_id IN (SELECT id FROM t WHERE active))) ORDER BY id"
            )),
            vec![1, 2]
        );
        db.cleanup();
    }

    #[test]
    fn subquery_in_a_join_query_is_pushed_to_the_right_scan() {
        let db = seeded("x_join_subq");
        let mut got = ids(db.rows(
            "SELECT t.id FROM t JOIN orders ON orders.t_id = t.id \
             WHERE orders.amount > (SELECT MIN(amount) FROM orders)",
        ));
        got.sort();
        assert_eq!(got, vec![1, 1, 2]);
        db.cleanup();
    }

    #[test]
    fn two_subqueries_in_one_predicate() {
        let db = seeded("x_two_subq");
        assert_eq!(
            ids(db.rows(
                "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount >= 75) \
                 AND id NOT IN (SELECT t_id FROM orders WHERE amount = 75 AND t_id IS NOT NULL) ORDER BY id"
            )),
            vec![1]
        );
        db.cleanup();
    }

    // ---- UPDATE / DELETE ------------------------------------------------------

    #[test]
    fn update_where_in_subquery() {
        let db = seeded("x_update_in");
        let r = db.write("UPDATE t SET name = 'big' WHERE id IN (SELECT t_id FROM orders WHERE amount > 60)");
        assert_eq!(r.rows_affected, 2);
        assert_eq!(
            db.rows("SELECT id, name FROM t WHERE name = 'big' ORDER BY id"),
            vec![vec![int(1), text("big")], vec![int(2), text("big")]]
        );
        db.cleanup();
    }

    #[test]
    fn delete_where_not_in_subquery_and_not_exists() {
        let db = seeded("x_delete_notin");
        let r = db.write(
            "DELETE FROM t WHERE id NOT IN (SELECT t_id FROM orders WHERE t_id IS NOT NULL)",
        );
        assert_eq!(r.rows_affected, 2);
        assert_eq!(ids(db.rows("SELECT id FROM t ORDER BY id")), vec![1, 2, 3]);
        let r = db.write("DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE amount > 1000)");
        assert_eq!(r.rows_affected, 0);
        let r = db.write("DELETE FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE amount > 1000)");
        assert_eq!(r.rows_affected, 3);
        db.cleanup();
    }

    #[test]
    fn delete_with_a_scalar_subquery_over_the_same_table() {
        let db = seeded("x_delete_same_table");
        // The subquery is evaluated against the pre-statement state, then
        // the matching row (order 10, amount 100) is deleted.
        let r = db.write("DELETE FROM orders WHERE amount = (SELECT MAX(amount) FROM orders)");
        assert_eq!(r.rows_affected, 1);
        assert_eq!(one(&db, "SELECT COUNT(*) FROM orders"), big(4));
        db.cleanup();
    }

    #[test]
    fn a_failing_subquery_in_dml_where_changes_nothing() {
        let db = seeded("x_dml_fail");
        // Multi-row scalar subquery: runtime error, no partial write.
        let err = db
            .try_write("UPDATE t SET name = 'x' WHERE id = (SELECT t_id FROM orders)")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert_eq!(
            ids(db.rows("SELECT id FROM t WHERE name = 'x'")),
            Vec::<i32>::new()
        );
        db.cleanup();
    }

    // ---- transactions: the inner scan sees the outer transaction's writes ----

    #[test]
    fn a_subquery_scan_sees_its_own_transactions_uncommitted_writes() {
        // The mission text assumed scans do not see a transaction's own
        // writes. Increment 18 fixed that (PHASE_RUBIXDB_INCREMENT18_RESULTS.md
        // item 3); a subquery's inner scan is the same operator, so it sees
        // them too. Pinned here so a regression is loud.
        let db = seeded("x_txn_ryow");
        let mut txn = db.txm.begin().unwrap();
        let orders = db.table_id("orders");
        txn.put_row(orders, &[big(20), text("c9"), int(999), int(5)]).unwrap();
        let in_txn = db
            .run_in_txn("SELECT (SELECT MAX(amount) FROM orders)", &txn)
            .unwrap();
        assert_eq!(in_txn.rows, vec![vec![int(999)]]);
        let in_txn = db
            .run_in_txn("SELECT id FROM t WHERE id IN (SELECT t_id FROM orders) ORDER BY id", &txn)
            .unwrap();
        assert_eq!(ids(in_txn.rows), vec![1, 2, 3, 5]);
        // An autocommit reader (a different snapshot) sees neither.
        assert_eq!(one(&db, "SELECT (SELECT MAX(amount) FROM orders)"), int(100));
        txn.rollback().unwrap();
        db.cleanup();
    }

    // ---- evaluate-once, resource bounds, cancellation --------------------------

    #[test]
    fn an_uncorrelated_subquery_runs_once_per_statement() {
        let db = seeded("x_once");
        let metrics = ExecMetrics::default();
        let rows = db
            .try_run_with(
                "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders) ORDER BY id",
                &ExecLimits::default(),
                &metrics,
                &[],
            )
            .unwrap()
            .rows;
        assert_eq!(ids(rows), vec![1, 2, 3]);
        let m = metrics.snapshot();
        assert_eq!(m.subquery_executions, 1, "{m:?}");
        assert_eq!(m.subquery_cache_hits, 4, "5 outer rows: 1 miss + 4 hits ({m:?})");
        db.cleanup();
    }

    #[test]
    fn in_subquery_exceeding_max_materialized_rows_fails_closed_never_truncates() {
        let db = seeded("x_limit_rows");
        let sql = "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders)";
        let tight = ExecLimits {
            max_materialized_rows: 4,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(sql, &tight, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        // The exact boundary: 5 rows against a limit of 5 is fine.
        let exact = ExecLimits {
            max_materialized_rows: 5,
            ..ExecLimits::default()
        };
        let rows = db
            .try_run_with(sql, &exact, &ExecMetrics::default(), &[])
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 3);
        db.cleanup();
    }

    #[test]
    fn in_subquery_exceeding_max_materialized_bytes_fails_closed() {
        let db = seeded("x_limit_bytes");
        let limits = ExecLimits {
            max_materialized_bytes: 8,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(
                "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders)",
                &limits,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn scalar_and_exists_do_not_materialize_so_the_row_limit_does_not_apply() {
        let db = seeded("x_limit_scalar");
        let limits = ExecLimits {
            max_materialized_rows: 1,
            ..ExecLimits::default()
        };
        let rows = db
            .try_run_with(
                "SELECT EXISTS (SELECT * FROM orders), (SELECT MAX(amount) FROM orders)",
                &limits,
                &ExecMetrics::default(),
                &[],
            )
            .unwrap()
            .rows;
        assert_eq!(rows, vec![vec![boolean(true), int(100)]]);
        db.cleanup();
    }

    #[test]
    fn deadline_and_cancellation_surface_as_controlled_errors_through_subqueries() {
        let db = seeded("x_deadline");
        let sql = "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders)";
        std::thread::sleep(std::time::Duration::from_millis(2));
        let limits = ExecLimits {
            deadline: Some(std::time::Duration::from_nanos(1)),
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with(sql, &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert_eq!(err, SqlError::DeadlineExceeded);

        let plan = db.plan(sql);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = crate::exec::execute_autocommit(
            &plan,
            &db.txm,
            &db.store,
            &db.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &cancel,
        )
        .unwrap_err();
        assert_eq!(err, SqlError::Cancelled);
        db.cleanup();
    }

    #[test]
    fn subqueries_nested_to_the_depth_limit_execute_without_overflowing_the_stack() {
        let db = seeded("x_deep_exec");
        // The parser's own recursion bound is raised so that the subquery
        // nesting bound (32, the default) is what is being exercised.
        let limits = crate::limits::SqlLimits {
            max_expression_depth: 4096,
            ..crate::limits::SqlLimits::default()
        };
        let mut sql = String::from("SELECT MAX(amount) FROM orders");
        for _ in 0..crate::limits::SqlLimits::default().max_subquery_depth {
            sql = format!("SELECT ({sql})");
        }
        let plan = db.try_plan_with(&sql, &limits).unwrap();
        let result = crate::exec::execute_autocommit(
            &plan,
            &db.txm,
            &db.store,
            &db.builder,
            &[],
            &ExecLimits::default(),
            &ExecMetrics::default(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result.rows, vec![vec![int(100)]]);
        // One past the limit is refused at bind time, before any execution.
        let too_deep = format!("SELECT ({sql})");
        let err = db.try_plan_with(&too_deep, &limits).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    // ---- EXPLAIN ---------------------------------------------------------------

    #[test]
    fn explain_renders_the_nested_subquery_plan() {
        let db = seeded("x_explain");
        let plan = db.plan("EXPLAIN SELECT id FROM t WHERE id IN (SELECT t_id FROM orders)");
        let text = crate::plan::explain(&plan);
        assert!(text.contains("Subquery #0"), "{text}");
        assert!(text.contains("SeqScan"), "{text}");
        assert!(text.contains("IN subquery#0"), "{text}");
        db.cleanup();
    }

    #[test]
    fn explain_of_update_and_delete_renders_their_where_subquery_plans() {
        let db = seeded("x_explain_dml");
        for sql in [
            "EXPLAIN UPDATE t SET name = 'x' WHERE id IN (SELECT t_id FROM orders)",
            "EXPLAIN DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders)",
        ] {
            let text = crate::plan::explain(&db.plan(sql));
            assert!(text.contains("Subquery #0"), "{sql}: {text}");
        }
        db.cleanup();
    }

    #[test]
    fn explain_does_not_execute_the_subquery() {
        let db = seeded("x_explain_noexec");
        // A multi-row scalar subquery would be a runtime error if executed.
        let plan = db.plan("EXPLAIN SELECT (SELECT amount FROM orders)");
        assert!(matches!(plan, crate::plan::Plan::Explain(_)));
        db.cleanup();
    }
}

// =======================================================================
// Randomized differential test against an independent reference model
// =======================================================================

mod differential {
    use super::*;
    use rubixdb::relational::RelationalValue;

    /// A tiny deterministic generator (xorshift64*), so a failing seed
    /// reproduces exactly.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.below(100) < pct
        }
    }

    /// SQL three-valued logic, written out from the definitions (not shared
    /// with the engine): `Option<bool>` where `None` is UNKNOWN.
    type Tri = Option<bool>;

    fn eq(a: Option<i32>, b: Option<i32>) -> Tri {
        match (a, b) {
            (Some(x), Some(y)) => Some(x == y),
            _ => None,
        }
    }
    fn or(a: Tri, b: Tri) -> Tri {
        match (a, b) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        }
    }
    fn not(a: Tri) -> Tri {
        a.map(|x| !x)
    }
    /// `x IN (set)` by definition: the OR of `x = s` over every member
    /// (empty set -> FALSE).
    fn in_model(x: Option<i32>, set: &[Option<i32>]) -> Tri {
        let mut acc = Some(false);
        for s in set {
            acc = or(acc, eq(x, *s));
        }
        acc
    }

    struct Data {
        t_ids: Vec<i32>,
        /// (order id, amount, t_id)
        orders: Vec<(i64, Option<i32>, Option<i32>)>,
    }

    fn build(rng: &mut Rng, db: &Db) -> Data {
        let n_t = 1 + rng.below(8) as i32;
        let t_ids: Vec<i32> = (1..=n_t).collect();
        let t_rows: Vec<(i32, Option<&str>, Option<bool>)> =
            t_ids.iter().map(|&i| (i, Some("n"), Some(true))).collect();
        db.seed_t(&t_rows);
        let n_o = rng.below(10) as i64;
        let mut orders = Vec::new();
        for id in 0..n_o {
            let amount = if rng.chance(25) { None } else { Some(rng.below(6) as i32) };
            let t_id = if rng.chance(25) { None } else { Some(1 + rng.below(9) as i32) };
            orders.push((100 + id, amount, t_id));
        }
        let rows: Vec<(i64, &str, Option<i32>, Option<i32>)> =
            orders.iter().map(|&(i, a, t)| (i, "c", a, t)).collect();
        db.seed_orders(&rows);
        Data { t_ids, orders }
    }

    fn got_ids(rows: Vec<Row>) -> Vec<i32> {
        let mut v: Vec<i32> = rows
            .into_iter()
            .map(|r| match &r[0] {
                Some(RelationalValue::Integer(n)) => *n,
                other => panic!("{other:?}"),
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn in_not_in_exists_and_scalar_match_the_reference_model_on_random_data() {
        let mut checked = 0usize;
        for seed in 1..=40u64 {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
            let db = Db::new(&format!("diff_a_{seed}"));
            let d = build(&mut rng, &db);
            let threshold = rng.below(6) as i32;

            // The inner set: orders.t_id for orders with amount > threshold
            // (a NULL amount makes the predicate UNKNOWN -> row excluded).
            let set: Vec<Option<i32>> = d
                .orders
                .iter()
                .filter(|(_, a, _)| a.map(|a| a > threshold).unwrap_or(false))
                .map(|(_, _, t)| *t)
                .collect();
            let inner = format!("SELECT t_id FROM orders WHERE amount > {threshold}");

            // IN / NOT IN: WHERE keeps a row only when the predicate is TRUE.
            let want_in: Vec<i32> = d
                .t_ids
                .iter()
                .copied()
                .filter(|&i| in_model(Some(i), &set) == Some(true))
                .collect();
            let want_not_in: Vec<i32> = d
                .t_ids
                .iter()
                .copied()
                .filter(|&i| not(in_model(Some(i), &set)) == Some(true))
                .collect();
            assert_eq!(
                got_ids(db.rows(&format!("SELECT id FROM t WHERE id IN ({inner})"))),
                want_in,
                "IN seed {seed}"
            );
            assert_eq!(
                got_ids(db.rows(&format!("SELECT id FROM t WHERE id NOT IN ({inner})"))),
                want_not_in,
                "NOT IN seed {seed}"
            );

            // EXISTS / NOT EXISTS: uncorrelated, so all-or-nothing.
            let any = !set.is_empty();
            let all_ids = d.t_ids.clone();
            assert_eq!(
                got_ids(db.rows(&format!("SELECT id FROM t WHERE EXISTS ({inner})"))),
                if any { all_ids.clone() } else { vec![] },
                "EXISTS seed {seed}"
            );
            assert_eq!(
                got_ids(db.rows(&format!("SELECT id FROM t WHERE NOT EXISTS ({inner})"))),
                if any { vec![] } else { all_ids.clone() },
                "NOT EXISTS seed {seed}"
            );

            // The standalone three-valued value of `x IN (...)` for probes
            // including NULL.
            for probe in [Some(1), Some(7), None] {
                let lit = probe.map(|p| p.to_string()).unwrap_or_else(|| "NULL".to_string());
                let want = match in_model(probe, &set) {
                    Some(b) => boolean(b),
                    None => NULL,
                };
                let got = db.rows(&format!("SELECT {lit} IN ({inner})"));
                assert_eq!(got, vec![vec![want]], "{lit} IN seed {seed}");
                let want_neg = match not(in_model(probe, &set)) {
                    Some(b) => boolean(b),
                    None => NULL,
                };
                let got = db.rows(&format!("SELECT {lit} NOT IN ({inner})"));
                assert_eq!(got, vec![vec![want_neg]], "{lit} NOT IN seed {seed}");
            }

            // Scalar: MAX over the amounts > threshold (NULL when none).
            let want_max = d
                .orders
                .iter()
                .filter_map(|(_, a, _)| *a)
                .filter(|a| *a > threshold)
                .max();
            let got = db.rows(&format!(
                "SELECT (SELECT MAX(amount) FROM orders WHERE amount > {threshold})"
            ));
            assert_eq!(got, vec![vec![want_max.map(int).unwrap_or(NULL)]], "MAX seed {seed}");

            // Scalar comparison operand: rows whose id exceeds that MAX.
            let want_gt: Vec<i32> = d
                .t_ids
                .iter()
                .copied()
                .filter(|&i| want_max.map(|m| i > m).unwrap_or(false))
                .collect();
            assert_eq!(
                got_ids(db.rows(&format!(
                    "SELECT id FROM t WHERE id > (SELECT MAX(amount) FROM orders WHERE amount > {threshold})"
                ))),
                want_gt,
                "scalar-cmp seed {seed}"
            );
            checked += 1;
            db.cleanup();
        }
        assert_eq!(checked, 40);
    }
}
