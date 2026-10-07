# rubiXDb — Full Observability CERTIFICATION

**Scope: OBSERVABILITY only.** This document certifies the observability layer added in this mission. It does **not** certify WAL performance, product readiness, or rubiXDb production readiness; those remain independent and are not claimed here.

Mission: "Full Observability, one shot" (single session, no approval gates). Date: 2026-10-06.

## 0. Identity of what was certified

| Item | Value |
|---|---|
| Branch / base commit | `master` / `8e473791551cfb595876e2615f1bc5cf30774a62` |
| Source state | **Uncommitted working tree** (15 modified, 6 new paths; see section 22 and the report). Nothing committed, nothing pushed. |
| Binary measured (overhead, cross-validation, kill/restart) | release `rubixdb.exe`, 18,276,352 bytes, SHA-256 `fe511ff5c5afb40ce2c89e2de6ae93cb8790d7397e5e086850653438abd38d7f`, built by `cargo test --release --workspace` from this tree |
| Baseline binary (measured **before** any change) | copy of the HEAD-state release binary kept as `rubixdb_base.exe` (17,994,240 bytes) |
| Host | Windows 10 22H2 (19045), 4 physical / 8 logical cores, 15.9 GiB RAM, NTFS; the repository volume `C:` had 7.2 GiB free of 82 GiB (91.2 % used) during this work |
| Pre-flight | `git status` showed only this mission's own earlier edits; `CLAUDE.md` read; `docs/PROJECT_STATE.md` and `missions/ACTIVE.md` **do not exist in this repository** (recorded OPEN, see 18) |
| Protected paths | `git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/ src/error.rs Cargo.toml Cargo.lock` → **empty**. No dependency added (`Cargo.toml`/`Cargo.lock` untouched). |
| Engine-crate change | one: `src/lsm/mod.rs` gained a read-only accessor `LsmEngine::compaction_running()` (one relaxed atomic load of an existing flag; not a protected path; consulted by no engine decision) |

## 1. Product model summary

rubiXDb is a local single-node relational database. In v1 the server runs **in-process in `rubixdb.exe`** (`rubixdb gui` / `rubixdb cli` become the instance owner; `cli/src/host.rs`); the standalone `rubixdb-api` binary is out of v1 but was wired the same way (`api/src/main.rs`). The listener is `127.0.0.1` only, default port 302, authenticated by a bearer key; `Reader` may read, `Admin` may act.

The four existing questions keep their meanings (unchanged):

| Endpoint | Meaning | Changed? |
|---|---|---|
| `/healthz` | process is alive | no |
| `/readyz` | normal supported work can be accepted (constant `true` while the engine accepts work; **stays true during safe startup index recovery**) | no (field sets identical, section 16) |
| `/v1/status`, `/v1/admin/status` | current operational state | no (field sets identical) |
| `/v1/metrics` | quantitative route/engine measurements | no shape change; the route-label set is now closed (section 10) |

The new layer adds three kept-apart kinds of information: **metrics** (`/v1/metrics/system`, `/v1/metrics/system/timeseries`), **diagnostic state** (`/v1/observability/sessions`, `/queries`), **events** (`/v1/observability/events`, plus `version`).

## 2. Baseline metrics (Step 2, before any change)

Harness: black-box Python/`psutil`, real release binary, 16 concurrent SQL clients for 15 s, 20,000-row table, each run a fresh process on a copy of the same data, **5 runs** per workload; raw: `obs4_baseline.json` (kept outside the repository). Read = `SELECT … WHERE id = ?`; write = single-row `INSERT`.

| Measurement | p50 of 5 runs | p95 of 5 runs (= max) |
|---|---|---|
| Idle RSS (MB) | 14.06 | 14.16 |
| Idle private bytes (MB) | 6.63 | 7.18 |
| Idle threads / handles | 17 / 109 | 17 / 109 |
| Idle CPU (10 s window) | 0.00 % of one core | 0.00 % |
| RSS peak under 16 clients, read (MB) | 32.86 | 33.10 |
| Read query p50 / p95 / p99 (ms) | 0.944 / 1.819 / 4.946 | 0.949 / 1.908 / 5.629 |
| Read throughput (queries/s) | 14,083 | 14,457 |
| Server CPU, read (cores) | 2.82 | 2.96 |
| RSS peak under 16 clients, write (MB) | 17.27 | 17.44 |
| Write query p95 (ms) / throughput (q/s) | 53.58 / 345.4 | 54.58 / 349.2 |
| Server CPU, write (cores) | 0.249 | 0.299 |

Baseline findings recorded before implementation (Step 1/2 probes, `obs1.json`):

* The existing `/v1/metrics` route table was **unbounded**: 3,200 hostile requests (random paths, custom HTTP verbs, random key segments) grew the route-key set from 17 to **821** (804 new keys, e.g. `07G4AOKH /v1/kv/:key_b64`, because the HTTP method token was copied verbatim into the key). This was a measured baseline **FAIL** of the closed-key-set rule; it is fixed in this mission (section 10).
* Authentication failures, rate-limit rejections, admin actions and refused sessions were not counted anywhere queryable (only the rotating security log).
* No time series, no per-query records, no session list, no version endpoint, no system-memory / disk-capacity / I/O / CPU-percent measurement, no connection gauge.
* Every existing endpoint's field set was inventoried (18 endpoints) so compatibility could be diffed after the change (section 16).

## 3. Endpoints added (all additive, all `GET`, all behind the existing bearer authentication, minimum role **Reader**)

| Path | Fields | Units / nullability | Bounds |
|---|---|---|---|
| `/v1/metrics/system` | `timestamp_unix_ms`; `sample_freshness{last_sample_ms, age_ms, state, stale, tick_ms}`; `sample_generation`; `instance{id,name,uptime_seconds,healthy,readiness}`; `cpu{process_percent,peak_percent,vcpu_count}`; `memory{rss_bytes,peak_rss_bytes,system_total_bytes,system_used_bytes,system_used_percent}`; `disk{volume_total_bytes,volume_free_bytes,volume_used_percent,db_bytes,wal_bytes,sstable_bytes,sizes_age_ms,read_iops,write_iops,read_mb_per_sec,write_mb_per_sec}`; `throughput{http_requests_per_sec,sql_queries_per_sec,write_commits_per_sec,active_connections,active_sessions,active_transactions,active_queries}`; `latency{query_p50_ms,query_p95_ms,query_p99_ms}`; `wal{segment_count,bytes,state}`; `compaction{running,cycles_since_start,live_sstable_count,last_duration_ms}`; `background{flush_queue_depth,pending_groups,index_build_state,last_flush_ms}`; `security{auth_failures_since_start,forbidden_since_start,admin_actions_since_start,last_admin_action}`; `limits{rate_limited_since_start,sessions_rejected_since_start,wal_backpressure_rejections,sql_resource_limit_hits}`; `errors{http_server_errors_since_start,sql_errors_since_start,wal_write_errors,wal_sync_failures}`; `storage_state`; `latency_of_response_ms` | bytes, ms, seconds, fractions as stated; **any value the platform cannot measure is JSON `null`, never 0**; every string is from a closed set | one immutable snapshot; response ≈ 2 KB |
| `/v1/metrics/system/timeseries?window=15m\|1h\|24h\|7d` | `window`, `resolution_seconds` (60/15/60/3600), `series{cpu_process_percent, memory_rss_bytes, disk_read_iops, disk_write_iops, disk_read_mb_per_sec, disk_write_mb_per_sec, sql_queries_per_sec, active_queries}` each a list of `{t: unix_ms, v: number or null}`, oldest first; a series never measurable on this platform/instance is `null`; empty history is `200` with empty arrays | invalid or missing `window` → `400 VALIDATION_ERROR` (existing error shape); never `404` | body capped at 2 MiB (oldest dropped); measured 24 h body < 2 MiB (test) |
| `/v1/observability/sessions?limit=N` | `session_id, state (idle\|executing), age_seconds, transaction_state, operation_class, idle_seconds, idle_timeout_remaining_seconds, lifetime_remaining_seconds, timeout_state, cancellation_state` + `returned,total,truncated,max_records` | ordered by session id | ≤ 200 records |
| `/v1/observability/queries?limit=N` | `query_id, statement_class, state (running\|succeeded\|failed\|cancelled\|timed_out), start_unix_ms, duration_ms, rows_affected, rows_returned, timeout_state, cancellation_state, error_class` + `returned,active,untracked_active,max_records` | newest first; **never SQL text**, parameters, names | ≤ 200 records; ≤ 2,048 in flight tracked |
| `/v1/observability/events?limit=N` | `limit, max_limit, security[], operational[]`; each event `timestamp, event_type, severity, operation_class, result, duration_ms, request_id, session_id, error_class` | newest first; `limit` clamped to 1..200; non-integer → 400 | two separate rings of 256 events each |
| `/v1/observability/version` | `product_version, build_identifier, git_revision (null if unknown; `-dirty` suffix for an uncommitted tree), startup_timestamp_unix_ms` | never a path, user or host name | constant-size |

CLI: `rubixdb status --system [--json]` prints the same snapshot (null → `-`); `rubixdb status` is unchanged.

## 4. Counters added

`auth_failures`, `auth_forbidden`, `rate_limited`, `admin_actions` (+ last admin action: time, route **pattern**, outcome), `sessions_rejected`, `http_server_errors`, per-request ids, `active_connections` gauge (`serve_observed`), `active_queries` gauge (+ `untracked` overflow count), sampler `ticks` / `panics`, session creation time retained across statements (so session age is true age), route-key overflow bucket. All are atomics or short registry locks; none is keyed by input.

## 5. Counters reused (nothing duplicated)

`ReadStats`, `CompactionMetrics` (cycles, last cycle duration), `GroupCommitStats` / `BatchCoordinatorStats` (completed, rejected-backpressure, sync failures, pending waiters, queue), `SqlApiMetrics` (requests, errors, cancellations, timeouts), `ExecMetrics` / `PlannerMetrics` (aggregate limit hits), `TxnMetrics` (active transactions), the existing per-route latency reservoir (`POST /v1/sql` percentiles), the SQL session registry, `LsmEngine::storage_state()`, `pool_stats()`, the existing `disk_usage()` directory walk, the existing rotating JSON-lines security log (structured logging). The one engine addition is the read-only `compaction_running()` accessor.

## 6. Sampler design

* **One** named thread `rubixdb-sampler`, period 1 s (`TICK`), started by the host/`main` after the router exists, stopped (joined) **before** the engine is shut down. `SamplerHandle::stop()` is idempotent and also runs on `Drop`; a second concurrent start is refused (`AlreadyRunning`).
* First measurement is taken **synchronously at start**, so a snapshot exists before the server reports ready.
* One tick = read OS values through the `OsProbe` trait (real: `GetProcessTimes`, `K32GetProcessMemoryInfo`, `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW`, `GetProcessIoCounters`, `available_parallelism`; Linux `/proc` variants exist but are **untested**), read existing atomics / short registry locks, build one immutable `Snapshot`, swap an `Arc<Snapshot>` under an `RwLock` (readers clone the `Arc`; no reader ever waits on a tick), then feed the time-series rings.
* **Never** takes an engine write path lock, runs SQL, or changes storage state. The directory-size walk runs every 10 s (age exposed as `disk.sizes_age_ms`), not every tick.
* Rates (`*_per_sec`) are deltas of cumulative counters over the real elapsed time; the first tick has no delta and reports `null`. CPU % = Δcpu-seconds ÷ Δwall ÷ vCPU × 100, i.e. **percent of the whole machine** (100 = every logical CPU saturated); measured equal to `psutil`'s per-core figure ÷ 8 (section 13).
* Each tick runs under `catch_unwind`; a panic skips the tick (counted), five consecutive panics mark the sampler `failed`; any later good tick returns it to `running`.
* State exposed: `running | degraded | failed | not_started`. `degraded` = at least one probe returned nothing this tick. **Staleness is derived from the snapshot's age on every request** (stale > 5 s; `failed` when a "running" sampler has not published for > 10 s), so a wedged sampler is visible as such and an old value is never presented as current.
* Memory: see 7 and 11. Default cap across all sampler state: **1 MiB**; measured series store **240,856 bytes** (23 %).

