# SQL Mission 1 of 4 — Feature Matrix (Subqueries, CTEs, Set Operations)

Created before any implementation code (mission "first deliverable"). Rows are never
deleted; statuses are updated at the end of every sub-slice. Status vocabulary:
`IMPLEMENTED AND VERIFIED` · `PARTIAL` · `MISSING` · `BLOCKED`.
"Parser accepts the syntax" is not IMPLEMENTED; execution against a real table is required.

Base tree: `master` @ `279e40427fd5b47c91f115eb4aa813fb755d7745` (working tree carries the
maintainer's uncommitted F-08 changes under `src/sstable/`, `src/lsm/`, `src/ops/`, docs and
`scratch/`; this mission touches none of them).

## 0. Pre-mission baseline (recorded before editing; exact counts)

| Command | Result |
|---|---|
| `cargo test -p rubixdb-sql --lib` | 335 passed, 0 failed, 22 ignored |
| `cargo test -p rubixdb-sql --lib bind_tests` | 62 passed, 0 failed |
| `cargo test -p rubixdb-sql --lib exec_tests` | 63 passed, 0 failed |
| `cargo test -p rubixdb-sql --lib plan_tests` | 43 passed, 0 failed |
| `cargo test -p rubixdb-sql --test write_crash_consistency -- --ignored` (as given) | **0 tests ran** (see discrepancy D-1) |
| `cargo test -p rubixdb-sql --features test-util --test write_crash_consistency` (the runnable form, D-1) | 2 passed, 0 failed |
| `cargo test -p rubixdb-api` | 223 passed, 0 failed, 1 ignored (lib 78+1 ignored; admin_ops 12, api_cancellation 1, api_commit_ack_loss 1, api_delete_safety 12, api_http_fuzz 4, api_instance_and_frontend 6, api_integration 16, api_security_validation 11, api_sql_integration 25, compaction_failure_reporting 2, observability 38, observability_sampler_failure 2, security_events_and_headers 10, security_overhead_probe 1, sstable_integrity_reporting 4) |

### 0.1 Discrepancies between the mission text and the source (recorded, not invented around)

* **D-1** `sql/tests/write_crash_consistency.rs` is `#![cfg(feature = "test-util")]` and contains no
  `#[ignore]` test, so the command as written compiles an empty test binary and reports 0 tests.
  The real run is `cargo test -p rubixdb-sql --features test-util --test write_crash_consistency`
  (2 tests, recorded in the table above).
* **D-2** The mission says "the doc says … scans do not [have read-your-own-writes]". That statement
  is the superseded text of `PHASE_RELATIONAL_AGGREGATION_ARCHITECTURE.md` section 12.
  `PHASE_RUBIXDB_INCREMENT18_RESULTS.md` (lines 17, 36, 81) records it as **fixed/superseded**:
  a bounded, versioned write-set overlay is merged into every scan (`exec/access_op.rs`
  `OverlayMerge`, `sql/src/txn_scan_tests.rs`). A subquery scan runs through the same `AccessOp`
  with the caller's `Transaction`, so it inherits that behavior; a test in this mission pins it.
* **D-3** The mission names `plan_reference_model.rs`-style differential testing; that file models
  *plan shape* for single-table SELECT. `reference_model.rs` / `aggregate_reference_model.rs` /
  `index_read_differential_tests.rs` are the row-level references. New differential models are
  written independently per feature (they do not reuse engine code).
* **D-4** `api/src/error.rs` has no `BIND_OR_RUNTIME_ERROR` code. The mission's "runtime
  BIND_OR_RUNTIME_ERROR" for a multi-row scalar subquery maps onto the existing runtime class
  `SqlError::ExecutionParameter` -> HTTP 400 `EXECUTION_ERROR`. No new error code is invented.
* **D-5** The Slice B prompt asked for the two new limits in `sql/src/limits.rs` (`SqlLimits`). They are
  executor limits and live in `exec::ExecLimits` beside `max_materialized_rows` /
  `max_materialized_bytes` (the executor never sees `SqlLimits`). **D-5b** The same prompt says a
  forbidden table is a bind-time `AUTHORIZATION_ERROR` *and* indistinguishable from "table does not
  exist"; the implemented, tested behavior is the second (`UnknownObject`, HTTP 404 `NOT_FOUND`), which
  is what `bind::scope::resolve_table` has always done.
* **D-6** The Slice B prompt names `cargo test -p rubixdb-sql --test mission1_corpus`; the corpus is a
  lib module (`mission1_corpus_tests`), run as `--lib mission1_corpus`.
* **D-7** The Slice C prompt calls the corpus "17 statements" (it was 28 after Slice B); asks for a
  `derived_table=true` flag on `SubqueryNode` (derived tables and CTE references are *relations*, so they are
  the plan leaves `Derived` / `CteNode`, not an expression-level subquery node); and states that a CTE's
  self-reference is a bind error (implemented as: the name is not visible in its own body, so it means the
  catalog table if one exists and is the precise error otherwise).

## 1. Architecture facts this design rests on (read from source)

| Fact | Where |
|---|---|
| AST has no subquery/CTE/set-op/derived-table node; `convert_query` rejects `q.with`, non-`SELECT` bodies, `TableFactor::Derived`, `Expr::{Subquery,Exists,InSubquery}` | `sql/src/convert.rs` `convert_query`, `convert_table_factor`, `expr_kind_name` |
| `ast::Select` carries its own `order_by/limit/offset`; many existing tests destructure `Statement::Select(s)` / `BoundStatement::Select(s)` | `sql/src/ast.rs`, `parse_tests.rs`, `bind_tests.rs` |
| Column identity is `(table_ref, table_id, ordinal)`; `TableRefId` is the index into one SELECT's scope (restarts at 0 per SELECT) | `sql/src/bound.rs`, `bind/scope.rs` |
| `ScopeEntry` holds only a catalog `ResolvedTable`; there is no parent scope (no correlation) and no non-table relation | `bind/scope.rs` |
| Executor rows live in `RowContext` keyed by `table_ref`; `AccessOp` emits `outer.with_row(table_ref,row)` so an `outer` context is already carried into every tuple (used by the correlated index-nested-loop join) | `exec/mod.rs`, `exec/access_op.rs` |
| `EmptyRelationOp` discards `outer` | `exec/operators.rs` |
| `GroupingKey` already gives SQL "NULLs equal, -0.0 = 0.0, NaN canonical" equality + `Hash` | `sql/src/aggregate.rs` |
| Existing limits to respect | `SqlLimits::max_expression_depth`, `PlannerLimits::max_plan_nodes`, `ExecLimits::{max_materialized_rows, deadline, max_group_count, max_aggregate_state_bytes}` |

## 2. Matrix

Columns: **Gap** = the first pipeline stage that lacks the feature (Parser = `convert.rs`/`parse.rs`;
Binder = `bind/`; Planner = `plan/`; Executor = `exec/`). All rows start MISSING; every stage
named is a gap unless stated.

### 2.1 Subqueries

