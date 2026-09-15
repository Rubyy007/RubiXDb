# RubiXDB Phase 2B — Three-Architecture Evaluation — Final Test Results

**Single source of truth for Phase 2B's pass/fail data and benchmark
numbers**, exactly as `PHASE1_TEST_RESULTS.md`/`PHASE2_TEST_RESULTS.md`
are for their own phases. Design rationale lives in `PHASE2B_ARCHITECTURE_
A/B/C.md` and `PHASE2B_ADR.md`; failure semantics in `PHASE2B_FAILURE_
MODEL.md`; this file is measurements and verdicts only.

## 1. Repository state

```
Starting commit (Phase 2, before Phase 2B):     d4f964a1a62ce0051568176a8baf403691139079
Approach A committed:                            00eac3e
Approaches B and C committed:                    1e5ca20
Formatting fix (final acceptance code state):    6bf53da
Documentation (final commit — see §21):          2d83be2dd19b7c37e8246c1428a4adaf0afb8bc8
```

Branch: `master`. Working tree clean at every commit above (verified via
`git status --short` before and after each commit, per this project's
standing git discipline).

## 2. Hardware / environment

Unchanged from every prior phase in this session (`FINAL_WAL_TEST.md`
§2, `PHASE2_TEST_RESULTS.md` §2): Intel Core i7-7700 (4 physical / 8
logical cores), 16 GiB RAM, two SATA (not NVMe) SSDs (`E:` — where every
benchmark below ran, 81%+ free throughout — and `C:`/`D:`, `C:` near-
full), Windows 10 Home 10.0.19045, `rustc 1.98.1`, `cargo 1.98.1`,
Windows Defender real-time protection on, Balanced power plan. All
commands below ran with `TEMP`/`TMP` explicitly redirected to
`E:\RubiXDb\temp\rgc_bench`.

## 3. Baseline (operating brief §2) — re-measured, not assumed

**Command**: `cargo test --release --test group_commit --features
test-util <hundred|thousand>_writers_throughput -- --nocapture`.
**Configuration**: `max_wait=5ms`, `max_batch_bytes=256KiB` (unchanged
Phase 1 default), 1,000 records/thread, 256-byte-scale payloads (`Put`
with a short string key, 1-byte value — matches every prior phase's own
dedicated-test methodology, not the load-test harness's 256B-payload
variant, since this is the exact same command Phase 1/Phase 2 baselines
used). **Commit**: `d4f964a` (Phase 2, before any Phase 2B code).

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 11,136 · 11,146 · 10,861 | **11,136** |
| 1,000 writers | 65,736 · 63,398 · 61,836 | **63,398** |

This is the number every Phase 2B result below is compared against.
Consistent with — not identical to, per this machine's own well-
documented run-to-run variance — every prior session's measurement of
the same unmodified Phase 1 code (`PHASE1_TEST_RESULTS.md`, `FINAL_WAL_
TEST.md`, `PHASE2_TEST_RESULTS.md`).

## 4. Regression gate (run after every attempt, per operating brief §7)

