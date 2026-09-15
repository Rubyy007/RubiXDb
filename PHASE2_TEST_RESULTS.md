# RubiXDB Phase 2 (Write Worker Pool) — Test Results

**Single source of truth for Phase 2 pass/fail data and benchmark
numbers**, exactly as `PHASE1_TEST_RESULTS.md` is for Phase 1. Design
rationale lives in `PHASE2_WORKER_POOL_ARCHITECTURE.md`/`PHASE2_ADR.md`;
this file is measurements and verdicts only.

## 1. Repository state

```
Baseline commit (Phase 1, before any Phase 2 code): 3af373246e79793bec9b8c02cc7bb3850370c6c2
Branch: master
```

Phase 2 work is implemented on top of that commit, uncommitted at the
time this document was first written (committed together with this
document — see this file's own final section for the commit hash
recorded after the fact, per the operating brief's own instruction).

## 2. Hardware / environment

Unchanged from `FINAL_WAL_TEST.md` §2/§3 (same machine, same session):
Intel Core i7-7700 (4C/8T), 16 GiB RAM, two SATA SSDs (`E:` healthy/81%
free, `C:`/`D:` share a disk, `C:` 97% full), Windows 10 Home 10.0.19045,
`rustc 1.98.1`, `cargo 1.98.1`, Windows Defender real-time protection
on. All commands below ran with `TEMP`/`TMP` redirected to
`E:\RubiXDb\temp\rgc_bench` (the healthy volume), exactly as every prior
phase in this session did.

## 3. Current implementation under test

- `GroupCommitter`/WAL: **unchanged from Phase 1** (`61244f2`'s serial
  `LEADER_ACTIVE` design, atomic segment-creation rotation) — Phase 2
  adds zero behavioral changes to this code. The only two touches:
  `estimate_frame_len` changed from private to `pub(crate)` (reused by
  the worker pool for queue-byte accounting, avoiding a second,
  independently-drifting formula), and `WalOpOwned::as_wal_op` was
  added (a pure borrow-conversion, no format change).
- `execution::WriteWorkerPool` (`src/execution/write_pool.rs`, new):
  a bounded `Mutex<VecDeque>` + two `Condvar`s queue, `N` worker
  threads, each calling `GroupCommitter::append` once then retrying
  `await_durable` (never `append`) up to `await_retry_budget` on
  `Timeout`. Full design: `PHASE2_WORKER_POOL_ARCHITECTURE.md`.

## 4. Regression gate (run before and after every benchmark cycle)

| Command | Result | PASS/FAIL | Evidence |
|---|---|---|---|
| `cargo test --lib` | 96/96 (87 Phase 1 + 9 new `execution` tests) | PASS | terminal output, this session |
| `cargo test --release --lib --features test-util` | 96/96 | PASS | terminal output, this session |
| `cargo build --lib` / `--all-features` | clean, both configs | PASS | terminal output, this session |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean | PASS | terminal output, this session |
| `cargo fmt --check` | clean (after 2 auto-fixes applied) | PASS | terminal output, this session |
| `cargo test --release --test group_commit --features test-util` | 6/8 — **only** the pre-existing, unrelated M1.2/M1.3 throughput misses; every correctness/crash test (M1.4, M1.5, M1.6, watermark_monotonicity) passes | PASS (no new failures) | `temp/rgc_bench/` (local, not committed) |
| `cargo test --release --test group_commit --features test-util crash_consistency_across_abort_points -- --nocapture`, x2 | ok, ok | PASS | terminal output |
| `cargo test --release --test crash_consistency --features test-util` | 2/2 | PASS | terminal output |

**Zero Phase 1 regressions** — every Phase 1 result is bit-for-bit the
same class of outcome (same 2 known throughput misses, same everything-
else-passes) as recorded in `PHASE1_TEST_RESULTS.md`/`FINAL_WAL_TEST.md`.

## 5. `execution::write_pool` unit/correctness tests

`cargo test --lib execution`, run both `--test-threads=1` (serial) and
default (parallel), 3+ repetitions each, all green:

| Test | What it verifies | Result |
|---|---|---|
| `single_submit_completes_durably_and_recovers` | Basic submit → wait → durable → recoverable path | PASS |
| `many_concurrent_submitters_all_land_a_gap_free_recoverable_prefix` | 50 real OS threads × 20 ops each through a 4-worker pool: exact count, gap-free ordered sequences, recovery | PASS |
| `queue_full_rejects_with_timeout_not_silently` | Bounded backpressure: full queue → `EngineError::Timeout`, nothing silently dropped, queue depth stays bounded | PASS |
| `shutdown_rejects_new_submissions_but_drains_existing_queue` | Already-queued work survives `shutdown()`; new submissions after it are rejected | PASS |
| `shutdown_is_idempotent` | Calling `shutdown()` twice is safe, same terminal state both times | PASS |
| `into_inner_returns_the_committer_after_a_clean_shutdown` | `GroupCommitter` reclaimable after a clean shutdown, durability state intact | PASS |
| `drop_without_explicit_shutdown_still_drains_and_leaks_nothing` | RAII safety net: dropping the pool without calling `shutdown()` still completes queued work | PASS |
| `fsync_failure_propagates_to_the_completion_not_silently` | Injected `fsync` failure (`install_fsync_fault_hook`) reaches the caller's `Completion` as `Err`, never silently dropped or turned into a false success | PASS |
| `one_worker_panicking_fails_only_its_own_request_and_does_not_lose_others` | A genuine worker-thread panic (injected via the same fault hook, this time panicking) resolves its own request's `Completion` with an error and the pool still reaches a terminal `shutdown()` state — no hang | PASS |

**Stability**: each test run 3x with `--test-threads=1` and 3x with
default parallel execution — 9/9 every time, no flakes observed.

## 6. Phase 2 baseline (Phase 1, direct-thread, same session — operating brief §20)

**Command**: `cargo test --release --test group_commit --features
test-util <hundred|thousand>_writers_throughput -- --nocapture`
**Configuration**: `max_wait=5ms`, `max_batch_bytes=256KiB` (unchanged
Phase 1 default), 1,000 records/thread. **Commit**: `3af3732` (Phase 1
code, before any Phase 2 addition — identical commit this whole
document's comparisons are measured against).

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 10,093 · 10,323 · 9,847 | **10,093** |
| 1,000 writers | 59,322 · 65,447 · 60,726 | **60,726** |

This is the number every Phase 2 result below is compared against — not
an older historical run, per the operating brief's explicit rule.

## 7. Critical experiment: worker-count sweep at 1,000 logical writers

**Command**: `cargo run --release --example worker_pool_load_test --
1000 <worker_count> <per_thread>`. **Configuration**: identical `WalConfig`
to §6 (`max_wait=5ms`, `max_batch_bytes=256KiB`); `per_thread=10` for the
sweep (kept small so `worker_count=1`'s ~4s/op serial case completes in
reasonable time — see §7's own note on why this is still a valid,
comparable measurement of the underlying mechanism), `per_thread=50` for
the `worker_count=1000` confirmation point. **Date**: this session.
**Evidence**: `temp/rgc_bench/worker_sweep_1000w.txt`,
`temp/rgc_bench/worker_pool_1000w_1000workers.txt` (local, not
committed).

| Worker count | ops/sec | avg_batch_records | mean_processing | Recovery |
|---:|---:|---:|---:|---|
| 1 | 245 | 1.00 | 4.058ms | OK |
| 2 | 283 | 2.00 | 7.070ms | OK |
| 4 | 539 | 3.99 | 7.395ms | OK |
| 8 | 1,023 | 7.97 | 7.794ms | OK |
| 16 | 1,951 | 15.92 | 8.167ms | OK |
| 32 | 3,701 | 31.75 | 8.582ms | OK |
| 64 | 7,603 | 62.89 | 8.261ms | OK |
| 1,000 (= writer count) | 43,705 | 442.48 | 17.133ms | OK |

**Finding, unambiguous**: `avg_batch_records` tracks `worker_count`
almost exactly (1.00, 2.00, 3.99, 7.97, 15.92, 31.75, 62.89 for
worker_count 1/2/4/8/16/32/64 respectively) — not the 1,000-writer
logical demand. `mean_processing` (dequeue-to-completion time) stays
flat at ~7–8.6ms across every worker count from 2 through 64 — matching
this machine's already-established `fsync`-dominated batch-cycle time
(`FINAL_WAL_ANALYSIS.md`/`FINAL_WAL_TEST.md`, 4.0–6.1ms `fsync` alone) —
confirming throughput scales *because* batch size scales with worker
count, at an essentially constant per-batch cost, not because per-request
latency improves.

**Root cause, per operating brief §25's own question** — measured, not
assumed: **batch formation**, not OS-thread scheduling, not WAL
serialization, not physical durability latency, not queue coordination.
A `GroupCommitter` batch can only ever contain requests that have
already reached `append()`; requests still sitting in the pool's own
queue (however many logical writers are behind them) are invisible to
the batching window entirely. With `worker_count` workers, at most
`worker_count` requests can ever be "in" `GroupCommitter` at once — this
is a direct, deterministic architectural ceiling on `avg_batch_records`,
confirmed by the data matching it to within rounding at every point
tested.

## 8. Worker-count sweep at 100 logical writers

**Command/config**: same as §7, `writer_count=100`, `per_thread=100`.

| Worker count | ops/sec | avg_batch_records | mean_queue_wait | Recovery |
|---:|---:|---:|---:|---|
| 8 | 996 | 7.99 | 91.728ms | OK |
| 32 | 3,868 | 31.55 | 17.094ms | OK |
| 100 (= writer count) | 11,060 | 96.15 | 0.015ms | OK |

**At `worker_count = writer_count = 100`**: throughput (11,060 ops/sec)
and `avg_batch_records` (96.15) both land almost exactly on Phase 1's
own directly-measured 100-writer numbers (§6: median 10,093 ops/sec;
`FINAL_WAL_ANALYSIS.md` §8: 93.4 records/sync) — `mean_queue_wait` drops
to essentially zero, confirming that at this configuration the queue is
not really queueing at all; the pool is, in effect, reproducing Phase
1's own direct-thread behavior through an extra layer of indirection.
This is the clean converse confirmation of §7's finding: the mechanism
is exactly what the sweep shape predicts, at both writer-count levels
tested.

## 9. Overhead at parity (`worker_count = writer_count`, both levels)

| Level | Phase 1 direct (median, §6) | Phase 2, `worker_count = writer_count` | Delta |
|---|---:|---:|---:|
| 100 writers | 10,093 ops/sec | 11,060 ops/sec | **+9.6%** (within this machine's documented run-to-run noise, `FINAL_WAL_ANALYSIS.md`'s own ~9–14% spread at this level — not a confirmed real gain) |
| 1,000 writers | 60,726 ops/sec | 43,705 ops/sec | **−28.0%** |

The 1,000-writer parity point is the more reliable one to read (larger
sample, `per_thread=50` vs. `per_thread=10`, and Phase 1's own
1,000-writer noise band, §7's four-run spread in `FINAL_WAL_TEST.md`, is
~12%, well short of covering a 28% gap): even configured to eliminate
its own batch-throttling effect entirely, the pool still measurably
underperforms Phase 1's simpler direct-thread architecture, from the
queue/`Completion`/extra-allocation overhead this design necessarily
adds on top of `GroupCommitter`'s own unchanged cost.

## 10. Comparison table (operating brief §23)

| Metric | Phase 1 (direct) | Phase 2 (worker pool, best config: `worker_count = writer_count`) | Change |
|---|---:|---:|---:|
| 100-writer durable ops/sec | 10,093 | 11,060 | +9.6% (noise-band) |
| 1,000-writer durable ops/sec | 60,726 | 43,705 | **−28.0%** |
| Records/sync, 100w | 93.4 | 96.15 | +2.9% |
| Records/sync, 1000w | 693.5 | 442.48 | **−36.2%** |
| CPU / context switches | measured for Phase 1 only (`FINAL_WAL_ANALYSIS.md` §12); not separately re-measured for Phase 2 — the throughput result alone already settles the decision (§13) | — | — |

p50/p95/p99 are reported per-configuration in §7/§8's evidence files
(`worker_pool_load_test`'s own latency percentiles) rather than
duplicated here — every worker-count-below-parity configuration's
latency is trivially worse than Phase 1's (tail latency scales with
queue wait, itself driven by the same undersized-batch mechanism), so
the only latency comparison that matters for the decision is at parity,
where Phase 2's p50/p95/p99 (15.7/31.9/44.5ms at 1,000 writers,
`per_thread=50`) are not directly comparable to Phase 1's own dedicated-
test methodology (which does not report percentiles at all — only
`group_commit_load_test.rs` does, at a different, smaller op count) —
flagged as a genuine measurement-methodology gap rather than papered
over with a misleading number.

## 11. Long-duration stability, resource exhaustion, property tests

**Not run as separate, dedicated experiments this cycle.** The
operating brief's own decision-making principle (§40: "Measure → Design
→ Implement → Verify → Benchmark → Compare → Keep or Revert") stops at
"Compare" once that comparison already yields a clear, decisive,
mechanistically-explained answer — §7–§10 already show the architecture
does not achieve its stated goal at any tested configuration, and
converges to a *worse* result than Phase 1 even at its best case. Per
§26's own explicit instruction ("The Worker Pool is a hypothesis... If
the existing architecture is faster, retain the existing architecture"),
spending further effort on long-duration/resource-exhaustion/property
testing of an architecture this document is about to recommend
rejecting was judged not to be the highest-value use of further
measurement time. The correctness-focused tests in §5 (bounded queue,
backpressure, shutdown, panic/fault propagation) already cover this
component's safety properties to the depth needed to responsibly leave
the code in the tree as a documented, available-but-not-recommended
artifact (§37) — a decision this file names explicitly rather than
silently skipping past.

## 12. Security verification

- Bounded memory: `queue_capacity`/`max_queued_bytes` enforced in
  `submit()`'s own admission check (§5, `queue_full_rejects_with_
  timeout_not_silently`) — no unbounded `Vec`/channel exists anywhere in
  `write_pool.rs`.
- No unsafe code: `grep -n unsafe src/execution/write_pool.rs` returns
  no matches.
- No payload logging: every `format!`/error-detail string in
  `write_pool.rs` names request IDs, counts, durations, and states —
  never a key/value byte.
- No new filesystem access, no new path construction: the pool never
  touches a path itself; all filesystem interaction remains inside the
  unchanged `GroupCommitter`/`FileWal`.
- No new dependency: `Cargo.toml` is unchanged by Phase 2 — `write_pool`
  uses only `std::{collections, sync, thread, time}`.

## 13. Regressions found and fixed during this cycle

1. **Test-authoring bug**: two of this module's own unit tests reopened
   a WAL directory for recovery verification while the pool's own
   `GroupCommitter` (still holding the OS-level exclusive lock) was
   still alive — surfaced as `WalUnavailable`. Not a `WriteWorkerPool`
   bug; fixed by explicitly dropping the pool (or reclaiming its
   `GroupCommitter` via `into_inner`) before reopening, matching the
   same discipline Phase 1's own crash tests already follow.
2. **Real design gap, fixed in the implementation, not just the tests**:
   the first version of `process_entry` called `GroupCommitter::
   append_durable` (append + a single, non-retried `await_durable`),
   which under any real, sustained concurrent load could surface a
   transient `EngineError::Timeout` straight to the caller even though
   the underlying `seq` was already assigned and would very likely
   become durable moments later. Fixed by retrying only `await_durable`
   (never `append` — zero duplicate-append risk, since `append` runs
   exactly once) up to `WriteWorkerPoolConfig::await_retry_budget`,
   mirroring `tests/group_commit/support.rs`'s own established
   `await_durable_retrying_on_timeout` pattern, now applied in the
   production path rather than only in test harnesses. See `PHASE2_ADR.md`.
3. **`shutdown()` report edge case**: with `worker_count=0` (exercised
   only by `queue_full_rejects_with_timeout_not_silently`), `pool_state`
   could report stuck at `Draining` instead of `Stopped` after a fully-
   drained shutdown, since no `WorkerAliveGuard::drop` ever ran to flip
   it. Fixed with an explicit fallback in `shutdown()` itself.

## 14. Final target assessment (operating brief §24, unchanged from Phase 1)

Phase 2 does not change Phase 1's own target assessment — the worker
pool was evaluated as a candidate to *close the gap*, and it does not:

| Level | Target | Phase 1 (unchanged) | Phase 2 best | Status |
|---|---:|---:|---:|---|
| 100 writers | ≥15,000 ops/sec | 10,093 (67.3%) | 11,060 (73.7%, within noise of Phase 1) | **NOT ACHIEVED** |
| 1,000 writers | ≥80,000 ops/sec | 60,726 (75.9%) | 43,705 at best config (54.6%) — **worse** | **NOT ACHIEVED, and worse than Phase 1** |

## 15. Optimization decision (operating brief §37)

**REJECT WORKER POOL.**

Evidence, summarized from §7–§10: at every worker count meaningfully
smaller than the logical writer count — the entire premise the worker
pool was built to test — throughput regresses by one to two orders of
magnitude relative to Phase 1's direct-thread model, because
`GroupCommitter`'s batch size is architecturally capped at
`worker_count`, and Phase 1's own established evidence (`FINAL_WAL_
ANALYSIS.md`, `FINAL_WAL_TEST.md`) already shows `durable_ops_per_sync`
is the single lever with the most remaining leverage on this hardware.
At the one configuration where the pool avoids that ceiling
(`worker_count = writer_count`), it still underperforms Phase 1 by 28%
at 1,000 writers, from its own added queue/`Completion`/allocation
overhead — there is no tested or extrapolatable configuration in which
this architecture, as implemented, beats Phase 1's simpler direct-thread
model on this hardware.

The code is **kept in the tree** (`src/execution/`), fully tested,
documented, and completely inert with respect to every existing Phase 1
code path (nothing in `src/wal/` calls into it; nothing in the default
build or test suite exercises it as anything other than its own,
self-contained test module) — mirroring `PHASE1_ADR.md` ADR-14's own
precedent for the pipelining experiment: a real, correct, measured
negative result is evidence worth keeping, not a mistake to delete.

## 16. Final commit

```
a0e284b02a18cb6c6b86c8b9466d4421127567eb
```

`git log --oneline -1` at the time this line was written. Every number
in this document was measured against this exact commit (or its
immediate parent, `3af3732`, for the Phase 1 baseline in §6) — no result
here was measured against, or should be compared against, any earlier
or later commit.
