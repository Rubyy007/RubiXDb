# Increment 17: Results & Certification

Companion to `PHASE_RUBIXDB_INCREMENT17_INDEX_SNAPSHOT_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT17_COST_MODEL_ARCHITECTURE.md` and
`PHASE_RUBIXDB_INCREMENT17_PERFORMANCE.md`. Append-only closure of the two
open items Increment 16 recorded (F-2 and the missing cost model). The
Increment 14, 15 and 16 records are unchanged; in particular Increment 16's
text still says F-2 was "open" — that was true then, and this document
supersedes it.

## 0. Outcome

- **F-2 is fixed.** Before changing anything it was reproduced on HEAD in
  five deterministic scenarios; a secondary index may now serve a read only
  if it was `Ready` *as of the reader's snapshot* (derived from the
  catalog row's own MVCC version, so no new persisted metadata exists to
  lose), and otherwise the executor runs the identical table scan —
  including re-establishing `ORDER BY` order where the planner had dropped
  the `Sort`. A mutation test (validity check ignoring the snapshot) fails
  **all 10** F-2 tests; the real code passes them, plus 76 randomized
  index rebuilds with open snapshots verified against an independent model.
- **Index-vs-scan selection is now cost-based**, decided at execution from
  the exact match count, a rigorously drift-bounded table-size estimate and
  self-calibrating per-row costs — no percentage constant. Across 57
  measured points in four regimes (10K–1M rows, memory-resident to 12
  SSTables) the model picked the measured-faster path at 55, and at the
  other two the paths were within 1% of each other (regret ≤ 1.01×). It
  removes up to 3.7× (100K rows, 90% selectivity) and 2.7× (1M rows, 50%) of
  needless index cost, at zero measurable overhead for selective queries.
- No certified-engine file was changed (audit in §5); no engine ADR was
  needed.
- A claim in my own first draft of the F-2 architecture document was
  corrected during the work: F-2 produces only *missing* rows (backfilled
  entries are invisible to an older snapshot, and entries written during a
  build always agree with the row version they describe), not extra ones.
  The tests still assert all three directions (no missing, extra, or
  duplicate rows).

## 1. Findings recorded

| Id | Status | Detail |
|---|---|---|
| F-2 | **FIXED** | snapshot older than an index (re)build / promotion read through that index and missed rows |
| (new) planner/executor cost-blindness | **FIXED** | always-index at high selectivity (Increment 16 measurement) |
| (new) read-your-own-writes on scans | *noted, pre-existing, unchanged* | a scan inside a transaction does not overlay that transaction's own uncommitted writes (PK lookup does; verified by a probe). Applies to every scan access path |
| (new) PK range vs index not cost-compared | *noted, not changed* | when both a PK range and a secondary index are sargable the planner keeps its old "most consumed conjuncts" rule |
| (new) contended write hook | *measured, accepted* | `note_mutations` ≈ 21–26ns uncontended, ≈ 850ns/call with 8 contending threads (shared read lock); ≈ 0.02% of a durable write |
| Eager index materialization | *deferred, measured* | +13% over the result itself at 25,000 matches; not a meaningful risk at tested sizes; a separate future item |
| `max_index_scan_rows` | *unchanged* | an index scan with more than that many matches still fails closed with `ResourceLimit` rather than silently falling back |

## 2. New and changed tests

