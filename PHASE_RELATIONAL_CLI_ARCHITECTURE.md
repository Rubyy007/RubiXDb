# Phase: Relational Database — CLI Architecture (Increment 12)

## 1. What this is, and — critically — what it is not

`rubixdb` (the compiled binary, crate `rubixdb-cli`) is a **thin HTTP
client of `POST /v1/sql`**. It has **no** dependency on `rubixdb` or
`rubixdb-sql` (verified: `cli/Cargo.toml` depends only on `reqwest`,
`serde`/`serde_json`, `uuid`, `rustyline`, `rpassword`) and embeds no
`TransactionManager`, parser, binder, planner, or executor of any kind.
Every SQL statement the CLI ever runs is submitted to a real server over
real HTTP and rendered from that server's own typed JSON response — the
server is the *only* SQL execution authority (item 27/105).

The CLI parses **only** its own backslash meta-command syntax
(`cli/src/meta.rs`) plus the minimal, purely lexical statement-boundary
splitting needed for script mode (`cli/src/sql_split.rs`: tracking
single-quoted string literals so a `;` inside `'foo;bar'` is never
mistaken for a statement terminator — the same thing `psql`/`mysql`/
`sqlite3`'s own CLIs do for the identical structural reason: the
server's own `parse_statement` refuses multi-statement input in one
request, `sql/src/parse_tests.rs::parse_statement_rejects_more_than_
one_statement`, so a multi-statement script has no choice but to be
split client-side before each piece is submitted). Neither module has
any notion of SQL keywords, statement validity, or semantics.

## 2. Module map

| Module | Responsibility |
|---|---|
| `main.rs` | Argument parsing (`-c`, `-f`, `--help`, `--version`), credential resolution, dispatch to REPL/script mode |
| `client.rs` | `Connection` — the one HTTP session this process holds; `session_id` tracking, typed error mapping |
| `protocol.rs` | Plain `serde` DTOs mirroring the `/v1/sql` wire contract — a fresh definition, not a dependency on server-internal types (§1) |
| `sql_split.rs` | Quote-aware statement-boundary splitting (lexical only, never semantic) |
| `runner.rs` | One statement/meta-command → submit → render — the single code path both REPL and script mode share |
| `meta.rs` | The locked `\l \ls \lt \d \di \du \conninfo \c \help \q` command contract, each querying real `/v1/catalog/*` data |
| `render.rs` | Typed-value → terminal text, with adversarial-content sanitization |
| `repl.rs` | Interactive `rubixdb>` loop (`rustyline`) |
| `script.rs` | `-c`/`-f` batch execution, one session for the whole run |

## 3. The locked command contract (item 28/97/107)

```
\l          LIST DATABASES   -- real /v1/catalog/databases, honestly one row today
\ls         LIST SCHEMAS     -- real /v1/catalog/schemas
\lt         LIST TABLES      -- real /v1/catalog/tables, authorization-filtered
\d table    DESCRIBE TABLE   -- real /v1/catalog/tables/:name
\di         LIST INDEXES     -- real /v1/catalog/indexes
\du         AUTHORIZATION    -- real /v1/catalog/authz, current principal's own grants only
\conninfo   CONNECTION INFO  -- endpoint, principal, role, session state; never the API key
\c          CONNECT/SWITCH   -- honest no-op reconnect; RubiXDB has no multi-database switching (item 34/98)
\help       HELP
\q          QUIT
```

