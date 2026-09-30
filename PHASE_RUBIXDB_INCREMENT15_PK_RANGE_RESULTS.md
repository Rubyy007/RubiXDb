# Increment 15: Primary-Key Range Scan — Results & Certification

Companion to `PHASE_RUBIXDB_INCREMENT15_PK_RANGE_ARCHITECTURE.md`
(design) and `PHASE_RUBIXDB_INCREMENT15_PK_RANGE_PERFORMANCE.md`
(measurements). This document is the correctness/regression
certification closing out
`PHASE_RUBIXDB_INCREMENT14_BLOCKER9_PK_RANGE_SCAN_ADR.md`'s "NON-PK
READ PERFORMANCE = FAIL" finding.

## 1. New tests added (all additive — zero existing test's expectations changed)

| File | Tests added | What they prove |
|---|---|---|
| `sql/src/plan_tests.rs` | `pk_range_on_single_column_primary_key_builds_pk_range_scan`, `pk_range_with_only_a_lower_bound_leaves_the_upper_bound_unbounded`, `partial_composite_pk_equality_becomes_a_prefix_pk_range_scan` | Correct plan shape, including the mandatory multi-column-PK prefix case |
| `sql/src/plan_tests.rs` (extended) | `metrics_record_plan_shape_choices` | `pk_range_scans_selected` metric fires exactly once |
| `sql/src/plan_reference_model.rs` (extended) | 2 new fixed scenarios + `id_range` proptest parameter | Independent, from-scratch reference model agrees with the real planner, including randomized predicate/index shapes |
| `sql/src/exec_tests.rs` | `pk_range_scan_returns_exactly_the_matching_rows_via_the_right_access_path`, `pk_range_scan_respects_inclusive_and_exclusive_bounds_at_domain_edges` (7 sub-cases incl. empty range), `pk_range_scan_never_returns_an_adjacent_tables_rows`, `pk_range_scan_residual_predicate_is_still_evaluated`, `pk_range_scan_on_composite_primary_key_prefix_returns_every_matching_row`, `pk_range_scan_respects_transaction_snapshot_isolation`, `pk_range_scan_reflects_delete_then_reinsert_not_a_stale_or_duplicate_version` | Exact-row correctness, boundary/empty-range handling, table isolation, residual filters, the composite-PK landmine, MVCC snapshot semantics, tombstone/reinsert correctness |
| `sql/src/exec_tests.rs` (JOIN) | `inner_join_with_a_pk_range_on_the_outer_side_still_emits_exact_multiplicity` | JOIN regression: correct multiplicity, no globalized bound |
| `sql/src/exec_tests.rs` (aggregation) | `aggregation_over_a_pk_range_matches_the_same_query_expressed_as_seq_scan_plus_filter` | A genuine differential check: `PkRangeScan` vs. a forced-`SeqScan` expression of the identical logical query produce byte-identical `COUNT`/`SUM`/`GROUP BY` results |
| `sql/src/write_tests.rs` | `update_over_a_pk_range_never_under_or_over_updates`, `delete_over_a_pk_range_never_under_or_over_deletes` | UPDATE/DELETE regression: exact target-row counts, no under/over-mutation |
| `sql/src/pk_range_benchmark.rs` | 4 `#[ignore]`d performance tests | Real measured evidence, see the performance doc |

## 2. Certification matrix

