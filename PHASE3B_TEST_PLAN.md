# RubiXDB Phase 3B — Test Plan

## 1. Regression-bound rule (operating brief §25 — established BEFORE the final acceptance benchmark run, not after seeing its numbers)

This project's own historical benchmark data, gathered across every
prior phase's own documented runs of the winning Dedicated Batch
Coordinator architecture:

| Source | 100w range | 100w spread | 1000w range | 1000w spread |
|---|---|---|---|---|
| `PHASE2B_FINAL_TEST_RESULTS.md` §21 (5 reps) | 15,777–17,940 | ~14% | 90,999–95,769 | ~5% |
| `PHASE3_PERFORMANCE.md` (pre/post-3A, 10 reps total) | 13,736–18,487 | ~35% | 90,728–97,555 | ~7% |
| `PHASE3B_TEST_RESULTS.md` §2 baseline (3 reps) | 17,229–18,151 | ~5% | 92,183–92,987 | ~1% |

**Rule, fixed before this session's final acceptance run:**

1. A newly-measured median is treated as **normal machine noise, not a
   regression**, if it falls within the widest historically-observed
   band above (100w: 13,700–18,500; 1000w: 90,700–97,600) **and** is not
   consistently reproduced as low across ≥3 independent repetitions.
2. A newly-measured median is treated as a **real regression** only if
   it falls *below* that historical band **and** is reproducible
   (consistently low, not a one-off) across ≥3 independent repetitions —
   at which point the cause is investigated and reported, not dismissed.
3. **Absolute floor, independent of the above**: the original Phase 2B
   targets (100w ≥15,000 ops/sec, 1000w ≥80,000 ops/sec) remain the hard
   acceptance criterion regardless of how a number compares to history.
   Falling below either, even if explainable as noise, blocks
   acceptance until investigated.
4. Every comparison uses the same machine, storage, power mode, and
   release-profile binary as every prior phase's own numbers (unchanged
   throughout this project's history) — see `PHASE3B_PERFORMANCE.md` §1.

## 2. What is tested this increment

1. **Baseline freeze** (operating brief §1): `git status`/`rev-parse`/
   `branch`/`log`, re-run of the winning architecture at 100 and 1,000
   writers with full stats (throughput, p50/p95/p99/max, records/sync,
   queue depth, failures, timeouts).
2. **Regression gate** (§2): `cargo test`/`--release`/`--features
   test-util`/clippy/fmt, plus `tests/group_commit`/`tests/crash_
   consistency`, both before and after every code change.
3. **Coordinator fault matrix** (§3/§6): `CoordinatorFaultPoint`'s 7
   points, each with a dedicated deterministic test — see `PHASE3B_
   FAILURE_MODEL.md` §5 for the per-point crash-semantics checklist.
4. **Overflow/large-payload safety** (§9): `queued_bytes` arithmetic
   hardened across all four `execution::*` modules; a dedicated
   large-payload accounting test with a deterministic barrier.
5. **Resource exhaustion / shutdown** (§7/§8): rapid submit/shutdown
   cycling, shutdown while the queue is actively populated, coordinator-
   panic-during-shutdown (bounded).
6. **Rotation stress** (§14): frequent automatic rotation under
   sustained concurrent load through the full production path.
7. **Observability audit** (§15–§18): existing metrics checked against
   the brief's full list; safe, zero-contention gaps closed
   (`queue_capacity`, `queued_bytes_capacity`, `highest_sequence`,
   `segment_rotations`); the rest explicitly recorded as NOT built this
   increment, not silently marked done.
8. **Soak test** (§10–§11): `examples/soak_test.rs` against the
   production `BatchCoordinatorPool`, bounded duration (not multi-hour —
   flagged), periodic sampling, start/mid/end drift analysis.
9. **Security review** (§21): manual audit — `unsafe` usage (none found
   in production code), payload logging (none found — the one
   `eprintln!` path is an aggregate-only, `test-util`-gated diagnostic),
   dependency review (`Cargo.lock` — no new production dependencies this
   phase; `crc32c` remains the only one, pinned exact), overflow
   arithmetic (see item 4).
10. **Final performance re-verification** (§19–§20): re-run the frozen
    baseline benchmark after all hardening changes, compare per the
    rule in §1 above.

## 3. What is explicitly NOT run/built this increment (§28: record as NOT RUN, not PASS)

- **True multi-hour soak** (§10) — NOT RUN. A bounded-duration run was
  performed instead (see `PHASE3B_TEST_RESULTS.md`).
- **Periodic forced-crash soak** (§12: killing the process at intervals
  *during* a live soak and verifying recovery repeatedly) — NOT RUN this
  increment. Related but distinct coverage exists (`tests/crash_
  consistency.rs`'s abort-point matrix, run separately from any soak;
  Phase 3A's leader-panic-then-reopen test) — not a substitute for the
  literal periodic-crash-during-soak exercise §12 describes.
- **Recovery stress with deliberately constructed pathological WALs**
  (§13: many segments/batches, partial final frames, mixed corrupted +
  valid segments in one directory, etc.) — NOT RUN as a dedicated new
  exercise this increment. Overlapping coverage already exists from
  Phase 0/1's own hardening pass (`tests/wal_tests.rs`, `wal::fuzz_
  tests`, `tests/crash_consistency.rs`) but was not re-run or extended
  specifically for Phase 3B.
- **Full production metrics layer** (§15, most of the named counters) —
  NOT BUILT. See `PHASE3B_ADR.md` ADR-P3B-3.
- **Metrics on/off performance comparison** (§16) — NOT RUN (no new
  hot-path metrics were added to compare).
- **Dedicated production logging audit with new log statements** (§17) —
  audited (see §21 above), not extended with new logging this increment.
- **`cargo-audit`/`cargo-deny`** (§22) — neither tool is installed on
  this machine; not installed this session per the brief's own "do not
  install a new security tool without documenting the decision" rule —
  a manual `Cargo.lock` review was performed instead (§21 above).
- **Full combinatorial fault matrix at the `GroupCommitter` level**
  beyond the leader-panic point Phase 3A already covers (before/after
  snapshot as points distinct from the existing `AbortPoint` set,
  before/during waiter notification as points distinct from `AfterWatermarkBeforeWake`) —
  the existing `AbortPoint` enum (11 variants) already covers most of
  this at the WAL/`GroupCommitter` layer; no new variants were added
  this increment.

## 4. Reproduction

```
cargo test --lib
cargo test --release --lib
cargo test --lib --features test-util
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --release --test group_commit --features test-util
cargo test --release --test crash_consistency --features test-util
cargo run --release --example batch_coordinator_load_test -- 100 1000
cargo run --release --example batch_coordinator_load_test -- 1000 1000
cargo run --release --example soak_test -- 100 900 60
cargo run --release --example soak_test -- 1000 900 60
```
