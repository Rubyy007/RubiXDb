# PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE

**Scope**: this document is the decision record for Increment 8 — a
production-grade, **rule-based** query planner and optimizer,
implementing D16/D17/D18 (`PHASE_RELATIONAL_DATABASE_ADR.md`) exactly
as already approved. `SQL → Parser → AST → Binder → BoundStatement →
LogicalPlan → (rule-based optimization) → PhysicalPlan`. **No executor
exists** — `Plan`/`PhysicalPlan` are data structures a future executor
consumes; nothing in this crate executes a query. `RELATIONAL DATABASE
PRODUCTION READY = NO` after this increment.

**Where the code lives**: `sql/src/plan/` (a new module inside the
already-established `rubixdb-sql` crate, per `PHASE_RELATIONAL_
DATABASE_ARCHITECTURE.md` §15's own "one crate owns parser/binder/
planner/executor" design — never a new workspace crate).

---

## §1. Plan representation (item 3)

Two structured enums, neither embedding raw SQL text or a third-party
parser AST node:

- **`LogicalPlan`** (`sql/src/plan/logical.rs`) — relational *intent*:
  `EmptyRelation`, `Scan`, `Join`, `Filter`, `Projection`, `Distinct`,
  `Sort`, `Limit`.
- **`PhysicalPlan`** (`sql/src/plan/physical.rs`) — chosen *access
  operators*: the same shape, with `Scan` replaced by `Access
  (PhysicalAccess)` — `PkLookup` / `IndexScan` / `SeqScan`
  (`sql/src/plan/access.rs`) — and `Join` additionally carrying a
  `JoinAlgorithm` (`NestedLoop` / `IndexNestedLoop`).
- **`Plan`** (`sql/src/plan/mod.rs`) — the top-level output for any
  `BoundStatement`: `Query { physical, max_parameter }` for `SELECT`;
  `Insert`/`Ddl`/`Begin`/`Commit`/`Rollback` as structural pass-throughs
  (item 47 — no execution nodes fabricated for statement kinds this
  increment does not plan); `Update`/`Delete` carrying the same
  `PhysicalAccess` a `SELECT` scan would use for their target row set;
  `Explain(Box<Plan>)`.

`Aggregate`/`GroupBy` are **deliberately not represented**, despite
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §11 listing `Aggregate`
among the v1 logical operators: `GROUP BY`/`HAVING`/aggregate functions
are still rejected at bind time (verified this increment, unchanged
since Increment 6 — `sql/src/convert.rs`'s `GroupByExpr::Expressions`
arm still returns `Unsupported`), so no `BoundSelect` this crate can
ever produce contains aggregation. Item 48's own instruction ("leave
them unsupported... do not expand scope simply because the planner
could theoretically represent them") is followed literally — adding
dead variants no code path can construct would itself be the
unrequested scope expansion the instruction forbids.

## §2. Logical/physical separation (items 4/5)

`build_logical_plan` (from `BoundSelect`) never chooses a storage index
— every `Scan` node starts with `predicate: None`, `required_columns:
None`. `build_physical_plan` runs only after the optimizer
(`crate::plan::optimize`) has annotated the logical tree, and is the
**only** place an access path is chosen. Every `PhysicalAccess` variant
maps to a real, already-implemented storage primitive — never an
operator with no primitive behind it:

| `PhysicalAccess` | Storage primitive |
|---|---|
| `PkLookup` | `TableStore::get_row` / `Transaction::get_row` |
| `IndexScan` (`Equality`) | `IndexBuilder::index_lookup` |
| `IndexScan` (`Range`) | `IndexBuilder::index_range_scan` |
| `SeqScan` | `TableStore::scan_table` |

**`IndexKind::Primary` is never selected as an `IndexScan`** — a real,
inspected fact, not an arbitrary rule: `CatalogService::create_table`
auto-creates a `Primary`-kind `IndexRow` purely as a catalog/naming
record (`{name}_pkey`), but `TableStore`'s own `maintained_indexes`
helper explicitly filters `IndexKind::Primary` out of every row's
index-maintenance set (`src/relational/table_store.rs`, `i.kind !=
IndexKind::Primary`) — **no physical entries are ever written for it**.
Selecting it as an `IndexScan` would silently return zero rows for
every lookup; `access::plan_table_access` filters it out explicitly and
routes `PRIMARY KEY` access through the dedicated `PkLookup` variant
instead. Directly tested (`primary_kind_catalog_index_is_never_
selected_as_an_index_scan`).

## §3. `UPDATE`/`DELETE` target-row planning — one algorithm, reused

`Plan::Update`/`Plan::Delete` call `access::plan_table_access` with the
statement's own `selection`, **the exact same function** a `SELECT`
scan's physical planning calls — one implementation, never a duplicated
copy with a chance to disagree (this project's own established
convention, `rubixdb::relational::table_store`'s shared `pub(crate)`
helpers being the storage-layer precedent for the same reasoning).
`UPDATE`/`DELETE` have no `JOIN` in this grammar (D18), so the single-
table call is always unambiguous — no `null_extended` concern applies.

## §4. `PRIMARY KEY` lookup detection (items 6/7)

`access::plan_table_access` decomposes the predicate into top-level
`AND` conjuncts (`expr_util::conjuncts`) and builds an `ordinal →
equality-conjunct` map. A `PkLookup` is chosen **only** when *every*
`pk_ordinals` position has its own equality conjunct — `table.pk_
ordinals.iter().all(|ord| equality_by_ordinal.contains_key(ord))`.
For `PRIMARY KEY(a, b)`, `WHERE a = 1` alone (`b` unconstrained) fails
this check and correctly falls through to secondary-index-or-`SeqScan`
planning — directly, adversarially tested (`partial_composite_pk_
equality_never_becomes_a_point_lookup`, both the "must not" and "must,
once complete" cases). Consumed conjuncts are removed from the
predicate; everything else survives as `residual` (item 10).

## §5. Secondary index selection and range planning (items 8/9/10/11)

For each `Ready`, non-`Primary` index (`CatalogService::list_indexes`,
already a deterministic, ascending-`index_id`, engine-`range_scan`-
backed ordering — never a `HashMap`), `access::candidate_index_access`
walks the index's own declared `column_ordinals` **in order** (item 8:
"composite-index selection must respect declared column order"):

1. Consume a maximal, **contiguous-from-the-start** run of equality
   conjuncts as the index's leading columns.
2. If that run covers every column, the access is `IndexAccessMode::
   Equality { prefix }` (→ `IndexBuilder::index_lookup`).
3. Otherwise, if the *next* column (immediately after the equality
   prefix) has a `<`/`<=`/`>`/`>=` conjunct, fold it into an
   `IndexAccessMode::Range { start, end }` bound (→ `IndexBuilder::
   index_range_scan`) — `Included`/`Excluded` exactly matching the
   operator, `Unbounded` on whichever side has no comparison conjunct
   (item 9's exact inclusive/exclusive/unbounded contract).
4. An index with **no** usable leading-column predicate at all is
   skipped entirely (never a partial/unsafe plan, item 40/41).

Among multiple usable indexes, the one consuming the **most**
conjuncts wins; ties keep the first-seen (lowest `index_id`, since
`list_indexes`'s own ordering is already deterministic) — no
`HashMap`-iteration-order dependence anywhere in the tie-break (item
53). This is a purely **structural** rule (more of the given predicate
consumed), never a selectivity/cost guess — D16's own "no invented cost
constants" is satisfied by construction, not by omission.

**The index never eats more than it can prove** (item 10): any conjunct
not consumed by the chosen access — an equality on a non-indexed
column, a comparison on a column beyond the index's usable prefix —
survives as `residual`, always re-applied. `index_narrows_but_
residual_predicate_is_preserved` tests this directly.

## §6. Predicate pushdown and `LEFT JOIN` safety (items 12/20/21)

`crate::plan::optimize::pushdown_predicates` decomposes the top `WHERE`
`Filter`'s predicate into conjuncts and, for each one referencing
**exactly one** table, attempts to merge it into that table's own
`Scan.predicate` — **but only when that table's own `BoundTableRef::
null_extended` is `false`** (`try_push_into_scan`). `null_extended` is
already set correctly by `crate::bind` for the introduced (right-hand)
side of a `LEFT JOIN` — D18's binder pass, unmodified, simply consumed
here. A conjunct referencing 0 or 2+ tables, or the null-extended table,
is never pushed and survives in a `Filter` node sitting **above** the
`Join` — directly, adversarially tested (`left_join_predicate_on_
nullable_side_is_never_pushed_into_its_scan`): `WHERE orders.amount =
100` on a `LEFT JOIN`'s right side stays a top-level `Filter`, never
migrates into `orders`' own scan (which would silently turn "no
match, `amount` compared against `NULL`, row excluded" into "`amount`
pre-filtered before the join even runs, row still appears null-
extended" — a real semantic change this project's own adversarial test
suite now guards against, not merely asserted safe by inspection).

**No rule here ever rewrites `BoundExpr` logic** — `expr_util`'s own
module doc comment states this as the load-bearing reason three-valued
logic (item 21) is preserved automatically: nothing decomposes or
rebuilds `AND`/`OR`/`NOT`/`IS NULL` algebra, only *where* an
unmodified subtree is evaluated changes. `is_null_predicate_is_never_
treated_as_an_equality_lookup_key` directly guards the one place this
could have gone wrong: `as_column_equality` only matches `BinaryOp::Eq`
— `col IS NULL` (`BoundExprKind::IsNull`) can never be mistaken for
`col = NULL`.

**Join-key pushdown is a separate, always-safe mechanism** (§8) — the
`ON` condition's own equality conjuncts against the *inner* table may
still be combined into that table's access even under a `LEFT JOIN`,
because narrowing how matching candidates are *found* does not change
what happens when none are found (the executor's null-extension is
unaffected either way) — a different operation from WHERE-pushdown, not
an exception to §6's own rule.

## §7. Projection pruning (item 13) — honestly metadata-only

`crate::plan::optimize::prune_projections` computes, for every table,
the union of column ordinals referenced by the final projection, every
`Sort`/`Join.on` expression, and every `Filter`/pushed-`Scan`-predicate
still standing, and annotates each `Scan.required_columns` with the
result. **This does not currently reduce physical decode cost** — a
real, inspected limitation, stated honestly rather than assumed away:
`TableStore::get_row`/`scan_table` have no partial-column-decode
primitive (`decode_full_row` always decodes/returns the whole `Row`),
so there is no real storage operator this pruning could attach to yet
(item 5's own "do not produce a physical operator that has no actual
storage primitive behind it" is satisfied by *not* inventing one —
pruning stays a plan-metadata annotation, useful to a future executor
for building a narrower output tuple and to `EXPLAIN` for visibility,
not claimed as an I/O optimization it is not).

## §8. `LIMIT`/`OFFSET` pushdown (item 14) — conservative, literal to spec

`PhysicalPlan::Limit` carries a `pushable: bool`, set by `crate::plan::
optimize::limit_pushable`: `true` **only** when the chain from `Limit`
down through zero-or-more `Filter`/`Projection` wrappers reaches a bare
`Scan` directly — item 14's own only given example ("a simple ordered
index scan"). `JOIN`/`SORT`/`DISTINCT` are named explicitly in item 14
as default blockers; this implementation follows that list literally
rather than reasoning about whether a specific join shape might
actually be safe (a more aggressive rule was considered and rejected —
see §12). No node is *moved*: the pull-based executor model
(architecture doc §11: "stop iterating once `LIMIT` is satisfied —
falls out naturally") already makes early termination automatic once an
executor exists; `pushable` is metadata a future executor/`EXPLAIN`
consumes, not a physical rewrite.

## §9. `ORDER BY` / `Sort` elimination (item 15) — conservative, exact-match

`physical::order_satisfied_by_access` eliminates `Sort` only in checked,
exact-match cases, never by approximate reasoning:

- **`PkLookup`**: always eliminable — at most one row, any order is
  trivially already satisfied.
- **`IndexScan`**: eliminable only when every `ORDER BY` item is a
  plain ascending column reference, **`NULLS FIRST`**, and the item
  list, in order, exactly equals the index's own full `column_
  ordinals`. **`NULLS FIRST` is not incidental** — it is the *only*
  order the engine can physically produce: no reverse-scan primitive
  exists anywhere in `LsmEngine` (grepped, not assumed), and `PHASE_
  RELATIONAL_INDEX_BACKFILL_ADR.md` §3 fixes `NULL` as always sorting
  first. D5's own SQL-standard *default* for a bare `ORDER BY col`
  (ascending) is **`NULLS LAST`** — the opposite — so the common case
  (`ORDER BY name` with no explicit `NULLS` clause) correctly **retains**
  `Sort`, and only an explicit `ORDER BY name NULLS FIRST` eliminates
  it. Both directions are directly tested (`sort_is_retained_for_the_
  sql_standard_default_nulls_last_ascending`, `sort_is_eliminated_
  when_index_scan_already_provides_matching_ascending_order`) — this
  distinction was found by a failing test during this increment's own
  development, not assumed correct from the start (see §14).
- **`DESC`**: always retains `Sort` — no reverse-scan primitive exists,
  full stop (`sort_is_retained_for_descending_order_no_reverse_scan_
  primitive_exists`).
- **A pure `ORDER BY` with no `WHERE` at all** still benefits: `physical
  ::order_only_index_access` recognizes a bare `Projection` directly
  over an un-filtered `Scan` and, if a `Ready` index's `column_
  ordinals` exactly match the `ORDER BY` items (same ascending/`NULLS
  FIRST` requirement), substitutes an *unbounded* `IndexScan` purely for
  the ordering it provides "for free" — a real `index_range_scan`
  call with both bounds `Unbounded`, not an invented operator.

## §10. `DISTINCT` (item 16)

Represented as its own explicit `LogicalPlan::Distinct`/`PhysicalPlan::
Distinct` node — **never** silently lowered to `GroupBy` (which does
not even exist in this crate's plan model, §1) and never optimized away
(no uniqueness-proof mechanism exists yet to justify that). Evaluation-
order-correct placement: `Projection → Distinct → Sort` (`DISTINCT`
de-duplicates the *projected* output before `ORDER BY` sees it).

## §11. `JOIN` planning (items 17/18/19)

Only `INNER`/`LEFT` are representable (`crate::ast::JoinKind` has no
other variant — the grammar itself has never supported more). D18's
Nested-Loop-with-mechanical-Index-Nested-Loop-substitution is
implemented exactly: `physical::build_physical_plan`'s `Join` arm plans
the outer (`left`) side normally, then — if `right` is a bare `Scan` —
combines its own local predicate with the `ON` condition's conjuncts
that reference it and calls the **same** `access::plan_table_access`
used everywhere else. `JoinAlgorithm::IndexNestedLoop` is not a
separate mechanism: `physical::is_correlated_to` simply checks whether
the resulting access's own key/bound expressions reference a
`TableRefId` from the *outer* side (`BoundExpr::Column`, the exact same
representation `crate::bound` already uses for a correlated reference —
no new expression type invented). If the inner access ends up `PkLookup`
or `IndexScan` with such a correlated key, the join is `IndexNestedLoop`;
otherwise `NestedLoop` — directly tested for both the PK case (`join_
on_inner_pk_selects_index_nested_loop`) and the no-usable-index case
(`join_with_no_usable_inner_index_selects_plain_nested_loop`).

**The full `ON` condition is always still carried on the `Join` node
and evaluated in full** by a future executor, regardless of what the
inner access already consumed as a candidate-narrowing key (item 10's
own "an index is an access path, not proof a predicate is fully
satisfied," applied here to join keys too) — this is what makes
combining `ON`-conjuncts into the inner scan's predicate safe for
`LEFT JOIN` as well as `INNER JOIN`: narrowing *how matching candidates
are found* never changes what the join does when none are found.

## §12. Rule engine determinism and termination (items 27/28)

Every optimization rule runs **exactly once**, in one fixed order
(`optimize::optimize`: pushdown → pruning → limit-marking), never
iterated and never re-triggering an earlier rule. This is not "an
iterative fixpoint proven to converge" — there is no loop, by
construction, so there is nothing that could cycle (`A → B → A`) or
hang on a malformed query. Chosen over an iterative fixpoint absent any
evidence one is needed, per this project's own "do not build
unjustified complexity ahead of evidence" convention (D16's own
citation of it). **Every rule is provably result-preserving by
construction**, not by per-rule proof: no rule rewrites a `BoundExpr`'s
own logic (§6) — only where it is evaluated, or which optimizer-
internal metadata (`required_columns`) is attached.

## §13. Plan validation, explainability, determinism (items 29/30/31/53)

`crate::plan::validate::validate_plan` is a **defensive self-check** on
this crate's own output, not a re-resolution: it confirms every
accessed `table_id` still resolves in the catalog, every selected
`index_id` actually belongs to the scanned table (a real, meaningful
check against a stale/mismatched selection bug, not merely structurally
guaranteed by the call graph), and every `Parameter` index referenced
anywhere in the plan is within the statement's own declared `max_
parameter`. It never re-resolves an identifier by name and never re-
runs authorization (D25 stays entirely at the binder).

`crate::plan::explain::explain` is a secondary, deterministic text
formatter (never the *only* representation — `Plan`/`PhysicalPlan`
themselves are) — a compact, indented tree showing the chosen access
path, join algorithm, filters, sort, and limit/pushability, using a
non-SQL-reconstructing rendering of every `BoundExpr` (column
references by `(table_ref, ordinal)`, parameters as `$n`, literals by
their own typed value) that never echoes raw user SQL text and never
includes a filesystem path (verified directly, `explain_output_is_
deterministic_and_contains_no_filesystem_path`).

**Plan determinism** (item 53) falls out of the above: no rule
consults a `HashMap`'s iteration order for anything observable in the
output (index-selection tie-breaking uses `list_indexes`'s own already-
deterministic ordering, §5; conjunct/required-column collection uses
`BTreeSet`, not `HashSet`), and nothing in the pipeline reads wall-clock
time, thread id, or any other non-reproducible input. Directly tested
(`identical_input_produces_an_identical_plan_every_time`, full
`PartialEq` comparison of two independently-built plans from the same
input).

## §14. Resource limits (item 26)

`PlannerLimits` (`sql/src/plan/limits.rs`), distinct from `crate::
limits::SqlLimits` (the parser/binder boundary's own limits, already
enforced before a `BoundStatement` exists): `max_joins` (64),
`max_predicate_conjuncts` (1,000), `max_plan_nodes` (10,000) — every
one checked *before* the corresponding planner work (`build_logical_
plan` rejects an over-wide `FROM` before building any `Join` node;
`optimize` rejects an over-large conjunct decomposition before pushing
any of them; a blanket node-count check runs before any rule at all).

## §15. Security (items 24/25/36/51)

**Authorization is never re-run, never re-resolved by name**: every
`table_id`/`ColumnRef`/`index_id` this crate's plan carries originates
from `bound` itself, verbatim or relocated — there is no code path that
looks up an object by string name during planning (`access::plan_
table_access` resolves by already-bound `table_id` only). No planner
error variant (`SqlError::PlanValidation`, reused `ResourceLimit`)
carries a physical filesystem path, raw storage key, or unauthorized-
object hint — `validate.rs`'s own messages are generic ("a table that no
longer resolves," "does not belong to the scanned table"), matching
`SqlError`'s own established "never carries key/value bytes or row
contents" discipline.

No `unsafe` anywhere in `sql/src/plan/`. No `.unwrap()`/`.expect()` on
caller-controlled input — every fallible catalog call propagates
`Result`; the only unwraps in the planner's own tests are `#[test]`-only.
No unbounded recursion: every expression-tree walker in `expr_util.rs`
is a direct structural recursion bounded by `SqlLimits::max_expression_
depth` (already enforced at bind time, before a `BoundExpr` this deep
can exist) — no new, separate depth limit was needed because the
planner never builds an expression tree deeper than what the binder
already validated. No optimizer loop exists to hang on adversarial
input (§12). Adversarially tested: a 100-term `OR` chain and a 51-term
`AND` chain both plan successfully without panicking (`deeply_nested_
or_predicate_within_sql_limits_does_not_overflow_the_planner`, `many_
and_conjuncts_within_default_limits_plan_successfully`).

## §16. A real, pre-existing vulnerability found and fixed

While writing the adversarial `OR`-chain test above, planning a mere
**20-term** `WHERE` chain took ~4 seconds and climbing exponentially
with each additional term (measured: 15→139ms, 18→1.07s, 20→3.94s — a
clean ~1.92×-per-term growth curve). Traced to `sql/src/bind/expr.rs`'s
`bind_shared` (Increment 6, pre-dating this increment): its second pass
(re-binding every operand once the shared type across `exprs` was
known) unconditionally re-bound **every** operand, including one that
was already a fully-resolved, rigidly-typed subtree (a nested
`BinaryOp`, a column) whose type could never change on a second bind.
Because `bind_shared` sits on `bind`'s own recursive path (`bind_binary`
calls it for every operator, including nested ones), that unconditional
re-bind doubled the work at every nesting level — `O(2^depth)`, not
`O(depth)` — a genuine, exploitable CPU-exhaustion vector reachable
with an ordinary, `SqlLimits`-compliant `WHERE` clause (a 100-term
chain is well within the default 128-deep expression-depth limit; the
*depth* limit alone does not bound the *work*, only the *shape*).

**Fixed** (`sql/src/bind/expr.rs`): the second pass now only re-binds
operands that were a flexible `Literal` on the first pass (the one case
that genuinely needs to conform to the newly-determined shared type); a
rigid expression's already-bound result is kept as-is, with its type
merely re-checked (`check_assignable`) against the shared type — O(1)
per operand instead of a full re-walk. D21's "no implicit coercion,
exact match only" semantics are unchanged and re-tested (`bind_tests::
shared_type_unification_still_rejects_mismatched_rigid_types`, `...
_still_conforms_a_flexible_literal_to_a_rigid_column_type`). Verified
fixed: a 100-term chain now binds in under 1ms; a 500-term chain is
correctly rejected by the pre-existing, unrelated `SqlLimits::max_
expression_depth` guard (128) rather than hanging.

This is the same recurring pattern this project has hit twice before
(Increment 5's phantom-index-entry race, Increment 6's stack-overflow-
from-a-flat-operator-chain) — a real bug surfaced by *writing an
adversarial test against real code*, not by inspection, in a layer this
increment did not set out to touch. Fixed immediately rather than
deferred, per this project's own established practice.

## §17. Transaction/snapshot/autocommit compatibility (items 44/45/46)

The planner never calls into `TransactionManager`/`Transaction`/
`TableStore` at all — it only ever reads catalog metadata
(`CatalogService::get_table`/`get_columns`/`get_index`/`list_indexes`).
`PhysicalAccess`'s key/bound expressions are `BoundExpr`s a future
executor evaluates against whatever read context (an autocommit call or
an explicit `Transaction`) it is given — the plan itself carries no
snapshot, no transaction handle, no mutable-current-state read of any
kind, so it is structurally unable to bypass a snapshot or hard-code
autocommit-vs-explicit-transaction behavior. No transaction-layer code
was touched this increment (`git diff --stat -- src/relational/txn.rs`
is empty).

## §18. Performance

See `PHASE_RELATIONAL_QUERY_PLANNER_INCREMENT8_RESULTS.md` for full
measured numbers. Headline findings: plan-build latency is in the tens-
of-microseconds range for ordinary statement shapes (a point lookup:
~27 µs; a two-table join: ~50 µs), dominated by the same uncached
catalog-resolution cost `sql_binder_bench.rs` already found one layer
up (§7's own honest "no caching added" stance, consistent here);
predicate-conjunct count and join count both scale linearly, never
exponentially — directly the property §16's fix restored.

---

## Testing summary

35 new tests in `sql/src/plan_tests.rs` (logical/physical separation,
`PRIMARY KEY`/index/range/residual-filter correctness, `LEFT JOIN`
safety, `NULL` semantics, projection pruning, limit/sort analysis in
every direction including the two D5-default-vs-index-physical-order
cases, `DISTINCT`, join algorithm selection, `UPDATE`/`DELETE` reuse,
`INSERT`/DDL/transaction-control pass-through, `EXPLAIN` determinism,
resource limits, metrics, adversarial deep predicates, and race-free
concurrent planning), 2 new tests in `sql/src/plan_reference_model.rs`
(a fixed differential matrix plus a randomized `proptest` comparing
`plan_table_access`'s `PkLookup`/`IndexScan`/`SeqScan` classification
against an independent, from-scratch reference model over generated
predicate/index shapes — never the production planner as its own
oracle), and 3 new regression tests in `sql/src/bind_tests.rs` for §16's
binder fix.
