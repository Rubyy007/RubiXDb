# RubiXDB Phase 5 — Test Results

Single source of truth for Phase 5's pass/fail data, per this project's
own standing rule. Does not overwrite or replace `PHASE1_TEST_RESULTS.md`
through `PHASE4B_TEST_RESULTS.md` — all stand as the historical record
of their own phase. Every item below is explicitly one of **PASS**,
**FAIL**, **INCONCLUSIVE**, or **NOT RUN** — no unresolved item is
converted to PASS merely because another test passed.

## 0. Precondition / release-gate audit

- **Phase 3C long soak**: **IN PROGRESS.** Relaunched, uninterrupted,
  as the final action of this phase's own work
  (`PHASE5_ADR.md` ADR-P5-0), matching `PHASE3C_TEST_PLAN.md`'s exact
  original methodology (`long_soak_test -- 100 14400 ...` then `...
  1000 14400 ...`). Not complete at the time this document is
  finalized — recorded honestly, not fabricated into a PASS.
- **Phase 4B 100-writer performance anomaly**: **INCONCLUSIVE
  (narrowed, not resolved).** A dedicated ablation
  (`examples/freeze_ablation_test.rs`) conclusively rules out the
  bounded-`BTreeMap`-depth hypothesis; the actual mechanism behind the
  original observation remains unexplained (`PHASE5_ADR.md` ADR-P5-1,
  `PHASE5_PERFORMANCE.md` §1). Non-blocking — no correctness property
  depends on this being explained.

## 1. Repository state / environment

```
Baseline commit (session start, before any Phase 5 work):
    416573e89ebe6f8aea7ae749f6e67976cc529fc1
```

Hardware/environment: unchanged — Intel Core i7-7700 (4 physical / 8
logical cores), 16 GiB RAM, SATA SSD (`E:`), Windows 10 Home
10.0.19045, `rustc 1.98.1`, `cargo 1.98.1`. Measured 2026-09-17,
verified idle before each measurement window (re-verified after the
soak-sequencing correction, `PHASE5_ADR.md` ADR-P5-0).

