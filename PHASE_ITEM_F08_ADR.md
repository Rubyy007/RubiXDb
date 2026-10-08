# ADR-SST-01 — A damaged SSTable must be found before any file is changed, and reported while it is serving

**Status: PROPOSED — awaiting maintainer approval. Do not apply.** Nothing in this ADR has been implemented; no engine, protected-path, test, `Cargo.toml` or `Cargo.lock` change was made in the mission that wrote it. Evidence: `PHASE_ITEM_F08_DISCOVERY.md` (2026-10-08, tree `03295e0`). Related: ADR-ENG-OPS-001 (`PHASE_RUBIXDB_ENGINE_PERFORMANCE_ADR.md:107-115`), ADR-WAL-01 (`PHASE_ITEM_F07_ADR.md`, ACCEPTED and implemented).

## Decision

In the product layer only: (1) extend the startup-only guard with a read-only SSTable preflight that runs before the F-07 attestation is consumed and before the engine opens — every Manifest-live table must exist, match the Manifest's recorded size and sequence range, and pass the same footer / bloom / index validation the engine runs, otherwise the start is refused (exit 1, directory unmodified, no override); and (2) after the server is serving, verify every data block of the tables that were live at start in a throttled, cancellable background pass whose state and findings are reported through additive fields of `/readyz` and `/v1/status`, a security-log event and stderr, without ever changing `ready`.

## Reason (measured, `PHASE_ITEM_F08_DISCOVERY.md`)

