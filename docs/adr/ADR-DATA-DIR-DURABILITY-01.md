# ADR-DATA-DIR-DURABILITY-01 — Make the directory entries the engine creates durable (POSIX); accept the Windows exposure explicitly

| | |
|---|---|
| Status | **IMPLEMENTED 2026-10-14** for POSIX semantics (PROPOSED 2026-10-14; decided SPLIT by the maintainer in the D1b/D1c STEP 1 instruction). Windows: **ACCEPTED as a documented limitation, not fixed.** Implementation record: §7. |
| Date | 2026-10-14 |
| Finding | **F-D1b-2** (`OPEN_ITEMS.md`; gap **G6**, `docs/DURABILITY_CONTRACT.md` §11; `docs/PERSISTENCE_MODEL.md` §7.2) — split into **G6-POSIX (CLOSED here)** and **G6-Windows (OPEN)** |
| Touches | `src/wal/mod.rs`, `src/manifest/mod.rs` (**protected**; this ADR is the required record), `src/lsm/mod.rs`, `src/persist/mod.rs`, `src/ops/format.rs`, `src/ops/open.rs` (product layer). No on-disk format change. |
| Tests | `tests/dir_durability.rs` (feature `sim-fs`): `posix_model_*` (3) red before / green after; `windows_model_*` (1) and `windows_real_sync_dir_is_a_noop` (1) green before and after; `real_posix_kernel_directory_durability` IGNORED (= SKIPPED) |

Tags: `[R]` read, `[M]` measured, `[A]` analysis.

## 1. Context `[M]`

D1b measured that, under the pessimistic namespace model (`NsPolicy::DropUnsynced`) with an *effective* directory sync, every acknowledged write is lost, because the entries the engine creates are never synced into the directory that holds them: the data directory itself (and any ancestor the engine created), `wal/`, `sstables/`, `MANIFEST`, and the `DATA_FORMAT` marker (51 of 153 smoke rows). By reading every call site the engine synced only `wal/` (segment publish, purge) and `sstables/` (publish, orphan unlink). On POSIX a directory entry is durable only after an `fsync` of the containing directory (assumption A4/A5); without it a power loss can leave a database whose `wal/` or `MANIFEST` entry does not exist, however many `fsync`s the files received.

## 2. Decision (maintainer): SPLIT

* **(a) POSIX — FIX.** Sync the parent directory after each of: creation of the data directory (and ancestors it created), creation of `wal/`, creation of `sstables/`, creation of `MANIFEST`, and the rename of `DATA_FORMAT`.
* **(b) Windows — ACCEPT, with the limitation stated verbatim.** No fix and no pretence of one. The sentence (do not soften), recorded in `DURABILITY_CONTRACT.md` §3 (C-SYNC-2) and §6 (A5) and `PERSISTENCE_MODEL.md` §3 (M6):

  > On Windows, sync_dir is a no-op. Protection of directory entries rests on assumption W1 (NTFS metadata-log ordering), which is NOT established by any test in this repository. Power-loss durability of newly created wal/, sstables/ and MANIFEST entries on Windows is therefore NOT established. This is a Windows-specific exposure, not a documented guarantee.

* **(c)** Gap G6 splits: **G6-POSIX CLOSED**, **G6-Windows OPEN** with the sentence above.

## 3. The change `[R]`

