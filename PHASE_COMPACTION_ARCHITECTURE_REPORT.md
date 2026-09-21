# RubiXDB — Compaction Architecture Report

**Status:** Read-only audit. No `src/` file, `Cargo.toml`, or generated
artifact was modified to produce this document. One throwaway,
standalone Rust program was compiled and run *outside* the project
(scratchpad directory, not part of `Cargo.toml`) to empirically answer
one platform question (§6) rather than guess at it — discarded after
use, no trace left in the repository.

**Date:** 2026-09-21

**Baseline**: Write Engine certified (`PHASE_WRITE_ENGINE_
CERTIFICATION.md`, commit `7d02554`). Read Engine certified
(`PHASE_READ_ENGINE_CERTIFICATION.md`, commit `d94064d`, implementation
`22be3e4`). Both treated as frozen throughout this audit — nothing
below proposes changing either.

---

## 1. Current Compaction implementation state — verified directly, not assumed

```
src/compaction/mod.rs (5 lines, the entire file):

//! Size-tiered, full-merge compaction, per LSM Engine Spec Section 5:
//! triggers at `compaction_trigger_count` live SSTables, enforces the
//! tombstone-safety rule against outstanding `snapshot_refs`.
//!
//! Not yet implemented — Phase 0, Step 6.
```

`src/lib.rs:9` declares `pub mod compaction;` — the module **is**
wired into the crate (so `cargo build`/`cargo test` already compile
it), but it contains **zero types, zero functions, zero tests, zero
configuration**. This is a doc-comment-only placeholder, not a partial
implementation — there is nothing here that must be "preserved" in the
sense of existing behavior to protect; the file itself, however,
already commits to two facts that the rest of this report treats as
authoritative starting points rather than open questions (§9 of the
ADR brief's own strategy question, §18's trigger question):

- **Strategy is already named**: "Size-tiered, full-merge compaction."
- **Trigger config name is already named**: `compaction_trigger_count`.
- **Snapshot-safety mechanism name is already named**: `snapshot_refs`
  (Architecture Spec terminology — mapped to this codebase's actual
  `Snapshot`/`SnapshotRegistry` in §3 below).

**Existing configuration**: `LsmConfig` (`src/lsm/mod.rs:55-84`,
verified by direct read, not assumed) has **no** `compaction_trigger_
count` field today — `memtable_max_size_bytes`, `max_immutable_
memtables`, `sstable_target_block_size`, `bloom_bits_per_key`, `max_
flush_retries`, `storage_pressure_retry_interval`. The original v1
design sketch in `RubixDB-LSM-Engine-Specification-v1.0.md` §4.1 lists
a `compaction_trigger_count: usize` field (default 4) on `LsmConfig` —
**that field was never added** to the real, evolved struct. This is
the first of several places this report found the original spec's
pseudocode has drifted from the real, shipped implementation (expected
and unsurprising — three certified phases of real engineering happened
between the spec being written and today — but confirmed by reading
the actual struct, not assumed from the spec).

**Existing Manifest support — already complete, already tested,
currently unused**: `ManifestEdit::RemoveSstable { id: u64 }`
(`src/manifest/format.rs:26-28`) exists, encodes/decodes, and has its
own dedicated round-trip and corruption tests (`src/manifest/format.rs`
tests, `src/manifest/recovery.rs` tests). `ManifestState::apply`
(`src/manifest/state.rs:77-87`) already implements idempotent
`RemoveSstable` handling (a repeat removal of an already-removed id is
a no-op; a removal of an id never added is `EngineError::Corruption`).
**No code in this codebase currently constructs a `RemoveSstable`
edit** (`grep -rn "RemoveSstable" src/` confirms every non-test hit is
either the `format.rs`/`state.rs` definition itself or a doc comment
explicitly noting "unreachable this phase, since nothing issues
`REMOVE_SSTABLE` without Compaction" — `src/lsm/mod.rs:1740`,
`:1784`). Compaction is the **first** production caller this type will
ever have.

**Existing recovery support — already complete in design, zero test
coverage today**: `reconcile_sstables_with_manifest`
(`src/lsm/mod.rs:1750-1819`) already has a fully-implemented branch for
exactly the scenario Compaction's crash protocol needs: a `.sst` file
on disk whose id is in `state.ever_added` but *not* in `state.live_
sstables` (i.e., a `RemoveSstable` edit was durably written, but the
physical file deletion never happened, or happened partially) is
unlinked at startup (`src/lsm/mod.rs:1781-1785`). The doc comment
literally says this branch is "unreachable this phase (no Compaction
issues `REMOVE_SSTABLE` yet), handled for forward compatibility." This
means: **the original Write Engine authors deliberately built the
recovery-side half of Compaction's crash protocol years before this
phase started**, anticipating it — but because nothing exercises it
today, it has **zero direct test coverage**. §7 below treats this as
"structurally sound by code review, not yet proven by a real test" —
a distinction this report does not blur.

