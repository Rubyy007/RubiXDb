# RubiXDB Phase 3 — Test Plan

## Increment 3A (leader-failure P0 fix) — this document's current scope

### What is tested

1. **Deterministic single-threaded leader-panic test**
   (`wal::group_commit::tests::leader_panic_clears_leader_active_
   poisons_and_recovers_cleanly_on_reopen`): a record is made durable,
   then a second leader batch is armed with a panicking `fsync` fault
   hook. Asserts, in one place: the pre-panic record stays durable; the
   panicking call itself returns (via unwind) promptly; the committer
   transitions to the documented poisoned state
   (`is_poisoned() == true`); a later call on the same committer fails
   fast (bounded, < 500ms) rather than hanging or riding out a timeout;
   a fresh write submitted after the panic also fails fast, never
   silently dropped or falsely acknowledged; discarding the poisoned
   committer and reopening the WAL directory recovers exactly the
   pre-panic durable prefix, gap-free, uncorrupted; a brand-new
   `GroupCommitter` over the reopened WAL works normally.
2. **Concurrent leader-panic test**
   (`wal::group_commit::tests::concurrent_followers_all_fail_fast_
   when_the_leader_panics`): 17 concurrent callers race for leadership
   of one batch (which of them wins is a genuine runtime race — the
   test does not assume which); the fault hook blocks until all 17 have
   started before panicking, guaranteeing genuine concurrent contention.
   Asserts exactly one caller panics (whichever won), every other caller
   observes a clean poisoned-committer error (not a timeout, not a
   hang, not a false success) within 1 second, and the committer ends
   poisoned.
3. **Existing tests updated to reflect the fix, not just left stale**:
   `execution::leader_drain::tests::one_worker_panicking_fails_only_
   its_own_request_and_does_not_lose_others` and `::a_second_request_
   after_the_leader_panics_still_fails_safely_not_permanently_blocked`
   had doc comments and assertions describing the *pre-fix* behavior
   (permanently-stuck `leader_active`, ~5s shutdown cost, a second
   request failing only after riding out its own retry budget). Both
   are updated: the doc comments now describe the fix, and new timing
   assertions (`< 1 second`) lock in the improved behavior so a future
   regression back to the old behavior would be caught, not just
   silently re-documented.
4. **Full regression gate** (operating brief §31, unchanged commands):
   `cargo test`, `cargo test --release`, `cargo test --features
   test-util`, `cargo clippy --all-targets --all-features -- -D
   warnings`, `cargo fmt --check`, plus the existing `tests/group_
   commit/` integration suite and `tests/crash_consistency.rs`.
5. **Benchmark re-verification** (Approach B, the production
   architecture — not the Phase 1 direct-thread harness `tests/group_
   commit` uses for its own M1.2/M1.3, whose pre-existing, documented
   misses are unrelated to this fix and unaffected by it): 100-writer
   and 1,000-writer throughput re-measured post-fix and compared against
   a freshly re-measured pre-fix baseline. See `PHASE3_PERFORMANCE.md`.

### What is deliberately not tested this increment (see `PHASE3_FAILURE_MODEL.md` §5 for the full list)

- The full fault-injection point matrix (operating brief §8) — only
  "leader panics during `fsync`" (the scenario `PHASE2B_FAILURE_MODEL.md`
  §3 originally diagnosed) has a dedicated test. Panics injected at
  other points (before leader election, during batch formation, before/
  after snapshot, after watermark publication, before/after completion)
  are structurally covered by the same `LeaderFailureGuard` (it is armed
  for the guard's *entire* scope, not just around the `fsync` call), but
  each has not been individually exercised with its own dedicated test.
- Coordinator-level failure (distinct from leader/`GroupCommitter`-level
  failure) — `BatchCoordinatorPool`'s own panic-safety
  (`coordinator_panicking_fails_safely_and_rejects_further_work`) was
  already tested in Phase 2B and re-verified green this increment, but
  not re-examined for new coverage.
- Soak testing, repeated crash testing at controlled intervals,
  resource-exhaustion testing (operating brief §13–§17).
- Observability/metrics/logging layer (operating brief §18–§19).
- Stage B (MemTable) entirely.

### Reproduction

```
cargo test --lib wal::group_commit:: -- --nocapture
cargo test --lib execution:: -- --nocapture
cargo test --lib
cargo test --release --lib
cargo test --lib --features test-util
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --release --test crash_consistency --features test-util
cargo run --release --example batch_coordinator_load_test -- 100 1000
cargo run --release --example batch_coordinator_load_test -- 1000 1000
```
