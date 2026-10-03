# PHASE RUBIXDB — FINAL SINGLE-NODE RELEASE ENGINEERING

**Date:** 2026-10-04 · Evidence: `PHASE_RUBIXDB_PRODUCTION_OPERATIONS_RESULTS.md` §7–§9, `scripts/release.ps1`, `scripts/ops/upgrade_test.py`, `scripts/ops/fault_campaign.py`, `src/ops/format.rs`, `src/ops/physical_tests.rs`.

## 1. Versions and compatibility (checked in source)
| Format | Where it is versioned | Value | A reader of another version… |
|---|---|---|---|
| Product | `Cargo.toml` `version` (workspace crates all 0.1.0); `rubixdb --version`; `VERSION` file in the release package | 0.1.0 | – |
| Data directory | `DATA_FORMAT` marker (`rubixdb-data-format=N`), written when a product entry point **creates** a directory (`ops::format`) | 1 | **refuses** with `UNSUPPORTED_DATA_FORMAT`, directory untouched (tested: byte-identical hash) |
| WAL segment | magic `RBXWALv1` + `format_version` in every segment header (`wal::format`) | 1 | refuses (`decode_segment_header`) — **but see engine finding A below** |
| SSTable | footer magic `RBXSST01` + `format_version` | 1 | `EngineError::Unsupported`; open fails; directory untouched (tested) |
| MANIFEST | **none** — frames only (`manifest::format`) | – | fail-closed only by unknown edit types / checksums. **Engine finding B** |
| Catalog / table rows | `ROW_FORMAT_VERSION` byte in every row envelope; `schema_version` per row | 1 | decoder rejects unknown versions (`RowValue format_version`) |
| Backup | preamble + header `format_version`, header CRC | 1 | `BACKUP_UNSUPPORTED_VERSION`, nothing read |
| Instance identity | `instance.json` (`instance_id`, `name`, `api_port`, creation time) — unversioned, strict serde; a parse failure fails closed | – | – |
| Configuration | environment variables (`RUBIXDB_*`), validated before anything is opened (below) | – | – |

**Engine findings recorded, not fixed (engine boundary — `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`, ADR-ENG-OPS-001):**
* **A — `LsmEngine::open` ignores corrupt WAL segments** (replay summary discarded) although the WAL layer requires the caller to halt. Reproduced; mitigated at every product entry point by `ops::format::startup_guard` (read-only WAL preflight before open; refusal leaves the directory byte-identical). *Not* mitigated for anyone opening the library directly.
* **B — manifest has no format version.** Mitigated by the directory-level marker.

## 2. Startup compatibility rules (implemented in `startup_guard`, applied by `rubixdb-api`, `rubixdb gui`/CLI host, restore, offline check)
1. Directory absent or empty → fresh; marker written after a successful open.
2. Marker present and supported → proceed.
3. Marker unsupported/garbled → **refuse, change nothing**.
4. No marker, non-empty directory → *legacy* (created by an earlier release): accepted **and left unmodified** (no marker is added — "never silently migrate").
5. WAL preflight finds corrupted segment(s) or an unreadable WAL → **refuse, change nothing**.
6. Then the engine's own checks (SSTable footer versions, manifest replay, missing-file detection) — tested: a missing live SSTable refuses to start and leaves the directory unchanged.
Never done on incompatible data: overwrite, truncate, reinitialise, silently migrate.

## 3. Configuration validation (tested, `fault_campaign.py` F12/F12b + `api/src/config.rs` unit tests)
`Config::load_from_env` runs first and exits non-zero with a message for: missing/short/malformed API keys, unknown role, bad listen address, non-numeric limits. In every such case an existing data directory is **byte-identical afterwards** and a non-existent one is **not created** (the engine opens only after configuration succeeds). Remaining ordering caveat (stated): if the *bind* fails after the engine opened, normal crash recovery has already run — that is ordinary startup work, not corruption.

## 4. Upgrade, downgrade
* **Upgrade (tested with real persistent data, two previous builds — Increment 18 `7b7aaaa` and Increment 17 `bc80c79`):** old directories (killed mid-write, real WAL tails, several SSTables, indexes, transactions) open with the current binary, all rows equal the independent model, indexes serve correct reads, integrity check clean, backup→restore of the upgraded database equal. **SUPPORTED** for the two tested predecessors; earlier builds are not claimed.
* **Downgrade:** older builds opened the directory the current build wrote (including a marked fresh directory) and read every row — because **no on-disk format version changed in this release**. That is an observation, not a guarantee: older builds do not read the marker, have no restore, and cannot protect themselves from a future format. **Downgrade policy: UNSUPPORTED (best-effort)**. The supported way to move data between builds is *backup → restore into the newer build's new instance*.
* Rule for future releases: any change to a WAL/SSTable/catalog/backup format bumps that format's version **and** `CURRENT_DATA_FORMAT`, ships a migration or a documented "restore from backup" path, and adds an upgrade test from the previous release before it ships.

## 5. Reproducible release procedure (`scripts/release.ps1`, run end to end: exit 0)
1. `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -D warnings`.
2. (default) `cargo test --release --workspace --no-fail-fast`; the WAL throughput gate is `scripts/wal_certify.ps1`.
3. `npm ci` + `npm run build` (typecheck + production bundle).
4. `cargo build --release --locked` with `RUSTFLAGS="-C link-arg=/Brepro --remap-path-prefix=<repo>=/src --remap-path-prefix=<cargo home>=/cargo"` — **two independent clean builds produced identical SHA-256** for `rubixdb.exe` and `rubixdb-api.exe`.
5. Package `rubixdb-<version>/` = `rubixdb.exe`, `rubixdb-api.exe`, `frontend-dist/` (the layout `cli/src/frontend_dist.rs` searches), `VERSION` (version, commit, `-dirty` flag, toolchain, format versions), `SHA256SUMS`. Refuses to overwrite an existing release directory.
6. Smoke test of the **packaged** copy in a fresh instances root: `--version`; headless instance started; healthz; DDL; DML; query; `backup create`; `backup verify`; `check`; the frontend page served from the package; `instance stop` (clean exit).
Defaults verified: instance directory `<instances root>/<name>/{instance.json, credentials.json, data/, backups/}`, default port 302 (falls back to a free port and records it), loopback only, logging: `rubixdb gui` writes status lines to stderr, `rubixdb-api` writes JSON `tracing` logs to stdout (the product itself writes no log files; the test harness redirects them), credentials generated per instance and never in backups.
`cargo audit`: 0 vulnerabilities, 0 warnings (a yanked transitive `yoke-derive 0.8.3` was updated to 0.8.4 in `Cargo.lock`). `cargo deny --workspace --all-features check` with `deny.toml` (permissive licences only; `r-efi`'s `LGPL-2.1-or-later` is one option of an OR expression and is satisfied through MIT): **advisories ok, bans ok, licenses ok, sources ok**.
