# RubiXDB — Storage Pressure and ENOSPC Handling ADR

**ADR ID:** ADR-WE-SP-001

**Status:** **Accepted, implemented, and verified by the re-soak it was written to unblock** (implemented 2026-09-19, endurance re-soak passed 2026-09-20 — see "Implementation Notes" at the end of this document for the exact code landed, deliberate scoping decisions, and the final re-soak result)

**Date:** 2026-09-19

**Scope:** Write Engine only

**Related Components:** LsmEngine, flush thread, MemTable, RUBIC SSTable, Manifest, checkpoint, WAL purge, Dedicated Batch Coordinator

**Decision Type:** Write-path failure-handling and availability semantics

## 1. Decision Summary

RubiXDB will introduce an explicit **storage-pressure state machine** for persistent storage failures so that a full or exhausted filesystem does not cause the Write Engine to retry indefinitely, generate an uncontrolled failure storm, or continue accepting work without an explicit availability policy.

The state model will be:

**HEALTHY → STORAGE_PRESSURE → STORAGE_FULL**

The existing bounded fast-retry behavior will remain for genuinely transient I/O failures. **ENOSPC-class failures will be classified separately** from generic I/O failures. After the configured fast-retry budget is exhausted for ENOSPC, the flush subsystem will stop the current unbounded retry behavior and enter **STORAGE_PRESSURE**.

While in STORAGE_PRESSURE, the engine will retain all required in-memory durable state, will not advance the checkpoint incorrectly, will not purge WAL unsafely, and will apply controlled backpressure rather than continuing an uncontrolled retry loop.

STORAGE_FULL will represent a confirmed inability to make forward persistence progress. New writes will fail fast with a dedicated storage-capacity error once the engine reaches this terminal availability state, while previously durable state remains recoverable.

Recovery from STORAGE_FULL will require evidence that storage availability has returned and that at least one subsequent persistence attempt succeeds. A successful persistence transition returns the engine to **HEALTHY**.

This ADR changes **failure-handling and write-availability behavior**, but it does not change the WAL binary format, SSTable binary format, Manifest format, sequence assignment rules, durability watermark rules, checkpoint ordering, or WAL-purge safety rules.

## 2. Context

The current production write path is:

Write API → Dedicated Batch Coordinator → Group Commit → WAL → MemTable → Immutable MemTable → SSTable → Manifest → Checkpoint → WAL Purge.

During the realistic full-pipeline soak, the test volume exhausted its available storage. The resulting failure exposed two separate issues. The test fixture was using a chronically nearly-full temporary volume, which caused ENOSPC earlier than an appropriately provisioned production volume would have. More importantly, the actual Write Engine continued retrying the failed flush indefinitely after the configured `max_flush_retries` value had been exceeded.

The implementation currently uses `max_flush_retries` to select a backoff duration rather than as a retry limit. After the configured threshold, the code falls into a fixed two-second retry cadence indefinitely. The observed soak produced thousands of repeated flush failures and a prolonged availability collapse, confirming that the behavior is not merely theoretical.

The same soak also demonstrated that the database retained crash-recovery correctness despite the storage-pressure failure. Recovery completed successfully, Manifest state remained coherent, SSTables remained reconciled, and no durability violation was observed. Therefore the required change is primarily an **availability, retry, and backpressure mechanism**, not a redesign of the persistence format or durability protocol.

The existing certification results therefore remain valid for the already-tested correctness properties, but the Write Engine remains **NOT READY** until storage-pressure behavior is explicitly designed, implemented, tested, and re-verified.

## 3. Problem Statement

The current implementation does not distinguish between a transient I/O failure and a persistent storage-capacity failure.

As a result, a filesystem that cannot accept another SSTable can cause the flush thread to remain occupied indefinitely. This prevents normal flush progress and may prevent checkpoint advancement, which in turn delays safe WAL reclamation. At the same time, callers can continue submitting requests until some unrelated capacity limit or direct WAL failure appears.

