# Phase: Relational Database — Aggregation Architecture (Increment 11)

## Scope

`GROUP BY`, `HAVING`, and the aggregate functions `COUNT`, `SUM`, `AVG`,
`MIN`, `MAX`, threaded through the full existing pipeline: `SQL → parser
→ internal AST → binder → logical plan → rule optimizer → physical plan
→ executor → transaction/read context → real relational storage`. No
second query engine, no client-side/CLI-side/frontend aggregation, no
fake results.

Explicitly **out of scope** (unchanged from before this increment, not
newly rejected): subqueries, CTEs, `UNION`/`INTERSECT`/`EXCEPT`, window
functions, `ROLLUP`/`CUBE`/`GROUPING SETS`, `COUNT(DISTINCT …)`,
parallel/distributed aggregation, materialized views, query cache, CLI,
HTTP SQL API, frontend SQL console.

## 1. What already existed when this increment started

A prior partial pass had already added, uncommitted:

- `sql/src/aggregate.rs`: `AggregateFunc`, `AggregateArg<T>`,
  `AggregateContract` (argument-type/return-type rules), `AggregateState`
  (per-function accumulator with `update`/`finalize`/`merge`), and
  `GroupingValue`/`GroupingKey` (a canonical, collision-free, hashable
  grouping-key representation).
- `sql/src/ast.rs`: `Expr::Aggregate { func, arg }`, `Select::group_by`/
  `having`.
- `sql/src/bound.rs`: `BoundExprKind::Aggregate(BoundAggregateExpr)` and
  `BoundExprKind::AggregateRef(usize)`, `BoundSelect::group_by`/`having`.
- `sql/src/bind/expr.rs`: `ExprBinder::allow_aggregate`/`with_aggregate`,
  `bind_aggregate` (type-checks an aggregate call against
  `AggregateContract`, rejects nested aggregates).
- `sql/src/convert.rs`: real `GROUP BY`/`HAVING`/aggregate-function-call
  AST conversion from `sqlparser`'s tree (`GROUP BY ALL`/`ROLLUP`/`CUBE`/
  `GROUPING SETS`/`COUNT(DISTINCT …)` still explicitly `Unsupported`).

