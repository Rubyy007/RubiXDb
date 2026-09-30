# ADR: Primary-Key Range Predicates Fall Back to Full Sequential Scan

## 0. Status

**Discovered during Blocker 9** (chained long-duration endurance run,
Segment 1 of 3), not caused by it. This is a pre-existing
characteristic of the already-certified relational planner and
storage-facing table API (`sql/src/plan/access.rs`,
`src/relational/table_store.rs`) — Blocker 9 is simply the first
workload in this project's history to run a bounded PK-range query
against a table that both (a) grows past ~100,000 rows and (b) is
measured continuously for over an hour, which is what makes the cost
visible and its growth-correlated drift measurable.

Per this phase's own contract ("preserve the certified relational
layers... planner... read executor" and the engine-escalation
principle of not silently modifying a certified component when a
measured bottleneck is proven to live there), **no source change is
made in this document**. This is a STOP-and-report artifact, not a
fix.

## 1. Problem

`long_endurance_t`'s workload (`api/examples/long_endurance.rs`) runs
a `range_select` op — `SELECT * FROM long_endurance_t WHERE id >= lo
AND id < lo + 50` — a query that always touches at most 50 rows,
regardless of total table size, since `lo` is drawn from the fixed
`0..900` seed range. Segment 1 of the Blocker 9 endurance run grew the
table from 1,000 seed rows to 105,907 rows over ~115 minutes. Over
that run, `range_select`'s own aggregate average latency was
**149.9ms** (max 450.1ms) — roughly **300x** the aggregate average of
a comparable single-row primary-key lookup (`select`, by exact `id`),
**0.460ms** (max 21.8ms), even though both queries target the same
`id` column and the same hot 0–1000 id-space.

The same pattern appears in `join` (149.9ms avg, 455.9ms max — its
`long_endurance_t` side uses the identical `id` range predicate) and
`group_by`/`HAVING` (171.5ms avg, 522.2ms max — a full unfiltered
scan by construction). The equality-based paths stay fast throughout:
`select` (PK equality) and `indexed_select` (secondary-index equality
on `grp`) both resolve to dedicated, non-full-scan access paths.

## 2. Reproduction (isolated, zero concurrent load)

Immediately after Segment 1 stopped (before Segment 2 started), the
same persistent instance/table (105,907 rows, untouched, no other
client) was restarted and probed directly with plain sequential
`curl` calls — no concurrency, so this isolates query cost from any
MVCC/lock-contention effect of the endurance workload's own 6
concurrent writers:

| Query | Predicate | Rows touched | Latency (3 samples) |
|---|---|---|---|
| Control: PK point select | `id = 500` | 1 | 1.29ms, 1.19ms, 1.01ms (avg 1.16ms) |
| `range_select` | `id >= 100 AND id < 150` | ≤50 | 199.4ms, 205.0ms, 193.8ms (avg 199.4ms) |
| `indexed_select` | `grp = 'g3'` | ~9,600 (≈1/11 of table) | 376.8ms, 374.5ms, 374.9ms (avg 375.4ms) |
| `join` | `t.id >= 100 AND t.id < 120` ⨝ `grp` | ≤20 | 195.5ms, 194.7ms, 205.3ms (avg 198.5ms) |
| `group_by`/`HAVING` | none (full scan) | 105,907 | 250.6ms, 228.3ms, 225.7ms (avg 234.9ms) |

The isolated (final-table-size) `range_select`/`join`/`group_by`
numbers are all *higher* than their own whole-run aggregate averages
from Segment 1 (149.9ms / 149.9ms / 171.5ms) — exactly what a cost
that grows with total table size predicts, since the aggregate blends
in the early part of the run when the table was still small. This
confirms the drift is real and is driven by **total table size**, not
by the workload's own concurrency or by the size of each query's
result set (`range_select` and `join` return at most 50/20 rows
throughout — their cost has no business scaling with table growth at
all if the access path were bounded to the predicate).

## 3. Root cause (verified by reading the actual planner/storage source, not guessed)

`sql/src/plan/access.rs::plan_table_access` chooses exactly one of
three physical access paths for a table, in this order:

1. **`PkLookup`** — only when *every* primary-key ordinal has its own
   **equality** conjunct (lines 154–175). A range comparison
   (`>=`/`<`) on the PK never qualifies, by construction — `PkLookup`
   has no range form.
2. **`IndexScan`** — a **secondary** index only. Line 182–183
   explicitly skips `IndexKind::Primary`, with the module's own doc
   comment (lines 19–28) explaining why: `TableStore`'s index
   maintenance never writes physical index entries for the
   catalog-only `Primary`-kind index row, so selecting it as an
   `IndexScan` "would silently return zero rows for every lookup."
   This is a deliberate, correct safety rule — but its side effect is
   that **no `IndexScan` path exists for the primary key at all**,
   range or equality (equality is instead handled by `PkLookup`
   above; range has no handler anywhere).
3. **`SeqScan`** — the fallback for everything else, including a PK
   range predicate, which becomes `SeqScan`'s residual (unindexed)
   predicate (line 240–244).

