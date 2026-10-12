//! SQL-mission-1, Slice C -- derived tables, `LATERAL`, and non-recursive
//! CTEs.
//!
//! Layout: `parser`, `binder` (resolution, shadowing, boundaries, column
//! lists, authorization, limits), `derived` / `lateral` / `cte` (execution
//! against the 60/222-row fixture of `correlated_tests::data`), `diff*`
//! (randomized comparison with an independent model).

use crate::correlated_tests::data::*;
use crate::mission1_support::*;

use crate::bound::{BoundRelation, BoundStatement};
use crate::error::SqlError;

fn bound_select(db: &Db, sql: &str) -> Box<crate::bound::BoundSelect> {
    match db
        .try_bind(sql)
        .unwrap_or_else(|e| panic!("expected {sql:?} to bind, got {e}"))
    {
        BoundStatement::Select(s) => s,
        other => panic!("expected a SELECT, got {other:?}"),
    }
}

fn column_names(db: &Db, sql: &str) -> Vec<String> {
    db.run(sql)
        .schema
        .fields
        .iter()
        .map(|f| f.name.clone())
        .collect()
}

fn detail(err: &SqlError) -> String {
    err.to_string()
}

// =======================================================================
// Parser
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

    #[test]
    fn a_derived_table_parses_with_its_alias_and_body() {
        let s = select("SELECT x FROM (SELECT id AS x FROM t) AS d");
        let first = &s.from.as_ref().unwrap().first;
        assert!(first.derived.is_some());
        assert!(!first.lateral);
        assert_eq!(first.alias.as_ref().unwrap().value, "d");
        assert!(first.column_aliases.is_empty());
    }

    #[test]
    fn a_derived_table_without_an_alias_parses_and_is_left_for_the_binder_to_refuse() {
        let s = select("SELECT 1 FROM (SELECT id FROM t)");
        let first = &s.from.as_ref().unwrap().first;
        assert!(first.derived.is_some());
        assert!(first.alias.is_none());
    }

    #[test]
    fn a_column_name_list_parses_on_a_derived_table() {
        let s = select("SELECT a FROM (SELECT id, name FROM t) AS d(a, b)");
        let cols: Vec<&str> = s
            .from
            .as_ref()
            .unwrap()
            .first
            .column_aliases
            .iter()
            .map(|c| c.value.as_str())
            .collect();
        assert_eq!(cols, ["a", "b"]);
    }

    #[test]
    fn nested_derived_tables_parse() {
        let s = select("SELECT z FROM (SELECT y AS z FROM (SELECT id AS y FROM t) AS a) AS b");
        let inner = s.from.as_ref().unwrap().first.derived.as_ref().unwrap();
        assert!(inner.from.as_ref().unwrap().first.derived.is_some());
    }

    #[test]
    fn a_derived_table_may_be_joined_and_lateral_is_flagged() {
        let s = select(
            "SELECT 1 FROM t JOIN LATERAL (SELECT 1 FROM orders WHERE orders.t_id = t.id) AS l ON true",
        );
        let join = &s.from.as_ref().unwrap().joins[0];
        assert!(join.table.lateral);
        assert!(join.table.derived.is_some());
        let s = select("SELECT 1 FROM t JOIN (SELECT 1 AS a) AS l ON true");
        assert!(!s.from.as_ref().unwrap().joins[0].table.lateral);
    }

    #[test]
    fn a_with_list_parses_into_ctes() {
        let s = select("WITH a AS (SELECT 1 AS x), b(p, q) AS (SELECT 1, 2) SELECT * FROM a");
        assert_eq!(s.ctes.len(), 2);
        assert_eq!(s.ctes[0].name.value, "a");
        assert!(s.ctes[0].columns.is_empty());
        let cols: Vec<&str> = s.ctes[1].columns.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(cols, ["p", "q"]);
    }

    #[test]
    fn with_before_update_delete_and_insert_attaches_the_list_to_the_statement() {
        let limits = SqlLimits::default();
        for (sql, which) in [
            ("WITH x AS (SELECT 1 AS v) DELETE FROM t WHERE id IN (SELECT v FROM x)", 'd'),
            ("WITH x AS (SELECT 1 AS v) UPDATE t SET name = 'a' WHERE id IN (SELECT v FROM x)", 'u'),
            ("WITH x AS (SELECT 1 AS v) INSERT INTO t (id) VALUES (1)", 'i'),
        ] {
            let ctes = match parse_statement(sql, &limits)
                .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
            {
                Statement::Delete(d) => {
                    assert_eq!(which, 'd');
                    d.ctes
                }
                Statement::Update(u) => {
                    assert_eq!(which, 'u');
                    u.ctes
                }
                Statement::Insert(i) => {
                    assert_eq!(which, 'i');
                    i.ctes
                }
                other => panic!("{other:?}"),
            };
            assert_eq!(ctes.len(), 1, "{sql}");
        }
    }

    #[test]
    fn what_is_not_in_scope_is_a_typed_unsupported_error() {
        for sql in [
            "WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r",
            "WITH a AS (SELECT 1 UNION SELECT 2) SELECT * FROM a",
            "SELECT * FROM (SELECT 1 UNION SELECT 2) AS u",
            "UPDATE (SELECT 1) AS d SET a = 1",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(err, crate::error::SqlError::Unsupported { .. }),
                "{sql:?} -> {err:?}"
            );
        }
        // Twin: the non-recursive forms next to them DO parse (so the
        // refusals above are about recursion / set operations / DML targets,
        // not a blanket refusal of WITH or of derived tables).
        for sql in [
            "WITH r AS (SELECT 1) SELECT * FROM r",
            "SELECT * FROM (SELECT 1 AS a) AS d",
        ] {
            parse_statement(sql, &SqlLimits::default())
                .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"));
        }
    }
}

// =======================================================================
// C1 / C2 -- binder: derived tables
// =======================================================================

mod binder {
    use super::*;
    use crate::auth::AuthContext;
    use crate::limits::SqlLimits;
    use rubixdb::catalog::schema::{ObjectKind, Privilege};

    fn seeded(tag: &str) -> Db {
        Db::new(tag)
    }

