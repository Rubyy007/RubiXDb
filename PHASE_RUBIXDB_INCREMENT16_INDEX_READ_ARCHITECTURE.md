# Increment 16: Secondary-Index Read Architecture

Companion to `PHASE_RUBIXDB_INCREMENT16_INDEX_READ_PERFORMANCE.md`
(measurements) and `PHASE_RUBIXDB_INCREMENT16_INDEX_READ_RESULTS.md`
(correctness, certification matrix, open findings).

This document is **append-only closure** for the Blocker 9
`indexed_select` finding. It does not rewrite
`PHASE_RUBIXDB_INCREMENT14_BLOCKER9_LONG_DURATION_ENDURANCE.md` or the
Increment 15 PK-range records, which remain historical evidence.

## 1. Problem statement

Blocker 9's endurance run recorded `indexed_select`
(`SELECT * FROM long_endurance_t WHERE grp = 'g{n}'`, secondary-index
equality) at roughly 277.8ms → 783.7ms → 1,148.1ms across three
segments while the table grew from ~105K to ~206K rows. Increment 15
deliberately did not touch it.

The mandate's critical question: **is the bottleneck the secondary index
itself, the row fetch after the index lookup, or the number of matching
rows?** Answered by measurement, not assumption.

## 2. The implementation as it was (read, not assumed)

Pipeline for `IndexScan` (equality or range), all above the certified
engine:

1. `sql/src/plan/access.rs::plan_table_access` picks `IndexScan`.
2. `sql/src/exec/operators.rs::AccessOp::build` calls
   `IndexBuilder::index_lookup_as_of` / `index_range_scan_as_of` and
   receives an eager `Vec<(pk_values, Row)>`.
3. `src/relational/index.rs::scan_entries`:
   - resolves index metadata,
   - `engine.range_scan` over the physical index-entry byte range
     (`index_key.rs`: `0x02 | table_id | index_id | indexed-cols | pk`),
   - for **each** entry: decode indexed columns and PK, then
     `TableStore::get_row_as_of(table_id, pk, as_of)`.
4. `get_row_as_of` called `resolve_table(table_id)` on **every call**:
   `catalog.get_table` (an engine point read plus row decode) and
   `catalog.get_columns` (an engine **range scan** — read-view capture,
   k-way merge setup, decode — over the catalog's `system.columns`
   rows), then the engine point read of the row itself, then
   `decode_full_row`.
5. The executor then enforced `max_index_scan_rows` **after** the whole
   `Vec` had been materialized.

Notably, the PK-range and seq-scan paths resolve table metadata exactly
once per scan (`scan_table_rows_as_of`, `scan_table_pk_range_rows_as_of`);
only the index-then-fetch path re-resolved it per matched row.

## 3. Measured root cause (summary; full tables in the performance doc)

Per-stage decomposition of one indexed-equality read, N = 100,000 rows,
K = 10,000 matches (median of repeated runs):

| Stage | Cost | Share of 1,238ms |
|---|---|---|
| parse + bind + plan | 0.3ms | 0.03% |
| index-range traversal (10,000 entries) | 3.8ms | 0.3% |
| + indexed-column and PK decoding | +2.2ms | 0.2% |
| catalog metadata resolution × 10,000 rows | ~1,007ms | **81%** |
| raw engine point reads × 10,000 rows | ~108ms | 8.7% |
| row decode + projection + remaining executor | ~60ms | 4.8% |
| (HTTP JSON serialization, measured separately) | ~8.5ms | — |

Answers to the critical question:

- **Not the secondary index itself.** Index traversal plus entry decode
  costs ~0.6µs per entry and is independent of table size.
- **Not the engine.** A raw engine point read is ~11µs; the metadata
  resolution wrapped around it was ~100µs (and ~700µs at 1M rows).
- **Row fetch — specifically redundant per-row catalog resolution — is
  the dominant cost, multiplied by the number of matching rows.**
- Cost is proportional to **matched rows (K)**, not to total table size,
  with one coupling: the *per-row constant* rose with LSM state
  (≈12µs/row when everything was in the MemTable, ≈100µs/row with 9
  SSTables, ≈700µs/row with 64 SSTables) because each redundant catalog
  lookup walks every SSTable. In the endurance workload the predicate
  matches ~1/10 of the table (`grp` has 10 values), so K grows with the
  table **and** the per-row constant grows with the LSM — which is why
  latency grew faster than linearly in table size (2× table → ~3–4×
  latency). Both effects are removed by the same fix.

Because the dominant cost lives in the relational layer, the
engine-escalation rule did not trigger and
`PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md` was **not** created. The
residual cost (raw engine point reads, ~11–13µs each) is the certified
engine's normal point-read cost; see §6 for why that is not an engine
defect finding.

## 4. Candidate architectures evaluated

