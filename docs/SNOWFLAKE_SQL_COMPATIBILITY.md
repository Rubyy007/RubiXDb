# rubiXDb — SQL & Query Engine Audit (Phase 0 of 6, read-only)

Document: `docs/SNOWFLAKE_SQL_COMPATIBILITY.md` · Date: 2026-10-14 · Base commit: `dc5dc44ae7ccad2de0dc32955974653bbf0ebfd6` (branch `master`)
Status of this document: **audit only**. Nothing was implemented, nothing was committed, no source, test or configuration file was modified.
This is **not** a claim of Snowflake compatibility. Where rubiXDb matches PostgreSQL or the SQL standard instead of Snowflake, §12 says so.

## 0. How to read this document

**Status vocabulary (exactly one per row):**

| Status | Meaning in this document |
|---|---|
| `IMPLEMENTED_AND_TESTED` | Code exists, executes against a real table, and at least one executable test pins the behaviour. The test names are cited. Every cited suite was run in this audit (§2) unless the row says otherwise. |
| `IMPLEMENTED_NOT_FULLY_VERIFIED` | Code exists; a named part of the behaviour has no test, or the only test is at a lower level than the behaviour (stated in the row), or the suite that covers it was not run in this audit. |
| `PARTIALLY_SUPPORTED` | Works for a stated subset; the rest is refused or absent (stated in the row). |
| `NOT_IMPLEMENTED` | No working code. Usually the converter refuses the syntax with a typed `UNSUPPORTED` error. The row says whether a test pins that refusal. |
| `BLOCKED_BY_ARCHITECTURE` | Cannot be built correctly on the current storage / transaction / catalog design without an architectural change. The row names the change. |
| `NOT_PLANNED` | A recorded decision not to build it (ADR or mission decision). The decision can be reversed by the maintainer. |

**Evidence rules.** Source and tests in the working tree are authoritative. Old documents were treated as leads only (§3 lists where they disagree with source). "Pinned by a test" means an executed test asserts the behaviour; a refusal that exists only in `convert.rs` and is asserted by no test is called **unpinned**.

**Path shorthand.** `S/` = `sql/src/`, `E/` = `src/` (core crate `rubixdb`), `A/` = `api/src/`. `R1`…`R16` = reference list in §11.

## 1. Pre-flight record

| Item | Value |
|---|---|
| `git branch --show-current` | `master` |
| `git rev-parse HEAD` | `dc5dc44ae7ccad2de0dc32955974653bbf0ebfd6` |
| `git log -5 --oneline` | `dc5dc44 frontend code morning` · `de336da frontend: use the rubiXDb X mark as the browser tab icon` · `2b70de8 frontend design commit` · `1dadb4c frontend: SQL IDE editing, Schema design, theme switching, catalog refresh, RAM card` · `dde8ae9 sql: Mission 1 Slices D/E (WITH RECURSIVE, set operations) and Mission 2 (window functions, advanced aggregation)` |
| Uncommitted changes at start | 22 modified files + 3 untracked files, all from the durability mission (D1b/D1c): root docs (`CHANGELOG.md`, `OPEN_ITEMS.md`, `PROGRESS.md`), `docs/*` durability documents, `missions/ACTIVE.md`, `Cargo.toml`, `src/{lsm,manifest,ops,persist,wal}`, `tests/{pending_fixes.rs,sim_power_loss}`, plus untracked `docs/adr/ADR-DATA-DIR-DURABILITY-01.md`, `docs/adr/ADR-TORN-TAIL-LOST-SECTOR-01.md`, `tests/dir_durability.rs`. |
| Overlap with this mission | **None at start.** This mission writes one new file (`docs/SNOWFLAKE_SQL_COMPATIBILITY.md`). At 21:22 no uncommitted change touched `sql/`, `api/`, `cli/` or `frontend/`, so the SQL source audited and tested here is the committed source of `dc5dc44`. The STOP condition did not apply. |
| **Concurrent change observed during the audit** | Between 21:31 and 21:35 — after all three test runs in §2 had finished (21:28-21:30) — files under `sql/` and `api/` became modified by something other than this audit: `sql/src/{ast,convert,error}.rs`, `sql/src/bind/{mod,scope}.rs`, `sql/src/exec/write.rs`, `api/src/{error,lib}.rs`, `api/src/routes/{catalog,sql}.rs`, and the new untracked `api/src/sql_hints.rs`. Read from `git diff`, the change adds a `USE SCHEMA` session statement, an `AlreadyExists` error class, friendlier `CREATE DATABASE` wording and parse-error "did you mean" hints. It touches none of areas A–M (the only `convert.rs` change is a new `sp::Statement::Use` arm; the only `write.rs` change is the `CREATE DATABASE` message). I did not edit these files, did not audit them, and classify nothing about them; all evidence and every row above refer to `dc5dc44`. Whoever owns that work should treat the line numbers cited in this document (`convert.rs:…`) as valid for `dc5dc44` only. |
| `docs/PROJECT_STATE.md` | Present. States: not production ready; protected paths `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`. |
| `missions/ACTIVE.md` | Present. Its SQL Mission 1 and Mission 2 sections are marked "delivered, awaiting maintainer review". It names no active mission. |
| Protected paths | See §14. `src/wal/`, `src/manifest/` and `Cargo.toml` **already carry uncommitted diffs from the durability mission before this audit began**. This audit adds zero bytes to them (SHA-256 of their combined `git diff` was recorded before and compared after, §14). |

## 2. Evidence executed in this audit

All runs used `--locked` (so `Cargo.lock` cannot change). No test was edited.

| Command | Result |
|---|---|
| `cargo test -p rubixdb-sql --lib --locked` | **979 passed, 0 failed, 22 ignored** (328 s). The 22 ignored tests are benchmarks (`index_read_benchmark::*`, `pk_range_benchmark::*`, `inc17_overhead_bench::*`, `inc18_compare_bench::*`); they were not run and nothing below relies on them. |
| `cargo test -p rubixdb-api --locked --test api_sql_integration --test api_sql_mission2` | **47 + 13 = 60 passed, 0 failed** |
| `cargo test -p rubixdb-sql --locked --features test-util --test write_crash_consistency` | **2 passed, 0 failed** |

Passing tests per module in the SQL lib run (counted from the output): `setop_tests` 129, `correlated_tests` 108, `window_tests` 101, `relation_tests` 84, `recursive_tests` 82, `subquery_tests` 67, `exec_tests` 63, `bind_tests` 62, `aggregation_tests` 59, `write_tests` 44, `plan_tests` 43, `parse_tests` 18, `limits_tests` 12, `cost_model_tests` 12, `security_tests` 11, `fuzz_tests` 10, `txn_scan_tests` 8, `index_snapshot_tests` 8, `index_read_differential_tests` 7, `materialization_tests` 6, `window_reference_model` 5, `aggregate_reference_model` 5, `mission1_corpus_tests` 1, `mission2_corpus_tests` 2, others < 10.

**Not executed in this audit** (rows that depend only on these say so): the frontend test suites (`frontend/src/**/*.test.*`, Playwright), the `rubixdb-cli` integration tests, the other `rubixdb-api` test binaries, the core-crate tests (`src/relational/*_tests.rs`, `src/catalog/tests.rs`), and every benchmark. No live server was started: starting one would create instance state outside the repository, which a read-only audit should not do. Refusal behaviour that is not pinned by an executed test was therefore established **from source** (file and function cited) and is labelled "unpinned".

## 3. Documents that disagree with source (recorded, not repaired)

`CLAUDE.md`: current source wins; conflicts are reported. None of these changes a classification; each is a stale statement.

