# RubixDB Architecture Specification v1.0

Status: Draft for implementation sign-off
Scope: Single-node embedded database engine (Rust)
Audience: RubixDB implementers, reviewers of the RubixDB design review

---

## 0. Purpose and Scope

This document is the design contract for RubixDB. It exists to answer, precisely and in writing, every question an implementer would otherwise have to invent an answer to while writing code — what a partition is, what an engine promises, what a version means, what the router is allowed to do, what happens when a machine crashes mid-migration, and how a claim of "adaptive" gets measured rather than asserted.

RubixDB is one logical database backed by three interchangeable physical storage engines — Log, LSM, and B+Tree — with per-partition routing decided by an observed-workload cost model. The engineering promise of the project is not "we built three storage engines." It is "we built one database that quietly picks the right storage engine for each slice of data, and can prove it was the right choice."

This specification covers a single-node, embedded deployment. Distribution, replication, and multi-node consensus are explicitly out of scope for v1 and are listed under Future Work (Section 19).

### 0.1 Non-goals for v1

- No distributed deployment, replication, or sharding across machines.
- No full ACID multi-statement transactions across partitions. Single-key and single-partition-batch atomicity only (defined in Section 7).
- No online (zero-downtime) migration. v1 migration is offline-per-partition (Section 13); online migration is Future Work.
- No cost-based SQL query optimizer in the traditional sense (join ordering, etc.) — the Query Layer (Section 14) optimizes for partition pruning and engine push-down, not multi-table joins.
- No user-facing knowledge of which engine backs their data. Engine identity is an internal, observable-but-not-configurable-by-default implementation detail.

### 0.2 How to read this document

Sections 1–9 define what RubixDB *is* — the data model, the engine contract, and the invariants every engine must uphold regardless of routing decisions. Sections 10–13 define the *adaptive* subsystem — workload observation, cost modeling, routing, and migration. Sections 14–17 define the *surrounding* systems — query layer, observability, security, and the benchmark protocol that validates the whole thing. Section 18 is a one-page module boundary summary. Anything not nailed down here is an implementation detail the engineer is free to choose; anything nailed down here requires a spec change (and a version bump) to alter.

---

## 1. Design Principles

1. **One logical database, three physical engines.** Callers above the Partition Manager never branch on engine type. If application code needs to know whether a partition is Log, LSM, or B+Tree, the abstraction has failed.
2. **Routing happens at partition granularity, never at table granularity.** A table is a namespace; a partition is the unit of physical storage strategy. This avoids all-or-nothing table conversions and lets a single table have hot and cold partitions on different engines simultaneously.
3. **Observe, don't guess.** The router never selects an engine from a static hint ("this looks like a write-heavy table"). It selects from measured workload statistics accumulated over a defined observation window (Section 10).
4. **Migration cost is a gate, not an input.** The router first asks "which engine is theoretically cheapest right now," entirely ignoring the cost of getting there, and only then asks "is switching worth it." Folding migration cost into the primary cost comparison creates a circular dependency (the cost of migrating depends on the current engine, which depends on the last migration decision) and is explicitly rejected as a design (Section 12).
5. **Every engine speaks one recovery language.** WAL format and the crash-recovery procedure are engine-independent at the logical level, even though physical replay differs per engine (Section 8).
6. **Every routing decision must be explainable after the fact.** If RubixDB moves a partition from B+Tree to LSM, an operator must be able to ask "why" and get a concrete answer built from recorded workload statistics and cost estimates (Section 15) — not "the router decided to."
7. **Build order matters and is part of this spec, not just a project-planning artifact.** Engines first, with a real WAL and real recovery. Metadata layer early — before multi-engine coexistence, since the metadata schema shapes everything above it. Unified read layer working across at least two engines before the third is added. Cost model and router added only after real benchmark numbers exist for each engine in isolation — a cost model calibrated against invented numbers is worse than no cost model, because it hides its own wrongness. Migration only after routing is stable. This ordering is binding: no adaptive-router code should be merged before Sections 3–9 are implemented and passing their own conformance tests.

---

## 2. Logical Data Model

RubixDB presents a conventional relational-shaped hierarchy to callers:

```
Database
  └── Schema
        └── Table
              └── Partition
                    └── Records
```

- **Database**: the top-level namespace; one RubixDB instance serves one database in v1.
- **Schema**: a namespace grouping tables (mirrors standard SQL `schema`/`namespace`). Optional — a database may use a single implicit `default` schema.
- **Table**: a named, typed collection of records sharing one logical schema (column set and types) and one partitioning scheme.
- **Partition**: the unit of physical storage. Every partition belongs to exactly one table, owns a contiguous or hash-defined key range (partitioning scheme is table-level, chosen at table creation: range or hash), and is backed by exactly one storage engine at any point in time.
- **Record**: a key/value tuple (Section 7 defines the versioned record format actually stored).

A table's partitioning scheme (range vs. hash, and the partition key columns) is fixed at table creation. *Which engine* backs a given partition is not fixed — that is precisely the dimension RubixDB is adaptive over. Partition *boundaries* (the key range or hash-bucket assignment) are not changed by the adaptive subsystem in v1; only the physical engine changes. Partition splitting/merging due to size is a separate, orthogonal mechanism (briefly noted in Section 19) and must not be conflated with engine migration.

---

## 3. Partition Model

A partition is the atomic unit of routing, migration, and recovery. Formally, a partition has:

