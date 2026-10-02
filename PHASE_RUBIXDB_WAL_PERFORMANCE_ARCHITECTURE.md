# PHASE RUBIXDB — WAL PERFORMANCE ARCHITECTURE & DECISIONS

**Date:** 2026-10-02/03 · **Base:** `7b7aaaa` (master) · **Work branch:** `wal-batch-buffer-fillq` (changes **uncommitted**, not merged)
**Authoritative inputs:** `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`, `FINAL_WAL_ANALYSIS.md`, current source. Historical documents were not modified.
Evidence tags: **[RUN]** measured this phase · **[MODEL]** measured on an isolated prototype (not production code) · **[PRIOR]** earlier record, not re-run.

## 0. Decision summary (read this first)

| Question | Answer | Basis |
|---|---|---|
| Is NVMe available? | **No.** Both physical disks are SATA SSDs. NVMe experiment = **OPEN / HARDWARE-GATED**. No claim below depends on NVMe. | §1 |
| Does the unmodified WAL meet M1.2 / M1.3 on this hardware? | **No** (M1.2 10.3-10.7k vs 15k; M1.3 59-63k vs 80k). | `..._PERFORMANCE.md` §2 |
| Is that purely a hardware limit? | **No.** Raw flush latency (~4.4 ms) is fixed hardware, but the *algorithm* leaves the disk ~94% idle and serializes per-record write syscalls; an isolated prototype with a different design reaches **17-18k / 93-101k on the same SATA disk**. | §4, §6 |
| Was a safe improvement implemented? | **Yes — "stage 1": count-aware, quiescence-guarded early window close** (192 added lines in `src/wal/group_commit.rs`). M1.2 **+13.5%**, 2-16 writers **+44% to +85%**, p50 **-32% to -46%** at 2-16 writers, M1.3 neutral. Durability, ordering, recovery, on-disk format and failure semantics **unchanged**. Re-certified (§8). | §5, `..._RESULTS.md` |
| Does stage 1 meet the targets? | **No.** M1.2 ≈ 11.5k (target 15k), M1.3 ≈ 62.6k (target 80k). | |
| What would meet them? | **Stage 2: leader-written batch buffer + stage 1** (prototype 17-18k / 93-101k). **NOT implemented — STOP: it changes the WAL's failure semantics and needs your decision** (§6). | §6 |
| Sharded WAL? | **Rejected for this hardware.** Parallel flushes on one SATA device serialize (2 lanes: +15% flush rate, 1.7x per-flush latency); only independent physical devices scale (~2.2x). | §7 |
| Pipelined commit? | **Rejected** (reproduced in a model; no gain, 1.65x more fsyncs, worse tails). | §4.4 |
| Spin vs sleep? | **Spin retained**; sleep/hybrid cost 2-3x CPU at 100 writers with no separable throughput gain. | §4.3 |
| Targets changed / durability traded? | **No.** 15,000 and 80,000 unchanged; no periodic fsync, no ack-before-fsync, no lossy batching. | |
| Final status | **WRITE ENGINE = ENGINE-BLOCKED (performance) — correctness RE-CERTIFIED for the stage-1 change.** M1.2/M1.3 = **FAIL**, **not** hardware-gated. | `..._RESULTS.md` |

## 1. Environment audit (verified at OS level, not assumed from drive letters)
`git`: HEAD `7b7aaaa`, branch `master` at start. i7-7700 (4 cores / 8 threads), 15.9 GB RAM, Windows 10 (19045), rustc/cargo 1.98.1.

| Physical disk | Model | Bus | Size | Volumes |
|---|---|---|---|---|
| Disk 0 | "SSD 128GB" | **SATA** | 128 GB | `E:` (NTFS) — repo and all benchmark data directories used here (`E:\waltmp`) |
| Disk 1 | "LAPCARE" | **SATA** | 128 GB | `C:` (NTFS, default `%TEMP%`) and `D:` |

`Get-PhysicalDisk`/`Get-Disk` report `BusType = SATA` for both; there is no NVMe device. **Important correction to earlier practice:** the M1 tests create their WAL under `%TEMP%` (`C:`, Disk 1) unless `TMP/TEMP` are overridden. Every measurement in this document states its disk; baseline comparisons were made on `E:` (Disk 0) with `TMP=TEMP=E:\waltmp`, and the unchanged baseline was also run on `C:` for a SATA-vs-SATA comparison.

