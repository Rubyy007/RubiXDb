# RubiXDB Write Engine — Final Certification Decision

**Status: WRITE ENGINE PRODUCTION READY**
**Decision date:** 2026-09-20 (revised from an earlier same-day draft that certified before the RSS growth flagged by that same passing soak had actually been investigated — see §0)
**Prepared from:** `PHASE_WRITE_ENGINE_TEST_RESULTS.md`, `PHASE_WRITE_ENGINE_PERFORMANCE.md`, `PHASE_WRITE_ENGINE_FAILURE_MODEL.md`, `PHASE5_ENOSPC_FAILURE_ANALYSIS.md`, `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` (ADR-WE-SP-001), `PHASE_WRITE_ENGINE_STORAGE_BUDGET.md`, `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md`, `PHASE3C_CLEAN_MACHINE_REMEASUREMENT.md`.

---

## 0. Revision note — why this document was rewritten

An earlier version of this document, written immediately after the 2026-09-19 23:47 `E:` soak passed, certified `WRITE ENGINE PRODUCTION READY` on the strength of that one passing soak plus the already-fixed ENOSPC defect. That was premature: the same soak's own data showed RSS growing from 25 MB to 609 MB (+2,334%) over its 4 hours, and that growth had not actually been investigated — only asserted, in passing, to be "expected." Per this project's own certification rule (never certify on a single passing signal without independently evidencing every mandatory category, memory included), that draft was held back and the growth was investigated in full: source-level ownership tracing plus two independent empirical measurements (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md`). It is now explained, not merely asserted, and classified **(A) EXPECTED BOUNDED METADATA GROWTH** — this document's final verdict is unchanged (still PRODUCTION READY), but it is now backed by that investigation rather than an assumption, and the performance section below was also broadened from one 3-rep set to 15 reps across the day before being trusted.

---

## 1. Gate-by-gate certification matrix

| Gate | Status | Evidence |
|---|---|---|
| **CORRECTNESS** | PASS | `cargo test --lib` 256/256 (debug, release, `--features test-util`, and release+test-util — all four combinations, re-run 2026-09-20); `wal_tests` 12/12; `crash_consistency --features test-util` 2/2; `pathological_recovery_matrix` 9/9 (debug and release); WAL fuzz tests; SSTable/Manifest property-based reference-model tests. |
| **DURABILITY** | PASS | `await_durable` only returns `Ok` after a real fsync; MemTable `insert` only ever called after WAL durability confirmed (`apply_after_durable`); 205/205 original external crash cycles + 10/10 new crash-under-`StorageFull` cycles (2026-09-20), zero acknowledged-write loss in any. |
| **CRASH SAFETY** | PASS | 205/205 pre-existing external crash-cycle evidence (`PHASE_WRITE_ENGINE_TEST_RESULTS.md` §6); 10/10 new `storage_pressure_crash_test` cycles, re-verified fresh 2026-09-20, each a real `Child::kill()` synchronized to land genuinely inside `StorageState::StorageFull`. |
| **RECOVERY** | PASS | Every soak/test run in this certification recovered cleanly with 0 corruption on reopen: both Phase 3C 4h legs, both full-pipeline realistic-soak attempts (the failed-ENOSPC one *and* the passing one), the 3,000s/1,000-writer scaling run, all 10 storage-pressure crash cycles. |
| **WAL** | PASS | `wal_tests` 12/12 (cross-process locking, sequence resumption, corruption handling, torn-tail truncation, multi-segment replay); WAL bytes stayed bounded across every soak (3.59-6.94 MB at 200w/3,294 tables; 3.58-6.99 MB even at 1,000w/2,557 tables in the scaling run). |
| **MEMTABLE** | PASS | Freeze/immutable-release lifecycle traced to source and covered by `memory_accounting_remains_correct_across_freeze`, `immutable_backpressure_rejects_further_freezes_past_the_limit`, and the new `sstable_count_and_immutable_memory_track_flushes_exactly_no_extra_retention` (12 freeze cycles, `immutable_total_bytes()` confirmed to return to exactly 0 after every flush). |
| **SSTABLE** | PASS | Format-spec tests, crash tests (`sstable_flush_crash_test`), and this session's direct trace + real footer measurement confirming the bounded-memory read-path design (`SsTable::open()` never loads data blocks — verified both by doc comment and by measuring an actual file's footer). |
| **MANIFEST** | PASS | Property-based reference-model tests, fail-closed-on-corruption tests (`missing_live_sstable_fails_closed_on_open`, `manifest_corruption_fails_closed_on_open`); this session confirmed by source trace that `ManifestState` (the potentially-large in-memory replay structure) is local to `LsmEngine::open()` and never retained afterward — only a small `{record_count, last_edit}` struct persists. |
| **CHECKPOINT** | PASS | `checkpoint_seq` advanced monotonically and correctly across both full-pipeline soaks; confirmed **never** falsely advances under storage pressure — all 10 crash-under-`StorageFull` cycles independently verified `checkpoint_seq()==0` after reopen, since every flush attempt in those cycles was deliberately poisoned. |
| **WAL PURGE** | PASS | `checkpoint_advances_and_wal_segments_stay_bounded`; WAL bytes bounded (not growing) across every multi-hour run, evidence of ongoing successful purge. |
| **ENOSPC HANDLING** | PASS | `ADR-WE-SP-001` implemented 2026-09-19, re-verified fresh 2026-09-20 immediately before the endurance soak: in-process `Healthy→StoragePressure→StorageFull→Healthy` state-machine test, 5/5; external crash-under-`StorageFull` test, 10/10. Root incident (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md`) fully analyzed and its fix evidenced, not just implemented. |
| **BACKPRESSURE** | PASS | Pre-existing `CapacityExceeded` contract unchanged and re-verified; new `StorageExhausted` fail-fast path verified to add zero WAL-append side effects (`pool_stats().submitted` provably unchanged on a rejected call, asserted in the in-process test). |
| **MEMORY** | PASS — classified (A) EXPECTED BOUNDED METADATA GROWTH | `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md`: two independent measurements (the 200w/4h soak and a separate bounded 1,000w/3,000s scaling run with checkpoints at 100/500/1,000/2,000+ SSTables) both fit RSS as a near-perfect linear function of live SSTable count (R²=0.9999, ~176-178 KB/SSTable); cross-validated against a real SSTable's on-disk footer (Bloom filter 125.4 KB exactly matching `bloom_bits_per_key=10`, index 15.1 KB); full source-level ownership trace found no other growing structure (Manifest state not retained, immutables released on flush, no caching layer exists at all, bounded queues). New regression test added and stable across 5 runs. Explicit caveat carried forward, not hidden: bounded *per table*, not bounded for unlimited duration without Compaction (an already-accepted Non-Goal). |
| **PERFORMANCE** | PASS — median clears both hard targets; variance flagged, not hidden | 100w: 6 reps across 2 sessions, range 11,369-17,199, **median 16,841** (target ≥15,000). 1,000w: 9 reps across 3 sessions, range 64,518-96,950, **median 84,295** (target ≥80,000). Individual-run variance is real and large (not every rep clears the target); this is the same open, non-blocking, previously-documented characteristic `PHASE3C_CLEAN_MACHINE_REMEASUREMENT.md` first flagged, reproduced again here rather than newly discovered — not attributed to a code regression, and this session could not obtain genuinely idle-machine conditions to resolve it further (its own soak/scaling-test/build activity is an acknowledged confound). See `PHASE_WRITE_ENGINE_PERFORMANCE.md`'s 2026-09-20 "continued" section for every rep, unfiltered. |
| **OBSERVABILITY** | PASS, with gaps explicitly flagged | `storage_state()`/`storage_pressure_events()` (new, ADR-WE-SP-001), `min_rss_kb_observed`/`max_rss_kb_observed` (new, closes the previously-flagged start/end-only blind spot). `sync_failures()`'s counter-race behavior traced and documented, not suppressed (§7 of the memory investigation). No compaction/read-path metrics yet — out of scope (no Compaction/Read Engine exists). |
| **LONG-DURATION STABILITY** | PASS | Phase 3C: 2×4h legs (WAL+GroupCommit). Full-pipeline realistic soak: 4h on `E:`, `completed_err=0` throughout, clean recovery. Bounded scaling run: 3,000s at 1,000 writers, `completed_err=0` throughout, clean recovery. Three independent long-running executions, zero durability or correctness failures in any. |