`\dn`/`\dt` are **not** the primary command names anywhere in this
crate or its documentation (item 97's own explicit prohibition) — `\l`/
`\ls`/`\lt` are the only names implemented and documented.

Every one of these queries the server's real `/v1/catalog/*` routes at
call time — none is a hardcoded example or a fixture. `cli/tests/
cli_integration.rs::meta_commands_reflect_real_backend_state_changes`
proves this the way item 68 requires: `CREATE TABLE widgets` then `\lt`
must show `widgets`, `CREATE INDEX` then `\di` must show it, `DROP
TABLE` then `\lt` must no longer show it — a real backend verification,
not a snapshot.

## 4. Session/transaction handling (item 35/39)

`Connection` holds exactly one `session_id: Option<String>` for the
whole process lifetime. A successful response's own `session_id` field
is the single source of truth: `BEGIN`'s response sets it, `COMMIT`/
`ROLLBACK`'s response (`session_id: null`) clears it, every other
successful response echoes back whatever was current (unchanged if the
statement failed — the server's own `put_back` semantics guarantee the
session, if any, is still open under the same id, so the client simply
keeps using the id it already has rather than re-deriving it from an
error response that carries none).

This is not a second transaction implementation — the CLI never decides
commit/rollback/conflict outcomes itself; it only ever forwards
`session_id` and renders exactly what the server returned. Script mode
(`-f`) and interactive mode share the *one* `Connection` for the whole
run, so `BEGIN; INSERT; SELECT; COMMIT;` across multiple lines of a
script genuinely shares one server-side transaction (item 39), verified
by `cli_integration.rs::script_transaction_shares_one_session_across_
statements` against a real running server.

## 5. Credential handling (item 37)

No `--api-key` flag exists anywhere in this crate — a bare CLI argument
would land in shell history and be visible to every other process on
the machine via `ps`/Task Manager. Resolution order: `RUBIXDB_API_KEY`
environment variable, then (only when connected to a real terminal,
`std::io::IsTerminal`) an interactive, non-echoing prompt (`rpassword`).
Neither path ever prints, logs, or otherwise surfaces the key — `cli_
integration.rs::conninfo_never_prints_the_api_key` asserts this against
real captured stdout/stderr from the compiled binary, and `missing_api_
key_in_a_non_interactive_run_fails_cleanly` proves a non-interactive
run with no key set fails fast rather than hanging on a prompt that
can never be answered.

## 6. Script mode (item 38/39)

`-c "SQL"` / `-f script.sql`: every statement (and every standalone
meta-command line) runs in order, on the one session for the whole run.
The first failure — a statement error, or an unknown meta-command —
stops the run immediately (no silent continuation, item 38) and the
process exits `1`; a clean `\q` exits `0`; full success exits `0`.
Verified against the real compiled binary and a real running server
(`cli_integration.rs::f_mode_runs_a_real_script_file_and_reports_exit_
code`, `c_mode_fatal_error_exits_non_zero_and_stops`).

## 7. Output rendering and terminal safety (item 40/125)

`render.rs` keeps every value typed (the server's own tagged JSON
shape) until the very last step, where it becomes display text — never
collapsed to a plain string earlier in the pipeline. Every ASCII control
character (`0x00..=0x1F`, `0x7F` — the range that includes `ESC`, the
start of every ANSI escape sequence) in adversarial row content is
replaced with its `\xNN` hex escape before printing, so a malicious
`TEXT` value can never move the cursor, change terminal colors, or
otherwise manipulate the terminal — ordinary printable text and
multi-byte UTF-8 both pass through completely unchanged (`render::
tests::sanitizes_ansi_escape_sequences`/`preserves_ordinary_unicode_
text`). `BIGINT`/`DECIMAL` are rendered from their wire-safe string/
unscaled+scale representation (§`PHASE_RELATIONAL_SQL_API_
ARCHITECTURE.md` §2), never through a lossy intermediate float. `BLOB`
values are shown as a byte-length summary, never decoded/rendered as
text (no safe universal terminal rendering for arbitrary binary data
exists, and this crate does not attempt to guess one).

## 8. A real bug found and fixed while testing this crate

`cli/tests/cli_integration.rs`'s first version deadlocked: `#[tokio::
test]` defaults to a **single-threaded** runtime, and the test spawned
a real server task (`tokio::spawn(axum::serve(...))`) on that runtime
while also calling `std::process::Command::output()` — a **blocking**
call — from the same test function, on the same one OS thread. The
server task could never be polled while the test thread was blocked
waiting for the CLI subprocess, and the CLI subprocess could never get
an HTTP response from a server that was never running — an unrecoverable
deadlock, discovered when a `cargo test` run exceeded its timeout and
had to be traced to two hung `rubixdb.exe` processes and a hung test
binary still holding the output `.exe` file locked (confirmed via `Get-
Process`, then terminated). Fixed by adding `flavor = "multi_thread"` to
every `#[tokio::test]` in that file, so the spawned server task runs on
a different worker thread than the one blocked in `Command::output()`.
This is a bug in the *test harness*, not in the CLI or API — named here
because it is exactly the kind of finding item 113 requires being
proven and fixed rather than silently worked around.

## 9. Known limitations

- No typed-`$n`-parameter submission from the interactive/script surface
  — a user embeds literal values directly in SQL text (still safe: the
  CLI never templates untrusted data into that text itself, so this is
  ordinary SQL, not string-interpolated injection risk; it is simply a
  convenience feature, parameter binding via placeholders, not yet
  exposed through this client's own UI). The wire protocol itself
  (`protocol.rs::SqlRequestBody::params`) already supports it — a future
  increment could add CLI-side syntax for it without touching the
  server.
- No connection-profile/config-file support (item 138) — only
  environment variables and the interactive prompt, per the smallest
  necessary v1 scope.
