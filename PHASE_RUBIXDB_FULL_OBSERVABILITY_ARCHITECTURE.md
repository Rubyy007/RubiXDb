# rubiXDb — Full Observability ARCHITECTURE (as built)

**Status of this document:** a description of the code as it exists at `HEAD 2cbd5a7e2b1891898c34c58e2adb8df57a9c3acb` (branch `master`, clean working tree at the start of the reconciliation session, 2026-10-06). It is **not** a redesign. Every statement below was read from the source files named next to it; where a statement is a measurement it points to `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md`. Nothing here changes `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` (updated only by the next prompt) and nothing supersedes the older `PHASE_RUBIXDB_OBSERVABILITY_ARCHITECTURE.md` (the `/v1/admin/status` layer, which still exists unchanged).

Scope: observability only. This document makes no WAL-performance, product-readiness or production-readiness statement.

## 1. Layers (what answers which question)

| Question | Surface | Source of the value | Auth |
|---|---|---|---|
| Is the process alive? | `GET /healthz` | constant (`api/src/routes/health.rs:21`) | none |
| Can normal work be accepted? | `GET /readyz` → `ready`, `storage_state`, `index_recovery` | `ready` is the **constant `true`** (`health.rs:45`); `storage_state` read live from the engine; `index_recovery` from `IndexRecovery` | Reader |
| Current operational state | `GET /v1/status`, `GET /v1/admin/status` | unchanged by the observability work | Reader / Admin |
| Route / engine counters | `GET /v1/metrics` | `ServiceMetrics` (per route-label count / error / 1,000-sample reservoir / lifetime max) + engine read/write stats | Reader |
| **System metrics (new)** | `GET /v1/metrics/system` | latest immutable sampler snapshot | Reader |
| **History (new)** | `GET /v1/metrics/system/timeseries?window=15m\|1h\|24h\|7d` | bounded rings | Reader |
| **Diagnostic state (new)** | `GET /v1/observability/sessions`, `/queries` | session metadata registry; query registry | Reader |
| **Events (new)** | `GET /v1/observability/events` | two in-memory rings | Reader |
| **Build identity (new)** | `GET /v1/observability/version` | compile-time constants + process start time | Reader |
| CLI | `rubixdb status --system [--json]` (`cli/src/ops_cmd.rs:275`) | `GET /v1/metrics/system` | admin key of the instance |

Role gating: `required_role` (`api/src/auth.rs:82`) maps every `GET` outside `/v1/admin/*` to `Reader`; the new endpoints are therefore Reader-visible (not Admin-only). Every route except `/healthz` and `/v1/instance` passes `auth_middleware` (`api/src/routes/mod.rs:399-406`).

## 2. Components and where they live

| Component | File | Notes |
|---|---|---|
| Module root, `Observability` struct, `ObsCounters`, lock probe | `api/src/observability/mod.rs` | `disk_low_percent()` returns the constant `10.0` (`mod.rs:108`) |
| Sampler (thread, `collect`, snapshot, state machine) | `api/src/observability/sampler.rs` | constants `TICK` 1 s (`:30`), `SIZES_EVERY` 10 s (`:32`), `STALE_AFTER_MS` 5,000 (`:34`), `DEAD_AFTER_MS` 10,000 (`:36`), `FAIL_AFTER_PANICS` 5 (`:38`), `NOT_READY_FAILS_AFTER` 30 s (`:40`) |
| OS probe seam | `api/src/observability/probe.rs`, `api/src/resources.rs` | `OsProbe` trait; `RealProbe` calls the Windows APIs `GetProcessTimes`, `K32GetProcessMemoryInfo`, `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW`, `GetProcessIoCounters`, `available_parallelism`; non-Windows branches read `/proc` (and `disk_total_free` returns `None`, `resources.rs:376-380`) |
| Time-series rings | `api/src/observability/ring.rs` | 8 series × (15 + 240 + 1,440 + 168) = 1,863 samples × 16 B, hard cap 1 MiB (`ring.rs:17-44`) |
| Query registry | `api/src/observability/queries.rs` | `RECENT_CAP` 200, `IN_FLIGHT_CAP` 2,048, `MAX_QUERIES_RETURNED` 200 |
| Event rings | `api/src/observability/events.rs` | `EVENT_RING_CAP` 256 per ring (security / operational), `MAX_EVENTS_RETURNED` 200 |
| Version | `api/src/observability/version.rs`, `api/build.rs` | revision from `git rev-parse --short=12 HEAD` at build time (+`-dirty` if `git status --porcelain --untracked-files=no` is non-empty), or `RUBIXDB_GIT_REVISION_OVERRIDE` (hex only), else absent → `null` |
| Handlers | `api/src/routes/metrics_system.rs`, `api/src/routes/observability.rs` | |
| Route metrics (closed key set) | `api/src/metrics.rs`, `api/src/routes/mod.rs` (`route_label`, `metrics_middleware`) | 128 keys + 1 overflow bucket (`metrics.rs:20-21`) |
| Counters fed from the request path | `api/src/auth.rs` (auth failures, forbidden, rate-limited), `api/src/routes/mod.rs` (`audit_middleware`: admin actions; 5xx), `api/src/sql_session.rs` + `api/src/routes/sql.rs` (sessions rejected, query registry, events) | |
| Connection gauge | `api/src/server.rs` `serve_observed` (`:92-153`) | |
| Wiring | `cli/src/host.rs:281-325` (embedded owner), `api/src/main.rs` (standalone; out of v1) | sampler started after the router exists and stopped (joined) before the engine shuts down |
| One engine-crate addition | `src/lsm/mod.rs:2267` `compaction_running()` | one relaxed atomic load; not a protected path |

