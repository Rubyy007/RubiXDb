# RubiXDB Phase 4A — Test Results

Single source of truth for Phase 4A's pass/fail data and benchmark
numbers, per this project's own standing rule. Does not overwrite or
replace `PHASE1_TEST_RESULTS.md` through `PHASE3C_TEST_RESULTS.md` —
all stand as the historical record of their own phase.

**Document status**: Phase 4A's own implementation/testing is complete
per the scope in `PHASE4A_TEST_PLAN.md` §1. **Two items remain open**,
both explicitly named, not hidden: the full multi-threaded WAL-vs-
WAL+MemTable performance comparison (§9/§11), and Phase 3C's own final
WAL certification (§0), which had not completed when this phase's work
concluded.

## 0. Precondition: Phase 3C certification status

**Not complete.** Per `PHASE3C_TEST_RESULTS.md`, the true 4-hour-per-
writer-level long soak (launched before this phase began) was still
running throughout Phase 4A's entire implementation. At last check
(t≈3,991s of the 100-writer phase's own 14,400s): healthy — 17,036
ops/sec, RSS flat ~8.6 MB, zero errors, zero corrupted segments across
34 checkpoint cycles. No new evidence of any WAL/coordinator defect was
found during Phase 4A's own work (which deliberately never modified any
WAL/coordinator internals — §1 below). Phase 4A proceeded on the
explicit, documented basis recorded in `PHASE4A_ARCHITECTURE.md` §0.

## 1. Repository state / environment

```
Phase 4A baseline (last Phase 3C commit before this phase began):
    383cac7f  (phase-3c: cross-phase doc updates -- the long soak was
               launched under this commit and continued running
               throughout all of Phase 4A's own work, unmodified)
Phase 4A commits (chronological):
    3c63559  RUBIC format specification (foundation only)
    ce29abd  architecture -- write-path integration + MemTable internal design
    63e7f08  MemTable core implementation
    6335add  MemTable property tests
    55005c3  bounded-memory WAL replay API (replay_streaming)
    92313e9  WAL <-> MemTable integration (LsmEngine)
    0fb1e34  1000-writer concurrency + recovery-equivalence + security edge cases
    8276e2d  WAL/MemTable-boundary crash tests (real external process kills)
    bf9e4ff  performance harnesses
```

Hardware/environment: unchanged from every prior phase this session —
Intel Core i7-7700 (4 physical / 8 logical cores), 16 GiB RAM, SATA SSD
(`E:`), Windows 10 Home 10.0.19045, `rustc 1.98.1`, `cargo 1.98.1`.

**MemTable configuration**: `BTreeMap<(Vec<u8>, u64), MemtableValue>`
(per `RubixDB-LSM-Engine-Specification-v1.0.md` §1.1, "Status: Final");
`DEFAULT_MAX_SIZE_BYTES = 4 MiB` (`PHASE4A_ADR.md` ADR-P4A-4);
`ENTRY_OVERHEAD = 32` bytes/entry (spec §1.3).

## 2. Regression gate

