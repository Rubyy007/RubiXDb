# RubiXDB Phase 4A — MemTable Architecture

Companion to `PHASE4A_ARCHITECTURE.md` (system-level integration) —
this document covers the MemTable's own internal design and the new
WAL replay API it depends on.

## 1. Data model (operating brief §7)

Per `RubixDB-LSM-Engine-Specification-v1.0.md` §1.1 (already final —
followed exactly, not re-derived):

```rust
pub struct MemTable {
    map: BTreeMap<(Vec<u8>, u64), MemtableValue>, // (user_key, seq) -> value, ascending
    size_bytes: usize,
    max_size_bytes: usize,
    min_seq: Option<u64>,
    max_seq: Option<u64>,
}

pub enum MemtableValue {
    Put(Vec<u8>),
    Tombstone,
}
```

`UserKey` is `Vec<u8>` (matching `WalOpOwned::Put { key: Vec<u8>, .. }`
— no new key type introduced). `SequenceNumber` is `u64` (matching
`WalPosition::seq`/`GroupCommitter`'s own `u64` throughout — no
duplicate sequence representation). `OperationType` is `MemtableValue`'s
own two variants (`Put`/`Tombstone`), mapping directly from
`WalOpOwned::{Put{key,value} -> Put(value), Delete{key} -> Tombstone}`
— `op` byte values `1 = PUT`, `2 = DELETE` are shared with the WAL by
design (LSM Engine Spec §0.2); no duplicate representation of the same
WAL operation is introduced (operating brief §7's own explicit
instruction).

## 2. Data structure selection (operating brief §10) — followed the existing spec, not re-litigated

