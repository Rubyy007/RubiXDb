# Increment 18 (Part 1): Unified Access-Path Selection

Companion to `PHASE_RUBIXDB_INCREMENT18_MATERIALIZATION_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT18_TRANSACTION_SCAN_SEMANTICS.md`,
`PHASE_RUBIXDB_INCREMENT18_PERFORMANCE.md` and
`PHASE_RUBIXDB_INCREMENT18_RESULTS.md`. Append-only: Increment 15, 16 and 17
records are unchanged.

## 1. The question

Increment 17's cost model compared a secondary index with a sequential scan.
It did not compare a PK range with a secondary index: when both were sargable
the planner kept its Increment 8 rule -- strictly more consumed conjuncts wins,
the PK range first on a tie. The mandate: do not assume that rule is optimal;
find every situation where more than one path can legally satisfy a predicate;
measure.

## 2. When more than one access path is legal

For one table access the planner builds candidates in a fixed order:

1. `PkLookup` if every PK column has an equality (always optimal: one key; never
   a choice).
2. A `PkRangeScan` if a leading-PK-prefix range exists (bounded by definition:
   never costlier than a table scan).
3. Every `Ready` secondary index with a usable leading-column predicate
   (equality prefix, optionally a range on the next column), in catalog order.
4. A table scan if nothing is sargable.

(2) and (3) can coexist whenever a predicate has both a PK comparison and an
indexed-column comparison (`id >= lo AND id < hi AND a = v`), and several (3)
candidates coexist whenever several indexed columns appear (`a = 1 AND b = 's'`
with indexes on `a` and `b`, or a composite index on both). Each is a complete,
independently executable access with its own residual; every one applies the
same complete predicate, so they are result-equivalent by construction.
Composite indexes are supported (leading-prefix equality plus a range on the
next column) and compete like any other candidate; a candidate that cannot
cover a conjunct leaves it in its residual.

## 3. Baseline (the Increment 17 tree; N = 100,000, auto-Compaction on)

Each path measured independently on the *same logical predicate* by writing it
in a shape the planner cannot use for the other path (`id + 0 >= lo` defeats the
PK range; `(e = 'g0' OR e = 'g0')` defeats the index). `COUNT(*)` keeps result
materialization out. R = rows in the PK range, K = rows matching the index
predicate, p50 ms; "auto" is what the planner chose (always the PK range here).

| R | K | auto (PK range) | best path | best p50 | regret |
|---|---|---|---|---|---|
| 1,000 | 10 | 3.139 | index | 0.230 | **13.6x** |
| 1,000 | 100 | 3.814 | index | 1.488 | 2.6x |
| 10,000 | 10 | 35.03 | index | 0.230 | **152x** |
| 10,000 | 100 | 38.00 | index | 1.574 | 24x |
| 50,000 | 10 | 193.3 | index | 0.231 | **838x** |
| 50,000 | 100 | 194.0 | index | 1.554 | 125x |
| 50,000 | 10,000 | 196.7 | index | 131.0 | 1.5x |
| every R <= K case | | | PK range | | 0.93-1.0x (planner right) |

Where the PK range is the right answer the planner was right; where the index
is, it was wrong by up to 838x. The planner's conjunct-consumption rule is a
structural heuristic with no relation to row counts, so it cannot be right in
general. **OPEN 1 is confirmed as a real defect, not a theoretical gap.**

## 4. Design

### 4.1 Planner: keep the structural choice, carry the alternatives

`plan_table_access` now collects *all* candidates, applies exactly the old rule
to pick the planned (primary) access, and attaches the others as
`alternatives: Vec<PhysicalAccess>` (bounded by `MAX_ACCESS_ALTERNATIVES = 3`,
so plan size does not grow with the number of indexes). Every candidate is
finished with its own residual and an `IndexFallback` (the complete predicate;
`PkRangeScan` gained one in this increment). Plan shape tests, `EXPLAIN` and
the plan reference model are unchanged: they see the primary. The planner
overhead is unchanged for ordinary shapes (7.9-21us); a predicate that yields
a PK range plus two index candidates costs ~7us more because each candidate
clones the predicate (21.0 -> 27.7us, a bounded constant).

### 4.2 Executor: price each candidate by its exact row count

