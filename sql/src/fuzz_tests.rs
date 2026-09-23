//! Parser/binder fuzz & property tests — item 10/41: arbitrary hostile
//! input must produce a successful parse or a controlled error, **never**
//! a panic, abort, stack overflow, hang, or unbounded allocation.
//! Deterministic, bounded runs (`proptest`'s own default case count),
//! matching this project's established fuzz-testing methodology
//! (`RELATIONAL ADR AMENDMENT 001`'s `group_decode_rejects_oversized_
//! member_count_gracefully`-style discipline, one layer up).

use proptest::prelude::*;

use crate::error::SqlError;
use crate::limits::SqlLimits;
use crate::parse::parse_statement;

fn limits() -> SqlLimits {
    // A tight expression-depth bound so a proptest-generated deeply
    // nested input is rejected quickly rather than spending the fuzz
    // budget on legitimate-but-slow parsing.
    SqlLimits {
        max_expression_depth: 64,
        ..SqlLimits::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Arbitrary UTF-8 text, unconstrained — the rawest possible hostile
    /// input at this boundary.
    #[test]
    fn arbitrary_utf8_never_panics(sql in ".{0,200}") {
        let _ = parse_statement(&sql, &limits());
    }

    /// Arbitrary bytes reinterpreted as (possibly invalid) UTF-8, via
    /// lossy conversion -- exercises the tokenizer against byte
    /// sequences a naive fuzzer would actually generate.
    #[test]
    fn arbitrary_bytes_as_lossy_utf8_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
        let sql = String::from_utf8_lossy(&bytes);
        let _ = parse_statement(&sql, &limits());
    }

    /// SQL-keyword-biased random text -- more likely to reach deeper
    /// parser states than pure noise, still never valid SQL.
    #[test]
    fn keyword_biased_noise_never_panics(
        tokens in proptest::collection::vec(
            prop_oneof![
                Just("SELECT".to_string()), Just("FROM".to_string()), Just("WHERE".to_string()), Just("JOIN".to_string()),
                Just("(".to_string()), Just(")".to_string()), Just(",".to_string()), Just("=".to_string()), Just("'".to_string()),
                Just("--".to_string()), Just(";".to_string()), Just("NULL".to_string()), Just("$1".to_string()), Just("*".to_string()),
                Just("CREATE".to_string()), Just("TABLE".to_string()), Just("DROP".to_string()), Just("INDEX".to_string()),
                "[a-zA-Z0-9_]{0,10}",
            ],
            0..30,
        )
    ) {
        let sql = tokens.join(" ");
        let _ = parse_statement(&sql, &limits());
    }

    /// Deeply right-nested unary `NOT`/parens -- the canonical stack-
    /// depth attack shape -- at a range of depths, always either parses
    /// (under the limit) or fails with a controlled error (over it),
    /// never crashes the process.
    #[test]
    fn deep_unary_not_nesting_never_overflows_the_stack(depth in 0usize..5000) {
        let sql = format!("SELECT {}TRUE FROM t", "NOT ".repeat(depth));
        let _ = parse_statement(&sql, &limits());
    }

    /// A random number of parameter placeholders with random (possibly
    /// huge, possibly zero, possibly non-numeric-looking) indices.
    #[test]
    fn arbitrary_parameter_indices_never_panic(idx in "[0-9]{0,20}") {
        let sql = format!("SELECT id FROM t WHERE id = ${idx}");
        let _ = parse_statement(&sql, &limits());
    }

    /// Malformed numeric/decimal-shaped literals.
    #[test]
    fn malformed_numeric_literals_never_panic(text in "[0-9.eE+-]{0,30}") {
        let sql = format!("SELECT {text} FROM t");
        let _ = parse_statement(&sql, &limits());
    }

    /// Malformed typed-string literals (DATE/TIME/TIMESTAMP with
    /// arbitrary text) -- this crate's own `temporal` parsing is new
    /// code and gets independent fuzz coverage here.
    #[test]
    fn malformed_typed_literals_never_panic(text in "[0-9:\\-. TZ]{0,30}") {
        for kind in ["DATE", "TIME", "TIMESTAMP"] {
            let sql = format!("SELECT {kind} '{text}' FROM t");
            let _ = parse_statement(&sql, &limits());
        }
    }
}

#[test]
fn deeply_nested_binary_expression_is_rejected_not_a_stack_overflow() {
    // A left-associative chain of 20,000 `+`s -- this exact shape
    // reproducibly crashed the process with a stack overflow before
    // `parse::reject_pathological_operator_chains` was added (the crash
    // is in the third-party parse tree's own recursive `Drop`, not in
    // this crate's code or in `sqlparser`'s own parse-time recursion
    // guard) -- the regression test for that finding.
    let mut sql = "SELECT 1".to_string();
    for _ in 0..20_000 {
        sql.push_str(" + 1");
    }
    sql.push_str(" FROM t");
    let limits = SqlLimits {
        max_expression_depth: 100,
        max_statement_bytes: 10 * 1024 * 1024,
        ..SqlLimits::default()
    };
    let err = parse_statement(&sql, &limits)
        .expect_err("a 20,000-term chain must be rejected, not crash");
    assert!(matches!(err, SqlError::ResourceLimit { .. }));
}

#[test]
fn empty_and_whitespace_only_input_is_a_controlled_error() {
    for sql in ["", " ", "\n\t  \n", ";", "   ;   "] {
        assert!(parse_statement(sql, &limits()).is_err());
    }
}

#[test]
fn null_bytes_and_control_characters_never_panic() {
    let cases = [
        "SELECT \0 FROM t",
        "SELECT id FROM t\0\0\0",
        "SELECT id FROM t WHERE name = '\u{0}'",
        "\u{7}\u{8}\u{1b}SELECT id FROM t",
    ];
    for sql in cases {
        let _ = parse_statement(sql, &limits());
    }
}
