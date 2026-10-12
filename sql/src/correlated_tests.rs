//! SQL-mission-1, Slice B -- correlated subqueries.
//!
//! Layout follows `subquery_tests`: `parser` (grammar -> AST), `binder`
//! (scope chain, resolution, typing, authorization), then the execution and
//! differential modules added by the later sub-slices (B2..B6).

use crate::mission1_support::*;

use crate::bound::{BoundExprKind, BoundSelect, BoundStatement, BoundSubqueryExpr, SubqueryKind};
use crate::error::SqlError;

// =======================================================================
// Helpers shared by the modules below
// =======================================================================

fn bound_select(db: &Db, sql: &str) -> Box<BoundSelect> {
    match db
        .try_bind(sql)
        .unwrap_or_else(|e| panic!("expected {sql:?} to bind, got {e}"))
    {
        BoundStatement::Select(s) => s,
        other => panic!("expected a SELECT, got {other:?}"),
    }
}

/// The subquery node that is the whole `WHERE` of `s`.
fn where_subquery(s: &BoundSelect) -> &BoundSubqueryExpr {
    match &s.selection.as_ref().expect("a WHERE clause").kind {
        BoundExprKind::Subquery(sq) => sq,
        other => panic!("expected the WHERE to be a subquery, got {other:?}"),
    }
}

/// The (single) subquery nested in the `WHERE` of `s`, wherever in the
/// boolean/comparison structure it sits.
fn find_subquery(expr: &crate::bound::BoundExpr) -> Option<&BoundSubqueryExpr> {
    match &expr.kind {
        BoundExprKind::Subquery(sq) => Some(sq),
        BoundExprKind::BinaryOp { left, right, .. } => {
            find_subquery(left).or_else(|| find_subquery(right))
        }
        BoundExprKind::UnaryOp { expr, .. } => find_subquery(expr),
        _ => None,
    }
}

fn columns_of(expr: &crate::bound::BoundExpr, out: &mut Vec<crate::bound::ColumnRef>) {
    match &expr.kind {
        BoundExprKind::Column(c) => out.push(*c),
        BoundExprKind::BinaryOp { left, right, .. } => {
            columns_of(left, out);
            columns_of(right, out);
        }
        BoundExprKind::UnaryOp { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            columns_of(expr, out)
        }
        _ => {}
    }
}

// =======================================================================
// Parser -- the grammar accepts a correlated reference
// =======================================================================

mod parser {
    use crate::ast::*;
    use crate::limits::SqlLimits;
    use crate::parse::parse_statement;

    fn select(sql: &str) -> Box<Select> {
        match parse_statement(sql, &SqlLimits::default())
            .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
        {
            Statement::Select(s) => s,
            other => panic!("expected a SELECT, got {other:?}"),
        }
    }

    fn inner_of(expr: &Expr) -> &Select {
        match expr {
            Expr::Exists { subquery, .. }
            | Expr::InSubquery { subquery, .. } => match subquery.as_ref() {
                QueryExpr::Select(s) => s,
            },
            Expr::ScalarSubquery(q) => match q.as_ref() {
                QueryExpr::Select(s) => s,
            },
            other => panic!("expected a subquery expression, got {other:?}"),
        }
    }

    #[test]
    fn a_correlated_exists_parses_with_a_qualified_outer_reference() {
        let s = select("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id)");
        let inner = inner_of(s.selection.as_ref().unwrap());
        let Some(Expr::BinaryOp { right, .. }) = inner.selection.as_ref() else {
            panic!("expected a comparison in the inner WHERE");
        };
        let Expr::Column(c) = right.as_ref() else {
            panic!("expected a column reference");
        };
        assert_eq!(
            c.parts.iter().map(|p| p.value.as_str()).collect::<Vec<_>>(),
            ["t", "id"]
        );
    }

    #[test]
    fn a_correlated_scalar_in_the_select_list_parses() {
        let s = select(
            "SELECT id, (SELECT MAX(amount) FROM orders o WHERE o.t_id = t.id) FROM t",
        );
        assert_eq!(s.projection.len(), 2);
    }

    #[test]
    fn a_correlated_in_and_not_in_parse() {
        for sql in [
            "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders o WHERE o.customer = t.name)",
            "SELECT id FROM t WHERE id NOT IN (SELECT t_id FROM orders o WHERE o.customer = t.name)",
        ] {
            select(sql);
        }
    }

    #[test]
    fn three_levels_of_nesting_parse() {
        let s = select(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
             EXISTS (SELECT 1 FROM t t3 WHERE t3.id = o.t_id AND t3.name = t.name))",
        );
        let mid = inner_of(s.selection.as_ref().unwrap());
        assert!(mid.selection.is_some());
    }

    #[test]
    fn a_correlated_subquery_parses_in_update_and_delete() {
        for sql in [
            "UPDATE t SET name = 'x' WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
            "DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
            "UPDATE t SET name = (SELECT MAX(customer) FROM orders WHERE orders.t_id = t.id)",
        ] {
            parse_statement(sql, &SqlLimits::default())
                .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"));
        }
    }

    #[test]
    fn a_cte_inside_a_subquery_is_refused_so_it_cannot_reference_the_outer_query() {
        // SLICE C SUPERSESSION (flagged in sql/MISSION1_FEATURE_MATRIX.md).
        // This test used to assert that ANY `WITH` was a typed Unsupported
        // error -- Slice B's way of guaranteeing "a CTE cannot reference the
        // enclosing query" while CTEs did not exist. Slice C implements
        // non-recursive CTEs, so `WITH` now parses; the guarantee the test
        // protected is kept, one stage later: a CTE body that names an
        // enclosing query's column is a bind error (the standard's rule),
        // while a CTE that does not is accepted.
        use crate::mission1_support::Db;
        let db = Db::new("b_cte_outer_ref");
        let err = db
            .try_bind("SELECT id FROM t WHERE EXISTS (WITH c AS (SELECT t.id AS x) SELECT 1 FROM c)")
            .unwrap_err();
        assert!(
            matches!(err, crate::error::SqlError::InvalidIdentifier { .. }),
            "{err:?}"
        );
        assert!(err.to_string().contains("CTE 'c' cannot reference outer column"), "{err}");
        assert!(db
            .try_bind("WITH c AS (SELECT 1) SELECT id FROM t WHERE EXISTS (SELECT 1 FROM c)")
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn lateral_is_a_typed_unsupported_error_not_a_panic() {
        // SLICE C SUPERSESSION (flagged in sql/MISSION1_FEATURE_MATRIX.md):
        // `JOIN LATERAL (...) l ON ...` used to be in this list; Slice C
        // implements it (`relation_tests::lateral`), so that case is removed.
        // The comma spelling is still a typed Unsupported error (comma-
        // separated FROM items, i.e. implicit CROSS JOIN, are out of the
        // mission), and this test still proves it is typed, never a panic.
        {
            let sql = "SELECT t.id FROM t, LATERAL (SELECT o.id FROM orders o WHERE o.t_id = t.id) l";
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(err, crate::error::SqlError::Unsupported { .. }),
                "{sql:?} -> {err:?}"
            );
        }
    }
}

// =======================================================================
// B1 -- binder: the scope chain
// =======================================================================

mod binder {
    use super::*;
    use crate::auth::AuthContext;
    use crate::limits::SqlLimits;
    use rubixdb::catalog::schema::{ObjectKind, Privilege};

    fn seeded(tag: &str) -> Db {
        Db::new(tag)
    }

    // ---- resolution -----------------------------------------------------