| File | Tests | What they prove |
|---|---|---|
| `sql/src/index_snapshot_tests.rs` (new) | 8 | the five F-2 reproductions (CREATE INDEX with old snapshot; DROP→CREATE with old snapshot; UPDATE between snapshot and rebuild; BEGIN during the build; INSERT/UPDATE/DELETE interleaved with three snapshots), the fallback counter fires exactly when the snapshot predates the index, ordered-scan fallback preserves order (Sort asserted absent from the plan), JOIN / aggregation / `DELETE` through the index inside an old-snapshot transaction |
| `sql/src/index_read_differential_tests.rs` | +2; 1 un-ignored; harness changed | the Increment 16 `#[ignore]`d F-2 test is now a permanent regression (`snapshot_started_before_index_rebuild_sees_all_rows_f2_regression`); the workaround that retired snapshots across `DROP`/`CREATE INDEX` is **deleted**; new churn property (6 seeds, 76 rebuilds, 37 Compactions, open snapshots checked against the model); **every step now runs under a randomly chosen access path** (`Auto`/`ForceIndex`/`ForceSeq`) |
| `sql/src/cost_model_tests.rs` (new) | 9 incl. proptest | all query shapes × all three modes × **poisoned statistics** vs the independent model (12 fixed seeds + 16 proptest cases; tables of 0–700 rows; INNER and LEFT JOIN with a correlated inner index, COUNT); decisions (selective keeps the index, unselective scans, no-estimate small results never pay for statistics, forced modes, PK paths untouched, drifted estimates resolve toward the index); **drift bound property** (400 random mutations, bound checked after every step); registry bounded |
| `sql/src/exec/cost.rs` | 6 unit | break-even derived from the cost ratio (no constant), drift widens toward the index, no zero/overflow, reliability rule, prediction = break-even |
| `src/relational/stats.rs` | 3 unit (+1 ignored micro-bench) | estimate/drift/reset, ≤ 4,096 tracked tables, clamped cost estimates ignoring tiny samples |
| `src/relational/index_tests.rs` | +1 | `index_ready_sequence_survives_restart`: real engine restart; the pre-promotion version still reads `Building`, the promotion boundary and a `DROP`/`CREATE` cycle survive |
| benchmarks (`#[ignore]`) | `selectivity_crossover_sweep`, `memory_by_result_size`, `count_rows_cost`, `inc17_overhead_*` | all evidence in the performance document |

Test counts after this increment: engine/relational crate 547 passed; SQL
crate 314 passed (15 ignored measurement tests); API 45; every other binary
unchanged.

## 3. Certification matrix

