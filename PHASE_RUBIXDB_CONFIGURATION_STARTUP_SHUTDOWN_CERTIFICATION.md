# rubiXDb — Configuration / Startup / Shutdown CERTIFICATION (Phase 3 of 3, read-only)

Scope chosen by the maintainer: **re-certify the lifecycle baseline** (`PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_BASELINE.md`) against the final Phase 2 build; full regression; no fixes; **no production-readiness claim**. No source, test, script or configuration file was modified in this phase; this document (plus one appended PROGRESS.md entry) is the only output. Status vocabulary: PASS / FAIL / OPEN / NOT REQUIRED / NOT TESTED / NOT IMPLEMENTED. The banned vocabulary listed in the mission is not used.

## 0. Identity of what was certified

| Item | Value |
|---|---|
| Base commit | `d80020f8c5ebe9e8b45faface801e316b995c0bb` (branch `master`) |
| Source state | **Uncommitted working tree**: the Phase 2 work (Increments A, B, C, C2, D) on top of the base commit: 26 modified + 7 untracked paths; `git diff HEAD` (tracked part) sha-256 prefix `649a5522eb33e3b5`; no commit exists for this build |
| Production binary | `target\release\rubixdb.exe`, 17,994,240 bytes, SHA-256 `a88a3873f79a6f5a748fb7fb53139490c0d2f580dee63cc0e76cc31bbe7f41b1`, built with `cargo build --release --locked -p rubixdb-cli` (rustc 1.98.1) |
| Standalone binary (observation only, not v1) | `target\release\rubixdb-api.exe`, SHA-256 `1df16ae0e6ef51967e017760bb3dc921361998dd7f384273b1329863748bf26d` |
| Frontend | existing `frontend/dist` (not rebuilt; tree hash prefix `97b2085a626c96f9`), embedded into the binary by `cli/build.rs` |
| Host | Windows 10 22H2 (19045), 4 cores / 8 threads, 15.9 GB RAM, NTFS, SATA SSDs |
| Protected paths | `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`, `Cargo.toml`, `Cargo.lock`: **zero diff** against the base commit. The only engine-crate change in the working tree is `src/relational/index.rs` (+ its test file), authorized by the maintainer (ADR-LIFECYCLE-001) |
| Binary reproducibility note | Two plain `cargo build --release` runs of this tree gave different SHA-256 (no `/Brepro`); the release script's reproducible build was not run (section 8). The certified binary is the one hashed above, and every measurement in sections 4-7 used it |

Method: black-box Python/`psutil` harness (kept outside the repository), real `rubixdb.exe`, isolated `RUBIXDB_INSTANCES_ROOT`, readiness = `/healthz` 200 + authenticated `/readyz` + authenticated `SELECT 1`; no fixed sleeps. The user's real instance (`%LOCALAPPDATA%\rubiXDb\instances\default`) was not opened. The same scripts as the baseline were re-run unchanged (one script section was obsolete, see 3.4).

## 1. Result in one table

| Area | Status | Basis |
|---|---|---|
| Format / lint / compile (`cargo fmt --check`, `clippy -D warnings`, `check`) | PASS | all three exit 0 |
| Full workspace regression (debug and release) | **FAIL** | 3 distinct failing tests, none in this phase's scope to fix (section 2) |
| Baseline findings closed | 14 of 19 closed with evidence (F-13 and F-19 each keep a residual) | F-01..F-06, F-09, F-12..F-17, F-19; section 3 |
| Baseline findings still open | 4 (F-07, F-08, F-11, F-18); F-10 is NOT REQUIRED for v1 (standalone); 2 residuals | section 3 |
| Lifecycle certification matrix (35 rows) | see section 9 | PASS 22, FAIL 1, OPEN 3, NOT TESTED 9 |
| **Production readiness** | **NOT DECLARED** | CLAUDE.md requires every mandatory row PASS; there is a FAIL, OPEN and NOT TESTED rows (including power loss) |

