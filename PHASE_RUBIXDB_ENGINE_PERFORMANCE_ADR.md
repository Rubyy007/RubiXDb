# PHASE RUBIXDB — ENGINE PERFORMANCE ADR (WAL group-commit throughput)

**Status:** ENGINE-BLOCKED (product certification gate: *WRITE* throughput targets M1.2 / M1.3).
**Date:** 2026-10-02. **HEAD at start:** `b7f0e8b`. **Scope rule honoured:** `src/wal/`
was not modified (`git diff --stat -- src/wal/ src/manifest/ src/sstable/ src/compaction/`
empty before and after this phase). No test, threshold, or concurrency level was changed.

This ADR is append-only history of a *verified, still-failing* gate. It does not claim a PASS.

---

## 1. The failing gates (reproduced on current HEAD)

| Test | Target | Requirement origin |
|---|---|---|
| `m1_2_hundred_writers_throughput` (`tests/group_commit/hundred_writers_throughput.rs`) | >= 15,000 ops/s, 100 writer threads x 1,000 records | Phase 1 (group commit) brief |
| `m1_3_thousand_writers_throughput` (`tests/group_commit/thousand_writers_throughput.rs`) | >= 80,000 ops/s, 1,000 writer threads x 1,000 records | Phase 1 (group commit) brief |

Both fail in debug **and** release. Both are real, unweakened assertions.

## 2. Current measurements (this session, HEAD `b7f0e8b`, i7-7700 / 16 GB / SATA SSD on `E:`)

Isolated runs (`cargo test [--release] --test group_commit m1_ -- --test-threads=1`):

| Run | Mode | M1.1 single-writer median (baseline `Immediate` -> GroupCommitter) | M1.2 ops/s | M1.3 ops/s |
|---|---|---|---|---|
| 1 | release | 2.839 -> 3.121 ms (p99 9.750) | 8,599 | 63,103 |
| 2 | release | 2.888 -> 3.073 ms | 11,280 | 63,715 |
| 3 | release | 2.991 -> 3.124 ms | 10,870 | 64,132 |
| 4 | debug | 2.874 -> 3.009 ms (p99 5.839) | 11,717 | 6,140 (162.9 s, CPU-bound by 1,000 debug threads) |

Inside the full `cargo test --workspace --no-fail-fast` runs (default parallel test threads
inside the `group_commit` binary, so the M1.x tests contend with each other for the same disk):

| Run | M1.2 ops/s | M1.3 ops/s |
|---|---|---|
| full workspace, release | 6,013 | 39,538 |
| full workspace, debug | 718 | 4,934 |

**Isolated release spread:** M1.2 8,599 - 11,280 (min/max, 31% spread); M1.3 63,103 - 64,132 (1.6% spread).
M1.2 attainment: 57% - 75% of target. M1.3 attainment: 79% - 80% of target.
Browser/IDE processes (Brave, Visual Studio) were running during every measurement; this is
an ordinary developer machine, not a quiesced rig.

## 3. Historical measurements (recorded elsewhere; not rewritten)

| Source | M1.2 | M1.3 | Note |
|---|---|---|---|
| `PROCESS.md` 2026-09-14 (first measurement) | 10,115 | 36,673 | original batch-window formula |
| `FINAL_WAL_ANALYSIS.md` §19 original formula | 8,944 | 31,438 | before window-size sweep |
| `FINAL_WAL_ANALYSIS.md` §19 after sweep (adopted formula) | ~9,800 - 11,800 | ~50,600 - 64,900 | production formula today |
| `FINAL_WAL_ANALYSIS.md` §23 final (median of 4) | **10,746** (71.6%) | **63,207** (79.0%) | |
| `PROGRESS.md` Increment 2 (`write_batch`) | 7,330 | 45,395 - 52,460 | measured under parallel-test contention |
| `PROGRESS.md` Increments 3-18 | "recurs identically" | "recurs identically" | each verified pre-existing via `git stash` / zero `tests/` diff |

**Conclusion on drift:** today's isolated numbers (M1.2 median 10,870; M1.3 median 63,715)
are within 1.2% of the certified-WAL analysis medians (10,746 / 63,207). There is **no
regression**; this is a stable, reproducible ceiling.

## 4. Why the target is not met (evidence, not assertion)

All of the following is from `FINAL_WAL_ANALYSIS.md` and is *consistent with* today's
measurements (single-writer baseline `fsync` path is again 2.84 - 2.99 ms, matching the
2.36 - 2.42 ms raw `fsync_only` + write cost recorded there):

1. Throughput = records-per-fsync / cycle-time. Cycle time is ~46 - 54% `FlushFileBuffers`
   latency (4.0 - 6.1 ms measured three independent ways, stable across a 400x window-size range).
2. At 100 writers the batch is already 93% full (~93.4 of 100 writers per sync). Reaching 15,000
   ops/s would need ~130 records/sync (more than the writer count) **or** ~40% shorter cycles;
   window tuning was swept (10 configurations) and the adopted 5 ms window is within 1.5 - 2.6% of best.
3. The hardware is two consumer SATA SSDs, not NVMe. The targets were set before any measurement
   on this device.
4. **New this session:** isolated-vs-parallel variance (Section 2) shows the pass/fail outcome is *also*
   sensitive to what else is touching the disk, but even the quietest isolated run is below target.

### Is the target realistic?
- **On this machine, with the certified algorithm: no.** Best isolated observation was 11,717 (M1.2)
  and 64,132 (M1.3), i.e. 78% / 80% of target, with all three measured against the same fsync floor.
- **On faster storage: unknown, plausibly yes.** `FINAL_WAL_ANALYSIS.md` §20 P1-1 names re-running the
  unmodified binary on NVMe as the single most valuable experiment; it has still not been done and
  cannot be done on this machine. That is an open measurement, **not** evidence of a pass.

