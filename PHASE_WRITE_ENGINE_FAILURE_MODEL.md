# RubiXDB Write Engine — Consolidated Failure Model

**Commit:** `8c90433edb21c5dcf518fe33adad1261bd96901e`

This consolidates the failure model across the full certified write path (Write API → Dedicated Batch Coordinator → Group Commit → RUBIC WAL → MemTable → Immutable MemTable → RUBIC SSTable → Manifest → Checkpoint → WAL Purge), cross-referencing the per-phase failure models (`PHASE3C_FAILURE_MODEL.md`, `PHASE4A_FAILURE_MODEL.md`, `PHASE4B_FAILURE_MODEL.md`, `PHASE5_FAILURE_MODEL.md`) with what this certification actually re-verified.

## Completion Paths (every one resolves exactly once — verified)

| Path | Evidence |
|---|---|
| Success | All 254 lib tests; both 4-hour soaks (millions of successful completions, 0 errors) |
| WAL append failure | `wal::testing::tests::injected_write_failure_is_surfaced` |
| WAL sync failure | `wal::testing::tests::injected_sync_failure_is_surfaced_and_counted`, `fsync_failure_is_surfaced_and_a_later_sync_still_works` |
| Timeout | `wal::group_commit::tests` timeout paths; `await_durable_retrying_on_timeout` documented as the correct caller pattern (not a failure) |
| Queue capacity failure | `execution::*::tests::queue_full_rejects_with_timeout_not_silently` (all 4 execution architectures) |
| Leader panic | `wal::group_commit::tests::{a_failed_leader_fsync_poisons_the_committer_permanently, leader_panic_clears_leader_active_poisons_and_recovers_cleanly_on_reopen, concurrent_followers_all_fail_fast_when_the_leader_panics}` |
| Coordinator panic | `execution::batch_coordinator::tests::coordinator_panic_{before_batch_formation,after_append,before_await_durable,after_durable,before_completion,after_drain,during_shutdown}_fails_safely` — every named stage individually covered |
| MemTable `CapacityExceeded` | `src/lsm/mod.rs` trigger site; independently reproduced live during the performance suite (`CapacityExceeded { requested: 65, max: 64 }`) |
| Flush failure | `FlushFaultPoint` injection (5 points) + `flush_thread_panic_is_caught_and_retried_without_data_loss_or_duplication` |
| Manifest failure | `manifest::recovery::tests` (torn-tail vs. corruption classification), `manifest::tests::non_tail_corruption_fails_closed_on_open` |
| Checkpoint failure | `manifest::state::tests::checkpoint_regression_is_corruption`, `AfterCheckpointMarker`/`AfterSetCheckpoint` fault points |
| WAL purge failure | `wal::tests::purge_before_{surfaces_a_remove_failure_alone, surfaces_dir_fsync_failure_and_leaves_state_consistent, combined_remove_and_fsync_failure_mentions_both}` |
| Shutdown | `execution::*::tests::shutdown_is_idempotent` (all 4 architectures), `shutdown_while_queue_is_actively_populated_still_drains_every_request`, `rapid_submit_shutdown_cycles_do_not_leak_or_hang` |

## Crash Safety (real external-process kills — this certification)

205 crash cycles across 4 harness/seed combinations, 0 failures, 0 corruption, gap-free sequences, monotonic `durable_through`/`highest_seq`/`checkpoint_seq` in every cycle. See `PHASE_WRITE_ENGINE_TEST_RESULTS.md` §6.

## Key Invariants (re-verified this certification)

- `durable_through` never exceeds the actually-durable WAL prefix — enforced structurally by `GroupCommitter`; the soaks' CSV logs show `durable_through == highest_sequence` at every steady-state sample across both 4-hour legs.
- No sequence duplication or rollback — `sequences_gap_free_from_first_surviving_record=true` on every crash-cycle and soak-final recovery.
- No false durability acknowledgement — `await_durable` only returns `Ok` after `GroupCommitter`'s own fsync completes; `Timeout` is documented and tested as the only "not yet" outcome (never a false positive).
- Manifest is the sole SSTable-liveness authority (not directory enumeration) — `missing_live_sstable_fails_closed_on_open`, `open_fails_closed_when_a_published_sstable_is_corrupt`.
- No WAL record leaves the recoverable footprint before its data is durably represented in the Manifest checkpoint — enforced by the fixed publish→ADD→marker→SET_CHECKPOINT→purge ordering in `src/lsm/mod.rs`'s flush pipeline, and empirically confirmed by 205/205 crash cycles never losing an acknowledged write.

## Known, Accepted, Non-Blocking Characteristics

- **`CapacityExceeded` backpressure**: not a failure — a documented transient signal that the caller must retry. Confirmed unchanged (`durable + applied write, freeze cannot proceed ⇒ CapacityExceeded`).
- **Phase 1 direct-thread architecture tests failing their throughput assertion** (`tests/group_commit/{hundred,thousand}_writers_throughput.rs`): expected, historical, out of scope — see `PHASE_WRITE_ENGINE_TEST_RESULTS.md` §4.
- **RSS grows linearly with live SSTable count** (~176-178 KB/SSTable, confirmed by two independent measurements, R²=0.9999): each open `SsTable` retains its Bloom filter + sparse block index in memory for the engine's lifetime, and nothing currently removes an SSTable from that set (no Compaction yet — `PHASE4B_ADR.md` ADR-P4B-1). Not a leak (full ownership trace: `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md`) — a real, already-anticipated operational characteristic of running without Compaction for a very long time, not a Write Engine defect to fix in this phase.
- **`sync_failures()` can transiently read 1 even when nothing failed**: `sync_attempts`/`sync_successes` are two independently-incremented atomics; a stats snapshot taken while one fsync is mid-flight can observe "attempted, not yet succeeded" as a phantom failure that resolves itself. Confirmed via the full 121-sample soak series (values only ever 0 or 1, never accumulating) and via source (`src/wal/group_commit.rs`). Not a real durability gap — `completed_err` and recovery corruption counts are the authoritative signals, both clean throughout. See `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` §7.

## Open Items

1. **Documented test-coverage gap** (not a defect): no single test combines an independent value-level reference model with a *real* external crash (today's coverage splits crash-safety-via-real-kill from value-correctness-via-clean-restart across different tests). See `PHASE_WRITE_ENGINE_TEST_RESULTS.md` §5.
2. **1000-writer (and 100-writer) throughput variance**: large, reproduced across multiple independent sessions and every layer/methodology tested (`PHASE_WRITE_ENGINE_PERFORMANCE.md`'s 2026-09-20 "continued" update: 9 reps ranging 64,518-96,950 for 1000w). Median clears the hard target; individual runs do not always. Root cause (code vs. environment) not isolated — no genuinely idle-machine measurement was available in any session to date. Not attributed to a code regression (clean-machine spot checks have cleared the target); carried forward as an open, non-blocking measurement-environment question.
