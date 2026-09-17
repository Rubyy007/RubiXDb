# RubiXDB Phase 4B — Architecture

## 0. Precondition status (operating brief §1-§2)

See `PHASE4B_ADR.md` ADR-P4B-0 for the full accounting. Summary: Phase
3C's WAL certification remains **Deferred** (never completed); Phase
4A's own certification explicitly named **"NOT YET READY — BLOCKERS
REMAIN."** Neither blocker touches WAL/coordinator internals or the
MemTable's own correctness, both are carried forward honestly rather
than hidden, and one (the WAL-vs-WAL+MemTable performance comparison)
is subsumed by this phase's own required §40-41 benchmarks. Phase 4B
proceeds on this explicit, documented, provisional basis — identical
in kind to how Phase 4A itself proceeded on top of a then-incomplete
Phase 3C.

## 1. Scope

```text
Before:  Logical Writers -> Coordinator -> Group Commit -> Durable WAL -> MemTable -> Immutable MemTable
After:   Logical Writers -> Coordinator -> Group Commit -> Durable WAL -> MemTable -> Immutable MemTable -> RUBIC SSTable
```

Phase 4B implements the RUBIC SSTable exactly as already specified —
"Status: Final — ready for implementation" —
`RubixDB-LSM-Engine-Specification-v1.0.md` §2-§3, plus the flush
integration and read-path extension needed to make it reachable from
`LsmEngine`. Per the user's explicit decision (`PHASE4B_ADR.md`
ADR-P4B-1), it does **not** implement Manifest, WAL purge-on-flush, or
Compaction — those remain future phases.

## 2. Preserved architecture (unchanged, no exception found)

Per operating brief §2/§3, none of the following are redesigned this
phase: `BatchCoordinatorPool`, `GroupCommitter`, `durable_through`,
`LeaderFailureGuard`, `CoordinatorFaultPoint`, the WAL binary format,
WAL rotation, the WAL recovery contract, `wal::replay_streaming`,
`MemTable`'s data structure/API, or `LsmEngine`'s write-path ordering
(WAL append -> durability wait -> MemTable apply). The one additive
change to existing code is a single visibility export
(`PHASE4B_ADR.md` ADR-P4B-4) — no behavior change to any preserved
component.

## 3. The write path, extended with explicit new ownership

```text
                 WRITE PATH (unchanged through MemTable)

Logical Client -> Coordinator -> Group Commit -> Durable WAL -> Mutable MemTable -> Immutable MemTable
                                                                                          |
                                                                                          v
                                                                          Background Flush Thread (new)
                                                                                          |
                                                                                          v
                                                                          RUBIC SSTable Writer (new)
                                                                                          |
                                                                                          v
                                                                          sstables/{id}.sst.tmp (new)
                                                                                          |
                                                                              fsync, rename, dir fsync
                                                                                          |
                                                                                          v
                                                                          sstables/{id}.sst  (new, PUBLISHED)
                                                                                          |
                                                                                          v
                                                                  LsmEngine.sstables list (new, read-path source)
```

### Responsibilities, stated once

| Component | Responsible for | NOT responsible for |
|---|---|---|
| **Background flush thread** | Draining `immutables` oldest-first, building + publishing SSTables, dropping a flushed `Arc<MemTable>` from `immutables` once its SSTable is durably published | Durability of the underlying records (already durable via WAL before the memtable was ever frozen); WAL retention/purging (ADR-P4B-1) |
| **RUBIC SSTable Writer** (`src/sstable/writer.rs`) | Byte-exact construction of data blocks, bloom filter block, index block, footer, per `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2; temp-file-then-atomic-rename publication | Choosing *when* to flush (the flush thread's job); WAL/Manifest interaction of any kind |
| **RUBIC SSTable Reader** (`src/sstable/reader.rs`) | Bounded-memory, validated reads: footer/index/bloom eager, data blocks lazy; `get_versioned`, `range_scan_raw` | Merging across multiple sources (the `LsmEngine` read path's job, unchanged pattern from the spec's `ReadView`) |
| **`LsmEngine`** | Multi-source read merge (`active` -> `immutables` newest-first -> `sstables` newest-first); triggering freeze at capacity; owning the background flush thread's lifecycle | The SSTable byte format itself; WAL internals |

## 4. Read path extension

`LsmEngine::get(key)` (and a new `get_as_of`/range-scan surface, if
added this phase per the test plan) now walks, in strict recency
order: `active` MemTable -> each `immutables` entry newest-to-oldest
-> each `sstables` entry newest-to-oldest (bloom-filter
short-circuited per entry). The **first source with any version at
`seq <= as_of_seq`** is authoritative — a `Tombstone` there means "not
found," a `Put(v)` there means `Some(v)`, and no older source is ever
consulted once a hit is found. This is exactly
`RubixDB-LSM-Engine-Specification-v1.0.md` §4.2's `ReadView` merge
rule, extended from Phase 4A's MemTable-only version to include the
new `sstables` list — no new merge semantics invented.

`sstables: RwLock<Vec<Arc<SsTable>>>` (newest-first) is a new
`LsmEngine` field, populated at `open()` by the Section 3.3 directory
sweep (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`) and appended to (at
index 0) by the flush thread after each successful publish.