One helper and five call-site edits; every new call is `crate::persist::sync_dir` / `wal::fsync_dir`, i.e. the existing primitive (real `fsync` on Unix, a no-op on Windows, the simulator's model under `sim-fs`).

| Where | Edit | Syncs |
|---|---|---|
| `src/persist/mod.rs` | new `create_dir_all_durable(path)`: `create_dir_all`, then `sync_dir` on the **parent of every directory this call created** | parent(s) of newly created directories |
| `src/wal/mod.rs` `canonicalize_data_dir` (creates `wal/`) | `create_dir_all_durable`, then `fsync_dir(parent)` on **every** open | the data directory (entry `wal/`), and the data directory's parent if the data directory was created |
| `src/manifest/mod.rs` `open_after_exclusive_lock` | `create_dir_all_durable(dir)`; after the open/create of `MANIFEST`, `sync_dir(dir)` on **every** open | the data directory (entry `MANIFEST`) |
| `src/lsm/mod.rs` `reconcile_sstables_with_manifest` | `create_dir_all_durable(sstables/)`, then `sync_dir(parent)` on **every** open | the data directory (entry `sstables/`) |
| `src/ops/format.rs` `stamp_if_fresh` | `create_dir_all_durable(dir)`; `sync_dir(dir)` after the `DATA_FORMAT.tmp` → `DATA_FORMAT` rename | the data directory (entry `DATA_FORMAT`) |
| `src/ops/open.rs` `open_engine_for_ops` | `create_dir_all_durable(dir)` | parent of a newly created data directory |

*Why "on every open" for the engine-owned parent.* If an earlier attempt created `wal/` (or `MANIFEST`, `sstables/`) and failed before syncing, a later open sees the entry already present; syncing only on creation would never repair it. The parent in these cases is the data directory, which the engine already reads and locks, so the extra `fsync` cannot fail for a permission reason the engine did not already have. *Why only "created by this call" for the data directory's own parent:* that parent is outside the engine's ownership and may be traversable but not readable; opening it for `fsync` could turn a working start into a failing one. A data directory created by an earlier failed attempt whose parent sync also failed remains a double-fault window; it is named in §6.

Cost `[A]`: at most four directory `fsync`s per engine open, none on the write path. No change to the acknowledgement barrier (C-ACK-1) or to any fsync ordering on the commit path.

## 4. Behaviour by platform

* **Unix:** the new syncs are real `fsync`s of directories. **Not executed on a real Unix kernel in this repository** `[U]`; the evidence is the simulator configured with POSIX semantics (`dir_sync_effective = true`, A5), which proves that the engine *requests* the sync in the right places and that, if the kernel honours it, no entry is left awaiting one.
* **Windows:** `sync_dir` is a no-op; the new calls change nothing. The exposure is **unchanged** and is exactly the sentence in §2(b). The Windows-model smoke rows that fail under the pessimistic namespace policy **remain FAIL**, attributed to F-D1b-2, with that sentence.

## 5. Tests

`tests/dir_durability.rs` (feature `sim-fs`, run on every host; evidence class **SIMULATED**, never power-loss evidence for a real filesystem):

* `posix_model_every_directory_entry_the_engine_creates_is_synced` — after a fresh `LsmEngine::open` of `/simroot/a/b/engine` and one acknowledged write, `non_durable_ns_ops()` mentions none of: `a`, `a/b`, `engine`, `wal`, `sstables`, `MANIFEST`. **Red before the fix** (`/simroot/a` awaiting a sync), green after.
* `posix_model_power_loss_with_the_pessimistic_namespace_policy_keeps_the_directories_and_the_acked_write` — strict data loss + `DropUnsynced` namespace: every directory and `MANIFEST` survives and the engine reopens on the survivors with the acknowledged value. Red before, green after.
* `posix_model_the_product_start_path_leaves_nothing_awaiting_a_directory_sync` — the same through `ops::open::open_engine_for_ops`, including `DATA_FORMAT`.
* `windows_model_the_same_open_leaves_directory_entries_unprotected` — the Windows model (`dir_sync_effective = false`) still shows entries awaiting a sync and loses the data directory under `DropUnsynced`. It passes **by observing the exposure**; it is not a claim that Windows is safe.
* `windows_real_sync_dir_is_a_noop` (Windows only) — pins that the real `sync_dir` does nothing (a directory that does not exist is accepted).
* `real_posix_kernel_directory_durability` (Windows only, `#[ignore = "SKIPPED: …"]`) — the test a real Unix kernel needs; reported **IGNORED**, never PASS. No test in this repository passes on Windows *because* `sync_dir` is a no-op.

Plus the D1b smoke cycle: the `dirsync=1` rows no longer fail with F-D1b-2.

## 6. What this does not do

* **Windows power-loss durability of directory entries is not established** (§2b). W1 is untested.
* No fault point is added for the new syncs (the D1a registry stays at its 64 points); their error path is therefore class-A-untested. Recorded in `OPEN_ITEMS.md`.
* Not addressed (other directory entries the engine relies on): compaction input unlink, backup publish rename, restore promote rename, WAL quarantine rename (gap G10, unchanged) — out of this ADR's scope.
* The double-fault window of §3 (parent sync failed once, process restarted, power loss) is named, not closed.
* Not run on a real Unix kernel; not run with `dm-flakey`; no real power cut.

## 7. Implementation record (2026-10-14)

Landed as in §3. Measured effect: `tests/dir_durability.rs` 5 pass + 1 ignored (the three `posix_model_*` tests fail on the tree with the five call-site edits reverted and pass with them, `[M]`). Smoke cycle: with the Windows model the 8 `dirsync=0` cycles × 3 drop-unsynced-namespace images = 24 rows still fail, all attributed F-D1b-2; the `dirsync=1` rows no longer fail with F-D1b-2 (the 51 rows of the D1b runs fell to 24, exactly the Windows-model rows).