## 2. Throughput model (derived from [RUN] instrumentation, validated against measurements)
`RGC_TIMING_REPORT` (`--features test-util`) gives per-batch stage means. For M1.2 on `E:`: window 4.3 ms + snapshot 0.17 ms + `fsync` 4.4 ms + coordination 0.1 ms ≈ 8.9 ms per batch of ~94.6 records ⇒ **10.6k ops/s — matches the measured 10.5k.** Disk utilization is only **~6%** (queue depth 0.06): the system is **latency-bound, not bandwidth-bound**, and the disk idles during the window.
In a closed loop of N writers, throughput = (records per batch) / (cycle time); cycle = collection + write + flush + wake. This model drives every conclusion below.

## 3. Root-cause breakdown (what is hardware, what is algorithm)
| Component | Cost | Class |
|---|---|---|
| `FlushFileBuffers` | ~4.4 ms (E:) / ~3.6 ms (C:) for a ~32 KiB batch; cannot be parallelized on one SATA device (§7) | **Hardware limit** |
| Batch window | ~4.3 ms, a *fixed* duration sized to fsync latency; closes early only on the byte cap | **Algorithm** (scheduling policy) |
| Re-arrival cascade | after each ack ~100 writers must wake, build a record and append; ~3.3 ms observed (M1.2) | Algorithm + OS scheduling |
| Per-record append | one `seek_write` syscall per record under the WAL lock (`SegmentIo::append`); serializes the cascade | **Algorithm** |
| Snapshot / coordination / completion | 0.15-0.2 ms / ~0.1 ms | negligible |
Window sweep (§4.2) shows no single fixed window serves both M1.2 and M1.3, which is why a fixed-duration window is the wrong abstraction.

## 4. Candidates measured
### 4.1 Unchanged WAL on NVMe — OPEN / HARDWARE-GATED
Cannot be run. Path **B** of the decision tree applies for this question. It remains the single most valuable *hardware* experiment (`FINAL_WAL_ANALYSIS.md` §20 P1-1); the unmodified binary and these exact tests are ready for it (`scripts/wal_bench_runner.ps1`).

### 4.2 Batch-window sweep (Phase 4) — current window is near-optimal for M1.2; one fixed window cannot serve both
Fixed window forced with the existing experiment hook (`PHASE1_EXPERIMENT_*`), 3 runs/point, `E:`, medians (min-max):

| Window | M1.2 ops/s (rec/sync) | M1.3 ops/s (rec/sync) |
|---|---|---|
| 0.5 ms | 8,136 (7,640-9,020) (51) | 43,179 (41,652-43,649) (308) |
| 1 ms | 8,973 (8,892-9,925) (60) | 59,787 (59,216-60,860) (514) |
| 1.5 ms | 9,973 (9,289-10,553) (69) | 60,776 (60,546-61,243) (529) |
| 2 ms | 10,453 (9,878-10,527) (76) | 61,463 (60,308-62,932) (536) |
| 3 ms | 10,625 (10,146-10,760) (86) | 60,949 (60,414-62,094) (556) |
| 4 ms | 10,262 (10,058-10,312) (92) | 62,366 (61,983-62,914) (623) |
| 6 ms | 9,067 (8,787-9,135) (98) | 61,230 (56,316-63,282) (777) |
| 8 ms | 7,840 (7,581-7,853) (99) | **70,590 (70,156-72,697) (975)** |
A larger window helps M1.3 (+14% at 8 ms) but hurts M1.2; shrinking it shrinks batches. The right close time depends on *how many writers are in the cohort*, not on a duration ⇒ count-aware close (§5). (Latency/CPU for the larger windows were not separately characterized; larger fixed windows were not adopted.)

### 4.3 Spin vs sleep vs hybrid (Phase 5) — spin retained
Existing `phase1-waitmode-experiment` hook on the final code, 3 runs/mode:

| Mode | M1.2 ops/s | M1.2 CPU-s | M1.3 ops/s | M1.3 CPU-s |
|---|---|---|---|---|
| spin (current; bounded: window <= min(max_wait, fsync EMA), periodic `yield_now`) | 10,914-12,012 | **10.0-13.2** | 62,019-64,126 | 81.0-87.2 |
| sleep | 11,193-12,501 | 23.5-32.0 (**2-3x**) | 66,149-66,910 | 81.3-83.1 |
| hybrid | 12,197-12,310 | 27.4-28.7 (**2.5x**) | 63,258-66,490 | 98.6-101.6 (**+20%**) |
Throughput ranges overlap for M1.2 (so no separable gain); sleep gives ~+5% on M1.3 at equal CPU but is not retained: the benefit is within the stage-1/stage-2 effects and it adds a mode. **No unbounded busy-wait exists in either the retained or rejected modes.** `WAIT STRATEGY = PASS (spin retained)`.