| Property | Description |
|---|---|
| `partition_id` | Globally unique identifier (ULID or equivalent monotonic-ish ID). |
| `table_id` | Owning table. |
| `key_range` | Inclusive/exclusive key boundaries (range partitioning) or bucket set (hash partitioning). |
| `engine` | Current storage engine: `LOG`, `LSM`, or `BTREE`. |
| `generation` | Monotonically increasing integer, incremented on every engine change or physical rebuild of the partition. Used to distinguish stale physical artifacts from current ones and to make recovery idempotent. |
| `status` | Partition lifecycle state (Section 3.1). |
| `sequence_range` | The range of global sequence numbers (Section 7) this partition's current generation is known to contain. |

### 3.1 Partition lifecycle states

```
CREATING → ACTIVE → (MIGRATING → ACTIVE)* → RETIRING → RETIRED
                 ↘ DEGRADED ↗
```

- **CREATING**: partition metadata registered, no engine instance yet initialized. Not visible to reads.
- **ACTIVE**: normal state. Exactly one engine owns writes and reads.
- **MIGRATING**: a specific sub-state machine owned by the Migration Manager (Section 13.2), nested inside the partition lifecycle. Reads continue to be served (from the old engine, per Section 13.1); writes are frozen for the freeze-window portion of migration only.
- **DEGRADED**: the partition's engine failed a health check (e.g., corruption detected, recovery incomplete) or a migration aborted. Reads may be served in a reduced/read-only capacity if a valid generation exists; writes are rejected. Requires operator or automated remediation to return to ACTIVE.
- **RETIRING / RETIRED**: only relevant to the *old* physical representation during/after a migration (Section 13.1) or to a partition being dropped at the table level.

A partition is never in two non-transitional states at once, and `status` transitions are themselves written through the Metadata Manager's versioned update path (Section 6.3), so a crash mid-transition is recoverable by re-reading metadata rather than by inferring state from file-system contents.

---

## 4. Storage Engine Contract

Every engine — Log, LSM, B+Tree, and any future engine — implements the same trait-level contract. The logical layers above (Partition Manager, Unified Read Layer, Query Layer) depend only on this contract, never on an engine's internals.

### 4.1 Required operations

| Operation | Signature (conceptual) | Semantics |
|---|---|---|
| `put(key, value, seq) -> Result<()>` | Write-through the engine's write path (Section 5) | Durable once acknowledged (WAL-fsynced per Section 8.2 durability policy). |
| `delete(key, seq) -> Result<()>` | Logical delete | Must produce a tombstone (Section 7.3), not a physical erasure, unless the engine is executing an explicit compaction/GC pass that has independently proven the tombstone is safe to drop (Section 7.3). |
| `get(key) -> Result<Option<VersionedValue>>` | Point lookup | Returns the highest-sequence non-obsolete value, or `None` if the highest-sequence record is a live tombstone. |
| `point_lookup(key, as_of_seq) -> Result<Option<VersionedValue>>` | Point lookup as of a sequence number | Required for snapshot reads (Section 7.4); engines that cannot support this natively must reject with `Unsupported` and are ineligible for workloads requiring snapshot reads until they do. |
| `range_scan(start, end, as_of_seq) -> Iterator<VersionedValue>` | Ordered range iteration | Must return keys in ascending key order, each at its highest sequence number ≤ `as_of_seq`, honoring tombstones. |
| `flush() -> Result<FlushReceipt>` | Force in-memory state to durable storage | Returns the sequence number watermark that is now durable. |
| `checkpoint() -> Result<CheckpointHandle>` | Create a durable, engine-internal recovery point that shortens future WAL replay | Engine-specific implementation; logically equivalent across engines. |
| `snapshot() -> Result<SnapshotHandle>` | Produce a read-only, point-in-time view usable by `point_lookup`/`range_scan` at a fixed sequence number | Backing implementation (COW, reference-counted SSTable set, MVCC page versions, etc.) is engine-specific. |
| `recover(wal_segment_iter) -> Result<()>` | Rebuild in-memory state from WAL + last checkpoint | Must be idempotent — replaying an already-applied WAL record is a no-op, detected via sequence number comparison against engine state. |
| `export_iterator() -> Iterator<VersionedRecord>` | Full ordered iteration of all live (and, if requested, tombstoned) records | Used exclusively by the Migration Manager (Section 13) to build a destination engine's representation. Must be a consistent, as-of-a-single-sequence-number snapshot iteration — i.e., built on top of `snapshot()`. |

### 4.2 Error model

All operations return a `Result` over a shared `EngineError` enum: `NotFound`, `Corruption`, `IoError`, `WalUnavailable`, `Unsupported(op)`, `CapacityExceeded`, `Aborted`. Callers above the engine layer (Partition Manager, Migration Manager) handle these uniformly; no caller should match on an engine-specific error type.

### 4.3 Conformance

Every engine implementation must pass a shared conformance test suite that exercises the contract above against all three engines with identical inputs and asserts identical *logical* outputs (allowing different physical layouts and different performance). This suite is a deliverable of Section 1's build-order principle #7 and must exist and pass before the Unified Read Layer (Section 9) is considered complete.

---

## 5. Engine Specializations and Write Paths

The three engines are not interchangeable indexing tricks; each is a distinct storage strategy tuned for a distinct workload shape (Section 10 defines the signals used to tell them apart quantitatively).