| # | Feature | Status | Where support lives (file / symbol) | Gap | Slice |
|---|---|---|---|---|---|
| S1 | Scalar subquery in SELECT list | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | `convert.rs::convert_expr` (`sp::Expr::Subquery`), `ast::Expr`, `bind/expr.rs::ExprBinder::bind`, `bound::BoundExprKind`, `exec/expr_eval.rs::eval` | Parser, Binder, Planner, Executor | A |
| S2 | Scalar subquery in WHERE | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | same as S1 + `bind/select.rs::bind_select` | all four | A |
| S3 | Scalar subquery in HAVING | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | same as S1 + `bind/select.rs` (`validate_group_compat`) | all four | A |
| S4 | Scalar subquery as an operand (`a = (SELECT …)`, arithmetic) | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | `bind/expr.rs::bind_binary`/`bind_shared`, `exec/expr_eval.rs::eval_binary` | all four | A |
| S5 | Derived table `FROM (SELECT …) AS t` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests` -- parser / binder / derived / lateral / cte_binder / cte / materialization / diff_relations, corpus, `api_sql_integration`) | `convert.rs::convert_table_factor`, `ast::FromClause/TableRef`, `bind/scope.rs::Scope`, `plan/logical.rs`, `exec/operators.rs` | all four | C (shares the relation-source machinery with CTE references) |
| S6 | `IN (subquery)` | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | `convert.rs` (`sp::Expr::InSubquery`), `bound::BoundExprKind`, `exec/expr_eval.rs::eval` | all four | A |
| S7 | `NOT IN (subquery)` (3-valued logic) | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | same as S6 | all four | A |
| S8 | `EXISTS (subquery)` | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | `convert.rs` (`sp::Expr::Exists`), `exec/expr_eval.rs::eval` | all four | A |
| S9 | `NOT EXISTS (subquery)` | IMPLEMENTED AND VERIFIED (Slice A: `subquery_tests::exec`, `::differential`, corpus, `api_sql_integration`) | same as S8 | all four | A |
| S10 | Correlated subqueries (outer column references, re-executed per outer row) | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests` -- binder / scalar / exists / in_subquery / dml / nested / shapes / diff*, `exec::subquery`, corpus, `api_sql_integration`) | `bind/scope.rs::Scope` (parent chain), `bound::BoundSubquery` (outer refs), `exec/expr_eval.rs`, `exec/operators.rs::EmptyRelationOp` | Binder, Planner, Executor | B |
| S11 | `LATERAL` derived table | IMPLEMENTED AND VERIFIED as `[LEFT] JOIN LATERAL (SELECT ...) alias ON cond` (Slice C: `relation_tests::lateral`, `::binder`, `::diff_relations`, corpus; the comma / CROSS spellings need comma / CROSS joins, out of the mission, and stay a typed `Unsupported`) | `convert.rs::convert_table_factor`, `bind/select.rs` | Parser, Binder, Planner, Executor | C (decision recorded there; BLOCKED only if S5/S10 prove infeasible) |
| S12 | Subquery in `UPDATE … WHERE` | IMPLEMENTED AND VERIFIED (uncorrelated: Slice A; correlated WHERE and SET: Slice B -- `correlated_tests::dml`, `exec::write::execute_update_with_subqueries`) | `bind/dml.rs::bind_update`, `plan/mod.rs` (`Plan::Update`), `exec/write.rs::execute_update` (evaluates through `build_operator` + `eval`, so subquery evaluation is inherited) | Parser, Binder, Planner | A (uncorrelated), B (correlated) |
| S13 | Subquery in `DELETE … WHERE` | IMPLEMENTED AND VERIFIED (uncorrelated: Slice A; correlated: Slice B -- `correlated_tests::dml`, `::nested`) | `bind/dml.rs::bind_delete`, `plan/mod.rs` (`Plan::Delete`), `exec/write.rs::execute_delete` | Parser, Binder, Planner | A (uncorrelated), B (correlated) |
| S10.1 | Correlated scalar subquery (SELECT list / WHERE / ORDER BY / HAVING), 0 / 1 / >1 rows per outer row, runtime error names the outer row | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests::scalar`, `::diff`, HTTP `correlated_scalar_subquery_through_the_real_api`) | `bind/expr.rs::bind_column`, `bind/scope.rs::{Scope::lookup_column, OuterScopes}`, `bind/env.rs`, `exec/subquery.rs`, `exec/operators.rs` (`EmptyRelationOp`, `AggregateOp`) | Binder, Executor | B |
| S10.2 | Correlated `EXISTS` / `NOT EXISTS` | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests::exists`, `::diff_exists`, HTTP `correlated_exists_and_not_exists_through_the_real_api`) | same | Binder, Executor | B |
| S10.3 | Correlated `IN` / `NOT IN` (NULL trap) | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests::in_subquery`, HTTP `correlated_in_and_not_in_with_null_handling_through_the_real_api`) | same + `exec/subquery.rs::in_tri` | Binder, Executor | B |
| S10.4 | `UPDATE ... SET x = (subquery)` incl. correlated, pre-statement snapshot semantics | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests::dml`, HTTP `correlated_subqueries_in_update_and_delete_through_the_real_api`) | `bind/dml.rs`, `plan/mod.rs`, `exec/write.rs::execute_update_with_subqueries` | Binder, Planner, Executor | B |
| S10.5 | Nested correlation, three levels, each level reading the levels above; mixed correlated/uncorrelated nesting | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests::nested`, HTTP `three_level_correlated_subqueries_through_the_real_api`) | `bind/env.rs` (capture frames) | Binder, Executor | B |
| S10.6 | Scope resolution: innermost wins, unknown at no level, no fall-through of an inner ambiguity | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests::binder`) | `bind/scope.rs`, `bind/expr.rs` | Binder | B |
| S10.7 | Re-execution bounds (`max_correlated_reexecutions` 1,000,000; `max_subquery_outer_rows` 100,000), deadline between outer rows, metric `correlated_subquery_reexecs` | IMPLEMENTED AND VERIFIED (Slice B: `correlated_tests::scalar`, `::exists`, `::in_subquery`, `::dml`, `::nested`; `exec::subquery::tests`) | `exec/mod.rs` (`ExecLimits`, `ExecMetrics`, `CorrelatedUsage`), `exec/subquery.rs` | Executor | B |
| S10.8 | Correlation inside a derived table | IMPLEMENTED AND VERIFIED through `LATERAL` (an ordinary derived table is correctly refused: `relation_tests::binder`, `::lateral`) | `bind/scope.rs` | Binder | C |
| S10.9 | A CTE may not reference an enclosing query (standard) | IMPLEMENTED AND VERIFIED as a rejection (every `WITH` is `Unsupported` until Slice C; pinned by `correlated_tests::parser::a_cte_inside_a_subquery_is_refused_...`; Slice C must keep it) | `convert.rs` | Parser | C |

### 2.2 CTEs

| # | Feature | Status | Where support lives | Gap | Slice |
|---|---|---|---|---|---|
| C1 | `WITH name AS (SELECT …) SELECT … FROM name` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests` -- parser / binder / derived / lateral / cte_binder / cte / materialization / diff_relations, corpus, `api_sql_integration`) | `convert.rs::convert_query` (`q.with`), `bind/scope.rs`, new relation node | all four | C |
| C2 | Multiple CTEs in one `WITH` (later CTE may reference an earlier one) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests` -- parser / binder / derived / lateral / cte_binder / cte / materialization / diff_relations, corpus, `api_sql_integration`) | same | all four | C |
| C3 | A CTE referenced more than once (same rows each time) | IMPLEMENTED AND VERIFIED (materialized once per statement: `relation_tests::cte`, `::materialization`, `::diff_relations`) | new `CteRef` + per-statement materialization cache in `ExecCtx` | Planner, Executor | C |
| C4 | `WITH RECURSIVE name AS (seed UNION ALL recursive) …` | MISSING | `convert.rs`, new recursive-CTE operator | all four | D (needs E's `SetOp` shape for the seed/recursive split) |
| C5 | Recursive CTE bounded by depth (default 1,000, configurable) | MISSING | new limit in `exec::ExecLimits` | Executor | D |
| C6 | Recursive CTE bounded by total rows (default 1,000,000, configurable) | MISSING | new limit in `exec::ExecLimits` | Executor | D |
| C7 | CTE shadowing a base table name | IMPLEMENTED AND VERIFIED (`relation_tests::cte_binder`, `::cte`; a qualified name is never a CTE) | `bind/scope.rs` name resolution order | Binder | C |
| S5.1 | Derived-table alias required and unique; column-name list `AS t(a, b)` (exact count) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::binder`, HTTP `derived_tables_through_the_real_api`) | `bind/select.rs::{bind_from_item, relation_output}` | Parser, Binder | C |
| S5.2 | Non-LATERAL derived table / CTE body cannot read outer or sibling columns (`derived table 'd' cannot reference outer column 'c' without LATERAL`) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::binder`, `::lateral`, `::cte_binder`) | `bind/scope.rs::{OuterScopes, Boundary}` | Binder | C |
| S5.3 | Derived table with GROUP BY / HAVING / ORDER BY / LIMIT / DISTINCT inside; ORDER BY inside is applied (decision), LIMIT without ORDER BY allowed | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::derived`) | `plan/logical.rs`, `exec/relation.rs` | Planner, Executor | C |
| S5.4 | Derived table in a join (right side materialized once, left side streamed); derived table in a subquery / correlated subquery / DML | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::derived`, `::materialization`) | `plan/physical.rs` (`rescanned`), `exec/relation.rs` | Planner, Executor | C |
| S5.5 | Set operation inside a derived table | BLOCKED until Slice E (typed `Unsupported`, pinned by `relation_tests::parser`) | `convert.rs` | Parser | E |
| C1.1 | CTE column-name list `WITH x(a, b) AS (...)` (exact count); reference rename `FROM x AS y(p, q)` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`) | `bind/select.rs::bind_with_list` | Binder | C |
| C1.2 | Self-reference without RECURSIVE: `CTE 'x' refers to itself; use WITH RECURSIVE` (a same-named catalog table is read instead, per SQL scoping) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`) | `bind/scope.rs::OuterScopes::lookup_cte` | Binder | C |
| C1.3 | Forward reference to a later CTE of the same list is a bind error | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`) | same | Binder | C |
| C1.4 | CTE referenced zero times: no error, never executed (still bound, so a bad / forbidden definition is an error) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte`, `::cte_binder`) | `plan/mod.rs::plan_ctes` | Binder, Planner | C |
| C1.5 | `WITH` before `UPDATE` / `DELETE` (usable) and `INSERT` (bound, never usable) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::parser`, `::cte`, `::materialization`, HTTP) | `convert.rs::convert_query_statement`, `bind/dml.rs` | all four | C |
| C1.6 | `WITH` inside a scalar subquery / derived table is scoped to it and never leaks; inner shadows outer only inside | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`, `::cte`) | `bind/scope.rs` | Binder | C |
| C1.7 | CTE materialization bounded by `max_materialized_rows` / `_bytes`; `max_cte_definitions` 64, `max_cte_references` 256, `max_derived_table_depth` 32; metric `cte_materializations_total` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte`, `::derived`; HTTP 64/65 definitions) | `exec/mod.rs::check_relation_limits`, `plan/relstats.rs`, `exec/relation.rs` | Executor | C |
| C1.8 | CTE whose body is a set operation | BLOCKED until Slice E (typed `Unsupported`) | `convert.rs` | Parser | E |
| C1.9 | `EXPLAIN` shows a twice-read CTE once, flagged materialized; a derived table with `outer_row_input` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte`, `::derived`, `::lateral`, HTTP) | `plan/explain.rs` | Planner | C |