**Manifest configuration**: `RUBIC_MANIFEST_FORMAT_SPECIFICATION.md`
§2-§3 — `length:u32 LE, crc32c:u32 LE, body` frames (independent
implementation, byte-compatible with the WAL's own frame header shape),
three edit types (`ADD_SSTABLE`, `REMOVE_SSTABLE`, `SET_CHECKPOINT`,
exactly as the authoritative LSM spec defines, no others invented). No
Manifest rotation/compaction this phase (matches LSM Engine Spec
§6.3's own explicit v1 non-goal).

## 2. Static quality gate

| Command | Result |
|---|---|
| `cargo fmt --check` | PASS — clean |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS — clean |
| `cargo test --lib` | PASS — 253/253 |
| `cargo test --lib --features test-util` | PASS — 253/253 |
| `cargo test --release --lib` | PASS — 253/253 |

253 total library tests (up from Phase 4B's 216): 32 new in
`src/manifest/` (format encode/decode, corruption detection,
idempotence, torn-tail classification, module-level integration tests,
a 64-case property test against an independent reference model), 5 new
in `src/lsm/tests.rs` (checkpoint-advances-and-WAL-bounded, bounded-
replay-after-restart, missing-live-sstable-fails-closed, garbage-
orphan-fails-closed, Manifest-corruption-fails-closed), plus every
pre-existing Phase 1-4B test unchanged and re-verified with zero
regressions.

## 3. Manifest format-level unit tests (`src/manifest/format.rs`, `src/manifest/state.rs`, `src/manifest/recovery.rs`)

| Test category | Count | Result |
|---|---|---|
| Edit encode/decode round-trip (all 3 types) | 3 | PASS |
| Structural corruption (unknown type, wrong length, min>max) | 3 | PASS |
| Frame-header byte-compatibility cross-check | 1 | PASS |
| Semantic corruption (invalid reference, checkpoint regression) | 2 (state) + 2 (recovery, end-to-end) | PASS |
| Idempotence (duplicate add/remove/checkpoint) | 3 | PASS |
| Torn-tail classification (mid-header, mid-body, trailing-checksum) | 3 | PASS |
| Non-tail corruption escalation | 1 | PASS |
| Implausible length rejection (before allocation) | 1 | PASS |
| Bounded-memory sequential replay, cursor-independence | 2 | PASS |

## 4. Manifest module integration tests (`src/manifest/tests.rs`)

| Test | Purpose | Result |
|---|---|---|
| `replay_readonly_on_absent_manifest_is_empty_not_an_error` | Fresh-engine bootstrap | PASS |
| `open_creates_the_file_and_append_sync_round_trips_through_replay` | Basic durability round-trip | PASS |
| `reopen_after_exclusive_lock_truncates_a_torn_tail_and_can_still_append` | Physical torn-tail truncation, continued append | PASS |
| `non_tail_corruption_fails_closed_on_open` | Fail-closed on real corruption | PASS |
| `multiple_flushes_accumulate_correctly` | Multi-edit accumulation | PASS |
| `property::manifest_replay_matches_independent_reference_model` (64 proptest cases) | Edit-replay equivalence + repeated recovery, against a from-scratch reference model | PASS |

## 5. LSM/Manifest integration tests (`src/lsm/tests.rs`)

| Test | Purpose | Result |
|---|---|---|
| `checkpoint_advances_and_wal_segments_stay_bounded` | Real checkpoint advance + real WAL purge under sustained writes | PASS |
| `restart_replays_only_post_checkpoint_records_and_all_data_remains_correct` | Bounded replay after restart, full data correctness | PASS |
| `missing_live_sstable_fails_closed_on_open` | Manifest/disk divergence, fail-closed | PASS |
| `garbage_orphan_sstable_file_fails_closed_not_silently_handled` | Invalid never-acknowledged file, fail-closed | PASS |
| `manifest_corruption_fails_closed_on_open` | Corrupted MANIFEST, fail-closed | PASS |

## 6. Crash-injection tests (real external-process kills)

Command: `cargo build --release --example sstable_flush_crash_child
--example sstable_flush_crash_test`, then:

```
target\release\examples\sstable_flush_crash_test.exe 100 4 42 1 60
target\release\examples\sstable_flush_crash_test.exe 80 8 1337 1 40
```

| Run | Cycles | Writers | Seed | Delay range | Result |
|---|---|---|---|---|---|
| 1 (post-fix) | 100 | 4 | 42 | 1-60 ms | PASS — 100/100 |
| 2 | 80 | 8 | 1337 | 1-40 ms | PASS — 80/80 |

**180/180 total cycles, zero failures, after fixing a real bug this
exact harness found** (`PHASE5_ADR.md` ADR-P5-4): the first run against
the initial idempotent-retry design failed 94/100 cycles on an exact-
accounting invariant (`active_entry_count() + recovery_stats().
checkpoint_markers_replayed == highest_seq - checkpoint_seq`), which
traced to a retried flush attempt durably resubmitting a second
`CHECKPOINT_MARKER` for one logical flush. Fixed by tracking each
step's own durable-success state independently
(`published`/`checkpoint_marker`/`checkpoint_recorded`) rather than
only the SSTable-build step. Re-run clean, 180/180, after the fix.

Every cycle additionally verified: `open()` never errored, no
`*.sst.tmp` survived the discovery sweep, `highest_sequence`/
`durable_through`/`checkpoint_seq` never regressed. Run 1 grew to a
final checkpoint of 376 with 43 live SSTables; run 2's shorter, higher-
concurrency cycles never completed a full checkpoint cycle (final
checkpoint 0) — both are legitimate, safe outcomes (checkpoint
monotonicity holds trivially at 0; all data remained fully recoverable
from the WAL either way).

## 7. Bounded soak with periodic real crashes

Command: `target\release\examples\manifest_soak_test.exe 8 8 20`. See
`PHASE5_PERFORMANCE.md` §4 for the full per-cycle table.

**8/8 cycles PASS**: `open()` never errored, the bounded-replay
invariant held exactly every cycle, `checkpoint_seq`/`highest_seq`
monotonically non-decreasing throughout. **WAL byte count stayed at
`0` across every measurement** (checkpoint tracked within ~1% of
`highest_seq` at all times under this tiny-memtable workload) —
directly demonstrating the required property: WAL storage does not
grow without bound while the system is operating normally and
successfully checkpointing. Manifest (24 KB -> 184 KB) and SSTable-
directory (966 KB -> 7.7 MB) size grew across the 8 cycles as expected
— an accepted, already-documented consequence of no Compaction/
Manifest-compaction existing yet (LSM Engine Spec §6.3's own explicit
v1 non-goal), not a defect.

Explicitly **not** run this phase: the multi-hour, realistic-
configuration soak Phase 3C's own methodology represents — this
bounded, stress-configuration soak demonstrates the mechanism, not
long-duration production stability at realistic scale (§0 above covers
the still-in-progress genuine long soak).

## 8. Performance

See `PHASE5_PERFORMANCE.md` for full numbers. Summary: at 1,000
writers under a realistic 4 MiB memtable, the full Manifest-integrated
pipeline (96,368 ops/sec) measured within ~2% of a clean WAL-only
baseline (98,225 ops/sec) — Manifest/checkpoint/purge overhead is
negligible at realistic configuration, extending `PHASE4B_
PERFORMANCE.md`'s identical finding for SSTable flush alone.

## 9. Security review

- No `unsafe` code added in `src/manifest/` or the `src/lsm/` changes
  this phase (verified: `grep -rn "unsafe" src/manifest` returns
  nothing; the one `unsafe`-adjacent-sounding addition,
  `std::panic::AssertUnwindSafe`, is a safe, zero-cost marker type, not
  `unsafe` code).
- Every length-prefixed field in a Manifest frame is checked against a
  documented maximum (`MAX_EDIT_BODY_LEN = 64`) before it can size an
  allocation — `src/manifest/recovery.rs`'s single choke point.
- No new production dependency this phase (Phase 4B's `xxhash-rust`
  remains the only one this project has added since Phase 3).
- Manifest file naming is fixed (`MANIFEST`, no user input), no path
  traversal surface.
- Temporary-file-then-atomic-rename discipline for SSTables is
  unchanged from Phase 4B; the Manifest's own durability discipline
  (append + immediate `fsync`, every edit) mirrors it.

## 10. Final certification decision

See `PHASE5_ARCHITECTURE.md`/`PHASE5_ADR.md` for full reasoning.

**MANIFEST NOT READY FOR COMPACTION — BLOCKERS REMAIN.**

Two explicit blockers, **neither a correctness defect found in this
phase's own extensive testing** (253/253 unit tests, 180/180 real
crash cycles across two seeds, an 8-cycle bounded soak, all clean after
one real bug was found and fixed by the crash harness itself):

1. **Phase 3C's own WAL certification has still never completed.**
   This item has now been carried forward, unresolved, across Phase
   4A, Phase 4B, and this phase — every one of those phases proceeded
   provisionally on the documented basis that they do not modify
   WAL/coordinator internals, which remains true here too. It is
   recorded here, not silently dropped, and is **not** something Phase
   5's own work can close by itself (only completing that specific
   long soak can).
2. **The true multi-hour, realistic-configuration Phase 5 soak is not
   complete** — only a bounded, stress-configuration soak (§7) and the
   relaunched-but-still-running Phase 3C soak (§0) exist as evidence at
   the time this document is finalized.

Every correctness/durability/crash-recovery/idempotence/corruption-
handling property actually tested this phase passed cleanly and does
not need to be redone once these two items close — the recommended
next step is closing them (let both soaks run to completion on an
idle machine), not redoing correctness work. Compaction, the next
phase per operating brief §55, should not begin until this
certification is re-evaluated with that evidence in hand.
