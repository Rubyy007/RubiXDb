# PHASE RUBIXDB — WAL PERFORMANCE MEASUREMENTS

**Date:** 2026-10-02/03 · i7-7700 (4C/8T), 15.9 GB, Windows 10, **two SATA SSDs, no NVMe**. rustc 1.98.1, release builds.
All runs: unchanged M1.2/M1.3 workloads (100 threads x 1,000 records, 1,000 threads x 1,000 records), `--test-threads=1`, run in isolation, `RGC_TIMING_REPORT` build (`--features test-util`). Resources sampled once per second by `scripts/wal_bench_runner.ps1`. Raw data: `scratch/wal_resolution/`. Browser/IDE processes were open on this developer machine throughout; **no slow run was dropped.**
Targets (unchanged): **M1.2 >= 15,000 ops/s, M1.3 >= 80,000 ops/s.**

## 1. Phase 1 — current baseline, unmodified WAL (5 runs per cell)
Disk **E:** (Disk 0), `TMP=TEMP=E:\waltmp`:

| Test | ops/s min / **median** / max | batches | records/sync | mean window | mean fsync | snapshot | coordination | verdict |
|---|---|---|---|---|---|---|---|---|
| M1.2 | 10,347 / **10,512** / 10,706 | 1,052-1,062 | ~94.6 | 4.23-4.34 ms | 4.34-4.44 ms | 0.15-0.23 ms | ~0.1 ms | **FAIL** (all 5) |
| M1.3 | 59,231 / **61,528** / 62,649 | 1,482-1,507 | ~668 | 5.15-5.42 ms | 4.80-5.27 ms | 0.52-0.62 ms | 0.03-0.06 ms | **FAIL** (all 5) |

Disk **C:** (Disk 1, same binary, `%TEMP%` default):

| Test | ops/s min / **median** / max | mean window | mean fsync | disk busy | verdict |
|---|---|---|---|---|---|
| M1.2 | 11,294 / **11,654** / 12,116 | 3.56-3.61 ms | 3.59-3.68 ms | 5-8% | **FAIL** |
| M1.3 | 42,888 / **50,881** / 59,311 | 4.69-5.01 ms | 4.60-8.17 ms | 10-35% | **FAIL** (noisier; `C:` carries OS activity) |

Resources (E:): threads 105 / 1,005, handles 160 / 1,061, RSS 5-11 MB / 74-88 MB, process CPU 11.9-13.1 s / 80.3-88.4 s; **physical-disk utilization 5-8%, queue length 0.05-0.08**, average write latency 0.24-0.32 ms (M1.2) — the device is **latency-bound and ~94% idle**.
Parallel-test contention (full `cargo test --workspace`, default threads): release 5.6-6.0k / 32-39k; debug 718-868 / 4.9-5.8k. (Variance, not capability.)
**Classification of the baseline:** CURRENT WAL BASELINE = reproduced and characterized; **M1.2/M1.3 FAIL** on both SATA disks.

## 2. Phase 3 — root-cause breakdown (measured)
See `..._ARCHITECTURE.md` §2-§3. Cycle model (M1.2/E:): 4.3 + 0.17 + 4.4 + 0.1 = 8.9 ms per ~94.6-record batch ⇒ 10.6k ops/s (measured 10.5k). Stages not separately timed by existing instrumentation (attributed by experiment instead): per-record `write` syscall cost under the WAL lock and the post-ack wake/re-arrival cascade (isolated by the prototype: batching the writes was worth ~+30% on top of window closing).

## 3. Raw storage probes (no WAL code)
`fsync_lanes_probe` (3 reps): 1 lane `E:` 193-197 flushes/s, p50 4.85 ms; 2 lanes same disk 224-228/s, p50 8.1-8.3 ms; 4 lanes 210-219/s, p50 17.5 ms; 8 lanes 380-396/s, p50 18.8-19.0 ms; **two physical disks, 1 lane each 419-437/s, p50 4.3-4.4 ms (2.2x)**.
`fsync_overlap_probe` (3 reps): flush-only p50 4.84 ms (E:) / 4.27 (C:); with a concurrent writer on the **same** file 5.6-5.9 / 4.8-4.9 ms (+15-20%); on a **different** file 4.5-4.7 / 3.9 ms (no slowdown; occasional 0.3-1.2 s outliers on E:).

## 4. Window sweep, wait strategy, pipelining
Tables in `..._ARCHITECTURE.md` §4.2-§4.4 (window sweep 8 points x 3 runs x 2 tests; spin/sleep/hybrid 3 runs x 2 tests with CPU).

## 5. The implemented change (stage 1) — A/B on the real WAL
Interleaved BASE (unmodified, experiment-hook build with env vars unset = production behavior) vs FINAL, 6 pairs, `E:`:

| Test | BASE min / median / max | FINAL min / median / max | Change (median) | Separated? |
|---|---|---|---|---|
| M1.2 | 9,325 / 10,099 / 10,327 | **11,142 / 11,474 / 11,717** | **+13.6%** | **yes** (FINAL min > BASE max) |
| M1.3 | 62,662 / 63,376 / 63,710 | 60,239 / 62,556 / 63,193 | -1.3% | no — overlapping; neutral within noise (target > 256 ⇒ identical code path) |
Targets: **M1.2 11.5k < 15k (FAIL), M1.3 62.6k < 80k (FAIL).** Earlier pre-cutoff pairs (not used for the conclusion): M1.2 +13-26%, M1.3 between -12% and +7% across 12 pairs.

