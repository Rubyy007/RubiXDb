# PHASE RUBIXDB — PRODUCTION OPERATIONS ARCHITECTURE

**Date:** 2026-10-03 · Scope: single-node, local, relational, durable. No RBAC/login/password added to local mode; no router/replication/partitioning; no second storage or SQL engine.
Index of this phase's documents: `PHASE_RUBIXDB_PRODUCTION_BASELINE.md` (starting state) · `PHASE_RUBIXDB_WAL_CERTIFICATION_CLOSURE.md` · `PHASE_RUBIXDB_BACKUP_RESTORE_ARCHITECTURE.md` · `PHASE_RUBIXDB_DISASTER_RECOVERY.md` · `PHASE_RUBIXDB_INTEGRITY_ARCHITECTURE.md` · `PHASE_RUBIXDB_OBSERVABILITY_ARCHITECTURE.md` · `PHASE_RUBIXDB_MAINTENANCE_ARCHITECTURE.md` · `PHASE_RUBIXDB_FINAL_SINGLE_NODE_RELEASE.md` · `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md`.

## 1. Where the new code lives and why
```
src/ops/                 engine-crate module on the PUBLIC engine API only
  backup.rs              RUBXBKUP v1 writer/reader/verifier
  restore.rs             crash-safe restore into a fresh directory
  check.rs               logical (online) + physical (offline) integrity checker
  catalog_mirror.rs      raw-key catalog decoder shared by check and backup verify
  maintenance.rs         orphan-data purge (dry run + exact-count apply)
  storage.rs             per-table / per-index storage accounting
  format.rs              data-directory format marker
  open.rs                production-configured engine open for ops commands
api/src/routes/admin.rs  /v1/admin/* (Admin role for every method)
api/src/resources.rs     process/disk sampling without new dependencies
cli/src/ops_cmd.rs       rubixdb status|storage|check|backup|restore|maintenance
frontend/src/pages/OperationsPage.tsx   operator console
```
Nothing under `src/wal/` (production code), `src/manifest/`, `src/sstable/`, `src/compaction/`, `src/lsm/` or `src/execution/` was modified. The single change in `src/wal/` this phase is a test (see the WAL closure document). One metrics type in the API (`RouteMetricsSnapshot`) gained a lifetime `max_ms` field.

## 2. Operation classes (every administrative surface)
SAFE · INSPECTION · RECOVERY · DESTRUCTIVE · REPAIR — table in `PHASE_RUBIXDB_MAINTENANCE_ARCHITECTURE.md` §3. A destructive operation is never behind a generic command: `backup delete`, `maintenance purge-orphans --apply`, `instance drop`, `DROP …` each have their own verb, their own exact confirmation, and a backend re-check.

## 3. Interfaces
| Need | CLI | API (Admin) | GUI |
|---|---|---|---|
| instance/WAL/compaction/queries/resources/disk | `rubixdb status [--json]` | `GET /v1/admin/status` | Operations cards |
| table / index size | `rubixdb storage` | `GET /v1/admin/storage` | (CLI/API) |
| backup create / list / verify / delete | `rubixdb backup …` | `/v1/admin/backups[/:name[/verify]]` | Operations → Backups |
| restore (new database) | `rubixdb restore --from FILE --instance NAME \| --data-dir DIR` | – (offline by design) | – (offline by design) |
| integrity check | `rubixdb check [--instance NAME \| --data-dir DIR] [--json]` | `POST /v1/admin/check` | Operations → Integrity |
| reclaim dropped-table data | `rubixdb maintenance purge-orphans [--apply --expect N]` | `POST /v1/admin/maintenance/purge-orphans` | Operations → Maintenance |
| compaction status | (status) | `GET /v1/compaction/*` (existing) | Compaction page (existing) |
Restore is deliberately **not** an API call: it builds a new database in a new directory, which a running server cannot swap in without owning a second engine. The operator stops nothing — they restore into a *new* instance name and start it.

## 4. Safety rules enforced (and where tested)
| Rule | Enforcement | Test |
|---|---|---|
| Never restore over existing data | `DESTINATION_NOT_EMPTY`; no force option | `restore_refuses_a_non_empty_destination_and_changes_nothing` |
| No path from a client | name-only API, server-side directory | `backup_names_are_validated_and_errors_never_leak_paths` |
| Destructive ⇒ exact confirmation, backend-validated | `confirm == name`, `expected_entries == recomputed` | `deleting_a_backup_needs_the_exact_name…`, `purge_orphans_is_a_dry_run_until…` |
| Admin only, every method | `required_role` | `every_admin_route_requires_the_admin_role_for_every_method` |
| Bounded metrics, no secrets/paths/SQL | fixed keys; closed code sets | `metrics_stay_bounded_under_hostile_input`, `status_reports…` |
| Incompatible data directory refused, untouched | `DATA_FORMAT` marker + per-file versions | `wrong_format_versions_are_refused_and_the_directory_is_not_modified` |
| Backup contains no credentials or paths | header content | `backup_contains_no_paths_or_secrets` |
| One heavy operation at a time, cancel on disconnect | `BusyGuard`, `CancelOnDrop` | `a_second_concurrent_operation_of_the_same_kind_is_refused`, `cancelled_backup_leaves_nothing_behind` |

## 5. Engine boundary
No problem was *proven to originate inside* the WAL / read engine / write engine / manifest / SSTable / compaction that required a change here. One engine-side **gap** was found and is recorded, not fixed: the MANIFEST file has no magic and no format version (`src/manifest/format.rs`: frames only), so its compatibility is protected only by fail-closed decoding of unknown edit types. Recorded in `PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md`-style form in the release document as an engine ADR item; the directory-level `DATA_FORMAT` marker closes the product-level risk without touching the manifest.
