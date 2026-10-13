//! SQL-mission-1, Slice D -- `WITH RECURSIVE`.
//!
//! Layout: `parser` (D1), `binder` (D2: resolution, the at-most-once rule, the
//! restrictions on a recursive term), `counter` / `limits` (D3: `UNION ALL`
//! recursion, the bounds), `union` (D4: `UNION` recursion, the two stop
//! conditions), `shape` (D5: finding the separator), `contexts` (D6: several
//! recursive CTEs, derived tables, subqueries, DML), `observability` (D7:
//! `EXPLAIN`, metrics, authorization), `diff` (a fixpoint computed by an
//! independent loop). The HTTP tests are in `api/tests/api_sql_integration.rs`.

use crate::mission1_support::*;
use crate::setop_tests::{col, load, rel_db, run_tups, some, sorted, tups, Tup};

use crate::bound::{BoundSelect, BoundStatement};
use crate::error::SqlError;
use crate::exec::{ExecLimits, ExecMetrics, ExecMetricsSnapshot};

/// The `setop_tests` tables (`a`, `b`, `c`, `e`, `p`, `q`; the shared `t` and
/// `orders`) plus:
///
/// * `tree(id, parent, name)`: 1 root; 2, 3 under 1; 4, 5 under 2; 6 under 3;
///   7 under 6 (depths 0, 1, 1, 2, 2, 2, 3);
/// * `edges(id, src, dst)`: the diamond 1->2, 1->3, 2->4, 3->4, then 4->5;
/// * `cyc(id, nxt)`: 1 -> 2 -> 3 -> 1 and 4 -> 3 (a cycle);
/// * `roots(id)`: just 1;
/// * `base(id, v)`: v = 1..5.
fn rec_db(tag: &str) -> Db {
    let db = rel_db(tag);
    db.write("CREATE TABLE tree (id INTEGER PRIMARY KEY, parent INTEGER, name TEXT)");
    db.write(
        "INSERT INTO tree (id, parent, name) VALUES (1, NULL, 'root'), (2, 1, 'a'), (3, 1, 'b'), \
         (4, 2, 'c'), (5, 2, 'd'), (6, 3, 'e'), (7, 6, 'f')",
    );
    db.write("CREATE TABLE cyc (id INTEGER PRIMARY KEY, nxt INTEGER)");
    db.write("INSERT INTO cyc (id, nxt) VALUES (1, 2), (2, 3), (3, 1), (4, 3)");
    db.write("CREATE TABLE roots (id INTEGER PRIMARY KEY)");
    db.write("INSERT INTO roots (id) VALUES (1)");
    db.write("CREATE TABLE edges (id INTEGER PRIMARY KEY, src INTEGER, dst INTEGER)");
    db.write("INSERT INTO edges (id, src, dst) VALUES (1, 1, 2), (2, 1, 3), (3, 2, 4), (4, 3, 4), (5, 4, 5)");
    db.write("CREATE TABLE base (id INTEGER PRIMARY KEY, v INTEGER)");
    db.write("INSERT INTO base (id, v) VALUES (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)");
    db
}

const COUNTER: &str =
    "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t";

fn bound(db: &Db, sql: &str) -> Box<BoundSelect> {
    match db
        .try_bind(sql)
        .unwrap_or_else(|e| panic!("expected {sql:?} to bind, got {e}"))
    {
        BoundStatement::Select(s) => s,
        other => panic!("expected a SELECT, got {other:?}"),
    }
}

fn bind_error(db: &Db, sql: &str) -> SqlError {
    db.try_bind(sql)
        .err()
        .unwrap_or_else(|| panic!("expected {sql:?} to fail to bind"))
}

/// Runs `sql` with `limits` and returns the result and what the metrics saw.
fn run_with(
    db: &Db,
    sql: &str,
    limits: &ExecLimits,
) -> (crate::Result<crate::exec::QueryResult>, ExecMetricsSnapshot) {
    let metrics = ExecMetrics::default();
    let r = db.try_run_with(sql, limits, &metrics, &[]);
    (r, metrics.snapshot())
}

fn ints(db: &Db, sql: &str) -> Vec<i64> {
    run_tups(db, sql)
        .into_iter()
        .map(|r| r[0].expect("a non-NULL integer"))
        .collect()
}

// =======================================================================
// D1 -- parser
// =======================================================================

mod parser {
    use crate::ast::*;
    use crate::error::SqlError;
    use crate::limits::SqlLimits;
    use crate::parse::parse_statement;
    use super::{run_tups, sorted, Db};

    fn select(sql: &str) -> Box<Select> {
        match parse_statement(sql, &SqlLimits::default())
            .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
        {
            Statement::Select(s) => s,
            other => panic!("expected a SELECT, got {other:?}"),
        }
    }

    #[test]
    fn with_recursive_marks_every_definition_of_its_list() {
        let s = select("WITH RECURSIVE a AS (SELECT 1), b(x) AS (SELECT 2) SELECT * FROM a");
        assert_eq!(s.ctes.len(), 2);
        assert!(s.ctes.iter().all(|c| c.recursive));
        let s = select("WITH a AS (SELECT 1), b(x) AS (SELECT 2) SELECT * FROM a");
        assert!(s.ctes.iter().all(|c| !c.recursive));
    }

    #[test]
    fn a_plain_with_parses_exactly_as_before() {
        // The AST of a plain WITH is the AST of the same text with RECURSIVE
        // once the flag is cleared: nothing else about the list changed.
        for body in [
            "x AS (SELECT 1) SELECT * FROM x",
            "x(a, b) AS (SELECT 1, 2), y AS (SELECT a FROM x) SELECT * FROM y",
            "x AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM x",
        ] {
            let plain = select(&format!("WITH {body}"));
            let mut recursive = select(&format!("WITH RECURSIVE {body}"));
            assert!(recursive.ctes.iter().all(|c| c.recursive));
            for c in &mut recursive.ctes {
                c.recursive = false;
            }
            assert_eq!(plain, recursive, "{body}");
            assert!(plain.ctes.iter().all(|c| !c.recursive));
        }
    }

    #[test]
    fn every_recursive_form_in_scope_parses() {
        let limits = SqlLimits::default();
        for sql in [
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t",
            "WITH RECURSIVE a(n) AS (SELECT 1 UNION ALL SELECT n FROM a WHERE FALSE), b(n) AS (SELECT 2) SELECT * FROM a",
            "WITH RECURSIVE r AS (SELECT id FROM roots UNION ALL SELECT b.id FROM r JOIN tree b ON b.parent = r.id), v AS (SELECT id FROM r) SELECT * FROM v",
            "SELECT * FROM (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r) AS d",
            "SELECT (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT COUNT(*) FROM r)",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT 100)) SELECT n FROM r",
            "WITH RECURSIVE r(n) AS ((SELECT 1 UNION ALL SELECT 2) UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < (SELECT MAX(v) FROM base)) SELECT n FROM r",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) DELETE FROM a WHERE v IN (SELECT n FROM r)",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) UPDATE a SET v = 0 WHERE v IN (SELECT n FROM r)",
        ] {
            parse_statement(sql, &limits).unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"));
        }
    }

    #[test]
    fn a_window_function_is_refused_before_binding() {
        // Mission 2: OVER / ROLLUP / COUNT(DISTINCT) is implemented; see MISSION2_FEATURE_MATRIX.md §6.2.
        // `OVER` now parses; the refusal moved from the parser to the binder
        // ("not allowed in the recursive term"), and a window in the *seed* is
        // allowed. The guarantee is unchanged: the clause is never accepted and
        // then silently ignored.
        let db = Db::new("rec_window_refused");
        let term = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT ROW_NUMBER() OVER () FROM t) SELECT n FROM t";
        assert!(parse_statement(term, &SqlLimits::default()).is_ok(), "{term}");
        let err = db.try_bind(term).unwrap_err();
        assert!(
            matches!(&err, SqlError::InvalidIdentifier { detail } if detail.contains("window functions")),
            "{term}: {err:?}"
        );
        let seed = "WITH RECURSIVE t(n) AS (SELECT SUM(1) OVER () UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t";
        assert!(parse_statement(seed, &SqlLimits::default()).is_ok(), "{seed}");
        db.try_bind(seed).unwrap_or_else(|e| panic!("a window in the seed binds: {e:?}"));
        db.cleanup();
        // Twin: the same recursion without the window function parses, so the
        // refusals above are about the OVER clause and not about WITH RECURSIVE.
        parse_statement(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT SUM(n) FROM t) SELECT n FROM t",
            &SqlLimits::default(),
        )
        .expect("the recursion without OVER parses");
    }

    #[test]
    fn an_over_clause_is_refused_everywhere_not_silently_dropped() {
        // Outside any recursion, so this does not lean on `WITH RECURSIVE`
        // having been refused: the converter used to drop the `OVER` clause and
        // run `SUM(id) OVER ()` as the plain aggregate `SUM(id)`.
        // Mission 2: OVER / ROLLUP / COUNT(DISTINCT) is implemented; see MISSION2_FEATURE_MATRIX.md §6.2.
        // Same failure mode, restated: the clause must reach the AST as a window
        // call (converter) AND change what runs (planner/executor). A dropped or
        // ignored `OVER` collapses the three rows below to one aggregate row.
        let db = Db::new("rec_over_not_dropped");
        db.seed_t(&[(1, Some("a"), None), (2, Some("a"), None), (3, Some("b"), None)]);
        for (sql, expected) in [
            ("SELECT SUM(id) OVER () FROM t", vec![vec![Some(6)], vec![Some(6)], vec![Some(6)]]),
            (
                "SELECT COUNT(*) OVER (PARTITION BY name) FROM t",
                vec![vec![Some(1)], vec![Some(2)], vec![Some(2)]],
            ),
            (
                "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM t",
                vec![vec![Some(1)], vec![Some(2)], vec![Some(3)]],
            ),
        ] {
            let stmt = parse_statement(sql, &SqlLimits::default()).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            let Statement::Select(s) = stmt else { panic!("{sql}: not a SELECT") };
            let SelectItem::Item(item) = &s.projection[0] else { panic!("{sql}: not an item") };
            assert!(matches!(item.expr, Expr::Window(_)), "{sql}: OVER was dropped: {:?}", item.expr);
            assert_eq!(sorted(run_tups(&db, sql)), expected, "{sql}");
        }
        db.cleanup();
        // The plain aggregates next to them parse.
        parse_statement("SELECT SUM(id) FROM t", &SqlLimits::default()).unwrap();
    }

    #[test]
    fn what_is_still_outside_the_grammar_is_a_typed_error() {
        for sql in [
            "WITH RECURSIVE t(n) AS MATERIALIZED (SELECT 1) SELECT n FROM t",
            "WITH RECURSIVE t(n INTEGER) AS (SELECT 1) SELECT n FROM t",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(err, SqlError::Unsupported { .. } | SqlError::Parse { .. }),
                "{sql}: {err:?}"
            );
        }
        // Twin: without those spellings the recursion parses.
        parse_statement(
            "WITH RECURSIVE t(n) AS (SELECT 1) SELECT n FROM t",
            &SqlLimits::default(),
        )
        .expect("the plain recursive list parses");
    }
}

