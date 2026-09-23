//! Parser correctness — item 11: every supported statement family,
//! whitespace/comments/case/quoting variations, and the negative space
//! (invalid syntax, truncated input, unexpected tokens) never panicking.

use crate::ast::*;
use crate::error::SqlError;
use crate::limits::SqlLimits;
use crate::parse::{parse_script, parse_statement};

fn limits() -> SqlLimits {
    SqlLimits::default()
}

fn parse_ok(sql: &str) -> Statement {
    parse_statement(sql, &limits()).unwrap_or_else(|e| panic!("expected {sql:?} to parse, got {e}"))
}

fn parse_err(sql: &str) {
    assert!(
        parse_statement(sql, &limits()).is_err(),
        "expected {sql:?} to fail to parse/convert"
    );
}

#[test]
fn whitespace_and_case_variations_parse_identically() {
    let a = parse_ok("select id from t");
    let b = parse_ok("SELECT   id\nFROM\tt");
    let c = parse_ok("Select Id From T");
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn comments_are_ignored() {
    let a = parse_ok("SELECT id FROM t");
    let b = parse_ok("SELECT id -- trailing comment\nFROM t");
    let c = parse_ok("SELECT /* inline */ id FROM t");
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn quoted_identifiers_preserve_case_unquoted_fold_to_lowercase() {
    let Statement::Select(s) = parse_ok(r#"SELECT "MixedCase", plain FROM t"#) else {
        panic!("expected SELECT");
    };
    let ColumnRef { parts } = match &s.projection[0] {
        SelectItem::Item(item) => match &item.expr {
            Expr::Column(c) => c.clone(),
            _ => panic!("expected column"),
        },
        _ => panic!("expected item"),
    };
    assert_eq!(parts[0], Ident::new("MixedCase", true));
    let ColumnRef { parts } = match &s.projection[1] {
        SelectItem::Item(item) => match &item.expr {
            Expr::Column(c) => c.clone(),
            _ => panic!("expected column"),
        },
        _ => panic!("expected item"),
    };
    assert_eq!(parts[0], Ident::new("plain", false));
}

#[test]
fn escaped_single_quote_in_string_literal_round_trips() {
    let Statement::Select(s) = parse_ok("SELECT 'it''s' FROM t") else {
        panic!()
    };
    let SelectItem::Item(item) = &s.projection[0] else {
        panic!()
    };
    assert_eq!(item.expr, Expr::Literal(Literal::Text("it's".to_string())));
}

#[test]
fn null_boolean_and_number_literals() {
    let cases: &[(&str, Expr)] = &[
        ("SELECT NULL FROM t", Expr::Literal(Literal::Null)),
        ("SELECT TRUE FROM t", Expr::Literal(Literal::Boolean(true))),
        (
            "SELECT FALSE FROM t",
            Expr::Literal(Literal::Boolean(false)),
        ),
        (
            "SELECT 42 FROM t",
            Expr::Literal(Literal::Number {
                text: "42".to_string(),
                is_integer: true,
            }),
        ),
        (
            "SELECT -42 FROM t",
            Expr::Literal(Literal::Number {
                text: "-42".to_string(),
                is_integer: true,
            }),
        ),
        (
            "SELECT 3.14 FROM t",
            Expr::Literal(Literal::Number {
                text: "3.14".to_string(),
                is_integer: false,
            }),
        ),
        (
            "SELECT -9223372036854775808 FROM t",
            Expr::Literal(Literal::Number {
                text: "-9223372036854775808".to_string(),
                is_integer: true,
            }),
        ),
    ];
    for (sql, expected) in cases {
        let Statement::Select(s) = parse_ok(sql) else {
            panic!()
        };
        let SelectItem::Item(item) = &s.projection[0] else {
            panic!()
        };
        assert_eq!(&item.expr, expected, "sql: {sql}");
    }
}

#[test]
fn typed_date_time_timestamp_literals_parse() {
    let Statement::Select(s) = parse_ok(
        "SELECT DATE '2024-01-01', TIME '12:00:00', TIMESTAMP '2024-01-01 12:00:00' FROM t",
    ) else {
        panic!()
    };
    assert_eq!(s.projection.len(), 3);
    for item in &s.projection {
        let SelectItem::Item(item) = item else {
            panic!()
        };
        assert!(matches!(item.expr, Expr::Literal(Literal::Typed { .. })));
    }
}

#[test]
fn parameter_placeholders_parse_as_one_based_indices() {
    let Statement::Select(s) = parse_ok("SELECT id FROM t WHERE id = $1 AND name = $2") else {
        panic!()
    };
    let Some(Expr::BinaryOp {
        left,
        op: BinaryOp::And,
        right,
    }) = s.selection.as_ref()
    else {
        panic!()
    };
    let Expr::BinaryOp { right: p1, .. } = left.as_ref() else {
        panic!()
    };
    let Expr::BinaryOp { right: p2, .. } = right.as_ref() else {
        panic!()
    };
    assert_eq!(**p1, Expr::Parameter(1));
    assert_eq!(**p2, Expr::Parameter(2));
}

#[test]
fn nested_parens_and_precedence() {
    let Statement::Select(s) = parse_ok("SELECT (1 + 2) * 3 FROM t") else {
        panic!()
    };
    let SelectItem::Item(item) = &s.projection[0] else {
        panic!()
    };
    assert!(matches!(
        item.expr,
        Expr::BinaryOp {
            op: BinaryOp::Mul,
            ..
        }
    ));
}

#[test]
fn table_and_column_aliases() {
    let Statement::Select(s) = parse_ok("SELECT id AS my_id FROM t AS tbl") else {
        panic!()
    };
    let SelectItem::Item(item) = &s.projection[0] else {
        panic!()
    };
    assert_eq!(item.alias, Some(Ident::new("my_id", false)));
    assert_eq!(
        s.from.as_ref().unwrap().first.alias,
        Some(Ident::new("tbl", false))
    );
}

#[test]
fn schema_qualified_table_name() {
    let Statement::Select(s) = parse_ok("SELECT id FROM public.t") else {
        panic!()
    };
    let name = &s.from.as_ref().unwrap().first.name;
    assert_eq!(name.0.len(), 2);
    assert_eq!(name.0[0], Ident::new("public", false));
    assert_eq!(name.0[1], Ident::new("t", false));
}

#[test]
fn every_supported_statement_family_parses() {
    let cases = [
        "SELECT id FROM t",
        "SELECT id FROM t WHERE id > 1 ORDER BY id DESC LIMIT 10 OFFSET 5",
        "SELECT t.id, o.customer FROM t INNER JOIN orders o ON t.id = o.id",
        "SELECT t.id FROM t LEFT JOIN orders o ON t.id = o.id",
        "INSERT INTO t (id, name) VALUES (1, 'a')",
        "INSERT INTO t VALUES (1, 'a', TRUE), (2, 'b', FALSE)",
        "UPDATE t SET name = 'x' WHERE id = 1",
        "DELETE FROM t WHERE id = 1",
        "CREATE TABLE foo (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        "DROP TABLE foo",
        "DROP TABLE IF EXISTS foo",
        "CREATE INDEX idx ON t (name)",
        "CREATE UNIQUE INDEX idx ON t (name)",
        "DROP INDEX idx ON t",
        "CREATE SCHEMA s",
        "CREATE DATABASE d",
        "EXPLAIN SELECT id FROM t",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
    ];
    for sql in cases {
        parse_ok(sql);
    }
}

#[test]
fn invalid_syntax_is_rejected_not_panicked() {
    let cases = [
        "SELEC id FROM t",
        "SELECT FROM",
        "SELECT id FROM",
        "INSERT INTO",
        "CREATE TABLE",
        "",
        "SELECT id FROM t WHERE",
        ";;;",
        "SELECT ((((1",
    ];
    for sql in cases {
        parse_err(sql);
    }
}

#[test]
fn truncated_sql_is_a_controlled_error() {
    let full = "SELECT id, name FROM t WHERE id = 1 AND name = 'abc'";
    for end in 1..full.len() {
        // Never panics for any prefix -- the actual assertion.
        let _ = parse_statement(&full[..end], &limits());
    }
}

#[test]
fn unsupported_grammar_is_a_typed_error_not_a_panic() {
    let cases = [
        "SELECT id FROM t GROUP BY id",
        "SELECT id FROM t HAVING id > 1",
        "SELECT * FROM t, orders",
        "WITH x AS (SELECT 1) SELECT * FROM x",
        "SELECT id FROM t UNION SELECT id FROM orders",
        "SELECT (SELECT 1)",
        "SELECT id FROM t RIGHT JOIN orders o ON t.id = o.id",
        "SELECT id FROM t FULL JOIN orders o ON t.id = o.id",
    ];
    for sql in cases {
        match parse_statement(sql, &limits()) {
            Err(SqlError::Unsupported { .. }) => {}
            other => panic!("expected Unsupported for {sql:?}, got {other:?}"),
        }
    }
}

#[test]
fn multiple_statements_via_parse_script() {
    let stmts = parse_script("SELECT 1 FROM t; SELECT 2 FROM t;", &limits()).unwrap();
    assert_eq!(stmts.len(), 2);
}

#[test]
fn parse_statement_rejects_more_than_one_statement() {
    let err = parse_statement("SELECT 1 FROM t; SELECT 2 FROM t;", &limits()).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

#[test]
fn parse_statement_rejects_empty_input() {
    let err = parse_statement("", &limits()).unwrap_err();
    assert!(matches!(err, SqlError::Parse { .. }));
}

#[test]
fn hex_string_blob_literal_parses() {
    let Statement::Select(s) = parse_ok("SELECT X'DEADBEEF' FROM t") else {
        panic!()
    };
    let SelectItem::Item(item) = &s.projection[0] else {
        panic!()
    };
    assert_eq!(
        item.expr,
        Expr::Literal(Literal::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]))
    );
}