| # | Document | Says | Source says |
|---|---|---|---|
| C-1 | `PHASE_RELATIONAL_SQL_GRAMMAR.md` §12 addendum table, row "Window functions" | "still refused, now reliably" | Window functions are implemented (Mission 2, `S/exec/window.rs`, 101 `window_tests` pass). The addendum predates Mission 2. |
| C-2 | `PHASE_RELATIONAL_DATABASE_ADR.md` D3 / D31 | `schema_version` per row "is the enforced rule"; "a reader must never silently misinterpret an old-schema-version row"; `ALTER TABLE ADD COLUMN` is "correct and instantaneous (a catalog-only change)" | The version is written (`E/relational/table_store.rs:834`) but **never read**: `decode_row` (`E/relational/value.rs:116`) discards the returned version and decodes against the *current* column list. D3's backward-decoding is a design intent, not an implemented property. See H-1. |
| C-3 | `PHASE_RELATIONAL_DATABASE_ADR.md` D31 | `DROP COLUMN` / type changes "are parsed and bound … but rejected at execution time" | `ALTER TABLE` is rejected in the **converter** (`S/convert.rs:204-218`, falls to the "not in the approved grammar subset" arm); nothing is parsed into an AST node. |
| C-4 | `S/ast.rs` doc comments on `Statement` variants | "PARSED, BOUND. NOT EXECUTABLE YET" | Every one of those statements executes (`S/exec/mod.rs`, `S/exec/write.rs`). Stale since Increment 12. |
| C-5 | `sql/MISSION2_FEATURE_MATRIX.md` §6.1 | 5 lib tests and 1 API test "fail by design" | In `dc5dc44` the full lib run has 0 failures and the two API binaries pass; the six tests were updated after that matrix was written. The matrix's counts are historical. |
| C-6 | `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §(joins) and ADR D18 | Hash and Merge Join "explicitly deferred", to be revisited "once Increment 10 has a working baseline" | Increments 10-18 are complete; the revisit never happened. No hash/merge join exists (J-3, J-4). Consistent with source, but the stated trigger has passed. |

## 4. Feature matrix

Columns: **ID · Feature · Status · Implementation · Tests (evidence) · Limitations · Dependencies · Remaining work · Ref**.
Where a row says "refused", the typed error is `SqlError::Unsupported` → HTTP 400 `UNSUPPORTED` (`A/error.rs`; pinned generically by `api_sql_integration::unsupported_grammar_is_a_stable_error_not_silently_accepted`).

### Area A — Aggregation

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| A-1 | `COUNT(*)`, `COUNT(x)`, `SUM`, `AVG`, `MIN`, `MAX` with `GROUP BY` / `HAVING` | IMPLEMENTED_AND_TESTED | `S/aggregate.rs` (`AggregateFunc`, `AggregateState`), `S/exec/operators.rs` `AggregateOp` (hash map keyed by `GroupingKey`, insertion-ordered output), `S/bind/select.rs` | `aggregation_tests` (59), `exec_tests::null_semantics_full_matrix_count_sum_avg_min_max`, `aggregate_reference_model` (5, independent model), `aggregate::tests` | Exactly these five aggregates (A-12). Groups bounded by `max_group_count` (1,000,000) and `max_aggregate_state_bytes` (256 MiB); excess is `RESOURCE_LIMIT`, never truncation. | `GroupingKey` | none | R15 |
| A-2 | `COUNT(DISTINCT x)` | IMPLEMENTED_AND_TESTED | `AggregateAccumulator` (seen-set) in `S/aggregate.rs`; per group and per grouping set | `aggregation_tests::exec_2f::count_distinct_per_group`, `distinct_is_per_group_not_across_groups`, `distinct_on_a_null_only_column_and_on_empty_input`, `distinct_treats_negative_zero_and_zero_as_one_value`; reference model | Seen-set counts against `max_aggregate_state_bytes`. `-0.0 = +0.0`, all NaN equal. `NULL` ignored. | A-1 | none | R15 |
| A-3 | `SUM/AVG/MIN/MAX(DISTINCT x)` | IMPLEMENTED_AND_TESTED | same | `sum_and_avg_distinct`, `every_standard_aggregate_accepts_distinct`, `distinct_in_having_and_order_by_and_alongside_plain_aggregates` | same | A-2 | none | R15 |
| A-4 | Several `DISTINCT` aggregates over **different** columns in one `SELECT` | IMPLEMENTED_NOT_FULLY_VERIFIED | one accumulator and seen-set per aggregate (`S/aggregate.rs`) | No test uses two different columns in one statement: `aggregation_tests.rs:1045-1072` and `aggregate_reference_model.rs:501` use one column (`x` or `d`). | Behaviour follows from the per-aggregate design but is unpinned. | A-2 | Add a differential test (two distinct columns, grouped, with NULLs). | R15 |
| A-5 | `COUNT(DISTINCT a, b)` (multi-expression distinct) | NOT_IMPLEMENTED | `S/convert.rs` `convert_aggregate_call`: more than one argument → `TypeMismatch` ("requires exactly 1 argument") | None (unpinned) | Row-value distinct needs a composite `GroupingKey` over several expressions. Whether Snowflake accepts the form was **not verified** in this audit. | `GroupingKey` already composite for `GROUP BY` | Decide after dialect check; if wanted: allow n args for `COUNT` only. | — |
| A-6 | `NULL` handling and empty input (COUNT 0; SUM/AVG/MIN/MAX `NULL`; grouped aggregate over empty input → 0 rows; `NULL` group key = one group) | IMPLEMENTED_AND_TESTED | `S/aggregate.rs`, `AggregateOp` | `exec_tests::count_star_over_empty_table_returns_one_row_with_zero`, `sum_avg_min_max_over_empty_table_are_null_count_is_zero`, `grouped_aggregate_over_empty_table_returns_zero_rows`, `null_grouping_key_forms_exactly_one_group`; `aggregate::tests::*` | — | A-1 | none | std |
| A-7 | `agg(x) FILTER (WHERE c)` (plain and window aggregates) | IMPLEMENTED_AND_TESTED | `BoundAggregateExpr.filter`; evaluated per row in `AggregateOp` and `WindowOp` | `aggregation_tests::exec_2f::filter_applies_to_one_aggregate_not_the_whole_select`, `a_null_filter_condition_excludes_the_row`, `filter_on_a_window_aggregate` | `FILTER` on a non-aggregate function is refused (`convert.rs`, unpinned). | A-1 | none | R6 |
| A-8 | Arithmetic overflow in aggregates | IMPLEMENTED_NOT_FULLY_VERIFIED | `checked_add` for COUNT / SUM / SUM(DECIMAL) and for merge paths in `S/aggregate.rs` → typed `RESOURCE_LIMIT` / error | Only `aggregate::tests::sum_semantics_empty_null_overflow` (BIGINT `SUM` at state level). No SQL-level overflow test for `SUM`, `SUM(DECIMAL)`, `AVG` accumulation or `COUNT`. | Overflow is an error, never wrap or saturate. | A-1 | SQL-level overflow tests per type. | std |
| A-9 | `GROUP BY ALL` | NOT_IMPLEMENTED | refused, `S/convert.rs:469` | Pinned: `parse_tests::unsupported_grammar_is_a_typed_error_not_a_panic`, `window_tests::parser::what_is_still_outside_the_grammar_after_mission_2_is_a_typed_error` | — | — | Snowflake-only convenience; low priority. | — |
| A-10 | Other aggregate functions (`STDDEV`, `VARIANCE`, `MEDIAN`, `LISTAGG`/`STRING_AGG`, `ARRAY_AGG`, `ANY_VALUE`, `BOOL_AND/OR`, `COUNT_IF`, …) | NOT_IMPLEMENTED | `AggregateFunc` has five variants (`S/aggregate.rs:17`); other names fail binding as unknown functions (`S/functions.rs` registry) | `functions::tests` pins that an unknown name is not found | — | Aggregate state types; string/array types for `LISTAGG`/`ARRAY_AGG` (N-5) | Per-function missions; order by dependency on types. | R12 |

### Area B — Subqueries

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| B-1 | Scalar subquery (SELECT list, WHERE, HAVING, operand) | IMPLEMENTED_AND_TESTED | `S/convert.rs` (`sp::Expr::Subquery`), `S/bind/expr.rs`, `S/exec/subquery.rs` | `subquery_tests` (67: `exec`, `differential`), `api_sql_integration::multi_row_scalar_subquery_is_a_runtime_execution_error_over_http` | 0 rows → `NULL`; 1 row → value; 2nd row → runtime error. Depth ≤ 32 (`max_subquery_depth`), ≤ 256 per statement. | — | none | std |
| B-2 | `IN (subquery)` / `NOT IN (subquery)`, three-valued logic | IMPLEMENTED_AND_TESTED | `S/exec/subquery.rs` `InSet` (`HashSet<GroupingKey>` + `has_null` + `empty`) | `subquery_tests::exec`, `::differential` (vs independent model) | A `NULL` in the set makes a non-match `UNKNOWN`. Set held in memory, bounded by materialization limits. | B-1 | none | std |
| B-3 | `EXISTS` / `NOT EXISTS` | IMPLEMENTED_AND_TESTED | same; pulls at most one row | `subquery_tests::exec`, `correlated_tests::diff_exists` | Never `NULL`. | B-1 | none | std |
| B-4 | Correlated subqueries (scalar / `IN` / `EXISTS`, in SELECT / WHERE / HAVING / `UPDATE … SET` / `UPDATE`/`DELETE … WHERE`) | IMPLEMENTED_AND_TESTED | `S/bind/scope.rs` (parent chain), `BoundSubquery` outer refs, executor re-runs the inner plan per outer row | `correlated_tests` (108), `api_sql_integration` correlated cases | **Per-outer-row re-execution; no result reuse.** Bounded by `max_correlated_reexecutions` and `max_subquery_outer_rows` (typed `RESOURCE_LIMIT`). | B-1 | Performance only (B-5). | std |
| B-5 | Decorrelation / semi-join / anti-join rewrite | NOT_IMPLEMENTED | `S/exec/subquery.rs` module doc states "no hoisting, no decorrelation and no result reuse across outer rows"; `PhysicalPlan` has no semi/anti join | none | `WHERE x IN (SELECT …)` over a large inner is O(outer × inner) when correlated. Uncorrelated subqueries are cached once (`ExecCtx::subquery_cache`; `subquery_tests`). | Hash join (J-3); statistics (L-7) | Phase 5: semi/anti join operator + rewrite rules, differential-tested against B-4. | R7, R8 |
| B-6 | Quantified comparison `x op ANY/ALL (subquery)`, row-value `(a,b) IN (SELECT …)` | NOT_IMPLEMENTED | falls to the `expr kind not in the approved grammar subset` arm (`S/convert.rs:1419`); `ast::Expr` has no row constructor | none (unpinned) | `x IN (subquery)` covers the common single-column case. | Row-value type | Low priority. | R6 |
| B-7 | Subquery inside a window call's arguments | NOT_IMPLEMENTED | refused, BIND/`UNSUPPORTED` (Mission 2 matrix X8) | `window_tests::binder` (cited by Mission 2 matrix) | — | — | Low priority. | — |

### Area C — CTEs and derived tables

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| C-1 | `WITH name AS (SELECT …)`, derived tables `(SELECT …) alias [(cols)]` | IMPLEMENTED_AND_TESTED | `S/convert.rs convert_with`, `S/bind/scope.rs`, `S/exec/relation.rs` | `relation_tests` (84), `materialization_tests` (6), `mission1_corpus_tests` | The alias of a derived table is required. | — | none | R4 |
| C-2 | Several CTEs in one `WITH`, later referencing earlier | IMPLEMENTED_AND_TESTED | same | `relation_tests::cte*`, `recursive_tests::contexts` | `max_cte_definitions` 64, `max_cte_references`, `max_derived_table_depth` bound the work. | C-1 | none | R4 |
| C-3 | A CTE referenced more than once | IMPLEMENTED_AND_TESTED | materialized once per statement and shared (`Arc<CtePlan>`; `exec::relation`) | `relation_tests::materialization`, `materialization_tests` | **Always materialized** (no inlining of a single-reference CTE; PostgreSQL inlines it, R4). Bounded by `max_materialized_rows` / `_bytes`. | C-1 | Optimizer inlining is a Phase 5 option. | R4 |
| C-4 | `LATERAL` derived table as `[LEFT] JOIN LATERAL (…) alias ON cond` | IMPLEMENTED_AND_TESTED | `S/bind/select.rs`, `PhysicalPlan::Derived { correlated }` | `relation_tests::lateral`, `::diff_relations` | Comma form `FROM t, LATERAL (…)` and `CROSS JOIN LATERAL` need comma/CROSS joins (J-6). | J-6 for the comma spelling | with J-6 | R6 |
| C-5 | `WITH` before `UPDATE` / `DELETE` | IMPLEMENTED_AND_TESTED | `convert_query_statement` | `correlated_tests::dml`, `recursive_tests::contexts` | — | — | none | R4 |
| C-6 | `WITH` before `INSERT` | PARTIALLY_SUPPORTED | `convert_query_statement` attaches the list | grammar addendum states it "is accepted and has no effect" | `INSERT` source must be `VALUES` (G-2), so a CTE cannot be read. | G-2 | closes with INSERT … SELECT | R1 |
| C-7 | `WITH RECURSIVE … seed UNION ALL term` | IMPLEMENTED_AND_TESTED | `S/bind/recursive.rs`, `S/exec/recursive.rs run_fixpoint`, `RecursiveCteNode` | `recursive_tests` (82: `counter`, `union`, `shape`, `contexts`, `diff`, `limits`, `observability`), HTTP `recursive_ctes_through_the_real_api` | Term restrictions: no aggregate / GROUP BY / HAVING / ORDER BY / LIMIT / window / ROLLUP; recursive reference exactly once, directly in the term's `FROM`, not on the nullable side of an outer join. | C-1, set operations (D-1) | none | R4 |
| C-8 | `WITH RECURSIVE … seed UNION term` (set semantics, cycle ends by itself) | IMPLEMENTED_AND_TESTED | `SetKey` dedup against all rows so far | `recursive_tests::union` (cycle, diamond, NULL rows) | Like PostgreSQL, `UNION` stops a cycle only when a whole row repeats; a changing depth column defeats it (then bound C-9 applies). | C-7 | none | R4 |
| C-9 | Termination and resource bounds | IMPLEMENTED_AND_TESTED | `max_recursive_cte_depth` 1,000, `max_recursive_cte_rows` 1,000,000, `max_recursive_cte_definitions` 8; deadline and cancellation per iteration | `recursive_tests::limits` (exact at the bound), `api_sql_integration::recursive_cte_errors_and_bounds_over_http` | Exhaustion is a typed `RESOURCE_LIMIT`, never silent truncation. | C-7 | none | R4 |
| C-10 | `SEARCH … FIRST BY` / `CYCLE … SET … USING` clauses | NOT_IMPLEMENTED | no reference in `sql/src` | none (unpinned) | Users must write path columns by hand. | C-7 + array type (N-5) for `path` | Optional; needs a path/array type. | R4 |
| C-11 | Multi-reference recursion; mutually recursive CTEs | NOT_PLANNED | bind error: "must appear exactly once in the recursive term", "recursive CTEs that refer to each other are not supported" | `recursive_tests::binder` | Matches the standard's linear-recursion restriction; PostgreSQL also rejects non-linear recursion. | — | none | R4 |
| C-12 | An earlier CTE of the same list named inside a recursive term | NOT_PLANNED | `hidden_ctes` in `S/bind/scope.rs`, refused by the mission prompt | `recursive_tests::binder` | `OPEN_ITEMS.md` (2026-10-12) records that PostgreSQL, MySQL and Snowflake allow it. **A Snowflake-compat gap by an explicit decision**; reversible. | C-7 | Decide in Phase 5 whether to reverse. | R4 |
| C-13 | `AS [NOT] MATERIALIZED` hints | NOT_IMPLEMENTED | refused, `S/convert.rs:281` | unpinned | — | C-3 inlining | optional | R4 |
| C-14 | Data-modifying CTEs (`WITH x AS (DELETE … RETURNING …)`) | NOT_IMPLEMENTED | a CTE body must be a query (`convert_with`); `RETURNING` does not exist (G-3) | none (unpinned) | — | G-3 `RETURNING` | after G-3 | R4 |
| C-15 | A CTE body that reads an outer-query column | NOT_IMPLEMENTED | bind error "CTE '…' cannot reference outer column" (`bind/scope.rs::Boundary`) | `relation_tests::cte_binder` | PostgreSQL allows a CTE inside a correlated subquery to reference outer columns. | B-4 | optional | R4 |

### Area D — Set operations

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| D-1 | `UNION`, `UNION ALL` | IMPLEMENTED_AND_TESTED | `S/bind/setop.rs`, `LogicalPlan::SetOp`, `S/exec/setop.rs` (streaming for `ALL`; hash set for distinct) | `setop_tests` (129: `union_all`, `union`, `diff`, `contexts`, `limits`), `mission1_corpus_tests`, HTTP `all_six_set_operators_through_the_real_api` | Bounded by `max_materialized_rows` / `_bytes`; `max_set_op_branches` 32. | — | none | R5, R14 |
| D-2 | `INTERSECT`, `INTERSECT ALL` | IMPLEMENTED_AND_TESTED | right operand counted once in a hash map, left streamed | `setop_tests::intersect`, `::diff` | Right operand fully materialized (bounded). | D-1 | none | R5 |
| D-3 | `EXCEPT`, `EXCEPT ALL` | IMPLEMENTED_AND_TESTED | same machinery | `setop_tests::except`, `::diff` (counts never negative) | same | D-1 | none | R5 |
| D-4 | Duplicate / `NULL` semantics | IMPLEMENTED_AND_TESTED | dedicated predicate `rows_not_distinct` + `SetKey` (`NULL` = `NULL`) | `setop_tests::distinctness`, `::union`, `::intersect`, `::except` | — | D-1 | none | R5 |
| D-5 | Precedence and associativity (`INTERSECT` tighter; `UNION`/`EXCEPT` left-to-right) | IMPLEMENTED_AND_TESTED | converter keeps `sqlparser`'s tree | `setop_tests::parser`, `intersect::intersect_binds_tighter_than_…`, `diff` (all 36 operator pairs × parenthesisation) | Matches PostgreSQL and Snowflake (R5, R14). | — | none | R5, R14 |
| D-6 | Column type coercion | PARTIALLY_SUPPORTED | `unify_types` in `S/bind/setop.rs` | `setop_tests::binder` (all 36 numeric pairs; every non-numeric pair) | **Numeric widening only.** No implicit cast to `TEXT` or between non-numeric types (e.g. `DATE` vs `TIMESTAMP` is a bind error). Snowflake documents mismatched types as needing explicit casts (R14), but `CAST` does not exist (N-4). | `CAST` (N-4) | with N-4 | R14 |
| D-7 | `ORDER BY` / `LIMIT` / `OFFSET` over the whole set result | PARTIALLY_SUPPORTED | set-level `Sort` / `Limit` over the output relation | `setop_tests::binder::order_by_*`, `::union_all` | **Output-column names only**; an ordinal sorts by the constant (`ORDER BY 1` is a constant key, `MISSION1_FEATURE_MATRIX.md` O16). A table column is `InvalidIdentifier`. | ORDER BY ordinal/alias (L-14) | with L-14 | R5, R14 |
| D-8 | Set operations in subqueries, CTEs, derived tables, DML | IMPLEMENTED_AND_TESTED | same node | `setop_tests::contexts` (17) | — | — | none | R5 |
| D-9 | `MINUS` (Snowflake alias of `EXCEPT`) | NOT_IMPLEMENTED | refused, `S/convert.rs:380` ("use EXCEPT") | `setop_tests::parser` | Trivial alias if wanted. | D-3 | Phase 5 parser alias. | R14 |
| D-10 | `UNION [ALL] BY NAME` | NOT_IMPLEMENTED | refused, `S/convert.rs:385` | `setop_tests::parser` | Snowflake feature (R14). | D-1 | optional | R14 |

### Area E — Window functions

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| E-1 | `ROW_NUMBER`, `RANK`, `DENSE_RANK` | IMPLEMENTED_AND_TESTED | `S/bind/window.rs`, `S/exec/window.rs WindowOp` (blocking, buffers input, partitions by `SetKey`, emits in input order) | `window_tests::exec_2a` (29), `window_reference_model` (5, 3,800+ queries) | `RANK`/`DENSE_RANK` require a window `ORDER BY` (bind error otherwise). Output keeps input row order (Mission 2 decision N1). | — | none | R6 |
| E-2 | `NTILE`, `PERCENT_RANK`, `CUME_DIST` | IMPLEMENTED_AND_TESTED | same | `window_tests::exec_2c`, reference model `other_funcs` | — | E-1 | none | R6 |
| E-3 | `LAG`, `LEAD` (offset, default) | IMPLEMENTED_AND_TESTED | same | `exec_2c::lag_*`, reference model | Position in the ordered partition; `NULL` not skipped. No `IGNORE NULLS` (E-9). | E-1 | none | R6 |
| E-4 | `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE` | IMPLEMENTED_AND_TESTED | same | `exec_2c::last_value_with_the_default_frame_*`, `nth_value_*`, `first_value_*` | Frame-relative; default-frame `LAST_VALUE` is the last peer (standard). | E-6, E-7 | none | R6 |
| E-5 | `SUM/COUNT/AVG/MIN/MAX … OVER (…)` | IMPLEMENTED_AND_TESTED | window aggregates reuse `AggregateState` | `exec_2a::window_aggregates_work_over_every_type`, `a_sliding_sum_matches_a_hand_computed_reference`, reference model | Frames not starting at the partition start recompute per row: O(rows × width) (Mission 2 N19). | A-1 | Incremental sliding frames (Phase 5, optional). | R6 |
| E-6 | `PARTITION BY` / `ORDER BY`, default frames, peer groups | IMPLEMENTED_AND_TESTED | `WindowOp`, `compare_sort_keys`; `NULL` partition keys form one partition; `NULL` order keys are peers | `exec_2a::partition_by_*`, `null_partition_keys_form_one_partition`, `default_frame_*`, `an_all_null_order_column_makes_every_row_a_peer` | A partition is buffered whole (K-7). | — | none | R6 |
| E-7 | `ROWS` frames, all boundaries | IMPLEMENTED_AND_TESTED | `Partition::frame` | `exec_2b` (18), reference model (11 datasets × 22 frame pairs × 4 orders) | Frame empty for every row is a bind error (standard syntax rule). | E-6 | none | R6 |
| E-8 | `RANGE` frames with value offsets (numeric keys) | IMPLEMENTED_AND_TESTED | binary search over the numeric key | `exec_2b::range_*`, `null_order_keys_*`, `range_offsets_work_on_every_numeric_key_type`, reference model | One numeric `ORDER BY` key; no interval type, so no `RANGE INTERVAL …` on dates (X7). | — | with temporal/interval types | R6 |
| E-9 | `GROUPS` frames; frame `EXCLUDE`; `IGNORE NULLS` / `RESPECT NULLS`; `FROM FIRST/LAST` | NOT_IMPLEMENTED | refused, `S/convert.rs:653`, `:702` | Pinned: `window_tests::parser::what_is_still_outside_the_grammar_after_mission_2_is_a_typed_error` (`GROUPS`, `window_tests.rs:190`), `api_sql_mission2::illegal_frames_are_bind_errors_and_groups_frames_are_unsupported`; `IGNORE NULLS` / `EXCLUDE` refusals are unpinned | PostgreSQL 18 supports `GROUPS` and `EXCLUDE` (R6). | peer-group machinery exists in `WindowOp` | Phase 5: `GROUPS` first (peer groups are already computed); `EXCLUDE`; `IGNORE NULLS`. | R6 |
| E-10 | `DISTINCT` inside a window aggregate | NOT_IMPLEMENTED | refused, `S/convert.rs`/binder (X3) | `aggregation_tests::exec_2f::distinct_is_not_allowed_on_the_wildcard_nor_on_a_window_aggregate` | PostgreSQL also rejects it. | — | none planned | R6 |
| E-11 | Named windows `WINDOW w AS (…)`, `OVER w`, `OVER (w …)` | IMPLEMENTED_AND_TESTED | `S/bind/window.rs` | `window_tests::exec_2a::a_named_window_is_shared_*`, `named_window_misuse_is_a_typed_error` | — | — | none | R6 |
| E-12 | `QUALIFY` | IMPLEMENTED_AND_TESTED | `Select::qualify` → `Filter` between the last `Window` node and the projection | `window_tests::exec_2d` (8), `mission2_corpus_tests`, `api_sql_mission2::qualify_over_http` | Select-list aliases resolve in `QUALIFY` (Snowflake behaviour, R13). Refused when the statement has no window function anywhere (Mission 2 decision N11), which agrees with Snowflake's stated requirement (R13). | E-1 | none | R13 |
| E-13 | Windows over aggregated queries, joins, derived tables, CTEs, subqueries, set-operation branches, DML subqueries | IMPLEMENTED_AND_TESTED | logical order `FROM → WHERE → Aggregate → HAVING → Window → QUALIFY → Projection → DISTINCT → ORDER BY → LIMIT` | `window_tests::composition` (10), `a_window_reads_the_statement_snapshot` | Predicate pushdown never crosses a `Window`. | — | none | R6, R13 |
| E-14 | Window-function placement rules | IMPLEMENTED_AND_TESTED | allowed in SELECT list, `QUALIFY`, `ORDER BY`; refused in `WHERE`/`HAVING`/`GROUP BY`/`ON` | `window_tests::binder::a_window_function_is_refused_outside_*` | Matches PostgreSQL ("only in SELECT list and ORDER BY") plus Snowflake's `QUALIFY`. | — | none | R6, R13 |
| E-15 | Window functions beyond the eleven + five aggregates (e.g. `RATIO_TO_REPORT`, `MEDIAN OVER`, `LISTAGG OVER`) | NOT_IMPLEMENTED | `WINDOW_ONLY_FUNCTIONS` list, `S/convert.rs:680` | none (unpinned) | — | A-10 | per-function | R12 |
| E-16 | Limits: partition rows, functions per statement, windows' memory | PARTIALLY_SUPPORTED | `max_window_partition_rows` 1,000,000, `max_window_functions_per_statement` 64, `max_materialized_rows/_bytes` | `window_tests::exec_2a::a_partition_larger_than_the_limit_is_a_resource_limit`, `too_many_window_functions_is_a_resource_limit`, `the_buffer_is_bounded_by_the_materialization_limits` | Bounded and rejecting; **no spill** (K-4/K-7). | — | Spill is Phase 5, optional. | R9 |

### Area F — Grouping extensions

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| F-1 | `GROUPING SETS` | IMPLEMENTED_AND_TESTED | `BoundGrouping`, one pass in `AggregateOp` (`rows_scanned` = table size, not × sets) | `aggregation_tests::exec_2e` (27), `aggregate_reference_model::mission2` (13 clauses × 6 scenarios + 24 random tables) | ≤ 64 sets per statement (`max_grouping_sets_per_statement`, counted before expansion); ≤ 64 distinct grouping expressions. | A-1 | none | std |
| F-2 | `ROLLUP`, `CUBE` | IMPLEMENTED_AND_TESTED | same | `exec_2e::rollup_*`, `cube_*`, reference model | `CUBE` of 60 columns is refused without expansion. | F-1 | none | std |
| F-3 | `GROUPING(col, …)` | IMPLEMENTED_AND_TESTED | group-key mask | `exec_2e::grouping_is_zero_for_a_real_null_*`, `grouping_with_several_arguments_is_a_bit_string` | Argument must be a `GROUP BY` expression (else bind error). | F-1 | none | std |
| F-4 | Plain columns mixed with extensions, composite members, `GROUP BY ()` | IMPLEMENTED_AND_TESTED | `GroupElement` | `exec_2e::mixing_plain_columns_with_an_extension_multiplies_the_sets`, `the_empty_grouping_set_alone_is_a_global_aggregate`, `empty_input_still_yields_the_empty_sets_row` | Duplicate sets are kept (standard). Grouping-column nullability is conservative. | F-1 | none | std |
| F-5 | Correlated subquery reading a grouped column under a grouping extension | NOT_IMPLEMENTED | refused, Mission 2 matrix X11 | `aggregation_tests` (cited) | — | B-4 | optional | std |
| F-6 | `GROUP BY … WITH ROLLUP/CUBE/TOTALS` spellings | NOT_PLANNED | refused; use `ROLLUP(…)` | cited by Mission 2 matrix X12 | MySQL / ClickHouse spellings. | — | none | — |

### Area G — DML

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| G-1 | `INSERT … VALUES` (multi-row, column subset, `DEFAULT` literal for omitted columns), `UPDATE`, `DELETE` with `WHERE` (incl. subqueries) | IMPLEMENTED_AND_TESTED | `S/exec/write.rs` (`execute_insert/update/delete`), all writes through `Transaction::put_row/delete_row` | `write_tests` (44: `insert_*`, `update_*`, `delete_*`, `concurrent_*`, `matches_reference_model_for_a_generated_insert_update_delete_sequence`), `sql/tests/write_crash_consistency` (2) | Write-set ≤ 10,000 ops and 16 MiB per transaction (`TxnLimits`); `max_dml_target_rows` fails closed (`mass_delete_beyond_max_dml_target_rows_fails_closed`); ≤ 10,000 `VALUES` rows. | Transactions (N-1) | none | R1 |
| G-2 | `INSERT … SELECT` (and `INSERT … WITH … SELECT`) | NOT_IMPLEMENTED | refused, `S/convert.rs:1137-1141` ("INSERT source must be a VALUES list") | Unpinned by a unit test; corroborated by the 2026-10-02 probe table in `PHASE_RUBIXDB_FINAL_SINGLE_NODE_ARCHITECTURE.md` | The 10,000-op write-set limit makes a large `INSERT … SELECT` fail closed unless chunked. | Streaming read inside the write transaction (exists: `execute_update_with_subqueries`); write-set limit decision | Phase 1. | R1, R11 |
| G-3 | `INSERT/UPDATE/DELETE … RETURNING` | NOT_IMPLEMENTED | refused, `S/convert.rs:1084-1092, 1178, 1229`; `WriteResult { kind, rows_affected }` carries no rows (`S/exec/write.rs:43`); `SqlResultBody::Write { statement, rows_affected }` (`A/routes/sql.rs`) | Unpinned; probe table above | Snowflake's INSERT page (R11) does not list `RETURNING`; PostgreSQL does (R1). | Result-shape change in executor, API, CLI renderer, frontend | Phase 1 (decide dialect: PostgreSQL semantics, rows for rows actually written). | R1, R11 |
| G-4 | `ON CONFLICT DO NOTHING` | NOT_IMPLEMENTED | refused, same arm | unpinned | The duplicate-key check already exists (`execute_insert` pre-write existence check, `S/exec/write.rs:370`) and is the natural arbiter hook for PK; unique-index conflicts are detected only at commit (`Transaction::validate_and_build_ops`). | G-1 | Phase 1. | R1 |
| G-5 | `ON CONFLICT (target) DO UPDATE SET … [WHERE …]`, `EXCLUDED` | NOT_IMPLEMENTED | same | unpinned | PostgreSQL requires a conflict target for `DO UPDATE` and raises a cardinality violation if one statement affects a row twice (R1); needs the same detection. `UPDATE` of a primary-key column is refused (G-8). | G-4, G-8 | Phase 1. | R1 |
| G-6 | `MERGE` | NOT_IMPLEMENTED | `statement_kind_name` → "MERGE" (`S/convert.rs:216`) | unpinned | Snowflake's upsert idiom is `MERGE`, not `ON CONFLICT`; PostgreSQL 18 has both. | G-2, joins (J-3) | Phase 1 follow-up or later. | R1 |
| G-7 | `UPDATE … FROM`, `DELETE … USING`, multi-table forms | NOT_IMPLEMENTED | refused, `S/convert.rs:1178, 1229` | unpinned | Subquery forms (`WHERE x IN (SELECT …)`, `SET c = (SELECT …)`) cover many cases (B-4). | J-6 joins in DML | after J-6 | R12 |
| G-8 | `UPDATE` of a primary-key column | NOT_IMPLEMENTED | refused at bind time | `write_tests::update_primary_key_column_is_rejected_end_to_end`, `bind_tests::update_of_primary_key_column_is_rejected` | Needed by `ON UPDATE CASCADE` (I-5) and `DO UPDATE` on key columns. | row-key rewrite = delete + insert + index maintenance | with I-5 | R2 |
| G-9 | `DEFAULT` values | PARTIALLY_SUPPORTED | `S/bind/ddl.rs` | `write_tests::insert_omitted_default_column_gets_its_declared_default_end_to_end` | **Literal only**; `DEFAULT NULL` refused; expression defaults refused (`S/bind/ddl.rs:190-197`). Snowflake's `ADD COLUMN … DEFAULT` is also literal-only (R12). | — | expression defaults: optional | R12 |

### Area H — ALTER TABLE

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| H-1 | `ALTER TABLE … ADD COLUMN [DEFAULT] [NOT NULL]` | BLOCKED_BY_ARCHITECTURE | refused, `S/convert.rs:204-218`. **The stored-row decoder is positional against the current column list**: `decode_full_row` (`E/relational/table_store.rs:857`) builds the type list from the current columns and `decode_row_envelope` sizes the null bitmap from that field count (`E/catalog/encoding.rs:218-245`); the row's `schema_version` is read and discarded. A row written before a column was added would be mis-sized or misread. `TableRow.schema_version` is the constant 1 (`E/catalog/service.rs:377`); nothing increments it. | none (unpinned); `PHASE_RUBIXDB_FINAL_SINGLE_NODE_ARCHITECTURE.md` probe: NOT IMPLEMENTED | ADR D3 specifies the missing design (decode by the row's own version; missing trailing columns read as default/`NULL`). `NOT NULL` without a default needs a validation scan or is refused on non-empty tables. | **Versioned row decode** (E/relational + catalog column history), index/PK ordinals stable | Phase 3 ADR + implementation + recovery tests. | R3, R12, R15 |
| H-2 | `DROP COLUMN` | BLOCKED_BY_ARCHITECTURE | refused. A column's identity is its array position: `ColumnRow` is keyed `(table_id, ordinal)`, PK/index definitions store ordinals (`E/catalog/schema.rs`, `E/catalog/service.rs:329-345`), `decode_full_row` maps by position. Dropping a middle column renumbers every later ordinal. | none | ADR D31 defers breaking `ALTER` (needs a bounded background rewrite). PostgreSQL does not rewrite either: it hides the column (R3). | Stable column ids distinct from position; H-1 | Phase 3 or later (after H-1). | R3, R12 |
| H-3 | `RENAME COLUMN`, `RENAME TO` (table) | NOT_IMPLEMENTED | refused. The name lives only in the `system.columns` / `system.tables` row; row bytes, PK and indexes use ordinals and ids, so no stored data depends on the name. | none | Statement caches / bound plans that embed names were **not audited**. | catalog DDL primitive (rewrite one catalog row in `write_batch`) | Phase 3 (cheapest ALTER). | R3, R12 |
| H-4 | `ALTER COLUMN SET/DROP NOT NULL`, `SET/DROP DEFAULT` | NOT_IMPLEMENTED | refused. Nullability is a catalog flag; the row null bitmap exists for every field regardless, so no format change. | none | `SET NOT NULL` needs a full validation scan (R3) and must be atomic with concurrent writes (epoch lock). | catalog primitive; validation scan | Phase 3. | R3 |
| H-5 | `ADD/DROP CONSTRAINT` (`PRIMARY KEY`, `UNIQUE`, `CHECK`) | NOT_IMPLEMENTED | refused. Unique constraints exist only as unique indexes (`CREATE UNIQUE INDEX`, online build, G-1 tests). Catalog `ConstraintKind` has `PrimaryKey/Unique/NotNull/Check`, but `create_constraint` is called only by `E/catalog/tests.rs`; no SQL path writes it. | catalog tests (core crate, not run) | `CHECK` stores opaque text and is never evaluated (`check_expression` doc comment). | expression engine reuse for `CHECK` | Phase 4. | R2, R3 |
| H-6 | `ADD FOREIGN KEY` | NOT_IMPLEMENTED | see I-* | none | PostgreSQL's `NOT VALID` / `VALIDATE CONSTRAINT` pattern (R3) separates enforcement of new rows from validation of old ones. | I-1…I-7 | Phase 4. | R3 |
| H-7 | `ALTER COLUMN … TYPE` | NOT_PLANNED | ADR D31: needs a bounded background rewrite, "explicitly deferred" | — | — | — | none | R15 |

### Area I — Constraints and foreign keys

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| I-1 | Inline `col … REFERENCES t(c)` | NOT_IMPLEMENTED | `convert_column_def` refuses unknown column options (`S/convert.rs:1764`) | unpinned | — | — | Phase 4 | R2 |
| I-2 | Table-level `FOREIGN KEY (a) REFERENCES t(b)`, single column | NOT_IMPLEMENTED | refused, `S/convert.rs:1708` ("FOREIGN KEY constraints") | unpinned; probe table; `frontend/src/erd/SchemaDesign.tsx` states "rubiXDB has no foreign keys" | — | catalog FK record (I-8) | Phase 4 | R2 |
| I-3 | Composite FK | NOT_IMPLEMENTED | same | none | `MATCH SIMPLE` (any NULL exempts the row) is PostgreSQL's default (R2). | I-2 | Phase 4 | R2 |
| I-4 | Referenced PK / UNIQUE resolution | NOT_IMPLEMENTED | Primary key: `TableRow.pk_ordinals`. Unique: only as `IndexKind::Unique` indexes; table-level and inline `UNIQUE` are refused (`S/convert.rs:1703, 1759`) | unique index tests: `write_tests::unique_conflict_exactly_one_committer_wins`, `update_unique_column_to_an_existing_value_conflicts` | PostgreSQL requires the referenced columns to be PK, unique constraint or a non-partial unique index (R2). | I-2 | Phase 4 | R2 |
| I-5 | `ON DELETE` / `ON UPDATE`: `NO ACTION`, `RESTRICT`, `CASCADE`, `SET NULL`, `SET DEFAULT` | NOT_IMPLEMENTED | none | none | Cascades write to many rows in one transaction; the write-set limit (10,000 ops) must fail closed. `ON UPDATE` needs PK update (G-8). `SET DEFAULT` needs literal defaults (G-9: exists). | G-8, G-9, bounded cascade | Phase 4 | R2 |
| I-6 | Immediate enforcement and concurrency | BLOCKED_BY_ARCHITECTURE | Transactions are snapshot isolation. `Transaction::commit` locks the epoch lock of each **written** table (sorted) and validates freshness of the **written keys** plus unique-index existence (`E/relational/txn.rs:545-700`). It has **no read-set**: a parent-row check done at statement time is invisible to a concurrent commit that deletes the parent. Write skew is a documented, accepted D10 property (`txn.rs` header). Two transactions (insert child / delete parent) would both commit. | none | A correct design needs commit-time validation of referenced parent keys (and children for parent deletes) under the parent table's epoch lock, in the existing sorted lock order. | Commit-time FK validation hook in `validate_and_build_ops`; ADR; concurrency tests with barriers (pattern exists in `write_tests::concurrent_*`) | Phase 4 ADR first. | R2, R15 |
| I-7 | Validation of existing rows when a FK is added | NOT_IMPLEMENTED | none | none | PostgreSQL: plain `ADD FOREIGN KEY` scans and blocks writers; `NOT VALID` + `VALIDATE` splits the two (R3). | H-6, I-6 | Phase 4 | R3 |
| I-8 | Metadata persistence of FKs | NOT_IMPLEMENTED | Catalog `system.constraints` row = `(table_id, name, kind, column_ordinals, check_expression, added_in_schema_version)`; `ConstraintKind` has no FK; no referenced table / columns / actions (`E/catalog/schema.rs:446-512`) | catalog tests exercise the generic table (not run here) | A new kind or a second table is a catalog on-disk format change. | catalog format ADR | Phase 4 | R2 |
| I-9 | FK recovery | NOT_IMPLEMENTED | nothing to recover. Child writes and cascades would be one `write_batch` (atomic through the certified WAL/manifest path, `Transaction::commit`) — so recovery inherits atomicity, but FK-specific recovery tests do not exist. | none | `src/wal/` / `src/manifest/` are protected and are **not** expected to change. | I-6, I-8 | Phase 4 tests: crash between FK catalog write and first enforcement. | R15 |
| I-10 | `PRIMARY KEY` (single / composite), `NOT NULL` | IMPLEMENTED_AND_TESTED | `S/bind/ddl.rs`, `E/relational/txn.rs` | `write_tests::insert_null_primary_key_is_rejected`, `primary_key_conflict_exactly_one_committer_wins`; `plan_tests::partial_composite_pk_equality_*` | `NOT NULL` for a non-PK column is enforced at bind time (`S/bind/dml.rs:103`); PK columns are always `NOT NULL`. Snowflake **does not enforce** `PRIMARY KEY` / `UNIQUE` / `FOREIGN KEY` on standard tables (R10): rubiXDb is stricter. | — | none | R10 |
| I-11 | `UNIQUE` (via `CREATE UNIQUE INDEX`) | IMPLEMENTED_AND_TESTED | `IndexBuilder::create_index_online`, commit-time existence check | `write_tests::unique_conflict_exactly_one_committer_wins`; `sql/src/exec_tests` index tests | `NULL`s are never equal (standard). Inline / table-level `UNIQUE` refused. | — | Optional: accept `UNIQUE` syntax as sugar for a unique index. | R2 |
| I-12 | `CHECK` constraints | NOT_IMPLEMENTED | refused, `S/convert.rs:1711` | unpinned | Snowflake enforces `CHECK` on standard tables (R10). | expression evaluation at write time | Phase 4 | R10 |

### Area J — Join algorithms

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| J-1 | Nested-loop join, `INNER` / `LEFT`, full `ON` always evaluated | IMPLEMENTED_AND_TESTED | `S/exec/operators.rs NestedLoopJoinOp`, `JoinAlgorithm::NestedLoop` | `exec_tests::inner_join_emits_exact_multiplicity`, `left_join_*`, `plan_tests::join_with_no_usable_inner_index_selects_plain_nested_loop`, `reference_model` | A fresh inner operator is built per outer row; for an unindexed inner this is **O(|outer| × |inner|)**. Left-deep in `FROM` order. | — | superseded by J-3 for large inputs | R9 |
| J-2 | Index nested-loop join | IMPLEMENTED_AND_TESTED | substituted mechanically when the inner access path's key references an outer table (`S/plan/physical.rs is_correlated_to`) | `plan_tests::join_on_inner_pk_selects_index_nested_loop`, `exec_tests::index_nested_loop_join_produces_the_same_result_as_plain_nested_loop` | Chosen by a rule, not by cost. | indexes | cost-aware choice (L-5) | R7, R9 |
| J-3 | Hash join (in-memory) | NOT_IMPLEMENTED | none; ADR D18 deferred it pending a baseline | none | The single largest executor gap for analytic queries (equi-joins on unindexed columns). | memory accounting (reuse `max_materialized_*`), build-side choice (L-6), statistics (L-7) | Phase 2. | R8, R9 |
| J-4 | Sort-merge join | NOT_IMPLEMENTED | none | none | Needs sorted inputs or external sort (K-4). | K-3, K-4 | Evaluate after J-3; likely not needed. | R9 |
| J-5 | Grace / hybrid hash join (spilling) | NOT_IMPLEMENTED | none; the executor has no spill facility at all | none | Everything is bounded-and-rejecting by design (D17). | J-3, temp-file/spill subsystem (new; touches durability/security surface: file safety, cleanup, quotas) | Not before J-3 is measured; separate ADR. | R9 |
| J-6 | Join kinds beyond `INNER`/`LEFT ... ON`: `RIGHT`, `FULL`, `CROSS`, comma `FROM a, b`, `NATURAL`, `USING (…)`, join without `ON` | NOT_IMPLEMENTED | refused, `S/convert.rs:538, 1026-1046` | Pinned: `parse_tests::unsupported_grammar_is_a_typed_error_not_a_panic` (comma, `RIGHT`, `FULL`), `window_tests::parser::what_is_still_outside_the_grammar_after_mission_2_is_a_typed_error`, `api_sql_integration::unsupported_grammar_is_a_stable_error_not_silently_accepted` (`RIGHT`). `CROSS`, `NATURAL`, `USING`: unpinned. | `JoinKind` has two variants (`S/ast.rs`). | `FULL`/`RIGHT` need unmatched-inner tracking (natural in hash join, awkward in nested loop) | Phase 2. | R6 |
| J-7 | `LATERAL` joins | IMPLEMENTED_AND_TESTED | see C-4 | `relation_tests::lateral` | — | — | none | R6 |
| J-8 | Join-count limit | IMPLEMENTED_AND_TESTED | `PlannerLimits::max_joins` | `plan_tests::max_joins_limit_is_enforced` | — | — | none | — |

### Area K — Aggregation, sorting and memory

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| K-1 | Hash aggregation | IMPLEMENTED_AND_TESTED | `AggregateOp` (`HashMap<(set, GroupingKey), usize>` into an insertion-ordered `Vec<Group>`); blocking | `exec_tests::null_grouping_key_forms_exactly_one_group`, `aggregate_reference_model::matches_reference_model_for_high_cardinality_groups` | Output order is first-seen order of the input scan (deterministic, not sorted). | — | none | R9 |
| K-2 | Sort-based / streaming aggregation | NOT_IMPLEMENTED | none | none | An ordered-index input could allow streaming group-by; not attempted. | K-9 sort-order tracking | optional | R9 |
| K-3 | In-memory sort (`ORDER BY`, `NULLS FIRST/LAST`) | IMPLEMENTED_AND_TESTED | `SortOp`: materialize, compute keys once per row, `sort_by` | `exec_tests::sort_ascending_and_descending_with_nulls_first_and_last` | `RESOURCE_LIMIT` at `max_materialized_rows` (1,000,000). | — | none | std |
| K-4 | External sort / spill to disk | NOT_IMPLEMENTED | `S/exec/operators.rs:591` comment: "no external sort/spill exists, matching D17's … 'bounded, reject when exceeded'" | none | A sort above the row cap fails with a typed error. | spill subsystem (J-5) | not recommended before measured need | R9 |
| K-5 | Top-N (heap) for `ORDER BY … LIMIT n` | NOT_IMPLEMENTED | `Limit` over a fully materialized `Sort`; optimizer marks a `Limit` pushable only when directly over a scan (`plan_tests::limit_above_sort_is_never_marked_pushable`) | pinned negatively by that test | `ORDER BY x LIMIT 10` over 1M rows sorts all 1M. | K-3 | Phase 2 (small, self-contained, differential-testable). | R8 |
| K-6 | Streaming / early termination | IMPLEMENTED_AND_TESTED | pull operators; `LimitOp` stops the input | `exec_tests::limit_stops_scanning_early`, `setop_tests::union_all` ("`LIMIT 3` reads < 50 of 1000 rows") | Blocking operators (sort, aggregate, distinct, window, set-op right side) are not streaming. | — | none | R9 |
| K-7 | Memory-aware windows | PARTIALLY_SUPPORTED | see E-16 | cited in E-16 | bounded and rejecting; no spill | K-4 | optional | — |
| K-8 | `SELECT DISTINCT` | IMPLEMENTED_NOT_FULLY_VERIFIED | `DistinctOp` keeps seen rows in a `Vec` and tests membership with `contains` (`S/exec/operators.rs:289-320`): **O(n²) in distinct rows** | `exec_tests::distinct_deduplicates_by_projected_value_including_null` (correctness only) | Correct but quadratic; recorded in `OPEN_ITEMS.md` (2026-10-11). No scale test. The set-operation path already has a hash-based `SetKey` set. | `SetKey` reuse | Phase 2 (reuse `SetKey`; differential test vs current). | — |
| K-9 | Sort elimination via index order | IMPLEMENTED_AND_TESTED | `S/plan/physical.rs order_satisfied_by_access`, `order_only_index_access` | `plan_tests::sort_is_eliminated_*`, `sort_is_retained_*` | Ascending, `NULLS FIRST`, exact index-column match only; no reverse scan primitive exists. | — | none | R7 |
| K-10 | Resource limits reject, never truncate | IMPLEMENTED_AND_TESTED | `ExecLimits` (`S/exec/mod.rs:283-440`) | `limits_tests` (12), per-feature `limits` modules | — | — | none | — |

### Area L — Optimizer

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| L-1 | Predicate pushdown, `LEFT JOIN`-safe | IMPLEMENTED_AND_TESTED | `S/plan/optimize.rs pushdown_predicates`, `try_push_into_scan` | `plan_tests::single_table_predicate_is_fully_pushed_*`, `left_join_predicate_on_nullable_side_is_never_pushed_into_its_scan`, `inner_join_predicate_on_either_side_may_be_pushed` | One pass, fixed rule order, no fixpoint; never crosses `Window`; no expression rewriting (by design). | — | none | R7 |
| L-2 | Projection pruning | IMPLEMENTED_AND_TESTED | `prune_projections` → `required_columns` on `Scan` | `plan_tests::required_columns_include_projection_and_filter_but_not_unused_columns`, `logical_required_columns_excludes_unreferenced_column` | — | — | none | R7 |
| L-3 | Limit pushdown | IMPLEMENTED_AND_TESTED | `mark_pushable_limits` | `plan_tests::limit_directly_above_a_scan_is_marked_pushable`, `limit_above_a_join_is_never_marked_pushable` | Only directly over a scan. | — | none | — |
| L-4 | Join reordering | NOT_IMPLEMENTED | joins are left-deep in `FROM` order (`S/plan/logical.rs:182-193`); `PHASE_RELATIONAL_QUERY_PLANNER_INCREMENT8_RESULTS.md` records it as out of scope | none | A bad `FROM` order cannot be repaired by the planner. SQLite reorders inner joins but never outer joins (R7); DuckDB uses dynamic programming with a greedy fallback (R8). | Cardinality estimation (L-8), statistics (L-7), join algorithms (J-3) | Phase 5. | R7, R8 |
| L-5 | Cost-based selection | PARTIALLY_SUPPORTED | Only **index-vs-table-scan, decided at execution time** (`S/exec/cost.rs`, `E/relational/stats.rs`): scan abandons the index when matches exceed `K* = N_hi × seq_ns / index_ns`. Join algorithm and join order are rule-based. | `cost_model_tests` (12), `index_read_differential_tests` (7), `index_snapshot_tests` (8) | Statistics may change which of two result-equivalent paths runs, never the result (module doc). | — | extend to joins in Phase 5 | R7 |
| L-6 | Build-side selection | NOT_IMPLEMENTED | no hash join | none | — | J-3, L-8 | with J-3 | R8, R9 |
| L-7 | Statistics | PARTIALLY_SUPPORTED | `E/relational/stats.rs`: per-table **row-count estimate with a drift bound**, in memory, ≤ 4,096 tables, two atomics each; plus two EWMA per-row cost constants. **Not persisted**: lost at restart (the benchmark `restart_statistics_experiment` is `#[ignore]`d) | `cost_model_tests`; `E/relational` tests not run here | No distinct counts, histograms, null fractions or min/max. No `ANALYZE`. | — | Phase 5 (persisted, bounded statistics need an ADR: durability and recovery surface). | R7, R8 |
| L-8 | Cardinality estimation | NOT_IMPLEMENTED | none beyond table row count | none | No filter or join selectivity. | L-7 | Phase 5 | R8 |
| L-9 | Index vs scan selection (`PkLookup`, `PkRangeScan`, `IndexScan`, `SeqScan`) | IMPLEMENTED_AND_TESTED | `S/plan/access.rs plan_table_access`, `S/exec/access_op.rs` | `plan_tests` (pk/index cases), `index_read_differential_tests`, `txn_scan_tests` (8; read-your-own-writes overlay) | Primary-kind index is never an `IndexScan`; partial composite PK equality becomes a prefix range. | — | none | R7 |
| L-10 | Expression simplification / constant folding | NOT_PLANNED | `S/plan/optimize.rs` header: "no rule here ever rewrites the logic of a `BoundExpr`" (D16) | — | — | — | none | R15 |
| L-11 | `ORDER BY` by select-list alias or ordinal in a plain `SELECT` | NOT_IMPLEMENTED | an alias that is not a column is not resolved; an integer is a constant key | `OPEN_ITEMS.md` (Mission 2 D-7); no pinning test located | Snowflake and PostgreSQL accept both. `QUALIFY` resolves aliases itself (E-12). | binder | Phase 1 (small, high user value). | R13 |