## 7. Ring buffer caps

Per series: 15 m @ 60 s = **15**, 1 h @ 15 s = **240**, 24 h @ 60 s = **1,440**, 7 d @ 3,600 s = **168** → **1,863 samples** × 16 bytes = 29,808 bytes per series. 8 series → **238,464 bytes** of slots; with bookkeeping the store is **240,856 bytes measured** after 25 h of injected 1 Hz ticks (hard cap 1,048,576, asserted at construction and in test). Slots are allocated once (`Box<[Sample]>`); a push overwrites the oldest slot and never allocates. Each stored value is the **mean of the present samples in that period** (a missing measurement is NaN → `null`, never 0).

Other bounded structures: event rings 2 × 256; query registry 200 recent + 2,048 in flight; route-key table 128 + 1 overflow bucket with a closed method set; session list ≤ 200 returned; timeseries body ≤ 2 MiB.

## 8. Health classification policy — **DECISION REQUIRING REVIEW**

No health policy existed in the repository. As the mission directs, `instance.healthy` is computed **once per tick** (stable between ticks):

| Class | Rule |
|---|---|
| `failed` | readiness has been continuously false for > 30 s, **or** `storage_state == StorageFull`, **or** the instance lock is no longer held (probe installed by the embedded host) |
| `degraded` | `storage_state == StoragePressure`, **or** volume free space < 10 % of volume total (strict `<`; exactly 10 % is not degraded) |
| `healthy` | otherwise |

The thresholds (30 s, 10 %) are the mission's choice, not derived from repository policy → **reviewer must confirm**; they live in one place (`NOT_READY_FAILS_AFTER`, `Observability::disk_low_percent`) and the field is recomputed if they change. Real observation on this host: the temp/data volume `C:` was 91 % used (7.2 GiB free of 82 GiB, 8.7 % free) during this work, so a real instance there reports `degraded` by the 10 % rule — the rule fired on real data, not only in the test.

## 9. Full test sweep results

New tests (all drive the **real** engine and the **real** axum server on a real `127.0.0.1` socket unless stated):

| # | Mission test | Test(s) |
|---|---|---|
| 1 | all fields, correct types | `metrics_system_has_every_required_field_with_the_right_types` |
| 2 | null, not zero | `a_platform_that_measures_nothing_yields_nulls_never_zeros` |
| 3 | p95 < 50 ms at 16 readers, 10 s | `metrics_system_p95_is_below_50ms_with_16_concurrent_readers` — **0.771 ms** (release test) / 2.646 ms (debug), n = 301,671 / 86,689 |
| 4 | timeseries windows | `timeseries_windows_resolution_empty_history_and_invalid_window`, `timeseries_returns_injected_history_oldest_first_with_exact_resolution_and_a_size_cap` |
| 5–7 | sessions, queries, events | `sessions_endpoint_…`, `queries_endpoint_…never_sql_text`, `events_endpoint_is_bounded_clamped_…` |
| 8 | version | `version_reports_product_version_build_identity_and_startup_time` |
| 9 | no credential in any response | `no_api_key_appears_in_any_observability_response` + CLI `status_system_prints_the_live_snapshot_and_never_a_credential` |
| 10 | cardinality | `adversarial_inputs_never_grow_the_metric_key_set` (+ black-box probe, section 10) |
| 11–12 | ring 2× cap, 24 h simulated | unit tests in `observability/ring.rs`; 25 h injected in the integration test |
| 13 | 100 start/stop cycles | `sampler_start_stop_100_times_leaves_no_thread_behind` |
| 14 | torn reads | `back_to_back_reads_while_the_sampler_ticks_never_see_a_torn_snapshot` (300 reads, 5 ms ticks) |
| 15–17 | CPU / RSS / disk failure | `cpu_rss_and_disk_read_failures_null_the_field_degrade_the_sampler_and_never_stop_it` |
| 18 | multi-instance | `two_instances_report_only_their_own_data_…` (in-process) and CLI `two_real_instances_…` (two real processes) |
| 19 | kill / restart | CLI `killing_the_owner_and_restarting_it_starts_observability_state_from_empty` |
| 20–21 | regression | section 9.2 |

Additional tests (health, state, counters): `health_follows_storage_state_and_disk_free_and_leaves_readiness_alone`, `readiness_stays_true_while_safe_index_recovery_runs`, `a_dead_sampler_is_visible_as_stale_…`, `a_wedged_sampler_goes_stale_then_failed_and_recovers_when_unblocked`, `a_panic_in_a_tick_is_contained_and_the_sampler_recovers`, `a_statement_past_its_deadline_is_recorded_as_timed_out_with_an_event`, `running_and_cancelled_queries_are_visible_while_real_expensive_queries_run`, `rates_and_gauges_follow_real_load_and_fall_back_when_it_stops`, `latency_percentiles_are_consistent_with_client_measured_times`, `session_cap_refusals_and_admin_actions_are_counted_and_visible`, `rate_limit_rejections_are_counted_with_events`; plus unit tests in `ring.rs`, `events.rs`, `queries.rs`, `metrics.rs`, `sql_session.rs`, `version.rs`.

### 9.1 Observability suites

`api/tests/observability.rs`: **26 passed, 0 failed** (debug and release); `cli/tests/observability_integration.rs`: **3 passed, 0 failed** (debug and release); the API suite was run 12 more times consecutively in debug after the last fix: 12/12 green.

One test flaked once in 6 runs (`cpu_rss_and_disk_read_failures…`): the test read the sampler generation **before** clearing the injected fault, so ticks that ran during the fault could count as recovery ticks. Fixed in the test (generation read after the fault is cleared); 25 isolated runs before and 12 full runs after: 0 failures. Cause is reasoned from the code, not reproduced after the fix.

### 9.2 Final regression (Step 6)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS (exit 0) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS (exit 0) |
| `cargo check --workspace --all-targets --all-features` | PASS (exit 0) |
| `cargo test --workspace --no-fail-fast` (debug) | **1,331 passed, 2 failed**, 28 ignored (exit 101 because of the 2 listed below) |
| `cargo test --release --workspace --no-fail-fast` | **1,332 passed, 3 failed**, 26 ignored (exit 101 because of the 3 listed below) |

Failing tests, each **verified against clean HEAD `8e47379`** and unrelated to this mission:

| Test | Evidence it pre-exists |
|---|---|
| `repo_hygiene::no_tracked_credentials_json`, `…no_tracked_file_contains_a_64_hex_admin_key_literal` | the file `frontend/.e2e-crossbrowser-data/default/credentials.json` is tracked in HEAD (`git ls-tree -r HEAD`, committed in `a40fcf8`) and unmodified; the same two failures appear in every earlier phase's regression |
| `m1_3_thousand_writers_throughput` (release only, WAL throughput gate ≥ 80,000 ops/s) | run in a clean `git worktree` of HEAD with its own target directory: **69,557 ops/s** (miss); this tree: 68,133 and 49,300 ops/s in two runs (single-run noise is large; machine was also running other work). WAL performance is a separate, known-open item |

Focused suites (all part of the runs above): API integration (`api_integration`, `api_sql_integration`, `admin_ops`, `api_security_validation`, `security_events_and_headers`, `api_http_fuzz`, `api_cancellation`, …), CLI (`cli_integration`, `gui_instance_integration`, `lifecycle_status_and_attach_integration`, `startup_*`), crash/restart (`crash_recovery_integration`, `index_backfill_crash_integration`), multi-instance (`multi_instance_sustained_load`, `instance_drop_integration`): all green except as listed.

## 10. Cardinality safety results

* **Construction:** every route label is `"<closed method set> <matched route pattern or UNMATCHED>"` (`route_label`, methods outside GET/POST/PUT/DELETE/HEAD/OPTIONS/PATCH collapse to `OTHER`); the table holds at most 128 keys plus one overflow bucket; query/event labels are `&'static str` constants; session and query records hold no user text.
* **Adversarial integration test** (real server): after warm-up the metric key set (all JSON key paths of `/v1/metrics`, `/v1/metrics/system`, `/v1/status`, `/v1/compaction/metrics`, `/v1/admin/status`) and the route-label set were frozen, then 1,000 random-text SQL statements, 500 + 500 random table names (SQL and REST), 1,000 random bearer keys (including random `name:key` strings), and 1,000 requests with random HTTP method tokens and paths were sent. Key set **unchanged after every phase**; route labels unchanged, except that random methods may add at most the closed `OTHER`/`UNMATCHED` buckets; the 128-key cap held.
* **Black-box re-run of the baseline probe on the final binary** (same 3,200 hostile requests that produced 821 keys before): routes 17 → **23** (6 new, all closed-set: `GET /v1/catalog/tables/:name`, `GET /v1/kv/:key_b64`, `GET /v1/snapshots/:id`, `OTHER /v1/kv/:key_b64`, `OTHER /v1/sql`, `PATCH /v1/metrics`). Baseline FAIL (821) → **bounded (23)**.
* No SQL text, table/column/schema name, principal name, path or error string appears in any key; the leak scan on the final binary found no credential in any output, no SQL/object names in metrics/status, no credential in the command line, stdout/stderr, files or the security log.

## 11. Memory safety results

* Ring buffers: pushing 2× capacity (+1) leaves `memory_bytes` identical and drops the oldest (unit test, all four rings); 25 hours of injected 1 Hz ticks → **240,856 bytes** (cap 1,048,576); resolution of every window verified exact (60 / 15 / 60 s steps); timeseries body for 24 h < 2 MiB.
* Events: two rings of 256; a flood of 600 security events left 200 security events and **kept** the earlier operational event (separate rings). Queries: 260 extra statements left ≤ 200 records. Sessions ≤ 200 returned.
* RSS cost of the whole layer measured end to end: **+0.48 MB idle** (14.06 → 14.53 MB, section 16); under load RSS is within the baseline run-to-run range.
* Not measured: exact byte size of the event and query registries (bounded by entry count; the idle RSS delta above is the combined empirical bound).

## 12. Snapshot consistency results

300 back-to-back `GET /v1/metrics/system` calls while the sampler ticked every 5 ms against a probe whose RSS, system-used and disk-free values are all derived from one counter: every response had `rss == g·1000`, `peak == g·1000+1`, `system_used == g·1000`, `disk_free == 40,000,000 − g` for its own `sample_generation g`; **0 torn reads**, > 3 distinct generations seen. `instance.healthy` is computed once per tick and equal across reads of one generation (tested).

## 13. Failure injection results

| Injected fault | Result |
|---|---|
| every platform call fails | all OS-derived fields `null` (15 fields asserted), product-side fields still real, instance **not** `failed`, time series `null` |
| CPU / RSS / disk call fails (one at a time) | exactly that field becomes `null`, other probes unaffected, sampler `degraded`, endpoint answers 200, SQL and `/healthz` still work, sampler returns to `running` when the fault clears |
| panic inside a tick | contained (`catch_unwind`), tick skipped and counted, server and `/healthz` unaffected, sampler ticks again and returns to `running` |
| sampler wedged inside a platform call | `stale: true` after 5 s (age ≥ 5,000 ms), `state: failed` after 10 s (age ≥ 10,000 ms), last good snapshot returned with its true age, SQL still works, recovers to `running` when released |
| sampler thread stopped | state `not_started`, age grows (≥ 400 ms in 400 ms), never presented as current |
| storage `StoragePressure` / `StorageFull` / free disk < 10 % / instance lock lost | `degraded` / `failed` / `degraded` / `failed`, and back to `healthy`; `/readyz` and `/healthz` unchanged |