| # | Candidate | Verdict | Evidence |
|---|---|---|---|
| A | Current: index entry → individual row fetch (per-row catalog resolve) | Baseline | §3 |
| B | **Batched/amortized fetch: resolve table metadata once per scan, fetch each row by the already-encoded PK from the index entry** | **Chosen** | 7–29× at K ≥ 1,000; zero write/disk/memory cost |
| C | Safe parallel prefetch of the row point reads | Rejected for now | 10K point reads: 102ms sequential → 45ms with 4 threads (2.3×), but identical total CPU; throughput under concurrent load is already CPU-bound (4 cores / 8 threads), so it only helps an otherwise-idle single query while adding thread-spawn cost, resource-limit and cancellation complexity |
| D | Covering index (store the row in the index entry) | Rejected | Existing index entry 18.9B vs row 81.6B; a covering entry would be ~100B (**5.3× index growth, index ≈ 123% of the table per index**); +232ms per 20K rows of extra write cost per covering index; every UPDATE of any column would have to rewrite every covering index; read ceiling (contiguous decode-only scan) ≈ 34ms at K=10,000 vs ≈ 140ms after fix B — a further ≤ 4× at a large, permanent write/disk price, and useless for `SELECT *` on wide rows |
| E | Cache of decoded rows / metadata | Rejected | Not needed once metadata is resolved once per scan (the repeated work disappears at its source); a cache would add invalidation/snapshot-correctness risk for no further measured gain |
| F | Lazy (streaming) index scan instead of eager `Vec` | Deferred (not implemented) | Memory at K=20,600 rows is ~22–27MB RSS; the eager `Vec` is now bounded **while collecting** (§5.3). A streaming operator is a larger executor change that would also alter `UPDATE`/`DELETE` target collection; no measured need yet |
| G | More threads | Rejected | Not a design; the fix removes work rather than adding workers |

Fix B was the only candidate whose cost is zero in every non-read
dimension, and it removes the measured dominant cost at its source.

## 5. The change

All changes are in the relational/SQL layers. **Protected engine
paths (`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`)
are untouched** (audit in the results doc).

### 5.1 Resolve metadata once per scan (the fix)

`src/relational/index.rs::scan_entries` now calls
`TableStore::resolve_table` **once**, derives the indexed-column types
and PK types from that single column list (also removing a duplicate
`get_columns` call per scan), and fetches each row through
`TableStore::fetch_row_by_encoded_pk` (promoted to `pub(crate)`), passing
the **encoded PK bytes taken directly from the index entry** — no PK
decode/re-encode round trip.

Semantics preserved:

- Same `as_of_seq` for the entry scan and every row fetch (snapshot
  semantics unchanged).
- A row whose entry exists but whose row is not visible at `as_of_seq`
  (deleted/tombstoned) is still silently skipped, as before.
- Schema-snapshot semantics are the same ones `scan_table_rows_as_of`
  already has: metadata is resolved once per scan. The catalog has no
  `ALTER TABLE`; a concurrent `DROP TABLE` is handled as it is for every
  other access path.

### 5.2 Bounded metrics

`IndexStatsSnapshot` gained two label-free counters:
`index_rows_fetched` and `index_scan_micros_total`, beside the existing
`index_lookups` / `index_range_scans` / `index_entries_examined`. The
executor already exposes `index_scans`, `index_rows_examined`,
`table_fetches`, `rows_returned`. No SQL text, parameter value, table
name, or index name is ever a label; no row content is exposed.
(`IndexBuilder::stats()` is not wired to the HTTP `/metrics` route today
— that was true before this increment and is flagged, not changed.)

### 5.3 Materialization bound enforced while collecting

`max_index_scan_rows` used to be checked **after** the whole result had
been built. `IndexBuilder` gained
`index_lookup_as_of_bounded` / `index_range_scan_as_of_bounded`; the
executor passes `ExecLimits::max_index_scan_rows`, and the scan now fails
closed with `ResourceLimit` the moment the bound is exceeded (exactly at
the boundary: `n` rows pass, `n+1` fail). The original unbounded methods
remain as thin wrappers (`usize::MAX`) for existing callers/tests.

### 5.4 Planner correctness fix found by differential testing (F-1)

Not a performance change; see the results doc. An index range with an
upper bound but no lower bound (`a <= x`, `a < x`, or
`a = p AND b <= x` on a composite index) physically spans that column's
NULL entries (NULL sorts first in the key encoding) and the comparison
was marked "consumed", so NULL-valued rows were returned. The upper
bound is now kept as a residual filter for that case only; it still
counts toward access-path selection, so the index range continues to be
chosen. This bug was reproduced on unmodified `HEAD` before being fixed.

## 6. Why the residual cost is not an engine finding

After fix B, per-row cost is ≈ 14–17µs at 100K–206K rows, of which a raw
`get_as_of` is ≈ 10.7–12.7µs. The engine's per-read cost does depend on
LSM shape (SSTable count), but (a) the benchmark fixtures in this
increment ran with automatic Compaction **disabled** unless stated (the
library default; the shipped server/CLI enable it), so absolute SSTable
counts here overstate production, and (b) point reads of PK equality
(`pk_eq`) pay the identical engine cost and were not part of the
reported regression. The engine did not show anomalous behaviour
(correct results, linear scaling after the fix, no regressions); no
evidence supports a certified-engine change, so none was made.

## 7. Invariants preserved

MVCC / snapshot reads, transaction visibility, tombstones, index
maintenance, `CREATE INDEX`/`DROP INDEX`, UPDATE of indexed columns,
DELETE of indexed rows, concurrent writes, Compaction interaction,
binder/planner/authorization (no raw index keys are built from unchecked
input — bounds still flow through the existing bound-expression
resolver and `encode_indexed_columns`), resource limits (now stricter).
Recovery semantics are **inherited unchanged**: the change is read-only
and touches no WAL, Manifest, SSTable, or Compaction code and no
durability path.

## 8. Known pre-existing limitations surfaced (not fixed here)

- **F-2**: a snapshot transaction that began *before* an index was
  (re)built, and then reads through that index, misses rows (the
  backfilled entries carry post-snapshot sequence numbers while the
  planner still selects the index). Reproduced; `#[ignore]`d test
  `known_gap_snapshot_started_before_index_rebuild_misses_rows`. Needs a
  catalog/planner/transaction design decision (e.g. recording an index's
  ready-sequence and refusing it for older snapshots) — out of scope.
- `IndexBuilder::stats()` is not exported through the API metrics route.
- Eager `Vec` materialization for index scans (candidate F).