## 3. The two paths

**Sampler path (one thread, `rubixdb-sampler`, 1 Hz).** `start_with` (`sampler.rs:606`) takes the first measurement **synchronously** before returning, then loops on a `Condvar` timed wait (`:633-654`; never bursts to catch up). One tick (`Worker::tick`, `:503`) runs `collect` under `catch_unwind`, publishes `Arc<Snapshot>` under an `RwLock` write held only for the pointer swap (`publish`, `:232`), then pushes the eight series values into the rings under a separate `Mutex` (`:522-526`).

`collect` (`:291`) reads: the six OS probes; engine accessors (`pool_stats()`, `compaction_metrics()`, `storage_state()`, `sstable_count()`, `immutable_count()`, `compaction_running()`); SQL/route counters; registry sizes. Every rate is a delta of a cumulative counter over measured elapsed time; the first tick has no delta and reports `null`. Directory sizes (`walk_sizes`, `:268`) are refreshed only when ≥ `SIZES_EVERY` has elapsed (`:343-350`) and carry their own measurement time.

**Request path.** `/v1/metrics/system` (`metrics_system.rs:27`) clones the `Arc` (`shared.latest()`), derives freshness from the snapshot's age on every call, builds a `serde_json::Value`, and returns. It calls no engine API, executes no SQL and takes no lock other than the `RwLock` read in `latest()`. (It does pass through `auth_middleware`, which takes the rate-limiter mutex, and `metrics_middleware`, which takes the route-table mutex when the response finishes.)

## 4. The snapshot

`Snapshot` (`sampler.rs:71`) is built completely, then published, and never mutated. Each value that depends on a platform call or on a delta is `Option<_>`; `None` is rendered JSON `null`, never `0` (`metrics_system.rs` header). Groups in the response: `timestamp_unix_ms`, `sample_freshness`, `sample_generation`, `instance`, `cpu`, `memory`, `disk`, `throughput`, `latency`, `wal`, `compaction`, `background`, `security`, `limits`, `errors`, `storage_state`, `latency_of_response_ms`. The field list is the one recorded in the certification document §3 and re-verified against `metrics_system.rs:70-162`.

Two fields are constants in the handler, not measurements: `background.last_flush_ms` is always `null` (`metrics_system.rs:139`); `sessions[].transaction_state` is always `"open"` and `cancellation_state` always `"not_requested"` (`observability.rs:46,53`).

## 5. Freshness model

