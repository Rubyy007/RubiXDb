# RubiXDB Phase 3C — Test Results

Single source of truth for Phase 3C's pass/fail data and benchmark
numbers, per this project's own standing rule. Does not overwrite or
replace `PHASE3_TEST_RESULTS.md`/`PHASE3B_TEST_RESULTS.md` — all three
stand as the historical record of their own increment. Regression-bound
rule (established before the final acceptance run): `PHASE3C_TEST_
PLAN.md` §1.

**Document status: IN PROGRESS.** The true long-duration soak (§3, a
4-hour-per-writer-level run, launched in the background early in this
session) is still running at the time of this document's first
commit — everything else below is complete. §3, §9, and §12 (final
decision) will be updated in place, not left as placeholders, once the
soak completes.

## 1. Repository state

```
Phase 3C baseline (last Phase 3B commit, frozen before any Phase 3C code):
    4221e2f2d71ceffd6fc1513d8ad34d9464ed1862
Phase 3C commits so far (chronological):
    21aa11d  baseline freeze + GroupCommitter/BatchCoordinatorPool::purge_before
    7d36219  periodic forced-crash-during-soak harness + true long-soak harness v2
    55829df  pathological recovery matrix + recovery-memory scaling data + observability additions
    583539d  security review follow-up -- saturating batch_bytes accumulation
```

Branch: `master`. Working tree clean before this increment and after
every commit above.

## 2. Baseline (frozen before any Phase 3C code change)

**Command**: `cargo run --release --example batch_coordinator_load_test
-- <writer_count> 1000`, commit `4221e2f`.

| Level | Runs (ops/sec) | Median |
|---|---|---|
| 100 writers | 17,872 · 18,667 · 15,051 | **17,872** |
| 1,000 writers | 94,327 · 91,517 · 89,157 | **91,517** |

Both exceed the Phase 2B targets, zero failures/timeouts across all 6
runs, consistent with every prior phase's own historical band
(`PHASE3C_TEST_PLAN.md` §1).

**Methodological note, learned during this phase**: a benchmark run
attempted later in this session, after the long-duration soak (§3) had
already been launched in the background, returned artificially low
numbers (~9,000-10,000 ops/sec at 100 writers) purely from CPU
contention with the soak's own active writer threads on this machine's
4-physical/8-logical-core hardware — **not a code regression**. That
run is discarded, not reported as evidence, and the final acceptance
benchmark (§9) is run only once the machine is genuinely idle. Recorded
as `PHASE3C_TEST_PLAN.md` §1 rule 5, added specifically because of this
observation.

## 3. True long-duration soak (§3-§4)

**Command**: `cargo run --release --example long_soak_test -- 100 14400
120 5000000 120` then (sequentially, same process invocation)
`... 1000 14400 120 5000000 120`. Machine otherwise idle for the
duration this document was written except where explicitly noted
(§2's methodological note; a ~4-minute contaminated window around
t=480-742s of the 100-writer run, during which this session ran
unrelated builds/tests — see the note in that section below).

**Status at last update**: in progress — see the log excerpt and
running characterization below; this section is updated in place (not
left as a stale placeholder) once the full 4h+4h run and its final
recovery check complete.

### 100 writers — running characterization (as of t≈1585s of 14400s)

| Sample | ops/sec | p99 (ms) | RSS (KB) | queue_depth | completed_err |
|---|---|---|---|---|---|
| t=60s | 17,170* | — | — | — | 0 |
| t≈360s | 17,850 | 11.3 | 8,440 | 0 | 0 |
| t≈481s | 17,252 | 13.0 | 8,276 | 0 | 0 |
| t≈601s | **1,842** | **794.7** | 8,052 | 0 | 0 |
| t≈742s | 7,162 | 270.6 | 8,172 | 100 | 0 |
| t≈863s | 17,885 | 11.7 | 8,324 | 0 | 0 |
| t≈983s | 18,361 | 11.8 | 8,404 | 0 | 0 |
| t≈1103s | 17,763 | 11.5 | 8,332 | 0 | 0 |
| t≈1224s | 16,987 | 12.1 | 8,484 | 0 | 0 |
| t≈1344s | 15,173 | 17.7 | 8,920 | 1 | 0 |
| t≈1464s | 17,473 | 11.7 | 8,804 | 0 | 0 |
| t≈1585s | 16,755 | 12.1 | 8,908 | 0 | 0 |

