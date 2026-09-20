# RubiXDB — Read Engine Range-Scan Performance ADR

**ADR ID:** ADR-RE-002

**Status:** **Implemented** (Increment 6, 2026-09-20 — see §9 below).
Originally proposed 2026-09-20 as "Proposed for Review — no
implementation in this document or this step"; §0-§8 below are that
original proposal, preserved unedited as the historical record of what
was proposed and why. §9 is new, appended after implementation, per
this project's own append-only documentation convention (never rewrite
a past section's own conclusions — add a new one).

**Date:** 2026-09-20 (proposed); 2026-09-20 (implemented, Increment 6)

**Scope:** `RangeScanIter`'s per-source cursor strategy only (`src/lsm/
mod.rs:551-` ff.). Does not touch point lookups (`get`/`get_as_of`/
`contains`), the Write Engine, the Manifest, WAL, or the snapshot
registry — none of those are implicated by the evidence below.

**Prepared from:** `PHASE_READ_ENGINE_RESOURCE_INVESTIGATION.md`
(Increment 5, this session) — every claim below traces to that
document's §2-§4, or to a fresh, direct source check performed while
writing this ADR, not to assumption. Builds on, and does not
contradict, `ADR-RE-001`'s own already-adopted `ReadView`/k-way-merge
design (§1/§5 there) — this ADR is about **cursor lifetime within**
that design, not about replacing it.

**Hard constraint acknowledged and honored throughout:** the phase
brief that authorized this investigation explicitly forbids adding a
cache, mmap, prefetch, or parallel-read machinery "unless a dedicated
ADR approves it." This document evaluates cursor-lifetime options only
— none of the options below is a cache, mmap, prefetch, or
parallel-read mechanism, and none is approved for implementation by
this document regardless.

---

## 0. What this document is and is not

This is the ADR the resource investigation's §10 concluded was
warranted: real, reproduced evidence (not a hunch) that the current
range-scan cursor design is a measured bottleneck on this project's own
established "realistic" (overlapping-key) endurance workload. **Nothing
in this document has been implemented** — it evaluates options and
proposes a direction for a *future* increment's decision, per the
investigation's own explicit instruction not to act on this evidence
unilaterally.

## 1. The problem, restated from the investigation

`RangeScanIter` re-queries each source (`peek_sstable`,
`src/lsm/mod.rs:644-697`) fresh every time that source contributes to
a winning key's group (`refill`, `:706-733`) — a real binary search +
block read/decode per call, by design (`ADR-RE-001` §3's own
instruction: reuse `range_scan_raw` unmodified, do not rewrite its
iteration behavior). On this project's own realistic endurance
workload (small key cardinality relative to per-flush write volume —
not a contrived case), this reduces to **O(distinct keys yielded ×
live SSTables holding a version of each key)** — reproduced exactly
(`sstables_consulted/sstable` pinned at a constant integer across five
SSTable-count checkpoints) in `PHASE_READ_ENGINE_RESOURCE_
INVESTIGATION.md` §4.3. Range-scan p50 in the real 4-hour soak grew
from 1.15ms to 43.1 **seconds** over the run (§2 there) — a real,
user-visible cost, not a theoretical one.

## 2. Why it was built this way (not a mistake — a deliberate,
documented trade-off)

`RangeScanIter`'s own doc comment (`src/lsm/mod.rs:523-550`) already
states the reason directly: both `MemTable::range()` and `SsTable::
range_scan_raw()` return iterators that *borrow* from the object they
were called on (`range_scan_raw<'a>(&'a self, ..)`). Storing such a
borrowing iterator in the same struct as the `Arc<SsTable>` it borrows
from is a self-referential struct — Rust's borrow checker cannot
express this without `unsafe` or an external crate (`ouroboros`/
`self_cell`), and `ADR-RE-001`'s own increment judged neither warranted
at the time. The short-lived resume-point-cursor design is the direct,
reasoned consequence of avoiding that self-referential structure, not
an oversight — this ADR's job is to decide whether the now-measured
cost changes that judgment, not to imply the original choice was
careless.

## 3. Options evaluated

### Option A — Persistent source cursors via an owned-`Arc` iterator refactor

Change `SsTable::range_scan_raw` (and `MemTable::range`, for the
immutable-MemTable sources) so the returned iterator **owns its own
`Arc<SsTable>`/`Arc<MemTable>` clone** internally, rather than
borrowing `&'a self`. Concretely: a new method taking `table: Arc
<SsTable>` by value (or `self: Arc<Self>`) and returning a `struct
SsTableRangeCursor { table: Arc<SsTable>, /* position state */ }` that
implements `Iterator` without any borrowed lifetime parameter at all.
`RangeScanIter` would then store `Vec<Option<SsTableRangeCursor>>`
directly (persistent, held across the whole scan) instead of `Vec
<Option<Bound<Vec<u8>>>>` resume points, and `refill` would call
`.next()` on the already-open cursor instead of constructing a fresh
`range_scan_raw` call.

**Why this avoids the self-referential problem `ADR-RE-001` correctly
flagged**: the borrow that made the original design self-referential
was `&'a self` tied to a field *in the same struct*. An iterator that
owns its own `Arc` clone borrows nothing from `RangeScanIter` — it is
an ordinary, non-self-referential field, exactly like `ReadView`'s
existing `Vec<Arc<SsTable>>` already is. No `unsafe`, no new
dependency.

