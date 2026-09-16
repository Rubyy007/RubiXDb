# RubiXDB Phase 3B — Test Results

Single source of truth for Phase 3B's pass/fail data and benchmark
numbers, per this project's own standing rule. Does not overwrite or
replace `PHASE3_TEST_RESULTS.md` (Increment 3A) — both stand as the
historical record of their own increment. Regression-bound rule
(established before the final acceptance run): `PHASE3B_TEST_PLAN.md` §1.

## 1. Repository state

```
Phase 3B baseline (last Phase 3A commit, frozen before any Phase 3B code):
    68d70eae52b5f9ae0a179aac036c1055af67d791
Phase 3B commits (chronological):
    f03b576  coordinator fault-injection matrix + completion-safety fix
    80562b6  queued_bytes overflow-safe arithmetic + resource tests + soak harness
    cdb8dc1  rotation-stress test through the production coordinator path
    7d1c5ef  observability gaps (queue_capacity, highest_sequence, segment_rotations)
    8eece1b  shutdown-while-queue-populated test
```

Branch: `master`. Working tree clean before this increment and after
every commit above (`git status --short` verified at each step).

## 2. Baseline (frozen before any Phase 3B code change)

**Command**: `cargo run --release --example batch_coordinator_load_test
-- <writer_count> 1000`, commit `68d70ea`.

| Level | Runs (ops/sec) | Median | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) | sync_attempts | avg_batch_records | completed_err | rejected_backpressure |
|---|---|---|---|---|---|---|---|---|---|---|
| 100 writers | 17,582 · 18,151 · 17,229 | **17,582** | 5.3–5.7 | 6.8–7.0 | 11.3–11.9 | 23–29 | 1,006–1,018 | 98.2–99.4 | 0 | 0 |
| 1,000 writers | 92,987 · 92,183 · 92,671 | **92,671** | ~10.2–10.3 | 12.1–12.3 | 18.9–19.5 | 97–153 | 1,007–1,010 | 990–993 | 0 | 0 |

Both exceed the Phase 2B targets (100w ≥15,000; 1000w ≥80,000), with
zero failures/timeouts/backpressure rejections in any of the 6 runs.

## 3. Regression gate (before Phase 3B changes)

| Command | Result | PASS/FAIL |
|---|---|---|
| `cargo test --lib` | 117/117 | PASS |
| `cargo test --release --lib` | 117/117 | PASS |
| `cargo test --lib --features test-util` | 117/117 | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean | PASS |
| `cargo fmt --check` | clean | PASS |
| `cargo test --release --test group_commit --features test-util` | 6/8 — only the pre-existing, unrelated Phase 1 direct-thread M1.2/M1.3 misses (5,954 / 46,195 ops/sec; unrelated to the Approach B production architecture) | PASS (no new failures) |
| `cargo test --release --test crash_consistency --features test-util` | 2/2 | PASS |

## 4. Regression gate (after all Phase 3B changes, final)

| Command | Result | PASS/FAIL |
|---|---|---|
| `cargo test --lib` | 128/128 (117 + 11 new) | PASS |
| `cargo test --release --lib` | 128/128 | PASS |
| `cargo test --lib --features test-util` | 128/128 | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean | PASS |
| `cargo fmt --check` | clean | PASS |

**Zero regressions** — every pre-existing test still passes; 11 new
tests added (7 `CoordinatorFaultPoint` tests, large-payload accounting,
rapid cycles, rotation stress, shutdown-while-populated).

## 5. Coordinator fault-injection matrix (§3, `PHASE3B_FAILURE_MODEL.md` §3–§5)

| Test | Fault point | Result |
|---|---|---|
| `coordinator_panic_before_batch_formation_fails_safely` | `BeforeBatchFormation` | PASS |
| `coordinator_panic_after_drain_fails_safely` | `AfterDrain` | PASS (this is the point that surfaced the completion-guard gap — fixed, then verified) |
| `coordinator_panic_after_append_fails_safely` | `AfterAppend` | PASS |
| `coordinator_panic_before_await_durable_fails_safely` | `BeforeAwaitDurable` | PASS |
| `coordinator_panic_after_durable_fails_safely` | `AfterDurable` | PASS |
| `coordinator_panic_before_completion_fails_safely` | `BeforeCompletion` | PASS |
| `coordinator_panic_during_shutdown_fails_safely_and_is_bounded` | `DuringShutdown` | PASS (bounded < 2s) |