**Cross-validation against independent OS readings** (`obs6.py`, final binary, one snapshot vs `psutil`/directory walk): RSS API 11,505,664 vs psutil 11,743,232 B (**2.0 %** apart, one tick of skew); peak RSS equal (18,722,816 B); system memory total equal (17,060,876,288 B), used % 41.93 vs 41.9; vCPU 8 = 8; volume total and free **equal to the byte**; process write IOPS 335 (API) vs 330 (psutil), 0.150 vs 0.150 MB/s; process CPU under 8 writers: API mean ≈ 3.0 % of the machine vs psutil 23.98 % of one core ÷ 8 = 3.0 %; after load `db_bytes`/`wal_bytes`/`sstable_bytes` **equal to the directory walk** (3,756,303 / 3,756,243 / 0); under a 44 MB and a 140 MB write load (`obs7*.py`): compaction cycles 3 = 3 and 11 = 11 versus the existing `/v1/compaction/metrics`, live SSTables 2 = 2 and 3 = 3, `last_duration_ms` identical (434.3931 / 1349.2239), `sstable_bytes` 46,497,945 and 152,173,663 equal to the walk, WAL segments 2 = 2 files; `compaction.running = true` and `flush_queue_depth = 1` were observed during the larger run (not during the smaller one: its 0.43 s cycles fell between 1 Hz ticks).

## 14. Multi-instance isolation results

In-process: two servers with separate engines/data directories: distinct instance ids and names; instance B's single failed login appears only in B (`auth_failures_since_start` 0 vs 1); query lists are separate (61+ vs 7 records); A's security event ring is empty; **stopping A leaves B serving and sampling** (generation advances, SQL succeeds). Real processes (`rubixdb gui` owners `alpha` and `beta`): different instance ids, each reports its own name; killing `alpha` leaves `beta`'s generation advancing. No state is shared (no globals; the sampler, registries and counters hang off each `AppState`).

## 15. Crash / restart results

Real binary: owner started, five statements run, `status --system --json` read; owner **killed** (process kill — not a graceful shutdown, not a power loss); exit status non-zero, the PID is gone from `tasklist` (so the sampler thread cannot survive), **the listener port refuses connections** (no orphan), the instance restarts cleanly with the **same instance id**, a **fresh** process (`uptime_seconds` < 30, counters from 0, history empty — observability state is deliberately not persisted), the sampler publishes new generations after the restart, SQL works. Power loss: **NOT TESTED**.

## 16. Overhead vs baseline (Step 5, same harness, 5 runs, final binary)

| Measurement (5 runs each) | Baseline p50 | Baseline p95 | After p50 | After p95 | Delta of p50 |
|---|---|---|---|---|---|
| Idle RSS (MB) | 14.06 | 14.16 | 14.53 | 14.62 | +0.48 (+3.4%) |
| Idle private bytes (MB) | 6.63 | 7.18 | 7.19 | 7.23 | +0.56 (+8.5%) |
| Idle threads | 17 | 17 | 18 | 18 | +1 (+5.9%) |
| Idle handles | 109 | 109 | 110 | 112 | +1 (+0.9%) |
| Idle CPU (% of one core, 10 s window) | 0.00 | 0.00 | 0.00 | 0.00 | +0.00 |
| RSS peak under 16 clients, read (MB) | 32.86 | 33.10 | 26.10 | 33.27 | -6.77 (-20.6%) |
| Query p50, read (ms) | 0.944 | 0.949 | 0.914 | 0.919 | -0.030 (-3.2%) |
| Query p95, read (ms) | 1.819 | 1.908 | 1.692 | 1.750 | -0.128 (-7.0%) |
| Query p99, read (ms) | 4.946 | 5.629 | 4.213 | 4.958 | -0.733 (-14.8%) |
| Throughput, read (queries/s) | 14083 | 14457 | 14918 | 14987 | +835 (+5.9%) |
| Server CPU, read (cores) | 2.82 | 2.96 | 3.08 | 3.15 | +0.27 (+9.5%) |
| Threads peak, read | 529 | 529 | 274 | 530 | -255 (-48.2%) |
| Handles peak, read | 647 | 647 | 392 | 648 | -255 (-39.4%) |
| RSS peak under 16 clients, write (MB) | 17.27 | 17.44 | 17.74 | 17.95 | +0.46 (+2.7%) |
| Query p95, write (ms) | 53.58 | 54.58 | 52.69 | 53.00 | -0.90 (-1.7%) |
| Throughput, write (queries/s) | 345.4 | 349.2 | 350.7 | 351.0 | +5.3 (+1.5%) |
| Server CPU, write (cores) | 0.249 | 0.299 | 0.242 | 0.247 | -0.007 (-2.9%) |
| Threads peak, write | 33 | 40 | 34 | 34 | +1 (+3.0%) |
| Handles peak, write | 152 | 159 | 153 | 153 | +1 (+0.7%) |


Additional measurements (`obs5.py`, 3 runs of 60 s each, idle): process CPU **0.0 s in 60 s for both binaries** (resolution of the OS counter 15.6 ms, i.e. < 0.026 % of one core); threads 16 → 17 (+1 = the sampler) on the post-start steady state. `/v1/metrics/system` against the real release binary with **16 concurrent readers for 15 s** (3 runs, ≈ 326,000–330,000 requests each, 0 errors): p50 **0.58 ms**, p95 **1.01–1.04 ms**, p99 **3.8–3.9 ms**, ≈ 21,800 requests/s, and max 387–504 ms in the final run (an earlier identical run had a 1.17–1.35 s max; the cause of the occasional long single request is **not investigated**, see 18). Cost of the existing `/v1/admin/status` (it walks the data directory per call) is unchanged in kind (p50 31.9 → 23.8 ms).

Reading of the table: idle cost of the whole layer is **+0.48 MB RSS (+3.4 %), +1 thread, +1 handle, ~0 CPU**. Under 16-client load, query p95, throughput, CPU and RSS differ from baseline by less than the baseline's own run-to-run range, except that peak thread/handle counts under read load are widely scattered between runs (after: 70–530 threads; baseline: 515–529) because they are sampled every 0.5 s against a transient thread pool; **no improvement and no regression is claimed from those rows**.

## 17. Null fields on this platform (Windows 10)

| Field | When `null` | Why |
|---|---|---|
| `cpu.process_percent`, rates (`*_per_sec`, `read_iops`, `write_iops`, `read_mb_per_sec`, `write_mb_per_sec`) | first sample after start | rates need two samples; no value is invented |
| `cpu.*`, `memory.*`, `disk.volume_*`, `disk.*_iops`, `disk.*_mb_per_sec`, `cpu.vcpu_count` | whenever the OS call fails | tested; per-field |
| `background.last_flush_ms` | **always** | the engine keeps no flush-completion timestamp; adding one would be an engine change |
| `latency.query_p*_ms` | until the first SQL statement | no samples |
| `compaction.last_duration_ms` | until the first compaction cycle | none has run |
| `security.last_admin_action` | until the first admin action | none |
| `git_revision` | when the build had no git | never invented |
| timeseries series | never measurable on this instance | `null`, not an empty list |

