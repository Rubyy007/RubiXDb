# ADR-COMPACTION-LEAK-01 — A failed compaction must clean up after itself, say so, and stop retrying what can never succeed

**Status: ACCEPTED (2026-10-08) — implemented; see "Implementation notes" at the end.** The text below is the approved proposal and is unchanged. When it was written no engine, protected-path, test, `Cargo.toml` or `Cargo.lock` change had been made. Every change it proposes is under a protected path (`src/sstable/writer.rs`, `src/lsm/mod.rs`, possibly `src/compaction/mod.rs`) and therefore needs an explicit engine-change authorisation (CLAUDE.md, Protected Engine Boundary). Evidence: `PHASE_ITEM_F08_COMPACTION_LEAK_DISCOVERY.md` (2026-10-08, tree `13dc534`). Related: ADR-WE-SP-001 (`PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`), ADR-COMPACTION-001 (`PHASE_COMPACTION_ADR.md`), ADR-SST-01 (`PHASE_ITEM_F08_ADR.md`, PROPOSED).

## Decision

Make the SSTable writer remove its own temporary file on every failure path; record every compaction failure (kind, count, last error) in `CompactionMetrics` and show it, with a derived state, on the existing compaction surfaces and in the security log; and classify failures by structured error variant so that a worker that has failed `max_flush_retries` (3) consecutive times with a permanent-class error (`Corruption`, `Unsupported`, or a panic) stops attempting until the process restarts, leaving `StorageState` and ADR-WE-SP-001's model untouched.

## Reason (measured, `PHASE_ITEM_F08_COMPACTION_LEAK_DISCOVERY.md`)

* **Cadence and size** (case A, a three-table fixture, one flipped bit in a middle data block of table 1): 25 failed attempts at **5.048 s mean spacing (5.022-5.066 s)** from file-system timestamps; each leaves a `.sst.tmp` of **exactly 1,991,320 bytes**; 1 / 6 / 12 / 24 files at t = 0 / 30 / 60 / 120 s, 49,783,000 bytes at 125 s; ids 5-29 consumed, none published (`discovery` 4.2 a, b).
* **Nothing reports it** (4.2 c-e): `cycles_completed: 0`, every total 0, `last_cycle: null` (`/v1/compaction/metrics`); `storage_state: Healthy`, `ready: true`; stderr one line per attempt (`compaction: failed, will retry on the next trigger: corruption: block: checksum mismatch`, naming no table); `security.log` unchanged. The only moving number is `disk.sstable_bytes` (18.1 -> 65.9 MB with `live_sstable_count` constant at 4).
* **Not reclaimed by a clean shutdown** (25 files before and after); **swept by the next start** (0 at ready) **and the leak resumes at the first 5 s tick**; with the automatic worker disabled the open sweeps and nothing regrows over 20 s (4.2 f, g).
* **The retry is expensive even if the files were deleted** (4.4, 5): case C (damage in the last block) wrote **11,939,728 bytes per attempt**, 2.3 MB/s sustained, on a 16 MB database; one attempt writes up to the data that precedes the damaged block in key order, i.e. up to the whole database every 5 s.
* **Continued writes make it worse** (4.3): an extra attempt after each flush (shortest gap 0.281 s), the table count grows 4 -> 9 in 125 s because compaction never succeeds.
* **The code has no cleanup anywhere** (`writer.rs` has no `remove_file`; the early returns at :130, :135, :151-:196), **no classification** (one arm, `lsm/mod.rs:2949-2952`), **no failure metric** (`:614-619`), and the sweep at open (`:2482-2485`) is the only remover. The promise it contradicts: "corrupt input (fail closed, **no partial output**)", `PHASE_COMPACTION_ADR.md:752-754`.
* **The same writer is used by the flush path**, which takes a new id per attempt (`lsm/mod.rs:3040`): by code reading it leaks under ADR-WE-SP-001's ENOSPC retries too (not reproduced).

## Alternatives considered (P1-P4; failure mode of each)