### 2.3 Set operations

| # | Feature | Status | Where support lives | Gap | Slice |
|---|---|---|---|---|---|
| O1 | `UNION` | MISSING | `convert.rs::convert_query` (`sp::SetExpr::SetOperation`), new `BoundSetOp`, `plan/logical.rs::LogicalPlan::SetOp`, new operator | all four | E |
| O2 | `UNION ALL` | MISSING | same | all four | E |
| O3 | `INTERSECT` | MISSING | same | all four | E |
| O4 | `INTERSECT ALL` | MISSING | same | all four | E |
| O5 | `EXCEPT` | MISSING | same | all four | E |
| O6 | `EXCEPT ALL` | MISSING | same | all four | E |
| O7 | `INTERSECT` binds tighter than `UNION`/`EXCEPT` | MISSING (sqlparser builds the tree with standard precedence; the converter must preserve it) | `convert.rs` | Parser | E |
| O8 | Output column names come from the first branch | MISSING | `bind/select.rs` (new set-op binder) | Binder | E |
| O9 | Column-count and compatible-type check at bind time | MISSING | `bind/` (new), uses `bind/expr.rs::check_assignable` rules (exact type match, D21) | Binder | E |
| O10 | NULL = NULL for set-operation distinctness | MISSING | new operator over `aggregate::GroupingKey` | Executor | E |
| O11 | `ORDER BY` / `LIMIT` / `OFFSET` over the whole set result (output-column references only) | MISSING | `bind/` (new), `plan/logical.rs` | Binder, Planner, Executor | E |

## 3. Blocked / out-of-scope decisions

None yet. `LATERAL` (S11) is the one row that may become BLOCKED; it is decided in Slice C with
evidence, not assumed.

**Slice C decisions (2026-10-11):** `LATERAL` (S11) is **IMPLEMENTED**, not blocked -- sqlparser has the grammar
and the nested-loop join already re-runs its right side per left row; supported spelling `[LEFT] JOIN LATERAL
(...) alias ON cond`. BLOCKED until Slice E: set operations inside a derived table or a CTE body (S5.5, C1.8).
OUT OF SCOPE here, Slice D: `WITH RECURSIVE` (typed `Unsupported`).

## 4. Design commitments (one node family; no per-syntax special cases)

* **One relational-query-tree family**: `QueryExpr` (AST) -> `BoundQuery` (bound) ->
  `LogicalPlan`/`PhysicalPlan` nodes `SubqueryScan` (a query used as a relation: derived table and
  CTE reference), `SetOp` (set operations and the recursive-CTE seed/step split) and expression-level
  `BoundExprKind::Subquery` (scalar / `EXISTS` / `IN`). A subquery, a CTE reference and a set-op
  branch are all "a nested plan producing rows consumed by an outer operator".
* **Statement-wide unique `TableRefId`s** (a counter threaded through the binder), so an inner query's
  rows can be merged with an outer row context without ever colliding on `table_ref`.
* **Existing tests are preserved untouched**: `Statement::Select` / `BoundStatement::Select` keep their
  shapes (fields are only added); nothing is renamed.
* **Resource bounds**: subquery/IN-set/CTE materialization is bounded by
  `ExecLimits::max_materialized_rows` and fails with `SqlError::ResourceLimit`; never truncates.
* **No protected path touched** (`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`,
  `src/error.rs`), no new dependency, no persisted-format change, no frontend change.

## 5. Per-slice status log (updated at the end of every slice)

| Slice | State | Date |
|---|---|---|
| A — scalar / IN / EXISTS, uncorrelated (+ UPDATE/DELETE WHERE) | **DELIVERED** (report in section 6) | 2026-10-11 |
| B — correlated | **DELIVERED** (report in section 6) | 2026-10-11 |
| C — derived tables, non-recursive CTEs (+ LATERAL decision) | **DELIVERED** (report in section 6) | 2026-10-11 |
| D — recursive CTEs | not started | |
| E — set operations | not started | |

## 6. Slice reports

### Slice A -- scalar / `IN` / `EXISTS` subqueries (uncorrelated), and their use in `UPDATE`/`DELETE ... WHERE`

**Implemented semantics.** A scalar `(SELECT ...)`, `[NOT] EXISTS (SELECT ...)` and
`expr [NOT] IN (SELECT ...)` are accepted wherever a `SELECT` list item, `WHERE`, `HAVING` or
`ORDER BY` expression is, and in the `WHERE` of `UPDATE`/`DELETE`. All three are one
`BoundExprKind::Subquery(BoundSubqueryExpr { kind: Scalar | Exists | In, operand, query, plan })`:
the binder binds the inner `SELECT` through the same `bind_select_in` the statement uses (a fresh
scope, statement-wide unique `TableRefId`s, statement-wide subquery id/count/depth accounting in
`BindEnv`), the planner plans it with the same logical -> optimize -> physical pipeline and attaches
the `PhysicalPlan` to the node, and the executor runs it as a nested operator tree through
`build_operator`/`Operator::next` (`exec/subquery.rs`). Scalar: pulls at most 2 rows -- 0 rows is
`NULL`, 1 row is its value, a 2nd row is a **runtime** `SqlError::ExecutionParameter` (HTTP 400
`EXECUTION_ERROR`), a subquery with more than one column is a **bind** error. `EXISTS`: pulls at most
1 row, never `NULL`. `IN`/`NOT IN`: the single column is materialized into a hash set keyed by
`GroupingKey` (bounded by `max_materialized_rows` and the new `max_materialized_bytes`, failing
closed with `RESOURCE_LIMIT`, never truncating) and evaluated in SQL three-valued logic (empty set ->
FALSE even for a NULL operand; NULL operand -> UNKNOWN; member -> TRUE; non-member with a NULL in the
set -> UNKNOWN; so `x NOT IN (..., NULL)` is never TRUE). An uncorrelated subquery runs once per
statement (result cached in `ExecCtx` by subquery id; metrics `subquery_executions` /
`subquery_cache_hits`). A subquery in any clause that cannot run it (`JOIN ... ON`, `GROUP BY`,
`LIMIT`/`OFFSET`, `INSERT ... VALUES`, `UPDATE ... SET`, `DEFAULT`) fails closed with
`SqlError::Unsupported`. `EXPLAIN` renders each nested plan (`Subquery #N`) and never executes it.

**Changed files** (tracked files: see the table; new files with line counts):