## 5. Flush sequence, explicit ownership handoff

```text
freeze_and_enqueue():                          (unchanged trigger point: active.is_full())
  1. old = active.write().take_and_replace_with(MemTable::new(...))
  2. frozen = old.freeze()                     // Arc<MemTable>
  3. immutables.write().push_front(frozen.clone())
  4. flush_queue.push(frozen)                  // wakes the background flush thread

background_flush_thread loop:
  1. frozen = flush_queue.pop_oldest()         // blocks when empty
  2. id = next_sstable_id.fetch_add(1)
  3. SsTable::write_from_memtable(&frozen, id, sstables_dir)
       -> sstables/{id}.sst.tmp -> fsync -> rename -> sstables/{id}.sst -> dir fsync
  4. sstable = SsTable::open(sstables/{id}.sst, id)?
  5. sstables.write().insert(0, Arc::new(sstable))
  6. immutables.write().retain(|m| !Arc::ptr_eq(m, &frozen))
     // (no wal.purge_before call — ADR-P4B-1)
```

The `ImmutableMemTable` is **never released** (dropped from
`immutables`) before step 6, i.e., strictly after its SSTable is fully
published (step 3 complete) and successfully reopened for reading
(step 4) — operating brief §24's ownership-handoff requirement,
satisfied by construction: nothing removes an entry from `immutables`
except step 6, and step 6 never runs before step 5's `Arc::new`
succeeds.

## 6. Flush failure handling (operating brief §25)

