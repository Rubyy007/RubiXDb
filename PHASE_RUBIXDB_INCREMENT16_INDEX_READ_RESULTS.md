# Increment 16: Secondary-Index Read — Results & Certification

Companion to `PHASE_RUBIXDB_INCREMENT16_INDEX_READ_ARCHITECTURE.md` and
`PHASE_RUBIXDB_INCREMENT16_INDEX_READ_PERFORMANCE.md`. Append-only
closure of the Blocker 9 `indexed_select` finding. The Increment 14
Blocker 9 record and the Increment 15 PK-range certification are
unchanged historical evidence.

## 0. One-paragraph outcome

The Blocker 9 `indexed_select` drift was **not** the secondary index and
**not** the certified engine. It was the number of matching rows
multiplied by a per-row cost that the relational layer inflated by
re-resolving table/column catalog metadata (an engine point read plus an
engine range scan) for **every** matched row, a cost that itself grew
with the number of SSTables. Resolving metadata once per scan removes it:
3–4× faster in the production-representative regime (auto-Compaction on),
7–29× with SSTables accumulated, and the cost is now linear in matched
rows (~15µs/row) and flat in table size from 100K to 1M rows. No
certified-engine file was touched; no engine-performance ADR was needed.
Along the way the randomized differential tests found **two pre-existing
correctness defects**: F-1 (wrong results — fixed here) and F-2 (open).

## 1. Findings (flagged, not silently resolved)

### F-1 — FIXED: upper-bound-only index range returned NULL-valued rows
- **What:** `WHERE a <= 3` / `a < 3` (and `a = p AND b <= x` on a
  composite index) used an index range whose physical start is the
  column's NULL entries (NULL sorts first in the key encoding) and the
  comparison was "consumed", so rows with `a IS NULL` were returned.
  Wrong result, not a performance issue.
- **Reproduced on unmodified `HEAD`** (with my read-path edits reverted):
  `[1, 2, 4, 5]` returned vs correct `[2, 4, 5]`.
- **Fix:** `sql/src/plan/access.rs::candidate_index_access` keeps the
  upper-bound conjunct as a residual filter when no lower bound exists
  for that column; it still counts toward access-path selection.
- **Test:** `upper_bound_only_index_range_excludes_null_indexed_values`.
- Not found by earlier increments' tests because their fixtures had no
  NULL indexed values in one-sided ranges.

### F-2 — OPEN (pre-existing, not fixed): snapshot older than an index rebuild misses rows
- A transaction that began before an index was dropped and re-created
  (or created), and then reads through that index, sees an empty index
  (the backfill entries have post-snapshot sequence numbers) while the
  planner still selects it → missing rows.
- Reproduced: `known_gap_snapshot_started_before_index_rebuild_misses_rows`
  (`#[ignore]`d so the suite does not codify the bug). The differential
  harness retires snapshots across `DROP INDEX`/`CREATE INDEX` rather
  than assert it.
- Needs a design decision (record an index's ready-sequence and make the
  planner refuse the index for older snapshots, or fall back to a scan).
  Out of scope for a read-performance increment; recommended as its own
  increment.

### F-3 — pre-existing test defects / environment, unchanged
- `cli/tests/multi_instance_sustained_load.rs`
  `two_instances_simultaneous_sustained_read_and_write_load_never_cross_contaminate`:
  in the debug run it panics at 0.11s with tokio's "Cannot drop a runtime
  in a context where blocking is not allowed" (a `reqwest::blocking`
  client created inside an async test). **Identical panic with the file
  restored to `HEAD`**, so it predates this increment; it passed in the
  release run (timing-dependent). Not fixed (out of mandate).
- M1.2 / M1.3 WAL group-commit throughput targets (15,000 / 80,000
  ops/s) miss on this machine (debug: 423 and 3,301 ops/s; release:
  5,901 and 45,982 ops/s). Already documented as failing in
  `PROCESS.md` (2026-09-14). WAL is untouched.
