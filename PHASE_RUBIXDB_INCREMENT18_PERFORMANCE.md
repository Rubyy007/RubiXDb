# Increment 18: Performance Evidence

Companion to the three Increment 18 architecture documents and
`PHASE_RUBIXDB_INCREMENT18_RESULTS.md`. Every number is from an in-process,
release-build test (`sql/src/index_read_benchmark.rs`,
`sql/src/inc17_overhead_bench.rs`, `sql/src/inc18_compare_bench.rs`; real
`LsmEngine`, catalog, and SQL parse/bind/plan/execute). Raw outputs are under
`temp/inc16/out7..out10` (untracked scratch). Unfavourable results are kept.

## 0. Method, environment, limits

- Intel i7-7700 (4 cores / 8 threads), 16GB, SSD, Windows 10; TEMP on `E:`.
- **"Before" = the Increment 17 tree (`bc80c79`)**: for the access-path and
  materialization baselines it is the same source measured before the change;
  for the overhead comparisons it is a clean `git worktree` of `bc80c79`
  running the same benchmark file as the new tree.
- Production-representative regime: automatic Compaction on
  (`INC16_COMPACT=1`).
- p99 ~= max under ~100 samples. Process-cold / cache-dropped runs were not
  done. Machine-state noise of ~5-7% between batches exists on this machine
  (documented in Increment 17); comparisons that matter are same-batch or
  interleaved, and one apparent anomaly is reported in section 3.

## 1. PK range vs secondary index vs table scan

COUNT(*) projection, p50 ms. R = rows in the PK range, K = rows matching the
index predicate, N = 100,000. "auto" is the planner+executor's choice; "best"
is the fastest of the three independently measured paths; regret = auto / best.

| R | K | **before** auto | best | regret | **after** auto | regret |
|---|---|---|---|---|---|---|
| 10 | 10 | 0.096 | 0.104 pk | 0.93x | 0.125 | 1.18x |
| 10 | 100 | 0.095 | 0.098 pk | 0.98x | 0.130 | 1.23x |
| 10 | 10,000 | 0.095 | 0.097 pk | 0.98x | 0.129 | 1.21x |
| 10 | 50,000 | 0.097 | 0.099 pk | 0.98x | 0.130 | 1.21x |
| 1,000 | 10 | 3.139 | 0.230 index | **13.6x** | 0.339 | 1.44x |
| 1,000 | 100 | 3.814 | 1.488 index | 2.6x | 2.134 | 1.44x |
| 1,000 | 10,000 | 3.830 | 3.978 pk | 0.96x | 4.810 | 1.20x |
| 1,000 | 50,000 | 3.913 | 4.050 pk | 0.97x | 5.801 | 1.42x |
| 10,000 | 10 | 35.03 | 0.230 index | **152x** | 0.334 | 1.42x |
| 10,000 | 100 | 38.00 | 1.574 index | 24x | 1.933 | 1.30x |
| 10,000 | 10,000 | 40.38 | 40.71 pk | 0.99x | 45.93 | 1.16x |
| 10,000 | 50,000 | 39.67 | 41.23 pk | 0.96x | 48.47 | 1.20x |
| 50,000 | 10 | 193.3 | 0.231 index | **838x** | 0.337 | 1.43x |
| 50,000 | 100 | 194.0 | 1.554 index | 125x | 1.936 | 1.27x |
| 50,000 | 10,000 | 196.7 | 131.0 index | 1.50x | 154.9 | 1.21x |
| 50,000 | 50,000 | 195.2 | 206.5 pk | 0.95x | 235.3 | 1.16x |