- **Log Engine** — optimized for high-throughput sequential ingestion where reads are rare, mostly-recent, or handled by a downstream consumer. Weakest at random point lookups and range scans over old data.
- **LSM Engine** — optimized for write-heavy and mixed read/write workloads, tolerates high update/delete ratios, amortizes write cost via background compaction at the expense of read and space amplification.
- **B+Tree Engine** — optimized for ordered access: range scans, point lookups on a mostly-stable key set, and in-place updates. Weakest under sustained high-velocity random writes (page-split churn).

### 5.1 Write paths

**Log Engine**
```
Write → WAL → Sequential Append → Segment → Segment Metadata Update
```
Segments are immutable once sealed (by size or time threshold); segment metadata (min/max sequence number, key range observed, size) is registered with the Metadata Manager on seal.

**LSM Engine**
```
Write → WAL → MemTable → Immutable MemTable → SSTable (flush) → Compaction
```
MemTable flush to an SSTable is triggered by size threshold. Compaction strategy (leveled vs. size-tiered) is an engine-internal implementation choice but must expose compaction pressure as an observable signal (Section 10) regardless of strategy.

**B+Tree Engine**
```
Write → WAL → Find Target Page → Modify Page → Possible Page Split → Persist Page
```
Page splits propagate upward per standard B+Tree rebalancing; the engine must log enough in the WAL to redo a split deterministically during recovery (page-level physiological logging, i.e., logical operation + physical page ID, not full before/after page images, to keep WAL volume tractable).

All three paths write through the same logical WAL abstraction (Section 8) before acknowledging the caller, regardless of how differently they subsequently organize durable state.

---

## 6. Metadata and Ownership Layer

The Metadata Manager is not optional infrastructure bolted on later — it is the layer that makes multi-engine coexistence possible at all, and per the build-order principle (Section 1.7) it is implemented early, immediately after single-engine conformance, before any second engine is wired into routing.

### 6.1 Partition metadata schema

| Field | Type | Description |
|---|---|---|
| `partition_id` | UUID/ULID | Primary key of this metadata record. |
| `table_id` | UUID | Owning table. |
| `key_range` | `(lower: Bytes, upper: Bytes, lower_inclusive: bool, upper_inclusive: bool)` or `bucket_set: Vec<u32>` | Partition boundaries. |
| `engine` | enum `LOG \| LSM \| BTREE` | Current physical engine. |
| `generation` | u64 | Incremented on every engine change / physical rebuild. |
| `status` | enum (Section 3.1) | Lifecycle state. |
| `physical_location` | String (path/handle) | Where this generation's data lives on disk. |
| `sequence_range` | `(min_seq: u64, max_seq: u64)` | Sequence numbers known to be represented in this generation. |
| `migration_state` | `Option<MigrationRecord>` | Present only while `status == MIGRATING`; see Section 13.2. |
| `schema_version` | u32 | Table schema version this partition's records conform to. |
| `snapshot_refs` | `Vec<SnapshotHandle>` | Outstanding snapshots holding this generation open; a generation cannot be retired while non-empty (Section 13.1, retire step). |
| `stats_ref` | pointer/handle | Link to the Workload Analyzer's rolling statistics for this partition (Section 10). |

Example instance (illustrative, not literal wire format):

```
Partition P001
  table_id        = employees
  key_range        = [0, 250000)
  engine           = LSM
  generation       = 17
  status           = ACTIVE
  physical_location = /data/employees/p001/gen17/
  sequence_range   = [100000, 250000]
  schema_version   = 3
  snapshot_refs    = []
```

### 6.2 Table and schema metadata