| | Rule | Failure mode it leaves open |
|---|---|---|
| **P1** | writer removes its tmp file on every failure path | the silent retry stays: 394 KB/s (case A) to 2.3 MB/s (case C) of disk writes, scaling with the database, and a growing table count under writes; the one visible symptom (`disk.sstable_bytes`) disappears too |
| **P2** | P1 + failure fields in `CompactionMetrics` shown on the existing endpoints | visible, but the loop still never ends: permanent and transient failures look alike, the I/O amplification continues |
| **P3 (this ADR)** | P2 + structured classification; after the existing fast-retry budget of consecutive permanent-class failures the worker stops until restart; separate compaction state | compaction is off until restart, so the table count (read amplification) grows under writes and tombstones are retained; the damaged rows are still lost (ADR-SST-01 reports them); a transient error mis-seen as permanent would wrongly stop compaction |
| **P4** | (i) cleanup only in `compact_once_impl`; or (ii) one reused fixed tmp name | (i) leaves the flush-path leak (same writer); (ii) bounds the pile to one file but changes naming, reports nothing and still wastes the writes |

## Design

**1. Writer cleanup** (`src/sstable/writer.rs`). `write_from_sorted_records` creates a small guard for its own tmp path immediately after the successful open at :115, declared **before** the file handle so the handle is closed first on Windows; the guard's `Drop` removes that path unless it has been disarmed after the successful `fs::rename` (:195). Removal is best-effort (an error is ignored; the sweep at open stays as the backstop for a kill or a failed removal). It covers every early return, including a panic unwinding through the writer. It removes only the file this call created.

**2. Failure record and classification** (`src/lsm/mod.rs`, `CompactionMetricCounters`). In the worker's failure arm (`:2949`) and the panic arm (`:2953`):

| Outcome | Class | Counts toward the permanent streak |
|---|---|---|
| `EngineError::Corruption`, `EngineError::Unsupported` | permanent-candidate | yes |
| a panic | permanent-candidate | yes |
| `EngineError::Io` (including ENOSPC, `is_storage_exhausted()`), anything else | transient | no — and it resets the streak |
| success | — | resets the streak |

Classification is by variant, never by parsing text (the ADR-WE-SP-001 section 7 rule). Every failure updates `failures_total`, `consecutive_failures`, `last_failure { at_unix_ms, kind, message }` (`message` = the `EngineError` display; no key, value or row bytes).

**3. Blocked.** When the permanent streak reaches `LsmConfig::max_flush_retries` (3, the existing fast-retry budget), the worker records `blocked = true` and stops calling `compact_once_impl` (it still drains `MaybeCompact`, honours shutdown, and does no I/O). `blocked` is in memory only: a restart re-arms the worker, which then makes at most that many further attempts, each cleaned up. Transient failures keep today's behaviour exactly: retried at `storage_pressure_retry_interval` (5 s), now cleaned up and counted.

**4. State and reporting.** A derived `state`: `idle` | `running` | `failing` (`consecutive_failures > 0`, not blocked) | `blocked`. Additive fields on the four existing surfaces — `/v1/compaction/status`, `/v1/compaction/metrics`, `/v1/admin/status.compaction`, `/v1/metrics/system.compaction`: `state`, `failures_total`, `consecutive_failures`, `blocked`, `last_failure`. Security log (emitted by the api layer when it observes the transition, never by the engine): `compaction.failing` once when a streak starts and `compaction.blocked` once when blocked — never per retry (per-retry detail stays on stderr, which gains the attempt number and, at the block, an explicit line telling the operator to restore the damaged table and restart). `/readyz`, `ready`, `storage_state` and every existing field are unchanged.

## Correctness impact