- Three engine tests (`write_pool::…many_concurrent_submitters…`,
  `compaction_tests::auto_trigger_fires_at_and_above_threshold_never_below`,
  `…repeated_automatic_compaction_cycles…`) failed in one debug
  `cargo test --workspace` run and passed in isolation, in a rerun of
  `cargo test -p rubixdb --lib` (543/543), and in both later full runs.
  Timing/load-sensitive; no engine file changed.

## 2. New tests (all additive; no existing assertion was changed)

| File | Test | Proves |
|---|---|---|
| `sql/src/index_read_differential_tests.rs` | `index_reads_match_reference_model` (proptest, 24 cases × 160 steps) | randomized INSERT / UPDATE of indexed columns / DELETE / re-insert / UPDATE+DELETE through index predicates / `DROP`+`CREATE INDEX` mid-run against an independent `BTreeMap` model; 13 query shapes per checkpoint: index equality, index range (≥/<, >, ≤), PK equality, PK range, composite equality, mixed (index+residual), (PK range + index), unindexed, empty, `OR`; plus COUNT and JOIN over an index predicate; reads inside open snapshot transactions; exact `rows_affected` for every UPDATE/DELETE |
| same | `index_reads_match_reference_model_fixed_seeds_long` | 5 seeds × 600 steps with a 24KB MemTable and automatic Compaction: **37 Compaction cycles completed during the runs**; ends with the physical index-entry audit (decode every entry of `ia`, `ib`, composite `iab`; must equal exactly the model: **zero stale / orphan / duplicate entries**) |
| same | `index_reads_match_model_across_selectivities_and_result_sizes` | 3,000 rows, high (1 row) / medium (100) / low (1,000) selectivity, 10-row range, empty result; churn leaves tombstones; no duplicates |
| same | `index_scan_row_limit_is_enforced_while_collecting` | `max_index_scan_rows`: exactly-at-limit passes, one over fails closed with `ResourceLimit` |
| same | `upper_bound_only_index_range_excludes_null_indexed_values` | F-1 regression |
| same | `known_gap_snapshot_started_before_index_rebuild_misses_rows` (`#[ignore]`) | F-2 reproduction |
| `src/relational/index_tests.rs` | `bounded_index_scans_fail_closed_at_the_limit_and_count_work` | bounded == unbounded under the limit; fail-closed one below; new counters move by exactly the work done |
| `sql/src/index_read_benchmark.rs` | 6 `#[ignore]`d measurement tests | all evidence in the performance doc |
| `api/src/routes/sql.rs` | `serialization_benchmark::…` (`#[ignore]`d) | serialization stage cost |

Composite indexes **are** supported by the current implementation
(`IndexRow.column_ordinals` is a list; leading-prefix equality plus a
range on the next column) and are covered (`iab(a, b)`). Existing
transaction, snapshot, backfill-crash, compaction and index tests
(`src/relational/{index_tests,txn_tests,tests}.rs`, 543 tests in the
engine/relational crate) pass unchanged; the engine's
`index_survives_automatic_compaction…` and
`index_lookup_as_of_is_stable_against_a_later_write` cover the
Compaction/snapshot interaction independently of the new tests.

## 3. Certification matrix

