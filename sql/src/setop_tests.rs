//! SQL-mission-1, Slice E -- set operations (`UNION` / `INTERSECT` /
//! `EXCEPT`, each with and without `ALL`).
//!
//! Layout: `parser` (E1: shape, precedence, associativity), `binder` (E2:
//! column count, type unification, `ORDER BY`), then the execution modules
//! added with E3..E7, and the HTTP tests in `api/tests/api_sql_integration.rs`.

use crate::mission1_support::*;

use rubixdb::relational::{RelationalType, RelationalValue};

use crate::bound::{BoundExprKind, BoundSelect, BoundStatement};
use crate::error::SqlError;

/// The shared fixture schema plus `nums`, one column of every type:
/// `nums(id INTEGER PK, i INTEGER, b BIGINT, r REAL, d DOUBLE,
/// dec2 DECIMAL(10,2), dec4 DECIMAL(12,4), txt TEXT, flag BOOLEAN, dt DATE,
/// tm TIME, ts TIMESTAMP, bl BLOB)`.
pub(crate) fn typed_db(tag: &str) -> Db {
    let db = Db::new(tag);
    db.write(
        "CREATE TABLE nums (id INTEGER PRIMARY KEY, i INTEGER, b BIGINT, r REAL, d DOUBLE,          dec2 DECIMAL(10,2), dec4 DECIMAL(12,4), txt TEXT, flag BOOLEAN, dt DATE, tm TIME,          ts TIMESTAMP, bl BLOB)",
    );
    db
}

pub(crate) fn bound(db: &Db, sql: &str) -> Box<BoundSelect> {
    match db
        .try_bind(sql)
        .unwrap_or_else(|e| panic!("expected {sql:?} to bind, got {e}"))
    {
        BoundStatement::Select(s) => s,
        other => panic!("expected a SELECT, got {other:?}"),
    }
}

pub(crate) fn bind_err(db: &Db, sql: &str) -> SqlError {
    db.try_bind(sql)
        .err()
        .unwrap_or_else(|| panic!("expected {sql:?} to fail to bind"))
}

fn output_types(db: &Db, sql: &str) -> Vec<Option<RelationalType>> {
    bound(db, sql).projection.iter().map(|i| i.expr.ty).collect()
}

fn output_names(db: &Db, sql: &str) -> Vec<String> {
    bound(db, sql)
        .projection
        .iter()
        .map(|i| i.output_name.clone())
        .collect()
}

// =======================================================================
// E1 -- parser: operators, precedence, associativity
// =======================================================================

mod parser {
    use std::collections::{BTreeSet, HashMap};

    use crate::ast::*;
    use crate::error::SqlError;
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

    fn set_op(s: &Select) -> &SetOperation {
        s.set_op
            .as_ref()
            .unwrap_or_else(|| panic!("expected a set operation, got {s:?}"))
    }

    /// `(operator, all)` of a set-operation node.
    fn head(s: &Select) -> (SetOperator, bool) {
        let o = set_op(s);
        (o.op, o.all)
    }

    /// The tree as text -- `UNION(a, INTERSECT(b, c))` -- with a plain
    /// `SELECT v FROM x` operand printed as `x`.
    fn shape(s: &Select) -> String {
        match &s.set_op {
            None => {
                let from = s.from.as_ref().expect("a leaf in these tests has a FROM");
                from.first.name.last().value.clone()
            }
            Some(o) => format!(
                "{}{}({}, {})",
                o.op.name(),
                if o.all { " ALL" } else { "" },
                shape(&o.left),
                shape(&o.right)
            ),
        }
    }

    #[test]
    fn each_of_the_six_operators_parses_to_its_operator_and_quantifier() {
        for (sql, op, all) in [
            ("SELECT v FROM a UNION SELECT v FROM b", SetOperator::Union, false),
            ("SELECT v FROM a UNION ALL SELECT v FROM b", SetOperator::Union, true),
            ("SELECT v FROM a INTERSECT SELECT v FROM b", SetOperator::Intersect, false),
            ("SELECT v FROM a INTERSECT ALL SELECT v FROM b", SetOperator::Intersect, true),
            ("SELECT v FROM a EXCEPT SELECT v FROM b", SetOperator::Except, false),
            ("SELECT v FROM a EXCEPT ALL SELECT v FROM b", SetOperator::Except, true),
        ] {
            let s = select(sql);
            assert_eq!(head(&s), (op, all), "{sql}");
            assert_eq!(shape(&s).replace(' ', ""), format!("{}{}(a,b)", op.name(), if all { "ALL" } else { "" }));
            // The operation is the whole query: no clause of a plain SELECT.
            assert!(s.projection.is_empty() && s.from.is_none() && s.selection.is_none());
            assert!(!s.distinct && s.group_by.is_empty() && s.having.is_none());
        }
    }

    #[test]
    fn an_explicit_distinct_quantifier_is_the_default_one() {
        for op in ["UNION", "INTERSECT", "EXCEPT"] {
            let plain = select(&format!("SELECT v FROM a {op} SELECT v FROM b"));
            let distinct = select(&format!("SELECT v FROM a {op} DISTINCT SELECT v FROM b"));
            assert_eq!(plain, distinct, "{op}");
            assert!(!head(&plain).1);
        }
    }

    #[test]
    fn intersect_binds_tighter_than_union_and_except() {
        // A UNION B INTERSECT C == A UNION (B INTERSECT C)
        assert_eq!(
            shape(&select("SELECT v FROM a UNION SELECT v FROM b INTERSECT SELECT v FROM c")),
            "UNION(a, INTERSECT(b, c))"
        );
        // A INTERSECT B UNION C == (A INTERSECT B) UNION C
        assert_eq!(
            shape(&select("SELECT v FROM a INTERSECT SELECT v FROM b UNION SELECT v FROM c")),
            "UNION(INTERSECT(a, b), c)"
        );
        // A EXCEPT B INTERSECT C == A EXCEPT (B INTERSECT C)
        assert_eq!(
            shape(&select("SELECT v FROM a EXCEPT SELECT v FROM b INTERSECT SELECT v FROM c")),
            "EXCEPT(a, INTERSECT(b, c))"
        );
        // The quantifier stays with its own operator.
        assert_eq!(
            shape(&select(
                "SELECT v FROM a UNION ALL SELECT v FROM b INTERSECT SELECT v FROM c"
            )),
            "UNION ALL(a, INTERSECT(b, c))"
        );
    }

    #[test]
    fn union_and_except_are_left_associative_at_one_level() {
        assert_eq!(
            shape(&select("SELECT v FROM a UNION SELECT v FROM b EXCEPT SELECT v FROM c")),
            "EXCEPT(UNION(a, b), c)"
        );
        assert_eq!(
            shape(&select("SELECT v FROM a EXCEPT SELECT v FROM b UNION SELECT v FROM c")),
            "UNION(EXCEPT(a, b), c)"
        );
        assert_eq!(
            shape(&select("SELECT v FROM a EXCEPT SELECT v FROM b EXCEPT SELECT v FROM c")),
            "EXCEPT(EXCEPT(a, b), c)"
        );
        assert_eq!(
            shape(&select("SELECT v FROM a UNION SELECT v FROM b UNION SELECT v FROM c")),
            "UNION(UNION(a, b), c)"
        );
        assert_eq!(
            shape(&select(
                "SELECT v FROM a INTERSECT SELECT v FROM b INTERSECT SELECT v FROM c"
            )),
            "INTERSECT(INTERSECT(a, b), c)"
        );
    }

    #[test]
    fn parentheses_override_precedence_and_associativity() {
        assert_eq!(
            shape(&select(
                "(SELECT v FROM a UNION SELECT v FROM b) INTERSECT SELECT v FROM c"
            )),
            "INTERSECT(UNION(a, b), c)"
        );
        assert_eq!(
            shape(&select(
                "SELECT v FROM a EXCEPT (SELECT v FROM b EXCEPT SELECT v FROM c)"
            )),
            "EXCEPT(a, EXCEPT(b, c))"
        );
        assert_eq!(
            shape(&select(
                "(SELECT v FROM a) UNION ((SELECT v FROM b) UNION (SELECT v FROM c))"
            )),
            "UNION(a, UNION(b, c))"
        );
    }

    /// The parse trees are evaluated over three tiny relations chosen so the
    /// two possible parses of `A UNION B INTERSECT C` give different answers:
    /// `A UNION (B INTERSECT C)` = {1, 2, 3} but `(A UNION B) INTERSECT C` =
    /// {3}. (Execution of the same statements over real tables is in the
    /// execution modules.)
    fn eval(s: &Select, rel: &HashMap<&str, Vec<i32>>) -> BTreeSet<i32> {
        match &s.set_op {
            None => {
                let name = s.from.as_ref().unwrap().first.name.last().value.clone();
                rel[name.as_str()].iter().copied().collect()
            }
            Some(o) => {
                let (l, r) = (eval(&o.left, rel), eval(&o.right, rel));
                match o.op {
                    SetOperator::Union => l.union(&r).copied().collect(),
                    SetOperator::Intersect => l.intersection(&r).copied().collect(),
                    SetOperator::Except => l.difference(&r).copied().collect(),
                }
            }
        }
    }

    #[test]
    fn the_two_parses_of_union_then_intersect_give_different_answers_on_distinguishing_data() {
        let rel: HashMap<&str, Vec<i32>> =
            HashMap::from([("a", vec![1, 2]), ("b", vec![2, 3]), ("c", vec![3, 4])]);
        let unparenthesised =
            select("SELECT v FROM a UNION SELECT v FROM b INTERSECT SELECT v FROM c");
        let parenthesised =
            select("(SELECT v FROM a UNION SELECT v FROM b) INTERSECT SELECT v FROM c");
        assert_eq!(eval(&unparenthesised, &rel), BTreeSet::from([1, 2, 3]));
        assert_eq!(eval(&parenthesised, &rel), BTreeSet::from([3]));
        // And the same for EXCEPT: A EXCEPT B INTERSECT C = A EXCEPT (B INTERSECT C).
        let e = select("SELECT v FROM a EXCEPT SELECT v FROM b INTERSECT SELECT v FROM c");
        let e_paren = select("(SELECT v FROM a EXCEPT SELECT v FROM b) INTERSECT SELECT v FROM c");
        assert_eq!(eval(&e, &rel), BTreeSet::from([1, 2]));
        assert_eq!(eval(&e_paren, &rel), BTreeSet::new());
        // And left associativity: (C EXCEPT B) UNION A differs from
        // C EXCEPT (B UNION A).
        let l = select("SELECT v FROM c EXCEPT SELECT v FROM b UNION SELECT v FROM a");
        let r = select("SELECT v FROM c EXCEPT (SELECT v FROM b UNION SELECT v FROM a)");
        assert_eq!(eval(&l, &rel), BTreeSet::from([1, 2, 4]));
        assert_eq!(eval(&r, &rel), BTreeSet::from([4]));
    }

    #[test]
    fn a_trailing_order_by_and_limit_belong_to_the_whole_set_operation() {
        let s = select("SELECT v FROM a UNION SELECT v FROM b ORDER BY v DESC LIMIT 3 OFFSET 1");
        assert_eq!(s.order_by.len(), 1);
        assert!(s.order_by[0].descending);
        assert!(s.limit.is_some() && s.offset.is_some());
        let o = set_op(&s);
        for branch in [&o.left, &o.right] {
            assert!(branch.order_by.is_empty() && branch.limit.is_none(), "{branch:?}");
        }
    }

    #[test]
    fn a_parenthesised_operand_keeps_its_own_order_by_and_limit() {
        let s = select(
            "(SELECT v FROM a ORDER BY v LIMIT 1) UNION ALL (SELECT v FROM b ORDER BY v DESC LIMIT 2)",
        );
        assert!(s.order_by.is_empty() && s.limit.is_none());
        let o = set_op(&s);
        assert_eq!(o.left.order_by.len(), 1);
        assert!(o.left.limit.is_some());
        assert!(o.right.order_by[0].descending);
        assert!(o.right.limit.is_some());
    }

    #[test]
    fn a_with_list_in_front_of_a_set_operation_belongs_to_the_whole_operation() {
        let s = select("WITH c AS (SELECT 1 AS v) SELECT v FROM c UNION SELECT v FROM c");
        assert_eq!(s.ctes.len(), 1);
        let o = set_op(&s);
        assert!(o.left.ctes.is_empty() && o.right.ctes.is_empty());
    }

    #[test]
    fn a_set_operation_parses_as_a_subquery_derived_table_and_cte_body() {
        // Scalar / IN / EXISTS subquery.
        let s = select("SELECT (SELECT v FROM a UNION SELECT v FROM b) FROM t");
        let SelectItem::Item(item) = &s.projection[0] else { panic!() };
        let Expr::ScalarSubquery(q) = &item.expr else { panic!() };
        let QueryExpr::Select(inner) = q.as_ref();
        assert!(inner.set_op.is_some());
        let s = select("SELECT 1 FROM t WHERE id IN (SELECT v FROM a UNION SELECT v FROM b)");
        let Some(Expr::InSubquery { subquery, .. }) = &s.selection else { panic!() };
        let QueryExpr::Select(inner) = subquery.as_ref();
        assert!(inner.set_op.is_some());
        let s = select("SELECT 1 FROM t WHERE EXISTS (SELECT v FROM a EXCEPT SELECT v FROM b)");
        let Some(Expr::Exists { subquery, .. }) = &s.selection else { panic!() };
        let QueryExpr::Select(inner) = subquery.as_ref();
        assert_eq!(head(inner), (SetOperator::Except, false));
        // Derived table (the prompt's exact shape).
        let s = select("SELECT * FROM ((SELECT v FROM a) UNION (SELECT v FROM b)) AS u");
        let body = s.from.as_ref().unwrap().first.derived.as_ref().unwrap();
        assert_eq!(head(body), (SetOperator::Union, false));
        // CTE body (the prompt's UNION ALL separator shape).
        let s = select("WITH cte AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM cte");
        assert_eq!(head(&s.ctes[0].query), (SetOperator::Union, true));
    }

    #[test]
    fn a_set_operation_parses_in_update_and_delete_where() {
        let limits = SqlLimits::default();
        for sql in [
            "DELETE FROM t WHERE id IN (SELECT v FROM a UNION SELECT v FROM b)",
            "UPDATE t SET name = 'x' WHERE id IN (SELECT v FROM a INTERSECT SELECT v FROM b)",
            "UPDATE t SET n = (SELECT v FROM a EXCEPT SELECT v FROM b) WHERE id = 1",
        ] {
            parse_statement(sql, &limits)
                .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"));
        }
    }

    #[test]
    fn what_is_still_not_a_set_operation_is_a_typed_unsupported_error() {
        for sql in [
            // Another dialect's spelling of EXCEPT.
            "SELECT v FROM a MINUS SELECT v FROM b",
            // Column matching by name rather than position.
            "SELECT v FROM a UNION BY NAME SELECT v FROM b",
            "SELECT v FROM a UNION ALL BY NAME SELECT v FROM b",
            // Not a SELECT operand.
            "VALUES (1) UNION SELECT v FROM b",
            "SELECT v FROM a UNION VALUES (1)",
            "SELECT v FROM a UNION TABLE b",
            // A bare parenthesised body is not a query shape this grammar has.
            "(SELECT v FROM a)",
        ] {
            let err = parse_statement(sql, &SqlLimits::default()).unwrap_err();
            assert!(
                matches!(err, SqlError::Unsupported { .. } | SqlError::Parse { .. }),
                "{sql:?} -> {err:?}"
            );
        }
        // Twin: the spelled-out forms next to them DO parse, so the
        // refusals above are about those spellings, not about set operations.
        for sql in [
            "SELECT v FROM a EXCEPT SELECT v FROM b",
            "SELECT v FROM a UNION ALL SELECT v FROM b",
            "(SELECT v FROM a) UNION (SELECT v FROM b)",
        ] {
            parse_statement(sql, &SqlLimits::default())
                .unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"));
        }
    }

    #[test]
    fn a_very_long_chain_is_a_controlled_limit_error_not_a_stack_overflow() {
        let chain = |n: usize| {
            (0..n)
                .map(|_| "SELECT v FROM a")
                .collect::<Vec<_>>()
                .join(" UNION ALL ")
        };
        let ok = select(&chain(100));
        // Left-nested: the top node's left operand is the 99-branch chain.
        assert!(set_op(&ok).left.set_op.is_some());
        let err = parse_statement(&chain(400), &SqlLimits::default()).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
    }
}

// =======================================================================
// E2 -- binder: column count, type unification, names, ORDER BY
// =======================================================================

mod binder {
    use super::*;
    use crate::bind::setop::unify_types;
    use crate::limits::SqlLimits;
    use RelationalType::*;

    // ---- column count -------------------------------------------------------------