| File | + / - | Role |
|---|---|---|
| `sql/src/limits.rs` | +13 | `SqlLimits::{max_subquery_depth = 32, max_subqueries_per_statement = 256}` |
| `sql/src/ast.rs` | +23 | `Expr::{ScalarSubquery, Exists, InSubquery}`, `QueryExpr` |
| `sql/src/convert.rs` | +26 / -3 | convert the three sqlparser nodes through the existing `convert_query` |
| `sql/src/bound.rs` | +34 | `BoundExprKind::Subquery`, `BoundSubqueryExpr`, `SubqueryKind` |
| `sql/src/bind/env.rs` (new) | 96 | `BindEnv`: statement-wide ref counter, subquery id/count/depth |
| `sql/src/bind/scope.rs` | +18 / -1 | `Scope` allocates `TableRefId` from the shared counter |
| `sql/src/bind/expr.rs` | +128 / -1 | `ExprBinder::with_env`, scalar/EXISTS/IN binding and typing |
| `sql/src/bind/select.rs` | +45 / -6 | `bind_select_in`; env passed to projection/WHERE/HAVING/ORDER BY; aggregate walkers |
| `sql/src/bind/dml.rs`, `bind/mod.rs` | +10 / -5 | env for `UPDATE`/`DELETE` WHERE |
| `sql/src/plan/mod.rs` | +113 / -6 | `plan_select` + per-subquery planning, used by SELECT/UPDATE/DELETE |
| `sql/src/plan/expr_util.rs` | +99 | `Subquery` arms in the table-ref/ordinal walkers; `for_each_subquery_mut` |
| `sql/src/plan/validate.rs` | +23 | `validate_subplan`; `Subquery` arm |
| `sql/src/plan/explain.rs` | +160 / -2 | `Subquery #N` rendering (SELECT, UPDATE, DELETE) |
| `sql/src/exec/mod.rs` | +40 | `ExecCtx` subquery cache, `max_materialized_bytes`, two metrics |
| `sql/src/exec/subquery.rs` (new) | 268 | scalar / EXISTS / IN evaluation |
| `sql/src/exec/expr_eval.rs` | +4 / -3 | dispatch to `eval_subquery`; three helpers made `pub(super)` |
| `sql/src/parse_tests.rs` | +9 / -1 | **see "Flagged test edit" below** |
| `sql/src/lib.rs` | +10 | registers the new test modules |
| `api/tests/api_sql_integration.rs` | +285 | 5 new HTTP tests |
| `sql/src/subquery_tests.rs` (new) | ~1,400 | parser / binder / exec / differential tests |
| `sql/src/mission1_support.rs`, `mission1_corpus_tests.rs` (new) | 233 / 149 | shared harness; cross-slice statement corpus |
| `sql/mission1_evidence/` (new) | | mutation harness + results, pre-mission-tree corpus run |

**Tests added and results.** `subquery_tests`: 67 (`parser` 8, `binder` 17, `exec` 41 incl. the
IN/NOT IN truth tables, `differential` 1 = 40 random seeds x IN / NOT IN / EXISTS / NOT EXISTS /
scalar / scalar-comparison against an independently written three-valued-logic model);
`mission1_corpus_tests`: 1 (17 statements); `api_sql_integration`: 5 new (HTTP round trip, error
codes, DML, in-session read-your-own-writes, EXPLAIN). All pass.

**Non-vacuity.** (1) *Pre-mission tree:* the 17-statement corpus, which compiles unchanged on the
pre-mission commit, was run there: 0 of 17 pass, each fails with
`unsupported: expression kind: subquery expression | IN (subquery) | EXISTS`
(`sql/mission1_evidence/sliceA_corpus_on_pre_mission_tree.txt`). (2) *Mutation:* 8 one-line mutations
of the new code (`sliceA_mutations.json`, harness `mutate.py`) each make at least one test fail:
NOT IN trap (4 tests), empty-set NULL (3), multi-row scalar accepted (2), cache disabled (1),
table-ref ids colliding (1), IN row bound removed (1), EXISTS polarity inverted (6), IN byte bound
removed (1).

**Regression (same commands as the baseline).**

| Command | Pre-mission | After Slice A |
|---|---|---|
| `cargo test -p rubixdb-sql --lib` | 335 passed, 22 ignored | 335 + 67 + 1 = **403 passed**, 0 failed, 22 ignored |
| `--lib bind_tests` | 62 | 62 |
| `--lib exec_tests` | 63 | 63 |
| `--lib plan_tests` | 43 | 43 |
| `--features test-util --test write_crash_consistency` | 2 | 2 |
| `cargo test -p rubixdb-api` | 223 passed, 1 ignored | **228 passed**, 0 failed, 1 ignored |
| `cargo clippy -p rubixdb-sql --tests` | | 0 warnings |

**Flagged test edit (needs maintainer acknowledgement).** One existing test,
`parse_tests::unsupported_grammar_is_a_typed_error_not_a_panic`, listed `"SELECT (SELECT 1)"` among
statements that must be `Unsupported`. That is exactly the feature this mission is authorized to add,
so the case was removed (one line, with an in-file comment); every other case in the list is
untouched and the test still proves "unsupported grammar is a typed error, never a panic". The
`"WITH ..."` and `"... UNION ..."` cases in the same list will have to leave it the same way in
Slices C/E. This is the only edit to a pre-existing assertion.

**Known limitations / honest scope notes.**

* Uncorrelated only: an outer column referenced inside a subquery fails as an unknown column
  (pinned by `an_outer_column_is_not_visible_inside_an_uncorrelated_subquery_scope`, which Slice B
  will legitimately replace).
* Subqueries are refused (typed `Unsupported`) in `JOIN ... ON`, `GROUP BY`, `LIMIT`/`OFFSET`,
  `INSERT ... VALUES`, `UPDATE ... SET`, `DEFAULT`.
* A literal inside a subquery does not adapt to the outer operand's type
  (`bigint_col IN (SELECT 5)` is a `TypeMismatch`; `IN (5)` adapts) -- D21, pinned by a test.