This creates three production risks.

The first is an **unbounded retry loop**.

The second is an **unbounded failure-generation loop at the caller boundary**, where failed writes can be retried by callers without receiving a stateful indication that the storage subsystem is unavailable.

The third is the absence of an explicit **operator-visible storage-health state**, making it difficult to distinguish normal transient persistence failures from persistent storage exhaustion.

The design must remove these three failure modes without weakening durability.

## 4. Design Goals

The implementation must make storage exhaustion a **first-class operational condition**.

The implementation must guarantee that repeated ENOSPC does not result in an infinite short-interval retry loop.

The implementation must provide **controlled write backpressure** when persistence cannot continue.

The implementation must retain all data required for recovery.

The implementation must never advance the durable checkpoint merely because a flush attempt was scheduled or partially executed.

The implementation must never purge WAL records whose state is not durably represented by the Manifest checkpoint.

The implementation must allow storage recovery without requiring a process restart when the underlying filesystem becomes writable again.

The implementation must expose enough state and metrics for an operator or test harness to determine whether the database is healthy, under storage pressure, or storage-full.

The implementation must preserve the established `CapacityExceeded` contract for MemTable-freeze backpressure.

The implementation must introduce no change to existing on-disk formats.

## 5. Non-Goals

This ADR does not introduce Compaction.

This ADR does not reduce SSTable count.

This ADR does not change the RUBIC SSTable format.

This ADR does not change the Manifest format.

This ADR does not change WAL encoding or recovery format.

This ADR does not introduce replication or multi-node behavior.

This ADR does not attempt to solve general disk-capacity planning.

This ADR does not turn `CapacityExceeded` into a storage-full error.

This ADR does not guarantee that writes remain available when the physical storage device has no capacity. In that condition, controlled rejection is preferable to silent degradation or unbounded memory growth.

## 6. State Model

### 6.1 HEALTHY

HEALTHY is the normal operating state.

The Write Engine may accept writes according to normal capacity rules.

Flush workers operate normally.

Checkpoint advancement operates normally.

WAL purge operates according to the existing durable-checkpoint rules.

Transient I/O failures use the normal bounded retry mechanism.

A successful flush clears any transient retry state.

### 6.2 STORAGE_PRESSURE

STORAGE_PRESSURE means the engine has observed a storage failure that requires special handling, but persistence may still become available again without operator intervention.

The principal trigger is an **ENOSPC-classified failure** after the bounded fast-retry budget has been exhausted.

In STORAGE_PRESSURE, the current immutable MemTable must remain retained.

The failed flush must remain retryable.

The engine must not advance the checkpoint because a flush has not successfully completed its full durable sequence.

The engine must not purge WAL solely because the flush attempt exists.

The engine must stop the current two-second-forever retry behavior.

Subsequent retries must use a **longer, bounded, configurable recovery cadence** and, where platform support permits, an explicit storage-health check before another expensive persistence attempt.

The system must emit a state-transition event when entering STORAGE_PRESSURE rather than emitting an unbounded stream of identical error lines.

Normal successful writes may continue temporarily only while the engine has sufficient MemTable and WAL headroom to preserve bounded resource behavior.

When continued persistence failure causes the engine to reach its configured safe resource boundary, the state transitions to STORAGE_FULL.

### 6.3 STORAGE_FULL

STORAGE_FULL means the engine cannot safely make persistence progress and continuing to accept new writes would create uncontrolled resource growth or immediate write failure.

Once in STORAGE_FULL, new write requests must fail fast with a dedicated **storage-capacity error** rather than repeatedly entering the same persistence failure path.

Previously durable writes remain durable.

Previously applied MemTable state remains retained until its persistence lifecycle can complete.

Checkpoint state remains unchanged until a successful persistence sequence occurs.

WAL purge remains constrained by the existing checkpoint rule.

The engine must remain recoverable and must be able to transition back toward normal operation after storage becomes available.

