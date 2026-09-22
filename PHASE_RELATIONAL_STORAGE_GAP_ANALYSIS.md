# RubiXDB — Relational Storage Gap Analysis

**Date:** 2026-09-22

**Status:** Phase 0 deliverable — read-only audit finding. This document exists
because the audit found at least one required relational guarantee the
current LSM API cannot provide as-is (§2.1 below), per the governing
instruction: create this document "if the current LSM API is insufficient
for any required relational guarantee." It does not propose implementation
code and does not modify any certified path. It is a precondition input to
`PHASE_RELATIONAL_DATABASE_ADR.md`, not a substitute for it.

---

## 0. Method

Read-only. No file under `src/` was modified. Findings are sourced from:

- `RubixDB-LSM-Engine-Specification-v1.0.md`, `RubixDB-Architecture-Specification-v1.0.md`
- `PHASE_WRITE_ENGINE_CERTIFICATION.md`, `PHASE_READ_ENGINE_CERTIFICATION.md`, `PHASE_COMPACTION_CERTIFICATION.md`
- `PHASE_API_ARCHITECTURE.md` §0 (its own fresh `grep`-verified public-API audit, cross-checked directly against source below)
- Direct source inspection: `src/lsm/mod.rs`, `src/error.rs`, `src/manifest/format.rs`, `src/execution/batch_coordinator.rs`

Every claim below was checked against the actual `src/` source at the
currently-committed `HEAD` (clean working tree, `git status` confirmed
before this audit began), not inferred from documentation alone.

---

## 1. What is reusable as-is (no extension required)

