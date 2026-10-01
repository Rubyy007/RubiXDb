# Increment 18: Results & Certification

Companion to `PHASE_RUBIXDB_INCREMENT18_ACCESS_PATH_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT18_MATERIALIZATION_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT18_TRANSACTION_SCAN_SEMANTICS.md` and
`PHASE_RUBIXDB_INCREMENT18_PERFORMANCE.md`. Append-only closure of the four
open items Increment 17 recorded. Increment 14-17 records are unchanged; where
they describe a behaviour as a "scope boundary" or "deferred" that this
increment resolved, this document supersedes them.

## 0. Outcome per open item

| # | Open item | Investigated | Verdict |
|---|---|---|---|
| 1 | PK range vs secondary index not cost-compared | baseline grid: planner wrong by up to **838x** | **fixed**: candidates priced by exact row counts, raced in lockstep; worst regret 1.44x at 100K (1.68x at 1M) |
| 2 | eager index-result materialization | memory fine; **`LIMIT` cost 90% of the full query** | **fixed** (hybrid: enumerate entries, fetch rows lazily): `LIMIT 10` over 25,000 matches 373 -> 16.2ms; enumeration still eager (recorded) |
| 3 | scans ignore the transaction's own writes | contract (D10 + transaction ADR section 2) requires it; `UPDATE` after `INSERT` in one transaction silently hit **3 of 5** rows | **fixed**: bounded, versioned overlay merged into every scan; 5 of 6 reproductions failed before, all pass |
| 4 | runtime statistics relearned after restart | real restart experiment | **not required**: one exact count (0.6us/row, once per table), ~8% on one borderline query until parameters are re-learned; no persistence added |

Nothing in `src/wal/`, `src/manifest/`, `src/sstable/` or `src/compaction/`
changed (audit in section 5); no engine ADR was needed.

## 1. Findings

| Id | Status | Detail |
|---|---|---|
| Transaction scan semantics | **FIXED** | see the semantics document; this was a correctness defect in multi-statement transactions (DML skipped the transaction's own rows) |
| PK-range vs index regret | **FIXED** | 838x -> 1.44x worst case |
| Eager-materialization `LIMIT` | **FIXED** | 23x at K=25,000 |
| Race selection regression found and fixed during the work | *recorded* | the first unified selection introduced 65x regret (R=10, K=10,000) by probing the index with a table-scan-sized budget; replaced by the lockstep race (see access-path doc section 4.3) |
| Row-clone inefficiency | **FIXED, general speedup** | every fetched row was cloned twice into its context; `RowContext::with_row` makes seq scan -33% (342.6 -> 229.7ms) and PK range -14% |
| Read-path concurrency plateau | **OBSERVED, NOT INVESTIGATED** | fetch-heavy queries plateau at ~72-100 op/s from ~4 threads even with idle CPU (new mixed path uses 0.4x the CPU of the old one at the same throughput). Outside the four open items; would be an engine-profiling question. Recorded with data (performance doc section 5) so it is not lost |
| Fixed cost of a two-candidate decision | *accepted, documented* | ~0.1ms; regret > 1.5x only on queries < ~0.3ms (N=1K worst 7.6x on a 0.147 vs 0.019ms query) |
| Overlay cost O(w) per scan | *accepted, documented* | +0.7ms at w=1,000 on a 1.5ms query; <= 4% to w=100; 0 for tables not written |
| Index enumeration eager | *deferred, measured* | `LIMIT` is O(K) key work (~0.65us/entry) |
| Aggregation doc's "scans lack read-your-own-writes" | **superseded** | `PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.md` section 12 is unchanged history |

An apparent 25% seq-scan slowdown at w=10 and an apparent 6%-then-7% DELETE
gap in Increment 17 were both chased with probes rather than explained away;
this increment's w=10 anomaly was measurement noise (probe at w=7..12 showed
none), documented in the performance doc.

## 2. New and changed tests

| File | Tests | What they prove |
|---|---|---|
| `sql/src/txn_scan_tests.rs` (new) | 8 incl. proptest | six deterministic transaction-scan scenarios (insert / update / delete / insert-update-delete / DML finds own rows / other snapshots isolated) x 11 query shapes x 3 access-path modes vs an independent model (5 failed on HEAD); randomized property: 16 proptest cases x 90 steps + 6 seeds x 250 steps with exact DML target counts, outside writers, index rebuilds under the open snapshot and 7 overlapping Compactions |
| `sql/src/materialization_tests.rs` (new) | 6 | `LIMIT` fetches exactly the rows it returns; no read-ahead (one fetch per pull); cancel stops the fetch and the transaction stays correct and commits; expired deadline fetches nothing; entry buffer bounded by `max_index_scan_rows`; LIMIT composes with the transaction overlay |
| `sql/src/cost_model_tests.rs` | +3, 4 modes, +4 mixed shapes | path independence now across `Auto`/`ForceIndex`/`ForcePkRange`/`ForceSeq` with PK-range + index mixed predicates and poisoned statistics; PK range + selective index switches to the index (counter and rows examined asserted); narrow PK range stays; the more selective of two indexes is chosen vs the independent model; ordered scans never switch |
| `src/relational/stats.rs`, `sql/src/exec/cost.rs` | updated | `index_open_ns` parameter |
| benchmarks (`#[ignore]`) | `pk_range_vs_index_baseline`, `materialization_baseline`, `txn_overlay_overhead`, `restart_statistics_experiment`, `inc18_compare_*` | all evidence in the performance document |