This was inspected in full before writing anything (the governing
directive's own "no guessing" rule) — one real bug was found and fixed
(`AggregateState::merge`'s `Max` arm wrote through an unbound `max`
identifier instead of the matched `m1` binding — a compile error, so it
had never actually run). The crate did not compile at all at the start
of this increment (`BoundExprKind::Aggregate`/`AggregateRef` were added
to the enum but ~10 exhaustive `match` sites across `sql/src/exec/
expr_eval.rs`, `plan/explain.rs`, `plan/expr_util.rs`, `plan/validate.rs`
did not yet have arms for them, and `rubixdb::relational::{validate_
decimal, MAX_DECIMAL_PRECISION}` were referenced but not re-exported from
the core crate).

What did **not** yet exist, and is this increment's own work: binding
`GROUP BY` itself, select-list group-compatibility validation, aggregate
extraction into a shared index space, the `Aggregate` logical/physical
plan node, the `AggregateOp` executor, resource limits for aggregation
state, aggregate-specific metrics, and all testing/benchmarking/
documentation.

## 2. Design decision: aggregate calls are extracted to a positional index, not left inline

`BoundSelect` gained a new field, `aggregates: Vec<BoundAggregateExpr>`.
Binding a `SELECT`/`HAVING`/`ORDER BY` expression tree first produces
`BoundExprKind::Aggregate(...)` nodes wherever an aggregate call
appeared (exactly as the prior partial pass already did); `crate::bind::
select::extract_aggregates` then walks the fully-bound projection/
`HAVING`/`ORDER BY` trees once, replacing every `Aggregate(agg)` node
with `AggregateRef(idx)`, where `idx` is `agg`'s position in the shared
`aggregates` list (deduplicated by `BoundAggregateExpr`'s own
`PartialEq` — `SELECT SUM(x), SUM(x) + 1` shares one aggregate slot, one
physical accumulator, computed once).

This is the same design the prior partial pass's own doc comment on
`BoundExprKind::AggregateRef` already anticipated ("a reference to the
output of an Aggregate plan operator"), completed here. It keeps
`BoundExpr` itself simple (no runtime state, item 25's own requirement)
and gives the executor a fixed, statically-known output shape for the
`Aggregate` node: exactly `aggregates.len()` values per group, in a
fixed order, addressed by index, never by name/re-resolution.

## 3. Design decision: `HAVING` is an ordinary `Filter` directly above `Aggregate`

No new plan node, no second boolean-logic model. `build_logical_plan`
now does:

```
Filter(WHERE)?
  → Aggregate { group_by, aggregates }   [only if is_aggregated]
    → Filter(HAVING)?                    [only if HAVING was given]
      → Projection
        → Distinct? → Sort? → Limit?
```

`is_aggregated` is `!group_by.is_empty() || having.is_some() ||
!aggregates.is_empty()` — computed independently, identically, in both
`crate::bind::select` (to decide whether select-list validation applies)
and `crate::plan::logical::build_logical_plan` (to decide whether to
emit the `Aggregate` node), rather than threading a redundant flag
through `BoundSelect` that the two could disagree about.

Reusing `Filter` for `HAVING` means it automatically inherits
`eval_predicate`'s existing three-valued (`Tri`) logic — `TRUE` keeps a
group, `FALSE`/`UNKNOWN` both discard it — with zero new code (items
20/21). It also makes "`HAVING` evaluates once per group, never once per
input row" a *structural* fact rather than something a counter has to
separately prove: the `HAVING` `Filter`'s only possible input is
`AggregateOp`, whose `next()` yields exactly one `Tuple` per group,
already fully aggregated. `sql/src/exec_tests.rs::having_evaluates_once_
per_group_not_once_per_input_row` demonstrates this with real metrics
(300 input rows, 2 groups, 1 discarded by `HAVING`) rather than merely
asserting it holds by construction.

The optimizer's predicate-pushdown pass (`crate::plan::optimize::
pushdown_predicates`) gained one new `LogicalPlan::Aggregate` arm that
only recurses into the *input* side — it never attempts to push a
conjunct *through* `Aggregate` in either direction, matching item 26
literally ("do not push WHERE/HAVING/LIMIT/projection through
Aggregate unless the transformation is proven semantics-preserving" —
no such proof was attempted, so no such rule was written). This is
free: `try_push_into_scan`'s own match only handles `Scan`/`Join`
nodes, so a `HAVING` conjunct that happens to reference a plain grouped
column (and would therefore parse as a single-`table_ref` predicate)
still cannot be pushed below `Aggregate`, because the node immediately
below the `HAVING` `Filter` is `Aggregate`, not a `Scan`/`Join` —
`try_push_into_scan` falls through to its `_ => false` arm.

## 4. Design decision: representative-row execution model

`AggregateOp` (`sql/src/exec/operators.rs`) is a blocking operator (the
same shape as the existing `Sort`/`Distinct` operators — correctness
requires seeing every input row before any group's state is final).
Per group it retains exactly two things:

1. **One `RowContext`** — the *first* input row encountered for that
   group, unmodified. Item 12's own "streaming... discard the input row
   once it has contributed to aggregate state" holds for every row
   *after* the first: only the first row per group is retained at all;
   every subsequent row for that group is discarded immediately after
   `AggregateState::update` consumes it.
2. **A `Vec<AggregateState>`** — one accumulator per entry in
   `aggregates`, updated per input row.

On emitting a group's result tuple, `AggregateOp` builds a `RowContext`
that is the representative row's own bindings (`RowContext::merged`'s
existing shape, unchanged) *plus* a new `aggregates: Vec<Option<
RelationalValue>>` field holding each accumulator's finalized value.
`crate::exec::expr_eval::eval` gained one new arm: `BoundExprKind::
AggregateRef(idx) => ctx.get_aggregate(idx)`.

**Why this is correct.** `crate::bind::select::validate_group_compat`
(§5 below) guarantees that every non-aggregate leaf of a projection/
`HAVING`/`ORDER BY` expression either is inside an aggregate call, or
is part of a subtree that exactly (structurally) equals one of
`group_by`'s own bound expressions. Since `GroupingKey` equality is
exactly what defines group membership, re-evaluating that same
`group_by` expression against *any* member row of the group — not
just the specific row that produced the key — reproduces the identical
value. The representative row therefore correctly answers every
legal (group-compatible) expression above `Aggregate`, without storing
more than one row per group.

**Known, deliberate, documented edge case**: `GroupingKey`'s own
canonicalization (`-0.0`/`+0.0` to one bit pattern, every `NaN` bit
pattern to one canonical `NaN`, item 14) means two rows whose raw
`REAL`/`DOUBLE` grouping value differs only in that canonicalized way
(e.g. one row's `grp = -0.0`, another's `grp = 0.0`) are the *same*
group, but the representative row shown for a plain `SELECT grp, ...`
reflects whichever row was encountered first (deterministic given a
fixed input scan order, but not independently meaningful — no SQL
engine defines "which -0.0/NaN wins" either). This is the identical
choice every engine with this feature makes; it is named here rather
than silently accepted.

## 5. Design decision: select-list/`HAVING`/`ORDER BY` group-compatibility validation

`crate::bind::select::validate_group_compat(expr, group_by)` implements
item 17's core correctness rule with one recursive rule: an expression
is legal iff it exactly matches a `group_by` expression (stop
recursing — the whole subtree is constant within a group), or it is
(or is inside) an aggregate call (never checked against `group_by` —
its argument is evaluated per input row, *before* grouping, a different
evaluation phase entirely), or it is a literal/parameter (always
constant), or — for every other node shape (`BinaryOp`, `Case`, `IN`,
`BETWEEN`, `LIKE`, scalar function calls, ...) — every child
subexpression independently passes the same check. A bare `Column`
reaching a leaf without having matched anything above it is rejected
with a `TypeMismatch`.

This runs whenever `is_aggregated` is `true`, over every projection
item, `HAVING` (if present), and every `ORDER BY` item (once bound with
`allow_aggregate` set to `is_aggregated`, so an aggregate call in
`ORDER BY` is legal exactly when the statement is already aggregated —
item 22).

`WHERE` never runs this check and never allows aggregates in the first
place — `crate::bind::select::bind_select` binds `selection` with a
plain `ExprBinder::new(...)` that never calls `.with_aggregate(true)`,
so any `Expr::Aggregate` there hits `bind_aggregate`'s own
`allow_aggregate` guard and fails with `TypeMismatch` *before*
`validate_group_compat` is even relevant (item 19).

## 6. Grouping key: `GroupingValue`/`GroupingKey` (inherited, verified)

Already present from the prior partial pass, verified correct by
inspection and by `aggregate.rs`'s own unit tests plus
`aggregate_reference_model.rs`'s differential tests:

- One enum variant per `RelationalType` (`Null`, `Boolean`, `Integer`,
  `Bigint`, `Real(u32 bits)`, `Double(u64 bits)`, `Decimal(i128, u8)`,
  `Text(String)`, `Blob(Vec<u8>)`, `Date`, `Time`, `Timestamp`) — no
  lossy debug-string concatenation, so a composite key `(1, 23)` can
  never collide with `(12, 3)` (item 13): each field is its own typed
  enum variant, not a substring of a joined string.
- `NULL` groups with `NULL` (a dedicated `GroupingValue::Null` variant,
  matched by `derive(PartialEq, Eq, Hash)` like any other value) — the
  opposite of `WHERE`'s three-valued `NULL = NULL → UNKNOWN`, and
  deliberately never reuses `expr_eval::values_eq`/`compare` for this
  reason (item 15).
- `REAL`/`DOUBLE` canonicalize `-0.0`/`+0.0` to one bit pattern and every
  `NaN` payload to one canonical `NaN` before hashing (item 14) — this
  is the one place in the executor that intentionally diverges from
  `RelationalValue`'s own bitwise `PartialEq` (which would treat
  `-0.0 != +0.0`'s IEEE-754 bit pattern difference as distinct groups,
  and every distinct `NaN` payload as its own group — neither is useful
  `GROUP BY` behavior, and no other value-equality site in this crate is
  affected: `expr_eval::value_cmp` still rejects `NaN` comparisons
  entirely, unchanged).

## 7. Aggregate function contracts (`AggregateContract`, `AggregateState`)

Centralized in `sql/src/aggregate.rs`, not scattered across the binder/
executor (item 6). One contract per function:

| Function | Argument | Return type | Empty/all-NULL input | `NaN`/overflow |
|---|---|---|---|---|
| `COUNT(*)` | none (wildcard) | `BIGINT`, never `NULL` | `0` | `checked_add`, `ResourceLimit` on `u64` overflow |
| `COUNT(expr)` | any type | `BIGINT`, never `NULL` | `0` | same |
| `SUM(expr)` | numeric | `INTEGER/BIGINT→BIGINT`, `REAL→REAL`, `DOUBLE→DOUBLE`, `DECIMAL→DECIMAL` (same scale) | `NULL` | `checked_add`; `ExecutionParameter` (a controlled error) on overflow, never silent wraparound |
| `AVG(expr)` | `INTEGER`/`BIGINT`/`DOUBLE→DOUBLE`, `REAL→REAL` | `NULL` on 0 qualifying rows | float division only after the full sum/count are known (never an average-of-averages, item 9) |
| `MIN`/`MAX(expr)` | any orderable type, not `*` | same type as input | `NULL` on 0 qualifying rows | `NaN` comparison is a controlled `ExecutionParameter` error, never an arbitrary ordering |

`COUNT(*)` vs. `COUNT(expr)` are distinguished at every layer —
`AggregateArg::Wildcard` vs. `::Expr` in the AST/bound representation,
`AggregateState::CountStar` vs. `::CountExpr` at runtime (item 7):
`COUNT(*)` counts every row reaching the group (even an all-`NULL` one);
`COUNT(expr)` counts only rows where `expr` evaluated non-`NULL`.

**`AVG(DECIMAL)` is a controlled, documented `Unsupported` error**
(never attempted): `RelationalValue::Decimal(i128, u8)` carries no
rescale primitive, and computing `sum / count` at the same scale as the
input would silently truncate fractional precision the true average
requires — the exact same "would be numerically wrong, not merely
unimplemented" reasoning `crate::exec::expr_eval::arith`'s own `DECIMAL`
multiplication/division refusal already documents for the general
arithmetic case (item 42). `SUM(DECIMAL)` *is* supported (addition at a
fixed scale is exact) and validates its result against `rubixdb::
relational::validate_decimal(_, MAX_DECIMAL_PRECISION, scale)` on every
update, so a `SUM` that would overflow the column's declared precision
fails closed with a controlled error rather than silently wrapping.

## 8. Resource limits (item 30/73/74)

Two new `ExecLimits` fields, checked *before* the corresponding growth,
mirroring `Distinct`'s own existing `max_materialized_rows` check
exactly:

- `max_group_count` (default `1_000_000`, matching `max_materialized_
  rows`'s own default and reasoning — "most distinct keys one query's
  seen-set may hold" is the identical resource shape `Distinct` already
  bounds). Checked before a *new* group is inserted.
- `max_aggregate_state_bytes` (default `256 MiB`) — a group-count limit
  alone does not bound a `TEXT`/`BLOB` grouping key or a `MIN`/`MAX`
  accumulator holding a large value; this is the independent byte-level
  bound item 30 calls for when count alone is insufficient. Tracked
  incrementally (`GroupingKey::estimated_bytes()` on group creation,
  `AggregateState::estimated_bytes()` delta on every `update`), checked
  after each row's contribution.

Both failures are `SqlError::ResourceLimit`, both increment the new
`ExecMetrics::aggregate_resource_limit_hits` counter, and both fail
*closed* — no group's state already computed is ever returned as a
partial result (item 66); the whole query errors.

## 9. Metrics (item 71/72)

`ExecMetrics` gained `groups_created` (once per newly-inserted group,
never per input row), `groups_emitted` (final group count, a separate
counter from `groups_created` rather than derived from it, so a
divergence between the two would itself be observable evidence of a
bug), `aggregate_rows_processed` (every input row `AggregateOp`
consumed), and `aggregate_resource_limit_hits`. `PlannerMetrics` gained
`aggregate_plans` (incremented once per `Aggregate` physical node
built). All bounded-cardinality counters — no table/column/SQL-text/
group-value ever becomes a label, unchanged discipline from every
existing metric in this crate.

A dedicated `having_evaluations` counter was considered and deliberately
**not** added: wiring it through `FilterOp` would have required adding
an `is_having` flag to both `LogicalPlan::Filter` and `PhysicalPlan::
Filter` and touching roughly fifteen match sites across the planner/
executor/`EXPLAIN` purely for one metric's sake, when `groups_emitted`
already is, by construction (§3 above), the exact count of `HAVING`
evaluations for any aggregated query with a `HAVING` clause.

## 10. Plan determinism (item 61)

`AggregateOp::materialize` never iterates a `HashMap` to produce output
order. A `HashMap<GroupingKey, usize>` is used only to answer "have I
seen this key" in `O(1)`; the actual emitted order is the order groups
were first inserted into a plain `Vec`, which is itself a deterministic
function of input scan order (already deterministic, given a fixed
storage state, per the certified read engine). Two identical `BoundSelect`/
catalog-state/optimizer-configuration inputs therefore always produce
the same aggregate plan and the same result-row order — verified by
`sql/src/aggregate_reference_model.rs`'s differential tests, which
compare an `ORDER BY`-stabilized query against an independent reference
model rather than depending on emission order at all (the safer,
`ORDER BY`-explicit test design — emission order itself is an
implementation detail no SQL standard requires without `ORDER BY`, so
no test asserts a *specific* unordered emission order, only that it is
stable/reproducible in principle via the `Vec`-not-`HashMap`-iteration
construction described above).

## 11. Security (item 48/75)

Every table/column/function `AggregateOp` ever touches comes from the
already-bound, already-authorized `BoundSelect`/`BoundAggregateExpr` —
no new catalog resolution happens at execution time, no physical ID is
ever accepted from a caller. `sql/src/aggregate.rs` contains zero
`unwrap`/`expect`/`panic!`/`unsafe` in its production code (verified by
inspection: no such call exists anywhere before its `#[cfg(test)]`
module). The `.expect(...)` calls added to `exec/operators.rs`
(`AggregateOp::next`) and `exec/expr_eval.rs` (none needed there — the
new `AggregateRef` arm is a plain, infallible `Vec::get`) match this
crate's own pre-existing, already-certified idiom exactly: `SortOp::
next`'s identical `self.buffer.as_mut().expect("materialized above")`
one function below it in the same file is the precedent. No new
dependency was added (item 76) — every new type/function lives in this
crate, over the existing `rubixdb`/`sqlparser` dependency set.

## 12. Known limitations (explicit, not silently accepted)

- **Aggregation over a `SeqScan`/`IndexScan` input does not exhibit
  read-your-own-writes.** This is not new — `AccessOp::build`'s
  `SeqScan`/`IndexScan` arms have always read via `TableStore::
  scan_table_rows_as_of(snapshot_seq)` directly, never through
  `Transaction::get_row`'s write-set-overlay path (only `PkLookup`
  does). `AggregateOp` simply consumes whatever rows its input operator
  produces, so it inherits this pre-existing, architecture-documented
  (`ExecCtx`'s own doc comment) scope boundary unchanged. Proven, not
  merely asserted, by `sql/src/exec_tests.rs::aggregate_query_honors_
  transaction_snapshot`, which separately verifies external-commit
  snapshot stability (which does hold) and `PkLookup`-shaped
  read-your-own-writes (which does hold), while explicitly not
  asserting `SeqScan`-shaped read-your-own-writes (which does not hold,
  for any query, aggregated or not).
- **No parallel/vectorized aggregation** — one thread, one pass,
  `AggregateState::merge` exists (for a future partitioned-aggregation
  increment) but is never called by anything in this increment.
- **No hash-aggregation spill to disk** — `max_group_count`/
  `max_aggregate_state_bytes` bound memory by failing closed instead,
  matching `Sort`/`Distinct`'s own existing "bounded, reject when
  exceeded" choice (no external sort/spill primitive exists anywhere in
  this executor).
- Explicitly still unsupported (never claimed): `GROUP BY ALL`,
  `ROLLUP`/`CUBE`/`GROUPING SETS`, `COUNT(DISTINCT …)`, window functions,
  subqueries, CTEs, set operators — every one verified rejected by a
  test in `sql/src/bind_tests.rs` (`group_by_all_and_rollup_remain_
  explicitly_unsupported`, `count_distinct_remains_explicitly_
  unsupported`), never merely assumed.