Table metadata records: `table_id`, `schema` (ordered column definitions with types and nullability), `schema_version` history (append-only list of prior schema versions, to support reading old partitions before a background schema-migration pass, if any, catches up — full schema-evolution semantics are Future Work per Section 19, but the version history field is part of v1 so it isn't a retrofit), `partitioning_scheme` (range/hash + key columns), and the current live `partition_id` list.

### 6.3 Metadata durability and update protocol

Metadata itself is a small, high-value, low-volume dataset and is stored durably and separately from partition data (its own WAL + compact on-disk representation — effectively metadata is "partition zero," durable via the same WAL contract as Section 8, just logically distinct). All metadata mutations (status transitions, generation bumps, migration state changes) are applied via compare-and-swap on an expected prior version, so two concurrent actors (e.g., a migration completing and a concurrent DDL) cannot silently clobber each other; a losing writer retries against the new version.

### 6.4 Why this layer is mandatory

Ownership, versioning, tombstone lifecycle, and migration state only need to be tracked *because* multiple engines can coexist per table. Any read must know which physical generation is authoritative for a given key range before it can dispatch to an engine; any recovery procedure must know which generation was mid-migration when the crash occurred; any cost decision must know which stats belong to which partition. The Metadata Manager is the single source of truth all of that depends on.

---

## 7. Versioning and Consistency Model

### 7.1 Global sequence number

RubixDB maintains a single monotonically increasing **global logical sequence number (LSN)**, assigned at write-acceptance time (after WAL durability, before engine apply — see Section 8.2). Every stored record carries the LSN it was written at. This is the one piece of global, cross-engine state the whole versioning and consistency story is built on.

### 7.2 Record format

Conceptually, every stored unit is:

```
Record {
  key: Bytes,
  value: Bytes | Tombstone,
  seq: u64,       // global LSN at write time
  op: PUT | DELETE,
}
```

Example:
```
101 → "Arun"      → seq 100  (PUT)
101 → "Arun R."    → seq 150  (PUT)
```
The Unified Read Layer (Section 9) resolves multiple versions of the same key by taking the highest `seq` ≤ the read's `as_of_seq` (current time for a normal read; a fixed value for a snapshot read).

### 7.3 Tombstones

A `delete` never physically removes a record from an engine's live representation. It writes a tombstone record (`op = DELETE`) at the delete's LSN. A tombstone is eligible for physical removal (during compaction, page reclamation, or segment GC) only when **both**: (a) no outstanding snapshot (`snapshot_refs`, Section 6.1) has an `as_of_seq` below the tombstone's `seq`, and (b) every older version of that key across every physical generation that could still be read has itself been removed or is provably unreachable (i.e., the tombstone is the oldest remaining trace of the key). This rule is engine-independent and is part of the shared conformance suite (Section 4.3).

### 7.4 Consistency guarantees (what RubixDB promises)

These are the guarantees the Unified Read Layer and Query Layer are built against; anything not listed here is explicitly *not* promised in v1.

- **Read-your-writes**: within a single session/connection, a read issued after a write it depends on is guaranteed to observe that write (the session tracks the LSN of its last write and reads `as_of_seq ≥` that LSN by default).
- **Ordering**: writes are made durable and become visible in LSN order; no write is ever visible before an earlier-LSN write to the same key.
- **Snapshot reads**: `snapshot()` fixes an `as_of_seq`; all reads through that snapshot handle observe a consistent point-in-time view across all partitions touched, regardless of which engines back them, for as long as the snapshot handle is held.
- **Delete visibility**: a delete is visible (i.e., subsequent reads return `None`) as soon as its WAL write is durable, exactly like a `put`.
- **Cross-partition atomicity**: not guaranteed in v1 beyond a single partition's single-batch write (a batch of puts/deletes targeting one partition is applied atomically — all-or-nothing — via the engine's normal write path). Multi-partition transactions are Future Work.
- **Migration-time consistency**: during `MIGRATING` (Section 13.1), reads continue to observe the pre-migration engine's data (frozen at the freeze-point LSN) until the atomic ownership switch; no read ever observes a partially-migrated, mixed-engine view of a single partition. This is the specific guarantee that makes offline migration safe to reason about.
- **Isolation level**: RubixDB v1 provides snapshot-read consistency for reads issued against an explicit snapshot, and read-committed-equivalent behavior (always the latest durable version as of read time) for ordinary reads. No serializable multi-statement transactions.

### 7.5 What the Unified Read Layer must never do

Silently drop a delete because it originated on a different physical engine than the one currently serving reads for that key range; return two different values for the same key from two different engines during a migration window; or resolve version conflicts by any mechanism other than LSN comparison (e.g., never by "generation number," "physical write time," or wall-clock timestamp — those are metadata, not authority).

---

## 8. Write-Ahead Log and Recovery Contract

### 8.1 WAL record format (logical, engine-independent)

```
WalRecord {
  seq: u64,             // global LSN
  partition_id: Uuid,
  op: PUT | DELETE | CHECKPOINT_MARKER | ENGINE_SWITCH,
  key: Bytes,
  value: Option<Bytes>,
  crc: u32,
}
```

`ENGINE_SWITCH` is written by the Migration Manager at the atomic ownership switch (Section 13.1) and is what lets recovery determine, without consulting anything but the WAL and metadata, which engine generation was authoritative at any point in the log.

### 8.2 Durability boundary

```
Write → WAL append → fsync (per configured durability policy) → Durability Boundary → apply to engine in-memory state
```

A write is acknowledged to the caller only after the WAL record is durable (fsynced), per Section 7.4's ordering guarantee. Durability policy (fsync every write vs. group-commit on an interval) is a tunable operational parameter, not a per-engine concern — it is enforced once, at the WAL layer, uniformly.

### 8.3 Recovery procedure

```
Database Start
  → Read Metadata (last known partition states/generations)
  → For each partition: locate last checkpoint, replay WAL from checkpoint's LSN forward
  → Rebuild in-memory engine state (MemTable / page cache / segment index, per engine)
  → Validate storage (checksum spot-check + generation/sequence_range cross-check against metadata)
  → Resume service
```

Replay is idempotent (Section 4.1, `recover`): every WAL record carries its LSN, and each engine tracks the highest LSN already durably applied to its own on-disk state, so replaying an already-applied record is detected and skipped. A crash during replay is safe to retry from the beginning for the same reason.

A partition whose last WAL record before the crash is an `ENGINE_SWITCH` with no matching completed migration record in metadata is recovered into `DEGRADED` (Section 3.1), not guessed into either engine — this is the one case recovery cannot resolve automatically, and it is handled explicitly rather than silently, per Section 13.3 (migration failure handling).

---

## 9. Unified Read Layer

### 9.1 Read path

```
Query
  → Partition Manager (resolve table → candidate partitions via key_range / predicate)
  → Identify Engine Per Partition (from Metadata Manager, current `engine` field)
  → Dispatch Engine-Specific Read (point_lookup / range_scan, per Section 4)
  → Merge (across partitions, and within a partition across a migration boundary if MIGRATING)
  → Version Resolution (highest seq ≤ as_of_seq per key, per Section 7.4)
  → Final Result
```

### 9.2 Merge semantics

For a range query spanning multiple partitions, results are merged in key order across partitions (partitions are, by construction, disjoint in key range under range partitioning, so this is a k-way merge, not a conflict resolution). Within a single partition, in the ordinary `ACTIVE` case there is exactly one engine and no merge is needed; the *only* case requiring intra-partition merge logic is a read arriving during `MIGRATING`, where the read layer must reliably choose the old engine's generation (per Section 7.4's migration-time consistency guarantee) rather than merge old and new — the two generations are never combined.