* IN-set memory: bounded by 1,000,000 rows and 256 MiB (estimated).
* `ExecCtx` is now `!Sync` (it holds a `RefCell` cache); it was never shared across threads.
* The pre-existing `DistinctOp` is O(n^2) in distinct values (recorded in `OPEN_ITEMS.md`, not
  touched; Slice E's set operations use hashing).

**Mission-text discrepancies recorded:** D-1..D-4 (section 0.1). **Environment incident:** running
the pre-mission corpus from a `git archive` export with the shared `CARGO_TARGET_DIR` made cargo
reuse a stale `rubixdb-sql` test binary for the main tree (a 358-test run that failed the corpus);
caught because the test count was wrong, fixed with `cargo clean -p rubixdb -p rubixdb-sql
-p rubixdb-api` and a full rebuild; all numbers above are from after that rebuild. Later baseline
runs use a separate target directory.

### Slice B -- correlated subqueries (B1 scope chain .. B6 nested correlation)

**Verdict: PASS WITH OPEN ITEMS.** Every in-scope correlated form is implemented end to end and
verified (parser, binder, real-fixture execution, `POST /v1/sql`, randomized differential test against
an independent model, non-vacuity, mutation). The open items are listed in "Open items" below; none
is a failing test.

**Errata to the Slice A report (section 6 above), found when Slice B re-ran the baseline:** the
post-Slice-A `rubixdb-sql --lib` count is **403** (335 + 67 + 1), not 404, and `subquery_tests` has
**67** tests, not 68. The Slice A closing message used 403 / 67; only this document's table was wrong.

#### Baseline confirmed before editing (2026-10-11)

| Command | Result |
|---|---|
| `cargo test -p rubixdb-sql --lib` | 403 passed, 22 ignored |
| `cargo test -p rubixdb-sql --lib subquery` | 67 passed |
| corpus (`mission1_corpus_tests`; there is no `--test mission1_corpus` target, the corpus is a lib module -- D-6) | 1 passed (17 statements) |
| `cargo test -p rubixdb-api` | 228 passed, 1 ignored (sum over its 16 test binaries) |

#### Design (one mechanism, no hoisting, no second outer-context mechanism)

* **Bind time -- a scope chain, not an "outer slot".** `bind::scope::OuterScopes` is a linked chain of
  borrows (innermost enclosing query first). `Scope::lookup_column` answers *this level only*:
  `Ok(None)` means "this level does not define the name" (unqualified name absent from every FROM
  item, or a qualifier no FROM item is called) and is the only case that moves the search outward. An
  ambiguity inside one level, or a qualifier that names an inner FROM item lacking the column, is a
  definitive error for that level and is **never** re-resolved against an outer one.
  `ExprBinder::bind_column` tries its own scope, then `OuterScopes::lookup`; a name found nowhere is
  reported exactly as it would be with no enclosing query.
* **Outer references are recorded per nesting level.** `BindEnv` keeps one capture frame per nested
  query being bound. A column resolved `n` levels out is recorded in the innermost `n` frames, so a
  query that merely *contains* a correlated subquery is itself correlated (and never cached). The result
  is `BoundSubqueryExpr::outer_refs` (what is read) and `outer_labels` (`alias.column` plus the primary
  key of the outer table, used only to name the row in an error).
* **Execution reuses Slice A's outer-context handover.** `eval_subquery` passes the current `RowContext`
  to `build_operator`; `AccessOp` already emits `outer.with_row(..)`. Two operators that used to discard
  it now keep it: `EmptyRelationOp` (`SELECT t.id + 1`) and the one empty group `AggregateOp` emits over
  zero rows (`SELECT CASE WHEN COUNT(*) = 0 THEN t.id ELSE 0 END ...`). No plan node was added.
* **Re-execution gate** (`exec/subquery.rs::begin_correlated_evaluation`), run before every correlated
  evaluation: `ExecCtx::check()` (deadline + cancellation, *between outer rows*), then the statement-wide
  and per-subquery counters, then `correlated_subquery_reexecs`. Correlated results are never cached.
* **Aggregates.** `validate_group_compat` treats a column of an *enclosing* query as a constant; a
  correlated subquery in a grouped query requires the columns it reads from that query to be grouping
  columns. An aggregate whose arguments name **only** enclosing-query columns (which SQL assigns to the
  enclosing query) is refused as `Unsupported` rather than silently evaluated over the inner rows.
* **`UPDATE ... SET (subquery)`** (needed for hazard 5 / the required edge case; Slice A refused it).
  `execute_update_with_subqueries`: phase 1 evaluates every matched row's new value against the
  unmodified table with one shared `ExecCtx` (the WHERE phase's correlated usage carries over, so the
  budget is per statement); phase 2 writes. Pending rows are bounded by `max_dml_target_rows` (already
  enforced while collecting targets) and `max_materialized_bytes`. A refused/failed statement writes
  nothing. `UPDATE`/`DELETE ... WHERE` already collected target keys before writing, so a correlated
  WHERE sees the pre-statement table.
* **EXPLAIN** prints `Subquery #N correlated outer_row_input=true (re-executed per outer row; outer
  columns: col(tN.M), ...)` or `Subquery #N uncorrelated (executed once)`. The API never executes an
  EXPLAIN (`routes/sql.rs`); proven over HTTP with a statement that would error if executed.

#### Sub-slices

| | Semantics | Tests (engine / HTTP) |
|---|---|---|
| **B1** scope chain | innermost wins at every level; same alias at two levels; unknown at no level = `UnknownObject`; inner ambiguity/qualifier never escapes outward; types checked across the boundary; authorization at bind time for every level, identical to a missing table; 3-level capture propagation; aggregates/GROUP BY rules; ON/GROUP BY of an inner query may read outer columns | `correlated_tests::parser` 7, `::binder` 30 |
| **B2** correlated scalar | 0 rows -> NULL, 1 -> value, >1 -> runtime `EXECUTION_ERROR` naming the outer row (`(t.id = 10)`; PK added when correlating on another column; text escaped); aggregate over the inner scope only; FROM-less and empty-aggregate subqueries read the outer row; outer column not in the select list; WHERE/ORDER BY/HAVING; metric; both limits exact (N passes, N-1 fails); deadline | `::scalar` 17, `::diff` 3 (20 queries x random data x 20 seeds), `exec::subquery` 4 (gate unit tests), HTTP 1 |
| **B3** EXISTS / NOT EXISTS | exact complements; NULL in the inner WHERE or on the outer correlated column never makes EXISTS unknown; `EXISTS (SELECT COUNT(*) ...)` always true vs `HAVING`; value in SELECT list / CASE; short-circuits at the first inner row | `::exists` 10, `::diff_exists` 1, HTTP 2 |
| **B4** IN / NOT IN | full 3VL with a correlated inner set: NOT IN with a NULL in the inner set is **never TRUE** (dedicated test + `NOT EXISTS` contrast); NULL operand UNKNOWN unless the set is empty; UNKNOWN visible as SQL NULL; type mismatch is a bind error; per-evaluation set bounded by `max_materialized_rows` | `::in_subquery` 8 (incl. random 3VL differential on INTEGER and nullable TEXT), HTTP 1 |
| **B5** UPDATE / DELETE | correlated WHERE and SET; the subquery sees the pre-statement table and the row's pre-update values (self-correlated UPDATE/DELETE over duplicate groups; one-step shift; correlated column also assigned); failed/refused statement changes nothing; one budget across WHERE+SET | `::dml` 13, HTTP 2 |
| **B6** nesting | 3 levels (innermost reads both enclosing levels), scalar/EXISTS/IN, shadowed alias at level 3, nearest enclosing level wins, correlated-inside-uncorrelated and uncorrelated-inside-correlated (cache still used), exact per-level budget, innermost error names its enclosing row, UPDATE/DELETE; full-scale 60 x 222 x ~220 run | `::nested` 12, HTTP 1 |
| shapes | outer JOIN / LEFT JOIN (null-extended side), ORDER BY + LIMIT 1 per outer row, DISTINCT, `$1` parameter, grouped outer query, explicit transaction sees its own writes | `::shapes` 7 |

Totals: `correlated_tests` 108 + `exec::subquery` 4 = **112 engine tests**, **7 HTTP tests**
(`api_sql_integration`: scalar, exists, reader+unknown table, in, dml, explain, three-level), **11 corpus
statements** (corpus now 28, expected rows derived by hand from the seed data).

Fixture scale: the standard fixture is 60 outer rows (`t`) and 222 inner rows (`orders`; 206 matching,
10 orphans, 6 NULL `t_id`); the 3-level full-scale run adds ~220 `items`. The heaviest non-short-circuiting
3-level cases run on a compact 20/82/80 fixture (the evaluator evaluates both sides of an `AND`, so cost
is outer x orders x items; at full size they sit near the 30 s default deadline).

#### Non-vacuity -- the Slice-A-complete tree

There is no snapshot of the Slice A tree (nothing is committed), so it is reproduced by switching Slice B
off (`sql/mission1_evidence/sliceB_feature_off_spec.json`): outer columns invisible to a subquery,
`EmptyRelationOp` / empty-group `AggregateOp` dropping the outer row, `SET` subqueries refused. Every
Slice B test still compiles, so each failure is the feature's absence. Result
(`sliceB_feature_off.json`): **of the 120 new tests, 111 FAIL and 9 pass.** One failing test per sub-form:
scalar `scalar::a_correlated_max_matches_a_btreemap_join_for_every_outer_row`; EXISTS
`exists::correlated_exists_and_not_exists_match_a_btreemap_join_and_partition_the_outer_rows`; IN
`in_subquery::correlated_not_in_with_a_null_in_the_inner_set_never_returns_true`; UPDATE
`dml::update_where_a_correlated_exists_changes_exactly_the_matching_rows` and
`dml::set_with_a_correlated_scalar_subquery_matches_the_reference`; DELETE
`dml::delete_where_correlated_exists_not_exists_and_in_remove_exactly_the_matching_rows`; nested
`nested::three_levels_exists_where_the_innermost_reads_both_enclosing_levels`; corpus
`corpus_statements_return_exactly_the_expected_rows`; all 7 HTTP tests.

**Deviation from "every new test must fail" (9 tests, named):** 7 `parser::*` tests, `diff::the_model_groups_like_a_btreemap_join`
(a self-check of the reference model, no engine) and `binder::single_select_numbering_is_unchanged_by_the_scope_chain`.
The parser tests pin a grammar-level property that Slice B neither changed nor needed to (sqlparser already
accepted a correlated reference; the converter does not resolve names), plus two guard rows (`WITH` inside a
subquery, `LATERAL`) whose required outcome is "still rejected". The numbering test is an explicit
"unchanged" regression guard. Eight other tests that originally passed on the old tree (innermost-wins and
no-fall-through guards, the uncorrelated-metric test) were strengthened with a positive "twin" assertion that
fails there, and one (`a_text_value_in_the_error_message_cannot_inject_terminal_escapes`) was vacuous and was
rewritten to correlate on the text column and to assert the escaped value is present.

#### Mutation check -- 23 deliberate breakages, 23 caught

(`mutate_b.py`, `sliceB_mutations_spec.json` -> `sliceB_mutations.json`; each row is the number of tests, of
109 run, that fail.)

| id | breakage | failing tests |
|---|---|---|
| M01_outer_level_beats_inner | scope resolution reversed: a name the subquery defines itself is re-bound to an enclosing query's column (innermost no longer wins) | 7 |
| M02_not_in_null_is_false | NOT IN null trap broken: a non-member with a NULL in the inner set is FALSE instead of UNKNOWN | 5 |
| M03_outer_row_context_cleared | the correlated inner plan is built with an empty outer row context instead of the current outer row | 50 |
| M04_reexec_metric_never_incremented | correlated_subquery_reexecs is never recorded | 8 |
| M05_total_counter_never_advances | the statement-wide re-execution counter is never advanced, so max_correlated_reexecutions can never fire | 8 |
| M06_deadline_only_inside_inner_plan | no deadline/cancellation check between outer rows; only the inner plan's own operators check | 2 |
| M07_authorization_skipped_for_nested_queries | a nested (correlated) query is bound with an admin principal, so only the outermost FROM is authorization-checked | 3 |
| M08_capture_not_propagated_to_intermediate_levels | an outer column read two levels down is recorded only on the innermost subquery, so the middle subquery looks uncorrelated | 1 |
| M09_from_less_subquery_drops_outer_row | EmptyRelationOp discards the outer row | 3 |
| M10_empty_aggregate_group_drops_outer_row | the one group an aggregate emits over zero input rows carries no outer row | 2 |
| M11_primary_key_not_recorded_for_error_labels | the outer table's primary key is no longer recorded, so a runtime error cannot name the outer row by key | 2 |
| M12_set_evaluated_row_at_a_time | UPDATE ... SET (subquery) writes each row before the next row's subquery runs (no pre-statement snapshot) | 3 |
| M13_per_subquery_limit_not_enforced | max_subquery_outer_rows is never checked | 2 |
| M14_total_limit_not_enforced | max_correlated_reexecutions is never checked | 8 |
| M15_budget_not_shared_between_where_and_set | the SET phase starts its correlated-subquery budget from zero instead of continuing the WHERE phase's | 1 |
| M16_outer_columns_treated_as_ungrouped | an outer-query column beside an aggregate in a subquery's projection is rejected as ungrouped | 3 |
| M17_outer_only_aggregate_not_refused | an aggregate over only outer columns is silently evaluated over the inner rows | 1 |
| M18_join_condition_captures_dropped | an outer column used in an inner JOIN ... ON is not recorded as a correlation | 1 |
| M19_inner_ambiguity_falls_through_to_outer | an ambiguous name inside the subquery is silently re-resolved against an enclosing query | 1 |
| M20_correlated_result_hoisted_and_cached | a correlated subquery is treated as uncorrelated (evaluated once and cached) -- the hoisting the slice forbids | 58 |
| M21_error_does_not_name_the_outer_row_values | the multi-row error names the columns but not their values | 5 |
| M22_text_in_error_not_escaped | a text value in the error message is printed raw, so control characters reach the terminal | 2 |
| M23_explain_hides_the_correlation | EXPLAIN prints a correlated subquery without saying it is correlated | 1 |

The four mutations that exercise the guard tests (M01, M09, M10, M19) were re-run on the final test set
(`sliceB_mutations_recheck.json`): 7 / 3 / 2 / 1 failing.

#### Limits chosen (both reachable in tests, both `RESOURCE_LIMIT`, both exact at the boundary)

* `ExecLimits::max_correlated_reexecutions` = **1,000,000** per statement (all correlated subqueries
  together). Same figure as `max_materialized_rows` / `max_group_count`, the codebase's existing "about a
  million of these per statement" budget.
* `ExecLimits::max_subquery_outer_rows` = **100,000** per subquery (the number of enclosing rows allowed
  to drive one subquery; for a subquery nested in another correlated one, every evaluation across all the
  outer evaluations). One tenth of the statement budget, so a runaway subquery cannot starve the others.
* **Placement (D-5):** the mission asked for `sql/src/limits.rs` (`SqlLimits`). Both limits are *execution*
  limits enforced by the executor, which only receives `ExecLimits`; `SqlLimits` is the parse/bind limit set.
  They live next to their siblings (`max_materialized_rows`, `max_materialized_bytes`, `max_dml_target_rows`).

#### Regression (final tree)

| Command | Slice A | Slice B |
|---|---|---|
| `cargo test -p rubixdb-sql --lib` | 403 passed, 22 ignored | **515 passed**, 22 ignored (403 + 108 + 4) |
| `--lib subquery_tests` | 67 | 67 |
| `--lib mission1_corpus_tests` | 1 (17 statements) | 1 (28 statements) |
| `--lib bind_tests` / `exec_tests` / `plan_tests` | 62 / 63 / 43 | 62 / 63 / 43 |
| `--features test-util --test write_crash_consistency` (`-p rubixdb-sql`) | 2 | 2 |
| `cargo test -p rubixdb-api` | 228 passed, 1 ignored | **235 passed**, 1 ignored |
| `cargo clippy -p rubixdb-sql -p rubixdb-api --tests` | 0 | 0 |

#### Edits to existing tests (flagged; both are removals of a Slice A *limitation* pin, none weakened)

1. `subquery_tests::binder::an_outer_column_is_not_visible_inside_an_uncorrelated_subquery_scope` asserted
   `UnknownObject` for an outer reference -- Slice A's own placeholder for this slice (its comment says
   "correlation arrives in Slice B"). Replaced in place (renamed `..._is_visible_inside_a_subquery_scope_since_slice_b`):
   the reference must resolve to the enclosing `t` and be recorded; the guarantee it protected (never bind to
   the wrong table) is kept and extended in `correlated_tests::binder`.
