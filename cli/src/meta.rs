//! Backslash meta-commands — item 27/28/97 (the **locked** command
//! contract): `\l` = list databases, `\ls` = list schemas, `\lt` = list
//! tables, `\d table` = describe table, `\di` = list indexes, `\du` =
//! authorization metadata, `\conninfo` = connection info, `\c` =
//! connect/switch, `\help`, `\q`. Every one of these queries **real**
//! backend metadata through `crate::client::Connection`'s own `/v1/
//! catalog/*` routes — never a hardcoded example, never a fixture
//! (item 29/54/55/56).
//!
//! This module parses **only** its own backslash syntax — it never
//! parses SQL (item 27's own "the CLI may parse ONLY its own backslash
//! meta-commands... must NOT parse SQL semantics itself"): anything not
//! starting with `\` is handed to `Connection::execute` completely
//! unexamined.

use std::io::Write;

use serde_json::Value;

use crate::client::{CliError, Connection};
use crate::render::{render_table, sanitize_for_terminal};

pub enum MetaOutcome {
    Ok,
    Err,
    Quit,
}

fn print_error(out: &mut impl Write, e: &CliError) -> bool {
    let _ = writeln!(out, "ERROR: {e}");
    false
}

fn print_json_or_error(out: &mut impl Write, result: Result<Value, CliError>) -> bool {
    match result {
        Ok(v) => {
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_default()
            );
            true
        }
        Err(e) => print_error(out, &e),
    }
}

/// Dispatches one backslash command (the leading `\` already stripped
/// by the caller). Returns `MetaOutcome::Quit` only for `\q`; `Err` for
/// an unknown command or a request that itself failed (script mode's
/// own exit-code semantics, item 38, treat meta-command failures the
/// same as SQL statement failures -- both are "this line of the script
/// did not succeed").
pub fn handle(conn: &mut Connection, command_line: &str, out: &mut impl Write) -> MetaOutcome {
    let mut parts = command_line.trim().splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim).unwrap_or("");

    let ok = match cmd {
        "l" => list_databases(conn, out),
        "ls" => list_schemas(conn, out),
        "lt" => list_tables(conn, out),
        "d" => describe_table(conn, arg, out),
        "di" => list_indexes(conn, out),
        "du" => authz(conn, out),
        "conninfo" => conninfo(conn, out),
        "c" => connect(conn, arg, out),
        "help" | "?" => {
            help(out);
            true
        }
        "q" => return MetaOutcome::Quit,
        "" => {
            let _ = writeln!(out, "ERROR: empty meta-command (type \\help for a list)");
            false
        }
        other => {
            let _ = writeln!(
                out,
                "ERROR: unknown meta-command \\{}  (type \\help for a list)",
                sanitize_for_terminal(other)
            );
            false
        }
    };
    if ok {
        MetaOutcome::Ok
    } else {
        MetaOutcome::Err
    }
}