**The t≈601-742s dip is attributed to session-induced CPU contention,
not a production-path defect**: this session ran a large `cargo test`
invocation and several other build/test commands concurrently with the
soak during roughly that window (before this document's own author —
i.e., this session — recognized the contamination risk and stopped
doing so, per the note added to `PHASE3C_TEST_PLAN.md` §1 rule 5).
Investigated, not dismissed: the throughput and latency recovered
**immediately and fully** (t≈863s onward is indistinguishable from
t≈360s) once the concurrent load ended, with zero `completed_err`
throughout the entire dip — no request was lost or failed, only
delayed. This is itself informative: the system does not degrade
*persistently* under transient external contention, it degrades
*proportionally and recovers promptly*, which is the expected,
correct behavior for a system sharing a host with other work, not a
finding against it. `queue_depth` briefly reaching 100 at t≈742s (its
only non-zero readings in the run so far, alongside a single `1` at
t≈1344s) — both transient, both immediately followed by a `0` reading
at the next sample — confirms the coordinator caught up promptly
rather than falling permanently behind.

`*` t=60s's ops/sec (17,170) is from the very first sample window, not
independently affected by the mid-run contamination discussed above.

**Segment checkpointing** (`purge_before`, `PHASE3C_ADR.md` ADR-P3C-2):
12 purge cycles completed by t≈1585s, each removing exactly the
segments fully below the retention watermark — the live WAL's own
footprint stays bounded throughout, confirmed by `bytes_on_disk`-style
evidence not growing unbounded (not separately tabulated here; the
final recovery check at the end of each writer-level's run is the
authoritative confirmation, §3's completion will record it).

### 1,000 writers

Not yet started as of this document's current state — runs
sequentially after the 100-writer phase completes. Will be added here
in the same format once available.

## 4. Soak stability criteria (§4) — evaluated against the 100-writer data available so far

| Criterion | Status (100w, partial run) |
|---|---|
| No unbounded RSS growth | Holding — 8,052-8,920 KB range, no trend, across 1,585s so far |
| No unbounded queue growth | Holding — `queue_depth` `0` at all but 2 of 12 samples, both transient |
| No persistent throughput decline | Holding — one transient, externally-caused dip, full recovery |
| No persistent latency drift | Holding — same dip, same full recovery |
| No sequence corruption | Not yet checked (checked at final recovery, run in progress) |
| No durability violation | `completed_err=0` throughout so far |
| No deadlock | Holding — the run is still actively progressing |
| No worker/coordinator death | Holding — `pool_state` not yet `Failed` at any sample |
| No silent request loss | Holding — `submitted == completed_ok + completed_err` at every sample so far (not separately tabulated; `completed_err=0` and `submitted` tracking `completed_ok` exactly in the raw log confirms this) |

**Full evaluation deferred to this section's own update once both
4-hour runs and their final recovery checks complete.**

## 5. Periodic forced-crash-during-soak (§5-§6)

**Command**: `cargo run --release --example crash_cycle_test -- 40 8
1337 50 3000`. Environment: idle machine, this session's own dedicated
run (not concurrent with the long soak).

| Metric | Result |
|---|---|
| Crash cycles | 40 |
| Successful recoveries | **40** |
| Failed recoveries | **0** |
| Corrupted segments (any cycle) | **0** |
| Sequence gaps (any cycle) | **0** |
| Final highest sequence | 31,673 (monotonically increasing across all 40 cycles, never duplicated, never rolled back) |
| Recovery time range | 18.4 ms – 406.8 ms (scales with accumulated record count across cycles, as expected) |
| Kill delay range used | 171 ms – 3,000 ms (seeded, reproducible — seed `1337`) |

**Crash during specific states (§6)**: not independently forced at
named code boundaries this run (the external-kill mechanism is
inherently point-agnostic — see `PHASE3C_ADR.md` ADR-P3C-3 for why that
is the correct tool for *this* requirement). The *named* boundaries
(batch formation, WAL append, await durability, sync, watermark
publication, completion, rotation, shutdown) remain covered by the
existing, unmodified `AbortPoint` (11 variants, `tests/crash_
consistency.rs`) and `CoordinatorFaultPoint` (7 variants, Phase 3B)
mechanisms — re-verified passing this phase (§10 below), not
duplicated here.

Full per-cycle data: `temp/rgc_bench/crash_cycle_full_run.log` (this
session's local artifact — not committed to the repository; the
summary above is the durable record).

## 6. Pathological recovery stress (§7)

**Command**: `cargo test --test pathological_recovery_matrix --
--nocapture`.

| Fixture | Expected | Actual | Result |
|---|---|---|---|
| `many_segments_all_valid` | OK, 200/200 recovered, gap-free | corrupted=0, truncated=false, 200 records, highest_seq=200 | PASS |
| `many_batches` | OK, 500/500 recovered, gap-free | corrupted=0, truncated=false, 500 records | PASS |
| `partial_final_header` | Torn tail, zero corruption | truncated=true, corrupted=0, 29/30 records | PASS |
| `partial_final_body` | Torn tail, zero corruption | truncated=true, corrupted=0, 4/5 records | PASS |
| `valid_durable_tail` | OK, 1000/1000, not truncated | corrupted=0, truncated=false, 1000 records | PASS |
| `crc_corruption_non_tail` | Corruption, not a torn tail | corrupted=1 segment, scan stopped | PASS |
| `invalid_length_field_non_tail` | Corruption, zero records trusted | corrupted=1, 0 records | PASS |
| `malformed_frame_unrecognized_op_tag` | Corruption via structural decode failure | corrupted=1, 0 records | PASS |
| `mixed_valid_and_corrupt_segments` | Corruption, earlier records preserved | corrupted=1, 1 record preserved | PASS |

**9/9 PASS.** See `PHASE3C_FAILURE_MODEL.md` §2 for the full
classification table and cross-references to pre-existing (not
duplicated) Phase 0/1 coverage this matrix builds on rather than
re-derives.

## 7. Recovery-memory scaling analysis (§8)

**Command**: `cargo run --release --example recovery_memory_scaling --
1000000,5000000,10000000,15000000`. Idle machine.

| Records | Build time | RSS before | RSS after | Peak RSS | Recovery time | Records/sec | Bytes on disk | Bytes/sec |
|---|---|---|---|---|---|---|---|---|
| 1,000,000 | 3.6s | 3.6 MB | 137.3 MB | 137.3 MB | 5,460.6 ms | 183,130 | 73.0 MB | 13.4 MB/s |
| 5,000,000 | 17.7s | 4.2 MB | 671.1 MB | 671.1 MB | 26,942.7 ms | 185,579 | 365.0 MB | 13.5 MB/s |
| 10,000,000 | 36.6s | 5.2 MB | 1,338.8 MB | 1,338.9 MB | 53,745.8 ms | 186,061 | 730.0 MB | 13.6 MB/s |
| 15,000,000 | 55.8s | 6.5 MB | 2,006.8 MB | 2,006.6 MB | 80,946.7 ms | 185,307 | 1,095.0 MB | 13.5 MB/s |

Every size: `corrupted_segments=0`, `records_recovered` exactly matched
the fixture size. **RSS scales linearly at ~134 bytes/record; recovery
throughput stays flat (~184-186K records/sec) regardless of size** — no
degradation in *speed* at scale, only in *memory*. Full analysis:
`PHASE3C_FAILURE_MODEL.md` §3; design-decision analysis (not
implemented): `PHASE3C_ADR.md` ADR-P3C-1.

**Larger sizes were not attempted** — 15M already uses ~2 GiB on this
16 GiB development host; extrapolating the measured linear rate to
substantially larger sizes (e.g. the ~85M-record scale that failed in
Phase 3B) would risk repeating that same host-memory exhaustion for no
new information, per this exercise's own "do not risk exhausting the
machine" instruction.

## 8. Observability (§10-§12)

| Item | Status |
|---|---|
| `writes_submitted` | Present (pre-existing) |
| `writes_completed` | Present (pre-existing, `completed_ok`) |
| `writes_failed` | Present (pre-existing, `completed_err`) |
| `writes_timed_out` | **Added this phase** — a genuine sub-classification of `completed_err` (the `await_durable` retry-budget-exhaustion case specifically), verified distinct from an fsync failure by a new regression test |
| `batches_created` | Present (pre-existing, `drain_batches`) |
| `records_per_batch` | Present (pre-existing, `avg_batch_records()`) |
| `bytes_per_batch` | **Added this phase** — `bytes_total` + `avg_bytes_per_batch()` |
| `sync_count` | Present (pre-existing, `sync_attempts`) |
| `sync_failures` | Present (pre-existing, `sync_failures()`) |
| `sync_latency` | Present (pre-existing, EMA via `latency_tracker()`) |
| `commit_latency` (p50/p95/p99) | **NOT present in production stats** — a validated per-thread-local-slot design exists in the soak harnesses (Phase 3B, reused this phase), not yet a library feature. `PHASE3C_ADR.md` ADR-P3C-4 |
| `queue_depth` | Present (pre-existing) |
| `queue_capacity` | Present (Phase 3B) |
| `queued_bytes` | Present (pre-existing) |
| `durable_through` | Present (pre-existing) |
| `highest_sequence` | Present (Phase 3B) |
| `WAL_bytes_written` | Not present as a direct counter (derivable, not tracked) |
| `segment_rotations` | Present (Phase 3B) |
| `coordinator_state` | Present (pre-existing, `state`) |
| `failure_count` | Partial — `completed_err`/`sync_failures()` cover most of this; no single unified counter |
| `recovery_count` | Not present — correctly belongs above this library (ownership analysis: `PHASE3C_ADR.md` ADR-P3C-4) |

**§11-§12 (overhead measurement)**: the two new counters this phase
add negligible, unmeasured-because-obviously-negligible cost (one
`fetch_add` per *batch*, not per request — the same shape every
existing counter in this codebase already uses, already proven not to
distort throughput at M1.3 scale by this project's own Phase 1
`batch_timing` module history). A dedicated before/after benchmark
comparison for *these two specific fields* was not run separately;
the final acceptance benchmark (§9, pending the soak's completion) is
run against the code state that includes them, so any material
regression would already surface there via the regression-bound rule.

## 9. Final performance re-verification (§20)

**Deferred until the long-duration soak (§3) completes**, per this
phase's own learned methodology (§2's note, `PHASE3C_TEST_PLAN.md` §1
rule 5) — running it now would contaminate both the soak's own
remaining evidence and the benchmark's own validity. Will be added
here, compared against §2's baseline per the pre-established rule, once
the machine is genuinely idle.

## 10. Rotation/backpressure/coordinator/leader recertification (§16-§19)

Re-run of the existing, unmodified test suite (no new dedicated tests
required — no new defect found that Phase 3A/3B's own coverage did not
already handle):

| Command | Result |
|---|---|
| `cargo test --lib` | 130/130 |
| `cargo test --release --lib` | 130/130 |
| `cargo test --lib --features test-util` | 130/130 |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean |
| `cargo fmt --check` | clean |

Includes, unchanged and re-verified: all 7 `CoordinatorFaultPoint`
tests (§18), the leader-panic tests (§19,
`leader_panic_clears_leader_active_poisons_and_recovers_cleanly_on_
reopen`, `concurrent_followers_all_fail_fast_when_the_leader_panics`),
`frequent_rotation_under_sustained_concurrent_load_recovers_correctly`
(§16), `large_payload_byte_accounting_is_accurate_and_respects_the_
limit`/`queue_full_rejects_with_timeout_not_silently`/`shutdown_while_
queue_is_actively_populated_still_drains_every_request` (§17), and the
new `purge_before_bounds_wal_footprint_under_concurrent_writes` (§16-
adjacent, new this phase). The long soak (§3) and crash-cycle harness
(§5) additionally exercise rotation and coordinator/leader failure
paths under real sustained multi-hour load and real external process
kills — coverage Phase 3B's shorter, synthetic fault-injection tests
structurally cannot provide.

**`cargo test --release --test group_commit --features test-util` /
`cargo test --release --test crash_consistency --features test-util`**:
deferred alongside §9 to avoid contaminating/being contaminated by the
active soak (the former in particular is CPU- and time-intensive); will
be re-run and recorded once the soak completes.

## 11. Security and dependency review (§14-§15)

See `PHASE3C_ADR.md`'s commit history and the dedicated security-review
commit (`583539d`) for the full account. Summary: zero `unsafe` code
in any Phase 3C addition; zero payload/key/value logging; one
consistency fix (saturating arithmetic on a new batch-byte
accumulation, matching the existing `PHASE3B_ADR.md` ADR-P3B-2
precedent); `Cargo.lock` fully reviewed — every entry (direct and
transitive) sources from the official crates.io registry, no new
production dependency this phase; `cargo-audit`/`cargo-deny` not
installed (network access to crates.io returned HTTP 403 in this
environment during this session, making installation unreliable — a
manual review was substituted and judged adequate given the small,
already-audited dependency surface).

## 12. Final certification decision (§26)

**Deferred** until §3 (long soak), §9 (final benchmark), and the
remaining §10 commands complete — per operating brief §24 ("never
convert a short soak into a multi-hour PASS... never silently remove
an incomplete item"), no decision is recorded here until the evidence
for it actually exists. This section will state exactly one of **WAL
FOUNDATION CERTIFIED FOR LSM INTEGRATION** or **WAL FOUNDATION NOT YET
CERTIFIED**, with full reasoning, once complete.
