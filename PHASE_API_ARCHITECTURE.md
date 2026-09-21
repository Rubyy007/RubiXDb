# RubiXDB — Product API Architecture

**Date:** 2026-09-22

**Status:** Architecture decisions for the new service/API layer sitting
above the already-certified storage engine (Write Engine, Read Engine,
Compaction — see `PHASE_COMPACTION_CERTIFICATION.md`). Nothing in this
document proposes changing certified storage-engine semantics; every
decision below was checked against that rule.

---

## 0. Phase 0 audit findings (read-only, run before any design decision)

Verified directly against the current repository, not assumed:

- **No binary/CLI entry point exists.** `Cargo.toml` has no `[[bin]]`
  section; `src/` is a pure library crate (`pub mod compaction; error;
  execution; lsm; manifest; memtable; sstable; wal;`). No `src/bin/`
  directory.
- **No HTTP/API code exists anywhere** (`grep -r "axum\|actix\|warp\|
  hyper\|tower" src/ Cargo.toml` — zero hits).
- **No frontend code exists anywhere** — no `web/`, `frontend/`,
  `ui/`, `package.json`, or `node_modules/` in the repository. The
  `.gitignore` is a generic Visual Studio template with no Node/web
  entries. This is a greenfield addition, not an extension of an
  existing stack.
- **No `README.md`** at the repository root.
- **No existing configuration-file loader** — every example/test
  constructs `WalConfig`/`BatchCoordinatorConfig`/`LsmConfig` directly
  in Rust code; there is no env-var or TOML/YAML config surface today.
- **The engine is fully synchronous.** `grep -r "tokio\|async fn" src/
  Cargo.toml` — zero hits. Every `LsmEngine` method takes `&self` and
  performs real, blocking I/O (WAL fsync, SSTable reads) on the
  calling thread. `Arc<LsmEngine>` shared across threads is the
  established pattern (used throughout `examples/*.rs`).