| Failure | Response | `immutables` entry | Caller-visible effect |
|---|---|---|---|
| Allocation failure during block/bloom/index build | Process-fatal (Rust default OOM abort), matching every other component in this codebase (`PHASE4A_FAILURE_MODEL.md`'s identical treatment of MemTable allocation) | N/A (process aborts) | N/A |
| Write/short-write/sync failure on the `.sst.tmp` file | `write_from_memtable` returns `Err`; the `.tmp` file is left on disk (unswept until next `open()`'s Section 3.3 sweep) | **Retained** — the flush thread logs/records the failure and retries the same `frozen` entry (bounded retry policy, Section 8) rather than dropping it | None yet — the record remains durable in the WAL and visible via `active`/`immutables` read path regardless |
| Rename failure | Same as above — `.tmp` file retained, `.sst` never created | Retained, retried | None |
| Post-rename, pre-directory-fsync crash | Indistinguishable from full success on restart (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4's note) — the `.sst` file is either present and valid, or (Windows no-op case) the rename may not have survived depending on filesystem/hardware; either way, the WAL was never purged (ADR-P4B-1), so the data is never at risk | N/A across a crash (recovery rebuilds `immutables` as empty, matching Phase 4A's existing recovery invariant, Section 7) | N/A |
| `SsTable::open` fails right after a successful build (self-check) | Treated as a build failure — the just-written file is not trusted merely because this process just wrote it; `open()`'s full validation runs unconditionally | Retained, retried (Section 8) | None |

A failed flush **never** produces a partially-published `.sst` file
under its final name (atomic rename is all-or-nothing) and **never**
silently discards the `ImmutableMemTable` — the table above shows
every failure path either retries or process-aborts, never both drops
the in-memory data and fails to persist it.

## 7. Crash-during-flush -> recovery (operating brief §26, §30)

Recovery is **unchanged from Phase 4A** at the MemTable/WAL level
(ADR-P4B-1): `LsmEngine::open` still runs `wal::replay_streaming`
first, into a fresh `active` MemTable, exactly as before. What's new:
before that, it runs the Section 3.3 directory sweep to populate
`sstables`. `immutables` starts empty after any restart, exactly as
Phase 4A already documents (`PHASE4A_FAILURE_MODEL.md` §3) — this
remains true unchanged, because nothing about SSTable existence
changes what `immutables` means (it is still purely an in-memory,
pre-flush staging list with no on-disk representation of its own).

Concretely, for every crash point operating brief §26 lists:

- **Before SSTable creation / after temp file creation / during block,
  index, or footer write / before sync**: the `.sst.tmp` (if any) is
  swept away unconditionally on the next `open()`. The frozen
  memtable's data is still fully in the WAL (it was written there
  before ever reaching the memtable) and gets replayed into the fresh
  `active` MemTable by the unchanged `wal::replay_streaming` call —
  identical reasoning to `PHASE4A_FAILURE_MODEL.md` §3's "during
  freeze" row, extended one step further down the pipeline.
- **After sync, before rename**: same as above — still a `.sst.tmp`,
  still swept, still fully recovered from WAL.
- **During rename**: `std::fs::rename` is atomic with respect to
  observers on both platforms (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`
  §4) — either it happened (file is now `.sst`, valid) or it didn't
  (`.sst.tmp`, swept). No third state.
- **After rename, before directory fsync**: on Unix, the directory
  fsync closes this window; on Windows, this is the one documented gap
  named in `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §4 — but even in
  the worst case (the rename's directory-entry update itself did not
  survive), no data is lost, only that one SSTable's read-path
  availability, per ADR-P4B-1/ADR-P4B-2.
- **After publication**: the file validates normally on the next
  `open()`'s sweep and joins `sstables`.

**MemTable data never disappears** across any of the above — the
invariant `PHASE4A_FAILURE_MODEL.md` §3 already established ("after
restart, the recovered MemTable reflects exactly the durable WAL
state") is not weakened by anything Phase 4B adds, precisely because
Phase 4B never changes what the WAL retains or what gets replayed.

## 8. Bounded flush retry (new this phase, small and explicit)

A flush that fails (Section 6) is retried a bounded number of times
(configurable, default 3) with a short backoff, then, if still
failing, the flush thread logs the failure, leaves the entry at the
head of the queue, and continues attempting it on a timer rather than
either (a) dropping the data (never acceptable — Section 6) or (b)
spinning a tight retry loop that starves other work. This is
deliberately simple — a full flush-health/alerting subsystem is out of
scope this phase — and is explicitly named as a minimal, sufficient
mechanism rather than a complete operational story.

## 9. Memory lifecycle (operating brief §44)

```
Mutable MemTable        (bounded by memtable_max_size_bytes, unchanged)
Immutable MemTable(s)   (bounded by max_immutable_memtables, unchanged bound,
                          now transient rather than permanent -- Section 5 drains it)
SSTable writer buffers  (bounded: one block buffer at a time, <= target_block_size
                          plus at most one oversized record's own buffer, per
                          RUBIC_SSTABLE_FORMAT_SPECIFICATION.md §2.4/§2.9 --
                          never the whole table materialized in memory)
SSTable reader buffers  (bounded: footer (72 bytes) + bloom filter block + index
                          block held for the table's lifetime; data blocks read
                          on demand, one at a time, never eagerly)
```

Once a flush's step 6 (Section 5) removes an `Arc<MemTable>` from
`immutables`, that memtable's memory is reclaimed as soon as every
other `Arc` clone referencing it (any in-flight `ReadView`-equivalent
snapshot taken by a concurrent reader before the drop) is itself
dropped — ordinary `Arc` semantics, verified with measured RSS in
`PHASE4B_PERFORMANCE.md`, not merely asserted.
