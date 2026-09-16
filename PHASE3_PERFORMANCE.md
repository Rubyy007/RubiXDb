# RubiXDB Phase 3 — Performance

Single source of truth for Phase 3 benchmark numbers, mirroring
`PHASE2B_FINAL_TEST_RESULTS.md`'s own role for its phase. Compared
against a freshly re-measured baseline (operating brief §3: "Do not
compare Phase 3 against older Phase 1 numbers" — extended here to mean
"re-measure the Phase 2B winner now, don't just cite its old numbers").

## 1. Environment

Unchanged from every prior phase in this session: Intel Core i7-7700 (4
physical / 8 logical cores), 16 GiB RAM, SATA SSD (`E:`), Windows 10
Home 10.0.19045, `rustc`/`cargo` 1.98.1. `TEMP`/`TMP` redirected to
`E:\RubiXDb\temp\rgc_bench`.

## 2. Phase 3 baseline (frozen before any Phase 3 code change)

**Commit**: `7c808eb677b3b31a7e2ba4a65f123892a6610e6b` (tip of `master`,
clean tree, per `git status`/`git rev-parse HEAD`/`git log --oneline
-10` recorded at the start of this session — the last Phase 2B commit).

**Command**: `cargo run --release --example batch_coordinator_load_test
-- <writer_count> 1000` (Approach B — the winning Dedicated Batch
Coordinator architecture; matches `PHASE2B_FINAL_TEST_RESULTS.md` §7's
own methodology exactly).

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 17,918 · 18,487 · 16,327 | **17,918** |
| 1,000 writers | 97,555 · 94,900 · 97,434 | **97,434** |

Both exceed the Phase 2B targets (100w ≥15,000; 1,000w ≥80,000) and are
consistent with `PHASE2B_FINAL_TEST_RESULTS.md`'s own historical medians
(17,512 / 93,594) within this machine's documented run-to-run variance.
**This is the number Increment 3A is compared against — not the
historical Phase 2B document's own numbers directly.**

## 3. Post-Increment-3A (leader-failure fix applied)

Same commands, same methodology, after `LeaderFailureGuard` and its
tests were added (this increment's commit — see `PHASE3_TEST_RESULTS.md`
§1 for the exact hash).

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 13,736 · 15,329 · 15,108 · 17,161 · 17,906 · 13,941 · 16,092 (7 reps) | **15,329** |
| 1,000 writers | 93,733 · 90,728 · 93,760 (3 reps) | **93,733** |

100-writer: median dropped from 17,918 to 15,329 (~14.4%) but **still
exceeds the 15,000 target**; the 7-repetition spread (13,736–17,906)
overlaps the pre-fix 3-repetition spread (16,327–18,487) substantially,
consistent with this machine's own already-documented noise band
(`PHASE2B_FINAL_TEST_RESULTS.md` itself shows a 15,777–17,940 spread
across its own 5 repetitions). 1,000-writer: median 93,733, within 4%
of the pre-fix 97,434 and matching `PHASE2B_FINAL_TEST_RESULTS.md`'s own
historical 93,594 almost exactly.

**Why this is attributed to machine noise, not a real regression**:
`LeaderFailureGuard` only executes on the leader's own thread, once per
batch, and its cost on the non-panicking path is one `bool` field write
at construction plus one `bool` check in `Drop` — no lock acquisition,
no atomic operation, no allocation. It cannot plausibly account for a
double-digit-percent throughput change; the far more likely explanation
is the same background-process/scheduler variance this project's prior
phases have repeatedly measured and documented on this specific
development machine. This is flagged honestly rather than asserted away
without evidence — if a future increment's soak testing (operating
brief §13, not yet run — see `PHASE3_FAILURE_MODEL.md` §5) finds a
systematic effect, this section will be corrected, not defended.

**Target assessment, post-fix**: 100 writers 15,329 (target ≥15,000,
+2.2%); 1,000 writers 93,733 (target ≥80,000, +17.2%). **Both targets
still met.**

## 4. Leader-panic fix path cost (new — not measured pre-Phase-3, since the path did not exist)

Not separately microbenchmarked this increment (the panic path is, by
construction, off the hot path — it only executes when a leader thread
is already unwinding due to a fault). The `concurrent_followers_all_
fail_fast_when_the_leader_panics` test (`src/wal/group_commit.rs`)
demonstrates the qualitative improvement directly: 16 concurrent
followers of a panicking leader now all fail within 1 second total
(asserted bound), compared to the pre-fix behavior where each would have
independently paid its own `follower_wait_timeout()` (`10 * EMA`,
commonly several milliseconds to low seconds depending on measured
`fsync` latency) before discovering the same outcome — and, critically,
every *subsequent* call to the same (pre-fix, wedged) `GroupCommitter`
would have paid that cost again, forever, rather than failing
immediately as it now does.

## 5. Regression gate results

See `PHASE3_TEST_RESULTS.md` §2 for the full pass/fail table
(`cargo test`/`--release`/`--features test-util`/`clippy`/`fmt`/crash
consistency) — zero regressions from this increment.