## 7. Error Classification

The persistence path must distinguish at least two broad categories.

The first category is **transient I/O failure**, where a bounded number of immediate retries remain useful.

The second category is **storage-capacity failure**, represented by ENOSPC or the corresponding platform-specific disk-full condition.

ENOSPC must no longer be treated as an indistinguishable generic `EngineError::Io` condition for retry-policy purposes.

The classification may preserve the existing low-level I/O error for compatibility, but the flush subsystem must derive a storage-pressure-specific classification from the underlying operating-system error.

The classification must not depend on parsing human-readable error strings.

The implementation must use the platform's structured error identity wherever available.

## 8. Retry Policy

The existing configured `max_flush_retries` value of 3 will remain the **bounded fast-retry budget**.

For an ordinary transient I/O failure, the current fast-retry model may continue unchanged.

For ENOSPC, the system may perform the same bounded fast-retry budget if the implementation determines that a transient allocation or filesystem race can clear immediately. After that bounded budget, the engine must not enter the current infinite two-second retry loop.

Instead, the engine transitions to STORAGE_PRESSURE.

The storage-pressure retry policy must be explicitly stateful.

Each subsequent attempt must be separated by a **longer recovery interval** than the normal transient retry interval.

The interval must be configurable.

The retry sequence must be capped so that a permanent storage failure cannot produce a tight periodic disk-and-CPU storm.

Where a reliable platform-specific free-space check is available, a retry should first perform that inexpensive health check and skip the expensive flush attempt when the storage condition is still clearly unavailable.

The exact default retry interval and maximum recovery cadence are implementation parameters and must be established by benchmark and fault-injection evidence during implementation. They must not reproduce the current two-second-forever behavior.

A successful flush resets the retry state and allows transition toward HEALTHY.

## 9. Backpressure Contract

Backpressure is a **write-availability mechanism**, not a durability mechanism.

When the engine enters STORAGE_PRESSURE, it must preserve the existing data already accepted into the durable write path.

The triggering failed flush must not discard its Immutable MemTable.

The write path must not acknowledge persistence that has not occurred.

The Write API may continue accepting writes only while the system can maintain bounded WAL and MemTable resources.

When the safe resource boundary is reached, new writes must be rejected deterministically.

When the engine is STORAGE_FULL, the Write API must return the new storage-capacity error without performing a repeated expensive persistence operation merely to discover that the filesystem is still full.

The existing `CapacityExceeded` error remains reserved for its established MemTable-freeze/backpressure semantics. It must not be repurposed to mean filesystem exhaustion. The existing project contract is that `CapacityExceeded` represents a durable-and-applied write for which the requested MemTable freeze cannot proceed.

A caller receiving a storage-capacity error must be able to distinguish it from a generic application failure and may retry after storage availability returns.

## 10. Durability Invariants

The following invariants remain unchanged.

`durable_through` must never advance beyond the actually durable WAL prefix.

Checkpoint sequence must never move backwards.

A failed flush must never become a successful checkpoint merely because the SSTable file was created.

The existing persistence order remains:

SSTable durable publication → Manifest ADD durable → checkpoint marker durable → Manifest SET_CHECKPOINT durable → WAL purge eligibility.

WAL purge must remain dependent on the durable checkpoint and must never become an independent response to storage pressure.

These invariants are already established by the current Write Engine failure model and must remain unchanged by this ADR.

## 11. Flush Failure Semantics

A failed flush retains ownership of its Immutable MemTable.

The flush state must remain retryable.

A failed attempt must not create an apparently live SSTable unless the existing publication rules have been satisfied.

The existing independent state tracking for publication, checkpoint marker, and checkpoint recording must remain intact.

A storage-pressure event must therefore stop or delay progress without weakening the already-established idempotent retry state machine.

## 12. Recovery Semantics

If the process crashes while in STORAGE_PRESSURE or STORAGE_FULL, startup recovery follows the existing Manifest and WAL rules.

