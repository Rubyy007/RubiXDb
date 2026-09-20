# RubiXDB — Read Engine Architecture Decision Record

**ADR ID:** ADR-RE-001

**Status:** Proposed for Review — **no implementation in this document or this step**

**Date:** 2026-09-20

**Scope:** Read Engine only (point lookup hardening, `range_scan`, `snapshot()`, `contains()`, read observability)

**Prepared from:** `PHASE_READ_ENGINE_ARCHITECTURE_REPORT.md` (the read-only audit this ADR builds on — every fact cited below traces to that report or to a fresh, direct source/spec check performed while writing this ADR, not to assumption), `RubixDB-LSM-Engine-Specification-v1.0.md`, `RubixDB-Architecture-Specification-v1.0.md`, `RUBIC_SSTABLE_FORMAT_SPECIFICATION.md`, `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md`, `PHASE5_MANIFEST_ARCHITECTURE.md`, `PHASE_WRITE_ENGINE_CERTIFICATION.md`.

**Hard safety rule acknowledged and honored throughout:** the certified Write Engine (WAL format/recovery, Group Commit, Dedicated Batch Coordinator, MemTable durability, SSTable publication, Manifest durability, checkpoint, WAL purge) is a protected dependency. Every decision below was checked against this rule; none requires changing any of those semantics. Where a decision touches a write-path *type* (e.g., `LsmEngine`'s fields), it is additive only — no existing field, method signature, or on-disk format changes.

---

## 0. What this document is and is not

This is the **contract document** required before any Read Engine implementation begins (per the phase brief's own §3/§28). It resolves the 13 named decisions, each with Decision / Reason / Alternatives / Safety impact / Performance impact / Tests required, plus the exact files/tests/benchmarks/certification gates the phase brief's §28 also requires.

**Nothing in this document has been implemented.** `git status` at the end of this session shows no production source file changed by this step (verified in §12).

---

## 1. Decision: ReadView model

**Decision:** Two different consistency mechanisms for two different operation shapes, not one mechanism for both:

- **Point lookups (`get`/`get_as_of`) keep the existing sequential-per-source-lock pattern.** No change to `LsmEngine::get_as_of`'s current structure (`src/lsm/mod.rs:685-716`): take `active`'s read lock, check, release; take `immutables`'s read lock, check, release; take `sstables`'s read lock, check, release; return on first hit.
- **`range_scan` (new) captures a formal `ReadView` once, at call start, before any merge work happens**: read-lock `immutables` just long enough to clone its `VecDeque<Arc<MemTable>>` into a local `Vec<Arc<MemTable>>` (an `Arc` clone is a refcount bump, not a data copy), release; read-lock `sstables` just long enough to clone its `Vec<Arc<SsTable>>` into a local `Vec<Arc<SsTable>>`, release; read-lock `active` to extract the requested key range into a local sorted buffer (see §3), release. The entire subsequent k-way merge (§5) runs against these three captured, lock-free, Arc-stable references — a concurrent flush during the scan cannot alter what this specific scan sees, because the `Arc`s it already holds keep their pointed-to `MemTable`/`SsTable` objects alive and unchanged regardless of what the live `LsmEngine.immutables`/`sstables` lists do afterward.

**Reason:** `RubixDB-LSM-Engine-Specification-v1.0.md` §4.2 describes a single combined `ReadView` capture for reads generally. The architecture report (§8) traced, precisely, why the *current* sequential-lock pattern is safe for point lookups specifically — the flush thread publishes to `sstables` (step 4) before removing from `immutables` (step 9), so a racing point lookup always finds the data in whichever of the two sources it happens to check, never neither. That argument is scoped to a single, effectively-instantaneous per-source check; it does not extend to a `range_scan`, which iterates for a duration and must not see two different, inconsistent versions of "which immutables/SSTables exist" at different points in its own iteration. A full spec-literal `ReadView` (capture all three, release all locks, then work) is the correct mechanism specifically where the current pattern's safety argument stops applying — not needed where it already holds.

**Alternatives considered:**
- *Full `ReadView` for point lookups too*: rejected — no evidence of a correctness gap for point lookups (the traced argument holds), and it would add an `Arc`-clone-and-drop cost to the hottest, highest-frequency read path for no measured benefit. Per the phase brief's own §2/§23 ("do not add an optimization — or a cost — before measuring/justifying it"), this is deferred unless the new concurrent-flush-read test (§8 below) finds a real gap.
- *Hold `active`'s lock for the whole scan duration*: rejected — would block every writer for the scan's entire duration, violating "bounded per-request" cost and this project's existing single-writer-never-blocks-on-reads precedent (`PHASE4B_ADR.md`'s flush-thread design explicitly avoids blocking writers on background work).
- *Snapshot `active`'s full contents (not just the requested range) into an owned copy*: rejected — unbounded memory relative to `active`'s total size regardless of how narrow the scan's key range is; extracting only the requested range keeps the snapshot cost proportional to the scan's own bound, not to unrelated write volume.

**Safety impact:** Eliminates the one open question the architecture report flagged (§8/§14.2) for the operation shape (range scans) where it actually mattered; point lookups are unchanged, so no regression risk to the already-certified read behavior they already exhibit.

**Performance impact:** Point lookups: zero change. Range scans: one extra pair of short lock-acquire/`Arc`-clone/release operations per source at scan start (three total), then zero further locking for the scan's duration — strictly cheaper than the current per-item locking `range_scan_raw`/`MemTable::range` would need if driven per-step from live structures.

**Tests required:** The missing concurrent-flush-read test named in the architecture report (§8/§16 item 1), generalized to also cover a `range_scan` in progress during a flush (§4/§8 of this phase's own brief) — see §7 of this ADR.

---

## 2. Decision: snapshot model

**Decision:** Introduce `pub struct Snapshot { seq: u64, registry: Arc<SnapshotRegistry> }` (name TBD at implementation time, shape fixed here), obtained via `LsmEngine::snapshot(&self) -> Snapshot`. Internally, `LsmEngine` gains one new field, `snapshot_registry: Arc<SnapshotRegistry>`, where `SnapshotRegistry` wraps a `Mutex<BTreeMap<u64, u64>>` mapping `seq -> outstanding_count` (a multiset, since two callers can independently snapshot the same `seq`). `Snapshot::seq(&self) -> u64` exposes the watermark for use with `get_as_of`/`range_scan`. `Snapshot`'s `Drop` impl decrements its entry in the registry (removing it once the count hits 0). `LsmEngine::oldest_live_snapshot_seq(&self) -> Option<u64>` exposes the registry's minimum key — the one piece of information a future Compaction will need ("never remove a version still needed by the oldest live snapshot") without this phase having to build Compaction itself.

**Reason:** The phase brief is explicit: "Design snapshot support so that future Compaction will not require an API-breaking change" and "ensure the Read API is compatible with the future snapshot-ref model" — while also explicit that Compaction itself must not be built now. Building the registration bookkeeping (not just the type shape) is small, cheap (one `Mutex<BTreeMap>`, bounded by the number of concurrently-live snapshots, not by data volume), and independently testable today (take two snapshots, drop one, assert `oldest_live_snapshot_seq()` reflects the other; drop both, assert `None`) without needing Compaction to exist as a consumer. Building only the type shape and deferring the bookkeeping would satisfy the letter of "don't implement Compaction" but not the substance of "must not require an API-breaking change" — the registration API (`snapshot()`/`Drop`/`oldest_live_snapshot_seq()`) *is* the part of the surface Compaction will actually call; sketching an unregistered type and adding registration later would be the breaking change this decision exists to avoid.

**Alternatives considered:**
- *Only `snapshot_seq() -> u64` (today's bare watermark), no handle type*: rejected — this is exactly the gap the architecture report flagged (§7, §14.3); it cannot express "this seq must stay readable until I'm done with it," which Compaction will need.
- *A handle type with no registration, added later*: rejected per Reason above — this is the actual breaking change risk.
- *Full MVCC-style snapshot isolation across all future engines*: out of scope — `RubixDB-Architecture-Specification-v1.0.md`'s multi-engine/partition abstraction (§3-§19) does not exist in this codebase (confirmed in the architecture report's scope note) and is not being built here.

**Safety impact:** Purely additive (new field, new type, new methods) — no existing method's signature or behavior changes. `get_as_of`/`range_scan` already take an explicit `as_of_seq: u64`; `Snapshot::seq()` is just a safer way to obtain and hold that value across multiple calls, not a new read mechanism.

**Performance impact:** `snapshot()`: one `Mutex` lock + `BTreeMap` insert, O(log n) in outstanding-snapshot count (typically tiny). `Drop`: same, O(log n). Zero cost for any caller that never calls `snapshot()`.

**Tests required:** Registration/deregistration correctness (multiset increment/decrement, `oldest_live_snapshot_seq()` correctness across overlapping snapshots), `Snapshot::seq()` usable with both `get_as_of` and `range_scan` and returning results consistent with the watermark at the time `snapshot()` was called even after later writes.

---

## 3. Decision: `range_scan` contract

**Decision:**

```rust
pub fn range_scan(
    &self,
    start: std::ops::Bound<&[u8]>,
    end: std::ops::Bound<&[u8]>,
    as_of_seq: u64,
) -> RangeScanIter<'_>
```

returning a lazy iterator, `Item = Result<(Vec<u8>, Vec<u8>)>` (key, value — tombstones never yielded, see §6), sorted ascending by key, exactly one entry per logical key. `Bound<&[u8]>` matches `MemTable::range`'s and `SsTable::range_scan_raw`'s existing signatures exactly (verified, not assumed: both already take `Bound<&[u8]>` for `start`/`end` — `src/memtable/mod.rs:174-178`, `src/sstable/reader.rs:233-237`) — this is the established convention in this codebase, not a new one being introduced. `as_of_seq` matches `get_as_of`'s existing parameter exactly, for the same reason. A convenience `LsmEngine::range(start, end)` calling `range_scan(start, end, u64::MAX)` mirrors the existing `get`/`get_as_of` relationship (`get` already delegates to `get_as_of` with `u64::MAX`, `src/lsm/mod.rs:671`).

The iterator captures its `ReadView` (§1) once, on construction (inside `range_scan`, before returning), not lazily on first `.next()` — so the snapshot instant is the call to `range_scan` itself, matching a caller's intuitive expectation and matching when `Snapshot::seq()` would have been read if the caller passed one.

**Reason:** Reuses two already-implemented, already-tested building blocks (`MemTable::range`, `SsTable::range_scan_raw`) rather than inventing new source-level iteration; matches their existing type signatures exactly so the merge layer (§5) is straightforward glue, not a parallel reimplementation.

**Alternatives considered:**
- *`Vec<u8>` bounds instead of `Bound<&[u8]>`*: rejected — would force an allocation for every call and diverge from the existing source-level convention for no benefit.
- *Eager (`Vec`-returning) `range_scan` instead of a lazy iterator*: rejected — unbounded memory for a large range, directly contradicting §14/§18's "no entire-dataset materialization" / "bounded per-request memory" requirements.
- *Snapshot captured lazily on first `.next()` instead of on construction*: rejected — makes the visible snapshot instant depend on when the caller first polls the iterator rather than when they asked for it, a surprising and hard-to-reason-about consistency model.

**Safety impact:** New method, no interaction with any existing method's behavior.

**Performance impact:** See §1 (ReadView capture cost) and §16 (read amplification — to be measured, not assumed, per §15/§23 of the phase brief).

**Tests required:** See §7 (correctness), §8/§9 of the phase brief's own numbering (concurrency), §10 (corruption propagation through the iterator).

---

## 4. Decision: merge ordering

**Decision:** A k-way merge using a binary min-heap keyed by `(user_key ascending, source_recency_rank ascending)`, where `source_recency_rank` is `0` for `active`, `1..=immutables.len()` for immutables (newest immutable = rank 1), and beyond that for SSTables (newest SSTable = next rank), exactly mirroring the existing recency order `get_as_of` already checks (`active` → immutables newest-first → SSTables newest-first, confirmed `src/lsm/mod.rs:685-716`). Each of the `2 + immutables.len() + sstables.len()` source iterators (one `MemTable::range` slice, one iterator per immutable, one `SsTable::range_scan_raw` per SSTable) contributes its current head entry to the heap. The merge repeatedly pops the smallest `(key, recency_rank)` pair; for a run of popped entries sharing the same key, only the **first-popped** (i.e., newest-source) entry is kept as that key's winner, and every subsequent same-key entry from an older source is discarded without being read further (each discarded source iterator is then advanced past that key and its head re-pushed).

**Reason:** This is the standard merging-iterator algorithm used by every production LSM implementation for exactly this problem (the mechanism, not a specific codebase, is cited as prior art — RocksDB's/LevelDB's `MergingIterator` follow the identical shape: a heap ordered primarily by key, tie broken by source recency). It generalizes the *already-correct, already-tested* point-lookup recency rule (§9 of the architecture report) to the multi-key case without inventing new semantics — "first hit wins" becomes "first-popped-per-key wins," the same rule applied across a sorted stream instead of a single key.

**Alternatives considered:**
- *Concatenate + sort + dedupe*: rejected — the phase brief explicitly forbids this ("must NOT concatenate source iterators"), and it would require materializing the full range before returning the first result, violating boundedness.
- *Merge sources pairwise (active vs. immutables, then result vs. sstables, ...)*: rejected — correct but effectively a heap of size 2 applied `N` times instead of one heap of size `N`; strictly more comparisons and more intermediate allocation for the same result, no advantage identified.

**Safety impact:** None to existing code — new algorithm operating only on the new `range_scan` path.

**Performance impact:** `O(log N)` per popped entry where `N` = number of active source iterators (bounded by `1 + immutables.len() + sstables.len()`, itself bounded by `max_immutable_memtables` and the live SSTable count) — standard, well-understood cost, to be measured per §16 (read amplification: candidate SSTables/blocks touched) rather than asserted.

**Tests required:** §12 (differential testing against an independent reference model — this is precisely the property a reference-model oracle is needed for, since "the merge is correct" is not something the production algorithm can prove about itself).

---

## 5. Decision: version resolution

**Decision:** Identical rule to the existing, already-certified point-lookup rule, applied per-key inside the merge (§4): the visible version of a key is the one with the highest `seq <= as_of_seq` found across every source, where "found first" in recency order already guarantees "found first" implies "newest" (no explicit seq comparison across sources is needed, exactly as `get_as_of` already relies on structural ordering rather than seq comparison — confirmed `src/lsm/mod.rs:685-716`, no seq comparison appears in that function). Within a single source's own multi-version run for one key (e.g., `active`'s `BTreeMap<(key,seq), _>` can hold several versions of the same key), the source's own existing per-source logic already returns the right one (`MemTable::get_as_of`'s `range(...).next_back()`, `SsTable::get_versioned`'s explicit "highest seq <= as_of_seq" scan) — `range_scan`'s merge only needs the *winning* entry from each source's contribution at a given key, which `range()`/`range_scan_raw()` do not pre-collapse (they yield every version in range, per §3's earlier confirmation of `RangeScanRaw`'s `Item` type including every version) — so version collapse within a single source's own same-key run must also happen in the merge layer, not assumed to be pre-done by the source iterators.

**Reason:** Reuses the exact rule already proven correct at the point-lookup layer; the only new work is applying it across a sorted stream instead of one key, and explicitly handling that `range()`/`range_scan_raw()` are lower-level (all-versions) iterators than `get`/`get_as_of` (single-resolved-version) — a distinction this decision states explicitly rather than leaving implicit (matching the phase brief's "explicitly record every discrepancy" instruction).

**Alternatives considered:** None substantive — this is the only rule consistent with the already-certified point-lookup behavior; inventing a different rule for `range_scan` would make `get(k)` and a `range_scan` containing `k` potentially disagree, which is not an acceptable design.

**Safety impact:** None — restates the existing rule precisely.

**Performance impact:** Negligible additional cost (comparing consecutive popped entries' keys) over the base merge cost in §4.

**Tests required:** §12 differential test's core property ("`get_as_of(k, s)` and iterating `range_scan` to find `k` must agree, for every `k`/`s` the reference model can generate").

---

## 6. Decision: tombstone semantics

**Decision:** Unchanged rule, generalized: after version resolution (§5) determines a key's winning entry, if that entry is `MemtableValue::Tombstone`/`RecordValue::Tombstone`, the key is **not yielded** by `range_scan` at all (not yielded as a deleted-marker, not yielded with a sentinel value) — exactly matching `get`/`get_as_of`'s existing collapse-to-`None` behavior (`resolve`/`resolve_sstable`, confirmed applied "once, at the very end," `src/lsm/mod.rs`), so a caller iterating a `range_scan` sees the identical logical result set they would get from calling `get`/`get_as_of` on every key in the range individually.

**Reason:** API consistency with the already-certified point lookup; the phase brief itself lists "must NOT resurrect tombstoned keys" as a hard requirement, and this decision satisfies it by construction (a tombstoned key literally never reaches the iterator's output).

**Alternatives considered:** *Yield tombstones as an explicit `(key, None)` entry* — rejected as inconsistent with `get`'s existing `Ok(None)` (not `Ok(Some(None))`) shape, and no identified caller need for "I want to see which keys were explicitly deleted vs. never existed" within the current API surface (no spec requirement for this either).

**Safety impact:** None — restates the existing rule.

**Performance impact:** None beyond §5's cost.

**Tests required:** Covered by §12/§13 (differential and property tests both must include DELETE operations in their generated workloads, per the phase brief's own §12/§13 instructions).

---

## 7. Decision: corruption semantics

**Decision:** `range_scan` preserves `SsTable::range_scan_raw`'s existing, already-tested contract exactly: on the first corrupted block encountered by any underlying source iterator, that iterator yields exactly one `Err(Corruption)` and the **entire merge stops** — the `RangeScanIter` yields that one `Err` as its next item and then yields `None` (ends) on all subsequent polls, never attempting to skip past the corruption and continue with other sources' data for keys beyond that point. `I/O` errors (`Err(Io(..))`) from a source's underlying file read propagate identically.

**Reason:** This is not a new decision so much as a refusal to invent a *different* one — the phase brief explicitly requires fail-closed behavior ("no read operation should silently return incorrect data"), and the existing SSTable-layer precedent (`range_scan_raw`, confirmed in the architecture report §2) already establishes exactly this "stop on first corruption, never partial-skip" behavior for the building block `range_scan`'s merge layer sits on top of. Adopting a *more lenient* engine-level policy (e.g., "skip the corrupted table, keep merging the others") would silently produce an incomplete result set for a range that legitimately had data in the corrupted region — exactly the "silently return incorrect data" the brief forbids, even though no single value would be wrong, the *set* returned would be wrong by omission.

**Alternatives considered:** *Skip the corrupted source and continue merging the rest, returning a partial-but-flagged result*: rejected per Reason above — "flagged" partial results require a new result shape (`Result<Vec<Result<Item>>>`-style) with no precedent anywhere else in this codebase's read or write path, and the brief's own instruction is unambiguous ("fail closed").

**Safety impact:** Directly implements the brief's fail-closed requirement.

**Performance impact:** None (the failure path is by definition off the common path).

**Tests required:** §10 corruption matrix (this ADR's §7 below), one entry per corruption class from the architecture report's §10 table, run against `range_scan` specifically (not just `get`/`get_as_of`, which the architecture report already covers), asserting the exact error variant, not "some `Err`."

---

## 8. Decision: concurrent flush visibility

**Decision:** Governed entirely by §1 (`ReadView` for `range_scan`, existing traced-safe sequential pattern for point lookups) — no separate mechanism. The **new deterministic test** required by the phase brief's own §8 (and by the architecture report's §8/§16 item 1, already identified as a real, currently-missing test) will be built using this codebase's **existing, established fault-injection/timing-control precedent** — `FlushFaultPoint`/`install_flush_fault_hook` (`src/lsm/mod.rs`, already used by `flush_panic_at_every_fault_point`-style tests) and `set_flush_delay_for_test` (already used by `immutable_backpressure_rejects_further_freezes_past_the_limit`/`memory_accounting_remains_correct_across_freeze`) — **not** a new mechanism and **not** sleep-based timing, matching the phase brief's explicit instruction ("Use barriers/latches/test hooks... Do not rely only on sleeps"). Concretely: a test thread starts a `range_scan`/`get` concurrently with a `put` large enough to trigger a freeze+flush, with a flush fault hook installed at `FlushFaultPoint::AfterSstablePublish` (the exact instant between "SSTable added to `sstables`" and "entry removed from `immutables`," i.e., the transition window traced in §1/the architecture report's §8) that blocks the flush thread on a channel/condvar until the test's read has definitely started (for a `range_scan`) or completed (for a point lookup, to assert its outcome is one of the two valid answers, never a third, impossible one).

**Reason:** Reuses proven infrastructure instead of inventing new test machinery; directly targets the exact transition window the safety argument in §1 depends on, rather than a generic "sleep and hope" race.

**Alternatives considered:** *A new, dedicated concurrency-test-only synchronization primitive*: rejected — `FlushFaultPoint` already exists for precisely this purpose (controlling exact flush-thread timing from a test) and adding a second, parallel mechanism would be needless duplication.

**Safety impact:** Test-only; no production code path affected.

**Performance impact:** None (test-only, and `FlushFaultPoint`'s hook check is already a cheap no-op in production per its own existing doc comment).

**Tests required:** This decision *is* the test requirement — see §7 of this ADR (exact test list) item 1.

---

## 9. Decision: SSTable visibility authority

**Decision:** Unchanged. `range_scan`/`get`/`contains` consult only `LsmEngine.sstables` (via the `ReadView` capture in §1 for `range_scan`, or the direct lock-scoped read for point lookups) — never the filesystem directly, never `ManifestState` (confirmed not retained after `open()`, architecture report §3), never any structure other than the already Manifest-reconciled, footer-validated `Arc<RwLock<Vec<Arc<SsTable>>>>` that already exists.

**Reason:** This is already correct and already the sole authority (`PHASE5_MANIFEST_ARCHITECTURE.md` §1/§6, confirmed in the architecture report); there is no reason for a Read Engine phase to introduce a second path to SSTable visibility.

**Alternatives considered:** None — no alternative was identified that wouldn't reintroduce the "orphan file treated as live" risk the Manifest-authority design specifically exists to prevent.

**Safety impact:** Preserves an already-certified invariant.

**Performance impact:** None (no change).

**Tests required:** None new — already covered (`missing_live_sstable_fails_closed_on_open`, architecture report §10).

---

## 10. Decision: `contains()` — included

**Decision:** Include `pub fn contains(&self, key: &[u8], as_of_seq: u64) -> Result<bool>`. Implementation reuses `get_versioned`'s existing bloom-check → index-lookup → block-read → key-scan path but returns `Ok(true)` the instant a matching, non-tombstone entry is found — **without** cloning/returning the value bytes — and `Ok(false)` for both "genuinely absent" and "found but tombstoned" (matching `get`'s existing `Ok(None)` collapse for the same two cases, so `contains(k, s) == get_as_of(k, s)?.is_some()` is a documented, tested invariant, not just an implied one).

**Reason:** The phase brief's own §10/§14.6 instruction ("only worth adding if it can be meaningfully cheaper than `get(key).is_some()`") is satisfiable and real, but narrower than it might first appear: the architecture report already confirmed a bloom-negative *miss* is already near-zero-cost via `get` today (bloom check short-circuits before any block read) — so `contains`'s real, measurable win is specifically **avoiding the value-byte allocation/copy on a hit**, not making misses cheaper (they're already cheap). This is included because it is cheap to build (reuses existing logic, no new I/O path) and the benefit, while narrower than "faster in general," is real and measurable — subject to §23's rule (benchmark before/alongside, not instead of, building it): the benchmark plan (§12 of this ADR / §15 of the architecture report) explicitly includes a `contains`-vs-`get(...).is_some()` comparison at hit and miss cases both, so the claimed benefit is verified, not assumed, before this method is presented as "done."

**Alternatives considered:** *Exclude, since `get(key).is_some()` already exists*: considered seriously (this is the phase brief's own suggested default absent a justified need) — not chosen because the hit-case value-copy avoidance is a real, identifiable, cheap-to-build win with a concrete falsifiable benchmark attached, not a speculative one.

**Safety impact:** None — read-only, no new error paths beyond what `get_versioned` already has.

**Performance impact:** To be measured (§12 of this ADR) — expected improvement only for hit-case, large-value workloads; expected to be a no-op (or possibly a small net negative from the extra method-call indirection) for miss-case/small-value workloads, and the report on this must say so honestly either way.

**Tests required:** `contains(k,s) == get_as_of(k,s)?.is_some()` property test (generalizes over the existing differential-model workload, §12), plus the benchmark comparison named above.

---

## 11. Decision: `batch_get()` — deferred, not included this phase

**Decision:** Do not build `batch_get` in this phase. `LsmEngine::get`/`get_as_of` remain the only per-key entry points; a caller needing several keys calls them in a loop.

**Reason:** Neither `RubixDB-Architecture-Specification-v1.0.md` nor `RubixDB-LSM-Engine-Specification-v1.0.md` requires it (confirmed, architecture report §7). No current caller in this codebase needs it. The phase brief's own §14.6 instruction is explicit: "confirm there's an actual caller/use case before adding either [`contains`/`batch_get`]" — `contains` clears that bar (§10 above), `batch_get` does not: without a real caller, its two most consequential design questions (does it provide any atomicity/consistency guarantee across the batch, or is it purely "loop `get` and collect"? does a single corrupt/missing key fail the whole batch or return a partial result?) have no forcing use case to decide them against, and deciding them speculatively risks exactly the kind of invented, unvalidated API surface this project's rigor convention warns against.

**Alternatives considered:** *Build a minimal "loop and collect, no atomicity" version now*: rejected — even the "no atomicity" framing is itself a design decision with no current consumer to validate it against; better to defer until a real need states its own requirements.

**Safety impact:** None (nothing built).

**Performance impact:** None (nothing built) — note a future `batch_get` could in principle batch I/O more efficiently than a `get` loop (e.g., grouping by SSTable to avoid redundant per-call bloom/index overhead per table), which is itself an argument for waiting until real usage patterns are known rather than guessing the right batching strategy now.

**Tests required:** None (nothing built). Re-open this decision if/when a real caller need appears.

---

## 12. Decision: memory ownership (benchmark instrumentation + no double-retention)

**Decision:**
1. **No double-retention, enforced by construction, not just by policy**: `range_scan`'s `ReadView` (§1) holds only `Arc` clones of already-existing `MemTable`/`SsTable` objects — it is structurally impossible for it to duplicate the ~176-178 KB/SSTable Bloom-filter-plus-index memory `PHASE_WRITE_ENGINE_MEMORY_INVESTIGATION.md` already measured and attributed to `SsTable`'s own fields, because nothing in this design constructs a second `BloomFilter`/`Vec<IndexEntry>` anywhere. This is a structural guarantee to verify in code review at implementation time (confirm no new struct duplicates `bloom`/`index`), not merely a stated intention.
2. **Read instrumentation** (§12/§17 of the phase brief): a new `ReadStats` struct, mirroring the existing `BatchCoordinatorStats`/`capacity_pressure_events()` observability convention already established in this codebase (`src/execution/batch_coordinator.rs`, `src/lsm/mod.rs`'s `capacity_pressure_events: AtomicU64`) — cheap `Relaxed`-ordering atomics on `LsmEngine`, incremented at well-defined points: `read_requests`, `read_hits`, `read_misses`, `bloom_negatives`, `blocks_read` (needs a new counter threaded through `SsTable`/`read_block`, confirmed by the architecture report §14.5 as a genuinely new, currently-absent instrumentation point — not a reuse of an existing counter), `sstables_consulted`. Exposed via `LsmEngine::read_stats(&self) -> ReadStats`, matching `pool_stats()`'s existing shape.
3. **No cache added this phase**: per §14/§23 of the phase brief ("do not add caching yet," "do not introduce... cache... without benchmark evidence") — the benchmark plan (§15 of the architecture report, §12 test list below) must produce a real measured baseline before any caching discussion is reopened, and reopening it is explicitly out of scope for this ADR.

**Reason:** Directly implements the phase brief's §14 requirement and the architecture report's own §13 warning, now made concrete and structurally enforced rather than just stated as a goal.

**Alternatives considered:** *A per-request block cache "just for range scans"*: rejected outright per the phase brief's explicit "do not add caching yet" instruction — not weighed as a live option, named here only to record that it was considered and rejected on instruction, not on missing merit.

**Safety impact:** None (read-only additions).

**Performance impact:** Atomics on the hot path are `Relaxed` `fetch_add`s — the same cost class already accepted elsewhere in this codebase's hot paths (`capacity_pressure_events`, `storage_pressure_events`); `blocks_read`'s new counter is the only genuinely new hot-path touch point and will be measured (§15) to confirm it doesn't materially change point-lookup latency, per this project's own "don't add hot-path instrumentation without justification" convention (already stated in the ADR-WE-SP-001 precedent for `storage_pressure_events`).

**Tests required:** `ReadStats` correctness (counters increment exactly once per matching event, verified in a controlled single-threaded test), benchmark comparison with/without the new `blocks_read` counter to confirm negligible overhead.

---

## 13. Decision: API scope for this phase

**Decision — final surface for this phase:**

| Method | Status |
|---|---|
| `get(key) -> Result<GetResult>` | Existing, unchanged, regression-hardened (new tests, no behavior change) |
| `get_as_of(key, as_of_seq) -> Result<GetResult>` | Existing, unchanged, regression-hardened |
| `range_scan(start, end, as_of_seq) -> RangeScanIter` | **New**, per §3-§7 |
| `range(start, end) -> RangeScanIter` | **New**, convenience wrapper (`as_of_seq = u64::MAX`) |
| `contains(key, as_of_seq) -> Result<bool>` | **New**, per §10 |
| `snapshot() -> Snapshot` | **New**, per §2 |
| `oldest_live_snapshot_seq() -> Option<u64>` | **New**, per §2 |
| `read_stats() -> ReadStats` | **New**, per §12 |
| `batch_get(keys) -> Result<Vec<GetResult>>` | **Deferred**, per §11 — not built this phase |

`EngineError` is **not** modified — every error shape a read call can produce already exists in the enum (`NotFound` remains defined-but-unused by reads, per the decision below; `Corruption`, `Io`, `Unsupported`, `CapacityExceeded` are all already-used, already-correct shapes per the architecture report §10).

**`NotFound` vs. `Ok(None)` — resolved explicitly, not left ambiguous:** `get`/`get_as_of`/`range_scan`/`contains` all use `Ok(None)`/`Ok(false)`/simply not yielding a key for a genuine miss — **never** `Err(NotFound)`. `EngineError::NotFound` remains defined in the enum but is not constructed anywhere in the read path; it is reserved for a possible future, structurally different case (e.g., "no such database directory" at `open()`, or a future higher-level construct outside the current single-engine scope) that does not exist today. This decision is locked in by a regression test (§7 of this ADR) so a future change cannot silently flip it without that test failing first.

---

## Exact files that must change (implementation, not this step)

| File | Change |
|---|---|
| `src/lsm/mod.rs` | Add `range_scan`/`range`/`contains`/`snapshot`/`oldest_live_snapshot_seq`/`read_stats`; add `snapshot_registry: Arc<SnapshotRegistry>` and `read_stats: ReadStats` (atomics) fields to `LsmEngine`; add `RangeScanIter`/`Snapshot`/`SnapshotRegistry`/`ReadStats` types. No change to `get`/`get_as_of`'s existing logic. |
| `src/sstable/reader.rs` | Add a `blocks_read` counter parameter/callback to `read_block`'s call sites (or an `Arc<AtomicU64>` passed into `range_scan_raw`/`get_versioned`) — additive only, `get_versioned`/`range_scan_raw`'s existing logic and signatures otherwise unchanged. |
| `examples/read_engine_bench.rs` (new) | Benchmark suite per §15 of the architecture report / §15 of the phase brief. |
| `src/lsm/tests.rs` | New tests per §7 of this ADR below. |
| `PHASE_READ_ENGINE_CERTIFICATION.md` (new, later) | Final certification matrix — not part of this step. |

No change to `src/memtable/`, `src/manifest/`, `src/wal/`, `src/execution/`, or `src/error.rs` beyond the `blocks_read` counter plumbing above — confirmed against every decision in this ADR; none requires touching the protected Write Engine dependency.

---

## Exact tests to add

1. **Concurrent-flush-read consistency** (§8 of this ADR / §8 of the phase brief) — point-lookup AND `range_scan` variants, using `FlushFaultPoint`, no sleeps.
2. **`range_scan` correctness** across active+multiple-immutables+multiple-SSTables, including tombstone suppression and bound edge cases (`Included`/`Excluded`/`Unbounded` on both ends).
3. **`NotFound`-never-returned regression test** — assert every read method returns `Ok`, never `Err(NotFound)`, across hit/miss/tombstone/corrupt cases (corrupt cases still return `Err`, just never `Err(NotFound)`).
4. **`contains(k,s) == get_as_of(k,s)?.is_some()`** invariant test.
5. **`Snapshot`/`SnapshotRegistry` correctness**: overlapping snapshots, drop ordering, `oldest_live_snapshot_seq()` at every step.
6. **Corruption matrix for `range_scan`** — one test per row of the architecture report's §10 table, run through `range_scan` (not just `get`), asserting the exact `EngineError` variant.
7. **Recovery-then-read correctness** at each of the 8 crash points named in the phase brief's §11 (before flush / during flush / after SSTable publish / after Manifest ADD / after checkpoint marker / after Manifest checkpoint / before WAL purge / after WAL purge), calling `get`/`get_as_of`/`range_scan` after each reopen and asserting exact expected values, not just "recovery succeeded" — extending, not duplicating, the existing 205+10 crash-cycle infrastructure.
8. **Differential test against an independent reference model** (§12 of the phase brief) — a `BTreeMap<Vec<u8>, BTreeMap<u64, Option<Vec<u8>>>>`-shaped oracle (key → seq → value-or-tombstone) driven by the same PUT/DELETE/snapshot-read sequence as the real engine, compared after normal operation, flush, restart, and crash.
9. **Property-based tests** (§13 of the phase brief) — using `proptest` (already a dev-dependency, `Cargo.toml`, used elsewhere in this codebase for MemTable/SSTable/Manifest reference-model tests per this project's existing convention) for: ordering, no duplicate logical keys, snapshot correctness, tombstone correctness, newest-wins, reopen equivalence, reference-model equivalence.
10. **Read-stats correctness** (§12 of this ADR).

---

## Exact benchmarks to add

`examples/read_engine_bench.rs`, matching this project's existing benchmark harness conventions (real process, real `Get-Process`-based RSS sampling, sorted-latency p50/p95/p99, reusing the memory-investigation's proven scaling methodology for the N-SSTables axis):

MemTable hit; immutable-MemTable hit; SSTable hit at 1/10/100/1000+ live tables; bloom-negative miss; bloom-positive/index-negative miss; tombstone lookup; `get_as_of` (past snapshot); small/medium/large `range_scan`; `contains` vs. `get(...).is_some()` at hit and miss; cold vs. warm OS page cache. Each measured for p50/p95/p99/max, ops/sec, CPU%, RSS (start/min/max), and `blocks_read`/`sstables_consulted` (via `read_stats()`, §12). Multiple repetitions per case (matching this project's now-established practice of reporting median/min/max/variance, not a single run, per the Write Engine certification's own corrected performance methodology).

---

## Exact production certification gates (for the eventual `PHASE_READ_ENGINE_CERTIFICATION.md`, not produced in this step)

Matching the phase brief's §24 list exactly: CORRECTNESS, POINT LOOKUP, RANGE SCAN, SNAPSHOT, VERSION RESOLUTION, TOMBSTONES, CONCURRENCY, FLUSH VISIBILITY, CRASH SAFETY, RECOVERY, CORRUPTION HANDLING, MANIFEST CONSISTENCY, WAL VISIBILITY, MEMORY, RESOURCE BOUNDS, PERFORMANCE, READ AMPLIFICATION, OBSERVABILITY, LONG-DURATION STABILITY, FAIL-CLOSED BEHAVIOR, SECURITY — each requiring independent PASS/FAIL/OPEN evidence, matching exactly how `PHASE_WRITE_ENGINE_CERTIFICATION.md`'s 16-gate matrix was structured (established precedent, reused deliberately rather than inventing a different certification format for this engine).

---

## Discrepancies recorded explicitly (per the phase brief's own instruction — not silently reconciled)

1. `RangeScanRaw`/`MemTable::range` yield **every version** of a key in range, not the resolved/collapsed single version `get`/`get_as_of` return — the merge layer (§5) is where this collapse must happen; nothing today does it above the single-source level. Stated explicitly here because it is easy to assume incorrectly that these building blocks are "almost" a ready-made range scan; they are the correct *un-collapsed* building blocks, not a smaller version of the final feature.
2. `EngineError::NotFound` exists in the enum with no current producer anywhere in this codebase (read or write path) — the Architecture Spec's error-model section does not say which operations produce it. §13's decision resolves this for reads specifically; it remains genuinely open whether *anything* in this codebase will ever construct it, which this ADR does not need to resolve and does not attempt to.
3. The Architecture Spec's multi-engine/partition/router abstraction (§3-§19 of `RubixDB-Architecture-Specification-v1.0.md`) does not exist in this codebase at all (confirmed in the architecture report's scope note, re-confirmed while writing this ADR). Every decision above is scoped to the single LSM engine that does exist; none of it should be read as implying the broader abstraction is being built.

---

## Final Decision

Adopt all 13 decisions above as the Read Engine contract. Implementation proceeds only after this ADR is reviewed and approved, in the order the phase brief's own §26 specifies (consistency/concurrency tests before the API surface that needs them settled, `range_scan` before differential/corruption/crash tests, benchmarks before any performance-motivated change, integrated write/read testing and a long-duration soak before certification is attempted).

**Not implemented in this step.** `git status` verification: see below.