    #[test]
    fn a_column_count_mismatch_is_a_bind_error_for_every_operator() {
        let db = typed_db("e2_count");
        for op in [
            "UNION",
            "UNION ALL",
            "INTERSECT",
            "INTERSECT ALL",
            "EXCEPT",
            "EXCEPT ALL",
        ] {
            let sql = format!("SELECT id, name FROM t {op} SELECT id FROM t");
            let err = bind_err(&db, &sql);
            assert!(matches!(err, SqlError::TypeMismatch { .. }), "{sql}: {err:?}");
            let text = err.to_string();
            assert!(
                text.contains("same number of columns") && text.contains("left one has 2") && text.contains("right one has 1"),
                "{sql}: {text}"
            );
            // And the other way round.
            let sql = format!("SELECT id FROM t {op} SELECT id, name FROM t");
            assert!(matches!(bind_err(&db, &sql), SqlError::TypeMismatch { .. }), "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn a_count_mismatch_anywhere_in_a_longer_chain_is_found() {
        let db = typed_db("e2_count_chain");
        for sql in [
            "SELECT id FROM t UNION SELECT id FROM t UNION SELECT id, name FROM t",
            "SELECT id FROM t UNION SELECT id, name FROM t UNION SELECT id FROM t",
            "SELECT id, name FROM t UNION SELECT id FROM t UNION SELECT id FROM t",
            // The mismatch is inside a parenthesised sub-operation.
            "SELECT id FROM t UNION (SELECT id FROM t INTERSECT SELECT id, name FROM t)",
            // A star expands to a different number of columns than the other side.
            "SELECT * FROM t UNION SELECT id FROM t",
            "SELECT id FROM t EXCEPT SELECT * FROM orders",
        ] {
            let err = bind_err(&db, sql);
            assert!(matches!(err, SqlError::TypeMismatch { .. }), "{sql}: {err:?}");
        }
        // Matching counts through a star on both sides is fine.
        bound(&db, "SELECT * FROM t UNION SELECT * FROM t");
        db.cleanup();
    }

    // ---- type unification --------------------------------------------------------------

    /// The numeric widening table, written out by hand (this is the
    /// documented contract, not derived from the implementation).
    /// Rows and columns: INTEGER, BIGINT, REAL, DOUBLE, DECIMAL(2), DECIMAL(4).
    const NUMERIC_TYPES: [RelationalType; 6] = [
        Integer,
        Bigint,
        Real,
        Double,
        Decimal { scale: 2 },
        Decimal { scale: 4 },
    ];
    const NUMERIC_COLUMNS: [&str; 6] = ["i", "b", "r", "d", "dec2", "dec4"];
    fn expected_numeric(row: usize, col: usize) -> RelationalType {
        let table: [[RelationalType; 6]; 6] = [
            // INTEGER
            [Integer, Bigint, Double, Double, Decimal { scale: 2 }, Decimal { scale: 4 }],
            // BIGINT
            [Bigint, Bigint, Double, Double, Decimal { scale: 2 }, Decimal { scale: 4 }],
            // REAL
            [Double, Double, Real, Double, Double, Double],
            // DOUBLE
            [Double, Double, Double, Double, Double, Double],
            // DECIMAL(scale 2)
            [Decimal { scale: 2 }, Decimal { scale: 2 }, Double, Double, Decimal { scale: 2 }, Decimal { scale: 4 }],
            // DECIMAL(scale 4)
            [Decimal { scale: 4 }, Decimal { scale: 4 }, Double, Double, Decimal { scale: 4 }, Decimal { scale: 4 }],
        ];
        table[row][col]
    }

    #[test]
    fn every_pair_of_numeric_types_widens_as_documented_through_the_real_binder() {
        let db = typed_db("e2_widen");
        for (r, left) in NUMERIC_COLUMNS.iter().enumerate() {
            for (c, right) in NUMERIC_COLUMNS.iter().enumerate() {
                let sql = format!("SELECT {left} FROM nums UNION SELECT {right} FROM nums");
                assert_eq!(
                    output_types(&db, &sql),
                    vec![Some(expected_numeric(r, c))],
                    "{sql}"
                );
            }
        }
        db.cleanup();
    }

    #[test]
    fn unify_types_agrees_with_the_documented_table_for_every_pair_and_is_symmetric() {
        for (r, a) in NUMERIC_TYPES.iter().enumerate() {
            for (c, b) in NUMERIC_TYPES.iter().enumerate() {
                assert_eq!(unify_types(*a, *b), Some(expected_numeric(r, c)), "{a:?} {b:?}");
                assert_eq!(unify_types(*b, *a), unify_types(*a, *b), "symmetry {a:?} {b:?}");
            }
        }
        // Associativity over every triple of numeric types: the result does
        // not depend on how a chain is grouped.
        for a in NUMERIC_TYPES {
            for b in NUMERIC_TYPES {
                for c in NUMERIC_TYPES {
                    let left = unify_types(a, b).and_then(|ab| unify_types(ab, c));
                    let right = unify_types(b, c).and_then(|bc| unify_types(a, bc));
                    assert_eq!(left, right, "{a:?} {b:?} {c:?}");
                }
            }
        }
    }

    #[test]
    fn the_integer_and_bigint_example_from_the_contract_is_bigint() {
        let db = typed_db("e2_int_bigint");
        // {1 (INT)} UNION {2 (BIGINT)}: the column is BIGINT.
        assert_eq!(
            output_types(&db, "SELECT i FROM nums UNION SELECT b FROM nums"),
            vec![Some(Bigint)]
        );
        // The same through a literal on each side.
        assert_eq!(
            output_types(&db, "SELECT 1 UNION SELECT 10000000000"),
            vec![Some(Bigint)]
        );
        // INT + DOUBLE -> DOUBLE; INT + DECIMAL -> DECIMAL.
        assert_eq!(output_types(&db, "SELECT 1 UNION SELECT 2.5"), vec![Some(Double)]);
        assert_eq!(
            output_types(&db, "SELECT i FROM nums UNION SELECT dec2 FROM nums"),
            vec![Some(Decimal { scale: 2 })]
        );
        db.cleanup();
    }

    #[test]
    fn types_with_no_common_type_are_a_bind_error_never_a_silent_cast() {
        let db = typed_db("e2_incompatible");
        // Every non-numeric type combines with itself only.
        let non_numeric = ["txt", "flag", "dt", "tm", "ts", "bl"];
        let others = ["i", "b", "r", "d", "dec2", "txt", "flag", "dt", "tm", "ts", "bl"];
        for left in non_numeric {
            for right in others {
                let sql = format!("SELECT {left} FROM nums UNION SELECT {right} FROM nums");
                let r = db.try_bind(&sql);
                if left == right {
                    assert!(r.is_ok(), "{sql}: {r:?}");
                } else {
                    let err = r.err().unwrap_or_else(|| panic!("{sql} should not bind"));
                    assert!(matches!(err, SqlError::TypeMismatch { .. }), "{sql}: {err:?}");
                    assert!(err.to_string().contains("column 1"), "{sql}: {err}");
                }
            }
        }
        // The contract's example: {1} UNION {'a'}.
        let err = bind_err(&db, "SELECT 1 UNION SELECT 'a'");
        let text = err.to_string();
        assert!(matches!(err, SqlError::TypeMismatch { .. }));
        assert!(text.contains("INTEGER") && text.contains("TEXT"), "{text}");
        // The error names the offending column position.
        let err = bind_err(&db, "SELECT 1, 'a' UNION SELECT 2, 3");
        assert!(err.to_string().contains("column 2"), "{err}");
        // A text column is never an INTEGER's partner, in either order.
        for op in ["UNION", "INTERSECT ALL", "EXCEPT"] {
            let sql = format!("SELECT name FROM t {op} SELECT id FROM t");
            assert!(matches!(bind_err(&db, &sql), SqlError::TypeMismatch { .. }), "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn a_chain_unifies_pairwise_and_one_bad_branch_anywhere_is_an_error() {
        let db = typed_db("e2_chain_types");
        assert_eq!(
            output_types(
                &db,
                "SELECT i FROM nums UNION SELECT b FROM nums UNION SELECT dec4 FROM nums"
            ),
            vec![Some(Decimal { scale: 4 })]
        );
        assert_eq!(
            output_types(
                &db,
                "SELECT i FROM nums INTERSECT SELECT r FROM nums UNION ALL SELECT b FROM nums"
            ),
            vec![Some(Double)]
        );
        for sql in [
            "SELECT i FROM nums UNION SELECT b FROM nums UNION SELECT txt FROM nums",
            "SELECT i FROM nums UNION SELECT txt FROM nums UNION SELECT b FROM nums",
            "SELECT txt FROM nums UNION SELECT i FROM nums UNION SELECT b FROM nums",
        ] {
            assert!(matches!(bind_err(&db, sql), SqlError::TypeMismatch { .. }), "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn an_untyped_null_adopts_the_other_branchs_type() {
        let db = typed_db("e2_null_type");
        assert_eq!(output_types(&db, "SELECT NULL UNION SELECT 1"), vec![Some(Integer)]);
        assert_eq!(output_types(&db, "SELECT 1 UNION SELECT NULL"), vec![Some(Integer)]);
        assert_eq!(
            output_types(&db, "SELECT NULL UNION SELECT 'a' UNION SELECT NULL"),
            vec![Some(Text)]
        );
        // Both untyped: the column stays untyped (and the statement still binds).
        assert_eq!(output_types(&db, "SELECT NULL UNION SELECT NULL"), vec![None]);
        db.cleanup();
    }

    // ---- names and nullability ----------------------------------------------------------

    #[test]
    fn column_names_come_from_the_first_branch() {
        let db = typed_db("e2_names");
        assert_eq!(output_names(&db, "SELECT 1 AS a UNION SELECT 2 AS b"), ["a"]);
        assert_eq!(output_names(&db, "SELECT 2 AS b UNION SELECT 1 AS a"), ["b"]);
        assert_eq!(
            output_names(&db, "SELECT id AS a FROM t UNION SELECT id AS b FROM orders"),
            ["a"]
        );
        // The same column name on both sides.
        assert_eq!(
            output_names(&db, "SELECT id FROM t UNION SELECT id FROM orders"),
            ["id"]
        );
        // A star takes the first table's names.
        assert_eq!(
            output_names(&db, "SELECT * FROM t UNION SELECT * FROM t"),
            ["id", "name", "active"]
        );
        // Every operator, three branches: still the first branch's names.
        for op in ["UNION", "UNION ALL", "INTERSECT", "INTERSECT ALL", "EXCEPT", "EXCEPT ALL"] {
            let sql = format!(
                "SELECT id AS x, name AS y FROM t {op} SELECT id AS p, name AS q FROM t {op} SELECT id, name FROM t"
            );
            assert_eq!(output_names(&db, &sql), ["x", "y"], "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn an_unaliased_expression_is_named_question_column_the_engines_default() {
        let db = typed_db("e2_default_name");
        // The engine's existing default for an expression that is neither a
        // column nor a function (recorded in the matrix as the chosen name).
        assert_eq!(output_names(&db, "SELECT 1 UNION SELECT 2"), ["?column?"]);
        assert_eq!(output_names(&db, "SELECT 1 + 1 UNION SELECT 2"), ["?column?"]);
        // The first branch decides even when the second has a better name.
        assert_eq!(output_names(&db, "SELECT 1 UNION SELECT id FROM t"), ["?column?"]);
        assert_eq!(output_names(&db, "SELECT id FROM t UNION SELECT 1"), ["id"]);
        db.cleanup();
    }

    #[test]
    fn a_columns_nullability_is_true_if_either_branch_can_produce_null() {
        let db = typed_db("e2_nullable");
        let nullable = |sql: &str| -> Vec<bool> {
            bound(&db, sql).projection.iter().map(|i| i.expr.nullable).collect()
        };
        assert_eq!(nullable("SELECT id FROM t UNION SELECT id FROM t"), [false]);
        assert_eq!(nullable("SELECT id FROM t UNION SELECT i FROM nums"), [true]);
        assert_eq!(nullable("SELECT i FROM nums UNION SELECT id FROM t"), [true]);
        assert_eq!(nullable("SELECT 1 UNION SELECT NULL"), [true]);
        assert_eq!(nullable("SELECT 1 UNION SELECT 2"), [false]);
        db.cleanup();
    }

    // ---- ORDER BY ---------------------------------------------------------------------------

    #[test]
    fn order_by_resolves_output_column_names_and_expressions_over_them() {
        let db = typed_db("e2_order_ok");
        for sql in [
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY id",
            "SELECT id AS a FROM t UNION SELECT id FROM orders ORDER BY a DESC",
            "SELECT id AS a, name FROM t UNION SELECT id, customer FROM orders ORDER BY a, name DESC NULLS FIRST",
            "SELECT id AS a FROM t UNION SELECT id FROM orders ORDER BY a + 1",
            "(SELECT id FROM t) UNION (SELECT id FROM orders) ORDER BY id",
            "SELECT id FROM t UNION ALL SELECT id FROM t UNION SELECT id FROM t ORDER BY id LIMIT 3 OFFSET 1",
            "SELECT id FROM t EXCEPT SELECT t_id FROM orders ORDER BY id",
        ] {
            let s = bound(&db, sql);
            assert!(!s.order_by.is_empty(), "{sql}");
            // Every ORDER BY key reads the operation's output relation, never a table.
            let set_ref = s.set_op.as_ref().unwrap().set_ref;
            for item in &s.order_by {
                let mut refs = Vec::new();
                collect_column_refs(&item.expr.kind, &mut refs);
                assert!(!refs.is_empty() || matches!(item.expr.kind, BoundExprKind::Literal(_)));
                assert!(refs.iter().all(|c| c.table_ref.0 == set_ref), "{sql}: {refs:?}");
            }
        }
        db.cleanup();
    }

    fn collect_column_refs(kind: &BoundExprKind, out: &mut Vec<crate::bound::ColumnRef>) {
        match kind {
            BoundExprKind::Column(c) => out.push(*c),
            BoundExprKind::BinaryOp { left, right, .. } => {
                collect_column_refs(&left.kind, out);
                collect_column_refs(&right.kind, out);
            }
            BoundExprKind::UnaryOp { expr, .. } => collect_column_refs(&expr.kind, out),
            _ => {}
        }
    }

    #[test]
    fn order_by_naming_a_table_column_is_a_bind_error() {
        let db = typed_db("e2_order_table_col");
        // `t.name` / `x.b`-style references and a bare name that is not an
        // output column. Both branches expose the same column name `id`.
        for sql in [
            // A column of the first branch's table that the select list omits.
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY name",
            // A column of the second branch's table.
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY customer",
            // Qualified by a branch's table: even a column that IS an output column.
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY t.id",
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY orders.id",
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY t.name",
            // The second branch's alias is not an output name.
            "SELECT id AS a FROM t UNION SELECT id AS b FROM orders ORDER BY b",
            // An expression that reads a non-output column.
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY id + amount",
            // Unknown outright.
            "SELECT id FROM t UNION SELECT id FROM orders ORDER BY nope",
        ] {
            let err = bind_err(&db, sql);
            assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{sql}: {err:?}");
            assert!(
                err.to_string().contains("ORDER BY of a set operation"),
                "{sql}: {err}"
            );
        }
        // Twin: the plain SELECT next to it accepts a non-output column, so the
        // refusal above is about set operations, not about the column.
        bound(&db, "SELECT id FROM t ORDER BY name");
        bound(&db, "SELECT id FROM t ORDER BY t.name");
        db.cleanup();
    }

    #[test]
    fn order_by_on_a_duplicated_output_name_is_ambiguous_and_an_aggregate_or_subquery_is_refused() {
        let db = typed_db("e2_order_misc");
        let err = bind_err(&db, "SELECT id, id FROM t UNION SELECT id, id FROM t ORDER BY id");
        assert!(matches!(err, SqlError::AmbiguousColumn { .. }), "{err:?}");
        // Distinct names are fine.
        bound(&db, "SELECT id AS a, id AS b FROM t UNION SELECT id, id FROM t ORDER BY a, b");
        let err = bind_err(&db, "SELECT id FROM t UNION SELECT id FROM t ORDER BY COUNT(*)");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = bind_err(
            &db,
            "SELECT id FROM t UNION SELECT id FROM t ORDER BY (SELECT 1)",
        );
        assert!(matches!(err, SqlError::Unsupported { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn order_by_on_an_untyped_null_column_is_refused_like_every_other_relation_column() {
        let db = typed_db("e2_order_null");
        // The statement itself binds ...
        bound(&db, "SELECT NULL UNION SELECT NULL");
        // ... but its untyped column cannot be sorted on (nothing to compare).
        let err = bind_err(&db, "SELECT NULL AS n UNION SELECT NULL ORDER BY n");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn limit_and_offset_bind_as_integers_and_count_parameters() {
        let db = typed_db("e2_limit");
        let s = bound(&db, "SELECT id FROM t UNION SELECT id FROM t LIMIT 3 OFFSET 1");
        assert!(s.limit.is_some() && s.offset.is_some());
        let s = bound(&db, "SELECT id FROM t UNION SELECT id FROM t WHERE id = $2 LIMIT $1");
        assert_eq!(s.max_parameter, 2);
        // A non-numeric LIMIT is a bind error, as for any SELECT.
        assert!(matches!(
            bind_err(&db, "SELECT id FROM t UNION SELECT id FROM t LIMIT 'x'"),
            SqlError::TypeMismatch { .. }
        ));
        db.cleanup();
    }

    // ---- authorization ------------------------------------------------------------------------

    fn grant_select(db: &Db, principal: &str, table: &str) {
        use rubixdb::catalog::schema::{ObjectKind, Privilege};
        let id = db.table_id(table);
        db.catalog
            .grant(principal, ObjectKind::Table, id, Privilege::Select)
            .unwrap();
    }

    #[test]
    fn a_forbidden_or_missing_table_in_any_branch_is_unknown_object_with_one_shape() {
        use crate::auth::AuthContext;
        let db = typed_db("e2_authz");
        grant_select(&db, "limited", "t");
        let auth = AuthContext::none("limited");
        let limits = SqlLimits::default();
        // The forbidden table is branch 2 of 4.
        let forbidden = db
            .try_bind_with(
                "SELECT id FROM t UNION SELECT id FROM nums UNION SELECT id FROM t UNION SELECT id FROM t",
                &auth,
                &limits,
            )
            .unwrap_err();
        // A table that does not exist, in the same position.
        let missing = db
            .try_bind_with(
                "SELECT id FROM t UNION SELECT id FROM no_such UNION SELECT id FROM t UNION SELECT id FROM t",
                &auth,
                &limits,
            )
            .unwrap_err();
        assert!(matches!(forbidden, SqlError::UnknownObject { kind: "table", .. }), "{forbidden:?}");
        assert!(matches!(missing, SqlError::UnknownObject { kind: "table", .. }), "{missing:?}");
        // Same message shape (only the object name differs).
        assert_eq!(
            forbidden.to_string().replace("nums", "X"),
            missing.to_string().replace("no_such", "X")
        );
        // Every operator, the forbidden table on either side and nested in a
        // subquery, a derived table and a CTE.
        for op in ["UNION", "UNION ALL", "INTERSECT", "INTERSECT ALL", "EXCEPT", "EXCEPT ALL"] {
            for sql in [
                format!("SELECT id FROM t {op} SELECT id FROM nums"),
                format!("SELECT id FROM nums {op} SELECT id FROM t"),
                format!("SELECT id FROM t WHERE id IN (SELECT id FROM t {op} SELECT id FROM nums)"),
                format!("SELECT u.id FROM (SELECT id FROM t {op} SELECT id FROM nums) AS u"),
                format!("WITH u AS (SELECT id FROM t {op} SELECT id FROM nums) SELECT id FROM t"),
            ] {
                let err = db.try_bind_with(&sql, &auth, &limits).unwrap_err();
                assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{sql}: {err:?}");
            }
        }
        // Once granted, the same statement binds.
        grant_select(&db, "limited", "nums");
        assert!(db
            .try_bind_with("SELECT id FROM t UNION SELECT id FROM nums", &auth, &limits)
            .is_ok());
        db.cleanup();
    }

    // ---- contexts -----------------------------------------------------------------------------

    #[test]
    fn a_with_list_in_front_of_a_set_operation_is_visible_in_every_operand() {
        let db = typed_db("e2_with");
        let s = bound(
            &db,
            "WITH c AS (SELECT id AS v FROM t) SELECT v FROM c UNION ALL SELECT v FROM c",
        );
        assert_eq!(s.ctes.len(), 1);
        assert_eq!(s.projection.len(), 1);
        // An operand's own WITH shadows the inherited one (same name, same type: binds).
        bound(
            &db,
            "WITH c AS (SELECT id AS v FROM t) SELECT v FROM c UNION ALL (WITH c AS (SELECT id + 1 AS v FROM t) SELECT v FROM c)",
        );
        let err = db
            .try_bind(
                "WITH c AS (SELECT id AS v FROM t) SELECT v FROM c UNION ALL (WITH c AS (SELECT name AS v FROM t) SELECT v FROM c) ",
            )
            .err();
        // INTEGER (outer c) vs TEXT (inner c shadowing it) cannot combine.
        assert!(matches!(err, Some(SqlError::TypeMismatch { .. })), "{err:?}");
        // A CTE defined only inside one operand is not visible in the other.
        let err = bind_err(
            &db,
            "(WITH c AS (SELECT id AS v FROM t) SELECT v FROM c) UNION ALL SELECT v FROM c",
        );
        assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_set_operation_in_a_scalar_in_or_exists_subquery_binds_with_the_unified_type() {
        let db = typed_db("e2_subq");
        // Scalar: one column, the unified type, always nullable.
        let s = bound(
            &db,
            "SELECT (SELECT i FROM nums UNION SELECT b FROM nums) FROM t",
        );
        assert_eq!(s.projection[0].expr.ty, Some(Bigint));
        assert!(s.projection[0].expr.nullable);
        // A scalar subquery must be one column wide.
        let err = bind_err(&db, "SELECT (SELECT id, name FROM t UNION SELECT id, name FROM t) FROM t");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // IN: exactly one column; the operand must have the unified type.
        bound(&db, "SELECT id FROM t WHERE id IN (SELECT id FROM t UNION SELECT i FROM nums)");
        let err = bind_err(
            &db,
            "SELECT id FROM t WHERE id IN (SELECT id FROM t UNION SELECT b FROM nums)",
        );
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        let err = bind_err(
            &db,
            "SELECT id FROM t WHERE id IN (SELECT id, id FROM t UNION SELECT id, id FROM t)",
        );
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        // EXISTS accepts any width.
        bound(&db, "SELECT id FROM t WHERE EXISTS (SELECT id, name FROM t EXCEPT SELECT id, name FROM t)");
        db.cleanup();
    }

    #[test]
    fn a_set_operation_in_a_derived_table_or_cte_exposes_the_unified_columns() {
        let db = typed_db("e2_rel");
        let s = bound(
            &db,
            "SELECT u.v FROM ((SELECT id AS v FROM t) UNION (SELECT b FROM nums)) AS u",
        );
        assert_eq!(s.projection[0].expr.ty, Some(Bigint));
        assert_eq!(s.projection[0].output_name, "v");
        // A column-name list renames the columns; the count must match.
        let s = bound(&db, "SELECT x FROM (SELECT id FROM t UNION SELECT id FROM t) AS u(x)");
        assert_eq!(s.projection[0].output_name, "x");
        let err = bind_err(&db, "SELECT x FROM (SELECT id FROM t UNION SELECT id FROM t) AS u(x, y)");
        assert!(matches!(err, SqlError::InvalidIdentifier { .. }), "{err:?}");
        // The prompt's CTE separator shape.
        let s = bound(&db, "WITH cte AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM cte");
        assert_eq!(s.projection.len(), 1);
        assert_eq!(s.projection[0].expr.ty, Some(Integer));
        // A CTE with a column list over a set operation.
        let s = bound(&db, "WITH c(p) AS (SELECT id FROM t UNION SELECT id FROM t) SELECT p FROM c");
        assert_eq!(s.projection[0].output_name, "p");
        // The names of the first branch are the CTE's columns.
        bound(&db, "WITH c AS (SELECT id AS a FROM t UNION SELECT id AS b FROM t) SELECT a FROM c");
        let err = bind_err(
            &db,
            "WITH c AS (SELECT id AS a FROM t UNION SELECT id AS b FROM t) SELECT b FROM c",
        );
        assert!(matches!(err, SqlError::UnknownObject { kind: "column", .. }), "{err:?}");
        // Two columns with the same name: referring to the name is ambiguous.
        let err = bind_err(
            &db,
            "SELECT id FROM (SELECT id, id FROM t UNION SELECT id, id FROM t) AS u",
        );
        assert!(matches!(err, SqlError::AmbiguousColumn { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn a_set_operation_binds_in_update_and_delete_where_and_set() {
        let db = typed_db("e2_dml");
        for sql in [
            "DELETE FROM t WHERE id IN (SELECT t_id FROM orders UNION SELECT id FROM t)",
            "UPDATE t SET name = 'x' WHERE id IN (SELECT t_id FROM orders INTERSECT SELECT id FROM t)",
            "UPDATE t SET name = (SELECT customer FROM orders UNION SELECT name FROM t) WHERE id = 1",
            "DELETE FROM t WHERE EXISTS (SELECT 1 EXCEPT SELECT 2)",
        ] {
            db.try_bind(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let err = bind_err(&db, "DELETE FROM t WHERE id IN (SELECT t_id, id FROM orders UNION SELECT id, id FROM t)");
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn an_operand_may_read_the_enclosing_query_and_is_then_correlated() {
        let db = typed_db("e2_corr");
        let s = bound(
            &db,
            "SELECT t.id, (SELECT amount FROM orders WHERE t_id = t.id UNION SELECT 0) FROM t",
        );
        let BoundExprKind::Subquery(sq) = &s.projection[1].expr.kind else { panic!() };
        assert!(!sq.outer_refs.is_empty(), "the operand reads t.id");
        assert!(sq.query.set_op.is_some());
        // Uncorrelated twin.
        let s = bound(&db, "SELECT t.id, (SELECT amount FROM orders UNION SELECT 0) FROM t");
        let BoundExprKind::Subquery(sq) = &s.projection[1].expr.kind else { panic!() };
        assert!(sq.outer_refs.is_empty());
        db.cleanup();
    }

    #[test]
    fn every_operand_owns_its_scope_so_a_name_in_one_is_not_visible_in_the_other() {
        let db = typed_db("e2_scope");
        // `amount` belongs to orders only; the second operand cannot see it.
        let err = bind_err(&db, "SELECT amount FROM orders UNION SELECT amount FROM t");
        assert!(matches!(err, SqlError::UnknownObject { kind: "column", .. }), "{err:?}");
        // The same alias in both operands is fine (no clash).
        bound(&db, "SELECT x.id FROM t AS x UNION SELECT x.id FROM orders AS x");
        db.cleanup();
    }

    #[test]
    fn a_long_chain_binds_without_native_recursion_and_the_tree_is_left_nested() {
        let db = typed_db("e2_chain");
        let sql = (0..100).map(|_| "SELECT id FROM t").collect::<Vec<_>>().join(" UNION ALL ");
        let s = bound(&db, &sql);
        let mut depth = 0;
        let mut cur = &*s;
        while let Some(set) = &cur.set_op {
            depth += 1;
            cur = &set.left;
        }
        assert_eq!(depth, 99);
        db.cleanup();
    }
}

// =======================================================================
// Shared execution fixtures and the independent reference model
// =======================================================================

/// One result row as plain integers (`None` = SQL NULL).
pub(crate) type Tup = Vec<Option<i64>>;

pub(crate) fn tups(rows: &[Row]) -> Vec<Tup> {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    None => None,
                    Some(RelationalValue::Integer(i)) => Some(i64::from(*i)),
                    Some(RelationalValue::Bigint(i)) => Some(*i),
                    other => panic!("expected an integer column, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

/// Rows in a canonical order (NULL first, then numeric), for comparing results
/// whose order is unspecified.
pub(crate) fn sorted(mut rows: Vec<Tup>) -> Vec<Tup> {
    rows.sort();
    rows
}

/// Single-column rows.
pub(crate) fn col(values: &[Option<i64>]) -> Vec<Tup> {
    values.iter().map(|v| vec![*v]).collect()
}

/// The six operators over multisets of tuples, computed the slow obvious way:
/// counting occurrences with `==` on `Vec<Option<i64>>` (where `None == None`,
/// the "not distinct" reading) and no hashing. This is the reference every
/// execution test compares against; it shares no code with the engine.
pub(crate) fn model(op: &str, all: bool, left: &[Tup], right: &[Tup]) -> Vec<Tup> {
    let count = |rows: &[Tup], t: &Tup| rows.iter().filter(|x| *x == t).count();
    let distinct = |rows: &[Tup]| -> Vec<Tup> {
        let mut out: Vec<Tup> = Vec::new();
        for x in rows {
            if !out.contains(x) {
                out.push(x.clone());
            }
        }
        out
    };
    let copies = |t: &Tup, n: usize| -> Vec<Tup> { (0..n).map(|_| t.clone()).collect() };
    match (op, all) {
        ("UNION", true) => left.iter().chain(right).cloned().collect(),
        ("UNION", false) => distinct(&left.iter().chain(right).cloned().collect::<Vec<_>>()),
        ("INTERSECT", false) => distinct(left)
            .into_iter()
            .filter(|t| count(right, t) > 0)
            .collect(),
        ("INTERSECT", true) => distinct(left)
            .iter()
            .flat_map(|t| copies(t, count(left, t).min(count(right, t))))
            .collect(),
        ("EXCEPT", false) => distinct(left)
            .into_iter()
            .filter(|t| count(right, t) == 0)
            .collect(),
        ("EXCEPT", true) => distinct(left)
            .iter()
            .flat_map(|t| copies(t, count(left, t).saturating_sub(count(right, t))))
            .collect(),
        other => panic!("{other:?}"),
    }
}

pub(crate) const OPERATORS: [(&str, bool); 6] = [
    ("UNION", false),
    ("UNION", true),
    ("INTERSECT", false),
    ("INTERSECT", true),
    ("EXCEPT", false),
    ("EXCEPT", true),
];

pub(crate) fn op_sql(op: &str, all: bool) -> String {
    format!("{op}{}", if all { " ALL" } else { "" })
}

/// `a`, `b`, `c`, `e` (`id INTEGER PRIMARY KEY, v INTEGER`) and `p`, `q`
/// (`id INTEGER PRIMARY KEY, x INTEGER, y INTEGER`), created empty.
pub(crate) fn rel_db(tag: &str) -> Db {
    let db = Db::new(tag);
    for t in ["a", "b", "c", "e"] {
        db.write(&format!("CREATE TABLE {t} (id INTEGER PRIMARY KEY, v INTEGER)"));
    }
    for t in ["p", "q"] {
        db.write(&format!(
            "CREATE TABLE {t} (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)"
        ));
    }
    db
}

pub(crate) fn sql_int(v: &Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "NULL".to_string())
}

/// Replaces the rows of the one-column table `name` with `values` (ids 1..).
pub(crate) fn load(db: &Db, name: &str, values: &[Option<i64>]) {
    for (i, v) in values.iter().enumerate() {
        db.write(&format!("INSERT INTO {name} (id, v) VALUES ({}, {})", i + 1, sql_int(v)));
    }
}

/// Loads a two-column table `name` (ids 1..).
pub(crate) fn load2(db: &Db, name: &str, rows: &[(Option<i64>, Option<i64>)]) {
    for (i, (x, y)) in rows.iter().enumerate() {
        db.write(&format!(
            "INSERT INTO {name} (id, x, y) VALUES ({}, {}, {})",
            i + 1,
            sql_int(x),
            sql_int(y)
        ));
    }
}

pub(crate) fn some(values: &[i64]) -> Vec<Option<i64>> {
    values.iter().map(|v| Some(*v)).collect()
}

/// Rows of `sql` as plain integers, in the order the engine returned them.
pub(crate) fn run_tups(db: &Db, sql: &str) -> Vec<Tup> {
    tups(&db.rows(sql))
}

// =======================================================================
// E3 -- UNION ALL execution (concatenation, no deduplication)
// =======================================================================

mod union_all {
    use super::*;
    use crate::exec::{ExecLimits, ExecMetrics};

    #[test]
    fn it_returns_every_row_of_both_sides_without_deduplicating() {
        let db = rel_db("e3_basic");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1, 1, 1]));
        let rows = run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b");
        assert_eq!(sorted(rows), sorted(col(&some(&[1, 1, 2, 1, 1, 1]))));
        db.cleanup();
    }

    #[test]
    fn the_left_rows_come_first_then_the_right_rows() {
        let db = rel_db("e3_order");
        load(&db, "a", &some(&[5, 4, 3]));
        load(&db, "b", &some(&[9, 8]));
        // A table scan returns its rows in primary-key order, and UNION ALL
        // is a concatenation: exactly this sequence.
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b"),
            col(&some(&[5, 4, 3, 9, 8]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM b UNION ALL SELECT v FROM a"),
            col(&some(&[9, 8, 5, 4, 3]))
        );
        db.cleanup();
    }

    #[test]
    fn two_identical_relations_give_twice_the_rows() {
        let db = rel_db("e3_identical");
        load(&db, "a", &[Some(1), Some(1), Some(2), None]);
        load(&db, "b", &[Some(1), Some(1), Some(2), None]);
        let rows = run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b");
        assert_eq!(rows.len(), 8);
        assert_eq!(
            sorted(rows),
            sorted(col(&[Some(1), Some(1), Some(2), None, Some(1), Some(1), Some(2), None]))
        );
        // The very same table on both sides.
        assert_eq!(run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM a").len(), 8);
        db.cleanup();
    }

    #[test]
    fn disjoint_relations_give_the_sum() {
        let db = rel_db("e3_disjoint");
        load(&db, "a", &some(&[1, 2, 3]));
        load(&db, "b", &some(&[10, 20]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b")),
            sorted(col(&some(&[1, 2, 3, 10, 20])))
        );
        db.cleanup();
    }

    #[test]
    fn an_empty_branch_in_any_position_changes_nothing() {
        let db = rel_db("e3_empty");
        load(&db, "a", &some(&[1, 2]));
        load(&db, "b", &some(&[3]));
        // `c` and `e` are empty.
        for (sql, expected) in [
            ("SELECT v FROM a UNION ALL SELECT v FROM c", some(&[1, 2])),
            ("SELECT v FROM c UNION ALL SELECT v FROM a", some(&[1, 2])),
            ("SELECT v FROM c UNION ALL SELECT v FROM e", some(&[])),
            ("SELECT v FROM a UNION ALL SELECT v FROM c UNION ALL SELECT v FROM b", some(&[1, 2, 3])),
            ("SELECT v FROM c UNION ALL SELECT v FROM a UNION ALL SELECT v FROM b", some(&[1, 2, 3])),
            ("SELECT v FROM a UNION ALL SELECT v FROM b UNION ALL SELECT v FROM c", some(&[1, 2, 3])),
        ] {
            assert_eq!(sorted(run_tups(&db, sql)), sorted(col(&expected)), "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn limit_applies_to_the_concatenation_less_than_equal_and_greater_than_the_total() {
        let db = rel_db("e3_limit");
        load(&db, "a", &some(&[1, 2, 3]));
        load(&db, "b", &some(&[4, 5]));
        let q = |limit: &str| {
            run_tups(&db, &format!("(SELECT v FROM a) UNION ALL (SELECT v FROM b) LIMIT {limit}"))
        };
        assert_eq!(q("2"), col(&some(&[1, 2])), "less than the left side");
        assert_eq!(q("4"), col(&some(&[1, 2, 3, 4])), "across the boundary");
        assert_eq!(q("5"), col(&some(&[1, 2, 3, 4, 5])), "equal to the total");
        assert_eq!(q("10"), col(&some(&[1, 2, 3, 4, 5])), "greater than the total");
        assert_eq!(q("0"), Vec::<Tup>::new());
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b LIMIT 2 OFFSET 2"),
            col(&some(&[3, 4]))
        );
        db.cleanup();
    }

    #[test]
    fn order_by_and_limit_apply_to_the_whole_result() {
        let db = rel_db("e3_order_limit");
        load(&db, "a", &some(&[3, 1]));
        load(&db, "b", &some(&[4, 2]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b ORDER BY v"),
            col(&some(&[1, 2, 3, 4]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b ORDER BY v DESC LIMIT 2"),
            col(&some(&[4, 3]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b ORDER BY v LIMIT 2 OFFSET 1"),
            col(&some(&[2, 3]))
        );
        // ORDER BY an expression over the output column, and NULL placement.
        db.write("INSERT INTO c (id, v) VALUES (1, NULL)");
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM c ORDER BY v"),
            vec![vec![Some(1)], vec![Some(3)], vec![None]],
            "NULLS LAST ascending"
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM c ORDER BY v DESC"),
            vec![vec![None], vec![Some(3)], vec![Some(1)]],
            "NULLS FIRST descending"
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b ORDER BY 0 - v"),
            col(&some(&[4, 3, 2, 1]))
        );
        db.cleanup();
    }

    #[test]
    fn a_parenthesised_operand_keeps_its_own_order_by_and_limit() {
        let db = rel_db("e3_operand_limit");
        load(&db, "a", &some(&[1, 2, 3]));
        load(&db, "b", &some(&[7, 8, 9]));
        assert_eq!(
            run_tups(
                &db,
                "(SELECT v FROM a ORDER BY v DESC LIMIT 2) UNION ALL (SELECT v FROM b ORDER BY v LIMIT 1)"
            ),
            col(&some(&[3, 2, 7]))
        );
        db.cleanup();
    }

    #[test]
    fn three_or_more_branches_concatenate_in_order() {
        let db = rel_db("e3_three");
        load(&db, "a", &some(&[1]));
        load(&db, "b", &some(&[2]));
        load(&db, "c", &some(&[3]));
        load(&db, "e", &some(&[4]));
        assert_eq!(
            run_tups(
                &db,
                "SELECT v FROM a UNION ALL SELECT v FROM b UNION ALL SELECT v FROM c UNION ALL SELECT v FROM e"
            ),
            col(&some(&[1, 2, 3, 4]))
        );
        // Right-nested parentheses concatenate the same rows.
        assert_eq!(
            run_tups(
                &db,
                "SELECT v FROM a UNION ALL (SELECT v FROM b UNION ALL (SELECT v FROM c UNION ALL SELECT v FROM e))"
            ),
            col(&some(&[1, 2, 3, 4]))
        );
        db.cleanup();
    }

    #[test]
    fn nulls_are_kept_and_never_merged() {
        let db = rel_db("e3_nulls");
        load2(&db, "p", &[(Some(1), None), (None, None)]);
        load2(&db, "q", &[(Some(1), None), (None, None), (Some(1), Some(0))]);
        let rows = run_tups(&db, "SELECT x, y FROM p UNION ALL SELECT x, y FROM q");
        assert_eq!(rows.len(), 5);
        assert_eq!(
            sorted(rows),
            sorted(vec![
                vec![Some(1), None],
                vec![None, None],
                vec![Some(1), None],
                vec![None, None],
                vec![Some(1), Some(0)],
            ])
        );
        db.cleanup();
    }

    #[test]
    fn the_output_schema_has_the_first_branchs_names_and_the_unified_types() {
        let db = typed_db("e3_schema");
        db.write("INSERT INTO nums (id, i, b) VALUES (1, 7, 9000000000)");
        let r = db.run("SELECT i AS first_name FROM nums UNION ALL SELECT b AS ignored FROM nums");
        let fields: Vec<(String, Option<RelationalType>)> =
            r.schema.fields.iter().map(|f| (f.name.clone(), f.ty)).collect();
        assert_eq!(fields, vec![("first_name".to_string(), Some(RelationalType::Bigint))]);
        // The INTEGER value was widened: both rows are BIGINT.
        assert_eq!(
            r.rows,
            vec![vec![Some(RelationalValue::Bigint(7))], vec![Some(RelationalValue::Bigint(9_000_000_000))]]
        );
        db.cleanup();
    }

    #[test]
    fn numeric_values_are_widened_to_the_unified_column_type() {
        let db = typed_db("e3_widen_values");
        db.write(
            "INSERT INTO nums (id, i, b, r, d, dec2, dec4) VALUES (1, 7, 9000000000, 1.5, 2.25, 3.10, 4.1234)",
        );
        let one = |sql: &str| -> Vec<Option<RelationalValue>> {
            let mut rows = db.rows(sql);
            rows.sort_by_key(|r| format!("{r:?}"));
            rows.into_iter().map(|mut r| r.remove(0)).collect()
        };
        use RelationalValue::*;
        // INTEGER + DECIMAL(2) -> DECIMAL(2): 7 becomes 7.00.
        assert_eq!(
            one("SELECT i FROM nums UNION ALL SELECT dec2 FROM nums"),
            vec![Some(Decimal(310, 2)), Some(Decimal(700, 2))]
        );
        // DECIMAL(2) + DECIMAL(4) -> DECIMAL(4): 3.10 becomes 3.1000.
        assert_eq!(
            one("SELECT dec2 FROM nums UNION ALL SELECT dec4 FROM nums"),
            vec![Some(Decimal(31000, 4)), Some(Decimal(41234, 4))]
        );
        // INTEGER + DOUBLE -> DOUBLE, REAL + INTEGER -> DOUBLE.
        assert_eq!(
            one("SELECT i FROM nums UNION ALL SELECT d FROM nums"),
            vec![Some(Double(2.25)), Some(Double(7.0))]
        );
        assert_eq!(
            one("SELECT r FROM nums UNION ALL SELECT i FROM nums"),
            vec![Some(Double(1.5)), Some(Double(7.0))]
        );
        // BIGINT + DECIMAL(4) -> DECIMAL(4).
        assert_eq!(
            one("SELECT b FROM nums UNION ALL SELECT dec4 FROM nums"),
            vec![Some(Decimal(41234, 4)), Some(Decimal(90_000_000_000_000, 4))]
        );
        // A widening that cannot be represented is an error, never a wrapped value.
        db.write("INSERT INTO nums (id, b, dec4) VALUES (2, 9223372036854775807, 0.0001)");
        db.write("CREATE TABLE wide (id INTEGER PRIMARY KEY, w DECIMAL(38, 30))");
        db.write("INSERT INTO wide (id, w) VALUES (1, 1.5)");
        let err = db
            .try_run("SELECT b FROM nums UNION ALL SELECT w FROM wide")
            .unwrap_err();
        assert!(matches!(err, crate::error::SqlError::ExecutionParameter { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn it_works_over_aggregates_derived_tables_and_joins_in_its_operands() {
        let db = rel_db("e3_operand_kinds");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[5]));
        assert_eq!(
            run_tups(
                &db,
                "SELECT COUNT(*) FROM a UNION ALL SELECT COUNT(*) FROM b UNION ALL SELECT SUM(v) FROM a"
            ),
            vec![vec![Some(3)], vec![Some(1)], vec![Some(4)]]
        );
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT d.v FROM (SELECT v FROM a WHERE v > 1) AS d UNION ALL SELECT a.v FROM a JOIN b ON a.id = b.id"
            )),
            sorted(col(&some(&[2, 1])))
        );
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT v, COUNT(*) FROM a GROUP BY v UNION ALL SELECT v, 0 FROM b"
            )),
            sorted(vec![vec![Some(1), Some(2)], vec![Some(2), Some(1)], vec![Some(5), Some(0)]])
        );
        db.cleanup();
    }

    #[test]
    fn it_streams_so_a_limit_stops_the_scan_early() {
        let db = rel_db("e3_stream");
        let many: Vec<Option<i64>> = (0..500).map(Some).collect();
        load(&db, "a", &many);
        load(&db, "b", &many);
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with(
                "SELECT v FROM a UNION ALL SELECT v FROM b LIMIT 3",
                &ExecLimits::default(),
                &metrics,
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 3);
        let scanned = metrics.snapshot().rows_scanned;
        assert!(scanned < 50, "a LIMIT 3 over UNION ALL must not read 1000 rows (read {scanned})");
        // The right operand is never even opened.
        let metrics = ExecMetrics::default();
        db.try_run_with(
            "SELECT v FROM a UNION ALL SELECT v FROM b LIMIT 2",
            &ExecLimits::default(),
            &metrics,
            &[],
        )
        .unwrap();
        assert_eq!(metrics.snapshot().seq_scans + metrics.snapshot().index_scans + metrics.snapshot().pk_range_scans, 1);
        db.cleanup();
    }

    #[test]
    fn union_all_removes_no_rows_so_the_dedup_counter_stays_zero_and_the_branches_are_counted() {
        let db = rel_db("e3_metrics");
        load(&db, "a", &some(&[1, 1]));
        load(&db, "b", &some(&[1]));
        let metrics = ExecMetrics::default();
        db.try_run_with(
            "SELECT v FROM a UNION ALL SELECT v FROM b UNION ALL SELECT v FROM a",
            &ExecLimits::default(),
            &metrics,
            &[],
        )
        .unwrap();
        let m = metrics.snapshot();
        assert_eq!(m.set_op_rows_deduplicated_total, 0);
        assert_eq!(m.set_op_branches_total, 3);
        db.cleanup();
    }

    #[test]
    fn explain_shows_a_set_op_node_with_its_operator_and_branch_count() {
        let db = rel_db("e3_explain");
        let text = crate::plan::explain(&db.plan("SELECT v FROM a UNION ALL SELECT v FROM b UNION ALL SELECT v FROM c"));
        assert!(
            text.contains("SetOp UNION ALL branches=3 shape=UNION ALL(#1, #2, #3)"),
            "{text}"
        );
        // One node, three operand plans under it.
        assert_eq!(text.matches("SetOp").count(), 1, "{text}");
        assert_eq!(text.matches("Projection").count(), 3, "{text}");
        db.cleanup();
    }
}

// =======================================================================
// The tuple-distinctness predicate (the null-safe comparison used to
// deduplicate rows) -- unit tests, independent of any SQL
// =======================================================================

mod distinctness {
    use super::*;
    use crate::aggregate::GroupingKey;
    use crate::exec::setop::{rows_not_distinct, values_not_distinct, SetKey};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    type V = Option<RelationalValue>;

    fn int(n: i32) -> V {
        Some(RelationalValue::Integer(n))
    }
    fn big(n: i64) -> V {
        Some(RelationalValue::Bigint(n))
    }
    fn txt(s: &str) -> V {
        Some(RelationalValue::Text(s.to_string()))
    }

    #[test]
    fn two_nulls_are_not_distinct_and_a_null_and_a_value_are() {
        assert!(values_not_distinct(None, None));
        assert!(!values_not_distinct(None, Some(&RelationalValue::Integer(0))));
        assert!(!values_not_distinct(Some(&RelationalValue::Integer(0)), None));
        assert!(!values_not_distinct(None, Some(&RelationalValue::Text(String::new()))));
    }

    #[test]
    fn rows_with_nulls_compare_by_the_not_distinct_rule() {
        // {1, NULL} and {1, NULL}: the same row.
        assert!(rows_not_distinct(&[int(1), None], &[int(1), None]));
        // {1, NULL} and {1, 0}: different rows.
        assert!(!rows_not_distinct(&[int(1), None], &[int(1), int(0)]));
        assert!(!rows_not_distinct(&[int(1), int(0)], &[int(1), None]));
        // {NULL, NULL} and {NULL, NULL}: the same row.
        assert!(rows_not_distinct(&[None, None], &[None, None]));
        // NULL in different positions: different rows.
        assert!(!rows_not_distinct(&[None, int(1)], &[int(1), None]));
    }

    #[test]
    fn the_empty_tuple_is_not_distinct_from_the_empty_tuple_and_lengths_must_match() {
        assert!(rows_not_distinct(&[], &[]));
        assert!(!rows_not_distinct(&[], &[None]));
        assert!(!rows_not_distinct(&[None], &[]));
        assert!(!rows_not_distinct(&[int(1)], &[int(1), int(1)]));
    }

    #[test]
    fn mixed_type_tuples_compare_column_by_column() {
        let a = [int(1), txt("x"), None, Some(RelationalValue::Boolean(true))];
        let b = [int(1), txt("x"), None, Some(RelationalValue::Boolean(true))];
        assert!(rows_not_distinct(&a, &b));
        let c = [int(1), txt("X"), None, Some(RelationalValue::Boolean(true))];
        assert!(!rows_not_distinct(&a, &c), "text is case sensitive");
        // A value of another type in the same column is simply different.
        assert!(!rows_not_distinct(&[int(1)], &[big(1)]));
        assert!(!rows_not_distinct(&[int(1)], &[txt("1")]));
        assert!(!rows_not_distinct(&[Some(RelationalValue::Boolean(false))], &[int(0)]));
    }

    #[test]
    fn floating_point_zero_signs_and_nans_follow_the_grouping_rule() {
        use RelationalValue::*;
        assert!(values_not_distinct(Some(&Double(0.0)), Some(&Double(-0.0))));
        assert!(values_not_distinct(Some(&Real(0.0)), Some(&Real(-0.0))));
        assert!(values_not_distinct(Some(&Double(f64::NAN)), Some(&Double(f64::NAN))));
        assert!(values_not_distinct(Some(&Real(f32::NAN)), Some(&Real(f32::NAN))));
        assert!(!values_not_distinct(Some(&Double(f64::NAN)), Some(&Double(0.0))));
        assert!(!values_not_distinct(Some(&Double(1.0)), Some(&Double(1.0 + f64::EPSILON))));
        assert!(values_not_distinct(Some(&Double(f64::INFINITY)), Some(&Double(f64::INFINITY))));
        assert!(!values_not_distinct(Some(&Double(f64::INFINITY)), Some(&Double(f64::NEG_INFINITY))));
        // A decimal compares by value and scale.
        assert!(values_not_distinct(Some(&Decimal(150, 2)), Some(&Decimal(150, 2))));
        assert!(!values_not_distinct(Some(&Decimal(150, 2)), Some(&Decimal(15, 1))));
    }

    fn pool() -> Vec<V> {
        use RelationalValue::*;
        vec![
            None,
            Some(Boolean(true)),
            Some(Boolean(false)),
            Some(Integer(0)),
            Some(Integer(1)),
            Some(Bigint(0)),
            Some(Bigint(1)),
            Some(Real(0.0)),
            Some(Real(-0.0)),
            Some(Double(0.0)),
            Some(Double(-0.0)),
            Some(Double(f64::NAN)),
            Some(Double(1.5)),
            Some(Decimal(100, 2)),
            Some(Decimal(1, 0)),
            Some(Text(String::new())),
            Some(Text("a".into())),
            Some(Text("A".into())),
            Some(Blob(vec![])),
            Some(Blob(vec![0])),
            Some(Date(1)),
            Some(Time(1)),
            Some(Timestamp(1)),
        ]
    }

    fn hash_of(k: &SetKey) -> u64 {
        let mut h = DefaultHasher::new();
        k.hash(&mut h);
        h.finish()
    }

    #[test]
    fn the_hash_key_agrees_with_the_predicate_and_with_the_grouping_key_on_every_pair_of_the_pool() {
        let pool = pool();
        // Single-column rows and two-column rows built from the pool.
        let mut rows: Vec<Vec<V>> = pool.iter().map(|v| vec![v.clone()]).collect();
        rows.push(vec![]);
        for a in &pool {
            for b in pool.iter().step_by(3) {
                rows.push(vec![a.clone(), b.clone()]);
            }
        }
        for x in &rows {
            for y in &rows {
                let predicate = rows_not_distinct(x, y);
                let (kx, ky) = (SetKey(x.clone()), SetKey(y.clone()));
                assert_eq!(kx == ky, predicate, "{x:?} vs {y:?}");
                assert_eq!(
                    GroupingKey::from_values(x) == GroupingKey::from_values(y),
                    predicate,
                    "GROUP BY's key must agree: {x:?} vs {y:?}"
                );
                if predicate {
                    assert_eq!(hash_of(&kx), hash_of(&ky), "equal rows must hash alike: {x:?} vs {y:?}");
                }
            }
        }
    }

    #[test]
    fn the_predicate_is_reflexive_symmetric_and_transitive_on_the_pool() {
        let pool = pool();
        for a in &pool {
            assert!(values_not_distinct(a.as_ref(), a.as_ref()), "reflexive {a:?}");
            for b in &pool {
                assert_eq!(
                    values_not_distinct(a.as_ref(), b.as_ref()),
                    values_not_distinct(b.as_ref(), a.as_ref()),
                    "symmetric {a:?} {b:?}"
                );
                for c in &pool {
                    if values_not_distinct(a.as_ref(), b.as_ref()) && values_not_distinct(b.as_ref(), c.as_ref()) {
                        assert!(values_not_distinct(a.as_ref(), c.as_ref()), "transitive {a:?} {b:?} {c:?}");
                    }
                }
            }
        }
    }
}

// =======================================================================
// E4 -- UNION execution (concatenation + null-safe distinct)
// =======================================================================

mod union {
    use super::*;
    use crate::error::SqlError;
    use crate::exec::{ExecLimits, ExecMetrics};

    #[test]
    fn it_returns_the_distinct_rows_of_both_sides() {
        let db = rel_db("e4_basic");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1, 1, 1]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b")),
            col(&some(&[1, 2]))
        );
        // UNION DISTINCT is UNION.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION DISTINCT SELECT v FROM b")),
            col(&some(&[1, 2]))
        );
        db.cleanup();
    }

    #[test]
    fn rows_come_out_in_the_order_of_their_first_occurrence() {
        let db = rel_db("e4_order");
        load(&db, "a", &some(&[3, 1, 3]));
        load(&db, "b", &some(&[2, 1, 4]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b"),
            col(&some(&[3, 1, 2, 4]))
        );
        db.cleanup();
    }

    #[test]
    fn nulls_are_one_value_for_deduplication_in_one_and_two_columns() {
        let db = rel_db("e4_nulls");
        // {1, NULL} vs {1, NULL}: one row.
        load2(&db, "p", &[(Some(1), None)]);
        load2(&db, "q", &[(Some(1), None)]);
        assert_eq!(
            run_tups(&db, "SELECT x, y FROM p UNION SELECT x, y FROM q"),
            vec![vec![Some(1), None]]
        );
        db.cleanup();

        // {1, NULL} vs {1, 0}: two rows.
        let db = rel_db("e4_nulls2");
        load2(&db, "p", &[(Some(1), None)]);
        load2(&db, "q", &[(Some(1), Some(0))]);
        assert_eq!(
            sorted(run_tups(&db, "SELECT x, y FROM p UNION SELECT x, y FROM q")),
            sorted(vec![vec![Some(1), None], vec![Some(1), Some(0)]])
        );
        db.cleanup();

        // {NULL, NULL} vs {NULL, NULL}: one row; and duplicates inside one side too.
        let db = rel_db("e4_nulls3");
        load2(&db, "p", &[(None, None), (None, None)]);
        load2(&db, "q", &[(None, None)]);
        assert_eq!(
            run_tups(&db, "SELECT x, y FROM p UNION SELECT x, y FROM q"),
            vec![vec![None, None]]
        );
        // A single NULL column.
        load(&db, "a", &[None, None, Some(1)]);
        load(&db, "b", &[None]);
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b")),
            sorted(vec![vec![None], vec![Some(1)]])
        );
        db.cleanup();
    }

    #[test]
    fn two_identical_relations_give_the_relation_once_each_row() {
        let db = rel_db("e4_identical");
        load(&db, "a", &[Some(1), Some(2), None]);
        load(&db, "b", &[Some(1), Some(2), None]);
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b")),
            sorted(col(&[Some(1), Some(2), None]))
        );
        // The same table on both sides, with duplicates inside it.
        load(&db, "c", &some(&[7, 7, 8]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM c UNION SELECT v FROM c")),
            col(&some(&[7, 8]))
        );
        db.cleanup();
    }

    #[test]
    fn disjoint_relations_give_the_sum_of_their_distinct_rows() {
        let db = rel_db("e4_disjoint");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[10, 10, 20]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b")),
            sorted(col(&some(&[1, 2, 10, 20])))
        );
        db.cleanup();
    }

    #[test]
    fn an_empty_branch_is_a_no_op_apart_from_deduplicating_the_other() {
        let db = rel_db("e4_empty");
        load(&db, "a", &some(&[1, 1, 2]));
        // `c`, `e` empty.
        for sql in [
            "SELECT v FROM a UNION SELECT v FROM c",
            "SELECT v FROM c UNION SELECT v FROM a",
            "SELECT v FROM c UNION SELECT v FROM a UNION SELECT v FROM e",
        ] {
            assert_eq!(sorted(run_tups(&db, sql)), col(&some(&[1, 2])), "{sql}");
        }
        assert_eq!(run_tups(&db, "SELECT v FROM c UNION SELECT v FROM e"), Vec::<Tup>::new());
        db.cleanup();
    }

    #[test]
    fn order_by_and_limit_apply_after_the_deduplication() {
        let db = rel_db("e4_order_limit");
        load(&db, "a", &some(&[3, 1, 3]));
        load(&db, "b", &some(&[2, 1, 4]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b ORDER BY v"),
            col(&some(&[1, 2, 3, 4]))
        );
        // The LIMIT counts distinct rows, not input rows.
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b ORDER BY v LIMIT 3"),
            col(&some(&[1, 2, 3]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b ORDER BY v DESC LIMIT 2 OFFSET 1"),
            col(&some(&[3, 2]))
        );
        assert_eq!(run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b LIMIT 100").len(), 4);
        db.cleanup();
    }

    #[test]
    fn three_branches_are_deduplicated_across_all_of_them() {
        let db = rel_db("e4_three");
        load(&db, "a", &some(&[1, 2]));
        load(&db, "b", &some(&[2, 3]));
        load(&db, "c", &some(&[3, 1, 9]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b UNION SELECT v FROM c")),
            col(&some(&[1, 2, 3, 9]))
        );
        // Mixed with UNION ALL: left to right, so (a UNION ALL b) UNION c.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION ALL SELECT v FROM b UNION SELECT v FROM c")),
            col(&some(&[1, 2, 3, 9]))
        );
        // ... and a UNION (b UNION ALL c): the inner ALL keeps its duplicates, the outer UNION removes them.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION (SELECT v FROM b UNION ALL SELECT v FROM c)")),
            col(&some(&[1, 2, 3, 9]))
        );
        // UNION ALL on the outside of a UNION keeps the inner distinct result and the other side's duplicates.
        assert_eq!(
            sorted(run_tups(&db, "(SELECT v FROM a UNION SELECT v FROM a) UNION ALL SELECT v FROM a")),
            col(&some(&[1, 1, 2, 2]))
        );
        db.cleanup();
    }

    #[test]
    fn values_of_different_numeric_types_that_are_equal_after_widening_are_one_row() {
        let db = typed_db("e4_widen_dedup");
        db.write("INSERT INTO nums (id, i, b, r, d, dec2, dec4) VALUES (1, 1, 1, 1.0, 1.0, 1.00, 1.0000)");
        db.write("INSERT INTO nums (id, i, b, r, d, dec2, dec4) VALUES (2, 2, 3, 2.5, 3.0, 2.00, 3.0000)");
        // INTEGER 1 and BIGINT 1 are the same BIGINT value.
        assert_eq!(db.rows("SELECT i FROM nums WHERE id = 1 UNION SELECT b FROM nums WHERE id = 1").len(), 1);
        // INTEGER / DECIMAL / DOUBLE spellings of one number collapse too.
        assert_eq!(db.rows("SELECT i FROM nums WHERE id = 1 UNION SELECT dec2 FROM nums WHERE id = 1").len(), 1);
        assert_eq!(db.rows("SELECT dec2 FROM nums WHERE id = 1 UNION SELECT dec4 FROM nums WHERE id = 1").len(), 1);
        assert_eq!(db.rows("SELECT i FROM nums WHERE id = 1 UNION SELECT d FROM nums WHERE id = 1").len(), 1);
        assert_eq!(db.rows("SELECT r FROM nums WHERE id = 1 UNION SELECT d FROM nums WHERE id = 1").len(), 1);
        // A column of all the types at once.
        let rows = db.rows(
            "SELECT i FROM nums UNION SELECT b FROM nums UNION SELECT dec2 FROM nums UNION SELECT dec4 FROM nums",
        );
        // {1, 2} u {1, 3} u {1.00, 2.00} u {1.0000, 3.0000} = {1, 2, 3} as DECIMAL(4).
        assert_eq!(rows.len(), 3, "{rows:?}");
        // Different numbers stay different: 2.5 (REAL) is not 3.0 (DOUBLE) ...
        assert_eq!(db.rows("SELECT r FROM nums WHERE id = 2 UNION SELECT d FROM nums WHERE id = 2").len(), 2);
        db.cleanup();
    }

    #[test]
    fn floating_point_zero_signs_are_one_row() {
        let db = typed_db("e4_zero");
        db.write("INSERT INTO nums (id, d) VALUES (1, 0.0)");
        db.write("INSERT INTO nums (id, d) VALUES (2, -0.0)");
        assert_eq!(db.rows("SELECT d FROM nums WHERE id = 1 UNION SELECT d FROM nums WHERE id = 2").len(), 1);
        assert_eq!(db.rows("SELECT d FROM nums UNION SELECT d FROM nums").len(), 1);
        db.cleanup();
    }

    #[test]
    fn text_is_compared_exactly_and_other_types_deduplicate_too() {
        let db = typed_db("e4_types");
        db.write("INSERT INTO nums (id, txt, flag, dt) VALUES (1, 'a', true, DATE '2024-01-02')");
        db.write("INSERT INTO nums (id, txt, flag, dt) VALUES (2, 'A', true, DATE '2024-01-02')");
        db.write("INSERT INTO nums (id, txt, flag, dt) VALUES (3, 'a', false, DATE '2024-01-03')");
        assert_eq!(db.rows("SELECT txt FROM nums UNION SELECT txt FROM nums").len(), 2, "'a' and 'A' differ");
        assert_eq!(db.rows("SELECT flag FROM nums UNION SELECT flag FROM nums").len(), 2);
        assert_eq!(db.rows("SELECT dt FROM nums UNION SELECT dt FROM nums").len(), 2);
        assert_eq!(db.rows("SELECT txt, flag, dt FROM nums UNION SELECT txt, flag, dt FROM nums").len(), 3);
        db.cleanup();
    }

    #[test]
    fn rows_removed_by_the_distinct_step_are_counted() {
        let db = rel_db("e4_metric");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1, 1, 1]));
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with("SELECT v FROM a UNION SELECT v FROM b", &ExecLimits::default(), &metrics, &[])
            .unwrap();
        assert_eq!(r.rows.len(), 2);
        // Six rows went in, two came out.
        assert_eq!(metrics.snapshot().set_op_rows_deduplicated_total, 4);
        assert_eq!(metrics.snapshot().set_op_branches_total, 2);
        db.cleanup();
    }

    #[test]
    fn the_distinct_rows_held_are_bounded_by_max_materialized_rows() {
        let db = rel_db("e4_bound_rows");
        load(&db, "a", &some(&[1, 2, 3]));
        load(&db, "b", &some(&[3, 4]));
        let limits = ExecLimits {
            max_materialized_rows: 4,
            ..ExecLimits::default()
        };
        // Exactly 4 distinct rows: fits.
        let r = db
            .try_run_with("SELECT v FROM a UNION SELECT v FROM b", &limits, &ExecMetrics::default(), &[])
            .unwrap();
        assert_eq!(r.rows.len(), 4);
        // One more distinct row than the bound: RESOURCE_LIMIT, nothing truncated.
        db.write("INSERT INTO b (id, v) VALUES (3, 5)");
        let err = db
            .try_run_with("SELECT v FROM a UNION SELECT v FROM b", &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_materialized_rows"), "{err}");
        // Duplicates do not count against the bound: many input rows, few distinct.
        let many: Vec<Option<i64>> = (0..50).map(|i| Some(i % 3)).collect();
        load(&db, "c", &many);
        let r = db
            .try_run_with("SELECT v FROM c UNION SELECT v FROM c", &limits, &ExecMetrics::default(), &[])
            .unwrap();
        assert_eq!(r.rows.len(), 3);
        db.cleanup();
    }

    #[test]
    fn the_bytes_held_are_bounded_by_max_materialized_bytes() {
        let db = rel_db("e4_bound_bytes");
        load(&db, "a", &some(&[1, 2, 3, 4, 5]));
        load(&db, "b", &some(&[6]));
        let limits = ExecLimits {
            max_materialized_bytes: 50,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with("SELECT v FROM a UNION SELECT v FROM b", &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        assert!(err.to_string().contains("max_materialized_bytes"), "{err}");
        // UNION ALL holds nothing, so the same bound does not touch it.
        let r = db
            .try_run_with("SELECT v FROM a UNION ALL SELECT v FROM b", &limits, &ExecMetrics::default(), &[])
            .unwrap();
        assert_eq!(r.rows.len(), 6);
        db.cleanup();
    }

    #[test]
    fn a_union_over_a_wide_text_column_counts_the_text_bytes() {
        let db = typed_db("e4_bound_text");
        for i in 1..=4 {
            db.write(&format!("INSERT INTO nums (id, txt) VALUES ({i}, '{}')", format!("{i}").repeat(2000)));
        }
        let limits = ExecLimits {
            max_materialized_bytes: 5000,
            ..ExecLimits::default()
        };
        let err = db
            .try_run_with("SELECT txt FROM nums UNION SELECT txt FROM nums", &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn explain_shows_a_union_node() {
        let db = rel_db("e4_explain");
        let text = crate::plan::explain(&db.plan("SELECT v FROM a UNION SELECT v FROM b"));
        assert!(text.contains("SetOp UNION branches=2 shape=UNION(#1, #2)"), "{text}");
        db.cleanup();
    }
}

// =======================================================================
// E5 -- INTERSECT and INTERSECT ALL
// =======================================================================

mod intersect {
    use super::*;
    use crate::error::SqlError;
    use crate::exec::{ExecLimits, ExecMetrics};

    #[test]
    fn the_distinct_form_returns_each_common_row_once() {
        let db = rel_db("e5_basic");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1, 1, 1]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a INTERSECT SELECT v FROM b"),
            col(&some(&[1]))
        );
        db.cleanup();
    }

    #[test]
    fn the_all_form_returns_min_of_the_counts_not_the_distinct_row() {
        let db = rel_db("e5_all");
        // [1,1,2] INTERSECT ALL [1,1,1] = [1,1]: min(2,3) = 2 copies of 1, none of 2.
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1, 1, 1]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a INTERSECT ALL SELECT v FROM b"),
            col(&some(&[1, 1]))
        );
        // And the other way round: min(3,2) is still 2.
        assert_eq!(
            run_tups(&db, "SELECT v FROM b INTERSECT ALL SELECT v FROM a"),
            col(&some(&[1, 1]))
        );
        db.cleanup();
    }

    #[test]
    fn the_all_form_counts_every_value_separately() {
        let db = rel_db("e5_all_counts");
        load(&db, "a", &some(&[1, 2, 2, 2, 3, 3]));
        load(&db, "b", &some(&[2, 2, 3, 3, 3, 4]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a INTERSECT ALL SELECT v FROM b")),
            col(&some(&[2, 2, 3, 3]))
        );
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a INTERSECT SELECT v FROM b")),
            col(&some(&[2, 3]))
        );
        db.cleanup();
    }

    #[test]
    fn two_identical_relations_intersect_to_the_relation() {
        let db = rel_db("e5_identical");
        load(&db, "a", &[Some(1), Some(1), Some(2), None]);
        load(&db, "b", &[Some(1), Some(1), Some(2), None]);
        // INTERSECT: the distinct relation.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a INTERSECT SELECT v FROM b")),
            sorted(col(&[Some(1), Some(2), None]))
        );
        // INTERSECT ALL: min(c, c) = c copies of each row, i.e. the relation
        // itself with its duplicates -- not twice the rows.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a INTERSECT ALL SELECT v FROM b")),
            sorted(col(&[Some(1), Some(1), Some(2), None]))
        );
        db.cleanup();
    }

    #[test]
    fn disjoint_relations_intersect_to_nothing() {
        let db = rel_db("e5_disjoint");
        load(&db, "a", &some(&[1, 2]));
        load(&db, "b", &some(&[3, 4]));
        for op in ["INTERSECT", "INTERSECT ALL"] {
            let sql = format!("SELECT v FROM a {op} SELECT v FROM b");
            assert_eq!(run_tups(&db, &sql), Vec::<Tup>::new(), "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn an_empty_branch_makes_the_result_empty_in_either_position() {
        let db = rel_db("e5_empty");
        load(&db, "a", &some(&[1, 2]));
        // `c`, `e` empty.
        for op in ["INTERSECT", "INTERSECT ALL"] {
            for sql in [
                format!("SELECT v FROM a {op} SELECT v FROM c"),
                format!("SELECT v FROM c {op} SELECT v FROM a"),
                format!("SELECT v FROM c {op} SELECT v FROM e"),
            ] {
                assert_eq!(run_tups(&db, &sql), Vec::<Tup>::new(), "{sql}");
            }
        }
        db.cleanup();
    }

    #[test]
    fn rows_with_nulls_intersect_as_not_distinct_values() {
        let db = rel_db("e5_nulls");
        // {1, NULL} vs {1, NULL}: they intersect.
        load2(&db, "p", &[(Some(1), None)]);
        load2(&db, "q", &[(Some(1), None)]);
        for op in ["INTERSECT", "INTERSECT ALL"] {
            assert_eq!(
                run_tups(&db, &format!("SELECT x, y FROM p {op} SELECT x, y FROM q")),
                vec![vec![Some(1), None]],
                "{op}"
            );
        }
        db.cleanup();

        // {1, NULL} vs {1, 0}: they do not.
        let db = rel_db("e5_nulls2");
        load2(&db, "p", &[(Some(1), None)]);
        load2(&db, "q", &[(Some(1), Some(0))]);
        for op in ["INTERSECT", "INTERSECT ALL"] {
            assert_eq!(
                run_tups(&db, &format!("SELECT x, y FROM p {op} SELECT x, y FROM q")),
                Vec::<Tup>::new(),
                "{op}"
            );
        }
        db.cleanup();

        // {NULL, NULL} vs {NULL, NULL}.
        let db = rel_db("e5_nulls3");
        load2(&db, "p", &[(None, None), (None, None), (None, None)]);
        load2(&db, "q", &[(None, None), (None, None)]);
        assert_eq!(
            run_tups(&db, "SELECT x, y FROM p INTERSECT SELECT x, y FROM q"),
            vec![vec![None, None]]
        );
        assert_eq!(
            run_tups(&db, "SELECT x, y FROM p INTERSECT ALL SELECT x, y FROM q"),
            vec![vec![None, None], vec![None, None]],
            "min(3, 2) copies of the all-NULL row"
        );
        // A single NULL column, NULL on one side only.
        load(&db, "a", &[None, Some(1)]);
        load(&db, "b", &[Some(1)]);
        assert_eq!(
            run_tups(&db, "SELECT v FROM a INTERSECT SELECT v FROM b"),
            col(&some(&[1]))
        );
        db.cleanup();
    }

    #[test]
    fn values_of_different_numeric_types_intersect_after_widening() {
        let db = typed_db("e5_widen");
        db.write("INSERT INTO nums (id, i, b, dec2) VALUES (1, 5, 5, 5.00)");
        db.write("INSERT INTO nums (id, i, b, dec2) VALUES (2, 6, 7, 6.00)");
        // INTEGER {5, 6} INTERSECT BIGINT {5, 7} = {5}.
        let rows = db.run("SELECT i FROM nums INTERSECT SELECT b FROM nums");
        assert_eq!(tups(&rows.rows), col(&some(&[5])));
        assert_eq!(rows.schema.fields[0].ty, Some(RelationalType::Bigint));
        // INTEGER {5, 6} INTERSECT DECIMAL {5.00, 6.00} = both.
        assert_eq!(db.rows("SELECT i FROM nums INTERSECT SELECT dec2 FROM nums").len(), 2);
        db.cleanup();
    }

    #[test]
    fn order_by_and_limit_apply_to_the_intersection() {
        let db = rel_db("e5_order");
        load(&db, "a", &some(&[5, 1, 3, 3, 2]));
        load(&db, "b", &some(&[3, 2, 5, 9]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a INTERSECT SELECT v FROM b ORDER BY v"),
            col(&some(&[2, 3, 5]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a INTERSECT SELECT v FROM b ORDER BY v DESC LIMIT 2"),
            col(&some(&[5, 3]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a INTERSECT ALL SELECT v FROM b ORDER BY v"),
            col(&some(&[2, 3, 5]))
        );
        db.cleanup();
    }

    #[test]
    fn three_branches_intersect_left_to_right() {
        let db = rel_db("e5_three");
        load(&db, "a", &some(&[1, 2, 3, 4]));
        load(&db, "b", &some(&[2, 3, 4, 5]));
        load(&db, "c", &some(&[3, 4, 5, 6]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a INTERSECT SELECT v FROM b INTERSECT SELECT v FROM c")),
            col(&some(&[3, 4]))
        );
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a INTERSECT ALL SELECT v FROM b INTERSECT ALL SELECT v FROM c")),
            col(&some(&[3, 4]))
        );
        db.cleanup();
    }

    /// The distinguishing data of the parser test, executed: A = {1, 2},
    /// B = {2, 3}, C = {3, 4}.
    fn abc(tag: &str) -> Db {
        let db = rel_db(tag);
        load(&db, "a", &some(&[1, 2]));
        load(&db, "b", &some(&[2, 3]));
        load(&db, "c", &some(&[3, 4]));
        db
    }

    #[test]
    fn intersect_binds_tighter_than_union_and_except_on_data_that_tells_the_parses_apart() {
        let db = abc("e5_precedence");
        let run = |sql: &str| sorted(run_tups(&db, sql));
        // A UNION B INTERSECT C == A UNION (B INTERSECT C) = {1,2} u {3} = {1,2,3}.
        assert_eq!(
            run("SELECT v FROM a UNION SELECT v FROM b INTERSECT SELECT v FROM c"),
            col(&some(&[1, 2, 3]))
        );
        // ... and NOT (A UNION B) INTERSECT C = {3}.
        assert_eq!(
            run("(SELECT v FROM a UNION SELECT v FROM b) INTERSECT SELECT v FROM c"),
            col(&some(&[3]))
        );
        // A INTERSECT B UNION C == (A INTERSECT B) UNION C = {2} u {3,4}.
        assert_eq!(
            run("SELECT v FROM a INTERSECT SELECT v FROM b UNION SELECT v FROM c"),
            col(&some(&[2, 3, 4]))
        );
        // A EXCEPT B INTERSECT C == A EXCEPT (B INTERSECT C) = {1,2} - {3} = {1,2}.
        assert_eq!(
            run("SELECT v FROM a EXCEPT SELECT v FROM b INTERSECT SELECT v FROM c"),
            col(&some(&[1, 2]))
        );
        // ... and NOT (A EXCEPT B) INTERSECT C = {1} n {3,4} = {}.
        assert_eq!(
            run("(SELECT v FROM a EXCEPT SELECT v FROM b) INTERSECT SELECT v FROM c"),
            Vec::<Tup>::new()
        );
        // With ALL: the same precedence.
        assert_eq!(
            run("SELECT v FROM a UNION ALL SELECT v FROM b INTERSECT ALL SELECT v FROM c"),
            col(&some(&[1, 2, 3]))
        );
        db.cleanup();
    }

    #[test]
    fn the_right_operand_is_read_once_not_once_per_left_row() {
        let db = rel_db("e5_once");
        let many: Vec<Option<i64>> = (0..200).map(Some).collect();
        load(&db, "a", &many);
        load(&db, "b", &many);
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with("SELECT v FROM a INTERSECT SELECT v FROM b", &ExecLimits::default(), &metrics, &[])
            .unwrap();
        assert_eq!(r.rows.len(), 200);
        let m = metrics.snapshot();
        assert_eq!(m.seq_scans, 2, "one scan per operand");
        assert!(m.rows_scanned <= 400, "{}", m.rows_scanned);
        db.cleanup();
    }

    #[test]
    fn the_distinct_step_counts_the_duplicates_it_removes() {
        let db = rel_db("e5_metric");
        load(&db, "a", &some(&[1, 1, 2, 7]));
        load(&db, "b", &some(&[1, 2]));
        let metrics = ExecMetrics::default();
        db.try_run_with("SELECT v FROM a INTERSECT SELECT v FROM b", &ExecLimits::default(), &metrics, &[])
            .unwrap();
        // Left rows that are on the right: 1, 1, 2 -> the second 1 is removed.
        // (The 7 is dropped for not being on the right; that is not a dedup.)
        assert_eq!(metrics.snapshot().set_op_rows_deduplicated_total, 1);
        let metrics = ExecMetrics::default();
        db.try_run_with("SELECT v FROM a INTERSECT ALL SELECT v FROM b", &ExecLimits::default(), &metrics, &[])
            .unwrap();
        assert_eq!(metrics.snapshot().set_op_rows_deduplicated_total, 0);
        db.cleanup();
    }

    #[test]
    fn the_rows_held_for_the_right_operand_are_bounded() {
        let db = rel_db("e5_bound");
        load(&db, "a", &some(&[1, 2, 3, 4]));
        load(&db, "b", &some(&[1, 2, 3, 4]));
        let limits = ExecLimits {
            max_materialized_rows: 3,
            ..ExecLimits::default()
        };
        for op in ["INTERSECT", "INTERSECT ALL"] {
            let err = db
                .try_run_with(&format!("SELECT v FROM a {op} SELECT v FROM b"), &limits, &ExecMetrics::default(), &[])
                .unwrap_err();
            assert!(matches!(err, SqlError::ResourceLimit { .. }), "{op}: {err:?}");
        }
        db.cleanup();
    }
}

// =======================================================================
// E6 -- EXCEPT and EXCEPT ALL
// =======================================================================

mod except {
    use super::*;
    use crate::error::SqlError;
    use crate::exec::{ExecLimits, ExecMetrics};

    #[test]
    fn the_distinct_form_removes_every_copy_of_a_row_that_is_on_the_right() {
        let db = rel_db("e6_basic");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a EXCEPT SELECT v FROM b"),
            col(&some(&[2]))
        );
        db.cleanup();
    }

    #[test]
    fn the_all_form_subtracts_counts_so_one_copy_of_a_duplicated_row_remains() {
        let db = rel_db("e6_all");
        // [1,1,2] EXCEPT ALL [1] = [1,2]: two 1s on the left, one on the right.
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT ALL SELECT v FROM b")),
            col(&some(&[1, 2]))
        );
        db.cleanup();
    }

    #[test]
    fn the_all_form_never_goes_below_zero() {
        let db = rel_db("e6_underflow");
        // [1,1,2] EXCEPT ALL [1,1,1]: max(0, 2-3) = 0 copies of 1; the 2 stays.
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1, 1, 1]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a EXCEPT ALL SELECT v FROM b"),
            col(&some(&[2]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a EXCEPT SELECT v FROM b"),
            col(&some(&[2]))
        );
        // The reverse direction: [1,1,1] EXCEPT ALL [1,1,2] = [1].
        assert_eq!(
            run_tups(&db, "SELECT v FROM b EXCEPT ALL SELECT v FROM a"),
            col(&some(&[1]))
        );
        // ... while the distinct form removes the 1 entirely.
        assert_eq!(
            run_tups(&db, "SELECT v FROM b EXCEPT SELECT v FROM a"),
            Vec::<Tup>::new()
        );
        db.cleanup();
    }

    #[test]
    fn two_identical_relations_leave_nothing() {
        let db = rel_db("e6_identical");
        load(&db, "a", &[Some(1), Some(1), Some(2), None]);
        load(&db, "b", &[Some(1), Some(1), Some(2), None]);
        for op in ["EXCEPT", "EXCEPT ALL"] {
            let sql = format!("SELECT v FROM a {op} SELECT v FROM b");
            assert_eq!(run_tups(&db, &sql), Vec::<Tup>::new(), "{sql}");
        }
        db.cleanup();
    }

    #[test]
    fn disjoint_relations_leave_the_left() {
        let db = rel_db("e6_disjoint");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[7, 8]));
        // EXCEPT: the left, deduplicated. EXCEPT ALL: the left as it is.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT SELECT v FROM b")),
            col(&some(&[1, 2]))
        );
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT ALL SELECT v FROM b")),
            col(&some(&[1, 1, 2]))
        );
        db.cleanup();
    }

    #[test]
    fn an_empty_right_branch_leaves_the_left_and_an_empty_left_branch_leaves_nothing() {
        let db = rel_db("e6_empty");
        load(&db, "a", &some(&[1, 1, 2]));
        // `c`, `e` empty.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT SELECT v FROM c")),
            col(&some(&[1, 2]))
        );
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT ALL SELECT v FROM c")),
            col(&some(&[1, 1, 2]))
        );
        for op in ["EXCEPT", "EXCEPT ALL"] {
            for sql in [
                format!("SELECT v FROM c {op} SELECT v FROM a"),
                format!("SELECT v FROM c {op} SELECT v FROM e"),
            ] {
                assert_eq!(run_tups(&db, &sql), Vec::<Tup>::new(), "{sql}");
            }
        }
        db.cleanup();
    }

    #[test]
    fn rows_with_nulls_are_removed_when_they_match_as_not_distinct_values() {
        // {1, NULL} - {1, NULL} = nothing.
        let db = rel_db("e6_nulls");
        load2(&db, "p", &[(Some(1), None)]);
        load2(&db, "q", &[(Some(1), None)]);
        for op in ["EXCEPT", "EXCEPT ALL"] {
            assert_eq!(
                run_tups(&db, &format!("SELECT x, y FROM p {op} SELECT x, y FROM q")),
                Vec::<Tup>::new(),
                "{op}"
            );
        }
        db.cleanup();

        // {1, NULL} - {1, 0} = {1, NULL}.
        let db = rel_db("e6_nulls2");
        load2(&db, "p", &[(Some(1), None)]);
        load2(&db, "q", &[(Some(1), Some(0))]);
        for op in ["EXCEPT", "EXCEPT ALL"] {
            assert_eq!(
                run_tups(&db, &format!("SELECT x, y FROM p {op} SELECT x, y FROM q")),
                vec![vec![Some(1), None]],
                "{op}"
            );
        }
        db.cleanup();

        // {NULL, NULL} counted: three on the left, one on the right.
        let db = rel_db("e6_nulls3");
        load2(&db, "p", &[(None, None), (None, None), (None, None)]);
        load2(&db, "q", &[(None, None)]);
        assert_eq!(
            run_tups(&db, "SELECT x, y FROM p EXCEPT SELECT x, y FROM q"),
            Vec::<Tup>::new()
        );
        assert_eq!(
            run_tups(&db, "SELECT x, y FROM p EXCEPT ALL SELECT x, y FROM q"),
            vec![vec![None, None], vec![None, None]]
        );
        db.cleanup();
    }

    #[test]
    fn a_chain_applies_left_to_right() {
        let db = rel_db("e6_chain");
        load(&db, "a", &some(&[1, 2, 3, 4]));
        load(&db, "b", &some(&[2, 3]));
        load(&db, "c", &some(&[3, 4, 9]));
        // (A - B) - C = {1,4} - {3,4,9} = {1}; A - (B - C) = {1,2,3,4} - {2} = {1,3,4}.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT SELECT v FROM b EXCEPT SELECT v FROM c")),
            col(&some(&[1]))
        );
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT (SELECT v FROM b EXCEPT SELECT v FROM c)")),
            col(&some(&[1, 3, 4]))
        );
        // A EXCEPT B UNION C == (A EXCEPT B) UNION C: {1,4} u {3,4,9}.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT SELECT v FROM b UNION SELECT v FROM c")),
            col(&some(&[1, 3, 4, 9]))
        );
        // ... and not A EXCEPT (B UNION C) = {1}.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a EXCEPT (SELECT v FROM b UNION SELECT v FROM c)")),
            col(&some(&[1]))
        );
        // A UNION B EXCEPT C == (A UNION B) EXCEPT C = {1,2,3,4} - {3,4,9}.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM a UNION SELECT v FROM b EXCEPT SELECT v FROM c")),
            col(&some(&[1, 2]))
        );
        db.cleanup();
    }

    #[test]
    fn values_of_different_numeric_types_are_removed_after_widening() {
        let db = typed_db("e6_widen");
        db.write("INSERT INTO nums (id, i, b, d) VALUES (1, 5, 5, 5.0)");
        db.write("INSERT INTO nums (id, i, b, d) VALUES (2, 6, 7, 7.5)");
        // INTEGER {5, 6} EXCEPT BIGINT {5, 7} = {6}.
        assert_eq!(
            tups(&db.rows("SELECT i FROM nums EXCEPT SELECT b FROM nums")),
            col(&some(&[6]))
        );
        // INTEGER {5, 6} EXCEPT DOUBLE {5.0, 7.5} = {6.0}.
        let r = db.run("SELECT i FROM nums EXCEPT SELECT d FROM nums");
        assert_eq!(r.rows, vec![vec![Some(RelationalValue::Double(6.0))]]);
        db.cleanup();
    }

    #[test]
    fn order_by_and_limit_apply_to_the_difference() {
        let db = rel_db("e6_order");
        load(&db, "a", &some(&[5, 1, 3, 3, 2, 8]));
        load(&db, "b", &some(&[2, 8]));
        assert_eq!(
            run_tups(&db, "SELECT v FROM a EXCEPT SELECT v FROM b ORDER BY v DESC"),
            col(&some(&[5, 3, 1]))
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM a EXCEPT ALL SELECT v FROM b ORDER BY v LIMIT 3"),
            col(&some(&[1, 3, 3]))
        );
        db.cleanup();
    }

    #[test]
    fn the_distinct_step_counts_the_duplicates_it_removes() {
        let db = rel_db("e6_metric");
        load(&db, "a", &some(&[1, 1, 2, 2, 3]));
        load(&db, "b", &some(&[3]));
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with("SELECT v FROM a EXCEPT SELECT v FROM b", &ExecLimits::default(), &metrics, &[])
            .unwrap();
        assert_eq!(sorted(tups(&r.rows)), col(&some(&[1, 2])));
        // Left rows not on the right: 1, 1, 2, 2 -> two duplicates removed.
        assert_eq!(metrics.snapshot().set_op_rows_deduplicated_total, 2);
        let metrics = ExecMetrics::default();
        db.try_run_with("SELECT v FROM a EXCEPT ALL SELECT v FROM b", &ExecLimits::default(), &metrics, &[])
            .unwrap();
        assert_eq!(metrics.snapshot().set_op_rows_deduplicated_total, 0);
        db.cleanup();
    }

    #[test]
    fn the_rows_held_are_bounded_for_the_right_operand_and_for_the_distinct_left() {
        let db = rel_db("e6_bound");
        load(&db, "a", &some(&[1, 2, 3, 4]));
        load(&db, "b", &some(&[1, 2, 3, 4]));
        load(&db, "c", &[]);
        let limits = ExecLimits {
            max_materialized_rows: 3,
            ..ExecLimits::default()
        };
        // The right operand's 4 distinct rows exceed the bound ...
        for op in ["EXCEPT", "EXCEPT ALL"] {
            let err = db
                .try_run_with(&format!("SELECT v FROM a {op} SELECT v FROM b"), &limits, &ExecMetrics::default(), &[])
                .unwrap_err();
            assert!(matches!(err, SqlError::ResourceLimit { .. }), "{op}: {err:?}");
        }
        // ... and so do the left's, when the right is empty and EXCEPT must
        // remember what it has already produced.
        let err = db
            .try_run_with("SELECT v FROM a EXCEPT SELECT v FROM c", &limits, &ExecMetrics::default(), &[])
            .unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        // EXCEPT ALL against an empty right holds nothing and streams the left.
        let r = db
            .try_run_with("SELECT v FROM a EXCEPT ALL SELECT v FROM c", &limits, &ExecMetrics::default(), &[])
            .unwrap();
        assert_eq!(r.rows.len(), 4);
        db.cleanup();
    }
}

// =======================================================================
// Differential: every operator, random multisets with NULLs, against the
// reference model
// =======================================================================

mod diff {
    use super::*;

    /// xorshift64*: deterministic, no dependency.
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

    fn random_values(rng: &mut Rng, max_len: u64, domain: u64) -> Vec<Option<i64>> {
        let len = rng.below(max_len + 1);
        (0..len)
            .map(|_| if rng.below(5) == 0 { None } else { Some(rng.below(domain) as i64) })
            .collect()
    }

    fn reload(db: &Db, name: &str, values: &[Option<i64>]) {
        db.write(&format!("DELETE FROM {name}"));
        load(db, name, values);
    }

    fn reload2(db: &Db, name: &str, rows: &[(Option<i64>, Option<i64>)]) {
        db.write(&format!("DELETE FROM {name}"));
        load2(db, name, rows);
    }

    #[test]
    fn one_column_all_six_operators_match_the_model_on_random_multisets() {
        let db = rel_db("diff_one");
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _seed in 0..30 {
            let a = random_values(&mut rng, 9, 4);
            let b = random_values(&mut rng, 9, 4);
            reload(&db, "a", &a);
            reload(&db, "b", &b);
            for (op, all) in OPERATORS {
                let sql = format!("SELECT v FROM a {} SELECT v FROM b", op_sql(op, all));
                let got = sorted(run_tups(&db, &sql));
                let want = sorted(model(op, all, &col(&a), &col(&b)));
                assert_eq!(got, want, "{sql}\nleft={a:?}\nright={b:?}");
            }
        }
        db.cleanup();
    }

    #[test]
    fn two_columns_with_nulls_in_either_column_match_the_model() {
        let db = rel_db("diff_two");
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let rows = |rng: &mut Rng| -> Vec<(Option<i64>, Option<i64>)> {
            let len = rng.below(8);
            (0..len)
                .map(|_| {
                    let pick = |rng: &mut Rng| if rng.below(3) == 0 { None } else { Some(rng.below(3) as i64) };
                    (pick(rng), pick(rng))
                })
                .collect()
        };
        for _seed in 0..30 {
            let p = rows(&mut rng);
            let q = rows(&mut rng);
            reload2(&db, "p", &p);
            reload2(&db, "q", &q);
            let as_tups = |r: &[(Option<i64>, Option<i64>)]| -> Vec<Tup> { r.iter().map(|(x, y)| vec![*x, *y]).collect() };
            for (op, all) in OPERATORS {
                let sql = format!("SELECT x, y FROM p {} SELECT x, y FROM q", op_sql(op, all));
                let got = sorted(run_tups(&db, &sql));
                let want = sorted(model(op, all, &as_tups(&p), &as_tups(&q)));
                assert_eq!(got, want, "{sql}\nleft={p:?}\nright={q:?}");
            }
        }
        db.cleanup();
    }

    /// `A op1 B op2 C` with SQL's precedence (INTERSECT first, the rest left
    /// to right), written independently of the parser.
    fn model3(op1: (&str, bool), op2: (&str, bool), a: &[Tup], b: &[Tup], c: &[Tup]) -> Vec<Tup> {
        if op2.0 == "INTERSECT" && op1.0 != "INTERSECT" {
            let bc = model(op2.0, op2.1, b, c);
            model(op1.0, op1.1, a, &bc)
        } else {
            let ab = model(op1.0, op1.1, a, b);
            model(op2.0, op2.1, &ab, c)
        }
    }

    #[test]
    fn three_branches_with_every_pair_of_operators_follow_the_precedence_rules() {
        let db = rel_db("diff_three");
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        for _seed in 0..6 {
            let a = random_values(&mut rng, 6, 4);
            let b = random_values(&mut rng, 6, 4);
            let c = random_values(&mut rng, 6, 4);
            reload(&db, "a", &a);
            reload(&db, "b", &b);
            reload(&db, "c", &c);
            for op1 in OPERATORS {
                for op2 in OPERATORS {
                    let sql = format!(
                        "SELECT v FROM a {} SELECT v FROM b {} SELECT v FROM c",
                        op_sql(op1.0, op1.1),
                        op_sql(op2.0, op2.1)
                    );
                    let got = sorted(run_tups(&db, &sql));
                    let want = sorted(model3(op1, op2, &col(&a), &col(&b), &col(&c)));
                    assert_eq!(got, want, "{sql}\na={a:?}\nb={b:?}\nc={c:?}");
                    // Parentheses around the right pair force the other grouping.
                    let sql_r = format!(
                        "SELECT v FROM a {} (SELECT v FROM b {} SELECT v FROM c)",
                        op_sql(op1.0, op1.1),
                        op_sql(op2.0, op2.1)
                    );
                    let bc = model(op2.0, op2.1, &col(&b), &col(&c));
                    let want_r = sorted(model(op1.0, op1.1, &col(&a), &bc));
                    assert_eq!(sorted(run_tups(&db, &sql_r)), want_r, "{sql_r}");
                    // Parentheses around the left pair.
                    let sql_l = format!(
                        "(SELECT v FROM a {} SELECT v FROM b) {} SELECT v FROM c",
                        op_sql(op1.0, op1.1),
                        op_sql(op2.0, op2.1)
                    );
                    let ab = model(op1.0, op1.1, &col(&a), &col(&b));
                    let want_l = sorted(model(op2.0, op2.1, &ab, &col(&c)));
                    assert_eq!(sorted(run_tups(&db, &sql_l)), want_l, "{sql_l}");
                }
            }
        }
        db.cleanup();
    }

    #[test]
    fn the_model_and_the_engine_agree_with_the_documented_examples() {
        // The contract's worked examples, hard-coded, checked against the
        // model (so a mistake in the model cannot hide a mistake in the
        // engine) and against the engine over real tables.
        let db = rel_db("diff_examples");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[1, 1, 1]));
        load(&db, "c", &some(&[1]));
        let l = col(&some(&[1, 1, 2]));
        let r = col(&some(&[1, 1, 1]));
        let one = col(&some(&[1]));
        for (op, all, left, right, right_table, expected) in [
            ("INTERSECT", true, &l, &r, "b", some(&[1, 1])),
            ("INTERSECT", false, &l, &r, "b", some(&[1])),
            ("EXCEPT", true, &l, &one, "c", some(&[1, 2])),
            ("EXCEPT", false, &l, &one, "c", some(&[2])),
            ("UNION", false, &l, &r, "b", some(&[1, 2])),
            ("UNION", true, &l, &r, "b", some(&[1, 1, 1, 1, 1, 2])),
        ] {
            assert_eq!(sorted(model(op, all, left, right)), sorted(col(&expected)), "model {op} {all}");
            let sql = format!("SELECT v FROM a {} SELECT v FROM {right_table}", op_sql(op, all));
            assert_eq!(sorted(run_tups(&db, &sql)), sorted(col(&expected)), "engine {sql}");
        }
        assert_eq!(
            sorted(model("UNION", false, &[vec![Some(1), None]], &[vec![Some(1), None]])),
            vec![vec![Some(1), None]]
        );
        db.cleanup();
    }
}

// =======================================================================
// E7 -- set operations in derived tables, CTE bodies, subqueries, DML
// =======================================================================

mod contexts {
    use super::*;
    use crate::correlated_tests::data::{standard_db, ORow, TRow};
    use crate::error::SqlError;
    use crate::exec::{ExecLimits, ExecMetrics};

    fn run_params(db: &Db, sql: &str, params: &[V]) -> crate::exec::QueryResult {
        db.try_run_with(sql, &ExecLimits::default(), &ExecMetrics::default(), params)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    // ---- derived tables -------------------------------------------------------------

    #[test]
    fn a_derived_table_can_be_a_set_operation_with_the_outer_where_applied_on_top() {
        let db = rel_db("e7_derived");
        load(&db, "a", &some(&[1, 2, 3]));
        load(&db, "b", &some(&[3, 4]));
        assert_eq!(
            sorted(run_tups(&db, "SELECT u.v FROM ((SELECT v FROM a) UNION (SELECT v FROM b)) AS u")),
            col(&some(&[1, 2, 3, 4]))
        );
        // The outer WHERE sees the set operation's output.
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT u.v FROM ((SELECT v FROM a) UNION (SELECT v FROM b)) AS u WHERE u.v > 2"
            )),
            col(&some(&[3, 4]))
        );
        // The prompt's other spelling, without parentheses around the operands.
        assert_eq!(
            sorted(run_tups(&db, "SELECT v FROM (SELECT v FROM a UNION ALL SELECT v FROM b) AS u WHERE v >= 3")),
            col(&some(&[3, 3, 4]))
        );
        // Aggregates, ORDER BY and LIMIT above it.
        assert_eq!(
            run_tups(&db, "SELECT COUNT(*) FROM (SELECT v FROM a UNION ALL SELECT v FROM b) AS u"),
            vec![vec![Some(5)]]
        );
        assert_eq!(
            run_tups(&db, "SELECT COUNT(*) FROM (SELECT v FROM a UNION SELECT v FROM b) AS u"),
            vec![vec![Some(4)]]
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM (SELECT v FROM a UNION SELECT v FROM b) AS u ORDER BY v DESC LIMIT 2"),
            col(&some(&[4, 3]))
        );
        // A column-name list renames the output.
        assert_eq!(
            sorted(run_tups(&db, "SELECT x FROM (SELECT v FROM a UNION SELECT v FROM b) AS u(x) WHERE x < 3")),
            col(&some(&[1, 2]))
        );
        db.cleanup();
    }

    #[test]
    fn a_derived_set_operation_joins_with_a_table_and_nests_in_another() {
        let db = rel_db("e7_derived_join");
        load(&db, "a", &some(&[1, 2]));
        load(&db, "b", &some(&[2, 3]));
        load(&db, "c", &some(&[10, 20, 30]));
        // Join (the derived set operation on the right is materialized once).
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT c.id, u.v FROM c JOIN (SELECT v FROM a UNION SELECT v FROM b) AS u ON u.v = c.id"
            )),
            sorted(vec![vec![Some(1), Some(1)], vec![Some(2), Some(2)], vec![Some(3), Some(3)]])
        );
        // A set operation inside a set operation's derived operand.
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT d.v FROM (SELECT v FROM a UNION SELECT v FROM b) AS d \
                 INTERSECT SELECT v FROM (SELECT v FROM b UNION SELECT v FROM c) AS e"
            )),
            col(&some(&[2, 3]))
        );
        // Nested derived tables, set operations at both levels.
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT v FROM (SELECT v FROM (SELECT v FROM a UNION ALL SELECT v FROM b) AS x EXCEPT ALL SELECT v FROM c) AS y"
            )),
            col(&some(&[1, 2, 2, 3]))
        );
        db.cleanup();
    }

    #[test]
    fn a_non_lateral_derived_set_operation_cannot_read_the_query_around_it_but_lateral_can() {
        let (db, t, orders) = standard_db("e7_lateral");
        // Non-LATERAL: refused, with the same message as for a plain body.
        let err = db
            .try_run("SELECT t.id FROM t JOIN (SELECT amount FROM orders WHERE t_id = t.id UNION SELECT 0) AS d ON true")
            .unwrap_err();
        assert!(err.to_string().contains("cannot reference outer column"), "{err}");
        assert!(err.to_string().contains("without LATERAL"), "{err}");
        // LATERAL: each operand may read the preceding FROM item.
        let rows = tups(&db.rows(
            "SELECT t.id, l.n FROM t LEFT JOIN LATERAL \
             (SELECT o.id AS n FROM orders o WHERE o.t_id = t.id UNION SELECT 0 - t.id) AS l ON true \
             WHERE t.id <= 4",
        ));
        let mut want: Vec<Tup> = Vec::new();
        for id in 1..=4i64 {
            for o in orders.iter().filter(|o| o.3 == Some(id as i32)) {
                want.push(vec![Some(id), Some(o.0)]);
            }
            want.push(vec![Some(id), Some(-id)]);
        }
        assert_eq!(sorted(rows), sorted(want));
        let _ = t;
        db.cleanup();
    }

    // ---- CTE bodies -----------------------------------------------------------------------

    #[test]
    fn a_cte_body_can_be_a_set_operation() {
        let db = rel_db("e7_cte");
        load(&db, "a", &some(&[1, 2, 3]));
        load(&db, "b", &some(&[3, 4]));
        assert_eq!(
            run_tups(&db, "WITH u AS (SELECT v FROM a UNION SELECT v FROM b) SELECT v FROM u ORDER BY v"),
            col(&some(&[1, 2, 3, 4]))
        );
        // The forward-looking shape: a UNION ALL body, non-recursive.
        assert_eq!(
            sorted(run_tups(&db, "WITH cte AS (SELECT 1 UNION ALL SELECT 2) SELECT * FROM cte")),
            col(&some(&[1, 2]))
        );
        // A column-name list renames the first branch's names.
        assert_eq!(
            run_tups(&db, "WITH u(x) AS (SELECT v FROM a UNION SELECT v FROM b) SELECT x FROM u WHERE x > 3"),
            col(&some(&[4]))
        );
        // A later CTE reads an earlier set-operation CTE and is one itself.
        assert_eq!(
            sorted(run_tups(
                &db,
                "WITH u AS (SELECT v FROM a UNION SELECT v FROM b), \
                      w AS (SELECT v FROM u EXCEPT SELECT v FROM b) \
                 SELECT v FROM w"
            )),
            col(&some(&[1, 2]))
        );
        // The WITH in front of a set operation is visible in every operand.
        assert_eq!(
            sorted(run_tups(
                &db,
                "WITH u AS (SELECT v FROM a) SELECT v FROM u UNION ALL SELECT v + 100 FROM u"
            )),
            col(&some(&[1, 2, 3, 101, 102, 103]))
        );
        db.cleanup();
    }

    #[test]
    fn a_set_operation_cte_read_twice_is_computed_once_and_both_reads_see_the_same_rows() {
        let db = rel_db("e7_cte_twice");
        load(&db, "a", &some(&[1, 1, 2]));
        load(&db, "b", &some(&[2, 3]));
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with(
                "WITH u AS (SELECT v FROM a UNION ALL SELECT v FROM b) \
                 SELECT x.v, y.v FROM u x JOIN u y ON x.v = y.v",
                &ExecLimits::default(),
                &metrics,
                &[],
            )
            .unwrap();
        // u = [1,1,2,2,3]: a self-join on the value = 2*2 + 2*2 + 1 = 9 pairs.
        assert_eq!(r.rows.len(), 9);
        let m = metrics.snapshot();
        assert_eq!(m.cte_materializations_total, 1, "materialized once");
        assert_eq!(m.seq_scans, 2, "each base table scanned once, not once per reference");
        assert_eq!(m.set_op_branches_total, 2, "the definition is counted once");
        // A scalar read of the same CTE twice, in one statement.
        let r = db.run(
            "WITH u AS (SELECT v FROM a UNION ALL SELECT v FROM b) \
             SELECT (SELECT COUNT(*) FROM u), (SELECT SUM(v) FROM u)",
        );
        assert_eq!(tups(&r.rows), vec![vec![Some(5), Some(9)]]);
        db.cleanup();
    }

    #[test]
    fn a_set_operation_cte_nobody_reads_is_bound_but_never_run() {
        let db = rel_db("e7_cte_dead");
        load(&db, "a", &some(&[1]));
        let metrics = ExecMetrics::default();
        let r = db
            .try_run_with(
                "WITH dead AS (SELECT v FROM a UNION SELECT v FROM b) SELECT v FROM a",
                &ExecLimits::default(),
                &metrics,
                &[],
            )
            .unwrap();
        assert_eq!(r.rows.len(), 1);
        let m = metrics.snapshot();
        assert_eq!(m.cte_materializations_total, 0);
        assert_eq!(m.seq_scans, 1, "only the live query scans anything");
        // ... but it is still checked at bind time.
        let err = db
            .try_run("WITH dead AS (SELECT v FROM a UNION SELECT id, v FROM b) SELECT v FROM a")
            .unwrap_err();
        assert!(matches!(err, SqlError::TypeMismatch { .. }), "{err:?}");
        db.cleanup();
    }

    // ---- scalar subqueries ----------------------------------------------------------------

    #[test]
    fn a_scalar_set_operation_returns_null_for_zero_rows_the_value_for_one_and_errors_for_many() {
        let db = rel_db("e7_scalar");
        load(&db, "a", &some(&[1, 2]));
        load(&db, "b", &some(&[2, 3]));
        let one = |sql: &str| -> Option<i64> {
            let rows = tups(&db.rows(sql));
            assert_eq!(rows.len(), 1, "{sql}");
            rows[0][0]
        };
        // 0 rows -> NULL.
        assert_eq!(one("SELECT (SELECT v FROM a WHERE v > 100 UNION SELECT v FROM b WHERE v > 100)"), None);
        assert_eq!(one("SELECT (SELECT v FROM a INTERSECT SELECT v FROM c)"), None);
        // 1 row -> the value (UNION collapses the two 2s).
        assert_eq!(one("SELECT (SELECT v FROM a WHERE v = 2 UNION SELECT v FROM b WHERE v = 2)"), Some(2));
        assert_eq!(one("SELECT (SELECT v FROM a INTERSECT SELECT v FROM b)"), Some(2));
        assert_eq!(one("SELECT (SELECT v FROM a EXCEPT SELECT v FROM b WHERE v = 2)"), Some(1));
        // > 1 rows -> a runtime error, not a bind error.
        for sql in [
            "SELECT (SELECT v FROM a UNION SELECT v FROM b)",
            "SELECT (SELECT v FROM a WHERE v = 2 UNION ALL SELECT v FROM b WHERE v = 2)",
            "SELECT (SELECT v FROM a EXCEPT ALL SELECT v FROM c)",
        ] {
            db.try_bind(sql).expect("binds");
            let err = db.try_run(sql).unwrap_err();
            assert!(
                matches!(err, SqlError::ExecutionParameter { ref detail } if detail.contains("more than one row")),
                "{sql}: {err:?}"
            );
        }
        db.cleanup();
    }

    #[test]
    fn a_correlated_scalar_set_operation_is_evaluated_for_each_outer_row() {
        let (db, t, _orders) = standard_db("e7_scalar_corr");
        // Both operands read the outer row; UNION collapses them to one value.
        let rows = tups(&db.rows(
            "SELECT t.id, (SELECT x.id FROM t x WHERE x.id = t.id UNION SELECT y.id FROM t y WHERE y.id = t.id) FROM t",
        ));
        let want: Vec<Tup> = t.iter().map(|r: &TRow| vec![Some(i64::from(r.0)), Some(i64::from(r.0))]).collect();
        assert_eq!(sorted(rows), sorted(want));
        // UNION ALL keeps both: more than one row, and the error names the outer row.
        let err = db
            .try_run("SELECT t.id, (SELECT x.id FROM t x WHERE x.id = t.id UNION ALL SELECT y.id FROM t y WHERE y.id = t.id) FROM t")
            .unwrap_err();
        assert!(err.to_string().contains("more than one row"), "{err}");
        assert!(err.to_string().contains("t.id"), "the error names the enclosing row: {err}");
        db.cleanup();
    }

    // ---- IN / EXISTS -------------------------------------------------------------------------

    #[test]
    fn in_sees_the_deduplicated_result_and_follows_three_valued_logic() {
        let db = rel_db("e7_in");
        load(&db, "a", &[Some(1), Some(2)]);
        load(&db, "b", &[Some(2), Some(3)]);
        // Membership in the union.
        assert_eq!(
            sorted(run_tups(&db, "SELECT id FROM e UNION ALL SELECT v FROM a WHERE v IN (SELECT v FROM a UNION SELECT v FROM b)")),
            col(&some(&[1, 2]))
        );
        let flags = |db: &Db, sql: &str| -> Vec<(i64, Option<bool>)> {
            let mut out: Vec<(i64, Option<bool>)> = db
                .rows(sql)
                .into_iter()
                .map(|r| {
                    let id = match &r[0] {
                        Some(RelationalValue::Integer(i)) => i64::from(*i),
                        other => panic!("{other:?}"),
                    };
                    let flag = match &r[1] {
                        Some(RelationalValue::Boolean(b)) => Some(*b),
                        None => None,
                        other => panic!("{other:?}"),
                    };
                    (id, flag)
                })
                .collect();
            out.sort();
            out
        };
        // Without a NULL in the set: members TRUE, others FALSE.
        db.write("INSERT INTO c (id, v) VALUES (1, 1), (2, 2), (3, 3), (4, 4)");
        assert_eq!(
            flags(&db, "SELECT v, v IN (SELECT v FROM a UNION SELECT v FROM b) FROM c"),
            vec![(1, Some(true)), (2, Some(true)), (3, Some(true)), (4, Some(false))]
        );
        // A NULL in the (deduplicated) set: a non-member is UNKNOWN, not FALSE.
        db.write("INSERT INTO a (id, v) VALUES (10, NULL)");
        db.write("INSERT INTO b (id, v) VALUES (10, NULL)");
        assert_eq!(
            flags(&db, "SELECT v, v IN (SELECT v FROM a UNION SELECT v FROM b) FROM c"),
            vec![(1, Some(true)), (2, Some(true)), (3, Some(true)), (4, None)]
        );
        // NOT IN: members FALSE, non-members UNKNOWN -- so a WHERE keeps nothing.
        assert_eq!(
            flags(&db, "SELECT v, v NOT IN (SELECT v FROM a UNION SELECT v FROM b) FROM c"),
            vec![(1, Some(false)), (2, Some(false)), (3, Some(false)), (4, None)]
        );
        assert_eq!(
            run_tups(&db, "SELECT v FROM c WHERE v NOT IN (SELECT v FROM a UNION SELECT v FROM b)"),
            Vec::<Tup>::new()
        );
        // The same NULL in an INTERSECT / EXCEPT result.
        assert_eq!(
            flags(&db, "SELECT v, v IN (SELECT v FROM a INTERSECT SELECT v FROM b) FROM c"),
            vec![(1, None), (2, Some(true)), (3, None), (4, None)]
        );
        assert_eq!(
            flags(&db, "SELECT v, v IN (SELECT v FROM a EXCEPT SELECT v FROM e) FROM c"),
            vec![(1, Some(true)), (2, Some(true)), (3, None), (4, None)]
        );
        // The set operation removing the NULL removes the UNKNOWN.
        assert_eq!(
            flags(&db, "SELECT v, v IN (SELECT v FROM a EXCEPT SELECT v FROM a WHERE v IS NULL) FROM c"),
            vec![(1, Some(true)), (2, Some(true)), (3, Some(false)), (4, Some(false))]
        );
        db.cleanup();
    }

    #[test]
    fn exists_over_a_set_operation() {
        let db = rel_db("e7_exists");
        load(&db, "a", &some(&[1, 2]));
        load(&db, "b", &some(&[1, 2]));
        load(&db, "c", &some(&[5]));
        let exists = |sql: &str| -> bool {
            let rows = db.rows(sql);
            assert_eq!(rows.len(), 1, "{sql}");
            matches!(rows[0][0], Some(RelationalValue::Boolean(true)))
        };
        assert!(exists("SELECT EXISTS (SELECT v FROM a UNION SELECT v FROM c)"));
        assert!(!exists("SELECT EXISTS (SELECT v FROM a EXCEPT SELECT v FROM b)"));
        assert!(exists("SELECT EXISTS (SELECT v FROM a EXCEPT ALL SELECT v FROM c)"));
        assert!(!exists("SELECT EXISTS (SELECT v FROM a INTERSECT SELECT v FROM c)"));
        assert!(exists("SELECT NOT EXISTS (SELECT v FROM a INTERSECT SELECT v FROM c)"));
        // Correlated: rows of `a` whose value is also in `b` but not in `c`.
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT v FROM a WHERE EXISTS (SELECT b.v FROM b WHERE b.v = a.v EXCEPT SELECT c.v FROM c WHERE c.v = a.v)"
            )),
            col(&some(&[1, 2]))
        );
        db.cleanup();
    }

    // ---- UPDATE / DELETE -------------------------------------------------------------------

    #[test]
    fn delete_where_accepts_a_set_operation_subquery() {
        let db = rel_db("e7_delete");
        load(&db, "a", &some(&[1, 2, 3, 4, 5, 6]));
        load(&db, "b", &some(&[2, 3]));
        load(&db, "c", &some(&[3, 4]));
        // v IN (b UNION c) = {2,3,4}
        let r = db.write("DELETE FROM a WHERE v IN (SELECT v FROM b UNION SELECT v FROM c)");
        assert_eq!(r.rows_affected, 3);
        assert_eq!(sorted(run_tups(&db, "SELECT v FROM a")), col(&some(&[1, 5, 6])));
        // v IN (b INTERSECT c) = {3}: already gone, nothing left to delete.
        let r = db.write("DELETE FROM a WHERE v IN (SELECT v FROM b INTERSECT SELECT v FROM c)");
        assert_eq!(r.rows_affected, 0);
        // EXCEPT: the ids {1, 5, 6} of the remaining rows minus b's {2, 3}
        // are {1, 5, 6}, and the remaining values are exactly those.
        let r = db.write("DELETE FROM a WHERE v IN (SELECT id FROM a EXCEPT SELECT v FROM b)");
        assert_eq!(r.rows_affected, 3);
        assert_eq!(run_tups(&db, "SELECT v FROM a"), Vec::<Tup>::new());
        db.cleanup();
    }

    #[test]
    fn update_where_and_set_accept_set_operation_subqueries() {
        let db = rel_db("e7_update");
        load(&db, "a", &some(&[1, 2, 3, 4]));
        load(&db, "b", &some(&[2, 3]));
        load(&db, "c", &some(&[3, 4]));
        let r = db.write("UPDATE a SET v = v + 100 WHERE v IN (SELECT v FROM b INTERSECT SELECT v FROM c)");
        assert_eq!(r.rows_affected, 1);
        assert_eq!(sorted(run_tups(&db, "SELECT v FROM a")), col(&some(&[1, 2, 4, 103])));
        // SET (set-operation subquery): every row gets the single value.
        let r = db.write("UPDATE a SET v = (SELECT v FROM b EXCEPT SELECT v FROM c)");
        assert_eq!(r.rows_affected, 4);
        assert_eq!(sorted(run_tups(&db, "SELECT v FROM a")), col(&some(&[2, 2, 2, 2])));
        // A scalar set operation returning more than one row changes nothing.
        let before = sorted(run_tups(&db, "SELECT id, v FROM a"));
        let err = db
            .try_write("UPDATE a SET v = (SELECT v FROM b UNION SELECT v FROM c)")
            .unwrap_err();
        assert!(err.to_string().contains("more than one row"), "{err}");
        assert_eq!(sorted(run_tups(&db, "SELECT id, v FROM a")), before);
        db.cleanup();
    }

    // ---- operands of any shape ---------------------------------------------------------------

    #[test]
    fn operands_may_group_distinct_order_and_limit_on_their_own() {
        let db = rel_db("e7_operand_shapes");
        load(&db, "a", &some(&[1, 1, 2, 2, 2, 3]));
        load(&db, "b", &some(&[7, 7, 8]));
        assert_eq!(
            sorted(run_tups(
                &db,
                "SELECT v FROM a GROUP BY v HAVING COUNT(*) > 1 UNION ALL SELECT DISTINCT v FROM b"
            )),
            col(&some(&[1, 2, 7, 8]))
        );
        assert_eq!(
            run_tups(
                &db,
                "(SELECT v FROM a ORDER BY v DESC LIMIT 2) UNION ALL (SELECT v FROM b ORDER BY v LIMIT 1) ORDER BY v"
            ),
            col(&some(&[2, 3, 7]))
        );
        db.cleanup();
    }

    #[test]
    fn operands_that_contain_subqueries_derived_tables_and_ctes_mix_all_the_features() {
        let (db, t, orders) = standard_db("e7_mixed");
        // operand 1: a CTE read inside an IN subquery; operand 2: a derived
        // table with an aggregate; operand 3: a correlated scalar subquery.
        let sql = "WITH small AS (SELECT id FROM t WHERE id < 6) \
                   SELECT id FROM small WHERE id IN (SELECT t_id FROM orders) \
                   UNION ALL SELECT d.x FROM (SELECT MAX(id) AS x FROM t) AS d \
                   UNION SELECT (SELECT COUNT(*) FROM orders WHERE orders.t_id = t.id) FROM t WHERE t.id <= 3";
        let got = sorted(tups(&db.rows(sql)));
        // Reference, computed from the fixture data directly.
        let has_orders = |id: i32| orders.iter().any(|o: &ORow| o.3 == Some(id));
        let mut part1: Vec<Tup> = t
            .iter()
            .filter(|r| r.0 < 6 && has_orders(r.0))
            .map(|r| vec![Some(i64::from(r.0))])
            .collect();
        part1.push(vec![Some(i64::from(t.iter().map(|r| r.0).max().unwrap()))]);
        let part3: Vec<Tup> = (1..=3)
            .map(|id| vec![Some(orders.iter().filter(|o| o.3 == Some(id)).count() as i64)])
            .collect();
        let want = sorted(model("UNION", false, &part1, &part3));
        assert_eq!(got, want);
        db.cleanup();
    }

    #[test]
    fn parameters_are_numbered_across_all_the_operands() {
        let db = rel_db("e7_params");
        load(&db, "a", &some(&[1, 2, 3]));
        load(&db, "b", &some(&[3, 4, 5]));
        let r = run_params(
            &db,
            "SELECT v FROM a WHERE v >= $1 UNION SELECT v FROM b WHERE v <= $2 ORDER BY v",
            &[int_v(2), int_v(4)],
        );
        assert_eq!(tups(&r.rows), col(&some(&[2, 3, 4])));
        // A parameter in the LIMIT of the whole operation.
        let r = run_params(
            &db,
            "SELECT v FROM a UNION ALL SELECT v FROM b ORDER BY v LIMIT $1",
            &[int_v(2)],
        );
        assert_eq!(tups(&r.rows), col(&some(&[1, 2])));
        db.cleanup();
    }

    fn int_v(n: i32) -> V {
        Some(RelationalValue::Integer(n))
    }

    #[test]
    fn a_parameter_of_the_wrong_type_for_the_unified_column_is_an_execution_error() {
        let db = rel_db("e7_param_type");
        load(&db, "a", &some(&[1]));
        let sql = "SELECT $1 UNION ALL SELECT v FROM a";
        // An INTEGER parameter fits the unified INTEGER column.
        assert_eq!(tups(&run_params(&db, sql, &[int_v(7)]).rows), col(&some(&[7, 1])));
        // A TEXT parameter does not, and says so as an execution error.
        let err = db
            .try_run_with(sql, &ExecLimits::default(), &ExecMetrics::default(), &[Some(RelationalValue::Text("x".into()))])
            .unwrap_err();
        assert!(matches!(err, SqlError::ExecutionParameter { .. }), "{err:?}");
        assert!(err.to_string().contains("set operation"), "{err}");
        db.cleanup();
    }

    // ---- a forbidden table is a 404-class error at bind time -------------------------------

    #[test]
    fn a_forbidden_table_in_any_branch_stops_the_statement_before_anything_runs() {
        use crate::auth::AuthContext;
        use rubixdb::catalog::schema::{ObjectKind, Privilege};
        let db = rel_db("e7_authz");
        load(&db, "a", &some(&[1]));
        let id = db.table_id("a");
        db.catalog
            .grant("limited", ObjectKind::Table, id, Privilege::Select)
            .unwrap();
        let auth = AuthContext::none("limited");
        let err = db
            .try_bind_with(
                "SELECT v FROM a UNION ALL SELECT v FROM a UNION ALL SELECT v FROM b UNION ALL SELECT v FROM a",
                &auth,
                &crate::limits::SqlLimits::default(),
            )
            .unwrap_err();
        assert!(matches!(err, SqlError::UnknownObject { kind: "table", .. }), "{err:?}");
        db.cleanup();
    }
}

