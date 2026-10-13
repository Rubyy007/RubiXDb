# RubiXDB — SQL Grammar, AST, and Binder Reference (Increment 6)

**Date:** 2026-09-23. The `rubixdb-sql` crate — parser integration,
internal AST, binder, authorization resolution. **No execution.** This
document is the structural/reference map; `PHASE_RELATIONAL_SQL_
INCREMENT6_RESULTS.md` has the certification matrix and measurements.

> **PARTLY SUPERSEDED (2026-10-12):** subqueries, derived tables, `LATERAL`,
> `WITH` / `WITH RECURSIVE` and `UNION` / `INTERSECT` / `EXCEPT` are supported
> since SQL Mission 1. The original text below is unchanged; section 12 at
> the end lists exactly what changed.

---

## 1. Parser dependency

`sqlparser` (crate `sqlparser`, Apache DataFusion project), pinned
exact: `sqlparser = "=0.63.0"`, matching this project's established
pinning discipline (`crc32c`/`xxhash-rust`/`proptest` in the core
crate). Verified before adoption:

| Property | Finding |
|---|---|
| License | Apache-2.0 (permissive, compatible) |
| Maintenance | Apache DataFusion project; 78M+ downloads; updated within the week of this increment |
| `unsafe` | None introduced by this crate's own use of it (the dependency itself is a separate compilation unit, not audited byte-for-byte here, but no `unsafe` appears in this crate's own code) |
| Recursion protection | `recursive-protection` is a **default** feature (`recursive`/`stacker` crates) — a real, built-in stack-depth guard for the parser's own recursive-descent calls, plus `Parser::with_recursion_limit(n)` (backed by `ParserError::RecursionLimitExceeded`), used here with `SqlLimits::max_expression_depth` |
| Transitive deps pulled in | `recursive`, `recursive-proc-macro-impl`, `stacker`, `psm`, `object`, `ar_archive_writer` (the latter two are `stacker`'s own platform-stack-probing dependencies, not SQL-specific) |
| Build scripts | `stacker`/`psm` have build scripts (platform stack-pointer detection) — inspected, not adversarial, standard for this class of crate |

Lives in its own workspace crate (`rubixdb-sql`, directory `sql/`),
**never** a dependency of the core `rubixdb` engine crate (D14) — `sql/
Cargo.toml` depends on `rubixdb`, never the reverse, preserving the core
crate's own certified "zero new dependency" property exactly as `api/`
already does for its own dependencies.

**Dialect**: `GenericDialect`, not `PostgreSqlDialect` — D14 approves
either; `GenericDialect` is the more conservative choice for this crate's
deliberately narrow grammar subset, since it does not additionally admit
Postgres-specific extensions this crate would then have to reject one by
one. The double-quoted-identifier/single-quoted-string split the
Architecture doc's §0 asks for is ANSI-standard, not Postgres-only, so
`GenericDialect` already provides it.

## 2. Supported grammar subset

**Statements**: `SELECT` (incl. `INNER`/`LEFT JOIN`, `WHERE`, `ORDER BY`,
`LIMIT`/`OFFSET`, `DISTINCT`, `*`/`table.*` wildcards), `INSERT` (`VALUES`
only, single or multi-row, explicit or implicit column list), `UPDATE`
(`SET`, `WHERE`; never a primary-key column), `DELETE` (`WHERE`),
`CREATE DATABASE`/`CREATE SCHEMA`/`CREATE TABLE`/`DROP TABLE`/`CREATE
[UNIQUE] INDEX`/`DROP INDEX ... ON ...`, `EXPLAIN <stmt>`, `BEGIN`/
`COMMIT`/`ROLLBACK` (trivial markers only).

**Explicitly not supported** (rejected as `SqlError::Unsupported`, never
silently mis-bound): `GROUP BY`/`HAVING`/aggregate functions (D19 is
execution-layer scope, deferred), subqueries (`IN (SELECT ...)`,
`EXISTS`, scalar subqueries), `UNION`/`EXCEPT`/`INTERSECT`, `WITH`
(CTEs), `RIGHT`/`FULL`/`CROSS`/`NATURAL JOIN` and `JOIN ... USING`
(D18: only `INNER`/`LEFT` with an explicit `ON`), `INSERT ... SELECT`,
window functions, `CAST`, table-level `UNIQUE`/`FOREIGN KEY`/`CHECK`
constraints (create the table, then `CREATE UNIQUE INDEX`), and every
dialect-specific extension `GenericDialect` itself declines to parse.

**Literals**: `NULL`, `TRUE`/`FALSE`, integer/decimal-looking numbers
(kept as raw text through parsing — see §4), single/double-quoted
strings, `X'...'` hex `BLOB`, `DATE '...'`/`TIME '...'`/`TIMESTAMP '...'`
typed string literals (ANSI syntax; parsed by this crate's own
dependency-free `temporal` module, not a library).

**Identifiers**: unquoted identifiers are case-folded to lowercase;
quoted identifiers (`"MixedCase"`) are kept byte-for-byte, matching
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §1's already-committed rule
— the "Identifier Rules" section that document promised but the ADR
never wrote is filled in here as this increment's own amendment (§9).
Object names support 1–3 parts (`table`, `schema.table`, `db.schema.
table`); column references support 1–2 parts (`column`, `table.column`)
— a 3-part column reference is a stated v1 scope cut (`InvalidIdentifier`),
not silently mis-parsed.

**Parameters**: `$1`, `$2`, ... only (1-based); `?` and any other
placeholder form is rejected (item 7).

**Functions**: an explicit, closed registry (`crate::functions`) —
`length(TEXT)->INTEGER`, `upper(TEXT)->TEXT`, `lower(TEXT)->TEXT`,
`abs(numeric)->same type`. An unknown name fails at bind time (item 23).

## 3. Internal AST (`crate::ast`)

Never exposes `sqlparser::ast` beyond `crate::convert`'s own conversion
boundary (item 4) — `Statement`/`Select`/`Insert`/`Update`/`Delete`/
`CreateTable`/`CreateIndex`/.../`Explain`/`Begin`/`Commit`/`Rollback`,
`Expr` (column/literal/parameter/unary/binary/`IS NULL`/`BETWEEN`/
`IN`/`LIKE`/`CASE`/function call), `Literal` (kept as raw, lossless text
for numbers — see §4), `SqlDataType` (D4's type set, named at the SQL
surface). Every `Statement` variant's doc comment states its own
execution-status boundary explicitly: **PARSED, BOUND, NOT EXECUTABLE
YET** (item 5) — no variant is ever a claim that the feature works end
to end.

## 4. Literal-to-`RelationalValue` mapping (item 6/21)

A numeric literal's text (`Literal::Number { text, is_integer }`) is
kept **raw** through parsing — never guessed into a concrete type until
`crate::bind::expr` has a target type to bind against. With an expected
type (a column's declared type, a comparison partner's resolved type):
the literal is parsed directly into that type (`i32`/`i64`/`f32`/`f64`/
scaled `i128` for `DECIMAL`, precision-checked via `rubixdb::relational::
value::validate_decimal`). With **no** expected type (item 21's "no
context" case): an integer-looking literal prefers `INTEGER`, falling
back to `BIGINT` only if it doesn't fit; a decimal-looking literal
(`.`/exponent present) defaults to `DOUBLE` — stated explicitly here,
not silently invented per call site, per item 21's own instruction.
`DATE`/`TIME`/`TIMESTAMP` values only ever arrive via a typed string
literal (`DATE '...'` etc.) — there is no bare-number-to-temporal
coercion.

## 5. Type resolution (D21) — no implicit coercion

**Decision**: exact type match only; `NULL`/untyped is always
assignable to any expected type. Two expressions that must "agree" (a
binary comparison's two sides, `BETWEEN`'s three operands, an `IN`
list, `CASE`'s branch results) are unified via `ExprBinder::bind_shared`:
a **rigid** type (from a column, parameter-with-context, or function
result) always wins over a **flexible** one (a bare literal's own
context-free default guess), regardless of which side of the expression
it appears on — `5 = orders.id` and `orders.id = 5` bind identically.
When every operand is a bare literal, the first one's own default wins.
This is the "safest consistent behavior" item 21 asks for when full
coercion rules aren't independently specified, documented here rather
than invented silently.

## 6. Binder architecture (`crate::bind`)

```
bind_statement(catalog, ctx: BindContext, auth: AuthContext, metrics, limits, &ast::Statement)
  -> Result<bound::BoundStatement>
```

- `scope.rs` — table/column resolution **and** D25 authorization in the
  same pass (item 15): `resolve_table` never returns a table without
  having already checked the caller's privilege on it; "does not exist"
  and "exists but forbidden" are the *same* `SqlError::UnknownObject`
  outcome (item 32), never distinguishable.
- `expr.rs` — expression binding, type resolution (§5), the `$n`
  parameter-index bound.
- `select.rs` — `FROM`/`JOIN` scope construction (ambiguity across
  joined tables detected and rejected, item 28), wildcard expansion
  using real catalog column order (item 18 — no second per-column grant
  lookup: D25 is table-level granularity only, so every column of an
  already-authorized table is authorized by construction), `ORDER BY`
  `NULLS FIRST`/`LAST` defaulting (D5).
- `dml.rs` — `INSERT`/`UPDATE`/`DELETE`, shaped to carry exactly what a
  future executor needs (one bound value per catalog column, in ordinal
  order) without reparsing (item 26). `UPDATE` of a primary-key column is
  rejected at bind time (D6: unsupported in v1, execute as delete+insert
  instead — never silently mis-bound as an in-place update).
- `ddl.rs` — `CREATE`/`DROP TABLE`/`INDEX`/`SCHEMA`/`DATABASE`, shaped to
  be the literal argument set `CatalogService`/`IndexBuilder` already
  expect (item 25) — never bypasses them, never duplicates catalog logic.

**"Current database/schema"** (item 17): `BindContext { database_id,
default_schema_id }`, supplied by the caller — this crate invents no
`search_path` subsystem beyond this single default, the stated minimum
bar.

## 7. Authorization (`crate::auth`, D25)

`AuthContext { principal, default_access: None | Reader | Admin }` —
the v1 default-privilege mapping stated explicitly: `Admin` bypasses
every check; `Reader` covers `SELECT` on every object; `None` relies
purely on explicit `system.grants` rows. This crate does **not** depend
on `rubixdb-api`'s own `Role` type (the reverse dependency direction
would be architecturally backwards) — whatever eventually wires a real
session to this binder maps its own role concept onto one of these
three (explicitly out of this increment's scope, item 48).

Grants are checked against the full ancestor chain (table → schema →
database) resolved from the real catalog — a grant at *any* level
suffices (D25). `CREATE DATABASE` has no ancestor object to check against
(D25's grant model has no level above `Database`), so it is gated purely
on `default_access == Admin`.

## 8. Resource limits (`crate::limits::SqlLimits`)

| Limit | Default | Enforced |
|---|---|---|
| Max SQL statement bytes | 1 MiB | before tokenizing (D27) |
| Max expression nesting depth | 128 | `sqlparser`'s own `with_recursion_limit` + this crate's own `DepthGuard` during conversion |
| Max identifier length | 255 | during conversion |
| Max `$n` parameter index | 10,000 | at bind time (`ExprBinder::bind_parameter`) |
| Max statements per script | 1,000 | after parsing, before per-statement conversion |
| Max columns (projection/`INSERT` list/`CREATE TABLE`) | 1,600 (matches D27) | during conversion/binding |
| Max `IN (...)`/`CASE` branch elements | 10,000 | during conversion |
| Max `VALUES` rows | 10,000 | during conversion |

**A found, fixed hazard** (§10 has the full account): a long **flat
chain** of binary/unary operators bypasses `sqlparser`'s own recursion
guard (a Pratt parser loops through an infix chain rather than
recursing) and reproducibly crashed the process with a stack overflow —
not a returned error — via the resulting deep `Box<Expr>` tree's
ordinary recursive `Drop`. Closed with a pre-parse operator-density
check (`parse::reject_pathological_operator_chains`), the only available
mitigation that does not require modifying `sqlparser` itself (item 9's
explicit instruction).

## 9. Identifier Rules (ADR amendment — filling a genuine gap)

`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §1 promises a dedicated
"Identifier Rules" section in `PHASE_RELATIONAL_DATABASE_ADR.md`; that
section was never actually written (verified: no such heading exists in
the ADR). This increment supplies it, using exactly the behavior the
Architecture doc's own prose already committed to (not inventing new
semantics): unquoted identifiers are case-folded to lowercase; quoted
identifiers (`"..."`) are case-sensitive and kept byte-for-byte;
duplicate names within one namespace level are a catalog-level concern
(D1, unchanged). Implemented in `crate::convert::convert_ident`, tested
directly (`parse_tests::quoted_identifiers_preserve_case_unquoted_fold_
to_lowercase`) and exercised by every authorization-bypass test (item 42
— case-folding cannot be used to dodge a grant check, since resolution
always happens against the catalog's own stored, already-folded names).

## 10. Security posture summary

See `PHASE_RELATIONAL_SQL_INCREMENT6_RESULTS.md` §5–§7 for the full
test account; the load-bearing points:

- **Structural injection prevention** (D26): the binder never accepts a
  raw client string as grammar; parameter/literal *values* are typed
  data with no path back into SQL text; identifiers resolve only through
  `CatalogService`, never string-built into a physical key.
- **Information disclosure** (item 15/32): verified directly — a
  nonexistent table and a forbidden-but-existing table produce the
  *same* error variant and shape.
- **Resource exhaustion / the stack-overflow finding** (§8): a real
  crash was found and closed, not merely tested against a hypothesis.

## 11. Known limitations

No SQL execution (planner/optimizer/executor do not exist — item 52's
stop condition). No transactions (`BEGIN`/`COMMIT`/`ROLLBACK` are inert
markers). No `GROUP BY`/aggregates/subqueries/`UNION` binding (execution-
layer or later-increment scope, explicitly rejected rather than silently
mis-bound). 3-part column references and table-level `CHECK`/`FOREIGN
KEY` constraints are stated v1 scope cuts. Binder latency scales
linearly with catalog table count (§ Performance in the results doc) —
a pre-existing, already-documented `CatalogService::list_tables` full-
scan characteristic (`RELATIONAL ADR AMENDMENT 002` CA.2), newly
exercised by the binder, not introduced by it; no cache was added
(item 36's explicit "do not introduce caching automatically").


---

## 12. Addendum -- SQL Mission 1 (2026-10-11 / 2026-10-12): what is now supported

**This section supersedes the "Explicitly not supported" list in section 2 and the
"Known limitations" in section 11 for the items below. The original text above is
kept as written (it describes Increment 6).** The live per-feature status, with
the tests that verify each row, is `sql/MISSION1_FEATURE_MATRIX.md`. Nothing here
is a claim of Snowflake compatibility.

| Was "not supported" | Now | Slice | Notes |
|---|---|---|---|
| Scalar subqueries `(SELECT ...)` | **DONE** | A | 0 rows is NULL, 1 row its value, more than one row is a run-time error |
| `expr [NOT] IN (SELECT ...)` | **DONE** | A | three-valued logic: a NULL in the set makes a non-match UNKNOWN |
| `[NOT] EXISTS (SELECT ...)` | **DONE** | A | never NULL |
| Subqueries in `UPDATE` / `DELETE` `WHERE` | **DONE** | A | |
| Correlated subqueries (scalar / `IN` / `EXISTS`), nested to depth 32 | **DONE** | B | re-run per outer row; `UPDATE ... SET (subquery)` uses a pre-statement snapshot |
| Derived tables `FROM (SELECT ...) AS alias [(columns)]` | **DONE** | C | the alias is required; a non-`LATERAL` body cannot read the query around it |
| `[LEFT] JOIN LATERAL (SELECT ...) alias ON cond` | **DONE** | C | `FROM t, LATERAL (...)` and `CROSS JOIN LATERAL` stay unsupported (no comma / `CROSS` joins) |
| `WITH` (non-recursive CTEs), before `SELECT` / `UPDATE` / `DELETE` | **DONE** | C | computed once per statement; `WITH` before `INSERT` is accepted and has no effect |
| `UNION`, `UNION ALL`, `INTERSECT`, `INTERSECT ALL`, `EXCEPT`, `EXCEPT ALL` | **DONE** | E | `INTERSECT` binds tighter; NULLs are one value for deduplication; numeric columns widen, other types never convert; `ORDER BY` after the last query names output columns only |
| `WITH RECURSIVE name AS (seed UNION [ALL] recursive-term)` | **DONE** | D | the term reads the previous iteration; stops on no new rows; depth 1,000 / rows 1,000,000 / 8 recursive CTEs per statement; the term may not aggregate, `ORDER BY` or `LIMIT` |
| Window functions (`... OVER (...)`) | **still refused**, now reliably | D | the `OVER` clause used to be silently dropped (`SUM(x) OVER ()` ran as `SUM(x)`); it is a typed `Unsupported` error now |

**Still not supported** (unchanged): `RIGHT` / `FULL` / `CROSS` / `NATURAL JOIN`, `JOIN ... USING`,
comma joins, `INSERT ... SELECT`, `CAST`, `MINUS`, `UNION BY NAME`, `VALUES` / `TABLE` as a set-operation
operand, multi-reference recursion, mutually recursive CTEs, table-level `UNIQUE` / `FOREIGN KEY` / `CHECK`.

The "No SQL execution" and "No transactions" statements of section 11 were already superseded by
Increments 7-18 and are not repeated here.


## 13. Addendum -- SQL Mission 2 (2026-10-13): what is now supported

This section is appended; sections 2, 3 and 11 above are the original text and are SUPERSEDED where they list window functions, `ROLLUP`/`CUBE`/`GROUPING SETS`,
`COUNT(DISTINCT ...)` or aggregate `FILTER` as outside the grammar. The live status, the full matrix and every decision are in `sql/MISSION2_FEATURE_MATRIX.md`.

Accepted now:

* Window calls `f(args) [FILTER (WHERE ...)] OVER (spec | name)` with `spec` = `[base_window] [PARTITION BY e, ...] [ORDER BY e [ASC|DESC] [NULLS FIRST|LAST], ...] [ROWS|RANGE frame]`; `WINDOW name AS (spec), ...`;
  frames `{ROWS|RANGE} {start | BETWEEN start AND end}` with bounds `UNBOUNDED PRECEDING | n PRECEDING | CURRENT ROW | n FOLLOWING | UNBOUNDED FOLLOWING`.
  Functions: `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `NTILE`, `PERCENT_RANK`, `CUME_DIST`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTH_VALUE`.
* `SELECT ... QUALIFY condition` (after `HAVING`).
* `GROUP BY` elements `expr`, `ROLLUP (m, ...)`, `CUBE (m, ...)`, `GROUPING SETS (m, ...)` with members `expr` or `(expr, ...)`; `GROUP BY ()`; `GROUPING(expr, ...)`.
* `agg(expr) FILTER (WHERE cond)` and `agg(DISTINCT expr)` for `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`.

Still refused with a typed error: `GROUPS` frames, `EXCLUDE`, `IGNORE|RESPECT NULLS`, `DISTINCT` in a window aggregate, `COUNT(DISTINCT *)`, `GROUP BY ALL`, `GROUP BY ... WITH ROLLUP|CUBE|TOTALS`,
window functions outside the select list / `QUALIFY` / `ORDER BY`.