fn as_rows_table(v: &Value, columns: &[(&str, &str)]) -> (Vec<String>, Vec<Vec<String>>) {
    let headers: Vec<String> = columns.iter().map(|(_, h)| h.to_string()).collect();
    let rows: Vec<Vec<String>> = v
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|row| {
                    columns
                        .iter()
                        .map(|(key, _)| {
                            row.get(*key)
                                .map(|v| match v {
                                    Value::String(s) => sanitize_for_terminal(s),
                                    other => sanitize_for_terminal(&other.to_string()),
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default();
    (headers, rows)
}

/// `\l` — LIST DATABASES (item 28/29/53/98: real, never faked; today
/// always exactly one row, honestly, since RubiXDB does not yet support
/// multiple logical databases).
fn list_databases(conn: &Connection, out: &mut impl Write) -> bool {
    match conn.list_databases() {
        Ok(v) => {
            let (headers, rows) = as_rows_table(&v, &[("database_id", "id"), ("name", "name")]);
            let _ = writeln!(out, "{}", render_table(&headers, &rows));
            true
        }
        Err(e) => print_error(out, &e),
    }
}

/// `\ls` — LIST SCHEMAS.
fn list_schemas(conn: &Connection, out: &mut impl Write) -> bool {
    match conn.list_schemas() {
        Ok(v) => {
            let (headers, rows) = as_rows_table(
                &v,
                &[
                    ("schema_id", "id"),
                    ("name", "name"),
                    ("database_id", "database_id"),
                ],
            );
            let _ = writeln!(out, "{}", render_table(&headers, &rows));
            true
        }
        Err(e) => print_error(out, &e),
    }
}

/// `\lt` — LIST TABLES.
fn list_tables(conn: &Connection, out: &mut impl Write) -> bool {
    match conn.list_tables() {
        Ok(v) => {
            let (headers, rows) = as_rows_table(
                &v,
                &[
                    ("table_id", "id"),
                    ("name", "name"),
                    ("schema_id", "schema_id"),
                ],
            );
            let _ = writeln!(out, "{}", render_table(&headers, &rows));
            true
        }
        Err(e) => print_error(out, &e),
    }
}

/// `\d table_name` — DESCRIBE TABLE.
fn describe_table(conn: &Connection, arg: &str, out: &mut impl Write) -> bool {
    if arg.is_empty() {
        let _ = writeln!(out, "ERROR: \\d requires a table name, e.g. \\d users");
        return false;
    }
    match conn.describe_table(arg) {
        Ok(v) => {
            let columns = v.get("columns").cloned().unwrap_or(Value::Array(vec![]));
            let (headers, rows) = as_rows_table(
                &columns,
                &[
                    ("ordinal", "#"),
                    ("name", "column"),
                    ("data_type", "type"),
                    ("nullable", "nullable"),
                    ("primary_key", "pk"),
                ],
            );
            let _ = writeln!(
                out,
                "Table {:?}\n{}",
                v.get("name").and_then(Value::as_str).unwrap_or(arg),
                render_table(&headers, &rows)
            );
            true
        }
        Err(e) => print_error(out, &e),
    }
}

/// `\di` — LIST INDEXES.
fn list_indexes(conn: &Connection, out: &mut impl Write) -> bool {
    match conn.list_indexes() {
        Ok(v) => {
            let (headers, mut rows) = as_rows_table(
                &v,
                &[
                    ("name", "name"),
                    ("table_name", "table"),
                    ("kind", "kind"),
                    ("unique", "unique"),
                    ("state", "state"),
                ],
            );
            // `columns` is an array field, rendered separately (as_rows_table
            // only handles scalar/string fields).
            if let Some(arr) = v.as_array() {
                for (row, src) in rows.iter_mut().zip(arr) {
                    let cols = src
                        .get("columns")
                        .and_then(Value::as_array)
                        .map(|c| {
                            c.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    row.push(sanitize_for_terminal(&cols));
                }
            }
            let mut headers = headers;
            headers.push("columns".to_string());
            let _ = writeln!(out, "{}", render_table(&headers, &rows));
            true
        }
        Err(e) => print_error(out, &e),
    }
}

/// `\du` — safe authorization metadata for the *current* principal
/// (item 32: RubiXDB has no separate user/role directory to enumerate;
/// this is the honest scope, never fabricated).
fn authz(conn: &Connection, out: &mut impl Write) -> bool {
    print_json_or_error(out, conn.authz())
}

/// `\conninfo` — safe connection info (item 33: never the API key).
fn conninfo(conn: &Connection, out: &mut impl Write) -> bool {
    let _ = writeln!(out, "endpoint: {}", conn.base_url());
    let ok = match conn.whoami() {
        Ok(v) => {
            let _ = writeln!(
                out,
                "principal: {}  role: {}",
                v.get("principal_name")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                v.get("role").and_then(Value::as_str).unwrap_or("?")
            );
            true
        }
        Err(e) => print_error(out, &e),
    };
    match conn.session_id() {
        Some(id) => {
            let _ = writeln!(out, "session: {id} (transaction open)");
        }
        None => {
            let _ = writeln!(out, "session: none (autocommit)");
        }
    }
    ok
}

/// `\c` — item 34/98: RubiXDB does not yet support multiple logical
/// databases/connections, so this is honestly a no-op reconnect
/// confirmation, never a fake "switched to database X." A future
/// increment that adds real multi-database support would extend this,
/// not this one.
fn connect(conn: &Connection, arg: &str, out: &mut impl Write) -> bool {
    if !arg.is_empty() {
        let _ = writeln!(
            out,
            "NOTICE: RubiXDB does not yet support multiple logical databases/connections; \
             \\c cannot switch to {:?}. Still connected to {}.",
            sanitize_for_terminal(arg),
            conn.base_url()
        );
        return true;
    }
    let _ = writeln!(out, "connected to {}", conn.base_url());
    true
}

fn help(out: &mut impl Write) {
    let _ = writeln!(
        out,
        r#"RubiXDB CLI -- meta-commands:
  \l              list databases
  \ls             list schemas
  \lt             list tables
  \d table_name   describe a table
  \di             list indexes
  \du             show authorization metadata for the current principal
  \conninfo       show connection information
  \c              reconnect / show connection target
  \help           show this help
  \q              quit

Anything not starting with \ is sent to the server as SQL. Terminate a
statement with a trailing ';' (optional -- one line is one statement).
Use BEGIN/COMMIT/ROLLBACK to open an explicit transaction spanning
multiple statements in this session."#
    );
}
