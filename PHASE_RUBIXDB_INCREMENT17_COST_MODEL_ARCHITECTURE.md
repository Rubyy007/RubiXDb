# Increment 17 (Part 2): Cost-Based Access-Path Selection

Companion to `PHASE_RUBIXDB_INCREMENT17_INDEX_SNAPSHOT_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT17_PERFORMANCE.md` and
`PHASE_RUBIXDB_INCREMENT17_RESULTS.md`. Append-only: Increment 14/15/16
records are unchanged.

## 1. The problem, reproduced before any change

Increment 16 observed that a sargable secondary index always won plan
selection, and that near ~25% selectivity a sequential scan was cheaper.
Before designing anything this increment reproduced it on the Increment 16
tree (`ForceIndex` is exactly that behaviour; N = 100,000 rows,
auto-Compaction on, 3 SSTables, exact-K predicates scattered across the
whole primary-key space):

| Selectivity (K) | index p50 | table scan p50 | index / scan |
|---|---|---|---|
| 0.01% (10) | 0.28ms | 330ms | 0.001× |
| 1% (1,000) | 16.8ms | 305ms | 0.06× |
| 10% (10,000) | 169ms | 318ms | 0.53× |
| 25% (25,000) | 425ms | 333ms | **1.28×** |
| 50% (50,000) | 843ms | 394ms | **2.1×** |
| 75% (75,000) | 1,272ms | 396ms | **3.2×** |
| 90% (90,000) | 1,524ms | 408ms | **3.7×** |

So the always-index behaviour is up to 3.7× slower than necessary at high
selectivity, and its cost keeps rising linearly while the scan's barely
moves. (The 25% "crossover" is a *measurement on this machine and data*,
not a design constant — see §4.)

## 2. What the system already knows (statistics inventory)

| Quantity | Available today? |
|---|---|
| table row count | **No** — not in the catalog, not tracked anywhere |
| index entry count | No (equals the row count for a non-unique index including NULLs, but unrecorded) |
| distinct values / histograms | No |
| exact number of entries matching a predicate | **Yes, cheaply**: enumerating index entries is key-only, ~0.6µs per entry (Increment 16 measurement) |
| engine-level counts (SSTable count, block reads) | Process-global only; not per table |
| per-row scan and fetch costs | Observable by the executor itself |

## 3. Statistics architecture — options and choice

| Option | Cost | Staleness | Verdict |
|---|---|---|---|
| Existing catalog metadata | none | n/a | contains no counts |
| Persisted per-table/column statistics (`ANALYZE`-style) | a new catalog table, maintenance on every write or a periodic job, recovery rules, schema/migration | stale between runs | **rejected**: a subsystem for one heuristic; durable state to keep consistent |
| Per-write maintained exact counters | a read-modify-write per row write, or a read per write to tell insert from update | none | **rejected**: materially slows every write |
| **Bounded in-memory runtime statistics, derived from work the system already does** | one relaxed atomic add per write, only for a table that has an estimate | rigorously bounded (below) | **chosen** |

The chosen design (`src/relational/stats.rs`, `RuntimeStats`, owned by
`TableStore`):

- **Row-count estimate with a drift bound, per table.** Learned *for free*
  from three events: a scan that visits a whole table to exhaustion, an
  index backfill (which enumerates every row), and — only when a decision
  genuinely needs one — an explicit `TableStore::count_rows` (one key pass).
  Every row mutation applied through this process (`put_row`, `put_rows`,
  `delete_row`, transaction commit) increments the table's `drift`; since one
  mutation changes the row count by at most one, `|true_count − rows| ≤ drift`
  **holds rigorously** (property-tested under random INSERT/UPDATE/DELETE/
  multi-row delete, 400 steps, checked after every step). A table with no
  estimate is not tracked at all, so for it the write hook is a read-lock and
  a hash probe.
- **Two per-row cost parameters** (ns per row produced by a sequential scan;
  ns per row produced by the index path), initialised to values measured on
  this machine and then folded with exponentially-weighted observations
  taken from the executor's own scans (weight 1/8, clamped to
  `[default/8, default×8]`; sampled every 16th row for lazy scans; ignored
  below 64 rows). They are not constants: the crossover differs between
  machines and data residency.
- **Bounded:** ≤ 4,096 tracked tables (two atomics each, ≈ 400KB at the cap),
  one cost record per process. No per-index, per-value, per-query, or
  label-bearing state.
- **Nothing persisted → nothing to recover, migrate or corrupt.** A restart
  forgets the estimates and the first decisions are made conservatively
  (below) until they are re-learned.

## 4. The cost model (`sql/src/exec/cost.rs`)

```
cost_seq   = N · seq_ns_per_row            (decode + predicate for every row)
cost_index = K · index_ns_per_row          (enumerate entry + point read + decode)
K* = N_hi · seq_ns_per_row / index_ns_per_row        N_hi = rows + drift
```