* **Compaction results, the Manifest, the SSTable and WAL formats, durability, recovery and every read and write: unchanged.** The success path is byte-for-byte what it is today.
* **Changed:** (a) a failed attempt leaves no file behind (the contract ADR-COMPACTION-001 already promised); (b) a worker that has hit a permanent-class failure three times in a row stops instead of retrying forever; (c) failures are visible.
* **What a consumer observes with a permanently unreadable source table:** `cycles_completed` stays at its value; `failures_total` = 3, `consecutive_failures` = 3, `blocked: true`, `state: "blocked"`, `last_failure.kind: "corruption"` with the message; `storage_state: Healthy`, `ready: true`; reads and writes behave as before; no further growth of the `sstables` directory from compaction; the live table count grows with each flush until the table is replaced and the process restarted. Not-running compaction never changes what a read returns.

## Performance impact (predicted only; to be measured in the implementation mission)

* **Hot paths (append, read, flush success):** none; the guard is one local with a no-op `Drop` after the rename.
* **Compaction worker:** success path unchanged; a failure costs one `remove_file` (tens of microseconds); a blocked worker stops the retry traffic measured at 394 KB/s - 2.3 MB/s (scaling with the data) and the roughly 30-100 ms of CPU per 5 s.
* **Startup:** unchanged; the sweep at open remains.

## Failure semantics