The decision is now correct at all 16 points (the faster path was always
chosen). The price is a probing overhead that is visible as a regret of
1.16-1.44x: ~0.1ms of fixed cost (opening a second cursor) on the sub-
millisecond cells, and about 16-20% (the cost of exactly counting the winning
PK range at ~0.4us per row against ~3.4us per row to scan it, plus the
losers' proportional probing) on the large ones. Where the planner was
previously right it is now 15-25% slower; where it was wrong it is up to 838x
faster. The bound is the point: the worst regret went from **838x to 1.44x**.

Other table sizes (after only; the before-grid was measured at 100K): N = 1M
(R = 1,000/100,000, K = 1,000..500,000): regret 1.16-1.68x (e.g. R=100,000,
K=1,000: 19.2ms vs 14.7ms best; the Increment 17 behaviour would walk the
100,000-row PK range, 385ms); N = 10K: 1.12-2.18x with the two worst cells
being 0.166 and 0.165ms absolute; N = 1K: 1.09-7.62x where the worst cell is
0.147ms vs 0.019ms -- at 1,000 rows every path is sub-millisecond and the
~0.13ms fixed cost of a two-candidate decision dominates. That fixed cost is a
known property of the design (section 8), not a bug.

Evolution of the selection algorithm, kept because the intermediate results
are the reason for the shipped design (R=10, K=10,000: 0.095ms before):

| selection algorithm | worst regret (N=100K grid) | R=10, K=10,000 |
|---|---|---|
| Increment 17 (structural: PK range) | 838x | 0.095ms |
| index-first with a table-scan budget, then count the PK range | **65x** | 7.27ms |
| budget widening x4, re-probing each round | 1.75x | 0.128ms |
| **lockstep race over resumable cursors (shipped)** | **1.44x** | 0.129ms |

Selective-index-versus-scan crossover (Increment 17's sweep), **re-run on this
tree** (N = 100,000, compaction on, same dataset and method): the model
predicted the faster path at 16 of 16 points (range and equality, 0.01%-90%),
`Auto` matched the best path at every point, and the crossover is unchanged
(25%: index 424ms vs scan 279ms; 10%: 169ms vs 258ms). Absolute table-scan
times are ~27% lower than in Increment 17 (e.g. 10% selectivity: 258ms vs
318ms) because of the single-clone row context (section 5); index times are
unchanged.

## 2. Index result materialization (forced index, N = 100,000)

| K | full p50 ms | RSS MB | `LIMIT 10` before | `LIMIT 10` after | speedup |
|---|---|---|---|---|---|
| 10 | 0.252 | 0.0 | 0.242 | 0.249 | 1.0x |
| 100 | 1.662 | 0.0 | 1.502 | 0.269 | 5.6x |
| 1,000 | 16.53 | 0.1 | 14.70 | 0.794 | 18.5x |
| 10,000 | 169.6 | 6.4 | 149.6 | 6.595 | 22.7x |
| 25,000 | 421.4 | 15.7 | 373.2 | 16.22 | 23.0x |

Before: full 0.243 / 1.656 / 16.54 / 168.96 / 414.28ms, RSS 0 / 0 / 0.3 / 7.0 /
17.0MB. Full-result latency unchanged; resident growth slightly lower. The
remaining `LIMIT` cost is the eager key-only enumeration (~0.65us/entry).
Fetch counts (exact, from the relational counter): `LIMIT 5` over 1,000
matches fetched 5 rows; a consumer pulling 40 rows one at a time caused
exactly 40 fetches; after a cancel at 10 rows, 0 further fetches.

## 3. Transaction overlay overhead (N = 100,000, p50 ms)

`autocommit` = no transaction; w = rows the transaction has written to the
table (inserted with fresh keys). Before the change a transaction scan did not
include its own writes, so there is no like-for-like "before" for w > 0; the
w = 0 column is the like-for-like comparison with the old behaviour.

| query | autocommit | w=0 | w=1 | w=10 | w=100 | w=1,000 |
|---|---|---|---|---|---|---|
| seq scan COUNT | 311.0* | 255.2 | 258.4 | 323.8** | 258.5 | 258.7 |
| PK range K=1,000 | 3.294 | 3.268 | 3.269 | 3.288 | 3.394 | 4.001 |
| index eq K=100 | 1.490 | 1.477 | 1.474 | 1.477 | 1.538 | 2.264 |
| index range K=100 | 1.582 | 1.574 | 1.575 | 1.579 | 1.637 | 2.299 |

*the first measurement of a run is a warm-up outlier (the autocommit column
for the seq scan). **w=10 on the seq scan looked 25% slower in three runs. A
reversed-order pass reproduced it; a probe at w = 0,7,8,9,10,10,10,11,12
showed **no anomaly** (257-268ms everywhere, w=10 = 260-267ms), so it was
measurement noise tied to position in the run, not an overlay cost; the merge
loop does identical work at every w.

- w = 0 equals autocommit (the common case costs one hash probe).
- Cost for w > 0 is O(w) per scan (one clone and one predicate evaluation per
  overlay row): <= 4% up to w = 100; at w = 1,000 +0.7ms on a 1.5-3.3ms query
  (index 1.5x, PK range 1.2x), 0% on the full scan whose cost dwarfs it.
- Allocation fix found by measurement: the first implementation cloned every
  overlay row up front and cloned each fetched row twice into its context;
  index lookup at w = 1,000 was 3.03ms and PK range 5.30ms. Lazy visiting of
  the overlay and a single-clone row context (`RowContext::with_row`) brought
  them to 2.26 / 4.00ms **and** made every scan faster (below).

## 4. Runtime statistics across a restart (identical database; real engine
restart, WAL replay + SSTable recovery; COUNT(*) queries)

| N | query | plan us before / after | first exec ms before / after | warm p50 before / after | decision before / after |
|---|---|---|---|---|---|
| 100K | selective K=10 | 120.3 / 195.8 | 0.31 / 0.39 | 0.206 / 0.219 | index / index |
| 100K | 10% | 119.4 / 131.9 | 118.5 / **186.0** | 116.5 / 129.5 | index / index |
| 100K | 50% | 109.0 / 119.6 | 292.0 / 291.7 | 276.7 / 276.4 | scan / scan |
| 100K | PK range + index | 126.5 / 133.5 | 151.0 / 166.2 | 150.7 / 163.6 | **switch to index / stays on PK range** |
| 1M | selective K=10 | 250.2 / 126.7 | 1.99 / 1.75 | 1.78 / 1.50 | index / index |
| 1M | 10% | 220.5 / 126.8 | 1,383 / **1,822** | 1,180 / 1,303 | index / index |
| 1M | 50% | 112.4 / 112.7 | 3,458 / 2,872 | 2,683 / 2,707 | scan / scan |
| 1M | PK range + index | 128.0 / 127.7 | 1,518 / 1,641 | 1,512 / 1,640 | switch / PK range |

What restarting costs: **(a)** the first query that needs a table size pays
one exact count -- +68ms at 100K rows, +440ms at 1M (0.6us/row), once per
table, and only when the match count exceeds 128 (the selective query pays
nothing); **(b)** the cost parameters revert to their measured defaults, which
flips one borderline decision (PK range + index) to the other, ~8% slower
path until the parameters are re-learned; **(c)** results are identical and no
decision that mattered changed (the 10% and 50% queries chose the same paths
before and after). Plan time is statistics-independent (the 120 -> 196us at
100K is first-run catalog warm-up). 100K: before-restart estimate present
(`Some(100000)`), after restart `None`, populated by the first needing query.

## 5. Overheads of the new machinery (Increment 17 tree vs Increment 18 tree,
same benchmark file, same machine, compaction on)