### Area M — Explainability

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| M-1 | `EXPLAIN <stmt>` (structural text, deterministic, never executes) | IMPLEMENTED_AND_TESTED | `S/plan/explain.rs`; API `SqlResultBody::Explain { plan_text }` (`A/routes/sql.rs:73-78, 405-411`) | `plan_tests::explain_output_is_deterministic_and_contains_no_filesystem_path`, `explain_wraps_the_inner_statements_own_plan`, `api_sql_mission2::explain_shows_windows_grouping_filters_and_distinct` | Parameter values never appear. | — | none | R8 |
| M-2 | Operators shown: access path (`PkLookup`, `PkRangeScan`, `IndexScan` with index name and mode, `SeqScan`), join kind and algorithm, filter, projection, distinct, aggregate (+ grouping sets), window (partition, order, frame, functions), sort, limit, set operations (shape), recursive CTE (seed / term), derived/CTE nodes, subquery plans | IMPLEMENTED_AND_TESTED | `write_physical`, `write_access` | `setop_tests::union_all` (EXPLAIN), `recursive_tests::observability`, `window_tests::exec_2a::explain_shows_the_window`, `correlated_tests::dml::explain_shows_the_correlation_for_where_and_set` | — | — | none | R8 |
| M-3 | Join algorithm shown | IMPLEMENTED_AND_TESTED | `"{kind} Join ({algo}) on …"` (`explain.rs:187`) | `plan_tests` join cases | Only two algorithms exist. | J-3 | with J-3 | — |
| M-4 | Build / probe sides | NOT_IMPLEMENTED | no hash join | none | — | J-3 | with J-3 | R8 |
| M-5 | Estimated rows / cost per operator | NOT_IMPLEMENTED | `explain.rs` prints no estimates | none | — | L-8 | Phase 5 | R8 |
| M-6 | Materialization shown (CTE printed once when read twice; derived "materialized once, re-read for each left row") | IMPLEMENTED_AND_TESTED | `explain.rs` Derived/CteNode | `recursive_tests::observability`, `relation_tests::materialization` | — | — | none | — |
| M-7 | Spill shown | NOT_IMPLEMENTED | no spill exists | none | — | J-5/K-4 | n/a | — |
| M-8 | `EXPLAIN ANALYZE` (actual rows, timing per operator) | NOT_IMPLEMENTED | refused, `S/convert.rs:158-167` ("EXPLAIN with modifiers"). `ExecMetrics` (`S/exec/mod.rs:584-612`) are **statement-level counters** (rows scanned, sorted, groups, joins…), not per-operator measurements. | unpinned | DuckDB `EXPLAIN ANALYZE` reports actual cardinalities and operator timing (R8). Genuine per-operator measurement would need counters in each `Operator::next` wrapper. | operator instrumentation (must be zero-cost when off) | Phase 5 | R8 |
| M-9 | EXPLAIN surfaced in CLI and frontend | IMPLEMENTED_NOT_FULLY_VERIFIED | `cli/src/runner.rs:104` prints the returned `plan_text`; `frontend/src/ide/plan.ts` parses the text into a tree | `frontend/src/ide/*.test.*` exist; **not run in this audit** | `parsePlan` is purely structural (first word = node name). | — | run frontend / CLI suites at the start of Phase 5 | — |

