# Increment 17 (Part 1): Secondary-Index Snapshot Correctness — F-2

Companion to `PHASE_RUBIXDB_INCREMENT17_COST_MODEL_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT17_PERFORMANCE.md` and
`PHASE_RUBIXDB_INCREMENT17_RESULTS.md`. Append-only: the Increment 14, 15
and 16 records, including Increment 16's statement of F-2 as an open
finding, are unchanged historical evidence.

## 1. The defect (F-2), reproduced on HEAD before any change

A transaction whose snapshot predates an index's creation or rebuild
could be answered **through that index**, and receive wrong results.
Increment 16 had one `#[ignore]`d reproduction; this increment first
verified, on unmodified HEAD (commit `730ecca`), five independent
deterministic reproductions in
`sql/src/index_snapshot_tests.rs` (no sleeps; interleavings fixed by
construction), each comparing the indexed answer with an independent
table-scan reference of what the snapshot can see:

| Scenario | HEAD result (first failing assertion) |
|---|---|
| `CREATE INDEX` after `BEGIN` | `T1: b = 's1'` returned `[]`, expected the visible row |
| `DROP INDEX` + `CREATE INDEX` after `BEGIN` | same, rows missing |
| indexed value `UPDATE`d between snapshot and rebuild | rows missing under the old value (the rebuilt index only knows the new one) |
| `BEGIN` *during* `CREATE INDEX` (index `Building`, then promoted) | rows missing |
| INSERT/UPDATE/DELETE interleaved across the build with snapshots T1 (before), T2 (during), T3 (after) | T1 and T2 wrong, T3 correct |

All five fail on HEAD and pass after the fix. The "during the build"
cases are deterministic because they drive the index's *real* lifecycle
pieces: `CatalogService::create_index` (state `Building`) followed by
`IndexBuilder::recover_incomplete_builds` (the real backfill and the real
`Building -> Ready` promotion), so a transaction can be begun at an exact
point of the timeline.

## 2. Root cause — the exact timeline

```
seq   event                                            who
----  -----------------------------------------------  ---------------------------
 ..   rows R1..Rn written, each at its own seq         writers
 T0   catalog row for index X inserted, state=Building writer of CREATE INDEX (epoch write lock)
 --   from here every writer maintains X in the SAME   TableStore::put_row/put_rows/
      write_batch as its row (entry seq == row seq)    delete_row, Transaction::commit
 T1   backfill takes snapshot S1 and enumerates PKs    IndexBuilder::backfill
 T2.. backfill chunk k: under the table epoch write lock, re-reads each row's CURRENT
      value and writes its entry via write_batch  ->  entry stamped with the BACKFILL
      chunk's sequence number, NOT the row's
 T5   catalog row for X rewritten, state=Ready          mark_index_ready  (this write has
                                                        engine sequence R)
 ..   planner lists indexes from the CURRENT catalog and
      treats every Ready index as usable                 plan_table_access
```

A reader holds snapshot sequence `s` (`Transaction::snapshot_seq`, the
durable watermark at `BEGIN`). The executor reads **index entries as of
`s`** and each **row as of `s`**:

- A backfilled entry's sequence is the *backfill's*, which is greater
  than a snapshot taken before (or during) the build. So the entry is
  invisible at `s`, while the row it describes (written long before) is
  visible. Result: **missing rows**.
- If the row's indexed value changed after `s` but before the rebuild,
  the only entry the rebuild wrote is for the *new* value, so the row is
  absent from the index under the value its visible version has. Result:
  again **missing rows**.

Only *missing* rows arise. I examined whether the mirror-image failure
(an index entry that is visible to the snapshot but contradicts the
row version the snapshot sees -- an *extra* row) is possible and it is
not: entries written by ordinary writers during the build carry the
row's own sequence (written in the same atomic batch, so they always
agree with the row version at any snapshot), and backfilled entries are
simply invisible to any snapshot earlier than their chunk. The
safety tests nevertheless assert both directions (no missing, no extra,
no duplicate rows), because the property is "identical to the table
scan", not "no missing rows".