// =======================================================================
// Resource limits: operand count, and what a set operation holds
// =======================================================================

mod limits {
    use super::*;
    use crate::error::SqlError;
    use crate::exec::{ExecLimits, ExecMetrics};

    fn chain(n: usize, op: &str) -> String {
        (0..n).map(|_| "SELECT v FROM a").collect::<Vec<_>>().join(&format!(" {op} "))
    }

    #[test]
    fn the_default_allows_32_operands_and_refuses_33_with_resource_limit() {
        let db = rel_db("e7_limit_default");
        load(&db, "a", &some(&[1]));
        for op in ["UNION ALL", "UNION", "INTERSECT", "EXCEPT ALL"] {
            let ok = db.try_run(&chain(32, op));
            assert!(ok.is_ok(), "{op}: 32 operands must run: {ok:?}");
            let err = db.try_run(&chain(33, op)).unwrap_err();
            assert!(matches!(err, SqlError::ResourceLimit { .. }), "{op}: {err:?}");
            assert!(err.to_string().contains("max_set_op_branches"), "{err}");
        }
        assert_eq!(ExecLimits::default().max_set_op_branches, 32);
        db.cleanup();
    }

    #[test]
    fn the_count_is_per_statement_across_every_set_operation_in_it() {
        let db = rel_db("e7_limit_across");
        load(&db, "a", &some(&[1]));
        let limits = ExecLimits {
            max_set_op_branches: 6,
            ..ExecLimits::default()
        };
        let run = |sql: &str| db.try_run_with(sql, &limits, &ExecMetrics::default(), &[]);
        // 3 + 3 operands in two subqueries: exactly 6.
        let six = format!(
            "SELECT v FROM a WHERE v IN ({}) AND v IN ({})",
            chain(3, "UNION"),
            chain(3, "UNION")
        );
        assert!(run(&six).is_ok());
        // 3 + 4: one over.
        let seven = format!(
            "SELECT v FROM a WHERE v IN ({}) AND v IN ({})",
            chain(3, "UNION"),
            chain(4, "UNION")
        );
        let err = run(&seven).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        // The same total through a derived table and a CTE.
        let mixed = format!(
            "WITH u AS ({}) SELECT v FROM u WHERE v IN (SELECT v FROM ({}) AS d)",
            chain(3, "UNION ALL"),
            chain(4, "UNION ALL")
        );
        assert!(matches!(run(&mixed).unwrap_err(), SqlError::ResourceLimit { .. }));
        // A CTE read twice counts once.
        let twice = format!("WITH u AS ({}) SELECT x.v FROM u x JOIN u y ON x.v = y.v", chain(6, "UNION ALL"));
        assert!(run(&twice).is_ok());
        db.cleanup();
    }

