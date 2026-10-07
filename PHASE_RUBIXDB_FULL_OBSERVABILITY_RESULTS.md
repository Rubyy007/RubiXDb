# rubiXDb — Full Observability RECONCILIATION RESULTS

**Date:** 2026-10-06. **Mission:** "Observability Reconciliation & Policy Resolution" — read-only reconciliation of the already-implemented observability layer. **Nothing was implemented, no production source or test was modified, no commit was made.**

**Scope:** observability only. This document does not certify WAL performance, product readiness or rubiXDb production readiness, and it does not modify `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` (the closure addendum belongs to the next prompt). Companion: `PHASE_RUBIXDB_FULL_OBSERVABILITY_ARCHITECTURE.md` (as-built architecture).

## 0. Identity of what was reconciled

Commands run first, before anything else (outputs verbatim):

```
$ git status --short
(no output: clean working tree)
$ git branch --show-current
master
$ git rev-parse HEAD
2cbd5a7e2b1891898c34c58e2adb8df57a9c3acb
$ git log -10 --oneline --decorate
2cbd5a7 (HEAD -> master, origin/master, origin/HEAD) observability: sampler, system metrics, time series, diagnostics, status --system
8e47379 docs: Phase 2 progress/changelog/open items and Phase 3 certification
6bf7d91 lifecycle: strict config validation, observable recovery, stop signals (Phase 2 A-D)
3cb134f relational: cooperative cancellation of startup index recovery (ADR-LIFECYCLE-001)
6f6d243 docs: Phase 1 configuration/startup/shutdown baseline (read-only)
d80020f fix: CLI first-run race (attached client lost its owner) and test-harness hang
c8c0f3a fix: unpack the embedded console under app data, not inside the instances root
fc424e1 frontend: new shell, Home page and SQL Console redesign
d602cf9 docs: WAL M1.2/M1.3 diagnosis, final certification text and diagnostic scratch data
a40fcf8 security: Phase 7 gap closure, single-file console binary, default port 302
$ git diff --stat
(no output)
```

Consequence: the observability layer certified as an *uncommitted working tree* in the certification document is now commit `2cbd5a7` (parent `8e47379`). The certification's identity block ("uncommitted working tree", binary `fe511ff5…`) therefore describes a state that no longer exists as such.

