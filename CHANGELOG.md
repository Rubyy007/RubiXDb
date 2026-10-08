# Changelog

All notable changes to RubixDB are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project has not cut a
release yet, so everything so far lives under `[Unreleased]`.

## [Unreleased]

### F-08: a damaged SSTable is found before any file is changed, and reported while it is serving (ADR-SST-01, 2026-10-08)

**Added**
- **SSTable preflight at start.** The startup-only guard (`rubixdb gui`, `rubixdb-api`) now checks, read-only and before the F-07 attestation is consumed, that every Manifest-live table exists, has the size and sequence range the Manifest recorded and opens with the engine's own validation (footer, bloom, index); every other `*.sst` the engine would adopt must open. A failure refuses the start with exit 1 and `SSTABLE_CORRUPT`, `SSTABLE_MISSING` or `SSTABLE_MISMATCH` (the engine's text and the file named), leaves the directory unmodified and has no override. A valid file that is not the one the Manifest recorded (previously accepted, serving a missing catalog) is now refused.
- **Background data-block verification.** After the server is serving, one throttled, cancellable thread reads every data block of the tables live at start. `GET /readyz` gains `sstable_verification` (`disabled` | `running` | `complete` | `damaged`); `GET /v1/status` gains `sstable_integrity` (`state`, `tables_total`, `tables_verified`, `bytes_verified`, `damaged: [{id, path, records_before_failure, records_total}]`); a damaged table writes the security event `sstable.damaged` and one stderr line. `ready` is unchanged (constant true). Reads of the damaged block still fail with the typed corruption error.
- **`RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC`**: digits only; unset or empty = 64 (an unvalidated default: its effect on foreground latency is not measured); `0` disables the pass; at most 1024; anything else stops startup naming the variable.

**Fixed**
- **A refused start no longer consumes `WAL_CLEAN_STOP`.** Before, any SSTable refusal had already removed the F-07 attestation, so after the operator repaired the table a damaged WAL tail was silently truncated (one acknowledged record lost in the reproducer); now the next start is still attested and refuses with `WAL_TAIL_DAMAGED`.

**Changed**
- The refusal wording for a damaged table is now `engine open refused: <CODE>: <engine text>; refusing to open: ... The directory has not been modified.` (was `engine open failed: <engine text>`). A truncated table is reported as `SSTABLE_MISMATCH` (its length no longer matches the Manifest). When a damaged table and a damaged WAL tail coexist, the SSTable refusal is reported first.

### F-08: a failing compaction no longer leaks temp files, and says so (ADR-COMPACTION-LEAK-01, 2026-10-08)

**Fixed**
- **Partial compaction output is removed.** `write_from_sorted_records` removes its own `<id>.sst.tmp` on every failure path (guard disarmed after the rename). Before, one ~2 MB file was leaked every ~5 s while a compaction input was damaged (24 files / 47.8 MB after 120 s); nothing but the process restart removed them. The flush path shares the writer and gets the same cleanup (not separately tested).

**Added**
- **Compaction failures are classified, counted and shown.** `state` (`idle`/`running`/`failing`/`blocked`), `failures_total`, `consecutive_failures`, `blocked` and `last_failure {at_unix_ms, kind, message}` on `/v1/compaction/status`, `/v1/compaction/metrics`, `/v1/admin/status.compaction` and `/v1/metrics/system.compaction` (additive). Security events `compaction.failing` and `compaction.blocked` on state change only.

**Changed**
- **The worker stops retrying what cannot succeed.** After `max_flush_retries` (3) consecutive permanent-class failures (Corruption, Unsupported, panic) the compaction worker is `blocked` until the process restarts; transient failures (I/O incl. disk full) never block. `StorageState` and `/readyz` are unchanged. The stderr line now reads `compaction: failed (attempt k of 3), will retry: ...`, then `compaction: blocked after 3 consecutive permanent failures (...); restore the damaged table and restart`; it still names no table.

### F-07: a damaged WAL tail after a clean stop is no longer opened silently (ADR-WAL-01, 2026-10-08)

**Added**
- **`WAL_CLEAN_STOP` attestation.** A graceful `rubixdb gui` shutdown records where the WAL ended (segment, length, last sequence, CRC32C) after the engine has fully stopped. At the next start, if the WAL (or the manifest checkpoint) no longer reaches that sequence, the start is refused with `WAL_TAIL_DAMAGED` (exit 1; names the segment, both sequence numbers and the exact number of missing acknowledged records; the directory is not modified).
- **Tail quarantine.** Whenever recovery will truncate a torn tail, the exact removed bytes are first preserved in `<data_dir>/wal-quarantine/wal-<segment>.<offset>.<unix_ms>.tail` (header + bytes, CRC32C, temp file + fsync + atomic rename) and reported on stderr, in the security log (`wal.tail_quarantined`) and by the stopped `rubixdb check` (informational `WAL_TAIL_QUARANTINED`). If the bytes cannot be preserved the start is refused (`WAL_TAIL_QUARANTINE_FAILED`).
- **`RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL=1`** (unset or empty = off; any other value stops startup naming the variable): accepts the loss named by `WAL_TAIL_DAMAGED` (or an unwritable quarantine). It never bypasses `WAL_CORRUPT`. `wal.tail_override` is logged only when it changes an outcome.

**Changed**
- `rubixdb check` knows `WAL_CLEAN_STOP` and `wal-quarantine/` (no longer `UNEXPECTED_FILE`). `WAL_TORN_TAIL` is still a warning; no exit code changes.
- Graceful shutdown now performs one extra read-only replay of the stopped WAL and a small fsynced file write (about +70 ms for a 20,000-row WAL; proportional to the number of records retained in the WAL). Start-up time is unchanged.

**Not changed**
- Anything under `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/error.rs`; the WAL format and the engine's recovery classification and truncation; `ops::open` (so `rubixdb check` and `restore` behave as before); every existing `WAL_CORRUPT` refusal; `Cargo.toml`, `Cargo.lock`.

**Limits**
- After a kill or power loss there is no attestation: a damaged acknowledged tail is still truncated (now quarantined and reported). Power loss is NOT TESTED. The standalone `rubixdb-api` binary writes no attestation.

### Item C: operator-settable cap on the blocking thread pool (2026-10-07)

Added `RUBIXDB_LOCAL_MAX_BLOCKING_THREADS` (integer 16..=512; unset or empty = 512, tokio's default, i.e. the behaviour before this change). It bounds the thread pool every SQL statement runs on (`spawn_blocking`) in the embedded host (`rubixdb gui`); a bad value stops startup and names the variable. Measured (`PHASE_ITEM_C_DISCOVERY.md`, `PHASE_ITEM_C_CERTIFICATION.md`): with 16 clients connecting inside the timed window the server created 165-530 threads in the first fraction of a second and ran at 24.0-29.2k req/s; capped at 16 or 64 the thread count is exactly 18 + cap, throughput 29.5-30.6k req/s and p99 1.14-1.24 ms; warm reads and writes are unchanged. **The default is not changed**, so an operator who sets nothing sees the previous behaviour. ADR-ITEM-C-01. Not changed: `api/`, timeouts, cancellation, sessions, snapshot semantics, error shapes, response schemas, engine (`src/`), `Cargo.toml`, `Cargo.lock`.

### Observability coverage-gap closure (2026-10-07)

Tests only: three observability rows that were NOT TESTED for want of a test are now covered (`errors.*`, `limits.*` and `storage_state` of `/v1/metrics/system`; the freshness response before any snapshot exists; five consecutive sampler panics driving the state to `failed` and back to `running`, in the new test file `api/tests/observability_sampler_failure.rs`). No production code changed. Section A of the certification is 72 PASS, 0 FAIL, 0 OPEN, 6 NOT REQUIRED, 7 NOT TESTED (85 rows); two value-trigger cases stay NOT TESTED inside row A12. See certification section 28.

### Observability final closure (2026-10-07)

Observability closure completed: section A of the certification is 69 PASS, 0 FAIL, 0 OPEN, 6 NOT REQUIRED, 10 NOT TESTED (85 rows; A66 reclassified to NOT TESTED - ENVIRONMENT LIMITATION by the maintainer), so the observability implementation status is PASS with every NOT REQUIRED and NOT TESTED row enumerated (section 27). The workspace regression status is FAIL with pre-existing failures only, and whole-product readiness is not declared. No code changed in this step.

### Observability follow-up 2 (2026-10-07)

Full record: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 26, `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md` (now ACCEPTED), `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_03.md` (scope extended). Observability-scoped; protected engine paths unchanged.

#### Changed

- `GET /v1/metrics/system` `instance.coordinator_state` is `poisoned`, and `instance.healthy` is `failed`, when the WAL group committer is poisoned by a failed fsync or a leader panic (or the coordinator thread is dead). `GET /readyz` and `instance.readiness` are unchanged (constant `ready: true`).
- `GET /v1/admin/status` `wal.sync_failures` is `null` (key kept): the engine's only counter is racy (ADR-OBS-03). The CLI inspection text prints `sync_failures=-`; the GUI Operations page shows `-`.

#### Documentation

- Section 26 of the observability certification: classification of the non-reproducible build hashes, the final wording of the D8 handle criterion, the read-p95 and idle-CPU outliers with their interpretation, the OPEN rows of section A by name.

### Observability follow-up (2026-10-07)

Full record: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 25, `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_02.md`, `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_03.md`. Observability-scoped; nothing under `src/wal/`, `src/manifest/`, `src/sstable/`, `src/compaction/` or `src/error.rs` changed.

#### Fixed

- `GET /v1/admin/status` `wal.poisoned` no longer reads `true` while an fsync is merely in flight: it is `GroupCommitter::is_poisoned()` (read-only pass-through added to `BatchCoordinatorPool` and `LsmEngine`) or a failed coordinator. Real-process polls under 4 writers: 124 of 147 `true` before, 0 of 142 after. Field name, type and meaning unchanged.
- `GET /v1/observability/queries` `untracked_active` is a gauge of statements running now that the in-flight table could not track, not a lifetime total.
- `git_revision` of a binary built from a modified tree now ends in `-dirty` reliably: `api/build.rs` re-runs when HEAD, the ref, the index or any tracked file changes.

#### Changed

- `GET /v1/metrics/system` `errors.wal_sync_failures` is always `null` (key kept): the engine's only counter for it is racy (ADR-OBS-03).

#### Documentation

- Section 25 of the observability certification: binary identity statement (which binary produced which evidence, with hashes), clean-rebuild overhead re-measurement, amended D8 handle criterion, 30-minute RSS classification (engine, not observability), per-row classification of the section B failures, updated matrices.

### Observability closure (2026-10-07)

Full record: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` section 24, `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` sections 16-17, `PHASE_RUBIXDB_FULL_OBSERVABILITY_ADR_01.md`.

#### Fixed

- A client disconnect while a statement runs inside an explicit transaction no longer leaves a permanent session entry (`active_sessions` is truthful, the per-principal cap counts executing sessions, a `session.closed` event is recorded).

#### Changed (`GET /v1/metrics/system`; the five older endpoints are byte-for-byte compatible in the black-box inventory)

- Renamed: `disk.read_iops`, `disk.write_iops`, `disk.read_mb_per_sec`, `disk.write_mb_per_sec` -> `process.read_ops_per_sec`, `process.write_ops_per_sec`, `process.read_mb_per_sec`, `process.write_mb_per_sec` (the process's own I/O, not device activity); time-series names likewise (`process_*`).
- `instance.healthy` follows repository-defined states only (`failed`: coordinator poisoned / `StorageFull` / lock not held; `degraded`: `StoragePressure`); the 10 % free-space rule is now the advisory `disk.free_advisory`; the 30 s grace is gone.

#### Added

- `instance.lock_state` (`held` | `not_held` | `unavailable`), `instance.coordinator_state` (`alive` | `poisoned` | `not_started`), `disk.free_advisory`, `disk.free_advisory_threshold_percent`; `rubixdb status --system` prints them.
- A WARN log line when the sampler cannot start or keeps failing; `docs/PROJECT_STATE.md`, `missions/ACTIVE.md`.

#### Unchanged

- `/readyz.ready` keeps its certified meaning (constant `true`); `instance.readiness` reports the same value from the same function. `background.last_flush_ms` stays `null`. No engine, dependency or protected-path change. ADR-OBS-01 (extending `/readyz`) is PROPOSED, not applied.

### Docs: Full observability reconciliation (2026-10-06, read-only; no code change)

Full record: `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md`, `PHASE_RUBIXDB_FULL_OBSERVABILITY_ARCHITECTURE.md`.

#### Added

- `PHASE_RUBIXDB_FULL_OBSERVABILITY_ARCHITECTURE.md` (the layer as built at `2cbd5a7`) and `PHASE_RUBIXDB_FULL_OBSERVABILITY_RESULTS.md` (fresh three-part matrix, classification of the twelve open items, health-policy decision packet, latency investigation, verifications, proposed fixes P1-P14, maintainer decisions).

#### Found (documented, not fixed)

- Session-observation entries leak after a client disconnect mid-statement inside a transaction (`active_sessions` inflated, unbounded); `untracked_active` is cumulative; `disk.*_iops` / `*_mb_per_sec` are process-level but unlabelled; health thresholds have no approved policy; `/readyz.ready` and `instance.readiness` are two different definitions; several fields and rules are untested.

#### Unchanged

No production source, test, dependency or protected path was modified; `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md` is unchanged (closure addendum belongs to the next prompt).

### Product: Full observability -- sampler, system metrics, time series, diagnostics (2026-10-06)

Full record: `PHASE_RUBIXDB_FULL_OBSERVABILITY_CERTIFICATION.md`.

#### Added

- **Background sampler** (one thread, 1 Hz, immutable snapshots, panic-safe, stale/failed state derived from snapshot age) and **bounded time-series rings** (15 m / 1 h / 24 h / 7 d; 240,856 bytes measured, cap 1 MiB).
- `GET /v1/metrics/system` (CPU, memory, disk capacity/free/sizes, process I/O, rates, gauges, latency, WAL, compaction, background, security and limit counters, health); a value that cannot be measured is `null`, never `0`.
- `GET /v1/metrics/system/timeseries?window=15m|1h|24h|7d`; `GET /v1/observability/sessions|queries|events|version` (bounded, Reader, never SQL text, credentials or paths).
- `rubixdb status --system [--json]`.
- Counters: auth failures, forbidden, rate-limited, admin actions (+ last), refused sessions, 5xx, active connections, active queries. Health classification (decision requiring review).
- `LsmEngine::compaction_running()` (read-only accessor).

#### Fixed

- **`/v1/metrics` route table was unbounded:** the HTTP method token was copied into the key (3,200 hostile requests grew it from 17 to 821 keys). Labels are now a closed method set plus the matched route pattern, capped at 128 keys with an overflow bucket (same probe: 23 keys).
- SQL session age is the true age (creation time kept across statements).

#### Unchanged

`/healthz`, `/readyz`, `/v1/status`, `/v1/admin/status` and every other existing endpoint keep their field sets, status codes and headers (diffed on 18 endpoints); listener, port, CSP, nosniff, Referrer-Policy and Cache-Control unchanged; no new dependency.

### Product: Increment 18 -- unified access path, lazy index fetch, transaction scan semantics (2026-10-02)

Full record: `PHASE_RUBIXDB_INCREMENT18_ACCESS_PATH_ARCHITECTURE.md`,
`_MATERIALIZATION_ARCHITECTURE.md`, `_TRANSACTION_SCAN_SEMANTICS.md`,
`_PERFORMANCE.md`, `_RESULTS.md`. Closes the four open items of Increment 17.

#### Fixed

- **A transaction's scans now include its own uncommitted writes.** Previously
  only PK lookups did, so a multi-statement transaction's `UPDATE`/`DELETE`
  silently skipped rows the same transaction had inserted (reproduced: 3 of 5
  rows updated) and scans returned stale rows after an in-transaction
  update/delete. A bounded, versioned overlay of the write set is merged into
  every access path; no cost when the table is not written. Contract: D10 and
  the transaction ADR's "local overlay first" read path.
- **PK range vs secondary index:** when both could satisfy a predicate the
  planner always kept the PK range (up to 838x slower than the index). The
  executor now prices every candidate by its exact row count (a lockstep race of
  resumable cursors, bounded probing cost) and runs the cheapest; worst regret
  1.44x at 100K rows.
- **Index results are fetched lazily:** `LIMIT 10` over 25,000 index matches
  373 -> 16ms; cancellation and deadlines stop the fetch; per-scan memory is the
  bounded entry list.

#### Added

- `Transaction::overlay_for`, `TableOverlay`; `IndexRowFetcher`,
  `IndexProbeCursor`, `PkCountCursor`, `TableStore::count_pk_range_rows_as_of`;
  `PhysicalAccess::{IndexScan,PkRangeScan}::alternatives`,
  `AccessPathMode::ForcePkRange`, counter `access_path_switches`, cost
  parameter `index_open_ns`; `sql/src/exec/access_op.rs` (the access operator,
  moved out of `operators.rs`).
- Differential/property tests: transaction scans vs an independent model
  (randomized, with outside writers, index rebuilds and Compaction), lazy-fetch
  behaviour (LIMIT, backpressure, cancel, deadline), path independence across
  four access-path modes with mixed PK/index predicates; benchmarks.

#### Changed (performance, measured)

- A single-clone row context makes every fetched row cheaper: seq scan -33%,
  PK range -14%.

#### Known / open

- Index entry enumeration is still eager (`LIMIT` is O(K) key work); a
  two-candidate decision costs ~0.1ms fixed; overlay cost is O(w) per scan; a
  read-path concurrency plateau (~72-100 op/s from ~4 threads) is observed and
  not investigated; statistics are in-memory and re-learned after restart
  (measured immaterial).
- Full regression fails only on pre-existing, unrelated items (WAL throughput
  M1.2/M1.3; debug-only CLI two-instance test), re-verified on the previous tree.

### Product: Increment 17 -- index snapshot correctness + cost-based access path (2026-10-02)

Full record: `PHASE_RUBIXDB_INCREMENT17_INDEX_SNAPSHOT_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INCREMENT17_COST_MODEL_ARCHITECTURE.md`, `_PERFORMANCE.md`,
`_RESULTS.md`. Closes Increment 16's open F-2 finding and its missing cost
model; Increment 14/15/16 records unchanged.

#### Fixed

- **F-2:** a transaction whose snapshot predates an index's creation,
  rebuild or `DROP`/`CREATE` could be answered through that index and miss
  rows (backfilled entries carry a later sequence than the rows). An index
  now serves a read only if its catalog row, read as of the snapshot, was
  `Ready`; otherwise the identical table scan runs (ordering re-established
  where a `Sort` had been eliminated). No new persisted metadata: the
  readiness sequence is the catalog row's own MVCC version.
- Always choosing a sargable secondary index: up to 3.7x slower than a table
  scan at high selectivity (100K rows, 90%) and 2.7x at 1M rows (50%).
  Selection is now cost-based, made at execution from the exact match count,
  a drift-bounded table-size estimate and self-calibrating per-row costs --
  no hard-coded percentage; measured crossover 17%-25% and regime-dependent.

#### Added

- `CatalogService::get_index_as_of`, `IndexBuilder::index_row_usable_at`,
  two-phase index reads (`probe_index_entries_as_of` -> `IndexProbe`,
  `fetch_index_rows_as_of`); `IndexFallback` on `PhysicalAccess::IndexScan`.
- `rubixdb::relational::stats::RuntimeStats` (bounded, in-memory, never
  persisted; statistics affect performance only), `TableStore::count_rows`,
  `sql::exec::cost` (`AccessPathMode` in `ExecLimits` for benchmarking;
  server configuration, not reachable from SQL), counters
  `index_snapshot_fallbacks`, `index_cost_fallbacks`.
- Differential/property tests: F-2 scenarios and churn, path independence
  under poisoned statistics, drift-bound property, restart test; benchmarks.

#### Known / open

- Eager index-result materialization (deferred; +13% over the result at
  25,000 matches), PK-range-vs-index not cost-compared, read-your-own-writes
  not overlaid on scans (pre-existing), estimates re-learned after restart.
- Full regression fails only on pre-existing, unrelated items (WAL throughput
  M1.2/M1.3; debug-only CLI two-instance test), re-verified on the previous
  tree.

### Product: Increment 16 -- secondary-index read performance (2026-10-01)

Full record: `PHASE_RUBIXDB_INCREMENT16_INDEX_READ_ARCHITECTURE.md`,
`_PERFORMANCE.md`, `_RESULTS.md`. Append-only closure of Blocker 9's
`indexed_select` finding; Increment 14/15 records unchanged.

#### Fixed

- Secondary-index reads re-resolved table/column catalog metadata once
  per matched row (`get_row_as_of` inside `IndexBuilder::scan_entries`),
  making cost ~ matched rows x an LSM-dependent constant. Metadata is
  now resolved once per scan and rows are fetched by the encoded PK
  from the index entry. Measured (release, same build): 3-4x faster with
  auto-Compaction on, 7-29x with SSTables accumulated; ~15us/row,
  flat in table size 100K -> 1M. No certified-engine change.
- **Wrong results:** an upper-bound-only index range (`a <= x`,
  `a < x`, or `a = p AND b <= x` on a composite index) returned rows
  whose indexed value is NULL. Reproduced on unmodified HEAD; the bound
  is now kept as a residual filter (`sql/src/plan/access.rs`).
- `max_index_scan_rows` is enforced while collecting rather than after
  the whole result is materialized.

#### Added

- `IndexBuilder::index_lookup_as_of_bounded` /
  `index_range_scan_as_of_bounded`; label-free counters
  `index_rows_fetched`, `index_scan_micros_total`.
- Randomized differential/property tests against an independent
  reference model (snapshots, composite index, JOIN/aggregation, exact
  UPDATE/DELETE counts, physical index-entry audit, overlapping automatic
  Compaction); selectivity/result-size sweep; limit-boundary and
  bounded-scan tests; ignored measurement benchmarks.
- `Fixture::new_with_lsm` test helper.

#### Known / open

- F-2: a snapshot older than an index (re)build misses rows when reading
  through that index (reproduced; `#[ignore]`d test; not fixed).
- No cost-based index-vs-scan choice; eager index-scan `Vec`; index stats
  not on the HTTP metrics route.

#### Housekeeping

- `cargo fmt --all` applied (formatting-only changes to nine
  pre-existing test/example files); the two known CLI test clippy
  failures fixed without changing test semantics.

### Product: Increment 15 -- PK range scan fix (2026-09-30)

Full record: `PHASE_RUBIXDB_INCREMENT15_PK_RANGE_ARCHITECTURE.md`,
`_PERFORMANCE.md`, `_RESULTS.md`. Closes
`PHASE_RUBIXDB_INCREMENT14_BLOCKER9_PK_RANGE_SCAN_ADR.md`'s "NON-PK
READ PERFORMANCE = FAIL" finding.

#### Fixed

- A primary-key range predicate (`WHERE id >= x AND id < y`) fell back
  to a full `SeqScan` regardless of range width, with cost growing
  with total table size (measured up to ~300x a comparable single-row
  PK lookup, and up to 828ms at 105,907 rows for a <=50-row range).
  Added `PhysicalAccess::PkRangeScan` (`sql/src/plan/access.rs`) and
  `TableStore::scan_table_pk_range_rows_as_of`
  (`src/relational/table_store.rs`), reusing the certified `LsmEngine::
  range_scan` primitive and the existing `index_key::index_scan_range`
  prefix/successor byte-range logic verbatim (no certified engine code
  changed). `UPDATE`/`DELETE`/`JOIN`/aggregation inherit the fix
  automatically through the shared executor machinery. Measured: p50
  latency flat at 0.09-0.31ms from 1,000-100,000 rows vs. the old
  path's 3.9ms-337.6ms for the identical query on identical data (up
  to ~1,099x at 100,000 rows).

