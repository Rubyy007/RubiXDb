# RubiXDB Phase 4A — Architecture

## 0. Precondition status (operating brief §1)

**The WAL certification (Phase 3C) was not complete at the time this
phase began.** `PHASE3C_TEST_RESULTS.md` was explicitly marked
"Document status: IN PROGRESS" — the true 4-hour-per-writer-level long
soak was (and, as this phase's own work proceeded, remained) running in
the background; the final performance re-verification and final
certification decision (§26) were deferred pending its completion.
Per this project's own "do not mark the WAL as certified if the
evidence does not support it" rule, Phase 4A proceeds on this explicit,
documented basis: **provisional, not certified**, justified because
Phase 4A does not modify the WAL/`GroupCommitter`/`BatchCoordinatorPool`
internals at all (§2 below) — the two efforts are independent, and the
soak continues gathering its own evidence in the background throughout
Phase 4A's own work. `PHASE4A_TEST_RESULTS.md`'s own final certification
decision (§26 there) is explicitly conditioned on Phase 3C's own
certification also landing clean — see that document.

## 1. Scope

Phase 4A extends the certified (pending §0) write path with a new,
purely in-memory storage layer:

```text
Before:  Logical Writers -> Dedicated Batch Coordinator -> Group Commit -> Durable WAL
After:   Logical Writers -> Dedicated Batch Coordinator -> Group Commit -> Durable WAL -> MemTable
```

Phase 4A implements the Memtable exactly as already specified —
"Status: Final — ready for implementation" — in `RubixDB-LSM-Engine-
Specification-v1.0.md` §1, plus the WAL-integration and recovery
machinery needed to keep it correctly populated and durable-consistent.
It does **not** implement RUBIC SSTable, Manifest, or Compaction
(operating brief §5, §37) — those remain Phase 4B+.

## 2. Preserved architecture (unchanged, no exception found)

Per operating brief §2, none of the following were redesigned — no
correctness defect was found in any of them during this phase's work:
`BatchCoordinatorPool`, `GroupCommitter`, `durable_through`,
`LeaderFailureGuard`, `CoordinatorFaultPoint`, the WAL binary format,
WAL rotation, or the WAL recovery contract (`open_for_recovery`,
`WalReplayResult`, `walk_segment`). **The MemTable is not a second
durability system** — it has no `fsync`, no independent crash-recovery
logic of its own, and no code path that can mark data durable that the
WAL has not already marked durable first (§5 below is the exact
ordering contract).

## 3. The write path, with explicit ownership per stage

```text
                 WRITE PATH

Logical Client
      |
      v
Dedicated Batch Coordinator      <- owns: request scheduling, batch formation
      |
      v
Group Commit                     <- owns: durability decision, durable_through watermark,
      |                              leader election, fsync
      v
Durable WAL                      <- owns: on-disk durability, sequence assignment,
      |                              rotation, crash recovery contract
      v
Mutable MemTable                 <- owns: current queryable database state (this seq's value
      |                              for every key), NOT durability
      v
Immutable MemTable                <- owns: a stable, frozen snapshot of past mutable state,
      |                              pending a future flush
      v
Future RUBIC SSTable (Phase 4B)   <- will own: persistent sorted immutable storage
```

### Responsibilities, stated once, not blurred

| Component | Responsible for | NOT responsible for |
|---|---|---|
| **MemTable** | Current mutable in-memory database state; ordered `(key, seq)` lookup and iteration within its own lifetime; its own memory accounting | Durability, sequence assignment, crash recovery, WAL format |
| **WAL / Group Commit** | Durability and crash recovery, sequence assignment (`GroupCommitter::append` is the sole assigner of `seq` — unchanged) | In-memory query state, snapshot reads, tombstone GC |
| **RUBIC (future SSTable)** | Persistent sorted immutable storage, once implemented | Anything Phase 4A implements — no code in this phase depends on or anticipates a specific RUBIC SSTable byte layout beyond the already-specified, not-yet-built one referenced in `RUBIC_FORMAT_SPECIFICATION.md` §2 |

### Ownership, explicitly

- **Data ownership**: the MemTable owns its own `(key, seq) -> value`
  entries once inserted — no shared mutable ownership with the WAL
  (values are copied into the MemTable at apply time, not borrowed from
  WAL-owned buffers, avoiding a lifetime coupling between two
  components with very different lifecycles).
- **Sequence ownership**: unchanged — `GroupCommitter`/`FileWal` remain
  the sole assigner of `seq` (operating brief §8). The MemTable never
  generates a sequence number; it only ever consumes one already
  assigned by the durable write path or by WAL replay during recovery.
- **Durability ownership**: unchanged — `durable_through` (`GroupCommitter`)
  remains the single source of truth for "is this `seq` durable." A
  caller's logical write is acknowledged only after WAL durability is
  confirmed (§5), never merely after MemTable insertion.
- **Memory ownership**: the MemTable (mutable and each immutable
  instance) owns its own memory accounting independently (operating
  brief §17) — the WAL's own memory/queue accounting
  (`BatchCoordinatorStats`) is unrelated and unchanged.
- **Recovery ownership**: the WAL remains solely responsible for
  determining *what is durable* (unchanged recovery contract); the new
  Phase 4A recovery integration (`PHASE4A_MEMTABLE_ARCHITECTURE.md` §5)
  is responsible only for *replaying* what the WAL already determined
  durable into a fresh MemTable — it introduces no new durability
  decision of its own.

## 4. What Phase 4A adds, by module

- **`src/memtable/mod.rs`**: `MemTable`, `MemtableValue`, per
  `RubixDB-LSM-Engine-Specification-v1.0.md` §1 — see `PHASE4A_
  MEMTABLE_ARCHITECTURE.md` for the full design.
- **`src/wal/mod.rs` (additive)**: a new, bounded-memory WAL replay API
  (`replay_streaming` or equivalent — see `PHASE4A_MEMTABLE_
  ARCHITECTURE.md` §5) — additive only, `open_for_recovery`/
  `WalReplayResult`/`walk_segment` unchanged, per operating brief §24's
  explicit instruction.
- **`src/lsm/mod.rs`**: the Phase-4A-scoped subset of `RubixDB-LSM-
  Engine-Specification-v1.0.md` §4's `LsmEngine` facade — write-path
  integration (WAL append -> durability wait -> MemTable apply),
  freeze-to-immutable, and recovery, **without** the `sstables`/
  `manifest`/`next_sstable_id` fields or any compaction logic (Phase
  4B+). Extended, not replaced, when Phase 4B adds SSTable/Manifest.

## 5. WAL -> MemTable durability ordering (operating brief §22 — the exact contract)

```text
logical operation
      |
      v
WAL append           (GroupCommitter::append — assigns seq, memory-speed)
      |
      v
WAL durability        (GroupCommitter::await_durable — blocks until durable_through >= seq)
      |
      v
MemTable apply         (MemTable::insert — only after durability is confirmed)
      |
      v
logical completion    (caller's write is acknowledged only now)
```

**A logical write is never acknowledged before WAL durability is
confirmed, and MemTable visibility never implies WAL durability by
itself** — the MemTable is applied strictly *after* the same
`await_durable` call the pre-existing write path already performs, not
as a substitute for it, not concurrently with it, and not before it.
This is the one non-negotiable ordering rule Phase 4A's entire design
serves — see `PHASE4A_FAILURE_MODEL.md` for what happens when a crash
lands at every point along this chain.
