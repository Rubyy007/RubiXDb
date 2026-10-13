# ADR-PERSISTENCE-TRAIT-01 — One persistence layer for every durable byte, with a simulator behind it

| | |
|---|---|
| Status | **ACCEPTED under the D1b mission text** (the mission names the Persistence trait, `RealFs`, `SimFs` and the sites to thread; no separate approval gate was requested). Recorded here because it touches protected paths. |
| Date | 2026-10-13 |
| Slice | D1b — power-loss persistence model + simulation |
| Closes | prerequisite **P9** (persistence abstraction) |
| Touches | **new** `src/persist/` (`mod.rs`, `sim.rs`); mechanical call-site changes in `src/wal/` (`mod.rs`, `file_io.rs`, `group_commit.rs`, `recovery.rs`), `src/manifest/` (`mod.rs`, `recovery.rs`), `src/sstable/` (`mod.rs`, `reader.rs`, `writer.rs`) — **protected** — and `src/lsm/mod.rs`, `src/ops/*`, `src/startup_probe.rs` (product layer) |
| Out of scope (by design) | the `instance`, `api` and `cli` crates (`instance.json`, `credentials.json`, `security.log`, host files): product bookkeeping, not database data; listed in `docs/PERSISTENCE_MODEL.md` §5 so that nothing is silently excluded |

## 1. Context

D0 found only WAL *segment bytes* behind a trait (`WalFile`); every other persistence operation called `std::fs` directly (P9, ~65 sites estimated; the measured count is in §4). Power loss cannot be produced on this platform (Windows/NTFS, no dm-flakey, no hardware cut rig), so the only obtainable evidence is a **model**: a simulator that records what was written and what was made durable, and a way to apply "power loss" to that record. That requires every durable byte and every namespace change to flow through one layer.

## 2. Decision

A module `crate::persist` provides, for the engine crate:

* `PFile` — a drop-in for `std::fs::File` (it implements `Read`, `Write`, `Seek`, the WAL's `WalFile`, and has `sync_all`, `set_len`, `len`, `try_clone`, `try_lock`, `try_lock_shared`);
* `OpenOptions`, and free functions with the `std::fs` names and shapes: `rename`, `remove_file`, `remove_dir`, `remove_dir_all`, `create_dir`, `create_dir_all`, `hard_link`, `copy`, `read`, `read_to_string`, `write`, `metadata`, `exists`, `is_dir`, `is_file`, `canonicalize`, `read_dir`, `sync_dir`;
* the operations of the mission's trait list — open, create, rename, remove, create_dir, read_dir, write_at, read_at (`Read`/`Seek` over positional reads), set_len, sync_all, sync_dir, len — all present as above.

**`RealFs`** is the only implementation compiled into a product build: each function is an `#[inline]` forward to the corresponding `std::fs` call and `PFile` is a newtype around `std::fs::File`, so a product build has **no dynamic dispatch, no global state and no behavioural change**, including the Windows no-op `sync_dir` and the Windows `seek_write` loop of `WalFile::write_all_at`.

**`SimFs`** exists only with the cargo feature `sim-fs` (absent from product builds, guarded like `fault-points`): when a simulator is installed in the process, `persist` routes every call to it instead of `std::fs`; `PFile` then holds a simulated handle. Process-global selection is deliberate: a handle through every type (`FileWal`, `Manifest`, `LsmEngine`, backup, restore, the guard) would touch hundreds of signatures for no gain, because a simulated process is simulated *entirely* (a child process whose whole data directory is virtual).

Rejected alternatives: (a) a generic parameter `FileWal<F: Persistence>` threaded through every type — invasive in protected code, same behaviour; (b) a trait object stored in each type — adds a vtable call to every product I/O for a test-only need; (c) intercepting at the OS level (Detours, dm-flakey) — not available here.

## 3. The simulator's contract (executable form of `docs/PERSISTENCE_MODEL.md`)

Per file: volatile content, content as of the last successful `sync_all` (durable), and the ordered list of writes / length changes since then. Per directory: a log of namespace operations (create, mkdir, rename, link, remove) each marked durable once `sync_dir` has been called on the affected directory(ies) after it. `power loss` is a pure function of `(dump, crash config, seed)`: M1 unsynced writes may be lost; M2 synced writes survive (assumption A1); M3 the last persisted write may be torn at sector granularity; M4 unsynced writes may persist in any subset/order when reordering is enabled; M5 a file's length may become durable before its data (zero tail); M6 a non-durable namespace operation may be lost (and is the common case on Windows, where `sync_dir` does nothing in `RealFs`); M7 `sync_all` may lie (off by default); M8 deterministic. After a simulated crash the simulator is dead: later calls park, so no operation can complete (and be acknowledged) after the dump instant.

## 4. Scope of the threading — measured

Inventory by script (`fs_inventory`, production code only, `#[cfg(test)]` modules excluded): **157 lines** mention a filesystem API in `src/` plus **51** path-method queries (`exists`, `is_dir`, `metadata`, `canonicalize`, `read_to_string`, `copy`, `write`) = ≈ **200 operations** in 17 files — about three times the D0 estimate (D0 counted `std::fs` call sites in the four engine directories and `ops` and did not count handle types or path queries). The conversion is mechanical (same names, same shapes). The full per-file table and the closing check are in `docs/PERSISTENCE_MODEL.md` §5.

## 5. Completeness is enforced, not asserted

`tests/persistence_abstraction_complete.rs` (default build) scans the production part of every file under `src/` and fails on any use of `std::fs`, `fs::File`, `std::fs::File`, `OpenOptions` from `std`, `Path::exists/is_dir/is_file/canonicalize/metadata` outside `src/persist/` and an explicit allow-list (test seams in `#[cfg(test)]` modules are excluded by construction). A new direct filesystem call therefore fails the build of the test suite, and a simulated-crash claim can never silently depend on an uncovered path.

## 6. Risks and how they are bounded

1. **Product behaviour change** — bounded by `RealFs` being a straight forward; verified by the unchanged workspace suite (D0 counts) and the byte-equivalence of the existing WAL/manifest/SSTable tests.
2. **Simulator fidelity** — the model is *assumptions made executable*, not a measurement of any device; every class-C result is labelled "Simulated under docs/PERSISTENCE_MODEL.md. Assumptions A1-A5 inherited and untested on this platform." The four proof tests (P1–P4) gate admissibility.
3. **Windows-only behaviour in `RealFs`** (`seek_write` position side-effect, rename restrictions on open handles) is preserved by construction; `SimFs` does not model it.
4. **Global state in test builds** — installing a simulator is process-wide; tests that install one run in their own child process (the crash-injection cycle) or serialise on a lock.

## 7. Blocker rule

If the completeness test could not be made to pass — i.e. a persistence path could not be routed — D1b would be reported **BLOCKED** and the durability claim downgraded to "process-crash safety only". That did not happen; see the D1b report.