| Gate | Result | Evidence |
|---|---|---|
| SECONDARY INDEX ROOT CAUSE | **PASS** | Per-stage decomposition (performance §1): per-row catalog metadata resolution = 81% of 1,238ms at K=10,000/100K rows; engine point read 8.7%; traversal+decode 0.5%; front-end 0.03%. Not engine, not index traversal |
| INDEX LOOKUP PERFORMANCE | **PASS** | ~0.6µs per index entry, unchanged by the fix, independent of N |
| ROW FETCH PERFORMANCE | **PASS (scoped)** | 7–29× faster with SSTables accumulated, 3–4× in the production-representative regime; residual ≈ 14.5µs/row is the certified engine's own point-read cost (raw get 10.8µs) — not reducible above the engine without caching/covering, both measured and rejected |
| MATCHED-ROW SCALING | **PASS** | Linear in K both before and after; per-row cost 56µs → 14.5µs (compaction on) |
| TABLE-SIZE SCALING | **PASS (scoped)** | With compaction on: K=10: 0.227ms (100K) → 0.291ms (1M); K=1,000: 15.3 → 18.2ms. Not measured beyond 1M; 1M only for K=10/1,000; the 10K→100K step is engine MemTable-vs-SSTable read cost (same step visible in PK equality) |
| INDEX RANGE PERFORMANCE | **PASS** | K=100: 10.9 → 1.55ms; K=1,000 at 1M rows: 14.7ms |
| INDEX EQUALITY PERFORMANCE | **PASS** | performance §2; replay at 206K rows: 1,199 → 327ms |
| HIGH-SELECTIVITY | **PASS** | K=1: 0.167 → 0.096ms full SQL; K=10: 0.656 → 0.213ms (compaction on) |
| LOW-SELECTIVITY | **PASS (with named limitation)** | K=10,000 (10% of the table): 556 → 145ms (3.8×) and beats a full SeqScan (~340ms). The planner has no cost model; it always uses a sargable index, so at selectivities above roughly 25% a SeqScan would win. Not addressed here (flagged) |
| CONCURRENCY | **PASS** | 1/2/4/8/16/32 threads, K=100, K=1,000 and PK control: throughput ceiling 2.4× higher, p99 down 2.1–2.3×, PK unchanged |
| TAIL LATENCY | **PASS** | p95/p99/max recorded everywhere; warm single-thread p99 ≤ 1.1× p50 at K ≥ 100; concurrent p99 roughly halved |
| MEMORY IMPACT | **PASS** | no cache/prefetch/covering added; RSS 14–27MB at K = 10,500–20,600 rows, unchanged; scan memory now capped while collecting |
| DISK IMPACT | **PASS** | none (no format or index change; covering index rejected at +5.3× index size) |
| WRITE IMPACT | **PASS** | write path untouched; UPDATE/DELETE via index 1.2–1.6× faster; per-index write cost measured (+≈30ms per index per 20K rows) |
| SNAPSHOT CORRECTNESS | **PASS (scoped; F-2 open)** | snapshot reads inside open transactions verified against the model throughout the randomized runs, across flushes and 37 Compactions; **a snapshot older than an index (re)build is a known pre-existing failure (F-2)** |
| TRANSACTION CORRECTNESS | **PASS** | existing `txn_tests.rs` unchanged and passing; transactional reads in the differential runs |
| INDEX MAINTENANCE | **PASS** | physical entry audit equals the model for `ia`, `ib`, composite `iab`: zero stale/orphan/duplicate entries after hundreds of mutations |
| UPDATE INDEXED COLUMN | **PASS** | randomized `UPDATE … SET a=…, b=…` with exact counts, entry audit |
| DELETE INDEXED ROW | **PASS** | randomized deletes by PK and by index predicate with exact counts, entry audit |
| COMPACTION INTERACTION | **PASS** | 37 automatic Compaction cycles completed while index reads (incl. snapshot reads) ran; zero wrong/missing/duplicate rows |
| JOIN REGRESSION | **PASS** | JOIN over an index predicate: correct vs model; 22.5 → 13.0ms |
| AGGREGATION REGRESSION | **PASS** | COUNT/SUM/GROUP BY/HAVING over an index predicate: correct vs model; 108 → 15.4ms |
| UPDATE REGRESSION | **PASS** | exact target counts; 35.0 → 26.2ms |
| DELETE REGRESSION | **PASS** | exact target counts; 7.6 → 6.4ms (K=10), 272 → 167ms (K=1,000) |
| DIFFERENTIAL TESTING | **PASS** | independent brute-force model; 24 proptest cases + 5×600-step fixed seeds; found F-1 and F-2 |
| PROPERTY TESTING | **PASS** | `proptest` over seeds as above |
| SECURITY | **PASS** | no new SQL surface; bounds still flow through the binder/planner/`resolve_bound` and `encode_indexed_columns`; no raw index key built from unchecked input; new counters are label-free and carry no SQL text/values/row content; F-1 removes a wrong-result path |
| RESOURCE BOUNDS | **PASS (improved)** | `max_index_scan_rows` now enforced while collecting (boundary-tested); eager `Vec` still the materialization strategy (candidate F deferred) |
| FULL REGRESSION | **FAIL (pre-existing/unrelated; see §4)** | everything this increment touches passes; remaining failures are the documented M1.2/M1.3 WAL throughput targets and a pre-existing CLI test defect (F-3) |