    #[test]
    fn a_derived_table_exposes_the_inner_select_lists_names_not_the_tables() {
        let db = seeded("c1_names");
        // Inner SELECT renames id -> x, name -> y: the outer query sees x, y.
        let names = column_names(&db, "SELECT x, y FROM (SELECT id AS x, name AS y FROM t) AS d");
        assert_eq!(names, ["x", "y"]);
        // The underlying names are NOT visible through the derived table.
        let err = db
            .try_bind("SELECT id FROM (SELECT id AS x FROM t) AS d")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "column", .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn select_star_over_a_derived_table_uses_the_inner_output_names() {
        let db = seeded("c1_star");
        assert_eq!(
            column_names(&db, "SELECT * FROM (SELECT id AS x, name FROM t) AS d"),
            ["x", "name"]
        );
        assert_eq!(
            column_names(&db, "SELECT d.* FROM (SELECT name AS who, id FROM t) AS d"),
            ["who", "id"]
        );
        // An inner `SELECT *` exposes the underlying table's names.
        assert_eq!(
            column_names(&db, "SELECT * FROM (SELECT * FROM t) AS d"),
            ["id", "name", "active"]
        );
        db.cleanup();
    }

    #[test]
    fn a_derived_table_requires_an_alias_and_none_is_invented() {
        let db = seeded("c1_alias");
        let err = db.try_bind("SELECT 1 FROM (SELECT id FROM t)").unwrap_err();
        assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{err:?}");
        assert!(detail(&err).contains("alias"), "{err}");
        db.cleanup();
    }

    #[test]
    fn a_derived_table_alias_must_not_collide_with_another_from_item() {
        let db = seeded("c1_dup_alias");
        for sql in [
            "SELECT 1 FROM t JOIN (SELECT id FROM t) AS t ON true",
            "SELECT 1 FROM (SELECT 1 AS a) AS d JOIN (SELECT 2 AS b) AS d ON true",
            "SELECT 1 FROM orders AS d JOIN (SELECT 2 AS b) AS d ON true",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{sql}: {err:?}");
            assert!(detail(&err).contains("duplicate"), "{sql}: {err}");
        }
        // A derived alias that merely equals a real table name (not in the same
        // FROM) shadows nothing and is fine.
        assert!(db.try_bind("SELECT x FROM (SELECT id AS x FROM t) AS orders").is_ok());
        db.cleanup();
    }

    #[test]
    fn a_column_name_list_renames_and_must_match_the_column_count_exactly() {
        let db = seeded("c1_collist");
        assert_eq!(
            column_names(&db, "SELECT a, b FROM (SELECT id, name FROM t) AS d(a, b)"),
            ["a", "b"]
        );
        // The inner names are replaced, not supplemented.
        assert!(db.try_bind("SELECT id FROM (SELECT id, name FROM t) AS d(a, b)").is_err());
        for sql in [
            "SELECT 1 FROM (SELECT id, name FROM t) AS d(a)",
            "SELECT 1 FROM (SELECT id, name FROM t) AS d(a, b, c)",
            "SELECT 1 FROM (SELECT id FROM t) AS d(a, b)",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{sql}: {err:?}");
            assert!(detail(&err).contains("column name"), "{sql}: {err}");
        }
        // A column list on a base table alias is not part of this slice.
        let err = db.try_bind("SELECT a FROM t AS x(a, b, c)").unwrap_err();
        assert!(matches!(err, SqlError::Unsupported { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn column_types_and_nullability_flow_through_the_derived_table() {
        let db = seeded("c1_types");
        // INTEGER stays INTEGER: comparing with a TEXT is a bind error.
        let err = db
            .try_bind("SELECT 1 FROM (SELECT id AS x FROM t) AS d WHERE d.x = 'a'")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        assert!(db.try_bind("SELECT 1 FROM (SELECT id AS x FROM t) AS d WHERE d.x = 3").is_ok());
        // The result schema reports the derived column's nullability.
        let r = db.run("SELECT x, y FROM (SELECT id AS x, name AS y FROM t) AS d");
        assert!(!r.schema.fields[0].nullable);
        assert!(r.schema.fields[1].nullable);
        db.cleanup();
    }

    #[test]
    fn a_column_with_no_determinable_type_is_refused_not_guessed() {
        let db = seeded("c1_null_type");
        let err = db.try_bind("SELECT a FROM (SELECT NULL AS a) AS d").unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        assert!(detail(&err).contains("type"), "{err}");
        db.cleanup();
    }

    #[test]
    fn two_columns_with_the_same_name_make_a_reference_to_it_ambiguous() {
        let db = seeded("c1_dup_cols");
        let err = db
            .try_bind("SELECT a FROM (SELECT id AS a, id AS a FROM t) AS d")
            .unwrap_err();
        assert!(matches!(err, SqlError::AmbiguousColumn { .. }), "{err:?}");
        // Qualified, too; but `*` is fine (no name is looked up).
        assert!(db
            .try_bind("SELECT d.a FROM (SELECT id AS a, id AS a FROM t) AS d")
            .is_err());
        assert!(db.try_bind("SELECT * FROM (SELECT id AS a, id AS a FROM t) AS d").is_ok());
        db.cleanup();
    }

    #[test]
    fn a_derived_body_cannot_reference_a_name_its_select_list_does_not_expose() {
        // Three levels: the innermost reads `id`, the middle exposes only
        // `y`, so the outer cannot read `id` or `x` through it.
        let db = seeded("c1_three_scopes");
        assert!(db
            .try_bind("SELECT z FROM (SELECT y AS z FROM (SELECT id AS y FROM t) AS b) AS c")
            .is_ok());
        for sql in [
            "SELECT id FROM (SELECT y AS z FROM (SELECT id AS y FROM t) AS b) AS c",
            "SELECT y FROM (SELECT y AS z FROM (SELECT id AS y FROM t) AS b) AS c",
            "SELECT b.y FROM (SELECT y AS z FROM (SELECT id AS y FROM t) AS b) AS c",
            "SELECT z FROM (SELECT id AS z FROM (SELECT id AS y FROM t) AS b) AS c",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(matches!(err, SqlError::UnknownObject { .. }), "{sql}: {err:?}");
        }
        db.cleanup();
    }

    // ---- C2: a non-LATERAL derived table cannot read outer columns ----------

    #[test]
    fn a_derived_table_cannot_read_a_preceding_from_item_without_lateral() {
        let db = seeded("c2_sibling");
        let err = db
            .try_bind(
                "SELECT 1 FROM t JOIN (SELECT orders.id FROM orders WHERE orders.t_id = t.id) AS d ON true",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{err:?}");
        assert_eq!(
            detail(&err),
            "invalid identifier: derived table 'd' cannot reference outer column 't.id' without LATERAL"
        );
        db.cleanup();
    }

    #[test]
    fn a_derived_table_cannot_read_the_enclosing_query_of_the_subquery_it_sits_in() {
        let db = seeded("c2_enclosing");
        // Hazard 8: the derived table sees its own subquery's scope? No -- it
        // is a derived table; it sees NEITHER the subquery's FROM items NOR the
        // outer query's.
        for sql in [
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM (SELECT t_id FROM orders WHERE amount > t.id) AS d)",
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o JOIN (SELECT t_id FROM orders WHERE t_id = o.t_id) AS d ON true WHERE o.t_id = t.id)",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(detail(&err).contains("without LATERAL"), "{sql}: {err}");
        }
        // The subquery's own WHERE may still read the outer query AND the
        // derived table's columns.
        assert!(db
            .try_bind(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM (SELECT t_id FROM orders) AS d WHERE d.t_id = t.id)"
            )
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn a_derived_table_nested_in_a_derived_table_cannot_read_the_one_around_it() {
        let db = seeded("c2_nested");
        let err = db
            .try_bind(
                "SELECT 1 FROM (SELECT id FROM t JOIN (SELECT 1 AS k WHERE t.id > 0) AS i ON true) AS o",
            )
            .unwrap_err();
        assert!(detail(&err).contains("derived table 'i'"), "{err}");
        assert!(detail(&err).contains("without LATERAL"), "{err}");
        db.cleanup();
    }

    #[test]
    fn a_name_that_exists_nowhere_is_still_just_unknown() {
        let db = seeded("c2_unknown");
        let err = db
            .try_bind("SELECT 1 FROM t JOIN (SELECT 1 AS a WHERE nonesuch = 1) AS d ON true")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "column", .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_lateral_derived_table_reads_the_items_before_it_and_records_the_dependency() {
        let db = seeded("c2_lateral_bind");
        let s = bound_select(
            &db,
            "SELECT t.id FROM t JOIN LATERAL (SELECT orders.id FROM orders WHERE orders.t_id = t.id) AS l ON true",
        );
        let BoundRelation::Derived(d) = &s.from[1].relation else {
            panic!("expected a derived table");
        };
        assert!(d.lateral);
        assert_eq!(d.outer_refs.len(), 1);
        assert_eq!(d.outer_refs[0].table_ref, s.from[0].table.id);
        // A LATERAL body that reads nothing outside itself records nothing.
        let s = bound_select(&db, "SELECT 1 FROM t JOIN LATERAL (SELECT 1 AS a) AS l ON true");
        let BoundRelation::Derived(d) = &s.from[1].relation else {
            panic!();
        };
        assert!(d.outer_refs.is_empty());
        db.cleanup();
    }

    #[test]
    fn a_lateral_body_cannot_read_a_later_from_item() {
        let db = seeded("c2_lateral_later");
        let err = db
            .try_bind(
                "SELECT 1 FROM t JOIN LATERAL (SELECT 1 AS a WHERE orders.id = 1) AS l ON true JOIN orders ON true",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn lateral_inside_a_correlated_subquery_may_read_the_outer_query_too() {
        let db = seeded("c2_lateral_outer");
        assert!(db
            .try_bind(
                "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o JOIN LATERAL \
                 (SELECT 1 AS k WHERE o.t_id = t.id) AS l ON true)"
            )
            .is_ok());
        db.cleanup();
    }

    // ---- authorization -----------------------------------------------------------

    fn grant_select(db: &Db, principal: &str, table: &str) {
        let id = db.table_id(table);
        db.catalog
            .grant(principal, ObjectKind::Table, id, Privilege::Select)
            .unwrap();
    }

    #[test]
    fn a_forbidden_table_inside_a_derived_table_is_unknown_like_a_missing_one() {
        let db = seeded("c1_authz");
        grant_select(&db, "limited", "t");
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        let forbidden = db
            .try_bind_with("SELECT x FROM (SELECT t_id AS x FROM orders) AS d", &auth, &limits)
            .unwrap_err();
        let missing = db
            .try_bind_with("SELECT x FROM (SELECT t_id AS x FROM no_such) AS d", &auth, &limits)
            .unwrap_err();
        assert!(matches!(forbidden, SqlError::UnknownObject { kind: "table", .. }), "{forbidden:?}");
        assert!(matches!(missing, SqlError::UnknownObject { kind: "table", .. }));
        grant_select(&db, "limited", "orders");
        assert!(db
            .try_bind_with("SELECT x FROM (SELECT t_id AS x FROM orders) AS d", &auth, &limits)
            .is_ok());
        db.cleanup();
    }

    // ---- limits -------------------------------------------------------------------

    fn nested_derived(levels: usize) -> String {
        let mut sql = String::from("SELECT id AS c0 FROM t");
        for i in 1..=levels {
            sql = format!("SELECT c{} AS c{i} FROM ({sql}) AS d{i}", i - 1);
        }
        sql
    }

    #[test]
    fn derived_table_nesting_is_bounded_at_bind_time_exactly_at_the_subquery_depth_limit() {
        let db = seeded("c1_depth");
        let limits = SqlLimits {
            max_expression_depth: 10_000,
            ..SqlLimits::default()
        };
        let admin = AuthContext::admin("p");
        assert!(db.try_bind_with(&nested_derived(32), &admin, &limits).is_ok());
        let err = db.try_bind_with(&nested_derived(33), &admin, &limits).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// C1 -- execution: derived tables
// =======================================================================

mod derived {
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};
    use std::collections::BTreeMap;

    fn ints(rows: &[Row]) -> Vec<i32> {
        rows.iter().map(id_of).collect()
    }

    /// `t_id -> number of orders`, for the orders that have a `t_id`.
    fn counts(orders: &[ORow]) -> BTreeMap<i32, i64> {
        let mut m = BTreeMap::new();
        for o in orders {
            if let Some(t) = o.3 {
                *m.entry(t).or_insert(0i64) += 1;
            }
        }
        m
    }

    #[test]
    fn a_plain_derived_table_returns_the_inner_rows_under_the_inner_names() {
        let (db, _t, _o) = standard_db("c1_plain");
        let got = ints(&db.rows(
            "SELECT d.x FROM (SELECT id AS x FROM t WHERE id <= 10) AS d ORDER BY d.x",
        ));
        assert_eq!(got, (1..=10).collect::<Vec<_>>());
        db.cleanup();
    }

    #[test]
    fn group_by_having_order_by_and_limit_inside_a_derived_table() {
        let (db, _t, orders) = standard_db("c1_grouped");
        let sql = "SELECT g.t_id, g.n FROM (SELECT t_id, COUNT(*) AS n FROM orders \
                   WHERE t_id IS NOT NULL GROUP BY t_id HAVING COUNT(*) >= 3 \
                   ORDER BY COUNT(*) DESC, t_id LIMIT 5) AS g ORDER BY g.n DESC, g.t_id";
        let got = db.rows(sql);
        let mut want: Vec<(i32, i64)> =
            counts(&orders).into_iter().filter(|(_, n)| *n >= 3).collect();
        want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        want.truncate(5);
        let want: Vec<Row> = want.into_iter().map(|(t, n)| vec![int(t), big(n)]).collect();
        assert_eq!(want.len(), 5);
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn an_outer_aggregate_ranges_over_the_derived_tables_rows() {
        let (db, _t, orders) = standard_db("c1_outer_agg");
        let got = db.rows(
            "SELECT COUNT(*), MAX(g.n), SUM(g.n) FROM (SELECT t_id, COUNT(*) AS n FROM orders \
             WHERE t_id IS NOT NULL GROUP BY t_id) AS g",
        );
        let c = counts(&orders);
        let total: i64 = c.values().sum();
        assert_eq!(
            got,
            vec![vec![big(c.len() as i64), big(*c.values().max().unwrap()), big(total)]]
        );
        db.cleanup();
    }

    const G: &str = "(SELECT t_id, COUNT(*) AS n FROM orders WHERE t_id IS NOT NULL GROUP BY t_id)";

    #[test]
    fn a_derived_table_on_the_right_of_a_join_is_read_once_per_left_row_from_one_materialization() {
        let (db, t, orders) = standard_db("c1_join_right");
        let c = counts(&orders);
        let got = db.rows(&format!(
            "SELECT t.id, g.n FROM t JOIN {G} AS g ON g.t_id = t.id ORDER BY t.id"
        ));
        let want: Vec<Row> = t
            .iter()
            .filter_map(|r| c.get(&r.0).map(|n| vec![int(r.0), big(*n)]))
            .collect();
        assert_eq!(got, want);
        // LEFT JOIN keeps the unmatched outer rows with NULLs.
        let got = db.rows(&format!(
            "SELECT t.id, g.n FROM t LEFT JOIN {G} AS g ON g.t_id = t.id ORDER BY t.id"
        ));
        let want: Vec<Row> = t
            .iter()
            .map(|r| vec![int(r.0), c.get(&r.0).map(|n| big(*n)).unwrap_or(NULL)])
            .collect();
        assert_eq!(got.len(), 60);
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_derived_table_on_the_left_and_two_derived_tables_in_one_from() {
        let (db, t, orders) = standard_db("c1_join_left");
        let c = counts(&orders);
        let got = db.rows(&format!(
            "SELECT g.t_id, t.name FROM {G} AS g JOIN t ON t.id = g.t_id ORDER BY g.t_id"
        ));
        let want: Vec<Row> = c
            .keys()
            .filter_map(|k| t.iter().find(|r| r.0 == *k))
            .map(|r| vec![int(r.0), r.1.as_deref().map(text).unwrap_or(NULL)])
            .collect();
        assert_eq!(got, want);
        let got = db.rows(&format!(
            "SELECT a.t_id, b.n FROM {G} AS a JOIN {G} AS b ON b.t_id = a.t_id ORDER BY a.t_id"
        ));
        let want: Vec<Row> = c.iter().map(|(k, n)| vec![int(*k), big(*n)]).collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn limit_without_order_by_returns_at_most_limit_rows_and_order_by_with_limit_is_applied() {
        let (db, _t, _o) = standard_db("c1_limit");
        let n = db.rows("SELECT COUNT(*) FROM (SELECT id FROM t LIMIT 7) AS d");
        assert_eq!(n, vec![vec![big(7)]]);
        let n = db.rows("SELECT COUNT(*) FROM (SELECT id FROM t LIMIT 100) AS d");
        assert_eq!(n, vec![vec![big(60)]]);
        let top = ints(&db.rows(
            "SELECT x FROM (SELECT id AS x FROM t ORDER BY id DESC LIMIT 3) AS d ORDER BY x",
        ));
        assert_eq!(top, vec![58, 59, 60]);
        // ORDER BY without LIMIT is accepted and applied (the outer query
        // makes no promise about order unless it orders itself).
        let all = ints(&db.rows("SELECT x FROM (SELECT id AS x FROM t ORDER BY id DESC) AS d ORDER BY x"));
        assert_eq!(all, (1..=60).collect::<Vec<_>>());
        db.cleanup();
    }

    #[test]
    fn empty_and_all_null_derived_tables_do_not_crash() {
        let (db, _t, _o) = standard_db("c1_empty_null");
        assert!(db.rows("SELECT * FROM (SELECT id FROM t WHERE id > 1000) AS d").is_empty());
        assert_eq!(
            db.rows("SELECT COUNT(*), MAX(x) FROM (SELECT id AS x FROM t WHERE id > 1000) AS d"),
            vec![vec![big(0), NULL]]
        );
        // id 45: name and active are both NULL.
        assert_eq!(
            db.rows("SELECT a, b FROM (SELECT name AS a, active AS b FROM t WHERE id = 45) AS d"),
            vec![vec![NULL, NULL]]
        );
        assert_eq!(
            db.rows("SELECT COUNT(*) FROM (SELECT name AS a FROM t WHERE id = 45) AS d WHERE d.a IS NULL"),
            vec![vec![big(1)]]
        );
        db.cleanup();
    }

    #[test]
    fn three_derived_tables_deep() {
        let (db, _t, _o) = standard_db("c1_three_deep");
        let got = ints(&db.rows(
            "SELECT z FROM (SELECT y AS z FROM (SELECT x AS y FROM (SELECT id AS x FROM t WHERE id < 5) AS a) AS b) AS c ORDER BY z",
        ));
        assert_eq!(got, vec![1, 2, 3, 4]);
        db.cleanup();
    }

    #[test]
    fn a_derived_body_may_contain_subqueries_and_a_derived_table_may_sit_inside_a_subquery() {
        let (db, t, orders) = standard_db("c1_mixed");
        let idx = by_t_id(&orders);
        // Subqueries (EXISTS, IN) inside the derived body.
        let got = ints(&db.rows(
            "SELECT d.id FROM (SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id) \
             AND id IN (SELECT t_id FROM orders WHERE amount > 90)) AS d ORDER BY d.id",
        ));
        let want: Vec<i32> = t
            .iter()
            .map(|r| r.0)
            .filter(|i| {
                idx.contains_key(i)
                    && orders.iter().any(|o| o.3 == Some(*i) && o.2.is_some_and(|a| a > 90))
            })
            .collect();
        assert!(!want.is_empty());
        assert_eq!(got, want);
        // A derived table inside a correlated subquery: the subquery's WHERE
        // reads the outer t.id, the derived body reads only its own tables.
        let got = db.rows(
            "SELECT id, (SELECT COUNT(*) FROM (SELECT t_id FROM orders WHERE amount > 50) AS d WHERE d.t_id = t.id) \
             FROM t ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .map(|r| {
                let n = orders.iter().filter(|o| o.3 == Some(r.0) && o.2.is_some_and(|a| a > 50)).count();
                vec![int(r.0), big(n as i64)]
            })
            .collect();
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn distinct_and_qualified_wildcard_over_derived_tables() {
        let (db, _t, orders) = standard_db("c1_distinct");
        let got = db.rows_sorted(
            "SELECT d.* FROM (SELECT DISTINCT customer FROM orders) AS d",
        );
        let mut names: Vec<String> = orders.iter().map(|o| o.1.clone()).collect();
        names.sort();
        names.dedup();
        let mut want: Vec<Row> = names.iter().map(|n| vec![text(n)]).collect();
        want.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(got, want);
        assert_eq!(
            db.rows("SELECT * FROM (SELECT id, name FROM t WHERE id = 1) AS d"),
            vec![vec![int(1), text("n1")]]
        );
        db.cleanup();
    }

    #[test]
    fn a_derived_table_in_delete_and_update_where() {
        let (db, _t, _o) = standard_db("c1_dml");
        let r = db.write(
            "DELETE FROM t WHERE id IN (SELECT x FROM (SELECT id AS x FROM t WHERE id > 55) AS d)",
        );
        assert_eq!(r.rows_affected, 5);
        let r = db.write(
            "UPDATE t SET name = 'z' WHERE id IN (SELECT x FROM (SELECT id AS x FROM t WHERE id <= 3) AS d)",
        );
        assert_eq!(r.rows_affected, 3);
        assert_eq!(db.rows("SELECT COUNT(*) FROM t WHERE name = 'z'"), vec![vec![big(3)]]);
        db.cleanup();
    }

    #[test]
    fn parameters_inside_a_derived_body_are_bound_at_execution() {
        let (db, _t, _o) = standard_db("c1_param");
        let r = db
            .try_run_with(
                "SELECT COUNT(*) FROM (SELECT id FROM t WHERE id > $1) AS d",
                &ExecLimits::default(),
                &ExecMetrics::default(),
                &[int(50)],
            )
            .unwrap();
        assert_eq!(r.rows, vec![vec![big(10)]]);
        db.cleanup();
    }

    #[test]
    fn a_rescanned_derived_table_is_bounded_by_max_materialized_rows() {
        let (db, _t, _o) = standard_db("c1_bound_rows");
        let sql = "SELECT t.id FROM t JOIN (SELECT id AS k FROM t) AS d ON d.k = t.id";
        assert!(db.try_run(sql).is_ok());
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_materialized_rows: 10, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_materialized_bytes: 100, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn max_derived_table_depth_is_enforced_exactly_before_any_work() {
        let (db, _t, _o) = standard_db("c1_exec_depth");
        let sql = "SELECT c FROM (SELECT b AS c FROM (SELECT a AS b FROM (SELECT id AS a FROM t) AS x) AS y) AS z";
        let run = |n: u32| {
            db.try_run_with(
                sql,
                &ExecLimits { max_derived_table_depth: n, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
        };
        assert_eq!(run(3).unwrap().rows.len(), 60);
        let err = run(2).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_derived_table_depth"), "{err}");
        db.cleanup();
    }

    #[test]
    fn explain_shows_a_derived_table_as_an_uncorrelated_subplan() {
        let (db, _t, _o) = standard_db("c1_explain");
        let text = crate::plan::explain(&db.plan(&format!(
            "EXPLAIN SELECT t.id, g.n FROM t JOIN {G} AS g ON g.t_id = t.id"
        )));
        assert!(text.contains("Derived #"), "{text}");
        assert!(text.contains("alias=g"), "{text}");
        assert!(text.contains("outer_row_input=false"), "{text}");
        assert!(text.contains("materialized once"), "the right side of a join: {text}");
        let text = crate::plan::explain(&db.plan("EXPLAIN SELECT x FROM (SELECT id AS x FROM t) AS d"));
        assert!(text.contains("outer_row_input=false") && !text.contains("materialized once"), "{text}");
        db.cleanup();
    }
}

// =======================================================================
// C2 -- LATERAL derived tables (implemented; supported spelling is
// `[LEFT] JOIN LATERAL (...) alias ON cond`)
// =======================================================================

mod lateral {
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};

    #[test]
    fn top_one_per_outer_row_with_inner_and_left_join_lateral() {
        let (db, t, orders) = standard_db("c2_top1");
        let idx = by_t_id(&orders);
        let sql_inner = "SELECT t.id, l.oid FROM t JOIN LATERAL (SELECT orders.id AS oid FROM orders \
                         WHERE orders.t_id = t.id ORDER BY orders.id DESC LIMIT 1) AS l ON true ORDER BY t.id";
        let got = db.rows(sql_inner);
        let want: Vec<Row> = t
            .iter()
            .filter_map(|r| {
                idx.get(&r.0)
                    .and_then(|os| os.iter().map(|o| o.0).max())
                    .map(|m| vec![int(r.0), big(m)])
            })
            .collect();
        assert!(!want.is_empty() && want.len() < 60);
        assert_eq!(got, want);
        // LEFT keeps every outer row, with NULL where the body is empty.
        let got = db.rows(&sql_inner.replace("JOIN LATERAL", "LEFT JOIN LATERAL"));
        let want: Vec<Row> = t
            .iter()
            .map(|r| {
                let top = idx.get(&r.0).and_then(|os| os.iter().map(|o| o.0).max());
                vec![int(r.0), top.map(big).unwrap_or(NULL)]
            })
            .collect();
        assert_eq!(got.len(), 60);
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn a_lateral_body_is_re_run_once_per_left_row_through_the_correlated_gate() {
        let (db, _t, _o) = standard_db("c2_gate");
        let sql = "SELECT t.id FROM t JOIN LATERAL (SELECT 1 AS k FROM orders WHERE orders.t_id = t.id LIMIT 1) AS l ON true";
        let metrics = ExecMetrics::default();
        db.try_run_with(sql, &ExecLimits::default(), &metrics, &[]).unwrap();
        assert_eq!(metrics.snapshot().correlated_subquery_reexecs, 60);
        // The shared limits apply.
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_correlated_reexecutions: 59, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_subquery_outer_rows: 10, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_lateral_body_that_reads_nothing_before_it_is_not_correlated() {
        let (db, _t, _o) = standard_db("c2_uncorrelated");
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with(
                "SELECT t.id FROM t JOIN LATERAL (SELECT 1 AS k) AS l ON true",
                &ExecLimits::default(),
                &metrics,
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 60);
        assert_eq!(metrics.snapshot().correlated_subquery_reexecs, 0);
        db.cleanup();
    }

    #[test]
    fn lateral_can_read_both_the_items_before_it_and_a_subquerys_enclosing_query() {
        let (db, t, orders) = standard_db("c2_lateral_deep");
        // For every t: is there an order o whose lateral body (reading o and
        // the outer t) finds a row?
        let got = db.rows(
            "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM orders o JOIN LATERAL \
             (SELECT 1 AS k WHERE o.t_id = t.id AND o.amount > 90) AS l ON true) ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .filter(|r| orders.iter().any(|o| o.3 == Some(r.0) && o.2.is_some_and(|a| a > 90)))
            .map(|r| vec![int(r.0)])
            .collect();
        assert!(!want.is_empty());
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn explain_marks_a_lateral_derived_table_as_taking_the_outer_row() {
        let (db, _t, _o) = standard_db("c2_explain");
        let text = crate::plan::explain(&db.plan(
            "EXPLAIN SELECT t.id FROM t JOIN LATERAL (SELECT 1 AS k FROM orders WHERE orders.t_id = t.id) AS l ON true",
        ));
        assert!(text.contains("outer_row_input=true"), "{text}");
        assert!(text.contains("LATERAL"), "{text}");
        db.cleanup();
    }

    #[test]
    fn a_non_lateral_derived_table_with_the_same_body_is_refused() {
        let (db, _t, _o) = standard_db("c2_vs_plain");
        let err = db
            .try_run("SELECT t.id FROM t JOIN (SELECT 1 AS k FROM orders WHERE orders.t_id = t.id) AS l ON true")
            .unwrap_err();
        assert!(err.to_string().contains("without LATERAL"), "{err}");
        db.cleanup();
    }
}

// =======================================================================
// C3 - C6 -- binder: non-recursive CTEs
// =======================================================================

mod cte_binder {
    use super::*;
    use crate::auth::AuthContext;
    use crate::limits::SqlLimits;
    use rubixdb::catalog::schema::{ObjectKind, Privilege};

    #[test]
    fn a_cte_shadows_a_base_table_of_the_same_name_for_the_whole_statement() {
        let db = Db::new("c3_shadow");
        let s = bound_select(&db, "WITH orders AS (SELECT 1 AS a) SELECT a FROM orders");
        assert!(matches!(s.from[0].relation, BoundRelation::Cte(_)));
        assert_eq!(s.ctes.len(), 1);
        // Through a subquery, too.
        let s = bound_select(
            &db,
            "WITH orders AS (SELECT 1 AS a) SELECT id FROM t WHERE id IN (SELECT a FROM orders)",
        );
        assert_eq!(s.ctes.len(), 1);
        db.cleanup();
    }

    #[test]
    fn a_qualified_name_is_never_a_cte() {
        let db = Db::new("c3_qualified");
        let (db2, _t, orders) = standard_db("c3_qualified_exec");
        let got = db2.rows("WITH orders AS (SELECT 1 AS a) SELECT COUNT(*) FROM public.orders");
        assert_eq!(got, vec![vec![big(orders.len() as i64)]]);
        let s = bound_select(&db, "WITH orders AS (SELECT 1 AS a) SELECT 1 FROM public.orders");
        assert!(matches!(s.from[0].relation, BoundRelation::Table));
        db.cleanup();
        db2.cleanup();
    }

    #[test]
    fn cte_columns_come_from_the_body_output_names() {
        let db = Db::new("c3_names");
        assert_eq!(
            column_names(&db, "WITH x AS (SELECT id AS k, name AS who FROM t) SELECT * FROM x"),
            ["k", "who"]
        );
        let err = db
            .try_bind("WITH x AS (SELECT id AS k FROM t) SELECT id FROM x")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "column", .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_cte_column_name_list_renames_and_must_match_exactly() {
        let db = Db::new("c3_collist");
        assert_eq!(
            column_names(&db, "WITH x(a, b) AS (SELECT id, name FROM t) SELECT b, a FROM x"),
            ["b", "a"]
        );
        for sql in [
            "WITH x(a) AS (SELECT id, name FROM t) SELECT * FROM x",
            "WITH x(a, b, c) AS (SELECT id, name FROM t) SELECT * FROM x",
            "WITH x(a, b) AS (SELECT id FROM t) SELECT * FROM x",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{sql}: {err:?}");
            assert!(detail(&err).contains("column name"), "{sql}: {err}");
        }
        // A list on a *reference* renames again, with the same exact-count rule.
        assert_eq!(
            column_names(&db, "WITH x AS (SELECT id, name FROM t) SELECT p, q FROM x AS y(p, q)"),
            ["p", "q"]
        );
        let err = db
            .try_bind("WITH x AS (SELECT id, name FROM t) SELECT * FROM x AS y(p)")
            .unwrap_err();
        assert!(detail(&err).contains("column name"), "{err}");
        db.cleanup();
    }

    #[test]
    fn a_cte_referencing_itself_without_recursive_is_a_clear_bind_error() {
        let db = Db::new("c4_self");
        let err = db
            .try_bind("WITH x AS (SELECT * FROM x) SELECT * FROM x")
            .unwrap_err();
        assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{err:?}");
        assert!(detail(&err).contains("CTE 'x' refers to itself; use WITH RECURSIVE"), "{err}");
        // Also from inside a subquery or a derived table in its own body.
        for sql in [
            "WITH x AS (SELECT (SELECT COUNT(*) FROM x) AS n) SELECT * FROM x",
            "WITH x AS (SELECT n FROM (SELECT 1 AS n FROM x) AS d) SELECT * FROM x",
        ] {
            let err = db.try_bind(sql).unwrap_err();
            assert!(detail(&err).contains("refers to itself"), "{sql}: {err}");
        }
        db.cleanup();
    }

    #[test]
    fn inside_its_own_body_a_ctes_name_means_the_catalog_table_if_there_is_one() {
        // SQL scoping: the CTE's name is not visible in its own body, so the
        // name there is the real table; only afterwards does the CTE win.
        let (db, _t, orders) = standard_db("c4_self_table");
        let s = bound_select(&db, "WITH orders AS (SELECT id FROM orders WHERE id < 1020) SELECT id FROM orders");
        assert!(matches!(s.ctes[0].query.from[0].relation, BoundRelation::Table));
        assert!(matches!(s.from[0].relation, BoundRelation::Cte(_)));
        let want = orders.iter().filter(|o| o.0 < 1020).count();
        let got = db.rows("WITH orders AS (SELECT id FROM orders WHERE id < 1020) SELECT COUNT(*) FROM orders");
        assert_eq!(got, vec![vec![big(want as i64)]]);
        assert!(want > 0 && want < orders.len());
        db.cleanup();
    }

    #[test]
    fn a_cte_may_use_earlier_ctes_but_never_later_ones() {
        let db = Db::new("c4_order");
        assert!(db
            .try_bind("WITH a AS (SELECT 1 AS v), b AS (SELECT v FROM a), c AS (SELECT v FROM b) SELECT v FROM c")
            .is_ok());
        let err = db
            .try_bind("WITH a AS (SELECT v FROM b), b AS (SELECT 1 AS v) SELECT v FROM a")
            .unwrap_err();
        assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{err:?}");
        assert!(detail(&err).contains("referenced before it is defined"), "{err}");
        // Later CTEs of the same list are not visible from a nested position either.
        let err = db
            .try_bind("WITH a AS (SELECT (SELECT v FROM b) AS w), b AS (SELECT 1 AS v) SELECT w FROM a")
            .unwrap_err();
        assert!(detail(&err).contains("referenced before it is defined"), "{err}");
        db.cleanup();
    }

    #[test]
    fn duplicate_cte_names_and_duplicate_reference_aliases_are_errors() {
        let db = Db::new("c4_dups");
        let err = db
            .try_bind("WITH x AS (SELECT 1 AS v), x AS (SELECT 2 AS v) SELECT * FROM x")
            .unwrap_err();
        assert!(detail(&err).contains("more than once"), "{err}");
        let err = db
            .try_bind("WITH x AS (SELECT 1 AS v) SELECT 1 FROM x JOIN x ON true")
            .unwrap_err();
        assert!(detail(&err).contains("duplicate"), "{err}");
        assert!(db
            .try_bind("WITH x AS (SELECT 1 AS v) SELECT 1 FROM x AS a JOIN x AS b ON a.v = b.v")
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn a_cte_body_cannot_read_a_column_of_an_enclosing_query() {
        let db = Db::new("c6_cte_outer");
        let err = db
            .try_bind(
                "SELECT id FROM t WHERE EXISTS (WITH x AS (SELECT 1 AS v WHERE t.id > 0) SELECT v FROM x)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{err:?}");
        assert_eq!(
            detail(&err),
            "invalid identifier: CTE 'x' cannot reference outer column 't.id'"
        );
        // ...and not a column of a FROM item next to it either.
        let err = db
            .try_bind("WITH x AS (SELECT t.id AS v) SELECT 1 FROM t")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn ctes_are_scoped_to_the_query_that_wrote_them() {
        let db = Db::new("c6_scope");
        // Visible inside a subquery / derived table / join branch of that query.
        assert!(db
            .try_bind("WITH x AS (SELECT 5 AS v) SELECT (SELECT v FROM x), d.v FROM (SELECT v FROM x) AS d")
            .is_ok());
        // WITH inside a scalar subquery does not leak into the enclosing query.
        assert!(db.try_bind("SELECT (WITH x AS (SELECT 5 AS v) SELECT v FROM x)").is_ok());
        let err = db
            .try_bind("SELECT (WITH x AS (SELECT 5 AS v) SELECT v FROM x), (SELECT v FROM x)")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{err:?}");
        // Nor out of a derived table.
        let err = db
            .try_bind("SELECT 1 FROM (WITH x AS (SELECT 5 AS v) SELECT v FROM x) AS d JOIN x ON true")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn an_inner_with_shadows_an_outer_cte_of_the_same_name_only_inside() {
        let (db, _t, _o) = standard_db("c6_inner_shadow");
        let got = db.rows(
            "WITH x AS (SELECT 1 AS v) SELECT (WITH x AS (SELECT 2 AS v) SELECT v FROM x), (SELECT v FROM x)",
        );
        assert_eq!(got, vec![vec![int(2), int(1)]]);
        db.cleanup();
    }

    #[test]
    fn a_dead_cte_is_still_bound_so_a_bad_one_is_an_error() {
        let db = Db::new("c3_dead_bad");
        assert!(db.try_bind("WITH x AS (SELECT id FROM t) SELECT 1").is_ok());
        let err = db
            .try_bind("WITH x AS (SELECT id FROM no_such_table) SELECT 1")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_forbidden_table_inside_a_cte_is_unknown_even_if_the_cte_is_never_read() {
        let db = Db::new("c3_authz");
        let t = db.table_id("t");
        db.catalog
            .grant("limited", ObjectKind::Table, t, Privilege::Select)
            .unwrap();
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        for sql in [
            "WITH x AS (SELECT t_id FROM orders) SELECT * FROM x",
            "WITH x AS (SELECT t_id FROM orders) SELECT 1",
            "WITH x AS (SELECT 1 AS v), y AS (SELECT t_id FROM orders) SELECT * FROM x",
        ] {
            let err = db.try_bind_with(sql, &auth, &limits).unwrap_err();
            assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{sql}: {err:?}");
        }
        assert!(db
            .try_bind_with("WITH x AS (SELECT id FROM t) SELECT * FROM x", &auth, &limits)
            .is_ok());
        db.cleanup();
    }

    #[test]
    fn parameters_in_a_cte_body_count_toward_the_statements_maximum() {
        let db = Db::new("c3_params");
        let s = bound_select(&db, "WITH x AS (SELECT id FROM t WHERE id > $2) SELECT id FROM x WHERE id < $1");
        assert_eq!(s.max_parameter, 2);
        db.cleanup();
    }

    #[test]
    fn a_cte_column_with_no_determinable_type_is_refused() {
        let db = Db::new("c3_null_type");
        let err = db.try_bind("WITH x AS (SELECT NULL AS a) SELECT a FROM x").unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// C3 - C6 -- execution: non-recursive CTEs
// =======================================================================

mod cte {
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};
    use std::collections::BTreeMap;

    fn counts(orders: &[ORow]) -> BTreeMap<i32, i64> {
        let mut m = BTreeMap::new();
        for o in orders {
            if let Some(t) = o.3 {
                *m.entry(t).or_insert(0i64) += 1;
            }
        }
        m
    }

    fn run_m(db: &Db, sql: &str) -> (Vec<Row>, crate::exec::ExecMetricsSnapshot) {
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with(sql, &ExecLimits::default(), &metrics, &[])
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        (r.rows, metrics.snapshot())
    }

    #[test]
    fn a_single_reference_cte_returns_the_body_rows_and_materializes_once() {
        let (db, t, _o) = standard_db("c3_single");
        let (rows, m) = run_m(
            &db,
            "WITH x AS (SELECT id AS k, name FROM t WHERE id <= 5) SELECT k, name FROM x ORDER BY k",
        );
        let want: Vec<Row> = t
            .iter()
            .take(5)
            .map(|r| vec![int(r.0), r.1.as_deref().map(text).unwrap_or(NULL)])
            .collect();
        assert_eq!(rows, want);
        assert_eq!(m.cte_materializations_total, 1, "{m:?}");
        db.cleanup();
    }

    #[test]
    fn a_cte_joined_to_a_base_table_and_filtered_by_the_outer_query() {
        let (db, t, orders) = standard_db("c3_join");
        let c = counts(&orders);
        let (rows, m) = run_m(
            &db,
            "WITH g AS (SELECT t_id, COUNT(*) AS n FROM orders WHERE t_id IS NOT NULL GROUP BY t_id) \
             SELECT t.id, g.n FROM t JOIN g ON g.t_id = t.id WHERE g.n >= 3 ORDER BY t.id",
        );
        let want: Vec<Row> = t
            .iter()
            .filter_map(|r| c.get(&r.0).filter(|n| **n >= 3).map(|n| vec![int(r.0), big(*n)]))
            .collect();
        assert!(!want.is_empty());
        assert_eq!(rows, want);
        assert_eq!(m.cte_materializations_total, 1);
        db.cleanup();
    }

    #[test]
    fn chained_ctes_see_earlier_ones_and_each_materializes_once() {
        let (db, t, _o) = standard_db("c4_chain");
        let (rows, m) = run_m(
            &db,
            "WITH a AS (SELECT id FROM t WHERE id <= 20), b AS (SELECT id FROM a WHERE id > 10), \
             c AS (SELECT id * 2 AS dbl FROM b) SELECT dbl FROM c ORDER BY dbl",
        );
        let want: Vec<Row> = t.iter().map(|r| r.0).filter(|i| *i > 10 && *i <= 20).map(|i| vec![int(i * 2)]).collect();
        assert_eq!(rows, want);
        assert_eq!(m.cte_materializations_total, 3, "{m:?}");
        db.cleanup();
    }

    #[test]
    fn multiple_ctes_in_one_with_are_independent_relations() {
        let (db, _t, orders) = standard_db("c4_multi");
        let (rows, m) = run_m(
            &db,
            "WITH lo AS (SELECT id FROM t WHERE id <= 3), hi AS (SELECT id FROM t WHERE id >= 58) \
             SELECT lo.id, hi.id FROM lo JOIN hi ON true ORDER BY lo.id, hi.id",
        );
        assert_eq!(rows.len(), 3 * 3);
        assert_eq!(rows[0], vec![int(1), int(58)]);
        assert_eq!(m.cte_materializations_total, 2);
        let _ = orders;
        db.cleanup();
    }

    #[test]
    fn a_cte_referenced_twice_is_materialized_once_and_both_references_see_the_same_rows() {
        let (db, t, orders) = standard_db("c5_twice");
        let c = counts(&orders);
        let body = "SELECT t_id, COUNT(*) AS n FROM orders WHERE t_id IS NOT NULL GROUP BY t_id";
        // Two references joined to each other.
        let (rows, m_twice) = run_m(
            &db,
            &format!(
                "WITH x AS ({body}) SELECT a.t_id, b.n FROM x AS a JOIN x AS b ON a.t_id = b.t_id ORDER BY a.t_id"
            ),
        );
        let want: Vec<Row> = c.iter().map(|(k, n)| vec![int(*k), big(*n)]).collect();
        assert_eq!(rows, want);
        assert_eq!(m_twice.cte_materializations_total, 1, "{m_twice:?}");
        // The underlying table is read as many times as for ONE reference.
        let (_, m_once) = run_m(&db, &format!("WITH x AS ({body}) SELECT t_id FROM x"));
        assert_eq!(m_once.cte_materializations_total, 1);
        // Two references in two scalar subqueries: the base table is scanned
        // for the CTE once, not once per reference.
        let (rows, m2) = run_m(
            &db,
            &format!("WITH x AS ({body}) SELECT (SELECT COUNT(*) FROM x), (SELECT MAX(n) FROM x)"),
        );
        assert_eq!(rows, vec![vec![big(c.len() as i64), big(*c.values().max().unwrap())]]);
        assert_eq!(m2.cte_materializations_total, 1, "{m2:?}");
        let (_, m_one_ref) = run_m(&db, &format!("WITH x AS ({body}) SELECT (SELECT COUNT(*) FROM x)"));
        assert_eq!(
            m2.rows_scanned, m_one_ref.rows_scanned,
            "a second reference must not re-read the underlying table"
        );
        let _ = t;
        db.cleanup();
    }

    #[test]
    fn a_nondeterministic_looking_body_gives_both_references_identical_rows() {
        let (db, _t, _o) = standard_db("c5_same_rows");
        // LIMIT without ORDER BY: whichever 5 rows the body yields, a join of
        // the two references on the key matches all of them.
        let (rows, m) = run_m(
            &db,
            "WITH x AS (SELECT id FROM t LIMIT 5) SELECT COUNT(*) FROM x AS a JOIN x AS b ON a.id = b.id",
        );
        assert_eq!(rows, vec![vec![big(5)]]);
        assert_eq!(m.cte_materializations_total, 1);
        db.cleanup();
    }

    #[test]
    fn a_cte_read_from_a_correlated_subquery_is_materialized_once_not_per_outer_row() {
        let (db, t, orders) = standard_db("c5_correlated");
        let c = counts(&orders);
        let (rows, m) = run_m(
            &db,
            "WITH x AS (SELECT t_id, COUNT(*) AS n FROM orders WHERE t_id IS NOT NULL GROUP BY t_id) \
             SELECT id, (SELECT n FROM x WHERE x.t_id = t.id) FROM t ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .map(|r| vec![int(r.0), c.get(&r.0).map(|n| big(*n)).unwrap_or(NULL)])
            .collect();
        assert_eq!(rows, want);
        assert_eq!(m.cte_materializations_total, 1, "{m:?}");
        assert_eq!(m.correlated_subquery_reexecs, 60);
        db.cleanup();
    }

    #[test]
    fn a_cte_referenced_zero_times_is_neither_an_error_nor_materialized() {
        let (db, _t, _o) = standard_db("c3_dead");
        let (rows, m) = run_m(&db, "WITH x AS (SELECT * FROM orders) SELECT COUNT(*) FROM t");
        assert_eq!(rows, vec![vec![big(60)]]);
        assert_eq!(m.cte_materializations_total, 0, "{m:?}");
        assert_eq!(m.rows_scanned, 60, "orders was never read: {m:?}");
        db.cleanup();
    }

    #[test]
    fn a_cte_named_like_a_base_table_wins_over_it() {
        let (db, _t, _o) = standard_db("c3_hazard4");
        let rows = db.rows("WITH orders AS (SELECT 1 AS a) SELECT * FROM orders");
        assert_eq!(rows, vec![vec![int(1)]]);
        db.cleanup();
    }

    #[test]
    fn empty_and_all_null_ctes() {
        let (db, _t, _o) = standard_db("c3_empty_null");
        assert!(db.rows("WITH x AS (SELECT id FROM t WHERE id > 1000) SELECT * FROM x").is_empty());
        assert_eq!(
            db.rows("WITH x AS (SELECT id FROM t WHERE id > 1000) SELECT COUNT(*) FROM x"),
            vec![vec![big(0)]]
        );
        assert_eq!(
            db.rows("WITH x AS (SELECT name AS a, active AS b FROM t WHERE id = 45) SELECT a, b FROM x"),
            vec![vec![NULL, NULL]]
        );
        db.cleanup();
    }

    #[test]
    fn a_cte_body_may_use_subqueries_and_where_clauses() {
        let (db, t, orders) = standard_db("c3_mixed");
        let got = db.rows(
            "WITH x AS (SELECT id FROM t WHERE id IN (SELECT t_id FROM orders WHERE amount > 90) \
             AND EXISTS (SELECT 1 FROM orders WHERE orders.t_id = t.id)) SELECT id FROM x ORDER BY id",
        );
        let want: Vec<Row> = t
            .iter()
            .filter(|r| orders.iter().any(|o| o.3 == Some(r.0) && o.2.is_some_and(|a| a > 90)))
            .map(|r| vec![int(r.0)])
            .collect();
        assert!(!want.is_empty());
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn with_before_delete_update_and_insert() {
        let (db, _t, orders) = standard_db("c3_dml");
        let hot: std::collections::BTreeSet<i32> = orders
            .iter()
            .filter(|o| o.2.is_some_and(|a| a > 90))
            .filter_map(|o| o.3)
            .filter(|t| (1..=60).contains(t))
            .collect();
        assert!(!hot.is_empty());
        let r = db.write(
            "WITH big AS (SELECT t_id FROM orders WHERE amount > 90) \
             UPDATE t SET name = 'hot' WHERE id IN (SELECT t_id FROM big)",
        );
        assert_eq!(r.rows_affected, hot.len() as u64);
        let r = db.write(
            "WITH big AS (SELECT t_id FROM orders WHERE amount > 90) \
             DELETE FROM t WHERE id IN (SELECT t_id FROM big)",
        );
        assert_eq!(r.rows_affected, hot.len() as u64);
        assert_eq!(db.rows("SELECT COUNT(*) FROM t"), vec![vec![big(60 - hot.len() as i64)]]);
        // An INSERT cannot read a CTE (its VALUES hold no subqueries); the list is
        // bound, then dropped.
        let r = db.write("WITH x AS (SELECT 1 AS v) INSERT INTO t (id) VALUES (999)");
        assert_eq!(r.rows_affected, 1);
        let err = db
            .try_write("WITH x AS (SELECT id FROM no_such) INSERT INTO t (id) VALUES (998)")
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn ctes_inside_a_subquery_and_inside_a_derived_table() {
        let (db, _t, _o) = standard_db("c6_inside");
        assert_eq!(
            db.rows("SELECT (WITH x AS (SELECT 5 AS v) SELECT v FROM x)"),
            vec![vec![int(5)]]
        );
        assert_eq!(
            db.rows("SELECT d.w FROM (WITH x AS (SELECT 7 AS v) SELECT v AS w FROM x) AS d"),
            vec![vec![int(7)]]
        );
        // The inner CTE may use an outer one.
        assert_eq!(
            db.rows("WITH o AS (SELECT 3 AS v) SELECT (WITH i AS (SELECT v + 1 AS u FROM o) SELECT u FROM i)"),
            vec![vec![int(4)]]
        );
        // A CTE inside a correlated subquery is materialized once per
        // evaluation of that subquery -- a fresh materialization each time it
        // runs? No: its body cannot depend on the outer row, so the first
        // materialization serves every evaluation.
        let (rows, m) = run_m(
            &db,
            "SELECT id, (WITH x AS (SELECT COUNT(*) AS n FROM orders) SELECT n FROM x) FROM t WHERE id <= 3 ORDER BY id",
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(m.cte_materializations_total, 1, "{m:?}");
        db.cleanup();
    }

    #[test]
    fn explain_shows_a_referenced_twice_cte_once_with_the_materialized_flag() {
        let (db, _t, _o) = standard_db("c5_explain");
        let text = crate::plan::explain(&db.plan(
            "EXPLAIN WITH x AS (SELECT t_id, COUNT(*) AS n FROM orders GROUP BY t_id) \
             SELECT a.t_id FROM x AS a JOIN x AS b ON a.t_id = b.t_id",
        ));
        assert_eq!(text.matches("defined here").count(), 1, "{text}");
        assert_eq!(text.matches("CteNode").count(), 2, "{text}");
        assert!(text.contains("materialized once per statement"), "{text}");
        assert!(text.contains("reads the rows materialized above"), "{text}");
        // The body (the GROUP BY over orders) is printed exactly once.
        assert_eq!(text.matches("Aggregate").count(), 1, "{text}");
        db.cleanup();
    }

    #[test]
    fn cte_limits_are_enforced_before_any_work() {
        let (db, _t, _o) = standard_db("c3_limits");
        let run = |sql: &str, limits: ExecLimits| db.try_run_with(sql, &limits, &ExecMetrics::default(), &[]);
        let chain = "WITH a AS (SELECT 1 AS v), b AS (SELECT v FROM a), c AS (SELECT v FROM b) SELECT v FROM c";
        let lim = |n: u32| ExecLimits { max_cte_definitions: n, ..ExecLimits::default() };
        assert!(run(chain, lim(3)).is_ok());
        let err = run(chain, lim(2)).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_cte_definitions"), "{err}");
        // Dead definitions count too.
        let err = run("WITH a AS (SELECT 1 AS v), b AS (SELECT 2 AS v) SELECT 1", lim(1)).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        let thrice = "WITH x AS (SELECT 1 AS v) SELECT 1 FROM x AS a JOIN x AS b ON true JOIN x AS c ON true";
        let refs = |n: u32| ExecLimits { max_cte_references: n, ..ExecLimits::default() };
        assert!(run(thrice, refs(3)).is_ok());
        let err = run(thrice, refs(2)).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_cte_references"), "{err}");
        db.cleanup();
    }

    #[test]
    fn a_cte_larger_than_max_materialized_rows_fails_closed_without_truncating() {
        let (db, _t, _o) = standard_db("c3_bound");
        let sql = "WITH x AS (SELECT id FROM t) SELECT COUNT(*) FROM x";
        assert_eq!(db.rows(sql), vec![vec![big(60)]]);
        let metrics = ExecMetrics::default();
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_materialized_rows: 59, ..ExecLimits::default() },
                &metrics,
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert_eq!(metrics.snapshot().cte_materializations_total, 0, "a failed materialization is not counted");
        assert!(db
            .try_run_with(
                sql,
                &ExecLimits { max_materialized_rows: 60, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .is_ok());
        let err = db
            .try_run_with(
                sql,
                &ExecLimits { max_materialized_bytes: 100, ..ExecLimits::default() },
                &ExecMetrics::default(),
                &[],
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_cte_inside_lateral_and_a_cte_read_by_a_lateral_body() {
        let (db, t, orders) = standard_db("c6_lateral_cte");
        let c = counts(&orders);
        let rows = db.rows(
            "WITH x AS (SELECT t_id, COUNT(*) AS n FROM orders WHERE t_id IS NOT NULL GROUP BY t_id) \
             SELECT t.id, l.n FROM t JOIN LATERAL (SELECT n FROM x WHERE x.t_id = t.id) AS l ON true ORDER BY t.id",
        );
        let want: Vec<Row> = t
            .iter()
            .filter_map(|r| c.get(&r.0).map(|n| vec![int(r.0), big(*n)]))
            .collect();
        assert_eq!(rows, want);
        db.cleanup();
    }
}

// =======================================================================
// Randomized differential tests (derived tables and CTEs)
// =======================================================================

mod diff_relations {
    use super::*;
    use std::collections::BTreeMap;

    /// SplitMix64 -- a deterministic generator, no dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }

    /// Random `t` / `orders` with NULLs in every nullable column.
    fn random_data(seed: u64) -> (Vec<TRow>, Vec<ORow>) {
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
        let mut orders = Vec::new();
        for k in 0..(30 + rng.below(40) as i64) {
            let t_id = if rng.chance(15) { None } else { Some(1 + rng.below(t_count as u64 + 3) as i32) };
            let amount = if rng.chance(20) { None } else { Some(rng.below(30) as i32) };
            orders.push((5000 + k, names[rng.below(4) as usize].to_string(), amount, t_id));
        }
        (t, orders)
    }

    fn run_seed<F: FnMut(&Db, &[TRow], &[ORow], u64)>(tag: &str, seeds: u64, mut f: F) {
        for seed in 0..seeds {
            let db = Db::new(&format!("{tag}_{seed}"));
            let (t, orders) = random_data(seed.wrapping_mul(0x1234_5678_9ABC_DEF1).wrapping_add(11));
            seed_rows(&db, &t, &orders);
            f(&db, &t, &orders, seed);
            db.cleanup();
        }
    }

    fn seed_rows(db: &Db, t: &[TRow], orders: &[ORow]) {
        crate::correlated_tests::data::seed(db, t, orders);
    }

    /// `t_id -> (count, max amount)` over the orders whose amount exceeds `min`.
    fn per_t(orders: &[ORow], min: i32) -> BTreeMap<i32, (i64, Option<i32>)> {
        let mut m: BTreeMap<i32, (i64, Option<i32>)> = BTreeMap::new();
        for o in orders {
            if let (Some(t), Some(a)) = (o.3, o.2) {
                if a > min {
                    let e = m.entry(t).or_insert((0, None));
                    e.0 += 1;
                    e.1 = Some(e.1.map_or(a, |x| x.max(a)));
                }
            }
        }
        m
    }

    #[test]
    fn derived_tables_and_a_twice_read_cte_match_the_reference_on_random_data() {
        run_seed("diff_rel", 15, |db, t, orders, seed| {
            for min in [3, 12, 24] {
                let m = per_t(orders, min);
                let body = format!(
                    "SELECT t_id, COUNT(*) AS n, MAX(amount) AS mx FROM orders WHERE amount > {min} AND t_id IS NOT NULL GROUP BY t_id"
                );
                // derived table, LEFT JOIN (right side: materialized once)
                let sql = format!(
                    "SELECT t.id, g.n, g.mx FROM t LEFT JOIN ({body}) AS g ON g.t_id = t.id ORDER BY t.id"
                );
                let want: Vec<Row> = t
                    .iter()
                    .map(|r| match m.get(&r.0) {
                        Some((n, mx)) => vec![int(r.0), big(*n), opt_int(*mx)],
                        None => vec![int(r.0), NULL, NULL],
                    })
                    .collect();
                assert_eq!(db.rows(&sql), want, "seed {seed}: {sql}");
                // the same body as a CTE read twice
                let sql = format!(
                    "WITH g AS ({body}) SELECT t.id, a.n, b.mx FROM t JOIN g AS a ON a.t_id = t.id JOIN g AS b ON b.t_id = a.t_id ORDER BY t.id"
                );
                let want: Vec<Row> = t
                    .iter()
                    .filter_map(|r| m.get(&r.0).map(|(n, mx)| vec![int(r.0), big(*n), opt_int(*mx)]))
                    .collect();
                assert_eq!(db.rows(&sql), want, "seed {seed}: {sql}");
                // outer aggregate over a derived table
                let sql = format!("SELECT COUNT(*), SUM(g.n) FROM ({body}) AS g");
                let total: i64 = m.values().map(|(n, _)| *n).sum();
                let want = vec![vec![
                    big(m.len() as i64),
                    if m.is_empty() { NULL } else { big(total) },
                ]];
                assert_eq!(db.rows(&sql), want, "seed {seed}: {sql}");
            }
        });
    }

    #[test]
    fn lateral_top_one_matches_the_reference_on_random_data() {
        run_seed("diff_lateral", 12, |db, t, orders, seed| {
            let sql = "SELECT t.id, l.a FROM t LEFT JOIN LATERAL (SELECT orders.amount AS a FROM orders \
                       WHERE orders.t_id = t.id AND orders.amount IS NOT NULL ORDER BY orders.amount DESC, orders.id LIMIT 1) AS l ON true \
                       ORDER BY t.id";
            let want: Vec<Row> = t
                .iter()
                .map(|r| {
                    let top = orders
                        .iter()
                        .filter(|o| o.3 == Some(r.0))
                        .filter_map(|o| o.2)
                        .max();
                    vec![int(r.0), opt_int(top)]
                })
                .collect();
            assert_eq!(db.rows(sql), want, "seed {seed}");
        });
    }
}

// =======================================================================
// Materialization evidence and concurrency
// =======================================================================

mod materialization {
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};
    use std::sync::Arc;

    #[test]
    fn a_right_side_derived_table_reads_its_source_once_not_once_per_left_row() {
        let (db, _t, _o) = standard_db("c1_scan_once");
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with(
                "SELECT t.id FROM t JOIN (SELECT id AS k FROM t) AS d ON d.k = t.id",
                &ExecLimits::default(),
                &metrics,
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 60);
        // 60 outer rows + 60 inner rows once. Re-running the derived table for
        // each of the 60 left rows would read 60 + 60 * 60.
        assert!(metrics.snapshot().rows_scanned <= 130, "{:?}", metrics.snapshot());
        db.cleanup();
    }

    #[test]
    fn a_ctes_two_references_agree_while_another_session_is_writing_the_source_table() {
        let (db, _t, _o) = standard_db("c5_concurrent");
        let store = Arc::clone(&db.store);
        let orders_table = db.table_id("orders");
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut results = Vec::new();
        let written = std::thread::scope(|scope| {
            // A paced, bounded writer: it stops by itself, so a failing
            // assertion on the main thread can never leave it running.
            let writer = scope.spawn(|| {
                let mut k = 0i64;
                while k < 1_500 && !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    store
                        .put_row(orders_table, &[big(50_000 + k), text("w"), int(1), int(1)])
                        .unwrap();
                    k += 1;
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                k
            });
            for _ in 0..15 {
                results.push(db.try_run(
                    "WITH x AS (SELECT id FROM orders) \
                     SELECT (SELECT COUNT(*) FROM x) - (SELECT COUNT(*) FROM x), \
                            (SELECT MAX(id) FROM x) - (SELECT MAX(id) FROM x)",
                ));
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            writer.join().unwrap()
        });
        assert!(written > 0, "the writer really ran");
        for r in results {
            assert_eq!(r.unwrap().rows, vec![vec![big(0), big(0)]]);
        }
        db.cleanup();
    }

    #[test]
    fn a_derived_table_and_a_cte_work_in_an_update_set_subquery() {
        let (db, t, orders) = standard_db("c1_update_set");
        let r = db.write(
            "WITH top AS (SELECT t_id, MAX(amount) AS mx FROM orders WHERE t_id IS NOT NULL GROUP BY t_id) \
             UPDATE t SET name = (SELECT CASE WHEN d.m > 90 THEN 'hi' ELSE 'lo' END FROM \
             (SELECT t_id AS k, mx AS m FROM top) AS d WHERE d.k = t.id)",
        );
        assert_eq!(r.rows_affected, 60);
        // A t with orders becomes 'hi' (max amount > 90) or 'lo' (a NULL max
        // makes the CASE condition UNKNOWN, i.e. ELSE); a t with none gets the
        // empty scalar subquery's NULL.
        let want: Vec<Row> = t
            .iter()
            .map(|row| {
                let mine: Vec<&ORow> = orders.iter().filter(|o| o.3 == Some(row.0)).collect();
                let name = if mine.is_empty() {
                    NULL
                } else if mine.iter().filter_map(|o| o.2).max().is_some_and(|m| m > 90) {
                    text("hi")
                } else {
                    text("lo")
                };
                vec![int(row.0), name]
            })
            .collect();
        assert_eq!(db.rows("SELECT id, name FROM t ORDER BY id"), want);
        db.cleanup();
    }
}