The engine must reconstruct the last durable checkpoint.

The engine must replay the recoverable WAL suffix.

The engine must not treat a failed flush as a successful checkpoint.

The engine must not purge WAL based on an in-memory storage-pressure state.

Recovery must therefore remain equivalent to the existing crash model.

The existing certification evidence already demonstrates successful recovery after severe storage pressure, so this ADR is intended to preserve that property while improving availability behavior.

## 13. Recovery from STORAGE_FULL

The engine may move from STORAGE_FULL to STORAGE_PRESSURE only after a storage-availability check indicates that retry is reasonable.

It must not move directly to HEALTHY merely because free space appears to have returned.

A successful end-to-end flush is required before normal operation is restored.

After a successful flush, checkpoint advancement may proceed according to the existing rules.

After successful persistence and stable resource state, the engine returns to HEALTHY.

This prevents a false recovery state in which the disk has free space but the actual persistence operation still fails for another reason.

## 14. Observability

The Write Engine must expose a distinct storage-pressure state.

At minimum, the operator-visible metrics must distinguish:

current storage state

ENOSPC event count

storage-pressure transitions

storage-full transitions

flush failures

flush retries

flush retry exhaustion

current immutable backlog

SSTable count

WAL size

checkpoint sequence

durable sequence

WAL purge progress

The existing failure analysis identified that current metrics provide no dedicated signal for repeated storage-exhaustion-induced flush failure. This ADR explicitly closes that observability gap.

Repeated identical errors must not flood logs.

The preferred logging pattern is a state transition plus aggregate retry information rather than one log line per retry.

Keys, values, and application payloads must never appear in these diagnostics.

## 15. Test-Harness Requirements

The realistic full-pipeline soak harness must not consider a run successful merely because the process returns exit code zero.

A soak PASS must also require healthy workload completion and absence of a persistent storage-pressure failure.

The harness must detect at least:

nonzero completed errors

persistent throughput collapse

ENOSPC in stderr

unbounded retry behavior

unsuccessful recovery

incomplete shutdown

The existing certification harness was already corrected to apply these stronger conditions; that correction must remain permanent.

## 16. Capacity-Exhaustion Test

A dedicated deterministic **ENOSPC test** must intentionally exhaust a controlled test volume or bounded quota.

The test must not fill the real system disk.

The expected sequence is:

healthy storage

flush succeeds

storage becomes unavailable

ENOSPC is classified

bounded fast retries occur

STORAGE_PRESSURE is entered

retry rate becomes controlled

immutable data remains retained

checkpoint does not advance incorrectly

WAL purge does not advance incorrectly

new writes eventually receive deterministic backpressure when the safe resource boundary is reached

storage is restored

flush resumes

checkpoint progresses correctly

normal operation resumes

The test must verify that no infinite short retry loop occurs.

## 17. Crash During STORAGE_PRESSURE

A dedicated external-process crash test must terminate the process while the engine is in STORAGE_PRESSURE or STORAGE_FULL.

After restart, verify:

Manifest recovery

SSTable validity

checkpoint correctness

WAL recovery

sequence monotonicity

durable watermark monotonicity

no acknowledged durable write loss

no unsafe WAL purge

no orphaned live state

correct transition back to normal operation after storage is restored

## 18. Regression Requirements

All currently passing WAL, Group Commit, MemTable, SSTable, Manifest, and crash-recovery tests must continue to pass.

The existing 205/205 external crash-cycle evidence establishes the current durability baseline that this change must preserve.

No existing successful durability invariant may be relaxed to improve storage-pressure availability.

## 19. Endurance Re-Test Requirements

The realistic full-pipeline soak must not be rerun on the previously exhausted test fixture.

The earlier analysis estimated that the selected workload could require roughly **10–15 GB of live SSTable storage** for an undisturbed 14,400-second run without Compaction, based on the observed healthy-period SSTable creation rate. This is an order-of-magnitude planning estimate, not a contractual capacity number.

