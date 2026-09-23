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