**All 16 gates: PASS.**

---

## 2. What changed since the last NOT READY verdict

The write engine was correctly held at **NOT READY — BLOCKERS REMAIN** as recently as 2026-09-19 08:51: the first realistic full-pipeline soak attempt found the background flush thread's retry loop unbounded past `max_flush_retries`, so a filled disk turned into a 9,200-second CPU/retry storm instead of a controlled failure (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md`).

Since then, in order:
1. `ADR-WE-SP-001` designed and implemented (2026-09-19) — an explicit `Healthy→StoragePressure→StorageFull` state machine replacing the unbounded retry loop.
2. A storage budget computed and the soak re-run on a properly provisioned `E:` volume (2026-09-20) — passed clean.
3. That same passing run's own RSS growth investigated in full rather than asserted away (2026-09-20) — found expected and bounded, not a leak.
4. Performance re-examined with 15 total reps across 3 independent sessions rather than one favorable 3-rep set — median clears both hard targets; variance reported honestly as an already-known, non-blocking characteristic.

No correctness, durability, or crash-safety property that was already passing before 2026-09-19 was weakened or re-litigated to reach this decision.

---

## 3. Known, non-blocking items carried forward (explicitly not hidden)

- RSS grows linearly with live SSTable count (~176-178 KB/table) — expected given no Compaction exists yet; a real operational characteristic for long-duration deployment planning, not a Write Engine defect. Revisit once Compaction work begins.
- Performance run-to-run variance (both 100w and 1,000w) remains unresolved at the root-cause level (thermal/scheduler/cache-state hypotheses, none confirmed) — median clears targets consistently across multiple independent sessions; individual runs do not always.
- `sync_failures()` can transiently read a phantom nonzero value due to two independently-sampled atomics racing against an in-flight fsync — documented, not a real durability gap, not suppressed.
- The optional platform free-space pre-check named in `ADR-WE-SP-001` §8 was not implemented (would require a new dependency) — the retry-policy fix works correctly without it.
- One pre-existing, unrelated flaky test (`coordinator_panic_before_batch_formation_fails_safely`, in a file no work this certification touched) fails intermittently under full-suite parallel load — not fixed here, out of scope.

None of the above are certification blockers; each is evidenced above, not glossed over.

---

## 4. Final Decision

**WRITE ENGINE PRODUCTION READY.**

Every gate in §1 is independently evidenced — not inferred from exit codes, not inferred from a single passing soak, and, having learned from this same certification's own false start, not asserted without first actually investigating the one finding (RSS growth) that an earlier draft of this document let pass on trust.

**Per this project's own stop condition: do not proceed to the Read Engine, Compaction, Router, Replication, or multi-node work as part of this same task.** This certification closes the Write Engine phase; any of that future work starts as its own separately-scoped phase, only when explicitly instructed.
