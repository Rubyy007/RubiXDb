# PHASE RUBIXDB — INTEGRITY VERIFICATION & REPAIR ARCHITECTURE

**Date:** 2026-10-03 · Implementation: `src/ops/check.rs`, `src/ops/catalog_mirror.rs`; API `POST /v1/admin/check`; CLI `rubixdb check`; GUI Operations page. Tests: `src/ops/integration_tests.rs` (check_* / the corruption matrix). Evidence for the corruption matrix is in the certification document.

## 1. Why this is not "SELECT every table"
The checker decodes the physical keyspace itself (`CatalogMirror` from raw `0x00` keys; raw table-row and index-entry keys/values) and compares the pieces **with each other**. A query only ever follows one access path (a primary-key lookup never touches the secondary index; an index lookup never re-validates the row it returns), so a table can read perfectly and still carry a missing or stale index entry, a row that violates its schema, or dangling data. Each of those is a distinct finding below and is produced by an injected-fault test (§5).

## 2. Two passes, both read-only

### 2.1 Logical — `check_engine(&LsmEngine)` (works online)
Takes ONE `LsmEngine::snapshot()` and does every read `as_of` that sequence, so it is internally consistent while writers run (test: `the_checker_is_consistent_at_one_snapshot_while_writers_run` — 5 checks against a live writer that inserts and deletes rows with two maintained indexes; 0 findings), and cannot be confused by a commit landing mid-scan.

| Check | Finding codes |
|---|---|
| Catalog keys well-formed; every row decodes against its system-table schema | `CATALOG_KEY_MALFORMED`, `CATALOG_ROW_UNDECODABLE` |
| schema→database, table→schema, column→table, index→table, constraint→table | `CATALOG_DANGLING_REF` |
| columns contiguous from 0, ≥ 1 per table; PK and index column lists within range | `CATALOG_COLUMNS_INVALID` |
| ID counters ≥ every issued id (a lower counter would re-issue a live id) | `CATALOG_COUNTER_BEHIND` |
| Row key decodes to the table's PK types **and is the canonical encoding** of its own values | `ROW_KEY_UNDECODABLE`, `ROW_KEY_NONCANONICAL` |
| Row value decodes against the column types (bounds-checked decoder); row `schema_version` ≤ the table's; NOT NULL honoured | `ROW_VALUE_UNDECODABLE`, `ROW_SCHEMA_VERSION_AHEAD`, `ROW_NULL_VIOLATION` |
| **row → index**: for every row and every `Ready` secondary index, the exact expected entry key exists | `INDEX_ENTRY_MISSING` |
| **index → row**: every entry decodes, its PK suffix decodes, the row exists, and the row's indexed-column values equal the entry's | `INDEX_ENTRY_UNDECODABLE`, `INDEX_ENTRY_DANGLING`, `INDEX_ENTRY_STALE` |
| UNIQUE index: no two entries with the same non-NULL indexed values | `INDEX_UNIQUE_VIOLATION` |
| Primary-key uniqueness | structural: one physical key per PK (the PK *is* the row key, D6); duplicates cannot be stored, and a non-canonical key is caught above |
| Index not `Ready` (Building/Failed/Dropping) — interrupted background operation | `INDEX_NOT_READY` (warning; completeness is not asserted for it) |
| Table marked `Dropping` | `TABLE_DROPPING` (warning) |
| Rows/entries of a table or index id that is not in the catalog | `ORPHAN_TABLE_DATA`, `ORPHAN_INDEX_DATA` (warning — see §4) |
| Keys outside the two namespaces (raw key-value API data) | `NON_RELATIONAL_KEYS` (info) |
| Scan/read failure, cancellation | `CHECK_INCOMPLETE` (error; report is marked incomplete) |

Resource bounds: memory is the catalog plus one counter pair; reads are streaming range scans and point `contains`/`get_as_of` lookups; cancellable (client disconnect → cancel flag, checked every 4,096 rows); findings retained ≤ 200 with *complete* per-code counts. One check at a time (`409 OPERATION_IN_PROGRESS`).

### 2.2 Physical — `check_physical(data_dir)` (stopped directory; never creates/truncates/deletes)
Run **before** any recovery so it sees what a crash left behind:
* layout (`MANIFEST`, `wal/`, `sstables/`, lock file; anything else → `UNEXPECTED_FILE` info; `*.sst.tmp` → info, removed at startup);
* MANIFEST replay (`manifest::replay_readonly`): corruption → `MANIFEST_CORRUPT`; torn tail → `MANIFEST_TORN_TAIL` (warning, recoverable);
* every live SSTable: file present, size equals the manifest's, footer/bloom/index validated by `SsTable::open`, **every data block's checksum verified** by reading all of them, record count equals the footer → `SSTABLE_MISSING`, `SSTABLE_SIZE_MISMATCH`, `SSTABLE_CORRUPT`; unreferenced `.sst` → `SSTABLE_ORPHAN` (warning);
* WAL (`wal::inspect`, shared lock, read-only): corrupt segment → `WAL_CORRUPT` (error), torn tail → `WAL_TORN_TAIL` (warning — *expected after a crash*, recovery truncates it), unreadable → `WAL_UNREADABLE`.
Torn tails are classified as warnings, not errors, because the certified recovery contract defines them as the normal outcome of a crash; a mid-segment bad frame is corruption and an error (`WAL Spec §6.3`).