* **Crash or kill mid-compaction, or between failure and cleanup:** unchanged — a kill runs no `Drop`, the partial file stays, and `reconcile_sstables_with_manifest` deletes it at the next open (`lsm/mod.rs:2482-2485`); the crash-cycle suites exercise exactly that and keep working.
* **Permanent versus transient:** the table above. A mixed history never blocks: a transient failure or a success resets the permanent streak.
* **Panic:** the unwinding runs the writer's guard (cleanup), is caught by the worker's `catch_unwind`, and counts as permanent-candidate (a deterministic panic will not heal).
* **Cleanup that fails:** ignored; the file is swept at the next open; nothing else depends on it.
* **ENOSPC during compaction:** transient — cleaned (which returns the partial output's space to the volume), counted, retried at the same cadence; ADR-WE-SP-001's flush-side machine is unchanged. The same writer fix closes the flush-path leak (by code reading).
* **Operator override: none.** The recovery path is the one in ADR-SST-01 and `rubixdb check`: restore a verified backup into a new instance; a restart alone re-arms the worker and it will block again.
* **Shutdown while failing or blocked:** unchanged (the worker honours the stop flag).

## Configuration surface

| Item | Value |
|---|---|
| New env var / default / bounds | **none.** The budget reuses `LsmConfig::max_flush_retries` (3) and the cadence `storage_pressure_retry_interval` (5 s), both `LsmConfig` fields the hosts leave at their defaults (`cli/src/host.rs`, `..LsmConfig::default()`); neither is exposed through the `RUBIXDB_LOCAL_*` family and this ADR does not add one. If the maintainer later wants the budget operator-visible it would join that family, but nothing measured argues for a different value. |
| New API fields | `state`, `failures_total`, `consecutive_failures`, `blocked`, `last_failure` (additive) |
| New messages | stderr: `compaction: failed (attempt k of 3), will retry: <error>`; `compaction: blocked after 3 consecutive permanent failures (<error>); restore the damaged table and restart` |
| Model | **a new, orthogonal compaction-health dimension** that reuses ADR-WE-SP-001's *shape* (structured classification, bounded budget, slower interval, explicit observable state) and its two numbers, **not** `StorageState`: that state gates writes, defers compaction when it is not `Healthy`, is `/readyz`'s `storage_state`, and maps any unknown value to `StorageFull` (`lsm/mod.rs:193-195`); a blocked compaction affects none of that. |

## Migration

* **No format change; no existing table or directory is affected.**
* **Existing tests:** none found that expects a tmp file to survive an in-process failure. The tmp assertions (`src/lsm/tests.rs:4267-4276`, `:5056-5057`, `:5321-5322`, `src/sstable/tests.rs:354-366`) are made only after a reopen (the sweep) or on a hand-written orphan and hold unchanged; `src/compaction/tests.rs:296-330` tests the merge, not the writer. Tests that inject `CompactionIoFaultHook` errors inject `Io` (transient) and keep their behaviour. A prediction from reading and searching, to be confirmed by running the suites.
* **The four crash-consistency suites** — `tests/crash_consistency.rs` and `tests/group_commit/crash_consistency.rs` (WAL), `examples/compaction_crash_cycle_test.rs`, `examples/sstable_flush_crash_test.rs` (and the related `lsm_crash_cycle_test`, `storage_pressure_crash_*`) — kill or abort the process, so no `Drop` runs and they still rely on the open-time sweep: **none is expected to fail**; the compaction and flush suites assert only that no `*.sst.tmp` survives recovery, which can only become easier to satisfy.
* **`CompactionMetrics`:** every existing field keeps its meaning (`cycles_completed` still counts successful cycles only); new fields are additive; one struct-literal construction site (`lsm/mod.rs:2280`); readers: `api/src/routes/compaction.rs`, `admin.rs`, `metrics_route.rs`, `api/src/observability/sampler.rs`, `cli/src/ops_cmd.rs`. `api/tests/api_integration.rs` asserts on compaction responses: to be checked for exact key-set comparisons.
* **Rollback:** revert the three changes; nothing persists.

## Relationship to ADR-WE-SP-001 and ADR-SST-01

* **ADR-WE-SP-001:** the second application of its *shape* (it did flush-side ENOSPC: classify by structured error, bounded fast retry, slower interval, explicit state, observability), to a different worker and a different failure class (permanent rather than capacity). It is **not** an extension of its states and does not weaken any of its guarantees; the writer fix additionally completes its section 11 ("a failed attempt must not create an apparently live SSTable") for the resource side it did not address (partial files).
* **ADR-SST-01 (F-08):** a **distinct** problem and independent. ADR-SST-01 is about finding a damaged table (at open, in the background) and reporting it; it changes nothing under the protected paths and does not touch the worker. This ADR is about what the worker does once damage has been hit. Neither needs the other; with both approved the operator gets two corroborating reports; with ADR-SST-01 alone the leak remains reachable through a damaged data block. It may be approved, authorised and implemented first, last or alone.

## Out of scope

Compacting around the damaged table (unsafe: dropping tombstones needs every table); repairing, quarantining or excluding a table; ENOSPC behaviour of compaction beyond cleanup and counting; the manual `compact_once` path (it benefits from the writer fix); any change to `StorageState`, `/readyz` or decision D5; the flush thread's retry policy; a periodic or shutdown-time tmp sweep; the severity of the `UNEXPECTED_FILE` info in `rubixdb check`; naming the failing input table in the compaction error (a possible small follow-up if the cursor exposes the table id); F-11, F-18 and everything else.

## Implementation notes (2026-10-08, appended; the ADR above is unchanged)

Implemented as approved (P3), by the commit that carries `PHASE_ITEM_F08_COMPACTION_LEAK_IMPLEMENTATION.md`. Evidence, before/after timeline, tests, mutation check and regression are in that document.

* Writer: `TmpFileGuard` in `src/sstable/writer.rs`, disarmed after the successful rename.
* Worker and state: `CompactionFailureKind`, `CompactionHealth` (`AtomicU8`), derived `CompactionState` in `src/lsm/mod.rs`; budget `max_flush_retries` (3) consecutive permanent-class failures; a success never lifts Blocked; the exit is a process restart. `StorageState` and `/readyz` untouched.
* Surfaces: additive fields on `/v1/compaction/status`, `/v1/compaction/metrics`, `/v1/admin/status.compaction`, `/v1/metrics/system.compaction`; security events `compaction.failing` and `compaction.blocked` on state change only.
* Measured, case A: before, 1/6/12/24 tmp files (1.99 MB each) at t = 0/30/60/120 s; after, 0 at every sample and the worker blocked at ~10.1 s (3 attempts).
* Not covered, as the ADR said: the error line still names no table (ADR-SST-01); no real-disk-full test; manual `compact_once` is not recorded by the worker arm.
* Deviation from the ADR text: none.