// =======================================================================
// D2 -- binder
// =======================================================================

mod binder {
    use super::*;
    use crate::auth::AuthContext;
    use crate::limits::SqlLimits;
    use rubixdb::catalog::schema::{ObjectKind, Privilege};
    use rubixdb::relational::RelationalType;

    fn message(db: &Db, sql: &str) -> String {
        bind_error(db, sql).to_string()
    }

    #[test]
    fn a_self_referencing_union_body_is_a_recursive_cte_with_a_seed_and_a_term() {
        let db = rec_db("d2_shape");
        let s = bound(&db, COUNTER);
        assert_eq!(s.ctes.len(), 1);
        let cte = &s.ctes[0];
        let rec = cte.recursion.as_ref().expect("the CTE recurses");
        assert!(rec.all, "UNION ALL");
        let set = cte.query.set_op.as_ref().expect("seed UNION term");
        assert_eq!(set.op, crate::ast::SetOperator::Union);
        // The seed reads nothing; the term reads exactly the working table.
        assert!(set.left.from.is_empty());
        assert_eq!(set.right.from.len(), 1);
        assert!(matches!(
            &set.right.from[0].relation,
            crate::bound::BoundRelation::Working(w) if w.work_id == rec.work_id && w.name == "t"
        ));
        // The outside reads the finished CTE.
        assert!(matches!(s.from[0].relation, crate::bound::BoundRelation::Cte(_)));
        // UNION (distinct) sets `all` to false.
        let s = bound(&db, &COUNTER.replace("UNION ALL", "UNION"));
        assert!(!s.ctes[0].recursion.as_ref().unwrap().all);
    }