`disk.read_iops`/`write_iops`/`*_mb_per_sec` are **process-level** (this process's own I/O from `GetProcessIoCounters`), **not device-level** disk activity; device-level counters were not implemented (PDH / `IOCTL_DISK_PERFORMANCE` need handles or privileges the mission did not authorize).

## 18. Open items

* `docs/PROJECT_STATE.md` and `missions/ACTIVE.md` do not exist in this repository (pre-flight said "record OPEN if absent"); `CLAUDE.md` references both.
* The two `repo_hygiene` failures: a credentials file is tracked in git (`frontend/.e2e-crossbrowser-data/default/credentials.json`) — a pre-existing security item outside this mission.
* `m1_3_thousand_writers_throughput` fails the ≥ 80,000 ops/s gate at clean HEAD (69,557) — WAL performance, outside this mission.
* Occasional long single-request latency (hundreds of ms to 1.35 s max) for `/v1/metrics/system` under 16 readers over 15 s; p99 is 3.9 ms; cause not investigated.
* Peak thread/handle counts under read load vary widely between runs (70–530) in the measurement harness; not investigated.
* The temp/data volume on this host is 91 % full, so `healthy` is not observable on this machine without a fake disk probe (the integration test uses one).
* Health thresholds (30 s, 10 %) are a **decision requiring review** (section 8).
* The unauthenticated-vs-reader question: all new endpoints require a Reader key. Whether `/v1/metrics/system` should be reachable by lower-privilege or unauthenticated callers is a product decision, not made here.

## 19. Not tested

Power loss (kill only was tested); non-Windows platforms (Linux `/proc` / `statvfs` paths compiled only); the standalone `rubixdb-api` binary end to end; real 24 h / 7 d wall-clock soak of the rings (an injected clock covered 25 h); device-level disk I/O (not implemented); GUI consumption of the new endpoints (the frontend does not use them yet; the browser e2e suites were not run in this mission); a long-running (> 1 h) real-process leak soak of the sampler (the 100-cycle in-process start/stop test and 60 s idle runs were done).

## 20. Not required (with justification)

* **Persisting metric history across restarts** — the mission specifies in-memory bounded rings; persistence would add disk writes and a durability surface; restart-from-empty is tested and documented.
* **Per-principal / per-table metrics** — forbidden by the closed-key rule (they would be user-controlled keys).
* **Raw SQL text in the query list** — explicitly forbidden (secrets); only statement class and error class are kept.

## 21. Not implemented

Device-level disk IOPS/throughput; a flush-completion timestamp (`last_flush_ms` stays `null`); persistence of events beyond the existing security log; GUI screens for the new endpoints; a non-Windows validation.

## 22. Final certification matrix

Statuses: PASS / FAIL / OPEN / NOT REQUIRED / NOT TESTED / NOT IMPLEMENTED. "T:" names the test, "E:" the raw evidence. Implementation lives in `api/src/observability/{sampler,ring,events,queries,probe,version,mod}.rs`, `api/src/routes/{metrics_system,observability}.rs`, `api/src/{resources,metrics,auth,sql_session,server,state}.rs`, `api/src/routes/{mod,sql}.rs`, `cli/src/{host,ops_cmd}.rs`, `src/lsm/mod.rs` (accessor) unless a row names more.

| # | Row | Status | Implementation | Test | Raw evidence |
|---|---|---|---|---|---|
| 1 | HEALTH | **PASS** | sampler.rs `collect` (classification), mod.rs `disk_low_percent`; `/healthz` untouched | `health_follows_storage_state_and_disk_free_and_leaves_readiness_alone` | pressure→degraded, full→failed, free<10 %→degraded (exactly 10 % healthy), lock lost→failed, all back to healthy; real C: at 8.7 % free reported degraded. Thresholds = decision requiring review (section 8) |
| 2 | READINESS | **PASS** | unchanged `/readyz`; sampler reports `readiness` | `readiness_stays_true_while_safe_index_recovery_runs`; `/readyz` 200 under degraded/recovery | readiness `ready` with index recovery `running` and `failed`; `/readyz` field set identical before/after (section 16) |
| 3 | STATUS | **PASS** | unchanged `/v1/status`, `/v1/admin/status` | black-box inventory diff `obs1.py` vs `obs1_after.py`; existing `api_integration`, `admin_ops` | 18 existing endpoints: 0 differences in status code, unauthenticated status, Cache-Control, field names and types |
| 4 | PRODUCT VERSION / BUILD IDENTITY | **PASS** | version.rs, `api/build.rs`, routes/observability.rs | `version_reports_product_version_build_identity_and_startup_time` | product_version = CARGO_PKG_VERSION; git revision hex (+`-dirty`) or null; no `\`/`/` in build id; startup time within 60 s of now |
| 5 | METRICS SNAPSHOT | **PASS** | sampler.rs `Snapshot`, routes/metrics_system.rs | `metrics_system_has_every_required_field_with_the_right_types` | all required groups/fields present with correct types; one immutable snapshot per response |
| 6 | METRIC FRESHNESS | **PASS** | metrics_system.rs (`age_ms`, `stale`, `last_sample_ms`) | `a_wedged_sampler_goes_stale_then_failed_and_recovers_when_unblocked`, `a_dead_sampler_is_visible_as_stale_…` | stale=true and age ≥ 5,000 ms after 6 s of no publish; age ≥ 10,000 ms after 11 s; age grows ≥ 400 ms in 400 ms with the thread stopped |
| 7 | SAMPLER HEALTH | **PASS** | sampler.rs `SamplerState`, `effective_state` | wedged / dead / panic / probe-failure tests | running→degraded on a failing probe, →failed after 10 s without a publish, →running on recovery; `not_started` after stop |
| 8 | TIME-SERIES | **PASS** | ring.rs, metrics_system.rs `timeseries` | `timeseries_windows_…`, `timeseries_returns_injected_history_…`, ring unit tests | resolution 60/15/60/3600 s exact; 15/240/1,440 points returned from 25 h of injected ticks; empty history = 200 + empty arrays; 8 invalid windows = 400 VALIDATION_ERROR; 24 h body < 2 MiB; never-measured series = null. 7 d window verified by ring unit test (injected clock), not by a real 7-day run (NOT TESTED in wall-clock, section 19) |
| 9 | REQUEST THROUGHPUT | **PASS** | sampler.rs (delta of `total_requests`) | `rates_and_gauges_follow_real_load_and_fall_back_when_it_stops` | under 4 writers: http ≈ 384–400 req/s observed, falls to measured 0.0 (not null) when idle |
| 10 | SQL THROUGHPUT | **PASS** | sampler.rs (delta of SQL api requests) | same test | ≈ 363–372 sql/s under load; 0.0 after |
| 11 | WRITE THROUGHPUT | **PASS** | sampler.rs (delta of `completed_ok`) | same test; `obs6.py` | ≈ 363–368 commits/s under load; 0.0 after; process write IOPS 335 vs psutil 330 |
| 12 | LATENCY DISTRIBUTION | **PASS** | reuses `route_percentiles("POST /v1/sql")`; metrics_system.rs | `latency_percentiles_are_consistent_with_client_measured_times` | server p50/p95/p99 = 3.207/4.889/11.101 ms vs client 3.965/5.680/6.945 ms: positive, ordered, server p50 ≤ client p50, server p95 ≤ client p99; null before any statement. Limitation: reservoir percentiles of one route (`POST /v1/sql`) |
| 13 | ACTIVE CONNECTIONS | **PASS** | server.rs `serve_observed` gauge | `rates_and_gauges_…` | peak 10 with 5 held + pooled clients; falls by ≥ 5 when the 5 held sockets close |
| 14 | ACTIVE SESSIONS | **PASS** | sql_session.rs `open_count`, sessions endpoint | `rates_and_gauges_…`, `sessions_endpoint_lists_bounded_records_…` | 3 open BEGINs → 3; list ordered by id, ≤ 200, limit honoured, COMMIT removes |
| 15 | ACTIVE TRANSACTIONS | **PASS** | reuses `TxnMetrics` active count | `rates_and_gauges_…` | ≥ 3 under load (open sessions + in-flight implicit transactions, peak 7), exactly 3 when idle again |
| 16 | ACTIVE QUERIES | **PASS** | queries.rs `active` gauge | `running_and_cancelled_queries_are_visible_…` | gauge ≥ 1 and `running` records observed under 24-way load; 0 after drain |
| 17 | QUERY STATES | **PASS** | queries.rs `QueryState` | `queries_endpoint_…`, `running_and_cancelled_…`, timeout test | running / succeeded / failed / cancelled / timed_out all observed; classes from a closed list; rows_returned/affected correct |
| 18 | QUERY TIMEOUTS | **PASS** | routes/sql.rs, queries.rs | `a_statement_past_its_deadline_is_recorded_as_timed_out_with_an_event` | HTTP 504, state timed_out, timeout_state deadline_exceeded, error_class SQL_TIMEOUT, event `query.timeout` |
| 19 | QUERY CANCELLATIONS | **PASS** | queries.rs `Drop` + `log_disconnect_to`, routes/sql.rs | `running_and_cancelled_queries_are_visible_…` | client disconnects produce `cancelled` records and `query.cancelled` events (the test found the missing event; fixed) |
| 20 | QUERY FAILURES | **PASS** | routes/sql.rs | `queries_endpoint_…never_sql_text`, `events_endpoint_…` | parse failure and unknown-table failure recorded with error_class; `query.failed` event; no SQL text, names or parser message in any response |
| 21 | PROCESS CPU | **PASS** | probe.rs/resources.rs `cpu_seconds` (`GetProcessTimes`), sampler.rs | CPU-failure test; `obs6.py` cross-check | API mean 3.0 % of machine vs psutil 23.98 % of one core ÷ 8 = 3.0 %; null on first tick and on probe failure. Definition: percent of all logical CPUs |
| 22 | PROCESS RSS | **PASS** | resources.rs `rss_and_peak` | RSS-failure test; `obs6.py` | 11,505,664 vs psutil 11,743,232 B (2.0 %); peak RSS equal to the byte |
| 23 | SYSTEM MEMORY | **PASS** | resources.rs `system_memory` (`GlobalMemoryStatusEx`) | `obs6.py` | total equal to the byte (17,060,876,288); used 41.93 % vs 41.9 % |
| 24 | DISK CAPACITY | **PASS** | resources.rs `disk_total_free` (`GetDiskFreeSpaceExW`) | disk-failure test; `obs6.py` | volume total equal to psutil to the byte (87,995,445,248); null on failure |
| 25 | DISK FREE | **PASS** | same | `obs6.py`; health test | free equal to psutil to the byte (7,641,255,936); feeds health rule |
| 26 | DATABASE SIZE | **PASS** | reuses `disk_usage` directory walk every 10 s (`sizes_age_ms` exposed) | `obs6.py`, `obs7*.py` | db_bytes 3,756,303 and 52,763,853 and 159.5 MB equal the directory walk after settle; value is up to 10 s old by design (idle point 84 vs 480 B shows that lag) |
| 27 | WAL SIZE | **PASS** | same + wal segment count | `obs6.py`, `obs7*.py` | wal_bytes 3,756,243 and 6,264,707 equal to the walk; segments 2 = 2 files |
| 28 | SSTABLE SIZE | **PASS** | same | `obs7*.py` | sstable_bytes 46,497,945 and 152,173,663 equal to the walk |
| 29 | DISK IOPS | **PASS** | resources.rs `process_io` (`GetProcessIoCounters`), sampler.rs deltas | `obs6.py`; null-all test | **process-level** (this process), not device-level: write 335 IOPS vs psutil 330; read 0; null when the call fails. Device-level = NOT IMPLEMENTED (section 21) |
| 30 | DISK THROUGHPUT | **PASS** | same | `obs6.py` | write 0.150 MB/s vs psutil 0.150 MB/s; process-level as above |
| 31 | WAL STATE | **PASS** | reuses `pool_stats().state`, wal segment/bytes | `metrics_system_has_every_required_field_…`; `obs6.py` | `Running`, segments, bytes present and equal to files |
| 32 | COMPACTION STATE | **PASS** | `LsmEngine::compaction_running()` (src/lsm/mod.rs) + reused `CompactionMetrics` | `obs7.py`, `obs7b.py` | cycles 3 = 3 and 11 = 11, live SSTables equal, last_duration_ms identical to the existing endpoint; `running=true` observed during the 140 MB run |
| 33 | INDEX RECOVERY STATE | **PASS** | recovery.rs (`set_for_test`, event), sampler.rs | `readiness_stays_true_while_safe_index_recovery_runs`; existing `index_backfill_crash_integration` (3 passed) | `index_build_state` = running / failed / complete reported; readiness unaffected |
| 34 | BACKGROUND OPERATIONS | **PASS** | sampler.rs (`flush_queue_depth`, `pending_groups`, `index_build_state`) | `obs7b.py`; field test | flush_queue_depth = 1 observed under load, pending_groups present; `last_flush_ms` is `null` by design (no engine timestamp; section 17) |
| 35 | RESOURCE-LIMIT COUNTERS | **PASS** | auth.rs, sql_session.rs, routes/sql.rs; reuses `ExecMetrics`/`PlannerMetrics`/group-commit rejections | `rate_limit_rejections_are_counted_with_events`, `session_cap_refusals_and_admin_actions_are_counted_and_visible` | rate_limited == number of 429s observed; sessions_rejected == 1 after one refused BEGIN; events `auth.rate_limited`, `session.rejected`. wal_backpressure and sql_resource_limit_hits are reused existing counters (exercised by their own existing suites) |
| 36 | SECURITY EVENT COUNTERS | **PASS** | auth.rs, routes/mod.rs `audit_middleware` | `session_cap_…admin_actions…`, `two_instances_…`, `events_endpoint_…` | auth_failures exact (0 vs 1 across instances), forbidden == 1, admin_actions == 1 with last action route pattern `/v1/admin/check` + time; a Reader's 403 is not an admin action |
| 37 | OPERATIONAL EVENT RETRIEVAL | **PASS** | events.rs, routes/observability.rs | `events_endpoint_is_bounded_clamped_…` | newest first, limit 1..200 clamped, non-integer 400, security and operational separate, every field present |
| 38 | STRUCTURED LOGGING | **PASS** | reused `security_log.rs` (JSON lines, fixed fields, 128-char field cap) | existing `security_log_integration` (2), `security_events_and_headers` (10), `security_log` unit tests | security log lines are structured JSON with fixed fields, no credential (leak scan). Operational events are in-memory only (not persisted): by design, section 20 |
| 39 | LOG BOUNDS | **PASS** | reused `security_log.rs` (1 MiB × 5 files) | `size_cap_and_generation_count_bound_total_disk_use`, `auth_failures_are_rate_bounded_…` | existing certified tests pass in the final regression; unchanged by this mission |
| 40 | EVENT BOUNDS | **PASS** | events.rs (256 + 256), queries.rs (200 + 2,048) | `events_endpoint_…` (600-event flood), `queries_endpoint_…` (260 extra), unit tests | 200 security events kept after 640 inserts and the operational event survived; ≤ 200 query records |
| 41 | METRIC CARDINALITY | **PASS** | routes/mod.rs `route_label`, metrics.rs (128 + overflow) | `adversarial_inputs_never_grow_the_metric_key_set`; `obs1_after.py` | key set unchanged after 1,000 random SQL, 1,000 random names, 1,000 random bearer keys, 1,000 random methods/paths; baseline FAIL 821 route keys → 23 |
| 42 | METRIC MEMORY BOUNDS | **PASS** | ring.rs (fixed `Box<[Sample]>`), caps in events/queries/metrics | ring unit tests; `timeseries_returns_injected_history_…` | series store 240,856 B after 25 h of injected ticks (cap 1,048,576); 2× capacity pushes do not grow; idle RSS delta +0.48 MB |
| 43 | SAMPLER FAILURE HANDLING | **PASS** | sampler.rs (`catch_unwind`, `OsProbe`) | failure-injection tests, panic test, wedge test | section 13 |
| 44 | SNAPSHOT CONSISTENCY | **PASS** | sampler.rs (`Arc<Snapshot>` under `RwLock`) | `back_to_back_reads_while_the_sampler_ticks_never_see_a_torn_snapshot` | 300 reads at 5 ms ticks: 0 torn reads, > 3 generations seen |
| 45 | MULTI-INSTANCE ISOLATION | **PASS** | per-`AppState` registries/counters; no statics | `two_instances_report_only_their_own_data_…`; CLI `two_real_instances_…` | section 14 |
| 46 | RESTART BEHAVIOR | **PASS** | host.rs / main.rs wiring (sampler start/stop order) | CLI `killing_the_owner_and_restarting_…` | same instance id, fresh uptime/counters, sampler publishes again, SQL works |
| 47 | CRASH BEHAVIOR | **PASS** | same | same test (process kill) | PID gone, listener refuses, restart clean. Process kill only; power loss NOT TESTED (section 19) |
| 48 | API PERFORMANCE | **PASS** | snapshot read path (no lock held across work) | `metrics_system_p95_is_below_50ms_with_16_concurrent_readers`; `obs5.py` | p95 0.771 ms (release test), 2.646 ms (debug test), 1.01–1.04 ms against the real binary (3 × 15 s, 16 readers, 0 errors) vs the 50 ms gate |
| 49 | OBSERVABILITY OVERHEAD | **PASS** | whole layer | `obs4_overhead.py` (5 runs × 3 workloads), `obs5.py` (3 × 60 s idle) | idle +0.48 MB RSS (+3.4 %), +1 thread, +1 handle, 0.0 s CPU in 60 s; load metrics within baseline run-to-run range (section 16). No pass/fail threshold was set by the mission; the numbers are reported, not called negligible |
| 50 | CLI OBSERVABILITY | **PASS** | cli/src/ops_cmd.rs `status_system` | CLI `status_system_prints_the_live_snapshot_and_never_a_credential` | `rubixdb status --system [--json]`: all sections printed, null shown as `-`, no `null`, no control characters, no credential; `rubixdb status` unchanged |
| 51 | GUI API COMPATIBILITY | **PASS** | additive endpoints only; existing handlers untouched | `obs1_after.py` inventory diff; existing API suites | 18 existing endpoints: identical field names/types/status/cache headers; the frontend does not consume the new endpoints yet and the browser e2e suites were not run (section 19) |
| 52 | REGRESSION | **FAIL** | — | section 9.2 | 3 pre-existing failures verified at clean HEAD: `repo_hygiene` ×2 (tracked credentials file) and `m1_3_thousand_writers_throughput` (release). Introduced by this mission: none. Per the rule, a failing mandatory regression row is FAIL |
| 53 | DOCUMENTATION | **PASS** | this file; PROGRESS.md, CHANGELOG.md, OPEN_ITEMS.md appended | — | all four documents written; append-only files only appended to |

Rows: 53 — PASS 52, FAIL 1, OPEN 0, NOT REQUIRED 0, NOT TESTED 0, NOT IMPLEMENTED 0.

## 23. Overall verdict

**OBSERVABILITY: IMPLEMENTED AND VERIFIED — OVERALL PASS NOT DECLARED**, because the mandatory REGRESSION row is FAIL (3 pre-existing failures, verified at clean HEAD and not introduced here); 52 of 53 rows PASS, 1 FAIL, 0 OPEN, 0 NOT TESTED rows (untested areas are listed in section 19); this certifies observability only, not WAL performance, product readiness or production readiness.


## 24. CLOSURE - 2026-10-07 (dated addendum; sections 1-23 above are unchanged)

This section is appended; nothing above was rewritten. Where it conflicts with sections 1-23 (for example "OPEN 0 / NOT TESTED 0" in section 22, the "uncommitted working tree" identity in section 0, the health thresholds of section 8, the `disk.*` I/O names, row 49 PASS without a threshold), **this section governs**; the earlier text remains as the historical record. Companion documents: `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` (reconciliation, sections 16-17 closure evidence), `PHASE_RUBIXDB_FULL_OBSERVABILITY_ARCHITECTURE.md` (sections 12.1-12.7), `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md`.

### 24.1 Identity of what was closed

* Repository: branch `master`, HEAD `2cbd5a7e2b1891898c34c58e2adb8df57a9c3acb` (`observability: sampler, system metrics, time series, diagnostics, status --system`) plus the uncommitted closure changes committed together with this section (the closure commit hash is reported in the final report and in `git log`; it cannot be written into a file inside that same commit).
* Start state: the working tree already held the Prompt 1 documents and the **uncommitted edits of an earlier, stopped closure attempt** (it had stopped at a second P0 defect, section 24.5). That work was resumed, not discarded; every claim below was re-measured with the final build.
* **Final build used for every measurement:** `E:\rubixdb_closure\rubixdb_final2.exe`, SHA-256 `044c45eaecd30fa07f38c7528f15a6a90867962f587a9c7d5fd517363280373e`, built 2026-10-06 23:58 from the working tree. No file under `src/`, `api/src/`, `cli/src/`, `instance/src/` is newer than it; only tests and documents changed afterwards. It reports `git_revision 2cbd5a7e2b18` **without** `-dirty` although the tree was modified (row A10, FAIL). Reference binaries: prior certified `E:\rubixdb_recon\rubixdb_certified_fe511ff.exe`; Prompt 1 HEAD build `rubixdb_head_5c01ef.exe`; pre-observability baseline `rubixdb_base_8e4737.exe` (`8e47379`).
* Protected paths: zero diff (row B14).

### 24.2 Maintainer decisions applied (value actually used)

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

### 24.3 Code and test changes

`api/src/sql_session.rs`, `api/src/routes/sql.rs` (session lease and cap, D2); `api/src/observability/{sampler,mod,ring,events}.rs`, `api/src/routes/{metrics_system,health}.rs` (D1, D3, D5, D6, D9); `cli/src/host.rs` (three-valued lock probe), `cli/src/ops_cmd.rs` (`status --system` prints `coordinator=`, `lock=`, advisory, `process io`); tests `api/tests/observability.rs` (34 tests), `cli/tests/observability_integration.rs` (6 tests), unit tests in `sql_session.rs` and `sampler.rs`. No file in `src/` (engine), no dependency, no `Cargo.toml` / `Cargo.lock` changed.

### 24.4 P0: session registry leak (D2)

* **Reproduced first** on the Prompt 1 HEAD build with a real server and real sockets (`ghost_repro.py`, 60 sessions each `BEGIN` then a heavy `SELECT` carrying the session id, socket reset mid-statement): 3 s and 23 s after the last abort `sessions_total` 60 (all `executing`, ages 7.7 s -> 27.8 s), `active_sessions` 60, `active_transactions` 0, `COMMIT` on the first 10 non-200 (`final/ghost_before_head.txt`).
* **Fix:** `SessionLease` (`routes/sql.rs`) closes the observation entry and emits a `session.closed` event (existing event shape, `CLIENT_DISCONNECTED`) when the statement future is dropped; the transaction is rolled back by the existing `Transaction` drop path (no second rollback path); the per-principal cap counts `SessionMeta.principal`, so an executing session counts.
* **After, same script, final build:** `sessions_total` 0, `active_sessions` 0, `active_transactions` 0 at both 3 s and 23 s (`final/ghost_after_final.txt`). A second client can open a session at once, and the cap rejects at the cap (`a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind`, `per_principal_cap_counts_a_session_whose_statement_is_running`). Soak: 716 mid-statement aborts, `sessions_total` never above 2.

### 24.5 Second P0 found during closure: readiness flapped under write load (D5)

The earlier closure attempt derived readiness from `GroupCommitStats::sync_failures()` (`sync_attempts - sync_successes`, two independent atomics): it is transiently 1 while an fsync is in flight (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` section 7), so `/readyz` and `instance.readiness` flapped. Measured with 4 writers on real binaries (`final/readyz_flap_before_after.txt`):