- **`LsmEngine`'s actual public API** (verified via `grep -n "^\s*pub
  fn " src/lsm/mod.rs`, not assumed from memory):
  - Lifecycle: `open(dir, WalConfig, BatchCoordinatorConfig,
    LsmConfig) -> Result<Self>`, `shutdown(&self) ->
    ShutdownReportBC`. There is no separate "close" — `shutdown()`
    joins the WAL/flush/compaction threads; the caller then drops the
    `LsmEngine` value itself to release the directory's exclusive
    lock.
  - Writes: `put(&self, key: &[u8], value: &[u8]) -> Result<u64>`,
    `delete(&self, key: &[u8]) -> Result<u64>` (both return the
    assigned sequence number).
  - Reads: `get(&self, key) -> Result<GetResult>`, `get_as_of(&self,
    key, as_of_seq: u64) -> Result<GetResult>`, `contains(&self, key,
    as_of_seq: u64) -> Result<bool>`, `range(&self, start: Bound<&[u8]>,
    end: Bound<&[u8]>) -> RangeScanIter` ("now"), `range_scan(&self,
    start, end, as_of_seq: u64) -> impl Iterator<...>` (historical).
    `GetResult = Option<Vec<u8>>`.
  - Snapshots: `snapshot(&self) -> Snapshot` (RAII — releases on
    `Drop`), `Snapshot::seq(&self) -> u64`, `snapshot_seq(&self) ->
    u64` (current durable watermark, no snapshot held),
    `oldest_live_snapshot_seq(&self) -> Option<u64>`.
  - Observability: `read_stats(&self) -> ReadStats` (`read_requests`,
    `read_hits`, `read_misses`, `bloom_negatives`, `blocks_read`,
    `sstables_consulted`), `compaction_metrics(&self) ->
    CompactionMetrics` (`cycles_completed`, `input_sstables_total`,
    `input_bytes_total`, `output_bytes_total`, `records_read_total`,
    `records_retained_total`, `records_dropped_total`, `tombstones_
    dropped_total`, `versions_dropped_total`, `duration_total`,
    `duration_max`, `peak_temp_disk_bytes_max`, `last_cycle: Option
    <CompactionStats>`), `storage_state(&self) -> StorageState`
    (`Healthy`/`StoragePressure`/`StorageFull`), `storage_pressure_
    events(&self) -> u64`, `pool_stats(&self) ->
    BatchCoordinatorStats` (write-path queue/commit stats),
    `sstable_count`, `checkpoint_seq`, `recovery_stats`, `manifest_
    record_count`, `manifest_size_bytes`, `manifest_last_edit`,
    `live_sstable_ids`, `capacity_pressure_events`, `active_entry_
    count`, `immutable_count`, `immutable_total_bytes`, `active_size_
    bytes`, `next_sstable_id`, `sstables_dir`.
  - **`compact_once`/`should_compact` are `pub(crate)`, not `pub`** —
    `ADR-COMPACTION-001` Decision 13, still in force (confirmed by
    `PHASE_COMPACTION_CERTIFICATION.md`'s own Protected Dependencies
    audit). **There is no manual-compaction-trigger capability
    available to this API layer, and this document does not add
    one** — doing so would be a new public method on the certified
    engine, which per this phase's own governing instruction
    ("Do NOT modify certified storage-engine semantics unless an
    explicit new ADR approves it") is out of scope here. Compaction
    exposure in this API is **read-only** (status + metrics).
  - Test/fault-injection-only surface (`install_*_fault_hook`,
    `set_*_for_test`) exists but is explicitly **not** part of this
    product API — it is a testing seam, not a production capability.
- **No table/keyspace concept exists.** `LsmEngine` is a single flat
  binary-key → binary-value keyspace. There is no schema, no multiple
  named tables, no column concept anywhere in `src/`. This API does
  **not** invent one.
- **No SQL execution layer exists.** No parser, no query planner, no
  SQL module anywhere. This API does **not** invent SQL support.
- **`EngineError`** (`src/error.rs`) has exactly 10 variants:
  `NotFound`, `Corruption { detail }`, `Io(io::Error)`,
  `WalUnavailable { detail }`, `Unsupported { operation }`,
  `CapacityExceeded { requested, max }`, `Aborted { detail }`,
  `InvalidPath { detail, path }`, `Timeout { detail }`,
  `StorageExhausted { detail }`. The error type's own doc comment
  already establishes a security convention this API's own logging
  design inherits: *"payload contents are never logged."*
- **Toolchain**: `rustc`/`cargo` 1.98.1 — current, no MSRV constraint
  on modern async-ecosystem crates (`tokio`, `axum`).

## 1. Architecture

```
Client (frontend console, curl, any HTTP client)
   |
   |  HTTPS/HTTP, JSON over REST
   v
HTTP API  (axum router, request/response typed models, auth middleware)
   |
   v
Service layer  (validation, auth, error mapping, snapshot-handle
                registry, rate limiting, structured logging/metrics)
   |
   v
Arc<LsmEngine>  (certified, unchanged)
```

**The service layer owns**: request validation, the authentication/
authorization boundary, request↔response mapping, `EngineError`→API-
error mapping, service-level observability (request count/latency/
error count, distinct from the engine's own `ReadStats`/
`CompactionMetrics`), configuration loading, and process lifecycle
(startup/readiness/graceful shutdown). It owns **no** storage logic —
every read/write/range/snapshot/metrics call is a direct, unmodified
call into the certified `LsmEngine`, never a reimplementation.

**The certified engine remains solely responsible for**: WAL,
durability, MemTable, SSTables, Manifest, the Read Engine, Compaction,
snapshot semantics, and `StoragePressure`/`StorageFull` behavior — all
unchanged, all still governed by their own certification documents.

### 1.1 Crate/workspace structure

The repository root `Cargo.toml` becomes a Cargo **workspace** whose
members are `.` (the existing, untouched `rubixdb` engine crate) and a
new `api/` crate (`rubixdb-api`):

```toml
[workspace]
members = [".", "api"]
```

This is the **only** change to the root `Cargo.toml` — the existing
`[package]`, `[dependencies]`, `[dev-dependencies]`, `[features]`, and
`[[bench]]` sections are byte-for-byte unchanged. `rubixdb`'s own
dependency graph (`crc32c`, `xxhash-rust`) does not gain a single new
entry. This is deliberate, not incidental: it keeps the certified
engine crate's own "zero new dependency" property intact and testable
in isolation (`cargo test -p rubixdb` still builds with exactly the
same dependency set the certification audited), while letting the API
layer bring in whatever it genuinely needs (`axum`, `tokio`, `serde`,
etc.) without diluting what "the certified engine has zero new
dependencies" means. `api/Cargo.toml` depends on the engine via a path
dependency: `rubixdb = { path = ".." }`.

**Rejected alternative**: adding HTTP dependencies directly to the
root `Cargo.toml`. Rejected because it would make every future
"zero new dependency" claim about the *engine* ambiguous — a reader of
`Cargo.toml` would no longer be able to tell, at a glance, which
dependencies the certified storage engine itself needs versus which
belong to the product layer sitting above it.

## 2. API Contract

Base path: `/v1`. JSON request/response bodies throughout (`Content-
Type: application/json`), except where noted. Arbitrary binary keys/
values are represented as base64 (standard, unpadded is **not**
used — standard padded base64, `base64::engine::general_purpose::
STANDARD`) inside JSON fields, suffixed `_b64` — this project's keys
and values are declared `&[u8]` at the engine boundary, not `&str`, so
the API must not silently assume UTF-8.

| Method | Path | Purpose | Engine call |
|---|---|---|---|
| GET | `/healthz` | liveness (process is up) | none — no engine call |
| GET | `/readyz` | readiness (engine open, serving) | `storage_state()` (any state = ready; only "engine not yet open" is not-ready) |
| GET | `/v1/status` | engine status snapshot | `storage_state`, `storage_pressure_events`, `sstable_count`, `checkpoint_seq`, `manifest_record_count`, `manifest_size_bytes`, `live_sstable_ids().len()`, `capacity_pressure_events` |
| GET | `/v1/metadata` | database/keyspace metadata | static: single flat keyspace, engine config echoed back (the config the service itself opened with — the engine has no `config()` getter, so the service remembers what it passed to `open()`) |
| PUT | `/v1/kv` | put a key/value | `put(key, value)` |
| DELETE | `/v1/kv/{key_b64}` | delete a key | `delete(key)` |
| GET | `/v1/kv/{key_b64}` | get (optionally `?as_of_seq=N`) | `get(key)` or `get_as_of(key, seq)` |
| GET | `/v1/kv/{key_b64}/exists` | contains (optionally `?as_of_seq=N`) | `contains(key, seq)` (defaults `as_of_seq` to `u64::MAX`, matching every existing example's own "now" convention) |
| GET | `/v1/range` | range query — `start_b64`/`end_b64` (optional, unbounded if absent), `start_inclusive`/`end_inclusive` (bool, default `true`/`false`), `as_of_seq` (optional — "now" if absent), `limit` (default 100, max 10,000) | `range(...)` or `range_scan(..., seq)` |
| POST | `/v1/snapshots` | create a snapshot, held by the service until released | `snapshot()` |
| GET | `/v1/snapshots` | list snapshots this service instance currently holds | service-side registry (see §2.1) |
| GET | `/v1/snapshots/{id}` | inspect one held snapshot | service-side registry |
| DELETE | `/v1/snapshots/{id}` | release a held snapshot | `Drop` the held `Snapshot` |
| GET | `/v1/compaction/status` | auto-trigger enabled, trigger threshold, live SSTable count | echoed config + `sstable_count()` |
| GET | `/v1/compaction/metrics` | full `CompactionMetrics` | `compaction_metrics()` |
| GET | `/v1/metrics` | engine + service metrics (JSON) | `read_stats`, `pool_stats`, plus the status fields above, plus service-level counters (§Observability) |

**No table/keyspace listing endpoint beyond `/v1/metadata`'s own
single-keyspace description** — there is nothing to list; inventing a
multi-table endpoint would misrepresent the certified engine's actual
capability.

**No SQL/query-language endpoint** — `/v1/range` with explicit bound/
limit/seq parameters is the entire query surface, matching what
`range`/`range_scan` actually provide.

**No "force compaction" endpoint** — no engine capability exists to
call (see §0).

### 2.1 Snapshot lifecycle, stated precisely

`Snapshot` (the engine type) is a `Drop`-releasing RAII guard, not a
persistent on-disk object — it exists only in the process that created
it, and its sole effect is to keep `oldest_live_snapshot_seq()` pinned
at or below its own `seq` for as long as it's alive, protecting older
versions from Compaction. `POST /v1/snapshots` creates one and stores
it in an in-process `Mutex<HashMap<SnapshotId, Snapshot>>` owned by the
service (a `SnapshotId` is a service-generated UUID, not an engine
concept). `DELETE /v1/snapshots/{id}` removes it from the map, which
drops it, which releases it from the engine's `SnapshotRegistry`.

**Consequence, documented explicitly, not glossed over**: a service
restart clears every held snapshot (the map is empty on a fresh
process; the *engine's* own `SnapshotRegistry` also starts empty on a
fresh `open()`, since it too is in-memory only). This does **not**
affect historical reads at a specific `as_of_seq` — `get_as_of`/
`range_scan` at any seq whose data was never compacted away remains
correct with or without a live `Snapshot` object; a `Snapshot` only
*protects* a seq's data from future Compaction, it is not required to
*read* at that seq after the fact. Phase 9's own persistence-across-
restart test verifies exactly this distinction (§ `PHASE_API_
IMPLEMENTATION.md`).

## 3. Error Model

`EngineError` is mapped to a stable, typed external shape — never a
bare `Display` string as the sole signal — and the mapping is
**additive only**: nothing about `EngineError` itself changes.

```json
{
  "error": {
    "code": "NOT_FOUND",
    "message": "human-readable, safe to display",
    "detail": "optional, present only for Corruption/InvalidPath/etc."
  }
}
```

| `EngineError` variant | HTTP status | `code` | Notes |
|---|---:|---|---|
| `NotFound` | 404 | `NOT_FOUND` | Only for `get`/`get_as_of`/`DELETE` on a genuinely absent key where the engine itself distinguishes this (most read paths return `Ok(None)`, mapped to 404 by the handler, not this variant — see below) |
| `Corruption { detail }` | 500 | `CORRUPTION` | `detail` is engine-internal (e.g. "bad checksum") — safe to surface (per `error.rs`'s own doc comment: corrupted *payload bytes* are never included, only structural detail) |
| `Io(e)` | 502 | `IO_ERROR` | Generic upstream I/O failure; `e`'s `Display` is logged server-side, **not** returned in the response body (may contain OS-level path/permission detail) |
| `WalUnavailable { detail }` | 503 | `WAL_UNAVAILABLE` | Retryable |
| `Unsupported { operation }` | 400 | `UNSUPPORTED` | `operation` is safe to echo (a fixed string, e.g. a `SyncMode` name) |
| `CapacityExceeded { requested, max }` | 413 | `CAPACITY_EXCEEDED` | MemTable-freeze backpressure — echo `requested`/`max` (both plain numbers, safe) |
| `Aborted { detail }` | 500 | `ABORTED` | |
| `InvalidPath { detail, path }` | 500 | `INVALID_PATH` | `path` is a server-side configuration detail — logged, **not** returned in the body |
| `Timeout { detail }` | 504 | `TIMEOUT` | Retryable |
| `StorageExhausted { detail }` | 507 | `STORAGE_EXHAUSTED` | Maps directly to HTTP 507 Insufficient Storage — the one variant with a semantically exact HTTP status |

**`GetResult = Option<Vec<u8>>` "not found" is not an `EngineError` at
all** — `get`/`get_as_of` return `Ok(None)` for an absent key
(`EngineError::NotFound` is reserved for a different, narrower
internal case). The **handler**, not the error-mapping layer, turns
`Ok(None)` into HTTP 404 for `GET /v1/kv/{key}` — this is a request-
shape decision (§2), not a change to what the engine itself
distinguishes as "not found" vs. "absent value."

**Validation errors** (malformed base64, `limit` out of bounds, empty
key on `PUT`, etc.) are detected in the service layer **before** any
engine call and never touch `EngineError` at all — mapped directly to
HTTP 400 with `code: "VALIDATION_ERROR"`.

**Conflict/state errors**: this is a single-writer-per-key-space LSM,
not a system with optimistic-concurrency version conflicts — there is
no `EngineError` variant representing a write conflict, so none is
invented. The one state-dependent rejection the engine already makes
(`StorageExhausted`, returned *before* any WAL append once `StorageFull`
is confirmed) is exactly `EngineError::StorageExhausted` above; no
separate "conflict" HTTP code is fabricated for a condition the engine
doesn't have.

**No change to `EngineError` itself, anywhere, for HTTP-mapping
convenience** — per this phase's own explicit instruction.

## 4. API Security Foundation

**Authentication**: bearer API-key, `Authorization: Bearer <key>`.
Keys are loaded from configuration (`RUBIXDB_API_KEYS`, see §6) at
startup — never hardcoded, never committed to source control. No
external identity provider exists in this environment, so none is
integrated; the design is a single, focused `AuthProvider` trait
implemented once (`StaticApiKeyProvider`) so a future OIDC/OAuth
provider can be added later without changing the request-handling
code that consumes it.

**Authorization**: two roles, the minimum this API's own surface
actually needs — `reader` (every `GET` endpoint) and `admin` (`PUT`/
`DELETE`/`POST /v1/snapshots`/`DELETE /v1/snapshots/{id}`). Each
configured API key is associated with exactly one role. `/healthz` is
the one unauthenticated endpoint (a load balancer must be able to
probe liveness without a credential); every other endpoint, including
`/readyz`, requires a valid key.

**Request identity**: each authenticated request carries a `Principal`
(the key's configured name, not the raw key) through the service
layer, attached to its structured log line and to audit logging.

**Audit logging**: every mutating request (`PUT /v1/kv`, `DELETE /v1/
kv/{key}`, `POST /v1/snapshots`, `DELETE /v1/snapshots/{id}`) emits one
structured log line: `principal`, `method`, `path`, a `key_b64` field
**only when the key itself is short enough not to be sensitive
payload** — actually, simplest and safest, matching the engine's own
established convention (`error.rs`: "payload contents are never
logged"): log the **key's own bytes are never logged either** by
default, only a fact of "a key was written/deleted", the request's
`seq` result, outcome (success/error code), and latency. This is a
deliberate, conservative default — a database key can itself be
sensitive (e.g. a customer identifier used as a key) — not merely the
value.

**Secret handling**: API keys live in environment variables or a
gitignored local file (`.env.local`-style, **never** committed); the
architecture explicitly forbids a default/example key being valid in
a shipped default configuration (§6, §19).

**Input validation / payload limits**: empty keys rejected (400);
value size capped at a configurable `max_value_bytes` (default 1 MiB);
`/v1/range`'s `limit` capped at `max_range_limit` (default 10,000,
requested default 100); malformed base64 on any `_b64` field rejected
(400) before any engine call.

**Rate limiting**: a per-API-key token-bucket limiter (hand-rolled —
one `Mutex<HashMap<KeyId, TokenBucket>>`, no new dependency needed for
something this small), configurable requests/second and burst size per
role (`admin` and `reader` keys may be given different limits).
Exceeding the bucket returns HTTP 429 with `code: "RATE_LIMITED"`.

**CSRF**: **not applicable** to this API's actual authentication
design. CSRF is a browser-cookie-session attack; this API uses only
`Authorization: Bearer` header credentials, which a cross-site form/
script cannot attach without the credential already being available to
its own origin's JavaScript — the standard, correct reasoning for why
bearer-token APIs are not CSRF-vulnerable the way cookie-session APIs
are. If a future cookie-based browser session mode is ever added, CSRF
tokens would become required at that point, not before.

## 5. Observability

**Engine-level metrics** (never modified, only read): `ReadStats`,
`CompactionMetrics`, `storage_state`/`storage_pressure_events`, and
every `pool_stats()`/status getter listed in §0 — exposed verbatim via
`/v1/metrics` and `/v1/compaction/metrics`, not reinterpreted or
recomputed.

**Service-level metrics** (new, minimal — only what the brief actually
asks for): request count (per route, per status code), request latency
(p50/p95/p99, per route), error count (per `code`), active in-flight
request count. Held in-process (atomics + a small histogram), exposed
alongside the engine metrics in `/v1/metrics`'s JSON body under a
`service` key.

**Structured logging**: one JSON log line per request (`method`,
`path`, `status`, `latency_ms`, `principal`, request id) plus the
audit lines from §4 — no raw key/value payload content in any log line
this API emits, matching the engine's own already-established
convention.

## 6. API Lifecycle

**Startup** (deterministic, fails fast and loud on any error — never
silently degrades): load configuration (env vars: `RUBIXDB_DATA_DIR`
required, `RUBIXDB_LISTEN_ADDR` default `127.0.0.1:8080`, `RUBIXDB_
API_KEYS` required — format `name:role:key,name:role:key,...` —
`RUBIXDB_MAX_VALUE_BYTES`, `RUBIXDB_MAX_RANGE_LIMIT`, rate-limit
settings, all with sane defaults except the two `required` ones) →
`LsmEngine::open(data_dir, WalConfig::default(), BatchCoordinatorConfig
{..}, LsmConfig::default())` (any `Err` here exits the process with a
non-zero status and a clear message — the service never starts serving
against a half-open engine) → bind the TCP listener → begin serving.
Readiness (`/readyz`) only returns 200 once both the engine is open
**and** the listener is bound.

**Graceful shutdown**: on `SIGINT`/`SIGTERM` (or, on Windows, Ctrl-C —
`tokio::signal::ctrl_c()`), the server stops accepting **new**
connections and lets in-flight requests complete, bounded by a
configurable drain timeout (default 30s, matching the same order of
magnitude the engine's own `BatchCoordinatorConfig::shutdown_drain_
bound` examples already use) — then, and only then, calls `engine.
shutdown()`. This preserves the engine's own already-certified
shutdown contract exactly (`ADR-COMPACTION-001` Amendment 1 §A3: an
in-progress compaction cycle always completes, never aborted; the
flush thread's own shutdown sequence is unchanged) — the API layer
never forces the process to exit while an engine operation the
contract requires to complete is still in flight, and never calls
`shutdown()` from more than one place.

---

## 7. What this document does not decide

Deferred to `PHASE_API_IMPLEMENTATION.md` (implementation-time detail,
not architecture): exact Rust types/module layout inside `api/src/`,
the specific `axum` router wiring, the exact JSON field names beyond
what §2/§3 already fix, and test file organization.

Deferred to `PHASE_FRONTEND_ARCHITECTURE.md`: everything about the
console UI that consumes this contract.

**This API contract (§2/§3) is the source of truth the frontend is
built against — not the other way around** (per this phase's own
explicit instruction).
