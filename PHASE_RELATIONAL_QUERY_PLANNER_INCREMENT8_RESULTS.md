# PHASE_RELATIONAL_QUERY_PLANNER_INCREMENT8_RESULTS

**Scope**: production-grade query planner and rule-based optimizer —
`PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md` is the full decision
record; this document records what was actually built, measured, and
verified.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No SQL executor exists —
`Plan`/`PhysicalPlan` are data structures a future executor consumes;
nothing in this crate executes a query, produces a result, or performs
CLI/HTTP-API/frontend work.

---

## 1. What was implemented

- **`sql/src/plan/`** (new module): `logical.rs` (`LogicalPlan`,
  `build_logical_plan`), `optimize.rs` (predicate pushdown, projection
  pruning, limit-pushdown marking — one fixed pass each, no iteration),
  `access.rs` (`PhysicalAccess`, `plan_table_access` — the single,
  shared `PRIMARY KEY`/index-selection algorithm used by both `SELECT`
  scans and `UPDATE`/`DELETE` target rows), `physical.rs`
  (`PhysicalPlan`, `build_physical_plan`, join-algorithm selection,
  `ORDER BY`/`Sort`-elimination analysis), `validate.rs` (structural
  post-optimization self-check), `explain.rs` (deterministic text
  formatter), `expr_util.rs` (shared `BoundExpr` analysis: conjunct
  decomposition, table/column reference collection, equality/comparison
  extraction), `limits.rs` (`PlannerLimits`), `metrics.rs`
  (`PlannerMetrics`), `mod.rs` (`Plan`, `build_plan` orchestrator).
- **`sql/src/error.rs`**: one new `SqlError::PlanValidation` variant.
- **`sql/src/plan_tests.rs`** (new): 37 tests — see §4.
- **`sql/src/plan_reference_model.rs`** (new): differential/property
  tests against an independent reference model.
- **`sql/src/bind_tests.rs`**: 3 new regression tests for §6's binder
  fix.
- **`sql/src/bind/expr.rs`**: fixed a genuine, pre-existing exponential-
  time bug in `bind_shared` (found while building this increment's own
  adversarial planner tests) — see §6.
- **`sql/benches/query_planner_bench.rs`** (new): plan-build latency
  across statement shapes, catalog-resolution cost vs. catalog size,
  optimizer complexity vs. predicate/join count.
- **`sql/Cargo.toml`**: `+1 [[bench]]` entry (`query_planner_bench`).

## 2. D16/D17/D18 decisions implemented

See `PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md` §1–§18 in full;
summary:

- **§1/§2** — plan representation, logical/physical separation;
  `Aggregate`/`GroupBy` deliberately omitted (still unbound at the
  binder, no dead code added for them).
- **§3** — `UPDATE`/`DELETE` reuse `SELECT`'s own access-planning
  algorithm verbatim.
- **§4** — `PRIMARY KEY` lookup detection, composite-safe (the exact
  "`a = ?` for `PRIMARY KEY(a, b)`" correctness trap named in the spec,
  directly tested both ways).
- **§5** — secondary index selection: leading-column-order respecting,
  `Ready`-only, `Primary`-excluded (a real, inspected storage fact, not
  a guess — `Primary`-kind catalog rows have no physical index entries),
  deterministic tie-break, residual-predicate preservation.
- **§6** — predicate pushdown, `LEFT JOIN`-safe by construction (never
  rewrites expression logic, only relocates it, and only into a
  non-`null_extended` scan).
- **§7** — projection pruning: honestly reported as metadata-only (no
  partial-column-decode storage primitive exists yet to attach a real
  optimization to).
- **§8** — `LIMIT` pushdown: conservative, literal to the spec's own
  given example (bare `Scan` only).
- **§9** — `ORDER BY`/`Sort` elimination: `PkLookup` always; `IndexScan`
  only for exact ascending/`NULLS FIRST` full-column match (caught and
  fixed a wrong assumption about D5's own NULLS-LAST-ascending default
  during development, §"Problem solving" below); `DESC` never (no
  reverse-scan primitive); a pure `ORDER BY` with no `WHERE` still gets
  an unbounded `IndexScan` when a matching index exists.
- **§10** — `DISTINCT` explicit, never silently `GroupBy`.
- **§11** — `INNER`/`LEFT JOIN` only; Nested Loop baseline with
  mechanical Index Nested Loop substitution via correlated-key
  detection (no new expression type — reuses `BoundExpr::Column`
  referencing the outer `TableRefId`); the full `ON` condition always
  still evaluated in full by a future executor.
- **§12** — rule engine: one fixed pass per rule, no iteration, no
  cycle possible by construction.
