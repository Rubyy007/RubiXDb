# ADR-FAULTPOINTS-01 — One test-only fault-point registry

| | |
|---|---|
| Status | **APPROVED** (AD-1 … AD-7 approved by the maintainer, 2026-10-13) **with amendments A1–A7 (section 12, same date).** Section 12 prevails where it differs from sections 1–11. |
| Date | 2026-10-13 |
| Slice | D1a (durability / PITR / backup mission) |
| Baseline | `dde8ae9` on `master`; D0 documents in `docs/` are the source of truth |
| Touches | protected: `src/wal/`, `src/manifest/`, `src/sstable/` (call sites only). Product layer: `src/lsm/mod.rs`, `src/ops/`, `Cargo.toml`, new `src/faults.rs` |
| Depends on | `docs/PERSISTENCE_MAP.md` §3, `docs/PREREQUISITES.md` P10, `docs/DURABILITY_CONTRACT.md` §4/§7/§8 |

Evidence tags as in D0: `[R]` read in source at HEAD, `[D]` stated by an existing document, `[U]` unverified.

---

## 1. Context

D1a must fire a fault at **every** persistence boundary the mission lists, deterministically, with an execution log, and verify recovery after a process kill at each one.
D0 (P10) found the existing facilities are one hook per subsystem, each with its own shape `[R]`:

| Existing facility | Where | Reaches | Cannot |
|---|---|---|---|
| `AbortPoint` + `set_abort_hook(fn)` (feature `test-util`) | `src/wal/mod.rs:1293-1362` | 11 WAL / group-commit boundaries; one process-wide `fn` pointer in a `OnceLock`, first writer wins | inject an error, count hits, select the Nth, log, reach SSTable/Manifest/ops |
| `FlushFaultPoint` (5) / `CompactionFaultPoint` (6) via `install_*_hook(Box<dyn Fn>)` | `src/lsm/mod.rs:288-420, 1735, 1778` | flush and compaction step boundaries, in-process only | cross a process boundary |
| `RestorePhase` (10) via `RestoreOptions.hook` | `src/ops/restore.rs:46-85` | restore phases, error-returning closure | cross a process boundary |
| `startup_probe::hit` (15 points, feature `startup-probe`, env-driven pause) | `src/startup_probe.rs` | startup/recovery checkpoints; the precedent for env control | do anything but pause |
| `DirFsyncHook` (thread-local, `cfg(test)`) | `src/wal/file_io.rs:384-412` | `fsync_dir` errors in unit tests only | build/feature-gate for integration tests |
| `GroupCommitter::fsync_fault_hook` (`cfg(any(test, test-util))`) | `src/wal/group_commit.rs:701, 1524-1536` | the leader's `sync_all` | any other site |
| `WalFile` / `FaultInjectingIo` | `src/wal/file_io.rs:166`, `src/wal/testing.rs` | WAL segment byte I/O | everything that is not a WAL segment byte |

**Boundaries the mission requires and nothing reaches today** `[R]` (P10): temp-file rename (WAL segment, SSTable, `DATA_FORMAT`, backup, restore), parent-directory sync as its own point, SSTable tmp write / tmp sync, Manifest write / sync / replace as separate points, obsolete-file deletion as a point, backup-metadata publication, and a unified way to select "the Nth hit" or "with probability p, seeded" and to read back which point fired.
Nothing can be driven from a child process except `startup_probe` (pause only).

## 2. Decision

Add **one** registry, `src/faults.rs`, compiled **only** with a new cargo feature `fault-points`. Every persistence boundary gets one `faults::fire(Point::X)` call. Rules (which point, when, what happens) come from the environment (child process) or from a programmatic API (in-process test). Every firing is written to an execution log.

Not decided here (deliberately): the persistence abstraction (`Persistence` trait, D1b). This registry sits at call sites and does **not** intercept bytes; it cannot model unsynced-write loss, torn sectors or lost directory entries. That is why every result obtained through it is **class A or class B only** (contract §7).

### 2.1 Why a single registry rather than the scattered hooks

1. One naming scheme and one place to ask "is every mandatory boundary covered?" — the coverage test can enumerate `Point::ALL` against a trace (§6.3) instead of grepping five hook families.
2. One firing grammar (at-once / at-Nth / seeded-probabilistic) and one log, instead of per-hook ad hoc behaviour.
3. Works across a process boundary for all of WAL, SSTable, Manifest, flush, compaction, backup and restore (today only `startup_probe` does, and only to pause).
4. One feature gate whose absence from product builds is mechanically checkable (§4).

**What it does not do:** it does not remove or rewrite the existing hooks. Their tests (`tests/crash_consistency.rs`, `tests/group_commit/*`, `src/lsm/tests.rs`, `src/ops/integration_tests.rs`, F-11) keep passing unchanged; "never rewrite a failing/passing regression test" applies. The registry is *additional*. `fire_flush_fault_hook`, `fire_compaction_fault_hook`, `fire_abort_hook`, `RestorePhase` and `startup_probe::hit` call sites are reused as anchors: the registry call is placed next to each, with an identical position, so the two cannot drift. Retiring the legacy hooks is recorded as an OPEN_ITEM, not done in D1a.

### 2.2 Naming convention (so a "kill at X" has one meaning)

A point named `X` fires **immediately before** operation `X` is issued. Killing at `X` therefore leaves the state "everything before `X` done, `X` not issued". The state "after `X`" is the point of the next operation in the same sequence, so there is never a duplicate `X.done` unless there is no following operation (those are named `…published` / `…done` and listed as such). This makes the point list below an ordered sequence per procedure and makes "what did the crash leave behind?" a lookup, not a judgement.

An **Error** action at `X` means: operation `X` is not performed and `fire` returns `Err(io::Error)` to the caller, which then runs its existing failure path (class A).

## 3. The points and their exact call sites

"Shape": **I** = insert one `faults::fire(...)` statement; **C** = the statement sits inside an existing `.and_then` chain and the chain gains one link; **W** = a one-line wrapper replacing a `write_all` (§3.1); **F** = infallible site (`let _ =` deletion or a function that cannot return `Err`): supports Abort/Pause/Log/Delay, **not** Error. Line numbers are HEAD (`dde8ae9`) and will move. Protected = ✔.

