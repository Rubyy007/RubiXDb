# rubiXDb — PERSISTENCE MODEL (D1b)

Slice D1b, 2026-10-13. Base `dde8ae9` plus the uncommitted D0/D1a/gate work. Implementation: `src/persist/` (`mod.rs` = `RealFs`, `sim.rs` = `SimFs`). Decision record: `docs/adr/ADR-PERSISTENCE-TRAIT-01.md`. Contract: `docs/DURABILITY_CONTRACT.md` (assumptions A1–A8 are defined there; this file makes them executable).

> **Platform truth.** Everything in this document was produced on **Windows 10 Home 10.0.19045, NTFS, SATA SSD, `rustc 1.98.1`, debug builds**. **Nothing was run on Linux or macOS.** On this platform the engine's `sync_dir` is a **no-op** (Windows has no directory fsync; `src/wal/file_io.rs`), and there is no `dm-flakey` and no hardware power-cut rig. Every class C ("power loss") statement below is therefore **simulated and model-based**.
>
> **Real power loss (cutting power to a machine): NOT tested.** Nothing here is a measurement of any storage device. Assumptions A1–A5 and W1 are inherited, are **untested on this platform**, and the simulator makes them *executable*; it does not *verify* them.

## 1. What this is, and what it is not

A model is a set of rules for what a power loss does to a filesystem. `SimFs` implements those rules; the engine runs on it unmodified (`crate::persist`), a crash is applied to its record, the survivors are written to a real directory and the real engine recovers them under the independent oracle of D1a. The rules are **assumptions**. If they hold, "recovery is correct" is a statement about the engine; if a real device breaks them, the statement says nothing about that device. A claim that depends on an assumption carries its ID (`A1`…`A8`, `W1`) and, where the assumption cannot be tested here, the tag **[UNTESTABLE]**.

Class C evidence is admissible **only because the four proof tests of §6 pass**. If any of them fails, every class C row is inadmissible.

## 2. Assumptions (definitions in `DURABILITY_CONTRACT.md` §6)

| ID | In one line | Executable as | Testable on this platform? |
|---|---|---|---|
| **A1** | after `sync_all` returns `Ok`, the bytes survive a power loss | M2 (and M7 breaks it on purpose) | **[UNTESTABLE]** — needs a real power cut |
| **A2** | a physical sector write is atomic; a multi-sector write persists in any subset/order | M3, M4 (sector size 512 by default, configurable) | **[UNTESTABLE]** — device property; sector size unknown to the engine |
| **A3** | a file's length can become durable before its data (zero or stale tail) | M5 | **[UNTESTABLE]** |
| **A4** | `rename` is atomic at the namespace level; its durability needs a directory sync on POSIX | M6 | **[UNTESTABLE]** on NTFS |
| **A5** | a directory-entry change is durable only after a directory sync, where the platform has one — **Windows: none** | M6, with `dir_sync_effective = false` for the Windows model | **[UNTESTABLE]** — what NTFS does instead is **W1** |
| **W1** | NTFS's metadata journal preserves the order of metadata operations, so a later flushed file implies earlier renames | the optimistic namespace policy `KeepAll` (and the pessimistic one is its negation) | **[UNTESTABLE]** |
| **A6** | after an `fsync` error the dirty pages' state is unknown; the engine never retries an fsync | class A (D1a), not modelled by `SimFs` | tested as class A only |
| **A7** | synced data changes only by bit rot, which checksums detect | not modelled | not tested here |
| **A8** | one process owns the data directory | `try_lock` is modelled per file (shared/exclusive) | yes |

## 3. The model, M1–M8

`SimFs` keeps, per file: the volatile content, the content as of the last successful `sync_all` (*durable*), and the ordered list of writes and length changes since (*pending*). Per directory it keeps an ordered log of namespace operations (create, mkdir, rename, link, remove, rmdir), each durable only after a `sync_dir` on the directory(ies) it affects. A power loss is `SimFs::crash(&CrashCfg)`: a **pure function of (record, configuration, seed)**.

