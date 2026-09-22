# ADR-RELATIONAL-001 — RubiXDB Relational Database Architecture Decisions

**Date:** 2026-09-22

**Status:** Proposed (architecture phase; no implementation exists). Every
decision below was checked against the rule "nothing here proposes
changing certified storage-engine semantics without an explicit new ADR
decision that says so" — exactly one decision (D9) proposes a storage
extension, and it is called out as such everywhere it is referenced.

**Inputs**: `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md` (what the engine
can and cannot do today), `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`
(the structural design these decisions justify), the certified engine's
own specifications and certification documents (cited inline), and
`PHASE_API_ARCHITECTURE.md`/`PHASE_FRONTEND_ARCHITECTURE.md` (the product
layer this design must not contradict).

**Format**: every decision states Decision, Reason, Alternatives
(rejected, with why), and impact across Correctness / Security /
Performance / Memory / Persistence / Recovery, plus Testing requirements.

---

## D1. Catalog architecture

**Decision**: The catalog is not a new storage mechanism. It is a fixed
set of system tables (`system.databases`, `system.schemas`,
`system.tables`, `system.columns`, `system.indexes`, `system.constraints`,
`system.grants`) whose rows live in the same `LsmEngine` keyspace as user
table rows, under the reserved `0x00` namespace prefix (D2), encoded with
the same `RowValue` format user rows use (D3).

**Reason**: `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md` §2.2 established
that the certified Manifest (`src/manifest/format.rs`) is a closed,
3-variant enum tightly coupled to SSTable lifecycle, is an explicitly
protected path across every prior certification's own audit gate, and is
itself explicitly non-compacting/unbounded-growth (an accepted limitation
for SSTable bookkeeping traffic, a poor fit for long-lived, frequently
rewritten catalog data). Self-hosting the catalog inside the ordinary
keyspace means it inherits WAL durability, crash recovery, snapshot reads,
and (eventually) compaction reclamation for free, with zero new
persistence code.

**Alternatives rejected**:
- *Extend `ManifestEdit` with a catalog variant.* Rejected: requires
  editing a certified, protected path; conflates two unrelated concerns
  in one format; inherits the Manifest's own unbounded-growth limitation
  for exactly the kind of long-lived data that limitation is worst for.