    #[test]
    fn column_names_come_from_the_list_or_the_seed_and_types_from_the_seed() {
        let db = rec_db("d2_columns");
        let s = bound(&db, COUNTER);
        assert_eq!(s.projection[0].output_name, "n");
        assert_eq!(s.projection[0].expr.ty, Some(RelationalType::Integer));
        // Without a column list the seed's names are the CTE's.
        let s = bound(
            &db,
            "WITH RECURSIVE t AS (SELECT 1 AS k UNION ALL SELECT k + 1 FROM t WHERE k < 3) SELECT k FROM t",
        );
        assert_eq!(s.projection[0].output_name, "k");
        // A wrong count of names is an error.
        let m = message(
            &db,
            "WITH RECURSIVE t(x, y) AS (SELECT 1 UNION ALL SELECT x + 1 FROM t WHERE x < 3) SELECT x FROM t",
        );
        assert!(m.contains("column name"), "{m}");
        // The aggregate seed of the spec's example is BIGINT and so is the term.
        let s = bound(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT COUNT(*) FROM base UNION ALL SELECT n - 1 FROM r WHERE n > 0) SELECT * FROM r",
        );
        assert_eq!(s.projection[0].expr.ty, Some(RelationalType::Bigint));
    }

    #[test]
    fn a_list_member_that_does_not_name_itself_is_an_ordinary_cte() {
        let db = rec_db("d2_plain_member");
        let s = bound(
            &db,
            "WITH RECURSIVE x(v) AS (SELECT 1 UNION ALL SELECT 2), y AS (SELECT v FROM x) SELECT * FROM y",
        );
        assert!(s.ctes.iter().all(|c| c.recursion.is_none()));
        // ... including one whose operands unify (INTEGER with BIGINT).
        let s = bound(
            &db,
            "WITH RECURSIVE x(v) AS (SELECT 1 UNION ALL SELECT 10000000000) SELECT v FROM x",
        );
        assert_eq!(s.projection[0].expr.ty, Some(RelationalType::Bigint));
        // Without the keyword the same self reference is still the Slice C error.
        let m = message(&db, "WITH x AS (SELECT * FROM x) SELECT * FROM x");
        assert!(m.contains("CTE 'x' refers to itself; use WITH RECURSIVE"), "{m}");
    }

    #[test]
    fn inside_the_term_the_cte_name_is_the_working_table_not_the_catalog_table() {
        let db = rec_db("d2_working_wins");
        // `t` is a real table (id, name, active); inside the term `t` is the
        // working table with the single column `n`.
        let s = bound(
            &db,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
        );
        assert!(s.ctes[0].recursion.is_some());
        // The catalog table's columns are not visible there.
        let e = bind_error(
            &db,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE name = 'x') SELECT n FROM t",
        );
        assert!(matches!(e, SqlError::UnknownObject { kind: "column", .. }), "{e:?}");
        // A qualified name is never a CTE: schema.t is the catalog table.
        let m = message(
            &db,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM public.t) SELECT n FROM t",
        );
        assert!(!m.contains("exactly once"), "{m}");
    }

    #[test]
    fn the_cte_name_in_its_seed_is_an_error() {
        let db = rec_db("d2_seed_ref");
        let m = message(
            &db,
            "WITH RECURSIVE t(n) AS (SELECT n FROM t UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
        );
        assert!(m.contains("must not appear in the seed"), "{m}");
        // ... also when a catalog table of the same name exists: structural.
        let m = message(
            &db,
            "WITH RECURSIVE orders(id) AS (SELECT id FROM orders UNION ALL SELECT id + 1 FROM orders WHERE id < 3) SELECT id FROM orders",
        );
        assert!(m.contains("must not appear in the seed"), "{m}");
        // In the middle operand of three: the seed is `A UNION ALL B`.
        let m = message(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT 100) SELECT n FROM r",
        );
        assert!(m.contains("must not appear in the seed"), "{m}");
    }

    #[test]
    fn the_reference_must_appear_exactly_once_in_the_term() {
        let db = rec_db("d2_once");
        for sql in [
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT a.n + b.n FROM r a JOIN r b ON a.n = b.n WHERE a.n < 3) SELECT n FROM r",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3 AND n IN (SELECT n FROM r)) SELECT n FROM r",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT n FROM r)) SELECT n FROM r",
        ] {
            let m = message(&db, sql);
            assert!(
                m.contains("must appear exactly once in the recursive term")
                    || m.contains("not inside a subquery"),
                "{sql}: {m}"
            );
        }
        // The two plain FROM-clause occurrences give exactly the spec's message.
        let m = message(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT x.n + y.n FROM r x JOIN r y ON x.n = y.n WHERE x.n < 3) SELECT n FROM r",
        );
        assert!(m.contains("recursive reference 'r' must appear exactly once in the recursive term"), "{m}");
    }

    #[test]
    fn the_reference_must_be_a_from_item_of_the_term_itself() {
        let db = rec_db("d2_position");
        for (sql, needle) in [
            (
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM base WHERE v IN (SELECT n FROM r)) SELECT n FROM r",
                "not inside a subquery, a derived table or a nested WITH",
            ),
            (
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT d.n + 1 FROM (SELECT n FROM r) AS d) SELECT n FROM r",
                "not inside a subquery, a derived table or a nested WITH",
            ),
            (
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT (SELECT COUNT(*) FROM r) FROM base) SELECT n FROM r",
                "not inside a subquery, a derived table or a nested WITH",
            ),
            (
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT b.v FROM base b LEFT JOIN r ON r.n = b.v) SELECT n FROM r",
                "nullable side of an outer join",
            ),
        ] {
            let m = message(&db, sql);
            assert!(m.contains(needle), "{sql}: {m}");
        }
        // The preserved side of a LEFT JOIN is fine.
        bound(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT r.n + 1 FROM r LEFT JOIN base b ON b.v = r.n WHERE r.n < 3) SELECT n FROM r",
        );
    }

    #[test]
    fn the_term_may_not_aggregate_group_having_order_or_limit() {
        let db = rec_db("d2_restrictions");
        let wrap = |term: &str| {
            format!("WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL {term}) SELECT n FROM r")
        };
        for (term, needle) in [
            ("SELECT COUNT(*) FROM r", "aggregate functions, GROUP BY and HAVING are not allowed in the recursive term"),
            ("SELECT MAX(n) + 1 FROM r", "aggregate functions, GROUP BY and HAVING are not allowed in the recursive term"),
            ("SELECT n + 1 FROM r GROUP BY n", "aggregate functions, GROUP BY and HAVING are not allowed in the recursive term"),
            ("SELECT n + 1 FROM r GROUP BY n HAVING n < 3", "aggregate functions, GROUP BY and HAVING are not allowed in the recursive term"),
            ("SELECT DISTINCT n + 1 FROM r WHERE n < 3", "SELECT DISTINCT is not allowed in the recursive term"),
            ("(SELECT n + 1 FROM r WHERE n < 3 ORDER BY n)", "ORDER BY is not allowed in the recursive term"),
            ("(SELECT n + 1 FROM r WHERE n < 3 LIMIT 1)", "LIMIT and OFFSET are not allowed in the recursive term"),
            ("(SELECT n + 1 FROM r WHERE n < 3 LIMIT 5 OFFSET 1)", "LIMIT and OFFSET are not allowed in the recursive term"),
            // A set operation containing the reference, with an ORDER BY / LIMIT of its own.
            ("(SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT 5 ORDER BY 1)", "not allowed on the recursive term"),
            ("(SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT 5 LIMIT 2)", "not allowed on the recursive term"),
        ] {
            let m = message(&db, &wrap(term));
            assert!(m.contains(needle), "{term}: {m}");
        }
        // After the term, ORDER BY / LIMIT / OFFSET of the whole body.
        for tail in ["ORDER BY n", "LIMIT 3", "LIMIT 3 OFFSET 1", "OFFSET 1"] {
            let m = message(
                &db,
                &format!("WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3 {tail}) SELECT n FROM r"),
            );
            assert!(m.contains("after the recursive term of CTE 'r' are not allowed"), "{tail}: {m}");
        }
        // The seed may aggregate, group, order, limit and be DISTINCT.
        bound(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT COUNT(*) FROM base UNION ALL SELECT n - 1 FROM r WHERE n > 0) SELECT n FROM r",
        );
        bound(
            &db,
            "WITH RECURSIVE r(n) AS ((SELECT DISTINCT v FROM base ORDER BY v LIMIT 2) UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
        );
    }

    #[test]
    fn distinct_is_allowed_in_the_term_only_under_union() {
        let db = rec_db("d2_distinct");
        let sql = |op: &str| {
            format!("WITH RECURSIVE r(n) AS (SELECT 1 {op} SELECT DISTINCT n + 1 FROM r WHERE n < 3) SELECT n FROM r")
        };
        bound(&db, &sql("UNION"));
        let m = message(&db, &sql("UNION ALL"));
        assert!(m.contains("SELECT DISTINCT is not allowed in the recursive term"), "{m}");
    }

    #[test]
    fn a_body_that_cannot_be_split_into_a_seed_and_a_term_is_refused() {
        let db = rec_db("d2_not_shaped");
        for (sql, needle) in [
            ("WITH RECURSIVE r(n) AS (SELECT n + 1 FROM r) SELECT n FROM r", "its body is a single SELECT"),
            ("WITH RECURSIVE r(n) AS (SELECT 1 INTERSECT SELECT n FROM r) SELECT n FROM r", "outermost set operator is INTERSECT"),
            ("WITH RECURSIVE r(n) AS (SELECT 1 EXCEPT ALL SELECT n FROM r) SELECT n FROM r", "outermost set operator is EXCEPT"),
            ("WITH RECURSIVE r(n) AS ((SELECT 1 UNION ALL SELECT n + 1 FROM r) INTERSECT SELECT 3) SELECT n FROM r", "outermost set operator is INTERSECT"),
        ] {
            let m = message(&db, sql);
            assert!(m.contains(needle), "{sql}: {m}");
        }
        let e = bind_error(
            &db,
            "WITH RECURSIVE r(n) AS (WITH w AS (SELECT 1) SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
        );
        assert!(matches!(e, SqlError::Unsupported { .. }), "{e:?}");
    }

    #[test]
    fn the_terms_column_types_must_equal_the_seeds() {
        let db = rec_db("d2_types");
        let m = message(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT 10000000000 FROM r) SELECT n FROM r",
        );
        assert!(m.contains("column 1 has type INTEGER in the seed but BIGINT in the recursive term"), "{m}");
        let m = message(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT 'x' FROM r) SELECT n FROM r",
        );
        assert!(m.contains("column 1 has type INTEGER in the seed but TEXT"), "{m}");
        let e = bind_error(
            &db,
            "WITH RECURSIVE r(n, m) AS (SELECT 1, 2 UNION ALL SELECT n FROM r) SELECT n FROM r",
        );
        assert!(matches!(e, SqlError::TypeMismatch { .. }), "{e:?}");
        // An untyped NULL in the term adopts the seed's type.
        bound(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT NULL FROM r) SELECT n FROM r",
        );
        // The columns are nullable: the term may produce a NULL where the seed never does.
        let s = bound(&db, COUNTER);
        assert!(s.projection[0].expr.nullable);
    }

    #[test]
    fn what_other_ctes_may_see_inside_a_recursive_definition() {
        let db = rec_db("d2_visibility");
        // A CTE defined BEFORE the recursive one is visible in its seed ...
        bound(
            &db,
            "WITH RECURSIVE s(x) AS (SELECT 5), r(n) AS (SELECT x FROM s UNION ALL SELECT n + 1 FROM r WHERE n < 8) SELECT n FROM r",
        );
        // ... but not in its recursive term.
        let m = message(
            &db,
            "WITH RECURSIVE s(x) AS (SELECT 5), r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r JOIN s ON r.n < s.x) SELECT n FROM r",
        );
        assert!(
            m.contains("CTE 's' is defined earlier in the same WITH RECURSIVE list and is not visible inside a recursive term"),
            "{m}"
        );
        // A CTE defined AFTER it is not visible in its body at all ...
        let m = message(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r JOIN later ON r.n < later.x), later(x) AS (SELECT 3) SELECT n FROM r",
        );
        assert!(m.contains("'later' is referenced before it is defined"), "{m}");
        // ... and is visible to the main query.
        bound(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3), later(x) AS (SELECT n FROM r) SELECT x FROM later",
        );
        // A CTE of an ENCLOSING query stays visible in the term.
        bound(
            &db,
            "WITH s(x) AS (SELECT 3) SELECT (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r JOIN s ON r.n < s.x) SELECT COUNT(*) FROM r)",
        );
        // Catalog tables are visible in the term (they are not CTEs).
        bound(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r JOIN base ON base.v = r.n) SELECT n FROM r",
        );
    }

    #[test]
    fn mutually_recursive_ctes_are_a_bind_error() {
        let db = rec_db("d2_mutual");
        let m = message(
            &db,
            "WITH RECURSIVE a(n) AS (SELECT 1 UNION ALL SELECT n FROM b), b(n) AS (SELECT 1 UNION ALL SELECT n FROM a) SELECT n FROM a",
        );
        assert!(m.contains("'b' is referenced before it is defined"), "{m}");
        assert!(m.contains("refer to each other are not supported"), "{m}");
        // Self reference plus a reference to a later one.
        let m = message(
            &db,
            "WITH RECURSIVE a(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM a JOIN b ON true), b(n) AS (SELECT 1) SELECT n FROM a",
        );
        assert!(m.contains("referenced before it is defined"), "{m}");
    }

    #[test]
    fn several_recursive_ctes_and_a_dead_one_bind() {
        let db = rec_db("d2_several");
        let s = bound(
            &db,
            "WITH RECURSIVE a(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM a WHERE n < 3), \
             b(m) AS (SELECT 10 UNION ALL SELECT m + 1 FROM b WHERE m < 12), \
             dead(k) AS (SELECT 1 UNION ALL SELECT k + 1 FROM dead WHERE k < 2) SELECT n, m FROM a JOIN b ON true",
        );
        assert_eq!(s.ctes.iter().filter(|c| c.recursion.is_some()).count(), 3);
        // Distinct working-table ids.
        let ids: std::collections::BTreeSet<u32> =
            s.ctes.iter().filter_map(|c| c.recursion.as_ref().map(|r| r.work_id)).collect();
        assert_eq!(ids.len(), 3);
        // A dead recursive CTE with a bad body is still an error.
        let m = message(
            &db,
            "WITH RECURSIVE dead(k) AS (SELECT 1 UNION ALL SELECT COUNT(*) FROM dead) SELECT 1",
        );
        assert!(m.contains("aggregate functions"), "{m}");
    }

    #[test]
    fn a_forbidden_table_in_the_seed_or_the_term_is_unknown_and_the_reference_needs_no_grant() {
        let db = rec_db("d2_authz");
        let grant = |table: &str| {
            let id = db.table_id(table);
            db.catalog
                .grant("limited", ObjectKind::Table, id, Privilege::Select)
                .unwrap();
        };
        grant("tree");
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        // Only `tree` is granted: a recursion over it binds, and its own name
        // (the working table) is not a catalog lookup.
        let walk = "WITH RECURSIVE w(id) AS (SELECT id FROM tree WHERE parent IS NULL UNION ALL SELECT tree.id FROM w JOIN tree ON tree.parent = w.id) SELECT id FROM w";
        db.try_bind_with(walk, &auth, &limits).expect("binds with only `tree` granted");
        // A forbidden table in the seed.
        let seed = "WITH RECURSIVE w(id) AS (SELECT id FROM base UNION ALL SELECT tree.id FROM w JOIN tree ON tree.parent = w.id) SELECT id FROM w";
        // A forbidden table in the term.
        let term = "WITH RECURSIVE w(id) AS (SELECT id FROM tree UNION ALL SELECT base.id FROM w JOIN base ON base.v = w.id) SELECT id FROM w";
        let missing = "WITH RECURSIVE w(id) AS (SELECT id FROM tree UNION ALL SELECT no_such.id FROM w JOIN no_such ON no_such.v = w.id) SELECT id FROM w";
        for sql in [seed, term, missing] {
            let e = db.try_bind_with(sql, &auth, &limits).unwrap_err();
            assert!(matches!(e, SqlError::UnknownObject { kind: "table", .. }), "{sql}: {e:?}");
        }
        // The forbidden and the missing table give the same shape.
        let f = db.try_bind_with(term, &auth, &limits).unwrap_err().to_string();
        let m = db.try_bind_with(missing, &auth, &limits).unwrap_err().to_string();
        assert_eq!(f.replace("base", "X"), m.replace("no_such", "X"));
        // Granted, the term binds.
        grant("base");
        db.try_bind_with(term, &auth, &limits).expect("binds once granted");
        db.cleanup();
    }

    #[test]
    fn a_recursive_cte_reading_an_outer_column_keeps_the_slice_c_boundary_error() {
        let db = rec_db("d2_boundary");
        let m = message(
            &db,
            "SELECT t.id, (WITH RECURSIVE r(n) AS (SELECT t.id UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT COUNT(*) FROM r) FROM t",
        );
        assert!(m.contains("CTE 'r' cannot reference outer column 't.id'"), "{m}");
        let m = message(
            &db,
            "SELECT t.id, (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + t.id FROM r WHERE n < 3) SELECT COUNT(*) FROM r) FROM t",
        );
        assert!(m.contains("cannot reference outer column"), "{m}");
    }
}

// =======================================================================
// D3 -- UNION ALL recursion: counter, accumulator, walks
// =======================================================================

mod counter {
    use super::*;
    use rubixdb::relational::RelationalType;