### Area N — Adjacent surface that bounds Snowflake compatibility (recorded for dependencies; not in the A–M list)

| ID | Feature | Status | Implementation | Tests | Limitations | Dependencies | Remaining work | Ref |
|---|---|---|---|---|---|---|---|---|
| N-1 | Transactions: `BEGIN` / `COMMIT` / `ROLLBACK`, snapshot isolation, read-your-own-writes, atomic `write_batch` commit | IMPLEMENTED_AND_TESTED | `E/relational/txn.rs`; `A/sql_session.rs` | `write_tests::{rollback_*, read_your_own_writes_*, other_transaction_does_not_see_uncommitted_insert, concurrent_*}`, `txn_scan_tests`, `sql/tests/write_crash_consistency` (2); core `txn_tests.rs` not run | **Write skew is possible** (documented, accepted). `SAVEPOINT`, `AND CHAIN`, transaction modes refused. DDL is atomic but executes outside the SQL transaction (`S/exec/write.rs` doc); DDL inside an explicit transaction was **not audited**. | — | none for this program | R15 |
| N-2 | Savepoints | NOT_IMPLEMENTED | refused, `S/convert.rs:200` | unpinned | — | write-set partial rollback | optional | — |
| N-3 | Scalar function surface | NOT_IMPLEMENTED | registry has four entries: `length`, `upper`, `lower`, `abs` (`S/functions.rs`). `COALESCE`, `NULLIF`, `IFF`, `SUBSTR`, `CONCAT`, `ROUND`, date functions: unknown-function errors at bind (derived from the registry; not executed). | `functions::tests` | `CASE`, `LIKE`/`ILIKE`, `IN`, `BETWEEN`, `IS NULL` exist (`ast::Expr`). | function registry growth, type system | separate program | R12 |
| N-4 | `CAST` / `::` | NOT_IMPLEMENTED | `expr_kind_name` → "CAST" refused (`S/convert.rs:1428`) | unpinned | Blocks D-6 coercions and most Snowflake-style SQL. | type conversion rules | separate program | R14 |
| N-5 | Data types | PARTIALLY_SUPPORTED | `BOOLEAN`, `INTEGER`, `BIGINT`, `REAL`, `DOUBLE`, `DECIMAL(≤38)`, `TEXT`, `BLOB`, `DATE`, `TIME`, `TIMESTAMP` (no time zone) | `bind_tests`, `temporal` | No `VARIANT`/`ARRAY`/`OBJECT`, no `TIMESTAMP_TZ`, no interval type. | — | separate program | R12 |
| N-6 | HTTP `POST /v1/sql` typed errors, `EXPLAIN`, sessions | IMPLEMENTED_AND_TESTED | `A/routes/sql.rs`, `A/error.rs` | `api_sql_integration` (47), `api_sql_mission2` (13) — executed | — | — | none | — |
| N-7 | CLI (`rubixdb -c`, `-f`, interactive) and frontend SQL IDE | IMPLEMENTED_NOT_FULLY_VERIFIED | `cli/src/{runner,repl,script}.rs`; `frontend/src/ide/*` | suites exist; **not run in this audit** | The IDE keyword list offers `ALTER TABLE` and `RETURNING` (`frontend/src/ide/keywords.ts:51`, `suggest.ts:180`) although the engine refuses both (H-*, G-3) — completion is ahead of the engine. The Schema design page infers relationships because there are no foreign keys. | each phase must update IDE completions and the Schema page | with each phase | — |

## 5. Counts

Features audited: **137** (areas A–M: 130; area N, adjacent: 7). Counts are computed from the status column of §4 by script, not typed by hand.

| Classification | Count |
|---|---|
| IMPLEMENTED_AND_TESTED | 61 |
| IMPLEMENTED_NOT_FULLY_VERIFIED | 5 |
| PARTIALLY_SUPPORTED | 9 |
| NOT_IMPLEMENTED | 54 |
| BLOCKED_BY_ARCHITECTURE | 3 |
| NOT_PLANNED | 5 |
| **Total** | **137** |

Per area:

| Area | IMPLEMENTED AND TESTED | IMPLEMENTED NOT FULLY VERIFIED | PARTIALLY SUPPORTED | NOT IMPLEMENTED | BLOCKED BY ARCHITECTURE | NOT PLANNED | Total |
|---|---|---|---|---|---|---|---|
| A | 5 | 2 | 0 | 3 | 0 | 0 | 10 |
| B | 4 | 0 | 0 | 3 | 0 | 0 | 7 |
| C | 8 | 0 | 1 | 4 | 0 | 2 | 15 |
| D | 6 | 0 | 2 | 2 | 0 | 0 | 10 |
| E | 12 | 0 | 1 | 3 | 0 | 0 | 16 |
| F | 4 | 0 | 0 | 1 | 0 | 1 | 6 |
| G | 1 | 0 | 1 | 7 | 0 | 0 | 9 |
| H | 0 | 0 | 0 | 4 | 2 | 1 | 7 |
| I | 2 | 0 | 0 | 9 | 1 | 0 | 12 |
| J | 4 | 0 | 0 | 4 | 0 | 0 | 8 |
| K | 5 | 1 | 1 | 3 | 0 | 0 | 10 |
| L | 4 | 0 | 2 | 4 | 0 | 1 | 11 |
| M | 4 | 1 | 0 | 4 | 0 | 0 | 9 |
| N | 2 | 1 | 1 | 3 | 0 | 0 | 7 |

No row is a performance PASS, and no row was converted from OPEN/FAIL to PASS: statuses describe implementation and test evidence only (`CLAUDE.md` certification rule). `IMPLEMENTED_AND_TESTED` is not a certification.

## 6. Dependency map

Arrows read "needs". Items in the same row can proceed in parallel.

```
Row-format versioning (H-1) ──► ADD COLUMN (H-1) ──► NOT NULL w/o default (H-4)
        │                       └► stable column ids ──► DROP COLUMN (H-2)
        └─ catalog column history
RENAME / SET-DROP NOT NULL / SET-DROP DEFAULT (H-3, H-4) ── catalog row rewrite only (no row-format change)

Catalog constraint record (I-8, format ADR) ──► FK declaration (I-1..I-4)
Commit-time read validation (I-6, txn.rs) ──► FK enforcement ──► ON DELETE/UPDATE actions (I-5) ◄── PK update (G-8)
FK enforcement ──► ADD FOREIGN KEY + validation scan (H-6, I-7)

RETURNING (G-3) = result shape in write executor + API + CLI + frontend ──► data-modifying CTEs (C-14)
INSERT … SELECT (G-2) ──► MERGE (G-6) ◄── joins in DML (G-7, J-6)
ON CONFLICT (G-4, G-5) ◄── existing PK pre-check (execute_insert) + unique-index commit check; needs row-affected-twice detection

Hash join (J-3) ──► RIGHT/FULL (J-6), semi/anti join (B-5), build-side (L-6), EXPLAIN build/probe (M-4)
Statistics (L-7) ──► cardinality (L-8) ──► join reorder (L-4), build-side (L-6), EXPLAIN estimates (M-5)
Spill subsystem (J-5/K-4) ──► grace/hybrid hash (J-5), external sort (K-4), windows beyond memory (E-16)
Top-N (K-5), hash DISTINCT (K-8): independent of everything above
CAST + function registry (N-3, N-4) ──► type coercion in set ops (D-6), most Snowflake-style SQL
Per-operator counters ──► EXPLAIN ANALYZE (M-8)
```

## 7. Architectural blockers

| # | Blocker | Evidence | What unblocks it | Touches protected paths? |
|---|---|---|---|---|
| AB-1 | **Stored rows decode positionally against the current schema; the row's `schema_version` is ignored.** `ADD COLUMN` / `DROP COLUMN` cannot be done safely. | `E/relational/value.rs:116-125`, `E/relational/table_store.rs:857-897`, `E/catalog/encoding.rs:218-245` (bitmap length from the current field count), `E/catalog/service.rs:377` (`schema_version: 1` constant) | Decode by the row's own version against a recorded column history; stable column ids separate from position. ADR first (it changes how bytes already on disk are read); recovery and mixed-version-row tests. | No (`src/relational`, `src/catalog`). It does change the interpretation of persisted data, so it needs the durability checklist even though the certified boundary stays untouched. |
| AB-2 | **Snapshot isolation without a read set.** Commit validates written keys and unique-index existence only. A foreign key's parent check cannot be made immediate-and-safe without new commit-time validation. | `E/relational/txn.rs:545-700`, header of `txn.rs` (write skew accepted) | A validation step in `validate_and_build_ops` that locks referenced parents (and checks children on parent delete) using the existing per-table epoch lock in sorted order; barrier-based concurrency tests (pattern in `write_tests::concurrent_*`). | No. |
| AB-3 | **Catalog constraint record cannot express a foreign key.** | `E/catalog/schema.rs:435-512` (`CONSTRAINTS_SCHEMA`, `ConstraintKind`) | A new constraint kind with referenced table / columns / actions, or a sibling system table. On-disk catalog format change → ADR, backward-compatibility decision, recovery test. | No. |
| AB-4 | **No spill facility.** Every blocking operator is "bounded, reject when exceeded". Grace/hybrid join, external sort and larger-than-memory windows are impossible without one. | `S/exec/operators.rs:591`, `ExecLimits`, ADR D17 | A temp-file subsystem with quotas, cleanup after crash, path safety and instance isolation. It is a new security and durability surface and must be designed on its own. | No, but `CLAUDE.md` "resource safety" and "filesystem safety" rules apply. |
| AB-5 | **Statistics are volatile and row-count-only.** Join ordering and build-side selection have nothing to estimate from. | `E/relational/stats.rs` header ("No persisted state") | In-memory sampled statistics rebuilt at startup, or persisted statistics (the latter is a format change). Start with the former. | No. |
| AB-6 | **Write path returns counts only.** `RETURNING` needs the result shape, API body, CLI renderer and frontend table to change together. | `S/exec/write.rs:43-55`, `A/routes/sql.rs:52-82` | Coordinated change across four layers; JSON body is additive (`kind` variant). | No. |
| AB-7 | **Write-set cap (10,000 ops / 16 MiB).** Large `INSERT … SELECT`, cascading deletes and `UPDATE … FROM` fail closed above it. | `E/relational/txn.rs:46-80` | Decide: keep the cap and chunk, or raise it with a memory ADR. Not a blocker for correctness, a blocker for scale. | No. |

## 8. Highest-risk dependencies

Ranked by (probability of data-integrity or durability damage) × (blast radius if wrong).

1. **AB-1 versioned row decode (H-1/H-2).** Every table's every row passes through this decoder. A mistake corrupts reads of existing data, not just new features. Highest correctness risk in the program.
2. **AB-2 commit-time FK validation (I-6).** Lives inside the transaction commit critical section (epoch locks, sorted acquisition). A deadlock, a missed lock, or a stale check re-introduces the very anomalies FKs exist to prevent, and it is on the write hot path measured by the throughput gates.
3. **AB-3 catalog format change (I-8).** On-disk catalog format; mixed old/new binaries and recovery of a half-applied DDL must be proven.
4. **Hash join memory accounting (J-3).** Straightforward logic but the first operator whose memory is proportional to an input the user does not bound; wrong accounting is an out-of-memory risk (`CLAUDE.md`: resource safety). Mitigated by reusing `max_materialized_rows/_bytes`, which already govern sort and set-ops.
5. **`ON CONFLICT DO UPDATE` under SI (G-5).** Semantics under concurrent insert of the same key and "row affected twice" detection are subtle; the existing PK pre-check is statement-time only.
6. **Spill subsystem (AB-4).** New filesystem surface; not required by any Phase 1-4 item.

Lower risk: Top-N (K-5), hash `DISTINCT` (K-8), `GROUPS` frames (E-9), `MINUS` alias (D-9), `RENAME COLUMN` (H-3), `INSERT … SELECT` (G-2) — each is local, differential-testable against an existing path, and touches no persisted format.

## 9. Recommended implementation order

Scored on three axes (1 = low, 3 = high): **dependency position** (does it unblock others), **correctness/durability risk** (higher risk is done later, after tooling exists), **user value for Snowflake-style workloads**. This is a recommendation; the maintainer decides.

| Order | Work | Dependency | Risk | Value | Why here |
|---|---|---|---|---|---|
| 1 | **DML completion**: ORDER BY alias/ordinal (L-11), `INSERT … SELECT` (G-2), `RETURNING` (G-3), `ON CONFLICT` (G-4/G-5) | unblocks C-6, C-14, G-6 | 1-2 (no persisted format change) | 3 | Highest immediate value, no on-disk change; `RETURNING` also forces the result-shape change (AB-6) once, early. |
| 2 | **Join and executor completeness**: hash join, `CROSS`/comma/`RIGHT`/`FULL`/`USING`/`NATURAL`, Top-N, hash `DISTINCT` | unblocks B-5, L-4, L-6, M-4 | 2 (memory accounting) | 3 | Pure executor work, no persisted format; every item is differential-testable against the nested-loop path that already exists. |
| 3 | **ALTER TABLE**: `RENAME`, `SET/DROP NOT NULL`, `SET/DROP DEFAULT` first; then `ADD COLUMN` (AB-1); `DROP COLUMN` last or deferred | unblocks H-6 | 3 for AB-1, 1 for renames | 3 | Do the metadata-only forms first for value, then the versioned-decode work as its own gated step with an ADR. |
| 4 | **Constraints and foreign keys** (AB-3, AB-2) | needs AB-1 only for `ADD CONSTRAINT` on existing tables via ALTER | 3 | 2 (Snowflake does not enforce FK/PK/UNIQUE, R10) | Highest risk, and the least Snowflake-relevant. Do after ALTER exists and the test tooling is mature. `CHECK` can ship earlier within this phase. |
| 5 | **Optimizer, statistics, decorrelation, `EXPLAIN ANALYZE`, `GROUPS` frames, `MINUS`** | needs hash join (2) | 2 | 2 | Cost-based work is only worth doing once there is more than one join algorithm to choose between. |

## 10. Proposed bounded missions (Phases 1-5)

Each mission is one session, one stop condition, and must end with the report format in `CLAUDE.md`. Every phase keeps `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs` at zero diff unless an ADR and an explicit authorization say otherwise.

| Phase | Mission | In scope | Out of scope | Stop condition / PASS requires |
|---|---|---|---|---|
| **1** | **DML completion** | (a) `ORDER BY` select-alias and ordinal in plain `SELECT` and set operations; (b) `INSERT … SELECT` (incl. `WITH`), bounded by the write-set cap, fails closed; (c) `RETURNING` for INSERT/UPDATE/DELETE across executor, API, CLI renderer, frontend; (d) `ON CONFLICT DO NOTHING` / `DO UPDATE` with conflict target and `EXCLUDED`, with "affected twice" detection | `MERGE`, `UPDATE … FROM`, `DELETE … USING`, any catalog change | Differential test vs an independent model for each; concurrency tests for (d) (same key, two sessions, barrier); kill-9 crash test with an acked `INSERT … SELECT`; pinned refusal tests for what stays out; docs |
| **2** | **Join and executor completeness** | In-memory hash join (equi-joins, inner/left) with build-side choice by row-count estimate when known, else smaller written side; `CROSS`, comma, `RIGHT`, `FULL`, `USING`, `NATURAL`; Top-N; hash `DISTINCT` via `SetKey`; semi/anti join for uncorrelated `IN`/`EXISTS` only | Spill, join reordering, statistics, decorrelation of correlated subqueries | Differential tests: every join algorithm × every join kind against the existing nested loop on random data with NULLs and duplicates; memory-limit tests (exact at the bound); **performance PASS needs** workload, method, ≥ 3 runs, latency distribution, RSS measurements (`CLAUDE.md` certification rule) |
| **3** | **ALTER TABLE** | 3a: `RENAME COLUMN`, `RENAME TO`, `SET/DROP NOT NULL` (validation scan), `SET/DROP DEFAULT`. 3b (separate ADR, separate gate): versioned row decode, then `ADD COLUMN [DEFAULT] [NOT NULL]`. `DROP COLUMN` only if stable column ids are designed in 3b | `ALTER COLUMN TYPE` (NOT_PLANNED), constraints, FKs | 3a: restart + concurrent-DML tests. 3b: ADR first; mixed-version-row read tests at every null-bitmap byte boundary; crash tests around the catalog `write_batch`; **STOP if the decode change cannot be proven backward compatible with existing data directories** |
| **4** | **Constraints and foreign keys** | `CHECK`; table-level/inline `UNIQUE` as sugar for unique indexes; catalog constraint format ADR; FK single and composite, `MATCH SIMPLE`, `NO ACTION`/`RESTRICT`/`CASCADE`/`SET NULL`/`SET DEFAULT`; commit-time validation (AB-2); `ADD FOREIGN KEY` with validation scan; PK update (G-8) if `ON UPDATE CASCADE` is wanted | Deferrable constraints, `MATCH FULL`, FK on hybrid-style tables | ADR for AB-2 and AB-3 before code; barrier concurrency tests (parent delete vs child insert, both orders); cascade past the write-set cap fails closed and leaves nothing; recovery tests; the throughput gate must be re-run because commit is on the hot path |
| **5** | **Optimizer and explainability** | Persisted-or-rebuilt statistics (ADR), cardinality estimation, join reordering for inner joins (DP with greedy fallback, bounded), correlated-subquery decorrelation to semi/anti join, `EXPLAIN ANALYZE` with real per-operator actuals, estimated rows in `EXPLAIN`, `GROUPS`/`EXCLUDE` frames, `IGNORE NULLS`, `MINUS`, reverse the C-12 decision if the maintainer wants it | Spill (separate ADR), new data types | Plans must be result-identical with the optimizer on and off (differential); EXPLAIN ANALYZE counters are zero-cost when off (measured); stale-statistics tests prove results never change |

Not assigned to a phase (separate programs, noted in N-3…N-5): scalar function library, `CAST`, semi-structured and time-zone types, `MERGE` (follows Phase 1 + 2).

## 11. Research record

Each entry: reference → what it says → approach chosen for rubiXDb → alternatives → why it fits. No design here is a claim of Snowflake compatibility.