| ID | Rule | Where / how configured | Assumption | Shown by |
|---|---|---|---|---|
| **M1** | Writes and length changes not covered by a successful `sync_all` may be lost. | `DataPolicy::DiscardUnsynced` (all lost) or `Random` (any subset) | A1 | P1 |
| **M2** | Writes covered by a successful `sync_all` survive, whatever else happens to the file afterwards. | always | **A1** | P2 |
| **M3** | The last persisted unsynced write may be torn at sector granularity: its final sector keeps its first half, its second half, or none. | `Random { tear: true }`, `sector` (512 default) | **A2** | P4 |
| **M4** | Unsynced writes may persist in any subset (reordering). Without reordering only a prefix of them persists. | `Random { reorder: true/false }` | **A2** | P4 (and §7) |
| **M5** | The *length* of an unsynced extension may become durable without its data: a zero-filled tail. | `Random { length_before_data: true }` | **A3** | P4, §7 |
| **M6** | A namespace operation not covered by `sync_dir` may be lost: `KeepAll` (optimistic, W1), `DropUnsynced` (pessimistic), `Random(p)`. `dir_sync_effective = false` makes `sync_dir` do nothing — **the Windows model** — so nothing becomes durable by itself. A lost directory takes its contents with it. | `NsPolicy`, `SimConfig::dir_sync_effective` | **A4, A5, W1** | P3 |
| **M7** | `sync_all` may return `Ok` and make nothing durable (a device that lies). **Off by default**; exists for one dedicated test. | `SimConfig::fsync_lie` | violates **A1** | `fsync_lie_loses_synced_data_and_is_a_violation_of_a1` |
| **M8** | Determinism: the same dump, configuration and seed give identical survivors. Decisions are hashed from `(seed, tag, file, write uid)`, never from call order, so thread timing cannot change an outcome. | `crash` is pure | — | `same_dump_and_seed_give_the_same_survivors` |

**Also modelled:** `DataPolicy::KeepAll` — *not a power-loss rule*: every completed write and namespace operation survives. It is the **process-kill image** (crash class B); the kill-9 variant of the cycle uses it, which is the "skip step 4" of the mission. **After a dump the machine is dead:** `dump_and_kill` marks the simulator dead in the same critical section, so no operation can complete — and be acknowledged — after the dump instant.

**Not modelled** (so nothing downstream may claim it): bit rot and silent corruption (A7); torn *directory* entries; `rename` atomicity failures (rename is atomic in the model, A4); fsync *errors* (class A covers them); disk-full and read-only remount (class D); file timestamps and permissions; a write cache lying about order across files beyond M4; multi-device setups; sector sizes other than the configured one (4 KiB is configurable but was not run).

## 4. The crash-injection cycle

Implemented by `tests/sim_power_loss/main.rs` driving `examples/fault_child.rs` (both need `--features fault-points,sim-fs`).

1. The parent spawns the child on a **simulated** filesystem (`sim=1`, data dir `/simroot/data`, which does not exist on the real disk), with a fault point + trigger whose action is `simcrash` (or `end=simcrash`) and a workload seed. The ledger of submitted/acknowledged operations stays on the real disk (test machinery).
2. The child runs the deterministic D1a workload.
3. At the point, the `simcrash` action calls the installed hook: `dump_and_kill` of the simulator to a file, then `exit(0)`. **No real-disk flush of the data directory ever happens.**
4. The parent rebuilds the simulator from the dump and applies the model (`CrashCfg`: discard unsynced, tear, reorder, drop un-dir-synced entries, zero tails).
5. The parent writes the survivors under a real temp directory (`Survivors::write_under`).
6. A fresh engine on `RealFs` recovers it through the product start sequence (`oracle::product_open`).
7. The independent oracle (`tests/support/oracle.rs`, unchanged from D1a) checks R1–R13 and that the recovered state equals `model(K)` per writer for some `K` with `acked <= K <= submitted`.

**Kill-9 variant** (class B): identical, but step 4 is `process_kill_image()` — nothing is discarded. It must agree with D1a's class B results, and the smoke test asserts that none of its rows fails.

**Labels.** Every class C row carries: *"crash class C - SIMULATED power loss (docs/PERSISTENCE_MODEL.md; A1-A5/W1; real power loss NOT TESTED)"*. Every kill-9 row carries *"crash class B - process kill (kill-9 variant; not power-loss evidence)"*. The row files are `target/sim-power-loss-results/smoke.tsv`.

