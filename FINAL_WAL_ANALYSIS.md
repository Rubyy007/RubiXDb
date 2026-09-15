# RubiXDB WAL + Group Commit — Final Forensic Analysis

**Analysis-only document.** No production code (`src/`) was modified to
produce this report. Two new analysis-only tools were added under
`examples/` (`storage_baseline.rs`, `thread_spawn_cost.rs` — pure
`std::fs`/`std::thread`, zero new dependencies, self-cleaning, never
touch `src/`) purely to gather independent measurements; they are
documented in §3/§4 below like any other evidence-generation tool in this
repository (`window_sweep.rs`, `append_only_benchmark.rs`).

**This document does not replace `PHASE1_TEST_RESULTS.md`.** That file
remains the single source of truth for Phase 1's own pass/fail gate and
milestone history. This document is a from-first-principles forensic
analysis that reuses, re-verifies, and in several places corrects or
extends that record — every place the two documents could be read as
disagreeing is called out explicitly (see §16 "Hardware vs. Software
Attribution" and §20 "Final Engineering Decision").

**Repository state analyzed** (§2 of the operating brief):

```
git rev-parse HEAD:        61244f2da1f2e8725a93c5929e34fc5d0a5fa225
git branch --show-current: master
git status:                clean at analysis start; two new example
                            files (examples/storage_baseline.rs,
                            examples/thread_spawn_cost.rs) added during
                            this analysis, no src/ changes
```

```
git log --oneline -20
61244f2 phase-1: execute ADR-14's pipelining revert; fix a real crash-consistency bug found along the way
a2c2dc0 phase-1: implement Phase B pipelining; measured regression, not adopted
00fc5d0 phase-1: fix Phase A instrumentation's own contention; correct §9C's root cause
ac1b23c phase-1: re-confirm Phase A timing disagreement a third time; still STOP
047cfd6 phase-1: Phase A timing diagnostic for pipelining fix -- STOP per its own gate, disk near-full
bdc4fc2 phase-1: record final commit hash in PHASE1_TEST_RESULTS.md
4281d34 phase-1: resolve throughput target miss with window-size sweep (formula was under-tuned, not purely disk-bound)
e5d4340 intemediate commit for the food
d256d4e phase-1(group-commit): record final commit hash in PHASE1_TEST_RESULTS.md
d564fa1 phase-1(group-commit): add PHASE1_TEST_RESULTS.md; update CHANGELOG/PROGRESS/ARCHITECTURE
b660b59 phase-1(group-commit): add M1.4-M1.6/watermark tests; backpressure, shutdown, stats, 11 abort points
a923db9 phase-1(group-commit): add M1.1-M1.3 throughput suite; M1.2/M1.3 miss target on this disk
7451b5b phase-1(group-commit): fix durable_seq footgun, spin CPU burn, and stale docs
26c4624 phase-1(group-commit): add FsyncLatencyTracker and core GroupCommitter
26a2823 wal completion some things are need to check
f338e66 Add project files.
7779040 Add .gitattributes and .gitignore.
```

**Chronology (which code produced which historical numbers, §2 of the
operating brief):**

| Commit | What changed | Result status today |
|---|---|---|
| `a923db9`–`d564fa1` | Original M1.1–M1.6 suite, `200µs`/`EMA÷10` window formula | Historical — formula since replaced |
| `4281d34` | Window-size sweep; `WINDOW_EMA_DIVISOR` `10→1`, `max_wait` `200µs→5ms` | Historical — this formula is still current, unchanged |
| `047cfd6`/`ac1b23c` | Phase A timing diagnostic added; misdiagnosed as disk-near-full | **Superseded** — see `00fc5d0` |
| `00fc5d0` | Fixed the diagnostic's own follower-side contention; corrected root cause | Instrumentation design still current |
| `a2c2dc0` | Pipelining (`filling_active`/`fsyncing_active`) implemented, measured, regressed throughput | **Reverted** by `61244f2` — code no longer in the tree's active path (recoverable from git history only) |
| `61244f2` (**HEAD, analyzed here**) | Reverted `a2c2dc0` back to serial `leader_active`; fixed a real non-atomic-segment-creation crash-consistency bug found while re-verifying the revert | **Current** — every number in this document is measured against this exact commit unless explicitly marked historical |

No result in this document is drawn from `a2c2dc0` (pipelined) or from
the pre-`4281d34` window formula without being explicitly labeled
historical.

---

## 1. Executive Summary

RubiXDB's WAL + Group Commit implementation, at `61244f2`, is
**correctness- and durability-safe** (11/11 fresh crash-consistency runs
this session, full regression gate clean) but **does not meet its
original throughput targets** on the current development machine: best
observed **10,708–10,826 ops/sec at 100 writers** (target 15,000, 71–72%)
and **61,691–63,784 ops/sec at 1,000 writers** (target 80,000, 77–80%).

The dominant, directly measured cause is **`fsync`/`FlushFileBuffers`
latency on this machine's SATA SSDs** (~4.0–6.1ms per physical sync,
confirmed three independent ways: WAL-internal stage timing, a raw
`std::fs`-only storage baseline, and Criterion microbenchmarks), which
occupies **46–54% of every batch cycle** and cannot be amortized away by
window-size tuning beyond what this codebase's existing
window-size-sweep fix (`PHASE1_ADR.md` ADR-12) already captures — a
fresh, independent 10-point window sweep at both concurrency levels in
this analysis found the production default (5ms) within **~3% of the
best window tested** at both levels, not tens of percent off. CPU is not
saturated (60–75% of all 8 logical processors at 1,000 writers, 22–27%
at 100), allocation/serialization cost is 1,000–1,700x smaller than
`fsync` cost, and lock contention — while real and measurable via a
strong indirect signal (see §9) — is a secondary, not primary,
contributor. This is a **storage-latency-bound system with secondary,
real, but non-dominant software overhead**, not a software-bound one.

**The single highest-value, lowest-risk next step is not a code change
at all: re-run this exact, unmodified binary on faster storage** (NVMe,
or any device with materially lower `fsync` latency) — §16's attribution
table and §14's analytical model both show this is where the leverage
is. Among code changes, the analysis identifies one genuinely
promising, currently *unverified* hypothesis worth a small, targeted
experiment (see §17, P1): replacing the leader's window-wait busy-spin
with a park/sleep-based wait now that the production window is
milliseconds, not the sub-millisecond scale the current spin design was
justified against.

---

## 2. Current Implementation

`GroupCommitter` (`src/wal/group_commit.rs`) wraps a `FileWal` and
implements leader/follower batching exactly as described in
`PHASE1_GROUP_COMMIT.md` (updated by this session's own prior commit to
match the current, post-revert code):

- **Single-phase `LEADER_ACTIVE` span** (`BatchState.leader_active`):
  one thread owns a batch from election through a completed `fsync`,
  strictly serialized — no batch `N+1` window overlaps batch `N`'s
  `fsync`. (The alternative, pipelined design exists in git history at
  `a2c2dc0` — implemented, correctness-verified, but reverted after
  being measured to regress throughput on this platform; see §12.)
- **Leader window**: `min(max_wait_cap, EMA(fsync) / WINDOW_EMA_DIVISOR)`
  (`WINDOW_EMA_DIVISOR = 1`), with a demand-adaptive two-stage probe
  (`PROBE_WINDOW = 200µs`): if no follower has appended anything by
  200µs, the leader stops waiting immediately rather than paying the
  full window for a batch of one. Production default `max_wait = 5ms`,
  `max_batch_bytes = 256 KiB`.
- **Window-wait mechanism**: a tight poll loop (`spin_wait_for_batch_
  window`) — `std::hint::spin_loop()` most iterations, `std::thread::
  yield_now()` every 10,000th — not `thread::sleep` (a documented,
  historical decision: at the *original* 200µs window, sleep's Windows
  timer-resolution overshoot would have cost more than the window was
  worth; see §17 for whether this still holds at the *current* 5ms
  window).
- **Snapshot**: leader clones the active segment's file handle and
  reads the batch's target `seq` under the `wal` mutex
  (`snapshot_sync_target`) — the *only* critical section this design
  puts around any part of the `fsync` path.
- **`fsync`**: executed on the cloned handle, **outside** the `wal`
  lock — the WAL mutex never spans a physical sync (verified by
  reading `run_as_leader`/`snapshot_sync_target`, not merely assumed).
- **Durability watermark**: `durable_through` (an `AtomicU64`),
  advanced only via `fetch_max` after a successful `fsync`, published
  before one `condvar.notify_all()` wakes every waiter.
