# PHASE RUBIXDB — DISASTER RECOVERY (single node)

**Date:** 2026-10-03 · Driver: `scripts/ops/dr_measure.py` (real release binary, black-box HTTP + the real CLI; the driver keeps its own record of every acknowledged write). Raw results: `scratch/prod_ops/dr.json`. Hardware: i7-7700, SATA SSD (not NVMe), Balanced power plan.
No numeric promise is invented below: every figure is a measurement from this machine, with the workload and the conditions stated.

## 1. Failure classes and what recovers them
| Failure | Disk after the failure | Recovery path | Data loss |
|---|---|---|---|
| Process crash / kill (this machine's `TerminateProcess` test) | intact | restart: certified WAL recovery (replay, torn-tail truncation) | **0 acknowledged commits** (measured, §2). *Process kill only — see §6.* |
| Operating-system crash / power loss | intact, but device cache contents unknown | same restart path | **NOT TESTED — gate OPEN** (no power-cut capability; a process kill is not a power loss) |
| Disk / directory lost or corrupted beyond recovery | gone | restore the most recent verified backup into a **new** instance | everything committed after that backup's snapshot (§3) |
| Logical damage found by `rubixdb check` | present | index damage: `DROP INDEX` + `CREATE INDEX` (proven); anything else: restore a verified backup | commits after the backup, for the restore path |
| Point-in-time ("restore to 14:03") | – | **NOT IMPLEMENTED** (§5) | – |

## 2. RPO with the disk intact — acknowledged writes across repeated kills
Workload: 6 concurrent client threads against a table with a secondary index — 4 do single-row INSERTs, 2 do explicit transactions that insert **two** rows atomically (a "pair"); 4 s of load, then `TerminateProcess` on the server, restart, verify; 8 cycles on one database that keeps accumulating.
After every restart the driver checked: every acknowledged row present; every acknowledged pair present with **both** rows; no pair with exactly one row (atomicity); no row nobody sent; and `rubixdb check` exit code.

| Cycle | acked rows | acked pairs | lost acked rows | lost pairs | torn pairs | unknown-outcome rows that survived | restart→first query | engine recovery | WAL records applied | `check` |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 607 | 607 | 0 | 0 | 0 | 1 of 4 | 0.52 s | 31 ms | 1,220 | clean |
| 2 | 1,276 | 1,273 | 0 | 0 | 0 | 0 of 4 | 0.54 s | 52 ms | 2,555 | clean |
| 4 | 2,619 | 2,616 | 0 | 0 | 0 | 1 of 4 | 0.54 s | 77 ms | 5,241 | clean |
| 8 | 5,195 | 5,180 | 0 | 0 | 0 | 1 of 4 | 0.54 s | 146 ms | 10,381 | clean |
**Totals over 8 cycles: 23,386 acknowledged single/pair writes checked, 0 lost, 0 torn, 0 integrity findings.** (The 4 outcome-unknown requests per cycle — sent, connection died — are legitimately either present or absent; they are never counted as acknowledged and are folded into the model by observation so later cycles stay exact.)
Statement supported: *an acknowledged commit survives a process crash with the disk intact.* This is the certified WAL contract (ack = durable) exercised end to end through HTTP, SQL, the relational layer and the engine. It is **not** a power-loss statement.
(Recovery time rises with the un-checkpointed WAL tail — 1.2 k → 10.4 k records here because these runs never filled a memtable and so never flushed; at the 1M-row database below, a real tail replays in 113 ms.)

## 3. RPO with the disk lost — backup taken under write load
Same writers (4 single + 2 pair) running continuously; a backup created through the API **while they run** (t = 8 s), the server killed 8 s after the backup finished, the backup restored into a fresh instance:

| Measurement | Result |
|---|---|
| Backup duration under load | 45 ms (small database) — writers were never blocked |
| Rows acknowledged *before the backup started* | 1,319 — **0 missing** from the restored database |
| Rows acknowledged *after the backup finished* | 1,301 — **0 present** (exactly the loss to be expected) |
| Transaction pairs acknowledged before / after | 1,314 / 1,296 — 0 before-pairs missing, 0 after-pairs present, **0 torn pairs** |
| Restored row count | 1,319 (= acknowledged before the backup; the snapshot caught no in-flight commit) |
| `rubixdb check` on the restored instance | clean |
| Loss window (backup end → kill) | 8.0 s ≈ **163 acknowledged rows/s lost at this write rate** |
**RPO for the disk-lost path = the age of the newest verified backup** (here: the loss window × the commit rate). There is no continuous protection: WAL archiving is not implemented, so a lost disk loses everything after the last backup. This is stated, not hidden.

## 4. RTO — measured components at 1,000,000 rows (≈ 2 M stored entries with a secondary index; 103 MB on disk, 78.6 MB backup)
| Path | Component | Measured |
|---|---|---|
| **Crash restart** (kill during writes, disk intact) | process spawn → first authenticated query (engine open, manifest replay, WAL replay, catalog bootstrap lazy) | **0.52 s** |
| | engine recovery (`recovery.duration_ms`): 105 manifest edits, 2,064 WAL records visited, 1,933 applied, 3 SSTables, 3.0 MB WAL | **113 ms** |
| | spawn → first `COUNT(*)` over the 1 M-row table (full scan) | **2.0 s** |
| **Restore from backup** (disk lost) | verify backup (130+ MB read, all checksums, catalog) | 0.14 s |
| | load 2.0 M entries through the write path (atomic batches) | 37.4 s |
| | integrity check of the restored database (logical) | 33.6 s |
| | **restore total** | **72.3 s** |
| | start the restored instance → first query | 0.08 s |
| | first `COUNT(*)` over 1 M rows | 1.5 s |
| | **end-to-end RTO (restore + start + first full query)** | **73.9 s** |
Component notes: catalog load and index load are *lazy* in this engine — there is no index-load phase (indexes are ordinary keys in the same LSM); the catalog bootstraps on first SQL use. A second 1 M-row run (5 columns, 3.0 M entries, 121 MB backup) restored in 145.5 s: restore time scales with stored entries (~21–26 k entries/s) and half of it is the mandatory post-restore integrity check. RTO therefore grows linearly with database size; the 100 K-row database restores in 12.8 s. Backups themselves are fast (2.2 s for 121 MB).

## 5. WAL-based recovery / PITR — NOT IMPLEMENTED (reasoned from source)
Could the existing WAL support "backup point + replay"? **Not without a change to certified code.** The engine deletes WAL segments after each checkpoint (`Wal::purge_before`, driven by the flush/manifest checkpoint); a replayable history would need either retained or archived segments (an engine/WAL retention change) and a replay entry point that stops at a chosen sequence (none exists; `replay_streaming` replays everything to the end). Both are inside the protected boundary (WAL, manifest, engine open path), so this phase does not touch them, and no PITR feature is offered or simulated. The backup records `snapshot_seq` (an engine sequence number) so a future design has a stable anchor.
Requirement list for a future PITR phase: (1) WAL segment archival hook before purge with a retention/size policy; (2) a replay-to-sequence primitive; (3) a safe interplay with compaction's snapshot floor; (4) tests that restore at arbitrary sequences against a reference model.

## 6. What this document does not claim
* Power-loss durability — NOT TESTED. Process-kill is not equivalent: acknowledged-before-fsync bugs are invisible to it (the page cache survives a kill).
* Zero data loss in general — not claimed. Claimed and measured: zero loss of acknowledged commits across process crashes with the disk intact; loss bounded by the backup age if the disk is lost.
* Scale beyond what was run: 1 M rows, one SATA SSD, single instance.
* The recovery numbers are for this machine and its Balanced power plan.

## 7. Operator runbook (verified commands)
1. Crash/kill: just start the instance (`rubixdb gui` / any `rubixdb` command). Run `rubixdb check` afterwards.
2. Disk lost: `rubixdb restore --from <verified backup file> --instance NEW` → `rubixdb gui --instance NEW` → `rubixdb check --instance NEW`. Restore never overwrites an existing database; credentials are not part of a backup (the new instance generates its own).
3. Take backups on a schedule yourself (`rubixdb backup create`); `rubixdb backup verify NAME` after copying them off the machine; keep at least one copy on another disk — a backup on the same disk is not disaster recovery.