| Primitive | Reuse | Evidence |
|---|---|---|
| `LsmEngine::put`/`get`/`get_as_of`/`delete`/`contains` | Single-key row storage for table/index/catalog rows, unchanged | `src/lsm/mod.rs:1637-1861` |
| `LsmEngine::range`/`range_scan` | Table scans, index range scans, ordered catalog iteration — all as k-way-merged, bounded-memory, tombstone-correct iterators | `src/lsm/mod.rs:1958-1988`; certified Read Engine, `PHASE_READ_ENGINE_CERTIFICATION.md` rows 3-5 |
| `LsmEngine::snapshot`/`Snapshot` | Point-in-time consistent multi-key reads (a query's own read view; the read half of a future transaction's snapshot) | `src/lsm/mod.rs:1874-1885`; certified rows 7-8 |
| WAL + fsync durability boundary | Durability for every catalog/table/index write, transitively, once writes go through `put`/`delete`/the new batch primitive (§2.1) | Certified Write Engine, unchanged since `7d02554` |
| Crash recovery (Manifest replay + orphan sweep + WAL replay) | Full crash recovery for table/index/catalog data **for free**, with zero new recovery code, provided catalog/table/index rows are stored as ordinary engine keys (this is the central reason `PHASE_RELATIONAL_DATABASE_ADR.md`'s catalog-placement decision stores the catalog *inside* the existing keyspace rather than as a second mechanism) | `RubixDB-LSM-Engine-Specification-v1.0.md` §7 |
| Compaction (size-tiered, full-merge, automatic) | Reclaims obsolete row/index versions and tombstones for table and index data identically to any other key — compaction is byte-key-agnostic and requires no relational awareness to function correctly | `PHASE_COMPACTION_CERTIFICATION.md`; compaction operates on raw `(key, seq)` order with no assumption about key structure |
| `StoragePressure`/`StorageFull`/`StorageExhausted` backpressure contract | Applies unchanged to relational writes — a `CREATE TABLE` or `INSERT` is, physically, still just calls into the same write path | `[[project_rubixdb_capacity_contract]]`-equivalent: accepted contract, durable+applied write, freeze is a backpressure signal only — unchanged by this analysis |
| `EngineError` taxonomy | Sufficient for the relational layer's own error mapping to reuse the same "never log payload contents, never leak raw I/O detail" discipline the API layer already established | `src/error.rs:17-59`; `PHASE_API_ARCHITECTURE.md` §3 |
| Byte-lexicographic key ordering (`BTreeMap<(Vec<u8>, u64), _>`, SSTable records sorted `(key asc, seq asc)`) | The physical substrate every ordered index/table-scan design in `PHASE_RELATIONAL_DATABASE_ADR.md` depends on — order-preserving typed-value encoding (ADR "Row and Key Encoding" decision) is required specifically *because* this ordering is raw byte comparison, not a typed comparison | `RubixDB-LSM-Engine-Specification-v1.0.md` §1.1, §2.2 |

None of the above requires a new storage primitive. This is the majority of
what a relational layer needs from a storage substrate, and it is why the
architecture in `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` builds the
catalog and table storage *inside* the existing single flat keyspace rather
than inventing a second storage mechanism alongside it.

---

## 2. What is insufficient as-is

### 2.1 No multi-key atomic write primitive — the load-bearing gap

**Finding.** `LsmEngine::put`/`delete` (`src/lsm/mod.rs:1637-1659`) each
independently: submit one `WalOpOwned` to the batch coordinator, wait for
that one op's own durability, then apply that one op to the active
MemTable. There is no method on `LsmEngine` that accepts more than one
key/value pair and applies them as a single all-or-nothing unit. The
`BatchCoordinatorPool` (`src/execution/batch_coordinator.rs`) batches
*multiple concurrent callers'* independent single-key WAL appends together
for **group-commit throughput** — this is a durability/fsync-amortization
optimization, not a logical atomicity guarantee. Each caller's own op is
still individually durable-then-applied; nothing ties two different
`put`/`delete` calls together such that a crash between them can be ruled
out, and no caller-visible "all N keys landed, or none did" contract
exists anywhere in the certified engine.

This was independently confirmed by `PHASE_API_ARCHITECTURE.md` §0's own
`grep`-verified public API enumeration, which lists exactly `put`/`delete`
as the only write methods, and by the Architecture Spec's own admission
(§7.4): *"Cross-partition atomicity: not guaranteed in v1 beyond a single
partition's single-batch write... Multi-partition transactions are Future
Work"* — and even that "single-batch write" atomicity is a Section 4.1
*contract requirement for a future engine trait*, not something the
current, certified, single-partition `LsmEngine` facade actually
implements as a public multi-key operation today.

**Why this matters.** Per the governing Phase 9 stop condition: *"A
committed table state must never disagree with its indexes... If current
LsmEngine primitives cannot guarantee this: STOP. Do NOT fake atomicity."*
A relational `INSERT` that must write one table row plus N secondary-index
entries, or a `CREATE TABLE` that must write a table-catalog row plus its
column rows plus its default primary-key index row, has exactly this
shape: multiple physical keys that must become visible together or not at
all. Built on `put`/`delete` alone, a crash between the table-row write and
an index-row write leaves the table and its index durably disagreeing —
an unrecoverable, silent correctness violation, not a recoverable ambiguity
(recovery has no way to know the two writes were ever meant to be linked).

**Resolution required, not implemented here.** `PHASE_RELATIONAL_DATABASE_ADR.md`'s
Transaction Model and Index/Table Consistency decisions **do** resolve this
architecturally — by specifying the minimum new engine primitive required
(a single atomic multi-key `write_batch` operation: one WAL frame carrying
N ops, applied to the MemTable as one critical section, acknowledged with
one seq range) — but that primitive does not exist in the certified engine
today, and this document does not implement it. Building it is explicitly
out of scope for Phase 0/Phase 1 (architecture only); it is the one piece
of net-new *storage-layer* work every later relational increment
(Increments 5 onward) depends on, and per the directive's own governing
rule, extending certified storage-engine semantics requires "an explicit
new ADR" before any code is written — that ADR is `PHASE_RELATIONAL_DATABASE_ADR.md`'s
"Transaction Model" and "Storage Extension Required" decisions (this
document's own findings are their evidentiary basis), with the
implementation-time ADR (`PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md`)
deferred to Phase 9 of the overall build order, per instruction.

### 2.2 No durable catalog mechanism — and the existing Manifest cannot be repurposed for one

**Finding.** `ManifestEdit` (`src/manifest/format.rs:19-34`) is a closed,
3-variant Rust enum (`AddSstable`, `RemoveSstable`, `SetCheckpoint`) with a
hand-written binary encoding tightly coupled to SSTable lifecycle
bookkeeping. It is not a general-purpose durable log. Adding a 4th variant
for catalog metadata would require editing `src/manifest/` — a certified,
protected path every phase's own certification has explicitly audited as
byte-for-byte unchanged (`PHASE_COMPACTION_CERTIFICATION.md` § Protected
Dependencies: `git diff ... -- src/wal/ src/manifest/ ...` asserted to
produce zero output as a certification gate). Repurposing it would both
violate that protected-path discipline and conflate two unrelated
concerns (SSTable lifecycle vs. relational metadata) inside one format.

Separately, the Manifest is explicitly *not* compaction-safe: "The
Manifest file itself is not compacted/snapshotted in v1... grows
unboundedly. This is an accepted Phase 0 limitation" (`RubixDB-LSM-Engine-
Specification-v1.0.md` §6.3). A catalog piggybacked onto it would inherit
that unbounded-growth characteristic for a dataset (table/column/index
definitions) that, unlike SSTable bookkeeping, is genuinely long-lived and
rewritten over a database's lifetime (`ALTER TABLE`, `DROP INDEX`, etc.) —
a materially worse fit than the SSTable-add/remove traffic the Manifest
was designed for.

**Resolution required, not implemented here.** The catalog must be its own
durable structure. `PHASE_RELATIONAL_DATABASE_ADR.md`'s "Catalog
Architecture" decision resolves this by storing catalog objects as
ordinary rows in the existing flat keyspace (under a reserved system key
prefix), reusing `put`/`get`/`range_scan` rather than the Manifest — this
requires zero engine changes and zero new persistence mechanism, at the
cost of catalog mutations needing the same atomic multi-key primitive as
§2.1 (a `CREATE TABLE` writes several catalog rows that must commit
together).

### 2.3 No structured key-namespace convention — must be designed, not assumed

**Finding.** The engine imposes no structure on keys beyond raw byte
ordering (`Vec<u8>`, compared lexicographically). This is correctly listed
as "reusable" in §1 as an ordering *substrate*, but it is not itself
sufficient: nothing today prevents two different physical uses of the
keyspace (e.g. a pre-existing flat-KV client of `/v1/kv` and a new
relational table) from writing colliding keys. `PHASE_API_ARCHITECTURE.md`'s
existing `/v1/kv` contract lets any authenticated `admin` write any
`&[u8]` key with no reserved-prefix concept at all.

**Resolution required, not implemented here.** `PHASE_RELATIONAL_DATABASE_ADR.md`'s
"Physical Key Layout" decision reserves an explicit namespace byte for
system/relational data and requires the existing flat KV surface to reject
client-supplied keys inside that reserved namespace once the relational
layer is enabled — a compatibility-breaking change for any pre-existing
flat-KV deployment that happens to already use keys inside the reserved
range, called out explicitly (not silently) in that ADR's "Backward
Compatibility" decision.

### 2.4 No concurrency-control primitive beyond snapshot-read isolation

**Finding.** The engine provides read-side MVCC (per-key `(key, seq)`
versioning, `Snapshot` for a pinned read view) but no write-side
concurrency control whatsoever beyond the fact that each individual
`put`/`delete` call is internally serialized through one WAL-append path.
There is no optimistic write-conflict detection, no read-set tracking, no
lock manager, and (per §2.1) no multi-op transaction boundary for either
reads or writes to be grouped under.

**Resolution required, not implemented here.** `PHASE_RELATIONAL_DATABASE_ADR.md`'s
"Transaction Model / Isolation" decision specifies Snapshot Isolation built
*on top of* the existing `Snapshot` primitive plus the new `write_batch`
primitive from §2.1, with conflict detection implemented at the relational
layer (comparing a transaction's buffered write-set against the latest
committed state of the same physical keys at commit time) — no engine
change beyond §2.1's `write_batch` is required for this specific decision,
but it is only resolvable once §2.1 exists.

### 2.5 No resource/statement-shaped limits at the engine level

**Finding.** The engine itself enforces no notion of "statement,"
"transaction," or "query" — `CapacityExceeded` is a MemTable-freeze
backpressure signal (unrelated, and an already-accepted, unchanged
contract per current project memory), and `max_value_bytes`/
`max_range_limit`/rate limiting are API-layer, not engine-layer,
constructs (`PHASE_API_ARCHITECTURE.md` §4). None of these are SQL-shaped
(statement size, parameter count, transaction op count, sort/join memory).

**Resolution required, not implemented here.** These are new relational-
and API-layer limits with no dependency on a storage-engine change;
`PHASE_RELATIONAL_DATABASE_ADR.md`'s "Resource Limits" decision defines
them at the SQL executor and API layers only, following the same
enforcement pattern (reject before doing the expensive work, fail loud,
never silently truncate) the existing API layer already uses.

---

## 3. Gaps NOT found (explicitly, so none is assumed later by omission)

- **Key/value size ceilings**: the engine imposes no *hard* per-key/value
  size ceiling of its own (unbounded `Vec<u8>`); this is a design choice
  the relational row-size limit (§2.5, ADR "Resource Limits") must impose
  at a layer above the engine, not a gap the engine needs to be extended
  to support.
- **Range-scan efficiency**: confirmed sufficient (§1) — no extension
  needed for table/index scans.
- **Compaction awareness of "logical row groups"**: not needed. Compaction
  operates correctly on raw byte-ordered keys with no knowledge of table/
  row/index structure, and must continue to do so — the relational layer
  must not require compaction to become schema-aware (doing so would
  itself violate the protected-path discipline).
- **Checksum/corruption handling**: sufficient as-is; a corrupted catalog
  or table row fails closed exactly like any other engine key, with no
  new corruption-handling code needed.

---

## 4. Summary

| Required relational guarantee | Engine primitive today | Sufficient? |
|---|---|---|
| Row/catalog storage, durability, recovery | `put`/`get`/`range`/WAL/Manifest replay | Yes (§1) |
| Ordered table/index scans | `range`/`range_scan` | Yes (§1) |
| Point-in-time consistent multi-key reads | `snapshot()`/`Snapshot` | Yes (§1) |
| Atomic multi-key writes (table+index, multi-row catalog DDL, DML) | **None** | **No — new primitive required (§2.1)** |
| Durable catalog metadata store | **None reusable** (Manifest is closed/protected/unbounded) | **No — architecture resolves via reserved keyspace, not an engine change (§2.2)** |
| Namespace isolation between relational and existing flat-KV data | **None** | **No — reserved prefix + compatibility-breaking API change required (§2.3)** |
| Write-side concurrency control / transaction isolation | **None** | **No — resolvable via new primitive + relational-layer logic, once §2.1 exists (§2.4)** |
| SQL-shaped resource limits | **None** (engine has none; API has unrelated ones) | **No — new limits, no engine dependency (§2.5)** |

**One genuine storage-layer extension is required**: an atomic multi-key
write primitive (§2.1). Everything else insufficient above is resolvable
at the relational/API layer without modifying certified engine code. This
finding is carried into `PHASE_RELATIONAL_DATABASE_ADR.md` as the single
governing constraint on the Transaction Model, Index Consistency, and DDL
Durability decisions.

**No relational implementation code was written to produce this
document.** `git status` / `git diff --stat` / `git diff --name-only`
(re-run at the end of this phase, per governing instruction) confirm this.
