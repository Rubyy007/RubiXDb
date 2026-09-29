//! Statement-boundary splitting — the one piece of SQL *lexical*
//! awareness this CLI has (tracking single-quoted string literals so a
//! `;` inside `'foo;bar'` is never mistaken for a statement terminator)
//! — never SQL *semantics* (item 27's own "must NOT parse SQL semantics
//! itself"): this module has no notion of keywords, statement kinds, or
//! validity, and never rejects anything itself. The server's own
//! `parse_statement` already refuses multi-statement input in one
//! request (`sql/src/parse_tests.rs::parse_statement_rejects_more_
//! than_one_statement`), so script mode (`-f`, multiple `;`-terminated
//! statements in one file) has no choice but to split client-side
//! before submitting each statement as its own `POST /v1/sql` call —
//! exactly what `psql`/`mysql`/`sqlite3`'s own CLIs do for the
//! identical reason, not a RubiXDB-specific SQL engine.
//!
//! Quoting rule matched here: a single quote toggles "inside a string
//! literal" state; `''` inside a literal is the standard SQL escaped-
//! quote sequence (`sql/src/parse_tests.rs::escaped_single_quote_in_
//! string_literal_round_trips` confirms this is what the real grammar
//! accepts) and is treated as two toggles that cancel out, i.e. stays
//! inside the literal.

/// Splits `script` into individual statement texts on top-level (non-
/// quoted) `;` characters. Each returned statement has its trailing `;`
/// removed and is trimmed of leading/trailing whitespace; empty
/// statements (blank lines, trailing `;` with nothing after it) are
/// dropped.
pub fn split_statements(script: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_string = false;

    for c in script.chars() {
        match c {
            '\'' => {
                in_string = !in_string;
                current.push(c);
            }
            ';' if !in_string => {
                let stmt = current.trim().to_string();
                if !stmt.is_empty() {
                    statements.push(stmt);
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    let tail = current.trim().to_string();
    if !tail.is_empty() {
        statements.push(tail);
    }
    statements
}

/// REPL incremental use: `true` iff `buffer` (accumulated input so far)
/// contains at least one top-level `;` — i.e. a complete statement is
/// ready to submit. A bare backslash meta-command line never needs
/// this (handled as a single line unconditionally by the caller before
/// this function is ever consulted).
pub fn has_complete_statement(buffer: &str) -> bool {
    let mut in_string = false;
    for c in buffer.chars() {
        match c {
            '\'' => in_string = !in_string,
            ';' if !in_string => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple_statements() {
        let stmts = split_statements("SELECT 1; SELECT 2;");
        assert_eq!(stmts, vec!["SELECT 1".to_string(), "SELECT 2".to_string()]);
    }

    #[test]
    fn does_not_split_inside_a_string_literal() {
        let stmts = split_statements("INSERT INTO t (name) VALUES ('a;b'); SELECT 1;");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'a;b'"));
    }

    #[test]
    fn handles_escaped_quote_inside_a_literal() {
        let stmts = split_statements("SELECT 'it''s; fine';");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT 'it''s; fine'");
    }

    #[test]
    fn drops_empty_statements_and_trims_whitespace() {
        let stmts = split_statements("  ;\nSELECT 1 ;  \n\n;  SELECT 2  ");
        assert_eq!(stmts, vec!["SELECT 1".to_string(), "SELECT 2".to_string()]);
    }

    #[test]
    fn statement_without_trailing_semicolon_is_still_captured() {
        let stmts = split_statements("SELECT 1");
        assert_eq!(stmts, vec!["SELECT 1".to_string()]);
    }

    #[test]
    fn has_complete_statement_respects_quoting() {
        assert!(!has_complete_statement("SELECT 'a;b'"));
        assert!(has_complete_statement("SELECT 'a;b';"));
        assert!(has_complete_statement("SELECT 1;"));
    }
}
