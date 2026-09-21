# RubiXDB — Product API Implementation

**Date:** 2026-09-22

**Status:** Implements `PHASE_API_ARCHITECTURE.md` in full. Builds on
the already-certified storage engine (`PHASE_COMPACTION_
CERTIFICATION.md`) — no engine semantics were changed (§ Protected
Engine Audit below).

---

## 1. Crate layout

```
api/
  Cargo.toml          rubixdb-api — depends on rubixdb via a path dependency
  src/
    main.rs            entry point: config load -> engine open -> serve -> shutdown
    lib.rs              module declarations, re-exports for tests
    config.rs           Config/Role/ApiKeyConfig, env-var loading (§6 of the arch doc)
    state.rs             AppState (Arc-shared across every handler)
    auth.rs               AuthProvider, Principal, auth_middleware
    rate_limit.rs          per-principal token-bucket limiter
    error.rs                ApiError, EngineError -> HTTP mapping
    encoding.rs               base64 key/value helpers
    metrics.rs                 service-level request metrics (count/latency/errors)
    server.rs                   bounded graceful-shutdown serving loop (testable in isolation)
    routes/
      mod.rs                     router assembly, auth + metrics middleware wiring
      health.rs, status.rs, kv.rs, range.rs, snapshots.rs, compaction.rs, metrics_route.rs
  tests/
    api_integration.rs   Phase 8/9 — real engine, real router, no mocks
```