The operating brief asks to evaluate `BTreeMap` vs. `SkipList` "if the
specification permits both." **It does not** — `RubixDB-LSM-Engine-
Specification-v1.0.md` §1.1 is "Status: Final — ready for
implementation" and prescribes `BTreeMap<(Vec<u8>, u64), MemtableValue>`
by exact type signature, with a specific, load-bearing algorithm
(`get_as_of` via `range(...).next_back()`, §1.2) built directly on
`BTreeMap`'s ordered-range-query API. Per this project's own
established practice throughout Phases 0-3 (every WAL/Group-Commit
design decision that a Tier-1 spec already settled was implemented
as specified, not re-opened for debate — `ARCHITECTURE.md`'s own
"Decide-vs-Ask Policy" framing), a settled Tier-1 specification is
followed, not re-litigated absent a discovered defect. No `SkipList`
evaluation was performed, and none is needed: the spec leaves no
degree of freedom here to evaluate.

**This is also the simpler, safer choice on its own merits** (operating
brief §10's own fallback preference, "prefer the simplest correct
structure"): `std::collections::BTreeMap` requires zero `unsafe` code,
is already used throughout this codebase (`FileWal`'s own
`sealed_max_seq: BTreeMap<u64, u64>`), and — per §3 below — is never
accessed concurrently for writes in the first place, so a concurrent
lock-free structure (which a `SkipList` would typically exist to
provide) solves a contention problem this design does not have.

## 3. Concurrency model (operating brief §11) — per the existing spec, §1.5

"A `MemTable` is **not** internally synchronized — same single-writer
principle as the WAL." The MemTable type itself takes `&mut self` for
every mutating method; synchronization is the *caller's* responsibility
(the LSM facade, `src/lsm/mod.rs`), exactly as the spec assigns it.

**Why this is correct, not merely convenient, for Phase 4A's own write
path**: every write reaching the MemTable has already been serialized
through the **Dedicated Batch Coordinator** — the one coordinator
thread is the *only* thread that ever calls `MemTable::insert` for the
active memtable (operating brief §31's own instruction: "must continue
using the production Dedicated Batch Coordinator rather than creating
unnecessary OS threads inside MemTable" already implies this). There is
never write-write contention on the active MemTable to design around at
all — mirroring exactly the reasoning that made batching (not a
lock-free WAL) the actual fix for the coordinator's own historical
throughput problem (`PHASE2_ADR.md`).

**Reads** (arbitrary caller threads calling `get`/`get_as_of`/`range`
concurrently with the coordinator's own writes) do need synchronization
against the single writer. `src/lsm/mod.rs` wraps the active MemTable
in `RwLock<MemTable>` — write path takes the write lock only for the
`insert` call itself (memory-speed, never held across the WAL
`await_durable` wait that precedes it — see `PHASE4A_ARCHITECTURE.md`
§5), reads take the read lock only for the specific lookup/iteration
call. A frozen (immutable) MemTable needs **no lock at all** for reads
— once `freeze()` consumes `self` and returns `Arc<MemTable>`, Rust's
own ownership rules make it compile-time-impossible to mutate again
(spec §1.2's own stated guarantee), so concurrent readers of an
`Arc<MemTable>` need nothing beyond the `Arc` itself.

**No lock-free/unsafe concurrent structure was introduced** — per
operating brief §11's own explicit instruction ("do not implement
lock-free concurrency without evidence that ordinary synchronization is
inadequate"), and no such evidence exists: `RwLock`'s cost is paid once
per batch (the coordinator's own batching already amortizes it across
however many logical writers land in one batch), not once per logical
writer, mirroring exactly how `GroupCommitter`'s own single `fsync`
per batch already amortizes the WAL's dominant cost.

## 4. Ordering model (operating brief §9)

`(user_key, seq)` ascending in the map means, for a fixed `user_key`,
all its versions are contiguous and ordered by ascending `seq`. The
**newest visible version** for a snapshot at `as_of_seq` is the highest
`seq <= as_of_seq` — found via `range((key,0)..=(key,as_of_seq)).
next_back()` (spec §1.2, unmodified), an `O(log n)` range query, no
linear scan, no special-casing.

- **Same key, multiple sequence numbers**: naturally contiguous in the
  map; `get_as_of` at any intermediate `seq` returns exactly the
  version current at that point (spec §1.6 test 3).
- **Put followed by Delete**: the `Tombstone` entry has the higher
  `seq`, so it is found first by any `as_of_seq >=` its own `seq` — a
  visible tombstone means "not found" (operating brief §9's own rule),
  implemented at the `get` layer (§6 below), not inside the MemTable's
  raw lookup (which deliberately returns the raw `MemtableValue`,
  tombstone included — spec §1.2's own doc comment: "version resolution
  and tombstone filtering are the caller's... responsibility, not the
  memtable's").
- **Delete followed by Put**: symmetric — the later `Put` has the
  higher `seq` and is found first, exactly resurrecting the key for any
  `as_of_seq` at or after that `Put`'s own `seq`.
- **Snapshot reads**: `as_of_seq` is the one parameter that defines a
  snapshot's visibility boundary — see §7.

## 5. Memory accounting (operating brief §17-§18) — per the existing spec, §1.3

`entry_size(key, value) = key.len() + value_payload_len + ENTRY_OVERHEAD`,
`ENTRY_OVERHEAD = 32` — a **documented, stable, conservative estimate**
for `seq: u64` (8 bytes) + the `MemtableValue` enum discriminant +
`BTreeMap` node overhead, explicitly **not** claimed to be exact
physical allocation (operating brief §17's own caution against that
overclaim — the spec's own §1.3 already states this estimate "is not
exact, and not required to be; it only needs to be a stable,
conservative estimate"). `size_bytes()`/`max_size_bytes`/`is_full()`
expose current/limit/threshold directly; `remaining_capacity()` and
`entry_count()` are Phase-4A-added accessors (not in the original spec
text, but directly implied by operating brief §17's explicit
requirement to expose them) built from the same tracked fields.

`max_size_bytes` is a configured value, not scattered constants
(operating brief §18) — a `MemTableConfig` (or equivalent) struct with
a documented default of **64 MiB** (the value operating brief §18
itself names as "the prior design," matching `RubixDB-LSM-Engine-
Specification-v1.0.md` §4.1's own `LsmConfig::memtable_max_size_bytes`
default — that spec's default is actually documented as **4 MiB**, not
64 MiB; this discrepancy is flagged, not silently resolved — see
`PHASE4A_ADR.md`).

## 6. `Get` semantics (operating brief §14) — layered on top of the spec's raw `get_as_of`

```rust
// MemTable::get_as_of (spec §1.2, unmodified) returns the raw
// MemtableValue, tombstone included.
pub fn get(&self, key: &[u8], as_of_seq: u64) -> Option<&[u8]> {
    match self.get_as_of(key, as_of_seq) {
        Some((_, MemtableValue::Put(v))) => Some(v),
        Some((_, MemtableValue::Tombstone)) => None, // visible tombstone => not found
        None => None,                                  // no version at or before as_of_seq
    }
}
```

`Get(key)` (no snapshot argument, "latest as of now") is `get(key,
u64::MAX)` — matching spec §1.2's own "equivalent to
`get_as_of(key, u64::MAX)`" note for the no-snapshot case, in Phase 4A
served by passing the MemTable's own current `max_seq` (or `u64::MAX`
where no upper bound is meaningful) rather than requiring every caller
to separately track "what's the latest seq right now."

## 7. Snapshot model (operating brief §16)

A minimal snapshot is exactly one `u64`: the sequence number at
snapshot-creation time (`GroupCommitter::durable_through()` or the
MemTable's own current `max_seq`, whichever the caller's own read path
uses as its consistency boundary — Phase 4A's own facade uses the
former, since only durable data should ever be snapshot-visible,
consistent with `PHASE4A_ARCHITECTURE.md` §5's ordering rule). Passed
as `as_of_seq` to every `get`/`range` call — the MemTable itself holds
no snapshot registry or reference-counting (that belongs to the future
LSM facade's tombstone-GC/compaction-safety logic, LSM Engine Spec
§4.2/§5.2 — explicitly out of Phase 4A's scope, which does no
compaction at all yet, so no tombstone is ever actually dropped this
phase regardless of any snapshot's lifetime).

A snapshot remains stable under concurrent writes purely because
`as_of_seq` is a fixed number compared against immutable per-entry
`seq` values already in the map — a write after the snapshot was taken
inserts entries with a **higher** `seq`, which the snapshot's own
`as_of_seq` comparison naturally excludes; no special bookkeeping is
needed for this property to hold (this is the same reasoning the spec's
own `get_as_of` algorithm already relies on for its correctness, not a
new mechanism).

## 8. Freeze / Immutable MemTable (operating brief §19-§21)

```text
Mutable MemTable
       |
       v
    FREEZE (spec §1.2: fn freeze(self) -> Arc<MemTable>)
       |
       +--------> Immutable MemTable (Arc<MemTable>, pushed onto an immutables list)
       |
       +--------> New Mutable MemTable (fresh MemTable::new(..), swapped in atomically
                    under the same RwLock write-guard as the freeze itself)