    #[test]
    fn the_counter_yields_one_to_ten() {
        let db = rec_db("d3_counter");
        let (r, m) = run_with(&db, COUNTER, &ExecLimits::default());
        let r = r.unwrap();
        assert_eq!(tups(&r.rows), col(&some(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])));
        assert_eq!(r.schema.fields[0].name, "n");
        assert_eq!(r.schema.fields[0].ty, Some(RelationalType::Integer));
        // Ten evaluations of the term (the tenth produces nothing) and ten rows.
        assert_eq!(m.recursive_cte_iterations_total, 10);
        assert_eq!(m.recursive_cte_rows_emitted_total, 10);
        db.cleanup();
    }

    #[test]
    fn the_factorial_accumulator_ends_with_ten_factorial() {
        let db = rec_db("d3_factorial");
        let sql = "WITH RECURSIVE f(n, acc) AS (SELECT 1, 1 UNION ALL SELECT n + 1, acc * (n + 1) FROM f WHERE n < 10) \
                   SELECT acc FROM f";
        let rows = run_tups(&db, sql);
        assert_eq!(rows.len(), 10);
        let want = [1i64, 2, 6, 24, 120, 720, 5040, 40320, 362880, 3628800];
        assert_eq!(sorted(rows.clone()), col(&some(&want)));
        assert_eq!(rows.last().unwrap()[0], Some(3_628_800), "the last row is 10!");
        assert_eq!(
            ints(&db, "WITH RECURSIVE f(n, acc) AS (SELECT 1, 1 UNION ALL SELECT n + 1, acc * (n + 1) FROM f WHERE n < 10) SELECT MAX(acc) FROM f"),
            vec![3_628_800]
        );
        // Past INTEGER's range the arithmetic overflows: a typed error, no panic.
        let err = db
            .try_run("WITH RECURSIVE f(n, acc) AS (SELECT 1, 1 UNION ALL SELECT n + 1, acc * (n + 1) FROM f WHERE n < 13) SELECT acc FROM f")
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        db.cleanup();
    }

    const WALK: &str = "WITH RECURSIVE walk(id, depth) AS (\
         SELECT id, 0 FROM tree WHERE parent IS NULL \
         UNION ALL \
         SELECT tree.id, walk.depth + 1 FROM walk JOIN tree ON tree.parent = walk.id) ";

    #[test]
    fn a_tree_walk_visits_every_node_once_with_its_depth() {
        let db = rec_db("d3_tree");
        let rows = run_tups(&db, &format!("{WALK} SELECT id, depth FROM walk ORDER BY id"));
        let want: Vec<Tup> = [(1, 0), (2, 1), (3, 1), (4, 2), (5, 2), (6, 2), (7, 3)]
            .iter()
            .map(|(i, d)| vec![Some(*i), Some(*d)])
            .collect();
        assert_eq!(rows, want);
        // Row count and maximum depth.
        assert_eq!(ints(&db, &format!("{WALK} SELECT COUNT(*) FROM walk")), vec![7]);
        assert_eq!(ints(&db, &format!("{WALK} SELECT MAX(depth) FROM walk")), vec![3]);
        db.cleanup();
    }

    #[test]
    fn each_iteration_sees_the_previous_iterations_rows_and_only_those() {
        let db = rec_db("d3_levels");
        // The size of each level of the tree is what an iteration that saw the
        // right input produces.
        let rows = run_tups(
            &db,
            &format!("{WALK} SELECT depth, COUNT(*) FROM walk GROUP BY depth ORDER BY depth"),
        );
        assert_eq!(
            rows,
            vec![
                vec![Some(0), Some(1)],
                vec![Some(1), Some(2)],
                vec![Some(2), Some(3)],
                vec![Some(3), Some(1)],
            ]
        );
        // No node appears twice (an iteration that re-read its own output, or
        // an accumulating working table, would repeat them).
        assert_eq!(
            sorted(run_tups(&db, &format!("{WALK} SELECT id FROM walk"))),
            col(&some(&[1, 2, 3, 4, 5, 6, 7]))
        );
        db.cleanup();
    }

    #[test]
    fn the_ancestor_chain_comes_out_in_the_order_of_the_walk_up() {
        let db = rec_db("d3_ancestors");
        // Node 5's ancestors: 2, 1 (a join on the working table's parent column).
        let sql = "WITH RECURSIVE anc(id, parent, depth) AS (\
                   SELECT id, parent, 0 FROM tree WHERE id = 5 \
                   UNION ALL \
                   SELECT tree.id, tree.parent, anc.depth + 1 FROM anc JOIN tree ON tree.id = anc.parent) \
                   SELECT id, depth FROM anc ORDER BY depth";
        assert_eq!(
            run_tups(&db, sql),
            vec![vec![Some(5), Some(0)], vec![Some(2), Some(1)], vec![Some(1), Some(2)]]
        );
        db.cleanup();
    }

    #[test]
    fn the_recursive_reference_joined_to_a_base_table() {
        let db = rec_db("d3_join");
        let sql = "WITH RECURSIVE r AS (SELECT id FROM roots UNION ALL SELECT b.id FROM r JOIN tree b ON b.parent = r.id) \
                   SELECT * FROM r ORDER BY id";
        assert_eq!(run_tups(&db, sql), col(&some(&[1, 2, 3, 4, 5, 6, 7])));
        // With the base table first and a filter on the working table.
        let sql = "WITH RECURSIVE r(id) AS (SELECT id FROM roots UNION ALL SELECT b.id FROM tree b JOIN r ON b.parent = r.id WHERE r.id < 3) \
                   SELECT id FROM r ORDER BY id";
        assert_eq!(run_tups(&db, sql), col(&some(&[1, 2, 3, 4, 5])));
        db.cleanup();
    }

    #[test]
    fn a_seed_with_an_aggregate_runs_once() {
        let db = rec_db("d3_seed_agg");
        let sql = "WITH RECURSIVE r(n) AS (SELECT COUNT(*) FROM base UNION ALL SELECT n - 1 FROM r WHERE n > 0) SELECT n FROM r";
        let (r, m) = run_with(&db, sql, &ExecLimits::default());
        // COUNT(*) = 5, then 4, 3, 2, 1, 0.
        assert_eq!(tups(&r.unwrap().rows), col(&some(&[5, 4, 3, 2, 1, 0])));
        assert_eq!(m.seq_scans, 1, "the seed's scan of `base` happened once, not once per iteration");
        assert_eq!(m.recursive_cte_iterations_total, 6);
        db.cleanup();
    }

    #[test]
    fn the_seed_is_evaluated_once_whatever_the_number_of_iterations() {
        let db = rec_db("d3_seed_once");
        let sql = "WITH RECURSIVE r(id) AS (SELECT id FROM tree WHERE id = 1 UNION ALL SELECT id + 1 FROM r WHERE id < 50) SELECT COUNT(*) FROM r";
        let (res, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(tups(&res.unwrap().rows), vec![vec![Some(50)]]);
        assert_eq!(m.recursive_cte_iterations_total, 50);
        // One primary-key lookup for the seed, nothing else touched `tree`.
        assert_eq!(m.pk_lookups + m.seq_scans, 1, "{m:?}");
        db.cleanup();
    }

    #[test]
    fn an_empty_seed_terminates_at_once_with_no_rows() {
        let db = rec_db("d3_empty_seed");
        let sql = "WITH RECURSIVE t(n) AS (SELECT 1 WHERE FALSE UNION ALL SELECT n + 1 FROM t) SELECT * FROM t";
        let (r, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(r.unwrap().rows.len(), 0);
        assert_eq!(m.recursive_cte_iterations_total, 0);
        assert_eq!(m.recursive_cte_rows_emitted_total, 0);
        db.cleanup();
    }

    #[test]
    fn an_empty_recursive_term_stops_after_one_iteration() {
        let db = rec_db("d3_empty_term");
        let sql = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM t WHERE FALSE) SELECT * FROM t";
        let (r, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(tups(&r.unwrap().rows), col(&some(&[1])));
        assert_eq!(m.recursive_cte_iterations_total, 1);
        assert_eq!(m.recursive_cte_rows_emitted_total, 1);
        db.cleanup();
    }

    #[test]
    fn the_working_table_beats_a_catalog_table_of_the_same_name() {
        let db = rec_db("d3_name_wins");
        db.seed_t(&[(100, Some("x"), None), (200, Some("y"), None)]);
        // `t` is a real table with ids 100 and 200; the recursion named `t` ignores it,
        // inside the term (working table) and outside (the finished CTE).
        assert_eq!(
            ints(&db, "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t"),
            vec![1, 2, 3]
        );
        // The catalog table is untouched and readable under its own name elsewhere.
        assert_eq!(ints(&db, "SELECT id FROM t ORDER BY id"), vec![100, 200]);
        db.cleanup();
    }

    #[test]
    fn the_term_may_contain_uncorrelated_and_correlated_subqueries() {
        let db = rec_db("d3_term_subquery");
        // base.v = 1..5: the bound is MAX(v) = 5.
        assert_eq!(
            ints(&db, "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < (SELECT MAX(v) FROM base)) SELECT n FROM r"),
            vec![1, 2, 3, 4, 5]
        );
        // Correlated to the working table's row: EXISTS (SELECT .. WHERE base.v > n).
        assert_eq!(
            ints(&db, "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE EXISTS (SELECT 1 FROM base WHERE base.v > r.n)) SELECT n FROM r"),
            vec![1, 2, 3, 4, 5]
        );
        // A subquery in the select list of the term.
        assert_eq!(
            ints(&db, "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + (SELECT 1) FROM r WHERE n < 4) SELECT n FROM r"),
            vec![1, 2, 3, 4]
        );
        db.cleanup();
    }

    #[test]
    fn the_result_works_with_the_outer_clauses() {
        let db = rec_db("d3_outer");
        assert_eq!(
            ints(&db, "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t ORDER BY n DESC LIMIT 3"),
            vec![10, 9, 8]
        );
        assert_eq!(
            ints(&db, "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT SUM(n) FROM t"),
            vec![55]
        );
        assert_eq!(
            ints(&db, "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t WHERE n % 2 = 0 ORDER BY n"),
            vec![2, 4, 6, 8, 10]
        );
        db.cleanup();
    }
}

// =======================================================================
// Limits: the depth and row bounds, the definitions bound, bytes
// =======================================================================

mod limits {
    use super::*;

    const UNBOUNDED: &str =
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT n FROM t";

    fn counter_to(top: i64) -> String {
        format!(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < {top}) SELECT n FROM t"
        )
    }

    #[test]
    fn the_defaults_are_the_documented_values() {
        let d = ExecLimits::default();
        assert_eq!(d.max_recursive_cte_depth, 1_000);
        assert_eq!(d.max_recursive_cte_rows, 1_000_000);
        assert_eq!(d.max_recursive_cte_definitions, 8);
    }

    #[test]
    fn a_recursion_with_no_stop_condition_ends_in_a_typed_error_naming_the_cte() {
        let db = rec_db("d3_unbounded");
        let (r, m) = run_with(&db, UNBOUNDED, &ExecLimits::default());
        let err = r.unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("recursive CTE 't'"), "{text}");
        assert!(text.contains("max_recursive_cte_depth (1000)"), "{text}");
        // It ran exactly the allowed number of iterations, no more.
        assert_eq!(m.recursive_cte_iterations_total, 1000);
        db.cleanup();
    }

    #[test]
    fn the_depth_bound_is_exact_one_thousand_iterations_pass_and_one_more_fails() {
        let db = rec_db("d3_depth_exact");
        // n < 1000: the term runs 1000 times (the last one produces nothing).
        let (r, m) = run_with(&db, &counter_to(1000), &ExecLimits::default());
        assert_eq!(r.unwrap().rows.len(), 1000);
        assert_eq!(m.recursive_cte_iterations_total, 1000);
        // n < 1001 needs a 1001st evaluation.
        let err = db.try_run(&counter_to(1001)).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn the_depth_bound_is_configurable_and_exact_at_small_values_too() {
        let db = rec_db("d3_depth_small");
        let limits = ExecLimits {
            max_recursive_cte_depth: 5,
            ..ExecLimits::default()
        };
        let (r, _) = run_with(&db, &counter_to(5), &limits);
        assert_eq!(r.unwrap().rows.len(), 5);
        let (r, _) = run_with(&db, &counter_to(6), &limits);
        let err = r.unwrap_err();
        assert!(err.to_string().contains("max_recursive_cte_depth (5)"), "{err}");
        // Zero iterations allowed: a recursion that needs one fails, a seed alone passes.
        let none = ExecLimits {
            max_recursive_cte_depth: 0,
            ..ExecLimits::default()
        };
        let empty_seed = "WITH RECURSIVE t(n) AS (SELECT 1 WHERE FALSE UNION ALL SELECT n + 1 FROM t) SELECT n FROM t";
        assert!(run_with(&db, empty_seed, &none).0.is_ok(), "no seed row, so no iteration is needed");
        assert!(run_with(&db, &counter_to(1), &none).0.is_err(), "even an empty term is an evaluation");
        db.cleanup();
    }

    #[test]
    fn the_row_bound_counts_every_row_the_seed_included_and_is_exact() {
        let db = rec_db("d3_rows_exact");
        let limits = ExecLimits {
            max_recursive_cte_rows: 20,
            ..ExecLimits::default()
        };
        let (r, _) = run_with(&db, &counter_to(20), &limits);
        assert_eq!(r.unwrap().rows.len(), 20);
        let (r, m) = run_with(&db, &counter_to(21), &limits);
        let err = r.unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("recursive CTE 't' produced more than max_recursive_cte_rows (20)"), "{err}");
        assert_eq!(m.recursive_cte_rows_emitted_total, 20, "nothing past the bound was kept");
        db.cleanup();
    }

    #[test]
    fn the_row_bound_fires_for_a_recursion_whose_depth_is_small() {
        let db = rec_db("d3_rows_wide");
        // Every working row joins to all 5 `base` rows: 1, 5, 25, 125, ... rows
        // per iteration, a handful of iterations.
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT b.v FROM r JOIN base b ON true) SELECT COUNT(*) FROM r";
        let limits = ExecLimits {
            max_recursive_cte_rows: 100,
            ..ExecLimits::default()
        };
        let (r, m) = run_with(&db, sql, &limits);
        let err = r.unwrap_err();
        assert!(err.to_string().contains("max_recursive_cte_rows (100)"), "{err}");
        assert!(m.recursive_cte_iterations_total < 10, "{m:?}");
        db.cleanup();
    }

    #[test]
    fn the_bytes_held_are_bounded() {
        let db = rec_db("d3_bytes");
        let limits = ExecLimits {
            max_materialized_bytes: 200,
            ..ExecLimits::default()
        };
        let (r, _) = run_with(&db, &counter_to(100), &limits);
        let err = r.unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_materialized_bytes"), "{err}");
        db.cleanup();
    }

    #[test]
    fn the_definitions_bound_counts_the_recursive_ctes_a_statement_runs() {
        let db = rec_db("d3_definitions");
        let many = |n: usize, referenced: usize| -> String {
            let defs: Vec<String> = (1..=n)
                .map(|i| format!("c{i}(n) AS (SELECT {i} UNION ALL SELECT n + 1 FROM c{i} WHERE n < {})", i + 1))
                .collect();
            let uses: Vec<String> = (1..=referenced).map(|i| format!("SELECT n FROM c{i}")).collect();
            format!("WITH RECURSIVE {} {}", defs.join(", "), uses.join(" UNION ALL "))
        };
        // The default is 8: eight run, nine are refused.
        assert!(db.try_run(&many(8, 8)).is_ok());
        let err = db.try_run(&many(9, 9)).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_recursive_cte_definitions is 8"), "{err}");
        // A recursive CTE nobody reads is bound but never runs, so it is not counted.
        assert!(db.try_run(&many(9, 8)).is_ok());
        // The bound is configurable.
        let two = ExecLimits {
            max_recursive_cte_definitions: 2,
            ..ExecLimits::default()
        };
        assert!(run_with(&db, &many(2, 2), &two).0.is_ok());
        assert!(run_with(&db, &many(3, 3), &two).0.is_err());
        db.cleanup();
    }

    #[test]
    fn the_bound_is_per_cte_not_shared() {
        let db = rec_db("d3_per_cte");
        let limits = ExecLimits {
            max_recursive_cte_depth: 6,
            ..ExecLimits::default()
        };
        // Two recursions of 5 iterations each (10 in all) under a depth bound of 6.
        let sql = "WITH RECURSIVE a(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM a WHERE n < 5), \
                   b(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM b WHERE n < 5) \
                   SELECT a.n, b.n FROM a JOIN b ON a.n = b.n";
        let (r, m) = run_with(&db, sql, &limits);
        assert_eq!(r.unwrap().rows.len(), 5);
        assert_eq!(m.recursive_cte_iterations_total, 10);
        db.cleanup();
    }

    #[test]
    fn a_refused_dml_statement_changes_nothing() {
        let db = rec_db("d3_dml_rollback");
        load(&db, "a", &some(&[1, 2, 3]));
        let err = db
            .try_write(
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) DELETE FROM a WHERE v IN (SELECT n FROM t)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        let err = db
            .try_write(
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) UPDATE a SET v = (SELECT MAX(n) FROM t)",
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert_eq!(sorted(run_tups(&db, "SELECT v FROM a")), col(&some(&[1, 2, 3])));
        db.cleanup();
    }

    #[test]
    fn a_deadline_stops_a_recursion() {
        let db = rec_db("d3_deadline");
        let limits = ExecLimits {
            deadline: Some(std::time::Duration::from_nanos(1)),
            ..ExecLimits::default()
        };
        let (r, _) = run_with(&db, UNBOUNDED, &limits);
        assert!(matches!(r.unwrap_err(), SqlError::DeadlineExceeded));
        db.cleanup();
    }
}

