# Phase: Relational Database — SQL API Architecture (Increment 12)

## 0. Phase 0 audit findings (read-only, before any design decision)

Verified directly against the repository, not assumed:

- **`rubixdb-api` had zero dependency on `rubixdb-sql`** — `api/Cargo.toml`
  depended only on `rubixdb`. Every existing route (`/v1/kv`, `/v1/range`,
  `/v1/snapshots`, `/v1/compaction/*`, `/v1/metrics`) is a thin wrapper
  directly over `LsmEngine`'s own flat binary-key/binary-value API —
  confirmed by reading every file in `api/src/routes/`. There was no SQL
  execution surface anywhere in the product before this increment.
- **`AppState.engine` was a bare `LsmEngine`, not `Arc<LsmEngine>`.**
  `CatalogService`/`TableStore`/`IndexBuilder`/`TransactionManager` each
  require their own `Arc<LsmEngine>` clone (the exact pattern `sql/src/
  test_support.rs::Fixture` already establishes) — changing this field's
  type was the one structural change needed; every existing `state.
  engine.method(...)` call site is unaffected by it (`Arc<T>` derefs to
  `&T` transparently), verified by grepping every such call site before
  making the change, not assumed safe.
- **`auth_middleware`'s existing role gate is HTTP-method-based**
  (`GET`/`HEAD` → `Reader`, everything else → `Admin`) — correct for
  every existing route (each has one fixed CRUD-like purpose), wrong for
  `POST /v1/sql` (one endpoint, many different privilege levels
  depending on the SQL text). §4 below is the resulting design decision.
- **`rubixdb-sql`'s own `auth.rs` doc comment already names the exact
  wiring this increment needed to supply**: "whatever assembles a
  session for a principal is responsible for mapping its own role
  concept onto one of [`DefaultAccess::None/Reader/Admin`], once that
  wiring exists." This increment is that wiring (§4).
- **`system.*` catalog objects are not reachable through ordinary SQL
  `SELECT` at all** — `sql/src/bind/scope.rs::resolve_table` only ever
  resolves a name via `CatalogService::get_table_by_name` against a
  schema's own *user* tables. There is no SQL-level read path to
  `system.databases`/`schemas`/`tables`/`columns`/`indexes`/`grants`.
  This is why §6 adds dedicated, read-only `/v1/catalog/*` routes rather
  than trying to serve `\lt`/`\d`/`\di` through `POST /v1/sql` — proven
  necessary by inspection (the governing directive's own item 57
  standard), not assumed.
- **`CatalogService::bootstrap()` persists `system.databases`/`system.
  schemas` rows into the *same flat keyspace* `/v1/kv` and `/v1/range`
  already scan** — there is no separate catalog storage area. This was
  discovered as a real regression during this increment (§3.4) and
  fixed by making bootstrap lazy.
- **RubiXDB has exactly one bootstrapped database (`"default"`) and one
  default schema (`"public"`)** today; `CREATE DATABASE` has no
  execution primitive (Increment 10's own already-documented boundary,
  unchanged). `CREATE SCHEMA` *is* implemented, so multiple schemas are
  genuinely possible.
- **`rubixdb_sql::exec::CancellationToken`/`ExecLimits::deadline`
  already exist**, built for exactly this integration (item 40/41 of
  the aggregation-era directive) — reused verbatim, never re-implemented
  (§5).
- **`Transaction`/`Snapshot` are plain owned, `Send` types** (`Snapshot
  { seq: u64, registry: Arc<SnapshotRegistry> }`, `Transaction { inner:
  Arc<Inner>, snapshot: Snapshot, writes: HashMap<...>, ... }`) — no
  borrows, no `!Send` markers anywhere in either type. Confirmed by
  reading `src/relational/txn.rs` and `src/lsm/mod.rs::Snapshot` in full
  before designing the session registry (§3), which stores a live
  `Transaction` in `Mutex<HashMap<Uuid, SqlSession>>` shared across the
  Tokio multi-threaded runtime.
