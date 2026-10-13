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
* **D-8** The Slice E prompt (a) quotes 235 API tests (it is 237 since Slice C); (b) says `INTERSECT ALL` of two
  identical relations "has twice the rows" -- by the SQL standard it is `min(c, c) = c` copies of each row, i.e. the
  relation itself, which is what is implemented and tested (`INTERSECT ALL` of `[1,1,2]` and `[1,1,1]` is `[1,1]`
  as the prompt's own worked example says); (c) describes `INTERSECT` as a join and `EXCEPT` as an anti-join but also
  forbids modelling a set operation as a join: they are implemented as hashed membership counting over the right
  operand, with no join operator; (d) asks to reuse the existing distinct machinery: `DistinctOp` is an O(n^2)
  `Vec` scan with its own bound and message (OPEN_ITEMS), so the set operations use a hash set under the same
  `max_materialized_rows` / `max_materialized_bytes` bounds; (e) asks for an "inner distinct flag" on the node: the
  node carries the operator and the `ALL` flag (distinct is `!all`).
* **D-9** The Slice D prompt (a) says a CTE defined before the recursive one is visible in its seed "but not its recursive
  term (SQL standard)" -- PostgreSQL, MySQL and Snowflake all make it visible in the term; the prompt is followed (a clear
  bind error), see R18; (b) asks to verify that a recursive CTE "sees the correlated scope from the enclosing query" --
  Slice C's boundary (pinned by a Slice C test) refuses any CTE body reading an outer column, so only a recursive CTE
  defined inside a correlated subquery whose *consumer* is correlated is supported (R8, R17); (c) wants window functions
  refused with a bind error -- they are refused by the parser as a typed `Unsupported`, after fixing a defect where the
  `OVER` clause was silently dropped (R14); (d) names the node `RecursiveCteNode(name, seed_plan, recursive_plan, distinct)`
  -- implemented with `cte_id` and `work_id` added; (e) suggests a snapshot-per-iteration mutant (M15), which cannot be a
  one-line breakage because every read shares the statement's one `ExecCtx`.

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
| C4 | `WITH RECURSIVE name AS (seed UNION ALL recursive) …` | IMPLEMENTED AND VERIFIED (Slice D: `recursive_tests` -- `counter`, `union`, `shape`, `contexts`, `diff`, corpus, HTTP `recursive_ctes_through_the_real_api`) | `convert.rs`, new recursive-CTE operator | all four | D (delivered) |
| C5 | Recursive CTE bounded by depth (default 1,000, configurable) | IMPLEMENTED AND VERIFIED (`max_recursive_cte_depth` 1,000, exact at the bound: `recursive_tests::limits`, HTTP `recursive_cte_errors_and_bounds_over_http`) | new limit in `exec::ExecLimits` | Executor | D |
| C6 | Recursive CTE bounded by total rows (default 1,000,000, configurable) | IMPLEMENTED AND VERIFIED (`max_recursive_cte_rows` 1,000,000, checked before a row is kept: `recursive_tests::limits`; the default is pinned, exhaustion is exercised at small configured values) | new limit in `exec::ExecLimits` | Executor | D |
| C7 | CTE shadowing a base table name | IMPLEMENTED AND VERIFIED (`relation_tests::cte_binder`, `::cte`; a qualified name is never a CTE) | `bind/scope.rs` name resolution order | Binder | C |
| S5.1 | Derived-table alias required and unique; column-name list `AS t(a, b)` (exact count) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::binder`, HTTP `derived_tables_through_the_real_api`) | `bind/select.rs::{bind_from_item, relation_output}` | Parser, Binder | C |
| S5.2 | Non-LATERAL derived table / CTE body cannot read outer or sibling columns (`derived table 'd' cannot reference outer column 'c' without LATERAL`) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::binder`, `::lateral`, `::cte_binder`) | `bind/scope.rs::{OuterScopes, Boundary}` | Binder | C |
| S5.3 | Derived table with GROUP BY / HAVING / ORDER BY / LIMIT / DISTINCT inside; ORDER BY inside is applied (decision), LIMIT without ORDER BY allowed | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::derived`) | `plan/logical.rs`, `exec/relation.rs` | Planner, Executor | C |
| S5.4 | Derived table in a join (right side materialized once, left side streamed); derived table in a subquery / correlated subquery / DML | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::derived`, `::materialization`) | `plan/physical.rs` (`rescanned`), `exec/relation.rs` | Planner, Executor | C |
| S5.5 | Set operation inside a derived table | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests::contexts::a_derived_table_can_be_a_set_operation_*`, HTTP) | `convert.rs` | Parser | E |
| C1.1 | CTE column-name list `WITH x(a, b) AS (...)` (exact count); reference rename `FROM x AS y(p, q)` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`) | `bind/select.rs::bind_with_list` | Binder | C |
| C1.2 | Self-reference without RECURSIVE: `CTE 'x' refers to itself; use WITH RECURSIVE` (a same-named catalog table is read instead, per SQL scoping) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`) | `bind/scope.rs::OuterScopes::lookup_cte` | Binder | C |
| C1.3 | Forward reference to a later CTE of the same list is a bind error | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`) | same | Binder | C |
| C1.4 | CTE referenced zero times: no error, never executed (still bound, so a bad / forbidden definition is an error) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte`, `::cte_binder`) | `plan/mod.rs::plan_ctes` | Binder, Planner | C |
| C1.5 | `WITH` before `UPDATE` / `DELETE` (usable) and `INSERT` (bound, never usable) | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::parser`, `::cte`, `::materialization`, HTTP) | `convert.rs::convert_query_statement`, `bind/dml.rs` | all four | C |
| C1.6 | `WITH` inside a scalar subquery / derived table is scoped to it and never leaks; inner shadows outer only inside | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte_binder`, `::cte`) | `bind/scope.rs` | Binder | C |
| C1.7 | CTE materialization bounded by `max_materialized_rows` / `_bytes`; `max_cte_definitions` 64, `max_cte_references` 256, `max_derived_table_depth` 32; metric `cte_materializations_total` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte`, `::derived`; HTTP 64/65 definitions) | `exec/mod.rs::check_relation_limits`, `plan/relstats.rs`, `exec/relation.rs` | Executor | C |
| C1.8 | CTE whose body is a set operation | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests::contexts::a_cte_body_can_be_a_set_operation`, `a_set_operation_cte_read_twice_is_computed_once_*`, HTTP; non-recursive only) | `convert.rs` | Parser | E |
| C1.9 | `EXPLAIN` shows a twice-read CTE once, flagged materialized; a derived table with `outer_row_input` | IMPLEMENTED AND VERIFIED (Slice C: `relation_tests::cte`, `::derived`, `::lateral`, HTTP) | `plan/explain.rs` | Planner | C |

### 2.3 Set operations

| # | Feature | Status | Where support lives | Gap | Slice |
|---|---|---|---|---|---|
| O1 | `UNION` | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests` -- `union`, `diff`, `contexts`, corpus, HTTP `all_six_set_operators_through_the_real_api`) | `convert.rs::convert_query` (`sp::SetExpr::SetOperation`), new `BoundSetOp`, `plan/logical.rs::LogicalPlan::SetOp`, new operator | all four | E |
| O2 | `UNION ALL` | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests` -- `union_all`, `diff`, `contexts`, corpus, HTTP) | same | all four | E |
| O3 | `INTERSECT` | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests` -- `intersect`, `diff`, `contexts`, corpus, HTTP) | same | all four | E |
| O4 | `INTERSECT ALL` | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests` -- `intersect` (min of the counts), `diff`, corpus, HTTP) | same | all four | E |
| O5 | `EXCEPT` | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests` -- `except`, `diff`, `contexts`, corpus, HTTP) | same | all four | E |
| O6 | `EXCEPT ALL` | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests` -- `except` (max(0, l - r), never negative), `diff`, corpus, HTTP) | same | all four | E |
| O7 | `INTERSECT` binds tighter than `UNION`/`EXCEPT` | IMPLEMENTED AND VERIFIED (the converter keeps sqlparser's tree: `setop_tests::parser`; executed on data that tells the parses apart: `intersect::intersect_binds_tighter_than_...`, `diff::three_branches_with_every_pair_of_operators...`, HTTP `set_operation_precedence_and_associativity_through_the_real_api`) | `convert.rs` | Parser | E |
| O8 | Output column names come from the first branch | IMPLEMENTED AND VERIFIED (`setop_tests::binder::column_names_come_from_the_first_branch`; an unaliased expression is named `?column?`, the engine's existing default) | `bind/select.rs` (new set-op binder) | Binder | E |
| O9 | Column-count and compatible-type check at bind time | IMPLEMENTED AND VERIFIED (column count, and the numeric widening table in section 6 / Slice E: `setop_tests::binder`; no implicit cast to TEXT or between non-numeric types) | `bind/` (new), uses `bind/expr.rs::check_assignable` rules (exact type match, D21) | Binder | E |
| O10 | NULL = NULL for set-operation distinctness | IMPLEMENTED AND VERIFIED (dedicated predicate `exec::setop::rows_not_distinct` + `SetKey`, not `=` and not `GroupingKey`: `setop_tests::distinctness`, `::union`, `::intersect`, `::except`) | new operator over `aggregate::GroupingKey` | Executor | E |
| O11 | `ORDER BY` / `LIMIT` / `OFFSET` over the whole set result (output-column references only) | IMPLEMENTED AND VERIFIED (output-column names only; a table column is `InvalidIdentifier`: `setop_tests::binder::order_by_*`, `::union_all`, `::union`) | `bind/` (new), `plan/logical.rs` | Binder, Planner, Executor | E |
| O12 | Set operation in a derived table, CTE body, scalar / `IN` / `EXISTS` subquery, `UPDATE` / `DELETE` `WHERE` and `UPDATE ... SET`; operands that are correlated, grouped, ordered, limited, derived, CTE references or contain subqueries | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests::contexts`) | `bind/setop.rs`, `plan/mod.rs::plan_select_subqueries` | all four | E |
| O13 | Bounded materialization: `max_materialized_rows` / `_bytes` for the rows a set operation holds; `max_set_op_branches` 32 | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests::union`, `::intersect`, `::except`, `::limits`, HTTP) | `exec/setop.rs`, `exec/mod.rs::check_relation_limits`, `plan/relstats.rs` | Executor | E |
| O14 | Metrics `set_op_rows_deduplicated_total`, `set_op_branches_total` (ExecMetrics only) | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests::union`, `::intersect`, `::except`, `::limits`) | `exec/mod.rs` | Executor | E |
| O15 | `EXPLAIN` shows `SetOp <operator> branches=N shape=OP(#1, OP(#2, #3))` | IMPLEMENTED AND VERIFIED (Slice E: `setop_tests::union_all`, `::union`, HTTP) | `plan/explain.rs` | Planner | E |
| O16 | `MINUS`, `UNION BY NAME`, `VALUES` / `TABLE` operands, `ORDER BY <ordinal>` | BLOCKED / not in scope (typed `Unsupported`; `ORDER BY 1` sorts by the constant 1 exactly as in a plain `SELECT`) | `convert.rs` | Parser | - |
| R1 | Recursive CTE, `UNION ALL`: seed once, term against the previous iteration only, stops on an empty iteration | IMPLEMENTED AND VERIFIED (Slice D: `recursive_tests::counter`, `::diff`, corpus, HTTP) | `exec/recursive.rs::run_fixpoint`, `plan/physical.rs::RecursiveCteNode` | all four | D |
| R2 | Recursive CTE, `UNION`: deduplicated against everything produced so far (null-safe); stops on no new distinct row; a cycle ends by itself | IMPLEMENTED AND VERIFIED (Slice D: `recursive_tests::union`, `::diff`) | same, `exec/setop.rs::SetKey` | Executor | D |
| R3 | In the term the CTE name is the working table, beating a catalog table of the same name | IMPLEMENTED AND VERIFIED (`recursive_tests::binder`, `::counter`) | `bind/scope.rs::lookup_cte` | Binder | D |
| R4 | The recursive reference appears exactly once, directly in the term's `FROM`, not on the nullable side of an outer join | IMPLEMENTED AND VERIFIED (`recursive_tests::binder`) | `bind/select.rs::bind_working_reference` | Binder | D |
| R5 | Term restrictions: no aggregate / GROUP BY / HAVING / ORDER BY / LIMIT / OFFSET; `DISTINCT` only under `UNION`; types equal the seed's | IMPLEMENTED AND VERIFIED (`recursive_tests::binder`) | `bind/recursive.rs::check_term` | Binder | D |
| R6 | Separator = the outermost `UNION [ALL]` of the body (seed on its left, term on its right) | IMPLEMENTED AND VERIFIED (`recursive_tests::shape`) | `bind/recursive.rs` | Binder | D |
| R7 | Several recursive CTEs in one `WITH`; ordinary CTEs before / after; a recursive CTE read many times runs once | IMPLEMENTED AND VERIFIED (`recursive_tests::contexts`) | `exec/relation.rs::open_cte`, `exec/recursive.rs::recursive_rows` | all four | D |
| R8 | Recursive CTE in a derived table, scalar / `IN` / `EXISTS` subquery, a correlated subquery's consumer, `WITH RECURSIVE` before `UPDATE` / `DELETE` | IMPLEMENTED AND VERIFIED (`recursive_tests::contexts`, HTTP) | same | all four | D |
| R9 | `EXPLAIN` shows `RecursiveCte` with the seed and the recursive term apart | IMPLEMENTED AND VERIFIED (`recursive_tests::observability`, HTTP) | `plan/explain.rs` | Planner | D |
| R10 | Metrics `recursive_cte_iterations_total`, `recursive_cte_rows_emitted_total` (ExecMetrics only) | IMPLEMENTED AND VERIFIED (`recursive_tests::counter`, `::observability`) | `exec/mod.rs` | Executor | D |
| R11 | `max_recursive_cte_definitions` 8 per statement | IMPLEMENTED AND VERIFIED (`recursive_tests::limits`, HTTP) | `exec/mod.rs::check_relation_limits`, `plan/relstats.rs` | Executor | D |
| R12 | Same snapshot in every iteration | IMPLEMENTED AND VERIFIED (`recursive_tests::contexts::the_recursion_reads_the_statements_snapshot_...`) | `exec/recursive.rs` (one `ExecCtx`) | Executor | D |
| R13 | Authorization of seed and term at bind time (404); the reference needs no grant | IMPLEMENTED AND VERIFIED (`recursive_tests::binder`, HTTP) | `bind/recursive.rs` | Binder | D |
| R14 | Window functions (`OVER`) refused (previously dropped silently) | IMPLEMENTED AND VERIFIED (`recursive_tests::parser`, HTTP) | `convert.rs::convert_function` | Parser | D |
| R15 | Multi-reference recursion (several references to the CTE in the term) | BLOCKED by decision: refused with "must appear exactly once in the recursive term" | `bind/select.rs` | Binder | - |
| R16 | Mutually recursive CTEs | BLOCKED by decision: bind error ("recursive CTEs that refer to each other are not supported") | `bind/scope.rs` | Binder | - |
| R17 | A recursive CTE body reading an outer column (correlated seed) | BLOCKED by the Slice C boundary (`CTE 'r' cannot reference outer column ...`) | `bind/scope.rs::Boundary` | Binder | - |
| R18 | An earlier CTE of the same list named inside the recursive term | REFUSED as the prompt specifies (other engines allow it; see D-9) | `bind/scope.rs::lookup_cte` | Binder | - |

