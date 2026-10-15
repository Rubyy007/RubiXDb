# rubiXDb — DURABILITY CONTRACT (D0 draft, version 0.1)

Written 2026-10-13, Slice 0 (D0). Documentation only. Every later oracle, fault-injection result, backup claim and PITR guarantee in this mission is judged against **this** file.
It is derived from `docs/PERSISTENCE_MAP.md` (read it first; evidence tags `[R] [M] [D] [U]` mean the same here).

How to read this contract. Each clause has up to three parts:

* **CONTRACT** — what rubiXDb promises, stated so a test can fail it.
* **TODAY** — what the evidence in the repository supports right now (tagged). If TODAY is weaker than CONTRACT, the clause is a **GAP** and is listed in section 11.
* **CLAIM RULE** — what evidence is required before the promise may be stated outside this document.

Nothing here is a readiness statement. `CLAUDE.md` still forbids "PRODUCTION READY" until the whole certification matrix is PASS, and `docs/PROJECT_STATE.md` still lists power loss and disk-full as untested. Nothing in this file changes that.

---

## 1. Vocabulary

| Term | Definition |
|---|---|
| **Commit unit** | One `LsmEngine::put`, `delete` or `write_batch`. A `write_batch` is encoded as **one** WAL group frame; the relational layer puts a row with its index entries, a DDL statement's catalog rows, and a transaction's whole write-set into one `write_batch` `[D]`. A commit unit is atomic: all of its operations or none. |
| **Sequence number (`seq`)** | `u64`, gap-free, assigned in WAL append order, WAL-wide (not per segment). One commit unit = one `seq` `[R]` `wal/mod.rs:971-993`. |
| **LSN** (this mission's term) | The `seq` of a commit unit. The only internal recovery coordinate this contract uses. The full WAL coordinate is `(segment_id, offset, seq)` (`WalPosition`, `wal/mod.rs:182`). The WAL records **no wall-clock time** `[R]` (frame body is `seq | op | …`, `wal/format.rs:152-188`), so a timestamp is not a recovery coordinate and cannot be derived from existing data. |
| **Acknowledged (ACK)** | The engine call returned `Ok(seq)` to its caller. |
| **Durable** | The commit unit's frame is on stable storage in a form recovery will find, under the storage assumptions of section 6. |
| **Applied** | Visible to reads (inserted into the active MemTable after the durability barrier). |
| **Outcome-unknown** | The caller received an error, a timeout, or no return at all (crash), and the frame may or may not have been written. |
| **Checkpoint** | `SET_CHECKPOINT{flushed_through_seq}` in the Manifest: every commit unit with `seq <=` it is contained in a live SSTable or is a checkpoint marker (C-CKPT-1, in force 2026-10-13: the checkpoint is the contiguous-applied watermark captured at freeze, not the table's own `max_seq`). |
| **Recovery boundary** | The highest `seq` recovery reconstructs. Recovery reconstructs *a prefix of the commit sequence* (invariant R3). |

---

## 2. Acknowledgement semantics

**C-ACK-1 (CONTRACT).** `put` / `delete` / `write_batch` returns `Ok` only after the commit unit's WAL frame has been written **and** a `sync_all` on the active WAL segment, issued after that write, has returned `Ok`. An acknowledged commit unit is durable under assumptions A1-A8 (section 6) and survives every crash class in section 7 for which this contract grants a guarantee.

**TODAY.** `[R]` The only barrier is `GroupCommitter::run_as_leader` → `file.sync_all()` (`wal/group_commit.rs:1255-1337, 1524-1536`); the watermark `durable_through` is published with `fetch_max` strictly after `Ok`, and `completion.wait()` returns only when `durable_through >= seq`; the MemTable is updated after that (`lsm/mod.rs:1892-1960`). No code path returns `Ok` earlier (`sync_data`, an async mode, a "no-fsync" mode do not exist — grep). Not mutation-tested in D0 `[U]`.
`[D]` Process-kill evidence: 23,386 acknowledged writes, 8 kill cycles, 0 lost, 0 torn (`PHASE_RUBIXDB_DISASTER_RECOVERY.md` §2); the WAL ack oracle `examples/wal_ack_oracle.rs`.

**CLAIM RULE.** "An acknowledged commit survives a *process* crash with the disk intact" may be claimed (class B evidence exists `[D]`). "…survives a *power loss*" requires class C evidence for **every** fault point (Slice 1c) *and* either class-D/real-hardware evidence or an explicit statement that A1 is an assumption. Today: **not claimable**.

**C-ACK-2 (CONTRACT).** A call that returns `Ok` after the write was durable but the caller never received it (client disconnect) is still durable. A call that returns `Err` may still have written its frame (outcome-unknown). The engine does not promise rollback of an errored call.

**TODAY.** `[R]` Cases that return `Err` *after* the commit unit is durable and applied: `CapacityExceeded` from `freeze_locked` (`lsm/mod.rs:2027-2065`; accepted contract, "freeze backpressure signal only"). Cases that return `Err` with the frame possibly in the file: group-commit leader fsync failure (the frame is in the page cache; the committer is poisoned, `group_commit.rs:1549-1556`); follower timeout / await-retry-budget exhaustion (`batch_coordinator.rs:827-…`).

**C-ACK-3 (CONTRACT).** A commit unit that was **not** acknowledged may, after recovery, be wholly present or wholly absent. It is never partially present and never present with altered content.

---

## 3. Sync modes and barriers

| Mode / primitive | Exists? | Barrier | Notes |
|---|---|---|---|
| `SyncMode::Immediate` (library default `WalConfig::default()`) | yes, `FileWal::append_sync` | `sync_all` per record | **Cannot be used with `LsmEngine::open`**: `GroupCommitter::new` rejects it `[M]` |
| `SyncMode::GroupCommit{max_wait, max_batch_bytes}` | yes — **the product mode**: 5 ms / 256 KiB (`ops/open.rs:14-22`) | one `sync_all` per leader batch; followers wait on the watermark | acknowledgement is per commit unit, durability is per batch |
| async / delayed / "no sync" mode | **no** | — | none may be added without a contract change |
| `fdatasync` / `sync_data` | **no** (`sync_all` everywhere: Windows `FlushFileBuffers`, Unix `fsync` — Windows mapping `[U]`) | | |
| directory sync | **partial**: `fsync_dir` real on Unix, **no-op on Windows** (`wal/file_io.rs:260-296`) | after WAL segment create/unlink, SSTable publish, compaction-oversize removal; **since 2026-10-14 (ADR-DATA-DIR-DURABILITY-01): also after creation of the data directory (its parent), `wal/`, `sstables/`, `MANIFEST` and the `DATA_FORMAT` rename (their parent, the data directory)** | **not** applied after: compaction input unlink, backup publish, restore promote, WAL quarantine rename (PERSISTENCE_MAP 3.x; G10). On Windows none of these syncs does anything (see C-SYNC-2) |
| group-commit batching window | yes: an adaptive window ≤ 5 ms | — | affects latency only; never changes which bytes an `fsync` covers `[D]` (`group_commit.rs:113-130`) |

**C-SYNC-1 (CONTRACT).** No performance setting may weaken the barrier in C-ACK-1. A throughput change that moves, merges or drops a barrier is a durability change and needs this contract amended first.

**C-SYNC-2 (CONTRACT).** Every directory-entry change that the contract relies on for durability (a new WAL segment, a newly published SSTable, a removed obsolete file when its resurrection would matter) must be followed by a directory sync **on platforms that provide one**, and the platform's behaviour where it does not must be an *explicit, tested assumption* (A5), not silence.

**TODAY.** The Windows gap is documented in code (`file_io.rs:260-277`) but there is no test of what NTFS does (A5/W1 `[U]`). The Unix path is `[R]`-only, never executed in D0.
**UPDATE 2026-10-14 (ADR-DATA-DIR-DURABILITY-01, POSIX fix IMPLEMENTED):** the engine now syncs the parent directory after it creates the data directory, `wal/`, `sstables/`, `MANIFEST` and `DATA_FORMAT`. Evidence is **simulated** (the simulator with POSIX directory-sync semantics, `tests/dir_durability.rs`); it has **not** been run on a real Unix kernel `[U]`.
**WINDOWS LIMITATION (accepted by the maintainer 2026-10-14, verbatim):** On Windows, sync_dir is a no-op. Protection of directory entries rests on assumption W1 (NTFS metadata-log ordering), which is NOT established by any test in this repository. Power-loss durability of newly created wal/, sstables/ and MANIFEST entries on Windows is therefore NOT established. This is a Windows-specific exposure, not a documented guarantee.

---

## 4. Failure behaviour

| Event | TODAY (`[R]` unless tagged) | CONTRACT |
|---|---|---|
| **WAL write returns an error / short write** | `SegmentIo::append` truncates the file back to the pre-append length and `sync_all`s; if the rollback fails the segment is **poisoned** and refuses all later I/O (`wal/file_io.rs:91-117`). `[D]` unit-tested with `FaultInjectingIo`. | No acknowledged commit unit may be lost or altered; the failed unit is outcome-unknown; no later record may be written after unknown bytes. |
| **WAL `fsync` fails (EIO, device error)** | Committer poisoned **permanently**, `durable_through` never advances, all waiters get `Err`, the fsync is **never retried** (`group_commit.rs:1333-1337, 1549-1556`). The process must be restarted; recovery re-reads from disk. | After a failed fsync the engine must never report durability for bytes written before it, and must not continue to accept writes on that WAL. (matches) |
| **ENOSPC on WAL append** | returned to the caller as `EngineError::Io`; `is_enospc` = `ErrorKind::StorageFull` or raw OS error 112/28 (`error.rs:94-104`). Rollback truncates. `[D]` | The write is not acknowledged; nothing acknowledged is lost. |
| **ENOSPC / EIO in flush** | `?`-propagated; retried `max_flush_retries=3` with 50 ms×n back-off, then every 2 s (non-ENOSPC) or `storage_pressure_retry_interval` (ENOSPC); `TmpFileGuard` removes the partial `.sst.tmp`; state machine `Healthy → StoragePressure → StorageFull`; once the immutable backlog hits `max_immutable_memtables=4`, writers get `CapacityExceeded`, and in `StorageFull` `StorageExhausted` **before** any WAL append (`lsm/mod.rs:3340-3566, 2027-2065, 1841`). `[D]` injected-error tests only. | Flush failure must never advance the checkpoint or purge the WAL (steps 7-9 of flush are structurally unreachable unless steps 1-4 succeeded). (matches, `[D]`) |
| **ENOSPC in compaction** | classified; permanent-class failures block the worker after the retry budget (`ADR-COMPACTION-LEAK-01`) `[D]` | Compaction failure must leave the previous live set intact. |
| **EIO / short write on `MANIFEST` append** | **no rollback** (`manifest/mod.rs:114-121`); a retry appends after the prefix and the next open fails `Corruption` `[M]` (F-D0-3). | A failed MANIFEST write must not make the directory unopenable. **GAP.** |
| **Read-only remount / EROFS / permission error** | No dedicated handling (`grep ReadOnlyFilesystem|EROFS` → nothing); surfaces as generic `Io`. | Writes must fail without acknowledgement; reads of durable state must keep working; restart on a read-only mount must either open read-only or refuse with an actionable error and must not truncate or adopt anything. `[U]` not tested. |
| **Real disk-full** | `[D]` `PROJECT_STATE.md`/`OPEN_ITEMS.md`: "real disk-full not tested" (injected ENOSPC and a `fail_write_after` backup seam only). | See class D (section 7). |
| **Checksum failure, bad magic, unsupported version, missing live object** | Fail closed with `EngineError::Corruption`/`Unsupported` for MANIFEST (non-trailing), SSTable footer/index/bloom, missing live table; WAL non-tail corruption returns `corrupted_segments` (**ignored by the bare engine**, honoured by the product guard, PERSISTENCE_MAP 4.1); trailing-frame damage in WAL and MANIFEST is classified "torn" and truncated. | R9 (section 8): never silently ignored. Exceptions must be exactly the *torn tail of the active log*, truncated **and reported**. |

---

## 5. Torn writes and sector atomicity

**C-TORN-1 (CONTRACT).** The WAL frame is the unit of integrity: `len | crc32c | body`. Recovery must (a) accept a frame only if its CRC verifies and it decodes; (b) treat *any* damage confined to the bytes after the last durable frame — however long, whatever its content (short, partial, zero-filled, stale garbage, 0xFF) — as a **torn tail**: truncated, reported, and never allowed to condemn the durable prefix; (c) treat damage *before* a later valid frame as corruption.

**TODAY.** `[R]` The classifier is positional: a bad frame is torn iff its claimed extent ends at end-of-file (`wal/recovery.rs:63-203`). `[M]` Zero-filled tails of 4 and 8 bytes are torn (ok); **16 bytes and longer make the whole segment — including its 3 acknowledged records — "corrupted"** (F-D0-1). The MANIFEST classifier errors on an 8-byte-or-longer zero tail (F-D0-2).
**UPDATE 2026-10-13 (F-D0-1/2 fixed, ADR-TORN-TAIL-ZERO-01):** an all-zero region from a frame boundary to end-of-file, of any length, is now a torn tail in the WAL and in the MANIFEST. Clause (b) is **narrowed** accordingly — stale-garbage tails are no longer promised to be torn (see §12). **D1b found a shape the narrowed rule does not cover (F-D1b-1, gap G14):** a lost first sector of the last unsynced frame with later sectors persisted. **UPDATE 2026-10-14 (ADR-TORN-TAIL-LOST-SECTOR-01, IMPLEMENTED): that shape is now a torn tail (clause C-TORN-1(b4), §12.1); P4 "rejected as corrupt" fell from 108 of 400 seeds to 0. A different shape, produced by the model's sub-sector tear, still reaches "corrupt": gap G15.**
**GAP (as originally found)** against clause (b). Resolving it is a Slice 1b/1c decision and, if it requires changing `src/wal/recovery.rs` or `src/manifest/recovery.rs`, needs an ADR (`CLAUDE.md`: certified engine boundary) *and* the mission's explicit authorisation (which names those paths).

**C-TORN-2 (CONTRACT).** No frame is padded or aligned to sector boundaries; a frame may straddle a sector boundary and so may tear. The contract therefore assumes only A2 (below), not that any write is atomic.

---

## 6. Storage and platform assumptions (named, so they can be tested or disclaimed)

These become the executable model of Slice 1b (`docs/PERSISTENCE_MODEL.md`). An assumption that cannot be tested on the available platform stays an *assumption* and every claim that depends on it carries its ID.

| ID | Assumption | Status |
|---|---|---|
| **A1** | After `sync_all` on file F returns `Ok`, every byte previously written to F (through any handle) will be read back after a power loss, unless a later `sync_all` on F returned an error. A device that lies about flushing violates A1 and is outside the contract. | `[U]` on this SATA SSD; cannot be tested without hardware power cuts |
| **A2** | A write of one physical sector is atomic; a multi-sector write may be persisted partially, with any subset/order of its sectors; unsynced writes to different files/regions may persist in any order. Sector size: unknown to the engine (512 B or 4 KiB). | `[U]`; simulated in Slice 1b |
| **A3** | A file's *length* may become durable before the data it covers: a crash can leave a zero-filled or stale-content tail. Recovery must classify such tails as torn (C-TORN-1). | contract requirement; GAP today |
| **A4** | `rename` is atomic with respect to crashes at the namespace level (old name or new name, not neither/both-broken). On POSIX its durability additionally requires a directory fsync; on NTFS durability of the rename is not established by `std::fs::rename` returning. | `[U]` |
| **A5** | A directory entry change (create, rename-in, unlink) is durable only after a directory sync where the platform has one. **On Windows the engine performs none.** What NTFS guarantees instead (W1: its metadata log preserves the order of metadata operations, so a later flushed file implies earlier renames) is `[U]`. **On Windows, sync_dir is a no-op. Protection of directory entries rests on assumption W1 (NTFS metadata-log ordering), which is NOT established by any test in this repository. Power-loss durability of newly created wal/, sstables/ and MANIFEST entries on Windows is therefore NOT established. This is a Windows-specific exposure, not a documented guarantee.** | `[U]` — biggest single unknown for Windows power-loss claims |
| **A6** | After an `fsync` error the dirty pages' state is unknown; the engine never retries an fsync and never reports earlier writes as durable. | implemented (`group_commit.rs:1549`); `[D]` tested with an injected hook |
| **A7** | Synced data does not change silently except by bit rot, which checksums detect and the engine reports (it does not repair). | |
| **A8** | Exactly one engine process owns the data directory (OS advisory lock on `wal/LOCK` plus the instance lock); nothing else modifies the files. | `[R]` |

---

## 7. Crash classes (never conflated; every result is labelled)

| Class | Name | What it models | Evidence that counts | What it can and cannot support |
|---|---|---|---|---|
| **A** | Returned I/O error | a syscall in the persistence layer returns `Err` and the process lives | fault injection at named points with the process alive; assertion on the error path and on later recovery | Supports: error-path correctness (rollback, poisoning, no false ack). Not a crash. |
| **B** | Process termination (`kill -9` / `TerminateProcess` / `abort`) | the process disappears; the OS and its page cache survive; every successfully-returned `write()` is visible to the next process, synced or not | external kill at named points | Supports: recovery logic, ordering of *logical* steps, idempotence. **Does not** support any statement about power loss: unsynced writes survive a kill, so a missing fsync is invisible to it. (`tests/crash_consistency.rs` says the same.) |
| **C** | Simulated power loss | all state not covered by a successful sync is discarded or damaged according to the persistence model (A1-A5): unsynced bytes lost, last sector torn, optional reordering, dir entries without dir sync lost | the Slice 1b interception layer with its four proof tests — **implemented in D1b** (`crate::persist` + `SimFs`, `docs/PERSISTENCE_MODEL.md`; proofs `tests/sim_proof.rs`: PASS). Every class C row carries the label "simulated, model-based, assumptions A1–A5/W1; real power loss NOT TESTED" | Supports: "if the storage obeys the model, recovery is correct". Does not support: statements about real devices. |
| **D** | Filesystem / storage error preventing recovery | recovery itself hits EIO, ENOSPC, EROFS, permission, missing/corrupt/unsupported object | fault injection during recovery and corrupted fixtures | Supports: "recovery fails closed with an actionable error and does not damage the directory". |
| (real power loss) | Tier 3/4: `dm-flakey`, hardware cut | not available in this environment | — | Until it exists, class C is the ceiling and every power-loss statement must say "simulated, model-based". |

**C-CLASS-1 (CONTRACT).** A result is recorded with its class. A class B result never appears in a "power loss" column, never discharges a class C obligation, and a document that says "crash-safe" without a class is wrong.

---

## 8. Recovery invariants (the oracle's checklist)

For a directory produced by any sequence of operations followed by a crash of class A, B or C at any named fault point, **recovery (the start of the next process) must satisfy**, or fail closed with a reported error (class D):

| ID | Invariant | TODAY |
|---|---|---|
| **R1** | **Acknowledged commits are recovered.** Every commit unit acknowledged before the crash is reflected in the recovered state (unless a later recovered commit supersedes it). | class B single writer `[M]` D1a: held at every reachable point (docs/CRASH_CLASS_MATRIX.md §2.1); ~~class B / graceful stop with CONCURRENT writers: VIOLATED `[M]` (F-D1a-1, G13)~~ **fixed 2026-10-13 (C-CKPT-1): D1a concurrent column 31/31, 0 lost in 300 hunt runs**; class C `[U]` until D1c (D1b smoke: docs/PERSISTENCE_MODEL.md §7) |
| **R2** | **Atomicity.** For every commit unit, recovered state contains all of its operations or none. | by construction (one CRC-framed group frame) `[R]`; class C `[U]` |
| **R3** | **Prefix.** There is a `K` such that exactly the commit units with `seq <= K` are recovered (no gaps, no later-without-earlier). `K >=` the highest acknowledged `seq`. | by construction `[R]` (sequential append, scan stops at first invalid frame); not asserted by any test in D0 |
| **R4** | **No phantom or malformed data.** Nothing is recovered that was never submitted; no recovered value differs from what was submitted. Unacknowledged-but-complete frames may appear (C-ACK-3). | |
| **R5** | **No double application.** Replay of a frame already contained in an SSTable (`seq <= checkpoint`, or a duplicate adoption) has no visible effect; no commit is applied twice under different seqs. | skip-by-checkpoint `lsm/mod.rs:1563`; **breaks if seqs restart below the checkpoint** (F-D0-5) |
| **R6** | **Monotonic sequence.** After recovery the next assigned `seq` is `> max(checkpoint, max seq in any live SSTable, max seq in the WAL)`. | ~~VIOLATED when the WAL content is missing or ends below the checkpoint~~ **fixed 2026-10-13 (C-SEQ-1: floor at checkpoint + 1)**; reachability of the premise `[U]` |
| **R7** | **Agreement.** Manifest live set ↔ files on disk ↔ WAL ↔ catalog agree: every live table exists and validates; no table is both live and removed; every table file is live, an adoptable valid orphan, or a deletable removed/temp file; checkpoint ≤ highest durable seq; catalog rows decode and obey the schema rules of `ops::check`. | open-time checks `[R]` (PERSISTENCE_MAP 4.3); `rubixdb check` for the logical level |
| **R8** | **No partial publication is valid.** A `.tmp` file, a segment without a verifiable header, an SSTable without a verifying footer/index/bloom, a MANIFEST frame without a verifying CRC is never treated as published. | `[R]` by construction |
| **R9** | **Corruption is reported, not converted.** Checksum failure, bad magic, unsupported version, missing live object, unreadable required file → an error naming the object. The only silent-class exceptions are a torn tail of the *last* WAL segment / *last* MANIFEST frame, which are truncated and **reported** (quarantine + stderr + security log at the product layer). | GAPs: bare engine ignores WAL corruption (PERSISTENCE_MAP 4.1); trailing MANIFEST frame with a bad CRC is dropped without a report; F-D0-1/F-D0-2 classify *recoverable* shapes as corruption |
| **R10** | **Determinism and idempotence.** Recovering the same bytes twice yields the same state; a crash *during* recovery followed by another recovery yields the same final state as an uninterrupted one; a recoverable directory never becomes unrecoverable by a failed or interrupted recovery. | `[D]` class B only (F-11: 14 named startup points, process kill) |
| **R11** | **Orphans are inert.** Orphan/temp/leftover files are never read as data; they are deleted, adopted by a rule listed in PERSISTENCE_MAP 4.3, or ignored — and never prevent opening a directory that is otherwise recoverable. | `[R]` |
| **R12** | **Unacknowledged writes** are present/absent atomically (C-ACK-3); never malformed. | |
| **R13** | **Failed recovery/restore is non-destructive.** A recovery, restore or (later) PITR that fails does not alter the source directory beyond the defined, reported tail truncation, and never publishes a partial result. | restore: staging + one rename `[R]`; engine open truncates in place before any decision (mitigated by quarantine at the product layer `[D]`) |

Oracle requirement (Slice 1c): the recovery oracle must be **independent of the engine under test** — it keeps its own log of submitted/acknowledged commit units (key, value, seq, ack status) and compares the recovered *keyspace content* (every key/value, and for relational workloads the decoded rows + index entries), not row counts. Row counts are explicitly insufficient.

---

## 9. RPO, RTO and recovery-point granularity

No number below is a promise; each is either a definition or a measurement on one machine.

| Scenario | RPO | Basis |
|---|---|---|
| Process crash, disk intact | 0 acknowledged commits | CONTRACT; `[D]` class B measurements (0 lost in 23,386) |
| Power loss, disk intact | 0 acknowledged commits **if** A1-A5 hold | CONTRACT; **not established** — class C evidence required |
| Disk lost, logical backups only | age of the newest verified backup (`[D]` one measured case: an 8.0 s loss window ≈ 163 acknowledged rows/s at that run's write rate) | `PHASE_RUBIXDB_DISASTER_RECOVERY.md` §3 |
| Disk lost, backup + archived WAL (PITR) | 0 relative to the last archived durable frame | **does not exist** — Slice 6 BLOCKED |

| Scenario | RTO measured on this machine (i7-7700, SATA SSD, Balanced plan) `[D]` |
|---|---|
| Crash restart, 1 M-row database, 2,064 WAL records | 0.52 s to first query; engine recovery 113 ms |
| Restore from logical backup, same database | 72.3 s restore + 73.9 s end-to-end (scales ~linearly with stored entries) |

RTO is reported by Slice measurements, never promised. **Recovery-point granularity** (the unit by which a recovery target can be named): one commit unit, identified by its `seq` (LSN). A target `T` means "all commit units with `seq <= T`, inclusive". Timestamp targets are *not* supported by existing data (no WAL timestamps); supporting them would need a new persisted time→`seq` index and its own contract clause (Slice 6).

---

## 10. Contract for later slices (what each may promise only if its evidence exists)

| Slice | May claim | Only after |
|---|---|---|
| D1a | named fault points fire deterministically; class A and class B behaviour at each | each fault point fired at-once / Nth / seeded-probabilistic with an execution log; kill-9 recovery verified by the independent oracle |
| D1b | power loss is *simulated* faithfully to the model of section 6 | persistence abstraction used by **all** persistence I/O (or the BLOCKER RULE downgrade to "process-crash safety only"); the four proof tests |
| D1c | recovery invariants R1-R13 hold at every fault point under class C | matrix in `docs/CRASH_CLASS_MATRIX.md`, untested points listed with reasons |
| D2 | versioned, crash-safe manifest | D1c passed; migration crash-tested under B and C |
| D3/D4 | a backup/increment is consistent and independently restorable | consistency boundary defined here (backup = whole commit units at one `seq`); restore validates the full chain before touching a target |
| D5 | encrypted, authenticated backups | vetted AEAD, key-handling rules, tamper tests |
| D6 | PITR to a `seq` | WAL/change-history retention exists and is contiguous; otherwise BLOCKED |

---

## 11. Gaps between CONTRACT and TODAY (each must be closed, accepted by the maintainer, or disclaimed)

| ID | Gap | Where discovered | Needed |
|---|---|---|---|
| G1 | Zero-filled/garbage WAL tail ≥ 16 B condemns the whole segment (C-TORN-1b) | F-D0-1 `[M]` | **CLOSED 2026-10-13** (ADR-TORN-TAIL-ZERO-01; residual shape: G14) |
| G2 | Zero-filled MANIFEST tail ≥ 8 B makes open fail | F-D0-2 `[M]` | **CLOSED 2026-10-13** (ADR-TORN-TAIL-ZERO-01) |
| G3 | MANIFEST write has no rollback; a retried short write corrupts the file | F-D0-3 `[M]` (reachability `[U]`) | **CLOSED 2026-10-13** (ADR-MANIFEST-APPEND-ROLLBACK-01); premise reachability `[U]` |
| G4 | `next_seq` is derived only from the WAL; can fall below the checkpoint (R6) | F-D0-5 `[M]`/`[U]` | **CLOSED 2026-10-13** (ADR-WAL-SEQ-FLOOR-01); the oracle checks R6 in every recovery |
| G5 | Windows performs no directory sync; NTFS ordering (W1) unverified (A5) | `file_io.rs:288-296` | persistence model statement + class C simulation of "dir entry lost". **OPEN (accepted limitation, 2026-10-14).** D1b made "dir entry lost" executable (M6). The limitation, verbatim: On Windows, sync_dir is a no-op. Protection of directory entries rests on assumption W1 (NTFS metadata-log ordering), which is NOT established by any test in this repository. Power-loss durability of newly created wal/, sstables/ and MANIFEST entries on Windows is therefore NOT established. This is a Windows-specific exposure, not a documented guarantee. |
| **G6-POSIX** | `wal/`, `sstables/`, `MANIFEST`, the data directory and `DATA_FORMAT` were not followed by a parent-directory sync | PERSISTENCE_MAP 3.1; D1b F-D1b-2 `[M]` (51 of 153 smoke rows lost every acknowledged write under the pessimistic namespace model) | **CLOSED 2026-10-14** (ADR-DATA-DIR-DURABILITY-01, clause C-DIR-1). Evidence is simulated POSIX semantics; not run on a real Unix kernel `[U]`. |
| **G6-Windows** | On Windows the same entries are not protected by any directory sync (`sync_dir` is a no-op); protection rests on W1 | F-D1b-2 `[M]`: the Windows-model smoke rows (8 cycles × 3 drop-unsynced-namespace images = 24 rows) still fail | **OPEN (accepted limitation, maintainer 2026-10-14).** On Windows, sync_dir is a no-op. Protection of directory entries rests on assumption W1 (NTFS metadata-log ordering), which is NOT established by any test in this repository. Power-loss durability of newly created wal/, sstables/ and MANIFEST entries on Windows is therefore NOT established. This is a Windows-specific exposure, not a documented guarantee. The failing Windows-model class C rows stay FAIL, attributed to F-D1b-2, with this sentence. |
| G7 | Bare `LsmEngine::open` ignores WAL corruption (guard exists only in product entry points) | PERSISTENCE_MAP 4.1 | accept (product layer is the supported entry) or fix; tests that use the bare engine must not be read as evidence about corruption handling |
| G8 | Trailing MANIFEST frame with bad CRC dropped silently | `manifest/recovery.rs:110-117` | R9 reporting |
| G9 | No read-only-remount handling or test | grep | class D test |
| G10 | No directory sync after backup publish / restore promote / `DATA_FORMAT` rename | PERSISTENCE_MAP 3.7 | needed before D3/D4 claim "published backup survives power loss" |
| G11 | `fsync` honesty (A1) and Windows `sync_all` mapping unverified | §6 | disclaim or Tier 3/4 test |
| G12 | MANIFEST has no format version | PERSISTENCE_MAP 8 | Slice 2 |
| **G13** | **Acknowledged writes lost across a restart under concurrent writers**: the MemTable apply happens after the WAL acknowledgement, in arbitrary order among writers sharing a batch; a freeze between two members makes the flushed table's `max_seq` (hence the manifest checkpoint) cover a seq whose record is in no SSTable, and recovery skips every WAL record `<= checkpoint`. Happens across a graceful stop too. Not a power-loss question. | D1a F-D1a-1 `[M]` (pristine dde8ae9: 5 of 15 runs with 4 writers, 0 of 15 with 1) | **CLOSED 2026-10-13** (ADR-CHECKPOINT-CONTIGUITY-01, C-CKPT-1); C-ACK-1 is claimable for concurrent writers under class B. F-D1a-2 (`snapshot_seq`) remains open for D3. |
| **G14** | A WAL tail whose first sector was lost while later sectors of the same unsynced write persisted (all-zero header or zero prefix to the next sector boundary, non-zero remainder) was classified as **corruption**, not torn | D1b F-D1b-1 `[M]` (class C, simulated; A2/M3/M4); 108 of 400 P4 seeds | **CLOSED 2026-10-14** (ADR-TORN-TAIL-LOST-SECTOR-01, clause C-TORN-1(b4)): P4 "rejected as corrupt" 108 → 0 of 400; MANIFEST analogue included. Residual: G15. |
| **G15** | The model's M3 tears the final sector of a write at **half-sector** granularity (the first or second half of the write's bytes inside that sector), which contradicts A2 (a sector write is atomic). The resulting tails — a zero prefix of arbitrary, unaligned length followed by the rest of one frame, or a group-commit batch whose first frame is lost while a later frame survives intact — are not lost-first-sector shapes and are still classified **corruption** (fail closed; the synced prefix is intact). | STEP 1 re-run of the smoke cycle 2026-10-14 `[M]`: 3–5 class C rows per run (cycles `wal wal.sync=nth=50`, `engine flush.after_set_checkpoint`); four of the five rows of one run inspected byte by byte (zero run = exactly half of the write, ends mid-sector). Experiment (not landed): with M3 conformed to A2 the residual is **0 rows in 3 smoke runs** and the three runs agree (112 / 112 / 112 PASS). | **OPEN — maintainer decision**: (1) conform M3 to A2 (a sector is persisted whole or not at all); or (2) keep M3 as a deliberate stress beyond A2 and either extend the C-TORN-1(b4) rule to any zero prefix (this also turns `guard_f_d1b_1_only_the_header_zeroed_is_still_corruption` into a torn tail — a wider change) or accept the batch shape as fail-closed corruption with contract text. |


---

## 12. Clauses decided at the D1a decision gate (2026-10-13) — **IN FORCE** (maintainer approval 2026-10-13; `docs/D1a_DECISION_GATE.md` §0.0)

Each clause below belongs to one ACCEPTED, IMPLEMENTED ADR in `docs/adr/` and is falsifiable by a named test. The decision was **fix** for all of them, so the clause text is the contract and gaps G1–G4, G13 are closed. The "if accepted instead" column preserves the alternative that was **not** adopted, so that the choice stays visible.

| ID | Clause (IN FORCE) | Falsified by | The text that would have applied had the finding been **accepted** (NOT adopted) | ADR |
|---|---|---|---|---|
| **C-CKPT-1** | A manifest checkpoint `c` is recorded, and the WAL purged up to `c`, only when every commit unit with `seq <= c` (acknowledged or not) is contained in the table being published or in an earlier one, or is a checkpoint marker. Hence every acknowledged commit unit survives any restart — graceful or crash — regardless of the order in which concurrent callers applied their entries. Replacing the §1 definition "every commit with seq <= it is contained in a live SSTable", which is false under concurrency by construction. | `tests/pending_fixes.rs` `f_d1a_1_*` (deterministic); any row of the D1a concurrent column with the F-D1a-1 signature | "Under concurrent writers an acknowledged commit unit may be lost across any restart, graceful or not, when a MemTable freeze overtakes its apply." (R1 is then not claimable for multi-writer use.) | ADR-CHECKPOINT-CONTIGUITY-01 |
| **C-CKPT-2** | Replaying a WAL record whose entry is already in a table is idempotent (same key, seq, value; no visible effect). Restates R5. | prototype hunts / matrix with compaction on; to be tightened with a live-snapshot soak if C-CKPT-1 is adopted | (unchanged R5: moot because the filter prevents replay) | ADR-CHECKPOINT-CONTIGUITY-01 |
| **C-TORN-1(b)** *(narrowed)* | Recovery treats as a torn tail, truncates and reports: (b1) an incomplete frame; (b2) a frame whose claimed extent ends at end-of-file and fails its check; (b3) a region that is entirely zero from a frame boundary to end-of-file, of any length. Anything else that fails before a later valid frame is corruption, reported, never truncated silently. **Stale-garbage tails are no longer promised to be torn.** | `f_d0_1_*`, `f_d0_2_*` (and the guards that keep mid-file damage fatal) | "A zero-filled tail of 9+ bytes on the WAL (8+ on the MANIFEST) makes the directory unopenable until an operator removes it; the product start refuses with WAL_CORRUPT, the bare engine drops the segment." | ADR-TORN-TAIL-ZERO-01 |
| **C-MANIFEST-1** | A failed MANIFEST append leaves the file exactly as it was, or poisons the manifest; a later open succeeds or fails only for a reason that existed before the call. | `f_d0_3_*` | "A transient write error during a MANIFEST append can leave an unopenable directory; recovery requires an operator." | ADR-MANIFEST-APPEND-ROLLBACK-01 |
| **C-SEQ-1** (= R6) | After recovery the next assigned seq exceeds the manifest checkpoint, every seq in any live table and every seq in the WAL. | `f_d0_5_*`; the oracle's R6 check in every recovery | "If WAL content is missing, sequence numbers may restart below the checkpoint and later acknowledged commits are lost at the next restart." | ADR-WAL-SEQ-FLOOR-01 |

### 12.1 Clauses decided at the D1b/D1c STEP 1 (2026-10-14) — **IN FORCE** (maintainer decision 2026-10-14)

| ID | Clause (IN FORCE) | Falsified by | Not adopted alternative | ADR |
|---|---|---|---|---|
| **C-TORN-1(b4)** | Recovery also treats as a torn tail, truncates and reports: a frame whose first sector (the bytes from its start to the next 512-byte boundary) is entirely zero, with no complete valid frame behind the hole and no more than one maximum-size frame to end-of-file. A hole in front of a readable valid frame is corruption. WAL and MANIFEST. **Consequence stated plainly:** an acknowledged last frame whose first sector the medium later destroys, with nothing readable after it, is truncated and reported rather than refused. | `tests/pending_fixes.rs` `f_d1b_1_*` (2) and the guards `guard_f_d1b_1_*` (4); unit tests in `wal/recovery.rs` and `manifest/recovery.rs`; P4 | "A lost first sector makes the directory unopenable until an operator removes the tail." | ADR-TORN-TAIL-LOST-SECTOR-01 |
| **C-DIR-1** | After the engine creates the data directory, `wal/`, `sstables/`, `MANIFEST` or the `DATA_FORMAT` marker, it syncs the containing directory (the data directory's parent for the data directory itself) before relying on the entry, and re-syncs the data directory on every open. **POSIX only in effect:** on Windows the sync is a no-op and the limitation recorded in C-SYNC-2 / A5 applies unchanged. | `tests/dir_durability.rs` `posix_model_*` (simulated); `real_posix_kernel_directory_durability` is IGNORED (SKIPPED) on Windows | "The first-ever write may be lost with its directory entries under power loss on any platform." | ADR-DATA-DIR-DURABILITY-01 |

**Not a clause but a recorded gap (F-D1a-2, `OPEN_ITEMS.md`):** `snapshot_seq()` is the *durable* watermark, so a snapshot can have `seq >= s` for a durable commit `s` that is not yet applied and therefore not visible to it; a logical backup taken in that window omits `s` although the backup header claims consistency at `snapshot_seq` (§7 of the persistence map). Needed before D3 claims a consistency boundary.