- **`LsmEngine` is fully synchronous** (`PHASE_API_ARCHITECTURE.md` §0,
  still true) — every existing KV route calls it directly inside an
  `async fn` handler with no `spawn_blocking` boundary, an established,
  accepted pattern for bounded single-key operations. SQL execution can
  run far longer (a full scan / large aggregation), so §5 adds a
  boundary the KV routes never needed.

## 1. Architecture

```
Client (CLI, frontend SQL console, curl, any HTTP client)
   |
   |  HTTPS/HTTP, JSON
   v
POST /v1/sql            <-- the one SQL execution path (item 105)
   |
   |  parse -> bind -> authorize -> plan -> execute -> serialize
   v
rubixdb-sql              (unmodified: parser, binder, planner, executor)
   |
   v
rubixdb::relational       (TableStore / IndexBuilder / TransactionManager)
   |
   v
Arc<LsmEngine>            (certified, unchanged)
```

`GET /v1/catalog/*` is a **second, narrower** surface for metadata the
SQL engine itself never exposes (system.* objects) — never a second SQL
execution path; every route calls straight into `CatalogService`'s own
existing read methods and the SQL crate's own `is_authorized`, never a
new authorization model (§6).

## 2. Request/response contract

```
POST /v1/sql
{
  "sql": "SELECT id, name FROM users WHERE id = $1",
  "params": [ { "type": "integer", "value": 1 } ],
  "session_id": "3fa85f64-...—optional"
}

->

{
  "session_id": null,
  "result": {
    "kind": "rows",
    "columns": [ { "name": "id", "type": "integer", "nullable": false }, ... ],
    "rows": [ [ {"type":"integer","value":1}, {"type":"text","value":"alice"} ] ],
    "row_count": 1
  }
}
```

`result.kind` is one of `rows` (`SELECT`/`EXPLAIN`'s inner query never
executed, see §7), `write` (`INSERT`/`UPDATE`/`DELETE`, `statement` +
`rows_affected`), `ddl` (`CREATE`/`DROP TABLE`/`INDEX`/`SCHEMA`),
`explain` (`plan_text`, the real structured plan, never fabricated),
`begin`/`commit`/`rollback`.

