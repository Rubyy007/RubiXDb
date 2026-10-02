# PHASE RUBIXDB — FINAL SINGLE-NODE ARCHITECTURE (as certified)

**Date:** 2026-10-02 · **Base HEAD:** `b7f0e8b` (+ uncommitted fixes D-0/D-1/D-2, see `..._SECURITY.md` §6).
This document describes the product *as it actually behaves*, verified against the running binary in this session. Where it
restates earlier architecture documents it says so; nothing here supersedes them.

## 1. Product boundary
One `rubixdb` binary. **One** local instance, **one** engine owner, **one** catalog, **one** SQL engine, **one** HTTP API,
**one** embedded frontend, **one** CLI client. No router, replication, partitioning, distributed execution or remote deployment.
No RBAC / login / password ceremony in local mode: the instance credential lives in the instance directory and is never typed.

| Executable | Behavior (verified **[RUN]**) |
|---|---|
| `rubixdb gui` | find-or-start the `default` instance (or `--instance NAME`), host API + frontend on `127.0.0.1:302`, optionally open a browser (`--no-browser`). A second `gui` for the same name attaches instead of duplicating. |
| `rubixdb cli` / bare `rubixdb` | discover the running local instance (credential read from its directory), start one if none exists. `-c "SQL"`, `-f file.sql`, interactive REPL. Meta-commands `\l \ls \lt \d \di \du \conninfo \c \help \q` all returned real catalog state. |
| `rubixdb instance list|status` | enumerate / inspect local instances. |

## 2. Layering (unchanged by this phase)
```
 browser (React SPA, untrusted-content rendering) ─┐
 rubixdb cli (terminal sanitizer)  ────────────────┤  HTTP/JSON, bearer key, 127.0.0.1 only
                                                   ▼
 rubixdb-api  (axum: auth, limits, sessions<=50/principal, 5 min idle reaper, metrics, cancellation, deadlines)
        │  POST /v1/sql  (one statement per request)
        ▼
 rubixdb-sql  (parse → bind → plan → cost-based access path → execute; limits: 1 MiB/statement, depth 128, chain budget 512)
        ▼
 rubixdb relational layer  (catalog, row storage, secondary indexes, Snapshot-Isolation transactions, per-table epoch lock)
        ▼
 certified engine (WAL group commit → memtable → SSTables → manifest → compaction)   ← NOT modified in this phase
 rubixdb-instance (instance dir, lock, manifest, credentials, port 302 / ephemeral fallback, identity handshake)
```
API routes present: `/healthz`, `/readyz`, `/v1/whoami`, `/v1/instance`, `/v1/status`, `/v1/metadata`, `/v1/metrics`,
`/v1/compaction/{status,metrics}`, `/v1/kv*`, `/v1/range`, `/v1/sql`, `/v1/catalog/{databases,schemas,tables,tables/:name,indexes,authz}`
plus schema/table/index delete endpoints guarded by exact-name confirmation. There is **no** database/instance delete route.

## 3. Instance, port, identity (verified)
* Default instance dir contains `instance.json` (`instance_id`, `name`, `api_port`), `credentials.json`, `instance.lock`, `data/`.
* Default port **302** decimal (canonical `0302`), bound `127.0.0.1` only. Additional named instances got ephemeral ports
  (e.g. 54137/54140/60666), also loopback-only. Independent storage, catalog, sessions, credentials: a session opened on instance A is
  `404 SESSION_NOT_FOUND` on B; A's key is `401` on B (multi-instance test, release **and** debug).
* **Hard-kill recovery [RUN]:** `taskkill /F` of the real process mid-transaction, restart `gui`: same `instance_id`, same credentials, same data,
  committed rows and indexes intact, **uncommitted transaction rolled back**, no new database directory created.
* **Port collision with a *foreign* (non-rubixdb) process on 302 was not re-tested this session** (the endurance instance holds 302);
  the instance-manager fallback is covered by `gui_instance_integration` (passed) and the multi-instance test (second instance took an
  ephemeral port). Marked as *covered by suite, not independently re-exercised*.

## 4. Transaction semantics — **Snapshot Isolation** (not Serializable)
Demonstrated on the real product this session (**[RUN]**, two sessions on one instance):

| Scenario | Observed |
|---|---|
| **Write skew** (two txns each read `COUNT(on_call)=2`, then each set a *different* row off-call) | **Both COMMIT (200); final count 0.** The invariant "at least one on call" is violated. **Write skew occurs and is part of the product contract.** |
| Write-write conflict (both update the *same* row) | First COMMIT 200; second **409 `CONFLICT_ERROR`** ("a row this transaction wrote was modified after its snapshot") — first-committer-wins |
| Snapshot read stability | A transaction reads the same value before/after another client commits a change; a fresh read sees the new value |
| Read-your-own-writes in a transaction | `INSERT` then `SELECT` in the same session returns the row; `ROLLBACK` removes it; `COMMIT` publishes it |
| `BEGIN ISOLATION LEVEL SERIALIZABLE` | **Rejected** (`UNSUPPORTED`): the product does not offer or imply Serializable |
| `SAVEPOINT` | Rejected (`UNSUPPORTED`) |

**Contract:** applications needing cross-row invariants (like the example) must materialize the conflict (write a shared row/guard row) or
use a UNIQUE index; the engine will not detect read-write dependencies. Session cap: 50 open per principal; idle sessions rolled back after 5 minutes.