## 2. Regression (final tree, final binary)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS (exit 0) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS (exit 0) |
| `cargo check --workspace --all-targets --all-features` | PASS (exit 0) |
| `cargo test --workspace --no-fail-fast` (debug) | 1,287 passed, **2 failed**, 28 ignored |
| `cargo test --release --workspace --no-fail-fast` | 1,287 passed, **4 failed**, 26 ignored |
| `cargo build --release --locked -p rubixdb-cli -p rubixdb-api` | PASS (exit 0) |

Failing tests, each analysed:

| Test | Runs | Cause / evidence | STATUS |
|---|---|---|---|
| `tests/repo_hygiene.rs::no_tracked_credentials_json` and `::no_tracked_file_contains_a_64_hex_admin_key_literal` | fail in debug and release | `frontend/.e2e-crossbrowser-data/default/credentials.json` (a burnt, throwaway key) is still tracked at HEAD; OPEN_ITEMS 2026-10-04 (untrack is uncommitted). Re-verified on a clean checkout of the base commit with the Phase 2 change stashed (Increment A): same 2 failures | FAIL (pre-existing; not a lifecycle defect) |
| `group_commit::m1_3_thousand_writers_throughput` | fails in the release run | documented intermittent WAL throughput test (engine, protected). On the clean base commit it failed 1 of 4 isolated runs (Increment B); with Phase 2 it passed isolated 3 of 3 and passed full-suite runs in Increments C2 and D | FAIL (intermittent, pre-existing; engine, out of scope) |
| `cli/tests/index_backfill_crash_integration.rs::a_graceful_stop_during_recovery_cancels_it_quickly_and_the_next_start_finishes_it` | failed once, in the final **release** full-suite run (assertion at the first `/readyz` read: the `index_recovery` key was absent from the answer, i.e. not a readiness body) | **A Phase 2 test (mine, Increment C2).** Not reproduced: 5 of 5 isolated runs and 4 of 4 runs with six CPU-hog processes passed; it also passed in the Increment C2 and D full runs. Root cause **not established**. Hypothesis, unproven: the shared helper `start_owner_and_wait_ready` reads the manifest port before the restarted owner may have rewritten it, and the three tests in that file run in parallel while competing for port 302, so the first request can reach a different test's server. This is a defect in the test (or its helper), not evidence of a product defect, but it is a failing test in the suite | **FAIL (new, intermittent, cause unproven)** |

No test was changed in this phase.

## 3. Baseline findings: disposition on the final build

Every baseline finding (`PHASE_RUBIXDB_CONFIGURATION_STARTUP_SHUTDOWN_BASELINE.md` section 14) re-checked by execution on the certified binary where it can be.