No existing assertion was changed. **Mutation check:** making
`overlay_for` return `None` fails 8 of the 9 transaction tests (the ninth
asserts other transactions never see uncommitted writes and correctly still
passes).

Test counts: engine/relational crate 547 passed; SQL crate 331 passed (20
ignored measurement tests); API 45; every other binary unchanged. Workspace:
debug 1,106 passed / 3 failed; release 1,107 passed / 2 failed.

## 3. Certification matrix

| Gate | Result | Evidence |
|---|---|---|
| PK RANGE VS INDEX COST | **PASS (scoped)** | 16/16 decisions correct at N=100K; worst regret 1.44x (was 838x); N=1M <= 1.68x; N=10K 1.12-2.18x; N=1K up to 7.6x on sub-0.2ms queries (fixed ~0.1ms cost) |
| PK RANGE VS SEQSCAN COST | **PASS** | a PK range is bounded by the table; it is never priced against the scan and wins without counting once every index has lost to the scan |
| SECONDARY INDEX VS SEQSCAN COST | **PASS** | Increment 17 sweep re-run on this tree: 16/16 correct, crossover unchanged |
| UNIFIED ACCESS PATH | **PASS** | `PkRangeScan`, `IndexScan` (any number of indexes) and the table scan are chosen by exact cost; `ForcePkRange` mode and `access_path_switches` counter exist for verification |
| COST MODEL CORRECTNESS | **PASS** | results identical to the independent model in all four modes, with poisoned statistics; decision tests assert the switch |
| COST MODEL OVERHEAD | **PASS (scoped)** | planner unchanged except +6.7us (bounded) for a predicate with several carried candidates; execution of simple shapes unchanged or faster; ~0.1ms fixed cost when two candidates compete |
| COST MODEL CONCURRENCY | **PASS** | 1/2/4/8/16/32 threads: existing matrix identical within noise; mixed predicate 3.0x throughput and 0.32x CPU at 1 thread, converging to the (unexplained, pre-existing) read-path plateau at 8+ |
| SELECTIVITY CROSSOVER | **PASS** | unchanged and re-validated (16/16) |
| INDEX MATERIALIZATION | **PASS (hybrid); enumeration eager** | row fetch lazy; entry enumeration (key-only, bounded) remains eager and is recorded as the residual `LIMIT` cost |
| MEMORY BOUND | **PASS** | per-scan buffer is the entry list, bounded by `max_index_scan_rows`; overlay bounded by `max_write_set_ops`; resident growth 15.7MB at 25,000 matches (was 17.0) |
| STREAMING CORRECTNESS | **PASS** | MVCC/snapshot, LIMIT/OFFSET, ORDER BY, DISTINCT, JOIN, aggregation, UPDATE/DELETE (bounded target collection) verified by the existing and new suites; writes are not pretended to stream |
| BACKPRESSURE | **PASS** | no read-ahead: exactly one fetch per pull over 40 pulls |
| CANCELLATION | **PASS** | cancel after 10 pulls: `Cancelled`, 0 further fetches, transaction intact and committable |
| DEADLINE | **PASS** | an expired deadline fetches nothing |
| TRANSACTION SCAN SEMANTICS | **PASS** | contract analysed (semantics doc sections 1-2); implemented per D10 / transaction ADR section 2 |
| READ-YOUR-OWN-WRITES | **PASS** (was FAIL on scans) | spec-defined, now implemented for every access path |
| SCAN INSERT VISIBILITY | **PASS** | inserted rows visible to PK range, seq scan, index equality/range, composite, mixed |
| SCAN UPDATE VISIBILITY | **PASS** | only the new version visible; moves into/out of index predicates handled |
| SCAN DELETE VISIBILITY | **PASS** | deleted rows invisible; delete+reinsert shows the new row |
| SNAPSHOT CORRECTNESS | **PASS** | other transactions and autocommit never see uncommitted writes; old-snapshot + index-rebuild + overlay combined in the property |
| INDEX CORRECTNESS | **PASS** | every candidate path equals the model; F-2 suites unchanged and passing |
| COMPACTION INTERACTION | **PASS** | 7 automatic Compactions overlapped the transaction property runs (asserted > 0); earlier suites unchanged |
| STATISTICS RESTART | **NOT REQUIRED** | measured: one count per table (+68ms / +440ms at 100K / 1M rows), ~8% on one borderline decision until re-learned, identical results; persistence not added |
| STATISTICS WRITE OVERHEAD | **PASS** | hook unchanged (21-26ns; ~850ns contended); end-to-end within the spread; CREATE/DROP INDEX unchanged |
| DIFFERENTIAL TESTING | **PASS** | independent `BTreeMap` model extended to PK range, secondary index, seq scan, cost-based choice, transaction-local state, old snapshots and index rebuilds |
| PROPERTY TESTING | **PASS** | proptest over sizes, selectivities, NULLs, composite indexes, transaction writes, snapshots, poisoned statistics |
| SECURITY | **PASS** | no new SQL surface or raw-key construction (`ForcePkRange`/`access_path` are server configuration); new counter `access_path_switches` is label-free; statistics hold no SQL text/values; overlay holds only the transaction's own rows |
| RESOURCE LIMITS | **PASS** | `max_result_rows`, `max_index_scan_rows` (checked during racing, fail closed), `max_materialized_rows` (ordered fallbacks), deadline, cancellation all preserved and tested |
| FULL REGRESSION | **FAIL (pre-existing, unrelated)** | section 4 |

