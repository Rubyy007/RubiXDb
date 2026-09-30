# Increment 15: Primary-Key Range Scan — Architecture

## 0. Scope and mandate

Closes the specific, named failure recorded in
`PHASE_RUBIXDB_INCREMENT14_BLOCKER9_PK_RANGE_SCAN_ADR.md`: a bounded
PK-range predicate (`WHERE id >= x AND id < y`) fell back to a full
`SeqScan` of the whole table, with cost growing with total table size
rather than the requested range. This document is the architecture
record for the fix; `PHASE_RUBIXDB_INCREMENT15_PK_RANGE_PERFORMANCE.md`
is the measured evidence; `PHASE_RUBIXDB_INCREMENT15_PK_RANGE_RESULTS.md`
is the correctness/regression certification.

Per the Increment 15 mandate: **no certified storage-engine code**
(`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, the
Read/Write Engine) **was touched**. Everything below lives in the
relational layer (`sql/src/plan/access.rs`, `sql/src/exec/*`,
`src/relational/table_store.rs`) and reuses certified engine
primitives (`LsmEngine::range_scan`, `LsmEngine::get_as_of`)
unchanged, through the exact same calling convention `SeqScan` and
secondary-index `IndexScan` already used before this increment.

## 1. Investigation (performed before any code change)

### 1.1 Physical relational key layout

`src/relational/key.rs::table_row_key(table_id, encoded_pk)` builds:

```
0x01 (RELATIONAL_NAMESPACE) || table_id:u32 BE || 0x00000000:u32 BE || encoded_pk
```

`table_row_range(table_id)` bounds the whole table to
`[table_row_key(table_id, &[]), table_id||1)` — i.e. the reserved
`index_id = 0` slot. This is **structurally identical** to a secondary
index entry's own layout
(`src/relational/index_key.rs::index_entry_key`):

```
0x01 || table_id:u32 BE || index_id:u32 BE (>0) || encoded_indexed_columns || encoded_pk
```

`index_key.rs`'s own module doc comment already states: *"`index_id =
0` reserved for the table's own row key"* — the two layouts were
designed to share one physical addressing scheme from the start; nothing
had yet been built to exploit it for the primary key's own range
queries.

### 1.2 Primary-key encoding (order-preserving, proven, not assumed)

`encode_composite_key`/`encode_key_value` (`src/relational/key.rs`)
encode every key-bearing type so that
`byte_lexicographic_compare(encode(a), encode(b)) == logical_compare(a, b)`
— proven by property tests already in the file (integers via a
sign-flip transform, floats via a monotonic IEEE-754 bit transform,
`TEXT`/`BLOB` via escape-then-terminate). Composite (multi-column) keys
are the per-column encodings concatenated in declared column order,
and order lexicographically by the first column, then the second, an
already-tested property (`composite_key_orders_by_first_column_then_second`).
This is exactly the ordering a range-bound translation needs to be
safe.

### 1.3 Existing certified LSM range capability — sufficient, unmodified

`TableStore::scan_table_as_of`/`scan_table_rows_as_of` already call
`LsmEngine::range_scan(start, end, as_of_seq)` — a certified,
snapshot-aware (threads `as_of_seq` through, same mechanism a
transaction's pinned-snapshot reads already use), **lazy** primitive
(`RangeScanIter`, `ADR-RE-002`; `scan_table_rows_as_of`'s own doc
comment: *"decodes one row per `next()` call, never collects"*) — used
today with the *whole table's* byte range. **Answer to the mission's
own decision gate: YES, the existing certified engine primitive
already supports an arbitrary relational PK range** — no engine change,
and no `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md` escalation, was
needed.

### 1.4 Reused, not reinvented: `index_scan_range`

`src/relational/index_key.rs::index_scan_range(table_id, index_id, start, end)`
already converts a possibly-partial-prefix `Bound<Vec<u8>>` pair into
the *correct* physical `[start, end)` byte range for one index's
entries — including the subtle "successor of a prefix" computation
(`index_entry_prefix_range`'s carry/increment loop) required so that
an `Included`/`Excluded` bound on a **prefix shorter than the full key**
correctly includes/excludes every row sharing that prefix, regardless
of trailing-column suffix bytes. This is exactly the same correctness
requirement a PK-range prefix bound has (§2 below), and
`index_entry_range(table_id, 0)` is bit-for-bit identical to
`table_row_range(table_id)` (§1.1). **The fix reuses
`index_scan_range(table_id, 0, ...)` verbatim** — zero new byte-range
math was written; the existing, already-tested successor/prefix logic
is inherited exactly as it exists for secondary indexes today.

## 2. Multi-column PK correctness (the mission's explicit hard requirement)

A PK range is only ever planned as a **leading-column prefix**,
mirroring `IndexAccessMode::Range`'s own established, safe contract:

- Zero or more leading PK ordinals matched by an **equality** conjunct
  form a fixed prefix.
- At most one more PK ordinal — the one immediately after the equality
  prefix — may contribute a `>`/`>=`/`<`/`<=` range bound.
- Every PK ordinal after that is left **entirely unconstrained** within
  the bound (not evaluated, not assumed) — correct because, per §1.2,
  a shorter prefix's encoded bytes are always a genuine byte-string
  prefix of any longer key extending it (fixed-width columns) or
  self-delimited via escape-termination (`TEXT`/`BLOB`), never an
  ambiguous truncation.
- Any predicate shape that does **not** fit this pattern (independent
  ranges on two different PK columns with no equality prefix between
  them, `OR`, a non-column expression) is **not recognized** and the
  planner falls through to the pre-existing `SeqScan` path unchanged —
  never a guessed or partial physical range.

Verified concretely (not just argued) for the two-column case in
`sql/src/exec_tests.rs::pk_range_scan_on_composite_primary_key_prefix_returns_every_matching_row`:
a table with `PRIMARY KEY (a, b)`, rows `(1,1),(1,2),(1,3),(2,1),(0,9)`,
`WHERE a = 1` returns exactly `{(1,1),(1,2),(1,3)}` — every row sharing
the prefix, not one arbitrary match a naive truncated-key `Included`
bound would have produced (the exact landmine the mission named).

## 3. Design: `PkRangeScan`

### 3.1 New physical access variant

`sql/src/plan/access.rs::PhysicalAccess::PkRangeScan { table_id,
table_ref, start: Bound<Vec<BoundExpr>>, end: Bound<Vec<BoundExpr>>,
residual: Option<BoundExpr> }` — same shape as `IndexScan`'s `Range`
mode, but over the table's own row key rather than a secondary index.

### 3.2 Planner: `candidate_pk_range_access`

A **new, separate, non-shared** function
(`sql/src/plan/access.rs::candidate_pk_range_access`), deliberately
**not** refactored out of the existing `candidate_index_access` into
common code, even though the two functions' bodies are structurally
very similar (~40 duplicated lines). This is an explicit trade-off:

| | Shared refactor | Separate function (chosen) |
|---|---|---|
| Code size | Smaller | ~40 lines larger |
| Risk to certified secondary-index path | Any bug in the shared extraction risks both callers | Zero — `candidate_index_access` is untouched, byte-for-byte |
| Review/verification cost | Must re-verify the shared core against both use sites | Each function independently verifiable against its own existing test suite |

Given this phase's explicit "preserve the certified relational layers"
mandate and the mission's own "never trade correctness for
performance," the separate function was chosen: it guarantees the
already-certified secondary-index selection logic (`candidate_index_
access`, `plan_table_access`'s `PkLookup` branch) is provably unchanged
— confirmed by every pre-existing planner test in `plan_tests.rs`
still passing verbatim, with zero modifications to their expectations.

`plan_table_access` seeds `best` with the PK-range candidate (when one
exists) *before* the secondary-index selection loop, so it competes on
the same "most consumed conjuncts wins" rule already governing
secondary-index choice; on an exact tie, `PkRangeScan` wins (seeded
first, and the loop only replaces `best` on a *strictly greater*
score) — a table's own row key needs no extra index-entry indirection,
so this is the natural default.

### 3.3 Storage primitive: `TableStore::scan_table_pk_range_rows_as_of`

```rust
pub fn scan_table_pk_range_rows_as_of(
    &self,
    table_id: u32,
    start: Bound<Vec<RelationalValue>>,
    end: Bound<Vec<RelationalValue>>,
    as_of_seq: u64,
) -> Result<impl Iterator<Item = Result<(Vec<RelationalValue>, Row)>> + '_>
```

Encodes `start`/`end` via the existing `encode_composite_key` (the
exact function `put_row`/`get_row`/`delete_row` already use for PK
values — never a new encoding), calls
`index_key::index_scan_range(table_id, 0, start_bytes, end_bytes)`
(§1.4) to get the correct physical byte range, then calls
`self.engine.range_scan(...)` — **the same certified call
`scan_table_rows_as_of` already makes**, only with a narrower range.
Returns a lazy iterator (`.map` over the certified `RangeScanIter`),
never a `Vec` — bounded memory is inherited from the engine's own
primitive, not re-implemented.

### 3.4 Executor integration

`sql/src/exec/operators.rs::AccessOp::build` gained one new match arm,
alongside `PkLookup`/`IndexScan`/`SeqScan`. A new `resolve_pk_bound`
helper (distinct from the existing `resolve_bound`, which wraps values
in `Option` for secondary-index `NULL` support that PK columns never
need, D5) resolves `BoundExpr` bounds against the current row/parameter
context, then calls the new `TableStore` primitive. A `NULL`-valued
bound endpoint (e.g. a parameterized query with a `NULL` parameter)
produces an empty result, matching `PkLookup`/`IndexScan`'s identical
existing rule ("a `NULL` bound endpoint can never match").

`UPDATE`/`DELETE`'s target-row planning
(`sql/src/exec/write.rs::find_target_pks`) already shares this exact
executor machinery with `SELECT` — **no separate implementation was
needed** for write statements; they inherit `PkRangeScan` automatically
through the same `PhysicalAccess`/`build_operator` path Increment 9
already certified for `PkLookup`/`IndexScan`/`SeqScan`.

### 3.5 Every other `PhysicalAccess` consumer updated (compiler-enforced)

Rust's exhaustive-match checking surfaced every site that pattern-
matches `PhysicalAccess` and required a decision, not a guess:

- `sql/src/exec/operators.rs::access_table_ref`, `sql/src/exec/write.rs::access_table_ref` — trivial `table_ref` extraction.
- `sql/src/plan/explain.rs::write_access` — `EXPLAIN` output: `PkRangeScan table_id=… table_ref=t… start=… end=…`, reusing the existing `fmt_bound` helper.
- `sql/src/plan/physical.rs::order_satisfied_by_access` — **conservatively returns `false`** (never claims `ORDER BY` satisfaction), matching `SeqScan`'s own existing conservative treatment even though the underlying scan *is* PK-ordered — a deliberate, explicit "leave this optimization for a future, separately-scoped increment" choice, not an oversight (§5).
- `sql/src/plan/validate.rs::access_predicate`, `collect_parameters_access` — structural self-check and parameter-bound validation, mirroring `IndexScan`'s `Range` mode handling exactly (walks `start`/`end` for embedded `$N` parameters).
- `sql/src/plan_tests.rs`, `sql/src/plan_reference_model.rs` — test-only sites (§4).

## 4. Independent differential/property-testing model

`sql/src/plan_reference_model.rs` already existed as an independent,
from-scratch re-implementation of the planner's access-path
classification, used to differentially test `plan_table_access`
without ever calling into it as its own oracle. Extended (not
replaced) with:

- `RefAccess::PkRangeScan` and `pk_range_score` — a from-scratch
  re-derivation of the same leading-prefix-plus-one-range scoring rule
  as `candidate_pk_range_access`, written independently in this
  already-independent test module.
- Two new fixed scenarios (`id >= 1 AND id < 5`, `id > 1`) in the fixed
  differential matrix.
- A new `id_range` proptest parameter (mutually exclusive with
  `id_eq`, mirroring the existing `name_eq`/`name_range` pattern) in
  the randomized differential property test, so PK-range classification
  is now covered by the same randomized cross-check as `PkLookup`/
  `IndexScan`/`SeqScan` always were.

## 5. Explicitly deferred (not part of this increment)

- **`ORDER BY` elimination for `PkRangeScan`** (§3.5): the scan is
  genuinely PK-ordered, but claiming that in `order_satisfied_by_
  access` is a pure optimization with a real (if small) correctness
  risk surface (residual-filter interaction, future prefix-bound
  shapes) not yet independently scoped and tested. Left conservative,
  matching `SeqScan`'s own precedent.
- **Table-size scaling beyond 100,000 rows** and **the full 1/2/4/8/16/32
  concurrency matrix**: scoped down to 1K/10K/100K and 1/8/32
  respectively for this pass's time budget — see
  `PHASE_RUBIXDB_INCREMENT15_PK_RANGE_PERFORMANCE.md` for the explicit,
  named reduction and the evidence gathered within the reduced scope.
- **A shared refactor of `candidate_index_access`/`candidate_pk_range_
  access`** (§3.2): an accepted, explicit trade-off, not a future TODO
  born of oversight.

## 6. Protected-engine audit

```
git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/
```

Empty — confirmed and recorded in
`PHASE_RUBIXDB_INCREMENT15_PK_RANGE_RESULTS.md` §6. No certified
storage-engine file was touched by this increment.