* **The engine already refuses everything it validates at open**: footer magic / CRC / offsets, bloom, index, format version, a truncated file, a missing live file, a junk foreign table — 11 of 11 scenarios exit 1 with a path-naming message and the damaged file untouched (section 4.2). The gap is not "damage at open reports ready"; it is what the engine does *not* check.
* **Data-block damage opens `ready`** (scenarios a, k): `/readyz` `ready: true` / `Healthy`; the 18 rows in the flipped block fail every lookup (HTTP 500 `STORAGE_ERROR` on SQL, 500 `CORRUPTION` on KV), the other 1,082 sampled lookups are correct, `COUNT(*)` is 500, writes succeed, and no endpoint names the table. After Phase 5 those rows exist nowhere else in the directory (`lsm/mod.rs:3123`, the fixture's WAL segment 1 is purged), so this is data loss; `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.5 and `src/sstable/mod.rs:6-16` still say it is only an availability concern.
* **The Manifest's record of a table is never enforced** (scenario h): a valid but wrong file in place of table 1 opens `ready` and every statement returns 404 `NOT_FOUND` because the catalog rows are gone; size 4,030,612 vs 4,030,165 and sequence range 1-88 vs 89-175 were recorded and never compared (`lsm/mod.rs:2492-2512`; `check.rs:914` compares them).
* **A refused start already changed a file** (all refusals b1-j): the F-07 guard consumed `WAL_CLEAN_STOP` before the engine's own SSTable checks (`wal_tail.rs:879`), so the next start is unattested.
* **Damage after open is invisible while running** for footer / bloom / index (m2-m4, m6) and is found only where a read lands for data blocks (m1, m5); a compaction over a damaged block fails every five seconds, leaks a 2 MB `.sst.tmp` each time and reports nothing (m7).
* **Cost**: engine-style open of a table (footer + bloom + index) 0.22 / 1.72 / 6.01 ms for 4 / 64 / 256 MiB; a full data-block scan 8.8 / 97 / 389 ms (470-680 MiB/s, one core, warm cache; `raw\cost_probe.txt`) — about 1.5 s per GiB, against the embedded host's **30 s** readiness bound (`cli/src/host.rs`, `recv_timeout(30)`).

## Alternatives considered (P1-P4; failure mode of each)

| | Rule | Failure mode it leaves open or creates |
|---|---|---|
| **P1** | refuse at open if any table is invalid. As worded (footer / CRC / index) it is **the status quo**. Extended to every data block: a synchronous full read per start | blocks startup for large databases by the 30 s bound (about 1.5 s per GiB warm, more cold); the intact rows cannot be opened for export; still blind to damage after the scan and, unless the Manifest is also compared, to a swapped file |
| **P2** | lazy: errors only for queries that touch the damage; health stays ready | the status quo for data blocks: silent until touched, grows unnoticed, backups of a damaged directory taken unknowingly |
| **P3** | open validates metadata and serves the rest with the damaged table reported *degraded*; queries on it fail typed | for metadata-invalid tables it would **weaken today's refusal** and needs the engine to open around a bad table (protected paths); dropping a table from service resurrects older versions and deleted keys; `ready` is a constant `true` by decision D5 and `StorageState` has no degraded value |
| **P4 (this ADR)** | keep the refusal; make it earlier, directory-preserving and Manifest-authoritative; find data-block damage with a throttled background pass and put the result on the existing health surface | detection is delayed by the pass; damage after a pass is found only by the next pass, a read, or `rubixdb check`; the instance keeps serving and accepting writes with known-lost rows; the compaction retry leak is not fixed (engine, separate finding) |

## Design

**A. Preflight** (new module under `src/ops/`, called from `startup_guard_with_tail_policy` after the unchanged `WAL_CORRUPT` preflight and **before** `apply_tail_policy`, so every refusal is decided before the first mutation — which also stops the attestation being consumed by a start that will be refused). Read-only. `manifest::replay_readonly(dir)` gives the live set; then:

1. every live id has `sstables/<id:020>.sst`, else `SSTABLE_MISSING` (same text as the engine's);
2. its length equals the recorded `file_size` (a recorded `0` means "unknown" — `lsm/mod.rs:2733` records `unwrap_or(0)` if the metadata call failed — and is not compared), else `SSTABLE_MISMATCH`;
3. `SsTable::open(path, id)` succeeds (the engine's own function, so the checks and messages cannot drift), else `SSTABLE_CORRUPT` carrying the engine's text;
4. the footer's `min_seq` / `max_seq` equal the recorded ones, else `SSTABLE_MISMATCH`;
5. every other `*.sst` that is neither live nor in `ever_added` (the engine will adopt it) must pass `SsTable::open`, else `SSTABLE_CORRUPT`; `*.sst.tmp` and removed-but-undeleted tables are left to the engine; an unreadable manifest refuses with its own message.

Refusal text (proposed): `engine open refused: SSTABLE_CORRUPT: sstable <path>: <detail>; refusing to open: a table the Manifest lists as live is damaged and the engine would not serve a complete database. Run `rubixdb check` and restore a verified backup into a new instance. The directory has not been modified.` (`SSTABLE_MISSING` and `SSTABLE_MISMATCH` analogous). Exit 1. The first failing table in directory order is named, as today.

**B. Background verification.** Started by the host after readiness, like the index-recovery thread (`cli/src/host.rs`), for the ids and paths the preflight validated. One low-priority thread; per table: its own `SsTable::open` handle, `range_scan_raw(Unbounded, Unbounded)` over every block (the same scan `rubixdb check` performs, `check.rs:925-956`); a table that disappears (compaction retired it) is skipped as retired; the first `Err` for a table ends that table's scan and is recorded as `{table id, relative path, records read before the failure, records in the footer}` (the engine error carries no block offset and keys are not logged). Throttled to a bytes-per-second budget; stops at the next block on shutdown and is joined by the graceful shutdown before the engine stops. Nothing is persisted.

**C. Reporting** (api crate and host; additive only; `ready` and every existing field unchanged):

* `/readyz`: `sstable_verification` = `disabled` | `running` | `complete` | `damaged` (the same shape as `index_recovery`);
* `/v1/status`: `sstable_integrity` = `{ state, tables_total, tables_verified, bytes_verified, damaged: [{ id, path, records_before_failure, records_total }] }`;
* security log: `sstable.damaged` with `object` = `table=<id> records_before=<n>`; stderr: one line per damaged table naming the file and the operator action. Never row data, keys or credentials.

## Correctness impact

* **Engine contract for opens and reads: unchanged.** `LsmEngine::open`, `SsTable::open`, `decode_block` and every error variant stay as they are; the preflight calls the same public functions read-only, so it can only refuse earlier, never accept what the engine refuses.
* **What a consumer observes.** (1) A start that the engine would have refused now also leaves `WAL_CLEAN_STOP` and every other file untouched. (2) A start with a table whose size or sequence range disagrees with the Manifest — previously accepted and serving wrong data — is now refused; `rubixdb check` already reported it as an error. (3) A running instance with a damaged data block now says so on `/readyz`, `/v1/status`, the security log and stderr after the pass reaches it; its queries fail exactly as before. (4) Nothing else changes: reads, writes, compaction and `ready` behave as today.
* **Not changed and stated:** the damaged rows remain unrecoverable from the directory; writes and compaction continue; the SQL route still returns the generic `STORAGE_ERROR`.

## Performance impact (predicted only; to be measured in the implementation mission)

* **Hot append path and read path:** none — no code runs there; the pass reads through its own handles.
* **Open path:** the preflight reads each table's footer, bloom and index once more (measured 0.2-6 ms per table, 25 ms per GiB of tables) plus a `stat` and a manifest replay: tens of milliseconds for a database of many GiB, doubled by the engine's own open.
* **Background pass:** unthrottled it is one core at 470-680 MiB/s (warm); at the proposed default of 64 MiB/s about 10-14 % of one core warm and 64 MiB/s of disk reads cold, i.e. about 160 s for 10 GiB, 27 min for 100 GiB. The effect on foreground latency under load is **not measured** and is the first thing to measure; the default is a proposal, not a finding.

## Failure semantics

* **Crash during validation:** the preflight is read-only and the pass keeps no state; a crash leaves nothing to repair, the next start repeats both.
* **Corruption appearing mid-run:** a data block damaged before the pass reaches it is reported by the pass or by the first read, whichever is earlier; damaged after the pass passed it, only by the next start's pass, a read, or `rubixdb check`; footer / bloom / index damage after open is not detected while running (held in memory) and is refused at the next start by the preflight. A deleted or replaced file under a running instance is likewise found at the next start. This is a stated limit, not a claim of continuous protection.
* **Corrupt Manifest entry:** a manifest the read-only replay cannot parse refuses the start with the replay's message; a torn trailing frame is the engine's existing expected case and does not refuse. A Manifest entry whose recorded size / sequence range disagrees with a valid file is `SSTABLE_MISMATCH` (above).
* **Operator override: none.** See Configuration.
* **Damaged table discovered after start:** reporting only. No automatic action: no write blocking, no compaction control, no self-repair. The documented operator action is `rubixdb check`, then restore a verified backup into a new instance. A background compaction that reads the damaged block continues to fail and leak (separate finding).

## Configuration surface

| Item | Value |
|---|---|
| `RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC` | integer, digits only (the strict `RUBIXDB_LOCAL_*` parser, `cli/src/startup_env.rs`); unset or empty = **64** (proposed); `0` = disabled (`sstable_verification: disabled`); upper bound 1024 (above the measured 677 MiB/s one-core rate there is nothing to gain); anything else stops startup naming the variable, before anything is created |
| New response fields | `/readyz.sstable_verification`, `/v1/status.sstable_integrity` (additive; existing fields and `ready` unchanged) |
| New refusal codes | `SSTABLE_CORRUPT`, `SSTABLE_MISSING`, `SSTABLE_MISMATCH` |
| New dependency / `unsafe` | none |
| **Override mirroring `RUBIXDB_ALLOW_TRUNCATE_CORRUPT_WAL`?** | **Not appropriate.** F-07's override accepts a loss that has already happened and is bounded: a missing suffix of history, leaving the database prefix-consistent. An override here would open a database with a hole in the middle of its history: the engine cannot do that without an engine change (a table excluded from the live set), and the result would not be a smaller database but a different one — older versions of keys reappear and deleted keys come back (`P3`). The recovery path is `rubixdb restore` into a new instance and read-only `rubixdb check`; salvage tooling that exports the readable remainder is a separate design. |

## Migration

* **No on-disk change:** no SSTable, Manifest or WAL format change; every existing table stays readable.
* **A directory that opened before may refuse now only if the Manifest and a file disagree** (size or sequence range). Healthy writers cannot produce that: flush records the actual size (`lsm/mod.rs:3047`), compaction records `metadata().len()` (`:2733`), adoption records the table's own range (`:2503-2507`). To be verified in the implementation by running the check over the soak and crash-cycle output directories (`examples/`) before enabling the refusal.
* **Existing tests:** none identified that must change. `src/ops/physical_tests.rs::a_missing_live_sstable_is_detected_and_open_fails_closed_without_modifying` (:166) and its siblings call the engine and `check_physical` directly, not the startup guard; the engine-level suites (`tests/wal_tests.rs`, `tests/pathological_recovery_matrix.rs`, `tests/crash_consistency.rs`, `tests/group_commit`, `src/sstable/tests.rs`, the crash-cycle examples) do not pass through the guard, and **none of the four crash-consistency suites is expected to fail**. The F-07 real-binary test `a_cleanly_stopped_database_with_a_damaged_last_wal_frame_is_refused_with_the_exact_sequence_gap` and the rest of `cli/tests/f07_tail_damage_integration.rs` use undamaged SSTable directories and must still pass. A prediction from reading and searching, to be confirmed by running the suites.
* **Rollback:** disable the pass with `RUBIXDB_LOCAL_SSTABLE_VERIFY_MIB_PER_SEC=0`; the preflight has no switch (it only refuses what the engine, or `rubixdb check`, already treats as damage).

## Relationship to ADR-ENG-OPS-001 and ADR-WAL-01

* **ADR-ENG-OPS-001 (the engine discards corrupted WAL segments):** *not* the same problem in a different layer. There the engine ignored a signal and a product guard compensated. For SSTables the engine already returns a hard `Err` for everything it validates and does not skip or fall through (`reconcile_sstables_with_manifest`, `reader.rs`), so no engine ADR of that kind is needed; what is distinct here is coverage (data blocks, the Manifest's record), timing (before any mutation, and after open) and reporting.
* **ADR-WAL-01 (F-07):** the same product-layer gate and the same startup-only guard, but a different required decision (Discovery section 8): WAL tail damage is ambiguous and needed an extra fact and evidence preservation; SSTable damage is never ambiguous, loses the middle of history, and cannot be overridden. The two interact in one place — the attestation is consumed before an SSTable refusal — which this ADR fixes by ordering the preflight first.

## Out of scope

Any change under `src/sstable/`, `src/lsm/`, `src/manifest/`, `src/wal/`, `src/compaction/`, `src/error.rs`; the compaction retry loop and its leaked `.sst.tmp` files (separate finding in `OPEN_ITEMS.md`); a degraded-but-serving engine, per-table quarantine or read-only mode; salvage / export of the readable remainder; automatic repair; mapping the typed corruption onto the SQL route's `STORAGE_ERROR`; the `rubixdb check --instance` defect and the `CHECK_INCOMPLETE` naming (separate findings); a periodic re-verification; correcting `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.5 and `src/sstable/mod.rs:6-16` (a documentation correction the maintainer may order); changing decision D5 (`ready` stays a constant `true`); F-11, F-18 and everything else.