#### Added

- Multi-column composite-PK-prefix correctness tests (the mission's
  named correctness landmine: a partial PK equality must return every
  row sharing the prefix, never one arbitrary match), MVCC snapshot
  and tombstone/reinsert correctness tests, table-isolation tests,
  JOIN/aggregation/UPDATE/DELETE regression tests, and an extended
  independent differential/property-testing reference model
  (`sql/src/plan_reference_model.rs`).

### Product: Increment 14 hardening -- blockers 1-12 (2026-09-30)

Query starvation, CLI endurance, GUI endurance/browser memory,
`CREATE INDEX` mid-backfill crash safety, commit-ack-loss, a real
`cargo-audit` dependency scan, simultaneous multi-instance sustained
load, heap-level ownership tracing (`dhat`), cross-browser GUI timing,
the true 100,000-row GUI case, and delete safety -- each with its own
`PHASE_RUBIXDB_INCREMENT14_BLOCKER*.md` evidence document. The
remaining item from Increment 13's eleven, long-duration endurance, is
Blocker 9 (below), still in progress.

#### Fixed

- An accidentally-committed `dhat-heap.json` profiler dump (572KB) was
  untracked and gitignored.

### Product: Blocker 9 -- chained long-duration endurance (2026-10-01, complete)

Full record: `PHASE_RUBIXDB_INCREMENT14_BLOCKER9_LONG_DURATION_
ENDURANCE.md`.

New `api/examples/long_endurance.rs` (persistence-aware, resumes
across a process restart instead of resetting state; adds a real
`JOIN` to the operation mix) and `scripts/run_long_endurance_
segment.ps1` (orchestrates one segment against the real product
startup flow). A pre-flight smoke test caught and fixed a driver bug
(a heartbeat task overshooting its configured deadline by up to 300s).

Three ~115-minute segments (~5.76 cumulative hours) chained on the
same persistent instance/data (never reset, hard-stopped and restarted
between segments as a real crash-recovery exercise); table grew 1,000
-> 205,987 rows; resources stayed fully bounded throughout with
automatic compaction observed consolidating SSTables mid-run. Segment
1's own data surfaced the Increment 15 finding above; segment 2
(continuing, pre-fix binary) hit real 504 timeout failures as the
table grew further; segment 3 (continuing, rebuilt with the Increment
15 fix) showed `range_select`/`join` improve ~193x/~231x with zero
errors on the same growing dataset. Verdict: **PASS**, with the
pre-fix segments' real failures kept in the record and one distinct,
still-open item (`indexed_select` secondary-index cost growing with
table size, out of Increment 15's scope) named rather than hidden.

### Product: Increment 13 hardening -- final certification matrix (2026-09-29)

Full record: `PHASE_RUBIXDB_INCREMENT13_CERTIFICATION.md`
(consolidated with `PHASE_RUBIXDB_INCREMENT13_PERFORMANCE.md`,
`_SECURITY.md`, `_RELIABILITY.md`).

#### Added

- Four final consolidation documents covering every gate this
  Increment 13 continuation closed, cross-referencing rather than
  duplicating the detailed evidence documents already produced this
  session. Every gate is reported `PASS` (with `NON-BLOCKING
  LIMITATION` where scope-limited), `NOT APPLICABLE`, or `NOT DONE
  THIS PASS` -- never a converted or assumed `PASS`.

**Final production decision: RUBIXDB PRODUCT SURFACE = NOT PRODUCTION
READY**, with eleven explicitly named, genuinely unexecuted items as
the exact blockers -- a materially stronger evidence-backed position
than existed before this continuation began, but not a certification
claim these documents make.

### Product: Increment 13 hardening -- real HTTP-disconnect cancellation (2026-09-29)

#### Added

- `api/tests/api_cancellation.rs`: closes Phase O with a real server
  and a real client dropping its connection mid-request against a
  genuinely expensive query made slow via real 24-way concurrent
  contention. Proves the server stays fully responsive to new requests
  immediately after a disconnect, never wedges, and all concurrent
  background queries eventually complete rather than hanging.

### Product: Increment 13 hardening -- real GUI/frontend performance (2026-09-29)

Full results: `PHASE_RUBIXDB_GUI_PERFORMANCE.md`.

#### Added

- `frontend/playwright.gui.config.ts`,
  `frontend/e2e-gui/gui_performance.spec.ts`: real Playwright
  performance suite against the actual `rubixdb gui`-hosted product
  path (one origin, real release binary, real production frontend
  build). Measures page-load timing and execute+render timing at
  100/1,000/10,000-row result sizes, confirming pagination keeps
  rendered DOM rows capped at 200 regardless of result size.

#### Fixed

- `frontend/vite.config.ts`: the new `e2e-gui/` directory was picked
  up by `vitest` (only `e2e/` was excluded), causing every Playwright
  `test.describe` to collide with Vitest's own runner -- added to
  `test.exclude`.

### Product: Increment 13 hardening -- CLI performance and handle/thread stability (2026-09-29)

Real measurements against the release binary, no code changes.
`PHASE_RUBIXDB_PERFORMANCE_BASELINE.md` §8-9: single-query CLI timing
(50-75ms, dominated by process startup/instance-attach, not per-
statement cost), 100/1,000-statement script timing (~4-6ms/statement,
tracking real server latency closely), and 50+50 real connect/
disconnect and session cycles showing the server's own handle/thread
counts move by +1/+1 total across the first 50 cycles and not at all
across the second 50, with committed/rolled-back correctness verified
in the same pass.

### Product: Increment 13 hardening -- real sustained endurance run (2026-09-29)

Full results: `PHASE_RUBIXDB_ENDURANCE.md`.

#### Added

- `api/examples/endurance.rs`: a real sustained mixed-workload driver
  (6 concurrent read/write workers + a dedicated session/transaction-
  cycling worker) used for a real 180-second endurance run against a
  release `rubixdb gui` instance, with real `Get-Process` RSS/handle/
  thread sampling throughout. 97,000+ requests; all 2,460 errors
  confirmed to be genuine snapshot-isolation conflicts, not a bug;
  handle/thread counts stayed flat across the whole run (real evidence
  against a leak); RSS growth correlated with real ~16x data growth.

### Product: Increment 13 hardening -- full concurrency ladder and real resource sampling (2026-09-29)

#### Changed

- `api/examples/sql_bench.rs`: extended to the full 1-64 concurrency
  ladder (reads) / 1-32 (writes), 1,600 iterations/level for
  statistically meaningful high-concurrency samples.
- `cli/src/host.rs`: local rate limit raised again to
  `rate_limit_rps=100_000.0`/`burst=200_000` -- the first raise
  (2000/4000) was itself proven too low by this run's own real
  measured throughput (up to 28,793 req/s for a single legitimate
  client). See `PHASE_RUBIXDB_PERFORMANCE_BASELINE.md` §7.

### Product: Increment 13 hardening -- real HTTP/JSON/SQL fuzzing (2026-09-29)

#### Added

- `api/tests/api_http_fuzz.rs`: real HTTP boundary fuzz/robustness
  harness against a real running server (not the in-process router
  shortcut) -- malformed/truncated JSON, semantically-wrong JSON,
  invalid UTF-8, random-byte bodies, deep/large SQL expressions through
  the real parser/binder/planner/executor pipeline, malformed
  authorization/headers, and repeated abrupt raw-TCP connection
  termination mid-request. Expensive/oversized cases (deep parens,
  2MB literal, 50,000-element parameter array) assert active
  rejection by a real resource limit, not just crash-safety.

### Product: Increment 13 hardening -- canonical port 302 and real crash-kill matrix (2026-09-29)

#### Added

- `instance::port::DEFAULT_API_PORT` changed to `302` (from `8080`),
  per explicit user direction, accepted as a Windows-only product
  constraint (privileged-port restriction on Linux/macOS). Propagated
  to the standalone API's own default, the benchmark tool, and the
  frontend dev-proxy; `bind_loopback` now logs a clear diagnostic on a
  `PermissionDenied` bind failure specifically. Two new tests
  (`default_port_is_302_decimal_not_octal`,
  `port_collision_with_an_unrelated_process_falls_back_safely`).
- `cli/tests/crash_recovery_integration.rs`: four real crash-kill
  tests using the actual compiled binary and real `Child::kill()` --
  committed-write survival, uncommitted-transaction non-survival,
  committed-DDL-and-index survival, and no-torn-writes under a kill
  during sustained concurrent write load -- all exercised through the
  real product entry point (CLI -> HTTP -> embedded server -> engine),
  not the raw engine test harness.

### Product: Increment 13 hardening -- release build and performance baseline (2026-09-29)

Full decision record and results: `PHASE_RUBIXDB_PERFORMANCE_BASELINE.md`.

#### Added

- `api/examples/sql_bench.rs`: a real, reusable end-to-end `POST
  /v1/sql` latency/throughput benchmark tool -- real concurrent HTTP
  clients, real percentile computation from actual samples, covering
  PK lookup, indexed lookup, range scan, full-table count, `GROUP BY`/
  `HAVING`, INSERT, UPDATE, DELETE at multiple concurrency levels.

#### Fixed

- `cli/src/host.rs`: the local embedded instance's rate limit inherited
  the standalone API's multi-tenant-deployment default (200rps/burst
  400) unexamined, which throttled a single legitimate local client
  under realistic concurrent load -- raised to a documented, still-
  bounded local default (2000rps/burst 4000), operator-overridable via
  `RUBIXDB_LOCAL_RATE_LIMIT_RPS`/`_BURST`. The standalone
  `rubixdb-api` binary's own env-configured default is unchanged.

### Product: GUI launcher and local instance manager (2026-09-29)

Adds the local instance manager and `rubixdb gui` launcher assumed
(but never actually built) by the Increment 13 hardening mission spec
-- built as its own dedicated, real increment per explicit user
direction rather than silently folded into a "certification" of
nonexistent functionality. Full decision records:
`PHASE_RUBIXDB_INSTANCE_ARCHITECTURE.md`,
`PHASE_RUBIXDB_INSTANCE_SECURITY.md`,
`PHASE_RUBIXDB_GUI_ARCHITECTURE.md`; certification evidence:
`PHASE_RUBIXDB_GUI_INSTANCE_INCREMENT_RESULTS.md`.

#### Added

- `instance/` (new workspace member, `rubixdb-instance`): real OS-level
  instance ownership via `fs4::FileExt` (`flock`/`LockFileEx`), never a
  PID file -- the OS itself releases the lock the instant an owning
  process exits or crashes, so there is no staleness heuristic
  anywhere in this crate. Per-OS app-data directory resolution
  (`%LOCALAPPDATA%`/`~/Library/Application Support`/`$XDG_DATA_HOME`);
  instance names restricted to `[A-Za-z0-9_-]{1,64}`, the entire
  path-traversal defense.
- `instance::manifest`/`instance::credentials`: a persistent, non-secret
  `instance.json` (`instance_id`, `name`, `api_port`,
  `created_at_unix_secs`) and a generated 256-bit local admin API key
  (`credentials.json`, mode `0600` on Unix), read directly by both
  `gui` and `cli` -- no login prompt, no weakening of the existing
  bearer-key auth model underneath.
- `instance::port::bind_loopback`: hardcoded loopback-only binding,
  bind-once (no probe-then-rebind TOCTOU), collision fallback to an
  OS-assigned ephemeral port.
- `instance::handshake`: real HTTP identity verification (`GET
  /v1/instance`) and readiness polling (`GET /healthz`) -- never a
  fixed sleep, never trusting a lock or a manifest alone.
- `instance::browser`: dependency-free default-browser launch
  (`cmd /C start`/`open`/`xdg-open`).
- `instance::acquire`/`discover`/`list_instances`: the one algorithm
  both `gui` and the CLI client use to find-or-create an instance,
  including bounded, backoff-retried attach handling for the real
  "two processes racing an unstarted instance" case.
- `api/src/routes/instance.rs`: `GET /v1/instance` -- new, additive,
  unauthenticated identity-handshake route.
- `api/src/routes/mod.rs`: optional frontend static/SPA-fallback
  serving via `tower_http::services::ServeDir`, gated by a new
  `Config.frontend_dist: Option<PathBuf>` field (`None` by default --
  every pre-existing deployment shape unchanged); mounted as a
  `.fallback_service` so it can never shadow a real API route.
- `api/src/config.rs`: three new additive `Option` fields
  (`instance_id`, `instance_name`, `frontend_dist`).
- `cli/src/gui.rs`, `cli/src/host.rs`: `rubixdb gui` -- finds/creates
  the instance, hosts the real `rubixdb-api` server in-process on a
  dedicated thread (reuses `rubixdb_api::{AppState, Config,
  routes::build_router, server::serve}` directly, never a second SQL
  engine), serves the real frontend build, opens the browser, blocks
  on Ctrl+C/SIGTERM, shuts down gracefully. Presents "continue
  existing / start new instance" when an already-running, handshake-
  verified instance is found.
- `cli/src/instance_cmd.rs`: `rubixdb instance list`/`status`.
- `cli/src/frontend_dist.rs`: locates a built frontend (`RUBIXDB_
  FRONTEND_DIST` override, then paths relative to the executable).
- `cli/src/main.rs`: subcommand dispatch (`gui`/`instance`/`cli`,
  default unchanged); the plain client role now auto-discovers (or, if
  none exists yet, headlessly becomes the owner of) a local instance
  when `RUBIXDB_API_URL` is unset, so `rubixdb` alone is a complete
  first-run entry point -- `RUBIXDB_API_URL` remains a full, unchanged
  explicit override.
- `api/tests/api_instance_and_frontend.rs` (6 tests),
  `cli/tests/gui_instance_integration.rs` (6 tests, real compiled
  binary, real racing OS processes), 27 new unit tests in
  `rubixdb-instance`.

#### Fixed

- `ServeDir::not_found_service` forces every fallback response to HTTP
  404 regardless of whether a file was actually served -- switched to
  plain `.fallback(...)`, which preserves the real 200 for a
  successfully served file.
- The embedded server's `std::net::TcpListener` was never set
  non-blocking before being handed to
  `tokio::net::TcpListener::from_std`, so the async runtime never
  actually polled it for acceptance -- every request silently hung
  until timeout despite the TCP handshake completing at the OS level.
  Found via `netstat`/`curl` against a real running process.
- A failed `EmbeddedServer::start()` could leave an orphaned server
  thread/engine/listening socket running -- every post-spawn failure
  path now signals shutdown and joins the thread before returning.
- The plain CLI client's connection resolution trusted a stale,
  unverified `instance.json` left behind by a since-exited process
  (using the liveness-blind `discover()` as its primary path) instead
  of verifying anyone was actually listening -- now routes
  unconditionally through the handshake-verifying `acquire()`.
- A lock-release-on-process-kill test used an unqualified libtest
  filter name and silently never exercised the scenario it claimed to
  cover.

### Relational database: Increment 12 (SQL API, CLI, and frontend SQL console) (2026-09-29)

Exposes the already-certified SQL engine as a real product surface:
`POST /v1/sql` (the one SQL execution path), a PostgreSQL-style CLI
(`rubixdb-cli`, new workspace member), and a frontend SQL console --
API, CLI, and frontend all terminate at the identical HTTP handler,
never a second parser/binder/planner/executor. `PHASE_RELATIONAL_SQL_
API_ARCHITECTURE.md`, `PHASE_RELATIONAL_CLI_ARCHITECTURE.md`, and
`PHASE_RELATIONAL_FRONTEND_SQL_ARCHITECTURE.md` are the full decision
records; `PHASE_RELATIONAL_SQL_API_INCREMENT12_RESULTS.md` has the
certification matrix.

#### Added

- `api/src/routes/sql.rs`: `POST /v1/sql` -- parse/bind/plan/execute
  wired verbatim into `rubixdb-sql`, typed request parameters and
  response values (`api/src/sql_params.rs`; `bigint`/`decimal`/`time`/
  `timestamp` travel as wire-safe strings, never a lossy JSON number),
  execution run inside `tokio::task::spawn_blocking` with a drop-
  triggered `CancellationToken` cancel and a deadline backstop above
  `rubixdb_sql::exec::ExecLimits::deadline`'s own internal check.
- `api/src/sql_session.rs`: the SQL transaction/session registry --
  sessions exist only while an explicit transaction is open (`BEGIN`
  .. `COMMIT`/`ROLLBACK`), everything else runs fully stateless
  autocommit; per-principal session cap, idle timeout, max lifetime, a
  background reaper.
- `api/src/routes/catalog.rs`: read-only `GET /v1/catalog/{databases,
  schemas,tables,tables/:name,indexes,authz}` -- added only after
  confirming `system.*` catalog objects have no SQL `SELECT` path at
  all; reuses the identical `rubixdb_sql::auth::is_authorized` check
  per row, never a second authorization system.
- `api/src/auth.rs`: `/v1/sql`'s own `Reader`-minimum role gate (a
  documented exception to the existing method-based default) and
  `to_sql_auth_context`, the one API-role -> SQL-`AuthContext` mapping.
- `api/src/error.rs`: every `SqlError` variant mapped to a stable HTTP
  status/machine-readable code (`PARSE_ERROR`/`BIND_ERROR`/
  `AUTHORIZATION_ERROR`/`CONFLICT_ERROR`/`RESOURCE_LIMIT`/`TIMEOUT`/
  `CANCELLED`/`UNSUPPORTED`/`STORAGE_ERROR`/...).
- `api/src/sql_metrics.rs`: bounded SQL-endpoint metrics, folded into
  the existing `GET /v1/metrics`.
- `rubixdb-cli` (new workspace member, binary `rubixdb`): a thin HTTP
  client of `POST /v1/sql` -- no dependency on `rubixdb`/`rubixdb-sql`
  at all. The locked `\l \ls \lt \d \di \du \conninfo \c \help \q`
  command contract against real backend metadata, a real interactive
  REPL (`rustyline`), real `-c`/`-f` script mode sharing one server
  session per run, quote-aware (lexical only, never semantic)
  statement-boundary splitting, adversarial-content-safe terminal
  rendering (ANSI escape sequences sanitized).
- `frontend/src/pages/SqlConsolePage.tsx` (new `/sql` route/nav item):
  SQL editor, Execute/Cancel/Clear, typed paginated result grid,
  transaction/session indicator, query history. Cancel performs a real
  `AbortController`/`fetch` abort the server observes as a dropped
  connection. Every result cell renders as a React text node -- no
  `dangerouslySetInnerHTML` anywhere.
- 84 new tests: 40 in `rubixdb-api` (including a real concurrent-
  transaction test, 12 simultaneous sessions via `tokio::spawn`, zero
  cross-contamination), 25 in `rubixdb-cli` (against the real compiled
  binary and a real running server), 19 in the frontend (16 Vitest + 3
  real Playwright browser E2E) -- see Results doc.
- `AppState.engine` changed from bare `LsmEngine` to `Arc<LsmEngine>`
  (additive; every existing call site unaffected, verified by grep
  before the change).

#### Fixed

