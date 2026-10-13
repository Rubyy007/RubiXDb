# rubiXDb — PERSISTENCE MAP (D0 reconnaissance)

Written 2026-10-13 for the durability / PITR / backup mission, Slice 0 (D0). Documentation only: no production code was
edited. Source of truth is the repository at `dde8ae9` (branch `master`, tree clean at the start of the slice). Where an
older document disagrees with the source, the source wins and the disagreement is listed in section 11.

Evidence tags used throughout (a claim without a tag is a description, not evidence):

| Tag | Meaning |
|---|---|
| `[R]` | Read in the source at the cited `file:line` (current HEAD). Not executed. |
| `[M]` | Measured in D0 by a throw-away probe outside the repository (sources and raw output: `docs/D0_BASELINE.md` appendix). Real engine code, disposable temp directories. |
| `[D]` | Stated by an existing repository document; not re-verified in D0. |
| `[U]` | Unverified / NOT TESTED. Listed so nobody mistakes it for a result. |

Platform of every measurement: Windows 10 Home 10.0.19045, NTFS, SATA SSD, `rustc 1.98.1`. Nothing here was run on Linux/ext4/xfs.
"Crash" below means what the existing tests exercise (process kill) unless the text says otherwise. **No power-loss
result exists anywhere in this repository** (`PROJECT_STATE.md`, `OPEN_ITEMS.md`, `PHASE_RUBIXDB_DISASTER_RECOVERY.md` §6).

---

## 1. What is persisted, and where

Data directory layout of a fresh engine `[M]` (probe H3: one `put`, then `shutdown`):

```
MANIFEST            0 B   (created empty at open; edits appended later)
sstables/                 (empty until the first flush)
wal/
  LOCK              0 B
  wal-00000000000000000001.log   51 B  (24 B header + one 27 B frame)
```

| Artifact | Content | Writer | Format indicator / integrity | Sync discipline |
|---|---|---|---|---|
| `wal/wal-<20 digits>.log` | WAL segment: 24 B header then frames | `FileWal` (`src/wal/mod.rs`) | header: magic `RBXWALv1`, `format_version=1`, `segment_id`, `flags=0` (`src/wal/format.rs:20-24,63-70`); frame `len:u32 | crc32c:u32 | body(seq:u64, op:u8, …)` (`format.rs:152-188`); ops PUT/DELETE/CHECKPOINT_MARKER/GROUP (`format.rs:30-44`) | file `sync_all` per batch; see 3.1-3.2 |
| `wal/wal-<id>.tmp` | segment under construction | `create_new_segment_file` | not matched by `parse_segment_file_name` (`mod.rs:234-242`) | `sync_all` before rename |
| `wal/LOCK` | advisory lock target | `acquire_exclusive_lock` (`file_io.rs:322`) | none | none |
| `MANIFEST` | append-only edit log: AddSstable / RemoveSstable / SetCheckpoint | `Manifest::append_sync` (`src/manifest/mod.rs:114`) | per-frame `len|crc32c|body` only. **No file magic, no version, no whole-file checksum** (section 8) | `write_all` + `sync_all` per edit |
| `sstables/<20 digits>.sst` | immutable sorted run | `sstable::write_from_sorted_records` (`src/sstable/writer.rs:127`) | footer magic `RBXSST01`, `FORMAT_VERSION=1` (`src/sstable/format.rs:13-14`); CRC32C on every data block, bloom, index and footer (`format.rs:153-180,244-262,324-344,383-417`) | tmp `sync_all` → rename → dir fsync |
| `sstables/<id>.sst.tmp` | table under construction | same | none (swept at open, `lsm/mod.rs:2713`) | — |
| catalog, rows, indexes | **no files of their own**: all live in the one LSM keyspace (`0x00` catalog, `0x01` relational rows and index entries; any other first byte = raw KV) | `LsmEngine::write_batch` | per-row envelope version `ROW_FORMAT_VERSION` `[D]`; `src/catalog` performs no filesystem I/O `[R]` (grep: zero matches) | via WAL |
| `DATA_FORMAT` (+`.tmp`) | directory format stamp (`rubixdb-data-format=1`, `wal-segment=1`, `sstable=1`, `catalog-row=1`) | `ops::format::stamp_if_fresh` (`src/ops/format.rs:238-257`) | text, parsed strictly; unsupported value → refuse to open | tmp `sync_all` → rename; **no dir fsync** |
| `WAL_CLEAN_STOP` (+`.tmp`) | attestation of the WAL end after a graceful stop | `ops::wal_tail` | text with CRC32C (`wal_tail.rs:84-168`) | `[D]` |
| `wal-quarantine/` | copy of a torn WAL tail before recovery removes it | `ops::wal_tail` | — | temp, fsync, rename `[D]` |
| `RESTORE_IN_PROGRESS` | marker inside a restore staging dir | `ops::restore` (`restore.rs:163-169`) | — | none |
| `<instance>/backups/*.rbxbackup` (+`.partial`) | logical backup file | `ops::backup::create_backup` | `RUBXBKUP` magic + version + header CRC + per-chunk CRC + footer digest (`src/ops/backup.rs:18-26`) | fsync file; **no dir fsync** after publish |
| `instance.json` | instance identity (`instance_id`, `name`, `created_at`, `api_port`) | `instance/src/manifest.rs:100-101` | serde JSON, **no version field**; written with `fs::write` then rename, **no fsync** | none |
| `credentials.json` | admin key | `instance/src/credentials.rs:126-152` | JSON | stage → fsync → read-back → rename |
| `security.log*` | bounded security/audit log | `api/src/security_log.rs` | JSON lines | write errors swallowed by design (`security_log.rs` header) |

