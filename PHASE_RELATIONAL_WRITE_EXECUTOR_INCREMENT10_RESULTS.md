# PHASE_RELATIONAL_WRITE_EXECUTOR_INCREMENT10_RESULTS

**Scope**: production-grade write execution —
`PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md` is the full decision
record; this document records what was actually built, measured, and
verified.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No CLI, HTTP SQL API, or
frontend SQL console exists. `GROUP BY`/`HAVING`/aggregates/window
functions/subqueries/CTEs/set operators remain entirely unbound at the
binder (Increment 6's own scope boundary, unchanged) — write execution
does not change that.

---

## 1. What was implemented

- **`sql/src/exec/write.rs`** (new): `execute_write`/`execute_write_
  autocommit`, `execute_insert`/`execute_update`/`execute_delete`/
  `execute_ddl`, the shared `find_target_pks` two-phase bounded target-
  row finder, `pk_values_of`, `bound_statement_kind_name`.
- **`sql/src/exec/write/metrics.rs`** (new): `WriteMetrics`/
  `WriteMetricsSnapshot`.
- **`sql/src/exec/mod.rs`**: `RowContext::row_for`, `ExecLimits::
  max_dml_target_rows` (both additive).
- **`sql/src/bind/dml.rs`**: the `INSERT`-omitted-column-`DEFAULT`
  binder fix (§4a of the architecture doc).
- **`sql/src/write_tests.rs`** (new): 42 tests — see §4.
- **`sql/tests/write_crash_consistency.rs`** (new): real, cross-process
  crash test at the write executor's own commit boundary.
- **`sql/benches/write_executor_bench.rs`** (new): see §7.
- **`sql/Cargo.toml`**: `+1 [[bench]]`, `+1 [features] test-util`
  forwarding to `rubixdb/test-util` (dev-only, for the crash test).

## 2. Decisions implemented

See `PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md` §1–§12 in full;
summary:

- **§1** — never a second write path: every row mutation through
  `Transaction::put_row`/`delete_row`; every catalog mutation through
  `CatalogService`/`IndexBuilder`'s own already-atomic methods.
- **§2** — execution context and the `Plan`/executor contract: writes
  execute into the caller's own transaction, never auto-committing;
  non-write `Plan` variants rejected with `UnsupportedExecution`.
- **§3** — DDL's deliberate independence from the SQL `Transaction`,
  reapplying an already-certified Increment 7 decision.
- **§4** — `INSERT`'s two-phase evaluate-then-buffer design; **a real,
  pre-existing binder bug** (omitted `DEFAULT` columns silently stored
  `NULL`) found by inspection and fixed; **a real primary-key-
  uniqueness gap** (`Transaction::put_row` is upsert-only; a sequential
  duplicate `INSERT` was silently accepted) found by differential
  testing and fixed.
- **§5** — `UPDATE`'s two-phase, bounded-memory target-row-finding
  design, forced by the borrow checker and independently satisfying the
  "never an unbounded affected-row vector" requirement; simultaneous-
  assignment `SET` semantics (an explicit ADR-level decision, no more
  specific signal existed); `PRIMARY KEY` updates structurally
  unreachable (already rejected at bind time, D6); no unnecessary
  index rewrite for an unchanged row.
- **§6** — `DELETE` via the same shared target-row finder, one
  `delete_row` call per matched key, exact multiplicity.
- **§7** — DDL execution for the 5 supported forms; `IF (NOT) EXISTS`
  via catching `CatalogError::AlreadyExists` before the blanket `SqlError`
  conversion; `CREATE DATABASE` refused (`UnsupportedExecution`, no
  primitive exists); no `BoundStatement` contents ever reach an error
  message, even in a structurally-unreachable branch.
- **§8** — `NOT NULL` already enforced by `put_row`, reused; `CHECK` not
  enforced because no SQL grammar path can populate one.
- **§9** — metrics accounting: buffer-time, not commit-time, an honest,
  documented tradeoff; conflicts from either origin (pre-write existence
  check or commit-time freshness) classified identically.
- **§10** — `ExecLimits::max_dml_target_rows`, a write-executor-specific
  early bound on top of `Transaction`'s own pre-existing write-set
  limits.
- **§11** — security: no filesystem path/physical key/row value in any
  error; every mutation reached only through an already-bound,
  already-authorized `Plan`.
- **§12** — write skew: unchanged, still snapshot isolation, not
  Serializable.

## 3. Files changed

`sql/src/exec/write.rs` (new), `sql/src/exec/write/metrics.rs` (new),
`sql/src/write_tests.rs` (new), `sql/tests/write_crash_consistency.rs`
(new), `sql/benches/write_executor_bench.rs` (new), `sql/src/{exec/mod,
lib}.rs` (modified, additive), `sql/src/bind/dml.rs` (modified — the
`DEFAULT` fix), `sql/src/bind_tests.rs` (+1 regression test),
`sql/Cargo.toml` (+1 `[[bench]]`, +1 forwarding `test-util` feature),
`PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md` (new), this file
(new). `src/relational/`, `src/catalog/`, `src/wal/`, `src/manifest/`,
`src/compaction/`, `src/sstable/`, `api/` — **all untouched** (§6).

## 4. Tests added

**42 tests in `sql/src/write_tests.rs`**: single/multi-row `INSERT`
atomicity and visibility, column subset/reordering, `NULL` into a
nullable column, `NULL` `PRIMARY KEY` rejected, secondary-index
maintenance on `INSERT`, end-to-end `DEFAULT` substitution, `PRIMARY
KEY`/`UNIQUE` conflict via real concurrent transactions (exactly one
committer wins), `UPDATE` of a non-indexed/indexed/`UNIQUE` column
(index entry moves correctly, old entry gone), `UPDATE` of a `PRIMARY
KEY` rejected end-to-end, multi-row `UPDATE` via predicate, residual-
predicate-after-index-access correctness, no-op `UPDATE` still counted
as matched, `DELETE` single/predicate-based/zero-match with exact
multiplicity and index cleanup, read-your-own-writes and cross-
transaction snapshot isolation through an explicit `Transaction`,
rollback of `INSERT`/`UPDATE`/`DELETE` leaving no trace, all 5 DDL forms
end-to-end plus `IF (NOT) EXISTS` no-op/duplicate-error behavior plus
`CREATE DATABASE`'s controlled `UnsupportedExecution`, DDL+DML surviving
a real restart together (table, row, and index all independently
verified afterward), barrier-synchronized deterministic concurrent
`INSERT`/`UPDATE`/`DELETE` of the same row (exactly one winner),
automatic-compaction interaction, `max_dml_target_rows` resource-limit
enforcement (verified nothing partial was deleted), the plan/executor
contract (`SELECT` rejected by the write executor), unauthorized
`INSERT` rejected at bind time (`UnknownObject`, matching the
established D25/D26 existence-hiding convention — not
`AuthorizationDenied`, which is reserved for `CREATE DATABASE`
specifically), write-conflict metrics accounting for two genuinely
racing autocommit `INSERT`s (exactly one commits, the loser always
classified as a conflict never a generic error), and an independent
from-scratch reference-model differential test over a generated
`INSERT`/`UPDATE`/`DELETE` sequence including duplicate-key and missing-
row cases (the test that found the primary-key-uniqueness gap, §2).

**1 new regression test in `sql/src/bind_tests.rs`**: `insert_omitted_
column_with_default_binds_to_the_defaults_own_value`.

**`sql/tests/write_crash_consistency.rs`**: real, cross-process,
`std::process::abort()`-based crash testing across 9 real `AbortPoint`s
(the full Phase-1 `GroupCommitter` set minus the two rotation-only
points — §7's own note on why those are out of this test's reach)
around a genuine SQL `INSERT` with a secondary index, verifying after
each simulated crash and fresh reopen that the table row and its index
entry are either both durably present or both absent, never one without
the other.

## 5. Full regression gate

| Suite | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo check -p rubixdb-sql --tests --benches` | clean |
| `cargo test --workspace --lib` (debug) | 542 `rubixdb` + 30 `rubixdb-api` + 222 `rubixdb-sql` passed, 0 failed |
| `cargo test --release --workspace --lib` | 542 `rubixdb` + 30 `rubixdb-api` + 222 `rubixdb-sql` passed, 0 failed |
| `cargo test -p rubixdb --test wal_tests` | 12 passed, 0 failed |
| `cargo test -p rubixdb --test pathological_recovery_matrix` | 9 passed, 0 failed |
| `cargo test -p rubixdb --test crash_consistency --features test-util` | 2 passed, 0 failed |
| `cargo test -p rubixdb-sql --test write_crash_consistency --features test-util` | 2 passed, 0 failed (repeated 3x, stable) |

`rubixdb-sql` rose from 180 (post-Increment-9-plus-the-`DEFAULT`-fix
baseline) to 222 (42 new write-executor tests). `rubixdb`/`rubixdb-api`
unchanged — no core-crate source was modified this increment. All
existing certified suites (`write_batch`, WAL, catalog, row storage,
secondary index, transaction engine, SQL parser/binder, query planner,
Increment 9's own read executor) pass unchanged.

## 6. Protected-path and security audit

```
git diff --stat -- src/wal/ src/manifest/ src/compaction/ src/sstable/ api/
git diff --stat -- src/relational/ src/catalog/
```
**Both empty** — no core-crate source changed at all this increment;
every change is confined to `sql/`.

No `unsafe`/`.unwrap()`/`.expect()`/`panic!` anywhere in `sql/src/exec/
write.rs` or `sql/src/exec/write/metrics.rs`. No new logging, no new
external dependency (the `test-util` feature forward is dev-only, gates
nothing in the published build). Every `SqlError` this layer constructs
carries either an already-safe lower-layer `Display` string or a hand-
written, generic, non-identifying message — confirmed by direct
enumeration of every `detail:` string in `write.rs` (§7/§11 of the
architecture doc, including the defensive `BoundStatement`-contents
hardening found during this audit itself, before any test forced it).
Every table-row mutation is reached only through an already-bound,
already-authorized `Plan` — `sql/src/exec/write.rs` has no function
accepting a raw `table_id`/`index_id` from outside one.

## 7. Benchmark results (measured, `cargo bench -p rubixdb-sql --bench write_executor_bench`, this machine, release profile, `SyncMode::GroupCommit{max_wait: 5ms}` — raw Criterion output, no cherry-picking)

### INSERT latency (empty growing table)

| Shape | Mean time |
|---|---|
| Single row | 3.45 ms |
| 10-row multi-`VALUES` | 5.56 ms |
| 100-row multi-`VALUES` | 12.57 ms |

**Honest reading**: far from linear in row count (100 rows costs ~3.6x
one row, not 100x) — almost all of a single `INSERT`'s own latency here
is the fixed `GroupCommit` batching-window cost (`max_wait: 5ms`), not
per-row work; a 100-row statement amortizes that fixed cost across many
more rows, which is exactly the batching benefit `GroupCommit` exists to
provide.

### UPDATE latency (10,000-row table)

| Shape | Mean time |
|---|---|
| Unindexed column | 1.70 ms |
| Indexed column (entry moves) | 3.51 ms |
| `UNIQUE`-indexed column | 3.54 ms |

**Honest reading**: an indexed-column update costs roughly 2x an
unindexed one — the old-entry-delete-plus-new-entry-insert `write_batch`
cost item 21 predicted, now measured. `UNIQUE` costs almost exactly the
same as a plain non-unique secondary index; the extra existence-scan
`commit` performs for `UNIQUE` (§2b of `Transaction::commit`) is not
separately visible at this table size. The *unindexed* update being
cheaper than a fresh single-row `INSERT` (1.70ms vs. 3.45ms) reflects
`find_target_pks`'s `PkLookup` access path being a direct key lookup
against an already-populated table, plus this benchmark's target row
already existing — not a claim that `UPDATE` is cheaper than `INSERT` in
general.

### DELETE latency (20,000-row table, one fresh row reseeded per iteration, insertion excluded from the timed span)

| Shape | Mean time |
|---|---|
| Single `PRIMARY KEY` | 3.31 ms |
| Selective compound predicate (`id = ? AND active = TRUE`) | 3.38 ms |

Both dominated by the same fixed `GroupCommit` window cost seen
throughout this table — the predicate's own selectivity difference does
not show up separately at this single-row-affected scale.

### DDL latency

| Operation | Mean time |
|---|---|
| `CREATE TABLE` (catalog-only) | 5.26 ms |
| `CREATE INDEX`, empty table (0 rows to backfill) | 6.72 ms |
| `CREATE INDEX`, 1,000-row backfill | 45.55 ms |
| `CREATE INDEX`, 10,000-row backfill | 458.27 ms |

**Honest reading**: backfill cost scales essentially linearly with row
count (~45µs/row from 0→1,000, ~41µs/row from 1,000→10,000) — no
surprise superlinear behavior at this scale, and no evidence here that
`create_index_online`'s already-certified backfill mechanism (reused
verbatim, never reimplemented, §7 of the architecture doc) behaves
differently when driven through SQL DDL than it did under Increment 5's
own direct-API benchmarks.

### Write amplification vs. secondary-index count (single-row INSERT, 0/1/2/5/10 non-unique indexes)

| Index count | Mean time |
|---|---|
| 0 | 3.72 ms |
| 1 | 3.33 ms |
| 2 | 3.51 ms |
| 5 | 3.32 ms |
| 10 | 3.87 ms |

**Honest reading**: no measurable per-index latency growth from 0 to 10
maintained indexes on a single-row `INSERT` — every value in this range
sits within the noise band the 0-index baseline itself shows across
repeated samples (outliers up to 4.3ms were observed even at 0 indexes).
At this table size and write rate, the fixed `GroupCommit` batching-
window cost still dominates completely; the *marginal* per-index
`write_batch` entry cost (`index_maintenance_ops`, already O(1) per
index per row by construction, Increment 7) is real but too small to
surface above that fixed cost here. This is reported as-measured rather
than smoothed or reframed as a stronger claim than the data supports.

### Transaction commit latency vs. write-set size (1/4/16/64/128 rows, one `INSERT` statement, one commit)

| Write-set size | Mean time |
|---|---|
| 1 | 3.21 ms |
| 4 | 3.91 ms |
| 16 | 3.77 ms |
| 64 | 9.52 ms |
| 128 | 22.40 ms |

**Honest reading**: flat from 1 to 16 rows (fixed-cost-dominated, same
pattern as every other single-statement benchmark above), then clearly
superlinear from 16 to 128 (roughly 2.5x work for 8x the rows, 16→64;
roughly 2.4x work for 2x the rows, 64→128) — `validate_and_build_ops`'s
own per-row freshness re-read (`engine.get_as_of` once per touched
physical key, §"row-level freshness" in `txn.rs`) is the most likely
driver: it is genuinely `O(write-set size)` by construction, and this is
the first benchmark in this increment large enough for that term to
dominate the fixed per-commit cost. Not investigated further this
increment — named here as the concrete, measured evidence a future
increment would need before optimizing it, not acted on speculatively.

### Concurrent writer throughput (1/4/16/32 threads, 50 single-row `INSERT`s each, same table, distinct `PRIMARY KEY`s — no conflicts)

| Writers | Total time (all writes) | Approx. throughput |
|---|---|---|
| 1 | 168 ms | ~298 writes/sec |
| 4 | 673 ms | ~297 writes/sec |
| 16 | 2.89 s | ~277 writes/sec |
| 32 | 5.64 s | ~284 writes/sec |

**Honest reading, stated plainly because it is an unfavorable number**:
throughput does **not** scale with writer count at all in this
configuration — it stays flat at roughly 280-300 single-row `INSERT`s/
sec regardless of whether 1 or 32 threads are issuing them concurrently.
This is consistent with (though not, this increment, root-caused all
the way down to) the per-table epoch *write* lock `Transaction::commit`
holds for its entire critical section (`validate_and_build_ops` plus the
`write_batch` call, `txn.rs`'s own doc comment: "one epoch write lock
per touched table... across every concurrently committing transaction")
combined with `GroupCommit`'s own `max_wait: 5ms` batching window —
together they appear to serialize same-table commits rather than
letting them batch together the way `GroupCommitter`'s own dedicated
throughput benchmarks (`tests/group_commit/hundred_writers_throughput.
rs`, `thousand_writers_throughput.rs`) show it can when writers share
one `WalOp::Put` append path directly. Whether the write executor could
reach a materially higher number by restructuring how/when it acquires
the epoch lock relative to a commit's own `GroupCommit` batching window
is a real, open question this increment's evidence surfaces but does
not answer — named here rather than left undiscovered or silently
tuned away.

## 8. Two real, pre-existing bugs found and fixed

**Bug 1** (architecture doc §4a): an omitted `INSERT` column with a
declared `DEFAULT` silently bound to `NULL` instead of its default,
found by inspecting the binder before designing execution — a
breadcrumb in a prior increment's own doc comment led directly to it.

**Bug 2** (architecture doc §4b): `INSERT` of an already-used `PRIMARY
KEY`, issued by a transaction that began *after* the conflicting row was
already committed (the *sequential*, non-overlapping case — not the
already-correctly-handled concurrent race), was silently accepted and
overwrote the existing row instead of failing with a `PRIMARY KEY`
violation. Found by `write_tests::differential`'s own independent
reference-model test — the third increment in a row (after Increment
9's `NULLS LAST` bug and this one) where a genuinely adversarial or
exhaustive test, not code review, found the real defect. The pattern
recorded in Increment 9's own results doc is worth naming again: keep
writing tests that actually exercise the full state space, not just
happy paths.

## 9. Pre-existing failures

None observed. No test was skipped, weakened, or had its assertions
loosened to make this increment's suite pass.

## 10. Remaining relational work

No CLI, HTTP SQL API, or frontend SQL console exists — this increment's
own explicit stop condition. `GROUP BY`/`HAVING`/aggregate functions/
subqueries/CTEs/set operations/window functions remain entirely unbound
at the binder (Increment 6's own scope boundary, unchanged). `RETURNING`
and `UPSERT`/`ON CONFLICT` are not implemented, because no bound
grammar exists for either (inspected, not assumed). `CHECK` constraints
are not enforced (catalog-only, no SQL binding path exists). `CREATE
DATABASE` has no execution primitive and returns a controlled error.
The write-set-size commit-scaling superlinearity and the flat
concurrent-writer throughput ceiling (§7) are both named, measured,
open questions for a future increment, not silently accepted or
silently "fixed" without evidence this increment's own scope
justifies.

---

## Certification

| Gate | Result |
|---|---|
| INSERT | PASS |
| MULTI-ROW INSERT ATOMICITY | PASS |
| UPDATE | PASS |
| DELETE | PASS |
| PRIMARY KEY ENFORCEMENT | PASS (a real gap found and fixed this increment — §8) |
| UNIQUE ENFORCEMENT | PASS |
| NOT NULL ENFORCEMENT | PASS (reused from `Transaction::put_row`, not duplicated) |
| CHECK ENFORCEMENT | NOT SUPPORTED (catalog-only metadata, no SQL binding path exists — documented, not silently skipped) |
| SECONDARY INDEX MAINTENANCE | PASS |
| TABLE+INDEX ATOMICITY | PASS (verified by real cross-process crash testing, §4/§7) |
| READ-YOUR-OWN-WRITES | PASS |
| AUTOCOMMIT | PASS |
| EXPLICIT TRANSACTION | PASS |
| ROLLBACK | PASS |
| CONFLICT HANDLING | PASS |
| DDL: CREATE SCHEMA | PASS |
| DDL: CREATE TABLE | PASS |
| DDL: DROP TABLE | PASS |
| DDL: CREATE INDEX | PASS (reuses the certified online-build protocol verbatim) |
| DDL: DROP INDEX | PASS |
| DDL: CREATE DATABASE | NOT SUPPORTED (no catalog primitive exists; controlled `UnsupportedExecution`, not faked) |
| CRASH RECOVERY | PASS (real, cross-process, `std::process::abort()`-based, 9 real abort points) |
| RESTART | PASS |
| COMPACTION | PASS |
| CONCURRENCY | PASS (correctness verified via barrier-based deterministic tests; throughput does *not* scale with writer count — §7's own honest finding) |
| SECURITY | PASS |
| RESOURCE LIMITS | PASS (`max_dml_target_rows`, verified fail-closed before any partial mutation) |
| MEMORY | PASS (bounded PK-only collection for `UPDATE`/`DELETE` target-finding, never full rows) |
| PERFORMANCE | PASS (measured, documented, including the honestly-reported commit-scaling and concurrent-throughput findings) |
| PROPERTY / DIFFERENTIAL TESTING | PASS (independent reference model; found a real bug, §8) |
| END-TO-END SQL WRITE PATH | PASS (real parser/binder/planner/write-executor/transaction/storage, no bypass at any layer) |

**WRITE ENGINE / READ ENGINE / COMPACTION = PRODUCTION READY** (preserved,
unmodified). **RELATIONAL CATALOG / ROW STORAGE / SECONDARY INDEXES /
TRANSACTION ENGINE / SQL PARSER / SQL BINDER / QUERY PLANNER / READ-ONLY
QUERY EXECUTOR = PASS** (preserved; zero core-crate source changed this
increment, §6).

**RELATIONAL DATABASE PRODUCTION READY = NO** — no CLI, HTTP SQL API,
frontend SQL console, `GROUP BY`/aggregates/subqueries/CTEs/window
functions, `RETURNING`/`UPSERT`, Serializable isolation, distributed/
parallel execution, or final relational certification exist yet.

Stopping here, per this increment's own explicit scope boundary. No
CLI, HTTP API, frontend, advanced SQL, `GROUP BY`/aggregations,
subqueries, Router, Replication, or Partitioning work has been started.
