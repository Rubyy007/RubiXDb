# Phase: RubiXDB GUI + Local Instance Manager — Increment Results

## 0. Scope and why this increment exists

Increment 13's mission spec assumed a local instance manager and GUI
launcher already existed, "implemented but not fully certified." A
real repository audit found neither existed at all (workspace had four
members, no `gui`/`instance` crate or subcommand, none of the
GUI/instance architecture docs the spec asked Phase 1 to read). Per
explicit user direction, this dedicated increment built both as real,
production-grade greenfield work — its own architecture/ADR/
certification documents, not folded silently into "certifying"
something that was never built. Baseline commit: `3e4a868` (Increment
12's final commit, workspace clean, `master`).

Full design records: `PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INSTANCE_SECURITY.md`, `PHASE_RUBIXDB_GUI_ARCHITECTURE.
md`. This document is the evidence record.

## 1. What was built

- **New crate `rubixdb-instance`** (workspace member): real OS-level
  instance ownership via `fs4::FileExt` (`flock`/`LockFileEx`), never a
  PID file; per-OS app-data directory resolution with path-traversal-
  proof instance naming; a persistent, non-secret manifest
  (`instance.json`); a generated, high-entropy local admin credential
  (`credentials.json`); loopback-only, collision-safe port binding with
  no TOCTOU; a real HTTP identity handshake (`GET /v1/instance`); a
  best-effort cross-platform browser launcher; the one `acquire`/
  `discover`/`list_instances` algorithm both `gui` and the plain CLI
  client use.
- **`rubixdb gui` subcommand** (`cli/src/gui.rs`, `cli/src/host.rs`):
  finds or creates the local instance, hosts the real
  `rubixdb-api` server in-process on a dedicated thread (no subprocess,
  no second SQL engine), serves the real production frontend build with
  SPA fallback, opens the default browser, blocks on Ctrl+C/SIGTERM,
  shuts down gracefully.
- **`rubixdb instance list`/`status` subcommands**
  (`cli/src/instance_cmd.rs`): real introspection over the same
  manager, used both for operator debugging and for this increment's
  own scripted certification.
- **Plain `rubixdb`/`rubixdb cli`/`-c`/`-f` client role**: unchanged
  execution semantics, gained automatic local-instance discovery
  (becomes the owner itself, headless, if no instance exists yet) so it
  is a complete first-run entry point on its own, not dependent on
  `rubixdb gui` having run first. `RUBIXDB_API_URL` remains a full,
  unchanged explicit override to a remote/manual server.
- **Two small, additive `rubixdb-api` changes**: `GET /v1/instance`
  (new, unauthenticated, additive route) and optional frontend static/
  SPA-fallback serving gated by a new `Config.frontend_dist: Option
  <PathBuf>` field (`None` by default — the pre-existing standalone-API
  deployment shape is byte-for-byte unchanged). Two other new `Option`
  fields, `instance_id`/`instance_name`, both `None` by default.

## 2. Real bugs found and fixed (in the order they were found)

1. **SPA fallback forced every client-route load to HTTP 404.**
   `ServeDir::not_found_service` (tower-http) overrides the fallback
   response's status to 404 regardless of whether a file was actually
   served — found by a test asserting 200 and getting 404. Fixed by
   switching to plain `.fallback(...)`, which preserves the real 200.
   (`api/tests/api_instance_and_frontend.rs::
   with_frontend_dist_unmatched_client_route_falls_back_to_index_html`)
2. **`lock::tests::hold_lock_forever_helper` never actually ran.**
   The first version invoked the libtest-filtered helper subprocess
   with an unqualified test name (`hold_lock_forever_helper` instead of
   `lock::tests::hold_lock_forever_helper`), so the filter matched
   nothing and the "kill the owner, confirm the lock releases" test
   passed for the wrong reason (no contention was ever created). Found
   by manually invoking the compiled test binary and observing "0
   tests" ran. Fixed by qualifying the filter path.
3. **The embedded server's listener never actually served anything.**
   `tokio::net::TcpListener::from_std` requires the socket already be
   in non-blocking mode; without it, TCP handshakes completed at the OS
   level (visible in `netstat` as `ESTABLISHED`) but the async runtime
   never polled the listener for acceptance, so every request —
   including the server's own readiness check — hung until timeout.
   Found by `netstat`/`curl` inspection of a real running process, not
   guessed from the test failure alone. Fixed with one
   `set_nonblocking(true)` call.
4. **A failed `EmbeddedServer::start()` left an orphaned server
   thread/engine/socket running.** The error-return paths after
   spawning the background thread didn't signal shutdown or join it.
   Fixed by routing every post-spawn failure through a `fail(...)`
   helper that always signals shutdown and joins before returning.
5. **The plain CLI client trusted a stale, unverified `instance.json`.**
   `resolve_connection()`'s first version used `discover()` (a pure
   disk read, no liveness check) as the primary connection path — a
   second `rubixdb -c` invocation after the first had already exited
   (lock released) tried to connect to a port nothing was listening on
   anymore instead of correctly becoming the new owner. Found by
   `data_persists_across_separate_owner_invocations` failing outright
   (the `SELECT` couldn't reach the server at all). Fixed by routing
   the client's connection resolution through `acquire()`
   unconditionally, which never trusts a manifest without either owning
   it or handshake-verifying it live.

Every one of these was found by actually running real code (a real
process, `netstat`, a failing assertion traced to its root cause), not
inferred from inspection alone — consistent with this project's own
"measure everything, flag don't silently resolve" convention.

## 3. Testing evidence

- **`rubixdb-instance` crate: 27 unit tests, real filesystem/process/
  network** (no mocking): path-traversal rejection (7 payloads),
  lock acquire/contend/release-on-`Drop`/release-on-real-process-kill
  (via a spawned, `SIGKILL`ed child, not simulated), manifest/
  credentials round-trip and corrupt-data handling, generated-key
  entropy/uniqueness, Unix file-mode verification, port-collision
  fallback, loopback-only binding, handshake against nothing
  listening, and `acquire`/`discover`/`list_instances` against a real,
  isolated filesystem root (env-override, never the developer's real
  `%LOCALAPPDATA%`).
- **`api/tests/api_instance_and_frontend.rs`: 6 tests** — `/v1/instance`
  on/off, unmatched-route 404 preserved when no frontend is configured,
  real static-asset serving, real SPA fallback, and proof the fallback
  never shadows a real protected route (401 without a credential, 200
  with one).
- **`cli/tests/gui_instance_integration.rs`: 6 tests, real compiled
  binary, real subprocesses, real races**: first-run headless
  ownership; data persistence across two entirely separate OS
  processes; two `-c` processes racing an unstarted instance to exactly
  one owner; two `gui --no-browser` processes racing, the second
  attaching rather than duplicating ownership; a `gui`-owned instance
  and a separate CLI client sharing one dataset; `instance list`/
  `status` against real, changing state.
- **Manual end-to-end smoke test** (this session, real production
  frontend build via `npm run build`, real `rubixdb gui` subprocess):
  `GET /` returned the real built `index.html`; `GET /assets/<real
  hash>.js` returned the real built asset (200); `GET /sql` (a
  client-side route, not a real file) returned the SPA shell (200);
  `GET /v1/instance` returned the real generated `instance_id`. Full
  transcript captured in this session's tool output.
- **Full pre-existing suite re-run, unmodified, after every change**:
  `rubixdb-api` 45+15+11+25 tests (Increment 12's own certified
  suites) all still passing; `rubixdb-cli`'s pre-existing 15+10 tests
  all still passing.
- **Full workspace regression** (`cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D
  warnings`, `cargo check --workspace --all-targets --all-features`,
  `cargo test --workspace`): fmt and clippy clean; core `rubixdb`
  crate 542 unit + 2 crash-consistency tests passing; the only failures
  are `group_commit.rs::m1_2_hundred_writers_throughput` and
  `m1_3_thousand_writers_throughput` — debug-mode throughput-threshold
  tests, the same class of pre-existing flake documented in Increment
  12's own results doc, confirmed unrelated by `git diff --stat -- src/
  sql/` reporting **zero lines changed** in either certified core-engine
  or SQL-crate path this increment.

## 4. Protected-path audit

```
git diff --stat -- src/wal/ src/manifest/ src/compaction/ src/sstable/ src/relational/ sql/
```
→ empty. No certified storage, SQL, or transaction code was touched.
All new/changed files: `instance/**`, `cli/src/{gui,host,frontend_dist,
instance_cmd}.rs` + `cli/src/main.rs` edits, `api/src/config.rs` (3
additive `Option` fields), `api/src/routes/{instance.rs,mod.rs}`
(1 new route + fallback wiring), `api/Cargo.toml`
(`tower-http` `fs` feature), `Cargo.toml` (workspace member), plus new
test files.

## 5. Status

| Gate | Status | Evidence |
|---|---|---|
| Instance discovery | PASS | §3 unit + integration tests |
| Instance locking (real OS lock, not PID) | PASS | kill-test, §3 |
| Instance persistence | PASS | cross-process persistence test |
| Existing-instance detection | PASS | handshake-verified `AlreadyRunning` |
| New-instance creation | PASS | `next_available_instance_name`, isolation proven |
| Startup race (CLI+CLI, GUI+GUI) | PASS | real concurrent-process tests, §3 |
| Multi-instance isolation | PASS | `two_named_instances_get_independent_ports`, separate directories proven |
| Filesystem path-traversal safety | PASS | 7-payload rejection test |
| Process security (no arbitrary kill) | PASS by construction | §`PHASE_RUBIXDB_INSTANCE_SECURITY.md` §7 — capability doesn't exist |
| Loopback-only binding | PASS | direct socket assertion, no configurable widening |
| Local credential generation/safety | PASS | entropy test, Unix mode test, never-logged |
| GUI serves real frontend build | PASS | real `npm run build` + real subprocess smoke test, §3 |
| GUI opens browser | PASS (best-effort, documented) | real `Command` launch; headless fallback message, not a crash |
| GUI + CLI shared instance | PASS | real dual-process test |
| API backward compatibility | PASS | full pre-existing suite re-run unmodified |
| Full regression (fmt/clippy/check) | PASS | clean |
| Full regression (test --workspace) | PASS except pre-existing unrelated flake | §3, zero core/SQL diff |
| GUI performance/load/endurance | NOT MEASURED | deferred to the broader Increment 13 product-hardening pass this increment's own mission explicitly scopes separately |
| Delete-object safety (database/schema/table/index confirmation) | NOT APPLICABLE | no delete-object UI exists anywhere in the frontend console (Increment 12 built an editor + results grid, not an object browser); see `PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §10 |
| Native GUI window/process security beyond the launcher | NOT APPLICABLE | no native windowed shell exists; "GUI" is the launcher + existing browser console, `PHASE_RUBIXDB_GUI_ARCHITECTURE.md` §1 |

## 6. Explicit non-claims

- Not claiming a native desktop application — `rubixdb gui` is a
  lifecycle launcher for the existing browser-based console.
- Not claiming delete-safety UI exists — it does not, anywhere in this
  product, yet.
- Not claiming GUI/instance performance, load, or endurance
  measurement in this increment — those remain open items for the
  broader Increment 13 product-hardening pass this sub-increment's own
  mission (per the user's explicit direction) scoped separately, and
  are not silently assumed passing here.
- Not claiming cross-machine or remote instance discovery — strictly
  local-machine, matching the mission's own "LOCAL MODE only" scope.

## 7. Stop condition

Per the user's explicit instruction, this dedicated GUI/Instance
increment stops here. No Router, Replication, Partitioning, or other
unrelated feature work follows automatically. The remaining Increment
13 product-hardening gaps (API/CLI/frontend performance, load,
endurance, fuzzing, crash testing, full certification matrix) are the
next scoped body of work, now against the **combined** API + CLI + GUI
+ instance-management product surface this increment added.