The decision cannot be made at plan time for the same reasons as in Increment
17 (bound values, the transaction snapshot, a correlated outer row), and it
needs only counts the executor can obtain cheaply: a PK range's row count is a
key-only walk (~0.4us per row), an index candidate's match count is a key-only
enumeration (~0.65us per entry). Costs are priced with the Increment 17
parameters, now extended with one more *measured* parameter:

```
cost_pk    = R * seq_ns_per_row
cost_index = index_open_ns + K * index_ns_per_row
```

`index_open_ns` (100us, `DEFAULT_INDEX_OPEN_NS`) is the fixed cost of opening
an index scan -- catalog reads for the index row as of the snapshot, table and
column metadata -- measured from the K = 1..10 index-lookup latencies of
Increments 16-18. It is not calibrated at runtime; it only decides whether
pricing an index is worth starting. No selectivity threshold exists anywhere.

### 4.3 The race (`choose_by_race`)

A first implementation priced the index with a table-scan-sized budget and
then counted the PK range. It fixed the catastrophic cases but introduced new
regret (R = 10, K = 10,000: 0.095 -> 7.3ms, **65x**) because it spent
milliseconds enumerating index entries to learn that a 10-row PK range was
already cheaper. A budget-widening variant (x4 per round, re-probing each round) cut that to
1.75x but still paid for repeated enumeration. The shipped design:

1. **Round 0** -- PK ranges are opened first and advanced only as far as
   `index_open_ns / seq_ns_per_row` rows (~29). A range that small cannot be
   beaten by any index (the index's *fixed* cost alone exceeds it) and wins
   without opening an index at all.
2. **Race** -- every contender is a **resumable cursor**
   (`IndexProbeCursor`, `PkCountCursor`). Each round advances every contender
   by the same slice of *estimated cost* (slices double); the first to finish
   has an exact cost. It is final once every unfinished contender has been
   given at least that much cost-equivalent progress (none can still win).
   **No enumeration is ever repeated**, and a losing candidate has done only
   about as much probing as the winner's cost -- a few percent of executing it.
3. **Bounded by the table scan** -- if no contender finishes before its
   progress reaches the full-table-scan cost, every index candidate has lost to
   the scan; if a PK range is among the candidates it wins *without being
   counted* (a PK range is never costlier than the scan), otherwise the scan
   runs. A stale or unknown table-size estimate is refreshed by an exact count
   first (as in Increment 17), once.
4. A **single index candidate** (nothing to race) keeps the Increment 17
   decision exactly; a single PK range is never priced.

Properties kept from Increment 17: statistics affect only *which* of
result-equivalent paths runs; unknown/stale information resolves toward the
safer choice; the F-2 snapshot validity check applies to every index candidate
(an unusable index drops out of the race; a PK range -- always valid -- is
preferred over a full scan); `max_index_scan_rows` still fails closed; and an
access whose `Sort` was eliminated never switches candidates (its index's order
is part of its value).

### 4.4 What is deliberately not done

- No PK-range *vs full scan* pricing: a PK range is bounded and cannot lose.
- No attempt to intersect two indexes (index merge); candidates are executed
  singly.
- No cost-based choice *among* several indexes beyond exact counts (no
  histograms; exact counts make them unnecessary).

## 5. Correctness

Result equality across paths is tested, not argued
(`sql/src/cost_model_tests.rs`): every query shape -- index equality, one- and
two-sided ranges, composite, PK equality and range, **PK range + secondary
predicate, PK range + two indexed columns, PK bound + unindexed + indexed,
PK range + one-sided index range**, unindexed, empty, `OR`, COUNT, INNER and
LEFT JOIN with a correlated inner index -- runs under `Auto`, `ForceIndex`,
`ForcePkRange` and `ForceSeq` on tables of 0-700 rows **with deliberately
poisoned statistics** and must equal an independent brute-force model (12
fixed seeds + 16 proptest cases). Decision tests assert the switch itself:
a 3,000-row PK range + a 1-row index predicate executes through the index
(`access_path_switches == 1`, <= 2 index rows examined); a 20-row PK range +
a 2,000-entry index stays on the PK range; the more selective of two
secondary indexes is chosen; an ordered scan never switches.