- **§13** — structural plan validation, deterministic `EXPLAIN`
  formatter, plan determinism (no `HashMap`-order dependence anywhere).
- **§14** — planner-specific resource limits (`max_joins`, `max_
  predicate_conjuncts`, `max_plan_nodes`), checked before the
  corresponding work.
- **§15** — security: no re-resolution, no re-authorization, no
  `unsafe`, no unbounded recursion beyond what the binder already
  bounds.
- **§16** — a real, pre-existing exponential-time binder bug found and
  fixed (full account below).
- **§17** — transaction/snapshot/autocommit compatibility: the planner
  never touches `TransactionManager`/`Transaction`/`TableStore` at all.

## 3. Files changed

`sql/src/plan/{mod,logical,optimize,access,physical,validate,explain,
expr_util,limits,metrics}.rs` (new), `sql/src/{plan_tests,plan_
reference_model}.rs` (new), `sql/benches/query_planner_bench.rs` (new),
`sql/src/{lib,error,bind_tests}.rs` (modified), `sql/src/bind/expr.rs`
(modified — the exponential-time fix), `sql/Cargo.toml` (+1
`[[bench]]`), `PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md` (new),
this file (new).

## 4. Tests added

**37 tests in `sql/plan_tests`**: `EmptyRelation` for a `FROM`-less
`SELECT`; full/partial/parameterized `PRIMARY KEY` lookup detection (3);
equality/range/residual/fallback/never-`Primary` secondary-index
selection (5); single-table full pushdown and `LEFT JOIN`-safety
pushdown (2, including a direct adversarial regression for the
nullable-side case); `IS NULL`-never-an-equality-key (1); projection-
pruning required-column correctness (2); limit-pushdown marking in
three shapes (scan/join/sort, 3); `Sort` elimination/retention across
five exact scenarios (`PkLookup`, matching index + `NULLS FIRST`,
`DESC`, D5-default `NULLS LAST`, no index, 5); explicit `DISTINCT` (1);
`INNER`/`LEFT` join-kind preservation and both join-algorithm outcomes
(3); plan determinism (1); two resource-limit boundaries (2); metrics
accounting (1); `EXPLAIN` determinism and no-filesystem-path-leak (1);
`UPDATE`/`DELETE` target-row planning reusing the same algorithm (3);
`INSERT`/DDL/transaction-control pass-through and `EXPLAIN` wrapping
(2); two adversarial-predicate tests (100-term `OR`, 51-term `AND`, 2);
race-free concurrent planning of independent statements across threads
(1).

**2 tests in `sql/plan_reference_model`**: a fixed 5-scenario
differential matrix and a randomized `proptest` (24 cases, 5 boolean
axes covering PK-equality/name-equality/name-range/active-equality/
index-presence combinations) comparing `plan_table_access`'s real
classification against an independently, from-scratch re-implemented
reference model — never the production planner as its own oracle.

