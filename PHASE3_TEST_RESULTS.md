# RubiXDB Phase 3 — Test Results

Single source of truth for Phase 3's pass/fail data and benchmark
numbers, per this project's own standing rule (matching `PHASE1_TEST_
RESULTS.md`/`PHASE2_TEST_RESULTS.md`/`PHASE2B_FINAL_TEST_RESULTS.md`'s
role for their own phases). Design rationale: `PHASE3_ARCHITECTURE.md`;
failure semantics: `PHASE3_FAILURE_MODEL.md`; what was and wasn't
tested: `PHASE3_TEST_PLAN.md`. This file is measurements and verdicts
only.

## 1. Repository state

```
Phase 3 baseline (last Phase 2B commit, frozen before any Phase 3 code):
    7c808eb677b3b31a7e2ba4a65f123892a6610e6b
Increment 3A (leader-failure P0 fix) committed:
    efad213ffbb72cfdc267ac0e490d9cb5980714ef
```

Branch: `master`. Working tree clean before and after this increment
(verified via `git status --short`).

Hardware/environment: see `PHASE3_PERFORMANCE.md` §1 (unchanged from
every prior phase this session).

## 2. Regression gate

| Command | Result | PASS/FAIL |
|---|---|---|
| `cargo test --lib` | 117/117 (115 pre-existing + 2 new leader-panic tests) | PASS |
| `cargo test --release --lib` | 117/117 | PASS |
| `cargo test --lib --features test-util` | 117/117 | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | clean | PASS |
| `cargo fmt --check` | clean (two pre-existing, unrelated trailing-newline nits in `src/wal/mod.rs`/`src/wal/recovery.rs` fixed as a drive-by; zero logic change) | PASS |
| `cargo test --release --test group_commit --features test-util` | 6/8 — only the pre-existing, unrelated M1.2/M1.3 throughput misses (6,003 / 46,821 ops/sec — Phase 1's own direct-thread harness, not the Approach B architecture this project actually recommends; unchanged, documented gap since Phase 1). Every correctness/crash test (M1.1, M1.4, M1.5, M1.6, `watermark_monotonicity`) passes | PASS (no new failures) |
| `cargo test --release --test crash_consistency --features test-util` | 2/2 | PASS |

**Zero regressions** — every result above matches the same class of
outcome `PHASE2B_FINAL_TEST_RESULTS.md` §4 recorded for the identical
commands at the end of Phase 2B.

## 3. New tests, per `PHASE3_TEST_PLAN.md` §1–§3

| Test | Result |
|---|---|
| `wal::group_commit::tests::leader_panic_clears_leader_active_poisons_and_recovers_cleanly_on_reopen` | PASS |
| `wal::group_commit::tests::concurrent_followers_all_fail_fast_when_the_leader_panics` | PASS (17 concurrent callers; exactly 1 panics, 16 fail cleanly within 1s, verified every run) |
| `execution::leader_drain::tests::one_worker_panicking_fails_only_its_own_request_and_does_not_lose_others` (updated: now asserts shutdown returns in < 1s, was previously documented as costing ~5s) | PASS |
| `execution::leader_drain::tests::a_second_request_after_the_leader_panics_still_fails_safely_not_permanently_blocked` (updated: now asserts the second request fails in < 1s, was previously allowed the full `await_retry_budget`) | PASS |
| `execution::batch_coordinator::tests::coordinator_panicking_fails_safely_and_rejects_further_work` (unchanged assertions — re-verified green, now backed by the fixed `GroupCommitter`) | PASS |
| `execution::sharded_ingress::tests::coordinator_panicking_fails_safely_and_rejects_further_work` (unchanged assertions — re-verified green) | PASS |

Each of these two panic tests (item 1–2) was run repeatedly (5+
consecutive `cargo test` invocations of the full `wal::group_commit::`
module during development) with no flakes — per operating brief §9
("It must not depend on random timing alone"), determinism comes from
the fault hook blocking on an atomic counter until every concurrent
caller has actually started, not from sleep-based timing.

## 4. Benchmarks

See `PHASE3_PERFORMANCE.md` for the full table. Summary: both the
100-writer (≥15,000) and 1,000-writer (≥80,000) Phase 2B targets remain
met after this increment (100w: 15,329 median, +2.2% over target; 1000w:
93,733 median, +17.2% over target), using the identical Approach B
(Dedicated Batch Coordinator) architecture this project already
recommends — this increment did not change which architecture is used,
only hardened `GroupCommitter`'s failure behavior underneath it.

## 5. Recovery verification (operating brief §16, scoped to this increment's own failure mode)

Performed as part of the leader-panic test itself (§3, test 1), not as
a separate exercise this increment:

| Check | Result |
|---|---|
| Expected WAL record count after reopen | Matches (pre-panic durable prefix present) |
| Highest recovered sequence | Gap-free, sequential from 1 |
| Acknowledged durable prefix | Exactly the one `append_durable`-confirmed record before the panic, verified byte-for-byte (`WalOpOwned::Put { key: b"before-panic", value: b"v1" }`) |
| Torn-tail / corruption detection | `replay.corrupted_segments.is_empty()` — asserted true |
| Segment continuity | Single segment, no rotation involved in this test — rotation-specific crash recovery is unchanged, pre-existing coverage (`tests/group_commit/rotation_mid_batch.rs`, M1.5) |

Full operating-brief-scale recovery validation (§15–§17: repeated
process-level crash testing at controlled intervals, resource
exhaustion) is explicitly out of this increment's scope — see
`PHASE3_FAILURE_MODEL.md` §5.

## 6. Scope note

This document covers **Increment 3A only** (the P0 leader-failure fix).
It will be extended, not replaced, as further Phase 3 increments land —
matching `PHASE2B_FINAL_TEST_RESULTS.md`'s own precedent of being the
single authoritative results file for its whole phase, added to over
multiple commits within that phase.
