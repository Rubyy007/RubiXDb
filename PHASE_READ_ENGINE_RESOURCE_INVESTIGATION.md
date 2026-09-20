# Read Engine — Resource Investigation (Increment 5)

Append-only historical record, same convention as
`PHASE_READ_ENGINE_PERFORMANCE.md`: never edit a past run's numbers,
only add new dated sections. This document is the memory + range-
performance investigation the phase brief ("READ ENGINE — INCREMENT 5:
MEMORY + RANGE PERFORMANCE INVESTIGATION") required *before* any
optimization, run against Increment 4's completed, `RESULT=PASS`
4-hour integrated write/read endurance soak
(`temp/read_write_soak_output.log`, preserved as historical evidence,
not overwritten or rerun for this investigation). Nothing here
optimizes anything.

## 0. Soak identity (for cross-reference)

`read_write_soak_test`, `duration_secs=14400 writer_count=8
reader_count=16 seed=20260920`, `E:\rubixdb_read_write_soak_main`.
`RESULT=PASS`: `writes_issued=13,483,811 deletes_issued=3,375,298
reads_issued=4,582,352 range_scans_issued=261,455 in_run_mismatches=0
recovery_ok=true post_recovery_mismatches=0
capacity_backpressure_events=0 final_sstables=614
final_db_bytes=2,368,131,525 final_rss_kb=2,011,784`. 115 `HEALTH`
samples (~125s apart), 919 `LAT` windowed-latency lines, parsed
programmatically (regex over every field, not eyeballed) into
`health.csv`/`lat.csv` for every number below — every number in this
document traces to that log or to a real, reproduced benchmark run
noted where it appears.

## 1. Memory investigation

### 1.1 RSS vs. SSTable count / db size

| sample | t (s) | rss_kb | sstables | db_bytes | handles | threads |
|---:|---:|---:|---:|---:|---:|---:|
| 1   | 120    | 110,928   | 5   | 23,435,415    | 109 | 29 |
| 29  | 3,571  | 896,820   | 156 | 606,711,377   | 261 | 29 |
| 58  | 7,226  | 1,648,836 | 313 | 1,211,597,275 | 418 | 29 |
| 87  | 10,918 | 1,197,004 | 470 | 1,815,029,293 | 575 | 29 |
| 115 | 14,510 | 2,011,800 | 614 | 2,368,131,525 | 715 | 4  |

Whole-run linear regression (least squares over all 115 samples,
computed programmatically): `rss_kb ≈ 1970.8 × sstables + 450,606`
(R²=0.698) and, equivalently, `rss_kb ≈ 0.000512 × db_bytes + 447,609`
(R²=0.698) — RSS tracks roughly **half a byte of working set per byte
of on-disk data**, moderately but not perfectly (R²=0.70, not ~1.0).

**Why the fit isn't tighter, traced rather than hand-waved**: RSS is
not monotonic like `sstables`/`db_bytes` are — it has real
multi-hundred-MB dips (worst: sample 59→61, `t=7353s→7607s`,
`rss_kb` 1,674,428 → 969,988, a **−704,440 KB** drop in ~254s) while
`sstables`/`active_entries`/`immutables` show nothing unusual at the
same samples (`sstables` 319→330, strictly increasing;
`immutables=0` throughout; `active_entries` in its normal 5,000–25,000
sawtooth range, the same range it's in everywhere else in the run —
see full table in `health.csv`). The active-MemTable sawtooth
(`active_entries` cycling roughly 5K–27K entries, each entry at most a
few hundred bytes) is worth at most a few MB — three orders of
magnitude too small to explain a 704 MB drop.

**Ownership check (brief's explicit instruction: do not call this a
leak without ownership evidence)** — traced directly in `src/sstable/
reader.rs` and `src/lsm/mod.rs`:

- `SsTable` (`src/sstable/reader.rs:66-81`) holds exactly one `File`,
  one `Footer`, one `BloomFilter`, one `Vec<IndexEntry>`, for its
  entire lifetime — no data block is ever cached (`open()`'s own doc
  comment: "Data blocks are **not** read here — bounded memory at
  `open()` regardless of table size"). This structure is **never
  freed** for a live table (no compaction exists yet in this codebase
  — every SSTable this engine ever opens stays open until process
  shutdown), so this is a **known, monotonic, by-design** memory
  floor, not a leak: it only ever grows, exactly matching
  `sstables_consulted`'s and `db_bytes`'s own strictly-increasing
  shape.
- `grep -rn "static \|lazy_static\|OnceCell\|OnceLock\|thread_local!"`
  across `src/lsm/mod.rs` and `src/sstable/reader.rs`: **zero matches**
  — no global cache, no process-wide table registry beyond the
  engine's own `sstables: Vec<Arc<SsTable>>` (which is exactly the
  monotonic, by-design structure above).
- No engine-side registry of live `RangeScanIter`s exists (`grep` for
  `active_scans`/`iterators:`/`scan_registry` across `src/lsm/mod.rs`:
  no matches) — a `RangeScanIter` is a plain, caller-owned value; the
  engine keeps no reference to it once `range()`/`range_scan()`
  returns it. See §5 below for the full lifetime trace.

**Conclusion**: no ownership path exists in this codebase for a real
memory leak. The **monotonic** component of RSS growth is fully
explained, with source evidence, by per-SSTable index+bloom-filter
accumulation (expected, and unavoidable until a future Compaction
phase exists to remove superseded/dead versions — the same structural
fact Increment 3 §4/§11 already established for read-amplification, now
confirmed to have a memory-side counterpart too). The **non-monotonic**
component (multi-hundred-MB dips that later re-grow past their
pre-dip level, e.g. 969,988 KB at sample 61 → 1,197,004 KB at sample
87 → 2,011,800 KB final) is not explained by any application-level
data structure in this codebase (nothing shrinks), and is consistent
with — but not independently proven via a memory profiler, which this
investigation did not have tool access to run — Windows'
`WorkingSet64` metric being trimmed/reclaimed by the OS under memory
pressure and refaulted back in on next touch, a known characteristic
of that specific metric, not of this process's own allocations.
Stated as the most plausible explanation given the evidence actually
available, not asserted as proven.

### 1.2 Comparison against Increment 4's `memory_scaling` section

Increment 4's own dedicated, disjoint-sequential-key `memory_scaling`
check (`PHASE_READ_ENGINE_PERFORMANCE.md`, "Increment 4" section)
found ≈1.16 KB/SSTable — apparently far below this soak's ≈1,970
KB/SSTable regression slope. **Not a contradiction, and not re-run
here to "fix" the discrepancy** — traced to a real, stated difference
in fixture shape: that section's fixture used 8 tiny keys/table,
1-byte values; this soak's tables average
`(writes_issued+deletes_issued)/final_sstables ≈ (13,483,811+3,375,298)
/614 ≈ 27,424` entries/table, real 16–256-byte values. A `BloomFilter`
and `Vec<IndexEntry>` sized for ~27,424 entries is necessarily far
larger than one sized for 8 — this is the same "absolute per-table
constant differs with table content size, shape doesn't" caveat
Increment 4's own writeup already stated, now confirmed quantitatively
by an independent, real production-shaped workload rather than assumed.

## 2. Range latency investigation

Per-op `p50`/`p95`/`p99`/`max` across the run (early/¼/mid/¾/late
samples; full series in `lat.csv`):

| op | sample 1 (5 sstables) p50 | sample 58 (313) p50 | sample 115 (614) p50 |
|---|---:|---:|---:|
| `get`                      | 0.8us     | 3.9us        | 3.9us        |
| `get_as_of`                | 1.4us     | 62.5us       | 68.9us       |
| `contains_miss`            | 0.6us     | 72.9us       | 142.1us      |
| `range_small` (10-key)     | 22.1us    | 247,916.0us  | 560,264.6us  |
| `range_medium` (100-key)   | 148.4us   | 2,190,405.4us| 4,396,788.3us|
| `range_large` (1000-key)   | 1,152.9us | 21,219,028.4us | 35,356,567.3us (peak 43.1s at sample 111/597 sstables) |

Point-lookup latencies (`get`, `get_as_of`, `contains_miss`) grow and
then **flatten** — consistent with Increment 3 §4's already-established
finding (bloom-negative-dominated, cheap per extra table) continuing
to hold at this scale, nothing new.

**Range-scan latencies do not flatten — they keep climbing for the
entire run**, and climb *faster* than SSTable count itself:

| sample | sstables | `range_large` p50 (us) |
|---:|---:|---:|
| 1   | 5   | 1,152.9        |
| 11  | 58  | 2,918,557.2    |
| 31  | 167 | 10,034,054.6   |
| 61  | 330 | 22,557,129.9   |
| 91  | 492 | 34,944,806.2   |
| 111 | 597 | 43,137,993.1   |

From sstables=5 to sstables=597 (×119.4) `range_large` p50 grew
1,152.9us → 43,137,993.1us (×37,414) — an apparent power-law exponent
of `log(37414)/log(119.4) ≈ 2.20`, i.e. **super-linear, not the linear
scaling point lookups show**. This is the headline finding this
increment exists to explain.

## 3. Read amplification

`ReadStats` deltas per `HEALTH` window (`blocks_read_delta`,
`sstables_consulted_delta`, `read_requests_delta` — window totals,
mixing point + range ops in the denominator, so §4's clean single-op
reproduction below is the authoritative per-scan number; this section
establishes the trend that motivated building that reproduction):

| sample | sstables | read_requests_delta | blocks_read_delta | sstables_consulted_delta |
|---:|---:|---:|---:|---:|
| 1   | 5   | 2,327,098 | 21,451,482 | 14,811,024 |
| 29  | 156 | 2,992     | 44,580,287 | 30,750,579 |
| 58  | 313 | 1,750     | 41,898,906 | 28,919,623 |
| 87  | 470 | 994       | 39,730,833 | 27,425,545 |
| 115 | 614 | 148       | 11,931,449 | 8,225,124  |

`read_requests_delta` collapses from millions (sample 1, dominated by
cheap point reads while few SSTables exist) to under 1,000 by the end,
while `blocks_read_delta`/`sstables_consulted_delta` *stay in the tens
of millions* — meaning the average cost per read request, not just
total volume, exploded. At sample 115, only ~148 read requests (of
which ~27 were range scans — `range_medium n=6 + range_small n=4 +
range_large n=17` in that window) produced 8,225,124
`sstables_consulted` — arithmetic elimination (point/`contains` reads
at 614 SSTables cost ~600 consultations each per Increment 3 §4's
established scaling, so ~121 non-range reads × ~600 ≈ 72,600, under 1%
of the total) attributes essentially all of it to the ~27 range scans:
**≈300,000 sstable-consultations per single range operation** at this
SSTable count — roughly **490× the live SSTable count itself**. That
ratio (consultations per range scan ≫ live SSTable count) is the exact
signature investigated and confirmed in §4.

## 4. Range-scan cursor cost — root cause, traced and reproduced

### 4.1 The mechanism, traced in source

`RangeScanIter` (`src/lsm/mod.rs:551-570`) is a k-way merge over
per-source **resume-point cursors** (`sstable_next_start: Vec<Option
<Bound<Vec<u8>>>>`) — the type's own doc comment (`src/lsm/mod.rs:
523-550`) already documents a **known, deliberately deferred**
performance characteristic: "an `SsTable` source may re-run
`range_scan_raw`'s own block-locating binary search, and re-read+
re-decode a data block, once per distinct key that block holds, rather
than once per block" — an intra-source, per-block-reuse cost. That
documented cost is real but is **not** what dominates this soak's
numbers.

The actual dominant cost is in `refill()` (`src/lsm/mod.rs:706-733`)
and its caller in `Iterator::next()` (`src/lsm/mod.rs:742-` ff.): each
time the merge resolves one winning key, it pops **every heap entry
that ties on that key** (the `group` loop, `src/lsm/mod.rs:757-764`)
and calls `refill(Some(&sources_just_advanced))`, which re-peeks
(fresh `range_scan_raw` call, `sstables_consulted` incremented once
per call — `src/lsm/mod.rs:649-651`) **every source that contributed
to that group**. If a key exists in *N* live SSTables, resolving it
costs *N* fresh `peek_sstable` calls — by design, this is correct and
necessary (each source's version of that key must be inspected to find
the visible one). The cost scales with **how many live SSTables
actually hold a version of each key the scan yields**, not with the
live SSTable count in the abstract.

### 4.2 Why this soak's workload makes that cost ≈ the full SSTable count

`read_write_soak_test.rs`'s `KEY_CARDINALITY = 4000`
(`examples/read_write_soak_test.rs:67`), and each memtable-fill flush
cycle accumulates on average `(writes_issued+deletes_issued)/
final_sstables ≈ 27,424` write events before flushing — **≈6.9 writes
per distinct key per flush cycle** on this small a keyspace. The
probability a specific key is *never* chosen across 27,424 uniform
draws from 4,000 keys is `(3999/4000)^27424 ≈ 0.00105` — so **≈99.9%
of the 4,000 keys land in every single flushed SSTable**. This soak's
workload is not an adversarial edge case; it is the project's own
established "realistic, overlapping/scattered key layout" endurance
methodology (`PHASE_READ_ENGINE_PERFORMANCE.md`'s Increment 4 section
explicitly contrasts it with the "favorable, contiguous-key" fixture
used everywhere else). Given that, §4.1's per-key `refill` cost is not
bounded by the number of sources *actually holding a version of a
range's keys* being small — it's the near-entire live SSTable count,
for every key, because nearly every SSTable's flush touched nearly
every key.

### 4.3 Independent, deterministic reproduction (brief §9 — do not trust one soak)

Built as a new, standalone section of the existing benchmark harness
(`examples/read_engine_bench.rs`'s `section_overlap_repro`, run via
`cargo run --release --example read_engine_bench -- overlap_repro`,
**not** part of the default `ALL` run — same convention as Increment
4's `memory_scaling`). Design: a small, deliberately overlapping
keyspace (`KEY_CARDINALITY=20`) with a `memtable_max_size_bytes` tuned
so each flush cycle accumulates ~10× that cardinality in write events
(the same overlap *ratio* as this soak's real ~6.9×, not copied
verbatim — tuned up slightly so the effect is unambiguous in a small,
fast run), reproducing the soak's *shape* in **3m19s** wall-clock
instead of 4 hours.

A first attempt (500-key cardinality, 350-byte memtable ⇒ only ~2.8
writes/key/flush ⇒ ~0.6% per-table coverage probability) reproduced
only a **1.6–2.5** `sstables_consulted`/`sstable` ratio — nowhere near
the soak's ~490×, because the overlap ratio was too low. This negative
result is reported, not hidden: it is itself evidence that overlap
*ratio*, not raw SSTable count, is the driving variable. Retuned to
match the soak's real ratio (`KEY_CARDINALITY=20`,
`memtable_max_size_bytes=30,000` ⇒ ~200 entries/flush ⇒ ~99.995%
per-key coverage probability per table):

```
checkpoint target=20  actual_sstables=20  rss_kb=4,236  range100of500_elapsed_us=5,488.0   range_sstables_consulted=420   sstables_consulted/sstable=21.000
checkpoint target=50  actual_sstables=50  rss_kb=4,456  range100of500_elapsed_us=13,997.0  range_sstables_consulted=1,050 sstables_consulted/sstable=21.000
checkpoint target=100 actual_sstables=100 rss_kb=4,692  range100of500_elapsed_us=28,358.0  range_sstables_consulted=2,100 sstables_consulted/sstable=21.000
checkpoint target=200 actual_sstables=200 rss_kb=5,036  range100of500_elapsed_us=57,533.0  range_sstables_consulted=4,200 sstables_consulted/sstable=21.000
checkpoint target=300 actual_sstables=300 rss_kb=5,488  range100of500_elapsed_us=93,726.0  range_sstables_consulted=6,300 sstables_consulted/sstable=21.000
```

**Verified, not assumed**: `sstables_consulted/sstable` is **exactly
21.000 at every checkpoint** (420=20×21, 1050=50×21, 2100=100×21,
4200=200×21, 6300=300×21 — exact integer multiples, not approximate) —
proving `sstables_consulted` per scan scales *precisely linearly* with
live SSTable count once the overlap ratio is high enough, matching
§4.1's traced mechanism exactly (21 ≈ the 20 keys in the scanned range,
each requiring ~1 consultation per live SSTable). Range-scan elapsed
time also grows roughly linearly here (5,488us → 93,726us, ×17.1, for
a ×15 SSTable-count increase) — close to linear, **not** the soak's
observed ×37,414-for-×119.4 (~n^2.2) blowup. RSS in this tiny-table
repro stays small and grows slowly (4,236 → 5,488 KB), consistent with
§1's finding that RSS scale depends on table content size, not scan
behavior.

**Honest gap, flagged rather than papered over**: this repro
structurally proves the *linear-in-live-SSTable-count-per-yielded-key*
mechanism (§4.1/§4.2) is real and reproducible on demand. It does
**not** fully explain the soak's steeper apparent ~n^2.2 exponent. Two
plausible, unverified-here contributing factors, stated as open
questions rather than asserted as proven: (a) this soak's real
SSTables carry ~27,424 entries each with 16–256-byte values — much
larger per-table indexes than this repro's ~200-entry tables, so each
individual `peek_sstable` call (binary search + block read/decode)
itself costs more as tables grow, compounding the linear-in-count
mechanism; (b) the soak ran 8 concurrent writers + 16 concurrent
readers + a health sampler generating real CPU/disk contention absent
from this repro's single unthreaded process. Both are plausible,
neither is proven here — no additional experiment was run to isolate
them, per the brief's instruction not to chase every angle before
reporting.

## 5. Memory ownership — completed scan lifetime

Traced directly in `src/lsm/mod.rs`, not assumed:

- `RangeScanIter` (`:551-570`) owns its entire state: `read_view:
  ReadView` (an owned struct, not a reference), `Vec<Option<Bound<Vec
  <u8>>>>` resume points (owned `Vec<u8>` clones, no borrows), a
  `BinaryHeap<Reverse<HeapEntry>>` where each `HeapEntry` (`:483-492`)
  owns its `key: Vec<u8>` and `versions: Vec<(u64, MemtableValue)>` —
  no field of `RangeScanIter` borrows from anything outside itself.
- `ReadView` (`:406-417`) holds `Vec<Arc<MemTable>>`/`Vec<Arc<SsTable>>`
  — `Arc` clones taken once at `capture_read_view` time (a refcount
  bump each, explicitly documented as never cloning `BloomFilter`/
  index contents, `:399-403`) and `active_range: Vec<((Vec<u8>, u64),
  MemtableValue)>`, fully materialized (owned) at construction, never
  re-queried.
- No engine-side list, cache, or registry holds a reference to a
  `RangeScanIter` once returned to the caller (§1.1's `grep` result).
  There is no code path by which the engine could keep one alive after
  the caller drops it.
- Consequence, verified by the absence of any such path rather than by
  a runtime memory-profiler trace (not available in this environment):
  when a caller drops a `RangeScanIter` (goes out of scope, or the
  scan runs to completion and is dropped), every `Arc<SsTable>`/
  `Arc<MemTable>` clone it held is dropped, decrementing those
  refcounts back to whatever the live engine's own `sstables`/
  `immutables` lists still hold; every `HeapEntry`'s owned `Vec`s drop
  with it; nothing survives. This matches this soak's own observation
  that `handles` (§6 below) tracked SSTable *count*, not cumulative
  scan count, across 261,455 range scans issued — had scan-held state
  leaked, handle count (or RSS) would show a visible dependency on
  `range_scans_issued`, and it does not (handles vs. `sstables`: see
  §6; RSS's only monotonic driver, per §1, is SSTable count).

## 6. Snapshot registry

`snapshots_live=50` at **every one of the 115 samples** — constant,
never higher, never lower (`sorted(set(snapshots_live)) == [50]`,
computed over the full series, not eyeballed).

**Traced, not assumed, to the test harness itself, not the engine**:
`read_write_soak_test.rs`'s `snapshot_loop` (`:474-485`) calls
`SnapshotPool::take` then `SnapshotPool::prune(50)` every 2 seconds —
`prune` (`:306-311`) removes the oldest snapshot whenever the pool
exceeds 50. The constant 50 is this harness's own deliberate cap, not
an engine-side ceiling or leak.

**Engine-side registry, traced in `src/lsm/mod.rs:266-342`**:
`SnapshotRegistry` is a `Mutex<BTreeMap<u64, u64>>` multiset
(`seq -> outstanding count`). `acquire`/`release` are simple
increment/decrement-and-remove-at-zero operations
(`:280-296`) — `release` is called from `Snapshot`'s `Drop` impl
(`:338-342`), unconditionally, every time a `Snapshot` value is
dropped. `oldest()` (`:298-303`) reads `counts.keys().next()` — never
reports a fully-released `seq`. This is correct multiset semantics by
inspection: registry size can never exceed live-snapshot count,
dropping a snapshot always shrinks or removes its entry, no leak path
exists. **No engine change made or needed** — the brief's own
instruction ("do not modify snapshot semantics without evidence") is
honored: there is no evidence of a problem here.

## 7. Memory scaling test (bounded SSTable-count sweep)

Increment 4's own `memory_scaling` section already ran exactly this
sweep (100/500/1000/2000/5000 SSTables) on a disjoint-key,
point-plus-range-mixed workload — see
`PHASE_READ_ENGINE_PERFORMANCE.md`'s Increment 4 section for that full
table, preserved there, not duplicated here. §4.3 above extends the
same *shape* of sweep (20/50/100/200/300 SSTables) specifically on an
*overlapping*-key workload, isolating range-only cost from that
workload's own memory/latency curve — the direct
overlapping-vs-disjoint comparison this section exists to produce is
in §1.2 (memory) and §4.3 (range latency: linear-with-overlap here vs.
Increment 4's linear-with-count-alone on disjoint keys). A
point-only/range-only/mixed three-way split at 2000+ SSTables on the
overlapping keyspace was not additionally run — §4.3's linear result
was unambiguous enough (exact integer-multiple scaling at every
checkpoint) that a larger sweep was judged unlikely to add new
information, per the brief's own "do not chase every angle" framing;
flagged here as something a future increment could still add if the
ADR process below decides it needs more data before choosing an
optimization approach.

## 8. Handle / thread check

`handles`: 109 (sample 1, 5 sstables) → 718 (peak, near end, ~613
sstables) — `(718-109)/(613-5) ≈ 1.00` handle per additional SSTable,
confirming Increment 3 §8's "one open `File` per `SsTable`, held for
its whole lifetime" finding continues to hold at this scale, under
real concurrent load, across 261,455 range scans and 4,582,352 point
reads — no evidence of a per-read or per-scan handle leak (handle
count tracks SSTable count, not read/scan volume).

`threads`: stable at 29–31 for the entire 14,400-second active
workload (8 writers + 16 readers + snapshot-taker + health-sampler +
background flush/WAL threads + main = matches expectation), dropping
to 4 immediately after `stop.store(true, ...)` at `t=14,509.5s`
(sample 115) once writer/reader/snapshot threads join — a clean,
expected shutdown transition, not a leak.

## 9. Certification status (per brief §11 — do not collapse into PASS/FAIL)

- **Correctness: PASS.** Unchanged from Increment 4's own summary:
  `in_run_mismatches=0`, `recovery_ok=true`,
  `post_recovery_mismatches=0`, `storage_state` stayed `Healthy`
  throughout, `capacity_backpressure_events=0`.
- **Performance: OPEN.** Point-lookup scaling matches Increment 3's
  already-known linear-with-SSTable-count behavior (not new, not a
  regression). Range-scan scaling on this project's own established
  realistic (overlapping-key) workload is **super-linear** (§2) and
  traced to a real, reproducible mechanism (§4) — not yet addressed,
  not something this increment is authorized to fix unilaterally.
- **Memory: OPEN, but no leak found.** Monotonic growth is structurally
  explained (§1.1, per-SSTable index+bloom, expected pre-Compaction).
  Non-monotonic swings are plausibly, not conclusively, attributed to
  OS-level working-set volatility (§1.1) — no application-level
  ownership path for a real leak was found (§1.1, §5), but the precise
  cause of the swings was not independently confirmed with a memory
  profiler (not available here). "OPEN" reflects that residual
  uncertainty about the swings specifically, not a suspected leak.

**READ ENGINE remains NOT READY** — unchanged from Increment 4's own
status, for the reasons above, not because the completed soak failed
(it did not: `RESULT=PASS` stands, unmodified, as historical evidence).

## 10. Optimization decision

Evidence in §4 shows the current per-key `refill` cursor design is a
real, reproducible bottleneck **specifically on overlapping-keyspace
workloads** (this project's own established "realistic" endurance
profile, not a contrived edge case) — meeting the brief's own bar for
writing an ADR. See `PHASE_READ_ENGINE_RANGE_PERFORMANCE_ADR.md`
(new, this increment) for the options evaluated. **No optimization was
implemented in this increment** — the ADR presents options for a
future increment's decision, per the brief's explicit instruction.
