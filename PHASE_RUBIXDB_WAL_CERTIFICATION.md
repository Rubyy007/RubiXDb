# PHASE RUBIXDB — WAL CERTIFICATION (flat-combining group commit)

**Date:** 2026-10-03 · **Branch:** `wal-batch-buffer-fillq` (not merged; source committed there by the user as `e557be8`, plus the final edits listed in §6) · **Base:** `master` `7b7aaaa`.
Supporting: `PHASE_RUBIXDB_WAL_ARCHITECTURE_OPTIONS.md`, `..._IMPLEMENTATION.md`, `..._CRASH_RECOVERY.md`, `..._PERFORMANCE_FINAL.md`. Earlier ADRs and the previous phase's documents are history and were not rewritten (the mandate's file name `PHASE_RUBIXDB_WAL_CRASH_RECOVERY.md` coincided with the previous phase's file and **replaced** it; the old text is in git history).

## 1. Decision

> **The throughput targets are now MET on this SATA hardware in isolated release runs (M1.2 17.1k warm / 18.1k cold; M1.3 99.0k warm / 100.0k cold; 18 of 18 runs pass), by a design that preserves `append`'s contract and the durability model, and all correctness evidence passes. Under Rule 38 as written the WAL is still NOT PRODUCTION READY, because FULL REGRESSION is not clean: the two throughput tests fail under the default concurrent/debug test harness (and one pre-existing, load-sensitive unit test failed once in debug). That is a harness-definition question that needs a human decision; I did not change tests, thresholds or the harness.**

What is established vs not:
| Statement | Status |
|---|---|
| M1.2 >= 15,000 and M1.3 >= 80,000 on this machine, isolated release, same tests/thresholds | **Yes** — 18/18 interleaved runs (min margins +9% / +15%) + 3/3 canonical `cargo test --release --test group_commit -- --test-threads=1` on the final source |
| NVMe result | **HARDWARE UNAVAILABLE** (both disks SATA) — not claimed either way; the SATA result stands as evidence for this environment |
| Same targets under the default concurrent `cargo test` harness | **No**: release workspace 5,772 / 52,265 ops/s (baseline in the same harness 6.0k / 39.5k); debug 539 / 5,387 |
| Durability of acknowledgments across **power loss** | **Not tested** (no capability); process-kill durability, ordering, atomicity and recovery all PASS |
| Product SQL write throughput | **Unchanged (parity ±5%)** — not an improvement claim |