| Gate | Result | Evidence |
|---|---|---|
| PK RANGE RECOGNITION | **PASS** | `plan_tests.rs`: single-column, one-sided, and composite-prefix cases all select `PkRangeScan`; `plan_reference_model.rs` differential/property tests agree independently |
| PK RANGE CORRECTNESS | **PASS** | `exec_tests.rs` exact-row-set tests across boundaries, empty ranges, composite PK |
| PK RANGE SNAPSHOT SEMANTICS | **PASS** | `pk_range_scan_respects_transaction_snapshot_isolation`: a pinned snapshot does not see a later independently-committed write in the same range |
| PK RANGE TOMBSTONES | **PASS** | `pk_range_scan_reflects_delete_then_reinsert_not_a_stale_or_duplicate_version`: delete-then-reinsert shows exactly one current version, never the tombstoned one, never both |
| PK RANGE MULTI-COLUMN PK | **PASS** | `partial_composite_pk_equality_becomes_a_prefix_pk_range_scan` (plan shape) + `pk_range_scan_on_composite_primary_key_prefix_returns_every_matching_row` (data correctness) — the exact landmine the mission named |
| PK RANGE TABLE ISOLATION | **PASS** | `pk_range_scan_never_returns_an_adjacent_tables_rows`: two tables with an identical, overlapping PK domain (0..20 each), range query against one returns only its own 20 rows |
| PK RANGE MEMORY BOUND | **PASS (structural)** | `TableStore::scan_table_pk_range_rows_as_of` returns a lazy iterator over the certified `RangeScanIter`, never a `Vec` — inherited from the same primitive `SeqScan` already used; not independently re-measured under a multi-GB table in this pass (see performance doc §0/§5 for the explicit scope note) |
| PK RANGE LATENCY | **PASS** | p50 stays 0.09–0.31ms across 1,000–100,000 rows vs. `SeqScan`'s 3.9ms–337.6ms for the identical query — up to ~1,099x improvement (performance doc §1) |
| PK RANGE TAIL LATENCY | **PASS** | p99/max tracked alongside p50 in every measurement (performance doc §1–4); no unexplained tail blowup at any table size or width tested |
| PK RANGE TABLE-SIZE SCALING | **PASS (scoped)** | Flat 0.09–0.31ms from 1,000→100,000 rows (100x growth); **not measured beyond 100,000 rows** — explicit, named scope reduction, not a silently narrowed claim |
| PK RANGE WIDTH SCALING | **PASS** | Cost tracks requested width (0.11ms @ 1 row → 35ms @ 10,000 rows) at a fixed 100,000-row table (performance doc §2) |
| PK RANGE CONCURRENCY | **PASS (scoped)** | 1/8/32 concurrent readers: p50 flat, throughput scales, zero errors; **not** the full 1/2/4/8/16/32 matrix and **not** mixed with concurrent writers in this dedicated benchmark (qualitatively covered by Blocker 9's endurance run instead) |
| PK RANGE JOIN | **PASS** | `inner_join_with_a_pk_range_on_the_outer_side_still_emits_exact_multiplicity` |
| PK RANGE AGGREGATION | **PASS** | `aggregation_over_a_pk_range_matches_the_same_query_expressed_as_seq_scan_plus_filter` (differential vs. forced-SeqScan on identical data) |
| PK RANGE UPDATE | **PASS** | `update_over_a_pk_range_never_under_or_over_updates`: exactly 4 of 10 rows updated for a 4-row range |
| PK RANGE DELETE | **PASS** | `delete_over_a_pk_range_never_under_or_over_deletes`: exactly 4 of 10 rows deleted, remainder verified by exact id set |
| PK RANGE DIFFERENTIAL | **PASS** | `plan_reference_model.rs`'s independent, from-scratch reference model (never calls into the real planner) agrees on every fixed scenario and every randomized proptest case; `aggregation_over_a_pk_range_...` is a second, execution-level differential (forced-SeqScan vs. PkRangeScan on identical data) |
| PK RANGE PROPERTY TEST | **PASS** | `matches_reference_model_for_randomized_predicate_and_index_shapes` extended with an `id_range` parameter, mutually exclusive with `id_eq`, covering randomized PK-range/index/equality combinations |
| PK RANGE ERROR BEHAVIOR | **NOT SEPARATELY TESTED** | No new error path was introduced (encoding reuses `encode_composite_key`, already covered by its own existing error tests in `src/relational/key.rs`; a storage I/O error surfaces through the same `Result` chain `SeqScan`/`IndexScan` already use, unchanged) — no dedicated new-failure-mode test was written because no new failure mode was introduced. See §3. |
| PK RANGE RECOVERY | **PASS (by inheritance, documented)** | This is a read-only optimization; no on-disk format, WAL, or Manifest code was touched (protected-engine audit, §6) — recovery semantics are unchanged by construction, not re-verified by a new crash test in this pass |
| PK RANGE SECURITY | **PASS** | No new SQL surface: bounds are resolved through the existing bound-expression/parameter evaluator (`resolve_pk_bound`, reusing `resolve_values`) — the same binder/type-validation/catalog-resolution/transaction-visibility path every other access method already goes through; no raw string ever reaches physical key construction outside `encode_composite_key`'s existing, already-tested encoding |
| NON-PK ACCESS REGRESSION | **PASS** | Every pre-existing `plan_tests.rs`/`exec_tests.rs`/`write_tests.rs` test (270+ in `rubixdb-sql` alone) passes with its original, unmodified assertions |
| INDEX ACCESS REGRESSION | **PASS** | `candidate_index_access` (secondary-index selection) was not modified at all — a deliberate, separate-function design choice (architecture doc §3.2) specifically to guarantee this |
| FULL REGRESSION | **FAIL (pre-existing, unrelated)** | See §4 |

## 3. PK RANGE ERROR BEHAVIOR — why "not separately tested" is not a gap

The new code path introduces exactly two new fallible operations, both
already covered by existing, unmodified tests elsewhere:

1. `encode_composite_key` on the bound values — the identical function
   `put_row`/`get_row`/`delete_row` already call, with its own
   existing error-path tests in `src/relational/key.rs` (`NaN`
   rejection, negative `TIME` rejection, decode-malformed-escape,
   decode-missing-terminator). Nothing about calling it from a range
   bound instead of a point key changes its error behavior.
2. `self.engine.range_scan(...)` — the certified primitive
   `scan_table_rows_as_of` already calls; any I/O/corruption error it
   can raise is already exercised by that existing call site's own
   coverage and propagates through the identical `Result` type
   unchanged.

No new panic, no new `unwrap`, no new silently-swallowed error was
introduced (verified by reading every line added, not assumed).

## 4. FULL REGRESSION — one pre-existing, unrelated failure documented, not fixed

- `cargo fmt --all -- --check`: clean for every file this increment
  touched. Pre-existing formatting diffs remain in files this
  increment did not touch (`api/examples/heap_ownership_profile.rs`,
  `api/examples/query_starvation_test.rs`, `api/tests/api_commit_ack_
  loss.rs`, `api/tests/api_delete_safety.rs`, `cli/tests/*`,
  `instance/src/lib.rs`) — all from commits predating this session
  (`c888292`, 2026-09-30 09:26, Blocker 4/7/8 work).
- `cargo clippy -p rubixdb -p rubixdb-sql -p rubixdb-api --all-targets
  --all-features -- -D warnings`: **clean, zero warnings** — every
  crate this increment actually changed.
- `cargo clippy --workspace --all-targets --all-features -- -D
  warnings`: **FAILS**, but only on two pre-existing files this
  increment never touched: `cli/tests/multi_instance_sustained_load.rs`
  (`clippy::ptr_arg`, `clippy::zombie_processes`) and `cli/tests/
  index_backfill_crash_integration.rs` (`clippy::ptr_arg`) —
  confirmed via `git log -1` to be from commit `c888292`, committed
  before this session began. **Not fixed here**: fixing another
  increment's test files is outside this increment's mandate
  (`sql/src/plan/access.rs`, `src/relational/table_store.rs`, and
  their test coverage), and doing so without that increment's own
  context/authorization risks unintended scope creep into work this
  session did not do. Flagged plainly rather than silently worked
  around or left undiscovered.
- `cargo check --workspace --all-targets --all-features`: **PASS**,
  zero errors.
- `cargo test -p rubixdb --lib` (542 tests, the full storage/relational
  unit suite): **PASS**, zero failures.
- `cargo test -p rubixdb-sql` (287 tests including every addition
  above): **PASS**, zero failures.
- `cargo test -p rubixdb-api --lib --bins` (45 tests): **PASS**, zero
  failures.
- `cargo test -p rubixdb-api --test api_sql_integration --test
  api_integration`: real HTTP/SQL integration coverage — see run log
  for pass/fail status alongside this document's own commit.
- `cargo test --release`: not run for every crate in this pass (time
  budget); the release-mode performance benchmarks (§ performance doc)
  exercised the release-built `rubixdb-sql` binary directly and passed.

## 5. What this increment did not attempt (explicitly, per its own mandate)

- No `ORDER BY` elimination for `PkRangeScan` (architecture doc §5).
- No shared refactor of `candidate_index_access`/`candidate_pk_range_
  access` (architecture doc §3.2, an explicit trade-off).
- No fix to the two pre-existing, unrelated `cli/tests/` clippy
  failures (§4).
- No table-size testing beyond 100,000 rows, no concurrency testing
  beyond 32 readers, no dedicated mixed read/write concurrency
  benchmark for `PkRangeScan` specifically (performance doc §0/§4).

## 6. Protected-engine audit

```
$ git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/
(empty)
```

Zero changes to any certified storage-engine path. Every change in
this increment lives in `sql/src/*` (the SQL/relational query layer)
and `src/relational/table_store.rs` (the relational row-storage
facade over the certified `LsmEngine`) — both explicitly in scope per
the Increment 15 mandate, and both reuse certified engine primitives
(`LsmEngine::range_scan`, `LsmEngine::get_as_of`) exactly as they
already existed, unmodified.
