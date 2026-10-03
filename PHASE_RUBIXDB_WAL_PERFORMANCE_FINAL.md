# PHASE RUBIXDB — WAL PERFORMANCE (FINAL, flat-combining build)

**Date:** 2026-10-03 · i7-7700 (4C/8T), 15.9 GB, Windows 10, **two SATA SSDs, no NVMe** (`NVME VALIDATION = HARDWARE UNAVAILABLE`). rustc 1.98.1, release builds.
Targets (unchanged): **M1.2 >= 15,000 ops/s (100 writers x 1,000 records), M1.3 >= 80,000 ops/s (1,000 writers x 1,000 records).** Tests, writer counts, record counts and thresholds were not modified.
This file supersedes the previous phase's `PHASE_RUBIXDB_WAL_PERFORMANCE.md` (preserved in git history).

## 0. Method and its pitfalls (read first)
* **Device:** all runs on `E:` (Disk 0, SATA), `TMP=TEMP=E:\waltmp`. (The M1 tests write under `%TEMP%`, which is `C:` / Disk 1 unless overridden — an earlier phase's attribution to `E:` was imprecise.)
* **Machine state matters.** The PC uses the *Balanced* power plan and idles at ~30% CPU clock; absolute numbers shifted by up to ±30% between runs minutes apart (an unexplained transient "fast regime" reached ~21k at M1.2 for a while, then vanished). Therefore **no conclusion rests on sequential or single runs**: every comparison below is *strictly interleaved* (A,B,A,B...) with a 3-second all-core CPU warm-up before each run (`scripts/cpu_warm.py`) — plus a separate **cold** condition (20 s idle before each run). I did not change the user's power plan.
* **Never the best run:** min / median / max are reported; pass counts are over *every* run; slow runs were not dropped.
* **Isolation:** `--test-threads=1`, one test at a time, single process. (The default concurrent harness is a different — and failing — condition, §7.)
* Instrumentation: `RGC_TIMING_REPORT` (`test-util`), process CPU/RSS/threads/handles per second, `PhysicalDisk` counters (`scripts/wal_bench_runner.ps1`); raw data in `scratch/wal_resolution/`.

## 1. Baseline — unmodified WAL (Rule 3)
Measured interleaved with the final design (final A/B below). E:, isolated, release:
| | warm (6) | cold (3) |
|---|---|---|
| M1.2 | 10,238 / **10,456** / 10,731 | 10,496 / 10,499 / 10,681 |
| M1.3 | 61,680 / **62,700** / 67,592 | 60,594 / 63,438 / 64,149 |
Per-stage (previous phase, E:, 5 runs): window 4.2-4.3 ms, fsync 4.3-4.4 ms, snapshot 0.15-0.23 ms, coordination ~0.1 ms, **94.6 records/fsync**, disk busy 5-8%, queue length 0.05-0.08, CPU 12-13 s (M1.2) / 80-88 s (M1.3), threads 105 / 1,005, handles 154-160 / 1,055-1,061, RSS <= 13 / 88 MB. Ladder baselines (commit latency) are in §4.

## 2. Final design vs baseline — M1.2 / M1.3 (final code, strictly interleaved)
| Condition / test | baseline min / med / max | **final design min / med / max** | passes (final) |
|---|---|---|---|
| warm M1.2 (6 runs) | 10,238 / 10,456 / 10,731 | **16,353 / 17,145 / 17,610** | **6 / 6** |
| warm M1.3 (6 runs) | 61,680 / 62,700 / 67,592 | **91,730 / 98,975 / 101,427** | **6 / 6** |
| cold M1.2 (3 runs) | 10,496 / 10,499 / 10,681 | **17,281 / 18,053 / 18,058** | **3 / 3** |
| cold M1.3 (3 runs) | 60,594 / 63,438 / 64,149 | **98,555 / 100,045 / 103,118** | **3 / 3** |
**18 of 18 runs pass.** Worst-run margins: M1.2 +9.0% (16,353), M1.3 +14.7% (91,730). Canonical isolated reruns of the exact test commands on the final source before the last constant change: 3/3 (M1.2 17.7-18.7k, M1.3 87.5-96.7k).
**Honest history of the margin:** an earlier iteration of this design passed only marginally (M1.2 min 15,036; M1.3 1 failing run in 12 at 76,380). The margin came from two *measured* corrections, not from tuning to the benchmark: (1) the lone-writer probe was closing ~49% of batches prematurely (~70 of 100 records); restricting it to the lone-writer regime made batches full (see implementation doc §2.3); (2) the cohort target created a product-level regression (§5) fixed by a bounded straggler fallback (multiplier 4).

### Resources (medians; baseline → final)
| | M1.2 | M1.3 |
|---|---|---|
| process CPU | 12.8 s → **10.3 s** (warm), 13.5 → **8.7 s** (cold) | 82.6 → **65.5 s** (warm), 81.6 → **66.0 s** (cold) |
| records / fsync | 95 → 88 | 666 → 746 |
| threads | 105 → 105 | 1,005 → 1,005 |
| handles (max) | 154 → 160 | 1,055 → 1,055 |
| RSS (max) | 12 → 8 MB | 78 → 89 MB |
| disk busy (median) | 10.2% → 16.4% | 6.5% → 7.9% |
Lower CPU: fewer write syscalls (one per run, not per record). Disk utilization is still low: the system remains latency-bound.

## 3. Why throughput rises (model validated against measurement — Rule 24)
`ops/s ≈ records_per_batch / (window + snapshot + fsync + wake)`. Baseline M1.2: 94.6 / (4.3 + 0.17 + 4.4 + 0.1 ms) = 10.5k predicted, 10.5k measured. Final design M1.2: batches of ~88-100 records, window ~0.9 ms (cohort+quiescence close instead of a fixed ~4.3 ms wait), snapshot ~0.01 ms (no lock contention from per-record writes), fsync ~4.6 ms: 95 / (0.9 + 0.01 + 4.6 + 0.2 ms) ≈ 16.6k predicted vs 17.1k measured. The gain is **not** more parallel flushing (flushes stay serialized; probe: one device) but a **shorter, fuller cycle**: records/flush stays ~constant while the idle part of the cycle shrinks.

## 4. Commit latency across concurrency (final code, warm, interleaved, 3 reps; medians)
`examples/wal_commit_latency.rs`: the M1.x workload shape with per-commit latency. Baseline vs final.
| Writers | ops/s base → **final** | p50 ms | p95 ms | p99 ms | p99.9 ms | max ms |
|---|---|---|---|---|---|---|
| 1 | 245 → 253 | 3.65 → 3.52 | 5.91 → 5.82 | 7.24 → 7.20 | 20.6 → 21.0 | 20.6 → 21.0 |
| 2 | 276 → **491** | 6.89 → **3.68** | 8.89 → 5.77 | 9.81 → 6.52 | 58.0 → 49.0 | 58.0 → 49.0 |
| 4 | 550 → **987** | 6.83 → **3.71** | 8.79 → 5.62 | 9.92 → 6.99 | 62.7 → 23.7 | 62.8 → 23.8 |
| 8 | 1,040 → **2,056** | 7.37 → **3.49** | 9.28 → 5.50 | 10.54 → 6.70 | 63.6 → 20.8 | 66.3 → 22.5 |
| 16 | 2,044 → **3,891** | 7.49 → **3.76** | 9.42 → 5.73 | 10.86 → 6.63 | 60.9 → 20.5 | 76.1 → 21.5 |
| 32 | 4,018 → **7,439** | 7.77 → **3.99** | 9.52 → 5.74 | 10.86 → 7.91 | 26.3 → 22.7 | 64.0 → 69.3 |
| 64 | 7,178 → **12,669** | 8.26 → **4.81** | 12.16 → 7.39 | 18.86 → 11.21 | 44.8 → 26.0 | 86.7 → 77.3 |
| 100 | 10,368 → **17,381** | 8.94 → **5.20** | 15.38 → 9.64 | 20.13 → 12.63 | 76.8 → 30.1 | 77.9 → 34.6 |
| 256 | 26,128 → **35,867** | 9.03 → **5.83** | 13.17 → 11.06 | 20.10 → 16.68 | 47.9 → **108.3** | 50.8 → **109.5** |
| 512 | 50,722 → **70,329** | 9.20 → **6.10** | 15.71 → 11.40 | 20.06 → 17.58 | 67.5 → **99.0** | 76.2 → **102.8** |
| 1,000 | 59,298 → **96,055** | 15.13 → **8.52** | 28.65 → 18.77 | 42.94 → 29.23 | 116.1 → 49.4 | 134.8 → 55.5 |
**Tail-latency disclosures (Rule 18, not hidden):**
* **256 and 512 writers: p99.9 and max are worse** than baseline (108/110 ms vs 48/51 at 256; 99/103 ms vs 68/76 at 512; per-rep: 256 fc p99.9 = 108, 700, 71 ms; 512 fc = 99, 91, 101 ms vs base 27-190). Cause: at these sizes the final design closes batches slightly earlier (~188 vs ~245 records/sync at 256; ~410 vs ~475 at 512), so a straggler waits an extra cycle. p50/p95/p99 still improve 34-35%/16-27%/12-17%. p99.9 ~100 ms is within the baseline's own envelope at 1,000 writers (116-135 ms).
* One 256-writer run showed a 700 ms stall (device stalls of 0.3-1.4 s occur on this SSD in raw probes and in both builds).
* 32/64 writers: max is not better (69/77 ms vs 64/87) — device-stall dominated.
Single-writer (M1.1) latency is unchanged (3.52 vs 3.65 ms p50); `m1_1_single_writer_latency_unchanged` passes.

## 5. Product-level write path (real `rubixdb gui`, real HTTP, baseline vs final)
Warm, interleaved, 3 rounds, zero errors in all runs.
| | baseline | **final** |
|---|---|---|
| same-table insert c=1 / 4 / 16 (ops/s) | 189,165,205 / 237,249,233 / 242,238,229 | 194,211,200 / 227,232,227 / 239,239,243 |
| same-table update c=1 / 4 / 16 | 205,193,194 / 230,220,230 / 228,236,241 | 188,211,200 / 228,248,237 / 245,249,244 |
| same-table txn c=1 / 4 / 16 | 175,180,181 / 262,247,259 / 238,233,248 | 183,187,168 / 267,213,262 / 248,244,242 |
| multi-table 16 writers / 16 tables (commits/s) | 2,119 / 2,116 / 1,943 | 2,068 / 2,114 / 2,125 |
| 16 writers / 4 tables | 463 / 509 / 509 | 500 / 515 / 499 |
| 8 writers / 8 tables | 973 / 1,046 / 1,028 | 1,051 / 1,018 / 940 |
**Parity** at the product level (within run noise, which for the baseline alone spans ±6%). **A regression was found and fixed during this phase:** an intermediate build with a count-only cohort target (no straggler fallback) was **30-32% slower** on multi-table writes (16w/16 tables: 1,394-1,449 vs 2,095-2,125 commits/s; 16w/4: 340-364 vs 496-509; 8w/8: 682-770 vs 1,001-1,066), reproduced over repeated interleaved rounds. Cause: product concurrency fluctuates (per-table locks), so a batch smaller than the previous one never reached its target and waited out the ~4.4 ms deadline. The M1.x tests cannot expose this (closed loop, constant cohort). Fix = bounded straggler fallback (§ implementation 2.3), chosen by sweeping the multiplier on both workloads: x2 M1.2 15.3-16.7k; **x4** M1.2 16.2-17.7k / M1.3 98-102k / product parity; x8 product -3..-7%; x16 product worse; fallback disabled product -30%.
The product's engine path (`BatchCoordinatorPool`: one coordinator appends sequentially, then one `await_durable`) does not benefit from the multi-threaded combining, and the relational per-table commit lock (separate item) still caps same-table commits at ~230-260/s. **This phase therefore does not raise product-level SQL throughput; it raises the throughput of the WAL for multi-threaded callers and removes no product capability.**

## 6. Sustained workload (Rule 36) — 64 writers, default-size rotation with purge
`examples/wal_soak.rs` + `scripts/wal_soak_monitor.ps1`: 64 writers looping `append` + `await_durable` (100-byte values) through the real `GroupCommitter`, 8 MB segments (so rotation, sealing and `purge_before` all run), process RSS/threads/handles/CPU sampled every 10 s.

**Exact throughput** (from `highest_seq` deltas per 10 s interval; the final build's run was 25 minutes):
| Run | avg ops/s | interval p5 / p50 / p95 | quarter medians | intervals < 80% of median |
|---|---|---|---|---|
| **unmodified baseline** (10 min) | **6,460** | 5,960 / 6,417 / 6,880 | 6.3k / 6.4k / 6.7k / 6.5k | 0 |
| previous build, cohort-only (40 min) | 11,076 | 9,993 / 11,046 / 12,202 | 11.5k / 11.0k / 10.9k / 10.9k | 0 |
| **final code** (25 min, 16,754,984 records) | **11,155** | 9,912 / 11,052 / 12,835 | 10.7k / 12.1k / 11.0k / 11.1k | 1 |
**+72% sustained at 64 writers** (6.5k → 11.2k), no collapse, no drift beyond about ±5% across quarters. Commit latency (final): p50 4.8-5.3 ms by quarter (baseline 9.3 ms), p99 12.4-13.4 ms (baseline 21.5-21.8 ms); worst single interval max 0.3-1.2 s in both builds (device stalls, as in the raw probes).
**Resources (final-code soak):** RSS 7.0 MB flat (max 8.1; slope +0.02 MB/min over the last 60%), threads 65-68, handles 110-112 for the whole run; previous 40-minute run: RSS 8.3 MB (max 8.9, slope -0.006 MB/min), threads 65-68, handles 117-118. No leak, no queue growth.
**WAL growth:** bounded by purging: 14-32 MB / 2-4 segments (10-minute run), 14-22 MB / 2-3 segments (second 10-minute run), baseline 8-25 MB / 1-4 segments; the earlier 40-minute run (external sampler) 16-40 MB / 2-5 segments. (The 25-minute final-code run's own WAL-size sampler started before the process and exited immediately, so that run has no size series; same code path as the 10-minute runs.)
**Integrity:** at exit `highest_seq == durable_through` in every run (26,638,277 and 16,754,984 records).

**A false alarm, retracted (Rule: report faithfully).** My first analysis of the 25-minute run reported that 29% of intervals ran at about half throughput ("a degraded two-cohort mode") and I built and tested a mechanism (a decaying high-water cohort target) to cure it. A control soak of the *unmodified baseline* showed the same pattern and an impossible ratio (throughput column 13.1k with p50 9.3 ms for 64 writers). The cause was the soak tool, not the WAL: it published latency samples in batches of 2,048 per thread, so at 100-200 ops/s per thread the per-interval "ops/s" column (sample count) alternated between ~2x and 0. Recomputing from the exact `highest_seq` deltas (table above) shows a steady rate. The tool was fixed (64-sample flush, throughput from sequence deltas); the high-water mechanism had no measurable effect (10-minute A/B: 11,011 vs 11,089 ops/s, identical batch sizes) and was **reverted**. The p95 column in the 25-minute run's raw CSV is also affected by the old batching and is not reported.

## 7. Failure of the default concurrent harness (reported, not hidden)
`cargo test --release --workspace` (default test threads) runs M1.2 and M1.3 **concurrently** inside one binary on one SATA disk; they compete for the device's serialized flushes (and the debug-profile run is additionally CPU-bound). Results in the full-regression runs: see `PHASE_RUBIXDB_WAL_CERTIFICATION.md` §3. The tests' own reproduction commands run them in isolation; I did not alter the tests or thresholds.

## 8. Windows I/O alternatives (Rule 10) — rejected
Measured: write-through-only 2.15 ms vs write+`FlushFileBuffers` 4.80 ms per 32 KiB durable write (2.2x), but durability past the device's volatile cache cannot be shown (documentation scope excludes it; process-kill tests cannot distinguish it; no power-loss capability), unbuffered modes show p95 up to 74 ms and max > 1 s and require sector alignment. No flag changes made.

## 9. Gates
M1.2 **PASS** (isolated, 18/18; margin >= +9%) · M1.3 **PASS** (isolated, 18/18; margin >= +15%) · **NVMe: HARDWARE UNAVAILABLE** · TAIL LATENCY **PASS with disclosed p99.9/max regression at 256-512 writers** · CPU **PASS** (lower) · RSS **PASS** · THREADS **PASS** · HANDLES **PASS** · product path **PARITY**.
