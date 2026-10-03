# PHASE RUBIXDB — BACKUP & RESTORE ARCHITECTURE

**Date:** 2026-10-03 · Implementation: `src/ops/{backup,restore,catalog_mirror,open}.rs`, API `api/src/routes/admin.rs`, CLI `cli/src/ops_cmd.rs`. Evidence: `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PRODUCTION_CERTIFICATION.md` §Backup/Restore and `scratch/prod_ops/`.
Current source is authoritative; everything below was verified against it before being written.

## 1. What is a "consistent database state" here (checked in source, not assumed)

* One physical keyspace. `src/catalog/encoding.rs` (`CATALOG_NAMESPACE = 0x00`) and `src/relational/key.rs` (`RELATIONAL_NAMESPACE = 0x01`) both live in the single `LsmEngine` keyspace, together with the raw key-value API's keys (any other first byte). There is no separate catalog store, no separate index store, no per-table file.
* Every logical commit is ONE `LsmEngine::write_batch` = one WAL `Group` frame = one sequence number:
  * `TableStore::put_row/put_rows/delete_row` (`src/relational/table_store.rs`) fold the row and **all** its `Building`/`Ready` index-entry puts/deletes into the same call;
  * `CatalogService::create_table` writes the table row, all column rows, the default PRIMARY index row **and the ID-counter bump** in one batch (`allocate_id`'s own doc: "the counter's advance and the object's creation are atomic together");
  * `Transaction::commit` submits its whole write-set as one batch.
* `LsmEngine::snapshot()` returns a registered read snapshot at the durable watermark; `range_scan(.., as_of_seq)` is a k-way-merged, version-resolved scan at that sequence, and compaction's version floor honours the oldest live snapshot (`src/compaction/mod.rs`, `oldest_live_snapshot_seq`).

**Therefore a full-keyspace scan at one snapshot sequence `S` is a consistent database image**: it contains whole commits only. It cannot contain a table without its catalog entry, a catalog entry without the state its commit wrote, a row without its index entries, or half a transaction — those would have to be split across two sequence numbers, and the scan sees exactly the set `{seq ≤ S}` of the view captured at call time.

Boundary statement (the precise contract): *the backup contains every commit whose `write_batch` had returned before the backup's snapshot was taken, and possibly some commits that were still in flight at that moment (durable but not yet applied to the memtable are not visible to the scan); it never contains a partial commit.* A commit still in flight at snapshot time was not yet acknowledged, so excluding or including it is equally valid; a commit that depended on an earlier one (same table, serialised by the relational per-table epoch lock held across the durable commit) cannot be included without its predecessor.

Background operations at snapshot time:
* **Compaction** — pinned by the snapshot floor; it can run during the backup (tested).
* **`CREATE INDEX` backfill** — the catalog row is `Building`, the entries are partial. The backup records exactly that. On restore the new instance's existing startup recovery (`recover_incomplete_builds`, wired into both entry points in Increment 14) restarts the build. The checker reports a non-`Ready` index as a *warning*, and does not assert completeness for it. Tested (`backup_during_index_build` in the campaign).
* **Active transactions** — uncommitted write-sets live in the session registry, not in the keyspace; they are, correctly, not in the backup.

## 2. Modes evaluated (do not implement all; choose the smallest that gives reliable restore)

| Mode | Consistency story | Restore story | Verdict |
|---|---|---|---|
| Physical file copy of the data directory while running | **Unsound** unless writes and compaction are stopped: SSTables are immutable but the MANIFEST/WAL/SSTable-set change independently (a flush or compaction between copying the WAL and the manifest yields a state no process ever had). Rejected by the mandate itself ("never copy files blindly while writes are active"). | – | REJECTED |
| Physical snapshot (VSS / filesystem snapshot) | Crash-consistent only if the volume snapshot is atomic; no VSS integration exists; unavailable on this SKU/environment. | Recovery = ordinary crash recovery | NOT AVAILABLE (future option) |
| Physical snapshot + WAL archive (PITR) | Needs WAL segments retained past checkpoint. The engine purges segments after each checkpoint (`Wal::purge_before`); retaining/archiving them is a change to certified WAL/engine behaviour. | replay | **NOT IMPLEMENTED** (see `PHASE_RUBIXDB_DISASTER_RECOVERY.md`) |
| **Logical snapshot backup (chosen)** | Proven above; uses only public engine APIs; no engine change. | Rebuild into a fresh engine through the normal write path; compare digest; run the integrity checker. | **IMPLEMENTED** |
| Incremental / differential | Needs a per-key "changed since S" index; engine has none; would be a full scan anyway with a seq filter. | – | NOT IMPLEMENTED |

Costs of the choice, stated: backup and restore are O(data) and restore is a *rebuild* (replays every key through the write path) — measured, not hand-waved, in the performance document. Compaction history/tombstones are not preserved (restore yields a compact, tombstone-free database — a benefit). Statistics (runtime, in-memory, bounded) are not persisted by the engine and therefore not in a backup; they re-learn after restart.

## 3. Backup format `RUBXBKUP` v1 (all integers little-endian, fully specified in `src/ops/backup.rs`)

```
file    := "RUBXBKUP" version:u32 header_len:u32 header[header_len] header_crc32c:u32 chunk* footer
header  := UTF-8 "key=value\n" lines: format, format_version, product_version, backup_id,
           created_unix_ms, snapshot_seq, [source_instance_id], keyspace, compression=none
chunk   := 0xC1 chunk_no:u64 entry_count:u32 payload_len:u32 payload crc32c:u32   (crc over tag..payload)
payload := ( key_len:u32 val_len:u32 key value )*
footer  := 0xF0 chunk_count:u64 entry_count:u64 payload_bytes:u64 content_digest:u64 snapshot_seq:u64
           crc32c:u32 "RBXBKEND"
```
* Versioned twice (preamble and header, which must agree); a reader of version ≠ 1 fails with `BACKUP_UNSUPPORTED_VERSION` and reads nothing further.
* Entries are in strictly ascending key order across the file (verified while reading).
* `content_digest` = xxh64 over `key_len||key||val_len||val` of every entry, independent of chunking. Restore re-scans the restored database and compares entry count and digest to the footer — an independent check of the restored state against the *file*, not against the restore code's own bookkeeping.
* "Database identity": `backup_id` (process-unique 128-bit label, not a secret), `snapshot_seq`, `source_instance_id` (the opaque id `GET /v1/instance` already exposes unauthenticated). Schema/table/index identity is inside the entries (the catalog), and `verify` decodes it.
* Compression: none in v1 (stated in the header; a reader rejects anything else). No undocumented binary format exists; the layout above is the specification.
* **Not stored:** API keys, instance credentials, filesystem paths, listen addresses, process info. Tested (`backup_contains_no_paths_or_secrets`).

## 4. Integrity of a backup (`verify_backup`, bounded memory: one chunk + catalog)
Checks, in order: magic · version · header length plausibility · header CRC · header syntax and required keys (strict, no duplicate keys, no control characters) · per chunk: tag, plausible length (never allocates more than the file holds, hard cap 128 MiB), CRC32C, chunk numbering, entry framing, key order · footer CRC, end marker, chunk/entry/byte totals, content digest, snapshot sequence · no trailing data · then **semantics**: every catalog row decodes against its system-table schema; schema→database, table→schema, columns contiguous and ≥ 1, primary key within columns, index→table and index columns within columns, constraint→table, **ID counters ≥ every issued id** (a lower counter would re-issue a live id); and for every `Ready` secondary index, *entries == rows* of its table.
Exhaustive evidence: the test `every_single_byte_flip_and_every_truncation_is_detected` flips every single bit-0 of every byte of a real backup (one at a time) and truncates it at every length: **0 undetected**.

## 5. Creation protocol (`create_backup`)
`dest` must not exist → write `dest.partial` (`create_new`) → fsync → **read the partial file back and fully verify it**, and require the read-back digest to equal the digest computed while writing → publish with a *no-replace* operation (`hard_link` then remove the partial; rename fallback guarded by an existence check) → a crash or failure at any point leaves no file or a complete verified one. Cancellation (client disconnect, shutdown) is checked at the start, every 1,024 entries and at every chunk; a cancelled backup removes its partial file. A second concurrent backup is refused (`409 OPERATION_IN_PROGRESS`).

## 6. Restore protocol (`restore_backup`) — RECOVERY class, never overwrites
1. `verify_backup` first — a corrupt or incompatible backup writes **nothing**.
2. Destination must not exist or be an *empty* directory (`DESTINATION_NOT_EMPTY` otherwise; there is no `--force`). Restore into the instance (`--instance NAME`, which also takes the instance lock so it cannot start meanwhile) or any fresh directory (`--data-dir`).
3. Build in a staging directory `.<name>.restoring-<id>` carrying a `RESTORE_IN_PROGRESS` marker; the destination name does not exist yet.
4. Open the engine with **production** durability settings (`ops::open::product_*_config`), apply every entry through atomic `write_batch` calls (≤ 500 ops / 4 MiB), re-scan and compare entry count + content digest with the backup, run the logical integrity checker (any *Error* fails the restore).
5. Clean `engine.shutdown()` (must report fully drained), then ONE directory rename promotes; the marker is removed afterwards (a leftover marker in a promoted directory is harmless — everything was verified before the rename).
6. Failure or crash handling: an in-process failure removes the staging directory; after a crash the next restore to the same destination removes stale staging directories **only if** the name matches *and* the marker is present (an unmarked look-alike directory is never touched — tested).
Crash points are deterministic (`RestorePhase`): after Verified, StagingCreated, EngineOpened, each BatchApplied(n), Loaded, DigestVerified, CheckPassed, EngineClosed, Promoted, MarkerRemoved. Real process-kill evidence is in the certification document.

## 7. Filesystem and API safety
* The HTTP API never accepts a path. A backup is addressed by a *name* (`validate_simple_name`: 1–64 of `[A-Za-z0-9._-]`, not starting/ending with `.`, no Windows reserved device names — 17 hostile names tested) and lives in a server-configured directory (`RUBIXDB_BACKUP_DIR`; the local instance manager uses `<instance>/backups`). With no directory configured the endpoints answer `501 NOT_CONFIGURED`.
* Every `/v1/admin/*` route requires the `Admin` role for **every** method (`auth::required_role`), including `GET` — a reader key cannot list backups or read integrity findings.
* Error bodies never contain a filesystem path: `DEST_EXISTS`, `IO`, `ENGINE` etc. are mapped to fixed messages; backup-damage classifications (`chunk 3: checksum mismatch`) carry no path.
* The CLI (`rubixdb backup verify --file`, `restore --from/--data-dir`) takes paths because it runs as the operator with the operator's own filesystem rights; the destination rules above still apply.
* Destructive operations (`backup delete`, `maintenance purge-orphans --apply`) need an exact confirmation that the **backend** validates again: the exact backup name; the exact entry count shown by the dry run (so a changed database refuses).

## 8. Known limits (not hidden)
* Restore is a rebuild: time and temporary disk are proportional to the data (measured in the performance document); the staging directory needs the final database's size free on the destination volume.
* A backup does not preserve the instance's identity or credentials: restoring into an instance gives the new instance fresh credentials; `source_instance_id` is informational.
* The keyspace is one flat space shared with the raw KV API: raw KV keys outside the two namespaces are backed up (counted as `non_relational_entries`) but cannot be semantically verified.
* `DROP TABLE` leaves the dropped table's rows and index entries in the keyspace (measured; see `PHASE_RUBIXDB_MAINTENANCE_ARCHITECTURE.md`); they are backed up too until purged.
* Point-in-time recovery: NOT IMPLEMENTED.