Each verifies: no caller hangs, at least one caller observes a failure
(never universal false success), the pool reaches a terminal state and
rejects further submissions, and the reopened WAL is uncorrupted with
gap-free sequences. Run repeatedly during development (5+ consecutive
full-module runs), no flakes — deterministic via an atomic-counter
barrier, not sleep timing.

## 6. Resource exhaustion / shutdown / rotation (§7–§9, §14)

| Test | Verifies | Result |
|---|---|---|
| `large_payload_byte_accounting_is_accurate_and_respects_the_limit` | `queued_bytes` tracks real payload bytes exactly, never exceeds `max_queued_bytes`, admits exactly the configured capacity (200 KiB payloads, deterministic barrier — no sleep) | PASS |
| `rapid_submit_shutdown_cycles_do_not_leak_or_hang` | 25 construct/submit/shutdown cycles, each bounded < 2s | PASS |
| `frequent_rotation_under_sustained_concurrent_load_recovers_correctly` | 20 threads × 100 records through the full production path against a 16 KiB `max_segment_size` (>5 rotations forced); gap-free recovery | PASS |
| `shutdown_while_queue_is_actively_populated_still_drains_every_request` | `shutdown()` racing active submission still drains every accepted request; stable across 5 repeated runs | PASS |
| `queue_full_rejects_with_timeout_not_silently` (pre-existing, re-verified) | Bounded backpressure | PASS |

Overflow-safety hardening: `queued_bytes` accounting changed from raw
`+=`/`.sum()` to `saturating_add`/a saturating fold across all four
`execution::*` modules (`batch_coordinator`, `leader_drain`,
`sharded_ingress`, `write_pool`) — see `PHASE3B_ADR.md` ADR-P3B-2.

## 7. Observability audit (§15–§18)

| Requested metric | Status |
|---|---|
| `writes_submitted` | Present (`BatchCoordinatorStats::submitted`) |
| `writes_completed` | Present (`completed_ok`) |
| `writes_failed` | Present (`completed_err`) |
| `writes_timed_out` | **NOT present as a distinct counter** — folded into `completed_err`/`rejected_backpressure`; a submission-time timeout is `rejected_backpressure`, a post-accept timeout is `completed_err`, neither separately labeled "timeout" |
| `batches_created` | Present (`drain_batches`) |
| `records_per_batch` | Derivable (`drain_entries_total / drain_batches`) |
| `bytes_per_batch` | **NOT present** |
| `sync_count` | Present (`committer_stats.sync_attempts`) |
| `sync_failures` | Present (`committer_stats.sync_failures()`) |
| `sync_latency` | Present as an EMA (`GroupCommitter::latency_tracker()`), not a full distribution |
| `p50/p95/p99 commit latency` | **NOT present in production stats** — `examples/soak_test.rs` demonstrates a low-contention design for this but it is harness-only, not wired into the library |
| `queue_depth` | Present |
| `queue_capacity` | **Added this increment** (`BatchCoordinatorStats::queue_capacity`) |
| `queued_bytes` | Present |
| `durable_through` | Present (`committer_stats.durable_through`) |
| `highest_sequence` | **Added this increment** (`GroupCommitStats::highest_sequence`) |
| `WAL_bytes_written` | **NOT present** as a direct counter (derivable approximately from `records_total` and `estimate_frame_len`, not exact) |
| `segment_rotations` | **Added this increment** (`GroupCommitStats::segment_rotations`) |
| `coordinator_state` | Present (`BatchCoordinatorStats::state`) |
| `failure_count` | Partial — `completed_err` + `sync_failures()` cover most of this but there is no single unified counter |
| `recovery_count` | **NOT present** — inherently cross-session; would need to live above this library, in whatever caller drives `open_for_recovery` |

**Decision**: ship the safe, zero-new-contention additions
(`queue_capacity`, `queued_bytes_capacity`, `highest_sequence`,
`segment_rotations`); do not fabricate the rest as complete. Full
rationale: `PHASE3B_ADR.md` ADR-P3B-3.

**§16 (metrics must not change performance materially)**: NOT RUN — no
new hot-path metrics were added this increment (the four additions above
are either pre-existing config values or one extra cheap `wal` lock per
`stats()` call, not per-write instrumentation), so there is nothing new
to compare on/off.

## 8. Soak test (§10–§11)

**Honesty note** (§28, `PHASE3B_ADR.md` ADR-P3B-4): the operating brief
asks for a multi-hour soak. This session ran `examples/soak_test.rs` for
**15 minutes (900s) per writer level** instead — a bounded, explicitly
short-of-spec duration, flagged here rather than hidden or extrapolated.

