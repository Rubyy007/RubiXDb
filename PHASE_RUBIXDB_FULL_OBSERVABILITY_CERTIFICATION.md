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
