# Increment 16: Secondary-Index Read Performance — Measurements

Companion to `PHASE_RUBIXDB_INCREMENT16_INDEX_READ_ARCHITECTURE.md` and
`PHASE_RUBIXDB_INCREMENT16_INDEX_READ_RESULTS.md`. Every number here is
from `sql/src/index_read_benchmark.rs` (`#[ignore]`d in-process tests,
release build, real `LsmEngine` + catalog + SQL parse/bind/plan/execute)
or `api/src/routes/sql.rs::serialization_benchmark`. Raw outputs are
under `temp/inc16/out*/` (untracked scratch).

## 0. Method, environment, and honest limits

- **Machine:** Intel i7-7700 (4 cores / 8 threads), 16GB RAM, SSD,
  Windows 10. TEMP redirected to `E:` (C: has ~500MB free).
- **"Before" vs "after" are the same benchmark binary source, same
  machine, run back to back** by swapping only
  `src/relational/index.rs`, `src/relational/table_store.rs`,
  `sql/src/exec/operators.rs` to `HEAD` and back (backups kept, restored
  and verified). The planner fix F-1 and the benchmark code were present
  in both; F-1 affects only upper-bound-only ranges, which none of the
  measured queries use.
- **Two LSM regimes, reported separately — never mixed:**
  - **Compaction OFF** (library default `LsmConfig`: SSTables
    accumulate: 9 at 100K rows, 64 at 1M rows). Useful to show how the
    old per-row cost scales with LSM size; **overstates** production
    SSTable counts.
  - **Compaction ON** (`INC16_COMPACT=1`, the shipped server/CLI
    default): 0 SSTables at 10K rows (all MemTable), 2 at 100K, 4 at
    1M. **This is the production-representative regime and the headline.**
- **Dataset:** `ix(id INT PK, r INT = id, g1..g10000 TEXT, pad TEXT)`,
  81.6 B/row; `g{K}` = `"g{id % (N/K)}"` so `g{K} = 'g0'` matches
  exactly K rows for any N, spread across the whole PK domain. Each
  `g{K}` and `r` has a secondary index. The endurance replay uses the
  exact Blocker 9 schema (`id, grp, val, v`, `grp = g{id % 10}`).
- **Latency:** warm = adaptive repeat count (8–200) of the same prepared
  plan; "cold" = the first execution after setup (engine-internal state
  cold; OS page cache is not dropped; **process-cold / cache-dropped
  runs were not done** — named limit). p50/p95/p99/max reported; with
  fewer than ~100 samples p99 ≈ max (stated where it matters).
- **Resource counters:** CPU = process CPU time delta; RSS from
  `K32GetProcessMemoryInfo`; handles from `GetProcessHandleCount`;
  threads sampled via PowerShell. Rows examined / returned and engine
  `ReadStats` (requests / SSTables consulted / blocks read) are in the
  raw output lines (`idx_ex`, `scanned`, `reads(req/sst/blk)`).
- **Not measured:** 10M+ rows (disk/time), JSON/HTTP end to end under
  real network, process-cold reads, mixed read+write concurrency (writes
  are exercised only in the regression matrix and the correctness
  suite).

## 1. Root cause — stage decomposition (N = 100,000)

Median ms per stage. "× K" columns are the cost of doing that stage once
per matched row, K times, exactly as the old code did.

### 1a. Compaction ON (production-representative, 2 SSTables), BEFORE the fix

| K | parse | bind | plan | traverse (K entries) | + decode | catalog meta × K | old fetch × K | raw engine get × K | `index_lookup_as_of` | full SQL |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 0.010 | 0.058 | 0.079 | 0.022 | 0.022 | 0.040 | 0.054 | 0.011 | 0.161 | 0.167 |
| 10 | 0.011 | 0.058 | 0.076 | 0.025 | 0.028 | 0.393 | 0.519 | 0.098 | 0.651 | 0.656 |
| 100 | 0.010 | 0.057 | 0.074 | 0.059 | 0.083 | 4.134 | 5.562 | 1.143 | 5.919 | 5.860 |
| 1,000 | 0.010 | 0.057 | 0.077 | 0.400 | 0.618 | 39.60 | 53.68 | 10.78 | 57.36 | 57.60 |
| 10,000 | 0.010 | 0.057 | 0.066 | 3.669 | 5.791 | 397.4 | 526.1 | 99.16 | 557.0 | 556.4 |

### 1b. Compaction ON, AFTER the fix