## 4. Full-regression record

- `cargo fmt --all -- --check` clean; `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` clean; `cargo check ... --all-features` clean.
- `cargo test --workspace --no-fail-fast` (debug): 1,106 passed, 3 failed.
- `cargo test --release --workspace --no-fail-fast`: 1,107 passed, 2 failed.
- The failures are the same pre-existing items as Increments 16 and 17, **re-verified
  on the Increment 17 tree** in a clean worktree:
  - `m1_2_hundred_writers_throughput` / `m1_3_thousand_writers_throughput`
    (WAL group-commit throughput targets of 15,000 / 80,000 ops/s): fail on
    both trees. Increment 17 tree, release: 8,215 and 59,359 ops/s; this tree,
    release: 6,147 and 48,074 (debug: 487 and 3,800); the spread between runs
    of the same tree on this machine is as large as that difference. Documented
    as failing in `PROCESS.md` since 2026-09-14. WAL is untouched.
  - `two_instances_simultaneous_sustained_read_and_write_load_never_cross_contaminate`
    (debug only; tokio "cannot drop a runtime in a context where blocking is
    not allowed"; passes in release); it leaves an orphaned `rubixdb gui`
    child process that blocks later builds -- killed again.
- Process-level crash and recovery suites (`crash_recovery_integration`,
  `index_backfill_crash_integration`, `crash_consistency`,
  `pathological_recovery_matrix`, `api_commit_ack_loss`) pass in both runs.
  This increment adds no persisted state, no recovery path and no write-path
  change, so no new crash test was warranted.

## 5. Protected-path audit

```
git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/   ->  (empty)
```

## 6. Files changed

Product: `src/relational/txn.rs` (overlay), `src/relational/index.rs` (lazy
row fetcher, resumable probe cursor, `retain_pk_bytes`), `src/relational/
table_store.rs` (`PkCountCursor`, bounded PK count), `src/relational/stats.rs`
(`index_open_ns`), `src/relational/mod.rs`; `sql/src/exec/access_op.rs` (new:
the whole access operator, moved out of `operators.rs`), `sql/src/exec/
operators.rs`, `sql/src/exec/cost.rs` (`ForcePkRange`), `sql/src/exec/mod.rs`
(`with_row`, `access_path_switches`), `sql/src/plan/access.rs`, `physical.rs`,
`explain.rs` (alternatives, `PkRangeScan` fallback). Tests/benchmarks as in
section 2; `sql/src/test_support.rs` (consuming `reopen`).

## 7. Claims, deliberately limited

- **Access-path optimizer:** correct and cost-based across PK range, any number
  of secondary indexes and the table scan, within the measured range (<= 1M
  rows). Not claimed: optimal for sub-0.3ms queries (fixed ~0.1ms), index
  intersection, or beyond 1M rows.
- **Secondary-index executor:** lazy row fetch, snapshot- and
  transaction-correct. Entry enumeration is still eager.
- **Transactional scans:** now consistent with the project's own transaction
  model across every access path. Isolation remains Snapshot Isolation with its
  documented write-skew limitation.
- **Statistics subsystem:** in-memory, bounded, performance-only; restart cost
  measured and judged immaterial. No claim of production-readiness for any of
  these four, nor for RubixDB as a whole.

Not measured: more than 1M rows, process-cold runs, mixed read+write
concurrency, the read-path plateau's cause. Stopped after Increment 18 as
instructed: subqueries, CTEs, set operations, window functions, Router,
Replication and Partitioning were not started.