| Ref | Source | Finding | Approach for rubiXDb | Alternatives | Why it fits |
|---|---|---|---|---|---|
| R1 | PostgreSQL 18 `INSERT` — https://www.postgresql.org/docs/18/sql-insert.html | `ON CONFLICT` takes a conflict target (inferred unique index or named constraint); `DO NOTHING` target optional; `DO UPDATE` target required, sees the existing row and `excluded`, optional `WHERE`; atomic insert-or-update; a statement may not affect one row twice (cardinality violation); `RETURNING` returns only rows actually inserted/updated, can expose old/new. | PostgreSQL semantics (G-4, G-5): conflict target = PK or a unique index; "affected twice" is an error; `RETURNING` returns written rows. | Snowflake-style `MERGE` only; `INSERT OR REPLACE` (SQLite). | rubiXDb already has a PK pre-check and unique-index commit validation to hang the arbiter on; `RETURNING` is needed by data-modifying CTEs. |
| R2 | PostgreSQL 18 constraints — https://www.postgresql.org/docs/18/ddl-constraints.html | FK: referenced columns must be PK / unique / non-partial unique index; actions `NO ACTION` (default, may be deferred), `RESTRICT`, `CASCADE`, `SET NULL [(cols)]`, `SET DEFAULT`; default `MATCH SIMPLE` (any NULL exempts the row); composite keys are table constraints; self-reference allowed; referencing columns are not auto-indexed. | Phase 4 subset: single and composite, `MATCH SIMPLE`, five actions, immediate checking only. | Deferrable constraints (needs end-of-transaction queue); unenforced "informational" FKs as in Snowflake (R10). | Matches the engine's immediate, per-statement model; deferral needs machinery rubiXDb lacks. |
| R3 | PostgreSQL 18 `ALTER TABLE` — https://www.postgresql.org/docs/18/sql-altertable.html | `ADD COLUMN` with a non-volatile default stores the default in metadata, no rewrite; `DROP COLUMN` hides the column, space reclaimed lazily; `RENAME COLUMN` metadata only; `ADD FOREIGN KEY … NOT VALID` then `VALIDATE CONSTRAINT`; `SET NOT NULL` scans. | Same shape as ADR D3/D31: metadata-only `ADD`, hidden `DROP`, `NOT VALID`/`VALIDATE` for FKs if a scan would block writers. | Eager table rewrite (rejected by ADR D3: unbounded DDL). | This is exactly the design D3 already chose; the gap is the reader (AB-1). |
| R4 | PostgreSQL 18 `WITH` — https://www.postgresql.org/docs/18/queries-with.html | Recursive evaluation = working table + intermediate table loop; `UNION` stops cycles only on whole-row repeats; `SEARCH` / `CYCLE` clauses; single-use side-effect-free CTEs are inlined, multi-use are materialized; `MATERIALIZED` hints; data-modifying CTEs run once, same snapshot, `RETURNING` is the only channel. | Existing implementation matches the evaluation model (C-7/C-8). Not adopted: inlining, hints, `SEARCH`/`CYCLE`, data-modifying CTEs (need R1 `RETURNING`). | Always-inline (loses materialization once-only guarantee). | Existing behaviour is bounded and tested; the extra forms are optional. |
| R5 | PostgreSQL 18 set operations — https://www.postgresql.org/docs/18/queries-union.html | `UNION`/`INTERSECT`/`EXCEPT` dedupe, `ALL` keeps; `INTERSECT` binds tighter; trailing `ORDER BY`/`LIMIT` apply to the whole operation unless parenthesized. | Already implemented as stated (D-1…D-5). | — | Verified by differential tests over all operator pairs. |
| R6 | PostgreSQL 18 window calls — https://www.postgresql.org/docs/18/sql-expressions.html | `ROWS`/`RANGE`/`GROUPS`, `EXCLUDE`, default frame `RANGE UNBOUNDED PRECEDING`, `FILTER` on window aggregates, window calls allowed only in SELECT list and `ORDER BY`, `OVER w` ≠ `OVER (w …)`, frame-start/end restrictions. | Implemented except `GROUPS`/`EXCLUDE` (E-9). `GROUPS` next: peer groups are already computed. | — | Cheap given existing peer-group code. |
| R7 | SQLite query planner — https://www.sqlite.org/optoverview.html | Nested loops only; planner reorders inner joins but never outer joins; polynomial join-order search; cost from `ANALYZE` statistics with documented default guesses when absent; automatic transient index ≈ hash join; covering indexes; subquery flattening (blocked by DISTINCT, aggregates, LIMIT). | Phase 5: keep outer joins fixed; order only inner joins; defaults when statistics are absent; flattening of derived tables as a separate, optional rule. | Full cost-based search from day one. | rubiXDb's planner is already a one-pass rule engine with per-rule differential tests; SQLite shows a small planner can be good with nested loops plus indexes — which is the current J-1/J-2. |
| R8 | DuckDB docs — https://duckdb.org/docs/current/guides/performance/join_operations.html and https://duckdb.org/docs/current/dev/profiling | Join order found by dynamic programming with greedy fallback for large joins; cost from base-table statistics; with the optimizer off it builds a left-deep tree in `FROM` order (rubiXDb's behaviour today); `EXPLAIN ANALYZE` reports actual cardinalities and per-operator metrics. A separate `build_side_probe_side` rule swaps join inputs. The pages I read do **not** state how the build side is chosen or whether estimates are shown beside actuals. | Phase 2 build side = smaller estimated side, fallback smaller written side; Phase 5 DP with greedy fallback and a node budget (`PlannerLimits::max_plan_nodes` precedent). `EXPLAIN ANALYZE` with real counters only. | Heuristic-only order (SQLite-style). | Build-side detail is **not** claimed from DuckDB; the choice rests on R9. |
| R9 | MIT 6.5830/6.830 join lectures — https://dsg.csail.mit.edu/6.5830/lectures/lec7.pdf, https://dsg.csail.mit.edu/6.5830/lectures/lec8.pdf, https://ocw.mit.edu/courses/6-830-database-systems-fall-2010/cf3d2f95cf16fa717f571771c3792d38_MIT6_830F10_lec09.pdf | Nested loop ≈ \|S\|+\|S\|\|R\|; hash join ≈ \|R\|+\|S\| when the build side fits in memory; sort-merge ≈ \|R\|+\|S\| on sorted inputs; Grace hash ≈ 3(\|R\|+\|S\|) with partitioning; hybrid hash keeps the first partition in memory. (Read from search snippets and the course pages; the PDFs were not read in full.) | In-memory hash join first (Phase 2); Grace/hybrid only with a spill subsystem (AB-4). Index nested loop stays for indexed inner sides. | Sort-merge first (needs sorted inputs; external sort absent). | In-memory hash join is the smallest step that removes the O(\|L\|×\|R\|) behaviour of J-1, reuses existing memory limits, and needs no new filesystem surface. |
| R10 | Snowflake constraints — https://docs.snowflake.com/en/sql-reference/constraints-overview | Standard tables enforce only `NOT NULL` and `CHECK`; `PRIMARY KEY`, `UNIQUE`, `FOREIGN KEY` are "optional, not enforced". | Documented as a **dialect difference** (§12): rubiXDb enforces PK and unique indexes and would enforce FKs. Not changed. | Make FKs informational (Snowflake behaviour). | Enforcement is the safer database behaviour (CLAUDE.md: correctness first); compatibility of metadata (`REFERENCES` accepted) is a separate decision. |
| R11 | Snowflake `INSERT` — https://docs.snowflake.com/en/sql-reference/sql/insert | `INSERT [OVERWRITE] INTO t [(cols)] { VALUES … \| query }`; the page lists no `RETURNING` and no `ON CONFLICT`. (Absence on that page only; other pages were not checked.) | Phase 1 follows PostgreSQL for `RETURNING` / `ON CONFLICT`; `OVERWRITE` not planned. | Snowflake `MERGE`. | — |
| R12 | Snowflake `ALTER TABLE` — https://docs.snowflake.com/en/sql-reference/sql/alter-table | `ADD COLUMN` default must be a literal; `DROP COLUMN` is metadata-only; `RENAME COLUMN`; `ALTER COLUMN` sets/drops `NOT NULL`/default/type; constraint add/drop with `[NOT] ENFORCED`, `RELY`. (First 100,000 of 110,852 characters read.) | Literal-only defaults match rubiXDb G-9. | — | — |
| R13 | Snowflake `QUALIFY` — https://docs.snowflake.com/en/sql-reference/constructs/qualify | Order `FROM, WHERE, GROUP BY, HAVING, WINDOW, QUALIFY, DISTINCT, ORDER BY, LIMIT`; select-list aliases usable; at least one window function required (in SELECT list or `QUALIFY`). | Implemented as documented (E-12). | — | — |
| R14 | Snowflake set operators — https://docs.snowflake.com/en/sql-reference/operators-query | `MINUS` = `EXCEPT`; `UNION [ALL] BY NAME`; `INTERSECT` tighter; names from the first query; mismatched types need explicit casts. | `MINUS` alias and `BY NAME` listed (D-9, D-10). | — | — |
| R15 | In-repo ADRs: `PHASE_RELATIONAL_DATABASE_ADR.md` D3, D16, D17, D18, D31; `sql/MISSION1_FEATURE_MATRIX.md`; `sql/MISSION2_FEATURE_MATRIX.md` | Design intent and per-feature decisions used above. | — | — | Authoritative for intent; source and tests are authoritative for behaviour. |
| std | ISO/IEC 9075 | Cited by the earlier mission matrices for window and grouping semantics. The standard text itself was **not** consulted in this audit. | — | — | — |

## 12. Dialect differences (do not read as compatibility claims)

| Topic | rubiXDb today | PostgreSQL 18 | Snowflake (per pages read) |
|---|---|---|---|
| Unquoted identifier folding | folds to **lower case** (`convert_ident`; `parse_tests::quoted_identifiers_preserve_case_unquoted_fold_to_lowercase`) | lower case | not verified in this audit |
| `PRIMARY KEY` / `UNIQUE` | **enforced** | enforced | **not enforced** on standard tables (R10) |
| `FOREIGN KEY` | refused | enforced | accepted, **not enforced** (R10) |
| `CHECK` | refused | enforced | enforced (R10) |
| `RETURNING` | refused | supported | not on the INSERT page (R11) |
| `ON CONFLICT` | refused | supported | not on the INSERT page (R11); `MERGE` is the idiom |
| `QUALIFY` | supported (select aliases usable) | not supported | supported (R13) |
| `MINUS` / `UNION BY NAME` | refused | not supported | supported (R14) |
| `GROUPS` frames / `EXCLUDE` | refused | supported (R6) | not verified |
| `LIMIT`/`FETCH`, `OFFSET` | `LIMIT n OFFSET m` | both | not verified |
| Recursive CTE sees earlier sibling CTEs | **no** (decision C-12) | yes | yes (`OPEN_ITEMS.md` 2026-10-12) |
| Single-use CTE | always materialized once | inlined | not verified |
| `ORDER BY` alias / ordinal | not supported in plain `SELECT` | supported | supported (QUALIFY page shows alias use; ORDER BY not verified) |
| Isolation | snapshot; write skew possible | READ COMMITTED default, SERIALIZABLE available | not verified |

## 13. Not tested / open

* The three test runs of §2 pre-date the concurrent `sql/` + `api/` edits described in §1; they were not re-run against the edited tree.
* The frontend and CLI suites, the other API test binaries and the core-crate relational and catalog tests were **not run** (§2). Rows citing them say so.
* No statement was executed against a live server. Refusals that no test pins (G-2…G-7, H-*, I-1…I-3, I-12, C-10, C-13, C-14, B-6, N-2…N-4) are from source (`convert.rs`) and corroborated, where noted, by the 2026-10-02 probe table in `PHASE_RUBIXDB_FINAL_SINGLE_NODE_ARCHITECTURE.md`. **Phase 1 should begin by adding pinning tests for every unpinned refusal in the phase's scope.**
* DDL inside an explicit transaction; behaviour of statement/plan caches under `RENAME`: not audited.
* Whether Snowflake accepts `COUNT(DISTINCT a, b)` and identifier folding: not verified.
* Power-loss behaviour of any SQL feature: **NOT TESTED** (inherited from `PROJECT_STATE.md`; the durability mission is separate and in progress).
* Performance of every row: no measurement was taken; no row is a performance PASS.

## 14. Protected paths and git state

* Files created by this mission: exactly one, `docs/SNOWFLAKE_SQL_COMPATIBILITY.md`. Files in `sql/` and `api/` that became modified during the session were not touched by this mission (§1, "Concurrent change observed").
* `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`, `Cargo.toml`, `Cargo.lock`, `tests/repo_hygiene.rs`: **zero diff introduced by this mission.** Pre-existing uncommitted durability-mission diffs exist in `src/wal/`, `src/manifest/` and `Cargo.toml`; the combined `git diff` of the protected set hashed to `3f5d912f2590c696b94f53019ea245f5b227fc8841cbab6928455e9080e2c86d` before this audit and `3f5d912f2590c696b94f53019ea245f5b227fc8841cbab6928455e9080e2c86d` after it (identical).
* `Cargo.lock`: unchanged (all cargo runs used `--locked`).
* The "tracked credentials file" named in the mission is `frontend/.e2e-crossbrowser-data/default/credentials.json` (the only tracked path matching `credentials`); it was not opened or modified.
* Commit: **none** (uncommitted, as instructed).


---

## 15. Phase 1 (SQL correctness gaps) -- appended 2026-10-14

This section is **appended**; sections 0-14 above are the Phase 0 audit at `dc5dc44` and are unchanged. Where they disagree with this section, this section is newer. Status words are the Phase 0 vocabulary. Nothing here is a claim of Snowflake compatibility.

Phase 1 implemented the rows of sections 4-5 that were `PARTIALLY_SUPPORTED` or `NOT_IMPLEMENTED` and belong to the SQL **correctness** surface (aggregation, subqueries, set operations, windows, grouping sets, `ORDER BY`). Not touched, because they are Phases 2-5: `RETURNING`, `ON CONFLICT`, `INSERT ... SELECT`, `ALTER TABLE`, foreign keys, join algorithms, the optimizer, statistics, `EXPLAIN ANALYZE`.

### 15.1 Rows whose status changed

| ID | Feature | Phase 0 status | Status now | Evidence (tests run in this phase) |
|---|---|---|---|---|
| A-4 | Several `DISTINCT` aggregates over different columns in one `SELECT` | IMPLEMENTED_NOT_FULLY_VERIFIED | IMPLEMENTED_AND_TESTED | `phase1_tests::count_distinct::several_distinct_aggregates_over_different_columns_in_one_select` |
| A-5 | `COUNT(a, b, ...)` and `COUNT(DISTINCT a, b, ...)` | NOT_IMPLEMENTED | IMPLEMENTED_AND_TESTED | `phase1_tests::count_distinct::*` (11 tests: no-NULL rule, distinct combinations, per group, empty / all-NULL / single value / 300 combinations, collision-free encoding, `-0.0` / `NaN`, `FILTER`, `HAVING`, as a window aggregate, errors, EXPLAIN, state bound), `api_sql_phase1::count_of_several_expressions_and_group_by_all_over_http` |
| A-8 | Aggregate overflow | IMPLEMENTED_NOT_FULLY_VERIFIED | IMPLEMENTED_AND_TESTED (SUM paths) | `phase1_tests::overflow::*`: `SUM(BIGINT)` plain / grouped / window / sliding window / `DISTINCT` return a typed error, a negative partner is no false positive, `SUM(INTEGER)` widens, `SUM(DECIMAL(38,0))` overflows its precision. `COUNT` counters are `u64` and `AVG` accumulates in floating point: neither has an overflow a test can reach. |
| A-9 | `GROUP BY ALL` | NOT_IMPLEMENTED | IMPLEMENTED_AND_TESTED | `phase1_tests::group_by_all::*` (5), `api_sql_phase1` |
| B-6 | `x <op> ANY / SOME / ALL (subquery)` | NOT_IMPLEMENTED | PARTIALLY_SUPPORTED | Implemented and tested: `phase1_tests::any_all::*` (9) and `subquery_edges::*` (3). **Still not implemented**: row-value forms (`(a, b) IN (SELECT ...)`, `ROW(...) = ANY (...)`) and `ANY` over an array; both are pinned as refusals (`any_all::typing_and_other_errors`). |
| D-7 | `ORDER BY` of a set operation | PARTIALLY_SUPPORTED | IMPLEMENTED_AND_TESTED | `order_by::set_operation_positions_and_aliases` (positions, output names, `LIMIT` / `OFFSET`, out-of-range) |
| D-9 | `MINUS` | NOT_IMPLEMENTED | IMPLEMENTED_AND_TESTED | `set_ops::minus_is_except` (also `MINUS ALL`, precedence), `api_sql_phase1::minus_and_union_by_name_over_http` |
| D-10 | `UNION [ALL] BY NAME` | NOT_IMPLEMENTED | IMPLEMENTED_AND_TESTED | `set_ops::union_by_name_*` (6 tests: reordering, `NULL` fill, type unification, dedup vs `ALL`, empty operands, chains, a set operation as operand, `ORDER BY` / `LIMIT`, case rule, errors, recursive CTE refused) |
| E-9a | `GROUPS` frames | NOT_IMPLEMENTED (row E-9) | IMPLEMENTED_AND_TESTED | `phase1_window_tests::groups::*` (8) and the oracle `oracle::groups_rows_and_range_frames_match_the_oracle` (over 1,500 frame combinations x 15 functions, `GROUPS` against an independent from-the-definition model) |
| E-9b | `IGNORE NULLS` / `RESPECT NULLS` on `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE` | NOT_IMPLEMENTED (row E-9) | IMPLEMENTED_AND_TESTED | `phase1_window_tests::ignore_nulls::*` (5) and `oracle::lag_and_lead_ignore_nulls_match_the_oracle` (over 1,000 values) |
| E-9c | Frame `EXCLUDE` | NOT_IMPLEMENTED (row E-9) | BLOCKED_BY_ARCHITECTURE | `sqlparser` 0.63.0 (pinned `=0.63.0` in `sql/Cargo.toml`, whose workspace `Cargo.lock` is a protected path) has no `EXCLUDE` in its `WindowFrame` (`// TBD: EXCLUDE`). The refusal is pinned: `groups::frame_exclusion_stays_refused_because_the_parser_has_no_such_syntax`. Unblocking needs a parser upgrade or a pre-parser, i.e. a dependency decision. |
| E-9d | `NTH_VALUE ... FROM FIRST / LAST` | NOT_IMPLEMENTED (row E-9) | NOT_IMPLEMENTED | now pinned: `ignore_nulls::only_the_value_functions_accept_it` |
| E-10 | `DISTINCT` inside a window aggregate | NOT_IMPLEMENTED | IMPLEMENTED_AND_TESTED | `phase1_window_tests::window_distinct::*` (5) and the oracle's `SUM / COUNT / AVG(DISTINCT x)` over `ROWS`, `GROUPS`, `RANGE` frames |
| F-5 | Correlated subquery that reads a grouped column under `ROLLUP` / `CUBE` / `GROUPING SETS` | NOT_IMPLEMENTED | IMPLEMENTED_AND_TESTED | `phase1_tests::grouping_sets_correlated::*` (3) |
| L-11 | `ORDER BY` select-list alias and position in a plain `SELECT` | NOT_IMPLEMENTED | IMPLEMENTED_AND_TESTED | `phase1_tests::order_by::*` (10), `api_sql_phase1::order_by_positions_and_aliases_over_http` |

Rows not in this table keep the Phase 0 status.

### 15.2 Counts after Phase 1