| Command | Result |
|---|---|
| `cargo test --lib` | 170/170 |
| `cargo test --lib --features test-util` | 170/170 |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo fmt --check` | clean |

Re-run after every commit this phase (11 commits, listed in git log)
— zero regressions at any point. `cargo test --release --lib` was not
re-run in isolation at the very end of this phase (deferred alongside
the performance comparison, §9, to avoid contaminating/being
contaminated by the still-running background soak's own release-profile
binary) — recorded as an open item, `PHASE4A_TEST_PLAN.md` §2.

## 3. MemTable core tests

| Test | Verifies | Result |
|---|---|---|
| `put_n_keys_get_them_all_back_with_correct_values` | Round trip | PASS |
| `delete_returns_tombstone_not_silently_absent` | Raw tombstone visibility | PASS |
| `multiple_versions_get_as_of_returns_the_version_current_at_that_point` | Multi-version `get_as_of` | PASS |
| `as_of_seq_before_first_write_returns_none` | Pre-first-write boundary | PASS |
| `put_delete_put_resolves_correctly_at_every_boundary` | Put/Delete/Put | PASS |
| `size_bytes_grows_by_exactly_entry_size_per_insert` | Size accounting | PASS |
| `tombstone_entry_size_has_zero_payload_len` | Tombstone accounting | PASS |
| `is_full_flips_at_the_configured_threshold` | Threshold | PASS |
| `range_returns_strictly_sorted_key_seq_order` | Ordering | PASS |
| `empty_memtable_behaves_correctly_without_panicking` | Empty edge cases | PASS |
| `freeze_produces_a_shared_immutable_handle_preserving_state` | Freeze, exercised across real threads | PASS |
| `seq_range_tracks_min_and_max_across_out_of_order_inserts` | `seq_range()` | PASS |
| `remaining_capacity_and_entry_count_are_accurate` | Phase-4A accessors | PASS |

**13/13 PASS** — every item in LSM Engine Spec §1.6's own checklist,
including the compile-time freeze guarantee (item 8: enforced by the
type system, not a runtime test — see `src/memtable/mod.rs`'s own
commented-out proof).

## 4. Property tests

| Test | Cases | Result |
|---|---|---|
| `memtable_matches_naive_reference_model` | 1,000 | PASS |
| `freeze_preserves_every_query_answer` | 1,000 | PASS |

Reproducible via `proptest`'s own standard mechanism (seed persisted to
`proptest-regressions/` on any failure — none occurred).

## 5. WAL/MemTable boundary — bounded-memory replay + crash tests

### `replay_streaming` unit tests (`src/wal/mod.rs`)

| Test | Result |
|---|---|
| `replay_streaming_delivers_every_record_in_order` | PASS |
| `replay_streaming_never_accumulates_more_than_one_segment_at_a_time` | PASS |
| `replay_streaming_stops_at_first_corruption_matching_scan_directory` | PASS |
| `replay_streaming_on_empty_wal_delivers_nothing_without_panicking` | PASS |
| `replay_streaming_on_a_never_created_directory_is_empty_not_an_error` | PASS |
| `replay_streaming_propagates_a_callback_error_and_stops` | PASS |

**6/6 PASS.** Existing `open_for_recovery`/`inspect`/`WalReplayResult`/
`walk_segment`/`scan_directory` tests (90 pre-existing WAL tests)
unchanged and still green — confirms this addition is genuinely
additive, not a silent modification (operating brief §24).

### Real external-process-kill crash tests (`examples/lsm_crash_cycle_test.rs`)

**Command**: `lsm_crash_cycle_test 25 4 2024 100 1200` (seed=2024, 4
concurrent writer threads per child process, kill delays 100-1200ms).
Idle machine (this session's own dedicated run, not concurrent with the
long soak).

| Metric | Result |
|---|---|
| Cycles | 25 |
| Successful recoveries | **25** |
| Failed recoveries | **0** |
| `LsmEngine::open` errors | **0** |
| Watermark regressions (`highest_sequence`/`durable_through` going backward) | **0** |
| Final `highest_sequence` / `durable_through` | 3,916 / 3,916 |
| Per-cycle `active_entries == highest_sequence == durable_through` | **True at every single cycle** |
| Recovery time range | 26.7 ms – 197.6 ms (scales with accumulated data, as expected) |

The exact-match property (`active_entries == highest_sequence`) at
*every* cycle is the strongest evidence this phase has for operating
brief §26's core invariant: the recovered MemTable always contains
precisely the WAL's own durable record count — never more (no
duplication), never less (no loss) — under real, externally-triggered,
uncooperative process kills, not merely a fsync-failure unit test.

## 6. WAL/MemTable integration tests (`src/lsm/mod.rs`)

| Test | Result |
|---|---|
| `put_then_get_round_trips` | PASS |
| `delete_then_get_returns_not_found` | PASS |
| `put_delete_put_resolves_to_the_newest_write` | PASS |
| `snapshot_reads_remain_stable_across_later_writes` | PASS |
| `freeze_triggers_at_the_configured_threshold_and_data_remains_visible` | PASS |
| `immutable_backpressure_rejects_further_freezes_past_the_limit` | PASS |
| `memory_accounting_remains_correct_across_freeze` | PASS |
| `wal_durability_ordering_is_respected_not_just_memtable_visibility` | PASS (real injected fsync failure) |
| `recovery_reconstructs_the_memtable_from_the_wal_after_restart` | PASS |
| `recovery_reconstructs_multiple_versions_correctly_across_a_restart` | PASS |
| `recovery_across_multiple_wal_segments_and_rotation` | PASS |
| `concurrency_one_writer` / `_ten_writers` / `_hundred_writers` / `_thousand_logical_writers` | PASS (all 4) |
| `large_key_and_value_are_handled_without_overflow_or_panic` | PASS |
| `an_entry_larger_than_the_configured_limit_does_not_hang_or_overflow` | PASS |
| `memtable_size_accounting_never_overflows_with_many_large_entries` | PASS |
| `recovery_matches_a_reference_model_after_restart` | PASS |

**19/19 PASS.**

## 7. Memory tests

Covered by `memory_accounting_remains_correct_across_freeze` (§6) and
`memtable_size_accounting_never_overflows_with_many_large_entries` (§6)
directly; `remaining_capacity_and_entry_count_are_accurate` (§3) and
`immutable_backpressure_rejects_further_freezes_past_the_limit` (§6)
indirectly. **Not separately run this phase**: a dedicated long-running
memory-leak/retained-allocation soak specifically for `LsmEngine`
(operating brief §33's fuller list — "memory after immutable release,"
which additionally requires Phase 4B's own flush-to-SSTable to actually
*release* an immutable memtable, since Phase 4A never drains the
immutable list at all) — recorded as a Phase 4B-relevant follow-up, not
fabricated.

## 8. Security tests

| Test | Verifies | Result |
|---|---|---|
| `large_key_and_value_are_handled_without_overflow_or_panic` | 64 KiB key, 1 MiB value | PASS |
| `an_entry_larger_than_the_configured_limit_does_not_hang_or_overflow` | Single entry >> configured limit | PASS |
| `memtable_size_accounting_never_overflows_with_many_large_entries` | Monotonic growth (wraparound detection) | PASS |
| `immutable_backpressure_rejects_further_freezes_past_the_limit` | Bounded immutable count under sustained load | PASS |

Manual audit: zero `unsafe` code introduced this phase (grepped);
`freeze_locked`'s backpressure check uses `EngineError::CapacityExceeded`
(existing type, no new error taxonomy); no payload/key/value logging in
any new code (`examples/lsm_crash_cycle_child.rs`/`lsm_load_test.rs`
print counts/stats only).

## 9. Performance

### MemTable-only (`examples/memtable_bench.rs`, n=100,000, idle-adjacent — single-threaded, CPU-bound, not meaningfully affected by the concurrent background soak)

| Metric | Result |
|---|---|
| Put throughput | 1,451,495 ops/sec |
| Get p50 latency | 400 ns |
| Get p99 latency | 1,200 ns |
| Get max latency | 62,200 ns |
| Ordered iteration | 74,085,050 entries/sec |
| Mixed 50/50 read/write | 1,504,223 ops/sec |

### WAL + MemTable (`examples/lsm_load_test.rs`) — **NOT reported as clean evidence**

A 20-writer smoke run (contaminated by the concurrent background soak)
returned 1,479 ops/sec — an order of magnitude below what a clean run
would show, matching the exact contention signature `PHASE3C_TEST_
PLAN.md` §1 rule 5 already diagnosed. **This number is explicitly
excluded from this document's own evidence base** — it confirms the
write path functions correctly (all 10,000 submitted entries landed
with correct values) but is not trustworthy as a performance figure.
The full 100/1,000-writer WAL-only-vs-WAL+MemTable comparison operating
brief §32/§36 requires remains **NOT RUN** — an explicit, named gap
(`PHASE4A_ADR.md` ADR-P4A-6), not converted to a fabricated PASS.

## 10. Concurrency tests

Covered in §6 above (1/10/100/1,000 logical writers through the real,
unmodified `BatchCoordinatorPool`) — all PASS, zero lost records, zero
duplicate/incorrect state at every scale tested.

## 11. Known limitations (carried forward honestly, not hidden)

1. **The full multi-threaded WAL-vs-WAL+MemTable performance comparison
   is NOT RUN** (§9). This is the single largest open item for
   certification purposes.
2. **`cargo test --release --lib` was not re-run in isolation at the
   end of this phase** (§2) — debug-profile testing is comprehensive
   and green (170/170), but the project's own established regression
   gate includes a release-profile pass this phase did not complete
   separately from the background soak's own release binary.
3. **Phase 3C's own WAL certification had not completed** when this
   phase's implementation work concluded (§0) — Phase 4A's own
   correctness work does not depend on it (no WAL/coordinator code was
   touched), but a fully clean certification chain requires it to land.
4. **`CapacityExceeded` on immutable backpressure reports failure for
   an already-succeeded write** (`PHASE4A_FAILURE_MODEL.md` §2) — a
   named, accepted trade-off, not a hidden defect; expected to become
   materially less severe once Phase 4B's flush-to-SSTable work makes
   backpressure transient rather than a hard wall.
5. **No dedicated long-running memory-leak soak for `LsmEngine`** (§7)
   — the existing memory tests are correctness-focused (accounting
   accuracy), not leak-detection-focused; a "memory after immutable
   release" test is not meaningful until Phase 4B actually releases
   immutables via flush.

## 12. Final certification decision (§42)

See `PROGRESS.md`'s Phase 4A entry and `ARCHITECTURE.md` for the
summary; this section states the decision itself.

**MEMTABLE NOT YET READY FOR RUBIC SSTABLE IMPLEMENTATION —
BLOCKERS REMAIN**, stated honestly per operating brief §42's own "do
not certify based only on unit tests" instruction, even though every
correctness/durability/crash-recovery/concurrency test attempted this
phase passed cleanly. Two explicit blockers:

1. **The full WAL-vs-WAL+MemTable performance characterization is not
   run** — operating brief §36 requires it as a certification
   precondition ("performance is characterized"), and it genuinely is
   not, yet, for the combined path (only the MemTable-only, single-
   threaded numbers are clean).
2. **The WAL foundation itself (Phase 3C) had not reached its own final
   certification** when this phase's work concluded — Phase 4A's own
   architecture doc (`PHASE4A_ARCHITECTURE.md` §0) explicitly
   conditioned proceeding on this being resolved before Phase 4A itself
   could be considered fully certified, not merely "implemented and
   tested in isolation."

**What is solid and does not need to be redone**: MemTable correctness
(unit + property tests), the WAL-durability-before-MemTable-apply
ordering (verified both by unit test and by 25/25 real external-kill
crash cycles with an exact `active_entries == highest_sequence` match
every time), bounded-memory recovery (`replay_streaming`, verified both
structurally and by dedicated tests), concurrency correctness up to
1,000 logical writers through the real production coordinator, and
memory-accounting correctness across freeze. None of this needs to be
re-verified once the two blockers above close — the next step is
closing them (finish the Phase 3C soak, then re-run the WAL-vs-
WAL+MemTable comparison on the resulting idle machine), not redoing any
correctness work.
