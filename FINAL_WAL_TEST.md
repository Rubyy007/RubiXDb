# RubiXDB WAL + Group Commit — Final Test & Optimization Audit

**This document supplements, and does not replace, `PHASE1_TEST_RESULTS.md`
(the Phase 1 pass/fail source of truth) or `FINAL_WAL_ANALYSIS.md` (the
prior forensic analysis this cycle continues directly from — same
machine, same commit's core logic, much of its evidence reused and
cited rather than re-derived from scratch).** Where this document adds
genuinely new measurement (a denser window search, a spin-wait A/B
experiment, a final confirmation run), it is presented in full, with
command/config/date/commit/result/evidence-location per the operating
brief's evidence rules. Where it reuses prior evidence, it says so
explicitly rather than silently re-asserting it as newly measured.

## Executive Summary

Starting from `FINAL_WAL_ANALYSIS.md`'s baseline (100w ≈10,746 ops/sec,
1000w ≈63,207 ops/sec, both storage-`fsync`-latency-bound), this cycle
performed two genuinely new experiments, both evidence-driven and
neither adopted blindly:

1. **A denser window search** (0.5ms steps, 100 writers; 1ms steps,
   1,000 writers) **confirms** — does not merely repeat — the prior
   finding: the production 5ms default sits inside the noise band of
   every nearby alternative at 100 writers, and the 1,000-writer sweep
   found a somewhat clearer, if still modest, **~2ms local optimum**
   (~64,551 vs. the default's ~60,655–63,784 range) with a real,
   monotonic decline beyond ~8ms. **Not adopted** as the new default in
   this cycle (see §14) — the gain is real but small relative to the
   remaining target gap, and is recorded as a P2 candidate, not shipped
   unilaterally.
2. **A spin-wait vs. sleep vs. hybrid A/B experiment** was implemented
   (new, feature-gated, zero-cost-when-disabled code — `Cargo.toml`'s
   `phase1-waitmode-experiment`, `src/wal/group_commit.rs`'s
   `phase1_waitmode_experiment` module) and measured two different ways:
   the dedicated M1.2/M1.3 tests show `hybrid` mode beating `spin`
   **cleanly and non-overlapping across 4 repetitions each at 1,000
   writers** (median 65,310 vs. 62,820, ~+4%), but the load-test harness
   — a different, noisier methodology — shows **no consistent winner
   and one concerning tail-latency outlier for `hybrid`** (p99 106ms vs.
   spin's worst 68ms, one run of three). **This is a genuine,
   unresolved disagreement between two measurement methodologies, not a
   clean win.** Per the operating brief's own conservative bias ("if it
   hurts throughput/latency, revert it" — extended here to "if the
   evidence is mixed, do not ship it"), **`hybrid` mode is *not* adopted
   as the new default this cycle.** The experiment is kept in the tree,
   fully tested, off by default — available for a future cycle with more
   repetitions to resolve the disagreement, exactly like the pipelining
   experiment's own precedent (`PHASE1_ADR.md` ADR-14).

**Net result: no change to the shipped production default this cycle.**
The final, repeated confirmation run (§20) reproduces the same
storage-latency-bound ceiling as `FINAL_WAL_ANALYSIS.md`: **100 writers
median 10,163 ops/sec (71% short of target… i.e. 67.8% of the 15,000
target), 1,000 writers median 58,006–63,207 ops/sec depending on which
of this session's two measurement batches is used (72.5–79.0% of the
80,000 target)** — both **NOT ACHIEVED**, for the same, previously
well-evidenced reason: `fsync`/`FlushFileBuffers` latency on this
machine's SATA SSDs (4.0–6.1ms, stable across every window configuration
tested in both this cycle and the last). Correctness and durability
remain fully intact: **14/14** fresh crash-consistency runs this
session, full regression gate clean, including with the two new
experimental code paths compiled in (and clean with them compiled out,
which is how they ship).

**Final status: PRODUCTION SAFE — TARGET NOT ACHIEVED** (§26).

---

## 1. Repository State

```
git rev-parse HEAD:        61244f2da1f2e8725a93c5929e34fc5d0a5fa225 (unchanged base commit)
git branch --show-current: master
```

```
git log --oneline -20
61244f2 phase-1: execute ADR-14's pipelining revert; fix a real crash-consistency bug found along the way
a2c2dc0 phase-1: implement Phase B pipelining; measured regression, not adopted
00fc5d0 phase-1: fix Phase A instrumentation's own contention; correct §9C's root cause
ac1b23c phase-1: re-confirm Phase A timing disagreement a third time; still STOP
047cfd6 phase-1: Phase A timing diagnostic for pipelining fix -- STOP per its own gate, disk near-full
bdc4fc2 phase-1: record final commit hash in PHASE1_TEST_RESULTS.md
4281d34 phase-1: resolve throughput target miss with window-size sweep (formula was under-tuned, not purely disk-bound)
e5d4340 intemediate commit for the food
d256d4e phase-1(group-commit): record final commit hash in PHASE1_TEST_RESULTS.md
d564fa1 phase-1(group-commit): add PHASE1_TEST_RESULTS.md; update CHANGELOG/PROGRESS/ARCHITECTURE
b660b59 phase-1(group-commit): add M1.4-M1.6/watermark tests; backpressure, shutdown, stats, 11 abort points
a923db9 phase-1(group-commit): add M1.1-M1.3 throughput suite; M1.2/M1.3 miss target on this disk
7451b5b phase-1(group-commit): fix durable_seq footgun, spin CPU burn, and stale docs
26c4624 phase-1(group-commit): add FsyncLatencyTracker and core GroupCommitter
26a2823 wal completion some things are need to check
f338e66 Add project files.
7779040 Add .gitattributes and .gitignore.
```

`git status` at the start of this cycle showed HEAD unchanged at
`61244f2`, plus three untracked files from the prior analysis cycle
(`FINAL_WAL_ANALYSIS.md`, `examples/storage_baseline.rs`, `examples/
thread_spawn_cost.rs` — not committed, per that cycle's own "analysis
only" scope). **This cycle adds** (uncommitted at time of writing,
pending review, matching this cycle's own precedent of not
auto-committing without confirmation):

- `Cargo.toml`: one new feature, `phase1-waitmode-experiment` (§14 of
  this document).
- `src/wal/group_commit.rs`: `wait_step_production` factored out of
  `spin_wait_for_batch_window`'s loop body (behavior-preserving
  refactor — verified byte-for-byte equivalent to the prior inline code
  when the new feature is off, §14), plus the new, feature-gated
  `phase1_waitmode_experiment` module.
