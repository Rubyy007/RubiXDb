# PHASE RUBIXDB — FINAL SINGLE-NODE CERTIFICATION

**Date:** 2026-10-02 · **Product:** rubiXDb (`rubixdb gui`, `rubixdb cli`, default `127.0.0.1:302`) · **Base HEAD:** `b7f0e8b`; this phase's fixes landed in `7b7aaaa`.
**Supporting documents:** `..._ARCHITECTURE.md`, `..._PERFORMANCE.md`, `..._SECURITY.md`, `..._RELIABILITY.md`, `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`.
Historical Increment 13-18 records were not rewritten. Evidence tags: **[RUN]** executed this session, **[SUITE]** automated test passed this session, **[PRIOR]** earlier record, not re-run.

## 1. FINAL DECISION

> **RUBIXDB PRODUCT SURFACE = NOT DECLARED PRODUCTION READY — certification status: ENGINE-BLOCKED.**

* Every *product-surface* gate that could be evidenced is **PASS** (API, CLI, GUI, instance manager, GUI/CLI consistency, restart, crash recovery, security, read/aggregate/SQL-write behavior, 5.76 h endurance on current code).
* **One mandatory requirement cannot be met by the certified engine:** WAL group-commit throughput targets **M1.2 (>=15,000 ops/s)** and **M1.3 (>=80,000 ops/s)** fail in debug and release, reproducibly, bounded by SATA fsync latency (`PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`). Per the mandate: `src/wal/` was **not** modified, no threshold was changed, and no PASS is claimed.
* Therefore the "no mandatory gate FAIL/OPEN" condition for declaring PRODUCTION READY is **not satisfied**. This is a performance-target block, **not** a correctness or durability defect: every WAL/crash/recovery/consistency test passes.
* Also open, non-engine: license scan not performed (`cargo-deny` not installed), and one committed throwaway test credential (§5). These do not change the decision above but must be closed before any production declaration.

Reopening paths (need your authorization; nothing started): (a) re-measure the unmodified WAL on NVMe-class storage; (b) an ADR for the relational per-table commit lock / sharded WAL.

## 2. Failure classification (not collapsed)
Final regression, **post-fix**, `--no-fail-fast`: **debug 1,113 passed / 2 failed / 26 ignored; release 1,113 passed / 2 failed / 26 ignored.** (Pre-fix: debug 1,107/2/26 — originally recorded as 3 failures — and release 1,107/2/26. The +6 are new regression tests.) The 26 ignored are the long soak/measurement tests intentionally `#[ignore]`d (unchanged).

| Class | Items |
|---|---|
| **OUR CODE FAILURES** | **0 remaining.** Found and fixed this phase: **D-1** (SQL operator-chain guard false-rejected valid statements), **D-2** (CLI sanitizer missed C1 controls). Each reproduced, root-caused, fixed, regression-tested, re-verified on the real binary. |
| **TEST-HARNESS DEFECT (fixed)** | **D-0** `two_instances_simultaneous_...`: `reqwest::blocking` in a tokio test panicked in debug **and leaked real server processes** (no `Drop`). Fixed; passes debug+release, 0 leaks. Not a product defect; no release artifact affected; assertions unchanged. |
| **PRE-EXISTING** | WAL M1.2/M1.3 failures (documented since 2026-09-14; verified again here against current HEAD) — classified below as ENGINE-BLOCKED, **not** as "non-blocking". |
| **ENVIRONMENTAL** | *Variance*, not the cause: isolated runs 8.6-11.7k (M1.2) / 63-64k (M1.3) vs 5.6-6.0k / 32-39k release (and 718-842 / 4.9-5.8k debug) when run inside the parallel `cargo test` — the two M1 tests and others contend for one SATA disk. Even the best isolated run misses target. Dev-machine apps (Brave, VS) were open. |
| **ENGINE-BLOCKED REQUIREMENTS** | **M1.2** and **M1.3** WAL throughput (ADR). Plus a *separate, non-engine* observation: same-table SQL commits cap at ~270/s (relational per-table lock held across fsync; scales 7.4x over 16 tables) — characterized, not changed. |

## 3. FINAL CERTIFICATION MATRIX

**CORE ENGINE**
| Item | Result | Basis |
|---|---|---|
| WRITE ENGINE | **ENGINE-BLOCKED** | durability/recovery certified and passing; M1.2/M1.3 throughput targets unmet |
| READ ENGINE | **CERTIFIED** | read/recovery suites pass; plateau localized to CPU-bound index fetch, storage *not proven* responsible |
| COMPACTION | **CERTIFIED** | suites pass; auto-compaction active through 3 endurance segments, SSTables <= 3 |
| RELATIONAL CATALOG | PASS | suites + live `\l \ls \lt \d \di` |
| ROW STORAGE | PASS | suites; endurance integrity (0 orphans, counts exact) |
| INDEXES | PASS | UNIQUE enforced (409); crash test; index reads verified |
| TRANSACTIONS | PASS | Snapshot Isolation demonstrated incl. **write skew (documented contract)**; same-row conflict 409 |
| SQL PARSER/BINDER | PASS | after D-1; limits/fuzz/security suites |
| PLANNER | PASS | suites + EXPLAIN |
| OPTIMIZER | PASS | cost-based access path (Inc 17/18 not reopened); suites |
| READ EXECUTOR | PASS | probes + sweep |
| WRITE EXECUTOR | PASS | probes + sweep + endurance |
| AGGREGATION | PASS | GROUP BY/HAVING/aggregates executed; `COUNT(DISTINCT)` NOT IMPLEMENTED (safe error) |

