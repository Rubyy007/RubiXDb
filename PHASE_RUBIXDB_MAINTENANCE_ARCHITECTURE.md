# PHASE RUBIXDB — MAINTENANCE ARCHITECTURE

**Date:** 2026-10-03 · Implementation: `src/ops/maintenance.rs`, `src/ops/storage.rs`; API `/v1/admin/maintenance/purge-orphans`, `/v1/admin/storage`, `/v1/admin/backups*`; CLI `rubixdb maintenance|storage|backup`; GUI Operations page.
Rule applied: *inspect what exists first; do not duplicate engine functionality; every operation is classified SAFE / INSPECTION / DESTRUCTIVE / RECOVERY / REPAIR.*

## 1. Inventory of what already exists (inspected in source)

| Area | Existing capability | Operator surface before this phase | After |
|---|---|---|---|
| Compaction | Automatic, size-tiered, full-merge; `compact_once()` exists in the engine but is deliberately not exposed (`ADR-COMPACTION-001` Decision 13); metrics (`CompactionMetrics`) | `GET /v1/compaction/status|metrics`, Compaction page | + auto flag, cycle count, last-cycle ms, bytes in/out, tombstones dropped in `GET /v1/admin/status`, `rubixdb status`, Operations page. **No manual trigger** (not added: a forced full-merge is an engine primitive the certified design keeps internal). |
| Index maintenance | online `CREATE INDEX` (restart-not-resume), `DROP INDEX`, startup recovery of interrupted builds/drops (`recover_incomplete_*`, wired into both entry points) | `\di`, `/v1/catalog/indexes` | + per-index state and entry count/bytes (`GET /v1/admin/storage`, `rubixdb storage`), non-`Ready` indexes flagged by the checker (`INDEX_NOT_READY`). **Rebuild** = `DROP INDEX` + `CREATE INDEX` (proven by `index_rebuild_repairs_a_corrupted_index`); no second index implementation was added. |
| Statistics | runtime, in-memory, bounded (`relational::stats::RuntimeStats`), re-learned after restart | none | **Not persisted** (mandate: only if a measured production problem proves it necessary — none measured). Visibility: the status endpoint exposes what the statistics drive (planner choices) through existing planner metrics only; a "statistics available / age" field does not exist because `RuntimeStats` keeps no timestamp — recorded as a gap, not faked. |
| WAL cleanup | the engine purges segments `≤ checkpoint` after each flush (`Wal::purge_before`), under the certified checkpoint protocol | none | **No operator-facing WAL deletion exists, by design.** Visibility only: WAL bytes on disk, rotations, durable_through, checkpoint_seq. WAL cleanup safety is argued in §4 and exercised by the existing engine crash suites; this phase adds no deletion path, so it cannot violate recovery/backup/durability. |
| Catalog cleanup | none — `DROP TABLE` leaves the table's data behind (measured, see §2) | – | **`purge-orphans`** (below). |
| Backup cleanup | none | – | `backup delete` (exact-name confirmation). |
| Disk usage | none | – | data / WAL / SSTable / manifest bytes + volume free space in the status endpoint. |

## 2. The one real maintenance gap found: dropped objects leave their data behind
`CatalogService::drop_table` removes the catalog rows (table, columns, indexes, constraints) in one batch and nothing else (`sql/src/exec/write.rs` calls it directly; no sweep exists — verified, and measured: the integrity checker reports `ORPHAN_TABLE_DATA` with the exact entry count, and so does a backup). This is a space leak and also grows every backup. It is a relational-layer behaviour; the engine is not involved.

### `purge-orphans` (DESTRUCTIVE, bounded, two-step)
Safety argument (enforced in code, `src/ops/maintenance.rs`):
1. IDs are issued from monotonic counters that are bumped in the **same atomic batch** that creates the object and are never reused. At one snapshot `S`, an id `≤ counter(S)` that is absent from the catalog at `S` belongs to an object that existed and was removed — nobody can create it again and nobody writes to it. Ids above the counter are never touched (test: `purge_never_touches_live_tables_or_ids_above_the_counter`, a key for table id 9,999 survives).
2. Everything is decided at one snapshot, so an in-flight `CREATE TABLE`/`CREATE INDEX` cannot be mistaken for an orphan (its catalog row and counter commit atomically, before any data of it).
3. **Dry run by default** (INSPECTION): reports the dropped table ids, orphaned index ids and the exact entry count.
4. `apply` additionally requires `expected_entries` equal to the freshly recomputed count — the caller must have seen the plan, and a changed database refuses (`CONFLICT PRECONDITION_FAILED`); missing confirmation → `CONFIRMATION_REQUIRED`. Tested: both refusals change nothing (digest unchanged).
5. Deletes are tombstone `write_batch`es of ≤ 1,000 keys; the operation re-counts afterwards and reports `remaining` (must be 0; tested). Disk space returns when compaction rewrites the affected tables (tombstones are dropped by compaction; verified by the compaction metrics `tombstones_dropped_total`).
6. One maintenance operation at a time (`409 OPERATION_IN_PROGRESS`), `Admin` role only, GUI requires typing the entry count.
A fresh verified backup before applying is **recommended** (the data is unreachable through SQL, but it is the only copy of a table someone dropped); it is not forced, and the CLI/GUI say so.

## 3. Operation classification (every administrative operation)
| Operation | Class | Needs confirmation | Backend-validated |
|---|---|---|---|
| `status`, `storage`, `check`, `backup list`, `backup verify`, dry-run purge | INSPECTION | – | – |
| `backup create` | SAFE (writes one new file; never replaces; one at a time) | – | name rules, no overwrite |
| `restore` | RECOVERY (builds a new database; refuses a non-empty destination) | destination must be absent/empty | yes |
| `backup delete` | DESTRUCTIVE | exact backup name | `confirm == name` re-checked in the API |
| `purge-orphans --apply` | DESTRUCTIVE | exact entry count from the dry run | recomputed at apply time |
| `instance drop` (existing) | DESTRUCTIVE | exact instance name; refused while running | yes (Increment 14) |
| `DROP TABLE/INDEX/SCHEMA` via GUI (existing) | DESTRUCTIVE | exact names, backend re-check (Increment 14 Blocker 12) | yes |
| repair | NOT IMPLEMENTED (see integrity architecture §6) | – | – |

## 4. WAL cleanup safety (statement and evidence)
No code added in this phase deletes, truncates or rotates a WAL segment, and no API route can. The only deleter is the engine's own checkpoint-driven `purge_before`, unchanged and covered by its certified suites (`wal_tests` 12/12, `pathological_recovery_matrix` 9/9, `crash_consistency` abort points, the engine kill campaign). The checkpoint boundary that makes a purge safe is: *every record with seq ≤ `flushed_through_seq` is in a live SSTable listed by the MANIFEST*. A backup does not depend on WAL segments at all (it reads the engine's merged view at a snapshot), so WAL purging cannot invalidate a backup; and because backups are logical, retaining WAL for them is unnecessary. The corollary — **point-in-time recovery would need retained WAL and is therefore NOT IMPLEMENTED** — is recorded in the disaster-recovery document.