* `timestamp_unix_ms` = the **response** time; `sample_freshness.last_sample_ms` = when the snapshot was taken; `age_ms` = `taken_at.elapsed()` computed per request (`metrics_system.rs:31-33`); `sample_generation` = tick counter at publication.
* `stale` = `age_ms > max(5,000 ms, 3 × tick_ms)` (`:35`). With **no** snapshot yet, `age_ms` is `null` and `stale` is `false` (`is_some_and`), and `state` is `not_started`.
* `state` = `effective_state` (`sampler.rs:181`): `not_started` if the thread was stopped; `failed` if `age_ms > max(10,000 ms, 10 × tick_ms)` even while the thread "runs"; otherwise the stored state (`running`, `degraded`).
* Stored state: `degraded` when a probe that answered before stops answering (`ever_ok`, `:316-322`), or after a contained panic; `failed` after 5 consecutive panicking ticks (`:549`); any good tick returns it to `running` (`:527-532`); a state change pushes an operational event (`:533-544`).

## 6. Health classification (the one policy-bearing computation)

Computed once per tick inside `collect` (`sampler.rs:379-406`), stable between ticks:

```
ready      = pool.coordinator_alive && !(committer sync_failures > 0 || pool.state == Failed)
failed     = not_ready for > 30 s  ||  storage_state == StorageFull  ||  instance lock probe says "not held"
degraded   = storage_state == StoragePressure  ||  volume_free < 10 % of volume_total   (strict <)
healthy    = otherwise
```

`instance.readiness` in `/v1/metrics/system` is this sampler-derived `ready` (`"ready"`/`"not_ready"`); it is **not** the constant `ready: true` of `/readyz`. The lock probe is installed by the embedded host (`cli/src/host.rs:285-292`); it calls `InstanceLock::try_acquire` on the instance directory every tick and reports the lock as held only when that returns `Err(AlreadyLocked)` — any other outcome, including `Err(Io)`, counts as "not held", and `try_acquire` creates the directory and lock file if they are missing (`instance/src/lock.rs:52-66`). The standalone binary installs no probe, so the lock-loss rule is inert there. The thresholds are not derived from repository policy (see RESULTS §5).

## 7. Bounded structures (every cap, with the file that enforces it)

| Structure | Bound | Eviction | Source |
|---|---|---|---|
| Series rings | 1,863 samples × 8 series, 1 MiB hard cap asserted at compile time | overwrite oldest slot, no allocation | `ring.rs:42-44` |
| Timeseries body | 2 MiB | drops oldest samples (`trim += 25`, max 90 %) | `metrics_system.rs:25,217-227` |
| Security events | 256 | `pop_front` (oldest) | `events.rs:111-117` |
| Operational events | 256 (separate ring) | `pop_front` | same |
| Events returned | ≤ 200, `limit` clamped 1..200, non-integer → 400 | newest first | `observability.rs:21-31`, `events.rs:119-122` |
| Finished queries | 200 | `pop_front` (oldest) | `queries.rs:249-252` |
| In-flight queries | 2,048 tracked; overflow only counted (`untracked`) | never queued | `queries.rs:139-157` |
| Queries returned | ≤ 200 | sorted by start time desc, then id desc | `queries.rs:178-192` |
| Sessions returned | ≤ 200 | sorted by session id | `sql_session.rs:269-312` |
| Route-label table | 128 + `OVERFLOW` | fold into overflow | `metrics.rs:59-63` |
| Latency reservoir | 1,000 samples per route | ring overwrite | `metrics.rs:72-77` |
| Query / event / error text | none stored; `&'static str` closed sets (`error.code()` is `&'static str`, `error.rs:89`) | — | `queries.rs:7`, `events.rs:6-8` |

## 8. Correlation identifiers

`request_id` (monotonic `u64` from `ObsCounters::next_request_id`, `mod.rs:51`), `session_id` (random UUID v4, `sql_session.rs:152`) and `query_id` (monotonic `u64`, `queries.rs:137`) appear only as **fields of diagnostic records** (events, query list, session list). No metric key, route label or series name is built from them (route labels: `routes/mod.rs:34-48`; series names: closed array `ring.rs:23-32`).

## 9. Concurrency map (locks a request or the sampler can touch)