`SeqScan`'s one storage-facing primitive is
`TableStore::scan_table(&self, table_id: u32) -> Result<Vec<(Vec<RelationalValue>, Row)>>`
(`src/relational/table_store.rs:295`) — **it takes no bounds
parameter at all**. It unconditionally materializes *every* row of
the table into a `Vec` before the executor applies the residual
predicate/join/aggregate on top. There is no code path by which a
`WHERE id >= x AND id < y` predicate ever narrows what is actually
read from storage — only what is kept after reading everything.

By contrast, secondary-index range predicates already have a proper
bounded primitive one layer down:
`src/relational/index.rs::index_range_scan` (used by `IndexScan`'s
`Range` mode, `sql/src/plan/access.rs:45-54`) accepts real
inclusive/exclusive/unbounded `Bound<Vec<BoundExpr>>` start/end
values. The capability *pattern* already exists in this codebase for
secondary indexes; it simply was never extended to the primary key's
own naturally-key-ordered storage (an LSM engine's rows are already
stored in primary-key order, so a bounded PK scan is very plausibly a
cheap addition at the storage layer — this ADR does not assume that
without the storage-layer's own investigation, per §5).

## 4. Impact assessment

- **Correctness**: none. `SeqScan` + residual filter is always
  correct — every correctness check across Segment 1 (final row
  count, JOIN orphan check) passed cleanly. This is a pure
  performance/resource finding, not a data-integrity one.
- **Durability / recovery**: none — read-only path, no on-disk format
  or WAL/Manifest interaction.
- **Security**: none directly, but see the memory point below.
- **Memory / resource exhaustion**: `scan_table` materializing the
  *entire* table into one `Vec` for *any* non-PK-equality,
  non-secondary-index-equality/range predicate is a real,
  distinct-from-Blocker-11 resource concern: `ExecLimits::
  max_result_rows` (Blocker 11, `PHASE_RUBIXDB_INCREMENT14_
  BLOCKER11_100K_ROW_GUI.md`) bounds what is returned to the *client*,
  but does **not** bound what `SeqScan` reads into server memory
  *before* filtering. A table far larger than this run's 105,907 rows
  with a highly selective PK-range or unindexed predicate would still
  pay full-table materialization cost server-side even though the
  final result set (and therefore the client-visible response) stays
  small and within limits. This is a latent, unmeasured resource-
  exhaustion angle at table sizes beyond what any pass this project
  has run so far has reached.
- **Performance / production readiness**: this is a genuine,
  reproducible, root-caused finding. It maps directly onto this
  project's own named certification gate **"NON-PK READ
  PERFORMANCE"** (distinct from "INDEX READ PERFORMANCE," which is
  unaffected — secondary-index equality/range access is fine). That
  gate is **FAIL**, not merely `OPEN`: it has been tested, and it does
  not meet a reasonable production bar (a bounded 50-row query taking
  150–450ms and growing with unrelated table size is not acceptable
  production read performance for a range query on the primary key).

## 5. Candidate architectures (not implemented here)

**Option A — New `PkRangeScan` physical-access variant.** Extend
`plan_table_access` to recognize PK comparison conjuncts that form a
valid range (mirroring the existing `IndexAccessMode::Range` logic
already used for secondary indexes) and plan a new `PhysicalAccess::
PkRangeScan`. Requires a new storage-facing primitive, e.g.
`TableStore::scan_table_range(table_id, start: Bound<Vec<RelationalValue>>, end: Bound<Vec<RelationalValue>>)`,
that seeks directly into the LSM engine's already-key-ordered rows for
this table. This is the architecturally clean fix, but its actual
cost/complexity depends on facts about `LsmEngine`'s per-table key
layout that only the storage-layer owners can confirm — hence "STOP,"
not "implement."

**Option B — Add bounds directly to `scan_table`.** Smaller API
surface than Option A (extend the existing primitive with optional
start/end bounds rather than adding a new plan-node type), same
underlying storage-layer question and same STOP.

**Option C — Document as a known limitation, no code change.** Lowest
risk, but leaves a real, production-relevant gap uncorrected and an
unbounded-memory-on-read-path resource question (§4) unresolved. Not
recommended as a permanent position, only as this pass's actual
outcome.

**This pass's outcome is Option C**, explicitly *not* because it is
the best design, but because `sql/src/plan/access.rs` and
`src/relational/table_store.rs` are certified relational-layer
components this phase's contract requires be preserved, not silently
changed. Option A or B should be scoped as its own explicitly
authorized increment, with its own baseline, storage-layer
investigation, measurement, and regression pass — never bundled
quietly into endurance/hardening work.

## 6. Certification-matrix entry

| Gate | Result |
|---|---|
| NON-PK READ PERFORMANCE | **FAIL** — PK range predicates fall back to full-table `SeqScan`; measured 150–450ms for a ≤50-row query at 105,907 rows, growing with unrelated table size. Root-caused in `sql/src/plan/access.rs` + `src/relational/table_store.rs::scan_table`. Not fixed this pass (certified-layer boundary). |
| INDEX READ PERFORMANCE | Unaffected by this finding — secondary-index equality (`indexed_select`) uses a dedicated `IndexScan` and its cost scales with matched-row count, not total table size. |

This FAIL is carried into `PHASE_RUBIXDB_INCREMENT14_CERTIFICATION.md`
verbatim, not softened, and is one of the reasons the overall
Increment 14 production-readiness verdict cannot be an unqualified
PASS even once Blocker 9's endurance segments all complete cleanly.