| K | `index_lookup_as_of` | full SQL |
|---|---|---|
| 1 | 0.089 | 0.096 |
| 10 | 0.197 | 0.213 |
| 100 | 1.477 | 1.608 |
| 1,000 | 14.05 | 15.20 |
| 10,000 | 132.3 | 145.1 |

(front-end stages unchanged: parse ≈ 0.01ms, bind ≈ 0.057ms, plan ≈ 0.07ms.)

### 1c. Compaction OFF (9 SSTables), BEFORE → AFTER

K = 10,000: parse 0.009 / bind 0.18 / plan 0.13 / traverse 3.8 /
decode +2.2 / **catalog meta × K 1,007ms (81%)** / raw get × K 108ms
(8.7%) / full SQL **1,238ms**. After fix: `index_lookup_as_of`
142.6ms, full SQL **156.7ms** (7.9×). K = 1,000: 118.8 → 17.2ms (6.9×).

### 1d. What the decomposition proves

1. **Index traversal + entry decode: ~0.6µs per entry** (3.7 + 2.1ms for
   10,000 entries), identical before and after, independent of N.
   → *the secondary index itself is not the bottleneck.*
2. **Catalog metadata resolution inside the per-row fetch dominated:**
   ~40µs/row with compaction on, ~100µs/row at 9 SSTables, ~700µs/row at
   64 SSTables (see §3). It is a `get_table` + `get_columns` (engine
   point read + engine range scan) per matched row.
3. **The raw engine point read is ~10–11µs/row and is now the dominant
   remaining cost** (≈ 75–80% of the post-fix time at K ≥ 1,000).
4. **Serialization** (JSON encode of the result, measured separately in
   the API crate): K=1: 0.003ms, 10: 0.010, 100: 0.106, 1,000: 0.851,
   10,000: 8.47ms (≈0.85µs/row, 2.5MB JSON at 10K rows) — ≈ 5–6% of the
   post-fix cost, never dominant.
5. Projection/materialization into result rows (full SQL − relational
   lookup): 0.4–13ms across K=1..10,000.

→ **Cost is proportional to the number of matched rows (K), not to
total table size**, with the per-row constant coupled to LSM state in
the OLD code only.

## 2. The two mandatory scaling tests

### 2a. Constant K, growing N (table-size effect) — warm p50 ms, indexed equality `g{K} = 'g0'`

**Compaction ON, AFTER (headline):**

| K | N = 10K (MemTable only) | N = 100K (2 SSTables) | N = 1M (4 SSTables) |
|---|---|---|---|
| 10 | 0.054 | 0.227 | 0.291 |
| 100 | 0.388 | 1.709 | — |
| 1,000 | 3.647 | 15.28 | 18.20 |
| 10,000 | 38.57 | 144.6 | — |

Reading it: **from 100K to 1M rows (10× the data) latency moves 0.227 →
0.291ms (K=10) and 15.3 → 18.2ms (K=1,000): table size has a weak
effect once the data is SSTable-resident.** The 10K→100K step (≈ 4×) is
the engine's own MemTable-vs-SSTable point-read cost (raw `get_as_of`
≈ 3.6µs/row from the MemTable vs ≈ 10.8µs from an SSTable); it is the
same step PK-equality shows (0.013ms at 10K → 0.057ms at 100K) and is
not index-specific. 1M with Compaction ON was run for K=10 and K=1,000
only; K=100/10,000 at 1M were not run.

**Compaction OFF, BEFORE vs AFTER (shows how the old code scaled with LSM size):**

| K | N=1K | N=10K | N=100K (9 SST) | N=1M (64 SST) |
|---|---|---|---|---|
| 10 before | 0.138 | 0.136 | 2.407 | 9.042 |
| 10 after | 0.142 | 0.059 | 0.502 → 0.392 | 1.330 |
| 1,000 before | 12.29 | 11.87 | 155.2 | **734.5** |
| 1,000 after | 3.88 | 3.96 | 17.4 | **25.6** |

(After-fix N=100K column: 0.502 first run, 0.392 after the second
metadata de-duplication; K=1,000 17.4 → 18.0 across repeat runs — normal
run-to-run variation ≈ ±5%.) At 1M rows with 64 SSTables, K=1,000 was
**28.7× faster** (734.5 → 25.6ms).

### 2b. Constant N = 100,000, growing K (matched-row effect) — Compaction ON

| K | BEFORE p50 ms | AFTER p50 ms | speedup | per-row after |
|---|---|---|---|---|
| 10 | 0.660 | 0.227 | 2.9× | 23µs (fixed overhead dominates) |
| 100 | 5.912 | 1.709 | 3.5× | 17µs |
| 1,000 | 56.92 | 15.28 | 3.7× | 15.3µs |
| 10,000 | 564.5 | 144.6 | 3.9× | 14.5µs |