| Lock | Taken by | Held for |
|---|---|---|
| `SamplerShared.latest` (`RwLock`) | sampler (write: pointer swap + drop of the previous `Arc<Snapshot>`), every `/v1/metrics/system` (read: `Arc` clone) | a few instructions |
| `SamplerShared.series` (`Mutex`) | sampler (push, 1 Hz), `/v1/metrics/system/timeseries` (**builds the whole JSON while holding it**, `metrics_system.rs:181-198`) | push: short; timeseries: proportional to window size |
| `ServiceMetrics.routes` (`Mutex`, one for all routes) | every authenticated request on completion (`RequestGuard::finish`), sampler once per tick (`total_requests`, `route_percentiles` clone of ≤ 1,000 floats) | short |
| `RateLimiter.buckets` (`Mutex`) | every authenticated request | short |
| `QueryRegistry.inner` (`Mutex`) | every SQL statement start / classify / finish; `/v1/observability/queries` (**sorts up to 2,248 records while holding it**, `queries.rs:178-192`) | start/finish: short; list: proportional to records |
| `SqlSessionRegistry.meta` (`Mutex`) | session create/take/put_back/finish; sampler (`open_count`); sessions endpoint | short |
| `EventLog` rings (`Mutex` ×2) | event producers; events endpoint | short |
| Engine: `lock_immutables_read`, `lock_sstables_read` (read locks), `pool.stats()` → write-queue `Mutex` (`batch_coordinator.rs:539`), `committer.stats()` → **`wal` `Mutex<FileWal>` twice** (`group_commit.rs:1144,1148`) | sampler only, once per tick | a few instructions each (the WAL module documents that this mutex is "held only for memory-speed operations", `group_commit.rs:16-19`; hold times were not measured here) |

## 10. Lifecycle

Start: `host.rs` builds the router, installs the lock probe, starts the sampler (`sampler::start`), then serves. Stop: `host.rs:326` stops and joins the sampler **before** `engine.shutdown()` (`host.rs:328`). `SamplerHandle::stop` is idempotent and also runs on `Drop`; a second concurrent `start` fails with `AlreadyRunning` (`sampler.rs:612-618`). State is per `AppState` (no statics), so two instances in one process, or two processes, share nothing. Nothing is persisted: a restart starts counters, rings and events from empty; the only durable record is the existing rotating `security.log` (default 1 MiB × (4 generations + 1), `security_log.rs:52-53`).

## 11. As-built deviations found during the reconciliation (details and evidence: `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md`)

These are facts about the code as it stands, recorded here so this document stays truthful; none is fixed by this mission.

* **Session observation map.** `SqlSessionRegistry.meta` (`sql_session.rs:113`) is created in `create`, updated in `take` / `put_back`, and removed only by `finish` (COMMIT / ROLLBACK / deadline or cancellation error) or by `shutdown`. `reap_expired` iterates `sessions`, not `meta`. A statement future that is dropped while a session's statement runs (client disconnect) calls neither `put_back` nor `finish`, so the entry stays `executing` forever, is counted by `active_sessions`, and is not counted by the per-principal cap (which counts `sessions`). Reproduced: 60 of 60.
* **`untracked_active`** (`routes/observability.rs:93`) reports `QueryRegistry::untracked()`, which only increases.
* **Two readiness definitions** (section 6): `/readyz.ready` is the constant `true`; `instance.readiness` is derived from the WAL coordinator and poison state and can be `not_ready` while `/readyz` says `true`.
* **`disk.*_iops`, `*_mb_per_sec` and the `disk_*` series** are `GetProcessIoCounters` deltas of this process; the payload does not say so.
* **Latency fields** (`latency.query_p*_ms`) are handler times of the newest 1,000 `POST /v1/sql` requests including error responses; `uptime_seconds` and `startup_timestamp_unix_ms` count from `AppState` construction (after engine recovery); `background.pending_groups` is the number of waiters pending durability.
* **Untested as built:** `errors.*`, `limits.wal_backpressure_rejections`, `limits.sql_resource_limit_hits`; the not-ready > 30 s rule; the real lock probe; five consecutive panics → `failed`; the no-snapshot response (`stale:false`, `age_ms:null`).
* **`sampler::start` failure** is discarded (`cli/src/host.rs:293`).


## 12. Closure update, 2026-10-06 (supersedes parts of sections 6, 9 and 11; append-only)

Applied maintainer decisions D1, D3, D4, D5, D6 and the P0 session-leak fix. **Superseded by this section, kept above as history:** the health classification block and the `instance.readiness` paragraph of section 6; the `disk.*` I/O field names in sections 3–4 (and the `disk_*` series names in section 7); the "As-built deviations" bullets of section 11 about the session observation map, the two readiness definitions and the swallowed sampler start failure. Everything else above still describes the code.

