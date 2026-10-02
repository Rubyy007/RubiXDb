# PHASE RUBIXDB — WAL PERFORMANCE RESOLUTION: RESULTS & CERTIFICATION

**Date:** 2026-10-02/03 · **Base:** `7b7aaaa` · **Branch:** `wal-batch-buffer-fillq` (changes **uncommitted**, nothing merged)
Supporting: `PHASE_RUBIXDB_WAL_PERFORMANCE_ARCHITECTURE.md`, `PHASE_RUBIXDB_WAL_PERFORMANCE.md`, `PHASE_RUBIXDB_WAL_CRASH_RECOVERY.md`, `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md` (historical, unchanged).

## 1. Outcome in one paragraph
There is **no NVMe** on this machine, so the NVMe experiment is **OPEN / HARDWARE-GATED**. On the available SATA SSDs the unmodified WAL fails both targets (M1.2 median 10.5k vs 15k; M1.3 61.5k vs 80k). The limit is **not purely hardware**: raw fsync latency is fixed (~4.4 ms) and cannot be parallelized on one SATA device, but the algorithm leaves the disk ~94% idle. I implemented and fully re-certified one durability-neutral change (count-aware, quiescence-guarded window close): **M1.2 +13.6% (11.5k), 2-16 writers +44% to +85%, p50 -32% to -46% at 2-16 writers, M1.3 neutral** — still **below target**. An isolated prototype of a second change (leader-written batch buffer) reaches **17-18k / 93-101k on the same disk**, but it **alters the WAL's failure semantics** (a failed write would poison the committer instead of failing one caller), which is a STOP condition and **your decision** — it was not implemented. Sharded WAL and pipelining were measured and rejected for this hardware. No target was lowered and no durability was traded. **At the product level (SQL over HTTP) write throughput is unchanged** by stage 1, because the relational per-table commit lock keeps the WAL's batches at ~1 for same-table writes; the gain is at the WAL/engine layer.

## 2. Decision-tree path
* **PATH B** (unmodified WAL + NVMe unavailable → performance gate HARDWARE-GATED for the NVMe question): applies. Not marked PASS.
* **PATH C** (architecture can safely improve it → benchmark → implement → re-certify): applied to the **safe part (stage 1)**; the full target-meeting design (stage 2) is **STOPPED at a failure-semantics decision**.
* **PATH D/E** (target unreachable without weakening durability / unrealistic target): **not** concluded. The prototype shows the target is reachable on this SATA disk without weakening durability; whether the *product* accepts the required failure-semantics change is the open decision.

## 3. Exactly what the evidence proves (and does not)
| Statement | Supported? |
|---|---|
| The unmodified WAL does not reach 15k/80k on these two SATA disks | **Yes** (5 runs per disk, plus 10+ A/B baselines) |
| That is a pure hardware limit | **No** — an alternative design reached 17-18k / 93-101k on the same disk (prototype/model; ~10% calibration error) |
| The unmodified WAL cannot reach 15k on NVMe | **Not claimed** — untested |
| The unmodified WAL can reach 15k on NVMe | **Not claimed** — untested |
| Stage 1 meets the targets | **No** |
| Stage 1 is durability-neutral | **Yes**, by construction and by 240 oracle kill cycles (1.48 M acks) — *not* provable for power-loss with the available method |
| A sharded WAL would help here | **No** — measured: one SATA device serializes flushes; two physical devices scale 2.2x |
| Pipelining would help here | **No** (model reproduction; historical commit not rebuilt) |