## 3. Blocked / out-of-scope decisions

None yet. `LATERAL` (S11) is the one row that may become BLOCKED; it is decided in Slice C with
evidence, not assumed.

**Slice C decisions (2026-10-11):** `LATERAL` (S11) is **IMPLEMENTED**, not blocked -- sqlparser has the grammar
and the nested-loop join already re-runs its right side per left row; supported spelling `[LEFT] JOIN LATERAL
(...) alias ON cond`. BLOCKED until Slice E: set operations inside a derived table or a CTE body (S5.5, C1.8).
OUT OF SCOPE here, Slice D: `WITH RECURSIVE` (typed `Unsupported`).

**Slice E decisions (2026-10-12):** set operations are **IMPLEMENTED** (O1-O15), which lifts the two BLOCKED rows
S5.5 and C1.8. Still `Unsupported`: `MINUS`, `UNION BY NAME`, `VALUES` / `TABLE` operands (O16). `WITH RECURSIVE`
remains Slice D.

**Slice D decisions (2026-10-12):** `WITH RECURSIVE` is **IMPLEMENTED** (C4-C6, R1-R14), which completes Mission 1.
Refused by decision: multi-reference recursion (R15), mutual recursion (R16). Kept refused: a recursive CTE body
reading an outer column (R17, Slice C boundary), and an earlier CTE of the same list inside the term (R18).

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
| D — recursive CTEs | **DELIVERED** (report in section 6) | 2026-10-12 |
| E — set operations | **DELIVERED** (report in section 6) | 2026-10-12 |

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