### 4.4 Pipelined group commit (Phase 6) — REJECTED
Historical: M1.3 63k -> 37-40k with ~2x slower fsync (`FINAL_WAL_ANALYSIS.md` §16). **Not blindly retried.** A raw probe shows concurrent writes to a *different* file do not slow a flush (p50 4.5-4.7 vs 4.84 ms flush-only) and to the *same* file only +15-20% — so the old 2x is not a pure hardware property. A durability-equivalent **prototype** (`examples/commit_pipeline_proto.rs`, real flushes) reproduces the non-adoption: `pipe` 9.6-9.9k (= baseline), batches shrink to ~60 records, **1.65x as many fsyncs**, worse p95/p99; `pipe-fill` 7.1-7.7k. In a closed loop nearly all writers are inside the in-flight batch, so overlap just splits the cohort. The historical commit (`a2c2dc0`) was not rebuilt; this is a model reproduction of its *direction*, not of its exact numbers.

## 5. Selected change implemented — Stage 1: count-aware, quiescence-guarded window close
**What:** in `spin_wait_for_batch_window` the leader additionally exits when (a) the open batch holds at least as many records as the previous batch (the expected cohort), **and** (b) no record has been appended for `quiescence(target)` = `clamp(400 ns x target, 100 µs, 1 ms)`. Implemented with one extra relaxed atomic update per append (`appended_seq.fetch_max`) and the batch size already computed after each fsync (`prev_batch_records`).
**Why a quiescence guard:** a target taken from the previous batch alone creates a **ratchet** (a small batch makes the next target small, and the batch can never grow). Prototype: plain fill showed it (one run at 74 records/sync, 14.4k); fill + quiescence held 100 records/sync every run. Regression test `small_prior_batch_does_not_ratchet_batches_small`.
**Cutoff:** applies only to cohorts <= `COHORT_CLOSE_MAX_TARGET = 256`. Without it, 1,000 writers showed no throughput gain and ~2x worse p99.9 (stragglers that miss an early close wait a whole cycle). 32 writers +30%, 256 writers +12-25%, 512 neutral, 1,000 harmful ⇒ 256 is the largest demonstrated-beneficial size. **Empirical and hardware-dependent (this 8-thread machine).** Above the cutoff the code path is identical to before (target forced to 0, no polling).
**What it does not touch:** which records an `fsync` covers (decided later at `snapshot_sync_target`), `durable_through` publication, the on-disk format, recovery, poisoning/failure handling, rotation. It can only close the window **earlier than the existing deadline, never later**.
**Durability argument:** an acknowledgement still requires a completed `sync_all` that began after the record was appended; the early close changes only *when* the leader calls snapshot+fsync. Ordering is unchanged (seq assigned under the WAL lock exactly as before).

## 6. Stage 2 — leader-written batch buffer — **NOT IMPLEMENTED; DECISION REQUIRED**
**Evidence:** [MODEL] `buf-fillq` (appends are a `memcpy` into an in-memory buffer; the leader does **one** `write_all` of the whole batch, then the `fsync`; plus stage-1 closing). 3-5 runs each on the same SATA disk:

| Writers | current-model | `condvar-fill` (stage 1 only) | `buf` only | **`buf-fillq`** |
|---|---|---|---|---|
| 100 | 9.6-9.8k (p50 9.7, p99 20-22 ms) | 12.0-13.9k (p50 6.7-7.1) | 9.1-10.3k | **17.3-18.0k** (p50 **5.0**, p95 6.2-6.6, p99 12.9-13.9) |
| 1,000 | 64.6-77.7k (11 runs) | — | — | **93.0-101.1k** (p50 8.3-8.4, p99 19.8-25.5) |
Neither half alone reaches the target; together they exceed it with *lower* latency. The prototype calibrates to production within ~10% (`condvar` 9.0-9.7k vs production 10.5k at 100 writers) — a **model**, not production evidence.

**Why it needs your decision (STOP condition: failure semantics change):** today `FileWal::append` writes each record immediately with per-record rollback (`SegmentIo::append`: on write failure truncate to the pre-append length; poison only if the rollback fails). A failed write (e.g. ENOSPC) fails **only that caller**; the committer continues (`PHASE5_ENOSPC_FAILURE_ANALYSIS.md` documents this as observed behavior). With a deferred batch write, callers have already received `Ok(position)` when the flush fails. The safe options are:
1. **Poison on flush failure** (identical to the existing fsync-failure model: "this GroupCommitter is poisoned and must be discarded"). Simple and safe, but a transient ENOSPC would become *fail-stop until reopen* instead of per-write errors — an availability/semantics change for the Write Engine.
2. **Rewind sequence numbers** after a rolled-back flush — **unsafe**: a straggler waiter still holding the old seq could observe a later batch's watermark and receive a **false acknowledgement**.
3. **A failed-range protocol** (record rolled-back seq ranges, check them before the watermark) — a new commit protocol; the mission forbids improvising one in implementation code.
4. **Preallocate segment space** so ENOSPC can only occur at append-time reservation — a recovery/format-adjacent change (zero-filled tails), also out of scope here.
Other stage-2 interactions that need an explicit design before coding: rotation mid-buffer (flush + rotate ordering), `WalPosition` offsets (logical vs physical), `FaultInjectingIo` write-failure tests, abort-point `MidAppend` semantics (a record would not yet be on disk), shutdown draining, memory bound of the buffer (`max_batch_bytes` already bounds it).
**Recommendation:** option 1, documented as the new failure contract, **if** you accept it; otherwise leave the performance gate blocked/hardware-gated. I have not changed anything for stage 2.