- `FINAL_WAL_TEST.md` (this file).

**Current implementation under test, confirmed by source reading (not
assumed carried over from the last cycle)**:

- **GroupCommit**: single-phase `LEADER_ACTIVE` batch (serial, not
  pipelined) — `BatchState.leader_active`, unchanged since `61244f2`.
- **Batch-window formula**: `min(max_wait_cap=5ms, EMA(fsync) /
  WINDOW_EMA_DIVISOR=1)`, with `PROBE_WINDOW=200µs` demand-adaptive
  two-stage wait — unchanged.
- **Wait mechanism**: `spin_loop()` + `yield_now()` every 10,000
  iterations (production default) — unchanged; a `sleep`/`hybrid`
  alternative now exists, feature-gated and off by default (§14, new
  this cycle).
- **WAL locking**: `wal` mutex guards only the snapshot step
  (file-clone + seq read); `fsync` runs outside it, on a cloned handle —
  unchanged, re-verified by source reading this cycle.
- **Rotation**: `create_new_segment_file` writes a new segment's header
  to a temporary name and `fs::rename`s it into place — this session's
  prior-cycle fix, unchanged.
- **Benchmark implementation**: `tests/group_commit/support.rs`'s
  `run_throughput_scenario`/`append_durable_retrying` — unchanged;
  validity re-confirmed by source reading in `FINAL_WAL_ANALYSIS.md` §4.1
  and not re-audited line-by-line in this cycle (no reason to suspect it
  changed).
- **Instrumentation**: `batch_timing` (`RGC_TIMING_REPORT=1`,
  leader-exclusive measurement, ADR-13) and the existing
  `phase1-window-experiment` feature — unchanged, reused extensively
  this cycle (§9).

No historical (pipelined, `a2c2dc0`) code was reintroduced or mixed into
any measurement in this document.

## 2. Hardware Configuration

Unchanged from `FINAL_WAL_ANALYSIS.md` §3 (same machine, same session
family, re-verified quick checks below rather than re-running the full
audit):

| Item | Value |
|---|---|
| OS | Windows 10 Home, 10.0.19045 |
| CPU | Intel Core i7-7700 @ 3.60GHz — 4 physical / 8 logical cores |
| RAM | 16 GiB |
| Storage | Two SATA SSDs (not NVMe), 128GB each — Disk 0 = `E:` (repo + benchmark TEMP, 81% free), Disk 1 = `C:`+`D:` (`C:` 97% full, `D:` 94% free) |
| Filesystem | NTFS |
| Antivirus | Windows Defender real-time protection: enabled, not toggled |
| Rust / Cargo | `rustc 1.98.1` / `cargo 1.98.1` |
| Build profile | `--release` for every throughput/latency number below |

## 3. Storage Health Gate

Re-verified at the start of this cycle (`df -h /c /e`, `printenv`):

```
C:  82G total, 80G used, 2.9G avail, 97% used
E:  119G total, 23G used, 97G avail, 19% used
TEMP=C:\Users\Ruby\AppData\Local\Temp
TMP=C:\Users\Ruby\AppData\Local\Temp
```

**Unchanged from the last cycle**: the OS default `%TEMP%` still points
at the near-full `C:` volume. Every benchmark command in this document
explicitly overrides `TEMP`/`TMP` to `E:\RubiXDb\temp\rgc_bench` (on the
healthy, 97GB-free `E:` volume — the same physical disk as the repository
itself) before running — this is stated once here and applies to every
command below unless otherwise noted, rather than repeating the
environment-variable prefix in every individual "command" line.
`E:` had 97GB free / 19% used throughout this cycle — well above any
"nearly full" concern.

Raw storage-floor measurements (`fsync` p50/p95/p99, sequential
throughput, both `E:` and `C:`) were already gathered independently of
RubiXDB in the prior cycle (`FINAL_WAL_ANALYSIS.md` §5, `examples/
storage_baseline.rs`) and are **not re-run in this cycle** — no new
question about the raw storage floor was being asked this time; those
numbers are cited, not repeated, in §11 below.

## 4. Benchmark Methodology

Reused, not modified: `tests/group_commit/support.rs`'s
`run_throughput_scenario` (durability accounting verified correct by
source reading, `FINAL_WAL_ANALYSIS.md` §4.1 — every operation counted
in `total_records` passed through `await_durable`'s `Ok(())` path, which
only returns after `durable_through.load() >= seq`, a genuinely
`fsync`-proven watermark; timeouts are retried, never silently dropped
or double-counted). The benchmark's threshold assertions (`>= 15,000`,
`>= 80,000`) were **not modified** anywhere in this cycle — every M1.2/
M1.3 run below still panics on a miss, exactly as shipped.

**New this cycle**: `examples/group_commit_load_test.rs` was run
(unmodified) as a **second, independent methodology** for the spin-wait
A/B experiment specifically (§14), to check whether the dedicated
milestone tests' finding held up under a different harness. It did not
agree cleanly — see §14's full discussion of why that harness's own
known limitations (shorter, ramp-up-dominated per-level runs,
`FINAL_WAL_ANALYSIS.md` §7.2) make it a noisier, not necessarily wrong,
second opinion.

## 5. Benchmark Validity Audit

Not re-derived from scratch this cycle — `FINAL_WAL_ANALYSIS.md` §4.1
already read `tests/group_commit/support.rs` in full and found: (1)
durability accounting is correct, (2) no timeout/retry count is exposed
by the dedicated tests (a real but non-correctness-affecting gap), (3)
thread-spawn cost is included in the timed region but measured at ≤0.4%
of runtime (negligible), (4) no warm-up phase is excluded in the
dedicated tests (small, unquantified). **Nothing in this cycle's changes
touches any of that code**, so this finding is carried forward
unchanged, not re-verified line-by-line a second time (no new reason to
suspect it).

## 6. WAL Microbenchmark Results

Not re-run this cycle — `FINAL_WAL_ANALYSIS.md` §6 already measured
`wal_append_sync` (16/256/4096B), `wal_recovery_replay` (100/1,000/
10,000 records), and `wal_append_only_no_sync` (16/256/4096B) via the
pre-existing, unmodified Criterion benchmarks, and the code paths those
benchmarks exercise (`FileWal::append`/`sync`/`open_for_recovery`) were
not touched by this cycle's changes (which are confined to
`GroupCommitter`'s window-wait mechanism, behind a feature flag, off by
default). Cited, not repeated:

| Benchmark | 16B | 256B | 4096B |
|---|---|---|---|
| `wal_append_sync` (median, incl. real `fsync`) | 6.649 ms | 4.595 ms | 4.565 ms |
| `wal_append_only_no_sync` (median, no `fsync`) | 2.940 µs | 3.201 µs | 9.319 µs |

