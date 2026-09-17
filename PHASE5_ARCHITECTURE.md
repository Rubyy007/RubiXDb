# RubiXDB Phase 5 — Architecture

This document is the top-level Phase 5 architecture record required by
the operating brief; the detailed Manifest design (ownership
boundaries, format derivation, startup-ordering proof, and the full
flush/checkpoint/purge crash-state-machine) lives in
`PHASE5_MANIFEST_ARCHITECTURE.md` and is referenced, not duplicated,
here.

## 0. Precondition status

See `PHASE5_ADR.md` ADR-P5-0/ADR-P5-1 for the full release-gate audit:
Phase 3C's long soak is genuinely relaunched (uninterrupted this time)
as the final action of this phase, status **IN PROGRESS** at the time
this document set is finalized, not fabricated into a PASS; the
Phase 4B 100-writer performance anomaly is conclusively narrowed (one
specific hypothesis ruled out by a dedicated ablation) but not fully
explained, recorded as an accepted open observation.

## 1. Scope

```text
Before:  ... -> MemTable -> Immutable MemTable -> RUBIC SSTable
After:   ... -> MemTable -> Immutable MemTable -> RUBIC SSTable -> Manifest -> Safe WAL Checkpoint/Purge
```

Phase 5 implements the Manifest exactly as already specified — "Status:
Final" — `RubixDB-LSM-Engine-Specification-v1.0.md` §3, §6, §7 — and
closes the exact gap `PHASE4B_ADR.md` ADR-P4B-1 deliberately left open:
SSTable liveness and WAL retention are now governed by a real,
crash-safe Manifest, not a footer-derived heuristic. Compaction,
replication, and multi-node operation remain explicitly out of scope
(operating brief).

## 2. What is frozen from Phase 4B, unchanged

Per `PHASE5_MANIFEST_ARCHITECTURE.md` §2: the SSTable writer's atomic
publication sequence, the plain-`File`-I/O bounded-memory reader
design (separately re-confirmed with the user this phase — the
previously-unanswered "mmap vs. plain file I/O" question from
`ARCHITECTURE.md`, resolved during Phase 4B, is not reopened), the
RUBIC SSTable byte format, and the WAL binary format are all
byte-for-byte, step-for-step unchanged. 140/140 (Phase 4B) plus
180/180 (this phase, re-verifying the same mechanism under Manifest
integration) real external-process crash cycles gave no reason to
touch any of it.

## 3. Ownership boundaries

See `PHASE5_MANIFEST_ARCHITECTURE.md` §1's table. Summary: **the WAL
owns durability of individual writes and sequence assignment; the
SSTable owns immutable persisted sorted data; the Manifest owns the
authoritative metadata describing which SSTables are live and which
sequence has been checkpointed.** `LsmEngine` orchestrates; it invents
no new authority of its own.

## 4. Startup ordering

See `PHASE5_MANIFEST_ARCHITECTURE.md` §4 for the full two-phase
Manifest-recovery derivation (`manifest::replay_readonly` under a
shared lock, before the exclusive lock; `Manifest::open_after_
exclusive_lock` plus the SSTable-directory reconciliation sweep, after
it) and why it preserves `PHASE4A_ADR.md` ADR-P4A-3's existing
lock-ordering constraint without any change to `wal::replay_streaming`.

## 5. Flush -> publish -> checkpoint -> purge

See `PHASE5_MANIFEST_ARCHITECTURE.md` §5 for the exact ten-step
sequence and its idempotent-retry design (`PHASE5_ADR.md` ADR-P5-4 has
the account of a real bug found and fixed in that design by this
phase's own crash testing).

## 6. Read path

`sstables` is now populated exclusively from the Manifest-authoritative
live set (`PHASE5_MANIFEST_ARCHITECTURE.md` §6) — never from "every
`.sst` file found in the directory," Phase 4B's own now-retired
approach. A missing live table or an unrecognized-but-invalid file
fails `LsmEngine::open` closed; nothing is ever silently included or
silently excluded.

## 7. Observability added this phase

- `LsmEngine::checkpoint_seq() -> u64` — current durable checkpoint.
- `LsmEngine::recovery_stats() -> RecoveryStats` — WAL records visited/
  applied/skipped-by-checkpoint, checkpoint markers replayed, Manifest
  edits replayed, recovery wall-clock duration (`PHASE5_ADR.md`
  ADR-P5-6 — added because the crash test actually needed exact
  accounting, not as a checklist exercise).
- `LsmEngine::manifest_record_count()`/`manifest_size_bytes()`/
  `manifest_last_edit()` — the Manifest inspection surface (operating
  brief: "current checkpoint, live SSTables, Manifest record count,
  Manifest size, last valid record... without mutating storage"). All
  three read the already-open, already-locked `Manifest` handle; none
  perform I/O beyond what the handle already has cached (`size_bytes()`
  is the one exception — a single `fstat`-class metadata call, not a
  read of file content).
- `LsmEngine::live_sstable_ids() -> Vec<u64>` — the current live set,
  newest-first.

No new metric here is on the hot write path — `put`/`delete`/`get`
touch none of these accessors; they exist purely for an operator or
test harness to call independently.

## 8. Flush-thread panic handling

See `PHASE5_ADR.md` ADR-P5-5 for the full decision: each flush attempt
runs inside `catch_unwind`, treated identically to an I/O failure by
the same idempotent-retry state machine (ADR-P5-4) — not a supervised-
restart thread design, which the operating brief itself flagged as
risky if done carelessly.

## 9. Logging

Flush failures (I/O or caught panic) are logged via `eprintln!` with
the attempt number and current idempotence state
(`published`/`checkpoint_marker`/`checkpoint_recorded` presence, not
their content) — no key or payload bytes are ever included, consistent
with this project's standing security rule (`ARCHITECTURE.md`'s error-
type doc comment: "payload contents are never logged"). This project
has no structured logging framework as of this phase (none was
introduced in any prior phase either) — `eprintln!` remains the
project-wide convention; introducing one is judged out of scope for
this phase's own goals and not attempted speculatively.

## 10. Metrics

Every new observability accessor (§7) is a plain, lock-guarded read of
already-maintained in-memory state (`AtomicU64` loads, a `Mutex<
Manifest>` lock held only long enough to read a `u64`/small struct) —
no new counter introduces contention beyond what already existed for
`sstable_count()`/`immutable_count()` in Phase 4B, which were never
found to be a bottleneck (`PHASE5_PERFORMANCE.md`'s own measurements
confirm the write hot path is unaffected).
