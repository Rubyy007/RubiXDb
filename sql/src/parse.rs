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
            detail: "expression/statement nesting exceeds the configured recursion limit".to_string(),
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
