# RubiXDB Phase 3C — Test Plan

## 1. Regression-bound rule (operating brief §20 — established BEFORE the final acceptance benchmark run)

Extends `PHASE3B_TEST_PLAN.md` §1 with two more phases of historical
data now available:

| Source | 100w range | 1000w range |
|---|---|---|
| `PHASE2B_FINAL_TEST_RESULTS.md` §21 (5 reps) | 15,777–17,940 | 90,999–95,769 |
| `PHASE3_PERFORMANCE.md` (pre/post-3A, 10 reps) | 13,736–18,487 | 90,728–97,555 |
| `PHASE3B_TEST_RESULTS.md` §2 baseline (3 reps) | 17,229–18,151 | 92,183–92,987 |
| `PHASE3B_TEST_RESULTS.md` §9 final (3 reps) | 15,800–17,621 | 90,309–91,512 |
| `PHASE3C_TEST_RESULTS.md` §2 baseline (3 reps, this phase) | 15,051–18,667 | 89,157–94,327 |

**Cumulative historical band**: 100 writers **13,700–18,700** ops/sec;
1,000 writers **89,000–97,600** ops/sec.

**Rule, fixed before this session's final acceptance run:**

1. A newly-measured median is **normal machine noise, not a
   regression**, if it falls within the cumulative historical band
   above **and** is not consistently reproduced as low across ≥3
   independent repetitions.
2. A newly-measured median is a **real regression** only if it falls
   *below* that band **and** is reproducible (consistently low) across
   ≥3 independent repetitions — at which point the cause is
   investigated and reported, not dismissed.
3. **Absolute floor, independent of the above**: the original Phase 2B
   targets (100w ≥15,000 ops/sec, 1000w ≥80,000 ops/sec) remain the
   hard acceptance criterion.
4. Same machine, storage, power mode, and release-profile binary as
   every prior phase's own numbers (unchanged throughout this
   project's history).
5. **New this phase, learned the hard way** (`PHASE3C_TEST_RESULTS.md`
   §2's own note): the acceptance benchmark must run on an otherwise-
   idle machine — not concurrently with a background soak or other
   CPU-intensive session activity. A benchmark run contaminated by
   concurrent load is discarded, not reported as evidence, and re-run
   once the machine is actually idle.

## 2. What is tested this increment

1. **Baseline freeze** (§1): `git status`/`rev-parse`/`branch`/`log`,
   re-run of the winning architecture at 100 and 1,000 writers with
   full stats.
2. **Regression gate** (§2): `cargo test`/`--release`/`--features
   test-util`/clippy/fmt, plus `tests/group_commit`/`tests/crash_
   consistency`, run before Phase 3C changes and again at the end.
3. **True long-duration soak** (§3-§4): `examples/long_soak_test.rs`,
   100 and 1,000 writers, 4 hours each (operating brief's own stated
   minimum), with periodic checkpointing (`purge_before`) so the live
   WAL stays within the proven-safe recovery scale for the run's
   entire duration (`PHASE3C_ADR.md` ADR-P3C-2).
4. **Periodic forced-crash-during-soak** (§5-§6): `examples/crash_
   cycle_test.rs`, external `Child::kill()` at randomized (seeded,
   reproducible) delays, many cycles against one accumulating WAL
   directory, full recovery verification after every cycle.
5. **Pathological recovery stress** (§7): `tests/pathological_
   recovery_matrix.rs`, 9 fixtures against the existing recovery
   contract.
6. **Recovery-memory analysis** (§8): `examples/recovery_memory_
   scaling.rs`, 1M/5M/10M/15M-record sweep.
7. **Recovery API design decision** (§9): analysis only, no
   implementation — `PHASE3C_ADR.md` ADR-P3C-1.
8. **Observability** (§10-§12): two new safe additions (`bytes_total`/
   `avg_bytes_per_batch`, `writes_timed_out`), audited against the full
   requested list, gaps recorded explicitly.
9. **Logging audit** (§13): no payload/key/value logging in any new
   Phase 3C code — verified by direct inspection.
10. **Dependency/security review** (§14-§15): manual `Cargo.lock`
    review (no new production dependency this phase); `cargo-audit`/
    `cargo-deny` not installed (network access to crates.io returned
    403 in this environment, making installation unreliable) — decision
    documented, not silently skipped.
11. **Rotation/backpressure/coordinator/leader recertification**
    (§16-§19): re-confirmed via the existing, unmodified Phase 3A/3B
    test suite passing unchanged (130/130) — no new correctness defect
    found that would require new dedicated tests beyond what Phase 3B
    already built; the long soak and crash-cycle harness additionally
    exercise rotation and coordinator/leader failure paths under
    real sustained load and real external kills, which Phase 3B's own
    shorter, synthetic fault-injection tests could not.
12. **Final performance re-verification** (§20): re-run the frozen
    baseline benchmark after all Phase 3C changes, on an idle machine
    (see §1 rule 5), compare per the rule above.

## 3. What is explicitly NOT built/run this increment, or is a known gap

- **A production percentile (`p50`/`p95`/`p99`) `commit_latency` metric
  wired into the library itself** — the per-thread-local-slot design is
  validated (in the soak harnesses) but not yet a library feature.
  `PHASE3C_ADR.md` ADR-P3C-4.
- **`writes_timed_out`'s scope** — only the `await_durable` retry-
  budget-exhaustion case is counted; submission-time backpressure
  timeouts remain `rejected_backpressure` (a pre-existing, separate
  counter), not folded in.
- **`recovery_count`, `WAL_bytes_written` as a direct counter** — see
  `PHASE3C_ADR.md` ADR-P3C-4's ownership-layer note for the former;
  the latter is derivable but not tracked as an explicit counter.
- **A streaming/bounded-memory recovery API** — analyzed (ADR-P3C-1),
  not implemented, per the operating brief's own explicit instruction.
- **`cargo-audit`/`cargo-deny`** — not installed this session.

## 4. Reproduction

```
cargo test --lib
cargo test --release --lib
cargo test --lib --features test-util
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --release --test group_commit --features test-util
cargo test --release --test crash_consistency --features test-util
cargo test --test pathological_recovery_matrix -- --nocapture
cargo run --release --example batch_coordinator_load_test -- 100 1000
cargo run --release --example batch_coordinator_load_test -- 1000 1000
cargo run --release --example long_soak_test -- 100 14400 120 5000000 120
cargo run --release --example long_soak_test -- 1000 14400 120 5000000 120
cargo run --release --example crash_cycle_test -- 40 8 1337 50 3000
cargo run --release --example recovery_memory_scaling -- 1000000,5000000,10000000,15000000
```