### 12.1 Health policy (as of 2026-10-06; the `failed` row is refined by 12.7)

`instance.healthy` is computed once per sampler tick (`api/src/observability/sampler.rs`, `collect`) and is stable between ticks:

| Value | Rule |
|---|---|
| `failed` | `instance.readiness` is `not_ready`, **or** `storage_state == StorageFull`, **or** `instance.lock_state == not_held` |
| `degraded` | `storage_state == StoragePressure` |
| `healthy` | otherwise |

Only states defined by the repository decide the verdict (`PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` for the storage states; the terminal WAL coordinator / poison states, `group_commit.rs:340`, for readiness; the instance lock). No numeric threshold is an input.

* **No grace period.** The earlier 30 s rule is gone: readiness false is `failed` at the next tick. Readiness is already visible at request time as `instance.readiness`. (The conditions that make readiness false are terminal in the engine, and the sampler is stopped before the engine shuts down, so shutdown cannot trip the rule.)
* **Lock probe is three-valued** (`api/src/observability/mod.rs`, `LockState`): `held` | `not_held` | `unavailable`, reported as `instance.lock_state`. `unavailable` (an I/O error from the probe, or no probe installed — the standalone binary) is reported as such and is **never** read as `not_held` and never makes the instance `failed`. The embedded host's probe (`cli/src/host.rs`) maps `Err(AlreadyLocked)` → `held`, acquiring the lock (and releasing it at once) → `not_held`, `Err(Io)` → `unavailable`. Limitation, unchanged: the probe cannot tell this process's hold from another process's hold, and `try_acquire` creates the lock file/directory if they are missing.
* **Free-space advisory.** `disk.free_advisory` ∈ `low` | `ok` | `unknown` and `disk.free_advisory_threshold_percent` (currently 10.0). `low` means free space on the data volume is below that percent of the volume total. It is an advisory: it never changes `instance.healthy`, and the threshold is provisional — **not a policy and not a production guarantee** (see `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` §5 for why: on the development host the rule split two volumes 0.8 percentage points apart).
* Raw inputs a client can audit: `storage_state`, `instance.readiness`, `instance.lock_state`, `disk.volume_total_bytes`, `disk.volume_free_bytes`, `disk.free_advisory`.

### 12.2 Disk I/O naming

The process's own I/O (what `GetProcessIoCounters` reports: operations and bytes the process requested) is now exposed under its own group, `process`:

| Old name | New name |
|---|---|
| `disk.read_iops` | `process.read_ops_per_sec` |
| `disk.write_iops` | `process.write_ops_per_sec` |
| `disk.read_mb_per_sec` | `process.read_mb_per_sec` |
| `disk.write_mb_per_sec` | `process.write_mb_per_sec` |
| time-series `disk_read_iops`, `disk_write_iops`, `disk_read_mb_per_sec`, `disk_write_mb_per_sec` | `process_read_ops_per_sec`, `process_write_ops_per_sec`, `process_read_mb_per_sec`, `process_write_mb_per_sec` |

The `disk` group now holds only volume capacity, free space, the advisory and directory sizes. These are **not** device activity: on an fsync-heavy write workload the data device was measured at 2.0× the process's write operations and ~177× its megabytes (`PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` §10). The `device_*` namespace is **reserved and empty in v1** (Decision D4: device-level I/O is not required for v1; `IOCTL_DISK_PERFORMANCE` is readable without elevation, 4–27 µs, so exposing it later is a policy decision, not a feasibility one). A test asserts that the old names are absent and that no `device*` key exists.

### 12.3 Readiness unification (SUPERSEDED by 12.7 on the same day: the derivation described here flapped under write load and was withdrawn; the "one definition, one source" rule it states still holds)

One definition, one function: `observability::sampler::ready_from` — the WAL batch coordinator is alive and the committer is not poisoned. The sampler evaluates it once per tick; `instance.readiness` (`"ready"` / `"not_ready"`) and `GET /readyz` `ready` (`true` / `false`) both report that value: for one sampler generation `ready == true` ⇔ `readiness == "ready"`. `/readyz` reads the latest snapshot (`sampler::is_ready`); if there is no snapshot or it is stale (age above `max(5 s, 3 ticks)`), it evaluates `ready_from` over live engine state instead, so a stale value is never presented as current. `/readyz` can therefore now be `false` (WAL coordinator stopped or committer poisoned); previously it was the constant `true`. `index_recovery` and `storage_state` in the `/readyz` body are unchanged, and a running startup index recovery still does not make the instance not-ready. The two fields have different JSON types (bool vs string) because `/readyz.ready` is an existing field whose type is kept.