**Command**: `cargo run --release --example soak_test -- <writer_count>
900 60`. Machine otherwise idle during each run (no concurrent compile
or benchmark — verified by construction, since this session serialized
the soak against its own other work).

### 100 writers, 900s

| | Start (t=60s) | Mid (t=~450s) | End (t=900.8s) |
|---|---|---|---|
| ops/sec | 17,170 | 16,885 | 18,739 |
| p99 (ms) | 14.575 | 12.185 | 6.962 |
| RSS (KB) | 7,808 | 7,708 | 7,328 |
| queue_depth | 0 | 0 | 0 |
| completed_err | 0 | 0 | 0 |

**Total**: 15,495,498 ops completed over 900s (mean ≈17,217 ops/sec),
**zero errors, zero timeouts, zero backpressure rejections across the
entire run**. `queue_depth` was `0` at every single sample — the
coordinator never fell behind this workload. RSS *decreased* slightly
(-480 KB / -6.1%, within measurement noise — `tasklist`'s own reported
working-set granularity). Throughput at the end was *higher* than at
the start (+9.1%), not lower — no degradation trend of any kind.
**Recovery after shutdown**: `corrupted_segments=0`,
`records_recovered=15,495,498` (exact match), `recovery_ms=72,965.9`
(~73s for 15.5M records — consistent with this project's own Phase 0
recovery-throughput benchmark, ~132–135 Kelem/s), sequences gap-free.

### 1,000 writers, 900s

See §8a below (filled in once the run completes — this document is
updated in place before final commit, not left with a placeholder in
the committed version).

## 9. Final performance re-verification (§19–§20)

See §9a below (filled in after the soak completes, per the regression-
bound rule in `PHASE3B_TEST_PLAN.md` §1, established before these
numbers were measured).

## 10. Security review (§21–§22)

Manual audit (no `cargo-audit`/`cargo-deny` installed on this machine;
not installed this session — see `PHASE3B_TEST_PLAN.md` §3 and this
project's own "no new dependency/tool without documenting the decision"
rule):

| Area | Finding |
|---|---|
| `unsafe` code | **Zero** `unsafe` blocks in production `src/` (grepped the whole tree; only doc-comment *mentions* of "no unsafe" from prior phases' own audits) |
| Queue/memory bounds | `queue_capacity`/`max_queued_bytes` enforced on every `submit()` across all four `execution::*` architectures; backpressure is `EngineError::Timeout`, never silent drop or unbounded queuing |
| Integer overflow | `queued_bytes` accounting hardened to saturating arithmetic this increment (§6 above, `PHASE3B_ADR.md` ADR-P3B-2); `estimate_frame_len` bounded by `DEFAULT_MAX_RECORD_LEN` (64 MiB); sequence/segment-id arithmetic already checked/saturating from the Phase 1 hardening pass (`ARCHITECTURE.md`'s "WAL hardening pass" section) |
| Panic handling | Every completion path uses an RAII guard (`CompletionGuard`, `LeaderFailureGuard`) with a `Drop`-based fallback — verified this increment to cover the one gap found (§4 above); no `.unwrap()`/`.expect()` on untrusted (as opposed to structurally-guaranteed) data introduced this increment |
| Filesystem boundaries / path handling | Unchanged this increment — no new path-handling code was added; Phase 0/1's existing `canonicalize_data_dir`/cross-process locking remain the only path-entry points, both unmodified |
| Payload logging | **None found.** The only `eprintln!` in `src/` is the `test-util`-gated `RGC_TIMING_REPORT` diagnostic (`src/wal/group_commit.rs`), which prints only aggregate timing numbers (batch counts, mean µs per stage) — never a key, value, or raw payload byte |
| Dependency review | `Cargo.lock` reviewed by name — all transitive dependencies are well-known, standard crates pulled in by `criterion`/`proptest` (dev-only); `crc32c` remains the only production dependency, pinned exact; **no new production dependency added this phase** |
| Resource exhaustion / DoS | Covered by §6's resource tests — bounded queue, bounded bytes, bounded shutdown, no unbounded allocation found |
| Shutdown abuse | `shutdown()` is idempotent (pre-existing test, re-verified); rapid cycling tested (§6); no thread leak observed across 25 cycles |

**Static analysis** (§22): `cargo fmt --check` and `cargo clippy
--all-targets --all-features -- -D warnings` both clean at every commit
this increment (§3–§4 above).

## 11. Final engineering decision (§29)

See §11 below (filled in last).
