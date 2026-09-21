# RubiXDB — Compaction Architecture Decision Record

**ADR ID:** ADR-COMPACTION-001

**Status:** Proposed for Review — **no implementation in this document or this step**

**Date:** 2026-09-21

**Scope:** `src/compaction/` only — the deterministic core `compact()`
operation, its Manifest/SSTable/Snapshot integration, and its test
strategy. Does **not** wire an automatic trigger (background thread or
otherwise — see Decision 14), does not touch the Write Engine or
protected Read Engine contracts (audited clean in `PHASE_COMPACTION_
ARCHITECTURE_REPORT.md` §2/§3), and does not change the SSTable
on-disk format, WAL, or Manifest edit types.

**Prepared from:** `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` (this
session's own read-only audit) — every decision below traces to that
report's numbered section or to a fresh, direct source check performed
while writing this ADR, not to assumption. Authoritative source:
`RubixDB-LSM-Engine-Specification-v1.0.md` §5 (Compaction), cross-
checked against `RubixDB-Architecture-Specification-v1.0.md` §6.1/§7.3
for the `snapshot_refs`/tombstone-safety terminology this codebase's
actual `Snapshot`/`SnapshotRegistry` already implements.

**Hard constraint acknowledged and honored throughout**: the Write
Engine (`PHASE_WRITE_ENGINE_CERTIFICATION.md`) and Read Engine
(`PHASE_READ_ENGINE_CERTIFICATION.md`) are both certified and frozen.
Every decision below was checked against this rule; none requires
changing WAL format/recovery/durability, Group Commit, the Dedicated
Batch Coordinator, MemTable durability, SSTable on-disk format,
Manifest edit types or durability, checkpoint semantics, WAL purge, or
`get`/`get_as_of`/`contains`/`range`/`range_scan`/`Snapshot`/
`ReadStats` behavior.

---

## 0. What this document is and is not

This is the contract document required before any Compaction
implementation begins. It resolves every decision area the phase
brief named, each with Decision / Reason / Alternatives / Safety
impact / Performance impact / Tests required. **Nothing in this
document has been implemented** — `src/compaction/mod.rs` remains the
5-line stub it was before this audit (verified in §"Source Tree Safety
Check" of the final report).

---

## 1. Decision: Compaction strategy

**Decision:** Size-tiered, full-merge — compact **all** currently-live
SSTables into exactly one new SSTable whenever the live count reaches
`compaction_trigger_count`. No partial/leveled selection in this ADR's
scope.

**Reason:** `RubixDB-LSM-Engine-Specification-v1.0.md` §5.1 already
specifies this, in exactly these terms, as a deliberate v1 scope cut
("Size-tiered, deliberately simple for v1... 'full compaction' rather
than a partial/leveled selection... a documented scope cut, not an
oversight"). This ADR inherits, rather than re-derives, that decision
— the authoritative specification is the source of truth here, and it
explicitly defers any more selective strategy to a future, measurement
-driven decision (Architecture Spec §17's ablation protocol), not to
be guessed at now.

**Alternatives considered:** Leveled compaction, size-tiered-partial
(select a subset by size bucket) — both rejected, not because they are
worse in the abstract, but because the authoritative spec already
resolved this question for v1 and neither this audit nor this ADR is
authorized to override it without new measurement evidence per
Architecture Spec §17.

**Safety impact:** None — purely a selection-scope decision, no
protected contract touched.

**Performance impact:** Full-merge means worst-case temporary ~2x
disk usage (Decision 12) and a merge cost proportional to total live
data volume every cycle — accepted as the stated v1 tradeoff, not
reassessed here.

**Tests required:** Covered by the test matrix (Decision 17) —
"many-table merge," "single-table no-op" (trigger at exactly
`compaction_trigger_count` with only that many tables, verifying the
full set participates).

## 2. Decision: Selection criteria / input selection

**Decision:** Input selection is **not** a separate step — it is
"every SSTable in the live `sstables` list at the moment Compaction's
`ReadView`-style capture happens," captured once via the identical
brief-read-lock-then-clone-`Arc`s pattern `ADR-RE-001` §1's `capture_
read_view` already uses for range scans.

**Reason:** Since the strategy (Decision 1) is always "all live
tables," there is no selection *logic* to design — only a capture
*mechanism*, and the existing `ReadView` capture pattern is already
the established, certified, safe way to take a stable, `Arc`-backed
snapshot of the live SSTable list without holding a lock for the
capture's downstream work. Reusing it means zero new locking pattern
is introduced.

**Alternatives considered:** A dedicated `CompactionInputSet` type
distinct from `ReadView` — rejected as unnecessary duplication; a
compaction input capture is structurally identical to a range scan's
`ReadView.sstables: Vec<Arc<SsTable>>` (it does not need `ReadView`'s
`active`/`immutables` fields at all, since Compaction only ever reads
already-flushed SSTables, never the MemTable layers — so it is a
strict subset, not a different mechanism).

**Safety impact:** None — read-only capture, same guarantees `ReadView`
already provides (a concurrent flush publishing a *new* SSTable after
capture is simply not included in this compaction cycle; correct,
not a race, since that new table will be included in the *next*
trigger check).

**Performance impact:** O(live table count) `Arc` clones under one
brief read-lock — identical cost profile to an ordinary range scan's
own capture, already measured negligible (`PHASE_READ_ENGINE_
PERFORMANCE.md`).

**Tests required:** "Concurrent flush" (brief §19) — a flush publishing
a new table while a compaction's input capture is in progress or just
completed must not corrupt either operation; the new table is either
included (captured before) or excluded-and-eligible-next-cycle
(captured after) — never lost, never double-counted.

## 3. Decision: Output formation — generalize the SSTable writer's input, not the format

**Decision:** Generalize the SSTable writer's *entry point* to accept
any sorted `Iterator<Item = (Vec<u8>, u64, RecordValue)>` (a new
function, e.g. `write_from_sorted_records`, sharing 100% of the
existing block/bloom/index/footer construction internals with
`write_from_memtable`, which becomes "iterate the MemTable in sorted
order, then call the shared core") — rather than materializing
Compaction's merged, surviving-version output into a temporary
in-memory `MemTable` and calling `write_from_memtable` unchanged.

**Reason:** `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §10 found this
is the one genuine implementation gap in the existing writer API —
`write_from_memtable` takes `&MemTable`, not a record stream, and
Compaction's k-way merge naturally produces a sorted *stream*, not a
pre-built `MemTable`. Materializing the whole merge output into an
in-memory `MemTable` first would mean peak memory proportional to
*total surviving data volume across all input tables* — for a full
compaction of `compaction_trigger_count`-or-more tables, this is
exactly the "unbounded memory requirement" the brief's §15 explicitly
warns against, and directly contradicts this project's own established
bounded-memory-streaming-write philosophy (`ADR-RE-001` §12: range
scans never materialize a whole result set; flush's own SSTable writer
already streams block-by-block from its *already-in-memory* MemTable
without an extra full copy). A generalized, streaming writer entry
point keeps Compaction's peak memory bounded to roughly one in-
progress output block, matching every other bounded-memory component
this codebase has built so far.

**Alternatives considered:** *Temporary `MemTable` materialization*
(rejected — see Reason: unbounded-relative-to-trigger-count memory,
the opposite of this project's established convention). *A wholly
separate Compaction-only writer, duplicating the block/bloom/index/
footer construction logic* (rejected — `RUBIC_SSTABLE_FORMAT_
SPECIFICATION.md`'s own construction rules must be followed identically
by every producer of a `.sst` file; duplicating that logic risks the
two implementations silently drifting apart over time, exactly the
"format change nobody signed off on" risk the brief's §10 explicitly
forbids without a dedicated format ADR — sharing one core function
structurally prevents that drift).

**Safety impact:** The on-disk format is **byte-for-byte unchanged** —
this is an internal Rust API refactor (a new, additional entry point;
`write_from_memtable`'s existing signature/behavior/callers are
unaffected), not a format change, and requires no new format ADR per
the brief's own §10 instruction. `write_from_memtable`'s own existing
test suite must continue to pass unmodified, proving the shared-core
refactor did not alter flush's own behavior.

**Performance impact:** Removes an entire redundant in-memory copy of
the compaction output compared to the rejected `MemTable`-
materialization alternative — a real, structural improvement, not
speculative (the same class of finding Increment 6's own persistent-
cursor work already demonstrated the value of: avoid a redundant
materialization step rather than accept it as a given).

**Tests required:** `write_from_sorted_records` (or equivalent) must
produce byte-identical output to `write_from_memtable` for the same
logical records (a differential test between the two entry points on
an equivalent input), plus the full existing SSTable-writer test suite
re-run against the new shared core to confirm flush's own behavior is
unaffected.

## 4. Decision: Version / tombstone retention model

**Decision:** A version (including a tombstone) at sequence `s` for
key `k` is dropped during compaction if and only if: (a) some *other*,
retained version of `k` already answers every `as_of_seq` that could
have selected version `s` (i.e., `s` is not the single newest version
of `k`, and no live snapshot's `as_of_seq` falls in the range that
would make `s` the answer), and (b) `s < oldest_live_snapshot_seq()`
*or* `oldest_live_snapshot_seq()` is `None`. The newest version of any
key is **never** dropped, tombstone or not (it is always a valid
answer for "now").

**Reason:** This is the exact rule `PHASE_COMPACTION_ARCHITECTURE_
REPORT.md` §11 derived and worked through a full truth table (PUT@1,
PUT@2, DELETE@3, PUT@4, evaluated against no-snapshot / @1 / @2 / @3 /
@4), validated mechanically against the Read Engine's own frozen
"highest version with `seq <= as_of_seq`" resolution rule
(`ADR-RE-001` §4/§5) — not a new rule, a restatement of LSM Engine
Spec §5.2 step 3's own prose, made precise and checked against a
concrete example.

**Alternatives considered:** A simpler "drop everything below `oldest_
live_snapshot_seq()` unconditionally, ignore supersession" rule —
rejected: this would incorrectly drop, e.g., `v1` in the "@1 live"
row of the worked table if a naive implementation dropped anything
`< oldest_live_snapshot_seq()` without checking it is still the *only*
version answering that snapshot's read. The two-part rule (supersession
**and** snapshot-safety) is necessary; supersession alone (ignoring
snapshots) is exactly the certified-frozen-guarantee violation this
whole ADR exists to prevent.

**Safety impact:** This is the single most safety-critical decision in
this ADR — an error here directly violates the certified Read Engine's
snapshot-consistency guarantee. Mitigated by: (a) the worked truth
table itself, (b) the differential/property test strategy (Decision
17) that compares pre- and post-compaction reads against an independent
reference model, never the production algorithm as its own oracle.

**Performance impact:** None beyond the merge cost already accounted
for in Decision 1.

**Tests required**: the full worked truth table (§11 of the
architecture report) as literal test cases, at minimum; the property-
test suite (Decision 17) generalizing it to random keys/versions/
snapshots.

## 5. Decision: Snapshot interaction — no new API

**Decision:** Compaction calls `LsmEngine::oldest_live_snapshot_seq()`
exactly once per compaction cycle, at (or just before) the point it
decides which versions survive — no new `Snapshot`-related type,
method, or field is introduced.

**Reason:** `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §3/§5 traced this
directly from source and proved the existing `Snapshot`/
`SnapshotRegistry`/`oldest_live_snapshot_seq()` API (already certified,
`PHASE_READ_ENGINE_CERTIFICATION.md` row 8) is exactly sufficient —
its own original doc comment already names this as its intended future
consumer. Per the brief's own explicit instruction ("do not invent a
new snapshot API if the current API is sufficient"), none is invented.

**Alternatives considered:** A per-compaction-cycle "pin the current
oldest snapshot and re-check it hasn't changed mid-merge" mechanism —
considered and rejected as unnecessary: a *new* snapshot taken by
another thread *during* an in-progress compaction cycle can only ever
*raise or maintain* `oldest_live_snapshot_seq()`'s protection (a new
snapshot registers at the *current* max seq, which is `>=` every
version being considered for the ongoing compaction), never lower it
— so a single read of `oldest_live_snapshot_seq()` at the start of the
retention decision is a conservative, safe bound; it is never *unsafe*
to have measured it slightly before a brand-new snapshot registers
(that snapshot's own `as_of_seq` was, by construction, not yet fixed
when compaction's own decision point already ran, so it cannot need a
version compaction already correctly decided to keep for older
snapshots, and it will always be `>=` any version's `seq` if it's a
genuinely new snapshot appearing after compaction's own input capture
— see Decision 2's own reasoning for why concurrent-flush timing
follows the identical "captured-before vs. eligible-next-cycle" shape).

**Safety impact:** None — purely additive, read-only use of an
already-certified API.

**Performance impact:** One `Mutex` lock + `BTreeMap::keys().next()`
read (`SnapshotRegistry::oldest`, already O(log n) in outstanding-
snapshot count, already measured negligible) — once per compaction
cycle, not per record.

**Tests required:** the snapshot-retention rows of the differential/
property test matrix (Decision 17); specifically, a new integration
test exercising `oldest_live_snapshot_seq()` *together with* a real
compaction run for the first time (flagged in the architecture report
§3 as never having had a real consumer before).

## 6. Decision: Tombstone retention — folded into Decision 4

**Decision:** A tombstone is retained/dropped by the *exact same* rule
as any other version (Decision 4) — it is not a special case. It is
dropped once both superseded (a newer surviving version of the same
key exists) and snapshot-safe (no live snapshot's `as_of_seq` could
still select it).

**Reason:** LSM Engine Spec §5.2 step 3 states the rule once, covering
"a version (including a tombstone)" — there is no separate tombstone
rule to design. Restated as its own decision only because the brief
lists it separately; the substance is Decision 4.

**Alternatives considered:** N/A — folded decision.

**Safety impact / Performance impact / Tests required:** Same as
Decision 4.

## 7. Decision: Manifest transition — reuse exactly, no new edit type

**Decision:** Compaction's Manifest sequence is exactly: one
`AddSstable` (the new output) durably appended and fsynced, **then**
one `RemoveSstable` per input, each durably appended and fsynced. No
new `ManifestEdit` variant is introduced.

**Reason:** `ManifestEdit::AddSstable`/`RemoveSstable` already exist,
already encode/decode/replay correctly (idempotently, per §1 of the
architecture report), and already have dedicated tests — the very
first production caller of `RemoveSstable` does not need a new type,
it needs to *call the existing one*. LSM Engine Spec §5.2 steps 5-6
specify this exact ordering and the reasoning for it (quoted in full in
the architecture report §7): the ordering is deliberate but **not**
load-bearing for correctness (unlike the SSTable-vs-Manifest ordering
in the atomic-construction discipline), because the new output is
already a complete, self-sufficient, correctly-resolved merge — a
crash at any point in this sequence leaves the database in a state
Recovery's existing Manifest replay already handles correctly (§7 of
the architecture report walks every window).

**Alternatives considered:** A single, atomic "compound" Manifest edit
combining the add and all the removes in one frame — rejected: would
require a new frame format/edit type (a format change, out of this
ADR's scope per the brief's own §10 instruction, and unnecessary,
since the existing sequential-edits-with-idempotent-replay approach
already tolerates a crash anywhere in the sequence without corruption,
per the spec's own explicit reasoning).

**Safety impact:** None new — reuses an already-certified, already-
fsync'd-durably mechanism exactly as designed.

**Performance impact:** `N+1` Manifest appends per compaction cycle
(one add, `N` removes) instead of one — each is a small, already-cheap
fsync'd frame append (`RUBIC_MANIFEST_FORMAT_SPECIFICATION.md`'s own
established cost profile); not expected to be the bottleneck relative
to the merge/write cost itself, but not separately benchmarked in this
audit (explicitly out of scope, brief §22 preamble).

**Tests required:** Manifest-transition rows of the crash-consistency
matrix (Decision 9/Decision 17): crash between `AddSstable` and the
first `RemoveSstable`, crash mid-`RemoveSstable`-sequence, crash after
the last `RemoveSstable`.

## 8. Decision: Crash protocol — no new recovery mechanism, one new required test

**Decision:** Compaction relies entirely on the *existing* Manifest
replay (`ManifestState::apply`) and directory reconciliation sweep
(`reconcile_sstables_with_manifest`) for crash recovery — no new
recovery code path is added. The one required *test* addition (not
mechanism addition) is a dedicated exercise of `reconcile_sstables_
with_manifest`'s "removed-but-undeleted orphan" branch
(`src/lsm/mod.rs:1781-1785`), which has zero existing test coverage
today (verified directly, architecture report §1/§7).

**Reason:** The architecture report §7 walked every named crash window
(before output creation, during output creation, after fsync/rename
before `ADD_SSTABLE`, after `ADD_SSTABLE` before any `REMOVE_SSTABLE`,
mid-`REMOVE_SSTABLE` sequence, after all removes before physical
deletion, after physical deletion) against the *actual* existing
recovery code and found every window already correctly handled by
mechanism that predates this phase — built by the Write Engine's own
authors specifically anticipating Compaction. Adding new recovery code
where working code already exists would violate this project's own
established "don't duplicate/drift a mechanism that already works"
principle (Decision 3's own reasoning applies equally here).

**Alternatives considered:** A dedicated `CompactionRecoveryState`
tracking in-progress compaction cycles across a restart (e.g., a
"compaction intent" log record) — rejected: unnecessary, since the
existing mechanism already tolerates every crash window safely and
without ambiguity (per the spec's own "not load-bearing" note, §7 of
this ADR / Decision 7) — adding one would be speculative complexity
with no correctness gap to justify it.

**Safety impact:** Directly safety-critical — this is the crash-
consistency guarantee for the entire feature. Mitigated by reusing
proven, already-certified mechanism wherever possible, and by
explicitly calling out the one branch that is *structurally* sound
but *empirically* untested, rather than silently trusting code review
alone.

**Performance impact:** None beyond the existing reconciliation sweep's
own already-measured startup cost (proportional to live SSTable count,
already the existing recovery cost profile, not changed by Compaction
existing).

**Tests required:** The full crash-window matrix from architecture
report §7, as real, deterministic fault-injection tests (Decision 15),
specifically including — as a **new, required** test, not assumed
already covered — a crash exactly after a `RemoveSstable` Manifest
edit is durable but before the corresponding file is deleted, followed
by a restart, verifying the orphan file is correctly swept and the
resulting live set is correct.

## 9. Decision: SSTable deletion protocol — deferred, non-blocking, `Arc`-refcount-gated

**Decision:** After an input SSTable's `RemoveSstable` edit is durable
and it has been spliced out of the live `sstables` list, its physical
file is deleted **only when its `Arc<SsTable>` strong count has
dropped to 1** (i.e., only the momentarily-held local clone from the
just-completed splice remains — proven safe as a *no-new-reader-can-
appear* invariant in the architecture report §16, because the splice
happens under the same write-lock that gates every possible source of
a *new* `Arc` clone). If the count is still `> 1` (an in-flight reader
still holds a clone), deletion is **deferred and retried later**
(e.g., on the next compaction cycle's own cleanup pass, or a bounded
periodic sweep) — **compaction never blocks synchronously waiting for
a reader to finish.**

**Reason:** LSM Engine Spec §5.2 step 8 specifies the `Arc`-strong-
count mechanism itself but does not specify blocking-vs-deferred
behavior when the count is still `> 1`; this ADR resolves that
explicitly rather than leaving it implicit. Blocking would risk
compaction stalling indefinitely under sustained read load (a
range scan can legitimately hold a table's `Arc` clone for the
scan's entire duration) — directly contradicting the brief's own
§8 requirement that "writers must not be blocked for the entire
duration of a large Compaction unless the specification explicitly
requires it" (extended here to: compaction's own *cleanup* step must
not stall the whole compaction cycle, or the next one, waiting on an
arbitrary reader).

**Alternatives considered:** *Synchronous blocking wait for `strong_
count == 1`* — rejected per Reason above. *Force-closing/invalidating
outstanding readers* — rejected outright: would require `unsafe`
lifetime manipulation or breaking an already-certified read guarantee
(a range scan must be allowed to run to completion against a
consistent view, `ADR-RE-001`), exactly what the brief's §16
explicitly forbids ("do not use unsafe lifetime tricks").

**Safety impact:** The file is never deleted while any reader could
still access it — the deferred-retry design is strictly more
conservative than a hypothetical immediate/forced deletion, at the
cost of transient extra disk usage (already accounted for in
Decision 1's "worst case ~2x" figure) until the retry succeeds.

**Performance impact:** In the common case (no long-running reader
holding the table), deletion happens promptly on the same cycle. In
the worst case (a reader holds it for a long time), disk usage stays
transiently elevated until that reader finishes — bounded by however
long the slowest concurrent reader takes, which the Read Engine's own
Increment 6/7 work already measured and improved (range scans no
longer take tens of seconds on the overlapping-key workload that used
to be the worst case).

**Tests required:** A dedicated test creating a long-lived `ReadView`/
cursor over a table, running a compaction that retires that exact
table, confirming (a) the read completes correctly and fully despite
the retiring compaction, (b) the file is *not* deleted while the read
is in progress (observable via the OS/`Arc::strong_count`), and (c)
the file *is* eventually deleted once the read completes and a
subsequent cleanup pass runs.

## 10. Decision: Concurrency model — brief write-lock splice, no global lock, serialized with flush

**Decision:** Compaction performs its k-way merge and output-file
construction with **no lock held at all** (mirroring flush's own
existing pattern, `src/lsm/mod.rs:1978-1984`), then takes the *same*
`sstables: Arc<RwLock<Vec<Arc<SsTable>>>>` write-lock flush already
uses, only for the brief final splice (remove N input `Arc`s, insert 1
output `Arc`). No new lock type, no global engine lock.

**Reason:** `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §8 derived this
directly from flush's own already-certified pattern — reusing the
identical shape means writers are never blocked for the duration of
the (potentially long) merge, only for the same brief, already-
measured-cheap splice flush's own publish step already performs.
Reusing the *same* lock (not a new one) automatically serializes
Compaction's splice against a concurrent flush's own splice, with no
new coordination primitive needed.

**Alternatives considered:** A dedicated `compaction_lock: Mutex<()>`
serializing compaction cycles against each other (relevant only if
Compaction ever becomes concurrent with itself, which Decision 14
explicitly defers) — not needed in this ADR's scope, since no trigger/
background-thread wiring exists yet; a single synchronous `compact()`
call has no concurrent-with-itself hazard to guard against until a
future increment introduces one. A global engine-wide lock covering
the whole merge — rejected per the brief's own explicit instruction
("do not assume a global engine lock is acceptable").

**Safety impact:** Readers are protected by the exact same `ReadView`/
`Arc`-clone guarantee that already makes concurrent-flush-during-a-scan
safe and certified (`range_scan_during_concurrent_flush_sees_a_
coherent_snapshot`) — extending an already-proven pattern, not
inventing a new one.

**Performance impact:** Positive — writers see no additional blocking
beyond what flush's own splice already costs them today.

**Tests required:** `range_scan_during_concurrent_compaction_sees_a_
coherent_snapshot` (direct analogue of the existing flush test);
concurrent `get`/`get_as_of`/`contains`/`range_scan`/`Snapshot`-
creation tests during an in-progress compaction (brief §19's full
concurrency list).

## 11. Decision: Storage-pressure behavior — observe only, never mutate

**Decision:** Compaction reads `LsmEngine::storage_state()` before
triggering/running a cycle and **defers/skips** triggering while
`StorageState::StorageFull` (and, conservatively, while `Storage
Pressure` too, since Compaction transiently *increases* disk usage
before it decreases it — Decision 1's ~2x worst case). Compaction
**never** writes to `storage_state`/`storage_pressure_events` — only
a successful flush's own success path may transition `StorageState`,
unchanged from today.

**Reason:** `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` §13 and
`src/lsm/mod.rs`'s own `freeze_locked` doc comment both establish, as
an already-ratified contract, that only flush's success path clears
`StorageFull` — architecture report §14 flagged the open question of
whether a successful *compaction* should also be allowed to, and this
ADR resolves it **conservatively: no**, for this phase. Reason:
granting Compaction write access to a state machine the Write Engine
explicitly reserves to itself is itself a protected-contract change,
requiring its own separate sign-off under the brief's own "do not
weaken any existing guarantee" instruction — and Compaction is a poor
candidate to attempt under confirmed storage exhaustion anyway, since
it needs *more* transient headroom, not less.

**Alternatives considered:** *Allow successful compaction to also
clear `StorageFull`* — rejected for this phase per Reason above (not
ruled out forever, just not decided unilaterally here); *ignore
`StorageState` entirely and let compaction attempt regardless* —
rejected: risks compaction itself triggering a *second*, compaction-
caused ENOSPC on top of an already-degraded storage state, actively
making the situation worse rather than waiting for flush's own
recovery path to clear it first.

**Safety impact:** Strictly conservative — Compaction can only ever
reduce its own footprint's contribution to storage risk by deferring,
never increase it by proceeding blindly.

**Performance impact:** Compaction cycles may be delayed during a
storage-pressure episode — acceptable, since availability (the Write
Engine's own already-ratified priority under `ADR-WE-SP-001`) already
takes precedence over compaction's own housekeeping goal.

**Tests required:** A compaction-triggered-while-`StorageFull` test
confirming it defers/skips rather than attempting and confirming it
never mutates `storage_state`/`storage_pressure_events` itself
(observable via the existing accessors).

## 12. Decision: Resource limits

**Decision:** Peak Compaction memory is bounded by **input table
count**, not total data volume — one `Arc<SsTable>` clone plus one
active `SsTableRangeCursor` (Increment 6's own owned-`Arc` persistent
cursor type, reused directly — see Decision 13) per input table for
the merge's duration, plus one in-progress output block (Decision 3's
streaming writer). File handles: one per input table (already open,
shared via `Arc`) plus one for the in-progress output — within the
Read Engine's own already-measured-and-certified "~1 handle per live
SSTable" budget. No new thread is created by the deterministic core
operation itself (Decision 14 defers trigger/thread wiring).

**Reason:** Directly derived from `PHASE_COMPACTION_ARCHITECTURE_
REPORT.md` §15, itself built from the Read Engine's own already-
certified, already-measured bounded-memory range-scan design
(`ADR-RE-001` §12) and Increment 6's persistent-cursor memory profile
(`PHASE_READ_ENGINE_INCREMENT7_SOAK.md` §3's own resource-check
precedent: 400 scans, zero handle/thread leak).

**Alternatives considered:** No alternative resource model was
considered necessary — Decision 3 (streaming writer) already
eliminates the one identified path to an unbounded-memory design
(materializing the whole merge output).

**Safety impact:** None new.

**Performance impact:** Bounded, predictable resource usage regardless
of total data volume — a positive, not a cost.

**Tests required:** A resource check directly analogous to the
existing `cursor_resource_check` benchmark section (`examples/read_
engine_bench.rs`) — repeated compaction cycles, verifying handle/
thread/RSS return to baseline, no leak.

## 13. Decision: Compaction API — smallest internal surface, no public API yet

**Decision:** A single, crate-private (`pub(crate)`, not `pub`)
function: `fn compact(inputs: Vec<Arc<SsTable>>, sstables_dir: &Path,
next_id: &AtomicU64, writer_config: &SsTableWriterConfig,
oldest_live_snapshot_seq: Option<u64>) -> Result<SstableMeta>` —
performs the k-way merge (reusing `SsTableRangeCursor`, Increment 6),
applies the retention rule (Decision 4), and writes the output via
the generalized streaming writer (Decision 3). No `CompactionPolicy`/
`CompactionPlan`/`CompactionJob` type is introduced in this ADR's
scope — those are trigger/scheduling concepts, and Decision 14
explicitly defers trigger wiring. A `CompactionStats` observability
type (Decision 16) is the one additional type this ADR does define,
since it is needed by `compact()`'s own return/side-channel contract,
not by a future trigger.

**Reason:** Per the brief's own explicit instruction ("do not add
public APIs without a real caller... prefer internal APIs unless the
specification requires public exposure") — no external caller exists
yet (Decision 14), so nothing beyond the deterministic core operation
itself should be public. `LsmEngine` itself does not yet gain a public
`compact()`/`trigger_compaction()` method in this ADR's scope.

**Alternatives considered:** Exposing `LsmEngine::compact_now()`
publicly, for manual/test-driven triggering ahead of automatic trigger
wiring — considered, and deferred rather than rejected outright: a
test-only or `#[cfg(any(test, feature = "test-util"))]`-gated entry
point (mirroring `set_flush_delay_for_test`'s own existing convention)
is reasonable for the implementation increment's own test suite to
add, but is an implementation-time decision, not an architecture
decision this ADR needs to resolve now.

**Safety impact:** None — crate-private, no external contract created.

**Performance impact:** None — API shape decision only.

**Tests required:** Unit tests directly against `compact()` as a
crate-internal function (the implementing increment's own test module,
`src/compaction/tests.rs` or similar) — the full matrix in Decision 17.

## 14. Decision: Compaction trigger — deferred, deterministic core first

**Decision:** This ADR defines and approves the deterministic core
`compact()` operation (Decision 13) only. **Automatic triggering**
(whether a background thread analogous to `spawn_flush_thread`, or a
synchronous post-flush check analogous to `freeze_locked`'s own
capacity check) is **explicitly deferred** to a separate, future,
human-approved implementation increment — not decided by this ADR.

**Reason:** Direct instruction from the brief ("do not introduce a
background Compaction thread yet unless the architecture requires it.
First define the deterministic core Compaction operation") — followed
exactly. `LsmConfig.compaction_trigger_count` (the config field named
in the original spec but absent from the current struct, per
architecture report §1) is a trigger-wiring concern, not a core-
operation concern, and is therefore also deferred to that future
increment, not added here.

**Alternatives considered:** Deciding the trigger mechanism now, ahead
of implementation — rejected per the brief's own explicit instruction;
premature relative to having a working, tested core operation to
trigger in the first place.

**Safety impact:** None — this decision narrows scope, it does not
introduce risk.

**Performance impact:** None assessed — no trigger cadence exists yet
to have a performance profile.

**Tests required:** None for this decision itself (it is a scope
boundary, not a behavior) — the deterministic core operation's own
tests (Decision 17) do not depend on trigger wiring existing.

## 15. Decision: Failure handling — mirror the existing `FlushFaultPoint`/`FlushIoFaultHook` pattern

**Decision:** Two new, Compaction-specific fault-injection types,
modeled directly on the existing, certified precedent: `CompactionFaultPoint`
(an enum of named crash points — e.g. `BeforeOutputWrite`,
`AfterOutputPublish` [output fsync+rename+`AddSstable` all durable,
before any `RemoveSstable`], `DuringRemoveSequence`, `AfterAllRemoves`
[before physical deletion]) + `install_compaction_fault_hook` (panic-
based, mirroring `FlushFaultPoint`/`install_flush_fault_hook`), and
`CompactionIoFaultHook` (synthetic `io::Error` injection for the
output-write step specifically, mirroring `FlushIoFaultHook`/`install_
flush_io_fault_hook`, for ENOSPC-during-compaction testing).

**Reason:** `PHASE_COMPACTION_ARCHITECTURE_REPORT.md` §19 found these
exact two patterns already exist, are already certified, and are the
established convention for "deterministic synchronization/fault
injection... no sleep-based race tests" (the brief's own explicit
§19 instruction) — inventing a third, different pattern would be
unjustified divergence from working precedent.

**Alternatives considered:** Sleep-based timing to induce races —
rejected outright per the brief's explicit instruction and this
project's own established convention throughout every phase to date.

**Safety impact:** None — test-only infrastructure, `#[cfg(any(test,
feature = "test-util"))]`-gated exactly like its two precedents,
never active in production.

**Performance impact:** None in production (compiled out / always
`None`-hook in the non-test path, identical to the existing pattern's
own already-measured negligible overhead).

**Tests required:** The full crash-window matrix (Decision 8/Decision
17), driven deterministically through these two hook types.

## 16. Decision: Observability — `CompactionStats`, no speculative metrics

**Decision:** A new `CompactionStats` struct (returned by `compact()`,
not a global running-total counter like `ReadStats` — each compaction
cycle produces its own stats), with exactly the fields the brief's
§22 lists and no others: `input_bytes`, `output_bytes`, `records_read`,
`records_retained`, `records_dropped`, `tombstones_dropped`, `duration`,
`temporary_disk_peak`. No `throughput`/`memory_peak` field is added
speculatively (`throughput` is trivially derivable by a caller from
`output_bytes`/`duration`; `memory_peak` is not something this ADR's
bounded-by-input-count design (Decision 12) makes a first-class
runtime-measured quantity — it is a static bound, not a per-run
observation).

**Reason:** Per the brief's own explicit instruction ("do not add
speculative metrics... define `CompactionStats` before implementation
if appropriate") — the field list is drawn directly and only from
what brief §22 enumerated, matching this project's own established
"precisely defined, one counting point per metric" convention
(`ReadStats`, certified, `ADR-RE-001` §12).

**Alternatives considered:** Reusing/extending `ReadStats` itself for
compaction metrics — rejected: `ReadStats` is a frozen, certified,
protected type (`PHASE_READ_ENGINE_CERTIFICATION.md` row 15); adding
fields to it would be exactly the kind of "modify the certified Read
Engine" action this ADR's own preamble forbids until Compaction has
its own separate, approved design (which this document now is) — a
wholly separate `CompactionStats` type avoids the question entirely.

**Safety impact:** None — observability only, never consulted by any
correctness decision (matching `ReadStats`'s own established
convention).

**Performance impact:** Negligible — a small number of counter
increments/one duration measurement per compaction cycle, not per
record in the hot path sense `ReadStats` is (compaction cycles are
inherently far less frequent than individual reads).

**Tests required:** A dedicated test asserting each field's exact
counting semantics against a known input (mirroring `read_stats_
counts_range_scan_calls_and_sstable_consultation_correctly`'s own
established style).

## 17. Decision: Test strategy — full matrix, built from existing precedent

**Decision:** Before any implementation, the following test matrix is
required (categories only — exact test names are an implementation-
time detail):

- **Structural**: single-table no-op (trigger at exactly `compaction_
  trigger_count`, one surviving table), two-table merge, many-table
  merge, overlapping key ranges, disjoint key ranges.
- **Version/tombstone**: the full worked truth table (Decision 4),
  delete/recreate, snapshot retention at every position in the truth
  table, `oldest_live_snapshot_seq()` exercised together with a real
  compaction run for the first time (architecture report §3's flagged
  gap).
- **Manifest**: `AddSstable`/`RemoveSstable` sequence correctness,
  idempotent replay.
- **Crash consistency**: every window in Decision 8's table, via
  `CompactionFaultPoint` (Decision 15) — including the specifically-
  flagged, currently-zero-coverage "removed-but-undeleted orphan"
  recovery branch.
- **Failure**: failed output write, `CompactionIoFaultHook`-injected
  fsync/ENOSPC failure, rename failure, corrupt input (fail closed,
  no partial output), corrupt output (caught by `SsTable::open`'s own
  re-validation on publish).
- **Concurrency**: concurrent `get`/`get_as_of`/`range_scan`/
  `Snapshot`-creation/flush during an in-progress compaction (Decision
  10/Decision 11), the long-lived-reader-vs-deferred-deletion test
  (Decision 9).
- **Storage pressure**: compaction deferred while `StorageFull`
  (Decision 11).
- **Differential**: compare pre-compaction and post-compaction logical
  reads (`get`/`get_as_of`/`contains`/`range_scan`) against an
  independent reference model — **never the production compaction
  algorithm as its own oracle** (`ADR-RE-001` §17's own already-
  established, certified principle, extended here).
- **Property-based**: random keys/versions/tombstones/snapshot
  sequences/SSTable partitions/compaction boundaries, asserting
  logical equivalence, sorted output, no lost visible value, no
  resurrected tombstone, no invalid snapshot visibility — using
  `proptest = "=1.11.0"` (already an exact-pinned dependency, no
  version change).

**Reason:** Directly enumerated from the brief's own §19/§20/§21,
cross-checked against existing precedent (architecture report §19-21)
to confirm every category has a reusable pattern already in this
codebase rather than needing to be invented.

**Alternatives considered:** N/A — this decision is the enumeration
itself.

**Safety impact:** This *is* the safety mitigation for every other
decision in this ADR — no implementation should proceed without this
matrix passing.

**Performance impact:** None (test infrastructure).

**Tests required:** N/A — self-referential.

---

## Final Decision Summary

All 17 decision areas above resolved. **No implementation performed.**
`src/compaction/mod.rs` unmodified. The next step is a human/
maintainer decision on whether to open a new, separately-scoped
Compaction implementation increment, informed by this ADR and
`PHASE_COMPACTION_ARCHITECTURE_REPORT.md` — not an automatic
continuation from this audit.

---

## Amendment 1 (2026-09-21): Increment 2 — automatic trigger + execution integration

**Context.** Decision 14 above deliberately deferred trigger wiring.
Increment 1 (`PHASE_COMPACTION_INCREMENT1_RESULTS.md`) then implemented
the deterministic core (`compact_once`/`should_compact`) with zero
production caller. Increment 2's brief required exactly one thing this
ADR had not yet resolved: **how** `compact_once`'s logic gets called
automatically, and everything that follows from that choice
(concurrency, shutdown, retry, storage-pressure interaction, default
rollout posture). This amendment records those decisions with the same
Decision/Reason/Alternatives/Safety/Performance/Tests-required
structure as the original 17 — nothing above is edited or
retroactively rewritten.

### A1. Decision: Execution model — background worker thread, not synchronous post-flush

**Decision:** A dedicated background compaction worker thread
(`spawn_compaction_thread`, `src/lsm/mod.rs`), started by `LsmEngine::
open` only when `LsmConfig.compaction_auto_trigger` is `true` (A5
below), mirroring `spawn_flush_thread`'s own existing shape as closely
as the two components' different jobs allow: a free function taking
cloned field `Arc`s (never `&LsmEngine`, since `open()` returns a bare
`Self` and a spawned thread cannot borrow it), driven by a channel, with
its own stop flag and `JoinHandle`.

**Reason.** The brief required this decision be grounded in *this
project's own actual contracts*, not external convention, and
explicitly named the criteria to check it against — each addressed
directly:

- **Writer blocking.** A synchronous post-flush check (running
  `compact_once` directly inline, on the flush thread, right after a
  publish — analogous to `freeze_locked`'s own inline capacity check)
  would block that same flush thread for the full duration of the
  merge (tens to hundreds of milliseconds measured this increment,
  §5 below, scaling with total live data volume, Decision 1's own
  acknowledged cost) before it could process the *next* queued
  immutable MemTable. Under sustained write load this directly risks
  `max_immutable_memtables` backpressure (`CapacityExceeded`,
  `PHASE4A_FAILURE_MODEL.md`) purely as a side effect of compaction's
  own housekeeping — an availability cost this codebase's own
  established priority (`ADR-WE-SP-001`: availability over
  housekeeping) does not accept implicitly. A background worker keeps
  the flush thread's own job (publish, checkpoint, purge) exactly as
  fast as it was before Increment 2, unconditionally.
- **Flush interaction.** The flush thread only ever sends a
  best-effort, non-blocking notification (`compaction_sender.try_send
  (CompactionMsg::MaybeCompact)`, dropped silently if the bounded(1)
  channel is already full) immediately after publishing — zero
  coupling to compaction's own duration.
- **Concurrent reads.** Unaffected either way — Decision 10's brief
  write-lock-splice-only concurrency model is identical regardless of
  which thread calls `compact_once_impl`.
- **Snapshot lifetime.** Unaffected — `oldest_live_snapshot_seq()`
  (Decision 5) is read fresh by whichever thread runs the merge; a
  background worker changes *when* that read happens, never its
  result's meaning.
- **Storage-pressure behavior.** Unaffected — Decision 11's
  observe-only gate is evaluated identically regardless of caller
  thread.
- **Compaction re-entry.** A background worker introduces the *first*
  real possibility of two overlapping trigger sources (a flush's own
  notification racing a manual `compact_once()` call, or the worker's
  own catch-up loop racing a fresh notification) — resolved by A2
  below, the smallest possible primitive, not a synchronous design's
  problem to solve differently.
- **Shutdown.** Resolved explicitly by A3 below — a synchronous
  design would have no separate shutdown contract to define at all
  (it would simply run inline), which is itself evidence a background
  worker is the more complex choice **only** where genuine new
  behavior (independent lifecycle, needing its own stop/join contract)
  actually exists, not complexity added without cause.
- **Failure propagation.** A synchronous inline call could let a
  compaction failure propagate into (or otherwise perturb) the flush
  thread's own error handling, a certified, protected path
  (`ADR-WE-SP-001`). A background worker fully isolates compaction's
  own failure handling (A4 below) from flush's, by construction —
  they are different threads with independent `catch_unwind`
  boundaries.
- **Resource ownership.** One additional, always-either-`Some`-or-
  `None` `JoinHandle` field, mirroring `flush_handle`'s own existing
  `Mutex<Option<JoinHandle<()>>>` pattern exactly — no new resource
  *kind* is introduced, only one more instance of an already-proven
  shape.

**Alternatives considered.** *Synchronous post-flush* (the "what
`freeze_locked` does" analogy) — rejected per the writer-blocking
analysis above, the single most concrete, measurable cost difference
between the two designs. *A dedicated `Condvar`-based worker instead
of a channel* — rejected: a bounded `mpsc::sync_channel` already gives
free coalescing (a full channel silently drops a redundant
notification) and a built-in blocking `recv_timeout` for the dual
wake-source design (A4), with no additional synchronization primitive
to reason about.

**Safety impact.** None beyond what A2–A6 individually account for —
this decision is the *shape* of the caller, not a new correctness
rule.

**Performance impact.** Strictly better than the rejected synchronous
alternative for writer latency (writers never wait on compaction);
worse than "no automatic trigger at all" only in the sense that a
background thread now exists and periodically wakes (bounded by the
existing `storage_pressure_retry_interval` cadence, no new timer
introduced — A4).

**Tests required.** Covered by §6 of `PHASE_COMPACTION_INCREMENT2_
RESULTS.md`'s own test list — automatic firing at/above threshold with
no manual call, no writer-blocking regression (the existing write-path
test suite, unmodified, still green).

### A2. Decision: Re-entrancy — one `AtomicBool` RAII guard, not a global lock

**Decision:** `CompactionRunGuard` (`src/lsm/mod.rs`) — a
`compare_exchange(false, true)` on one shared `Arc<AtomicBool>`
(`compaction_running`), acquired at the start of `compact_once_impl`'s
real work (after the trigger-count check, so "below threshold" and
"another compaction already running" are both legitimate, cheap,
`Ok(None)` skip reasons) and released via `Drop` on every exit path,
including `?`-propagated errors and panics unwound through
`catch_unwind`.

**Reason.** The brief explicitly required "the smallest primitive,"
naming a global engine lock as unacceptable. `compact_once_impl` is
now reachable from three places — the manual `LsmEngine::compact_once`
method, the background worker's own trigger-driven call, and the same
worker's own catch-up loop — any two of which could otherwise overlap
(a manual test call racing the worker, or the worker's catch-up loop
racing a fresh `MaybeCompact`-driven call it hasn't returned from yet).
One boolean is exactly sufficient: compaction cycles are never nested
or pipelined by design (Decision 1, full-merge, one cycle at a time),
so "is one already running" is the entire question that needs
answering.

**Alternatives considered.** *A `Mutex<()>` held for the cycle's
duration* — rejected: a blocking lock would make a losing caller
*wait* rather than cheaply skip, reintroducing exactly the kind of
implicit blocking A1's writer-blocking analysis rejected, just moved
to a different caller. *A global engine lock* — explicitly rejected by
the brief itself.

**Safety impact.** Guarantees at most one compaction cycle's merge/
write/Manifest-transition/splice sequence is ever in flight at a time,
without introducing any new blocking for any other engine operation
(readers, writers, snapshots are entirely unaffected — the guard only
gates compaction against compaction).

**Performance impact.** One uncontended `compare_exchange` per
attempted cycle — negligible, identical cost class to the existing
`storage_state` atomic checks already on this same path.

**Tests required.** `compaction_run_guard_permits_exactly_one_
concurrent_holder` (§6 of the results doc) — a direct stress test of
the primitive itself (16 threads, 500 acquire/release attempts each,
asserting the maximum observed concurrent holder count is exactly 1),
independent of engine/thread-timing considerations entirely.

### A3. Decision: Shutdown contract

**Decision:** `LsmEngine::shutdown()` (extended, `src/lsm/mod.rs`),
after its existing flush-thread shutdown sequence, does exactly:
`compaction_stop.store(true)`, then a **blocking** `compaction_sender.
send(CompactionMsg::Shutdown)` (not `try_send`), then joins the worker
handle if one exists. The worker's own loop checks `compaction_stop`
at the top of its catch-up loop (before starting a *new* cycle) but
never mid-cycle — an in-progress compaction is always allowed to run
to completion; it is never aborted.

**Reason, per point, exactly as the brief required this be determined
before coding:**

- **New work stops scheduling.** `compaction_stop` is set *before* the
  `Shutdown` message is sent, so by the time the worker could possibly
  observe the message, the stop flag is already visible (`Release`/
  `Acquire` ordering on both).
- **In-progress compaction policy.** Runs to completion, never
  aborted — the same reasoning as Decision 9's own "never force-close
  an in-flight operation" principle, applied to the compaction cycle
  itself rather than a reader holding one of its inputs.
- **No worker leak.** The `JoinHandle` is always joined before
  `shutdown()` returns (mirroring `flush_handle`'s own identical,
  already-certified pattern) — `Mutex<Option<JoinHandle<()>>>`'s
  `.take()` makes a second `shutdown()` call a safe no-op (`None`
  found, nothing to join), exactly like the flush thread's own
  contract.
- **No join deadlock.** The blocking `send` cannot deadlock: either
  the worker thread is alive and will drain the channel promptly
  (it returns to `recv_timeout` quickly whenever there is no real
  compaction work — a channel of capacity 1 has at most one item to
  drain), or the worker has already exited on its own, in which case
  `send` to a disconnected channel returns an `Err` immediately without
  blocking.
- **No partial publish.** Unaffected by shutdown specifically — this
  is Decision 7/Decision 8's own existing crash-consistency guarantee,
  which does not distinguish "the process crashed" from "the process
  shut down mid-cycle then a later `open()` reconciled state"; both
  are already-handled windows.
- **No lost Manifest state.** Same reasoning — an in-progress cycle's
  Manifest edits are each individually fsync'd durable as they happen
  (Decision 7), regardless of whether the *process* later continues
  running or shuts down.

**A real bug found and fixed while determining this contract**: an
earlier version of this shutdown code used `try_send(Shutdown)`
(mirroring the flush thread's own notification-style `try_send`
usage elsewhere) rather than a blocking `send`. Since the channel is
bounded to capacity 1, a `MaybeCompact` notification already sitting
unconsumed in the channel would make `try_send(Shutdown)` silently
fail to enqueue — the worker would then only notice the stop request
via its own periodic fallback tick (up to `storage_pressure_retry_
interval` later, default 5s). Under a property test issuing heavy
write load against dozens of engines, this compounded into a real,
measured, multi-minute slowdown that looked like a hang under an
external 60-second timeout probe — found empirically during this
increment's own stability verification, not hypothesized. Fixed by
switching specifically the `Shutdown` message (only) to a blocking
`send`, per the "no join deadlock" reasoning above.

**Alternatives considered.** *Abort an in-progress cycle on shutdown*
— rejected per Decision 9's own established principle, extended here.
*`try_send` for `Shutdown`* — the bug above; superseded by the fix,
not a live alternative.

**Safety impact.** Strictly positive — the fix closes a real
(non-correctness, but real-availability/latency) defect found this
increment.

**Performance impact.** `shutdown()` may now wait up to one in-flight
compaction cycle's own duration (§5's measured range) before
returning, when a cycle happens to be running at the moment of the
call — an accepted, bounded cost, consistent with "never abort
in-progress work" outweighing "shutdown must be instantaneous."

**Tests required.** `shutdown_lets_an_in_progress_automatic_
compaction_finish_before_returning` (§6 of the results doc) — an
injected, bounded delay inside the compaction fault hook gives
`shutdown()` a real, deterministic window in which a cycle is
genuinely in flight; asserts `shutdown()` returns promptly (no hang)
and the resulting state is never partially published.

### A4. Decision: Retry/backoff policy — no busy-retry, dual wake source

**Decision:** On `Err` (or a caught panic) from `compact_once_impl`,
the worker logs once (`eprintln!`) and does **not** retry immediately
— it simply lets its own outer loop return to waiting. That wait uses
`mpsc::Receiver::recv_timeout(fallback_interval)`, where
`fallback_interval` reuses the existing `LsmConfig.storage_pressure_
retry_interval` value (no new config field introduced). This gives
two independent wake sources unified into one code path: a real
`MaybeCompact` notification (sent by the flush thread right after any
new publish) and this periodic fallback tick — so a failed or
deferred cycle is retried either by the next real trigger, or, if
write traffic stops entirely, by the next fallback tick, whichever
comes first.

**Reason.** The brief explicitly forbade "an infinite retry loop,
busy-looping, storm, or log spam." A fixed, bounded fallback cadence
reusing an *existing* config value (rather than a new, speculative
one) satisfies "deterministic retry/backoff policy... documented in
the ADR" without inventing a second timer concept for one subsystem
to reason about. Reusing `storage_pressure_retry_interval`
specifically is deliberate, not arbitrary: both are "how often should
a background component re-check whether it's safe/useful to act
again" cadences, and a storage-pressure episode is precisely the kind
of condition (Decision 11) that would otherwise leave a compaction
permanently deferred with no future flush ever arriving to notify it
again (once `max_immutable_memtables` backpressure engages, writes —
and therefore flushes, and therefore `MaybeCompact` notifications —
can themselves stop).

**Alternatives considered.** *Exponential backoff* — rejected as
unnecessary complexity: a single fixed interval already bounds worst-
case retry latency, and compaction failures are not expected to be
frequent enough (Decision 8's crash-window analysis: every window is
already correctly recoverable) to need backoff's specific benefit
(avoiding pile-up under a *sustained* failure storm) — the "log once,
don't spam" requirement is already met by logging on `Err` exactly
once per attempt, not per retry-loop-iteration. *A dedicated new
`compaction_retry_interval` config field* — rejected per the brief's
own "avoid introducing a new, potentially-speculative config field"
instruction elsewhere in this same brief, and unnecessary given the
existing field's own cadence is already the right order of magnitude
for this purpose.

**Safety impact.** None new — a failed cycle simply leaves the live
set unchanged (Decision 11's own "observe only" pattern, generalized:
a failure never leaves compaction's own state half-applied, per
Decision 7/8's crash-consistency guarantee already covering every
mid-cycle failure window identically to a mid-cycle crash).

**Performance impact.** Bounded worst-case retry latency of one
`fallback_interval` (default 5s) after a failure with no subsequent
write traffic; effectively immediate retry (next `MaybeCompact`) under
any ongoing write load.

**Tests required.** `auto_trigger_retries_after_a_failed_attempt_via_
the_next_fallback_tick` (§6 of the results doc) — a synthetic,
fires-exactly-once `CompactionIoFaultHook` forces the first automatic
attempt to fail, then asserts the worker eventually retries and
succeeds with no manual intervention, and that the retry is genuinely
a second attempt (an attempt counter), not the first call somehow
succeeding.

### A5. Decision: `compaction_auto_trigger` — new config field, default `false`

**Decision:** `LsmConfig.compaction_auto_trigger: bool` (new field) —
`LsmEngine::open` spawns the background worker (A1) if and only if
this is `true`. **Default: `false`.** `compact_once()`/`should_
compact()` (the manual entry points) remain directly callable
regardless of this flag's value, unaffected either way — this is a
gate on automatic wiring only, not a new trigger *criterion*
(`compaction_trigger_count`, Decision 14, is unchanged).

**Reason.** This default was **reversed during this increment**, and
the reversal is recorded here rather than silently applied. The
initial implementation defaulted `compaction_auto_trigger` to `true`
(closer to what a "finished" feature's eventual production default
should probably be) reasoning that automatic compaction should simply
work once implemented. This was found, empirically, to be the wrong
default for *this* rollout step: `compaction_trigger_count`'s own
spec-mandated default (4) is low enough that a great many pre-
existing tests and fixtures across this crate — not just this phase's
own — legitimately accumulate more than 4 live SSTables in the course
of testing something else entirely (memory-growth regression tests,
range-scan fixtures, recovery matrices). Defaulting automatic
triggering *on* would have silently started compacting out from under
all of that already-certified, already-passing test surface the
moment this field shipped — directly the kind of "silently weaken an
existing guarantee" outcome this project's own standing principle
forbids. This was not a hypothetical risk: it was caught this
increment by two concrete, reproducible test failures the first time
`default = true` was tried — `compact_once_with_a_single_table_
reapplies_retention_correctly` (an Increment 1 test using `compaction_
trigger_count: 1`, which a live background worker raced against that
test's own manual `compact_once()` call) and a broader class of
Increment 1 fixture-building helpers that assume full, deterministic,
manual control over exactly when compaction runs. `false` is the
conservative, standard rollout posture for a new automatic subsystem:
the full capability is implemented, tested, and available to any
caller that explicitly opts in (this increment's own new `auto_
trigger_tests` module does exactly that), without silently changing
behavior for every existing caller that has not.

**Alternatives considered.** *Default `true`* — the initial choice;
reversed per the Reason above, with the two concrete failures as the
evidence. *A separate `#[cfg(test)]`-only default* — rejected:
would hide the real production default behind a build-configuration
difference, making `cargo test`'s own behavior diverge from what a
real caller using `LsmConfig::default()` gets, exactly the kind of
"test-only illusion of coverage" this project's own culture has
consistently rejected in every prior phase.

**Safety impact.** Strictly conservative — no existing caller's
observed behavior changes by taking this update, since the new
capability is opt-in.

**Performance impact.** None for existing callers (no worker spawned
unless explicitly requested); the documented cost profile above (A1,
A4) for any caller that does opt in.

**Tests required.** `small_flush_config`'s own Increment 1 fixture
helper and the one affected single-table-retention test both now
explicitly set `compaction_auto_trigger: false` with a doc comment
explaining why (protecting Increment 1's own test surface, unmodified
in its assertions); the new `auto_trigger_tests` module explicitly
opts in per test, per A1 above.

### A6. Note: a genuinely unbounded test-design hazard found and generalized (not a production defect)

Not a new architectural decision — recorded here because it shaped
several of this increment's own tests and is exactly the kind of
"measure, don't assume" finding this project's standing culture
requires surfacing rather than quietly working around. Several of
this increment's first-draft tests built a fixture by looping `put()`
calls against an **already-running** background worker until the test
thread's own `sstable_count()` poll happened to observe the live count
at or above `compaction_trigger_count`. Because the worker's own
reaction (capture → merge → splice) can complete inside the same
tens-of-milliseconds window the test thread needs to notice the count
crossed the threshold, this is a genuine race the test thread loses
far more often than it wins once the worker is already warm — in one
observed run, a fixture-rebuild loop needed 725 individual `put()`
calls (not 4) before the test thread's own check happened to land in
the narrow pre-splice window. This is not unbounded in the strict
mathematical sense (each retry is an independent, non-degenerate
chance of winning) but is unbounded *in practice* for test-timeout
purposes, and was traced directly to one specific test hanging past a
60-second external timeout probe during this increment's own
verification. **Fix, applied uniformly across every affected test**:
build any fixture that must reach or exceed `compaction_trigger_count`
*offline* first (`compaction_auto_trigger: false`, using Increment 1's
own already-proven-deterministic fixture-building helper), then reopen
with the worker enabled to observe its real, automatic reaction — a
one-directional, monotonic wait (the count only ever goes *down* once
triggered) rather than a race to observe it hold *at* a value against
a thread also trying to reduce it. No production code was implicated
or changed by this finding — it is purely a test-construction hazard,
specific to writing tests *against* an automatic system whose whole
job is to react to the same condition the test is trying to observe.