2. `subquery_tests::binder::subqueries_fail_closed_in_clauses_that_cannot_run_them`: the case
   `UPDATE t SET name = (SELECT 'x') WHERE id = 1` was removed (with a comment) because Slice B implements
   subqueries in `SET`. The other four cases are unchanged. (Together with the Slice A edit to
   `parse_tests`, three pre-existing assertions have now been changed; each is the removal of an
   "unsupported" pin for a feature this mission was asked to add.)

#### Matrix

S10 and S12/S13 moved to IMPLEMENTED AND VERIFIED; new rows S10.1-S10.9 below. Correlation *inside a derived
table* is MISSING until Slice C (derived tables do not exist yet; the binder passes the enclosing chain to any
nested query, so it needs no new mechanism). `LATERAL` is **not** BLOCKED by grammar (sqlparser parses it;
`convert.rs` rejects it with a typed `Unsupported`, pinned by `parser::lateral_is_a_typed_unsupported_error_not_a_panic`);
it stays a Slice C decision. A CTE referencing an outer query is rejected (every `WITH` is `Unsupported` today,
pinned by `parser::a_cte_inside_a_subquery_is_refused_...`); Slice C must keep the rejection.

#### Open items / limitations (nothing here fails a test)

* No index-driven correlated lookup and no decorrelation, by instruction: each outer row re-runs the inner
  query with a scan; cost is O(outer x inner).
* The evaluator evaluates **both sides of `AND`/`OR`** (pre-existing): a correlated subquery to the right of
  a guard runs (and can raise its runtime error) for rows the guard already rejected. Logged in `OPEN_ITEMS.md`.
* `correlated_subquery_reexecs` is in `ExecMetrics`/`ExecMetricsSnapshot`; it is **not** added to the HTTP
  observability JSON (`/v1/metrics/system` `limits.sql_resource_limit_hits` is a documented, test-pinned
  document under the observability ADRs; extending it is a separate decision). Logged.
* `UPDATE`/`DELETE` have no alias syntax, so a correlated reference to the target must use the table name.
* `exec::execute` on a `Plan::Explain` runs the inner statement (the API never calls it that way).
  Pre-existing; logged.
* The DML statement's correlated budget covers the WHERE and SET phases together; a subquery in `SET`
  materializes the new rows (bounded) before writing.
* Mission text said a forbidden table is an `AUTHORIZATION_ERROR`; the implementation (and the mission's
  own "indistinguishable from does not exist" requirement) is `UnknownObject` -> HTTP 404 `NOT_FOUND` (D-5b).

**Recommended next slice: C (derived tables and non-recursive CTEs).** It reuses the relation-source
machinery and unlocks the one Slice B row left MISSING (correlation inside a derived table) and the LATERAL
decision, and removes the `WITH` case from `parse_tests` the same documented way.

### Slice C -- derived tables, LATERAL, non-recursive CTEs (C1 .. C6)

**Verdict: PASS WITH OPEN ITEMS.** Derived tables (with alias rule, column-name lists, nesting, joins,
GROUP BY / HAVING / ORDER BY / LIMIT inside), `[LEFT] JOIN LATERAL`, and non-recursive CTEs (single,
multiple, chained, referenced many times, zero times, shadowing, in subqueries / derived tables / DML) are
implemented end to end and verified (parser, binder, real-fixture execution, `POST /v1/sql`, randomized
differential tests, non-vacuity, 22 mutations). The open items are listed at the end; none is a failing test.

#### Baseline confirmed before editing (2026-10-11)