### 9.3 Engine-specific read execution

A range query against a B+Tree-backed partition performs an ordered index traversal from the start key. Against an LSM-backed partition, it scans the relevant SSTable key ranges (and the active MemTable) and merges by LSN. Against a Log-backed partition, it performs a sequential segment scan (optionally accelerated by segment-level min/max key metadata, Section 5.1, to skip whole segments). All three return an iterator conforming to the same `range_scan` contract (Section 4.1); the Unified Read Layer does not need to know which of the three actually ran.

---

## 10. Workload Analyzer

### 10.1 Principle

The analyzer measures *behavior*, not data shape. It does not inspect record contents; it observes the stream of operations against a partition and maintains rolling statistics.

### 10.2 Signals collected per partition

| Signal | Description |
|---|---|
| Write rate | Writes/sec (rolling). |
| Read rate | Reads/sec (rolling), split into point-lookup rate and range-scan rate. |
| Update ratio | Fraction of writes that are overwrites of an existing key vs. new keys (requires a lightweight existence check or Bloom-filter-based estimate — engine-specific hook). |
| Delete ratio | Fraction of writes that are deletes. |
| Key distribution | Sequential vs. random key pattern (measured via delta between consecutive write keys, classified against a configurable threshold). |
| Range-scan frequency | Fraction of reads that are range scans vs. point lookups, and average scan width. |
| Batch size | Average write-batch size. |
| Value-size distribution | p50/p95/p99 of value sizes (explicitly called out by the design review as needed alongside the other signals — an engine that's cheap for small values may not be cheap for large ones). |
| Memory pressure | Current MemTable / page-cache footprint relative to configured budget for the partition's engine. |
| Latency SLO | Configured or inferred target latency for this partition/table (explicitly called out by the design review; the cost model's read/write cost terms are meaningless without a target to evaluate them against). |
| Compaction pressure | (LSM-backed partitions) pending compaction backlog. |
| Storage throughput observed | Actual measured I/O throughput achieved, for calibrating cost estimates against reality over time. |

### 10.3 Observation window

Statistics are maintained over a **sliding window**, evaluated on a fixed cadence, not reactively per-operation (reacting per-operation would make the router itself a major source of write-path latency and instability).

**v1 default parameters** (explicitly tunable research parameters, not fixed constants — Section 12.3):

- Window: 60 seconds *or* 10,000 operations, whichever comes first.
- Evaluation cadence: every 60 seconds.
- Statistics use exponential decay across windows (not a hard reset) so a single anomalous window doesn't fully dominate or fully vanish the trend.

### 10.4 Output

Once per evaluation cadence, the analyzer emits a `WorkloadProfile` snapshot per partition (the signals above, aggregated over the current window) to the Cost Model. This snapshot is also what Observability (Section 15) records to make later routing decisions explainable.

---

## 11. Cost Model

### 11.1 Principle

The router does not ask "which engine is fastest in general." It asks "which engine is cheapest for *this partition's current, measured* `WorkloadProfile`." Cost is estimated per candidate engine, for the workload profile as observed — not benchmarked live against real candidate engines (that would require actually running the workload on all three engines simultaneously, which defeats the purpose).

### 11.2 Cost function

```
TotalCost(engine, profile) =
      Ww · WriteCost(engine, profile)
    + Wr · ReadCost(engine, profile)
    + Ws · SpaceCost(engine, profile)
    + Wc · CompactionCost(engine, profile)
```

Where `Ww, Wr, Ws, Wc` are configurable weights (defaulting to equal weighting, tunable per deployment/table via the `latency SLO` and operator priorities — e.g., a latency-sensitive table weights `Wr` higher; a space-constrained deployment weights `Ws` higher) and each term is a calibrated function of the workload profile's signals, per engine:

- `WriteCost(engine, profile)`: dominated by write rate and key-distribution (sequential writes are near-free for Log and LSM, expensive for B+Tree due to page-split churn under random insertion; random writes are cheap for LSM's append-only MemTable path, expensive for B+Tree, awkward for Log which has no update semantics at all).
- `ReadCost(engine, profile)`: dominated by point-lookup rate, range-scan frequency/width, and update ratio (high update ratio means more versions to skip through on LSM reads; B+Tree point lookups stay flat regardless; Log point lookups degrade with segment count / data size since there's no index).
- `SpaceCost(engine, profile)`: dominated by delete/update ratio (space amplification from retained old versions and tombstones — worst on LSM pre-compaction, near-zero on B+Tree which overwrites in place, grows unbounded on Log until segment GC).
- `CompactionCost(engine, profile)`: zero for Log and B+Tree (no compaction concept as defined here), a function of write rate and current compaction backlog for LSM.

Each per-engine cost function is a small set of calibrated coefficients (fit against the benchmark protocol's Section 17 measured amplification and latency numbers for each engine in isolation, *not* invented) — this is precisely why Section 1.7's build order requires real single-engine benchmark data before the cost model is written.

### 11.3 Two-decision router (see Section 12) consumes this model's output as:

```
best_engine = argmin_engine TotalCost(engine, current_profile)
current_cost = TotalCost(current_engine, current_profile)
improvement = (current_cost - TotalCost(best_engine, current_profile)) / current_cost
```

`improvement` is the sole numeric input to the hysteresis gate in Section 12.2.

---

## 12. Adaptive Router

### 12.1 Two-decision model

The router is deliberately split into two independent decisions to avoid the circular dependency of folding migration cost into engine selection:

```
Decision 1 (Cost Model, Section 11):
  Which engine is theoretically cheapest for this partition's
  current workload, ignoring migration cost entirely?

Decision 2 (Hysteresis Gate, Section 12.2):
  Is the improvement large enough, and persistent enough,
  to justify paying the migration cost to get there?
```

Decision 1 runs every evaluation cycle (Section 10.3) for every `ACTIVE` partition and is cheap (pure arithmetic over the current `WorkloadProfile`). Decision 2 is the only place migration is actually triggered.

### 12.2 Hysteresis policy

A migration is triggered only when **all** of the following hold:

1. `best_engine != current_engine`.
2. `improvement > improvement_threshold` (v1 default: **20%**).
3. Condition 1 and 2 have both held for `persistence_cycles` **consecutive** evaluation cycles (v1 default: **3** consecutive cycles, i.e., ~3 minutes at default 60s cadence).
4. The partition is currently `ACTIVE` (not already `MIGRATING`, `DEGRADED`, `CREATING`, or `RETIRING`) — see Section 3.1.
5. No migration of this partition has completed within the cooldown window (v1 default: **10 minutes**) — an explicit anti-thrash floor independent of the persistence check, to bound worst-case migration frequency even under a workload that oscillates on a period longer than the persistence window.

If the best engine changes to a *different* candidate before the persistence requirement is met, the consecutive-cycle counter resets — persistence is measured against a single consistent `best_engine` choice, not merely "some engine other than current."

### 12.3 Why these values are research parameters, not constants

`improvement_threshold = 20%`, `persistence_cycles = 3`, window = 60s/10k-ops, and `cooldown = 10min` are the specification's starting point, carried over from the design review, and are exactly what Section 17's ablation studies (hysteresis on/off, threshold sweep, window-size sweep) exist to validate or revise. They are configuration, not hardcoded — every deployment/table may override them, and the benchmark protocol is expected to produce evidence for different production defaults before v1 ships.

### 12.4 What the hysteresis gate prevents

Without it, a workload that alternates between write-heavy and range-heavy on a period close to the evaluation cadence would otherwise cause:

```
LSM → Log → LSM → Log → ...
```

— a partition perpetually mid-migration, which is strictly worse than picking either engine and staying put, since it pays migration cost continuously while never benefiting from either engine's steady-state behavior.

---

## 13. Migration Manager

### 13.1 Offline migration protocol (v1)

```
Current Partition (status = ACTIVE, engine = E_old)
        │
        ▼
Freeze Writes            — new writes to this partition are queued/rejected
        │                   with a retriable error; in-flight writes drain.
        ▼
Allow Reads From Old Engine  — status → MIGRATING; reads continue against
        │                       E_old uninterrupted (Section 7.4).
        ▼
Build Destination Representation
        │                   — E_new instance built from E_old.export_iterator()
        │                     (Section 4.1), an as-of-snapshot consistent scan
        │                     at the freeze-point LSN.
        ▼
Validate Destination      — row count / key-range / checksum cross-check
        │                     between E_old snapshot and E_new build output.
        ▼
Atomic Ownership Switch   — single Metadata Manager CAS update (Section 6.3):
        │                     engine: E_old→E_new, generation += 1,
        │                     status: MIGRATING→ACTIVE. WAL ENGINE_SWITCH
        │                     record written (Section 8.1) as part of the
        │                     same durable step.
        ▼
Resume Writes             — queued/new writes now apply to E_new.
        │
        ▼
Retire Old Representation — E_old's physical generation is marked RETIRING,
                              kept until no snapshot_refs (Section 6.1) still
                              reference it, then physically reclaimed.
```

This directly resolves the read-availability gap identified in the design review: reads are never blocked for the duration of the build+validate steps (the expensive part), only for the short freeze-writes step and the near-instantaneous atomic switch.

### 13.2 Migration state machine (nested inside `status = MIGRATING`)

| State | Description | Exit conditions |
|---|---|---|
| `FREEZING` | Writes being drained/rejected. | → `BUILDING` once drained. |
| `BUILDING` | `export_iterator` → destination engine construction in progress. | → `VALIDATING` on completion; → `ABORTING` on engine error. |
| `VALIDATING` | Cross-checking destination against source snapshot. | → `SWITCHING` on pass; → `ABORTING` on mismatch. |
| `SWITCHING` | Atomic metadata CAS + WAL `ENGINE_SWITCH` in flight. | → `ACTIVE` (parent state) on commit; retried on CAS conflict (Section 6.3). |
| `RETIRING_OLD` | Post-switch, waiting on `snapshot_refs` drain for old generation. | → reclaim, no further metadata state needed. |
| `ABORTING` | Validation or build failure. | → `ACTIVE` on old engine (migration abandoned, old generation untouched — since writes never touched `E_new` before `SWITCHING`, abort is always safe) — see Section 13.3. |

The `migration_state` field on the partition record (Section 6.1) holds the current nested state plus enough detail (target engine, freeze-point LSN, build progress) to resume or safely abort after a crash.

### 13.3 Failure handling

Because `E_old` is never mutated and writes never reach `E_new` before the `SWITCHING` step commits, every state up through `VALIDATING` is trivially abortable: discard the partial `E_new` build, return the partition to `ACTIVE` on `E_old`, resume writes, and log the aborted attempt (feeding into Observability, Section 15, as a migration-abort event — repeated aborts against the same target engine are itself a signal worth alerting on). The only state requiring crash-recovery logic beyond "discard and retry" is `SWITCHING` itself, because it is the one durable, atomic transition — Section 8.3 covers the exact recovery rule (a WAL `ENGINE_SWITCH` with no corresponding completed metadata transition recovers to `DEGRADED`, requiring explicit remediation rather than a guess).

### 13.4 Online migration (Future Work)

Snapshot + replay, dual-write during a cutover window, and rollback are deferred to a later version (Section 19). v1's offline protocol is judged acceptable because the freeze-writes window is bounded to drain time only (typically sub-second to low-single-digit seconds), not to the build+validate duration.

---

## 14. Query Layer

### 14.1 Pipeline

```
SQL / Simple Query
        │
        ▼
     Parser
        │
        ▼
      AST
        │
        ▼
  Logical Plan
        │
        ▼
   Optimizer  ── partition pruning (key_range vs. predicate),
        │         predicate/projection push-down to engines
        ▼
Engine Selection   ── resolved per-partition from Metadata Manager,
        │              not chosen by the query layer itself
        ▼
 Physical Plan
        │
        ▼
   Execution
```

The optimizer's scope in v1 is partition pruning and single-partition push-down (filters and projections handed to the engine's `range_scan`/`point_lookup` where the engine can exploit them — e.g., a B+Tree can seek directly to a filtered key prefix; an LSM/Log engine may only be able to push down projection, not the filter, without a secondary index). Multi-table join planning is out of scope for v1 (Section 0.1).

### 14.2 Simple Human Interface

The previously-scoped plain-English interface (e.g., `SHOW employees` as sugar for `SELECT * FROM employees;`) is a **syntactic front end only** — it produces the same AST/Logical Plan as the SQL parser and shares 100% of the pipeline from the Logical Plan stage onward. It must remain a deterministic, grammar-defined parser (not an AI/LLM-based interpretation layer) so that its behavior is testable and its failure mode is a parse error, not a silently-wrong query.

---

## 15. Observability

### 15.1 Principle

A production database must be able to justify its own decisions after the fact. For RubixDB specifically, the question "why did this partition end up on engine X" must always have a concrete, recorded answer — not a plausible-sounding guess.

### 15.2 Recorded per partition, per evaluation cycle

Current engine, the full `WorkloadProfile` snapshot (Section 10.4), the computed `TotalCost` for all three candidate engines, `best_engine`, `improvement`, and the hysteresis gate's running consecutive-cycle counter. Retained on a rolling basis (configurable retention; default long enough to cover several migration cooldown windows) specifically so that "why did RubixDB choose LSM for partition P001 at 14:32" is answerable by looking up that cycle's recorded state, not by re-deriving it.

### 15.3 Recorded per migration

Trigger reason (which cost/improvement values crossed the gate), start/end timestamps per state in Section 13.2's state machine, build duration, validation result, and outcome (`COMMITTED` / `ABORTED` with reason).

### 15.4 System-level metrics

Page I/O, WAL throughput (writes/sec and bytes/sec), compaction activity (LSM partitions), cache hit rate, and read/write latency including p50/p95/p99, both system-wide and per-partition.

### 15.5 Explainability contract

Every automated routing or migration decision must be reconstructable from Sections 15.2–15.3's recorded data alone, without needing to inspect engine internals or re-run the cost model against reconstructed state. This is treated as a hard requirement, equivalent in priority to a correctness invariant, because an adaptive system that cannot explain itself is not operable in production regardless of how good its decisions are.

---

## 16. Security and Operational Concerns

*(Scoped narrowly for v1 — RubixDB is an embedded engine, not a network service, so the surface area is smaller than a client/server database, but it is not zero.)*

- **At-rest protection**: WAL segments, SSTables, B+Tree pages, and Log segments are treated as sensitive by default. v1 provides an optional encryption-at-rest hook at the Storage Manager / File Manager layer (Section 8), applied uniformly below all three engines rather than per-engine, so encryption is not something each engine implementer reasons about separately.
- **Access control**: as an embedded engine, RubixDB inherits the host process's OS-level file permissions for its data directory; it does not implement its own user/auth model in v1 (that belongs to whatever service embeds RubixDB). This boundary is explicit so it isn't silently assumed away.
- **Integrity**: every WAL record and every engine-level physical block (segment, SSTable block, B+Tree page) carries a checksum (Section 8.1's `crc` field, and engine-specific equivalents), checked on read and on recovery (Section 8.3's "Validate storage" step), so silent corruption is detected rather than propagated into query results.
- **Operational safety of migration**: the freeze-writes window (Section 13.1) has a configurable maximum duration; if build+validate cannot complete and commit within an operator-configured bound under abnormal conditions, the migration is aborted (Section 13.3) rather than left holding writes indefinitely — availability of writes takes priority over completing a routing optimization.
- **Resource isolation**: the Workload Analyzer and Cost Model run on a bounded, low-priority background schedule (tied to the evaluation cadence, Section 10.3) and must not compete materially with foreground read/write latency — this is a stated non-functional requirement, validated by the benchmark protocol's latency measurements under the adaptive configuration (Section 17.3) explicitly including analyzer/router overhead, not just engine overhead.

---

## 17. Benchmark and Research Protocol

This protocol is designed *before* the adaptive router is implemented (per Section 1.7's build order) because the cost model's coefficients (Section 11.2) are calibrated from its single-engine results, and because "RubixDB adapts well" is a claim that only means something against a defined control group.

### 17.1 Control group

```
Fixed Log        — all partitions pinned to Log engine
Fixed LSM        — all partitions pinned to LSM engine
Fixed B+Tree     — all partitions pinned to B+Tree engine
Static Router    — engine chosen once at partition creation from
                    workload type (if known), never re-evaluated
Adaptive RubixDB — full system: analyzer + cost model + router +
                    migration, as specified in Sections 10–13
```

`Static Router` exists specifically to isolate the value of *adaptivity* from the value of *having three engines to choose from* — a static-but-correct initial choice is a much fairer comparison than only comparing against single fixed engines.

### 17.2 Workloads

YCSB-style workload mixes (read-heavy, write-heavy, mixed, scan-heavy) as a baseline, plus explicit **workload transitions** within a single run — e.g., append-heavy → update-heavy → range-heavy, each phase held long enough to exceed several evaluation windows (Section 10.3) — because adaptivity specifically cannot be demonstrated against a single unchanging workload; a static router wins trivially there by definition.

### 17.3 Measurements

Throughput; p50/p99 latency; write amplification; read amplification; space amplification; compaction overhead; migration overhead (frequency, duration, freeze-write impact); memory usage; and **time-to-adapt** (wall-clock or operation-count from a workload-shape transition to the router completing a migration that improves cost, and the residual cost gap immediately after that migration completes).

### 17.4 Ablations

Hysteresis enabled vs. disabled (to demonstrate Section 12.4's thrash-prevention claim with a number, not just an argument); observation-window size sweep (Section 10.3's 60s/10k-op defaults vs. alternatives); `improvement_threshold` sweep around the 20% default (Section 12.3); and cost-model weight (`Ww, Wr, Ws, Wc`, Section 11.2) sensitivity. Each ablation must isolate exactly one variable against the same workload transition set from Section 17.2, so results are attributable to the specific mechanism being tested rather than to overall system variance.

### 17.5 Acceptance bar

A claim in project documentation or marketing that RubixDB "adapts to workload changes" is only made once Section 17.2–17.4's transition workloads show Adaptive RubixDB converging toward (not necessarily matching, given migration overhead) the best `Fixed *` engine's steady-state throughput/latency for each phase, within a bounded time-to-adapt, and the hysteresis-disabled ablation demonstrably thrashes where the hysteresis-enabled configuration does not.

---

## 18. Production-Level Design Boundaries (Module Summary)

| Module | Responsibility | Depends on |
|---|---|---|
| Log / LSM / B+Tree Engines | Implement the shared storage-engine contract (Section 4) for one physical strategy each. | WAL/Recovery contract (§8) |
| Workload Analyzer | Observes partition behavior; emits `WorkloadProfile` per evaluation cycle. | Metadata Manager (§6) |
| Cost Model | Estimates per-engine `TotalCost` from a `WorkloadProfile`. | Workload Analyzer (§10), calibration data (§17) |
| Adaptive Router | Two-decision engine selection + hysteresis gate (§12). | Cost Model (§11) |
| Migration Manager | Executes the offline migration state machine (§13) when the router triggers it. | Storage Engine Contract's `export_iterator` (§4.1), Metadata Manager (§6.3) |
| Metadata Manager | Owns partition/table/schema records, versioning, and ownership (§6). | WAL/Recovery contract (§8) for its own durability |
| Unified Read Layer | Hides physical engine differences from callers; merges and resolves versions (§9). | Metadata Manager (§6), Storage Engine Contract (§4) |
| Query Layer | Parses, plans, optimizes (partition pruning/push-down), executes (§14). | Unified Read Layer (§9), Metadata Manager (§6) |
| WAL / Recovery | Common durability foundation, engine-independent record format (§8). | — (foundational) |
| Observability | Records workload/cost/routing/migration state for explainability (§15). | Workload Analyzer, Cost Model, Router, Migration Manager |

---

## 19. Open Questions and Future Work

Explicitly deferred, not forgotten:

- **Online migration** (Section 13.4): snapshot + replay with dual-write and a bounded cutover window, plus a defined rollback procedure if cutover validation fails after dual-write has started.
- **Multi-partition / multi-statement transactions**: current spec guarantees stop at single-partition batch atomicity (Section 7.4); a cross-partition transaction protocol (likely 2PC-style, given the single-node constraint could still simplify this considerably) is future work.
- **Schema evolution**: `schema_version` is tracked (Section 6.2) but the actual migration-of-old-partitions-to-new-schema mechanism is not specified in v1.
- **Partition splitting/merging by size**, independent of engine migration — noted in Section 2 as an orthogonal mechanism, not specified here.
- **Distributed RubixDB**: replication, sharding across nodes, and distributed consensus for metadata are entirely out of scope for this document (Section 0.1) and would likely require re-examining several "single global LSN" assumptions in Section 7.
- **Engine auto-discovery**: whether a fourth engine type could be added without a spec revision (the contract in Section 4 is designed to make this possible in principle, but no plug-in registration mechanism is specified in v1).

---

## 20. Glossary

- **LSN**: Logical Sequence Number — the single global, monotonically increasing write-order counter (Section 7.1).
- **Generation**: a partition's physical-rebuild counter, incremented on every engine change (Section 3).
- **Tombstone**: a logical delete marker retained until provably safe to remove (Section 7.3).
- **WorkloadProfile**: the Workload Analyzer's per-partition, per-evaluation-cycle statistics snapshot (Section 10.4).
- **Hysteresis gate**: the persistence + threshold + cooldown check that must pass before a migration is triggered (Section 12.2).
- **Freeze window**: the short, write-blocked span at the start of migration, bounded to write-drain time only (Section 13.1).
