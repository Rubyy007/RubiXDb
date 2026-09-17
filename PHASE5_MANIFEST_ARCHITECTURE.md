# RubiXDB Phase 5 — Manifest Architecture

## 0. Precondition status

Verified directly from source before any design work, not assumed:

- `git log`/`git status`: working tree clean at Phase 4B's final commit
  (`416573e`, "cross-phase doc updates"), 8 commits ahead of
  `origin/master`.
- `PHASE4B_TEST_RESULTS.md` §10: **RUBIC SSTABLE READY FOR MANIFEST**,
  conditioned on Phase 3C's long-soak certification, which remained
  "Deferred."
- `PHASE3C_TEST_RESULTS.md` §12: still "Deferred" — the true
  4-hour-per-writer-count long soak (`long_soak_test -- 100 14400 ...`
  then `... 1000 14400 ...`, ~8 hours total) never completed.
- `PHASE4B_PERFORMANCE.md` §3.3/§5: the 100-writer "flush faster than
  no-flush" oddity was recorded as open, not root-caused.

**Release-gate audit performed this phase** (see `PHASE5_PERFORMANCE.md`
for full detail):
- The Phase 3C long soak was launched in the background, then
  deliberately stopped and deferred to the end of this phase's own work
  after a sequencing mistake (killing leg 1 early caused the wrapper
  script to advance straight into leg 2 — see the retrospective in
  `PHASE5_ADR.md`) — running an 8-hour background soak *before* this
  phase's own required clean benchmarks would have contaminated every
  one of them, repeating the exact mistake `PHASE4A_ADR.md` ADR-P4A-6
  was written to avoid. It is relaunched, uninterrupted this time, only
  after every clean measurement this phase needs has already been
  captured (`PHASE5_PERFORMANCE.md` §6) — genuinely attempted, not
  silently dropped, but its full multi-hour result is not available by
  the time this document set is finalized; recorded honestly as
  **IN PROGRESS**, not fabricated into a PASS.
- The 100-writer anomaly was investigated with a dedicated,
  purpose-built ablation (`examples/freeze_ablation_test.rs`) that
  isolates freeze-frequency from flush I/O. Result: freeze frequency
  alone does **not** reproduce the speedup (a freeze-and-discard-only
  variant measures at or slightly below the no-freeze baseline, never
  above it) — this **rules out** the bounded-`BTreeMap`-depth
  hypothesis `PHASE4B_PERFORMANCE.md` floated. The real anomaly (real
  flush I/O measuring faster than no flush, consistently, at 100
  writers) remains genuinely unexplained by this phase's own
  investigation and is recorded as an unresolved, non-blocking
  observation, not papered over with an unsupported story
  (`PHASE5_PERFORMANCE.md` §5).

Phase 5 proceeds on this same explicit, provisional basis every prior
phase since Phase 4A has used.

## 1. Ownership boundaries — stated once, never blurred

| Component | Owns | Does NOT own |
|---|---|---|
| **WAL** (`src/wal/`) | Durability of individual writes; sequence assignment (`GroupCommitter::append` remains the sole assigner, unchanged); crash recovery of the raw record stream; segment rotation; **now also**: durably recording a `CHECKPOINT_MARKER` when told to, and physically removing fully-covered segments when told a safe watermark (`purge_before`) | Whether an SSTable is valid; whether a checkpoint is safe to act on; which SSTables are live |
| **RUBIC SSTable** (`src/sstable/`) | Immutable, persisted, sorted data for one flush's worth of records; its own on-disk validity (checksums, footer) | Whether it is *currently* considered live by the engine; anything about the WAL or other SSTables |
| **Manifest** (`src/manifest/`, new this phase) | **The authoritative metadata**: which SSTable ids are currently live, and which sequence has been durably checkpointed | The bytes of any SSTable; the bytes of any WAL record; sequence *assignment* (it only ever *records* a sequence already assigned by the WAL) |
| **MemTable** (`src/memtable/`) | Current mutable/frozen-immutable in-memory state | Durability, persistence, liveness metadata |
| **LsmEngine** (`src/lsm/`) | Orchestration: wiring the above together in the correct order, exposing the read/write API, owning the background flush thread's lifecycle | Any of the above components' own internal correctness — it composes them, it does not reimplement their guarantees |