| Gate | Result | Evidence |
|---|---|---|
| F-2 REPRODUCTION | **PASS** | 5 deterministic scenarios fail on `730ecca` before any change (architecture doc §1) |
| F-2 ROOT CAUSE | **PASS** | exact timeline; backfilled entries are retroactive derived data stamped with a later sequence; the readiness sequence is the engine sequence of the `Building→Ready` catalog write (architecture doc §2) |
| OLD SNAPSHOT + INDEX | **PASS** | `create_index_with_old_snapshot`, `fallback_is_taken_exactly_when_…`, mutation test |
| INDEX REBUILD SNAPSHOT | **PASS** | BEGIN-during-build and three-snapshot scenarios; churn property (76 rebuilds) |
| DROP/CREATE SNAPSHOT | **PASS** | `drop_then_create_index_with_old_snapshot`; restart test for the cycle |
| INDEX SNAPSHOT CORRECTNESS | **PASS** | zero missing / extra / duplicate rows vs the independent model in all F-2 scenarios and randomized runs, also under every access-path mode |
| INDEX READY METADATA | **PASS** | no new metadata: the readiness sequence is the catalog row's MVCC version (`CatalogService::get_index_as_of`); no schema change, no migration |
| INDEX READY RECOVERY | **PASS (scoped)** | `index_ready_sequence_survives_restart` over a real engine restart; existing process-level crash suites pass unchanged (`index_backfill_crash_integration`, `crash_recovery_integration`, `crash_consistency`, `pathological_recovery_matrix`, `api_commit_ack_loss`). Because no new persisted state exists, no *new* process-level crash test was warranted |
| COST MODEL CORRECTNESS | **PASS** | 55 / 57 measured points predicted the faster path, the other 2 were dead-even (regret ≤ 1.01×); results identical to the model under all modes and poisoned statistics |
| COST MODEL PLANNER OVERHEAD | **PASS** | planner parse/bind/plan unchanged within ±1µs (8–23µs); the decision runs at execution at the cost of one map probe and two atomic loads |
| SEQSCAN SELECTION | **PASS** | chosen at K > K\*; 2.0–3.7× faster than the index at 50–90% selectivity (100K), 2.7× at 1M |
| INDEX SELECTION | **PASS** | kept for every selective point (≤ 10% at 100K/1M; ≤ 25% where cheaper); identical latency to `ForceIndex` |
| PK EQUALITY REGRESSION | **PASS** | 0.047 → 0.046ms; plan/metric test `pk_paths_are_unaffected_by_the_cost_model` |
| PK RANGE REGRESSION | **PASS** | 0.373 → 0.377ms; still `PkRangeScan` (metric asserted) |
| INDEX RANGE REGRESSION | **PASS** | K=100: 1.391 → 1.383ms |
| JOIN REGRESSION | **PASS** | INNER and LEFT JOIN with a correlated per-outer-row inner index correct under every mode and poisoned statistics; timing 6.53 → 6.64ms (noise); the decision is per outer row with that row's exact K |
| AGGREGATION REGRESSION | **PASS** | COUNT, SUM, GROUP BY/HAVING correct; 14.13 → 14.72 / 14.30 → 14.27ms. (AVG/MIN/MAX share the same operator and were not separately parameterized.) |
| UPDATE REGRESSION | **PASS** | exact target counts in the randomized runs under every mode; 13.29 → 13.02ms |
| DELETE REGRESSION | **PASS** | exact target counts likewise; interleaved A/B 108.2 → 107.7ms (a back-to-back batch appeared 6% slower; bisected to machine drift, see performance §4) |
| HIGH SELECTIVITY | **PASS** | K=10 at 100K: 0.284ms index, `Auto` 0.276ms vs 330ms scan |
| LOW SELECTIVITY | **PASS** | 90%: `Auto` 421ms vs index 1,524ms |
| SELECTIVITY CROSSOVER | **PASS (scoped)** | measured in 4 regimes (10K memory-resident, 100K compaction on/off, 1M): 17%–25%, differing per regime; not a constant. 10K, 100K and 1M only; not beyond |
| STATISTICS ACCURACY | **PASS (scoped)** | the table-size estimate carries a rigorous bound `\|true−rows\| ≤ drift`, property-tested; the match count K is **exact** (so no selectivity estimation error exists to measure). No distinct-count or histogram statistics exist, by design |
| STATISTICS STALENESS SAFETY | **PASS** | poisoned statistics (size 0, 1, 2⁶³, random; inverted costs) change only the path, never a result; stale estimates are refreshed by an exact count before a scan is chosen |
| WRITE OVERHEAD | **PASS** | hook 21–26ns uncontended (≈ 850ns with 8 contending threads) vs ≈ 4,150µs per durable write; end-to-end +0.8…1.9% inside the ±1–2% inter-repetition spread; CREATE INDEX cost zero |
| MEMORY | **PASS** | statistics ≤ ~400KB at the cap; index-path RSS +13% over the streaming scan at 25,000 matches; eager `Vec` recorded as a deferred item |
| CONCURRENCY | **PASS** | 1/2/4/8/16/32 threads, K=100, K=1,000 and PK control: identical before/after within noise |
| COMPACTION INTERACTION | **PASS** | 37 automatic Compactions overlapped the snapshot/rebuild/path-independence suites; sweeps run with Compaction on and off |
| CRASH RECOVERY | **PASS (scoped)** | see INDEX READY RECOVERY |
| DIFFERENTIAL TESTING | **PASS** | independent `BTreeMap` model extended to old snapshots, rebuilds, fallbacks and per-step random access paths |
| PROPERTY TESTING | **PASS** | proptest over sizes (0–700), selectivities, NULLs, composite indexes, snapshot positions, rebuild states, statistics poison, plus the drift-bound property |
| SECURITY | **PASS** | no new SQL surface; `access_path` is server configuration (`ExecLimits`), unreachable from SQL; bounds still pass through the binder/planner/`resolve_bound`/`encode_indexed_columns`; no raw keys from unchecked input; new counters are label-free (`index_snapshot_fallbacks`, `index_cost_fallbacks`, plus Increment 16's); statistics hold no SQL text, values, or row content |
| RESOURCE LIMITS | **PASS** | statistics bounded (4,096 tables); fallback sort bounded by `max_materialized_rows`; `max_index_scan_rows` still fails closed (boundary test unchanged, now run under `ForceIndex`) |
| FULL REGRESSION | **FAIL (pre-existing, unrelated)** | see §4 |

## 4. Full-regression record

- `cargo fmt --all -- --check`: clean. `cargo clippy --workspace
  --all-targets --all-features -- -D warnings`: clean. `cargo check
  --workspace --all-targets --all-features`: clean.
- `cargo test --workspace --no-fail-fast` (debug): 1,089 passed, 3 failed.
- `cargo test --release --workspace --no-fail-fast`: 1,090 passed, 2 failed.
- The failures are the same known items as Increment 16, **re-verified on the
  Increment 16 tree in a clean worktree**, so they are not caused by this
  increment:
  - `m1_2_hundred_writers_throughput` / `m1_3_thousand_writers_throughput`
    (WAL group-commit throughput targets of 15,000 / 80,000 ops/s): fail on
    both trees. Increment 16 tree, release: 8,304 and 59,055 ops/s; this
    tree, release: 6,327 and 47,261 ops/s (debug: 609 and 4,179). The spread
    between runs of the *same* tree on this machine is as large as that
    difference (an earlier Increment 16 release run measured 5,901 and
    45,982). Documented as failing in `PROCESS.md` since 2026-09-14. WAL is
    untouched.
  - `two_instances_simultaneous_sustained_read_and_write_load_never_cross_contaminate`
    (debug only): tokio "cannot drop a runtime in a context where blocking is
    not allowed"; fails identically with the file at Increment 16's HEAD and
    passes in release. It leaves an orphaned `rubixdb gui` child process when
    it panics; the orphan was killed. Not fixed (out of mandate).
- The engine tests that flaked once during Increment 16 passed in all runs
  this time.

## 5. Protected-path audit

```
git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/   ->  (empty)
```

## 6. Files changed

Product: `src/catalog/service.rs` (`get_index_as_of`), `src/relational/index.rs`
(snapshot validity, two-phase scan, backfill observation),
`src/relational/table_store.rs` (statistics owner, `count_rows`, write hooks),
`src/relational/txn.rs` (commit hook), `src/relational/stats.rs` (new),
`src/relational/mod.rs`; `sql/src/plan/access.rs`, `physical.rs`, `explain.rs`
(`IndexFallback`), `sql/src/exec/operators.rs` (validity fallback, decision
loop, scan observation), `sql/src/exec/cost.rs` (new), `sql/src/exec/mod.rs`
(`ExecLimits::access_path`, counters). Tests/benchmarks as in §2.

## 7. Claims, deliberately limited

- **Secondary indexes:** the F-2 blocker named by Increment 16 is fixed and
  the affected regression passes. I do not make a blanket "production-ready"
  claim: the limits below remain, and the full workspace regression is not
  clean for the pre-existing reasons in §4.
- **Cost-based planner:** selection quality is demonstrated by measurement in
  the four regimes above (≤ 1M rows). It is not demonstrated beyond that,
  under mixed read/write load, or for a PK-range-vs-index choice.
- **FULL RUBIXDB** is not claimed production-ready by this increment.

Limits: process-cold runs not done; >1M rows not run; `ORDER BY`-dependent
scans never abandon the index (their high-selectivity cost was not
measured); the decision is made at execution so `EXPLAIN` shows the planned
access only; estimates are in-memory and re-learned after a restart (decisions
are conservative until then); a snapshot older than an index is served by a
table scan until it ends.

Stopped after Increment 17 as instructed: Router, Replication, Partitioning
and advanced SQL were not started.