| Binary | `/readyz.ready` false | `instance.readiness` not_ready | `instance.healthy` failed |
|---|---|---|---|
| earlier closure attempt (readiness from `sync_failures()`) | 374 of 426 | 76 of 85 | 76 of 85 |
| Prompt 1 HEAD build | 0 of 424 | 46 of 84 | 0 of 84 |
| **final (R2)** | **0 of 424** | **0 of 84** | **0 of 84** |

The work stopped there and the maintainer chose R2 (24.2). The same counter still feeds `errors.wal_sync_failures` (row A81) and the certified `/v1/admin/status` `wal.poisoned` (row C09): both measured wrong under load; neither was changed.

### 24.6 Overhead methodology (D7) and measurements

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

### 24.7 Soak (D8) and retention attribution

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

### 24.8 API compatibility diff (black box)

Method: `compat.py` starts the certified binary and the final binary on copies of the same data and compares, for `/healthz`, `/readyz`, `/v1/status`, `/v1/metrics`, `/v1/admin/status` (and the unmatched route): status, headers (content-type, cache-control, CSP, nosniff, referrer-policy), no-key / bad-key / POST behaviour, error shape, every JSON path and type, and latency over 200 sequential requests. **Result: 0 differences for all five endpoints and the unmatched route.** `/v1/metrics/system` (added in `2cbd5a7`, so it is also in the certified binary) differs in exactly 12 schema paths, every one justified by a decision:

| Change | Decision |
|---|---|
| ADDED `instance.lock_state` (str) | D1 |
| ADDED `instance.coordinator_state` (str) | D5 (additive extension) |
| ADDED `disk.free_advisory` (str), `disk.free_advisory_threshold_percent` (float) | D1 |
| REMOVED `disk.read_iops`, `disk.write_iops`, `disk.read_mb_per_sec`, `disk.write_mb_per_sec` | D3 (rename) |
| ADDED `process.read_ops_per_sec`, `process.write_ops_per_sec`, `process.read_mb_per_sec`, `process.write_mb_per_sec` | D3 (rename) |

Not purely additive, as the maintainer was told: four existing field names moved by D3. No field was retyped. `instance.readiness` keeps its type (string). Security headers (CSP, nosniff, referrer-policy, cache-control) unchanged on every endpoint; loopback only; no secret in any observability response (`no_api_key_appears_in_any_observability_response`).

### 24.9 Full regression and classification

Commands (logs `E:\rubixdb_closure\final\reg_*.log`, per-suite table `suite_tables.txt`): `cargo fmt --all -- --check` (exit 0), `cargo clippy --workspace --all-targets --all-features -- -D warnings` (exit 0), `cargo check --workspace --all-targets --all-features` (exit 0), `cargo test --workspace --no-fail-fast` (1,346 passed, 2 failed, 28 ignored), `cargo test --release --workspace --no-fail-fast` (1,347 passed, 3 failed, 26 ignored). The observability suites by name: `api/tests/observability.rs` 34/34, `security_events_and_headers` 10/10, `api_integration` 16/16, `admin_ops` 12/12, `api_security_validation` 11/11, `api_http_fuzz` 4/4, `api_cancellation` 1/1, CLI `observability_integration` 6/6, `rubixdb-api --lib` 77 passed 1 ignored (debug and release).