## 2. Certification matrix (mandate format)
| Item | Result | Basis |
|---|---|---|
| CURRENT WAL | **FAIL** (performance only): correctness PASS, M1.2 10.5k / M1.3 62.7k | baseline, interleaved |
| EARLY CLOSE | **PASS** — retained, generalized (cohort/quiescence + straggler fallback, no size cutoff, probe fix); alone it is insufficient (11.4k / 62.5k) | options §4 |
| DOUBLE BUFFER | **REJECTED** (acknowledges `append` before bytes are written; failure semantics unprovable without poisoning) | options §4-B |
| PIPELINE | **REJECTED** (model: no gain, 1.65x fsyncs, worse tails) | options §4-C |
| WINDOWS I/O MODE | **REJECTED** (write-through-only 2.2x faster per write but durability past the device cache unprovable; unbuffered: p95 up to 74 ms, max > 1 s, alignment/format impact) | options §4-F |
| SHARDED WAL | **NOT IMPLEMENTED** (one SATA device serializes flushes: 2 lanes +15% rate at 1.7x latency; two devices 2.2x) | options §4-D |
| ACK DURABILITY | **PASS** — fsync/watermark/poison path unchanged; 0 of 1,471,733 oracle acks lost (final code) + 1,518,796 (preceding build); **power loss not testable** | crash doc |
| ATOMICITY | **PASS** — multi-record `Group` = one frame; 0 partial groups | crash doc |
| ORDERING | **PASS** — seq assigned under the WAL lock in queue order; differential property test vs sequential append | impl doc, crash doc |
| RECOVERY | **PASS** — format and recovery unchanged; pathological matrix 9/9, `wal_tests` 12/12 | |
| CRASH CONSISTENCY | **PASS** — 11 abort points; 140 WAL-layer + 70 engine-layer external kill cycles (final code) | |
| TRANSACTION RECOVERY | **PASS at WAL/engine level** — `Group`-frame atomicity in the oracle (0 partial groups) and engine kill cycles 70/70 (0 read mismatches); and a **product-level** (SQL/API) smoke on the final build: DDL, DML, a committed 2-row transaction, then `taskkill /F` with an open uncommitted transaction and 4 concurrent writers running until the kill → restart: committed rows and transaction present, deleted row and uncommitted row absent, index scan correct (a smoke, not a campaign) | |
| RESOURCE BOUNDS | **PASS** — queue ≤ #threads, rounds ≤ 4,096 frames, no new threads/channels; soak RSS 7.0 MB flat, threads 65-68, handles 110-112, WAL 14-32 MB bounded | performance doc §6 |
| TAIL LATENCY | **PASS with disclosure** — p50/p95/p99 better at every concurrency (1-1,000 writers); **p99.9/max worse at 256-512 writers** (≈71-108 ms vs 28-50 at 256; 91-101 vs 27-190 at 512) | performance doc §4 |
| M1.2 | **PASS** isolated (17,145 warm median, min 16,353; 18,053 cold) · FAIL under default concurrent harness · NVMe: HARDWARE UNAVAILABLE | |
| M1.3 | **PASS** isolated (98,975 warm median, min 91,730; 100,045 cold) · FAIL under default concurrent harness · NVMe: HARDWARE UNAVAILABLE | |
| FULL REGRESSION | **FAIL** — see §3 (all failures classified; none is a correctness defect) | |
Also: CPU **PASS** (lower: 10.3 vs 12.8 s, 65.5 vs 82.6 s) · RSS **PASS** (≤ 89 MB) · THREAD STABILITY **PASS** · HANDLE STABILITY **PASS** · LONG-DURATION **PASS** (about 5,700 s of sustained 64-writer load across five soak runs, 40 + 25 + 3 x 10 minutes; exact throughput flat) · PRODUCT PATH **PARITY** (after fixing an intermediate -30% regression).

## 3. Full regression (final source) and classification
Gates: `cargo fmt --all -- --check` **clean**; `cargo clippy --workspace --all-targets --all-features -- -D warnings` **clean**; `cargo check --workspace --all-targets --all-features` **clean**.
| Command | passed / failed / ignored | Failures |
|---|---|---|
| `cargo test --workspace --no-fail-fast` (debug) | **1,121 / 3 / 26** | `m1_2`, `m1_3` (539 / 5,387 ops/s), `wal::group_commit::tests::concurrent_followers_all_fail_fast_when_the_leader_panics` |
| `cargo test --release --workspace --no-fail-fast` | **1,122 / 2 / 26** | `m1_2`, `m1_3` (5,772 / 52,265 ops/s) |
Relevant binaries (release): `rubixdb` lib **556/556**, `wal_tests` 12/12, `pathological_recovery_matrix` 9/9, `crash_consistency` 2/2, `group_commit` 6 passed + the 2 throughput tests. API, CLI, instance, SQL suites pass in both modes.

