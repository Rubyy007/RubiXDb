//! The parser boundary — `sqlparser-rs` integration (D14). Untrusted SQL
//! text enters the system here and nowhere else; every resource limit
//! (item 9/34) is checked before the corresponding expensive step.
//!
//! Dialect choice: `GenericDialect`, not `PostgreSqlDialect` — D14
//! approves either; `GenericDialect` is the more conservative pick for
//! this crate's deliberately narrow approved grammar subset (`PHASE_
//! RELATIONAL_SQL_GRAMMAR.md`), since it does not additionally accept
//! Postgres-specific syntax extensions this crate would then have to
//! reject one-by-one at conversion time — every construct `PostgreSql
//! Dialect` would uniquely admit is exactly the kind of "vendor-specific
//! syntax we don't support" surface `GenericDialect` simply never
//! parses in the first place. The double-quoted-identifier / single-
//! quoted-string-literal split the Architecture doc's §0 asks for is
//! ANSI-standard behavior, not a Postgres-only extension, so
//! `GenericDialect` already provides it.

use sqlparser::dialect::GenericDialect;
use sqlparser::parser::{Parser, ParserError};

use crate::ast::Statement;
use crate::error::{Result, SqlError};
use crate::limits::SqlLimits;

fn map_parser_error(e: ParserError) -> SqlError {
    match e {
        ParserError::RecursionLimitExceeded => SqlError::ResourceLimit {
            detail: "expression/statement nesting exceeds the configured recursion limit"
                .to_string(),
        },
        ParserError::TokenizerError(detail) | ParserError::ParserError(detail) => {
            SqlError::Parse { detail }
        }
    }
}

/// Parses `sql` as a `;`-separated script (1 or more statements),
/// converts each to `crate::ast::Statement`, and returns them in order.
/// Every limit in `limits` is enforced before the work it bounds: total
/// byte length before tokenizing, expression/statement nesting depth
/// during parsing (`sqlparser`'s own `RecursionLimitExceeded`, backed by
/// `Parser::with_recursion_limit`) and again during conversion
/// (`crate::convert`'s own `DepthGuard`), and statement count before
/// per-statement conversion begins.
pub fn parse_script(sql: &str, limits: &SqlLimits) -> Result<Vec<Statement>> {
    if sql.len() > limits.max_statement_bytes {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "SQL text exceeds max_statement_bytes ({} > {})",
                sql.len(),
                limits.max_statement_bytes
            ),
        });
    }
    reject_pathological_operator_chains(sql, limits)?;

    let dialect = GenericDialect {};
    let raw_statements = Parser::new(&dialect)
        .with_recursion_limit(limits.max_expression_depth)
        .try_with_sql(sql)
        .map_err(map_parser_error)?
        .parse_statements()
        .map_err(map_parser_error)?;

    if raw_statements.len() > limits.max_statements_per_script {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "statement count exceeds max_statements_per_script ({} > {})",
                raw_statements.len(),
                limits.max_statements_per_script
            ),
        });
    }

    raw_statements
        .iter()
        .map(|s| crate::convert::convert_statement(s, limits))
        .collect()
}

/// A pre-tokenizer, pre-parser defense against a real, measured hazard:
/// `sqlparser`'s own `Parser::with_recursion_limit` protects its
/// *parsing* call stack, but a long **flat chain** of binary/unary
/// operators (`1 + 1 + 1 + ... + 1`, `a AND a AND a AND ...`) still
/// builds a correspondingly deep `Box<Expr>`-linked tree — and Rust's
/// ordinary recursive `Drop` for that tree overflows the stack when the
/// chain is dropped, *regardless of* whether parsing itself ever
/// recursed deeply or whether this crate's own `crate::convert::
/// DepthGuard` ever ran (both are irrelevant once the already-built
/// third-party tree is merely dropped). Confirmed by this crate's own
/// test suite: a 20,000-term chain reproducibly crashes the process with
/// a stack overflow, not a returned `Err`, through `sqlparser`'s
/// recursion-limit guard alone. This function closes it the only way
/// available without modifying `sqlparser` itself (item 9's explicit
/// instruction) — reject *before* the parser ever builds the tree.
///
/// A count of operator-shaped characters/keywords is a conservative
/// over-approximation of the resulting tree's possible depth for a flat
/// chain (every occurrence is a candidate chain link) — cheap (one
/// linear scan, no tokenizing), and only ever *more* likely to reject,
/// never to under-count, a pathological input. `limits.max_expression_
/// depth * PATHOLOGICAL_CHAIN_SAFETY_FACTOR` is generous relative to any
/// realistic hand-written query.
const PATHOLOGICAL_CHAIN_SAFETY_FACTOR: usize = 4;

fn reject_pathological_operator_chains(sql: &str, limits: &SqlLimits) -> Result<()> {
    let budget = limits
        .max_expression_depth
        .saturating_mul(PATHOLOGICAL_CHAIN_SAFETY_FACTOR);
    let symbol_count = sql
        .matches(['+', '-', '*', '/', '%', '=', '<', '>'])
        .count();
    let upper = sql.to_ascii_uppercase();
    let keyword_count =
        count_word(&upper, "AND") + count_word(&upper, "OR") + count_word(&upper, "NOT");
    if symbol_count.saturating_add(keyword_count) > budget {
        return Err(SqlError::ResourceLimit {
            detail: format!(
                "too many chained operators for max_expression_depth ({}); a long flat chain of \
                 binary/unary operators can build a stack-overflow-prone expression tree even \
                 when nesting is otherwise shallow",
                limits.max_expression_depth
            ),
        });
    }
    Ok(())
}

fn count_word(haystack: &str, word: &str) -> usize {
    let bytes = haystack.as_bytes();
    let wlen = word.len();
    let mut count = 0;
    let mut i = 0;
    while let Some(pos) = haystack[i..].find(word) {
        let start = i + pos;
        let end = start + wlen;
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            count += 1;
        }
        i = start + 1;
        if i >= haystack.len() {
            break;
        }
    }
    count
}

/// Parses exactly one statement — the common case (item 27 etc. all
/// speak in terms of "a statement"). Rejects empty input and more than
/// one statement with a plain, safe error (never silently takes "the
/// first" of several, which would hide a client's own script-boundary
/// mistake).
pub fn parse_statement(sql: &str, limits: &SqlLimits) -> Result<Statement> {
    let mut statements = parse_script(sql, limits)?;
    match statements.len() {
        0 => Err(SqlError::Parse {
            detail: "empty SQL text".to_string(),
        }),
        1 => Ok(statements.pop().expect("checked len 1")),
        n => Err(SqlError::ResourceLimit {
            detail: format!("expected exactly one statement, got {n}"),
        }),
    }
}