## 4. FINAL CERTIFICATION MATRIX
| Item | Result | Basis |
|---|---|---|
| CURRENT WAL BASELINE | **PASS** (measured/characterized) — performance targets not met | §3 of performance doc |
| M1.2 | **FAIL** — 11.5k (after stage 1) / 10.5k (baseline) vs 15k. Not hardware-gated: algorithm-limited on SATA; NVMe untested | |
| M1.3 | **FAIL** — 62.6k / 61.5k vs 80k. Same | |
| FSYNC BOTTLENECK | **PASS** (characterized: ~4.4 ms, serialized per device) | probes |
| BATCH WINDOW | **PASS** (re-verified by 8-point sweep; refined by early close) | sweep |
| WAIT STRATEGY | **PASS** (bounded spin retained; sleep/hybrid rejected: 2-3x CPU, no separable gain) | |
| PIPELINED COMMIT | **REJECTED** | model |
| NVME RE-MEASUREMENT | **OPEN** (no NVMe hardware) | Phase 0 |
| SHARDED WAL DESIGN | **NOT IMPLEMENTED** (analysis done; no benefit on one device) | |
| DURABILITY | **PASS** (ack-after-fsync path untouched; **power-loss test not possible here**) | |
| ORDERING | **PASS** | |
| RECOVERY | **PASS** | |
| CRASH CONSISTENCY | **PASS** | |
| TRANSACTION ATOMICITY | **PASS** (0 partial groups in 1.48 M acks) | |
| CONCURRENT WRITERS | **PASS** (1-1,000 writers) | |
| TAIL LATENCY | **PASS with disclosures**: improved at 2-256 writers; at 64 writers max 110 ms vs 45-49 ms and p95 +1.4-3 ms; one 100-writer run with p99.9 218 ms (unattributed) | performance doc §6-§7 |
| CPU | **PASS** (13.5 vs 13.6 s; 82.5 vs 84.2 s) | |
| RSS | **PASS** (<= 86 MB) | |
| THREAD STABILITY | **PASS** (105 / 1,005, identical) | |
| HANDLE STABILITY | **PASS** (160-166 / 1,061) | |
| FULL REGRESSION | **FAIL** — 1,115 passed / 2 failed / 26 ignored (debug and release); the 2 failures are exactly M1.2/M1.3. fmt, clippy `-D warnings`, check **clean** | |

**Certification status:** the stage-1 change is **re-certified for correctness** (durability, ordering, recovery, crash consistency, atomicity, resources). **WRITE ENGINE remains ENGINE-BLOCKED on the performance target** — it may not be declared CERTIFIED again because the required performance is neither met nor classified hardware-gated by an approved architecture decision. I did not reinterpret the target.

## 5. Failure classification (not collapsed)
* **OUR CODE FAILURES:** none remaining. (During this phase my own first clippy pass found an unused variable in an analysis example and lints in two probe examples; fixed. A decision-table test assertion of mine was wrong, not the product; fixed.)
* **PRE-EXISTING:** `m1_2`, `m1_3` (known since 2026-09-14).
* **ENVIRONMENTAL:** large run-to-run variance (M1.2 ±5%, M1.3 ±5%, parallel-test contention up to 10x lower); SSD stalls of 0.3-1.2 s seen in raw probes and in both builds.
* **ENGINE-BLOCKED REQUIREMENTS:** M1.2, M1.3 (+ the separate relational per-table commit lock, untouched).

