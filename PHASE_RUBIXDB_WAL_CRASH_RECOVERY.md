# PHASE RUBIXDB — WAL CRASH, RECOVERY & DURABILITY EVIDENCE

**Date:** 2026-10-03 · **Subject:** the implemented change (stage 1: count-aware, quiescence-guarded window close in `src/wal/group_commit.rs`) on branch `wal-batch-buffer-fillq`.
Because `src/wal/` was modified, the previous WAL certification is invalidated *for the changed behavior*; this document is the re-certification evidence. All runs below are on the **final** build unless stated.
Legend: **[RUN]** executed on the final build · **[SUITE]** automated test that passed in the final debug+release regression.

## 1. What the change can and cannot affect (reasoned before testing)
| Property | Can the change affect it? | Why |
|---|---|---|
| Which records a given `fsync` covers | No | decided at `snapshot_sync_target`, after the window closes; unchanged |
| When `durable_through` advances / acks | No | still published only after a successful `sync_all`; unchanged |
| Ordering / sequence assignment | No | seq assigned under the WAL lock exactly as before; the new `appended_seq` is a read-only observer (`fetch_max`) |
| On-disk format / recovery | No | no write-path or format change |
| Failure handling / poisoning | No | no change to append, rollback or fsync-failure paths |
| *When* the leader proceeds to fsync | **Yes (earlier only)** | the sole behavioral change; bounded by the existing deadline |
So the risk surface is *timing*: early close could in principle produce smaller batches or starve a straggler, never lose or reorder an acknowledged record. The tests below are aimed at that and at regressions.

## 2. Unit / integration / property / fuzz suites [SUITE]
Final regression (`cargo test --workspace --no-fail-fast`), **debug and release identical:** 1,115 passed / 2 failed / 26 ignored. The 2 failures are the ENGINE-BLOCKED throughput targets `m1_2`/`m1_3`; **every correctness test passes.** Relevant binaries (both modes): `rubixdb` lib **549/549** (includes the WAL unit suite, `wal::fuzz_tests`, group-commit abort-point tests and 2 new tests), `tests/wal_tests` 12/12, `tests/pathological_recovery_matrix` 9/9, `tests/crash_consistency` 2/2, `tests/group_commit` 6 passed (M1.1 single-writer latency, M1.4 leader-failure propagation, M1.5 rotation mid-batch, M1.6 `crash_consistency_across_abort_points`, watermark monotonicity) + the 2 throughput failures. 101 crash/abort/recovery/shutdown/watermark-named tests passed in each mode.
New tests: `cohort_close_decision_table` (exhaustive pure-function table incl. cutoff and scaling) and `small_prior_batch_does_not_ratchet_batches_small` (64 concurrent writers after a single-writer phase, simulated 3 ms fsync: all records covered, `durable_through == highest_sequence`, batches average >= 4 — i.e. no ratchet).
**Abort-point windows covered by existing suites [SUITE]:** BeforeLeader, AfterLeaderElection, DuringBatchWaitPre/Post (the code I modified), MidAppend, BeforeSync, AfterSync, AfterWatermarkBeforeWake, rotation pre/post, abort inside batch (11 points, `crash_consistency_across_abort_points`, real child process per point).

## 3. Real external process-kill cycles [RUN] (final build)
`Child::kill()` = `TerminateProcess`: abrupt, no cooperation, may land mid-syscall.
| Harness | Config | Cycles | Result |
|---|---|---|---|
| `crash_cycle_test` (WAL layer) | 16 writers, seed 142 | 40 | all gap-free, 0 corrupted segments |
| | 100 writers, seed 107 | 40 | all gap-free, 0 corrupted segments |
| | 256 writers, seed 1334 | 30 | all gap-free, 0 corrupted segments |
| `lsm_crash_cycle_test` (full engine: WAL -> memtable -> reopen) | 16 writers, seed 142 | 30 | **30/30 OK**, 0 read mismatches, `durable_through` never regressed |
Earlier-build runs of the same harnesses (before the final cutoff/scaling tweak; not counted): 110 + 30 cycles, all clean.
**Limitation (stated honestly):** these two harnesses verify prefix consistency (gap-free, no corruption, watermark monotone). They do **not** by themselves prove that every *acknowledged* record survived — the child's acks are explicitly "not load-bearing" in their own comments. Hence §4.