## 5. Where the abstraction is used — and where it is not

### 5.1 Threaded through `crate::persist` (production code)

| File | Persistence operations |
|---|---|
| `src/wal/file_io.rs` | segment `open/create`, positional write, `sync_all`, `set_len`, `sync_dir`, `try_lock`; `impl WalFile for PFile` |
| `src/wal/mod.rs` | segment discovery (`read_dir`), tmp-write + `rename` + dir sync of a new segment, purge `remove_file`, `LOCK` |
| `src/wal/group_commit.rs` | the leader's `sync_all` |
| `src/manifest/mod.rs`, `src/manifest/recovery.rs` | append + `sync_all`, rollback `set_len`, replay reads, lock |
| `src/sstable/writer.rs`, `reader.rs`, `mod.rs` | `.sst.tmp` write + `sync_all` + `rename` + dir sync; table reads |
| `src/lsm/mod.rs` | startup sweeps (`*.sst.tmp`, orphans, removed tables), directory creation, physical delete after compaction |
| `src/ops/{backup,restore,format,open,check,sstable_integrity,wal_tail}.rs` | backup partial + publish, restore staging + promote, `DATA_FORMAT` stamp, quarantine temp + rename, read-only checkers |

Closing check (enforced, not asserted): `tests/persistence_abstraction_complete.rs` (default build) scans the production part of every file under `src/` and fails on any `std::fs` path or any `.exists()` / `.canonicalize()` / `.is_dir()` / `.is_file()` on a path outside `src/persist/`. Dynamic evidence: the whole engine runs on a virtual root that does not exist on the real disk (`the_engine_runs_entirely_on_the_simulated_filesystem`), and the crash cycle does the same in a child process; a leaked direct call would fail there with `NotFound`.

**Allow-list (two files, justified):** `src/faults.rs` and `src/startup_probe.rs` write the *test harness's own* files (execution log, ready marker, probe marker). They must stay on the real disk while the database under test is simulated, and hold no database data.

### 5.2 Not threaded — out of scope of D1b, and what that means

The `instance`, `api`, `cli` and `sql` crates call `std::fs` directly. None of it is engine data (WAL, MANIFEST, SSTables), but some of it is persistence a power loss can damage, so it is listed rather than ignored:

| Crate / file | What it persists | Consequence |
|---|---|---|
| `instance/src/credentials.rs` | `credentials.json` (tmp write + `rename`) | durability of the admin credential file under power loss is **NOT TESTED** |
| `instance/src/lib.rs` | instance directory create / list / remove | instance creation/removal atomicity under power loss **NOT TESTED** |
| `api/src/security_log.rs` | append-only `security.log` with rotation (`rename`) | log loss/tearing under power loss **NOT TESTED**; not database data |
| `api/src/routes/admin.rs` | deletes a backup file | **NOT TESTED** under power loss |
| `api/src/observability/sampler.rs`, `api/src/resources.rs` | read-only listing / `/proc` reads | no persistence |
| `cli/src/embedded_frontend.rs` | extracts the embedded frontend (staging dir + rename) | cache directory; not database data |
| `cli/src/host.rs`, `cli/src/ops_cmd.rs` | `create_dir_all` of the data/backup directory | the creation of the data directory itself is **not synced into its parent** — see F-D1b-2 |
| `cli/src/main.rs` | reads user-supplied files | no persistence |
| `sql` crate | tests only | — |

These do not weaken the statements about the engine and `ops`; they bound them: **no power-loss statement in this document covers `credentials.json`, `security.log`, `instance.json`-style bookkeeping, the frontend cache, or directory creation by the host.**

## 6. The four proof tests (gate for every class C claim)

`tests/sim_proof.rs`, `cargo test --features sim-fs --test sim_proof`. They run in-process; one lock serialises them because the simulator is process-global.

