# Realistic Full-Pipeline Soak — ENOSPC Failure Analysis

**Status: BLOCKING FINDING. The write engine is NOT certified by this run.**

**Run analyzed:** `realistic_soak_200w_20260919_085109` (200 writers, `LsmConfig::default()`,
14,400s target duration, `cargo run --release --example realistic_full_pipeline_soak -- 200 14400 120`)

**Evidence preserved, unmodified, at:**
- `temp/long_soak_logs/realistic_soak_200w_20260919_085109.log` (harness stdout: per-sample CSV + final summary lines)
- `temp/long_soak_logs/realistic_soak_200w_20260919_085109.log.stderr` (4,409 lines, 4,407 of them `flush attempt N failed ... os error 112`)
- `temp/long_soak_logs/realistic_soak_result_20260919_085109.txt` (harness-level start/exit/PASS record)
- `temp/realistic_soak_harness.ps1` (the PowerShell driver that produced the above)

All three are untracked and under `temp/` (gitignored) — not touched or overwritten by this
analysis.

---

## 1. Verdict

The harness recorded `REALISTIC FULL-PIPELINE SOAK RESULT: PASS` solely because `exit_code == 0`
and the process tree was clean on exit. That is **not** a valid production-soak pass. The run
exhausted the target disk volume roughly 36% of the way through the 14,400s duration and never
recovered meaningful throughput for the remaining ~9,227s (64% of the run). This is documented
here as a genuine finding, not silently resolved — see [[project_rubixdb_phase3]]-style rigor
conventions this repo follows.

**Two separable findings, not one:**

1. **Test-fixture defect (likely primary cause of the disk filling at all):** the harness calls
   `std::env::temp_dir()` (`examples/realistic_full_pipeline_soak.rs:121`), which resolves to the
   Windows default `%TEMP%` on this machine's `C:` drive. This project's own prior measurements
   (`FINAL_WAL_ANALYSIS.md` §5, `PHASE1_TEST_RESULTS.md` §9C.4) already document this exact `C:`
   drive as chronically **97% full** with only ~2.9 GB free — a known, previously-diagnosed
   condition, not new. A 200-writer, 4-hour, 4 MiB-memtable soak at ~25k ops/sec was never going
   to fit in ~2.9 GB regardless of engine correctness (see §7, storage budget). This does not
   excuse finding 2 below, but it is the reason ENOSPC was hit *this soon* on *this run*.
2. **Real production defect, independent of why the disk filled:** once ENOSPC starts, the
   background flush thread's retry loop is **unbounded** and the write-facing API applies **no
   backpressure or backoff** on repeated failure. A production disk-exhaustion event — which can
   happen on a correctly-provisioned volume too, from a runaway neighbor process, log growth, or
   underestimated capacity — would reproduce the same CPU/logging storm and the same effective
   write-availability collapse documented below. This is the blocking part; see §5-§6 for the
   code-level root cause and PHASE5_ENOSPC recommendations.

---

## 2. Timeline

All times are `t_secs` from harness start (`08:51:09.528 IST`, see `.log` line 1).

