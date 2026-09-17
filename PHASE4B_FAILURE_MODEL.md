# RubiXDB Phase 4B — Failure Model

Companion to `PHASE1_FAILURE_MODEL.md` through `PHASE4A_FAILURE_MODEL.md`
— every existing failure mode still applies unchanged. This file covers
what Phase 4B adds: the MemTable/SSTable boundary's own failure
semantics, under the no-Manifest, no-WAL-purge design
(`PHASE4B_ADR.md` ADR-P4B-1).

## 1. Write-path failure table

| Failure | Detection | Response | Caller-visible outcome |
|---|---|---|---|
| SSTable allocation failure (block/bloom/index build) | Not caught specially — process-fatal OOM abort, matching MemTable's identical treatment (`PHASE4A_FAILURE_MODEL.md` row 3) | N/A | N/A |
| `.sst.tmp` write/short-write failure | `io::Error` from the writer's `write_all`/`write_all_at` | Flush attempt fails, entry retained in `immutables`, bounded retry (`PHASE4B_ARCHITECTURE.md` §8) | None to the original `put`/`delete` caller (already returned, already durable) — only a delayed, internally-retried flush |
| `.sst.tmp` fsync failure | `io::Error` from `sync_all()` | Same as above | Same as above |
| Rename failure (`.sst.tmp` -> `.sst`) | `io::Error` from `std::fs::rename` | Same as above; `.sst.tmp` left in place, swept on next `open()` if the process restarts before a retry succeeds | Same as above |
| Directory fsync failure | `io::Error` from `wal::fsync_dir` (Unix only — Windows never errors, being a no-op) | Same as above — treated as a flush failure, not silently ignored, even though the underlying rename may already be durable | Same as above |
| Post-build self-validation (`SsTable::open` on the file just written) fails | The full reader validation path (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §6) runs unconditionally, even on a file this process just wrote | Treated as a flush failure (Section 6 of the architecture doc) | Same as above |
| Flush thread panics | Not specially caught this phase — matches this project's existing "a poisoned lock/panicked thread is a fail-closed event, not silently recovered" posture (`PHASE1_FAILURE_MODEL.md`'s fail-closed poison policy, applied here by the same reasoning) | The flush thread's death is detectable (its `JoinHandle` becomes joinable/panicked) but not auto-restarted this phase — an explicit, named gap, not a silent one | Writes continue to succeed (durability is WAL-backed, unchanged); flushing simply stops until the process is restarted, which re-derives `immutables` as empty (Section 3) and resumes normally |

## 2. Read-path failure table

| Failure | Detection | Response | Caller-visible outcome |
|---|---|---|---|
| Corrupt `.sst` discovered at `open()` | Footer/index/bloom validation (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §6) | `LsmEngine::open` returns `Err(EngineError::Corruption{detail})` naming the exact path — fails closed, per `PHASE4B_ADR.md` ADR-P4B-2 | Engine does not start; operator moves the file aside (no data loss — Section 3) and retries |
| Corrupt data block discovered during a read (post-`open()`) | Per-block CRC32C check, lazy (blocks are read on demand) | `get_versioned`/iteration for that specific call returns `Err(EngineError::Corruption{..})` | That one read fails; the engine keeps running (other blocks/sources are unaffected); the same key is still correctly answerable from `active`/`immutables` if a version still lives there, or (always) via the WAL if the engine is restarted |
| Bloom filter false positive | N/A — expected, not a failure | Falls through to the real block read (never trusted as a negative answer) | Correct result, one extra (wasted) block read |
| Unsupported `format_version` on read | Checked before any other field is trusted | `Err(EngineError::Unsupported{..})`, same fail-closed treatment as any other corruption for open-time discovery | Same as "corrupt `.sst`" row above |
| Oversized `key_len`/`value_len` read from disk | Checked against `max_key_size`/`max_value_size` **before** allocation (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.9) | `Err(EngineError::CapacityExceeded{..})` — never an allocation attempt sized by an unvalidated field | Same as "corrupt data block" row — that read fails, engine keeps running |

## 3. The one genuinely important trade-off, named explicitly

**Flush never bounds WAL growth or restart replay cost this phase**
(`PHASE4B_ADR.md` ADR-P4B-1, `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`
§3.6). This is not an oversight — it is the direct, deliberate
consequence of the user's explicit choice to avoid any Manifest-
shaped checkpoint mechanism this phase. Practical effect: a
long-running instance that flushes many times will have a WAL that
keeps growing (exactly as it already did in Phase 4A, which had no
flush path at all) and a restart that keeps replaying the *entire* WAL
history into a fresh MemTable regardless of how many SSTables exist.
This is explicitly a **temporary, phase-scoped limitation**, closed by
the future Manifest phase, not by anything in Phase 4B.

## 4. Crash boundaries (extends `PHASE4A_FAILURE_MODEL.md` §3)

| Crash point | What survives | Why |
|---|---|---|
| Every row already in `PHASE4A_FAILURE_MODEL.md` §3 | Unchanged | Phase 4B does not alter the WAL/MemTable boundary at all |
| During flush build (any point before rename) | The frozen memtable's data — fully in the WAL, replayed fresh on restart; the `.sst.tmp` itself is swept, never trusted | `PHASE4B_ARCHITECTURE.md` §7 |
| During/after rename, before directory fsync | Same — data is WAL-backed regardless of this file's fate | Same |
| After full publication | The `.sst` file (validated on next open) **and** the WAL's own copy of the same records (never purged) — genuinely redundant, by design | ADR-P4B-1 |

**The single invariant every row reduces to, extended from Phase 4A**:
*after restart, the recovered MemTable reflects exactly the durable
WAL state, and the recovered `sstables` list reflects exactly the set
of `.sst` files that fully validate — the two are allowed to overlap
in content (redundant), and that redundancy is the explicit price of
not yet having a Manifest.*

## 5. What Phase 4B does NOT change

- Every existing WAL/`GroupCommitter`/`BatchCoordinatorPool`/MemTable
  failure mode (`PHASE1_FAILURE_MODEL.md` through
  `PHASE4A_FAILURE_MODEL.md`) — unchanged, re-verified passing.
- `wal::replay_streaming`'s behavior, or any WAL recovery contract.
- The MemTable-is-not-a-durability-system principle — SSTable is not
  one either; it is a second, purely-derived, purely-additive
  representation of already-WAL-durable data, never a first-class
  durability decision point of its own this phase.