**Cost**: each live source's cursor now holds whatever in-progress
position state `range_scan_raw`'s current binary-search/block-decode
logic needs to resume efficiently (likely: current block index +
decoded-block buffer position) — a real, bounded, per-*live-scan*
memory cost proportional to the number of sources still contributing
to that one scan, released when the scan (or that source's cursor)
finishes. This is different in kind from a cache: it is scoped to one
caller's one in-flight scan, not shared, not retained after the scan
ends — matching `ADR-RE-001`'s own "not a cache" framing for `ReadView`
itself (`src/lsm/mod.rs:399-403`).

### Option B — Block-position reuse without full cursor persistence

A lighter middle ground: keep the current re-query-per-key structure,
but have each source remember the **last data block index** it
resolved a key from (not a full iterator), so a `peek_sstable` call for
the *same* block as last time can skip the binary search and reuse the
already-decoded block instead of re-reading+re-decoding it from disk.
Reduces the intra-source cost `ADR-RE-001`'s own doc comment already
flagged (§4.1 of the investigation) but does **not** address the
dominant cost this investigation found (§4.1/§4.2: re-*peeking* every
source that holds a version of the winning key, once per key — that
happens regardless of whether the target block changed). Cheaper to
implement than Option A; addresses a real but secondary cost.

### Option C — Range-aware SSTable seeking (skip sources provably outside the remaining range)

Track, per source, whether its remaining key range still overlaps the
scan's `[start, end)` bound at all (using each `SsTable`'s already-open
index's own min/max key, no new I/O), and stop calling `peek_sstable`
for a source once it's provably exhausted for this scan — not once it
returns `None` (already done today), but *before* even trying, when
the index's cached bounds prove it can't contribute. **Does not help
this investigation's specific finding**: the soak's overlapping-keyspace
workload means most live sources *do* still overlap the range for most
of the scan (that's exactly what "high overlap ratio" in §4.2 means) —
this option's benefit is real but concentrated on a different
workload shape (large key cardinality, narrow range spans) than the
one this investigation measured.

### Option D — Reduce repeated Bloom/index work specifically

Point lookups already skip a table entirely on a bloom-negative
(Increment 3 §4's own established finding); range scans do not consult
the bloom filter at all today (`ADR-RE-001`'s own counter-semantics
note, restated in `PHASE_READ_ENGINE_PERFORMANCE.md`: "not incremented
by `range_scan_raw`, which does not consult the bloom filter — a range
scan walks blocks by key-range, not by point lookup" — correct, since a
bloom filter answers "does this table hold *this exact key*," not "does
this table hold *anything in this range*"). Not directly applicable
without a different filter structure (e.g., a range/interval filter)
— out of scope for a same-shaped fix; would itself need its own ADR
if pursued, and risks drifting toward the explicitly-forbidden
"secondary index" category.

## 4. Explicitly rejected without further evaluation (per the
investigating brief's own prohibition)

**Block cache, mmap, prefetch, parallel-read/parallel-source-fan-out.**
Not evaluated here at all — the brief that commissioned this
investigation explicitly forbids adding any of these without "a
dedicated ADR" approving them, and this ADR does not approve them. Any
future proposal to add one of these needs its own, separately-argued
ADR, not a paragraph inside this one.

## 5. Proposed direction (not a decision to implement — see §0)

**Option A** is the direction this ADR recommends a future increment
pursue, with **Option B** as a plausible, smaller-scoped fallback if
Option A's implementation cost (touching `SsTable::range_scan_raw`'s
and `MemTable::range`'s public shapes) turns out larger than expected
once attempted. Reasoning:

- Option A addresses the **actual, measured, dominant** cost this
  investigation found (§4.1/§4.2 of the investigation — the per-key,
  per-source re-peek, not the smaller intra-source re-seek cost).
  Options B/C/D each address a real but secondary or workload-mismatched
  cost.
- Option A does not require `unsafe` or a new external dependency
  (`ouroboros`/`self_cell`), the two alternatives `ADR-RE-001`'s
  original design explicitly avoided — the owned-`Arc` refactor sidesteps
  the self-referential-struct problem structurally, not by suppressing
  the borrow checker.
- Option A is naturally bounded in scope: it changes cursor *storage*
  and *lifetime* inside `RangeScanIter`, not the k-way merge algorithm,
  the `ReadView` capture semantics, the fail-closed-on-corruption
  contract, or any Write Engine surface — all already-certified
  behaviors this ADR does not propose touching.

**What a future increment implementing Option A would still need to
determine (explicitly not decided here)**: the exact shape of the
per-cursor position state (how much of `range_scan_raw`'s current
block-decode state is reused vs. rebuilt on each `.next()`), whether
`MemTable::range`'s already-cheap in-memory iteration needs the same
treatment or only `SsTable`'s I/O-bound path does (the investigation's
evidence is entirely about SSTable-side cost — `MemTable::range` never
touches disk), and a new benchmark section (extending `overlap_repro`,
§4.3 of the investigation) to measure the *actual* improvement before
claiming one, per this project's own established "benchmark first,
never assume" convention (`PHASE_READ_ENGINE_PERFORMANCE.md`'s own
Increment 3 methodology).

## 6. Safety impact (of the proposed direction, if a future increment implements it)

Purely internal to `RangeScanIter`'s own cursor bookkeeping — the
public `LsmEngine::range`/`range_scan` signatures, `ADR-RE-001`'s
fail-closed-on-corruption contract (`errored: bool` sticky-stop, `src/
lsm/mod.rs:566-569`), and the k-way merge's ordering/visibility
guarantees (§4/§5 there) are all unaffected in shape — only *how* each
source's next value is obtained changes, not *what* value is obtained
or *when* the scan reports an error. No Write Engine surface is
touched. Explicitly not evaluated for safety here beyond this
structural note, since nothing is being implemented in this document.

## 7. Tests that would be required (not written here)

A future implementing increment would need: the existing
`RangeScanIter` test suite (`src/lsm/tests.rs`) re-run unchanged and
still passing (behavior must not change, only cost); the corruption-
matrix tests (`PHASE_READ_ENGINE_PERFORMANCE.md`'s own table) re-run
to confirm fail-closed behavior survives the cursor-lifetime change;
and a new, `overlap_repro`-style before/after benchmark (§4.3 of the
investigation) proving the change actually reduces `sstables_consulted`
per scan on the overlapping-keyspace workload, not just theoretically.

## 8. Status and next step

**Proposed for Review.** No code changes accompany this document. The
next step is a human/maintainer decision on whether to open a new
Read Engine increment scoped specifically to implementing Option A (or
B), informed by this ADR and `PHASE_READ_ENGINE_RESOURCE_
INVESTIGATION.md` — not an automatic continuation from this
investigation, per the phase brief's own explicit instruction to stop
and report rather than optimize automatically.

## 9. Implementation record (Increment 6, 2026-09-20)

Option A was implemented as proposed in §5, with no deviation from the
design sketched there: `SsTableRangeCursor` (`src/sstable/reader.rs`,
new type, 126 lines added, zero lines removed from that file) owns its
own `Arc<SsTable>` clone plus owned `Bound<Vec<u8>>` range bounds — no
`unsafe`, no `ouroboros`/`self_cell`, no new `Cargo.toml`/`Cargo.lock`
entry (confirmed by `git status`, zero changes to either file).
`RangeScanIter` (`src/lsm/mod.rs`) now stores one persistent
`Option<Peekable<SsTableRangeCursor>>` per live SSTable source
(`sstable_cursors`), constructed once in `RangeScanIter::new` and
driven forward via ordinary `Peekable::peek`/`next` for the scan's
entire remaining lifetime — replacing the old `sstable_next_start:
Vec<Option<Bound<Vec<u8>>>>` resume-point-plus-fresh-`range_scan_raw`-
call design entirely. `RangeScanRaw`/`range_scan_raw` (the pre-existing
borrowed-iterator type/method) were left completely unmodified — kept,
not replaced, exactly as §5's proposed direction anticipated (their
existing callers/tests are untouched).

**§7's required tests, all run and passing**: full existing
`RangeScanIter`/corruption-matrix/range-bounds/version-tombstone/
concurrent-flush/property-test suite (306/306 `cargo test --lib`,
debug and release; the +1 over the pre-Increment-6 305 is this
increment's own new regression test, `range_scan_source_cursor_
persists_across_keys_instead_of_reconstructing_per_key`,
`src/lsm/tests.rs`, asserting `sstables_consulted` increases by
*exactly* the live SSTable count for one scan over a small,
fully-overlapping keyspace — an observable-counter regression guard
per brief §23, not a timing-dependent one); `wal_tests` (12/12),
`crash_consistency --features test-util` (2/2), `pathological_
recovery_matrix` debug+release (9/9 each); `cargo fmt --check`/`cargo
clippy --all-targets --all-features -- -D warnings` both clean.

**§6's safety impact, verified rather than merely asserted**: no
`unsafe`; the k-way merge algorithm (`refill`/`Iterator::next`) is
byte-for-byte the same as before this increment (only `peek_sstable`'s
body and the `sstable_cursors` field changed); `errored`-flag
fail-closed-on-corruption behavior preserved and re-verified by the
full corruption-matrix test run; `src/wal/`, `src/manifest/`, `src/
error.rs` untouched (confirmed by `git status`); point-lookup code
paths (`get`/`get_as_of`/`contains`, `SsTable::get_versioned`/
`contains_versioned`) untouched (confirmed by diff — `src/sstable/
reader.rs`'s entire diff is additive).

**Benchmark evidence (full detail: `PHASE_READ_ENGINE_PERFORMANCE.md`'s
own dated Increment 6 section, `PROGRESS.md`'s Increment 6 entry)**:
before/after comparison using the identical, unmodified `overlap_repro`
workload at the same five checkpoints (20/50/100/200/300 SSTables),
`n=7` repetitions each. `blocks_read` — whose counting point was not
touched by this increment — dropped by an exact, constant **4.714x**
at every single checkpoint, direct proof the redundant re-read/re-decode
mechanism this ADR targeted is gone. Wall-clock p50 improved
**3.20x-3.69x** across all five checkpoints. `sstables_consulted`
dropped by an exact, constant 21x at every checkpoint, reflecting both
the mechanism fix and this increment's documented, regression-tested
redefinition of that counter (§16 of the phase brief; not a silent
change). A resource-lifetime check (`read_engine_bench cursor_
resource_check`) ran 400 repeated create/consume/drop scan cycles and
found zero handle delta, zero thread delta, and RSS growth (220 KB
total) consistent with ordinary allocator noise, not a per-scan leak.

**Conclusion**: the measured dominant re-peek cost (§1-§2 above,
originally traced and reproduced in `PHASE_READ_ENGINE_RESOURCE_
INVESTIGATION.md` §4) is substantially reduced, with no correctness
regression, no protected-behavior change, and no new resource leak.
**`ADR-RE-002` status: IMPLEMENTED.**

**What remains open, not resolved by this increment**: the real
4-hour soak's own steeper apparent ~n^2.2 latency exponent was never
fully reproduced by `overlap_repro` at benchmark scale (before *or*
after this fix — both show a closer-to-linear shape at this small
scale); this increment was not re-validated against another real,
multi-hour soak (deliberately not run, per the phase brief's explicit
instruction). READ ENGINE PRODUCTION READY remains **NO** — final
corruption/recovery validation, final integrated endurance validation,
final performance validation, and the final certification matrix are
still outstanding, unstarted gates, not something this increment
declares complete.