**Binaries measured in this session** (all outside the repository, `E:\rubixdb_recon\`):

| Role | File | Size | SHA-256 | How built |
|---|---|---|---|---|
| HEAD | `rubixdb_head_5c01ef.exe` | 18,271,744 B | `5c01ef052225755a6cfc7edceccedb4da5461400f20fdf4e6e10002876820eb8` | `cargo build --release --locked -p rubixdb-cli` in the clean HEAD checkout (`git_revision` reported by the binary: `2cbd5a7e2b18`) |
| Baseline (observability off) | `rubixdb_base_8e4737.exe` | 17,612,800 B | `5bfd731fc9d0d0ba3d4540d35a9de0d011c50a982151ffcc2c9c229c42c764c8` | same command on `git archive 8e47379` extracted outside any repository (no embedded console) |
| Certified binary (not used for measurement) | `E:\rubixdb_recon\rubixdb_certified_fe511ff.exe` | 18,276,352 B | `fe511ff5c5afb40ce2c89e2de6ae93cb8790d7397e5e086850653438abd38d7f` | the certification's own binary (built by `cargo test --release --workspace`); it differs from a plain `cargo build` of HEAD (feature unification), so measurements use the HEAD build |

**Host:** Windows 10 22H2 (19045), 4 physical / 8 logical CPUs, 15.9 GiB RAM, NTFS. Data volume `E:` = `PhysicalDrive0`; `C:` and `D:` = `PhysicalDrive1`. Free space: `E:` 10.4 % (13.2 of 127.4 GB), `C:` 9.2 % (8.1 of 88.0 GB). Windows Defender real-time protection, IOAV protection and behaviour monitoring are enabled (`Get-MpComputerStatus`). The session is **not elevated** (`net session` → exit 2).

## 1. Files read

In the required order: `CLAUDE.md`; `docs/PROJECT_STATE.md` (**absent → OPEN**); `missions/ACTIVE.md` (**absent → OPEN**); in full: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md`; `PHASE_RUBIXDB_FULL_OBSERVABILITY_BASELINE.md` (**does not exist**); `OPEN_ITEMS.md` (all 82 lines); `PROGRESS.md` (the 2026-10-06 Full Observability entry, lines 4804-4812, and its predecessor); `CHANGELOG.md` (Full Observability section, lines 9-29). Source (source beats summary): `api/src/observability/{sampler,ring,events,queries,probe,version,mod}.rs`, `api/src/routes/{metrics_system,observability}.rs`, `api/src/{metrics,resources,sql_session,server,state,auth,error}.rs`, `cli/src/ops_cmd.rs`, `src/lsm/mod.rs` (read-only). Read in addition because the investigation required it: `api/src/routes/{mod,sql,health}.rs`, `api/src/rate_limit.rs`, `api/build.rs`, `cli/src/host.rs`, `instance/src/lock.rs`, `src/wal/group_commit.rs` (header, `stats`), `src/execution/batch_coordinator.rs` (`stats`, coordinator lifecycle), `api/tests/observability.rs` and `cli/tests/observability_integration.rs`. Pre-existing architecture: `PHASE_RUBIXDB_OBSERVABILITY_ARCHITECTURE.md`. The earlier mission text that prescribed the health thresholds exists only as a scratch file of a previous session (`…\claude\E--RubiXDb\5d23e69a-…\scratchpad\mission.txt`, §3D) — **not a repository document**.

## 2. Evidence rules applied

* Every row below states exactly one of PASS / FAIL / OPEN / NOT REQUIRED / NOT TESTED / NOT IMPLEMENTED and cites a test, a file:line, a measurement or a documented decision. "The certification says so" was not accepted as evidence; where the only evidence is a certification-time measurement that was not repeated, the row says "second-hand".
* **Three separate matrices** keep OBSERVABILITY IMPLEMENTATION status (section 3.A), WORKSPACE REGRESSION status (3.B) and WHOLE-PRODUCT READINESS (3.C) apart; no status transfers between them and each has its own arithmetic.
* Not done, because the mission forbids it: the final regression, the soak, any fix, any production-source or test change. Not done because it was impossible here: elevated tracing (Process Monitor), a non-Windows run.
* Raw evidence lives **outside the repository**: `E:\rubixdb_recon\results\` (45 matrix runs), `results_extra\` (controls), `results_repro\` (original-harness shape), `*.log` (test and build logs), `scripts\` (every harness script used), `analysis.txt`. Every latency percentile in this document is computed from **every** request of the run (nothing trimmed, no best-run selection); only requests slower than 50 ms are retained individually. p99.9 is reported only where the run has ≥ 10,000 requests.

## 3. Reconciliation matrices (fresh; the old matrix was not copied)

### 3.A OBSERVABILITY IMPLEMENTATION STATUS

Fresh test runs at HEAD this session (debug): `api/tests/observability.rs` **26/26**, `api/tests/security_events_and_headers.rs` **10/10**, `rubixdb-api --lib` **73 passed, 1 ignored**, `cli/tests/observability_integration.rs` **3/3**.

| # | Row | Status | Evidence |
|---|---|---|---|
| A01 | HEALTH (classification `instance.healthy`) | **OPEN** | Code `api/src/observability/sampler.rs:379-406`. Test `health_follows_storage_state_and_disk_free_and_leaves_readiness_alone` PASS (StoragePressure→degraded, StorageFull→failed, free<10 %→degraded and exactly 10 %→healthy, injected lock loss→failed, each back to healthy). OPEN because the two numeric thresholds (30 s, 10 %) have no approved repository policy (§4) and fire on real data: an instance on `C:` (9.23 % free) reported `degraded`, on `E:` (10.43 % free) `healthy` (`health_on_c.py`). |
| A02 | HEALTH rule: readiness false > 30 s → `failed` | **NOT TESTED** | `sampler.rs:386-393`. No test references `not_ready`, `NOT_READY_FAILS_AFTER`, `coordinator_alive` or poisoning (grep of `api/tests/observability.rs`: 0 matches). Note the underlying conditions are terminal in the engine (`group_commit.rs:340` poison is permanent; `batch_coordinator.rs:271` coordinator death is never undone), so the 30 s only delays escalation. |
| A03 | HEALTH rule: real instance-lock probe (`cli/src/host.rs:285-292`) | **NOT TESTED** | The test uses a closure probe. The real probe calls `InstanceLock::try_acquire` every tick and treats anything except `AlreadyLocked` as 'lock lost' (`matches!(…Err(AlreadyLocked))`), so an `Err(Io)` (sharing violation, permission, directory missing) would read as lock loss → `failed`; `try_acquire` also creates the directory and lock file (`instance/src/lock.rs:52-66`). Healthy path observed live: only `healthy` in 53 observed runs, never `failed` (§5). Loss and I/O-error paths never exercised. |
| A04 | READINESS (`/readyz` unchanged; index recovery reported beside it) | **PASS** | `api/src/routes/health.rs` has zero diff against `8e47379` (`git diff --stat 8e47379 HEAD -- api/src/routes/health.rs` empty). Test `readiness_stays_true_while_safe_index_recovery_runs` PASS. Live: `/readyz` → `{ready:true, storage_state:Healthy, index_recovery:complete}`. |
| A05 | READINESS DEFINITION CONSISTENCY (`instance.readiness` vs `/readyz.ready`) | **OPEN** | Two definitions of 'ready' coexist: `/readyz.ready` is the constant `true` (`health.rs:45`); `instance.readiness` is `coordinator_alive && !poisoned` (`sampler.rs:380-385`). They can disagree (poisoned WAL: `not_ready` vs `true`). No test makes them disagree. Needs a maintainer decision (unify or rename). |
| A06 | STATUS (`/v1/status`, `/v1/admin/status`, other pre-existing endpoints) | **PASS** | Handlers unchanged: `git diff --stat 8e47379 HEAD` over `api/src/routes/{status,admin,health,kv,range,catalog,snapshots,compaction,metrics_route,instance}.rs` is empty. Not re-diffed black-box (the earlier `obs1*.py` inventory scripts live in an earlier session's scratch directory and were not re-run). |
| A07 | PRODUCT VERSION / BUILD IDENTITY | **PASS** | Live HEAD release build: `product_version 0.1.0`, `build_identifier 0.1.0-release-x86_64-windows`, `git_revision 2cbd5a7e2b18` = `git rev-parse --short=12 HEAD`; no path, user or host fragment in the response (`version_nogit.py`). Tests `version_reports_product_version_build_identity_and_startup_time`, unit `identity_has_no_paths_and_a_null_revision_is_allowed` PASS. |
| A08 | GIT REVISION UNKNOWN → `null` (no-git release build) | **PASS** | Release build of `git archive HEAD` extracted outside any repository: `git_revision: null`, same `build_identifier` form; a revision is not invented (`version_nogit.py`). |
| A09 | DEV-PROFILE (debug) BUILD IDENTITY, revision unknown | **NOT TESTED** | No debug no-git binary was built. By source (`api/build.rs` `emit_profile`, `observability/version.rs:18-26`) the only dev/release difference is the profile token in `build_identifier`; `git_revision` is `null` in both when unknown. |
| A10 | GIT REVISION `-dirty` SUFFIX FRESHNESS | **NOT TESTED** | `api/build.rs` re-runs only when `build.rs`, `../.git/HEAD`, `../.git/index` or `RUBIXDB_GIT_REVISION_OVERRIDE` change, and uses `git status --porcelain --untracked-files=no`: an edit that leaves the index untouched may be compiled without re-evaluating dirty state; new untracked files are never reflected. Hazard identified by reading; not built or observed. |
| A11 | METRICS SNAPSHOT shape (groups instance, cpu, memory, disk, throughput, latency, wal, compaction, background, security) | **PASS** | Test `metrics_system_has_every_required_field_with_the_right_types` PASS (fresh). Live snapshot inspected. |
| A12 | `errors.*` (4 fields), `limits.wal_backpressure_rejections`, `limits.sql_resource_limit_hits`, top-level `storage_state`: presence / type / value | **NOT TESTED** | No test asserts them (grep of `api/tests` and `cli/tests` for `http_server_errors_since_start`, `sql_errors_since_start`, `wal_write_errors`, `wal_backpressure_rejections`, `sql_resource_limit_hits`: 0 hits; the shape test stops at `security`). Present and `0` in every live snapshot. The certification's row 5 ('all required groups/fields … correct types') is therefore not supported for these fields. |
| A13 | METRIC FRESHNESS (timestamp, generation, age, state, stale) | **PASS** | Tests `a_wedged_sampler_goes_stale_then_failed_and_recovers_when_unblocked`, `a_dead_sampler_is_visible_as_stale_and_failed_never_as_current_data` PASS. Live: snapshot age never above 1,020 ms in 53 observed runs (§5, §7). |
| A14 | FRESHNESS when no snapshot exists yet (`stale:false` with `age_ms:null`) | **NOT TESTED** | `api/src/routes/metrics_system.rs:35,47-58`: with no snapshot `age_ms` is `null` and `stale` is `false` (`is_some_and`); `state` says `not_started`. Reachable only if the sampler never started (tests, library use, or the swallowed start failure A17). Not exercised. |
| A15 | SAMPLER HEALTH (running ↔ degraded ↔ failed ↔ not_started) | **PASS** | Tests `cpu_rss_and_disk_read_failures_null_the_field_degrade_the_sampler_and_never_stop_it`, `a_panic_in_a_tick_is_contained_and_the_sampler_recovers`, `a_wedged_sampler_…`, `a_dead_sampler_…` PASS. |
| A16 | SAMPLER: 5 consecutive panicking ticks → `failed` | **NOT TESTED** | `sampler.rs:38,549`. The panic test accepts `degraded`, `failed` or `running` after ≥2 panics (`api/tests/observability.rs:2013`); the 5-panic transition is not asserted. |
| A17 | SAMPLER START FAILURE VISIBILITY | **OPEN** | `cli/src/host.rs:293`: `sampler::start(&state).ok()` discards the error with no log line and no event; the endpoint would then report `not_started` forever. Source reading only. |
| A18 | TIME-SERIES (windows, resolution 60/15/60/3600 s, bounds, 400 on bad window, null for never-measured, ≤ 2 MiB) | **PASS** | Tests `timeseries_windows_resolution_empty_history_and_invalid_window`, `timeseries_returns_injected_history_oldest_first_with_exact_resolution_and_a_size_cap`, ring unit tests PASS. |
| A19 | TIME-SERIES over real 24 h / 7 d wall-clock | **NOT TESTED** | Only an injected clock (25 h) was used; no real-time run longer than seconds in this session. |
| A20 | REQUEST THROUGHPUT | **PASS** | Test `rates_and_gauges_follow_real_load_and_fall_back_when_it_stops` PASS (fresh). |
| A21 | SQL THROUGHPUT | **PASS** | Same test. |
| A22 | WRITE THROUGHPUT | **PASS** | Same test. Live: process write ops equal SQL commits 1:1 (4,467 = 4,467 in C5 run 1). |
| A23 | LATENCY DISTRIBUTION (`latency.query_p50/p95/p99_ms`) | **PASS** | Test `latency_percentiles_are_consistent_with_client_measured_times` PASS. Definition caveat (not a failure): the values are handler times of the newest 1,000 `POST /v1/sql` requests including error responses (`api/src/metrics.rs:66-77`), measured inside the auth layer (excludes HTTP read/parse/auth/queueing); no p99.9. |
| A24 | ACTIVE CONNECTIONS | **PASS** | Test `rates_and_gauges_…` PASS; `server.rs:92-153` gauge. |
| A25 | ACTIVE SESSIONS gauge and `/v1/observability/sessions` truthfulness | **FAIL** | Defect found and reproduced (`ghost_test.py`): a client disconnect while a statement runs inside an explicit transaction leaves its `SqlSessionRegistry.meta` entry forever. 60 of 60 aborted sessions: sessions endpoint `total 60`, all `executing`, ages growing (7 s → 27 s), `active_sessions` gauge 60 while `active_transactions` 0 and `COMMIT` on each returns non-200. Cause: `routes/sql.rs:596-645,685-737` take the session, await the blocking task, and call `finish`/`put_back` only if the future completes; the reaper iterates `sessions`, not `meta` (`sql_session.rs:333-355`). Introduced by the observability change (the `meta` map is new). |
| A26 | ACTIVE TRANSACTIONS | **PASS** | Reuses `TxnMetrics`; test `rates_and_gauges_…` PASS. (Live: stays 0 while ghost sessions persist — the gauge is correct, the session gauge is not.) |
| A27 | ACTIVE QUERIES | **PASS** | Test `running_and_cancelled_queries_are_visible_while_real_expensive_queries_run` PASS. |
| A28 | QUERY STATES | **PASS** | Same test + `queries_endpoint_lists_bounded_records_never_sql_text` PASS. |
| A29 | QUERY TIMEOUTS | **PASS** | Test `a_statement_past_its_deadline_is_recorded_as_timed_out_with_an_event` PASS. |
| A30 | QUERY CANCELLATIONS (autocommit statements) | **PASS** | Test `running_and_cancelled_…` PASS (client disconnect → `cancelled` record + `query.cancelled` event). The in-transaction case is A25. |
| A31 | QUERY FAILURES | **PASS** | Tests `queries_endpoint_…never_sql_text`, `events_endpoint_…` PASS. |
| A32 | TRACKED / UNTRACKED QUERY COUNTER CONSISTENCY | **FAIL** | `GET /v1/observability/queries` returns `untracked_active` = `QueryRegistry::untracked()` (`routes/observability.rs:93`), a cumulative counter that is never decremented (`queries.rs:123,156,174`); unit test `recent_and_in_flight_are_bounded` shows it stays 10 after every statement was dropped while `active()` is 0. The field name says 'active'; the value is a lifetime total. |
| A33 | PROCESS CPU | **PASS** | Test `cpu_rss_and_disk_read_failures…` PASS. Fresh steady-state cross-check vs psutil over the same 10 s window, 3 runs: API mean 71.14 / 72.60 / 72.26 % vs psutil 70.93 / 72.45 / 72.41 % of the whole machine (`cpu_xval.py`). Definition (percent of all logical CPUs) is not stated in the payload. |
| A34 | PROCESS RSS (working set) | **PASS** | Fresh: API 15,900,672 B vs psutil 15,908,864 B (≤ 1 tick apart); peak 16,068,608 B = 16,068,608 B (`xval.py`). |
| A35 | SYSTEM MEMORY | **PASS** | Fresh: total 17,060,876,288 B equal to psutil to the byte (`xval.py`). |
| A36 | DISK CAPACITY | **PASS** | Fresh: `volume_total_bytes` 127,355,359,232 on `E:` equal to psutil to the byte. |
| A37 | DISK FREE | **PASS** | Fresh: `volume_free_bytes` 13,209,919,488 equal to psutil to the byte. |
| A38 | DATABASE SIZE (`disk.db_bytes`) | **PASS** | Fresh: 894,596 B equal to a directory walk of `data/` (includes the WAL; `db_bytes` ≥ `wal_bytes`). Age exposed: `sizes_age_ms` 3,006–7,002 ms in 100 sampled responses of the 50,000-request cost run, and 0–9,999 ms across all 53 observed runs, never above the 10 s refresh period. |
| A39 | WAL SIZE / segment count | **PASS** | Fresh: `wal_bytes` 894,536 equal to walk of `data/wal`; `wal.segment_count` 1 = 1 `wal-*.log` file. |
| A40 | SSTABLE SIZE | **PASS** | Fresh: 0 = 0 (no flush happened in the short run). Larger flushed sizes were cross-checked in the certification (second-hand, not re-run). |
| A41 | DISK IOPS / THROUGHPUT measurement (process-level) | **PASS** | Measurement is what the OS reports for this process: process write ops equal commits (4,467 = 4,467, 4,501 = 4,501 …, C5 runs). The *labelling* is A42. |
| A42 | DISK I/O LABELLING (process-level vs device-level unmistakable to an operator) | **FAIL** | The response fields are `disk.read_iops`, `disk.write_iops`, `disk.read_mb_per_sec`, `disk.write_mb_per_sec` (and series `disk_*`), beside `volume_*` capacity fields; nothing in the payload says 'process'. Only a code comment (`metrics_system.rs:101`), the CLI line and the certification say so. It matters numerically: in C5 the process reported 4,467 write ops / 0.241 MB while the data volume's device counters moved 9,043 writes / 42.59 MB (2.0× ops, 177× bytes) in the same window (§9). |
| A43 | DEVICE-LEVEL DISK I/O | **NOT IMPLEMENTED** | Not implemented (certification §21). Feasibility measured this session: a non-elevated process can read `IOCTL_DISK_PERFORMANCE` from `\\.\E:` in 4–27 µs with no new crate (`ioctl_probe.py`). Classification H (§3) = OPEN POLICY DECISION. |
| A44 | WAL STATE | **PASS** | Test `metrics_system_has_every_required_field…` (`wal.state == Running`, segments, bytes) PASS; `wal.state` is the closed-set `PoolState` Debug name. |
| A45 | COMPACTION STATE | **PASS** | Test shape assertions PASS; `compaction_running()` is one relaxed atomic load (`src/lsm/mod.rs:2267`). Value cross-checks against `/v1/compaction/metrics` were certification-time (second-hand). |
| A46 | INDEX RECOVERY STATE | **PASS** | Test `readiness_stays_true_while_safe_index_recovery_runs` PASS (running/failed reported, health unaffected). |
| A47 | BACKGROUND OPERATIONS (`flush_queue_depth`, `pending_groups`, `index_build_state`) | **PASS** | Shape test PASS. Naming caveat: `pending_groups` is `GroupCommitStats.pending_waiters` (callers waiting for durability), `sampler.rs:442`. |
| A48 | FLUSH TIMESTAMP (`background.last_flush_ms`) | **NOT IMPLEMENTED** | Constant `null` (`metrics_system.rs:139`); asserted null by the shape test. Classification I (§3) = NOT REQUIRED FOR SINGLE-NODE V1; exact value would need an engine change (`src/lsm/mod.rs`, flush thread near line 3158). |
| A49 | RESOURCE-LIMIT COUNTERS `rate_limited`, `sessions_rejected` (values) | **PASS** | Tests `rate_limit_rejections_are_counted_with_events`, `session_cap_refusals_and_admin_actions_are_counted_and_visible` PASS. The other limit/error fields are A12. |
| A50 | SECURITY EVENT COUNTERS | **PASS** | Same suites + `two_instances_report_only_their_own_data_…` PASS. |
| A51 | OPERATIONAL EVENT RETRIEVAL | **PASS** | Test `events_endpoint_is_bounded_clamped_and_keeps_security_and_operational_apart` PASS. |
| A52 | STRUCTURED LOGGING (existing `security.log`) | **PASS** | Reused, unchanged module; `api/tests/security_events_and_headers.rs` 10/10 PASS (fresh) + `security_log` unit tests in the 73 lib tests. |
| A53 | LOG BOUNDS (`security.log`: 1 MiB × (4 generations + 1)) | **PASS** | `security_log.rs:52-53`; lib unit tests (fresh, in the 73) include the size-cap / generation tests. |
| A54 | EVENT BOUNDS (rings 256 + 256, ≤ 200 returned) | **PASS** | Unit `rings_are_bounded_separate_and_newest_first`; API 600-event flood test PASS. |
| A55 | OPERATIONAL EVENT PERSISTENCE | **NOT REQUIRED** | v1 contract is bounded in-memory history (`events.rs:6-8`); security events persist in the bounded rotating `security.log`. Classification J (§3). |
| A56 | METRIC CARDINALITY (closed key set) | **PASS** | Test `adversarial_inputs_never_grow_the_metric_key_set` PASS; `route_label`, 128 + overflow (`metrics.rs:20-21,59-63`). |
| A57 | METRIC / RING MEMORY BOUNDS | **PASS** | Ring unit tests + compile-time assertion `ring.rs:44`; measured 240,856 B after 25 h injected (certification, second-hand) — bound is by construction, asserted in tests. |
| A58 | NO UNBOUNDED GROWTH FROM ANY REQUEST (all registries) | **FAIL** | Every ring/registry has a cap except the session-observation map `SqlSessionRegistry.meta`: each aborted in-transaction statement adds an entry that nothing removes and that the per-principal cap does not count (the cap counts `sessions`, `sql_session.rs:145-149`). Reproduced with 60 sessions against a cap of 50 (A25). Memory per entry is small (~100 B) but unbounded. |
| A59 | SAMPLER FAILURE HANDLING (probe failure, panic, wedge) | **PASS** | Tests listed in A15 PASS. |
| A60 | SNAPSHOT CONSISTENCY (no torn reads) | **PASS** | Test `back_to_back_reads_while_the_sampler_ticks_never_see_a_torn_snapshot` PASS. |
| A61 | MULTI-INSTANCE ISOLATION | **PASS** | Test `two_instances_report_only_their_own_data_and_stopping_one_does_not_affect_the_other` + CLI `two_real_instances_…` PASS (3/3 CLI). |
| A62 | RESTART BEHAVIOR | **PASS** | CLI `killing_the_owner_and_restarting_it_starts_observability_state_from_empty` PASS. |
| A63 | CRASH BEHAVIOR (process kill) | **PASS** | Same CLI test (process kill, not graceful stop, not power loss). |
| A64 | POWER LOSS | **NOT TESTED** | Belongs to durability certification (classification K); observability state is memory-only. |
| A65 | API PERFORMANCE gate: `/v1/metrics/system` p95 < 50 ms at 16 readers | **PASS** | Fresh: p95 0.526–0.554 ms (C3, 5 runs, 692,872–716,265 requests each), 0.506–0.528 ms (C3t); original-harness shape 0.88–0.90 ms (5 runs). Gate 50 ms. (§5.) |
| A66 | API TAIL LATENCY (the reported 387 ms – 1.35 s maxima) | **OPEN** | Not reproduced in this session: the slowest of 12,107,104 `/v1/metrics/system` requests (27 runs, synchronized / staggered / mixed-with-SQL client shapes) was 214.0 ms, and the slowest of 1,852,006 requests in the original harness shape (5 runs) was 65.7 ms. What was established (§5): every >50 ms request in the 16-reader runs sits in the first 100 ms of the run (synchronized client start); with a staggered, pre-connected start the maximum over 3.73 M requests is 3.0–18.0 ms. The cause of the original observations is not proven. |
| A67 | OBSERVABILITY OVERHEAD vs baseline | **OPEN** | Measured (§5.3): idle RSS median 13.37 MB vs 12.68 MB (+0.69 MB), 18 vs 17 threads, 106–108 vs 104–106 handles, CPU ≤ 0.005 core in both; SQL read p50/p95/p99 and throughput ranges of baseline and HEAD overlap completely at n = 5 each. No acceptance threshold exists in any approved document, so PASS cannot be assigned; the certification's row 49 'PASS' rests on 'numbers reported, not called negligible'. Maintainer must set the threshold. |
| A68 | CLI OBSERVABILITY (`rubixdb status --system`) | **PASS** | CLI test `status_system_prints_the_live_snapshot_and_never_a_credential` PASS (3/3 fresh). |
| A69 | GUI API COMPATIBILITY (existing endpoints untouched) | **PASS** | See A06 (source diff). |
| A70 | GUI / BROWSER CONSUMPTION of the new endpoints | **NOT TESTED** | No frontend code references them (`grep -rn 'metrics/system\|observability/' frontend/src`: no matches); no screen exists. Classification E. |
| A71 | NON-WINDOWS RUNTIME | **NOT TESTED** | Only a Windows host; Linux branches compiled only; `disk_total_free` returns `None` off Windows (`resources.rs:376-380`). Classification F. |
| A72 | REAL LONG-DURATION SAMPLER SOAK (> 1 h) | **NOT TESTED** | Not run (forbidden by this mission). Classification G = REQUIRED FOR SINGLE-NODE V1. |
| A73 | STANDALONE `rubixdb-api` BINARY | **NOT REQUIRED** | Maintainer decision D-2 (`OPEN_ITEMS.md` 2026-10-04): not packaged, unsupported and not certified for v1. Classification L. |
| A74 | READER PRIVILEGE POLICY for the observability endpoints | **NOT REQUIRED** | The v1 embedded host provisions exactly one principal, `local`, role Admin (`cli/src/host.rs:121-125`); `Reader` exists only through the standalone server's key list. Classification D. (Recorded for a future multi-principal shape: the endpoints are Reader-visible while the older operator endpoint `/v1/admin/status` is Admin-only, `auth.rs:86-91`.) |
| A75 | DOCUMENTATION | **OPEN** | `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` (not modified here, by instruction) is inconsistent with the repository and with itself: header says 'uncommitted working tree' (now committed as `2cbd5a7`); its matrix totals 'OPEN 0, NOT TESTED 0' while §18/§19 list open and untested items; row 1 PASS beside §8 'decision requiring review'; row 5 'all fields' (A12); row 49 PASS with no threshold (A67); 'cause not investigated' items now investigated here. Correction is Prompt 2's addendum. |

Rows: **75** — PASS 48, FAIL 4, OPEN 6, NOT REQUIRED 3, NOT TESTED 12, NOT IMPLEMENTED 2  (check: 48 + 4 + 6 + 3 + 12 + 2 = 75)

The certification's matrix had 53 rows with 52 PASS. This matrix has more rows because capabilities that were folded into one certified row are separated, and rows were added for gaps found by reading the source. Rows whose status differs from the certification: HEALTH, READINESS DEFINITION, METRICS SNAPSHOT (`errors.*`), ACTIVE SESSIONS, DISK I/O (labelling), BACKGROUND OPERATIONS (flush timestamp), OBSERVABILITY OVERHEAD, API PERFORMANCE (tail), DOCUMENTATION.

### 3.B WORKSPACE REGRESSION STATUS (independent of 3.A)

| # | Row | Status | Evidence |
|---|---|---|---|
| B01 | `cargo fmt --all -- --check` | **PASS** | Fresh: exit 0 (`E:\rubixdb_recon\fmt.log` empty). |
| B02 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **NOT TESTED** | Not re-run (the final regression is excluded by this mission). Last recorded PASS: certification §9.2 (second-hand). |
| B03 | Build of `rubixdb-api` + `rubixdb-cli` at HEAD (release binary; debug test builds) | **PASS** | Fresh: `cargo build --release --locked -p rubixdb-cli` finished; the debug test builds of `rubixdb-api` and `rubixdb-cli` compiled and ran. Whole-workspace `cargo check --all-targets --all-features` not re-run. |
| B04 | `api/tests/observability.rs` (debug) | **PASS** | Fresh: 26 passed, 0 failed (`E:\rubixdb_recon\test_api_obs.log`). |
| B05 | `api/tests/security_events_and_headers.rs` (debug) | **PASS** | Fresh: 10 passed, 0 failed. |
| B06 | `rubixdb-api --lib` unit tests (debug) | **PASS** | Fresh: 73 passed, 0 failed, 1 ignored (`test_api_lib.log`). |
| B07 | `cli/tests/observability_integration.rs` (debug) | **PASS** | Fresh: 3 passed, 0 failed (`test_cli_obs.log`). |
| B08 | `tests/repo_hygiene.rs` (2 tests) | **FAIL** | Fresh at HEAD: `no_tracked_credentials_json` and `no_tracked_file_contains_a_64_hex_admin_key_literal` FAILED (2 passed, 2 failed; `test_repo_hygiene.log`). Cause present: `git ls-files` lists `frontend/.e2e-crossbrowser-data/default/credentials.json` at HEAD (burnt key; `OPEN_ITEMS.md` 2026-10-04). Not caused by observability. |
| B09 | `m1_3_thousand_writers_throughput` (release, ≥ 80,000 ops/s) | **FAIL** | Carried forward, NOT re-run this session (performance test, intermittent): certification §9.2 / `OPEN_ITEMS.md` 2026-10-06: clean HEAD 69,557 ops/s, this tree 68,133 / 49,300 / 60,922. FAIL stays FAIL until resolved. |
| B10 | Whole-workspace `cargo test --workspace` (debug) | **FAIL** | Carried forward, not re-run: recorded 1,331 passed / 2 failed (the two B08 tests). FAIL while B08 fails. |
| B11 | Whole-workspace `cargo test --release --workspace` | **FAIL** | Carried forward, not re-run: recorded 1,332 passed / 3 failed (B08 ×2 + B09). |
| B12 | Protected paths zero diff (`src/wal`, `src/manifest`, `src/sstable`, `src/compaction`, `src/error.rs`, `Cargo.toml`, `Cargo.lock`) between `8e47379` and HEAD, and none touched in this session | **PASS** | Fresh: `git diff --stat 8e47379 HEAD -- <paths>` empty; `git status --short` at the end shows only documentation files. |

Rows: **12** — PASS 7, FAIL 4, OPEN 0, NOT REQUIRED 0, NOT TESTED 1, NOT IMPLEMENTED 0  (check: 7 + 4 + 0 + 0 + 1 + 0 = 12)

### 3.C WHOLE-PRODUCT READINESS (independent of 3.A and 3.B; carried forward from repository documents, not re-measured)

| # | Row | Status | Evidence |
|---|---|---|---|
| C01 | WAL throughput gate M1.3 (≥ 80,000 ops/s) | **FAIL** | `OPEN_ITEMS.md` 2026-10-06; `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md`. Carried forward; not re-run. |
| C02 | No tracked credential material in the repository | **FAIL** | Verified fresh: the burnt `credentials.json` is tracked at HEAD (B08). `OPEN_ITEMS.md` 2026-10-04: untracking is uncommitted/unpushed; the key stays readable in history (maintainer D-1). |
| C03 | Power-loss durability | **NOT TESTED** | `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` §19; `PHASE_RUBIXDB_PRODUCTION_OPERATIONS_RESULTS.md`. Kill-only evidence exists. |
| C04 | Real disk-full (ENOSPC) on a real volume | **NOT TESTED** | `PHASE_RUBIXDB_PRODUCTION_OPERATIONS_RESULTS.md:84` (F14): no elevation / VHD; injected `StorageFull` only. |
| C05 | Lifecycle baseline findings F-07, F-08, F-11, F-18 | **OPEN** | `OPEN_ITEMS.md` 2026-10-05 (last bullet): WAL final-record damage opens silently, damaged SSTable reports ready, readiness bound not a bound on exit (unreproduced), non-loopback `RUBIXDB_API_URL` policy. |
| C06 | `docs/PROJECT_STATE.md` and `missions/ACTIVE.md` (required reading per `CLAUDE.md`) | **OPEN** | Neither exists (`ls docs missions` fails); `OPEN_ITEMS.md` 2026-10-04 and 2026-10-06. Consequently 'certified' components beyond the four named directories cannot be enumerated. |
| C07 | Standalone `rubixdb-api` (unsupported for v1) | **NOT REQUIRED** | `OPEN_ITEMS.md` 2026-10-04 SG-6 / D-2. |
| C08 | Observability layer meets its own matrix (this document, section A) | **OPEN** | Section A totals: PASS 48, FAIL 4, OPEN 6, NOT REQUIRED 3, NOT TESTED 12, NOT IMPLEMENTED 2 of 75 (section 3.A). rubiXDb is not declared PRODUCTION READY; the mandatory matrix is not entirely PASS. |

Rows: **8** — PASS 0, FAIL 2, OPEN 3, NOT REQUIRED 1, NOT TESTED 2, NOT IMPLEMENTED 0  (check: 0 + 2 + 3 + 1 + 2 + 0 = 8)

**None of the three sections says rubiXDb is production ready.** Section A has FAIL, OPEN and NOT TESTED rows; section B has FAIL rows that pre-date this work; section C has FAIL, OPEN and NOT TESTED rows.

## 4. Classification of the twelve open items (A–L)

Classes: REQUIRED FOR SINGLE-NODE V1 · NOT REQUIRED FOR SINGLE-NODE V1 · OPEN POLICY DECISION · NOT TESTED ENVIRONMENT LIMITATION · REQUIRES ANOTHER ROADMAP PHASE · REQUIRES ENGINE CHANGE. **This mission proposes to implement nothing for any of the twelve items.**

| # | Item | Classification |
|---|---|---|
| A | Health thresholds require review | **OPEN POLICY DECISION** |
| B | Occasional long `/v1/metrics/system` latency | **NOT TESTED ENVIRONMENT LIMITATION** |
| C | Large variance in thread/handle measurements | **REQUIRES ANOTHER ROADMAP PHASE** (SQL / runtime resource tuning) |
| D | Reader privilege policy for observability endpoints | **NOT REQUIRED FOR SINGLE-NODE V1** |
| E | Missing real browser consumption | **REQUIRES ANOTHER ROADMAP PHASE** (frontend) |
| F | Non-Windows runtime measurement | **NOT TESTED ENVIRONMENT LIMITATION** |
| G | Real long-duration sampler soak | **REQUIRED FOR SINGLE-NODE V1** |
| H | Device-level disk I/O | **OPEN POLICY DECISION** |
| I | `background.last_flush_ms` | **NOT REQUIRED FOR SINGLE-NODE V1** |
| J | Persistent operational events | **NOT REQUIRED FOR SINGLE-NODE V1** |
| K | Power-loss testing | **REQUIRES ANOTHER ROADMAP PHASE** (durability certification) |
| L | Standalone `rubixdb-api` testing | **NOT REQUIRED FOR SINGLE-NODE V1** |

**A — OPEN POLICY DECISION.** The 30 s / 10 % thresholds are not repository policy (search in section 5). Their only provenance is the maintainer's own earlier mission text, which said to record them as "a decision that must be reviewed" and to recompute the field if overridden. They also have measurable effects: an instance rooted on `C:` (9.23 % free) reports `degraded`, one on `E:` (10.43 % free) reports `healthy`, on the same machine, with identical engine state; the 30 s grace sits on conditions the engine documents as terminal (`src/wal/group_commit.rs:340`, `src/execution/batch_coordinator.rs:271`); two of the five rules have no test (A02, A03) and two of the five inputs (lock state, not-ready duration) are not exposed as raw fields. Decision packet: section 5. Nothing implemented.

**B — NOT TESTED ENVIRONMENT LIMITATION.** The reported 387 ms–1.35 s maxima could not be reproduced (section 6): 12.1 M endpoint requests in 27 runs (slowest 214.0 ms) and 1.85 M requests with the original harness shape (slowest 65.7 ms). The conditions of the original observation cannot be reconstructed (the certification records a 91 %-full `C:` volume and, for its throughput runs, other work running on the machine); the original-harness reproduction here did use `C:` as the data volume, like the original. What is established is that every >50 ms request in the 16-reader runs sits in the first 100 ms of the run and disappears when the clients start staggered and pre-connected; the same start-of-run effect appears on the pre-observability baseline binary. The matrix row (A66) stays OPEN; no PASS is claimed; no observability-code cause was found and none is proposed.

**C — REQUIRES ANOTHER ROADMAP PHASE.** Root cause proven and unrelated to observability: during a 16-client SQL read load the server's thread count rises from 8 `tokio-rt-worker` threads to **489** and decays over the following seconds (thread-description census, `census_run.py`); the pre-observability baseline binary shows the same 353–529 threads (C1) as HEAD (353–530, C1b); the metrics-only runs stay at 18 threads. Cause: `tokio::runtime::Builder::new_multi_thread().enable_all()` with no `max_blocking_threads` (`cli/src/host.rs:180-183`) → tokio's default blocking-pool bound of 512, and `tokio::task::spawn_blocking` per SQL statement (`api/src/routes/sql.rs:211`). 530 − 18 steady-state threads = 512. Fast runs (24.7–25.7 k req/s) all had ≤ 385 threads, but one 353-thread run was slow (21.6 k req/s), so the throughput correlation is suggestive, not proven. Whether v1 should bound the blocking pool is a SQL/runtime resource decision (priority 5, "resource safety"); it is recorded for that phase, not fixed here.

**D — NOT REQUIRED FOR SINGLE-NODE V1.** The v1 embedded host provisions exactly one principal, `local`, role Admin (`cli/src/host.rs:121-125`); a `Reader` credential exists only through the standalone server's configuration, which the maintainer decided is out of v1 (D-2, `OPEN_ITEMS.md` 2026-10-04). Recorded for any future multi-principal shape: the new endpoints are Reader-visible (`auth.rs:82-95`) while the older operator endpoint `/v1/admin/status` (RSS, disk, WAL internals) is Admin-only by an explicit comment (`auth.rs:86-91`); `/v1/observability/sessions` lists every principal's session ids (harmless as a capability: `SqlSessionRegistry::take` binds a session to its principal, `sql_session.rs:180-206`); and a Reader can slow SQL by polling `/v1/observability/queries` (section 6, C4q). A product need for Reader visibility exists in the frontend (`OPEN_ITEMS.md` 2026-10-04 Increment 2: RAM figures "not served" to non-admins).

**E — REQUIRES ANOTHER ROADMAP PHASE.** No frontend code references the new endpoints (`grep -rn "metrics/system\|observability/" frontend/src` → no matches) and no screen exists; browser consumption cannot be tested until a screen is built. The CLI consumer is tested (`status_system_prints_the_live_snapshot_and_never_a_credential`, 3/3 fresh).

**F — NOT TESTED ENVIRONMENT LIMITATION.** Only a Windows host is available. WSL2 distributions exist (Ubuntu, kernel 6.18) but contain no Rust toolchain (`which cargo rustc gcc` → nothing; no `~/.cargo`); provisioning one is outside a read-only reconciliation. Source facts for whoever tests it: Linux reads `/proc/self/{stat,status,io}` and `/proc/meminfo`, assumes `USER_HZ = 100` (`resources.rs:335`), and `disk_total_free` returns `None` (`resources.rs:376-380`) — so on Linux `disk.volume_*` is always `null` and the low-disk health rule can never fire.

**G — REQUIRED FOR SINGLE-NODE V1.** The sampler is a permanent 1 Hz thread issuing six OS calls, a lock-file open (the lock probe) and, every 10 s, three directory walks; a per-tick leak (handle, thread, allocation) is only provable by a real-time run. Evidence so far: 100 start/stop cycles, a 25 h injected-clock ring test, and ~21 s idle runs in this session with constant handles/threads (106–108 / 18 in five runs). `CLAUDE.md` ("no hidden NOT TESTED") makes this a gate for any readiness claim. Not run here (the mission forbids the soak).

**H — OPEN POLICY DECISION.** The certification says device-level counters need "handles or privileges" that were not authorized. **Measured here:** a non-elevated process can open `\\.\E:` with zero access and read `IOCTL_DISK_PERFORMANCE` in 4–27 µs through plain `kernel32` FFI (`CreateFileW` + `DeviceIoControl`, the same style as the existing probes; no new crate) — so feasibility is not the obstacle on Windows (Linux `/proc/diskstats` was not tested). The reason a decision is still needed: the present process-level fields can understate device activity by 2.0× (operations) and 177× (megabytes) on an fsync-heavy workload (section 9), so the product cannot currently answer "how busy is the disk"; yet no repository requirement mandates device-level metrics. Recommendation: do not add it for v1; fix the labelling (A42) and say so in the contract.

**I — NOT REQUIRED FOR SINGLE-NODE V1** (keep `null`, contract documented — or drop the field). Populating it exactly **is an engine change**: the flush thread (`src/lsm/mod.rs` near line 3158) increments a `pub(crate)` `flush_completions` counter used only by `cfg(test)` code (`:1244-1250`, `:1626`); recording a time there modifies the storage engine's flush-completion path. `src/lsm/mod.rs` is not one of the four named protected directories, but it is the certified storage engine and `docs/PROJECT_STATE.md` (which would list certified components) does not exist — so treat it as needing an ADR. An observability-only approximation needs **no** engine change: the sampler can record the time at which it first sees `checkpoint_seq()` (public, `src/lsm/mod.rs:2173`) advance, with ±1 tick resolution, labelled as observed-by-sampler and `null` until first observed.

**J — NOT REQUIRED FOR SINGLE-NODE V1.** The documented v1 scope is bounded in-memory history (`events.rs:6-8`: "Events are ephemeral … durable security events go to the bounded `security.log` file"). The security side persists in the rotating security log: ≤ 1 MiB per file × (4 generations + 1), fields fixed and capped at 128 characters, authentication-failure lines rate-limited to one per 10 s (`security_log.rs:46-54`). Persisting operational events adds disk writes and a durability surface for events whose loss is harmless (restart-from-empty is tested: `killing_the_owner_and_restarting_it_starts_observability_state_from_empty`). The maintainer may override.

**K — REQUIRES ANOTHER ROADMAP PHASE.** Power loss concerns WAL / manifest durability, not observability (observability state is memory-only). `CLAUDE.md`: "Never call process-kill a power loss. Power-loss = NOT TESTED if untested." The observability evidence is kill-only. Not tested here.

**L — NOT REQUIRED FOR SINGLE-NODE V1.** `OPEN_ITEMS.md` 2026-10-04 (SG-6, D-2): `rubixdb-api.exe` is no longer packaged; the standalone binary is unsupported and not certified for v1. Repository policy has not changed since (OPEN_ITEMS to 2026-10-06 contains no reversal). The standalone `main.rs` is wired to the sampler (`api/src/main.rs`) but untested end to end; its lock probe is absent so the lock-loss rule is inert there.

## 5. Health policy resolution — DECISION PACKET

### 5.0 Is there an approved policy? — No.

Searched the repository (all `*.md`, `api/src`, `cli/src`, `src`) for health / degraded / failed classification thresholds, disk-free percentages and not-ready durations: `CLAUDE.md` (no mention), `PHASE_RUBIXDB_*` and `PHASE*` documents, `RubixDB-Architecture-Specification-v1.0.md`, `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`, `OPEN_ITEMS.md`. Findings:

* `RubixDB-Architecture-Specification-v1.0.md:83-89` defines a **partition** state `DEGRADED` (multi-partition design: failed health check / aborted migration); it has no thresholds and is not instance health.
* `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` defines the **engine** states `Healthy / StoragePressure / StorageFull` (§6, implemented at `lsm::StorageState`), explicitly refuses to trust free space alone for recovery ("It must not move directly to HEALTHY merely because free space appears to have returned", §13) and records that an optional free-space pre-check was **not implemented** because it needs a new dependency (Implementation Notes, §8). That ADR is the only repository-approved vocabulary behind two of the five rules.
* `OPEN_ITEMS.md` 2026-10-04 (Increment 2): memory-pressure thresholds in the console (75 % / 90 %) are "a UI choice" and "a future API mission must define them" — i.e. the repository already records that such thresholds are undefined.
* The 30 s / 10 % numbers appear only in the maintainer's prior mission text (scratch file, §3D: "If no policy exists in the repository … Record the threshold choice … as a decision that must be reviewed. If the reviewer later overrides it, the field is recomputed.") and in the certification §8, which says the same. **No ADR, no `CLAUDE.md` line, no PHASE document approves them.**

I therefore did not invent a policy; the packet follows.

### 5.1 (a) Current thresholds and where they live

| Rule | Threshold | Location |
|---|---|---|
| `failed` if instance not ready continuously | 30 s | `api/src/observability/sampler.rs:40` (`NOT_READY_FAILS_AFTER`), used `:391-393` |
| `failed` if storage full | `storage_state == StorageFull` | `sampler.rs:400` |
| `failed` if instance lock no longer held | probe returns "not held" | `sampler.rs:394`; probe installed at `cli/src/host.rs:285-292` |
| `degraded` if storage pressure | `storage_state == StoragePressure` | `sampler.rs:402` |
| `degraded` if volume free space low | `< 10 %` of volume total, strict | `api/src/observability/mod.rs:108-110` (`disk_low_percent()`), used `sampler.rs:395-398` |
| "ready" definition feeding the 30 s rule | `coordinator_alive && !(sync_failures > 0 \|\| pool.state == Failed)` | `sampler.rs:380-385` |

### 5.2 (b) Raw state the API already exposes (exact field paths)

`GET /v1/metrics/system`: `storage_state`; `instance.readiness` (derived, see c); `sample_freshness.{state,stale,age_ms,last_sample_ms,tick_ms}`; `sample_generation`; `disk.volume_total_bytes`, `disk.volume_free_bytes`, `disk.volume_used_percent`; `wal.state`; `errors.wal_sync_failures`, `errors.wal_write_errors`; `limits.wal_backpressure_rejections`; `background.index_build_state`; `compaction.running`. `GET /readyz`: `ready` (constant `true`), `storage_state`, `index_recovery`. `GET /v1/status`: `storage_state`.

**Not exposed raw:** the instance-lock state (only its effect on `healthy`), the duration the instance has been not-ready, the `coordinator_alive` flag, and the poisoned flag as such (derivable: `errors.wal_sync_failures > 0 || wal.state == "Failed"`). Two of the five inputs therefore cannot be audited by a client.

### 5.3 (c) Derived classification computed from the raw state

`instance.healthy ∈ {healthy, degraded, failed}` and `instance.readiness ∈ {ready, not_ready}`, computed once per tick (`sampler.rs:379-406`), stable between ticks, `null` when no snapshot exists (`metrics_system.rs:55-56`). `instance.readiness` is **not** `/readyz.ready`: the latter is constant `true` (`health.rs:45`) (matrix A05).

### 5.4 (d) Options and consequences

**Option 1 — adopt the current thresholds as v1 policy, cite them in an ADR / `CLAUDE.md`.**
Consequence: no behaviour change. The numbers acquire repository authority they have no measured basis for: on this machine the 10 % rule splits two volumes 0.8 percentage points apart into `degraded` and `healthy`; `degraded` has no action attached; the 30 s grace has no recovery meaning (terminal conditions); the false-`failed` risk from the lock probe's `Err(Io)` mapping (A03) and the untested rules (A02, A03) remain.
Files: a new ADR (suggested `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md`), one policy line in `CLAUDE.md` or the ADR index; the certification addendum and `OPEN_ITEMS.md` closure (next prompt). No source change (optionally doc comments at `sampler.rs:39-40`, `mod.rs:105-110` pointing at the ADR).

**Option 2 — expose raw state only; make derived `healthy` explicitly advisory.**
Consequence: same rules, but the field is renamed (e.g. `instance.health_advisory`) or documented in the payload as "policy-dependent", and the two missing raw inputs are added (`instance.lock_state` ∈ {held, lost, unknown}, `instance.not_ready_seconds`) so a client can recompute the verdict under its own policy. The invented numbers stay in code but stop being presented as fact. Additive API change; no consumer exists yet (frontend: none; CLI prints `health=`).
Files: `api/src/observability/sampler.rs` (snapshot fields), `api/src/routes/metrics_system.rs` (names / new fields), `cli/src/ops_cmd.rs:291-302` (label), `api/tests/observability.rs` (health test + new field tests), certification addendum, `OPEN_ITEMS.md`.

**Option 3 — split by provenance: only repository-defined states decide `healthy`; invented numeric thresholds become separate advisories.**
`instance.healthy` = `failed` on `StorageFull`, confirmed lock loss, or WAL writer dead / poisoned; `degraded` on `StoragePressure`; otherwise `healthy` — each defined by an existing ADR or a certified terminal state, none by an invented number. The 10 % free-space rule becomes `disk.low_free_advisory {active, threshold_percent}` and no longer changes `healthy`; the 30 s grace is removed (the underlying conditions are terminal; the sampler is stopped before the engine shuts down, so shutdown cannot trip it); the lock probe becomes three-valued (`held` / `lost` / `unknown`) and an I/O error is `unknown`, not `failed`.
Consequence: `healthy` on this machine's `C:` volume (9.23 % free) with the advisory raised; one fewer false-alarm path; removes an invented number from the primary answer; larger source change than 1 or 2.
Files: `api/src/observability/sampler.rs` (classification; drop `NOT_READY_FAILS_AFTER`), `api/src/observability/mod.rs` (`disk_low_percent` → advisory constant), `api/src/routes/metrics_system.rs` (advisory block, lock state), `cli/src/host.rs:285-292` (tri-state probe), `instance/src/lock.rs` (a non-creating "is it held" check), `cli/src/ops_cmd.rs` (print advisory), `api/tests/observability.rs` (health test split, plus the missing not-ready / lock-unknown tests), certification addendum, `OPEN_ITEMS.md`.

### 5.5 (e) Recommendation

**Option 3.** Reasoning: `CLAUDE.md` says never invent numbers and the maintainer's own text asked for review; the 10 % rule demonstrably misclassifies on a typical development machine; the 30 s rule delays escalation of conditions that cannot recover; two of five inputs are unauditable; and the repository already has defined vocabulary (the storage-pressure ADR) for exactly the states that should decide health. Option 3 keeps one operator-facing answer, keeps the numbers visible (echoing `threshold_percent`) without letting them decide `healthy`, and does not need a new policy document. If the maintainer prefers no source change, Option 1 is acceptable provided the ADR states the thresholds are provisional; Option 2 is the middle path.

### 5.6 (f) Files that change under each option — see the option texts above (summarised: Option 1 docs only; Option 2 four source files plus tests; Option 3 five source files plus tests). None is a protected path.

## 6. Latency investigation (item B) — an investigation, not a fix

### 6.1 Design

One real `rubixdb gui --no-browser` owner per run on a **copy of a 20,000-row template** (`t(id INTEGER PRIMARY KEY, v TEXT)`), fresh process each run, data on `E:`; 15 s closed-loop load (16 clients unless stated) from separate OS processes over keep-alive connections (raw sockets, no client-side parsing); a 20 Hz observer poller on `/v1/metrics/system` in every HEAD run to record sampler cadence and health; process counters (CPU, RSS, private bytes, threads, handles) sampled every 0.5 s with psutil; whole-run process I/O counters and per-disk device counters (`PhysicalDrive0` = data volume `E:`, `PhysicalDrive1` = temp volume `C:`); server-side figures from `/v1/metrics` (`GET /v1/metrics/system` route max) and from each response's own `latency_of_response_ms`. **5 runs per condition, all reported, runs interleaved across conditions** (exceptions: C3n 2 runs and C3nh 1 run per binary, because a new-connection test consumes the client's ephemeral ports). **Discarded and redone, for transparency:** (i) a first SQL attempt used `?` parameters, which this API rejects (`only $n-style parameters are supported`) — the harness counted every response as non-200, so all SQL-based runs were discarded and redone with `$1`; (ii) two copies of the matrix ran concurrently for a few minutes and contaminated each other, so all results were deleted and rerun (the metrics-only runs C2/C3/C3h/C3s run 1 were produced by the first, uncontaminated invocation of the same harness and kept — they use no SQL); (iii) a 16-client new-connection variant exhausted the client's ephemeral ports (`WSAEADDRINUSE`, ~16 k `TIME_WAIT` sockets) and was replaced by the paced C3n. All reported runs: 0 errors, 0 non-200.

Conditions requested by the mission: (1) SQL path without observability = **C1** baseline binary (a sampler cannot be stopped at runtime; the pre-observability binary is the honest "observability off"), with **C1b** HEAD under the same load; (2) endpoint idle = **C2** (and **C2b** baseline idle); (3) 16 concurrent readers = **C3**; (4) with SQL read load = **C4**; (5) with writes = **C5** (and **C5b** writes alone). Controls added to attribute causes: **C3h** `/healthz` (no auth, no metrics middleware), **C3s** 4 readers, **C3t** staggered pre-connected start, **C3n / C3nh** new TCP connection per request, **C4q** queries-endpoint polling. Tooling is in `E:\rubixdb_recon\scripts\`.

### 6.2 Results (all runs; every percentile is over every request of the run)

Summary across the 5 runs of each condition (min / median / max):

| Condition / client kind | runs | p50 ms (min / median / max) | p95 ms | p99 ms | max ms (min / median / max) | req/s (min / median / max) |
|---|---|---|---|---|---|---|
| C1  baseline binary (8e47379, no observability code), 16 SQL read clients - `sqlread` | 5 | 0.502 / 0.545 / 0.554 | 1.180 / 1.676 / 1.703 | 2.195 / 3.285 / 3.320 | 42.1 / 61.8 / 105.7 | 21025 / 21403 / 25713 |
| C1b HEAD binary, 16 SQL read clients (20 Hz observer poller) - `sqlread` | 5 | 0.505 / 0.558 / 0.582 | 1.228 / 1.437 / 1.777 | 2.250 / 2.755 / 3.496 | 36.5 / 43.6 / 127.8 | 20130 / 21626 / 25422 |
| C2  HEAD binary, observability on, endpoint idle (20 Hz observer poller only) (observer) | 5 | 0.807 / 0.812 / 0.827 | 0.900 / 0.936 / 1.023 | 0.964 / 6.006 / 6.607 | 5.86 / 6.50 / 7.09 | 20 |
| C3  HEAD, 16 concurrent `/v1/metrics/system` readers (synchronized start, connect inside timed region) - `metrics` | 5 | 0.309 / 0.312 / 0.320 | 0.526 / 0.542 / 0.554 | 0.692 / 0.725 / 0.764 | 50.3 / 61.6 / 108.0 | 46192 / 47149 / 47751 |
| C3t HEAD, 16 readers, connect + warm-up before timing, start staggered 40 ms apart - `metrics` | 5 | 0.292 / 0.295 / 0.306 | 0.506 / 0.521 / 0.528 | 0.657 / 0.705 / 0.717 | 3.0 / 8.8 / 18.0 | 48868 / 49880 / 50437 |
| C3h control: 16 readers on `/healthz` (no auth, no metrics middleware) - `healthz` | 5 | 0.208 / 0.210 / 0.214 | 0.335 / 0.339 / 0.353 | 0.423 / 0.428 / 0.468 | 24.3 / 31.9 / 49.0 | 70235 / 72181 / 72358 |
| C3s 4 readers on `/v1/metrics/system` - `metrics` | 5 | 0.124 / 0.125 / 0.126 | 0.177 / 0.180 / 0.181 | 0.204 / 0.207 / 0.210 | 6.5 / 13.9 / 16.9 | 28804 / 28976 / 29303 |
| C3n 6 readers, NEW TCP connection per request, paced 80 req/s each - `metrics` | 2 | 2.172 / 2.364 / 2.556 | 25.107 / 25.835 / 26.564 | 28.629 / 29.182 / 29.735 | 36.3 / 38.4 / 40.6 | 480 / 480 / 480 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `sqlread` | 5 | 0.650 / 0.652 / 0.666 | 1.334 / 1.691 / 1.744 | 2.357 / 3.102 / 3.263 | 58.9 / 75.0 / 215.0 | 18815 / 19452 / 20767 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 5 | 0.469 / 0.476 / 0.486 | 0.999 / 1.238 / 1.282 | 1.477 / 1.854 / 1.906 | 34.0 / 51.8 / 214.0 | 6788 / 7037 / 7480 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `sqlread` | 5 | 0.799 / 0.808 / 0.865 | 1.543 / 1.740 / 1.843 | 2.374 / 2.787 / 2.877 | 53.5 / 84.7 / 126.1 | 15828 / 17037 / 17751 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `queries` | 5 | 1.994 / 2.056 / 2.069 | 3.406 / 3.569 / 3.636 | 9.939 / 10.691 / 12.568 | 44.3 / 70.0 / 124.4 | 1620 / 1649 / 1711 |
| C5b HEAD, 16 SQL write clients (single-row INSERT) - `sqlwrite` | 5 | 65.671 / 66.876 / 70.004 | 75.597 / 75.798 / 82.619 | 87.889 / 88.693 / 99.520 | 99.8 / 100.7 / 116.7 | 237 / 239 / 242 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `sqlwrite` | 5 | 51.773 / 52.430 / 52.817 | 56.377 / 58.656 / 63.449 | 75.091 / 75.928 / 77.254 | 94.2 / 123.5 / 162.5 | 298 / 301 / 303 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 5 | 0.127 / 0.127 / 0.131 | 0.185 / 0.186 / 0.192 | 0.214 / 0.216 / 0.224 | 15.8 / 20.8 / 21.0 | 27666 / 28366 / 28486 |

Per-run latency (full table):

<details><summary>Per-run latency table</summary>

| Condition / client kind | run | n requests | req/s | p50 ms | p95 ms | p99 ms | p99.9 ms | max ms | errors / non-200 |
|---|---|---|---|---|---|---|---|---|---|
| C1  baseline binary (8e47379, no observability code), 16 SQL read clients - `sqlread` | 1 | 320508 | 21367 | 0.539 | 1.703 | 3.312 | 7.263 | 105.7 | 0 / 0 |
| C1  baseline binary (8e47379, no observability code), 16 SQL read clients - `sqlread` | 2 | 323642 | 21576 | 0.547 | 1.643 | 3.176 | 6.826 | 42.1 | 0 / 0 |
| C1  baseline binary (8e47379, no observability code), 16 SQL read clients - `sqlread` | 3 | 315369 | 21025 | 0.554 | 1.701 | 3.320 | 7.578 | 78.9 | 0 / 0 |
| C1  baseline binary (8e47379, no observability code), 16 SQL read clients - `sqlread` | 4 | 385692 | 25713 | 0.502 | 1.180 | 2.195 | 4.912 | 49.7 | 0 / 0 |
| C1  baseline binary (8e47379, no observability code), 16 SQL read clients - `sqlread` | 5 | 321045 | 21403 | 0.545 | 1.676 | 3.285 | 7.654 | 61.8 | 0 / 0 |
| C1b HEAD binary, 16 SQL read clients (20 Hz observer poller) - `sqlread` | 1 | 381332 | 25422 | 0.505 | 1.228 | 2.250 | 4.772 | 41.4 | 0 / 0 |
| C1b HEAD binary, 16 SQL read clients (20 Hz observer poller) - `sqlread` | 2 | 312673 | 20845 | 0.558 | 1.727 | 3.389 | 7.443 | 43.6 | 0 / 0 |
| C1b HEAD binary, 16 SQL read clients (20 Hz observer poller) - `sqlread` | 3 | 324394 | 21626 | 0.582 | 1.437 | 2.755 | 8.381 | 58.9 | 0 / 0 |
| C1b HEAD binary, 16 SQL read clients (20 Hz observer poller) - `sqlread` | 4 | 301950 | 20130 | 0.573 | 1.777 | 3.496 | 8.809 | 127.8 | 0 / 0 |
| C1b HEAD binary, 16 SQL read clients (20 Hz observer poller) - `sqlread` | 5 | 369845 | 24656 | 0.517 | 1.266 | 2.413 | 5.401 | 36.5 | 0 / 0 |
| C2  HEAD binary, observability on, endpoint idle (20 Hz observer poller only) (observer poller, 20 Hz) | 1 | 311 | 20 | 0.827 | 1.023 | 6.607 | n/a (n<10,000) | 7.09 | 0 / 0 |
| C2  HEAD binary, observability on, endpoint idle (20 Hz observer poller only) (observer poller, 20 Hz) | 2 | 310 | 20 | 0.812 | 0.900 | 0.964 | n/a (n<10,000) | 5.86 | 0 / 0 |
| C2  HEAD binary, observability on, endpoint idle (20 Hz observer poller only) (observer poller, 20 Hz) | 3 | 320 | 20 | 0.818 | 0.945 | 6.006 | n/a (n<10,000) | 6.50 | 0 / 0 |
| C2  HEAD binary, observability on, endpoint idle (20 Hz observer poller only) (observer poller, 20 Hz) | 4 | 324 | 20 | 0.807 | 0.936 | 6.025 | n/a (n<10,000) | 6.89 | 0 / 0 |
| C2  HEAD binary, observability on, endpoint idle (20 Hz observer poller only) (observer poller, 20 Hz) | 5 | 324 | 20 | 0.810 | 0.930 | 4.870 | n/a (n<10,000) | 6.37 | 0 / 0 |
| C3  HEAD, 16 concurrent `/v1/metrics/system` readers (synchronized start, connect inside timed region) - `metrics` | 1 | 692872 | 46192 | 0.320 | 0.549 | 0.735 | 1.269 | 108.0 | 0 / 0 |
| C3  HEAD, 16 concurrent `/v1/metrics/system` readers (synchronized start, connect inside timed region) - `metrics` | 2 | 707240 | 47149 | 0.309 | 0.554 | 0.764 | 1.364 | 54.0 | 0 / 0 |
| C3  HEAD, 16 concurrent `/v1/metrics/system` readers (synchronized start, connect inside timed region) - `metrics` | 3 | 706734 | 47116 | 0.314 | 0.542 | 0.725 | 1.202 | 50.3 | 0 / 0 |
| C3  HEAD, 16 concurrent `/v1/metrics/system` readers (synchronized start, connect inside timed region) - `metrics` | 4 | 716265 | 47751 | 0.312 | 0.526 | 0.692 | 1.084 | 61.6 | 0 / 0 |
| C3  HEAD, 16 concurrent `/v1/metrics/system` readers (synchronized start, connect inside timed region) - `metrics` | 5 | 715785 | 47719 | 0.309 | 0.537 | 0.721 | 1.133 | 93.0 | 0 / 0 |
| C3t HEAD, 16 readers, connect + warm-up before timing, start staggered 40 ms apart - `metrics` | 1 | 733023 | 48868 | 0.306 | 0.519 | 0.680 | 0.987 | 3.9 | 0 / 0 |
| C3t HEAD, 16 readers, connect + warm-up before timing, start staggered 40 ms apart - `metrics` | 2 | 742472 | 49498 | 0.304 | 0.506 | 0.657 | 0.962 | 3.0 | 0 / 0 |
| C3t HEAD, 16 readers, connect + warm-up before timing, start staggered 40 ms apart - `metrics` | 3 | 756556 | 50437 | 0.292 | 0.521 | 0.705 | 1.101 | 10.7 | 0 / 0 |
| C3t HEAD, 16 readers, connect + warm-up before timing, start staggered 40 ms apart - `metrics` | 4 | 748194 | 49880 | 0.295 | 0.528 | 0.713 | 1.087 | 8.8 | 0 / 0 |
| C3t HEAD, 16 readers, connect + warm-up before timing, start staggered 40 ms apart - `metrics` | 5 | 751714 | 50114 | 0.293 | 0.526 | 0.717 | 1.132 | 18.0 | 0 / 0 |
| C3h control: 16 readers on `/healthz` (no auth, no metrics middleware) - `healthz` | 1 | 1083595 | 72240 | 0.210 | 0.335 | 0.423 | 0.595 | 31.9 | 0 / 0 |
| C3h control: 16 readers on `/healthz` (no auth, no metrics middleware) - `healthz` | 2 | 1082710 | 72181 | 0.208 | 0.341 | 0.445 | 0.679 | 32.1 | 0 / 0 |
| C3h control: 16 readers on `/healthz` (no auth, no metrics middleware) - `healthz` | 3 | 1053526 | 70235 | 0.214 | 0.353 | 0.468 | 0.757 | 24.3 | 0 / 0 |
| C3h control: 16 readers on `/healthz` (no auth, no metrics middleware) - `healthz` | 4 | 1072300 | 71487 | 0.213 | 0.339 | 0.426 | 0.586 | 49.0 | 0 / 0 |
| C3h control: 16 readers on `/healthz` (no auth, no metrics middleware) - `healthz` | 5 | 1085368 | 72358 | 0.209 | 0.337 | 0.428 | 0.604 | 31.2 | 0 / 0 |
| C3s 4 readers on `/v1/metrics/system` - `metrics` | 1 | 439543 | 29303 | 0.124 | 0.177 | 0.204 | 0.284 | 13.9 | 0 / 0 |
| C3s 4 readers on `/v1/metrics/system` - `metrics` | 2 | 432057 | 28804 | 0.126 | 0.181 | 0.210 | 0.252 | 6.5 | 0 / 0 |
| C3s 4 readers on `/v1/metrics/system` - `metrics` | 3 | 434639 | 28976 | 0.125 | 0.180 | 0.207 | 0.280 | 14.0 | 0 / 0 |
| C3s 4 readers on `/v1/metrics/system` - `metrics` | 4 | 433661 | 28911 | 0.125 | 0.181 | 0.208 | 0.254 | 16.9 | 0 / 0 |
| C3s 4 readers on `/v1/metrics/system` - `metrics` | 5 | 435748 | 29050 | 0.125 | 0.179 | 0.207 | 0.253 | 7.9 | 0 / 0 |
| C3n 6 readers, NEW TCP connection per request, paced 80 req/s each - `metrics` | 1 | 7199 | 480 | 2.172 | 25.107 | 28.629 | n/a (n<10,000) | 36.3 | 0 / 0 |
| C3n 6 readers, NEW TCP connection per request, paced 80 req/s each - `metrics` | 2 | 7195 | 480 | 2.556 | 26.564 | 29.735 | n/a (n<10,000) | 40.6 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `sqlread` | 1 | 291779 | 19452 | 0.652 | 1.691 | 3.102 | 6.355 | 106.7 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 1 | 105561 | 7037 | 0.476 | 1.238 | 1.854 | 3.148 | 115.1 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `sqlread` | 2 | 311502 | 20767 | 0.652 | 1.334 | 2.357 | 5.643 | 75.0 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 2 | 112192 | 7480 | 0.470 | 0.999 | 1.477 | 3.578 | 35.8 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `sqlread` | 3 | 298117 | 19874 | 0.650 | 1.573 | 2.904 | 6.427 | 58.9 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 3 | 108548 | 7236 | 0.469 | 1.151 | 1.725 | 3.170 | 34.0 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `sqlread` | 4 | 287289 | 19153 | 0.659 | 1.713 | 3.197 | 6.834 | 67.6 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 4 | 104124 | 6942 | 0.480 | 1.246 | 1.888 | 3.584 | 51.8 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `sqlread` | 5 | 282223 | 18815 | 0.666 | 1.744 | 3.263 | 7.017 | 215.0 | 0 / 0 |
| C4  HEAD, 16 SQL read clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 5 | 101825 | 6788 | 0.486 | 1.282 | 1.906 | 3.719 | 214.0 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `sqlread` | 1 | 253653 | 16910 | 0.809 | 1.778 | 2.869 | 5.757 | 84.7 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `queries` | 1 | 24740 | 1649 | 2.056 | 3.569 | 10.883 | 22.233 | 54.4 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `sqlread` | 2 | 255557 | 17037 | 0.808 | 1.740 | 2.787 | 5.537 | 126.1 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `queries` | 2 | 24554 | 1637 | 2.069 | 3.636 | 10.691 | 21.445 | 124.4 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `sqlread` | 3 | 266261 | 17751 | 0.799 | 1.543 | 2.374 | 4.772 | 53.5 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `queries` | 3 | 25668 | 1711 | 1.994 | 3.406 | 9.939 | 19.552 | 44.3 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `sqlread` | 4 | 260071 | 17338 | 0.803 | 1.666 | 2.649 | 5.359 | 63.8 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `queries` | 4 | 25284 | 1686 | 1.997 | 3.489 | 10.525 | 20.962 | 70.0 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `sqlread` | 5 | 237419 | 15828 | 0.865 | 1.843 | 2.877 | 6.282 | 115.3 | 0 / 0 |
| C4q HEAD, 16 SQL read clients + 4 closed-loop `/v1/observability/queries?limit=200` readers - `queries` | 5 | 24304 | 1620 | 2.060 | 3.625 | 12.568 | 25.428 | 91.5 | 0 / 0 |
| C5b HEAD, 16 SQL write clients (single-row INSERT) - `sqlwrite` | 1 | 3579 | 239 | 67.484 | 82.619 | 99.520 | n/a (n<10,000) | 115.4 | 0 / 0 |
| C5b HEAD, 16 SQL write clients (single-row INSERT) - `sqlwrite` | 2 | 3561 | 237 | 70.004 | 75.798 | 88.693 | n/a (n<10,000) | 100.0 | 0 / 0 |
| C5b HEAD, 16 SQL write clients (single-row INSERT) - `sqlwrite` | 3 | 3585 | 239 | 65.671 | 75.597 | 87.889 | n/a (n<10,000) | 99.8 | 0 / 0 |
| C5b HEAD, 16 SQL write clients (single-row INSERT) - `sqlwrite` | 4 | 3558 | 237 | 66.876 | 76.502 | 93.013 | n/a (n<10,000) | 100.7 | 0 / 0 |
| C5b HEAD, 16 SQL write clients (single-row INSERT) - `sqlwrite` | 5 | 3629 | 242 | 65.785 | 75.709 | 88.159 | n/a (n<10,000) | 116.7 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `sqlwrite` | 1 | 4467 | 298 | 52.817 | 63.449 | 76.698 | n/a (n<10,000) | 162.5 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 1 | 414984 | 27666 | 0.131 | 0.192 | 0.224 | 0.277 | 20.8 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `sqlwrite` | 2 | 4501 | 300 | 52.615 | 58.656 | 75.091 | n/a (n<10,000) | 123.5 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 2 | 419654 | 27977 | 0.129 | 0.190 | 0.221 | 0.271 | 19.4 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `sqlwrite` | 3 | 4550 | 303 | 51.773 | 59.545 | 77.254 | n/a (n<10,000) | 130.9 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 3 | 426537 | 28436 | 0.127 | 0.185 | 0.216 | 0.263 | 15.8 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `sqlwrite` | 4 | 4516 | 301 | 52.430 | 56.706 | 75.928 | n/a (n<10,000) | 94.2 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 4 | 427286 | 28486 | 0.127 | 0.185 | 0.214 | 0.255 | 20.8 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `sqlwrite` | 5 | 4536 | 302 | 52.333 | 56.377 | 75.469 | n/a (n<10,000) | 97.7 | 0 / 0 |
| C5  HEAD, 16 SQL write clients + 4 closed-loop `/v1/metrics/system` readers - `metrics` | 5 | 425496 | 28366 | 0.127 | 0.186 | 0.215 | 0.255 | 21.0 | 0 / 0 |

</details>

Resources per run (server process: CPU in cores averaged over the load window, RSS, private bytes, threads, handles; machine CPU):

<details><summary>Per-run process resources</summary>

| Condition | run | server CPU (cores, mean over window) | machine CPU % mean / max | RSS MB min-max | private MB max | threads min-max | handles min-max |
|---|---|---|---|---|---|---|---|
| C1_base_sqlread16 | 1 | 5.792 | 93.5 / 100.0 | 24.4-32.94 | 40.21 | 529-529 | 645-646 |
| C1_base_sqlread16 | 2 | 5.671 | 95.1 / 100.0 | 22.56-31.49 | 37.54 | 453-482 | 570-599 |
| C1_base_sqlread16 | 3 | 5.694 | 95.4 / 100.0 | 25.58-32.86 | 40.33 | 529-529 | 646-646 |
| C1_base_sqlread16 | 4 | 5.533 | 95.4 / 100.0 | 22.16-27.56 | 29.96 | 353-353 | 470-470 |
| C1_base_sqlread16 | 5 | 5.654 | 94.1 / 100.0 | 23.74-32.81 | 40.01 | 529-529 | 646-646 |
| C1b_head_sqlread16 | 1 | 5.689 | 95.4 / 100.0 | 21.75-29.11 | 32.17 | 385-385 | 503-503 |
| C1b_head_sqlread16 | 2 | 5.688 | 94.7 / 100.0 | 21.79-33.42 | 40.31 | 530-530 | 648-648 |
| C1b_head_sqlread16 | 3 | 5.633 | 97.4 / 100.0 | 22.98-28.08 | 30.12 | 353-353 | 471-471 |
| C1b_head_sqlread16 | 4 | 5.738 | 95.9 / 100.0 | 25.33-33.1 | 40.1 | 530-530 | 647-648 |
| C1b_head_sqlread16 | 5 | 5.613 | 95.8 / 100.0 | 23.0-28.87 | 31.42 | 379-379 | 497-497 |
| C2b_base_idle | 1 | 0.0 | 2.1 / 6.0 | 12.63-12.63 | 6.43 | 17-17 | 104-104 |
| C2b_base_idle | 2 | 0.001 | 2.0 / 4.9 | 12.82-12.82 | 6.65 | 17-17 | 104-104 |
| C2b_base_idle | 3 | 0.005 | 2.2 / 10.8 | 12.51-12.51 | 6.41 | 17-17 | 104-104 |
| C2b_base_idle | 4 | 0.002 | 2.6 / 6.1 | 12.82-12.82 | 6.46 | 17-17 | 106-106 |
| C2b_base_idle | 5 | 0.001 | 2.5 / 10.2 | 12.68-12.68 | 6.47 | 17-17 | 106-106 |
| C2_head_idle | 1 | 0.003 | 2.8 / 9.1 | 13.37-13.39 | 6.83 | 18-18 | 106-106 |
| C2_head_idle | 2 | 0.002 | 4.5 / 15.9 | 13.37-13.37 | 6.76 | 18-18 | 108-108 |
| C2_head_idle | 3 | 0.001 | 3.1 / 7.0 | 13.32-13.34 | 6.76 | 18-18 | 108-108 |
| C2_head_idle | 4 | 0.002 | 3.8 / 8.9 | 13.46-13.47 | 6.75 | 18-18 | 106-106 |
| C2_head_idle | 5 | 0.004 | 3.8 / 9.8 | 13.34-13.35 | 6.75 | 18-18 | 108-108 |
| C3_head_metrics16 | 1 | 4.637 | 96.7 / 100.0 | 14.96-15.27 | 9.15 | 18-18 | 136-136 |
| C3_head_metrics16 | 2 | 4.596 | 96.5 / 100.0 | 14.89-15.42 | 9.08 | 18-18 | 136-136 |
| C3_head_metrics16 | 3 | 4.745 | 96.5 / 100.0 | 14.87-15.52 | 9.11 | 18-18 | 136-136 |
| C3_head_metrics16 | 4 | 4.823 | 96.3 / 100.0 | 14.97-15.38 | 9.18 | 18-18 | 136-136 |
| C3_head_metrics16 | 5 | 4.683 | 96.9 / 100.0 | 14.99-15.46 | 9.13 | 18-18 | 136-136 |
| C3t_head_metrics16_staggered | 1 | 4.834 | 95.3 / 100.0 | 14.05-15.47 | 8.93 | 18-18 | 126-136 |
| C3t_head_metrics16_staggered | 2 | 5.066 | 95.4 / 100.0 | 14.02-15.56 | 8.93 | 18-18 | 126-136 |
| C3t_head_metrics16_staggered | 3 | 4.787 | 95.3 / 100.0 | 13.65-15.24 | 8.66 | 18-18 | 124-136 |
| C3t_head_metrics16_staggered | 4 | 4.834 | 95.6 / 100.0 | 13.67-15.27 | 8.94 | 18-18 | 128-136 |
| C3t_head_metrics16_staggered | 5 | 4.801 | 95.6 / 100.0 | 13.74-15.23 | 8.66 | 18-18 | 128-136 |
| C3h_head_healthz16 | 1 | 3.398 | 93.0 / 100.0 | 14.39-15.11 | 9.16 | 18-18 | 136-136 |
| C3h_head_healthz16 | 2 | 3.191 | 94.2 / 100.0 | 14.06-14.97 | 9.09 | 18-18 | 136-136 |
| C3h_head_healthz16 | 3 | 3.304 | 94.1 / 100.0 | 13.88-15.12 | 9.12 | 18-18 | 136-136 |
| C3h_head_healthz16 | 4 | 3.363 | 93.7 / 98.5 | 14.14-14.98 | 9.02 | 18-18 | 136-136 |
| C3h_head_healthz16 | 5 | 3.354 | 93.2 / 99.2 | 14.35-15.11 | 9.08 | 18-18 | 136-136 |
| C3s_head_metrics4 | 1 | 2.648 | 53.7 / 58.3 | 13.71-14.37 | 7.83 | 18-18 | 118-120 |
| C3s_head_metrics4 | 2 | 2.682 | 56.5 / 78.4 | 13.93-14.44 | 7.9 | 18-18 | 118-120 |
| C3s_head_metrics4 | 3 | 2.674 | 54.1 / 61.9 | 13.8-14.59 | 7.97 | 18-18 | 118-120 |
| C3s_head_metrics4 | 4 | 2.646 | 55.2 / 63.2 | 13.93-14.45 | 7.93 | 18-18 | 118-120 |
| C3s_head_metrics4 | 5 | 2.631 | 54.0 / 60.9 | 13.82-14.6 | 7.99 | 18-18 | 118-120 |
| C3n_head_metrics6_newconn_paced | 1 | 0.282 | 19.5 / 79.3 | 13.89-15.88 | 9.37 | 18-18 | 120-126 |
| C3n_head_metrics6_newconn_paced | 2 | 0.41 | 10.0 / 17.5 | 13.63-15.55 | 9.42 | 18-18 | 118-125 |
| C4_head_sqlread16_metrics4 | 1 | 5.671 | 94.8 / 100.0 | 24.65-33.88 | 40.67 | 530-530 | 649-652 |
| C4_head_sqlread16_metrics4 | 2 | 5.257 | 96.3 / 100.0 | 18.49-28.32 | 30.41 | 354-354 | 476-476 |
| C4_head_sqlread16_metrics4 | 3 | 5.458 | 95.1 / 100.0 | 24.69-32.35 | 37.91 | 480-480 | 602-602 |
| C4_head_sqlread16_metrics4 | 4 | 5.438 | 95.2 / 100.0 | 25.6-33.89 | 40.96 | 530-530 | 652-652 |
| C4_head_sqlread16_metrics4 | 5 | 5.473 | 95.1 / 100.0 | 19.71-33.66 | 40.66 | 530-530 | 648-652 |
| C4q_head_sqlread16_queries4 | 1 | 6.231 | 96.6 / 100.0 | 27.02-36.86 | 43.54 | 530-530 | 652-652 |
| C4q_head_sqlread16_queries4 | 2 | 6.195 | 97.0 / 100.0 | 27.06-36.75 | 43.51 | 530-530 | 652-652 |
| C4q_head_sqlread16_queries4 | 3 | 6.129 | 96.5 / 100.0 | 25.19-33.39 | 36.87 | 402-413 | 524-535 |
| C4q_head_sqlread16_queries4 | 4 | 6.195 | 96.7 / 100.0 | 27.11-36.1 | 42.25 | 506-506 | 628-628 |
| C4q_head_sqlread16_queries4 | 5 | 6.241 | 96.6 / 100.0 | 24.43-34.23 | 38.32 | 435-435 | 557-557 |
| C5b_head_sqlwrite16 | 1 | 0.338 | 13.0 / 25.8 | 15.92-16.79 | 9.51 | 34-34 | 144-145 |
| C5b_head_sqlwrite16 | 2 | 0.402 | 12.2 / 26.2 | 16.15-17.14 | 9.83 | 34-34 | 144-145 |
| C5b_head_sqlwrite16 | 3 | 0.356 | 9.2 / 17.3 | 15.81-16.62 | 9.44 | 34-34 | 144-145 |
| C5b_head_sqlwrite16 | 4 | 0.336 | 10.1 / 21.1 | 15.92-16.79 | 9.61 | 34-34 | 144-145 |
| C5b_head_sqlwrite16 | 5 | 0.342 | 11.0 / 21.9 | 15.99-16.98 | 9.84 | 34-34 | 146-147 |
| C5_head_sqlwrite16_metrics4 | 1 | 2.803 | 61.2 / 80.3 | 16.41-18.4 | 11.3 | 34-35 | 152-156 |
| C5_head_sqlwrite16_metrics4 | 2 | 2.774 | 59.0 / 75.5 | 17.01-18.84 | 12.1 | 34-35 | 155-156 |
| C5_head_sqlwrite16_metrics4 | 3 | 2.778 | 57.7 / 86.5 | 16.95-18.41 | 11.3 | 34-35 | 153-158 |
| C5_head_sqlwrite16_metrics4 | 4 | 2.819 | 57.1 / 64.9 | 17.02-18.58 | 11.54 | 34-34 | 153-155 |
| C5_head_sqlwrite16_metrics4 | 5 | 2.78 | 56.5 / 65.0 | 16.62-18.58 | 11.55 | 34-34 | 153-155 |

</details>

File-system I/O: process-level counters vs device-level counters of the data volume (`E:` = PhysicalDrive0) and temp volume (`C:` = PhysicalDrive1), whole run:

<details><summary>Per-run I/O counters</summary>

| Condition | run | process-level I/O (GetProcessIoCounters): write ops / MB | device-level, data volume E: (PhysicalDrive0): write ops / MB | device-level, temp volume C: (PhysicalDrive1): write ops / MB | E: device writes ÷ process write ops | E: device MB ÷ process MB |
|---|---|---|---|---|---|---|
| C5b_head_sqlwrite16 | 1 | 3579 / 0.193 | 7384 / 35.41 | 116 / 1.15 | 2.06 | 183 |
| C5b_head_sqlwrite16 | 2 | 3561 / 0.192 | 7188 / 33.98 | 114 / 1.2 | 2.02 | 177 |
| C5b_head_sqlwrite16 | 3 | 3585 / 0.194 | 7234 / 34.17 | 82 / 0.71 | 2.02 | 176 |
| C5b_head_sqlwrite16 | 4 | 3558 / 0.192 | 7166 / 33.82 | 70 / 0.72 | 2.01 | 176 |
| C5b_head_sqlwrite16 | 5 | 3629 / 0.196 | 7325 / 34.66 | 180 / 1.72 | 2.02 | 177 |
| C5_head_sqlwrite16_metrics4 | 1 | 4467 / 0.241 | 9043 / 42.59 | 113 / 0.99 | 2.02 | 177 |
| C5_head_sqlwrite16_metrics4 | 2 | 4501 / 0.243 | 9084 / 42.84 | 93 / 1.13 | 2.02 | 176 |
| C5_head_sqlwrite16_metrics4 | 3 | 4550 / 0.246 | 9174 / 43.2 | 77 / 0.68 | 2.02 | 176 |
| C5_head_sqlwrite16_metrics4 | 4 | 4516 / 0.244 | 9109 / 42.95 | 91 / 1.02 | 2.02 | 176 |
| C5_head_sqlwrite16_metrics4 | 5 | 4536 / 0.245 | 9133 / 42.97 | 91 / 0.79 | 2.01 | 175 |
| C3_head_metrics16 | 1 | 0 / 0.0 | 39 / 0.22 | 152 / 1.34 | n/a (process wrote 0) | n/a |
| C3_head_metrics16 | 2 | 0 / 0.0 | 27 / 0.21 | 181 / 2.82 | n/a (process wrote 0) | n/a |
| C3_head_metrics16 | 3 | 0 / 0.0 | 30 / 0.17 | 156 / 1.33 | n/a (process wrote 0) | n/a |
| C3_head_metrics16 | 4 | 0 / 0.0 | 34 / 0.28 | 94 / 0.83 | n/a (process wrote 0) | n/a |
| C3_head_metrics16 | 5 | 0 / 0.0 | 29 / 0.21 | 73 / 0.7 | n/a (process wrote 0) | n/a |
| C1b_head_sqlread16 | 1 | 0 / 0.0 | 29 / 0.21 | 94 / 0.83 | n/a (process wrote 0) | n/a |
| C1b_head_sqlread16 | 2 | 0 / 0.0 | 28 / 0.22 | 115 / 1.14 | n/a (process wrote 0) | n/a |
| C1b_head_sqlread16 | 3 | 0 / 0.0 | 26 / 0.14 | 96 / 0.93 | n/a (process wrote 0) | n/a |
| C1b_head_sqlread16 | 4 | 0 / 0.0 | 29 / 0.18 | 150 / 1.13 | n/a (process wrote 0) | n/a |
| C1b_head_sqlread16 | 5 | 0 / 0.0 | 12 / 0.15 | 49 / 0.48 | n/a (process wrote 0) | n/a |
| C2_head_idle | 1 | 0 / 0.0 | 28 / 0.22 | 72 / 1.15 | n/a (process wrote 0) | n/a |
| C2_head_idle | 2 | 0 / 0.0 | 16 / 0.15 | 64 / 0.65 | n/a (process wrote 0) | n/a |
| C2_head_idle | 3 | 0 / 0.0 | 30 / 0.16 | 249 / 18.65 | n/a (process wrote 0) | n/a |
| C2_head_idle | 4 | 0 / 0.0 | 31 / 0.22 | 122 / 1.41 | n/a (process wrote 0) | n/a |
| C2_head_idle | 5 | 0 / 0.0 | 28 / 0.17 | 61 / 0.59 | n/a (process wrote 0) | n/a |

</details>

Observability cadence seen by the 20 Hz observer (health values, snapshot age, `sizes_age_ms`):

| Condition | runs | health values seen | largest snapshot age seen ms (min..max over runs) | `sizes_age_ms` range seen | highest sample generation reached (one per second since start) |
|---|---|---|---|---|---|
| C1b_head_sqlread16 | 5 | healthy | 1001..1012 | 0..9990 | 21..21 |
| C2_head_idle | 5 | healthy | 1002..1014 | 0..9997 | 21..21 |
| C3_head_metrics16 | 5 | healthy | 1003..1020 | 0..9996 | 21..21 |
| C3t_head_metrics16_staggered | 5 | healthy | 1002..1014 | 0..9996 | 22..22 |
| C3h_head_healthz16 | 5 | healthy | 1001..1010 | 0..9996 | 21..21 |
| C3s_head_metrics4 | 5 | healthy | 1005..1011 | 0..9998 | 21..21 |
| C3n_head_metrics6_newconn_paced | 2 | healthy | 1013..1014 | 0..9999 | 21..21 |
| C4_head_sqlread16_metrics4 | 5 | healthy | 1010..1019 | 0..9994 | 21..21 |
| C4q_head_sqlread16_queries4 | 5 | healthy | 998..1011 | 0..9989 | 21..21 |
| C5b_head_sqlwrite16 | 5 | healthy | 1003..1014 | 0..9997 | 21..21 |
| C5_head_sqlwrite16_metrics4 | 5 | healthy | 1005..1012 | 0..9998 | 21..21 |

Slow requests (> 50 ms) and where they fall in the run, with the server's own view:

| Condition | run | requests > 50 ms | of which in the first 100 ms of the run | later than 100 ms | worst request ms | server handler self-time max ms (`latency_of_response_ms`) | server route-table max ms (handler only) | machine CPU % mean |
|---|---|---|---|---|---|---|---|---|
| C1_base_sqlread16 | 1 | 2 | 2 | 0 | 105.7 | n/a | n/a | 93.5 |
| C1_base_sqlread16 | 2 | 0 | 0 | 0 | 0.0 | n/a | n/a | 95.1 |
| C1_base_sqlread16 | 3 | 1 | 0 | 1 | 78.9 | n/a | n/a | 95.4 |
| C1_base_sqlread16 | 4 | 0 | 0 | 0 | 0.0 | n/a | n/a | 95.4 |
| C1_base_sqlread16 | 5 | 2 | 2 | 0 | 61.8 | n/a | n/a | 94.1 |
| C1b_head_sqlread16 | 1 | 0 | 0 | 0 | 0.0 | n/a | 0.349 | 95.4 |
| C1b_head_sqlread16 | 2 | 0 | 0 | 0 | 0.0 | n/a | 0.346 | 94.7 |
| C1b_head_sqlread16 | 3 | 2 | 0 | 2 | 58.9 | n/a | 2.725 | 97.4 |
| C1b_head_sqlread16 | 4 | 2 | 1 | 1 | 127.8 | n/a | 1.049 | 95.9 |
| C1b_head_sqlread16 | 5 | 0 | 0 | 0 | 0.0 | n/a | 0.623 | 95.8 |
| C3_head_metrics16 | 1 | 4 | 4 | 0 | 108.0 | 8.953 | 19.569 | 96.7 |
| C3_head_metrics16 | 2 | 1 | 1 | 0 | 54.0 | 12.979 | 13.067 | 96.5 |
| C3_head_metrics16 | 3 | 1 | 1 | 0 | 50.3 | 10.993 | 16.589 | 96.5 |
| C3_head_metrics16 | 4 | 3 | 3 | 0 | 61.6 | 2.429 | 4.930 | 96.3 |
| C3_head_metrics16 | 5 | 5 | 5 | 0 | 93.0 | 7.663 | 7.721 | 96.9 |
| C3t_head_metrics16_staggered | 1 | 0 | 0 | 0 | 0.0 | 1.200 | 2.499 | 95.3 |
| C3t_head_metrics16_staggered | 2 | 0 | 0 | 0 | 0.0 | 1.728 | 2.497 | 95.4 |
| C3t_head_metrics16_staggered | 3 | 0 | 0 | 0 | 0.0 | 2.435 | 10.424 | 95.3 |
| C3t_head_metrics16_staggered | 4 | 0 | 0 | 0 | 0.0 | 3.683 | 8.119 | 95.6 |
| C3t_head_metrics16_staggered | 5 | 0 | 0 | 0 | 0.0 | 2.010 | 17.712 | 95.6 |
| C3h_head_healthz16 | 1 | 0 | 0 | 0 | 0.0 | n/a | 0.401 | 93.0 |
| C3h_head_healthz16 | 2 | 0 | 0 | 0 | 0.0 | n/a | 0.638 | 94.2 |
| C3h_head_healthz16 | 3 | 0 | 0 | 0 | 0.0 | n/a | 0.384 | 94.1 |
| C3h_head_healthz16 | 4 | 0 | 0 | 0 | 0.0 | n/a | 5.955 | 93.7 |
| C3h_head_healthz16 | 5 | 0 | 0 | 0 | 0.0 | n/a | 0.400 | 93.2 |
| C3s_head_metrics4 | 1 | 0 | 0 | 0 | 0.0 | 0.098 | 0.449 | 53.7 |
| C3s_head_metrics4 | 2 | 0 | 0 | 0 | 0.0 | 0.317 | 0.380 | 56.5 |
| C3s_head_metrics4 | 3 | 0 | 0 | 0 | 0.0 | 0.422 | 0.444 | 54.1 |
| C3s_head_metrics4 | 4 | 0 | 0 | 0 | 0.0 | 0.352 | 0.374 | 55.2 |
| C3s_head_metrics4 | 5 | 0 | 0 | 0 | 0.0 | 0.097 | 0.397 | 54.0 |
| C4_head_sqlread16_metrics4 | 1 | 4 | 4 | 0 | 115.1 | 6.765 | 11.580 | 94.8 |
| C4_head_sqlread16_metrics4 | 2 | 2 | 2 | 0 | 75.0 | 18.106 | 18.139 | 96.3 |
| C4_head_sqlread16_metrics4 | 3 | 2 | 1 | 1 | 58.9 | 9.226 | 11.992 | 95.1 |
| C4_head_sqlread16_metrics4 | 4 | 3 | 3 | 0 | 67.6 | 20.894 | 20.924 | 95.2 |
| C4_head_sqlread16_metrics4 | 5 | 8 | 8 | 0 | 215.0 | 7.537 | 28.550 | 95.1 |
| C4q_head_sqlread16_queries4 | 1 | 4 | 3 | 1 | 84.7 | n/a | 0.306 | 96.6 |
| C4q_head_sqlread16_queries4 | 2 | 7 | 7 | 0 | 126.1 | n/a | 0.331 | 97.0 |
| C4q_head_sqlread16_queries4 | 3 | 1 | 1 | 0 | 53.5 | n/a | 1.307 | 96.5 |
| C4q_head_sqlread16_queries4 | 4 | 3 | 3 | 0 | 70.0 | n/a | 0.349 | 96.7 |
| C4q_head_sqlread16_queries4 | 5 | 7 | 6 | 1 | 115.3 | n/a | 0.660 | 96.6 |

Reproduction with the **original** harness shape (`mp.Pool(16)`, one `http.client` connection per reader opened before timing, 15 s, as in the earlier `obs5.py`; 5 runs instead of 3; records every request slower than 20 ms):

| run | requests | req/s | p50 ms | p95 ms | p99 ms | p99.9 ms | max ms | > 20 ms | > 100 ms | > 300 ms | > 1,000 ms |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 367211 | 24481 | 0.578 | 0.897 | 2.718 | 10.051 | 36.0 | 12 | 0 | 0 | 0 |
| 2 | 371041 | 24736 | 0.571 | 0.882 | 2.687 | 10.080 | 40.4 | 17 | 0 | 0 | 0 |
| 3 | 371551 | 24770 | 0.570 | 0.880 | 2.684 | 10.605 | 40.5 | 18 | 0 | 0 | 0 |
| 4 | 371169 | 24745 | 0.571 | 0.880 | 2.648 | 10.344 | 65.7 | 20 | 0 | 0 | 0 |
| 5 | 371034 | 24736 | 0.570 | 0.881 | 2.730 | 10.270 | 38.0 | 17 | 0 | 0 | 0 |

### 6.3 What the data shows

1. **Not reproduced.** Slowest `/v1/metrics/system` request in this session: 214.0 ms (12,107,104 requests, 27 runs); original-harness shape: 65.7 ms (1,852,006 requests, 5 runs); none above 1 s anywhere. The certification's 387 ms–1.35 s are not seen. p99 here is 0.7–0.8 ms (synchronized, own harness) and 2.7 ms (original shape) vs 3.8–3.9 ms in the certification; p99.9 0.96–1.4 ms and 10 ms respectively.
2. **Every > 50 ms request in the 16-reader endpoint runs is in the first 100 ms of the run** (C3: 14 of 14; C4: 18 of 19; C4q: 20 of 22), starts within ≤ 2.7 ms of the others, and ends spread over up to 156 ms — a cold, synchronized start (16–20 client processes, connections opened inside the timed region, a server that had been idle for seconds), not steady state. The original harness shape shows the same: its > 30 ms requests sit at offset 0.0 s.
3. **Removing the synchronized start removes them.** C3t (connect and one warm-up request before timing, starts staggered 40 ms): 3,731,959 requests, **0 above 50 ms, maximum 3.0–18.0 ms**, p99 0.66–0.72 ms, at the same ~95 % machine CPU.
4. **Dose-response with CPU saturation.** 4 readers (machine CPU ~54 %): client max 6.5–16.9 ms, in-handler max 0.10–0.42 ms. 16 readers (machine CPU ~96 %): in-handler max up to 13 ms (C3) and 21 ms (C4). The in-handler time (`latency_of_response_ms`: JSON build only, p50 18.6 µs) can only exceed milliseconds if its thread lost its CPU — OS preemption on a saturated 8-thread machine, not endpoint work.
5. **Not specific to the endpoint or to observability.** `/healthz` (no auth, no rate limiter, no route-table mutex) under the same 16 readers: max 24–49 ms, zero requests above 50 ms; the pre-observability baseline binary's SQL path (C1) has 5 slow requests in 5 runs (max 105.7 ms), HEAD's SQL path (C1b) 4 (max 127.8 ms), with 3 isolated single-client 52–59 ms events later in the run. New TCP connections (C3n, C3nh): p95 25–29 ms for 5 % of connections on **both** HEAD and the baseline binary, on `/healthz` as well as the metrics endpoint — a property of this host's loopback accept path, not of the code under test.

### 6.4 Candidate causes, one at a time

| Candidate | Evidence | Verdict |
|---|---|---|
| OS probe calls (`GetProcessTimes`, `K32GetProcessMemoryInfo`, `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW`, `GetProcessIoCounters`) | They run only on the sampler thread (`sampler.rs:301-307`), never on the request path. Per-call latency measured from a scratch process (`probe_bench.py`, 20,000 calls each): idle p50 1.5–2.6 µs (disk-free 16 µs), max 33–76 µs (disk-free 548 µs); **under 16 readers** p50 2.6–4.3 µs, p99 244–282 µs (disk-free 853 µs), max 0.69–1.46 ms (disk-free 2.35 ms) — preemption jitter, bounded to a few ms. Sampler cadence at 47 k req/s and 96 % CPU: snapshot age ≤ 1,020 ms over 53 observed runs, i.e. a tick never late by more than 20 ms | Eliminated as a cause of request latency |
| Directory-size walk | Sampler thread only, every 10 s (`sampler.rs:32,343-350`); `sizes_age_ms` 0–9,999 ms. A synthetic tree with the certified endurance shape (3,300 SSTable files + WAL), walked three times as the sampler does: 33 ms idle (max 49) / 84 ms under load (max 100) in Python; no request-path involvement | Eliminated for request latency; costs the sampler thread tens of ms per 10 s |
| Snapshot serialization / clone | In-handler self time: p50 18.6 µs, p99 0.14 ms; max 0.10–0.42 ms with 4 readers, 1.2–3.7 ms staggered at 16 readers, up to 13 ms only under synchronized saturation | Eliminated (bounded, scales with CPU starvation) |
| Registry lock contention (sessions, queries, events, route metrics) | The endpoint touches none of the registries; its middleware takes the rate-limiter and route-table mutexes per request. At 47–50 k req/s no request after warm-up exceeded 18 ms. **Separately**, `/v1/observability/queries` holds the query-registry mutex while it sorts up to 2,248 records: with 4 closed-loop pollers (6.5 k calls/s) SQL read throughput fell from 20.1–25.4 k to 15.8–17.8 k req/s and p50 rose 0.51–0.58 → 0.80–0.87 ms (C4q vs C1b); C4 (4 pollers of the metrics endpoint, 7 k calls/s) cost ~4–13 %. Lock-vs-CPU attribution **not proven** (no instrumentation allowed) | Not the cause of the endpoint's tail; a real, separate cost of the queries endpoint (matrix A32 neighbour; proposal P11) |
| Allocation spikes | Not directly observable without instrumentation. RSS 13.65–15.56 MB under 16 readers (no growth), private bytes ≤ 9.2 MB, handler max ≤ 3.7 ms staggered | No evidence |
| Tokio runtime scheduling | 8 worker threads; metrics-only runs keep 18 threads. In-handler maxima track CPU saturation (above). The runtime is not instrumented (no tokio metrics enabled); no scheduler-level cause is proven | Consistent with OS-level starvation, not proven at tokio level |
| Windows API latency under load | See OS probe row | Eliminated beyond ms-scale jitter |
| Antivirus / filter driver | Defender real-time protection is on. Loopback TCP is not a file-I/O path; the file-I/O paths (lock-probe `CreateFileW` 1/s, directory walk, WAL) are off the request path. Filter-driver latency cannot be observed without Process Monitor / elevation | NOT TESTED (unobservable here) |
| Connection establishment | C3n/C3nh above | Real on this host, generic, identical for the baseline binary |
| Client-side artefacts | Synchronized start of 16 spinning/waking Python processes; first request on a cold connection | Established as the source of all > 50 ms requests in the 16-reader runs |

### 6.5 Classification of the result

Per the mission's rule: **not reproducible in this session → OPEN** (matrix A66; item B classified NOT TESTED ENVIRONMENT LIMITATION). No cause for the original 387 ms–1.35 s is proven. Nothing in the evidence points to an observability-code defect, so no fix is proposed for it; if the maintainer wants the original observation closed rather than accepted as unreproducible, the only remaining experiment is a long run under the original host conditions.

### 6.6 Overhead of the layer vs baseline (supports matrix A67; no acceptance threshold exists)

| Measurement (5 runs each) | Baseline binary | HEAD binary |
|---|---|---|
| Idle RSS, MB (C2b / C2) | 12.51–12.82 (median 12.68) | 13.32–13.47 (median 13.37) → **+0.69 MB** |
| Idle threads / handles | 17 / 104–106 | 18 / 106–108 → **+1 / +2** |
| Idle CPU (cores, 15 s window) | 0.000–0.005 | 0.001–0.004 (indistinguishable at the OS counter's resolution) |
| 16-client SQL read: p50 / p95 / p99 ms, medians (C1 / C1b) | 0.545 / 1.676 / 3.285 | 0.558 / 1.437 / 2.755 |
| 16-client SQL read: throughput req/s, min / median / max | 21,025 / 21,403 / 25,713 | 20,130 / 21,626 / 25,422 |
| Threads / RSS peak under that load | 353–529 / 27.6–32.9 MB | 353–530 / 28.1–33.4 MB |

The two SQL-read distributions overlap completely and are bimodal in both binaries (a ~25 k req/s mode and a ~21 k req/s mode). No difference attributable to the layer can be claimed; equally, with no approved threshold, no PASS can be assigned.

## 7. Response cost (§7 of the mission)

**By source.** `GET /v1/metrics/system` (`metrics_system.rs:27-172`) reads `state.obs.sampler.latest()`, `state.config` and `uptime(state)`; a grep of the file for `state.engine`, `state.sql` and `sessions.` finds nothing. It does not scan the database or a table, execute SQL, take an engine lock, or walk the file system. Per request it passes `auth_middleware` (rate-limiter mutex) and `metrics_middleware` (route-table mutex at completion).

**By one runtime check** (`cost_check.py`): 50,000 sequential `GET /v1/metrics/system` (4.5 s; p50 0.086 ms, p99 0.148 ms, max 0.61 ms) against a live instance holding the 20,000-row table, counters read from `/v1/metrics` and `/v1/admin/status` before and after:

| Counter | Δ over 50,000 requests |
|---|---|
| `read.read_requests`, `read.blocks_read`, `read.sstables_consulted` | 0, 0, 0 |
| `write.submitted`, `write.completed_ok`, `admin.wal.submitted`, `admin.wal.sync_attempts` | 0, 0, 0, 0 |
| `route POST /v1/sql`, `admin.queries.requests` | 0, 0 |
| compaction cycles | 0 |
| process I/O (`GetProcessIoCounters`): read / write ops and bytes | 0 / 0 / 0 / 0 |

**Does it block commits?** Not by construction (no engine call). Measured: writes with 4 closed-loop endpoint readers (C5) were not slower than writes alone (C5b): p50 51.8–52.8 ms vs 65.7–70.0 ms, throughput 298–303 vs 237–242 req/s — an improvement whose cause was not investigated (possibly CPU power-state or group-commit window dynamics; not measured).

**What the *sampler* (not the endpoint) touches, once per second**, stated precisely because `sampler.rs:3-11` claims it takes no lock "the engine's write path holds across work": engine read locks `lock_immutables_read` and `lock_sstables_read`; the batch-coordinator write-queue mutex (`pool.stats()`, `batch_coordinator.rs:539`); and the WAL `Mutex<FileWal>` **twice** (`committer.stats()` → `next_seq()` and `current_segment_id()`, `group_commit.rs:1144,1148`). The WAL module documents that this mutex is "held only for memory-speed operations — never across the `fsync`" (`group_commit.rs:16-19`), so the claim holds if that invariant does; hold times were not measured here. Consequence worth knowing: a stall of that mutex stalls the *sampler*, which the endpoint then reports as stale/failed — a correct symptom, attributed to the sampler.

**Periodic file-system walk and staleness.** The walk runs only in the sampler (`SIZES_EVERY` 10 s, `sampler.rs:32,343-350`); its age is exposed (`disk.sizes_age_ms`, `metrics_system.rs:100`) and was observed between 0 and 9,999 ms (never above the period) in 53 runs and 3,006–7,002 ms in 100 sampled responses of the cost run; a value is never presented as newer than it is. (`sizes_age_ms` is a wall-clock subtraction; a backwards clock step would clamp it to 0.)

## 8. Freshness (§8 of the mission)

Every `/v1/metrics/system` response carries: `timestamp_unix_ms` (response time), `sample_freshness.last_sample_ms` (when the snapshot was taken), `sample_freshness.age_ms` (computed per request), `sample_generation`, `sample_freshness.state` (`running | degraded | failed | not_started`, derived from age as well as stored state), `sample_freshness.stale` (age > max(5 s, 3 ticks)), `tick_ms`. "Failed" is the value `state == "failed"` (no separate boolean). An operator can tell whether a value is current from `age_ms` + `stale` + `state`. Live: 53 observer series show a new generation each second and snapshot age ≤ 1,020 ms at up to 72 k req/s of load.

| Case | Existing test | Status |
|---|---|---|
| Normal sampling | `metrics_system_has_every_required_field_with_the_right_types` (last_sample_ms, age_ms numeric, `running`, `stale:false`, generation ≥ 5) | PASS (fresh) |
| Delayed sampler | `a_wedged_sampler_goes_stale_then_failed_and_recovers_when_unblocked` (at 6 s: `stale:true`, age ≥ 5,000 ms, state still `running`) | PASS |
| Wedged → failed | same test (at 11 s: `state:failed`, age ≥ 10,000 ms, last good snapshot returned with its true age, SQL still works) | PASS |
| Failed tick (probe failure) | `cpu_rss_and_disk_read_failures_null_the_field_degrade_the_sampler_and_never_stop_it` | PASS |
| Sampler panic contained | `a_panic_in_a_tick_is_contained_and_the_sampler_recovers` | PASS; **5 consecutive panics → `failed` NOT TESTED** (A16) |
| Recovery after failure | wedge test (→ `running`, `stale:false`), panic test (→ `running`), probe test | PASS |
| Thread stopped | `a_dead_sampler_is_visible_as_stale_and_failed_never_as_current_data` (`not_started`, age grows ≥ 400 ms) | PASS (asserts `not_started` + growing age; the > 5 s `stale` flip for a stopped thread uses the same derivation as the wedge test) |
| No snapshot yet | none | NOT TESTED (A14: `stale:false`, `age_ms:null`) |
| Sampler start failure | none | NOT TESTED / OPEN (A17: swallowed at `host.rs:293`) |

## 9. Sessions, queries, events, correlation (§9–§12 of the mission)

| Property | Sessions | Queries | Events |
|---|---|---|---|
| Bound exists and is enforced | ≤ 200 returned (`observability.rs:19`); registry bounded by per-principal cap 50 and the global transaction cap — **except** the observation map `meta`, which a disconnect can grow without limit (**FAIL**, A25/A58) | 200 finished (`pop_front`) + 2,048 in flight (overflow counted, never queued); ≤ 200 returned (`queries.rs:18-20,139-157,249-252`) | 256 per ring × 2; ≤ 200 returned; `limit` clamped 1..200, non-integer → 400 (`events.rs:17-19`, `observability.rs:21-31`) |
| Deterministic ordering | by session id (`sql_session.rs:273-274`); test asserts `ids.windows(2)` ascending | newest start first, ties by id (`queries.rs:187`); test asserts non-increasing `start_unix_ms`. (The doc comment at `queries.rs:178` says running statements come first; the code sorts all together — comment inaccurate, harmless) | newest first (`last()` reverses the ring); test asserts non-increasing `timestamp` |
| No secrets / SQL / parameters / row content | fields are ids, ages, closed-set states (no principal, no SQL) | `statement_class` and `error_class` are `&'static str` from closed lists; test `queries_endpoint_lists_bounded_records_never_sql_text` plants marker strings in table, column, value, parse-error and unknown-table positions and asserts none is returned | every textual field is `&'static str`; ids are numbers/UUIDs; `no_api_key_appears_in_any_observability_response` PASS |
| Correlation ids only as diagnostic fields | `session_id` | `query_id` | `request_id`, `session_id`; none is a metric key or label (route labels `routes/mod.rs:34-48`, series names `ring.rs:23-32` are closed constants) |
| No unbounded growth from a request | **FAIL** (ghost entries, reproduced: 60 of 60) | PASS by construction + unit test | PASS: 640 inserts leave 200 returned, operational events survive a security flood |
| Tracked / untracked consistency | `active_sessions` gauge counts `meta` (inflated by ghosts; `active_transactions` stays correct) | `active()` counts tracked + untracked; `untracked_active` is cumulative (**FAIL**, A32) | n/a |
| Oldest-eviction / newest-retention | n/a (entries are removed, not evicted) | oldest evicted by `pop_front`; the test asserts only the count, not which — **NOT TESTED** (by construction) | oldest evicted; newest retention asserted by unit `rings_are_bounded_separate_and_newest_first` (`sec[0].request_id` = last pushed) |

**Reproduction of the session defect** (`ghost_test.py`, real instance, real sockets): 60 times — `BEGIN`; send a heavy `SELECT` (89 ms when allowed to finish) carrying the `session_id`; abort the socket (RST) 2 ms later. Result 3 s later: `/v1/observability/sessions` `total 60`, all 60 `executing`, `active_sessions` 60, `active_transactions` 0; `COMMIT` on 10 of them: 10 of 10 non-200 (the sessions no longer exist); 23 s later: still 60, oldest age 7.1 → 27.3 s; the per-principal cap (50) did not stop the 60 `BEGIN`s. Mechanism: `routes/sql.rs:596-645,685-737` take the session out of `sessions` and call `finish`/`put_back` only when the statement future completes; a dropped future does neither (the transaction itself is rolled back when the blocking task ends, which is why `active_transactions` is right); `reap_expired` iterates `sessions`, not `meta` (`sql_session.rs:333-355`). With one `local` principal this is reachable by the product's own GUI/CLI (closing a tab or killing a client mid-statement inside a transaction).

## 10. Version, resources, disk I/O (§13–§15 of the mission)

**Version** (`/v1/observability/version`, live): HEAD release build → `{product_version "0.1.0", build_identifier "0.1.0-release-x86_64-windows", git_revision "2cbd5a7e2b18", startup_timestamp_unix_ms …}`; `git rev-parse --short=12 HEAD` = `2cbd5a7e2b18` — **truthful**. Release build of `git archive HEAD` outside any repository → `git_revision: null` — **unknown is `null`, never invented**. No local path, user name, host name or credential fragment in either response (substring check against drive letters, `Users`, the account name, the computer name). Developer (debug) vs release: by source the only difference is the profile token in `build_identifier`; a debug no-git build was not produced (A09). Caveats found by reading: `-dirty` freshness depends on `build.rs` re-run triggers (A10); `startup_timestamp_unix_ms` and `instance.uptime_seconds` are `AppState` creation time, i.e. after engine recovery, not process start.

**Resources — every field classified**

| Field(s) | Classification | Note |
|---|---|---|
| `timestamp_unix_ms`, `sample_freshness.*`, `sample_generation`, `latency_of_response_ms` | MEASURED-TRUTHFUL | `timestamp` is the response time, `last_sample_ms` the sample time |
| `instance.id`, `instance.name`, `uptime_seconds` | MEASURED-TRUTHFUL | uptime counts from `AppState` creation |
| `instance.healthy` | **MISLEADING** (until policy approved) | a policy-dependent judgement presented as a fact (section 5) |
| `instance.readiness` | **MISLEADING** | differs in meaning from `/readyz.ready` (A05) |
| `cpu.process_percent`, `cpu.peak_percent` | MEASURED-TRUTHFUL, NULL-WHEN-UNAVAILABLE | percent of **all logical CPUs** (machine percent), first tick `null`; the definition is not in the payload (`vcpu_count` is); verified within 0.3 points of psutil |
| `cpu.vcpu_count` | MEASURED-TRUTHFUL | equals psutil |
| `memory.rss_bytes`, `peak_rss_bytes` | MEASURED-TRUTHFUL | Windows working set / peak working set |
| `memory.system_total_bytes`, `system_used_bytes`, `system_used_percent` | MEASURED-TRUTHFUL | used = total − available (includes standby cache in "available") |
| `disk.volume_total_bytes`, `volume_free_bytes`, `volume_used_percent` | MEASURED-TRUTHFUL | the volume holding the **data** directory; free = bytes available to the caller |
| `disk.db_bytes`, `wal_bytes`, `sstable_bytes`, `wal.segment_count`, `wal.bytes` | MEASURED-TRUTHFUL (≤ 10 s old, age in `sizes_age_ms`) | `db_bytes` includes the WAL (`db ≥ wal + sstables`) |
| `disk.read_iops`, `write_iops`, `read_mb_per_sec`, `write_mb_per_sec` (and series `disk_*`) | **MISLEADING** | process-level counters under device-sounding names (below) |
| `throughput.*_per_sec` | MEASURED-TRUTHFUL, NULL-WHEN-UNAVAILABLE | deltas over measured time; first tick `null`; `0.0` is a measured zero |
| `throughput.active_connections`, `active_transactions`, `active_queries` | MEASURED-TRUTHFUL | |
| `throughput.active_sessions` | **MISLEADING** | inflated by ghost entries (A25) |
| `latency.query_p50/p95/p99_ms` | MEASURED-TRUTHFUL with caveat | newest 1,000 `POST /v1/sql` handler times incl. error responses; `null` until the first statement |
| `wal.state`, `storage_state`, `compaction.*`, `background.flush_queue_depth`, `index_build_state` | MEASURED-TRUTHFUL (`last_duration_ms` NULL-WHEN-UNAVAILABLE) | |
| `background.pending_groups` | MISLEADING (minor) | it is the number of **waiters** pending durability, not groups |
| `background.last_flush_ms` | NULL-WHEN-UNAVAILABLE (always) | constant `null` |
| `security.*`, `limits.*`, `errors.*` | MEASURED-TRUTHFUL counters since start | value coverage by tests: A12 |

**Disk I/O is process-level — and the names do not say so.** Evidence (C5: 16 writers, ~4,500 commits per run; process counters from `GetProcessIoCounters` vs the data volume's device counters):

| Condition | run | process-level I/O (GetProcessIoCounters): write ops / MB | device-level, data volume E: (PhysicalDrive0): write ops / MB | device-level, temp volume C: (PhysicalDrive1): write ops / MB | E: device writes ÷ process write ops | E: device MB ÷ process MB |
|---|---|---|---|---|---|---|
| C5b_head_sqlwrite16 | 1 | 3579 / 0.193 | 7384 / 35.41 | 116 / 1.15 | 2.06 | 183 |
| C5b_head_sqlwrite16 | 2 | 3561 / 0.192 | 7188 / 33.98 | 114 / 1.2 | 2.02 | 177 |
| C5b_head_sqlwrite16 | 3 | 3585 / 0.194 | 7234 / 34.17 | 82 / 0.71 | 2.02 | 176 |
| C5b_head_sqlwrite16 | 4 | 3558 / 0.192 | 7166 / 33.82 | 70 / 0.72 | 2.01 | 176 |
| C5b_head_sqlwrite16 | 5 | 3629 / 0.196 | 7325 / 34.66 | 180 / 1.72 | 2.02 | 177 |
| C5_head_sqlwrite16_metrics4 | 1 | 4467 / 0.241 | 9043 / 42.59 | 113 / 0.99 | 2.02 | 177 |
| C5_head_sqlwrite16_metrics4 | 2 | 4501 / 0.243 | 9084 / 42.84 | 93 / 1.13 | 2.02 | 176 |
| C5_head_sqlwrite16_metrics4 | 3 | 4550 / 0.246 | 9174 / 43.2 | 77 / 0.68 | 2.02 | 176 |
| C5_head_sqlwrite16_metrics4 | 4 | 4516 / 0.244 | 9109 / 42.95 | 91 / 1.02 | 2.02 | 176 |
| C5_head_sqlwrite16_metrics4 | 5 | 4536 / 0.245 | 9133 / 42.97 | 91 / 0.79 | 2.01 | 175 |

On this fsync-heavy workload the device performed **2.0×** the process's write operations and **~177×** its megabytes (e.g. 4,467 ops / 0.241 MB at the process vs 9,043 writes / 42.59 MB on `PhysicalDrive0`). An operator who has not read the source cannot tell from `disk.write_iops` / `disk.write_mb_per_sec` that these are the server process's own requested writes (one per commit here), nor that they exclude the sectors, metadata and flushes the device really does. The only statements of the scope are a code comment (`metrics_system.rs:101-102`), the CLI line "(this process, not the device)" and the certification. **Matrix A42: FAIL.** No new device-level metric is proposed (item H: OPEN POLICY DECISION; feasibility measured, section 4).

## 11. flush timestamp, event persistence, power loss, standalone API (§11 of the mission)

* **`background.last_flush_ms`** — NOT REQUIRED FOR SINGLE-NODE V1; stays `null` with the contract documented (or the field is removed). Populating the exact time is an engine change (flush completion path in `src/lsm/mod.rs`, certified storage engine; `docs/PROJECT_STATE.md` absent, so ADR first); an observability-only "first seen by the sampler" approximation is possible without one.
* **Operational event persistence** — NOT REQUIRED: bounded in-memory history is the correct v1 scope; the security log (1 MiB × 5, `security_log.rs:46-54`) is the durable record for security events.
* **Power loss** — belongs to durability certification, not observability; not tested here; observability evidence is process-kill only.
* **Standalone `rubixdb-api`** — NOT REQUIRED / OUT OF V1 (D-2, `OPEN_ITEMS.md` 2026-10-04); repository policy has not changed.

## 12. Protected-path audit and ADR drafts

`git diff --stat 8e47379 HEAD -- src/wal src/manifest src/sstable src/compaction src/error.rs Cargo.toml Cargo.lock` → **empty**. The only engine-crate change in the observability commit is `src/lsm/mod.rs` (+8 lines, `compaction_running()`), which is not a protected path. (`api/src/error.rs` is a different file from the protected `src/error.rs`.) This session modified no source or test file (`git status --short` at the end lists documentation files only). **ADR drafts: none were needed** — no required capability was found to need a protected-path change; item I's exact variant would, and is classified NOT REQUIRED, so no ADR was drafted.

## 13. Defects and weaknesses found (documented, NOT applied) — proposals for the next prompt

| ID | Finding (matrix) | Proposed change | Files | Protected? |
|---|---|---|---|---|
| P1 | Ghost session entries after a mid-statement disconnect; inflated `active_sessions`; unbounded `meta` (A25, A58) | Make `take` return a lease that calls `finish(id)` when dropped unless it is put back; make `reap_expired` also remove `meta` entries that are neither in `sessions` nor executing; count the per-principal cap against `meta`; new test (abort mid-statement inside `BEGIN`, assert `total` returns to 0 and `active_sessions == active_transactions`) | `api/src/sql_session.rs`, `api/src/routes/sql.rs:596-645,685-737`, `api/tests/observability.rs` | no |
| P2 | `untracked_active` is cumulative (A32) | rename to `untracked_total` or compute `active − in_flight.len()`; API-level test | `api/src/observability/queries.rs`, `api/src/routes/observability.rs:93` | no |
| P3 | Process-level I/O under device-sounding names (A42) | move to `process_io{…}` (and series `process_io_*`) or add `scope:"process"`; update CLI text; decide device-level separately (item H) | `api/src/routes/metrics_system.rs:93-107`, `api/src/observability/ring.rs:23-32`, `cli/src/ops_cmd.rs:326-331`, tests at `api/tests/observability.rs:318-331` | no |
| P4 | `errors.*`, two `limits.*`, `storage_state` untested (A12) | assert presence/type; add value tests (e.g. force a 5xx / WAL backpressure / SQL limit hit) | `api/tests/observability.rs` | no |
| P5 | Health policy (A01–A03) | per the chosen option of section 5 | section 5.4 | no |
| P6 | Lock probe: `Err(Io)` ⇒ `failed`; probe creates files; start failure swallowed (A03, A17) | tri-state non-creating probe; log + event when `sampler::start` fails | `cli/src/host.rs:285-293`, `instance/src/lock.rs` | no |
| P7 | Two readiness definitions (A05) | unify with `/readyz` or rename `instance.readiness` (e.g. `wal_writer_state`) | `api/src/observability/sampler.rs:380-385`, `metrics_system.rs`, tests | no |
| P8 | `stale:false` with no snapshot (A14) | report `stale:null` (or `true`) when `age_ms` is `null`; test | `api/src/routes/metrics_system.rs:35` | no |
| P9 | `-dirty` freshness hazard (A10) | document, or add re-run triggers / compute at link time | `api/build.rs` | no |
| P10 | Untested rules (A02, A16) | tests: not-ready > 30 s (or its replacement), 5 consecutive panics → `failed` | `api/tests/observability.rs` | no |
| P11 | `/v1/observability/queries` sorts under the registry mutex (cost, C4q) | copy the records under the lock, release, then sort; re-measure with C4q | `api/src/observability/queries.rs:178-192` | no |
| P12 | Soak not run (A72, G) | a real-time sampler soak (hours) with handle/thread/RSS sampling | `scripts` / documentation only | no |
| P13 | Tokio blocking pool grows to 512 under SQL load (item C) | a runtime-resource decision in the SQL/runtime phase | `cli/src/host.rs:180-183` | no |
| P14 | Certification document inconsistencies (A75) | dated closure addendum | `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` (next prompt only) | no |

## 14. Decisions the maintainer must make before the next prompt

1. **Health policy:** Option 1, 2 or 3 (section 5; recommended: **3**).
2. **Ghost sessions (P1):** fix in the next prompt? (recommended: yes — it falsifies a gauge and has no bound.)
3. **Disk I/O labelling (P3) and device-level scope (H):** rename/regroup as process-level (recommended: yes) and decide whether device-level counters are in v1 (recommended: no).
4. **Readiness definition (P7):** unify `instance.readiness` with `/readyz`, or rename it.
5. **`background.last_flush_ms` (I):** keep `null` with the contract documented, remove it, or populate it as "observed by the sampler".
6. **Overhead acceptance threshold (A67):** none exists; set one (measured basis in section 6.6) or accept "reported, no threshold" explicitly.
7. **Soak (G):** duration and acceptance criteria for the real-time sampler soak (REQUIRED FOR V1 by this classification).
8. **Item B:** accept "not reproducible in this session" as closed-by-limitation, or require a long run under the original host conditions.
9. **Lock probe / start-failure behaviour (P6):** approve the tri-state probe and the logging of a failed sampler start.
10. **Blocking-pool growth (C):** assign to a SQL/runtime phase.
11. **`docs/PROJECT_STATE.md` / `missions/ACTIVE.md`:** create, or amend `CLAUDE.md` — both are still absent (OPEN).

## 15. Overall observability reconciliation status

**A mix, not a single word.** Implementation (section 3.A, PASS 48, FAIL 4, OPEN 6, NOT REQUIRED 3, NOT TESTED 12, NOT IMPLEMENTED 2 of 75): the layer's core is implemented and passes its own suites (26/26 + 3/3 + lib, fresh), values cross-check against independent OS readings to the byte or within 0.3 points, the endpoint costs nothing measurable in the engine, and the sampler cadence held at 47–72 k req/s; **but** four rows FAIL (A25 active-session truthfulness, A32 untracked-counter naming, A42 disk I/O labelling, A58 unbounded session map), six are OPEN (health policy, readiness definition, sampler start failure, the unreproduced tail-latency observation, overhead without an acceptance threshold, documentation), and twelve are NOT TESTED. Workspace regression (3.B): FAIL, caused by two repository-hygiene tests and one performance gate that pre-date this work; the observability suites themselves pass. Whole-product readiness (3.C): not ready — FAIL, OPEN and NOT TESTED rows remain. **rubiXDb is not declared PRODUCTION READY by this document.**


## 16. Closure (2026-10-07): the one existing test assertion that changed, and why

* **File / test:** `api/tests/observability.rs`, `health_follows_storage_state_and_disk_free_and_leaves_readiness_alone`.
* **Old expectation (at `2cbd5a7`):** with free space at `total / 10 - 1` (just under 10 %), `instance.healthy` is `"degraded"`; at exactly `total / 10` and above it is `"healthy"`.
* **New expectation:** at `total / 10 - 1` `instance.healthy` is `"healthy"` and the new field `disk.free_advisory` is `"low"`; at exactly `total / 10` it is `"healthy"` and `"ok"`. The `"healthy"` expectations at 10 % and above are unchanged.
* **Reason:** Decision D1 (health policy Option 3) explicitly reverses the 10 % rule: free space is now an advisory field and no longer an input of `instance.healthy`. **The assertion changed because the policy changed, not because the test was flaky, intermittent or inconvenient**, and no other health assertion in that test was weakened (StoragePressure -> degraded, StorageFull -> failed, lock lost -> failed all remain asserted). The old behaviour is not lost: the new expectations assert the replacement behaviour (advisory `low` / `ok`) at the same two boundary values.
* **Other edits to existing tests** are mechanical consequences of Decision D3 (the four `disk.*` I/O names and four `disk_*` series names became `process.*` / `process_*`): the same assertions on the renamed fields, plus a test that the old names are absent.
* **Decision IDs:** D1 (assertion), D3 (renames).


## 17. Closure evidence (2026-10-07)

The closure evidence below is identical to sections 24.1-24.11 of `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md`, which also holds the final matrix (section 24.12). Raw logs: `E:\rubixdb_closure\final\`, `E:\rubixdb_closure\overhead_final\`, `D:\rubixdb_soak\`, `D:\rubixdb_soak_ctl\`.

### 17.1 Identity of what was closed

* Repository: branch `master`, HEAD `2cbd5a7e2b1891898c34c58e2adb8df57a9c3acb` (`observability: sampler, system metrics, time series, diagnostics, status --system`) plus the uncommitted closure changes committed together with this section (the closure commit hash is reported in the final report and in `git log`; it cannot be written into a file inside that same commit).
* Start state: the working tree already held the Prompt 1 documents and the **uncommitted edits of an earlier, stopped closure attempt** (it had stopped at a second P0 defect, section 24.5). That work was resumed, not discarded; every claim below was re-measured with the final build.
* **Final build used for every measurement:** `E:\rubixdb_closure\rubixdb_final2.exe`, SHA-256 `044c45eaecd30fa07f38c7528f15a6a90867962f587a9c7d5fd517363280373e`, built 2026-10-06 23:58 from the working tree. No file under `src/`, `api/src/`, `cli/src/`, `instance/src/` is newer than it; only tests and documents changed afterwards. It reports `git_revision 2cbd5a7e2b18` **without** `-dirty` although the tree was modified (row A10, FAIL). Reference binaries: prior certified `E:\rubixdb_recon\rubixdb_certified_fe511ff.exe`; Prompt 1 HEAD build `rubixdb_head_5c01ef.exe`; pre-observability baseline `rubixdb_base_8e4737.exe` (`8e47379`).
* Protected paths: zero diff (row B14).

### 17.2 Maintainer decisions applied (value actually used)

| ID | Decision | Applied as |
|---|---|---|
| D1 | Health policy | Option 3: `failed` = not ready, or coordinator `poisoned`, or `StorageFull`, or lock `not_held`; `degraded` = `StoragePressure`; otherwise `healthy`. 10 % free-space rule is the advisory `disk.free_advisory` (`low`/`ok`/`unknown`) with `disk.free_advisory_threshold_percent` echoed; 30 s grace removed; lock probe three-valued (`instance.lock_state`). The `coordinator poisoned` term is the closure's own reading of "repository-defined terminal state" after D5 left readiness constant (maintainer to confirm). |
| D2 | Session leak (P0) | Fixed (24.4). |
| D3 | Disk I/O naming | `process.{read,write}_ops_per_sec`, `process.{read,write}_mb_per_sec` (series `process_*`); `disk.*` is volume capacity / free / sizes / advisory only; `device_*` reserved and empty (no such key exists). |
| D4 | Device-level I/O | NOT REQUIRED FOR V1; feasibility evidence recorded (unelevated `IOCTL_DISK_PERFORMANCE`, 4-27 us, no new crate). |
| D5 | Readiness | Maintainer's later instruction, option **R2 + additive extension**: `/readyz.ready` keeps its certified meaning (constant `true`); `instance.readiness` and `/readyz.ready` both call `sampler::ready()`; nothing reads `sync_failures()` for readiness; new additive `instance.coordinator_state`; ADR-OBS-01 proposes extending `/readyz` later (OPEN policy, not applied). |
| D6 | `last_flush_ms` | Kept `null`; contract documented (doc comment, architecture 12.4, OPEN_ITEMS). No engine change. |
| D7 | Overhead threshold | Left blank, so the defaults apply: idle RSS delta <= +2.0 MB, idle CPU <= 0.1 % of one core, thread delta <= +2, load p95 within the baseline run-to-run range. |
| D8 | Soak | 2 h wall-clock minimum: achieved 127.5 min (24.7). |
| D9 | Item B | Closed as NOT REPRODUCED, status OPEN with cause NOT TESTED (environment limitation); Prompt 1's volume reused; not re-investigated. |
| D10 | Item C | Deferred to a SQL/runtime phase; recorded in `OPEN_ITEMS.md` with Prompt 1's measurement (~489-530 tokio blocking threads under 16-client SQL load). Runtime untouched. |
| D11 | State documents | `docs/PROJECT_STATE.md` and `missions/ACTIVE.md` created, minimal; `CLAUDE.md` not amended. |

### 17.3 Code and test changes

`api/src/sql_session.rs`, `api/src/routes/sql.rs` (session lease and cap, D2); `api/src/observability/{sampler,mod,ring,events}.rs`, `api/src/routes/{metrics_system,health}.rs` (D1, D3, D5, D6, D9); `cli/src/host.rs` (three-valued lock probe), `cli/src/ops_cmd.rs` (`status --system` prints `coordinator=`, `lock=`, advisory, `process io`); tests `api/tests/observability.rs` (34 tests), `cli/tests/observability_integration.rs` (6 tests), unit tests in `sql_session.rs` and `sampler.rs`. No file in `src/` (engine), no dependency, no `Cargo.toml` / `Cargo.lock` changed.

### 17.4 P0: session registry leak (D2)

* **Reproduced first** on the Prompt 1 HEAD build with a real server and real sockets (`ghost_repro.py`, 60 sessions each `BEGIN` then a heavy `SELECT` carrying the session id, socket reset mid-statement): 3 s and 23 s after the last abort `sessions_total` 60 (all `executing`, ages 7.7 s -> 27.8 s), `active_sessions` 60, `active_transactions` 0, `COMMIT` on the first 10 non-200 (`final/ghost_before_head.txt`).
* **Fix:** `SessionLease` (`routes/sql.rs`) closes the observation entry and emits a `session.closed` event (existing event shape, `CLIENT_DISCONNECTED`) when the statement future is dropped; the transaction is rolled back by the existing `Transaction` drop path (no second rollback path); the per-principal cap counts `SessionMeta.principal`, so an executing session counts.
* **After, same script, final build:** `sessions_total` 0, `active_sessions` 0, `active_transactions` 0 at both 3 s and 23 s (`final/ghost_after_final.txt`). A second client can open a session at once, and the cap rejects at the cap (`a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind`, `per_principal_cap_counts_a_session_whose_statement_is_running`). Soak: 716 mid-statement aborts, `sessions_total` never above 2.

### 17.5 Second P0 found during closure: readiness flapped under write load (D5)

The earlier closure attempt derived readiness from `GroupCommitStats::sync_failures()` (`sync_attempts - sync_successes`, two independent atomics): it is transiently 1 while an fsync is in flight (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` section 7), so `/readyz` and `instance.readiness` flapped. Measured with 4 writers on real binaries (`final/readyz_flap_before_after.txt`):