### Slice E -- set operations (`UNION`, `INTERSECT`, `EXCEPT`, each with and without `ALL`)

Delivered 2026-10-12 on `master` @ `7a6ed9f` (the Slice A-C commit; the working tree was clean at the start),
uncommitted. Baseline re-confirmed before editing: `--lib` 599 passed / 22 ignored, `subquery` filter 115,
`correlated` 114, `relation` 89, corpus 1 test (39 statements), `rubixdb-api` 237 passed / 1 ignored.

**Implemented semantics.**

* **Shape.** A set operation is a `Select` whose `set_op` is `Some` (AST) and a `BoundSelect` whose `set_op` is
  `Some(BoundSetOp)` (bound): two whole operand queries, an operator, the `ALL` flag and an output relation
  (`set_ref`, a statement-unique table reference). Both are *additive fields* -- `Statement::Select`,
  `QueryExpr::Select`, `TableRef.derived`, `Cte.query` and every other existing shape are unchanged, so a set
  operation is accepted wherever a query is: top level, derived table, CTE body, scalar / `IN` / `EXISTS`
  subquery, `UPDATE` / `DELETE` `WHERE` and `UPDATE ... SET`. The plan node is `LogicalPlan::SetOp` /
  `PhysicalPlan::SetOp(SetOpNode)`; its output rows are exposed as the relation `set_ref`, so the set-level
  `ORDER BY` (a `Sort` above it) reads columns of that relation.
* **Not a join.** `UNION ALL` is a concatenation; `UNION` is concatenation + a hash-set distinct step;
  `INTERSECT` / `EXCEPT` count the right operand's rows (hash map of tuple -> count) and stream the left operand
  against it. `INTERSECT ALL` consumes one right copy per left copy (`min(l, r)`); `EXCEPT ALL` likewise
  (`max(0, l - r)`, never negative).
* **Precedence / associativity.** sqlparser builds the standard tree (`INTERSECT` 20, `UNION` / `EXCEPT` 10, left
  associative); the converter keeps its shape (left spine walked in a loop, so a long chain costs no native
  stack). `A UNION B INTERSECT C` is `UNION(A, INTERSECT(B, C))`; parentheses override.
* **NULL equality.** Rows are compared by a dedicated predicate (`exec::setop::rows_not_distinct`): `NULL` is not
  distinct from `NULL`, `NULL` is distinct from every value, `+0.0` / `-0.0` and all `NaN`s are one value, an empty
  tuple equals an empty tuple, values of different types are distinct. The hash-map key (`SetKey`) uses exactly that
  predicate as its `Eq`; a test checks it against `GroupingKey` and for hash consistency over a pool of 23 values
  (single-column, empty and two-column rows). The `=` operator is never used.
