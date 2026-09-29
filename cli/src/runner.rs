//! One SQL statement or meta-command, submitted and rendered — the
//! single code path both `repl.rs` (interactive) and `script.rs`
//! (`-c`/`-f`) drive, so the two modes can never silently diverge in
//! what a given line of input does (item 39: a script's `BEGIN`/
//! `COMMIT` must behave exactly like the same lines typed interactively
//! into the same session).

use std::io::Write;

use serde_json::Value;

use crate::client::{CliError, Connection};
use crate::meta::{self, MetaOutcome};
use crate::render::{render_table, sanitize_for_terminal, value_to_display};

pub enum LineOutcome {
    Ok,
    Err,
    Quit,
}

pub fn run_line(conn: &mut Connection, line: &str, out: &mut impl Write) -> LineOutcome {
    let trimmed = line.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return LineOutcome::Ok;
    }
    if let Some(rest) = line.trim().strip_prefix('\\') {
        return match meta::handle(conn, rest, out) {
            MetaOutcome::Quit => LineOutcome::Quit,
            MetaOutcome::Ok => LineOutcome::Ok,
            MetaOutcome::Err => LineOutcome::Err,
        };
    }
    match conn.execute(trimmed) {
        Ok(resp) => {
            render_sql_result(&resp.result, out);
            LineOutcome::Ok
        }
        Err(e) => {
            if let CliError::Api { code, .. } = &e {
                if code == "SESSION_NOT_FOUND" {
                    conn.forget_session();
                }
            }
            let _ = writeln!(out, "ERROR: {e}");
            LineOutcome::Err
        }
    }
}

fn render_sql_result(result: &Value, out: &mut impl Write) {
    let kind = result.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "rows" => {
            let columns: Vec<String> = result
                .get("columns")
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .map(|c| {
                            c.get("name")
                                .and_then(Value::as_str)
                                .map(sanitize_for_terminal)
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let rows: Vec<Vec<String>> = result
                .get("rows")
                .and_then(Value::as_array)
                .map(|rs| {
                    rs.iter()
                        .map(|r| {
                            r.as_array()
                                .map(|cells| cells.iter().map(value_to_display).collect())
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let row_count = result.get("row_count").and_then(Value::as_u64).unwrap_or(0);
            if columns.is_empty() {
                let _ = writeln!(out, "({row_count} rows)");
            } else {
                let _ = writeln!(out, "{}", render_table(&columns, &rows));
                let _ = writeln!(out, "({row_count} rows)");
            }
        }
        "write" => {
            let statement = result
                .get("statement")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let n = result
                .get("rows_affected")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let _ = writeln!(out, "{statement} {n}");
        }
        "ddl" => {
            let _ = writeln!(out, "OK");
        }
        "explain" => {
            let text = result
                .get("plan_text")
                .and_then(Value::as_str)
                .unwrap_or("");
            let _ = write!(out, "{text}");
        }
        "begin" => {
            let _ = writeln!(out, "BEGIN");
        }
        "commit" => {
            let _ = writeln!(out, "COMMIT");
        }
        "rollback" => {
            let _ = writeln!(out, "ROLLBACK");
        }
        other => {
            let _ = writeln!(
                out,
                "{}",
                sanitize_for_terminal(&format!("(unrecognized result kind {other:?}: {result})"))
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_result_renders_table_and_count() {
        let mut out = Vec::new();
        let result = serde_json::json!({
            "kind": "rows",
            "columns": [{"name": "id", "type": "integer", "nullable": false}],
            "rows": [[{"type": "integer", "value": 1}]],
            "row_count": 1
        });
        render_sql_result(&result, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("id"));
        assert!(text.contains('1'));
        assert!(text.contains("(1 rows)"));
    }

    #[test]
    fn write_result_renders_statement_and_count() {
        let mut out = Vec::new();
        let result =
            serde_json::json!({"kind": "write", "statement": "INSERT", "rows_affected": 3});
        render_sql_result(&result, &mut out);
        assert_eq!(String::from_utf8(out).unwrap().trim(), "INSERT 3");
    }
}