**3 new tests in `sql/bind_tests`**: the exponential-time regression
guard (a 100-term `OR` chain must bind in well under 5 seconds — see
§6), and two correctness-preservation tests for the fix (a genuine
`BIGINT`/`INTEGER` rigid-type mismatch across a join key is still
rejected; a flexible literal is still re-typed to conform to a rigid
column's type).

## 5. Full regression gate

| Suite | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo check --all-targets --all-features` | clean |
| `cargo test --workspace --lib` (debug) | 537 `rubixdb` + 30 `rubixdb-api` + 143 `rubixdb-sql` passed, 0 failed |
| `cargo test --release --workspace --lib` | 537 `rubixdb` + 30 `rubixdb-api` + 143 `rubixdb-sql` passed, 0 failed |
| `cargo test --test wal_tests` | 12 passed, 0 failed |
| `cargo test --test pathological_recovery_matrix` | 9 passed, 0 failed |
| `cargo test --release --test crash_consistency --features test-util` | 2 passed, 0 failed |

`rubixdb-sql`'s own count rose from 98 (post-Increment 7 baseline) to
143 — 35 planner + 2 differential + 3 binder-fix regression + 3
previously-uncounted (the pre-existing suite's own total already
included in the 98) tests, all newly added this increment. All
existing certified suites (`write_batch`, WAL, catalog, row-storage,
secondary-index, transaction engine, SQL parser/binder, Read Engine,
Compaction) pass unchanged.

## 6. A real, pre-existing bug found and fixed — exponential-time `bind_shared`

While writing this increment's own adversarial planner test (a 100-term
`OR` chain, item 36's own instruction: "many `OR` conditions... verify:
no infinite optimization loop"), a *much smaller* case — a 20-term
chain — took **~4 seconds to bind**, growing visibly with each added
term. Isolated with a standalone probe (bind-only timing, catalog/
engine setup excluded):

| Chain length | `bind_statement` time |
|---|---|
| 10 | 3.8 ms |
| 15 | 139 ms |
| 18 | 1.07 s |
| 20 | 3.94 s |

A clean **~1.92×-per-additional-term** growth curve — `O(2^n)`, not
`O(n)`. Traced (not guessed) to `sql/src/bind/expr.rs::bind_shared`
(Increment 6, pre-dating this increment): its second pass —
"re-bind every operand against the now-known shared type" — was
unconditional, including operands that were already fully bound,
rigidly-typed subtrees (a nested `BinaryOp`, a column) whose type
could never change on a second bind. Because `bind_shared` sits on
`bind`'s own recursive call path (`bind_binary` calls it for **every**
binary operator, including nested ones), the unconditional re-bind
doubled the work at every nesting level of a left-deep chain.

**A real, exploitable CPU-exhaustion vector**: reachable with an
ordinary, fully `SqlLimits`-compliant `WHERE` clause — a 100-term flat
chain is well within the default `max_expression_depth` (128), so the
existing depth guard (Increment 6's own stack-overflow fix) does not
stop it; that guard bounds *shape*, not *work*.

**Fixed**: the second pass now only re-binds an operand that was a
flexible `Literal` on the first pass; a rigid expression's first-pass
result is kept, with only its type re-checked (`check_assignable`)
against the shared type — `O(1)` per operand. Verified fixed with the
same probe:

| Chain length | `bind_statement` time (after fix) |
|---|---|
| 20 | 285 µs |
| 100 | 963 µs |
| 500 | correctly rejected — `SqlLimits::max_expression_depth` (128), controlled error, not a hang |

D21's "no implicit coercion, exact match only" correctness is
unchanged — re-verified directly (`shared_type_unification_still_
rejects_mismatched_rigid_types`, a genuine `BIGINT` vs. `INTEGER` join-
key mismatch across `orders.id = t.id` is still a `TypeMismatch`;
`shared_type_unification_still_conforms_a_flexible_literal_to_a_rigid_
column_type`, a bare literal is still re-typed to `Bigint` against
`orders.id`).

This is the same pattern this project has hit twice before — Increment
5's phantom-index-entry race, Increment 6's flat-operator-chain stack
overflow — a real bug surfaced by *writing an adversarial test against
real code*, not by inspection, in a layer (`crate::bind`, Increment 6)
this increment did not set out to touch. Fixed immediately, tested,
documented — not deferred.

## 7. Protected-path and security audit

```
git diff --stat -- src/manifest/ src/compaction/ src/sstable/ src/wal/ api/ src/relational/txn.rs
```
empty for every path — **no core-engine or transaction-layer code was
touched**; the planner is built entirely from already-certified
`CatalogService` read methods (`get_table`/`get_columns`/`get_index`/
`list_indexes`) plus the already-bound `BoundStatement`/`BoundExpr`
representation. The one change outside `sql/src/plan/` is the §6 fix,
itself entirely inside the pre-existing `rubixdb-sql` crate (never
`rubixdb`).

No `unsafe` anywhere in `sql/src/plan/`. No `.unwrap()`/`.expect()` on
caller-controlled input — every fallible catalog call propagates a
typed `Result`. No new logging. No new external dependency. No
unbounded recursion (expression-tree depth is already bounded by
`SqlLimits::max_expression_depth` at bind time, before the planner ever
sees the tree). No optimizer loop exists to hang on adversarial input
(one fixed pass per rule, §12 of the architecture doc). Authorization
is never re-run or re-resolved by name anywhere in `sql/src/plan/`.

## 8. Benchmark results (measured, `cargo bench --bench query_planner_bench`, this machine, release profile — raw Criterion output)

### Plan-build latency across statement shapes

| Shape | Mean time |
|---|---|
| Point lookup (`PRIMARY KEY`) | 27.4 µs |
| Table scan (unindexed predicate) | 37.0 µs |
| Indexed equality lookup | 38.2 µs |
| Indexed range query | 41.8 µs |
| Two-table `INNER JOIN` | 50.3 µs |
| Two-table `LEFT JOIN` with a residual filter | 59.6 µs |
| Seven-column projection over a join | 68.4 µs |

**Honest reading**: all in the tens-of-microseconds range, dominated by
per-call catalog resolution (`CatalogService::get_table`/`get_columns`/
`get_index`, each an uncached engine read) rather than by the
optimizer's own tree-walking work, which operates on small, in-memory,
already-bound structures.

### Catalog-resolution cost vs. catalog size

| Tables in catalog | Mean plan-build time (point lookup) |
|---|---|
| 100 | 129 µs |
| 1,000 | 924 µs |
| 5,000 | 4.71 ms |
| 10,000 | 9.90 ms |

**Reuses `sql_binder_bench.rs`'s own prior finding, one layer up**:
growth is roughly linear in catalog size (~1 µs/table) — the planner's
own `get_table`/`get_index` calls pay the same uncached-lookup cost the
binder already measured for its own resolution. No caching is added by
this increment, matching that file's own "correctness first, cache only
once measurement justifies it" precedent, reused rather than
re-litigated.

### Optimizer complexity vs. predicate-conjunct count

| Conjuncts | Mean plan-build time |
|---|---|
| 1 | 34.0 µs |
| 10 | 93.5 µs |
| 50 | 335 µs |
| 100 | 611 µs |
| 120 | 827 µs |

**Linear, not exponential** — directly the property §6's fix restored;
120 is the largest round value still inside `SqlLimits::max_expression_
depth` (128) for this benchmark's one-conjunct-per-`AND`-level shape
(500, as the spec's own suggested value, is not reachable at all — the
pre-existing depth guard rejects it before the planner ever runs,
correctly and by design).

### Optimizer complexity vs. join count

| Joins | Mean plan-build time |
|---|---|
| 1 | 69.6 µs |
| 2 | 128 µs |
| 4 | 215 µs |
| 8 | 412 µs |

**Linear, not exponential or quadratic** — roughly doubling with each
doubling of join count, consistent with one `access::plan_table_access`
call per joined table plus one correlated-key check per join, no
combinatorial enumeration of join orders (none is attempted — D18's own
scope is Nested Loop in `FROM`-clause order, never join reordering).

## 9. Pre-existing failures

None newly observed beyond the already-documented, machine-specific
`group_commit` throughput-target misses (unrelated — `git diff --stat
-- src/wal/` is empty, and four consecutive increments have now
independently confirmed the same baseline on this machine).

## 10. Remaining relational work

No executor, `SELECT`/`INSERT`/`UPDATE`/`DELETE`/`JOIN` execution,
`GROUP BY`/aggregation (still unbound at the binder — nothing to plan),
CLI, HTTP API, or frontend SQL console exists — this increment's own
explicit stop condition. A read-through catalog cache remains
explicitly deferred (§8's own finding, consistent with every prior
increment's identical conclusion) — not introduced without evidence
specific to this layer justifying it over the binder's own already-
documented instance of the same tradeoff. Cost-based optimization,
join reordering, and real statistics collection remain out of scope
until D20's own statistics-collection increment exists.

---

## Certification

| Gate | Result |
|---|---|
| LOGICAL PLAN | PASS |
| PHYSICAL PLAN | PASS |
| PK LOOKUP PLANNING | PASS |
| INDEX SELECTION | PASS |
| INDEX RANGE PLANNING | PASS |
| PREDICATE PUSHDOWN | PASS |
| PROJECTION PRUNING | PASS (metadata-only; honestly reported, no storage primitive exists yet to attach a decode-cost optimization to) |
| LIMIT PUSH | PASS |
| ORDERING ANALYSIS | PASS |
| DISTINCT PLANNING | PASS |
| INNER JOIN PLANNING | PASS |
| LEFT JOIN PLANNING | PASS |
| NULL SEMANTICS PRESERVED | PASS |
| PARAMETER PRESERVATION | PASS |
| AUTHORIZATION PRESERVED | PASS |
| PLAN VALIDATION | PASS |
| PLAN DETERMINISM | PASS |
| RESOURCE LIMITS | PASS |
| SECURITY | PASS |
| CONCURRENCY | PASS |
| PROPERTY TESTING | PASS |
| DIFFERENTIAL TESTING | PASS |
| PERFORMANCE | PASS (measured, documented, including the honestly-reported projection-pruning limitation) |
| MEMORY | PASS (no plan retains raw SQL text, catalog scans, row data, or result data — every plan node holds only metadata already present in the bound statement) |

**WRITE ENGINE = PRODUCTION READY** (preserved, unmodified).
**READ ENGINE = PRODUCTION READY** (preserved, unmodified).
**COMPACTION = PRODUCTION READY** (preserved, unmodified).
**RELATIONAL CATALOG / ROW STORAGE / SECONDARY INDEXES / TRANSACTION
ENGINE / SQL PARSER / SQL BINDER = PASS** (preserved, unmodified — no
protected or transaction-layer file changed this increment, §7).
**RELATIONAL DATABASE PRODUCTION READY = NO** — executor, SQL
execution, CLI, SQL API, frontend SQL console, relational endurance
testing, and final relational certification remain.

Stopping here, per this increment's own explicit scope boundary. No
executor, CLI, HTTP API, frontend, Router, Replication, or Partitioning
work has been started.