`K` is **exact**: the executor enumerates the matching index entries *before*
fetching any row. The index is abandoned for a table scan only when
`K > K*`. There is no percentage in the code; the default parameters
reproduce the measured ~23.4% crossover at 3,400 / 14,500 ns, but a
cheaper-index machine moves it without a code change (unit test:
`break_even_scales_with_table_size_and_cost_ratio`).

### Conservative by construction

The two errors are asymmetric: choosing the index when a scan is cheaper
costs a bounded factor (the per-row cost ratio, ≈ 4×); choosing a scan when
the index is cheaper costs `N/K`, unbounded. Every uncertainty resolves
toward the index:

1. `N_hi = rows + drift` — the largest the table can be, which makes the
   scan look *more* expensive.
2. With **no estimate**, a match count below `PROBE_FLOOR = 128` goes
   straight to the index (worst-case loss ≈ 2ms). Only when 128 matches are
   exceeded is the table counted — once, then cached — so a point-ish
   lookup never pays for statistics.
3. An estimate that may have drifted by more than a quarter of the table
   (`drift > rows/4 + 64`) is refreshed by an exact count before a scan is
   chosen. The count resets the drift, so a table is recounted at most
   once per quarter-table of mutations — amortized constant work per write.
4. An index scan an eliminated `Sort` relies on never abandons the index.
5. A `NULL` key component, a snapshot older than the index (F-2), and
   `max_index_scan_rows` keep their existing, stricter semantics.

**Statistics never affect correctness.** Both candidate paths run the same
predicate over the same snapshot, so a wrong estimate can only choose the
slower one. This is enforced by test, not by promise: every query shape
(index equality, one- and two-sided range, composite, mixed, PK equality,
PK range, unindexed, empty, `OR`, COUNT, INNER and LEFT JOIN with a
correlated inner index, DML) runs under `Auto`, `ForceIndex` and `ForceSeq`
on tables from 0 to 700 rows, **with the statistics deliberately poisoned**
(table size believed 0, 1, `u64::MAX/2`, random; scan cost absurdly cheap,
index cost absurdly dear, and the reverse), and must equal an independent
brute-force model.

## 5. Where the decision is made — and why not in the planner

A plan is built before the transaction's snapshot, the bound parameter
values, and (for the inner side of a correlated join) the current outer row
exist, and `K` depends on all three. So the cost-based choice is made at
**execution**, per `AccessOp::build`, using exact `K`. This also satisfies
"do not apply a global selectivity estimate to an access path whose values
depend on each outer row": an index nested-loop join re-decides for every
outer row with that row's own exact `K`.

The *planner* is unchanged in what it generates (PK equality →
`PkLookup`; PK range → `PkRangeScan`; sargable secondary predicate →
`IndexScan`), except that each `IndexScan` now carries its complete
fallback predicate (`IndexFallback`, Part 1). The planner's cost is
unchanged within measurement noise (parse/bind/plan: 8–23µs before and
after). `EXPLAIN` shows the planned access; the executed one is visible in
the counters (`index_cost_fallbacks`, `index_snapshot_fallbacks`,
`seq_scans`).

### Execution flow for an `IndexScan`

```
resolve key/bounds from this execution's values   (NULL component -> empty)
loop:
    est = row_estimate(table); limit = break_even(est) | PROBE_FLOOR if none
    probe(index, spec, snapshot, entry_limit=limit, max_rows)
        Entries(e)  -> fetch rows            (index path; record cost)
        Unusable    -> table scan            (F-2: index not Ready at snapshot)
        Truncated   -> if estimate missing/stale and not yet counted:
                           count_rows(); continue
                       else table scan       (K > K*: cost-based fallback)
```

Both paths' costs are fed back: a completed table scan records its sampled
per-row cost and (if it visited the whole table) the exact row count; a
completed index path records `entries / elapsed`.

## 6. Candidate access paths evaluated

| Path | Treatment |
|---|---|
| PK equality (`PkLookup`) | unchanged; never a choice |
| PK range (`PkRangeScan`) | unchanged (Increment 15); bounded by definition, never costlier than a scan |
| Secondary-index equality / range | cost-based: index vs table scan, exact `K` |
| Composite secondary index (prefix equality + range) | same decision, same model |
| `SeqScan` | the fallback; also the plan when no sargable predicate exists |
| Index vs PK range, both applicable | **not** cost-compared (the planner prefers the path consuming more conjuncts, as before). Noted as a possible future refinement; not measured as a problem here |

## 7. What was deliberately not done

- No histograms, distinct counts or per-column statistics (the exact `K`
  makes them unnecessary for this decision).
- No persisted statistics; no `ANALYZE` command; no SQL surface.
- No change to eager index materialization (see the results document:
  measured at +13% over the result itself at 25,000 matches, and the cost
  model now routes high-selectivity queries away from it).
- No engine change (`src/wal/`, `src/manifest/`, `src/sstable/`,
  `src/compaction/` untouched).