Planner (us, median of 2,000): PK lookup 7.7 -> 7.9; index equality 17.8 ->
17.5; index range 19.2 -> 18.8; seq scan 16.7 -> 15.9; **complex predicate
(PK range + two index candidates) 21.0 -> 27.7 (p99 34.5 -> 52.4)**; join
19.4 -> 21.3. The one real increase is the bounded cost of cloning the
predicate into each carried alternative.

Execution p50 (ms): PK lookup 0.048 -> 0.046; index eq K=10 0.195 -> 0.192;
K=100 1.581 -> 1.548; K=1,000 14.38 -> 14.15; **PK range K=100 0.376 -> 0.322
(-14%)**; **seq scan COUNT 342.6 -> 229.7 (-33%)**. The last two are the
single-clone row context (`with_row`): every fetched row of every access used
to be cloned twice; an unplanned, measured, general speedup.

Regression matrix p50 (ms), same session: PK equality 0.046 -> 0.048; PK
range 0.377 -> 0.322; index range 1.420 -> 1.392; seq scan 342.5 -> 278.1; JOIN
6.485 -> 6.288; COUNT/SUM over index 13.96 -> 14.08; GROUP BY/HAVING 14.74 ->
14.37; `SELECT *` K=1,000 14.69 -> 15.08; narrow 13.98 -> 13.56; UPDATE via
index (100 rows) 12.56 -> 13.07; UPDATE indexed column (10 rows) 6.05 ->
7.00 (one 186ms outlier in 30 samples; the same measurement ranged 6.0-9.3ms
across Increment 17's runs); DELETE (10 rows) 4.75 -> 4.79; DELETE (1,000
rows) 107.3 -> 103.6. No regression outside noise.