Cost grows **linearly in K in both versions** (before: ≈ 56µs/row;
after: ≈ 14.5–15µs/row) — the matched-row effect is real and
irreducible below the engine's per-row point-read cost; the fix removed
the redundant ≈ 40µs/row on top of it. Compaction OFF (9 SSTables) at
the same N: 1,475 → 160.5ms at K=10,000 (9.2×), 155.2 → 17.4ms at
K=1,000 (8.9×), 15.7 → 2.0ms at K=100 (7.7×).

### 2c. Answer: what is index cost proportional to?

- **Number of index entries examined:** traversal+decode ≈ 0.6µs each —
  negligible.
- **Number of rows fetched:** yes — dominant, linear (≈ 14.5µs/row after;
  ≈ 56µs/row before with compaction on).
- **Total table size:** no (after the fix): flat from 100K to 1M with
  compaction on; the only table-size coupling in the OLD code was the
  per-row metadata cost growing with SSTable count.

## 3. Replay of the Blocker 9 `indexed_select` shape

Same schema and predicate (`SELECT * FROM rp WHERE grp = 'g3'`, 10
groups, so matches ≈ N/10), table sizes of the three endurance segments.
Warm p50 ms.

| N (K = N/10) | Compaction ON before | after | Compaction OFF before | after |
|---|---|---|---|---|
| 105,000 (10,500) | 613.1 | **193.9** (3.2×) | 627.6 | 173.1 (3.6×) |
| 155,000 (15,500) | 709.5 | **240.1** (3.0×) | 1,290.4 | 250.8 (5.1×) |
| 206,000 (20,600) | 1,199.1 | **326.7** (3.7×) | 1,936.1 | 333.5 (5.8×) |

The original build reproduces the recorded drift shape (recorded:
277.8 → 783.7 → 1,148.1ms; compaction-ON replay: 613 → 709 → 1,199ms,
2× table → ~2× latency; compaction-OFF: 628 → 1,290 → 1,936ms). Absolute
values differ from the recorded endurance numbers (different machine
load, concurrent writers/compaction in the real run, 6 concurrent
workers) — **the replay reproduces the mechanism and the growth, not the
exact milliseconds.** After the fix the growth is linear in matched rows
(per-row 18.5 → 15.5 → 15.9µs) with no superlinear component.

## 4. Candidate costs (N = 100,000, Compaction OFF; measured, not all implemented)

| Quantity | Measured |
|---|---|
| Table bytes | 8,156,790 B (81.6 B/row) |
| One secondary index | 1,890,000 B (18.9 B/entry = **23%** of table) |
| Covering entry (row stored as value) | ≈ 100 B/entry → index ≈ **123% of table** (5.3× larger), per covering index |
| Write 20K rows, 0 indexes | 273ms (CPU 94ms) |
| Write 20K rows, 2 indexes | 333ms (CPU 172ms) — ≈ +30ms/index |
| Write 20K rows, 6 indexes | 402ms (CPU 234ms) |
| Extra write for 20K covering-simulated entries (81B values) | +232ms **per covering index**, on top of the above |
| 10K point reads: sequential vs parallel prefetch | 101.7ms vs 61.9 / 45.2 / 45.5ms (2 / 4 / 8 threads, spawn included); same total CPU |
| Covering read ceiling (contiguous decode-only scan) | K=100: 0.45ms, K=1,000: 3.4ms, K=10,000: 34.5ms (vs 132–145ms after the fix) |

The chosen fix has **zero** index-size, write, and memory cost
(nothing new is stored or cached). Covering would roughly double the
write cost of every row mutation touching a covered column and 5×
index size for at most a further ≈ 4× read gain on `SELECT *`.
Parallel prefetch has no throughput benefit when the machine is already
CPU-bound (§5). See the architecture doc §4 for the verdicts.

## 5. Concurrency (N = 100,000, Compaction ON), BEFORE → AFTER

Throughput op/s, p99 ms. 32 threads on 4 cores / 8 threads.