// =======================================================================
// D4 -- UNION (distinct) recursion
// =======================================================================

mod union {
    use super::*;

    #[test]
    fn the_counter_with_union_gives_the_same_rows_and_stops_at_the_end() {
        let db = rec_db("d4_counter");
        let (r, m) = run_with(&db, &COUNTER.replace("UNION ALL", "UNION"), &ExecLimits::default());
        assert_eq!(tups(&r.unwrap().rows), col(&some(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])));
        assert_eq!(m.recursive_cte_iterations_total, 10);
        db.cleanup();
    }

    const REACH_FROM_1: &str = "WITH RECURSIVE reach(id) AS (SELECT 1 {OP} SELECT cyc.nxt FROM reach JOIN cyc ON cyc.id = reach.id) SELECT id FROM reach";

    #[test]
    fn a_cycle_terminates_by_itself_under_union() {
        let db = rec_db("d4_cycle");
        let sql = REACH_FROM_1.replace("{OP}", "UNION");
        let (r, m) = run_with(&db, &sql, &ExecLimits::default());
        assert_eq!(sorted(tups(&r.unwrap().rows)), col(&some(&[1, 2, 3])));
        // 1 -> 2 (new), 2 -> 3 (new), 3 -> 1 (already produced: nothing new).
        assert_eq!(m.recursive_cte_iterations_total, 3);
        assert_eq!(m.recursive_cte_rows_emitted_total, 3);
        // From node 4 the walk visits 4, 3, 1, 2 and stops when it returns to 3.
        let sql = "WITH RECURSIVE reach(id) AS (SELECT 4 UNION SELECT cyc.nxt FROM reach JOIN cyc ON cyc.id = reach.id) SELECT id FROM reach";
        let (r, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(sorted(tups(&r.unwrap().rows)), col(&some(&[1, 2, 3, 4])));
        assert_eq!(m.recursive_cte_iterations_total, 4);
        db.cleanup();
    }

    #[test]
    fn the_same_cycle_under_union_all_runs_into_the_depth_bound() {
        let db = rec_db("d4_cycle_all");
        let sql = REACH_FROM_1.replace("{OP}", "UNION ALL");
        let (r, m) = run_with(&db, &sql, &ExecLimits::default());
        let err = r.unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("recursive CTE 'reach'"), "{err}");
        assert_eq!(m.recursive_cte_iterations_total, 1000);
        db.cleanup();
    }

    #[test]
    fn union_stops_when_nothing_new_appears_union_all_stops_when_nothing_appears() {
        let db = rec_db("d4_two_stops");
        // 1 -> 2 -> 3 -> 1: the third iteration produces a row, but not a new one.
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT (n % 3) + 1 FROM r) SELECT n FROM r";
        let (res, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(sorted(tups(&res.unwrap().rows)), col(&some(&[1, 2, 3])));
        assert_eq!(m.recursive_cte_iterations_total, 3);
        // The very same term under UNION ALL never produces an empty iteration.
        let (res, _) = run_with(&db, &sql.replace("UNION", "UNION ALL"), &ExecLimits::default());
        assert!(matches!(res.unwrap_err(), SqlError::ResourceLimit { .. }));
        db.cleanup();
    }

    #[test]
    fn union_deduplicates_against_everything_produced_so_far_not_just_the_last_iteration() {
        let db = rec_db("d4_all_so_far");
        // Node 1 comes back after THREE iterations; the iteration before it only
        // produced node 3. Comparing against the previous iteration alone would
        // accept 1 again and loop for ever.
        let sql = REACH_FROM_1.replace("{OP}", "UNION");
        let (res, _) = run_with(&db, &sql, &ExecLimits::default());
        assert!(res.is_ok());
        db.cleanup();
    }

    #[test]
    fn rows_with_nulls_are_one_row_for_the_stop_condition() {
        let db = rec_db("d4_nulls");
        // The seed is (1, NULL) (the root's NULL parent); the term yields the
        // same row every iteration. NULL = NULL is not how that is decided: the
        // row is not new, so the recursion stops after one iteration.
        let sql = "WITH RECURSIVE r(a, b) AS (SELECT id, parent FROM tree WHERE id = 1 UNION SELECT a, b FROM r) SELECT a, b FROM r";
        let (res, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(tups(&res.unwrap().rows), vec![vec![Some(1), None]]);
        assert_eq!(m.recursive_cte_iterations_total, 1);
        // Two NULL-bearing rows, one of them repeated.
        let sql = "WITH RECURSIVE r(a, b) AS (SELECT id, parent FROM tree WHERE parent IS NULL OR id = 2 UNION SELECT a, b FROM r) SELECT a, b FROM r";
        let (res, _) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(
            sorted(tups(&res.unwrap().rows)),
            vec![vec![Some(1), None], vec![Some(2), Some(1)]]
        );
        db.cleanup();
    }

    #[test]
    fn a_diamond_gives_the_shared_node_twice_under_union_all_and_once_under_union() {
        let db = rec_db("d4_diamond");
        let sql = |op: &str| {
            format!(
                "WITH RECURSIVE w(id) AS (SELECT 1 {op} SELECT edges.dst FROM w JOIN edges ON edges.src = w.id) SELECT id FROM w"
            )
        };
        let (all, m_all) = run_with(&db, &sql("UNION ALL"), &ExecLimits::default());
        // 1; 2, 3; 4 (via 2), 4 (via 3); 5, 5.
        assert_eq!(sorted(tups(&all.unwrap().rows)), col(&some(&[1, 2, 3, 4, 4, 5, 5])));
        assert_eq!(m_all.recursive_cte_rows_emitted_total, 7);
        let (dist, m_dist) = run_with(&db, &sql("UNION"), &ExecLimits::default());
        assert_eq!(sorted(tups(&dist.unwrap().rows)), col(&some(&[1, 2, 3, 4, 5])));
        assert_eq!(m_dist.recursive_cte_rows_emitted_total, 5, "counted after deduplication");
        db.cleanup();
    }

    #[test]
    fn a_seed_with_duplicates_is_deduplicated_under_union_and_kept_under_union_all() {
        let db = rec_db("d4_seed_dups");
        let sql = |op: &str| {
            format!("WITH RECURSIVE r(n) AS ((SELECT 1 UNION ALL SELECT 1) {op} SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r")
        };
        assert_eq!(
            sorted(run_tups(&db, &sql("UNION"))),
            col(&some(&[1, 2, 3]))
        );
        assert_eq!(
            sorted(run_tups(&db, &sql("UNION ALL"))),
            col(&some(&[1, 1, 2, 2, 3, 3]))
        );
        db.cleanup();
    }

    #[test]
    fn union_compares_whole_rows_not_just_the_first_column() {
        let db = rec_db("d4_whole_rows");
        // Two seed rows that share their first column: (1, 2) and (1, 3).
        let sql = "WITH RECURSIVE r(a, b) AS (SELECT src, dst FROM edges WHERE src = 1 UNION SELECT a, b FROM r) SELECT a, b FROM r";
        let (res, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(
            sorted(tups(&res.unwrap().rows)),
            vec![vec![Some(1), Some(2)], vec![Some(1), Some(3)]]
        );
        assert_eq!(m.recursive_cte_iterations_total, 1);
        // A row in the second column differing from a repeat in the first.
        let sql = "WITH RECURSIVE r(a, b) AS (SELECT src, dst FROM edges WHERE id = 1 UNION SELECT a, b + 1 FROM r WHERE b < 4) SELECT a, b FROM r";
        assert_eq!(
            sorted(run_tups(&db, sql)),
            vec![vec![Some(1), Some(2)], vec![Some(1), Some(3)], vec![Some(1), Some(4)]]
        );
        db.cleanup();
    }

    #[test]
    fn distinct_in_the_term_is_allowed_under_union() {
        let db = rec_db("d4_distinct_term");
        assert_eq!(
            sorted(run_tups(
                &db,
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT DISTINCT n + 1 FROM r WHERE n < 4) SELECT n FROM r"
            )),
            col(&some(&[1, 2, 3, 4]))
        );
        db.cleanup();
    }
}

