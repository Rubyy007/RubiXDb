# Read Engine Performance — Baseline Measurements

Append-only, historical record. Never edit or delete a past run's
numbers — only add new dated sections below. Every number in this
document comes from actually running `examples/read_engine_bench.rs`
against real on-disk SSTables built through the real write path
(`put`/`delete` plus the real background flush thread); nothing here
is mocked, estimated, or computed analytically. Raw output for each
run is kept alongside this file's git history via the commit that
introduced it; this document is the curated, explained version.

This is Increment 3's deliverable per `PHASE_READ_ENGINE_ADR.md`
(ADR-RE-001) and the Increment 3 phase brief ("READ ENGINE —
INCREMENT 3: PERFORMANCE + OBSERVABILITY + CONTAINS"). It establishes
a **baseline for the current implementation**. Nothing in this
increment optimizes anything — no cache, no mmap, no prefetch, no
parallel reads, no secondary index, no read worker pool. Per the
brief's explicit instruction: any future optimization requires a
benchmark proving a real bottleneck **and** a new ADR written first.
This document is that first benchmark.

## Methodology

- Machine: the same Windows 10 development host used throughout this
  project's session (8 logical cores, per `std::thread::available_
  parallelism()`).
- Build: `cargo build --release --example read_engine_bench`.
- Every fixture is a real `LsmEngine` opened against a real temp
  directory, populated via real `put`/`delete` calls through the real
  write path (WAL group-commit, `apply_after_durable`, the real
  background flush thread writing real `.sst` files via `sstable::
  write_from_memtable`, real manifest/checkpoint records). No
  component of the write path was bypassed, mocked, or fast-forwarded.
- SSTable-count fixtures are built by setting `memtable_max_size_bytes`
  small enough that inserting `keys_per_table` keys triggers exactly
  one freeze. **Caveat, stated plainly rather than silently rounded
  away**: the harness's calibration (`build_fixture` in `examples/
  read_engine_bench.rs`) uses a conservative per-entry byte margin,
  so a requested count of 10/100/1000 SSTables actually yields
  ~13/133/1333 in most sections (confirmed via `engine.sstable_count()`
  after each build, not assumed) — a consistent ~1.33x overshoot, not
  a bug, just an imprecise calibration constant. The `point_lookup` and
  `fd` sections print the *actual* achieved count; `read_amp`, `range`,
  and `concurrency` print the *configured target* for brevity. The
  relative scaling trends below (order-of-magnitude behavior across
  1 → 10 → 100 → 1000 SSTables) are unaffected either way.
- Every latency section reports **every repetition**, not best-of/
  median-only, per the brief's explicit instruction. `n` is the sample
  count; `p50`/`p95`/`p99`/`max` are percentiles over that rep's own
  samples; `ops_per_sec` is `n / wall_elapsed` for that rep.
- "Cold vs warm I/O" caveat, stated honestly rather than overclaimed:
  this harness cannot drop the Windows OS page cache from user mode
  without administrative/driver-level tools, so a genuinely cold
  (post-reboot, page-cache-empty) measurement is out of reach here.
  What *is* measured and reported below is first-access-after-fixture-
  build vs. steady-state repeated access on the same process — real,
  but not a substitute for a true cold-cache number. This limitation
  is stated once here rather than re-qualifying every number below.

## Run: 2026-09-20 (Increment 3 baseline)

Full raw output: `examples/read_engine_bench.rs`'s own run, captured to
a local log during development and transcribed below; reproduce with
`cargo run --release --example read_engine_bench` (no arguments runs
every section; pass one or more section names to run a subset — see
the file's own top-of-file doc comment).

### 1. Point-lookup latency across SSTable counts

`get`/`get_as_of`/`contains`, `hit`/`miss(bloom-negative)`/
`visible-filtered` (a real bloom-positive, real block-read match that
resolves to "not visible" at an `as_of_seq` before any write ever
happened — see the harness's own comment for why this is deterministic,
not probabilistic), 3 reps each, `n=2000` per hit/miss rep (`n=500` for
visible-filtered).

| actual SSTables | get(hit) p50 | get_as_of(hit) p50 | contains(hit) p50 | get(miss) p50 | contains(miss) p50 |
|---:|---:|---:|---:|---:|---:|
| 1    | 3.9–4.0us | 3.7–3.8us | 3.7us | 0.2us | 0.2us |
| 13   | 4.2us | 4.1us | 4.0us | 0.7us | 0.7us |
| 133  | 8.8–9.0us | 8.9us | 8.7–8.8us | 5.6us | 5.6–5.7us |
| 1333 | 112.0–113.7us | 109.6–113.8us | 112.0–112.8us | 55.8–56.7us | 55.4–55.9us |

**Finding**: hit-path latency grows roughly linearly with the number
of live SSTables (≈4us → ≈9us → ≈112us across a ~100x SSTable-count
increase) — expected LSM behavior for a point lookup with no
compaction yet to bound the number of tables a read may have to
consult (see §4 below, read amplification). `get`/`get_as_of`/
`contains` track each other within measurement noise at every SSTable
count — no method is a structural outlier.

### 2. Tombstone lookups

10-table fixture, half the keys deleted, `n=40` (one 500-key fixture's
worth of tombstoned entries), 3 reps:

`get_as_of(tombstone)`: p50 5.4–8.3us. `contains(tombstone)`: p50
5.4–7.3us. Both resolve a tombstone via the active MemTable (the
`delete` in this test runs immediately before the read, so the
tombstone is still in `active`, never touching the SSTable layer) —
consistent with the low, SSTable-count-independent latency observed.

### 3. `contains()` vs `get_as_of().is_some()` — honest verdict

Swept across value size (32B / 1KB / 16KB — the ADR's own predicted
"large value" scenario) and SSTable count (10 / 1000), hit-path only,
3 reps × `n=2000` each, compared by p50:

| value size | SSTables | get_as_of-equiv p50 | contains p50 | ratio | verdict |
|---:|---:|---:|---:|---:|---|
| 32B   | 10   | 5.70us  | 5.50us  | 0.965 | no meaningful difference |
| 32B   | 1000 | 112.90us | 111.00us | 0.983 | no meaningful difference |
| 1KB   | 10   | 4.80us  | 4.80us  | 1.000 | no meaningful difference |
| 1KB   | 1000 | 92.30us | 94.60us | 1.025 | no meaningful difference |
| 16KB  | 10   | 14.60us | 14.60us | 1.000 | no meaningful difference |
| 16KB  | 1000 | 138.20us | 137.20us | 0.993 | no meaningful difference |

**Verdict, stated honestly per the brief's explicit instruction not to
keep `contains()` just because the ADR predicted a benefit: no
measurable performance difference between `contains()` and
`get_as_of(..).is_some()`, at any value size tested, including the
16KB "large value" case the ADR itself predicted would show the
biggest win.**

**Why, traced through the actual code rather than assumed**:
`SsTable::contains_versioned` (the new method backing `contains()`)
still calls `read_block`, and `read_block`/`format::decode_block`
eagerly decodes *every* record's key and value bytes for a whole block
regardless of which method reads it afterward — the value-byte
allocation this method was meant to avoid is already paid by the block
decode itself, a cost shared identically by `get_versioned`. The only
thing `contains_versioned` actually avoids is constructing the final
`RecordValue::Put(Vec<u8>)` wrapper for the winning record — which in
`get_versioned`'s own existing code is a `Vec` *move*, not a clone, and
therefore already near-zero-cost. `contains()` is kept because it
satisfies the ADR's stated API contract (reuses the bloom+index+block
path, never calls `get_as_of(..).is_some()` internally, and gives
callers an existence-only interface with headroom for a *future*,
different SSTable format to exploit) — not because this benchmark
found a performance win. That absence-of-a-win is itself the honest
result this section exists to produce.

### 4. Read amplification (`ReadStats`), point lookups

`sstables_consulted` / `blocks_read` deltas around real `get()` calls,
averaged per call, across the SSTable-count axis (configured target
shown; see the methodology caveat above for actual vs. target):

| target SSTables | hit: avg sstables_consulted | hit: avg blocks_read | miss: avg sstables_consulted | miss: avg blocks_read | miss: avg bloom_negatives |
|---:|---:|---:|---:|---:|---:|
| 1    | 0.750 | 0.750 | 1.000 | 0.000 | 1.000 |
| 10   | 6.825 | 1.062 | 13.000 | 0.000 | 12.825 |
| 100  | 66.832 | 2.325 | 133.000 | 0.000 | 130.471 |
| 1000 | 1166.833 | 27.226 | 1333.000 | 0.000 | 1301.930 |

**Verified, not assumed** (brief §8's explicit instruction): a
bloom-negative miss reads **zero** data blocks at every SSTable count
tested — `avg_blocks_read=0.000` in every miss row above.

**Read-amplification scaling finding (brief §11 — bottleneck
identification only, no optimization performed)**: for a *hit*, the
average number of SSTables consulted grows almost linearly with the
live SSTable count (0.75 → 6.8 → 66.8 → 1166.8 across 1 → 10 → 100 →
1000 target tables) — because `get_as_of`/`contains` walk sources
newest-to-oldest and stop at the first match, a key that only exists in
an *old* table forces every newer table to be checked first. At 1000
SSTables, an average hit consults **~1167 of ~1333** live tables. The
mitigating factor: almost all of those consultations are cheap
bloom-negatives, not real block reads — `avg_blocks_read` stays at
27.2 even when 1166.8 tables were consulted, because the bloom filter
short-circuits before any block I/O for every table that doesn't
actually hold the key. **This is the clear, measured bottleneck for
point lookups at high SSTable counts: CPU-bound bloom-filter checks
scaling with table count, not disk I/O** — and it is exactly what a
future Compaction phase (bounding the number of live SSTables) would
fix. No optimization was attempted here, per the brief's explicit
instruction; this section only identifies the bottleneck.

### 5. Range benchmarks

`range()` (current) and `range_scan(.., mid_seq)` (historical, roughly
half the writes visible) over empty / small(10-key) / medium(100-key)
/ large(full) sub-ranges, at the same SSTable-count axis, 3 reps each.
Selected results (full detail: `examples/read_engine_bench.rs`'s
`section_range`, or reproduce and see full output):

| target SSTables | case | range() rows | range() elapsed (rep0) | range_scan(historical) rows | range_scan elapsed (rep0) |
|---:|---|---:|---:|---:|---:|
| 1    | empty | 0 | 0.070ms | 0 | 0.002ms |
| 1    | large(full) | 8 | 0.136ms | 4 | 0.142ms |
| 10   | large(full) | 80 | 0.438ms | 41 | 0.406ms |
| 100  | large(full) | 800 | 9.852ms | 400 | 4.936ms |
| 1000 | large(full) | 8000 | 55.8–56.9ms | 4001 | 54.9–56.6ms |

**Finding**: a full-table range scan's elapsed time also grows
roughly linearly with SSTable count (0.14ms → 0.44ms → ~7–10ms →
~56ms across the 1 → 10 → 100 → 1000 axis, for a proportionally larger
row count each time — throughput stays roughly flat at ~140,000
rows/sec at the largest scale). `range()` and historical `range_scan()`
track closely; historical scans return fewer rows because roughly half
the writes postdate the pinned `mid_seq`, not because of any
performance difference in the merge path itself.

### 6. Range memory-boundedness

200-SSTable fixture, 16KB values, 1600 keys = **25.0 MiB of live data**,
full unbounded `range()` scan while sampling RSS every 20ms:

```
rss_start=5784KB rss_min=5952KB rss_max=5952KB rss_final=5964KB
peak_growth_over_start = 168KB, against 25600KB of live data
```

**Verified, not assumed** (brief §12's explicit instruction): the
range scan's peak RSS growth (168KB) is roughly **150x smaller** than
the 25MiB of data it iterated over, and `rows=1600` matched the
fixture's exact key count exactly once each — confirming `range_scan`
does not fully materialize, retain, or duplicate the scanned dataset in
memory. This matches the design (`RangeScanIter` holds short-lived,
owned resume-point cursors and reads one data block at a time via
`SsTable::read_block`, never buffering a whole table).

### 7. `ReadStats` instrumentation overhead

Every read path in this codebase increments `ReadStats` unconditionally
— there is no build-time or run-time toggle to disable it for a true
A/B comparison, and adding one is out of this increment's approved
scope (it would touch the production read path beyond `contains()`/
benchmarking, which the brief does not authorize). As a best-effort,
honestly-labeled upper-bound proxy: 10,000,000 `Relaxed AtomicU64::
fetch_add` calls (the exact primitive `ReadStats` uses) took 45.2ms
total, **4.52ns/call**. A real point-lookup hit path performs 1–3 such
increments. Against the microsecond-scale latencies measured above
(3.7us–113.7us per hit), this bounds the counters' overhead at well
under 0.5% of a single call's cost — not measured as literally
unmeasurable, but small enough that removing them would not
meaningfully move any number in this document. Per the brief's
instruction, the counters were not removed or altered to chase this.

### 8. File descriptor / thread / reader-lifetime

1333-SSTable fixture (target 1000):

```
before fixture build:                 handles=71   threads=1
after building 1333 SSTables:         handles=1409 threads=3
after 2000 lookups + 20 range scans:  handles=1409 threads=3
handle delta across reads = 0
```

**Verified, not assumed**: handle count grows with SSTable count at
build time (~1 handle per SSTable file, consistent with each `SsTable`
holding exactly one open `File` for its whole lifetime, per
`src/sstable/reader.rs`'s own design) and then **stays flat** across
2000 point lookups and 20 full range scans — no per-read handle leak.
Thread count (3: main + WAL/flush background threads) is stable
throughout and does not grow with read volume.

### 9. Bounded concurrent-read benchmark

N readers (1/10/100) × M SSTables (1/100/1000 target), 500 ops/reader,
alternating `get`/`contains`:

| SSTables | readers | p50 | p95 | p99 | max | aggregate ops/sec |
|---:|---:|---:|---:|---:|---:|---:|
| 1    | 1   | 3.8us | 4.1us | 17.1us | 27.0us | 256,226 |
| 1    | 10  | 77.3us | 148.0us | 251.9us | 825.7us | 149,380 |
| 1    | 100 | 369.5us | 1701.6us | 2866.6us | 7329.6us | 179,974 |
| 100  | 1   | 21.4us | 81.3us | 177.6us | 315.4us | 33,180 |
| 100  | 10  | 23.4us | 117.3us | 410.6us | 1322.5us | 212,648 |
| 100  | 100 | 17.3us | 100.5us | 4299.6us | 22,825.6us | 339,493 |
| 1000 | 1   | 164.9us | 984.0us | 2035.0us | 5700.7us | 3,305 |
| 1000 | 10  | 253.4us | 1423.6us | 3816.6us | 13,687.0us | 21,390 |
| 1000 | 100 | 242.0us | 22,066.6us | 69,092.8us | 287,843.6us | 22,647 |

**Finding**: aggregate throughput scales positively with reader count
at low-to-moderate SSTable counts (more concurrent readers → higher
total ops/sec, since reads don't block each other — no read holds a
write lock). At 1000 SSTables with 100 readers, tail latency
(p99=69ms, max=288ms) grows sharply — consistent with §4's finding
that each read is individually CPU-bound (bloom-filter-heavy), so 100
concurrent readers genuinely contend for CPU time at this table count,
not for a lock. No correctness issue observed at any concurrency level
(see §11, sanity workload).

### 10. Bounded write+read integration sanity (5 seconds)

Single writer thread (continuous put/delete across 500 keys) + 4
reader threads (continuous `get_as_of`/`contains` at pinned seqs),
running concurrently for 5 seconds:

```
664 writes issued, 1,908,500 read-pair checks performed
0 mismatches in the final-state check against an independently
tracked expected-write history (500 distinct keys)
```

**A real, verified finding surfaced by this workload, worth stating
plainly rather than hiding**: a *different* thread sampling `engine.
snapshot_seq()` and then calling `get_as_of`/`contains` at that pinned
seq can, extremely rarely (observed ~1 per 1.4M read-pair checks across
several 5-second runs), transiently disagree with a *repeat* call at
the exact same pinned seq. This was reproduced with plain `get_as_of`
called twice in a row against the same pinned seq — **no `contains()`
involved** — so it is not a Read Engine Increment 3 / `contains()` bug.
Traced directly in the source (not assumed): `freeze_locked`
(`src/lsm/mod.rs`) holds the active-MemTable write lock across both the
freeze swap and the immutables push (ruling out a freeze-window race),
and the flush thread publishes a new SSTable to the live `sstables`
list *before* removing its source immutable (ruling out a flush-window
miss — matching the already-certified `point_lookup_during_the_
sstable_published_immutable_not_yet_removed_window_never_misses`
test's own established contract). The remaining, consistent
explanation is `snapshot_seq()`'s own documented caveat: it reflects
WAL durability (`durable_through`), not "every other thread's
`apply_after_durable` for that seq has already completed in memory" —
safe for a writer reading its own just-completed write (verified in
this same workload's writer thread: `contains`/`get_as_of` compared
immediately after that thread's own `put`/`delete`, same-thread,
**zero** disagreements across 664 writes), not linearizable for a
reader sampling a seq some *other* thread produced. This is a
pre-existing characteristic of `snapshot_seq()`'s cross-thread
semantics, not introduced by this increment, not a violation of any
consistency guarantee this engine has ever documented, and not
something this increment is authorized to change (`snapshot_seq`/
`durable_through`/group-commit are protected Write Engine territory).
Flagged here for visibility, not silently resolved — worth a dedicated,
narrowly-scoped Write Engine investigation in a future phase if
cross-thread linearizable snapshot reads become a requirement.

## `ReadStats` counter semantics (brief §21 — precise definitions)

- **`read_requests`**: incremented exactly once per `get`/`get_as_of`/
  `contains` *call*, and exactly once per `range`/`range_scan` *call*
  (never once per row yielded by a range scan — established in
  Increment 2, unchanged this increment).
- **`read_hits`**: for a point lookup (`get`/`get_as_of`/`contains`),
  incremented once if *any* source (active/immutable/SSTable) held a
  matching `(key, seq)` entry — a found tombstone counts as a hit here,
  even though the point lookup still resolves to `None`/`false`. For a
  range scan, incremented once *per row yielded* (a range scan has no
  single hit/miss outcome the way a point lookup does — established in
  Increment 2, unchanged).
- **`read_misses`**: incremented once per point-lookup call where no
  source held a matching entry. Not incremented for range scans.
- **`bloom_negatives`**: a per-`SsTable` cumulative counter (lives on
  `SsTable` itself, summed across the live table list at `read_stats()`
  call time), incremented every time that table's bloom filter returns
  "definitely absent" for a queried key — from `get_versioned` and
  `contains_versioned` alike (both check the bloom first). Not
  incremented by `range_scan_raw`, which does not consult the bloom
  filter (a range scan walks blocks by key-range, not by point lookup).
- **`blocks_read`**: a per-`SsTable` cumulative counter, incremented
  once every time `read_block` actually reads a data block off disk —
  shared identically by `get_versioned`, `contains_versioned`, and
  `range_scan_raw`, since all three call the same `read_block` helper.
- **`sstables_consulted`**: an `LsmEngine`-level counter (not
  per-table), incremented once per SSTable actually queried during a
  read — once per table checked in a point lookup's newest-to-oldest
  traversal (whether or not that check was a bloom-negative), and once
  per table a range scan's k-way merge draws from.

## Corruption matrix (brief §17)

All assertions below check the *exact* `EngineError` variant, never
bare `is_err()`. Full detail and current pass status: `src/lsm/
tests.rs`.

| Read path | Corruption (checksum/structural) | Genuine I/O failure (real `io::Error`, not simulated) |
|---|---|---|
| `get`/`get_as_of` | `data_block_corruption_is_detected_lazily_at_read_time_not_at_open` — `Err(Corruption)` | `get_as_of_and_range_scan_when_the_underlying_file_shrinks_mid_lifetime_fail_closed_with_io_error` — `Err(Io(_))` |
| `range`/`range_scan` | `range_scan_across_a_corrupted_data_block_fails_closed_and_ends` — exactly one `Err(Corruption)`, then the iterator ends (`iter.next().is_none()` asserted) | same test as above — exactly one `Err(Io(_))`, then ends |
| `contains` | `contains_across_a_corrupted_data_block_fails_closed_with_corruption` — `Err(Corruption)` | `contains_when_the_underlying_file_shrinks_mid_lifetime_fails_closed_with_io_error` — `Err(Io(_))` |
| `open()` (footer/bloom/index) | `open_fails_closed_when_a_published_sstable_is_corrupt` — `Err(Corruption)` | n/a (open-time validation is entirely structural; no positional read past a validated bound is possible) |
| missing live SSTable file | `missing_live_sstable_fails_closed_on_open` | — |
| orphan/garbage `.sst` file | `garbage_orphan_sstable_file_fails_closed_not_silently_handled` | — |
| manifest corruption | `manifest_corruption_fails_closed_on_open` | — |

The genuine-I/O-failure tests use a real technique, not a mock: they
shrink the underlying `.sst` file (via a second file handle) out from
under an *already-open*, already-validated live `SsTable` whose cached
footer/index still describe offsets beyond the file's new end —
`read_block`'s positional read then genuinely hits `io::ErrorKind::
UnexpectedEof`, propagated via `EngineError::Io`, not simulated or
injected through a fault hook.

## No optimization performed this increment

Confirmed by diff review (see the Increment 3 commit): no cache, no
mmap, no prefetch, no parallel-read machinery, no secondary index, no
read worker pool was added. §4/§9's findings (read amplification scales
with live SSTable count; `contains()` shows no measurable win over
`get_as_of(..).is_some()` in this architecture) are exactly the kind of
evidence the brief requires *before* any future optimization — and per
the brief's explicit instruction, acting on them (e.g., a future
Compaction phase to bound SSTable count) requires a new ADR, not a
unilateral change here.