**Typed parameters/values, never string interpolation** (item 6): every
value is a tagged JSON object (`{"type": "...", "value"|"unscaled"+
"scale"|"value_b64": ...}`), converted directly to a `rubixdb::
relational::RelationalValue` (`api/src/sql_params.rs`) and passed
*positionally* to `rubixdb_sql::exec::{execute, execute_write}`'s own
`params: &[Option<RelationalValue>]` — structurally incapable of being
concatenated into SQL text, because nothing in this path ever builds a
SQL string from a parameter value. `BIGINT`/`DECIMAL`'s unscaled
part/`TIME`/`TIMESTAMP` microsecond counts travel as JSON **strings**,
never bare JSON numbers — `JSON.parse`/`serde_json`'s own `f64`-backed
number type silently loses precision beyond 2^53, and a `BIGINT` or
`DECIMAL` value in that range is not a hypothetical (item 95's own
"large or malformed relational values must not cause... an invalid
JSON" reasoning, applied proactively rather than discovered later).
`DATE`/`TIME`/`TIMESTAMP` parameters are parsed with `rubixdb_sql::
temporal`'s own already-certified parser — never a second date/time
parser.

**A known, pre-existing (not new) characteristic**: `rubixdb-sql`
itself does not validate a supplied parameter's runtime `RelationalValue`
variant against the type the binder inferred for `$n` from its usage
context — no code path in that crate does this today, for any caller,
not only this API. Adding that check here would mean this API layer
re-implementing SQL type-checking semantics, which item 4 forbids
("never reimplement SQL semantics inside Axum"). A parameter supplied
with a semantically mismatched type reaches whatever behavior `rubixdb-
sql`'s own executor already has for that case (typically a controlled
`ExecutionParameter`/`EXECUTION_ERROR`, per that crate's own "binder
already guarantees matching types" internal-consistency assumption) —
this is `rubixdb-sql`'s own existing contract, not a gap this increment
introduces or is in scope to fix.

## 3. The HTTP transaction/session model

This was the governing directive's own flagged "critical design area."

### 3.1 Design: sessions exist only while a transaction is open

A session is created **only** by `BEGIN` and destroyed **only** by
`COMMIT`/`ROLLBACK` (or idle/lifetime expiry, §3.3). An ordinary,
non-transactional statement — the overwhelming majority of real
traffic — never touches the session registry at all: it runs through
`rubixdb_sql::exec::{execute_autocommit, execute_write_autocommit}`,
which already begin-and-commit their own throwaway `Transaction` per
call, exactly like any other caller of that crate.

This was a deliberate choice among the possible designs, not the only
one: an alternative ("every connection gets a session from its first
request, even without `BEGIN`") was rejected because it would give
every one-shot `SELECT`/`INSERT` a durable registry entry to track and
expire, for no correctness benefit — the *only* reason HTTP request
boundaries need session state to persist across them at all is an
*explicit* multi-statement transaction (item 22's own framing: "if
`BEGIN` is sent in one request and `INSERT`/`SELECT`/`COMMIT` follow in
later requests, they must remain attached to the correct session").
Tying session lifetime to transaction lifetime satisfies that exactly,
while keeping the registry's own size bounded by "how many clients
currently have an open transaction," not "how many clients have ever
connected."

### 3.2 Mechanics (`api/src/sql_session.rs`, `api/src/routes/sql.rs`)

- `SqlSessionRegistry`: `Mutex<HashMap<Uuid, SqlSession>>`, `SqlSession
  { principal, txn: Transaction, created_at, last_active }`.
- `BEGIN` with no `session_id` → `txm.begin()` + `registry.create(
  principal, txn)` → a new `Uuid::new_v4()` (CSPRNG-backed, the same
  generator `AppState::snapshots`'s own held-snapshot registry already
  uses for an identical "opaque server-issued handle" shape — never a
  counter, transaction id, or other physical/incrementing identifier,
  item 23) returned as `session_id`.
- Every other statement with a `session_id`: `registry.take(id,
  principal)` (validates ownership + not-expired in one check, §3.3),
  runs the statement against the live `&Transaction`/`&mut Transaction`,
  then `registry.put_back(...)` unless the statement was `COMMIT`/
  `ROLLBACK` (which consume it, removing the session entirely).
- A statement that *fails* inside an open transaction does **not**
  implicitly roll the transaction back (this architecture's own
  existing semantics, unchanged by this increment) — the (still-Active)
  `Transaction` is put back exactly as `Sort`/`Filter` et al. would
  leave it; the caller must explicitly `ROLLBACK` or keep going.
- `take`'s failure (`SessionLookupError::NotFound`) is the *same*
  outcome whether the id is genuinely unknown, expired, or belongs to a
  *different* authenticated principal (item 74/26/130) — the identical
  existence-hiding discipline `SqlError::UnknownObject` already applies
  one layer down, extended here to sessions.

### 3.3 Resource limits (item 24) — reused, not reinvented

`SqlSessionLimits`:

- `max_sessions_per_principal` (default 50) — bounds fan-out from one
  credential, independent of every other principal's own budget.
- `idle_timeout` (default 300s) / `max_lifetime` (default 1800s) —
  item 24/124's own "no abandoned transaction may pin a snapshot
  indefinitely." A background reaper (`spawn_reaper`, a `tokio::spawn`
  loop on a periodic `tokio::time::interval`) sweeps and rolls back
  expired sessions; `take` also defensively reaps the one session a
  request happens to touch, so expiry is never observed later than the
  next access to that specific id either way.
- **No new, disconnected global cap was invented**: every session's
  own `Transaction` already counts against `rubixdb::relational::txn::
  TxnLimits::max_concurrent_transactions` (1,000, D27's own existing
  process-wide bound) — `max_sessions_per_principal` protects fairness
  *within* that shared ceiling, not a second ceiling on top of it.

### 3.4 A real regression found and fixed: lazy catalog bootstrap

`AppState::new` originally called `catalog.bootstrap()` eagerly, once,
at process start — every deployment, whether or not SQL was ever used.
`CatalogService` persists `system.databases`/`system.schemas` rows into
the *same flat keyspace* `/v1/kv`/`/v1/range` already scan (there is no
separate catalog storage area). This broke two pre-existing, certified
`api_integration.rs` tests (`range_preserves_ordering_and_reports_
truncation`, `persists_across_a_real_restart_...`) that count keys
returned by an unbounded `/v1/range` scan, and silently falsified `/v1/
metadata`'s own `"no tables, no schema, no SQL"` claim for every
deployment. Fixed by making bootstrap **lazy**: `SqlContext::bind_
context()` bootstraps (idempotently) and resolves `(database_id,
default_schema_id)` on the *first* call from any SQL/catalog route,
caching the result (`Mutex<Option<(u32, u32)>>`) thereafter. A pure-KV
deployment that never calls `/v1/sql` or `/v1/catalog/*` now has a
byte-for-byte identical keyspace to before this increment — verified by
re-running the two previously-broken tests, which now pass unchanged.

## 4. Authentication/authorization — one boundary, reused

`POST /v1/sql`'s own role gate is a documented **exception** to the
existing HTTP-method-based rule (`api/src/auth.rs::required_role`):
this one endpoint is always `POST`, yet can carry a read-only `SELECT`
or a mutating `INSERT`/DDL — the method alone cannot distinguish them
the way it can for every other route's single fixed purpose. Gating the
whole endpoint at `Admin` would make an ordinary reader-role `SELECT`
through SQL strictly more restricted than the identical read via `GET
/v1/kv`, for no security reason. The fix: `/v1/sql` (and every `GET
/v1/catalog/*` route) requires only `Role::Reader` at the middleware
layer; the *real* per-statement decision is deferred entirely to `rubixdb_
sql::auth::is_authorized`, via one mapping function, `to_sql_auth_
context`:

```
Role::Admin  -> AuthContext::admin(principal)   (every privilege, every object)
Role::Reader -> AuthContext::reader(principal)  (SELECT on every object, plus explicit grants)
```

This is exactly the wiring `sql/src/auth.rs`'s own doc comment
anticipated ("whatever assembles a session for a principal is
responsible for mapping its own role concept onto one of these three").
No third, API-invented access tier exists. A `Reader` who submits
`INSERT`/`UPDATE`/`DELETE`/DDL is correctly rejected — not by this API,
but by the identical `resolve_table`/`is_authorized` check every other
`rubixdb-sql` caller already goes through, verified end to end in
`api/tests/api_sql_integration.rs::reader_can_select_but_not_insert_
through_sql` (which also documents a real, verified finding: denial for
a *specific* privilege — e.g. `Insert` — resolves to the same
existence-hiding `NOT_FOUND`/`UnknownObject` outcome a genuinely unknown
table would, even when the same principal can see the table via a
*different* privilege like `Select` — D25's own per-privilege
resolution, confirmed against the real binder, not assumed).

`/v1/catalog/*` routes reuse this identical mapping and the identical
`is_authorized` call for every row they return (`Privilege::Select`) —
never a second, competing authorization system (item 105).

## 5. Async runtime safety, cancellation, deadlines (item 10/11/12)

Parse/bind/plan are bounded purely by input SQL text size/complexity
(`SqlLimits`/`PlannerLimits` already cap statement bytes, expression
depth, plan node count, predicate conjuncts) — millisecond-class work
regardless of stored data volume — and run synchronously inline in the
async handler, the same pattern the existing KV routes already use for
bounded operations.

**Execution** is different: a `SELECT`/aggregate/write can run for as
long as `ExecLimits::deadline` allows (30s default, `RUBIXDB_SQL_
STATEMENT_DEADLINE_SECS`-configurable) over arbitrarily large stored
data, unbounded by anything parse-time limits can see. `routes::sql::
run_with_cancellation_and_deadline`:

1. Runs the actual `execute`/`execute_write`/`_autocommit` call inside
   `tokio::task::spawn_blocking` — never occupies an async worker
   thread for the query's real duration.
2. Passes it a `rubixdb_sql::exec::CancellationToken` (reused verbatim
   — the SQL crate's own executor already polls this at every
   operator's iteration point, built for exactly this).
3. A `CancelOnDrop` guard, held on the async handler's own stack frame
   (never moved into the blocking closure), cancels that token the
   instant the handler's future is dropped — which is exactly what
   happens when an HTTP client disconnects (axum drops the in-flight
   handler future). The abandoned blocking task notices at its own next
   `ExecCtx::check()` call and returns `SqlError::Cancelled`; nothing
   awaits its result any longer, so it is simply discarded once it
   exits.
4. Races the blocking task against `deadline + 2s` (`tokio::time::
   timeout`) as a backstop *above* the executor's own internal deadline
   check — covers the narrow case where a single underlying storage
   call runs long enough that the internal check never gets a chance to
   fire. The internal check is expected to win in the ordinary case;
   the backstop exists so no execution can run truly unbounded even if
   it does not.

Verified: `statement_exceeding_its_deadline_is_a_controlled_timeout`
(a zero-second deadline trips even a trivial `SELECT 1`, deterministically,
since `ExecCtx`'s own `deadline_at` is already in the past by the first
`check()` call — no dependency on real scan duration). Cancellation via
a real client-side `AbortController`/`fetch` abort is exercised by the
frontend's own Cancel button (`PHASE_RELATIONAL_FRONTEND_SQL_
ARCHITECTURE.md` §5) — a genuine dropped-connection test, not merely a
UI-side "hide the spinner."

## 6. `/v1/catalog/*` — read-only metadata routes

`GET /v1/catalog/databases|schemas|tables|indexes|authz`, `GET /v1/
catalog/tables/:name`. Exist because §0 proved `system.*` is not
reachable through SQL `SELECT` at all — the CLI's `\l \ls \lt \d \di
\du` have no `POST /v1/sql` query they could send. Each route:

- Calls `CatalogService`'s own existing `list_databases`/`list_schemas`/
  `list_tables`/`get_columns`/`list_indexes`/`list_grants_for_
  principal` directly — never a second index/catalog implementation.
- Applies the identical `rubixdb_sql::auth::is_authorized(...,
  Privilege::Select, ...)` check per row (`tables`/`indexes`/`describe_
  table` silently omit anything the caller cannot `SELECT` — the same
  existence-hiding discipline, never a distinguishable "forbidden" vs.
  "not found" response shape).
- Never fabricates data: `databases` returns exactly the one
  bootstrapped `"default"` row today (RubiXDB does not support multiple
  logical databases — item 29/53/98's own explicit "do not fake it");
  `\du`'s own `authz` route returns only the *authenticated caller's
  own* `system.grants` rows (RubiXDB has no separate user/role
  directory to enumerate a full list from — item 32's "no fake rows"
  extended honestly here rather than inventing one).

## 7. `EXPLAIN` (item 135)

`rubixdb_sql::exec::execute`'s own `Plan::Explain(inner)` handling
*executes* the inner statement (an existing, already-tested v1
simplification, unchanged by this increment). This API deliberately
does **not** reuse that path for `EXPLAIN`: it calls `rubixdb_sql::
plan::explain(&plan)` — the crate's own already-certified, pure,
non-executing structural-plan-to-text renderer — and returns it as a
distinct `"explain"` result kind, before any session/params/execution
logic runs at all. This is more honest (a real `EXPLAIN` should not run
the query) and sidesteps `execute_write`'s own unrelated rejection of
`Plan::Explain` entirely, while still literally satisfying item 135's
"use the actual structured plan, never fabricate" — the text comes from
`rubixdb-sql`'s own renderer, not reconstructed here.

## 8. Metrics (item 19/71/87/88)

`SqlApiMetrics` (bounded atomics): `requests`, `success`, `errors`,
`cancellations`, `deadline_exceeded`, `rows_returned`, `rows_affected`.
`SqlSessionRegistry::metrics()`: `active_sessions`. Folded into the
existing `GET /v1/metrics` response's own `sql` section, alongside
`rubixdb-sql`'s own already-bounded `SqlMetrics`/`PlannerMetrics`/
`ExecMetrics`/`WriteMetrics` snapshots (read verbatim, never
recomputed). No raw SQL text, table/column/schema name, parameter
value, row value, or principal name is ever a label or held in any of
these — every recorder takes only an already-classified count or a
small enum, structurally, not by convention (the same discipline every
metric in this codebase already applies).

## 9. Resource limits — consistency audit (item 89)

| Layer | Limit | Source |
|---|---|---|
| HTTP body | `body_size_limit()` (~1.41 MiB, derived from KV settings) | already comfortably exceeds `SqlLimits::max_statement_bytes` (1 MiB default) — verified numerically, not assumed |
| SQL statement bytes/expression depth/parameters | `SqlLimits::default()` | `rubixdb-sql`, unchanged |
| Plan nodes/joins/predicate conjuncts | `PlannerLimits::default()` | `rubixdb-sql`, unchanged |
| Result rows/materialized rows/index scan rows/group count/aggregate state bytes | `ExecLimits::default()` | `rubixdb-sql`, unchanged (Increment 9/11) |
| Transaction write-set ops/bytes, concurrent transactions | `TxnLimits::default()` | `rubixdb`, unchanged |
| SQL sessions per principal / idle / lifetime | `SqlSessionLimits` | this increment, §3.3 |
| Statement execution deadline | `ExecLimits::deadline`, `RUBIXDB_SQL_STATEMENT_DEADLINE_SECS` | this increment wires the config knob into the existing field |
| API rate limiting | `RateLimiter` (existing, per-principal token bucket) | unchanged; `/v1/sql` participates identically to every other authenticated route via the shared `auth_middleware` |

No layer bypasses another — the API never widens a limit `rubixdb-sql`/
`rubixdb` already enforce, and never introduces a second, looser
resource accounting path.

## 10. Known limitations (explicit, not silently accepted)

- Sessions are ephemeral, in-process only — a restart of `rubixdb-api`
  drops every open transaction (rolled back, never left dangling — the
  session registry's own `Drop`/process-exit semantics rely on the
  underlying `Transaction`'s own already-certified `Drop` = implicit
  rollback). This is the honest, documented contract (item 80: "do not
  claim session durability... document the actual contract"), not a gap.
- No idempotency/retry-safety primitive exists for the "client timed
  out, server actually committed" case (item 52) — the database itself
  remains correct (the commit either fully happened or fully did not,
  per D10's own already-certified atomicity), but a client that retries
  a timed-out `INSERT` without its own idempotency key can double-insert
  distinct rows exactly as it could through any other RubiXDB write
  path. This increment invents no distributed-idempotency mechanism
  (explicitly out of scope, item 52's own "do not invent").
- `\c`'s real backend capability is "reconnect to the same, only
  database" — RubiXDB does not support multiple logical database/schema
  switching via a `SET`/`USE`-style statement (no such grammar exists;
  verified by inspection, not assumed) — documented honestly as
  "HONESTLY UNSUPPORTED" beyond that, per the governing directive's own
  explicit tri-state allowance for this exact command.