Features audited: **140** (Phase 0's 137, with row E-9 split into E-9a..E-9d).

| Classification | Phase 0 | Phase 1 |
|---|---|---|
| IMPLEMENTED_AND_TESTED | 61 | 73 |
| IMPLEMENTED_NOT_FULLY_VERIFIED | 5 | 3 |
| PARTIALLY_SUPPORTED | 9 | 9 |
| NOT_IMPLEMENTED | 54 | 46 |
| BLOCKED_BY_ARCHITECTURE | 3 | 4 |
| NOT_PLANNED | 5 | 5 |
| **Total** | **137** | **140** |

### 15.3 References consulted, approach, alternatives, dialect decision

| Feature | Reference | Chosen approach | Alternatives | Dialect decision |
|---|---|---|---|---|
| `ORDER BY` alias / position | Snowflake `ORDER BY` page (https://docs.snowflake.com/en/sql-reference/constructs/order-by): "column alias appearing in the SELECT list", "position of an expression in the SELECT list"; the `SELECT` page warns against an alias equal to another column and is silent on `ORDER BY` precedence. PostgreSQL: output names win in a bare `ORDER BY name`. | An unsigned integer literal is a 1-based position (out of range: bind error). A bare identifier equal to an explicit select-list alias is that item, even over a same-named `FROM` column; inside a larger expression the `FROM` column is seen first and the alias is the fallback. Qualified names never name an alias. | (a) alias only when no column has the name (QUALIFY's rule here, Snowflake-leaning): rejected for bare names because it silently sorts by the wrong thing for `SELECT -x AS x ... ORDER BY x`; (b) PostgreSQL's "no alias inside expressions": rejected, Snowflake allows aliases in later clauses. | **Behaviour change**: `ORDER BY 1` used to be a constant sort key; it is now position 1. No existing test pinned the old behaviour (the full `rubixdb-sql` suite passed without edits for it). |
| `MINUS` | Snowflake set operators (https://docs.snowflake.com/en/sql-reference/operators-query): `MINUS` and `EXCEPT` interchangeable. | converter maps `MINUS` to `EXCEPT` (`ALL` accepted too). | a separate operator | Superset: `MINUS ALL` is not in the Snowflake page. |
| `UNION [ALL] BY NAME` | same page: match by name, unquoted case-insensitive / quoted case-sensitive, missing columns `NULL`. | Bind-time alignment `NameAlignment {left, right}`: result = left columns, then right-only columns; the executor maps each operand row into it (a missing column is `NULL`); types unify per result column; names from the first operand that has the column. | Rewriting each operand's select list: rejected, a nested set operation's rows are produced by the set operator, not its projection. | Stricter than the page where it is silent: no common name at all, or a duplicated name inside one operand, is a bind error; `BY NAME` only for `UNION`; refused in a recursive CTE body. |
| `COUNT(a, b)` | Snowflake `COUNT` (https://docs.snowflake.com/en/sql-reference/functions/count): `COUNT([DISTINCT] expr1 [, expr2 ...])`, `DISTINCT` "applies to all columns"; the page does not state the `NULL` rule for several expressions. | `AggregateArg::Multi`; a row counts when **no** argument is `NULL` (the rule the same page gives for `COUNT(alias.*)`; MySQL's); the arguments fold to one injective byte string (type tag, length-prefixed payloads, canonical `-0.0` / `NaN`) that the existing `DISTINCT` seen-set stores. | a tuple type: rejected, no row type exists. | `NULL` rule for several expressions **not verified** against Snowflake. Only `COUNT` takes several arguments. |
| `DISTINCT` window aggregates | PostgreSQL 18 (https://www.postgresql.org/docs/18/sql-expressions.html): rejected; Snowflake `COUNT` page: allowed with `OVER ... ORDER BY` for one column, a multi-column `DISTINCT` count "not valid when the OVER clause also contains ORDER BY". | The window operator feeds an `AggregateAccumulator` per frame (the one `DISTINCT` machinery `GROUP BY` uses); the seen-set counts against `max_aggregate_state_bytes`. Running frames keep one accumulator (`current()` copies only the small state). | O(n) incremental multiset for sliding frames: not needed, sliding frames already recompute per row. | **Superset** of both: any frame, all five aggregates, several columns with or without `ORDER BY`. |
| `GROUP BY ALL` | Snowflake `GROUP BY` page (https://docs.snowflake.com/en/sql-reference/constructs/group-by): groups by every select item that does not use an aggregate; all-aggregate select lists behave as no `GROUP BY`. Silent on window functions and constants. | Keys = select items containing no aggregate, no window function and that are not constants/parameters (a constant key would turn the one row of an empty global aggregate into zero groups). `GROUPING()` is refused (no written `GROUP BY` list to name). | PostgreSQL has no `GROUP BY ALL`. | Window / constant handling is this engine's choice. |
| `ANY` / `SOME` / `ALL` | PostgreSQL 18 (https://www.postgresql.org/docs/18/functions-subquery.html): the three-valued tables, `= ANY` is `IN`, `<> ALL` is `NOT IN`, an empty set is `FALSE` / `TRUE`; Snowflake (https://docs.snowflake.com/en/sql-reference/operators-subquery): six operators, single column, "supported only in a WHERE clause" and not under `OR`; `NULL` and empty-set behaviour not stated. | New `SubqueryKind::Quantified { op, all }`; the inner column is materialized (bounded by `max_materialized_rows` / `_bytes`, cached when uncorrelated) and folded with the engine's own comparison; tests pin `= ANY == IN` and `<> ALL == NOT IN` over five sets, `NULL`s included. | Rewriting `= ANY` to `IN`: rejected, the general path is the one that has to be right and the equivalence is a test. | **Superset** of Snowflake's placement rule (also select list, `HAVING`, DML). `SOME` accepted. |
| `GROUPS` frames | PostgreSQL 18 window-call syntax (same page as above). Snowflake's window documentation lists `ROWS` / `RANGE`; `GROUPS` there was not verified. | Peer groups are numbered once per partition; a start before the first group begins at the partition start, one past the last group is empty; an end before the first group is empty. Requires `ORDER BY`; offsets are non-negative integer constants. | Reusing the `RANGE` machinery: rejected, `GROUPS` offsets count groups, not key distance. | PostgreSQL extension. |
| `IGNORE NULLS` | Snowflake `LAG` page (https://docs.snowflake.com/en/sql-reference/functions/lag): `{IGNORE | RESPECT} NULLS` after the call, rows with a `NULL` expression are skipped when counting `offset`; `RESPECT NULLS` default. | Per partition, a positions-of-non-`NULL` list and a prefix count turn "k-th non-`NULL` row before / after / from here" into an index (`O(1)` per row). Offset 0 is the current row. Accepted on `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE`; refused elsewhere. | A per-row scan: rejected, `O(n^2)` on sparse data. | Same syntax as Snowflake. The `LAG` page also says a **negative offset behaves like `LEAD`**; here a negative offset is still a bind error (OPEN, below). |
| F-5 | ISO/IEC 9075-2 (via the Mission 2 matrix): a grouping column is `NULL` in the rows of a grouping set that omits it. | `validate_group_compat` already limits a correlated subquery to bare grouping columns; `AggregateOp` now makes such a column `NULL` in the representative row of every set that leaves it out, so the subquery sees the standard's value. | Rewriting inside the subquery: rejected, the subquery's tree is a separate plan. | Standard. |

### 15.4 Older assertions that pinned a refusal Phase 1 removed (not weakened: each was true of the engine before and is false by design now)

`CLAUDE.md` forbids changing a test to make it pass unless the test is proven wrong; these assertions are not wrong, they are superseded by the mission's own scope, as in Mission 2 section 6.2. Each was edited in place to assert the new behaviour or a refusal that remains, with a comment naming the replacement test.

| Test | Pinned | Now |
|---|---|---|
| `aggregation_tests::exec_2f::distinct_is_not_allowed_on_the_wildcard_nor_on_a_window_aggregate` | `COUNT(DISTINCT x) OVER ()` is `UNSUPPORTED` | binds; still refused: `SUM(DISTINCT x, y)`, `COUNT(DISTINCT *)` |
| `aggregation_tests::parser::what_stays_outside_the_grammar_is_a_typed_error` | `GROUP BY ALL` parses to an error | case removed |
| `aggregation_tests::exec_2e::a_correlated_subquery_over_a_grouped_column_is_refused_with_grouping_sets` (renamed `..._works_with_grouping_sets`) | F-5 refusal | binds; a non-grouped column is still a `TypeMismatch` |
| `bind_tests::group_by_all_and_rollup_remain_explicitly_unsupported` | `GROUP BY ALL` unsupported | binds with one key |
| `bind_tests::count_distinct_remains_explicitly_unsupported` | window `DISTINCT` unsupported | binds, flag kept |
| `parse_tests::unsupported_grammar_is_a_typed_error_not_a_panic` | `GROUP BY ALL` | case removed |
| `setop_tests::parser::what_is_still_not_a_set_operation_is_a_typed_unsupported_error` | `MINUS`, `UNION [ALL] BY NAME` | `INTERSECT BY NAME`, `EXCEPT BY NAME` |
| `window_tests::parser::what_is_still_outside_the_grammar_after_mission_2_is_a_typed_error` | `GROUP BY ALL` | case removed |
| `window_tests::parser::what_stays_outside_the_grammar_is_a_typed_error` | `GROUPS`, `IGNORE NULLS` | frame `EXCLUDE`, `FROM LAST` |
| `api_sql_integration::set_operation_errors_and_limits_over_http` | `MINUS`, `UNION BY NAME` give 400 | `INTERSECT BY NAME` |
| `api_sql_mission2::illegal_frames_are_bind_errors_and_groups_frames_are_unsupported` | `GROUPS`, window `DISTINCT`, `IGNORE NULLS` give `UNSUPPORTED` | a `FILTER` on a non-aggregate window function |

### 15.5 Still open after Phase 1 (each row's Phase 0 status stands unless listed in 15.1)

* Row-value comparison (`(a, b) IN (SELECT ...)`) and `ANY` over arrays (B-6, the unimplemented half).
* Frame `EXCLUDE` (E-9c): needs a parser change; `NTH_VALUE ... FROM LAST` (E-9d).
* `LAG` / `LEAD` with a negative offset (Snowflake: acts as the other function): bind error here, by an earlier, tested decision (`window_tests`); Snowflake behaviour from the `LAG` page.
* Other aggregates (A-10), scalar functions and `CAST` (N-3, N-4) and the coercions that need them (D-6).
* Decorrelation (B-5), `SEARCH` / `CYCLE` (C-10), the recursive-CTE visibility decision (C-12), `MATERIALIZED` hints (C-13).
* The frontend IDE keyword list and autocompletion do not know the new forms (`MINUS`, `BY NAME`, `GROUPS`, `IGNORE NULLS`, `ANY`, `ALL`, `GROUP BY ALL`); N-7.
* Performance of every new row: not measured; no new row is a performance PASS. `DISTINCT` window frames that slide recompute per row (`O(rows x width)`), like the other sliding aggregates (E-5); `SELECT DISTINCT` is still `O(n^2)` (K-8, Phase 2).
* Power-loss behaviour: NOT TESTED (no new row touches persistence).


### 15.6 Regression (2026-10-14, every cargo run with `--locked` and a private target directory)

Baseline for "pre-existing" is a clean checkout of `dc5dc44` (a temporary `git worktree`, removed afterwards).

| Command | Result | Classification |
|---|---|---|
| `cargo fmt --all -- --check` | FAIL | **Pre-existing**: 28 files have rustfmt differences at clean `dc5dc44`; 27 now (`sql/src/convert.rs` was formatted by another session). Phase 1 left **0 newly unformatted files**: the 3 files it created and the 4 it changed that became unformatted (`aggregation_tests.rs`, `bind/window.rs`, `bound.rs`, `exec/window.rs`) were formatted with `rustfmt`. |
| `cargo check --workspace --all-targets --all-features` | PASS | |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS (0 warnings) | |
| `cargo test --workspace --no-fail-fast` (debug) | 2,286 passed, 2 failed, 32 ignored | The 2 failures are `repo_hygiene::no_tracked_credentials_json` and `::no_tracked_file_contains_a_64_hex_admin_key_literal`: the tracked throwaway credential `frontend/.e2e-crossbrowser-data/default/credentials.json`, in `dc5dc44`'s tree, recorded in `OPEN_ITEMS.md` (2026-10-04). **Pre-existing.** |
| `cargo test --release --workspace --no-fail-fast` | 2,287 passed, 3 failed, 30 ignored | The same 2, plus `group_commit::m1_3_thousand_writers_throughput` (60,146 ops/s in the full run, target 80,000). **Pre-existing**: at clean `dc5dc44` the same test, alone, also fails (78,314 ops/s); it is the WAL throughput gate M1.3 recorded in `PROJECT_STATE.md`. It is in the protected WAL area and unrelated to SQL. |

Per package, in both profiles: `rubixdb-sql` lib **1,060 passed, 0 failed, 22 ignored** (the 22 are benchmarks); `rubixdb-api` `api_sql_integration` 47, `api_sql_mission2` 13, `api_sql_phase1` 6 (new), `api_sql_schemas` 8 (another session's), all passed. `sql/tests/write_crash_consistency` (needs `--features test-util`): 2 passed when run in Phase 0, and no code under it changed.

One **timing-dependent pre-existing test**: `recursive_tests::contexts::the_recursion_reads_the_statements_snapshot_while_another_session_writes` failed once in a loaded run before the final tree and fails every time **when run alone**, at clean `dc5dc44` as well as on this tree (25 s and 27 s): the writer thread appends rows while the recursion walks the chain, and when the statement is slow enough the chain exceeds `max_recursive_cte_depth` (1,000). It passes inside the full parallel suite (both profiles above). Not changed; recorded in `OPEN_ITEMS.md`.

Phase 1 added **83 tests**: 77 in `rubixdb-sql` (`phase1_tests`: order_by 10, set_ops 8, count_distinct 11, overflow 3, group_by_all 5, any_all 9, grouping_sets 4, subquery_edges 3, grouping_sets_correlated 3 = 56; `phase1_window_tests`: groups 8, ignore_nulls 5, window_distinct 5, oracle 3 = 21) and 6 in `rubixdb-api` (`api_sql_phase1`). The lib went from 983 tests (979 at `dc5dc44` plus 4 parse tests of another session) to 1,060. The 11 older assertions edited in 15.4 are not counted as new.



## 16. Phase 2a (DML extensions) -- appended 2026-10-15

This section is **appended**; sections 0-15 above are unchanged. Where they disagree with this section, this section is newer. Status words are the Phase 0 vocabulary plus the Phase 2a mission's (`PASS` / `FAIL` / `OPEN` / `NOT REQUIRED` / `NOT TESTED` / `NOT IMPLEMENTED`). Nothing here is a claim of Snowflake compatibility: Snowflake's `INSERT` page (R11) lists neither `RETURNING` nor `ON CONFLICT`; the semantics below follow PostgreSQL 18 (R1), as Phase 0 section 9 recommended.

Phase 2a implemented the DML rows G-2, G-3, G-4, G-5 and the part of C-6 that depended on G-2. Not touched, because they are other phases or out of scope: `ALTER TABLE`, foreign keys, join algorithms, the optimizer, `MERGE` (G-6), data-modifying CTEs (C-14), `UPDATE ... FROM` (G-7), the CLI renderer and the frontend. No file under `src/` changed, so nothing in the certified engine moved: the write path is the existing `Transaction` / `TransactionManager` (`put_row`, `delete_row`, `commit`) and no WAL, MANIFEST or SSTable format changed.

### 16.1 Rows whose status changed

| ID | Feature | Phase 0 status | Status now | Evidence (tests run in this phase) |
|---|---|---|---|---|
| G-2 | `INSERT ... SELECT` (any query body: `SELECT`, set operations, `VALUES`, `WITH`) | NOT_IMPLEMENTED | PASS | `phase2a_tests::insert_select::*` (7): column list and defaults, source read before anything is written, query forms, numeric widening and the one checked narrowing, count and NOT NULL errors change nothing, bounded by `max_dml_target_rows`, EXPLAIN; `on_conflict::insert_select_with_on_conflict`; `statement_atomicity::insert_select_failing_part_way_changes_nothing`; `authorization::insert_select_needs_select_on_its_source`; `api_sql_phase2a::insert_select_over_http` |
| G-3 | `INSERT / UPDATE / DELETE ... RETURNING` | NOT_IMPLEMENTED | PASS | `phase2a_tests::returning::*` (11): every row of a multi-row insert, names / types / expressions / wildcards / parameters, UPDATE new values, UPDATE with a subquery in `SET`, DELETE old values, returned count = changed count on every path, inside a transaction, failed statement returns nothing, bounded, what a RETURNING list may contain, EXPLAIN; `restart::returned_rows_are_durable_and_the_upsert_is_idempotent_across_a_restart`; `api_sql_phase2a::{insert_returning_is_a_rows_result_with_columns_and_every_row, update_and_delete_returning_over_http, returning_inside_a_session_follows_the_transaction, returned_rows_survive_a_restart_of_the_engine}` |
| G-4 | `ON CONFLICT [target] DO NOTHING` | NOT_IMPLEMENTED | PASS | `phase2a_tests::on_conflict::{do_nothing_skips_the_conflicting_row_and_inserts_the_rest, do_nothing_without_a_target_arbitrates_on_every_unique_constraint, rows_proposed_earlier_in_the_same_statement_are_seen_by_later_ones, metrics_count_skipped_conflict_rows}`, `concurrency::{two_transactions_inserting_the_same_key_leave_exactly_one_row, concurrent_do_nothing_inserts_of_one_key_insert_it_exactly_once}`, `statement_atomicity::on_conflict_do_nothing_sees_the_transactions_own_uncommitted_rows` |
| G-5 | `ON CONFLICT (target) DO UPDATE SET ... [WHERE ...]`, `EXCLUDED` | NOT_IMPLEMENTED | PASS | `on_conflict::{do_update_replaces_the_row_and_excluded_is_the_proposed_row, excluded_carries_the_defaults_of_omitted_columns, do_update_where_filters_which_conflicts_update, a_unique_index_can_be_the_conflict_target, a_target_that_is_not_a_unique_constraint_is_refused_at_bind_time, do_update_set_is_validated_at_bind_time, one_statement_cannot_update_the_same_row_twice, upsert_is_idempotent, an_update_that_changes_nothing_is_still_counted_and_returned}`, `concurrency::*` (6), `statement_atomicity::*` (11), `restart::*` (3), `authorization::*` (3), `api_sql_phase2a::{on_conflict_do_nothing_and_do_update_over_http, concurrent_upserts_over_http_make_one_row_and_lose_no_update, failures_map_to_the_documented_http_errors_and_change_nothing}` |
| C-6 | `WITH` before `INSERT` | PARTIALLY_SUPPORTED | PASS | the `WITH` list is now read by the `INSERT ... SELECT` source: `insert_select::source_query_forms` |
| C-14 | Data-modifying CTEs | NOT_IMPLEMENTED | NOT IMPLEMENTED (unchanged) | `RETURNING` now exists, which was its prerequisite; a CTE body is still a query only. Out of Phase 2a scope. |
| G-6 | `MERGE` | NOT_IMPLEMENTED | NOT IMPLEMENTED (unchanged) | out of scope |

New rows found while implementing (not in the Phase 0 matrix):

| ID | Feature | Status | Evidence / reason |
|---|---|---|---|
| G-4a | `ON CONFLICT ON CONSTRAINT <unique index name>` | PASS | `on_conflict::a_unique_index_can_be_the_conflict_target`. The primary key has no name in the catalog's index list, so `ON CONSTRAINT <table>_pkey` is not accepted (a bind error "constraint ... does not exist"); name the key columns instead. |
| G-4b | `INSERT INTO t AS alias ...` | NOT IMPLEMENTED | `sqlparser` 0.63 parses the alias only for the PostgreSQL dialect; this crate parses with `GenericDialect` (D14). Pinned as a clean parse error by `on_conflict::an_insert_target_alias_is_refused_by_the_parser_dialect`. The existing row is reachable as `<table>.<column>`. |
| G-4c | `INSERT ... ON DUPLICATE KEY UPDATE`, `INSERT ... OUTPUT`, `UPDATE ... FROM ... RETURNING` | NOT IMPLEMENTED (refused) | `parser::what_stays_unsupported_is_refused_with_a_parse_error` |
| G-4d | Multi-column `ON CONFLICT` target in any order; superset of a unique key | PASS / refused | the column set must equal the primary key or a ready unique index, in any order: `a_target_that_is_not_a_unique_constraint_is_refused_at_bind_time` |

### 16.2 Semantics as implemented (each is pinned by a test named in 16.1)

References: PostgreSQL 18 `INSERT` (R1, https://www.postgresql.org/docs/18/sql-insert.html: conflict target, `EXCLUDED`, `DO UPDATE ... WHERE`, rows affected twice, `RETURNING` returns only rows actually inserted or updated); Snowflake `INSERT` (R11): no `RETURNING`, no `ON CONFLICT` on that page (absence on that page only).

* **`RETURNING` reports the rows the statement changed, not a second query.** The rows are produced inside the write executor from the very row values handed to the transaction (`ReturningSink`): the written row, with column `DEFAULT`s applied, for `INSERT`; the new row for `UPDATE` and for `DO UPDATE`; the deleted row as it was for `DELETE`. A conflicting row skipped by `DO NOTHING`, or one whose `DO UPDATE ... WHERE` is not true, is neither counted nor returned. The returned count equals `rows_affected` on every path (`returning::the_returned_count_is_the_changed_count_on_every_path`). A `RETURNING` list may hold expressions, `*`, `t.*`, parameters; aggregates, window calls and subqueries are refused at bind time.
* **Result shape.** A statement with `RETURNING` answers with the existing `rows` result (`columns`, `rows`, `row_count`), with no new client contract; a statement without it still answers with the `write` result and its count. A `RETURNING` result is bounded by `max_result_rows` and a statement that exceeds it fails and changes nothing.
* **A statement is all or nothing.** `INSERT` runs in two phases: every proposed row is decided (inserted, skipped, updated, or an error) against a shadow of the statement's own earlier rows, and only when the whole statement has been decided are the writes buffered into the transaction. A failing statement therefore leaves its transaction exactly as it was: tested in autocommit and inside an explicit transaction (`statement_atomicity::a_failed_statement_inside_a_transaction_...`).
* **Defined failure behaviour** (the mission's four cases): a primary-key duplicate or an unsatisfiable unique index in a multi-row insert aborts the whole statement with `Conflict` (HTTP 409 `CONFLICT_ERROR`); an error while evaluating `DO UPDATE` (division by zero, `NOT NULL` column set to `NULL`) aborts the whole statement, including rows already decided as plain inserts (HTTP 400 `EXECUTION_ERROR`); a `DO UPDATE` that would give one row a unique-index value another row holds fails at commit with `Conflict`; `RETURNING` combined with `ON CONFLICT` returns rows only when the statement succeeds. The proposed row is checked for `NOT NULL` before any conflict is looked at, so a `NULL` in a `NOT NULL` column fails even where the row would only have conflicted.
* **Secondary (unique-index) constraints.** Unchanged from the certified transaction layer: a plain `INSERT` finds a unique-index duplicate when the transaction commits, not at the statement; in autocommit that is the end of the statement. A unique index is arbitrated at statement time only where `ON CONFLICT` names it, or `DO NOTHING` has no target (the primary key and every ready unique index then arbitrate). `ON CONFLICT (k)` does not cover a violation on another unique index; that stays an error (`do_nothing_without_a_target_arbitrates_on_every_unique_constraint`).
* **The upsert is atomic under concurrency.** The transaction layer is snapshot isolation with first-committer-wins on written keys and unique-index existence; two upserts of one key therefore cannot both commit from the same snapshot. In **autocommit**, a statement that loses that race is run again from a fresh snapshot (bounded by `ExecLimits::max_upsert_attempts`, default 32, jittered backoff, `rows_affected` / `RETURNING` come from the successful attempt only), and the retry sees the winner's row and arbitrates on it: 4 threads x 10 upserts of one new key end as one row with `n = 40`, 8 racing `DO NOTHING` inserts report exactly 1 inserted row in total, 4 racing upserts on a unique-index key end as one row (`concurrency::*`, repeated 25 times, 25/25 passed; HTTP: 4 tasks x 5 upserts, 6 repeats, all passed). If the attempts are exhausted the caller gets `Conflict` (HTTP 409), never a duplicate or a lost update (`a_retry_is_bounded_and_reports_the_conflict_when_it_cannot_win`). Inside an **explicit transaction** the snapshot belongs to the caller, so nothing is retried: the loser's `COMMIT` fails with a conflict and its retry sees the winner (`two_transactions_upserting_the_same_key_lose_no_update_on_retry`). The first default (8 attempts) was too small for 4-way contention on one key in an HTTP test, which is why the default is 32; the worst case adds roughly a third of a second of backoff before a 409.
* **`DO UPDATE` rules.** A conflict target is required (PostgreSQL); the target is the primary key's columns or a ready unique index's columns (any order), or a unique index by name. `SET` values and `WHERE` see two relations of the table: the existing row under the table's name and the proposed row as `EXCLUDED`, so a bare column name is ambiguous (as in PostgreSQL). `EXCLUDED` carries the `DEFAULT` of every column the `INSERT` did not name. Assigning a primary-key column is refused (v1: D6). A subquery in the conflict action is refused. A statement that proposes the same key twice to `DO UPDATE` fails with a cardinality error and changes nothing (R1); `DO NOTHING` skips the repeat. A `DO UPDATE` row counts in `rows_affected` and `RETURNING` whether or not a value changed.
* **A transaction sees its own rows.** Conflict detection reads through the transaction's overlay: an upsert over a row inserted earlier in the same transaction updates it; over a row deleted earlier in the same transaction it inserts afresh (`statement_atomicity::on_conflict_do_nothing_sees_the_transactions_own_uncommitted_rows`, `an_upsert_over_a_row_deleted_in_the_same_transaction_inserts_it_afresh`).
* **`INSERT ... SELECT`.** The source query is fully executed before the first write; its output columns are matched to the target positionally (count must match; omitted target columns take their `DEFAULT` or `NULL`). Types: the same type, a lossless numeric widening (`INTEGER` to `BIGINT` / `DOUBLE`, ...), and `BIGINT` to `INTEGER` with a per-value range check at execution; nothing else converts (`TEXT` to `INTEGER` and `DOUBLE` to an integer type are bind errors). The number of source rows is bounded by `max_dml_target_rows`. Combined with `ON CONFLICT`, each source row is a proposed row.
* **Privileges** (table level, D25): `INSERT` needs `INSERT`; `RETURNING`, `ON CONFLICT` and `INSERT ... SELECT`'s source need `SELECT` (reading rows back or arbitrating on them); `DO UPDATE` also needs `UPDATE`; `UPDATE ... RETURNING` / `DELETE ... RETURNING` need `SELECT` as well. A principal lacking one gets the same `UnknownObject` outcome as a missing table (`authorization::each_clause_needs_the_privilege_it_actually_exercises`). A `reader` key cannot write over HTTP with or without `RETURNING`.
* **EXPLAIN.** Shows `source=select` / `rows=N`, `on_conflict=do_nothing(arbiters=[...])` or `do_update(arbiter=[...], set=N[, where])`, and `returning=[names]`, and nests the source plan.
* **Metrics.** `WriteMetrics` gains `upsert_conflict_rows_skipped`, `upsert_retries`, `rows_returned`.

### 16.3 Tests added by Phase 2a

Phase 2a added **69 tests**, all passing in both profiles:

* `rubixdb-sql` lib, `sql/src/phase2a_tests.rs`, **60**: `returning` 11, `insert_select` 7, `on_conflict` 15, `statement_atomicity` 11, `concurrency` 6, `restart` 3, `authorization` 3, `parser` 4. They drive the real parse, bind, plan and write-executor pipeline against a real engine (no mocks); `restart` closes the engine and reopens it over the same directory (WAL replay and SSTable recovery); `concurrency` uses real threads behind a barrier. The concurrency module was run 25 times in a row: 25/25 passed.
* `rubixdb-api`, `api/tests/api_sql_phase2a.rs`, **9** (router + engine, no mocks): `RETURNING` shape and rows, UPDATE / DELETE `RETURNING`, DO NOTHING / DO UPDATE, `INSERT ... SELECT`, the HTTP error mapping (409 `CONFLICT_ERROR`, 400 `EXECUTION_ERROR`, 400 `BIND_ERROR`) with nothing written, a `reader` key refused, `RETURNING` inside a session (a failing statement leaves the session alive and the transaction unchanged), 4 concurrent HTTP tasks x 5 upserts of one key ending as one row with `n = 20` (6 repeats, all passed), and `RETURNING` data surviving an engine restart.

The mission's required list maps as: parser (`parser::*`), binder / type (`on_conflict::*_at_bind_time`, `returning::what_a_returning_clause_may_contain`, `insert_select::numeric_widening_...`), planner (`explain_shows_returning`, `insert_select::explain_shows_the_source_plan`), SQL integration (all), transaction atomicity (`statement_atomicity::*`), concurrency (`concurrency::*`, HTTP), restart (`restart::*`, HTTP), error path (`statement_atomicity::*`, HTTP errors), regression (the full suites in 16.5).

No existing test was changed except the expected value in `write_tests.rs` (the `WriteResult` literal gained `returning: None`, a field, not a behaviour); no assertion was weakened. No older test pinned a refusal of `RETURNING` / `ON CONFLICT` / `INSERT ... SELECT` (matrix: "unpinned"). One expectation of mine was corrected while writing the suite: `INSERT ... AS alias` is refused by the parser dialect (see G-4b), which the matrix did not know.

### 16.4 Open / not tested / not required after Phase 2a

* `MERGE` (G-6), data-modifying CTEs (C-14), `UPDATE ... FROM` / `DELETE ... USING` (G-7): NOT IMPLEMENTED (other phases).
* `INSERT ... AS alias` (G-4b), `ON CONSTRAINT <table>_pkey`, `ON DUPLICATE KEY`, `OUTPUT`: NOT IMPLEMENTED (refused with a typed error).
* Referencing `OLD` / `NEW` in `RETURNING` (PostgreSQL 18 allows exposing old and new values): NOT IMPLEMENTED.
* The write-set cap (10,000 operations / 16 MiB, matrix AB-7) applies to `INSERT ... SELECT` and to a large `ON CONFLICT` statement exactly as to any other write: a statement above it fails closed. Its behaviour for these statements was **NOT TESTED** in Phase 2a (no new row changes the cap).
* Throughput and latency of `RETURNING`, `INSERT ... SELECT` and upsert (including retry cost under heavy contention): **NOT TESTED**; no performance PASS. The retry worst case is analysed above, not measured.
* Fairness: under sustained contention a single upsert can lose every one of its 32 attempts and receive a 409; it is retriable and loses nothing. **NOT TESTED** beyond 4 writers on one key.
* The CLI renderer and the frontend were not changed; `RETURNING` results reach them as an ordinary `rows` result. A CLI or browser run of a `RETURNING` statement was **NOT TESTED**. The frontend IDE keyword list does not know `RETURNING`, `ON CONFLICT`, `EXCLUDED` (N-7).
* Power-loss behaviour: **NOT TESTED**. The only durability evidence is a clean engine restart (`restart::*`, `api_sql_phase2a::returned_rows_are_durable_...`), which is class A graceful-shutdown evidence, not a crash or power-loss test. A returned row is reported after the commit has returned in autocommit; inside a session it is reported when buffered, and is durable only if the session later commits (documented, tested: `returning_inside_a_transaction_...`).
* A write-write race that is neither an upsert nor in autocommit (an explicit transaction racing another) is reported to the loser at `COMMIT`, as it was before Phase 2a.

### 16.5 Regression (2026-10-15, every cargo run with `--locked` and a private target directory)

Baseline for "pre-existing" is a clean `git archive` of `dc5dc44`.

| Command | Result | Classification |
|---|---|---|
| `cargo fmt --all -- --check` | FAIL | **Pre-existing**: 28 files have rustfmt differences at clean `dc5dc44`; the same 28 now. Phase 2a left **0 newly unformatted files**: `sql/src/exec/write.rs` (clean at HEAD) and the two new files were formatted with `rustfmt` on those files only; `cargo fmt --all` was not run on the shared tree. |
| `cargo check --workspace --all-targets --all-features` | PASS | |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS (0 warnings) | |
| `cargo test --workspace --no-fail-fast` (debug) | 2,354 passed, 2 failed, 32 ignored (run before the 60th sql test was added; that test passed in its own run) | The 2 failures are `repo_hygiene::no_tracked_credentials_json` and `::no_tracked_file_contains_a_64_hex_admin_key_literal` (tracked `frontend/.e2e-crossbrowser-data/default/credentials.json`; `tests/repo_hygiene.rs` is protected). **Pre-existing**, identical to the Phase 1 run. |
| `cargo test --release --workspace --no-fail-fast` | 2,355 passed, 4 failed, 30 ignored | The same 2, plus `group_commit::m1_3_thousand_writers_throughput` (WAL gate M1.3, pre-existing at clean HEAD, see Phase 1 section 15.6), plus `observability::a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind` (see below). |

`observability::a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind` is a **pre-existing flaky** test, not a Phase 2a regression: it needs a `SELECT COUNT(*) ... LIKE` over 8,000 rows still to be `executing` when a 2 ms poll sees it, and its own message says "use a heavier statement". Run alone in release it failed 3 of 25 times on a clean `git archive` of `dc5dc44` and 1 of 8 times on this tree; the debug run passed. Its setup uses multi-row `INSERT ... VALUES` (a path Phase 2a rewrote), but the timed statement is a plain `SELECT`, which Phase 2a did not change. Not changed.

Per package: `rubixdb-sql` lib **1,119 passed (debug) / 1,120 passed (release), 0 failed, 22 ignored** (the 22 are benchmarks); `rubixdb-api` `api_sql_integration` 47, `api_sql_mission2` 13, `api_sql_phase1` 6, `api_sql_phase2a` 9 (new), `api_sql_schemas` 8: all passed in both profiles.

### 16.6 Protected paths

SHA-256 of the combined `git diff` of `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`, `Cargo.toml`, `Cargo.lock`, `tests/repo_hygiene.rs` is `3f5d912f2590c696b94f53019ea245f5b227fc8841cbab6928455e9080e2c86d` after Phase 2a, the same value recorded before and after Phase 1: **zero diff by this mission** (those paths carry the durability mission's earlier uncommitted work, unchanged). The tracked credentials file was not touched. Phase 2a changed `sql/` (`ast`, `bound`, `convert`, `bind/{dml,select}`, `plan/{mod,relstats,explain}`, `exec/{mod,write}`, `exec/write/metrics`, `lib`, `write_tests`, new `phase2a_tests`), one function in `api/src/routes/sql.rs` (`write_result_to_json`: a `RETURNING` result is sent as a `rows` body) and the new `api/tests/api_sql_phase2a.rs`. No WAL, MANIFEST, SSTable or catalog format changed, so the mission's "WAL format change: STOP" condition did not arise.


## 17. Phase 2b + 3 (ALTER TABLE and foreign keys) -- appended 2026-10-15

Phase 2b + 3 implemented the `ALTER TABLE` subset the matrix identified and foreign keys, together, because they share the catalog metadata layer and the transactional enforcement path. Design record, including the one decision the mission required (how foreign-key enforcement is made safe under snapshot isolation) and the catalog-format change: `docs/adr/ADR-DDL-FK-01.md`. **No WAL, MANIFEST or SSTable format changed; no protected path changed.** Status words below are the closed vocabulary only: PASS / FAIL / OPEN / NOT REQUIRED / NOT TESTED / NOT IMPLEMENTED. A form that cannot be done safely is "NOT IMPLEMENTED" with the reason (BLOCKED where the mission asked for that word).

**Concurrency decision for foreign-key enforcement:** commit-time validation under the existing per-table epoch locks (exclusive on tables written, shared on the other side of each key, taken in table-id order, with a catalog generation check), plus statement-time checks for immediate errors; not parent-row locking (the engine has no row locks). ADR section D5.

### 17.1 Rows whose status changed

| ID | Feature | Phase 0 status | Status now | Evidence (tests run in this phase; `rel::` = `src/relational/fk_tests.rs`, `sql::` = `sql/src/phase2b3_tests.rs`, `api::` = `api/tests/api_sql_phase2b3.rs`) |
|---|---|---|---|---|
| H-1 | `ALTER TABLE ... ADD COLUMN [DEFAULT] [NOT NULL]` | BLOCKED_BY_ARCHITECTURE | PASS | Rows decode by their own `schema_version`; a column added later reads as its `backfill_value`. `rel::versioned_decode::*` (9: old rows read the default, `SET DEFAULT` does not change old rows, no-default reads `NULL`, three generations of rows in one table, `NOT NULL` needs a default unless empty, an index built afterwards indexes the backfill, a version-1 catalog row still decodes, restart, an open transaction conflicts); `sql::alter::{add_column_with_a_default_is_applied_to_existing_rows_everywhere_they_are_read, add_column_without_a_default_reads_null_for_existing_rows, rows_written_after_the_alter_store_the_column_and_old_rows_can_be_updated, add_column_not_null_needs_a_default_unless_the_table_is_empty, add_column_if_not_exists_and_duplicates, alter_survives_a_restart_catalog_and_data, alter_during_concurrent_writes_loses_and_corrupts_nothing}`; `catalog schema::tests::column_row_version_one_decodes_and_version_two_round_trips_backfill`; `api::alter_table_over_http_reports_a_ddl_result_and_changes_the_schema` |
| H-2 | `DROP COLUMN` | BLOCKED_BY_ARCHITECTURE | NOT IMPLEMENTED (BLOCKED) | A column's identity is its ordinal; the primary key, every index, every foreign key, every stored row and the whole SQL layer use it as a position. Hiding a column would need a logical-to-physical map through all of them; the mission forbids a hiding that can mis-decode a row or mis-maintain an index. Refused at parse time with the reason: `sql::parser::the_forms_rubixdb_cannot_do_safely_are_refused_with_the_reason`, `sql::alter::drop_column_is_refused_as_blocked_and_changes_nothing`. Smallest sound design recorded in `OPEN_ITEMS.md` and ADR D3. |
| H-3 | `RENAME COLUMN` | NOT_IMPLEMENTED | PASS | `sql::alter::rename_column_is_visible_everywhere_and_old_name_stops_working` (including an index on the column and a primary-key column), `sql::check_constraints::a_column_a_check_reads_cannot_be_renamed_but_others_can`, `rel::versioned_decode::schema_changes_survive_a_restart` |
| H-3b | `RENAME TO` (table), `ALTER COLUMN ... TYPE` | NOT_IMPLEMENTED / NOT_PLANNED | NOT IMPLEMENTED | refused with a reason: `sql::parser::the_forms_rubixdb_cannot_do_safely_are_refused_with_the_reason` |
| H-4 | `ALTER COLUMN SET/DROP NOT NULL`, `SET/DROP DEFAULT` | NOT_IMPLEMENTED | PASS | `sql::alter::{set_not_null_validates_existing_rows_and_leaves_nothing_on_failure, set_default_changes_new_rows_only}`; `rel::versioned_decode::set_default_does_not_change_what_old_rows_read`; `sql::fk_definition::set_null_needs_a_nullable_column_outside_the_primary_key` (a `NOT NULL` is refused on a column a `SET NULL` key writes) |
| H-5 | `ADD/DROP CONSTRAINT`: `UNIQUE` | NOT_IMPLEMENTED | PASS | `sql::alter::add_unique_constraint_validates_existing_rows_and_enforces_afterwards`, `invalid_alters_are_rejected_without_partial_state`, `rel::versioned_decode::an_index_built_after_add_column_indexes_the_backfill_value` (unique over repeated values refused, no index row left) |
| H-5b | `ADD/DROP CONSTRAINT`: `CHECK` | NOT_IMPLEMENTED | PASS | `sql::check_constraints::*` (10), `api::check_constraints_over_http_are_enforced_with_their_own_error_code` |
| H-5c | `ADD/DROP PRIMARY KEY` | NOT_IMPLEMENTED | NOT IMPLEMENTED (by design) | a table's primary key is fixed at `CREATE TABLE` (D6); `sql::bind_and_plan::drop_constraint_finds_a_foreign_key_or_a_unique_index_by_name` pins that `<table>_pkey` cannot be dropped |
| H-6 | `ADD FOREIGN KEY` | NOT_IMPLEMENTED | PASS | `sql::fk_enforcement::a_foreign_key_can_be_added_to_a_table_with_valid_rows_and_not_to_one_with_orphans`, `sql::alter::invalid_alters_are_rejected_without_partial_state`, `rel::commit_validation::adding_a_foreign_key_while_children_are_inserted_leaves_no_orphan` (15 rounds), `sql::fk_concurrency::adding_a_key_while_children_are_inserted_leaves_no_orphan` (6 rounds) |
| I-1 | Inline `col ... REFERENCES t(c)` | NOT_IMPLEMENTED | PASS | `sql::parser::create_table_collects_inline_and_table_level_foreign_keys_in_order`, `sql::fk_definition::inline_and_table_level_declarations_both_record_a_key` |
| I-2 | Table-level `FOREIGN KEY (a) REFERENCES t(b)` | NOT_IMPLEMENTED | PASS | same; `sql::fk_enforcement::*` |
| I-3 | Composite FK (`MATCH SIMPLE`) | NOT_IMPLEMENTED | PASS | `sql::fk_enforcement::{composite_keys_are_enforced_and_a_partial_null_is_exempt, a_composite_key_over_a_unique_index_matches_columns_in_any_order, a_child_keyed_by_the_parent_reference_uses_the_key_prefix}`, `sql::fk_definition::composite_keys_need_matching_arity_and_types`. `MATCH FULL|PARTIAL` and `DEFERRABLE`: NOT IMPLEMENTED (refused, `sql::parser`) |
| I-4 | Referenced PK / UNIQUE resolution | NOT_IMPLEMENTED | PASS | `sql::fk_definition::the_referenced_columns_must_be_the_primary_key_or_a_ready_unique_index`, `sql::fk_enforcement::a_key_over_a_unique_non_pk_column_is_enforced_and_cascades`, `sql::fk_paths::text_and_decimal_keys_are_matched_exactly`, `rel::lookup_paths::every_access_path_finds_the_same_rows`. Documented requirement: the referenced column set is exactly the primary key or exactly a `Ready` unique index; identical types |
| I-5 | `ON DELETE` / `ON UPDATE`: `NO ACTION`, `RESTRICT`, `CASCADE`, `SET NULL`, `SET DEFAULT` | NOT_IMPLEMENTED | PASS | `sql::fk_enforcement::{delete_of_a_referenced_parent_is_refused_under_no_action_and_restrict, delete_cascades_through_several_levels, delete_sets_null_in_the_children, delete_sets_the_default_in_the_children_and_the_default_must_itself_be_valid, update_of_a_parent_key_cascades_to_the_children, update_of_a_parent_key_sets_null_or_is_refused, a_self_referencing_table_inserts_a_tree_in_one_statement_and_cascades_down}`, `sql::fk_paths::*` (8), `rel::enforcement::*` (3: a cascade past the write-set cap and a chain deeper than 32 fail closed) |
| I-6 | Immediate enforcement and concurrency | BLOCKED_BY_ARCHITECTURE | PASS (decision: commit-time validation under the epoch locks; ADR D5) | both orderings at three layers: `rel::commit_validation::{child_insert_loses_to_a_parent_delete_that_committed_first, parent_delete_loses_to_a_child_insert_that_committed_first, racing_child_insert_and_parent_delete_never_leave_an_orphan (60 barrier rounds), many_concurrent_child_inserts_on_one_parent_all_commit, foreign_keys_in_opposite_directions_do_not_deadlock}`; `sql::fk_concurrency::*` (6 incl. a 3-writer stress); `sql::fk_snapshot_views::*` (3); `api::{concurrent_http_children_and_parent_deletes_leave_no_orphan, foreign_keys_inside_a_session_transaction_are_checked_at_the_statement_and_at_commit}` |
| I-7 | Validation of existing rows when a FK is added | NOT_IMPLEMENTED | PASS | see H-6 |
| I-8 | Metadata persistence of FKs | NOT_IMPLEMENTED | PASS | new system table 8; `catalog schema::tests::foreign_key_row_round_trips_with_and_without_a_parent_index`, `sql::fk_restart::definitions_and_enforcement_survive_a_restart`, `api::alter_table_and_foreign_keys_survive_a_restart` |
| I-9 | FK recovery | NOT_IMPLEMENTED | PASS for a clean engine restart after commit and after an abandoned transaction; **NOT TESTED** for process kill / power loss in the middle of a cascade commit | `sql::fk_restart::{a_committed_cascade_is_whole_after_a_restart, an_uncommitted_cascade_is_nowhere_after_a_restart, a_dropped_key_stays_dropped_and_a_dropped_table_takes_its_keys}`; whole-or-absent rests on the certified single `write_batch` |
| I-11 | Inline / table-level `UNIQUE` in `CREATE TABLE` | PARTIALLY | NOT IMPLEMENTED (unchanged) | `ALTER TABLE ... ADD CONSTRAINT UNIQUE` (H-5) is the supported spelling |
| I-12 | `CHECK` constraints | NOT_IMPLEMENTED | PASS | see H-5b; column-level and table-level in `CREATE TABLE`, `ALTER ... ADD CONSTRAINT`, enforced on `INSERT`, `INSERT ... SELECT`, `ON CONFLICT`, `UPDATE` and on rows a foreign-key action rewrites |
| G-8 | `UPDATE` of a primary-key column | NOT_IMPLEMENTED | PASS | `sql::primary_key_update::*` (7: move, shift among a statement's own rows, collision fails and changes nothing, `NULL` refused, `ON CONFLICT DO UPDATE` still refuses, in a transaction, two transactions racing for one key); the two Phase 0 tests that pinned the refusal are replaced (`bind_tests::update_of_primary_key_column_now_binds`, `write_tests::update_primary_key_column_moves_the_row_end_to_end`) |
| AB-1 | Positional row decode | BLOCKED | resolved | H-1 |
| AB-2 | No read-set under snapshot isolation | BLOCKED | resolved for foreign keys | I-6 |
| AB-3 | Constraint record cannot express a foreign key | BLOCKED | resolved | I-8 |

New rows found while implementing (not in the Phase 0 matrix):

| ID | Feature | Status | Evidence / reason |
|---|---|---|---|
| X-1 | `CREATE UNIQUE INDEX` over rows that already hold a value twice | defect, FIXED | it used to succeed and mark the index `Ready`; a `UNIQUE` index is now verified (bounded memory, one pass in key order) before it becomes `Ready`. `rel::versioned_decode::an_index_built_after_add_column_indexes_the_backfill_value`, `sql::alter::add_unique_constraint_validates_existing_rows_and_enforces_afterwards` |
| X-2 | Statement atomicity inside an explicit transaction | PASS | every DML statement runs in a savepoint; a failing statement undoes only itself: `rel::savepoints::*` (4), `sql::fk_atomicity::{a_failing_statement_inside_a_transaction_undoes_only_itself, a_cascade_that_exceeds_the_write_set_cap_fails_closed_and_leaves_the_transaction_untouched, an_autocommit_cascade_past_the_cap_changes_nothing_at_all}`. Also closes the Phase 2a caveat that a failure while buffering could leave earlier rows of the statement in the transaction |
| X-3 | Transaction open across an `ALTER` or a key/constraint change | PASS (defined behaviour) | the commit fails with a `Conflict`: `rel::versioned_decode::a_transaction_that_wrote_before_an_alter_conflicts_at_commit`, `sql::alter::a_transaction_open_across_an_alter_conflicts_rather_than_writing_an_old_shape`, `sql::check_constraints::a_transaction_open_across_add_check_conflicts_and_a_racing_insert_never_slips_past` |
| X-4 | Drop a table another table references; drop a unique index a key resolves to | PASS (refused) | `sql::fk_definition::{dropping_a_referenced_table_is_refused_and_dropping_the_child_removes_its_keys, the_referenced_columns_must_be_the_primary_key_or_a_ready_unique_index}`, `api::{dropping_a_referenced_table_is_a_client_error_over_sql, rest_drops_of_a_referenced_table_and_a_referenced_unique_index_are_client_errors}` |

### 17.2 Semantics as implemented (each is pinned by a test named in 17.1)

* **ADD COLUMN** appends a column; existing rows are not rewritten and read the new column as its `DEFAULT` (or `NULL`). `SET DEFAULT` / `DROP DEFAULT` affect only rows written afterwards. `NOT NULL` without a `DEFAULT` is accepted only on an empty table. Every `ALTER` and every foreign-key change bumps the table's persisted `schema_version` in the same atomic batch.
* **Atomicity / recovery of every `ALTER`**: epoch write-lock on each table involved, validation (definition, and every existing row for `SET NOT NULL`, `ADD FOREIGN KEY`, `ADD UNIQUE`), then ONE catalog `write_batch`. A refused or failed `ALTER` changes nothing (tested by comparing the whole catalog before and after); a restart sees either the old or the new schema, never a mix. A transaction that wrote the table before an `ALTER` and commits after it gets a `Conflict`.
* **One operation per `ALTER` statement.**
* **Foreign keys**: inline (`col T REFERENCES p (c)`) and table-level, single column and composite (up to 16), referencing a primary key or a `Ready` `UNIQUE` index (any column order), identical types required. `MATCH SIMPLE`: a row with any `NULL` referencing column is never checked. `NO ACTION` and `RESTRICT` are the same (enforcement is always immediate, at the end of the statement). `ON DELETE` and `ON UPDATE`: `CASCADE`, `SET NULL`, `SET DEFAULT`. Enforced on `INSERT`, `INSERT ... SELECT`, `UPDATE`, `DELETE`, `ON CONFLICT DO NOTHING | DO UPDATE`.
* **Composite-key requirement**: every referenced column must exist, have the same type as its referencing column, and the referenced column *set* must be exactly the parent's primary key or exactly the columns of a `Ready` `UNIQUE` index (a prefix of a key, or a non-unique index, is refused at definition).
* **Violation errors** (`SqlError::ForeignKeyViolation`, HTTP 409 `FOREIGN_KEY_VIOLATION`) name the constraint, the tables and the columns, never a value.
* **Statement atomicity**: every DML statement runs inside a savepoint; a statement that fails part-way (violation, cascade past a limit) leaves the transaction exactly as before it.
* **Limits**: cascade depth 32; the transaction write-set cap (10,000 operations / 16 MiB) bounds a cascade; the children read for one statement are bounded by the same cap.
* **`UPDATE` of a primary key** is now supported (G-8): delete of the old key and insert of the new, old keys removed first so keys can shift; a collision fails the statement.
* **`CREATE UNIQUE INDEX` over existing duplicates** is now refused (defect found: it used to succeed).

### 17.3 Tests added by Phase 2b + 3

139 new tests, all in the repository's normal test run, plus two `#[ignore]`d micro-benchmarks:

| File | Tests | Covers |
|---|---|---|
| `src/relational/fk_tests.rs` | 29 | savepoints (4); versioned row decode / `ADD COLUMN` / restart / conflict with an open transaction (9); commit-time foreign-key validation and both race orderings (12, incl. a 60-round barrier stress, 8 concurrent writers on one parent, keys in opposite directions, `ADD FOREIGN KEY` racing inserts); statement-time enforcement and cascades (3, incl. write-set cap and depth limit failing closed); row-lookup access paths (1) |
| `sql/src/phase2b3_tests.rs` | 98 | parser (4) / binder + planner (7) / `ALTER TABLE` (14) / `UPDATE` of a primary key (7) / foreign-key definition (6) / enforcement (20) / more paths (8) / statement-time views (3) / atomicity (5) / concurrency (6) / restart (4) / authorization (2) / catalog level (2) / `CHECK` (10) |
| `api/tests/api_sql_phase2b3.rs` | 10 | `ALTER` over HTTP; 409 `FOREIGN_KEY_VIOLATION` / `CHECK_VIOLATION` with no row value; cascades with `RETURNING`; a session transaction; `DROP` of a referenced table; concurrent HTTP writers; restart |
| `src/catalog/schema.rs` (unit) | 2 | foreign-key row round trip; version-1 / version-2 `system.columns` rows |
| `src/relational/commit_bench_tests.rs`, `fk_bench_tests.rs` | 2 (`#[ignore]`) | commit-path micro-benchmarks, section 17.6 |

Two older tests were replaced because the behaviour they pinned is gone by design (G-8): `bind_tests::update_of_primary_key_column_now_binds`, `write_tests::update_primary_key_column_moves_the_row_end_to_end`.

Test categories required by the mission, and where they are:

| Required | Where |
|---|---|
| ALTER: add column with rows, default applied | `sql::alter::add_column_with_a_default_*`, `rel::versioned_decode::old_rows_read_the_backfill_*` |
| drop column | refused (BLOCKED): `sql::alter::drop_column_is_refused_as_blocked_and_changes_nothing` |
| rename column; set / drop NOT NULL; add / drop constraint | `sql::alter::{rename_column_*, set_not_null_*, add_unique_constraint_*}`, `sql::check_constraints::add_check_*`, `sql::fk_definition::drop_constraint_removes_the_key_*` |
| interrupted ALTER leaves no partial state | `sql::alter::invalid_alters_are_rejected_without_partial_state` (the whole catalog compared before and after each refused ALTER), `add_check_validates_every_existing_row_and_records_nothing_on_failure`; each ALTER is one `write_batch` (ADR D4). A process kill inside `write_batch` is the certified WAL's territory and was not injected: NOT TESTED |
| restart after ALTER, catalog + data | `sql::alter::alter_survives_a_restart_catalog_and_data`, `rel::versioned_decode::schema_changes_survive_a_restart`, `api::alter_table_and_foreign_keys_survive_a_restart` |
| ALTER during concurrent writes | `sql::alter::alter_during_concurrent_writes_loses_and_corrupts_nothing`, `sql::check_constraints::a_transaction_open_across_add_check_*`, `rel::versioned_decode::a_transaction_that_wrote_before_an_alter_conflicts_at_commit` |
| invalid ALTER rejected without partial state | `sql::alter::invalid_alters_are_rejected_without_partial_state`, `sql::bind_and_plan::invalid_statements_fail_to_bind` |
| FK: valid parent / missing parent / RESTRICT / CASCADE / SET NULL / cascade on PK update / composite / NULL not checked / add FK valid & invalid / UPDATE / UPSERT | `sql::fk_enforcement::*` (names in 17.1) |
| two concurrent transactions racing on a FK | `rel::commit_validation::*`, `sql::fk_concurrency::*` |
| crash during cascade → whole or none | commit is one `write_batch`: `sql::fk_restart::{a_committed_cascade_is_whole_after_a_restart, an_uncommitted_cascade_is_nowhere_after_a_restart}`; a failing cascade leaves nothing: `sql::fk_atomicity::*`, `rel::enforcement::*`. Process kill / power loss mid-commit: NOT TESTED |
| restart after cascade — FK state verified | `sql::fk_restart::*` |

### 17.4 Open / not tested / not required / not implemented after Phase 2b + 3

**NOT IMPLEMENTED**
* `DROP COLUMN` (BLOCKED -- a column's identity is its ordinal; ADR D3), `ALTER COLUMN ... TYPE`, `RENAME TO`, `ADD/DROP PRIMARY KEY`, `NOT VALID` / `VALIDATE CONSTRAINT`, `DEFERRABLE` keys, `MATCH FULL|PARTIAL`, `CHECK ... NOT ENFORCED`, several operations in one `ALTER`, `ADD COLUMN ... REFERENCES|CHECK`, inline / table-level `UNIQUE` inside `CREATE TABLE`.
* `MERGE` (G-6), data-modifying CTEs (C-14), `UPDATE ... FROM` / `DELETE ... USING` (G-7), `INSERT ... AS alias` (G-4b): unchanged from section 16.

**NOT TESTED**
* A process kill or power loss in the middle of committing a cascade. A cascade reaches the engine as the single `write_batch` of the commit, so it is whole or absent by the certified batch atomicity; only a clean engine restart after commit, and after an abandoned transaction, were exercised here.
* Foreign-key checks, cascades, `ADD FOREIGN KEY` and `ADD CHECK` validation over **large** tables (a child table with no usable access path is scanned once per statement/commit with the tables locked; no timing was taken); more than 16 concurrent writers; contention on a single hot parent beyond the micro-benchmarks in 17.6.
* The engine's WAL gates (M1.2 / M1.3) re-run against the new commit path. M1.3 already fails at clean HEAD; the micro-benchmark below is fsync-bound and cannot see CPU cost.
* CLI and browser rendering of `ALTER TABLE` results and of the `FOREIGN_KEY_VIOLATION` / `CHECK_VIOLATION` errors; the frontend keyword / completion lists and `SchemaDesign.tsx` ("rubiXDB has no foreign keys") were not touched.
* Downgrade: an older binary opening a catalog that has foreign keys or an altered table (not a supported path; ADR D1).

**Behaviours to know about (all pinned by tests)**
* A transaction that wrote a table and commits after an `ALTER` / foreign-key / `CHECK` change on it fails with a `Conflict`.
* A cascading delete that races a newly committed child is refused with a foreign-key violation (retrying cascades the new child); autocommit does not retry it.
* Inside an explicit transaction a parent committed after `BEGIN` can be referenced (the statement-time check reads the latest committed parents too; commit decides).
* `NO ACTION` = `RESTRICT`; a statement that deletes parents and their children together succeeds.
* `DROP CONSTRAINT <name>` looks for a foreign key, then a `CHECK`, then a unique index of that name.
* `RENAME COLUMN` of a column a `CHECK` reads is refused.
* `TableStore::put_row` / `put_rows` (non-transactional test and maintenance entry points) are not constraint aware; `Transaction` and `autocommit_put_row` are for foreign keys, and the SQL executor is for `CHECK`.
* A process must hold exactly one `CatalogService` per engine (the foreign-key and `CHECK` caches live in the instance); the server does.

**NOT REQUIRED**: parent-row locking (the engine has no row locks; ADR D5); a read-set for general serializability (write skew on other constraints remains the documented D10 property).

**Foreign-key enforcement is NOT declared production ready**: transaction races, cascading operations, failure during a cascade and clean-restart recovery are tested, but process kill / power loss during a cascade and large-table behaviour are NOT TESTED, and the repository's mandatory matrix still has FAIL entries that predate this phase (17.5).

### 17.5 Regression (2026-10-15, every cargo run with a private target directory)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | **FAIL, pre-existing**: the same 28 files that fail at `dc5dc44` (a clean `git archive` of it was checked the same way); this phase added none. Every file this phase created, and the files whose formatting was clean at HEAD, are rustfmt-clean; the unformatted hunks in the 28 files belong to earlier phases' code and other sessions' work and were left alone (the tree is shared; `cargo fmt --all` was not run on it) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS, 0 warnings (two lints in this phase's code were fixed, not allowed) |
| `cargo check --workspace --all-targets --all-features` | PASS |
| `cargo test --workspace --no-fail-fast` (debug) | 60 test binaries: **2,494 passed, 2 failed, 34 ignored**. The two failures are `repo_hygiene::no_tracked_credentials_json` and `no_tracked_file_contains_a_64_hex_admin_key_literal` (the tracked `frontend/.e2e-crossbrowser-data/default/credentials.json`; protected path, not touched), failing at clean HEAD too |
| `cargo test --release --workspace --no-fail-fast` | 60 test binaries: **2,495 passed, 3 failed, 32 ignored**. The same two `repo_hygiene` failures, and `group_commit::m1_3_thousand_writers_throughput` (20,949 ops/s against a target of 80,000: an engine-level WAL gate that drives `LsmEngine` directly, which this phase does not touch; it fails at clean HEAD too) |

By package (identical in both profiles): `rubixdb` lib 722 passed (6 ignored, 2 of them this phase's benchmarks), `rubixdb-sql` lib 1,218 passed (22 ignored), `api_sql_integration` 47, `api_sql_phase2a` 9, `api_sql_phase2b3` 10, `api_sql_mission2` 13, `api_sql_phase1` 6, `api_sql_schemas` 8.

Two failures seen in the **first** full debug run of this phase, both resolved and neither a product defect: (1) `persistence_abstraction_complete::no_production_code_outside_persist_touches_the_filesystem_directly` flagged `std::fs` in the two new benchmark files (the scan exempts files named `*tests.rs`); the files were renamed `commit_bench_tests.rs` / `fk_bench_tests.rs`, and the scan passes. (2) `observability::two_concurrent_instances_share_nothing_and_a_restart_in_one_process_starts_empty` failed once under the full run (an auth-failure counter read 0, a sampler-timing assertion in the same family as the known flaky `a_disconnect_mid_statement_...`); it passed 6/6 alone, 5/5 as a whole binary, and in the final full runs. No code in this phase touches authentication or the sampler.

### 17.6 Measured

Machine: the maintainer's Windows 10 workstation (shared with other sessions), SATA disk, engine `GroupCommit` with a 1 ms wait. Both benchmarks are `#[ignore]`d tests (`src/relational/commit_bench_tests.rs`, `fk_bench_tests.rs`), release build, 16 threads x 400 single-row transactions (`begin`, `put_row`, `commit`), a fresh database per run.

| Benchmark | Runs | commits/s min / median / max | latency (per commit) |
|---|---|---|---|
| Tables **without** a foreign key, unmodified checkout of `dc5dc44` | 7 | 253 / 272 / 290 | p50 3,173 us, p99 909,673 us |
| The same, this tree | 7 | 275 / 294 / 306 | p50 3,178 us, p99 644,033 us |
| Child inserts, no foreign key (this tree, same process) | 5 | 275 / 287 / 305 | not recorded |
| Child inserts, foreign key to a 16-row parent (this tree, same process) | 5 | 288 / 299 / 305 | not recorded |

Reading: the workload is bound by the disk's fsync (p50 about 3.2 ms, p99 close to a second), so every difference above is inside run-to-run noise, **in both directions**. It shows that the new commit path (foreign-key set read, lock plan, generation check; plus the parent lookup when a key exists) did not make a group-committed write measurably slower on this machine. It does **not** isolate the CPU cost of those additions, and it says nothing about large tables or contended hot parents. No performance PASS is claimed (no workload sweep, no resource measurements, no latency distribution for the foreign-key case).

Correctness stress that ran as tests: 60 barrier-started rounds of a child insert against the delete of its parent (relational), a 3-writer + 1-deleter SQL stress and a 3-task HTTP stress, 15 + 6 rounds of `ADD FOREIGN KEY` racing inserts, 8 threads x 25 commits on one parent, opposite-direction keys from 4 threads, `ALTER TABLE ADD COLUMN` + `SET DEFAULT` racing 3 writers, `ADD CHECK` racing a writer -- no orphan, no lost row, no deadlock in any run.

### 17.7 Protected paths

Zero diff attributable to this phase: the combined diff hash of `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`, `Cargo.toml`, `Cargo.lock`, `tests/repo_hygiene.rs` and the tracked credentials file is `3f5d912f2590c696b94f53019ea245f5b227fc8841cbab6928455e9080e2c86d`, the same value as before Phase 1 (other sessions' uncommitted work in `src/wal/` and `src/manifest/` is part of that value and is unchanged by this phase). `src/relational/` and `src/catalog/` (modifiable per the mission) changed.

### 17.8 Git state

Phase 2b + 3 was started at `dc5dc44`. While it ran, the maintainer committed the working tree twice (`ce9cb59` 08:11 -- Phases 1 and 2a and other sessions' work; `bdb94db` 09:10 -- an intermediate snapshot of this phase). This phase made no commit and no push. At the time of writing `HEAD` is `bdb94db`; this phase's later edits (rustfmt of its own files, the CHECK-error and benchmark-file renames, the REST-drop test, the lint fixes) and this documentation are uncommitted. The protected-path hash in 17.7 is computed against `dc5dc44` (`git diff dc5dc44 -- <protected paths>`), which stays valid after those commits because they contain other sessions' work in `src/wal/` and `src/manifest/` byte-for-byte as it was when this phase began.