### 3.1 Mandatory points

| Point | Site | Shape | Protected | Notes |
|---|---|---|---|---|
| **WAL record write** `wal.record_write` | `src/wal/mod.rs:985` before `self.active.append(&frame)?` (and the run path at `:885`) | I | ✔ | existing `AbortPoint::MidAppend` is *after* the append; `wal.record_written` is kept at `:988` as the "frame written, not synced" state |
| `wal.record_written` | `src/wal/mod.rs:988` (next to `MidAppend`) | I (F) | ✔ | |
| **WAL flush** | — | — | — | **There is no separate flush.** No userspace buffer exists: `write_all_at` goes to the OS (`file_io.rs:91-117`) `[R]`. "Flush" is the boundary between `wal.record_written` and `wal.fsync`. Recorded as an alias, not a fabricated point. |
| **WAL fsync, leader** `wal.fsync` | `src/wal/group_commit.rs:1535` in `do_leader_fsync`, before `file.sync_all()` | I | ✔ | the call the existing `FaultInjectingIo` cannot reach. **Error here ⇒ committer poisons permanently** (contract §4). Fires once per batch, not per record. |
| `wal.fsync_done` | same function, after `Ok` | I | ✔ | = state "synced, watermark not yet published" (neighbours: `AbortPoint::AfterSync`, `AfterWatermarkBeforeWake`) |
| WAL fsync, `FileWal::sync` (Immediate / seal path) `wal.sync` | `src/wal/mod.rs:998` before `self.active.sync()?` (beside `BeforeSync`) | I | ✔ | used by rotation seal (`:1020`) |
| WAL segment create: `wal.segment_tmp_write` | `src/wal/mod.rs:356` before the header `write_all` | I | ✔ | |
| `wal.segment_tmp_sync` | `src/wal/mod.rs:357` inside the `.and_then` chain | C | ✔ | |
| `wal.segment_rename` | `src/wal/mod.rs:381` before `fs::rename` | I | ✔ | |
| `wal.segment_dir_sync` | `src/wal/mod.rs:403` before `fsync_dir` | I | ✔ | no-op on Windows (D0 A5); the kill still leaves the post-rename state |
| `wal.segment_published` | `src/wal/mod.rs:416` beside `AfterHeader` | I (F) | ✔ | |
| WAL purge `wal.purge_unlink` | `src/wal/mod.rs:1071` before each `remove_file` | I | ✔ | Nth selects which segment |
| `wal.purge_dir_sync` | `src/wal/mod.rs:1093` before `fsync_dir` | I | ✔ | |
| WAL tail truncate (recovery) `wal.tail_truncate` | `src/wal/mod.rs:578` before `set_len` | I | ✔ | repeated-crash-during-recovery |
| **SSTable tmp write** `sstable.tmp_write` | `src/sstable/writer.rs:253` (block), `:201` (bloom), `:207` (index), `:222` (footer) | I ×4 | ✔ | one name, fired per `write_all`; `nth=k` kills mid-file at a chosen write. A kill **between** two `write_all` leaves a prefix; it does **not** model a torn single write (class C) |
| **SSTable tmp sync** `sstable.tmp_sync` | `src/sstable/writer.rs:224` before `file.sync_all()` | I | ✔ | |
| **SSTable rename** `sstable.rename` | `src/sstable/writer.rs:227` before `fs::rename` | I | ✔ | publication point for both flush and compaction output |
| **SSTable parent-directory sync** `sstable.dir_sync` | `src/sstable/writer.rs:231` before `fsync_dir` | I | ✔ | no-op on Windows |
| **Manifest write** `manifest.write` | `src/manifest/mod.rs:116` before `self.file.write_all(&frame)?` | **W** (see §3.2) | ✔ | |
| **Manifest sync** `manifest.sync` | `src/manifest/mod.rs:117` before `self.file.sync_all()?` | I | ✔ | |
| `manifest.synced` | `src/manifest/mod.rs:118` after `sync_all`, before `record_count += 1` | I (F) | ✔ | edit durable, in-memory state not yet updated |
| Manifest tail truncate `manifest.tail_truncate` | `src/manifest/mod.rs:97` before `file.set_len(...)` | I | ✔ | the truncation is not synced (D0 3.6) — this point is how its idempotence is tested |
| **Manifest replace** `manifest.replace` | **no code path exists** | — | — | **Declared, UNREACHABLE.** `Manifest` is append-only; there is no rewrite-and-rename (`manifest/mod.rs`, D0 §8) `[R]`. The point is registered with status `unreachable` so the coverage test reports it explicitly as NOT FIREABLE; D2 (manifest versioning) is where a replace path first appears and the call site is added with it. **Approval item AD-2.** |
| **Compaction output publication** | `sstable.tmp_sync/rename/dir_sync` with scope `compaction` (§5.4) **and** `compaction.before_manifest_add` / `compaction.after_manifest_add` | I | `lsm` (product) | no separate "compaction rename" exists: compaction reuses the SSTable writer `[R]`. The existing `CompactionFaultPoint::BeforeManifestAdd/AfterManifestAdd` are the anchors (`lsm/mod.rs:3011, 3026`) |
| **Obsolete-file deletion** `compaction.obsolete_unlink` | `lsm/mod.rs:3072` before `remove_file(input.path())` | I (F) | product | per-input; the unlink is `let _ =` so Error is unsupported here |
| Recovery sweeps `lsm.sweep_tmp_unlink`, `lsm.sweep_orphan_unlink` | `lsm/mod.rs:2714`, `:2731` before `remove_file` | I | product | these deletes *are* `?`-propagated so Error is supported |
| Flush: `flush.before_sstable_write`, `flush.after_sstable_publish`, `flush.after_rotate`, `flush.after_checkpoint_marker`, `flush.after_set_checkpoint` | `lsm/mod.rs:3347, 3374, 3400, 3412, 3429` beside the matching `FlushFaultPoint` | I ×5 | product | |
| Compaction: `compaction.before_output_write`, `compaction.during_remove_sequence`, `compaction.after_all_removes`, `compaction.before_physical_delete` | `lsm/mod.rs:2969, 3035, 3040, 3060` beside the matching `CompactionFaultPoint` | I ×4 | product | `during_remove_sequence` fires once per input REMOVE edit, so `nth=k` selects which input |
| **Backup metadata publication** `backup.partial_synced`, `backup.publish`, `backup.published` | `src/ops/backup.rs:469` (after the partial's `sync_all`), `:305` before `hard_link`, `:307` after `Ok` | I ×3 | product | `backup.publish` is the point where `dest` becomes visible; D0 G10: no parent-dir sync follows |
| **Restore publication** `restore.promote`, `restore.promoted` | `src/ops/restore.rs:188` before `rename(&staging, dest)`, `:192` beside `RestorePhase::Promoted` | I ×2 | product | |
| Restore phases (other 8) | the single function `at()` at `src/ops/restore.rs:82`: one `fire` keyed by `RestorePhase` | I ×1 | product | covers `Verified … EngineClosed`, `BatchApplied(n)` without eight call sites |
| `DATA_FORMAT` stamp `format.stamp_rename` | `src/ops/format.rs:255` before `rename(&tmp, &marker)` | I | product | listed in P10 as missing |
| **Startup-recovery checkpoints** (named, not random) `startup.<name>` ×15 | the 15 existing `startup_probe::hit` call sites are *not edited*; `startup_probe::hit` itself forwards to the registry (`src/startup_probe.rs`, not protected) | 0 new call sites | — | the 15 names are the contract: `guard_wal_preflight_record … host_ready_sent`. `wal.tail_truncate`, `manifest.tail_truncate`, `lsm.sweep_*` above add the recovery boundaries `startup_probe` lacks |

**New lines in protected paths:** `src/wal/mod.rs` 12, `src/wal/group_commit.rs` 2, `src/manifest/mod.rs` 4 (one is a wrapper), `src/sstable/writer.rs` 7. **Total 25 statements, all expanding to nothing without the feature.** No branch, ordering, error mapping, lock scope or fsync call in those files is moved or removed. Product layer: `src/lsm/mod.rs` 16 (14 points + 2 scope guards), `src/ops/` 7.

### 3.2 The one non-trivial site: `manifest.write` (approval item AD-3)

`Manifest::append_sync` has no rollback of a failed or short write (F-D0-3). To settle F-D0-3 empirically at the *real* call site, the registry must be able to perform a **short write**: write the first `n` bytes of the frame to the file, then return `Err`. A `fire()` placed before `write_all` cannot do that. Therefore `manifest/mod.rs:116` becomes

```rust
faults::write_all(faults::Point::ManifestWrite, &mut self.file, &frame)?;   // feature off: exactly `self.file.write_all(&frame)`
```

where `faults::write_all` is `#[inline(always)] fn write_all(_p, f: &mut File, b: &[u8]) -> io::Result<()> { f.write_all(b) }` without the feature. This is the only call site that changes an existing statement instead of adding one. Alternative if you refuse AD-3: `manifest.write` supports Abort/Pause/Error only (no `shortwrite`), and F-D0-3 stays at its D0 status (consequence measured, reachability `[U]`) until D1b's simulator can tear the write. Either is acceptable to D1a's exit criteria; the first gives strictly more evidence.

### 3.3 Points that exist in code but cannot fire an Error

Sites marked **F** (`wal.record_written`, `wal.segment_published`, `manifest.synced`, `compaction.obsolete_unlink`, and the `*.after_*` flush/compaction points) are boundaries where the caller has nothing to return an error from or where an error would invent a failure the real code cannot have. They support Abort, Pause, Delay, Log. The registry rejects an Error rule on an F point at **parse time** (named in the log) rather than silently ignoring it.

## 4. Why the feature is absent from product builds (precedent: `src/startup_probe.rs`)

* Cargo: `fault-points = []` in `Cargo.toml`, next to `startup-probe = []`, with the same comment. Not in `default`. No workspace member lists it in `[dependencies]` or `[dev-dependencies]`. The root package's tests/examples that need it carry `required-features = ["fault-points"]`, so a plain `cargo test --workspace` compiles none of them and the D0 baseline counts (2,168 / 2 / 32) cannot change.
* Source: without the feature, `faults::fire` and `faults::write_all` are `#[inline(always)]` functions returning `Ok(())` / forwarding to `write_all`; the whole `imp` module (env parsing, `OnceLock`, log file, atomics, `process::abort`) is `#[cfg(feature = "fault-points")]` and **not compiled**. Same structure as `startup_probe::hit` `[R]` (`src/startup_probe.rs:90-96`).
* Evidence that must exist before D1a is reported (these are exit-criteria items, not claims):
  1. `cargo tree -e features -p rubixdb-cli -p rubixdb-instance -p rubixdb-api -p rubixdb-sql` shows no `rubixdb feature "fault-points"`.
  2. A new test (`tests/fault_points_absent_from_product.rs`, not feature-gated) asserts that no `Cargo.toml` in the workspace names `fault-points` outside `[features]` of the root and the root's `required-features` lines. It does **not** touch `tests/repo_hygiene.rs`.
  3. The release `rubixdb` binary built without the feature contains none of the env names (`RUBIXDB_FAULT_SPEC`, `RUBIXDB_FAULT_LOG`, `RUBIXDB_FAULT_TRACE`) — byte search of the produced `.exe`.
  4. Before/after size and hash of the release engine library and CLI binary on the same toolchain, reported as measured (byte equality is *not* promised: embedded paths/LTO may differ; if they differ the difference is reported, not explained away).
* Known hazard, stated up front: Cargo unifies features across a single invocation. `cargo test --workspace --features rubixdb/fault-points` would enable the registry for every crate in that build. With the feature **on but no rule set** the registry is inert: `fire` does one relaxed atomic load (`ARMED`) and returns `Ok`. Anyone building a shipping binary this way is outside the product build rule above, and (3) detects it.

## 5. Control interface (child-process and in-process)

### 5.1 Environment (precedent: `RUBIXDB_TEST_STARTUP_PAUSE*`)

| Variable | Meaning |
|---|---|
| `RUBIXDB_FAULT_SPEC` | `;`-separated rules `point[@scope]=trigger:action`. Parsed once (`OnceLock`) at first `fire`. An unknown point name, an Error rule on an F point, or `shortwrite` on any point except `manifest.write` is a **fatal parse error**: the process writes the reason to stderr and exits non-zero. A rule that silently never fires would make a "passed" kill test meaningless. |
| `RUBIXDB_FAULT_LOG` | path; each firing appended and `sync_all`'d **before** its action runs, so the record survives `abort` and an external kill |
| `RUBIXDB_FAULT_TRACE` | `1` = additionally log every *hit* (not just firings) with a running per-point count; used to enumerate reachable points and choose N, and by the coverage test |
| `RUBIXDB_FAULT_READY` | path; written+synced when the action is `pause`, so a parent can wait for "point reached" and then kill (precedent: `RUBIXDB_TEST_STARTUP_PROBE_FILE`) |

### 5.2 Rule grammar

```
rule     := point [ "@" scope ] "=" trigger ":" action
point    := name from Point::ALL            e.g. manifest.write
scope    := "flush" | "compaction" | "recovery" | "foreground"   (optional; §5.4)
trigger  := "once" | "nth=" N | "prob=" P ",seed=" S              (N>=1, 0<P<=1, S u64)
action   := "abort" | "pause" [ "=" ms ] | "error=" kind | "shortwrite=" n | "delay=" ms | "log"
```

### 5.3 Firing semantics

* **at-once** (`once`): fires on the first hit of the point, then that rule is disarmed. Equivalent to `nth=1`.
* **at-Nth** (`nth=N`): fires on exactly the N-th hit of the point (counted per point per process, 1-based, atomic), then disarms. This is how "the 3rd SSTable block write" or "the 2nd compaction input unlink" is selected.
* **seeded-probabilistic** (`prob=P,seed=S`): hit number *h* of the point fires iff `splitmix64(S ^ hash64(point_name) ^ h) / 2^64 < P`. A pure function of `(seed, point, h)`: the same seed reproduces the same firing hit-indices on every run. **Stated limit:** which thread receives hit *h* is not controlled, so determinism of *what is on disk* needs a single-writer driver; multi-writer runs are reproducible per hit-index, not per interleaving. The rule does not disarm: it may fire repeatedly (only `abort`/`pause` end the process).
* **Actions.** `abort`: `std::process::abort()` after the log is synced (self-delivered process death at a chosen instruction; no destructor, no flush, the OS keeps its page cache — **class B**). `pause[=ms]`: log, write `RUBIXDB_FAULT_READY`, park (forever, or ms); the parent then kills the child with `Child::kill()` (`TerminateProcess` on Windows, `SIGKILL` on Unix) — the true external kill -9 at a deterministic instruction (**class B**). `error=<kind>`: `fire` returns `Err(io::Error::from(kind))` (kinds: `Other`, `StorageFull`, `PermissionDenied`, `WriteZero`, `Interrupted`, `UnexpectedEof`; `raw=28/112` for ENOSPC) — **class A**. `shortwrite=n`: `faults::write_all` writes `min(n, len-1)` bytes, then returns `Err(WriteZero)`. `delay=ms`: sleep then continue (widens races). `log`: record and continue.
* No action ever alters, skips or fakes the surrounding work; the state on disk at the point is exactly what the real code leaves at that instruction (same promise as `startup_probe`).

### 5.4 Scope

SSTable writer points are shared by flush and compaction. Two guard lines in `lsm/mod.rs` (flush thread entry, `compact_once_impl` entry) set a thread-local scope label for the duration of the call; a rule with `@compaction` matches only hits made under that label. Without `@scope` a rule matches every scope. The scope is recorded in every log line, so "the rename that was a compaction output" is readable from the log rather than inferred.

### 5.5 Execution log line

```
<fire_no> <unix_ms> <point> <scope> hit=<n> trigger=<…> action=<…> thread=<name>
```

`fire_no` is a process-wide counter. In-process tests get the same records through `faults::take_log() -> Vec<Firing>`. Tests assert on the log (that the **named point** fired, how many times) *and* on the recovered state; a kill test whose log does not contain its point is reported FAIL ("point not reached"), never PASS.

### 5.6 In-process API

`faults::install(rules)` returns a guard that clears rules and counters on drop and takes a process-wide mutex, so tests that arm the registry serialize. (The registry is process-global by necessity: the call sites have no handle to thread a rule set through. Parallel in-process tests that arm it are not supported and the guard enforces that.)

## 6. Kill-9 rig and the independent recovery oracle (D1a deliverables this ADR also scopes)

Not protected-path changes; recorded here so the approval covers the whole slice.

### 6.1 Rig

* `examples/fault_child.rs` (`required-features = ["fault-points"]`): opens an engine on a directory given in the environment with the **product** configuration (GroupCommit 5 ms / 256 KiB; the same constructor `ops/open.rs` uses), runs a deterministic seeded workload, and writes the **ledger** (§6.2). It contains no recovery logic.
* `tests/fault_points/` (`required-features`): a parent that, for each point and each mode (`abort`; `pause` + external kill), spawns the child on a fresh temp dir, waits for exit/ready, **requires the log to name the point**, then runs recovery and the oracle in a *fresh* process-state (the engine reopened by the parent).
* Workloads sized so each mandatory point is reached: a small MemTable and small `max_segment_size` force flush, rotation, purge and compaction; one workload for backup/restore points. A point that no workload reaches is reported NOT REACHED with its reason and is not counted as passed.

### 6.2 Oracle (new: `tests/support/oracle.rs`, P11)

Independent of the engine under test: it does not call engine recovery internals and does not use the engine's own state to decide the expected state.

* **Ledger.** The child appends, outside the data directory, one line **before** each commit (`S <i> <seed-derived op summary>`) and one **after** each `Ok` (`A <i> <seq>`). Parent reconstructs the full operation list from the same seed (not from the child), so the ledger only carries *what was acknowledged*. A kill between the `Ok` and the ledger write makes that unit "submitted, ack unknown" — the conservative direction.
* **Model.** A `BTreeMap<Vec<u8>, Vec<u8>>` built by applying operations in order. Expected recovered state is `model(K)` for some `K` with `acked <= K <= submitted` (R3 prefix: the in-flight/unacknowledged units may be wholly present or wholly absent, C-ACK-3).
* **Comparison.** Full keyspace scan of the recovered engine versus `model(K)`: every key and every value, plus absence of extra keys. Row counts are never the check. A mismatch prints the first differing key/value and `K`.
* **Checks per run** (each labelled with the invariant it tests): R1/R2/R3/R4/R12 (state equals `model(K)` for a K in range); R5 (no key shows a value from a commit with a different seq); **R6** (a post-recovery write is acknowledged with a seq greater than every acknowledged pre-crash seq and than the manifest checkpoint — the F-D0-5 invariant, reported as measured either way); R7/R8 (`ops::check` on the recovered directory, and every live SSTable listed by the manifest exists); R10 (reopen a second time: byte-identical dump; plus a kill *during* recovery at each `startup.*` point followed by a clean recovery gives the same state); R11 (orphan/tmp files present after recovery are listed and none prevents open).
* **Relational variant.** For the SQL layer the same ledger/model idea over decoded rows + their index entries (the model decodes through the public SQL API into `(table, pk) → row` and `(index, key) → pk`, and the oracle checks the two maps against each other and against the recovered storage). This depends on the `sql` crate's `test-util` seam used by `sql/tests/write_crash_consistency.rs`; D1a will read that seam first and, if the relational comparison cannot be built without editing SQL production code, it is reported **OPEN** (not approximated by row counts). **Approval item AD-6.**
* **The oracle must be able to fail.** Self-tests (deliberate-failure checks): the oracle is run against (a) a recovered directory with one acknowledged key removed, (b) one value altered, (c) one extra key added, (d) a commit applied out of order, (e) a post-recovery seq forced below the checkpoint — each must be reported as a violation of the named invariant. A passing kill matrix is only believable if these fail.

### 6.3 Coverage test

With `RUBIXDB_FAULT_TRACE=1`, a probe workload per procedure records every hit; the test asserts `Point::ALL` minus the declared-unreachable set is a subset of the union of hits, and prints the unreachable ones. This makes "every mandatory point fires" a test, not an assertion in prose.

## 7. Class labelling (mission rule, restated as a hard rule for this slice)

* Every result is tagged `class A` (error returned, process alive) or `class B — process kill`. `docs/CRASH_CLASS_MATRIX.md` is created with a class-B section populated by D1a, and **empty** class-C and class-D sections whose header says "awaiting D1b/D1c". No class B row is copied there.
* The report will state: **kill-9 is not power-loss evidence.** A kill at `wal.fsync` leaves the written-but-unsynced frame in the OS cache, so the next process sees it; this proves recovery logic and ordering of logical steps, not that an fsync was needed. Removing a `sync_all` is invisible to every test in this slice by construction; D1a does not claim otherwise.
* Windows is the only platform executed. Unix `fsync_dir` is real in the source but is `[R]`-only; no Unix result will be reported.

## 8. Consequences

* **Positive:** every mandatory boundary is reachable and named; kill and error tests share one grammar; the D1c class-C work reuses the same point names (the simulator will fire at the same instants and also discard unsynced state); the oracle is reusable by D1c–D7.
* **Cost:** 25 protected-path statements (no behaviour change without the feature), one wrapper site (AD-3). More `Point` names to maintain; the coverage test makes drift visible.
* **Risks accepted:** (1) feature-on, rule-armed code paths exist only in test builds, so they are test-support code that can itself be wrong — mitigated by the log requirement and the oracle self-tests; (2) a `pause`/`abort` inside a group-commit leader holds engine locks — acceptable, the process is about to die; (3) in-process arming is process-global.
* **Not addressed:** unsynced-write loss, torn sectors, reordering, directory-entry loss, fsync lies (D1b/D1c, contract A1–A5), real power loss (Tier 3/4 — not available).

## 9. Alternatives rejected

1. **Extend `AbortPoint`/`set_abort_hook`.** One process-wide `fn`, first caller wins, no state; adding an Nth counter, a log and an action set to a `fn(AbortPoint)` means replacing it — i.e. this ADR — while keeping its WAL-only reach and the `test-util` feature's wider meaning.
2. **Put the points behind the D1b `Persistence` trait instead.** Correct for class C, but D1b is gated on D1a passing and on the F-D0 decisions, and it is the harder, unproven change (P9, ~65 sites). A registry at call sites is the smallest thing that gets class A/B evidence now, and its point names carry over.
3. **External-only kill (parent `kill` at a random time).** Not deterministic; cannot say *which* boundary was hit — exactly what `startup_probe` was created to fix. Kept only as the second step of `pause` mode.
4. **`#[cfg(test)]`-only hooks.** Unreachable from integration tests and child processes (separate crate compilation).
5. **A runtime flag in product builds.** Would put a fault injector, an `abort()` and env parsing in the shipping binary; rejected by the project's Security-Always-On rule.

## 10. Items needing a maintainer decision before any call site is added

| ID | Decision | My recommendation |
|---|---|---|
| **AD-1** | Approve this ADR and the 25 protected-path statements in §3.1 (list is exhaustive; no other edit to `src/wal`, `src/manifest`, `src/sstable`, `src/compaction`, `src/error.rs`) | approve |
| **AD-2** | `manifest.replace` has no code path; accept "declared, UNREACHABLE until D2" rather than inventing a rewrite path | accept |
| **AD-3** | Allow the single wrapper `faults::write_all` at `manifest/mod.rs:116` so a short write can be injected (§3.2) | approve (gives F-D0-3 real evidence) |
| **AD-4** | Legacy hooks stay in place and untouched in D1a; retirement logged to `OPEN_ITEMS.md` | accept |
| **AD-5** | "WAL flush" is the alias described in §3.1 (no separate flush exists) | accept |
| **AD-6** | Relational oracle variant is built if the existing `sql` test seam allows it; otherwise reported OPEN, not approximated | accept |
| **AD-7** | ADR path `docs/adr/` (per the mission) although earlier ADRs live at the repo root as `PHASE_*_ADR.md` | accept |

## 11. Out of scope for D1a (so nobody reads it in)

The `Persistence` trait and simulator (D1b); resolving F-D0-1/2/3/5 (maintainer decision gate after D1a); manifest header (D2); anything about backup format, encryption or PITR; Linux/macOS execution; real power cut.


---

## 12. Amendments A1–A7 (2026-10-13, written before any call site was added)

### 12.1 Point | Status | Reason table (A6) — machine-read

The coverage test (`tests/fault_points/coverage.rs`) parses **only** the rows between the two marker comments below. Status vocabulary: `Reachable` (the test fails if the point never fires in its named workload), `Unreachable` (the test fails if the point **does** fire), `Alias` (not a registry point; the reason must name an existing registry point in backticks, which the test checks exists). A point in `Point::ALL` that is missing from this table, or a row naming a point that does not exist, fails the test. Statuses are claims; the test is what turns them into evidence. Workload codes in the reason: `W` = engine workload (flush/rotation/compaction), `B` = bare-WAL workload (Immediate-mode `FileWal`), `T` = torn-tail fixture then reopen, `O` = orphan/stale-tmp fixture then reopen, `OPS` = backup/restore/format calls on a disposable instance directory.

<!-- POINT-STATUS-BEGIN -->
| Point | Status | Reason |
|---|---|---|
| `wal.record_write` | Reachable | W: every commit unit |
| `wal.record_written` | Reachable | W |
| `wal.fsync` | Reachable | W: group-commit leader `sync_all` (`group_commit.rs`) |
| `wal.fsync_done` | Reachable | W |
| `wal.sync` | Reachable | B: `FileWal::sync` is the Immediate-mode path; the product (GroupCommit) leader bypasses it, so W alone may not reach it |
| `wal.segment_tmp_write` | Reachable | W: segment creation at open and at rotation |
| `wal.segment_tmp_sync` | Reachable | W |
| `wal.segment_rename` | Reachable | W |
| `wal.segment_dir_sync` | Reachable | W: the call is made on Windows too; the sync itself is a no-op there (D0 A5) |
| `wal.segment_published` | Reachable | W |
| `wal.purge_unlink` | Reachable | W: purge after flush |
| `wal.purge_dir_sync` | Reachable | W |
| `wal.tail_truncate` | Reachable | T: only runs when recovery finds a torn WAL tail |
| `sstable.tmp_write` | Reachable | W: per `write_all` into the tmp file |
| `sstable.tmp_sync` | Reachable | W |
| `sstable.rename` | Reachable | W |
| `sstable.dir_sync` | Reachable | W |
| `manifest.write` | Reachable | W |
| `manifest.sync` | Reachable | W |
| `manifest.synced` | Reachable | W |
| `manifest.tail_truncate` | Reachable | T: only runs when recovery finds a torn MANIFEST tail |
| `manifest.replace` | Unreachable | no rewrite-and-rename path exists in `src/manifest/` (append-only); a call site is added with D2 |
| `flush.before_sstable_write` | Reachable | W |
| `flush.after_sstable_publish` | Reachable | W |
| `flush.after_rotate` | Reachable | W |
| `flush.after_checkpoint_marker` | Reachable | W |
| `flush.after_set_checkpoint` | Reachable | W |
| `compaction.before_output_write` | Reachable | W (auto-trigger on) |
| `compaction.before_manifest_add` | Reachable | W |
| `compaction.after_manifest_add` | Reachable | W |
| `compaction.during_remove_sequence` | Reachable | W: once per input REMOVE edit |
| `compaction.after_all_removes` | Reachable | W |
| `compaction.before_physical_delete` | Reachable | W |
| `compaction.obsolete_unlink` | Reachable | W: per input whose `Arc` count is 1 |
| `lsm.sweep_tmp_unlink` | Reachable | O: stale `*.sst.tmp` |
| `lsm.sweep_orphan_unlink` | Reachable | O: a table that was added then removed but not unlinked |
| `lsm.before_apply` | Reachable | W: every commit unit, between its WAL acknowledgement and its MemTable apply (added by the D1a decision gate as the deterministic F-D1a-1 seam; 2 statements in `src/lsm/mod.rs`, behaviour-neutral) |
| `backup.partial_synced` | Reachable | OPS |
| `backup.publish` | Reachable | OPS |
| `backup.published` | Reachable | OPS |
| `restore.promote` | Reachable | OPS |
| `restore.verified` | Reachable | OPS: `RestorePhase::Verified` via `at()` |
| `restore.staging_created` | Reachable | OPS |
| `restore.engine_opened` | Reachable | OPS |
| `restore.batch_applied` | Reachable | OPS: once per batch |
| `restore.loaded` | Reachable | OPS |
| `restore.digest_verified` | Reachable | OPS |
| `restore.check_passed` | Reachable | OPS |
| `restore.engine_closed` | Reachable | OPS |
| `restore.promoted` | Reachable | OPS: emitted at the `Promoted` site |
| `restore.marker_removed` | Reachable | OPS |
| `format.stamp_rename` | Reachable | OPS: fresh-directory stamp |
| `startup.guard_wal_preflight_record` | Reachable | OPS: product guard preflight, in-process |
| `startup.quarantine_temp_written` | Reachable | T+OPS: needs a torn tail under the product tail policy |
| `startup.quarantine_renamed` | Reachable | T+OPS |
| `startup.quarantine_tmp_swept` | Reachable | T+OPS: needs a stale `*.tail.tmp` |
| `startup.attestation_consumed` | Reachable | T+OPS |
| `startup.engine_wal_replay_record` | Reachable | W: once per replayed record |
| `startup.engine_wal_replayed` | Reachable | W |
| `startup.engine_wal_opened` | Reachable | W |
| `startup.manifest_opened` | Reachable | W |
| `startup.reconcile_tmp_swept` | Reachable | O |
| `startup.reconcile_before_adopt` | Reachable | O: a valid table unknown to the Manifest |
| `startup.reconcile_after_adopt` | Reachable | O |
| `startup.engine_opened` | Reachable | W |
| `startup.format_marker_temp_written` | Reachable | OPS |
| `startup.host_ready_sent` | Unreachable | fires only in the `rubixdb-cli` host process; D1a runs the engine and `ops` in-process and does not start the host. Covered, as class B pause only, by the F-11 suite (`startup-probe`). |
| `wal.flush` | Alias | there is no separate WAL flush; see `wal.record_written` |
| `compaction.output_publication` | Alias | compaction reuses the SSTable writer: `sstable.rename` with scope `compaction`, then `compaction.before_manifest_add` |
| `backup.metadata_publication` | Alias | the backup file is the metadata; see `backup.publish` |
| `obsolete_file_deletion` | Alias | `compaction.obsolete_unlink`, `wal.purge_unlink`, `lsm.sweep_orphan_unlink` |
<!-- POINT-STATUS-END -->

Where the first run of the coverage test shows a claim above is wrong, the row is **corrected in this table with the measured reason** (never by weakening the test) and the correction is listed in the D1a report.

### 12.2 A1 — byte-equivalence of `faults::write_all` (AD-3)

`tests/faults_write_all_equivalence.rs`, **not** feature-gated, so the default `cargo test` (the production build) runs it. It (1) opens a `Manifest` in a temp dir, calls `append_sync` for one `AddSstable`, one `RemoveSstable` and one `SetCheckpoint` edit, and byte-compares the resulting `MANIFEST` with a frame sequence **encoded by hand in the test** from the layout in `docs/PERSISTENCE_MAP.md` §8 (`body_len:u32 LE | crc32c(body):u32 LE | body`, `edit_type:u8`, fields `u64 LE`), using the `crc32c` crate directly and none of `manifest::format`; (2) calls `faults::write_all` and `std::io::Write::write_all` on the same kinds of failing handle (read-only file) and asserts identical `ErrorKind` and identical bytes written (none), and on a good handle asserts identical bytes; (3) reopens the file with `manifest::replay_readonly` and requires the three edits back. With `--features fault-points` and no rule armed the same file must pass unchanged (the registry is inert).

### 12.3 A2 — scope-label integrity (§5.4)

The coverage test asserts that **every** `sstable.*` hit recorded under `RUBIXDB_FAULT_TRACE` carries a non-empty scope (`flush` or `compaction`); a hit with an empty scope is a test failure. Scope is thread-local: `write_from_sorted_records` is single-threaded today (it runs on the calling flush or compaction thread; no `thread::spawn` or pool use exists in `src/sstable/` `[R]`). **If the SSTable writer ever spawns or hands work to helper threads, thread-local scope matching breaks silently (hits would be logged with an empty scope) and this section, the scope design and the coverage assertion must be re-examined before that change merges.**

### 12.4 A3 — rig timeouts (§6.1)

* Parent waits for `RUBIXDB_FAULT_READY` (pause mode) or for child exit (abort mode) at most **60 s** (debug build, workloads sized to finish well inside this). Expiry ⇒ the child is killed, the case is recorded **POINT NOT REACHED (timeout)** and the test **fails**.
* After `Child::kill()` the parent waits at most **10 s** for the child to exit. Expiry ⇒ **KILL TIMEOUT**, the test fails, the temp directory is left in place and its path printed.
* A child that exits (any status) without a log line naming the requested point ⇒ **POINT NOT REACHED**, fail. A child that exits normally having never fired the point is likewise not a pass.
* Neither state is ever mapped to PASS, "skipped" or "ignored". The only way a case is reported PASS is: log names the point, the process stopped there (abort status or killed-while-paused), recovery completed, and the oracle accepted the recovered state.
* Each case uses a fresh `std::env::temp_dir()` subdirectory (never a real database) and removes it on PASS only.

### 12.5 A4 — D1c concurrency plan: choice **(a)**, fixed now

D1c (class C) will run, for **every** reachable point, N = **4** concurrent writers (disjoint key spaces per writer, plus one shared "hot" key written by all so that cross-writer ordering is exercised) under `prob=P,seed=S`, **M = 5** distinct seeds per point; **all** M runs must satisfy the oracle for some per-writer prefix vector K = (K1 … K4) and the global-prefix check (R3 by acknowledged `seq`). Evidence from fewer writers is recorded under the label **"single-writer"** and may not be reported as concurrent. The matrix has a `writers` column; a row without N=4 and M=5 completed is reported as "single-writer evidence only — concurrent not run" regardless of how many single-writer repetitions passed. The honest limit (§5.3) stays: reproducible per hit-index, not per interleaving. D1a's oracle is built multi-writer-capable now (disjoint-key partitioning, per-writer ledgers); D1a records its own concurrent class-B results under the same labelling rule, and a class-B concurrent result is never reused for class C.

### 12.6 A5 — AD-6 resolution rule (relational oracle)

D1a exits with exactly one of: **(a)** the relational oracle (decoded rows + index entries, self-tested with the same five deliberate-failure checks as the engine oracle) is implemented and its results are in the matrix; or **(b)** it is **OPEN** with (i) the exact missing seam (file, function, why it is insufficient), (ii) what would unblock it, (iii) whether the unblocking change lies in a protected path. The D1a report states which, in a dedicated line. Row counts are never substituted. The outcome is appended to this section when D1a closes.

### 12.7 A7 — the feature must not ship (CI guard)

There is no CI service in this repository (`.github/` absent `[R]`); the shipping path is `scripts/release.ps1`, and `cargo clippy --workspace --all-targets --all-features` in that script *does* compile the feature (compile-only; it must stay `-D warnings` clean with the feature on). Enforcement, in three layers:

1. **Workspace test** `tests/fault_points_absent_from_product.rs` (not feature-gated, runs in the default `cargo test --workspace`): (i) no `Cargo.toml` in the workspace names `fault-points` except the root `[features]` entry and `required-features` lines; (ii) no `*.ps1`/`*.sh`/`*.py` script outside an explicit allow-list contains a `cargo build|install|run|rustc` invocation with `fault-points` or `--all-features`; the allow-list is `scripts/release.ps1`'s clippy line only (clippy is not a shipping artefact), **refined when the guard first ran**: a command that names an `--example`, `--test` or `--bench` target builds a rig artefact, not the product binary, and is allowed (the guard had flagged the documented rig build `cargo build --features fault-points --example fault_child` in `scripts/fault_points_matrix.py`); a bare `build`/`install`/`run`/`rustc`/`package`/`publish` with the feature or `--all-features` is still a violation; (iii) `src/faults.rs` reads no environment variable outside `#[cfg(feature = "fault-points")]`.
2. **Release gate** `scripts/release.ps1` gains a step after `cargo build --release --locked -p rubixdb-cli`: byte-search the produced `rubixdb.exe` for `RUBIXDB_FAULT_SPEC`, `RUBIXDB_FAULT_LOG`, `RUBIXDB_FAULT_TRACE`, `RUBIXDB_FAULT_READY`; any hit fails the release. (Product-layer script edit, justified by this section.)
3. `cargo deny` is not used for this: deny checks dependencies, and the feature has none. Recorded so nobody believes it is covered there.

### 12.8 Other clarifications recorded while amending

* `ops/` statement count: `restore.promoted` is emitted from the `Promoted` site (`restore.rs:192`) and `at()` maps the **other nine** phases, giving 7 `ops` statements as approved (backup 3, `restore.promote` 1, `restore.promoted` 1, `at()` 1, `format.stamp_rename` 1).
* **Correction found by the coverage test (first run):** `wal.record_written` was specified only beside `AbortPoint::MidAppend` on the single-frame path (`FileWal::append`), which the product never uses: group commit writes through the batched-run path (`FileWal::append_run`, the `match self.active.append(run)` arm). The call was added there too, so `src/wal/mod.rs` carries **13** statements, not the 12 counted in section 3.1; the total is 26. The table row stays `Reachable`, now true for the engine workload.
* F-D0-4 gets its own `OPEN_ITEMS.md` line with the same visibility as F-D0-1/2/3/5 (D1a).

### 12.9 D1a closing record (what was built, what differs from sections 1–11)

**A5 outcome — route (a): the relational oracle is implemented and self-tested.** Scope and limits, exactly:

* It runs the real relational write path below SQL text: `TableStore::put_row` / `delete_row`, each of which is one `write_batch` carrying the row **and** its secondary-index entries (`tests/support/rel_workload.rs`, child modes `rel-setup` / `rel` of `examples/fault_child.rs`). It is judged by (1) the decoded rows of `TableStore::scan_table` against a model regenerated from the seed, and (2) a **raw-keyspace differential** against a reference database that never crashed (setup template + the first K operations through the same API); (2) is what sees a row without its index entry, a dangling index entry, a stale index entry after an update and catalog damage, none of which a decoded-row comparison can see (`relational_oracle_rejects_a_row_without_its_index_entry_that_decoded_rows_cannot_see` proves exactly that: the decoded check stays silent and the differential objects).
* Self-tests that must fail and do: a missing index entry, a dangling index entry, an altered raw value, an extra raw key, a missing raw key, a lost acknowledged row, an out-of-order commit (`tests/fault_points/rel_tests.rs`).
* **Not covered:** SQL parsing/binding/planning (the `rubixdb-sql` crate). Reaching the registry from that crate would require `rubixdb-sql` (or a forwarding feature) to name `fault-points`, which §12.7 forbids; the relational layer itself lives in the root crate, so nothing below SQL text is missing. The reference uses the same engine code as the system under test (differential, not a specification oracle). Single writer only.

**Deviations from sections 1–11, all measured or decided while building:**

1. `wal.record_written` is also fired on the batched-run path (§12.8): `src/wal/mod.rs` carries 13 statements, the protected paths 26 in total (WAL 13, group commit 2, manifest 4 incl. the one wrapper, SSTable writer 7). Product layer: `src/lsm/mod.rs` 16, `src/ops/` 7, as approved.
2. The execution log has two line kinds: `F <fire_no> <unix_ms> <point> <scope|-> hit=<n> trigger=<…> action=<…> thread=<…>` for a firing (written and `sync_all`'d before the action runs) and, with `RUBIXDB_FAULT_TRACE=1`, `H <unix_ms> <point> <scope|-> count=<n> thread=<…>` for every hit.
3. `nth=N` / `once` count the hits **matching the rule's scope filter, per rule** (§5.3 said per point); with no `@scope` this equals the per-point count.
4. `startup_probe::hit` feeds the registry (`startup.<name>`); the 15 existing call sites are untouched. `startup.host_ready_sent` fires only in the `rubixdb-cli` host process and is `Unreachable` for D1a.
5. `error=` is rejected at parse time on points whose call site has no `Result` (marked `F` in `src/faults.rs`); `shortwrite=` only on `manifest.write` (`faults::write_all`, byte-equivalence test `tests/faults_write_all_equivalence.rs`). `manifest.write` `shortwrite=` is implemented and unit-tested (`faults::imp::tests::short_write_writes_a_strict_prefix_then_errors`) but **no kill/error row uses it yet**: settling F-D0-3 with it is left to D1c, after the maintainer's decision on F-D0-3.
6. Kill modes: `abort` (the process aborts itself at the point) and `pause` + the parent's `Child::kill()` (`TerminateProcess`). Hard timeouts per §12.4 (60 s to reach / 10 s to die) are enforced in `tests/fault_points/rig.rs`.
7. Workloads: engine (1 and 4 writers; 8 KiB MemTable, 24 KiB WAL segments, compaction trigger 2, product GroupCommit 5 ms / 256 KiB), bare Immediate-mode `FileWal` (`wal.sync` is only reachable there), recovery on fixtures (`AbortEnd`, `OrphanSst`, `RemovedNotDeleted`, `Attested`, and four SYNTHETIC damage fixtures), empty-directory start, backup + restore, and the relational workload.
8. **A4 stays as decided (choice (a)).** D1a's concurrent column used N = 4 writers and one `prob=` run per point (M = 1), not the M = 5 D1c will require; it is labelled "concurrent, M = 1" and may not be quoted as D1c evidence.

**Finding made by the rig (not by design):** F-D1a-1, a pre-existing loss of acknowledged writes under concurrent writers (`OPEN_ITEMS.md`). The concurrent class B column fails on it; each failing row carries an automatic attribution (acknowledged seq ≤ manifest checkpoint, present in no live SSTable). Single-writer rows are unaffected.
