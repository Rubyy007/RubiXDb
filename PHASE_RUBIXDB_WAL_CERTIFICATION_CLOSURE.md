# PHASE RUBIXDB — WAL CERTIFICATION CLOSURE

**Date:** 2026-10-03 · **Branch:** `wal-batch-buffer-fillq` · **Base:** `master` `7b7aaaa`
Closes the three open items the previous phase (`PHASE_RUBIXDB_WAL_CERTIFICATION.md`) left for a human decision: how M1.2/M1.3 are executed, the load-sensitive WAL unit test, and the power-loss gate. No threshold, record count, writer count or durability behaviour was changed; `src/wal/` production code is unchanged in this step (one *test* inside it was corrected, see §3).

## 1. The M1.2 / M1.3 test contract (normative)

| | |
|---|---|
| Normative command | `scripts/wal_certify.ps1` = `cargo test --release --features test-util --test group_commit -- --test-threads=1 --nocapture` (the tests' own printed "reproduce with" lines already name `--release` and a single test) |
| Targets | M1.2 ≥ 15,000 ops/s (100 writers × 1,000), M1.3 ≥ 80,000 ops/s (1,000 writers × 1,000) — read from the tests, unchanged |
| Profile | **release only.** 1,000 unoptimised threads are CPU-bound in debug (baseline 718–868 / 4.9–5.8 k, this branch 539 / 5.4 k): no WAL design can meet 15 k / 80 k there. |
| Device state | **one scenario at a time on an otherwise idle device.** Both scenarios fsync the same SATA disk; they measure each other when run together. |

### Why the default workspace run is not equivalent (measured, not argued)
Same final source, release profile, same machine:

| Execution | M1.2 | M1.3 |
|---|---|---|
| default `cargo test --workspace` (both tests + the other 3 tests of the binary concurrently) | 5,772 | 52,265 |
| only the two throughput tests made mutually exclusive | 12,454 | 70,233 |
| **throughput scenarios exclusive of every other test of the binary** (`cargo test --release --test group_commit`, default threads) — 3 runs | 25,451 / 25,496 / 25,758 | 100,620 / 91,005 / 106,858 |
| normative command, 3 runs (`scratch/prod_ops/wal_certify_3runs.txt`) | 20,328 / 19,758 / 19,811 | 111,296 / **84,604** / 103,148 |

Absolute numbers swing with the Balanced power plan (±30 %, documented in the previous phase); the lowest M1.3 value in this step is 84,604 (+5.8 % over target) — thin, reported as measured. The baseline (`master`) fails both in isolation (10.5 k / 62.7 k, previous phase's interleaved A/B), so the harness change alone does not make a pass.

### How it is encoded (no test removed, no threshold touched)
1. `tests/group_commit/support.rs`: a binary-wide `RwLock`. `run_throughput_scenario` takes it **exclusively**; every other test of the `group_commit` binary holds `support::shared()`. Throughput scenarios therefore never overlap each other or any other test of that binary, in any thread configuration. (Cargo runs test *binaries* one after another, so nothing else competes for the disk.)
2. `m1_2`/`m1_3`: `#[cfg_attr(debug_assertions, ignore = "…release profile only…")]`. In a debug run they are reported as **ignored with the reason printed**, not silently passing and not failing for a reason no code can fix. In release they run and assert exactly as before.
3. `scripts/wal_certify.ps1` is the single certification entry point; its exit code is cargo's.
4. The general workspace regression stays separate and keeps running both tests in release.

## 2. Reproduction of the load-sensitive test
`wal::group_commit::tests::concurrent_followers_all_fail_fast_when_the_leader_panics`, debug lib test binary, 8 busy CPU processes as load (`scratch/prod_ops/repro_flaky.sh`, output `flaky_repro.txt`):

| Source | Unloaded | 8 busy CPU processes |
|---|---|---|
| unmodified `master` (`7b7aaaa`) | 12 / 12 pass | **0 / 12 pass** |
| this branch, before the fix | 11 / 12 pass | **0 / 12 pass** |
| this branch, test corrected | **20 / 20 pass** | **20 / 20 pass** |

Identical failure on both sources: `a caller that lost the leader race must see a clear poisoned-committer error, not a bare timeout: Timeout { detail: "await_durable(seq=2) timed out after 5.5ms … (durable_through=0, ema_fsync_latency_ns=…)" }`.

**Classification: the test is mis-specified, not the product.** `await_durable` is documented (`tests/group_commit/support.rs`) to return `Timeout` as a *recoverable* outcome of one bounded wait; that bound is derived from the fsync-latency EMA (≈5.5 ms). Under CPU saturation a follower's first wait can expire before the starved leader thread has run its hook and panicked. The product behaviour on retry is correct (a retry observes the poison promptly as `Io`).

**Fix (test semantics only):** followers retry on `Timeout` within a 5 s overall bound and the assertions are kept on the call that observes the poison: never `Ok`; must be `EngineError::Io`; must return < 1 s; exactly one panicker; committer poisoned; overall < 6 s. The intent ("no follower of a panicked leader ever sees success, and each fails promptly with a clear poison error") is intact; the unrealistic "the very first 5 ms wait is long enough" is removed.

## 3. Power-loss gate
`PROCESS-KILL DURABILITY = PASS` (1.47 M oracle acknowledgements, 0 lost; 140 + 70 external kill cycles). `POWER-LOSS DURABILITY = NOT TESTED — gate OPEN.` Environment inspected this step: no QEMU, no VirtualBox, no Hyper-V (Windows 10 Home; the feature query needs elevation and the SKU does not ship it), WSL2 present but only the `docker-desktop` distribution — and a VM/process kill would not drop the *host's* disk cache anyway. A kill is not a power cut; it is not substituted for one.

## 4. Merge readiness (user rule: do not merge until source, tests, benchmarks, certification and documentation correspond to the same implementation)
`git diff master --stat` for this branch after the closure commit: `src/wal/` (4 files, production + tests), `tests/group_commit/` (8 files, +36 lines), `examples/` and `scripts/` analysis tools, WAL documents. Nothing else from the previous phase's scope is outside WAL. **Not merged** — merging is a separate decision; this step only makes the correspondence true: the certified source, the tests, the benchmark command and the documents now describe one implementation, and the one open gate (power loss) is recorded as OPEN, not hidden.

## 5. Gates
WAL CERTIFICATION CLOSURE = **PASS for the execution-mode and test-semantics items**; **OPEN for power loss (NOT TESTED)** · M1.2 = **PASS** (normative command, 3/3) · M1.3 = **PASS** (normative command, 3/3, lowest +5.8 %) · NVMe = HARDWARE UNAVAILABLE.

---
## Addendum 2026-10-04 — throughput variance found after the closure (supersedes the PASS wording in §5 for M1.3; the original text above is kept)
Later in the production-operations phase the normative command was run ~30 more times on the same machine:
* **M1.2:** 17–23 k in every run but one **12.3 k outlier** (degraded window). Baseline `master`, interleaved, same window: 9.3–12.0 k (6/6 below target).
* **M1.3 is bimodal:** a *fast mode* 101–113 k and a *slow mode* 61–79 k. A sampler of system CPU + top processes during six consecutive runs (two slow: 78.9 k, 76.6 k; four fast: 108–113 k) showed **no external competitor** in either mode (the test binary ≈ 54 s CPU, everything else ≈ 5 %); the slow mode just takes ~14 s instead of ~10.5 s. Baseline `master`: 48–63 k in 6/6. The 80 k line sits between the two modes, so the target is crossed in roughly 70–75 % of quiet runs and **in every full-workspace release run (61,202 / 35,531 [with a leaked server process from a failing test] / 48,298)**.
* Earlier observations of "18/18" and "3/3" (21:xx runs 84.6–111 k) were in the fast regime; they are not retracted, but they do not establish reproducibility.
**Reclassification:** M1.3 = **FAIL (intermittent on this hardware)**, M1.2 = **OPEN**, WAL CERTIFICATION CLOSURE = **OPEN**. The design comparison against the baseline is unaffected (current ≥ 1.6× baseline on M1.2 and ≥ 1.2× on M1.3 even in the slow mode); the shortfall is a hardware-regime issue (SATA SSD) that needs the NVMe re-measurement or an authorised ADR, not a threshold change. Raw data: `scratch/prod_ops/wal_certify_10runs.txt`, `wal_ab_interleaved.txt`, `m13_runs_with_sampler.txt`, `m13_sampler.csv`.