| Command | Result |
|---|---|
| `cargo test -p rubixdb-sql --lib` | 515 passed, 22 ignored |
| `--lib subquery` (`subquery_tests`) | 67 passed |
| `--lib correlated` | 109 passed (108 `correlated_tests` + 1 other name match) |
| `--lib mission1_corpus` | 1 passed (the prompt says 17 statements; it is 28 after Slice B) |
| `cargo test -p rubixdb-api` | 235 passed, 1 ignored |

#### Design

* **One relation mechanism, no second scope mechanism.** A derived table and a CTE reference are both
  *relations* whose rows come from an already-planned query. Binder: `bind::select::bind_from_item` pushes a
  synthetic `ScopeEntry` (columns built from the body's output: `scope::relation_columns`) into the same
  `Scope`, so name resolution, `SELECT *`, `t.*`, typing, nullability, LEFT JOIN null-extension and the
  aggregate/GROUP BY rules need no change. Planner/executor: leaf nodes `LogicalPlan::{Derived, Cte}` ->
  `PhysicalPlan::{Derived, CteNode}`; expression-level `BoundSubqueryExpr` is untouched (a subquery
  expression is a value, a relation is a row source -- so the prompt's "`derived_table=true` flag on
  SubqueryNode" was not used; the `Derived` node carries `outer_row_input` instead, shown in EXPLAIN).
* **The Slice B chain is reused, with one addition: a boundary.** `OuterScopes` gained `boundary:
  Option<Boundary>`. The body of an ordinary derived table or of a CTE is bound beneath a boundary link: a
  column that exists *only beyond it* is found (so the error can be precise) and refused --
  `derived table 'd' cannot reference outer column 't.id' without LATERAL` /
  `CTE 'x' cannot reference outer column 't.id'`. A name found nowhere is still `UnknownObject`. A `LATERAL`
  body is bound with no boundary and its own capture frame, so what it reads from the items before it is
  recorded exactly like a correlated subquery's `outer_refs`.
* **CTE names are lexical.** `Scope` carries `ctes` (defined so far), `pending_cte_names` (later in the same
  list) and `defining_cte` (the body being bound); `OuterScopes::lookup_cte` walks the same chain. A name
  resolves to a CTE (nearest enclosing `WITH` wins; an inner `WITH` shadows an outer one only inside) before
  the catalog; only an *unqualified* name can be a CTE (`public.orders` is always the table). Inside its own
  body a CTE's name is not visible: it means the catalog table if there is one (SQL scoping: `WITH orders AS
  (SELECT ... FROM orders ...)`), otherwise `CTE 'x' refers to itself; use WITH RECURSIVE`; a name defined
  later in the list gives `... is referenced before it is defined ...`.
* **Materialization rule.** A CTE is planned once (`CtePlan`, shared by `Arc` among its references) and its
  rows are materialized once per execution, at the first read, into `ExecCtx::relation_cache`; every
  reference serves from that copy, so all references see identical rows and the source tables are read once.
  A CTE nobody reads is bound and planned (a bad or forbidden definition is an error) but never run. The copy
  is bounded by `max_materialized_rows` and `max_materialized_bytes` and fails closed (`RESOURCE_LIMIT`);
  nothing is truncated and nothing falls back to re-execution. An ordinary derived table is streamed -- except
  on the right of a join (read once per left row), where it is marked `rescanned` and materialized once
  through the same cache. A `LATERAL` body that actually reads an earlier item runs once per left row through
  the Slice B correlated gate (deadline between rows, `max_correlated_reexecutions`,
  `max_subquery_outer_rows`, `correlated_subquery_reexecs`).
* **WITH on UPDATE / DELETE / INSERT.** sqlparser reads `WITH ... DELETE|UPDATE|INSERT` as a query whose body
  is the statement; the converter attaches the list to the statement. A DML target is always a catalog table
  (never a CTE). An `INSERT`'s VALUES cannot hold a subquery, so its CTE list is bound (errors/authorization)
  and dropped.
* **Decisions the prompt asked for.** (1) *Derived table without alias:* refused (`every derived table must
  have an alias`); nothing is invented. (2) *ORDER BY inside a derived table:* accepted and **applied** (never
  dropped), with or without LIMIT; the outer query makes no order promise unless it orders itself. (3) *LIMIT
  without ORDER BY:* accepted; at most LIMIT rows, which rows is unspecified. (4) *A column whose type cannot be
  determined (a bare `NULL`)* in a derived table / CTE is refused rather than guessed. (5) *Two columns with
  the same name:* allowed in the body; referring to such a name is `AmbiguousColumn`. (6) *Column-name list:*
  must name exactly as many columns as the body produces (both on `WITH x(a, b)` and `AS d(a, b)`); on a
  base-table alias it stays unsupported.
* **LATERAL -- decision: IMPLEMENTED** (not blocked: sqlparser has the grammar, and `NestedLoopJoinOp` already
  rebuilds its right side per left row with the left row's context). Supported spelling: `[LEFT] JOIN LATERAL
  (SELECT ...) alias ON cond`. `FROM t, LATERAL (...)` and `CROSS JOIN LATERAL` need comma / CROSS joins, which
  are out of the mission, and remain a typed `Unsupported` (pinned).
* **EXPLAIN.** `Derived #N alias=a as tK outer_row_input=false` (`(materialized once, re-read for each left
  row)` on a join's right side; `outer_row_input=true (LATERAL ...)` for a correlated lateral).
  `CteNode name=x as tK cte#N defined here (materialized once per statement)` prints the body once; every
  further reference prints `... (reads the rows materialized above)`.
* **Limits** (in `ExecLimits`, checked once before any work by `exec::check_relation_limits` over the finished
  plan: `plan::relstats`): `max_cte_definitions` 64 (counts every definition, referenced or not),
  `max_cte_references` 256, `max_derived_table_depth` 32. Derived-table nesting is also bounded at bind time
  by the existing subquery depth limit (`SqlLimits::max_subquery_depth`, 32), which protects the binder's own
  recursion. Nothing was added to `limits.rs`.
* **Metric.** `ExecMetrics::cte_materializations_total` (not exposed over HTTP, as in Slice B).

#### Sub-slices

| | Semantics | Tests |
|---|---|---|
| **C1** derived tables | names/types/nullability from the inner SELECT list; `SELECT *` / `d.*`; alias required and unique; column-name list (exact count); nested three deep; GROUP BY / HAVING / ORDER BY / LIMIT / DISTINCT inside; outer aggregate/join/LEFT JOIN over it; derived body holding subqueries and a derived table inside a correlated subquery; parameters; DML; bounds (`max_materialized_rows`/`_bytes`, depth) | `relation_tests::binder` 18 (C1+C2), `::derived` 15, `::parser` 8 |
| **C2** scope boundary + LATERAL | sibling / enclosing-query / enclosing-derived-table columns refused with the exact message; unknown stays unknown; LATERAL top-1 per row with INNER and LEFT; through the correlated gate (metric, both limits); an unused LATERAL is not correlated; LATERAL inside a correlated subquery; EXPLAIN flag | `::lateral` 6 (+ binder) |
| **C3** CTE, single reference | one materialization, metric; shadowing a base table; qualified names are never CTEs; zero-row and all-NULL bodies; body with subqueries; WITH before UPDATE/DELETE/INSERT | `::cte_binder` 15, `::cte` 17 |
| **C4** chained / multiple | earlier visible to later, forward reference and self reference are bind errors (message pinned), duplicate names, body cannot read outer columns | (in the above) |
| **C5** referenced more than once | one materialization for N references; the source table scanned once (compared with a one-reference run); read from a correlated subquery re-run 60 times: still one materialization; identical rows while another session writes the source table; EXPLAIN prints it once | `::cte`, `::materialization` 3 |
| **C6** WITH in subqueries / derived tables | scoped to the query that wrote it, never leaks; inner shadows outer only inside; inner may use an outer CTE; CTE inside LATERAL | `::cte_binder`, `::cte` |
| all | randomized comparison against independent models (derived table LEFT JOIN, twice-read CTE chain, outer aggregate, LATERAL top-1) | `::diff_relations` 2 |

Totals: `relation_tests` **84**, **2** new HTTP tests (derived tables + LATERAL + errors + depth limit;
CTEs + errors + 64/65-definition limit + WITH DELETE + EXPLAIN), **11** corpus statements (corpus now 39,
expected rows derived by hand), 3 existing tests edited (below), 2 `match` helpers in `plan_tests.rs` given the new
arms.

#### Non-vacuity -- the Slice-B-complete tree

Reproduced by switching Slice C off (`sql/mission1_evidence/sliceC_feature_off_spec.json`: the converter
refuses a derived table / LATERAL and every WITH list as `Unsupported`, exactly as before; every Slice C test
still compiles). Result (`sliceC_feature_off.json`): **all 87 new tests FAIL** -- the 84 `relation_tests`,
both new HTTP tests and the corpus -- plus the one Slice B test that Slice C had to replace. **No new test
passes on that tree.** (One guard test, `parser::what_is_not_in_scope_is_a_typed_unsupported_error`, first
passed there because it listed only refusals that existed before; it now also asserts that the non-recursive
forms parse, and fails there.) One failing test per required sub-form: derived table
`derived::group_by_having_order_by_and_limit_inside_a_derived_table`; CTE single
`cte::a_single_reference_cte_returns_the_body_rows_and_materializes_once`; chained
`cte::chained_ctes_see_earlier_ones_and_each_materializes_once`; twice-referenced
`cte::a_cte_referenced_twice_is_materialized_once_and_both_references_see_the_same_rows`; shadowing
`cte_binder::a_cte_shadows_a_base_table_of_the_same_name_for_the_whole_statement`; self-reference
`cte_binder::a_cte_referencing_itself_without_recursive_is_a_clear_bind_error`.

#### Mutation check -- 22 deliberate breakages, 22 caught

(`mutate_c.py`, `sliceC_mutations_spec.json` -> `sliceC_mutations.json`; the number is how many of the 140
tests run fail. M01-M10 are the prompt's suggested mutants: M01 materialization, M02/M03 first/last row, M04
catalog-first, M05 self-reference, M06 forward reference, M07 invented alias, M08 column names -- here the
column-name list being ignored, since an inner output name cannot be separated from an alias after binding --
M09 non-LATERAL sees outer, M10 reversed WITH order.)

| id | breakage | failing tests |
|---|---|---|
| M01_cte_re_executed_per_reference | CTE rows are not stored after the first read, so every reference re-runs the definition (the materialization rule broken) | 6 |
| M02_materialization_drops_the_first_row | the materialized rows lose their first row | 22 |
| M03_materialization_drops_the_last_row | the materialized rows lose their last row | 18 |
| M04_catalog_checked_before_the_cte_list | a name is looked up in the catalog first and only a miss consults the CTEs, so a CTE no longer shadows a base table | 4 |
| M05_self_reference_falls_through_to_the_catalog | a CTE's own name inside its body is no longer recognized as a self reference; it is looked up in the catalog (and fails there with a different error) | 1 |
| M06_forward_reference_not_recognized | a CTE defined later in the same WITH list is no longer recognized as a forward reference | 1 |
| M07_derived_alias_silently_invented | a derived table without an alias is given an invented one instead of being refused | 1 |
| M08_column_name_list_ignored | the relation exposes the body's own output names even when an AS alias(a, b) column list renames them | 3 |
| M09_non_lateral_derived_table_sees_outer_columns | an ordinary derived table is bound without a boundary, so it can read the query it sits in | 4 |
| M10_cte_list_bound_in_reverse_order | the WITH list is bound back to front, so the visibility order of CTEs is reversed (later ones visible to earlier ones, earlier ones not to later ones) | 4 |
| M11_materialized_byte_bound_removed | max_materialized_bytes is never checked while materializing a relation | 2 |
| M12_materialized_row_bound_off_by_one | the row bound admits one row too many | 1 |
| M13_materialization_metric_not_recorded | cte_materializations_total is never incremented | 8 |
| M14_cte_references_not_counted | CTE references are not collected, so max_cte_references can never fire | 1 |
| M15_unreferenced_ctes_not_counted | only CTEs the plan reads count toward max_cte_definitions | 1 |
| M16_derived_depth_not_incremented | derived-table nesting depth is always 0 | 1 |
| M17_join_right_derived_table_not_materialized | the right side of a join is not marked rescanned, so its derived table is re-run for every left row | 3 |
| M18_lateral_runs_outside_the_correlated_gate | a LATERAL derived table is re-run without the deadline check or the re-execution counters | 1 |
| M19_boundary_refusal_removed | a name found beyond a derived-table / CTE boundary is returned as a value instead of being refused | 6 |
| M20_duplicate_output_names_not_ambiguous | two columns with the same name in a derived table resolve silently to the first | 1 |
| M21_column_count_not_checked | an AS alias(a, b) / WITH x(a, b) list that does not match the body's column count is accepted | 2 |
| M22_explain_prints_the_cte_body_every_time | EXPLAIN prints a CTE's definition at every reference | 1 |

#### Regression (final tree)

| Command | Slice B | Slice C |
|---|---|---|
| `cargo test -p rubixdb-sql --lib` | 515 passed, 22 ignored | **599 passed**, 22 ignored (515 + 84) |
| `--lib subquery_tests` | 67 | 67 |
| `--lib correlated_tests` | 108 | 108 (one test replaced in place, one case removed -- below) |
| `--lib mission1_corpus_tests` | 1 (28 statements) | 1 (39 statements) |
| `--lib bind_tests` / `exec_tests` / `plan_tests` | 62 / 63 / 43 | 62 / 63 / 43 |
| `--features test-util --test write_crash_consistency` (`-p rubixdb-sql`) | 2 | 2 |
| `cargo test -p rubixdb-api` | 235 passed, 1 ignored | **237 passed**, 1 ignored |
| `cargo clippy -p rubixdb-sql -p rubixdb-api --tests` | 0 | 0 |

#### Edits to existing tests (flagged; each removes a pin on a limitation this slice lifts)

1. `parse_tests::unsupported_grammar_is_a_typed_error_not_a_panic`: the case `"WITH x AS (SELECT 1) SELECT * FROM x"`
   removed (with a comment), as announced in Slices A/B. The `UNION` case stays until Slice E.
2. `correlated_tests::parser::a_cte_inside_a_subquery_is_refused_so_it_cannot_reference_the_outer_query`
   (Slice B) asserted that any `WITH` is `Unsupported`. Replaced in place: a CTE body naming an enclosing
   query's column is still refused (now at bind: `CTE 'c' cannot reference outer column`), and a CTE that does
   not is accepted. The guarantee it protected is kept.
3. `correlated_tests::parser::lateral_is_a_typed_unsupported_error_not_a_panic` (Slice B): the case `JOIN LATERAL
   (SELECT 1) l ON true` removed because LATERAL is implemented; the comma spelling remains pinned as typed
   `Unsupported`.
4. `plan_tests.rs`: two helper functions with exhaustive `match`es over `PhysicalPlan` / `LogicalPlan` got the
   new variants added to their existing arms (no assertion touched).

#### Deviations from the prompt (stated honestly)

* *Corpus size:* the prompt says 17 statements; it was 28 after Slice B (and is 39 now).
* *Plan nodes:* `Derived` / `CteNode` leaves instead of a `derived_table=true` flag on `SubqueryNode` (a
  subquery expression is not a relation); documented above and in EXPLAIN.
* *Self-reference with a same-named base table* reads the table (SQL scoping) rather than erroring; the error
  appears when no such table exists. Chosen so that `WITH orders AS (SELECT ... FROM orders ...)` -- the common
  "filtered view" idiom -- keeps working. Flagging it because the prompt's wording ("self-reference ... is a bind
  error") can be read more strictly.
* *M08* mutates the column-name list, not the body's output names (see the note under the mutation table).
* *Limits* are enforced when execution starts (they live in `ExecLimits`, as instructed), so an over-limit
  statement is bound and planned first; bind-time recursion is separately bounded by the subquery depth limit.
* *`WITH` before `INSERT`* is accepted but can never be used (VALUES hold no subquery); it is bound for its
  errors and dropped.

#### Open items / limitations (nothing here fails a test)

* Set operations inside a derived table or a CTE body (`SELECT ... UNION ...`) are a typed `Unsupported`:
  BLOCKED until Slice E. `WITH RECURSIVE` is a typed `Unsupported`: Slice D.
* `FROM t, LATERAL (...)` and `CROSS JOIN LATERAL` are unsupported (comma / CROSS joins are out of the mission).
* A derived table or CTE column that is a bare `NULL` literal has no type and is refused.
* No predicate pushdown into, and no hoisting of, derived tables or CTEs (by instruction): `WHERE d.x = 1`
  filters above a fully produced derived table.
* A CTE that is read only on one branch of a never-taken path is never materialized (it is lazy), which is
  also why `max_cte_definitions` is checked up front rather than at materialization.
* `cte_materializations_total` is not surfaced over HTTP (observability document unchanged).

**Recommended next slice: E (set operations) before D.** `UNION [ALL]` is what a recursive CTE's seed/step
split is built from and it is the one remaining BLOCKED item in this slice (set-operation bodies in derived
tables and CTEs); Slice D then reuses the materialization and relation nodes added here.