- **Real regression, found before writing any new tests**: eagerly
  bootstrapping the catalog in `AppState::new` injected `system.
  databases`/`system.schemas` rows into the *same flat keyspace* `/v1/
  kv`/`/v1/range` already scan (there is no separate catalog storage
  area), breaking two pre-existing, certified KV integration tests and
  silently falsifying `/v1/metadata`'s own "no tables, no schema, no
  SQL" claim for every deployment. Fixed by making catalog bootstrap
  lazy (`SqlContext::bind_context()`, first call only, cached
  thereafter) -- a pure-KV deployment's keyspace is now byte-for-byte
  unaffected by this increment's existence.
- **Real deadlock in the CLI's own test harness**: `#[tokio::test]`'s
  default single-threaded runtime competed with a blocking `Command::
  output()` subprocess call for its one available thread, hanging the
  spawned real-server task forever. Diagnosed via `Get-Process` (two
  hung `rubixdb.exe` instances, a locked test binary), fixed by
  switching the affected tests to a multi-threaded runtime.

### Relational database: Increment 11 (production GROUP BY / HAVING / aggregation) (2026-09-29)

Adds production `GROUP BY`/`HAVING`/aggregate execution (`COUNT`, `SUM`,
`AVG`, `MIN`, `MAX`) to the existing `rubixdb-sql` crate, threaded
through the full pipeline: parser -> internal AST -> binder -> logical
plan -> rule optimizer -> physical plan -> executor -> transaction/read
context -> real relational storage. `PHASE_RELATIONAL_AGGREGATION_
ARCHITECTURE.md` is the full decision record; `PHASE_RELATIONAL_
AGGREGATION_INCREMENT11_RESULTS.md` has the certification matrix and
measured benchmark numbers.

#### Added

- `sql/src/bind/select.rs`: `GROUP BY` binding, select-list/`HAVING`/
  `ORDER BY` group-compatibility validation (`validate_group_compat`),
  aggregate-call extraction into `BoundSelect::aggregates` (a shared,
  deduplicated, positionally-indexed list -- `extract_aggregates`
  rewrites every bound `Aggregate(...)` node to `AggregateRef(idx)`).
- `crate::plan::logical::LogicalPlan::Aggregate` / `crate::plan::
  physical::PhysicalPlan::Aggregate` (new plan-node variants). `HAVING`
  is represented as an ordinary `Filter` node placed directly above
  `Aggregate` -- reuses existing three-valued-logic `Filter` semantics
  verbatim, never a second boolean model.
- `crate::exec::operators::AggregateOp` -- hash aggregation over
  `crate::aggregate::GroupingKey`'s canonical, collision-free grouping
  representation; retains one representative input row plus a small
  `Vec<AggregateState>` per group (never full per-row materialization);
  insertion-ordered emission (never raw `HashMap`-iteration order, for
  plan determinism).
- `RowContext::aggregates`/`with_aggregates`/`get_aggregate`;
  `BoundExprKind::AggregateRef` evaluation in `crate::exec::expr_eval`.
- `ExecLimits::max_group_count`/`max_aggregate_state_bytes` (checked
  before the corresponding growth, mirroring `Distinct`'s existing
  `max_materialized_rows` check); `ExecMetrics::groups_created`/
  `groups_emitted`/`aggregate_rows_processed`/`aggregate_resource_
  limit_hits`; `PlannerMetrics::aggregate_plans`.
- `sql/src/aggregate_reference_model.rs`: an independent reference
  aggregation engine (never calls the planner/executor/storage),
  compared against the real pipeline via a 7-scenario fixed matrix, a
  2,000-group high-cardinality case, and 64 `proptest`-generated random
  tables -- all matched exactly.
- `sql/benches/aggregation_bench.rs`: per-function cost, `GROUP BY`
  cardinality (10/1,000/10,000 groups), composite keys, `HAVING`
  overhead, `GROUP BY` + `ORDER BY` + `LIMIT`, rows/sec scaling.
- 48 new tests: 21 in `sql/src/bind_tests.rs`, 18 in `sql/src/
  exec_tests.rs`, 3 in `aggregate_reference_model.rs` -- see Results doc.
- `rubixdb::relational::{validate_decimal, MAX_DECIMAL_PRECISION}`
  re-exported from the crate root (`src/relational/mod.rs`, additive;
  the only change outside `sql/` this increment made).

#### Fixed

- `sql/src/aggregate.rs::AggregateState::merge`'s `Max` arm wrote
  through an unbound `max` identifier instead of its own matched `m1`
  binding -- inherited from an earlier, uncommitted, never-compiling
  session; found and fixed while inspecting the existing partial pass
  before writing anything new (the crate did not compile at all at the
  start of this increment).
- `sql/src/parse_tests.rs::unsupported_grammar_is_a_typed_error_not_a_
  panic`: its `"SELECT id FROM t GROUP BY id"`/`"... HAVING ..."` cases
  asserted `GROUP BY`/`HAVING` were unsupported grammar -- exactly the
  feature this increment adds. Replaced with the `GROUP BY`-adjacent
  forms that remain genuinely unsupported (`GROUP BY ALL`, `GROUP BY
  ROLLUP(...)`), keeping the test's own stated premise true rather than
  asserting something this increment made false.

### Relational database: Increment 10 (production write executor) (2026-09-24)

Adds a production-grade write executor (`sql/src/exec/write.rs`,
`sql/src/exec/write/metrics.rs`, new modules in the existing
`rubixdb-sql` crate): `SQL write -> Parser -> AST -> Binder -> Plan ->
Transaction -> Write Executor -> TableStore/IndexStore -> write_batch ->
durable committed state`. `INSERT`, `UPDATE`, `DELETE`, and the DDL
forms the current catalog/index architecture actually supports
(`CREATE SCHEMA`/`TABLE`, `DROP TABLE`, `CREATE`/`DROP INDEX`) now
really execute -- no demo, no wrapper, no transaction bypass.
`PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md` is the full decision
record; `PHASE_RELATIONAL_WRITE_EXECUTOR_INCREMENT10_RESULTS.md` has the
certification matrix and measured benchmark numbers.

#### Added

- `sql/src/exec/write.rs`: `execute_write`/`execute_write_autocommit`,
  `execute_insert`/`execute_update`/`execute_delete`/`execute_ddl`. A
  two-phase, bounded-memory design for `UPDATE`/`DELETE` target-row
  finding -- reuses Increment 9's own certified read-access machinery
  (`PkLookup`/`IndexScan`/`SeqScan`) to collect only matching rows'
  `PRIMARY KEY` values (never full rows), bounded by a new `ExecLimits::
  max_dml_target_rows`, then mutates each row in a second pass.
- `sql/src/exec/write/metrics.rs`: `WriteMetrics`/`WriteMetricsSnapshot`
  -- bounded-cardinality counters only (`insert_statements`/`rows_
  inserted`/etc., `write_conflicts`, `dml_errors`/`ddl_errors`), no
  table/schema/SQL-text/principal label ever accepted.
- `sql/src/write_tests.rs`: 42 new tests -- see Results doc.
- `sql/tests/write_crash_consistency.rs`: real, cross-process, OS-level
  crash testing at the write executor's own commit boundary (item 30's
  "HARD PRODUCTION GATE"), reusing `rubixdb::wal::{AbortPoint, FileWal::
  set_abort_hook}` verbatim across 9 real abort points -- proves a crash
  during `INSERT`'s own `write_batch` call can never leave a table row
  durable without its secondary-index entry, or the reverse.
- `sql/benches/write_executor_bench.rs`: `INSERT`/`UPDATE`/`DELETE`/DDL
  latency, write amplification vs. index count (0/1/2/5/10), commit
  latency vs. write-set size (1-128 rows), concurrent-writer throughput
  (1/4/16/32 threads).
- `RowContext::row_for` and `ExecLimits::max_dml_target_rows` (`sql/src/
  exec/mod.rs`, additive).

#### Fixed / Found

- **Binder bug** (`sql/src/bind/dml.rs`): an *omitted* `INSERT` column
  was always bound to a plain `NULL` literal, even when the column had a
  declared `DEFAULT` -- structurally indistinguishable from an explicit
  `NULL`, so any `DEFAULT`-bearing column omitted from an `INSERT`'s
  column list would have silently stored `NULL` instead of its declared
  default. Found by inspecting the binder before designing execution (a
  breadcrumb in `encode_default_literal`'s own Increment-6 doc comment
  named exactly this future decode step). Fixed at bind time; regression
  test added.
- **Real primary-key-uniqueness gap, found by differential testing**:
  `Transaction::put_row` is a generic upsert-at-key primitive with no
  notion of "this key must not already exist" -- `commit`'s own
  freshness/`UNIQUE` validation catches a *concurrently racing* `INSERT`
  of the same `PRIMARY KEY`, but not a plain, *later*, non-overlapping
  `INSERT` reusing a key an earlier, already-committed transaction used
  (both snapshots agree, so nothing looks like a conflict) -- the row
  was silently overwritten instead of the `INSERT` failing. Found by
  `write_tests::differential`'s own independent reference-model test,
  not by inspection. Fixed: `execute_insert` now checks for an existing
  row via `Transaction::get_row` (the same snapshot-correct read every
  other statement already uses, entirely within the same transaction,
  never a second detector) immediately before each row's `put_row`,
  reported as the same `SqlError::Conflict` class a concurrent conflict
  already uses. See `PHASE_RELATIONAL_WRITE_EXECUTOR_ARCHITECTURE.md`
  §4b for the full argument that this closes a *sequential* gap the
  existing commit-time freshness check structurally cannot see, without
  weakening or duplicating that check's own, still-sole authority over
  the concurrent case.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No CLI, HTTP SQL API, or
frontend SQL console exists; `GROUP BY`/`HAVING`/aggregates/window
functions/subqueries/CTEs/set operators remain unbound at the binder.
Full account: `PHASE_RELATIONAL_WRITE_EXECUTOR_INCREMENT10_RESULTS.md`.

### Relational database: Increment 9 (read-only query executor) (2026-09-24)

Adds a production-grade query executor (`sql/src/exec/`, new module in
the existing `rubixdb-sql` crate): `Plan/PhysicalPlan -> Execute ->
Typed Result` against the real `TableStore`/`IndexBuilder`/
`Transaction` primitives. `PHASE_RELATIONAL_QUERY_EXECUTOR_
ARCHITECTURE.md` is the full decision record; `PHASE_RELATIONAL_QUERY_
EXECUTOR_INCREMENT9_RESULTS.md` has the certification matrix and
measured benchmark numbers. Only `SELECT` executes -- no write
statement or DDL runs yet.

#### Added

- `sql/src/exec/{mod,expr_eval,operators}.rs`: a pull-based `Operator`
  trait, one struct per `PhysicalPlan` node (`PkLookup`/`IndexScan`/
  `SeqScan`, `Filter`, `Projection`, `Distinct`, `Sort`, `Limit`,
  `NestedLoopJoin`/`IndexNestedLoop`); runtime `BoundExpr` evaluation
  with real SQL three-valued logic (`NULL`/`AND`/`OR`/`NOT`/`IS [NOT]
  NULL`/`BETWEEN`/`IN`/`LIKE`/`CASE`, plus the four registered scalar
  functions); `LEFT JOIN` null-extension, residual-predicate
  preservation, and per-outer-row correlated-key re-evaluation for
  `IndexNestedLoop`, all directly tested.
- `TableStore::get_row_as_of`/`scan_table_as_of`/`scan_table_rows_as_of`
  (the last genuinely lazy) and `IndexBuilder::index_lookup_as_of`/
  `index_range_scan_as_of` (core crate, additive): the snapshotted
  scan-shaped read primitives the executor needed and the storage layer
  did not yet expose -- closes a gap `IndexBuilder::scan_entries`'s own
  Increment 5 doc comment had already named and deferred to "D10's
  future transaction layer," which now exists. Plus a trivial
  `Transaction::snapshot_seq()` getter.
- `PhysicalAccess::table_ref` (planner, additive): the one gap in
  Increment 8's own planner output this increment's executor needed
  filled in -- without it, a self-join or even a bare predicateless
  scan has no way to resolve a `ColumnRef` against its own row.
- `sql/benches/query_executor_bench.rs`: PK-lookup layering (raw engine
  get vs. `TableStore` vs. full executor), scan-vs-index selectivity
  (~940x faster for a 1-in-10,000-selective indexed lookup), `LIMIT`
  early termination (~11x faster, independently proven via
  `rows_scanned` metrics), join-algorithm comparison (`IndexNestedLoop`
  ~15x faster than plain `NestedLoop` for a selective 200x200 join).
- 36 new executor tests plus 4 new core-crate regression tests for the
  new snapshotted primitives.

#### Fixed / Found

- `ORDER BY ... DESC NULLS LAST` produced `NULL` values first instead
  of last -- the sort comparator reversed the already-absolute `NULLS
  FIRST`/`LAST` placement a second time whenever `DESC` was also
  present. Found while writing a test covering exactly that
  combination, not by inspection -- the fourth consecutive increment
  where a real bug or gap was found this way (Increments 5/6/8's own
  documented findings).

### Relational database: Increment 8 (rule-based query planner and optimizer) (2026-09-23)

Adds a production-grade, rule-based query planner (`sql/src/plan/`, new
module in the existing `rubixdb-sql` crate): `BoundStatement ->
LogicalPlan -> (one fixed-order optimization pass) -> PhysicalPlan`,
implementing D16/D17/D18. `PHASE_RELATIONAL_QUERY_PLANNER_ARCHITECTURE.md`
is the full decision record; `PHASE_RELATIONAL_QUERY_PLANNER_
INCREMENT8_RESULTS.md` has the certification matrix and measured
benchmark numbers. No executor exists yet.

#### Added

- `sql/src/plan/{logical,optimize,access,physical,validate,explain,
  expr_util,limits,metrics,mod}.rs`: `PRIMARY KEY` lookup detection
  (composite-safe -- `a = ?` for `PRIMARY KEY(a, b)` never becomes a
  partial point lookup); secondary-index selection respecting declared
  leading-column order, `Ready`-only, and never selecting a `Primary`-
  kind catalog index as an `IndexScan` (a real, inspected storage fact:
  no physical entries are ever written for one); predicate pushdown
  that is `LEFT JOIN`-safe by construction (never rewrites `BoundExpr`
  logic, only relocates where it is evaluated, and only into a non-
  null-extended scan); projection pruning (honestly reported as
  metadata-only -- no partial-column-decode storage primitive exists
  yet); conservative `LIMIT`/`ORDER BY` analysis (a bare ascending
  index scan does *not* satisfy the SQL-standard `NULLS LAST` default
  -- only an explicit `NULLS FIRST` does, matching the engine's own
  physical, non-reversible ascending order); explicit `DISTINCT`;
  `INNER`/`LEFT JOIN` via Nested Loop with mechanical Index Nested Loop
  substitution; `UPDATE`/`DELETE` reusing `SELECT`'s own access-
  planning algorithm verbatim; structural plan validation; a
  deterministic `EXPLAIN` formatter; planner-specific resource limits
  and bounded-cardinality metrics.
- `sql/benches/query_planner_bench.rs`: plan-build latency across
  statement shapes, catalog-resolution cost vs. catalog size (100-
  10,000 tables), optimizer complexity vs. predicate count (1-120) and
  join count (1-8) -- both confirmed linear.
- 42 new tests: logical/physical separation, PK/index/range/residual
  correctness, LEFT JOIN safety, NULL semantics, ordering analysis in
  every direction, join algorithm selection, resource limits,
  adversarial deep predicates, race-free concurrency, and a randomized
  `proptest` differential test against an independent reference model.

#### Fixed / Found

- `sql/src/bind/expr.rs::bind_shared` (Increment 6, pre-dating this
  increment) unconditionally re-bound every operand of a shared-type
  unification a second time, including already-rigidly-typed subtrees
  -- because it sits on `bind`'s own recursive path, this doubled the
  work at every nesting level of a chain of binary operators,
  `O(2^depth)` instead of `O(depth)`. A real, exploitable CPU-
  exhaustion vector: a 20-term `WHERE ... OR ...` chain took ~4 seconds
  and climbing, well within `SqlLimits`' own resource limits (the
  existing depth guard bounds *shape*, not *work*). Found while writing
  this increment's own adversarial planner test, not by inspection --
  the same pattern as Increment 5's phantom-index-entry race and
  Increment 6's flat-operator-chain stack overflow. Fixed: only a
  flexible literal operand needs re-binding; a rigid expression's type
  is merely re-checked, not re-walked. A 100-term chain now binds in
  <1ms; D21's "no implicit coercion" correctness re-verified unchanged.

### Relational database: Increment 7 (Snapshot Isolation transaction engine) (2026-09-23)

Adds a production-grade transaction engine implementing D10's already-
approved Snapshot Isolation model. `PHASE_RELATIONAL_TRANSACTION_
ARCHITECTURE.md` is the full decision record; `PHASE_RELATIONAL_
TRANSACTION_INCREMENT7_RESULTS.md` has the certification matrix and
measured benchmark numbers. No SQL execution exists yet.

#### Added

- `src/relational/txn.rs` (new): `TransactionManager`/`Transaction` --
  `BEGIN` pins one `LsmEngine::Snapshot`; reads resolve against a local
  write-set overlay first, the pinned snapshot second (read-your-own-
  writes); `COMMIT` re-validates every touched key's freshness (value
  comparison, catching `PRIMARY KEY` conflicts with no special-casing),
  enforces `UNIQUE` for real for the first time (a physical existence
  scan reusing Increment 5's own index structure; intra-transaction-
  duplicate and self-vacated-entry races both closed; standard-SQL
  `NULL`-never-conflicts semantics), then applies the whole write-set
  -- table row and every affected index entry together -- through one
  `LsmEngine::write_batch` call. `ROLLBACK` is O(1). `commit(self)`/
  `rollback(self)` consume `self` by value, making "commit twice" a
  compile error.
- Commit serializes on Increment 5's per-table `epoch_lock` (write
  side), acquired for every touched table in sorted order -- reused,
  not duplicated. Table-level, not key-level: measured directly, one
  table's commit throughput does not scale past ~1 concurrent
  committer even on fully disjoint keys -- reported honestly, not
  hidden.
- `TransactionManager::autocommit_put_row`/`autocommit_delete_row` --
  the reusable BEGIN-op-COMMIT primitive a future SQL executor's
  implicit transactions will use.
- `benches/transaction_bench.rs`: `BEGIN` latency, four-read-path
  overhead comparison, commit latency vs. write-set size (1-128),
  commit latency vs. table size at a fixed write-set (confirms
  conflict validation does not scan the table), catalog-resolution
  cost vs. catalog size (100-10,000 tables, reusing `sql/`'s own
  "no automatic caching" finding), concurrent-commit throughput
  scaling (1-32 threads).
- 42 new tests: lifecycle, read-your-own-writes, snapshot consistency,
  conflict detection, `UNIQUE` enforcement (6 tests), atomic
  table+index commit, autocommit, a direct write-skew demonstration,
  five deterministic barrier-synchronized concurrency tests (never
  sleep-based), resource-limit boundaries, snapshot/registry lifetime,
  real automatic-compaction interaction, two real-process-restart
  crash-recovery tests, metrics accounting, an authorization-boundary
  test, and two differential tests -- a fixed scenario and a
  randomized, interleaved `proptest` across multiple simultaneously-
  open transaction slots -- against an independent, from-scratch
  Snapshot Isolation reference model.

#### Documented

- **Write skew is possible under Snapshot Isolation** -- directly
  demonstrated (the two-on-call-doctors scenario), never claimed
  fixed; this is Snapshot Isolation, not Serializable isolation.

### Relational database: Increment 6 (SQL parser, internal AST, binder, authorization) (2026-09-23)

Adds the SQL front-end foundation in a new `rubixdb-sql` workspace
crate: parser integration, an internal AST decoupled from the
third-party parser, and a binder that resolves catalog identifiers and
authorization in one pass. `PHASE_RELATIONAL_SQL_GRAMMAR.md` is the
full reference; `PHASE_RELATIONAL_SQL_INCREMENT6_RESULTS.md` has the
certification matrix. No SQL execution exists.

#### Added

- `sql/` (crate `rubixdb-sql`): `ast`/`convert`/`parse`/`bind`/`bound`/
  `auth`/`functions`/`temporal`/`error`/`limits`/`metrics` modules.
  Depends on `sqlparser = "=0.63.0"` (Apache-2.0, pinned exact) and the
  core `rubixdb` crate; never the reverse.
- Supported grammar: `SELECT` (`INNER`/`LEFT JOIN`, `WHERE`, `ORDER BY`,
  `LIMIT`/`OFFSET`, `DISTINCT`, wildcards), `INSERT`/`UPDATE`/`DELETE`,
  `CREATE`/`DROP TABLE`/`INDEX`/`SCHEMA`/`DATABASE`, `EXPLAIN`, `BEGIN`/
  `COMMIT`/`ROLLBACK` -- every internal AST statement variant documents
  its own PARSED/BOUND/NOT-EXECUTABLE-YET boundary.
- The binder (`bind::scope::resolve_table`) resolves identifiers and D25
  authorization together: a nonexistent object and a forbidden-but-
  existing one produce the identical error, never distinguishable.
