//! SQL resource limits — `PHASE_RELATIONAL_DATABASE_ADR.md` D27, applied
//! to the parser/binder boundary specifically (item 9/34 of the
//! governing directive). Every limit is enforced *before* the
//! corresponding expensive work, mirrors D27's existing defaults where
//! one already exists (statement size, parameter count), and adds the
//! parser/binder-specific limits D27 did not itself enumerate (nesting
//! depth, identifier length, statement count per script) since D27 was
//! written before a parser existed to need them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlLimits {
    /// D27: "Max SQL statement size" — 1 MiB, matching the existing
    /// `RUBIXDB_MAX_VALUE_BYTES`-class convention exactly.
    pub max_statement_bytes: usize,
    /// Checked against `sqlparser`'s own `Parser::with_recursion_limit`
    /// (backed by `RecursionCounter`, `ParserError::RecursionLimitExceeded`
    /// on overflow) — the parser boundary's own defense against deeply
    /// nested expressions (item 9/10), reused rather than re-implemented,
    /// plus this crate's own equal-or-tighter recursion check during
    /// AST conversion (`convert.rs`) and later during binding, since a
    /// third-party parser succeeding is not proof every later pass over
    /// the same tree is itself recursion-safe.
    pub max_expression_depth: usize,
    /// Longest a single (already-unescaped) identifier value may be.
    pub max_identifier_len: usize,
    /// Highest `$n` parameter index accepted (also bounds how many
    /// distinct parameters one statement may reference).
    pub max_parameters: usize,
    /// Most statements accepted from one parse call (`parse_script`) —
    /// a script/batch resource bound, distinct from statement size.
    pub max_statements_per_script: usize,
    /// Most columns one `SELECT` projection, `INSERT` column list, or
    /// `CREATE TABLE` column list may contain.
    pub max_columns: usize,
    /// Most elements one `IN (...)` list or one `VALUES (...)` row may
    /// contain.
    pub max_list_elements: usize,
    /// Most rows one `INSERT ... VALUES (...), (...), ...` may supply.
    pub max_values_rows: usize,
}

impl Default for SqlLimits {
    fn default() -> Self {
        SqlLimits {
            max_statement_bytes: 1024 * 1024,
            max_expression_depth: 128,
            max_identifier_len: 255,
            max_parameters: 10_000,
            max_statements_per_script: 1_000,
            max_columns: 1_600, // D27: "Max columns/table"
            max_list_elements: 10_000,
            max_values_rows: 10_000,
        }
    }
}