- *A second, parallel WAL+file mechanism dedicated to the catalog*
  (mirroring the larger Architecture Spec's own "metadata is partition
  zero" language literally, as a wholly separate subsystem). Rejected as
  unjustified complexity: it would duplicate WAL/recovery logic the
  certified engine already provides, for no benefit over simply using
  that engine directly as ordinary keys.
- *In-memory-only catalog, rebuilt by scanning*. Rejected outright —
  explicitly forbidden ("Do not store the authoritative catalog only in
  memory").

**Correctness impact**: catalog reads are ordinary `range_scan`s over a
reserved prefix — the same version-resolution and tombstone-collapse
correctness the certified Read Engine already provides applies unchanged.
**Security impact**: catalog access goes through the same
authorization check (D25) as any table access — no separate, easier-to-
forget authorization path for metadata. **Performance impact**: catalog
lookups are point/range reads against a small, typically hot (cacheable
at a future increment) keyspace region; no measurable cost beyond ordinary
engine reads, and catalog traffic volume is many orders of magnitude below
table-data traffic in any realistic workload. **Memory impact**: none
beyond ordinary MemTable/SSTable index residency, already accounted for in
the certified engine's own bounded-by-live-SSTable-count model. **Persistence
impact**: full WAL durability, inherited. **Recovery impact**: full crash
recovery, inherited, zero new code. **Testing requirements**: catalog
round-trip (write, restart, re-read), concurrent DDL against the same
catalog rows (must go through D9's atomicity), catalog authorization
bypass attempts (D25/D26).

---

## D2. Physical key layout / namespace reservation

**Decision**: Reserve the first key byte: `0x00` = system/catalog,
`0x01` = relational table/index data. Any other leading byte remains
available to pre-existing flat-KV clients of `/v1/kv`. Table/index keys
use fixed-width big-endian `table_id`/`index_id` (u32) immediately after
the `0x01` byte, followed by an order-preserving encoding of key column
values (D4).

**Reason**: the certified engine imposes no key structure at all
(`PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md` §2.3) — without an explicit,
enforced reservation, a relational table's physical keys and an existing
flat-KV client's arbitrary keys can collide, silently corrupting one or
the other. Fixed-width big-endian IDs are required, not incidental: a
variable-width or textual ID would break byte-lexicographic grouping
(e.g., table_id `9` and `10` sorting adjacent to table_id `1`'s data under
naive string comparison) — the correctness of every "one table = one
contiguous range" scan in this design depends on fixed-width numeric IDs.

**Alternatives rejected**:
- *Hash-based key derivation (e.g., a hash of table name as prefix)*.
  Rejected: destroys ordering — table scans would degrade to
  full-keyspace scans with a filter, defeating the entire point of the
  certified engine's range-scan primitive.
- *Variable-length textual namespacing (e.g., `"mytable:" || pk`)*.
  Rejected: variable-length prefixes break clean prefix-boundary range
  queries (a scan for table `"a"` could spuriously include table `"ab"`
  without careful, error-prone boundary-byte handling) and cost more
  bytes per key than a fixed 4-byte ID at scale.

**Correctness impact**: table/index isolation is structural (byte-prefix
boundaries), not merely conventional — a scan bounded by
`[0x01||table_id||0x00, 0x01||table_id||0x01)` cannot observe another
table's rows by construction. **Security impact**: this is also a
tenant/object isolation primitive — a future per-table authorization
check (D25) can trust that no cross-table data leakage is physically
possible via a mis-scoped range query. **Performance impact**: 5 fixed
overhead bytes (`0x01` + 4-byte ID) per key beyond the encoded key
columns — negligible relative to typical row/index key sizes, and
strictly beneficial versus the alternatives' scan-cost consequences.
**Memory impact**: none beyond the small, fixed per-key overhead reflected
in SSTable/MemTable size accounting exactly as any other key bytes are.
**Persistence impact**: none beyond ordinary key/value durability.
**Recovery impact**: none — recovery is key-structure-agnostic.
**Testing requirements**: reserved-prefix rejection at the flat-KV API
boundary (D32), table_id/index_id boundary correctness at min/max u32
values, cross-table/cross-index scan-isolation property tests.

---

## D3. Row format / encoding

**Decision**: `RowValue := format_version:u8 || schema_version:u32 LE ||
null_bitmap:[u8; ceil(n/8)] || values...` (full layout in
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §4). Row-major: one engine
value encodes an entire row. Primary-key columns are never duplicated
inside the value (recoverable from the physical key itself, D2).

**Reason**: row-major avoids multiplying per-edit engine key count (a
column-per-cell layout would turn one logical `UPDATE` touching one column
into N separate engine keys touched, multiplying WAL/MemTable/SSTable
write amplification by column count for no benefit at this project's
scale). `schema_version` per row (not a single global version) is required
because `ALTER TABLE ADD COLUMN` must not force an eager rewrite of every
existing row — an old row is decoded against the schema version it was
written under; a reader upgrades it to current-schema semantics
(missing trailing columns = their default/`NULL`) at read time, never by a
background rewrite pass in v1.

**Alternatives rejected**:
- *JSON-encode every row.* Explicitly forbidden ("Do NOT JSON serialize
  every row") and would cost materially more CPU/bytes per row than a
  typed binary encoding, and loses type-safe ordering entirely.
- *Column-per-cell physical layout* (one engine key per (row, column)).
  Rejected per Reason above — real write-amplification cost with no
  offsetting correctness or performance benefit at this project's scale;
  reconsider only if a future workload specifically needs wide-sparse-
  column access patterns this layout would help, with real measurement.
- *Global single schema version, rewrite-on-alter.* Rejected: an eager
  full-table rewrite on every `ALTER TABLE` is an unbounded-duration,
  unbounded-resource DDL operation — directly against "avoid an unbounded
  memory/resource path merely because it is easier to code."

**Correctness impact**: per-row schema versioning makes `ALTER TABLE ADD
COLUMN` correct and instantaneous (a catalog-only change); reading an old
row never mis-decodes newer columns it doesn't contain. **Security
impact**: none directly; row bytes are only ever interpreted after
authorization (D25) has already permitted the read. **Performance impact**:
point/PK-access decoding cost is minimized (§ Architecture doc §4) by
never re-storing PK columns in the value. **Memory impact**: bounded per
row by the row-size limit (D27); no growth mechanism beyond ordinary
value storage. **Persistence impact**: format-versioned (`format_version`
byte) for future breaking format changes, without requiring a data
migration to introduce this document's v1 format itself (nothing exists
yet). **Recovery impact**: none beyond ordinary value durability/
corruption-detection, inherited from the certified engine's own checksum
discipline. **Testing requirements**: encode/decode round-trip per type
(D4), `NULL`-bitmap correctness at every bit-boundary, `schema_version`
backward-decoding after a simulated `ALTER TABLE ADD COLUMN`, max-row-size
boundary rejection.

---

## D4. Type system

**Decision**: closed set — `BOOLEAN`, `INTEGER`, `BIGINT`, `REAL`,
`DOUBLE`, `DECIMAL(p,s)`/`NUMERIC(p,s)` (bounded precision, scaled `i128`),
`TEXT`, `BLOB`/`BYTEA`, `DATE`, `TIME`, `TIMESTAMP`, plus `NULL` as a
per-value state, not a type. Full binary representation table in
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §3. Every type has both a
**row-value encoding** (used inside `RowValue`, D3 — may be length-
prefixed, not order-preserving) and, for key-bearing columns (PK or
indexed), an **order-preserving key encoding** (fixed-width, sign/bit-
transformed so byte-lexicographic order equals numeric/logical order).

**Reason**: "no everything-is-a-string" is an explicit requirement.
Order-preserving key encodings are required specifically because the
certified engine's ordering is raw byte comparison (D2's structural
correctness depends on this at the row/index level, not just the table/
index namespace level) — a numeric column's natural sort order must
survive being flattened into engine key bytes, which requires the
sign-flip transform for signed integers/decimals and a bit-level
monotonic transform for IEEE-754 floats (the standard, well-understood
technique: flip the sign bit for positive floats, flip all bits for
negative floats, to make the bit pattern's unsigned-integer order match
IEEE-754's own total order for non-NaN values).

**Alternatives rejected**:
- *Arbitrary-precision `DECIMAL`.* Rejected for v1: a variable-length,
  order-preserving arbitrary-precision numeric encoding is materially
  more complex (and slower) than a fixed-width scaled integer, and no
  workload requirement in this phase demands unbounded precision. Bounded
  `DECIMAL(p,s)` (documented, configurable max `p`) is the honest v1 scope
  cut — analogous to the LSM spec's own precedent of shipping a simpler,
  documented v1 and deferring the harder general case.
- *Timezone-aware `TIMESTAMPTZ`.* Deferred: adds a second timestamp
  representation and a timezone-database dependency for no requirement
  this phase has evidence for.
- *Locale-aware `TEXT` collation.* Rejected for v1: binary (byte-order)
  collation only, stated explicitly as the compatibility target
  (`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §0) rather than silently
  assumed equivalent to a locale-aware default.

**Correctness impact**: order-preserving encodings are the single most
correctness-critical piece of the type system — an incorrect transform
silently produces wrong range-scan/`ORDER BY` results with no engine-level
signal that anything is wrong (the engine sees only bytes). **Security
impact**: literal parsing must reject malformed/oversized literals before
any encoding step (D26 — never trust client-supplied literal text as
already-valid). **Performance impact**: fixed-width numeric key encodings
are O(1) to encode/decode/compare — no parsing cost on the hot
scan/comparison path. **Memory impact**: fixed-width types bound per-value
memory trivially; `TEXT`/`BLOB` bound by the row-size limit (D27).
**Persistence impact**: type encodings are part of the row/key format
version (D3) — a future type addition is additive, never a rewrite of
existing encoded values. **Recovery impact**: none beyond ordinary
corruption detection. **Testing requirements**: per-type round-trip,
exhaustive boundary-value ordering tests (min/max/zero/negative-adjacent-
to-positive for every numeric type), a property test asserting "encode
then byte-compare" matches "numeric/logical compare" for randomly
generated values of every type — this is exactly the kind of invariant
this project's own established style verifies with `proptest` rather than
by inspection alone (`proptest = "=1.11.0"`, already an accepted, pinned
dev-dependency for the engine crate; the new `rubixdb-sql` crate would use
its own equivalent pinned dependency, not the same crate instance, per
D14's dependency-isolation reasoning).

---

## D5. NULL semantics

**Decision**: three-valued logic (`TRUE`/`FALSE`/`UNKNOWN`). `NULL`
compares `UNKNOWN` to everything via `=`/`<`/etc. (including another
`NULL`); only `IS [NOT] NULL` observes it directly. `WHERE`/`HAVING`/`ON`
treat `UNKNOWN` as row-excluding, identically to `FALSE`. Aggregates skip
`NULL` inputs except `COUNT(*)`. `GROUP BY` groups all `NULL`s of a column
together. `ORDER BY` default `NULLS LAST` ascending, `NULLS FIRST`
descending (grammar-overridable). Primary-key columns may never be `NULL`
(enforced at bind time as a structural rule, not merely a runtime
`NOT NULL` constraint check).

**Reason**: this is standard SQL three-valued logic — RubiXDB does not
invent a variant. Stating it explicitly and exhaustively (rather than
leaving it implicit) is required per instruction ("SQL NULL semantics are
unclear" is a listed stop condition) and is what makes the executor's
comparison/aggregate logic independently testable against a reference
model (D30) rather than only against itself.

**Alternatives rejected**: *SQL-92 `NULL`-excludes-from-`GROUP BY`
entirely* (i.e., dropping `NULL`-keyed groups). Rejected: this is not
standard SQL behavior and would surprise anyone with prior SQL experience
without a strong reason to diverge; RubiXDB does not deviate from
mainstream SQL `NULL` handling without a stated reason, and none exists
here.

**Correctness impact**: this is the definition of correctness for every
comparison/aggregate/grouping operator — every executor operator's test
suite must include a `NULL`-input case, not only a happy-path case.
**Security impact**: none directly. **Performance impact**: `NULL`
checks are O(1) bitmap lookups (D3's `null_bitmap`) — no cost beyond a
bit test. **Memory impact**: none beyond the fixed per-row bitmap.
**Persistence impact**: `NULL` is structural (bitmap), never a sentinel
value inside a typed column's own encoding — this avoids ambiguity between
"a real value that happens to look like a sentinel" and "actually NULL,"
a correctness hazard some naive designs introduce. **Recovery impact**:
none. **Testing requirements**: a dedicated `NULL`-semantics test matrix
(every comparison operator × `NULL` on each side, every aggregate ×
all-`NULL`-input, `GROUP BY` `NULL`-grouping, `ORDER BY` `NULL`
positioning both directions) checked against an independent reference
model per D30's own established "never the production algorithm as its
own oracle" principle.

---

## D6. Primary keys

**Decision**: every table has exactly one primary key, defined at
`CREATE TABLE` time, composite-capable (ordered list of ≥1 columns).
The primary key *is* the table's physical row key (D2) — there is no
separate "primary key index" structure distinct from the table's own
storage, matching how a clustered-index-organized table works in other
systems, and matching this project's own preference to avoid inventing a
redundant structure when the row key already serves the purpose.

**Reason**: an LSM engine's native access pattern is already
"look up by key" — making the primary key be the physical key is the
simplest, zero-redundant-storage design, and gives PK point lookups the
certified engine's own `get`/`get_as_of` performance characteristics
directly, with no secondary indirection.

**Alternatives rejected**: *A synthetic internal row ID as the physical
key, with the declared primary key as a mandatory unique secondary
index.* Rejected: doubles physical storage for every row (the row data
plus a redundant PK-index entry) and adds an indirection hop to every
PK-based lookup, for no benefit this design needs (some systems do this
to support cheap primary-key changes; RubiXDB v1 does not support
`UPDATE`ing primary-key column values in place — such an `UPDATE` is
executed as `DELETE`+`INSERT`, an accepted, documented v1 scope cut).

**Correctness impact**: PK uniqueness is enforced by the same D9/D10
write-conflict mechanism as any other write-write conflict — no separate
uniqueness-checking subsystem. **Security impact**: none directly.
**Performance impact**: PK point lookup = `O(1)` *in matching-row count*
(amortized bloom-filtered engine `get`; certified Read Engine
performance characteristics apply directly, unchanged) — actual I/O/CPU
cost also scales with the number of live SSTables consulted, per the
certified Read Engine's own read-amplification model, bounded (not
eliminated) by Compaction — see D28's complexity table, and
`RELATIONAL ADR AMENDMENT 001` AA.15 for the full correction. **Memory impact**:
none beyond ordinary engine key/value residency. **Persistence impact**:
none beyond D3. **Recovery impact**: none beyond ordinary engine recovery.
**Testing requirements**: composite-PK ordering correctness (multi-column
encode/compare), PK-uniqueness-violation-is-a-conflict test (ties to D9/
D10's own test suite), `UPDATE` of a PK column correctly executing as
delete+insert with correct index maintenance.

---

## D7. Indexes

**Decision**: `PRIMARY KEY` (D6, not a separate structure), `UNIQUE`, and
non-unique secondary indexes, each a real persistent ordered structure
built directly on the certified engine's keyspace (D2's index-entry key
layout) — never an in-memory `HashMap`. Every index entry always appends
the row's primary key (tie-break + row pointer), even for `UNIQUE`
indexes. `CREATE INDEX` on a non-empty table performs a bounded,
full-table-scan backfill as part of the same DDL operation.

**Reason**: persistent indexes are explicitly required ("Do not implement
indexes as an in-memory HashMap") — an in-memory index would not survive
restart, violating the most basic durability expectation for an index.
Appending the PK to every index entry (rather than only to non-unique
ones) keeps the index-entry format uniform — one encode/decode path for
every index kind, rather than two.

**Alternatives rejected**: *Per-per-block bloom filters or index-specific
caching layers.* Deferred: the certified SSTable format already provides
a whole-file bloom filter and sparse block index (`RubixDB-LSM-Engine-
Specification-v1.0.md` §2.4-2.5) that apply to index-entry keys exactly as
they do to any other key — no index-specific caching is needed to get a
correct, reasonably fast index lookup in v1; a dedicated index cache is a
future, measurement-justified optimization, not a v1 requirement.
*Online (zero-downtime, dual-write) index backfill.* Deferred, matching
the larger Architecture Spec's own explicit "no online migration in v1"
precedent — a bounded, single-pass backfill is the v1 scope.

**Correctness impact**: index-to-table consistency is guaranteed only
once D9's atomic write primitive exists — this decision is *not*
independently sufficient for correctness; it depends on D9/D10.
**Security impact**: index entries are subject to the same table-level
authorization as the table itself — a caller without `SELECT` on a table
cannot use an index to infer its contents indirectly (enforced at the
binder/planner level: an index is never chosen as an access path for a
query the caller isn't authorized to run in the first place). **Performance
impact**: indexed equality/range lookup = `O(log n)` *in the index's own
matching-entry count*, engine-level bloom+block-index-assisted lookup
plus one row fetch per matching entry (index-then-fetch) — actual I/O/CPU
cost also scales with the number of live SSTables consulted for both the
index's own physical region and each subsequent row fetch, per the
certified Read Engine's own read-amplification model (`RELATIONAL ADR
AMENDMENT 001` AA.15) — see D28. **Memory impact**: identical profile to table
SSTable metadata residency (certified Read Engine's own established,
measured characteristic — `PHASE_READ_ENGINE_CERTIFICATION.md` row 16)
applies to index SSTable regions equally; no new memory model needed.
**Persistence impact**: identical to table rows (D3), inherited.
**Recovery impact**: identical to table rows, inherited — *provided*
index maintenance is atomic with its table write (D9/D10); if it is not,
recovery cannot distinguish "index legitimately behind" from "index
permanently inconsistent," which is exactly why D9 is a hard prerequisite,
not an optional enhancement. **Testing requirements**: index backfill
correctness against a concurrently-written table (must be excluded/
serialized correctly — see D9's concurrency-model testing requirements),
`UNIQUE` violation-is-a-conflict (D6), non-unique duplicate-key ordering
stability, index range-scan correctness against an independent reference
model (D30).

---

## D8. Constraints

**Decision**: `PRIMARY KEY` (D6), `UNIQUE` (D7), `NOT NULL`, `CHECK`
(a boolean expression over the row's own columns, evaluated at bind time
against the same expression engine the executor uses, never a second,
separate expression evaluator). All enforced **server-side, in the
binder/executor**, never delegated to a client. `NOT NULL`/`CHECK` are
purely row-local (evaluated against the row being written, no cross-row
read required); `PRIMARY KEY`/`UNIQUE` are cross-row and resolved via
D9/D10's write-conflict mechanism.

**Reason**: "Never rely on frontend/CLI/API client to maintain database
integrity" is explicit and non-negotiable. Reusing the executor's own
expression engine for `CHECK` (rather than a second evaluator) avoids
maintaining two implementations of the same three-valued-logic semantics
(D5) that could silently drift apart.

**Alternatives rejected**: *Client-side validation as the primary
enforcement, server-side as an optional double-check.* Rejected outright
by explicit instruction — server-side enforcement is the *only*
authoritative enforcement; a client-side check (if the frontend adds one
for UX responsiveness) is advisory only and never assumed sufficient.

**Correctness impact**: a constraint violation must abort the entire
statement/transaction (D9's atomic write-set discipline) — a partially
applied write that violates a constraint it was supposed to be checked
against before commit is a correctness defect. **Security impact**: a
`CHECK` expression is bound and typed by the same binder as any query
expression — it cannot execute arbitrary code or access data outside the
row it's evaluating (no subqueries in `CHECK`, a stated v1 grammar
restriction). **Performance impact**: `NOT NULL`/`CHECK` cost is O(column
count) per row, paid once per write, no scan required. **Memory impact**:
none beyond ordinary per-row expression evaluation, already bounded by row
size (D27). **Persistence impact**: constraint definitions are catalog
rows (D1), versioned the same way table schema is (D3's `schema_version`
applies to constraint sets too — an `ALTER TABLE ADD CONSTRAINT` bumps the
version; old rows are not retroactively re-validated in v1, a stated scope
cut, matching the same reasoning as D3's schema-evolution approach).
**Recovery impact**: none beyond ordinary catalog-row recovery (D1).
**Testing requirements**: constraint-violation-aborts-the-whole-statement
(not just the violating row, for a multi-row statement), `CHECK`
expression against every comparison/NULL edge case (D5), constraint
addition against a pre-existing table with violating data (must be
rejected explicitly, not silently accepted — the DDL itself should fail
if existing data violates a new constraint, requiring a validation scan as
part of `ALTER TABLE ADD CONSTRAINT`).

---

## D9. Storage extension required: atomic multi-key `write_batch`

**Decision**: propose exactly one new `LsmEngine` public method:

```rust
pub fn write_batch(&self, ops: &[WriteOp]) -> Result<u64>
// WriteOp::Put { key: Vec<u8>, value: Vec<u8> } | WriteOp::Delete { key: Vec<u8> }
```

Semantics: one WAL frame durably encodes all N ops before any is applied
(extending, not replacing, the existing per-op WAL frame format — an
additive new op-group frame type, analogous to how `SET_CHECKPOINT` was
added alongside `PUT`/`DELETE` without disturbing them); after that single
fsync, all N ops are applied to the active MemTable inside one critical
section under the existing `active` write lock (the same lock
`apply_after_durable` already takes per-op today, held for the whole batch
instead of once per op); the call returns once all N ops have a durable,
assigned, and applied seq. Either every op in the batch is visible to a
subsequent read, or none are — no partial-batch state is ever observable
by any reader, including one racing the batch under the certified
`ReadView` pattern.

**Reason**: `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md` §2.1 established
this is the one genuine gap — no existing primitive provides it, and every
higher-level relational guarantee (D10 transactions, D11 index/table
consistency, D13 DDL durability) is unimplementable without it, per the
governing stop condition ("If current LsmEngine primitives cannot
guarantee this: STOP. Do NOT fake atomicity."). This decision **is** the
stop, resolved architecturally: the minimum extension is named, scoped,
and justified here; it is explicitly **not implemented in this phase**
(Phase 0/1 is architecture-only) and requires its own dedicated
implementation-time ADR (`PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`,
Phase 9 of the build order) before a single line of engine code changes.

**Alternatives rejected**:
- *Application-level "fake" atomicity via retry/reconciliation* (write
  each op independently, detect and repair partial application after the
  fact via a reconciliation pass). Rejected outright, explicitly forbidden
  ("Do NOT fake atomicity... Never solve these by hardcoding, faking,
  silently weakening guarantees"). A reconciliation pass cannot
  distinguish "crashed mid-batch" from "a concurrent, legitimate partial
  state" without the same atomicity primitive it's trying to avoid
  building.
- *Two-phase commit across independent single-key writes* (write a
  "pending" marker key, then each op, then a "committed" marker,
  interpreting a missing committed marker at recovery as "roll back the
  pending ops"). Rejected: this reinvents, in user-space, exactly the
  atomic-group-commit machinery a proper engine-level batch primitive
  provides directly and correctly, at higher latency (multiple WAL
  round-trips instead of one) and higher implementation risk (a hand-
  rolled 2PC-over-KV protocol has a much larger bug surface than one new,
  narrowly-scoped, engine-level method reusing the existing WAL/MemTable
  machinery).
- *A distinct WAL/MemTable instance per "transaction domain."* Rejected:
  massively more invasive than necessary, and breaks the single global
  seq-ordering guarantee (`RubixDB-Architecture-Specification-v1.0.md`
  §7.1) every other consistency guarantee in this design depends on.

**Correctness impact**: this is the single primitive every atomicity claim
in this ADR (D10, D11, D13) rests on — it must be proven correct (crash-
window analysis, concurrent-reader-never-sees-partial-batch property test)
before any relational feature depending on it may be implemented.
**Security impact**: no new attack surface at the engine level (same
caller trust model as `put`/`delete` today — an internal API, not
externally reachable without going through the relational layer's own
authorization, D25). **Performance impact**: for the common single-op case
(most `INSERT`/`UPDATE`/`DELETE` touching no secondary index),
`write_batch` with N=1 must perform no worse than today's `put`/`delete` —
this is a testable, required performance-parity property, not an assumed
one. For N>1, one WAL fsync replaces N, which is a **throughput
improvement** over any hypothetical N-independent-calls baseline, not
just a correctness fix — group-commit-style amortization applies here
exactly as it already does for concurrent single-key writers
(`BatchCoordinatorPool`). **Memory impact**: bounded by the caller-supplied
batch size — the relational layer (D26/D27) is responsible for capping
transaction write-set size before calling `write_batch`; the primitive
itself should also reject a batch exceeding a fixed hard ceiling
(defense in depth, not solely relying on the caller). **Persistence
impact**: one new WAL frame *kind* (op-group), additive to the existing
format — the existing `PUT`/`DELETE`/`CHECKPOINT_MARKER` frames and every
existing recovery test remain valid and unchanged; a WAL reader that
doesn't yet understand op-group frames must fail closed (Corruption), not
silently skip them, matching the existing "any invalid frame that is not
the trailing torn one is corruption, escalate" discipline. **Recovery
impact**: WAL replay of an op-group frame must apply all N ops or (if the
frame itself is the torn trailing one) none — this is a direct,
mechanical extension of the existing torn-tail-truncation rule
(`RubixDB-LSM-Engine-Specification-v1.0.md` §7.1 step 4), not a new
recovery concept. **Testing requirements** (binding on the future
implementation ADR, stated here so it cannot be quietly narrowed later):
crash-injection at every byte offset within a multi-op WAL frame write (a
torn op-group frame is fully discarded, never partially replayed);
concurrent-reader-during-in-flight-batch property test (a reader's
`ReadView`, captured mid-batch-apply, must never observe M<N of the
batch's ops); N=1 performance-parity benchmark against today's `put`;
a differential test comparing `write_batch` against a reference model
that applies the same ops as N independent, artificially-serialized
`put`/`delete` calls, asserting identical final logical state.

---

## D10. Transaction model (atomicity + isolation)

**Decision**: Snapshot Isolation, client/session-buffered write-set,
commit-time optimistic conflict check, built entirely on D9 plus the
certified `Snapshot` primitive — no new engine primitive beyond D9.
`BEGIN` → `snapshot()`; reads resolve against that snapshot's seq;
writes buffer in-memory (never touch the engine until commit); `COMMIT`
re-reads the *current* value of every physical key (table row + every
affected index entry) the write-set touches, compares against what the
transaction's snapshot saw, aborts with a conflict error on any mismatch,
otherwise applies the entire write-set via one `write_batch` call.
`ROLLBACK` discards the buffered write-set (no engine call at all).

**Reason**: fully elaborated in `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`
§9. Optimistic, snapshot-based concurrency fits this engine's existing
MVCC read model directly (no new read-side mechanism needed) and avoids
the "avoid global locks unless absolutely required" anti-pattern a
pessimistic lock-manager design would risk.

**Alternatives rejected**:
- *Pessimistic row/table locking.* Rejected: requires a new lock-manager
  subsystem (deadlock detection, lock-wait queues) with no existing
  analog in the certified engine, and directly conflicts with "avoid
  global locks unless absolutely required... do not serialize every SQL
  transaction through one bottleneck." Optimistic SI, needing only D9,
  is strictly less new machinery for a comparable (if not stronger, for
  low-contention workloads) practical guarantee.
- *Full Serializability (SSI or 2PL).* Rejected for v1: requires either
  read-set tracking with anti-dependency detection (SSI) or full locking
  (2PL/S2PL) — both materially more complex than SI, and no workload
  requirement in this phase justifies the cost. The write-skew anomaly SI
  admits is a stated, honest limitation (`PHASE_RELATIONAL_DATABASE_
  ARCHITECTURE.md` §9), not hidden.
- *Read-committed-only (no multi-statement transaction snapshot at all).*
  Rejected: weaker than what the certified engine's own `Snapshot`
  primitive already makes nearly free to provide — using it is strictly
  better for the same implementation cost.

**Correctness impact**: this is the central correctness claim of the
whole relational layer — atomicity is only as strong as D9's own proof;
isolation is exactly Snapshot Isolation, stated with its real limitation
(write skew), never oversold as Serializable. **Security impact**: a
transaction's write-set is held in server-side process memory
(session-scoped), never trusts a client-supplied "this is atomic, trust
me" signal — the server is the sole authority on commit success/failure.
**Performance impact**: `BEGIN` cost = one `snapshot()` call (already
measured, cheap — certified `SnapshotRegistry`, `src/lsm/mod.rs:266-342`);
read cost = ordinary `get_as_of`/`range_scan` at a fixed seq, unchanged;
write cost = zero engine calls until commit (pure in-memory buffering);
commit cost = one freshness-check read per touched key plus one
`write_batch` call — dominant cost scales with write-set size, not
transaction duration, which is the desired property (a long-running
read-only or read-heavy transaction imposes no commit-time cost
proportional to how long it stayed open). **Memory impact**: bounded by
the write-set size limit (D27) — a transaction's buffered write-set is
the only new per-transaction memory cost, capped explicitly, never
unbounded. **Persistence impact**: nothing durable happens before commit
— a crash mid-transaction (before `COMMIT`) loses exactly the buffered,
never-yet-durable write-set, which is the correct, expected behavior (no
different from any client losing an unsent request). **Recovery impact**:
depends entirely on D9's own recovery correctness — no separate
transaction-log recovery mechanism exists beyond D9's op-group WAL frame
replay. **Testing requirements**: concurrent-transaction conflict
detection (two transactions writing the same key, second committer
aborts), write-skew reproduction (documenting the known limitation with a
concrete failing case, not merely asserting it exists), long-open-
transaction-does-not-block-unrelated-commits (a direct test of "avoid
global locks"), crash-during-commit (must land as D9's atomic
all-or-nothing, never a partially-applied write-set).

---

## D11. Index/table consistency

**Decision**: every DML statement that inserts, updates, or deletes a row
computes the *complete* set of physical key changes required — the table
row itself, plus one entry-add/entry-remove pair per affected index (an
`UPDATE` that changes an indexed column's value removes the old index
entry and adds the new one; one that only changes non-indexed columns
touches no index) — and applies that complete set as a single D9
`write_batch` (directly, for autocommit-mode single-statement writes; via
D10's transaction write-set, for a multi-statement transaction).

**Reason**: this is the direct mechanical consequence of D9's existence —
once atomic multi-key writes are possible, "table and index disagree" is
structurally impossible except during the write itself (which is exactly
what D9's all-or-nothing visibility guarantee rules out for any reader).
Without D9, this decision would be unimplementable, which is why D9 is a
hard prerequisite, not a parallel, independent decision.

**Alternatives rejected**: *Asynchronous/eventually-consistent index
maintenance* (write the table row synchronously, update indexes via a
background queue). Rejected outright: this reintroduces exactly the
"committed table state disagrees with its indexes" failure mode the
governing stop condition forbids, in a form that's *harder* to reason
about than a naive single-key-at-a-time approach because the inconsistency
window is unbounded and workload-dependent rather than a well-defined
crash window.

**Correctness impact**: this decision's entire correctness rests on D9;
it is stated as its own decision because "compute the complete key-change
set correctly" (accounting for every index, including partial/filtered
indexes if ever added later, and composite-index column coverage) is its
own non-trivial planning-layer responsibility, distinct from D9's
lower-level atomicity guarantee. **Security impact**: none beyond D9/D25.
**Performance impact**: write cost scales with the number of indexes
touched (`O(1 + affected_index_count)` physical key operations per
logical row write) — see D28's complexity table; this is the expected,
standard cost of maintaining secondary indexes in any database, not a
RubiXDB-specific inefficiency. **Memory impact**: bounded by row size ×
(1 + index count) per statement, folded into D27's write-set size limit.
**Persistence impact**: none beyond D9. **Recovery impact**: none beyond
D9 — recovery never needs index-specific repair logic, because D9 already
guarantees the table-plus-indexes write landed as one atomic unit or not
at all. **Testing requirements**: every DML statement shape (`INSERT`/
`UPDATE` touching 0/1/N indexed columns/`DELETE`) against a table with
multiple secondary indexes, verified via independent reference-model
comparison (D30) after every operation, including across a crash-and-
recover cycle (this is the single most important correctness property in
the entire design and warrants the most exhaustive test coverage of any
decision in this document).

---

## D12. Crash recovery (relational-layer-specific considerations)

**Decision**: no new relational-layer recovery procedure. Catalog, table,
and index rows recover via the certified engine's existing procedure
(`RubixDB-LSM-Engine-Specification-v1.0.md` §7) unchanged, because they
are ordinary engine keys (D1, D2). The only recovery logic this phase adds
is D9's own op-group WAL frame replay rule (torn-trailing-frame discard,
otherwise all-or-nothing replay) — a mechanical extension of the existing
WAL replay algorithm, not a parallel one.

**Reason**: this is the direct payoff of D1's "catalog-as-keyspace"
decision and D9's design as a WAL-frame-level extension rather than a
bolt-on separate log: there is exactly one recovery procedure in the whole
system, and every phase's own certification has already independently
verified it (`PHASE_WRITE_ENGINE_CERTIFICATION.md`'s RECOVERY gate,
`PHASE_COMPACTION_CERTIFICATION.md` rows 10-13). Building a second,
relational-specific recovery path would both duplicate proven machinery
and create two places recovery logic could disagree.

**Alternatives rejected**: *A relational-layer replay/reconciliation pass
that runs after engine recovery completes, to "fix up" catalog/index
state.* Rejected: implies the base recovery isn't actually correct on its
own, which would mean D9 wasn't proven correct in the first place — a
reconciliation pass is a symptom-treatment workaround, not a fix, for
exactly the kind of ambiguity the stop condition forbids solving by
"hiding errors."

**Correctness impact**: depends entirely on D9's own recovery proof (see
D9's Testing requirements) — this decision adds no independent
correctness claim, which is itself the point (one procedure, one proof).
**Security impact**: none. **Performance impact**: recovery time scales
with WAL replay volume exactly as it does today — relational writes add
op-group frames of proportionally similar size to the equivalent set of
single-key frames, no recovery-time regression expected (to be confirmed
empirically once implemented, not assumed). **Memory impact**: none
beyond the existing recovery procedure's own bounded, already-measured
memory profile. **Persistence impact**: none beyond D9. **Recovery
impact**: this decision *is* the recovery impact statement for the whole
relational layer — restated here for completeness per the directive's
explicit "DDL durability... crash recovery" requirement. **Testing
requirements**: full relational-workload crash-injection matrix (crash
during `INSERT`/`UPDATE`/`DELETE`/`COMMIT`/`ROLLBACK`/`CREATE TABLE`/
`DROP TABLE`/`CREATE INDEX`/`DROP INDEX`/catalog update — Phase 32's own
explicit list), each verified by post-restart catalog+row+index+
transaction-state re-verification against an independent model, mirroring
exactly the methodology `PHASE_COMPACTION_INCREMENT3_ENDURANCE.md` already
established for the storage engine's own crash testing.

---

## D13. DDL durability

**Decision**: every DDL statement (`CREATE`/`ALTER`/`DROP DATABASE`/
`SCHEMA`/`TABLE`/`INDEX`) is executed as one D9 `write_batch` covering
every catalog row it touches (e.g., `CREATE TABLE` = one `system.tables`
row + N `system.columns` rows + one default-PK-index `system.indexes`
row, all in one batch). `DROP TABLE` additionally requires deleting every
physical table row and index entry — for a non-trivial table this cannot
fit in one bounded `write_batch`, so `DROP TABLE` is defined as: (1) one
atomic catalog update marking the table `DROPPING` (durable, D9), which
immediately makes the table invisible to new queries (binder rejects it),
followed by (2) a bounded, resumable-on-crash background sweep physically
removing its rows/index entries, followed by (3) a final atomic catalog
update removing the `system.tables` row once the sweep completes. This
mirrors the certified engine's own precedent for exactly this shape of
problem (Compaction's own "physical deletion safety... deferred not
blocking," `ADR-COMPACTION-001` Decision 9) rather than inventing a new
pattern.

**Reason**: DDL correctness has the identical atomicity requirement as DML
(D11) — a `CREATE TABLE` that durably records column 1 and 2 but not
column 3 after a crash is exactly the same class of defect as an `INSERT`
whose index entry didn't land. `DROP TABLE`'s two-phase design is required
because an unbounded single `write_batch` for a potentially huge table
would violate the "avoid an unbounded memory path" principle (D9's batch
size ceiling, D27) — the catalog-visibility flip must still be atomic and
instant; only the physical reclamation may be gradual.

**Alternatives rejected**: *Synchronous `DROP TABLE` that must complete
the entire physical sweep before returning.* Rejected: makes `DROP TABLE`'s
latency and memory cost proportional to table size, unboundedly, exactly
the pattern this project's own compaction design already rejected for the
same reason.

**Correctness impact**: catalog visibility changes atomically and
immediately (readers never observe a half-created or half-dropped table);
physical reclamation lag after a `DROP TABLE` is a storage-reclamation
concern, not a correctness one (a `DROPPING` table is never queryable).
**Security impact**: a `DROPPING` table must be excluded from
authorization-checked catalog listings (D25) the instant its catalog row
flips, not after physical cleanup completes. **Performance impact**:
`CREATE`/simple `ALTER`/`DROP INDEX`/small-table `DROP TABLE` are O(catalog
row count touched) — cheap, bounded, single-batch. Large-table `DROP
TABLE`'s background sweep cost is proportional to table+index size,
identical in shape to Compaction's own already-measured "bounded by
live input count, not cumulative volume" resource profile. **Memory
impact**: the background sweep must be a bounded, streaming operation
(iterate-and-batch-delete in fixed-size chunks), never materializing the
full to-be-deleted key set in memory — a direct requirement, not
optional. **Persistence impact**: the `DROPPING` marker itself must be
durable before the sweep begins (D9), so a crash mid-sweep resumes
correctly (recovery finds a `DROPPING` table, restarts the sweep from
scratch — idempotent, since deleting an already-deleted key is a no-op).
**Recovery impact**: recovery must recognize a `DROPPING`-state table and
resume its sweep automatically (not require operator intervention) —
directly analogous to Compaction's own orphan-sweep recovery pattern.
**Testing requirements**: crash during each DDL statement type (Phase 32's
list), crash mid-`DROP TABLE`-sweep (resume-and-complete correctness),
concurrent query against a table mid-`DROPPING` (must see "table does not
exist," never a partial view).

---

## D14. SQL parser, AST, and its dependency security

**Decision**: `sqlparser-rs` (crate `sqlparser`), pinned to an exact
version (`=` pin, matching this project's own established pinning
discipline for `crc32c`/`xxhash-rust`/`proptest`), used directly — its own
`ast::Statement`/`ast::Expr` tree is the AST, not re-modeled. Lives in a
new `rubixdb-sql` workspace crate, never a dependency of the core
`rubixdb` engine crate.

**Reason**: "Use a proper parser... Do NOT use regex parsing" is explicit.
`sqlparser-rs` is the de facto standard mature choice in the Rust
ecosystem (used by DataFusion and others), meaning its grammar coverage
and edge-case correctness have real-world exercise this project would not
get from a from-scratch grammar at comparable effort. Isolating it to a
new crate preserves the core engine's own certified "zero new dependency"
property exactly as `api/`'s own `axum`/`tokio` dependencies already do
(`PHASE_API_ARCHITECTURE.md` §1.1) — this is a directly reused pattern,
not a new one.

**Alternatives rejected**: *Hand-rolled recursive-descent parser.*
Rejected: explicitly disfavored relative to a proper parser for a full SQL
grammar's complexity, and would need its own large, ongoing correctness/
security investment (a hand-rolled parser is a more, not less, likely
source of a parsing-based vulnerability than a widely-used maintained
one). *Regex-based parsing.* Explicitly forbidden. *A different mature
parser crate (e.g. `pg_query`, a bindings-to-libpg_query crate).*
Rejected for v1: `pg_query` bindings wrap a C library (`libpg_query`),
introducing a non-Rust, `unsafe`-FFI dependency — `sqlparser-rs`'s
pure-Rust, no-`unsafe` (to be verified, not assumed — see Testing
requirements) profile is a better fit for this project's own established
"zero `unsafe` introduced" discipline (`PHASE_READ_ENGINE_CERTIFICATION.md`
row 29, `PHASE_COMPACTION_CERTIFICATION.md`'s equivalent audit).

**Correctness impact**: parser correctness for the supported grammar
subset is inherited from an externally-maintained, widely-exercised
project rather than freshly written — real but bounded risk (a parser bug
is still possible; not eliminated, reduced). **Security impact**: the
parser is the first boundary untrusted SQL text crosses — it must never
panic on malformed input (a parser panic is a denial-of-service vector);
this is a testing requirement (fuzzing), not an assumption. Grammar the
parser accepts that the binder doesn't yet support must be rejected by the
binder with a clear "unsupported" error, never silently misinterpreted
(directly enforces "a feature is NOT complete merely because the parser
accepts it"). **Performance impact**: parsing cost is small and
per-statement, not on any per-row hot path — no special optimization
needed. **Memory impact**: bounded by the max-SQL-statement-size limit
(D27), enforced *before* the parser runs (reject oversized input at the
transport/API layer first, D22). **Persistence impact**: none — the
parser produces an ephemeral AST, never persisted. **Recovery impact**:
none. **Testing requirements**: exact version pin recorded and audited
(license, `unsafe` count via `cargo geiger` or equivalent, matching the
storage engine's own `git diff ... | grep -c "^\+.*unsafe"` audit
technique) before adoption is finalized; a fuzz-testing pass (malformed/
adversarial SQL text must never panic, only ever return a parse error);
an explicit, versioned "supported grammar subset" document, updated per
increment as binder/executor coverage grows — never silently assumed
complete.

---

## D15. Binder

**Decision**: resolves every AST identifier against the catalog (D1)
through the authorization layer (D25) in the same pass — an object the
caller is not authorized to see is treated identically to an object that
does not exist (never distinguishing "exists but forbidden" from "doesn't
exist" in the error message, D26's information-disclosure discipline
applied to schema information specifically). Produces a fully-typed,
fully-resolved logical tree; the planner never re-resolves an identifier.

**Reason**: binding and authorization must happen together, not as two
separate passes, specifically to avoid a class of bug where a later pass
forgets to re-check authorization for something an earlier pass already
resolved — collapsing them into one pass makes "forgot to check
authorization for this identifier" structurally harder to write.

**Alternatives rejected**: *Bind first, authorize as a separate later
pass over the bound tree.* Rejected: a two-pass design has a real history
of authorization-bypass bugs in other systems (an optimizer rewrite or a
later pass introducing a new object reference that skips the earlier
authorization pass) — collapsing to one pass removes this entire bug
class by construction, not merely by discipline.

**Correctness impact**: "unknown object/column/ambiguous column/invalid
function/invalid type/invalid scope" (Phase 13's explicit list) must each
produce a distinct, correct, safe-to-display error — conflating them (e.g.
reporting "unknown column" for an actually-ambiguous one) is a usability
defect worth testing against directly. **Security impact**: this is a
primary SQL-injection-adjacent defense layer — the binder is the point
where "this identifier refers to exactly this catalog object, of this
exact type, which this principal is authorized to touch" is established
once, authoritatively, before any execution. **Performance impact**:
one catalog lookup per identifier per statement (D28) — negligible
relative to query execution cost for any non-trivial statement.
**Memory impact**: bounded by AST/statement size (D27). **Persistence
impact**: none — binding is ephemeral, per-statement. **Recovery impact**:
none. **Testing requirements**: every error class in Phase 13's list, each
with a dedicated test; an authorization-bypass attempt via every distinct
identifier-resolution path (unqualified name, schema-qualified name,
alias, wildcard `SELECT *` expansion) each independently verified to
respect D25.

---

## D16. Query planner and optimizer

**Decision**: rule-based only (predicate pushdown, projection pruning,
primary-key-lookup detection, index selection by deterministic
leading-column match, limit pushdown — full list in
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §11). No cost-based
optimization, no invented cost constants, until real statistics exist
(D20).

**Reason**: directly reuses the storage engine's own established
discipline — "calibrate from measurement," not guess (compaction strategy
precedent, `RubixDB-LSM-Engine-Specification-v1.0.md` §5.1; cost-model
precedent, `RubixDB-Architecture-Specification-v1.0.md` §11.2's own
explicit "a cost model calibrated against invented numbers is worse than
no cost model"). A rule-based optimizer with deterministic, structurally-
justified rules (an index exists on this exact predicate's column → use
it) needs no invented numbers to be correct and beneficial.

**Alternatives rejected**: *A cost-based optimizer seeded with
hand-guessed constants.* Explicitly forbidden ("Do not create a fake
cost optimizer with arbitrary constants"). *No optimization at all
(always full `SeqScan` + late filter).* Rejected: leaves an obvious,
correctness-neutral performance win (using an existing index for an
equality predicate) unrealized for no reason — the directive explicitly
also forbids "obviously inefficient structures... postpone all
performance thinking until the end."

**Correctness impact**: every rule must be provably result-preserving
(predicate pushdown must not change which rows match; index selection
must not change which rows a query returns) — each rule needs its own
differential test against the unoptimized plan's output, not just an
optimized-plan-only test. **Security impact**: index selection must
respect D25 (never choose an index the caller isn't authorized to have
its existence implicitly revealed by, e.g., via a timing side-channel —
stated as a known, accepted residual risk common to essentially all
database systems, not specifically mitigated in v1, and explicitly noted
as such rather than silently ignored). **Performance impact**: this is
the entire point of the decision — see D28's per-operation complexity
table for the concrete before/after shape each rule targets. **Memory
impact**: the optimizer itself operates on the (small, bounded-by-
statement-size) logical plan tree, not on data — negligible. **Persistence
impact**: none — plans are ephemeral, per-statement (no persisted plan
cache in v1, a stated scope cut; explain-and-replan on every execution is
the v1 behavior). **Recovery impact**: none. **Testing requirements**: a
differential test per rule (optimized vs. unoptimized plan, same logical
result, over the D30 reference-model harness), `EXPLAIN` output
correctness (D16 ties directly to Phase 28's `EXPLAIN` requirement — the
chosen access path must be observable, not just internally correct).

---

## D17. Executor and streaming-execution memory strategy

**Decision**: Volcano-style pull-based iterator executor. `SeqScan`/
`IndexScan` wrap `LsmEngine::range`/`range_scan` directly (already
lazy, bounded-memory, per the certified Read Engine). `Filter`/
`Projection` are zero-materialization per-row transforms.
`Sort`/`GroupBy`/`Aggregate`/hash-based `Join` inputs are the only
operators requiring materialization; each is bounded by an explicit,
configurable per-operator memory cap (D27) and **fails the query with a
typed resource-limit error when exceeded** — no disk-spill in v1 (stated
plainly as a scope cut, not hidden), no silent truncation, no unbounded
growth.

**Reason**: matches the certified Read Engine's own established design
philosophy (`ADR-RE-002`'s persistent-cursor, bounded-memory range-scan
design) rather than introducing a materialize-everything pattern the
storage layer deliberately avoided. "Bounded, reject when exceeded" is
chosen over "disk-spill" per the same "don't build unjustified complexity
ahead of evidence" reasoning D16 uses — disk-spill is real, non-trivial
engineering (temp-file management, external sort/hash algorithms, their
own crash-safety story) that no workload in this phase has demonstrated a
need for yet.

**Alternatives rejected**: *Materialize entire input relations before
processing (e.g., a `Sort` that first collects the whole input into a
`Vec`).* Explicitly forbidden ("Do not materialize entire tables
unnecessarily"). *Silent truncation of an oversized sort/group input.*
Rejected: silently returning a wrong (partial) answer is far worse than a
clear, typed error — "never allow an unbounded memory path merely because
it is easier to code" pairs with "never silently narrow a result either."
*Disk-spill from day one.* Deferred, not rejected outright — named
explicitly as the future path once real measurement shows the bounded cap
is actually hit in realistic workloads (mirrors `RubixDB-LSM-Engine-
Specification-v1.0.md` §2.3's own deferred-prefix-compression precedent
exactly).

**Correctness impact**: a rejected oversized operation is a correct,
honest outcome (an error, not a wrong answer) — must be tested to
confirm the error fires at exactly the configured boundary, neither early
nor late. **Security impact**: the memory cap is also a resource-
exhaustion/DoS defense — a maliciously large `GROUP BY` or `ORDER BY`
cannot be used to exhaust server memory, it is bounded and rejected
(D26/D27). **Performance impact**: streaming scan/filter/project
operators inherit the certified Read Engine's own measured performance
characteristics directly (`PHASE_READ_ENGINE_CERTIFICATION.md`
Performance Evidence) — no new performance model needed for them;
materializing operators' cost is the subject of D28's complexity table.
**Memory impact**: this decision *is* the memory-impact statement for the
whole executor — every operator's memory bound must be stated in its own
implementation-time design, none left implicit. **Persistence impact**:
none — execution is ephemeral. **Recovery impact**: none — a query in
flight during a crash simply fails; no query-level recovery exists or is
needed (queries are not durable objects). **Testing requirements**: a
memory-bound-boundary test per materializing operator (N-1 rows within
budget succeeds, N rows exceeding it fails cleanly), a streaming-operator
memory-flatness test (RSS does not grow with input size for `SeqScan`/
`Filter`/`Projection`, mirroring the certified Read Engine's own resource-
flatness testing methodology).

---

## D18. Join algorithms

**Decision**: v1 = `INNER`/`LEFT JOIN` only, via Nested Loop Join (correct
baseline for any shape) with automatic substitution of Index Nested Loop
Join whenever a usable index exists on the inner relation's join key (a
mechanical, non-cost-based rule, per D16). Hash Join, Merge Join,
`RIGHT`/`FULL`/`CROSS JOIN` explicitly deferred.

**Reason**: "benchmark candidates... do not select an algorithm blindly"
cannot be honestly satisfied before an executor exists to benchmark
against — committing to Hash/Merge Join now, with no real data, would
itself be exactly the "select blindly" anti-pattern being avoided.
Starting with a correct, simple baseline (matching this project's
consistent "correct and simple first, optimize once measured" pattern
throughout the storage engine's own build order) is the honest path.

**Alternatives rejected**: *Committing to Hash Join as the v1 default*
(a common "obviously better" choice in the abstract). Rejected precisely
because "obviously better in the abstract" without this project's own
benchmark evidence is the exact anti-pattern Phase 19 forbids —
Nested/Index-Nested-Loop is not claimed superior, only that it is the
correct, simplest starting point pending real measurement.

**Correctness impact**: `LEFT JOIN`'s null-extension semantics (D5's
`NULL` handling applies to unmatched left rows) must be tested
independently of `INNER JOIN`'s matching logic. **Security impact**: none
beyond D25 applying to both joined relations independently (a join cannot
be used to read a table the caller lacks `SELECT` on via the other side of
the join — enforced at bind time, D15). **Performance impact**: Nested
Loop is `O(|outer| × |inner|)` worst case, Index Nested Loop is
`O(|outer| × log|inner|)` when the index rule fires — both stated
explicitly in D28's complexity table, with the expected-poor-scaling case
(large unindexed join) named honestly rather than hidden. **Memory
impact**: Nested Loop family requires no materialization of either side
beyond the current outer row (streaming, D17) — this is in fact a memory
*advantage* of starting with Nested Loop over Hash Join (which requires
materializing the build side), consistent with D17's bounded-memory
discipline. **Persistence impact**: none. **Recovery impact**: none.
**Testing requirements**: `INNER`/`LEFT` correctness against an
independent reference model (D30) across matched/unmatched/`NULL`-bearing
join-key cases; a differential test confirming Index Nested Loop and plain
Nested Loop produce identical results for the same query (the
optimization must be provably transparent); a benchmark harness comparing
both algorithms once implemented — explicitly named as the evidence a
future Hash Join decision would need, not assumed favorable in advance.

---

## D19. Aggregation and sorting

**Decision**: `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` with D5's `NULL` handling;
`GROUP BY`/`HAVING`/`DISTINCT`/`ORDER BY` per D17's bounded-memory
materialization strategy. `Sort` and hash-based `GroupBy`/`Aggregate` are
the same class of materializing operator D17 already governs — no
separate decision needed for their memory strategy, restated here only to
satisfy the directive's explicit naming of "sorting" and "statistics" as
distinct topics requiring resolution.

**Reason**: aggregation/sorting semantics are standard SQL (D5 already
resolves the hard part — `NULL` behavior); the memory strategy question
is already resolved by D17 and would be redundant to re-derive
differently here.

**Alternatives rejected**: none beyond what D5/D17 already considered —
this decision exists to explicitly name the topic, not to introduce a new
design choice.

**Correctness impact**: numeric precision for `SUM`/`AVG` over `DECIMAL`
columns must not silently lose precision relative to the declared
`DECIMAL(p,s)` (an explicit test requirement, not an assumption) —
`AVG`'s internal accumulation may need wider intermediate precision than
the column's own declared scale to avoid rounding-error accumulation
across many rows, a concrete, testable numeric-correctness property.
**Security/Performance/Memory/Persistence/Recovery impact**: identical to
D17's statements, by construction (this decision does not introduce a
new operator class). **Testing requirements**: `SUM`/`AVG` precision
tests over large row counts (rounding-error accumulation), empty-group/
empty-input edge cases for every aggregate (D3/D5), `HAVING` post-
aggregation filtering correctness, `DISTINCT` over `NULL`-bearing columns
(one `NULL` group, matching `GROUP BY`'s own rule).

---

## D20. Statistics

**Decision**: none collected in v1 beyond what D16's deterministic rules
require (none — every v1 rule is structural, not selectivity-based).

**Reason**: stated fully in `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`
§14 — introducing statistics collection with no consumer (no cost-based
rule exists yet to use them) would be exactly the kind of unjustified,
ahead-of-need complexity this project's own discipline avoids throughout.

**Alternatives rejected**: *Collect statistics now, "for later."*
Rejected: statistics infrastructure with no current consumer is untested
in the way that matters (is it actually useful for a real costing
decision?) and adds maintenance surface (every write would need to update
stats) for zero present benefit.

**Correctness impact**: none (no code exists). **Security impact**: none.
**Performance impact**: none — this decision defers a performance
investment, it doesn't make one. **Memory/Persistence/Recovery impact**:
none. **Testing requirements**: none for v1; this decision's own
"requirements" are for the *future* increment that introduces statistics:
it must be introduced only alongside a specific, named cost-based rule
that consumes it, with real benchmark evidence motivating the rule,
exactly mirroring D16's own reasoning.

---

## D21. CLI architecture

**Decision**: `rubixdb>` interactive CLI, backslash meta-commands
(`\l`/`\dn`/`\dt`/`\d`/`\di`/`\du`/`\conninfo`/`\c`/`\help`/`\q`) plus real
SQL, script mode (`-c`/`-f`). The CLI is a thin HTTP client of `POST
/v1/sql` (D22) — never an embedded-engine second execution path.

**Reason**: fully justified in `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`
§15 — the explicit "same executor for CLI/API/frontend" instruction is
best satisfied by there being exactly one executor process (inside
`rubixdb-api`) and three thin clients of its one entry point, not by
attempting to keep two independent executor code paths in sync by
discipline alone.

**Alternatives rejected**: *Embedded-engine local CLI mode* (CLI links
`rubixdb`/`rubixdb-sql` directly, bypassing the API for a single-process,
no-server-required experience). Rejected for v1: creates a second
execution path with its own authorization model (or, worse, none), a
second place bugs in the executor could exist without being caught by the
API's own test suite, and directly risks the CLI/API divergence the
"same executor" instruction exists to prevent. Could be reconsidered
later as an explicitly separate, additionally-scoped "embedded mode" ADR
if a real offline-tooling need arises — not assumed needed now.

**Correctness/Performance/Memory impact**: identical to whatever the API's
own `/v1/sql` endpoint provides (D22) — the CLI adds no new logic beyond
request formatting/response rendering. **Security impact**: the CLI must
never place a credential in shell history — command-line-supplied API
keys are explicitly discouraged in favor of an environment variable or a
prompted, non-echoed input, and script mode must not require a plaintext
key argument (Phase 27's explicit "never put passwords/API keys into
command history" requirement). **Persistence/Recovery impact**: none — the
CLI holds no durable state of its own beyond an optional, explicitly-opt-
in saved-connection config file, analogous to the frontend's own
`sessionStorage`-by-default / `localStorage`-opt-in pattern
(`PHASE_FRONTEND_ARCHITECTURE.md` §5). **Testing requirements**: every
meta-command against a real backend (no fabricated metadata output, per
explicit instruction), script-mode exit-code/stdout/stderr contract,
credential-never-in-history verification.

---

## D22. API (SQL endpoint) architecture

**Decision**: one new endpoint, `POST /v1/sql` (full contract in
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §15.1), added to the existing
`api/src/routes/` module the same way every existing route was added
(`api/src/routes/mod.rs`'s `build_router` — confirmed by direct source
inspection to already be a simple, additive `Router::new().route(...)`
composition with no structural obstacle to adding one more route under
the same `auth_middleware`/`metrics_middleware` stack every existing route
already uses). Parameterized queries only (`$1`, `$2`, ...) for
user-supplied values (D26).

**Reason**: the existing router assembly pattern requires no new
architecture to extend — confirmed directly from source, not assumed.
Reusing the exact same middleware stack (auth, metrics, body-size
limiting, CORS) every other route already goes through means the SQL
endpoint inherits every existing security/observability property for
free, rather than needing its own parallel implementation of any of them.

**Alternatives rejected**: *One endpoint per SQL statement kind (e.g.
`/v1/select`, `/v1/insert`).* Explicitly forbidden ("Do not create fake
endpoints for each SQL keyword"). *A GraphQL or other non-SQL query
surface.* Out of scope — not requested, and would duplicate rather than
reuse the parser/binder/planner/executor this design already builds.

**Correctness impact**: response shape must exactly reflect the
executor's typed output (columns with real types, not stringified) — a
`SELECT` returning a `BIGINT` column must round-trip that value as a JSON
number (or a string if precision-loss is a concern beyond `2^53`,
matching the existing frontend's own documented caveat about `u64`
JSON-number precision, `frontend/src/api/types.ts`'s own comment) rather
than silently coercing type information away. **Security impact**: this
endpoint is the single most security-critical new surface in the whole
phase — every input (`sql` text, `params`) must be treated as untrusted,
size-limited before parsing (D14), and never concatenated into anything
resembling a second SQL string. **Performance impact**: inherits the
existing API's own measured, sub-millisecond handler-dispatch overhead
characteristic (`PHASE_API_IMPLEMENTATION.md` §2) for the transport
portion; actual query cost is the executor's own (D17/D28). **Memory
impact**: request body bounded by max-SQL-statement-size (D27), enforced
via the same `DefaultBodyLimit` mechanism the existing body-size-limit
fix already established (`api/src/routes/mod.rs`'s `body_size_limit`) —
extended, not reimplemented, for this endpoint. **Persistence impact**:
none beyond whatever the executed statement itself durably changes (D9/
D12/D13). **Recovery impact**: none new — a mid-request crash simply
fails the in-flight HTTP request; the underlying statement's own
atomicity (D9/D10) governs what state, if any, was left durable. **Testing
requirements**: full statement-kind coverage (DDL/DML/`SELECT`/`EXPLAIN`/
transaction-control), parameterized-query correctness and injection-
resistance (D26), oversized-statement rejection, role-based authorization
per statement kind (D25), matching the existing `api_security_validation.rs`
harness's own established methodology for exactly this class of test.

---

## D23. Frontend architecture

**Decision**: extend the existing Data Explorer screen with a SQL editor,
result table, and `EXPLAIN` view, consuming `/v1/sql` (D22) exactly as
every other screen consumes its own endpoint (`ApiClient`'s existing
one-method-per-endpoint pattern, `frontend/src/api/client.ts`). Database/
schema/table/column/index/constraint browsing is implemented as ordinary
`SELECT`s against `system.*` catalog tables through the same endpoint —
no bespoke metadata API.

**Reason**: confirmed by direct source inspection that the existing
frontend has zero baked-in data-model assumptions (`frontend/src/api/
types.ts`'s `MetadataBody.keyspace_model: string` is the only "what kind
of keyspace is this" signal, already just an opaque descriptive string) —
there is nothing in the current frontend that a relational catalog view
would contradict or need to work around. Reusing `/v1/sql` for catalog
browsing (rather than inventing a `/v1/catalog/*` REST surface) keeps
"one executor, one authorization path" true all the way to the UI layer,
per D22's own reasoning.

**Alternatives rejected**: *A dedicated `/v1/catalog/*` REST API,
separate from `/v1/sql`.* Rejected: would duplicate authorization logic
(D25) in a second code path and violates the "no fake endpoint for each
SQL keyword"-adjacent principle — catalog browsing is just `SELECT *
FROM system.tables`, and inventing a parallel non-SQL path to express the
identical query is unjustified.

**Correctness/Performance/Memory impact**: identical to D22's, since the
frontend adds no new server-side logic. **Security impact**: every
write-capable SQL editor action remains role-gated in the UI as a
usability layer only (disabled + tooltip for a `reader`-role session,
matching the exact existing pattern `PHASE_FRONTEND_ARCHITECTURE.md` §3
already establishes for `/v1/kv` writes) — the binder/executor's own
authorization (D25) is what actually prevents an unauthorized write, not
the UI. **Persistence/Recovery impact**: none — the frontend holds no
durable state beyond its existing session-storage connection convention
(§5 of the frontend architecture doc, unchanged). **Testing requirements**:
e2e SQL-editor workflow test (write query, view typed results, view
`EXPLAIN`), role-gating verification (a `reader`-role session cannot
execute `INSERT`/`DDL` through the editor — both UI-disabled and, more
importantly, server-rejected if attempted directly), accessibility/
responsive coverage matching the existing screens' own established bar
(`PHASE_FRONTEND_VALIDATION.md`'s methodology, reused).

---

## D24. Authentication

**Decision**: extend, not replace, the existing bearer-API-key,
`AuthProvider`-trait model (`PHASE_API_ARCHITECTURE.md` §4,
`api/src/auth.rs`). No new authentication mechanism is introduced for SQL
specifically — a `Principal` authenticated for `/v1/sql` is the identical
`Principal` type used for `/v1/kv` today.

**Reason**: introducing a second, SQL-specific authentication mechanism
would fragment the security model with no offsetting benefit — the
existing mechanism already satisfies every stated v1 requirement
(configuration-loaded keys, never hardcoded, one unauthenticated
liveness endpoint).

**Alternatives rejected**: *A separate SQL "login" concept (e.g.
`user`/`password` over the wire, PostgreSQL-style).* Rejected: no wire-
protocol compatibility is claimed (§0), and introducing password-based
auth would require new secret-handling machinery (hashing, storage) with
no requirement driving it — bearer-key auth, already proven, is
sufficient.

**Correctness/Performance/Memory/Persistence/Recovery impact**: none
beyond the existing, certified `auth_middleware`'s own established
properties — unchanged. **Security impact**: this decision's entire
content *is* its security impact — reusing a proven mechanism is
strictly safer than introducing a second one with its own, freshly-
unexercised bug surface. **Testing requirements**: confirm `/v1/sql`
sits behind the identical `auth_middleware` every other protected route
uses (a route-registration test, not a new auth-logic test — the logic
itself is already certified and unchanged).

---

## D25. Authorization

**Decision**: extend the existing two-role (`reader`/`admin`) model with
object-level grants: `(principal, object [database/schema/table],
privilege [SELECT/INSERT/UPDATE/DELETE/DDL/CREATE INDEX])`, stored as
`system.grants` catalog rows (D1), checked by the binder (D15) before any
execution. v1 default mapping (stated explicitly, not silently assumed):
existing `reader` keys receive `SELECT` on every object; existing `admin`
keys receive every privilege on every object — the fine-grained `GRANT`/
`REVOKE` surface is the documented target model, with this coarse default
as the correct, safe starting point for every pre-existing key at the
moment the relational layer is enabled (never defaulting a pre-existing
key to *more* access than its current role already implies).

**Reason**: Phase 4/24's explicit requirement for database/schema/table/
operation-level authorization is a real, new capability beyond today's
whole-service-scoped reader/admin split — designing it as catalog rows
(D1) rather than a separate config file keeps authorization data subject
to the exact same durability/recovery/atomicity guarantees as everything
else in this design, and makes `GRANT`/`REVOKE` an ordinary, atomic (D9)
DDL-shaped operation rather than a special case.

**Alternatives rejected**: *Authorization expressed only in API-layer
configuration (e.g. an env-var-driven per-key allow-list of tables).*
Rejected: does not scale to per-object grants managed via SQL (`GRANT`/
`REVOKE`), and would need its own separate durability story instead of
reusing D1/D9 for free. *Row-level security / column-level grants.*
Deferred: table-level granularity is the stated v1 scope (matching Phase
24's own "at minimum distinguish... database/schema/table/operation"
wording, which does not require row/column granularity); finer-grained
security is real future work, not claimed here.

**Correctness impact**: an authorization check that is bypassable via any
alternate code path (a different endpoint, a differently-cased identifier,
a synonym) is a critical defect class — every object-resolution path
identified in D15's testing requirements must independently enforce this.
**Security impact**: this decision is itself the primary security control
for the whole relational layer — "never trust the frontend/CLI for
authorization" is enforced structurally by D15's single-pass bind-and-
authorize design; there is no code path that executes a bound statement
without having gone through it. **Performance impact**: one grants-table
lookup per object referenced per statement (folds into D15's per-
identifier catalog lookup cost, D28) — negligible. **Memory impact**:
bounded by statement complexity (few objects per statement, in practice).
**Persistence impact**: `system.grants` rows, D1/D9, inherited. **Recovery
impact**: none beyond D12. **Testing requirements**: full role/grant
matrix per statement kind, an authorization-bypass fuzzing pass across
every identifier-resolution path (D15), a specific test confirming
"object exists but forbidden" and "object does not exist" produce
identical, indistinguishable error responses (D26).

---

## D26. SQL injection defense

**Decision**: structural prevention, not sanitization. The binder (D15)
never accepts a raw client-supplied string as part of a statement's own
grammar — only `$n` parameters (D22) carry user data into a bound
expression, type-checked against the target column/expression type before
use; identifiers are resolved only through the parser's own quoting rules
(D14), never via string interpolation into a second, internally-
constructed SQL string. Physical key construction (D2) from a bound,
typed value uses the type's own defined encoding (D4) directly — never a
string-formatting/concatenation step that could be influenced by
unescaped user content.

**Reason**: this is the direct, mechanical consequence of using a real
parser/binder (D14/D15) and parameterized execution rather than any form
of string-building — "never construct physical keys unsafely from user
identifiers" and "never concatenate untrusted SQL values" are explicit,
and the architecture as designed has no code path that does either
(there is no place in this design where a value, as opposed to a
statement's own fixed grammar, is ever turned back into SQL/key text and
re-parsed).

**Alternatives rejected**: *Escaping/sanitizing string interpolation.*
Rejected: escaping-based defenses are a well-known weaker, error-prone
substitute for structural prevention (a single missed escape path
anywhere reintroduces the vulnerability) — parameterization is chosen
specifically because it has no such single-point-of-failure.

**Correctness impact**: a parameter's declared type must be enforced (a
`$1` bound as `INTEGER` rejects a non-integer-typed argument at bind time,
never silently coerces) — this is both a correctness and a security
property simultaneously. **Security impact**: this decision's entire
content *is* its security impact — it is the direct implementation of
Phase 23's requirement, verified by the testing requirements below, not
merely asserted. **Performance impact**: none beyond ordinary bind-time
type checking, already required for correctness regardless of this
decision. **Memory/Persistence/Recovery impact**: none. **Testing
requirements**: the OWASP-standard injection-attempt corpus (statement-
terminator characters, `UNION`-based, boolean-blind, time-based payloads)
submitted as parameter *values* must be treated as inert literal data by
every executed statement, verified by asserting the payload text never
influences the query's actual logical behavior; a dedicated identifier-
injection test (a table/column *name* containing SQL metacharacters,
correctly rejected or correctly quoted-and-isolated by the parser, never
executed as a second statement fragment); this matches, and should be run
alongside, the existing `api/tests/api_security_validation.rs` harness's
own established methodology for exactly this class of adversarial input.

---

## D27. Resource limits

**Decision**: full table (defaults, all configurable, all enforced
server-side before the corresponding expensive operation):

| Limit | Default |
|---|---|
| Max tables/database | 10,000 |
| Max columns/table | 1,600 |
| Max row size | 1 MiB |
| Max SQL statement size | 1 MiB |
| Max result rows (default / hard cap) | 100 / 10,000 |
| Max transaction write-set size | 10,000 ops |
| Max concurrent transactions | configurable, bounded pool |
| Max query runtime | 30s |
| Max sort/group/join memory per operator | 64 MiB |

**Reason**: every limit mirrors an existing, already-proven convention
in this codebase rather than inventing a new enforcement style — max
statement size mirrors the existing body-size-limit mechanism (D22); max
result rows mirrors `/v1/range`'s existing `limit`/`max_range_limit`
convention (`PHASE_API_ARCHITECTURE.md` §4) exactly, including its
default-vs-hard-cap shape; max row size matches the existing
`max_value_bytes` default (1 MiB) precisely, since a relational row *is*
a KV value (D3) and there is no reason for the two limits to diverge by
default.

**Alternatives rejected**: *No configurable limits (fixed constants).*
Rejected: every existing limit in this codebase is configurable
(`RUBIXDB_MAX_VALUE_BYTES`, `RUBIXDB_MAX_RANGE_LIMIT`, etc.) — a relational
layer with hardcoded limits would be a regression in operability relative
to the product's own established standard. *Unbounded (no limit) by
default.* Explicitly forbidden by the entire Phase 37 requirement and by
D17's own bounded-memory discipline.

**Correctness impact**: each limit's enforcement must fail loud with a
typed, specific error (never silently truncate a result set, never
silently reject only part of an oversized transaction) — a silently
truncated `SELECT` result presented as complete is a correctness defect
of exactly the kind this project's own "never silently narrow a result"
discipline exists to prevent. **Security impact**: every limit here is
also, simultaneously, a resource-exhaustion/DoS defense — sized and
reasoned about as such, not only as a usability guard. **Performance
impact**: enforcement itself is O(1) counter/size checks, paid before the
expensive operation begins — net performance-*protective*, not a
performance cost of its own. **Memory impact**: this table *is* the
memory-impact specification for the whole relational layer — every
number here is a concrete, testable memory-behavior boundary. **Persistence
impact**: none — limits are enforced at request time, not persisted state
(configuration only). **Recovery impact**: none. **Testing requirements**:
a boundary test per limit (N-1 succeeds, N fails with the correct typed
error, matching exactly the existing `oversized_value_is_rejected`-style
test methodology already established in `api/tests/`).

---

## D28. Performance strategy — per-operation complexity analysis

**Decision**: the following complexity/cost table is the governing
performance model for every executor operator; no operator implementation
may silently regress below the stated bound without a documented,
measured reason (mirroring the certified engine's own "no number without
a benchmark backing it" discipline, `RubixDB-LSM-Engine-Specification-
v1.0.md` §8).

| Operation | CPU/I-O cost | Hot path | What's read | What's written | Memory retained | Scales with |
|---|---|---|---|---|---|---|
| PK point lookup | O(1) *in matching-row count*, amortized (bloom+block-index) — **not** unconditional-O(1): cost also scales with live SSTable count, see "Scales with" | `LsmEngine::get`, unchanged | 1 SSTable block (typical) | none | none beyond block decode | live SSTable count per table (certified Read Engine's own established scaling law) |
| Secondary-index lookup | O(log n) *in the index's own entry count* + O(1) row fetch *per matching entry* (index-then-fetch, D5/D7) — both also scale with live SSTable count, see "Scales with" | `IndexScan` → `get` | 1 index block + 1 table block | none | none | live SSTable count for the index's own physical region + 1 extra fetch per matching entry |
| Table scan | O(n) in live row count | `SeqScan` → `range` | every live block in the table's key range | none | streaming, bounded (D17) | table row count, live SSTable count |
| Range scan (indexed) | O(log n + k) for k matching rows, *n/k in matching-entry/-row count* — also scales with live SSTable count, see "Scales with" | `IndexScan` → `range` | index blocks in range + k row fetches | none | streaming, bounded | range width, live SSTable count |
| `INSERT` | O(1 + affected_index_count) physical writes, one `write_batch` (D9) | write path | none (unless `UNIQUE` check, D6) | 1 row key + N index-entry keys | write-set size (D27), transient | index count |
| `UPDATE` | O(1 + touched_index_count) physical writes (only indexes on *changed* columns are touched, D11) | write path | 1 row (read-modify-write) | 1 row key + touched-index deltas | write-set size (D27) | touched index count |
| `DELETE` | O(1 + affected_index_count) physical deletes (tombstones) | write path | 1 row (to find affected indexes' values) | 1 row tombstone + N index-entry tombstones | write-set size (D27) | index count |
| `JOIN` (Nested Loop) | O(\|outer\| × \|inner\|) worst case; O(\|outer\| × log\|inner\|) with Index Nested Loop (D18) | executor | both relations, streamed | none | streaming outer, no materialization (D18) | inner-relation size (unindexed case — stated honestly as the poor-scaling case) |
| `SORT` | O(n log n) CPU, bounded by D17's memory cap | executor | full input, streamed in | none | up to the configured cap (D27), then rejects | input row count, up to the cap |
| `GROUP BY`/`Aggregate` | O(n) CPU (hash-based), bounded by D17's memory cap | executor | full input, streamed in | none | up to the configured cap (D27), then rejects | distinct group count, up to the cap |
| Transaction commit | O(write-set size) freshness checks + one `write_batch` call (D9/D10) | commit path | 1 current-value read per touched key | the whole write-set, atomically | write-set size until commit, then released | write-set size, not transaction duration |
| Catalog lookup | O(1) *in matching-row count*, point/small-range engine read (D1) — also scales with live SSTable count in the catalog's own reserved-prefix region | binder (D15) | 1-few catalog rows | none (except DDL) | none beyond ordinary read | catalog size (typically small relative to table data), live SSTable count |

**Reason**: Phase 11/18's explicit requirement to analyze CPU/I-O/memory
cost, write/read amplification, and per-subsystem scaling *before* writing
executor code, for every listed operation category — this table is that
analysis, and is the concrete artifact every later benchmark (D29) is
measured against.

**Alternatives rejected**: *Skip up-front complexity analysis, "measure
later."* Rejected: explicitly forbidden ("do NOT optimize blindly... but
also do NOT build obviously inefficient structures and postpone all
performance thinking until the end") — this table exists specifically so
no operator's implementation starts from a blank slate on this question.

**Correctness/Security impact**: none directly (this is a performance
artifact) — cross-referenced from every relevant decision above.
**Performance impact**: this table *is* the performance-impact
specification. **Memory impact**: the "Memory retained" column is this
table's own explicit memory-impact statement per operator, consistent
with D17/D27. **Persistence/Recovery impact**: none. **Testing
requirements**: D29 (Benchmark Methodology) is the direct testing-
requirements consequence of this table — every row above needs a
corresponding benchmark once implemented, exactly mirroring the certified
storage engine's own `lsm_bench.rs` precedent (`RubixDB-LSM-Engine-
Specification-v1.0.md` §8).

---

## D29. Benchmark methodology

**Decision**: a `rubixdb-sql`/`api`-level benchmark harness (new,
analogous to the certified engine's own `benches/lsm_bench.rs` and
`examples/compaction_bench.rs`/`read_engine_bench.rs` precedents),
measuring every row of D28's table once implemented: p50/p95/p99/max
latency, throughput, and (where applicable) write/read amplification,
across multiple repetitions, with raw measurements preserved (not only
summary statistics) — matching this project's own consistently-applied
methodology throughout every prior certification (`PHASE_WRITE_ENGINE_
CERTIFICATION.md` §3's own "15 reps across 3 sessions... every rep,
unfiltered" precedent, restated as the standard this new layer must also
meet, not a lower bar).

**Reason**: "no number without a benchmark backing it" is this project's
own established, repeatedly-applied discipline — there is no reason the
relational layer should be held to a lower evidentiary standard than the
storage engine it sits on.

**Alternatives rejected**: *Report only summary statistics (mean/median),
discard raw reps.* Rejected: directly against this project's own
established "preserve raw measurements... do not cherry-pick best runs"
standard (Phase 36's explicit instruction, already this project's
practice throughout every prior `*_PERFORMANCE.md`/`*_CERTIFICATION.md`
document).

**Correctness/Security impact**: none directly. **Performance impact**:
this decision governs how every future performance claim in this
project's relational layer is produced and defended. **Memory impact**:
none beyond the harness's own, bounded, test-only footprint. **Persistence/
Recovery impact**: none. **Testing requirements**: this decision is
itself a testing-requirements decision — no further nesting.

---

## D30. Reference-model/differential validation strategy

**Decision**: every correctness claim in D5 (`NULL` semantics), D10
(transaction isolation), D11 (index consistency), D16 (optimizer rule
transparency), D18 (join correctness), D19 (aggregation) is validated
against an **independently implemented reference model** — never the
production executor checked against itself — exactly matching the
certified engine's own repeatedly-stated, repeatedly-honored principle
("never the production algorithm as its own oracle," `ADR-RE-001` §17,
reused verbatim by every subsequent certification through
`PHASE_COMPACTION_CERTIFICATION.md`).

**Reason**: this principle has a proven track record in this exact
codebase (zero mismatches across billions of checked operations in the
storage engine's own certification evidence) — there is no reason to
weaken it for the relational layer, which has strictly more surface area
for subtle semantic bugs (three-valued logic, join semantics, isolation
anomalies) than raw key/value storage does.

**Alternatives rejected**: *Test the executor only against itself
(assert `EXPLAIN`'s chosen plan produces "a" result, without an
independent expected-result computation).* Rejected: this is precisely
the anti-pattern the reused principle exists to forbid — a bug in the
production algorithm and a bug in its own self-check can correlate in
ways an independent model cannot.

**Correctness impact**: this decision *is* the methodology by which every
other decision's correctness claims in this document are ultimately
proven, once implemented. **Security/Performance/Memory/Persistence/
Recovery impact**: none directly — a testing-methodology decision.
**Testing requirements**: a reference-model implementation for at least:
three-valued-logic comparison/aggregation, snapshot-isolation conflict
detection, and join semantics — each independent of the production
`rubixdb-sql` code, per the reused principle above.

---

## D31. Migration / schema versioning

**Decision**: `schema_version` is per-table (D1's `system.tables` row) and
per-row (D3), incremented on every `ALTER TABLE` that changes column
set/types/constraints. `ADD COLUMN` is non-breaking (old rows decode with
missing trailing columns defaulted/`NULL`, D3). `DROP COLUMN`/type changes
that are not losslessly re-interpretable under the old encoding require a
background rewrite pass, explicitly deferred to a later increment as
out-of-v1-scope DDL (`DROP COLUMN`/`ALTER COLUMN TYPE` are parsed and
bound for forward grammar coverage but rejected at execution time with a
clear `Unsupported` error in v1, mirroring D1's own `CREATE DATABASE`
precedent exactly — "the parser accepts it" is never treated as "the
feature is complete").

**Reason**: mirrors the Architecture Spec's own `schema_version` field
precedent (`RubixDB-Architecture-Specification-v1.0.md` §6.2 — "tracked
... but the actual migration-of-old-partitions-to-new-schema mechanism is
not specified in v1... so it isn't a retrofit") almost exactly — the field
exists from v1 so a future increment implementing the harder rewrite-
requiring `ALTER` variants is additive, not a retrofit of the row format
itself.

**Alternatives rejected**: *Support every `ALTER TABLE` variant in v1,
including breaking ones, via an eager rewrite.* Rejected: an eager,
unbounded-duration table rewrite as part of a single DDL statement
violates the same "avoid unbounded resource paths" principle D13's
`DROP TABLE` design already resolved differently (bounded, background,
resumable) — if breaking `ALTER TABLE` is ever added, it should reuse
that same bounded-background-operation pattern, not a new one; deferring
it entirely for v1 avoids building either version prematurely.

**Correctness impact**: a reader must never silently misinterpret an old-
schema-version row as new-schema-version — `schema_version` comparison
before decode is the enforced rule (D3). **Security impact**: none
directly. **Performance impact**: `ADD COLUMN` is O(1) (catalog-only) —
a real, immediate operational benefit of this design. **Memory impact**:
none new. **Persistence impact**: `schema_version` history itself should
be retained in the catalog (an append-only list of prior versions per
table, matching the Architecture Spec's own precedent) so a very old row
remains decodable indefinitely, not just against the immediately-prior
version. **Recovery impact**: none beyond D12. **Testing requirements**:
multi-version row coexistence within one table (some rows at
`schema_version` 1, some at 2, after a real `ADD COLUMN`, all correctly
readable), rejection-not-silent-acceptance test for every deferred
`ALTER` variant.

---

## D32. Backward compatibility with existing flat key/value data

**Decision**: enabling the relational layer on an existing deployment
requires the `/v1/kv` flat API to reject (400, `VALIDATION_ERROR`-class)
any client-supplied key whose first byte is `0x00` or `0x01` (D2's
reserved namespace) from the moment the relational layer is enabled
onward. This is a **stated, explicit, breaking change** for any
pre-existing flat-KV deployment that already has data with keys starting
in that range — such a deployment must audit its existing key space
before upgrading; there is no automatic, silent remediation, because
silently remapping or hiding a pre-existing client's own keys would be a
worse, more surprising outcome than a clear, upfront rejection at the API
boundary.

**Reason**: `PHASE_RELATIONAL_STORAGE_GAP_ANALYSIS.md` §2.3 and D2
established this reservation is required for correctness (no other design
achieves the collision-free namespace isolation D2's structural
correctness claims depend on). Stating the compatibility break explicitly,
rather than allowing it to be discovered by a confused operator after the
fact, is the direct, honest application of this project's own "flag, do
not silently resolve" standing discipline.

**Alternatives rejected**: *Silently namespace-shift existing flat-KV
keys out of the way (e.g. re-prefix all pre-existing data on first
relational-layer startup).* Rejected: a background, unbounded, silent
data-rewriting migration run automatically on startup is both a large,
unreviewed blast-radius operation and a violation of "never solve these
by... silently weakening guarantees" — a key remap changes what key a
client's existing code must use to find its own data, which is a breaking
change no matter how it's implemented; doing it silently only hides that
break from the operator who most needs to know about it. *No reservation
at all (best-effort, hope for no collision).* Rejected outright — this is
not a "best effort" kind of correctness property; a real collision
silently corrupts data.

**Correctness impact**: this decision is what makes D2's isolation claim
actually true in a mixed-workload deployment, not just a fresh one —
without it, D2's structural-isolation argument only holds for a
brand-new, empty database. **Security impact**: preventing key-namespace
collision is also, incidentally, a defense against a flat-KV client
deliberately attempting to write into the relational layer's reserved
region to corrupt catalog/table data — enforced the same way regardless
of intent (accidental or adversarial), which is the correct, simpler
design (no separate "is this malicious" judgment needed). **Performance
impact**: negligible — one byte comparison per `/v1/kv` write, already on
the validation path that exists today. **Memory impact**: none.
**Persistence impact**: none — this is a validation-time check, not a
data transformation. **Recovery impact**: none. **Testing requirements**:
a dedicated compatibility test asserting every reserved-prefix key is
rejected at the API boundary both before and after a restart, and a
documented, explicit upgrade-note (this ADR's own text, referenced from
the eventual release notes when this phase is ever implemented) stating
the compatibility break plainly for operators — not merely encoded in a
test assertion no operator will ever read.

---

## D33. Observability

**Decision**: relational metrics are additive-only and exposed under a
new `sql` key in `/v1/metrics`'s existing JSON body, alongside the
already-established top-level engine metrics and the `service` key
(`PHASE_API_ARCHITECTURE.md` §5) — never restructuring either existing
key. Tracked counters/histograms: SQL statement count by type (`SELECT`/
`INSERT`/`UPDATE`/`DELETE`/DDL), query latency (p50/p95/p99, mirroring
the `service` key's own existing percentile convention), rows scanned,
rows returned, rows affected, operator counts (seq scan, index scan,
join, sort, aggregate), transaction begin/commit/rollback/conflict
counts, and constraint-violation counts by constraint kind. All are held
in-process (atomics + the same small-histogram approach `service` already
uses) — no new persistence mechanism, no new dependency.

**Reason**: Phase 38 requires relational metrics as a first-class,
separately-exposed observability surface, and explicitly forbids altering
certified engine metrics. Reusing the `service` key's existing
in-process-counter pattern rather than inventing a second one keeps the
relational layer's observability code minimal and consistent with the
one precedent this API already established, rather than a parallel,
divergent implementation.

**Alternatives rejected**: *Fold SQL metrics into the existing `service`
key instead of a new `sql` key.* Rejected — `service`'s own documented
scope is HTTP-request-level (route, status code, latency), not
query-semantic (rows scanned, operator kind); conflating the two would
make both harder to reason about and would retroactively redefine an
already-certified key's meaning. *A separate `/v1/sql/metrics` endpoint,
mirroring `/v1/compaction/metrics`.* Rejected — compaction has its own
endpoint because it is a distinct engine subsystem with its own status
fields (§ engine metrics); SQL metrics are execution-layer counters with
no independent "status," so folding them into the existing aggregate
`/v1/metrics` body (as `service` already does) is the simpler, already-
precedented shape and avoids adding a fourth metrics surface for an
operator to have to know about. *An external metrics system (Prometheus
exposition format, OpenTelemetry).* Deferred, not rejected outright — out
of scope for this architecture phase; nothing here forecloses adding an
exposition-format adapter later that reads these same in-process counters,
since they are plain atomics, not tied to the JSON transport.

**Correctness impact**: none directly — these are observational counters,
not part of any committed state; a bug here cannot corrupt a table, index,
or transaction outcome (counters are updated after the operation they
describe, on the success/failure path, never gating it). **Security
impact**: constraint-violation and error counts must be aggregate/typed
(counts by `code`, matching the existing `service` key's own "error count
by code" convention) and must never include SQL text, parameter values,
row contents, or identifiers in a metrics response — metrics are readable
by any authenticated `Reader`-role principal today (`/v1/metrics` carries
no elevated-role requirement per the existing API), so any per-query
detail beyond aggregate counts and latency would leak cross-tenant/cross-
session query shape to every reader; this must be enforced structurally
(the metrics recorder's input type has no field capable of holding SQL
text or values), not by a "remember not to log that" convention.
**Performance impact**: one atomic increment/histogram observation per
operator invocation and per statement completion — the same
already-measured, sub-microsecond-class overhead the existing `service`
counters impose, not a new class of cost; no metrics recording sits on
the same lock/mutex the write path uses (must remain lock-free atomics,
consistent with D11's per-operator streaming-execution design, so metrics
collection cannot itself become a contention point on the hot path).
**Memory impact**: bounded — fixed-cardinality label sets (statement type
has ~5 values, operator kind has ~6, constraint kind has ~4); no per-
table, per-query-text, or per-session dynamic-cardinality labels, which
is the specific, well-known way a metrics system grows memory
unboundedly under adversarial or merely high-cardinality-schema
workloads — explicitly avoided here. **Persistence impact**: none — like
the existing `service` key, these counters are in-process only and reset
to zero on service restart; this is a stated limitation, not an oversight
(durable historical metrics would require a time-series store, which is
out of scope for this phase and not requested). **Recovery impact**:
none — counters are observational state, not recovered state; a restart
producing a zeroed metrics view is expected and requires no recovery
procedure of its own. **Testing requirements**: a test asserting the
`sql` key never mutates or removes any existing top-level or `service`-
key field (additive-only contract), a cardinality-bound test that runs a
workload against many distinct tables/statements and asserts the `sql`
key's serialized size stays within a fixed bound (catching an accidental
per-object label from being introduced later), and a negative test
asserting a metrics response never contains SQL text, parameter values,
or row data under adversarial/malformed-query workloads.

---

## Summary: decisions requiring a storage-engine change

**Exactly one: D9 (`write_batch`).** Every other decision in this
document is implementable entirely at the relational/API/frontend layer,
on top of the certified engine's existing, unchanged public surface. This
is the single fact `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`'s
Governing Finding refers to, and it is the one item any future
implementation increment must resolve — via its own dedicated
`PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md` (Phase 9 of the build
order) — before Increment 5 (the first executor with real `INSERT`/
`DELETE`) can begin.

---

# RELATIONAL ADR AMENDMENT 001

**Status**: append-only amendment. Nothing below deletes, reverses, or
silently edits D1–D33 above. Two edits are made directly to prior text
(D6's PK-lookup wording, `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`'s
catalog table list and its §6 index terminology) — both are narrow,
explicitly requested wording/consistency corrections to *phrasing that
could be misread*, not reversals of any decision's substance; each is
logged here, in AA.15/AA.16/AA.12, with the exact before/after text, so
nothing is silently changed. Every other item below is a pure addition:
either resolving a question D9 named but left open, or a new decision
this review directive raised that no prior section covered.

**Trigger**: an external review of Increment 2's plan found the original
ordering unsafe — it would have started building the persistent catalog,
tables, indexes, and transactions *before* the one storage primitive
(D9's `write_batch`) they all depend on had been precisely specified,
let alone implemented and certified. **Increment 2 is now retargeted to
`write_batch` alone.** Catalog/tables/indexes/transactions move to
Increment 3 and may not begin until this amendment's open items are
resolved (done, below) and `PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`
+ its own implementation are complete and certified.

**Method used throughout this amendment**: every semantic choice below
is derived from the *actual, currently-certified* implementation, read
directly — `src/wal/format.rs`, `src/wal/ops.rs`, `src/wal/group_commit.rs`,
`src/wal/recovery.rs`, `src/memtable/mod.rs`, `src/lsm/mod.rs`,
`src/execution/batch_coordinator.rs`, `src/error.rs` — not re-derived
from D9's prose summary of them. Where this amendment's conclusions
differ in precision from D9's original phrasing, D9's phrasing is
superseded by this amendment for implementation purposes; D9 itself is
left untouched as the historical record of the original architecture
decision.

---

## AA.1 `write_batch` sequence semantics

**Decision**: a `write_batch` call receives **exactly one commit
sequence for the entire batch** — not a contiguous range, not one
sequence per operation. Precisely:

- **When allocated**: at the moment the batch's single WAL frame is
  appended, by the same code path that assigns a `put`/`delete`'s
  sequence today — `FileWal::append`'s `let seq = self.next_seq; ...
  self.next_seq += 1;` (`src/wal/mod.rs:806-828`). A `write_batch` call
  is, from the WAL's point of view, indistinguishable from any other
  single `append()` call: one call, one `next_seq` incremented by
  exactly 1, regardless of how many logical operations the frame's body
  encodes.
- **One seq, not a range**: every operation inside the batch is applied
  to the MemTable under this *same* seq value. This is not a
  simplification made for convenience — it is the direct, forced
  consequence of three already-fixed constraints: (1) the on-disk WAL
  record body is `seq(8) || op_tag(1) || op_body` (WAL Spec §2.4/§2.3,
  `wal::format::encode_frame`) — **one `seq` field per frame**, and
  AA.2 below extends the WAL by adding a new `op_tag` value, not by
  restructuring the record header; (2) D9's already-fixed public
  signature returns `Result<u64>` — one number: "the seq this batch
  committed at," not a range descriptor; (3) `MemTable`'s map key is
  `(user_key, seq)` (`src/memtable/mod.rs:92`) — assigning every member
  of the batch the same `seq` makes the *whole batch* atomically
  positioned at one point in the engine's total order, which is exactly
  what "no reader may ever observe a partial batch" (AA.3) requires; a
  contiguous range would require picking, for each reader-visible
  `as_of_seq` value strictly between the range's first and last member,
  *which subset* of the batch is "visible" — a question with no correct
  answer for an atomic operation, and one a single shared seq makes
  structurally impossible to even ask.
- **Duplicate physical keys inside one batch**: resolved by AA.4 below —
  deterministic, last-operation-in-the-supplied-slice wins, expressed
  precisely as: apply operations to the MemTable in the caller's
  supplied order, all under the same seq; `MemTable::insert`
  (`src/memtable/mod.rs:117`) on an already-present `(key, seq)` tuple
  overwrites the map entry, so the last write to a given key within the
  batch is what survives — not a new rule, the *existing* `BTreeMap`
  insert semantics, applied without any new special-casing.
- **What a transaction observes**: for D10's Snapshot-Isolation
  transactions, `COMMIT`'s single `write_batch` call returns the seq
  the whole transaction committed at — this *is* the transaction's
  commit seq (D10 needs no separate concept).
- **What recovery replays**: exactly what live apply does. WAL replay
  (`wal::replay_streaming`, consumed by `LsmEngine::open` at
  `src/lsm/mod.rs:1324-1338`) decodes the op-group frame once, yielding
  `(seq, WalOpOwned::Group(members))`, then applies every member under
  that one `seq` via the *same* per-member insert function live apply
  uses (AA.6 requires this literal code-sharing, not just
  behavioral-equivalence-by-coincidence) — recovery and live apply are
  provably identical because they call the same function.
- **What the Read Engine sees**: nothing new to build. `get_as_of`,
  `contains`, `range`, `range_scan`, and `Snapshot` all already resolve
  purely from `(key, seq)` comparisons against the engine's total seq
  order (`MemTable::get_as_of`, `src/memtable/mod.rs:146`). A batch's N
  entries, once applied, are indistinguishable from N independently
  written entries that happen to share a seq — every existing Read
  Engine code path handles them with **zero changes**.
- **What snapshot visibility means**: `Snapshot { seq: S }` (or a bare
  `as_of_seq = S` read) observes the batch's committed post-image for
  every key it touched iff `S >= batch_seq`, and none of the batch's
  effects on any key iff `S < batch_seq` — an all-or-nothing boundary at
  exactly one seq value, by construction (one shared seq), never a
  partial view. Proven fully in AA.3.
- **Empty batch**: rejected before any encoding, allocation, or WAL
  interaction — `write_batch(&[])` returns
  `Err(EngineError::InvalidArgument { detail: "write_batch requires at
  least one operation".into() })` (AA.7 defines this new error variant).
  Reason: an empty batch has no logical write to make durable; silently
  consuming a `seq` value and writing a frame with zero operations would
  be an observable, pointless WAL/seq-space cost with no corresponding
  effect, and — worse — would be indistinguishable on disk from a
  legitimate future zero-arity op-group use case that does not exist
  today. Fail loud, don't allocate a meaningless commit point.
- **Maximum batch size**: `LsmConfig::max_batch_ops: usize`, new field,
  **default 10,000** — chosen to equal, not merely resemble, D27's
  already-decided relational-layer default ("max transaction write-set
  size (10,000 ops, configurable)"), so the engine's own hard ceiling
  and the relational layer's configured default agree by construction
  rather than being two independently-chosen numbers that could silently
  drift apart. The engine enforces this *independently* of whatever the
  relational layer does (defense in depth, AA.7) — `write_batch` returns
  `Err(EngineError::CapacityExceeded { requested: ops.len() as u64, max:
  self.config.max_batch_ops as u64 })` for a batch whose length exceeds
  it, checked before any encoding work begins.
- **Maximum encoded WAL frame size**: no new field — reuses
  `WalConfig::max_record_len` (`wal::format::DEFAULT_MAX_RECORD_LEN` = 64
  MiB) exactly as `put`/`delete` already do. AA.2 details how this bound
  is enforced for a multi-member frame.

**Reason**: this is the section D9 named as needing precise resolution
("The existing D9 wording is not precise enough about sequence
assignment") before implementation could begin safely; every sub-answer
above is derived from a specific, cited, already-certified code
constraint, not chosen freely — the single-seq-per-frame WAL layout and
the `(key, seq)`-keyed MemTable together leave essentially one coherent
design, which is what "prefer the simplest semantics that preserve the
existing LSM version model and atomic visibility" (the review
directive's own instruction) actually cashes out to once the real
format is read.

**Alternatives rejected**:
- *A contiguous seq range, one seq per operation.* Rejected: requires
  restructuring the WAL record body to carry N seq values instead of
  one (a real format change, not an additive one — violates "do not
  invent a second WAL subsystem" and "extend the existing WAL
  minimally"), requires `MemTable::insert` to be called with N distinct
  seqs for what should be one atomic commit point (reintroducing exactly
  the "which of the range is visible at an intermediate as_of_seq"
  question a single seq avoids), and buys nothing D9's stated
  requirements ask for — atomicity doesn't need per-operation
  versioning, only per-batch versioning.
- *Assign seq per-key at MemTable-apply time, independent of WAL order.*
  Rejected: breaks "the MemTable never generates a sequence number; it
  only ever consumes one already assigned by the durable write path"
  (`PHASE4A_ARCHITECTURE.md` §5 — cited directly, "Sequence ownership:
  unchanged"), a preserved-architecture invariant this amendment has no
  mandate to touch.

**Correctness impact**: this is the seq-allocation half of D9's own
required proof; AA.3 covers the other half (visibility). A single shared
seq is what makes "either every op is visible or none are" a structural
property of the `(key, seq)` model rather than something enforced by
extra logic that could have a bug. **Security impact**: none beyond
D9's original assessment (internal, service-controlled primitive).
**Performance impact**: identical seq-assignment cost to today (`self.
next_seq += 1`, one increment regardless of N) — no new counter, no new
contention point. **Memory impact**: none beyond AA.7's batch-size
bound. **Persistence impact**: one `u64` on disk per batch (the shared
`seq` field), not N — smaller on-disk footprint than N independent
frames would need, in addition to the fsync-count savings AA.6
measures. **Recovery impact**: WAL replay applies every member under
the frame's one decoded `seq` — no new bookkeeping to reconstruct which
member "should have" gotten which seq, because none did. **Testing
requirements**: a property test asserting `write_batch([Put(k1,v1),
Put(k2,v2)])`'s returned seq equals the seq visible at both `k1` and
`k2` immediately after (`get_as_of(k1, seq) == v1 && get_as_of(k2, seq)
== v2`, and `get_as_of(k1, seq-1) == None && get_as_of(k2, seq-1) ==
None` when no prior version exists); an empty-batch rejection test;
a max-batch-ops boundary test (`max_batch_ops` succeeds,
`max_batch_ops + 1` fails with `CapacityExceeded`); an oversized-frame
test reusing the existing `max_record_len` rejection path.

---

## AA.2 Atomic WAL frame format

**Decision**: one new WAL op tag, **`OP_GROUP = 5`** (`src/wal/
format.rs`, the next unused value after the already-reserved
`OP_ENGINE_SWITCH = 4`). Its `op_body` layout:

```
op_body := member_count:u32 LE || member*

member  := member_tag:u8 || member_body
member_tag ∈ { OP_PUT = 1, OP_DELETE = 2 }   // never OP_GROUP, never OP_CHECKPOINT_MARKER
member_body (PUT)    := key_len:u32 LE || key || value_len:u32 LE || value   // identical to today's PUT op_body
member_body (DELETE) := key_len:u32 LE || key                                // identical to today's DELETE op_body
```

This is **not a second WAL subsystem** — it is one more `op_body` shape
inside the *same* `length(4) || crc32c(4) || seq(8) || op_tag(1) ||
op_body` frame every existing record already uses
(`wal::format::encode_frame`, `wal::format::FRAME_HEADER_LEN`,
`wal::ops::encode_wal_frame`). Concretely:

- **Rust types**: a new, non-recursive `GroupMember<'a>` (`src/wal/
  ops.rs`) with exactly two variants, `Put { key: &'a [u8], value: &'a
  [u8] }` and `Delete { key: &'a [u8] }` — deliberately *not* a slice of
  `WalOp<'a>` itself, so nesting a group inside a group, or embedding a
  `CheckpointMarker` inside one, is a **compile error**, not a runtime
  check. `WalOp<'a>` gains one new variant, `Group { members: &'a
  [GroupMember<'a>] }`; `WalOpOwned` gains the matching owned form,
  `Group(Vec<GroupMemberOwned>)`, with `GroupMemberOwned` mirroring
  `GroupMember` (owned buffers) exactly the way `WalOpOwned::Put`
  already mirrors `WalOp::Put`.
- **Encoding** (`wal::ops::encode_wal_frame`'s new match arm): writes
  `member_count` then, for each member, its tag byte and body, reusing
  `write_len_prefixed` (`wal::format::write_len_prefixed`) unchanged for
  every key/value field — the exact function PUT/DELETE already use,
  called once per member instead of once per frame. No new
  length-encoding logic is written.
- **Decoding** (`wal::ops::decode_wal_body`'s new match arm): reads
  `member_count`, then loops decoding exactly that many members from the
  remaining bytes, appending each to a `Vec::new()` via `.push()` —
  **never `Vec::with_capacity(member_count as usize)`**. This is a
  deliberate, load-bearing choice, not a style preference: `member_count`
  is an as-yet-unvalidated `u32` read directly from on-disk bytes at
  this point in decoding (an attacker-crafted or corrupted file could
  set it to `u32::MAX`); pre-allocating a `Vec<GroupMemberOwned>` sized
  by that number before reading a single byte of actual member data
  would be exactly the "unchecked allocation... attacker-controlled path
  to unbounded memory allocation" this phase's own security bar
  forbids. Growing incrementally is safe *and* sufficient: the frame's
  `op_body` slice being decoded is itself already bounded by
  `max_record_len` (enforced by `wal::recovery::walk_segment`'s existing
  `length as usize > max_record_len` check, *before* `decode_wal_body`
  is ever called — `src/wal/recovery.rs:100`), so a bogus huge
  `member_count` simply causes the loop to run out of bytes and return
  `Corruption` after at most a few real iterations — no more memory is
  ever touched than the already-bounded frame body itself occupies.
  A member tag byte that is neither `OP_PUT` nor `OP_DELETE` (including
  `OP_GROUP` itself, or `OP_CHECKPOINT_MARKER`) is `Corruption`,
  matching `decode_wal_body`'s existing "unknown op byte" handling for
  the top-level tag — structural nesting prevention on the encode side
  (previous bullet) plus this explicit rejection on the decode side
  together mean a nested group can never be produced *or* accepted.
- **`estimate_frame_len`** (`src/wal/group_commit.rs:1658`, used by
  `BatchCoordinatorPool::submit` for queue-capacity accounting) gets a
  matching `Group` arm: `4 + members.iter().map(|m| 1 + match m {
  Put{key,value} => 4+key.len()+4+value.len(), Delete{key} =>
  4+key.len() }).sum::<usize>()` — the exact same per-field arithmetic
  the `Put`/`Delete` arms already use, summed. A dedicated regression
  test (mirroring the existing `estimate_frame_len_matches_the_real_
  encoder_for_put`) asserts this estimate equals `encode_wal_frame`'s
  actual output length for a real `Group`, byte for byte — the existing
  project convention for keeping an estimator honest against its real
  encoder.
- **`max_record_len` enforcement**: unchanged — inherited automatically.
  `format::encode_frame`'s existing `body_len > effective_max` check
  (computed *after* `encode_op_body` returns the fully-built `op_body`,
  exactly as it already does for `Put`/`Delete` today) applies to a
  `Group`'s `op_body` exactly as it does to any other op's. No separate
  byte-budget pre-check is added on the encode side: unlike the decode
  side, `write_batch`'s caller already holds every key/value as a real,
  fully-materialized `Vec<u8>` in process memory before the call — there
  is no untrusted "declared length" being trusted to size an allocation,
  so the existing incremental-`Vec`-growth encode path (the same
  `Vec::new()` + repeated `extend_from_slice` `format::encode_frame`
  already uses for every op kind) costs exactly the caller's own
  already-paid memory, never more. `AA.1`'s `max_batch_ops` cap is the
  operative defense against an oversized *encode-side* request, checked
  before encoding starts; `max_record_len`'s existing check is the
  defense against an oversized *encoded result*, checked exactly where
  it already is.
- **Old frames remain valid, unchanged**: `PUT` (1), `DELETE` (2),
  `CHECKPOINT_MARKER` (3) keep their exact existing byte layout and
  meaning. `walk_segment` (`src/wal/recovery.rs`) needs **no changes at
  all** — it classifies frames purely by `length`/`crc32c` at the
  frame-header level, never inspecting `op_tag`, so a `Group` frame
  flows through the *identical* torn-vs-corrupt classification logic
  every other frame already does. This is the single most valuable
  consequence of reusing the existing frame envelope rather than
  inventing a new one: **torn-trailing-batch-frame-discarded** and
  **corrupt-non-tail-batch-frame-fails-closed** require zero new
  recovery code — they are the existing, already-certified,
  already-tested tail-vs-corruption rule (WAL Spec §6.2–§6.3,
  `wal::recovery::classify_failure`), applied to a frame whose `op_tag`
  happens to be 5 instead of 1/2/3. A WAL reader from before this change
  would fail closed with `Corruption` ("unknown WAL op byte 5") on
  encountering a `Group` frame — the existing, correct "any invalid
  frame that is not the trailing torn one is corruption, escalate"
  discipline, extended to a genuinely new-and-unknown op byte exactly as
  it already handles one today.

**Reason**: directly resolves the review directive's WAL-format
questions, and is the concrete byte-level realization of D9's own
already-stated intent ("an additive new op-group frame type... extending,
not replacing, the existing per-op WAL frame format").

**Alternatives rejected**:
- *A distinct on-disk section/subsystem for batch frames (e.g. a second
  file, or a different frame envelope).* Rejected outright — explicitly
  forbidden ("Do not invent a second WAL subsystem... Do not duplicate
  WAL code"), and unnecessary: the existing `length || crc32c || body`
  envelope is already fully general over `body`'s contents.
- *Encode each member with its own full frame header inside the group
  (nested framing).* Rejected: redundant — the outer frame already
  provides one `length`/`crc32c`/`seq` covering the whole group; nesting
  a second header per member wastes 8+9=17 bytes per member for no
  benefit (no member needs its own seq, per AA.1) and complicates
  decoding for zero gain.
- *A `member_count` field of a narrower width (e.g. `u16`), reasoning
  that `max_batch_ops` (10,000) fits in 16 bits.* Rejected: `u32`
  matches every other length field in this format (`key_len`, `val_len`
  are all `u32 LE`) — introducing the only `u16` field in the entire WAL
  format for a marginal 2-byte saving is exactly the kind of
  inconsistent, harder-to-reason-about micro-optimization this project's
  "no speculative optimizations without measured evidence" standard
  rejects; `max_batch_ops` is a *policy* limit enforced separately
  (AA.1), not a format-width limit.

**Correctness impact**: the frame-level CRC covers `member_count` and
every member's bytes as one unit — any single-bit corruption anywhere in
the group is detected exactly as it would be for a single-op frame, with
identical (not weaker) integrity guarantees. **Security impact**: the
incremental-decode discipline (no `Vec::with_capacity` from an untrusted
field) is the concrete implementation of this phase's "no
attacker-controlled path to unbounded memory allocation" requirement,
verified by a dedicated test (AA.7). **Performance impact**: one CRC32C
computation over the whole group instead of N separate ones for N
independent frames — strictly cheaper per op at N>1 (measured in AA.6).
**Memory impact**: bounded by `max_batch_ops` (encode side, AA.1) and by
the pre-validated frame `length` (decode side, this section) — never
unbounded on either path. **Persistence impact**: additive-only format
change — every existing WAL fixture and every existing recovery test
remains byte-for-byte valid, verified by re-running the full existing
`wal_tests`/`crash_consistency`/`pathological_recovery_matrix` suites
unmodified (Increment 2's regression gate, §31 of the review directive).
**Recovery impact**: zero new frame-boundary-detection code, as detailed
above — the only new recovery code is the replay-application arm
(AA.1's "what recovery replays"), which calls the same per-member apply
function live-apply uses. **Testing requirements**: round-trip
encode/decode tests for `Group` (1 member, N members, N at
`max_batch_ops`); a decode-rejects-nested-group test (craft a
member_tag byte of `5` inside a group and assert `Corruption`); a
decode-rejects-oversized-member-count-gracefully test (craft a `member_
count` far exceeding what the frame's actual remaining bytes could hold,
assert `Corruption`, not a hang or an allocation spike — this is the
direct test of the anti-pattern this section forbids); the full existing
WAL fuzz/property-test harness (`src/wal/fuzz_tests.rs`,
`src/wal/testing.rs`) re-run and re-passing unmodified, plus extended to
generate `Group` frames alongside `Put`/`Delete`/`CheckpointMarker`.

---

## AA.3 Atomic visibility — proof

**Claim** (D9's own required invariant, restated precisely): for any
`write_batch(ops)` call assigned seq `S`, no concurrent reader — via
`get`, `get_as_of`, `contains`, `range`, `range_scan`, a `Snapshot`
taken at any seq, or WAL-replay-driven recovery — can ever observe a
state in which *some but not all* of `ops`' effects are visible. Every
reader observes either **zero** of `ops`' effects (as if `S` had not
yet happened) or **all** of them (as if `S` had already fully happened),
for every key `ops` touches, always.

**Proof, tracing the actual code**:

1. **Where entries become visible.** `apply_after_durable` (`src/lsm/
   mod.rs:1666`) is the *only* place `MemTable::insert` is ever called
   from the live write path, and it does so under `self.lock_active_
   write()` — a `std::sync::RwLock<MemTable>` write guard
   (`active: RwLock<MemTable>`, `src/lsm/mod.rs:1137`). `write_batch`'s
   new analog, `apply_batch_after_durable(&self, ops: &[WriteOp], seq:
   u64)`, takes that **same** write guard **once** and inserts all N
   entries inside that **one** critical section — the mechanical
   extension AA.1/D9 both describe ("the same lock `apply_after_durable`
   already takes per-op today, held for the whole batch instead of once
   per op"), verified here against the actual field and lock type, not
   assumed.
2. **What a reader actually does.** Every read method — `get_as_of`
   (`src/lsm/mod.rs:1767`), `contains` (`:1822`), and (by the same
   established pattern) `range`/`range_scan` — begins its active-
   MemTable step with `self.lock_active_read()`, a *read* guard on the
   same `RwLock<MemTable>`.
3. **The mutual-exclusion guarantee this rests on.** `std::sync::RwLock`
   guarantees no reader can hold a read guard while any writer holds
   the write guard, and vice versa — the standard library's own
   contract, not a property this project re-implements. Therefore any
   reader's read-guard acquisition is strictly ordered, in real time,
   either **entirely before** `apply_batch_after_durable`'s write-guard
   acquisition (in which case the reader's single, atomic `BTreeMap`
   snapshot-via-lock contains *none* of the batch's N inserts — they
   have not happened yet from the reader's perspective) or **entirely
   after** its write-guard release (in which case *all* N inserts are
   already present — `apply_batch_after_durable` does not release the
   write guard until every member has been inserted). There is no third
   case: `RwLock` provides no interleaving in which a reader's guard is
   held *during* the writer's guard hold, so no read can observe a
   `BTreeMap` state with M<N of the batch's entries present. This is
   the complete proof for the active MemTable — it is exactly the
   textbook `RwLock` exclusion property, applied to a critical section
   that has grown from 1 insert to N, with no new synchronization
   primitive, no new lock, and no new reasoning technique beyond what
   the certified engine already relies on for `put`/`delete`.
4. **Freeze cannot split a batch across active/immutable either.**
   `freeze_locked` (`src/lsm/mod.rs:1694`) — which swaps the active
   MemTable for a fresh one and pushes the old one onto the immutable
   list — is only ever called *from inside* `apply_after_durable`'s
   (and, by direct extension, `apply_batch_after_durable`'s) own
   write-lock critical section, on the *same* guard. So a freeze can
   only happen strictly before a given batch's insert loop begins or
   strictly after it completes — never in the middle of it — meaning
   all N of a batch's entries always land in the *same* MemTable
   generation (all active, or — after a later freeze — all in the same
   now-immutable, now-`Arc`-frozen table together). They can never be
   split across an active/immutable boundary. Immutable MemTables and
   SSTables are read via the *same* recency-ordered, first-hit-wins scan
   every read method already performs (`src/lsm/mod.rs:1783-1803`), so
   this property propagates through flush and (per D33/D9's own
   analysis) Compaction without new reasoning: once a batch's N entries
   are co-located in one MemTable generation, every subsequent physical
   representation of "that generation's data" (an SSTable it flushes
   into, a compacted SSTable it later merges into) carries all N of them
   together, because flush/compaction operate on whole MemTable/SSTable
   contents, never a key-level subset chosen mid-write.
5. **Snapshot pins a seq, not a lock hold.** `Snapshot { seq }`
   (`src/lsm/mod.rs:474`) is a plain `u64` plus a registry handle — it
   does not hold any `RwLock` guard across its lifetime; it only records
   "reads against me must resolve at seq S." Every read performed
   *through* a `Snapshot` still goes through the exact `get_as_of`/
   `range_scan` machinery step 2/3 already covers, passing `S` as `as_
   of_seq` — so the mutual-exclusion argument above applies identically;
   a `Snapshot`'s only extra role is fixing *which* `as_of_seq` a read
   uses, not *how* the read is synchronized against a concurrent write.

**An honest, explicitly-flagged pre-existing characteristic (not
introduced by `write_batch`, not weakened by it)**: `snapshot_seq()`
(`src/lsm/mod.rs:1861`) returns `GroupCommitter`'s `durable_through`
watermark, which — per the already-established, already-certified
write pipeline (`PHASE4A_ARCHITECTURE.md` §5: WAL append → WAL
durability [`durable_through` advances] → MemTable apply → logical
completion) — can advance *before* a specific in-flight caller's own
`apply_after_durable`/`apply_batch_after_durable` step has run. This
means a **different, concurrent** caller invoking `snapshot()` in that
narrow window receives a seq `S` such that a write already durable at
`S` might not yet be present in the MemTable, purely because the
original writer's own call has not returned yet. This is an existing
property of every single-key `put`/`delete` today, not something this
amendment discovers as new or introduces as a regression — `write_batch`
inherits it completely unchanged, and it does **not** contradict the
atomicity claim above: what it means is "a concurrent snapshot taken
while an unrelated write is still logically in-flight may or may not
reflect that write" (ordinary, expected behavior for any system where
"durable" and "returned-to-the-caller" are different instants) — it
never means a reader can see **part** of a batch. The proof above (steps
1–3) establishes the *all-or-nothing* property unconditionally; this
paragraph documents the separate, pre-existing, and orthogonal question
of exactly *when*, relative to an in-flight writer, that all-or-nothing
transition becomes observable to an unrelated concurrent snapshot. No
prior document names this explicitly; it is flagged here rather than
silently assumed away, per this project's own "flag, don't silently
resolve" standard — and is out of scope to change in this increment
(changing it would mean moving `durable_through`'s advancement to *after*
MemTable apply for every write, a `PHASE4A_ARCHITECTURE.md`-preserved-
architecture invariant no directive in this amendment authorizes
touching).

**Alternatives rejected**:
- *A new, dedicated lock for batch application, separate from `active`'s
  existing `RwLock`.* Rejected: unnecessary (the existing lock already
  provides exactly the required exclusion, per the proof above) and
  directly contrary to §23 of the review directive ("Do not introduce a
  new global engine lock unless the actual source analysis proves it is
  required" — this analysis proves the opposite).
- *Take the write lock once per member (N times) instead of once per
  batch.* Rejected: this is exactly the *non*-atomic behavior D9 exists
  to prevent — reintroduces the same interleaving window `put`/`delete`
  called in a loop would have, defeating the entire purpose of the
  primitive.

**Correctness impact**: this section *is* D9's central required proof,
completed. **Security impact**: none new. **Performance impact**: the
write lock is held for a measurably longer single critical section (N
inserts instead of 1) rather than N short ones — net effect on
throughput/contention is exactly what AA.6's multi-op benchmark
measures; no assumption is made about direction without measurement.
**Memory impact**: none beyond AA.1's batch-size bound. **Persistence
impact**: none (this section concerns in-memory visibility, not
durability). **Recovery impact**: recovery re-derives the identical
final MemTable state via the same per-member apply function (AA.1) —
no separate recovery-time visibility argument is needed, since recovery
has no concurrent readers at all (it runs before the engine is opened
for use). **Testing requirements**: the review directive's own §24
concurrent-reader test, built on barriers/fault points (never sleeps) —
a reader thread parked at a fault point *inside* `apply_batch_after_
durable`'s critical section (installed via a test-only hook analogous
to `install_flush_fault_hook`) attempts a concurrent `get`/`range`/
`Snapshot` read and must observe either the full pre-batch or full
post-batch state, asserted over many interleavings, never a partial
one.

---

## AA.4 Same-key operations within one batch

**Decision**: deterministic, **last-operation-in-supplied-order wins**,
for both same-key `Put`/`Put` and `Delete`/`Put` (and, symmetrically,
`Put`/`Delete`) sequences within one batch, and for the identical
question inside a D10 transaction's buffered write-set (a transaction's
write-set collapses to net-effect-per-key *before* being handed to
`write_batch` as its final `ops` slice — see below). Concretely:

- `write_batch(&[Put(A, "x"), Put(A, "y")])` → `A`'s final value is
  `"y"`. Mechanism: both inserts target `MemTable`'s map key `(A, S)`
  (same key, same shared seq, AA.1) in caller-supplied order; the
  second `insert` call overwrites the first at that exact map key —
  ordinary `BTreeMap::insert` semantics (`src/memtable/mod.rs:121`),
  not a new rule invented for this case.
- `write_batch(&[Delete(A), Put(A, "y")])` → `A`'s final state is
  `Put("y")` (live, not a tombstone) — same overwrite mechanism.
- `write_batch(&[Put(A, "x"), Delete(A)])` → `A`'s final state is
  `Tombstone` (deleted) — same mechanism, opposite order.
- **The WAL record is not deduplicated.** Both operations are encoded
  as separate members inside the group frame, in the caller's original
  order — the log stays a truthful record of exactly what was
  requested; only the *MemTable-apply* step (live or replay) collapses
  same-key members to their net effect, and it does so identically both
  times (AA.1's "recovery replays exactly what live apply does"), so
  live behavior and post-crash-recovery behavior can never disagree
  about which value survives.
- **Duplicate operations at the D10 transaction layer**: a
  multi-statement transaction's buffered write-set (`PHASE_RELATIONAL_
  DATABASE_ADR.md` D10) is itself a per-key map (last-buffered-write-per-
  key, by construction of how a session accumulates pending writes) —
  by the time `COMMIT` calls `write_batch`, the write-set already
  contains at most one entry per physical key, so the "duplicate key
  inside one batch" case only actually arises from `write_batch`'s own
  direct callers composing a batch with an intentional same-key sequence
  (e.g., a relational `UPDATE` that internally issues both an old-index-
  entry delete and a new-index-entry put for the *same* index key when
  an indexed column's value doesn't change) — the semantics above cover
  that case identically.

**Reason**: this is the review directive's own required resolution
("Choose deterministic semantics... do not allow ambiguous 'last
operation wins' behavior without defining how that maps to sequence/
version semantics") — satisfied precisely: "last operation wins" is not
an added special case here, it is the direct, mechanical consequence of
(a) one shared seq per batch (AA.1) and (b) ordered application through
the existing, unmodified `MemTable::insert` overwrite behavior. No new
code decides "which write wins" — the existing map semantics decide it,
and this section states exactly what that means in advance.

**Alternatives rejected**:
- *Reject a batch containing duplicate keys outright (validation
  error).* Rejected: this would make a legitimate, common case (an
  `UPDATE` touching both a table row and one of its own secondary-index
  entries, which can coincide with a table-row key only in contrived
  layouts, but a multi-column-family write touching the same *logical*
  row via two physical keys that happen to collide is not something
  `write_batch` itself can distinguish from an intentional same-key
  overwrite at the raw-bytes level it operates on) fail for no
  correctness reason — the last-wins rule is well-defined and cheap to
  compute; there is nothing to protect the caller from.
- *First-operation-wins instead of last.* Rejected: contrary to every
  ordinary SQL/imperative expectation (later statements' effects
  supersede earlier ones within the same unit of work) and to standard
  `BTreeMap`/most in-memory map semantics generally — "last wins" is
  the only choice that requires zero new code, since it is what
  `MemTable::insert`'s overwrite behavior already does unprompted.

**Correctness impact**: net-effect-per-key is well-defined and matches
what a sequential (un-batched) application of the same ops, in the same
order, to the same starting state, would produce — verified directly by
AA.6's differential test (compares `write_batch` against exactly this
serialized reference model). **Security impact**: none. **Performance
impact**: none — no dedup pass is run; the "collapse" is a side effect
of ordinary map insertion, at zero extra cost. **Memory impact**: none
beyond storing all N members in the WAL record (intentional, per the
"WAL record is not deduplicated" decision above — bounded by AA.1's
`max_batch_ops`/`max_record_len` regardless). **Persistence impact**:
the on-disk record is not minimal (both `Put`/`Delete` on `A` are
stored, not just the net effect) — an accepted, explicit tradeoff for
log truthfulness and recovery/live-apply identity, not an oversight.
**Recovery impact**: none beyond AA.1 (recovery applies members in
order via the same function live-apply uses, so it reaches the
identical net effect independently, never by reading a "already
collapsed" summary). **Testing requirements**: exactly the three worked
examples above as direct unit tests, plus inclusion in AA.6's property
test generator (which must be able to generate same-key sequences
specifically, not only distinct-key batches, since a random-key
generator would rarely hit this case by chance).

---

## AA.5 `write_batch` performance requirement

**Decision**: two required, testable — not assumed — performance
properties, to be measured (not merely predicted) before `write_batch`
may be considered complete:

1. **N=1 parity**: `write_batch(&[Put(k, v)])` must show no material
   throughput/latency regression against today's `put(k, v)` (and
   likewise `write_batch(&[Delete(k)])` against `delete(k)`), across
   p50/p95/p99/max latency and throughput, measured with the project's
   existing benchmark methodology (`benches/`, the same harness
   `PHASE_WRITE_ENGINE_PERFORMANCE.md` already used). This is plausible
   *a priori* — the N=1 path differs from today's only by one extra
   `member_count` `u32` (4 bytes) and one extra tag byte per member
   inside the frame, and by going through one new, thin `write_batch`/
   `apply_batch_after_durable` wrapper instead of `put`/`apply_after_
   durable` directly — but plausibility is not evidence; the benchmark
   in AA.5's testing requirements is what actually certifies it.
2. **N>1 throughput benefit, not just correctness**: at N>1, one WAL
   fsync replaces what would otherwise be N independent fsyncs (or,
   compared against N independent `put`/`delete` calls relying on
   `BatchCoordinatorPool`'s own cross-caller group-commit batching, one
   *guaranteed* single-fsync batch replaces a *probabilistic* one that
   depends on concurrent arrival timing) — measured at N ∈ {2, 4, 8, 16,
   32, 64} against an equivalent *serialized* baseline (N sequential
   `put`/`delete` calls from one caller, which do **not** benefit from
   cross-caller group-commit the way concurrent independent callers
   might), recording throughput, p50/p95/p99/max latency, WAL bytes
   written, CPU, and RSS for both.

**Reason**: the review directive's own explicit requirement ("Do not
claim a performance improvement until measured... Record raw
measurements") — this section exists so that requirement is binding on
the implementation before it happens, not decided after the fact by
whatever numbers happen to come out.

**Alternatives rejected**: *Skip N=1 parity benchmarking since the code
change is small and "obviously" cheap.* Rejected outright — this
project's own standing discipline (`[[feedback_rubixdb_rigor]]`:
"measure everything") and the review directive's explicit instruction
both forbid asserting a performance property without measuring it,
regardless of how small the change looks.

**Correctness impact**: none (this is a pure performance requirement).
**Security impact**: none. **Performance impact**: this section *is*
the performance-impact specification. **Memory impact**: RSS is one of
the required measured dimensions, not merely a correctness afterthought
— an implementation that happens to trade higher steady-state RSS for
throughput must have that tradeoff visible in the recorded numbers, not
hidden. **Persistence impact**: WAL bytes written is a required
measured dimension — verifies AA.1/AA.2's "smaller on-disk footprint
than N independent frames" claim empirically rather than leaving it
asserted. **Recovery impact**: none directly (recovery speed is not
this section's concern; it is covered by the crash-test matrix in the
implementation-time document). **Testing requirements**: this entire
section *is* a testing requirement, binding on
`PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`'s implementation and
reported in `PHASE_RELATIONAL_TRANSACTION_STORAGE_RESULTS.md` with raw,
unmassaged measurements — not summary claims alone.

---

## AA.6 Batch size and resource safety

**Decision**: defense-in-depth, enforced at the engine layer
independently of whatever the relational layer (D27) does above it —
`write_batch` must never trust a caller's own limit-enforcement to be
present or correct.

| Limit | Value | Enforced | Where |
|---|---|---|---|
| Max operations per batch | `LsmConfig::max_batch_ops`, default 10,000 (AA.1) | Before any encoding | `write_batch`, first line |
| Max batch bytes (encoded frame) | `WalConfig::max_record_len`, default 64 MiB (reused, unchanged) | During encoding, before allocating beyond the actual bound | `format::encode_frame`'s existing `body_len > effective_max` check |
| Max key bytes / max value bytes | `WalConfig::max_record_len` (reused, unchanged — same bound every individual `put`/`delete` key/value already respects) | Per-field, during encoding | `write_len_prefixed`'s existing `checked_u32_len` check, called once per member |
| Max WAL frame bytes | Same as "max batch bytes" — one bound, not two | — | — |

**Enforcement order** (review directive §8's explicit requirement:
"before allocation... before WAL construction... before engine
mutation"): (1) `ops.is_empty()` and `ops.len() > max_batch_ops` are
checked in `write_batch` itself, before any `WalOpOwned`/`GroupMember`
is constructed; (2) per-field key/value size violations are caught
inside `encode_wal_frame`'s existing `write_len_prefixed` calls, before
the oversized field is appended to the growing `op_body` buffer (the
existing behavior, reused, per AA.2); (3) the whole-frame size check
(`body_len > effective_max`) runs after `op_body` is fully built but
strictly before anything is written to disk or applied to the MemTable
— exactly `format::encode_frame`'s existing ordering, unchanged. At no
point does an oversized batch reach WAL construction (step 3 fails
first if step 1 didn't) or MemTable mutation (which only ever runs
*after* `completion.wait()` confirms durability — an encoding failure
at step 2/3 never reaches the coordinator at all, since `write_batch`
never calls `self.pool.submit` for a batch that failed local
validation).

**Reason**: this is the review directive's own explicit dual framing —
"This is a security requirement as well as a performance requirement" —
and AA.2 already establishes *why* the specific implementation
technique (incremental `Vec` growth on decode, no new pre-check needed
on encode) is sufficient; this section is the policy/limits layer that
sits on top of that mechanism.

**Alternatives rejected**: *A single combined "max batch bytes across
all limits" config knob instead of separate op-count and byte-size
bounds.* Rejected: op count and byte size are genuinely independent
failure modes (10,000 tiny 1-byte ops vs. 2 ops at 32 MiB each) and a
caller/operator reasoning about capacity needs to bound both
independently — collapsing them into one number would hide which
dimension actually mattered when a limit is hit.

**Correctness impact**: none directly (this is exclusively a resource-
bound section). **Security impact**: this section, combined with AA.2's
decode-side incremental-growth rule, is the complete answer to "there
must be no attacker-controlled path to unbounded memory allocation" —
stated here as the policy, implemented there as the mechanism.
**Performance impact**: every check is O(1) or O(N) in already-known
quantities (`ops.len()`, individual `Vec::len()`s) — paid once, before
any expensive work, net performance-*protective* (rejects a doomed
request cheaply instead of doing partial expensive work first).
**Memory impact**: this section's entire content is the memory-impact
bound. **Persistence impact**: none (nothing is written for a rejected
batch). **Recovery impact**: none (a batch that was never durable has
nothing to recover). **Testing requirements**: boundary tests at
exactly `max_batch_ops` (succeeds) and `max_batch_ops + 1` (fails,
`CapacityExceeded`, and — verified via a byte-counter or allocation-
tracking test harness — does not allocate space for the rejected extra
operations); an oversized-single-key test and oversized-single-value
test (existing `put`/`delete` coverage, re-run against `write_batch`
with a 1-member batch to confirm identical rejection behavior); an
oversized-combined-batch test (many legal-sized ops whose sum exceeds
`max_record_len`) confirming `CapacityExceeded` with no partial WAL
write (verified by asserting the WAL file's length is unchanged after
the rejected call).

---

## AA.7 Failure semantics

**Decision**: the external contract is unchanged in kind from today's
`put`/`delete` — it is generalized, not weakened. For every listed
failure point:

| Failure point | Outcome |
|---|---|
| WAL write fails (I/O error before fsync) | `write_batch` returns `Err` (propagated from `GroupCommitter`/`FileWal`, unchanged mechanism); **nothing** was applied to the MemTable — `apply_batch_after_durable` is only ever called *after* `completion.wait()` returns `Ok`, so a WAL-write failure can never reach the apply step at all. |
| WAL fsync fails | Same as above — `GroupCommitter`'s existing poisoning behavior (`PoisonReason::FsyncFailed`) applies unchanged; every waiter on that failed batch (which, for a `write_batch` call, is exactly this one caller's `Group` submission — never a partial subset of its own members, since the whole group is one `WalOpOwned` submitted once) receives `Err`, and the committer is permanently poisoned exactly as it already is today for a single-op fsync failure — no new poisoning logic. |
| WAL frame partially written (crash mid-append) | Not observable as a *caller-visible* failure at all — the caller either already received `Ok` (frame was fully durable, torn-write impossible for an already-fsynced frame) or the process is gone and there is no caller left to report to. On restart, `walk_segment` classifies the partial frame as a torn tail (AA.2) — discarded in full, exactly as a torn single-op frame already is. |
| MemTable apply "fails" | Cannot happen as a distinct failure mode: `apply_batch_after_durable`'s only fallible internal step is `freeze_locked` (`CapacityExceeded` on immutable-backlog exhaustion) — and, per the existing, twice-ratified contract (`PHASE4A_FAILURE_MODEL.md` §2, `PHASE4A_ADR.md` ADR-P4A-5, cited directly in `apply_after_durable`'s own doc comment), a `CapacityExceeded` from `freeze_locked` does **not** roll back the insert(s) that already happened in the same critical section — the write(s) are durable and now live in what remains the active MemTable; only the *freeze* is refused. This existing, accepted contract extends unchanged to the batch case: if freezing after a batch's N inserts hits the immutable-backlog limit, all N inserts are still fully applied and visible (durable + applied, satisfying AA.3's atomicity claim completely), and `write_batch` returns `Err(CapacityExceeded)` purely as backpressure signal to the caller, *not* as a report that the write itself failed — this must be documented at the `write_batch` call site precisely as clearly as `put`/`delete`'s existing doc comment already documents it for the single-op case, so a caller cannot mistake "the batch is durable and applied, but the resulting freeze was deferred" for "the batch failed." |
| Shutdown while a batch is pending | `ShutdownReport`/`GroupCommitter::shutdown` (`src/wal/group_commit.rs:830`) already generalizes without change: `highest_assigned_seq` (from `FileWal::next_seq() - 1`) already reflects a `Group` frame's one assigned seq exactly as it reflects any other frame's, and `has_undurable_pending()` (`highest_assigned_seq > durable_through`) already correctly reports "yes, something is still pending" for an in-flight batch precisely as it does for an in-flight single op — no new field, no new logic. |
| Worker/coordinator thread failure | `BatchCoordinatorPool`'s existing `PoolState::Failed` handling (`src/execution/batch_coordinator.rs:409-418`, returning `EngineError::WalUnavailable`) applies to a `write_batch` submission exactly as it does to a `put`/`delete` submission — `submit` doesn't inspect the *kind* of `WalOpOwned` it's rejecting. |

**The binding invariant, restated**: the external contract must never
report success unless the batch satisfies its durability and visibility
guarantee — satisfied structurally, not by careful case-by-case
bookkeeping, because `write_batch` only ever returns `Ok(seq)` *after*
`apply_batch_after_durable` has completed (which itself only ever runs
after `completion.wait()` confirms full-batch WAL durability) — there is
no code path that returns `Ok` before both steps have finished, and no
code path where some of the batch's members are durable/applied while
others are silently dropped, since the WAL frame and the MemTable-apply
critical section each treat the whole batch as one indivisible unit
(AA.1, AA.3).

**Reason**: directly resolves the review directive's explicit demand
("Do not create a path where: some operations are durable / caller
receives success / other operations are silently lost") by showing,
failure point by failure point, that no such path exists in the actual
design — not merely asserting it.

**Alternatives rejected**: *A partial-success return type (e.g., `Result<
Vec<Result<u64>>>`, reporting per-member outcomes).* Rejected outright —
directly contradicts D9's atomicity guarantee and the review directive's
explicit prohibition; a batch is atomic, so "partial success" is not a
meaningful outcome to have a type for.

**Correctness impact**: this section is the exhaustive case analysis
D9/AA.3's atomicity claim requires to be considered proven against real
failure modes, not just the happy path. **Security impact**: none new.
**Performance impact**: none (failure paths are not the hot path).
**Memory impact**: none beyond AA.6. **Persistence impact**: covered
per-row above; net effect — a batch is either fully durable or not
durable at all, never partially. **Recovery impact**: covered per-row
above; net effect — recovery only ever sees a `Group` frame as either
fully present (post-CRC-validated) or fully absent (torn, discarded) —
never a decodable-but-incomplete one, since decoding itself only
succeeds once every declared member has been read and CRC-validated as
part of the whole frame. **Testing requirements**: the review
directive's own §25 crash-test matrix — fault-injected at each of
"before batch WAL write," "during header," "during operation encoding,"
"during key/value write," "during checksum," "during WAL fsync,"
"after WAL durable before MemTable apply," "after apply," "before
response" — restart, recover, and assert exact expected state at every
point, using the existing fault-hook infrastructure
(`install_fsync_fault_hook`, `AbortPoint` hooks in `FileWal::append`)
extended to cover the new `Group` encode/apply steps specifically.

---

## AA.8 Concurrent writers / `BatchCoordinator` integration

**Decision**: **no new batching mechanism.** `write_batch` is a new
*caller* of the existing, unmodified `BatchCoordinatorPool`/
`GroupCommitter` — it submits exactly one `WalOpOwned::Group(members)`
via the same `pool.submit(op: WalOpOwned) -> Result<Completion>`
(`src/execution/batch_coordinator.rs:393`) every `put`/`delete` already
calls, and waits on the returned `Completion` the same way. Every
property the review directive asks about is therefore inherited, not
re-implemented:

- **Ordering among concurrent `write_batch` calls, and between `put`/
  `delete` and `write_batch`**: unchanged — the coordinator's queue
  (`guard.entries: VecDeque<QueueEntry>`) does not distinguish `Put`/
  `Delete`/`Group` `WalOpOwned` variants; every submission is queued,
  assigned a `RequestId`, and eventually leader-batched into one or more
  fsyncs in submission order, exactly as today. A `Group`'s single
  `next_seq` assignment (AA.1) happens at the same point in the same
  sequence every other submission's assignment does.
- **Fairness, queue limits, shutdown behavior, failure propagation**:
  unchanged — `queue_capacity`/`max_queued_bytes` backpressure
  (`src/execution/batch_coordinator.rs:421-422`) already accounts for a
  submission's estimated size via `estimate_frame_len` (AA.2 extends
  this function to `Group`, so a large batch correctly consumes more of
  the byte budget than a small one — no special-casing needed); shutdown
  (`PoolState::Draining`/`Stopped`) and coordinator failure
  (`PoolState::Failed`) rejection paths apply identically regardless of
  `WalOpOwned` variant.
- **Sequence monotonicity**: preserved trivially — `FileWal::next_seq`
  is incremented by exactly 1 per `append()` call regardless of which
  `WalOp` variant that call encodes (AA.1), so the *count* of
  seq-assigning events, and their strict monotonic order, is unaffected
  by whether some of those events are now `Group`s covering multiple
  logical operations instead of one.
- **No deadlock, no starvation**: no new lock is introduced (AA.3's
  proof already establishes `apply_batch_after_durable` reuses the
  existing `active` `RwLock` without acquiring any additional lock), and
  the coordinator's own existing leader-election/queue-fairness
  machinery (unmodified) already carries whatever deadlock/starvation-
  freedom properties it has today — nothing about `write_batch`
  introduces a new acquisition order, a new lock, or a new blocking
  wait that could interact with the existing ones in a new way.

**Reason**: directly satisfies the review directive's own instruction
("Do NOT create a second independent batching mechanism if the existing
infrastructure can safely be extended") — verified here, concretely,
that it *can* be safely extended, by tracing exactly which existing
function (`submit`) is reused and exactly which properties (ordering,
fairness, monotonicity, deadlock/starvation-freedom) transfer
unconditionally because no new synchronization is added.

**Alternatives rejected**: *A dedicated queue/coordinator specifically
for `write_batch` calls, separate from `BatchCoordinatorPool`.*
Rejected outright — this is precisely the "second independent batching
mechanism" the review directive forbids, and would additionally require
its own, independently-argued ordering/fairness/monotonicity/deadlock
analysis instead of inheriting one already-certified.

**Correctness impact**: sequence monotonicity (a correctness
requirement for the whole `(key, seq)` versioning model) is preserved
by construction, as shown above. **Security impact**: none new — the
same trust boundary (`submit` is not exposed raw over any external API,
D25) applies to `Group` submissions exactly as to `Put`/`Delete`.
**Performance impact**: a `Group` submission can be leader-batched
alongside concurrent single-op submissions in the *same* underlying
fsync exactly as today — `write_batch` does not opt out of cross-caller
group-commit amortization; AA.5 measures the net effect. **Memory
impact**: `estimate_frame_len`'s extension (AA.2) ensures the existing
`max_queued_bytes` backpressure bound correctly accounts for a large
`Group`'s real queued size — no new unbounded-queue risk. **Persistence
impact**: none beyond AA.1/AA.2. **Recovery impact**: none beyond AA.1.
**Testing requirements**: a concurrency test submitting a mix of `put`,
`delete`, and `write_batch` calls from many threads simultaneously,
asserting (a) every returned seq is unique and strictly increasing in
submission-completion order is *not* required (concurrent submissions
may complete in any relative order — only that no seq is ever reused or
assigned twice) and (b) the final MemTable state matches a serialized
reference application of the same operation set in *some* valid
interleaving (not a fixed one) — the standard concurrency-test shape
this project's existing group-commit test suite already uses, extended
to include `Group` submissions in the mix.

---

## AA.9 Security review for `write_batch`

**Decision**:

- **Trust boundary unchanged**: `write_batch` remains an internal,
  `LsmEngine`-level Rust API — never exposed as a raw, arbitrary-batch
  HTTP endpoint. The only caller of `write_batch` in any future
  relational-layer code is the DML/DDL/transaction-commit execution
  path, which has already passed D24 (authentication) and D25
  (authorization) checks *before* it ever constructs a `WriteOp` slice.
  `write_batch` itself performs no authentication/authorization — it is
  not its job, exactly as `put`/`delete` perform none today; the engine
  trusts its Rust-level caller, and the relational layer's execution
  path is what is responsible for ensuring only an authorized,
  validated operation ever reaches it (AA.10 makes this explicit for
  catalog rows specifically).
- **Engine-level validation** (defense in depth, independent of whatever
  the caller already checked): operation count (AA.1's `max_batch_ops`),
  key size, value size (AA.6's per-field bounds, reused unchanged from
  `put`/`delete`), and encoded batch size (AA.2/AA.6, reused
  `max_record_len`) are all validated by the engine itself, every call,
  regardless of caller — never solely relying on an upstream layer
  having already checked.
- **No logging of keys, values, or credentials**: `write_batch`
  introduces no new logging whatsoever — neither `println!`/`eprintln!`
  nor any structured-log call anywhere in its implementation touches
  operation contents. The one existing `eprintln!` this area of the
  code contains (`freeze_locked`'s storage-state transition message,
  `src/lsm/mod.rs:1721-1726`) logs only configuration numbers
  (`max_immutable_memtables`) — confirmed by direct inspection it never
  interpolates key/value bytes — and is unchanged by this addition.
- **Errors never expose filesystem paths or raw I/O internals**: every
  new error path (`CapacityExceeded`, the new `InvalidArgument`, and
  every propagated `Io`/`Corruption`/`WalUnavailable`) reuses the
  existing `EngineError` variants and their existing, already-audited
  `Display` implementations (`src/error.rs:61-82`) unchanged — no new
  variant added here (AA.10 defines exactly one, `InvalidArgument`, and
  its `Display` arm follows the same "detail message only, no raw OS
  error text, no path" discipline every existing variant already
  follows).

**Reason**: this is the review directive's own explicit security-review
checklist for the new primitive, resolved item by item against the
concrete design above rather than asserted in the abstract.

**Alternatives rejected**: *Add a lightweight authorization check inside
`write_batch` itself, as defense in depth against a future relational-
layer bug that forgets to check D25 first.* Rejected: `write_batch`, at
the engine layer, has no concept of "database," "schema," "table," or
"principal" — those are relational-layer concepts (D1/D24/D25) that do
not exist at this layer of the stack; inventing a partial, engine-level
authorization check here would either duplicate the relational layer's
real authorization model badly (wrong layer, wrong information
available) or be a no-op stub that creates false confidence. The
correct defense in depth is AA.10's explicit rule (only the internal
catalog/DDL service may construct a `WriteOp` batch targeting `system.*`
rows) enforced at the relational-execution layer, where the necessary
information (which rows are catalog rows, who the caller is) actually
exists.

**Correctness impact**: none beyond what's already covered. **Security
impact**: this section is the security-impact specification. **Performance
impact**: none beyond AA.6's already-counted validation cost.
**Memory impact**: none beyond AA.6. **Persistence impact**: none.
**Recovery impact**: none. **Testing requirements**: the review
directive's own §30 security test list — oversized key, oversized
value, oversized batch, oversized WAL frame, empty batch, malformed
input (fuzzed), duplicate operations, shutdown during batch, concurrent
callers — each asserting no panic, no allocation explosion (verified via
a bounded-allocation test harness, not merely "it didn't crash"), no
partial success (AA.7), no sensitive logging (grep-based test asserting
no key/value byte sequence used in the test appears in captured log
output), and no filesystem-path leakage in any returned `Err`.

---

## AA.10 Catalog security: system catalog rows are not ordinary user DML targets

**Decision**: a new rule, supplementing D1 (catalog architecture), D8
(constraints), and D25 (authorization) — none of which previously
stated this explicitly:

> **`system.databases`, `system.schemas`, `system.tables`, `system.
> columns`, `system.indexes`, `system.constraints`, and `system.grants`
> may only ever be mutated by the internal catalog/DDL execution path**
> — the same trusted, server-side component that already implements
> `CREATE DATABASE`/`CREATE SCHEMA`/`CREATE TABLE`/`CREATE INDEX`/DDL
> generally (D13). **A user-issued `INSERT`/`UPDATE`/`DELETE` statement
> targeting a `system.*` table by name must be rejected by the binder**
> (D15) before it ever reaches the executor or `write_batch` — the same
> binder step that already resolves a statement's target table against
> the catalog is exactly where "is this target a system table, and is
> this a DDL-originated write or a user DML statement" is already
> knowable, at zero extra catalog lookups (the binder already looked
> the table up to resolve it).

- **Read authorization for system metadata**: unchanged from D25's
  existing per-object authorization model — a `SELECT` against
  `system.tables` (e.g., to implement `\dt`/information-schema-style
  introspection) is checked exactly like a `SELECT` against any other
  table, using the querying principal's own grants, **except** that
  `system.grants` itself is additionally restricted to `Admin`-role
  principals and the row-subject principal's own grants only (a
  `Reader` principal must not be able to enumerate every other
  principal's full grant set) — a narrower, explicit carve-out on top
  of D25's general model, not a separate authorization system.
- **Server-side enforcement, never frontend/CLI**: restated explicitly
  because the review directive calls it out specifically for this case
  — the binder-level rejection above is the *only* enforcement point;
  the CLI/frontend may (as a usability nicety) simply not offer a
  `system.*` table in an "editable tables" picker, but that omission
  has zero security weight and this design does not rely on it.

**Reason**: D1 established the catalog *storage* model (ordinary rows
under a reserved prefix) but never previously stated the *access*
restriction this implies — the review directive correctly identifies
this as a real gap, not a restatement: without this rule, D1's own
"self-hosting" design (catalog rows are ordinary rows in the same
keyspace) would otherwise let any principal with `INSERT`/`UPDATE`/
`DELETE` privilege on *some* table construct a request that
happens to name `system.tables` and directly corrupt catalog
invariants D8/D13 depend on holding.

**Alternatives rejected**:
- *Enforce this via a `CHECK`-constraint-style rule on the system tables
  themselves, rather than a binder-level rejection.* Rejected: D8's
  `CHECK` constraints are a *table-owner-authored*, per-column-value
  validation mechanism — repurposing it to encode "no user may write
  here at all" conflates two different concerns (value validity vs.
  who-may-write-at-all) and would be checked too late (after the binder
  has already resolved and authorized the statement as an ordinary
  table write) to be the actual security boundary; a `CHECK` failure is
  also a normal, retryable, user-facing outcome, which "you may never
  write to this table" categorically is not.
- *Store the catalog in a genuinely separate physical namespace with
  its own, entirely different write path (bypassing `write_batch`
  altogether for catalog writes).* Rejected: reintroduces exactly the
  "two different atomicity mechanisms" problem D9/D1 already avoided by
  making the catalog self-hosting — catalog DDL needs the *same*
  atomic multi-row commit `write_batch` provides for ordinary DML
  (D1's own "Catalog mutation atomicity depends on the same new
  `write_batch` primitive as DML"), so routing catalog writes through a
  different mechanism would mean building and separately certifying a
  second atomicity story for no benefit.

**Correctness impact**: this is what makes D8 (constraints) and D13
(DDL durability)'s invariants actually hold against an *adversarial*,
not just a well-behaved, DML caller — without this rule, a malicious or
buggy DML statement could leave the catalog internally inconsistent
(e.g., a `system.columns` row with no corresponding `system.tables`
row) in a way no other decision in this document accounts for or
defends against. **Security impact**: this section is a direct
authorization-boundary specification — the binder is the enforcement
point, D25's existing checked-before-execution discipline is reused
unchanged, and the `system.grants` read carve-out prevents privilege
enumeration. **Performance impact**: the system-table-name check adds
one comparison against a small, fixed, already-resolved set of table
identities at a point (binder resolution) that already looked the
target table up — no measurable added cost. **Memory impact**: none.
**Persistence impact**: none beyond D1/D9 (unchanged). **Recovery
impact**: none beyond D12 (unchanged). **Testing requirements**: a
direct test that a `Reader`- or `Admin`-role principal's `INSERT`/
`UPDATE`/`DELETE` statement naming `system.tables` (or any other
`system.*` table) by name is rejected at bind time with a clear,
typed error — never silently succeeding, never reaching `write_batch`
at all (verified by asserting no WAL record is written for the rejected
attempt); a `system.grants`-specific test confirming a non-`Admin`
principal cannot read another principal's grant rows; a positive test
confirming the internal DDL path itself *can* still write `system.*`
rows (the rule is "user DML cannot," not "nothing ever can").

---

## AA.11 System catalog table-set consistency (documentation correction)

**Finding**: `PHASE_RELATIONAL_DATABASE_ADR.md` D1 lists the catalog's
complete system-table set as seven tables, including `system.grants`
(D1's own text: `` `system.databases`, `system.schemas`, `system.tables`,
`system.columns`, `system.indexes`, `system.constraints`,
`system.grants` ``), and D25 independently relies on `system.grants`
existing. `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §2, however,
listed only six, omitting `system.grants` — the exact documentation
disagreement the review directive's §13 flags.

**Correction applied** (direct edit, not append-only, per the note at
the top of this amendment — a factual-consistency fix to the
Architecture document, not a reversal of any ADR decision):
`PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §2's system-table list is
updated to include `system.grants`, matching D1 exactly, word for word.
Before: `` `system.databases`, `system.schemas`, `system.tables`,
`system.columns`, `system.indexes`, `system.constraints`) ``. After:
`` `system.databases`, `system.schemas`, `system.tables`,
`system.columns`, `system.indexes`, `system.constraints`,
`system.grants`) ``. No other content in that section changes.

**Reason**: the review directive's explicit instruction ("Make the ADR
and architecture document agree on the complete authoritative catalog
table set. Do not leave documentation disagreement") — resolved by
making the Architecture document match the ADR (the ADR's D1/D25 are
the more detailed, more recently cross-referenced treatment of the
catalog's full table set, so the Architecture document's summary list
is what was incomplete, not the ADR).

**Testing requirements**: none (documentation-only); verified by a
direct text search confirming both documents now list the identical
seven-table set.

---

## AA.12 Reserved-namespace upgrade safety: deterministic compatibility preflight

**Decision**: supplementing D32 (which already establishes *that* the
`/v1/kv` flat API must reject client-supplied keys in the reserved
`0x00`/`0x01` prefix range once relational mode is enabled), this
amendment adds the missing *procedure* for the moment of enabling
relational mode on a deployment that may already hold flat-KV data:

- **A deterministic compatibility preflight command** (CLI/API-level,
  specified fully — not merely implemented — in
  `PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`'s successor documents,
  since it depends on the relational layer's own tooling which does not
  exist yet; this decision fixes its *contract* now): a read-only scan
  (`range_scan` over `[0x00, 0x02)`, the certified engine's own existing
  operation, no new engine capability required) that reports **the
  exact count and a bounded sample of colliding keys**, or a clean
  "zero collisions" result.
- **Relational mode must refuse to start** if the preflight has not
  been run and passed (a persisted, explicit "preflight passed as of
  seq S" marker — itself an ordinary catalog-adjacent row, written only
  by the preflight tool, checked by the relational service's own
  startup path) — **not** merely documented as a recommended step an
  operator might skip.
- **No silent rewrite, hide, or migration of colliding data, ever** — if
  the preflight finds collisions, relational mode does not start;
  the operator must explicitly resolve the collision (moving/deleting
  the colliding flat-KV data via the existing, ordinary `/v1/kv`
  `DELETE`, or choosing not to enable relational mode on that
  deployment) and re-run the preflight.
- **Upgrade procedure** (documented here as the binding contract; full
  operator-facing text belongs in a future release note, per D32's own
  testing requirement): (1) back up the data directory (standard
  operational practice, not a new mechanism this project builds); (2)
  run the preflight command against the running engine; (3) on zero
  collisions, the preflight writes its passed-marker and relational mode
  may be enabled; (4) on any collisions, resolve them explicitly and
  re-run from step 2 — no step silently proceeds past a detected
  collision.
- **Failure behavior**: preflight failure (a collision found, or the
  preflight itself erroring, e.g. on an I/O failure mid-scan) leaves the
  deployment in its prior, unmodified flat-KV-only state — the preflight
  is read-only end to end (a `range_scan`, no writes) except for its own
  final passed-marker row, written only on a clean zero-collision
  result.

**Reason**: D32 established *why* the compatibility break is necessary
and unavoidable but did not specify *how* an operator safely discovers
whether they're affected before relational mode silently starts
rejecting their existing traffic — the review directive's §14
correctly identifies this as the missing operational half of D32's
otherwise-complete correctness argument.

**Alternatives rejected**: *Let relational mode start regardless, and
rely on D32's existing `/v1/kv` rejection to surface the problem
reactively, per-request, as it's discovered.* Rejected: this would mean
an operator's *first* signal of a collision is a production request
failing after the upgrade already happened — exactly the "confused
operator after the fact" outcome D32 itself already rejected for the
compatibility break in general; the preflight moves that discovery to
before the irreversible step, deterministically, in one pass, rather
than reactively and incompletely (a request-driven discovery might
never happen to touch every colliding key).

**Correctness impact**: this is what makes D32's isolation guarantee
actually operator-verifiable before relational mode is trusted with
real data, not just true in principle. **Security impact**: prevents an
upgrade from silently degrading into a state where some client's
pre-existing data is unreachable (a correctness/availability issue, not
confidentiality/integrity, but still a "never solve by silently
weakening guarantees" case). **Performance impact**: one bounded
`range_scan` over a two-byte-prefix range, run once at upgrade time
(not on any hot path) — cost scales with the number of colliding keys
found (typically zero), not with total data size beyond the scan's own
inherent linear cost, which is already the certified Read Engine's own
established range-scan performance characteristic, unchanged.
**Memory impact**: bounded — the reported sample of colliding keys is
capped (a fixed bound, e.g. the first 1,000 collisions), never
unbounded, even if the true collision count is very large. **Persistence
impact**: exactly one new row (the passed-marker), written once, on a
clean result. **Recovery impact**: the passed-marker is an ordinary
catalog-adjacent row — recovered via the existing, unmodified WAL/
Manifest/SSTable recovery path (D12), no new recovery procedure.
**Testing requirements**: a preflight-finds-collisions test (seed flat-
KV data in the reserved range, confirm relational mode refuses to
start); a preflight-clean test (confirm the passed-marker is written and
relational mode starts); a preflight-is-read-only test (confirm no
mutation occurs to any key outside the passed-marker itself, even when
collisions are found); a restart-preserves-passed-marker test.

---

## AA.13 SQL execution model (documented now, not implemented)

**Decision**: a bounded, dedicated blocking-task executor model for SQL
execution — chosen now, in writing, ahead of any executor
implementation (D17), because the review directive correctly identifies
that the API's existing `axum`/`tokio` async runtime (confirmed by the
Phase 0 audit: synchronous engine calls made directly on async worker
threads today, justified there only by the sub-millisecond latency of
today's raw KV operations) cannot be extended naively to a SQL executor
whose operations (large scans, sorts, joins, aggregations) have no such
latency ceiling.

- **Worker model**: a bounded pool of dedicated OS threads for SQL
  execution (Rust's `std::thread` or `tokio::task::spawn_blocking`
  backed by a bounded pool, not the default unbounded `spawn_blocking`
  pool — the distinction matters specifically because the default pool
  is sized for occasional blocking calls, not as a concurrency-limiting
  admission-control mechanism), separate from the async runtime's
  request-handling worker threads — so one large `SELECT`/`JOIN`/`SORT`/
  `GROUP BY`/range scan can run for as long as it needs without starving
  the async runtime's ability to accept and route *other* HTTP requests
  (including unrelated small queries, health checks, and — critically —
  the cancellation request for the very query monopolizing a worker).
- **Maximum concurrent SQL executions**: the dedicated pool's thread
  count is the hard admission-control bound — a configurable limit
  (default TBD by the implementation-time document, informed by
  measured hardware characteristics, not guessed now), enforced by the
  pool itself rejecting/queuing beyond capacity, mirroring D27's general
  "maximum concurrent transactions" resource limit but specifically for
  in-flight query *execution*, not transaction *duration*.
- **Query deadline**: every SQL execution is assigned a deadline at
  admission (D27's "maximum query runtime," default 30s) — enforced via
  cooperative cancellation checkpoints (next bullet), not preemptive
  thread termination (Rust has no safe mechanism for the latter, and
  forcibly killing a thread mid-execution while it holds engine-level
  read state — e.g., mid-`range_scan` — is exactly the kind of
  "resource cleanup" hazard §15 warns about).
- **Cooperative cancellation checkpoints**: every streaming operator
  (D17's own bounded-memory streaming executor design) checks its
  cancellation token at each unit of iteration it already performs
  (each row/batch pulled from a child operator) — no new iteration
  boundary is invented; cancellation-checking is layered onto the
  iteration structure D17 already requires for bounded-memory
  execution, at zero additional per-row cost beyond one atomic load.
- **Behavior after cancellation**: the query's dedicated worker thread
  unwinds its own operator tree cleanly (each operator's `Drop`
  releases whatever engine-level resource it held — an open `Snapshot`,
  an in-progress `range_scan` iterator — exactly as it would on any
  other early-return path), and the client-visible result is a typed,
  documented "query cancelled" error (D27's resource-limit error
  taxonomy), never a raw timeout or a silently-truncated partial result
  set.
- **Resource cleanup**: guaranteed by ordinary Rust `Drop` semantics
  over the operator tree — no manual cleanup registry is needed, since
  every engine-level handle a query execution can hold (`Snapshot`,
  transaction write-set buffer) already has an RAII `Drop` impl
  (`Snapshot`'s existing `Drop` releasing its `SnapshotRegistry` entry,
  D10's transaction write-set living in ordinary owned memory).
- **Shutdown behavior**: the SQL executor pool drains in-flight queries
  up to a bounded timeout (mirroring `GroupCommitter::shutdown`'s own
  `SHUTDOWN_DRAIN_BOUND` pattern — a real bound, never an indefinite
  wait), then cancels whatever remains via the same cooperative
  mechanism above, consistent with the API layer's existing graceful-
  shutdown lifecycle (Phase 0 audit's confirmed existing pattern).

**Reason**: the review directive's own explicit instruction — resolve
this *architecturally* now, before D17's executor is implemented,
specifically so the executor is designed against a bounded-concurrency,
cancellable execution model from its first line of code rather than
retrofitted onto one after the fact discovers async-worker starvation
in practice.

**Alternatives rejected**:
- *Run SQL execution directly on the async runtime's own worker
  threads, exactly as today's raw KV operations do.* Rejected
  explicitly — this is precisely the "must not allow a large SELECT/
  JOIN/SORT/GROUP BY/range scan to monopolize an async worker
  indefinitely" failure mode the review directive names outright; it
  was an acceptable, explicitly-justified choice for today's
  sub-millisecond raw KV operations and is not an acceptable one for
  unbounded-duration SQL execution.
- *Preemptive thread termination for cancellation/deadline enforcement.*
  Rejected: unsafe in Rust generally, and specifically hazardous here
  because a terminated thread mid-`range_scan` could leave an engine-
  level resource (a held `Snapshot`) never released, which — per D9's
  own oldest-live-snapshot tracking — could indefinitely block
  Compaction from reclaiming space no longer needed by any *other*
  live reader; cooperative cancellation with guaranteed `Drop`-based
  cleanup avoids this entirely.

**Correctness impact**: none directly (this section governs scheduling
and cancellation, not result correctness) — except that cooperative
cancellation must never allow a query to be cancelled *after* it has
already started applying a `write_batch` commit but before that
commit's `Ok`/`Err` is delivered back to the caller (a cancellation
checkpoint must never be placed inside `write_batch`'s own atomic
apply step — D9/AA.1-AA.9's atomicity guarantee is unconditional and
must not be made cancellable). **Security impact**: bounded concurrent-
execution admission control is itself a denial-of-service defense (D27)
— an unauthenticated or low-privilege flood of expensive queries cannot
exceed the dedicated pool's fixed size, regardless of how many HTTP
requests arrive. **Performance impact**: this section's entire content
is a performance/scheduling design; its own effectiveness is measured,
not assumed, once D17's executor exists (deferred to that
implementation phase's own benchmarking, per this project's "no
speculative optimizations without measured evidence" standard — this
section fixes the *architecture*, not a specific pool-size constant).
**Memory impact**: bounded by the dedicated pool's thread count (a hard
cap on concurrent query memory footprint, each individually bounded by
D17/D27's operator memory limits). **Persistence impact**: none.
**Recovery impact**: none (an in-flight, uncommitted query at crash time
loses nothing durable — D10's transaction model already established
this for the general case). **Testing requirements**: deferred to D17's
own implementation-time test plan, but bound by this section's
contract: a long-running-query-does-not-starve-unrelated-requests test,
a deadline-enforcement test, a cancellation-cleans-up-resources test
(asserting a cancelled query's held `Snapshot` is released promptly, not
leaked), and a shutdown-drains-then-cancels test.

---

## AA.14 Reserved for future use

*(Intentionally left as a numbering placeholder — no content. AA.15/
AA.16 below correspond to the review directive's §16/§17; this gap
keeps that correspondence easy to audit against the directive's own
numbering without renumbering already-written sections above.)*

---

## AA.15 Performance model correction: PK/secondary-index lookup wording (D6/D28)

**Finding**: D6 (Primary keys) states, in its Performance impact
sentence: "PK point lookup = `O(1)` amortized bloom-filtered engine
`get`" — technically qualified with "amortized," but positioned in a
way a reader could take as an unconditional per-lookup guarantee. D28's
complexity table states the same thing more carefully (`O(1) amortized
(bloom+block-index)`, with the caveat — "live SSTable count per table
(certified Read Engine's own established scaling law)" — placed in a
separate table column rather than beside the complexity claim itself).
The review directive's §16 is correct that this invites misreading: PK
lookup cost is **not** unconditionally `O(1)` — it is `O(1)` *in
matching-row count* (a point lookup touches at most one logical row),
but the actual I/O/CPU cost still scales with **the number of live
SSTables a lookup must consult** before a hit or an exhaustive
bloom-negative result is reached (`get_as_of`'s own documented merge
order, `src/lsm/mod.rs:1767-1806`: active → immutables → SSTables
newest-to-oldest, `sstables_consulted` incremented per table visited) —
exactly the certified Read Engine's own established, already-measured
scaling behavior, never contradicted by D6, but stated imprecisely
enough beside the `O(1)` claim itself to read as stronger than intended.

**Correction applied** (direct edit to D6's Performance-impact
sentence and to D28's table, per this amendment's top-of-file note —
tightening imprecise wording, not reversing the underlying claim,
which was never "guaranteed O(1) regardless of SSTable count" in
substance): every `O(1)`/`O(log n)` complexity claim touching engine
`get`/index lookup in D6, D7, D9(architecture summary), and D28's table
is restated as **"O(1)/O(log n) in matching-row count; actual I/O/CPU
cost also scales with the number of live SSTables consulted, per the
certified Read Engine's own read-amplification model — bounded, not
eliminated, by Compaction (D9/`ADR-COMPACTION-001`) keeping that count
low"** — the same substantive claim D28's table already made in its
caveat column, now stated adjacent to every complexity number itself
rather than only in a separable column a reader could skip.

**Reason**: the review directive's explicit instruction — "Do not
describe PK lookup as mathematically strict O(1)... Similarly correct
secondary-index and join complexity so matching-row cost and LSM
amplification are represented honestly... The performance document must
become the benchmark contract." A benchmark contract that a future
implementer/benchmarker could satisfy by measuring only a freshly-
compacted, single-SSTable state (technically "O(1)," misleadingly so)
would not actually validate the claim under realistic, multi-SSTable
conditions — precise wording here is what makes AA.5/D29's benchmark
methodology measure the right thing.

**Alternatives rejected**: *Leave D6/D28 as written, and add the
clarification only in this amendment, without touching the original
text.* Rejected: the review directive explicitly asks for the wording
itself to be corrected ("Correct any wording that could be interpreted
as an unconditional database-level guarantee" — an instruction to edit,
not merely to annotate elsewhere), and a reader encountering D6/D28 in
isolation (without having also read this amendment) would otherwise
still be misled — the whole point of the correction is that it must be
visible exactly where the original imprecise claim was.

**Correctness impact**: none (no behavior changes — this is a wording
precision fix). **Security impact**: none. **Performance impact**: this
correction is itself the performance-impact specification made precise
— it directly shapes what AA.5/D29's benchmark methodology must measure
(multi-SSTable-count conditions, not only a freshly-compacted single-
SSTable baseline) to actually validate the (now correctly stated) claim.
**Memory impact**: none. **Persistence impact**: none. **Recovery
impact**: none. **Testing requirements**: D29's benchmark methodology
must include a measurement point at a realistic, non-trivial live-
SSTable count (not only immediately after a fresh compaction), so the
now-precise complexity claim is validated under the conditions it
actually describes.

---

## AA.16 Index terminology correction: "ordered LSM-backed index," not "B-tree"

**Finding**: `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §6 (Indexes)
states: "All are real, persistent B-tree-shaped range structures built
directly on the certified engine's own ordered keyspace." No B-tree
implementation exists anywhere in this codebase, and none is planned by
this ADR — the actual physical structure is the certified LSM engine's
own ordered keyspace (MemTable `BTreeMap` + SSTable sorted blocks +
merge-on-read across sources), which provides ordered range iteration
without being a B-tree in the data-structure sense (no in-place page
splits/merges, no B-tree-specific balancing, no single on-disk B-tree
node format) — the review directive's §17 correctly identifies "B-tree"
as an inaccurate physical-structure claim, even though the *logical*
property being described (ordered, range-scannable) is correct.

**Correction applied** (direct edit to `PHASE_RELATIONAL_DATABASE_
ARCHITECTURE.md` §6, per this amendment's top-of-file note): "B-tree-
shaped range structures" is replaced with **"ordered LSM-backed range
structures"**, and the sentence is extended to state explicitly where
the ordering comes from: "All are real, persistent, ordered LSM-backed
range structures — their ordering is inherited directly from the
certified engine's own ordered keyspace (MemTable `BTreeMap` iteration
order + SSTable sorted-block layout + the certified merge-on-read across
sources), not from a B-tree or any other independent balanced-tree
implementation. If a future phase wants an actual B-tree-backed
secondary structure (e.g. for a workload profile the LSM-ordered
approach serves poorly), that is a new physical index type requiring
its own dedicated ADR — not something this design provides today."
Every other occurrence of "index" in the Architecture document and ADR
already correctly avoids claiming a B-tree (confirmed by a full-text
search of both documents finding exactly this one instance) — no other
edit is needed.

**Reason**: the review directive's explicit instruction — "Do not call
the physical secondary index a 'B-tree' unless an actual B-tree
implementation exists... If a future B-tree implementation is desired,
it requires a separate ADR." Precision here matters beyond pedantry:
"B-tree" carries specific, well-known performance connotations
(in-place update, no LSM-style read/write amplification tradeoffs) that
do not apply to this design — leaving the claim uncorrected would set a
future implementer or benchmarker up to expect B-tree performance
characteristics from a structure that does not have them.

**Alternatives rejected**: *Keep "B-tree-shaped" but add a footnote
clarifying it's a metaphor, not a literal claim.* Rejected: the review
directive is explicit that the term itself must not be used for a
non-B-tree structure, and a footnoted metaphor is exactly the kind of
"technically qualified but likely to be misread" wording AA.15 already
argues against in the adjacent case — better to use the accurate term
outright.

**Correctness impact**: none (wording-only; the underlying design —
D7's actual index architecture — is unchanged). **Security impact**:
none. **Performance impact**: none directly, but removes a misleading
performance connotation (B-tree-style in-place-update cost model) that
does not apply — this design's real performance characteristics are
D28/AA.15's LSM-amplification-aware complexity table, not a B-tree's.
**Memory impact**: none. **Persistence impact**: none. **Recovery
impact**: none. **Testing requirements**: none (documentation-only);
verified by a full-text search confirming no remaining "B-tree" claim
in either document.

---

## AA.17 Transaction model reaffirmation: Snapshot Isolation, write skew, and the autocommit/multi-statement distinction

**Reaffirmed, unchanged**: D10's choice of Snapshot Isolation remains
correct and is not reopened by this amendment — the review directive's
§18 asks this project to *keep* SI only if it still holds up under
review, and it does: D10 already states the write-skew limitation
explicitly ("The write-skew anomaly SI admits is a stated, honest
limitation... not hidden") and already refuses to claim Serializable.
No change to D10's substance.

**New content this amendment adds** (a genuine gap, not previously
stated by D10 or anywhere else in this document): the precise
distinction between an **autocommit statement** and a **multi-statement
transaction**.

**Decision**: an autocommit statement (any DML/DDL statement executed
without an explicit client-issued `BEGIN`) is defined as **exactly a
multi-statement transaction of length one, with an implicit `BEGIN`
inserted immediately before it and an implicit `COMMIT` inserted
immediately after it, using D10's identical Snapshot-Isolation
machinery — never a separate, faster, or weaker-guaranteed code path**.
Concretely: the executor takes a `snapshot()` (D10's `BEGIN` step),
evaluates the statement's read set (e.g. an `UPDATE`'s `WHERE` clause)
against that snapshot, buffers the resulting write-set, then
immediately performs D10's own commit-time freshness re-check and
`write_batch` call — the client never observes the `BEGIN`/`COMMIT`
boundary (no session-visible transaction ID is exposed, no further
statement may be added to it), but the underlying conflict-detection
and atomicity guarantees are identical to a client-issued single-
statement transaction, not a relaxed variant of them.

**Reason**: the review directive's explicit instruction ("Define the
exact distinction between: autocommit statement; multi-statement
transaction") — resolved by *not* inventing a second, separate
execution model for the common case. This is a correctness-motivated
choice, not merely a simplicity one: an autocommit `UPDATE` still forms
a read set (its `WHERE` clause) and a write set (the rows it modifies)
separated by real, non-zero server-side execution time, during which a
concurrent writer could touch the same rows — exactly the race D10's
commit-time freshness check exists to catch. A special-cased "fast path"
for autocommit that skipped the freshness check would silently
reintroduce a lost-update vulnerability D10 already closed for the
multi-statement case, purely because the statement happened to arrive
without an explicit `BEGIN` — precisely the kind of "silently weakening
guarantees" this project's standing principle forbids.

**Alternatives rejected**: *A genuinely separate, lock-free "autocommit
fast path" that applies a single statement's effects without D10's
commit-time freshness re-check, reasoning that "there's no multi-
statement window to race."* Rejected for the reason stated above — the
race window exists within a single statement's own read-then-write span
regardless of whether a client-visible transaction wraps it; treating
autocommit as exempt from D10's conflict detection would be a real
correctness regression, not a harmless optimization.

**Correctness impact**: this section is what prevents a lost-update bug
in the common (autocommit) case specifically — closing exactly the gap
described above. **Security impact**: none new. **Performance impact**:
an autocommit statement pays D10's same per-transaction costs (one
`snapshot()`, one commit-time freshness check, one `write_batch` call)
— no cheaper path exists, and none should, per the correctness argument
above; this is measured as part of D10/AA.5's existing performance
testing requirements, not a new benchmark category. **Memory impact**:
bounded exactly as D10's general write-set limit already bounds it (an
autocommit statement's write-set is typically small, but is not treated
specially — the same D27 cap applies). **Persistence impact**: none
beyond D10. **Recovery impact**: none beyond D9/D10 — an autocommit
statement's implicit transaction has no separate recovery story, since
it is not a separate mechanism. **Testing requirements**: a direct test
that a concurrent writer racing an in-flight autocommit `UPDATE`'s own
read-then-write window causes the autocommit statement to abort with a
conflict error (proving the freshness check is not skipped for the
autocommit case) — the concrete, executable proof that AA.17's
"identical machinery, not a relaxed variant" claim holds, not merely
asserted.

---

## Amendment summary: what changes for Increment 2

`write_batch`'s complete semantics — sequence assignment (AA.1), WAL
frame format (AA.2), atomic-visibility proof (AA.3), same-key resolution
(AA.4), required performance properties (AA.5), resource limits (AA.6),
failure semantics (AA.7), concurrency integration (AA.8), and security
review (AA.9) — are now fully specified and implementable. AA.10–AA.13
resolve catalog-security, documentation-consistency, upgrade-safety, and
SQL-execution-model questions that block later increments but do not
block `write_batch` itself. AA.15–AA.17 correct imprecise wording found
during this review. `PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md` may
now be written as a narrowly-scoped, implementation-time document that
*cites* this amendment rather than re-deriving any of it — its own job
is to record the implementation's file-by-file diff, its measured
performance (AA.5), and its full test-matrix results (AA.1–AA.9's
testing-requirements lists, combined), not to make any new architectural
decision.

**RELATIONAL IMPLEMENTATION = NOT STARTED beyond this amendment's own
documentation.** No source code changes accompany this amendment — the
next step is the implementation-time document and the `write_batch`
code itself, both still pending.

---

# RELATIONAL ADR AMENDMENT 002

**Status**: append-only, D1–D33 and AMENDMENT 001 untouched. `write_batch`
(D9, resolved by AMENDMENT 001) is implemented and certified (`PHASE_
RELATIONAL_TRANSACTION_STORAGE_RESULTS.md`, "WRITE BATCH = PASS"). This
amendment resolves the catalog increment's own remaining open points —
questions D1/D2/D7/D13/D25/D31 name a shape for but do not pin down to
an exact, implementable byte layout: per-system-table column schemas
and primary keys, `system_table_id` constant assignment, durable ID
allocation under concurrency (with no D10 transaction/conflict-
detection layer yet to lean on), and this increment's own DROP scope
given no table-row storage exists yet to sweep.

## CA.1 Durable, restart-safe, collision-free ID allocation

**Decision**: every allocatable catalog ID (`database_id`, `schema_id`,
`table_id`, `index_id`, `constraint_id`, `grant_id` — all `u32`) is
allocated from a **durable counter stored as an ordinary catalog row**
(`system_table_id = 0`, a reserved counters pseudo-table, keyed by a
fixed 1-byte counter-kind tag — never a process-local variable as the
source of truth), read and incremented inside the **same `write_batch`
call** that creates the object the ID names. Concurrency safety within
one process is provided by a `Mutex<()>` internal to the new
`CatalogService` (`src/catalog/mod.rs`), held across the whole
"read current counter value, construct the object's rows, `write_batch`
them together" critical section for every catalog-mutating operation —
serializing catalog DDL within this process, never table-row DML (no
such thing exists yet) and never ordinary reads.

**Reason**: the review directive requires durable, restart-safe IDs,
forbids a process-local counter as the *source of truth*, forbids hash-
derived IDs, and forbids an external sequence service — a catalog row
read fresh at every allocation satisfies all three simultaneously, for
free, via D1's own "catalog rows inherit WAL durability" property. The
`Mutex` is **not** the ID's storage — it is a concurrency-serialization
device closing a real race D10 would otherwise have closed: `write_
batch` (AMENDMENT 001) provides atomicity but, without D10's snapshot-
based conflict detection (not implemented in this increment — no SQL
executor exists to drive `BEGIN`/`COMMIT` yet), it provides no read-
then-write compare-and-swap. Two concurrent `CREATE TABLE` calls that
each read the same "next `table_id`" value and each `write_batch`
independently would both succeed, both durable, both claiming the same
`table_id` — a physical-namespace collision, not merely a logical race.
A single, narrowly-scoped `Mutex` around catalog-DDL's own allocate-
then-write critical section is a real, in-process fix for a real,
in-process hazard; it is not a workaround for a correctness property
this increment is pretending doesn't apply. This is sufficient — not a
compromise — because the certified engine already permits at most one
process to hold a given data directory open at a time (`FileWal`'s own
exclusive directory lock, relied on unchanged here); there is no cross-
process catalog-writer concurrency this design needs to additionally
defend against.

**Alternatives rejected**:
- *Derive IDs from a hash of the object's name.* Explicitly forbidden
  by the review directive, and would break D2's own fixed-width-BE-
  for-ordering correctness argument if a hash were ever used as a
  physical-key-ordering ID rather than an opaque identifier.
- *An external sequence service.* Explicitly forbidden, and unjustified
  complexity — a catalog row already provides exactly the required
  durability with zero new infrastructure.
- *No serialization at all, accepting last-write-wins on collision.*
  Rejected: a `table_id` collision is not a benign "someone's edit was
  overwritten" outcome (SQL `CREATE TABLE`'s own ordinary, acceptable
  race) — it is two *different* tables' data physically interleaved
  under D2's namespace scheme, a storage-corruption-class defect.
- *A separate counter per system table (7 counters) vs. one shared
  counter keyed by counter-kind.* Adopted the latter (one reserved
  pseudo-table, `system_table_id = 0`, rows keyed by a 1-byte counter-
  kind tag) over 7 independent top-level constants purely for a smaller,
  more uniform key space — not a correctness-relevant choice either way.

**Correctness impact**: this is what makes D2's "table_id/index_id
uniquely and collision-freely identify one physical namespace region"
claim actually hold under concurrent DDL, not only in the single-writer
case. **Security impact**: none directly — DDL authorization is D25's
concern, deferred per CA.4 below. **Performance impact**: DDL (rare,
administrative) is serialized process-wide; ordinary catalog *reads*
and, later, table-row DML are entirely unaffected — the lock's scope is
deliberately as narrow as the hazard it closes. **Memory impact**: none
— the `Mutex` guards no additional state beyond itself. **Persistence
impact**: one new reserved catalog row per counter kind, ordinary
durability. **Recovery impact**: none beyond D12 — a crash mid-
allocation leaves the counter at its last durably-committed value
(inherited from `write_batch`'s own all-or-nothing guarantee, AMENDMENT
001 AA.1/AA.3); recovery replays it like any other catalog row, never
needing a special "recompute the counter" pass. **Testing requirements**:
concurrent `CREATE TABLE` from many threads asserting no two tables ever
receive the same `table_id`; ID persistence across a real restart (next
allocation after reopen continues from the durable value, never resets
to a low number); an ID-allocation-then-crash-before-apply test (the
counter's own durability is exactly `write_batch`'s, already proven).

## CA.2 Per-system-table schema

**Decision**: `system_table_id` constants (fixed, `u32`, chosen small
and stable — a future system table, if ever added, gets the next
unused value, never reusing one):

| `system_table_id` | Table | Primary key | Notes |
|---|---|---|---|
| 0 | (reserved: ID counters, CA.1) | counter_kind:u8 | not a user-visible catalog table |
| 1 | `system.databases` | `database_id:u32` | v1: exactly one live row, bootstrapped (CA.3) |
| 2 | `system.schemas` | `schema_id:u32` | `public` always exists (bootstrapped) |
| 3 | `system.tables` | `table_id:u32` | |
| 4 | `system.columns` | `(table_id:u32, ordinal:u16)` | genuinely composite — ordinal is stable per D1 Architecture §1 ("never reorders") |
| 5 | `system.indexes` | `index_id:u32` | includes a `PRIMARY`-kind row per table (D13's own `CREATE TABLE` example), even though D6 stores no separate physical PK structure — this row is catalog/introspection metadata only |
| 6 | `system.constraints` | `constraint_id:u32` | |
| 7 | `system.grants` | `grant_id:u32` | surrogate — see rationale below |

Row-value columns (`RowValue` per D3 — `format_version:u8 ||
schema_version:u32 LE || null_bitmap || values`; PK columns above are
never re-stored in the value, D3's own rule):

- **`system.databases`**: `name:TEXT`, `created_at:TIMESTAMP(i64 µs)`.
- **`system.schemas`**: `database_id:u32`, `name:TEXT`,
  `created_at:TIMESTAMP`.
- **`system.tables`**: `schema_id:u32`, `name:TEXT`,
  `pk_ordinals:BLOB` (a length-prefixed `u16` list — the composite
  primary key's column ordinals, in key order), `schema_version:u32`
  (D31, starts at 1), `state:u8` (`0=ACTIVE`, `1=DROPPING` — CA.4),
  `created_at:TIMESTAMP`.
- **`system.columns`**: `name:TEXT`, `data_type:u8` (D4's type tag —
  only the subset a catalog-only increment needs to round-trip is
  implemented now, CA.5), `nullable:BOOLEAN`, `has_default:BOOLEAN`,
  `default_value:BLOB` (present iff `has_default`), `added_in_schema_
  version:u32` (D31).
- **`system.indexes`**: `table_id:u32`, `name:TEXT`, `kind:u8`
  (`0=PRIMARY`, `1=UNIQUE`, `2=NON_UNIQUE`), `column_ordinals:BLOB`
  (length-prefixed `u16` list, index-column order), `state:u8`
  (`0=ACTIVE`, `1=BUILDING` — backfill state, unused until a future
  increment actually backfills a non-empty table, D7), `created_at`.
- **`system.constraints`**: `table_id:u32`, `name:TEXT`, `kind:u8`
  (`0=PRIMARY_KEY`, `1=UNIQUE`, `2=NOT_NULL`, `3=CHECK`),
  `column_ordinals:BLOB` (empty for `CHECK`), `check_expression:TEXT`
  (empty for non-`CHECK` kinds — D8's expression engine does not exist
  yet; stored as opaque source text for a future increment to parse/
  bind/evaluate, never evaluated here), `added_in_schema_version:u32`.
- **`system.grants`**: `principal:TEXT`, `object_kind:u8`
  (`0=DATABASE`, `1=SCHEMA`, `2=TABLE`), `object_id:u32`,
  `privilege:u8` (`0=SELECT`,`1=INSERT`,`2=UPDATE`,`3=DELETE`,`4=DDL`,
  `5=CREATE_INDEX` — D25's own enumerated list), `granted_at:TIMESTAMP`.

**`system.grants`'s primary key is a surrogate `grant_id:u32`, not the
logical `(principal, object_kind, object_id, privilege)` tuple**,
because `principal` is variable-length `TEXT` and D4 only specifies
TEXT's *row-value* encoding ("length-prefixed") and its *standalone*
key encoding ("raw bytes") — neither ADR nor Architecture document
defines an order-preserving encoding for a variable-length field
*followed by more fields* inside one composite key (the general
technique — escape embedded terminator bytes, then append a terminator
— is a real, well-understood construction, but inventing and shipping
it here, unreviewed, for the one table that needs it, is exactly the
kind of "substitute an ad-hoc encoding where the ADR defines a binary
layout" the review directive forbids when a layout *is* defined, and an
unreviewed invention when it is not). A surrogate integer PK avoids the
question entirely; **logical uniqueness of `(principal, object_kind,
object_id, privilege)` is enforced in the `CatalogService`**, inside
the same `Mutex`-serialized critical section CA.1 already established
(read-check-then-write, race-free for the same reason CA.1 is): a
`grant` call that would duplicate an existing tuple is rejected before
`write_batch` is invoked.

**Reason**: every field above is the direct, minimal representation of
what D1/D6/D7/D8/D13/D25/D31 already say each object needs to record —
nothing here introduces a *new* catalog capability; it makes the ones
those decisions already named concretely encodable and testable.

**Alternatives rejected**:
- *Invent and ship an order-preserving variable-length-field-inside-a-
  composite-key encoding now, to give `system.grants` a "proper"
  composite physical key.* Rejected for the reason above — an
  unreviewed, novel encoding technique introduced unilaterally in an
  implementation increment, for a problem a surrogate key avoids
  cleanly, is unjustified risk for zero behavioral benefit.
- *Store `pk_ordinals`/`column_ordinals` as one `system.columns`-style
  row per (table, position) instead of a single length-prefixed `BLOB`
  in the owning row.* Rejected: these lists are small (bounded by
  D3/Architecture §4's own 1,600-column-per-table cap), read-and-
  written as a unit every time, and splitting them into N additional
  catalog rows would multiply write-batch member count and read-path
  row count for a list that is never queried by individual element —
  exactly the "column-per-cell" write-amplification D3 already rejected
  for ordinary table rows, applied consistently here.

**Correctness/Security/Performance/Memory/Persistence/Recovery
impact**: identical to D1/D3's own already-stated impacts — this
section only fixes the concrete field list those decisions already
committed to providing *a* correct, complete field list for.
**Testing requirements**: round-trip encode/decode for every system
table's `RowValue` (present/absent-default fields, `NULL`-bitmap edge
cases at every table's actual column count), the grants-uniqueness-
enforced-at-the-service-layer property specifically (duplicate grant
rejected, non-duplicate accepted), boundary tests for every `BLOB`-
encoded ordinal list at 0/1/many elements.

## CA.3 Catalog bootstrap

**Decision**: `CatalogService::bootstrap(&self) -> Result<()>` is
idempotent (a no-op if catalog rows already exist — checked by range-
scanning `system.databases` first) and, on a genuinely empty catalog,
creates exactly two rows in one `write_batch`: the single v1 database
(`system.databases`, `database_id = 1`, `name = "default"`) and its
`public` schema (`system.schemas`, `schema_id = 1`, `database_id = 1`,
`name = "public"`) — matching the Architecture document's own §1
statement that both always exist. `LsmEngine::open` does **not** call
`bootstrap` automatically in this increment (no caller — API/CLI
wiring is a later increment's scope); `bootstrap` is a public
`CatalogService` method a future increment's startup path calls once.

**Reason**: "a `public` schema always exists" (Architecture §1) needs
exactly one, explicit, idempotent creation path — not an implicit
assumption every catalog-reading code path would otherwise need to
special-case ("what if `public` doesn't exist yet").

**Alternatives rejected**: *Auto-bootstrap inside `CatalogService::new`.*
Rejected: `new` should be a cheap, infallible-in-practice constructor
(wrap an `Arc<LsmEngine>`); bootstrapping is a real, `write_batch`-
issuing, fallible operation with its own idempotency contract — keeping
it a separate, explicit call matches this project's own "no hidden
work in a constructor" convention (e.g. `LsmEngine::open`'s own
explicit, single, well-documented recovery sequence, never implicit).

**Correctness/Security/Performance/Memory/Persistence/Recovery
impact**: bootstrap is itself just two ordinary catalog rows via one
`write_batch` — no new impact beyond D1/D9's own already-stated ones.
**Testing requirements**: bootstrap-on-empty-catalog creates exactly
the two expected rows; bootstrap-when-already-bootstrapped is a true
no-op (no new `write_batch` call, verified via `next_seq` not
advancing); concurrent `bootstrap` calls from multiple threads never
create duplicate default-database/`public`-schema rows (serialized by
CA.1's same `Mutex`).

## CA.4 DROP scope for this increment

**Decision**: `DROP TABLE`/`DROP INDEX`/`DROP SCHEMA` in this increment
perform a **direct, atomic catalog-row removal** via one `write_batch`
(the table/index/schema's own row, plus — for `DROP TABLE` — its
`system.columns`, `system.indexes`, and `system.constraints` rows) —
**not** D13's full two-phase `DROPPING`-marker-then-background-sweep
protocol. This is a deliberate, explicitly-scoped-down application of
D13, not a reinterpretation of it: D13's sweep phase exists specifically
to bound the cost of physically removing a large table's *row and index
data* — and **no table-row storage exists in this increment** ("Do NOT
implement user-table row storage" is this increment's own explicit
scope boundary). There is structurally nothing for a sweep to sweep
yet. Once a future increment adds table-row storage, `DROP TABLE` must
be revisited to implement D13's full `DROPPING`-marker-plus-background-
sweep design exactly as written — this section does not weaken D13; it
states precisely which slice of D13 has substance today and which does
not yet.

**Reason**: implementing a background-sweep worker now, with nothing
for it to ever sweep, would be exactly the kind of speculative,
unneeded-by-the-current-phase machinery this project's own "don't add
complexity the current phase doesn't need" principle forbids — and
would be untestable in any way that actually exercises its resumable-
on-crash behavior, since no crash-mid-sweep scenario can exist without
real row data to interrupt sweeping.

**Alternatives rejected**: *Implement the `DROPPING` marker state
(`system.tables.state`) but not the sweep, leaving dropped tables stuck
`DROPPING` forever.* Rejected: worse than a direct removal — it invents
a state with no code path that ever resolves it, a dangling half-
feature. *Implement the full sweep now, against an empty/nonexistent
row set, "for completeness."* Rejected: untestable in its own most
important dimension (crash-mid-sweep-with-real-data) and pure
speculative complexity per the reasoning above.

**Correctness impact**: a dropped table/index/schema becomes invisible
atomically (its catalog row is gone the instant the `write_batch`
commits) — identical to D13's own stated catalog-visibility guarantee,
just without a physical-data phase that has nothing to act on yet.
**Security impact**: none beyond D13. **Performance impact**: O(catalog
rows touched) — cheap, bounded, matching D13's own "small-table DROP
TABLE" cost class exactly (this increment has no large-table case to
diverge from it). **Memory impact**: none. **Persistence/Recovery
impact**: ordinary catalog-row deletion, inherited durability/recovery,
zero new code. **Testing requirements**: `DROP TABLE` removes exactly
its own table/columns/indexes/constraints rows and no others (cross-
table isolation, D2); a dropped table's name becomes immediately
available for reuse; crash immediately after a `DROP TABLE` `write_
batch` commits (recovery shows the table gone, not partially gone —
this is exactly AMENDMENT 001's own atomicity proof, re-exercised
against catalog rows specifically, not a new proof).

## CA.5 Catalog-scoped value encoding (not the full D4 type system)

**Decision**: this increment implements a small, closed `CatalogValue`
enum — `U8`, `U16`, `U32`, `I64` (used only for `TIMESTAMP` fields
here), `Bool`, `Text`, `Blob` — sufficient to encode every field CA.2's
schema actually uses, built on D3's exact `RowValue` envelope
(`format_version || schema_version || null_bitmap || values`) and D4's
stated encoding conventions for the types it reuses (fixed-width for
numeric/bool, length-prefixed for `TEXT`/`BLOB`). It is **not** the
full D4 type system (`DECIMAL`, `REAL`, `DOUBLE`, `DATE`, `TIME`, full
`TIMESTAMP` semantics, order-preserving sign-flip/bit-transform *key*
encodings for signed/float types) — those exist only where D4 actually
requires them: user-table columns, which do not exist in this
increment ("Do NOT implement user-table row storage"). `system.columns.
data_type:u8` stores D4's full type tag set (so a future increment's
user-table rows can be typed correctly the moment they exist), but
this increment's own catalog rows never themselves contain a `DECIMAL`/
`REAL`/`DOUBLE`/`DATE`/`TIME` value — `CatalogValue` has no such variant
because nothing in CA.2's schema needs one yet.

**Reason**: building the full, general D4 type system (order-preserving
transforms for every numeric/temporal type, `DECIMAL`'s scaled-`i128`
arithmetic, etc.) has no caller in this increment — every catalog field
CA.2 defines is a `u8`/`u16`/`u32`/`i64`/`bool`/`TEXT`/`BLOB`. Building
it now "for completeness" would be exactly the untested, unexercised,
premature-complexity pattern this project consistently rejects
elsewhere (D7's index-cache deferral, D13's DROP-sweep-now-vs-later
reasoning above) — it will be built, tested, and property-tested
against real boundary values when a future increment's user-table row
storage actually needs it, not speculatively now.

**Alternatives rejected**: *Build the full D4 type system now, even
though only a subset is exercised.* Rejected per Reason above.

**Correctness/Security/Performance/Memory/Persistence/Recovery
impact**: identical to D3/D4's own stated impacts, scoped to the subset
actually implemented. **Testing requirements**: round-trip for every
`CatalogValue` variant actually implemented, `NULL`-bitmap correctness
at every system table's real column count (CA.2) — the full D4
type-system testing requirements (numeric ordering property tests,
etc.) are out of scope here and remain binding on whichever future
increment implements the rest of D4.

---

**Catalog increment implementability**: CA.1–CA.5 resolve every open
point standing between this ADR's already-decided architecture (D1–D33,
AMENDMENT 001) and a concrete, implementable catalog. The implementation
increment may now proceed directly against this specification.