- `benches/sql_parser_bench.rs`, `benches/sql_binder_bench.rs` --
  measured, not claimed (binder latency scales linearly with catalog
  table count, traced to `CatalogService::list_tables`'s pre-existing
  full-scan design; no cache added, per the directive's own "do not
  introduce caching automatically").
- 98 new tests: parser correctness, resource-limit boundaries,
  `proptest` fuzzing, binder integration, SQL-injection/authorization-
  bypass security tests, and an independent reference-model
  differential test for column resolution.

#### Fixed / Found

- A long flat chain of binary operators (`1 + 1 + 1 + ...`) bypasses
  `sqlparser`'s own parse-time recursion guard (Pratt-parsed
  iteratively, never recursing) while still building a deep `Box<Expr>`
  tree whose ordinary recursive `Drop` overflows the stack --
  reproduced deterministically (a 20,000-term chain crashed the process,
  not a returned error) and closed with a pre-parse operator-density
  check, the only mitigation available without modifying the third-party
  parser.
- `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md` §1 promised an "Identifier
  Rules" section in the ADR that was never written -- supplied in
  `PHASE_RELATIONAL_SQL_GRAMMAR.md` §9, using the case-folding behavior
  the Architecture doc's own prose already specified.

### Relational database: Increment 5 (production secondary indexes, online `CREATE INDEX`) (2026-09-23)

Adds real, persistent, online-buildable secondary indexes on top of the
certified catalog and row-storage foundation. `PHASE_RELATIONAL_INDEX_
BACKFILL_ADR.md` is the full decision record.

#### Added

- `src/relational/index_key.rs`: order-preserving, NULL-aware, composite
  index-entry key encoding/decoding (a 1-byte presence tag per indexed
  column: NULL sorts before every real value), whole-index/prefix/
  arbitrary-`Bound` physical range construction.
- `src/relational/index.rs`: `IndexBuilder` -- online `CREATE INDEX`
  (snapshot-bounded, chunked backfill running concurrently with ordinary
  writes; atomic `Building -> Ready` activation), `DROP INDEX` (bounded,
  resumable physical sweep), crash recovery (`recover_incomplete_
  builds`/`recover_incomplete_drops`, both restart-from-scratch, never
  silently promote), `index_lookup`/`index_range_scan` (real index-then-
  fetch, never a table-wide scan), bounded-cardinality stats.
- `TableStore::epoch_lock`: a per-table `RwLock<()>` that (a) makes the
  `Building`/`Dropping` catalog-state transition atomic with respect to
  every ordinary writer's own maintained-index-set resolution, and (b)
  makes each backfill chunk's final commit atomic with respect to
  concurrent maintenance of the same rows -- together, these close a
  "missed write" race and a "phantom entry" race (a stale backfilled
  write resurrecting an entry for a row deleted mid-build), both proven
  in the ADR and covered by deterministic, barrier-synchronized tests.
- `catalog::schema::IndexState` extended to `{Ready, Building, Failed,
  Dropping}` (`Active` renamed `Ready`, same on-disk tag `0`); new
  `CatalogService` transitions `mark_index_ready`/`mark_index_failed`/
  `mark_index_dropping`/`remove_index_row`/`list_indexes_in_state`;
  `MAX_INDEXES_PER_TABLE`/`MAX_COLUMNS_PER_INDEX` resource limits.
- `benches/secondary_index_bench.rs`: backfill throughput, index lookup/
  range scan vs. full table scan, write amplification vs. indexed-column
  count -- measured, not claimed (index equality lookup ~467x faster
  than an equivalent full scan at 1-in-5,000 selectivity on this
  machine; write latency flat across 0-10 indexes, `fsync`-dominated,
  matching the row-storage increment's own identical finding).

#### Fixed

- `relational::key::table_row_range` bounded a table scan by the entire
  `table_id` key prefix rather than just the `index_id = 0` slot -- a
  latent defect (harmless before secondary indexes existed under the
  same prefix, a real correctness bug the instant they do) found by this
  increment's own tests, not by inspection. Fixed to bound exactly
  `[table_id||0, table_id||1)`; the fix also happens to remove an
  existing `u32::MAX`-`table_id` `Bound::Unbounded` special case,
  replacing it with a precise `Bound::Excluded` in every case.

### Relational database: Increment 4 (row-storage foundation) (2026-09-22)

Connects the certified catalog (Increment 3, `d66029d`) to actual user-
table row storage. Before writing code, `RELATIONAL ADR AMENDMENT 003`
resolved the increment's own open points: exact order-preserving key
transforms for every D4 type (byte-level, property-tested — including
the `-0.0`/`+0.0` canonicalization edge case and the escape-then-
terminate scheme for `TEXT`/`BLOB` inside composite keys), and how
`DECIMAL`'s precision/scale gets persisted given the catalog's existing
`data_type:u8` tag alone had no room for it.

#### Added

- **`src/relational/`** (new module: `value`, `key`, `table_store`,
  `error`): `RelationalValue`/`RelationalType` (D4's full closed type
  set), order-preserving key encoding for every type, the table-row
  physical key layout implemented exactly as already specified
  (Architecture doc §5), and `TableStore` — `put_row`/`put_rows`/
  `get_row`/`delete_row`/`scan_table`. Every mutation is exactly one
  `LsmEngine::write_batch` call, even at N=1, verified directly by
  asserting the engine's sequence counter advances by exactly one per
  call. Every read resolves the table's shape from the unmodified
  `CatalogService` — no second metadata structure.
- **`system.columns.type_params`** (`src/catalog/schema.rs`): one new,
  additive trailing field (`[precision, scale]` for `DECIMAL`/`NUMERIC`)
  — D31-licensed, not a catalog redesign; every pre-existing field
  untouched, and the full existing 44-test catalog suite re-run
  unmodified and still passing.
- `catalog::encoding`'s `RowValue` envelope (`format_version`/
  `schema_version`/`null_bitmap` header) refactored into `encode_row_
  envelope`/`decode_row_envelope`, generic over the per-domain value
  type, so catalog rows and relational rows share the identical codec —
  not two independently-maintained copies of the same on-disk format.
- 79 new tests, including property tests for ordering (integer/`BIGINT`/
  `DECIMAL`/`DATE`/`TIMESTAMP`/`REAL`/`DOUBLE`/`TEXT`/`BLOB` — proptest
  generators, not hand-picked cases), table-scan namespace isolation
  verified as actual physical range boundaries (neighboring/min/max
  `table_id`), restart persistence, concurrent access (16-thread `put_
  row`, concurrent scan during writes, a delete/read race), and a
  differential test against an independent `BTreeMap` reference model.
- `benches/table_store_bench.rs`: measured, real overhead of `put_row`/
  `get_row`/`scan_table` over raw `LsmEngine` calls (see PROGRESS.md for
  the actual numbers — the read path shows a genuine ~20x cost from
  per-call, uncached catalog resolution; the `fsync`-dominated write
  path shows no measurable difference).

#### Explicitly not implemented in this increment

No SQL parser/binder/executor, no `CREATE TABLE`/`INSERT`/`UPDATE`/
`DELETE`/`SELECT` SQL, no query planning/joins/aggregation, no index
maintenance wired into `put_row`/`delete_row` yet (deliberately shaped
to need no call-site change when added), no authorization enforcement.
`RELATIONAL DATABASE PRODUCTION READY = NO.`

#### Regression gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, `cargo test --workspace` and `--release
--workspace` (468 `rubixdb` lib tests + 30 `rubixdb-api` tests, debug
and release), `wal_tests`, `pathological_recovery_matrix`, `crash_
consistency --features test-util` — all clean, all passing, both before
and after this increment. `src/manifest/`, `src/compaction/`, `src/
sstable/`, `src/wal/`, `api/` untouched.

### Relational database: Increment 3 (persistent catalog) (2026-09-22)

Adds the persistent relational catalog on top of the certified
`write_batch` primitive (Increment 2). Before writing any code,
`RELATIONAL ADR AMENDMENT 002` resolved the catalog's own remaining open
points — per-system-table column schemas and primary keys, durable ID
allocation under concurrency (with no D10 transaction/conflict-detection
layer yet to lean on), `system_table_id` constant assignment, and this
increment's own explicitly-scoped-down DROP semantics (no table-row
storage exists yet for D13's background-sweep phase to act on).

#### Added

- **`src/catalog/`** (new module: `encoding`, `schema`, `service`,
  `error`): the seven `system.*` tables (D1) as ordinary rows in the
  certified `LsmEngine`'s own keyspace, under the reserved `0x00`
  namespace (D2). Every mutation is one `LsmEngine::write_batch` call
  (D9/D13) — `CREATE TABLE` writes its `system.tables` row, every
  `system.columns` row, and its default `PRIMARY`-kind `system.indexes`
  row atomically, in exactly one call (verified directly: the engine's
  sequence counter advances by exactly one per `create_table`, not one
  per row).
- **`CatalogService`**: `bootstrap` (idempotent — creates the single v1
  database and its `public` schema on a genuinely empty catalog, a true
  no-op otherwise), `create_schema`/`create_table`/`create_index`/
  `create_constraint`/`grant`/`revoke`, matching `get_*`/`list_*` reads
  (ordinary `range_scan`s, no separate catalog cache), and `drop_table`/
  `drop_index`/`drop_schema` (direct atomic catalog-row removal — this
  increment's own explicit slice of D13, not its full `DROPPING`-marker-
  plus-background-sweep design, since no table-row storage exists yet
  for a sweep to act on).
- **Durable, restart-safe, collision-free ID allocation**: every ID is
  read from and incremented as an ordinary catalog row (never a
  process-local counter as the source of truth), inside the same
  `write_batch` as the object it names. A new, narrowly-scoped
  `Mutex` internal to `CatalogService` serializes this process's own
  catalog-mutating calls — closing a real intra-process ID-collision
  race `write_batch`'s atomicity alone cannot close without a
  transaction/conflict-detection layer (D10, not yet implemented).
- 43 new tests: encode/decode round-trips for every system table and
  every `NULL`-bitmap boundary, namespace-isolation from pre-existing
  flat-KV keys, restart/recovery (catalog rows survive a real engine
  close/reopen; ID allocation continues from its durable value, never
  resets), concurrent `CREATE TABLE` (16 threads, zero `table_id`
  collisions; same-name races, exactly one winner), concurrent scans
  never observing a half-created table, `DROP TABLE` cascade + cross-
  table isolation, grants uniqueness, and invalid-input rejection
  (empty name, no columns, no primary key, nullable PK column,
  duplicate column names, out-of-range ordinals).

#### Explicitly not implemented in this increment

No SQL parser/binder/executor, no `CREATE TABLE` SQL syntax, no
`INSERT`/`UPDATE`/`DELETE` execution, no user-table row storage, no
authorization *enforcement* (`system.grants` rows are stored; nothing
yet checks them — that is D15's binder). `RELATIONAL DATABASE
PRODUCTION READY = NO.`

#### Regression gate

`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, `cargo test --workspace` and `--release
--workspace` (416 `rubixdb` lib tests + 30 `rubixdb-api` tests, debug
and release), `wal_tests`, `pathological_recovery_matrix`, `crash_
consistency --features test-util` — all clean, all passing, both
before and after this increment. `src/manifest/`, `src/compaction/`,
`src/sstable/`, `src/wal/`, `src/error.rs`, `api/` untouched (`git diff
--stat` empty for each) — this increment's only source changes are the
new `src/catalog/` module and one added line in `src/lib.rs`. The same
pre-existing, machine-throughput-dependent `group_commit` test pair
(`m1_2`/`m1_3`) noted in Increment 2 recurs identically — confirmed
unrelated (`tests/` has zero diff from this increment).

### Relational database: Phase 0/1 architecture audit + Increment 2 (`write_batch` storage primitive) (2026-09-22)

Begins the relational-database phase on top of the certified engine
(Write/Read/Compaction) and Service API. Phase 0/1 (read-only audit +
architecture) produced `PHASE_RELATIONAL_DATABASE_ARCHITECTURE.md`,
`PHASE_RELATIONAL_DATABASE_ADR.md`, and `PHASE_RELATIONAL_STORAGE_GAP_
ANALYSIS.md` with **zero implementation code** — the audit's one
governing finding: the certified engine has no atomic multi-key write
primitive, which every relational guarantee (transactions, index/table
consistency, DDL) depends on. A follow-up review reordered the plan: the
atomic storage primitive must be built and certified *before* the
catalog/tables/indexes. `RELATIONAL ADR AMENDMENT 001` resolved every
open question in D9's original sequence-semantics/WAL-format/atomic-
visibility/failure-semantics sketch precisely, against the actual
certified code (not a summary of it), and this increment implements
exactly what it specified — nothing else.

#### Added

- **`LsmEngine::write_batch(&self, ops: &[WriteOp]) -> Result<u64>`**
  (`src/lsm/mod.rs`): atomically applies N `Put`/`Delete` operations
  under one shared, durable sequence. Either every operation becomes
  visible to a subsequent read or none do — proven, not merely argued,
  by a concurrent-reader test racing a real `write_batch` call held
  mid-critical-section against a single-lock-acquisition `range_scan`
  covering every touched key, across 30 repeated interleavings, zero
  partial observations. Same-physical-key operations within one batch
  resolve deterministically (last-in-slice wins), via the existing
  `MemTable` map-overwrite semantics, not a new rule.
- **WAL `Group` frame** (`OP_GROUP = 5`, `src/wal/format.rs`,
  `src/wal/ops.rs`): one new, additive op tag encoding N members inside
  the existing, unmodified frame envelope. `PUT`/`DELETE`/`CHECKPOINT_
  MARKER`'s byte layout is unchanged; `wal::recovery::walk_segment`
  needed zero changes (frame classification is `op_tag`-agnostic), so
  torn-trailing-batch-discarded and corrupt-non-tail-batch-fails-closed
  are the existing, already-certified rule, extended for free. Decode
  grows its member list incrementally (`.push()`, never `Vec::with_
  capacity` from the untrusted on-disk `member_count`) — the concrete
  defense against an attacker-controlled unbounded-allocation path.