### 2.3 Entry points
| Surface | Behaviour |
|---|---|
| `rubixdb check [--instance NAME]` | instance **running** → `POST /v1/admin/check` (logical, online, one snapshot). Instance **stopped** → physical pass first; if it found errors the logical pass is *skipped* (opening a damaged directory would run recovery over it); otherwise open the engine (ordinary crash recovery, exactly what the next start does) and run the logical pass. The stopped path holds the instance lock, so nothing can start meanwhile. |
| `rubixdb check --data-dir DIR` | offline only; the engine's own exclusive lock refuses a directory another process has open. |
| exit code | 0 clean · 1 warnings only · 2 errors · 3 incomplete · 4 could not run |
| API / GUI | `POST /v1/admin/check` (Admin, one at a time, cancel-on-disconnect); the Operations page shows the summary badge and the findings table. |

Nothing exposes a raw internal primitive to SQL.

## 3. Backup verification vs. integrity check
`verify_backup` (streaming, bounded) validates the file and the catalog and cross-checks entries == rows for `Ready` indexes. A *deep* verification is "restore into scratch and run the checker", which is exactly what `restore` does as its step 5 — so a backup that restores cleanly has been integrity-checked end to end.

## 4. Finding the checker made about the product (real, measured)
`DROP TABLE` (`CatalogService::drop_table`, `sql/src/exec/write.rs`) deletes the catalog rows only. The table's row keys and all of its index entries stay in the keyspace forever. Measured in the test fixture: 50 rows of a dropped table → `ORPHAN_TABLE_DATA` (50 entries) in the check, in `backup verify` (`orphan_table_entries`) and in a backup file. It is a **space leak, not corruption**: IDs are never reused (monotonic counters bumped in the creating batch), so the data is unreachable. It is reported as a warning and reclaimed by the maintenance operation (`PHASE_RUBIXDB_MAINTENANCE_ARCHITECTURE.md`). This is a relational-layer item, not an engine item, and was not changed here.

## 5. Corruption campaign (each row is an automated test that injects the fault into a real database and requires the stated finding)
| Injected fault | Required finding | Test |
|---|---|---|
| delete one entry of a `Ready` index | `INDEX_ENTRY_MISSING` | `a_missing_index_entry_is_detected` |
| put an entry for a PK that has no row | `INDEX_ENTRY_DANGLING` | `a_dangling_index_entry_is_detected` |
| move an entry to a different indexed value | `INDEX_ENTRY_STALE` (+ `…MISSING` for the row's own entry) | `a_stale_index_entry_is_detected` |
| garbage row value | `ROW_VALUE_UNDECODABLE` | `a_corrupt_row_value_is_detected_even_though_no_query_touches_it` |
| row key that cannot be a PK of the table | `ROW_KEY_UNDECODABLE` | `a_noncanonical_or_undecodable_row_key_is_detected` |
| garbage catalog row + lowered ID counter | `CATALOG_ROW_UNDECODABLE`, `CATALOG_COUNTER_BEHIND` | `an_undecodable_catalog_row_and_a_lagging_counter_are_detected` |
| delete a table's catalog row but not its data/columns | `CATALOG_DANGLING_REF` | `a_dangling_catalog_reference_is_detected` |
| duplicate value in a UNIQUE index | `INDEX_UNIQUE_VIOLATION` | `a_unique_index_violation_is_detected` |
| any single flipped bit / any truncation / trailing byte / wrong magic / wrong version of a **backup file** | the matching `BACKUP_*` code | `every_single_byte_flip_and_every_truncation_is_detected`, `wrong_magic_and_unknown_version_are_classified` |
| truncated manifest, missing/corrupt SSTable, bad WAL checksum, truncated WAL tail, wrong WAL/manifest/SSTable version | physical pass codes | physical corruption campaign in the certification document |

## 6. Repair — design only; REPAIR SAFETY = NOT IMPLEMENTED (by decision)
The mandate forbids a "repair everything" command. Nothing in `ops` modifies data in response to a finding. What exists, and what is proven:
* **Index rebuild (the one safe, bounded repair)** already exists as `DROP INDEX` + `CREATE INDEX` through the certified online `IndexBuilder` (restart-not-resume, recovery wired at startup). Test `index_rebuild_repairs_a_corrupted_index` corrupts a Ready index (missing + stale + dangling entries), rebuilds it with those two calls, and requires a clean check afterwards. It changes only the index, never rows, and is reversible by the same procedure.
* **Everything else** (row corruption, catalog corruption, SSTable/WAL/manifest corruption): the supported recovery is *restore the last verified backup into a fresh instance* — the only path whose result is verified end to end — and, for a torn WAL/manifest tail, the engine's own certified startup recovery.
* **What a future `repair` command must do** (recorded so it is not improvised): show the exact keys/objects it will modify (a plan, dry-run default); verify preconditions at one snapshot and refuse if the database changed; require a fresh verified backup first; perform bounded batches through `write_batch`; re-run the checker and require the targeted finding to be gone and no new one to appear; never delete evidence (move, don't drop). Until then: NOT IMPLEMENTED.
