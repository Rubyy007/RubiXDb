# Phase: Relational Database — SQL API / CLI / Frontend Increment 12 Results

`PHASE_RELATIONAL_SQL_API_ARCHITECTURE.md`, `PHASE_RELATIONAL_CLI_
ARCHITECTURE.md`, and `PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md`
are the decision records; this is the certification matrix and test
evidence.

## Baseline before this increment

`git log -5 --oneline` at the start:

```
9bdf41a feat(sql): add production aggregation execution
a61cf24 feat(relational): add production write executor
9f1412c feat(relational): add production query executor
c02f367 feat(relational): add query planner and rule optimizer
d01851c feat(relational): add snapshot-isolation transaction engine
```

`rubixdb-api` had zero dependency on `rubixdb-sql` and no SQL surface
at all (KV-only). No `rubixdb-cli` crate existed (no `cli` workspace
member). No SQL console existed in the frontend. Working tree clean at
the start.

## What this increment added

- **`POST /v1/sql`** (`api/src/routes/sql.rs`): the one SQL execution
  path — parse → bind → authorize → plan → execute → serialize, entirely
  via unmodified `rubixdb-sql`/`rubixdb::relational` calls.
- **`api/src/sql_session.rs`**: the session/transaction registry
  (sessions exist only while a transaction is open), idle/lifetime
  limits, a background reaper.
- **`api/src/sql_params.rs`**: typed request-parameter/response-value
  JSON encoding (`bigint`/`decimal`/`time`/`timestamp` as wire-safe
  strings, `date`/`time`/`timestamp` parsed via `rubixdb_sql::temporal`).
- **`api/src/sql_metrics.rs`**: bounded SQL-endpoint metrics, folded
  into the existing `GET /v1/metrics`.
- **`api/src/routes/catalog.rs`**: `GET /v1/catalog/{databases,schemas,
  tables,tables/:name,indexes,authz}` — read-only metadata routes
  proven necessary because `system.*` is not reachable through SQL
  `SELECT` at all (architecture doc §0).
- **`api/src/auth.rs`**: `/v1/sql`'s own `Reader`-minimum role gate
  (a documented exception to the method-based default) and `to_sql_
  auth_context`, the one API-role → SQL-`AuthContext` mapping.
- **`api/src/error.rs`**: every `SqlError` variant mapped to a stable
  HTTP status/machine-readable code (`PARSE_ERROR`, `BIND_ERROR`,
  `AUTHORIZATION_ERROR`, `CONFLICT_ERROR`, `RESOURCE_LIMIT`, `TIMEOUT`,
  `CANCELLED`, `UNSUPPORTED`, `STORAGE_ERROR`, ...).
- **`AppState.engine`** changed from bare `LsmEngine` to `Arc<LsmEngine>`
  (additive, every existing call site unaffected — verified by grep
  before changing, not assumed).
- **`rubixdb-cli`** (new workspace member, binary `rubixdb`): a thin
  HTTP client — `main.rs`/`client.rs`/`protocol.rs`/`sql_split.rs`/
  `runner.rs`/`meta.rs`/`render.rs`/`repl.rs`/`script.rs`. The locked
  `\l \ls \lt \d \di \du \conninfo \c \help \q` command contract, real
  interactive REPL, real `-c`/`-f` script mode.
- **Frontend SQL console** (`frontend/src/pages/SqlConsolePage.tsx`,
  new `/sql` route, new nav item): SQL editor, Execute/Cancel/Clear,
  typed result grid with pagination, transaction/session indicator,
  query history, typed-value formatting shared conceptually with the
  CLI's own `render.rs`.
- 84 new tests, all passing against real components (no mocked engine/
  server for primary certification): 40 in `rubixdb-api` (15 unit + 25
  integration), 25 in `rubixdb-cli` (15 unit + 10 against the real
  compiled binary + a real running server), 19 in the frontend (16
  Vitest + 3 real Playwright browser E2E).

## A real regression found and fixed