- **`LsmConfig::max_batch_ops`** (default 10,000, matching the
  relational layer's own already-decided write-set-size default):
  engine-level defense-in-depth cap, independent of whatever the
  caller checks.
- **`EngineError::InvalidArgument`** (`src/error.rs`): the one new
  error variant this increment required (an empty batch has no logical
  write to make durable) — mapped in `api/src/error.rs` to `400
  VALIDATION_ERROR`, the minimal, mechanical change needed to keep the
  workspace compiling (not an API feature addition).
- `PHASE_RELATIONAL_TRANSACTION_STORAGE_ADR.md` (narrowly scoped
  implementation-time document) and `PHASE_RELATIONAL_TRANSACTION_
  STORAGE_RESULTS.md` (full measured results: benchmarks, security
  audit, protected-path audit, regression-gate outcome).

#### Measured (not claimed)

- N=1 parity: `write_batch([Put])`/`write_batch([Delete])` show no
  material regression against `put`/`delete` (overlapping confidence
  intervals, `cargo bench --bench write_batch_bench`).
- N>1 throughput: `write_batch` time stays ~4.2–5.5 ms regardless of N
  (dominated by one `fsync`) while an equivalent serialized baseline (N
  sequential `put` calls) grows linearly — up to 57x faster at N=64.

#### Regression gate

Full existing suite re-run unmodified and passing: `cargo fmt --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo test --lib` (373 passed) and `--release --lib` (373 passed),
`wal_tests` (12), `pathological_recovery_matrix` (9), `crash_
consistency --features test-util` (2). `src/manifest/`, `src/
compaction/`, `src/sstable/` untouched (`git diff --stat` empty for
each). One pre-existing, machine-throughput-dependent `group_commit`
test pair (`m1_2`/`m1_3`) fails identically on the clean, unmodified
baseline (verified via `git stash`) — not a regression, not weakened,
not silently ignored.

**RELATIONAL DATABASE PRODUCTION READY = NO.** No catalog, schema,
table, index, SQL, transaction executor, or CLI exists yet.

### Productization: Service API + frontend console (2026-09-22)

Adds a Service API layer and a frontend console on top of the
certified engine (Write/Read/Compaction), without modifying it:
`Client -> HTTP API -> Service layer -> LsmEngine`. Router,
Replication, Partitioning, and leveled compaction remain explicitly
out of scope and are not started by this work.

#### Added

- **`rubixdb-api`** (new `api` workspace member, `axum` 0.7 + `tokio`,
  workspace change to `Cargo.toml` limited to adding the member):
  bearer API-key auth (`reader`/`admin` role hierarchy), per-principal
  token-bucket rate limiting, an `EngineError` -> HTTP status/code
  mapping that never leaks a raw `io::Error` or filesystem path into a
  response body, a snapshot-lifecycle service around the engine's own
  RAII `Snapshot`, bounded graceful shutdown with an injectable
  shutdown trigger (directly testable rather than relying on OS signal
  delivery), per-route p50/p95/p99 metrics, and a `CorsLayer` gated by
  `RUBIXDB_CORS_ALLOWED_ORIGINS` (empty/same-origin-only by default,
  never wildcards origin). Route surface: `/healthz`, `/v1/readyz`,
  `/v1/whoami`, `/v1/status`, `/v1/metadata`, KV put/get/delete/exists
  (`?as_of_seq=` historical reads), range scans, snapshot create/list/
  get/release, compaction status/metrics (read-only), combined
  service+engine metrics.
- **`GET /v1/whoami`**: a gap found while building the frontend (no
  way for a client to learn its own authenticated role) and closed as
  one new route reading the `Principal` `auth_middleware` already
  attaches to every request -- zero change to the auth model.
- **`frontend`** (new directory, React 18 + TypeScript + Vite): a
  database console -- Dashboard, Data Explorer (point lookup + range
  query), Snapshots, Compaction (status/metrics only, no manual-
  trigger control -- no such engine API exists), Health/Storage
  (per-route metrics table), Settings. Design-token light/dark
  theming, a small hand-built component set (no component-library
  dependency), `@tanstack/react-query` for server state, one
  `SessionContext` for global client state.

#### Fixed

- **Stale read cache after a write** (functional bug, found by
  `frontend/e2e/workflow.spec.ts`'s own overwrite-then-re-read step):
  `react-query` had no signal that a `PUT`/`DELETE` invalidated an
  already-cached `["kv", "get", key, ...]` query under an unchanged
  key. Fixed in `frontend/src/api/queries.ts`: `usePutMutation`/
  `useDeleteMutation` now invalidate every cached `["kv", ...]` query
  on success.
- **Two color-contrast failures** caught by `axe-core`'s automated
  audit against the real rendered app (`--color-healthy` 3.1:1,
  `--color-text-faint` 3.19:1, both below WCAG AA's 4.5:1): darkened
  in `frontend/src/styles/tokens.css`.
- **Heading-hierarchy skip** (`<h1>` page titles directly followed by
  `<h3>` `Card` titles, skipping `<h2>`), caught by `axe-core`'s
  `heading-order` rule: `Card` titles now render as `<h2>`.
- **Missing `<main>` landmark on the Connect screen** (it renders
  outside `AppShell`, before any session exists): wrapped in a labeled
  `<main>`.

All four backend fixes/additions and the frontend bug fixes above are
purely additive or corrective to the new API/frontend layer -- zero
change to `src/wal/`, `src/manifest/`, `src/error.rs`, `src/
compaction/`, or any other certified-engine path (`git diff --stat --
src/` empty across the whole phase). **WRITE ENGINE = PRODUCTION
READY**, **READ ENGINE = PRODUCTION READY**, and **COMPACTION =
PRODUCTION READY** are unchanged and re-verified (347/347 lib tests
debug+release, `wal_tests` 12/12, `crash_consistency` 2/2,
`pathological_recovery_matrix` 9/9). The new API/frontend layer's own
readiness rests on its own tests: `rubixdb-api` 29/29 unit + 15/15
real integration tests (real engine, no mocking); frontend 17/17 unit/
component tests + 10/10 real-backend Playwright e2e tests (full
workflow, role-gating, invalid-key rejection, `axe-core` a11y audit of
all 7 screens, responsive checks). Overall RubiXDB production
readiness is **not** declared by this work. Full account:
`PHASE_API_ARCHITECTURE.md`, `PHASE_API_IMPLEMENTATION.md`,
`PHASE_FRONTEND_ARCHITECTURE.md`, `PHASE_FRONTEND_IMPLEMENTATION.md`.

### Compaction: production performance + resource + endurance validation, Increment 3 (2026-09-22)

Closed every gate Increment 2 left open, with one purely additive
production-code change: `CompactionMetrics`/`LsmEngine::compaction_
metrics()` (mirrors `ReadStats`'s own cumulative-counters-plus-
snapshot shape), added because `compact_once`/`should_compact` remain
intentionally `pub(crate)` (`ADR-COMPACTION-001` Decision 13,
unchanged) and an external benchmark/soak harness otherwise has no way
to observe per-cycle stats from the real automatic worker. No trigger
model, retention rule, Manifest sequence, or concurrency model change.

Three new harnesses (`examples/compaction_bench.rs`, `compaction_
crash_cycle_test.rs`/`_child.rs`, `compaction_soak.rs`) delivered:
performance sweep across 4-256 input SSTables and a 9-shape overlap x
value-size sweep (measured on-disk storage-budget peak matched the
ADR's own theoretical `input+output` figure exactly, 0.00% delta, at
both 64 and 256 tables); concurrent read/write/compaction, snapshot,
and tombstone/version endurance (0 mismatches across ~2.37M ops / 39
cycles / 12 cycles respectively); a finding that compaction
measurably *improves* read latency (point-read p50 ~9x lower, range
p50 3-20x lower, compaction enabled vs. disabled, same workload) by
bounding live SSTable count; 38/38 real external-process crash cycles
through the automatic worker (18 targeted -- via a stdout-marker
technique precisely hitting each of the 6 `CompactionFaultPoint`s --
plus 20 random-delay); RSS growing only 9.7% while cumulative
compacted-through data grew ~668x (bounded by live input-table count,
per the ADR's own design intent); handles/threads returning **exactly**
to the pre-open process baseline after shutdown across 100 repeated
cycles.

**The first real long-duration integrated production soak with
automatic Compaction active**: 4 hours, the established production
profile (8 writers, 16 readers, `LsmConfig::default()`), ~14.9M writes,
~2.6M deletes, 3.3 billion point reads, 9.87M range scans, 193 real
compaction cycles, 17.46M records dropped, RSS stable at 60-70MB
throughout, live SSTable count never exceeding 3. **0 in-run
mismatches, 0 post-recovery mismatches** across all 20,000
independently-tracked keys, verified after a real shutdown + reopen.

Three real bugs found and fixed in the soak's own correctness harness
while building it (not in Compaction or the Read Engine): a range-
bound wraparound producing inverted/empty scans near the keyspace
boundary; a reference-model ring buffer breaking its own seq-sorted
invariant under racing same-key writers (fixed via seq-sorted
insertion); and a correctness comparison that ran into this project's
own already-documented `snapshot_seq()` cross-thread cadence caveat
(confirmed pre-existing and Compaction-unrelated by reproducing it
with zero compaction cycles running, then fixed by pinning every
comparison the same proven-race-free way point-checks already use). A
resource-contention false positive (2 unrelated `wal::group_commit`
tests, failing only under full-parallel-suite load concurrent with the
soak's own 8w/16r load on the same machine) was traced and confirmed
non-regressive before the authoritative gate was re-run cleanly with
the soak no longer active.

Full regression gate clean (347/347 debug+release, fmt/clippy,
`wal_tests` 12/12, `crash_consistency` 2/2, `pathological_recovery_
matrix` 9/9); zero changes to WAL/Manifest/error types/`src/compaction/
mod.rs`; zero new dependency; zero `unsafe`. Full detail:
`PHASE_COMPACTION_PERFORMANCE.md`, `PHASE_COMPACTION_INCREMENT3_
ENDURANCE.md`, `PROGRESS.md`'s 2026-09-22 entry.

**COMPACTION INCREMENT 3 = PASS. COMPACTION PRODUCTION READY = NO** --
final certification is a new, separately-scoped increment. Write
Engine and Read Engine production-ready status unchanged.

### Compaction: production trigger + execution integration, Increment 2 (2026-09-21)

`ADR-COMPACTION-001` Amendment 1 implemented: a real, automatic
background compaction worker (`spawn_compaction_thread`), wired to
`LsmEngine::open` behind a new opt-in `LsmConfig.compaction_auto_
trigger` flag (**default `false`**, deliberately -- reversed from an
initial `true` default after two concrete Increment-1 test failures
showed it would otherwise silently start compacting under already-
certified test surface). Dual wake source (flush-triggered
notification + periodic fallback tick, reusing the existing `storage_
pressure_retry_interval`, no new config field); `CompactionRunGuard`
(one `AtomicBool` RAII guard, the smallest primitive preventing
concurrent compactions, no global lock); `shutdown()` extended with an
explicit, tested contract (in-progress cycle always completes, never
aborted, no leak, no deadlock, no partial publish). Two real bugs
found and fixed empirically: a `shutdown()` message-loss bug
(`try_send` vs. blocking `send` on the bounded worker channel) that
caused a real multi-minute slowdown under heavy test load; and a
narrow pre-existing race in a shared test fixture helper (fixed via a
new, additive `flush_completions` observability counter). A separate,
genuinely unbounded test-design hazard (racing an already-running
worker to observe a live SSTable count) was found and fixed uniformly
across every affected test by building fixtures offline first, then
reopening with the worker enabled. 12 new tests (346/346 total,
`cargo test --lib`, debug and release, run twice post-fix with zero
flakiness): deterministic threshold firing, direct re-entrancy
stress test, storage-pressure defer/resume, failure+retry, shutdown
mid-cycle, snapshot safety, deferred-deletion retry, a bounded
production-like integration run, crash recovery through the real
automatic path, bounded resource safety, and a first bounded
performance/storage-budget/latency baseline (measured on-disk peak
matched the ADR's own theoretical `input+output` figure exactly).
Full regression gate clean; zero changes to WAL/Manifest/error types;
zero new dependency; zero `unsafe`. Full detail: `PHASE_COMPACTION_
INCREMENT2_RESULTS.md`, `PHASE_COMPACTION_ADR.md` Amendment 1,
`PROGRESS.md`'s 2026-09-21 "Compaction Increment 2" entry.

**COMPACTION CORE = PASS. COMPACTION TRIGGER INTEGRATION = PASS.
COMPACTION PRODUCTION READY = NO** -- remaining: full performance
characterization, a real OS-level resource benchmark, long-duration
endurance, storage-pressure endurance, and a final certification
matrix. Write Engine and Read Engine production-ready status
unchanged.

### Compaction: deterministic core implementation, Increment 1 (2026-09-21)

`ADR-COMPACTION-001` implemented as the deterministic core operation
only -- `LsmEngine::compact_once`/`should_compact` (no production
caller yet; no automatic trigger or background thread, by explicit
design), `LsmConfig.compaction_trigger_count` (default 4), the
engine-agnostic size-tiered full-merge k-way merge + version/tombstone
retention algorithm (`src/compaction/mod.rs`, reusing Increment 6's
persistent `SsTableRangeCursor` directly for bounded memory), and a
generalized, streaming SSTable writer entry point
(`sstable::write_from_sorted_records`) -- `write_from_memtable` is now
a thin adapter over the same shared core, verified byte-for-byte
behavior-preserving by a new differential test. A real correctness
refinement was found and fixed during implementation: the architecture
report's own worked truth table had two under-specified rows (only
correct under an unstated single-live-snapshot assumption); the
implemented, tested algorithm is the conservative, generally-correct
one, documented via an erratum rather than a silent rewrite. 26 new
tests plus 2 writer differential tests (334/334 total): correctness
differential (2,000 ops vs. an independent reference model), property
test, all 6 crash-window fault points (each a real panic + real
restart), the previously-zero-coverage orphan-recovery branch,
concurrent flush, concurrent readers, a real Windows positional-read-
after-unlink test, and storage-pressure deferral. Full regression gate
clean; zero changes to WAL/Manifest/error types; zero new dependency;
zero `unsafe`. Full detail: `PHASE_COMPACTION_INCREMENT1_RESULTS.md`,
`PROGRESS.md`'s 2026-09-21 "Compaction Increment 1" entry.

**COMPACTION PRODUCTION READY = NO** -- no automatic trigger, no
dedicated benchmark, no long-duration soak, no final certification.
Write Engine and Read Engine production-ready status unchanged.

### Read Engine: final certification (2026-09-21)

`PHASE_READ_ENGINE_CERTIFICATION.md` (new): final certification of the
single-engine, non-partitioned LSM Read Engine, commit `22be3e4`.
30-row certification matrix (point lookup through protected Write
Engine integrity) -- **30/30 PASS, 0 FAIL, 0 mandatory OPEN**. Full
protected-path audit (`git diff` against the Write Engine's own
certification baseline) confirms zero changes to WAL, Group Commit,
Batch Coordinator, Manifest, checkpoint, WAL purge, or `StoragePressure`
logic across the entire Read Engine phase. Final regression gate and a
bounded (not a new soak) performance reconfirmation both re-run clean
at certification time. Known, explicitly non-blocking limitations
documented rather than hidden: no Compaction yet (read amplification
still scales with live SSTable count), and the "no memory leak" finding
rests on source-level ownership analysis plus two converging soak
observations rather than an external memory profiler (none available
in this environment).

**READ ENGINE PRODUCTION READY = YES** -- scoped explicitly to the
single-engine, non-partitioned LSM Read Engine. Compaction, Router,
Replication, and the larger partitioned RubiXDB architecture are not
certified and do not exist in this codebase yet; full RubiXDB
production readiness is not claimed.

### Read Engine: fresh 4-hour integrated soak against the optimized implementation, Increment 7 (2026-09-21)

Re-validated `ADR-RE-002` Option A against a fresh, full 4-hour,
production-profile integrated write/read soak (identical profile to
Increment 4's own: 8 writers, 16 readers, seed 20260920), not just the
controlled `overlap_repro` benchmark. `RESULT=PASS`: 8,372,161
writes+deletes, 6,116,654 reads, 678,708 range scans (every one
checked against the independent reference model at an aged snapshot,
zero disagreed), zero in-run/post-recovery mismatches, clean recovery,
zero storage pressure events. `range_large` p50 improved **3.26x-3.89x**
against Increment 4 at matched SSTable counts (landing inside Increment
6's own 3.20x-3.69x controlled-benchmark prediction), with a flatter
growth curve (~1.25 apparent exponent vs. Increment 4's ~2.20).
`blocks_read`-based amplification improved 5.73x-8.05x at matched
counts. RSS-vs-SSTable-count fit tightened from R²=0.698 to **R²=0.984**
-- the large non-monotonic RSS swings Increment 4's own soak showed are
essentially gone under the same real workload with the bottleneck
fixed. A separate, bounded crash/recovery run (20/20 cycles) confirmed
real post-recovery read correctness. Full regression gate re-run clean.
Full detail: `PHASE_READ_ENGINE_INCREMENT7_SOAK.md` (new), `PROGRESS.
md`'s Increment 7 entry. **Increment 7 = PASS. Status unchanged: READ
ENGINE PRODUCTION READY = NO** -- final evidence consolidation,
performance validation, resource validation, and the certification
matrix remain outstanding.

### Read Engine: persistent range source cursors, `ADR-RE-002` Option A, Implementation Increment 6 (2026-09-20)

Implemented the optimization Increment 5's investigation identified and
`ADR-RE-002` proposed: `RangeScanIter`'s SSTable sources now use a
persistent, owned-`Arc` cursor (`SsTableRangeCursor`, new,
`src/sstable/reader.rs`) instead of a resume-point `Bound` plus a
fresh `range_scan_raw` call per key -- eliminating the repeated binary
search and repeated block re-read/re-decode Increment 5 traced and
reproduced. **Zero `unsafe`, zero new dependency** (owning `Arc
<SsTable>` inside the cursor sidesteps the self-referential-struct
problem structurally). Before/after benchmark on the identical,
unmodified `overlap_repro` workload (`n=7` reps/checkpoint, 5
checkpoints 20-300 SSTables): `blocks_read` (unchanged counting point)
dropped by an exact, constant **4.714x** at every checkpoint; wall-clock
p50 improved **3.20x-3.69x**. `sstables_consulted`'s semantics were
intentionally revised (once per live SSTable per scan, matching point
lookups' own convention, not once per key drawn) and documented, with a
new regression test (`lsm::tests::range_scan_source_cursor_persists_
across_keys_instead_of_reconstructing_per_key`) asserting the exact
count rather than timing. Resource-lifetime check: 400 repeated
create/consume/drop scan cycles show zero handle delta, zero thread
delta, ~0.55 KB/scan RSS noise (not a leak). Point-lookup code paths
(`get`/`get_as_of`/`contains`) untouched -- confirmed by diff, no
regression. Full regression suite (306/306 `cargo test --lib` debug and
release, `wal_tests`, `crash_consistency`, `pathological_recovery_
matrix`, `fmt`/`clippy`) clean. Full detail: `PHASE_READ_ENGINE_
PERFORMANCE.md`'s Increment 6 section, `PROGRESS.md`'s Increment 6
entry, `PHASE_READ_ENGINE_RANGE_PERFORMANCE_ADR.md` §9. **`ADR-RE-002`:
IMPLEMENTED.** **Status unchanged: READ ENGINE PRODUCTION READY = NO**
-- final corruption/recovery, integrated endurance, performance
validation, and certification matrix remain outstanding.

### Read Engine: memory + range-performance investigation, Implementation Increment 5 (2026-09-20)

Investigated three items Increment 4's completed 4-hour soak flagged
rather than silently resolved: constant `snapshots_live=50`, ~2GB
final RSS, and visibly high late-run `range_large` latency. **Range
scan latency root cause found, traced in source, and independently
reproduced**: `range_large` p50 grew from 1.15ms to 43.1 seconds over
the soak (super-linear, unlike point lookups' known-linear scaling),
traced to `RangeScanIter`'s per-key `refill` re-peeking every source
holding a version of each winning key -- on this project's own
realistic (small-cardinality, heavily-overwritten) endurance workload,
this is O(distinct keys yielded × live SSTable count). Reproduced
exactly in a new, deterministic ~3-minute benchmark
(`examples/read_engine_bench.rs`'s `overlap_repro` section):
`sstables_consulted/sstable` pinned at a constant integer across five
SSTable-count checkpoints. `PHASE_READ_ENGINE_RANGE_PERFORMANCE_ADR.md`
(new, ADR-RE-002) evaluates four fix options and proposes persistent
source cursors (an owned-`Arc` iterator refactor, no `unsafe`, no new
dependency) for a *future* increment's decision -- **no optimization
implemented this increment**. **No memory leak found**: RSS's
monotonic growth is fully explained by per-SSTable index/bloom-filter
metadata (expected pre-Compaction); non-monotonic swings are most
plausibly (not profiler-confirmed) Windows working-set volatility; the
constant `snapshots_live=50` was verified to be the test harness's own
deliberate pool cap, not an engine-side leak -- no snapshot semantics
changed. Full detail: `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md`,
`PROGRESS.md`'s 2026-09-20 "Increment 5" entry. Certification status
kept distinct rather than collapsed: correctness PASS, performance
OPEN, memory OPEN-but-no-leak-found. **Status unchanged: READ ENGINE
NOT READY.**

### Read Engine: long-duration read soak + integrated write/read endurance, Implementation Increment 4 (2026-09-20)

A real 4-hour soak (`examples/read_write_soak_test.rs`, new) under 8
concurrent writers + 16 concurrent readers against the full real stack
(WAL, MemTable, SSTables, Manifest, checkpoint, WAL purge),
continuously validated against an independent reference model.
`RESULT=PASS`: 13,483,811 writes, 3,375,298 deletes, 4,582,352 reads,
261,455 range scans, zero in-run or post-recovery mismatches, clean
recovery. Also: a mid-session (no restart) corruption-injection test
(`src/lsm/tests.rs`) and `lsm_crash_cycle_test.rs` extended from
"open() returned Ok" to real post-recovery read verification against
exact expected values. Full detail: `PROGRESS.md`'s 2026-09-20
"Increment 4" entry. **Status: READ ENGINE NOT READY** -- three
observations from the soak (RSS growth, constant snapshot count, late-
run range latency) were flagged, not silently resolved, and carried
forward into Increment 5 above rather than assumed benign.

### Read Engine: contains() + performance baseline, Implementation Increment 3 (2026-09-20)

`LsmEngine::contains(key, as_of_seq) -> Result<bool>` (`ADR-RE-001`
§2/§10), backed by a new `SsTable::contains_versioned` that reuses
`get_versioned`'s bloom+index+block walk without constructing an owned
value. 11 new tests (304/304), including two genuine (not simulated)
I/O-failure regression tests across `contains`/`get_as_of`/
`range_scan`, and the differential/property tests extended in place to
a three-way `reference model == get_as_of == contains` invariant.
`examples/read_engine_bench.rs`: a new, real (unmocked) performance
harness; full results in the new `PHASE_READ_ENGINE_PERFORMANCE.md`.
Headline, honestly-reported findings: **`contains()` shows no
measurable performance difference from `get_as_of(..).is_some()`** at
any value size or SSTable count tested (traced to why: the block
decode this method hoped to skip already happens unconditionally);
**point-lookup read amplification scales roughly linearly with live
SSTable count** (no Compaction yet to bound it), dominated by cheap
bloom-negative checks rather than disk I/O; `range_scan`'s
bounded-memory design confirmed with a real number (168KB peak RSS
growth over a 25MiB scan); and a rare, fully-traced, pre-existing
cross-thread `snapshot_seq()` characteristic (not a `contains()` bug,
not a protected-code change made or needed) flagged for future
investigation rather than silently resolved. No cache/mmap/prefetch/
parallel-read/secondary-index optimization added -- per the phase
brief, this increment measures only. Full detail: `PROGRESS.md`'s
2026-09-20 "Increment 3" entry, `PHASE_READ_ENGINE_PERFORMANCE.md`.
**Not** Read Engine production-ready.

### Read Engine: production-grade range_scan, Implementation Increment 2 (2026-09-20)

`LsmEngine::range_scan(start, end, as_of_seq)`/`range(start, end)`: a
real binary-heap k-way merge across active + immutable MemTables + live
SSTables, built on Increment 1's `ReadView`/`Snapshot`/`ReadStats`
foundation, per `ADR-RE-001`. Lazy, ordered, bounded-memory, exactly one
resolved value per logical key, tombstones suppressed, fail-closed on
corruption. Fixed one real, pre-existing bug along the way (test-first,
per the ADR's own explicit authorization): `MemTable::range`'s
`Excluded` bound never actually excluded the boundary key's own
entries. Two more real bugs were caught and fixed before this shipped
by actually running the new tests: a mid-group corruption `Err` was
being silently swallowed instead of propagated, and `BTreeMap::range`
panics (rather than returning empty) on `start > end` or degenerate
`Excluded==Excluded` bounds. 20 new tests (293/293 total), including a
64-case property test against an independent reference model. Full
detail: `PROGRESS.md`'s 2026-09-20 "Increment 2" entry. **Not** Read
Engine production-ready -- that certification has not started.

### Read Engine: architecture report, ADR-RE-001, Implementation Increment 1 (2026-09-20)

New, separately-scoped phase (Write Engine is certified and protected,
unchanged). `PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md` and
`PHASE_READ_ENGINE_ADR.md` (13 resolved decisions) precede any code.
Increment 1 (foundation only, no `range_scan` yet): `Snapshot`/
`SnapshotRegistry` (a real, `Drop`-released, multiset-correct read
watermark, forward-compatible with a future Compaction's `snapshot_
refs` needs), `ReadStats` observability (`read_requests`/`read_hits`/
`read_misses`/`bloom_negatives`/`blocks_read`/`sstables_consulted`,
plus two new counters on `SsTable`), and a `ReadView` foundation type
(`Arc`-clones existing sources, never duplicates a Bloom filter or
index, never copies a whole MemTable). `get`/`get_as_of` gained only
non-functional counter increments -- no behavior change. 17 new tests
(273/273 total), including a deterministic concurrent-flush point-read
test (existing `FlushFaultPoint` machinery, no sleeps) and a
previously-missing lazy-data-block-corruption regression test. Three
real bugs caught and fixed by running these tests before trusting them
(a `Sync`-bound compile error, two freeze-count miscalibrations, and a
real pre-existing `MemTable::range` `Excluded`-bound discrepancy,
explicitly recorded for the next increment). Full detail:
`PROGRESS.md`'s 2026-09-20 Read Engine entry.

### Write-Engine Certification: RSS growth investigated and explained; final decision reaffirmed (2026-09-20)

The soak certified below showed RSS growing +2,334% over its 4 hours.
That was flagged and fully investigated rather than certified past on
trust: traced to source (`SsTable::open()` retains a Bloom filter +
sparse index per open table for the engine's lifetime; no Compaction
exists yet to reclaim old tables) and confirmed by two independent
measurements fitting a near-perfect linear model against SSTable count
(R²=0.9999) — classified expected, bounded-per-table growth, not a
leak. `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` (new doc). A
regression test locking in the underlying ownership invariants was
added (`lsm::tests::sstable_count_and_immutable_memory_track_flushes_
exactly_no_extra_retention`), and the harness now reports
`min_rss_kb_observed`/`max_rss_kb_observed`, not just start-vs-end.
Performance acceptance was also broadened from one 3-rep set to 15
reps across 3 sessions before being trusted — median clears both hard
targets, with real, already-documented, non-blocking run-to-run
variance reported in full rather than the favorable subset. Final
certification verdict unchanged: **WRITE ENGINE PRODUCTION READY**,
now with a complete 16-gate matrix (`PHASE_WRITE_ENGINE_CERTIFICATION.md`).

### Write-Engine Certification: FINAL DECISION -- WRITE ENGINE PRODUCTION READY (2026-09-20)

The realistic full-pipeline endurance soak (200 writers, 14,400s,
`LsmConfig::default()`) was re-run on a properly provisioned `E:`
volume, per a documented storage budget (`PHASE_WRITE_ENGINE_
STORAGE_BUDGET.md`), and passed clean: `completed_err=0` throughout,
throughput sustained 19,332-26,116 ops/sec with no collapse, 3,294
SSTables published, checkpoint advancing continuously, 0 ENOSPC events.
Fresh 100w/1000w acceptance benchmarks both cleared their hard targets.
Final certification decision: `PHASE_WRITE_ENGINE_CERTIFICATION.md`
(new doc). A real bug in the soak harness's own PowerShell PASS/FAIL
logic (`-notmatch` array-filtering semantics, producing a false FAIL
despite a genuinely healthy run) was found and fixed, verified by
replaying the corrected logic against both this run and the original
failed run before trusting it (`temp/realistic_soak_harness.ps1`).

### Write-Engine Certification: storage-pressure / ENOSPC handling (ADR-WE-SP-001 -- implemented and verified by the re-soak above)

#### Fixed

- The background flush thread's retry loop (`spawn_flush_thread`,
  `src/lsm/mod.rs`) used `max_flush_retries` only to pick a backoff
  duration, never as an actual retry limit: past that budget it fell
  into an **unconditional, unbounded** flat 2-second retry cadence for
  every kind of I/O failure, including a genuinely permanent disk-full
  condition. The 2026-09-19 realistic full-pipeline soak (200 writers,
  `LsmConfig::default()`) hit this exact path when its target volume
  filled at t≈5,100s and spent the remaining ~9,200s of the run
  retrying a doomed flush every 2 seconds instead of failing safe --
  `completed_err` reached 580,190,298, throughput collapsed 97.9%. See
  `PHASE5_ENOSPC_FAILURE_ANALYSIS.md` for the full incident analysis
  (including a from-source proof that the huge `completed_err` number
  was not an accounting bug) and `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md`
  for the fix design and its "Implementation Notes" section for exactly
  what landed. Durability/crash-recovery correctness were never
  affected by the original defect -- this was purely an availability/
  retry/backpressure gap.
- The realistic-soak certification harness (`temp/realistic_soak_harness.ps1`)
  reported PASS on `exit_code == 0` + a clean process tree alone, which
  is how the above defect went uncaught. It now additionally requires
  `completed_err == 0`, no persistent throughput collapse, no ENOSPC/
  retry-storm lines in stderr, a successful recovery line, and a clean
  drained shutdown.

#### Added

- `EngineError::StorageExhausted` (`src/error.rs`) -- a new, additive
  error variant distinct from the generic `Io` variant and from the
  pre-existing `CapacityExceeded` (MemTable-freeze backpressure, a
  different failure mode, contract unchanged). Returned by `LsmEngine::
  put`/`delete` before any WAL append is attempted, once storage is
  confirmed exhausted.
- `lsm::StorageState` (`Healthy` / `StoragePressure` / `StorageFull`),
  an explicit storage-health state machine on `LsmEngine`
  (`storage_state()`, `storage_pressure_events()`), plus
  `LsmConfig::storage_pressure_retry_interval` (default 5s) -- the
  backoff used once a flush's bounded fast-retry budget is exhausted on
  a confirmed ENOSPC-classified failure, replacing the old flat-2s-
  forever cadence for that specific case.
- `LsmEngine::install_flush_io_fault_hook`/`clear_flush_io_fault_hook`,
  a test-only fault-injection point (extends the existing `install_
  flush_fault_hook`/`FlushFaultPoint` pattern to actually substitute a
  real I/O outcome, not just observe) used by the new deterministic
  ENOSPC test below without ever touching real disk capacity.
- Two new tests: `lsm::tests::storage_pressure_state_machine_recovers_after_injected_enospc`
  (in-process, walks the full `Healthy` -> `StoragePressure` ->
  `StorageFull` -> `Healthy` sequence) and `examples/
  storage_pressure_crash_{child,test}.rs` (external-process, kills the
  child while genuinely stuck in `StorageFull` and verifies clean
  recovery -- 10/10 cycles clean).

### Phase 5: RUBIC Manifest (MANIFEST NOT READY FOR COMPACTION -- blockers remain)

Extends the persistent architecture: `... -> RUBIC SSTable -> Manifest
-> Safe WAL Checkpoint/Purge`. Ran the release-gate audit first
(Phase 3C's still-outstanding long soak relaunched at the end of this
phase's own work, after catching and correcting a sequencing mistake
mid-session; the Phase 4B 100-writer anomaly investigated via a
dedicated ablation and conclusively narrowed, not fully explained).

- **`RUBIC_MANIFEST_FORMAT_SPECIFICATION.md`**/**`PHASE5_MANIFEST_
  ARCHITECTURE.md`** (new): the Manifest format was already fully
  specified by the LSM Engine Spec (three edit types, WAL-frame-format
  reuse) -- the real design work was the Manifest-free-to-Manifest-
  authoritative integration: a two-phase recovery split that preserves
  the existing WAL lock-ordering constraint, and the full ten-step
  publish -> checkpoint -> purge sequence.
- **`src/manifest/`** (new): independent (byte-compatible, not shared-
  code) frame implementation, sequential bounded-memory replay with the
  WAL's own torn-vs-corrupt classification, idempotent recovery. Wires
  up the WAL's own `CHECKPOINT_MARKER` op (defined since Phase 4A,
  inert until now) for the first time.
- **`src/lsm/mod.rs`** (extended): the flush pipeline now durably
  publishes, checkpoints, and purges in the exact safe order the WAL
  and LSM specs jointly require; the read path is now Manifest-
  authoritative, never "every `.sst` file found in the directory."
- **A real idempotent-retry bug found and fixed by this phase's own
  crash-cycle testing**: a retried flush attempt could durably resubmit
  a second `CHECKPOINT_MARKER` for one logical flush -- caught by an
  exact-accounting invariant added to the crash harness, not by
  inspection. Fixed via per-step (not just per-SSTable) idempotence
  tracking. Full account: `PHASE5_ADR.md` ADR-P5-4.
- **Flush-thread panic handling** (new): each flush attempt now runs
  inside `catch_unwind`, treated identically to an I/O failure by the
  same proven idempotent-retry machinery -- not a supervised-restart
  thread design, which the operating brief itself flagged as risky.
- **`LsmEngine::recovery_stats()`/`checkpoint_seq()`/Manifest
  inspection accessors** (new): real observability, added because the
  crash test's own exact-accounting invariant needed it.
- **`examples/manifest_soak_test.rs`** (new): a bounded (~3 minute)
  soak with periodic real process kills -- 8/8 cycles clean, WAL byte
  count stayed at exactly 0 across every measurement (checkpoint
  tracked within ~1% of `highest_seq` throughout).

253/253 lib tests pass (216 + 37 new: 32 in `src/manifest/`, 5 new
`LsmEngine` Manifest-integration tests), clippy and fmt clean. Full
design: `PHASE5_ARCHITECTURE.md`/`PHASE5_MANIFEST_ARCHITECTURE.md`;
failure model: `PHASE5_FAILURE_MODEL.md`; decisions: `PHASE5_ADR.md`;
performance: `PHASE5_PERFORMANCE.md`; results and final decision
(authoritative): `PHASE5_TEST_RESULTS.md` -- **MANIFEST NOT READY FOR
COMPACTION -- BLOCKERS REMAIN** (Phase 3C's own WAL certification still
never completed; the true multi-hour Phase 5 soak not yet complete).
No correctness defect found; nothing tested this phase needs to be
redone once those two items close.

### Phase 4B: RUBIC SSTable (RUBIC SSTABLE READY FOR MANIFEST)

Extends the write path: `... -> MemTable -> Immutable MemTable -> RUBIC
SSTable`. Began explicitly before Phase 3C's own WAL certification had
completed (still "Deferred") and while Phase 4A's own certification
remained "NOT YET READY -- BLOCKERS REMAIN" -- documented, provisional
basis: this phase touches no WAL/coordinator internals either, and its
own required benchmark (below) closes one of Phase 4A's two blockers
directly.

The Manifest is explicitly out of scope this phase (a genuine stop-and-
ask decision was made about the resulting WAL-purge/replay-boundary gap
-- see `PHASE4B_ADR.md` ADR-P4B-1): SSTable is a purely additional,
purely derived read-path source; the WAL is never purged/truncated by a
flush, so a corrupt or missing SSTable can never cause data loss this
phase, only reduced read-path availability (`LsmEngine::open` fails
closed on a corrupt discovered SSTable rather than silently degrading).

- **`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`** (new): consolidates the
  already-final LSM-spec byte layout (magic `"RBXSST01"`, CRC32C,
  4096-byte target blocks, 10-bits/key XXH64 bloom filter, 72-byte
  footer) and resolves the Manifest-free decisions this phase needed
  (directory-scan id recovery, "exists and validates" liveness, reused
  WAL `fsync_dir` platform primitive).
- **`src/sstable/`** (new): `format.rs`/`bloom.rs`/`writer.rs`/
  `reader.rs` -- byte-exact encode/decode, atomic tmp-file-then-rename
  publication, bounded-memory reader (index/bloom eager, data blocks
  lazy, lock-free concurrent positional reads). New dependency:
  `xxhash-rust` (pure Rust, zero transitive deps -- the spec-mandated
  XXH64 hash for the bloom filter).
- **`src/lsm/mod.rs`** (extended): background flush thread draining
  `immutables` into published SSTables; read path (`get`/`get_as_of`,
  now fallible) extended to check `active -> immutables -> sstables` in
  recency order; bounded flush retry; a test-only flush-delay hook.
- **A real correctness bug found and fixed**: `SsTable::get_versioned`'s
  `binary_search_by` could skip earlier blocks holding older versions
  of a key whose version run spans a block boundary (a tie-breaking gap
  `binary_search_by` doesn't guarantee against) -- found by this
  phase's own property test, fixed via `partition_point`. Full account:
  `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §2.6, `PHASE4B_TEST_RESULTS.md`
  §7.
- **`examples/sstable_flush_crash_child.rs`/`sstable_flush_crash_test.rs`**
  (new): 140/140 real external-process-kill crash cycles (two seeds),
  zero failures, against the flush pipeline specifically.
- **`examples/sstable_bench.rs`/`lsm_flush_load_test.rs`** (new):
  SSTable write 102.40 MB/sec / 1.38M records/sec, point lookup
  p50=12µs/p99=34µs; the WAL-only vs. WAL+MemTable vs.
  WAL+MemTable+SSTable-flush comparison Phase 4A's own `ADR-P4A-6`
  deferred -- at the LSM spec's realistic 4 MiB default memtable, flush
  overhead at 1,000 writers is within noise of the WAL-only baseline.

216/216 lib tests pass (170 + 46 new: 43 in `src/sstable/`, 3 new
`LsmEngine` flush-integration tests), clippy and fmt clean. Full design:
`PHASE4B_ARCHITECTURE.md`; failure model: `PHASE4B_FAILURE_MODEL.md`;
decisions: `PHASE4B_ADR.md`; performance: `PHASE4B_PERFORMANCE.md`;
results and final decision (authoritative): `PHASE4B_TEST_RESULTS.md`
-- **RUBIC SSTABLE READY FOR MANIFEST**, conditioned (exactly as Phase
4A's own certification was) on Phase 3C's long-soak certification
eventually landing clean.

### Phase 4A: MemTable + RUBIC format foundation (MEMTABLE NOT YET READY -- blockers remain)

Extends the write path: `Logical Writers -> Dedicated Batch Coordinator
-> Group Commit -> Durable WAL -> MemTable`. Began explicitly before
Phase 3C's own WAL certification had completed (its long soak was
still running) -- documented basis: this phase touches no WAL/
coordinator internals.

- **`RUBIC_FORMAT_SPECIFICATION.md`** (new): the RUBIC storage-format
  family's governance layer. Not Parquet. Not a renaming of the
  existing WAL. References (does not re-invent) the already-specified
  RUBIC SSTable byte layout; genuinely undecided items marked
  `UNDEFINED -- RESERVED FOR SSTABLE DESIGN`.
- **`src/memtable/mod.rs`** (new): `MemTable`/`MemtableValue`, exactly
  per `RubixDB-LSM-Engine-Specification-v1.0.md` §1 ("Status: Final") --
  `BTreeMap<(Vec<u8>, u64), MemtableValue>`, `get_as_of` via
  `range(...).next_back()`, documented size accounting, compile-time-
  enforced `freeze() -> Arc<MemTable>`. No `SkipList` evaluation: the
  spec leaves no degree of freedom there.
- **`wal::replay_streaming`** (new, additive): bounded-memory WAL
  replay, implementing the callback-replay direction `PHASE3C_ADR.md`
  ADR-P3C-1 already analyzed. `open_for_recovery`/`WalReplayResult`/
  `walk_segment`/`scan_directory` unchanged. Fixed a real same-process
  lock-ordering bug found while wiring this up (a not-yet-created WAL
  directory now correctly replays as empty rather than erroring).
- **`src/lsm/mod.rs`** (`LsmEngine`, new): Phase-4A-scoped write-path
  facade -- `put`/`delete`/`get`/`get_as_of`, WAL-durability-before-
  MemTable-apply ordering enforced and verified (a direct fsync-failure
  test, plus 25/25 real external-process-kill crash cycles each showing
  `active_entries == highest_sequence == durable_through` exactly).
  Freeze-to-immutable with bounded backpressure (`EngineError::
  CapacityExceeded`).
- **`examples/lsm_crash_cycle_child.rs`/`lsm_crash_cycle_test.rs`**
  (new): real external-process-kill crash tests at the WAL/MemTable
  boundary, mirroring Phase 3C's own proven design.
- **`examples/memtable_bench.rs`/`lsm_load_test.rs`** (new): performance
  harnesses. MemTable-only measured cleanly (1.45M puts/sec, get
  p50=400ns); the full WAL-vs-WAL+MemTable comparison explicitly
  deferred, not fabricated, after a smoke-scale attempt showed clear
  contamination from the still-running background soak.

170/170 lib tests pass (130 + 40 new: 13 MemTable unit tests, 2
property tests x 1,000 cases, 6 `replay_streaming` tests, 19 `LsmEngine`
tests), clippy and fmt clean. Full design: `PHASE4A_ARCHITECTURE.md`/
`PHASE4A_MEMTABLE_ARCHITECTURE.md`; failure model: `PHASE4A_FAILURE_
MODEL.md`; decisions: `PHASE4A_ADR.md`; results and final decision
(authoritative): `PHASE4A_TEST_RESULTS.md` -- **MEMTABLE NOT YET READY
FOR RUBIC SSTABLE IMPLEMENTATION -- BLOCKERS REMAIN** (the WAL-vs-
WAL+MemTable performance comparison is not run; Phase 3C's own WAL
certification had not completed). No correctness defect found; nothing
tested this phase needs to be redone once those two items close.

### Phase 3C: final WAL/coordinator release certification (long soak in progress)

Targets Phase 3B's own six named blockers directly.

- **`GroupCommitter`/`BatchCoordinatorPool::purge_before`** (new):
  mirrors the existing `rotate()` wrapper, delegating to `FileWal::
  purge_before` under the `wal` lock. Safe to call concurrently with
  ongoing writes. Enables realistic checkpointing during a genuinely
  long soak.
- **`examples/crash_cycle_child.rs` + `crash_cycle_test.rs`** (new):
  periodic forced-crash-during-soak testing via a real external
  process kill (`Child::kill()`, randomized/seeded/reproducible delay)
  against a real child process running the production
  `BatchCoordinatorPool` — a genuinely new, asynchronous, uncooperative
  fault-injection class. 40/40 cycles recovered cleanly, zero
  corruption, monotonic gap-free sequences.
- **`examples/long_soak_test.rs`** (new): extends Phase 3B's
  `soak_test.rs` with periodic checkpointing and CPU sampling
  alongside RSS. A true 4-hour-per-writer-level run (100w then 1000w)
  launched against the production `BatchCoordinatorPool`.
- **`tests/pathological_recovery_matrix.rs`** (new): 9 consolidated
  fixture tests against the existing, unmodified recovery contract —
  9/9 pass, including two genuinely new corruption classes beyond
  Phase 0/1's own coverage.
- **`examples/recovery_memory_scaling.rs`** (new): quantifies
  `PHASE3B_ADR.md` ADR-P3B-5's finding with real swept data (1M-15M
  records) — RSS scales linearly at ~134 bytes/record, recovery
  throughput stays flat regardless of scale. `PHASE3C_ADR.md`
  ADR-P3C-1 analyzes (does not implement) a future streaming/callback/
  bounded-batch recovery API redesign.
- **`BatchCoordinatorStats::{bytes_total, writes_timed_out}`** (new):
  two more genuine, low-contention observability fields.
- Security/dependency review completed: zero `unsafe` code, zero
  payload logging in any Phase 3C addition; `Cargo.lock` fully
  reviewed (no new production dependency); `cargo-audit`/`cargo-deny`
  not installed (crates.io network access unavailable this session,
  decision documented).

130/130 lib tests pass, clippy and fmt clean. Full design:
`PHASE3C_ARCHITECTURE.md`; failure model: `PHASE3C_FAILURE_MODEL.md`;
decisions: `PHASE3C_ADR.md`; results and current status (authoritative
— the long soak, final benchmark comparison, and final certification
decision were still in progress at this entry's own commit time):
`PHASE3C_TEST_RESULTS.md`.

### Phase 3, Increment 3B: coordinator fault matrix + resource/rotation/shutdown hardening (soak run complete — PHASE 3B INCOMPLETE, blockers remain)

Completes the coordinator-level half of Phase 3's production-hardening
scope, distinct from Increment 3A's `GroupCommitter`-level leader-panic
fix.

- **`CoordinatorFaultPoint`** (new, `src/execution/batch_coordinator.rs`):
  7 deterministically injectable points in the Dedicated Batch
  Coordinator's own batch-processing loop (`BeforeBatchFormation`,
  `AfterDrain`, `AfterAppend`, `BeforeAwaitDurable`, `AfterDurable`,
  `BeforeCompletion`, `DuringShutdown`), plus `install_coordinator_
  fault_hook`/`clear_coordinator_fault_hook` (`test-util`-gated).
- **Fixed a real completion-safety gap**, found while wiring up the
  `AfterDrain` test: `process_batch` previously only protected a
  dequeued entry with a `CompletionGuard` once the append loop
  individually reached it — a coordinator panic between dequeue and
  that point would have dropped every entry in the batch with callers
  hanging forever. Every entry now gets its guard as `process_batch`'s
  first action.
- **`queued_bytes` accounting hardened to saturating arithmetic** across
  `batch_coordinator`/`leader_drain`/`sharded_ingress`/`write_pool` —
  consistency fix, not currently exploitable, matching this project's
  own `wal_test.md` §3.7 precedent.
- **New tests**: large-payload byte accounting (deterministic barrier),
  rapid submit/shutdown cycling, frequent rotation under sustained load
  through the full production path, shutdown racing active submission,
  and 7 coordinator-panic tests (one per fault point).
- **Observability**: `BatchCoordinatorStats::{queue_capacity,
  queued_bytes_capacity}`, `GroupCommitStats::{highest_sequence,
  segment_rotations}` (new fields, zero new contention). Full metric-
  list audit and explicit gap accounting: `PHASE3B_TEST_RESULTS.md` §7.
- **`examples/soak_test.rs`** (new): sustained-workload harness against
  the production `BatchCoordinatorPool` with low-contention per-thread
  latency sampling and periodic aggregation, RSS tracking, and a
  start/mid/end drift comparison. Run for 900s (15 min) at both 100 and
  1,000 writers — write path clean at both levels (zero errors/timeouts,
  flat RSS, no degradation trend).
- **A genuine finding, not a Phase 3B-introduced defect**: the
  1,000-writer soak's own post-run recovery-verification step (not the
  write path) was killed by a real host out-of-memory condition while
  `FileWal::open_for_recovery` materialized ~85M records into one `Vec`
  — this project's existing (Phase 0) recovery API has no streaming
  variant, and its memory demand scales with WAL size. Investigated,
  confirmed correct at reduced scale by a supplementary run, documented
  precisely (`PHASE3B_ADR.md` ADR-P3B-5) rather than hidden; the harness
  now warns before repeating it. Fixing the underlying API is out of
  this phase's scope.
- Final post-hardening benchmark: no measurable regression (100w 16,133
  ops/sec, 1000w 91,208 ops/sec — both within the pre-established
  historical noise band and comfortably above target).

128/128 lib tests pass (117 + 11 new), clippy and fmt clean, zero
regressions. Full design: `PHASE3B_ARCHITECTURE.md`; failure model:
`PHASE3B_FAILURE_MODEL.md`; decisions: `PHASE3B_ADR.md`; performance:
`PHASE3B_PERFORMANCE.md`; results and final verdict: `PHASE3B_TEST_
RESULTS.md` — **PHASE 3B INCOMPLETE — BLOCKERS REMAIN** (six explicit,
named gaps against the operating brief's full scope; see that
document's §11 for the complete list and recommendation).

### Phase 3, Increment 3A: leader-failure P0 fix

Fixes a real availability gap `PHASE2B_FAILURE_MODEL.md` §3 diagnosed
but did not fix: a leader thread panicking mid-batch left
`GroupCommitter`'s `leader_active` flag (`src/wal/group_commit.rs`)
stuck `true` forever, degrading every future caller (on any
architecture — Approach A/B/C, or a direct caller) to a repeated-timeout
failure mode instead of a clean, bounded error.

- **`LeaderFailureGuard`** (new, `src/wal/group_commit.rs`): an RAII
  guard, armed the instant a caller is elected leader, disarmed only
  once `run_as_leader` returns normally. If the leader thread instead
  panics, the guard's `Drop` clears `leader_active` and poisons the
  committer during the unwind itself — mirrors `execution::common::
  CompletionGuard`'s existing pattern, not a new abstraction.
- **`PoisonReason`** (new enum, replaces `BatchState::poisoned`'s
  previous bare `io::ErrorKind`): `FsyncFailed(io::ErrorKind)` (the
  original Phase 1 poisoning path, unchanged) or `LeaderPanicked` (new).
  The `Err` a poisoned `GroupCommitter` returns now says which.
- **`GroupCommitter::is_poisoned() -> bool`** (new, public): observability
  accessor: poisoning was already externally observable via `await_
  durable`'s `Err`; this makes it queryable without a live batch.
- No change to the WAL format, `durable_through`'s semantics, sequence
  allocation, rotation, or `FileWal`'s recovery contract. No new
  dependency, no `unsafe` code. Recovery from a poisoned `GroupCommitter`
  is unchanged from Phase 1's own documented model: discard it, reopen
  the WAL directory (`FileWal::open_for_recovery` re-scans from disk),
  construct a fresh one — verified end-to-end by a new test.
- Two pre-existing `execution::leader_drain` tests, whose doc comments
  and implicit timing assumptions described the old, now-fixed behavior
  (~5s shutdown cost; a second request only failing after its full
  retry budget), were updated in place with new `< 1s` timing
  assertions locking in the fix, rather than left stale next to
  passing-but-now-misleading documentation.

Full design and the leader-failure state machine: `PHASE3_FAILURE_
MODEL.md`; decision record: `PHASE3_ADR.md` ADR-P3-1; results: `PHASE3_
TEST_RESULTS.md`; benchmarks: `PHASE3_PERFORMANCE.md` (both the
100-writer and 1,000-writer Phase 2B throughput targets remain met
after this fix, using the same Approach B/Dedicated Batch Coordinator
architecture, unchanged).

### Phase 1: Group Commit

Adds `wal::group_commit::GroupCommitter`, a leader-follower group commit
layer over the existing `FileWal`: concurrent callers share one `fsync`
per batch instead of paying one per write, with a monotone
`durable_through` watermark, bounded backpressure, explicit shutdown, and
observability counters. No change to the WAL's on-disk format, `Wal`
trait signatures, or `FileWal`'s single-writer internal model — see
`PHASE1_ARCHITECTURE.md`/`PHASE1_GROUP_COMMIT.md`/`PHASE1_ADR.md` for the
design and `PHASE1_TEST_RESULTS.md` for full results, benchmark numbers,
and the production-readiness decision (**not production ready**: the
100-writer/1,000-writer throughput targets are not met on the
development machine's disk, even after a controlled window-size sweep
(`PHASE1_ADR.md` ADR-12) substantially closed the gap by fixing the
leader's batch-window formula (`WINDOW_EMA_DIVISOR` `10 → 1`, `max_wait`
`200µs → 5ms`, plus a demand-adaptive probe protecting single-writer
latency) — 100 writers improved from ~67% to ~79% of target, 1,000
writers from ~46% to ~81%; every other gate is met).

- **`GroupCommitter`** (`src/wal/group_commit.rs`): `append`/
  `await_durable`/`append_durable`/`rotate`/`durable_through`/`stats`/
  `shutdown`, plus `with_max_pending_waiters` for explicit backpressure
  configuration. `SyncMode::GroupCommit` is no longer rejected by
  `FileWal::open_for_recovery` (it previously returned `EngineError::
  Unsupported` — see below).
- **`FsyncLatencyTracker`** (`src/wal/metrics.rs`): an `AtomicU64`-only
  EMA `fsync`-latency tracker (`new = 0.1 * sample + 0.9 * old`), driving
  the leader's batch-window sizing and a follower's timeout.
- **`FileWal::durable_seq`**: a new field, advanced only inside `sync()`/
  `rotate()` after a genuinely successful `fsync` — distinct from
  `next_seq() - 1` ("assigned," not "durable"), closing a real footgun
  where an unsynced raw `append()` before constructing a `GroupCommitter`
  could otherwise be silently treated as durable.
- **`AbortPoint`** (`src/wal/mod.rs`) expanded from 4 to 11 variants: the
  7 new ones (`BeforeLeader`, `AfterLeaderElection`, `DuringBatchWaitPre`/
  `Post`, `AfterWatermarkBeforeWake`, `DuringRotationPre`/`Post`) name
  real, reachable boundaries in `GroupCommitter`'s leader/rotation paths.
  `BeforeSync`/`AfterSync` now additionally fire from `GroupCommitter`'s
  own leader `fsync` call, not only from `FileWal::sync()`, which that
  path never calls.
- **`EngineError::Timeout`** (`src/error.rs`): a new variant for a
  follower's bounded wait expiring — required by the algorithm, distinct
  from `Io` (no I/O necessarily failed) and safe to retry.
- Seven new integration test files under `tests/group_commit/` (one per
  milestone, plus a proptest), a write-only load-test harness (`examples/
  group_commit_load_test.rs`), and a permanent append-path diagnostic
  (`examples/append_only_benchmark.rs`).
- **Window-size sweep and batch-window formula fix** (`PHASE1_ADR.md`
  ADR-12, `PHASE1_TEST_RESULTS.md` §9A/§9B): a temporary, feature-gated
  experiment (`phase1-window-experiment` Cargo feature, off by default;
  `examples/window_sweep.rs`) established empirically that the original
  leader batch-window formula (`min(200µs, EMA/10)`) was substantially
  under-tuned, not solely limited by disk `fsync` latency as first
  believed. `WINDOW_EMA_DIVISOR` changed `10 → 1`; every test/harness
  `max_wait` changed `200µs → 5ms`; a demand-adaptive probe
  (`PROBE_WINDOW = 200µs`) added so a lone, uncontended writer never
  pays for batching benefit that will never materialize — a real
  regression the naive fix introduced and this probe resolves, verified
  by re-running M1.1.

### Five follow-up fixes from external review

- **`purge_before` now attempts its directory fsync on the error path
  too**, not only on success — the "resurrection is tolerated" argument
  (every purged segment's data is already durable elsewhere, WAL Spec
  §10) is a second line of defense, not a substitute for actually trying
  the fsync whenever the directory genuinely changed. A `remove_file`
  failure and a subsequent fsync failure are now folded into one error
  that names both, rather than either one being silently dropped.
  Dropped the unconditional `eprintln!` (a library writing to stderr
  unconditionally is untestable and rude to embedders) — the combined
  error message itself now carries what the log line used to.
- **Documented, not "fixed," why recovery's torn-tail truncation doesn't
  need a directory fsync**: `set_len` + `sync_all` flush exactly the
  file's own inode metadata (its size field); no directory *entry* is
  created, renamed, or unlinked, so a directory fsync there would be a
  no-op on every mainstream filesystem and pure cost on the recovery hot
  path.
- **`write_all_at` now has a real Windows implementation** (`seek_write`
  in a retry loop matching `std::io::Write::write_all`'s own
  `Interrupted`-retry rule) instead of falling back to the portable
  seek-then-write-all default. **In verifying this by actually running
  the test on Windows** (this project's dev machine), found a genuine,
  previously-undocumented platform difference: Windows' `seek_write` on
  an ordinary synchronous handle *does* leave the file's position at the
  end of the just-written region, unlike Unix's `pwrite`, which never
  touches it. The byte content lands correctly at the correct offset on
  both platforms either way (nothing in this crate's production code
  relies on the position side-effect), but the doc comment and test
  previously claimed a cross-platform guarantee that turned out to be
  Unix-only — corrected rather than asserted from memory. See
  `WalFile::write_all_at`'s doc comment.
- Expanded `Fault::PartialThenFail`'s doc comment with its exact
  interaction with `std::io::Write::write_all`'s retry behavior and how
  it differs from `ShortWrite`, at the definition site rather than
  requiring a future test author to read `FaultInjectingIo::write`'s
  body to find out.
- Added a `NOTE` comment directly above `scan_directory`'s main loop
  making explicit that the first-corruption-stops-the-scan behavior
  (Group 3.1) is deliberate, and that a future operator-diagnostics
  function walking past corruption would need to be a *different*
  function with a *different* contract, not a loosened version of this
  loop.

### Cross-process file locking

- **`open_for_recovery` now takes an OS-level exclusive advisory lock**
  on the WAL directory (`std::fs::File::try_lock` — `flock` on Unix,
  `LockFileEx` on Windows, both via the standard library, no new
  dependency and no `unsafe`) for as long as the returned `FileWal`
  lives, closing a real gap where two concurrent `open_for_recovery`
  calls on the same directory — two separate processes, or two
  unsynchronized calls within one — could each independently scan,
  truncate torn tails, and append, silently corrupting each other's view
  of the WAL. A second attempt while the lock is held fails immediately
  (`EngineError::WalUnavailable`) — it never blocks, and it is never
  silently allowed to proceed.
- **`inspect` takes a compatible shared lock**: any number of `inspect`
  calls may run concurrently with each other, but not while a writer
  holds the exclusive lock (closing a narrower race — `FileWal::append`
  writes directly with no atomic-rename step, so `inspect` could
  otherwise observe a segment file mid-write). Takes no lock at all
  against a directory no writer has ever opened — `inspect` must never
  create anything.
- Verified with both an in-process regression test and a genuine
  cross-process test (`tests/wal_tests.rs`'s
  `cross_process_lock_prevents_concurrent_writers`, using the same
  spawn-this-test-binary-as-a-child-process technique as
  `tests/crash_consistency.rs`) — a real second OS process is rejected
  while the first is open and succeeds once it's dropped.

### WAL hardening pass (production-readiness review)

A targeted review of the WAL implementation (`src/wal/`) against
production-readiness criteria, fixing 20 issues across crash-safety,
format validation, recovery semantics, thread-safety documentation, and
test coverage, plus the cross-process locking gap above. The on-disk
format (WAL Spec §2) is unchanged — verified byte-for-byte against
`encode_segment_header(42)` and a sample `PUT` frame before and after
this pass.

#### Crash-safety / durability

- **`SegmentIo::append` now rolls back a failed write.** A partial write
  (some bytes physically land, then the write call fails) used to leave
  garbage bytes on disk past the tracked segment length, silently
  corrupting the *next* append's target region. `append` now truncates
  the file back to its pre-append length and fsyncs that truncation on
  any write failure. If the rollback itself fails, the `SegmentIo` is
  marked poisoned (`SegmentIo::is_poisoned`, `FileWal::is_poisoned`) and
  refuses all further I/O rather than write on top of an unknown-length
  file.
- **Directory fsync after segment create/remove.** `create_new_segment_file`
  and `FileWal::purge_before` now fsync the containing directory (Unix;
  documented no-op on Windows — see `file_io::fsync_dir`'s doc comment)
  so a file's *creation or removal*, not just its contents, survives a
  crash.
- **`create_new_segment_file` is now atomic w.r.t. partial failure.** A
  failure at any step after `create_new(true)` — header write, header
  fsync, or the new directory fsync — removes the partially-initialized
  file (best-effort) before returning, so a segment never exists on disk
  without a valid header.
- **`FileWal::rotate` is now atomic.** The new segment file is created
  *before* anything about the current `FileWal` state is touched; if
  sealing the old segment (`sync`) then fails, the just-created file is
  deleted and every field is left exactly as it was.
- **`FileWal::purge_before` removes segments one at a time**, updating its
  internal bookkeeping only after each individual removal succeeds, so a
  mid-list failure leaves accurate state rather than a mismatch between
  disk and memory. The directory is fsynced once after the whole batch.

#### Format validation

- `decode_segment_header` now rejects an unrecognized `format_version` and
  non-zero reserved `flags`, instead of accepting and silently
  misinterpreting a foreign/future format.
- `read_u32_le`/`read_u64_le` now return `Result` instead of relying on a
  `debug_assert!`-only precondition that disappears in release builds.
- WAL-frame encoding is now fully fallible end-to-end
  (`format::encode_frame`'s op-body closure, `write_len_prefixed`): an
  oversized field is rejected at the exact point of violation, not by
  emitting a sentinel value for a separate, later check to catch.
- `decode_wal_body`'s existing trailing-byte/wrong-length strictness for
  `PUT`/`DELETE`/`CHECKPOINT_MARKER` now has explicit regression tests.

#### Recovery semantics (behavior change — see below)

- **Recovery now stops at the first corrupted segment** and trusts
  nothing at or after it, including that segment's own records that
  preceded the corruption point within it. **This amends WAL Spec
  §6.2's original text**, which allowed scanning to continue past a
  corrupted non-last segment. See `wal::mod`'s "# Durability" section and
  `scan_directory`'s doc comment for the full rationale (fail-closed:
  once one segment's integrity is in question, a partial picture
  assembled from what comes after it is not more trustworthy for looking
  more complete). One pre-existing test asserted the old behavior by
  name and by assertion; it has been updated (not deleted) to assert the
  new contract, and a complementary test was added covering the
  "corruption is not in the first segment" case.
- `walk_segment`'s frame-extent overflow case now returns a `Corruption`
  error instead of `.expect()`-panicking on a value that is only
  provably non-overflowing for realistic input.
- Segment-ID arithmetic (`next_segment_id`, used by both `rotate` and
  fresh-segment creation) is now checked, returning `CapacityExceeded`
  instead of wrapping on overflow.

#### `inspect()` is now genuinely read-only

- `canonicalize_existing_dir` (used only by `inspect`) never creates the
  WAL directory — `canonicalize_data_dir` (used by `open_for_recovery`)
  still does.
- `scan_segment` takes a `mutate` flag; when false, every segment file is
  opened read-only, so `inspect` can run against a directory the caller
  can't write to.

#### Concurrency & API surface

- `FileWal` is documented as `Send` but deliberately not `Sync`
  (single-writer type), enforced at compile time via a
  `static_assertions::assert_not_impl_any!` check.
- `SyncMode::GroupCommit` is now rejected by `open_for_recovery`
  (`EngineError::Unsupported`) rather than silently running in
  `Immediate` mode — a caller that asks for batching is told there is
  none yet, instead of quietly getting different behavior than it
  configured.
- `wal::testing` (the `FaultInjectingIo` harness) is now gated behind
  `#[cfg(any(test, feature = "test-util"))]` instead of being
  unconditionally `pub`.

#### Performance

- `SegmentIo::append` uses a new `WalFile::write_all_at` method — a
  single `pwrite`-based syscall on Unix (via `std::fs::File`'s override,
  `std::os::unix::fs::FileExt`), falling back to the portable
  seek-then-write-all on other platforms. The doc comment claiming "one
  syscall per append" was accurate on neither platform before this
  change (it always did a separate `seek` first); it now says exactly
  what happens on each platform.
- Added `benches/append.rs` (behind the new `bench` Cargo feature),
  isolating pure `append` latency from `append_sync`'s `fsync` cost.

#### Tests & fuzz coverage

- Three new proptest cases in `wal::fuzz_tests` (≥1,000 runs each):
  random single-byte corruption anywhere outside the header, a
  partial-write-with-garbage-header-bytes scenario, and a fixed-seed,
  10,000-iteration arbitrary-byte-string panic check.
- `Fault::PartialThenFail` added to the `FaultInjectingIo` harness (bytes
  physically land, then the call fails), with regression tests for both
  the successful-rollback and poison-on-rollback-failure paths.
- New `tests/crash_consistency.rs` (behind the `test-util` feature):
  spawns this same test binary as a child process, which opens a real
  `FileWal`, appends records, and calls `std::process::abort()` at one of
  four configurable points (`FileWal::set_abort_hook`); the parent
  reopens and asserts a corruption-free, gap-free recovered prefix. See
  that file's doc comment for what this does and does not prove
  (`process::abort()` is not a power-loss simulation).
- A read-only-directory test (`#[cfg(unix)]`, `chmod 0o555`) asserting
  `inspect` succeeds where `open_for_recovery` fails with
  `PermissionDenied`.

#### Documentation

- Checked the entire `src/`/`tests/`/`benches/` tree for mojibake — found
  none (all files are valid UTF-8, `§`/`—`/`'` are correctly encoded
  throughout already). Added `scripts/check-encoding.sh`, a corrected
  version of the originally-specified check (the literal
  `grep -rP '[\x80-\xff]'` pattern matches *all* non-ASCII UTF-8 bytes,
  which would flag this codebase's own correct typography as an error).
- Added "# Safety" and "# Durability" sections to `wal::mod`'s
  module-level doc comment.

#### Fixed

- `create_new_segment_file` (rotation and initial-segment creation) now
  writes and fsyncs a new segment's header to a temporary file name and
  `fs::rename`s it into place, instead of writing the header directly at
  the segment's final name. A crash between file creation and header
  write previously left a zero-byte file at a real segment name, which
  recovery correctly (but undesirably) reported as a corrupted segment.
  Found by a deterministic `crash_consistency_across_abort_points`
  failure; see `PHASE1_TEST_RESULTS.md` §9F.2 and `PHASE1_ADR.md` ADR-15.
- Reverted the `filling_active`/`fsyncing_active` batch-pipelining split
  back to the single-phase `leader_active` design: measured to regress
  throughput on this project's development environment (Windows/NTFS)
  rather than improve it, with the change left uncommitted in the tree
  as pipelining's own regression check rather than reverted. See
  `PHASE1_TEST_RESULTS.md` §9E/§9F.1 and `PHASE1_ADR.md` ADR-14.

### Phase 2: Write Worker Pool (implemented, measured, rejected)

Adds `execution::WriteWorkerPool` (`src/execution/write_pool.rs`): a
bounded queue plus a configurable number of worker threads in front of
`GroupCommitter`, meant to separate logical client concurrency from
physical storage execution concurrency. `std`-only (`Mutex`+`Condvar`,
no new dependency), no change to `GroupCommitter`'s durability logic, no
WAL format change. Public API: `submit`/`Completion::wait`/
`wait_timeout`, `shutdown` (three-step: reject new work, drain the
queue, then finalize the underlying `GroupCommitter`), `stats`,
`into_inner`. Bounded everywhere: `queue_capacity`, `max_queued_bytes`,
`submission_timeout` (blocks then `EngineError::Timeout`, never drops a
write or blocks unboundedly), `shutdown_drain_bound`. A worker panic
resolves only its own in-flight request with an error (via an RAII
completion guard) and never loses another queued request; if every
worker terminates unexpectedly, the pool fails cleanly and drains the
remaining queue with an explicit error rather than leaving any caller
blocked forever.

**Measured and rejected as a production default**: a worker-count sweep
(1/2/4/8/16/32/64, plus a parity point at `worker_count = writer_count`)
at 100 and 1,000 logical writers found `GroupCommitter`'s batch size
architecturally capped at the worker count, not the logical writer
count — throughput regressed by one to two orders of magnitude at every
worker count meaningfully smaller than the writer count, and even at
parity (`worker_count = writer_count`) 1,000-writer throughput was 28%
below Phase 1's existing direct-thread architecture. Kept in the tree as
a documented, tested, but not-recommended artifact — see `PHASE2_TEST_
RESULTS.md`/`PHASE2_ADR.md` (ADR-P2-5) for the full evidence and
decision.

#### Fixed

- The worker pool's request-processing path originally retried nothing:
  a single-attempt `append` + `await_durable` call could surface a
  spurious `Timeout` to a caller under real concurrent load even though
  the underlying write was never lost. Fixed by retrying only `await_
  durable` (never `append` — appending exactly once means retrying the
  wait can never duplicate a record), mirroring the retry pattern Phase
  1's own test harness already established as correct
  (`tests/group_commit/support.rs::await_durable_retrying_on_timeout`).
  See `PHASE2_TEST_RESULTS.md` §13 and `PHASE2_ADR.md` ADR-P2-4.

### Phase 2B: three architectures evaluated — target achieved

Adds three further execution-layer architectures, evaluated against
Phase 2's rejected `WriteWorkerPool` and against each other:

- **`execution::leader_drain`** (Approach A, "Leader Queue Drain"): a
  worker drains the entire currently-queued backlog at once (not one
  request per loop iteration, unlike the rejected worker pool), appends
  every entry, then issues one `await_durable` for the whole batch.
  `worker_count=1` reached 16,806/95,686 ops/sec (100w/1,000w medians),
  exceeding both Phase 1 targets on the first attempt. A single-active-
  drain-leader coordination flag (`draining_active`/`DrainLeaderGuard`)
  lets `worker_count>1` provide hot-standby redundancy without
  fragmenting batches (the naive multi-worker failure mode this fixes),
  at a small cost to 100-writer margin.
- **`execution::batch_coordinator`** (Approach B, "Dedicated Batch
  Coordinator", **adopted as the recommended default**): exactly one
  coordinator thread, no worker-election machinery — structurally
  simpler than A. Reached 17,512/93,594 ops/sec (100w/1,000w medians,
  5 independent repetitions each) — the best 100-writer result of any
  architecture measured, with the least code.
- **`execution::sharded_ingress`** (Approach C, "Sharded/Per-Core
  Ingress"): `shard_count` independent ingress queues merged by one
  coordinator, evaluated once (the operating brief's own conditional
  framing — evaluate only if A and B fail, which they did not).
  15,234/96,033 ops/sec — no material improvement over B, confirming
  the single shared queue was never the bottleneck.

All three preserve the WAL format, `GroupCommitter`'s durability
contract, and crash-consistency guarantees unchanged. Zero Phase 1/
Phase 2 regressions across the full cycle. Full account: `PHASE2B_
FINAL_TEST_RESULTS.md`; design: `PHASE2B_ARCHITECTURE_A/B/C.md`;
decisions: `PHASE2B_ADR.md`.

**Target achieved**: Approach B reached a median 17,512 durable
ops/sec at 100 writers (target ≥15,000) and 93,594 at 1,000 writers
(target ≥80,000) — the first phase in this project's history to meet
the original Phase 1 throughput targets.

#### Fixed

- Every Phase 2B architecture's batch-processing path originally (in
  Approach A's first implementation) constructed each request's panic-
  safety guard (`CompletionGuard`) *after* the one shared `await_
  durable` call for a batch, rather than before — leaving every entry
  in a batch unprotected during the call most likely to observe a fault.
  A panic there hung the corresponding fault-injection test past a
  60-second timeout. Fixed by constructing every guard before the
  shared call and keeping them alive across it; Approaches B and C were
  written after this fix and used the correct ordering from the start.
  See `PHASE2B_FAILURE_MODEL.md` §2 and `PHASE2B_ADR.md` ADR-P2B-3.

#### Discovered (pre-existing Phase 1 behavior, not a regression)

- A leader/coordinator thread that panics specifically while inside the
  leader `fsync` call leaves `GroupCommitter`'s own `leader_active` flag
  (`src/wal/group_commit.rs`) permanently stuck — every architecture's
  worker/standby redundancy is powerless against this specific failure,
  since the underlying committer itself becomes globally wedged, not
  just the one thread that died. Verified the system still fails safely
  (bounded, no hang, no false acknowledgment) under this condition.
  See `PHASE2B_FAILURE_MODEL.md` §3.

## 2026-10-02 -- Final single-node certification (ENGINE-BLOCKED; not declared production ready)

### Fixed
- **SQL parser (D-1):** the pre-parse operator-chain guard no longer counts operator characters that
  occur inside string literals, quoted identifiers or comments. Previously valid statements (e.g. a
  single INSERT of 260+ ISO-date rows, or a 40 KB hyphenated text value) were rejected with
  `413 RESOURCE_LIMIT`. Genuine operator chains (incl. the 20,000-term stack-overflow case) are still
  rejected. Zero-allocation fast path unchanged.
- **CLI (D-2):** terminal sanitizer now also escapes C1 control characters (U+0080-U+009F, e.g. 8-bit
  CSI/OSC), as its documentation always claimed.
- **Test harness (D-0):** `two_instances_simultaneous_...` no longer panics in debug and no longer
  leaks spawned server processes. Assertions unchanged.

### Security
- Frontend `react-router-dom` 6.30 -> 7.18 (closes GHSA-wrjc-x8rr-h8h6, GHSA-337j-9hxr-rhxg); production
  `npm audit` now reports 0 vulnerabilities. `cargo audit`: 0 vulnerabilities.

### Added
- `frontend/e2e/xss_safety.spec.ts` (real browser, untrusted database content), 4 SQL and 2 CLI
  regression tests, and the certification documents `PHASE_RUBIXDB_FINAL_SINGLE_NODE_*.md` and
  `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`.

### Known / unchanged
- WAL throughput M1.2/M1.3 remain ENGINE-BLOCKED (fsync-latency bound); `src/wal/` untouched.
- Same-table SQL commit throughput ~270/s (per-table commit lock held across fsync) -- characterized only.

## 2026-10-03 -- WAL performance resolution (branch `wal-batch-buffer-fillq`, not merged)

### Changed
- **WAL group commit:** the leader's batch window now closes early once the open batch has reached the previous batch's size
  (cohorts <= 256) and arrivals have been quiet for `clamp(400 ns x cohort, 100 us, 1 ms)`. Only ever earlier than the existing
  deadline. No change to durability, ordering, recovery, on-disk format or failure handling. 2 new tests.
  Measured: M1.2 +13.6%; 2-16 writers +44% to +85%; M1.3 neutral; product-level SQL write throughput unchanged.

### Added (analysis tools, not on any production path)
- `examples/{fsync_lanes_probe,fsync_overlap_probe,commit_pipeline_proto,wal_commit_latency,wal_ack_oracle}.rs`,
  `scripts/wal_bench_runner.ps1`, and the `PHASE_RUBIXDB_WAL_*.md` documents.

### Known / unchanged
- M1.2 / M1.3 still below target (ENGINE-BLOCKED); no NVMe available (experiment open); leader-written batch buffer NOT implemented
  pending a failure-semantics decision.

## 2026-10-03 -- WAL flat-combining group commit (branch `wal-batch-buffer-fillq`, not merged)

### Changed (`src/wal/`)
- `GroupCommitter::append` now uses flat combining: concurrent appenders' frames are written by one combiner in a single
  syscall (`FileWal::append_group`); each appender still returns only after its own frame is written. Durability, ordering,
  recovery and on-disk format are unchanged; a failed batched write fails every writer in that run (all get `Err`, rolled back).
- Leader batch window: closes early when the previous cohort has arrived and arrivals are quiet, or after 4x that quiet interval
  if the cohort is not completing; the lone-writer probe applies only after a one-record batch.
- Measured (isolated, interleaved, this SATA machine): M1.2 10.5k -> 17.1k, M1.3 62.7k -> 99.0k; 2-1,000 writers +60-120%; sustained
  64-writer +72%; p50/p95/p99 lower at every concurrency, p99.9/max higher at 256-512 writers; product SQL write throughput unchanged.

### Added (tools, not on any production path)
- `examples/{commit_pipeline_proto,fsync_lanes_probe,fsync_overlap_probe,windows_io_modes_probe,wal_commit_latency,wal_soak,wal_ack_oracle}.rs`,
  `scripts/{wal_bench_runner.ps1,cpu_warm.py,wal_soak_monitor.ps1}`, tests `src/wal/group_append_tests.rs` and new group-commit tests.

### Known / open
- Full regression not clean under the default concurrent/debug harness (M1.2/M1.3) and one load-sensitive pre-existing unit test; NVMe
  unavailable; power-loss durability untested; p99.9/max regression at 256-512 writers.

## 2026-10-04 -- Production operations (branch `wal-batch-buffer-fillq`, not merged)

### Added
- `rubixdb backup create|list|verify|delete`, `restore`, `check`, `status`, `storage`, `maintenance purge-orphans`, `instance stop`; API `/v1/admin/*`
  (status, backups, verify, check, storage, purge-orphans, shutdown; Admin role for every method); GUI **Operations** page.
- Backup format `RUBXBKUP` v1; restore into a fresh directory only; integrity checker (logical online + physical offline); data-directory `DATA_FORMAT` marker.
- `deny.toml`, `scripts/release.ps1` (bit-reproducible build + packaged smoke test), `scripts/wal_certify.ps1`, black-box campaigns in `scripts/ops/`.

### Changed
- Index-build recovery runs after the server is ready (graceful shutdown joins it). HTTP front end: 1,024-connection cap, 10 s header-read and 30 s body-idle timeouts.
- CLI escapes bidi embedding/override/isolate characters; `-f` accepts regular files only.
- WAL tests: throughput scenarios run exclusively inside their binary, debug-ignored; one load-sensitive unit test retries on `Timeout`. Increment 14 index-backfill crash test waits (bounded) for `ready` after restart.
- `Cargo.lock`: `yoke-derive` 0.8.3 (yanked) -> 0.8.4.

### Known / open
- M1.3 intermittently below 80 k on this SATA machine (FAIL), M1.2 OPEN; power loss and real disk-full not tested; PITR not implemented; downgrade unsupported;
  engine: corrupt WAL segments do not stop `LsmEngine::open` and the manifest is unversioned (ADR-ENG-OPS-001, guarded at the product layer).

## 2026-10-04 -- WAL M1.2/M1.3 diagnosis and final certification status (documentation only)
### Added
- `PHASE_RUBIXDB_WAL_M12_M13_DIAGNOSIS.md`, `PHASE_RUBIXDB_WAL_M12_M13_CERTIFICATION_FINAL.md`, `scratch/wal_diag/` raw data.
### Changed
- Certification status only: M1.3 reclassified FAIL (intermittent) -> OPEN (aggregate acceptance policy undefined; historical slow mode preserved, not reproduced in 58 runs, trigger unidentified); M1.2 OPEN; full release regression FAIL; power loss NOT TESTED; NVMe HARDWARE UNAVAILABLE. No code, test or threshold changed.
### Known / open
- Slow-mode trigger unknown; acceptance policy for repeated runs undefined; workspace release run still fails m1_3 (cause not investigated).

## 2026-10-04 -- WAL certification status under Rule A (documentation only)
### Changed
- Certification status: M1.2 FAIL, M1.3 FAIL (Rule A, set S: 2/57 and 8/95 included runs below threshold), full release regression FAIL, WAL certification FAIL, power loss NOT TESTED, NVMe HARDWARE UNAVAILABLE. Supersedes the earlier OPEN statements (additive section in `PHASE_RUBIXDB_WAL_M12_M13_CERTIFICATION_FINAL.md`). No code, test or threshold change.
### Known / open
- Workspace m1_3 failure root cause UNRESOLVED; slow-run trigger unknown (one slow run observed on C: with device write latency 10.4 ms); certification-set boundary undefined; non-WAL CLI test flake logged; docs/PROJECT_STATE.md and missions/ACTIVE.md absent.

## 2026-10-04 -- Phase 7 Increment A: repository hygiene (under [Unreleased])
### Security
- Stopped tracking `frontend/.e2e-crossbrowser-data/` (it held a generated admin credential published in commit `2afa0e1`); directory is now git-ignored. The key is treated as compromised; history is not rewritten.
- Added `tests/repo_hygiene.rs`: fails if any `credentials.json`, key/certificate file, or 64-hex `admin_key` literal is tracked.
- Release script now runs `cargo audit`, `cargo deny` and `npm audit --package-lock-only --omit=dev` (new `scripts/dependency_gates.ps1`) and fails the release on any of them.
### Changed
- Release packages no longer include `rubixdb-api.exe`; the standalone API binary is unsupported and not certified in v1. The package is asserted to contain no `rubixdb-api*`.

## 2026-10-04 -- Phase 7 Increment B: credential at rest and replacement (under [Unreleased])
### Security
- `credentials.json` on Windows is now written with an owner + SYSTEM only, non-inherited DACL, applied to the empty staging file before the key is written. If the ACL cannot be set, nothing is persisted and instance creation fails. The accepted-risk note in `PHASE_RUBIXDB_INSTANCE_SECURITY.md` §2 is superseded for files written by this version.
- Credential persistence is now stage -> restrict -> write -> fsync -> read-back verify -> rename; a damaged or mismatched staged file is never committed.
### Added
- `rubixdb instance rotate-credential <NAME> --confirm <NAME>`: offline credential replacement; refuses a running or concurrently-rotating instance; prints no key.
- `rubixdb-instance`: `rotate_credential`, `RotateError`, `RotateOutcome`; `windows-sys` 0.61 as a `cfg(windows)` dependency (already in Cargo.lock; one new dependency edge, no new crate).

## 2026-10-04 -- Phase 7 Increment C: redaction, security events, response headers, GUI handoff (under [Unreleased])
### Security
- Security event log: bounded JSON-lines `security.log` per instance (and `instances-security.log` for `instance drop`) recording authentication failures (rate-bounded), `/v1/admin/*` actions, catalog DDL create/drop, instance start/stop/drop and credential replacement. Never records keys, tokens, SQL text, parameters, row data or request URIs. 1 MiB x (1 + 4 generations) per log.
- `Debug` for `InstanceCredentials`, `ApiKeyConfig` and `Config` no longer prints keys; `RUBIXDB_API_KEYS` parse errors no longer echo the offending entry (which contained the key).
- Response headers on every response: strict `Content-Security-Policy` (no `'unsafe-inline'`/`'unsafe-eval'`), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`; `Cache-Control: no-store` on `/v1/*`.
- `rubixdb gui` hands the instance key to the browser in the URL fragment; the console stores it in sessionStorage only and removes the fragment from the URL. "Remember" now carries an explicit warning.
### Changed
- The console can only reach its own origin (CSP `connect-src` falls back to `'self'`).
- Connect-screen visual baseline regenerated for the new warning text.
### Added
- Dependency edges (no new crates): `rubixdb-cli` -> `tracing`, `tracing-subscriber`; `rubixdb-api` -> `pin-project-lite`.

## 2026-10-04 -- Frontend shell redesign, Increment 1 (under [Unreleased])
### Changed
- Console layout: left navigation rail (grouped, collapsible), top bar with breadcrumb and search box, content container. Pages and routes are unchanged; navigation labels are now Home, SQL Console, Monitoring, Catalog, Governance & security, Compute, Admin, Snapshots.
- Log out moved to the profile chip at the bottom of the rail.
### Added
- Design tokens for the shell (additive), inline icon set, rubiXDb logo asset, GUI shell spec.

## 2026-10-04 -- Frontend shell redesign, Increment 2 (under [Unreleased])
### Added
- Home page: Quick actions, Recent items (queries from this session, held snapshots, backups for admins), SQL templates, and a RAM usage card in the rail ("Not available" until the server reports memory).
### Changed
- The Home page heading is now "Home" (was "Dashboard"); the existing dashboard cards are unchanged below it.

## 2026-10-04 -- SQL Console redesign (under [Unreleased])
### Changed
- SQL Console: worksheet tabs (kept in this browser tab's sessionStorage), toolbar with session chips and Run/Stop, syntax-highlighted editor with line numbers and resize, results panel with Results / Query details / History, paginated grid (100/200/500 rows), CSV download, status bar. Page no longer scrolls; the grid scrolls inside its panel.

## 2026-10-04 -- Self-contained executable (under [Unreleased])
### Changed
- `rubixdb.exe` now carries the console inside the file; no `frontend-dist` folder is needed next to it, and a leftover one can no longer show an old UI.
- The `default` instance goes back to port 302 whenever it is free (it no longer stays on a random port chosen during an earlier collision).

## 2026-10-04 -- CLI race fix (under [Unreleased])
### Fixed
- Two `rubixdb -c` processes starting together on a fresh instance could fail: the one that attached to the other's server found it gone when the owner finished. A statement that cannot connect now re-resolves the instance (never mid-transaction, at most 3 times), so it runs exactly once.

## 2026-10-05 -- Phase 2 Increment A: startup configuration and credential validation (under [Unreleased])

### Fixed
- **The local credential file is validated.** `credentials.json` with an empty, short (< 16), over-long (> 256) or non-token (`[A-Za-z0-9_-]` only) `admin_key` used to start an instance that reported ready while rejecting every request (empty key; not even the admin shutdown worked) or ran with a trivially guessable key (3 characters), because the embedded server bypassed the API's own 16-character rule. It is now refused at load time by every command (start, attach, `instance status/stop`) with `credentials.json: <reason>; replace it with rubixdb instance rotate-credential ...`. The message never contains the key or any file content. Generated keys (64 hex characters) are unaffected; `rotate-credential` repairs an unusable file.
- **`RUBIXDB_LOCAL_RATE_LIMIT_BURST=0` (and `RPS` <= 0, NaN, infinite) can no longer lock the operator out.** These values made the limiter reject every authenticated request, including `POST /v1/admin/shutdown` and `rubixdb instance stop`. Rate-limit values are now validated: RPS finite and in (0, 1e9], burst >= 1. The standalone loader (`RUBIXDB_RATE_LIMIT_RPS/_BURST`, unsupported in v1) uses the same rule.

### Changed
- **No silent fallbacks.** Unparsable `RUBIXDB_LOCAL_RATE_LIMIT_RPS/_BURST`, `RUBIXDB_INSTANCE_RETRY_BUDGET_MS` (integer 0..=600000) and an invalid `RUBIXDB_FRONTEND_DIST` (must be a directory containing `index.html`) now fail startup with a message naming the variable; previously each silently used a default. Unset or empty still means the documented default.
- **Validation happens before anything is created or locked.** `rubixdb gui` and the client role check these values before taking the instance lock, so a bad value creates no directory, manifest or credential and leaves no process, lock or socket behind.
- `rubixdb gui --help` documents the environment variables and their ranges.

### Not changed
Port contract (127.0.0.1:302), bind address, identity handshake, engine (`src/`), `Cargo.toml`, `Cargo.lock`.

## 2026-10-05 -- Phase 2 Increment B: argument, instance-name, manifest and path validation (under [Unreleased])

### Fixed
- **`rubixdb gui` honours `RUBIXDB_INSTANCE_NAME`** as its help text always said (precedence: `--instance`, then the variable, then `default`). It used to ignore it and open `default`.
- **`rubixdb gui` arguments are strict.** A missing or flag-shaped `--instance` value, a repeated `--instance`, an unknown option or a stray word is now an error that creates nothing. Previously `--instance` with no value silently opened `default`, `--instance --no-browser` created an instance named `--no-browser`, and unknown options were ignored.
- **An `instance.json` that does not belong to its directory is refused.** Its `name` must satisfy the instance-name rule and match the directory name; before, a mismatching name made `rubixdb instance stop` fail with HTTP 400 and a name with control characters was printed raw by `rubixdb instance list`. `instance drop` and `rotate-credential` still work on such an instance so it can be removed or repaired.

### Changed
- Error messages name what is wrong: unusable instances root / instance directory errors include the path and the `RUBIXDB_INSTANCES_ROOT` setting; manifest errors include the file path; `RUBIXDB_API_URL` is checked for syntax (absolute http/https URL with a host, no user info, query or fragment) before any prompt or network attempt, and the value is never echoed.

### Not changed
Which hosts `RUBIXDB_API_URL` may name (plaintext/non-loopback), port contract (127.0.0.1:302), bind address, identity handshake, engine (`src/`), `Cargo.toml`, `Cargo.lock`.

## 2026-10-05 -- Phase 2 Increment C: observable index recovery (under [Unreleased])

### Added
- **`GET /readyz` reports `index_recovery`** (`running` | `complete` | `failed` | `not_started`): whether the post-start recovery of interrupted `CREATE INDEX` / `DROP INDEX` operations is still running. `ready` keeps its meaning (normal work is safe, which is true while recovery runs) and does not change; the new field is additive. The state is `running` before the server is reported ready, so a client never reads a stale value.
- **`POST /v1/admin/shutdown` replies with `index_recovery` and `waiting_for_index_recovery`**, and `rubixdb instance stop` says so ("is finishing an interrupted index build before it stops; this can take minutes on a large table"). If its 120 s wait ends first it now says the instance is still shutting down and is not stuck, instead of "did not exit within 120 s".

### Changed
- The embedded host and the standalone `rubixdb-api` share one implementation of the startup index recovery thread (`rubixdb_api::recovery`); messages are unchanged.

### Not changed / blocked
Stop latency while an index recovery is running (11-17 s at 400,000 rows; unbounded in principle) is unchanged: bounding it needs cooperative cancellation inside the index builder, a change to certified relational code, so it is proposed in `PHASE_RUBIXDB_LIFECYCLE_ADR_INDEX_RECOVERY_CANCELLATION.md` (ADR-LIFECYCLE-001) and not implemented. Port contract, bind address, handshake, engine, `Cargo.toml`, `Cargo.lock` untouched.

## 2026-10-05 -- Phase 2 Increment C2: graceful stop no longer waits for index recovery (under [Unreleased])

### Fixed
- **Stopping an instance while it is still recovering an interrupted `CREATE INDEX` / `DROP INDEX` is now immediate** (about 0.1 s at 400,000 rows; it used to take 11-19 s and grew with table size, with no upper bound). Shutdown asks the recovery to stop at its next chunk boundary (500 rows for a build, 1,000 entries for a drop sweep); the unfinished index stays `Building` / `Dropping` -- the same state a process kill leaves -- and the next start restarts it from scratch through the existing recovery. The engine still shuts down through its normal path; nothing is terminated abruptly and no timeout was added.

### Added
- `GET /readyz` `index_recovery` can now also be `cancelled` (recovery was interrupted by shutdown). The owner process prints `index recovery was interrupted by shutdown; unfinished indexes stay Building/Dropping and restart at the next start`; `rubixdb instance stop` says it is interrupting the recovery.
- Engine API (additive, `rubixdb::relational::index`): `RecoverySummary`, `IndexBuilder::recover_incomplete_builds_cancellable` and `recover_incomplete_drops_cancellable`. The existing methods are unchanged and never cancel; client-driven `CREATE INDEX` / `DROP INDEX` are never cancelled.

### Not changed
WAL, manifest, SSTable, compaction, `src/error.rs`, `Cargo.toml`, `Cargo.lock`, port contract, bind address, handshake. The change is documented and authorized in `PHASE_RUBIXDB_LIFECYCLE_ADR_INDEX_RECOVERY_CANCELLATION.md` (ADR-LIFECYCLE-001, accepted).

## 2026-10-05 -- Phase 2 Increment D: stop signals, status accuracy, cleanup, attach latency (under [Unreleased])

### Fixed
- **Ctrl+Break, console close, logoff and system shutdown now stop `rubixdb gui` gracefully** (Windows), like Ctrl+C: drain, engine shutdown, lock and port released, exit code 0. Ctrl+Break used to end the process abruptly (exit code 0xC000013A, no shutdown line). The process prints what triggered the stop, e.g. `rubixdb gui: shutting down (Ctrl+Break)...`. SIGTERM is handled on Unix. A launcher that disabled Ctrl+C for the process is respected; stop such a process with `rubixdb instance stop`.
- **`rubixdb instance status` no longer says "not running" while a process owns the instance.** It reports `locked (...)` when the OS lock is held and nothing answers on the port.
- **The lock message no longer tells the operator to delete the lock file** (which does not release the lock and could let a second process open the same data); it says to wait or run `rubixdb instance stop <name>`.
- **Stale console staging folders are removed.** A `<hash>.tmp-<pid>` folder left by a process killed mid-unpack was never pruned; folders older than 24 hours other than the current build's are now removed, also when the console is already cached.
- **`DEFAULT`/`Default` follow the same "return to port 302" rule as `default`** (they name the same instance directory on NTFS).

### Changed
- A failed attach probe (a held lock whose owner does not answer) now gives up on the TCP connect after 0.5 s instead of about 2.1 s on Windows (single probe 2.1 -> 0.55 s). The outcome (`Unreachable`) and the retry budget semantics are unchanged.

### Not changed
Port contract (127.0.0.1:302), bind address, identity handshake semantics, engine (`src/`), `Cargo.toml`, `Cargo.lock`.
