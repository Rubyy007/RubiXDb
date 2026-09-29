//! `rubixdb>` interactive mode — item 36: input, statement termination,
//! meta-command handling, SQL submission, result/error rendering,
//! Ctrl-C, EOF.

use std::io::Write;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use crate::client::Connection;
use crate::runner::{run_line, LineOutcome};
use crate::sql_split::{has_complete_statement, split_statements};

const PROMPT: &str = "rubixdb> ";
const CONTINUATION_PROMPT: &str = "     ...> ";

/// Runs the interactive loop until `\q`, Ctrl-D (EOF), or an
/// unrecoverable line-editor error. Returns the process exit code.
pub fn run(conn: &mut Connection) -> i32 {
    let mut editor = match DefaultEditor::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("rubixdb: could not start the interactive line editor: {e}");
            return 1;
        }
    };

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut buffer = String::new();

    loop {
        let prompt = if buffer.is_empty() {
            PROMPT
        } else {
            CONTINUATION_PROMPT
        };
        match editor.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if buffer.is_empty() && trimmed.starts_with('\\') {
                    let _ = editor.add_history_entry(&line);
                    match run_line(conn, trimmed, &mut out) {
                        LineOutcome::Ok | LineOutcome::Err => continue,
                        LineOutcome::Quit => return 0,
                    }
                }
                if trimmed.is_empty() && buffer.is_empty() {
                    continue;
                }
                buffer.push_str(&line);
                buffer.push('\n');
                if has_complete_statement(&buffer) {
                    let _ = editor.add_history_entry(buffer.trim());
                    for stmt in split_statements(&buffer) {
                        match run_line(conn, &stmt, &mut out) {
                            LineOutcome::Ok | LineOutcome::Err => {}
                            LineOutcome::Quit => return 0,
                        }
                    }
                    buffer.clear();
                }
            }
            // item 36: Ctrl-C clears the current (possibly multi-line,
            // possibly partially-typed) input and returns to a fresh
            // prompt -- it does not exit the CLI, matching every
            // common SQL REPL's own convention.
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                let _ = writeln!(out, "^C");
                continue;
            }
            // item 36: EOF (Ctrl-D) exits cleanly.
            Err(ReadlineError::Eof) => return 0,
            Err(e) => {
                eprintln!("rubixdb: input error: {e}");
                return 1;
            }
        }
    }
}