| Failing test | Where | Class | Evidence |
|---|---|---|---|
| `repo_hygiene::no_tracked_credentials_json` | debug, release | PRE-EXISTING | the file is tracked at `8e47379` and HEAD (`git ls-tree`) |
| `repo_hygiene::no_tracked_file_contains_a_64_hex_admin_key_literal` | debug, release | PRE-EXISTING | same file |
| `m1_3_thousand_writers_throughput` | release | PRE-EXISTING, INTERMITTENT | workspace 43,228 ops/s; isolated 111,824 / 60,610 / 92,078 on this tree; clean `8e47379` worktree 55,256 / 80,562 / 110,880; `src/` has zero diff vs HEAD |
| `sampler_start_stop_100_times_leaves_no_thread_behind` (first release run only) | release | INTRODUCED BY THIS MISSION'S TEST, FIXED | see B13: 11 of 12 repeated runs failed while 10 of 10 isolated runs passed; skipping this mission's in-process write-load test: 6 of 6 pass; fixed by moving that check into the real-process CLI suite (no existing test edited); then 10 of 10 pass |

### 24.10 Changed test assertion (Decision D1) - stated explicitly

* **File and test:** `api/tests/observability.rs`, `health_follows_storage_state_and_disk_free_and_leaves_readiness_alone`.
* **Old expectation (at `2cbd5a7`):** with free space at `total / 10 - 1` (just under 10 %), `instance.healthy` is `degraded`; at exactly `total / 10` and above it is `healthy`.
* **New expectation:** at `total / 10 - 1`, `instance.healthy` is `healthy` and `disk.free_advisory` is `low`; at exactly `total / 10`, `healthy` and `ok`; the `healthy` expectations at 10 % and above are unchanged.
* **Reason:** **Decision D1 explicitly reverses the policy: the 10 % rule is now advisory and no longer an input of `instance.healthy`. The assertion changed because the policy changed, not because the test was flaky, intermittent or inconvenient.** No other health assertion in that test was weakened (StoragePressure -> degraded, StorageFull -> failed, lock lost -> failed all stay asserted). The other edits to existing tests are the mechanical D3 renames (`disk_*` -> `process_*`) with an added test that the old names are absent.

### 24.11 Findings recorded, not fixed (outside the approved items)

* A10 (FAIL): `-dirty` suffix not applied to a binary built from a modified tree.
* A32 (FAIL): `untracked_active` is cumulative (Prompt 1 P2).
* A81 (FAIL): `errors.wal_sync_failures` shows a phantom 1 under write load; C09 (FAIL): the certified `/v1/admin/status` `wal.poisoned` is true under write load with no failure.
* A83 (FAIL): handle count after the soak, attributed to the engine (24.7).
* A80 (NOT IMPLEMENTED): an fsync-poisoned committer is not visible without an engine accessor (ADR-OBS-01).
* Not tested: A09, A12, A14, A16, A19, A64, A70, A71, A82.

### 24.12 Final certification matrix

Statuses are exactly PASS / FAIL / OPEN / NOT REQUIRED / NOT TESTED / NOT IMPLEMENTED. The three sections are independent: no status transfers between them and each has its own arithmetic.

#### A. OBSERVABILITY IMPLEMENTATION STATUS