## 7. Sharded WAL investigation (Phase 7) — measured first; **not implemented**
**Premise test (no WAL code):** `examples/fsync_lanes_probe.rs`, 32 KiB write + `sync_all` per flush, 3 repetitions each:

| Lanes | Placement | Flushes/s (aggregate) | Per-flush p50 |
|---|---|---|---|
| 1 | `E:` | 193-197 | 4.85 ms |
| 2 | same disk | 224-228 (**+15%**) | 8.1-8.3 ms (**1.7x**) |
| 4 | same disk | 210-219 (no gain) | 17.5 ms |
| 8 | same disk | 380-396 | 18.8-19.0 ms |
| 2 (1 each) | **two physical disks** | **419-437 (2.2x)** | 4.3-4.4 ms (unchanged) |
| 4 (2 each) | two physical disks | 505-514 | 7.3-7.5 ms |
**Conclusion:** flushes on one SATA device are serialized (latency grows with lane count); only independent devices scale. In a closed loop with a fixed writer population, 2 lanes on one disk would *lower* throughput (cycle ≈ window + 8.3 ms instead of + 4.9 ms). **A sharded WAL has no benefit on a single SATA device**, and a single-node local product cannot assume a second physical device. The question is therefore **hardware-gated** (needs ≥2 independent devices or NVMe parallelism) and is not pursued.

**Analysis for the record (required before any future implementation).** A multi-lane design must keep one *global* sequence space and one global durability watermark:
* **Ordering/sequence:** a global atomic `seq` assigned before lane selection; recovery merges lanes by `seq` and must stop at the **first gap** (a record after a missing lower seq is *not* recovered — it was never acked, because acks require the global prefix).
* **Durability watermark:** `durable_through = min over lanes of (highest seq whose lane has fsynced AND all lower seqs on every lane are durable)`; a record is acked only when the **contiguous** global prefix through its seq is durable — otherwise "row A visible while row B (lower seq) is not durable" can occur.
* **Cross-shard atomicity:** a multi-record transaction is already one `Group` frame in one lane (atomic by construction); a transaction spanning lanes would need a commit record + two-phase durability — **disallowed**: keep one frame per transaction on one lane.
* **Manifest/compaction/read visibility/snapshots:** checkpoint `flushed_through_seq` must use the global durable prefix; reads/snapshots see seq <= global watermark only.
* **Crash windows** (per lane L, record r, seq s): before append — nothing durable, not acked; during append — torn tail in L truncated by recovery, records >= s on all lanes dropped at gap, not acked; after append / before fsync — present or absent on disk, **not acked** (permitted either way, but recovery keeps only the contiguous prefix); during fsync — same; after fsync, before ack — durable, recovered, client may retry (duplicate-safe by seq/key semantics of the caller); after ack — durable and recovered; cross-lane — a gap in another lane truncates the recovered prefix at the gap (acked records are never beyond a gap by construction); during recovery — recovery is read-only until truncation, idempotent.
This analysis is **design-only**; no sharded code was written.

## 8. Re-certification scope for the implemented change
`src/wal/` changed (one file) ⇒ the previous WAL certification is invalidated *for the changed behavior* and was re-run: unit/integration/crash/recovery/pathological/property/fuzz suites (debug **and** release), real external process-kill cycles (WAL and full engine), an **independent ack oracle** (`examples/wal_ack_oracle.rs`, self-tested with a mutant), resource and tail-latency comparisons, shutdown and full workspace regression. Results: `PHASE_RUBIXDB_WAL_CRASH_RECOVERY.md`, `PHASE_RUBIXDB_WAL_RESULTS.md`.

## 9. Relational commit lock — untouched (separate bottleneck)
The per-table epoch write lock held across the durable wait (`PHASE_RUBIXDB_FINAL_SINGLE_NODE_PERFORMANCE.md` §4) was **not** changed. Note the interaction: stage 1 improves *engine-level* latency at low concurrency, but same-table SQL commits remain serialized one-per-fsync by that lock until a separate, authorized ADR (e.g. append under the lock, await durability after releasing it — requires an engine API split).