| Query | Threads | before op/s | after op/s | before p99 | after p99 |
|---|---|---|---|---|---|
| idx_eq K=100 | 1 | 130.3 | 133.1 | 13.2 | 3.9 |
| | 2 | 225.4 | 289.7 | 11.1 | 4.1 |
| | 4 | 294.3 | 516.8 | 17.5 | 7.2 |
| | 8 | 361.8 | 771.1 | 28.6 | 14.4 |
| | 16 | 370.5 | 901.6 | 59.4 | 23.8 |
| | 32 | 377.2 | 905.3 | 99.3 | 47.0 |
| idx_eq K=1000 | 1 | 16.9 | 61.4 | 69.3 | 22.2 |
| | 2 | 26.8 | 92.0 | 87.9 | 27.7 |
| | 4 | 40.3 | 112.7 | 128.2 | 52.1 |
| | 8 | 39.9 | 106.2 | 209.3 | 81.4 |
| | 16 | 42.8 | 102.2 | 395.4 | 173.6 |
| | 32 | 41.5 | 99.2 | 820.3 | 358.1 |
| **pk_eq (control)** | 1 | 1,957 | 2,003 | 0.137 | 0.140 |
| | 8 | 14,202 | 14,241 | 0.518 | 0.494 |
| | 32 | 45,637 | 44,915 | 2.818 | 2.965 |

- CPU per run (K=1,000, 32 threads): 122.5s before vs 29.2s after for
  the same number of operations; RSS 12MB steady (peak 31MB / 28MB);
  handles 81–84 constant; threads = harness threads (≤ 40).
- Throughput saturates at ≈ 4–8 threads (CPU-bound) in both versions,
  at a ≈ 2.4× higher ceiling after. This is also why parallel prefetch
  was rejected: there is no idle CPU to exploit under concurrent load.
- **PK equality did not regress** (differences ≤ 2% at 32 threads, within
  run-to-run noise; PK code paths were not changed).
- Compaction OFF concurrency (first campaign) showed the same shape:
  K=1,000 at 32 threads 42.4 → 202.4 op/s, p99 986 → 288ms.

## 6. Regression matrix on shared infrastructure (N = 100,000, Compaction OFF)

Same build before/after; warm p50 ms unless noted.

| Operation | Before | After | Note |
|---|---|---|---|
| PK equality | 0.107 | 0.107 | no change |
| PK range, K=100 | 0.501 | 0.504 | no change |
| Full SeqScan + filter (COUNT) | 339.6 | 337.1 | no change |
| Secondary-index range, K=100 | 10.91 | 1.554 | 7.0× |
| JOIN with index predicate (K=100 outer) | 22.54 | 13.02 | 1.7× (inner PK lookups unchanged) |
| Aggregate COUNT/SUM, index K=1,000 | 107.8 | 15.36 | 7.0× |
| GROUP BY / HAVING, index K=1,000 | 108.0 | 15.58 | 6.9× |
| `SELECT *`, index K=1,000 | 110.3 | 15.40 | 7.2× |
| `SELECT id` (narrow), index K=1,000 | 108.1 | 15.36 | 7.0× — same cost: the fetch, not the projection, dominated |
| UPDATE via index (K=100, non-indexed col) | 34.97 | 26.22 | 1.3× |
| UPDATE indexed column via index (K=10) | 9.30 | 7.75 | 1.2× |
| DELETE via index (K=10) | 7.57 | 6.40 | 1.2× |
| DELETE via index (K=1,000) | 271.9 | 167.0 | 1.6× |

No regressed operation. (UPDATE/DELETE gains are smaller because their
cost is dominated by write-set construction and index maintenance, which
this change did not touch.) A first attempt of this matrix hit a
10,000-op engine write-batch limit on an accidental 10,000-row indexed
UPDATE; the engine failed closed (`capacity exceeded: requested 50000,
max 10000`) — a benchmark-script bug (wrong column cardinality), fixed
and re-run; recorded here because it is also a free confirmation that the
write path's bound works.

## 7. Memory, disk, write impact (MEMORY / DISK / WRITE IMPACT)

- **Memory:** nothing cached or prefetched. The eager result `Vec` is
  unchanged in size; RSS in the replay: 14–27MB at K = 10,500–20,600
  rows, same before and after. The materialization bound is now enforced
  while collecting (peak memory of a runaway scan is capped at
  `max_index_scan_rows` rows instead of the whole match set).
- **Disk:** identical — no on-disk format, index, or entry changed.
- **Writes:** identical write path (`put_row`/`put_rows`/`delete_row`
  untouched); write cost per index measured in §4 for reference.
  UPDATE/DELETE timings in §6 did not regress.

## 8. Cold vs warm

"Cold" (first execution after setup) is reported in every raw line and
tracks the warm p50 closely (e.g. N=100K, K=1,000, compaction ON after:
cold 15.46ms vs warm p50 15.28ms) because the index path does no
persistent-cache priming of its own. **Process-cold / OS-cache-dropped
measurements were not performed** (not practical in-process); recovery
semantics are unchanged (read-only change).