Before the next endurance run, the test environment must therefore have sufficient dedicated storage capacity with an explicit safety margin.

The final endurance test must exercise:

WAL

MemTable

freeze

RUBIC SSTable

Manifest

checkpoint

WAL purge

storage-pressure monitoring

recovery

The corrected soak harness must evaluate the actual workload health rather than process exit alone.

## 20. Performance Acceptance

The storage-pressure changes must not materially degrade the healthy-state write path.

Final performance testing must compare the new implementation against the current accepted Dedicated Batch Coordinator baseline.

The original hard targets remain:

100 writers: at least 15,000 ops/sec

1000 writers: at least 80,000 ops/sec

Performance measurements must be performed on an idle machine and separated from long-duration disk-pressure workloads.

The previously observed high run-to-run variance must remain documented rather than being hidden.

The clean-machine remeasurement demonstrated that the earlier post-soak 75–76K result did not reproduce as a stable ceiling, while still showing substantial variance across all layers.

## 21. Alternatives Considered

The current **infinite two-second retry loop** is rejected because the realistic soak demonstrated that it can consume the remainder of a multi-hour run without making meaningful persistence progress.

An immediate permanent failure on the first ENOSPC is rejected because storage may become temporarily unavailable and the Write Engine should retain the ability to recover without losing accepted durable state.

Blindly continuing to accept writes during persistent storage exhaustion is rejected because it can produce unbounded memory, WAL, or queue pressure.

Using `CapacityExceeded` for filesystem exhaustion is rejected because that would corrupt the meaning of an already-established MemTable backpressure contract.

Relying only on exit code to determine soak PASS is rejected because the previous realistic soak returned exit code zero despite severe workload failure.

## 22. Compatibility

No on-disk compatibility break is introduced.

No WAL frame-format change is introduced.

No SSTable format change is introduced.

No Manifest format change is introduced.

Existing recovery behavior remains authoritative.

The primary compatibility change is the addition of a more explicit **write-availability error/state** for persistent storage exhaustion.

The new error must be additive to the existing error model and must not change the established meaning of existing error variants.

## 23. Rollback Strategy

If the new state machine introduces a correctness regression, the implementation may be disabled behind an internal configuration flag during development, but the production default must not revert to the old infinite ENOSPC retry behavior.

Any rollback must preserve the existing crash-safety and idempotent-persistence safeguards.

The old infinite retry behavior is not considered a safe production fallback.

## 24. Acceptance Criteria for Implementation

This ADR is considered successfully implemented only when a deterministic test proves that ENOSPC causes bounded behavior rather than an infinite retry storm.

A deterministic test must prove that STORAGE_PRESSURE is observable.

A deterministic test must prove that STORAGE_FULL produces bounded write availability behavior.

A deterministic test must prove that immutable data remains recoverable.

A deterministic test must prove that checkpoint does not advance incorrectly.

A deterministic test must prove that WAL purge remains safe.

A deterministic test must prove that storage restoration allows successful recovery to HEALTHY.

A crash-under-storage-pressure test must pass.

The complete existing regression suite must pass.

The healthy-state performance benchmark must not materially regress.

The corrected full-pipeline soak must subsequently complete without ENOSPC-induced collapse on a properly provisioned test volume.

## 25. Final Decision

**Adopt an explicit HEALTHY → STORAGE_PRESSURE → STORAGE_FULL state machine, classify ENOSPC separately, retain bounded transient retries, eliminate the current infinite two-second retry behavior, introduce controlled storage-aware backpressure, and require successful persistence before returning to HEALTHY.**

This decision preserves the existing **durability model** while making storage exhaustion a deliberate and observable **availability state**.

## 26. Implementation Boundary

The implementation should be performed in independently verified increments.

First implement error classification and the state representation.

Then implement the retry-policy transition.

Then implement backpressure.

Then implement storage-pressure observability.