// =======================================================================
// D5 -- finding the separator between the seed and the recursive term
// =======================================================================

mod shape {
    use super::*;

    #[test]
    fn with_three_operands_the_last_union_all_is_the_separator() {
        let db = rec_db("d5_three");
        // `(1 UNION ALL 100) UNION ALL (n + 1 ...)`: the seed is {1, 100}.
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT 100 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r";
        assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 2, 3, 100])));
        let rec = bound(&db, sql).ctes[0].recursion.clone().unwrap();
        let set = bound(&db, sql).ctes[0].query.set_op.clone().unwrap();
        // The seed is itself the set operation `1 UNION ALL 100`; the term is the plain SELECT.
        assert!(set.left.set_op.is_some());
        assert!(set.right.set_op.is_none());
        assert!(rec.all);
        db.cleanup();
    }

    #[test]
    fn a_parenthesised_right_operand_is_the_whole_recursive_term() {
        let db = rec_db("d5_right_nested");
        // `1 UNION (( n + 1 ... ) UNION ALL 100)`: the outer UNION separates; the
        // term is a set operation with the reference in one branch.
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION (SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT 100)) SELECT n FROM r";
        assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 2, 3, 100])));
        let set = bound(&db, sql).ctes[0].query.set_op.clone().unwrap();
        assert!(set.left.set_op.is_none(), "the seed is the single SELECT");
        assert!(set.right.set_op.is_some(), "the term is the inner set operation");
        assert!(!bound(&db, sql).ctes[0].recursion.as_ref().unwrap().all, "the OUTER operator decides: UNION");
        // The same data with the OUTER operator UNION ALL and the inner UNION: the outer one decides.
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM r WHERE n < 3 UNION SELECT 100)) SELECT n FROM r";
        let (res, m) = run_with(&db, sql, &ExecLimits::default());
        // 1; it1: working {1} -> {2, 100}; it2: {2, 100} -> {3, 100}; it3: {3, 100} -> {100}; it4 ... the 100 comes back
        // every iteration under the outer UNION ALL, so this never ends: the bound fires.
        let err = res.unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert_eq!(m.recursive_cte_iterations_total, 1000);
        db.cleanup();
    }

    #[test]
    fn a_seed_that_is_a_set_operation_runs_inside_the_seed() {
        let db = rec_db("d5_seed_setop");
        let sql = "WITH RECURSIVE r(n) AS ((SELECT 1 UNION ALL SELECT 2) UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r";
        // seed {1, 2}; it1: {2, 3}; it2: {3}... : working {1,2} -> 2, 3; {2,3} -> 3; {3} -> none.
        assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 2, 2, 3, 3])));
        // INTERSECT / EXCEPT / UNION inside the seed too.
        assert_eq!(
            sorted(run_tups(
                &db,
                "WITH RECURSIVE r(n) AS ((SELECT 1 UNION ALL SELECT 2 EXCEPT SELECT 2) UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r"
            )),
            col(&some(&[1, 2, 3]))
        );
        db.cleanup();
    }

    #[test]
    fn the_term_may_be_an_intersect_or_except_with_the_reference_in_one_branch() {
        let db = rec_db("d5_term_intersect");
        // term = (n + 1 FROM r) INTERSECT (SELECT v FROM base): the numbers 2..5.
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM r INTERSECT SELECT v FROM base)) SELECT n FROM r";
        assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 2, 3, 4, 5])));
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM r EXCEPT SELECT 4)) SELECT n FROM r";
        // 1, 2, 3, then 3 + 1 = 4 is removed by EXCEPT: the walk stops there.
        assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 2, 3])));
        db.cleanup();
    }

    #[test]
    fn a_set_operation_inside_a_branch_of_the_term_runs_inside_the_term() {
        let db = rec_db("d5_inner_in_branch");
        // The branch `SELECT n + 1 FROM r WHERE n < 3` plus a branch of constants
        // filtered by a subquery over the working table is rejected (a second
        // reference); an unrelated constant branch is fine and is repeated each
        // iteration, which UNION absorbs.
        let sql = "WITH RECURSIVE r(n) AS (SELECT 1 UNION (SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT 7 UNION ALL SELECT 7)) SELECT n FROM r";
        assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 2, 3, 7])));
        db.cleanup();
    }
}

// =======================================================================
// D6 -- several recursive CTEs, ordinary CTEs around them, derived tables,
//       subqueries, DML, snapshot consistency
// =======================================================================

mod contexts {
    use super::*;
    use std::sync::Arc;

    fn counter(name: &str, top: i64) -> String {
        format!("{name}(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM {name} WHERE n < {top})")
    }

    #[test]
    fn several_recursive_ctes_in_one_with_are_each_their_own_fixpoint() {
        let db = rec_db("d6_several");
        let sql = format!(
            "WITH RECURSIVE {}, {} SELECT a.n FROM a JOIN b ON a.n = b.n ORDER BY a.n",
            counter("a", 3),
            counter("b", 4)
        );
        let (r, m) = run_with(&db, &sql, &ExecLimits::default());
        assert_eq!(tups(&r.unwrap().rows), col(&some(&[1, 2, 3])));
        // 3 + 4 evaluations of two different terms; 3 + 4 rows.
        assert_eq!(m.recursive_cte_iterations_total, 7);
        assert_eq!(m.recursive_cte_rows_emitted_total, 7);
        db.cleanup();
    }