**Workspace**: the repository root `Cargo.toml` gained one `[workspace]`
section (`members = [".", "api"]`) — its own `[package]`/
`[dependencies]` are otherwise byte-for-byte unchanged (verified:
`cargo tree -p rubixdb -e normal` still shows exactly `crc32c` +
`xxhash-rust`, nothing added). `Cargo.lock` is now workspace-wide (it
necessarily grew to include the API layer's own dependency graph) —
this is expected and does not change what the *engine crate itself*
depends on.

## 2. What the architecture doc left to implementation-time judgment

- **HTTP framework**: `axum` 0.7 + `tokio` 1 (multi-threaded runtime).
  The engine itself is fully synchronous; every handler calls
  `LsmEngine` methods directly on the async worker thread (the engine's
  own internal locking is what makes this safe under concurrent axum
  handlers — the same `Arc`-shared pattern every `examples/*.rs`
  harness already uses). No `spawn_blocking` wrapper was added: every
  measured handler call in this session's own testing completed in
  well under a millisecond for the operations exercised (see `/v1/
  metrics`'s own `service.routes[].p50_ms` figures during manual
  testing) — a future, separately-scoped increment could revisit this
  if a genuinely slow engine call (e.g. an extremely large range under
  contention) is found to block the async runtime's worker pool
  noticeably; not attempted here as a speculative optimization.
- **Body-size double-check, found empirically, fixed at the source**:
  axum's `Json` extractor enforces its own independent default body
  limit (2 MiB) *before* any handler runs. Left alone, a deployment
  configuring `max_value_bytes` above that default would see requests
  rejected by an undocumented framework limit instead of this
  service's own `VALIDATION_ERROR` response. Found by this crate's own
  `oversized_value_is_rejected` integration test (an early version
  picked a value large enough to trip *both* limits, silently never
  reaching the handler's own check at all) — fixed by computing an
  explicit `DefaultBodyLimit` from `max_value_bytes`/`max_key_bytes`
  at router-build time (`routes::body_size_limit`), so the two limits
  are consistent and the service's own check is what a client actually
  sees.
- **Route metrics labeling**: `axum::extract::MatchedPath`, available
  to `route_layer`-registered middleware (confirmed empirically via
  live testing, not assumed from documentation alone — the `/v1/
  metrics` smoke-test output showed correctly templated labels like
  `GET /v1/kv/:key_b64`, not one row per distinct key value).
- **Graceful shutdown, extracted for testability**: `server::serve`
  takes an injectable shutdown-trigger future rather than hardcoding
  `tokio::signal::ctrl_c()`, specifically because real `SIGINT`
  delivery to a detached Windows console process from a non-attached
  shell was found, empirically, to be unreliable in this development
  environment (`kill -INT` from Git Bash did not trigger the handler;
  `Stop-Process -Force` was needed to actually end the process) — a
  tooling/platform limitation, not a defect in the shutdown logic
  itself, and exactly the kind of environment quirk this project's own
  standing discipline says to trace and document rather than paper
  over. `main.rs` passes the real signal future; `server::tests` pass
  a programmatic one, exercising the identical drain/bound logic
  either way.
- **Snapshot registry**: `Mutex<HashMap<Uuid, HeldSnapshot>>` on
  `AppState`, per `PHASE_API_ARCHITECTURE.md` §2.1 exactly as
  specified — confirmed empirically (both manual `curl` testing and
  the `snapshot_lifecycle_protects_historical_reads` integration test)
  that release-then-double-release correctly 404s, and that a
  restarted service starts with an empty registry while historical
  reads at a pre-restart seq remain correct regardless.

## 3. Manual end-to-end verification (real server, real `curl`)

Before writing the automated integration suite, the running service
was exercised directly over HTTP against a real on-disk directory:
`/healthz` (no auth), `/readyz`/`/v1/status`/`/v1/metadata` (auth
required, 401 without), `PUT`/`GET`/`DELETE`/`exists` (including a
reader-role key correctly receiving 403 on `PUT`), `/v1/range`
(ordering, `limit`, `truncated`), snapshot create → historical read at
the pinned seq (correctly showing the pre-delete value) → list →
release → list-now-empty, `/v1/compaction/status`+`/metrics`, `/v1/
metrics` (real per-route p50/p95/p99 from `MatchedPath`-labeled
middleware), malformed-base64 → 400, wrong API key → 401. Every
response matched the architecture doc's own contract exactly.

## 4. Automated test suite

**Unit tests** (`cargo test -p rubixdb-api --lib`, 29/29): config
parsing/validation, auth key lookup, base64 round-trip, rate-limiter
token-bucket behavior (burst exhaustion, refill, per-principal
independence), service-metrics accounting, and — per this phase's own
explicit "keep unit tests where appropriate for request validation and
error mapping" instruction — one test per `EngineError` variant's
exact HTTP status/code mapping (13 tests), including two tests that
specifically assert a raw `io::Error` and a filesystem `path` are
**never** present in the response body (only logged server-side),
matching §3 of the architecture doc precisely. Two `server::` tests
exercise the bounded graceful-shutdown loop directly: prompt return
with no in-flight requests, and a real, already-in-flight `reqwest`
request (over a warmed-up, reused connection — an earlier version of
this test raced a brand-new connection attempt against the trigger
and was legitimately reset, a different and less interesting race;
fixed by warming the connection pool first) completing successfully
despite the shutdown trigger firing mid-request.

**Integration tests** (`cargo test -p rubixdb-api --test
api_integration`, 13/13) — every one drives the real `LsmEngine`
through the real router via `tower::ServiceExt::oneshot`, no mocking:
health/readiness, full put→get→delete flow with role enforcement,
`get_as_of` historical resolution after a later delete, `contains`,
range ordering/truncation/limit validation, full snapshot lifecycle
(create → historical read while a later write already landed → list →
release → double-release 404s), compaction status/metrics against a
freshly-opened engine, malformed-base64/empty-key/oversized-value
validation, `StorageFull` mapping to 507 with automatic recovery to
200 once healthy again (via the engine's own `set_storage_state_for_
test` — the same test-only hook the engine's own certified suite
uses, not a mock of engine behavior), and — the Phase 9 requirement
— a full real-restart test: writes, a delete, and a two-version key
against run 1, a real `engine.shutdown()` + `drop`, a fresh `LsmEngine::
open` against the **same directory** for run 2, then `get`/`get_as_
of`/`contains`/`range` all re-verified against the persisted state,
including confirming the snapshot registry is correctly empty again
post-restart while the historical `as_of_seq` read still resolves
correctly (exactly the distinction `PHASE_API_ARCHITECTURE.md` §2.1
draws between "a live `Snapshot` object" and "historical data survives
regardless").

## 5. Protected Engine Audit

```
git status --short -- src/ Cargo.toml Cargo.lock
git diff --stat -- src/
```

`src/` (the certified engine's own source): **zero changes**. Only
`Cargo.toml` (the additive `[workspace]` section) and `Cargo.lock`
(now workspace-wide) changed at the repository root. `cargo test -p
rubixdb --lib`: **347/347**, unchanged from `PHASE_COMPACTION_
CERTIFICATION.md`'s own final regression gate — confirming this
phase's own work introduced zero engine regression. No WAL, Manifest,
Read Engine, Compaction, or Snapshot semantic was touched; the API
layer calls only the certified public surface `PHASE_API_ARCHITECTURE.
md` §0 already enumerated.

## 6. Running it

```
RUBIXDB_DATA_DIR=/path/to/data \
RUBIXDB_API_KEYS="admin-svc:admin:<32+ char secret>,ui-svc:reader:<32+ char secret>" \
RUBIXDB_LISTEN_ADDR=127.0.0.1:8080 \
cargo run --release -p rubixdb-api
```

`RUBIXDB_DATA_DIR` and `RUBIXDB_API_KEYS` are the only required
settings; every other `RUBIXDB_*` variable listed in
`PHASE_API_ARCHITECTURE.md` §6 has a documented default. Startup
fails loudly (non-zero exit, no partial serving) on any missing/
invalid required setting or a failed `LsmEngine::open`.

## 7. Known limitations (this layer's own, not the engine's)

- No external identity provider is integrated (none exists in this
  environment — `PHASE_API_ARCHITECTURE.md` §4's own stated scope).
- The rate limiter is in-process/single-instance (a multi-instance
  deployment behind a load balancer would need a shared store — out of
  scope; this phase is a single-process service, matching the
  certified engine's own single-instance, non-partitioned scope).
- No manual "force compaction" endpoint — no such engine capability
  exists to call (§0/§2 of the architecture doc).
- Handlers call the synchronous engine directly on the async worker
  thread rather than via `spawn_blocking` (§2 above) — not observed to
  be a problem in this phase's own testing, flagged for future
  attention if a slow-call scenario is ever found.

This API layer has **not** undergone its own separate long-duration/
load-endurance validation the way the storage engine did
(`PHASE_COMPACTION_INCREMENT3_ENDURANCE.md`) — that is explicitly out
of this phase's scope per its own governing instruction ("The new API/
UI layer must earn its own production readiness through its own
tests," not by inheriting the engine's).