| Binary | `/readyz.ready` false | `instance.readiness` not_ready | `instance.healthy` failed |
|---|---|---|---|
| earlier closure attempt (readiness from `sync_failures()`) | 374 of 426 | 76 of 85 | 76 of 85 |
| Prompt 1 HEAD build | 0 of 424 | 46 of 84 | 0 of 84 |
| **final (R2)** | **0 of 424** | **0 of 84** | **0 of 84** |

The work stopped there and the maintainer chose R2 (24.2). The same counter still feeds `errors.wal_sync_failures` (row A81) and the certified `/v1/admin/status` `wal.poisoned` (row C09): both measured wrong under load; neither was changed.

### 17.6 Overhead methodology (D7) and measurements

Methodology: fresh process per run on a copy of the same 20,000-row data, 16 closed-loop clients, no observer poller, 5 runs per workload per binary interleaved (BASELINE, PROMPT 1, FINAL per workload per run; `overhead_final.py`). The FINAL column passes iff idle RSS delta <= +2.0 MB, idle CPU <= 0.1 % of one core, thread delta <= +2 and the load p95 median lies within the BASELINE run-to-run range (reported). Raw runs: `E:\rubixdb_closure\overhead_final\` (45 JSON files, `analysis.txt`).

| Workload / metric | BASELINE `8e47379` (pre-observability) | PROMPT 1 `2cbd5a7` build | FINAL (this mission) |
|---|---|---|---|
| idle RSS MB, median of 5 runs (min-max) | 12.58 (12.53-12.59) | 13.06 (13.05-13.17) | 13.06 (13-13.29) |
| idle threads (median per run; min-max) | 14 (14-14) | 15 (15-15) | 15 (15-15) |
| idle handles | 104 (104-104) | 105 (105-105) | 105 (105-107) |
| idle CPU, harness field (cores, resolution 0.001), 5 runs | 0, 0, 0, 0, 0 | 0, 0, 0, 0, 0 | 0, 0, 0.001, 0, 0 |
| read (16 clients, 15 s): p50 ms, median (min-max) of 5 | 0.562 (0.503-0.719) | 0.546 (0.523-0.567) | 0.558 (0.525-0.581) |
| read (16 clients, 15 s): p95 ms, median (min-max) of 5 | 1.547 (0.926-2.112) | 1.397 (1.1-1.515) | 1.471 (1.082-1.769) |
| read (16 clients, 15 s): p99 ms, median (min-max) of 5 | 2.91 (1.525-4.434) | 2.627 (1.978-2.919) | 2.825 (1.92-3.534) |
| read (16 clients, 15 s): req/s, median (min-max) of 5 | 21820.3 (16363.9-27760.7) | 23057.9 (21731.1-25648.6) | 22255.3 (20133.3-25748.6) |
| read: server CPU cores | 5.586 (5.397-5.795) | 5.546 (5.476-5.682) | 5.752 (5.239-5.874) |
| read: RSS max MB | 30.67 (23.86-33.03) | 29.27 (26.44-30.43) | 30.49 (25.53-33.28) |
| read: threads max | 449 (216-529) | 403 (296-433) | 435 (278-530) |
| read: handles max | 566 (333-646) | 520 (413-550) | 552 (395-647) |
| write (16 clients, 15 s): p50 ms, median (min-max) of 5 | 63.188 (60.294-68.565) | 64.743 (58.539-68.353) | 61.971 (59.968-66.3) |
| write (16 clients, 15 s): p95 ms, median (min-max) of 5 | 74.77 (72.085-76.402) | 74.482 (68.504-75.386) | 73.182 (68.072-73.637) |
| write (16 clients, 15 s): p99 ms, median (min-max) of 5 | 87.795 (76.731-90.925) | 87.075 (83.125-92.947) | 84.91 (81.23-88.309) |
| write (16 clients, 15 s): req/s, median (min-max) of 5 | 247.5 (235.3-259.7) | 244.2 (234.5-268.4) | 251.6 (239.4-262.4) |
| write: server CPU cores | 0.251 (0.187-0.312) | 0.253 (0.231-0.297) | 0.241 (0.198-0.337) |
| write: RSS max MB | 15.94 (15.86-16.02) | 16.65 (16.52-17.08) | 16.53 (16.35-16.58) |
| write: threads max | 33 (33-33) | 34 (34-34) | 34 (34-34) |
| write: handles max | 143 (143-147) | 144 (144-148) | 144 (142-148) |

D7 applied to the FINAL column: idle RSS delta **+0.48 MB** (limit 2.0); idle thread delta **+1** (limit 2); **read p95 median 1.471 ms** inside the baseline range 0.926-2.112; **write p95 median 73.18 ms** inside the baseline range 72.09-76.40; idle CPU: the harness field has 0.001-core resolution (FINAL run 3 read 0.001), so a precise measurement was made from cumulative process CPU time over 3 x 120 s per binary (`precise_idle_cpu.py`): BASELINE 0.0 / 0.0 / 0.0 %, FINAL 0.026 / 0.0 / 0.026 % of one core (limit 0.1 %; Windows CPU time resolves in 15.6 ms ticks, i.e. 0.013 % over 120 s). All five thresholds met; row A67 PASS. (The read-load thread and handle counts vary widely between runs in every column, 216-530 threads: the tokio blocking pool, item C / D10.)

### 17.7 Soak (D8) and retention attribution

* **Run:** real release process `rubixdb_final2.exe` launched as a real process, data directory `D:\rubixdb_soak\root` (D: had 33.4 GB free), log **outside the repository: `D:\rubixdb_soak\soak_log.jsonl`** (126 per-minute records), `soak_summary.json`, `soak_restart_reference.json`, analysis `D:\rubixdb_soak\soak_analysis.json` (copy `E:\rubixdb_closure\final\soak_analysis.txt`). Wall clock **7,652.3 s = 127.5 min** (no simulated clock). Profile: 15 min idle, 45 min read (8 readers at 200 req/s), 45 min write (4 writers), 15 min read + write + 4 metrics-endpoint clients, 5 min cool-down; session churn (7,162 cycles, 716 aborted mid-statement), queries / events / time-series pollers and a 10 Hz `/v1/metrics/system` observer throughout; compactions (4 cycles) and flushes ran.
* **A first soak attempt (2 h 7 min) was INVALID and discarded** (`D:\rubixdb_soak_attempt2_INVALID_port_collision`): the control instance was started first and took port 302, so the soak server fell back to a random port and every soak client hit the wrong instance (all 401s). This was a launch-order mistake in the harness, not a product result. The valid run above started the soak first (it owns port 302) and the control instance second (own port), and was verified from its first record before being left running.
* **Results:** sampler `running` in every record; generations 1 -> 7,561, **0 gaps, 0 regressions**; maximum snapshot age 1,015 ms; health `healthy` throughout; observer / diagnostic / client errors 0, non-200 responses 0 on every workload. Client latency medians: read p50 0.398 ms / p95 0.834 ms / p99 0.99 ms at 1,600 req/s; write p50 13.9 ms / p95 16.6 ms at ~282 req/s; metrics endpoint under the mixed load p50 0.149 ms / p95 0.221 ms / p99 0.263 ms at ~24.9 k req/s. Observer maxima per phase 8.05 / 18.5 / 459 / 895 / 444 ms (idle / read / write / mixed / first cool-down minute; host saturated, row A66). Occupancy inside documented bounds (sessions <= 2, queries <= 200, events <= 200 per ring, series 15 / 240 / 125 points for 15 m / 1 h / 24 h).
* **Observability memory:** the observability-only control instance (same pollers, no SQL) held RSS 12.08 -> 13.41 MB over 130 min, min 12.08, max 14.44; non-decreasing steps 56.6 % (a monotonic-growth rule of >= 90 % is not met); slope 0.65 MB/h overall, 0.46 MB/h in the second half; threads 14-17, handles 131-134, sockets 2, constant. No monotonic RSS growth from observability.
* **Return to baseline (traffic-free tail):** 90 s with no poller, no worker and no request in flight: threads **14 vs baseline 15 (-1, within +/-1)**, sockets 1 vs 2, **handles 121 vs 112 (+9; restart reference on the same data 109): NOT within +/-2 (row A83, FAIL)**. During the 5-minute cool-down (pollers still issuing ~3 statements/s) threads stayed at 31 and handles at 138 (blocking-pool threads are kept alive by that trickle; they decay only once traffic stops).
* **Attribution of the retention (not observability):** the same 3-minute read + write load with no pollers (`attribution.py`, `final/attribution.json`) on the pre-observability baseline `8e47379` and on the final build:

| | idle before | end of load | +60 s | +420 s |
|---|---|---|---|---|
| baseline `8e47379`: threads / handles / RSS MB | 17 / 104 / 12.57 | 90 / 206 / 28.44 | 14 / 119 / 25.28 | 14 / 118 / 25.29 |
| final: threads / handles / RSS MB | 18 / 107 / 13.11 | 104 / 220 / 30.03 | 15 / 120 / 27.83 | 15 / 119 / 27.78 |

  The engine retains about +14 handles and +12.7 MB after this load with no observability code at all; the final build retains +12 handles and +14.7 MB. The main soak instance therefore ended at RSS 57.9 MB (idle start 17.2 MB; 17.6 MB after a restart on the same data): that growth is the write path, not observability, and is recorded in `OPEN_ITEMS.md`; it was not investigated.

### 17.8 API compatibility diff (black box)

Method: `compat.py` starts the certified binary and the final binary on copies of the same data and compares, for `/healthz`, `/readyz`, `/v1/status`, `/v1/metrics`, `/v1/admin/status` (and the unmatched route): status, headers (content-type, cache-control, CSP, nosniff, referrer-policy), no-key / bad-key / POST behaviour, error shape, every JSON path and type, and latency over 200 sequential requests. **Result: 0 differences for all five endpoints and the unmatched route.** `/v1/metrics/system` (added in `2cbd5a7`, so it is also in the certified binary) differs in exactly 12 schema paths, every one justified by a decision:

| Change | Decision |
|---|---|
| ADDED `instance.lock_state` (str) | D1 |
| ADDED `instance.coordinator_state` (str) | D5 (additive extension) |
| ADDED `disk.free_advisory` (str), `disk.free_advisory_threshold_percent` (float) | D1 |
| REMOVED `disk.read_iops`, `disk.write_iops`, `disk.read_mb_per_sec`, `disk.write_mb_per_sec` | D3 (rename) |
| ADDED `process.read_ops_per_sec`, `process.write_ops_per_sec`, `process.read_mb_per_sec`, `process.write_mb_per_sec` | D3 (rename) |

Not purely additive, as the maintainer was told: four existing field names moved by D3. No field was retyped. `instance.readiness` keeps its type (string). Security headers (CSP, nosniff, referrer-policy, cache-control) unchanged on every endpoint; loopback only; no secret in any observability response (`no_api_key_appears_in_any_observability_response`).

### 17.9 Full regression and classification

Commands (logs `E:\rubixdb_closure\final\reg_*.log`, per-suite table `suite_tables.txt`): `cargo fmt --all -- --check` (exit 0), `cargo clippy --workspace --all-targets --all-features -- -D warnings` (exit 0), `cargo check --workspace --all-targets --all-features` (exit 0), `cargo test --workspace --no-fail-fast` (1,346 passed, 2 failed, 28 ignored), `cargo test --release --workspace --no-fail-fast` (1,347 passed, 3 failed, 26 ignored). The observability suites by name: `api/tests/observability.rs` 34/34, `security_events_and_headers` 10/10, `api_integration` 16/16, `admin_ops` 12/12, `api_security_validation` 11/11, `api_http_fuzz` 4/4, `api_cancellation` 1/1, CLI `observability_integration` 6/6, `rubixdb-api --lib` 77 passed 1 ignored (debug and release).

| Failing test | Where | Class | Evidence |
|---|---|---|---|
| `repo_hygiene::no_tracked_credentials_json` | debug, release | PRE-EXISTING | the file is tracked at `8e47379` and HEAD (`git ls-tree`) |
| `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` | debug, release | PRE-EXISTING | same file |
| `m1_3_thousand_writers_throughput` | release | PRE-EXISTING, INTERMITTENT | workspace 43,228 ops/s; isolated 111,824 / 60,610 / 92,078 on this tree; clean `8e47379` worktree 55,256 / 80,562 / 110,880; `src/` has zero diff vs HEAD |
| `sampler_start_stop_100_times_leaves_no_thread_behind` (first release run only) | release | INTRODUCED BY THIS MISSION'S TEST, FIXED | see B13: 11 of 12 repeated runs failed while 10 of 10 isolated runs passed; skipping this mission's in-process write-load test: 6 of 6 pass; fixed by moving that check into the real-process CLI suite (no existing test edited); then 10 of 10 pass |

### 17.10 Changed test assertion (Decision D1) - stated explicitly

* **File and test:** `api/tests/observability.rs`, `health_follows_storage_state_and_disk_free_and_leaves_readiness_alone`.
* **Old expectation (at `2cbd5a7`):** with free space at `total / 10 - 1` (just under 10 %), `instance.healthy` is `degraded`; at exactly `total / 10` and above it is `healthy`.
* **New expectation:** at `total / 10 - 1`, `instance.healthy` is `healthy` and `disk.free_advisory` is `low`; at exactly `total / 10`, `healthy` and `ok`; the `healthy` expectations at 10 % and above are unchanged.
* **Reason:** **Decision D1 explicitly reverses the policy: the 10 % rule is now advisory and no longer an input of `instance.healthy`. The assertion changed because the policy changed, not because the test was flaky, intermittent or inconvenient.** No other health assertion in that test was weakened (StoragePressure -> degraded, StorageFull -> failed, lock lost -> failed all stay asserted). The other edits to existing tests are the mechanical D3 renames (`disk_*` -> `process_*`) with an added test that the old names are absent.

### 17.11 Findings recorded, not fixed (outside the approved items)

* A10 (FAIL): `-dirty` suffix not applied to a binary built from a modified tree.
* A32 (FAIL): `untracked_active` is cumulative (Prompt 1 P2).
* A81 (FAIL): `errors.wal_sync_failures` shows a phantom 1 under write load; C09 (FAIL): the certified `/v1/admin/status` `wal.poisoned` is true under write load with no failure.
* A83 (FAIL): handle count after the soak, attributed to the engine (24.7).
* A80 (NOT IMPLEMENTED): an fsync-poisoned committer is not visible without an engine accessor (ADR-OBS-01).
* Not tested: A09, A12, A14, A16, A19, A64, A70, A71, A82.