**PRODUCT**
| Item | Result | Basis |
|---|---|---|
| API | PASS | all routes exercised; auth/limits/fuzz; cancellation+deadline tests |
| CLI | PASS | after D-2; meta-commands real; 300-run churn |
| GUI | PASS | 21 e2e + 8 real-product + 3 cross-browser + 2 XSS Playwright tests; 34 unit |
| INSTANCE MANAGER | PASS | ownership, attach-not-duplicate, ports, identity |
| GUI/CLI CONSISTENCY | PASS | **[RUN]** schema/table/index/rows created via HTTP (GUI path) visible in CLI and vice-versa, same durable state |
| RESTART | PASS | **[RUN]** persisted catalog/rows/indexes across restart; **[SUITE]** `gui_instance_integration` |
| CRASH RECOVERY | PASS | **[RUN]** `taskkill /F` mid-txn: same identity/data, txn rolled back; +2 endurance hard-kills |

**SECURITY**
LOOPBACK **PASS** · FILESYSTEM **PASS** · PROCESS **PASS** · SQL INJECTION **PASS** · XSS **PASS** · CLI TERMINAL SAFETY **PASS** (after D-2) · RESOURCE EXHAUSTION **PASS** · CREDENTIAL SAFETY **PASS** (flag: §5) · DELETE SAFETY **PASS** (schema/table/index; database/instance delete NOT IMPLEMENTED)

**PERFORMANCE**
PK LOOKUP **PASS** · PK RANGE **PASS** · SECONDARY INDEX **PASS** · SEQSCAN **PASS** · JOIN **PASS** · AGGREGATION **PASS** · **WRITE = ENGINE-BLOCKED** · READ CONCURRENCY **PASS** (characterized, not engine-blocked) · GUI **PASS** · CLI **PASS**

**RELIABILITY**
STARTUP **PASS** · SHUTDOWN **PASS** · STARTUP RACE **PASS** · INSTANCE CRASH **PASS** · DATABASE CRASH **PASS** · INDEX CRASH **PASS** · COMMIT ACK LOSS **PASS** · CANCELLATION **PASS** · DEADLINE **PASS** · **ENDURANCE PASS** (3 x 115 min on current code) · MULTI-INSTANCE **PASS**

**QUALITY**
DEBUG REGRESSION **FAIL** (2 tests, both ENGINE-BLOCKED M1.2/M1.3; 0 other failures) · RELEASE REGRESSION **FAIL** (same 2) · FMT **PASS** · CLIPPY **PASS** · CHECK **PASS** · FUZZING **PASS** (existing suites + manual probes; no new campaign) · DEPENDENCY SECURITY **PASS for advisories** (Rust 0 vulns; npm prod 0 after react-router 7 upgrade; **no license scan**)

## 4. What changed in this phase
`cli/tests/multi_instance_sustained_load.rs` (D-0) · `sql/src/parse.rs` + `limits_tests.rs` (D-1) · `cli/src/render.rs` (D-2) · `frontend/package*.json` (react-router-dom 6 -> 7, closes 2 prod advisories) · `frontend/e2e/xss_safety.spec.ts` (new). **Zero diff** in `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/` (checked before work, mid-way and at the end). No tests weakened, no thresholds changed, no concurrency reduced, endurance not shortened.

## 5. Open items and honest limits
1. **ENGINE-BLOCKED:** WAL M1.2/M1.3 (needs your decision on §1 paths).
2. **Committed credential:** `frontend/.e2e-crossbrowser-data/default/credentials.json` (+ WAL/MANIFEST/lock) tracked since `2afa0e1`; throwaway key, not removed (repo decision).
3. **License scan not done** (`cargo-deny` absent).
4. **Not re-run this session:** Increment-14 heap-ownership (dhat) memory analysis — no "no memory leak" claim is made, only flat-trend evidence; foreign-process-on-port-302 collision (the endurance instance held 302; covered by suite only); N-way concurrent-delete stress; cross-browser run of the XSS spec (Chromium only); coverage-guided fuzzing.
5. **Endurance caveats:** binary was built from `b7f0e8b` (pre D-1/D-2; neither is on the workload's path); developer activity overlapped roughly the first hour of segment 1; 293 snapshot-isolation conflicts, all in segment 1.
6. **Minor UX/hardening notes:** `ROW_NUMBER() OVER` is rejected as `404 NOT_FOUND` (misleading code); `/healthz` ignores the Host header; dev-only npm advisories (esbuild/vite/vitest) remain.
7. **Increment 18's idle-CPU read plateau** was not reproduced (workloads here saturate CPU); left as recorded.

## 6. Stop condition
Certification complete. **Not started:** subqueries, CTEs, UNION/INTERSECT/EXCEPT, window functions, Router, Replication, Partitioning. The next feature phase requires separate authorization.