    #[test]
    fn two_recursive_ctes_with_different_operators_keep_their_own_stop_conditions() {
        let db = rec_db("d6_mixed_ops");
        let sql = "WITH RECURSIVE \
                   up(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM up WHERE n < 3), \
                   cyc_reach(id) AS (SELECT 1 UNION SELECT cyc.nxt FROM cyc_reach JOIN cyc ON cyc.id = cyc_reach.id) \
                   SELECT n FROM up UNION ALL SELECT id FROM cyc_reach";
        assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 1, 2, 2, 3, 3])));
        db.cleanup();
    }

    #[test]
    fn an_ordinary_cte_over_a_recursive_one_is_materialized_once_over_the_fixpoint() {
        let db = rec_db("d6_nonrec_over_rec");
        let sql = format!(
            "WITH RECURSIVE {}, v AS (SELECT n * 2 AS d FROM r) SELECT d FROM v ORDER BY d",
            counter("r", 3)
        );
        let (res, m) = run_with(&db, &sql, &ExecLimits::default());
        assert_eq!(tups(&res.unwrap().rows), col(&some(&[2, 4, 6])));
        assert_eq!(m.cte_materializations_total, 1, "`v` is materialized once");
        assert_eq!(m.recursive_cte_iterations_total, 3, "the fixpoint ran once");
        // A chain of ordinary CTEs after it.
        let sql = format!(
            "WITH RECURSIVE {}, v AS (SELECT n FROM r WHERE n > 1), w AS (SELECT n + 10 AS k FROM v) SELECT k FROM w ORDER BY k",
            counter("r", 3)
        );
        assert_eq!(run_tups(&db, &sql), col(&some(&[12, 13])));
        db.cleanup();
    }

    #[test]
    fn a_recursive_cte_read_twice_runs_its_fixpoint_once() {
        let db = rec_db("d6_twice");
        let sql = format!(
            "WITH RECURSIVE {} SELECT x.n, y.n FROM r x JOIN r y ON x.n = y.n ORDER BY x.n",
            counter("r", 3)
        );
        let (res, m) = run_with(&db, &sql, &ExecLimits::default());
        assert_eq!(
            tups(&res.unwrap().rows),
            vec![vec![Some(1), Some(1)], vec![Some(2), Some(2)], vec![Some(3), Some(3)]]
        );
        assert_eq!(m.recursive_cte_iterations_total, 3);
        assert_eq!(m.recursive_cte_rows_emitted_total, 3);
        // Read from a derived table, a scalar subquery and a later CTE in one statement.
        let sql = format!(
            "WITH RECURSIVE {}, v AS (SELECT n FROM r) \
             SELECT (SELECT COUNT(*) FROM r), (SELECT COUNT(*) FROM v), (SELECT COUNT(*) FROM (SELECT n FROM r) AS d)",
            counter("r", 3)
        );
        let (res, m) = run_with(&db, &sql, &ExecLimits::default());
        assert_eq!(tups(&res.unwrap().rows), vec![vec![Some(3), Some(3), Some(3)]]);
        assert_eq!(m.recursive_cte_iterations_total, 3);
        db.cleanup();
    }

    #[test]
    fn a_recursive_cte_nobody_reads_does_not_run() {
        let db = rec_db("d6_dead");
        let sql = format!("WITH RECURSIVE {} SELECT 1", counter("dead", 50));
        let (res, m) = run_with(&db, &sql, &ExecLimits::default());
        assert_eq!(tups(&res.unwrap().rows), col(&some(&[1])));
        assert_eq!(m.recursive_cte_iterations_total, 0);
        db.cleanup();
    }

    #[test]
    fn earlier_and_enclosing_ctes_are_visible_where_the_rules_say() {
        let db = rec_db("d6_visible");
        // An earlier CTE of the list feeds the seed.
        assert_eq!(
            ints(&db, "WITH RECURSIVE s(x) AS (SELECT 5), r(n) AS (SELECT x FROM s UNION ALL SELECT n + 1 FROM r WHERE n < 8) SELECT n FROM r"),
            vec![5, 6, 7, 8]
        );
        // A CTE of the enclosing query is visible in the term.
        assert_eq!(
            ints(&db, "WITH s(x) AS (SELECT 3) SELECT (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r JOIN s ON r.n < s.x) SELECT COUNT(*) FROM r)"),
            vec![3]
        );
        // A CTE after the recursive one reads it.
        assert_eq!(
            ints(&db, "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3), later(x) AS (SELECT n FROM r) SELECT x FROM later ORDER BY x"),
            vec![1, 2, 3]
        );
        db.cleanup();
    }

    #[test]
    fn a_recursive_cte_inside_a_derived_table() {
        let db = rec_db("d6_derived");
        assert_eq!(
            ints(&db, "SELECT d.n FROM (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 4) SELECT n FROM r) AS d WHERE d.n > 1 ORDER BY d.n"),
            vec![2, 3, 4]
        );
        // The recursive CTE read through a derived table of the main query.
        assert_eq!(
            ints(&db, "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 4) SELECT d.k FROM (SELECT n * 10 AS k FROM r) AS d ORDER BY d.k"),
            vec![10, 20, 30, 40]
        );
        db.cleanup();
    }

    #[test]
    fn a_recursive_cte_inside_scalar_in_and_exists_subqueries() {
        let db = rec_db("d6_subqueries");
        // The spec's shape: a COUNT over a derived table over the recursion.
        assert_eq!(
            ints(&db, "SELECT (SELECT COUNT(*) FROM (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n FROM r) AS d)"),
            vec![5]
        );
        assert_eq!(
            ints(&db, "SELECT (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT SUM(n) FROM r)"),
            vec![15]
        );
        // IN and EXISTS.
        assert_eq!(
            ints(&db, "SELECT v FROM base WHERE v IN (WITH RECURSIVE r(n) AS (SELECT 2 UNION ALL SELECT n + 2 FROM r WHERE n < 4) SELECT n FROM r) ORDER BY v"),
            vec![2, 4]
        );
        assert_eq!(
            ints(&db, "SELECT COUNT(*) FROM base WHERE EXISTS (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r WHERE n = 3)"),
            vec![5]
        );
        db.cleanup();
    }

    #[test]
    fn inside_a_correlated_subquery_the_consumer_runs_per_outer_row_and_the_fixpoint_once() {
        let db = rec_db("d6_correlated");
        db.seed_t(&[
            (1, Some("a"), None),
            (2, Some("b"), None),
            (3, Some("c"), None),
            (4, Some("d"), None),
            (5, Some("e"), None),
        ]);
        let sql = "SELECT t.id, (WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) \
                   SELECT COUNT(*) FROM r WHERE r.n <= t.id) FROM t ORDER BY t.id";
        let (res, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!(
            tups(&res.unwrap().rows),
            vec![
                vec![Some(1), Some(1)],
                vec![Some(2), Some(2)],
                vec![Some(3), Some(3)],
                vec![Some(4), Some(3)],
                vec![Some(5), Some(3)],
            ]
        );
        // The subquery is evaluated for each of the 5 outer rows; the recursion
        // inside it cannot read the outer row, so its 3 iterations ran once.
        assert_eq!(m.correlated_subquery_reexecs, 5);
        assert_eq!(m.recursive_cte_iterations_total, 3);
        db.cleanup();
    }

    #[test]
    fn with_recursive_before_delete_and_update_and_inside_their_subqueries() {
        let db = rec_db("d6_dml");
        load(&db, "a", &some(&[1, 2, 3, 4, 5, 6]));
        let r = db.write(&format!(
            "WITH RECURSIVE {} DELETE FROM a WHERE v IN (SELECT n FROM r)",
            counter("r", 3)
        ));
        assert_eq!(r.rows_affected, 3);
        assert_eq!(sorted(run_tups(&db, "SELECT v FROM a")), col(&some(&[4, 5, 6])));
        let r = db.write(&format!(
            "WITH RECURSIVE {} UPDATE a SET v = v + (SELECT MAX(n) FROM r) WHERE v > 4",
            counter("r", 3)
        ));
        assert_eq!(r.rows_affected, 2);
        assert_eq!(sorted(run_tups(&db, "SELECT v FROM a")), col(&some(&[4, 8, 9])));
        let r = db.write(
            "DELETE FROM a WHERE v IN (WITH RECURSIVE r(n) AS (SELECT 8 UNION ALL SELECT n + 1 FROM r WHERE n < 9) SELECT n FROM r)",
        );
        assert_eq!(r.rows_affected, 2);
        assert_eq!(run_tups(&db, "SELECT v FROM a"), col(&some(&[4])));
        db.cleanup();
    }

    #[test]
    fn a_recursive_cte_in_a_set_operation_and_under_order_by_and_limit() {
        let db = rec_db("d6_setop_around");
        let w = format!("WITH RECURSIVE {}", counter("r", 3));
        assert_eq!(
            sorted(run_tups(&db, &format!("{w} SELECT n FROM r UNION ALL SELECT n FROM r"))),
            col(&some(&[1, 1, 2, 2, 3, 3]))
        );
        assert_eq!(
            sorted(run_tups(&db, &format!("{w} SELECT n FROM r EXCEPT SELECT v FROM base WHERE v > 1"))),
            col(&some(&[1]))
        );
        assert_eq!(
            run_tups(&db, &format!("{w} SELECT n FROM r ORDER BY n DESC LIMIT 2")),
            col(&some(&[3, 2]))
        );
        db.cleanup();
    }

    #[test]
    fn the_recursion_reads_the_statements_snapshot_while_another_session_writes() {
        // `chain` is 1 <- 2 <- ... <- 300 (each row names its predecessor). A
        // writer keeps appending rows 301, 302, ... to the end of the chain
        // while the recursion walks it. If a later iteration saw the writer's
        // rows, the walk would be longer than the table the same statement
        // counts; at the statement's snapshot they agree exactly.
        let db = rec_db("d6_snapshot");
        db.write("CREATE TABLE chain (id INTEGER PRIMARY KEY, prev INTEGER)");
        let values: Vec<String> = (1..=300)
            .map(|i| format!("({i}, {})", if i == 1 { "NULL".to_string() } else { (i - 1).to_string() }))
            .collect();
        db.write(&format!("INSERT INTO chain (id, prev) VALUES {}", values.join(", ")));
        let store = Arc::clone(&db.store);
        let chain = db.table_id("chain");
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut diffs = Vec::new();
        let written = std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                let mut k = 301i32;
                while k < 2_000 && !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    store.put_row(chain, &[int(k), int(k - 1)]).unwrap();
                    k += 1;
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                k - 301
            });
            for _ in 0..4 {
                diffs.push(db.try_run(
                    "WITH RECURSIVE w(id) AS (SELECT 1 UNION ALL SELECT chain.id FROM w JOIN chain ON chain.prev = w.id) \
                     SELECT (SELECT COUNT(*) FROM w) - (SELECT COUNT(*) FROM chain)",
                ));
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            writer.join().unwrap()
        });
        assert!(written > 0, "the writer really ran");
        for d in diffs {
            assert_eq!(d.unwrap().rows, vec![vec![big(0)]]);
        }
        db.cleanup();
    }
}