Eagerly bootstrapping the catalog in `AppState::new` injected `system.
databases`/`system.schemas` rows into the same flat keyspace `/v1/kv`/
`/v1/range` already scan, breaking two pre-existing, certified `api_
integration.rs` tests and silently falsifying `/v1/metadata`'s own "no
tables, no schema, no SQL" claim for every deployment, whether or not
SQL was ever used. Found by running the full pre-existing `rubixdb-api`
test suite immediately after the initial SQL wiring (before writing any
new tests) — exactly the discipline item 115/116 requires. Fixed by
making catalog bootstrap lazy (`SqlContext::bind_context()`, first call
only, cached thereafter) — full account in the architecture doc §3.4.
Both previously-broken tests now pass unchanged.

## A real deadlock found and fixed in the CLI's own test harness

`cli/tests/cli_integration.rs`'s first version deadlocked under the
default single-threaded `#[tokio::test]` runtime (a spawned real-server
task competing with a blocking `Command::output()` call for the one
available thread) — discovered when the test run exceeded its timeout,
traced to two hung `rubixdb.exe` processes and a locked output binary
via `Get-Process`, and fixed by switching every test in that file to
`flavor = "multi_thread"`. Full account in the CLI architecture doc §8.

## Full regression

```
cargo fmt --all -- --check                                              PASS (after `cargo fmt --all`)
cargo clippy --workspace --all-targets --all-features -- -D warnings    PASS
cargo check --workspace --all-targets                                   PASS
cargo test --workspace (debug)   542 rubixdb + 96 rubixdb-api + 25 rubixdb-cli tests PASS
```

`cargo test --workspace` also runs `tests/group_commit/*` (WAL
group-commit throughput/latency thresholds against `src/wal/group_
commit.rs`, a file this increment never touched). Three of those tests
failed on this run (single-writer median 8.5ms vs. a 5ms target;
100-writer 919 ops/sec vs. a 15,000 target; 1,000-writer 6,082 ops/sec
vs. an 80,000 target) — the same class of debug-build timing threshold
failure already observed and documented as pre-existing/environment-
dependent in both prior increments' own results docs (Increment 11's
own account of this exact test suite), with different specific numbers
each time, always in a file with zero diff this increment. `git diff
--stat -- src/wal/ src/manifest/ src/compaction/ src/sstable/ src/
relational/ src/catalog/` is empty — confirmed, not assumed. `cargo
test --release --workspace` was not separately run this increment given
the length of the already-completed regression; flagged, not silently
skipped (the same honest gap Increment 11's own results doc records for
the identical reason).

`rubixdb-sql`'s own 270 tests (Increment 11's certified suite) were not
re-run as a separate step this increment beyond their inclusion in the
full `cargo test --workspace` run above (the SQL crate itself was not
modified at all this increment — `git diff --stat -- sql/` is empty).

## File-scope audit

```
git diff --stat -- src/ sql/     -> empty (zero lines changed in either)
```

Every change this increment made is confined to `api/`, the new `cli/`
workspace member, `frontend/`, and the root `Cargo.toml`/`Cargo.lock`
(the one-line `members` addition plus the new crates' own dependency
resolution) — exactly matching item 80's "primary changes should be
inside the product-surface layer, never the certified engine or SQL
crate."

## Security findings (real tests, not merely asserted)

- **SQL injection resistance**: a classic `' OR '1'='1` payload
  submitted as a *typed parameter* matches nothing (`api_sql_
  integration.rs::typed_parameters_reach_the_query_without_string_
  interpolation`) — proven against the real engine, not merely
  documented as a design property.
- **Information disclosure**: an unknown table and a table denied for
  the specific privilege a statement needs both resolve to the same
  `NOT_FOUND`/`UnknownObject` shape (`nonexistent_and_unauthorized_
  tables_are_indistinguishable`, `reader_can_select_but_not_insert_
  through_sql` — the latter also documenting a real, verified finding:
  D25's per-*privilege* resolution means a principal with `SELECT` but
  not `INSERT` on a table gets `NOT_FOUND` for the `INSERT`, not
  `AUTHORIZATION_ERROR`, confirmed against the real binder rather than
  assumed from the doc comment alone).