- **Followers**: block on the shared `Condvar` with a bounded timeout
  (`10 × EMA`, floored at `max_wait_cap`), re-checking `durable_through`
  on every wake; a timeout returns `EngineError::Timeout`, never a false
  acknowledgment.
- **Backpressure**: a bounded permit count (`DEFAULT_MAX_PENDING_
  WAITERS = 65,536`), not a queue.
- **Rotation**: `create_new_segment_file` now writes a new segment's
  header to a temporary name and `fs::rename`s it into place only once
  fully durable (this session's own fix, `61244f2` — see §12, entry 9).

## 3. Current Hardware

Measured with native OS commands (`systeminfo`, `Get-CimInstance`,
`Get-PhysicalDisk`, `Get-Volume`, `Get-Partition`), not inferred from
product names.

| Item | Value |
|---|---|
| OS | Windows 10 Home, `10.0.19045` (Build 19045) |
| System | Gigabyte H110M-S2 (confirms a 6th/7th-gen "Kaby Lake"-class desktop platform) |
| CPU | Intel Core i7-7700 @ 3.60GHz — **4 physical cores / 8 logical processors** (hyperthreaded), `MaxClockSpeed` 3601 MHz |
| RAM | 16 GiB |
| Power plan | Balanced (`381b4222-f694-41f0-9685-ff5bb260df2e`) — not High Performance; not changed for this analysis (changing power policy is outside "analysis only") |
| Disk 0 | "SSD 128GB" (generic), **SATA bus**, 128,035,676,160 bytes — entirely volume `E:` (repo + test-TEMP location), 127,355,362,816 bytes, **103.6 GB free / 127.3 GB (81% free)** |
| Disk 1 | "LAPCARE" branded, **SATA bus**, 128,035,676,160 bytes — split into `C:` (87,995,449,344 bytes, **~2.9 GB free, 97% full** — the OS default `%TEMP%`) and `D:` (37,220,253,696 bytes, ~35 GB free, 94% free) |
| Filesystem | NTFS on all volumes |
| Antivirus | Windows Defender real-time protection: **enabled** (`Get-MpComputerStatus`) — not disabled or toggled for this analysis (out of authorized scope); a real, unisolated potential confound on every filesystem measurement below |
| Rust | `rustc 1.98.1 (48a229cea 2026-09-01)` |
| Cargo | `cargo 1.98.1 (797e8a9bc 2026-08-05)` |
| Build profile used | `--release` for every throughput/latency number in this document unless stated otherwise |

**Both physical disks are SATA SSDs, not NVMe.** This is itself
consequential: SATA's AHCI command queue and typical consumer-SSD
`FLUSH CACHE` handling carry materially higher durable-write latency
than NVMe's, independent of anything RubiXDB does. No hardware model was
assumed faster or slower than another without the measurements in §5/§7.

**Important structural fact for §12/§16**: `C:` and `E:` — the two
volumes this project's history has compared ("near-full" vs "healthy")
— are **different physical disks**, not the same disk at two fill
levels. `C:` and `D:` share a disk; `E:` does not share a disk with
either. This confound is carried through explicitly in §12; it was not
previously called out in `PHASE1_TEST_RESULTS.md`.

## 4. Benchmark Methodology

### 4.1 Validity audit of the dedicated M1.2/M1.3 harness (`tests/group_commit/support.rs`)

Read in full for this analysis. Findings:

1. **Durability accounting is correct.** `run_throughput_scenario`'s
   per-thread loop calls `append_durable_retrying`, which calls
   `committer.append()` then `await_durable_retrying_on_timeout` — the
   latter loops (up to 1,000 times) *only* on `Err(EngineError::
   Timeout)`, and the only way the function returns is `Ok(())` from
   `await_durable`, which itself only returns `Ok` when `durable_
   through.load() >= seq` (a genuinely `fsync`-proven watermark, per
   §2's description). **No operation counted in `total_records` can be
   anything other than genuinely durable.** A timed-out wait is retried
   (costing wall-clock time that *is* counted against throughput — the
   right choice: a real caller would experience that latency too), never
   silently dropped or double-counted.
2. **Gap: no timeout/retry count is reported.** Unlike `examples/
   group_commit_load_test.rs` (which explicitly reports `failed_
   operations` with a timeout/other breakdown), the dedicated M1.2/M1.3
   tests report only a final `ops/sec` — there is no visibility, from
   their own output, into how many `await_durable` calls initially timed
   out before succeeding. This is a real observability gap, not a
   correctness bug (see finding 1). Partial mitigation: the load-test
   harness (§4.2/§7) *did* report 0 failures at both the 100- and
   1,000-writer levels under the current formula (`PHASE1_TEST_RESULTS.md`
   §16), so in practice this isn't hiding a large effect — but the
   dedicated milestone tests cannot themselves prove that.
3. **Thread spawn is inside the timed region, not excluded.** `let
   started = Instant::now();` precedes the `(0..threads).map(|t| ...
   thread::spawn ...).collect()` call. Measured in isolation this
   session (`examples/thread_spawn_cost.rs`, 3 repetitions each, trivial
   per-thread work): spawn+join alone costs **~50–63ms at 1,000 threads**
   (~50–63µs/thread) and **~6–8ms at 100 threads**. Against total M1.3
   runtime (~15.7–20.5s) this is **≤0.4%**; against M1.2 (~9.2–10.6s) it
   is **≤0.1%**. **Included, but immaterial** — a measured answer, not
   an assumption.
4. **No warm-up phase is excluded** in the dedicated M1.2/M1.3 tests
   (unlike `group_commit_load_test.rs`, which has an explicit,
   unmeasured warm-up phase before each concurrency level). `GroupCommitter::
   new` does perform one real warm-up `fsync` at construction (seeding
   the EMA before any writer thread starts — `PHASE1_TEST_RESULTS.md`
   §17 finding #1), which limits the blast radius of this gap, but the
   very first several batches still ramp through the demand-adaptive
   probe before reaching steady state. Not separately quantified in this
   analysis; noted as a residual, small, unquantified source of
   measurement noise.
5. **Recovery/correctness assertions run after the timed region** (after
   `elapsed` is captured) — correctly excluded from the throughput
   number.

**Conclusion**: the dedicated M1.2/M1.3 throughput numbers are **valid
measurements of genuinely durable ops/sec** for the reasons in finding 1,
with two quantified-and-dismissed sources of inclusion (thread spawn,
≤0.4%) and one acknowledged-but-unquantified one (no warm-up, likely
small given the pre-seeded EMA).

### 4.2 Repetition policy

Every throughput number in §7/§8 below is reported as a **range across
multiple independent runs**, never a single run, per the operating
brief's explicit rule against single-lucky-run claims.

## 5. Storage Baseline (independent of RubiXDB)

`examples/storage_baseline.rs` — pure `std::fs`, no WAL code, no new
dependency, self-cleaning (asserts it leaves nothing behind before
exiting). Measures: 200 repetitions of write-then-`sync_all` at 24B/256B/
4KiB, 200 repetitions of `sync_all`-only (no new dirty bytes) on an
already-written 4KiB file, and one 32 MiB sequential write in 64 KiB
chunks with a single trailing sync.

**`E:` (healthy, Disk 0, 81% free)** — two independent runs:

| Metric | Run 1 | Run 2 |
|---|---|---|
| `sync_write[24B]` p50 / p95 / p99 / max | 5.735 / 7.853 / 8.410 / 8.587 ms | 6.249 / 8.335 / 9.725 / 24.864 ms |
| `sync_write[256B]` p50 / p95 / p99 / max | 4.413 / 7.564 / 8.913 / 23.915 ms | 6.299 / 8.202 / 8.606 / 8.898 ms |
| `sync_write[4KiB]` p50 / p95 / p99 / max | 3.312 / 4.978 / 6.628 / 6.797 ms | 3.258 / 4.946 / 6.065 / 6.207 ms |
| `fsync_only` p50 / p95 / p99 / max | 2.421 / 3.696 / 3.814 / 3.836 ms | 2.364 / 3.638 / 4.151 / 19.998 ms |
| Sequential throughput | 237.0 MiB/s | 322.6 MiB/s |

**`C:` (near-full, Disk 1, 97% full — the OS default `%TEMP%`)** — one
run (kept intentionally tiny: ≤32 MiB peak footprint, fully cleaned up,
on the location already established by this project's own history as
its `%TEMP%`):

| Metric | Value |
|---|---|
| `sync_write[24B]` p50 / p95 / p99 / max | 3.801 / 6.743 / 9.854 / 18.509 ms |
| `sync_write[256B]` p50 / p95 / p99 / max | 3.573 / 6.512 / 7.061 / 10.743 ms |
| `sync_write[4KiB]` p50 / p95 / p99 / max | 2.743 / 4.395 / 5.516 / 9.525 ms |
| `fsync_only` p50 / p95 / p99 / max | **31.056 / 94.560 / 111.926 / 140.838 ms** |
| Sequential throughput | 207.5 MiB/s |

**Two distinct, both-measured findings, not one:**

1. **Write-then-sync (real dirty data) is comparable between the two
   disks**, even slightly *faster* on the near-full one for this
   workload shape (3.3–6.3ms range on `E:` vs. 2.7–3.8ms on `C:`).
   Sequential throughput is comparable (207–323 MiB/s, both disks, run-
   to-run variance included).
2. **A bare `fsync` with no new dirty data is catastrophically slower on
   the near-full disk**: 31ms median, up to 141ms max, vs. ~2.4ms median
   / ~4–20ms max on the healthy disk — a **13x median, up to ~37x p99**
   difference. This reproduces the *direction* of `PHASE1_TEST_RESULTS.md`
   §9C's historical "~8x fsync inflation on near-full disk" finding
   independently, with zero WAL code involved, which is meaningful
   corroboration — but see the confound below.

**Confound, stated explicitly (not present in the historical record)**:
`C:` and `E:` are **different physical disks** (§3), not the same disk
at two fill levels. This analysis did not attempt a same-disk
comparison (`C:` vs. `D:`, which share Disk 1) — `D:` is the user's
personal drive with unrelated files on it, and creating even a small,
self-cleaning scratch directory there without being asked was judged out
of this analysis's scope. **The disk-fullness effect above is real and
reproduced, but has never been cleanly isolated from disk-identity/
model differences in this project's history, including this session's
own measurement.** This is stated as a limitation, not resolved by
assumption.

**Windows vs. POSIX semantics, explicitly** (operating brief §5's
requirement): `std::fs::File::sync_all()` calls `FlushFileBuffers` on
Windows, not `fsync`/`fdatasync`. `FlushFileBuffers` is documented as
flushing all buffered data for the file to the physical device,
including metadata, and has been observed in this project's own history
(`PHASE1_TEST_RESULTS.md` §9E.4, `PHASE1_ADR.md` ADR-14) to plausibly
serialize more aggressively across concurrent handles to the same file
than POSIX `fsync` does on Linux — the working hypothesis for why
pipelining regressed throughput on this platform (§12). This was not
independently re-verified as its own controlled experiment in this
analysis (it would require a Linux environment this project does not
have); it is carried forward as existing, real, but not newly-confirmed
evidence.

## 6. Baseline Results (WAL microbenchmarks, Criterion)

`cargo bench --bench wal_bench` (pre-existing, unmodified) and `cargo
bench --bench append --features bench` (pre-existing, unmodified,
feature-gated per `Cargo.toml`). Both already existed in the tree;
neither was changed to produce these numbers.

**`wal_append_sync`** (single-threaded, `SyncMode::Immediate` — every
call pays its own full `fsync`; Criterion's own [low, median, high]
confidence interval):

| Payload | Time (Criterion CI) | Throughput |
|---|---|---|
| 16 B | [6.492, 6.649, 6.865] ms | ~2.35 KiB/s |
| 256 B | [4.393, 4.595, 4.891] ms | ~54.4 KiB/s |
| 4096 B | [4.496, 4.565, 4.641] ms | ~876 KiB/s |

**`wal_recovery_replay`** (single `open_for_recovery` call, replaying a
pre-populated segment):

| Records | Time (Criterion CI) | Throughput |
|---|---|---|
| 100 | [693.7, 715.8, 741.0] µs | ~139.7 Kelem/s |
| 1,000 | [5.729, 5.863, 6.009] ms | ~170.6 Kelem/s |
| 10,000 | [51.18, 52.39, 53.73] ms | ~190.9 Kelem/s |

Recovery throughput **increases** with record count (not flat or
decreasing) — consistent with fixed per-`open` overhead amortizing over
more records, and inconsistent with a meaningful per-record allocation
bottleneck (§13).

**`wal_append_only_no_sync`** (single-threaded, no `fsync` at all —
isolates `SegmentIo::append`'s encode+CRC+write cost alone):

| Payload | Time (Criterion CI) | Implied ops/sec ceiling |
|---|---|---|
| 16 B | [2.883, 2.940, 3.018] µs | ~340,000/s |
| 256 B | [3.131, 3.201, 3.286] µs | ~312,000/s |
| 4096 B | [8.515, 9.319, 10.215] µs | ~107,000/s |

**`examples/append_only_benchmark.rs`** (pre-existing, unmodified; 100
concurrent threads through one `Mutex<FileWal>`, no `fsync`, this
session's fresh run): **175,587 ops/sec** — historically observed range
138,000–141,000 ops/sec (`PHASE1_TEST_RESULTS.md` §14.1); today's number
is higher, attributed to normal run-to-run machine-load variance, not a
code change (the append path is untouched by this session's revert/fix).
Either number is **far above** any `fsync`-bound throughput measured
anywhere in this document.

**What each isolates** (operating brief §7's explicit requirement):
`wal_append_only_no_sync`/`append_only_benchmark` isolate pure CPU-side
cost (encode, CRC32C, lock, write syscall — no durability wait at all);
`wal_append_sync` isolates the *un-batched* durability cost (one `fsync`
per logical operation — the `SyncMode::Immediate` floor `GroupCommitter`
exists to amortize past); `wal_recovery_replay` isolates the read/replay
path, entirely independent of the write path's throughput ceiling.

## 7. Concurrency Scaling

### 7.1 Dedicated milestone tests (M1.2/M1.3), multiple repetitions, current HEAD

| Concurrency | Runs this session (ops/sec) | Median |
|---|---|---|
| 100 writers | 9,939 · 10,708 · 10,784 · 10,826 | **10,746** |
| 1,000 writers | 61,691 · 63,204 · 63,210 · 63,784 | **63,207** |

(9,939 was measured with `RGC_TIMING_REPORT=1` instrumentation compiled
in — see §11 for whether that materially distorts the result; the other
three were plain runs.)

### 7.2 Full thread-count sweep, ramp-up-dominated harness (reused from this session's prior cycle, cited with its own caveat)

`examples/group_commit_load_test.rs`'s concurrency matrix was
temporarily widened (same session, prior cycle, reverted before commit —
`PHASE1_TEST_RESULTS.md` §9G) to `[1, 10, 32, 64, 100, 128, 256, 512,
1_000]`:

| Writers | ops/sec | Batches | Avg batch size |
|---|---|---|---|
| 1 | 246 | — | — |
| 10 | 1,091 | — | — |
| 32 | 3,257 | — | — |
| 64 | 6,254 | — | — |
| 100 | 7,175 | — | — |
| 128 | 11,537 | — | — |
| 256 | 20,928 | 51 | 195.76 |
| 512 | 39,574 | 25 | 409.60 |
| 1,000 | 41,646 | 38 | 526.32 |

**Throughput increases monotonically across the entire range — no
saturation point and no regression at any intermediate level.** 1,000
threads outperforms every lower level actually tested; there is no
thread count in `[1, 1000]` at which fewer threads win. **This directly
answers the operating brief's §9/§16 question: 1,000 threads is not
merely tolerable, it is the best-performing level measured.**

**Caveat, stated plainly**: this harness's per-level op-count target
(~10,000–20,000 total, so *fewer* ops/thread at higher concurrency —
only 10 ops/thread at 1,000 writers) means these absolute numbers are
lower than §7.1's dedicated, longer-running tests (1,000 ops/thread) at
the same concurrency — less time for the EMA-driven window to reach
steady state. The **shape** (monotonic, no saturation) is a valid,
independent answer to the saturation question; the **absolute numbers**
in this table should not be read as this system's steady-state ceiling
— use §7.1 for that.

### 7.3 Scheduler cost at the two dedicated levels (measured, not inferred — §11 below)

Confirms *why* more threads keeps helping here rather than costing more
than it's worth in this range: CPU never saturates (60–75% of 8 logical
processors at 1,000 writers) and batch size keeps growing with
concurrency (§8) faster than scheduling overhead grows — see §11 for the
full data.

## 8. Batch Efficiency

Computed from `RGC_TIMING_REPORT=1` runs (`batches` field) against each
level's known total record count — a direct, not estimated, count.

| Level | Total records | Batches | Avg records/batch (= durable_ops_per_sync) | `batch_fill_ratio` (avg batch / writer count) | Syncs/sec | Cross-check: syncs/sec × avg batch |
|---|---|---|---|---|---|---|
| 100 writers | 100,000 | 1,071 | **93.4** | **93.4%** | 106.4 | 9,938 (measured: 9,939) |
| 1,000 writers | 1,000,000 | 1,442 | **693.5** | **69.4%** | 92.0 | 63,802 (measured: 63,784) |

The cross-check column — multiplying independently-measured `syncs/sec`
by independently-measured `avg records/batch` reproduces the
independently-measured `ops/sec` to within 0.03–0.3% — is strong internal
validation that these are consistent, non-contradictory measurements of
the same real system, not artifacts of how each number was individually
computed.

**`p95_batch_records`: NOT VERIFIED.** The existing instrumentation
(`GroupCommitStats`, `batch_timing`) tracks only the **mean** (via
summed totals) and the **maximum** (`max_batch_records`) batch size, not
a full per-batch distribution. Reporting a p95 here would require
fabricating a number from data that does not exist — refused per the
operating brief's absolute rule. `max_batch_records` observed across
this session's runs reached 900+ at the 1,000-writer level (from
`GroupCommitStats`/load-test output), well above the mean of 693.5,
indicating real batch-to-batch variance (expected, given batches also
form in response to real (unpredictable) per-thread arrival timing).

**Interpretation**: at 100 writers, the leader captures **93% of all
currently-live writers per batch on average** — very high, arguably
near the practical ceiling for that writer count (a writer can only
contribute once per its own per-thread loop iteration, and 100 threads
each doing 1,000 sequential ops cannot all land in literally every
single batch). At 1,000 writers, batch fill drops to **69%** — not
because the window is too short (§11's sweep shows more window barely
helps at 1,000 writers either) but because with 10x the writers doing
10x the total work in a similar wall-clock time, more batches form per
second and each has proportionally less time to accumulate the full
population before the EMA-tuned window (itself bounded by `fsync`
latency, not writer count) closes.

## 9. Window Sweep

Used the **existing, pre-built experiment mechanism**
(`phase1-window-experiment` Cargo feature, `PHASE1_EXPERIMENT_MAX_WAIT_US`
/ `PHASE1_EXPERIMENT_EMA_DIVISOR=0` env vars — no production code
touched) exactly as the operating brief instructs. One run per
configuration (not the historical 3-repetition sweep in
`PHASE1_TEST_RESULTS.md` §9A, which is cited as corroborating rather
than repeated wholesale) at each of 10 window sizes, both dedicated
milestone levels.

### 9.1 — 100 writers

| Window | ops/sec | mean_window | mean_snapshot | mean_fsync | mean_coord | Batches |
|---|---|---|---|---|---|---|
| 25 µs | 7,367 | 63.3 µs | **2,042.0 µs** | 4,003.3 µs | 76.8 µs | 2,194 |
| 50 µs | 5,390 | 86.5 µs | 1,307.2 µs | 3,863.4 µs | 80.4 µs | 3,475 |
| 100 µs | 6,513 | 142.3 µs | 1,614.2 µs | 3,952.3 µs | 83.4 µs | 2,650 |
| 200 µs | 6,261 | 249.3 µs | 1,724.4 µs | 4,042.5 µs | 90.2 µs | 2,615 |
| 500 µs | 8,195 | 547.6 µs | 1,582.7 µs | 4,048.8 µs | 86.6 µs | 1,947 |
| 1 ms | 8,555 | 1,046.7 µs | 1,548.2 µs | 4,215.9 µs | 95.7 µs | 1,692 |
| 2 ms | 9,394 | 2,041.4 µs | 1,145.7 µs | 4,310.1 µs | 104.1 µs | 1,400 |
| **3 ms** | **10,154 (best)** | 3,054.2 µs | 664.2 µs | 4,393.6 µs | 99.9 µs | 1,199 |
| 5 ms (production default) | 10,004 | 5,031.6 µs | 83.8 µs | 4,419.2 µs | 111.8 µs | 1,036 |
| 10 ms | 6,765 | 10,027.5 µs | 4.2 µs | 4,636.6 µs | 109.3 µs | 1,000 |

### 9.2 — 1,000 writers

| Window | ops/sec | mean_window | mean_snapshot | mean_fsync | mean_coord | Batches |
|---|---|---|---|---|---|---|
| 25 µs | 33,239 | 52.0 µs | **3,971.5 µs** | 4,252.7 µs | 78.6 µs | 3,600 |
| 50 µs | 26,508 | 80.6 µs | 2,449.8 µs | 4,037.4 µs | 78.5 µs | 5,673 |
| 100 µs | 27,686 | 130.7 µs | 2,379.5 µs | 4,103.3 µs | 79.3 µs | 5,395 |
| 200 µs | 30,229 | 244.4 µs | 2,206.9 µs | 4,199.3 µs | 77.4 µs | 4,915 |
| 500 µs | 44,091 | 596.1 µs | 1,594.8 µs | 4,624.9 µs | 49.7 µs | 3,302 |
| 1 ms | 61,699 | 2,898.0 µs | 586.6 µs | 4,775.3 µs | 18.1 µs | 1,957 |
| 2 ms | 61,788 | 3,358.1 µs | 491.9 µs | 4,825.8 µs | 24.6 µs | 1,858 |
| **3 ms** | **62,218 (best)** | 3,635.8 µs | 490.7 µs | 4,843.6 µs | 21.0 µs | 1,786 |
| 5 ms (production default) | 60,655 | 5,873.8 µs | 591.2 µs | 5,205.8 µs | 32.0 µs | 1,408 |
| 10 ms | 55,650 | 11,290.2 µs | 68.1 µs | 6,092.5 µs | 158.2 µs | 1,020 |

**Findings**:

1. **The production default (5ms) is close to optimal, not tens of
   percent off.** Best-measured window (3ms) beats it by **1.5%** at 100
   writers (10,154 vs. 10,004) and **2.6%** at 1,000 writers (62,218 vs.
   60,655) — both well within this machine's documented run-to-run
   variance (§7.1's own 4-run spread is ~9% at 100 writers). **Window
   tuning has essentially no further headroom to give on this
   hardware**, confirming rather than contradicting `PHASE1_ADR.md`
   ADR-12's existing conclusion.
2. **Windows below ~1ms are dramatically worse, at both levels** —
   collapsing to 44–70% of the 3–5ms plateau's throughput at 1,000
   writers, 53–86% at 100 writers.
3. **A clear, consistent mechanism for finding 2, not just a
   correlation**: `mean_snapshot_us` — the leader's `wal`-mutex-guarded
   file-clone step — **balloons 10–100x at small windows** (up to
   3,971.5µs at 1,000 writers/25µs window vs. 68.1µs at 1,000
   writers/10ms window) while `mean_fsync_us` stays comparatively flat
   (4,037–6,093µs across the *entire* sweep). Small windows create far
   more batches/second (up to 5,673/run vs. 1,000–1,957 at 1–5ms), each
   needing its own `wal`-lock acquisition for the snapshot — this is the
   clearest evidence in this analysis of real lock contention scaling
   with leader-election frequency (§11).
4. **`mean_fsync_us` is the most stable quantity in the entire sweep**
   (a ~2:1 range, 4.0–6.1ms, across a window range spanning three orders
   of magnitude, 25µs–10ms) — strong, direct evidence that `fsync`
   latency is close to an intrinsic property of this hardware/OS, not a
   function of how RubiXDB batches around it.
5. `mean_coordination_us` stays under 160µs everywhere (≤2% of any
   configuration's total cycle) — the leader-exclusive coordination
   measurement fix (ADR-13) continues to show a genuinely cheap
   coordination path.

**Not automatically adopted** — per the brief's explicit instruction,
this is analysis, not a recommendation to change the shipped default;
see §17 for the ranked recommendation (a 3ms default is P2, not P0/P1,
given the marginal, noise-comparable gain).

## 10. Fsync Analysis

Three independent measurements of the same physical quantity, for
cross-validation:

| Source | Method | p50 | p95 | p99 |
|---|---|---|---|---|
| `storage_baseline` (`E:`, healthy) | raw `fsync_only`, no WAL code | 2.36–2.42 ms | 3.64–3.70 ms | 3.81–4.15 ms |
| `wal_append_sync` (Criterion) | `Immediate` mode, 4096B payload | 4.565 ms (median) | — | — |
| `RGC_TIMING_REPORT` (`GroupCommitter`) | real per-batch leader `fsync`, production config | 4.67 ms (100w) / 4.95 ms (1000w), *mean* | — | — |

The raw no-op `fsync_only` baseline (~2.4ms) is **roughly half** the
`fsync` cost measured *inside* the real, growing, multi-threaded WAL
(~4.7–5.0ms). This is a real, measured discrepancy, not just noise —
plausible (not independently isolated) explanations: the WAL's `fsync`
always follows genuine new writes (more dirty data to flush than a
repeated no-op sync of an already-clean file), and/or concurrent system
load from up to 1,000 competing threads raises effective flush latency
under real contention. Both `wal_append_sync` (single-threaded, but with
real writes) and `RGC_TIMING_REPORT` (multi-threaded, real writes) land
in the same 4.5–5.0ms neighborhood, while the write-free baseline does
not — consistent with "real dirty data, not thread count, explains most
of the gap," but this is stated as the best-supported reading of the
data, not a fully isolated proof (isolating thread-count's own
contribution would require the write-free baseline run under 1,000-way
contention too, which was not done).

**`fsync_cost_fraction` of the total batch cycle** (production default,
5ms cap):

- 100 writers: `4,667.5 / (4,452.1 + 163.6 + 4,667.5 + 109.0)` = **49.7%**
- 1,000 writers: `4,949.4 / (5,275.7 + 561.3 + 4,949.4 + 47.8)` = **45.7%**

At the sweep's best-measured window (3ms): 53.5% (100w) / 53.9% (1000w)
— `fsync` becomes a *larger* fraction of a *shorter* cycle, which is
exactly what "the window shrinks but fsync doesn't" predicts.

**Classification** (operating brief §13): **primarily storage-bound**,
with a measurable, secondary coordination/lock-contention component
concentrated specifically in the snapshot stage under small-window
configurations (§9, §11) — not scheduler-bound (§11 shows no CPU
saturation) and not CPU-bound (§13 shows allocation/encode cost 1,000x+
smaller than `fsync`).

## 11. Coordination and Lock-Contention Analysis

**Instrumentation contamination check** (operating brief §14's explicit
requirement): comparing this session's one `RGC_TIMING_REPORT=1` run
against the three plain runs at each level —

- 100 writers: instrumented 9,939 ops/sec vs. plain range 10,708–10,826
  (instrumented run is **7–9% lower**).
- 1,000 writers: instrumented 63,784 vs. plain range 61,691–63,210
  (instrumented run is **within** the plain range, not below it).

**Verdict**: at 1,000 writers, instrumentation shows no measurable
distortion. At 100 writers, there is a **borderline, not clearly
resolved** 7–9% gap — smaller than this machine's already-documented
run-to-run variance at this level (§7.1's own 4-run spread is ~9%), so
it cannot be confidently separated from ordinary noise with the sample
sizes gathered here, but it is not dismissed as certainly zero either.
This matches (does not contradict) `PHASE1_ADR.md` ADR-13's finding that
the *current* (leader-exclusive) instrumentation design eliminated the
*severe* contention the *original* per-waiter-write design had (10–11k
vs. 62–63k ops/sec, an order of magnitude) — nothing in this session's
data reopens that finding; it only notes a smaller, unresolved residual
at one concurrency level.

**Lock contention — no direct CPU-level lock-wait profiler was
available** (Windows, no new dependency authorized, matching `PHASE1_
TEST_RESULTS.md` ADR-11's standing decision). The strongest available
evidence is indirect but consistent: §9's `mean_snapshot_us` — which
*is* the `wal`-mutex critical section (`snapshot_sync_target`: lock +
`File::try_clone` + reading `active_segment_sync_handle`) — scales
10–100x with leader-election frequency across the window sweep. This is
circumstantial, not a direct wait-time trace, and is reported as such:
**"strong indirect evidence of lock contention that scales with batch
frequency," not "measured lock-wait time."**

Confirmed by direct source reading (not assumed): `fsync` executes
**outside** the `wal` lock, on a cloned file handle
(`run_as_leader`/`snapshot_sync_target` in `src/wal/group_commit.rs`) —
the mutex never spans the expensive operation. The only lock-guarded
work in the batch's critical path is the snapshot (file-clone + seq
read), which is why even its *worst* measured cost (~4.0ms, 100-writer/
25µs-window) never exceeds `fsync`'s own cost, despite being 40–100x its
own best case.

## 12. Scheduler / Thread Analysis

Measured with native Windows performance counters (`Get-Counter`, no new
dependency) during real, unmodified test runs — not inferred.

| Metric | Idle baseline (post-test) | During 100-writer run | During 1,000-writer run |
|---|---|---|---|
| `\Processor(_Total)\% Processor Time` | 12–17% | 22–27% | **60–75%** |
| `\System\Context Switches/sec` | ~2,000–2,450 | ~51,300–55,400 | **~557,000–625,500** |
| `\System\Processor Queue Length` | 0 | 0 (all samples) | mostly 0, spikes to **76** and 4 |

(`% Processor Time (_Total)` is already normalized across all 8 logical
processors by the OS counter — 75% means roughly 6 of 8 logical cores
busy on average, not one core pegged.)

**Findings**:

- Context switches scale **roughly linearly** with thread count (~11x
  more threads → ~11x more context switches: 51–55k/sec at 100 writers
  vs. 557–625k/sec at 1,000). This is real, measured overhead from the
  1,000-OS-thread model, not a hypothesis.
- The processor queue length spiking to **76** at 1,000 writers (vs.
  never above 0 at 100 writers) is direct evidence of real, if
  intermittent, scheduling backlog at that concurrency — some threads
  genuinely wait for a core.
- **CPU never saturates at either level** (max 75% of all 8 logical
  processors). There is real spare CPU capacity even at 1,000 writers —
  this is the direct evidence against "CPU/scheduling is the primary
  bottleneck": if it were, throughput would be expected to plateau or
  fall as concurrency rose past some point, but §7.2's sweep shows
  monotonic *increase* through 1,000 threads with no such plateau.
- **A plausible (source-grounded, not independently isolated)
  contributor to both figures**: `spin_wait_for_batch_window` (§2) is a
  busy-poll loop — `spin_loop()` most iterations, `yield_now()` every
  10,000th — held by the current leader for the *entire* window
  duration (~4.4–5.9ms in the production/near-optimal range, once per
  batch, ~90–110 batches/sec at the two dedicated levels). Each `yield_
  now()` call is itself a scheduling decision. This is a real, textually
  verified mechanism that would produce exactly the kind of elevated
  context-switch count measured above; it was **not independently
  isolated** from the rest of the system's context-switch sources (that
  would require per-thread CPU/scheduler tracing not available without a
  new dependency) — stated as a well-supported hypothesis, not a proven
  isolated cause.

**Comparison across thread counts (64/128/256/512/1,000) for scheduling
pressure specifically**: not independently CPU-sampled at every
intermediate level in this analysis (time-bounded); §7.2's thread-count
sweep already answers the throughput-shape half of this question
(monotonic, no regression) directly, and the 100-vs-1,000 CPU comparison
above brackets the two dedicated milestone levels. A full CPU sweep
across every intermediate thread count is identified as a missing
experiment (§15.B) rather than fabricated.

**No new concurrency runtime is recommended based on this data** — see
§17.

## 13. CPU Analysis

Covered by §12's `Get-Counter` data (aggregate CPU%) and §6's Criterion
microbenchmarks (per-operation CPU-side cost). **No literal flame graph
was generated** (Windows, no profiler dependency authorized, matching
`PHASE1_ADR.md` ADR-11's standing decision) — stated as **NOT VERIFIED**,
per-core utilization, CPU frequency/throttling behavior during load, and
kernel-vs-user CPU split are likewise **NOT VERIFIED**. The measured
alternative provided in its place: §6's isolated single-threaded
per-record cost (2.9–10.2µs depending on payload) and §12's aggregate
CPU%/context-switch data during real runs.

## 14. Allocation Analysis

§6's `wal_append_only_no_sync` and `append_only_benchmark` numbers
directly answer this: a single record's full encode+CRC32C+write cost
(no `fsync`) is **2.9–10.2µs** depending on payload size — **1,000 to
1,700x smaller** than the ~4.0–6.1ms `fsync` cost measured throughout
§9–§10. Under real 100-thread contention (lock included, still no
`fsync`), the append path alone sustains **175,587 ops/sec** — 16x the
best `fsync`-bound throughput ever measured at 100 writers (10,826), and
still 2.7x the best measured at 1,000 writers (63,784). **Allocation/
serialization/CRC cost is conclusively not a material contributor to
this system's throughput ceiling at any concurrency level tested.** No
buffer-reuse or allocation optimization is recommended (§17, "Do not
do").

## 15. Filesystem Analysis

- **NTFS** on all volumes tested.
- **`FlushFileBuffers` vs. POSIX `fsync`**: documented, real semantic
  difference (§5) — not independently re-verified as a fresh controlled
  experiment in this analysis (would need a Linux host this project does
  not have); carried forward from `PHASE1_ADR.md` ADR-14 as existing,
  unretracted evidence.
- **Disk fullness**: real, measured, large effect on *pure* `fsync`
  (§5) — **but confounded with disk identity** (§3), and the effect on
  *write-then-sync* (the WAL's actual workload shape) was measurably
  smaller than on write-free `fsync`, a nuance not previously recorded.
- **Antivirus**: Windows Defender real-time protection is on; not
  isolated. A real-time scanner intercepting every WAL segment
  create/write/rename (including this session's own new atomic-rotation
  rename, §2) is a plausible, unmeasured contributor to filesystem
  latency and variance. **Not quantified — a missing experiment (§15.B),
  not a dismissed one.**
- **Directory operations**: this session's own fix (§2, rotation
  atomicity) added one `fs::rename` + one directory `fsync` per
  rotation — rotations are infrequent relative to appends (bounded by
  `max_segment_size = 64 MiB`, not exercised at all in the throughput
  runs measured in this document, whose total WAL bytes never approach
  that threshold), so this is not expected to and was not observed to
  affect §7's throughput numbers (§7.1's numbers, gathered *after* the
  fix, are consistent with the historical pre-fix range).

### 15.A Missing experiments, named explicitly (not silently skipped)

1. A same-physical-disk fullness comparison (`C:` vs. `D:`), to cleanly
   separate disk fullness from disk identity — not performed; would
   require creating scratch files on the user's personal `D:` drive,
   judged out of this analysis's authorized scope without being asked.
2. Windows Defender on/off comparison — not performed; changing system
   security configuration is outside "analysis only."
3. A controlled test of `FlushFileBuffers`'s cross-handle serialization
   behavior specifically (the pipelining-regression hypothesis,
   `PHASE1_ADR.md` ADR-14) — not performed in this session; flagged
   there as future work, still true here.
4. Per-core CPU utilization and a full 64/128/256/512-writer CPU sweep
   (§12) — not performed; time-bounded, not fundamentally blocked.

## 16. Historical Experiment Review

Built from `PHASE1_TEST_RESULTS.md`/`PHASE1_ADR.md`'s own record plus
this session's two changes. Every reverted experiment remains classified
as historical evidence, not deleted or repeated.

| Experiment | Change | Before | After | Result | Keep/Revert |
|---|---|---|---|---|---|
| Original batch formula | `max_wait=200µs`, `EMA÷10` | — | M1.2 8,944 / M1.3 31,438 ops/sec | Left most available batching headroom on the table | **Reverted** (superseded by window-size sweep) |
| Naive larger window | Apply sweep's best window unconditionally | M1.1 (1 writer) 2.905ms | M1.1 5.761ms | Real regression for the single-writer case | **Reverted** — replaced by demand-adaptive two-stage probe |
| Window-size sweep → adaptive formula | `EMA÷10 → EMA÷1`, `max_wait 200µs→5ms`, `PROBE_WINDOW=200µs` | M1.2 8,944 / M1.3 31,438 | M1.2 ~9,800–11,800 / M1.3 ~50,600–64,900 | Substantial, real improvement; M1.1 unaffected | **Kept** (current production default) |
| Spin-then-block lock | Spin on `try_lock` before blocking `lock()` | ~10,100 ops/sec | ~4,500 ops/sec | This machine's 8 logical cores are oversubscribed by 100+ threads; spinning starves the actual lock holder | **Reverted**, documented as a negative result |
| Phase A instrumentation (original) | Per-batch stage timing, follower-side shared atomic | — | Measured window/fsync 2.4–8.3x inflated vs. model | **Misdiagnosed** as disk-near-full; actually the instrumentation's own contention | **Reverted/fixed** — see next row |
| Phase A instrumentation (corrected) | Leader-exclusive coordination measurement | — | Matches production model (window≈5.0ms, fsync≈5.0ms, coordination≈0.03ms), 63,293 ops/sec | Root cause corrected; clears the gate for the next experiment | **Kept** — this session's window sweep (§9) reuses it |
| Pipelining (`filling_active`/`fsyncing_active`) | Overlap batch N+1's window with batch N's `fsync` | M1.3 63,293 (serial baseline) | M1.3 36,899–39,625 (pipelined) | Reproducible regression — `fsync` itself got ~2x slower under overlap, likely `FlushFileBuffers` cross-handle serialization on Windows | Implemented, correctness-verified, **not adopted** |
| Pipelining revert (this session, part 1) | `git checkout 00fc5d0` for `group_commit.rs`/`mod.rs` | Pipelined code was the only path in the tree despite being "not adopted" | M1.2 10,708–10,826 / M1.3 61,691–63,784, matching pre-pipelining baseline | Recommendation from the pipelining experiment finally executed, not just documented | **Kept** (current HEAD) |
| Segment-creation atomicity bug (this session, part 2) | `create_new_segment_file`: write header to final name directly | `crash_consistency_across_abort_points`: 0/8 deterministic failures (both pre- and post-pipelining-revert) | Write to temp name, `fs::rename` into place: 8/8 (then, this session, **11/11** across all fresh runs) | Real crash-consistency bug, unrelated to any performance work, found by the revert's own regression gate | **Kept** (current HEAD) |

No experiment in this table was repeated in this analysis without a new
question to answer (the window sweep reuses the corrected instrumentation
to re-verify the *current, post-revert* code's window sensitivity — a
genuinely new question, not a rerun of an old one).

## 17. Hardware vs. Software Attribution

| Factor | Evidence | Estimated impact | Confidence |
|---|---|---|---|
| **Storage `fsync`/`FlushFileBuffers` latency** | Measured 3 independent ways (§10): raw baseline ~2.4ms (no-op), Criterion ~4.5ms (real write), WAL-internal ~4.7–5.0ms (real, concurrent). Stable (4.0–6.1ms) across a 400x window-size range (§9). | **Dominant** — 46–54% of every batch cycle directly; the window itself (the other ~45%) exists *because of* this latency (the EMA-driven formula targets it) | **High** |
| **Batch-window policy** | 10-point sweep, both levels (§9): production default within 1.5–2.6% of best-measured window; sub-1ms windows measurably worse | Already near-optimal; **no material further headroom** | **High** |
| **Lock contention (snapshot stage)** | Indirect but consistent: `mean_snapshot_us` scales 10–100x with batch frequency (§9, §11); no direct CPU-level lock-wait trace available | **Secondary**, concentrated at small-window/high-frequency configurations not used in production | **Medium** (mechanism strongly indicated, not directly traced) |
| **Thread scheduling / context switches** | Measured (§12): context switches scale ~linearly with thread count (51–55k/sec → 557–625k/sec, 100→1,000 writers); CPU never saturates (max 75%); queue-length spikes to 76 at 1,000 writers | **Real but secondary** — a measurable cost, not (yet, in the range tested) a throughput-limiting one; §7.2 shows throughput still rising at 1,000 threads | **Medium** (aggregate cost measured; the specific spin-loop mechanism is a well-supported but not independently isolated contributor) |
| **Allocation / CRC / serialization** | Measured (§6, §14): 2.9–10.2µs/op single-threaded, 175,587 ops/sec at 100-way contention (no `fsync`) | **Negligible** — 1,000–1,700x smaller than `fsync` cost | **High** |
| **Filesystem (NTFS, disk fullness, antivirus)** | Disk-fullness effect measured but confounded with disk identity (§5); antivirus/`FlushFileBuffers` semantics not independently isolated this session | Real but **not cleanly quantified in isolation** | **Low–Medium** |
| **Benchmark harness overhead** | Measured directly (§4.1): thread-spawn ≤0.4% of runtime; durability accounting verified correct by source reading; no warm-up exclusion in dedicated tests (unquantified, believed small) | **Immaterial to the reported throughput ceiling** | **High** |

## 18. Maximum Achievable Throughput

Using the median of independent runs (not a single best run), per the
operating brief's explicit instruction:

| Level | Runs (ops/sec) | **Median (best stable throughput)** | Records/sync | Sync latency (mean) |
|---|---|---|---|---|
| 100 writers | 9,939 · 10,708 · 10,784 · 10,826 | **10,746** | 93.4 | 4.67 ms |
| 1,000 writers | 61,691 · 63,204 · 63,210 · 63,784 | **63,207** | 693.5 | 4.95 ms |

(The window-sweep's best-measured configuration, §9 — 3ms — reaches
10,154/62,218, ~1–5% above these medians; not adopted as "the" number
here since it was not the production default at time of measurement and
the gain is within this machine's noise floor.)

## 19. Target Feasibility

**M1.2 (≥15,000 ops/sec, 100 writers)**: median 10,746, **71.6% of
target**. **M1.3 (≥80,000 ops/sec, 1,000 writers)**: median 63,207,
**79.0% of target**.

Evaluated against the operating brief's three-way rubric:

- **A. Achievable with current architecture on current hardware**: not
  supported by the evidence. §9's exhaustive window sweep found no
  configuration within the tested space (25µs–10ms) that closes more
  than ~1–5% of the remaining gap; §17's attribution table shows the
  dominant factor (`fsync` latency) is stable across that entire range
  and is not a software-tunable quantity in this codebase.
- **C. Not achievable on current hardware without a major architectural
  or hardware change** — **this is the best-supported conclusion**,
  with the following quantitative floor: at the measured `fsync_cost_
  fraction` (~46–54%) and the measured, near-saturated `durable_ops_
  per_sync` ceiling at each writer count (§8 — 93% fill at 100 writers,
  69% at 1,000), reaching 15,000 ops/sec at 100 writers would require
  either **~1.4x today's records/sync** (93.4 → ~130, impossible — it
  would exceed the writer count itself) **or a ~40% reduction in
  cycle time** — and §9 shows window tuning cannot deliver that (the
  non-`fsync` portion of the cycle is already down to ~45–50%, and
  `fsync` itself is the stable, hardware-set quantity). The same
  argument applies more strongly at 1,000 writers (record/sync headroom
  exists — 69% fill vs. 93% at 100 writers — but §9's sweep already
  explored it and found no configuration that captured it into
  additional throughput; batches simply form at whatever `avg_batch_
  size` the concurrent demand naturally supports within an `fsync`-
  latency-set window, not less).
- **B. Potentially achievable but not yet proven** applies to the
  *secondary* factors only (lock contention at the snapshot stage, the
  spin-loop's contribution to scheduling cost) — closing those
  completely, per §17's own "secondary" classification, would not be
  expected to close the ~20–29% gap on its own, since together they are
  a smaller measured share of the cycle than `fsync` alone.

**Conclusion: (C), with (B) as an unproven, bounded-upside secondary
avenue** — not a silent declaration of impossibility (per the brief's
explicit warning against that), but a specific, quantified argument built
from §8's `durable_ops_per_sync` ceiling and §9's exhaustive window
sweep, both freshly measured against the current code in this session.

## 20. Recommended Optimizations

Ranked per the operating brief's P0/P1/P2/"Do not do" scheme.

### P0 — Must do

**None identified.** No correctness, durability, or safety issue remains
open (§21) that would justify a "must do" *performance* item; the one
correctness bug found this session (§2, segment-creation atomicity) is
already fixed and verified, not a pending recommendation.

### P1 — High value

**1. Re-run this exact, unmodified binary on faster storage (NVMe or
equivalent lower-`fsync`-latency device).**
- **Problem**: `fsync`/`FlushFileBuffers` latency (4.0–6.1ms, stable
  across the entire window-size sweep) is the dominant, measured
  constraint (§17).
- **Evidence**: §10's three-way cross-validated `fsync` measurement;
  §9's window sweep showing throughput is insensitive to
  software-tunable window size once `fsync` is held fixed.
- **Expected effect**: directly proportional-ish reduction in cycle
  time (§10's `fsync_cost_fraction` of ~46–54% is the theoretical upper
  bound on the gain from this alone, before accounting for whether
  `durable_ops_per_sync` would also change on faster storage — a real
  unknown, since the window formula is itself EMA-driven off measured
  `fsync` latency and would re-tune itself smaller on faster storage,
  which could reduce batch size too; **this interaction is not measured
  and is flagged as the single most valuable next experiment**, not
  assumed).
- **Risk**: none to the software; a hardware/environment change only.
- **Correctness impact**: none.
- **Metric to validate**: M1.2/M1.3 ops/sec, `fsync` p50/p95/p99,
  `durable_ops_per_sync`, all re-measured on the new device with the
  exact same binary and test suite used in this document.
- **Rollback plan**: not applicable (no code change).

**2. Investigate the window-wait spin-vs-sleep tradeoff at the *current*
5ms scale (targeted experiment, not a blind implementation).**
- **Problem**: `spin_wait_for_batch_window`'s busy-poll design was
  justified (per its own doc comment) against a 200µs window, where
  `thread::sleep`'s Windows timer-resolution overshoot would cost more
  than the window itself. The production window is now 5ms — 25x
  longer — and this justification was not re-verified after the window
  formula changed (`PHASE1_ADR.md` ADR-12).
- **Evidence**: §12's measured context-switch/CPU data during real
  runs, plus the source-verified mechanism (`yield_now()` every 10,000
  iterations, otherwise unconditional `spin_loop()`, held by the leader
  for the entire window). Explicitly **not** independently isolated
  from the rest of the system's scheduling cost in this session — a
  well-supported hypothesis, not a proven isolated cause (§12).
- **Expected effect**: unknown without the experiment — could reduce
  CPU/context-switch overhead (§12) with no throughput cost (since the
  window's *duration* wouldn't change, only how the waiting thread
  spends that time), or could regress latency if a sleep-based wait's
  wake-up jitter turns out to matter more than the current spin's CPU
  cost at this scale. **This is exactly the kind of hypothesis the
  operating brief requires be tested small before being adopted** —
  proposed as a **measurement task**, not a code change, for the next
  cycle.
- **Risk**: low to implement as an isolated experiment (mirrors the
  existing `phase1-window-experiment` pattern — env-var-gated,
  reduces to current behavior when unset); a shipped change would need
  its own full crash-consistency re-verification per §21/§22's standing
  rule.
- **Correctness impact**: none expected (the wait mechanism doesn't
  touch durability logic), but must be verified, not assumed, before
  shipping.
- **Metric to validate**: CPU%/context-switches/sec during M1.2/M1.3
  (§12's methodology, reusable as-is), plus ops/sec and p50/p95/p99 to
  confirm no latency regression.
- **Rollback plan**: env-var-gated experiment first; if adopted, the
  change is small and localized to one function, trivially revertible
  (same pattern as this session's own pipelining revert, §2).

### P2 — Optional

**3. Adopt the window sweep's measured-best window (3ms) as the new
default.** Evidence: §9. Expected effect: ~1.5–2.6% throughput gain —
within this machine's own measured run-to-run noise (§7.1). Risk: very
low (same formula shape, different constant; `PHASE1_ADR.md` ADR-12
already establishes the process for re-tuning this constant safely,
including re-verifying M1.1 isn't regressed the way the *naive* larger
window was, §16). Correctness impact: none. Validate: re-run §7.1's
4-repetition methodology at the new default, plus M1.1 in isolation
(historical precedent: a *different* large-window change once regressed
M1.1 from 2.9ms to 5.8ms, §16 — the demand-adaptive probe already
protects this, but it must be re-confirmed after any window-cap change,
not assumed). Rollback: revert one constant.

**4. Quantify the spin-loop's isolated CPU/context-switch contribution**
(a dedicated micro-experiment: run the exact same spin loop, out of
context, at the same rate this workload drives it, with nothing else
running) before committing to recommendation #2 above as more than a
hypothesis. Low cost, directly de-risks #2.

**5. Same-physical-disk fullness experiment** (§15.A item 1) — would
resolve the one remaining confound in this project's disk-fullness
evidence. Requires either a scratch volume this analysis doesn't have
access to, or explicit permission to use space on `D:`.

### Do not do

- **`io_uring`**: Linux-only kernel interface; this project's only
  available development/target environment evidenced in this analysis
  is Windows. Even setting portability aside, §10/§17's evidence shows
  the bottleneck is `FlushFileBuffers`/`fsync` **durability latency
  itself**, not **I/O submission/completion overhead** — `io_uring`
  (and its `SQPOLL` CPU cost, which would need its own justification)
  addresses the latter, not the former. **Rejected — the measured
  bottleneck and this mechanism's actual benefit do not match, per
  operating brief §28's explicit gate.**
- **`O_DIRECT` (or its Windows analogue, `FILE_FLAG_NO_BUFFERING`)**:
  same core issue — this bypasses the OS page cache to reduce a
  double-buffering/copy cost, but §14 already shows copy/allocation
  cost is 1,000x+ smaller than `fsync` cost. `O_DIRECT`/`FILE_FLAG_NO_
  BUFFERING` would also introduce real, unaddressed complexity this
  analysis did not evidence a need for: alignment requirements, buffer
  management changes, and (per the brief's own required checklist)
  write-amplification and durability-semantics questions that a WAL
  already fsync-per-batch cannot benefit from the way a cache-bypassing
  read-heavy workload could. **Rejected — no measured bottleneck this
  mechanism would address.**
- **Thread-per-core / a new concurrency runtime**: §7.2's thread-count
  sweep shows throughput *still rising* at 1,000 threads, with no
  saturation or regression at any tested level; §12 shows CPU never
  saturates (max 75%). The operating brief's own gate for this
  recommendation ("only if profiling proves OS-thread scheduling or
  shared locking is materially limiting throughput") is **not met** —
  scheduling cost is real (§12) but secondary (§17), not shown to be
  *limiting* throughput in the range actually tested. **Rejected on
  current evidence** — revisit only if a future, larger-scale sweep
  (§15.A) finds an actual saturation/regression point this analysis did
  not reach.
- **Buffer-pool / allocator changes**: §14 — allocation is not a
  measurable contributor at any tested concurrency. **Rejected.**
- **Removing or weakening the `wal` lock's scope, spinlocks in place of
  the `Mutex`/`Condvar`**: `PHASE1_TEST_RESULTS.md` §17 finding #4
  already measured a spin-based lock regressing throughput by more than
  half (10,100 → 4,500 ops/sec) on this same oversubscribed-core
  machine. Not re-tested this session (no new hypothesis to justify
  repeating a known-bad experiment, per the brief's own rule) —
  **rejected on existing, unretracted evidence.**

## 21. Rejected Optimizations

See "Do not do" above (§20) for `io_uring`, `O_DIRECT`, thread-per-core,
buffer pooling, and spinlocks — each with the specific measured evidence
that rules it out, not a generic dismissal.

## 22. Production Risks

- **Throughput below target** (§18/§19) — the one open, honestly-labeled
  gap; not a hidden or minimized risk.
- **Disk-fullness sensitivity, not fully characterized** (§5/§15): a
  13–37x degradation was measured for *write-free* `fsync` calls on a
  near-full volume; the *actual* WAL workload (write-then-sync) showed
  a much smaller effect in this session's measurement, but the
  disk-identity confound (§3) means this has not been cleanly isolated.
  **Operationally**: a production deployment should not place the WAL
  directory on a near-full volume without its own dedicated
  verification, independent of this document's confounded evidence.
- **`p95_batch_records` and full CPU/scheduler percentile data are not
  observable** with the current instrumentation (§8, §13) — an operator
  investigating a future regression would have less visibility than
  this analysis would like, though `GroupCommitStats`'s existing
  `max_batch_records`/aggregate counters are a partial mitigation.
- **Windows Defender / antivirus interaction is unquantified** (§15) —
  a real-time scanner intercepting every WAL segment file operation
  (including this session's new rename-based rotation, §2) is a
  plausible source of both added latency and cross-run variance that
  this analysis did not isolate.
- **The pipelined design remains in git history, not the working tree**
  (§2, §16) — a future contributor reading old commits could
  reintroduce it without re-discovering why it was reverted;
  `PHASE1_ADR.md` ADR-14 and this document both exist specifically to
  prevent that, but this depends on the historical record continuing to
  be read before such a change is made.

## 23. Final Engineering Decision

| Item | Value |
|---|---|
| **Current best stable throughput** | 100 writers: **10,746 ops/sec** (median of 4 runs); 1,000 writers: **63,207 ops/sec** (median of 4 runs) |
| **Target throughput** | 15,000 / 80,000 ops/sec respectively |
| **Gap** | 100 writers: **28.4% short** (71.6% of target); 1,000 writers: **21.0% short** (79.0% of target) |
| **Primary bottleneck** | Storage `fsync`/`FlushFileBuffers` durability latency (4.0–6.1ms, measured 3 independent ways, stable across a 400x window-size range) — **46–54% of every batch cycle**, directly measured, not inferred |
| **Secondary bottlenecks** | Lock contention concentrated in the leader's snapshot stage, scaling with batch-election frequency (strong indirect evidence, not directly traced); real, linearly-scaling context-switch/scheduling overhead from the 1,000-OS-thread model (measured, but not shown to limit throughput in the range tested) |
| **Hardware contribution** | High-confidence, directly measured: two consumer-grade **SATA** (not NVMe) SSDs; raw `fsync`-only latency 2.4ms median even on the healthy disk; the production default window (5ms) sits within 1.5–2.6% of the best of 10 tested alternatives, meaning software has already captured essentially all the headroom available against this hardware floor |
| **Software contribution** | Verified negligible for allocation/CRC/serialization (1,000x+ smaller than `fsync`); verified near-optimal for window-size policy (§9); real but secondary and *not* shown to be throughput-limiting for lock contention and thread scheduling (§11, §12, §17) |
| **Best configuration discovered** | `max_wait ≈ 3ms` (vs. production 5ms) — 1.5–2.6% better, within noise; **not** recommended as an urgent change (§20, P2) |
| **Highest-value next change** | **Not a code change**: re-measure this exact binary on lower-latency storage (§20, P1-1). Highest-value *code* item: a small, env-var-gated experiment on the window-wait's spin-vs-sleep tradeoff at the current 5ms scale (§20, P1-2) — proposed as a measurement task, correctly labeled as an unverified hypothesis, not a ready-to-ship fix |
| **Is advanced I/O justified?** | **No.** `io_uring`: Linux-only, and addresses submission/completion overhead this analysis did not find to be the bottleneck. `O_DIRECT`/`FILE_FLAG_NO_BUFFERING`: addresses copy/cache overhead already shown negligible (§14). Neither passes the operating brief's own gate (§28) of "proven to address the exact measured bottleneck." |
| **Should the current WAL architecture be retained?** | **Yes.** Leader/follower group commit with a single-phase, strictly-serialized `LEADER_ACTIVE` batch (not the pipelined alternative, which measurably regressed on this platform, §16) is not shown to be the limiting factor at any concurrency level tested (§7.2's sweep finds no saturation through 1,000 threads); the measured ceiling tracks storage latency, not this architecture's coordination cost. |
| **Production-readiness status** | **PRODUCTION SAFE — PERFORMANCE TARGET NOT ACHIEVED** |

**On the production-readiness classification, explicitly** (this
document's own §30/§19 rubric, distinct from — and not overruling —
`PHASE1_TEST_RESULTS.md`'s own verdict):

`PHASE1_TEST_RESULTS.md` §19 records **"NOT PRODUCTION READY"** under
*that* document's own gate checklist, which — by that document's
explicitly stated policy — treats an unmet M1.2/M1.3 throughput
assertion as a failing gate item on its own terms, because those
targets were named in that document's originating brief as "the entire
justification for Phase 1." That verdict is correct under that
document's own rubric and is **not overruled or replaced here**.

This document's own operating brief (§30) supplies a **different,
finer-grained three-way rubric** that this analysis is required to use
instead: it explicitly forbids marking "NOT PRODUCTION READY" *solely*
because an arbitrary throughput target is unmet on the development
machine, if the WAL itself is otherwise safe — and explicitly requires
distinguishing that case from a genuine correctness/durability defect.
Under *that* rubric, applied to the evidence actually gathered in this
document:

- Every crash-consistency run this session passed (**11/11** — 8 from
  this session's prior cycle plus 3 fresh confirmations here, covering
  all 11 `AbortPoint`s, both crash-test binaries).
- The full regression gate is clean at HEAD `61244f2`: `cargo test --lib`
  (87/87, both build configurations), `cargo clippy --all-targets
  --all-features -- -D warnings` (clean, including this analysis's own
  two new example files), `cargo fmt --check` (clean).
- No `fsync` was skipped, no acknowledgment was returned before
  durability, no waiter was dropped, no corruption was hidden — every
  one of these was checked in this session (§21 of the operating brief),
  not assumed.
- The one real correctness defect found *this session* (non-atomic
  segment creation, §2/§16) was found, fixed, and re-verified before
  this document was written — it is not an open item.

Given that evidence, and per this document's own governing rubric:
**PRODUCTION SAFE — PERFORMANCE TARGET NOT ACHIEVED.** The two verdicts
are not in conflict; they are answers to two different, explicitly
different-scoped questions, and both are shown with their full
supporting evidence in their respective documents rather than silently
reconciled into one number.
