# RubiXDB — Relational Database Architecture

**Date:** 2026-09-22

**Status:** Architecture only. No relational implementation code exists or
was added to produce this document (verified: `git status` / `git diff
--stat` clean at the end of this phase — see §16). Builds on the
already-certified storage engine (Write Engine, Read Engine, Compaction —
`PHASE_COMPACTION_CERTIFICATION.md`) and the already-certified product
layer (API + frontend — `PHASE_PRODUCT_CERTIFICATION.md`). Every decision
below is elaborated with full Decision/Reason/Alternatives/impact analysis
in the companion document `PHASE_RELATIONAL_DATABASE_ADR.md`; this
document is the structural map, not the reasoning record.

**Governing finding, carried from `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md`**:
the certified engine has no atomic multi-key write primitive. Every design
below that requires one (table+index consistency, DDL, transactions) is
architected around that gap being closed by a specific, minimal, explicitly
scoped storage extension (§8) — not by ignoring it, weakening it, or
building on top of a false assumption that it already exists.

---

## 0. Compatibility target, stated explicitly

RubiXDB's SQL surface targets **a documented PostgreSQL-inspired syntax
subset** — double-quoted identifiers, single-quoted string literals,
standard operator precedence, `RETURNING`-free DML, no procedural
extensions. This is a **syntax** compatibility target only, chosen because
the selected parser (§10) supports it natively. It is explicitly **not**:
wire-protocol compatibility, `pg_catalog` emulation, PL/pgSQL, extensions,
or any claim that a PostgreSQL client/driver can connect to RubiXDB. Any
future document that says "PostgreSQL-compatible" without this paragraph's
qualification is in error.

---

## 1. Database hierarchy

```
Database (exactly one per running LsmEngine instance, v1)
  └── Schema (namespace; default schema "public" always exists)
        └── Table (named, typed row collection; one primary key)
              └── Column (name, type, nullability, default)
```