The difference between index-entry visibility and table-row visibility
is that entries written by ordinary writers share the row's sequence
(atomic batch), whereas **backfilled entries are retroactive derived data
stamped with a later sequence**. For a snapshot at or after the
promotion's sequence `R` the two views coincide again: every backfilled
entry has sequence `< R`, and every later write maintains the index in
the same atomic batch as its row.

Therefore the sequence that represents readiness is exactly the engine
sequence `R` of the `Building -> Ready` catalog write, and the correct
validity rule is: **an index may serve a read only if `snapshot_seq >= R`.**

Why the planner considered it valid: `plan_table_access` reads the
*current* catalog (state `Ready` now) and a plan is built before a
transaction's snapshot or parameter values are known; the executor then
trusted the plan.

## 3. Options evaluated

| | A. Ready sequence + planner/executor refusal | B. Fall back to a table path for old snapshots | C. Snapshot-aware index representation |
|---|---|---|---|
| Correctness | exact: `snapshot >= R` | exact (it *is* the table) | exact if entries carried the row's own sequence |
| Query latency (old snapshot) | refuses the index | table scan (cost of a scan) | index speed |
| Planner complexity | must know the snapshot at plan time (it does not) | plan carries a fallback; decided at execution | none visible |
| Catalog overhead | a new column in `system.indexes` (schema change, migration of existing rows, backfill of a value that was never recorded for existing indexes) | none | none |
| Index creation overhead | one extra field | none | **entries must be written with the originating row's sequence — requires an engine write API that accepts a caller-chosen sequence (certified-engine change) or per-version entry duplication** |
| Memory / disk | negligible | none | per-version entries multiply index size |
| Transaction interaction | refusal check per scan | same check per scan | none |
| Compaction interaction | n/a | n/a | entries of retired row versions must be retained and compacted in step with rows |
| Recovery complexity | the new field must be persisted, recovered, and defined for pre-existing indexes | none | high |

**Chosen: A and B combined, with the ready sequence *derived from the
catalog's own MVCC version* rather than stored as a new field.**

- *The validity predicate is A.* "Was the index `Ready` as of this
  snapshot?" is answered by reading the index's catalog row **as of the
  snapshot's sequence** (`CatalogService::get_index_as_of`, a plain
  `engine.get_as_of` on the catalog key). Catalog rows are ordinary
  MVCC-versioned engine keys; the version visible at `s` is `Building`
  (or absent) for `s < R` and `Ready` for `s >= R`. The promotion's own
  engine sequence **is** `R`. Nothing new is stored, so nothing new can
  be lost, stale, or forgotten across a crash, and there is no schema
  change and no migration (indexes that already exist work immediately).
- *The remedy is B.* When the predicate fails, the executor runs the
  semantically identical table scan over the same snapshot with the
  table access's complete predicate.
- *C is rejected:* it cannot be built without crossing the certified
  engine boundary (caller-supplied sequence numbers on write) or without
  multiplying index size. Per the mandate that would have required
  `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`; it was not needed.

## 4. Implementation

`src/catalog/service.rs`: `CatalogService::get_index_as_of(index_id, seq)`.

`src/relational/index.rs`:
- `IndexBuilder::index_row_usable_at(index_id, seq)` — returns the
  catalog row as that snapshot saw it, only if its state there was
  `Ready`.
- Index reads are split into **phase 1**
  `probe_index_entries_as_of(...) -> IndexProbe::{Unusable, Truncated,
  Entries}` (validate + enumerate entries, key-only) and **phase 2**
  `fetch_index_rows_as_of(...)`. `Unusable` is the F-2 signal. The old
  eager `index_lookup_as_of*` / `index_range_scan_as_of*` methods are thin
  wrappers that now *error* (rather than return wrong rows) for an
  unusable index, so no caller can silently receive F-2 results. An index
  `Ready` at the snapshot but dropped since remains usable for that
  snapshot (its entry tombstones carry later sequences); one that was
  `Dropping` at the snapshot is not (writers had stopped maintaining it).

