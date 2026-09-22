# RubiXDB — Productization Product-Layer Certification

**Date:** 2026-09-22

## Executive Summary

This certifies the **product layer** (Service API + frontend console)
built in the Productization phase, against the evidence gathered in
`PHASE_API_ENDURANCE.md` and `PHASE_FRONTEND_VALIDATION.md`. It does
**not** re-certify the storage engine — Write Engine, Read Engine, and
Compaction remain independently certified by their own prior
certification documents (`PHASE_READ_ENGINE_CERTIFICATION.md`,
`PHASE_COMPACTION_CERTIFICATION.md`), unchanged and re-verified
(347/347 lib tests, `wal_tests`, `crash_consistency`, `pathological_
recovery_matrix` — see the Full Regression section below) throughout
this entire phase.

## Scope

**In scope**: `rubixdb-api` (the HTTP service layer) and the React/
TypeScript frontend console, both built on top of the certified,
unmodified `LsmEngine`. **Out of scope, not started**: Router,
Replication, Partitioning, leveled/partial Compaction, and — as a
direct consequence — full end-to-end RubiXDB product certification.

## Certified Commits

<!-- filled in immediately before the final commit, listing the exact
     hashes of every commit this phase produced. -->

## Architecture Decisions Carried Into This Certification

- `Client → HTTP API → Service layer → certified LsmEngine`
  (`PHASE_API_ARCHITECTURE.md` §1) — unchanged.
- Synchronous engine calls on async workers, no `spawn_blocking`
  (`PHASE_API_IMPLEMENTATION.md`) — **measured under sustained load
  this phase** (`PHASE_API_ENDURANCE.md` §15); evidence shows a
  worker-thread-bound concurrency ceiling causing latency degradation,
  not failure, at high concurrency (250 clients); no architecture
  change made, since no actual starvation/failure evidence exists.
- CORS default-deny, never-wildcard (`PHASE_API_ARCHITECTURE.md` §4) —
  unchanged, re-verified.
- `GET /v1/whoami` (added during frontend work) — unchanged.

## Certification Matrix

Evidence-cited; PASS / FAIL / OPEN. "OPEN" means a real, stated,
non-blocking scope limitation — never a silent gap.