## 5. Final SQL feature matrix (every row probed against the real engine **[RUN]**; parser recognition ≠ support)
`IMPLEMENTED` = executes end-to-end, observed. `SUPPORTED` = executes with a stated restriction. `NOT IMPLEMENTED` = rejected with
`400 UNSUPPORTED` (safe, typed error). `OUT OF SCOPE` = outside the single-node local product.

| Area | Feature | Status | Evidence / restriction |
|---|---|---|---|
| DDL | CREATE TABLE (PK, NOT NULL, DEFAULT) | IMPLEMENTED | probe |
| DDL | CREATE INDEX / CREATE UNIQUE INDEX | IMPLEMENTED | UNIQUE enforced → `409 UNIQUE constraint violated` |
| DDL | CREATE SCHEMA | IMPLEMENTED | probe |
| DDL | DROP TABLE / DROP INDEX … ON table | IMPLEMENTED / SUPPORTED | `DROP INDEX name` alone rejected: needs `ON <table>` |
| DDL | DROP SCHEMA via SQL | NOT IMPLEMENTED | `UNSUPPORTED: DROP Schema` (schema delete exists as a guarded API/GUI action) |
| DDL | CREATE DATABASE | NOT IMPLEMENTED | `UNSUPPORTED: no catalog execution primitive` |
| DDL | ALTER TABLE | NOT IMPLEMENTED | probe |
| Constraints | PRIMARY KEY, NOT NULL | IMPLEMENTED | dup PK → 409; NULL → `BIND_ERROR` |
| Constraints | UNIQUE | SUPPORTED | via `CREATE UNIQUE INDEX` only; inline `UNIQUE` column option rejected |
| Constraints | FOREIGN KEY, CHECK | NOT IMPLEMENTED | `UNSUPPORTED` |
| DML | INSERT … VALUES (multi-row) | IMPLEMENTED | 2,000-row INSERT OK after D-1 |
| DML | UPDATE, DELETE | IMPLEMENTED | probe |
| DML | INSERT … SELECT | NOT IMPLEMENTED | `INSERT source must be a VALUES list` |
| DML | RETURNING (INSERT/UPDATE/DELETE) | NOT IMPLEMENTED | `UNSUPPORTED` |
| DML | UPSERT (`ON CONFLICT`, `INSERT OR REPLACE`) | NOT IMPLEMENTED | `UNSUPPORTED` |
| SELECT | WHERE / ORDER BY / LIMIT / OFFSET / DISTINCT / LIKE / IN-list / BETWEEN / CASE | IMPLEMENTED | probe |
| SELECT | EXPLAIN | IMPLEMENTED | probe |
| JOIN | INNER JOIN, LEFT JOIN | IMPLEMENTED | probe (rows 1 / 3 as expected) |
| JOIN | RIGHT, FULL, CROSS JOIN | NOT IMPLEMENTED | `join kind not in the approved grammar subset` |
| Aggregation | GROUP BY, HAVING, COUNT/SUM/MIN/MAX/AVG | IMPLEMENTED | probe + perf sweep |
| Aggregation | `COUNT(DISTINCT …)` / aggregate with DISTINCT | NOT IMPLEMENTED | `UNSUPPORTED` |
| Subqueries | IN (subquery), scalar subquery, EXISTS, derived table in FROM | NOT IMPLEMENTED | `UNSUPPORTED` (next feature phase, needs authorization) |
| CTEs | WITH | NOT IMPLEMENTED | `UNSUPPORTED: WITH` |
| Set ops | UNION, UNION ALL, INTERSECT, EXCEPT | NOT IMPLEMENTED | `query body must be a plain SELECT` |
| Window functions | `ROW_NUMBER() OVER …` | NOT IMPLEMENTED | rejected as **404 NOT_FOUND** (function lookup) — safe but the error code is misleading; recorded as a minor UX issue |
| Transactions | BEGIN / COMMIT / ROLLBACK, read-your-writes | IMPLEMENTED | Snapshot Isolation (§4) |
| Transactions | SAVEPOINT; `BEGIN ISOLATION LEVEL …` / modes | NOT IMPLEMENTED | `UNSUPPORTED` |
| Platform | Router, replication, partitioning, distributed execution, remote deployment | OUT OF SCOPE | single-node boundary |

## 6. Known, measured product limits (all enforced, safe-failing)
statement ≤ 1 MiB · expression depth 128 · operator-token budget 512 per statement (token-based after D-1) · identifier ≤ 255 · 50 sessions/principal ·
request body cap (20 MB → 413) · one statement per `/v1/sql` request. **Same-table commit throughput ≈ 270/s on this SATA SSD** because the relational
commit holds a per-table epoch write lock across the WAL fsync (see `..._PERFORMANCE.md` §4); scales with the number of distinct tables
(7.4× at 16 tables). Secondary-index reads cost ~54 µs/row (eager entry enumeration + per-row fetch; `LIMIT 1` is lazy).

## 7. What this phase changed in architecture
Nothing structural. Three small corrections (D-0 test harness, D-1 SQL pre-parse guard refinement, D-2 CLI sanitizer) and a frontend dependency
upgrade (`react-router-dom` 6 → 7). Protected engine paths show **zero diff**.