    #[test]
    fn a_correlated_reference_resolves_to_the_enclosing_query_and_is_recorded() {
        let db = seeded("b1_basic");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
        );
        let sq = where_subquery(&s);
        let outer_t = s.from[0].table.id;
        assert_eq!(sq.outer_refs.len(), 1, "exactly the one outer column: {:?}", sq.outer_refs);
        assert_eq!(sq.outer_refs[0].table_ref, outer_t);
        // t.id is column 0 of t.
        assert_eq!(sq.outer_refs[0].ordinal, 0);
        // The inner WHERE reads the *outer* table's reference, not its own.
        let mut cols = Vec::new();
        columns_of(sq.query.selection.as_ref().unwrap(), &mut cols);
        assert!(cols.iter().any(|c| c.table_ref == outer_t));
        assert!(cols.iter().any(|c| c.table_ref == sq.query.from[0].table.id));
        db.cleanup();
    }

    #[test]
    fn an_uncorrelated_subquery_still_has_no_outer_refs() {
        let db = seeded("b1_uncorr");
        let s = bound_select(&db, "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders)");
        assert!(where_subquery(&s).outer_refs.is_empty());
        // ...while the same shape with a correlating predicate is not.
        let c = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
        );
        assert_eq!(where_subquery(&c).outer_refs.len(), 1);
        db.cleanup();
    }

    #[test]
    fn an_unqualified_name_defined_by_the_subquery_itself_is_not_an_outer_reference() {
        // `id` exists in both `orders` (inner) and `t` (outer): the innermost
        // binding wins, so the subquery is NOT correlated.
        let db = seeded("b1_inner_wins");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE id = 10)",
        );
        let sq = where_subquery(&s);
        assert!(sq.outer_refs.is_empty(), "{:?}", sq.outer_refs);
        let mut cols = Vec::new();
        columns_of(sq.query.selection.as_ref().unwrap(), &mut cols);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].table_ref, sq.query.from[0].table.id, "bound to orders.id");
        // Twin: `customer` exists only inside, `name` only outside -> the
        // reference to `name` does resolve outward.
        let w = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE customer = name)",
        );
        assert_eq!(where_subquery(&w).outer_refs.len(), 1);
        db.cleanup();
    }

    #[test]
    fn an_unqualified_name_missing_from_the_subquery_resolves_outward() {
        // `name` exists only in `t`.
        let db = seeded("b1_outward");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE customer = name)",
        );
        let sq = where_subquery(&s);
        assert_eq!(sq.outer_refs.len(), 1);
        assert_eq!(sq.outer_refs[0].table_ref, s.from[0].table.id);
        assert_eq!(sq.outer_refs[0].ordinal, 1, "t.name");
        db.cleanup();
    }

    #[test]
    fn the_same_alias_at_two_levels_resolves_to_the_inner_one() {
        let db = seeded("b1_alias_shadow");
        let s = bound_select(
            &db,
            "SELECT 1 FROM t x WHERE EXISTS (SELECT 1 FROM orders x WHERE x.customer = 'c1')",
        );
        let sq = where_subquery(&s);
        assert!(sq.outer_refs.is_empty());
        let mut cols = Vec::new();
        columns_of(sq.query.selection.as_ref().unwrap(), &mut cols);
        assert_eq!(cols[0].table_ref, sq.query.from[0].table.id);
        assert_ne!(sq.query.from[0].table.id, s.from[0].table.id);
        // Twin: with distinct aliases the outer one IS reachable.
        let d = bound_select(
            &db,
            "SELECT 1 FROM t x WHERE EXISTS (SELECT 1 FROM orders y WHERE y.t_id = x.id)",
        );
        assert_eq!(where_subquery(&d).outer_refs.len(), 1);
        db.cleanup();
    }

    #[test]
    fn a_qualifier_that_names_an_inner_item_never_falls_through_to_an_outer_column() {
        // Inner `t` is `orders`, which has no `name`. The outer `t` does, but
        // the inner alias shadows it: this must be an error, not a silent
        // rebinding to the outer table.
        let db = seeded("b1_no_fallthrough");
        let err = db
            .try_bind("SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders t WHERE t.name = 'x')")
            .unwrap_err();
        assert!(
            matches!(err, SqlError::UnknownObject { kind: "column", .. }),
            "{err:?}"
        );
        // Twin: a qualifier the subquery does NOT define resolves outward.
        let s = bound_select(
            &db,
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders u WHERE u.customer = t.name)",
        );
        assert_eq!(where_subquery(&s).outer_refs.len(), 1);
        db.cleanup();
    }

    #[test]
    fn an_ambiguity_inside_the_inner_query_is_not_escaped_by_an_outer_match() {
        let db = seeded("b1_inner_ambiguity");
        let err = db
            .try_bind(
                "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders a JOIN orders b ON a.id = b.id WHERE id = 3)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::AmbiguousColumn { .. }), "{err:?}");
        // Twin: qualified (unambiguous) inner names plus an outer reference bind.
        let s = bound_select(
            &db,
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders a JOIN orders b ON a.id = b.id WHERE a.t_id = t.id)",
        );
        assert_eq!(where_subquery(&s).outer_refs.len(), 1);
        db.cleanup();
    }

    #[test]
    fn a_name_found_at_no_level_is_unknown() {
        let db = seeded("b1_unknown");
        for sql in [
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE zzz = t.id)",
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE t_id = t.zzz)",
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE t_id = nope.id)",
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE EXISTS (SELECT 1 WHERE qqq = 1))",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(matches!(err, SqlError::UnknownObject { .. }), "{sql:?} -> {err:?}");
        }
        // Each of the above is unknown only because of the typo: the corrected
        // statement (the name exists at some level) binds.
        for sql in [
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE customer = t.name)",
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE t_id = t.id)",
            "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE EXISTS (SELECT 1 WHERE customer = 'x'))",
        ] {
            assert!(db.try_bind(sql).is_ok(), "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn an_unknown_name_reports_the_same_error_as_without_an_enclosing_query() {
        let db = seeded("b1_unknown_same");
        let alone = db.try_bind("SELECT zzz FROM orders").unwrap_err();
        let nested = db
            .try_bind("SELECT 1 FROM t WHERE EXISTS (SELECT zzz FROM orders)")
            .unwrap_err();
        assert_eq!(alone, nested);
        // Twin: the same nesting with a resolvable correlation binds.
        assert!(db
            .try_bind("SELECT 1 FROM t WHERE EXISTS (SELECT customer FROM orders WHERE t_id = t.id)")
            .is_ok());
        db.cleanup();
    }

    // ---- the chain: three levels, innermost first ---------------------------

    #[test]
    fn with_two_enclosing_queries_defining_a_name_the_nearer_one_wins() {
        // `id` exists in t (level 2 out), in orders (level 1 out); the
        // innermost query has no FROM. The standard: innermost enclosing wins.
        let db = seeded("b1_chain_nearer");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE EXISTS (SELECT 1 WHERE id = 10))",
        );
        let mid = where_subquery(&s);
        let inner = where_subquery(&mid.query);
        assert_eq!(inner.outer_refs.len(), 1);
        assert_eq!(
            inner.outer_refs[0].table_ref, mid.query.from[0].table.id,
            "bound to orders (the nearer enclosing query), not t"
        );
        assert!(
            mid.outer_refs.is_empty(),
            "the middle query itself reads nothing from t: {:?}",
            mid.outer_refs
        );
        db.cleanup();
    }

    #[test]
    fn a_reference_two_levels_out_makes_every_query_in_between_correlated() {
        let db = seeded("b1_chain_through");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer = 'c1' AND \
             EXISTS (SELECT 1 FROM t t3 WHERE t3.name = t.name))",
        );
        let mid = where_subquery(&s);
        let mid_inner = {
            // the subquery is the right operand of the AND
            find_subquery(mid.query.selection.as_ref().unwrap()).expect("inner subquery")
        };
        let outer_t = s.from[0].table.id;
        assert_eq!(mid_inner.outer_refs.len(), 1);
        assert_eq!(mid_inner.outer_refs[0].table_ref, outer_t);
        assert!(
            mid.outer_refs.iter().any(|c| c.table_ref == outer_t),
            "the middle query is correlated to t because its child is: {:?}",
            mid.outer_refs
        );
        db.cleanup();
    }

    #[test]
    fn a_three_level_chain_records_each_levels_own_dependencies() {
        let db = seeded("b1_chain_each");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
             EXISTS (SELECT 1 FROM t t3 WHERE t3.id = o.t_id))",
        );
        let mid = where_subquery(&s);
        let inner = find_subquery(mid.query.selection.as_ref().unwrap()).unwrap();
        // inner reads o.t_id only
        assert_eq!(inner.outer_refs.len(), 1);
        assert_eq!(inner.outer_refs[0].table_ref, mid.query.from[0].table.id);
        // mid reads t.id only (its child reads mid's own table)
        assert_eq!(mid.outer_refs.len(), 1);
        assert_eq!(mid.outer_refs[0].table_ref, s.from[0].table.id);
        db.cleanup();
    }

    #[test]
    fn table_reference_ids_stay_unique_across_a_correlated_statement() {
        let db = seeded("b1_unique_refs");
        // The same table used at both levels, correlated through aliases.
        let s = bound_select(
            &db,
            "SELECT id FROM t a WHERE EXISTS (SELECT 1 FROM t b WHERE b.id = a.id)",
        );
        let sq = where_subquery(&s);
        assert_ne!(s.from[0].table.id, sq.query.from[0].table.id);
        assert_eq!(sq.outer_refs.len(), 1);
        assert_eq!(sq.outer_refs[0].table_ref, s.from[0].table.id, "the outer alias's own ref");
        db.cleanup();
    }

    // ---- what is recorded -----------------------------------------------------

    #[test]
    fn the_outer_row_is_labelled_by_its_primary_key_even_if_it_correlates_on_another_column() {
        let db = seeded("b1_labels");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.customer = t.name)",
        );
        let sq = where_subquery(&s);
        assert_eq!(sq.outer_refs.len(), 1, "only t.name is read");
        let labels: Vec<&str> = sq.outer_labels.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"t.name"), "{labels:?}");
        assert!(labels.contains(&"t.id"), "the primary key names the row: {labels:?}");
        db.cleanup();
    }

    #[test]
    fn a_correlated_in_keeps_its_outer_operand_and_records_the_inner_correlation() {
        let db = seeded("b1_in");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE orders.customer = t.name)",
        );
        let sq = where_subquery(&s);
        assert!(matches!(sq.kind, SubqueryKind::In { negated: false }));
        assert!(sq.operand.is_some());
        assert_eq!(sq.outer_refs.len(), 1);
        db.cleanup();
    }

    #[test]
    fn single_select_numbering_is_unchanged_by_the_scope_chain() {
        let db = seeded("b1_numbering");
        let s = bound_select(&db, "SELECT t.id FROM t JOIN orders ON orders.t_id = t.id");
        assert_eq!(s.from[0].table.id.0, 0);
        assert_eq!(s.from[1].table.id.0, 1);
        db.cleanup();
    }

    // ---- typing across scopes -------------------------------------------------

    #[test]
    fn types_are_checked_across_the_scope_boundary() {
        let db = seeded("b1_types");
        // INTEGER vs TEXT, with the TEXT side from the enclosing query.
        let err = db
            .try_bind("SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.amount = t.name)")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // Same types across the boundary are fine.
        assert!(db
            .try_bind("SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)")
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn an_outer_column_of_a_left_joined_table_is_nullable_in_the_subquery() {
        let db = seeded("b1_nullext");
        let s = bound_select(
            &db,
            "SELECT t.id FROM t LEFT JOIN orders o ON o.t_id = t.id \
             WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.id = o.t_id)",
        );
        let sq = where_subquery(&s);
        let mut cols = Vec::new();
        columns_of(sq.query.selection.as_ref().unwrap(), &mut cols);
        assert!(cols.iter().any(|c| c.table_ref == s.from[1].table.id));
        db.cleanup();
    }

    // ---- aggregates -----------------------------------------------------------

    #[test]
    fn an_aggregate_over_a_correlated_subquery_binds_over_the_inner_rows() {
        let db = seeded("b1_agg");
        let s = bound_select(
            &db,
            "SELECT id, (SELECT MAX(amount) FROM orders WHERE orders.t_id = t.id) FROM t",
        );
        let BoundExprKind::Subquery(sq) = &s.projection[1].expr.kind else {
            panic!("expected a subquery in the select list");
        };
        assert_eq!(sq.query.aggregates.len(), 1);
        assert_eq!(sq.outer_refs.len(), 1);
        // The outer statement is not aggregated.
        assert!(s.aggregates.is_empty());
        db.cleanup();
    }

    #[test]
    fn an_outer_column_beside_an_aggregate_in_the_inner_projection_is_a_constant() {
        // Without treating the outer column as a constant this would be
        // rejected as an ungrouped column.
        let db = seeded("b1_agg_const");
        assert!(db
            .try_bind("SELECT (SELECT MAX(amount) + t.id FROM orders WHERE orders.t_id = t.id) FROM t")
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn an_inner_own_column_beside_an_aggregate_is_still_rejected() {
        let db = seeded("b1_agg_ungrouped");
        let err = db
            .try_bind("SELECT (SELECT MAX(amount) + orders.id FROM orders WHERE orders.t_id = t.id) FROM t")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn an_aggregate_over_only_outer_columns_is_refused_not_silently_mis_scoped() {
        let db = seeded("b1_agg_outer_only");
        let err = db
            .try_bind("SELECT (SELECT COUNT(t.id) FROM orders) FROM t")
            .unwrap_err();
        assert!(matches!(err, SqlError::Unsupported { .. }), "{err:?}");
        // A mixed argument belongs to the inner query and is fine.
        assert!(db
            .try_bind("SELECT (SELECT MAX(orders.amount + t.id) FROM orders) FROM t")
            .is_ok());
        // A constant argument is fine.
        assert!(db.try_bind("SELECT (SELECT MAX(1) FROM orders) FROM t").is_ok());
        db.cleanup();
    }

    #[test]
    fn a_correlated_subquery_in_a_grouped_query_requires_its_outer_columns_to_be_grouped() {
        let db = seeded("b1_agg_group");
        let err = db
            .try_bind(
                "SELECT active, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t GROUP BY active",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // Grouping by the correlated column makes it legal.
        assert!(db
            .try_bind(
                "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t GROUP BY id",
            )
            .is_ok());
        db.cleanup();
    }

    // ---- other clauses ----------------------------------------------------------

    #[test]
    fn a_correlated_reference_is_accepted_in_having_order_by_and_the_select_list() {
        let db = seeded("b1_clauses");
        for sql in [
            "SELECT (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t",
            "SELECT id FROM t ORDER BY (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id)",
            "SELECT id FROM t WHERE (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) > 1",
            "SELECT active FROM t GROUP BY active HAVING EXISTS (SELECT 1 FROM orders WHERE orders.t_id = 1 AND t.name = 'x')",
        ] {
            let r = db.try_bind(sql);
            // The last statement reads an ungrouped column inside the
            // subquery and must be refused; the rest are plain.
            if sql.contains("HAVING") {
                assert!(r.is_err(), "{sql}");
            } else {
                assert!(r.is_ok(), "{sql} -> {:?}", r.err());
            }
        }
        db.cleanup();
    }

    #[test]
    fn an_outer_reference_in_an_inner_join_condition_binds() {
        let db = seeded("b1_on");
        let s = bound_select(
            &db,
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders a JOIN orders b ON b.t_id = a.t_id AND b.amount = t.id)",
        );
        let sq = where_subquery(&s);
        assert_eq!(sq.outer_refs.len(), 1);
        db.cleanup();
    }

    #[test]
    fn a_correlated_reference_in_update_and_delete_where_resolves_to_the_target() {
        let db = seeded("b1_dml");
        for sql in [
            "UPDATE t SET name = 'x' WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
            "DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
        ] {
            let bound = db.try_bind(sql).unwrap();
            let selection = match &bound {
                BoundStatement::Update(u) => u.selection.as_ref().unwrap(),
                BoundStatement::Delete(d) => d.selection.as_ref().unwrap(),
                other => panic!("{other:?}"),
            };
            let BoundExprKind::Subquery(sq) = &selection.kind else {
                panic!("expected a subquery");
            };
            assert_eq!(sq.outer_refs.len(), 1, "{sql}");
        }
        db.cleanup();
    }

    // ---- authorization ------------------------------------------------------------

    fn grant_select(db: &Db, principal: &str, table: &str) {
        let id = db.table_id(table);
        db.catalog
            .grant(principal, ObjectKind::Table, id, Privilege::Select)
            .unwrap();
    }

    #[test]
    fn a_forbidden_table_inside_a_correlated_subquery_fails_at_bind_like_a_missing_one() {
        let db = seeded("b1_authz");
        grant_select(&db, "limited", "t"); // may read t, not orders
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        let forbidden = db
            .try_bind_with(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
                &auth,
                &limits,
            )
            .unwrap_err();
        let missing = db
            .try_bind_with(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM no_such WHERE no_such.t_id = t.id)",
                &auth,
                &limits,
            )
            .unwrap_err();
        assert!(matches!(
            (&forbidden, &missing),
            (
                SqlError::UnknownObject { kind: "table", .. },
                SqlError::UnknownObject { kind: "table", .. }
            )
        ));
        // Indistinguishable apart from the name the caller itself typed.
        let (SqlError::UnknownObject { detail: d1, .. }, SqlError::UnknownObject { detail: d2, .. }) =
            (&forbidden, &missing)
        else {
            unreachable!()
        };
        assert_eq!(d1, "orders");
        assert_eq!(d2, "no_such");
        // With the grant the very same correlated statement binds, so the
        // refusal above is the authorization check and nothing else.
        grant_select(&db, "limited", "orders");
        assert!(db
            .try_bind_with(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
                &auth,
                &limits,
            )
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn authorization_is_checked_for_every_level_of_a_nested_correlated_chain() {
        let db = seeded("b1_authz_deep");
        grant_select(&db, "limited", "t"); // t only
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        // The forbidden table is two levels down; the correlation reaches it.
        let err = db
            .try_bind_with(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.id = t.id AND \
                 EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t2.id))",
                &auth,
                &limits,
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{err:?}");
        // And the permitted-only chain still binds.
        assert!(db
            .try_bind_with(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.id = t.id)",
                &auth,
                &limits
            )
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn the_outer_table_being_forbidden_is_the_same_error_with_or_without_a_subquery() {
        let db = seeded("b1_authz_outer");
        grant_select(&db, "limited", "orders"); // orders only
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        let with = db
            .try_bind_with(
                "SELECT 1 FROM orders WHERE EXISTS (SELECT 1 FROM t WHERE t.id = orders.t_id)",
                &auth,
                &limits,
            )
            .unwrap_err();
        let without = db.try_bind_with("SELECT 1 FROM t", &auth, &limits).unwrap_err();
        assert!(matches!(with, SqlError::UnknownObject { kind: "table", .. }), "{with:?}");
        assert!(matches!(without, SqlError::UnknownObject { kind: "table", .. }));
        // Granting the inner table makes the correlated statement bind.
        grant_select(&db, "limited", "t");
        assert!(db
            .try_bind_with(
                "SELECT 1 FROM orders WHERE EXISTS (SELECT 1 FROM t WHERE t.id = orders.t_id)",
                &auth,
                &limits,
            )
            .is_ok());
        db.cleanup();
    }

    // ---- limits still apply -----------------------------------------------------------

    #[test]
    fn nesting_depth_and_subquery_count_limits_still_apply_to_correlated_queries() {
        let db = seeded("b1_limits");
        let limits = SqlLimits {
            max_subquery_depth: 1,
            ..SqlLimits::default()
        };
        let err = db
            .try_bind_with(
                "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
                 EXISTS (SELECT 1 FROM t t3 WHERE t3.id = o.t_id))",
                &AuthContext::admin("p"),
                &limits,
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// Shared data for the execution modules: 60 outer rows, 222 inner rows
// =======================================================================

pub(crate) mod data {
    use super::*;

    pub(crate) type TRow = (i32, Option<String>, Option<bool>);
    pub(crate) type ORow = (i64, String, Option<i32>, Option<i32>);

    /// `t`: ids 1..=60; `name` repeats every 12 ids (so a correlation on it
    /// matches several rows) and is NULL when `id % 9 == 0`; `active` is NULL
    /// when `id % 5 == 0`.
    ///
    /// `orders`: row `i` of `t` has `i % 8` orders (0..=7, so every
    /// cardinality 0 / 1 / many occurs), `customer` cycles over the same
    /// twelve names `t.name` uses, `amount` is NULL for about a seventh of
    /// the rows. Plus 10 orphan orders (t_id with no `t` row) and 6 orders
    /// with a NULL t_id. 206 + 16 = 222 rows.
    pub(crate) fn standard() -> (Vec<TRow>, Vec<ORow>) {
        let mut t = Vec::new();
        for id in 1..=60i32 {
            let name = if id % 9 == 0 { None } else { Some(format!("n{}", id % 12)) };
            let active = if id % 5 == 0 { None } else { Some(id % 2 == 0) };
            t.push((id, name, active));
        }
        let mut orders = Vec::new();
        for i in 1..=60i32 {
            for j in 0..(i % 8) {
                let amount = if (i * 3 + j) % 7 == 0 { None } else { Some((i * 13 + j * 29) % 100) };
                orders.push((
                    1000 + i64::from(i) * 10 + i64::from(j),
                    format!("n{}", (i + j) % 12),
                    amount,
                    Some(i),
                ));
            }
        }
        for k in 0..10i32 {
            orders.push((9000 + i64::from(k), "orphan".to_string(), Some(k * 5), Some(100 + k)));
        }
        for k in 0..6i32 {
            orders.push((9100 + i64::from(k), format!("n{k}"), Some(k * 7), None));
        }
        (t, orders)
    }

    pub(crate) fn seed(db: &Db, t: &[TRow], orders: &[ORow]) {
        let t_rows: Vec<(i32, Option<&str>, Option<bool>)> =
            t.iter().map(|(i, n, a)| (*i, n.as_deref(), *a)).collect();
        db.seed_t(&t_rows);
        let o_rows: Vec<(i64, &str, Option<i32>, Option<i32>)> =
            orders.iter().map(|(i, c, a, t)| (*i, c.as_str(), *a, *t)).collect();
        db.seed_orders(&o_rows);
    }

    pub(crate) fn standard_db(tag: &str) -> (Db, Vec<TRow>, Vec<ORow>) {
        let db = Db::new(tag);
        let (t, orders) = standard();
        seed(&db, &t, &orders);
        (db, t, orders)
    }

    /// Orders grouped by `t_id` -- the reference model's "join index".
    pub(crate) fn by_t_id(orders: &[ORow]) -> std::collections::BTreeMap<i32, Vec<&ORow>> {
        let mut m: std::collections::BTreeMap<i32, Vec<&ORow>> = std::collections::BTreeMap::new();
        for o in orders {
            if let Some(t) = o.3 {
                m.entry(t).or_default().push(o);
            }
        }
        m
    }

    pub(crate) fn opt_int(v: Option<i32>) -> V {
        v.map(int).unwrap_or(NULL)
    }

    pub(crate) fn id_of(row: &Row) -> i32 {
        match &row[0] {
            Some(rubixdb::relational::RelationalValue::Integer(n)) => *n,
            other => panic!("expected an INTEGER id, got {other:?}"),
        }
    }
}

// =======================================================================
// B2 -- execution: correlated scalar subquery, re-run per outer row
// =======================================================================

mod scalar {
    use super::data::*;
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};

    #[test]
    fn a_correlated_max_matches_a_btreemap_join_for_every_outer_row() {
        let (db, t, orders) = standard_db("b2_max");
        let idx = by_t_id(&orders);
        let got = db.rows(
            "SELECT id, (SELECT MAX(amount) FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .map(|(id, _, _)| {
                let max = idx
                    .get(id)
                    .and_then(|os| os.iter().filter_map(|o| o.2).max());
                vec![int(*id), opt_int(max)]
            })
            .collect();
        assert_eq!(got.len(), 60);
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_correlated_count_is_zero_not_null_when_nothing_matches() {
        let (db, t, orders) = standard_db("b2_count");
        let idx = by_t_id(&orders);
        let got = db.rows(
            "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .map(|(id, _, _)| vec![int(*id), big(idx.get(id).map_or(0, |v| v.len()) as i64)])
            .collect();
        assert_eq!(got, want);
        assert!(got.iter().any(|r| r[1] == big(0)), "the fixture has unmatched outer rows");
        db.cleanup();
    }

    #[test]
    fn the_inner_aggregate_ranges_over_the_inner_rows_only_not_the_outer_table() {
        // SUM over all 222 orders would be one number for every row; the
        // correlated SUM differs per outer row.
        let (db, _t, orders) = standard_db("b2_scope");
        let idx = by_t_id(&orders);
        let got = db.rows(
            "SELECT id, (SELECT SUM(amount) FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        );
        let distinct: std::collections::BTreeSet<String> =
            got.iter().map(|r| format!("{:?}", r[1])).collect();
        assert!(distinct.len() > 10, "per-row sums, not one global value: {}", distinct.len());
        for r in &got {
            let id = id_of(r);
            let vals: Vec<i64> = idx
                .get(&id)
                .map(|os| os.iter().filter_map(|o| o.2).map(i64::from).collect())
                .unwrap_or_default();
            let sum: Option<i64> = if vals.is_empty() { None } else { Some(vals.iter().sum()) };
            assert_eq!(r[1], sum.map(big).unwrap_or(NULL), "id {id}");
        }
        db.cleanup();
    }

    #[test]
    fn a_correlated_scalar_returns_null_for_zero_rows_the_value_for_one_and_errors_for_many() {
        let (db, _t, orders) = standard_db("b2_cardinality");
        let idx = by_t_id(&orders);
        // id 8 has 0 orders, id 9 has 1, id 10 has 2 (i % 8).
        assert_eq!(idx.get(&8).map_or(0, |v| v.len()), 0);
        assert_eq!(idx.get(&9).map_or(0, |v| v.len()), 1);
        assert_eq!(idx.get(&10).map_or(0, |v| v.len()), 2);
        let sql = |id: i32| {
            format!("SELECT id, (SELECT customer FROM orders WHERE orders.t_id = t.id) FROM t WHERE id = {id}")
        };
        assert_eq!(db.rows(&sql(8)), vec![vec![int(8), NULL]]);
        let one = idx[&9][0].1.clone();
        assert_eq!(db.rows(&sql(9)), vec![vec![int(9), text(&one)]]);
        let err = db.try_run(&sql(10)).unwrap_err();
        assert!(
            matches!(err, SqlError::ExecutionParameter { .. }),
            "a runtime error, not a bind error: {err:?}"
        );
        db.cleanup();
    }

    #[test]
    fn the_many_rows_error_names_the_outer_row_that_caused_it() {
        let (db, _t, _orders) = standard_db("b2_names_row");
        // Rows 8 and 9 evaluate fine; 10 is the first with two inner rows.
        let err = db
            .try_run(
                "SELECT id, (SELECT customer FROM orders WHERE orders.t_id = t.id) FROM t WHERE id IN (8, 9, 10, 11)",
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("t.id = 10"), "the offending row's key is named: {msg}");
        assert!(!msg.contains("t.id = 11"), "later rows were never reached: {msg}");
        db.cleanup();
    }

    #[test]
    fn the_error_names_the_primary_key_even_when_correlating_on_another_column() {
        let (db, _t, _orders) = standard_db("b2_names_pk");
        // Correlate on t.name: several orders share a customer name, so the
        // subquery returns many rows; the message must still carry t.id.
        let err = db
            .try_run("SELECT id, (SELECT amount FROM orders WHERE orders.customer = t.name) FROM t WHERE id = 1")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("t.name = \"n1\""), "{msg}");
        assert!(msg.contains("t.id = 1"), "{msg}");
        db.cleanup();
    }

    #[test]
    fn a_text_value_in_the_error_message_cannot_inject_terminal_escapes() {
        let db = Db::new("b2_escape");
        let hostile = "a\u{1b}[31mred\nline";
        db.seed_t(&[(1, Some(hostile), Some(true))]);
        db.seed_orders(&[(1, hostile, Some(1), None), (2, hostile, Some(2), None)]);
        // Correlated on the TEXT column, so its value is what the message shows.
        let err = db
            .try_run("SELECT (SELECT amount FROM orders WHERE orders.customer = t.name) FROM t")
            .unwrap_err();
        // It must be the RUNTIME multi-row error (not, say, a bind error that
        // happens to contain no escape): the value is named, and escaped.
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("t.name = "), "the row is named: {msg}");
        assert!(!msg.contains('\u{1b}') && !msg.contains('\n'), "{msg:?}");
        assert!(msg.contains("\\u{1b}") && msg.contains("\\n"), "escaped, not dropped: {msg}");
        db.cleanup();
    }

    #[test]
    fn an_outer_column_that_is_not_in_the_outer_select_list_is_still_available() {
        let (db, t, orders) = standard_db("b2_hidden_col");
        let idx = by_t_id(&orders);
        let got = db.rows_sorted(
            "SELECT name FROM t WHERE (SELECT MAX(amount) FROM orders WHERE orders.t_id = t.id) > 90",
        );
        let mut want: Vec<Row> = t
            .iter()
            .filter(|(id, _, _)| {
                idx.get(id)
                    .and_then(|os| os.iter().filter_map(|o| o.2).max())
                    .is_some_and(|m| m > 90)
            })
            .map(|(_, n, _)| vec![n.as_deref().map(text).unwrap_or(NULL)])
            .collect();
        want.sort_by_key(|r| format!("{r:?}"));
        assert!(!want.is_empty());
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_from_less_subquery_reads_the_outer_row() {
        let (db, t, _) = standard_db("b2_from_less");
        let got = db.rows("SELECT id, (SELECT t.id + 1000) FROM t ORDER BY id");
        let want: Vec<Row> = t.iter().map(|(id, _, _)| vec![int(*id), int(id + 1000)]).collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn an_aggregate_over_zero_inner_rows_still_reads_the_outer_row() {
        // COUNT(*) is 0 for the unmatched rows; the THEN branch (`t.id`) needs the outer row
        // even though the inner side produced no row to carry it.
        let (db, t, orders) = standard_db("b2_empty_group");
        let idx = by_t_id(&orders);
        let got = db.rows(
            "SELECT id, (SELECT CASE WHEN COUNT(*) = 0 THEN t.id ELSE 0 END FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .map(|(id, _, _)| {
                vec![int(*id), int(if idx.get(id).map_or(0, |v| v.len()) == 0 { *id } else { 0 })]
            })
            .collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_correlated_scalar_works_in_where_order_by_and_having() {
        let (db, t, orders) = standard_db("b2_clauses");
        let idx = by_t_id(&orders);
        let count = |id: i32| idx.get(&id).map_or(0, |v| v.len());
        // WHERE
        let got = db.rows(
            "SELECT id FROM t WHERE (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) >= 5 ORDER BY id",
        );
        let want: Vec<Row> =
            t.iter().filter(|(i, _, _)| count(*i) >= 5).map(|(i, _, _)| vec![int(*i)]).collect();
        assert_eq!(got, want);
        // ORDER BY (descending by count, then id)
        let got = db.rows(
            "SELECT id FROM t ORDER BY (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) DESC, id",
        );
        let mut ordered: Vec<(usize, i32)> = t.iter().map(|(i, _, _)| (count(*i), *i)).collect();
        ordered.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let want: Vec<Row> = ordered.into_iter().map(|(_, i)| vec![int(i)]).collect();
        assert_eq!(got, want);
        // HAVING (the correlated column is the grouping column)
        let got = db.rows(
            "SELECT id FROM t GROUP BY id HAVING (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) = 3 ORDER BY id",
        );
        let want: Vec<Row> =
            t.iter().filter(|(i, _, _)| count(*i) == 3).map(|(i, _, _)| vec![int(*i)]).collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn the_inner_query_is_re_run_once_per_outer_row_and_never_cached() {
        let (db, _t, _o) = standard_db("b2_metrics");
        let metrics = ExecMetrics::default();
        db.try_run_with(
            "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t",
            &ExecLimits::default(),
            &metrics,
            &[],
        )
        .unwrap();
        let m = metrics.snapshot();
        assert_eq!(m.correlated_subquery_reexecs, 60, "{m:?}");
        assert_eq!(m.subquery_executions, 60, "{m:?}");
        assert_eq!(m.subquery_cache_hits, 0, "a correlated result is never reused: {m:?}");
        db.cleanup();
    }

    #[test]
    fn an_uncorrelated_subquery_is_not_counted_as_a_correlated_re_execution() {
        let (db, _t, _o) = standard_db("b2_metrics_uncorr");
        let metrics = ExecMetrics::default();
        db.try_run_with(
            "SELECT id, (SELECT COUNT(*) FROM orders) FROM t",
            &ExecLimits::default(),
            &metrics,
            &[],
        )
        .unwrap();
        let m = metrics.snapshot();
        assert_eq!(m.correlated_subquery_reexecs, 0, "{m:?}");
        assert_eq!(m.subquery_executions, 1, "{m:?}");
        // Twin: the correlated form of the same query IS counted, per outer row.
        let correlated = ExecMetrics::default();
        db.try_run_with(
            "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t",
            &ExecLimits::default(),
            &correlated,
            &[],
        )
        .unwrap();
        assert_eq!(correlated.snapshot().correlated_subquery_reexecs, 60);
        db.cleanup();
    }

    const CORRELATED_60: &str =
        "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t";

    #[test]
    fn max_correlated_reexecutions_is_enforced_and_exact() {
        let (db, _t, _o) = standard_db("b2_limit_total");
        let run = |n: u64| {
            db.try_run_with(
                CORRELATED_60,
                &ExecLimits { max_correlated_reexecutions: n, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
        };
        assert!(run(60).is_ok(), "exactly 60 evaluations fit a limit of 60");
        let err = run(59).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_correlated_reexecutions"), "{err}");
        db.cleanup();
    }

    #[test]
    fn max_subquery_outer_rows_is_enforced_and_exact() {
        let (db, _t, _o) = standard_db("b2_limit_rows");
        let run = |n: u64| {
            db.try_run_with(
                CORRELATED_60,
                &ExecLimits { max_subquery_outer_rows: n, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
        };
        assert!(run(60).is_ok());
        let err = run(59).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_subquery_outer_rows"), "{err}");
        db.cleanup();
    }

    #[test]
    fn the_per_subquery_limit_counts_each_subquery_separately() {
        // Two correlated subqueries, 60 evaluations each: a per-subquery limit
        // of 60 passes (120 evaluations in total), a statement-wide limit of
        // 60 does not.
        let (db, _t, _o) = standard_db("b2_limit_separate");
        let sql = "SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id), \
                   (SELECT MAX(amount) FROM orders WHERE orders.t_id = t.id) FROM t";
        let per = ExecLimits { max_subquery_outer_rows: 60, ..ExecLimits::default() };
        assert!(db.try_run_with(sql, &per, &ExecMetrics::default(), &[]).is_ok());
        let total = ExecLimits { max_correlated_reexecutions: 60, ..ExecLimits::default() };
        assert!(matches!(
            db.try_run_with(sql, &total, &ExecMetrics::default(), &[]).unwrap_err(),
            SqlError::ResourceLimit { .. }
        ));
        db.cleanup();
    }

    #[test]
    fn an_expired_deadline_stops_a_correlated_query_with_the_deadline_error() {
        let (db, _t, _o) = standard_db("b2_deadline");
        let limits = ExecLimits {
            deadline: Some(std::time::Duration::from_millis(1)),
            ..ExecLimits::default()
        };
        // 60 outer rows x a 222-row inner scan is far more than 1 ms of work.
        let err = db
            .try_run_with(CORRELATED_60, &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert_eq!(err, SqlError::DeadlineExceeded, "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// Randomized differential tests against an independent reference model
// =======================================================================
//
// The model never touches the SQL engine: it evaluates the correlation
// predicate row by row in Rust under explicit three-valued logic
// (`Option<bool>`, `None` = UNKNOWN) and builds the expected answer from
// plain `Vec`/`BTreeMap` operations. A SQL result that disagrees with it is a
// bug in one of them, and the model is small enough to check by reading.

mod diff {
    use super::data::*;
    use super::*;
    use std::collections::BTreeMap;

    /// Deterministic generator (SplitMix64) -- no external dependency.
    pub(super) struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        pub fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        pub fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }

    /// Random data with NULLs in every nullable column and values drawn from
    /// small domains so correlations match often (and sometimes not at all).
    pub(super) fn random_data(seed: u64) -> (Vec<TRow>, Vec<ORow>) {
        let mut rng = Rng(seed);
        let names = ["a", "b", "c", "d"];
        let t_count = 12 + rng.below(10) as i32;
        let mut t = Vec::new();
        for id in 1..=t_count {
            let name = if rng.chance(25) { None } else { Some(names[rng.below(4) as usize].to_string()) };
            let active = match rng.below(3) {
                0 => None,
                1 => Some(true),
                _ => Some(false),
            };
            t.push((id, name, active));
        }
        let o_count = 30 + rng.below(40) as i64;
        let mut orders = Vec::new();
        for k in 0..o_count {
            let t_id = if rng.chance(15) { None } else { Some(1 + rng.below(t_count as u64 + 3) as i32) };
            let amount = if rng.chance(20) { None } else { Some(rng.below(30) as i32) };
            orders.push((5000 + k, names[rng.below(4) as usize].to_string(), amount, t_id));
        }
        (t, orders)
    }

    pub(super) type Tri = Option<bool>;
    pub(super) fn and3(a: Tri, b: Tri) -> Tri {
        match (a, b) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        }
    }
    pub(super) fn or3(a: Tri, b: Tri) -> Tri {
        match (a, b) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        }
    }
    pub(super) fn not3(a: Tri) -> Tri {
        a.map(|b| !b)
    }
    pub(super) fn eq3<T: PartialEq>(a: Option<T>, b: Option<T>) -> Tri {
        match (a, b) {
            (Some(x), Some(y)) => Some(x == y),
            _ => None,
        }
    }

    /// A correlation predicate between an inner `orders` row and the outer
    /// `t` row: its SQL text and its model.
    pub(super) struct Pred {
        pub sql: &'static str,
        pub model: fn(&TRow, &ORow) -> Tri,
    }

    pub(super) fn predicates() -> Vec<Pred> {
        vec![
            Pred {
                sql: "orders.t_id = t.id",
                model: |t, o| eq3(o.3, Some(t.0)),
            },
            Pred {
                sql: "orders.customer = t.name",
                model: |t, o| eq3(Some(o.1.clone()), t.1.clone()),
            },
            Pred {
                sql: "orders.amount > t.id",
                model: |t, o| o.2.map(|a| a > t.0),
            },
            Pred {
                sql: "orders.t_id = t.id AND orders.amount IS NOT NULL",
                model: |t, o| and3(eq3(o.3, Some(t.0)), Some(o.2.is_some())),
            },
            Pred {
                sql: "orders.t_id = t.id OR orders.customer = t.name",
                model: |t, o| or3(eq3(o.3, Some(t.0)), eq3(Some(o.1.clone()), t.1.clone())),
            },
            Pred {
                sql: "NOT (orders.t_id = t.id)",
                model: |t, o| not3(eq3(o.3, Some(t.0))),
            },
        ]
    }

    /// The inner rows a predicate keeps for one outer row (`TRUE` only).
    pub(super) fn matching<'a>(p: &Pred, t: &TRow, orders: &'a [ORow]) -> Vec<&'a ORow> {
        orders.iter().filter(|o| (p.model)(t, o) == Some(true)).collect()
    }

    pub(super) fn run_seed<F: FnMut(&Db, &[TRow], &[ORow], u64)>(tag: &str, seeds: u64, mut f: F) {
        for seed in 0..seeds {
            let db = Db::new(&format!("{tag}_{seed}"));
            let (t, orders) = random_data(seed.wrapping_mul(0x1234_5678_9ABC_DEF1).wrapping_add(7));
            seed_all(&db, &t, &orders);
            f(&db, &t, &orders, seed);
            db.cleanup();
        }
    }

    fn seed_all(db: &Db, t: &[TRow], orders: &[ORow]) {
        super::data::seed(db, t, orders);
    }

    #[test]
    fn correlated_scalar_aggregates_match_the_reference_model_on_random_data() {
        run_seed("diff_scalar", 20, |db, t, orders, seed| {
            for p in predicates() {
                for (agg, model) in [
                    ("COUNT(*)", 0u8),
                    ("MAX(amount)", 1),
                    ("MIN(amount)", 2),
                    ("SUM(amount)", 3),
                    ("COUNT(amount)", 4),
                ] {
                    let sql = format!(
                        "SELECT id, (SELECT {agg} FROM orders WHERE {}) FROM t ORDER BY id",
                        p.sql
                    );
                    let got = db.rows(&sql);
                    let mut want: Vec<Row> = Vec::new();
                    for trow in t {
                        let m = matching(&p, trow, orders);
                        let amounts: Vec<i32> = m.iter().filter_map(|o| o.2).collect();
                        let v: V = match model {
                            0 => big(m.len() as i64),
                            1 => opt_int(amounts.iter().copied().max()),
                            2 => opt_int(amounts.iter().copied().min()),
                            3 => {
                                if amounts.is_empty() {
                                    NULL
                                } else {
                                    big(amounts.iter().map(|a| i64::from(*a)).sum())
                                }
                            }
                            _ => big(amounts.len() as i64),
                        };
                        want.push(vec![int(trow.0), v]);
                    }
                    assert_eq!(got, want, "seed {seed}: {sql}");
                }
            }
        });
    }

    #[test]
    fn a_correlated_scalar_compared_in_where_matches_the_reference_model() {
        run_seed("diff_scalar_where", 15, |db, t, orders, seed| {
            for p in predicates() {
                let sql = format!(
                    "SELECT id FROM t WHERE (SELECT MAX(amount) FROM orders WHERE {}) > 10 ORDER BY id",
                    p.sql
                );
                let got = db.rows(&sql);
                let want: Vec<Row> = t
                    .iter()
                    .filter(|trow| {
                        matching(&p, trow, orders)
                            .iter()
                            .filter_map(|o| o.2)
                            .max()
                            .is_some_and(|m| m > 10)
                    })
                    .map(|trow| vec![int(trow.0)])
                    .collect();
                assert_eq!(got, want, "seed {seed}: {sql}");
            }
        });
    }

    #[test]
    fn the_model_groups_like_a_btreemap_join() {
        // Sanity check of the model itself: for the equi-join predicate the
        // generic filter agrees with an explicit BTreeMap join.
        let (t, orders) = random_data(99);
        let idx: BTreeMap<i32, Vec<&ORow>> = by_t_id(&orders);
        let p = &predicates()[0];
        for trow in &t {
            let via_model = matching(p, trow, &orders);
            let via_map = idx.get(&trow.0).cloned().unwrap_or_default();
            assert_eq!(via_model, via_map);
        }
    }
}

// =======================================================================
// B3 -- execution: correlated EXISTS / NOT EXISTS
// =======================================================================

mod exists {
    use super::data::*;
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};

    fn ids_of(rows: &[Row]) -> Vec<i32> {
        rows.iter().map(id_of).collect()
    }

    #[test]
    fn correlated_exists_and_not_exists_match_a_btreemap_join_and_partition_the_outer_rows() {
        let (db, t, orders) = standard_db("b3_basic");
        let idx = by_t_id(&orders);
        let exists = ids_of(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id) ORDER BY id",
        ));
        let not_exists = ids_of(&db.rows(
            "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id) ORDER BY id",
        ));
        let want_exists: Vec<i32> =
            t.iter().filter(|(i, _, _)| idx.contains_key(i)).map(|(i, _, _)| *i).collect();
        let want_not: Vec<i32> =
            t.iter().filter(|(i, _, _)| !idx.contains_key(i)).map(|(i, _, _)| *i).collect();
        assert_eq!(exists, want_exists);
        assert_eq!(not_exists, want_not);
        assert!(!exists.is_empty() && !not_exists.is_empty());
        // EXISTS and NOT EXISTS are exact complements: every outer row in one.
        assert_eq!(exists.len() + not_exists.len(), 60);
        db.cleanup();
    }

    #[test]
    fn exists_is_never_unknown_when_the_inner_where_involves_null() {
        // orders.amount is NULL for some rows, so `amount > 50` is UNKNOWN for
        // them and they do not qualify -- but EXISTS itself is a plain
        // TRUE/FALSE, so NOT EXISTS is exactly the complement.
        let (db, t, orders) = standard_db("b3_null_inner");
        let idx = by_t_id(&orders);
        let qualifies = |id: &i32| {
            idx.get(id).is_some_and(|os| os.iter().any(|o| o.2.is_some_and(|a| a > 50)))
        };
        let e = ids_of(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND orders.amount > 50) ORDER BY id",
        ));
        let n = ids_of(&db.rows(
            "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND orders.amount > 50) ORDER BY id",
        ));
        assert_eq!(e, t.iter().map(|r| r.0).filter(|i| qualifies(i)).collect::<Vec<_>>());
        assert_eq!(n, t.iter().map(|r| r.0).filter(|i| !qualifies(i)).collect::<Vec<_>>());
        assert_eq!(e.len() + n.len(), 60);
        db.cleanup();
    }

    #[test]
    fn a_null_on_the_outer_correlated_column_makes_exists_false_and_not_exists_true() {
        // t.name is NULL for ids divisible by 9: `orders.customer = NULL` is
        // UNKNOWN for every inner row, so EXISTS is FALSE and NOT EXISTS TRUE.
        let (db, t, _o) = standard_db("b3_null_outer");
        let null_ids: Vec<i32> =
            t.iter().filter(|r| r.1.is_none()).map(|r| r.0).collect();
        assert!(!null_ids.is_empty());
        let e = ids_of(&db.rows(
            "SELECT id FROM t WHERE name IS NULL AND EXISTS (SELECT 1 FROM orders WHERE orders.customer = t.name) ORDER BY id",
        ));
        let n = ids_of(&db.rows(
            "SELECT id FROM t WHERE name IS NULL AND NOT EXISTS (SELECT 1 FROM orders WHERE orders.customer = t.name) ORDER BY id",
        ));
        assert!(e.is_empty(), "{e:?}");
        assert_eq!(n, null_ids);
        db.cleanup();
    }

    #[test]
    fn an_aggregate_without_group_by_inside_exists_always_yields_a_row() {
        // `EXISTS (SELECT COUNT(*) ...)` is true for every outer row, because a
        // global aggregate returns one row even over zero inputs -- the
        // classic trap. With HAVING the row is filtered and EXISTS means
        // what was intended.
        let (db, t, orders) = standard_db("b3_agg_exists");
        let idx = by_t_id(&orders);
        let all = ids_of(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) ORDER BY id",
        ));
        assert_eq!(all.len(), 60);
        let some = ids_of(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id HAVING COUNT(*) >= 3) ORDER BY id",
        ));
        let want: Vec<i32> = t
            .iter()
            .map(|r| r.0)
            .filter(|i| idx.get(i).map_or(0, |v| v.len()) >= 3)
            .collect();
        assert_eq!(some, want);
        db.cleanup();
    }

    #[test]
    fn exists_is_usable_as_a_value_in_the_select_list_and_in_case() {
        let (db, t, orders) = standard_db("b3_value");
        let idx = by_t_id(&orders);
        let got = db.rows(
            "SELECT id, EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id), \
             CASE WHEN NOT EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id) THEN 'none' ELSE 'some' END \
             FROM t ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .map(|r| {
                let has = idx.contains_key(&r.0);
                vec![int(r.0), boolean(has), text(if has { "some" } else { "none" })]
            })
            .collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn exists_combines_with_other_predicates_and_with_a_second_correlated_subquery() {
        let (db, t, orders) = standard_db("b3_combined");
        let idx = by_t_id(&orders);
        let count = |id: i32| idx.get(&id).map_or(0, |v| v.len());
        let got = ids_of(&db.rows(
            "SELECT id FROM t WHERE (EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND orders.amount IS NULL) \
             OR id > 55) AND NOT (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) = 2 ORDER BY id",
        ));
        let want: Vec<i32> = t
            .iter()
            .map(|r| r.0)
            .filter(|&i| {
                let has_null_amount = idx.get(&i).is_some_and(|os| os.iter().any(|o| o.2.is_none()));
                (has_null_amount || i > 55) && count(i) != 2
            })
            .collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn exists_over_an_inner_join_with_the_correlation_in_the_join_condition() {
        let (db, t, orders) = standard_db("b3_inner_join");
        let idx = by_t_id(&orders);
        // For each outer id: is there an order whose own t_id equals the
        // outer id and which shares its t_id with another order (a self join)?
        let got = ids_of(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders a JOIN orders b ON b.t_id = a.t_id AND b.id <> a.id \
             WHERE a.t_id = t.id) ORDER BY id",
        ));
        let want: Vec<i32> =
            t.iter().map(|r| r.0).filter(|i| idx.get(i).map_or(0, |v| v.len()) >= 2).collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn exists_stops_at_the_first_inner_row() {
        // EXISTS pulls at most one row from the inner plan; with a large
        // matching set it must not scan everything. rows_scanned for the
        // EXISTS form is far below the full-scan count of the COUNT form.
        let (db, _t, _o) = standard_db("b3_short_circuit");
        let m_exists = ExecMetrics::default();
        db.try_run_with(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id <> t.id)",
            &ExecLimits::default(),
            &m_exists,
            &[],
        )
        .unwrap();
        let m_count = ExecMetrics::default();
        db.try_run_with(
            "SELECT id FROM t WHERE (SELECT COUNT(*) FROM orders WHERE orders.t_id <> t.id) > 0",
            &ExecLimits::default(),
            &m_count,
            &[],
        )
        .unwrap();
        let (e, c) = (m_exists.snapshot().rows_scanned, m_count.snapshot().rows_scanned);
        assert!(e < c / 5, "EXISTS scanned {e} rows, COUNT scanned {c}");
        db.cleanup();
    }

    #[test]
    fn each_outer_row_costs_one_re_execution() {
        let (db, _t, _o) = standard_db("b3_metrics");
        let metrics = ExecMetrics::default();
        db.try_run_with(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
            &ExecLimits::default(),
            &metrics,
            &[],
        )
        .unwrap();
        assert_eq!(metrics.snapshot().correlated_subquery_reexecs, 60);
        db.cleanup();
    }

    #[test]
    fn the_correlated_exists_respects_max_correlated_reexecutions() {
        let (db, _t, _o) = standard_db("b3_limit");
        let err = db
            .try_run_with(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
                &ExecLimits { max_correlated_reexecutions: 10, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }
}

mod diff_exists {
    use super::diff::*;
    use super::*;

    #[test]
    fn correlated_exists_and_not_exists_match_the_reference_model_on_random_data() {
        run_seed("diff_exists", 20, |db, t, orders, seed| {
            for p in predicates() {
                for (negated, kw) in [(false, "EXISTS"), (true, "NOT EXISTS")] {
                    let sql = format!(
                        "SELECT id FROM t WHERE {kw} (SELECT 1 FROM orders WHERE {}) ORDER BY id",
                        p.sql
                    );
                    let got = db.rows(&sql);
                    let want: Vec<Row> = t
                        .iter()
                        .filter(|trow| (!matching(&p, trow, orders).is_empty()) != negated)
                        .map(|trow| vec![int(trow.0)])
                        .collect();
                    assert_eq!(got, want, "seed {seed}: {sql}");
                }
            }
        });
    }
}

// =======================================================================
// B4 -- execution: correlated IN / NOT IN (and the NULL trap)
// =======================================================================

mod in_subquery {
    use super::data::*;
    use super::diff::*;
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};

    fn ids_of(rows: &[Row]) -> Vec<i32> {
        rows.iter().map(id_of).collect()
    }

    /// `lhs IN (set)` in three-valued logic, written from the SQL definition
    /// rather than from the engine's implementation.
    pub(super) fn in3<T: PartialEq + Clone>(lhs: &Option<T>, set: &[Option<T>]) -> Tri {
        if set.is_empty() {
            return Some(false);
        }
        let Some(l) = lhs else {
            return None;
        };
        if set.iter().any(|m| m.as_ref() == Some(l)) {
            Some(true)
        } else if set.iter().any(|m| m.is_none()) {
            None
        } else {
            Some(false)
        }
    }

    /// The inner `orders.t_id` values selected for one outer row by the
    /// predicate `orders.customer = t.name`.
    fn t_ids_for_name(name: &Option<String>, orders: &[ORow]) -> Vec<Option<i32>> {
        orders
            .iter()
            .filter(|o| eq3(Some(o.1.clone()), name.clone()) == Some(true))
            .map(|o| o.3)
            .collect()
    }

    #[test]
    fn correlated_in_and_not_in_match_the_three_valued_reference_on_the_standard_data() {
        let (db, t, orders) = standard_db("b4_basic");
        let sql_in = "SELECT id FROM t WHERE id IN (SELECT o.t_id FROM orders o WHERE o.customer = t.name) ORDER BY id";
        let sql_not = "SELECT id FROM t WHERE id NOT IN (SELECT o.t_id FROM orders o WHERE o.customer = t.name) ORDER BY id";
        let want_in: Vec<i32> = t
            .iter()
            .filter(|r| in3(&Some(r.0), &t_ids_for_name(&r.1, &orders)) == Some(true))
            .map(|r| r.0)
            .collect();
        let want_not: Vec<i32> = t
            .iter()
            .filter(|r| not3(in3(&Some(r.0), &t_ids_for_name(&r.1, &orders))) == Some(true))
            .map(|r| r.0)
            .collect();
        assert_eq!(ids_of(&db.rows(sql_in)), want_in);
        assert_eq!(ids_of(&db.rows(sql_not)), want_not);
        assert!(!want_in.is_empty() && !want_not.is_empty(), "the data exercises both");
        db.cleanup();
    }

    #[test]
    fn correlated_not_in_with_a_null_in_the_inner_set_never_returns_true() {
        // THE TRAP. For an outer row whose inner set contains a NULL, `x NOT
        // IN set` is FALSE if x is in the set and UNKNOWN otherwise -- never
        // TRUE -- so such a row must never be returned. The orders with a
        // NULL t_id carry customers n0..n5, so every outer row named n0..n5
        // has a NULL in its inner set.
        let (db, t, orders) = standard_db("b4_null_trap");
        let poisoned: Vec<i32> = t
            .iter()
            .filter(|r| t_ids_for_name(&r.1, &orders).iter().any(|v| v.is_none()))
            .map(|r| r.0)
            .collect();
        assert!(poisoned.len() >= 5, "the fixture has outer rows with NULL in their set");
        let returned = ids_of(&db.rows(
            "SELECT id FROM t WHERE id NOT IN (SELECT o.t_id FROM orders o WHERE o.customer = t.name) ORDER BY id",
        ));
        for id in &poisoned {
            assert!(
                !returned.contains(id),
                "NOT IN returned outer row {id}, whose inner set contains NULL"
            );
        }
        // Contrast: remove the NULLs from the inner set and the same rows
        // come back (when they are not members), proving the filtering above
        // was the NULL and not something else.
        let returned_clean = ids_of(&db.rows(
            "SELECT id FROM t WHERE id NOT IN (SELECT o.t_id FROM orders o WHERE o.customer = t.name AND o.t_id IS NOT NULL) ORDER BY id",
        ));
        let came_back = poisoned.iter().filter(|id| returned_clean.contains(id)).count();
        assert!(came_back > 0, "without NULLs, poisoned rows that are non-members return");
        // NOT EXISTS is the safe idiom: it keeps every non-member regardless.
        let safe = ids_of(&db.rows(
            "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer = t.name AND o.t_id = t.id) ORDER BY id",
        ));
        assert!(safe.len() >= returned_clean.len());
        db.cleanup();
    }

    #[test]
    fn a_null_outer_value_is_unknown_unless_the_inner_set_is_empty() {
        // Rows: 1 has NULL name and an order (non-empty set); 2 has NULL name
        // and no orders (empty set); 3 has a name and no orders.
        let db = Db::new("b4_null_outer");
        db.seed_t(&[(1, None, Some(true)), (2, None, Some(true)), (3, Some("x"), Some(true))]);
        db.seed_orders(&[(1, "x", Some(1), Some(1))]);
        let sql = |kw: &str| {
            format!(
                "SELECT id FROM t WHERE name {kw} (SELECT customer FROM orders WHERE orders.t_id = t.id) ORDER BY id"
            )
        };
        // IN: NULL lhs with a non-empty set is UNKNOWN (excluded); with an
        // empty set it is FALSE (excluded); 'x' vs the empty set is FALSE.
        assert_eq!(ids_of(&db.rows(&sql("IN"))), Vec::<i32>::new());
        // NOT IN: row 1 UNKNOWN (excluded); rows 2 and 3 have empty sets, so
        // NOT IN is TRUE for both, even for the NULL name of row 2.
        assert_eq!(ids_of(&db.rows(&sql("NOT IN"))), vec![2, 3]);
        db.cleanup();
    }

    #[test]
    fn the_in_value_is_null_exactly_when_the_result_is_unknown() {
        let db = Db::new("b4_value");
        // 1: name 'x',  set {x}   -> TRUE
        // 2: name 'y',  set {x}   -> FALSE (not a member, no NULL in the set)
        // 3: name NULL, set {x}   -> UNKNOWN (a NULL operand, non-empty set)
        // 4: name 'x',  set {}    -> FALSE (an empty set is FALSE)
        // 5: name 'z',  set {x}   -> FALSE
        // (customer is NOT NULL, so a NULL *member* is exercised below with
        // the nullable t_id column instead.)
        db.seed_t(&[
            (1, Some("x"), None),
            (2, Some("y"), None),
            (3, None, None),
            (4, Some("x"), None),
            (5, Some("z"), None),
        ]);
        db.seed_orders(&[
            (1, "x", None, Some(1)),
            (2, "x", None, Some(2)),
            (3, "x", None, Some(3)),
            (5, "x", None, Some(5)),
            (6, "x", None, None),
        ]);
        let got = db.rows(
            "SELECT id, name IN (SELECT customer FROM orders WHERE orders.t_id = t.id) FROM t ORDER BY id",
        );
        assert_eq!(
            got,
            vec![
                vec![int(1), boolean(true)],
                vec![int(2), boolean(false)],
                vec![int(3), NULL],
                vec![int(4), boolean(false)],
                vec![int(5), boolean(false)],
            ]
        );
        // And a NULL member makes a non-member UNKNOWN: row 5 against the
        // whole t_id column, correlated through `orders.customer = 'x'`.
        let got = db.rows(
            "SELECT id, id IN (SELECT t_id FROM orders WHERE orders.customer = 'x' AND t.id > 4) FROM t WHERE id = 5 OR id = 4 ORDER BY id",
        );
        // id 4: `t.id > 4` is FALSE -> empty set -> FALSE.
        // id 5: set {1,2,3,5,NULL}: 5 is a member -> TRUE.
        assert_eq!(got, vec![vec![int(4), boolean(false)], vec![int(5), boolean(true)]]);
        let got = db.rows(
            "SELECT id, 9 IN (SELECT t_id FROM orders WHERE orders.customer = 'x' AND t.id = 5) FROM t WHERE id = 5",
        );
        assert_eq!(got, vec![vec![int(5), NULL]], "non-member, NULL in the set -> UNKNOWN");
        db.cleanup();
    }

    #[test]
    fn a_type_mismatch_between_the_outer_operand_and_the_inner_column_is_a_bind_error() {
        let (db, _t, _o) = standard_db("b4_types");
        let err = db
            .try_bind("SELECT id FROM t WHERE name IN (SELECT o.amount FROM orders o WHERE o.t_id = t.id)")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn an_expression_over_outer_columns_is_a_valid_in_operand() {
        let (db, t, orders) = standard_db("b4_expr_operand");
        let got = ids_of(&db.rows(
            "SELECT id FROM t WHERE (id + 1) IN (SELECT o.t_id FROM orders o WHERE o.t_id = t.id + 1) ORDER BY id",
        ));
        let have: std::collections::BTreeSet<i32> = orders.iter().filter_map(|o| o.3).collect();
        let want: Vec<i32> = t.iter().map(|r| r.0).filter(|i| have.contains(&(i + 1))).collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn each_outer_row_re_runs_the_inner_query_and_the_set_is_bounded_per_evaluation() {
        let (db, _t, _o) = standard_db("b4_limits");
        let sql = "SELECT id FROM t WHERE id IN (SELECT o.t_id FROM orders o WHERE o.customer = t.name)";
        let metrics = ExecMetrics::default();
        db.try_run_with(sql, &ExecLimits::default(), &metrics, &[]).unwrap();
        assert_eq!(metrics.snapshot().correlated_subquery_reexecs, 60);
        // The materialized set of ONE evaluation is capped by
        // max_materialized_rows: a name shared by many orders overflows 3.
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_materialized_rows: 3, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_correlated_reexecutions: 5, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn correlated_in_and_not_in_match_the_reference_model_on_random_data() {
        run_seed("diff_in", 20, |db, t, orders, seed| {
            // (operand SQL, inner column SQL, correlation SQL, model)
            // Operand t.id against orders.t_id; operand t.name against
            // orders.customer. Both have NULLs on each side.
            for p in predicates() {
                // INTEGER: t.id IN (SELECT orders.t_id ...)
                let set_of = |trow: &TRow| -> Vec<Option<i32>> {
                    matching(&p, trow, orders).iter().map(|o| o.3).collect()
                };
                for (kw, negated) in [("IN", false), ("NOT IN", true)] {
                    let sql = format!(
                        "SELECT id FROM t WHERE t.id {kw} (SELECT orders.t_id FROM orders WHERE {}) ORDER BY id",
                        p.sql
                    );
                    let want: Vec<Row> = t
                        .iter()
                        .filter(|r| {
                            let v = in3(&Some(r.0), &set_of(r));
                            (if negated { not3(v) } else { v }) == Some(true)
                        })
                        .map(|r| vec![int(r.0)])
                        .collect();
                    assert_eq!(db.rows(&sql), want, "seed {seed}: {sql}");
                }
                // The 3VL value itself (NULL = UNKNOWN) -- WHERE hides the
                // difference between FALSE and UNKNOWN, the projection does not.
                let sql = format!(
                    "SELECT id, t.id IN (SELECT orders.t_id FROM orders WHERE {}) FROM t ORDER BY id",
                    p.sql
                );
                let want: Vec<Row> = t
                    .iter()
                    .map(|r| {
                        vec![int(r.0), in3(&Some(r.0), &set_of(r)).map(boolean).unwrap_or(NULL)]
                    })
                    .collect();
                assert_eq!(db.rows(&sql), want, "seed {seed}: {sql}");

                // TEXT with a nullable operand: t.name IN (SELECT customer ...)
                let text_set = |trow: &TRow| -> Vec<Option<String>> {
                    matching(&p, trow, orders).iter().map(|o| Some(o.1.clone())).collect()
                };
                for (kw, negated) in [("IN", false), ("NOT IN", true)] {
                    let sql = format!(
                        "SELECT id FROM t WHERE t.name {kw} (SELECT orders.customer FROM orders WHERE {}) ORDER BY id",
                        p.sql
                    );
                    let want: Vec<Row> = t
                        .iter()
                        .filter(|r| {
                            let v = in3(&r.1, &text_set(r));
                            (if negated { not3(v) } else { v }) == Some(true)
                        })
                        .map(|r| vec![int(r.0)])
                        .collect();
                    assert_eq!(db.rows(&sql), want, "seed {seed}: {sql}");
                }
            }
        });
    }
}

// =======================================================================
// B5 -- UPDATE / DELETE with a correlated subquery
// =======================================================================

mod dml {
    use super::data::*;
    use super::*;
    use crate::exec::ExecLimits;

    fn dump_t(db: &Db) -> Vec<Row> {
        db.rows("SELECT id, name, active FROM t ORDER BY id")
    }

    fn t_rows(t: &[TRow]) -> Vec<Row> {
        t.iter()
            .map(|(i, n, a)| {
                vec![int(*i), n.as_deref().map(text).unwrap_or(NULL), a.map(boolean).unwrap_or(NULL)]
            })
            .collect()
    }

    fn has_big_order(idx: &std::collections::BTreeMap<i32, Vec<&ORow>>, id: i32) -> bool {
        idx.get(&id).is_some_and(|os| os.iter().any(|o| o.2.is_some_and(|a| a > 90)))
    }

    #[test]
    fn update_where_a_correlated_exists_changes_exactly_the_matching_rows() {
        let (db, mut t, orders) = standard_db("b5_update_exists");
        let idx = by_t_id(&orders);
        let hit: Vec<i32> =
            t.iter().map(|r| r.0).filter(|i| has_big_order(&idx, *i)).collect();
        assert!(hit.len() > 3 && hit.len() < 50, "the data splits: {}", hit.len());
        let r = db.write(
            "UPDATE t SET name = 'hit' WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND orders.amount > 90)",
        );
        assert_eq!(r.rows_affected, hit.len() as u64);
        for row in t.iter_mut() {
            if hit.contains(&row.0) {
                row.1 = Some("hit".to_string());
            }
        }
        assert_eq!(dump_t(&db), t_rows(&t));
        db.cleanup();
    }

    #[test]
    fn delete_where_correlated_exists_not_exists_and_in_remove_exactly_the_matching_rows() {
        let (_db0, t, orders) = standard_db("b5_delete_ref");
        _db0.cleanup();
        let idx = by_t_id(&orders);
        type Matches<'m> = Box<dyn Fn(i32) -> bool + 'm>;
        let cases: [(&str, Matches); 3] = [
            (
                "DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND orders.amount > 90)",
                Box::new(|i| has_big_order(&idx, i)),
            ),
            (
                "DELETE FROM t WHERE NOT EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
                Box::new(|i| !idx.contains_key(&i)),
            ),
            (
                "DELETE FROM t WHERE id IN (SELECT o.t_id FROM orders o WHERE o.customer = t.name AND o.amount > 50)",
                Box::new(|i| {
                    let name = &t[(i - 1) as usize].1;
                    name.as_ref().is_some_and(|n| {
                        orders.iter().any(|o| &o.1 == n && o.3 == Some(i) && o.2.is_some_and(|a| a > 50))
                    })
                }),
            ),
        ];
        for (k, (sql, pred)) in cases.iter().enumerate() {
            let (db, _t, _o) = standard_db(&format!("b5_delete_{k}"));
            let r = db.write(sql);
            let survivors: Vec<TRow> = t.iter().filter(|row| !pred(row.0)).cloned().collect();
            assert_eq!(r.rows_affected, (t.len() - survivors.len()) as u64, "{sql}");
            assert_eq!(dump_t(&db), t_rows(&survivors), "{sql}");
            assert!(!survivors.is_empty() && survivors.len() < t.len(), "{sql}");
            db.cleanup();
        }
    }

    #[test]
    fn a_self_correlated_update_sees_the_pre_statement_table_not_its_own_partial_changes() {
        // Every row whose name is shared with another row gets 'dup'. The
        // correlated column (t.name) is the very column being updated: if the
        // first row of a group were rewritten before the next row's subquery
        // ran, the group would lose members one by one and its last row would
        // wrongly escape.
        let (db, t, _o) = standard_db("b5_self_update");
        let count_of = |n: &Option<String>| {
            t.iter().filter(|r| n.is_some() && &r.1 == n).count()
        };
        let want: Vec<TRow> = t
            .iter()
            .map(|r| {
                if count_of(&r.1) >= 2 {
                    (r.0, Some("dup".to_string()), r.2)
                } else {
                    r.clone()
                }
            })
            .collect();
        let changed = want.iter().zip(&t).filter(|(a, b)| a != b).count();
        assert!(changed > 30, "the data has many shared names: {changed}");
        let r = db.write(
            "UPDATE t SET name = 'dup' WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.name = t.name AND t2.id <> t.id)",
        );
        assert_eq!(r.rows_affected, changed as u64);
        assert_eq!(dump_t(&db), t_rows(&want));
        db.cleanup();
    }

    #[test]
    fn a_self_correlated_delete_removes_every_member_of_a_duplicate_group() {
        // Sequential evaluation would delete all but the last member of each
        // group (the last has no remaining duplicate by then).
        let (db, t, _o) = standard_db("b5_self_delete");
        let count_of = |n: &Option<String>| t.iter().filter(|r| n.is_some() && &r.1 == n).count();
        let survivors: Vec<TRow> = t.iter().filter(|r| count_of(&r.1) < 2).cloned().collect();
        let r = db.write(
            "DELETE FROM t WHERE EXISTS (SELECT 1 FROM t t2 WHERE t2.name = t.name AND t2.id <> t.id)",
        );
        assert_eq!(r.rows_affected, (t.len() - survivors.len()) as u64);
        assert_eq!(dump_t(&db), t_rows(&survivors));
        db.cleanup();
    }

    // ---- SET with a (correlated) subquery ---------------------------------

    #[test]
    fn set_with_a_correlated_scalar_subquery_matches_the_reference() {
        let (db, t, orders) = standard_db("b5_set_scalar");
        let idx = by_t_id(&orders);
        let r = db.write(
            "UPDATE t SET name = (SELECT MAX(customer) FROM orders WHERE orders.t_id = t.id)",
        );
        assert_eq!(r.rows_affected, 60);
        let want: Vec<TRow> = t
            .iter()
            .map(|row| {
                let max = idx.get(&row.0).and_then(|os| os.iter().map(|o| o.1.clone()).max());
                (row.0, max, row.2)
            })
            .collect();
        assert_eq!(dump_t(&db), t_rows(&want));
        db.cleanup();
    }

    #[test]
    fn set_subqueries_see_the_table_as_it_was_before_the_statement() {
        // Each row takes the PREVIOUS row's name. Row-at-a-time evaluation in
        // primary-key order would propagate row 1's value down the whole
        // table; the correct result is a one-step shift.
        let (db, t, _o) = standard_db("b5_set_shift");
        db.write("UPDATE t SET name = (SELECT t2.name FROM t t2 WHERE t2.id = t.id - 1)");
        let want: Vec<TRow> = t
            .iter()
            .map(|row| {
                let prev = if row.0 == 1 { None } else { t[(row.0 - 2) as usize].1.clone() };
                (row.0, prev, row.2)
            })
            .collect();
        assert_eq!(dump_t(&db), t_rows(&want));
        db.cleanup();
    }

    #[test]
    fn the_correlated_column_may_be_updated_by_the_same_statement() {
        // `orders.amount` is both the correlation key and an assigned column:
        // the subquery must see the ORIGINAL amount (a simultaneous-assignment
        // statement), and a row's update must not alter another row's lookup.
        let (db, t, orders) = standard_db("b5_corr_col_updated");
        let name_of = |id: i32| t.iter().find(|r| r.0 == id).and_then(|r| r.1.clone());
        let qualifies =
            |o: &ORow| o.2.is_some_and(|a| name_of(a).is_some());
        let r = db.write(
            "UPDATE orders SET amount = 0, customer = (SELECT t.name FROM t WHERE t.id = orders.amount) \
             WHERE EXISTS (SELECT 1 FROM t WHERE t.id = orders.amount AND t.name IS NOT NULL)",
        );
        let expect_n = orders.iter().filter(|o| qualifies(o)).count();
        assert!(expect_n > 20, "{expect_n}");
        assert_eq!(r.rows_affected, expect_n as u64);
        let got = db.rows("SELECT id, customer, amount FROM orders ORDER BY id");
        let mut want: Vec<(i64, String, Option<i32>)> =
            orders.iter().map(|o| (o.0, o.1.clone(), o.2)).collect();
        for w in want.iter_mut() {
            let o = orders.iter().find(|o| o.0 == w.0).unwrap();
            if qualifies(o) {
                w.1 = name_of(o.2.unwrap()).unwrap();
                w.2 = Some(0);
            }
        }
        want.sort_by_key(|w| w.0);
        let want_rows: Vec<Row> =
            want.iter().map(|w| vec![big(w.0), text(&w.1), opt_int(w.2)]).collect();
        assert_eq!(got, want_rows);
        db.cleanup();
    }

    #[test]
    fn set_may_mix_a_subquery_with_a_plain_expression_over_the_old_row() {
        let (db, t, orders) = standard_db("b5_set_mixed");
        let idx = by_t_id(&orders);
        db.write(
            "UPDATE t SET active = NOT active, name = (SELECT MAX(customer) FROM orders WHERE orders.t_id = t.id AND t.active = true)",
        );
        // `t.active` inside the subquery is the OLD value; `NOT active` is
        // computed from the old value too.
        let want: Vec<TRow> = t
            .iter()
            .map(|row| {
                let name = if row.2 == Some(true) {
                    idx.get(&row.0).and_then(|os| os.iter().map(|o| o.1.clone()).max())
                } else {
                    None
                };
                (row.0, name, row.2.map(|b| !b))
            })
            .collect();
        assert_eq!(dump_t(&db), t_rows(&want));
        db.cleanup();
    }

    #[test]
    fn a_runtime_error_in_set_changes_nothing_and_names_the_row() {
        let (db, t, _o) = standard_db("b5_set_error");
        let before = dump_t(&db);
        // Rows 8 and 9 evaluate; row 10 has two orders -> runtime error.
        let err = db
            .try_write(
                "UPDATE t SET name = (SELECT customer FROM orders WHERE orders.t_id = t.id) WHERE id IN (8, 9, 10)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert!(err.to_string().contains("t.id = 10"), "{err}");
        assert_eq!(dump_t(&db), before, "no row was written");
        assert_eq!(before, t_rows(&t));
        db.cleanup();
    }

    // ---- limits across the WHERE and SET phases --------------------------------

    #[test]
    fn the_correlated_budget_is_one_per_statement_across_where_and_set() {
        // WHERE evaluates its subquery for each of the 60 rows; SET evaluates
        // its own for each of the 53 rows that matched (ids with >= 1 order).
        let (db, _t, orders) = standard_db("b5_budget");
        let idx = by_t_id(&orders);
        let matched = (1..=60).filter(|i| idx.contains_key(i)).count() as u64;
        assert_eq!(matched, 53);
        let sql = "UPDATE t SET name = (SELECT MAX(customer) FROM orders WHERE orders.t_id = t.id) \
                   WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)";
        let total = 60 + matched;
        let limits = |n: u64| ExecLimits { max_correlated_reexecutions: n, ..ExecLimits::default() };
        let before = dump_t(&db);
        let err = db.try_write_with(sql, &limits(total - 1)).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert_eq!(dump_t(&db), before, "a refused statement writes nothing");
        assert!(db.try_write_with(sql, &limits(total)).is_ok(), "exactly the total fits");
        db.cleanup();
    }

    #[test]
    fn rows_buffered_for_a_set_with_a_subquery_are_bounded_by_max_materialized_bytes() {
        let (db, _t, _o) = standard_db("b5_set_bytes");
        let before = dump_t(&db);
        let sql = "UPDATE t SET name = (SELECT MAX(customer) FROM orders WHERE orders.t_id = t.id)";
        let err = db
            .try_write_with(
                sql,
                &ExecLimits { max_materialized_bytes: 200, ..ExecLimits::default() },
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_materialized_bytes"), "{err}");
        assert_eq!(dump_t(&db), before, "a refused statement writes nothing");
        assert!(db.try_write(sql).is_ok(), "the default bound admits it");
        db.cleanup();
    }

    #[test]
    fn a_correlated_delete_respects_the_reexecution_limit_and_deletes_nothing_when_refused() {
        let (db, _t, _o) = standard_db("b5_delete_limit");
        let before = dump_t(&db);
        let err = db
            .try_write_with(
                "DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
                &ExecLimits { max_correlated_reexecutions: 10, ..ExecLimits::default() },
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert_eq!(dump_t(&db), before);
        db.cleanup();
    }

    // ---- EXPLAIN ---------------------------------------------------------------

    #[test]
    fn explain_shows_the_correlation_for_where_and_set() {
        let (db, _t, _o) = standard_db("b5_explain");
        for sql in [
            "EXPLAIN UPDATE t SET name = (SELECT MAX(customer) FROM orders WHERE orders.t_id = t.id) WHERE id > 3",
            "EXPLAIN UPDATE t SET name = 'x' WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
            "EXPLAIN DELETE FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)",
            "EXPLAIN SELECT id, (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t",
        ] {
            let text = crate::plan::explain(&db.plan(sql));
            assert!(text.contains("correlated outer_row_input=true"), "{sql}: {text}");
            assert!(!text.contains("uncorrelated"), "{sql}: {text}");
        }
        // The uncorrelated form says so.
        let text = crate::plan::explain(&db.plan("EXPLAIN SELECT id FROM t WHERE id IN (SELECT t_id FROM orders)"));
        assert!(text.contains("uncorrelated (executed once)"), "{text}");
        db.cleanup();
    }
}

// =======================================================================
// B6 -- nested correlation, three levels deep
// =======================================================================

mod nested {
    use super::data::*;
    use super::diff::*;
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};
    use rubixdb::catalog::service::ColumnDef;
    use rubixdb::relational::value::{TYPE_TAG_BIGINT, TYPE_TAG_INTEGER};

    /// `items(id INTEGER PK, order_id BIGINT, qty INTEGER)`.
    pub(super) type IRow = (i32, i64, i32);

    /// A third table, created through the fixture's own catalog the same way
    /// `t` and `orders` are, holding `order.id % 3` items per order.
    fn add_items(db: &Db, orders: &[ORow]) -> Vec<IRow> {
        let col = |name: &str, tag: u8, nullable: bool| ColumnDef {
            name: name.to_string(),
            data_type: tag,
            nullable,
            default_value: None,
            type_params: None,
        };
        db.f.catalog
            .create_table(
                db.f.ctx.default_schema_id,
                "items",
                &[
                    col("id", TYPE_TAG_INTEGER, false),
                    col("order_id", TYPE_TAG_BIGINT, true),
                    col("qty", TYPE_TAG_INTEGER, true),
                ],
                &[0],
            )
            .unwrap();
        let table = db.table_id("items");
        let mut items = Vec::new();
        let mut next = 1i32;
        for o in orders {
            for k in 0..(o.0 % 3) {
                items.push((next, o.0, ((o.0 * 7 + k * 5) % 40) as i32));
                next += 1;
            }
        }
        for (id, order_id, qty) in &items {
            db.store.put_row(table, &[int(*id), big(*order_id), int(*qty)]).unwrap();
        }
        items
    }

    fn standard_with_items(tag: &str) -> (Db, Vec<TRow>, Vec<ORow>, Vec<IRow>) {
        let (db, t, orders) = standard_db(tag);
        let items = add_items(&db, &orders);
        (db, t, orders, items)
    }

    /// The same shape at a size where a three-level query whose conjunction
    /// the evaluator does not short-circuit (both sides of an `AND` are always
    /// evaluated) finishes in about a second: 20 outer rows, the 82 orders
    /// that belong to them or to nobody, and their ~80 items. The full-scale
    /// three-level run (60 / 222 / ~220) is `three_levels_at_full_scale_*`,
    /// whose conjunction is guarded with `CASE` so the inner levels run only
    /// for the rows that can match.
    fn compact_with_items(tag: &str) -> (Db, Vec<TRow>, Vec<ORow>, Vec<IRow>) {
        let db = Db::new(tag);
        let (t, orders) = standard();
        let t: Vec<TRow> = t.into_iter().filter(|r| r.0 <= 20).collect();
        let orders: Vec<ORow> = orders
            .into_iter()
            .filter(|o| o.3.is_none_or(|v| v <= 20 || v >= 100))
            .collect();
        seed(&db, &t, &orders);
        let items = add_items(&db, &orders);
        (db, t, orders, items)
    }

    fn items_of(items: &[IRow], order_id: i64) -> Vec<&IRow> {
        items.iter().filter(|i| i.1 == order_id).collect()
    }

    fn ids(rows: &[Row]) -> Vec<i32> {
        rows.iter().map(id_of).collect()
    }

    /// Model of `EXISTS (orders o WHERE o.t_id = t.id AND EXISTS (items i
    /// WHERE i.order_id = o.id AND i.qty > t.id))`.
    fn deep_exists(t: &TRow, orders: &[ORow], items: &[IRow]) -> bool {
        orders
            .iter()
            .filter(|o| o.3 == Some(t.0))
            .any(|o| items_of(items, o.0).iter().any(|i| i.2 > t.0))
    }

    /// The three-level queries cost (outer rows) x (orders) x (items) row
    /// visits when nothing short-circuits; the heaviest run over this many
    /// outer rows to stay well inside the default deadline under a loaded
    /// test run.
    const DEEP_SUBSET: i32 = 15;

    const DEEP_EXISTS: &str = "EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
        EXISTS (SELECT 1 FROM items i WHERE i.order_id = o.id AND i.qty > t.id))";

    #[test]
    fn three_levels_exists_where_the_innermost_reads_both_enclosing_levels() {
        let (db, t, orders, items) = compact_with_items("b6_exists");
        let got = ids(&db.rows(&format!("SELECT id FROM t WHERE {DEEP_EXISTS} ORDER BY id")));
        let want: Vec<i32> =
            t.iter().filter(|r| deep_exists(r, &orders, &items)).map(|r| r.0).collect();
        assert!(!want.is_empty() && want.len() < 60, "the data splits: {}", want.len());
        assert_eq!(got, want);
        // NOT EXISTS evaluates the whole inner chain for the non-matching rows,
        // so it runs over a subset of the outer rows (the 60-row fixture is
        // still the one seeded).
        let got = ids(&db.rows(&format!(
            "SELECT id FROM t WHERE id <= {DEEP_SUBSET} AND NOT {DEEP_EXISTS} ORDER BY id"
        )));
        let want: Vec<i32> = t
            .iter()
            .filter(|r| r.0 <= DEEP_SUBSET && !deep_exists(r, &orders, &items))
            .map(|r| r.0)
            .collect();
        assert!(!want.is_empty());
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn three_levels_at_full_scale_match_the_reference() {
        // 60 outer rows x 222 orders x ~220 items. The `CASE` keeps the
        // inner EXISTS from being evaluated for orders that belong to some
        // other outer row (the evaluator evaluates both sides of an AND).
        let (db, t, orders, items) = standard_with_items("b6_full_scale");
        let got = ids(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE              CASE WHEN o.t_id = t.id THEN EXISTS (SELECT 1 FROM items i WHERE i.order_id = o.id AND i.qty > t.id) ELSE false END)              ORDER BY id",
        ));
        let want: Vec<i32> =
            t.iter().filter(|r| deep_exists(r, &orders, &items)).map(|r| r.0).collect();
        assert_eq!(t.len(), 60);
        assert!(orders.len() >= 200 && !items.is_empty());
        assert!(!want.is_empty() && want.len() < 60, "the data splits: {}", want.len());
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn three_levels_scalar_counts_match_the_nested_loop_reference() {
        let (db, t, orders, items) = compact_with_items("b6_scalar");
        let got = db.rows(
            "SELECT id, (SELECT COUNT(*) FROM orders o WHERE o.t_id = t.id AND \
             (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id) >= 2) FROM t WHERE id <= 15 ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .filter(|r| r.0 <= DEEP_SUBSET)
            .map(|r| {
                let n = orders
                    .iter()
                    .filter(|o| o.3 == Some(r.0))
                    .filter(|o| items_of(&items, o.0).len() >= 2)
                    .count();
                vec![int(r.0), big(n as i64)]
            })
            .collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn three_levels_in_where_the_inner_set_depends_on_two_enclosing_rows() {
        let (db, t, orders, items) = compact_with_items("b6_in");
        let got = ids(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
             o.id IN (SELECT i.order_id FROM items i WHERE i.qty > t.id)) ORDER BY id",
        ));
        let want: Vec<i32> = t
            .iter()
            .filter(|r| {
                orders.iter().filter(|o| o.3 == Some(r.0)).any(|o| {
                    items.iter().any(|i| i.1 == o.0 && i.2 > r.0)
                })
            })
            .map(|r| r.0)
            .collect();
        assert!(!want.is_empty());
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn the_innermost_binding_wins_when_an_alias_is_reused_at_the_third_level() {
        // `x` is `t` in the outer query and `items` in the innermost: inside
        // the innermost, `x.order_id` is items.order_id (t has no such
        // column, so a wrong outward binding would also fail to bind). Same
        // answer as with distinct aliases.
        let (db, _t, _o, _i) = compact_with_items("b6_shadow");
        let shadowed = db.rows(
            "SELECT id FROM t x WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = x.id AND \
             EXISTS (SELECT 1 FROM items x WHERE x.order_id = o.id AND x.qty > 20)) ORDER BY id",
        );
        let plain = db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
             EXISTS (SELECT 1 FROM items i WHERE i.order_id = o.id AND i.qty > 20)) ORDER BY id",
        );
        assert_eq!(shadowed, plain);
        assert!(!plain.is_empty());
        db.cleanup();
    }

    #[test]
    fn a_name_present_at_two_enclosing_levels_binds_to_the_nearer_one_at_run_time() {
        // `id` exists in t (two levels out) and in orders (one level out). The
        // innermost FROM-less query reads `id`: it must be the order's id.
        let (db, t, orders, _i) = standard_with_items("b6_nearer");
        let got = ids(&db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND \
             EXISTS (SELECT 1 WHERE id > 1000)) ORDER BY id",
        ));
        // orders.id >= 1000 + 10 for every order of a t row: always > 1000, so
        // every t row with an order qualifies. (Bound to t.id, which is <= 60,
        // `id > 1000` would be false for all.)
        let want: Vec<i32> = t
            .iter()
            .filter(|r| orders.iter().any(|o| o.3 == Some(r.0)))
            .map(|r| r.0)
            .collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_correlated_subquery_may_contain_an_uncorrelated_one_which_still_runs_once() {
        let (db, t, orders, _i) = standard_with_items("b6_mixed_a");
        let metrics = ExecMetrics::default();
        let sql = "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
                   o.amount > (SELECT MAX(amount) FROM orders) - 30) ORDER BY id";
        let got = ids(&db.try_run_with(sql, &ExecLimits::default(), &metrics, &[]).unwrap().rows);
        let max = orders.iter().filter_map(|o| o.2).max().unwrap();
        let want: Vec<i32> = t
            .iter()
            .filter(|r| {
                orders.iter().any(|o| o.3 == Some(r.0) && o.2.is_some_and(|a| a > max - 30))
            })
            .map(|r| r.0)
            .collect();
        assert!(!want.is_empty());
        assert_eq!(got, want);
        let m = metrics.snapshot();
        assert_eq!(m.correlated_subquery_reexecs, 60, "{m:?}");
        // 60 correlated runs + the uncorrelated MAX exactly once.
        assert_eq!(m.subquery_executions, 61, "{m:?}");
        assert!(m.subquery_cache_hits > 0, "{m:?}");
        db.cleanup();
    }

    #[test]
    fn an_uncorrelated_subquery_may_contain_a_correlated_one() {
        // The middle query reads nothing from `t`, so it is uncorrelated and
        // runs once; the query inside it is correlated to the middle's rows.
        let (db, t, orders, items) = standard_with_items("b6_mixed_b");
        let metrics = ExecMetrics::default();
        let sql = "SELECT id FROM t WHERE id IN (SELECT o.t_id FROM orders o WHERE \
                   EXISTS (SELECT 1 FROM items i WHERE i.order_id = o.id)) ORDER BY id";
        let got = ids(&db.try_run_with(sql, &ExecLimits::default(), &metrics, &[]).unwrap().rows);
        let want: Vec<i32> = t
            .iter()
            .filter(|r| {
                orders
                    .iter()
                    .any(|o| o.3 == Some(r.0) && !items_of(&items, o.0).is_empty())
            })
            .map(|r| r.0)
            .collect();
        assert_eq!(got, want);
        let m = metrics.snapshot();
        assert_eq!(m.correlated_subquery_reexecs, orders.len() as u64, "{m:?}");
        assert_eq!(m.subquery_executions, 1 + orders.len() as u64, "{m:?}");
        assert_eq!(m.subquery_cache_hits, 59, "60 outer rows: 1 miss + 59 hits ({m:?})");
        db.cleanup();
    }

    #[test]
    fn the_budget_counts_every_level_exactly() {
        let (db, _t, _o, _i) = standard_with_items("b6_budget");
        let sql = format!("SELECT id FROM t WHERE id <= 6 AND {DEEP_EXISTS}");
        let metrics = ExecMetrics::default();
        db.try_run_with(&sql, &ExecLimits::default(), &metrics, &[]).unwrap();
        let reexecs = metrics.snapshot().correlated_subquery_reexecs;
        assert!(reexecs > 6, "inner levels add to the 6 outer evaluations: {reexecs}");
        let run = |n: u64| {
            db.try_run_with(
                &sql,
                &ExecLimits { max_correlated_reexecutions: n, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
        };
        assert!(run(reexecs).is_ok());
        assert!(matches!(run(reexecs - 1).unwrap_err(), SqlError::ResourceLimit { .. }));
        // The per-subquery limit counts the INNER subquery across all of the
        // middle one's evaluations: it is hit while the middle one (6 runs)
        // is still far below it.
        let per = |n: u64| {
            db.try_run_with(
                &sql,
                &ExecLimits { max_subquery_outer_rows: n, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
        };
        assert!(per(20).is_err(), "the inner subquery is evaluated more than 20 times");
        assert!(per(reexecs).is_ok());
        db.cleanup();
    }

    #[test]
    fn a_runtime_error_in_the_innermost_level_names_the_enclosing_row_it_was_running_for() {
        let (db, _t, orders, items) = standard_with_items("b6_error");
        // An order with two items makes the innermost scalar return 2 rows.
        let two = orders.iter().find(|o| items_of(&items, o.0).len() == 2).unwrap();
        let err = db
            .try_run(&format!(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.id = {} AND \
                 (SELECT i.qty FROM items i WHERE i.order_id = o.id) >= 0)",
                two.0
            ))
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert!(err.to_string().contains(&format!("o.id = {}", two.0)), "{err}");
        db.cleanup();
    }

    #[test]
    fn nested_correlation_in_update_and_delete_matches_the_reference() {
        for (k, kind) in ["UPDATE t SET name = 'deep' WHERE", "DELETE FROM t WHERE"]
            .into_iter()
            .enumerate()
        {
            let (db, t, orders, items) = compact_with_items(&format!("b6_dml_{k}"));
            let hit: Vec<i32> = t
                .iter()
                .filter(|r| deep_exists(r, &orders, &items))
                .map(|r| r.0)
                .collect();
            assert!(!hit.is_empty());
            let r = db.write(&format!("{kind} {DEEP_EXISTS}"));
            assert_eq!(r.rows_affected, hit.len() as u64);
            let got = db.rows("SELECT id, name FROM t ORDER BY id");
            let want: Vec<Row> = if k == 0 {
                t.iter()
                    .map(|row| {
                        let name = if hit.contains(&row.0) {
                            text("deep")
                        } else {
                            row.1.as_deref().map(text).unwrap_or(NULL)
                        };
                        vec![int(row.0), name]
                    })
                    .collect()
            } else {
                t.iter()
                    .filter(|row| !hit.contains(&row.0))
                    .map(|row| vec![int(row.0), row.1.as_deref().map(text).unwrap_or(NULL)])
                    .collect()
            };
            assert_eq!(got, want);
            db.cleanup();
        }
    }

    #[test]
    fn nested_correlated_queries_match_the_reference_model_on_random_data() {
        run_seed("diff_nested", 6, |db, t, orders, seed| {
            let items = add_items(db, orders);
            // EXISTS and COUNT at depth 3, with a random inner threshold.
            for threshold in [5, 25] {
                let sql = format!(
                    "SELECT id, EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id AND \
                     EXISTS (SELECT 1 FROM items i WHERE i.order_id = o.id AND i.qty > {threshold} + t.id - t.id)), \
                     (SELECT COUNT(*) FROM orders o WHERE o.t_id = t.id AND \
                     (SELECT COUNT(*) FROM items i WHERE i.order_id = o.id AND i.qty > {threshold}) >= 1) \
                     FROM t ORDER BY id"
                );
                let got = db.rows(&sql);
                let want: Vec<Row> = t
                    .iter()
                    .map(|r| {
                        let mine: Vec<&ORow> = orders.iter().filter(|o| o.3 == Some(r.0)).collect();
                        let exists = mine.iter().any(|o| items.iter().any(|i| i.1 == o.0 && i.2 > threshold));
                        let count = mine
                            .iter()
                            .filter(|o| items.iter().filter(|i| i.1 == o.0 && i.2 > threshold).count() >= 1)
                            .count();
                        vec![int(r.0), boolean(exists), big(count as i64)]
                    })
                    .collect();
                assert_eq!(got, want, "seed {seed}: {sql}");
            }
        });
    }
}

// =======================================================================
// Shapes around the core forms: joins in the outer query, LIMIT / ORDER BY
// inside the subquery, parameters, grouped outer queries, transactions
// =======================================================================

mod shapes {
    use super::data::*;
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};

    #[test]
    fn a_correlated_subquery_over_a_join_reads_columns_of_both_joined_tables() {
        // For each (t, order) pair keep the order only if no other order of the
        // same t has a larger amount: the subquery reads t.id AND o.amount.
        let (db, t, orders) = standard_db("shape_join");
        let idx = by_t_id(&orders);
        let got = db.rows_sorted(
            "SELECT t.id, o.id FROM t JOIN orders o ON o.t_id = t.id \
             WHERE (SELECT COUNT(*) FROM orders o2 WHERE o2.t_id = t.id AND o2.amount > o.amount) = 0",
        );
        let mut want: Vec<Row> = Vec::new();
        for trow in &t {
            for o in idx.get(&trow.0).into_iter().flatten() {
                let beaten = idx[&trow.0].iter().any(|o2| match (o2.2, o.2) {
                    (Some(a2), Some(a)) => a2 > a,
                    _ => false,
                });
                if !beaten {
                    want.push(vec![int(trow.0), big(o.0)]);
                }
            }
        }
        want.sort_by_key(|r| format!("{r:?}"));
        assert!(!want.is_empty());
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_correlated_subquery_may_read_the_null_extended_side_of_a_left_join() {
        let (db, t, orders) = standard_db("shape_left_join");
        let idx = by_t_id(&orders);
        // Unmatched t rows have o.id NULL: `o2.id = NULL` is UNKNOWN, so NOT
        // EXISTS keeps them; matched rows are kept unless the order is big.
        let got = db.rows_sorted(
            "SELECT t.id, o.id FROM t LEFT JOIN orders o ON o.t_id = t.id \
             WHERE NOT EXISTS (SELECT 1 FROM orders o2 WHERE o2.id = o.id AND o2.amount > 90)",
        );
        let mut want: Vec<Row> = Vec::new();
        for trow in &t {
            match idx.get(&trow.0) {
                None => want.push(vec![int(trow.0), NULL]),
                Some(os) => {
                    for o in os {
                        if !o.2.is_some_and(|a| a > 90) {
                            want.push(vec![int(trow.0), big(o.0)]);
                        }
                    }
                }
            }
        }
        want.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn order_by_and_limit_inside_a_correlated_subquery_pick_one_row_per_outer_row() {
        let (db, t, orders) = standard_db("shape_top1");
        let idx = by_t_id(&orders);
        let got = db.rows(
            "SELECT id, (SELECT o.id FROM orders o WHERE o.t_id = t.id ORDER BY o.id DESC LIMIT 1) FROM t ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .map(|r| {
                let top = idx.get(&r.0).and_then(|os| os.iter().map(|o| o.0).max());
                vec![int(r.0), top.map(big).unwrap_or(NULL)]
            })
            .collect();
        assert_eq!(got, want);
        // LIMIT inside EXISTS is harmless.
        let n = db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o WHERE o.t_id = t.id LIMIT 1) ORDER BY id",
        );
        assert_eq!(n.len(), idx.keys().filter(|k| (1..=60).contains(*k)).count());
        db.cleanup();
    }

    #[test]
    fn distinct_inside_a_correlated_in_subquery_does_not_change_the_answer() {
        let (db, t, orders) = standard_db("shape_distinct");
        let with = db.rows_sorted(
            "SELECT id FROM t WHERE name IN (SELECT DISTINCT o.customer FROM orders o WHERE o.t_id = t.id)",
        );
        let without = db.rows_sorted(
            "SELECT id FROM t WHERE name IN (SELECT o.customer FROM orders o WHERE o.t_id = t.id)",
        );
        assert_eq!(with, without);
        let idx = by_t_id(&orders);
        let want = t
            .iter()
            .filter(|r| {
                r.1.as_ref().is_some_and(|n| {
                    idx.get(&r.0).is_some_and(|os| os.iter().any(|o| &o.1 == n))
                })
            })
            .count();
        assert_eq!(with.len(), want);
        db.cleanup();
    }

    #[test]
    fn a_parameter_inside_a_correlated_subquery_is_bound_at_execution() {
        let (db, t, orders) = standard_db("shape_param");
        let idx = by_t_id(&orders);
        for threshold in [10i32, 90] {
            let got = db
                .try_run_with(
                    "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id AND orders.amount > $1) ORDER BY id",
                    &ExecLimits::default(),
                    &ExecMetrics::default(),
                    &[int(threshold)],
                )
                .unwrap()
                .rows;
            let want: Vec<Row> = t
                .iter()
                .filter(|r| {
                    idx.get(&r.0)
                        .is_some_and(|os| os.iter().any(|o| o.2.is_some_and(|a| a > threshold)))
                })
                .map(|r| vec![int(r.0)])
                .collect();
            assert_eq!(got, want, "threshold {threshold}");
        }
        db.cleanup();
    }

    #[test]
    fn a_correlated_subquery_in_a_grouped_query_reads_the_group_representative() {
        // `orders.t_id` is the grouping column, so the subquery that reads it
        // is legal and is evaluated once per group.
        let (db, t, orders) = standard_db("shape_grouped");
        let got = db.rows(
            "SELECT t_id, (SELECT t.name FROM t WHERE t.id = orders.t_id), COUNT(*) \
             FROM orders WHERE t_id IS NOT NULL GROUP BY t_id ORDER BY t_id",
        );
        let idx = by_t_id(&orders);
        let want: Vec<Row> = idx
            .iter()
            .map(|(tid, os)| {
                let name = t.iter().find(|r| r.0 == *tid).and_then(|r| r.1.clone());
                vec![int(*tid), name.as_deref().map(text).unwrap_or(NULL), big(os.len() as i64)]
            })
            .collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_correlated_subquery_inside_a_transaction_sees_that_transactions_own_writes() {
        let (db, _t, _o) = standard_db("shape_txn");
        let mut txn = db.txm.begin().unwrap();
        // Outer row 8 has no orders; give it one inside the transaction.
        txn.put_row(db.table_id("orders"), &[big(777), text("mine"), int(5), int(8)])
            .unwrap();
        let sql = "SELECT id FROM t WHERE id = 8 AND EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)";
        let inside = db.run_in_txn(sql, &txn).unwrap();
        assert_eq!(inside.rows.len(), 1, "the transaction sees its own order");
        // Another reader does not, until commit.
        assert!(db.rows(sql).is_empty());
        txn.commit().unwrap();
        assert_eq!(db.rows(sql).len(), 1);
        db.cleanup();
    }
}