// =======================================================================
// D7 -- EXPLAIN, metrics
// =======================================================================

mod observability {
    use super::*;

    fn explain(db: &Db, sql: &str) -> String {
        crate::plan::explain(&db.plan(sql))
    }

    #[test]
    fn explain_shows_the_seed_and_the_recursive_term_apart_never_a_flat_union() {
        let db = rec_db("d7_explain");
        let text = explain(&db, COUNTER);
        assert!(text.contains("RecursiveCte name=t distinct=false"), "{text}");
        assert!(text.contains("seed (runs once):"), "{text}");
        assert!(text.contains("recursive (runs once per iteration, reads the working table 't'):"), "{text}");
        assert!(text.contains("WorkingTable name=t"), "{text}");
        // No set operation node stands in for the separator.
        assert!(!text.contains("SetOp"), "{text}");
        // The seed comes before the term, and the working table is under the term.
        let seed = text.find("seed (runs once)").unwrap();
        let rec = text.find("recursive (runs once").unwrap();
        let work = text.find("WorkingTable").unwrap();
        assert!(seed < rec && rec < work, "{text}");
        // UNION says distinct=true.
        let text = explain(&db, &COUNTER.replace("UNION ALL", "UNION"));
        assert!(text.contains("RecursiveCte name=t distinct=true"), "{text}");
        assert!(text.contains("UNION: stops when an iteration produces no new distinct row"), "{text}");
        db.cleanup();
    }

    #[test]
    fn explain_of_a_term_with_a_set_operation_shows_it_inside_the_recursive_part() {
        let db = rec_db("d7_explain_setop");
        let text = explain(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION (SELECT n + 1 FROM r WHERE n < 3 UNION ALL SELECT 100)) SELECT n FROM r",
        );
        assert_eq!(text.matches("RecursiveCte").count(), 1, "{text}");
        let rec = text.find("recursive (runs once").unwrap();
        let setop = text.find("SetOp UNION ALL").expect(&text);
        assert!(rec < setop, "the set operation is part of the term: {text}");
        db.cleanup();
    }

    #[test]
    fn a_recursive_cte_read_twice_is_printed_once() {
        let db = rec_db("d7_explain_twice");
        let text = explain(
            &db,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT x.n FROM r x JOIN r y ON x.n = y.n",
        );
        assert_eq!(text.matches("RecursiveCte").count(), 1, "{text}");
        assert_eq!(text.matches("defined here").count(), 1, "{text}");
        db.cleanup();
    }

    #[test]
    fn the_metrics_count_iterations_and_rows_across_every_recursive_cte_in_the_statement() {
        let db = rec_db("d7_metrics");
        let (_, m) = run_with(&db, COUNTER, &ExecLimits::default());
        assert_eq!((m.recursive_cte_iterations_total, m.recursive_cte_rows_emitted_total), (10, 10));
        // Two fixpoints in one statement add up.
        let sql = "WITH RECURSIVE a(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM a WHERE n < 3), \
                   b(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM b WHERE n < 5) \
                   SELECT a.n FROM a JOIN b ON a.n = b.n";
        let (_, m) = run_with(&db, sql, &ExecLimits::default());
        assert_eq!((m.recursive_cte_iterations_total, m.recursive_cte_rows_emitted_total), (8, 8));
        // A statement without recursion records none.
        let (_, m) = run_with(&db, "SELECT v FROM base", &ExecLimits::default());
        assert_eq!((m.recursive_cte_iterations_total, m.recursive_cte_rows_emitted_total), (0, 0));
        // The metrics accumulate in the shared ExecMetrics across statements.
        let metrics = ExecMetrics::default();
        for _ in 0..2 {
            db.try_run_with(COUNTER, &ExecLimits::default(), &metrics, &[]).unwrap();
        }
        assert_eq!(metrics.snapshot().recursive_cte_iterations_total, 20);
        db.cleanup();
    }

    #[test]
    fn a_failed_recursion_still_reports_the_work_it_did() {
        let db = rec_db("d7_metrics_failed");
        let limits = ExecLimits {
            max_recursive_cte_depth: 7,
            ..ExecLimits::default()
        };
        let (r, m) = run_with(
            &db,
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SELECT n FROM t",
            &limits,
        );
        assert!(r.is_err());
        assert_eq!(m.recursive_cte_iterations_total, 7);
        assert_eq!(m.recursive_cte_rows_emitted_total, 8, "the seed and the 7 iterations' rows");
        db.cleanup();
    }
}

// =======================================================================
// Differential: the fixpoint against an independent loop
// =======================================================================

mod diff {
    use super::*;

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
    }

    /// The fixpoint of "reachable from `start`" over `edges`, computed with a
    /// plain working-list loop (no SQL, no engine code): the result rows in
    /// production order and how many times the step ran.
    fn model_reach(edges: &[(i64, i64)], start: i64, distinct: bool) -> (Vec<i64>, u64) {
        let mut result = vec![start];
        let mut seen = std::collections::BTreeSet::new();
        seen.insert(start);
        let mut working = vec![start];
        let mut iterations = 0u64;
        while !working.is_empty() {
            iterations += 1;
            let mut produced = Vec::new();
            for w in &working {
                for (s, d) in edges {
                    if s == w {
                        produced.push(*d);
                    }
                }
            }
            let next: Vec<i64> = if distinct {
                produced.into_iter().filter(|d| seen.insert(*d)).collect()
            } else {
                produced
            };
            result.extend(next.iter().copied());
            working = next;
        }
        (result, iterations)
    }

    fn reload_edges(db: &Db, edges: &[(i64, i64)]) {
        db.write("DELETE FROM edges");
        for (i, (s, d)) in edges.iter().enumerate() {
            db.write(&format!("INSERT INTO edges (id, src, dst) VALUES ({}, {s}, {d})", i + 1));
        }
    }

    fn reach_sql(op: &str) -> String {
        format!(
            "WITH RECURSIVE reach(id) AS (SELECT 0 {op} SELECT edges.dst FROM reach JOIN edges ON edges.src = reach.id) SELECT id FROM reach"
        )
    }

    #[test]
    fn union_all_over_random_dags_matches_the_model_rows_and_iteration_count() {
        let db = rec_db("diff_dag");
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..25 {
            // A random DAG on nodes 0..6 (edges go to a larger node, so it ends).
            let edges: Vec<(i64, i64)> = (0..rng.below(10))
                .map(|_| {
                    let s = rng.below(6) as i64;
                    (s, s + 1 + rng.below(6 - s as u64).min(5) as i64)
                })
                .filter(|(_, d)| *d < 7)
                .collect();
            reload_edges(&db, &edges);
            let (want_rows, want_iterations) = model_reach(&edges, 0, false);
            let (res, m) = run_with(&db, &reach_sql("UNION ALL"), &ExecLimits::default());
            let got = sorted(tups(&res.unwrap().rows));
            assert_eq!(got, sorted(col(&want_rows.iter().map(|v| Some(*v)).collect::<Vec<_>>())), "{edges:?}");
            assert_eq!(m.recursive_cte_iterations_total, want_iterations, "{edges:?}");
            assert_eq!(m.recursive_cte_rows_emitted_total, want_rows.len() as u64, "{edges:?}");
        }
        db.cleanup();
    }

    #[test]
    fn union_over_random_graphs_with_cycles_matches_the_model() {
        let db = rec_db("diff_cyclic");
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        for _ in 0..25 {
            let edges: Vec<(i64, i64)> = (0..rng.below(12))
                .map(|_| (rng.below(6) as i64, rng.below(6) as i64))
                .collect();
            reload_edges(&db, &edges);
            let (want_rows, want_iterations) = model_reach(&edges, 0, true);
            let (res, m) = run_with(&db, &reach_sql("UNION"), &ExecLimits::default());
            let got = sorted(tups(&res.unwrap().rows));
            assert_eq!(got, sorted(col(&want_rows.iter().map(|v| Some(*v)).collect::<Vec<_>>())), "{edges:?}");
            assert_eq!(m.recursive_cte_iterations_total, want_iterations, "{edges:?}");
            assert_eq!(m.recursive_cte_rows_emitted_total, want_rows.len() as u64, "{edges:?}");
        }
        db.cleanup();
    }

    #[test]
    fn a_random_cycle_under_union_all_is_stopped_by_the_depth_bound_not_by_luck() {
        let db = rec_db("diff_loop");
        reload_edges(&db, &[(0, 1), (1, 2), (2, 0)]);
        let limits = ExecLimits {
            max_recursive_cte_depth: 50,
            ..ExecLimits::default()
        };
        let (res, m) = run_with(&db, &reach_sql("UNION ALL"), &limits);
        assert!(matches!(res.unwrap_err(), SqlError::ResourceLimit { .. }));
        assert_eq!(m.recursive_cte_iterations_total, 50);
        db.cleanup();
    }

    #[test]
    fn random_trees_walk_to_the_levels_the_model_computes() {
        let db = rec_db("diff_tree");
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        db.write("CREATE TABLE rt (id INTEGER PRIMARY KEY, parent INTEGER)");
        for _ in 0..15 {
            let n = 1 + rng.below(12) as i64;
            let parents: Vec<Option<i64>> = (1..=n)
                .map(|i| if i == 1 { None } else { Some(1 + rng.below((i - 1) as u64) as i64) })
                .collect();
            db.write("DELETE FROM rt");
            for (i, p) in parents.iter().enumerate() {
                db.write(&format!(
                    "INSERT INTO rt (id, parent) VALUES ({}, {})",
                    i + 1,
                    p.map(|v| v.to_string()).unwrap_or_else(|| "NULL".to_string())
                ));
            }
            // Model: depth of each node by walking up.
            let depth_of = |mut id: i64| -> i64 {
                let mut d = 0;
                while let Some(p) = parents[(id - 1) as usize] {
                    id = p;
                    d += 1;
                }
                d
            };
            let want: Vec<Tup> = (1..=n).map(|i| vec![Some(i), Some(depth_of(i))]).collect();
            let got = sorted(run_tups(
                &db,
                "WITH RECURSIVE w(id, depth) AS (SELECT id, 0 FROM rt WHERE parent IS NULL \
                 UNION ALL SELECT rt.id, w.depth + 1 FROM w JOIN rt ON rt.parent = w.id) SELECT id, depth FROM w",
            ));
            assert_eq!(got, sorted(want), "{parents:?}");
        }
        db.cleanup();
    }
}