Recovery/crash: the change is read-only and touches no WAL, Manifest,
SSTable, or Compaction code and no durability path, so recovery
semantics are **inherited unchanged** (no new crash test was warranted
or run; the existing crash-recovery and index-backfill-crash suites pass
unchanged).

## 4. Full-regression record

- `cargo fmt --all -- --check`: **clean**. Note: running `cargo fmt
  --all` also reformatted nine pre-existing test/example files from
  earlier commits (formatting only, committed separately as housekeeping).
- `cargo clippy --workspace --all-targets --all-features -- -D
  warnings`: **clean**. The two previously-known failures were cleaned up
  as housekeeping, test semantics unchanged:
  `cli/tests/index_backfill_crash_integration.rs` and
  `cli/tests/multi_instance_sustained_load.rs` (`&PathBuf` → `&Path`
  parameters; a targeted `#[allow(clippy::zombie_processes)]` with a
  comment because the spawned child is returned in `Instance` and
  `kill()`ed/`wait()`ed by the caller).
- `cargo check --workspace --all-targets --all-features`: clean.
- `cargo test --workspace --no-fail-fast` (debug): every test binary
  passes except M1.2, M1.3 (debug-mode throughput targets) and the CLI
  two-instance test (F-3). First debug run additionally showed the three
  timing-flaky engine tests (passed on rerun).
- `cargo test --release --workspace --no-fail-fast`: every test binary
  passes except M1.2 and M1.3 (known; release 5,901 and 45,982 ops/s).
  Engine/relational crate 543/543, SQL crate 289 passed / 11 ignored,
  API 45 passed.
- Protected-path audit: see §5.

## 5. Protected-path audit

```
git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/   →  (empty)
```
Also untouched: `src/lsm/mod.rs`, `src/memtable/`, `src/execution/`.
(`src/lsm/tests.rs` received `rustfmt` line-wrapping only.)

## 6. Files changed

Product: `src/relational/index.rs`, `src/relational/table_store.rs`
(visibility of one function), `sql/src/exec/operators.rs`,
`sql/src/plan/access.rs`. Tests/benchmarks: `sql/src/test_support.rs`
(`Fixture::new_with_lsm`), `sql/src/lib.rs`,
`sql/src/index_read_differential_tests.rs`,
`sql/src/index_read_benchmark.rs`, `src/relational/index_tests.rs`,
`api/src/routes/sql.rs` (test module). Housekeeping: nine files
reformatted by rustfmt; two CLI test files clippy-cleaned.

## 7. What is NOT claimed

- Not claimed: "secondary indexes are production-ready." Claimed:
  the measured scaling behaviour of the indexed-read path is understood,
  the dominant avoidable cost is removed, and the remaining cost is the
  certified engine's per-row point read.
- Open: F-2 (snapshot across index rebuild), no cost-based
  index-vs-scan choice, eager `Vec` materialization, index stats not
  exported on the HTTP metrics route, no process-cold/cache-dropped
  runs, no >1M-row runs, no mixed read+write concurrency benchmark.
- Deliberately not started (per mandate): Router, Replication,
  Partitioning, advanced SQL.