**The critical, non-negotiable boundary this phase adds**: an SSTable
being present and valid on disk is *never*, by itself, sufic ient
grounds to (a) treat it as live, or (b) treat any sequence as
checkpointed. Both require an explicit, durable Manifest edit. This is
the direct, structural fix for the exact gap `PHASE4B_ADR.md`
ADR-P4B-1 named and deliberately left open: "a footer-derived
checkpoint... would have been a hidden reimplementation of Manifest
logic." Now that a real Manifest exists, that heuristic is retired, not
extended.

## 2. What is frozen from Phase 4B, unchanged

Per the operating brief's own explicit instruction, and because
Phase 4B's own crash-cycle evidence (140/140 real external-process
kills, zero failures) gives no reason to touch it:

- `src/sstable/writer.rs`'s temp-file -> fsync -> atomic rename ->
  directory-fsync publication sequence — **byte-for-byte, step-for-step
  unchanged**.
- `src/sstable/reader.rs`'s plain-`File`-I/O, bounded-memory,
  positional-read design — unchanged (this question was separately,
  explicitly re-confirmed with the user during Phase 4B and is not
  reopened here).
- `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`'s byte layout — unchanged.
  Phase 5 adds no new SSTable-level field (no key-range, no level) —
  those live in the Manifest's `ADD_SSTABLE` edit only where the LSM
  spec actually puts them (i.e., nowhere; the spec's `ADD_SSTABLE` has
  no key-range field either, Section 3 of the format spec).
- The WAL binary format — **entirely unchanged**. `CHECKPOINT_MARKER`
  already existed since Phase 4A; Phase 5 makes durable, meaningful use
  of it without altering its on-disk shape.

What Phase 5 changes about Phase 4B's own code: `sstable::discover`'s
role narrows from "the liveness authority" to "a validation/sweep
helper the Manifest-driven startup path calls," and `LsmEngine`'s
`sstables` list is now populated by Manifest-authoritative ids rather
than by "every `.sst` file found in the directory" (Section 6).

## 3. Reconciling LSM Engine Spec §3.2/§4.4's two levels of description