| ID | Test | Asserts | Result |
|---|---|---|---|
| **P1** | `unsynced_write_is_lost_on_crash` | a write with no `sync_all` is gone under the strict policy; under the random policy it can be lost *and* kept (300 seeds, both occur) and only ever shows its own bytes or zeros | PASS |
| **P2** | `synced_write_survives_crash` | after `sync_all` + `sync_dir`, the synced bytes survive every data policy × every namespace policy × 300 seeds, even when later unsynced writes follow | PASS |
| **P3** | `dir_entry_lost_without_dir_sync` | a synced file in a directory that was not synced can vanish; it survives after an effective `sync_dir`; with the Windows model (`dir_sync_effective = false`) calling `sync_dir` changes nothing; the optimistic policy keeps it | PASS |
| **P4** | `torn_sector_is_rejected_or_repaired` | the **real** WAL on `SimFs`: three synced records + one unsynced 1.5 KB record spanning three sectors; 400 seeds; the synced prefix is byte-identical in every survivor, recovery never returns an altered record, and both outcomes occur | PASS |
| — | `fsync_lie_loses_synced_data_and_is_a_violation_of_a1` | M7 | PASS |
| — | `same_dump_and_seed_give_the_same_survivors` | M8 across a dump round trip, 30 seeds, outcomes differ between seeds | PASS |
| — | `the_engine_runs_entirely_on_the_simulated_filesystem` | WAL + MANIFEST + SSTables + flush + rotation + purge on `SimFs`, nothing on the real disk, reopen works, a strict power loss at a clean end recovers all 250 keys | PASS |
| — | `the_simulated_handle_reads_seeks_and_truncates_like_a_file` | handle semantics | PASS |

**P4 outcomes over 400 seeds** (deterministic): 4 exact records = 7; 3 records, the unsynced record lost without a stub = 103; 3 records, a torn tail detected and truncated = 182; **rejected as corrupt = 108**. The last bucket is finding F-D1b-1 (§7). "Rejected" is allowed by the proof — a torn sector is never *accepted* — but it is a finding.

## 7. Smoke results and findings (NOT the D1c matrix)

**This is a smoke set, not the D1c matrix.** Labelled: *crash class C — SIMULATED power loss, assumptions A1–A5/W1, real power loss NOT TESTED.* 17 cycles × 9 crash images = 153 rows (136 class C + 17 class B kill-9).

*Cycles (17):* engine workload (1 writer, 250 commit units, compaction off) crashed at `wal.fsync` (30th hit), `wal.fsync_done` (60th), `wal.record_written` (100th), `flush.after_sstable_publish`, `manifest.synced`, `flush.after_set_checkpoint`, and at the end of the workload; the bare WAL (Immediate mode) at `wal.sync` (50th hit); each with the Windows model (`dirsync=0`) and the effective-dir-sync model (`dirsync=1`), data directory pre-created and durable; plus one cycle with the data directory **not** pre-created (first open).
*Images (9):* process kill (class B); strict (all unsynced lost, all namespace kept); random data (reorder + tear + length-before-data) with the namespace kept, seeds 1–4; random data with unsynced namespace dropped, seeds 1–2; strict with unsynced namespace dropped.

