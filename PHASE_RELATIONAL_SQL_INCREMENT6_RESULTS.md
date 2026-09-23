# PHASE_RELATIONAL_SQL_INCREMENT6_RESULTS

**Scope**: production SQL parser + internal AST + binder + authorization
resolution — the security boundary between untrusted SQL text and the
future execution engine. `PHASE_RELATIONAL_SQL_GRAMMAR.md` is the full
structural/reference record; this document records what was actually
built, measured, and verified.

**No SQL execution exists.** No planner, optimizer, executor,
transaction execution, CLI, API SQL endpoint, or frontend SQL console.
**RELATIONAL DATABASE PRODUCTION READY = NO.**

---

## 1. What was implemented

New workspace crate `rubixdb-sql` (directory `sql/`), depending on the
core `rubixdb` crate (never the reverse) and `sqlparser = "=0.63.0"`
(never added to the core crate's own dependency list):

- `ast.rs` — the internal AST (never exposes `sqlparser::ast` beyond the
  conversion boundary), every `Statement` variant documenting its own
  PARSED/BOUND/NOT-EXECUTABLE-YET status.
- `convert.rs` — `sqlparser::ast` → `crate::ast`, rejecting every
  construct outside the approved grammar subset with a typed
  `Unsupported` error (never silently dropped).
- `parse.rs` — the parser boundary: byte-size limit, `sqlparser`'s own
  recursion guard, and (a finding of this increment — see §4) a pre-
  parse defense against a flat-operator-chain stack-overflow hazard
  `sqlparser`'s own guard does not cover.
- `bind/` (`scope.rs`/`expr.rs`/`select.rs`/`dml.rs`/`ddl.rs`) — the
  binder: catalog identifier resolution and D25 authorization in the
  same pass, type resolution with no implicit coercion, `JOIN`
  scope/ambiguity/nullability, wildcard expansion, DDL/DML shaped to the
  exact `CatalogService`/`IndexBuilder` argument sets a future executor
  needs.
- `bound.rs` — the fully resolved, typed, authorized representation.
- `auth.rs` — `AuthContext`/`DefaultAccess`, D25's default-privilege
  mapping plus explicit-grant ancestor-chain checking.
- `functions.rs` — the explicit, closed function registry.
- `temporal.rs` — dependency-free `DATE`/`TIME`/`TIMESTAMP` literal
  parsing (Howard Hinnant's `days_from_civil` algorithm — no `chrono`/
  `time` crate added).
- `error.rs`/`limits.rs`/`metrics.rs` — the safe error taxonomy, resource
  limits, bounded-cardinality parse/bind counters.
- `benches/sql_parser_bench.rs`, `benches/sql_binder_bench.rs`.

## 2. ADR amendment: Identifier Rules

`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §1 promises a dedicated
"Identifier Rules" section in the ADR that was never actually written
(verified: no such heading exists). This increment supplies it in
`PHASE_RELATIONAL_SQL_GRAMMAR.md` §9, using exactly the behavior the
Architecture doc's own prose already committed to — case-fold unquoted,
preserve quoted — not inventing new semantics. This is the "smallest
necessary ADR amendment" the governing directive anticipates for a
genuinely missing (not merely unwritten-out) piece of the architecture.

## 3. Files changed

`Cargo.toml` (+`sql` workspace member), `Cargo.lock` (+`sqlparser` and
its transitive deps), `sql/` (new crate, ~20 source files + 2
benchmarks + this results doc + `PHASE_RELATIONAL_SQL_GRAMMAR.md`).
**Zero changes to `src/` (the core `rubixdb` crate) or `api/`.**

## 4. A real finding: the flat-operator-chain stack overflow

`sqlparser`'s `Parser::with_recursion_limit`/`RecursionLimitExceeded`
protects its own *parsing* call stack — but a **flat, left-associative
chain** of binary/unary operators (`1 + 1 + 1 + ... `, `a AND a AND a
AND ...`) is parsed by a Pratt/precedence-climbing loop, not recursive
descent, so it never trips that guard. The result is still a
correspondingly deep `Box<Expr>`-linked tree, and Rust's ordinary
recursive `Drop` for that tree overflows the stack — **not a returned
error, a process crash** — the instant the tree (or anything containing
it) goes out of scope, regardless of whether this crate's own
`crate::convert::DepthGuard` ever ran.

**Reproduced deterministically**: a 20,000-term `+` chain, generated
by `fuzz_tests::deeply_nested_binary_expression_is_rejected_not_a_stack_
overflow`, crashed the test binary with `STATUS_STACK_OVERFLOW` before
the fix (`parse::reject_pathological_operator_chains`, a cheap pre-
tokenizer scan counting operator-shaped characters/keywords, rejecting
before the parser ever builds the tree — item 9's explicit "if
`sqlparser-rs` behavior itself requires a guard: implement the guard in
the RubiXDB SQL boundary" instruction, applied to a hazard actually
found, not hypothesized). Verified fixed: the same 20,000-term input now
returns `SqlError::ResourceLimit` and the process survives. Nested
parens/`NOT` chains were separately confirmed to already be correctly
covered by `sqlparser`'s own recursion guard (genuine recursive descent
for those constructs) — the fix targets only the specific gap found, not
a blanket re-implementation of `sqlparser`'s own protection.

## 5. Full regression gate

| Suite | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | clean |
| `cargo check --all-targets --all-features` | clean |
| `cargo test --workspace --lib` (debug) | 497 `rubixdb` + 30 `rubixdb-api` + 98 `rubixdb-sql` passed, 0 failed |
| `cargo test --release --workspace --lib` | 497 + 30 + 98 passed, 0 failed |
| `cargo test --test wal_tests` | 12 passed, 0 failed |
| `cargo test --test pathological_recovery_matrix` | 9 passed, 0 failed |
| `cargo test --release --test crash_consistency --features test-util` | 2 passed, 0 failed |

`git diff --stat -- src/manifest/ src/compaction/ src/sstable/ src/wal/
api/` — empty for every path. `git diff --stat -- src/` (the whole core
engine/relational crate) — **empty**; this increment touched no file
outside `sql/`, `Cargo.toml`, `Cargo.lock`, and its own two new `.md`
docs.

## 6. Tests added (98 in `rubixdb-sql`)

- **`parse_tests`** (item 11, 18 tests): whitespace/comments/case,
  quoted-identifier case preservation, escaped strings, `NULL`/boolean/
  numeric/negative/`BIGINT`-boundary/typed-temporal literals, parameter
  parsing, nested parens/precedence, aliases, schema qualification,
  every supported statement family, invalid syntax, truncated SQL
  (every prefix of a real statement, never panicking), unsupported
  grammar as a typed error, multi-statement scripts.
- **`limits_tests`** (item 34, 7 boundary tests): N-1 succeeds / N fails
  for statement bytes, statements-per-script, expression depth,
  identifier length, `IN` list size, `VALUES` row count, column count.
- **`fuzz_tests`** (item 10/41, `proptest`-driven + targeted, 9 tests):
  arbitrary UTF-8/bytes-as-lossy-UTF-8/keyword-biased noise, deep `NOT`
  nesting, arbitrary parameter indices, malformed numeric/typed
  literals, null bytes/control characters — all "never panic," plus the
  §4 stack-overflow regression test specifically.
- **`bind_tests`** (item 24 etc., 28 tests): column/table resolution and
  type checks for every statement family, `JOIN` binding (ambiguity
  rejection, `LEFT JOIN` nullability vs. `INNER JOIN`), wildcard/
  qualified-wildcard expansion, function binding, `BETWEEN`/`IN`
  shared-type unification, `NULL` assignability, `ORDER BY` `NULLS`
  defaults, parameter-index limit, `INSERT`/`UPDATE`/`DELETE` binding
  (missing-`NOT NULL`, explicit-`NULL`-into-`NOT NULL`, primary-key
  `UPDATE` rejection), DDL binding (composite `PRIMARY KEY`, missing
  `PRIMARY KEY`, `DROP ... IF EXISTS` no-op), authorization (reader/
  admin/none, explicit grants, the indistinguishable-error property),
  metrics recording.
- **`security_tests`** (item 33/42, 11 tests): the OWASP injection
  corpus as literal values (byte-for-byte inert) and as parameters,
  identifier injection via quoting, stacked-query rejection, and five
  distinct authorization-bypass attempts (case-folding, quoting, alias,
  wildcard, schema-qualification) plus a join-based column-leak attempt
  — all confirmed denied.
- **`reference_model`** (item 40, D30's "never the production algorithm
  as its own oracle" applied to the binder): a from-scratch column-
  resolution model, compared against the real `Scope::resolve_column`
  over a fixed matrix plus a 64-case `proptest` fuzz of generated
  queries — zero mismatches.

## 7. Security audit

No `unsafe` in this crate's own code (grepped). No new logging of SQL
text, parameter values, or row contents — `crate::metrics::SqlMetrics`'s
recorder methods take only already-classified counters, structurally
incapable of holding either (item 44). Physical catalog IDs are never
client-supplied — every resolution path goes through `CatalogService` by
name (verified directly, `security_tests::physical_ids_are_never_
accepted_as_client_input`). No new panics beyond the pre-existing
`.expect(...)`-on-already-checked-invariant pattern the core crate
already established (e.g. `bind_shared`'s length-preserving `try_into`).

## 8. Performance (measured, `cargo bench -p rubixdb-sql`, this machine, release profile)

### Parser (item 35)

| Input | Size | Mean time | Throughput |
|---|---|---|---|
| tiny (`SELECT id FROM t`) | 17 B | 7.9 µs | 1.93 MiB/s |
| small (params, `WHERE`, `ORDER BY`, `LIMIT`) | 79 B | 30.0 µs | 2.35 MiB/s |
| medium (`JOIN`, 20 `AND` predicates) | 693 B | 164 µs | 2.41 MiB/s |
| large (500-row `INSERT ... VALUES`) | 12.1 KiB | 2.48 ms | 4.74 MiB/s |
| malformed (same scale, broken syntax) | 12.1 KiB | 2.23 ms | 5.28 MiB/s |

**Honest reading**: a malformed statement at realistic size costs
essentially the same as a valid one of the same size (the tokenizer
still processes every byte before hitting the syntax error near the
end) — no pathological slow-path for hostile-but-large input was found.

### Binder vs. catalog size (item 36)

| Tables in schema | Mean bind time |
|---|---|
| 100 | 136 µs |
| 1,000 | 1.28 ms |
| 5,000 | 6.14 ms |
| 10,000 | 11.96 ms |

**Measured, not assumed**: bind latency scales linearly with table
count (100→10,000 tables, a 100x increase, produced an ~88x latency
increase) — dominated by `CatalogService::list_tables`'s own already-
documented full-scan-filtered-by-schema design (`RELATIONAL ADR
AMENDMENT 002` CA.2, a pre-existing, accepted tradeoff from the catalog
increment, not introduced here). This is exactly the shape item 36 warns
about ("the binder must not scan the entire catalog for every
identifier... if measurements show catalog lookup is dominating,
investigate the actual source") — investigated and traced to its real
source; **no cache was added**, per item 36's own explicit instruction
not to introduce one automatically. A read-through catalog cache is the
direct, measurement-motivated response a future increment would reach
for if this cost matters in practice (the same conclusion `PHASE_
RELATIONAL_ROW_STORAGE_RESULTS.md` §6 already reached for `get_row`'s
analogous per-call catalog resolution cost).

## 9. Dependency security audit (item 43)

`sqlparser = "=0.63.0"`, Apache-2.0, no `unsafe` introduced by this
crate's use of it, recursion-protected by default, pinned exact per this
project's established discipline. `Cargo.lock` records the exact
resolved versions of every transitive dependency it pulls in
(`recursive`, `stacker`, `psm`, `object`, `ar_archive_writer` — all
build/stack-probing support crates for the `recursive-protection`
feature, not SQL-parsing code themselves).

---

## Certification

| Gate | Result |
|---|---|
| SQL PARSER | PASS |
| SQL AST | PASS |
| TYPE/LITERAL PARSING | PASS |
| PARAMETERS | PASS |
| IDENTIFIER RESOLUTION | PASS |
| CATALOG BINDING | PASS |
| AUTHORIZATION BINDING | PASS |
| EXPRESSION BINDING | PASS |
| DDL BINDING | PASS |
| DML BINDING | PASS |
| SELECT BINDING | PASS |
| SECURITY | PASS (one real stack-overflow hazard found and closed, §4) |
| RESOURCE LIMITS | PASS |
| PARSER FUZZ/PROPERTY TESTS | PASS |
| BINDER PROPERTY TESTS | PASS (independent reference model, §6) |
| PERFORMANCE | PASS (measured, §8) |
| DEPENDENCY SECURITY | PASS |

**WRITE ENGINE = PRODUCTION READY**
**READ ENGINE = PRODUCTION READY**
**COMPACTION = PRODUCTION READY**
**RELATIONAL CATALOG = PASS**
**RELATIONAL ROW STORAGE = PASS**
**SECONDARY INDEXES = PASS**

**RELATIONAL DATABASE PRODUCTION READY = NO** — planner, executor,
transactions, constraint enforcement beyond what binding itself checks,
SQL execution, CLI, API SQL, frontend SQL, long-duration relational
endurance, and final relational certification all remain. Per the
governing stop condition, none of them is started by this increment.
