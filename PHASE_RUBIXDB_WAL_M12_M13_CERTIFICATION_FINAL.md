# PHASE RUBIXDB — WAL M1.2/M1.3 CERTIFICATION (FINAL STATUS)

**Date:** 2026-10-04 · **Base:** `master` `0da9e14` · **Mission type:** certification/documentation only. No production code, WAL code, test or threshold was changed; nothing was re-measured in this mission. All numbers below are quoted from the cited documents and raw files (`scratch/wal_diag/`).
**Inputs read:** `CLAUDE.md`, `PHASE_RUBIXDB_WAL_M12_M13_DIAGNOSIS.md`, `PHASE_RUBIXDB_WAL_CERTIFICATION.md`, `..._CERTIFICATION_CLOSURE.md` (+ addendum), `..._PERFORMANCE_FINAL.md`, `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md`, the test sources. **`docs/PROJECT_STATE.md` and `missions/ACTIVE.md` (required by `CLAUDE.md`) do not exist in this repository**; their absence is recorded, not worked around.

## 1. Current Acceptance Rules
The only formal, machine-enforced rule is in the tests (unchanged):
| Gate | Rule (verbatim source) | Source |
|---|---|---|
| M1.2 | 100 writers × 1,000 records; `assert!(ops_per_sec >= 15_000.0)`; every acknowledged record recoverable and gap-free after reopen | `tests/group_commit/hundred_writers_throughput.rs` |
| M1.3 | 1,000 writers × 1,000 records; `assert!(ops_per_sec >= 80_000.0)`; same recovery assertions | `tests/group_commit/thousand_writers_throughput.rs` |
| Execution contract | release profile only; one scenario at a time on an idle device; normative command `scripts/wal_certify.ps1` = `cargo test --release --features test-util --test group_commit -- --test-threads=1 --nocapture` | `PHASE_RUBIXDB_WAL_CERTIFICATION_CLOSURE.md` §1 |

What the specification does **not** define: no minimum-of-N, median, percentile or confidence rule, no number of required repetitions, no tolerance for an outlier, no definition of how per-invocation results combine into a certification guarantee. Earlier documents used "N of N runs pass" informally ("18/18", "3/3"); that is a description of those experiments, not an accepted rule. **The acceptance policy for repeated runs is undefined, and this document does not invent one.**
`CLAUDE.md` rule that applies: PASS requires implementation + test + reproducible evidence + documentation; a performance PASS additionally requires workload, methodology, multiple runs, latency distribution and resource measurements; missing evidence = OPEN; a failing test = FAIL.
Not available for any run in this record: per-run latency percentiles for the M1 tests themselves (the tests do not record them; see diagnosis §0).

## 2. M1.2 Evidence (threshold 15,000 ops/s)
| Source | Result |
|---|---|
| Interleaved A/B, isolated, release (`PERFORMANCE_FINAL` §2) | final design 16,353 / 17,145 / 17,610 (warm, 6 runs), 17,281 / 18,053 / 18,058 (cold, 3 runs): 9/9 ≥ 15 k |
| Normative command, 3 runs (closure §1) | 20,328 / 19,758 / 19,811 |
| Production-operations phase (closure addendum) | 17–23 k in ~27 runs, **one 12.3 k outlier** |
| **Diagnosis, this machine, 2026-10-04 (38 runs)** | 37 ≥ 15,000 (15,809–20,371; median 17.1 k), **1 = 14,618** (Phase 1 iteration 17; mean fsync 5,410 µs vs 4,149–4,544 µs in the other runs; no collected system counter differed) |
| Default `cargo test --workspace` harness | 5,772 (earlier phase, both throughput tests concurrent) — superseded as a measurement of the contract by closure §1 |
| Baseline `7b7aaaa`, same session | 9,443–12,101 (20 runs, 0 ≥ 15 k) |
Counting only the two sessions with run-level data here: 2 runs below 15,000 (12.3 k, 14.6 k) among roughly 65 current-code runs. Each such run would have failed the `assert!` of its own invocation. The cause of either run is not identified.