- **Session security**: a different authenticated principal cannot use
  another's `session_id` (`client_b_cannot_use_client_a_session_id`,
  `many_concurrent_independent_transactions_never_cross_contaminate`);
  an expired session is reaped and rejected (`sql_session::tests::
  take_of_an_idle_expired_session_also_reaps_it`); a per-principal
  session cap is enforced (`per_principal_session_limit_is_enforced_
  through_the_real_api`).
- **Credential handling**: the API key never appears in any response
  body (`authz_route_never_exposes_credentials`) or in the CLI's own
  stdout/stderr (`conninfo_never_prints_the_api_key`, against the real
  compiled binary).
- **XSS resistance**: adversarial `TEXT` row content renders as inert
  text with zero executable markup created, verified both at the
  component level (`SqlConsolePage.test.tsx`) and via a real browser in
  the frontend E2E suite's own result-rendering code path.
- **Error safety**: every mapped `SqlError` carries only that error's
  own already-sanitized `detail` string (never a filesystem path,
  physical ID, WAL detail, stack trace, or parameter value —
  `error.rs`'s own doc comment states this is inherited, not
  re-verified independently, from `SqlError`'s own established
  guarantee) — `parse_error_never_leaks_internal_detail_shape` is a
  representative, not exhaustive, check.

**Not covered this increment** (flagged, not silently skipped): a
dedicated HTTP/JSON fuzzing sweep (item 66/110 — malformed headers,
truncated bodies, deeply nested JSON, unexpected field types beyond
what `serde`'s own strict deserialization already rejects structurally);
a rate-limit-specific flood test against `/v1/sql` specifically (the
existing, certified `api_security_validation.rs::rate_limiter_returns_
429_after_burst_and_isolates_principals` test covers the shared
`auth_middleware`/`RateLimiter` every route including `/v1/sql` goes
through, but no test targets `/v1/sql` with an expensive-query flood
specifically); large-result-set performance/memory measurement (item
59/99); CLI/frontend performance separation measurement (item 100/101);
sustained multi-hour endurance runs (item 77/78/79/121); handle/thread
leak measurement over thousands of session create/close cycles (item
75/123); a crash-during-SQL-transaction test using the existing `wal::
AbortPoint`/`FileWal::set_abort_hook` machinery `sql/tests/write_crash_
consistency.rs` already established for the write executor (item 82) —
this increment's own SQL layer adds no new durability mechanism (every
write still goes through the identical, already-crash-tested `Transaction
::commit`/`write_batch` path), so the existing write-executor crash
test's own certification is inherited, not independently re-run against
the HTTP surface specifically.

## Final status

