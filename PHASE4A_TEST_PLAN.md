# RubiXDB Phase 4A — Test Plan

## 1. What is tested this phase

1. **MemTable core** (`src/memtable/mod.rs`, operating brief §12-§18):
   every item in `RubixDB-LSM-Engine-Specification-v1.0.md` §1.6's own
   test checklist, plus Phase 4A's two added accessors
   (`remaining_capacity`/`entry_count`) — 13 unit tests.
2. **MemTable property tests** (`src/memtable/property_tests.rs`,
   operating brief §25/§30): random operation sequences compared
   against a deliberately naive reference model (linear scan, not the
   memtable's own range-query trick) — 1,000 cases × 2 properties
   (query equivalence + ordering; freeze preservation).
3. **Bounded-memory WAL replay** (`src/wal/mod.rs`'s new `replay_
   streaming`, operating brief §24/§27): 6 dedicated tests — in-order
   delivery, a direct structural proof of the per-segment-bounded
   mechanism (not just an RSS measurement), identical corruption
   classification to `open_for_recovery` on the same fixture, empty-WAL
   and never-created-directory edge cases, callback-error propagation.
4. **WAL/MemTable integration** (`src/lsm/mod.rs`, operating brief §22-
   §24): 19 unit tests — Put/Get round trip, Delete/Get, Put/Delete/Put,
   snapshot stability across later writes, freeze-at-threshold, immutable
   backpressure, memory accounting after freeze, WAL-durability-ordering
   (a real injected fsync failure confirming MemTable is never touched
   before durability), recovery after restart (basic, multi-version,
   multi-segment/rotation), concurrency at 1/10/100/1,000 logical
   writers through the real `BatchCoordinatorPool`, large key/value
   handling, an oversized-single-entry edge case, size-accounting
   monotonic growth, and a recovery-equivalence test against an
   independent reference model.
5. **Crash tests at the WAL/MemTable boundary** (`examples/lsm_crash_
   cycle_test.rs`, operating brief §26): real external-process kills
   (`Child::kill()`) at randomized, seeded, reproducible delays against
   a real child process running `LsmEngine::put` continuously — 25
   cycles, verifying `durable_through`/`highest_sequence` never regress
   and (per-cycle) `active_entries == highest_sequence ==
   durable_through` exactly.
6. **Performance** (`examples/memtable_bench.rs`, `examples/lsm_load_
   test.rs`, operating brief §32/§36): MemTable-only Put/Get/iteration/
   mixed measured cleanly (single-threaded, CPU-bound); the full
   multi-threaded WAL-vs-WAL+MemTable comparison deferred to an idle-
   machine follow-up (`PHASE4A_ADR.md` ADR-P4A-6).
7. **Security/resource-bound edge cases** (operating brief §34): large
   key (64 KiB) / large value (1 MiB), an entry larger than the
   configured memtable limit, size-accounting overflow resistance
   (monotonic-growth check across many large entries, not an exact-value
   assertion that could mask a wraparound).
8. **Regression gate** (operating brief §35): `cargo test --lib`,
   `--features test-util`, `cargo clippy --all-targets --all-features
   -- -D warnings`, `cargo fmt --check` — all green throughout, re-run
   after every commit this phase.

## 2. What is explicitly NOT run/built this phase, or is a known gap

- **The full multi-threaded WAL-only vs. WAL+MemTable comparison at
  100/1,000 writers** (operating brief §32/§36) — deferred to an idle-
  machine follow-up; the background Phase 3C long soak was still
  running throughout this phase's work, and a smoke-scale attempt
  showed clear contamination (`PHASE4A_ADR.md` ADR-P4A-6).
- **`cargo test --release`** for the new MemTable/LSM code specifically
  — the existing full-suite `cargo test --release --lib` command was
  not re-run at the very end of this phase in isolation from the
  background soak (release-profile binaries would conflict with the
  soak's own running release binary); debug-profile `cargo test --lib`
  was run repeatedly and is green throughout. This is recorded as an
  open item for the final regression gate once the soak completes.
- **RUBIC SSTable, Manifest, Compaction, Bloom filter, block index,
  SSTable footer** — none implemented, per operating brief §5/§37's
  explicit exclusion.
- **A `SkipList` benchmark comparison** — not performed; the existing,
  final LSM Engine Spec already prescribes `BTreeMap` (`PHASE4A_ADR.md`
  ADR-P4A-1).
- **Phase 3C's own final certification** — the long soak (started
  before this phase began) had not completed by the time Phase 4A's own
  work concluded; `PHASE4A_TEST_RESULTS.md` §12's certification
  decision is explicitly conditioned on it.

## 3. Reproduction

```
cargo test --lib memtable::
cargo test --lib wal::tests::replay_streaming
cargo test --lib lsm::
cargo test --lib
cargo test --lib --features test-util
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo run --release --example memtable_bench -- 100000
cargo build --release --example lsm_crash_cycle_child --example lsm_crash_cycle_test
cargo run --release --example lsm_crash_cycle_test -- 25 4 2024 100 1200
cargo run --release --example lsm_load_test -- 100 1000   # once the machine is idle
```
