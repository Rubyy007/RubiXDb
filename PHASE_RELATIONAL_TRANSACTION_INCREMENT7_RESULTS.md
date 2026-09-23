# PHASE_RELATIONAL_TRANSACTION_INCREMENT7_RESULTS

**Scope**: production-grade Snapshot Isolation transaction engine —
`PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` is the full decision
record; this document records what was actually built, measured, and
verified.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No SQL parser
execution, planner, optimizer, or executor exists — `sql/`'s parser/
AST/binder (Increment 6) still produces `BoundStatement` with nothing
to run it. This increment adds the transaction primitive a future
executor will use; it does not add SQL execution.

---

## 1. What was implemented

- **`src/relational/txn.rs`** (new): `TransactionManager` (`begin`,
  `autocommit_put_row`/`autocommit_delete_row`, `active_transactions`,
  `metrics`) and `Transaction` (`get_row`, `put_row`, `delete_row`,
  `commit(self)`, `rollback(self)`, `id`, `state`) implementing D10's
  Snapshot Isolation model: snapshot-pinned reads with a local write-
  set overlay (read-your-own-writes), commit-time value-based conflict
  validation, real `UNIQUE` enforcement (physical existence scan, reusing
  Increment 5's own index structure), atomic table+index commit through
  one `LsmEngine::write_batch` call, an exclusive per-table lock
  (Increment 5's own `epoch_lock`, reused) as the sole concurrency-
  control mechanism, bounded resource limits, and bounded-cardinality
  metrics.
- **`src/relational/table_store.rs`**: four previously-private helper
  functions (`validate_row_shape`, `extract_pk_values`, `index_
  maintenance_ops`, `build_put_op`) promoted to `pub(crate)` so `txn.rs`
  reuses the exact same op-computation logic `put_row`/`delete_row`
  already use — no duplicated index-maintenance algorithm.
  `put_row`/`put_rows`/`delete_row` themselves are **unmodified**.
- **`src/relational/error.rs`**: three new `RelationalError` variants —
  `Conflict`, `ResourceLimit`, `InvalidTransactionState`.
- **`src/relational/txn_tests.rs`** (new): 42 tests — see §4.
- **`benches/transaction_bench.rs`** (new): `BEGIN` latency, read-path
  overhead, commit latency vs. write-set size, commit latency vs. table
  size at a fixed write-set (conflict-validation-cost isolation),
  catalog-resolution cost vs. catalog size, concurrent-commit
  throughput scaling.
- **`Cargo.toml`**: `+1 [[bench]]` entry (`transaction_bench`).

## 2. D10 decisions implemented

See `PHASE_RELATIONAL_TRANSACTION_ARCHITECTURE.md` §1–§16 in full;
summary:

- **§1** — lifecycle: `Active`/`Committed`/`RolledBack`/`Aborted`,
  double-commit/rollback prevented at **compile time** via
  `self`-consuming `commit`/`rollback`; implicit rollback on `Drop`.
- **§2** — snapshot capture exactly once per `begin()`; read-your-own-
  writes via local-overlay-first resolution.
- **§3** — transaction id: process-local, in-memory, never persisted —
  justified (no requirement to resume a transaction across a restart;
  the engine's own `write_batch` seq is the real durability unit).
- **§4** — write-set representation, the sorted-multi-table-lock commit
  critical section, exactly one `write_batch` call per commit.
- **§5** — value-based conflict validation (`get_as_of` at snapshot seq
  vs. current); `PRIMARY KEY` conflicts caught by the same check with
  no special-casing.
- **§6** — `UNIQUE` enforcement (the critical production gate): physical
  existence scan reusing Increment 5's own index structure; intra-
  transaction duplicate-claim exclusion; self-vacated-entry exclusion
  for delete-then-reinsert; standard-SQL `NULL`-never-conflicts
  semantics (this increment's own documented choice, since no prior
  `UNIQUE` enforcement existed to inherit a rule from).
- **§7** — table/index atomic commit via shared `pub(crate)` helpers,
  one `write_batch` call.
- **§8** — rollback: O(1), zero engine calls.
- **§9** — crash recovery: no new mechanism, verified the commit
  boundary sits correctly on `write_batch`'s own certified atomicity
  across two real-restart tests.
- **§10** — Compaction interaction: real automatic-compaction fixture,
  snapshot validity confirmed throughout.
- **§11** — concurrency control: per-table exclusive lock, honestly
  reported as table-level (not key-level) serialization.
- **§12** — write skew documented and demonstrated, never claimed fixed.
- **§13** — autocommit primitive.
- **§14** — resource limits (ops/bytes/concurrent-count), checked
  before allocation.
- **§15** — security self-review.
- **§16** — performance (measured numbers in §7 below).

## 3. Files changed

`src/relational/{txn,txn_tests}.rs` (new), `src/relational/{error,mod,
table_store}.rs` (modified), `benches/transaction_bench.rs` (new),
`Cargo.toml` (+1 `[[bench]]`), `PHASE_RELATIONAL_TRANSACTION_
ARCHITECTURE.md` (new), this file (new).

## 4. Tests added

42 new tests in `relational::txn_tests`:

- **Lifecycle** (5): basic BEGIN/PUT/COMMIT; rollback discards one/many
  writes/a delete; implicit rollback on drop; compile-time double-
  commit/rollback prevention (documentation test); read-only commit.
- **Reads** (5): read-your-own-writes (put-then-read, delete-then-read,
  put-then-delete-then-put); uncommitted write invisible to another
  reader before commit; snapshot read stable against a later external
  commit.
- **Conflict detection** (4): same-key write-write conflict (loser
  identified precisely); disjoint-key transactions both succeed;
  `PRIMARY KEY` conflict, both commit orderings; a long-open read-only
  transaction never blocks unrelated commits.
- **`UNIQUE` enforcement** (6): conflict both orderings; distinct
  values both commit; `NULL` values never conflict with each other;
  intra-transaction duplicate-value rejection; delete-then-reinsert-
  same-value-in-one-transaction success; two independent `UNIQUE`
  indexes each enforced separately.
- **Table/index atomic commit** (2): exactly one `write_batch` call
  regardless of write-set size (10 rows, engine-seq-delta verification,
  reusing RA.5's own technique); index entries visible atomically with
  their row (not before commit, present together after).
- **Autocommit** (1).
- **Write skew** (1): the two-on-call-doctors scenario, directly
  demonstrated — both disjoint-write-set transactions commit, combined
  result violates the invariant neither could see alone.
- **Deterministic concurrency, barrier-synchronized, never sleep-based**
  (4): two writers same key (exactly one wins); three writers disjoint
  keys (all succeed); a writer racing a reader (reader's snapshot
  stable regardless of completion order); a rollback racing another
  transaction's commit (the committer wins cleanly).
- **Resource limits** (3): `max_write_set_ops`, `max_write_set_bytes`,
  `max_concurrent_transactions` boundaries.
- **Registry/snapshot lifetime** (2): active-count returns to zero
  across 200 begin/commit/rollback cycles; `oldest_live_snapshot_seq`
  returns to `None` once the only open transaction finishes.
- **Compaction interaction** (1): a transaction's snapshot read stays
  correct across real, automatically-triggered compaction cycles.
- **Crash recovery** (2), real process-restart simulation (fresh
  `LsmEngine::open` at the same directory): uncommitted writes never
  survive; a committed multi-row transaction with a secondary index
  survives atomically (all 5 rows, all 5 index entries).
- **Metrics** (1): started/committed/rolled-back/aborted/conflict
  counters all verified against a scripted sequence with a known
  outcome.
- **Security/authorization boundary** (1): the transaction API accepts
  only already-resolved identifiers — no raw-physical-id bypass
  surface exists for it to provide.
- **Differential/property testing** (2): a fixed scripted scenario, and
  a **randomized, interleaved `proptest`** across 3 simultaneously-open
  transaction slots (12 cases × up to 20 operations each) — both
  compared against an independently implemented Snapshot Isolation
  reference model (`ReferenceDb`/`RefTxn`, its own from-scratch
  `BTreeMap`-backed history, never calling into `crate::relational::
  txn`), asserting every commit/abort decision and final state match.

## 5. Full regression gate

| Suite | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo check --all-targets --all-features` | clean |
| `cargo test --workspace --lib` (debug) | 537 `rubixdb` + 30 `rubixdb-api` + 98 `rubixdb-sql` passed, 0 failed |
| `cargo test --release --workspace --lib` | 537 `rubixdb` + 30 `rubixdb-api` + 98 `rubixdb-sql` passed, 0 failed |
| `cargo test --test wal_tests` | 12 passed, 0 failed |
| `cargo test --test pathological_recovery_matrix` | 9 passed, 0 failed |
| `cargo test --release --test crash_consistency --features test-util` | 2 passed, 0 failed |
| `cargo test --lib relational::` | 119 passed (42 new `txn_tests` + 77 pre-existing), 0 failed |

All existing certified suites (`write_batch`, WAL, catalog, row-
storage, secondary-index, SQL parser/binder, Read Engine, Compaction)
pass unchanged.

## 6. Protected-path and security audit

```
git diff --stat -- src/manifest/ src/compaction/ src/sstable/ src/wal/ api/
```
empty for every path — **no new storage-engine primitive was
required**; the transaction engine is built entirely from already-
certified `LsmEngine::snapshot`/`get_as_of`/`range_scan`/`write_batch`,
plus Increment 5's own per-table `epoch_lock`, reused (write side) for
commit serialization.

No `unsafe` anywhere in `src/relational/txn.rs`. No `.unwrap()`/
`.expect()`/`panic!` on caller-controlled or on-disk-derived input —
every fallible path returns a typed `Result`; the one `.unwrap_or_
else(|p| p.into_inner())` is non-panicking lock-poison recovery. Every
numeric cast is a widening cast on already-bounded, already-in-memory
values, never a narrowing cast on untrusted input. Metrics are a fixed
set of atomics, structurally incapable of holding a key, value,
principal, or SQL text — no sensitive-data-in-observability surface.
No new logging. No new external dependency. Authorization boundary
unchanged: the transaction API accepts only already-resolved
`table_id`s/typed values, never a raw physical key/index id/catalog
id from an external caller, so it introduces no privilege-escalation
surface around the binder-level authorization a future SQL executor
will perform (D25, unduplicated here).

## 7. Benchmark results (measured, `cargo bench --bench transaction_bench`, this machine, release profile — raw Criterion output)

### `BEGIN` latency

| Operation | Mean time |
|---|---|
| `txn_begin` (snapshot capture + registry bookkeeping) | 148 ns |

### Read-path overhead (10,000-row table, point lookup)

| Path | Mean time |
|---|---|
| `raw_get` (unsnapshotted) | 388 ns |
| `get_as_of` (snapshot-pinned) | 383 ns |
| `Transaction::get_row` (engine hit) | 4.87 µs |
| `Transaction::get_row` (local write-set overlay) | 4.17 µs |

**Honest reading**: pinning a snapshot costs nothing measurable over an
unsnapshotted read (383 ns vs. 388 ns, within noise). Both
`Transaction::get_row` paths cost roughly the same order of magnitude
(~4–5 µs) because `resolve_table`'s own uncached catalog-lookup cost
(§ below, ~2.5–2.7 µs) dominates either path — the local-overlay
saving (skipping one ~380 ns `get_as_of` call) is real but small next
to that. This is the same "no caching added" finding `sql/benches/
sql_binder_bench.rs` already reported, reused one layer lower; not
fixed here, per that finding's own "correctness first, cache only
once measurement justifies it" precedent.

### Commit latency vs. write-set size (1,000-row table)

| Write-set size | Mean time |
|---|---|
| 1 | 3.42 ms |
| 2 | 3.39 ms |
| 4 | 3.47 ms |
| 8 | 3.59 ms |
| 16 | 3.88 ms |
| 32 | 3.98 ms |
| 64 | 4.56 ms |
| 128 | 5.12 ms |

**Honest reading**: `fsync`-dominated at every size (consistent with
every other write path this project has measured on this machine,
`PHASE_RELATIONAL_ROW_STORAGE_RESULTS.md`/`PHASE_RELATIONAL_INDEX_
INCREMENT5_RESULTS.md`'s own identical finding) — cost grows
sub-linearly-to-linearly with write-set size (3.4 ms at 1 row to 5.1 ms
at 128 rows, roughly +13 µs/row marginal), consistent with the
underlying `WriteOp` count feeding one `write_batch` call rather than
128 separate `fsync`-bound operations.

### Commit latency vs. table size, write-set size fixed at 4 rows

| Table size | Mean time |
|---|---|
| 100 | 3.40 ms |
| 1,000 | 3.51 ms |
| 10,000 | 3.39 ms |
| 50,000 | 3.63 ms |

**Confirms conflict validation does not scan the table**: flat within
noise (~3.4–3.6 ms) from 100 to 50,000 rows at a fixed write-set size —
the cost is a function of write-set size, not table size, exactly as
§5 of the architecture doc requires.

### Catalog-resolution cost vs. catalog size (`CatalogService::get_table` + `get_columns`, the exact pair `resolve_table` wraps)

| Tables in catalog | Mean time |
|---|---|
| 100 | 2.53 µs |
| 1,000 | 2.63 µs |
| 5,000 | 2.76 µs |
| 10,000 | 2.68 µs |

**Reuses `sql/`'s own prior finding**: mild, sub-linear growth with
catalog size (2.5 µs → 2.7 µs across a 100x catalog-size increase), no
automatic caching added, matching `sql/benches/sql_binder_bench.rs`'s
own `bench_bind_vs_catalog_size` shape one layer up.

### Concurrent-commit throughput, disjoint keys, one shared table

| Concurrent transactions | Mean wall time | Throughput |
|---|---|---|
| 1 | 3.59 ms | 279 commits/sec |
| 2 | 7.41 ms | 270 commits/sec |
| 4 | 15.4 ms | 260 commits/sec |
| 8 | 30.6 ms | 262 commits/sec |
| 16 | 55.6 ms | 288 commits/sec |
| 32 | 117.2 ms | 273 commits/sec |

**Honest reading, a real and deliberate limitation, not a bug**:
throughput on **one table** stays flat at ~260–290 commits/sec from 1
to 32 concurrent committing threads, because every commit against that
table serializes on the same exclusive per-table `epoch_lock` for its
entire validate+apply critical section (architecture doc §11) —
disjoint keys do not parallelize within one table. This is the direct,
measured consequence of the table-level (not key-level) locking
decision, reported here rather than hidden; different tables commit
fully in parallel (not separately re-measured — a direct consequence of
locks being acquired per touched `table_id`, sorted, with no shared
lock across tables that don't overlap).

## 8. Pre-existing failures

None newly observed. The already-documented, machine-specific
`group_commit` throughput-target misses (`PHASE_RELATIONAL_INDEX_
INCREMENT5_RESULTS.md` §5, `PHASE_RELATIONAL_TRANSACTION_STORAGE_
RESULTS.md` §10) are unrelated to this increment (`git diff --stat --
src/wal/` is empty) and not re-verified again this increment since
three consecutive increments have already independently confirmed the
same baseline on this machine.

## 9. Remaining relational work

No SQL parser execution, planner, optimizer, executor,
`SELECT`/`INSERT`/`UPDATE`/`DELETE`/`JOIN` execution, `GROUP BY`,
aggregation, CLI, HTTP API, or frontend SQL console exists — this
increment's own explicit stop condition. Transactional DDL and
catalog-key conflict detection remain out of scope (architecture doc
§15) — `CatalogService`'s own `ddl_lock`-based atomicity is unchanged
and untouched. Key-level (rather than table-level) commit concurrency
remains a documented, deliberate limitation (§7 above), not attempted
without evidence it is required.

---

## Certification

| Gate | Result |
|---|---|
| TRANSACTION LIFECYCLE | PASS |
| SNAPSHOT CAPTURE | PASS |
| READ-YOUR-OWN-WRITES | PASS |
| WRITE SET | PASS |
| CONFLICT DETECTION | PASS |
| PRIMARY KEY CONFLICTS | PASS |
| UNIQUE ENFORCEMENT | PASS |
| TABLE/INDEX ATOMIC COMMIT | PASS |
| ROLLBACK | PASS |
| AUTOCOMMIT FOUNDATION | PASS |
| CONCURRENT TRANSACTIONS | PASS (correct; table-level, not key-level, serialization — §7/§11) |
| SNAPSHOT CONSISTENCY | PASS |
| WRITE SKEW DOCUMENTED | PASS |
| CRASH RECOVERY | PASS |
| COMPACTION INTEGRATION | PASS |
| RESOURCE LIMITS | PASS |
| SECURITY | PASS |
| MEMORY BEHAVIOR | PASS (active-transaction count and snapshot registry return to baseline across all tested cycles) |
| PERFORMANCE | PASS (measured, documented, including the honestly-reported concurrency-scaling limitation) |
| PROPERTY TESTING | PASS |
| DIFFERENTIAL TESTING | PASS |

**WRITE ENGINE = PRODUCTION READY** (preserved, unmodified).
**READ ENGINE = PRODUCTION READY** (preserved, unmodified).
**COMPACTION = PRODUCTION READY** (preserved, unmodified).
**RELATIONAL DATABASE PRODUCTION READY = NO** (no SQL execution path
exists yet).

Stopping here, per this increment's own explicit scope boundary. No
planner, optimizer, executor, CLI, HTTP API, or frontend work has been
started.