    #[test]
    fn the_branches_metric_counts_operands_once_per_statement_however_often_they_run() {
        let db = rel_db("e7_limit_metric");
        load(&db, "a", &some(&[1, 2, 3]));
        let metrics = ExecMetrics::default();
        // The correlated operand pair runs once per row of `a` (3 times) but is
        // 2 operands in the statement.
        db.try_run_with(
            "SELECT (SELECT x.v FROM a x WHERE x.v = a.v UNION SELECT y.v FROM a y WHERE y.v = a.v) FROM a",
            &ExecLimits::default(),
            &metrics,
            &[],
        )
        .unwrap();
        assert_eq!(metrics.snapshot().set_op_branches_total, 2);
        // Precedence: 3 operands in one tree.
        let metrics = ExecMetrics::default();
        db.try_run_with(
            "SELECT v FROM a UNION SELECT v FROM a INTERSECT SELECT v FROM a",
            &ExecLimits::default(),
            &metrics,
            &[],
        )
        .unwrap();
        assert_eq!(metrics.snapshot().set_op_branches_total, 3);
        // A statement with no set operation records none.
        let metrics = ExecMetrics::default();
        db.try_run_with("SELECT v FROM a", &ExecLimits::default(), &metrics, &[]).unwrap();
        assert_eq!(metrics.snapshot().set_op_branches_total, 0);
        db.cleanup();
    }

