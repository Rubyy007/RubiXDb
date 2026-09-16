# RubiXDB Phase 4A — Performance

Companion to `PHASE4A_TEST_RESULTS.md` (the authoritative pass/fail
source) — this file collects Phase 4A's benchmark numbers and the
regression comparison against the certified WAL foundation.

## 1. Environment

Unchanged from every prior phase this session: Intel Core i7-7700 (4
physical / 8 logical cores), 16 GiB RAM, SATA SSD (`E:`), Windows 10
Home 10.0.19045, `rustc`/`cargo` 1.98.1.

## 2. MemTable-only (`examples/memtable_bench.rs`, n=100,000)

Single-threaded, CPU-bound — not meaningfully affected by the
concurrently-running background WAL soak (unlike a multi-threaded
benchmark, which competes for OS scheduling with the soak's own many
writer threads).

| Metric | Result |
|---|---|
| Put throughput | 1,451,495 ops/sec |
| `size_bytes` after 100,000 puts | 7,400,000 (74 bytes/entry: ~15-byte key + ~29-byte value + 32-byte `ENTRY_OVERHEAD`) |
| Get p50 latency | 400 ns |
| Get p99 latency | 1,200 ns |
| Get max latency | 62,200 ns (one outlier — plausible page-fault/allocator/scheduler jitter at this scale, not investigated further since it is three orders of magnitude below any WAL-`fsync`-bound latency this system cares about) |
| Ordered iteration | 74,085,050 entries/sec |
| Mixed 50/50 read/write | 1,504,223 ops/sec |

**Interpretation**: MemTable's own per-operation cost (hundreds of
nanoseconds) is negligible relative to the WAL's own dominant cost
(`fsync` latency, several milliseconds on this machine's SATA SSD, per
every prior phase's own measurements) — roughly four orders of
magnitude apart. This is the expected shape for an in-memory `BTreeMap`
operation and is consistent with `ENTRY_OVERHEAD`'s own documented
conservative-estimate status (`PHASE4A_MEMTABLE_ARCHITECTURE.md` §5) —
no correctness claim rests on the exact nanosecond figures, only the
order-of-magnitude relationship to WAL latency.

## 3. WAL + MemTable (`examples/lsm_load_test.rs`) — deferred

**NOT reported as clean evidence.** A 20-writer, 500-per-thread smoke
run returned 1,479 ops/sec while the Phase 3C long soak was actively
running in the background (100+ concurrent writer threads of its own,
competing for this machine's 8 logical cores) — an order of magnitude
below what a clean run would show, and explicitly excluded from this
document's evidence base for that reason (`PHASE4A_ADR.md` ADR-P4A-6).
The write path itself was confirmed correct (all 10,000 submitted
entries landed with the expected values) — only the throughput number
is untrustworthy.

**What remains to measure, once the machine is idle** (operating brief
§32/§36):

```
cargo run --release --example batch_coordinator_load_test -- 100 1000   # WAL-only baseline
cargo run --release --example lsm_load_test -- 100 1000                 # WAL + MemTable
cargo run --release --example batch_coordinator_load_test -- 1000 1000  # WAL-only baseline
cargo run --release --example lsm_load_test -- 1000 1000                # WAL + MemTable
```

Report format, once run: throughput, p50/p95/p99/max latency, absolute
and percentage difference from the WAL-only baseline, CPU, RSS — per
operating brief §36's own exact list. This section will be updated in
place (not left as a placeholder) once that data exists.

## 4. Regression comparison against the certified WAL foundation

**The certified WAL-only baseline** (most recent clean measurement,
`PHASE3C_TEST_RESULTS.md` §2, commit `4221e2f`): 100 writers 17,872
ops/sec, 1,000 writers 91,517 ops/sec — both comfortably above the
original Phase 2B targets (≥15,000 / ≥80,000) and within this project's
own established historical noise band (`PHASE3C_TEST_PLAN.md` §1:
13,700–18,700 / 89,000–97,600).

**Phase 4A's own change to the WAL write path itself: none.** Every
`LsmEngine::put`/`delete` call submits through the exact same,
unmodified `BatchCoordinatorPool::submit` → `Completion::wait()` path
every existing WAL-only benchmark already exercises — the *only* added
cost is the `MemTable::insert` call after durability is confirmed
(§2 above: hundreds of nanoseconds, against a multi-millisecond
`fsync`-dominated batch). **No regression in the WAL-only path itself is
expected or possible from this phase's own changes** — nothing in
`src/wal/`, `src/execution/`, or `src/wal/group_commit.rs` was modified
in a way that touches the write path's own hot-path behavior (the one
WAL-layer addition, `replay_streaming`, is a read-only recovery
function never invoked during normal write-path operation).

**This expectation is not yet independently re-confirmed with a fresh
WAL-only benchmark run under Phase 4A's final code state** — deferred
alongside §3 to the same idle-machine follow-up, for the same
contamination-avoidance reason. Until that run exists, this section's
"no regression expected" claim is an architectural argument, not yet a
re-measured fact — stated as such, not overclaimed.
