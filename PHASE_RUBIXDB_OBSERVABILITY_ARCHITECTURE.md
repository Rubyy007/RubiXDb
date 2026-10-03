# PHASE RUBIXDB — OBSERVABILITY ARCHITECTURE

**Date:** 2026-10-03 · Implementation: `api/src/routes/admin.rs` (`GET /v1/admin/status`), `api/src/resources.rs`, `api/src/metrics.rs` (lifetime max added), CLI `rubixdb status|storage`, GUI **Operations** page. Existing surfaces kept unchanged: `/v1/status`, `/v1/metrics`, `/v1/compaction/*`, `/healthz`, `/readyz`.

## 1. What an operator can now see (and where it comes from)

| Question | Field(s) | Source (read verbatim, never recomputed) |
|---|---|---|
| Instance status | `instance.{id,name,uptime_secs,version}` | `Config`, `AppState` |
| Database / storage status | `storage.state`, `sstable_count`, `checkpoint_seq`, memtable bytes/entries, immutable count/bytes, manifest records/bytes, `oldest_live_snapshot_seq`, pressure events | `LsmEngine` accessors |
| Recovery | `recovery.{duration_ms, wal_records_visited, wal_records_applied, manifest_edits_replayed}` | `LsmEngine::recovery_stats()` |
| WAL state | `wal.{pool_state, coordinator_alive, durable_through, highest_sequence, pending_waiters, queue_depth/capacity, queued_bytes, submitted, completed_ok, completed_err, writes_timed_out, rejected_backpressure, sync_attempts, sync_failures, avg_batch_records, max_batch_records, avg_batch_bytes, avg_batch_processing_ms, segment_rotations, poisoned}` | `BatchCoordinatorStats` + `GroupCommitStats` |
| Compaction state | `compaction.{auto_trigger_enabled, trigger_count, live_sstable_count, cycles_completed, input/output_bytes_total, tombstones_dropped_total, duration_max_ms, last_cycle_ms}` | `CompactionMetrics` |
| Read activity | `reads.{requests, hits, misses, bloom_negatives, blocks_read}` | `ReadStats` |
| Active sessions / transactions | `sessions.active_transactions` | `SqlSessionRegistry::active_count()` (a session exists only while an explicit transaction is open) |
| Running queries | `queries.active_http_requests` | `ServiceMetrics::active_requests()` |
| Query count / errors / timeouts / cancellations | `queries.{requests, success, errors, timeouts, cancellations, rows_returned, rows_affected, parse_errors, bind_errors, authorization_denials}` | `SqlApiMetrics`, `SqlMetrics` (previously collected but **not exposed anywhere** — verified by grep) |
| Query latency | `queries.latency_ms.{p50,p95,p99,max}` | `POST /v1/sql` route reservoir (1,000 most recent samples) + **lifetime max** (added: the reservoir hides the tail) |
| Query throughput | `queries.requests` + `instance.uptime_secs` (rate = delta between polls; the GUI polls every 5 s) | counters |
| Table size / index size | `GET /v1/admin/storage` (on demand): rows, row bytes, per-index entries/bytes/state, catalog bytes, orphan bytes | one snapshot scan |
| Disk usage | `disk.{data_dir_bytes, wal_bytes, sstable_bytes, manifest_bytes, volume_free_bytes}` | directory walk, `GetDiskFreeSpaceExW` |
| Resource usage | `resources.{rss_bytes, threads, handles, cpu_seconds}` | `K32GetProcessMemoryInfo`, `CreateToolhelp32Snapshot` (threads of this pid), `GetProcessHandleCount`, `GetProcessTimes` (Linux: `/proc/self`) — no new dependency |
| Errors | `queries.errors`, `wal.completed_err/sync_failures`, `backups.failed_total`, integrity counts | counters |
| Background operation progress | `background.{backup_running, check_running, maintenance_running, last_check}`, `backups.{running, ok_total, failed_total, last}` | `AdminOps` (one-at-a-time flags) |

**Progress of long operations:** backup, check and purge are single synchronous requests with a running flag and a last-result record; there is no percentage. Stated honestly: a percentage would need the total in advance (a count scan costs as much as the operation). Cancellation exists (client disconnect).

## 2. Gaps stated, not papered over
* **fsync latency** is not a field of the certified `GroupCommitStats`. `avg_batch_processing_ms` (coordinator time per drained batch = append + durable wait) *includes* the fsync and is the closest honest number; no WAL change was made to add one.
* **`wal.poisoned` is derived**, not read: a failed fsync poisons the committer permanently (`a_failed_leader_fsync_poisons_the_committer_permanently`), so `sync_failures > 0 || pool_state == Failed`. It is not injectable end to end through the engine's public API (the WAL fsync hook is committer-level), so this flag is certified by derivation + the certified unit test, not by an end-to-end injection.
* **Statistics age / relearn status** (the planner's runtime statistics): `RuntimeStats` keeps no timestamp; nothing is reported rather than inventing one.
* "Running queries" is a count of in-flight HTTP requests, not a list: listing statements would put SQL text (user data) into an operator endpoint, which the metrics rule forbids.

## 3. Cardinality and safety rules (and how they are enforced)
1. Every key in the status document is a fixed field name in code. Nothing from a request or from user data becomes a key.
2. `ServiceMetrics` route labels come from axum's `MatchedPath` (the route *template*, e.g. `GET /v1/kv/:key_b64`) and the middleware is applied to the matched routes only — unmatched URLs never create a label. Bounded by the route table.
3. Error codes and finding codes are closed sets (`ops::codes`, `check::finding_codes`).
4. No raw SQL text, no credentials, no filesystem path (`data_dir` is **not** in the status document; `/v1/metadata` already exposes it to authenticated callers and is unchanged), no row values.
5. Per-table / per-index names appear only in the on-demand `GET /v1/admin/storage` inspection report and in integrity findings as ids (`table[3]`) — never as metric labels.
6. Memory: the latency reservoir is capped at 1,000 samples per route; `AdminOps` holds two small JSON records.
7. Cost: status is a handful of atomic loads, one directory walk (off the async threads) and one toolhelp snapshot — measured in the performance document; it is polled every 5 s by the GUI, not per request.
8. Access: all `/v1/admin/*` routes need the Admin role for every method (reader keys get 403 — tested).
Bounded-cardinality test (`api/tests/admin_ops.rs`): after 3,000 requests to distinct random URLs, random KV keys and random SQL strings, the number of route entries and the byte size of `/v1/admin/status` are unchanged.