## 4. Independent reference model — the acknowledgement oracle [RUN]
`examples/wal_ack_oracle.rs`. **Not an oracle derived from the WAL.** The child process runs W writers on `GroupCommitter::append_durable`; a writer prints an ack line to a pipe **only after** `append_durable` returned `Ok`. The parent receives those lines *outside* the process, kills the child at a seeded random moment (150-2,500 ms), reopens the WAL and checks, using payloads **it derives itself** from `(writer, n)`:
1. no corrupted segments; recovered sequence numbers gap-free from 1;
2. **every received ack is present** after recovery, byte-exact (key and value; for atomic `Group` writes all 3 members, in order), at its acknowledged seq;
3. per writer, recovered operations form a contiguous prefix `0..m` with `m` at most a small slack above the last received ack (only in-flight operations may exceed it);
4. group (multi-record transaction-shaped) writes are all-or-nothing — a partial group is a failure.
Cycles accumulate in the same directory; writer ids are unique per cycle. 1 in 4 writers uses 3-member `Group` frames (the shape transaction commits use).

| Config (cycles x writers, seed, window µs) | Cycles | Acks verified | Failures |
|---|---|---|---|
| 50 x 16, 211, 5000 | 50 | 144,303 | **0** |
| 50 x 100, 222, 5000 | 50 | 322,594 | **0** |
| 40 x 256, 233, 5000 | 40 | 369,089 | **0** |
| 40 x 64, 244, 1000 | 40 | 225,925 | **0** |
| 30 x 8, 255, 5000 | 30 | 54,372 | **0** |
| 30 x 400, 266, 5000 | 30 | 362,872 | **0** |
| **Total** | **240** | **1,479,155** | **0** |
(Plus 210 cycles / ~1.08 M acks on the earlier build, also 0 failures.)

**Oracle self-test (it can fail):** with `ACK_EARLY=1` the child acknowledges *before the record is even written* (so it lives only in process memory). The oracle reports `ACKED RECORD LOST` (the final-build self-test printed 8 loss lines, which is the oracle's per-cycle output cap of 8, so it shows at least one failing cycle, not a count of lost records). **Important negative finding about the method:** my *first* mutant — acknowledging after `append` but before the fsync — was **not** detected, because a killed process's already-written bytes remain in the OS page cache and survive. **Process-kill testing cannot detect ack-before-fsync bugs; that class needs power-loss simulation, which was not available.** For that property this phase relies on (a) the change not touching the fsync/watermark path (§1), (b) existing tests that gate `durable_through` publication on a successful `sync_all` (`watermark_monotonicity`, M1.4 fault-injected fsync failure, `AfterSync`/`AfterWatermarkBeforeWake` abort points), and (c) code review of the diff (2 deleted lines: the call and the signature).

## 5. Requested kill windows — coverage map
| Window | Covered by | Status |
|---|---|---|
| single write | oracle (n=1 phase), M1.1, kill cycles | covered |
| batch write | oracle / kill cycles (batches of 8-400) | covered |
| multiple writers | all cycles (8-400 writers) | covered |
| transaction commit / multi-record transaction | oracle `Group` writers; `lsm_crash_cycle_test` (engine path) | covered at WAL/engine level; **SQL-level transaction kill not re-run** (the product-level crash test passed in the earlier certification, `crash_recovery_integration` 4/4 in this regression) |
| concurrent commits | oracle (400 writers), M1.2/M1.3 workloads | covered |
| shutdown during flush | M1.4/M1.5 + shutdown unit tests [SUITE] | covered by suite; **no dedicated external-kill-during-graceful-shutdown run** |
| kill during WAL append | `MidAppend` abort point [SUITE] + random external kills | covered |
| kill during WAL flush | `BeforeSync`/`AfterSync` abort points [SUITE] + random external kills | covered |
| **restart during recovery** | — | **NOT RUN** (recovery is read-only until tail truncation and every cycle reopens a directory left by a prior kill; a kill *inside* recovery was not specifically targeted) |
| power loss (fsync semantics) | — | **NOT RUN** (no capability; see §4) |

## 6. Recovery comparison against a reference
Recovered state vs the oracle's independently derived expectation: **0 mismatches** over 1,479,155 acknowledged records (final build). `lsm_crash_cycle_test` additionally verifies reads after engine reopen (`reads_verified_ok=true`, 0 mismatches).

## 7. Shutdown, resources, leaks
Clean-shutdown behavior is covered by the M1.4-M1.6 / shutdown suites (all pass). Threads (105 / 1,005), handles (160-166 / 1,061), RSS (<= 88 MB), CPU (13.5 s / 82.5 s) are unchanged vs baseline (`..._PERFORMANCE.md` §5). No new threads, queues, buffers or channels; added state is two `AtomicU64`. WAL size on disk was **not** compared between builds; by construction the bytes written per record are identical (no write-path or format change).

## 8. Gates
DURABILITY **PASS** (ack-after-fsync path untouched; oracle 0 loss; *power-loss testing not possible here*) · ORDERING **PASS** · RECOVERY **PASS** · CRASH CONSISTENCY **PASS** · TRANSACTION ATOMICITY **PASS** (Group all-or-nothing, 0 partial groups) · SHUTDOWN **PASS** (suite) · THREAD/HANDLE/RSS/CPU STABILITY **PASS**.