## 6. Final report (mission format)
* **CURRENT BASELINE:** M1.2 10,347-10,706 (median 10,512) on E:, 11,294-12,116 on C:; M1.3 59,231-62,649 (61,528) on E:, 42,888-59,311 on C:. 94.6 / 668 records per sync; fsync 4.4 / 5.0 ms; disk ~6% busy.
* **NVME RESULT:** none — **no NVMe device exists**; OPEN / HARDWARE-GATED.
* **ROOT CAUSE:** latency-bound cycle: fixed-duration window (~4.3 ms) + serialized per-record write syscalls + wake cascade + ~4.4 ms flush; flush itself is serialized per device.
* **CANDIDATE DESIGNS / MEASUREMENTS:** window sweep, spin/sleep/hybrid, pipelining (rejected), park/unpark wake (no gain), count-aware close (**selected**), leader-written buffer (prototype, STOP), sharded lanes (rejected for one device). Tables in the performance and architecture documents.
* **SELECTED DESIGN / WHY:** stage 1 — largest *safe* gain with no durability, format or failure-semantics change, and a measured latency benefit at realistic concurrency.
* **DURABILITY IMPACT:** none by construction; 0 acknowledged-record losses in 1.48 M oracle acks. **Power-loss durability could not be tested.**
* **RECOVERY IMPACT:** none.
* **SECURITY IMPACT:** none (no new input handling, no `unsafe`, no new dependency; added state = two atomics). One extra relaxed atomic per append.
* **CPU / MEMORY IMPACT:** unchanged (13.5 vs 13.6 s; RSS <= 86 MB).
* **THROUGHPUT IMPACT:** M1.2 +13.6%; 2/4/8/16/32/64 writers +85/+76/+62/+44/+29/+14%; M1.3 neutral (-1.3%, overlapping).
* **TAIL-LATENCY IMPACT:** better p50/p95/p99 at 2-256 writers; disclosed small regressions at 64 writers (max, p95) and one unexplained 100-writer outlier run; no change above 256 writers.
* **REGRESSION RESULT:** 1,115 / 2 / 26 both modes; only M1.2/M1.3 fail; fmt, clippy, check clean; protected paths: `src/manifest/`, `src/sstable/`, `src/compaction/` **zero diff**, `src/wal/` = one file (+192/-2).
* **PRODUCT-LEVEL REGRESSION [RUN, real release `rubixdb` via HTTP/CLI, baseline vs new, same load generator]:**
  - *Write throughput is unchanged at product level* (zero errors on both builds). Same-table autocommit/txn writes, ops/s base vs new at 1/4/16 clients: insert 207/236/231 vs 217/240/237; update 208/229/228 vs 190/223/242; txn 182/241/236 vs 189/251/235; delete 101/119/114 vs 105/121/118 — all within run noise. Multi-table (16 writers): 1 table 223-234 vs 234-248 commits/s; 4 tables 511-517 vs 500-515; 16 tables 2,109-2,133 vs 2,109-2,168; 4 writers/4 tables 506-513 vs 527-534 (+4%).
  - **Honest conclusion: the stage-1 gain is real at the WAL layer (and for direct WAL/engine users) but does not reach product-level SQL throughput**: same-table commits are serialized one-per-fsync by the relational epoch lock, and the multi-table path showed no measurable change either.
  - *Functional smoke on the new binary:* CREATE TABLE/INDEX, multi-row INSERT, UPDATE, DELETE, a committed 2-row transaction, then `taskkill /F` with an open uncommitted transaction and restart: committed rows present, deleted row and uncommitted row (id 99) absent, index scan (`qty = 11` -> id 3) correct.
  - *Not re-run:* Playwright/GUI and a fresh security pass (the change is below the SQL layer and touches no input handling); the workspace regression (api/cli/instance/sql tests) passed.
* **OPEN ITEMS:** (1) **Decision needed:** stage 2 failure semantics (poison-on-flush-failure vs alternatives) — recommendation in architecture §6; (2) NVMe measurement (hardware); (3) power-loss durability testing (no capability); (4) `COHORT_CLOSE_MAX_TARGET = 256` is empirical/hardware-dependent — re-measure on other hardware; (5) the unexplained 100-writer p99.9 outlier and the 64-writer max difference deserve a longer repeat; (6) the relational per-table commit lock remains a separate bottleneck; (7) the branch is uncommitted and unmerged.

## 7. Repository state
New/changed (uncommitted): `src/wal/group_commit.rs`; examples `commit_pipeline_proto.rs`, `fsync_lanes_probe.rs`, `fsync_overlap_probe.rs`, `wal_ack_oracle.rs`, `wal_commit_latency.rs` (analysis tools, not on any production path); `scripts/wal_bench_runner.ps1`; evidence under `scratch/wal_resolution/`; these four documents. A git worktree used for the baseline build lives at `E:\wt_base` (disposable).