Out of scope for this mission but named so it is not confused with the engine manifest: **`instance.json` is not the LSM
`MANIFEST`**. When the mission text says "manifest" it means `src/manifest` (`MANIFEST`).

Filesystem call-site inventory (production code, non-test) `[R]`, relevant to the persistence abstraction (Slice 1b):
`src/wal/mod.rs` (16 sites: 261, 348, 350, 358, 362, 381-382, 403-404, 422, 449, 578-579, 1021, 1071, 1093),
`src/wal/file_io.rs` (103-104, 126, 279-296, 327, 359), `src/wal/group_commit.rs:1535` (the leader's `sync_all`),
`src/manifest/mod.rs` (47, 86, 88, 97, 117), `src/sstable/writer.rs` (43, 137, 143, 224, 227, 231), `src/sstable/mod.rs:115`,
`src/sstable/reader.rs:93`, `src/lsm/mod.rs` (2700, 2714, 2731, 2842, 2993, 2996, 3072), plus `src/ops/{backup,restore,format,wal_tail,open,check,sstable_integrity}.rs`,
`instance/src/{manifest,credentials}.rs`, `api/src/security_log.rs`. About 40 sites in the four protected engine modules and `lsm`;
roughly the same again in the product layer. Only the WAL segment append/truncate/sync goes through a trait today (`WalFile`,
`src/wal/file_io.rs:166`); create, rename, remove, directory sync, the group-commit leader's `sync_all`, the MANIFEST and the
SSTable writer all call `std::fs` directly.

---

## 2. Committed write → durable storage (the acknowledgement path)

`[R]` unless tagged. Product configuration: `SyncMode::GroupCommit { max_wait: 5 ms, max_batch_bytes: 256 KiB }` (`src/ops/open.rs:14-22`).
`LsmEngine::open` **requires** GroupCommit mode: `GroupCommitter::new` returns `Unsupported` for an `Immediate`-mode WAL `[M]` (probe H3, first run).

1. Entry points `LsmEngine::put` / `delete` / `write_batch` (`src/lsm/mod.rs:1892, 1908, 1932`). `write_batch` encodes the whole write-set as **one**
   `WalOpOwned::Group` = one frame = one sequence number; this is the engine's atomic commit unit (`mod.rs:1956`; `src/wal/format.rs:37-44`).
   The relational layer folds a row, all its index entries, a DDL statement's catalog rows or a transaction's write-set into one such call `[D]`
   (`PHASE_RUBIXDB_BACKUP_RESTORE_ARCHITECTURE.md` §1).
2. `reject_if_storage_full` (`mod.rs:1841`) fails fast with `StorageExhausted` before any WAL append when the engine is in `StorageFull`.
3. `pool.submit(...)` hands the op to the batch coordinator, which calls `FileWal::append` / `append_group` (`src/wal/mod.rs:788, 971`). The bytes are written
   with one positional `write_all_at` per contiguous run (`src/wal/file_io.rs:91-117`). **No fsync here.** On a failed write the segment is truncated back to its
   pre-append length and `sync_all`'d; if that rollback fails the segment is poisoned and refuses further I/O (`file_io.rs:101-115`).
4. Durability barrier: `GroupCommitter::run_as_leader` (`src/wal/group_commit.rs:1255`). The leader snapshots `(cloned File, batch_max_seq)`, then calls
   `file.sync_all()` on the clone (`group_commit.rs:1524-1536`) — **this call bypasses `SegmentIo`/`WalFile`**, so the existing `FaultInjectingIo` cannot reach it;
   a per-committer test hook `fsync_fault_hook` stands in (`group_commit.rs:1524-1534`). Only after `Ok` is the watermark published:
   `durable_through.fetch_max(batch_max_seq, Release)` (`group_commit.rs:1309`) and waiters are woken.
5. On `sync_all` error: `finish_batch_with_error` sets `poisoned = FsyncFailed(kind)` **permanently** and wakes everyone; `durable_through` never moves backward and the
   committer is never reused (`group_commit.rs:1333-1337, 1549-1556`). The failed fsync is never retried. This is the correct reaction to "fsync failed → page state unknown".
6. Acknowledgement: `completion.wait()` returns the `WalPosition` once `durable_through >= seq`; only then is the op applied to the active MemTable
   (`apply_after_durable` `mod.rs:1967`, `apply_batch_after_durable` `mod.rs:1989`) under one write guard, and `put` returns the seq.
   Consequences: (a) **durable ⇒ acknowledged-able, never the reverse**; no code path acknowledges before `sync_all` returned `Ok` (`sync_data` appears nowhere; `[R]` by grep, not mutation-tested);
   (b) a call can return `Err(CapacityExceeded)` *after* the write is durable and applied (freeze refused, `mod.rs:2027-2065`) — accepted contract, not data loss;
   (c) a caller can see an error (timeout/poison) for a write whose bytes are already in the file and may be replayed after restart — **outcome-unknown writes exist**.

What `sync_all` means on this machine is the OS/driver's business: on Windows `std` maps it to `FlushFileBuffers`, on Unix to `fsync`. D0 did not verify the Windows mapping or the
device's cache behaviour `[U]`. Whether this SATA SSD honours flushes is `[U]`.

---

## 3. Ordering of every durability-relevant step

Legend for the "barrier" column: **F** = file `sync_all`; **D** = directory fsync (`fsync_dir`); **—** = nothing. `fsync_dir` is a real `open(dir)+sync_all` on Unix
and **a documented no-op on Windows** (`src/wal/file_io.rs:260-296`) — so every "D" below is *absent on the platform of every test run to date*.

### 3.1 WAL segment creation (`create_new_segment_file`, `src/wal/mod.rs:339-424`)

| # | Step | Barrier after | Failure handling |
|---|---|---|---|
| 1 | best-effort `remove_file(wal-<id>.tmp)` (stale leftover) | — | ignored |
| 2 | `create_new` tmp, `write_all(24 B header)` | F (`sync_all`, `mod.rs:356-358`) | remove tmp |
| 3 | drop handle (Windows rename restriction) | — | |
| 4 | `rename(tmp → wal-<id>.log)` | — | remove tmp |
| 5 | `fsync_dir(wal/)` | D (Unix only) | remove renamed file |
| 6 | `AbortPoint::AfterHeader` hook | — | |
| 7 | reopen RW at final path | — | |

Gaps `[R]`: `fs::create_dir_all` for `wal/` (`mod.rs:261`) is never followed by an fsync of the data directory; `MANIFEST` is created by `OpenOptions::create` (`manifest/mod.rs:88-93`)
and its directory entry is never fsynced; `sstables/` is created by `create_dir_all` (`lsm/mod.rs:2700`, `writer.rs:137`) likewise. On a first-ever write to a brand-new directory the first
acknowledged commit is durable in the *file* but the *path* to it is not fsynced on any platform.

### 3.2 Rotation (`FileWal::rotate`, `mod.rs:1006-1043`)

Create the new segment first (3.1); only then `active.sync()` the old one (F); remember its max seq in `sealed_max_seq`; swap the active handle. A failure to seal removes the new file
and leaves the WAL as it was. `durable_seq` advances on a successful seal.

### 3.3 Flush (background thread, `src/lsm/mod.rs:3340-3437`)

| # | Step | Barrier after | Note |
|---|---|---|---|
| 1 | `write_from_memtable` → tmp, blocks, bloom, index, footer | F on tmp (`writer.rs:224`) | |
| 2 | `rename(.sst.tmp → .sst)` | — (`writer.rs:227`) | |
| 3 | `fsync_dir(sstables/)` | D (Unix only) (`writer.rs:231`) | failure leaves the published table for the next open's sweep |
| 4 | Manifest `AddSstable` | F (`append_sync`) | visible to recovery only from here |
| 5 | insert table into live list | — | |
| 6 | `pool.rotate()` | F on old segment, D | marker will land in the new segment |
| 7 | WAL `CheckpointMarker{flushed_through_seq}` via the normal submit/wait path | F (group commit) | durable before step 8 |
| 8 | Manifest `SetCheckpoint{seq, wal_segment_id, wal_offset}` | F | |
| 9 | drop the immutable MemTable; `purge_before(meta.max_seq)` | D after unlinks (Unix only) | WAL history **destroyed** here |

Each step records its own success so a retry never repeats a durable step (`lsm/mod.rs:3325-3327`). Fault-hook points: `FlushFaultPoint::{BeforeSstableWrite, AfterSstablePublish, AfterRotate, AfterCheckpointMarker, AfterSetCheckpoint}` (`mod.rs:288-309`).
Recovery ignores `wal_segment_id`/`wal_offset` from `SetCheckpoint`; it filters replay by `seq <= flushed_through_seq` only (`mod.rs:1563`) `[R]`.

### 3.4 Compaction (`compact_once_impl`, `src/lsm/mod.rs:2905-3097`)

| # | Step | Barrier | Note |
|---|---|---|---|
| 1 | capture **all** live tables (full merge) | | `mod.rs:2948` |
| 2 | merge → one output via the same writer (3.3 steps 1-3) | F, D | `oldest_live_snapshot_seq` decides which versions survive |
| 3 | guard: output record count within the reader's bound, else remove + `fsync_dir` (best effort) and fail | D (Unix) | F-08 / GAP-B |
| 4 | Manifest `AddSstable(output)` | F | |
| 5 | Manifest `RemoveSstable` **one edit per input**, each `append_sync` | F each | **non-atomic** sequence |
| 6 | swap live list | | |
| 7 | `remove_file` of each input whose `Arc` count is 1; others deferred in memory (`pending_compaction_deletes`) | — | **no dir fsync after unlink** (`mod.rs:3072`); deferral list is lost on crash and handled by the orphan sweep |

Fault-hook points: `CompactionFaultPoint::{BeforeOutputWrite, BeforeManifestAdd, AfterManifestAdd, DuringRemoveSequence, AfterAllRemoves, BeforePhysicalDelete}` (`mod.rs:356-363`).
Crash between steps 4 and 5-end leaves `{output} ∪ {remaining inputs}` live. This is *safe by construction* `[R]`: `retain_versions` always keeps the newest version of every key **including a tombstone**
(`src/compaction/mod.rs:109-114`), and every record keeps its original seq, so the duplicates resolve identically whatever subset of inputs survives. (Consequence: tombstones are never garbage-collected — a space
cost, not a correctness one.) Not re-executed in D0 `[U]`; the fault points exist and `[D]` tests cover them.

### 3.5 WAL purge (`purge_before`, `mod.rs:1045-1107`)

Unlink each eligible sealed segment (`sealed_max_seq < watermark`), then one `fsync_dir` if any was removed (D, Unix only). A lost unlink resurrects a segment whose records are already below the checkpoint:
a space cost only (`mod.rs:90-100`). The active segment is never purged.

### 3.6 Manifest append (`src/manifest/mod.rs:114-121`)

`write_all(frame)` at the file cursor, then `sync_all`. **There is no rollback of a failed or short write** (the WAL has one; the MANIFEST does not). No directory fsync when the file is created.
A torn tail is truncated at open with `set_len` but **without** a following `sync_all` (`manifest/mod.rs:96-98`); the truncation is idempotent so a lost truncation is re-done next open.

### 3.7 Backup publication and restore promotion

* Backup (`ops/backup.rs:254-337`): `create_new(dest.partial)` → write → `sync_all` → read-back full verification → `hard_link(partial → dest)` (rename fallback) → `remove_file(partial)`. **No fsync of the parent directory** after publication. A crash leaves `*.partial` that nothing sweeps (a retry removes it on its own error path, `backup.rs:275-278`, so the first retry fails and the second succeeds) `[R]`, not executed.
* Restore (`ops/restore.rs:120-211`): verify the backup → stage into `.<name>.restoring-<id>` with a marker → apply via `write_batch` → digest compare → `check_engine` → clean `engine.shutdown()` → **one `rename(staging, dest)`** (`restore.rs:188`) → marker removed best effort. **No fsync of the parent directory** after the rename. The restored database is a **new** database: fresh WAL, fresh sequence numbers, `snapshot_seq` of the source is informational only.

---

## 4. Startup recovery and orphan handling

### 4.1 Order

Product entry points (`rubixdb gui`, `restore`, `check`) run a guard **before** the engine opens:

1. `ensure_compatible` — `DATA_FORMAT` marker (`src/ops/format.rs:62-106`).
2. `wal_preflight` — read-only `replay_streaming`; **refuses** (`WAL_CORRUPT`) on `corrupted_segments_count > 0` or any read error (`format.rs:148-174`).
3. SSTable preflight (ADR-SST-01) `[D]`, then the tail policy: attestation / quarantine (`ADR-WAL-01`) `[D]`. Wired at `cli/src/host.rs:375`; `open_engine_for_ops` uses steps 1-2 (`src/ops/open.rs:38-54`).

Engine open (`LsmEngine::open`, `src/lsm/mod.rs:1544-1726`):

1. `manifest::replay_readonly` (read-only; checkpoint boundary).
2. `wal::replay_streaming` under a shared lock; records with `seq <= checkpoint` are skipped, others applied to a fresh MemTable (`mod.rs:1560-1575`).
3. `FileWal::open_for_recovery` — exclusive lock, directory scan, **in-place truncation of a torn tail** with `set_len` + `sync_all` (`wal/mod.rs:575-589`), creates segment N+1 if no segment is usable (`mod.rs:620-629`).
4. `Manifest::open_after_exclusive_lock` — re-replay, truncate a torn tail — then `reconcile_sstables_with_manifest` (4.3).

**The bare engine does not honour the WAL's own halt contract.** `LsmEngine::open` discards the replay summary (`let _summary =` `mod.rs:1560`, `_replay` `mod.rs:1578`), although `wal/mod.rs:613-617` says the caller "is already required to halt on non-empty corrupted_segments".
`[D]` `src/ops/format.rs:129-139` documents the compensation: only the product entry points refuse. Any other caller of `LsmEngine::open` (tests, examples, an embedder) silently loses the corrupt segment and every segment after it.

### 4.2 WAL scan rules (`src/wal/mod.rs:482-632`, `src/wal/recovery.rs:63-193`)

* Header unreadable, wrong magic/version/flags/segment id, or file shorter than 24 B → corruption, regardless of position.
* A bad frame is a **torn tail** only if its *claimed* extent ends exactly at end-of-file (or the declared length itself is out of range and nothing but that header follows); otherwise corruption.
* Scan **stops at the first corrupt segment** and discards *all* of that segment's records, including valid ones before the damage, and everything after it (Group 3.1, a deliberate amendment of WAL Spec §6.2).
* A torn-tail shape in a non-last segment is corruption.

### 4.3 Orphan and leftover handling

| Artifact found | Handling | Source |
|---|---|---|
| `*.sst.tmp` | deleted at open | `lsm/mod.rs:2713` |
| `*.sst` in Manifest live set | opened + footer/bloom/index validated; failure → **open fails** | `mod.rs:2724-2726` |
| `*.sst` listed as ever-added but not live (compaction left it) | deleted | `mod.rs:2727-2731` |
| valid `*.sst` unknown to the Manifest | validated, **adopted** with a fresh durable `AddSstable` | `mod.rs:2732-2745` |
| live Manifest entry with no file | **open fails** `Corruption` | `mod.rs:2749-2757` |
| data-block damage in a live table | not found at open (blocks are read lazily); found at read, by the F-08 preflight/verification `[D]`, or by `rubixdb check` | `reader.rs:92-174` |
| unparseable file name in `sstables/` | ignored | `mod.rs:2718` |
| `wal-<id>.tmp` | invisible to scans; removed when the same id is created again | `wal/mod.rs:304-315, 348` |
| torn WAL tail | quarantined (product layer) then truncated in place | `ops/wal_tail.rs` `[D]`, `wal/mod.rs:575-589` |
| torn MANIFEST tail | truncated in place (not synced) | `manifest/mod.rs:96-98` |
| trailing MANIFEST frame with bad CRC | treated as torn, **silently dropped** | `manifest/recovery.rs:110-117` |
| corrupt non-tail MANIFEST frame / unknown edit type / checkpoint regression / REMOVE of never-added id | open fails `Corruption` | `manifest/recovery.rs`, `state.rs:59-111` |
| `RESTORE_IN_PROGRESS` staging dir | removed by the *next restore to the same destination* only, and only if name pattern **and** marker match | `restore.rs:95-118` |
| `*.rbxbackup.partial` | never swept at startup | `[R]` |
| `DATA_FORMAT.tmp` | recognised as this build's interrupted stamp only if byte-identical | `format.rs:119-123` |

Recovery is idempotent by construction (the same tail is truncated again, an adoption appends an idempotent duplicate-safe edit). Repeated-crash-during-recovery is `[D]` covered by F-11
(process kill at 14 named startup points, 48 runs, CLI layer, Windows only); **power loss at those points is `[U]`**.

---

## 5. Snapshot / flush / compaction / recovery interaction

* A `Snapshot` is a **registered sequence number** (`lsm/mod.rs:501, 2207`). It pins nothing else: not SSTable files, not WAL segments, not a MANIFEST version. It affects exactly one decision: which old versions compaction may drop (`oldest_live_snapshot_seq` → `retain_versions`, `compaction/mod.rs:97-142`, call at `lsm/mod.rs:2965`).
* Input SSTable *files* stay readable for a scan in flight only through `Arc<SsTable>` reference counts (unlink deferred while `Arc::strong_count > 1`, `mod.rs:3067-3076`). That is an in-process mechanism: it protects a running scan, not an external copier.
* Flush ends by destroying WAL history (3.3 step 9). Compaction ends by destroying superseded versions and input files (3.4). Neither consults any backup, archive or "retention" notion — none exists (`grep -i archiv` over `src`: nothing).
* `snapshot_seq()` is `committer_stats.durable_through` (`mod.rs:2194-2196`): only durable data is ever snapshot-visible.
* Recovery trusts `MANIFEST.checkpoint` blindly: WAL records with `seq <= flushed_through_seq` are skipped. The WAL's `next_seq` is derived **only** from the records still in the WAL (`wal/mod.rs:949`), never from the checkpoint. If the WAL contents are missing or end below the checkpoint, new commits are numbered below it — see finding F-D0-5 (section 9).

---

## 6. Is history retained long enough for PITR? — **No**

`[R]`: (1) `purge_before(meta.max_seq)` after every flush deletes all sealed WAL segments below the checkpoint (`lsm/mod.rs:3437`); (2) there is no archive hook, no retention setting, no "keep segments for backup N"; (3) compaction keeps only the newest version of each key when no snapshot is live, and the newest tombstone;
(4) `replay_streaming` has no upper bound (it replays everything it finds; a callback `Err` can stop it, but there is nothing to replay *from*); (5) a logical restore creates a new WAL lineage, so sequence numbers are not preserved across backup/restore.
`[D]` `PHASE_RUBIXDB_DISASTER_RECOVERY.md` §5 and `PHASE_RUBIXDB_BACKUP_RESTORE_ARCHITECTURE.md` §2 state the same. Slice 6 is therefore **BLOCKED** until WAL retention/archiving exists (section "PITR" in `docs/PREREQUISITES.md`).

What *is* a usable LSN-like position `[R]`: the WAL `seq` is a gap-free `u64` assigned in append order, WAL-wide (not per segment), monotone within one database lineage, and one group frame = one atomic transaction. The backup header records `snapshot_seq` (`backup.rs:77`). It is stable across restart *only while the WAL still contains the latest records* (see F-D0-5).

---

## 7. Does a backup have an atomic consistency boundary today?

* **Logical backup: yes, by construction.** `create_backup` takes `engine.snapshot()` (a durable-watermark seq) and scans the whole keyspace at that seq (`backup.rs:355-414`). Because every logical commit is one group frame with one seq, the scan contains whole commits only. Compaction is held off from dropping needed versions by the snapshot floor. Writers are not blocked. `[D]` measured under load in `PHASE_RUBIXDB_DISASTER_RECOVERY.md` §3 (0 missing, 0 torn pairs). In-flight commits at snapshot time may be included or excluded (neither was acknowledged yet).
* **Physical backup (file copy of the data directory): no.** No quiesce, no pin, no manifest-consistent file list, no WAL pinning. `[D]` explicitly REJECTED by the earlier design (`BACKUP_RESTORE_ARCHITECTURE.md` §2). Compaction can unlink an input file at any moment (deferral covers only in-process readers). The only documented way to copy a database consistently is to stop it.
* The backup file is **unencrypted**, has `backup_id` (process-unique random label), `snapshot_seq`, optional opaque `source_instance_id`, a whole-content xxh64 digest and per-chunk CRC32C. There is no catalog of backups, no parent/child relationship, no per-object content hash, no database identity that survives the instance (`instance_id` lives in `instance.json`, not in the data directory).
* Restore is a full rebuild (O(data), `[D]` 72 s for 1 M rows on this machine) and never overwrites.

---

## 8. The manifest format, readers, writers, compatibility

**Layout** `[R]` (`src/manifest/format.rs:84-92`, `recovery.rs:64-149`): a file of back-to-back frames, nothing else.

```
frame := body_len:u32 LE | crc32c(body):u32 LE | body
body  := edit_type:u8 | fields
  1 ADD_SSTABLE    id:u64 min_seq:u64 max_seq:u64 file_size:u64     (body 33 B)
  2 REMOVE_SSTABLE id:u64                                            (body  9 B)
  3 SET_CHECKPOINT flushed_through_seq:u64 wal_segment_id:u64 wal_offset:u64   (body 25 B)
```

* **Version indicator: none.** No magic, no header, no version byte, no "end" record, no whole-file checksum. `MAX_EDIT_BODY_LEN = 64` (`recovery.rs:19`); an unknown `edit_type` byte → `Corruption` (`format.rs:158-160`).
  An older binary meeting a newer manifest that only added an edit type would report **corruption**, not "unsupported version". `[D]` the engine says so itself: `src/ops/format.rs:11-14` ("MANIFEST: frames only — no file magic, no format version… recorded as an engine ADR item").
* **Checksum:** CRC32C per frame body only. The *length* field is not covered by its own checksum. Torn vs corrupt is decided by position (a bad trailing frame is "torn" and silently dropped, `recovery.rs:110-117`).
* **Readers:** `manifest::replay_readonly` (`mod.rs:43`), `Manifest::open_after_exclusive_lock` (`mod.rs:85`) — both call `recovery::replay`; further callers of `replay_readonly`: `lsm/mod.rs:1553`, `ops/sstable_integrity.rs:88`, `ops/check.rs:870`, `ops/wal_tail.rs:941, 1077`. **Writers:** `Manifest::append_sync` only; callers: flush thread (`lsm/mod.rs:3365, 3421`), compaction (`3016, 3032`), reconcile adoption (`2742`).
* **State rules** (`state.rs:59-111`): ADD/REMOVE idempotent; REMOVE of a never-added id → `Corruption`; `SET_CHECKPOINT` regression → `Corruption`.
* **Compatibility guarantees today:** none beyond "an unreadable manifest fails closed". There is no migration path and no downgrade policy.
* **Lock discrepancy** `[M]`: `replay_readonly` tries `acquire_shared_lock_if_present(<data_dir>/LOCK)` (`manifest/mod.rs:44-45`), but the lock file is `<data_dir>/wal/LOCK` (`wal/mod.rs:283-285`); `<data_dir>/LOCK` does not exist (probe H3). The shared lock is therefore a silent no-op (`Ok(None)`), contrary to the module comment ("the WAL's own directory lock file"). Exclusivity of the engine itself is unaffected (the WAL exclusive lock, plus the instance lock); the read-only replay is simply not excluded from a live writer.

Other format indicators present in the tree: WAL segment header (magic + version, strict), SSTable footer (magic + version, strict), backup (magic + version twice), `DATA_FORMAT` (directory level, strict), row envelope version byte; **none** in `MANIFEST`, `instance.json`.

---

## 9. D0 findings (measured or read) that the later slices must resolve

All five are about *recovery classification* or *guards*, found while reading; measured where marked. None is a claim that data was lost in a real crash — reachability on a real power cut is `[U]` for every one.

**F-D0-1 `[M]` Zero-filled WAL tail ≥ 16 bytes ⇒ the whole segment is declared corrupt.**
Probe H1 (3 acknowledged records, then bytes appended to the last segment, `wal::inspect`):

| appended tail | records trusted | truncated | corrupted_segments |
|---|---|---|---|
| 4 or 8 zero bytes | 3 | true | [] |
| 16, 64, 4096 zero bytes | **0** | false | **[1]** |
| a valid frame then 4096 zeros | **0** | false | **[1]** |
| 8×0xFF, 20-byte partial frame, valid-length frame with zeroed body | 3 | true | [] |

Cause `[R]`: an all-zero 8-byte frame header has `length=0`, `crc=0`, and `crc32c("")==0`, so the CRC check passes and the empty body then fails to decode; because that frame is not provably the last (`recovery.rs:125-186`), it is classified corruption.
Why it matters: a file whose size was extended but whose data blocks were not written (a classic crash artifact on delayed-allocation filesystems and on NTFS) produces exactly this shape. Whether *this* machine's NTFS produces it after a power cut is `[U]` (no power-cut capability). Effect at the product layer: `WAL_CORRUPT`, start refused, directory untouched (fail-closed, not silent). Effect at the bare engine: all of the segment's records are silently ignored (4.1). → Slice 1b/1c must decide the contract (torn tail of arbitrary zero length is a *torn* write) and, if the engine must change, an ADR for `src/wal/recovery.rs`.

**F-D0-2 `[M]` A zero-filled MANIFEST tail ≥ 8 bytes makes the engine refuse to open.**
Probe H2: tail of 4 zero bytes → truncated (ok); **8, 16, 64 zero bytes → `Corruption: manifest: at offset 82: manifest edit: empty body (missing edit_type)`** (an `Err`, not a torn tail, because the empty body passes the CRC and then fails `decode_body`, and that error is returned regardless of position — `recovery.rs:124-129`). A 20-byte partial frame and a full-length frame with a zeroed body are correctly treated as torn.
The probe case "last 5 bytes zeroed" did not change the frame (those bytes were already zero) and is **not** a valid data point; it is excluded.

**F-D0-3 `[M]` No rollback of a failed MANIFEST write; a retry after a short write corrupts the file.**
`Manifest::append_sync` has no equivalent of `SegmentIo::append`'s truncate-on-failure (3.6). Probe H2 built `[2 valid frames][first N bytes of frame 3][full frame 3]` (what a retry after a short write would leave):
N=3 → `implausible body length 553648161`; N=7/12/20/30/40 → `checksum mismatch at offset 82 (not the trailing frame — real corruption, not a torn write)`. Every case = open fails. Reachability — a `write_all` that returns `Err` after writing a prefix of a 41-byte frame (ENOSPC/EIO) — is `[U]`; the flush thread *does* retry the whole flush after such an error (`lsm/mod.rs:3494-3566`), appending after the prefix.
The fault point "manifest write" in Slice 1a exists precisely to settle this.

**F-D0-4 `[R]`+`[M]` Manifest shared lock is a no-op** (section 8). Low severity, but any future physical-backup or archive tool that assumes "manifest readers are excluded from writers" would be wrong.

**F-D0-5 `[M]` (consequence) / `[U]` (reachability) Sequence numbers can restart below the checkpoint, and acknowledged writes then vanish after a clean restart.**
Probe H4: 400 writes with a small MemTable → `checkpoint_seq=365`, `durable_seq=406`, 12 Manifest edits, 6 SSTables; clean stop; then **all `wal/*.log` files deleted** (disposable directory); reopen succeeds silently (`checkpoint_seq=365`, next `durable_seq=0`); two `put`s are acknowledged at **seq 1 and 2**, readable; clean shutdown and reopen →
`get(post-1)=None`, `get(post-2)=None`, `get(a-0000)=Some(100)`, `RecoveryStats{ wal_records_visited:2, wal_records_applied:0, wal_records_skipped_by_checkpoint:2 }`. Two acknowledged commits were silently discarded.
The premise (WAL content missing/below the checkpoint) is *not* shown reachable by D0. The most plausible reachable route is a lost directory entry for a newly created segment (the Windows `fsync_dir` no-op, 3.1) or a lying flush, combined with a crash (no `WAL_CLEAN_STOP`, so the product layer's attestation check does not apply). `[D]` `OPEN_ITEMS.md` (2026-10-11, OI-F11-04 line) already records "a log ending below the manifest checkpoint is not refused". The recovery oracle of D1c must check this invariant explicitly.

---

## 10. What this map does NOT establish

* Any power-loss, torn-sector, lying-fsync, reordering or directory-entry-loss behaviour (no capability, no simulator yet).
* Any Linux/macOS/ext4/xfs behaviour (platform differences listed in 3.x are `[R]` of `cfg(unix)` vs `cfg(windows)` source only).
* That `sync_all` on this machine reaches stable media.
* The Windows semantics of `std::fs::rename` w.r.t. durability and replace atomicity (not verified; no `MOVEFILE_WRITE_THROUGH` is requested by this code either way).
* Concurrency behaviour of the product layer beyond what the existing suites cover.
* Performance, memory and temp-disk figures (D0 measured none).

---

## 11. Documentation vs source discrepancies found in D0

| # | Document says | Source says |
|---|---|---|
| 1 | `src/wal/mod.rs:162-171` (`SyncMode` doc): a `GroupCommit` config makes `open_for_recovery` return `Unsupported` | `open_for_recovery` accepts it (`mod.rs:919-969`); the module header (`mod.rs:76-89`) says so too. The stale doc comment is on a protected path; not edited. Conversely `GroupCommitter::new` rejects `Immediate`. |
| 2 | `src/manifest/mod.rs:1-6, 33-45` and `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md` §10: the Manifest reuses the WAL's directory lock file | the WAL lock is `<data>/wal/LOCK`; the Manifest looks at `<data>/LOCK` (F-D0-4) |
| 3 | `src/wal/mod.rs:608-619`: the caller "is already required to halt on non-empty corrupted_segments" | `LsmEngine::open` discards the result (4.1); only the product layer halts |
| 4 | `missions/ACTIVE.md` (Mission 2): "nothing is committed (instructed)" | `git log` shows `dde8ae9` containing Mission 1 D/E and Mission 2; `git status` is clean. `ACTIVE.md` is stale on this point; unrelated to persistence, left as written and recorded in `OPEN_ITEMS.md`. |

None of these contradicts the persistence behaviour mapped above; they are comment/document staleness, reported per the "docs conflict with source" rule.


---

## 12. D1a addendum (2026-10-13)

* **Fault points now exist** at the boundaries listed in section 3: a registry (`src/faults.rs`, feature `fault-points`, absent from product builds) with 64 reachable named points and two declared-unreachable ones (`manifest.replace`: no rewrite path exists in `src/manifest/`; `startup.host_ready_sent`: CLI host only); status table and call sites in `docs/adr/ADR-FAULTPOINTS-01.md` (§3.1, §12.1, §12.8). The existing hooks of the D0 inventory are unchanged.
* **New finding F-D1a-1 (section 5 / 3.3 / 3.5 are affected):** the flush/checkpoint/purge logic assumes the MemTable has applied every seq up to the table's `max_seq`, but `put`/`delete`/`write_batch` acknowledge from the WAL first and apply afterwards, in arbitrary order among writers of one batch. A freeze between two members makes the checkpoint cover a seq present in no SSTable, and recovery skips WAL records `seq <= checkpoint` (`src/lsm/mod.rs:1563`). Reproduced on a pristine export of `dde8ae9`, also across a graceful stop (docs/D0_BASELINE.md, D1a appendix C). Contract gap G13.
* **F-D0-3 observed at the real call site** (D1a appendix D): a short write at `Manifest::append_sync` followed by the flush thread's retry leaves a MANIFEST the product start sequence refuses to open.
* Section 11 (doc-vs-source discrepancies) is unchanged.

## 13. D1a decision-gate addendum (2026-10-13)

* Section 3.3 step 8 / section 5 describe the checkpoint as "every commit with seq <= it is in a live SSTable". **That holds only when MemTable apply order equals seq order** (single writer); under concurrent writers the value written is the frozen table's `max_seq`, which can exceed a still-unapplied lower seq (F-D1a-1, deterministic reproduction `tests/pending_fixes.rs`). A PROPOSED replacement invariant and four options are in `docs/adr/ADR-CHECKPOINT-CONTIGUITY-01.md`; nothing is changed in the code.
* Section 7 ("logical backup: consistent by construction") must be read with **F-D1a-2**: `snapshot_seq()` is the durable watermark and can exceed the applied prefix, so a snapshot or backup can omit a durable commit (observed, `observe_f_d1a_2_*`). Needed before D3.
* New test seam: fault point `lsm.before_apply` (between the WAL acknowledgement and the MemTable apply).

## 14. D1b addendum (2026-10-13)

* **Section 9 findings after the gate:** F-D1a-1, F-D0-1, F-D0-2, F-D0-3 and F-D0-5 are **fixed** (maintainer approval 2026-10-13; `docs/D1a_DECISION_GATE.md` §10); F-D0-4 is accepted and moves into the D2 manifest ADR; F-D1a-2 stays open for D3. The `[M]` evidence in sections 3 and 9 describes the tree *before* those fixes and is kept as written.
* **Section 10 ("what this map does not establish"):** the persistence abstraction now exists (`crate::persist`, `docs/PERSISTENCE_MODEL.md` §5). Every durable byte and namespace change of `src/wal`, `src/manifest`, `src/sstable`, `src/lsm` and `src/ops` goes through it; `instance`, `api`, `cli` and `sql` do not (listed in the model, §5.2).
* **Section 3.1 / gap G6, measured:** the only directories the engine ever syncs are `wal/` (`wal/mod.rs:408`, `:1117`) and `sstables/` (`sstable/writer.rs:238`, `lsm/mod.rs:3074`). The data directory, which holds `wal/`, `sstables/` and `MANIFEST`, and its parent are never synced. Under the pessimistic namespace model this loses every acknowledged write (F-D1b-2; 51 of 153 smoke rows).
* **Section 4 recovery classification, new shape (F-D1b-1, G14):** an all-zero 8-byte header followed by non-zero bytes (a lost first sector, later sectors persisted) is `len = 0 / crc = 0`, passes the CRC of an empty body, fails to decode, and is not at the tail by extent, so it is **corruption**: the product start refuses (`WAL_CORRUPT`); bare `inspect` discards the segment. 108 of 400 seeds in the P4 proof.