| t_secs | Event |
|---|---|
| 0 – 4,812.5 | Healthy steady state. Throughput 19,585–27,496 ops/sec (mean ≈25,400 across the 40 samples in this window). `completed_err = 0` throughout. SSTable count grows linearly, ~30-33 tables per 120s sample. RSS climbs linearly 30,284 KB → 244,960 KB (peak — see §4). |
| 4,932.8 – 5,053.1 | First visible strain: throughput drops 26,092 → 23,770 ops/sec; **RSS drops sharply, 244,960 KB → 209,640 KB → 215,008 KB** — the first sign flush is starting to fall behind and immutable memtables/allocations are being churned differently than steady state. Still `completed_err = 0`. |
| **5,053.1 → 5,173.5** | **Failure window.** `completed_err` goes from 0 to 3,453,722 within this single 120.3s sampling interval. Throughput collapses 23,770 → 12,565 ops/sec in the same interval. SSTable count stalls at 1,308 (last increment before the stall: 1,293→1,308 in the *previous* interval). This is the first sampling interval in which the flush thread is provably stuck on ENOSPC — see §3 for why sub-interval precision isn't available. |
| 5,173.5 – 7,099.4 | Throughput collapses further: 12,565 → 15 → 8 → 1 → ... → 0 ops/sec, staying at or near 0 for most of this ~1,926s window. `completed_err` climbs from 3.45M to 127.3M. SSTable count frozen at 1,308 the entire window (zero successful flushes for ~1,926s). `checkpoint_seq` frozen at 126,948,836 the entire window (WAL cannot advance its durability watermark because the flush thread that would call `purge_before`/set the new checkpoint is wedged in the ENOSPC retry loop). |
| 7,099.4 – 14,400.4 | Intermittent partial recovery: brief bursts of successful flush activity (e.g. t=7219.7 ops/sec=765, t=8784.5 ops/sec=5,183, t=8904.9 ops/sec=6,872, t=11311.7-11432.1 ops/sec up to 21,132) alternate with multi-sample stretches back at 0 ops/sec. SSTable count creeps 1,308→1,401 (93 more tables in 9,227s, vs. ~30 tables per single 120s sample pre-failure — roughly a 280x slowdown). This pattern (short recovery, relapse) is consistent with `purge_before` intermittently freeing just enough space for one more flush cycle before the disk fills again. |
| 14,400.4 | Duration elapsed. `stop` flag set, writer threads joined. Final counters recorded. |
| post-run | `engine.shutdown()` → `pool_state=Stopped fully_drained=true`. Engine dropped, WAL lock released, directory reopened fresh. **Recovery succeeded**: `recovery_ms=5582.4`, `wal_records_visited=364518`, `wal_records_applied=272285`, `1401` live SSTables reconciled, `2802` manifest records replayed, zero corruption. Directory then deleted by the harness's own cleanup (`fs::remove_dir_all`) since recovery hit the `Ok` branch — this is why no raw on-disk artifacts (SSTables/WAL/Manifest files) survive for deeper forensic inspection beyond what the run emitted to stdout/stderr. |

**First ENOSPC occurrence — precision caveat:** `stderr` lines (`flush attempt N failed ...`) are
not individually timestamped; only the 120s-interval stdout samples are. The failure is bounded to
the interval `(5,053.1s, 5,173.5s]` (≈10:16:59–10:18:59 IST) by the `completed_err`/throughput/
SSTable-count evidence above. Finer precision was not captured by this harness — flagged as an
observability gap, not guessed at (see §8).

---

## 3. The counter "mismatch" — resolved, not a bug

`completed_err` (580,190,298) vastly exceeds the local writer-loop counter
(`total_ops_completed_via_local_counter=136,011,200`, which only increments on `Ok` results). This
looked like a possible accounting bug. It is not — traced to source and verified arithmetically:

- **`submitted`** (`PoolShared.submitted`, `batch_coordinator.rs:461`): incremented exactly once
  per `BatchCoordinatorPool::submit()` call. `LsmEngine::put`/`delete` (`lsm/mod.rs:407-427`) call
  `submit()` exactly once per logical call, with **no internal retry** — a failed `put`/`delete`
  returns `Err` immediately to the caller.
- **`completed_ok`** (`batch_coordinator.rs:783`): incremented once per entry whose batch append +
  await-durable both succeeded.
- **`completed_err`** (`batch_coordinator.rs:736,748,786`): incremented once per entry that failed
  at *either* the WAL-append step or the await-durable step of `process_batch`. **One increment
  per failed logical `put`/`delete` call — not per retry.** (`process_batch` itself has no retry
  loop of its own around `committer.append`; `await_durable_retrying` retries only `Timeout`
  outcomes, and that retry only resolves once, at the very end, into a single completed_ok/err
  classification per entry — it does not multiply the counter.)
- **Verification:** `submitted (716,203,099) − completed_ok (136,012,601) − completed_err
  (580,190,298) = 200`, exactly `writer_count`. This is the expected in-flight count: each of the
  200 writer threads runs synchronously (`submit` → `wait` → apply, one outstanding request at a
  time), so at any sampling instant at most `writer_count` submissions can be counted as
  "submitted" but not yet resolved into `completed_ok`/`completed_err`. This is the *same* pattern
  already documented and trusted in `PHASE_WRITE_ENGINE_TEST_RESULTS.md` §12 for the two clean
  4-hour legs (`submitted − completed_ok` held at ~100/~1000, matching writer count there too) —
  this run is the first time `completed_err` has ever been nonzero, so the identity had to be
  restated as `submitted − completed_ok − completed_err = writer_count` to still hold, and it does,
  exactly.