| # | Row | Status | Evidence |
|---|---|---|---|
| A01 | HEALTH classification (`instance.healthy`) | **PASS** | `sampler::classify_health` (pure) + `collect`; unit `policy_tests::health_rules_one_by_one`; integration `health_follows_storage_state_and_disk_free_and_leaves_readiness_alone` (StoragePressure -> degraded, StorageFull -> failed, lock lost -> failed, each back to healthy), `a_three_valued_lock_probe_never_reads_unavailable_as_lost`, `storage_pressure_degrades_and_the_advisory_echoes_its_threshold`. Soak: `healthy` in every one of 7,561 generations. Decision D1. |
| A02 | HEALTH rule: readiness false -> `failed` at once (30 s grace removed) | **PASS** | Classifier level: `policy_tests::health_rules_one_by_one` (`classify_health(false, ..) == failed`, no timer). End to end the rule cannot fire today because readiness is constant `true` (D5/R2); it stays in the classifier so ADR-OBS-01 would flow through it. Decision D1. |
| A03 | HEALTH rule: lock probe, real instance lock, `held` path | **PASS** | Real process: `cli/tests/observability_integration.rs::a_real_owner_reports_the_lock_as_held_and_one_readiness_on_both_endpoints` (`lock_state == held`, `status --system` prints `lock=held`); soak: `lock_state` `held` throughout. Three-valued probe: `a_three_valued_lock_probe_never_reads_unavailable_as_lost` (closure probe: unavailable -> healthy, not_held -> failed). Decision D1. |
| A04 | READINESS endpoint (`/readyz` meaning unchanged; index recovery reported beside it) | **PASS** | Black-box inventory against the certified binary: `/readyz` 0 differences in status / headers / auth / error shape / schema (`E:\rubixdb_closure\compat\diff.txt`); `readiness_stays_true_while_safe_index_recovery_runs`; soak: `/readyz` `{ready:true, storage_state:Healthy, index_recovery:complete}` at the end. |
| A05 | READINESS UNIFICATION (`instance.readiness` and `/readyz.ready`: one definition) | **PASS** | Both call `sampler::ready()` (`sampler.rs:280`, `const fn` returning `true`): `sampler.rs:465` (per tick, stored as `instance.readiness`) and `routes/health.rs:47` (per request). Two call sites of one constant function, not one call; they cannot disagree. Tests: `policy_tests::readiness_is_one_constant_definition`; real binary under 4-writer load `readyz_and_instance_readiness_stay_true_and_equal_under_real_write_load` (> 50 polls, > 4 generations, > 100 writes, 0 deviations); `readyz_under_write2.py`: 0 of 424 `/readyz` false, 0 of 84 `readiness` not_ready (the earlier derivation from `sync_failures()`: 374 of 426 and 76 of 85). Decision D5 (option R2). Type note: `/readyz.ready` is bool, `instance.readiness` is the string `ready`/`not_ready` (both pre-existing types kept). |
| A06 | STATUS and other pre-existing endpoints unchanged (`/healthz`, `/readyz`, `/v1/status`, `/v1/metrics`, `/v1/admin/status`) | **PASS** | Black-box inventory, certified binary `rubixdb_certified_fe511ff.exe` vs final build, same data: 0 differences in status, headers (content-type, cache-control, CSP, nosniff, referrer), auth (no key / bad key / POST), error shape and JSON schema for all five plus the unmatched route; latency over 200 sequential requests equal within noise (`compat.py`, `diff.txt`). |
| A07 | PRODUCT VERSION / BUILD IDENTITY | **PASS** | Final build `/v1/observability/version`: `product_version 0.1.0`, `build_identifier 0.1.0-release-x86_64-windows`, `git_revision`, `startup_timestamp_unix_ms`; no path, host, user or credential. Tests `version_reports_product_version_build_identity_and_startup_time`, unit `identity_has_no_paths_and_a_null_revision_is_allowed`. Revision truthfulness is A10. |
| A08 | GIT REVISION UNKNOWN -> `null` (no-git release build) | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch (`version_nogit.py`: `git_revision: null`); `api/build.rs` and `observability/version.rs` unmodified. |
| A09 | DEV-PROFILE (debug) BUILD IDENTITY, revision unknown | **NOT TESTED** | No debug no-git binary was built (`api/build.rs` `emit_profile`). |
| A10 | GIT REVISION `-dirty` SUFFIX FRESHNESS | **FAIL** | Observed this mission: the binary soaked and measured here was built from a modified working tree (14 modified files) and reports `git_revision 2cbd5a7e2b18` with no `-dirty` (`E:\rubixdb_closure\final\wsf_version.txt`). `api/build.rs` re-evaluates only when `.git/HEAD`, `.git/index` or the override change. Not fixed (not an approved item); the build recorded at commit time is rebuilt from the committed tree so its revision is the new commit's. |
| A11 | METRICS SNAPSHOT shape (groups, types, nulls) | **PASS** | `metrics_system_has_every_required_field_with_the_right_types` (now also `instance.coordinator_state`), `a_platform_that_measures_nothing_yields_nulls_never_zeros`; 34/34 debug and release. |
| A12 | `errors.*` (4), `limits.wal_backpressure_rejections`, `limits.sql_resource_limit_hits`, `storage_state`: presence / type / value | **NOT TESTED** | No test asserts them (unchanged since Prompt 1 A12). `errors.wal_sync_failures` is A82. |
| A13 | METRIC FRESHNESS (timestamp, generation, age, state, stale) | **PASS** | `a_wedged_sampler_goes_stale_then_failed_and_recovers_when_unblocked`, `a_dead_sampler_is_visible_as_stale_and_failed_never_as_current_data`. Soak: snapshot age <= 1,015 ms over 7,561 generations. |
| A14 | FRESHNESS when no snapshot exists yet (`stale:false` with `age_ms:null`) | **NOT TESTED** | `routes/metrics_system.rs` no-snapshot branch; not exercised (Prompt 1 A14). |
| A15 | SAMPLER HEALTH (running / degraded / failed / not_started) | **PASS** | `cpu_rss_and_disk_read_failures_null_the_field_degrade_the_sampler_and_never_stop_it`, `a_panic_in_a_tick_is_contained_and_the_sampler_recovers`, `a_wedged_sampler_...`, `a_dead_sampler_...`. |
| A16 | SAMPLER: 5 consecutive panicking ticks -> `failed` | **NOT TESTED** | `a_sampler_that_keeps_failing_is_logged_once_and_reads_failed` asserts `failed` and exactly one WARN line, but the 5-panic transition through a real panicking probe is not exercised end to end (Prompt 1 A16 unchanged). |
| A17 | SAMPLER START LOGGING (start failure logged once at WARN; state `failed`) | **PASS** | `sampler::report_start_failure` (log + `failed`); tests `a_sampler_that_could_not_start_logs_the_reason_and_reads_failed_not_not_started`, `a_sampler_that_keeps_failing_is_logged_once_and_reads_failed`. Caveat: the start-failure test drives the handler through a `#[doc(hidden)]` test hook, because a thread-spawn failure cannot be forced from outside; `cli/src/host.rs` still ends in `.ok()`, after the log. Reuses the existing `tracing` path. Decision D9. |
| A18 | TIME-SERIES (windows, resolution, bounds, 400 on bad window, null for never measured, <= 2 MiB) | **PASS** | `timeseries_windows_resolution_empty_history_and_invalid_window`, `timeseries_returns_injected_history_oldest_first_with_exact_resolution_and_a_size_cap`; series renamed (A78). |
| A19 | TIME-SERIES over real 24 h / 7 d wall-clock | **NOT TESTED** | The 127.5-min soak filled the real 15 m (15 points) and 1 h (240 points) windows and 125 of 1,440 points of the 24 h window; a real 24 h / 7 d run was not performed (environment: no such run available in this session). |
| A20 | REQUEST THROUGHPUT | **PASS** | `rates_and_gauges_follow_real_load_and_fall_back_when_it_stops`. |
| A21 | SQL THROUGHPUT | **PASS** | Same test. |
| A22 | WRITE THROUGHPUT | **PASS** | Same test. |
| A23 | LATENCY DISTRIBUTION (`latency.query_p50/p95/p99_ms`) | **PASS** | `latency_percentiles_are_consistent_with_client_measured_times`. Definition caveat unchanged (handler time of the newest 1,000 SQL requests). |
| A24 | ACTIVE CONNECTIONS | **PASS** | `rates_and_gauges_...`; soak: sockets 2-18 under load, back to 1 in the traffic-free tail. |
| A25 | ACTIVE SESSIONS gauge and `/v1/observability/sessions` truthfulness | **PASS** | Was FAIL in Prompt 1. After the fix: `a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind`; real binary `ghost_repro.py`: 60 aborted sessions -> `sessions_total` 0 / `active_sessions` 0 (before: 60 / 60). Soak: 716 aborted mid-statement sessions, `sessions_total` never above 2, final 1 (the one held session). |
| A26 | ACTIVE TRANSACTIONS | **PASS** | Reuses `TxnMetrics`; soak final `active_transactions` 1 = the one held session's transaction. |
| A27 | ACTIVE QUERIES | **PASS** | `running_and_cancelled_queries_are_visible_while_real_expensive_queries_run`. |
| A28 | QUERY STATES | **PASS** | Same + `queries_endpoint_lists_bounded_records_never_sql_text`. |
| A29 | QUERY TIMEOUTS | **PASS** | `a_statement_past_its_deadline_is_recorded_as_timed_out_with_an_event`. |
| A30 | QUERY CANCELLATIONS | **PASS** | `running_and_cancelled_...`; in-transaction case: `a_disconnect_mid_statement_...` (A77). |
| A31 | QUERY FAILURES | **PASS** | `queries_endpoint_...never_sql_text`, `events_endpoint_...`. |
| A32 | TRACKED / UNTRACKED QUERY COUNTER CONSISTENCY (`untracked_active`) | **FAIL** | Unchanged from Prompt 1: `queries.rs:123,156,174`, `routes/observability.rs:93`: `untracked_active` is a cumulative counter, not a gauge. Not an approved item of this mission (Prompt 1 proposal P2); not fixed. |
| A33 | PROCESS CPU | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch (`cpu_xval.py`: 71.14 / 72.60 / 72.26 % vs psutil 70.93 / 72.45 / 72.41 %); `probe.rs` unmodified. |
| A34 | PROCESS RSS | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch (15,900,672 vs 15,908,864 B). |
| A35 | SYSTEM MEMORY | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch (equal to psutil to the byte). |
| A36 | DISK CAPACITY | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch (equal to psutil to the byte). |
| A37 | DISK FREE | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch (equal to psutil to the byte). |
| A38 | DATABASE SIZE (`disk.db_bytes`) and its age (`sizes_age_ms`) | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch; soak `sizes_age_ms` never above the 10 s refresh period. |
| A39 | WAL SIZE / segment count | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch; soak: `wal.segment_count` 1 -> 2 as writes ran. |
| A40 | SSTABLE SIZE | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch; soak: live SSTables 0 -> 3 -> 2 across 4 compaction cycles. |
| A41 | PROCESS I/O measurement | **PASS** | Prompt 1 measurement (2026-10-06, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` section 3.A) on code this mission did not touch (process write ops = commits 1:1). |
| A42 | DISK I/O LABELLING (process-level unmistakable) | **PASS** | Was FAIL. Fields are now `process.{read,write}_ops_per_sec`, `process.{read,write}_mb_per_sec`, documented in code and `ARCHITECTURE` 12.2 as the process's own I/O, not device activity (device saw 2.0x ops and ~177x bytes in Prompt 1 section 10). See A78. Decision D3. |
| A43 | DEVICE-LEVEL DISK I/O | **NOT REQUIRED** | Decision D4. Feasibility evidence (`ioctl_probe.py`, Prompt 1): `IOCTL_DISK_PERFORMANCE` reads unelevated in 4-27 us with no new crate; exposing it is a v2 policy decision. The `device_*` namespace is reserved and empty (test asserts none). |
| A44 | WAL STATE | **PASS** | `metrics_system_has_every_required_field_with_the_right_types`. |
| A45 | COMPACTION STATE | **PASS** | Same test; soak: `compactions` 4 cycles reported, `compaction_running()` unchanged accessor. |
| A46 | INDEX RECOVERY STATE | **PASS** | `readiness_stays_true_while_safe_index_recovery_runs`. |
| A47 | BACKGROUND OPERATIONS (`flush_queue_depth`, `pending_groups`, `index_build_state`) | **PASS** | Shape test; soak `flush_queue` series. |
| A48 | FLUSH TIMESTAMP `background.last_flush_ms` | **NOT REQUIRED** | Decision D6: always `null` in v1; the engine exposes no flush-completion timestamp; no engine change, no sampler-observed substitute. Contract in the field's doc comment (`routes/metrics_system.rs`), `ARCHITECTURE` 12.4 and `OPEN_ITEMS.md` 2026-10-07; asserted null by the shape test. |
| A49 | RESOURCE-LIMIT COUNTERS `rate_limited`, `sessions_rejected` | **PASS** | `rate_limit_rejections_are_counted_with_events`, `session_cap_refusals_and_admin_actions_are_counted_and_visible`. |
| A50 | SECURITY EVENT COUNTERS | **PASS** | Same suites + CLI `two_real_instances_keep_every_surface_apart_...` (alpha 3 auth failures, beta 0). |
| A51 | OPERATIONAL EVENT RETRIEVAL | **PASS** | `events_endpoint_is_bounded_clamped_and_keeps_security_and_operational_apart`. |
| A52 | STRUCTURED LOGGING (existing `security.log`) | **PASS** | `security_events_and_headers` 10/10 (debug and release), lib unit tests. |
| A53 | LOG BOUNDS (`security.log`) | **PASS** | `security_log.rs` unit tests (in the 77 lib tests); module unmodified. |
| A54 | EVENT BOUNDS (rings 256 + 256, <= 200 returned) | **PASS** | `events_endpoint_...` flood test; soak: operational events returned never above 200. |
| A55 | OPERATIONAL EVENT PERSISTENCE | **NOT REQUIRED** | v1 contract: bounded in-memory history (`events.rs:6-8`); durable security events go to the bounded `security.log`. CLI `killing_the_owner_and_restarting_it_starts_observability_state_from_empty`. |
| A56 | METRIC CARDINALITY (closed key set) | **PASS** | `adversarial_inputs_never_grow_the_metric_key_set`. |
| A57 | METRIC / RING MEMORY BOUNDS | **PASS** | Ring unit tests + compile-time assertion `ring.rs`; soak occupancy within documented bounds (A72). |
| A58 | NO UNBOUNDED GROWTH FROM ANY REQUEST (all registries) | **PASS** | Was FAIL (session-observation map). After the fix the map is cleaned on every exit path and counts against the per-principal cap: `per_principal_cap_counts_a_session_whose_statement_is_running` (unit), `a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind`; soak: 716 aborts, `sessions_total` <= 2. |
| A59 | SAMPLER FAILURE HANDLING (probe failure, panic, wedge) | **PASS** | Tests in A15. |
| A60 | SNAPSHOT CONSISTENCY (no torn reads) | **PASS** | `back_to_back_reads_while_the_sampler_ticks_never_see_a_torn_snapshot`. |
| A61 | MULTI-INSTANCE ISOLATION (re-verified, two real instances running together) | **PASS** | Real processes: CLI `two_real_instances_keep_every_surface_apart_and_a_restarted_one_starts_fresh` (metrics, sessions, queries, events, version, resources, background state of alpha vs beta: no cross-contamination under concurrent activity) and `two_real_instances_report_only_their_own_identity_and_a_stopped_one_leaves_the_other`; in-process `two_instances_report_only_their_own_data_...`. The soak ran a second real instance (control) beside the main one on a different port: no interference. |
| A62 | RESTART BEHAVIOR (state fresh; no global registry survives) | **PASS** | CLI `killing_the_owner_and_restarting_it_starts_observability_state_from_empty`; CLI two-instance test restarts beta (same id, new start time, 0 auth failures, 0 events, 0 sessions, alpha unchanged); in-process `two_concurrent_instances_share_nothing_and_a_restart_in_one_process_starts_empty` (fails if a global mutable registry survived). |
| A63 | CRASH BEHAVIOR (process kill) | **PASS** | Same CLI tests (process kill; not graceful stop, not power loss). |
| A64 | POWER LOSS | **NOT TESTED** | Belongs to durability certification; observability state is memory-only. Not tested. |
| A65 | API PERFORMANCE gate: `/v1/metrics/system` p95 < 50 ms with 16 readers | **PASS** | `metrics_system_p95_is_below_50ms_with_16_concurrent_readers` (debug and release); soak mixed phase: 4 metrics clients at ~24.9 k req/s alongside 8 readers and 4 writers: p50 0.149 ms, p95 0.221 ms, p99 0.263 ms. |
| A66 | API TAIL LATENCY (reported 387 ms - 1.35 s maxima) | **OPEN** | Decision D9: closed as NOT REPRODUCED, cause NOT TESTED (environment limitation); not re-investigated. Prompt 1 volume: 12,107,104 requests in 27 runs, slowest 214.0 ms; 1,852,006 requests in the original harness shape, slowest 65.7 ms; every request > 50 ms fell in the first 100 ms of a synchronized client start. Closes when the original host conditions can be reproduced. In this soak the 10 Hz observer's per-phase maxima were 8.05 ms (idle), 18.5 ms (read), 459 ms (write), 895 ms (mixed, 16 client processes saturating the host) and 444 ms (the first cool-down minute, when the load stopped); medians stayed 0.15-0.85 ms. Not attributed. |
| A67 | OBSERVABILITY OVERHEAD vs baseline (threshold D7) | **PASS** | D7 defaults applied to the FINAL column (5 runs per workload, interleaved, `overhead_final/analysis.txt`): idle RSS delta +0.48 MB (<= 2.0), idle CPU 0.026 % of one core at most over 3 x 120 s (<= 0.1; baseline 0.0 %), thread delta +1 (<= 2), read p95 median 1.471 ms inside the baseline range 0.926-2.112, write p95 median 73.18 ms inside the baseline range 72.09-76.40. See the overhead table in this section. |
| A68 | CLI OBSERVABILITY (`rubixdb status --system`) | **PASS** | `status_system_prints_the_live_snapshot_and_never_a_credential`; prints `coordinator=` and `lock=`. |
| A69 | GUI API COMPATIBILITY (existing endpoints untouched) | **PASS** | A06. |
| A70 | GUI / BROWSER CONSUMPTION of the new endpoints | **NOT TESTED** | No frontend code references them (no screen exists). |
| A71 | NON-WINDOWS RUNTIME | **NOT TESTED** | Environment limitation: Windows only; WSL2 has no Rust toolchain. Linux branches compile only; `disk_total_free` returns `None` off Windows. |
| A72 | REAL WALL-CLOCK SAMPLER SOAK (2 h, real process, real clients) | **PASS** | 127.5 min (7,652.3 s), 126 per-minute records, `D:\rubixdb_soak\soak_log.jsonl`. Profile 15 min idle / 45 read / 45 write / 15 mixed with a 4-client metrics load / 5 cool-down; sessions, queries, events pollers and mid-statement aborts throughout. Sampler `running` in every record; generations 1 -> 7,561 with 0 gaps and 0 regressions; max snapshot age 1,015 ms; 0 observer errors, 0 diagnostic errors, 0 client errors, 0 non-200 on any workload; observability-only control instance RSS 12.08 -> 13.41 MB (non-decreasing steps 56.6 %, not monotonic; 0.65 MB/h, second half 0.46 MB/h), threads 14-17, handles 131-134, sockets 2 throughout; threads in the 90 s traffic-free tail 14 vs baseline 15 (-1); sockets 1 vs 2; ring/series/event occupancy within bounds (sessions <= 2, queries <= 200, events <= 200, series 15/240/125 points). Decision D8. |
| A73 | STANDALONE `rubixdb-api` BINARY | **NOT REQUIRED** | `OPEN_ITEMS.md` 2026-10-04 SG-6 / D-2; unchanged. |
| A74 | READER PRIVILEGE POLICY for the observability endpoints | **NOT REQUIRED** | The v1 host provisions one principal, `local`, Admin (`cli/src/host.rs`). |
| A75 | DOCUMENTATION | **PASS** | Architecture sections 12.1-12.7, this closure section, RESULTS sections 16-17, PROGRESS / CHANGELOG / OPEN_ITEMS appended, ADR-OBS-01, `docs/PROJECT_STATE.md`, `missions/ACTIVE.md`. The Prompt 1 inconsistencies listed in RESULTS A75 are corrected by this dated section (the earlier text is kept above, unmodified). |
| A76 | HEALTH POLICY (Option 3: repository-defined states only; 10 % rule advisory; 30 s grace dropped; three-valued lock) | **PASS** | All of: `failed` = not ready / coordinator poisoned / StorageFull / lock not_held; `degraded` = StoragePressure; 9.9 % free -> healthy + advisory `low`, 10.0 % -> healthy + `ok` (`health_follows_storage_state_and_disk_free_and_leaves_readiness_alone`); lock `unavailable` neither failed nor any other change (`a_three_valued_lock_probe_never_reads_unavailable_as_lost`); documented in `ARCHITECTURE` 12.1 / 12.7 with the threshold labelled provisional, not a guarantee. One existing assertion changed because the policy changed (see "Changed test assertion"). Decision D1. |
| A77 | SESSION REGISTRY LEAK (disconnect mid-statement inside a transaction) | **PASS** | Reproduced on the Prompt 1 HEAD build (`ghost_before_head.txt`): 60 of 60 aborted sessions stay `executing`, `active_sessions` 60 vs `active_transactions` 0, `COMMIT` on them non-200. Fixed (`SessionLease`, `routes/sql.rs`; cap counts `meta`, `sql_session.rs`): same script on the final build: 0 / 0 / 0 and COMMIT non-200 (`ghost_after_final.txt`). Regression tests: `a_disconnect_mid_statement_inside_a_transaction_leaves_no_session_behind` (counts back to baseline, cap rejects at the cap, a new session opens at once, one `session.closed` event per abort), `per_principal_cap_counts_a_session_whose_statement_is_running`. The transaction is rolled back by the existing `Transaction` drop path; the event reuses the existing shape. Decision D2. |
| A78 | DISK I/O NAMING (`process.*`; `disk.*` = volume only; `device_*` reserved and empty) | **PASS** | `process_io_is_named_as_process_level_and_the_old_disk_names_are_absent` (old names absent, no `device*` key); black-box diff: the four `disk.*` I/O fields removed and `process.*` added, nothing else changed (approved rename, see API compatibility). Decision D3. |
| A79 | COORDINATOR STATE (additive `instance.coordinator_state`: alive / poisoned / not_started) | **PASS** | `policy_tests::coordinator_state_comes_from_public_pool_state_only`; `an_orderly_engine_stop_changes_coordinator_state_only_never_readiness`; real binary: `alive` under 4-writer load and in the soak's final state. Limit: A80. |
| A80 | COMMITTER-POISON VISIBILITY (fsync-poisoned committer) | **NOT IMPLEMENTED** | `LsmEngine` exposes no accessor for `GroupCommitter::is_poisoned()`; adding one is an engine change not authorised here. `coordinator_state == poisoned` covers only a dead coordinator thread (`PoolState::Failed`). Proposed in `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md` (OPEN policy). |
| A81 | `errors.wal_sync_failures` truthfulness | **FAIL** | Reads `GroupCommitStats::sync_failures()` = `sync_attempts - sync_successes`, transiently 1 while an fsync is in flight (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` section 7). Measured on the final binary under 4 writers: non-zero in 97 of 108 polls, values {0, 1}, no write failed (`wsf_version.txt`). Not an approved item; not changed. |
| A82 | REAL lock probe: `not_held` and `unavailable` paths on a real instance | **NOT TESTED** | Only the `held` path of `InstanceLock::try_acquire` is exercised on a real instance; lost-lock and I/O-error outcomes were exercised with closure probes only (A03). |
| A83 | SOAK: handle count returns to baseline +/- 2 (D8 literal criterion) | **FAIL** | Traffic-free tail (90 s, no request in flight): 121 handles vs idle baseline 112 (+9; restart reference on the same data 109). Attribution (`attribution.py`, same read+write load, no pollers): the pre-observability baseline binary `8e47379` retains +14 handles (104 -> 118) and +12.7 MB RSS, the final binary +12 (107 -> 119) and +14.7 MB; the observability-only control instance stayed at 131-134 handles. So the retention is the engine/runtime, not observability, but the criterion as written is not met. Recorded in `OPEN_ITEMS.md`; the maintainer may redefine it. |