    #[test]
    fn a_long_chain_within_the_parse_limit_but_over_the_operand_limit_fails_cleanly() {
        let db = rel_db("e7_limit_long");
        load(&db, "a", &some(&[1]));
        // 120 operands: parses and binds (the left-nested tree is built without
        // recursion), planning and the limit check refuse it -- no stack overflow.
        let err = db.try_run(&chain(120, "UNION ALL")).unwrap_err();
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
        db.cleanup();
    }

    #[test]
    fn update_and_delete_are_held_to_the_same_operand_limit() {
        let db = rel_db("e7_limit_dml");
        load(&db, "a", &some(&[1, 2, 3]));
        let ok = format!("DELETE FROM a WHERE v IN ({})", chain(32, "UNION"));
        assert_eq!(db.write(&ok).rows_affected, 3);
        load(&db, "a", &some(&[1, 2, 3]));
        for sql in [
            format!("DELETE FROM a WHERE v IN ({})", chain(33, "UNION")),
            format!("UPDATE a SET v = 0 WHERE v IN ({})", chain(33, "UNION")),
            format!("UPDATE a SET v = (SELECT MAX(v) FROM ({}) AS u)", chain(33, "UNION ALL")),
        ] {
            let err = db.try_write(&sql).unwrap_err();
            assert!(matches!(err, SqlError::ResourceLimit { .. }), "{sql}: {err:?}");
        }
        // Nothing was changed by the refused statements.
        assert_eq!(run_tups(&db, "SELECT COUNT(*) FROM a"), vec![vec![Some(3)]]);
        db.cleanup();
    }

    #[test]
    fn a_set_operation_inside_a_deadline_stops_with_the_deadline_error() {
        let db = rel_db("e7_deadline");
        let many: Vec<Option<i64>> = (0..300).map(Some).collect();
        load(&db, "a", &many);
        load(&db, "b", &many);
        let limits = ExecLimits {
            deadline: Some(std::time::Duration::from_nanos(1)),
            ..ExecLimits::default()
        };
        for op in ["UNION", "INTERSECT ALL", "EXCEPT"] {
            let err = db
                .try_run_with(&format!("SELECT v FROM a {op} SELECT v FROM b"), &limits, &ExecMetrics::default(), &[])
                .unwrap_err();
            assert!(matches!(err, SqlError::DeadlineExceeded), "{op}: {err:?}");
        }
        db.cleanup();
    }
}