**Conclusion: `completed_err` counts logical requests 1:1, with unambiguous semantics. No counter
accounting bug exists.** The 580M figure is real: it is the count of distinct `put`/`delete` calls
that failed, accumulated because (a) the writer-thread loop in the test harness itself
(`examples/realistic_full_pipeline_soak.rs:231-255`) applies **no backoff on error** — a failed
call is immediately followed by the next call with a new key — and (b) once the disk is fully
saturated, `committer.append`'s WAL write also starts failing with the same `os error 112`, which
fails *fast* (no I/O wait, just an OS-level rejection), so 200 threads in a tight
submit-fail-resubmit loop can accumulate hundreds of millions of failures in a few hours. This is
itself evidence for §6 (retry policy) and §7 (backpressure): the write engine currently gives a
caller no signal to slow down or stop retrying once storage is exhausted, so a caller that doesn't
self-throttle (like this test's own writer loop) will hammer it indefinitely.

---

## 4. Resource metrics

| Metric | Value |
|---|---|
| SSTables created | 1,401 total; 1,308 already existed at the failure boundary (t≈5,173s); only 93 more over the remaining ~9,227s |
| WAL bytes (final) | 13,543,928 (≈13.5 MB) — **stayed bounded**, did not grow unboundedly, because `purge_before` intermittently succeeded during partial-recovery bursts (§2) |
| Manifest bytes (final) | 103,674 (≈103 KB) |
| RSS at start | 30,284 KB |
| RSS at end | 77,796 KB (reported `rss_growth_pct=156.9%` by the harness's own start-vs-end-only computation) |
| **RSS peak (not reported by the harness)** | **244,960 KB at t=4,812.5s — 708.9% growth over the start-sample baseline**, over 4.5x worse than the harness's own headline 156.9% figure. RSS then *fell* sharply as the failure set in (bursty allocation churn from repeatedly-failing flush attempts, not a leak) before ending at 77,796 KB. **The harness's `rss_growth_pct` (first-sample vs. last-sample only) understates true peak memory pressure whenever a mid-run collapse follows a mid-run peak — flagged as an observability gap, see §8.** |
| Throughput before failure | 19,585–27,496 ops/sec sustained (mean ≈25,400 across 40 pre-failure samples) |
| Throughput after failure | Collapsed to 0–765 ops/sec for the large majority of samples in the failure window; a handful of partial-recovery bursts up to 21,132 ops/sec; harness's own `throughput_drop_pct_start_to_end=97.9%` (first sample 24,156 vs. last sample 498) |
| `completed_ok` (final) | 136,012,601 |
| `completed_err` (final) | 580,190,298 |
| `submitted` (final) | 716,203,099 |
| Flush retry attempts (stderr) | 4,407 distinct `flush attempt N failed` lines, attempt counter reaching **1,257** for a single stuck flush cycle before the run ended — see §5, this counter is never reset to a bounded failure state |
| `sync_failures` (WAL committer) | 0 throughout — WAL append failures are visible via `completed_err`, not `sync_failures`; worth reconciling if `sync_failures` is meant to be the WAL-layer ENOSPC signal (see §8) |
| `capacity_pressure_events` | 0 throughout — the existing MemTable-freeze backpressure signal (`[[project_rubixdb_capacity_contract]]`) never fired; ENOSPC is a different failure mode than MemTable capacity and is currently invisible to this counter (see §8) |
| `rejected_backpressure` | 0 throughout — engine never rejected a submission outright |
| Recovery after shutdown | **OK.** `recovery_ms=5582.4`, 0 corrupted segments, 1,401 live SSTables reconciled against the Manifest, 2,802 manifest records replayed, WAL replay prefix-correct (`wal_records_applied=272285`, `wal_records_skipped_by_checkpoint=92232`). Durability and crash-safety were never compromised by this failure — the defect is entirely in *availability/retry/backpressure behavior under sustained ENOSPC*, not in correctness or durability. |

---

## 5. Root cause — unbounded flush retry loop

`spawn_flush_thread`'s retry loop (`src/lsm/mod.rs:913-1037`) does **not** actually bound retries
by `max_retries` (`LsmConfig::default().max_flush_retries = 3`). `max_retries` is used *only* to
pick a backoff duration:

```rust
let backoff = if attempt <= max_retries {
    Duration::from_millis(50u64.saturating_mul(attempt as u64))
} else {
    Duration::from_secs(2)
};
```

Once `attempt > max_retries`, the loop does **not** stop, does **not** transition to any terminal
or degraded state, and does **not** surface the failure anywhere the caller or an operator could
see except a raw `eprintln!` per attempt. It simply keeps retrying forever at a flat 2s cadence.
The doc comment at `mod.rs:67-68` even names this as intentional design ("bounded flush retry
before falling back to a longer periodic retry — not a hard failure limit"), but as implemented
there is no distinction between "storage will recover shortly" (worth waiting indefinitely) and
"storage is fundamentally unavailable" (should fail loudly, stop retrying at full pace, and signal
backpressure upstream) — every I/O error, including a permanent one, gets the identical infinite
2s-forever treatment. `stderr`'s 4,407 failure lines (1,257 consecutive attempts in the worst
stretch) is this loop running exactly as coded, not a crash or hang — which is itself the problem:
a production database silently, indefinitely re-attempting a doomed operation once every 2 seconds
for over two and a half hours with no escalation.

This is the mechanism behind §2's frozen `checkpoint_seq`/`sstable_count` during the stall: the one
flush thread is wedged in this loop, so no other frozen memtable can be flushed, no checkpoint can
advance, and (per the idempotent-retry design at `mod.rs:894-909`) `pool.purge_before` — the very
last step of the loop body — is only reached once the *entire* flush succeeds, so WAL purge is also
blocked for the whole stall (WAL bytes stayed bounded here only because of the intermittent
partial-recovery bursts in §2, not because purge was proceeding independently).

---

## 6. Retry policy — recommendation (not yet implemented; needs design sign-off, see below)

Given the evidence above, a production-appropriate policy needs to distinguish, before it starts a
long 2s-forever loop, whether the error is one that unbounded retry can ever fix:

- **`ENOSPC` should be classified as a distinct, storage-health-relevant error** (not folded
  indistinguishably into the general `EngineError::Io` bucket it is today — `error.rs:25-26`).
- Bounded fast retries (the existing `50ms * attempt` ramp, up to `max_retries`) remain appropriate
  for genuinely transient I/O hiccups.
- Beyond `max_retries` **on an ENOSPC-classified error specifically**, the flush thread should stop
  hammering the disk at a fixed short interval and instead enter an explicit storage-pressure state
  (§7) with a much longer backoff and/or an active free-space check before the next attempt,
  emitting one clear state-transition log line instead of one `eprintln!` per attempt.
- This is a genuine design decision, not a mechanical fix — it changes the frozen write path's
  failure-handling contract. Per this project's own convention (`[[feedback_rubixdb_rigor]]`,
  "don't redesign without new measured evidence" / "large multi-stage specs executed as small,
  independently-verified increments"), the state-machine shape (§7 of the requesting brief:
  `HEALTHY → STORAGE_PRESSURE → STORAGE_FULL`) and its exact transition/recovery semantics should
  be written up as a `PHASE_WRITE_ENGINE_STORAGE_PRESSURE_ADR.md` and reviewed before
  implementation, the same way `PHASE4A_ADR.md`/`PHASE5_ADR.md` preceded their respective code
  changes. **Not implemented as part of this analysis** — this document is the investigation and
  root-cause record that ADR should be built on, per the requesting brief's own §11 ("do not
  immediately rerun the soak; first complete ENOSPC analysis, counter audit, retry-policy fix,
  backpressure verification...").

---

## 7. Storage budget (for the eventual re-run — not executed here)

Measured growth rate from this run's *healthy* window (t=0 to t≈4,812s, before the stall):
SSTables grew from 0 to ~1,293 in 4,812s ≈ **0.27 SSTables/sec**. Each `LsmConfig::default()`
memtable is capped at 4 MiB (`memtable_max_size_bytes=4194304`), so a fully-populated flushed
SSTable is on the order of a few MiB. At this rate, a full undisturbed 14,400s run would produce
roughly 3x the 1,293 figure ≈ **~3,900 SSTables**, i.e. very roughly **10-15 GB** of live SSTable
data alone (before accounting for the fact that this engine has no compaction yet — Manifest still
listed 1,401 *live* tables at the point of failure, meaning nothing is ever reclaimed short of
future compaction work). This is a rough order-of-magnitude estimate from partial-run data, not a
precise projection — flagged as such rather than presented with false precision. **Before any
re-run**, the actual free space on whatever volume is used must be confirmed to comfortably exceed
this estimate with margin, and per the requesting brief's §13, a dedicated volume other than this
machine's chronically-97%-full `C:` `%TEMP%` should be used (this repository is on `E:`, which
prior measurements record as far less full — see `FINAL_WAL_ANALYSIS.md` §5's disk table).

---

## 8. Observability gaps identified by this analysis

- **`rss_growth_pct` (start-vs-end only) can badly understate true peak memory pressure** — this
  run's true peak growth (708.9%) was 4.5x the reported headline number (156.9%) because the
  failure caused RSS to fall before the run ended. A `max_rss_kb_observed` line (the harness
  already computes `max_queue_depth_observed`/`max_wal_bytes_observed`/`max_immutable_count_
  observed` this same way — just needs an equivalent for RSS) would close this gap cheaply.
- **No sub-120s-granularity timestamp on individual flush failures** — `eprintln!` lines in
  `spawn_flush_thread` have no timestamp at all, making exact first-occurrence time impossible to
  recover after the fact (§2's timeline is bounded to a 120s window, not a precise instant).
- **`sync_failures` stayed 0 while `completed_err` carried the entire ENOSPC signal** — if
  `sync_failures` is intended as the WAL-layer's own dedicated I/O-failure counter, it should be
  reconciled with why WAL-append `Err`s during this run only ever showed up via `completed_err`,
  not here.
- **`capacity_pressure_events` stayed 0** — this is expected (it is the MemTable-freeze/backpressure
  signal per `[[project_rubixdb_capacity_contract]]`, a different failure mode), but it means there
  is currently **no dedicated counter for "flush is stuck on repeated I/O failure"** at all — an
  operator watching only the existing metrics would see throughput collapse and a growing
  `completed_err` but no counter that names *storage exhaustion* as the cause without reading logs.

---

## 9. What this document does and does not conclude

**Does:** establish, with cited line numbers and verified arithmetic, that (a) the counter values
are internally consistent and not a bug, (b) the flush retry loop is genuinely unbounded past
`max_retries` and is the mechanism behind the throughput collapse and the WAL-purge/checkpoint
stall, (c) durability and crash-recovery correctness were unaffected, and (d) the harness's binary
`exit_code == 0` → PASS logic is insufficient and must be replaced (see the requesting brief's own
§2 — harness fix tracked separately, not yet applied as of this document).

**Does not:** implement the storage-pressure state machine, the ENOSPC-specific retry-policy
change, the capacity-exhaustion test, or the crash-under-ENOSPC test. Those require a design
decision (§6) that should be written up and reviewed, consistent with how every other write-path
behavior change in this project (`CapacityExceeded`, idempotent flush retry, the Batch Coordinator
architecture itself) was decided via an explicit ADR before implementation, not decided inline
inside a failure-analysis document.

**Certification status: unchanged from before this run — write engine remains NOT READY.** This
run does not newly break anything that was previously certified (WAL, MemTable, SSTable, Manifest,
crash-recovery correctness all held up); it closes out the previously-open "full-pipeline multi-hour
soak: NOT RUN" gap in `PHASE_WRITE_ENGINE_TEST_RESULTS.md` §26/Summary by actually running it — and
what running it found is a new, real blocker (ENOSPC handling) that must be fixed and re-verified
before that gap can be marked PASS.
