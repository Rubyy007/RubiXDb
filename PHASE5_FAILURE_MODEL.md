# RubiXDB Phase 5 — Failure Model

Companion to `PHASE1_FAILURE_MODEL.md` through `PHASE4B_FAILURE_
MODEL.md` — every existing failure mode still applies unchanged. This
file covers what Phase 5 adds: the Manifest, checkpoint, WAL-purge,
SSTable-liveness, flush, recovery, and shutdown failure modes. Every
row states detection, response, caller-visible outcome, and whether
acknowledged durable data can be lost.

## 1. Manifest crash-state machine

The complete state machine the operating brief required, with what
survives a crash at each point and what restart recovery does. Every
row's answer to "data loss possible?" is **No** unless stated
otherwise — this is the single most important property of this whole
table, and it holds because of one structural fact: nothing in this
list ever runs until the WAL already durably holds the record in
question (`PHASE4A_ARCHITECTURE.md` §5's ordering, unchanged).

| # | Crash point | What survives | Recovery behavior | Data loss possible? |
|---|---|---|---|---|
| 1 | Before SSTable publication (temp file write in progress) | The frozen memtable's data, fully in the WAL | `.sst.tmp` swept unconditionally; WAL replay (unbounded by any checkpoint for this data, since none was ever recorded) reconstructs it into the fresh active MemTable | No |
| 2 | After SSTable publication (durable `.sst`), before Manifest `ADD_SSTABLE` append | The file, durable and valid; the WAL, untouched | Directory reconciliation sweep (`PHASE5_MANIFEST_ARCHITECTURE.md` §4 step 6a) finds it un-acknowledged, validates it, durably re-appends `ADD_SSTABLE` now; data also still fully replayable from WAL (checkpoint was never recorded for it) — briefly redundant, never wrong | No |
| 3 | Before Manifest append (append call not yet started) | Same as row 2 | Same as row 2 | No |
| 4 | During Manifest append (partial frame write) | A torn trailing frame — `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §4.2 | Torn tail truncated and discarded on next open (expected, not alarming); the edit is simply as if it never happened — sweep (row 2) re-adds the SSTable if warranted | No |
| 5 | After Manifest append, before its `fsync` | Frame bytes may or may not have reached stable storage — platform-dependent | If not durable: identical to row 4 (torn tail). If durable despite no explicit confirmation: replays normally next open | No |
| 6 | After Manifest `fsync` (`ADD_SSTABLE` fully durable) | The edit, durably | Replayed normally; SSTable is now live | No |
| 7 | Before checkpoint publication (SSTable live, no `CHECKPOINT_MARKER`/`SET_CHECKPOINT` yet) | The SSTable (live); the WAL (untouched, not purged) | WAL replay still covers this data (no checkpoint ever advanced past it); SSTable also serves reads — redundant, correct either way | No |
| 8 | `CHECKPOINT_MARKER` durably written to WAL, `SET_CHECKPOINT` not yet appended to Manifest | The marker (a real, durable WAL record, consuming a real sequence number); the Manifest's checkpoint is unchanged | Replay treats the marker as a no-op (it is not a Manifest checkpoint, just an inert record); it becomes a permanent, harmless "orphaned marker" until a *later* successful flush's checkpoint numerically subsumes it (`PHASE5_ADR.md` ADR-P5-4 — verified directly by 180 real crash cycles, `RecoveryStats::checkpoint_markers_replayed`) | No |
| 9 | `SET_CHECKPOINT` durably appended to Manifest | The checkpoint, durably | WAL replay now correctly discards everything at or below it (LSM Engine Spec §7.1 step 5) | No |
| 10 | After checkpoint publication, before WAL purge | The checkpoint (durable) and the not-yet-purged WAL segments (harmless redundancy) | WAL replay discards covered records via the checkpoint regardless of whether purge ran; a subsequent `purge_before` call (next flush, or none if none comes) eventually reclaims the space | No |
| 11 | During WAL purge (some covered segments deleted, not all) | The checkpoint; whichever segments were already deleted; whichever weren't | Replay is unaffected either way — purge only ever removes segments *entirely* below a durably-recorded checkpoint, per the existing, unchanged, already-tested `purge_before` contract (WAL Spec §10) | No |
| 12 | After WAL purge completes | The checkpoint; the reduced WAL; the SSTable set that fully covers the purged range by construction | Normal — this is the steady state the whole mechanism exists to reach | No |

**The one invariant every row reduces to**: a sequence is only ever
removed from the WAL's own recoverable footprint after (a) its data is
durably represented in a validated, Manifest-live SSTable, and (b) that
coverage is itself durably recorded via `SET_CHECKPOINT` — never
before. Rows 1-9 show every intermediate state is safe specifically
*because* purge (row 10-12) cannot yet have run.

## 2. Manifest corruption / discovery failure table

| Failure | Detection | Response | Caller-visible outcome | Data loss possible? |
|---|---|---|---|---|
| Non-tail corrupted Manifest frame | Checksum mismatch not at the trailing position (`RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §4) | `LsmEngine::open` returns `Err(EngineError::Corruption)` | Engine does not start | No (WAL still has everything not yet checkpointed; only the SSTable read-path and checkpoint state are ever at risk from Manifest damage, and refusing to start prevents any wrong answer) |
| Unsupported `format_version` in a Manifest edit | N/A this phase — no per-edit version field exists in the format the LSM spec defines; a byte outside `{1,2,3}` is a corrupt/unknown `edit_type`, not a version mismatch | `Err(EngineError::Corruption)` naming the offset and byte | Engine does not start | No |
| Impossible checkpoint regression persisted in the file | `ManifestState::apply` compares against the running max | `Err(EngineError::Corruption)` | Engine does not start | No |
| `REMOVE_SSTABLE` for an id never `ADD`ed | `ManifestState::apply` checks `ever_added` | `Err(EngineError::Corruption)` | Engine does not start | No |
| Duplicate `ADD_SSTABLE`/`REMOVE_SSTABLE` for the same id | Idempotence rule (`RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §7) | No-op, not an error | Engine starts normally | No |
| Manifest says an id is live, file missing from disk | Post-scan check in `reconcile_sstables_with_manifest` | `Err(EngineError::Corruption)` naming the missing path | Engine does not start | No (the WAL, untouched, still has every record — this can only ever cost *SSTable read-path availability*, never data; operator remedy: investigate the missing file, or if truly gone and its data is confirmed still fully covered by the WAL, an operator-driven recovery path outside this phase's scope) |
| Extra file at a never-`ADD`ed id, contents invalid | Footer/index validation in `reconcile_sstables_with_manifest` | `Err(EngineError::Corruption)` naming the path | Engine does not start | No |
| Extra file at a never-`ADD`ed id, contents valid | Same validation, passes | Durably `ADD_SSTABLE`'d now, joins the live set (LSM Engine Spec §7.2's own rule) | Engine starts, file becomes live | No — this is the intended, spec-mandated recovery path for row 2/3 of §1 above |
| Multiple Manifest files | Not applicable — `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §9: exactly one file, no rotation, no "latest wins" heuristic exists or is needed | N/A | N/A | N/A |

## 3. Flush pipeline failure table

| Failure | Detection | Response | Caller-visible outcome | Data loss possible? |
|---|---|---|---|---|
| `write_from_memtable` I/O failure | `Result::Err` from the writer | Retried (bounded backoff, then indefinite) from scratch (SSTable not yet published) | None to the original `put`/`delete` caller (already durable, already acknowledged) | No |
| `ADD_SSTABLE` Manifest append failure | `Result::Err` | Same retry | Same | No |
| `pool.rotate()` failure | `Result::Err` | Same retry; SSTable/`ADD_SSTABLE` already durable, not rebuilt (idempotent, ADR-P5-4) | Same | No |
| `CHECKPOINT_MARKER` submit/wait failure | `Result::Err` | Same retry; not resubmitted once it has already durably succeeded (ADR-P5-4) | Same | No |
| `SET_CHECKPOINT` Manifest append failure | `Result::Err` | Same retry; not re-appended once already durably succeeded (ADR-P5-4) | Same | No |
| `purge_before` failure | `Result::Err` | Same retry; checkpoint is already durable regardless, so a retry's redundant `purge_before` call is harmless if the earlier one partially succeeded | Same | No |
| Flush attempt panics | Caught by `catch_unwind` (ADR-P5-5) | Treated as the identical failure class as an I/O error — logged, retried with the same idempotent guards | Same | No |
| Flush thread's own I/O to the *stop flag*/*delay* atomics | Not a failure mode — these are plain `Ordering::Acquire`/`Release` loads/stores on `AtomicBool`/`AtomicU64`, cannot themselves fail | N/A | N/A | N/A |
| A flush failure of any kind | N/A (summary row) | **Never advances `checkpoint_seq`, never calls `purge_before` with that flush's watermark** — structurally unreachable unless every prior step already durably succeeded (`PHASE5_MANIFEST_ARCHITECTURE.md` §5) | The `ImmutableMemTable` is retained, never dropped, until its own flush fully succeeds | No |

## 4. Recovery failure table

| Failure | Detection | Response | Caller-visible outcome | Data loss possible? |
|---|---|---|---|---|
| Corrupted (non-tail) Manifest | §2 above | `open()` fails closed | Engine does not start | No |
| Corrupted live-set SSTable footer/index | Unchanged from Phase 4B (`PHASE4B_ADR.md` ADR-P4B-2) | `open()` fails closed | Engine does not start | No |
| WAL replay's own defensive invariant violated (LSM Engine Spec §7.4: first post-checkpoint record's `seq` should be `checkpoint + 1`) | **Not implemented as an explicit assertion this phase** — an accepted, named scope gap; see §5 below | N/A | N/A | Would indicate an impossible prior-invariant violation elsewhere in the system if it ever fired; not expected to be reachable given every other invariant in this table holds |
| Corrupted WAL segment (non-tail) | Unchanged, pre-existing WAL behavior (`PHASE3C_FAILURE_MODEL.md`) | `open()` fails closed | Engine does not start | No |

## 5. Explicitly named, not silently skipped, gaps this phase

- **The LSM Engine Spec §7.4 defensive invariant** ("if WAL replay is
  non-empty, its first record's `seq` should equal `checkpoint.
  flushed_through_seq + 1`") is not implemented as an explicit runtime
  assertion in `LsmEngine::open`. The 180 real crash cycles run this
  phase never observed a violation (`RecoveryStats`'s exact accounting
  would have caught one indirectly, since a gap would show up as a
  discrepancy in the same formula that caught ADR-P5-4's bug), but this
  is not the same as a dedicated, always-on defensive check. Recorded
  as an open item for the Manifest/Compaction follow-on phase.
- **A dedicated fault-injection test for the flush-thread panic path**
  (ADR-P5-5) was not built this phase — confidence rests on code review
  and the shared idempotence machinery already proven by real crashes,
  not on a literal `panic!()` injected mid-flush. Named explicitly in
  `PHASE5_ADR.md` ADR-P5-5, not hidden.
- **Manifest file growth is unbounded** (`RubixDB-LSM-Engine-
  Specification-v1.0.md` §6.3's own explicit v1 non-goal, carried
  forward unchanged, not newly introduced by this phase) — a future
  Compaction/Manifest-compaction phase's responsibility.