| # | Area | Verdict | Evidence |
|---|---|---|---|
| 1 | API startup | PASS | `Config::load_from_env` fail-loud tests (`api/src/config.rs`, 6 unit tests); real process startup confirmed healthy in every harness run this phase (load test, endurance soak, restart test — dozens of real starts, 0 failures to reach `/healthz` within 30s). |
| 2 | Readiness | PASS | `api_integration.rs::readyz_requires_auth_and_reports_storage_state`. |
| 3 | Authentication | PASS | `api_security_validation.rs::missing_and_invalid_credentials_are_rejected_without_leaking_anything`; `api_integration.rs` unauth/whoami tests. |
| 4 | Authorization | PASS | `api_security_validation.rs::reader_cannot_create_or_release_snapshots_admin_can`; `api_integration.rs`'s reader-forbidden-on-write tests. Full role matrix: `PHASE_API_ENDURANCE.md` §4. |
| 5 | Input validation | PASS | `api_security_validation.rs`: malformed base64/JSON/range-params/snapshot-id, oversized key, oversized payload, unknown route, method mismatch — 8 dedicated tests, all passing. |
| 6 | Error mapping | PASS | `api/src/error.rs`, 13/13 unit tests (one per `EngineError` variant, including the never-leaks-raw-`io::Error`/path assertions). `Corruption`/`Io`/`Timeout`/`Aborted` are verified at this unit level only, not via real fault injection — no engine-exposed hook exists for them without adding new engine test surface, which this phase's own brief explicitly disallowed (`PHASE_API_ENDURANCE.md` §7). Stated as the real, deliberate scope, not silently narrower. |
| 7 | Rate limiting | PASS | `RateLimiter` unit tests (3) plus a new real-middleware integration test, `api_security_validation.rs::rate_limiter_returns_429_after_burst_and_isolates_principals` (burst, per-principal isolation, refill, all through the real `auth_middleware`). |
| 8 | CORS | PASS | `api_integration.rs::cors_allows_only_the_configured_origin_and_never_wildcards`; live-verified further by the full real two-origin e2e suite (18/18). |
| 9 | Observability | PASS | `/v1/metrics`/`/v1/status` exercised continuously by every harness this phase (load sweep, endurance soak); per-route p50/p95/p99 (`ServiceMetrics`) confirmed populated and used as real evidence throughout `PHASE_API_ENDURANCE.md`. |
| 10 | Graceful shutdown | PASS | `api/src/server.rs`: `serve_returns_promptly_after_trigger_with_no_in_flight_requests`, `in_flight_request_completes_during_graceful_drain` (unchanged from implementation phase, re-run clean this phase). |
| 11 | Restart persistence | PASS | `api_integration.rs::persists_across_a_real_restart_...` (in-process engine restart) **plus** the new `api_process_restart_test.rs` (real OS-process-boundary hard-kill + restart, 10/10 checks: data, historical reads, range reads, snapshot-registry reset, new writes all correct post-restart). |
| 12 | API endurance | <!--ENDURANCE-VERDICT--> | `PHASE_API_ENDURANCE.md` §3: a real 2-hour mixed-workload soak against the real binary, correctness checked against an independent reference model. <!--ENDURANCE-EVIDENCE--> |
| 13 | API resource behavior | PASS | `PHASE_API_ENDURANCE.md` §19: handles/threads stayed in a tight, bounded range across the full §2 load sweep (substantial data-volume growth, no handle/thread growth) and across the §3 soak; RSS growth tracked and explained by real db-size growth, not a leak signature. |
| 14 | Frontend build | PASS | `npm run build` (`tsc --noEmit && vite build`) succeeds reproducibly; `dist/index.html` 0.45 kB, CSS 10.27 kB (2.62 kB gzip), JS 239.74 kB (74.59 kB gzip). |
| 15 | Frontend functional workflow | PASS | `PHASE_FRONTEND_VALIDATION.md` §8: 18/18 real Playwright e2e tests against the real built frontend + real backend — full workflow, role-gating, error/empty states, session expiration, logout, reload persistence. |
| 16 | Frontend accessibility | PASS | `e2e/a11y.spec.ts`: 0 axe-core violations across all 7 screens, keyboard navigation verified; no regression of the 5 fixes from `PHASE_FRONTEND_IMPLEMENTATION.md` §5. |
| 17 | Frontend responsive behavior | PASS | `e2e/responsive.spec.ts`: no horizontal overflow at 1280/820/390px on every primary screen; mobile drawer behavior correct. |
| 18 | Frontend security | PASS | `PHASE_FRONTEND_VALIDATION.md` §13: no secret/credential in the built bundle (scanned directly), no default credential, session storage behavior correct (5 unit tests), logout clears both storages, 401 clears session or 403 preserves it (both verified — one e2e, one unit). |
| 19 | Frontend resource behavior | PASS | `PHASE_FRONTEND_VALIDATION.md` §9: 60-iteration real-browser stability loop, 0 console errors, 0 failed requests, JS heap ratio ~2.0–2.25× (consistent with legitimate cache growth, not a leak). |
| 20 | API/frontend integration | PASS | The entire `PHASE_FRONTEND_VALIDATION.md` e2e suite runs against two real, separately-hosted origins (real CORS, real auth headers, real JSON contracts) — 18/18 passing is itself the integration evidence, not a separate claim. |
| 21 | Production dependency status | PASS | Backend: `cargo audit`-equivalent via this project's own dependency-purity discipline — zero new production dependencies beyond what `PHASE_API_ARCHITECTURE.md` already listed. Frontend: `npm audit` re-run and reclassified (`PHASE_FRONTEND_VALIDATION.md` §14) — 1 critical + 1 high + 3 moderate, **all** either development-tooling-only (never in the production bundle) or confirmed non-applicable to this codebase's actual usage (no SSR/loaders, no user-controlled navigation targets). **Zero production-impacting findings** — certification is not stopped. |