**Flagged explicitly, not silently resolved**: §4.4's `freeze_and_
flush()` pseudocode does not literally show the `ADD_SSTABLE` Manifest
append as a separate line — it goes straight from `SSTable::write_
from_memtable(...)` to inserting into the in-memory `sstables` list.
But §3.2 ("Build sequence") explicitly defines that same `ADD_SSTABLE`
append + fsync as step 5 of "the build sequence" that step 1-4 (temp
file, fsync, rename, directory fsync) are also part of — i.e., §3.2
treats the *complete* publication unit (including the Manifest append)
as one indivisible sequence, and §2.7 step 5 cross-references §3
directly: "becomes a real SSTable via the atomic-rename discipline
defined there." Read together, §4.4's `SSTable::write_from_memtable(...)`
call is the black-box invocation of that *entire* §3.2 sequence,
including its own step 5 — not a sequence that omits step 5. This is a
textual cross-reference, not a coin-flip, so implementation proceeds on
this reading without a stop-and-ask; it is written out in full here so
the reasoning is auditable, not hidden.

## 4. `LsmEngine::open` startup ordering — the new, load-bearing sequence

This is the most subtle correctness property this phase adds, and it
did not exist as a question before this phase (Phase 4A/4B had no
Manifest to sequence against the existing `replay_streaming`-before-
`FileWal::open_for_recovery` lock-ordering constraint, `PHASE4A_ADR.md`
ADR-P4A-3).

**The constraint that must still hold**: `FileWal::open_for_recovery`'s
exclusive lock blocks a same-process shared-lock attempt (empirically
verified when the WAL's own locking was built) — so anything needing
only a *shared* lock must run, and release that lock, strictly before
`FileWal::open_for_recovery` is called.

**The new requirement Manifest recovery adds**: the WAL replay pass
(`wal::replay_streaming`) must discard any record with `seq <=
checkpoint.flushed_through_seq` (LSM Engine Spec §7.1 step 5) — so the
checkpoint value must be known *before* that pass runs. But Manifest
recovery is not purely read-only in general (Section 7.2's directory
sweep can *write* a recovered `ADD_SSTABLE` edit for an orphaned-but-
valid `.sst` file) — writes require the *exclusive* lock, which cannot
be acquired before `replay_streaming` runs without violating the
constraint above.

**Resolution**: split Manifest recovery into its own two phases,
matching the LSM spec's own two-step structure (§7.1 step 1 "Replay
MANIFEST" vs. step 2 "Sweep sstables/ directory" are already textually
distinct steps):

```
LsmEngine::open(dir, wal_config, pool_config, lsm_config):

  1. manifest::replay_readonly(dir)                         [SHARED lock, read-only]
       -> ManifestState { live_sstables, ever_added, checkpoint }
       -- Corrupt (non-tail): Err, open() fails closed (RUBIC_MANIFEST_
          FORMAT_SPECIFICATION.md §4.1). Absent file: fresh engine,
          checkpoint = None (== flushed_through_seq 0).
       -- Lock acquired and released within this call, before step 2,
          for the exact same reason `replay_streaming` already must run
          before the exclusive lock (ADR-P4A-3) -- two SHARED locks
          from different logical steps never conflict with each other,
          only with an EXCLUSIVE holder.

  2. wal::replay_streaming(dir, &wal_config, |seq, op| {
         if seq > checkpoint.flushed_through_seq { apply_wal_op(&mut active, seq, op) }
         // else: already durably represented by the Manifest-authoritative
         // SSTable set: discarded, per LSM Engine Spec Sec7.1 step 5.
     })
       [SHARED lock, read-only, unchanged from Phase 4A -- no signature
        change to wal::replay_streaming itself: the discard-by-checkpoint
        filter lives entirely in this call's own closure, preserving
        "purely additive" for the WAL module and its own bounded-memory
        guarantee (every record is still visited exactly once, streamed,
        never materialized -- only whether it's *applied* changes]

  3. FileWal::open_for_recovery(dir, wal_config)             [EXCLUSIVE lock, unchanged]
  4. GroupCommitter::new(file_wal)                            [unchanged]
  5. BatchCoordinatorPool::new(pool_config)                   [unchanged]

  6. Now holding the exclusive lock -- the write-capable part of Manifest
     recovery:
     a. sstable directory sweep (RUBIC_SSTABLE_FORMAT_SPECIFICATION.md
        Sec3.3 / LSM Engine Spec Sec7.2), reconciled against
        ManifestState:
          - delete every `*.sst.tmp` unconditionally (unchanged from
            Phase 4B)
          - for every `*.sst` file NOT in `live_sstables`:
              - if its id IS in `ever_added` (it was added then removed
                -- impossible this phase, since there is no compaction
                to ever issue a REMOVE_SSTABLE, but handled per spec
                for forward-compatibility with the future Compaction
                phase that will issue them): delete the orphaned file
              - if its id is NOT in `ever_added` at all (the crash-
                between-fsync/rename-and-manifest-append case, RUBIC_
                SSTABLE_FORMAT_SPECIFICATION.md Sec3.3): validate its
                footer (Sec6 of that spec); if valid, durably append
                ADD_SSTABLE for it now (manifest::append_sync) and add
                it to the live set; if invalid, `open()` fails closed
                exactly as Sec3.3/ADR-P4B-2 already established for a
                corrupt SSTable
     b. open() every SSTable in the resulting live set -- a footer/
        index checksum failure on any of them fails `open()` closed
        (LSM Engine Spec Sec7.1 step 3 / Sec7.4)
     c. build `sstables: Arc<RwLock<Vec<Arc<SsTable>>>>` from exactly
        this validated, Manifest-authoritative set (newest-first by id)
     d. open the Manifest for appending (positioned at end-of-file,
        including whatever step 6a just appended), ready for the flush
        thread
     e. store `checkpoint.flushed_through_seq` (or 0) as the current
        checkpoint, shared with the flush thread (Arc<AtomicU64> or
        equivalent) so a future SET_CHECKPOINT's monotonicity can be
        enforced at the write side too (Section 5, belt-and-suspenders)

  7. immutables starts empty (LSM Engine Spec Sec7.3's own reasoning:
     unchanged, still holds exactly as in Phase 4A/4B)

  8. spawn the background flush thread, now also given: the Manifest
     handle, the current checkpoint, and (unchanged) sstables_dir /
     next_sstable_id / writer config

  9. return LsmEngine
```

No change to `wal::replay_streaming`'s signature or its own internal
bounded-memory design; no change to the `replay_streaming`-before-
`open_for_recovery` ordering; the only new lock acquisition is one
additional, self-contained shared-lock read pass (step 1) that
completes and releases before step 2 begins.

## 5. Flush -> publish -> checkpoint -> purge: the full sequence

```
background flush thread, per frozen memtable:

  1. id = next_sstable_id.fetch_add(1)
  2. sstable::write_from_memtable(&frozen, id, sstables_dir, writer_config)
       -- temp file -> fsync -> rename -> directory fsync (UNCHANGED, Sec2)
  3. manifest.append_sync(ManifestEdit::AddSstable { id, min_seq, max_seq, file_size })
       -- durable: append + fsync, before anything below
  4. sstables.write().insert(0, Arc::new(SsTable::open(...)?))
       -- now readable (Sec6's read-path change)
  5. pool.rotate()?                                   // WAL Spec Sec2.2's own note
  6. position = pool.submit(WalOpOwned::CheckpointMarker{flushed_through_seq: frozen.max_seq})?.wait()?
       -- durable, via the SAME leader/follower group-commit path as any write
  7. manifest.append_sync(ManifestEdit::SetCheckpoint {
         flushed_through_seq: frozen.max_seq,
         wal_segment_id: position.segment_id,
         wal_offset: position.offset,
     })
       -- durable: the checkpoint itself now exists
  8. checkpoint.store(frozen.max_seq)                 // shared observability/monotonicity value
  9. immutables.write().retain(|m| !Arc::ptr_eq(m, &frozen))
       -- only NOW is the ImmutableMemTable's in-memory copy released
 10. pool.purge_before(frozen.max_seq)?
       -- only NOW, with steps 2-7 all durable, is it safe
```

Every step above that can fail (2, 3, 6, 7, 10) is handled by
`PHASE5_FAILURE_MODEL.md`'s failure table; none of steps 8-10 ever runs
unless every step before it succeeded, and a failure at any step
before 8 leaves the `ImmutableMemTable` retained and the flush retried
(Section 8) — never dropped, never "partially checkpointed."

## 6. Read path: Manifest-authoritative liveness

`LsmEngine::get_as_of` (unchanged logic) now consults `sstables`
populated exclusively from the process above — never "every `.sst`
file found in `sstables/`." An `.sst` file physically present but not
in the Manifest's live set (an orphan the directory sweep, Section 4
step 6a, could not resolve — should not occur under this phase's own
write path, since there is no compaction to ever detach a live file
without also recording `REMOVE_SSTABLE`, but the *rule* is stated for
when Compaction does exist) is **never served** — this is the direct
fix for "never silently serve stale data from an SSTable the Manifest
says is no longer live." No such file is deleted automatically this
phase either (no `REMOVE_SSTABLE` edit exists yet to authorize
deletion, since Compaction is out of scope) — an orphan of this kind
this phase can only mean the never-added case (Section 4 step 6a),
which is always resolved by re-adding it, never by deleting it.

## 7. Manifest inspection / observability

See `PHASE5_ARCHITECTURE.md` §7 for the inspection tool and metrics
surface — reads the already-shared in-memory `ManifestState`/
`sstables`, never the file directly, on any production code path.

## 8. Flush-thread supervision — audited, not casually changed

See `PHASE5_FAILURE_MODEL.md` §6 and `PHASE5_ADR.md` for the full
audit and decision: the bounded-retry-then-indefinite-retry behavior
(`PHASE4B_ARCHITECTURE.md` §8) is **kept**, with one addition
(Section 9) — a flush failure now provably can never advance the
checkpoint or trigger a purge (Section 5's ordering already guarantees
this structurally: steps 6-10 are unreachable unless steps 2-3
succeeded). The flush-thread-panic gap is evaluated and a decision
recorded, not silently carried forward unexamined.

## 9. Idempotent flush retries

A retried flush attempt (Section 8) uses a **new** `sstable_id`
(`next_sstable_id.fetch_add(1)` is called once per *attempt*, not once
per logical flush — `PHASE4B_ARCHITECTURE.md`'s own existing design)
— so a duplicate SSTable file from a retry can never collide with an
earlier failed attempt's own leftover `.tmp` at the same id. The
Manifest's own idempotence rule (format spec Section 7) means even if
somehow two `ADD_SSTABLE` edits for logically-the-same-data existed
under two different ids (impossible via this phase's own single-
attempt-succeeds design, since a failed attempt never reaches step 3
of Section 5's sequence), both would simply become live — a redundant
but never *incorrect* outcome, matching the LSM spec's own explicit
tolerance for this class of redundancy (§3.3, §5.2's compaction
discussion of the identical principle).
