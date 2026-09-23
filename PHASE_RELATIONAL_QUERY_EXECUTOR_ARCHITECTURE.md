# PHASE_RELATIONAL_QUERY_EXECUTOR_ARCHITECTURE

**Scope**: this document is the decision record for Increment 9 — a
production-grade **query executor**: `Plan`/`PhysicalPlan → Execute →
Typed Result` against the real `TableStore`/`IndexBuilder`/
`Transaction` primitives. **Read-only** — only `Plan::Query` (a bound
`SELECT`) executes; every write-shaped `Plan` variant is an explicit
`SqlError::UnsupportedExecution`, never a silent no-op (item 5: "All
writes are outside this increment"). `RELATIONAL DATABASE PRODUCTION
READY = NO` after this increment.

**Where the code lives**: `sql/src/exec/` (a new module inside the
already-established `rubixdb-sql` crate, per the same "one crate owns
parser/binder/planner/executor" design Increment 8 already followed).

---

## §1. Executor architecture (item 3/6/9/44)

```
Plan → ExecCtx → Operator tree (Box<dyn Operator>) → pull loop → QueryResult
```

`sql/src/exec/operators.rs` defines one struct per `PhysicalPlan` node
kind — `AccessOp` (`PkLookup`/`IndexScan`/`SeqScan`), `NestedLoopJoinOp`,
`FilterOp`, `ProjectionOp`, `DistinctOp`, `SortOp`, `LimitOp`,
`EmptyRelationOp` — each implementing one trait:

```rust
pub trait Operator<'a> {
    fn next(&mut self, ec: &ExecCtx<'a>) -> Result<Option<Tuple>>;
}
```

Pull-based (item 6/44): a consumer only ever asks for the next row, so
`LIMIT` (§9), cancellation, and bounded memory all fall out of the
model itself rather than needing per-operator special-casing. No giant
match statement holds execution logic — `build_operator` (item 9's own
"clear inputs/outputs" requirement) only *dispatches* to the operator
matching each plan node; every operator's own `next` is self-contained.

## §2. A real, necessary architecture gap found and closed (not guessed)

Building this executor required inspecting, not guessing, three exact
APIs — and found two genuine, small primitive gaps the "DO NOT GUESS...
create the smallest necessary architecture/storage ADR" instruction
anticipated:

**Gap 1 — `PhysicalAccess` carried no `TableRefId`.** Every `BoundExpr`
above an `Access` node resolves a `Column` by `(table_ref, ordinal)`
(`crate::bound::ColumnRef`), but `PhysicalAccess::{PkLookup,IndexScan,
SeqScan}` (Increment 8) carried only `table_id` — insufficient for a
self-join (`FROM t a INNER JOIN t b ON ...`, two `Access` nodes sharing
one `table_id` but two distinct `table_ref`s) and insufficient even for
the base case of a bare, predicateless `Scan` (no `BoundExpr` exists
anywhere in that `Access` node to derive a `table_ref` from at
execution time). **Closed**: `sql/src/plan/access.rs` now carries an
explicit `table_ref: u32` on every `PhysicalAccess` variant, threaded
through from `plan_table_access`'s own already-available parameter
(previously computed but discarded). Additive, non-breaking, exercised
by every executor test in this increment.

**Gap 2 — no snapshotted scan-shaped read primitive.** `Transaction::
get_row` is snapshot-correct (D10, already certified), but `Transaction`
has no scan/range counterpart — `SeqScan`/`IndexScan` need one, since a
query running inside an explicit transaction must observe that
transaction's own pinned snapshot for its *whole* result set, not just
single-key reads. Inspected (not assumed): `TableStore::scan_table`/
`IndexBuilder::index_lookup`/`index_range_scan` all called `range_scan`
with `u64::MAX` (current committed state) unconditionally — the exact,
previously-documented, *accepted* gap `IndexBuilder::scan_entries`'s own
Increment 5 doc comment named explicitly: *"an ordinary, expected
index-then-fetch race under no active transaction/snapshot isolation
for reads — D10's future transaction layer is what removes this."* D10
now exists (Increment 7); this increment is its first scan-shaped
caller. **Closed**, smallest necessary addition, in the already-
certified core crate:

- `TableStore::get_row_as_of`/`scan_table_as_of` (`src/relational/
  table_store.rs`) — `..._as_of(..., as_of_seq: u64)` siblings of the
  existing methods, which now simply call the new ones with `u64::MAX`.
- `TableStore::scan_table_rows_as_of` — a genuinely **lazy**,
  row-at-a-time counterpart (see §3 — the one `SeqScan` specifically
  needs to honor "never materialize an unbounded table," item 10/44).
- `IndexBuilder::index_lookup_as_of`/`index_range_scan_as_of` (`src/
  relational/index.rs`) — same pattern; `scan_entries`'s internal row
  fetch now also uses `get_row_as_of` at the *same* `as_of_seq` as its
  own index-entry range scan, which is what actually closes the race
  (both the index scan and the row fetch now resolve at one consistent
  snapshot instead of two independent "now" reads).
- `Transaction::snapshot_seq(&self) -> u64` (`src/relational/txn.rs`) —
  a trivial, read-only getter exposing the one piece of already-private
  snapshot state the executor needs to drive the primitives above; adds
  no new capability to construct or mutate a `Snapshot`, and does not
  touch `SnapshotRegistry`/`oldest_live_snapshot_seq` accounting at all.

Both gaps are additive, non-breaking, and verified: 4 new core-crate
regression tests (`src/relational/{tests,index_tests}.rs`) directly
prove `scan_table_as_of`/`get_row_as_of`/`index_lookup_as_of`/
`index_range_scan_as_of` are stable against a write/delete made *after*
the captured snapshot — `git diff --stat -- src/wal/ src/manifest/
src/compaction/ src/sstable/` remains empty; the two touched core files
(`table_store.rs`, `index.rs`) and the one-line `txn.rs` addition are
the entire footprint outside `sql/`.

## §3. `SeqScan` is genuinely lazy; `IndexScan` is eagerly bounded — an honest, stated tradeoff

**`SeqScan`** (`AccessOp` with `AccessSource::Lazy`) wraps
`TableStore::scan_table_rows_as_of`, itself a thin decode-per-item
`.map()` over the certified `LsmEngine::range_scan`'s own already-lazy,
bounded-memory `RangeScanIter` (`ADR-RE-002`'s persistent-cursor
design) — one row decoded per `next()` call, never a materialized `Vec`
of the whole table. This is the case item 10/44/70's repeated "never
materialize unbounded data" principle is most acute for (no predicate,
no index — the only bound is `LIMIT`, if any), so it is the one made
genuinely streaming this increment.

**`IndexScan`** (`Equality`/`Range`) calls the new `..._as_of` primitives
above, which are still **eager** — `IndexBuilder::scan_entries`'s own
internal shape (Increment 5, certified) collects into a `Vec` before
returning. Making it lazy too would mean reworking `scan_entries`'
internals, a materially larger change to already-certified Increment 5
code than this increment's own scope justifies without evidence a real
workload needs it (this project's consistent "correctness first,
optimize once measurement justifies it" convention). Instead,
`ExecLimits::max_index_scan_rows` (default 1,000,000) bounds the
`Vec`'s length directly — a controlled `ResourceLimit` error, never an
unbounded allocation, satisfying the *safety* requirement (item 70)
without claiming the *streaming* property this specific access path
does not have. Stated here explicitly, not discovered by a reader
later.

## §4. Result schema and row (items 7/8)

```rust
pub struct ResultField { pub name: String, pub ty: Option<RelationalType>, pub nullable: bool }
pub struct ResultSchema { pub fields: Vec<ResultField> }
pub type ResultRow = Vec<Option<RelationalValue>>;
pub struct QueryResult { pub schema: ResultSchema, pub rows: Vec<ResultRow> }
```

Typed throughout — never collapsed to `String`; every `RelationalValue`
variant (`Boolean`/`Integer`/`Bigint`/`Real`/`Double`/`Decimal`/`Text`/
`Blob`/`Date`/`Time`/`Timestamp`) and `NULL` (`None`) round-trips
exactly as the binder's own `BoundSelectItem`s describe it. `Result
Schema::from_projection` derives directly from the final `Projection`
node's own `BoundSelectItem` list (name, resolved type, nullability) —
never re-derived from raw storage. No internal identifier (`table_id`,
`index_id`, physical key, storage version, WAL sequence) is ever a
result column — a `ResultRow` only ever holds values `Projection`
itself computed via `eval` (§6), never a raw physical row passed
through.

## §5. Row context — the multi-table `ColumnRef` resolution mechanism (items 8/9/19/28/29)

```rust
pub struct RowContext { rows: Vec<(u32 /* table_ref */, Row)> }
pub struct Tuple { pub ctx: RowContext, pub projected: ResultRow }
```

Every operator passes a `Tuple` upward. `ctx` is the full multi-table
row state (needed by anything whose own `BoundExpr`s may reference a
column outside the final `SELECT` list — `Sort`'s `ORDER BY`, standard
SQL, item 27's own allowance, directly tested:
`sort_may_reference_a_column_outside_the_projection`); `projected` is
empty until a `Projection` operator fills it in, then simply carried,
unread, by `Distinct`/`Sort`/`Limit` above it until the top-level driver
extracts it as the caller-visible row. This shape — rather than
narrowing to the projected columns the instant `Projection` runs —
is what lets `Sort`/`Distinct` sit *above* `Projection` in the plan
tree (Increment 8's own, unmodified, logical-plan node order:
`Scan/Join → Filter → Projection → Distinct → Sort → Limit`) while
still being able to evaluate an `ORDER BY` expression the projection
itself does not carry, with zero planner changes.

**`LEFT JOIN`'s unmatched inner row** is represented as an *empty* `Row`
(`vec![]`), not a correctly-column-counted all-`NULL` row —
`RowContext::get`'s own out-of-bounds `Vec::get` already returns `None`
for any ordinal against an empty row, which is exactly SQL `NULL`, so
no operator needs a catalog lookup just to null-extend an unmatched
join side (item 29's "emit exactly one row with `NULL` values for the
inner side," achieved without touching `CatalogService` at all during
execution).

## §6. Expression evaluation and three-valued logic (items 16/17/18/62)

`sql/src/exec/expr_eval.rs` implements runtime evaluation for exactly
the `BoundExprKind` variants the binder currently produces — `Literal`,
`Parameter`, `Column`, `UnaryOp` (`Neg`/`Not`), `BinaryOp` (arithmetic,
comparison, `AND`/`OR`), `IsNull`, `Between`, `InList`, `Like`, `Case`,
`Function` (the four-entry `crate::functions::REGISTRY`: `length`,
`upper`, `lower`, `abs`) — no more, nothing guessed. An unbound-in-
practice `BoundExprKind` would be an internal-consistency error, not a
possible input (the binder guarantees the shape), so no `catch-all`
"unsupported expression" branch exists; every arm is total.

**Three-valued logic** (`Tri::{True,False,Unknown}`) is handled
directly for `AND`/`OR`/`NOT`, never through ordinary two-valued `bool`
— the standard SQL truth tables, tested exhaustively (`and_or_three_
valued_truth_tables`, `null_semantics_truth_table`: `NULL = NULL`,
`NULL <> NULL`, `NULL AND {TRUE,FALSE}`, `NULL OR {TRUE,FALSE}`, `NOT
NULL`, `IS [NOT] NULL`). `WHERE`'s own keep/discard rule (item 16) is
exactly `Tri::is_true()` — both `False` and `Unknown` discard a row,
the one place three-valued logic collapses to a binary decision, and
only there.

**No new implicit coercion** (item 18/22): every arithmetic/comparison
`BinaryOp` reaching this module already has *matching* operand types on
both sides — a direct, load-bearing consequence of the binder's own
`bind_shared` unification (Increment 6, and the exponential-time fix
Increment 8 found in it), which the executor simply trusts rather than
re-deriving. A mismatched pair reaching `value_cmp`/`arith` is treated
as an internal-consistency violation (a typed `ExecutionParameter`
error, never a panic, never a silent wrong answer), not a case to
handle gracefully — it cannot arise from any input the binder accepts.

**`DECIMAL` multiplication/division is deliberately not implemented** —
a stated, honest scope boundary, not an oversight: true fixed-point
decimal multiplication changes scale (a product of two scale-`s` values
is scale-`2s`), but `RelationalValue::Decimal` carries no rescale
primitive and the binder's own arithmetic-type derivation does not
adjust scale either. Computing a same-scale result would be numerically
**wrong**, not merely unimplemented, so it is refused with a controlled
error instead (`DECIMAL` `Add`/`Sub`, same scale, are exact and fully
implemented).

## §7. Operator-by-operator notes

- **`PkLookup`** (item 11): `Transaction::get_row` directly — already
  snapshot- and write-set-overlay-correct (D10, certified). A `NULL`
  key-value component short-circuits to zero rows *before* calling
  storage at all (item 62: `col = NULL` can never match; passing a
  `NULL` into a point-key lookup would be a type error, not merely a
  miss).
- **`IndexScan`** (items 12/13/14/15/64): `Equality`/`Range` per §3.
  A `NULL`-valued prefix/bound component also short-circuits to zero
  rows *before* calling `IndexBuilder` — critically, this is not
  optional: passing a `NULL` into `index_lookup_as_of`'s own
  `Option<RelationalValue>` prefix would search for physically-`NULL`-
  indexed rows (`IS NULL` semantics), which is a **different, wrong**
  answer for `=` semantics, not merely a missed optimization. The
  planner's own `residual` predicate (never dropped, item 15/64
  directly tested: `index_narrowed_residual_predicate_is_still_
  evaluated`) is evaluated against the merged outer+fetched-row context
  after every candidate fetch.
- **`Filter`** (item 16): `Tri::is_true()`, nothing else.
- **`Projection`** (items 19/20): evaluates the exact `SELECT`-list
  order the binder already resolved (wildcard expansion is the binder's
  own completed job, per Increment 6 — never repeated per row here).
- **`Distinct`** (items 21/22): dedups by `Vec<Option<RelationalValue>>`
  equality (the derived `PartialEq` `Option<RelationalValue>`/
  `RelationalValue` already provide — two `NULL`s compare equal for
  grouping purposes, the standard SQL `DISTINCT` rule, deliberately
  *different* from `WHERE`'s `NULL = NULL → UNKNOWN`). Implemented as a
  bounded linear-scan `seen: Vec<ResultRow>` — **measured, not
  optimized away**: `RelationalValue` contains `f32`/`f64`, which are
  not `Hash`/`Eq` in Rust, so a `HashSet` would need a hand-rolled
  bit-level float hash; given this increment's own "correctness first"
  convention and no evidence yet that `O(n²)` dedup cost is a real
  bottleneck (§9's own benchmark numbers), the simple, obviously-correct
  linear scan is used, bounded by `ExecLimits::max_materialized_rows` —
  a stated, honest, revisit-if-measurement-justifies-it tradeoff, not a
  hidden one.
- **`Sort`** (items 23/24): materializes into a bounded `Vec` (no
  external sort/spill exists — D17's own v1 decision, "bounded, reject
  when exceeded," reused verbatim), evaluates every sort key once up
  front (never re-evaluated per comparison), and compares with `NULLS
  FIRST`/`LAST` handled as an **absolute placement independent of
  `ASC`/`DESC`** — a real bug found and fixed during this increment's
  own test-writing (§10).
- **`Limit`/`Offset`** (items 25/26/52): `Limit` never pulls another
  upstream row once satisfied (directly measured, §9); `Offset` skips
  lazily, one pulled-and-discarded row at a time, never a
  materialization of the skipped prefix.
- **`Join`** (items 28–33): `NestedLoopJoinOp` implements both
  `NestedLoop` and `IndexNestedLoop` with **one** struct — the
  distinction is not a separate code path, only a difference in what
  the freshly-rebuilt-per-outer-row right-side `AccessOp` resolves its
  key against (§8). The full `ON` condition is *always* evaluated in
  full against the merged outer+inner context (item 28), regardless of
  what the right side's own access already narrowed — the same "an
  index is an access path, not proof a predicate is fully satisfied"
  principle (item 10) applied to join keys. `LEFT JOIN`'s unmatched-row
  emission (§5) happens exactly once per outer row, tracked by a plain
  `outer_had_match: bool` reset every time a new outer row is pulled.

## §8. `IndexNestedLoop` — a fresh inner operator per outer row (items 31/33)

`NestedLoopJoinOp` stores the right side as an **owned, cloned**
`PhysicalPlan` (never a borrowed reference — the simplest resolution to
a `'p`-vs-`ExecCtx<'a>` lifetime conflict that arose while implementing
this: a query plan is small, and cloning it once per `Join` operator
construction is negligible next to the I/O each outer row's inner
lookup performs). For **every** outer row, `build_operator(&self.
right_plan, &outer_tuple.ctx, ec)` is called fresh: if the right side is
a bare `Access` node, `AccessOp::build` resolves its `key_values`/
`mode` bounds against `outer_tuple.ctx` **at that moment** — a
`BoundExpr::Column` referencing the outer `TableRefId` (exactly the
representation `crate::bound` already uses for a correlated reference,
no new expression type invented) evaluates to the outer row's own
current value, and the resulting `PkLookup`/`IndexScan` performs a real,
fresh storage call. This is what item 31's "do NOT cache one outer
row's lookup result for unrelated outer rows" means literally, not
merely by intention: there is no cache to accidentally reuse, because
nothing is retained across outer rows at all. The identical rebuild-per-
row mechanism is what also makes plain `NestedLoop` a *real* `O(|outer|
× |inner|)` nested loop (item 33) rather than an accidental one-time
materialization — a non-correlated right side still gets rebuilt (and,
for `SeqScan`, re-scanned from the beginning) once per outer row.

## §9. Performance (item 48–56)

See `PHASE_RELATIONAL_QUERY_EXECUTOR_INCREMENT9_RESULTS.md` §7 for full
measured numbers. Headline: `PkLookup` costs ~8 µs end-to-end through
the full planned executor (vs. ~385 ns raw `engine.get`, ~5.2 µs
`TableStore::get_row` — overhead is transaction/plan-tree/row-decode
cost, never hidden); an indexed equality lookup at 1-in-10,000
selectivity is ~900× faster than the equivalent full scan; `LIMIT 10`
against a 50,000-row table is ~11× faster than the unlimited scan and,
independently, directly proven to touch a tiny fraction of the table's
rows via `ExecMetrics::rows_scanned` (not merely inferred from wall-
clock time); `IndexNestedLoop` is ~15× faster than plain `NestedLoop`
for a 200×200 selective join.

## §10. A real bug found and fixed: `NULLS LAST` under `DESC`

While writing `sort_ascending_and_descending_with_nulls_first_and_last`,
`ORDER BY name DESC NULLS LAST` produced `NULL` **first**, the opposite
of what was asked. Root cause: `compare_sort_keys` computed the
`NULL`-vs-value ordering using `item.nulls`, then unconditionally
applied `.reverse()` for `descending` to *every* comparison result,
including the ones that already fully encoded the requested `NULLS
FIRST`/`LAST` placement — reversing a `None`-involving comparison a
second time silently flips `NULLS LAST` into `NULLS FIRST` whenever
`DESC` is also present. **Fixed**: `.reverse()` now applies only to the
`Some`/`Some` (value-vs-value) comparison arm; the `None`-involving arms
already encode their final, absolute placement and are never reversed.
Both directions are now directly tested. This is the same "found by
writing a real, specific test — not by inspection" pattern this project
has hit in every increment so far (Increments 5/6/8's own documented
findings).

## §11. Transaction/snapshot integration (items 4/5/35/36/37)

`ExecCtx::txn: &Transaction` is the **only** read-consistency mechanism
this module has — no second, independently-invented query-snapshot
subsystem exists anywhere in `sql/src/exec/`. An explicit transaction's
own caller-supplied `&Transaction` flows straight through `execute`;
`execute_autocommit` begins a throwaway, read-only `Transaction` via
`TransactionManager::begin()` (item 5's own "autocommit execution
foundation," reusing D10's already-certified snapshot mechanism
verbatim) and commits it afterward (trivial, zero-`write_batch` for a
read-only write-set, already certified by Increment 7's own `commit()`
implementation). **Read-your-own-writes** (item 36) is exercised
directly (`read_your_own_writes_within_an_explicit_transaction`): a
`Transaction::put_row` call followed by an executor-driven `SELECT`
through the *same* transaction observes the local, uncommitted value —
entirely `Transaction::get_row`'s own already-certified overlay, with
no second write-buffer implemented in this crate (item 36's own "do not
implement a second write overlay inside the SQL executor. Use
Transaction" is satisfied by construction: there is nowhere else in
this module a write-set *could* live). **Snapshot consistency** (item
37) is directly tested for `PkLookup`, `SeqScan`, and `IndexScan` alike
(`snapshot_is_stable_against_a_later_external_commit`, `seq_scan_and_
index_scan_both_honor_the_transaction_snapshot`) — an external commit
made after a transaction's own `BEGIN` is invisible to every access
path that transaction's own queries use, not just point lookups.

## §12. Cancellation and deadline (items 40/41/71)

`CancellationToken` (a plain `Arc<AtomicBool>`) is item 40's own
"smallest correct internal mechanism required by the approved
architecture" — no cancellation/deadline primitive existed anywhere in
the repository to reuse (inspected: `rubixdb`/`rubixdb-sql` have none;
`rubixdb-api`'s own async runtime is a separate, unconnected concern,
§13). `ExecCtx::check()` tests both cancellation and a monotonic
(`Instant`, never wall-clock) deadline in one call, invoked at **every**
operator's own natural iteration point — the top-level driver loop,
every `Filter`/`Distinct`/`Sort`-materialization/`Join` inner loop, and
every `AccessOp::next` pull — not only once at the top level, because a
single `Filter` call that discards every row without yielding could
otherwise never return to a top-level check at all (item 40's own
"every potentially long operation should periodically check
cancellation," taken literally). A cancelled or deadline-exceeded query
releases everything it holds simply by returning `Err` up through
ordinary Rust `Drop` — no operator retains a resource (cursor, buffer,
transaction handle) beyond its own stack frame's lifetime, so there is
no separate cleanup path to get wrong.

## §13. Async/thread-safety boundary (item 72) — deferred, not solved

Inspected: `rubixdb-api` runs on a `tokio` multi-thread runtime, but
**no SQL HTTP endpoint exists yet** (`PHASE_RELATIONAL_QUERY_PLANNER_
ARCHITECTURE.md`/this increment both explicitly exclude it) — there is
currently no code path that calls this executor from an async context
at all, so there is no real async/blocking boundary to design around
yet, only a hypothetical future one. `sql/` itself has no `tokio`
dependency and none was added. Building a `spawn_blocking`-style
thread-pool boundary now, before the endpoint that would need it
exists, would be exactly the "unjustified complexity ahead of evidence"
this project's conventions consistently reject (D16/D17's own repeated
citation of the same reasoning). This is a deliberate deferral, stated
here explicitly: the future SQL HTTP API increment's own job is to
choose and wire that boundary against this executor's real, synchronous
(non-async) interface, exactly as this document's own "choose the
production-safe model based on the actual repository" instruction
implies once that repository actually contains an async caller.

## §14. Security (items 38/39/47/67/68/69/70)

**Physical-ID boundary** (item 67/68): the executor consumes only an
already-bound, already-planned `Plan` — every `table_id`/`index_id`/
`ColumnRef` it touches originates from `crate::plan`'s own output,
itself sourced entirely from `crate::bind`'s authorized resolution
(D25). There is no code path in `sql/src/exec/` that accepts a raw
physical identifier from a caller, re-resolves a name through the
catalog, or otherwise provides a bypass around binder-level
authorization — the same boundary Increment 8's planner already
established, unduplicated and unbypassed here.

**Corruption fail-closed** (item 39/69): every storage call
(`Transaction::get_row`, `TableStore::scan_table_rows_as_of`,
`IndexBuilder::index_lookup_as_of`/`index_range_scan_as_of`) returns
`rubixdb::relational::Result`, propagated via `?` through `SqlError`'s
new `From<RelationalError>` impl (`sql/src/error.rs`) — a corruption or
I/O error from the certified Read Engine's own fail-closed contract
stops the query immediately; no operator catches and silently continues
past a storage error to return a partial result.

**Error taxonomy** (item 38/47): `SqlError` gained `Storage` (a lower-
layer error's own already-safe `Display` text, never a raw path/key),
`UnsupportedExecution`, `ExecutionParameter`, `Cancelled`,
`DeadlineExceeded`, `Conflict` (reserved — unreachable from any code
path this read-only increment executes, since only `Transaction::
commit()` with a non-empty write-set can produce a snapshot conflict,
and nothing here calls it with one). No variant carries a filesystem
path, physical key, credential, or parameter *value* (only the fact
that a parameter reference was invalid).

**Resource-exhaustion security** (item 70): `ExecLimits` bounds result-
row count, `Sort`/`Distinct` materialization, and `IndexScan` row
count — every check happens *before* the corresponding `Vec`/`HashMap`-
equivalent growth, not after (`max_result_rows` checked before pushing
each row past the cap; `Distinct`/`Sort` check before inserting past
their own cap). No `unsafe` anywhere in `sql/src/exec/`. No `.unwrap()`/
`.expect()` on caller-controlled or storage-derived input — every
fallible path returns a typed `Result`.

---

## Testing summary

36 new tests in `sql/src/exec_tests.rs`: `PkLookup`/`SeqScan`/
`IndexScan` correctness and residual-filter preservation, three-valued-
logic truth tables, projection ordering/wildcard, `DISTINCT`
(including `NULL` grouping), `Sort` in all four `ASC`/`DESC`×`NULLS
FIRST`/`LAST` combinations plus a column outside the projection,
`LIMIT`/`OFFSET` (including a direct `rows_scanned`-metric proof of
early termination), `INNER`/`LEFT JOIN` multiplicity in every named
shape (zero/one/many matches, `WHERE` on the nullable side),
`IndexNestedLoop`, parameters (including `NULL` and missing-parameter
error handling), read-your-own-writes and snapshot consistency across
all three access paths, cancellation, deadline, three resource-limit
boundaries, the plan/executor contract (`Unsupported Execution` for
non-`Query` plans, `EXPLAIN` still executing its inner statement),
metrics, race-free concurrent independent queries, real automatic-
compaction interaction, and a differential test against an independent,
from-scratch in-memory reference model. 4 new regression tests in the
core `rubixdb` crate for §2's new snapshotted primitives.