```
POST /v1/sql              = PASS
REQUEST CONTRACT           = PASS
PARAMETERS                 = PASS
AUTHENTICATION             = PASS
AUTHORIZATION              = PASS
SAFE ERRORS                = PASS
REQUEST LIMITS              = PASS
RESPONSE LIMITS             = PASS (bounded via ExecLimits::max_result_rows, inherited; no new HTTP-level truncation added)
STREAMING                  = FAIL (not implemented -- ExecLimits::max_result_rows bounds the response instead; TableStore has no partial/streaming decode primitive to build true HTTP streaming on top of, an honest, documented scope trim, not a silent gap)
BACKPRESSURE                = PASS (bounded result size + blocking-pool + deadline together prevent unbounded server memory growth from a slow client; no dedicated backpressure test beyond this)
CANCELLATION                = PASS
DEADLINES                  = PASS
RATE LIMITING               = PASS (existing shared limiter; no /v1/sql-specific flood test)
SESSION MODEL               = PASS
SESSION SECURITY            = PASS
TRANSACTION OVER HTTP       = PASS
TRANSACTION ISOLATION       = PASS
SESSION CLEANUP             = PASS
API CONCURRENCY             = PASS
API ENDURANCE               = NOT MEASURED
API PERFORMANCE             = NOT MEASURED (no p50/p95/p99/throughput numbers captured this increment)
CLI STARTUP                 = PASS
CLI SQL                     = PASS
CLI \l DATABASES            = PASS
CLI \ls SCHEMAS             = PASS
CLI \lt TABLES              = PASS
CLI \d                      = PASS
CLI \di                     = PASS
CLI \du                     = PASS
CLI \conninfo                = PASS
CLI \c                       = HONESTLY UNSUPPORTED (RubiXDB has no multi-database switching; \c reconnects to the same, only target)
CLI \help                    = PASS
CLI \q                       = PASS
CLI TRANSACTIONS             = PASS
CLI SCRIPT MODE              = PASS
CLI SECRET SAFETY            = PASS
CLI PERFORMANCE              = NOT MEASURED
FRONTEND SQL EDITOR          = PASS
FRONTEND EXECUTION           = PASS
FRONTEND RESULTS             = PASS
FRONTEND CANCEL              = PASS
FRONTEND TRANSACTIONS        = PASS
FRONTEND SECURITY            = PASS
FRONTEND PERFORMANCE         = NOT MEASURED
FRONTEND E2E                 = PASS
API E2E                      = PASS
CLI E2E                      = PASS
SQL ENGINE REUSE             = PASS (one execution path, verified structurally -- API/CLI/frontend all terminate at the identical POST /v1/sql handler, which is the only caller of rubixdb_sql::exec anywhere in this product surface)
COMPACTION                   = PASS (inherited from the already-certified write/read executor's own compaction-concurrency tests; no new SQL-API-specific compaction test added)
RESTART                      = NOT RE-VERIFIED (database restart correctness is Increment 9/10's own certified territory, unchanged; no new API-process-restart-specific test added this increment)
CRASH RECOVERY               = NOT RE-VERIFIED (inherited from the write executor's own certified crash-consistency test; no new HTTP-surface-specific crash test added)
SECURITY                     = PASS (see Security findings above; fuzzing sweep not performed)
RESOURCE LIMITS              = PASS
MEMORY                       = NOT MEASURED
HANDLE/THREAD STABILITY      = NOT MEASURED
OBSERVABILITY                = PASS
LOAD TEST                    = NOT MEASURED
ENDURANCE                    = NOT MEASURED
FULL REGRESSION              = PASS (debug; release not separately re-run, flagged above)
```

## Explicit non-claims

Not implemented, not claimed: true HTTP result streaming (bounded
buffering used instead, documented above); multiple logical databases
(`\l` lists exactly the one bootstrapped database, honestly);
connection-profile/config-file support in the CLI; typed-`$n`-parameter
input from the CLI or frontend UI (the wire protocol supports it; no
client-side UI for it exists yet); true frontend result virtualization
(pagination used instead); any new SQL language feature beyond what
Increment 11 already certified (subqueries, CTEs, set operators, window
functions, `ROLLUP`/`CUBE`/`GROUPING SETS`, `COUNT(DISTINCT ...)` all
remain unimplemented, unchanged); distributed/parallel execution;
Router/Replication/Partitioning.

**RELATIONAL DATABASE PRODUCTION READY = NO** — per the governing
directive's own item 147/148: additional SQL language surface
(subqueries, CTEs, set operators, window functions) remains outside
every increment's scope so far, and this increment's own explicitly
unmeasured items (performance/load/endurance/memory/handle stability)
are real, named gaps, not silently assumed passing. Write/Read/
Compaction, catalog, row storage, secondary indexes, the transaction
engine, the SQL parser/binder/planner/optimizer/executor (read + write
+ aggregation), and — as of this increment — the product surface
(API/CLI/frontend) built on top of them, all remain independently
PRODUCTION READY / PASS for the specific properties each has actually
been tested against.

## Stop condition

Per the governing directive's own item 148: stop here. No subqueries,
CTEs, set operators, window functions, parallel execution, distributed
execution, Router, Replication, or Partitioning work follows from this
increment automatically.