Rows: **83** - PASS 63, FAIL 4, OPEN 1, NOT REQUIRED 5, NOT TESTED 9, NOT IMPLEMENTED 1  (check: 63 + 4 + 1 + 5 + 9 + 1 = 83)

#### B. WORKSPACE REGRESSION STATUS

| # | Row | Status | Evidence |
|---|---|---|---|
| B01 | `cargo fmt --all -- --check` | **PASS** | exit 0 (`reg_fmt.log`). |
| B02 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **PASS** | exit 0 (`reg_clippy.log`); NOT TESTED in Prompt 1. |
| B03 | `cargo check --workspace --all-targets --all-features` | **PASS** | exit 0 (`reg_check.log`). |
| B04 | `api/tests/observability.rs`, debug and release | **PASS** | 34 passed in each; release 10 of 10 consecutive full-suite runs after the fix (see B13). |
| B05 | `api/tests/security_events_and_headers.rs` | **PASS** | 10/10 debug and release. |
| B06 | `api_integration` (16), `admin_ops` (12), `api_security_validation` (11), `api_http_fuzz` (4), `api_cancellation` (1) | **PASS** | All pass in debug and release (`suite_tables.txt`). |
| B07 | `rubixdb-api --lib` | **PASS** | 77 passed, 1 ignored, debug and release. |
| B08 | CLI `observability_integration` | **PASS** | 6/6 debug and release (adds the real-process lock/readiness test, the two-instance restart test and the 4-writer readiness test). |
| B09 | `tests/repo_hygiene.rs` (2 of 4 tests) | **FAIL** | `no_tracked_credentials_json`, `no_tracked_file_contains_a_64_hex_admin_key_literal`: `frontend/.e2e-crossbrowser-data/default/credentials.json` is tracked at `8e47379` and at HEAD (`git ls-tree`). Class: PRE-EXISTING (`OPEN_ITEMS.md` 2026-10-04); not caused by observability. |
| B10 | `m1_3_thousand_writers_throughput` (release, >= 80,000 ops/s) | **FAIL** | Workspace run: 43,228 ops/s (first run 43,557). Isolated on this tree: 111,824 / 60,610 / 92,078. Clean `8e47379` (separate worktree, same test): 55,256 / 80,562 / 110,880. `git diff HEAD -- src/` is empty. Class: PRE-EXISTING, INTERMITTENT (engine WAL throughput; unrelated to observability). |
| B11 | `cargo test --workspace --no-fail-fast` (debug) | **FAIL** | 1,346 passed, 2 failed, 28 ignored: exactly the two B09 tests. Both PRE-EXISTING. |
| B12 | `cargo test --release --workspace --no-fail-fast` | **FAIL** | 1,347 passed, 3 failed, 26 ignored: the two B09 tests and B10. All three PRE-EXISTING. |
| B13 | Failures introduced by this mission | **PASS** | One occurred and is fixed: `sampler_start_stop_100_times_leaves_no_thread_behind` failed in the first release workspace run and in 11 of 12 repeated full-suite runs of the release observability binary (threads 86-87 -> 88-95) while passing 10 of 10 alone; cause (proven by skipping): this mission's new in-process write-load test grew the tokio blocking pool in the same process (skipping it: 6 of 6 pass; `--test-threads=1`: 2 of 2). Class: INTRODUCED BY THIS MISSION'S TEST (test interference, not a sampler leak). Fix: the write-load check moved to the real-process CLI suite; no existing test was edited for it. After: 10 of 10 consecutive passes, and the final workspace runs above. |
| B14 | Protected paths: zero diff (`src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`) | **PASS** | `git diff --stat HEAD -- src/wal/ src/manifest/ src/sstable/ src/compaction/ src/error.rs` (working tree) and `git diff --cached --stat` on the same paths: both empty. `git diff --stat HEAD -- src/ Cargo.toml Cargo.lock`: empty (the observability commit's `LsmEngine::compaction_running()` accessor is already in HEAD `2cbd5a7`; nothing added since). |

Rows: **14** - PASS 10, FAIL 4, OPEN 0, NOT REQUIRED 0, NOT TESTED 0, NOT IMPLEMENTED 0  (check: 10 + 4 + 0 + 0 + 0 + 0 = 14)

#### C. WHOLE-PRODUCT PRODUCTION READINESS

| # | Row | Status | Evidence |
|---|---|---|---|
| C01 | WAL throughput gate M1.3 (>= 80,000 ops/s) | **FAIL** | B10; `OPEN_ITEMS.md`. |
| C02 | No tracked credential material in the repository | **FAIL** | B09. |
| C03 | Power-loss durability | **NOT TESTED** | A64; `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md`. |
| C04 | Real disk-full (ENOSPC) | **NOT TESTED** | `PHASE_RUBIXDB_PRODUCTION_OPERATIONS_RESULTS.md` F14: injected `StorageFull` only. |
| C05 | Lifecycle baseline findings F-07, F-08, F-11, F-18 | **OPEN** | `OPEN_ITEMS.md` 2026-10-05. |
| C06 | `docs/PROJECT_STATE.md` and `missions/ACTIVE.md` exist (required reading per `CLAUDE.md`) | **PASS** | Created by this mission (D11); minimal. |
| C07 | Standalone `rubixdb-api` (unsupported for v1) | **NOT REQUIRED** | D-2. |
| C08 | Observability layer meets its own matrix (section A) | **OPEN** | Section A contains FAIL, OPEN, NOT TESTED and NOT IMPLEMENTED rows (totals in section A). |
| C09 | Certified `GET /v1/admin/status` field `wal.poisoned` is truthful under write load | **FAIL** | `api/src/routes/admin.rs:253` (`g.sync_failures() > 0`, unchanged since the certified build): true in 100 of 108 polls under 4 writers on the final binary with no write failing (`wsf_version.txt`). Pre-existing in a certified endpoint; not changed (out of scope). |

Rows: **9** - PASS 1, FAIL 3, OPEN 2, NOT REQUIRED 1, NOT TESTED 2, NOT IMPLEMENTED 0  (check: 1 + 3 + 2 + 1 + 2 + 0 = 9)

### 24.13 Verdict (2026-10-07)

* **OBSERVABILITY IMPLEMENTATION: mixed.** 63 of 83 rows PASS; 4 FAIL (A10, A32, A81, A83), 1 OPEN (A66), 9 NOT TESTED, 5 NOT REQUIRED, 1 NOT IMPLEMENTED. Every defect the maintainer approved for this mission is fixed and verified (P0 session leak, health policy, disk I/O naming, readiness unification, flush-timestamp contract, sampler start logging, state documents). The remaining FAIL rows are findings outside the approved items; they are listed, not hidden.
* **OBSERVABILITY CERTIFICATION: not PASS.** The rule "PASS for the observability scope only if every row in section A is PASS or NOT REQUIRED" is not met (4 FAIL, 1 OPEN, 9 NOT TESTED, 1 NOT IMPLEMENTED).
* **WORKSPACE REGRESSION: FAIL**, entirely PRE-EXISTING (`repo_hygiene` x2 and the intermittent `m1_3`); nothing introduced by this mission remains (B13).
* **WHOLE-PRODUCT PRODUCTION READY: NOT DECLARED** (`CLAUDE.md`: the mandatory matrix must be entirely PASS first).