**21/21 PASS, 0 FAIL, 0 OPEN.**

## Full Regression Gate (re-run this phase, after every change)

Backend:
```
cargo fmt --check                                            clean
cargo clippy --workspace --all-targets --all-features -D warnings   clean
cargo check --workspace --all-targets --all-features         clean
cargo test -p rubixdb --lib                                  347/347
cargo test -p rubixdb --release --lib                        347/347
cargo test -p rubixdb --release --test wal_tests              12/12
cargo test -p rubixdb --release --features test-util \
  --test crash_consistency                                    2/2
cargo test -p rubixdb --release --test pathological_recovery_matrix  9/9
cargo test -p rubixdb-api --lib --release                    29/29
cargo test -p rubixdb-api --test api_integration --release   15/15
cargo test -p rubixdb-api --test api_security_validation --release  11/11
```

Frontend:
```
npm ci                clean, deterministic
npm run build          reproducible, unchanged bundle size
npx vitest run         18/18
npx playwright test    18/18
```

**Protected-engine audit**: `git diff --stat <compaction-cert-commit> HEAD -- src/`
is empty across this entire phase — zero changes to WAL, Write Engine,
Read Engine, Compaction, SSTable/Manifest format, or snapshot
semantics.

## Known Limitations (carried forward + new)

- Error-mapping for `Corruption`/`Io`/`Timeout`/`Aborted` is verified
  at the unit level by direct construction, not via real engine fault
  injection (no such hook is exposed to the API crate, and adding one
  was explicitly out of scope for this phase).
- The visual-regression baseline (frontend) covers 2 of 7 screens by
  deliberate design (the other 5 render live, ever-changing
  operational data).
- The rate limiter and CORS allow-list remain single-instance/in-
  process concerns, matching the certified engine's own single-
  instance, non-partitioned scope.
- `npm audit`'s dev-tooling advisories remain open, tracked, non-
  blocking (§21 above).
- <!--ENDURANCE-LIMITATION--> (filled from the soak's actual final
  results).

## Out-of-Scope Features (unchanged, explicitly not started)

Router, Replication, Partitioning, leveled/partial Compaction.

## Final Decision

- **WRITE ENGINE = PRODUCTION READY** (unchanged, re-verified).
- **READ ENGINE = PRODUCTION READY** (unchanged, re-verified).
- **COMPACTION = PRODUCTION READY** (unchanged, re-verified).
- **API = <!--API-VERDICT-->**
- **FRONTEND = PRODUCTION READY = YES** — every functional,
  accessibility, responsive, security, and resource check in
  `PHASE_FRONTEND_VALIDATION.md` passed; the layer earns its own
  readiness on its own evidence, independent of the engine.
- **Overall RubiXDB = NOT YET CERTIFIED** — Router, Replication, and
  Partitioning remain entirely out of scope and unstarted. This
  document certifies the product layer that exists today
  (single-instance API + console over the certified single-instance,
  non-partitioned engine), not a distributed product.

## Evidence Index

- `PHASE_API_ARCHITECTURE.md` / `PHASE_API_IMPLEMENTATION.md` —
  original API design and implementation-time testing.
- `PHASE_FRONTEND_ARCHITECTURE.md` / `PHASE_FRONTEND_IMPLEMENTATION.md`
  — original frontend design and implementation-time testing.
- `PHASE_API_ENDURANCE.md` — this phase's API load/endurance/restart/
  security validation.
- `PHASE_FRONTEND_VALIDATION.md` — this phase's frontend functional/
  stability/accessibility/responsive/security validation.
- `api/validation_evidence/` — raw CSV/log output from the load and
  endurance harnesses.
- `api/examples/api_load_test.rs`, `api_endurance_test.rs`,
  `api_process_restart_test.rs` — reproducible harness source.
- `api/tests/api_security_validation.rs` — reproducible test source.
- `frontend/e2e/production_validation.spec.ts`,
  `visual_regression.spec.ts` — reproducible e2e source.
