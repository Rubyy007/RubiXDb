# RubiXDB Phase 4B — Performance

Companion to `PHASE4B_TEST_RESULTS.md` (the authoritative pass/fail
source) — this file collects Phase 4B's benchmark numbers, measured
separately by category (operating brief §39), plus the integrated
WAL-only vs. WAL+MemTable vs. WAL+MemTable+SSTable-flush comparison
(operating brief §40-41) that closes the exact gap `PHASE4A_ADR.md`
ADR-P4A-6 and `PHASE4A_PERFORMANCE.md` §3 left open.

## 1. Environment

Unchanged from every prior phase this session: Intel Core i7-7700 (4
physical / 8 logical cores), 16 GiB RAM, SATA SSD (`E:`), Windows 10
Home 10.0.19045, `rustc`/`cargo` 1.98.1. Verified idle before measuring
(`Get-Process` showed no lingering soak/load-test process from a prior
session) — the exact contamination check `PHASE4A_ADR.md` ADR-P4A-6
skipped is applied here.

## 2. RUBIC SSTable, standalone (`examples/sstable_bench.rs`, n=500,000 records)

Single-threaded, direct `write_from_memtable`/`SsTable::open` calls —
isolates the SSTable component itself from the WAL/coordinator/MemTable
write path (measured separately, §3 below, per operating brief §39's
"do not mix benchmark categories").

| Metric | Result |
|---|---|
| Write throughput | 102.40 MB/sec, 1,376,721 records/sec |
| Resulting file size | 38,995,493 bytes (for 29,500,000 bytes of raw key+value payload — ~32% overhead: per-record length prefixes/seq/op, block checksums, index, bloom, footer) |
| `open()` latency | 1.699 ms (9,260 blocks, 500,000 records — footer+bloom+index only, no data block read) |
| Point lookup (present key, warm) | p50=12µs, p95=25µs, p99=34µs, max=111µs |
| Ordered iteration | 4,965,544 records/sec |
| Absent-key lookups | 3,049,599 lookups/sec, zero false negatives (by construction — bloom-filter false-positive *rate* is measured separately and statistically in `sstable::bloom::tests::false_positive_rate_within_reasonable_bound_of_target`, not by timing) |
| Writer memory (whole-process RSS delta across the write) | +2,700 KB |
| Reader memory (whole-process RSS delta across `open()`) | -1,276 KB net decrease at measurement time — see note below |

**Reader-memory note, not silently smoothed over**: the RSS sample
*before* `open()` (5,156 KB) is far below the RSS sample *after* the
write phase (107,592 KB) from earlier in the same run. This is not a
measurement bug: `drop(memtable)` runs immediately after the write
completes, releasing the 500,000-entry `BTreeMap` (tens of MB), and the
`sample_rss_kb` helper's own PowerShell-subprocess round trip takes
long enough (tens of milliseconds) that Windows' memory manager visibly
trims the process's working set in between. The reported reader-memory
figure is therefore a genuine, if noisy, whole-process delta across
`open()` alone, not attributable to the writer's already-freed memory.

**Interpretation**: `open()` never reads data blocks (bounded I/O at
open time, operating brief §20/§38) — its cost and memory footprint are
dominated by the index/bloom blocks alone, both of which stay small
relative to the data even at 500,000 records. Point-lookup latency
(single-digit-to-tens of microseconds) is dominated by one `pread`-class
positional read plus block-checksum verification, consistent with a
warm-cache SSD read plus CRC32C computation over a ~4 KB block — three
orders of magnitude below WAL `fsync` latency, the same order-of-
magnitude relationship `PHASE4A_PERFORMANCE.md` §2 already established
for MemTable operations.

## 3. Integrated: WAL-only vs. WAL+MemTable vs. WAL+MemTable+SSTable-flush

Methodology: `examples/batch_coordinator_load_test.rs` (WAL-only,
unmodified since Phase 2B), `examples/lsm_load_test.rs` (WAL+MemTable,
512 MiB memtable — never triggers a freeze, so this measures the
MemTable layer's steady-state cost with zero flush activity, unchanged
methodology from Phase 4A), `examples/lsm_flush_load_test.rs` (new this
phase — same write path, but a small `memtable_max_size_bytes` so
freeze/flush is continuously active throughout the run). All three use
identical `WalConfig`/writer-count/per-thread arguments, so the numbers
are directly comparable per operating brief §40's own requirement.

### 3.1 100 writers, 1,000 ops/writer (100,000 total), two repetitions each

| Configuration | Run 1 ops/sec | Run 2 ops/sec | p50 / p95 / p99 (run 1) |
|---|---|---|---|
| WAL-only | 21,505 | 20,879 | 4.75 / 5.11 / 7.24 ms |
| WAL+MemTable (512 MiB, no flush) | 17,154 | 17,007 | 4.77 / 9.08 / 10.59 ms |
| WAL+MemTable+flush (65,536 B memtable, 60 SSTables produced) | 19,260 | 20,197 | 4.76 / 7.51 / 10.21 ms |

### 3.2 1,000 writers, 1,000 ops/writer (1,000,000 total), single run each