Three consecutive runs on 2026-10-13 (the child's thread timing moves which step a point-counted trigger hits, so counts differ by one or two):

| run | class B (kill-9) | class C PASS | class C FAIL | of which F-D1b-1 | of which F-D1b-2 | UNCLASSIFIED |
|---|---|---|---|---|---|---|
| 1 | 17 / 17 PASS | 80 | 56 | 5 | 51 | 0 |
| 2 | 17 / 17 PASS | 80 | 56 | 5 | 51 | 0 |
| 3 | 17 / 17 PASS | 79 | 57 | 6 | 51 | 0 |

* **Class B agrees with D1a:** the kill-9 variant of the cycle (no model applied) passes in all 17 cycles in all runs.
* **Every class C FAIL is explained by one of two shapes** (attribution is by shape, printed per row in `target/sim-power-loss-results/smoke.tsv`, not proven row by row): *F-D1b-1* — recovery rejects the WAL as corrupt (the engine refuses to open); *F-D1b-2* — nothing the engine created survives the pessimistic namespace policy. No row is unexplained.
* **F-D1b-2 is exactly the set of drop-unsynced-namespace images:** 17 cycles × 3 such images = 51, in both dir-sync modes and with a pre-created data directory. Making `sync_dir` effective (`dirsync=1`) does **not** help, because the engine never syncs the directory that holds `wal/`, `sstables/` and `MANIFEST`.
* **Where the model is optimistic about the namespace (W1, `KeepAll`), 79–80 of the 85 images PASS** (17 cycles × the 5 images that keep the namespace); the failures there are F-D1b-1 only.
* A PASS means the oracle accepted that one simulated crash image: acknowledged writes recovered, `acked <= K <= submitted`, R1–R13 as implemented in D1a, recovery idempotent.

### 7.1 F-D1b-1 — a lost first sector makes the WAL "corrupt", not "torn" (class C, simulated)

When the first sector of the last unsynced frame is lost but later sectors of the same write persist (M3/M4 under A2 only), the tail has an all-zero header followed by non-zero bytes. The zero-tail rule of ADR-TORN-TAIL-ZERO-01 does not apply (the remainder is not all zero); the header reads as `len = 0, crc = 0`; the empty body passes its CRC (`crc32c("") = 0`), fails to decode, and — because its claimed extent does not reach end-of-file — is classified as **corruption**. The product start then **refuses to open** (`WAL_CORRUPT`, fail closed, directory not modified) and bare `inspect` discards the whole segment's records. The synced prefix is intact on disk; recovery needs an operator. 108 of 400 seeds in P4; 5–6 rows in each of three smoke runs. *Not fixed:* `src/wal/recovery.rs` is protected and D1b is not authorised to change it (contract gap **G14**).

### 7.2 F-D1b-2 — the engine never syncs the database directory (class C, simulated, A4/A5)

`SimFs::non_durable_ns_ops()` after a run shows `Mkdir wal`, `Create MANIFEST` and `Mkdir sstables` still *awaiting a sync of the data directory* (and, without a pre-created data directory, `Mkdir data` awaiting a sync of its parent). By reading every call site (`grep` for `fsync_dir` / `sync_dir`), the only directories the engine ever syncs are `wal/` (segment publish `wal/mod.rs:408`, purge `wal/mod.rs:1117`) and `sstables/` (publish `sstable/writer.rs:238`, orphan unlink `lsm/mod.rs:3074`). It never syncs the data directory that holds `wal/`, `sstables/` and `MANIFEST`, nor the data directory's own parent; the product start path (`ops/format.rs` stamp, `DATA_FORMAT`) has no directory sync either. The smoke cycles drive `LsmEngine::open` directly, so the *observation* is of that path; the grep covers the product path. Under the pessimistic namespace policy (`DropUnsynced`) those entries vanish, taking every acknowledged write with them: 51 of 153 smoke rows (all of the drop-unsynced-namespace images). On POSIX this is a real exposure (A4); **on Windows `sync_dir` is a no-op, so the result is the same whatever the engine does — what protects a Windows deployment is W1 [UNTESTABLE]**. This is the measured consequence of contract gap **G6**. *Not fixed:* needs an ADR (`src/wal`, `src/manifest`, `src/sstable`) and an explicit authorisation.

### 7.3 What the smoke set does and does not show

It shows that the cycle works end to end, that the model changes outcomes (every cycle yields differing images), that the kill-9 variant agrees with D1a, and that two specific shapes fail. It does **not** cover every fault point (that is D1c's matrix), every seed, 4 KiB sectors, concurrent writers under class C, compaction under class C (`compaction=off` in the child), or the relational workload. A PASS row means "the oracle accepted this one simulated crash image". Row counts vary slightly run to run because the child's thread timing decides which workload step a point-counted trigger hits; the *images* of a given dump are deterministic.

## 8. Reproduce

```
cargo build --features fault-points,sim-fs --example fault_child
cargo test  --features sim-fs                     --test sim_proof                 # P1-P4, M7, M8, engine on SimFs
cargo test  --features fault-points,sim-fs        --test sim_power_loss -- --nocapture   # smoke cycles; RUBIXDB_SIM_STRICT=1 fails on any FAIL row
cargo test  --test persistence_abstraction_complete                               # default build: the completeness scan
cargo test  --test fault_points_absent_from_product                               # neither feature may ship
```

A product build contains none of this: `src/persist/sim.rs` is compiled only with `sim-fs`, no workspace member lists the feature, `tests/fault_points_absent_from_product.rs` enforces that, and `scripts/check_no_fault_points.ps1` byte-searches the release binary for the control variables and for the simulator's dump magic `RBXSIMD1`.