| ID | Baseline finding | Now | Evidence (final build) | STATUS |
|---|---|---|---|---|
| F-01 | `gui` ignored `RUBIXDB_INSTANCE_NAME` | env honoured, flag beats env | harness P2a/P2b: env only opens the named instance; flag + env opens the flag's | PASS |
| F-02 | lax `gui` arguments | strict | `--instance` without value / followed by a flag / repeated / unknown option: exit 1, nothing created (P7) | PASS |
| F-03 | silent fallbacks (rate-limit env, retry budget, frontend dist) | validated | P6b/c/d/e/f/g/h/i, P4c/d, P10 budget `abc/-5/overflow`: exit 1 naming the variable; empty = unset kept (P6j, P4e, P10 empty) | PASS |
| F-04 | weak/empty credential accepted | refused | P9 empty / 3-char / spaced key: exit 1, message names `credentials.json` and `rotate-credential`, no key printed | PASS |
| F-05 | `RATE_LIMIT_BURST=0` lockout | rejected (v1 and standalone) | P6c/g, G18/G19/G28/G29 | PASS |
| F-06 | stop waits for index recovery without bound (11-17 s) | cancelled at next chunk | 400,000 rows: stop 0.11 / 0.11 / 0.11 s, exit 0, 0 leftovers; next start restarts the index (`building` right after ready; completes, row count 400,000) | PASS |
| F-07 | WAL final-record damage opens silently, acknowledged rows lost | **unchanged** | P9 torn tail and bit-flip: opens, `COUNT(*)` 0 of 50, no warning (header damage is refused) | OPEN (engine ADR territory; protected) |
| F-08 | damaged SSTable: ready but 500 on some reads | **unchanged** | P9/H2: `STORAGE_ERROR` HTTP 500 on `COUNT(*)`, PK lookup 200 | OPEN |
| F-09 | `instance status` said "not running" for a held lock | `locked (...)` | P10 status; integration test with a real OS lock | PASS |
| F-10 | standalone binary binds after opening the engine | unchanged, standalone is not v1 | G35 exit 1 | NOT REQUIRED (v1) |
| F-11 | 30 s readiness bound is not a bound on exit; WAL replayed twice | **unchanged**, not reproduced | no large WAL produced | OPEN |
| F-12 | `/readyz` could not report recovery | `index_recovery` field | T8 runs; integration tests | PASS |
| F-13 | unclear path / manifest / URL errors | path-bearing | P8c/d/e/g, P7 reserved names, P9 manifest, `RUBIXDB_API_URL=notaurl` / `http://` fail in 0.03 s naming the variable. **Residual:** P8f (a ~370-character root) still prints the raw `os error 123` without the path; `RUBIXDB_API_URL=http://127.0.0.1:1` still reports a transport error after 2.06 s | PASS (main cases) / OPEN (2 residuals) |
| F-14 | stale `<hash>.tmp-<pid>` staging folder never pruned | pruned incl. cached path | unit test of the prune logic; a real leftover from a kill mid-unpack was not produced | PASS (logic) / NOT TESTED (real kill) |
| F-15 | `DEFAULT` never returned to 302 | returns to 302 | H4: fresh 302, 302 busy -> 56219, 302 free again -> 302 | PASS |
| F-16 | only Ctrl+C graceful | Ctrl+Break graceful; others handled | real events: Ctrl+Break 10/10 exit 0 in 0.004-0.005 s with `shutting down (Ctrl+Break)`, Ctrl+C 10/10, launcher-disabled Ctrl+C respected and `instance stop` works. Console close / logoff / shutdown cannot be generated here | PASS (Break, C) / NOT TESTED (close, logoff, shutdown) |
| F-17 | manifest `name` unvalidated | validated against its directory | P9 `name != dir`, `../../x`: exit 1 naming instance.json | PASS |
| F-18 | `RUBIXDB_API_URL` accepts any host / plaintext | **unchanged** (policy undecided) | only syntax is checked | OPEN |
| F-19 | failed attach probe ~2.1 s | 0.53 s | P10 budget 0 and 300 ms: gui 0.53 s, cli 0.55 s (baseline 2.12 / 2.11 s). **Residual:** `rubixdb instance stop` against a silent owner 2.08 s; unroutable URL ~21 s | PASS (probe) / OPEN (2 residuals) |

## 4. Re-run of the configuration, precedence and validation rows

**Precedence (one run per scenario, final binary):** unchanged except F-01. Instances root: `RUBIXDB_INSTANCES_ROOT` > `LOCALAPPDATA` > `APPDATA` > error (P1a-e PASS). `gui`: `--instance` > `RUBIXDB_INSTANCE_NAME` > `default` (P2a/b PASS). Client: env name > `default` (P2c/d PASS); `instance status NAME` positional (P2e PASS). Server: `RUBIXDB_API_URL` > auto-discovery; key ignored without URL (P3a-d PASS). Console: valid `RUBIXDB_FRONTEND_DIST` > embedded; **an invalid value is now an error** (P4a/b/e PASS; P4c/d exit 1, no fallback). Port: unchanged (P5a-d PASS; no listen/port environment variable exists, `RUBIXDB_LISTEN_ADDR=127.0.0.1:9999` ignored, only `127.0.0.1:302` listens).