### Resources and CPU (median of 6 pairs)
| | BASE | FINAL |
|---|---|---|
| M1.2 process CPU | 13.6 s | 13.5 s |
| M1.3 process CPU | 84.2 s | 82.5 s |
| Threads (M1.2 / M1.3) | 105 / 1,005 | 105 / 1,005 |
| Handles (M1.2 / M1.3) | 160 / 1,061 | 166 / 1,061 |
| RSS max | 11 / 80 MB | 10 / 86 MB |
| Disk busy (M1.2) | 7.6% | 6.2% |
Added per-append cost: one relaxed `fetch_max` on an atomic; no allocation, no lock, no clock read. No new threads, queues, buffers or channels.

## 6. Commit latency — long runs (`examples/wal_commit_latency.rs`, same workload shape; 3 reps interleaved; medians shown, ranges in raw data)
| Writers | ops/s BASE → FINAL | p50 ms | p95 ms | p99 ms | p99.9 ms | max ms |
|---|---|---|---|---|---|---|
| 32 | 3,972 → **5,127 (+29%)** | 7.76 → **5.97** | 9.53 → 8.57 | 11.09 → 10.10 | 26.4 → 24.8 | 133.9 → 77.8 |
| 100 | 9,981 → 10,674 (+7%; 9,599-10,246 vs 10,044-11,042) | 9.04 → **8.28** | 17.07 → 16.74 | 22.4 → 24.3 | 32.8 → 82.3 | 44.2 → 90.1 |
| 256 | 25,602 → **30,081 (+17%)** | 9.16 → **7.93** | 15.00 → **11.42** | 20.76 → **17.99** | 81.2 → **25.6** | 91.8 → **41.2** |
| 1,000 (no early close, >256) | 60,232 → 59,769 (-1%) | 14.96 → 14.98 | 28.5 → 27.6 | 38.5 → 41.1 | 137 → 122 | 156 → 140 |
**Disclosures (not hidden):** at 100 writers one FINAL run showed p99.9 218 ms / max 431 ms (the others 33-82 / 58-90); I cannot attribute it (disk stalls of 0.3-1.2 s appear on this SSD in raw probes and in baseline runs, but this one occurred in FINAL). 100-writer p99 medians 22.4 vs 24.3 ms are within run noise. At 1,000 writers FINAL ≡ BASE logic, so differences are noise.

## 7. Diagnostic concurrency ladder (2 interleaved reps; M1.2/M1.3 themselves unchanged)
| Writers | ops/s BASE → FINAL | p50 ms BASE → FINAL | p99 ms BASE → FINAL | max ms BASE → FINAL |
|---|---|---|---|---|
| 1 | 245 → 250 (parity) | 3.7 → 3.6 | 7.9 → 7.5 | 25 / 21 → 22 / 27 |
| 2 | 268 → **495 (+85%)** | 6.9 → **3.7** | 10.6-12.8 → **6.7-7.1** | 28-114 → 21-22 |
| 4 | 548 → **965 (+76%)** | 6.9 → **3.9** | 9.7-10.3 → **6.3-6.6** | 38-41 → 21 |
| 8 | 1,046 → **1,690 (+62%)** | 7.3 → **4.6** | 11.0 → **7.3** | 42 → 22 |
| 16 | 2,060 → **2,963 (+44%)** | 7.5 → **5.1** | 10.6-10.8 → **8.0-9.7** | 39-42 → 50 |
| 32 | 3,972 → 5,127 (+29%) | 7.76 → 5.97 | 11.1 → 10.1 | 134 → 78 |
| 64 | 7,196 → 8,189 (+14%) | 8.3 → 6.9 | 18.4-18.8 → 19.2-20.8 | **45-49 → 110-111** |
| 100 / 256 / 512 / 1,000 | see §6 and below | | | |
512 writers (single rep each side, earlier build ≡ final logic at this size): 46,770 vs 46,255 ops/s — neutral.
**Disclosure:** at 64 writers FINAL's max was 110-111 ms in both reps vs 45-49 ms for BASE, and p95 was 1.4-3 ms worse; p99/p99.9 equal. Single-run maxima on this SSD are dominated by device stalls, but the 64-writer difference appeared twice, so it is reported as a real, small tail cost at that size.

## 8. Single-writer latency (M1.1) — unchanged
BASE 3.5-3.85 ms p50, FINAL 3.5-3.7 ms; `m1_1_single_writer_latency_unchanged` **passes** (debug and release).

## 9. Throughput vs target summary
| | Baseline (median) | After stage 1 | Target | Status |
|---|---|---|---|---|
| M1.2 | 10.1-10.5k | 11.5k | 15k | **FAIL** (77% of target) |
| M1.3 | 61.5-63.4k | 62.6k | 80k | **FAIL** (78% of target) |
Prototype stage 2 (not production): ~17-18k / 93-101k (see architecture §6).