**Existing atomic-construction discipline — already fully reusable**:
`SsTable::open`'s doc comment and `src/sstable/mod.rs::discover`'s own
`.sst.tmp` sweep ("Always an interrupted build — never trusted, swept
unconditionally") already, unconditionally, deletes any leftover
`.tmp` file at every startup, regardless of which component (flush or
a future Compaction) left it behind — Compaction reusing the *identical*
build-then-atomic-rename sequence (`RUBIC_SSTABLE_FORMAT_SPECIFICATION.
md` §3.4) means a crashed Compaction's partial output needs **zero**
new recovery code; the existing sweep already covers it.

**No existing snapshot-safety gap**: `oldest_live_snapshot_seq()`
(`src/lsm/mod.rs:1378-1380`, backed by `SnapshotRegistry`, `:266-342`)
already exists, is already certified (`PHASE_READ_ENGINE_
CERTIFICATION.md` row 8), and is already documented, in its own
original doc comment, as "the mechanism a future Compaction phase will
need ('never remove a version still needed by the oldest live
snapshot') without this phase building Compaction itself" — this is
*exactly* the API Compaction needs (§3 below traces the proof). **No
new Snapshot API is required.**

**No existing Compaction tests, thread, config, trigger, or fault-hook
type** of any kind exist anywhere in `src/` today (`grep -rn
"compaction\|Compaction" src/ --include=*.rs` outside `src/compaction/
mod.rs` itself returns zero hits in production code — confirmed
directly).

**Conclusion, stated plainly**: Compaction is **completely absent** as
working code. It is, however, **unusually well-prepared for** by the
existing, already-certified Write/Read Engine infrastructure —
Manifest `RemoveSstable`, the recovery reconciliation sweep's orphan-
cleanup branch, the atomic-construction/`.tmp`-sweep discipline, and
`oldest_live_snapshot_seq()` were all built with Compaction as an
anticipated future consumer, and none of them need to change.

## 2. Protected Write Engine contract — audited, not assumed frozen

Every item the brief names as frozen was checked directly against the
current source, not assumed unchanged because a prior document said
so:

| Frozen item | Verified location | Compaction's relationship to it |
|---|---|---|
| WAL format/recovery/durability | `src/wal/` | Compaction never touches the WAL — its data source is already-flushed SSTables, never the WAL directly (confirmed: no `wal::` import would be needed) |
| Group Commit | `src/wal/group_commit.rs` | Not involved — Compaction has no write-durability step of its own (its output is a *read-path-only* derived structure per `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.5's own framing, extended to compaction output) |
| Dedicated Batch Coordinator | `src/execution/batch_coordinator/` | Not involved — no WAL append originates from Compaction |
| MemTable durability, flush ordering | `src/lsm/mod.rs` `apply_after_durable`/`freeze_locked`/`spawn_flush_thread` | Compaction consumes `sstables`, never `active`/`immutables` directly — flush's own publish-then-checkpoint sequence is untouched |
| SSTable creation format | `src/sstable/writer.rs`, `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` | Compaction's output SSTable **must** use the identical writer (§10 below) — no format change is being proposed |
| SSTable publication ordering | `src/lsm/mod.rs:1821-` ff. (flush thread) | Compaction's own publish ordering is modeled on, but is a distinct sequence from, flush's (§ADR "Manifest transition") |
| Manifest durability | `src/manifest/` | `RemoveSstable` already exists and is already durable/tested (§1) |
| Checkpoint semantics | `src/lsm/mod.rs` `checkpoint_seq`, `ManifestEdit::SetCheckpoint` | Compaction never touches checkpoint — it has no WAL position to advance |
| WAL purge semantics | `src/wal/` `purge_before` | Not involved — Compaction never calls it |
| `StoragePressure`/`StorageFull` | `src/lsm/mod.rs:111-` ff. `StorageState`, `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` | Compaction must **observe**, never mutate, this state machine (§ADR "storage-pressure behavior" — only a successful *flush* clears `StorageFull` today, per `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` §13; whether a successful compaction should also be allowed to is an open decision, resolved conservatively in the ADR, not assumed) |

No item in this table requires a code change for Compaction to
integrate. This report found no case where Compaction's stated goals
(LSM Engine Spec §5) require weakening any of the above.

## 3. Protected Read Engine contract — snapshot safety traced in full

**The core proof this section exists to establish**: Compaction can
remove a version (including a tombstone) only when no live snapshot
could still observe it — and the existing `Snapshot`/`SnapshotRegistry`
API is *exactly* sufficient to decide this, with no new API needed.

Traced directly in source:

1. `LsmEngine::snapshot(&self) -> Snapshot` (`src/lsm/mod.rs:1367-1372`)
   captures `seq = self.snapshot_seq()` (the current durable watermark)
   and registers it: `self.snapshot_registry.acquire(seq)`.
2. `SnapshotRegistry` (`:266-342`) is a `Mutex<BTreeMap<u64, u64>>`
   multiset (`seq -> outstanding_count`). `acquire`/`release` are
   simple increment/decrement-and-remove-at-zero (`:280-296`).
3. `Snapshot::seq(&self) -> u64` exposes the pinned watermark. `Drop`
   (`:338-342`) always calls `release(self.seq)`.
4. `LsmEngine::oldest_live_snapshot_seq(&self) -> Option<u64>`
   (`:1378-1380`) returns `counts.keys().next().copied()` — the lowest
   currently-outstanding `seq`, or `None` if no snapshot is live.

**The proof**: a version at sequence `s` for key `k` is observable by
some live snapshot **if and only if** there exists a live snapshot
whose `seq >= s` (a snapshot pinned at `seq_snap` resolves `get_as_of
(k, seq_snap)`/`range_scan(.., seq_snap)` to "the highest version of
`k` with `version.seq <= seq_snap`" — `ADR-RE-001` §4/§5, certified,
frozen). Therefore a version at `s` is **provably unobservable by any
live snapshot** exactly when `s < oldest_live_snapshot_seq()` (every
live snapshot's own `seq` is `>= oldest_live_snapshot_seq()` by
definition of "oldest," so if `s` is below even the oldest, it is
below every live snapshot's pin point, so no live snapshot's resolution
rule can ever select it). When `oldest_live_snapshot_seq()` is `None`
(no live snapshot at all), every version is safe to consider for
removal purely on ordinary "superseded by a newer version" grounds
(LSM Engine Spec §5.2 step 3's own second clause) — there is no
snapshot to protect it. This is the exact rule LSM Engine Spec §5.2
step 3 states in prose ("a version ... may be dropped only if no live
`snapshot_ref` ... has an `as_of_seq` that could still need it, AND it
is superseded by a newer surviving version") — now derived from the
actual, certified API rather than merely cited from the spec.

**Terminology mapping, stated explicitly** (the brief's own §5
instruction): `RubixDB-Architecture-Specification-v1.0.md` §6.1's
`snapshot_refs: Vec<SnapshotHandle>` is a field of the **future,
not-yet-built** multi-engine partition-metadata schema. LSM Engine
Spec §4.2 already resolves this for the single-engine v1 case: "this
is the Phase 0, single-engine realization of Architecture Spec §6.1's
`snapshot_refs`." The actually-implemented, actually-certified
mechanism performing this role today is `Snapshot`/`SnapshotRegistry`/
`oldest_live_snapshot_seq()` — not a new type, not a gap. **No gap
found; no new Snapshot API is required or proposed.**

**One real, non-blocking observation**: `oldest_live_snapshot_seq()`
has never had a caller before this phase (it exists solely for this
future consumer, per its own doc comment). It is certified (tested for
correct registration/deregistration/ordering across multiple
snapshots — `PHASE_READ_ENGINE_CERTIFICATION.md` row 8) but has never
been exercised *together with* a real Compaction run. A dedicated
integration test exercising the two together is listed as required in
the ADR's test-strategy section, not assumed to already exist.

## 4. Compaction goals, per the authoritative specification

Directly from `RubixDB-LSM-Engine-Specification-v1.0.md` §5 (quoted/
paraphrased faithfully, not reinterpreted):

- **Responsible for**: merging all currently-live SSTables into one
  new SSTable when `compaction_trigger_count` is reached, dropping
  versions/tombstones that are both (a) superseded by a newer version
  of the same key and (b) not needed by any live snapshot.
- **Explicitly NOT responsible for** (v1 scope cut, stated in the spec
  itself, §5.1): partial/leveled/selective compaction — "full
  compaction" of *all* live tables every time is the only strategy
  defined for v1. Choosing a more selective strategy is explicitly
  deferred to "Architecture Spec §17's ablation/benchmark protocol,"
  not to be guessed at now.
- **Trigger**: live SSTable count reaching `compaction_trigger_count`
  (spec default: 4). No other trigger (size-based, time-based,
  read-amplification-based) is specified for v1.
- **Participating SSTables**: *all* currently-live SSTables — v1 has
  no partial-selection concept.
- **Output**: exactly one new SSTable, built via the identical process
  as a normal flush's SSTable construction (§2.7) and the identical
  atomic-rename discipline (§3).
- **Obsolete inputs**: removed from the Manifest (`REMOVE_SSTABLE` per
  input), then physically deleted once no in-flight reader holds a
  reference (§5.2 step 8, `Arc` strong-count-based).
- **Version retention**: per the tombstone-safety rule (§3 above).
- **Manifest updates**: `ADD_SSTABLE` (new output) *before*
  `REMOVE_SSTABLE` (each input) — explicitly stated as non-load-bearing
  ordering (§5.2's own note, quoted in full in §7 below).
- **Crash recovery**: reduces to the Manifest's own existing replay
  rules plus the existing orphan-file sweep (§1/§7).
- **Concurrent readers**: must never observe a partially-written
  output SSTable (guaranteed structurally by the existing atomic-
  construction discipline, §3 of the format spec) and must never have
  an in-use input SSTable's file deleted out from under them (§5.2 step
  8's `Arc` refcount rule, empirically verified safe on this project's
  actual Windows target platform — §6 below).
- **Concurrent flushes**: not directly addressed by LSM Engine Spec §5
  — treated as an open question in the ADR (§8 below), resolved
  conservatively (serialize Compaction against flush via the same
  `sstables` `RwLock` flush itself already uses, not a new lock).

## 5. Snapshot safety — see §3 (traced together, not duplicated)

## 6. Manifest authority — SSTable deletion timing, empirically verified

LSM Engine Spec §5.2 step 8's "simplest v1 implementation" is `Arc`
strong-count-based: an `Arc<SsTable>` whose `strong_count` has dropped
to 1 (only the being-removed live-list entry itself, about to be
dropped) is safe to unlink from disk. This assumes deleting a file
while another process/handle may still have it open does not corrupt
or block, which is POSIX-native behavior but **not** the Windows
default — and this project's actual, current development/test
environment is Windows (confirmed via this session's own environment).

**Not assumed — tested directly**, with a throwaway, standalone Rust
program compiled outside the project (not part of `Cargo.toml`,
deleted after use): opened a file, opened a **second** handle to the
same file via plain `std::fs::File::open` (the exact call `SsTable::
open` uses), then called `std::fs::remove_file` while both handles
were still open.

```
RESULT: remove_file SUCCEEDED while file still open (handle f2 alive)
RESULT: path.exists() after both handles dropped = false
```

**Finding**: Rust's `std::fs::File` on Windows sets `FILE_SHARE_DELETE`
by default (confirmed empirically, not from documentation recall) —
`remove_file` succeeds while a second handle is open, matching POSIX-
like behavior closely enough that the spec's `Arc`-strong-count-based
deletion timing is **safe on this project's actual target platform,
as-is, no platform-specific workaround needed**. This was a real,
concrete risk worth checking rather than assuming either way — the
answer is reassuring, not a gap.

**One scope boundary of this specific test, stated honestly**: it
confirmed *deletion* succeeds and the path disappears once every handle
closes — it did **not** separately re-verify that a still-open handle
can continue to perform a *positional read* (`read_exact_at`/
`seek_read`) against the now-unlinked-but-still-open file afterward.
This is the standard, well-established Windows NTFS "delete pending"
behavior (data remains accessible via already-open handles until they
all close) and is consistent with the observed result, but was not
independently re-tested here specifically for a positional-read-after-
unlink sequence — the ADR's test-strategy section lists this as a
required Compaction-specific test (a range scan whose already-open
`SsTable` file handle survives a concurrent unlink by a compaction
run), not assumed proven by this smaller check alone.

`ManifestState::apply` (§1) already enforces: `AddSstable` is
idempotent-safe (a duplicate for an already-live id is a no-op, never
an error); `RemoveSstable` for a *never*-added id is `EngineError::
Corruption` (fail-closed, matching the project's established
convention throughout); `RemoveSstable` for an already-removed id is
idempotent (no-op). **Directory scanning is not, and under this
design would not become, authoritative** — `reconcile_sstables_with_
manifest` (§1) already treats the Manifest's `live_sstables`/`ever_
added` as ground truth and reconciles the directory *against* it, in
every branch, already.

## 7. Crash consistency — the state machine, derived from existing code, not invented

LSM Engine Spec §5.2's own note (quoted in full, since it is the
single most load-bearing sentence for this whole section):

> "Step 5 happening before step 6 is deliberate and ... is **not**
> load-bearing for correctness here: because the new SSTable is a
> complete, self-sufficient, correctly version-resolved merge of the
> inputs, a crash between steps 5 and 6 leaves the old input SSTables
> redundantly still 'live' in the Manifest alongside the new one.
> Reads remain correct either way ... and Recovery's Manifest replay
> ... simply catches up the remaining `REMOVE_SSTABLE` edits — or, if
> they were never issued at all before the crash, a subsequent
> compaction cycle will naturally subsume the stale duplicates."

Walking every crash window named in the brief, against the *actual*
existing recovery code (`reconcile_sstables_with_manifest`, `Manifest::
open_after_exclusive_lock`, `ManifestState::apply`), not invented
fresh:

| Crash window | What survives | What the next `open()` observes | Extra code needed? |
|---|---|---|---|
| Before output creation | Nothing started | Old inputs, unchanged | None |
| During output creation (before fsync/rename) | A `.tmp` file at most | `.tmp` swept unconditionally at startup (`sstable::discover`'s existing sweep) — old inputs still live, unaffected | **None** — existing sweep already covers this |
| After output fsync + atomic rename, before `ADD_SSTABLE` | A valid, complete, but Manifest-unacknowledged `.sst` file | `reconcile_sstables_with_manifest`'s third branch (§1: "in neither... a fresh `ADD_SSTABLE` is durably appended... and it joins the live set") **adopts it automatically** — old inputs also still live (their own `REMOVE_SSTABLE`s never ran). Result: old inputs + new output all live simultaneously — safe (new output's records are a subset of, and agree with, the old inputs' records at every shared `(key, seq)`), just temporarily redundant | **None** — existing "orphan valid file, never acknowledged" branch already handles this, though never yet exercised by a real test (§1's caveat applies) |
| After `ADD_SSTABLE`, before any `REMOVE_SSTABLE` | New output live, all old inputs still live too | Exactly the state LSM Engine Spec §5.2's own note describes — safe, redundant, self-healing on the next compaction cycle | None |
| During the `REMOVE_SSTABLE` sequence (some inputs removed, some not) | New output live; some old inputs removed from Manifest, some still live | Removed-but-still-on-disk inputs are swept by `reconcile_sstables_with_manifest`'s second branch (§1) — **this is the one branch with zero current test coverage**, flagged, not assumed proven | **None** structurally, but **a new test is required** (ADR test-strategy) |
| After all `REMOVE_SSTABLE`s, before physical file deletion | New output live; inputs removed from Manifest, files may still be on disk | Same orphan-sweep branch as above | Same as above |
| After physical file deletion | New output live; inputs fully gone | Clean, final state | None |

**No new crash-recovery mechanism is required.** The existing Manifest
replay + directory reconciliation sweep, unmodified, already covers
every window — because Compaction's Manifest sequence (`ADD` then
`REMOVE`×N) is structurally the same shape as flush's own (`ADD` only,
one at a time in immutable-drain order), just with an added `REMOVE`
phase whose recovery-side handling was already built and is now, for
the first time, about to be reachable. The one concrete follow-up this
report identifies is **test coverage**, not new mechanism.

## 8. Concurrency model — derived from the existing lock/`Arc` pattern, not assumed

`LsmEngine.sstables: Arc<RwLock<Vec<Arc<SsTable>>>>` (`:825`). The
existing flush thread's own pattern (`:1978-1984`, verified by direct
read): build the new SSTable file **entirely outside any lock**
(`sstable::write_from_memtable`, no lock held), then take the `sstables`
write-lock only for the brief, in-memory list-splice (`insert(0, ...)`,
idempotent). **Compaction should follow the identical shape**: perform
the k-way merge and output-file construction (the expensive, long-
running part) with **no** lock held at all — reading the *inputs* only
requires the same brief read-lock a `ReadView` capture already uses
(`ADR-RE-001` §1) — then take the `sstables` write-lock only for the
final, brief splice (remove N input `Arc`s, insert 1 new `Arc`). This
means **writers are never blocked for the duration of a large
Compaction** — the brief's own explicit requirement — because the
write-lock Compaction needs is held for the same brief, O(1)-ish
duration flush's own equivalent step already is, not for the whole
merge.

**Readers**: a `range_scan`/`range` call's `ReadView` (`capture_read_
view`, `ADR-RE-001` §1) clones the `sstables` list's `Arc`s under a
brief read-lock, exactly like today — a concurrent Compaction splice
happening between two reads is invisible to a read already in
progress (its `Arc` clones keep the old, pre-splice objects alive
regardless of what the live list does afterward — the same guarantee
that already makes concurrent-flush-during-a-scan safe, per the
already-certified `range_scan_during_concurrent_flush_sees_a_coherent_
snapshot` test). **Point lookups** (`get`/`get_as_of`/`contains`) use
the existing sequential-per-source-lock pattern (`ADR-RE-001` §1's own
established, certified, frozen argument) — unaffected by Compaction
the same way they are unaffected by flush.

**Concurrent flush + Compaction**: not directly addressed by LSM
Engine Spec §5 — this report treats it as an open decision, resolved
in the ADR (not silently assumed): serialize Compaction's own
`sstables`-list splice against flush's, using the *same* `RwLock`
flush already takes (not a new lock, not a global engine lock). Since
both operations only hold that lock briefly (splice-only, per above),
this does not create a new blocking hazard.

**No global engine lock is proposed or required.**

## 9. Compaction strategy — already specified, not a v1 design choice

"Size-tiered, deliberately simple for v1... whenever the live SSTable
count reaches `compaction_trigger_count` (default 4), compact **all**
currently-live SSTables into a single new one ('full compaction' rather
than a partial/leveled selection)" — `RubixDB-LSM-Engine-Specification-
v1.0.md` §5.1, verbatim. The spec itself states this is "a documented
scope cut, not an oversight," deferring any more selective (leveled/
tiered-partial) strategy to a future measurement-driven decision per
Architecture Spec §17's ablation protocol. **This report does not
propose choosing a different strategy** — the authoritative
specification already resolved this question; the ADR records it as
inherited, not re-derived.

## 10. Input/output SSTable format — fully reusable, zero format change

`SsTable::write_from_memtable`... wait — verified directly: the actual
writer entry point is `write_from_memtable` (`src/sstable/writer.rs`,
re-exported `src/sstable/mod.rs:35`), which takes a `&MemTable`, not
an arbitrary sorted-record iterator. Compaction's merge output is a
sorted stream of surviving `(key, seq, RecordValue)` records — **not**
a `MemTable`. This is the one place this report found the *existing*
writer API does not, as-is, accept Compaction's natural input shape
directly — an accurate, verified gap (not a guess), and the ADR's
"output formation" decision addresses it: either (a) materialize the
merge's surviving records into a temporary in-memory `MemTable` and
reuse `write_from_memtable` unchanged, or (b) generalize the writer's
entry point to accept any sorted `Iterator<Item = (Vec<u8>, u64,
RecordValue)>`, of which `MemTable`'s own iteration would become one
caller. Both keep the on-disk format (block/bloom/index/footer
construction, checksums) **completely unchanged** — this is a call-
site/API-shape question, not a format question, and is resolved in
the ADR without touching `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`.
**No SSTable format change of any kind is required or proposed.**

## 11. Version/tombstone retention — worked truth table

Key `k`: `PUT@1(v1)`, `PUT@2(v2)`, `DELETE@3`, `PUT@4(v4)`. Applying
LSM Engine Spec §5.2 step 3's rule (a version may be dropped only if
**both** (a) no live snapshot's `as_of_seq` could still need it, i.e.
`as_of_seq < oldest_live_snapshot_seq()` is false for that version only
when some live snapshot's `seq >= this version's seq` — precisely,
per §3 above, a version at `s` is protected iff some live snapshot has
`seq >= s`, and among all versions of one key, `get_as_of`/`range_scan`
only ever needs the *single highest* surviving `seq <= that read's own
as_of_seq` — so a version can be dropped once a **newer** surviving
version already covers every `as_of_seq` that could have selected it),
**and** (b) it is superseded by a newer surviving version of the same
key:

| Live snapshot at | v1(@1) | v2(@2) | tombstone(@3) | v4(@4) | `get_as_of` result at each snapshot |
|---|---|---|---|---|---|
| none | drop | drop | drop | **keep** | (n/a — only "now" matters, resolves to `v4`) |
| @1 | **keep** (only version ≤1) | drop | drop | keep | `get_as_of(k,1)=v1`; `get_as_of(k,now)=v4` |
| @2 | drop (superseded by v2, and no snapshot ≤1 needs it) | **keep** | drop | keep | `get_as_of(k,2)=v2`; `get_as_of(k,now)=v4` |
| @3 | drop | drop | **keep** (the tombstone itself is the highest version ≤3) | keep | `get_as_of(k,3)=None`; `get_as_of(k,now)=v4` |
| @4 | drop | drop | drop (superseded by v4, and no live snapshot ≤3 needs it) | **keep** | `get_as_of(k,4)=v4`; `get_as_of(k,now)=v4` |

**Two live snapshots simultaneously** (e.g. @1 and @3): union of what
each alone requires — `v1` survives (needed by @1), the tombstone@3
survives (needed by @3), `v2` and `v4`... `v4` always survives (it is
the current/newest value, never superseded by anything). `v2` is
dropped (superseded by the tombstone@3, which itself is retained for
@3's sake, and no live snapshot has `seq` in `[2,3)` specifically
needing `v2` over the tombstone). This generalizes to: **the retained
set is exactly {every version needed as "the answer" by at least one
live snapshot's `as_of_seq`} ∪ {the single newest version of the key},
period** — a version is redundant, and therefore droppable, exactly
when some *other, retained* version already answers every `as_of_seq`
that could have selected it.

**Validated against the Read Engine's own frozen semantics**
(`get_as_of`/`range_scan`, `ADR-RE-001` §4/§5, certified): both already
resolve "highest version with `seq <= as_of_seq`" — the table above is
a direct, mechanical application of that exact rule, not a new or
different one. Compaction changes *what physically exists on disk*; it
must never change *what any valid `as_of_seq` read observes* — this
table is the concrete evidence that the stated retention rule achieves
that, for this worked example.

**Erratum, added during Increment 1 implementation, table above left
unedited (append-only convention — this note is the correction, not a
rewrite)**: the `@1` and `@2` rows' `drop` verdicts for `v2` and the
`@3` tombstone are correct **only** under an unstated assumption that
snapshot`@1`/`@2` is the *only* live snapshot at that point. `oldest_
live_snapshot_seq()` (§3/§5 above) exposes only the **minimum** live
snapshot `seq`, never the full set — so the actual retention
*function* (`retain_versions`, `src/compaction/mod.rs`, implemented
this increment) cannot distinguish "exactly one live snapshot at `@1`"
from "several live snapshots, the oldest at `@1`, with others at `@2`
or `@3` this function has no visibility into." The only safe, correct
behavior given that limited information is to conservatively retain
**every** version from the floor (the oldest live snapshot's own
answer) through the newest, not just the floor and the newest — for
row `@1` this means `v2` and the tombstone@3 must also survive
(nothing is dropped except by the "newest" rule not applying below the
floor); for row `@2`, `v1` alone is droppable, the tombstone@3 must
still survive. `PHASE_COMPACTION_INCREMENT1_RESULTS.md` has the full
account, including the corrected truth table and the regression tests
(`src/compaction/tests.rs`) that pin the *actual*, correct behavior
down explicitly, including a new boundary case (a live snapshot
pinned strictly *between* two versions) this original table's
exact-boundary examples did not exercise at all. The underlying ADR
decision (use `oldest_live_snapshot_seq()`, no new API, correctness
over reduction ratio) is unchanged — only this table's specific,
under-specified numbers for two of its five rows are corrected.

## 12. Compaction output correctness — the invariant, stated precisely

**Invariant**: for every snapshot sequence `s` that was valid before a
compaction run (i.e., `s <=` the current max seq at the time
compaction started, and if `s` corresponds to a live snapshot, that
snapshot's registration predates the compaction), `get(k)`/`get_as_of
(k, s)`/`contains(k, s)`/`range(...)`/`range_scan(..., s)` must return
**byte-for-byte identical results** before and after compaction, for
every key `k`. Point reads, historical reads, range scans, `contains`,
tombstones (a tombstone-shadowed key must still report `None`/`false`
identically), and recreated keys (delete-then-put, per `PHASE_READ_
ENGINE_PERFORMANCE.md`'s own established `range_scan_delete_then_
recreate_shows_only_the_recreated_value` precedent) are all covered by
the single, uniform statement above — no special-casing per operation
is needed, because every one of those operations is already defined,
frozen, and certified purely in terms of "highest surviving version
with `seq <= as_of_seq`," and §11 already proves compaction preserves
that answer for every retained snapshot. This is restated as its own
explicit ADR decision (not left implicit) because it is the single
most important correctness property Compaction must satisfy, and the
differential/property test strategy (ADR §"test strategy") exists
specifically to verify it empirically, not just by proof.

## 13. Corruption model — inherited, not reinvented

Existing, certified conventions this report found directly applicable,
verified by reading the actual code paths involved:

- **Corrupt input SSTable** (discovered while Compaction reads it):
  `read_block`/`format::decode_block` already fail closed with
  `EngineError::Corruption` on a bad checksum (`RUBIC_SSTABLE_FORMAT_
  SPECIFICATION.md`, already certified). Compaction's merge, consuming
  the same `range_scan_raw`/`SsTableRangeCursor` machinery the Read
  Engine already uses (§10), inherits this for free — a corrupt input
  block aborts the compaction attempt with an error, never silently
  drops or skips the corrupted key.
- **Write/fsync/rename failure while building the output**: identical
  to a flush's own atomic-construction failure mode (`RUBIC_SSTABLE_
  FORMAT_SPECIFICATION.md` §3.3's "what a crash at each step leaves
  behind" — a `.tmp` file, never trusted, swept at next startup). No
  new failure-classification logic is needed for this step; it already
  exists and is shared.
- **Manifest append failure**: identical to flush's own — `Manifest::
  append_sync` failing means the durable state is unchanged, the
  attempt is a no-op from the outside; already covered by existing
  Manifest fsync-failure tests.
- **ENOSPC while building the output**: this is the one genuinely new
  case — flush's `FlushIoFaultHook`/ENOSPC-classification machinery
  (`ADR-WE-SP-001`) exists for the flush path specifically; Compaction
  needs its own hook point to test this deterministically (ADR
  decision, modeled directly on the existing `FlushIoFaultHook`
  pattern, not invented from scratch) — see §14.
- **Corrupt output** (should never happen if input validation + the
  writer's own checksum construction are both correct, but must still
  be handled if it somehow did): the existing "never trust a just-
  written file without re-validating it" precedent is `SsTable::open`
  itself, which always re-validates footer/bloom/index on open,
  including for a freshly-written file (flush already relies on this —
  `SsTable::open(&meta.path, id)?` at `:1981` is not skipped for a
  table this same process just wrote). Compaction publishing its
  output the same way inherits the same protection for free.

**Fail-closed is preserved throughout — no error is swallowed, no
partial output is silently treated as complete**, matching every
existing precedent this codebase has established since the Write
Engine's own certification.

## 14. Storage pressure — integration point, not a new state machine

Compaction's output SSTable temporarily coexists on disk with all of
its inputs (worst case: the new output is the same total size as all
inputs combined minus whatever was actually dropped — i.e., **up to
~2x the pre-compaction live-SSTable disk footprint, transiently**,
until the inputs are deleted). This is a real, load-bearing production
concern, not a footnote.

Verified against `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` directly:
`StorageState` (`Healthy`/`StoragePressure`/`StorageFull`) is an
`Arc<AtomicU8>` on `LsmEngine`, read via `storage_state()` (`:1085-
1087`), and **only a successful flush's own success path ever clears
`StorageFull`** (`PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` §13,
`src/lsm/mod.rs` `freeze_locked`'s own doc comment: "only the flush
thread's own success path ever clears `StorageFull`... this promotion
is purely additive, per the ADR's explicit instruction not to
repurpose that established contract"). This is an explicit,
already-ratified constraint this report does **not** propose
loosening.

**Open question this report flags rather than silently resolves**:
should a *successful Compaction* (which durably proves the disk can
currently accept a large write, exactly like a successful flush does)
also be allowed to help clear `StorageFull`/`StoragePressure`? The
ADR resolves this conservatively: **no, not in this phase** — only
flush's own success path may transition `StorageState`, Compaction
only ever *observes* it. Reason (stated in full in the ADR): granting
Compaction write access to the same state machine flush owns is itself
a protected-contract change requiring its own sign-off, and the safer,
minimal-blast-radius default is to leave that machine as flush's sole
mutator and have Compaction defer/skip triggering while `StorageFull`
(since compaction needs *more* transient headroom, not less, making it
a poor candidate to run under storage exhaustion regardless).

## 15. Resource limits — bounds to define, not yet defined

No existing code bounds Compaction's memory, file-handle, thread, or
temporary-file usage, because no Compaction code exists (§1). Relevant
existing precedent this report found for the ADR to build from:
`SsTable`'s own per-table resident footprint (bloom filter + sparse
index, `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md` §1.1, already
measured and certified) already means a full-compaction merge over
`compaction_trigger_count` (or more, if it grows before a cycle
completes) live tables touches that many `Arc<SsTable>` clones plus
that many concurrent `SsTableRangeCursor`s (Increment 6's own owned-
`Arc` persistent cursor type, directly reusable here — see §10) — a
memory profile proportional to *input table count*, not to total data
volume, matching the Read Engine's own already-established bounded-
memory range-scan design (`ADR-RE-001` §12, certified). The ADR
defines explicit bounds (not speculative ones) for: merge buffer
memory (bounded by input count via the existing k-way-merge heap
pattern, not by data volume), temporary disk (the ~2x worst case
above, made explicit rather than hidden), and file handles (one per
input SSTable for the merge's duration, plus one for the in-progress
output — both already within the existing "~1 handle per live
SSTable" budget the Read Engine already measured and certified).

## 16. Read concurrency — `Arc<SsTable>` lifetime across a compaction

Already proven safe in §6 (empirical Windows test) and §8
(concurrency model) — restated here as its own item because the brief
calls it out separately: a `RangeScanIter`'s `ReadView` (`ADR-RE-001`
§1) and each of Increment 6's persistent `SsTableRangeCursor`s hold
their own `Arc<SsTable>` clone for the scan's entire duration,
independent of what the live `sstables` list does afterward. An
obsolete input SSTable is removed from the live list first (brief
write-lock splice, §8), and only physically unlinked once its `Arc`
strong count drops to 1 (LSM Engine Spec §5.2 step 8) — meaning any
reader that already captured a `ReadView`/cursor before the splice
keeps that table fully usable (open file, valid index/bloom, readable
blocks) for as long as its own scan needs it, via ordinary Rust `Arc`
reference counting. **No `unsafe`, no lifetime trick of any kind is
needed or proposed** — this is the same safe pattern `ReadView`
already uses today, extended to one more retiring-list transition.

## 17. Compaction API — smallest shape, not yet decided in this report

Deferred to the ADR (§"Compaction API" decision) rather than answered
here, per the brief's own instruction not to add public API without a
real caller — this report's job was to establish *what exists today*
(nothing) and *what the surrounding contracts require* (§1-16 above),
which the ADR now uses to size the smallest sufficient internal API
surface.

## 18. Compaction trigger — deterministic core operation first

LSM Engine Spec §5.1 specifies the trigger condition (`compaction_
trigger_count` live SSTables) but not *which thread checks it* or
*when*. No existing background-thread precedent for Compaction exists
(`spawn_flush_thread` is flush-specific, `mpsc`-driven by `FlushMsg`
from `freeze_locked`, not reusable as-is for a count-driven trigger).
Per the brief's own explicit instruction ("Do not introduce a
background Compaction thread yet unless the architecture requires it.
First define the deterministic core Compaction operation"), this
report recommends (ADR decision) defining `compact(inputs: Vec<Arc
<SsTable>>) -> Result<SstableMeta>` as a synchronous, directly-callable
function first — trigger wiring (background thread vs. checked
inline after each flush, matching `freeze_locked`'s own precedent of
checking conditions synchronously rather than polling) is a separate,
later decision this report does not resolve prematurely.

## 19-21. Test strategy, differential testing, property testing

Existing, directly reusable precedent this report found, all already
certified:

- **Differential/reference-model testing**: `range_scan_property_
  tests::lsm_engine_range_scan_matches_independent_reference_model`
  (Read Engine), `manifest::tests::property::manifest_replay_matches_
  independent_reference_model`, `memtable::property_tests::memtable_
  matches_naive_reference_model`, `sstable::tests::property::sstable_
  matches_memtable_reference` — four independent precedents, all using
  `proptest = "=1.11.0"` (already an exact-pinned dependency, `Cargo.
  toml:37` — **no version bump or new dependency needed**). Compaction's
  own differential tests should follow the identical shape: an
  independent reference model (never the production merge algorithm
  itself, `ADR-RE-001` §17's own already-established, certified
  principle) comparing pre-compaction and post-compaction logical reads
  for the same snapshot sequences.
- **Deterministic fault injection, no sleeps**: `FlushFaultPoint`
  (`src/lsm/mod.rs:205-` ff.) + `install_flush_fault_hook` (panic-based,
  named crash points) and `FlushIoFaultHook` + `install_flush_io_fault_
  hook` (synthetic `io::Error` injection) are the exact, already-
  certified precedent for exactly what the brief's §19 asks for
  ("deterministic synchronization/fault injection... no sleep-based
  race tests"). Compaction needs its own `CompactionFaultPoint`/
  `CompactionIoFaultHook` pair, modeled directly on these two existing
  types, not invented from a blank page.
- **Concurrent-operation tests**: `range_scan_during_concurrent_flush_
  sees_a_coherent_snapshot` is the direct precedent for a
  `range_scan_during_concurrent_compaction_sees_a_coherent_snapshot`
  equivalent.

The full test matrix the brief's §19 asks for is enumerated as its own
ADR section (test strategy), built from this precedent list, not
repeated twice.

## 22. Performance model — metrics to define, not yet implemented

No benchmark was run this phase (explicitly out of scope, brief §22/
Phase 1 preamble). Existing precedent for *what* to measure, reused
rather than invented: `ReadStats`' own six-counter shape (`ADR-RE-001`
§12, certified) — a small, precisely-defined counter set, each with
one unambiguous counting point, is this project's own established
convention for "instrumentation the production algorithm needs," and
`CompactionStats` (an ADR-level naming decision) should follow the
same shape: `input_bytes`, `output_bytes`, `records_read`, `records_
retained`, `records_dropped`, `tombstones_dropped`, `duration`,
`temporary_disk_peak` — no speculative metric beyond what the brief's
own §22 lists is proposed.

---

## Summary of what this audit found, in one place

1. Compaction is **completely absent as working code** — one 5-line
   doc-comment stub, wired into the crate but functionally empty.
2. It is **unusually well-prepared for**: `RemoveSstable` (Manifest),
   the orphan-file recovery sweep, the atomic-construction `.tmp`
   sweep, and `oldest_live_snapshot_seq()` were all built in advance,
   by the Write/Read Engine phases, specifically anticipating this
   phase — verified, not assumed.
3. **No existing frozen contract (Write or Read Engine) needs to
   change.** No SSTable format change. No new Snapshot API. No new
   Manifest edit type.
4. **One genuine implementation gap found**: `write_from_memtable`
   takes a `&MemTable`, not a sorted-record stream — Compaction's
   merge output needs either an in-memory `MemTable` intermediate or a
   generalized writer entry point (ADR decision, §10).
5. **One empirically-verified platform question, resolved favorably**:
   Windows delete-while-open works as the spec's `Arc`-refcount-based
   deletion timing assumes (§6) — tested directly, not assumed.
6. **One real test-coverage gap found**: the recovery sweep's
   "removed-but-undeleted orphan" branch is structurally complete but
   has never been exercised by any existing test (§1/§7) — flagged as
   a required new test, not silently trusted.
7. **One real open design decision, resolved conservatively in the
   ADR**: whether a successful Compaction may help clear `StorageFull`
   (§14) — resolved **no** for this phase, to avoid touching the
   already-ratified `StoragePressure` contract's sole-mutator
   guarantee.

No implementation was started. `src/compaction/mod.rs` is unmodified.