`sql/src/plan/access.rs`, `sql/src/plan/physical.rs`: every
`PhysicalAccess::IndexScan` carries an `IndexFallback { predicate,
order_ordinals }`: the table access's **complete** predicate (never just
the residual) and, when a `Sort` node was eliminated because the scan
delivers index order, the index's column ordinals. The decision is made
**at execution** (`AccessOp::build`) because a plan is built before the
snapshot and parameter values exist — and a correlated join supplies a
different key per outer row.

`sql/src/exec/operators.rs`: on `IndexProbe::Unusable` the operator
builds the fallback: a snapshot-consistent `scan_table_rows_as_of` with
the full predicate; if an eliminated `Sort` relied on the index order, the
matching rows are collected (bounded by `max_materialized_rows`) and
stably sorted ascending / `NULLS FIRST` by the index's ordinals with the
same comparator `SortOp` uses, so ties come out in primary-key order —
exactly the physical index order. It never returns an empty or partial
result and never rejects a query a table scan can answer. A NULL key
component still short-circuits to an empty result (a predicate that can
never be true is empty under every snapshot). `UPDATE`/`DELETE` find
their target rows through the same operator, so they inherit the fix.

Counters: `ExecMetrics::index_snapshot_fallbacks`.

## 5. The "index ready sequence" questions, answered

- **When assigned:** by the existing `mark_index_ready` catalog write; its
  engine sequence number is `R`. No second write exists.
- **How it survives restart / how stored:** it is the version stamp of the
  durable catalog row — the WAL/SSTable MVCC machinery already persists,
  recovers and orders it. A restart-time test
  (`index_ready_sequence_survives_restart`) reopens the engine and checks
  `get_index_as_of` still returns `Building` below `R` and `Ready` at and
  above it.
- **`CREATE INDEX`:** a fresh `index_id` whose first version is
  `Building`; every snapshot older than its promotion treats it as
  unusable.
- **`DROP INDEX` then `CREATE INDEX`:** the new index has a new surrogate
  `index_id`; an old snapshot finds no such row (or `Building`) as of its
  sequence and falls back.
- **Old snapshots:** fall back to the table path (§4).
- **Transaction start:** `BEGIN` pins the durable watermark and registers
  it (`SnapshotRegistry`); nothing else is recorded, and the check is made
  at each index access against that pinned sequence.
- **Compaction:** `compaction::merge` retains the version of a key
  visible at `oldest_live_snapshot_seq()`; an open transaction's
  registered snapshot therefore keeps the index row version it needs
  alive. Verified: 37+ automatic Compaction cycles overlapped the
  randomized snapshot suites with zero wrong results.
- **Crash during the build:** unchanged — the index is still `Building` on
  restart, `recover_incomplete_builds` restarts the backfill, and the new
  promotion defines a new `R`. A snapshot can never predate a promotion
  that has not happened.

## 6. Evidence

- 8 deterministic scenarios in `index_snapshot_tests.rs` (above, plus
  ordered-fallback order preservation with the Sort node asserted absent
  from the plan, JOIN / aggregation / `DELETE` through the index inside an
  old-snapshot transaction, and an assertion that the fallback counter
  fires exactly when the snapshot predates the index and not otherwise).
- The Increment 16 reproduction is no longer ignored:
  `snapshot_started_before_index_rebuild_sees_all_rows_f2_regression`.
- The randomized differential harness no longer retires snapshots across
  `DROP`/`CREATE INDEX` (the Increment 16 workaround is deleted); a new
  churn property performs 76 index rebuilds with open snapshots verified
  against the independent model.
- **Mutation check:** making the validity check ignore the snapshot
  (reading the catalog as of `u64::MAX`) makes **all 10** of these tests
  fail; the real code makes them pass.

## 7. Limits stated plainly

- A snapshot older than an index is served by a table scan, so such a
  query costs a scan until the snapshot ends; that is the price of
  correctness and only applies to snapshots that predate an index
  promotion.
- The decision is per access, at execution; `EXPLAIN` shows the planned
  access, not the executed one (the executed one is visible through the
  counters).
- Scans inside a transaction do not overlay that transaction's own
  uncommitted writes (a pre-existing design property of every scan
  access path, unchanged here).