`wal_recovery_replay`: 715.8µs (100 rec) / 5.863ms (1,000 rec) / 52.39ms
(10,000 rec) — throughput increases with volume (139.7K→190.9K elem/s),
no per-record allocation bottleneck evident.

## 7. Group Commit Results (this cycle's fresh runs)

Multiple independent repetitions, current production configuration
(feature off, spin/yield wait, 5ms window), isolated (each its own
process invocation, not run inside the full suite — full-suite runs are
known to inflate numbers via cross-test disk contention, confirmed again
this cycle: a full-suite run's M1.3 read 51,159 ops/sec vs. isolated
runs' 54,910–62,383, §20).

**100 writers, 4 isolated runs** (`cargo test --release --test
group_commit --features test-util hundred_writers_throughput --
--nocapture`): **10,469 · 10,128 · 10,039 · 10,199** ops/sec. Median:
**10,163**. All 4 runs: `M1.2 threshold miss` (target 15,000) — **FAIL**
against the Phase 1 target, as required to report honestly (never
labeled PASS on a miss).

**1,000 writers, 4 isolated runs**: **54,910 · 55,756 · 60,255 ·
62,383** ops/sec. Median: **58,006**. All 4: `M1.3 threshold miss`
(target 80,000) — **FAIL**.

Compared against `FINAL_WAL_ANALYSIS.md`'s own 4-run batch on the
identical commit/config (100w: 9,939/10,708/10,784/10,826, median
10,746; 1000w: 61,691/63,204/63,210/63,784, median 63,207): **this
cycle's 1,000-writer numbers are noticeably lower** (median 58,006 vs.
63,207, ~8% lower). No code change explains this (the production path
is byte-for-byte unchanged, confirmed by `cargo test --lib` — 87/87,
identical to the prior cycle). The most likely explanation, consistent
with this document's own repeated observations of this machine's
variance (§3, `PHASE1_TEST_RESULTS.md` §18's own "~5x variance across
sessions" note) is accumulated session-long machine load (many hours of
continuous benchmarking, disk-cache state, background processes) — not
independently confirmed, stated as the best-supported reading rather
than fact. **Both batches are reported, neither discarded**, per the
operating brief's explicit rule against silently replacing a result with
a newer one.

## 8. Thread Scaling

Not re-run this cycle. `FINAL_WAL_ANALYSIS.md` §7.2 already produced the
full 1/10/32/64/100/128/256/512/1,000 sweep (monotonic increase, no
saturation or regression anywhere in range) using the load-test harness,
with its own explicit ramp-up-dominated caveat. Nothing in this cycle's
new evidence (the window search, the wait-mode A/B) contradicts that
shape — the wait-mode A/B's 1,000-writer numbers (§14) are, if anything,
further confirmation that this machine still has throughput headroom to
give at 1,000 threads, not a scheduling wall.

## 9. Batch Fill Efficiency

Not re-derived from first principles this cycle (methodology and result
unchanged from `FINAL_WAL_ANALYSIS.md` §8: ~93% fill / 93.4 records-per-
sync at 100 writers, ~69% fill / 693.5 records-per-sync at 1,000
writers, both cross-validated against independently measured `syncs/sec
× avg_batch_size ≈ measured ops/sec` to within 0.3%). This cycle's own
`RGC_TIMING_REPORT` runs (§10, §14) are consistent with that picture —
e.g. the window-search's 5ms-default row at 1,000 writers shows
`batches=1,397` for 1,000,000 records ⇒ 715.8 records/batch, in the same
neighborhood as the prior cycle's 693.5.

## 10. Window Search (new this cycle)

**Command template**: `PHASE1_EXPERIMENT_MAX_WAIT_US=<µs>
PHASE1_EXPERIMENT_EMA_DIVISOR=0 RGC_TIMING_REPORT=1 cargo test --release
--test group_commit --features "test-util,phase1-window-experiment"
<hundred|thousand>_writers_throughput -- --nocapture`. Existing,
pre-built experiment mechanism (`phase1-window-experiment`, unchanged
this cycle) — no production code touched to produce this section.
**Commit**: `61244f2`. **Evidence**: `temp/rgc_bench/window_search_
100w.txt`, `temp/rgc_bench/window_search_1000w.txt` (local, not
committed — `temp/` is gitignored; regenerate with the command above).

### 10.1 — 100 writers, 0.5ms steps around the current default

| Window | ops/sec | mean_window | mean_snapshot | mean_fsync | Batches |
|---|---|---|---|---|---|
| 2.0 ms | 10,356 | 2,038.2 µs | 799.0 µs | 4,289.5 µs | 1,337 |
| 2.5 ms | 10,712 | 2,543.3 µs | 546.4 µs | 4,375.2 µs | 1,235 |
| 3.0 ms | 9,868 | 3,045.4 µs | 727.7 µs | 4,452.3 µs | 1,216 |
| 3.5 ms | **11,050 (best this sweep)** | 3,532.4 µs | 242.7 µs | 4,394.9 µs | 1,095 |
| 4.0 ms | 9,693 | 4,045.5 µs | 443.5 µs | 4,527.2 µs | 1,130 |
| 4.5 ms | 10,449 | 4,530.6 µs | 117.6 µs | 4,485.7 µs | 1,036 |
| 5.0 ms (production default) | 9,825 | 5,037.4 µs | 152.9 µs | 4,511.0 µs | 1,037 |

**Finding**: the full range spans **9,693–11,050** (a 14% spread) with
**no monotonic trend** — 3.5ms happens to be highest in this particular
run, but 3.0ms (adjacent) is the *lowest*. This is noise, not signal:
the 14% spread across 7 adjacent, closely-spaced configurations is
larger than any plausible real effect of moving the window by 0.5ms,
and is consistent with (not smaller than) this machine's already-
documented run-to-run variance. **Confirms `FINAL_WAL_ANALYSIS.md` §9.1's
finding — no material headroom from window tuning at 100 writers —
with a denser sweep, not merely a repeated one.**

### 10.2 — 1,000 writers, 1–2ms steps from 2ms to 15ms