**Validation rows that changed** (baseline V-id -> now): V-11/V-12/V-13 (`gui` arguments) ACCEPTED -> EARLY errors; V-19 manifest `name` ACCEPTED -> EARLY; V-23/V-24/V-25 credential PARTIAL/ACCEPTED -> EARLY; V-35..V-40 rate-limit and retry-budget FALLBACK/ACCEPTED -> EARLY (empty stays "unset"); V-42 `RUBIXDB_FRONTEND_DIST` FALLBACK -> EARLY; V-43/V-44 URL syntax errors now immediate; V-04..V-06, V-10 root and reserved-name errors now carry the path and the setting; V-39 lockout -> rejected; V-49 `instance status` now accurate.

**Validation rows unchanged (re-confirmed):** root relative `relroot` and traversal `..\escape_root` are still **accepted** and create the instance there (P8a/b; baseline V-02/V-03; `RUBIXDB_INSTANCES_ROOT` has no validation by design: OPEN as a policy question, not a regression); `api_port` 0 / 5 / 65535 accepted, 70000 / -1 / `"abc"` rejected (P5d); `DATA_FORMAT` 99 / garbage refused, deleted accepted (P9); WAL header damage refused; WAL tail damage, engine-MANIFEST truncation (rows intact) and SSTable damage behave as in the baseline (F-07, F-08); `-c`/`-f`/`instance` operand errors unchanged (P3i-k, P7).

**Values with no explicit validation (v1) now:** 1. `RUBIXDB_INSTANCES_ROOT`; 2. `RUBIXDB_API_URL` host/scheme policy (syntax only); 3. `RUBIXDB_API_KEY` (no format check; it is only a client-side secret); 4. `instance.json` `api_port` 0 / privileged values; 5. `--data-dir` of `check`/`restore` (not exercised). That is **5** (baseline: 12). Standalone `rubixdb-api` (not v1): every numeric setting that accepts `0` still does except the rate-limit pair (`MAX_VALUE_BYTES`, `MAX_KEY_BYTES`, range limits, drain, compaction count, session limits and deadline accept `0`; G13..G25).

## 5. Startup, readiness, failure, shutdown, forced termination (final build)

STATUS of every row in this section: PASS (measured, no acceptance threshold exists in the repository; none is asserted).

### 5.1 Startup-to-ready (seconds from process spawn; min / p50 / p95 / p99 / max; READY = first authenticated SQL 200)

| scenario | N | TCP accept p50 | READY min / p50 / p95 / p99 / max | baseline READY p50 |
|---|---|---|---|---|
| new instance (empty) | 20 | 0.052 | 0.072 / 0.078 / 0.102 / 0.653 / 0.653 | 0.093 |
| restart after clean shutdown, 1,010 rows | 20 | 0.060 | 0.049 / 0.063 / 0.081 / 0.082 / 0.082 | 0.095 |
| restart after forced termination, 1,010 rows (112 KB WAL tail) | 20 | 0.060 | 0.050 / 0.065 / 0.094 / 0.264 / 0.264 | 0.077 |
| restart after clean shutdown, 200,010 rows (28 MB) | 10 | 0.060 | 0.113 / 0.124 / 0.137 / 0.137 / 0.137 | 0.116 |
| restart after forced termination, 200,010 rows (3.5 MB WAL tail) | 10 | 0.044 | 0.122 / 0.140 / 0.382 / 0.382 / 0.382 | 0.121 |
| fresh app-data dir (console unpacked), new instance | 10 | - | p50 0.085, max 0.111 | 0.101 |
| second `rubixdb gui` (attach) | 20 | - | 0.030 / 0.035 / 0.039 / 0.039 / 0.039 | 0.031 |
| `rubixdb -c` attach | 20 | - | 0.033 / 0.038 / 0.043 / 0.044 / 0.044 | 0.038 |
| `rubixdb -c` becomes owner (existing data / new) | 20 / 10 | - | 0.062 / 0.084 whole process | 0.055 / 0.078 |
| after an interrupted `CREATE INDEX` (400,000 rows), 5 runs | 5 | - | ready 0.093-0.125 s with the index `building`; index `ready` 14.9-17.9 s after spawn (baseline 11.5-15.7); 13-17 correct point queries answered while building; 400,000/400,000 rows | 0.09-0.17 |