## 5. Candidate engine architectures (not implemented; require separate authorization)

| # | Candidate | Targets | Risk / cost | Status |
|---|---|---|---|---|
| A | Re-measure unchanged WAL on NVMe-class media | fsync floor | none to code | **Recommended first**, hardware-gated |
| B | Pipelined leader (overlap batch N+1 window with batch N fsync) | cycle time | already tried: M1.3 fell to 36.9 - 39.6k (fsync ~2x slower under overlap) and was reverted | rejected on this device |
| C | Spin-vs-sleep window wait experiment (`FINAL_WAL_ANALYSIS.md` §20 P1-2) | CPU / scheduling | low, env-gated; not expected to close the gap | measurement task |
| D | Sharded WALs (N independent logs, N concurrent fsyncs) | parallel fsync | changes recovery ordering/watermark invariants in a *certified* engine; needs full crash re-certification | requires new ADR + authorization |
| E | Relax durability (e.g. periodic sync) | trades away guarantee | violates certified durability contract | **rejected** |
| F | Lower the target to what the hardware supports | the test | would be threshold-weakening | **forbidden by this phase's rules** |

## 6. Decision

* **Product certification gate for WAL write throughput = ENGINE-BLOCKED.**
* `src/wal/` unchanged. No further engine work done in this phase (per mandate: "stop engine-related work").
* The WAL is **durability-correct** (certified, crash-consistency/pathological-recovery suites pass in this
  session's regression); the block is a *performance-target* block, not a correctness block.
* Product-level write performance is nevertheless characterised honestly in
  `PHASE_RUBIXDB_FINAL_SINGLE_NODE_PERFORMANCE.md` (single-statement INSERT/UPDATE/DELETE/txn latency under 1 - 64 clients).
* Reopening requires: (a) NVMe re-measurement with the unmodified binary, or (b) an authorized ADR for candidate D.


---

# ADR-ENG-OPS-001 — WAL corruption does not halt `LsmEngine::open`; MANIFEST has no format version
**Date:** 2026-10-03 · **Phase:** Production operations + disaster recovery · **Status:** FOUND, REPRODUCED, MITIGATED AT THE PRODUCT LAYER, ENGINE FIX NOT MADE (engine boundary: STOP).

## 1. Finding A — a corrupt WAL segment does not stop startup
**Reproduction** (`src/ops/physical_tests.rs::engine_open_ignores_a_corrupt_wal_segment`, plus `wrong_format_versions_are_refused…`): take a real data directory (2 WAL segments, live SSTables), set the *newest* segment's header `format_version` to 2 (or flip a byte inside its first frame), call `LsmEngine::open`. Result: **`Ok`**; the directory is modified (a fresh segment is created, obsolete ones purged). `check_physical` reports `WAL_CORRUPT` for the same directory.
**Root cause (source):** `src/lsm/mod.rs` `LsmEngine::open` — `let _summary = wal::replay_streaming(dir, &wal_config, …)?;` and `let (file_wal, _replay) = FileWal::open_for_recovery(dir, wal_config)?;` — both results carrying `corrupted_segments(_count)` are discarded. `src/wal/mod.rs` (`scan_directory`, comment at the end) states "the caller is already required to halt on non-empty corrupted_segments", and `scan_directory` stops at the first corrupted segment and drops its records and every later segment's records. So a damaged segment ⇒ silent loss of all records from that segment onward; startup succeeds; acknowledged data is gone with no error.
**Measurement:** the checker's physical pass classifies the case (`WAL_CORRUPT`); the guarded product open refuses it and leaves the directory byte-identical (snapshot hash test).
**Candidate designs:** (1) in `LsmEngine::open`, return `EngineError::Corruption` when either result reports a corrupted segment (≈4 lines; the WAL layer already supplies everything). (2) Same, but offer an explicit operator opt-in to open "salvage mode" that reports exactly what was dropped. (3) Leave the engine; guard each product entry point (**done**).
**Risk/correctness/durability/recovery of (1):** strictly safer — converts silent data loss into a loud refusal; recovery semantics for *torn tails* are unchanged (they are `truncated`, not `corrupted`). Needs the pathological-recovery matrix and `crash_consistency` re-run, and a decision on salvage mode. Performance: none.
**Decision here:** NOT changed (mission rule: stop before modifying the engine). **Mitigation shipped:** `ops::format::startup_guard` runs a read-only `wal::replay_streaming` preflight and the directory-format check before the engine opens, at every product entry point (`rubixdb-api` `main`, `rubixdb gui`/CLI host, `ops::open`). Cost: one extra streaming read of the WAL at startup (measured in the performance document). **Recommended next step (needs authorisation):** design (1).

## 2. Finding B — the MANIFEST file has no magic and no format version
`src/manifest/format.rs`: frames of `len/crc/body` only; `edit_type` is the only discriminator. A future incompatible manifest is rejected only as an unknown edit type or checksum failure (fail-closed, but unversioned and not self-describing). WAL segments (`RBXWALv1` + version) and SSTables (`RBXSST01` + version) are versioned.
**Mitigation shipped:** directory-level `DATA_FORMAT` marker (`ops::format`) written when a product entry point creates a directory, checked before open; legacy directories (no marker) are accepted unmodified. **Engine change (not made):** add a versioned header to the manifest; requires a migration rule for existing files.

## 3. Finding C (informational) — an obsolete WAL segment with an invalid header is tolerated and deleted at open
Segments entirely below the checkpoint are not needed; the engine removes them as normal housekeeping. Consistent with the design; recorded so nobody mistakes it for Finding A.
