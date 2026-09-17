# RubiXDB Phase 5 — Test Plan

## 1. Scope

Test the Manifest (`src/manifest/`) and its integration into
`LsmEngine`'s startup, flush, and read paths, per `RUBIC_MANIFEST_
FORMAT_SPECIFICATION.md` and `PHASE5_MANIFEST_ARCHITECTURE.md`.
Explicitly not in scope: Compaction, Manifest rotation/compaction,
replication, multi-node — none exist, so no test exercises them.

## 2. Format-level unit tests (`src/manifest/format.rs`, `src/manifest/state.rs`, `src/manifest/recovery.rs`)

- Edit encode/decode round-trips for all three edit types.
- Structural corruption: unknown `edit_type`, wrong body length,
  `min_seq > max_seq`.
- Semantic corruption: `REMOVE_SSTABLE` for a never-added id,
  checkpoint regression.
- Idempotence: duplicate `ADD_SSTABLE`, duplicate `REMOVE_SSTABLE`,
  repeated identical `SET_CHECKPOINT`.
- Torn-tail classification: mid-header, mid-body, trailing-checksum-
  mismatch — all torn, not corrupt. Non-tail checksum mismatch —
  corrupt, not torn. Implausible declared length — corrupt.
- Bounded-memory replay: sequential frame-by-frame walk, never a
  whole-file read.
- Property test (`proptest`, 64 cases): a randomized, semantically-
  legal sequence of add/remove/checkpoint-advance operations, applied
  both to a real `Manifest` (via `append_sync` + `replay_readonly`) and
  to an independent, from-scratch reference model (never calling into
  `ManifestState` itself) — checks live-set and checkpoint equivalence,
  plus that replaying the same file twice produces the same result
  both times (repeated recovery).

## 3. Manifest module integration tests (`src/manifest/tests.rs`)

- Fresh (absent) Manifest replays to empty state, not an error.
- `append_sync` + replay round-trip.
- Reopening after a torn tail physically truncates it and can still
  append correctly afterward.
- Non-tail corruption fails `open_after_exclusive_lock` closed.
- Multiple sequential flushes accumulate correctly.

## 4. LSM integration tests (`src/lsm/tests.rs`)

- A flush durably advances the checkpoint and keeps live WAL segment
  count bounded under sustained writes with a small `max_segment_size`
  (direct evidence `purge_before` is actually invoked, not merely
  wired).
- After a checkpoint-then-restart, replay is bounded (`active_entry_
  count() < total_keys_written`) and every key remains correctly
  readable regardless of which tier now holds it.
- A Manifest-live SSTable missing from disk fails `open()` closed.
- A valid-but-never-acknowledged SSTable file is durably re-added and
  becomes live (the crash-recovery path LSM Engine Spec §7.2 defines).
- An invalid file at a never-used id fails `open()` closed (neither
  silently included nor silently ignored).
- A corrupted (non-tail) `MANIFEST` file fails `open()` closed.
- Every pre-existing Phase 1-4B test continues to pass unmodified.

## 5. Crash-injection tests (real external process kills)

`examples/sstable_flush_crash_test.rs` + `_child.rs`, extended this
phase to verify, after every real kill-and-reopen cycle:
- `open()` never errors.
- No `*.sst.tmp` survives the sweep.
- `highest_sequence`/`durable_through` never regress.
- **The Manifest checkpoint never regresses.**
- **`active_entry_count() + recovery_stats().checkpoint_markers_
  replayed == highest_sequence - checkpoint_seq` exactly** — the
  precise bounded-replay accounting invariant that caught ADR-P5-4's
  real idempotent-retry bug.

Two independent seeds, ≥80 cycles each, both writer counts (4 and 8)
covering the full range from "before any flush" through "dozens of
SSTables and multiple checkpoints already durable."

## 6. Freeze-frequency ablation

`examples/freeze_ablation_test.rs` — isolates freeze frequency from
flush I/O to investigate the Phase 4B 100-writer performance anomaly
(`PHASE5_ADR.md` ADR-P5-1).

## 7. Performance

WAL-only, WAL+MemTable (no flush), WAL+MemTable+SSTable+Manifest
(realistic 4 MiB memtable), at 100 and 1,000 writers, same machine,
same methodology as every prior phase. See `PHASE5_PERFORMANCE.md`.

## 8. Soak

A bounded-duration (not the full multi-hour Phase 3C-style soak —
explicitly labeled as such) sustained-write soak with periodic real
process-kill crashes interleaved, verifying throughput, WAL/Manifest
size, SSTable count, checkpoint progression, and post-recovery
correctness after each crash. See `PHASE5_PERFORMANCE.md` §7.

## 9. Static quality gate

`cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D
warnings`, `cargo test --lib`, `cargo test --release --lib`, `cargo
test --lib --features test-util` — all required clean before
certification.

## 10. Explicitly out of scope / not built this phase

- A dedicated fault-injection test for the flush-thread panic path
  (`PHASE5_FAILURE_MODEL.md` §5) — confidence via code review plus the
  shared, already-crash-tested idempotence machinery instead.
- An explicit runtime assertion for LSM Engine Spec §7.4's defensive
  replay invariant — named as an open item, not implemented.
- Property-test coverage for "crash ordering" and "WAL purge
  eligibility" specifically as their own dedicated proptest properties
  (distinct from the edit-replay-equivalence property actually built,
  §2) — the 180 real external-process crash cycles exercise both
  properties directly and empirically (checkpoint monotonicity and
  purge safety were both asserted and held on every cycle), which was
  judged stronger evidence than a synthetic, in-process simulation of
  the same properties given this phase's time budget; named here as a
  deferred item, not silently dropped.