```

**Compile-time guarantee, not a runtime flag** (spec §1.2's own stated
property, and operating brief §19's own required test): `freeze`
consumes `self` by value and returns `Arc<MemTable>`, whose only public
methods are `&self` (read-only) — there is no `&mut` method reachable
on `Arc<MemTable>` at all, so "no further writes to the immutable
structure" is enforced by the Rust type system, not by a boolean check
a future code change could forget to consult. Stable sequence range
(`min_seq`/`max_seq`, unmodified after freeze, since nothing can write
again) and read visibility/memory accounting all remain correct for
the same reason — nothing about the frozen `MemTable`'s internal state
changes at the moment of freezing; only its *type-level mutability* does.

**Ownership** (operating brief §20): the Phase-4A-scoped LSM facade
(`src/lsm/mod.rs`) owns the `immutables: RwLock<VecDeque<Arc<MemTable>>>`
list (mirroring spec §4.1's own field, added to Phase 4A's scoped-down
facade). Any reader holding a cloned `Arc<MemTable>` (via a captured
`ReadView`-style snapshot of the list) may read it for as long as that
clone is held; it is destroyed automatically (no explicit "who can
destroy it" logic needed) once its `Arc` strong count reaches zero —
standard Rust ownership, not a bespoke lifecycle. **A later RUBIC
SSTable writer (Phase 4B) can consume an `ImmutableMemTable` through
its existing `range(..)` ordered-iteration method (spec §1.2) alone** —
exactly the interface `RubixDB-LSM-Engine-Specification-v1.0.md` §2.7
step 1 ("Iterate the memtable's `range(..)`... already in `(key asc,
seq asc)` order, no sort needed") already assumes. No internal tree
node, no block-layout detail, is exposed — the MemTable API is not
extended or modified to anticipate SSTable internals.

## 9. Immutable MemTable backpressure (operating brief §21)

The Phase-4A-scoped facade's `LsmConfig` (a subset of spec §4.1's own)
includes `max_immutable_memtables: usize` (default: a small bounded
number — see `PHASE4A_ADR.md` for the exact default and rationale).
When a freeze would push the immutable count past this bound, the
facade's write path returns a documented capacity error (mirroring
`BatchCoordinatorPool`'s own existing `EngineError::CapacityExceeded`/
`Timeout` backpressure pattern, not a new error taxonomy) **rather than
silently allocating an unbounded immutable list** — since Phase 4A
implements no flush-to-SSTable path yet (Phase 4B), immutable
MemTables in this phase are never actually drained, so this bound is
the one thing standing between sustained write load and unbounded
memory growth; it is treated as load-bearing, not decorative.

## 10. WAL -> MemTable recovery (operating brief §24-§27) — bounded memory, additive WAL API

**Explicitly not**: load the entire WAL into a `Vec` (`open_for_
recovery`'s existing behavior, `PHASE3B_ADR.md` ADR-P3B-5 /
`PHASE3C_ADR.md` ADR-P3C-1's own measured ~134 bytes-RSS-per-record
finding) and then apply that `Vec` to a MemTable — this would simply
relocate the already-diagnosed unbounded-memory problem one layer up,
not solve it.

**Chosen approach: callback replay**, the direction `PHASE3C_ADR.md`
ADR-P3C-1 itself judged "simplest to reason about correctness-wise...
closest in shape to what a MemTable-rebuild consumer would actually
call directly" — starting from that existing analysis rather than
inventing a fourth mechanism (operating brief §24's own instruction).

**New, additive WAL API** (`src/wal/mod.rs`, alongside — not replacing —
`open_for_recovery`): a function that walks the WAL directory using the
*exact same* per-segment primitives `scan_directory`/`open_for_recovery`
already use (`scan_segment`, `walk_segment`, `decode_segment_header` —
zero duplication of the corruption/torn-tail classification logic
itself, per operating brief §24's "do not create a second recovery
implementation"), but instead of accumulating every segment's records
into one combined `Vec` (`scan_directory`'s own `result.records.
extend(outcome.records)` line — the actual site of the unbounded
growth), invokes a caller-supplied callback once per record and lets
each segment's own (already-bounded, since segments are size-capped by
`max_segment_size`, default 64 MiB) `Vec<(u64, WalOpOwned)>` be dropped
immediately after that segment's records are delivered — bounding peak
memory to `O(one segment's records)`, not `O(total WAL records)`.

This is a **separately documented WAL increment** (operating brief
§24's own requirement) — see `PHASE4A_ADR.md` for the exact function
signature, its own dedicated tests, and why this does not touch
`open_for_recovery`/`WalReplayResult`/`walk_segment` at all (verified:
their own existing 130-test suite is unchanged and still green).

**MemTable recovery, end to end**: `FileWal::open_for_recovery` (or the
new streaming equivalent's own directory-opening step) reconstructs the
live `FileWal`/`GroupCommitter` exactly as today; the new streaming
replay function then walks the same directory a second time (read-only,
via `inspect`-style shared access — see `PHASE4A_ADR.md` for why a
second pass, not a fused single pass, was chosen), applying every
record in order to a fresh `MemTable` via `MemTable::insert` (spec
§1.2's own "used both by normal writes and by WAL replay during
recovery" design intent). No checkpoint/`SET_CHECKPOINT` filtering is
needed in Phase 4A (unlike the full spec §7.1 step 5) — there is no
flush-to-SSTable yet, so every durable WAL record belongs in the
recovered MemTable, full stop.