| Class | Items |
|---|---|
| **OUR CODE FAILURES** | **None remaining.** Found and fixed during this phase: a 30% product multi-table regression (count-only cohort target → straggler fallback); a premature lone-writer probe (~49% of batches closed at ~70/100 records); four lint/format issues; a wrong assertion in my own decision-table test; two bugs in my own measurement tools (soak throughput column; striped-wake prototype handling) — one of which led to a *retracted* false finding (a "degraded mode" that was an artifact). |
| **PRE-EXISTING** | `concurrent_followers_all_fail_fast_when_the_leader_panics`: fails **11 of 12** times under CPU saturation on the **unmodified baseline** and on this branch (its 22.7 ms follower timeout can expire before a panicking leader unwinds); passes unloaded (3/3). Not changed. |
| **ENVIRONMENTAL / HARNESS** | `m1_2`/`m1_3` under the default concurrent harness (both tests and their 1,100 threads share one SATA disk's serialized flushes; the baseline scores 6.0k / 39.5k there) and in the debug profile (CPU-bound by 1,000 unoptimized threads; baseline 718-868 / 4.9-5.8k). Their own printed "reproduce with" commands run them one at a time. |
| **ENGINE-BLOCKED** | **None** — the previous phase's ENGINE-BLOCKED gate (M1.2/M1.3) is cleared in isolation. |

## 4. Production acceptance (Rule 38) — evaluated honestly
durability **PASS** (power loss untested) · atomicity **PASS** · ordering **PASS** · recovery **PASS** · crash consistency **PASS** · resource stability **PASS** · M1.2 **PASS (isolated)** · M1.3 **PASS (isolated)** · **full regression FAIL** ⇒ **WAL = NOT PRODUCTION READY** under the rule as written.
What would change that, all requiring a human decision (none done by me): (a) declare that M1.2/M1.3 are specified for isolated release execution (as their own docs state) and have the regression commands run them serially (`-- --test-threads=1` for `group_commit`) — the debug profile can never meet a 15k/80k target; (b) fix or serialize the pre-existing load-sensitive unit test; (c) obtain power-loss evidence or accept its absence; (d) NVMe re-measurement is optional (the target is already met on SATA).

## 5. Open items and risks
1. **Decision:** how M1.2/M1.3 are executed in the regression gate (§4a); and the load-sensitive test (§4b).
2. **Power-loss durability** could not be tested (process kill cannot see ack-before-fsync); structural argument only (fsync/watermark path unchanged).
3. **Tail latency:** p99.9/max regression at 256-512 writers.
4. **Machine-state sensitivity:** the PC uses the Balanced plan; absolute numbers swing by up to ±30% between minutes (a transient "21k" state was seen and discarded as an outlier regime). Conclusions rest on interleaved, warm *and* cold comparisons, not on absolute values.
5. **Constants** (100 µs / 400 ns / 1 ms / x4 / 2 ms) were chosen on this machine; flat across 50-200 µs after the probe fix, but not re-measured on other hardware (NVMe unavailable).
6. **Product SQL throughput is not improved**; the relational per-table commit lock (Rule 23) was not touched and still caps same-table commits at ~230-260/s.
7. **Flat-combining failure semantics** (documented): a failed batched write fails every writer in that run (previously one); all get `Err` with no state change; recoverable.
8. **Branch state:** unmerged; the source is committed on `wal-batch-buffer-fillq` (`e557be8`), later edits (probe/straggler/final constants, test updates, tools, docs) are uncommitted in the working tree.
9. Experiment hooks (`phase1-window-experiment`, `phase1-waitmode-experiment`, test-util close-reason counters) remain feature-gated and removable.

## 6. Repository changes this phase
`src/wal/{group_commit,mod,ops}.rs`, new `src/wal/group_append_tests.rs`; `examples/{commit_pipeline_proto,fsync_lanes_probe,fsync_overlap_probe,windows_io_modes_probe,wal_commit_latency,wal_soak,wal_ack_oracle}.rs`; `scripts/{wal_bench_runner.ps1,cpu_warm.py,wal_soak_monitor.ps1}`; evidence in `scratch/wal_resolution/`; the five documents of this phase; `PROGRESS.md`, `CHANGELOG.md` (append-only). Protected paths: `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/lsm/`, `src/execution/`, SQL, API, CLI, GUI: **zero diff**.