- **Database**: v1 scope is **one database per running `LsmEngine`
  instance** — this matches the current API's own single-`RUBIXDB_DATA_DIR`
  startup contract exactly (`PHASE_API_ARCHITECTURE.md` §6). `CREATE
  DATABASE` is parsed and bound (forward-compatible grammar/catalog
  coverage) but **rejected at execution time** with a clear `Unsupported`-
  class error in this deployment mode — never silently accepted and
  faked. Multi-database-per-process (opening a second `LsmEngine` instance
  to serve a second database) is explicitly out of scope for this phase;
  it is a separate, later ADR if ever pursued. "Cluster" (the larger,
  not-yet-built multi-engine Architecture Spec's top-level concept) does
  not exist in this codebase and is not introduced here.
- **Schema**: a catalog-level namespace grouping tables, matching standard
  SQL. A `public` schema always exists and is the default resolution
  target for an unqualified table name.
- **Table**: a named, typed row collection with exactly one primary key
  (composite keys allowed — an ordered list of ≥1 columns), one
  partitioning-free physical layout (RubiXDB v1 has no partition concept;
  the larger Architecture Spec's `Partition` layer is explicitly Future
  Work and not part of this phase).
- **Column**: name, `DataType` (§5), nullability, optional default
  expression, ordinal position (fixed at creation; `ALTER TABLE ADD
  COLUMN` appends, never reorders — reordering would silently change every
  existing row's implied schema-version-to-row mapping, which the row
  format §6 depends on).

**Naming rules** (full detail in `PHASE_RELATIONAL_DATABASE_ADR.md`
"Identifier Rules"): case-insensitive-folded-to-lowercase unquoted
identifiers, case-sensitive quoted identifiers (`"MixedCase"`) — standard
SQL behavior, not a novel design. Duplicate names within one namespace
level are rejected at DDL bind time (`CREATE TABLE` on an existing name
errors unless `IF NOT EXISTS`). A small reserved-word list (SQL keywords)
is enforced by the parser, not hand-maintained separately.

---

## 2. Persistent catalog

**The catalog is not a new storage mechanism.** It is a set of ordinary
rows stored in the same `LsmEngine` keyspace as table data, under a
reserved system namespace prefix (§7). Concretely, the catalog is modeled
as a small number of fixed **system tables** (`system.databases`,
`system.schemas`, `system.tables`, `system.columns`, `system.indexes`,
`system.constraints`, `system.grants`) — self-hosting: the mechanism that stores user table
rows is the exact same mechanism that stores the catalog's own rows. This
is the direct resolution to `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md` §2.2
(the certified Manifest cannot be repurposed for this).

Consequences of this decision, each elaborated fully in the ADR:

- **Durability and crash recovery are inherited for free** — a catalog row
  survives a crash exactly as any table row does, via the already-certified
  WAL + Manifest + SSTable recovery path. No new recovery procedure is
  written.
- **Catalog mutation atomicity depends on the same new `write_batch`
  primitive (§8) as DML** — `CREATE TABLE` writes a `system.tables` row
  plus N `system.columns` rows plus one default-index `system.indexes` row;
  these must commit together.
- **Catalog reads are ordinary range scans** over the reserved prefix — no
  separate catalog cache is required for correctness (an in-process read-
  through cache may be added later purely as a performance optimization,
  explicitly not required for v1 correctness, per "do not add complexity
  the current phase doesn't need").
- **Catalog authorization** (§13) is enforced the same way table-row
  authorization is: at the binder/executor layer, before any catalog read
  is turned into a response, never by hiding rows in the frontend.

---

## 3. Type system

A closed, explicit set of typed values — never "everything is a string."

| SQL type | Representation | Ordering | v1 notes |
|---|---|---|---|
| `BOOLEAN` | 1 byte (0/1) | 0 < 1 | |
| `INTEGER` | `i32`, sign-flipped big-endian in keys | numeric | |
| `BIGINT` | `i64`, sign-flipped big-endian in keys | numeric | |
| `REAL` | IEEE-754 `f32`, monotonic bit-transform in keys | numeric (NaN excluded from key-bearing columns — see ADR) | |
| `DOUBLE` | IEEE-754 `f64`, monotonic bit-transform in keys | numeric (NaN excluded) | |
| `DECIMAL(p,s)` / `NUMERIC(p,s)` | fixed-point, scaled `i128`, sign-flipped big-endian in keys | numeric | **bounded precision/scale in v1** (documented scope cut, not arbitrary precision — see ADR) |
| `TEXT` | UTF-8 bytes, length-prefixed in row values / raw bytes in keys | byte-lexicographic (binary collation only) | no locale-aware collation in v1 |
| `BLOB`/`BYTEA` | raw bytes, length-prefixed | byte-lexicographic | |
| `DATE` | `i32` days since epoch, sign-flipped big-endian | numeric | |
| `TIME` | `i64` microseconds since midnight, big-endian (non-negative) | numeric | |
| `TIMESTAMP` | `i64` microseconds since epoch, sign-flipped big-endian | numeric | no timezone type in v1 (documented scope cut) |
| `NULL` | absence, tracked via a per-row null bitmap (§6), never a sentinel byte value inside a typed column | n/a — never equal to anything, including another `NULL`, under `=` | three-valued logic (§ below) |

Full binary-representation/comparison/NULL-behavior/literal-parsing detail
for each type is in `PHASE_RELATIONAL_DATABASE_ADR.md` "Type System" and
"NULL Semantics" decisions.

**Three-valued logic**: `TRUE`/`FALSE`/`UNKNOWN`. `NULL` propagates through
arithmetic and comparison to `UNKNOWN`; `IS NULL`/`IS NOT NULL` are the
only operators that observe `NULL` directly rather than propagating it.
`WHERE` filters out rows where the predicate evaluates to `UNKNOWN` (same
as `FALSE`, standard SQL). Aggregates skip `NULL` inputs except `COUNT(*)`.
`GROUP BY` groups all `NULL`s of a key column together (a documented
deviation from `=`-equality, standard SQL behavior). `ORDER BY` default is
`NULLS LAST` ascending / `NULLS FIRST` descending, overridable per the
grammar.

---

## 4. Row encoding

One `LsmEngine` value = one encoded row (row-major, not column-per-cell —
see ADR "Row and Key Encoding" for why a column-per-cell layout was
rejected: it would multiply WAL/MemTable/SSTable entry count per logical
row edit, directly working against the certified engine's own per-key
write-amplification profile).

```
RowValue :=
  format_version : u8            // 1
  schema_version  : u32 LE        // catalog schema_version this row was
                                   // encoded against — required so an
                                   // in-flight ALTER TABLE never needs to
                                   // rewrite every existing row (Phase 6
                                   // requirement: future format
                                   // versioning)
  null_bitmap     : [u8; ceil(column_count/8)]
  values          : per non-null column, in schema column-ordinal order,
                     type-specific fixed- or length-prefixed encoding
                     (§3's binary representation column)
```

Primary-key columns are never redundantly stored inside `RowValue` — they
are recoverable from the physical key itself (§7). This halves the
encoding/decoding cost for the extremely common "I already have the key,
give me the rest of the row" access pattern (point lookup, index-then-
fetch), and is the direct implementation of Phase 6's "minimize decoding
cost for primary-key access" requirement.

**Limits** (defaults, configurable, enforced before any engine call —
mirrors the existing API's own `max_value_bytes` enforcement pattern):
max row size (default 1 MiB, matching the existing flat-KV API default),
max column count per table (default 1,600), max single value size within a
row (bounded by max row size, no independent per-value cap in v1).

---

## 5. Primary keys and physical storage layout

```
Table row key   := 0x01 || table_id:u32 BE || 0x00000000:u32 BE (index_id=0 reserved for the table itself)
                     || encoded_pk_columns (order-preserving, §3)
Index entry key := 0x01 || table_id:u32 BE || index_id:u32 BE (>0)
                     || encoded_indexed_columns (order-preserving, §3)
                     || encoded_pk_columns (tie-break + row pointer, always appended)
Catalog row key := 0x00 || system_table_id:u32 BE || encoded_pk_columns
```

- `0x01`/`0x00` is the reserved top-level namespace split (relational data
  vs. system/catalog data) — see §7 and the ADR's "Backward Compatibility"
  decision for why this must be a *reserved*, API-enforced prefix, not
  merely a convention.
- Fixed-width big-endian `table_id`/`index_id` preserve correct
  byte-lexicographic grouping and ordering per table/index — this is what
  makes a table scan or an index range scan a single contiguous
  `range_scan` call against the certified engine, with no cross-table
  scanning ever required to answer a single-table query (Phase 7's "avoid
  designs that require scanning unrelated tables" requirement).
- A secondary index entry always appends the row's primary key, even for a
  `UNIQUE` index — this both resolves ties between rows with equal indexed
  values and gives the executor a direct pointer to fetch the full row
  (index-then-fetch), without needing a second index format for "covering"
  vs. "non-covering" indexes in v1.
- `NULL` in an indexed (non-PK) column: encoded as a reserved
  lowest-sorting marker byte, consistently, so `NULL`s in a secondary index
  sort together at one end rather than colliding with or interleaving
  incorrectly with real values. Primary-key columns may never be `NULL`
  (standard SQL `PRIMARY KEY` ⇒ `NOT NULL`, enforced at bind time, not
  merely by convention).

---

## 6. Indexes

**v1 scope**: `PRIMARY KEY` (the table's own row key, §5 — not a separate
physical structure), `UNIQUE`, and non-unique `NON-UNIQUE` secondary
indexes. All are real, persistent, **ordered LSM-backed range
structures** — their ordering is inherited directly from the certified
engine's own ordered keyspace (§5: MemTable `BTreeMap` iteration order +
SSTable sorted-block layout + the certified merge-on-read across
sources), not from a B-tree or any other independent balanced-tree
implementation — **never an in-memory `HashMap`**, per explicit
instruction. If a future phase wants an actual B-tree-backed secondary
structure, that is a new physical index type requiring its own
dedicated ADR (`RELATIONAL ADR AMENDMENT 001` AA.16). Index maintenance
(insert/update/delete keeping the index in lockstep with the table) is a
direct consequence of the atomic multi-key `write_batch` primitive (§8):
every DML statement that touches an indexed column writes the table row
and every affected index entry in one atomic unit.

`CREATE INDEX`/`DROP INDEX` are DDL, going through the same atomic
catalog-plus-data-write path as `CREATE TABLE`; building an index on a
table that already has rows requires a full table scan to backfill index
entries — v1 scope: this backfill runs as one bounded, resumable-on-crash-
via-recovery operation (not requiring an online, dual-write phase; that
level of migration complexity is explicit Future Work, matching the larger
Architecture Spec's own "no online migration in v1" precedent).

---

## 7. Storage layout summary

| Data class | Physical namespace | Ordering guarantee |
|---|---|---|
| Catalog (databases/schemas/tables/columns/indexes/constraints) | `0x00 \|\| system_table_id \|\| pk` | Per-system-table contiguous, enables `system.tables` listing via one range scan |
| Table rows | `0x01 \|\| table_id \|\| 0x0 \|\| pk` | Per-table contiguous; PK order |
| Index entries | `0x01 \|\| table_id \|\| index_id \|\| indexed_cols \|\| pk` | Per-index contiguous; indexed-column order |
| Pre-existing flat-KV client data (`/v1/kv`) | anything **not** starting `0x00` or `0x01` once the relational layer is enabled | unchanged |

Collision-safety rests entirely on the `0x00`/`0x01` reservation being
enforced at the API boundary (rejecting a flat-KV client write whose key
starts with either reserved byte) — detailed as a Decision, with its
compatibility consequence stated plainly, in the ADR.

---

## 8. Storage extension required (summary — full ADR entry has the complete analysis)

One new `LsmEngine`-level primitive, and only one:

```
pub fn write_batch(&self, ops: &[WriteOp]) -> Result<u64>
// WriteOp = Put { key, value } | Delete { key }
// Atomic: one WAL frame encoding all N ops, one durability wait, one
// critical-section MemTable apply. All-or-nothing visibility; the
// returned seq is the batch's single assigned seq (or the max of a
// contiguous assigned range — resolved precisely in the ADR).
```

This is the **only** point where this architecture proposes extending
certified engine surface. It is not implemented by this phase. Its full
Decision/Reason/Alternatives/impact analysis, along with why every other
gap in `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md` is resolvable without
touching the engine, is in `PHASE_RELATIONAL_DATABASE_ADR.md`.

---

## 9. Transaction model

**Model**: Snapshot Isolation (SI), not Serializable — stated honestly, not
oversold. `BEGIN` captures an `LsmEngine::snapshot()` as the transaction's
read view; all statements in the transaction read as of that snapshot's
seq; writes are buffered in an in-memory, per-transaction write-set (never
applied to the engine until commit); `COMMIT` validates the write-set
against the *current* committed state of every physical key it touches
(table row key, and every affected index entry key) — if any touched key
has been modified by a transaction that committed after this transaction's
snapshot was taken, the commit is aborted with a conflict error (the
client must retry); otherwise the entire write-set is applied via
`write_batch` (§8) as one atomic unit. `ROLLBACK` simply discards the
buffered write-set — nothing was ever applied to the engine, so there is
nothing to undo.

**A `PRIMARY KEY`/`UNIQUE` violation is a special case of exactly this same
conflict-detection mechanism**: inserting a row adds its own PK/unique-
index keys to the write-set; if a concurrent transaction already committed
a row with the same key, the commit-time key-freshness check catches the
collision the same way an ordinary write-write conflict is caught — no
separate uniqueness-checking code path is needed.

**Known, stated limitation**: write skew (a classic SI anomaly — two
transactions each read overlapping state, write disjoint keys, and both
commit even though the combined result violates an invariant neither
transaction's own write-set conflict check alone can see) is possible and
not prevented in v1. This is a scope decision, made explicitly, not an
oversight — full serializability requires either a read-set-tracking
scheme (SSI) or pessimistic locking, both substantially more expensive,
and the directive's own instruction not to over-claim isolation levels
without proof governs this choice directly.

---

## 10. SQL parser, AST, binder

- **Parser**: `sqlparser-rs` (crate `sqlparser`), a mature, widely-used,
  actively maintained pure-Rust SQL parser (used by DataFusion, GlueSQL,
  and others), configured with its `GenericDialect`/`PostgreSqlDialect`
  for the §0 syntax subset. Chosen over hand-rolled parsing per explicit
  instruction ("Use a proper parser... Do NOT use regex parsing") and over
  writing a new grammar from scratch because re-deriving a correct SQL
  grammar has no justified benefit here. Lives in a **new** crate
  (`rubixdb-sql`, a new workspace member) — never added as a dependency of
  the core `rubixdb` engine crate, preserving that crate's own
  "zero new dependency" certified property exactly the way `api/` already
  does for its own dependencies (`PHASE_API_ARCHITECTURE.md` §1.1).
  Security posture, exact supported-grammar subset, and dependency-hygiene
  verification (no `unsafe`, license compatibility) are **testing
  requirements for the increment that adopts it**, not asserted here
  without evidence — see ADR "SQL Parser" decision.
- **AST**: `sqlparser-rs`'s own `ast::Statement`/`ast::Expr` tree, consumed
  directly by the binder rather than re-modeled into a second, parallel
  AST type — avoids a translation layer with its own bug surface.
- **Binder**: resolves every identifier (database → schema → table →
  column, aliases, function names, type names) against the catalog (§2),
  through the same authorization check every other catalog access uses
  (§13). Produces a fully-typed, fully-resolved logical tree — no untyped
  or unresolved node reaches the planner. Detects and rejects (with a
  precise, safe-to-display error, never a raw parser/engine internal):
  unknown object, unknown column, ambiguous column, invalid function,
  invalid type, invalid scope (e.g., an aggregate outside `GROUP BY`
  context).

---

## 11. Query planner, optimizer, executor

**Pipeline**: `SQL → Parser → AST → Binder → Logical Plan → Rule-Based
Optimizer → Physical Plan → Executor`. The parser never executes anything
directly (Phase 15's explicit requirement).

**Logical operators (v1)**: `SeqScan`, `IndexScan`, `Filter`, `Projection`,
`Sort`, `Limit`, `Offset`, `Distinct`, `Aggregate`, `Join` (nested-loop
family only, §12).

**Optimizer**: rule-based only, deterministic, no invented cost constants —
matching the storage engine's own established discipline of not
guessing at a strategy before real benchmark data justifies one
(`RubixDB-LSM-Engine-Specification-v1.0.md` §5.1's compaction-strategy
precedent is the direct model for this choice). v1 rules: predicate
pushdown (translate a full-PK-equality or PK-range predicate directly into
an `IndexScan`/point `get` against the table's own key, §5), projection
pruning (decode only the columns a query actually needs from `RowValue`,
§4), index selection (an equality/range predicate on an indexed column
prefers `IndexScan` over `SeqScan`, chosen by a deterministic rule — most
specific matching index by leading-column match — not a cost model),
limit pushdown (stop iterating once `LIMIT` is satisfied — falls out
naturally from the pull-based executor model below). Statistics-driven
costing is explicitly deferred until real statistics exist (§14) — no
"fake cost optimizer with arbitrary constants," per instruction.

**Executor**: Volcano-style pull-based iterators — `next()` produces one
row (or small batch) at a time, matching the certified `range_scan`'s own
lazy, bounded-memory k-way-merge design (`ADR-RE-002`) rather than
introducing a materialize-then-process pattern the storage layer's own
architecture deliberately avoided. `SeqScan`/`IndexScan` wrap
`LsmEngine::range`/`range_scan` directly. `Filter`/`Projection` are
zero-materialization per-row transforms. `Sort`/`Aggregate`/`GroupBy`/
`HashJoin` are the operators that require materialization (§12/§13) —
each has an explicit, bounded memory strategy, never an unbounded one.

---

## 12. Joins

**v1 scope**: `INNER JOIN`, `LEFT JOIN`. `RIGHT`/`FULL`/`CROSS JOIN`
deferred to a later increment, per the directive's own phased instruction.

**Algorithm**: **Nested Loop Join** as the correct baseline for any join
shape, and **Index Nested Loop Join** automatically substituted by the
optimizer whenever a usable index exists on the inner relation's join key
(a direct, mechanical rule — not a cost decision, since no cost model
exists yet). **Hash Join and Merge Join are explicitly deferred** — Phase
19's own instruction to "benchmark candidates" cannot be honestly followed
before an executor exists to benchmark; committing to Hash/Merge Join now
would be exactly the "select an algorithm blindly" anti-pattern the
directive forbids. This is revisited, with real benchmark data, once
Increment 10 (joins + aggregation) has a working baseline to measure
against.

---

## 13. Aggregation, sorting, `GROUP BY`, `DISTINCT`

`COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, `NULL`-skipping per §3, `COUNT(*)` counts
rows regardless of `NULL`, empty-input `SUM`/`AVG`/`MIN`/`MAX` returns
`NULL`, empty-input `COUNT` returns 0. `GROUP BY` groups `NULL`s of a key
column together (§3). `HAVING` filters post-aggregation groups, same
three-valued-logic rule as `WHERE`.

**Memory strategy for `Sort`/`GroupBy`/hash-based operators**: **bounded
in-memory with a hard, configurable cap; exceeding it fails the query with
a explicit, typed resource-limit error** — not silent truncation, not
unbounded growth, and (v1 scope decision, stated plainly) **not
disk-spill**. Disk-spill is named as the documented future path once a
real workload shows the bounded-memory cap is actually hit in practice
(mirrors the exact reasoning `RubixDB-LSM-Engine-Specification-v1.0.md`
§2.3 used to defer prefix compression: "a candidate optimization once real
benchmark data shows [it] actually matters," not invented ahead of
evidence). This directly answers Phase 18's requirement to pick one of
"spills to disk / rejects oversized operations / uses bounded memory" and
document it — v1 picks the last two together: bounded, and rejects when
the bound is exceeded.

---

## 14. Statistics

**v1 scope: none beyond what the deterministic optimizer rules (§11)
need**, which is none — every v1 rule is structural (does an index exist
on this column?), not selectivity-based. Row-count/cardinality statistics
collection is explicitly deferred to a future increment, to be introduced
only once real benchmark data justifies a cost-based rule, matching
Section 11.2 of the larger Architecture Spec's own governing principle
almost verbatim ("a cost model calibrated against invented numbers is
worse than no cost model, because it hides its own wrongness").

---

## 15. CLI, API, frontend — one executor, three surfaces

Per explicit instruction ("The same planner/executor must be used by CLI,
API, and frontend"): the parser/binder/planner/executor (`rubixdb-sql`, a
library crate, §10) lives inside the `rubixdb-api` service process. The
API gains exactly one new endpoint, `POST /v1/sql` (§15.1). The CLI
(`rubixdb>` prompt, `\l`/`\dn`/`\dt`/`\d`/`\di`/`\du`/`\conninfo`/`\c`/
`\help`/`\q` plus real SQL) is a thin HTTP client of that same endpoint —
**not** a second process embedding the engine library directly. This is a
deliberate rejection of an "embedded/local CLI mode" alternative: two
divergent execution paths (one going through the service's binder/
planner/executor, one bypassing it) would let CLI and API results drift,
which the explicit "same executor" instruction exists specifically to
prevent. Script mode (`rubixdb -c "..."`, `rubixdb -f script.sql`) is the
same HTTP client in non-interactive mode.

### 15.1 `POST /v1/sql`

```
Request:  { sql: string, params: [TypedValue] (optional), tx_id: string (optional) }
Response: { columns: [{name, type}], rows: [[TypedValue]], rows_affected: u64,
            execution_time_ms: f64, error: ApiErrorBody | null }
```

Follows the existing API's own established conventions exactly: additive-
only JSON contract (never restructures `/v1/kv` etc.), the same
`EngineError`→typed-error mapping discipline (§13), the same base64-for-
binary-values convention where a column type is `BLOB`. Parameterized
queries (`$1`, `$2`, ...) are the **only** supported way to pass
user-supplied values into a statement — never string concatenation,
per §17.

### 15.2 Frontend

The existing Data Explorer screen (`PHASE_FRONTEND_ARCHITECTURE.md` §2)
gains a SQL editor + result table + `EXPLAIN` view, consuming `/v1/sql`
exactly as any other screen consumes its own endpoint — no separate query
logic duplicated client-side. Database/schema/table/column/index/
constraint browsers are read-only views over `system.*` catalog tables via
ordinary `SELECT`s through the same endpoint, not a bespoke metadata API.
Every screen's write action remains role-gated in the UI as a usability
layer only (§13 — authorization is enforced server-side, never by the
frontend).

---

## 16. Security summary (full detail in ADR "Security" decisions)

Authentication/authorization extend the existing bearer-key, role-based
model (`PHASE_API_ARCHITECTURE.md` §4) with object-level grants
(database/schema/table × SELECT/INSERT/UPDATE/DELETE/DDL/CREATE INDEX),
stored as catalog rows (`system.grants`), checked in the binder before
execution — never in the frontend, never in the CLI. SQL injection is
structurally prevented, not merely discouraged: the binder never accepts
a raw string as part of a statement's grammar; only `$n` parameters carry
user data, and identifiers go through the parser's own quoting rules, never
string interpolation. Every resource limit in §17 is enforced before the
expensive operation begins, server-side.

---

## 17. Resource limits (defaults — full table in ADR)

Max tables/database (10,000), max columns/table (1,600), max row size (1
MiB), max SQL statement size (1 MiB), max result rows (default 100 / hard
cap 10,000, mirroring `/v1/range`'s existing convention exactly), max
transaction write-set size (10,000 ops, configurable), max concurrent
transactions (bounded pool, configurable), max query runtime (30s
default), max sort/group/join memory per operator (64 MiB default,
configurable). Each is enforced server-side, fails loud with a typed
error, never silently truncates.

---

## 18. Observability

New, additive-only relational metrics (SQL statement count by type, query
latency percentiles, rows scanned/returned/affected, scan/join/sort/
aggregate operator counts, transaction begin/commit/rollback/conflict
counts, constraint-violation counts) exposed under a new `sql` key in
`/v1/metrics`'s existing JSON body — never restructuring the existing
engine-metrics keys or the `service` key (`PHASE_API_ARCHITECTURE.md`
§5's own convention). Engine-level metrics (`ReadStats`,
`CompactionMetrics`, etc.) are read, never recomputed or duplicated. Full
decision record — including the cardinality-bound and no-query-content-
leakage requirements — is the ADR's **D33. Observability**.

---

## 19. Recovery

Because catalog and table/index rows are ordinary engine keys (§2, §7),
crash recovery for all of them is the certified engine's own existing
recovery procedure (`RubixDB-LSM-Engine-Specification-v1.0.md` §7) with
**zero new recovery code**. The one new recovery consideration is at the
`write_batch` primitive (§8) itself — its own crash-window analysis is the
ADR's "Transaction Model" and "Crash Recovery" decisions' responsibility,
not a second, relational-layer-specific recovery procedure layered on top.

---

## 20. What this document does not decide

Deferred to implementation-time increments, per the directive's own
Implementation Order (Increments 2-18): exact Rust module layout inside
`rubixdb-sql`, exact `sqlparser-rs` version pin and full unsupported-
grammar enumeration, exact `system.*` catalog table column lists, the
`write_batch` primitive's own implementation-time ADR
(`PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`, Phase 9), and every
increment's own test plan. This document and `PHASE_RELATIONAL_DATABASE_ADR.md`
are the architecture Increments 2+ build against — not the other way
around, matching the same discipline `PHASE_API_ARCHITECTURE.md` §7
already established for its own downstream implementation phase.

---

## 21. Source safety

```
git status                 clean at the start and end of this phase
git diff --stat             (empty except this phase's own new .md files)
git diff --name-only        (this phase's own new .md files only)
```

No relational implementation code was added. See §22 (Final Report) in
this project's response for the explicit statement of scope closure.