Concurrency, existing matrix (op/s at 1/2/4/8/16/32 threads; before vs
after): idx_eq K=100 -- 60.5* / 278.3 / 505.9 / 835.5 / 891.1 / 889.1 vs
133.7* / 286.2 / 499.1 / 753.7 / 872.8 / 905.4 (*the first cell of each run
is a warm-up artifact in both trees); idx_eq K=1,000 -- 60.3 / 96.4 / 113.2 /
105.2 / 100.3 / 98.1 vs 60.8 / 93.9 / 111.4 / 104.8 / 101.7 / 99.2; PK
equality -- 1,987 / 3,949 / 7,523 / 14,219 / 26,433 / 45,331 vs 1,980 / 3,968
/ 7,683 / 13,808 / 26,278 / 45,036. Identical within noise.

**Concurrency, mixed predicate** (a 20,000-row PK range + a 1,000-row index
predicate; the shape the old planner executed as a full PK-range walk):

| threads | before op/s | after op/s | before p50 | after p50 | before CPU | after CPU |
|---|---|---|---|---|---|---|
| 1 | 17.4 | **52.1** | 57.5 | **19.1** | 688ms | 219ms |
| 2 | 32.3 | **76.0** | 60.7 | 26.4 | 1,469 | 641 |
| 4 | 60.8 | **85.2** | 64.8 | 47.4 | 3,109 | 1,797 |
| 8 | 60.6 | 76.3 | 120.5 | 104.6 | 11,734 | 5,594 |
| 16 | 69.9 | 72.6 | 227.5 | 225.1 | 21,453 | 10,672 |
| 32 | 70.6 | 71.5 | 439.2 | 451.5 | 42,328 | 19,109 |

Single-thread the new path is 3x faster and uses a third of the CPU. **At 8+
threads throughput converges to ~72 op/s in both trees even though the new
path burns less than half the CPU: the ceiling is not CPU.** The same plateau
(~100 op/s at 4+ threads) is visible for plain `K = 1,000` index fetches since
Increment 16. It is a property of the fetch-heavy read path under concurrency
(point reads through the certified engine), not of the access-path logic, and
it was **not investigated here** -- it is outside the four open items and
would be an engine-profiling question; recorded as an observation, with the
data, in the results document. (`idx eq K=100` and PK equality, which are not
fetch-bound at that scale, are unchanged: 540-936 and 14,079-51,471 op/s.)

## 6. Statistics write overhead

The Increment 17 write hook (21-26ns uncontended, ~850ns with 8 contending
threads, ~0.02% of a durable write) is unchanged. Increment 18 adds one
integer increment per buffered transaction write (the overlay version).
End-to-end writes, 3 reps, before -> after means: `put_row` x5,000 4,184 ->
4,297us/row (+2.7%; before's own reps span 4,160-4,232us); `put_rows` 20,000:
409 -> 477ms (one 662ms outlier; 393 / 377 otherwise); transactional
autocommit x3,000 4,303 -> 4,369us/row (+1.5%); 8 writers x1,500 13,276 ->
13,265ms. **CREATE INDEX** (100K rows): 2,286 / 2,346 / 2,695 -> 2,347 / 2,358
/ 2,794ms; **DROP INDEX**: 820 / 872 / 899 -> 847 / 811 / 935ms. All within
the spread between repetitions of the same tree. `INSERT`/`UPDATE`/`DELETE`
statements pay the same hook; CREATE INDEX records the table size from the
rows it already enumerates at no extra cost; DROP INDEX does nothing.

## 7. Memory

- Index results: lazy fetch holds only the entry list (a few tens of bytes per
  match); resident growth with the result alive 15.7MB (was 17.0) at K=25,000;
  `LIMIT 10` holds ten rows.
- Transaction overlay: one cloned `Row` per distinct key written (bounded by
  `max_write_set_ops`, default 10,000), shared by `Arc` across all accesses of
  the same transaction version; zero for a table the transaction has not
  written.
- Planner: up to 3 extra candidate accesses per `IndexScan`/`PkRangeScan`.
- Statistics: unchanged (<= 4,096 tracked tables, ~400KB at the cap).

## 8. Honest limits

- A two-candidate decision costs ~0.1ms fixed (a second cursor and, for the
  index, the open cost); regret exceeds 1.5x only on queries under ~0.3ms.
- Overlay cost is O(w) per scan for a transaction with w writes to the table
  (the index/PK-range merge could be narrowed by key range; not done).
- Index enumeration is still eager (section 2), so `LIMIT` is O(K) in key
  work.
- The read-path concurrency plateau (section 5) is unexplained.
- Not measured: more than 1M rows, process-cold runs, mixed read+write load,
  multi-index merges.