## 3. M1.3 Evidence (threshold 80,000 ops/s)
| Source | Result |
|---|---|
| Interleaved A/B, isolated, release (`PERFORMANCE_FINAL` §2) | 91,730 / 98,975 / 101,427 (warm), 98,555 / 100,045 / 103,118 (cold): 9/9 ≥ 80 k |
| Normative command, 3 runs (closure §1) | 111,296 / 84,604 / 103,148 |
| Production-operations phase (closure addendum, ~30 runs) | fast mode 101–113 k; **slow mode 61–79 k** |
| **Diagnosis, 2026-10-04 (58 runs)** | **58/58 met the numeric 80,000 ops/s threshold.** Range 93,533–108,235 (median 103.1 k). 57 classified FAST (≥ 95 k under the diagnosis's own classifier); 1 run (93,533) was classified AMBIGUOUS by that classifier, which is a diagnostic label, **not** a failure: it is above 80,000. That run coincided with ~81 % system disk busy and read activity that the test does not generate (source unattributed). |
| Full-workspace release runs | 61,202 / 35,531 (stray server process from a failing test) / 48,298 — see §5 |
| Baseline `7b7aaaa`, same session | 63,384–67,266 (20 runs, 0 ≥ 80 k) |
A run above the threshold is evidence of compliance **for that run**. It is not, by itself, evidence of reproducible compliance.

## 4. Historical Slow Mode
Preserved unchanged from `PHASE_RUBIXDB_WAL_CERTIFICATION_CLOSURE.md` (addendum, 2026-10-04): in an earlier session on the same machine the normative command was run ~30 times: M1.3 was bimodal, **fast mode 101–113 k, slow mode 61–79 k**, the slow mode in roughly 25–30 % of quiet-machine runs; a sampler during six runs (two slow) showed no external competitor in either mode; the slow runs took ~14 s instead of ~10.5 s. M1.2 showed one 12.3 k outlier in the same window.
Current evidence (diagnosis): 0 slow runs in 58.
> **The historical slow mode was not reproduced in the current 58-run diagnosis. Its causal trigger remains unidentified.**
This is not "fixed" and not "explained". The diagnosis rejected H1 (clock), H5 (warm-up), H6 (temp directory), H7 (ordering), H8 (affinity) for the variation it observed and left H2 (Defender, needs elevation), H3, H4, H9 inconclusive; no hypothesis was confirmed against the slow mode because it did not occur. The earlier "18/18" and "3/3" observations and the slow-mode observation are all retained; none retracts another.

## 5. Regression Reconciliation
Two distinct things:
1. **Normative WAL certification command** (`scripts/wal_certify.ps1`): the defined procedure for M1.2/M1.3 (closure §1). Evidence: §2, §3.
2. **Full workspace regression** (`cargo test --release --workspace --no-fail-fast`): a separate gate. The closure states "The general workspace regression stays separate and keeps running both tests in release" (§1.4) and does **not** declare it non-normative or exempt M1.3 from it. The repository contains no measured document that excludes the workspace run from the certification.
Latest exact evidence (`PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md`, matrix): release **1,192 passed / 1 failed / 26 ignored; the one failure is `m1_3` at 48,298 ops/s**; baseline-run workspace release: `m1_3` 61,202; debug 1,191 / 0 / 28 (throughput tests ignored in debug by contract). The test contract (closure §1: binary-wide lock, debug `#[ignore]`) was changed **before** these runs and is documented there; it was not changed in this mission. The workspace failure remains unexplained: the binary-wide lock removes overlap between throughput tests and the other tests of the `group_commit` binary, yet M1.3 still measured 48–61 k in the workspace runs (cause not investigated; the diagnosis harness ran the test binary directly, not the workspace command, so it neither confirms nor refutes this). No newer passing workspace release run exists.
**FULL RELEASE REGRESSION = FAIL** (not re-run in this mission; the status rests on the latest recorded run).

## 6. Certification Decision
| Item | Decision | Basis |
|---|---|---|
| M1.2 | **OPEN** | The test's per-invocation assertion exists; the policy that turns repeated invocations into a certification is **undefined**. Current evidence: 37/38 ≥ 15 k, one 14,618 run (a per-invocation miss), earlier 12.3 k outlier. Not PASS (a miss was recorded and no rule tolerates it); not FAIL (no rule says one miss in N is a failure, and the distribution is overwhelmingly above target); the policy gap is itself the open item |
| M1.3 | **OPEN** | 58/58 current runs ≥ 80 k; but earlier invocations at 61–79 k each missed the assertion, and the trigger is unidentified, so reproducible compliance is not established. The previous classification "FAIL (intermittent)" rests on the historical slow mode and on the workspace runs; the former is not reproduced now, the latter is covered by the next row, so M1.3 as a *normative-command* gate cannot be held at FAIL without a rule defining aggregate failure, and cannot be PASS without reproducible compliance |
| FULL RELEASE REGRESSION | **FAIL** | latest recorded release run fails `m1_3` (48,298); no documented, measured exemption; no later passing run |
| WAL CERTIFICATION CLOSURE | **OPEN** | M1.2/M1.3 OPEN, power loss untested, workspace regression FAIL |
| POWER LOSS | **NOT TESTED** | no physical power-loss test was ever performed; process kill (1.47 M oracle acks, 0 lost; 140 + 70 kill cycles) is not power loss |
| NVMe | **HARDWARE UNAVAILABLE** | both disks are SATA; no NVMe result is claimed |
No category other than PASS / FAIL / OPEN is used for the performance gates (NOT TESTED and HARDWARE UNAVAILABLE are the project's existing labels for the last two rows). **rubiXDb is NOT declared PRODUCTION READY by this mission**; it certifies only the WAL M1.2/M1.3 status, and with M1.2, M1.3, the closure and the workspace regression not PASS it cannot support that claim.

## 7. Environmental Limitations
* **Quiet machine:** targets are defined for one scenario on an otherwise idle device (closure §1). The diagnosis machine was an ordinary developer workstation (Brave, Visual Studio, etc. running), not a quiesced rig; one run coincided with unattributed disk activity.
* **Power plan:** Balanced (identical in all 120 diagnosis runs); the user's plan was not changed. Pre-run clock varied 22–116 % with no relation to ops/s in the diagnosis; earlier documents report ±30 % swings between minutes.
* **Hardware:** two SATA SSDs (`E:` = Disk 0), i7-7700 4C/8T; one SATA device serialises flushes (`WAL_ARCHITECTURE_OPTIONS` §D).
* **Process priority:** the diagnosis runs were at BelowNormal (harness-inherited), 8 extra at Normal; M1.3 unaffected, M1.2 ~5 % higher at Normal (n=8).
* **NVMe:** unavailable. **Power loss:** untestable here (no VM/hypervisor, no elevation).
* **Defender:** exclusions unviewable/unmodifiable without administrator rights (H2 untested).

## 8. Remaining Uncertainty
* The trigger of the historical M1.3 slow mode (and of the 12.3 k and 14.6 k M1.2 runs) is unknown.
* Whether the slow mode would reappear in a later session or a different machine state is unknown (the diagnosis shows it is state-dependent, not a fixed per-run probability: 0/58 at a historical ~25–30 % rate has probability < 1e-8).
* Why the full-workspace release run still fails M1.3 after the execution contract was encoded is unknown.
* The acceptance policy for repeated runs (N, aggregate statistic, outlier tolerance) is undefined; it needs a project decision, not a measurement.
* Latency distributions for the M1 runs (p50–p99.9/max) were not captured by the tests; tail latency at 256–512 writers is known only from `PERFORMANCE_FINAL` §4 (p99.9/max worse than baseline there).
* Defender's contribution, power-loss durability, and NVMe behaviour are untested/unavailable.

## 9. Supersession Record
| Earlier statement | Status after this document |
|---|---|
| `CERTIFICATION_CLOSURE` §5 "M1.2 = PASS (normative command, 3/3)" and "M1.3 = PASS (3/3)" | already superseded by its own addendum; remains superseded. Not restored |
| Closure addendum: "M1.3 = FAIL (intermittent on this hardware), M1.2 = OPEN" | **M1.3 superseded → OPEN** (the aggregate-FAIL label has no defining rule; the observation itself is preserved in §4). M1.2 = OPEN confirmed |
| Final certification matrix: "M1.3 FAIL (intermittent) … ≈ 25–30 % of quiet-machine runs land in the slow mode" | the **observation** stands as history; the **frequency claim is not supported by the current 58-run diagnosis** and must not be quoted as current behaviour; the matrix row for M1.3 is superseded by §6 |
| `PERFORMANCE_FINAL` §9 / `CERTIFICATION` §2 "M1.2/M1.3 PASS (isolated, 18/18)" | remain accurate descriptions of those 18 runs; they are not a reproducibility guarantee and are not restored as PASS |
| Final certification: full regression "FAIL", power loss NOT TESTED, NVMe HARDWARE UNAVAILABLE, NOT PRODUCTION READY | **unchanged / reaffirmed** |
Nothing is deleted or rewritten in the earlier documents; this file is additive.


---
# Update 2026-10-04 (second section) — Rule A applied; workspace M1.3 regression investigated
*The sections above are the earlier, historical statement of this document and are not edited. Where this section differs, it supersedes them (see "Supersession").*

## A. Acceptance Rule (maintainer-supplied, applied exactly)
**RULE A:** every run included in the certification set must individually satisfy the machine-enforced threshold (M1.2 ≥ 15,000 ops/s; M1.3 ≥ 80,000 ops/s). No outlier tolerance, median, percentile or K-of-N. One included run below threshold = that gate FAIL for that set. A run is excluded only if the certification procedure itself defines it invalid for a documented technical reason; no invalid-run rule was invented after seeing results.
**What Rule A does not define (stated, not filled in):** the *boundary* of a certification set. I therefore used the most inclusive set that has run-level data in the repository, excluding nothing for being low.
**Set S (all runs of the current WAL source with run-level values on file).** `git diff 3ec5034 HEAD -- src/wal` is empty, so `src/wal/` is identical for every run below (the diagnosis runs, the P1 runs and HEAD `0da9e14`; the 10-03/10-04 production-operations runs post-date `3ec5034`, 2026-10-03 20:53). One scenario per process, release, `--test-threads=1` or the equivalent test-binary invocation.
| Component of S | M1.2 runs | M1.3 runs | Below threshold |
|---|---|---|---|
| `scratch/prod_ops/wal_certify_3runs.txt` (normative command) | 3 | 3 | none |
| `scratch/prod_ops/wal_certify_10runs.txt` (normative command, `failed=5`) | 10 | 10 | M1.2: **12,305**; M1.3: **68,067; 34,059; 35,863; 51,860; 76,737** |
| `scratch/prod_ops/wal_ab_interleaved.txt` (current runs of the A/B) | 6 | 6 | none |
| `scratch/prod_ops/m13_runs_with_sampler.txt` | – | 6 | M1.3: **78,862; 76,614** |
| Diagnosis, 2026-10-04 (`scratch/wal_diag/{p1,h5,h7,prio}-cur-runs.csv`) | 38 | 58 | M1.2: **14,618** |
| This mission, TEMP comparison (`p1tmp-*`, 6 on `C:`, 6 on `E:\waltmp`) | – | 12 | M1.3: **61,195** (`C:` run 3) |
| **Total in S** | **57** | **95** | **M1.2: 2; M1.3: 8** |
**Excluded from S and why (nothing excluded for being low):** (1) the earlier aggregate tables ("18/18", 9 + 9 runs; only min/median/max on file, and the code identity before the last constant change is not confirmed): all stated ≥ target, so including them cannot change a Rule A result; (2) full-workspace runs (§C), which are a separate gate per closure §1 and are accounted there; (3) baseline `7b7aaaa` runs (different code).
**Discrepancy found in the historical record:** `wal_certify_10runs.txt` contains M1.3 values of 34,059 / 35,863 / 51,860, which are lower than the "slow mode 61–79 k" band stated in the closure addendum. The addendum under-describes its own raw file. The cause of those three runs is not recorded (it was a production-operations session with other campaigns possibly active); no documented invalid-run rule applies, so under Rule A they stay in S.

## B. Classification under Rule A
**M1.2 = FAIL** (for set S): 2 of 57 included runs are below 15,000 — 12,305 (historical, `wal_certify_10runs.txt`; it is the "12.3 k outlier" of the closure addendum) and 14,618 (diagnosis, 2026-10-04). 55 of 57 met the threshold.
**M1.3 = FAIL** (for set S): 8 of 95 included runs are below 80,000 — 34,059; 35,863; 51,860; 61,195 (new, this mission); 68,067; 76,614; 76,737; 78,862. 87 of 95 met the threshold, including **all 58 of the 2026-10-04 diagnosis runs**.
Neither result says "a performance defect was found in WAL code". Per Rule A they say that S contains runs that individually missed the machine-enforced threshold; this is a project-policy application, and no individual failing invocation is described as having passed.
*Informational only (not a classification):* the 2026-10-04 diagnosis session alone gives M1.2 FAIL (14,618 of 38) and M1.3 PASS (58/58). Rule A does not define whether sessions are separate sets; if the maintainer defines a fresh set boundary, the status would be re-evaluated on that set. A run that "passed" is evidence for that run only.

## C. Workspace regression (separate gate; not made identical to the normative command)
**Where the workspace test writes.** `tests/group_commit/support.rs::temp_dir` = `std::env::temp_dir().join("rubixdb_group_commit_it_<tag>_…")`, i.e. `%TMP%`/`%TEMP%`. Default here: `C:\Users\Ruby\AppData\Local\Temp` (Disk 1, a different SATA SSD, 89 % full). The normative runs of earlier phases set `TMP=TEMP=E:\waltmp` (Disk 0). The environment of the three recorded workspace failures is not recorded.
**Recorded history of the gate (unchanged):** release workspace, M1.3 failed in all three recorded runs (61,202; 35,531 with a stray server process from a failing test; 48,298; the last with 1,192 / 1 / 26).
**Measured in this mission (two complete `cargo test --release --workspace --no-fail-fast`, 1 Hz sampler of per-disk write latency / busy, CPU, clock, presence of `group_commit` and of other `rubixdb|python|node|chrom*` processes; raw: `scratch/wal_diag/ws-*`):**
| Run | TMP | Result | M1.2 / M1.3 | Notes |
|---|---|---|---|---|
| ws-default (15:17, 662 s) | default (`C:`) | **1,193 passed / 0 failed / 26 ignored** | both `ok` (values not printed for passing tests) | `group_commit` binary 8/8; M1.3 phase ≈ 10 s CPU-busy (consistent with a FAST run); no other watched process present |
| ws-etmp (15:28, 610 s) | `E:\waltmp` | **1,192 passed / 1 failed / 26 ignored** | both `ok` | the failure is `concurrent_first_run_processes_race_safely_to_one_owner` (`cli/tests/gui_instance_integration.rs:153`, message `a: ` empty), which passed in ws-default; **not a WAL test**, not investigated (out of scope, logged to `OPEN_ITEMS.md`) |
**Standalone P1 test (`C:` default TEMP vs `E:\waltmp`, the same test binary, workload, power plan, Normal priority, strictly interleaved, 6 + 6 M1.3 runs):**
| | ops/s | note |
|---|---|---|
| `C:` | 107,899 / 116,216 / **61,195** / 114,174 / 113,297 / 112,205 | run 3: mean fsync stage 9,859 µs (others ≈ 4,000–4,090), device `Avg. Disk sec/Write` mean **10.4 ms** (max 33 ms) vs 0.3–0.5 ms in the other `C:` runs, wall 23 s |
| `E:` | 92,055 / 105,943 / 102,052 / 101,645 / 102,177 / 102,920 | fsync stage 4,560–5,616 µs; one run (92,055) with fsync 5.6 ms |
Observation: in the single slow run the time went into the device's write/flush latency (counter and stage timing agree), not CPU (system CPU 45 % vs 56–65 %) or context switches (0.73 M vs 0.92–0.98 M/s). `C:` was faster than `E:` in 5 of 6 runs.
**P2 (workspace harness), by inspection and measurement.** The binary-wide `RwLock` is process-local; `crash_consistency` spawns child processes of the same test executable but its parent holds the shared lock until they finish, so M1.3 cannot start while children run (sampler: `group_commit` process count falls to 1 before the M1.3 CPU burst, other watched processes = 0 throughout). Cargo runs test *binaries* sequentially; the preceding binaries write heavily (lib unit tests ≈ 204 s, then `crash_consistency`, which showed `C:` write latency up to 85 ms in ws-default) — device state at the start of `group_commit` is therefore different from an isolated run. No overlap of throughput tests with other tests, no leftover process, no TEMP collision was observed. These observations explain nothing about the recorded failures, because those runs have no per-stage or counter data and did not recur.
**Classification of the recorded workspace M1.3 failure: UNRESOLVED.** P1 (TEMP location) is **not confirmed**: both TEMP settings passed the workspace M1.3 in this mission and a slow standalone run occurred on `C:` in 1 of 6 (against 0 of 6 on `E:`, a difference this small cannot be called significant). P2 found no harness defect. P3: the only mechanism measured for a slow M1.3 run is device write-latency inflation (single observation, direct counter and stage evidence); that it also caused the recorded workspace failures is not shown. "Environmental" is **not** claimed.
**FULL RELEASE REGRESSION = FAIL.** Not exempted. Evidence: 3 recorded runs failed `m1_3`; of the two runs in this mission one was fully clean (1,193 / 0 / 26) and one failed a different, non-WAL test. The gate is not met by a single clean run; no later series of clean runs exists.

## D. Unchanged gates
**POWER LOSS = NOT TESTED** (no physical power-loss test exists; process kill — 1.47 M oracle acks, 0 lost — is not power loss). **NVMe = HARDWARE UNAVAILABLE** (both disks SATA; no NVMe result is claimed).

## E. Overall
* **WAL certification: FAIL** — M1.2 FAIL and M1.3 FAIL under Rule A for set S; full release regression FAIL; power loss NOT TESTED. (Correctness evidence — ack durability, atomicity, ordering, recovery, crash consistency under process kill — is unchanged and was not re-run.)
* **rubiXDb production readiness: NOT PRODUCTION READY** (mandatory gates FAIL / NOT TESTED). This mission certifies only the WAL status.
* **Not resolved:** the trigger of slow M1.3/M1.2 runs; the cause of the recorded workspace failures; the boundary of a certification set; whether `docs/PROJECT_STATE.md` / `missions/ACTIVE.md` (required by `CLAUDE.md`, absent from the repository) will be supplied — recorded OPEN.

## F. Supersession (additive; earlier text stays in place)
| Earlier statement | Now |
|---|---|
| §6 above: "M1.2 = OPEN" and "M1.3 = OPEN" (no acceptance policy) | **superseded:** Rule A supplied → M1.2 FAIL, M1.3 FAIL for set S |
| §6 above: "FULL RELEASE REGRESSION = FAIL" | reaffirmed, with new runs (§C) |
| §6 above: "WAL CERTIFICATION CLOSURE = OPEN" | **superseded:** WAL certification = FAIL |
| §1/§6 above: "acceptance policy undefined" | superseded for the per-run requirement (Rule A); the set-boundary question remains open |
| Closure addendum band "slow mode 61–79 k" | preserved as history; the raw file also holds 34–52 k runs (see §A); not rewritten |
| "not reproduced" (diagnosis) | still true for the 58 diagnosis runs; a slow run (61,195) *was* observed on 2026-10-04 in this mission's TEMP test, so the slow mode is not absent from the current code on this machine; its trigger remains unidentified |