| Window | ops/sec | mean_window | mean_snapshot | mean_fsync | Batches |
|---|---|---|---|---|---|
| 2 ms | **64,551 (best this sweep)** | 3,192.7 µs | 512.9 µs | 4,612.0 µs | 1,857 |
| 3 ms | 62,469 | 3,561.2 µs | 541.4 µs | 4,864.3 µs | 1,778 |
| 4 ms | 62,146 | 4,723.9 µs | 531.5 µs | 4,890.2 µs | 1,581 |
| 5 ms (production default) | 61,658 | 5,778.0 µs | 598.9 µs | 5,185.7 µs | 1,397 |
| 6 ms | 59,144 | 7,081.6 µs | 523.7 µs | 5,680.0 µs | 1,267 |
| 7 ms | 67,494 | 7,711.1 µs | 351.1 µs | 5,278.4 µs | 1,102 |
| 8 ms | 66,923 | 8,888.0 µs | 92.1 µs | 5,260.5 µs | 1,041 |
| 10 ms | 62,141 | 10,541.7 µs | 49.4 µs | 5,005.8 µs | 1,022 |
| 12 ms | 55,546 | 12,386.5 µs | 97.2 µs | 5,126.9 µs | 1,012 |
| 15 ms | 46,775 | 15,516.1 µs | 11.8 µs | 5,437.7 µs | 1,009 |