Outliers (0.653, 0.382, 0.264 s) are single samples at N = 10-20 and are reported, not trimmed. Index-recovery duration varies between runs of identical code on this host (13.7-22.1 s over earlier runs); no speed claim is made. "Cold OS file cache" start: NOT TESTED.

### 5.2 Shutdown and forced termination

| path | N | result |
|---|---|---|
| graceful admin stop, request -> exit | 20 / 10 per scenario | p50 0.072-0.103 s, p95 0.111-0.121 s, max 0.122 s, exit code 0 in every run |
| Ctrl+C | 10 + 100 cycles | 0.004 / 0.004 / 0.005 s p50/p95/max; exit 0 |
| Ctrl+Break | 10 | 0.004 / 0.004 / 0.005 s; exit 0; rows intact on restart (restart first SQL p50 0.067 s) |
| `rubixdb instance stop` | 10 | 0.133 / 0.137 / 0.142 s wall |
| stop **while an index recovery runs** (400,000 rows) | 3 | 0.11 / 0.11 / 0.11 s, exit 0, 0 leftovers, next start restarts the index (baseline 11.0-17.2 s) |
| forced termination (`TerminateProcess`) | 10 + 100 cycles | process gone in 0.002 s; restart first SQL p50 0.057-0.060 s; **all acknowledged rows present** (1,010/1,010; 200,010/200,010; 3/3) |
| `taskkill` without `/F` | 1 | refused by the OS; process keeps running (console process) |
| launcher-disabled Ctrl+C | 1 | ignored (respected); `instance stop` stops it cleanly |

**Power loss: NOT TESTED** (no way to cut power here). Forced termination above is a process kill, not a power loss.

### 5.3 Resources and orphans (100 cycles per family, a fresh process each cycle)

| family | READY p50 / p95 / p99 | RSS MB first -> last (min-max) | threads | handles | sockets | stop or kill p50 / p95 / p99 / max | leftover procs / listeners on 302 / `*.tmp` | root entries |
|---|---|---|---|---|---|---|---|---|
| admin stop, empty | 0.063 / 0.084 / 0.147 | 10.7 -> 10.0 (10.0-10.7) | 17-18 | 107-130 | 1 | 0.091 / 0.114 / 0.115 / 0.197 | 0 / 0 / 0 | 13 -> 13 |
| admin stop, with data | 0.065 / 0.083 / 0.105 | 10.6 -> 10.7 (10.6-10.8) | 18 | 107-111 | 1 | 0.077 / 0.105 / 0.107 / 0.108 | 0 / 0 / 0 | 13 -> 13 |
| forced kill, with data | 0.060 / 0.080 / 0.088 | 10.6 -> 10.6 (10.6-10.8) | 17-18 | 107-111 | 1 | 0.003 / 0.003 / 0.003 / 0.004 | 0 / 0 / 0 | 13 -> 13 |
| Ctrl+C, with data | 0.060 / 0.087 / 0.100 | 10.6 -> 10.6 (10.6-10.9) | 17-18 | 107-111 | 1 | 0.004 / 0.006 / 0.006 / 0.006 | 0 / 0 / 0 | 13 -> 13 |
| CLI owner `rubixdb -c` | whole process 0.047 / 0.051 / 0.081 | - | - | - | - | - | 0 / 0 / - | 12 -> 12 |