* **Column names and types.** Names come from the first operand (an unaliased expression is `?column?`, the
  engine's existing default). Types are unified per column by the widening table below; operand values are widened
  to the column type before any comparison.
* **`ORDER BY` / `LIMIT` / `OFFSET`** after the last operand apply to the whole result. `ORDER BY` can name the
  output columns (the first operand's names) and expressions over them, not a table column of any operand: a
  qualified name, a second-branch alias or a hidden column is `InvalidIdentifier` ("ORDER BY of a set operation can
  only name the output columns ..."). A parenthesised operand keeps its own `ORDER BY` / `LIMIT`. A `WITH` in front
  of the operation is visible in every operand.
* **Bounds.** `UNION ALL` holds nothing. `UNION` holds its distinct rows; `INTERSECT` / `EXCEPT` hold the right
  operand's distinct rows (and, for the distinct forms, the left rows already produced). All are bounded by
  `max_materialized_rows` (distinct rows) and `max_materialized_bytes`; exceeding either is `RESOURCE_LIMIT`, never
  a truncated result. `max_set_op_branches` (default **32**) bounds the operand queries one statement executes.
* **Metrics** (`ExecMetrics` only): `set_op_rows_deduplicated_total` (rows removed by the distinct step: for `UNION`
  rows equal to one already produced; for `INTERSECT` / `EXCEPT` qualifying left rows that repeat one already
  produced -- not the rows filtered for being absent from / present in the other side; `ALL` forms remove none),
  `set_op_branches_total` (operand queries, once per statement when it starts).

**Type-unification rule** (`bind::setop::unify_types`; symmetric and associative, tested over all pairs and
triples). Numeric types widen to the wider one; every other type combines with itself only; anything else is a bind
error (`TypeMismatch`, naming the column position and both types), never a silent cast, and never a cast to TEXT.

| left \ right | INTEGER | BIGINT | REAL | DOUBLE | DECIMAL(s2) | BOOLEAN, TEXT, BLOB, DATE, TIME, TIMESTAMP |
|---|---|---|---|---|---|---|
| **INTEGER** | INTEGER | BIGINT | DOUBLE | DOUBLE | DECIMAL(s2) | error |
| **BIGINT** | BIGINT | BIGINT | DOUBLE | DOUBLE | DECIMAL(s2) | error |
| **REAL** | DOUBLE | DOUBLE | REAL | DOUBLE | DOUBLE | error |
| **DOUBLE** | DOUBLE | DOUBLE | DOUBLE | DOUBLE | DOUBLE | error |
| **DECIMAL(s1)** | DECIMAL(s1) | DECIMAL(s1) | DOUBLE | DOUBLE | DECIMAL(max(s1, s2)) | error |
| **non-numeric X** | error | error | error | error | error | X with X only |

An untyped `NULL` (literal or `$n` parameter) adopts the other operand's type. Widening an exact number to DOUBLE
rounds a BIGINT / DECIMAL above 2^53; widening to a DECIMAL that overflows 128 bits is an execution error
(`EXECUTION_ERROR`), never a wrapped value. Rationale for REAL: widening an exact integer to a 32-bit float loses
digits, so REAL with an exact numeric goes to DOUBLE.

**Sub-slices** (each tested before the next; the counts are tests in `sql/src/setop_tests.rs`, 129 in all):

| Sub-slice | Semantics | Tests added |
|---|---|---|
| E1 parser | `ast::SetOperator` / `SetOperation`, `Select.set_op`; `convert_query` / `convert_set_operation` (left spine walked in a loop; chain bounded by `max_expression_depth`); `MINUS`, `BY NAME`, `VALUES` / `TABLE` operands refused | `parser` 13: six operators, `DISTINCT` = default, precedence and associativity (`UNION(A, INTERSECT(B, C))`), parentheses, **the two parses evaluated over A={1,2} B={2,3} C={3,4} give {1,2,3} vs {3}**, trailing `ORDER BY` / `LIMIT` belong to the whole operation, a parenthesised operand keeps its own, `WITH` in front, contexts, a 100-chain parses and a 400-chain is `RESOURCE_LIMIT` |
| E2 binder | `bind/setop.rs`: column count, `unify_types`, first-branch names, nullability, set-level `ORDER BY` against the output relation only, `LIMIT` / `OFFSET`, inherited `WITH`, authorization through the ordinary operand binding | `binder` 24: count mismatch for every operator and chain position, all 36 numeric pairs through the real binder and via `unify_types` (symmetric, associative), every non-numeric type against every other, NULL adoption, names, `?column?`, `ORDER BY` accepted / refused cases, a forbidden-or-missing table in branch 2 of 4 (and in a derived table, CTE, subquery) is `UnknownObject`, contexts, DML, correlation, a 100-chain binds |
| E3 `UNION ALL` | planner `LogicalPlan::SetOp` / `PhysicalPlan::SetOp(SetOpNode)`, `exec/setop.rs` streaming concatenation, value widening, `EXPLAIN`, metric `set_op_branches_total`, `max_set_op_branches` | `union_all` 16: rows, left-then-right order, identical / disjoint / empty branches, `LIMIT` less / equal / greater, `ORDER BY`, own-`LIMIT` operands, 3-4 branches, NULLs kept, schema and widened values (DECIMAL rescale, BIGINT, DOUBLE; overflow is an error), aggregates / joins / derived tables as operands, **streams: `LIMIT 3` reads < 50 of 1000 rows and never opens the right operand**, metrics, `EXPLAIN` |
| E4 `UNION` | hash-set distinct under the dedicated predicate; first-occurrence order; bounded | `distinctness` 7 (predicate, `SetKey` vs `GroupingKey` and hash consistency, reflexive / symmetric / transitive), `union` 16: NULL tuples ({1,NULL} x2 -> 1 row, {1,NULL} vs {1,0} -> 2, {NULL,NULL} x2 -> 1), identical / disjoint / empty, after-dedup `ORDER BY` / `LIMIT`, 3 branches and mixed with `ALL`, equal-after-widening values collapse, -0.0, text case, dedup counter (4 of 6), row / byte bounds |
| E5 `INTERSECT[ ALL]` | right operand counted once (hash map), left streamed; `ALL` consumes one right copy per left copy | `intersect` 14: the `[1,1,2]` / `[1,1,1]` worked example, per-value counts, identical / disjoint / empty, NULL tuples, widening, 3 branches, **precedence on distinguishing data (executed)**, right operand scanned once (2 scans), dedup counter, bound |
| E6 `EXCEPT[ ALL]` | same machinery; `EXCEPT` remembers produced rows, `EXCEPT ALL` counts down and stops at zero | `except` 12: `[1,1,2] - [1]` (= `[1,2]` ALL, `[2]` distinct), no underflow (`[1,1,2] - [1,1,1]`), identical / disjoint / empty right and left, NULL tuples and counts, left-associativity, `A EXCEPT B UNION C`, widening, dedup counter, bounds. `diff` 4: every operator x 30 random one-column multisets, 30 two-column multisets with NULLs, all 36 operator pairs x parenthesisation x 6 random triples against an independent counting model, and the contract's examples against model *and* engine |
| E7 contexts | derived table / CTE body / scalar / `IN` / `EXISTS` / `UPDATE` / `DELETE` use the same node; no new mechanism | `contexts` 17: derived (outer `WHERE`, join, nested, column list, non-`LATERAL` refused, `LATERAL` accepted), CTE body, the `UNION ALL` separator shape, **a CTE read twice: 1 materialization, 2 scans, 2 operands counted**, a dead CTE, scalar 0 / 1 / many rows (+ correlated; the error names the outer row), `IN` with NULL (UNKNOWN, not FALSE) for UNION / INTERSECT / EXCEPT, `EXISTS`, `DELETE` / `UPDATE` `WHERE` and `SET` (a failing `SET` changes nothing), operand shapes, all four slices mixed in one statement, parameters, a wrong-typed parameter, a forbidden table. `limits` 6: 32 ok / 33 refused for four operators, per statement across subqueries / CTE / derived, DML held to the limit, metric semantics, a 120-chain, deadline |

Corpus: 18 hand-derived statements added (57 in all). HTTP: 5 tests added
(`all_six_set_operators_through_the_real_api`, `set_operation_precedence_and_associativity_through_the_real_api`,
`set_operations_in_derived_tables_ctes_and_subqueries_through_the_real_api`, `set_operation_errors_and_limits_over_http`,
`explain_of_a_set_operation_shows_the_precedence_and_executes_nothing_over_http`).

**Changed files (lines added / removed, `git diff --numstat`; new files by `wc -l`).** New: `sql/src/exec/setop.rs`
530, `sql/src/bind/setop.rs` 318, `sql/src/setop_tests.rs` 3399, `sql/mission1_evidence/mutate_e.py` 73 plus specs and
results. Modified: `ast.rs` +65, `convert.rs` +142/-41, `bound.rs` +33, `bind/select.rs` +19, `bind/env.rs` +9,
`bind/scope.rs` +3/-1, `bind/mod.rs` +1, `plan/logical.rs` +54/-3, `plan/explain.rs` +67, `plan/mod.rs` +11,
`plan/relstats.rs` +13, `plan/optimize.rs` +10/-4, `plan/physical.rs` +9/-1, `plan/validate.rs` +8,
`exec/mod.rs` +53/-5, `exec/operators.rs` +2, `exec/write.rs` +1/-1, `lib.rs` +3,
`mission1_corpus_tests.rs` +88, `parse_tests.rs` +7/-2, `plan_tests.rs` +5/-1, `relation_tests.rs` +4/-2,
`api/tests/api_sql_integration.rs` +390/-1.

**Design choice (extend, do not add a second mechanism).** The set operation is an additive field on the existing
`Select` / `BoundSelect`, so `Statement::Select`, `QueryExpr`, derived tables, CTEs and subqueries accept it without a
parallel type. Its operands are bound by the same `bind_select_in` against the same enclosing-query chain
(correlation, authorization and boundaries are inherited, not re-implemented). Its output is a relation exactly as a
derived table's is, so the Slice C materialization (a CTE body that is a set operation runs once) and the
`ORDER BY` / `LIMIT` machinery apply unchanged. The only new engine pieces are the binder rules (`bind/setop.rs`) and
the operator (`exec/setop.rs`).

**Limit values and why.**

| Limit | Value | Why |
|---|---|---|
| `ExecLimits::max_set_op_branches` | 32 | operand queries one statement executes, counted per statement over every set operation (an operand that is itself directly a set operation is expanded, so `A UNION B INTERSECT C` is 3; a CTE read twice counts once; a CTE nobody reads, not at all). Each operand is a full plan that runs (and, for the distinct forms, holds rows), so the count bounds the work a small statement can request; 32 equals `max_subquery_depth`, is far above hand-written queries and keeps the left-nested tree shallow. Checked when execution starts: `RESOURCE_LIMIT` (HTTP 413), also for `UPDATE` / `DELETE` |
| rows a set operation holds | `max_materialized_rows` (1,000,000, existing) | distinct rows for `UNION`; the right operand's distinct rows (plus produced left rows for the distinct `EXCEPT` / `INTERSECT`); duplicates do not count |
| bytes a set operation holds | `max_materialized_bytes` (256 MiB, existing) | the same per-value size estimates as `GROUP BY` / `DISTINCT` |
| chain length | `max_expression_depth` (128, existing) | refuses a longer `UNION` chain at conversion; no native recursion along the chain |

**Non-vacuity.** (1) *Real Slice-C tree*: `git archive HEAD` (7a6ed9f) with only the new corpus and HTTP test files
copied in, own target directory: the 18 new corpus statements fail (`corpus_statements_return_exactly_the_expected_rows`
reports "18 corpus statement(s) failed"), the 5 new HTTP tests fail, the 39 existing HTTP tests pass. `setop_tests.rs`
itself does not compile there (`SetOperator`, `unify_types` and `exec::setop` do not exist). (2) *Feature-off
simulation* (`sliceE_feature_off.json`; the converter refuses a set operation as `Unsupported`, all new code still
compiled): 152 tests run, 125 fail. The 27 that pass are 18 existing `parse_tests` plus 9 new tests: `binder::unify_types_agrees_...`
and the 6 `distinctness::*` tests exercise only new pure functions with no SQL path (on the real Slice-C tree they fail
by non-compilation), and `diff::the_model_agrees_with_the_documented_examples`, a self-test of the reference model that
asserted nothing about the engine. Per the "strengthen or remove" rule that test was **strengthened after the run**: it
is now `the_model_and_the_engine_agree_with_the_documented_examples` and also runs the contract's examples through the
engine, so it fails in the feature-off tree (not re-run through the harness; it fails there by construction since
`SELECT ... UNION ...` is `Unsupported`).

**Mutation evidence** (`sliceE_mutations.json`; 34 deliberate breakages, each applied alone to the Slice E tree, the
filtered suite run -- 182 tests: `setop_tests`, corpus, `parse_tests`, `relation_tests::cte` / `::materialization` --
and the source restored byte-for-byte; **34 of 34 caught**, at least one test fails for each). The 12 the prompt
suggested are M01-M12: M08 re-associates a right operand that is a bare set operation (INTERSECT as loose as UNION),
M09 sorts by the first column for an unknown name, M11 drops an output column (exposed to every derived table / CTE /
subquery alike, since they share one mechanism), M12 is the Slice C materialization broken. The harness ran before
two small edits (the strengthened model test, and the runtime widening error class `PlanValidation` ->
`ExecutionParameter`, with a test); neither touches a mutated line.

| id | breakage | failing tests (of 182 run) |
|---|---|---|
| M01_union_behaves_as_union_all | UNION keeps duplicates (no deduplication): the distinct kind is mapped to the concatenation operator | 28 |
| M02_union_all_behaves_as_union | UNION ALL deduplicates | 15 |
| M03_intersect_behaves_as_except | INTERSECT produces what EXCEPT would | 21 |
| M04_intersect_all_treated_as_intersect | INTERSECT ALL returns each common row once (a distinct count, not the minimum count) | 8 |
| M05_except_all_counts_go_negative | EXCEPT ALL keeps decrementing a right-hand count that is already zero (an unsigned count wrapping below zero), so every later copy of the row is removed | 7 |
| M06_distinctness_treats_null_as_different_from_null | the tuple-distinctness predicate says NULL and NULL are different values (the standard trap) | 16 |
| M07_column_names_from_the_second_branch | the output columns are named after the right operand | 9 |
| M08_intersect_has_the_same_precedence_as_union | the converter re-associates a right operand that is itself a set operation (what sqlparser built for a tighter-binding INTERSECT) to the left, so all three operators bind alike | 8 |
| M09_order_by_resolves_unknown_names_to_the_first_column | ORDER BY of a set operation, given a name that is not an output column, silently sorts by the first output column instead of failing | 1 |
| M10_unification_falls_back_to_text | two types with no common type unify to TEXT instead of being refused | 3 |
| M11_set_operation_output_loses_a_column | the shell's output columns skip the first operand column (the relation exposes the wrong column set to a derived table / CTE / subquery) | 93 |
| M12_cte_set_operation_rerun_per_reference | a CTE whose body is a set operation is not stored after its first read, so every reference re-runs it (the Slice C materialization rule broken) | 7 |
| M13_operand_values_not_widened | operand values keep their own types instead of being widened to the unified column type | 7 |
| M14_decimal_widening_does_not_rescale | widening to a larger DECIMAL scale keeps the unscaled digits (1.00 becomes 0.0100) | 3 |
| M15_union_dedup_not_counted | set_op_rows_deduplicated_total is never incremented by UNION | 1 |
| M16_branches_metric_not_recorded | set_op_branches_total is never recorded | 4 |
| M17_operand_limit_off_by_one | the operand limit refuses a statement that is exactly at the limit | 3 |
| M18_row_bound_removed | the materialized-rows bound on a set operation's held rows is removed | 3 |
| M19_byte_bound_removed | the materialized-bytes bound on a set operation's held rows is removed | 2 |
| M20_intersect_does_not_deduplicate_the_left | INTERSECT returns a left row once per left copy that is on the right (the distinct-left step removed) | 10 |
| M21_except_does_not_deduplicate_the_left | EXCEPT remembers nothing about rows it has produced, so a repeated left row repeats in the result | 7 |
| M22_union_all_reads_only_the_right_operand | UNION ALL starts in the right phase and never reads the left operand | 42 |
| M23_decimal_scale_unifies_to_the_smaller | DECIMAL(s1) with DECIMAL(s2) unifies to the smaller scale | 4 |
| M24_real_with_exact_numeric_unifies_to_real | REAL with INTEGER / BIGINT / DECIMAL unifies to REAL (losing precision) instead of DOUBLE | 4 |
| M25_column_count_not_checked | operands with different column counts are combined (zip truncates) | 3 |
| M26_nullability_and_instead_of_or | an output column is nullable only if both operands can produce NULL | 1 |
| M27_nan_is_distinct_from_nan | the predicate treats two NaN doubles as different values | 3 |
| M28_hash_does_not_canonicalise_zero_signs | the hash of REAL +0.0 and -0.0 differs although the values compare equal (hash/eq inconsistency) | 1 |
| M29_explain_prints_a_wrong_branch_count | EXPLAIN shows branches=2 for every set operation | 1 |
| M30_set_level_limit_ignored | LIMIT / OFFSET written after a set operation are dropped | 8 |
| M31_set_level_with_not_inherited | the WITH list in front of a set operation is not visible in its operands | 3 |
| M32_set_level_order_by_dropped | the ORDER BY written after a set operation is not applied | 7 |
| M33_intersect_all_is_not_consumed | INTERSECT ALL does not consume the right-hand count when it matches a left copy (every left copy of a common row survives) | 8 |
| M34_right_operand_counted_once_per_left_row_check | the right operand's rows are all treated as one copy (count never exceeds 1) | 9 |

**Regression** (final tree).

| Check | Before | After |
|---|---|---|
| `cargo test -p rubixdb-sql --lib` | 599 passed, 22 ignored | 728 passed, 22 ignored (599 + 129 `setop_tests`) |
| `subquery_tests::` / `correlated_tests::` / `relation_tests::` | 67 / 108 / 84 | 67 / 108 / 84 (unchanged) |
| `bind_tests` / `exec_tests` / `plan_tests` | 62 / 63 / 43 | 62 / 63 / 43 |
| `mission1_corpus` | 1 test, 39 statements | 1 test, 57 statements |
| `write_crash_consistency` (`--features test-util`) | 2 | 2 |
| `cargo test -p rubixdb-api` | 237 passed, 1 ignored | 242 passed, 1 ignored (`api_sql_integration` 39 -> 44) |
| `cargo clippy -p rubixdb-sql -p rubixdb-api --tests` | 0 warnings | 0 warnings |

(The prompt's name filters `subquery`, `correlated`, `relation` are substring matches and now also pick up Slice E
tests whose names contain those words: 119 / 116 / 98.)

**Existing tests edited (flagged; each pinned `UNION` as unsupported).**
`parse_tests::unsupported_grammar_is_a_typed_error_not_a_panic` lost its `UNION` case (comment added; the removal
announced in Slices A-C). `relation_tests::parser::what_is_not_in_scope_is_a_typed_unsupported_error` lost its two
set-operation cases (comment added; they were the Slice C BLOCKED rows S5.5 / C1.8).
`api_sql_integration::unsupported_grammar_is_a_stable_error_not_silently_accepted` now submits `... RIGHT JOIN ...`
instead of `... UNION ...` (comment added; same 400 `UNSUPPORTED` assertions). `plan_tests.rs`: two exhaustive helper
`match`es got the `SetOp` arm (no assertion changed). `api_http_fuzz` still submits a `UNION` statement as a hostile
input; it asserts only that the server survives. No other Slice A / B / C test was touched.

**Known limitations / outside the slice (named, not fixed).** `ORDER BY <ordinal>` is not an ordinal (it sorts by the
constant, as in a plain `SELECT`); `MINUS`, `UNION BY NAME`, `VALUES` / `TABLE` operands are `Unsupported`; no predicate
pushdown into operands; `UNION ALL` is unbounded by design (it streams); the metrics are not in the HTTP document (by
instruction) and an `UPDATE` / `DELETE` statement's executor metrics are discarded, as for every Slice B / C metric; set
operations in a CTE nobody reads are neither counted nor run; `SELECT NULL AS n UNION SELECT NULL ORDER BY n` is refused
(no type to compare); a `$n` parameter of a type the column cannot take is an execution error; `PHASE_RELATIONAL_SQL_GRAMMAR.md`
still lists `UNION` as outside the grammar (historical document, left as written).

**Recommended next slice: D (`WITH RECURSIVE`).** `UNION ALL` / `UNION` now exist as the seed / step separator and the
CTE materialization, bounds and scope chain are in place; D adds the iteration operator and its depth / row limits.

### Slice D -- `WITH RECURSIVE`

Delivered 2026-10-12 on `master` @ `7a6ed9f` (Slices A-C; Slice E, delivered earlier the same day, is still
uncommitted underneath), uncommitted. **This completes the five slices of Mission 1.** Baseline re-confirmed before
editing: `--lib` 728 passed / 22 ignored, `subquery` filter 119, `correlated` 116, `relation` 98, `setop` 129,
corpus 1 test (57 statements), `rubixdb-api` 242 passed / 1 ignored.

**Implemented semantics.**

* **What recurses.** A definition of a `WITH RECURSIVE` list that names itself must have the body `seed UNION [ALL]
  recursive-term`. The **outermost** set operator is the separator (found from the parse tree, not guessed):
  `A UNION ALL B UNION ALL C` is `(A UNION ALL B) UNION ALL C`, so the seed is `A UNION ALL B` and the term is `C`;
  `A UNION ALL (B UNION ALL C)` has the seed `A` and a term that is itself a set operation. A member that does not
  name itself is an ordinary CTE (`WITH RECURSIVE x AS (SELECT 1)` works). `Cte.recursive` records the keyword; the
  binder decides what really recurses.
* **Plan node.** `PhysicalPlan::RecursiveCte(RecursiveCteNode { name, cte_id, work_id, distinct, seed_plan,
  recursive_plan })` is the root of the CTE's `CtePlan`; the term's reference is the leaf `WorkingTable`. It is never
  a flat `UNION`, and it is **not** the Slice C materialization: the fixpoint owns a working table and its own result
  cache (`exec/recursive.rs`); it shares with the non-recursive kind only bind-time name resolution and
  `CtePlan` / `CteNode` bookkeeping. A recursive CTE read many times (outer query, later CTEs, derived tables,
  subqueries) runs its fixpoint once per statement.
* **The fixpoint** (`run_fixpoint`): the seed runs once; then, while the working table is not empty, the term is
  evaluated against the **previous iteration's rows only**. `UNION ALL`: the next working table is exactly what the
  term produced; the loop ends when an iteration produces **no row**. `UNION`: rows are deduplicated by the null-safe
  `SetKey` against **everything produced so far** (seed included); the loop ends when an iteration produces **no new
  distinct row**, so a cyclic graph ends by itself, while the same cycle under `UNION ALL` runs into the depth bound.
* **Name resolution.** In the seed the CTE's own name is an error ("must not appear in the seed"), as is a later CTE of
  the list ("referenced before it is defined ... recursive CTEs that refer to each other are not supported"); in the
  term the name is the **working table** and beats a catalog table of the same name (structural). It must appear
  **exactly once** (message: "recursive reference 'r' must appear exactly once in the recursive term"), as a table of
  the term's own `FROM` clause (not inside a subquery, derived table or nested `WITH`), and not on the nullable side
  of an outer join. A CTE of the same list defined **before** the recursive one is visible in its seed but not
  inside its term; a CTE of an enclosing query and every catalog table stay visible; a CTE defined **after** is
  visible only to later definitions and the main query.
* **Restrictions on a term.** A `SELECT` block that reads the working table may not aggregate, `GROUP BY`, use
  `HAVING`, `ORDER BY`, `LIMIT` or `OFFSET`, and may use `DISTINCT` only under `UNION` (under `UNION ALL` it is
  refused); a set operation containing the reference may not carry its own `ORDER BY` / `LIMIT`; the trailing
  `ORDER BY` / `LIMIT` / `OFFSET` after the term are refused; the seed may do all of these. Blocks of the term that
  never read the working table are just more seeds. The term's column **types must equal the seed's** (an untyped
  `NULL` adopts them; Slice E's widening is deliberately not applied, because the term feeds its own input). All
  columns of a recursive CTE are nullable. A `WITH` list inside the body is `Unsupported`.
* **Window functions** are not in the grammar. While testing this restriction I found that the converter **silently
  dropped** an `OVER (...)` clause (`SUM(id) OVER ()` ran as the plain aggregate `SUM(id)`, `ROW_NUMBER() OVER ()` as an
  unknown function). It is now a typed `Unsupported` error everywhere. This is a behaviour change outside the
  recursion (a statement that used to return a wrong answer now fails), made because the slice's rule is "do not
  silently accept window functions"; `M28` reverts it and is caught.
* **Authorization.** Seed and term bind independently against the catalog: a forbidden or missing table in either is
  `UnknownObject` (HTTP 404), in the same shape; the working-table reference needs no grant.
* **Snapshot.** Every iteration reads through the statement's single `ExecCtx` / pinned transaction, so a base table
  written by another session during the recursion is never seen by a later iteration (test: a 300-row chain walked
  while a writer extends it; the walk always equals the chain's size at the statement's snapshot).
* **EXPLAIN** prints `RecursiveCte name=r distinct=false (UNION ALL: stops when ...)` with the indented blocks
  `seed (runs once):` and `recursive (runs once per iteration, reads the working table 'r'):`, and
  `WorkingTable name=r` under the latter; a recursive CTE read twice is printed once; a set operation inside the term
  appears inside the recursive block. The seed/recursive split is never hidden behind a `SetOp`.

**Limits** (`ExecLimits`, all reachable in tests; each fires as `RESOURCE_LIMIT`, HTTP 413, naming the CTE, never a
truncated result and never a half-computed one):

| Limit | Default | Counted | Why |
|---|---|---|---|
| `max_recursive_cte_depth` | 1,000 | evaluations of the recursive term, per CTE (not shared), the last, empty one included: `n < 1000` passes, `n < 1001` fails | a hand-written recursion (counter, hierarchy, graph walk) ends in tens of iterations; a missing stop condition must end in a typed error, not a hang |
| `max_recursive_cte_rows` | 1,000,000 | rows added to the result of one recursive CTE, the seed included; checked **before** a row is kept (`>` bound exact: 20 pass, 21 fail at limit 20) | the codebase's usual "about a million rows" budget (`max_materialized_rows`) |
| `max_recursive_cte_definitions` | 8 | distinct recursive CTEs a statement runs (one nobody reads is bound and planned but not run, so not counted); checked when execution starts, for queries and for `UPDATE` / `DELETE` | each owns a working table and a result |
| `max_materialized_bytes` | 256 MiB (existing) | estimated bytes of the rows held | a few huge `TEXT` rows must not defeat the row bound |

The two stop conditions (`UNION ALL`: no row; `UNION`: no new distinct row) and the bounds are the only things that
end a recursion; the loop has no other exit and every bound is checked before the work it bounds.

**Metrics** (`ExecMetrics` only): `recursive_cte_iterations_total` (term evaluations, across every recursive CTE in the
statement, incremented before each) and `recursive_cte_rows_emitted_total` (rows added to results, seed included; under
`UNION` counted after deduplication; a recursion that fails still reports the work it did).

**Sub-slices** (each tested before the next; counts are tests in `sql/src/recursive_tests.rs`, 82 in all):

| Sub-slice | Semantics | Tests added |
|---|---|---|
| D1 parser | `ast::Cte.recursive`; `convert_with` no longer refuses `RECURSIVE`; the `OVER` guard | `parser` 6: the flag on every definition, **a plain `WITH` parses exactly as before** (AST equal once the flag is cleared), every in-scope form parses, window functions refused (+ twin), `OVER` refused outside any recursion, `MATERIALIZED` / typed columns still refused (+ twin) |
| D2 binder | `bind/recursive.rs`, `Scope.{working,hidden_ctes,seed_phase,recursive_keyword}`, `CteLookup::{Working,HiddenInRecursiveTerm,SelfInSeed,SelfNotShaped,ForwardInRecursiveList}`, `BoundRelation::Working`, `BoundCte.recursion` | `binder` 16: shape and separator, names and types, a non-recursive member, **working table beats the catalog**, seed reference, exactly-once (3 spellings), position (subquery / derived / scalar / nullable side), aggregate / GROUP BY / HAVING / DISTINCT / ORDER BY / LIMIT / OFFSET in the term and after it, DISTINCT only under UNION, un-splittable bodies, seed-vs-term types, visibility of earlier / later / enclosing CTEs, **mutual recursion**, several and dead recursive CTEs, **authorization** (seed, term, missing, shape, working table needs no grant), the Slice C boundary |
| D3 `UNION ALL` | `exec/recursive.rs` fixpoint, `WorkingTable` operator, bounds | `counter` 13: **counter (1..10)**, **factorial (last row 10!, overflow is a typed error)**, **tree walk**, **ancestor chain**, join to a base table, **seed aggregate runs once**, seed once for many iterations, **empty seed**, **empty term**, working table beats the catalog table, subqueries in the term, outer clauses; `limits` 11: runaway counter names the CTE, depth exact at 1000/1001 and at small values, row bound exact and for a wide recursion, bytes, definitions (8/9, dead not counted, configurable), per CTE not shared, refused `UPDATE` / `DELETE` change nothing, deadline, defaults |
| D4 `UNION` | `SetKey` dedup against all rows so far; own stop condition | `union` 10: counter, **cycle ends by itself** (and the same cycle under `UNION ALL` hits the bound), the two stop conditions side by side, dedup against everything so far, **NULL rows are one row**, a diamond (7 rows `UNION ALL` / 5 `UNION`), seed duplicates, whole rows not first columns, `DISTINCT` under `UNION` |
| D5 shape | outermost-separator selection | `shape` 5: three operands, parenthesised right operand (outer operator decides), seed that is a set operation, `INTERSECT` / `EXCEPT` term, set operation inside a term branch |
| D6 contexts | derived tables, subqueries, DML, several CTEs, snapshot | `contexts` 12: several recursive CTEs (7 iterations), different operators, ordinary CTE over a recursive one (materialized once), **read twice: one fixpoint**, dead CTE, visibility, derived table, scalar / `IN` / `EXISTS`, **correlated consumer (5 outer rows, 3 iterations in all)**, `WITH RECURSIVE` before `DELETE` / `UPDATE` and inside their subqueries, set operations and `ORDER BY` / `LIMIT` around it, **snapshot while another session writes** |
| D7 observability | `EXPLAIN`, metrics, limits, authorization | `observability` 5: seed and term apart, set operation inside the term, printed once when read twice, metrics across statements, partial work reported on failure; authorization is in `binder` |
| differential | independent working-list loop | `diff` 4: 25 random DAGs under `UNION ALL` and 25 random graphs with cycles under `UNION` against the model (rows, iterations, rows emitted), a cycle stopped by the bound, 15 random trees |

Corpus: 12 hand-derived statements added (69 in all). HTTP: 3 tests added (`recursive_ctes_through_the_real_api`,
`recursive_cte_errors_and_bounds_over_http`, `recursive_cte_explain_and_a_failed_recursion_inside_a_transaction_over_http`).

**Changed files** (new: `sql/src/exec/recursive.rs` 196, `sql/src/bind/recursive.rs` 330, `sql/src/recursive_tests.rs`
1821, `sql/mission1_evidence/mutate_d.py` 73 + specs / results. Modified, Slice D share, approximate because the working
tree also carries Slice E: `ast.rs` +8, `convert.rs` +10 / -7, `bound.rs` +23, `bind/scope.rs` +85, `bind/select.rs`
+130, `bind/env.rs` +5, `bind/mod.rs` +1, `plan/logical.rs` +13, `plan/physical.rs` +38, `plan/optimize.rs` +4,
`plan/validate.rs` +10, `plan/explain.rs` +27, `plan/relstats.rs` +9, `plan/mod.rs` +36, `exec/mod.rs` +60,
`exec/operators.rs` +15, `exec/relation.rs` +15, `lib.rs` +3, `plan_tests.rs` +4, `mission1_corpus_tests.rs` +61,
`api/tests/api_sql_integration.rs` +340; `setop_tests.rs` helper visibility only.)

**Non-vacuity.** (1) *Real Slice-C tree* (`git archive HEAD` + the new corpus and HTTP test files; Slice E is
uncommitted so there is no committed Slice-E tree): 30 corpus statements fail (the 12 `[D]` ones among them; the other
18 are Slice E's) and the 3 new HTTP tests fail (with Slice E's 5 and the amended `non_recursive_ctes_...`); `recursive_tests.rs`
does not compile there. (2) *Feature-off simulation* of the Slice-E tree (`sliceD_feature_off.json`: the converter refuses
`WITH RECURSIVE` as before): 113 tests run, **85 fail**. The 28 that pass are 26 pre-existing tests and 2 new ones: `limits::the_defaults_are_the_documented_values`
(pure constants of the new fields; it does not compile on the real tree) and `parser::an_over_clause_is_refused_everywhere_not_silently_dropped`
(tests the `OVER` fix, which is independent of the keyword; on the real tree it fails -- `M28` reverts the fix and is caught). Three
tests that first passed vacuously there (a window-function test and two grammar tests that were satisfied by `WITH RECURSIVE` itself being refused) were
**strengthened with twins** (the same statement without the refused spelling must parse) and now fail there.

**Mutation evidence** (`sliceD_mutations.json`; 34 deliberate breakages, each applied alone, the 141-test filtered
suite -- `recursive_tests`, corpus, `parse_tests`, `relation_tests::cte` / `::parser` -- run, the source restored byte for
byte). **33 of 34 caught.** The suggested M1-M14 are M01-M14; M15 ("fresh snapshot per iteration") **cannot be written as
a one-line breakage**: every read goes through the statement's one `ExecCtx`, which has no way to open another snapshot,
so the property is carried by the concurrent-writer test instead and the slot is used for M15 (rows metric ignores the
iterations). **M34 survived and is equivalent**: it removed the cleanup of the working table on an error path, which is
unobservable (the statement ends and its context is dropped); that code was **deleted** rather than kept unverifiable
(`outcome?;` with a comment), and M15, whose anchor text changed, was re-run (`sliceD_mutations_recheck.json`, caught,
10 failing).

| id | breakage | failing tests (of 141 run) |
|---|---|---|
| M01_fixpoint_does_not_stop_on_an_empty_iteration | the loop no longer ends when an iteration produces nothing (a query that should stop early runs to the depth bound) | 48 |
| M02_union_compares_only_the_first_column | the UNION stop condition / deduplication keys a row by its first column only | 2 |
| M03_union_treats_null_as_different_from_null | the null-safe distinctness predicate says NULL and NULL differ, so a row with a NULL is new every iteration | 2 |
| M04_depth_bound_off_by_one | the depth bound admits one iteration too many | 7 |
| M05_row_bound_off_by_one | the row bound admits one row too many | 1 |
| M06_seed_re_executed_every_iteration | the seed plan is run again at the start of every iteration | 2 |
| M07_term_also_reads_the_current_iterations_output | after an iteration the term is evaluated once more against the rows that iteration just produced, within the same iteration | 42 |
| M08_working_table_accumulates_all_rows_so_far | the term reads every row produced so far instead of only the previous iteration's | 40 |
| M09_recursive_reference_resolves_to_the_catalog_table | when a catalog table has the CTE's name, the recursive reference reads the catalog table instead of the working table | 24 |
| M10_two_references_silently_accepted | a second reference to the recursive CTE in the term is accepted | 1 |
| M11_aggregate_in_the_term_silently_accepted | aggregates, GROUP BY and HAVING in the recursive term are accepted | 2 |
| M12_order_by_in_the_term_silently_accepted | ORDER BY in the recursive term is accepted | 1 |
| M13_explain_prints_the_recursion_as_a_flat_union | EXPLAIN labels the recursive CTE as a set operation | 3 |
| M14_iteration_counter_not_incremented | recursive_cte_iterations_total is never incremented | 23 |
| M15_rows_metric_ignores_the_iterations | recursive_cte_rows_emitted_total counts the seed only | 10 |
| M16_rows_metric_ignores_the_seed | recursive_cte_rows_emitted_total omits the seed rows | 11 |
| M17_union_treated_as_union_all | the planner ignores the distinct flag: a UNION recursion keeps duplicates and never sees a cycle close | 13 |
| M18_union_all_treated_as_union | the planner always deduplicates: a UNION ALL recursion drops repeated rows | 10 |
| M19_definitions_bound_not_enforced | max_recursive_cte_definitions is never checked | 1 |
| M20_byte_bound_removed | the bytes held by a recursion are not bounded | 1 |
| M21_earlier_ctes_visible_in_the_term | a CTE defined before the recursive one is visible inside the recursive term | 1 |
| M22_seed_may_name_the_cte | the CTE's own name in its seed is not recognised as a seed reference | 1 |
| M23_nullable_side_reference_accepted | the recursive reference may be on the nullable side of an outer join | 1 |
| M24_reference_inside_a_subquery_accepted | the recursive reference may sit inside a subquery or derived table of the term | 1 |
| M25_seed_and_term_swapped_in_the_plan | the planner plans the term as the seed | 58 |
| M26_seed_term_type_check_removed | the term's column types are not compared with the seed's | 1 |
| M27_recursive_result_not_cached | a recursive CTE read more than once runs its fixpoint once per reference | 5 |
| M28_over_clause_dropped_again | the converter drops an OVER clause again (a window function silently runs as the plain function) | 2 |
| M29_recursion_routed_through_the_materialization_node | a recursive CTE reference is materialized by the non-recursive CTE path | 56 |
| M30_default_depth_changed | the default max_recursive_cte_depth is 100 | 6 |
| M31_default_row_bound_changed | the default max_recursive_cte_rows is 100,000 | 1 |
| M32_default_definitions_changed | the default max_recursive_cte_definitions is 9 | 2 |
| M33_rows_emitted_not_checked_before_keeping | the row bound is checked after the row is kept, not before | 1 |
| M34_working_table_left_after_an_error | the working table is not removed when the fixpoint fails (leaks into later reads) (equivalent: the code was deleted) | 0 |

**Regression** (final tree).

| Check | Before | After |
|---|---|---|
| `cargo test -p rubixdb-sql --lib` | 728 passed, 22 ignored | 810 passed, 22 ignored (728 + 82 `recursive_tests`) |
| `subquery_tests::` / `correlated_tests::` / `relation_tests::` / `setop_tests::` | 67 / 108 / 84 / 129 | 67 / 108 / 84 / 129 (unchanged) |
| `bind_tests` / `exec_tests` / `plan_tests` | 62 / 63 / 43 | 62 / 63 / 43 |
| `mission1_corpus` | 1 test, 57 statements | 1 test, 69 statements |
| `write_crash_consistency` (`--features test-util`) | 2 | 2 |
| `cargo test -p rubixdb-api` | 242 passed, 1 ignored | 245 passed, 1 ignored (`api_sql_integration` 44 -> 47) |
| `cargo clippy -p rubixdb-sql -p rubixdb-api --tests` | 0 warnings | 0 warnings |

(The prompt's name filters `subquery`, `correlated`, `relation`, `setop` are substring matches and also pick up
tests of later slices whose names contain those words: 119 / 116 / 98 / 129 before this slice.)

**Existing tests edited (flagged; each pinned `WITH RECURSIVE` as unsupported).**
`relation_tests::parser::what_is_not_in_scope_is_a_typed_unsupported_error` lost its `WITH RECURSIVE` case (comment
added; the list of one became a single assertion to keep clippy clean). In
`api_sql_integration::non_recursive_ctes_through_the_real_api` the block asserting `WITH RECURSIVE r AS (SELECT 1 AS n)`
is `UNSUPPORTED` now asserts that it runs and returns 1 (comment added; the other assertions are untouched). `plan_tests.rs`: two exhaustive helper `match`es
got the new arms (no assertion changed). `setop_tests.rs`: helper functions made `pub(crate)` so `recursive_tests` can
use them (no assertion changed). No other Slice A-E test was touched.

**Deviations from the prompt, stated honestly.**

1. **Earlier CTEs and the recursive term.** The prompt says a CTE defined before the recursive one is visible in its
   seed "but not its recursive term (SQL standard)". That is not what PostgreSQL, MySQL or Snowflake do (an earlier CTE
   is visible in the term, a common pattern). I implemented the prompt as written: naming an earlier CTE of the same
   list inside the term is a clear bind error. It is one line to relax (`hidden_ctes`, M21).
2. **A recursive CTE reading an outer column.** The prompt asks to verify that "the recursive CTE sees the correlated
   scope from the enclosing query". Slice C's boundary (pinned by a Slice C test) refuses any CTE body, recursive or not,
   that reads an outer column: `CTE 'r' cannot reference outer column 't.id'`. That is kept. What works, and is tested:
   a recursive CTE *defined inside* a correlated subquery whose consumer reads the outer row; the consumer re-runs per
   outer row (5 rows), the recursion, which cannot depend on that row, runs once (3 iterations in all).
3. **Window functions** are refused by the parser (`Unsupported`, HTTP 400 `UNSUPPORTED`), not by the binder, and the
   dropped-`OVER` defect described above was fixed for every statement.
4. **Type rule** is equality with the seed, not Slice E's widening, and all recursive columns are nullable (see above).
5. **Extra restrictions** beyond the prompt: the reference must be directly in the term's `FROM` (not inside a
   subquery / derived table), not on the nullable side of an outer join; `OFFSET` is refused with `LIMIT`; a `WITH`
   inside a recursive body is `Unsupported`.
6. **The default row bound (1,000,000) is not run to exhaustion** (it takes tens of seconds in a debug build); the
   bound is exercised at configurable small values, including a wide recursion with a small depth, and the default is
   pinned by `the_defaults_are_the_documented_values` and mutants M30-M32.
7. **Mutants:** M15 (snapshot) is not expressible as a one-line breakage and M34 was equivalent (above).
8. The prompt's `RecursiveCte(name=cte, seed=<plan>, recursive=<plan>)` EXPLAIN spelling is rendered as an indented
   block with `seed (runs once):` / `recursive (...):` headings.
9. `max_set_op_branches` does not count the `UNION [ALL]` that separates a seed from its term (it is not a `SetOp` node).

**Known limitations / outside the slice (named, not fixed).** Multi-reference recursion and mutually recursive CTEs
stay refused by design (matrix rows R15, R16); no predicate pushdown into a term; the working table is an in-memory
`Vec` per iteration (bounded, never on disk); `recursive_cte_*` metrics are in `ExecMetrics` only and an `UPDATE` /
`DELETE` statement's executor metrics are discarded, as for every Slice B-E metric; an unreferenced recursive CTE is
bound and planned but never run.

**Mission 1 is complete (Slices A-E).** **Recommended next: Mission 2** (the next family in the programme; Mission 1
leaves no BLOCKED or MISSING row except the ones named above as decisions).