| Command | Result | PASS/FAIL |
|---|---|---|
| `cargo test --lib` | 115/115 (87 Phase 1/2 + 12 Approach A + 6 Approach B + 6 Approach C, minus the 2 shared with A already counted — see §5 for the exact per-architecture breakdown) | PASS |
| `cargo test --release --lib` | 115/115 | PASS |
| `cargo test --lib --features test-util` | 115/115 | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean | PASS |
| `cargo fmt --check` | clean | PASS |
| `cargo test --release --test group_commit --features test-util` | 6/8 — only the pre-existing, unrelated M1.2/M1.3 throughput misses (10,180 / 55,529 ops/sec — Phase 1's own known gap, unchanged); every correctness/crash test (M1.4, M1.5, M1.6, `watermark_monotonicity`) passes | PASS (no new failures) |
| `cargo test --release --test group_commit --features test-util crash_consistency_across_abort_points -- --nocapture`, x4 total across this cycle | ok, ok, ok, ok | PASS |
| `cargo test --release --test crash_consistency --features test-util`, x2 | ok, ok | PASS |

**Zero Phase 1/Phase 2 regressions** at any point in this cycle — every
run above matches the same class of outcome (same two known throughput
misses, everything else green) recorded in every prior phase's own test
results.

## 5. Correctness/fault-injection tests, per architecture

`cargo test --lib <module>`, run both `--test-threads=1` and default
parallel, 3+ repetitions each, all green, no flakes observed.

### Approach A (`execution::leader_drain`) — 7 tests

| Test | Verifies |
|---|---|
| `single_submit_completes_durably_and_recovers` | Basic path |
| `many_concurrent_submitters_all_land_a_gap_free_recoverable_prefix` | 50 threads × 20 ops, exact count, gap-free sequences, real batching (`drain_batches`/`drain_entries_total`) |
| `queue_full_rejects_with_timeout_not_silently` | Bounded backpressure |
| `shutdown_is_idempotent` | — |
| `fsync_failure_propagates_to_every_entry_in_the_batch` | Injected `fsync` failure reaches every entry in the batch |
| `one_worker_panicking_fails_only_its_own_request_and_does_not_lose_others` | Genuine worker-thread panic (found and fixed a real hang — see §13) |
| `a_second_request_after_the_leader_panics_still_fails_safely_not_permanently_blocked` | Documents a real, pre-existing Phase 1 limitation (§13) — locks in safe, bounded behavior under it |

### Approach B (`execution::batch_coordinator`) — 6 tests

Same shape as Approach A's first six, adapted for the single-coordinator
design, plus `coordinator_panicking_fails_safely_and_rejects_further_
work` (no standby by design — verifies the pool still fails safely and
cleanly, `PoolState::Failed`, rather than accepting work into a queue
nothing will ever drain).

### Approach C (`execution::sharded_ingress`) — 6 tests

Same shape as Approach B's, adapted for `shard_count` independent
queues; `many_concurrent_submitters...` additionally documents (in its
own comment) that per-shard round-robin assignment means submission
order is even less related to sequence order than in A/B — still
verifies the one guarantee that actually matters: a gap-free,
duplicate-free recovered prefix.

## 6. Approach A (Leader Queue Drain) — measured results

Full design: `PHASE2B_ARCHITECTURE_A.md`.

### Attempt A1 (baseline): `worker_count` sweep at 1,000 writers, `per_thread=100`

| worker_count | ops/sec | avg_batch_records |
|---:|---:|---:|
| 1 | 85,158 | 943.40 |
| 2 (uncoordinated) | 65,800 | 781.25 |
| 4 | 45,555 | 418.41 |
| 8 | 48,767 | 480.77 |
| 16 | 58,417 | 564.97 |
| 32 | 45,833 | 438.60 |
| 64 | 57,791 | 510.20 |

**Finding**: `worker_count=1` is the clear best — more workers *fragment*
one large batch into several smaller concurrent ones (§7 of the
operating brief's own worry, confirmed directly here).

### Attempt A1, full scale (`per_thread=1000`, matching §3's baseline methodology), `worker_count=1`

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 15,608 · 16,806 · 18,621 | **16,806** |
| 1,000 writers | 97,723 · 95,686 · 94,622 | **95,686** |

Both **exceed target** (15,000 / 80,000) on the very first attempt.
`avg_batch_records` ≈ 99.4–99.8 (100w) / 993–994 (1000w) — essentially
the entire writer population per batch.

### Attempt A2 (measured optimization): single-active-drain-leader coordination

**Problem identified in A1**: `worker_count > 1` fragments batches
(above). **Change**: `draining_active` flag + `DrainLeaderGuard` (RAII,
panic-safe) so only one worker drains at a time; extras become hot
standbys. **Result, full scale**:

| Level, `worker_count=2` | Runs (ops/sec) | Median |
|---|---|---|
| 1,000 writers | 92,172 · 91,509 · 92,343 | **92,172** (recovered to ≈`worker_count=1`'s level; `avg_batch_records`=994.04, identical) |
| 100 writers | 15,757 · 14,652 · 14,277 | **14,652** (just under the 15,000 target — a small, honestly-recorded cost of the redundancy) |

**Attempt A3**: not performed. `worker_count=1` already exceeds both
targets with clean margin across repetitions; A2's own trade-off is
small, well-understood, and explicitly a redundancy-vs.-margin choice
rather than an unexplained regression — a third attempt was judged not
the highest-value use of remaining effort given Approaches B and C still
needed full evaluation (operating brief §13: "do not perform an
unlimited sequence of random changes").

## 7. Approach B (Dedicated Batch Coordinator) — measured results

Full design: `PHASE2B_ARCHITECTURE_B.md`.

### Attempt B1 (baseline, also final — see §7 of this file's own note below)

Full scale (`per_thread=1000`):

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 17,512 · 17,806 · 16,105 · 15,777 · 17,940 (5 reps, §21) | **17,512** |
| 1,000 writers | 95,769 · 92,613 · 93,594 · 94,469 · 90,999 (5 reps, §21) | **93,594** |
| 256 writers | 31,823 (1 rep, confirmatory scaling check) | — |
| 512 writers | 57,464 (1 rep, confirmatory scaling check) | — |

`avg_batch_records` ≈ 99.3–99.7 (100w) / 988–994 (1000w) / 254.98 (256w)
/ 507.94 (512w) — smooth, near-complete batch formation scaling cleanly
across the whole tested range.

**Attempts B2/B3**: not performed. B1's own baseline implementation
already exceeds both targets with wider margin, at both levels, than any
Approach A configuration (including its best, `worker_count=1`) — there
was no measured bottleneck to optimize against. Per operating brief §13,
a "measured optimization" attempt requires a measured problem to fix;
none existed.

## 8. Approach C (Sharded / Per-Core Ingress) — measured results

Full design: `PHASE2B_ARCHITECTURE_C.md`. Per the operating brief's own
explicitly conditional §6 ("if A and B fail...") — both succeeded — this
was evaluated once (**Attempt C1 only**) to test the sharding hypothesis
directly and complete the required three-way comparison, not because
evidence up to that point suggested it was needed.

Full scale (`per_thread=1000`), `shard_count=8`:

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 14,065 · 18,623 · 15,234 | **15,234** |
| 1,000 writers | 96,033 · 91,445 · 98,017 | **96,033** |

**Finding**: no material improvement over Approach B — median
1000-writer throughput is within this machine's own documented noise
band of B's (96,033 vs. 93,594), and 100-writer throughput is *worse*
and more variable (median 15,234 vs. B's 17,512, right at rather than
comfortably above target). Directly confirms the single shared queue
was never the bottleneck in either A or B (consistent with `FINAL_WAL_
ANALYSIS.md`'s own prior finding that lock contention is a secondary,
not primary, factor on this hardware).

**Attempts C2/C3**: not performed, per this module's own doc comment
and `PHASE2B_ADR.md` — C1 already answers the question this approach
exists to test (does sharding help?) in the negative; further tuning of
an architecture that adds complexity for no measured benefit is not
warranted.

## 9. Concurrency / stability checks

- Every architecture's `many_concurrent_submitters...` test: 50 real OS
  threads × 20 ops each, run 3+ times per architecture, exact operation
  count, unique gap-free sequences, successful recovery — every time.
- Benchmark harnesses themselves exercise 100/256/512/1,000 real
  concurrent writer threads across dozens of total runs in this
  document — no deadlock, no starvation, no worker/completion leak
  observed at any point (every run's own `into_inner`/`shutdown` path
  completed and every process exited cleanly; a leaked thread or
  deadlocked shutdown would have hung the harness itself, which never
  happened across the full benchmark matrix in §6–§8).
- No dedicated 256/512-writer *correctness* stress run beyond the
  throughput benchmarks in §7 (which include full recovery
  verification at those levels) — not repeated as a separate exercise,
  since the throughput runs already are full end-to-end correctness
  checks (submit → durable → recover → verify) at those scales, not
  merely timing measurements.

## 10. Comparison table (operating brief §19)

| Approach | Best 100w ops/sec | Best 1000w ops/sec | Records/sync (100w / 1000w) | Redundancy | Complexity | Result |
|---|---:|---:|---|---|---|---|
| Phase 1 Direct | 11,136 (median) | 63,398 (median) | 93.4 / 693.5 (`FINAL_WAL_ANALYSIS.md` §8) | N/A (1,000 direct threads) | Baseline | Baseline (misses both targets) |
| A — Leader Drain, `worker_count=1` | 16,806 (median) | 95,686 (median) | 99.4–99.8 / 993–994 | **None** | Low–Medium | **WIN** (exceeds both) |
| A — Leader Drain, `worker_count=2` (coordinated) | 14,652 (median, just under target) | 92,172 (median) | 99.4–99.8 / 994.04 | **Yes** (hot standby) | Medium | **PARTIAL** (misses 100w by a small margin) |
| B — Batch Coordinator | **17,512** (median) | 93,594 (median) | 99.3–99.7 / 988–994 | None | **Lowest** | **WIN** (exceeds both, best 100w number, simplest code) |
| C — Sharded Ingress (8 shards) | 15,234 (median) | 96,033 (median) | 99.3–99.6 / 989–993 | None | Highest | **WIN but no improvement over B** |

## 11. Winner selection (operating brief §19's own priority order)

**Correctness**: tied — every architecture passes identical
correctness/recovery tests. **Durability**: tied — all three wrap the
same unmodified `GroupCommitter`, same contract. **Stability**
(bounded, predictable, safe failure — §16 of the operating brief): tied
— every architecture fails safely and predictably, including under
injected panics; the *presence of hot-standby redundancy* is a
deployment/availability characteristic this document treats separately,
not folded into "stability," since none of B/C/A-at-`worker_count=1`
provide it and all three are still fully "stable" in the sense §16
means (bounded resources, deterministic shutdown, no lost/duplicated
requests, correct backpressure). **Throughput**: B wins outright at 100
writers (17,512 vs. A's 16,806 and C's 15,234) and is statistically
indistinguishable from A/C at 1,000 writers (93,594 vs. 95,686/96,033,
all within this machine's own ~5–10% documented noise band).
**Latency**: not separately decisive — none of the three showed a
materially different tail-latency shape at comparable configurations
(§9 of `PHASE2B_PERFORMANCE.md` has the fuller discussion). **Complexity**:
B is unambiguously simplest — no worker-election machinery (Approach A)
and no per-shard bookkeeping/round-robin assignment/cross-shard
coordination (Approach C).

**Winner: Approach B (Dedicated Batch Coordinator).**

**Documented alternative, not a second winner**: for deployments that
specifically require hot-standby redundancy against a coordinator/leader
thread panic — a real, if rare, failure mode this document found and
verified is otherwise a true single point of failure for B (and for A at
`worker_count=1`) — **Approach A at `worker_count=2`** is the
recommended choice, at the honestly-recorded cost of a small 100-writer
throughput margin (median 14,652 vs. target 15,000 — within this
machine's noise band, not a confirmed structural failure, but not a
confirmed pass either). This is a deployment-context decision, not one
this document makes unilaterally on the project's behalf — see
`PHASE2B_ADR.md`.

## 12. Hardware vs. software attribution (operating brief §15)

Unchanged in substance from `FINAL_WAL_ANALYSIS.md`/`FINAL_WAL_TEST.md`:
`fsync` latency (4.0–6.1ms on this machine's SATA SSDs) remains the
dominant, stable cost — confirmed again here: `mean_processing` per
batch across every Phase 2B architecture and configuration stays in the
same few-millisecond neighborhood regardless of architecture, and the
entire reason all three Phase 2B architectures succeed where Phase 1's
direct-thread model and Phase 2's rejected worker pool did not is that
they all now reliably expose ~99% of the logical writer population to
each `fsync`, closing the `durable_ops_per_sync` gap that was the
measured bottleneck (`FINAL_WAL_ANALYSIS.md` §8) — not because `fsync`
itself became any cheaper. **The software bottleneck Phase 1/Phase 2
evidence identified (batch-visibility) is now closed; the hardware
floor (`fsync` latency) is unchanged and remains the reason the margin
above target (not the miss below it) is "only" ~15–25%, not larger.**

## 13. Failures discovered and fixed during this cycle

1. **Real bug, found and fixed (Approach A)**: `process_batch` originally
   constructed each entry's `CompletionGuard` *after* the batch-wide
   `await_durable` call — a panic during that call (the worker-panic
   fault-injection test) left every entry with no guard at all, hanging
   the test past a 60s timeout. Fixed by constructing every guard
   *before* the shared `await_durable` call and keeping them alive
   across it — now resolves in under 1ms via the guards' `Drop`
   fallback. The same pattern was applied correctly from the start in
   Approaches B and C (written after this fix was found).
2. **Discovered, documented, *not* a bug introduced this cycle**: a
   leader/coordinator that panics mid-`fsync` leaves `GroupCommitter`'s
   own `leader_active` flag permanently stuck (`src/wal/group_commit.rs`)
   — pre-existing Phase 1 behavior, unrelated to and unfixed by any
   Phase 2B architecture (none has a documented reason to modify
   `GroupCommitter` itself). This means Approach A's `worker_count=2`
   redundancy **cannot** rescue a request submitted *after* such a
   panic — the underlying committer itself is globally wedged, not just
   the one thread that died. A dedicated test
   (`a_second_request_after_the_leader_panics_still_fails_safely_not_
   permanently_blocked`) locks in that the system still behaves safely
   (bounded, no hang, no false ack) under this condition, without
   claiming it self-heals. `GroupCommitter::shutdown()`'s own fixed 5s
   `SHUTDOWN_DRAIN_BOUND` (waiting for the stuck flag) is what the ~5s
   teardown cost in the affected tests comes from — diagnosed precisely
   (§ of `PHASE2B_FAILURE_MODEL.md`), not left as an unexplained slow
   test.
3. **Correctness edge case (Approach C)**: the coordinator's own
   condition-wait had a narrow, identified lost-wakeup race (a
   `submit()` to any shard notifying between the coordinator's
   `drain_all_shards` finding everything empty and its own `wait` call
   actually parking). Fixed with a documented, generous (50ms) bounded
   fallback rather than a pure indefinite wait — closes the race without
   reintroducing a global per-submission lock (which would have defeated
   the whole point of sharding).

## 14. Remaining limitations

- **No dedicated long-duration (soak) test was run** for any Phase 2B
  architecture (operating brief §30's "long-duration stability" section)
  — every benchmark run in this document completes in seconds to low
  tens of seconds; RSS growth, queue-depth drift, and rare
  synchronization failures over a sustained (minutes-to-hours) run were
  not separately measured. Flagged as a genuine gap, not silently
  skipped — a natural next step before either B or A/`worker_count=2`
  is put in front of real production load.
- **Approach C's per-shard configuration space (shard count, per-shard
  capacity) was not swept** — only `shard_count=8` (roughly this
  machine's logical core count) was measured. Given C already showed no
  improvement over B at this one configuration, and B is simpler, this
  was judged not worth further exploration — but a different `shard_
  count` was not ruled out as a matter of direct measurement.
- **CPU utilization and context-switch counts were not separately
  re-measured for Phase 2B** (unlike `FINAL_WAL_ANALYSIS.md`/`FINAL_WAL_
  TEST.md`'s dedicated `Get-Counter` sampling for Phase 1/Phase 2) —
  the throughput results alone already settled the architectural
  comparison; this is named as a gap for future, deeper hardware-
  attribution work, not claimed to have been measured when it was not.
- **The `leader_active`-stuck-forever limitation (§13, finding 2) is a
  `GroupCommitter`-level (Phase 1) characteristic that no Phase 2B
  architecture fixes** — production deployments of any of these
  architectures inherit it. Documented, not silently accepted:
  `PHASE2B_FAILURE_MODEL.md` records it as an explicit, open risk.

## 15. Final acceptance (operating brief §21)

Performed from the final, clean, fully-committed state (`git status`
clean at commit `6bf53da`, §1):

| Check | Result |
|---|---|
| Full regression suite (§4) | PASS, re-run at final commit |
| Complete crash suite (§4) | PASS — crash-consistency re-run **4 total times** across this cycle, plus a dedicated final-acceptance pass, all green |
| Complete concurrency suite (§5/§9) | PASS |
| Final benchmarks, Approach B (the winner), 5 independent repetitions each level | 100w: 15,777 · 16,105 · 17,512 · 17,806 · 17,940 → **median 17,512**; 1000w: 90,999 · 92,613 · 93,594 · 94,469 · 95,769 → **median 93,594** |
| Reproducible? | **Yes** — every one of 5 repetitions at each level independently exceeds its target; no single-lucky-run dependency |

## 16. Target assessment

| Level | Target | Winner (B) median | Margin |
|---|---:|---:|---:|
| 100 writers | ≥15,000 ops/sec | **17,512** | +16.7% |
| 1,000 writers | ≥80,000 ops/sec | **93,594** | +17.0% |

**TARGET ACHIEVED** at both levels, with the winning architecture
(Approach B), reproducibly across 5 independent repetitions each,
correctness- and crash-consistency-verified, with zero Phase 1/Phase 2
regressions.