### 12.4 Background operations contract

`background.last_flush_ms` is **always `null` in v1**. The engine does not currently expose a flush-completion timestamp. No engine change was made and no sampler-observed substitute was added (Decision D6: that would be a different field with a different meaning). `flush_queue_depth` (immutable memtables awaiting flush), `pending_groups` (callers waiting for durability) and `index_build_state` are measured values.

### 12.5 Session leases (P0 fix)

A statement that runs inside an explicit transaction takes the session out of the registry for its duration. `routes/sql.rs` now holds a `SessionLease` for that period: if the statement future is dropped (the client disconnected) the lease closes the session's observation entry (`SqlSessionRegistry::finish`) and leaves a `session.closed` operational event (`error_class CLIENT_DISCONNECTED`, the existing event shape). The transaction itself is rolled back by the existing `Transaction` drop path when the blocking task ends (the future's cancellation token is cancelled on drop, so the statement stops at its next cancellation check). The per-principal session cap now counts every open session of the principal — including one whose statement is running — because it counts the observation map (`SessionMeta.principal`).

### 12.6 Sampler failure logging

A sampler thread that cannot be spawned is logged once at WARN with the OS error and reads `failed` (`sampler.rs`, `report_start_failure`); a sampler whose ticks panic five times in a row is logged once at WARN on entering `failed`. Both use the existing `tracing` path; no new log target.

### 12.7 Readiness correction, Decision D5 option R2 (2026-10-06; supersedes 12.3 and the `failed` row of 12.1)

**Why.** The definition in 12.3 read `GroupCommitStats::sync_failures()`. That value is `sync_attempts - sync_successes` from two independently read atomics and is transiently 1 whenever an fsync is in flight (`PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` section 7), so under write load readiness (and with it `instance.healthy` and `/readyz`) flapped: measured with 4 writers, `/readyz.ready` was `false` in 286 of 426 polls. The maintainer chose R2.

**As built.**

* `/readyz.ready` keeps exactly its certified meaning: `true` while the engine handle is alive and answering, whatever `storage_state` says. No semantic change.
* One definition, one function: `observability::sampler::ready()` (a `const fn` returning `true`). `GET /readyz` and `instance.readiness` both call it, so they agree by construction in every sampler generation (`ready == true` <=> `readiness == "ready"`; the JSON types differ, bool vs string, because both fields pre-exist with those types).
* Nothing derived from `sync_failures()` feeds readiness or health. (`errors.wal_sync_failures` and the certified `/v1/admin/status` `sync_failures` / `poisoned` fields still read it: OPEN, see `OPEN_ITEMS.md`.)
* New additive field `instance.coordinator_state` (`alive` | `poisoned` | `not_started`), from public engine state only (`sampler::coordinator_state_from`): `poisoned` = the pool is in the terminal `PoolState::Failed` (the coordinator thread died); `alive` = the coordinator thread is alive; `not_started` = not alive and not failed (before the thread is up, or after an orderly stop). It is `null` until the first sample. **Limit:** a committer poisoned by an fsync error is not visible here, because `LsmEngine` exposes no accessor for it (`GroupCommitter::is_poisoned` is reachable only inside the engine); adding one is an engine change, not made.
* Health (`sampler::classify_health`, pure): `failed` = not ready **or** `coordinator_state == poisoned` **or** `StorageFull` **or** lock `not_held`; `degraded` = `StoragePressure`; `healthy` otherwise. The `coordinator_state == poisoned` term is how the real terminal signal the earlier code was trying to emit survives without changing `/readyz`. Because `ready()` is constant, the `!ready` term cannot fire today; it stays so a later decision (the ADR below) flows through the same classifier. `not_started`, an unavailable lock probe and the free-space advisory change nothing.

**Deferred (OPEN policy).** Extending `/readyz` to include a poisoned committer / dead coordinator is proposed in `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md` for a later lifecycle phase. It is not applied.