| Configuration | ops/sec | p50 / p95 / p99 / max | SSTables produced |
|---|---|---|---|
| WAL-only | 97,564 | 9.90 / 11.95 / 15.99 / 24.28 ms | n/a |
| WAL+MemTable (512 MiB, no flush) | 66,125 | 9.66 / 31.29 / 94.45 / 372.21 ms | 0 |
| WAL+MemTable+flush (65,536 B memtable — deliberately tiny, stress config) | 56,609 | 10.66 / 41.09 / 120.42 / 571.66 ms | 622 |
| WAL+MemTable+flush (**4,194,304 B memtable — the LSM spec's own actual default**) | **98,666** | 9.60 / 12.63 / 17.24 / 25.65 ms | 9 |

### 3.3 Interpretation — flagged, not silently resolved

**Under the LSM spec's own real default (`memtable_max_size_bytes` =
4 MiB, `RubixDB-LSM-Engine-Specification-v1.0.md` §4.1), SSTable flush's
measured overhead at 1,000 concurrent writers is within noise of the
WAL-only baseline** (98,666 vs. 97,564 ops/sec — flushing only 9 times
across the whole 1,000,000-record run). This is the realistic,
production-configuration number.

**Under a deliberately tiny 65,536-byte memtable** (chosen specifically
to make flush activity dominate a short benchmark run, producing 622
small SSTables from the same 1,000,000 records), flush contention is
clearly measurable and real: ~14% slower than the same small-memtable
configuration's own 100-writer run pattern would predict, and both
p95/p99/max latency and total throughput degrade visibly relative to
the 4 MiB configuration at the same writer count. **This is expected,
not a defect**: many small-file creates/writes/fsyncs/renames
(`RUBIC_SSTABLE_FORMAT_SPECIFICATION.md` §3.4's atomic-publication
sequence, run 622 times) genuinely contend with the WAL's own `fsync`
calls for the same physical disk's I/O queue, on the same 4-core/
8-thread machine used throughout this project. The takeaway is a
configuration guideline, not a code defect: `memtable_max_size_bytes`
should be sized so flush frequency stays low relative to write volume
(the spec's own 4 MiB default already achieves this at this benchmark's
scale) — this observation is recorded here rather than silently
smoothed over, per this project's own "flag, don't silently resolve"
principle.

**The 100-writer table's own apparent oddity** (WAL+MemTable-no-flush
measuring *slower* than WAL+MemTable+flush, both repetitions) is noted
but **not fully explained by this phase's own testing** — a plausible
contributing factor is that `lsm_load_test.rs`'s 512 MiB memtable never
freezes during the run, so every insert lands in one `BTreeMap` that
grows to 100,000 entries by the end, while the flush variant's frequent
freezing keeps each active `BTreeMap` small throughout (bounded
`O(log n)` insert cost); this hypothesis is not confirmed by a dedicated
ablation isolating freeze-frequency from flush-I/O, and is recorded
as an open item (§5) rather than asserted as fact. At 1,000 writers the
same-small-memtable comparison instead shows the *expected* direction
(flush measurably slower than no-flush) once I/O contention dominates
at higher concurrency — the two data points are not contradictory, but
neither fully explains the other, and this document says so plainly
rather than picking whichever story is more convenient.

## 4. Regression comparison against the certified WAL foundation

**The certified WAL-only baseline** (`PHASE3C_TEST_RESULTS.md` §2):
100 writers 17,872–18,700 ops/sec (historical noise band
13,700–18,700), 1,000 writers ~91,517–97,600 ops/sec. This phase's own
WAL-only measurements (100 writers: 20,879–21,505; 1,000 writers:
97,564) fall at or slightly above the top of that historical band —
consistent with a genuinely idle machine, not a regression (Phase 4B
changes zero WAL/coordinator code, per `PHASE4B_ARCHITECTURE.md` §2).

**Phase 4B's own change to the WAL write path**: none. Every
`LsmEngine::put`/`delete` call is byte-for-byte the same call Phase 4A
already made; the only new code on that path is the `flush_sender.send`
call in `freeze_locked` (an unbounded-channel push, not I/O) and the
background flush thread itself, which runs on its own OS thread and
never blocks a writer directly — its cost shows up only indirectly, as
disk-I/O contention (§3.3 above), never as added latency inside
`put`/`delete`'s own call graph.

## 5. Open items, named explicitly

- The 100-writer "flush appears faster than no-flush" data point (§3.3)
  is not root-caused by a dedicated ablation test in this phase. A
  future pass isolating "freeze frequency alone" from "flush I/O alone"
  (e.g., a config that freezes often but discards the frozen memtable
  instead of building an SSTable from it) would resolve this cleanly.
- `sstable_bench.rs`'s writer/reader memory figures are whole-process
  RSS deltas (Windows `WorkingSet64`, sampled via a PowerShell child
  process), not an allocator-level attribution to the SSTable
  writer/reader specifically — the same caveat every prior phase's own
  RSS-based memory measurements in this project already carry.
- No compression, no per-block restart points (both explicit v1 scope
  cuts in the already-final LSM spec, §2.3) — block-scan cost at larger
  scales than measured here is unmeasured, matching the spec's own
  "revisit once real benchmark data shows it matters" framing.