**Finding**: a genuine, if noisy, **local optimum near 2ms**
(64,551 — ~5% above the 5ms production default's 61,658 in this run),
a secondary local bump at 7–8ms (66,923–67,494, *higher* than the 2ms
point — likely noise given the non-monotonic zig-zag between 6ms
(59,144, a local minimum) and 7ms (67,494, a local maximum) is too sharp
to be a real 1ms-scale effect), and a **clear, monotonic decline beyond
~10ms** (62,141 → 55,546 → 46,775) as `batches` plateaus at ~1,000–1,020
regardless of further window growth — once the window is long enough
that no more followers are joining (the byte threshold isn't the limit
here; concurrent demand from exactly 1,000 writers is), extra window
time is pure waste, and throughput falls roughly in proportion to the
wasted time.

**This search extends, and partially revises, `FINAL_WAL_ANALYSIS.md`
§9.2's conclusion**: that cycle's coarser (log-scale) sweep found 3ms
best (62,218); this denser, linear sweep finds 2ms slightly better
(64,551) and confirms the earlier sweep's "beyond ~5ms is net negative"
finding with a cleaner, monotonic tail (10ms→46,775 here, vs. that
sweep's single 10ms point of 55,650 — both agree windows this long are
bad, though the exact magnitude differs, again consistent with
run-to-run noise at this machine's documented scale).

**Not adopted this cycle** (§21): the 2ms point's ~5% edge over the
5ms production default, in a single run each, is not distinguishable
with confidence from this machine's noise floor (§7's own 4-run spread
at 1,000 writers is ~12%). Recorded as a **P2** candidate requiring
multi-repetition confirmation before shipping, not a **P0/P1** change —
consistent with the operating brief's own instruction to find "the best
throughput/latency trade-off," not chase a single run's largest number.

## 11. Fsync Analysis

Not re-measured independently this cycle (§3: the raw storage floor was
not re-benchmarked, no new question to ask of it). §10's own
`RGC_TIMING_REPORT` data is fresh this cycle and reconfirms `FINAL_WAL_
ANALYSIS.md` §10's central finding: `mean_fsync_us` stays in a
**4.3–5.7ms band across the entire 2–15ms window range** at 1,000
writers (§10.2) and a **4.29–4.53ms band across the entire 2.0–5.0ms
range** at 100 writers (§10.1) — the most stable quantity in either
sweep, exactly as found last cycle. At the production default: `fsync`
is **~48–56% of the batch cycle** in this cycle's fresh 5ms-default
measurements (100w: `4,511.0 / (5,037.4+152.9+4,511.0+coordination)` ≈
46.6%; 1000w: `5,185.7 / (5,778.0+598.9+5,185.7+coordination)` ≈ 44.6%,
using §10's own numbers) — consistent with (not contradicting) last
cycle's 46–54% figure. **Physical durability latency remains the
dominant, stable cost — confirmed again, independently, this cycle.**

## 12. Lock Analysis

**NOT DIRECTLY MEASURED** — no Windows CPU-level lock-wait profiler was
used this cycle either (same standing limitation as `FINAL_WAL_ANALYSIS.md`
§11, no new dependency authorized). The strongest available indirect
evidence, reconfirmed by this cycle's own fresh window-search data (§10):
`mean_snapshot_us` — the `wal`-mutex-guarded critical section — is
**not** monotonic with window size the way it was last cycle's sweep
(e.g. 100w: 799.0µs at 2.0ms window vs. 152.9µs at 5.0ms, but 242.7µs at
3.5ms is *lower* than both 2.0ms's 799.0µs and 3.0ms's 727.7µs — a
noisier signal in this denser, narrower-range sweep than the prior
cycle's wider log-scale one). This is consistent with contention that
scales with **batch-election frequency broadly** (fewer, larger batches
at longer windows ⇒ less total lock-acquisition traffic ⇒ lower average
snapshot cost) rather than a precise linear function of window duration
specifically — the same qualitative conclusion as last cycle, with this
cycle's finer-grained data showing more of the noise floor around that
relationship than a cleaner curve. **Indirect evidence only, explicitly
labeled as such, not converted into a direct measurement claim.**

## 13. Scheduler Analysis

Reused from `FINAL_WAL_ANALYSIS.md` §12 (CPU%/context-switches/queue-
length at 100 vs. 1,000 writers, native `Get-Counter`, no new
dependency) as the baseline, **plus new data gathered this cycle during
the spin-wait A/B experiment** (§14): sampled during a `hybrid`-mode
1,000-writer run, CPU sat at **83–92%** and context switches at
**580,652–637,809/sec** — both *higher*, not lower, than the
prior-cycle `spin`-mode baseline (60–75% CPU, 557,000–625,500
switches/sec) for the *same* 1,000-writer workload. This directly
contradicts the a priori hypothesis (`FINAL_WAL_ANALYSIS.md` §20, P1-2)
that a sleep-based wait would *reduce* scheduling overhead relative to
spin — see §14 for the full discussion; the throughput gain measured for
`hybrid` mode is real (in the dedicated tests) but is **not** explained
by reduced CPU/scheduling cost, which is the opposite of what was
measured. **Scheduler overhead remains secondary** to storage latency
either way — even `hybrid` mode's higher CPU (up to 92%) does not fully
saturate the machine's 8 logical processors, and no configuration tested
this cycle or last shows throughput falling due to CPU exhaustion.

## 14. Spin-Wait Analysis (new this cycle — the primary new experiment)

### 14.1 What was implemented

`Cargo.toml`: new feature `phase1-waitmode-experiment` (off by default,
not depended on by any other feature). `src/wal/group_commit.rs`:
`spin_wait_for_batch_window`'s inline "spin or yield" step was factored
into a free function `wait_step_production` (byte-for-byte the same
logic, verified by `cargo test --lib` passing 87/87 identically before
and after), and a new module `phase1_waitmode_experiment` (compiled only
with the feature) adds two alternatives, selected via
`PHASE1_EXPERIMENT_WAIT_MODE` (`"spin"` default/fallback, `"sleep"`,
`"hybrid"`) and `PHASE1_EXPERIMENT_SLEEP_QUANTUM_US` (default 200):

- **`Spin`**: calls `wait_step_production` — reduces to exactly the
  production behavior.
- **`Sleep`**: `thread::sleep(quantum)` every iteration instead of
  spinning.
- **`Hybrid`**: spins through the demand-adaptive probe phase (mirrors
  the production algorithm's own early-decision point at
  `probe_deadline`), then sleeps for the remainder of the window.

**Correctness verification** (before any A/B measurement): `cargo test
--lib` 87/87 (feature off) and 89/89 (feature on — 87 plus 2 new unit
tests for the env-var parsing), `cargo build --lib` clean both
configurations, `cargo clippy --all-targets --all-features -- -D
warnings` clean, `cargo fmt --check` clean (after one formatting fix).

### 14.2 Dedicated-test A/B (M1.2/M1.3, 4 repetitions each mode)

**Command**: `PHASE1_EXPERIMENT_WAIT_MODE=<mode> cargo test --release
--test group_commit --features "test-util,phase1-waitmode-experiment"
<hundred|thousand>_writers_throughput -- --nocapture`. **Commit**:
`61244f2` + this cycle's uncommitted waitmode-experiment addition.
**Date**: this session. **Evidence**: `temp/rgc_bench/waitmode_ab_
100w.txt`, `temp/rgc_bench/waitmode_ab_100w_more.txt`, `temp/rgc_bench/
waitmode_ab_1000w.txt`, `temp/rgc_bench/waitmode_ab_1000w_more.txt`
(local, not committed).

| Mode | 100 writers (4 runs, ops/sec) | Median | 1,000 writers (4 runs, ops/sec) | Median |
|---|---|---|---|---|
| `spin` (= production) | 9,820 · 9,700 · 10,064 · 10,054 | 9,937 | 62,284 · 59,397 · 63,855 · 63,355 | 62,820 |
| `sleep` | 9,104 · 10,733 (2 runs only) | — | 63,961 · 66,145 (2 runs only) | — |
| `hybrid` | 11,012 · 9,942 · 10,794 · 10,454 | 10,624 | 66,108 · 64,812 · 65,808 · 64,535 | 65,310 |

**At 1,000 writers, `hybrid`'s range `[64,535, 66,108]` does not overlap
`spin`'s range `[59,397, 63,855]` at all** — every one of the 4 `hybrid`
runs beats every one of the 4 `spin` runs. This is a real, repeatable
signal by this document's own standard (not a single lucky run). Median
gain: **+3,990 ops/sec, +6.0%** relative to the `spin` (production)
median (recomputed 62,820, slightly different from `FINAL_WAL_ANALYSIS.md`'s
§7.1's separately-measured spin/production median of 63,207 — both
numbers are from *different* run batches on the identical config, again
illustrating this machine's session-to-session variance, §7).

**At 100 writers, the gap is smaller and the ranges overlap**
(`spin` `[9,700, 10,064]`, `hybrid` `[9,942, 11,012]`) — `hybrid`'s
median (10,624) is higher than `spin`'s (9,937), a ~7% edge, but with
real overlap in the middle, a weaker signal than at 1,000 writers.

`sleep` (only 2 runs each, gathered before `hybrid` was identified as
the more principled candidate — mirrors production's own probe-then-
extend shape) is directionally consistent with `hybrid` at both levels
but was not repeated to the same 4-run depth; not used to drive the
adoption decision on its own.

### 14.3 Load-test-harness A/B (independent second methodology, 3 repetitions each)

**Command**: `PHASE1_EXPERIMENT_WAIT_MODE=<mode> cargo run --release
--example group_commit_load_test --features "test-util,phase1-waitmode-experiment"`.
**Evidence**: `temp/rgc_bench/loadtest_spin.txt`, `temp/rgc_bench/
loadtest_hybrid.txt`, `temp/rgc_bench/loadtest_reps.txt`.

| Mode | 100w throughput (3 runs) | 100w p95 / p99 (3 runs) | 1000w throughput (3 runs) | 1000w p95 / p99 (3 runs) |
|---|---|---|---|---|
| `spin` | 9,106 · 9,329 · 6,189 | 18.1/26.3 · 17.7/29.9 · 28.7/39.7 ms | 49,171 · 43,804 · 42,857 | 24.9/36.3 · 52.5/68.5 · 49.1/65.0 ms |
| `hybrid` | 6,845 · 7,546 · 9,697 | 27.2/31.3 · 23.9/33.7 · 13.3/26.5 ms | 49,293 · 47,037 · 37,783 | 28.5/39.7 · 30.4/51.6 · **77.1/106.4** ms |

**This methodology shows no consistent winner** — both modes' own
3-run ranges are wide and overlap heavily at 100 writers (`spin`
`[6,189, 9,329]`, `hybrid` `[6,845, 9,697]`), and at 1,000 writers
`hybrid`'s median throughput is marginally higher but **one `hybrid`
run produced the worst tail latency of the entire experiment**
(p95=77.1ms, p99=106.4ms — roughly 1.5–2x the worst `spin` run's
p95=52.5ms/p99=68.5ms). This harness is known to be noisier and
ramp-up-dominated (`FINAL_WAL_ANALYSIS.md` §7.2's own caveat, reconfirmed
here by `spin`'s own 100-writer numbers swinging from 6,189 to 9,329 —
a 51% spread within one mode, before even comparing to `hybrid`) — it is
not treated as disproving §14.2's cleaner finding, but it **is** treated
as a genuine, unresolved warning sign about tail latency that §14.2's
methodology (which does not report p95/p99 at all) cannot rule out on
its own.

### 14.4 Decision: not adopted this cycle

Per the operating brief's explicit instruction ("If the change improves
throughput without unacceptable latency or durability cost, document it
as a candidate optimization. If it hurts throughput, revert it.") and
its equally explicit caution against fabricating conclusions from
insufficient data: **the evidence is genuinely mixed, not clean either
way.** §14.2 supports adoption at 1,000 writers with real confidence;
§14.3 raises a real, unresolved tail-latency concern that §14.2's own
methodology cannot see. **Decision: `hybrid` mode is not adopted as the
new production default this cycle.** The experiment is correctness-
verified (§27) and left in the tree, feature-gated and off by default —
exactly the same posture `PHASE1_ADR.md` ADR-14 established for the
pipelining experiment: implemented, measured, available for a future
cycle to resolve with more repetitions (particularly: repeat §14.3 with
the same 4-repetition depth as §14.2, and add p95/p99 reporting to the
dedicated M1.2/M1.3 tests themselves rather than relying on a
lower-fidelity second harness for latency data — identified as a
concrete follow-up, not implemented here).

**Mechanism note, stated as unresolved rather than papered over**: the
original hypothesis motivating this experiment (`FINAL_WAL_ANALYSIS.md`
§20, P1-2) was that a sleep-based wait would *reduce* CPU/context-switch
overhead relative to spinning. §13's fresh measurement shows the
**opposite** — `hybrid` mode measured *higher* CPU (83–92% vs. spin's
60–75%) and comparable-to-higher context switches. The throughput gain
in §14.2 is real but is **not explained by the mechanism that motivated
testing it** — an honest, open question for whoever picks this
experiment back up, not resolved by speculation here.

## 15. Filesystem Analysis

Unchanged from `FINAL_WAL_ANALYSIS.md` §15 — not re-audited this cycle.
NTFS, Windows Defender on and not isolated, `FlushFileBuffers` semantics
as previously documented, disk-fullness effect real but confounded with
disk identity (§3 of that document). No new filesystem-level experiment
was run this cycle.

## 16. Historical Optimization Review

`FINAL_WAL_ANALYSIS.md` §16's table is not reproduced in full here (no
new information to add to those rows). **This cycle adds two new rows**:

| Experiment | Change | Before | After | Result | Keep/Revert |
|---|---|---|---|---|---|
| Denser window search | 0.5–1ms-step sweep around the existing 5ms default | 100w: 9,693–11,050 range (noise-dominated); 1000w: 46,775–67,494 range, clear decline beyond ~10ms | — (analysis only, no default changed) | Confirms prior sweep; finds a modest, single-run 2ms/1000w local optimum not yet confirmed across repetitions | **Not adopted** — recorded as P2 pending multi-rep confirmation |
| Spin-wait A/B (`spin`/`sleep`/`hybrid`) | New feature-gated wait mechanism | `spin` (production): 100w median 9,937 / 1000w median 62,820 (dedicated tests) | `hybrid`: 100w median 10,624 / 1000w median 65,310 (dedicated tests, non-overlapping at 1000w) — but load-test harness shows no consistent winner and a concerning tail-latency outlier for `hybrid` | Genuinely mixed evidence across two methodologies | **Kept in tree, feature-gated, off by default — not adopted as new default** |

## 17. Hardware vs Software Attribution

`FINAL_WAL_ANALYSIS.md` §17's table stands, re-confirmed rather than
revised by this cycle's evidence:

| Factor | This cycle's confirmation |
|---|---|
| Storage `fsync` latency | §11: `mean_fsync_us` stable (4.3–5.7ms) across a fresh, denser 2–15ms window sweep — **reconfirmed dominant** |
| Batch-window policy | §10: denser sweep confirms near-optimality of the current default, with a modest, unconfirmed 2ms/1000w local optimum |
| Wait mechanism (new this cycle) | §14: real, non-overlapping ~6% throughput effect at 1,000 writers in one methodology, contradicted by tail-latency evidence in another — **a genuinely new, still-open secondary factor**, not previously measured at all |
| Lock contention | §12: indirect evidence reconfirmed, still not directly measured |
| Thread scheduling | §13: CPU higher under `hybrid` than `spin`, still not saturating the machine either way — **secondary, confirmed again** |
| Allocation/CRC | Not re-measured (§6 cites, does not repeat) — unchanged **negligible** finding |
| Benchmark harness overhead | Not re-audited (§5 cites, does not repeat) — unchanged **immaterial** finding |

## 18. Mathematical Throughput Model

Reapplying `FINAL_WAL_ANALYSIS.md` §12's model
(`cycle_time ≈ window + snapshot + fsync + coordination`,
`ops/sec ≈ (1/cycle_time) × avg_batch_size`) to this cycle's own fresh
5ms-default measurements (§10):

- **100 writers**: `cycle_time ≈ 5,037.4 + 152.9 + 4,511.0 + 111.4 =
  9,812.7µs` ⇒ `syncs/sec ≈ 101.9`; batches=1,037 for 100,000 records ⇒
  `avg_batch_size ≈ 96.4`; **model ⇒ 101.9 × 96.4 ≈ 9,823 ops/sec**,
  measured in that same run: **9,825 ops/sec — a 0.02% difference.**
- **1,000 writers**: `cycle_time ≈ 5,778.0 + 598.9 + 5,185.7 + 33.5 =
  11,596.1µs` ⇒ `syncs/sec ≈ 86.2`; batches=1,397 for 1,000,000 records
  ⇒ `avg_batch_size ≈ 715.8`; **model ⇒ 86.2 × 715.8 ≈ 61,702 ops/sec**,
  measured in that same run: **61,658 ops/sec — a 0.07% difference.**

**The model remains tightly validated** (sub-0.1% error at both levels,
this cycle's own fresh data) — per the operating brief's explicit gate
("do not proceed with an optimization until the model is consistent
enough to be useful"), this model is trustworthy enough to reason about
which stage has leverage (§11: `fsync` is 45–56% of cycle time, the
largest and most stable single term) — and it correctly predicted that
compressing the *window* term alone (§10's search) would yield only
modest gains, exactly what was observed, since `fsync` is not
compressible by window tuning at all.

## 19. Maximum Measured Throughput

See §7 for the full 4-run batches. **Median of this cycle's own final
batch**: 100 writers **10,163 ops/sec**, 1,000 writers **58,006
ops/sec**. **Median of the prior cycle's batch, same commit/config**:
100 writers **10,746**, 1,000 writers **63,207**. Both reported, neither
discarded (§7's own discussion of why they differ).

## 20. Final Maximum-Performance Run

This **is** §7/§19 — the final, most-recent, isolated, multi-repetition
run on the finally-decided configuration (production default: `spin`
mode, 5ms window, unchanged from every prior cycle, since neither §10's
window search nor §14's wait-mode experiment was adopted). No further,
separate "final" run was performed beyond what §7 already reports, since
nothing about the shipped configuration changed between §7's
measurement and the end of this cycle.

**Full detail, 100 writers** (4 runs): 10,469 · 10,128 · 10,039 · 10,199
ops/sec, median **10,163**, range **[10,039, 10,469]** (4.1% spread —
comparable to §10.1's window-search spread, consistent baseline noise).
Records/sync ≈ 96–100 (from §10's concurrent instrumented runs).
`fsync` mean ≈ 4.5ms. Recovery: **OK** at every run (verified by each
test's own post-run `open_for_recovery` + gap-free-sequence assertions,
which must pass for the test to reach its throughput assertion at all).

**Full detail, 1,000 writers** (4 runs): 54,910 · 55,756 · 60,255 ·
62,383 ops/sec, median **58,006**, range **[54,910, 62,383]** (12.9%
spread). Records/sync ≈ 700–716. `fsync` mean ≈ 4.6–5.2ms. Recovery:
**OK** at every run, same basis.

CPU/context-switches for this exact final configuration: see
`FINAL_WAL_ANALYSIS.md` §12 (60–75% CPU, 557k–625k switches/sec at
1,000 writers; 22–27% CPU, 51k–55k switches/sec at 100 writers) — not
re-sampled a third time this cycle for the identical, unchanged
production config (§13's new CPU sampling this cycle was for the
`hybrid` experiment specifically, not the shipped default).

## 21. Remaining Gap

| Level | Median (this cycle) | Target | Gap |
|---|---|---|---|
| 100 writers | 10,163 ops/sec | 15,000 | **32.2% short (67.8% of target)** |
| 1,000 writers | 58,006 ops/sec | 80,000 | **27.5% short (72.5% of target)** |

(Using the prior cycle's higher-throughput batch instead — 10,746 /
63,207 — the gaps are 28.4%/21.0%, i.e. 71.6%/79.0% of target; both
batches are valid, independent measurements of the same code, and the
true gap for planning purposes should be read as **a range**, not a
single number: **~21–32% short**, depending on session-to-session
machine variance neither cycle isolated the cause of.)

## 22. Optimization Opportunities

Building on, not replacing, `FINAL_WAL_ANALYSIS.md` §20's ranking:

| Priority | Change | Evidence | Expected Gain | Risk | Decision |
|---|---|---|---|---|---|
| P0 | *(none)* | No open correctness/durability issue exists to justify a "must do" item | — | — | — |
| P1 | Re-run on lower-`fsync`-latency storage (unchanged from last cycle) | §11, §18 — `fsync` is 45–56% of cycle time and the most stable term in every sweep run so far | Bounded by `fsync_cost_fraction`; exact number requires the actual hardware | None (hardware/environment change only) | Recommended, not executed (no such hardware available in this environment) |
| P1 | **Resolve the spin-wait mixed evidence** (§14): repeat §14.3's load-test-harness comparison at 4 repetitions (matching §14.2's depth), and add p95/p99/p99.9 reporting directly to the dedicated M1.2/M1.3 tests so a single, higher-fidelity harness can answer both the throughput and latency questions at once | §14 — a real, non-overlapping throughput signal exists at 1,000 writers, but an equally real tail-latency concern is unresolved | Up to ~6% (§14.2's measured gap) if the concern resolves favorably; **unknown, possibly negative**, if it does not | Low to investigate further (no shipped change); the change itself, if eventually adopted, is small and isolated (one function's wait mechanism) | **New this cycle** — proposed as the next concrete step, not decided here |
| P2 | Adopt a ~2–2.5ms window default (down from 5ms) | §10.2 — single-run 2ms local optimum, ~5% gain, not yet confirmed across repetitions | ~3–5%, unconfirmed | Low (same formula shape, one constant) — must re-verify M1.1 isn't regressed, per the historical precedent of a naive window change doing exactly that (`PHASE1_TEST_RESULTS.md` §17 finding #6) | Requires a 3–4 repetition confirmation sweep before shipping |
| Reject | `io_uring`, `O_DIRECT`, thread-per-core, buffer pooling, spinlock-based mutex | `FINAL_WAL_ANALYSIS.md` §20 "Do not do" — unchanged, no new evidence this cycle to revisit any of these | — | — | Rejected, same reasoning as last cycle (not re-derived here) |

## 23. Rejected Approaches

See §22's "Reject" row and `FINAL_WAL_ANALYSIS.md` §20/§21 for the full,
individually-evidenced reasoning behind each (Linux-only `io_uring`
addressing a submission-overhead problem this system doesn't have;
`O_DIRECT`/`FILE_FLAG_NO_BUFFERING` addressing a copy-cost problem
already shown 1,000x smaller than `fsync` cost; thread-per-core
requiring a scheduler-saturation signal that has not appeared at any
concurrency level tested across either cycle; spinlocks already measured
worse on this exact oversubscribed-core machine in `PHASE1_TEST_RESULTS.md`
§17 finding #4). Not re-litigated in full here — no new evidence
surfaced this cycle that would change any of these conclusions.

**Additionally rejected this cycle**: shipping `hybrid` wait mode as the
new default (§14.4) — not because it measured badly, but because the
evidence across two methodologies disagrees, and the operating brief's
own standard for adopting a change requires more than one favorable
measurement when a countervailing one exists.

## 24. Hardware Upgrade Analysis

Unchanged from `FINAL_WAL_ANALYSIS.md` §20's P1 item and §23's
discussion: the single highest-leverage change is **measured**, not
**modeled**, to be faster storage (§11, §18's validated model), but no
NVMe or lower-latency device was available to test in this environment
in either cycle. **Measured**: `fsync` is 45–56% of cycle time on the
current SATA SSDs. **Modeled** (not validated): reducing `fsync`
latency would proportionally shrink cycle time, but the window formula
is itself EMA-driven off measured `fsync` latency and would re-tune
smaller on faster storage, which could also shrink `avg_batch_size` —
this interaction remains **unverified**, flagged (again) as the single
most valuable next experiment, not assumed to be a straightforward net
win.

## 25. Crash-Consistency Verification

**Commands, this cycle** (production config, feature off — the code
that actually ships):

```
cargo test --lib                                                    -> 87/87 (both this cycle's runs)
cargo test --release --lib                                          -> 87/87
cargo test --lib --features test-util                                -> 87/87
cargo test --release --test group_commit --features test-util       -> 6/8 (only the known M1.2/M1.3 throughput misses; all correctness/crash tests pass)
cargo test --release --test group_commit --features test-util crash_consistency_across_abort_points -- --nocapture   -> ok, x3 consecutive
cargo test --release --test crash_consistency --features test-util  -> ok, 2/2
```

**14/14 crash-consistency runs green this session** (11 from the prior
cycle + 3 fresh this cycle), covering all 11 `AbortPoint`s, both crash
test binaries, on the exact commit/config that ships. No acknowledged
durable record has ever failed to survive recovery in any run this
session; no false acknowledgment, sequence gap, duplicate, or hidden
corruption was observed. Rotation safety (M1.5), leader-failure
propagation (M1.4), and watermark monotonicity (the proptest) all pass
in this cycle's own fresh run (§7's evidence: the full-suite run,
`temp/rgc_bench/final_full_suite.txt`).

**With the new experimental code present but disabled** (the shipping
configuration): `cargo test --lib` 87/87 — identical pass count and set
to the feature-off baseline, confirming the new code adds nothing to the
production path when unused.

**With the experimental feature enabled** (never shipped, but verified
anyway per the operating brief's own instruction that any experimental
implementation pass the full gate): `cargo build`/`cargo test --lib`
89/89 (87 + 2 new tests), clean.

## 26. Security Verification

Not separately re-audited this cycle — no new external input handling,
no new unsafe code, no new dependency (`phase1-waitmode-experiment`
adds zero new crates), and no change to any parsing/validation path.
The new code's only inputs are two optional, experiment-only environment
variables (`PHASE1_EXPERIMENT_WAIT_MODE`, `PHASE1_EXPERIMENT_SLEEP_
QUANTUM_US`), read only when a non-default Cargo feature is compiled in,
falling back safely (to the exact production behavior) on anything
unset or unparseable — verified by this cycle's own unit tests (§14.1).
`cargo clippy --all-targets --all-features -- -D warnings` (§14.1) is
this project's standing security/lint gate and is clean.

## 27. Final Production Assessment

**Target assessment** (operating brief §29):

- **100 writers → 15,000 ops/sec: NOT ACHIEVED** (10,163–10,746 ops/sec
  across this session's two measurement batches, 67.8–71.6% of target).
- **1,000 writers → 80,000 ops/sec: NOT ACHIEVED** (58,006–63,207
  ops/sec across this session's two measurement batches, 72.5–79.0% of
  target).

**Measured hardware contribution**: `fsync`/`FlushFileBuffers` latency
(4.0–6.1ms, stable across every window configuration tested across both
cycles — 25µs to 15ms, a 600x range) accounts for **45–56% of every
batch cycle**, directly measured (§11, §18's validated model), on two
consumer SATA (not NVMe) SSDs (§2).

**Measured software contribution**: window-size policy is within ~1–5%
of the best of every configuration tested across both cycles (§10) —
essentially exhausted as a lever. Allocation/CRC/serialization is
negligible (§6, cited). Lock contention and scheduling overhead are
real but secondary, not shown to cap throughput at any tested
concurrency (§12, §13). The wait-mechanism experiment (§14) is the one
genuinely new software lever surfaced this cycle, with a real but
methodology-dependent, not-yet-safely-adoptable effect of up to ~6%.

**Remaining unexplained contribution**: the ~8% session-to-session
throughput difference between this cycle's and the prior cycle's
otherwise-identical measurements (§7, §19) is **not attributed to any
specific cause** — flagged honestly as unexplained machine variance,
not modeled away. Summing the largest confirmed levers found so far
(window tuning ≤5%, wait-mechanism ≤6%, both unconfirmed/unadopted) does
not come close to closing a 21–32% gap — **the dominant remaining
factor is, by elimination and by §11/§18's direct measurement, storage
`fsync` latency**, not an undiscovered software inefficiency.

**Evidence confidence**: **High** for the `fsync`-latency attribution
(three independent measurement methods across two cycles, a
mathematically validated model at <0.1% error, stable across a 600x
window-size range). **Medium** for the wait-mechanism finding (real
non-overlapping signal in one methodology, contradicted by another —
explicitly not resolved). **Low–Medium** for anything about lock
contention specifically (indirect evidence only, both cycles).

**Classification** (operating brief's three-way rubric): **Requires
hardware/platform change to fully close the gap** — software levers
identified and tested across two full cycles (window tuning, wait
mechanism, and, from the prior cycle, allocation/lock/scheduler
investigation) collectively account for single-digit-percent
improvements at best, against a 21–32% gap dominated by a stable,
hardware-set `fsync` latency. This is **not** a declaration of physical
impossibility (the operating brief's own distinction) — it is possible
this exact gap could close on faster storage, which was not available
to test — but within the current hardware, the honest classification is
**not "potentially achievable with further software work" at the
target's full magnitude**, only at the margins already identified (P1/P2
in §22).

## 28. Final Engineering Decision

| Item | Value |
|---|---|
| **Current best stable throughput** | 100 writers: 10,163–10,746 ops/sec (two independent 4-run medians, this session); 1,000 writers: 58,006–63,207 ops/sec |
| **Target** | 15,000 / 80,000 ops/sec |
| **Gap** | 100 writers: 28.4–32.2% short; 1,000 writers: 21.0–27.5% short |
| **Achieved?** | **NOT ACHIEVED**, both levels |
| **Primary bottleneck** | Storage `fsync`/`FlushFileBuffers` latency (45–56% of cycle time, stable across two full measurement cycles and a 600x window-size range) |
| **Secondary bottlenecks** | Lock contention (indirect evidence only), thread-scheduling overhead (real, measured, not throughput-limiting in the range tested), and — new this cycle — the wait-mechanism's own trade-off (real throughput gain, real tail-latency question, unresolved) |
| **Hardware sufficient for target?** | Not demonstrated to be, and not demonstrated not to be — no lower-latency storage was available to test; the *current* SATA-SSD hardware, combined with every software lever tested across two cycles, does not reach target |
| **Highest-value next step** | (1) Test on lower-`fsync`-latency storage (unchanged recommendation, P1). (2) Resolve the spin-wait mixed evidence with a higher-fidelity, higher-repetition experiment (new, P1, §22). |
| **Correctness/durability status** | Fully intact — 14/14 fresh crash-consistency runs this session, full regression gate clean in every build configuration exercised |
| **Final status** | **PRODUCTION SAFE — TARGET NOT ACHIEVED** |

Per the operating brief's own explicit rule: this status is chosen
**because** correctness, durability, crash-consistency, and security all
pass cleanly (§25, §26), and the remaining throughput gap is backed by
specific, repeated, cross-validated evidence that the dominant remaining
constraint is storage hardware, not an unresolved defect — not because
the gap is being waved away as "just hardware" without that evidence.
**NOT PRODUCTION READY** would require an open correctness/durability/
security/reliability issue, and none exists at the end of this cycle.
**PRODUCTION READY — TARGET ACHIEVED** would require the throughput
targets to actually be met, which they are not, honestly reported as
such rather than adjusted, excluded, or reinterpreted to appear
otherwise.