Then add deterministic ENOSPC tests.

Then add external crash-under-ENOSPC tests.

Then rerun the complete regression suite.

Then perform the final full-pipeline endurance soak on a correctly provisioned volume.

Do not combine implementation, optimization, and certification into one uncontrolled change.

**This ADR must be approved before production write-path behavior is changed.**

---

## Implementation Notes (added 2026-09-19, post-implementation)

This ADR was approved and implemented the same day. What actually landed, verified with real command output (not asserted):

- **§7 Error classification**: `EngineError::StorageExhausted` (new variant, `src/error.rs`), plus a free function `is_enospc(&io::Error)` that checks `io::ErrorKind::StorageFull` first, falling back to the raw OS codes (`112` Windows, `28` POSIX) — never parses `Display` text. `EngineError::is_storage_exhausted()` classifies any `Io` variant this way.
- **§6 State model**: `lsm::StorageState { Healthy, StoragePressure, StorageFull }`, `AtomicU8`-backed on `LsmEngine`, exposed via `storage_state()`. Transitions live in `spawn_flush_thread`'s retry loop (`StoragePressure` entry, `Healthy` on any flush success) and in `freeze_locked` (`StoragePressure` → `StorageFull` promotion, on the immutable backlog reaching `max_immutable_memtables` — the existing resource boundary, reused rather than adding a new one).
- **§8 Retry policy**: `max_flush_retries` now genuinely bounds the fast-retry phase (unchanged 50ms×attempt ramp). Past that budget, an ENOSPC-classified failure enters `StoragePressure` and backs off at the new `LsmConfig::storage_pressure_retry_interval` (default 5s) instead of the old flat-2s-forever. A non-ENOSPC I/O error past the budget keeps the old flat-2s-forever cadence — **deliberately out of this ADR's scope** (targeted at storage-capacity exhaustion specifically; see `PHASE5_ENOSPC_FAILURE_ANALYSIS.md` §6), with logging throttled the same way regardless. The optional platform free-space pre-check (§8, "where a reliable check is available") was **not implemented** — it would need a new dependency (no cross-platform free-space API in `std`), which is a bigger decision than this increment; flagged here rather than silently dropped.
- **§9 Backpressure**: `LsmEngine::put`/`delete` call `reject_if_storage_full()` first, returning `StorageExhausted` before any `pool.submit()`/WAL append when `StorageFull` — verified in the test below by asserting `pool_stats().submitted` does not move on a rejected call. `CapacityExceeded`'s existing return from `freeze_locked` is completely unchanged.
- **§13 simplification, stated explicitly**: the ADR describes `StorageFull → StoragePressure → Healthy` as a required two-step (never trust free space alone; require an actual successful persistence attempt). The shipped code transitions `StorageFull`/`StoragePressure` → `Healthy` directly, but *only* on the flush thread's own success branch — i.e. only after the exact "successful persistence attempt" the ADR requires. The safety property is preserved exactly; the two-step intermediate state was collapsed because nothing else in this implementation ever moves the state on anything weaker than a real success.
- **§16 Capacity-exhaustion test**: `lsm::tests::storage_pressure_state_machine_recovers_after_injected_enospc` (`src/lsm/tests.rs`). Rather than filling a real or quota-limited volume, it injects a real `io::ErrorKind::StorageFull`-shaped error at the exact call site the 2026-09-19 soak actually failed at, via a new `LsmEngine::install_flush_io_fault_hook` (extends the existing `install_flush_fault_hook`/`FlushFaultPoint` pattern already used by this codebase's panic-injection tests, but able to substitute a real I/O outcome rather than just observe). Walks the full sequence: `Healthy` → `StoragePressure` (bounded fast-retry budget exhausted) → data retained / checkpoint frozen at 0 / no SSTable published → `StorageFull` (backlog fills) → new write rejected fast with zero `submitted` movement → fault cleared → `Healthy` again with checkpoint/SSTable progress → normal writes resume. Verified stable across 5 consecutive runs.
- **§17 Crash-during-storage-pressure test**: `examples/storage_pressure_crash_child.rs` + `examples/storage_pressure_crash_test.rs`, mirroring the existing `lsm_crash_cycle_{child,test}.rs` external-process pattern (`Child::kill()`, not a self-`abort()`), but synchronized to a marker line the child prints the instant it observes `StorageState::StorageFull` (unconditional injected ENOSPC, so the child reaches that state deterministically), instead of a blind wall-clock delay. Verifies, per cycle: `LsmEngine::open` recovers cleanly; `checkpoint_seq()==0` and `sstable_count()==0` (nothing could have legitimately flushed); `durable_through <= highest_sequence`; at least one WAL record replays; and — storage now "restored" (fresh reopen, no fault hook) — a real flush completes and normal writes resume. **Caught and fixed two real bugs in the test itself before trusting it** (first run: reused one directory across cycles, so a later cycle inherited an earlier cycle's own legitimate post-recovery progress, producing a false `checkpoint_seq != 0` failure; and a too-strict treatment of the pre-existing `CapacityExceeded` backpressure contract as a test failure during the post-recovery write burst). Verified stable across 10 consecutive cycles after both fixes.
- **Regression suite**: `cargo test --lib` / `--release --lib` / `--features test-util`: 255/255 (254 pre-existing + 1 new). `cargo clippy --all-targets --all-features -- -D warnings`: clean. `cargo fmt --check`: clean. One pre-existing, unrelated flaky test (`execution::batch_coordinator::tests::coordinator_panic_before_batch_formation_fails_safely`, a file this ADR's implementation never touched) was observed to fail intermittently under full-suite parallel load both before and after this work, and pass reliably alone or on a clean rerun — flagged here as a known pre-existing flake, not attributed to this change, and not fixed as part of this ADR (out of scope).
- **§19/§20 (endurance re-soak, final performance) — UPDATE 2026-09-20: DONE.** `PHASE_WRITE_ENGINE_STORAGE_BUDGET.md` computed the storage budget (E: measured at 94.66 GB free vs. ~35.78 GB padded/margined requirement) and a harness-only `RUBIXDB_SOAK_BASE_DIR` fix forced the soak's database directory onto `E:`. Both storage-pressure tests above were re-verified fresh same-day (5/5, 10/10) immediately before launch. The soak ran the full 14,400s on `E:` and passed clean: `completed_err=0` throughout, throughput sustained 19,332-26,116 ops/sec, 3,294 SSTables, checkpoint advancing continuously, 0 ENOSPC events (peak usage ≈8.86 GB, well within budget), clean recovery. A second real bug was caught and fixed in the *harness* itself during this step (`-notmatch` array-filtering semantics producing a false FAIL despite the log genuinely containing `recovery OK`/`fully_drained=true`) — verified by replaying the corrected logic against both this passing run and the original failing run before trusting it.
- **UPDATE 2026-09-20 (continued): the passing soak's own ~584 MB RSS growth was then investigated in full** (flagged before certifying, not certified past on trust) — see `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md`. Conclusion: expected, bounded-per-SSTable metadata growth (Bloom filter + sparse index retained per open `SsTable`, no Compaction to reclaim them yet), confirmed by an independent bounded scaling measurement (R²=0.9999) and full source-level ownership tracing, not a leak. `sync_failures=1` traced to a counter-sampling artifact, not a real failure. No code fix required; a regression test was added. The 100w/1,000w acceptance benchmarks were also re-examined more fully (9 reps across 3 sessions, not the single favorable 3-rep set first reported): median clears both hard targets, but individual-run variance is real and was already a known, previously-documented, non-blocking open characteristic before this session began — reported honestly, not smoothed over. Full detail and the final gate-by-gate certification matrix: `PHASE_WRITE_ENGINE_CERTIFICATION.md`.