One long-running instance under 600 attach / connection cycles (20 s): RSS 12.14 -> 12.98 MB (+0.04 MB per 100 cycles), threads 18 constant, handles 130 -> 132, 1 listening socket, 0 established. Thread count in a fresh process is 17 or 18 (baseline: always 18); not investigated (the runtime's worker count, not a leak: it does not grow within a run or across cycles). The measurements say nothing about hours of operation; the certified endurance documents are not re-run here (NOT TESTED).

Orphan result: **no leftover `rubixdb` process, no listener on 302 and no staging file after any start, stop, kill or signal in this phase** (more than 700 lifecycle events across the runs above).

## 6. Security properties re-checked (no weakening observed)

| Property | Evidence | STATUS |
|---|---|---|
| bind address loopback only; no setting can change it | only `LISTEN 127.0.0.1:302` for the process; `RUBIXDB_LISTEN_ADDR` / `RUBIXDB_PORT` ignored | PASS |
| default 302 decimal | every default-instance run; `instance.json` `api_port` 302 | PASS |
| identity handshake required before trusting a held lock | P10: held lock, nobody answering -> refused, never attached, lock never broken; status `locked` | PASS |
| credential never printed | rejection messages contain no key (P9, unit and integration tests); `RUBIXDB_API_URL` errors never echo the value | PASS |
| credential file ACL owner + SYSTEM only (fresh file) | `icacls` on a newly generated file (Phase 1 baseline); the credential save path is unchanged in Phase 2 apart from validation; re-checked by the unit test of the DACL | PASS (logic) / NOT TESTED (re-`icacls` on the final binary) |
| standalone API not reintroduced into v1 | not packaged by `scripts/release.ps1`; observed only on loopback port 34567 | PASS |
| unusable credential / manifest cannot start an instance | P9 | PASS |
| a held lock is never advised to be deleted | message text; integration test | PASS |

## 7. Failures, open items, not tested (consolidated)

**Known failures (FAIL):** `repo_hygiene` x2 (tracked burnt credentials file; pre-existing); `m1_3_thousand_writers_throughput` (intermittent, engine; pre-existing); the C2 stop-during-recovery integration test (intermittent, cause unproven; new).

**Open (OPEN):**
1. F-07 WAL final-record damage opens silently and drops acknowledged rows; needs an engine ADR decision.
2. F-08 a damaged SSTable still reports ready; reads of damaged blocks fail with 500.
3. F-11 readiness bound is not a bound on process exit; WAL is replayed twice at startup; unmeasured at large WAL sizes.
4. F-18 `RUBIXDB_API_URL` host/scheme policy.
5. Residuals of F-13 (~370-character root error without the path; transport error text for a refused URL) and F-19 (`instance stop` to a silent owner 2.08 s; unroutable URL ~21 s).
6. `RUBIXDB_INSTANCES_ROOT` relative / `..` values are accepted by design (policy question).
7. A recovery interrupted by shutdown restarts from scratch at every start (by design); repeated stops can starve a very large index.
8. The `waiting_for_index_recovery` field name predates the cancellation fix.
9. The flaky C2 test (section 2) needs a root cause.

**Not tested (NOT TESTED):** power loss; cold OS file cache; disk full / read-only volume; startup with a ~64 MiB WAL segment, many SSTables or large catalogs; stop latency during recovery above 400,000 rows; console close / logoff / system-shutdown events; a staging folder genuinely left by a kill mid-unpack; the release script and the packaged-artifact lifecycle (`scripts/release.ps1` writes `dist-release\` and rebuilds the frontend; also provides the reproducible `/Brepro` build); successful `rotate-credential` / `drop` re-run on this build; interactive TTY paths (gui menu, key prompt); browser launch (`--no-browser` used throughout); non-Windows platforms; the standalone binary beyond its validation; endurance over hours; the frontend in a browser against this build (the GUI Playwright suites were not run in this phase).

**Not implemented (by design, source):** configuration file, `--port` / `--listen` / `--data-dir` flags for `gui`, daemon/service mode, top-level `start`/`stop`, credential revocation (covered by `rotate-credential`).

## 8. Documents and process

* The baseline document is unchanged (historical). Phase 2 recorded its increments in PROGRESS.md, CHANGELOG.md and OPEN_ITEMS.md (append-only) and ADR-LIFECYCLE-001.
* This phase appended one entry to PROGRESS.md and created this file; OPEN_ITEMS.md and CHANGELOG.md were not changed (no product change).
* Nothing is committed or pushed. The certified state exists only as an uncommitted working tree; **committing it is a decision for the maintainer** and would also be the moment to untrack `frontend/.e2e-crossbrowser-data/default/credentials.json`, which clears the two hygiene failures.

## 9. Certification matrix (mandatory lifecycle rows)

Each row exactly one status. "Mandatory" = needed before PRODUCTION READY can be declared for the lifecycle area.

| ID | Row | STATUS |
|---|---|---|
| L-01 | `cargo fmt` / `clippy -D warnings` / `check` clean | PASS |
| L-02 | Full workspace regression has no failing test | **FAIL** (section 2) |
| L-03 | Default endpoint 127.0.0.1:302, loopback only, not configurable | PASS |
| L-04 | Every externally supplied v1 value fails early with a clear error or is documented as accepted | PASS (5 documented unvalidated values, section 4) |
| L-05 | No silent fallback for supplied values (empty = unset is the documented exception) | PASS |
| L-06 | Credential file validated; unusable credential refused; no secret in output | PASS |
| L-07 | Invalid instance / manifest / root / port values: no partial state, no orphan, lock and port released | PASS |
| L-08 | Configuration precedence verified by execution | PASS |
| L-09 | Readiness: product-internal readiness, `/healthz`, `/readyz`, handshake agree; recovery observable | PASS |
| L-10 | Startup failure paths clean (clear message, nothing left) | PASS |
| L-11 | Startup after clean shutdown | PASS |
| L-12 | Startup after forced termination, acknowledged data intact | PASS |
| L-13 | Startup after interrupted index build; stop during it bounded | PASS |
| L-14 | New-instance and existing-instance attach | PASS |
| L-15 | Concurrent start race (one owner) | PASS (covered by the suite's `gui_instance_integration` tests, passing in the full runs; not re-driven by the harness) |
| L-16 | Port collision falls back and never trusts an unrelated process (identity handshake) | PASS |
| L-17 | Instance ownership conflict (held, silent lock): refused, status accurate, lock never broken | PASS |
| L-18 | Graceful shutdown via Ctrl+C, Ctrl+Break, admin API, `instance stop` | PASS |
| L-19 | Graceful shutdown via console close / logoff / system shutdown | NOT TESTED |
| L-20 | Forced termination separated from graceful termination | PASS |
| L-21 | Repeated start/stop (100 cycles x 4 families): no process / listener / file / handle / thread growth | PASS (cycle-level only) |
| L-22 | Concurrent stop behaviour | NOT TESTED (no dedicated two-simultaneous-stop run in this phase) |
| L-23 | Long-run resource behaviour (hours) | NOT TESTED |
| L-24 | Power loss | NOT TESTED |
| L-25 | Disk full / read-only volume at startup | NOT TESTED |
| L-26 | Cold OS cache startup | NOT TESTED |
| L-27 | Large-state startup (large WAL / many SSTables) | NOT TESTED |
| L-28 | Corruption visibility: WAL tail damage and SSTable damage are surfaced to the operator at open | OPEN (F-07, F-08) |
| L-29 | Readiness bound equals a bound on process exit | OPEN (F-11) |
| L-30 | Release script and packaged-artifact lifecycle | NOT TESTED |
| L-31 | `RUBIXDB_API_URL` host policy | OPEN (F-18) |
| L-32 | Standalone API not part of v1 | PASS |
| L-33 | Frontend served from the embedded console and from a valid override | PASS |
| L-34 | Frontend behaviour in a browser against this build | NOT TESTED |
| L-35 | Protected engine paths unchanged | PASS |

Count of the 35 rows: **PASS 22, FAIL 1, OPEN 3, NOT TESTED 9, NOT REQUIRED 0, NOT IMPLEMENTED 0** (22 + 1 + 3 + 9 = 35).

## 10. Decision

**rubiXDb is NOT declared PRODUCTION READY by this document.** The lifecycle layer (configuration, startup, readiness, shutdown, signals, orphan prevention) meets its documented contract on every row that could be executed here; the baseline's two FAILs (credential and rate-limit lockout) and the unbounded shutdown are closed. Production readiness additionally requires: a clean regression (L-02), a decision on L-28 (damage visibility), the untested rows above (power loss, disk full, large-state startup, cold start, hours-long run, packaged release, browser), and a committed, reproducibly built artifact. Phase 3 stops here; no further phase is started.
