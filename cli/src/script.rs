//! `-c "SQL"` / `-f script.sql` — item 38/39: exact `stdout`/`stderr`/
//! exit-code semantics, one server session per script run (`-f`'s own
//! multiple statements, including `BEGIN`/`COMMIT`, share the one
//! `Connection` this whole run uses — never a fresh session per
//! statement), and a fatal statement failure stops the script (no
//! silent continuation) with a non-zero exit code.

use std::io::Write;

use crate::client::Connection;
use crate::runner::{run_line, LineOutcome};
use crate::sql_split::split_statements;

/// Runs every statement/meta-command line in `script` against `conn`,
/// in order, on the **same** session for the whole call (item 39).
/// Stops at the first failure (item 38: "do not silently continue
/// after a fatal error") or at `\q`. Returns the process exit code:
/// `0` on full success (or a clean `\q`), `1` if any statement failed.
pub fn run_script(conn: &mut Connection, script: &str, out: &mut impl Write) -> i32 {
    let mut buffer = String::new();
    let mut in_string = false;

    for line in script.lines() {
        let trimmed_line = line.trim();
        if buffer.is_empty() && trimmed_line.starts_with('\\') {
            match run_line(conn, trimmed_line, out) {
                LineOutcome::Ok => continue,
                LineOutcome::Err => return 1,
                LineOutcome::Quit => return 0,
            }
        }
        for c in line.chars() {
            if c == '\'' {
                in_string = !in_string;
            }
            buffer.push(c);
        }
        buffer.push('\n');

        if !in_string && buffer.contains(';') {
            for stmt in split_statements(&buffer) {
                match run_line(conn, &stmt, out) {
                    LineOutcome::Ok => {}
                    LineOutcome::Err => return 1,
                    LineOutcome::Quit => return 0,
                }
            }
            buffer.clear();
        }
    }

    let tail = buffer.trim();
    if !tail.is_empty() {
        match run_line(conn, tail, out) {
            LineOutcome::Ok => {}
            LineOutcome::Err => return 1,
            LineOutcome::Quit => return 0,
        }
    }
    0
}

#[cfg(test)]
mod tests {
    // Integration-level coverage (real server) lives in
    // `cli/tests/cli_integration.rs`; this module's own unit tests
    // cover `split_statements`/`has_complete_statement` directly
    // (`sql_split.rs`) since `run_script` itself needs a real
    // `Connection` to exercise meaningfully.
}
