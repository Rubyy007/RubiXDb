//! Resource limit boundary tests — item 34/9: N-1 succeeds, N fails with
//! a typed `ResourceLimit` error, never a panic/hang/unbounded
//! allocation. Mirrors the core crate's own established `oversized_
//! value_is_rejected`-style boundary-test methodology.

use crate::error::SqlError;
use crate::limits::SqlLimits;
use crate::parse::{parse_script, parse_statement};

#[test]
fn statement_byte_size_boundary() {
    let limits = SqlLimits {
        max_statement_bytes: 32,
        ..SqlLimits::default()
    };
    let ok = "SELECT 1 FROM t"; // well under 32 bytes
    assert!(parse_statement(ok, &limits).is_ok());
    let too_big = format!("SELECT {} FROM t", "1".repeat(64));
    let err = parse_statement(&too_big, &limits).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

#[test]
fn statements_per_script_boundary() {
    let limits = SqlLimits {
        max_statements_per_script: 3,
        ..SqlLimits::default()
    };
    let three = "SELECT 1 FROM t; SELECT 2 FROM t; SELECT 3 FROM t;";
    assert!(parse_script(three, &limits).is_ok());
    let four = "SELECT 1 FROM t; SELECT 2 FROM t; SELECT 3 FROM t; SELECT 4 FROM t;";
    let err = parse_script(four, &limits).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

#[test]
fn expression_nesting_depth_boundary_never_panics_or_hangs() {
    let limits = SqlLimits {
        max_expression_depth: 20,
        ..SqlLimits::default()
    };
    // Deliberately adversarial: deep unary-minus nesting, the classic
    // stack-overflow-shaped input (item 9/10).
    let deep = format!("SELECT {}1 FROM t", "-".repeat(1000));
    let result = parse_statement(&deep, &limits);
    assert!(result.is_err(), "must be rejected, not silently accepted");
    match result.unwrap_err() {
        SqlError::ResourceLimit { .. } | SqlError::Parse { .. } => {}
        other => panic!("expected a controlled error, got {other:?}"),
    }
}

#[test]
fn deeply_nested_parens_are_rejected_not_a_stack_overflow() {
    let limits = SqlLimits {
        max_expression_depth: 50,
        ..SqlLimits::default()
    };
    let mut sql = "SELECT ".to_string();
    sql.push_str(&"(".repeat(10_000));
    sql.push('1');
    sql.push_str(&")".repeat(10_000));
    sql.push_str(" FROM t");
    // The process must still be alive to make this assertion at all --
    // a stack overflow would abort the test binary, not return Err.
    let result = parse_statement(&sql, &limits);
    assert!(result.is_err());
}

#[test]
fn identifier_length_boundary() {
    let limits = SqlLimits {
        max_identifier_len: 8,
        ..SqlLimits::default()
    };
    let ok = format!("SELECT {} FROM t", "a".repeat(8));
    assert!(parse_statement(&ok, &limits).is_ok());
    let too_long = format!("SELECT {} FROM t", "a".repeat(9));
    let err = parse_statement(&too_long, &limits).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

#[test]
fn in_list_element_count_boundary() {
    let limits = SqlLimits {
        max_list_elements: 5,
        ..SqlLimits::default()
    };
    let ok = "SELECT id FROM t WHERE id IN (1,2,3,4,5)";
    assert!(parse_statement(ok, &limits).is_ok());
    let too_many = "SELECT id FROM t WHERE id IN (1,2,3,4,5,6)";
    let err = parse_statement(too_many, &limits).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

#[test]
fn values_row_count_boundary() {
    let limits = SqlLimits {
        max_values_rows: 2,
        ..SqlLimits::default()
    };
    let ok = "INSERT INTO t (id) VALUES (1), (2)";
    assert!(parse_statement(ok, &limits).is_ok());
    let too_many = "INSERT INTO t (id) VALUES (1), (2), (3)";
    let err = parse_statement(too_many, &limits).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

#[test]
fn insert_column_count_boundary() {
    let limits = SqlLimits {
        max_columns: 2,
        ..SqlLimits::default()
    };
    let ok = "INSERT INTO t (a, b) VALUES (1, 2)";
    assert!(parse_statement(ok, &limits).is_ok());
    let too_many = "INSERT INTO t (a, b, c) VALUES (1, 2, 3)";
    let err = parse_statement(too_many, &limits).unwrap_err();
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

// `max_parameters`'s own boundary (N-1 succeeds, N fails) is exercised
// in `bind_tests` (`parameter_index_exceeding_max_parameters_is_
// rejected`) -- it is enforced at bind time (`ExprBinder::bind_
// parameter`), not during parsing/conversion, since only binding
// resolves a `$n` reference into anything the limit is checked against.

// ---------------------------------------------------------------------------
// Final single-node certification, defect D-1: the pre-parse operator-chain
// guard must count real operator *tokens*, not operator characters that
// merely appear inside string literals / quoted identifiers / comments.
// Before the fix, a 260-row bulk INSERT of ISO-date strings, or one 40 KB
// hyphenated text value, was refused with a misleading `ResourceLimit`.
// ---------------------------------------------------------------------------

#[test]
fn operator_characters_inside_string_literals_do_not_trip_the_chain_guard() {
    let limits = SqlLimits::default();
    // 600 rows of ISO-date strings: 1,200 raw '-' characters, zero operators.
    let rows: Vec<String> = (0..600).map(|i| format!("({i}, '2026-10-02')")).collect();
    let sql = format!("INSERT INTO ev (id, d) VALUES {}", rows.join(", "));
    assert!(parse_statement(&sql, &limits).is_ok());

    // One long hyphenated / markup / URL text value (thousands of raw operator chars).
    for body in [
        "well-known state-of-the-art ".repeat(1_500),
        "<b>".repeat(10_000),
        "https://ex.com/a-b/c?x=1&y=2 ".repeat(500),
        "a AND b OR NOT c ".repeat(1_000),
    ] {
        let sql = format!("INSERT INTO t (id, v) VALUES (1, '{body}')");
        assert!(
            parse_statement(&sql, &limits).is_ok(),
            "literal content must never count as an operator chain"
        );
    }

    // Quoted identifiers and comments are not operators either.
    // 100 quoted identifiers x 7 operator characters = 700 raw operator chars (> budget 512).
    let idents: Vec<String> = (0..100).map(|_| "\"a-b+c*d/e=f\"".to_string()).collect();
    let ident = format!("SELECT {} FROM t", idents.join(", "));
    assert!(parse_statement(&ident, &limits).is_ok());
    let line_comment = format!("SELECT 1 FROM t -- {}", "+ - * / = < > ".repeat(500));
    assert!(parse_statement(&line_comment, &limits).is_ok());
    let block_comment = format!("SELECT 1 /* {} */ FROM t", "+ - * / = < > ".repeat(500));
    assert!(parse_statement(&block_comment, &limits).is_ok());
}

#[test]
fn real_operator_chains_are_still_rejected_even_when_mixed_with_literals() {
    // Default budget = 128 * 4 = 512 operators.
    let limits = SqlLimits::default();
    // Genuine flat chains of every counted operator family.
    for chain in [
        " + 1".repeat(600),
        " - 1".repeat(600),
        " * 1".repeat(600),
        " AND 1 = 1".repeat(300),
        " OR 1 < 2".repeat(300),
        " <= 1".repeat(300),
    ] {
        let sql = format!("SELECT 1{chain} FROM t");
        let err = parse_statement(&sql, &limits)
            .expect_err("a genuine operator chain over budget must be rejected");
        assert!(matches!(err, SqlError::ResourceLimit { .. }), "{err:?}");
    }
    // A chain hidden *after* literals that carry operator characters is still counted.
    let sql = format!(
        "SELECT '{}' {} FROM t",
        "2026-10-02 ".repeat(200),
        " + 1".repeat(600)
    );
    assert!(matches!(
        parse_statement(&sql, &limits).unwrap_err(),
        SqlError::ResourceLimit { .. }
    ));
    // Quote/comment tricks must not hide a chain from the guard: an escaped
    // quote ('') keeps the literal open exactly as the parser sees it, and a
    // block comment ends at its first `*/`.
    let tricky = format!("SELECT 'it''s' /* x */ 1{} FROM t", " + 1".repeat(600));
    assert!(matches!(
        parse_statement(&tricky, &limits).unwrap_err(),
        SqlError::ResourceLimit { .. }
    ));
    let after_comment = format!("SELECT 1 /* a */{} FROM t", " + 1".repeat(600));
    assert!(matches!(
        parse_statement(&after_comment, &limits).unwrap_err(),
        SqlError::ResourceLimit { .. }
    ));
}

#[test]
fn an_untokenizable_over_budget_statement_is_still_rejected_conservatively() {
    // An unterminated literal that is also over the raw budget keeps the
    // pre-fix conservative `ResourceLimit` (the parser never runs).
    let limits = SqlLimits::default();
    let sql = format!("SELECT '{}", "-".repeat(2_000));
    assert!(matches!(
        parse_statement(&sql, &limits).unwrap_err(),
        SqlError::ResourceLimit { .. }
    ));
}

#[test]
fn literal_heavy_statement_just_under_the_byte_limit_parses_within_bounded_time() {
    // The slow path (tokenize) runs on a near-max-size, operator-dense literal;
    // it must stay linear and fast.
    let limits = SqlLimits::default();
    let body = "-+*/=<>".repeat((limits.max_statement_bytes - 64) / 7);
    let sql = format!("SELECT '{body}'");
    assert!(sql.len() <= limits.max_statement_bytes);
    let start = std::time::Instant::now();
    assert!(parse_statement(&sql, &limits).is_ok());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "tokenizing refinement must be linear: {:?}",
        start.elapsed()
    );
}
